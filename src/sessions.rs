//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    cmp::Reverse,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity,
    AgentSelection, AgentSelectionOperationId, CreateSessionRequest, Message, MessageId,
    MessageRole, MessageStatus, ModelAvailability, ModelOptionValue, Prompt, PromptDelivery,
    PromptId, PromptOrder, PromptStatus, ProviderId, Session, SessionCatalogChange,
    SessionCatalogRevision, SessionCatalogSnapshot, SessionCatalogUpdate, SessionChange, SessionId,
    SessionListItem, SessionRevision, SessionSnapshot, SessionStatus, SessionSummary,
    SessionTimestamp, SessionUpdate, Turn, TurnId, TurnStatus, UnreadableSessionSummary,
    UpdateAgentSelectionRequest, Workspace,
};
use crate::provider::ProviderResumeState;
use crate::session_projection::apply_update;
use crate::storage::{PersistedSession, RestoredSessions, StorageSink, StoredResumeState};

const SESSION_UPDATE_CAPACITY: usize = 256;

#[derive(Clone)]
pub(crate) struct SessionStore {
    state: Arc<Mutex<SessionStoreState>>,
    storage: StorageSink,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreateSessionError {
    EmptyPrompt,
    InvalidWorkspace,
    PromptConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmitPromptError {
    EmptyPrompt,
    SessionNotFound,
    PromptConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptMutationError {
    SessionNotFound,
    PromptNotFound,
    PromptNotPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentSelectionMutationError {
    SessionNotFound,
    OperationConflict,
    ProviderConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterruptTurnError {
    SessionNotFound,
    TurnNotFound,
    TurnNotActive,
    ProviderFailure(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreOutcome<T> {
    Created(T),
    Existing(T),
}

struct SessionStoreState {
    sessions: HashMap<SessionId, SessionRecord>,
    unreadable_sessions: HashMap<SessionId, UnreadableSessionSummary>,
    prompts: HashMap<PromptId, PromptOwner>,
    last_timestamp: Option<SessionTimestamp>,
    catalog_revision: SessionCatalogRevision,
    catalog_updates: broadcast::Sender<SessionCatalogUpdate>,
}

struct SessionRecord {
    snapshot: SessionSnapshot,
    summary: SessionSummary,
    updates: broadcast::Sender<SessionUpdate>,
    next_prompt_order: PromptOrder,
    steer_targets: HashMap<PromptId, TurnId>,
    selection_operations: HashMap<AgentSelectionOperationId, AgentSelection>,
    selection_retry_prompt: Option<PromptId>,
    resume_states: HashMap<ProviderId, ProviderResumeState>,
}

struct PromptOwner {
    session_id: SessionId,
    text: String,
    agent_selection: Option<AgentSelection>,
    origin: PromptOrigin,
}

enum PromptOrigin {
    SessionCreation {
        requested_workspace: PathBuf,
        canonical_workspace: PathBuf,
    },
    Admission(PromptDelivery),
}

pub(crate) struct SessionFeed {
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) updates: broadcast::Receiver<SessionUpdate>,
}

pub(crate) struct SessionCatalogFeed {
    pub(crate) snapshot: SessionCatalogSnapshot,
    pub(crate) updates: broadcast::Receiver<SessionCatalogUpdate>,
}

pub(crate) struct PromptAdmission {
    pub(crate) prompt: Prompt,
    pub(crate) disposition: PromptAdmissionDisposition,
}

pub(crate) struct AgentSelectionMutation {
    pub(crate) selection: AgentSelection,
    pub(crate) retry_prompt_id: Option<PromptId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptAdmissionDisposition {
    StartImmediately,
    SteerActive,
    RemainPending,
}

pub(crate) struct DeliveredTurn {
    pub(crate) prompt: Prompt,
    pub(crate) turn: Turn,
}

pub(crate) enum DeliveredTurnStatus {
    Active,
    Failed { message: String },
}

pub(crate) enum ProviderTurnOutcome {
    Completed,
    Failed {
        trailing_output: TrailingCommandOutput,
        message: String,
    },
    Interrupted {
        trailing_output: TrailingCommandOutput,
    },
}

/// The unterminated line each in-flight command's normalizer held back when its
/// Turn settled, keyed by the command Activity it belongs to. Only the Provider
/// actor holds those normalizers, while which streams are still in flight is the
/// store's own knowledge, so a settle path that cannot reach the actor carries
/// an empty one and still settles every stream the Turn left open.
pub(crate) type TrailingCommandOutput = HashMap<ActivityId, NormalizedText>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ListSessionsError {
    InvalidWorkspace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeleteSessionError {
    SessionNotFound,
    Storage(String),
}

impl SessionStore {
    pub(crate) fn new(restored: RestoredSessions, storage: StorageSink) -> Self {
        let RestoredSessions {
            readable: persisted_sessions,
            unreadable,
        } = restored;
        let last_timestamp = persisted_sessions
            .iter()
            .map(|persisted| persisted.summary.updated_at)
            .chain(unreadable.iter().map(|summary| summary.updated_at))
            .max();
        let mut sessions = HashMap::new();
        let mut prompts = HashMap::new();
        for persisted in persisted_sessions {
            let PersistedSession {
                summary,
                snapshot,
                resume_states,
            } = persisted;
            let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
            let next_prompt_order = snapshot
                .prompts
                .iter()
                .map(|prompt| prompt.admission_order.0)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .map(PromptOrder)
                .expect("persisted Prompt admission order space is not exhausted");
            for prompt in &snapshot.prompts {
                prompts.insert(
                    prompt.id,
                    PromptOwner {
                        session_id: snapshot.session.id,
                        text: prompt.text.clone(),
                        agent_selection: None,
                        origin: PromptOrigin::Admission(prompt.delivery),
                    },
                );
            }
            sessions.insert(
                snapshot.session.id,
                SessionRecord {
                    snapshot,
                    summary,
                    updates,
                    next_prompt_order,
                    steer_targets: HashMap::new(),
                    selection_operations: HashMap::new(),
                    selection_retry_prompt: None,
                    resume_states,
                },
            );
        }
        let (catalog_updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        let unreadable_sessions = unreadable
            .into_iter()
            .map(|summary| (summary.id, summary))
            .collect();
        Self {
            state: Arc::new(Mutex::new(SessionStoreState {
                sessions,
                unreadable_sessions,
                prompts,
                last_timestamp,
                catalog_revision: SessionCatalogRevision::INITIAL,
                catalog_updates,
            })),
            storage,
        }
    }

    pub(crate) fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<StoreOutcome<SessionSnapshot>, CreateSessionError> {
        if request.prompt.text.trim().is_empty() {
            return Err(CreateSessionError::EmptyPrompt);
        }

        let retry_workspace = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if let Some(owner) = state.prompts.get(&request.prompt.id) {
                if owner.matches_requested_creation(&request) {
                    return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
                }
                Some(
                    owner
                        .canonical_creation_workspace(&request)
                        .ok_or(CreateSessionError::PromptConflict)?,
                )
            } else {
                None
            }
        };

        let workspace_path = fs::canonicalize(&request.workspace.path).map_err(|_| {
            if retry_workspace.is_some() {
                CreateSessionError::PromptConflict
            } else {
                CreateSessionError::InvalidWorkspace
            }
        })?;
        if let Some(expected) = retry_workspace {
            if workspace_path != expected {
                return Err(CreateSessionError::PromptConflict);
            }
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let owner = state
                .prompts
                .get(&request.prompt.id)
                .expect("Prompt owner remains indexed for the server lifetime");
            return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
        }
        if !workspace_path.is_dir() {
            return Err(CreateSessionError::InvalidWorkspace);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.matches_canonical_creation(&request, &workspace_path) {
                return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
            }
            return Err(CreateSessionError::PromptConflict);
        }

        let title = request.prompt.text.trim().to_owned();
        let session_id = SessionId::new();
        let prompt = Prompt {
            id: request.prompt.id,
            text: request.prompt.text.clone(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
        };
        let snapshot = SessionSnapshot {
            session: Session {
                id: session_id,
                workspace: Workspace {
                    path: workspace_path.clone(),
                },
                agent_selection: request.agent_selection.clone(),
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
            },
            revision: SessionRevision::INITIAL,
            prompts: vec![prompt],
            turns: Vec::new(),
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        let timestamp = state.next_timestamp();
        let summary = SessionSummary {
            session: snapshot.session.clone(),
            title,
            created_at: timestamp,
            updated_at: timestamp,
        };
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                agent_selection: request.agent_selection,
                origin: PromptOrigin::SessionCreation {
                    requested_workspace: request.workspace.path,
                    canonical_workspace: workspace_path,
                },
            },
        );
        let persisted_summary = summary.clone();
        state.sessions.insert(
            session_id,
            SessionRecord {
                snapshot: snapshot.clone(),
                summary,
                updates,
                next_prompt_order: PromptOrder(2),
                steer_targets: HashMap::new(),
                selection_operations: HashMap::new(),
                selection_retry_prompt: None,
                resume_states: HashMap::new(),
            },
        );
        self.storage.created(persisted_summary, snapshot.clone());
        state.publish_catalog_change(SessionCatalogChange::Created { session_id });
        Ok(StoreOutcome::Created(snapshot))
    }

    pub(crate) fn admit(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<StoreOutcome<PromptAdmission>, AdmitPromptError> {
        if request.prompt.text.trim().is_empty() {
            return Err(AdmitPromptError::EmptyPrompt);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.matches_admission(session_id, &request) {
                let prompt = state
                    .sessions
                    .get(&session_id)
                    .and_then(|record| {
                        record
                            .snapshot
                            .prompts
                            .iter()
                            .find(|prompt| prompt.id == request.prompt.id)
                    })
                    .expect("Prompt owner always references its Prompt")
                    .clone();
                return Ok(StoreOutcome::Existing(PromptAdmission {
                    prompt,
                    disposition: PromptAdmissionDisposition::RemainPending,
                }));
            }
            return Err(AdmitPromptError::PromptConflict);
        }

        if !state.sessions.contains_key(&session_id) {
            return Err(AdmitPromptError::SessionNotFound);
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let admission_order = record.next_prompt_order;
        let next_prompt_order = PromptOrder(
            admission_order
                .0
                .checked_add(1)
                .expect("Prompt admission order space is not exhausted"),
        );
        let (disposition, steer_target) = match (
            active_turn_id(&record.snapshot)
                .expect("stored Sessions preserve the one-active-Turn invariant"),
            request.delivery,
        ) {
            (None, _) => (PromptAdmissionDisposition::StartImmediately, None),
            (Some(turn_id), PromptDelivery::Steer) => {
                (PromptAdmissionDisposition::SteerActive, Some(turn_id))
            }
            (Some(_), PromptDelivery::Queue) => (PromptAdmissionDisposition::RemainPending, None),
        };
        let prompt = Prompt {
            id: request.prompt.id,
            text: request.prompt.text.clone(),
            delivery: request.delivery,
            admission_order,
            status: PromptStatus::Pending,
        };
        record
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptAdded {
                    prompt: prompt.clone(),
                }],
                updated_at,
            )
            .expect("admission changes preserve Session invariants");
        record.next_prompt_order = next_prompt_order;
        if let Some(turn_id) = steer_target {
            record.steer_targets.insert(prompt.id, turn_id);
        }
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                agent_selection: None,
                origin: PromptOrigin::Admission(request.delivery),
            },
        );
        Ok(StoreOutcome::Created(PromptAdmission {
            prompt,
            disposition,
        }))
    }

    pub(crate) fn subscribe(&self, session_id: SessionId) -> Option<SessionFeed> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state.sessions.get(&session_id)?;
        Some(SessionFeed {
            snapshot: record.snapshot.clone(),
            updates: record.updates.subscribe(),
        })
    }

    pub(crate) fn subscribe_catalog(&self) -> SessionCatalogFeed {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut session_ids = state
            .sessions
            .keys()
            .chain(state.unreadable_sessions.keys())
            .copied()
            .collect::<Vec<_>>();
        session_ids.sort_unstable_by_key(ToString::to_string);
        SessionCatalogFeed {
            snapshot: SessionCatalogSnapshot {
                revision: state.catalog_revision,
                session_ids,
            },
            updates: state.catalog_updates.subscribe(),
        }
    }

    pub(crate) fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.clone())
    }

    pub(crate) fn workspace(&self, session_id: SessionId) -> Option<PathBuf> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.session.workspace.path.clone())
    }

    pub(crate) fn resume_state(
        &self,
        session_id: SessionId,
        provider: &ProviderId,
    ) -> Option<ProviderResumeState> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .and_then(|record| record.resume_states.get(provider))
            .cloned()
    }

