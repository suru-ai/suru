//! Authoritative in-memory Session ownership for one shared server instance.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use tokio::sync::{broadcast, mpsc, watch};

use crate::protocol::{
    AgentSelection, AgentSelectionOperationId, PromptId, PromptOrder, ProviderId, Session,
    SessionCatalogChange, SessionCatalogSnapshot, SessionCatalogUpdate, SessionId, SessionListItem,
    SessionSnapshot, SessionSummary, SessionTimestamp, SessionUpdate, SettingsSnapshot, TurnId,
    ViewSessionOperationId, Workspace, WorkspaceId,
};
use crate::provider::{ProviderResumeState, ProviderWatchId, Report};
use crate::storage::{
    DeferredSessions, RestoredSessions, StorageSink, StoredResumeState, StoredSubagentIdentity,
    StoredWorkspace, UnreadableStoredSession,
};

mod actor_owner;
mod brokered;
mod catalog;
mod checkouts;
pub(crate) use checkouts::CheckoutActivity;
mod compaction_fill;
mod compactions;
pub(crate) use compactions::CompactSessionError;
mod hydration;
mod output;
mod owed_output;
mod posture;
#[cfg(test)]
mod posture_tests;
mod projection;
mod prompts;
mod remote_sessions;
pub(crate) use remote_sessions::{ConfirmedBeginning, TreeBounds};
mod reports;
mod restoration;
#[cfg(test)]
mod restoration_tests;
mod selection;
mod settled;
mod settlement;
mod sidekick_acts;
pub(crate) use sidekick_acts::{Beginning, RemoteAct};
mod sidekick_reports;
mod subagent_tree;
mod subagent_waits;
mod subagents;
mod subsessions;
mod title;
mod viewed;
mod watches;
mod workspace_description;
mod workspace_icon;
mod workspaces;

