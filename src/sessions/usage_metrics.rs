//! Usage Metrics: what the Sessions living on this Server consumed, read from
//! the Turns the store holds of every Session into the buckets a reader asks
//! for.
//!
//! Every Session's Turns are in memory from startup, its history deferred or
//! not (ADR 0022), so the read never hydrates one. Under the store lock it
//! only copies each Session's Turns and where it stands in its forest; it
//! indexes that copy, reads Cost Coverage from it, and adds everything up
//! outside the lock, so a large history holds up no other reader. Cost is
//! read through the same Cost Coverage the tree Costs a listing shows are
//! (ADR 0054): each contribution lands where the Turn carrying it does, so
//! over all of time the buckets add up to the Server's root Sessions' tree
//! Costs. A Remote's Session that a Sidekick here acted on is held apart from
//! the Sessions living here (ADR 0049), so it is never counted: its own
//! Server counts it.
//!
//! A Turn is broken down by the Model or Provider of the Agent that ran it,
//! or by the Workspace of the Session that owns it — a Subagent's by its
//! own Session's, not its parent's. A Cost lands under the key of the Turn
//! carrying it, so a running total over a whole tree counts under the
//! Workspace of the Session that reported it.
//!
//! A bucket counts each Session whose Turns it holds once — a top-level
//! Session apart from a Subagent — in its whole and once in each key it
//! worked under, so its keys can count more Sessions than it does. Distinct
//! counts do not add across buckets, so the report counts the buckets
//! taken together on their own.

use std::collections::{HashMap, HashSet};

use crate::protocol::{
    Cost, CostBasis, KeyedUsageMetrics, MAX_USAGE_METRICS_BUCKETS, ModelCatalog, SessionId,
    SessionTimestamp, Turn, TurnId, Usage, UsageMetricsAggregate, UsageMetricsBreakdown,
    UsageMetricsBucket, UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery,
    UsageMetricsReport, Workspace, WorkspaceId, WorkspacePaths,
};

use super::{
    SessionStore, SessionStoreState,
    cost_coverage::{CostContribution, CostForest, add_cost, tree_contributions},
};

/// Why a Usage Metrics query is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UsageMetricsRefusal {
    /// It asks for more buckets than [`MAX_USAGE_METRICS_BUCKETS`].
    TooManyBuckets,
    /// A bucket ends before it begins, or begins before the one ahead of it.
    OutOfOrder,
    /// A bucket begins before the one ahead of it ends.
    Overlapping,
}

impl UsageMetricsRefusal {
    /// Refuses a query whose buckets no report could answer one by one:
    /// too many of them, or any running out of order or overlapping
    /// another, so no Turn could be placed in two.
    fn of(query: &UsageMetricsQuery) -> Option<Self> {
        if query.buckets.len() > MAX_USAGE_METRICS_BUCKETS {
            return Some(Self::TooManyBuckets);
        }
        if query.buckets.iter().any(|bucket| bucket.end < bucket.start) {
            return Some(Self::OutOfOrder);
        }
        query.buckets.windows(2).find_map(|pair| {
            if pair[1].start < pair[0].start {
                Some(Self::OutOfOrder)
            } else if pair[1].start < pair[0].end {
                Some(Self::Overlapping)
            } else {
                None
            }
        })
    }
}

/// What the read takes of one Turn: which it is, whether a Subagent ran it,
/// where it is placed in time, the key it is broken down under, and its
/// Usage.
struct MeteredTurn {
    session_id: SessionId,
    turn_id: TurnId,
    is_subagent: bool,
    placed_at: Option<SessionTimestamp>,
    key: UsageMetricsKey,
    usage: Option<Usage>,
}

/// What the read takes of the store: every Turn it counts, what each adds
/// to the Server's Cost, and how each Workspace a Turn may be counted under
/// is presented.
struct Metered {
    turns: Vec<MeteredTurn>,
    costs: Vec<CostContribution>,
    workspaces: HashMap<WorkspaceId, Workspace>,
}

