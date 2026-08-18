//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::protocol::{
    Activity, ActivityId, ActivityKind, AdmitPromptRequest, CreateSessionRequest, Message,
    MessageId, MessageRole, Prompt, PromptId, PromptStatus, Session, SessionChange, SessionId,
    SessionRevision, SessionSnapshot, SessionStatus, SessionUpdate, Turn, TurnId, TurnStatus,
    Workspace,
};

const AGENT_UNAVAILABLE: &str =
    "No Agent is selected for this Session; provider integrations are unavailable.";

#[derive(Clone, Default)]
pub(crate) struct SessionStore {
    state: Arc<Mutex<SessionStoreState>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCommandError {
    EmptyPrompt,
    InvalidWorkspace,
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
    updates: watch::Sender<Option<SessionUpdate>>,
}

struct PromptOwner {
    session_id: SessionId,
    text: String,
    creation_workspace: Option<PathBuf>,
}

pub(crate) struct SessionFeed {
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) updates: watch::Receiver<Option<SessionUpdate>>,
}

impl SessionStore {
    pub(crate) fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<StoreOutcome<SessionSnapshot>, SessionCommandError> {
        if request.prompt.text.trim().is_empty() {
            return Err(SessionCommandError::EmptyPrompt);
        }

        let workspace_path = fs::canonicalize(&request.workspace.path)
            .map_err(|_| SessionCommandError::InvalidWorkspace)?;
        if !workspace_path.is_dir() {
            return Err(SessionCommandError::InvalidWorkspace);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.text == request.prompt.text
                && owner.creation_workspace.as_ref() == Some(&workspace_path)
            {
                let snapshot = state
                    .sessions
                    .get(&owner.session_id)
                    .expect("Prompt owner always references its Session")
                    .snapshot
                    .clone();
                return Ok(StoreOutcome::Existing(snapshot));
            }
            return Err(SessionCommandError::PromptConflict);
        }

        let session_id = SessionId::new();
        let (prompt, turn, message, activity) =
            delivered_prompt(request.prompt.id, request.prompt.text.clone());
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
            turns: vec![turn],
            messages: vec![message],
            activities: vec![activity],
        };
        let (updates, _) = watch::channel(None);
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                creation_workspace: Some(workspace_path),
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
    ) -> Result<StoreOutcome<Prompt>, SessionCommandError> {
        if request.prompt.text.trim().is_empty() {
            return Err(SessionCommandError::EmptyPrompt);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.session_id == session_id
                && owner.text == request.prompt.text
                && owner.creation_workspace.is_none()
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
            return Err(SessionCommandError::PromptConflict);
        }

        let Some(record) = state.sessions.get_mut(&session_id) else {
            return Err(SessionCommandError::SessionNotFound);
        };
        let (prompt, turn, message, activity) =
            delivered_prompt(request.prompt.id, request.prompt.text.clone());
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
            changes: vec![
                SessionChange::PromptAdded {
                    prompt: prompt.clone(),
                },
                SessionChange::TurnAdded { turn: turn.clone() },
                SessionChange::MessageAdded {
                    message: message.clone(),
                },
                SessionChange::ActivityAdded {
                    activity: activity.clone(),
                },
            ],
        };
        record.snapshot.revision = revision;
        record.snapshot.prompts.push(prompt.clone());
        record.snapshot.turns.push(turn);
        record.snapshot.messages.push(message);
        record.snapshot.activities.push(activity);
        record.updates.send_replace(Some(update));
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                creation_workspace: None,
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

fn delivered_prompt(prompt_id: PromptId, text: String) -> (Prompt, Turn, Message, Activity) {
    let turn_id = TurnId::new();
    (
        Prompt {
            id: prompt_id,
            text: text.clone(),
            status: PromptStatus::Delivered,
        },
        Turn {
            id: turn_id,
            prompt_id,
            status: TurnStatus::Failed,
        },
        Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::User,
            content: text,
        },
        Activity {
            id: ActivityId::new(),
            turn_id,
            kind: ActivityKind::Error,
            text: AGENT_UNAVAILABLE.to_owned(),
        },
    )
}