    pub(crate) fn save_resume_state(
        &self,
        session_id: SessionId,
        provider: ProviderId,
        resume_state: ProviderResumeState,
    ) -> anyhow::Result<()> {
        self.storage.save_resume_state(StoredResumeState {
            session_id,
            provider: provider.clone(),
            resume_state: resume_state.clone(),
        })?;
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .resume_states
            .insert(provider, resume_state);
        Ok(())
    }

    pub(crate) fn delete(&self, session_id: SessionId) -> Result<(), DeleteSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if !state.sessions.contains_key(&session_id)
            && !state.unreadable_sessions.contains_key(&session_id)
        {
            return Err(DeleteSessionError::SessionNotFound);
        }
        self.storage
            .deleted(session_id)
            .map_err(|error| DeleteSessionError::Storage(error.to_string()))?;
        state.sessions.remove(&session_id);
        state.unreadable_sessions.remove(&session_id);
        state
            .prompts
            .retain(|_, owner| owner.session_id != session_id);
        state.publish_catalog_change(SessionCatalogChange::Deleted { session_id });
        Ok(())
    }

    pub(crate) fn reconcile_effective_agent_selection(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        agent_id: AgentId,
        selection: AgentSelection,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let turn = record
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or_else(|| anyhow!("Effective Agent Selection referenced an unknown Turn"))?;
        if turn.status != TurnStatus::Active {
            return Err(anyhow!(
                "Effective Agent Selection referenced a terminal Turn"
            ));
        }
        let requested = turn
            .agent
            .as_ref()
            .ok_or_else(|| anyhow!("Effective Agent Selection referenced an unbound Turn"))?;
        if requested.agent != agent_id || requested.selection.provider != selection.provider {
            return Err(anyhow!(
                "Effective Agent Selection changed the Provider identity of an active Turn"
            ));
        }
        if requested.selection == selection {
            return Ok(None);
        }
        let update_authoritative =
            record.snapshot.session.agent_selection.as_ref() == Some(&requested.selection);
        let effective_agent = AgentIdentity {
            agent: agent_id,
            selection: selection.clone(),
        };
        let mut changes = Vec::new();
        if update_authoritative {
            changes.push(SessionChange::AgentSelectionChanged {
                selection: selection.clone(),
            });
        }
        changes.push(SessionChange::TurnAgentChanged {
            turn_id,
            agent: effective_agent,
        });
        if requested.selection.model != selection.model {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider used Model `{}` instead of requested Model `{}`.",
                        selection.model, requested.selection.model
                    ),
                },
            });
        }
        for effective in &selection.options {
            let requested_value = requested
                .selection
                .options
                .iter()
                .find(|candidate| candidate.id == effective.id)
                .map(|candidate| &candidate.value);
            if requested_value == Some(&effective.value) {
                continue;
            }
            let requested_value = requested_value
                .map(model_option_value_text)
                .unwrap_or_else(|| "no value".to_owned());
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider used Model Option `{}` value `{}` instead of requested value `{requested_value}`.",
                        effective.id,
                        model_option_value_text(&effective.value),
                    ),
                },
            });
        }
        for omitted in requested.selection.options.iter().filter(|requested| {
            !selection
                .options
                .iter()
                .any(|effective| effective.id == requested.id)
        }) {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider omitted requested Model Option `{}` value `{}`.",
                        omitted.id,
                        model_option_value_text(&omitted.value),
                    ),
                },
            });
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.commit(&self.storage, session_id, changes, updated_at)?;
        Ok(Some(update))
    }

    pub(crate) fn initialize_agent_selection(
        &self,
        session_id: SessionId,
        selection: AgentSelection,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .snapshot
            .session
            .agent_selection
            .is_some()
        {
            return Ok(None);
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.commit(
            &self.storage,
            session_id,
            vec![SessionChange::AgentSelectionChanged { selection }],
            updated_at,
        )?;
        Ok(Some(update))
    }

    pub(crate) fn apply_agent_selection_command(
        &self,
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    ) -> Result<AgentSelectionMutation, AgentSelectionMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or(AgentSelectionMutationError::SessionNotFound)?;
        if let Some(previous) = record.selection_operations.get(&request.operation_id) {
            return if previous == &request.selection {
                Ok(AgentSelectionMutation {
                    selection: previous.clone(),
                    retry_prompt_id: None,
                })
            } else {
                Err(AgentSelectionMutationError::OperationConflict)
            };
        }
        if record
            .snapshot
            .session
            .agent_selection
            .as_ref()
            .is_some_and(|current| current.provider != request.selection.provider)
        {
            return Err(AgentSelectionMutationError::ProviderConflict);
        }
        let selection_changed =
            record.snapshot.session.agent_selection.as_ref() != Some(&request.selection);
        let availability_changed =
            record.snapshot.session.agent_selection_availability != ModelAvailability::Available;
        let changed = selection_changed || availability_changed;
        let updated_at = changed.then(|| state.next_timestamp());
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        if changed {
            let mut changes = Vec::with_capacity(2);
            if selection_changed {
                changes.push(SessionChange::AgentSelectionChanged {
                    selection: request.selection.clone(),
                });
            }
            if availability_changed {
                changes.push(SessionChange::AgentSelectionAvailabilityChanged {
                    availability: ModelAvailability::Available,
                });
            }
            record
                .commit(
                    &self.storage,
                    session_id,
                    changes,
                    updated_at.expect("changed selection has a timestamp"),
                )
                .expect("Agent Selection commands preserve Session invariants");
        }
        record
            .selection_operations
            .insert(request.operation_id, request.selection.clone());
        Ok(AgentSelectionMutation {
            selection: request.selection,
            retry_prompt_id: record.pending_selection_retry_prompt(),
        })
    }

    pub(crate) fn reject_agent_selection(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (prompt, mark_unavailable, admission_order, mut changes) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            let turn = record
                .snapshot
                .turns
                .iter()
                .find(|turn| turn.id == turn_id)
                .ok_or_else(|| anyhow!("Selection rejection referenced an unknown Turn"))?;
            if turn.status != TurnStatus::Active {
                return Err(anyhow!("Selection rejection referenced a terminal Turn"));
            }
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == turn.prompt_id)
                .expect("every Turn retains its originating Prompt");
            let mark_unavailable = turn.agent.as_ref().is_some_and(|agent| {
                record.snapshot.session.agent_selection.as_ref() == Some(&agent.selection)
            });
            (
                prompt.clone(),
                mark_unavailable,
                record.next_prompt_order,
                settle_in_flight_changes(&record.snapshot, turn_id, trailing_output),
            )
        };
        let restored = Prompt {
            id: PromptId::new(),
            text: prompt.text,
            delivery: PromptDelivery::Queue,
            admission_order,
            status: PromptStatus::Pending,
        };
        changes.extend([
            SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            },
            SessionChange::TurnStatusChanged {
                turn_id,
                status: TurnStatus::Failed,
            },
        ]);
        if mark_unavailable {
            changes.push(SessionChange::AgentSelectionAvailabilityChanged {
                availability: ModelAvailability::Unavailable,
            });
        }
        changes.push(SessionChange::PromptAdded {
            prompt: restored.clone(),
        });
        let updated_at = state.next_timestamp();
        let update = {
            let record = state
                .sessions
                .get_mut(&session_id)
                .expect("Session existence was checked while holding the store lock");
            let update = record.commit(&self.storage, session_id, changes, updated_at)?;
            record.selection_retry_prompt = Some(restored.id);
            update
        };
        state.prompts.insert(
            restored.id,
            PromptOwner {
                session_id,
                text: restored.text,
                agent_selection: None,
                origin: PromptOrigin::Admission(restored.delivery),
            },
        );
        Ok(update)
    }

    pub(crate) fn deliver_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
        agent_id: Option<AgentId>,
        delivered_turn_status: DeliveredTurnStatus,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (prompt, agent) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)?.is_some() {
                return Ok(None);
            }
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .ok_or_else(|| anyhow!("Prompt delivery referenced an unknown Prompt"))?;
            if prompt.status != PromptStatus::Pending {
                return Ok(None);
            }
            let agent = agent_id.and_then(|agent| {
                record
                    .snapshot
                    .session
                    .agent_selection
                    .clone()
                    .map(|selection| AgentIdentity { agent, selection })
            });
            (prompt.clone(), agent)
        };
        let updated_at = state.next_timestamp();
        let (turn_status, failure_message) = match delivered_turn_status {
            DeliveredTurnStatus::Active => (TurnStatus::Active, None),
            DeliveredTurnStatus::Failed { message } => (TurnStatus::Failed, Some(message)),
        };
        let (delivered, mut changes) = prepare_prompt_delivery(prompt, agent, turn_status);
        if let Some(message) = failure_message {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id: delivered.turn.id,
                    text: message,
                },
            });
        }
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)?;
        record.steer_targets.remove(&prompt_id);
        if record.selection_retry_prompt == Some(prompt_id) {
            record.selection_retry_prompt = None;
        }
        Ok(Some(delivered))
    }

    pub(crate) fn next_pending_steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<Option<Prompt>> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        record.next_pending_steer(turn_id)
    }

    pub(crate) fn deliver_steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<Prompt>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            let Some(prompt) = record.pending_steer(turn_id, prompt_id)? else {
                return Ok(None);
            };
            let earliest = record
                .next_pending_steer(turn_id)?
                .expect("the target is a pending steer Prompt");
            if earliest.id != prompt_id {
                return Err(anyhow!(
                    "Steer Prompts must be delivered in admission order"
                ));
            }
            prompt.clone()
        };
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let mut changes = Vec::with_capacity(2);
        append_steer_delivery_changes(&mut changes, &prompt, turn_id);
        record.commit(&self.storage, session_id, changes, updated_at)?;
        record.steer_targets.remove(&prompt_id);
        let mut delivered = prompt;
        delivered.status = PromptStatus::Delivered;
        Ok(Some(delivered))
    }

    pub(crate) fn report_steer_failure(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        prompt_id: PromptId,
        message: String,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if record.pending_steer(turn_id, prompt_id)?.is_none() {
                return Ok(None);
            }
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.commit(
            &self.storage,
            session_id,
            vec![SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            }],
            updated_at,
        )?;
        record.steer_targets.remove(&prompt_id);
        Ok(Some(update))
    }

    pub(crate) fn fail_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let mut changes = settle_in_flight_changes(&record.snapshot, turn_id, trailing_output);
        changes.extend([
            SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            },
            SessionChange::TurnStatusChanged {
                turn_id,
                status: TurnStatus::Failed,
            },
        ]);
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)
    }

    pub(crate) fn finish_provider_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        agent_id: AgentId,
        outcome: ProviderTurnOutcome,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (trailing_output, failure_message, status) = match outcome {
            ProviderTurnOutcome::Completed => {
                (TrailingCommandOutput::new(), None, TurnStatus::Completed)
            }
            ProviderTurnOutcome::Failed {
                trailing_output,
                message,
            } => (trailing_output, Some(message), TurnStatus::Failed),
            ProviderTurnOutcome::Interrupted { trailing_output } => {
                (trailing_output, None, TurnStatus::Interrupted)
            }
        };
        let (pending_steers, next_queued_prompt, next_agent, settle_changes) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)? != Some(turn_id) {
                // The store has moved past this Turn, so another settle path reached it
                // first. Settling a Turn settles its in-flight streams in the same commit,
                // and a Provider actor stays reachable until it has settled the Turn it
                // was running, so the flushed output arriving here should find nothing
                // left to land on. Store what it does still find rather than trusting
                // that and discarding it unseen.
                let salvaged = settle_in_flight_changes(&record.snapshot, turn_id, trailing_output);
                if salvaged.is_empty() {
                    return Ok(None);
                }
                let updated_at = state.next_timestamp();
                let record = state
                    .sessions
                    .get_mut(&session_id)
                    .expect("Session existence was checked while holding the store lock");
                record.commit(&self.storage, session_id, salvaged, updated_at)?;
                return Ok(None);
            }
            let mut pending_steers = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                })
                .cloned()
                .collect::<Vec<_>>();
            pending_steers.sort_unstable_by_key(|prompt| prompt.admission_order);
            let next_queued_prompt = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Queue
                })
                .min_by_key(|prompt| prompt.admission_order)
                .cloned();
            let next_agent = record
                .snapshot
                .session
                .agent_selection
                .clone()
                .map(|selection| AgentIdentity {
                    agent: agent_id,
                    selection,
                });
            (
                pending_steers,
                next_queued_prompt,
                next_agent,
                settle_in_flight_changes(&record.snapshot, turn_id, trailing_output),
            )
        };

        let mut changes = Vec::with_capacity(pending_steers.len() * 2 + settle_changes.len() + 6);
        for prompt in &pending_steers {
            append_steer_delivery_changes(&mut changes, prompt, turn_id);
        }
        changes.extend(settle_changes);
        if let Some(message) = failure_message {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            });
        }
        changes.push(SessionChange::TurnStatusChanged { turn_id, status });

        let next_turn = next_queued_prompt.map(|prompt| {
            let (delivered, delivery_changes) =
                prepare_prompt_delivery(prompt, next_agent, TurnStatus::Active);
            changes.extend(delivery_changes);
            delivered
        });

        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)?;
        for prompt in pending_steers {
            record.steer_targets.remove(&prompt.id);
        }
        Ok(next_turn)
    }

    pub(crate) fn publish(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if !state.sessions.contains_key(&session_id) {
            return Err(anyhow!("Session does not exist on this server instance"));
        }
        let updated_at = state.next_timestamp();
        let stored = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        stored.commit(&self.storage, session_id, changes, updated_at)
    }

    pub(crate) fn publish_agent_output(
        &self,
        session_id: SessionId,
        change: SessionChange,
    ) -> anyhow::Result<SessionUpdate> {
        self.publish_agent_output_changes(session_id, vec![change])
    }

    /// Publishes Agent output whose changes describe one step of a Provider
    /// stream, as a single update so a client never observes content apart
    /// from the truncation that ended it.
    pub(crate) fn publish_agent_output_changes(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            for change in &changes {
                let turn_id = agent_output_turn_id(&record.snapshot, change)?;
                let turn = record
                    .snapshot
                    .turns
                    .iter()
                    .find(|turn| turn.id == turn_id)
                    .ok_or_else(|| anyhow!("Agent output referenced an unknown Turn"))?;
                if turn.status != TurnStatus::Active {
                    return Err(anyhow!("Agent output referenced a terminal Turn"));
                }
            }
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.commit(&self.storage, session_id, changes, updated_at)
    }

    pub(crate) fn continuation_boundary(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<Vec<Prompt>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let delivered = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)? != Some(turn_id) {
                return Err(anyhow!("Turn is not the active Turn for this Session"));
            }
            let mut prompts = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                })
                .cloned()
                .collect::<Vec<_>>();
            prompts.sort_unstable_by_key(|prompt| prompt.admission_order);
            prompts
        };
        if delivered.is_empty() {
            return Ok(delivered);
        }

        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let mut changes = Vec::with_capacity(delivered.len() * 2);
        for prompt in &delivered {
            changes.extend([
                SessionChange::PromptStatusChanged {
                    prompt_id: prompt.id,
                    status: PromptStatus::Delivered,
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: prompt.text.clone(),
                        truncated: false,
                    },
                },
            ]);
        }
        record.commit(&self.storage, session_id, changes, updated_at)?;
        Ok(delivered
            .into_iter()
            .map(|mut prompt| {
                prompt.status = PromptStatus::Delivered;
                prompt
            })
            .collect())
    }

    pub(crate) fn promote(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt, PromptMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut prompt = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or(PromptMutationError::SessionNotFound)?;
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .ok_or(PromptMutationError::PromptNotFound)?
                .clone();
            if prompt.status != PromptStatus::Pending {
                return Err(PromptMutationError::PromptNotPending);
            }
            if prompt.delivery == PromptDelivery::Steer {
                return Ok(prompt);
            }
            prompt
        };
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        prompt.delivery = PromptDelivery::Steer;
        record
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptDeliveryChanged {
                    prompt_id,
                    delivery: PromptDelivery::Steer,
                }],
                updated_at,
            )
            .expect("Prompt promotion preserves Session invariants");
        Ok(prompt)
    }

    pub(crate) fn cancel(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt, PromptMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut prompt = state
            .sessions
            .get(&session_id)
            .ok_or(PromptMutationError::SessionNotFound)?
            .snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .ok_or(PromptMutationError::PromptNotFound)?
            .clone();
        if prompt.status == PromptStatus::Cancelled {
            return Ok(prompt);
        }
        if prompt.status != PromptStatus::Pending || prompt.delivery != PromptDelivery::Queue {
            return Err(PromptMutationError::PromptNotPending);
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptStatusChanged {
                    prompt_id,
                    status: PromptStatus::Cancelled,
                }],
                updated_at,
            )
            .expect("Prompt cancellation preserves Session invariants");
        if record.selection_retry_prompt == Some(prompt_id) {
            record.selection_retry_prompt = None;
        }
        prompt.status = PromptStatus::Cancelled;
        Ok(prompt)
    }

    pub(crate) fn interrupt_target(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Turn, InterruptTurnError> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let turn = state
            .sessions
            .get(&session_id)
            .ok_or(InterruptTurnError::SessionNotFound)?
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or(InterruptTurnError::TurnNotFound)?
            .clone();
        if turn.status == TurnStatus::Interrupted {
            return Ok(turn);
        }
        if turn.status != TurnStatus::Active {
            return Err(InterruptTurnError::TurnNotActive);
        }
        Ok(turn)
    }

    pub(crate) fn list(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<SessionListItem>, ListSessionsError> {
        let workspace = workspace
            .map(fs::canonicalize)
            .transpose()
            .map_err(|_| ListSessionsError::InvalidWorkspace)?;
        if workspace.as_ref().is_some_and(|path| !path.is_dir()) {
            return Err(ListSessionsError::InvalidWorkspace);
        }

        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut summaries = state
            .sessions
            .values()
            .map(|record| record.summary.clone())
            .filter(|summary| {
                workspace
                    .as_ref()
                    .is_none_or(|path| summary.session.workspace.path == *path)
            })
            .map(SessionListItem::Readable)
            .chain(
                state
                    .unreadable_sessions
                    .values()
                    .filter(|summary| {
                        workspace.as_ref().is_none_or(|path| {
                            summary
                                .workspace
                                .as_ref()
                                .is_none_or(|workspace| workspace.path == *path)
                        })
                    })
                    .cloned()
                    .map(SessionListItem::Unreadable),
            )
            .collect::<Vec<_>>();
        summaries.sort_unstable_by_key(|summary| Reverse(summary.updated_at()));
        Ok(summaries)
    }
}

