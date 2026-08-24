//! Prompt admission, ordering, delivery, and the queue mutations a client drives.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::protocol::{
    Activity, ActivityId, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
    CreateSessionRequest, Message, MessageId, MessageRole, MessageStatus, ModelAvailability,
    Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, Session, SessionCatalogChange,
    SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStatus, SessionSummary,
    SessionUpdate, Turn, TurnId, TurnStatus, Workspace,
};

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore, SessionStoreState, StoreOutcome,
    projection::active_turn_id,
};

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

pub(crate) struct PromptAdmission {
    pub(crate) prompt: Prompt,
    pub(crate) disposition: PromptAdmissionDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptAdmissionDisposition {
    StartImmediately,
    SteerActive,
    RemainPending,
}

/// What starting a delivered Turn with its Provider takes. It names the Turn
/// rather than carrying a copy of it: the commit that delivers a Turn stamps
/// when it started, so a copy taken before that commit would disagree with the
/// Turn every other reader sees.
pub(crate) struct DeliveredTurn {
    pub(crate) prompt: Prompt,
    pub(crate) turn_id: TurnId,
    pub(crate) agent: Option<AgentIdentity>,
}

pub(crate) enum DeliveredTurnStatus {
    Active,
    Failed { message: String },
}

/// The Session a Prompt belongs to, alongside everything a retry of the request
/// that introduced it must match to be recognized as the same Prompt rather
/// than a conflicting one.
pub(super) struct PromptOwner {
    pub(super) session_id: SessionId,
    pub(super) text: String,
    pub(super) agent_selection: Option<AgentSelection>,
    pub(super) origin: PromptOrigin,
}

pub(super) enum PromptOrigin {
    SessionCreation {
        requested_workspace: PathBuf,
        canonical_workspace: PathBuf,
    },
    Admission(PromptDelivery),
}

impl SessionStore {
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
            // A Session begins with none: the Emoji beside its Title arrives
            // only once an Errand has derived one, and a Session that never
            // gets one draws just as cleanly.
            emoji: None,
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
                    turn_id: delivered.turn_id,
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
}

impl SessionRecord {
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
}

pub(super) fn append_steer_delivery_changes(
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

pub(super) fn prepare_prompt_delivery(
    prompt: Prompt,
    agent: Option<AgentIdentity>,
    turn_status: TurnStatus,
) -> (DeliveredTurn, Vec<SessionChange>) {
    let turn_id = TurnId::new();
    let changes = vec![
        SessionChange::PromptStatusChanged {
            prompt_id: prompt.id,
            status: PromptStatus::Delivered,
        },
        SessionChange::TurnAdded {
            turn: Turn {
                id: turn_id,
                prompt_id: prompt.id,
                agent: agent.clone(),
                status: turn_status,
                // The commit that lands this delivery stamps both, and settles
                // the Turn in the same breath when it arrives already settled.
                started_at: None,
                settled_at: None,
            },
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
    ];
    (
        DeliveredTurn {
            prompt,
            turn_id,
            agent,
        },
        changes,
    )
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
