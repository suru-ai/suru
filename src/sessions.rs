//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use tokio::sync::{broadcast, watch};

use crate::protocol::{
    AgentSelection, AgentSelectionOperationId, PromptId, PromptOrder, ProviderId,
    SessionCatalogChange, SessionCatalogSnapshot, SessionCatalogUpdate, SessionId, SessionListItem,
    SessionSnapshot, SessionSummary, SessionTimestamp, SessionUpdate, SettingsSnapshot, TurnId,
    ViewSessionOperationId, WorkspaceId,
};
use crate::provider::ProviderResumeState;
use crate::storage::{
    DeferredSessions, RestoredSessions, StorageSink, StoredResumeState, UnreadableStoredSession,
};

mod catalog;
mod checkouts;
pub(crate) use checkouts::CheckoutActivity;
mod hydration;
mod output;
mod posture;
#[cfg(test)]
mod posture_tests;
mod projection;
mod prompts;
mod restoration;
#[cfg(test)]
mod restoration_tests;
mod selection;
mod settled;
mod settlement;
mod subagents;
mod title;
mod viewed;
mod workspace_icon;
mod workspaces;

pub(crate) use output::{
    command_output_changes, message_content_changes, reasoning_content_changes,
};
pub(crate) use posture::{ApprovalPostureMutationError, ApprovalPostureUpdate};
pub(crate) use prompts::{
    AdmitPromptError, CreateSessionError, DeliveredTurn, DeliveredTurnStatus,
    PromptAdmissionDisposition, PromptMutationError, earliest_pending_prompt, effective_delivery,
};
pub(crate) use selection::AgentSelectionMutationError;
pub(crate) use settled::SettleSessionError;
pub(crate) use settlement::{
    InterruptSessionError, InterruptTarget, OpenInterventions, ProviderTurnOutcome,
    TrailingCommandOutput,
};
pub(crate) use title::{Derivation, SetIconError};
pub(crate) use viewed::ViewSessionError;
pub(crate) use workspace_icon::SetWorkspaceIconError;

use catalog::SessionCatalogPublisher;
use prompts::PromptOwner;

const SESSION_UPDATE_CAPACITY: usize = 256;