pub(crate) use brokered::{
    BrokeredReadError, BrokeredResumeError, BrokeredSpawn, BrokeredSpawnCap, BrokeredSpawnError,
    BrokeredSubagentReading, DelegatingAgent,
};
pub(crate) use output::{
    command_output_changes, message_content_changes, reasoning_content_changes,
    tool_call_output_changes,
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
pub(crate) use subagents::{DeliveredDelegation, StoredSubagent, opening_subagent_row};
pub(crate) use title::{Derivation, SetIconError};
pub(crate) use viewed::ViewSessionError;
pub(crate) use workspace_description::SetWorkspaceDescriptionError;
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
    /// Every Subagent tree a client is subscribed to, and what each was last
    /// heard to say.
    subagent_trees: subagent_tree::SubagentTreePublisher,
    /// Every Sidekick's latest act on each Session it acted on, which its
    /// Session's tree lists beneath it.
    sidekick_acts: sidekick_acts::SidekickActs,
    /// What each Remote kept in view last said of the Sessions acted on
    /// there.
    remote_readings: remote_sessions::RemoteReadings,
    /// Where a Sidekick's Session is known to be, once the server has said:
    /// none until then, so nothing is a Sidekick's.
    sidekick_workspace: Option<crate::sidekick::SidekickWorkspace>,
    /// The rows recording the Subagents each Session not yet read spawned,
    /// read alone for a tree that draws it, so the tree is drawn without
    /// reading that Session's history (ADR 0022). Dropped once the history is
    /// read, which holds the same rows.
    stored_subagent_rows: HashMap<SessionId, Vec<crate::protocol::Activity>>,
    /// The current reading of every Worktree observation presently watches,
    /// deduplicated by checkout id. It holds Worktrees no Session works in
    /// alongside the ones Sessions reference, and is empty whenever no catalog
    /// subscriber is expressing interest, so a reading is only ever one taken
    /// during that interest.
    observed_checkouts: HashMap<crate::protocol::CheckoutId, crate::protocol::CheckoutSummary>,
    /// How many readings taken under source control's mutation guard each
    /// Worktree has had recorded. Observation reads without that guard, so a
    /// reading it began before the latest of them is dropped rather than
    /// written over what the guarded one recorded.
    guarded_checkout_recordings: HashMap<crate::protocol::CheckoutId, u64>,
    deferred: Option<DeferredSessions>,
    /// The Sessions whose undelivered Prompt a stored Worktree preparation can
    /// still deliver, so restoring their history leaves that Prompt standing
    /// where it withdraws every other one nothing is left to deliver.
    resumable_preparations: HashSet<SessionId>,
    /// Orders live Approval Posture application without exposing bookkeeping
    /// in the public snapshot. Provider actors do not survive restoration, so
    /// restored Sessions begin a fresh local generation sequence.
    posture_generations: HashMap<SessionId, u64>,
    /// Every Workspace's Icon and Description this server knows of, seeded
    /// from the durable `workspaces` table at startup and kept current by
    /// [`SessionStore::commit_workspace_icon`],
    /// [`SessionStore::commit_workspace_description`], and their user-facing
    /// counterparts. This is the table's whole in-memory reading —
    /// deliberately no registry of Workspaces or their Repositories, of which
    /// ADR 0027 keeps none — and it is
    /// what every `Workspace` copy this store hands out is authoritatively
    /// read against (see [`dress_workspace`]), whether resolved fresh at
    /// Session creation, regrouped by discovery, or restored from a Session's
    /// own stored metadata.
    workspaces: HashMap<WorkspaceId, StoredWorkspace>,
    /// Where the store says a Report is waiting for a Session, once
    /// Provider orchestration has asked to hear of it: the Session the Report
    /// is for, whose Agent the orchestrator then delivers it to (see
    /// [`SessionStore::announce_held_reports_to`]).
    report_notices: Option<mpsc::UnboundedSender<SessionId>>,
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
    /// The Provider's own identity for the Subagent this Session is, on a
    /// Subagent's child Session alone.
    subagent_identity: Option<StoredSubagentIdentity>,
    /// Whether this is a brokered Subagent's Session: one Suru spawned through
    /// the Broker, on a Provider the delegating Agent chose, rather than one
    /// the delegating Agent's own Provider spawned. A brokered Subagent's
    /// conversation runs on a Provider actor of its own instead of riding the
    /// actor of its nearest ancestor that owns one, as a native Subagent's
    /// does (ADR 0035). Fixed at the spawn and stored with the Session, so the
    /// next process routes its Provider work the way this one did. A
    /// top-level Session owns its actor by having no parent, so this says
    /// nothing of one: ask [`SessionRecord::owns_provider_actor`] instead.
    brokered: bool,
    /// The Watches this Session's Agent left running, by the Provider's own
    /// identity for each. They are never stored: every Watch dies with the
    /// Provider process that runs it, so a Session read back from storage has
    /// none, and the Monitoring derived from them starts over with it.
    watches: HashMap<ProviderWatchId, watches::LiveWatch>,
    /// The `wait_subagents` calls attributed to this Session that are waiting
    /// now, in the order they began, from which the Session's
    /// [`SessionSnapshot::waiting_on_subagents`] is read. Never stored: no
    /// call outlives the process answering it.
    subagent_waits: Vec<subagent_waits::OpenWait>,
    /// When an interrupt last reached the work this Session's own Provider
    /// actor runs — its Turn, or the Subagents outliving one, whether the
    /// interrupt was this Session's or carried down from one above — which
    /// stands down whatever output its Provider still owed the brokered
    /// Subagents its Agents delegated to, as it stands down what the actor
    /// owed native ones (see
    /// [`SessionStoreState::owes_continuation_to_brokered_subagents`]). Never
    /// stored: no Provider connection survives a stop to owe anything.
    work_interrupted_at: Option<SessionTimestamp>,
    /// The Turn of this brokered Subagent's Session that an interrupt of a
    /// Session above it last found working, so that, settled as stopped, it
    /// reads as stopped from above rather than on its own — by the user, or
    /// by an Agent's `stop_subagent`. A Subagent Report tells the delegating
    /// Agent of a stop on its own and of none from above (CONTEXT.md:
    /// Subagent Report), so this is read as the Turn settles, through
    /// [`SessionStoreState::stopped_by_ancestor`]. Never stored: no Turn a
    /// restart finds open settles as stopped.
    stopped_by_ancestor: Option<TurnId>,
    /// The Reports waiting for this Session's Agent, oldest first: Subagent
    /// Reports built as the brokered Subagents it delegated to settled, and —
    /// for a Sidekick — Sidekick Reports of the Sessions it set to work, not
    /// yet handed to its Provider, because none is running to take them or
    /// because the orchestrator has yet to reach it. They wait for the head of
    /// the Session's next Turn, or for a Continuation one wakes (ADR 0035).
    /// Never stored: a restart loses what was held, and only the repair of a
    /// brokered Subagent's stretch the stop left open is reported afresh.
    held_reports: VecDeque<Report>,
    /// The acts of Sidekicks on this Session that its next commit to storage
    /// carries, so each lands in the same transaction as the change it
    /// follows. Never stored apart from that change.
    acts_to_store: Vec<crate::storage::StoredSidekickAct>,
    /// The work Sidekicks set going in this Session that they are owed
    /// Sidekick Reports of, piece by piece: each Prompt one sent that no Turn
    /// has taken yet, each Turn that took one or that an Answer one gave went
    /// on in, and each such Turn's branch of Subagents working on after it
    /// settled (see [`SessionStoreState::follow_sidekick_reports`]). Never
    /// stored: a restart loses what was owed, as it loses what was held.
    sidekick_work: Vec<sidekick_reports::SidekickWork>,
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
    /// Every Session whose Provider actor held a conversation the deletion
    /// took, the deleted Session first: each actor has nothing left to serve
    /// and is closed.
    pub(crate) actor_owners: Vec<SessionId>,
}

/// Dresses one copy of a Workspace in what the `workspaces` table holds for
/// it — its Icon and its Description, or neither where it holds no row —
/// whatever that copy carried before: the table is authoritative, and every
/// copy this store hands out is read against it.
fn dress_workspace(workspaces: &HashMap<WorkspaceId, StoredWorkspace>, workspace: &mut Workspace) {
    let stored = workspaces.get(&workspace.id);
    workspace.icon = stored.and_then(|stored| stored.icon.clone());
    workspace.description = stored.and_then(|stored| stored.description.clone());
}

