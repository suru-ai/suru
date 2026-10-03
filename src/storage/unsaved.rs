//! What of each Session storage does not hold yet, kept beside the Session
//! by the store that holds its history rather than in a copy of its own.
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
//!
//! Noting what moved is all a commit does here: nothing is encoded until a
//! save is taken, which reads the Session where it is held, encodes only what
//! storage lacks into rows of its own, and hands those to the writer. A
//! Message streaming in is therefore encoded once per save, never once per
//! chunk, and no second copy of the history is kept for the writer to encode
//! from.

use std::{collections::BTreeSet, time::Instant};

use crate::protocol::{SessionChange, SessionId, SessionSnapshot, SessionSummary};

use super::{
    SessionSave, StorageError, StoredSidekickAct, StoredSubagentIdentity, rows::StoredRows,
};

/// One Session as a save reads it where its history is held, borrowed for
/// as long as the save takes to encode what storage lacks of it.
pub(crate) struct SessionRows<'a> {
    pub(crate) summary: &'a SessionSummary,
    pub(crate) snapshot: &'a SessionSnapshot,
    /// The Provider's own identity for the Subagent the Session is, on a
    /// native Subagent's child Session alone.
    pub(crate) subagent_identity: Option<&'a StoredSubagentIdentity>,
    /// Whether the Session is a brokered Subagent's (ADR 0035).
    pub(crate) brokered: bool,
}

/// What one Session owes storage: whether it owes a save at all, which of its
/// rows that save writes, and the acts of Sidekicks on it that land in the
/// same transaction. Kept beside the Session by whoever holds its history,
/// which is the one copy a save is encoded from.
#[derive(Debug)]
pub(crate) struct Unsaved {
    /// Whether the Session's history is held beside this at all. One whose
    /// history waits in storage to be read has nothing a save could be
    /// encoded from, so it owes nothing, whatever moves about it.
    held: bool,
    /// Whether the Session owes storage a save.
    owed: bool,
    /// What that save writes. Rows a reading of the Session moved may wait
    /// here while it owes nothing, until something else it owes lands them.
    rows: UnsavedRows,
    /// The acts of Sidekicks on this Session that the changes not yet saved
    /// follow, which land in the same transaction as those changes.
    acts: Vec<StoredSidekickAct>,
    /// When the Session last came to owe more: a save waits for a quiet
    /// moment, so a Message streaming in is not encoded at every chunk.
    moved_at: Option<Instant>,
}

impl Unsaved {
    /// A Session whose history is not held, which owes storage nothing.
    pub(crate) fn unheld() -> Self {
        Self {
            held: false,
            owed: false,
            rows: UnsavedRows::whole(),
            acts: Vec::new(),
            moved_at: None,
        }
    }

    /// A Session just created, which storage has never held, and `acts`, the
    /// acts of Sidekicks its creation follows: its first save writes it
    /// whole, with them.
    pub(crate) fn created(acts: Vec<StoredSidekickAct>) -> Self {
        Self {
            held: true,
            owed: true,
            rows: UnsavedRows::whole(),
            acts,
            moved_at: Some(Instant::now()),
        }
    }

    /// A Session handed over whole at the start, as a test fixture is, whose
    /// shape in storage is not this process's to know: it owes nothing until
    /// it moves, and its first save then writes it whole.
    pub(crate) fn handed_over() -> Self {
        Self {
            held: true,
            owed: false,
            rows: UnsavedRows::whole(),
            acts: Vec::new(),
            moved_at: None,
        }
    }

    /// A Session just read back from storage as `snapshot` stands, save for
    /// the Activities at `recovered_activities`, which the reading moved
    /// without a change saying so: those land with whatever the Session next
    /// owes storage, and never on their own (ADR 0022).
    pub(crate) fn hydrated(
        snapshot: &SessionSnapshot,
        recovered_activities: impl IntoIterator<Item = usize>,
    ) -> Self {
        let mut rows = UnsavedRows::stored(snapshot);
        rows.moved_activities(recovered_activities);
        Self {
            held: true,
            owed: false,
            rows,
            acts: Vec::new(),
            moved_at: None,
        }
    }

    /// Whether the Session owes storage a save.
    pub(crate) fn owes_save(&self) -> bool {
        self.owed
    }

    /// When the Session last came to owe more, while it owes a save.
    pub(crate) fn moved_at(&self) -> Option<Instant> {
        self.moved_at.filter(|_| self.owed)
    }

    /// The Sidekicks' Sessions the acts waiting here name, each of which
    /// lands before the act naming it where both are saved together.
    pub(crate) fn sidekicks(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.acts.iter().map(|act| act.sidekick)
    }

    /// Notes a committed revision: the rows `changes` moved, given where each
    /// landed as [`crate::session_projection::ValidatedChanges::apply`]
    /// answers, and the Session's own row, which every revision moves.
    pub(crate) fn note(&mut self, changes: &[SessionChange], landed: &[Option<usize>]) {
        if !self.held {
            return;
        }
        self.rows.note(changes, landed);
        self.owe();
    }

    /// Notes that the Session's own row moved without a revision — whether it
    /// is set aside, or when it was viewed — with `acts`, the acts of
    /// Sidekicks the move follows. One whose history is not held drops both,
    /// as storage never held a Session it is told of after deleting it.
    pub(crate) fn note_summary(&mut self, acts: impl IntoIterator<Item = StoredSidekickAct>) {
        if !self.held {
            return;
        }
        self.acts.extend(acts);
        self.owe();
    }

    /// Holds `act`, an act of a Sidekick the Session's next revision follows,
    /// to land with that revision's save.
    pub(crate) fn hold_act(&mut self, act: StoredSidekickAct) {
        if self.held {
            self.acts.push(act);
        }
    }

    /// Has the Session's next save write it whole, because storage was found
    /// holding something other than what was last saved of it. Nothing about
    /// the Session moved, so a save waiting for it to go quiet need not wait
    /// longer for this.
    pub(crate) fn rewrite_whole(&mut self) {
        if !self.held {
            return;
        }
        self.rows = UnsavedRows::whole();
        self.owed = true;
    }

    fn owe(&mut self) {
        self.owed = true;
        self.moved_at = Some(Instant::now());
    }

    /// Encodes what storage lacks of the Session `session` reads, with the
    /// acts waiting here, into a save of its own, and owes nothing from then
    /// on: `None` where nothing is owed. Only the rows that moved are encoded,
    /// from borrows of the one copy of the Session, which is never cloned.
    /// A Session that cannot be encoded keeps owing what it did.
    pub(crate) fn take(
        &mut self,
        session: SessionRows<'_>,
    ) -> Result<Option<SessionSave>, StorageError> {
        if !self.owed {
            return Ok(None);
        }
        let rows = StoredRows::from_session(&session, &self.rows)?;
        self.rows.saved(session.snapshot);
        self.owed = false;
        self.moved_at = None;
        Ok(Some(SessionSave {
            rows,
            acts: std::mem::take(&mut self.acts),
        }))
    }
}

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
/// it stands where it is held.
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
