//! What of each Session the writer holds that storage does not hold yet.
//!
//! A Session's history only grows: a Prompt, Turn, Message, or Activity is
//! added once and moved in place after, and none is ever removed or
//! reordered. A row is therefore named for the Session's whole life by its
//! position, which is also its `row_order`, and what storage lacks of a
//! Session comes in two kinds: the rows past those it holds, which a save
//! inserts, and the rows it holds whose content has moved since, which a save
//! updates in place. A save writes those and the Session's own row, never the
//! rest of its history, so what a save costs follows what moved rather than
//! how long the Session has run.

use std::collections::BTreeSet;

use crate::protocol::{SessionChange, SessionSnapshot};

/// How many of each kind of row a Session holds, and how long its Transcript
/// is.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RowCounts {
    pub(super) prompts: usize,
    pub(super) turns: usize,
    pub(super) messages: usize,
    pub(super) activities: usize,
    pub(super) transcript: usize,
}

impl RowCounts {
    fn of(snapshot: &SessionSnapshot) -> Self {
        Self {
            prompts: snapshot.prompts.len(),
            turns: snapshot.turns.len(),
            messages: snapshot.messages.len(),
            activities: snapshot.activities.len(),
            transcript: snapshot.transcript.len(),
        }
    }
}

/// What the next save of one Session must write for storage to hold it as
/// the writer does.
#[derive(Debug)]
pub(super) struct UnsavedRows {
    /// Whether the save writes the Session whole, replacing whatever storage
    /// holds of it: a Session storage has never held, or one whose stored
    /// shape the writer cannot vouch for.
    whole: bool,
    /// How many of each row storage holds. A row at or past these is new to
    /// storage, and the save inserts it.
    stored: RowCounts,
    /// The positions of rows storage holds whose content has moved since it
    /// took them, which the save updates in place.
    pub(super) prompts: BTreeSet<usize>,
    pub(super) turns: BTreeSet<usize>,
    pub(super) messages: BTreeSet<usize>,
    pub(super) activities: BTreeSet<usize>,
}

impl UnsavedRows {
    /// A Session storage holds exactly as `snapshot` stands.
    pub(super) fn stored(snapshot: &SessionSnapshot) -> Self {
        Self {
            whole: false,
            stored: RowCounts::of(snapshot),
            prompts: BTreeSet::new(),
            turns: BTreeSet::new(),
            messages: BTreeSet::new(),
            activities: BTreeSet::new(),
        }
    }

    /// A Session whose next save writes it whole.
    pub(super) fn whole() -> Self {
        Self {
            whole: true,
            stored: RowCounts::default(),
            prompts: BTreeSet::new(),
            turns: BTreeSet::new(),
            messages: BTreeSet::new(),
            activities: BTreeSet::new(),
        }
    }

    pub(super) fn is_whole(&self) -> bool {
        self.whole
    }

    /// How many of each row storage holds; nothing, where the save writes
    /// the Session whole.
    pub(super) fn stored_counts(&self) -> RowCounts {
        if self.whole {
            RowCounts::default()
        } else {
            self.stored
        }
    }

    /// Notes the Activities at `positions`, which storage holds but which a
    /// reading of the Session moved without any change saying so.
    pub(super) fn moved_activities(&mut self, positions: impl IntoIterator<Item = usize>) {
        if self.whole {
            return;
        }
        self.activities.extend(
            positions
                .into_iter()
                .filter(|&position| position < self.stored.activities),
        );
    }

    /// Notes the rows `changes` moved, given where each landed as
    /// [`crate::session_projection::land_update`] answers. A row a change
    /// added needs no noting: it lies past those storage holds.
    pub(super) fn note(&mut self, changes: &[SessionChange], landed: &[Option<usize>]) {
        if self.whole {
            return;
        }
        debug_assert_eq!(changes.len(), landed.len());
        for (change, &position) in changes.iter().zip(landed) {
            let (Some(position), Some(table)) = (position, moved_row(change)) else {
                continue;
            };
            let (moved, stored) = match table {
                Table::Prompts => (&mut self.prompts, self.stored.prompts),
                Table::Turns => (&mut self.turns, self.stored.turns),
                Table::Messages => (&mut self.messages, self.stored.messages),
                Table::Activities => (&mut self.activities, self.stored.activities),
            };
            if position < stored {
                moved.insert(position);
            }
        }
    }