fn model_option_value_text(value: &ModelOptionValue) -> String {
    match value {
        ModelOptionValue::Select { choice } => choice.to_string(),
        ModelOptionValue::Toggle { enabled } => enabled.to_string(),
    }
}

/// The changes that carry one step of normalized Agent Message content into a
/// Session: the content the Provider sent, and the truncation if Suru's cap
/// ended the stream there. Both travel as data, so no client has to read a
/// marker back out of the content.
pub(crate) fn message_content_changes(
    message_id: MessageId,
    content: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText { content, truncated } = content;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::MessageContentAppended {
            message_id,
            content,
        });
    }
    if truncated {
        changes.push(SessionChange::MessageTruncated { message_id });
    }
    changes
}

/// The changes that carry one step of normalized command output into a
/// Session, on the same terms as [`message_content_changes`].
pub(crate) fn command_output_changes(
    activity_id: ActivityId,
    output: NormalizedText,
) -> Vec<SessionChange> {
    let NormalizedText {
        content,
        truncated: output_truncated,
    } = output;
    let mut changes = Vec::new();
    if !content.is_empty() {
        changes.push(SessionChange::CommandOutputAppended {
            activity_id,
            content,
        });
    }
    if output_truncated {
        changes.push(SessionChange::CommandOutputTruncated { activity_id });
    }
    changes
}

