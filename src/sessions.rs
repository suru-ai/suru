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

use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentIdentity, CreateSessionRequest,
    Message, MessageId, MessageRole, MessageStatus, Prompt, PromptDelivery, PromptId, PromptOrder,
    PromptStatus, Session, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStatus, SessionSummary, SessionTimestamp, SessionUpdate, Turn, TurnId, TurnStatus,
    Workspace,
};
use crate::session_projection::apply_update;

const SESSION_UPDATE_CAPACITY: usize = 256;

#[derive(Clone, Default)]
pub(crate) struct SessionStore {
    state: Arc<Mutex<SessionStoreState>>,
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

#[derive(Default)]
struct SessionStoreState {
    sessions: HashMap<SessionId, SessionRecord>,
    prompts: HashMap<PromptId, PromptOwner>,
    last_timestamp: Option<SessionTimestamp>,
}

struct SessionRecord {
    snapshot: SessionSnapshot,
    summary: SessionSummary,
    updates: broadcast::Sender<SessionUpdate>,
    next_prompt_order: PromptOrder,
    steer_targets: HashMap<PromptId, TurnId>,
}

struct PromptOwner {
    session_id: SessionId,
    text: String,
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
        streaming_message_id: Option<MessageId>,
        active_command_ids: Vec<ActivityId>,
        message: String,
    },
    Interrupted {
        streaming_message_id: Option<MessageId>,
        active_command_ids: Vec<ActivityId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ListSessionsError {
    InvalidWorkspace,
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
                agent: None,
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
                origin: PromptOrigin::SessionCreation {
                    requested_workspace: request.workspace.path,
                    canonical_workspace: workspace_path,
                },
            },
        );
        state.sessions.insert(
            session_id,
            SessionRecord {
                snapshot: snapshot.clone(),
                summary,
                updates,
                next_prompt_order: PromptOrder(2),
                steer_targets: HashMap::new(),
            },
        );
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
            .publish(
                session_id,
                vec![SessionChange::PromptAdded {
                    prompt: prompt.clone(),
                }],
            )
            .expect("admission changes preserve Session invariants");
        record.next_prompt_order = next_prompt_order;
        if let Some(turn_id) = steer_target {
            record.steer_targets.insert(prompt.id, turn_id);
        }
        record.summary.updated_at = updated_at;
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
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

