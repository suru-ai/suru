//! Insights: how the Sessions living on this Server worked over one span of
//! time, beside how much they consumed. The read counts the Sessions the
//! metrics read counts, copied under the store lock as that read copies them
//! and added up outside it, and places each Turn by the same rule; a Turn
//! with no recorded moment to place it by is left out.
//!
//! Each panel of the Insights tab reads one part of the report, which a
//! [`InsightsFold`] adds up a part apiece from the Turns the span holds.
//!
//! The Tools those Turns used, their Compactions, and the Approvals they
//! asked are Activities, which no Session holds in memory until its history
//! is read (ADR 0022). Rather than read any in, the read asks storage, in
//! one pass, for the few fields of each Activity it counts, and keeps those
//! whose Turn the span holds (ADR 0056).
//!
//! The quarter-hours carry Cost as the metrics read's buckets do: every
//! Cost Coverage contribution of the counted trees lands where the Turn
//! carrying it does, so the quarter-hours of a span add up to the Cost the
//! metrics read gives it. Delegation divides those same contributions by
//! the role of the Session whose Turn carries each, so its roles add up to
//! that Cost too.
//!
//! What caching saved is no Cost and lands in none: it is worked out at
//! every read from the rates each Turn's Provider prices its Model at today,
//! asked once the lock is let go, and a Model lacking the rate of a part of
//! the cache its Turns used is left out and the estimate marked partial.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::protocol::{
    CacheInsights, CountedCost, DelegationInsights, EstimatedSaving, InsightsQuery, InsightsReport,
    ModelId, ProviderId, QUARTER_HOUR_MS, QuarterHourTurns, SessionId, TURN_DURATION_BIN_ENDS_MS,
    TURN_DURATION_BINS, ToolTally, Turn, TurnDurations, TurnId, TurnInsights, TurnStatus, Usage,
    UsageMetricsAggregate, WorkspaceId, signed_nano_usd,
};

use crate::{pricing::CacheRateLookup, sidekick::SidekickWorkspace};

mod activities;

use activities::{ActivityTallies, ActivityTally};

use super::{
    super::{
        SessionStore,
        cost_coverage::{CostContribution, tree_contributions},
    },
    CopiedSession, CountedSession, MeteredForest, SessionTimestamp, TurnPlaces, placed_at,
};

/// Why an Insights query is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InsightsRefusal {
    /// Its interval ends before it begins.
    OutOfOrder,
    /// Storage could not be read for the Activities of the interval's Turns.
    Storage,
}

impl SessionStore {
    /// How the Sessions living here worked over `query`'s interval. A pure
    /// read, as the metrics read is: it moves no Session's revision, writes
    /// nothing, and reads no history in. The Tools, Compactions, and
    /// Approvals of the interval's Turns are read from the Activities
    /// storage holds, on a blocking task once
    /// the lock is let go (ADR 0056), and placed in time by the Turns that
    /// own them, which the copy taken under the lock holds.
    pub(crate) async fn insights(
        &self,
        query: &InsightsQuery,
        rates: &dyn CacheRateLookup,
    ) -> Result<InsightsReport, InsightsRefusal> {
        let interval = query.interval;
        if interval.end < interval.start {
            return Err(InsightsRefusal::OutOfOrder);
        }
        let (copied, repository, sidekick_workspace) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let repository = state
                .deferred
                .as_ref()
                .map(|deferred| deferred.repository.clone());
            (
                state.copied_sessions(),
                repository,
                state.sidekick_workspace.clone(),
            )
        };
        // Read once the lock is let go, since naming the Sidekick
        // Workspace reads the filesystem.
        let sidekick_workspace = sidekick_workspace.as_ref().map(SidekickWorkspace::id);
        let forest = MeteredForest::index(copied);
        let roots = forest.cost_tree_heads();
        let mut fold = InsightsFold::default();
        for CountedSession { session_id, .. } in forest.counted_sessions(&roots) {
            let session = &forest.sessions[&session_id];
            let role = SessionRole::of(session, sidekick_workspace.as_ref());
            for turn in &session.turns {
                if let Some(at) = placed_at(turn).filter(|at| interval.contains(*at)) {
                    fold.add_turn(session_id, role, turn, at);
                }
            }
        }
        for root in roots {
            for contribution in tree_contributions(&forest, root) {
                fold.add_cost(&contribution);
            }
        }
        // Only a store restored without storage, as some tests restore one,
        // lacks a repository; its Activities are in memory, and are not
        // counted.
        if let Some(repository) = repository
            && !fold.held_turns.is_empty()
        {
            let held = std::mem::take(&mut fold.held_turns);
            fold.activities = repository
                .activity_rows(held, ActivityTally::default())
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "reading the Activities Insights counts failed");
                    InsightsRefusal::Storage
                })?
                .into_tallies();
        }
        // Priced once the lock is let go, since a lookup may wait on a
        // fetch of the rate table.
        let cache = std::mem::take(&mut fold.cache).priced(rates).await;
        Ok(fold.into_report(cache))
    }
}

