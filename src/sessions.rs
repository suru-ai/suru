//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    cmp::Reverse,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::sync::broadcast;

use crate::protocol::{
    Activity, ActivityId, ActivityKind, AdmitPromptRequest, CreateSessionRequest, Message,
    MessageId, MessageRole, Prompt, PromptId, PromptStatus, Session, SessionChange, SessionId,
    SessionRevision, SessionSnapshot, SessionStatus, SessionSummary, SessionTimestamp,
    SessionUpdate, Turn, TurnId, TurnStatus, Workspace,
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
    last_timestamp: Option<SessionTimestamp>,
}

struct SessionRecord {
    snapshot: SessionSnapshot,
    summary: SessionSummary,
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
        let timestamp = state.next_timestamp();
        let summary = SessionSummary {
            id: session_id,
            title,
            workspace: snapshot.session.workspace.clone(),
            agent: snapshot.session.agent.clone(),
            status: snapshot.session.status,
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
            if owner.matches_steer(session_id, &request) {
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

        if !state.sessions.contains_key(&session_id) {
            return Err(AdmitPromptError::SessionNotFound);
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
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
        record.summary.status = record.snapshot.session.status;
        record.summary.updated_at = updated_at;
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

    pub(crate) fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.clone())
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
                    .is_none_or(|path| record.summary.workspace.path == *path)
            })
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        summaries.sort_unstable_by_key(|summary| Reverse(summary.updated_at));
        Ok(summaries)
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
            PromptOrigin::Steer => None,
        }
    }

    fn matches_canonical_creation(&self, request: &CreateSessionRequest, workspace: &Path) -> bool {
        self.canonical_creation_workspace(request)
            .is_some_and(|canonical| canonical == workspace)
    }

    fn matches_steer(&self, session_id: SessionId, request: &AdmitPromptRequest) -> bool {
        self.session_id == session_id
            && self.text == request.prompt.text
            && matches!(&self.origin, PromptOrigin::Steer)
    }
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