    /// Notes that storage now holds the Session exactly as `snapshot` stands.
    pub(super) fn saved(&mut self, snapshot: &SessionSnapshot) {
        *self = Self::stored(snapshot);
    }
}

/// A table of a Session's history.
enum Table {
    Prompts,
    Turns,
    Messages,
    Activities,
}

/// The table holding the row `change` moves in place, where it moves one.
/// Every kind is named, so a new one has to say what it moves.
fn moved_row(change: &SessionChange) -> Option<Table> {
    match change {
        SessionChange::PromptDeliveryChanged { .. }
        | SessionChange::PromptStatusChanged { .. }
        | SessionChange::PromptWithdrawn { .. }
        | SessionChange::PromptTaken { .. } => Some(Table::Prompts),
        SessionChange::TurnAgentChanged { .. }
        | SessionChange::SubagentAgentChanged { .. }
        | SessionChange::TurnUsageChanged { .. }
        | SessionChange::TurnOutputObserved { .. }
        | SessionChange::TurnStatusChanged { .. } => Some(Table::Turns),
        SessionChange::MessageContentAppended { .. }
        | SessionChange::MessageTruncated { .. }
        | SessionChange::MessageCompleted { .. } => Some(Table::Messages),
        SessionChange::QuestionnaireAccepted { .. }
        | SessionChange::QuestionnaireSettled { .. }
        | SessionChange::DecisionAccepted { .. }
        | SessionChange::ApprovalSettled { .. }
        | SessionChange::ApprovalFollowUpFailed { .. }
        | SessionChange::CommandOutputAppended { .. }
        | SessionChange::CommandOutputTruncated { .. }
        | SessionChange::CommandStatusChanged { .. }
        | SessionChange::FileChangeUpdated { .. }
        | SessionChange::FileChangeStatusChanged { .. }
        | SessionChange::ToolCallInputChanged { .. }
        | SessionChange::ToolCallOutputAppended { .. }
        | SessionChange::ToolCallOutputTruncated { .. }
        | SessionChange::ToolCallStatusChanged { .. }
        | SessionChange::ReasoningTitleChanged { .. }
        | SessionChange::ReasoningContentAppended { .. }
        | SessionChange::ReasoningContentTruncated { .. }
        | SessionChange::ReasoningStatusChanged { .. }
        | SessionChange::SubagentDescriptionChanged { .. }
        | SessionChange::SubagentModelChanged { .. }
        | SessionChange::SubagentStatusChanged { .. }
        | SessionChange::CompactionSettled { .. }
        | SessionChange::CompactionAfterMeasured { .. }
        | SessionChange::SubsessionTitleChanged { .. } => Some(Table::Activities),
        // A row added lies past those storage holds; everything else moves
        // the Session's own row, which every save writes, or nothing stored.
        SessionChange::PromptAdded { .. }
        | SessionChange::TurnAdded { .. }
        | SessionChange::MessageAdded { .. }
        | SessionChange::ActivityAdded { .. }
        | SessionChange::WorkspaceChanged { .. }
        | SessionChange::TitleChanged { .. }
        | SessionChange::ContextFillChanged { .. }
        | SessionChange::AgentSelectionChanged { .. }
        | SessionChange::AgentSelectionAvailabilityChanged { .. }
        | SessionChange::ApprovalPostureChanged { .. }
        | SessionChange::AttachmentsDescribed { .. }
        | SessionChange::SubagentInterventionsChanged { .. }
        | SessionChange::SubagentUsageChanged { .. }
        | SessionChange::TotalCostChanged { .. }
        | SessionChange::SessionWorkingChanged { .. }
        | SessionChange::SessionMonitoringChanged { .. }
        | SessionChange::SessionWatchesChanged { .. }
        | SessionChange::SessionWaitingOnSubagentsChanged { .. }
        | SessionChange::SessionStatusChanged { .. } => None,
    }
}