#[derive(Clone)]
pub(crate) struct SessionStore {
    state: Arc<Mutex<SessionStoreState>>,
    storage: StorageSink,
    hydration: Arc<tokio::sync::Mutex<()>>,
    /// The Server Settings a history catches up with when it is hydrated: a
    /// stored unpinned Approval Posture was shaped by the Settings of the
    /// process that wrote it, and this process may have opened with others.
    settings: watch::Receiver<SettingsSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreOutcome<T> {
    Created(T),
    Existing(T),
}

struct SessionStoreState {
    sessions: HashMap<SessionId, SessionRecord>,
    unreadable_sessions: HashMap<SessionId, UnreadableStoredSession>,
    prompts: HashMap<PromptId, PromptOwner>,
    last_timestamp: Option<SessionTimestamp>,
    catalog: SessionCatalogPublisher,
    /// The current reading of every Worktree observation presently watches,
    /// deduplicated by checkout id. It holds Worktrees no Session works in
    /// alongside the ones Sessions reference, and is empty whenever no catalog
    /// subscriber is expressing interest, so a reading is only ever one taken
    /// during that interest.
    observed_checkouts: HashMap<crate::protocol::CheckoutId, crate::protocol::CheckoutSummary>,
    deferred: Option<DeferredSessions>,
    /// The Sessions whose undelivered Prompt a stored Worktree preparation can
    /// still deliver, so restoring their history leaves that Prompt standing
    /// where it withdraws every other one nothing is left to deliver.
    resumable_preparations: HashSet<SessionId>,
    /// Orders live Approval Posture application without exposing bookkeeping
    /// in the public snapshot. Provider actors do not survive restoration, so
    /// restored Sessions begin a fresh local generation sequence.
    posture_generations: HashMap<SessionId, u64>,
    /// Every Workspace's Icon this server knows of, seeded from the durable
    /// `workspaces` table at startup and kept current by
    /// [`SessionStore::commit_workspace_icon`]. This is the table's whole
    /// in-memory reading — deliberately not a broader Workspace registry (ADR
    /// 0027) — and it is what every `Workspace` copy this store hands out is
    /// authoritatively read against, whether resolved fresh at Session
    /// creation, regrouped by discovery, or restored from a Session's own
    /// stored metadata.
    workspace_icons: HashMap<WorkspaceId, String>,
}

struct SessionRecord {
    context_fill_order: Option<(TurnId, u64)>,
    snapshot: SessionSnapshot,
    summary: SessionSummary,
    updates: broadcast::Sender<SessionUpdate>,
    next_prompt_order: PromptOrder,
    steer_targets: HashMap<PromptId, TurnId>,
    /// Every Prompt admitted to begin a Turn of its own, against the moment
    /// it was admitted — the moment its Session began Working, before any
    /// Provider was reached (ADR 0024). An entry outlives its Prompt's
    /// delivery so the Working interval it opened can be closed against the
    /// Turn that Prompt began, leaving no gap between admission and Turn; it
    /// also keeps a queued admission owed its own Turn even when its command
    /// reaches the actor after the Continuation it was admitted during has
    /// settled.
    turn_start_admissions: HashMap<PromptId, SessionTimestamp>,
    selection_operations: HashMap<AgentSelectionOperationId, AgentSelection>,
    viewed_operations: HashSet<ViewSessionOperationId>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DeleteSessionError {
    SessionNotFound,
    /// The Session is a Subagent's, whose deletion is its parent's alone:
    /// deleting it directly would leave the parent's Transcript row naming a
    /// Session that no longer exists.
    SubagentSession,
    /// Work is still running in the Session or one of its Subagents. The
    /// whole subtree must survive together until that work finishes.
    WorkingSession,
    Storage(String),
}

pub(crate) struct DeletedSession {
    /// The Repository whose orphaned Managed Worktrees may now be Reclaimed.
    /// A Session outside supported source control has no Repository pass to
    /// run after deletion.
    pub(crate) repository: Option<crate::protocol::Repository>,
}

impl SessionStore {
    /// `resumable_preparations` names the Sessions a stored Worktree
    /// preparation can still bring to their first Turn; their Prompts are the
    /// one kind restoration leaves standing (ADR 0024).
    ///
    /// `workspace_icons` is the `workspaces` table's whole reading at startup.
    /// It is applied over every restored Session's own copy of its Workspace
    /// here, once, rather than trusted from `StoredSessionMetadata`: that copy
    /// is exactly as stale as whatever the Session last committed, while the
    /// table is authoritative, so it must win regardless of which is newer.
    /// Later, on-demand history hydration ([`SessionStore::hydrate`]) already
    /// preserves grouping learned without opening that history over whatever a
    /// freshly decoded row says, which is what carries this correction
    /// forward.
    pub(crate) fn new(
        restored: RestoredSessions,
        storage: StorageSink,
        resumable_preparations: Vec<SessionId>,
        workspace_icons: HashMap<WorkspaceId, String>,
    ) -> Self {
        let started = std::time::Instant::now();
        let RestoredSessions {
            readable: persisted_sessions,
            unreadable,
            deferred,
        } = restored;
        // Every stored moment the store itself minted, so the clock resumes
        // past all of them. A Settle is minted without a commit, so it can
        // stand later than any `updated_at` and would otherwise be the one
        // moment a restored clock could hand out twice.
        let last_timestamp = persisted_sessions
            .iter()
            .flat_map(|persisted| {
                [
                    Some(persisted.summary.updated_at),
                    persisted.summary.settled_at,
                    persisted.summary.standing_inputs.viewed_at,
                ]
            })
            .flatten()
            .chain(
                unreadable
                    .iter()
                    .map(|unreadable| unreadable.summary.updated_at),
            )
            .max();
        let catalog = SessionCatalogPublisher::new();
        let mut sessions = HashMap::new();
        let mut prompts = HashMap::new();
        for mut persisted in persisted_sessions {
            let icon = workspace_icons
                .get(&persisted.snapshot.session.workspace.id)
                .cloned();
            persisted.snapshot.session.workspace.icon = icon.clone();
            persisted.summary.session.workspace.icon = icon;
            sessions.insert(
                persisted.snapshot.session.id,
                hydration::restored_record(persisted, &mut prompts),
            );
        }
        let unreadable_sessions = unreadable
            .into_iter()
            .map(|mut unreadable| {
                if let Some(workspace) = &mut unreadable.summary.workspace {
                    workspace.icon = workspace_icons.get(&workspace.id).cloned();
                }
                (unreadable.summary.id, unreadable)
            })
            .collect();
        let mut state = SessionStoreState {
            sessions,
            unreadable_sessions,
            prompts,
            last_timestamp,
            catalog,
            observed_checkouts: HashMap::new(),
            deferred,
            resumable_preparations: resumable_preparations.into_iter().collect(),
            posture_generations: HashMap::new(),
            workspace_icons,
        };
        // Durable Turns reconstruct Working and Usage before any Session can
        // be listed or opened, without committing synthetic changes.
        let projection_started = std::time::Instant::now();
        state.restore_projections();
        // A history read eagerly rather than on access is withdrawn here
        // instead; a deferred one waits for the hydration that reads it.
        let readable = state.sessions.keys().copied().collect::<Vec<_>>();
        state.withdraw_stranded_prompts(&storage, readable.clone());
        // A Turn read still open was run by a process this history outlived;
        // the same goes for one a deferred hydration reads later.
        state.settle_stopped_turns(&storage, readable);
        tracing::debug!(
            sessions = state.sessions.len(),
            projection_ms = projection_started.elapsed().as_secs_f64() * 1000.0,
            restoration_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Session restoration completed"
        );
        let (_detached, settings) = watch::channel(SettingsSnapshot::default());
        Self {
            state: Arc::new(Mutex::new(state)),
            storage,
            hydration: Arc::new(tokio::sync::Mutex::new(())),
            settings,
        }
    }

