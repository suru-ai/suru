//! What the Activities of the interval's Turns add up to: how often each
//! Tool was used and how many of those uses failed, the Compactions, and
//! the Approvals asked. Storage streams the rows (ADR 0056); what each
//! counts toward is decided here.

use std::collections::HashMap;

use crate::{
    protocol::{
        ActivityStatus, ApprovalInsights, ApprovalOutcome, CompactionInsights, CompactionTrigger,
        Decision, FreedShare, InsightsTool, ToolTally,
    },
    storage::{ActivityRowSink, ActivityTallyRow, Spelled},
};

/// What the Activities of a span's Turns add up to.
#[derive(Debug, Default)]
pub(super) struct ActivityTallies {
    /// How often each Tool was used, in no order.
    pub(super) tools: Vec<ToolTally>,
    pub(super) compactions: CompactionInsights,
    pub(super) approvals: ApprovalInsights,
}

/// The tallies as the rows stream in.
#[derive(Default)]
pub(super) struct ActivityTally {
    tools: HashMap<InsightsTool, (u64, u64)>,
    compactions: CompactionInsights,
    freed: FreedShares,
    approvals: ApprovalInsights,
}

/// The freed shares of the Compactions that can say, added up, and how
/// many there are.
#[derive(Default)]
struct FreedShares {
    shares: f64,
    compactions: u64,
}

impl ActivityRowSink for ActivityTally {
    fn add(&mut self, row: ActivityTallyRow) {
        match row {
            ActivityTallyRow::Compaction {
                trigger,
                status,
                before_tokens,
                after_tokens,
            } => self.add_compaction(trigger, status, before_tokens.zip(after_tokens)),
            ActivityTallyRow::Approval { outcome, decision } => {
                self.add_approval(outcome, decision);
            }
            ActivityTallyRow::Command { status } => self.add_use(InsightsTool::Command, status),
            ActivityTallyRow::FileChange { status } => {
                self.add_use(InsightsTool::FileChange, status);
            }
            // Each delegation, a spawn or a resume, stands as a Subagent
            // row of its own.
            ActivityTallyRow::Subagent { status } => self.add_use(InsightsTool::Subagent, status),
            // A Tool Call whose name is missing names no Tool.
            ActivityTallyRow::ToolCall {
                name: Some(name),
                server,
                status,
            } => self.add_use(InsightsTool::Named { name, server }, status),
            ActivityTallyRow::ToolCall { name: None, .. } => {}
        }
    }
}

impl ActivityTally {
    /// Counts a use of `tool`, failed where its status says so.
    fn add_use(&mut self, tool: InsightsTool, status: Spelled<ActivityStatus>) {
        let (uses, failed) = self.tools.entry(tool).or_default();
        *uses += 1;
        *failed += u64::from(status == Spelled::Known(ActivityStatus::Failed));
    }

    /// Counts a Compaction by its trigger and status, and where it
    /// completed knowing both figures with some context before, the share
    /// of that context it freed. One whose trigger or status this Suru
    /// cannot read is left out.
    fn add_compaction(
        &mut self,
        trigger: Spelled<CompactionTrigger>,
        status: Spelled<ActivityStatus>,
        fill: Option<(i64, i64)>,
    ) {
        let (Spelled::Known(trigger), Spelled::Known(status)) = (&trigger, &status) else {
            tracing::warn!(
                ?trigger,
                ?status,
                "Insights leave out a Compaction they cannot read"
            );
            return;
        };
        let counts = match trigger {
            CompactionTrigger::Automatic => &mut self.compactions.automatic,
            CompactionTrigger::Manual => &mut self.compactions.manual,
        };
        let count = match status {
            ActivityStatus::Active => &mut counts.active,
            ActivityStatus::Completed => &mut counts.completed,
            ActivityStatus::Failed => &mut counts.failed,
            ActivityStatus::Interrupted => &mut counts.interrupted,
        };
        *count += 1;
        if *status == ActivityStatus::Completed
            && let Some((before, after)) = fill
            && before > 0
        {
            self.freed.shares += (before - after) as f64 / before as f64;
            self.freed.compactions += 1;
        }
    }

    /// Counts an Approval asked, and how it stands by the Decision
    /// recorded for it. One whose outcome or Decision this Suru cannot read
    /// is left out, so every Approval asked stands in one other figure.
    fn add_approval(&mut self, outcome: Spelled<ApprovalOutcome>, decision: Spelled<Decision>) {
        // Absent, or a Decision this Suru reads.
        let read_decision = match &decision {
            Spelled::Absent => Some(None),
            Spelled::Known(decision) => Some(Some(*decision)),
            Spelled::Unknown(_) => None,
        };
        let (Spelled::Known(outcome), Some(decision)) = (&outcome, read_decision) else {
            tracing::warn!(
                ?outcome,
                ?decision,
                "Insights leave out an Approval they cannot read"
            );
            return;
        };
        let approvals = &mut self.approvals;
        approvals.asked += 1;
        let count = match (outcome, decision) {
            (ApprovalOutcome::Decided, Some(Decision::Accept)) => &mut approvals.accepted,
            (ApprovalOutcome::Decided, Some(Decision::AcceptForSession)) => {
                &mut approvals.accepted_for_session
            }
            (ApprovalOutcome::Decided, Some(Decision::Decline)) => &mut approvals.declined,
            (ApprovalOutcome::Decided, Some(Decision::DeclineAndInterrupt)) => {
                &mut approvals.declined_and_interrupted
            }
            (
                ApprovalOutcome::Pending
                | ApprovalOutcome::Submitting
                | ApprovalOutcome::SubmissionRejected,
                _,
            ) => &mut approvals.waiting,
            // Settled with no Decision recorded; a Decided one never lacks
            // its Decision, which the Session's projection refuses.
            (
                ApprovalOutcome::Decided
                | ApprovalOutcome::Withdrawn
                | ApprovalOutcome::TurnEnded
                | ApprovalOutcome::Unavailable
                | ApprovalOutcome::DeliveryUncertain,
                _,
            ) => &mut approvals.expired,
        };
        *count += 1;
    }

    pub(super) fn into_tallies(self) -> ActivityTallies {
        let FreedShares {
            shares,
            compactions,
        } = self.freed;
        let freed = (compactions > 0).then(|| FreedShare {
            compactions,
            average_millionths: (shares / compactions as f64 * 1_000_000.0).round() as i64,
        });
        ActivityTallies {
            tools: self
                .tools
                .into_iter()
                .map(|(tool, (uses, failed))| ToolTally { tool, uses, failed })
                .collect(),
            compactions: CompactionInsights {
                freed,
                ..self.compactions
            },
            approvals: self.approvals,
        }
    }
}
