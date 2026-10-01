//! The live reading of the tree a top-level Session heads: that Session and
//! every Subagent's Session beneath the one that spawned it, to any depth.
//!
//! The tree's shape is read off the Subagent rows each Session's Transcript
//! already carries — the rows the Provider-neutral Subagent orchestration
//! writes on a spawn, a resume, a settle, and an update — so every Provider is
//! covered without a word of its own. A resumed Subagent has a row for every
//! stretch of its work, all leading into its one Session, so the tree lists it
//! once, by the row its spawn left where it first spawned. Each row describes
//! only its own stretch, so where a Subagent's work stands — its Marker and
//! its time — is read from its own Session's Turns instead: the latest Turn's
//! Marker, and the time summed over them all (ADR 0031), passing over any
//! Continuation begun only to hold the row of a Subagent it delegated to after
//! its own stretch settled, where it did no work. A subscribed tree is
//! read again after each commit that could move it and compared with what its
//! subscribers last heard, so the changes they are told about are exactly the
//! difference, however the rows or Turns came to move.
//!
//! A Sidekick's Session heads a tree answering for everything its Sidekick
//! has a hand in: beneath its own Subagents stand the Sessions it began or
//! acted on, each read from its own Session and Turns as a Subagent's entry
//! is, with its own Subagents beneath it (see [`super::sidekick_acts`]). A
//! Subsession's tree is its Sidekick's, so a subscription through one is to
//! the Sidekick's, and a commit anywhere in a listed Session's tree moves
//! every tree listing it.

use std::collections::{HashMap, HashSet};

use tokio::sync::broadcast;

use crate::protocol::{
    Activity, ActivityStatus, SessionChange, SessionId, SessionTimestamp, SubagentTreeChange,
    SubagentTreeEntry, SubagentTreeRevision, SubagentTreeSession, SubagentTreeSnapshot,
    SubagentTreeTopLevel, SubagentTreeUpdate, Turn, TurnStatus,
};

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore, SessionStoreState,
    subagents::stretches_of_work,
};

/// The top-level Session of a subscribed tree with its Working and Monitoring
/// readings, as a commit found them before it landed.
pub(super) type TreeLiveness = (
    SessionId,
    Option<SessionTimestamp>,
    Option<SessionTimestamp>,
);

/// A subscription's opening snapshot, with a receiver opened at its revision.
pub(crate) struct SubagentTreeFeed {
    pub(crate) snapshot: SubagentTreeSnapshot,
    pub(crate) updates: broadcast::Receiver<SubagentTreeUpdate>,
}

/// The tree's reading without a revision, which is what two readings are
/// compared by.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SubagentTree {
    top_level: SubagentTreeTopLevel,
    subagents: Vec<SubagentTreeEntry>,
    sessions: Vec<SubagentTreeSession>,
}

/// Every tree some client is subscribed to, keyed by the top-level Session
/// heading it. A tree nobody listens to is forgotten, so a commit anywhere
/// else costs nothing more than finding that out.
#[derive(Default)]
pub(super) struct SubagentTreePublisher {
    trees: HashMap<SessionId, TreeChannel>,
}

struct TreeChannel {
    revision: SubagentTreeRevision,
    updates: broadcast::Sender<SubagentTreeUpdate>,
    /// The tree as its subscribers last heard it.
    announced: SubagentTree,
}

impl SubagentTreePublisher {
    /// Tells a tree's subscribers the tree is gone, then forgets it, ending
    /// every subscription to it once they have heard. Used when the top-level
    /// Session heading it is deleted: a subscriber learns why its stream ended
    /// rather than reconnecting to find nothing there.
    ///
    /// A Sidekick's tree listing a Subsession may be followed through it,
    /// and the Subsession outlives its Sidekick to head a tree of its own, so
    /// that tree's subscriptions end without a word instead: each subscriber
    /// asks again, finding the Subsession's own tree where it followed one,
    /// and nothing where it followed the Sidekick's Session.
    pub(super) fn invalidate(&mut self, top_level: SessionId) {
        let Some(mut channel) = self.trees.remove(&top_level) else {
            return;
        };
        if channel
            .announced
            .sessions
            .iter()
            .any(|session| session.subsession)
        {
            return;
        }
        channel.announce_change(SubagentTreeChange::TreeDeleted);
    }
}

impl TreeChannel {
    /// Sends one change at the revision after the last.
    fn announce_change(&mut self, change: SubagentTreeChange) {
        self.revision = SubagentTreeRevision(
            self.revision
                .0
                .checked_add(1)
                .expect("Subagent tree revision space is not exhausted"),
        );
        let _ = self.updates.send(SubagentTreeUpdate {
            revision: self.revision,
            change,
        });
    }
}

impl SessionStore {
    /// Subscribes to the tree `session_id` belongs to, whichever Session in it
    /// that is — the Sidekick's, for a Subsession and anything beneath one.
    /// `None` when no such Session is held, or its history, or that of the
    /// Session heading its tree, still waits to be read.
    pub(crate) fn subscribe_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> Option<SubagentTreeFeed> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        let top_level = state.tree_head(state.top_level_of(session_id)?);
        if state.is_deferred(top_level) {
            return None;
        }
        let tree = state.read_subagent_tree(top_level)?;
        // An existing subscription is brought up to this reading before
        // another joins it, so the joiner's snapshot and the revision it opens
        // at agree with what everyone else has heard.
        state.subagent_trees.announce(top_level, tree.clone());
        let channel = state
            .subagent_trees
            .trees
            .entry(top_level)
            .or_insert_with(|| TreeChannel {
                revision: SubagentTreeRevision::INITIAL,
                updates: broadcast::channel(SESSION_UPDATE_CAPACITY).0,
                announced: tree.clone(),
            });
        Some(SubagentTreeFeed {
            snapshot: SubagentTreeSnapshot {
                revision: channel.revision,
                top_level: tree.top_level,
                subagents: tree.subagents,
                sessions: tree.sessions,
            },
            updates: channel.updates.subscribe(),
        })
    }
}