    /// Follows the live Server Settings rather than the built-in defaults a
    /// store starts with, so every history hydrated from here on reconciles
    /// against what the server is actually running under. The server always
    /// chains this; only tests, which run under the defaults, omit it.
    pub(crate) fn with_settings(mut self, settings: watch::Receiver<SettingsSnapshot>) -> Self {
        self.settings = settings;
        self
    }

    pub(crate) fn subscribe(&self, session_id: SessionId) -> Option<SessionFeed> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        let record = state.sessions.get(&session_id)?;
        Some(SessionFeed {
            snapshot: record.snapshot.clone(),
            updates: record.updates.subscribe(),
        })
    }

    pub(crate) fn subscribe_catalog(
        &self,
        workspace_paths: crate::protocol::WorkspacePaths,
    ) -> SessionCatalogFeed {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut session_ids = state
            .sessions
            .iter()
            // A Subagent's child Session rides no catalog: every client lists
            // what the catalog holds, and a child joins no listing.
            .filter(|(_, record)| !record.snapshot.session.is_subagent())
            .map(|(session_id, _)| *session_id)
            .chain(
                state
                    .unreadable_sessions
                    .keys()
                    .filter(|id| !state.is_stored_child(**id))
                    .copied(),
            )
            .collect::<Vec<_>>();
        session_ids.sort_unstable_by_key(ToString::to_string);
        // Every observed Worktree, not only the ones a Session references, so a
        // client joining or rejoining takes the whole set at once.
        let mut checkout_states = state
            .observed_checkouts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        checkout_states
            .sort_unstable_by(|left, right| left.association.id.cmp(&right.association.id));
        let (revision, updates) = state.catalog.subscribe();
        SessionCatalogFeed {
            snapshot: SessionCatalogSnapshot {
                workspace_paths,
                revision,
                session_ids,
                checkout_states,
            },
            updates,
        }
    }

    pub(crate) fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        state
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

    pub(crate) fn execution_directory(&self, session_id: SessionId) -> Option<PathBuf> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.session.execution_directory.path.clone())
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