/// The report as it is added up, a part for each of its own.
#[derive(Default)]
struct InsightsFold {
    turns: TurnTally,
    quarter_hours: QuarterHourTally,
    delegation: DelegationTally,
    cache: CacheTally,
    /// The Turns the interval holds of each Session, whose Activities it
    /// counts.
    held_turns: HashMap<SessionId, HashSet<TurnId>>,
    /// What those Activities add up to, its Tools in no order yet.
    activities: ActivityTallies,
}

impl InsightsFold {
    /// Adds a Turn of `session_id`, a Session playing `role`, the interval
    /// holds, placed `at`.
    fn add_turn(
        &mut self,
        session_id: SessionId,
        role: SessionRole,
        turn: &Turn,
        at: SessionTimestamp,
    ) {
        self.turns.add(turn);
        self.quarter_hours.add_turn(session_id, turn, at);
        self.delegation.add_turn(session_id, role, turn);
        self.cache.add(turn);
        self.held_turns
            .entry(session_id)
            .or_default()
            .insert(turn.id);
    }

    /// Adds a Cost contribution of the counted trees, where the interval
    /// holds the Turn carrying it.
    fn add_cost(&mut self, contribution: &CostContribution) {
        self.quarter_hours.add_cost(contribution);
        self.delegation.add_cost(contribution);
    }

    fn into_report(self, cache: CacheInsights) -> InsightsReport {
        InsightsReport {
            turns: self.turns.into_insights(),
            quarter_hours: self.quarter_hours.into_quarter_hours(),
            tools: most_used_first(self.activities.tools),
            compactions: self.activities.compactions,
            approvals: self.activities.approvals,
            delegation: self.delegation.insights,
            cache,
        }
    }
}

/// `tools` the most used first, those used as often the most failed first,
/// and those alike in both as their Tools are ordered, so the order is the
/// same at every read.
fn most_used_first(mut tools: Vec<ToolTally>) -> Vec<ToolTally> {
    tools.sort_unstable_by(|left, right| {
        (right.uses, right.failed)
            .cmp(&(left.uses, left.failed))
            .then_with(|| left.tool.cmp(&right.tool))
    });
    tools
}

/// The interval's Turns by the UTC quarter-hour each is placed in, and
/// which quarter-hour holds each Turn, for the Cost it carries to land
/// beside it.
#[derive(Default)]
struct QuarterHourTally {
    /// Each quarter-hour a Turn is placed in, by its place since the epoch.
    quarters: BTreeMap<u64, QuarterHourTurns>,
    placed: TurnPlaces<u64>,
}