/// What names a report's keys: the Model Catalog, naming each Model and
/// Provider, and the Server's paths, naming each Workspace.
struct KeyNames<'a> {
    catalog: &'a ModelCatalog,
    workspace_paths: &'a WorkspacePaths,
}

impl SessionStore {
    /// What the Sessions living here consumed in each of `query`'s buckets,
    /// labelling each Model and Provider as `catalog` names it and each
    /// Workspace as `workspace_paths` do. A pure read: it moves no Session's
    /// revision, writes nothing, and reads no history in.
    pub(crate) fn usage_metrics(
        &self,
        query: &UsageMetricsQuery,
        catalog: &ModelCatalog,
        workspace_paths: &WorkspacePaths,
    ) -> Result<UsageMetricsReport, UsageMetricsRefusal> {
        if let Some(refusal) = UsageMetricsRefusal::of(query) {
            return Err(refusal);
        }
        let (copied, workspaces) = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .copied_forest(query.breakdown);
        let metered = MeteredForest::index(copied).metered(query.breakdown, workspaces);
        let names = KeyNames {
            catalog,
            workspace_paths,
        };
        Ok(fold(query, metered, &names))
    }
}

/// What the read copies of one Session under the store lock: its Turns,
/// and where it stands in its forest.
struct CopiedSession {
    parent: Option<SessionId>,
    workspace: WorkspaceId,
    is_brokered_subagent: bool,
    /// The Sidekick's Session that began it, where one did.
    sidekick: Option<SessionId>,
    turns: Vec<Turn>,
}

impl SessionStoreState {
    /// Every Session living here, copied as the read needs it and nothing
    /// more worked out while the lock is held, and — for a breakdown by
    /// Workspace — each of their Workspaces as the Session last updated
    /// there presents it, since every Session's copy is regrouped as its
    /// Workspace is presented anew.
    fn copied_forest(
        &self,
        breakdown: UsageMetricsBreakdown,
    ) -> (
        HashMap<SessionId, CopiedSession>,
        HashMap<WorkspaceId, Workspace>,
    ) {
        let mut presented =
            HashMap::<&WorkspaceId, (SessionTimestamp, uuid::Uuid, &Workspace)>::new();
        let sessions = self
            .sessions
            .iter()
            .map(|(session_id, record)| {
                record_work(1 + record.snapshot.turns.len());
                let workspace = &record.snapshot.session.workspace;
                if breakdown == UsageMetricsBreakdown::Workspace {
                    let seen = (record.summary.updated_at, session_id.as_uuid(), workspace);
                    presented
                        .entry(&workspace.id)
                        .and_modify(|held| {
                            if (seen.0, seen.1) > (held.0, held.1) {
                                *held = seen;
                            }
                        })
                        .or_insert(seen);
                }
                let copied = CopiedSession {
                    parent: record.snapshot.session.parent,
                    workspace: workspace.id.clone(),
                    is_brokered_subagent: record.is_brokered_subagent(),
                    sidekick: record.summary.session.sidekick(),
                    turns: record.snapshot.turns.clone(),
                };
                (*session_id, copied)
            })
            .collect();
        let workspaces = presented
            .into_iter()
            .map(|(id, (_, _, workspace))| (id.clone(), workspace.clone()))
            .collect();
        (sessions, workspaces)
    }
}

/// The copied Sessions, indexed once so each tree is walked from its root
/// without searching the whole forest at every Session.
struct MeteredForest {
    sessions: HashMap<SessionId, CopiedSession>,
    children: HashMap<SessionId, Vec<SessionId>>,
    subsessions: HashMap<SessionId, Vec<SessionId>>,
}

impl MeteredForest {
    fn index(sessions: HashMap<SessionId, CopiedSession>) -> Self {
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        let mut subsessions = HashMap::<SessionId, Vec<SessionId>>::new();
        for (session_id, session) in &sessions {
            record_work(1);
            if let Some(parent) = session.parent {
                children.entry(parent).or_default().push(*session_id);
            }
            if let Some(sidekick) = session.sidekick {
                subsessions.entry(sidekick).or_default().push(*session_id);
            }
        }
        Self {
            sessions,
            children,
            subsessions,
        }
    }