impl SessionStoreState {
    /// Tells the subscribers of every tree `session_id` belongs to whatever
    /// a commit to it moved: the tree its top-level Session heads, and each
    /// Sidekick's listing that Session. Cheap when nobody subscribes to any.
    pub(super) fn announce_subagent_tree(&mut self, session_id: SessionId) {
        if self.subagent_trees.trees.is_empty() {
            return;
        }
        let Some(top_level) = self.top_level_of(session_id) else {
            return;
        };
        for tree in self.trees_listing(top_level) {
            self.announce_tree_headed_by(tree);
        }
    }

    /// Tells the subscribers of the tree `head` heads whatever moved in it.
    /// Cheap when nobody subscribes to it.
    pub(super) fn announce_tree_headed_by(&mut self, head: SessionId) {
        if !self.subagent_trees.trees.contains_key(&head) {
            return;
        }
        match self.read_subagent_tree(head) {
            Some(tree) => self.subagent_trees.announce(head, tree),
            None => self.subagent_trees.invalidate(head),
        }
    }

    /// Where the tree `session_id` belongs to stands on Working and
    /// Monitoring, taken before a commit so the commit can tell whether it
    /// moved them — a commit anywhere in the tree can, through the readings
    /// rolled up above it. `None` when nobody subscribes to that tree, nor
    /// to any Sidekick's listing it, which is what keeps this free for every
    /// commit to a tree nobody is watching.
    pub(super) fn subscribed_tree_working(&self, session_id: SessionId) -> Option<TreeLiveness> {
        if self.subagent_trees.trees.is_empty() {
            return None;
        }
        let top_level = self.top_level_of(session_id)?;
        if !self
            .trees_listing(top_level)
            .iter()
            .any(|tree| self.subagent_trees.trees.contains_key(tree))
        {
            return None;
        }
        let session = &self.sessions.get(&top_level)?.snapshot.session;
        Some((top_level, session.working_since, session.monitoring_since))
    }

    /// Whether the top-level Session's Working or Monitoring reading has moved
    /// from what [`Self::subscribed_tree_working`] took before a commit.
    pub(super) fn moved_tree_working(&self, before: Option<TreeLiveness>) -> bool {
        before.is_some_and(|(top_level, working_since, monitoring_since)| {
            self.sessions.get(&top_level).map(|record| {
                (
                    record.snapshot.session.working_since,
                    record.snapshot.session.monitoring_since,
                )
            }) != Some((working_since, monitoring_since))
        })
    }

    /// The top-level Session heading the tree `session_id` belongs to, or
    /// `None` when the Session is not held or its line up to a top-level
    /// Session is broken.
    pub(super) fn top_level_of(&self, session_id: SessionId) -> Option<SessionId> {
        let (top_level, record) = self.ancestors(session_id).last()?;
        record
            .snapshot
            .session
            .parent
            .is_none()
            .then_some(top_level)
    }

    /// Reads the tree `top_level` heads, depth-first in spawn order: and for
    /// a Sidekick's Session, each Session it has a hand in after its own
    /// Subagents, the one acted on most recently first, with that Session's
    /// Subagents after it.
    fn read_subagent_tree(&self, top_level: SessionId) -> Option<SubagentTree> {
        let record = self.sessions.get(&top_level)?;
        let mut subagents = Vec::new();
        let mut visited = HashSet::from([top_level]);
        self.read_subagents_beneath(top_level, &mut subagents, &mut visited);
        let mut sessions = Vec::new();
        for (session_id, acted_at, subsession) in self.sessions_beneath(top_level) {
            let Some(listed) = self.sessions.get(&session_id) else {
                continue;
            };
            if !visited.insert(session_id) {
                continue;
            }
            sessions.push(tree_session(listed, acted_at, subsession));
            self.read_subagents_beneath(session_id, &mut subagents, &mut visited);
        }
        Some(SubagentTree {
            top_level: SubagentTreeTopLevel {
                session_id: top_level,
                title: record.snapshot.title.clone(),
                working_since: record.snapshot.session.working_since,
                monitoring_since: record.snapshot.session.monitoring_since,
                needs_intervention: needs_intervention(record),
                sidekick: self.is_sidekicks(top_level),
            },
            subagents,
            sessions,
        })
    }

    /// Reads every Subagent beneath `spawner`, depth-first in spawn order,
    /// into `subagents`, passing over any Session `visited` already holds.
    fn read_subagents_beneath(
        &self,
        spawner: SessionId,
        subagents: &mut Vec<SubagentTreeEntry>,
        visited: &mut HashSet<SessionId>,
    ) {
        // Each frame is a spawner's Subagents still to visit, nearest first.
        let mut stack = vec![self.spawned_by(spawner).into_iter()];
        while let Some(frame) = stack.last_mut() {
            let Some(entry) = frame.next() else {
                stack.pop();
                continue;
            };
            // However many rows lead into one Session, it is one Subagent,
            // listed once.
            let child = entry.session_id;
            if !visited.insert(child) {
                continue;
            }
            subagents.push(entry);
            stack.push(self.spawned_by(child).into_iter());
        }
    }