    pub(crate) fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.clone())
    }

    pub(crate) fn bind_agent(
        &self,
        session_id: SessionId,
        agent: AgentIdentity,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let current = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .snapshot
            .session
            .agent
            .clone();
        if current.as_ref() == Some(&agent) {
            return Ok(None);
        }
        if current.is_some() {
            return Err(anyhow!("Session already has a different Agent binding"));
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.publish(session_id, vec![SessionChange::AgentBound { agent }])?;
        record.summary.updated_at = updated_at;
        Ok(Some(update))
    }

    pub(crate) fn deliver_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
        delivered_turn_status: DeliveredTurnStatus,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = {
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
            prompt.clone()
        };
        let updated_at = state.next_timestamp();
        let (turn_status, failure_message) = match delivered_turn_status {
            DeliveredTurnStatus::Active => (TurnStatus::Active, None),
            DeliveredTurnStatus::Failed { message } => (TurnStatus::Failed, Some(message)),
        };
        let (delivered, mut changes) = prepare_prompt_delivery(prompt, turn_status);
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
        record.publish(session_id, changes)?;
        record.steer_targets.remove(&prompt_id);
        record.summary.updated_at = updated_at;
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
        record.publish(session_id, changes)?;
        record.steer_targets.remove(&prompt_id);
        record.summary.updated_at = updated_at;
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
        let update = record.publish(
            session_id,
            vec![SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            }],
        )?;
        record.steer_targets.remove(&prompt_id);
        record.summary.updated_at = updated_at;
        Ok(Some(update))
    }

    pub(crate) fn fail_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        streaming_message_id: Option<MessageId>,
        active_command_ids: Vec<ActivityId>,
        message: String,
    ) -> anyhow::Result<SessionUpdate> {
        let mut changes = terminal_output_changes(streaming_message_id, active_command_ids);
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
        self.publish(session_id, changes)
    }

    pub(crate) fn finish_provider_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        outcome: ProviderTurnOutcome,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (pending_steers, next_queued_prompt) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)? != Some(turn_id) {
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
            (pending_steers, next_queued_prompt)
        };

        let mut changes = Vec::with_capacity(pending_steers.len() * 2 + 6);
        for prompt in &pending_steers {
            append_steer_delivery_changes(&mut changes, prompt, turn_id);
        }
        match outcome {
            ProviderTurnOutcome::Completed => {
                changes.push(SessionChange::TurnStatusChanged {
                    turn_id,
                    status: TurnStatus::Completed,
                });
            }
            ProviderTurnOutcome::Failed {
                streaming_message_id,
                active_command_ids,
                message,
            } => {
                changes.extend(terminal_output_changes(
                    streaming_message_id,
                    active_command_ids,
                ));
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
            }
            ProviderTurnOutcome::Interrupted {
                streaming_message_id,
                active_command_ids,
            } => {
                changes.extend(terminal_output_changes(
                    streaming_message_id,
                    active_command_ids,
                ));
                changes.push(SessionChange::TurnStatusChanged {
                    turn_id,
                    status: TurnStatus::Interrupted,
                });
            }
        }

        let next_turn = next_queued_prompt.map(|prompt| {
            let (delivered, delivery_changes) = prepare_prompt_delivery(prompt, TurnStatus::Active);
            changes.extend(delivery_changes);
            delivered
        });

        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.publish(session_id, changes)?;
        for prompt in pending_steers {
            record.steer_targets.remove(&prompt.id);
        }
        record.summary.updated_at = updated_at;
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
        let update = stored.publish(session_id, changes)?;
        stored.summary.updated_at = updated_at;
        Ok(update)
    }

    pub(crate) fn publish_agent_output(
        &self,
        session_id: SessionId,
        change: SessionChange,
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
            let turn_id = agent_output_turn_id(&record.snapshot, &change)?;
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
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.publish(session_id, vec![change])?;
        record.summary.updated_at = updated_at;
        Ok(update)
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
                    },
                },
            ]);
        }
        record.publish(session_id, changes)?;
        record.summary.updated_at = updated_at;
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
            .publish(
                session_id,
                vec![SessionChange::PromptDeliveryChanged {
                    prompt_id,
                    delivery: PromptDelivery::Steer,
                }],
            )
            .expect("Prompt promotion preserves Session invariants");
        record.summary.updated_at = updated_at;
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
            .publish(
                session_id,
                vec![SessionChange::PromptStatusChanged {
                    prompt_id,
                    status: PromptStatus::Cancelled,
                }],
            )
            .expect("Prompt cancellation preserves Session invariants");
        record.summary.updated_at = updated_at;
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
    ) -> Result<Vec<SessionSummary>, ListSessionsError> {
        let workspace = workspace
            .map(fs::canonicalize)
            .transpose()
            .map_err(|_| ListSessionsError::InvalidWorkspace)?;
        if workspace.as_ref().is_some_and(|path| !path.is_dir()) {
            return Err(ListSessionsError::InvalidWorkspace);
        }

        let mut summaries = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .values()
            .filter(|record| {
                workspace
                    .as_ref()
                    .is_none_or(|path| record.summary.session.workspace.path == *path)
            })
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        summaries.sort_unstable_by_key(|summary| Reverse(summary.updated_at));
        Ok(summaries)
    }
}

fn terminal_output_changes(
    streaming_message_id: Option<MessageId>,
    active_command_ids: Vec<ActivityId>,
) -> Vec<SessionChange> {
    let mut changes =
        Vec::with_capacity(active_command_ids.len() + usize::from(streaming_message_id.is_some()));
    if let Some(message_id) = streaming_message_id {
        changes.push(SessionChange::MessageCompleted { message_id });
    }
    changes.extend(active_command_ids.into_iter().map(|activity_id| {
        SessionChange::CommandStatusChanged {
            activity_id,
            status: ActivityStatus::Failed,
            exit_status: None,
        }
    }));
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
            },
        },
    ]);
}

fn prepare_prompt_delivery(
    prompt: Prompt,
    turn_status: TurnStatus,
) -> (DeliveredTurn, Vec<SessionChange>) {
    let turn = Turn {
        id: TurnId::new(),
        prompt_id: prompt.id,
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
            && matches!(
                &self.origin,
                PromptOrigin::SessionCreation {
                    requested_workspace,
                    ..
                } if requested_workspace == &request.workspace.path
            )
    }

    fn canonical_creation_workspace(&self, request: &CreateSessionRequest) -> Option<PathBuf> {
        if self.text != request.prompt.text {
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
        let _ = self.updates.send(update.clone());
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
        | SessionChange::MessageCompleted { message_id } => snapshot
            .messages
            .iter()
            .find(|message| message.id == *message_id && message.role == MessageRole::Agent)
            .map(|message| message.turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown Agent Message")),
        SessionChange::ActivityAdded { activity } => Ok(activity.turn_id()),
        SessionChange::CommandOutputAppended { activity_id, .. }
        | SessionChange::CommandStatusChanged { activity_id, .. } => snapshot
            .activities
            .iter()
            .find(|activity| activity.id() == *activity_id)
            .map(Activity::turn_id)
            .ok_or_else(|| anyhow!("Agent output referenced an unknown command Activity")),
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