impl QuarterHourTally {
    fn add_turn(&mut self, session_id: SessionId, turn: &Turn, at: SessionTimestamp) {
        let place = at.0 / QUARTER_HOUR_MS;
        let quarter = self.quarters.entry(place).or_insert(QuarterHourTurns {
            start: SessionTimestamp(place * QUARTER_HOUR_MS),
            turns: 0,
            tokens: None,
            cost: CountedCost::default(),
        });
        quarter.turns += 1;
        if let Some(tokens) = turn.usage.as_ref().and_then(Usage::blended_tokens) {
            quarter.tokens = Some(quarter.tokens.unwrap_or(0).saturating_add(tokens));
        }
        self.placed.place(session_id, turn.id, place);
    }

    fn add_cost(&mut self, contribution: &CostContribution) {
        let Some(quarter) = self
            .placed
            .of(contribution)
            .and_then(|place| self.quarters.get_mut(place))
        else {
            return;
        };
        quarter.cost.add(contribution.counted());
    }

    fn into_quarter_hours(self) -> Vec<QuarterHourTurns> {
        self.quarters.into_values().collect()
    }
}

/// The role a Session plays in its forest, by which Delegation divides the
/// Cost its Turns carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRole {
    Own,
    NativeSubagent,
    BrokeredSubagent,
    Sidekick,
    Subsession,
}

impl SessionRole {
    /// The role `session` plays, where `sidekick_workspace` names the
    /// Sidekick Workspace, if the store knows it yet. A Subagent is one
    /// whoever spawned it, a Sidekick's or a Subsession's included; a
    /// Sidekick is a top-level Session of the Sidekick Workspace, and a
    /// Subsession one a Sidekick began, never in that Workspace.
    fn of(session: &CopiedSession, sidekick_workspace: Option<&WorkspaceId>) -> Self {
        if session.parent.is_some() {
            if session.is_brokered_subagent {
                Self::BrokeredSubagent
            } else {
                Self::NativeSubagent
            }
        } else if sidekick_workspace == Some(&session.workspace) {
            Self::Sidekick
        } else if session.is_subsession() {
            Self::Subsession
        } else {
            Self::Own
        }
    }
}

/// The interval's Cost by the role of the Session whose Turn carries each
/// contribution, and which role each Turn the interval holds is of.
#[derive(Default)]
struct DelegationTally {
    roles: TurnPlaces<SessionRole>,
    insights: DelegationInsights,
}

impl DelegationTally {
    /// Records the role of the Session owning a Turn the interval holds,
    /// for the Cost that Turn carries to land under it.
    fn add_turn(&mut self, session_id: SessionId, role: SessionRole, turn: &Turn) {
        self.roles.place(session_id, turn.id, role);
    }

    fn add_cost(&mut self, contribution: &CostContribution) {
        let Some(role) = self.roles.of(contribution) else {
            return;
        };
        let insights = &mut self.insights;
        let cost = match role {
            SessionRole::Own => &mut insights.own_sessions,
            SessionRole::NativeSubagent => &mut insights.native_subagents,
            SessionRole::BrokeredSubagent => &mut insights.brokered_subagents,
            SessionRole::Sidekick => &mut insights.sidekicks,
            SessionRole::Subsession => &mut insights.subsessions,
        };
        cost.add(contribution.counted());
    }
}

/// How the interval's Turns ended, and how long each Settled one took.
#[derive(Default)]
struct TurnTally {
    durations: Vec<u64>,
    completed: u64,
    failed: u64,
    interrupted: u64,
}

impl TurnTally {
    fn add(&mut self, turn: &Turn) {
        let count = match turn.status {
            TurnStatus::Active => return,
            TurnStatus::Completed => &mut self.completed,
            TurnStatus::Failed => &mut self.failed,
            TurnStatus::Interrupted => &mut self.interrupted,
        };
        *count += 1;
        // A Settle stamped before its start gives no duration, as the
        // Transcript's Turn Fold states none for it.
        if let Some(duration) = turn
            .started_at
            .zip(turn.settled_at)
            .and_then(|(started_at, settled_at)| duration_ms(started_at, settled_at))
        {
            self.durations.push(duration);
        }
    }