    /// The Subagents `spawner`'s Transcript records spawning, in the order
    /// they spawned. A Subagent stands where it first spawned: under the
    /// Session its own names as parent, by the first row there leading into
    /// it. The rows its resumes add — in that Session, or in a sibling's that
    /// sent the resume — lead into the same Session and add no entry. Its
    /// Model is the latest any of its rows here carries, since each of its
    /// stretches holds the Model the Provider confirmed for that stretch. Its
    /// title is what that first row describes, or — where the row describes
    /// nothing — its Session's Title.
    fn spawned_by(&self, spawner: SessionId) -> Vec<SubagentTreeEntry> {
        let Some(record) = self.sessions.get(&spawner) else {
            return Vec::new();
        };
        let activities = &record.snapshot.activities;
        // Rows stand in Transcript order, and a later row's Model replaces
        // an earlier one's for the same Session, so the last row wins.
        let models = activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    session_id,
                    model: Some(model),
                    ..
                } => Some((*session_id, model)),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let mut listed = HashSet::new();
        activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    status,
                    name,
                    description,
                    session_id,
                    duration_ms,
                    ..
                } => Some((*session_id, name, description, *status, *duration_ms)),
                _ => None,
            })
            .filter(|(session_id, ..)| {
                // A Session not held here says nothing of its parent, so its
                // first row stands for it.
                self.sessions
                    .get(session_id)
                    .is_none_or(|child| child.snapshot.session.parent == Some(spawner))
                    && listed.insert(*session_id)
            })
            .zip(0..)
            .map(
                |((session_id, name, description, status, duration_ms), spawn_order)| {
                    let own = self.sessions.get(&session_id);
                    // Where the Subagent's work stands is its own Session's
                    // to say. Only a Session not held here leaves its first
                    // row to say it instead.
                    let work = own
                        .and_then(|record| SubagentWork::read(stretches_of_work(&record.snapshot)))
                        .unwrap_or(SubagentWork {
                            status,
                            worked_ms: duration_ms,
                            working_since: None,
                        });
                    // A spawn that said nothing of the work leaves the
                    // entry to its Session's Title, and a Session not held
                    // here to the name its row gives it.
                    let title = match description.trim() {
                        "" => {
                            own.map_or_else(|| name.clone(), |record| record.snapshot.title.clone())
                        }
                        _ => description.clone(),
                    };
                    SubagentTreeEntry {
                        session_id,
                        parent_session_id: spawner,
                        spawn_order,
                        name: name.clone(),
                        title,
                        model: models.get(&session_id).map(|&model| model.clone()),
                        status: work.status,
                        worked_ms: work.worked_ms,
                        working_since: work.working_since,
                        // Its Session's own reading, rolled up over its
                        // subtree: a settled Subagent whose Watches outlive
                        // it is Monitoring, and says so in its entry.
                        monitoring_since: own
                            .and_then(|record| record.snapshot.session.monitoring_since),
                        needs_intervention: own.is_some_and(needs_intervention),
                    }
                },
            )
            .collect()
    }
}

/// Where a Subagent's work stands, as its entry says it: read from its own
/// Session's Turns, since the Subagent itself never Settles — only its Turns
/// do — and each of its rows describes only its own stretch.
#[derive(Debug, Eq, PartialEq)]
struct SubagentWork {
    /// The Marker of the latest Turn.
    status: ActivityStatus,
    /// The settled Turns' time, summed.
    worked_ms: Option<u64>,
    /// When the latest Turn began, while it works.
    working_since: Option<SessionTimestamp>,
}

impl SubagentWork {
    /// Reads a Subagent's stretches of work, oldest first — its Turns, less
    /// any that only hold a row (see [`stretches_of_work`]): the latest one's
    /// Marker — Working while it works, and otherwise the outcome it settled
    /// with — and the time every settled one worked, summed. While the latest
    /// works, that sum is what its time counts up from, from the moment it
    /// began. Once it settles, the sum takes it in too, unless Suru never
    /// learned when its work ended, which leaves the time unsaid rather than
    /// understated; an earlier one whose end went unlearned adds nothing.
    /// `None` for a Session with no Turn at all, which says nothing.
    fn read<'a>(turns: impl IntoIterator<Item = &'a Turn>) -> Option<Self> {
        let turns = turns.into_iter().collect::<Vec<_>>();
        let (latest, earlier) = turns.split_last()?;
        let earlier_ms = earlier
            .iter()
            .filter_map(|turn| worked_span(turn))
            .fold(0_u64, u64::saturating_add);
        let status = match latest.status {
            TurnStatus::Active => {
                return Some(Self {
                    status: ActivityStatus::Active,
                    worked_ms: Some(earlier_ms),
                    working_since: latest.started_at,
                });
            }
            TurnStatus::Completed => ActivityStatus::Completed,
            TurnStatus::Failed => ActivityStatus::Failed,
            TurnStatus::Interrupted => ActivityStatus::Interrupted,
        };
        Some(Self {
            status,
            worked_ms: worked_span(latest).map(|latest_ms| latest_ms.saturating_add(earlier_ms)),
            working_since: None,
        })
    }
}

/// The entry for a Session a Sidekick has a hand in, as its own Session and
/// Turns say it, with the moment of the Sidekick's latest act on it.
fn tree_session(
    record: &SessionRecord,
    acted_at: SessionTimestamp,
    subsession: bool,
) -> SubagentTreeSession {
    let session = &record.snapshot.session;
    let work = session_work(&record.snapshot);
    SubagentTreeSession {
        session_id: session.id,
        title: record.snapshot.title.clone(),
        subsession,
        workspace_path: session.workspace.path.clone(),
        workspace_icon: session.workspace.icon.clone(),
        model: session
            .agent_selection
            .as_ref()
            .map(|selection| selection.model.clone()),
        status: work.as_ref().map(|work| work.status),
        worked_ms: work.as_ref().and_then(|work| work.worked_ms),
        working_since: work.and_then(|work| work.working_since),
        monitoring_since: session.monitoring_since,
        needs_intervention: needs_intervention(record),
        acted_at,
    }
}