    /// Whether this Session's tree Cost is one of those the Server's Cost
    /// adds up: a top-level Session's, unless it is a Subsession whose
    /// Sidekick lives here, since the Sidekick's tree Cost carries it. Every
    /// Session reached from one of these is then counted once: a Subagent in
    /// its root's tree, and a Subsession in its Sidekick's.
    fn heads_a_cost_tree(&self, session: &CopiedSession) -> bool {
        session.parent.is_none()
            && session
                .sidekick
                .is_none_or(|sidekick| !self.sessions.contains_key(&sidekick))
    }

    /// The Turns of every Session in a tree headed here, and every Cost
    /// contribution of those trees. A Session in no such tree — a Subagent
    /// whose parent is gone, or one of a cycle — is in no tree Cost a
    /// listing shows, so neither its Usage nor its Cost is counted, and the
    /// two measures cover the same Sessions.
    fn metered(
        &self,
        breakdown: UsageMetricsBreakdown,
        workspaces: HashMap<WorkspaceId, Workspace>,
    ) -> Metered {
        let roots: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, session)| self.heads_a_cost_tree(session))
            .map(|(session_id, _)| *session_id)
            .collect();
        let costs = roots
            .iter()
            .flat_map(|root| tree_contributions(self, *root))
            .collect();
        let mut counted = HashSet::new();
        let mut turns = Vec::new();
        for root in roots {
            let trees = std::iter::once(root).chain(self.subsessions_begun_by(root));
            for session_id in trees.flat_map(|head| self.subtree_of(head)) {
                if !counted.insert(session_id) {
                    continue;
                }
                let session = &self.sessions[&session_id];
                turns.extend(session.turns.iter().map(|turn| MeteredTurn {
                    session_id,
                    turn_id: turn.id,
                    is_subagent: session.parent.is_some(),
                    placed_at: turn.started_at.or(turn.settled_at),
                    key: key_of(breakdown, session, turn),
                    usage: turn.usage.clone(),
                }));
            }
        }
        Metered {
            turns,
            costs,
            workspaces,
        }
    }
}

impl CostForest for MeteredForest {
    fn turns(&self, session_id: SessionId) -> Option<&[Turn]> {
        Some(&self.sessions.get(&session_id)?.turns)
    }

    fn parent(&self, session_id: SessionId) -> Option<SessionId> {
        self.sessions.get(&session_id)?.parent
    }

    fn is_brokered_subagent(&self, session_id: SessionId) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|session| session.is_brokered_subagent)
    }

    /// Walks the children index, each Session once, so even a Subagent
    /// caught in a cycle is reached no more than once.
    fn subtree_of(&self, session_id: SessionId) -> Vec<SessionId> {
        let mut walk = vec![session_id];
        let mut seen = HashSet::from([session_id]);
        let mut visit = 0;
        while visit < walk.len() {
            record_work(1);
            for child in self.children.get(&walk[visit]).into_iter().flatten() {
                if seen.insert(*child) {
                    walk.push(*child);
                }
            }
            visit += 1;
        }
        walk
    }

    fn subsessions_begun_by(&self, sidekick: SessionId) -> Vec<SessionId> {
        self.subsessions.get(&sidekick).cloned().unwrap_or_default()
    }
}

// Count the Sessions and Turns the read visits in tests, as restoration's
// are, so a read that searches the whole forest at every Session shows;
// production builds erase this instrumentation entirely.
#[inline]
fn record_work(_count: usize) {
    #[cfg(test)]
    super::restoration_tests::record_work(_count);
}

/// The key `turn`, one of `session`'s, is broken down under.
fn key_of(
    breakdown: UsageMetricsBreakdown,
    session: &CopiedSession,
    turn: &Turn,
) -> UsageMetricsKey {
    let selection = turn.agent.as_ref().map(|agent| &agent.selection);
    match (breakdown, selection) {
        (UsageMetricsBreakdown::Model, Some(selection)) => UsageMetricsKey::Model {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
        },
        (UsageMetricsBreakdown::Provider, Some(selection)) => UsageMetricsKey::Provider {
            provider: selection.provider.clone(),
        },
        (UsageMetricsBreakdown::Workspace, _) => UsageMetricsKey::Workspace {
            workspace: session.workspace.clone(),
        },
        (UsageMetricsBreakdown::Model | UsageMetricsBreakdown::Provider, None) => {
            UsageMetricsKey::Unknown
        }
    }
}