/// The changes that settle everything a Turn left in flight: its streaming Agent
/// Message completes, and each command or file-change Activity still Active
/// fails, every command first storing the trailing output its normalizer
/// flushed. Reading the in-flight set from the Session's own snapshot rather
/// than from the caller keeps every settle path equivalent, including the ones
/// that never reach the Provider actor holding that Turn. A Turn that completes
/// normally has nothing in flight — a Provider that leaves a stream open is
/// refused its completion — so this settles nothing on that path.
fn settle_in_flight_changes(
    snapshot: &SessionSnapshot,
    turn_id: TurnId,
    mut trailing_output: TrailingCommandOutput,
) -> Vec<SessionChange> {
    let mut changes = Vec::new();
    changes.extend(
        snapshot
            .messages
            .iter()
            .filter(|message| {
                message.turn_id == turn_id
                    && message.role == MessageRole::Agent
                    && message.status == MessageStatus::Streaming
            })
            .map(|message| SessionChange::MessageCompleted {
                message_id: message.id,
            }),
    );
    for activity in &snapshot.activities {
        if activity.turn_id() != turn_id {
            continue;
        }
        match activity {
            Activity::Command {
                id,
                status: ActivityStatus::Active,
                ..
            } => {
                if let Some(output) = trailing_output.remove(id) {
                    changes.extend(command_output_changes(*id, output));
                }
                changes.push(SessionChange::CommandStatusChanged {
                    activity_id: *id,
                    status: ActivityStatus::Failed,
                    exit_status: None,
                });
            }
            Activity::FileChange {
                id,
                status: ActivityStatus::Active,
                ..
            } => changes.push(SessionChange::FileChangeStatusChanged {
                activity_id: *id,
                status: ActivityStatus::Failed,
            }),
            _ => {}
        }
    }
    changes
}