/// Where a top-level Session's own work stands, read as a Subagent's is from
/// its stretches of work, except that a Session Working on a Prompt admitted
/// since its latest Turn settled — one waiting to begin a Turn of its own —
/// already works, from the moment it was admitted: a Session a Sidekick just
/// began or prompted is working before its Provider has begun its Turn.
/// Working that began before its latest Turn settled is its Subagents'
/// carrying on, which their own entries say.
fn session_work(snapshot: &crate::protocol::SessionSnapshot) -> Option<SubagentWork> {
    let stretches = stretches_of_work(snapshot);
    let latest_settled_at = stretches.last().and_then(|turn| turn.settled_at);
    let settled_ms = stretches
        .iter()
        .filter(|turn| turn.status.is_terminal())
        .filter_map(|turn| worked_span(turn))
        .fold(0_u64, u64::saturating_add);
    let work = SubagentWork::read(stretches);
    if work
        .as_ref()
        .is_some_and(|work| work.status == ActivityStatus::Active)
    {
        return work;
    }
    match snapshot.session.working_since {
        Some(since) if latest_settled_at.is_none_or(|settled_at| since >= settled_at) => {
            Some(SubagentWork {
                status: ActivityStatus::Active,
                worked_ms: Some(settled_ms),
                working_since: Some(since),
            })
        }
        _ => work,
    }
}

/// How long one settled Turn worked, where Suru learned when its work ended:
/// from the commit that began it to the one that settled it. A Turn missing
/// either moment has no span to give, and neither has one settled no later
/// than it began: a restart settles a Turn that never showed any work where
/// it began (ADR 0029), which says nothing of when that work ended.
fn worked_span(turn: &Turn) -> Option<u64> {
    turn.worked_ms().filter(|span| *span > 0)
}

/// Whether a Session's own Transcript holds a live Approval or Questionnaire —
/// the same pending readings the Sidebar's Standing counts, but this Session's
/// alone, without its Subagents'.
pub(super) fn needs_intervention(record: &SessionRecord) -> bool {
    let inputs = &record.summary.standing_inputs;
    !inputs.pending_approvals.is_empty() || !inputs.pending_questionnaires.is_empty()
}

impl SubagentTreePublisher {
    /// Announces the difference between what a tree's subscribers last heard
    /// and `tree`, one change per revision. A tree whose subscribers have all
    /// gone is forgotten instead.
    ///
    /// A difference the changes cannot say — an entry leaving the tree or
    /// moving within it, which nothing the Server does today brings about —
    /// ends every subscription instead, without a deletion, so each
    /// subscriber reconnects to a snapshot that says it whole rather than
    /// holding a tree that has quietly drifted.
    fn announce(&mut self, top_level: SessionId, tree: SubagentTree) {
        let Some(channel) = self.trees.get_mut(&top_level) else {
            return;
        };
        if channel.updates.receiver_count() == 0 {
            self.trees.remove(&top_level);
            return;
        }
        if channel.announced == tree {
            return;
        }
        let Some(changes) = tree_changes(&channel.announced, &tree) else {
            self.trees.remove(&top_level);
            return;
        };
        for change in changes {
            channel.announce_change(change);
        }
        channel.announced = tree;
    }
}

/// What moved between two readings of one tree, in the order a reader applying
/// them needs: a spawner always joins before what it spawns. `None` when the
/// difference is one no change can say: an entry gone, moved from where it
/// stood, or left without the Model it was known to run.
fn tree_changes(before: &SubagentTree, after: &SubagentTree) -> Option<Vec<SubagentTreeChange>> {
    let mut changes = Vec::new();
    if before.top_level.session_id != after.top_level.session_id
        || before.top_level.sidekick != after.top_level.sidekick
    {
        return None;
    }
    if before.top_level.title != after.top_level.title {
        changes.push(SubagentTreeChange::TopLevelRetitled {
            title: after.top_level.title.clone(),
        });
    }
    if before.top_level.working_since != after.top_level.working_since
        || before.top_level.monitoring_since != after.top_level.monitoring_since
    {
        changes.push(SubagentTreeChange::TopLevelWorkingChanged {
            working_since: after.top_level.working_since,
            monitoring_since: after.top_level.monitoring_since,
        });
    }
    if before.top_level.needs_intervention != after.top_level.needs_intervention {
        changes.push(SubagentTreeChange::NeedsInterventionChanged {
            session_id: after.top_level.session_id,
            needs_intervention: after.top_level.needs_intervention,
        });
    }
    // A Session leaving takes the Subagents beneath it along, and joins
    // before any Subagent of its own is announced beneath it.
    let listed = after
        .sessions
        .iter()
        .map(|entry| (entry.session_id, entry))
        .collect::<HashMap<_, _>>();
    let mut departed = HashSet::new();
    for entry in &before.sessions {
        if !listed.contains_key(&entry.session_id) {
            departed.insert(entry.session_id);
            changes.push(SubagentTreeChange::SessionLeft {
                session_id: entry.session_id,
            });
        }
    }
    for entry in &after.sessions {
        if !before.sessions.contains(entry) {
            changes.push(SubagentTreeChange::SessionChanged {
                entry: entry.clone(),
            });
        }
    }
    let known = before
        .subagents
        .iter()
        .map(|entry| (entry.session_id, entry))
        .collect::<HashMap<_, _>>();
    let remaining = after
        .subagents
        .iter()
        .map(|entry| entry.session_id)
        .collect::<HashSet<_>>();
    if before.subagents.iter().any(|entry| {
        !remaining.contains(&entry.session_id)
            && !departed.contains(&root_of(&known, entry.session_id))
    }) {
        return None;
    }
    for entry in &after.subagents {
        let Some(previous) = known.get(&entry.session_id) else {
            changes.push(SubagentTreeChange::SubagentSpawned {
                entry: entry.clone(),
            });
            continue;
        };
        if previous.parent_session_id != entry.parent_session_id
            || previous.spawn_order != entry.spawn_order
        {
            return None;
        }
        if previous.name != entry.name || previous.title != entry.title {
            changes.push(SubagentTreeChange::SubagentRetitled {
                session_id: entry.session_id,
                name: entry.name.clone(),
                title: entry.title.clone(),
            });
        }
        if previous.model != entry.model {
            // A Model once confirmed is only ever replaced, so a Model gone
            // is a difference no change can say.
            let model = entry.model.clone()?;
            changes.push(SubagentTreeChange::SubagentModelChanged {
                session_id: entry.session_id,
                model,
            });
        }
        if previous.status != entry.status
            || previous.worked_ms != entry.worked_ms
            || previous.working_since != entry.working_since
            || previous.monitoring_since != entry.monitoring_since
        {
            changes.push(SubagentTreeChange::SubagentWorkingChanged {
                session_id: entry.session_id,
                status: entry.status,
                worked_ms: entry.worked_ms,
                working_since: entry.working_since,
                monitoring_since: entry.monitoring_since,
            });
        }
        if previous.needs_intervention != entry.needs_intervention {
            changes.push(SubagentTreeChange::NeedsInterventionChanged {
                session_id: entry.session_id,
                needs_intervention: entry.needs_intervention,
            });
        }
    }
    Some(changes)
}