    pub(crate) fn delete(
        &self,
        session_id: SessionId,
    ) -> Result<DeletedSession, DeleteSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        match state.sessions.get(&session_id) {
            None if !state.unreadable_sessions.contains_key(&session_id) => {
                return Err(DeleteSessionError::SessionNotFound);
            }
            Some(record) if record.snapshot.session.is_subagent() => {
                return Err(DeleteSessionError::SubagentSession);
            }
            _ if state.is_stored_child(session_id) => {
                return Err(DeleteSessionError::SubagentSession);
            }
            _ => {}
        }
        let repository = state
            .sessions
            .get(&session_id)
            .and_then(|record| record.summary.session.workspace.repository.clone())
            .or_else(|| {
                state
                    .unreadable_sessions
                    .get(&session_id)
                    .and_then(|unreadable| unreadable.summary.workspace.as_ref())
                    .and_then(|workspace| workspace.repository.clone())
            });
        // A Session's Subagent subtree shares its deletion, walked deepest
        // first so a failure partway leaves no child severed from the parent
        // that is its only way in. Only the named Session is announced,
        // because its children were never announced to begin with.
        let mut doomed = vec![session_id];
        let mut walk = 0;
        while walk < doomed.len() {
            let parent = doomed[walk];
            doomed.extend(
                state
                    .sessions
                    .iter()
                    .filter(|(_, record)| record.snapshot.session.parent == Some(parent))
                    .map(|(child_id, _)| *child_id),
            );
            if let Some(deferred) = &state.deferred {
                let unreadable_children = deferred
                    .parents
                    .iter()
                    .filter(|(id, stored_parent)| {
                        **stored_parent == Some(parent)
                            && state.unreadable_sessions.contains_key(id)
                    })
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                doomed.extend(unreadable_children);
            }
            walk += 1;
        }
        if doomed.iter().any(|doomed_id| {
            state
                .sessions
                .get(doomed_id)
                .is_some_and(|record| record.summary.session.working_since.is_some())
        }) {
            return Err(DeleteSessionError::WorkingSession);
        }
        for doomed_id in doomed.iter().rev() {
            self.storage
                .deleted(*doomed_id)
                .map_err(|error| DeleteSessionError::Storage(error.to_string()))?;
            state.sessions.remove(doomed_id);
            state.posture_generations.remove(doomed_id);
            state.unreadable_sessions.remove(doomed_id);
            if let Some(deferred) = &mut state.deferred {
                deferred.summaries.remove(doomed_id);
                deferred.parents.remove(doomed_id);
                deferred.child_ids.remove(doomed_id);
            }
        }
        state
            .prompts
            .retain(|_, owner| !doomed.contains(&owner.session_id));
        state.publish_catalog_change(SessionCatalogChange::Deleted { session_id });
        Ok(DeletedSession { repository })
    }

    pub(crate) fn list(
        &self,
        workspace: Option<&crate::protocol::WorkspaceId>,
    ) -> Vec<SessionListItem> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut summaries = state
            .sessions
            .values()
            // A Subagent's child Session joins no listing: it is reachable
            // only through the row in its parent's Transcript.
            .filter(|record| !record.snapshot.session.is_subagent())
            .map(|record| record.summary.clone())
            .filter(|summary| {
                workspace
                    .as_ref()
                    .is_none_or(|path| summary.session.workspace.id == **path)
            })
            .map(|summary| SessionListItem::Readable(Box::new(summary)))
            .chain(
                state
                    .unreadable_sessions
                    .values()
                    .filter(|unreadable| !state.is_stored_child(unreadable.summary.id))
                    .filter(|unreadable| {
                        workspace.as_ref().is_none_or(|path| {
                            unreadable
                                .summary
                                .workspace
                                .as_ref()
                                .is_none_or(|workspace| workspace.id == **path)
                        })
                    })
                    .map(|unreadable| unreadable.summary.clone())
                    .map(SessionListItem::Unreadable),
            )
            .collect::<Vec<_>>();
        summaries.sort_unstable_by_key(|summary| Reverse(summary.updated_at()));
        summaries
    }
}

impl SessionStoreState {
    fn publish_catalog_change(&mut self, change: SessionCatalogChange) {
        self.catalog.publish(change);
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
