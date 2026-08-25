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
    AgentSelection, AgentSelectionOperationId, PromptId, PromptOrder, ProviderId,
    SessionCatalogChange, SessionCatalogRevision, SessionCatalogSnapshot, SessionCatalogUpdate,
    SessionId, SessionListItem, SessionSnapshot, SessionSummary, SessionTimestamp, SessionUpdate,
    TurnId, UnreadableSessionSummary,
};
use crate::provider::ProviderResumeState;
use crate::storage::{PersistedSession, RestoredSessions, StorageSink, StoredResumeState};

mod emoji;
mod output;
mod projection;
mod prompts;
mod selection;
mod settlement;
mod title;

pub(crate) use output::{
    command_output_changes, message_content_changes, reasoning_content_changes,
};
pub(crate) use prompts::{
    AdmitPromptError, CreateSessionError, DeliveredTurn, DeliveredTurnStatus,
    PromptAdmissionDisposition, PromptMutationError,
};
pub(crate) use selection::AgentSelectionMutationError;
pub(crate) use settlement::{InterruptTurnError, ProviderTurnOutcome, TrailingCommandOutput};
pub(crate) use title::TitleDerivation;

use prompts::{PromptOrigin, PromptOwner};

const SESSION_UPDATE_CAPACITY: usize = 256;

#[derive(Clone)]
pub(crate) struct SessionStore {
    state: Arc<Mutex<SessionStoreState>>,
    storage: StorageSink,
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

pub(crate) struct SessionFeed {
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) updates: broadcast::Receiver<SessionUpdate>,
}

pub(crate) struct SessionCatalogFeed {
    pub(crate) snapshot: SessionCatalogSnapshot,
    pub(crate) updates: broadcast::Receiver<SessionCatalogUpdate>,
}

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
                        skill_invocations: prompt.skill_invocations.clone(),
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

    pub(crate) fn knows_prompt(&self, prompt_id: PromptId) -> bool {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .prompts
            .contains_key(&prompt_id)
    }

    pub(crate) fn workspace(&self, session_id: SessionId) -> Option<PathBuf> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.session.workspace.path.clone())
    }

    /// The Provider the Session was selected with, or `None` while no Agent
    /// Selection has reached it yet. ADR-0005 fixes this per Session: the
    /// orchestrator routes by it, and selection mutations may not change it.
    pub(crate) fn provider(&self, session_id: SessionId) -> Option<ProviderId> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .and_then(|record| record.snapshot.session.agent_selection.as_ref())
            .map(|selection| selection.provider.clone())
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