fn append_steer_delivery_changes(
    changes: &mut Vec<SessionChange>,
    prompt: &Prompt,
    turn_id: TurnId,
) {
    changes.extend([
        SessionChange::PromptStatusChanged {
            prompt_id: prompt.id,
            status: PromptStatus::Delivered,
        },
        SessionChange::MessageAdded {
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: prompt.text.clone(),
                truncated: false,
            },
        },
    ]);
}

fn prepare_prompt_delivery(
    prompt: Prompt,
    agent: Option<AgentIdentity>,
    turn_status: TurnStatus,
) -> (DeliveredTurn, Vec<SessionChange>) {
    let turn = Turn {
        id: TurnId::new(),
        prompt_id: prompt.id,
        agent,
        status: turn_status,
    };
    let changes = vec![
        SessionChange::PromptStatusChanged {
            prompt_id: prompt.id,
            status: PromptStatus::Delivered,
        },
        SessionChange::TurnAdded { turn: turn.clone() },
        SessionChange::MessageAdded {
            message: Message {
                id: MessageId::new(),
                turn_id: turn.id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: prompt.text.clone(),
                truncated: false,
            },
        },
    ];
    (DeliveredTurn { prompt, turn }, changes)
}

fn snapshot_for_owner(state: &SessionStoreState, owner: &PromptOwner) -> SessionSnapshot {
    state
        .sessions
        .get(&owner.session_id)
        .expect("Prompt owner always references its Session")
        .snapshot
        .clone()
}

