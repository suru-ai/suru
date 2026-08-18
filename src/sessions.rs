//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::sync::broadcast;

use crate::protocol::{
    Activity, ActivityId, ActivityKind, AdmitPromptRequest, CreateSessionRequest, Message,
    MessageId, MessageRole, Prompt, PromptId, PromptStatus, Session, SessionChange, SessionId,
    SessionRevision, SessionSnapshot, SessionStatus, SessionUpdate, Turn, TurnId, TurnStatus,
    Workspace,
};

const AGENT_UNAVAILABLE: &str =
    "No Agent is selected for this Session; provider integrations are unavailable.";
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreOutcome<T> {
    Created(T),
    Existing(T),
}

#[derive(Default)]
struct SessionStoreState {
    sessions: HashMap<SessionId, SessionRecord>,
    prompts: HashMap<PromptId, PromptOwner>,
}

struct SessionRecord {
    snapshot: SessionSnapshot,
    updates: broadcast::Sender<SessionUpdate>,
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
    Steer,
}

pub(crate) struct SessionFeed {
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) updates: broadcast::Receiver<SessionUpdate>,
}

struct DeliveredTurn {
    prompt: Prompt,
    turn: Turn,
    message: Message,
    activity: Activity,
}

impl SessionStore {
    pub(crate) fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<StoreOutcome<SessionSnapshot>, CreateSessionError> {
        if request.prompt.text.trim().is_empty() {
            return Err(CreateSessionError::EmptyPrompt);
        }

        {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if let Some(owner) = state.prompts.get(&request.prompt.id) {
                let PromptOrigin::SessionCreation {
                    requested_workspace,
                    ..
                } = &owner.origin
                else {
                    return Err(CreateSessionError::PromptConflict);
                };
                if owner.text != request.prompt.text {
                    return Err(CreateSessionError::PromptConflict);
                }
                if requested_workspace == &request.workspace.path {
                    return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
                }
            }
        }

        let workspace_path = fs::canonicalize(&request.workspace.path)
            .map_err(|_| CreateSessionError::InvalidWorkspace)?;
        if !workspace_path.is_dir() {
            return Err(CreateSessionError::InvalidWorkspace);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.text == request.prompt.text
                && let PromptOrigin::SessionCreation {
                    canonical_workspace,
                    ..
                } = &owner.origin
                && canonical_workspace == &workspace_path
            {
                return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
            }
            return Err(CreateSessionError::PromptConflict);
        }

        let session_id = SessionId::new();
        let delivered = DeliveredTurn::new(request.prompt.id, request.prompt.text.clone());
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
            prompts: vec![delivered.prompt],
            turns: vec![delivered.turn],
            messages: vec![delivered.message],
            activities: vec![delivered.activity],
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
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
                updates,
            },
        );
        Ok(StoreOutcome::Created(snapshot))
    }

    pub(crate) fn admit(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<StoreOutcome<Prompt>, AdmitPromptError> {
        if request.prompt.text.trim().is_empty() {
            return Err(AdmitPromptError::EmptyPrompt);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.session_id == session_id
                && owner.text == request.prompt.text
                && matches!(&owner.origin, PromptOrigin::Steer)
            {
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
                return Ok(StoreOutcome::Existing(prompt));
            }
            return Err(AdmitPromptError::PromptConflict);
        }

        let Some(record) = state.sessions.get_mut(&session_id) else {
            return Err(AdmitPromptError::SessionNotFound);
        };
        let delivered = DeliveredTurn::new(request.prompt.id, request.prompt.text.clone());
        let revision = SessionRevision(
            record
                .snapshot
                .revision
                .0
                .checked_add(1)
                .expect("Session revision space is not exhausted"),
        );
        let update = SessionUpdate {
            session_id,
            revision,
            changes: delivered.changes(),
        };
        record.snapshot.revision = revision;
        let prompt = delivered.prompt.clone();
        delivered.append_to(&mut record.snapshot);
        let _ = record.updates.send(update);
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                origin: PromptOrigin::Steer,
            },
        );
        Ok(StoreOutcome::Created(prompt))
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
}

fn snapshot_for_owner(state: &SessionStoreState, owner: &PromptOwner) -> SessionSnapshot {
    state
        .sessions
        .get(&owner.session_id)
        .expect("Prompt owner always references its Session")
        .snapshot
        .clone()
}

impl DeliveredTurn {
    fn new(prompt_id: PromptId, text: String) -> Self {
        let turn_id = TurnId::new();
        Self {
            prompt: Prompt {
                id: prompt_id,
                text: text.clone(),
                status: PromptStatus::Delivered,
            },
            turn: Turn {
                id: turn_id,
                prompt_id,
                status: TurnStatus::Failed,
            },
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                content: text,
            },
            activity: Activity {
                id: ActivityId::new(),
                turn_id,
                kind: ActivityKind::Error,
                text: AGENT_UNAVAILABLE.to_owned(),
            },
        }
    }

    fn changes(&self) -> Vec<SessionChange> {
        vec![
            SessionChange::PromptAdded {
                prompt: self.prompt.clone(),
            },
            SessionChange::TurnAdded {
                turn: self.turn.clone(),
            },
            SessionChange::MessageAdded {
                message: self.message.clone(),
            },
            SessionChange::ActivityAdded {
                activity: self.activity.clone(),
            },
        ]
    }

    fn append_to(self, snapshot: &mut SessionSnapshot) {
        snapshot.prompts.push(self.prompt);
        snapshot.turns.push(self.turn);
        snapshot.messages.push(self.message);
        snapshot.activities.push(self.activity);
    }
}