/// Whether a committed change could move a tree's rows — a Subagent's name,
/// Title, or confirmed Model among them — a Subagent's work, or the top-level
/// Title. A Turn beginning or settling in a Subagent's Session moves its
/// entry's Marker and time — a resume, or a Continuation its own work began,
/// as much as a settle — even where no row in its spawner moves with it. The
/// tree's top-level Working and Intervention readings are compared by the
/// commit itself, because what moves them is derived rather than carried by
/// any one change. A commit that moves none of them leaves every subscribed
/// tree unread.
pub(super) fn moves_subagent_tree(change: &SessionChange) -> bool {
    matches!(
        change,
        SessionChange::ActivityAdded {
            activity: Activity::Subagent { .. }
        } | SessionChange::SubagentStatusChanged { .. }
            | SessionChange::SubagentDescriptionChanged { .. }
            | SessionChange::SubagentModelChanged { .. }
            | SessionChange::TitleChanged { .. }
            | SessionChange::TurnAdded { .. }
            | SessionChange::TurnStatusChanged { .. }
            // What a Sidekick's tree says of a Session beneath it.
            | SessionChange::AgentSelectionChanged { .. }
            | SessionChange::WorkspaceChanged { .. }
    )
}

/// The Session `session_id` stands beneath at the head of its branch, read
/// up the parents `subagents` name: the first that is no Subagent of theirs,
/// which is the tree's top-level Session or a Session listed beneath it.
fn root_of(subagents: &HashMap<SessionId, &SubagentTreeEntry>, session_id: SessionId) -> SessionId {
    let mut root = session_id;
    // A Subagent cannot be its own ancestor, so a walk longer than the tree
    // is one round a cycle, and ends there.
    for _ in 0..=subagents.len() {
        let Some(entry) = subagents.get(&root) else {
            break;
        };
        root = entry.parent_session_id;
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ActivityStatus, ModelId};

    fn entry(session_id: SessionId, parent: SessionId, spawn_order: u32) -> SubagentTreeEntry {
        SubagentTreeEntry {
            session_id,
            parent_session_id: parent,
            spawn_order,
            name: "Explore".to_owned(),
            title: "Map the seams".to_owned(),
            model: None,
            status: ActivityStatus::Active,
            worked_ms: Some(0),
            working_since: Some(SessionTimestamp(1_000)),
            monitoring_since: None,
            needs_intervention: false,
        }
    }

    fn top_level(session_id: SessionId, title: &str) -> SubagentTreeTopLevel {
        SubagentTreeTopLevel {
            session_id,
            title: title.to_owned(),
            working_since: Some(SessionTimestamp(500)),
            monitoring_since: None,
            needs_intervention: false,
            sidekick: false,
        }
    }

    #[test]
    fn a_reading_that_moves_several_things_announces_each_in_tree_order() {
        let top = SessionId::new();
        let child = SessionId::new();
        let grandchild = SessionId::new();
        let before = SubagentTree {
            top_level: top_level(top, "Delegate"),
            subagents: vec![entry(child, top, 0)],
            sessions: Vec::new(),
        };
        let mut settled = entry(child, top, 0);
        settled.status = ActivityStatus::Completed;
        settled.worked_ms = Some(12);
        settled.working_since = None;
        settled.monitoring_since = Some(SessionTimestamp(1_012));
        settled.title = "Mapped the seams".to_owned();
        settled.model = Some(ModelId::new("sonnet"));
        settled.needs_intervention = true;
        let mut after_top_level = top_level(top, "Delegate the mapping");
        after_top_level.working_since = None;
        after_top_level.monitoring_since = Some(SessionTimestamp(1_012));
        after_top_level.needs_intervention = true;
        let after = SubagentTree {
            top_level: after_top_level,
            subagents: vec![settled, entry(grandchild, child, 0)],
            sessions: Vec::new(),
        };

        assert_eq!(
            tree_changes(&before, &after),
            Some(vec![
                SubagentTreeChange::TopLevelRetitled {
                    title: "Delegate the mapping".to_owned(),
                },
                SubagentTreeChange::TopLevelWorkingChanged {
                    working_since: None,
                    monitoring_since: Some(SessionTimestamp(1_012)),
                },
                SubagentTreeChange::NeedsInterventionChanged {
                    session_id: top,
                    needs_intervention: true,
                },
                SubagentTreeChange::SubagentRetitled {
                    session_id: child,
                    name: "Explore".to_owned(),
                    title: "Mapped the seams".to_owned(),
                },
                SubagentTreeChange::SubagentModelChanged {
                    session_id: child,
                    model: ModelId::new("sonnet"),
                },
                SubagentTreeChange::SubagentWorkingChanged {
                    session_id: child,
                    status: ActivityStatus::Completed,
                    worked_ms: Some(12),
                    working_since: None,
                    monitoring_since: Some(SessionTimestamp(1_012)),
                },
                SubagentTreeChange::NeedsInterventionChanged {
                    session_id: child,
                    needs_intervention: true,
                },
                SubagentTreeChange::SubagentSpawned {
                    entry: entry(grandchild, child, 0),
                },
            ])
        );
        assert_eq!(tree_changes(&after, &after), Some(Vec::new()));
    }

    fn session(session_id: SessionId, acted_at: u64) -> SubagentTreeSession {
        SubagentTreeSession {
            session_id,
            title: "Fix the flaky login test".to_owned(),
            subsession: false,
            workspace_path: std::env::temp_dir().join("auth"),
            workspace_icon: None,
            model: Some(ModelId::new("sonnet")),
            status: Some(ActivityStatus::Active),
            worked_ms: Some(0),
            working_since: Some(SessionTimestamp(1_000)),
            monitoring_since: None,
            needs_intervention: false,
            acted_at: SessionTimestamp(acted_at),
        }
    }

    #[test]
    fn a_session_beneath_a_sidekick_joins_and_moves_whole_and_leaves_with_its_subagents() {
        let sidekick = SessionId::new();
        let acted_on = SessionId::new();
        let probe = SessionId::new();
        let mut sidekicks_own = top_level(sidekick, "Plan the week");
        sidekicks_own.sidekick = true;
        let alone = SubagentTree {
            top_level: sidekicks_own.clone(),
            subagents: Vec::new(),
            sessions: Vec::new(),
        };
        let joined = SubagentTree {
            top_level: sidekicks_own.clone(),
            subagents: vec![entry(probe, acted_on, 0)],
            sessions: vec![session(acted_on, 2_000)],
        };
        assert_eq!(
            tree_changes(&alone, &joined),
            Some(vec![
                SubagentTreeChange::SessionChanged {
                    entry: session(acted_on, 2_000),
                },
                SubagentTreeChange::SubagentSpawned {
                    entry: entry(probe, acted_on, 0),
                },
            ]),
            "a Session joins before the Subagents beneath it"
        );

        let acted_again = SubagentTree {
            sessions: vec![session(acted_on, 3_000)],
            ..joined.clone()
        };
        assert_eq!(
            tree_changes(&joined, &acted_again),
            Some(vec![SubagentTreeChange::SessionChanged {
                entry: session(acted_on, 3_000),
            }]),
            "acting on it again moves it, carried whole"
        );

        assert_eq!(
            tree_changes(&acted_again, &alone),
            Some(vec![SubagentTreeChange::SessionLeft {
                session_id: acted_on,
            }]),
            "a Session leaving takes the Subagents beneath it along"
        );
        let orphaned = SubagentTree {
            subagents: Vec::new(),
            ..acted_again.clone()
        };
        assert_eq!(
            tree_changes(&acted_again, &orphaned),
            None,
            "a Subagent gone from beneath a Session still listed is no change to say"
        );
    }

    #[test]
    fn a_difference_no_change_can_say_is_left_to_a_fresh_snapshot() {
        let top = SessionId::new();
        let child = SessionId::new();
        let before = SubagentTree {
            top_level: top_level(top, "Delegate"),
            subagents: vec![entry(child, top, 0)],
            sessions: Vec::new(),
        };

        let vanished = SubagentTree {
            subagents: Vec::new(),
            ..before.clone()
        };
        assert_eq!(tree_changes(&before, &vanished), None, "an entry gone");

        let mut moved = before.clone();
        moved.subagents[0].spawn_order = 1;
        assert_eq!(tree_changes(&before, &moved), None, "an entry moved");

        let mut confirmed = before.clone();
        confirmed.subagents[0].model = Some(ModelId::new("sonnet"));
        assert_eq!(
            tree_changes(&confirmed, &before),
            None,
            "a Subagent's confirmed Model gone"
        );
    }

    fn turn(status: TurnStatus, started_at: Option<u64>, settled_at: Option<u64>) -> Turn {
        Turn {
            id: crate::protocol::TurnId::new(),
            prompt_id: None,
            compaction_requested: false,
            agent: None,
            status,
            started_at: started_at.map(SessionTimestamp),
            settled_at: settled_at.map(SessionTimestamp),
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }
    }

    fn work(
        status: ActivityStatus,
        worked_ms: Option<u64>,
        working_since: Option<u64>,
    ) -> Option<SubagentWork> {
        Some(SubagentWork {
            status,
            worked_ms,
            working_since: working_since.map(SessionTimestamp),
        })
    }

    #[test]
    fn a_continuation_that_only_holds_a_row_is_no_stretch_of_a_subagents_work() {
        use crate::protocol::{Message, MessageId, MessageRole, MessageStatus};
        use crate::sessions::{opening_subagent_row, restoration_tests::persisted};

        let mut snapshot =
            persisted(std::path::Path::new("workspace"), Some(SessionId::new())).snapshot;
        let worked = turn(TurnStatus::Failed, Some(1_000), Some(4_000));
        let holding = turn(TurnStatus::Completed, Some(5_000), Some(5_000));
        let spoke_and_spawned = turn(TurnStatus::Completed, Some(6_000), Some(7_000));
        let did_nothing = turn(TurnStatus::Completed, Some(8_000), Some(9_000));
        let spawning = turn(TurnStatus::Active, Some(10_000), None);
        let said = |turn: &Turn| Message {
            id: MessageId::new(),
            turn_id: turn.id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "Found it.".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        };
        let row = |turn: &Turn| {
            opening_subagent_row(turn.id, "Scout".to_owned(), String::new(), SessionId::new())
        };
        snapshot.messages = vec![said(&worked), said(&spoke_and_spawned)];
        snapshot.activities = vec![row(&holding), row(&spoke_and_spawned), row(&spawning)];
        snapshot.turns = vec![
            worked.clone(),
            holding,
            spoke_and_spawned.clone(),
            did_nothing.clone(),
            spawning.clone(),
        ];

        assert_eq!(
            stretches_of_work(&snapshot),
            [&worked, &spoke_and_spawned, &did_nothing, &spawning],
            "only a settled Continuation holding nothing but a row is passed over: not one that \
             also did work, nor one that did nothing, nor one still working"
        );
        snapshot.turns.truncate(2);
        assert_eq!(
            SubagentWork::read(stretches_of_work(&snapshot)),
            work(ActivityStatus::Failed, Some(3_000), None),
            "so a Subagent's entry keeps the outcome and time of its own work"
        );
    }

    #[test]
    fn a_subagents_work_is_its_latest_turns_marker_and_its_turns_time_summed() {
        assert_eq!(
            SubagentWork::read(&[turn(TurnStatus::Active, Some(1_000), None)]),
            work(ActivityStatus::Active, Some(0), Some(1_000)),
            "working its first Turn, it counts up from nothing, from when that Turn began"
        );
        let first = turn(TurnStatus::Completed, Some(1_000), Some(4_000));
        assert_eq!(
            SubagentWork::read(std::slice::from_ref(&first)),
            work(ActivityStatus::Completed, Some(3_000), None)
        );
        assert_eq!(
            SubagentWork::read(&[first.clone(), turn(TurnStatus::Active, Some(9_000), None)]),
            work(ActivityStatus::Active, Some(3_000), Some(9_000)),
            "working again, it counts up from its first Turn's time, from when the new Turn began"
        );
        assert_eq!(
            SubagentWork::read(&[
                first,
                turn(TurnStatus::Interrupted, Some(9_000), Some(11_000)),
            ]),
            work(ActivityStatus::Interrupted, Some(5_000), None),
            "settled again, it wears the new Turn's outcome over both Turns' time"
        );
        assert_eq!(
            SubagentWork::read(&[
                turn(TurnStatus::Failed, Some(1_000), Some(2_000)),
                turn(TurnStatus::Completed, Some(5_000), Some(8_000)),
            ]),
            work(ActivityStatus::Completed, Some(4_000), None),
            "a failed Turn a later one completed after says nothing of the Marker"
        );
        assert_eq!(SubagentWork::read(&[]), None, "no Turn, nothing to say");
    }

    #[test]
    fn a_settled_subagent_whose_work_ended_unlearned_leaves_its_time_unsaid() {
        assert_eq!(
            SubagentWork::read(&[turn(TurnStatus::Failed, Some(1_000), Some(1_000))]),
            work(ActivityStatus::Failed, None, None),
            "a Turn a restart settled where it began showed no work to time"
        );
        assert_eq!(
            SubagentWork::read(&[turn(TurnStatus::Completed, None, Some(3_000))]),
            work(ActivityStatus::Completed, None, None),
            "a Turn stored before Suru timed Turns has no span"
        );
        let unlearned = turn(TurnStatus::Failed, Some(1_000), Some(1_000));
        assert_eq!(
            SubagentWork::read(&[
                unlearned.clone(),
                turn(TurnStatus::Active, Some(5_000), None)
            ]),
            work(ActivityStatus::Active, Some(0), Some(5_000)),
            "an earlier Turn whose end went unlearned adds nothing to count up from"
        );
        assert_eq!(
            SubagentWork::read(&[
                unlearned,
                turn(TurnStatus::Completed, Some(5_000), Some(7_000)),
            ]),
            work(ActivityStatus::Completed, Some(2_000), None),
            "nor to the time a later settle states"
        );
    }

    /// A settled Subagent woken by its own work — a Watch firing, say — runs on
    /// in a Continuation of its own Session. Nothing moves in its spawner's
    /// Transcript, yet its entry is Working again until that Continuation
    /// settles.
    #[tokio::test]
    async fn a_continuation_of_a_settled_subagents_own_session_sets_its_entry_working() {
        use crate::{
            protocol::{
                ActivityId, AgentId, AgentIdentity, AgentSelection, CreateSessionRequest,
                InitialPrompt, ModelId, PromptId, ProviderId,
            },
            sessions::{DeliveredTurnStatus, ProviderTurnOutcome, StoreOutcome},
            storage::{StorageRepository, StorageWriter, StoredSubagentIdentity},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let StoreOutcome::Created(parent) = store
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Delegate the mapping".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create the parent Session")
        else {
            panic!("a fresh Prompt creates a Session");
        };
        let parent_id = parent.session.id;
        let delivered = store
            .deliver_prompt(
                parent_id,
                parent.prompts[0].id,
                None,
                DeliveredTurnStatus::Active,
            )
            .expect("deliver the parent's Prompt")
            .expect("the parent has no other active Turn");
        let spawned = store
            .create_subagent(
                parent_id,
                StoredSubagentIdentity {
                    provider: ProviderId::new("controlled"),
                    subagent_id: crate::provider::ProviderSubagentId::new("task-1"),
                },
                "Explore",
                "Map the seams",
                None,
            )
            .expect("spawn the Subagent");
        let child = spawned.session_id;
        let row = ActivityId::new();
        store
            .publish_agent_output(
                parent_id,
                SessionChange::ActivityAdded {
                    activity: Activity::Subagent {
                        id: row,
                        turn_id: delivered.turn_id,
                        status: ActivityStatus::Active,
                        name: "Explore".to_owned(),
                        description: "Map the seams".to_owned(),
                        model: None,
                        session_id: child,
                        brokered: false,
                        duration_ms: None,
                    },
                },
            )
            .expect("add the spawn's row");
        let settle = |turn_id| {
            store
                .finish_provider_turn(
                    child,
                    turn_id,
                    ProviderTurnOutcome::Completed {
                        trailing_output: Default::default(),
                    },
                )
                .expect("settle the Subagent's Turn");
        };
        settle(spawned.turn_id);
        store
            .publish_agent_output(
                parent_id,
                SessionChange::SubagentStatusChanged {
                    activity_id: row,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(1),
                },
            )
            .expect("settle the spawn's row");
        let span = |turn: &Turn| {
            turn.settled_at.expect("the Turn settled").0
                - turn.started_at.expect("the Turn began").0
        };
        let first_ms = span(&store.snapshot(child).expect("the child is held").turns[0]);

        let mut feed = store
            .subscribe_subagent_tree(parent_id)
            .expect("the tree is held");
        let [settled] = feed.snapshot.subagents.as_slice() else {
            panic!("the tree lists the one Subagent");
        };
        assert_eq!(
            (settled.status, settled.worked_ms, settled.working_since),
            (ActivityStatus::Completed, Some(first_ms), None)
        );

        let identity = AgentIdentity {
            agent: AgentId::new("controlled"),
            selection: AgentSelection {
                provider: ProviderId::new("controlled"),
                model: ModelId::new("model"),
                options: Vec::new(),
            },
        };
        let continuation = store
            .begin_continuation(child, identity)
            .expect("the Subagent's own work begins a Continuation");
        let began = store.snapshot(child).expect("the child is held").turns[1].started_at;
        assert_eq!(
            feed.updates.try_recv().map(|update| update.change),
            Ok(SubagentTreeChange::SubagentWorkingChanged {
                session_id: child,
                status: ActivityStatus::Active,
                worked_ms: Some(first_ms),
                working_since: began,
                monitoring_since: None,
            }),
            "the entry is Working again, counting up from its first Turn's time"
        );
        assert!(
            feed.updates.try_recv().is_err(),
            "and nothing else in the tree moved"
        );
        let rows = store
            .snapshot(parent_id)
            .expect("the parent is held")
            .activities
            .iter()
            .filter(|activity| matches!(activity, Activity::Subagent { .. }))
            .count();
        assert_eq!(rows, 1, "the Continuation adds no row to the parent");

        settle(continuation);
        let turns = store.snapshot(child).expect("the child is held").turns;
        assert_eq!(
            feed.updates.try_recv().map(|update| update.change),
            Ok(SubagentTreeChange::SubagentWorkingChanged {
                session_id: child,
                status: ActivityStatus::Completed,
                worked_ms: Some(first_ms + span(&turns[1])),
                working_since: None,
                monitoring_since: None,
            }),
            "settled, it stands at both Turns' time"
        );
    }

    /// A store holding `readable` as the process that restored them would,
    /// with its writer so the test can keep it alive.
    async fn restored(
        directory: &std::path::Path,
        readable: Vec<crate::storage::PersistedSession>,
    ) -> (SessionStore, crate::storage::StorageWriter) {
        use crate::storage::{RestoredSessions, StorageRepository, StorageWriter};

        let repository = StorageRepository::open(directory).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(
            RestoredSessions {
                readable,
                ..Default::default()
            },
            sink,
            Vec::new(),
            Default::default(),
        );
        (store, writer)
    }

    /// The title the Subagents Section gives the one entry of a tree whose
    /// top-level Session's Transcript holds a single row, named `name` and
    /// described by `description`, leading into a Subagent Session titled
    /// `title` — or into a Session the store does not hold, where `title` is
    /// `None`.
    async fn entry_title(name: &str, description: &str, title: Option<&str>) -> String {
        use crate::sessions::{opening_subagent_row, restoration_tests::persisted};

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let mut top_level = persisted(workspace, None);
        let top_level_id = top_level.snapshot.session.id;
        let mut readable = Vec::new();
        let child_id = match title {
            Some(title) => {
                let mut child = persisted(workspace, Some(top_level_id));
                child.summary.title = title.to_owned();
                child.snapshot.title = title.to_owned();
                let child_id = child.snapshot.session.id;
                readable.push(child);
                child_id
            }
            None => SessionId::new(),
        };
        top_level.snapshot.activities = vec![opening_subagent_row(
            crate::protocol::TurnId::new(),
            name.to_owned(),
            description.to_owned(),
            child_id,
        )];
        readable.insert(0, top_level);
        let (store, _writer) = restored(workspace, readable).await;

        let feed = store
            .subscribe_subagent_tree(top_level_id)
            .expect("the tree is held");
        let [entry] = &feed.snapshot.subagents[..] else {
            panic!("one row, one entry: {:?}", feed.snapshot.subagents);
        };
        entry.title.clone()
    }

    #[tokio::test]
    async fn an_entry_whose_row_describes_nothing_takes_its_sessions_title() {
        assert_eq!(
            entry_title("standards", " ", Some("Review the diff")).await,
            "Review the diff",
            "a spawn that said nothing of the work leaves the entry to its Session's Title"
        );
    }

    #[tokio::test]
    async fn an_entry_whose_row_describes_its_work_is_titled_by_that_over_its_sessions_title() {
        assert_eq!(
            entry_title("standards", "Review the diff", Some("standards")).await,
            "Review the diff"
        );
    }

    #[tokio::test]
    async fn an_entry_whose_row_describes_nothing_and_whose_session_is_not_held_takes_its_name() {
        assert_eq!(entry_title("standards", "", None).await, "standards");
    }
}