impl PromptOwner {
    fn matches_requested_creation(&self, request: &CreateSessionRequest) -> bool {
        self.text == request.prompt.text
            && self.agent_selection == request.agent_selection
            && matches!(
                &self.origin,
                PromptOrigin::SessionCreation {
                    requested_workspace,
                    ..
                } if requested_workspace == &request.workspace.path
            )
    }

    fn canonical_creation_workspace(&self, request: &CreateSessionRequest) -> Option<PathBuf> {
        if self.text != request.prompt.text || self.agent_selection != request.agent_selection {
            return None;
        }
        match &self.origin {
            PromptOrigin::SessionCreation {
                canonical_workspace,
                ..
            } => Some(canonical_workspace.clone()),
            PromptOrigin::Admission(_) => None,
        }
    }

    fn matches_canonical_creation(&self, request: &CreateSessionRequest, workspace: &Path) -> bool {
        self.canonical_creation_workspace(request)
            .is_some_and(|canonical| canonical == workspace)
    }

    fn matches_admission(&self, session_id: SessionId, request: &AdmitPromptRequest) -> bool {
        self.session_id == session_id
            && self.text == request.prompt.text
            && matches!(&self.origin, PromptOrigin::Admission(delivery) if *delivery == request.delivery)
    }
}

impl SessionRecord {
    fn commit(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: Vec<SessionChange>,
        updated_at: SessionTimestamp,
    ) -> anyhow::Result<SessionUpdate> {
        let update = self.publish(session_id, changes)?;
        self.summary.updated_at = updated_at;
        storage.updated(self.summary.clone(), &update)?;
        let _ = self.updates.send(update.clone());
        Ok(update)
    }

