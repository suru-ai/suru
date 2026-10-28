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
//! taken together on their own, and the buckets and the untimed Turns
//! together likewise. A comparison interval stands apart from the buckets:
//! a Turn it holds counts there whether or not a bucket holds it too.
//!
//! The Sessions that cost the most across the buckets are the top-level
//! Sessions a reader can open, each owning the Cost its Turns and those of
//! every Subagent beneath it contribute there: a Subagent's work is what the
//! conversation that delegated it drove, and is opened from it. A Subsession
//! owns its own, apart from the Sidekick that began it, since it is a Session
//! of its own, listed and counted as one. Each is ranked once its Cost is
//! added, the few asked for picked out without sorting the rest.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use crate::protocol::{
    Cost, CostBasis, KeyedUsageMetrics, MAX_USAGE_METRICS_BUCKETS, MAX_USAGE_METRICS_TOP_SESSIONS,
    ModelCatalog, ModelId, ProviderId, SessionId, SessionTimestamp, Turn, TurnId, TurnStatus,
    Usage, UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsBucket, UsageMetricsKey,
    UsageMetricsKeyLabel, UsageMetricsQuery, UsageMetricsReport, UsageMetricsSession, Workspace,
    WorkspaceId, WorkspacePaths,
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
    /// A bucket or the comparison interval ends before it begins, or a
    /// bucket begins before the one ahead of it.
    OutOfOrder,
    /// A bucket begins before the one ahead of it ends.
    Overlapping,
    /// It asks for more top Sessions than [`MAX_USAGE_METRICS_TOP_SESSIONS`].
    TooManyTopSessions,
}

