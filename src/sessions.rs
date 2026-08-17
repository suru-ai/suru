//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    collections::HashMap,
    fs,
    sync::{Arc, Mutex},
};

use crate::protocol::{
    Activity, ActivityId, ActivityKind, CreateSessionRequest, Message, MessageId, MessageRole,
    Prompt, PromptStatus, Session, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
    Turn, TurnId, TurnStatus, Workspace,
};

const AGENT_UNAVAILABLE: &str =
    "No Agent is selected for this Session; provider integrations are unavailable.";

#[derive(Clone, Default)]
pub(crate) struct SessionStore {
    sessions: Arc<Mutex<HashMap<SessionId, SessionSnapshot>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreateSessionError {
    EmptyPrompt,
    InvalidWorkspace,
}

impl SessionStore {
    pub(crate) fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionSnapshot, CreateSessionError> {
        if request.prompt.text.trim().is_empty() {
            return Err(CreateSessionError::EmptyPrompt);
        }

        let workspace_path = fs::canonicalize(&request.workspace.path)
            .map_err(|_| CreateSessionError::InvalidWorkspace)?;
        if !workspace_path.is_dir() {
            return Err(CreateSessionError::InvalidWorkspace);
        }

        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let snapshot = SessionSnapshot {
            session: Session {
                id: session_id,
                workspace: Workspace {
                    path: workspace_path,
                },
                agent: None,
                status: SessionStatus::Idle,
            },
            revision: SessionRevision::INITIAL,
            prompts: vec![Prompt {
                id: request.prompt.id,
                text: request.prompt.text.clone(),
                status: PromptStatus::Delivered,
            }],
            turns: vec![Turn {
                id: turn_id,
                prompt_id: request.prompt.id,
                status: TurnStatus::Failed,
            }],
            messages: vec![Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                content: request.prompt.text,
            }],
            activities: vec![Activity {
                id: ActivityId::new(),
                turn_id,
                kind: ActivityKind::Error,
                text: AGENT_UNAVAILABLE.to_owned(),
            }],
        };

        self.sessions
            .lock()
            .expect("Session store lock is not poisoned")
            .insert(session_id, snapshot.clone());
        Ok(snapshot)
    }

    pub(crate) fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.sessions
            .lock()
            .expect("Session store lock is not poisoned")
            .get(&session_id)
            .cloned()
    }
}