/// One bucket's figures as they are added up.
#[derive(Default)]
struct Tally {
    total: Part,
    by_key: HashMap<UsageMetricsKey, Part>,
}

/// A bucket's whole, or one key's part of it, as it is added up: its
/// aggregate, and the Sessions already counted in it, so each counts once.
#[derive(Default)]
struct Part {
    aggregate: UsageMetricsAggregate,
    sessions: HashSet<SessionId>,
}

impl Part {
    fn add(&mut self, turn: &MeteredTurn) {
        if let Some(usage) = &turn.usage {
            self.aggregate.add_usage(usage);
        }
        if self.sessions.insert(turn.session_id) {
            let count = if turn.is_subagent {
                &mut self.aggregate.subagent_sessions
            } else {
                &mut self.aggregate.sessions
            };
            *count += 1;
        }
    }
}

impl Tally {
    fn add(&mut self, turn: &MeteredTurn) {
        self.total.add(turn);
        match self.by_key.get_mut(&turn.key) {
            Some(keyed) => keyed.add(turn),
            None => self.by_key.entry(turn.key.clone()).or_default().add(turn),
        }
    }

    fn add_cost(&mut self, key: &UsageMetricsKey, contribution: &CostContribution) {
        add_contribution(&mut self.total.aggregate, contribution);
        let keyed = self.by_key.entry(key.clone()).or_default();
        add_contribution(&mut keyed.aggregate, contribution);
    }

    fn into_bucket(mut self, keys: &[UsageMetricsKey]) -> UsageMetricsBucket {
        UsageMetricsBucket {
            total: self.total.aggregate,
            by_key: keys
                .iter()
                .filter_map(|key| {
                    Some(KeyedUsageMetrics {
                        aggregate: self.by_key.remove(key)?.aggregate,
                        key: key.clone(),
                    })
                })
                .collect(),
        }
    }
}

/// Adds what one Turn contributes to a Cost: its amount, and its Estimated
/// portion, where it counts one, and its partial mark.
fn add_contribution(aggregate: &mut UsageMetricsAggregate, contribution: &CostContribution) {
    if let Some(amount) = contribution.amount {
        let add = |held: Option<Cost>| Some(add_cost(held, amount.cost));
        aggregate.cost = add(aggregate.cost);
        if amount.basis == CostBasis::Estimated {
            aggregate.estimated_cost = add(aggregate.estimated_cost);
        }
    }
    aggregate.is_cost_partial |= contribution.is_partial;
}