    fn into_insights(mut self) -> TurnInsights {
        self.durations.sort_unstable();
        TurnInsights {
            durations: durations_of(&self.durations),
            completed: self.completed,
            failed: self.failed,
            interrupted: self.interrupted,
        }
    }
}

fn duration_ms(started_at: SessionTimestamp, settled_at: SessionTimestamp) -> Option<u64> {
    settled_at.0.checked_sub(started_at.0)
}

/// The figures of `sorted`, shortest first, none where it is empty.
fn durations_of(sorted: &[u64]) -> Option<TurnDurations> {
    let longest_ms = *sorted.last()?;
    // The nearest rank: the `numerator`/`denominator` share of the
    // durations, rounded up, is at least as long as the one at that rank.
    let rank = |numerator: usize, denominator: usize| {
        sorted[(numerator * sorted.len()).div_ceil(denominator) - 1]
    };
    let mut bins = [0; TURN_DURATION_BINS];
    for duration in sorted {
        bins[TURN_DURATION_BIN_ENDS_MS.partition_point(|end| end <= duration)] += 1;
    }
    Some(TurnDurations {
        median_ms: rank(1, 2),
        p90_ms: rank(9, 10),
        longest_ms,
        bins,
    })
}

/// The interval's input, part by part, and how many tokens the Turns of
/// each Model read from cache and wrote to it, for a saving to be worked
/// out from that Model's rates once the lock is let go.
#[derive(Default)]
struct CacheTally {
    input: UsageMetricsAggregate,
    /// What the Turns of each Model read from cache and wrote to it. A
    /// saving is linear in both, so a Model's Turns are priced together as
    /// they would be one by one.
    by_model: HashMap<(ProviderId, ModelId), CacheUse>,
    /// Whether a Turn that read or wrote the cache ran on no Model the
    /// store knows, so no rates can price it.
    is_unattributed: bool,
}

/// How many tokens some Turns read from cache and wrote to it.
#[derive(Clone, Copy, Default)]
struct CacheUse {
    reads: u64,
    writes: u64,
}

impl CacheTally {
    fn add(&mut self, turn: &Turn) {
        let Some(usage) = &turn.usage else {
            return;
        };
        self.input.add_usage(usage);
        let reads = usage.cache_read_tokens.unwrap_or(0);
        let writes = usage.cache_write_tokens.unwrap_or(0);
        if reads == 0 && writes == 0 {
            return;
        }
        let Some(agent) = &turn.agent else {
            self.is_unattributed = true;
            return;
        };
        let selection = &agent.selection;
        let used = self
            .by_model
            .entry((selection.provider.clone(), selection.model.clone()))
            .or_default();
        used.reads = used.reads.saturating_add(reads);
        used.writes = used.writes.saturating_add(writes);
    }

    /// The figures, the saving worked out from the rates `rates` states
    /// today for each Model, leaving out a Model lacking a rate for a part
    /// of the cache its Turns used, or whose rates price no finite saving.
    async fn priced(self, rates: &dyn CacheRateLookup) -> CacheInsights {
        let mut saving = EstimatedSaving {
            nano_usd: None,
            is_partial: self.is_unattributed,
        };
        for ((provider, model), used) in self.by_model {
            let Some(nano_usd) = rates
                .cache_rates(&provider, &model)
                .await
                .and_then(|rates| rates.saving_usd(used.reads, used.writes))
                .and_then(signed_nano_usd)
            else {
                saving.is_partial = true;
                continue;
            };
            saving.nano_usd = Some(saving.nano_usd.unwrap_or(0).saturating_add(nano_usd));
        }
        CacheInsights {
            fresh_input_tokens: self.input.fresh_input_tokens,
            cache_read_tokens: self.input.cache_read_tokens,
            cache_write_tokens: self.input.cache_write_tokens,
            saving,
        }
    }
}