impl UsageMetricsRefusal {
    /// Refuses a query whose buckets no report could answer one by one:
    /// too many of them, or any running out of order or overlapping
    /// another, so no Turn could be placed in two; or whose comparison
    /// interval ends before it begins.
    fn of(query: &UsageMetricsQuery) -> Option<Self> {
        if query.buckets.len() > MAX_USAGE_METRICS_BUCKETS {
            return Some(Self::TooManyBuckets);
        }
        if query.top_sessions > MAX_USAGE_METRICS_TOP_SESSIONS {
            return Some(Self::TooManyTopSessions);
        }
        if query
            .buckets
            .iter()
            .chain(&query.comparison)
            .any(|bucket| bucket.end < bucket.start)
        {
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
/// the top-level Session its Cost is owned by, where it is placed in time,
/// the key it is broken down under and the Model that ran it, how it ended,
/// and its Usage.
struct MeteredTurn {
    session_id: SessionId,
    turn_id: TurnId,
    is_subagent: bool,
    owner: SessionId,
    placed_at: Option<SessionTimestamp>,
    key: UsageMetricsKey,
    /// The Model key of the Agent that ran it, whatever the breakdown,
    /// absent where its Agent is not known.
    model: Option<UsageMetricsKey>,
    status: TurnStatus,
    usage: Option<Usage>,
}

/// What the read takes of the store: every Turn it counts, and what each
/// adds to the Server's Cost.
struct Metered {
    turns: Vec<MeteredTurn>,
    costs: Vec<CostContribution>,
}

/// What names a report's keys: the Model Catalog, naming each Model and
/// Provider, and the Server's paths and how each Workspace a Turn may be
/// counted under is presented, naming each Workspace.
struct KeyNames<'a> {
    catalog: &'a ModelCatalog,
    workspace_paths: &'a WorkspacePaths,
    workspaces: HashMap<WorkspaceId, Workspace>,
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
            .copied_forest();
        let forest = MeteredForest::index(copied);
        let metered = forest.metered(query.breakdown);
        let names = KeyNames {
            catalog,
            workspace_paths,
            workspaces,
        };
        Ok(fold(query, metered, &forest, &names))
    }
}

/// What the read copies of one Session under the store lock: its Title, its
/// Turns, and where it stands in its forest.
struct CopiedSession {
    title: String,
    parent: Option<SessionId>,
    workspace: WorkspaceId,
    is_brokered_subagent: bool,
    /// The Sidekick's Session that began it, where one did.
    sidekick: Option<SessionId>,
    turns: Vec<Turn>,
}

impl SessionStoreState {
    /// Every Session living here, copied as the read needs it and nothing
    /// more worked out while the lock is held, and each of their Workspaces
    /// — which a breakdown by Workspace and the top Sessions name — as the
    /// Workspace Registry presents it, which is how every listing of
    /// Workspaces names them, or as one of its Sessions does where its entry
    /// says nowhere.
    fn copied_forest(
        &self,
    ) -> (
        HashMap<SessionId, CopiedSession>,
        HashMap<WorkspaceId, Workspace>,
    ) {
        let mut sessions_copy = HashMap::<&WorkspaceId, &Workspace>::new();
        let sessions = self
            .sessions
            .iter()
            .map(|(session_id, record)| {
                record_work(1 + record.snapshot.turns.len());
                let workspace = &record.snapshot.session.workspace;
                sessions_copy.entry(&workspace.id).or_insert(workspace);
                let copied = CopiedSession {
                    title: record.summary.title.clone(),
                    parent: record.snapshot.session.parent,
                    workspace: workspace.id.clone(),
                    is_brokered_subagent: record.is_brokered_subagent(),
                    sidekick: record.summary.session.sidekick(),
                    turns: record.snapshot.turns.clone(),
                };
                (*session_id, copied)
            })
            .collect();
        // One for each Workspace a Session here works in, however many
        // Workspaces the Registry holds.
        let workspaces = sessions_copy
            .into_iter()
            .map(|(id, copy)| {
                let presented = self
                    .registry_presentation(id)
                    .unwrap_or_else(|| copy.clone());
                (id.clone(), presented)
            })
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
    fn metered(&self, breakdown: UsageMetricsBreakdown) -> Metered {
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
            // A Subsession heads a tree of its own, owning its Cost.
            let trees = std::iter::once(root).chain(self.subsessions_begun_by(root));
            let owned = trees.flat_map(|head| {
                self.subtree_of(head)
                    .into_iter()
                    .map(move |session_id| (head, session_id))
            });
            for (owner, session_id) in owned {
                if !counted.insert(session_id) {
                    continue;
                }
                let session = &self.sessions[&session_id];
                turns.extend(session.turns.iter().map(|turn| MeteredTurn {
                    session_id,
                    turn_id: turn.id,
                    is_subagent: session.parent.is_some(),
                    owner,
                    placed_at: turn.started_at.or(turn.settled_at),
                    key: key_of(breakdown, session, turn),
                    model: match key_of(UsageMetricsBreakdown::Model, session, turn) {
                        UsageMetricsKey::Unknown => None,
                        model => Some(model),
                    },
                    status: turn.status,
                    usage: turn.usage.clone(),
                }));
            }
        }
        Metered { turns, costs }
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

/// Where a Turn, and the Cost it carries, lands among the buckets, and
/// whether in the comparison interval besides.
#[derive(Clone, Default)]
struct Place {
    bucket: Landing,
    compared: bool,
}

/// Where among the buckets a Turn lands.
#[derive(Clone, Copy, Default)]
enum Landing {
    /// In the bucket at this index.
    Bucket(usize),
    /// At a moment no bucket holds.
    Outside,
    /// At no recorded moment: among the untimed. A Cost of a Turn the store
    /// does not hold, with nothing to place it by, lands here too.
    #[default]
    Untimed,
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
        self.aggregate.turns += 1;
        match turn.status {
            TurnStatus::Failed => self.aggregate.failed_turns += 1,
            TurnStatus::Interrupted => self.aggregate.interrupted_turns += 1,
            TurnStatus::Active | TurnStatus::Completed => {}
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

/// What a top-level Session's work across the buckets adds up to, as it is
/// added: its Cost, and each Model's part of it and of its tokens, from
/// which the Model it ran most is named.
#[derive(Default)]
struct OwnedCost {
    cost: Option<Cost>,
    is_partial: bool,
    models: HashMap<UsageMetricsKey, ModelShare>,
}

/// One Model's part of a Session's work across the buckets.
#[derive(Clone, Copy, Default)]
struct ModelShare {
    cost: Option<Cost>,
    tokens: u64,
}

impl OwnedCost {
    fn add_turn(&mut self, turn: &MeteredTurn) {
        let Some(model) = &turn.model else {
            return;
        };
        let tokens = turn.usage.as_ref().and_then(Usage::blended_tokens);
        let share = self.models.entry(model.clone()).or_default();
        share.tokens = share.tokens.saturating_add(tokens.unwrap_or(0));
    }

    fn add_cost(&mut self, model: Option<&UsageMetricsKey>, contribution: &CostContribution) {
        self.is_partial |= contribution.is_partial;
        let Some(amount) = contribution.amount else {
            return;
        };
        self.cost = Some(add_cost(self.cost, amount.cost));
        if let Some(model) = model {
            let share = self.models.entry(model.clone()).or_default();
            share.cost = Some(add_cost(share.cost, amount.cost));
        }
    }

    /// The Model the work cost the most under, or used the most tokens of
    /// where none cost more than another, the first in a report's order
    /// among any still alike.
    fn main_model(&self) -> Option<&UsageMetricsKey> {
        let weight = |share: &ModelShare| (share.cost.map_or(0, Cost::nano_usd), share.tokens);
        self.models
            .iter()
            .max_by(|(left_key, left), (right_key, right)| {
                weight(left)
                    .cmp(&weight(right))
                    .then_with(|| key_order(right_key).cmp(&key_order(left_key)))
            })
            .map(|(model, _)| model)
    }
}

/// The order top Sessions stand in: the dearest first, and among those that
/// cost alike, by identity, so a report names the same ones every time.
fn dearest_first(left: &(SessionId, Cost), right: &(SessionId, Cost)) -> Ordering {
    right
        .1
        .nano_usd()
        .cmp(&left.1.nano_usd())
        .then_with(|| left.0.as_uuid().cmp(&right.0.as_uuid()))
}

/// The `limit` Sessions of `owned` that cost the most, the dearest first:
/// only those with a known Cost, the few picked out in time linear in all of
/// them and only those few sorted.
fn top_owners(owned: &HashMap<SessionId, OwnedCost>, limit: usize) -> Vec<SessionId> {
    if limit == 0 {
        return Vec::new();
    }
    let mut priced: Vec<(SessionId, Cost)> = owned
        .iter()
        .filter_map(|(owner, owned)| Some((*owner, owned.cost?)))
        .collect();
    if priced.len() > limit {
        priced.select_nth_unstable_by(limit - 1, dearest_first);
        priced.truncate(limit);
    }
    priced.sort_unstable_by(dearest_first);
    priced.into_iter().map(|(owner, _)| owner).collect()
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

/// Where a Turn landed, the key it counts under, and the top-level Session
/// and Model its Cost is owned by, for the Cost it carries to land beside it.
#[derive(Clone)]
struct Placed {
    place: Place,
    key: UsageMetricsKey,
    owner: Option<SessionId>,
    model: Option<UsageMetricsKey>,
}

impl Default for Placed {
    /// Where the Cost of a Turn the store does not hold lands: among the
    /// untimed, under Unknown, owned by no Session.
    fn default() -> Self {
        Self {
            place: Place::default(),
            key: UsageMetricsKey::Unknown,
            owner: None,
            model: None,
        }
    }
}

fn fold(
    query: &UsageMetricsQuery,
    metered: Metered,
    forest: &MeteredForest,
    names: &KeyNames,
) -> UsageMetricsReport {
    let mut buckets = query
        .buckets
        .iter()
        .map(|_| Tally::default())
        .collect::<Vec<_>>();
    let mut untimed = Tally::default();
    // Every bucket at once, where a Session working in several counts once,
    // and those and the untimed Turns at once, likewise.
    let mut across_buckets = Tally::default();
    let mut across_buckets_and_untimed = Part::default();
    let mut comparison = query.comparison.map(|_| Part::default());
    let compared = |placed_at: Option<SessionTimestamp>| {
        placed_at.is_some_and(|placed_at| {
            query
                .comparison
                .is_some_and(|comparison| comparison.contains(placed_at))
        })
    };
    // The bucket a moment falls in, if any: the buckets run in order without
    // overlapping, so the one that can hold it is the first not over before it.
    let landing = |placed_at: Option<SessionTimestamp>| {
        let Some(placed_at) = placed_at else {
            return Landing::Untimed;
        };
        let index = query
            .buckets
            .partition_point(|bucket| bucket.end <= placed_at);
        if query
            .buckets
            .get(index)
            .is_some_and(|bucket| bucket.contains(placed_at))
        {
            Landing::Bucket(index)
        } else {
            Landing::Outside
        }
    };
    let mut earliest_turn_at = None::<SessionTimestamp>;
    // What each top-level Session's work across the buckets adds up to.
    let mut owned = HashMap::<SessionId, OwnedCost>::new();
    // Where each Turn landed, for the Cost it carries to land beside it.
    let mut placed = HashMap::with_capacity(metered.turns.len());
    for turn in metered.turns {
        let place = Place {
            bucket: landing(turn.placed_at),
            compared: compared(turn.placed_at),
        };
        if let Some(comparison) = comparison.as_mut().filter(|_| place.compared) {
            comparison.add(&turn);
        }
        placed.insert(
            (turn.session_id, turn.turn_id),
            Placed {
                place: place.clone(),
                key: turn.key.clone(),
                owner: Some(turn.owner),
                model: turn.model.clone(),
            },
        );
        if let Some(placed_at) = turn.placed_at {
            earliest_turn_at = Some(earliest_turn_at.map_or(placed_at, |held| held.min(placed_at)));
        }
        match place.bucket {
            Landing::Bucket(index) => {
                buckets[index].add(&turn);
                across_buckets.add(&turn);
                owned.entry(turn.owner).or_default().add_turn(&turn);
            }
            Landing::Untimed => untimed.add(&turn),
            Landing::Outside => continue,
        }
        across_buckets_and_untimed.add(&turn);
    }
    for contribution in &metered.costs {
        // Every contribution is of a Turn the store holds; one that were not
        // would still count, among the untimed.
        let Placed {
            place,
            key,
            owner,
            model,
        } = placed
            .get(&(contribution.session_id, contribution.turn_id))
            .cloned()
            .unwrap_or_default();
        if let Some(comparison) = comparison.as_mut().filter(|_| place.compared) {
            add_contribution(&mut comparison.aggregate, contribution);
        }
        match place.bucket {
            Landing::Bucket(index) => {
                buckets[index].add_cost(&key, contribution);
                across_buckets.add_cost(&key, contribution);
                if let Some(owner) = owner {
                    owned
                        .entry(owner)
                        .or_default()
                        .add_cost(model.as_ref(), contribution);
                }
            }
            Landing::Untimed => untimed.add_cost(&key, contribution),
            Landing::Outside => continue,
        }
        add_contribution(&mut across_buckets_and_untimed.aggregate, contribution);
    }

    let mut keys = buckets
        .iter()
        .chain([&untimed])
        .flat_map(|tally| tally.by_key.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| key_order(left).cmp(&key_order(right)));
    keys.dedup();
    let top = top_owners(&owned, query.top_sessions);
    // The top Sessions' Workspaces and Models are named with the keys, so a
    // Workspace is told apart from every other the report names alike.
    let named = top.iter().flat_map(|owner| {
        let workspace = UsageMetricsKey::Workspace {
            workspace: forest.sessions[owner].workspace.clone(),
        };
        std::iter::once(workspace).chain(owned[owner].main_model().cloned())
    });
    let mut named = keys.iter().cloned().chain(named).collect::<Vec<_>>();
    named.sort_by(|left, right| key_order(left).cmp(&key_order(right)));
    named.dedup();
    let mut labels: HashMap<UsageMetricsKey, String> =
        named.iter().cloned().zip(labels(&named, names)).collect();
    let top_sessions = top
        .into_iter()
        .map(|owner| {
            let session = &forest.sessions[&owner];
            let owned = &owned[&owner];
            let workspace = UsageMetricsKey::Workspace {
                workspace: session.workspace.clone(),
            };
            UsageMetricsSession {
                session_id: owner,
                title: session.title.clone(),
                workspace: labels
                    .get(&workspace)
                    .cloned()
                    .expect("every top Session's Workspace is named"),
                model: owned.main_model().map(|model| {
                    labels
                        .get(model)
                        .cloned()
                        .expect("every top Session's Model is named")
                }),
                cost: owned.cost.expect("a top Session has a known Cost"),
                is_cost_partial: owned.is_partial,
            }
        })
        .collect();
    UsageMetricsReport {
        buckets: buckets
            .into_iter()
            .map(|tally| tally.into_bucket(&keys))
            .collect(),
        untimed: untimed.into_bucket(&keys),
        across_buckets: across_buckets.into_bucket(&keys),
        across_buckets_and_untimed: across_buckets_and_untimed.aggregate,
        comparison: comparison.map(|comparison| comparison.aggregate),
        earliest_turn_at,
        keys: keys
            .into_iter()
            .map(|key| UsageMetricsKeyLabel {
                label: labels.remove(&key).expect("every key is named"),
                key,
            })
            .collect(),
        top_sessions,
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
/// as [`model_labels`] tells them apart, a Provider's display name in the
/// Model Catalog, otherwise its id; and a Workspace's as
/// [`workspace_labels`] tells them apart.
fn labels(keys: &[UsageMetricsKey], names: &KeyNames) -> Vec<String> {
    let catalog = names.catalog;
    let mut models = model_labels(keys, catalog);
    let mut workspaces = workspace_labels(keys, names);
    keys.iter()
        .map(|key| match key {
            UsageMetricsKey::Model { .. } => models.remove(key).unwrap_or_default(),
            UsageMetricsKey::Provider { provider } => provider_label(catalog, provider),
            UsageMetricsKey::Workspace { workspace } => {
                workspaces.remove(workspace).unwrap_or_default()
            }
            UsageMetricsKey::Unknown => "Unknown".to_owned(),
        })
        .collect()
}

/// A Provider's display name in the Model Catalog, otherwise its id: how
/// the Provider breakdown names it, and how a Model's label names its
/// Provider.
fn provider_label(catalog: &ModelCatalog, provider: &ProviderId) -> String {
    catalog
        .providers
        .iter()
        .find(|listed| listed.provider == *provider)
        .map_or_else(
            || provider.as_str().to_owned(),
            |listed| listed.display_name.clone(),
        )
}

/// A Model a report names, and how its label reads.
struct NamedModel<'a> {
    key: &'a UsageMetricsKey,
    provider: &'a ProviderId,
    model: &'a ModelId,
    /// Its name in the Model Catalog, otherwise its id.
    label: String,
    /// The ways its label reads: its letters and digits, lowercased, and
    /// those again with any of the report's Providers' names taken off the
    /// front where more follows.
    readings: HashSet<String>,
}

impl NamedModel<'_> {
    /// Whether it and `other` would read alike: whether any way one reads
    /// is a way the other does. Each Model is read against every Provider
    /// in the report, not its own alone, so the likeness runs both ways.
    fn reads_like(&self, other: &Self) -> bool {
        !self.readings.is_disjoint(&other.readings)
    }
}

/// The label of each Model among `keys`: its name in the Model Catalog
/// where it carries one, otherwise its id. Two read alike where their
/// letters and digits match, whatever their case, either as written or with
/// a Provider's name taken off the front — `claude-opus-5-5` reads as
/// `Opus 5.5` does. A Model that reads like one of another Provider is named
/// after its own Provider, as the Provider breakdown names it, as
/// `Copilot · GPT-5.5`; one that reads like another of its own Provider has
/// its id after it, as `Opus 5.5 (opus)`, unless its label is its id, which
/// already tells it from the other's.
fn model_labels(
    keys: &[UsageMetricsKey],
    catalog: &ModelCatalog,
) -> HashMap<UsageMetricsKey, String> {
    let letters = |text: &str| -> String {
        text.chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let mut prefixes: Vec<String> = keys
        .iter()
        .filter_map(|key| match key {
            UsageMetricsKey::Model { provider, .. } => Some(provider),
            _ => None,
        })
        .flat_map(|provider| {
            [
                provider.as_str().to_owned(),
                provider_label(catalog, provider),
            ]
        })
        .map(|name| letters(&name))
        .filter(|name| !name.is_empty())
        .collect();
    prefixes.sort();
    prefixes.dedup();
    let named: Vec<NamedModel<'_>> = keys
        .iter()
        .filter_map(|key| match key {
            UsageMetricsKey::Model { provider, model } => {
                let label = catalog.model(provider, model).map_or_else(
                    || model.as_str().to_owned(),
                    |descriptor| descriptor.display_name.clone(),
                );
                let written = letters(&label);
                let readings = prefixes
                    .iter()
                    .filter_map(|prefix| written.strip_prefix(prefix.as_str()))
                    .filter(|rest| !rest.is_empty())
                    .map(str::to_owned)
                    .chain([written.clone()])
                    .collect();
                Some(NamedModel {
                    key,
                    provider,
                    model,
                    label,
                    readings,
                })
            }
            _ => None,
        })
        .collect();
    named
        .iter()
        .map(|this| {
            let alike = || {
                named
                    .iter()
                    .filter(|other| !std::ptr::eq(*other, this) && this.reads_like(other))
            };
            let mut label = this.label.clone();
            if alike().any(|other| other.provider == this.provider) && label != this.model.as_str()
            {
                label = format!("{label} ({})", this.model.as_str());
            }
            if alike().any(|other| other.provider != this.provider) {
                label = format!("{} · {label}", provider_label(catalog, this.provider));
            }
            (this.key.clone(), label)
        })
        .collect()
}

/// Relabels, by `refine`, every Workspace in `labels` whose label another
/// shares.
fn tell_apart(
    labels: &mut HashMap<WorkspaceId, String>,
    refine: impl Fn(&WorkspaceId, &str) -> String,
) {
    let mut counts = HashMap::<String, usize>::new();
    for label in labels.values() {
        *counts.entry(label.clone()).or_default() += 1;
    }
    for (id, label) in labels.iter_mut() {
        if counts[label.as_str()] > 1 {
            *label = refine(id, label);
        }
    }
}

/// The label of each Workspace among `keys`, no two alike: its name, as
/// wherever Workspaces are listed; where two go by one name, where each is
/// presented; and where two are presented at one place — a directory that
/// became a Repository's while Sessions belonged to each, say — that and
/// the start of its identity besides.
fn workspace_labels(keys: &[UsageMetricsKey], names: &KeyNames) -> HashMap<WorkspaceId, String> {
    let paths = names.workspace_paths;
    let mut labels: HashMap<WorkspaceId, String> = keys
        .iter()
        .filter_map(|key| match key {
            UsageMetricsKey::Workspace { workspace } => Some(workspace),
            _ => None,
        })
        .map(|id| {
            let name = names
                .workspaces
                .get(id)
                .map_or_else(|| id.0.clone(), |workspace| paths.workspace_name(workspace));
            (id.clone(), name)
        })
        .collect();
    tell_apart(&mut labels, |id, label| {
        names.workspaces.get(id).map_or_else(
            || label.to_owned(),
            |workspace| paths.label(&workspace.path),
        )
    });
    tell_apart(&mut labels, |id, label| {
        // The identity's kind and the first eight of its hash.
        let shown = id.0.find(':').map_or(8, |colon| colon + 9);
        format!("{label} ({})", id.0.get(..shown).unwrap_or(&id.0))
    });
    labels
}