fn fold(query: &UsageMetricsQuery, metered: Metered, names: &KeyNames) -> UsageMetricsReport {
    let mut buckets = query
        .buckets
        .iter()
        .map(|_| Tally::default())
        .collect::<Vec<_>>();
    let mut untimed = Tally::default();
    // Every bucket at once, where a Session working in several counts once.
    let mut across_buckets = Tally::default();
    // The bucket a moment falls in, if any: the buckets run in order without
    // overlapping, so the one that can hold it is the first not over before it.
    let bucket_of = |placed_at: SessionTimestamp| {
        let index = query
            .buckets
            .partition_point(|bucket| bucket.end <= placed_at);
        query
            .buckets
            .get(index)
            .is_some_and(|bucket| bucket.contains(placed_at))
            .then_some(index)
    };
    let mut earliest_turn_at = None::<SessionTimestamp>;
    // Where each Turn landed, for the Cost it carries to land beside it.
    let mut placed = HashMap::with_capacity(metered.turns.len());
    for turn in metered.turns {
        let place = turn.placed_at.map(bucket_of);
        placed.insert((turn.session_id, turn.turn_id), (place, turn.key.clone()));
        let Some(placed_at) = turn.placed_at else {
            untimed.add(&turn);
            continue;
        };
        earliest_turn_at = Some(earliest_turn_at.map_or(placed_at, |held| held.min(placed_at)));
        if let Some(Some(index)) = place {
            buckets[index].add(&turn);
            across_buckets.add(&turn);
        }
    }
    for contribution in &metered.costs {
        // Every contribution is of a Turn the store holds; one that were not
        // would still count, with nothing to place it by.
        let (place, key) = placed
            .get(&(contribution.session_id, contribution.turn_id))
            .cloned()
            .unwrap_or((None, UsageMetricsKey::Unknown));
        match place {
            None => untimed.add_cost(&key, contribution),
            Some(Some(index)) => {
                buckets[index].add_cost(&key, contribution);
                across_buckets.add_cost(&key, contribution);
            }
            Some(None) => {}
        }
    }

    let mut keys = buckets
        .iter()
        .chain([&untimed])
        .flat_map(|tally| tally.by_key.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| key_order(left).cmp(&key_order(right)));
    keys.dedup();
    let labels = labels(&keys, names, &metered.workspaces);
    UsageMetricsReport {
        buckets: buckets
            .into_iter()
            .map(|tally| tally.into_bucket(&keys))
            .collect(),
        untimed: untimed.into_bucket(&keys),
        across_buckets: across_buckets.into_bucket(&keys),
        earliest_turn_at,
        keys: keys
            .into_iter()
            .zip(labels)
            .map(|(key, label)| UsageMetricsKeyLabel { key, label })
            .collect(),
    }
}

/// The order keys stand in a report: every Model by Provider and then by
/// id, every Provider by id, every Workspace by identity, and Unknown after
/// them all. One report holds keys of one breakdown alone.
fn key_order(key: &UsageMetricsKey) -> (bool, &str, &str) {
    match key {
        UsageMetricsKey::Model { provider, model } => (false, provider.as_str(), model.as_str()),
        UsageMetricsKey::Provider { provider } => (false, provider.as_str(), ""),
        UsageMetricsKey::Workspace { workspace } => (false, &workspace.0, ""),
        UsageMetricsKey::Unknown => (true, "", ""),
    }
}

/// The name a reader is shown for each of `keys`, in their order: a Model's
/// name in the Model Catalog where it carries one, a Provider's display
/// name there, each otherwise by its id; and a Workspace's name, as
/// wherever Workspaces are listed — or, where two Workspaces go by one
/// name, where each is, so no two series read alike.
fn labels(
    keys: &[UsageMetricsKey],
    names: &KeyNames,
    workspaces: &HashMap<WorkspaceId, Workspace>,
) -> Vec<String> {
    let catalog = names.catalog;
    let paths = names.workspace_paths;
    let workspace_name = |id: &WorkspaceId| {
        workspaces
            .get(id)
            .map_or_else(|| id.0.clone(), |workspace| paths.workspace_name(workspace))
    };
    let mut named = HashMap::<String, usize>::new();
    for key in keys {
        if let UsageMetricsKey::Workspace { workspace } = key {
            *named.entry(workspace_name(workspace)).or_default() += 1;
        }
    }
    keys.iter()
        .map(|key| match key {
            UsageMetricsKey::Model { provider, model } => {
                catalog.model(provider, model).map_or_else(
                    || model.as_str().to_owned(),
                    |descriptor| descriptor.display_name.clone(),
                )
            }
            UsageMetricsKey::Provider { provider } => catalog
                .providers
                .iter()
                .find(|listed| listed.provider == *provider)
                .map_or_else(
                    || provider.as_str().to_owned(),
                    |listed| listed.display_name.clone(),
                ),
            UsageMetricsKey::Workspace { workspace } => {
                let name = workspace_name(workspace);
                match workspaces.get(workspace) {
                    Some(presented) if named[&name] > 1 => paths.label(&presented.path),
                    _ => name,
                }
            }
            UsageMetricsKey::Unknown => "Unknown".to_owned(),
        })
        .collect()
}