    fn pending_selection_retry_prompt(&self) -> Option<PromptId> {
        let prompt_id = self.selection_retry_prompt?;
        self.snapshot
            .prompts
            .iter()
            .any(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending)
            .then_some(prompt_id)
    }

    fn next_pending_steer(&self, turn_id: TurnId) -> anyhow::Result<Option<Prompt>> {
        if active_turn_id(&self.snapshot)? != Some(turn_id) {
            return Ok(None);
        }
        Ok(self
            .snapshot
            .prompts
            .iter()
            .filter(|prompt| {
                prompt.status == PromptStatus::Pending
                    && prompt.delivery == PromptDelivery::Steer
                    && self.steer_targets.get(&prompt.id) == Some(&turn_id)
            })
            .min_by_key(|prompt| prompt.admission_order)
            .cloned())
    }

    fn pending_steer(
        &self,
        turn_id: TurnId,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<Prompt>> {
        if active_turn_id(&self.snapshot)? != Some(turn_id) {
            return Ok(None);
        }
        let prompt = self
            .snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .ok_or_else(|| anyhow!("Steering referenced an unknown Prompt"))?;
        if prompt.status != PromptStatus::Pending {
            return Ok(None);
        }
        if prompt.delivery != PromptDelivery::Steer {
            return Err(anyhow!("Steering referenced a queued Prompt"));
        }
        if self.steer_targets.get(&prompt_id) != Some(&turn_id) {
            return Err(anyhow!(
                "Steering referenced a Prompt admitted outside the active Turn"
            ));
        }
        Ok(Some(prompt.clone()))
    }

    fn publish(
        &mut self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let revision = SessionRevision(
            self.snapshot
                .revision
                .0
                .checked_add(1)
                .ok_or_else(|| anyhow!("Session revision is exhausted"))?,
        );
        let mut changes = changes
            .into_iter()
            .filter(|change| !matches!(change, SessionChange::SessionStatusChanged { .. }))
            .collect::<Vec<_>>();
        let terminal_turns = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::TurnStatusChanged { turn_id, status }
                    if *status != TurnStatus::Active =>
                {
                    Some(*turn_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut update = SessionUpdate {
            session_id,
            revision,
            changes: changes.clone(),
        };
        let mut next = self.snapshot.clone();
        apply_update(&mut next, &update)?;
        let status = derived_session_status(&next)?;
        if next.session.status != status {
            next.session.status = status;
            changes.push(SessionChange::SessionStatusChanged { status });
            update.changes = changes;
        }
        self.next_prompt_order = next
            .prompts
            .iter()
            .map(|prompt| prompt.admission_order.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .map(PromptOrder)
            .ok_or_else(|| anyhow!("Prompt admission order space is exhausted"))?;
        self.steer_targets
            .retain(|_, turn_id| !terminal_turns.contains(turn_id));
        self.snapshot = next;
        self.summary.session = self.snapshot.session.clone();
        Ok(update)
    }
}

fn agent_output_turn_id(
    snapshot: &SessionSnapshot,
    change: &SessionChange,
) -> anyhow::Result<TurnId> {
    match change {
        SessionChange::MessageAdded { message } if message.role == MessageRole::Agent => {
            Ok(message.turn_id)
        }
        SessionChange::MessageContentAppended { message_id, .. }
        | SessionChange::MessageTruncated { message_id }
        | SessionChange::MessageCompleted { message_id } => snapshot
            .messages
            .iter()
            .find(|message| message.id == *message_id && message.role == MessageRole::Agent)
            .map(|message| message.turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown Agent Message")),
        SessionChange::ActivityAdded { activity } => Ok(activity.turn_id()),
        SessionChange::CommandOutputAppended { activity_id, .. }
        | SessionChange::CommandOutputTruncated { activity_id }
        | SessionChange::CommandStatusChanged { activity_id, .. }
        | SessionChange::FileChangeUpdated { activity_id, .. }
        | SessionChange::FileChangeStatusChanged { activity_id, .. } => snapshot
            .activities
            .iter()
            .find(|activity| activity.id() == *activity_id)
            .map(Activity::turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown Activity")),
        _ => Err(anyhow!("Session change is not Agent output")),
    }
}

fn active_turn_id(snapshot: &SessionSnapshot) -> anyhow::Result<Option<TurnId>> {
    let mut active = snapshot
        .turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Active)
        .map(|turn| turn.id);
    let first = active.next();
    if active.next().is_some() {
        return Err(anyhow!("Session cannot contain more than one active Turn"));
    }
    Ok(first)
}

fn derived_session_status(snapshot: &SessionSnapshot) -> anyhow::Result<SessionStatus> {
    Ok(if active_turn_id(snapshot)?.is_some() {
        SessionStatus::Active
    } else {
        SessionStatus::Idle
    })
}

impl SessionStoreState {
    fn publish_catalog_change(&mut self, change: SessionCatalogChange) {
        self.catalog_revision = SessionCatalogRevision(
            self.catalog_revision
                .0
                .checked_add(1)
                .expect("Session catalog revision space is not exhausted"),
        );
        let _ = self.catalog_updates.send(SessionCatalogUpdate {
            revision: self.catalog_revision,
            change,
        });
    }

    fn next_timestamp(&mut self) -> SessionTimestamp {
        let current = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let timestamp = SessionTimestamp(self.last_timestamp.map_or(current, |previous| {
            current.max(previous.0.saturating_add(1))
        }));
        self.last_timestamp = Some(timestamp);
        timestamp
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::TranscriptItem;

    use super::*;

    fn settling_snapshot(
        turn_id: TurnId,
        activities: Vec<Activity>,
        messages: Vec<Message>,
    ) -> SessionSnapshot {
        SessionSnapshot {
            session: Session {
                id: SessionId::new(),
                workspace: Workspace {
                    path: PathBuf::from("/workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Active,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: vec![Turn {
                id: turn_id,
                prompt_id: PromptId::new(),
                agent: None,
                status: TurnStatus::Active,
            }],
            transcript: messages
                .iter()
                .map(|message| TranscriptItem::Message {
                    message_id: message.id,
                })
                .chain(activities.iter().map(|activity| TranscriptItem::Activity {
                    activity_id: activity.id(),
                }))
                .collect(),
            messages,
            activities,
        }
    }

    fn command(turn_id: TurnId, status: ActivityStatus) -> Activity {
        Activity::Command {
            id: ActivityId::new(),
            turn_id,
            status,
            command: "report progress".to_owned(),
            cwd: None,
            output: String::new(),
            output_truncated: false,
            exit_status: None,
        }
    }

    #[test]
    fn settling_a_turn_terminates_the_in_flight_streams_its_snapshot_still_shows() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let file_change = Activity::FileChange {
            id: ActivityId::new(),
            turn_id,
            status: ActivityStatus::Active,
            changes: Vec::new(),
        };
        let streaming = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Streaming,
            content: String::new(),
            truncated: false,
        };
        let snapshot = settling_snapshot(
            turn_id,
            vec![running.clone(), file_change.clone()],
            vec![streaming.clone()],
        );

        let changes = settle_in_flight_changes(&snapshot, turn_id, TrailingCommandOutput::new());

        assert_eq!(
            changes,
            vec![
                SessionChange::MessageCompleted {
                    message_id: streaming.id,
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
                SessionChange::FileChangeStatusChanged {
                    activity_id: file_change.id(),
                    status: ActivityStatus::Failed,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_stores_flushed_command_output_before_the_command_settles() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let snapshot = settling_snapshot(turn_id, vec![running.clone()], Vec::new());

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                running.id(),
                NormalizedText {
                    content: "progress 90%".to_owned(),
                    truncated: false,
                },
            )]),
        );

        assert_eq!(
            changes,
            vec![
                SessionChange::CommandOutputAppended {
                    activity_id: running.id(),
                    content: "progress 90%".to_owned(),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_stores_the_truncation_its_flushed_output_reached() {
        let turn_id = TurnId::new();
        let running = command(turn_id, ActivityStatus::Active);
        let snapshot = settling_snapshot(turn_id, vec![running.clone()], Vec::new());

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                running.id(),
                NormalizedText {
                    content: "as much as the cap held".to_owned(),
                    truncated: true,
                },
            )]),
        );

        assert_eq!(
            changes,
            vec![
                SessionChange::CommandOutputAppended {
                    activity_id: running.id(),
                    content: "as much as the cap held".to_owned(),
                },
                SessionChange::CommandOutputTruncated {
                    activity_id: running.id(),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running.id(),
                    status: ActivityStatus::Failed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn settling_a_turn_leaves_settled_streams_and_other_turns_alone() {
        let turn_id = TurnId::new();
        let other_turn_id = TurnId::new();
        let settled = command(turn_id, ActivityStatus::Completed);
        let elsewhere = command(other_turn_id, ActivityStatus::Active);
        let completed = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "done".to_owned(),
            truncated: false,
        };
        let user = Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "report progress".to_owned(),
            truncated: false,
        };
        let snapshot = settling_snapshot(
            turn_id,
            vec![settled.clone(), elsewhere.clone()],
            vec![user, completed],
        );

        let changes = settle_in_flight_changes(
            &snapshot,
            turn_id,
            TrailingCommandOutput::from([(
                settled.id(),
                NormalizedText {
                    content: "lost".to_owned(),
                    truncated: false,
                },
            )]),
        );

        assert!(
            changes.is_empty(),
            "a Turn with nothing in flight settles nothing: {changes:?}"
        );
    }
}