impl SessionStore {
    /// `resumable_preparations` names the Sessions a stored Worktree
    /// preparation can still bring to their first Turn; their Prompts are the
    /// one kind restoration leaves standing (ADR 0024).
    ///
    /// `workspaces` is the `workspaces` table's whole reading at startup.
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
        workspaces: HashMap<WorkspaceId, StoredWorkspace>,
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
            dress_workspace(&workspaces, &mut persisted.snapshot.session.workspace);
            dress_workspace(&workspaces, &mut persisted.summary.session.workspace);
            sessions.insert(
                persisted.snapshot.session.id,
                hydration::restored_record(persisted, &mut prompts),
            );
        }
        let unreadable_sessions = unreadable
            .into_iter()
            .map(|mut unreadable| {
                if let Some(workspace) = &mut unreadable.summary.workspace {
                    dress_workspace(&workspaces, workspace);
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
            subagent_trees: Default::default(),
            sidekick_acts: Default::default(),
            remote_readings: Default::default(),
            sidekick_workspace: None,
            stored_subagent_rows: HashMap::new(),
            observed_checkouts: HashMap::new(),
            guarded_checkout_recordings: HashMap::new(),
            deferred,
            resumable_preparations: resumable_preparations.into_iter().collect(),
            posture_generations: HashMap::new(),
            workspaces,
            report_notices: None,
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

    /// The Session itself, without the Transcript its snapshot carries.
    pub(crate) fn session(&self, session_id: SessionId) -> Option<Session> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.snapshot.session.clone())
    }

    /// A Session's snapshot beside the summary its listing carries, read at
    /// one moment, for a reader wanting both what a Session holds and what
    /// its listing says of it. `None` where [`Self::snapshot`] is.
    pub(crate) fn snapshot_and_summary(
        &self,
        session_id: SessionId,
    ) -> Option<(SessionSnapshot, SessionSummary)> {
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
            .map(|record| (record.snapshot.clone(), record.summary.clone()))
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
            .and_then(|record| {
                record
                    .summary
                    .session
                    .workspace
                    .repository
                    .as_deref()
                    .cloned()
            })
            .or_else(|| {
                state
                    .unreadable_sessions
                    .get(&session_id)
                    .and_then(|unreadable| unreadable.summary.workspace.as_ref())
                    .and_then(|workspace| workspace.repository.as_deref().cloned())
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
        // Every Provider actor holding a conversation the deletion takes
        // closes with it: the deleted Session's own — a top-level Session
        // owns one even when its history was unreadable and the store holds
        // no record to ask — and that of every brokered Subagent beneath it,
        // each on an actor of its own (ADR 0035). A native Subagent's
        // conversation rides one of those, and an unreadable Session below
        // never ran on one.
        let actor_owners = std::iter::once(session_id)
            .chain(state.actor_owners_beneath(session_id))
            .collect();
        // The Sidekicks' trees listing the Session, which it leaves.
        let listing = state.trees_listing(session_id);
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
        for doomed_id in &doomed {
            state.sidekick_acts.forget(*doomed_id);
            state.stored_subagent_rows.remove(doomed_id);
        }
        state.forget_sidekicks(&doomed);
        state.publish_catalog_change(SessionCatalogChange::Deleted { session_id });
        state.subagent_trees.invalidate(session_id);
        for tree in listing.into_iter().filter(|tree| *tree != session_id) {
            state.announce_tree_headed_by(tree);
        }
        Ok(DeletedSession {
            repository,
            actor_owners,
        })
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

    /// `session_id` and each Session up the line that spawned it, nearest
    /// first, for as long as the store holds them: every walk up a tree reads
    /// the tree through this. The walk ends at a top-level Session, whose
    /// line ends there, or where the line reaches a Session the store does
    /// not hold — `session_id` itself included — as restoration's orphans do.
    /// A line that loops back on itself, as a restored cyclic component's
    /// does, ends once it has yielded as many Sessions as the store holds,
    /// which no line that ends can outrun, so the walk always ends without
    /// remembering where it has been. A caller that must tell a line that
    /// reached its top from one that broke off reads the parent of the last
    /// Session yielded.
    fn ancestors(&self, session_id: SessionId) -> Ancestors<'_> {
        Ancestors {
            sessions: &self.sessions,
            next: Some(session_id),
            remaining: self.sessions.len(),
        }
    }
}

/// The walk [`SessionStoreState::ancestors`] takes.
struct Ancestors<'a> {
    sessions: &'a HashMap<SessionId, SessionRecord>,
    next: Option<SessionId>,
    remaining: usize,
}

impl<'a> Iterator for Ancestors<'a> {
    type Item = (SessionId, &'a SessionRecord);

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining = self.remaining.checked_sub(1)?;
        let session_id = self.next.take()?;
        let record = self.sessions.get(&session_id)?;
        self.next = record.snapshot.session.parent;
        Some((session_id, record))
    }
}
