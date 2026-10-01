//! Cached projection of Session transcript content into renderable rows.
//!
//! The projection walks render units. A unit owns the transcript entries that
//! render as one block, and is what keying, memoization, and click hit-testing
//! address, so which entries share a unit is answered in one walk rather than
//! in each of those four places. Most units hold exactly one entry; a Group
//! holds a run of adjacent Activities of one groupable kind; a Turn Fold's
//! marker holds none, standing instead where the entries its Turn hides
//! would have been. Which entries a folded Turn hides is decided before the
//! walk, in [`TurnFolding`], because a Turn Fold keys on the Turn's Settle
//! rather than on the entry adjacency the walk reads.
//!
//! Vertical rhythm is decided between units rather than inside them. A
//! renderer never pads itself, because whitespace at a boundary is a property
//! of the pair of entries it falls between and no renderer can see what
//! follows it; the walk that lays units out inserts a blank row instead. See
//! [`Spacing::separates`] for the rule and [`assign_separators`] for the two
//! boundaries exempt from it. One consequence the rule is written to keep: no
//! step of a Fold moves the header row the reader clicked to open it.
//!
//! Rendering happens on every input event, so this module memoizes the
//! expensive work at two levels. The whole view is keyed on the Session
//! revision and content width: unchanged frames reuse it outright. When the
//! Session does change, each unit keeps its rendered lines and wrapped-row
//! counts, so a streaming append only re-renders the unit it touched. Frames
//! then extract just the viewport-sized window of lines instead of handing the
//! whole transcript to the terminal.

use std::{
    cell::{Cell, Ref, RefCell},
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::{Component, Path},
};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    ansi::{AnsiScanner, FragmentRole, sgr_parameter_code, sgr_parameters},
    protocol::{
        Activity, ActivityId, AttachmentDescriptor, Author, Delegator, FileChange, FoldPosture,
        GroupPosture, InitialPrompt, Message, MessageId, MessageRole, PromptId,
        ReasoningVisibility, SessionId, SessionRevision, SessionSnapshot, ToolCallVisibility,
        TranscriptItem, TranscriptSettings, Turn, TurnId, TurnStatus,
    },
    theme::Theme,
};

use super::{
    attachment_preview::{AttachmentPreviews, AttachmentRows, AttachmentStrip, STRIP_ROWS},
    markdown,
    slots::{SlotText, truncate_slot_text},
    spinner,
    text_binding::TextBindings,
    text_layout::{StyledLayout, StyledLine, StyledRow, StyledSpan, TextLayout},
};

/// Source lines wrapping to more rows than this are split, so no one line's
/// wrap runs unbounded when a stream hands the projection an enormous line.
/// Every piece after the first is flagged as a continuation of the line it
/// was cut from, so a copy can join what the cap split.
const MAX_TRANSCRIPT_SOURCE_LINE_ROWS: usize = 1_000;

/// Wrapped rows of output tail a settled command Activity's or Tool Call's
/// Peek shows below its fold marker.
const PEEK_OUTPUT_ROWS: usize = 6;

/// Wrapped rows of live tail an Active command Activity or Tool Call shows
/// while it streams, before it settles into its folded single row.
const LIVE_TAIL_ROWS: usize = 3;

/// The gutter an Activity's subordinate content sits in, so a fold or
/// truncation marker lines up with the lines it stands in for.
const OUTPUT_INDENT: &str = "    ";

/// The gutter for the detail rows belonging to a command or Tool Call: its
/// directory, its output, and the lines Suru writes about them. A header's
/// Marker already seats its text at the ordinary Activity-body level; these
/// details sit one level beneath that text.
const OUTPUT_DETAIL_INDENT: &str = "      ";

/// The extra gutter an expanded Group's or Turn Fold's members sit in, so a
/// member reads as subordinate to the header row it folds back into.
const MEMBER_INDENT: &str = "  ";

/// The bar and space every row of a user Message opens with.
const USER_MESSAGE_GUTTER: &str = "┃ ";

/// Columns of air a user Message keeps at its right edge, so its text ends
/// short of the surface it is drawn on.
const USER_MESSAGE_RIGHT_MARGIN: usize = 1;

/// The bar and space every row of a Delegation, or of a Message a Sidekick
/// sent, opens with. It is drawn on the same surface as a user Message's,
/// since each is what an Agent was asked, but lighter and in another role, so
/// neither ever reads as the user's.
const DELEGATION_GUTTER: &str = "│ ";

/// Paths a folded FileChange Activity lists before its fold marker.
const FOLDED_FILE_CHANGE_PATHS: usize = 4;

/// What a Reasoning Activity's header calls the block in each of its states.
/// Suru's own word for the concept is Reasoning, but the Transcript speaks the
/// reader's: an agent is thinking, and what it leaves behind is a thought.
const REASONING_ACTIVE_LABEL: &str = "Thinking";
const REASONING_COMPLETED_LABEL: &str = "Thought";
const REASONING_FAILED_LABEL: &str = "Thinking interrupted";

/// Which way a disclosure axis leans before any per-entry override.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DisclosurePosture {
    /// Entries keep their compact presentation unless the reader opened that
    /// one.
    #[default]
    Closed,
    /// Entries show everything unless the reader closed that one.
    Open,
}

/// The Fold axis is the one a Setting speaks for, so the posture a Session
/// view opens at is stated in the protocol's terms and lowered here.
impl From<FoldPosture> for DisclosurePosture {
    fn from(posture: FoldPosture) -> Self {
        match posture {
            FoldPosture::Folded => Self::Closed,
            FoldPosture::Expanded => Self::Open,
        }
    }
}

/// One client's state for one disclosure axis of one Session's Transcript:
/// the posture the view leans to, plus the entries the reader flipped away
/// from it. Disclosure is presentation only, so this never reaches the
/// Session, never syncs between clients, and dies with the process. The axis
/// is generic over what it keys on because the Transcript's axes disclose
/// different things: Groups and Folds key on an Activity, a Turn Fold on the
/// Turn it stands for.
#[derive(Clone, Debug)]
struct DisclosureAxis<Id> {
    posture: DisclosurePosture,
    overrides: HashSet<Id>,
}

/// Written out rather than derived because a derived `Default` would demand
/// one of the key type, which an empty axis has no use for.
impl<Id> Default for DisclosureAxis<Id> {
    fn default() -> Self {
        Self {
            posture: DisclosurePosture::default(),
            overrides: HashSet::new(),
        }
    }
}

impl<Id: Copy + Eq + Hash> DisclosureAxis<Id> {
    fn is_closed(&self, id: Id) -> bool {
        (self.posture == DisclosurePosture::Closed) != self.overrides.contains(&id)
    }

    /// Flips the whole axis between closed-by-default and open-by-default.
    /// Per-entry overrides are dropped so one invocation always reaches a
    /// posture the reader can predict.
    fn toggle_posture(&mut self) {
        self.posture = match self.posture {
            DisclosurePosture::Closed => DisclosurePosture::Open,
            DisclosurePosture::Open => DisclosurePosture::Closed,
        };
        self.overrides.clear();
    }

    /// Drops the overrides holding entries open, leaving the ones holding
    /// entries closed. Under the closed posture every override is an opening,
    /// so all of them go; under the open posture none is, so none does — which
    /// is what makes this a one-way close rather than a reset to the posture.
    fn close_opened_entries(&mut self) {
        if self.posture == DisclosurePosture::Closed {
            self.overrides.clear();
        }
    }

    /// Flips one entry between the axis's two steps, whichever way the reader
    /// left it.
    fn toggle(&mut self, id: Id) {
        self.set_closed(id, !self.is_closed(id));
    }

    fn set_closed(&mut self, id: Id, closed: bool) {
        if (self.posture == DisclosurePosture::Closed) == closed {
            self.overrides.remove(&id);
        } else {
            self.overrides.insert(id);
        }
    }

    /// Digest of the axis state, so the view cache rebuilds when it changes.
    fn fingerprint(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        (self.posture as u8).hash(&mut hasher);
        self.overrides.len().hash(&mut hasher);
        id_set_digest(&self.overrides).hash(&mut hasher);
        hasher.finish()
    }
}

/// How far one entry's Fold is open. A Fold may open in stages: every
/// foldable entry has `Folded` and `Expanded`, and a settled command Activity
/// or Tool Call adds `Peek` between them — the tail of its output behind a
/// fold marker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum FoldStep {
    /// The entry's most compact presentation.
    Folded,
    /// The intermediate step of a staged Fold: part of what the Fold hides,
    /// behind a marker counting the rest.
    Peek,
    /// Everything stored.
    Expanded,
}

/// The disclosure grammar and rendered state a projected unit reports to
/// pointer handling. Binary units need to distinguish a genuinely open
/// header from a folded row that happens to have nothing to hide.
#[derive(Clone, Copy, Debug)]
pub(super) enum FoldDisclosure {
    Binary { folded: bool },
    Staged(FoldStep),
}

/// One client's Fold state for one Session's Transcript: the posture the view
/// leans to, plus the step the reader put each entry at. Like every disclosure
/// axis this is presentation only, so it never reaches the Session, never
/// syncs between clients, and dies with the process.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptFolds {
    posture: DisclosurePosture,
    overrides: HashMap<ActivityId, FoldOverride>,
}

#[derive(Clone, Copy, Debug)]
struct FoldOverride {
    step: FoldStep,
    source: FoldOverrideSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FoldOverrideSource {
    Persistent,
    Automatic,
}

impl TranscriptFolds {
    /// A fresh Session view's Fold state: nothing the reader has touched yet,
    /// leaning the way the `transcript.defaultFoldPosture` Setting says a view
    /// opens.
    pub(super) fn opening_at(posture: FoldPosture) -> Self {
        Self {
            posture: posture.into(),
            overrides: HashMap::new(),
        }
    }

    /// The step an entry presents at: the reader's override if they set one,
    /// otherwise `default` under the closed posture and everything under the
    /// open one. The default is the entry's own because it depends on what the
    /// entry is — a failed command or Tool Call opens to its Peek where a
    /// successful one folds away.
    pub(super) fn resolve(&self, activity_id: ActivityId, default: FoldStep) -> FoldStep {
        if let Some(fold_override) = self.overrides.get(&activity_id) {
            return fold_override.step;
        }
        match self.posture {
            DisclosurePosture::Closed => default,
            DisclosurePosture::Open => FoldStep::Expanded,
        }
    }

    /// An Active Command or Tool Call always enters on its one-line shape. The
    /// posture is for stored entries opening with a Session view; only a
    /// per-Activity override can disclose work that is still running.
    fn resolve_active_output(&self, activity_id: ActivityId) -> FoldStep {
        self.overrides
            .get(&activity_id)
            .map(|fold_override| fold_override.step)
            .unwrap_or(FoldStep::Folded)
    }

    /// The binary reading entries without a Peek use: anything short of
    /// `Expanded` is folded.
    #[cfg(test)]
    pub(super) fn is_folded(&self, activity_id: ActivityId) -> bool {
        self.resolve(activity_id, FoldStep::Folded) != FoldStep::Expanded
    }

    /// Flips the whole axis between closed-by-default and open-by-default.
    /// Per-entry steps are dropped so one invocation always reaches a posture
    /// the reader can predict.
    pub(super) fn toggle_posture(&mut self) {
        self.posture = match self.posture {
            DisclosurePosture::Closed => DisclosurePosture::Open,
            DisclosurePosture::Open => DisclosurePosture::Closed,
        };
        self.overrides.clear();
    }

    pub(super) fn set_step(&mut self, activity_id: ActivityId, step: FoldStep) {
        self.overrides.insert(
            activity_id,
            FoldOverride {
                step,
                source: FoldOverrideSource::Persistent,
            },
        );
    }

    /// Whether an automatic live-tail promotion may claim this Activity. Any
    /// persistent per-Activity override wins; the general opening posture does
    /// not, because every Active Command or Tool Call begins Folded
    /// independently of it.
    pub(super) fn can_auto_promote(&self, activity_id: ActivityId) -> bool {
        !self.overrides.contains_key(&activity_id)
    }

    pub(super) fn auto_promote(&mut self, activity_id: ActivityId) {
        if self.can_auto_promote(activity_id) {
            self.overrides.insert(
                activity_id,
                FoldOverride {
                    step: FoldStep::Peek,
                    source: FoldOverrideSource::Automatic,
                },
            );
        }
    }

    /// Drops presentation overrides whose Active Command or Tool Call has
    /// settled. A persistent Fold step carries its provenance beside the step,
    /// so it survives the same status transition.
    pub(super) fn retain_automatic_promotions(&mut self, active: &HashSet<ActivityId>) {
        self.overrides.retain(|activity_id, fold_override| {
            fold_override.source != FoldOverrideSource::Automatic || active.contains(activity_id)
        });
    }

    pub(super) fn clear_automatic_promotions(&mut self) {
        self.retain_automatic_promotions(&HashSet::new());
    }

    pub(super) fn expand(&mut self, activity_id: ActivityId) {
        self.set_step(activity_id, FoldStep::Expanded);
    }

    pub(super) fn fold(&mut self, activity_id: ActivityId) {
        self.set_step(activity_id, FoldStep::Folded);
    }

    /// Digest of the axis state, so the view cache rebuilds when it changes.
    fn fingerprint(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        (self.posture as u8).hash(&mut hasher);
        self.overrides.len().hash(&mut hasher);
        let mut digest = 0u64;
        for (id, fold_override) in &self.overrides {
            let mut entry = std::hash::DefaultHasher::new();
            id.hash(&mut entry);
            (fold_override.step as u8).hash(&mut entry);
            digest ^= entry.finish();
        }
        digest.hash(&mut hasher);
        hasher.finish()
    }
}

/// Order-independent digest of a set of ids, so a view-state fingerprint over
/// one never depends on hash iteration order.
fn id_set_digest<Id: Hash>(ids: &HashSet<Id>) -> u64 {
    let mut digest = 0u64;
    for id in ids {
        let mut hasher = std::hash::DefaultHasher::new();
        id.hash(&mut hasher);
        digest ^= hasher.finish();
    }
    digest
}

/// One client's Group state for one Session's Transcript: a Group, named by
/// its first member, collapses to its single header row while its axis is
/// closed. See `DisclosureAxis` for the posture and locality semantics.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptGroups(DisclosureAxis<ActivityId>);

impl TranscriptGroups {
    /// A fresh Session view's Group state, leaning the way the
    /// `transcript.groups` Setting says a view opens. Under `off` no Group
    /// forms, so the posture it leans to is one nothing reads.
    pub(super) fn opening_at(posture: GroupPosture) -> Self {
        Self(DisclosureAxis {
            posture: match posture {
                GroupPosture::Expanded => DisclosurePosture::Open,
                GroupPosture::Collapsed | GroupPosture::Off => DisclosurePosture::Closed,
            },
            overrides: HashSet::new(),
        })
    }

    pub(super) fn is_collapsed(&self, group_id: ActivityId) -> bool {
        self.0.is_closed(group_id)
    }

    pub(super) fn toggle_posture(&mut self) {
        self.0.toggle_posture();
    }

    pub(super) fn expand(&mut self, group_id: ActivityId) {
        self.0.set_closed(group_id, false);
    }

    pub(super) fn collapse(&mut self, group_id: ActivityId) {
        self.0.set_closed(group_id, true);
    }

    fn fingerprint(&self) -> u64 {
        self.0.fingerprint()
    }
}

/// One client's Turn Fold state for one Session's Transcript: a settled Turn
/// stands as its single marker row while its axis is closed. The axis is
/// binary — a Turn Fold has no Peek — and, like the other two, it is view
/// state of the client that holds it. See `DisclosureAxis` for the posture and
/// locality semantics.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptTurnFolds(DisclosureAxis<TurnId>);

impl TranscriptTurnFolds {
    pub(super) fn is_folded(&self, turn_id: TurnId) -> bool {
        self.0.is_closed(turn_id)
    }

    /// Flips the whole axis between folded-by-default and expanded-by-default.
    /// Per-Turn flips are dropped so one invocation always reaches a posture
    /// the reader can predict.
    pub(super) fn toggle_posture(&mut self) {
        self.0.toggle_posture();
    }

    /// Flips one Turn between its marker and the work behind it, which is what
    /// a reader asks for by clicking that marker.
    pub(super) fn toggle(&mut self, turn_id: TurnId) {
        self.0.toggle(turn_id);
    }

    /// Opens one Turn against the axis, which is what interrupting a Turn
    /// asks for: the reader was watching that work, so the fold it settles
    /// into must not close over the place they had reached.
    pub(super) fn expand(&mut self, turn_id: TurnId) {
        self.0.set_closed(turn_id, false);
    }

    /// Folds back every Turn the reader had opened, which is what a newer Turn
    /// beginning does to the ones before it. This is deliberately unlike a
    /// per-entry Fold, whose override is sticky: a Turn Fold exists to compress
    /// past work, so an expansion taken against a Turn lasts only until the
    /// reader moves on to the next one. It only ever closes: a Turn the reader
    /// folded by hand under the expanded posture stays folded, because
    /// compressing past work is no reason to open anything.
    pub(super) fn refold_expanded_turns(&mut self) {
        self.0.close_opened_entries();
    }

    fn fingerprint(&self) -> u64 {
        self.0.fingerprint()
    }
}

/// The disclosure axes a client renders a Transcript through: which kinds of
/// Activity are visible and Turn Folds decide which entries reach the
/// projection at all, grouping and Groups which of the survivors share a row,
/// and Folds how much of a row shows. Rendering reads all five, so they travel
/// as one input. Three are the reader's own clicks, held per Session; the
/// other two are decided by Settings, which is why they arrive by value from
/// the effective settings rather than as view state a Session keeps.
#[derive(Clone, Copy)]
pub(super) struct TranscriptDisclosure<'a> {
    pub(super) folds: &'a TranscriptFolds,
    pub(super) groups: &'a TranscriptGroups,
    pub(super) turns: &'a TranscriptTurnFolds,
    pub(super) visibility: ActivityVisibility,
    pub(super) grouping: Grouping,
}

/// Whether a Transcript gathers runs of Activities into Groups at all, read
/// off the `transcript.groups` Setting. Where the posture a view's Groups open
/// at only decides where a fresh view starts, this acts on arrival: turning
/// grouping off lays every Activity out as its own row in the Transcript
/// already on screen, and turning it back on gathers them again at whatever
/// posture that view holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Grouping {
    Formed,
    Off,
}

impl Grouping {
    pub(super) const fn of(settings: &TranscriptSettings) -> Self {
        match settings.groups {
            GroupPosture::Off => Self::Off,
            GroupPosture::Collapsed | GroupPosture::Expanded => Self::Formed,
        }
    }
}

/// Which kinds of Activity a reader has a Transcript draw at all — the kinds a
/// Setting may hide wholesale — read off the effective settings as one value,
/// so the walk deciding which entries are there to be read and the cache key
/// deciding when to read them again cannot disagree about any kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ActivityVisibility {
    pub(super) reasoning: ReasoningVisibility,
    pub(super) tool_calls: ToolCallVisibility,
}

impl ActivityVisibility {
    pub(super) const fn of(settings: &TranscriptSettings) -> Self {
        Self {
            reasoning: settings.reasoning_visibility,
            tool_calls: settings.tool_call_visibility,
        }
    }
}

/// A kind of Provider stream Suru stores under a cap. The kind decides what a
/// truncation marker names, so the marker ends the thing the reader was
/// reading rather than a word that only fits one of the two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CappedStream {
    /// Agent prose stored as the content of a Message.
    Message,
    /// The output a command wrote, stored on its Activity.
    CommandOutput,
    /// The input a Tool Call was given, stored on its Activity.
    ToolCallInput,
    /// The text a Tool Call's result carried, stored on its Activity.
    ToolCallOutput,
    /// The summary of a Reasoning block, stored on its Activity.
    Reasoning,
    /// The summary a Compaction left, stored on its Activity.
    CompactionSummary,
    /// The typed subject and reason stored on an Approval Activity.
    ApprovalDetail,
}

impl CappedStream {
    /// What the transcript shows in place of the content a cap cut short. Every
    /// marker is decided here so the stream kinds cannot drift apart in
    /// wording. A marker is drawn from the stored truncation signal rather than
    /// read out of stored content, so content that ends with these characters
    /// stays ordinary text.
    const fn truncation_marker(self) -> &'static str {
        match self {
            Self::Message => "[Message truncated]",
            Self::CommandOutput | Self::ToolCallOutput => "[output truncated]",
            Self::ToolCallInput => "[input truncated]",
            Self::Reasoning => "[Reasoning truncated]",
            Self::CompactionSummary => "[Summary truncated]",
            Self::ApprovalDetail => "[Approval detail truncated]",
        }
    }
}

/// A Prompt not yet delivered, drawn in the Transcript's position as the user
/// Message it will become: what it asks, and who sent it on the user's behalf
/// where the user did not, so it is drawn apart from the user's own words
/// before it is delivered as after.
#[derive(Clone, Debug)]
pub(super) struct PendingPrompt {
    pub(super) prompt: InitialPrompt,
    pub(super) author: Option<Author>,
}

impl PendingPrompt {
    /// A Prompt the user sent themselves.
    pub(super) fn users(prompt: InitialPrompt) -> Self {
        Self {
            prompt,
            author: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MessageStart {
    pub(super) message_id: MessageId,
    pub(super) row: usize,
}

/// Where a projected unit sits in the rendered rows, so a pointer lands on a
/// unit in one lookup instead of a re-render. `header_rows` covers the unit's
/// header line, the only part of an expanded unit that closes it again.
#[derive(Clone, Copy, Debug)]
pub(super) struct UnitStart {
    pub(super) key: UnitKey,
    pub(super) row: usize,
    pub(super) header_rows: usize,
    pub(super) row_count: usize,
    /// Whether the unit as drawn is holding content back, so a click only
    /// toggles a Fold that is really there.
    pub(super) hides_content: bool,
    /// The Fold grammar and state that were actually drawn, so a click does
    /// not have to re-derive presentation from the client's opening posture.
    pub(super) fold: FoldDisclosure,
    /// The row the unit's fold marker was drawn on, the click target that
    /// opens a Peek the rest of the way.
    pub(super) marker_row: Option<usize>,
}

impl UnitStart {
    pub(super) const fn contains(&self, row: usize) -> bool {
        row >= self.row && row < self.row + self.row_count
    }

    pub(super) const fn is_header(&self, row: usize) -> bool {
        row >= self.row && row < self.row + self.header_rows
    }
}

/// Memoized transcript view owned by the render state. Interior mutability
/// keeps the cache transparent to callers that render from `&TuiState`.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptCache {
    view: RefCell<Option<TranscriptView>>,
    selection_epoch: Cell<u64>,
}

impl TranscriptCache {
    pub(super) fn selection_epoch(&self) -> u64 {
        self.selection_epoch.get()
    }

    pub(super) fn copy_selection(
        &self,
        selection: super::selection::TextSelection,
    ) -> Option<super::ClipboardContent> {
        if selection.epoch != self.selection_epoch() {
            return None;
        }
        self.view.borrow().as_ref()?.copy_selection(selection)
    }

    pub(super) fn row_count(&self) -> usize {
        self.view
            .borrow()
            .as_ref()
            .map_or(0, TranscriptView::row_count)
    }

    /// The first and last cells of the word under a Transcript cell, or
    /// `None` where no word is (see [`TranscriptView::word_cells`]).
    pub(super) fn word_cells(
        &self,
        row: usize,
        column: usize,
    ) -> Option<(
        super::selection::SelectionCell,
        super::selection::SelectionCell,
    )> {
        self.view.borrow().as_ref()?.word_cells(row, column)
    }

    /// Bounds of the whole written Line, including pieces split by the cap.
    pub(super) fn line_cells(
        &self,
        row: usize,
    ) -> Option<(
        super::selection::SelectionCell,
        super::selection::SelectionCell,
    )> {
        self.view.borrow().as_ref()?.line_cells(row)
    }

    pub(super) fn hyperlink_at(&self, row: usize, column: usize) -> Option<String> {
        self.view.borrow().as_ref()?.hyperlink_at(row, column)
    }

    /// Returns the transcript view for the given content, rebuilding only the
    /// parts whose inputs changed since the previous frame.
    #[cfg(test)]
    pub(super) fn view(
        &self,
        generation: u64,
        snapshot: &SessionSnapshot,
        provisional: &[&PendingPrompt],
        disclosure: TranscriptDisclosure<'_>,
        theme: &Theme,
        width: u16,
    ) -> Ref<'_, TranscriptView> {
        self.view_with_hyperlinks(
            generation,
            snapshot,
            provisional,
            disclosure,
            &AttachmentPreviews::default(),
            theme,
            width,
            false,
        )
    }

    // One parameter per input the cached view is keyed on, so a change to any
    // of them is visibly a change to what gets rebuilt.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn view_with_hyperlinks(
        &self,
        generation: u64,
        snapshot: &SessionSnapshot,
        provisional: &[&PendingPrompt],
        disclosure: TranscriptDisclosure<'_>,
        previews: &AttachmentPreviews,
        theme: &Theme,
        width: u16,
        hyperlinks: bool,
    ) -> Ref<'_, TranscriptView> {
        let key = ViewKey {
            previews: previews.fingerprint(),
            generation,
            session_id: snapshot.session.id,
            revision: snapshot.revision,
            theme: *theme,
            width,
            hyperlinks,
            visibility: disclosure.visibility,
            grouping: disclosure.grouping,
            provisional_fingerprint: provisional_fingerprint(provisional),
            folds_fingerprint: disclosure.folds.fingerprint(),
            groups_fingerprint: disclosure.groups.fingerprint(),
            turns_fingerprint: disclosure.turns.fingerprint(),
        };
        let needs_rebuild = self
            .view
            .borrow()
            .as_ref()
            .is_none_or(|view| view.key != key);
        if needs_rebuild {
            let mut slot = self.view.borrow_mut();
            let previous = slot.take();
            let previous_key = previous.as_ref().map(|view| view.key);
            let previous_units = previous
                .as_ref()
                .map(|view| {
                    view.units
                        .iter()
                        .map(|unit| (unit.key, unit.fingerprint, unit.leading_separator))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let rebuilt = rebuild(
                previous,
                key,
                snapshot,
                provisional,
                disclosure,
                previews,
                theme,
                width,
            );
            let same_layout = previous_key.is_some_and(|old| {
                old.generation == key.generation
                    && old.session_id == key.session_id
                    && old.width == key.width
                    && old.hyperlinks == key.hyperlinks
                    && old.theme == key.theme
                    && old.visibility == key.visibility
                    && old.grouping == key.grouping
                    && old.folds_fingerprint == key.folds_fingerprint
                    && old.groups_fingerprint == key.groups_fingerprint
                    && old.turns_fingerprint == key.turns_fingerprint
            });
            let pure_append = rebuilt.units.len() >= previous_units.len()
                && previous_units
                    .iter()
                    .zip(&rebuilt.units)
                    .all(|(old, unit)| {
                        *old == (unit.key, unit.fingerprint, unit.leading_separator)
                    });
            if !same_layout || !pure_append {
                self.selection_epoch
                    .set(self.selection_epoch.get().wrapping_add(1));
            }
            *slot = Some(rebuilt);
        }
        Ref::map(self.view.borrow(), |view| {
            view.as_ref().expect("transcript view was just rebuilt")
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewKey {
    generation: u64,
    session_id: SessionId,
    revision: SessionRevision,
    theme: Theme,
    width: u16,
    hyperlinks: bool,
    /// Which kinds of Activity are drawn at all, which the Settings decide
    /// rather than the reader's clicks. It is a rendering input outside the
    /// Session snapshot all the same, so ADR 0007 puts it in the key: a reader
    /// asking for Reasoning, or hiding Tool Calls, mid-Session moves the
    /// Transcript already on screen, where a default Fold posture only decides
    /// where a fresh view starts.
    visibility: ActivityVisibility,
    /// Whether runs gather into Groups, a Setting acting on the Transcript
    /// already on screen for the same reason visibility does.
    grouping: Grouping,
    provisional_fingerprint: u64,
    /// Folds are a rendering input outside the Session snapshot, so ADR 0007
    /// requires them in the key or a flipped Fold would render a stale frame.
    folds_fingerprint: u64,
    /// Group state is the same kind of input, so a flipped Group rebuilds too.
    groups_fingerprint: u64,
    /// Turn Fold state is the same kind of input again, and the one that
    /// decides which entries are projected at all.
    turns_fingerprint: u64,
    /// Whether Attachments present as lines or strips, and which thumbnails
    /// are ready, are inputs outside the snapshot too: a thumbnail arriving
    /// turns its Message's dimmed lines into the strip they stood in for.
    previews: u64,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptView {
    key: ViewKey,
    units: Vec<UnitView>,
    row_count: usize,
    message_starts: Vec<MessageStart>,
    unit_starts: Vec<UnitStart>,
}

/// A place in the Transcript's projected text: which projected line, counted
/// across the whole view with separators as lines of their own, and a byte
/// offset into that line's text (see [`StyledLine::written_text`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TextPosition {
    pub(super) line: usize,
    pub(super) offset: usize,
}

/// The line a separator row projects: nothing, continuing nothing.
static SEPARATOR_LINE: StyledLine = StyledLine {
    spans: Vec::new(),
    continuation: false,
    omitted_prefix: String::new(),
    markdown: None,
};

/// One cell of a wrapped row that draws text: its row-local column, its
/// width, and the byte offset of its grapheme in the line's written text.
#[derive(Clone, Copy, Debug)]
struct TextCell {
    column: usize,
    width: usize,
    offset: usize,
}

/// What a Transcript row draws: a unit's separator, or one wrapped row of a
/// projected line.
enum RowAt<'a> {
    Separator {
        line: usize,
    },
    Wrapped {
        line: usize,
        source: &'a StyledLine,
        row: &'a StyledRow,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TranscriptLink {
    pub(super) target: String,
}

impl TranscriptView {
    fn hyperlink_at(&self, row: usize, column: usize) -> Option<String> {
        let RowAt::Wrapped {
            source,
            row: wrapped,
            ..
        } = self.wrapped_row_at(row)?
        else {
            return None;
        };
        hyperlink_ranges(source, wrapped, 0, usize::from(self.key.width))
            .into_iter()
            .find(|link| column >= link.column && column < link.column + link.width)
            .map(|link| link.target)
    }

    fn copy_selection(
        &self,
        selection: super::selection::TextSelection,
    ) -> Option<super::ClipboardContent> {
        let (start, end) = selection.ordered();
        let start = self.position_at(start.row, start.column)?;
        let end = self.position_at_edge(end.row, end.column, true)?;
        enum Part {
            Plain(String),
            Markdown(
                std::sync::Arc<markdown::copy::Document>,
                Vec<markdown::copy::SourceRange>,
            ),
        }
        let mut parts = Vec::new();
        let mut copied = String::new();
        let mut has_source_line = false;
        let mut markdown: Option<std::sync::Arc<markdown::copy::Document>> = None;
        let mut ranges = Vec::new();
        for index in start.line..=end.line {
            let line = self.projected_line(index)?;
            if !line.spans.is_empty()
                && line
                    .spans
                    .iter()
                    .all(|span| span.chrome && span.source.is_none())
            {
                continue;
            }
            let same_document = match (&markdown, &line.markdown) {
                (Some(left), Some(right)) => std::sync::Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            };
            if !same_document {
                if let Some(document) = markdown.take() {
                    parts.push(Part::Plain(std::mem::take(&mut copied)));
                    parts.push(Part::Markdown(document, std::mem::take(&mut ranges)));
                }
                if has_source_line && line.markdown.is_some() {
                    copied.push('\n');
                }
                markdown = line.markdown.clone();
            }
            if markdown.is_none() && has_source_line {
                if line.continuation {
                    copied.push_str(&line.omitted_prefix);
                } else {
                    copied.push('\n');
                }
            }
            has_source_line = true;
            let from = if index == start.line { start.offset } else { 0 };
            let to = if index == end.line {
                end.offset
            } else {
                usize::MAX
            };
            let selected = line.slice(from..to);
            for span in &selected.spans {
                if span.chrome && span.source.is_none() {
                    continue;
                }
                if markdown.is_some() {
                    if let Some(source) = &span.source {
                        ranges.push(source.clone());
                    }
                } else {
                    copied.push_str(&span.content);
                }
            }
        }
        if let Some(document) = markdown {
            parts.push(Part::Plain(std::mem::take(&mut copied)));
            parts.push(Part::Markdown(document, ranges));
        }
        parts.push(Part::Plain(copied));
        let single_content = parts
            .iter()
            .filter(|part| match part {
                Part::Plain(text) => !text.trim().is_empty(),
                Part::Markdown(_, ranges) => !ranges.is_empty(),
            })
            .count()
            == 1;
        let copied = parts
            .into_iter()
            .map(|part| match part {
                Part::Plain(text) => super::ClipboardContent::from(text),
                Part::Markdown(document, ranges) => document.copy(&ranges, single_content),
            })
            .collect::<Vec<_>>();
        let html = copied.iter().any(|part| part.html.is_some()).then(|| {
            copied
                .iter()
                .map(|part| {
                    part.html.clone().unwrap_or_else(|| {
                        if part.text.trim().is_empty() {
                            String::new()
                        } else {
                            part.plain_html()
                        }
                    })
                })
                .collect::<String>()
        });
        let text = copied.into_iter().map(|part| part.text).collect::<String>();
        Some(super::ClipboardContent {
            text: text
                .split('\n')
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n"),
            html,
        })
    }

    pub(super) fn row_count(&self) -> usize {
        self.row_count
    }

    pub(super) fn row_count_with_tail(&self, tail: &[Line<'static>]) -> usize {
        self.row_count.saturating_add(tail.len())
    }

    /// The projected line and byte offset behind a Transcript cell, resolved
    /// through the same wrap that put the cell there, or `None` for a row
    /// past the Transcript. A separator row resolves to its own empty line.
    pub(super) fn position_at(&self, row: usize, column: usize) -> Option<TextPosition> {
        self.position_at_edge(row, column, false)
    }

    fn position_at_edge(&self, row: usize, column: usize, after: bool) -> Option<TextPosition> {
        let (line, wrapped) = match self.wrapped_row_at(row)? {
            RowAt::Separator { line } => return Some(TextPosition { line, offset: 0 }),
            RowAt::Wrapped { line, source, row } => (line, (source, row)),
        };
        let (source, row) = wrapped;
        let offset = if after {
            row.offset_after(source, self.key.width, column)
        } else {
            row.offset_at(source, self.key.width, column)
        };
        Some(TextPosition { line, offset })
    }

    /// The wrapped row drawn at a Transcript row, with the projected line it
    /// draws, or `None` past the Transcript.
    fn wrapped_row_at(&self, row: usize) -> Option<RowAt<'_>> {
        if row >= self.row_count {
            return None;
        }
        let unit = self.unit_at(|unit| unit.start_row, row)?;
        let mut local_row = row - unit.start_row;
        let mut line = unit.start_line;
        if unit.leading_separator {
            if local_row == 0 {
                return Some(RowAt::Separator { line });
            }
            local_row -= 1;
            line += 1;
        }
        let wrapped = unit.rows.get(local_row)?;
        Some(RowAt::Wrapped {
            line: line + wrapped.line,
            source: &unit.lines[wrapped.line],
            row: &wrapped.row,
        })
    }

    /// Reverses the cells a selection covers, the way the copy reads them:
    /// text only. The gutter, hanging indent, and every other chrome span
    /// stay as drawn, so what lights up is what a copy would hold. Rows that
    /// draw no text — separators, blank lines — light nothing.
    pub(super) fn highlight_selection(
        &self,
        selection: super::selection::TextSelection,
        buffer: &mut Buffer,
        area: Rect,
        scroll: usize,
    ) {
        let (start, end) = selection.ordered();
        for y in area.y..area.bottom() {
            let row = scroll + usize::from(y - area.y);
            if row < start.row {
                continue;
            }
            if row > end.row {
                break;
            }
            let Some(RowAt::Wrapped {
                source,
                row: wrapped,
                ..
            }) = self.wrapped_row_at(row)
            else {
                continue;
            };
            for TextCell { column, width, .. } in self.text_cells(source, wrapped) {
                let selected_from_start = row > start.row || column + width > start.column;
                let selected_to_end = row < end.row || column <= end.column;
                if !(selected_from_start && selected_to_end) {
                    continue;
                }
                let left = u16::try_from(column).unwrap_or(u16::MAX);
                let right = u16::try_from(column + width).unwrap_or(u16::MAX);
                for x in area.x.saturating_add(left)..area.x.saturating_add(right).min(area.right())
                {
                    buffer[(x, y)].modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    /// The first and last text cells of the word under a Transcript cell,
    /// across every row the word's line wraps to. A word is the run of
    /// graphemes of one class (see [`super::selection::word_range`]) in the
    /// line's written text, stopped at chrome spans and the line's ends, so a
    /// word a soft wrap split is whole. `None` on a separator, on chrome, or
    /// past a row's text.
    pub(super) fn word_cells(
        &self,
        row: usize,
        column: usize,
    ) -> Option<(
        super::selection::SelectionCell,
        super::selection::SelectionCell,
    )> {
        use super::selection::SelectionCell;
        if row >= self.row_count {
            return None;
        }
        let unit = self.unit_at(|unit| unit.start_row, row)?;
        let mut local_row = row - unit.start_row;
        let mut first_row = unit.start_row;
        if unit.leading_separator {
            if local_row == 0 {
                return None;
            }
            local_row -= 1;
            first_row += 1;
        }
        let wrapped = unit.rows.get(local_row)?;
        let source = &unit.lines[wrapped.line];
        if column < wrapped.row.indent {
            return None;
        }
        let offset = wrapped.row.offset_at(source, self.key.width, column);
        if offset >= wrapped.row.end {
            return None;
        }
        // The word lives in the run of text spans around the offset: chrome
        // on either side bounds it as the line's ends do.
        let mut span_ranges = Vec::with_capacity(source.spans.len());
        let mut span_start = 0;
        for span in &source.spans {
            let span_end = span_start + span.content.len();
            span_ranges.push((span_start..span_end, span.chrome));
            span_start = span_end;
        }
        let hit = span_ranges
            .iter()
            .position(|(range, _)| range.contains(&offset))?;
        if span_ranges[hit].1 {
            return None;
        }
        let run_start = span_ranges[..hit]
            .iter()
            .rposition(|(_, chrome)| *chrome)
            .map_or(0, |index| span_ranges[index].0.end);
        let run_end = span_ranges[hit + 1..]
            .iter()
            .find(|(_, chrome)| *chrome)
            .map_or(span_start, |(range, _)| range.start);
        let text = source.written_text();
        let word = super::selection::word_range(&text[run_start..run_end], offset - run_start)?;
        let word = word.start + run_start..word.end + run_start;
        let mut first = None;
        let mut last = None;
        for (index, other) in unit.rows.iter().enumerate() {
            if other.line != wrapped.line {
                continue;
            }
            let screen_row = first_row + index;
            for text_cell in self.text_cells(source, &other.row) {
                if !word.contains(&text_cell.offset) {
                    continue;
                }
                let cell = SelectionCell {
                    row: screen_row,
                    column: text_cell.column,
                };
                first.get_or_insert(cell);
                last = Some(cell);
            }
        }
        Some((first?, last?))
    }

    /// Bounds of the written Line containing `row`, across soft wraps and
    /// cap-split continuations. Chrome and hanging indents are excluded by
    /// the same cell projection used to highlight and copy. Only Code Blocks
    /// retain authored leading whitespace; other Lines start at their text.
    pub(super) fn line_cells(
        &self,
        row: usize,
    ) -> Option<(
        super::selection::SelectionCell,
        super::selection::SelectionCell,
    )> {
        use super::selection::SelectionCell;
        let hit = self.wrapped_row_at(row)?;
        // The exclusive right edge maps to the row's text end when copied
        // and covers no painted cell, representing an empty Line.
        let empty = SelectionCell {
            row,
            column: usize::from(self.key.width),
        };
        let RowAt::Wrapped { .. } = hit else {
            return Some((empty, empty));
        };
        let unit = self.unit_at(|unit| unit.start_row, row)?;
        let row_base = unit.start_row + usize::from(unit.leading_separator);
        let mut first_line = unit.rows[row - row_base].line;
        while first_line > 0 && unit.lines[first_line].continuation {
            first_line -= 1;
        }
        let mut end_line = first_line + 1;
        while end_line < unit.lines.len() && unit.lines[end_line].continuation {
            end_line += 1;
        }
        let source = &unit.lines[first_line];
        let preserve_indent = source.markdown.as_ref().is_some_and(|document| {
            source.spans.iter().any(|span| {
                span.source
                    .as_ref()
                    .is_some_and(|range| document.is_code(range))
            })
        });
        let mut first = None;
        let mut last = None;
        let mut written_line = None;
        let mut text = String::new();
        for (index, wrapped) in unit.rows.iter().enumerate() {
            if !(first_line..end_line).contains(&wrapped.line) {
                continue;
            }
            let source = &unit.lines[wrapped.line];
            if written_line != Some(wrapped.line) {
                text = source.written_text();
                written_line = Some(wrapped.line);
            }
            for text_cell in self.text_cells(source, &wrapped.row) {
                let cell = SelectionCell {
                    row: row_base + index,
                    column: text_cell.column,
                };
                let nonblank = text[text_cell.offset..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace());
                if nonblank || preserve_indent {
                    first.get_or_insert(cell);
                }
                if nonblank {
                    last = Some(cell);
                }
            }
        }
        Some(last.map_or((empty, empty), |last| (first.unwrap_or(last), last)))
    }

    /// The cells of a wrapped row that draw text rather than chrome, read the
    /// way the draw and the offset lookup read them: symbol by symbol, span by
    /// span, past the indent.
    fn text_cells(&self, source: &StyledLine, wrapped: &StyledRow) -> Vec<TextCell> {
        use unicode_segmentation::UnicodeSegmentation;
        let maximum = usize::from(self.key.width);
        let mut cells = Vec::new();
        let mut column = wrapped.indent;
        let mut span_start = 0;
        for span in &source.spans {
            let span_end = span_start + span.content.len();
            let from = wrapped.start.clamp(span_start, span_end) - span_start;
            let to = wrapped.end.clamp(span_start, span_end) - span_start;
            for (offset, symbol) in span.content[from..to].grapheme_indices(true) {
                let width = symbol.width();
                if width == 0 || width > maximum {
                    continue;
                }
                if !span.chrome {
                    cells.push(TextCell {
                        column,
                        width,
                        offset: span_start + from + offset,
                    });
                }
                column += width;
            }
            span_start = span_end;
        }
        cells
    }

    /// A projected line by the index [`Self::position_at`] reports, or `None`
    /// past the last one.
    pub(super) fn projected_line(&self, line: usize) -> Option<&StyledLine> {
        let unit = self.unit_at(|unit| unit.start_line, line)?;
        let mut local = line - unit.start_line;
        if unit.leading_separator {
            if local == 0 {
                return Some(&SEPARATOR_LINE);
            }
            local -= 1;
        }
        unit.lines.get(local)
    }

    pub(super) fn message_starts(&self) -> &[MessageStart] {
        &self.message_starts
    }

    pub(super) fn unit_starts(&self) -> &[UnitStart] {
        &self.unit_starts
    }

    /// Parsed hyperlink targets retained for future semantic commands and
    /// pointer hit-testing.
    #[allow(dead_code)]
    pub(super) fn links(&self) -> impl Iterator<Item = &TranscriptLink> {
        self.units.iter().flat_map(|unit| unit.links.iter())
    }

    /// The last unit whose `start` is at or before `index`: the one an index
    /// counted across the view falls in, or `None` before the first unit.
    fn unit_at(&self, start: impl Fn(&UnitView) -> usize, index: usize) -> Option<&UnitView> {
        self.units.get(
            self.units
                .partition_point(|unit| start(unit) <= index)
                .checked_sub(1)?,
        )
    }

    /// Extracts the rows needed to draw `viewport_rows` rows starting at
    /// `scroll_position`. Every row is already wrapped, so a window opening
    /// partway through a wrapped line starts on that row rather than at the
    /// line's beginning. The result is bounded by the viewport, not the
    /// transcript.
    pub(super) fn window(&self, scroll_position: usize, viewport_rows: usize) -> TranscriptWindow {
        let mut rows = Vec::with_capacity(viewport_rows.min(self.row_count));
        let mut spinner_rows = Vec::new();
        let mut hyperlinks = Vec::new();
        if viewport_rows == 0 {
            return TranscriptWindow {
                rows,
                spinner_rows,
                hyperlinks,
                strips: Vec::new(),
            };
        }
        let first_unit = self
            .units
            .partition_point(|unit| unit.start_row <= scroll_position)
            .saturating_sub(1);
        let strips = self.strips_in(first_unit, scroll_position, viewport_rows);
        'units: for unit in &self.units[first_unit..] {
            let mut skip = scroll_position.saturating_sub(unit.start_row);
            if unit.leading_separator {
                // The separator is the unit's first row for indexing, so a
                // window opening on it draws it and one opening past it skips
                // it along with the rows before `scroll_position`.
                if skip == 0 {
                    rows.push(Line::default());
                    if rows.len() >= viewport_rows {
                        break 'units;
                    }
                } else {
                    skip -= 1;
                }
            }
            for (index, wrapped) in unit.rows.iter().enumerate().skip(skip) {
                if unit.spinner_rows.binary_search(&index).is_ok() {
                    spinner_rows.push(rows.len());
                }
                let window_row = rows.len();
                hyperlinks.extend(hyperlink_ranges(
                    &unit.lines[wrapped.line],
                    &wrapped.row,
                    window_row,
                    usize::from(self.key.width),
                ));
                rows.push(wrapped.row.line.clone());
                if rows.len() >= viewport_rows {
                    break 'units;
                }
            }
        }
        TranscriptWindow {
            rows,
            spinner_rows,
            hyperlinks,
            strips,
        }
    }

    /// Every strip with any of its reserved rows inside the window, from the
    /// unit the window opens in: whole or not, since one partly in view is
    /// still one whose thumbnails are worth fetching.
    fn strips_in(
        &self,
        first_unit: usize,
        scroll_position: usize,
        viewport_rows: usize,
    ) -> Vec<WindowStrip> {
        let end = scroll_position.saturating_add(viewport_rows);
        let mut strips = Vec::new();
        for unit in &self.units[first_unit..] {
            let first_row = unit.start_row + usize::from(unit.leading_separator);
            if unit.start_row >= end {
                break;
            }
            for strip in &unit.strips {
                let top = first_row + strip.start;
                if top + usize::from(STRIP_ROWS) > scroll_position && top < end {
                    strips.push(WindowStrip {
                        top: top as isize - scroll_position as isize,
                        left: strip.left,
                        width: strip.width,
                        strip: strip.strip.clone(),
                    });
                }
            }
        }
        strips
    }

    /// Draws the memoized Transcript followed by transient, one-row tail
    /// presentation. The tail participates in scrolling without joining the
    /// projection or its cache key, so changing elapsed time or animation
    /// frames never rebuilds persisted Transcript content (ADRs 0007, 0009).
    pub(super) fn window_with_tail(
        &self,
        tail: &[Line<'static>],
        scroll_position: usize,
        viewport_rows: usize,
    ) -> TranscriptWindow {
        let transcript_rows = self
            .row_count
            .saturating_sub(scroll_position)
            .min(viewport_rows);
        let mut window = if scroll_position < self.row_count {
            self.window(scroll_position, transcript_rows)
        } else {
            TranscriptWindow {
                rows: Vec::new(),
                spinner_rows: Vec::new(),
                hyperlinks: Vec::new(),
                strips: Vec::new(),
            }
        };
        let first_tail = scroll_position.saturating_sub(self.row_count);
        let tail_rows = viewport_rows.saturating_sub(transcript_rows);
        window
            .rows
            .extend(tail.iter().skip(first_tail).take(tail_rows).cloned());
        window
    }
}

/// The viewport-sized slice of a [`TranscriptView`] one frame draws: its
/// rows, each already wrapped to the view width, and which of them carry a
/// Spinner for the draw-time overlay (ADR 0009) to patch.
pub(super) struct TranscriptWindow {
    pub(super) rows: Vec<Line<'static>>,
    /// Indices into `rows` whose Marker cell holds a Spinner.
    pub(super) spinner_rows: Vec<usize>,
    pub(super) hyperlinks: Vec<VisibleHyperlink>,
    /// The strips of thumbnails with any reserved row in the window.
    pub(super) strips: Vec<WindowStrip>,
}

/// A strip of thumbnails as a window sees it: the window row its reserved
/// rows begin on — above the window where it has scrolled part way off — and
/// the columns it stands across.
pub(super) struct WindowStrip {
    pub(super) top: isize,
    pub(super) left: u16,
    pub(super) width: u16,
    pub(super) strip: AttachmentStrip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VisibleHyperlink {
    pub(super) row: usize,
    pub(super) column: usize,
    pub(super) width: usize,
    pub(super) target: String,
}

fn hyperlink_ranges(
    source: &StyledLine,
    wrapped: &StyledRow,
    row: usize,
    maximum: usize,
) -> Vec<VisibleHyperlink> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut ranges: Vec<VisibleHyperlink> = Vec::new();
    let mut column = wrapped.indent;
    let mut span_start = 0;
    for span in &source.spans {
        let span_end = span_start + span.content.len();
        let from = wrapped.start.clamp(span_start, span_end) - span_start;
        let to = wrapped.end.clamp(span_start, span_end) - span_start;
        for symbol in span.content[from..to].graphemes(true) {
            let width = symbol.width();
            if width == 0 || width > maximum || column >= maximum {
                continue;
            }
            let width = width.min(maximum - column);
            if let Some(target) = &span.target {
                if let Some(previous) = ranges.last_mut()
                    && previous.target == *target
                    && previous.column + previous.width == column
                {
                    previous.width += width;
                } else {
                    ranges.push(VisibleHyperlink {
                        row,
                        column,
                        width,
                        target: target.clone(),
                    });
                }
            }
            column += width;
        }
        span_start = span_end;
    }
    ranges
}

/// One block of the Transcript the projection renders as a whole: the entries
/// that share a cache key, a fingerprint, and a click target. A unit answers
/// for all three itself, so rendering, memoization, and hit-testing address
/// the unit rather than what it holds.
enum RenderUnit<'a> {
    /// A Message, with the parent of the Session whose Transcript holds it —
    /// a Delegation names its sender to the reader against that parent — and
    /// how a user Message presents its Attachments beneath its text.
    Message(&'a Message, Option<SessionId>, AttachmentRows),
    Activity(&'a Activity),
    /// A Group: a run of one or more adjacent Activities of one groupable
    /// kind. Collapsed it is the run's single row; expanded it is the header
    /// the run re-collapses from, followed by whatever its kind opens onto.
    /// Never empty; [`close_run`] holds that invariant.
    Group {
        kind: GroupableKind,
        members: Vec<&'a Activity>,
        expanded: bool,
    },
    /// One member of an expanded Group: an ordinary Activity drawn in the
    /// member gutter. A member is its own unit so its Fold, its cached lines,
    /// and its click target all work exactly as they do standalone. Only the
    /// kinds whose expansion opens onto their members plan these.
    GroupMember(&'a Activity),
    /// A unit an expanded Turn Fold disclosed: the unit it would be
    /// standalone, drawn in the member gutter so the disclosed work reads as
    /// subordinate to the marker it folds back into, exactly as an expanded
    /// Group's members read under their header. Wrapping rather than flagging
    /// keeps member-ness one seam: the inner unit's key, Fold, and click
    /// target all work exactly as they do standalone.
    TurnMember(Box<RenderUnit<'a>>),
    /// A settled Turn's fold: the single marker row standing where the work
    /// the fold hides happened. The entries behind it are absent from the
    /// plan rather than held by this unit, because a Turn Fold hides entries
    /// that are not adjacent — a steer Message and the final agent Message
    /// stay outside a fold whose hidden work surrounds them.
    TurnFold(TurnMarker),
    /// A Prompt not yet delivered — one this client sent that the Session has
    /// not echoed back, or one admitted to begin a Turn its Provider has yet
    /// to start — presenting its Attachments, and whoever sent it on the
    /// user's behalf, as its Message will.
    Provisional(&'a PendingPrompt, AttachmentRows),
}

impl RenderUnit<'_> {
    fn key(&self) -> UnitKey {
        match self {
            Self::Message(message, ..) => match &message.author {
                Some(Author::Sidekick { session_id, .. }) => UnitKey::SidekickMessage {
                    message: message.id,
                    sidekick: *session_id,
                },
                None => UnitKey::Message(message.id),
            },
            Self::Activity(activity) | Self::GroupMember(activity) => {
                Self::activity_unit_key(activity)
            }
            Self::Group { members, .. } => UnitKey::Group(members[0].id()),
            Self::TurnMember(unit) => unit.key(),
            Self::TurnFold(marker) => UnitKey::TurnFold(marker.turn_id),
            Self::Provisional(pending, _) => match &pending.author {
                Some(Author::Sidekick { session_id, .. }) => UnitKey::SidekickPrompt {
                    prompt: pending.prompt.id,
                    sidekick: *session_id,
                },
                None => UnitKey::Provisional(pending.prompt.id),
            },
        }
    }

    fn activity_unit_key(activity: &Activity) -> UnitKey {
        match activity {
            Activity::Subagent { id, session_id, .. } => UnitKey::Subagent {
                row: *id,
                session_id: *session_id,
            },
            Activity::Subsession { id, session_id, .. } => UnitKey::Subsession {
                row: *id,
                session_id: *session_id,
            },
            _ => UnitKey::Activity(activity.id()),
        }
    }

    /// Identifies everything the unit's rendering reads, so it re-renders when
    /// any entry it holds changes and reuses its lines when none did.
    fn fingerprint(&self, folds: &TranscriptFolds) -> u64 {
        match self {
            Self::Message(message, _, attachments) => message_fingerprint(message, attachments),
            Self::Activity(activity) => {
                activity_fingerprint(activity, resolved_fold_step(folds, activity))
            }
            Self::Group {
                kind,
                members,
                expanded,
            } => group_fingerprint(*kind, members, *expanded),
            // Member-ness joins the content signals: the same Activity keeps
            // its key when it moves between standalone and member rendering,
            // and only the fingerprint stops a cached standalone render from
            // surviving the move.
            Self::GroupMember(activity) => {
                let mut hasher = std::hash::DefaultHasher::new();
                activity_fingerprint(activity, resolved_fold_step(folds, activity))
                    .hash(&mut hasher);
                true.hash(&mut hasher);
                hasher.finish()
            }
            // Turn-member-ness joins the inner unit's own signals, so a
            // cached standalone render never survives the move into the
            // member gutter, nor the move back out.
            Self::TurnMember(unit) => {
                let mut hasher = std::hash::DefaultHasher::new();
                unit.fingerprint(folds).hash(&mut hasher);
                true.hash(&mut hasher);
                hasher.finish()
            }
            // A Turn Fold's marker says how the Turn settled, how long it
            // took, and whether it is folded; its fingerprint names each input
            // so a newer Session revision cannot reuse stale lines.
            Self::TurnFold(marker) => {
                let mut hasher = std::hash::DefaultHasher::new();
                (marker.outcome as u64).hash(&mut hasher);
                marker.duration_ms.hash(&mut hasher);
                marker.folded.hash(&mut hasher);
                hasher.finish()
            }
            // A provisional prompt's text only grows and carries no Fold, so
            // its length, its Attachments' presentation, and who sent it are
            // the whole of its rendering input.
            Self::Provisional(pending, attachments) => {
                let mut hasher = std::hash::DefaultHasher::new();
                pending.prompt.text.len().hash(&mut hasher);
                attachments.hash(&mut hasher);
                pending.author.hash(&mut hasher);
                hasher.finish()
            }
        }
    }

    /// The unit-local lines carrying a Spinner in their Marker cells, so the
    /// draw-time overlay (ADR 0009) knows where to patch the current frame.
    /// Most Active Activities animate their first line; a File Change repeats
    /// its Marker on every visible file row.
    fn spinner_lines(&self, folds: &TranscriptFolds) -> Vec<usize> {
        match self {
            Self::Activity(activity) | Self::GroupMember(activity) => {
                activity_spinner_lines(activity, folds)
            }
            Self::Group { kind, members, .. } => {
                kind.header_spinner_line(members).into_iter().collect()
            }
            Self::TurnMember(unit) => unit.spinner_lines(folds),
            Self::Message(..) | Self::TurnFold(_) | Self::Provisional(..) => Vec::new(),
        }
    }

    /// The Message a scroll anchor holds onto when it lands on this unit.
    /// Anchoring tracks conversation, so only a unit holding one answers.
    fn message_id(&self) -> Option<MessageId> {
        match self {
            Self::Message(message, ..) => Some(message.id),
            Self::TurnMember(unit) => unit.message_id(),
            Self::Activity(_)
            | Self::Group { .. }
            | Self::GroupMember(_)
            | Self::TurnFold(_)
            | Self::Provisional(..) => None,
        }
    }

    fn spacing_kind(&self) -> SpacingKind {
        match self {
            Self::TurnMember(unit) => unit.spacing_kind(),
            Self::Message(..) | Self::Provisional(..) => SpacingKind::Message,
            Self::Activity(activity) | Self::GroupMember(activity) => match activity {
                Activity::Error { .. } => SpacingKind::Error,
                _ => SpacingKind::Activity,
            },
            Self::Group { .. } | Self::TurnFold(_) => SpacingKind::Activity,
        }
    }

    /// Whether this unit is the first member of the expanded Group headed by
    /// `previous`, the one seam the separator rule never opens. A Group an
    /// expanded Turn Fold disclosed keeps that seam, so both sides unwrap
    /// their member gutter before the question is asked.
    fn heads_the_group(&self, previous: &Self) -> bool {
        if let (Self::TurnMember(unit), Self::TurnMember(previous)) = (self, previous) {
            return unit.heads_the_group(previous);
        }
        matches!(self, Self::GroupMember(_)) && matches!(previous, Self::Group { .. })
    }

    /// Projects the unit's lines, reporting the anchor a click acts on when
    /// the unit has one.
    fn render(
        &self,
        projection: &mut ActivityProjection<'_>,
        folds: &TranscriptFolds,
        theme: &Theme,
        width: u16,
        hyperlinks: bool,
        workspace: &Path,
    ) -> Option<UnitAnchor> {
        match self {
            Self::Message(message, parent, attachments) => {
                let strip = render_message(
                    projection.lines,
                    message,
                    *parent,
                    attachments,
                    theme,
                    width,
                    hyperlinks,
                );
                project_strip(projection, strip, attachments, width);
                // A Message someone sent on the user's behalf opens with the
                // heading naming its author, which is the way to them; it
                // hides nothing a press could open.
                message
                    .author
                    .is_some()
                    .then(|| UnitAnchor::binary(1, false))
            }
            Self::Activity(activity) => render_activity(
                projection,
                activity,
                resolved_fold_step(folds, activity),
                theme,
                width,
                hyperlinks,
                workspace,
            ),
            Self::Group {
                kind,
                members,
                expanded,
            } => Some(render_group(
                projection.lines,
                *kind,
                members,
                *expanded,
                theme,
                width,
                hyperlinks,
            )),
            Self::GroupMember(activity) => {
                let start = projection.lines.len();
                let anchor = render_activity(
                    projection,
                    activity,
                    resolved_fold_step(folds, activity),
                    theme,
                    width.saturating_sub(MEMBER_INDENT.len() as u16),
                    hyperlinks,
                    workspace,
                );
                indent_members(&mut projection.lines[start..]);
                anchor
            }
            Self::TurnMember(unit) => {
                let start = projection.lines.len();
                let strips = projection.strips.len();
                let anchor = unit.render(
                    projection,
                    folds,
                    theme,
                    width.saturating_sub(MEMBER_INDENT.len() as u16),
                    hyperlinks,
                    workspace,
                );
                indent_members(&mut projection.lines[start..]);
                for strip in &mut projection.strips[strips..] {
                    strip.left = strip.left.saturating_add(MEMBER_INDENT.len() as u16);
                }
                anchor
            }
            Self::TurnFold(marker) => Some(render_turn_fold(projection.lines, *marker, theme)),
            Self::Provisional(pending, attachments) => match &pending.author {
                None => {
                    let strip = push_user_message(
                        projection.lines,
                        &pending.prompt.text,
                        &TextBindings::from_prompt(&pending.prompt),
                        attachments,
                        theme,
                        width,
                    );
                    project_strip(projection, strip, attachments, width);
                    None
                }
                // Drawn as the Sidekick's Message will be, so its words are
                // never the user's while they wait to be delivered.
                Some(author) => {
                    push_authored_words(
                        projection.lines,
                        &pending.prompt.text,
                        false,
                        author,
                        theme,
                        width,
                    );
                    Some(UnitAnchor::binary(1, false))
                }
            },
        }
    }
}

/// Records where a user Message's strip stands among the lines it projected,
/// so the frame can find its reserved rows: past the gutter, and short of the
/// air at the block's right edge.
fn project_strip(
    projection: &mut ActivityProjection<'_>,
    line: Option<usize>,
    attachments: &AttachmentRows,
    width: u16,
) {
    let (Some(line), AttachmentRows::Strip(strip)) = (line, attachments) else {
        return;
    };
    let gutter = USER_MESSAGE_GUTTER.width() as u16;
    projection.strips.push(ProjectedStrip {
        start: line,
        left: gutter,
        width: width.saturating_sub(gutter + USER_MESSAGE_RIGHT_MARGIN as u16),
        strip: strip.clone(),
    });
}

/// Seats a member's lines in the gutter beneath the header they fold into.
fn indent_members(lines: &mut [StyledLine]) {
    for line in lines {
        line.spans
            .insert(0, StyledSpan::chrome(MEMBER_INDENT, Style::default()));
    }
}

fn activity_spinner_lines(activity: &Activity, folds: &TranscriptFolds) -> Vec<usize> {
    if activity.status() != Some(crate::protocol::ActivityStatus::Active) {
        return Vec::new();
    }
    match activity {
        Activity::FileChange { changes, .. } => {
            let listed = if resolved_fold_step(folds, activity) == FoldStep::Expanded {
                changes.len()
            } else {
                changes.len().min(FOLDED_FILE_CHANGE_PATHS)
            };
            (0..listed).collect()
        }
        _ => vec![0],
    }
}

/// Which of the Transcript's registers a unit speaks in, which is all the
/// separator rule needs to know about it. See [`Spacing::separates`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpacingKind {
    /// A Message, or a Prompt this client has not seen echoed back yet: a turn
    /// of the conversation, which always takes air around it.
    Message,
    /// An Activity, a Group, or a Group member: progress and operational
    /// detail, which packs into a tight list until one runs past a single
    /// line, which takes air after it rather than around it.
    Activity,
    /// An Error Activity, which takes air whatever its size, because a failure
    /// is the one thing a reader must never have to hunt for.
    Error,
}

/// What the separator rule reads off one unit: the register it speaks in, and
/// how much of the Transcript it occupies.
#[derive(Clone, Copy, Debug)]
struct Spacing {
    kind: SpacingKind,
    /// The unit's own projected lines, counted before layout wrapped any of
    /// them, so resizing the terminal reflows the text without ever changing
    /// the Transcript's vertical rhythm.
    lines: usize,
}

impl Spacing {
    /// Whether a blank row separates this unit from the one after it. The
    /// answer comes from the pair alone: whitespace at a boundary is a
    /// property of the boundary, not of either entry, which is why no renderer
    /// decides it. A run of Activities stays a tight list until one of them
    /// runs past a single line; every other boundary takes one blank row.
    ///
    /// Between two Activities, only the earlier one's size opens the boundary,
    /// which is why the later one is read for its register alone. Air below an
    /// entry marks where its revealed content stopped, which is what a reader
    /// needs; air above it would only announce content they are already
    /// looking at — and it would announce it by pushing the row down at the
    /// moment they opened that entry's Fold, moving the very row they clicked.
    /// So an entry that runs past a single line takes air after it and stays
    /// tight to whatever compact entry it follows.
    const fn separates(self, next: SpacingKind) -> bool {
        match (self.kind, next) {
            (SpacingKind::Activity, SpacingKind::Activity) => self.lines > 1,
            _ => true,
        }
    }
}

/// How a Turn settled, which is all a Turn Fold's marker says about it. Turn
/// Folds only ever stand for settled Turns, so the Active status has no
/// spelling here: an Active Turn cannot reach a marker because it cannot
/// become one of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettledTurn {
    Completed,
    Interrupted,
    Failed,
}

impl SettledTurn {
    /// How a Turn settled, or `None` while it is still Active.
    const fn of(status: TurnStatus) -> Option<Self> {
        match status {
            TurnStatus::Active => None,
            TurnStatus::Completed => Some(Self::Completed),
            TurnStatus::Interrupted => Some(Self::Interrupted),
            TurnStatus::Failed => Some(Self::Failed),
        }
    }

    /// How the marker words this outcome: the word it states the outcome in,
    /// and the preposition that word takes when a duration follows it. The
    /// two live in one table because they are one phrase — a Turn that ran to
    /// completion worked *for* the span it took, while one that was cut short
    /// stopped or failed *after* it, the span being the run-up to an ending
    /// rather than the shape of the work.
    ///
    /// The wording is status-true and permanent rather than recency-scoped,
    /// the way a Transcript says Thought for settled Reasoning, so a reader
    /// scrolling back through a session reads how each Turn ended however long
    /// ago it did.
    const fn phrasing(self) -> (&'static str, &'static str) {
        match self {
            Self::Completed => ("Worked", "for"),
            Self::Interrupted => ("Stopped", "after"),
            Self::Failed => ("Failed", "after"),
        }
    }
}

/// The marker one settled Turn's fold stands as: which Turn it speaks for, how
/// that Turn settled, how long it took to get there, and whether its fold is
/// closed as drawn. The marker outlives the fold closing, because it is the row
/// the reader clicks in both directions — folded it stands for the work, and
/// expanded it heads the work it opened onto.
#[derive(Clone, Copy, Debug)]
struct TurnMarker {
    turn_id: TurnId,
    outcome: SettledTurn,
    /// How long the Turn ran, or `None` when it is missing either of its
    /// timestamps — which is every Turn stored before Suru recorded them.
    duration_ms: Option<u64>,
    folded: bool,
}

impl TurnMarker {
    /// The marker a Turn would stand as, or `None` while it is still Active:
    /// a marker only ever speaks for a Turn that has settled.
    fn of(turn: &Turn, folded: bool) -> Option<Self> {
        SettledTurn::of(turn.status).map(|outcome| Self {
            turn_id: turn.id,
            outcome,
            duration_ms: turn_duration_ms(turn),
            folded,
        })
    }

    /// What the marker reads, which is the outcome word alone when the Turn
    /// carries no duration: a Turn stored before Suru recorded Turn timing
    /// still says how it ended, just not how long it took.
    fn label(self) -> String {
        let (word, preposition) = self.outcome.phrasing();
        self.duration_ms.map_or_else(
            || word.to_owned(),
            |duration_ms| format!("{word} {preposition} {}", humanized_duration(duration_ms)),
        )
    }
}

/// How long a Turn ran: the span between the commit that delivered its opening
/// Prompt and the commit that settled it. A Turn missing either timestamp has
/// no span to report rather than a zero-length one, and neither does one whose
/// settle somehow stamped before its start — a marker saying nothing about how
/// long a Turn took is honest, where one saying `0ms` is not.
fn turn_duration_ms(turn: &Turn) -> Option<u64> {
    let started_at = turn.started_at?;
    let settled_at = turn.settled_at?;
    settled_at.0.checked_sub(started_at.0)
}

/// Which of the roles a Turn Fold keeps outside it a Transcript entry plays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnEntryRole {
    /// A Message the user authored: the Turn's opening Prompt or a steer.
    /// Every one of them stays outside the fold, because a fold that hid what
    /// the reader asked for would hide the question its marker answers.
    UserMessage,
    /// A Delegation: what the delegating Agent asked of a Subagent, which
    /// opens the Turn its spawn or resume began. It stays outside the fold on
    /// the same terms as a user Message, because it is the question that
    /// Turn's marker answers.
    Delegation,
    /// A Message the agent authored. Only the Turn's last one survives its
    /// fold: that is the answer the Turn arrived at, and the ones before it
    /// are work.
    AgentMessage,
    /// An Error Activity. The last one a failed Turn recorded is its
    /// outcome and stays outside the fold; the ones it worked past are work.
    Error,
    /// A Compaction: where the Agent's memory of the Session changed. Every
    /// one stays outside the fold, because a memory boundary hidden with the
    /// work would leave the reader no way to see why the Agent later forgot.
    Compaction,
    /// A Subsession's row: a Session the Turn's Sidekick began, which is the
    /// user's own work from then on rather than the Turn's, so it stays
    /// outside the fold as the way into that Session from where it began.
    Subsession,
    /// Everything else, which is the work a Turn Fold hides.
    Work,
}

/// What a Turn Fold needs to know about one Transcript entry.
#[derive(Clone, Copy, Debug)]
struct TurnEntry {
    turn_id: TurnId,
    role: TurnEntryRole,
}

/// A Session's Messages and Activities by id, so the walks over its Transcript
/// resolve what each entry names in one lookup — and, in the same lookup,
/// whether the Transcript shows anything for it at all. Gathered once and
/// shared, because the projection walks the Transcript twice: once to decide
/// the Turn Folds, once to plan the units, and the two must agree on which
/// entries are there to be read.
struct TranscriptContent<'a> {
    messages: HashMap<MessageId, &'a Message>,
    activities: HashMap<ActivityId, &'a Activity>,
    /// Which kinds of Activity this reader is shown, which the lookup answers
    /// alongside every other reason an entry draws nothing.
    visibility: ActivityVisibility,
}

impl<'a> TranscriptContent<'a> {
    fn of(snapshot: &'a SessionSnapshot, visibility: ActivityVisibility) -> Self {
        Self {
            visibility,
            messages: snapshot
                .messages
                .iter()
                .map(|message| (message.id, message))
                .collect(),
            activities: snapshot
                .activities
                .iter()
                .map(|activity| (activity.id(), activity))
                .collect(),
        }
    }

    /// What a Transcript entry names, or `None` when the Transcript shows
    /// nothing for it — because the snapshot does not carry what it names, or
    /// because what it names projects no rows at all. Both walks over the
    /// Transcript ask the same question of an entry, which is whether it is
    /// one of the entries a reader sees, so both are answered here.
    fn entry(&self, item: &TranscriptItem) -> Option<TranscriptEntry<'a>> {
        match item {
            TranscriptItem::Message { message_id } => self
                .messages
                .get(message_id)
                .copied()
                .map(TranscriptEntry::Message),
            TranscriptItem::Activity { activity_id } => self
                .activities
                .get(activity_id)
                .copied()
                .filter(|activity| !transcript_shows_nothing(activity, self.visibility))
                .map(TranscriptEntry::Activity),
        }
    }
}

/// Whether the Transcript shows nothing at all for an Activity. A reader who
/// has not asked for Reasoning — which is every reader until one does, since
/// Suru hides it by default — is shown none of it, however fully the Provider
/// described it: the Setting decides the whole kind at once, where the rules
/// below decide one block at a time. Either way the Activity stays stored and
/// is simply absent from the projection, so asking for Reasoning brings back
/// every block that arrived while it was hidden — and a settled Turn whose only
/// work was Reasoning has no Turn Fold marker while it is, because a fold with
/// nothing left to disclose stands for nothing. A Turn that did anything else
/// keeps its marker, and the duration that marker reports.
///
/// A reader who hid Tool Calls — which Suru shows by default — is shown none of
/// them on exactly those terms: every one stays stored and keeps arriving, none
/// joins or ends the run around it or leaves a gap, and a settled Turn whose
/// only work was hidden Tool Calls has no marker while they are.
///
/// A Reasoning block that settles without the Provider ever describing it — no
/// heading and no content — has nothing a row could carry, so the Transcript
/// draws none: not a folded row, not an expanded one, and not the duration it
/// spent arriving at nothing. The Activity stays stored exactly as it arrived;
/// it is simply absent from the projection rather than empty within it, which
/// is what makes it neither join nor end the run around it and leave no gap in
/// the Transcript's spacing.
///
/// The rule reads Settle rather than success, so a block the Provider failed
/// or a Turn interrupted before either arrived is as invisible as one that
/// completed empty: how the thinking ended is the Turn's story to tell, and
/// its marker tells it. A block still running is never this, even before its
/// first delta — that an agent is thinking is itself progress worth a row —
/// and neither is one whose content the cap cut away, because content the
/// reader cannot see still happened and its truncation marker says so.
///
/// A File Change with no paths is absent on the same terms: there is no file
/// row to draw while it streams and no work for a settled Turn Fold to stand
/// for if it never receives one.
fn transcript_shows_nothing(activity: &Activity, visibility: ActivityVisibility) -> bool {
    match activity {
        Activity::FileChange { changes, .. } => changes.is_empty(),
        Activity::ToolCall { .. } => visibility.tool_calls == ToolCallVisibility::Hidden,
        Activity::Reasoning { .. } if visibility.reasoning == ReasoningVisibility::Hidden => true,
        Activity::Reasoning {
            status,
            title,
            content,
            content_truncated,
            ..
        } => {
            *status != crate::protocol::ActivityStatus::Active
                && reasoning_heading(title.as_deref()).is_none()
                && content.trim().is_empty()
                && !content_truncated
        }
        _ => false,
    }
}

/// The heading a Reasoning block leads with, which is a title that says
/// something: a Provider that sent only blank space named the block no better
/// than one that sent no title at all. Both readings of a title pass through
/// here — the header that would otherwise print an empty heading, and the rule
/// deciding whether the block is shown at all — so the two cannot disagree
/// about which blocks the Provider described.
fn reasoning_heading(title: Option<&str>) -> Option<&str> {
    title.filter(|title| !title.trim().is_empty())
}

/// One Transcript entry resolved to the content it names.
#[derive(Clone, Copy, Debug)]
enum TranscriptEntry<'a> {
    Message(&'a Message),
    Activity(&'a Activity),
}

impl TranscriptEntry<'_> {
    /// What a Turn Fold reads off the entry: whose Turn it belongs to, and
    /// which of the roles a fold keeps outside it the entry plays.
    fn in_turn(self) -> TurnEntry {
        match self {
            Self::Message(message) => TurnEntry {
                turn_id: message.turn_id,
                role: match message.role {
                    MessageRole::User => TurnEntryRole::UserMessage,
                    MessageRole::Agent => TurnEntryRole::AgentMessage,
                    MessageRole::Delegation(_) => TurnEntryRole::Delegation,
                },
            },
            Self::Activity(activity) => TurnEntry {
                turn_id: activity.turn_id(),
                role: match activity {
                    Activity::Error { .. } => TurnEntryRole::Error,
                    Activity::Compaction { .. } => TurnEntryRole::Compaction,
                    Activity::Subsession { .. } => TurnEntryRole::Subsession,
                    _ => TurnEntryRole::Work,
                },
            },
        }
    }
}

/// What each settled Turn's fold covers, decided in one pass over the
/// Transcript so the unit walk only has to ask by position. A Turn Fold keys on
/// the Turn's Settle rather than on entry adjacency, so it is answered here
/// rather than inside the walk that gathers adjacent entries into Groups.
#[derive(Debug, Default)]
struct TurnFolding {
    /// Whether the entry at each Transcript position is hidden by its Turn's
    /// fold, which only a closed fold does.
    hidden: Vec<bool>,
    /// Whether the entry at each Transcript position is covered by a fold the
    /// reader expanded: work the marker stands for, shown because they asked.
    /// A disclosed entry renders in the member gutter, so the disclosure
    /// reads as subordinate to the marker it folds back into.
    disclosed: Vec<bool>,
    /// The Turn whose marker stands at a Transcript position, which is the
    /// position of the first entry that Turn's fold covers, or of its final
    /// agent Message when every covered entry trails that — so the marker
    /// stands where the hidden work happened without ever falling past the
    /// answer. A marker stands in the same place open or closed, so expanding
    /// a Turn never moves the row the reader clicked. A Turn whose fold covers
    /// nothing has no entry here, which is how a marker that would disclose
    /// nothing is suppressed.
    markers: HashMap<usize, TurnMarker>,
}

impl TurnFolding {
    /// Decides every settled Turn's fold from the Transcript and the client's
    /// axis. Every settled Turn takes part, because a Turn the reader expanded
    /// still shows the marker it folds back from; only an Active Turn has no
    /// fold at all. Which of them hide their work is the axis's answer alone.
    fn plan(
        snapshot: &SessionSnapshot,
        content: &TranscriptContent<'_>,
        axis: &TranscriptTurnFolds,
    ) -> Self {
        let settled: HashMap<TurnId, TurnMarker> = snapshot
            .turns
            .iter()
            .filter_map(|turn| {
                TurnMarker::of(turn, axis.is_folded(turn.id)).map(|marker| (turn.id, marker))
            })
            .collect();
        if settled.is_empty() {
            return Self::default();
        }
        let mut entries: Vec<Option<TurnEntry>> = Vec::with_capacity(snapshot.transcript.len());
        let mut positions: HashMap<TurnId, Vec<usize>> = HashMap::new();
        for (position, item) in snapshot.transcript.iter().enumerate() {
            // An entry the Transcript shows nothing for is not one of the
            // entries a fold decides about: a fold covering only those would
            // disclose nothing, so no marker stands for it.
            let entry = content.entry(item).map(TranscriptEntry::in_turn);
            if let Some(entry) = entry
                && settled.contains_key(&entry.turn_id)
            {
                positions.entry(entry.turn_id).or_default().push(position);
            }
            entries.push(entry);
        }
        let mut hidden = vec![false; snapshot.transcript.len()];
        let mut disclosed = vec![false; snapshot.transcript.len()];
        let mut markers = HashMap::new();
        for (turn_id, positions) in positions {
            let turn_marker = settled[&turn_id];
            let role_at = |position: usize| entries[position].map(|entry| entry.role);
            let final_agent_message = positions
                .iter()
                .copied()
                .rev()
                .find(|position| role_at(*position) == Some(TurnEntryRole::AgentMessage));
            // A failed Turn's outcome is the last Error it recorded: the ones
            // it worked past are work, but the one it ended on stays outside
            // the fold even when a closing Message trails it, because a
            // failure is the one thing a reader must never have to hunt for.
            let terminal_error = (turn_marker.outcome == SettledTurn::Failed)
                .then(|| {
                    positions
                        .iter()
                        .copied()
                        .rev()
                        .find(|position| role_at(*position) == Some(TurnEntryRole::Error))
                })
                .flatten();
            let mut marker = None;
            for position in positions {
                if matches!(
                    role_at(position),
                    Some(
                        TurnEntryRole::UserMessage
                            | TurnEntryRole::Delegation
                            | TurnEntryRole::Compaction
                            | TurnEntryRole::Subsession
                    )
                ) || Some(position) == final_agent_message
                    || Some(position) == terminal_error
                {
                    continue;
                }
                // What the fold covers is the same whichever way the reader
                // left it; only a closed one hides what it covers, and only
                // an open one discloses it.
                hidden[position] = turn_marker.folded;
                disclosed[position] = !turn_marker.folded;
                marker.get_or_insert(position);
            }
            if let Some(position) = marker {
                // The marker stands where the hidden work happened, but never
                // past the answer that work led to: a Turn whose only hidden
                // entries trailed its final agent Message still marks them
                // above it, so the Transcript always reads question, marker,
                // answer.
                let position = final_agent_message.map_or(position, |answer| position.min(answer));
                markers.insert(position, turn_marker);
            }
        }
        Self {
            hidden,
            disclosed,
            markers,
        }
    }

    fn hides(&self, position: usize) -> bool {
        self.hidden.get(position).copied().unwrap_or(false)
    }

    fn discloses(&self, position: usize) -> bool {
        self.disclosed.get(position).copied().unwrap_or(false)
    }

    fn marker_at(&self, position: usize) -> Option<TurnMarker> {
        self.markers.get(&position).copied()
    }
}

/// A kind of Activity a Transcript gathers runs of into a Group. Everything a
/// Group's presentation depends on hangs off its kind — which Activities join
/// a run, what the marker reads, and what expanding it opens onto — so adding
/// a kind is a matter of answering those three questions rather than of
/// finding every place that quietly assumed another.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum GroupableKind {
    /// Commands, every one of which belongs to its run's Group from the moment
    /// it starts, however it settles.
    Command,
    /// Tool Calls, gathered on exactly the command rules but never into a
    /// command's Group.
    ToolCall,
    /// Reasoning blocks, which join a run from the moment they start too; a
    /// Group of one reads exactly as a lone block does.
    Reasoning,
}

impl GroupableKind {
    /// The kind of run an Activity extends, or `None` when it groups with
    /// nothing and so ends whatever run it follows. Membership reads the kind
    /// alone, never the status: an Activity joins its run as it starts and
    /// stays there however it settles — failed and interrupted included — so
    /// a Group stands in one place for its whole run and the Transcript's
    /// shape holds still while a Turn works. A Tool Call extends a run of its
    /// own kind, so a run of Tool Calls and a run of commands never share a
    /// Group. An entry the reader sees nothing for never reaches here at all,
    /// because the walk resolves it to no entry.
    const fn joined_by(activity: &Activity) -> Option<Self> {
        match activity {
            Activity::Command { .. } => Some(Self::Command),
            Activity::ToolCall { .. } => Some(Self::ToolCall),
            Activity::Reasoning { .. } => Some(Self::Reasoning),
            _ => None,
        }
    }

    /// The line of a Group's header carrying a Spinner in its Marker cell, or
    /// `None` when the Group stands for no work in progress. A command or Tool
    /// Call Group spins while any member runs, since its header is the only
    /// row a collapsed Group draws; a Reasoning Group does whenever its latest
    /// member is still thinking. Either way the Marker leads the header's
    /// first line.
    fn header_spinner_line(self, members: &[&Activity]) -> Option<usize> {
        let live = match self {
            Self::Command | Self::ToolCall => running_member(members).is_some(),
            Self::Reasoning => reasoning_group_is_live(members),
        };
        live.then_some(0)
    }

    /// What a Group's header reads off one member, and so what its rendering
    /// must re-key on when the member changes (ADR 0007). A command or Tool
    /// Call Group's header counts its members by how they settled and names
    /// the one still running, so which Activities they are, their status, and
    /// that summary are the whole of it; a kind whose header speaks for its
    /// members' content keys on that content here instead.
    fn member_fingerprint(self, member: &Activity) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        match self {
            Self::Command | Self::ToolCall => {
                member.id().hash(&mut hasher);
                member.status().map(|status| status as u8).hash(&mut hasher);
                member_summary(member).hash(&mut hasher);
            }
            // A Reasoning Group's header leads with the latest member's title
            // and sums every member's duration, its wording turns on whether
            // that member is still thinking, and its expansion draws each
            // member's whole prose, so all of what a member shows is header
            // input.
            Self::Reasoning => {
                if let Some(reasoning) = ReasoningActivity::of(member) {
                    (reasoning.status as u8).hash(&mut hasher);
                    reasoning.title.hash(&mut hasher);
                    reasoning.content.len().hash(&mut hasher);
                    reasoning.content_truncated.hash(&mut hasher);
                    reasoning.duration_ms.hash(&mut hasher);
                }
            }
        }
        hasher.finish()
    }

    /// The units an expanded Group plans after its header. A command or Tool
    /// Call Group opens onto its members, each as its own unit, so a member's
    /// Fold, its cached lines, and its click target need no Group-specific
    /// machinery. A kind whose expansion is the header's own content plans
    /// nothing here and renders it in [`render_group`] instead.
    fn expansion_units<'a>(self, members: &[&'a Activity]) -> Vec<RenderUnit<'a>> {
        match self {
            Self::Command | Self::ToolCall => members
                .iter()
                .copied()
                .map(RenderUnit::GroupMember)
                .collect(),
            // A Reasoning Group opens straight onto its members' prose with no
            // per-member fold stage to keep state for, so the whole expansion
            // is the Group unit's own content.
            Self::Reasoning => Vec::new(),
        }
    }
}

/// The run of adjacent Activities a walk is gathering: the groupable kind they
/// share and the members so far. A run only ever holds one kind, because an
/// Activity of another kind ends it before starting its own.
struct GroupRun<'a> {
    kind: GroupableKind,
    /// Whether an expanded Turn Fold disclosed the run's members, which the
    /// whole run shares: a run never spans a fold boundary, so its Group and
    /// every unit it opens onto sit in one gutter.
    disclosed: bool,
    members: Vec<&'a Activity>,
}

/// Walks a Session's transcript into the units a view renders. Which entries
/// share a unit is decided here and nowhere else, so gathering a run of them
/// into one stays a change to this walk rather than to rendering or layout.
fn plan_units<'a>(
    snapshot: &'a SessionSnapshot,
    provisional: &[&'a PendingPrompt],
    disclosure: TranscriptDisclosure<'_>,
    previews: &AttachmentPreviews,
) -> Vec<RenderUnit<'a>> {
    let groups = disclosure.groups;
    let content = TranscriptContent::of(snapshot, disclosure.visibility);
    let folding = TurnFolding::plan(snapshot, &content, disclosure.turns);
    let mut units = Vec::with_capacity(snapshot.transcript.len() + provisional.len());
    let mut run: Option<GroupRun<'a>> = None;
    // A transcript entry the Transcript shows nothing for projects nothing
    // rather than a gap, so it does not end a run either: the entries around
    // it are still adjacent as presented. An entry a Turn Fold hides projects
    // nothing for the same reason and is read the same way.
    for (position, item) in snapshot.transcript.iter().enumerate() {
        if let Some(marker) = folding.marker_at(position) {
            close_run(&mut units, &mut run, groups);
            units.push(RenderUnit::TurnFold(marker));
        }
        if folding.hides(position) {
            continue;
        }
        let Some(entry) = content.entry(item) else {
            continue;
        };
        let disclosed = folding.discloses(position);
        // A run never crosses a fold boundary: an Activity inside the
        // disclosure and one outside it sit in different gutters, so they
        // never share a Group row.
        if run.as_ref().is_some_and(|open| open.disclosed != disclosed) {
            close_run(&mut units, &mut run, groups);
        }
        match entry {
            TranscriptEntry::Message(message) => {
                close_run(&mut units, &mut run, groups);
                let attachments = attachment_rows(
                    &TextBindings::from_message(message),
                    &snapshot.attachments,
                    previews,
                );
                units.push(in_turn_gutter(
                    RenderUnit::Message(message, snapshot.session.parent, attachments),
                    disclosed,
                ));
            }
            TranscriptEntry::Activity(activity) => match GroupableKind::joined_by(activity)
                .filter(|_| disclosure.grouping == Grouping::Formed)
            {
                Some(kind) => {
                    if run.as_ref().is_some_and(|open| open.kind != kind) {
                        close_run(&mut units, &mut run, groups);
                    }
                    run.get_or_insert(GroupRun {
                        kind,
                        disclosed,
                        members: Vec::new(),
                    })
                    .members
                    .push(activity);
                }
                None => {
                    close_run(&mut units, &mut run, groups);
                    units.push(in_turn_gutter(RenderUnit::Activity(activity), disclosed));
                }
            },
        }
    }
    close_run(&mut units, &mut run, groups);
    units.extend(provisional.iter().map(|pending| {
        let attachments = attachment_rows(
            &TextBindings::from_prompt(&pending.prompt),
            &snapshot.attachments,
            previews,
        );
        RenderUnit::Provisional(pending, attachments)
    }));
    units
}

/// Ends the run in progress, which becomes one Group however few members it
/// holds: collapsed it is its header alone — every member, running or not,
/// hidden behind it — and expanded it is its header followed by whatever its
/// kind opens onto.
fn close_run<'a>(
    units: &mut Vec<RenderUnit<'a>>,
    run: &mut Option<GroupRun<'a>>,
    groups: &TranscriptGroups,
) {
    let Some(GroupRun {
        kind,
        disclosed,
        members,
    }) = run.take()
    else {
        return;
    };
    let mut closed = Vec::new();
    if groups.is_collapsed(members[0].id()) {
        closed.push(RenderUnit::Group {
            kind,
            members,
            expanded: false,
        });
    } else {
        let expansion = kind.expansion_units(&members);
        closed.push(RenderUnit::Group {
            kind,
            members,
            expanded: true,
        });
        closed.extend(expansion);
    }
    units.extend(
        closed
            .into_iter()
            .map(|unit| in_turn_gutter(unit, disclosed)),
    );
}

/// The Activities a reader can see stream in a Transcript laid out under
/// `disclosure`: every Active command or Tool Call drawn as a row of its own,
/// standalone or as a member of an expanded Group, rather than hidden behind a
/// collapsed Group's header or a folded Turn. Only these may grow into their
/// live tail on their own, because growing a row nobody can see changes
/// nothing but what the reader finds when they open it.
pub(super) fn visible_live_outputs(
    snapshot: &SessionSnapshot,
    disclosure: TranscriptDisclosure<'_>,
) -> HashSet<ActivityId> {
    fn drawn(unit: &RenderUnit<'_>) -> Option<ActivityId> {
        match unit {
            RenderUnit::Activity(activity) | RenderUnit::GroupMember(activity) => {
                streams_live_output(activity).then(|| activity.id())
            }
            RenderUnit::TurnMember(unit) => drawn(unit),
            _ => None,
        }
    }
    plan_units(snapshot, &[], disclosure, &AttachmentPreviews::default())
        .iter()
        .filter_map(drawn)
        .collect()
}

/// Seats a unit in the member gutter when an expanded Turn Fold disclosed it,
/// and leaves it standalone otherwise, so every planning path speaks one rule.
fn in_turn_gutter(unit: RenderUnit<'_>, disclosed: bool) -> RenderUnit<'_> {
    if disclosed {
        RenderUnit::TurnMember(Box::new(unit))
    } else {
        unit
    }
}

/// A unit's identity across rebuilds, taken from the entries it holds so the
/// cache recognizes the same unit in the next frame, and what a click resolves
/// to, so the interaction layer learns what it landed on without re-deriving
/// the projection. A unit that grows must keep the key it had, or the cache
/// reads it as a new unit and re-renders what only gained a line.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum UnitKey {
    Message(MessageId),
    Activity(ActivityId),
    /// A Subagent's row, carrying the child Session it stands for: a press on
    /// it opens that Session rather than toggling a Fold, so the key carries
    /// what the invocation needs. The row itself identifies the unit, since
    /// every row of a resumed Subagent leads into the one Session.
    Subagent {
        row: ActivityId,
        session_id: SessionId,
    },
    /// The row recording that a Sidekick began a Subsession, carrying the
    /// Subsession: a press on it opens that Session, as a Subagent's row
    /// opens its child's.
    Subsession {
        row: ActivityId,
        session_id: SessionId,
    },
    /// A Message a Sidekick sent on the user's behalf, carrying the
    /// Sidekick's Session: a press on its heading opens that Session, so the
    /// key carries what the invocation needs, as a Subagent's row's does.
    SidekickMessage {
        message: MessageId,
        sidekick: SessionId,
    },
    /// A Prompt a Sidekick sent that is not yet delivered, carrying the
    /// Sidekick's Session as a delivered one's Message does.
    SidekickPrompt {
        prompt: PromptId,
        sidekick: SessionId,
    },
    /// A Group, identified by its first member: the anchor a run keeps as it
    /// absorbs the next Activity to join it, where a key over the member set
    /// would read the grown Group as a new unit and re-render it every time.
    Group(ActivityId),
    /// A Turn Fold, identified by the Turn it stands for: the marker keeps its
    /// key as the Turn's hidden work grows behind it.
    TurnFold(TurnId),
    Provisional(PromptId),
}

#[derive(Clone, Debug)]
struct UnitView {
    key: UnitKey,
    fingerprint: u64,
    /// The unit's projected lines, after layout split any oversized one.
    lines: Vec<StyledLine>,
    /// Every row those lines wrap to at the view width, in order: what a
    /// frame draws, and what a cell resolves back through to its line.
    rows: Vec<WrappedRow>,
    links: Vec<TranscriptLink>,
    /// Wrapped row count per line at the view width, so an anchor's row is a
    /// prefix sum.
    rows_per_line: Vec<usize>,
    /// Lines the unit projected before layout split any oversized one, which
    /// is what the separator rule measures compactness in.
    source_lines: usize,
    /// Whether a blank row is drawn above this unit. Decided per rebuild from
    /// this unit and the one before it, and deliberately outside the cache
    /// fingerprint: a boundary is not a property of either unit, so a unit
    /// reused unchanged can still take a separator it did not take last frame.
    leading_separator: bool,
    /// Index of the unit's first line across the view, counting its own
    /// separator as that first line so a lookup mid-unit lands by simple
    /// subtraction.
    start_line: usize,
    /// Index of the unit's first row across the view, counting its separator
    /// the same way.
    start_row: usize,
    /// The unit-local rows whose Marker cells carry Spinners, recorded so the
    /// draw-time overlay (ADR 0009) can patch the current frame without the
    /// projection ever depending on it.
    spinner_rows: Vec<usize>,
    /// The strips of thumbnails the unit reserved rows for, each by the
    /// unit-local row its reserved rows begin on.
    strips: Vec<ProjectedStrip>,
    message_id: Option<MessageId>,
    anchor: Option<LaidOutAnchor>,
}

/// One row of a unit: which of the unit's lines it wraps, and the row itself.
#[derive(Clone, Debug)]
struct WrappedRow {
    line: usize,
    row: StyledRow,
}

/// A unit's anchor once layout resolved it: how many of the unit's laid-out
/// lines form its header, whether it held content back, and where its staged
/// Fold stands.
#[derive(Clone, Copy, Debug)]
struct LaidOutAnchor {
    header_lines: usize,
    hides_content: bool,
    fold: FoldDisclosure,
    /// Index of the fold-marker line among the unit's laid-out lines.
    marker_line: Option<usize>,
}

// One parameter per input a rebuild reads, as `view_with_hyperlinks` has.
#[allow(clippy::too_many_arguments)]
fn rebuild(
    previous: Option<TranscriptView>,
    key: ViewKey,
    snapshot: &SessionSnapshot,
    provisional: &[&PendingPrompt],
    disclosure: TranscriptDisclosure<'_>,
    previews: &AttachmentPreviews,
    theme: &Theme,
    width: u16,
) -> TranscriptView {
    let mut reusable: HashMap<UnitKey, UnitView> = previous
        .filter(|view| {
            view.key.generation == key.generation
                && view.key.session_id == key.session_id
                && view.key.theme == key.theme
                && view.key.width == key.width
                && view.key.hyperlinks == key.hyperlinks
        })
        .map(|view| {
            view.units
                .into_iter()
                .map(|unit| (unit.key, unit))
                .collect()
        })
        .unwrap_or_default();
    let planned = plan_units(snapshot, provisional, disclosure, previews);
    let mut units = planned
        .iter()
        .map(|unit| {
            reuse_or_render(
                &mut reusable,
                unit,
                disclosure.folds,
                theme,
                width,
                key.hyperlinks,
                &snapshot.session.execution_directory.path,
            )
        })
        .collect::<Vec<_>>();
    assign_separators(&planned, &mut units);

    let mut row_count = 0;
    let mut line_count = 0;
    let mut message_starts = Vec::new();
    let mut unit_starts = Vec::new();
    for unit in &mut units {
        unit.start_line = line_count;
        unit.start_row = row_count;
        if unit.leading_separator {
            row_count += 1;
            line_count += 1;
        }
        // Every row offset a caller acts on — a click target, a scroll anchor —
        // points past the separator, so the blank row belongs to no unit's
        // extent and a click on it resolves to nothing.
        let unit_start_row = row_count;
        if let Some(message_id) = unit.message_id {
            message_starts.push(MessageStart {
                message_id,
                row: unit_start_row,
            });
        }
        row_count += unit.rows.len();
        line_count += unit.lines.len();
        if let Some(anchor) = unit.anchor {
            unit_starts.push(UnitStart {
                key: unit.key,
                row: unit_start_row,
                header_rows: unit.rows_per_line[..anchor.header_lines].iter().sum(),
                row_count: row_count - unit_start_row,
                hides_content: anchor.hides_content,
                fold: anchor.fold,
                marker_row: anchor
                    .marker_line
                    .map(|line| unit_start_row + unit.rows_per_line[..line].iter().sum::<usize>()),
            });
        }
    }
    TranscriptView {
        key,
        units,
        row_count,
        message_starts,
        unit_starts,
    }
}

/// Decides which units take a blank row above them. The walk runs over the
/// planned units because the rule reads their kinds, and over the rendered
/// ones because it reads their sizes; the two are the same sequence.
///
/// Two boundaries are exempt from [`Spacing::separates`]. A unit that projected
/// nothing is not a boundary at all, so it neither takes a separator nor
/// becomes the entry the next one is measured against — the walk carries that
/// one predecessor, so every question about what came before this unit gets the
/// same answer. And an expanded Group's header is
/// always tight to its first member: that seam is inside one construct rather
/// than between two entries, so air there would read as detaching a header
/// from the very thing it heads.
fn assign_separators<'a>(planned: &'a [RenderUnit<'_>], rendered: &mut [UnitView]) {
    let mut previous: Option<(&'a RenderUnit<'_>, Spacing)> = None;
    for (plan, unit) in planned.iter().zip(rendered.iter_mut()) {
        if unit.source_lines == 0 {
            unit.leading_separator = false;
            continue;
        }
        let spacing = Spacing {
            kind: plan.spacing_kind(),
            lines: unit.source_lines,
        };
        unit.leading_separator = match previous {
            Some((previous_plan, previous_spacing)) => {
                !plan.heads_the_group(previous_plan) && previous_spacing.separates(spacing.kind)
            }
            None => false,
        };
        previous = Some((plan, spacing));
    }
}

/// Renders one unit, or returns the cached render when nothing it depends on
/// changed. The unit reports its own anchor, whose header line count is
/// re-derived here because oversized lines split during layout.
fn reuse_or_render(
    reusable: &mut HashMap<UnitKey, UnitView>,
    unit: &RenderUnit<'_>,
    folds: &TranscriptFolds,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
    workspace: &Path,
) -> UnitView {
    let key = unit.key();
    let fingerprint = unit.fingerprint(folds);
    if let Some(cached) = reusable.remove(&key)
        && cached.fingerprint == fingerprint
    {
        return cached;
    }
    let mut rendered = Vec::new();
    let mut links = Vec::new();
    let mut strips = Vec::new();
    let rendered_anchor = unit.render(
        &mut ActivityProjection {
            lines: &mut rendered,
            links: &mut links,
            strips: &mut strips,
        },
        folds,
        theme,
        width,
        hyperlinks,
        workspace,
    );
    let source_lines = rendered.len();
    let mut lines = Vec::with_capacity(rendered.len());
    let mut rows = Vec::with_capacity(rendered.len());
    let mut rows_per_line = Vec::with_capacity(rendered.len());
    let mut first_row_of_source_line = Vec::with_capacity(rendered.len());
    let mut header_lines = 0;
    let mut marker_line = None;
    for (index, line) in rendered.into_iter().enumerate() {
        if rendered_anchor.is_some_and(|anchor| anchor.marker_source_line == Some(index)) {
            marker_line = Some(lines.len());
        }
        first_row_of_source_line.push(rows.len());
        for (line, layout) in layout_line(line, width) {
            let line_index = lines.len();
            rows_per_line.push(layout.row_count());
            rows.extend(layout.into_rows().into_iter().map(|row| WrappedRow {
                line: line_index,
                row,
            }));
            lines.push(line);
        }
        if rendered_anchor.is_some_and(|anchor| index + 1 == anchor.header_source_lines) {
            header_lines = lines.len();
        }
    }
    let spinner_rows = unit
        .spinner_lines(folds)
        .into_iter()
        .filter_map(|line| first_row_of_source_line.get(line).copied())
        .collect();
    let strips = strips
        .into_iter()
        .filter_map(|strip| {
            let row = *first_row_of_source_line.get(strip.start)?;
            Some(ProjectedStrip {
                start: row,
                ..strip
            })
        })
        .collect();
    UnitView {
        key,
        fingerprint,
        lines,
        rows,
        links,
        rows_per_line,
        source_lines,
        leading_separator: false,
        start_line: 0,
        start_row: 0,
        spinner_rows,
        strips,
        message_id: unit.message_id(),
        anchor: rendered_anchor.map(|anchor| LaidOutAnchor {
            header_lines,
            hides_content: anchor.hides_content,
            fold: anchor.fold,
            marker_line,
        }),
    }
}

/// What a projected unit reports about the block it pushed: how many of those
/// lines form the header a reader clicks to close it again, whether it held
/// any content back, and where its staged Fold stands. The counts are in
/// pre-split source lines, which layout resolves to rows.
#[derive(Clone, Copy, Debug)]
struct UnitAnchor {
    header_source_lines: usize,
    hides_content: bool,
    /// The Fold grammar and state the unit drew.
    fold: FoldDisclosure,
    /// Index of the fold-marker source line within the unit's lines.
    marker_source_line: Option<usize>,
}

impl UnitAnchor {
    /// The anchor a binary Fold reports: no staged step and no marker target.
    const fn binary(header_source_lines: usize, hides_content: bool) -> Self {
        Self {
            header_source_lines,
            hides_content,
            fold: FoldDisclosure::Binary {
                folded: hides_content,
            },
            marker_source_line: None,
        }
    }
}

/// Message content is append-only, so its length identifies it within a
/// Session once the truncation signal, which flips without lengthening the
/// content, is folded in, beside its bindings and how its Attachments
/// present, which changes when a descriptor or a thumbnail arrives after the
/// Message.
fn message_fingerprint(message: &Message, attachments: &AttachmentRows) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    message.content.len().hash(&mut hasher);
    message.truncated.hash(&mut hasher);
    attachments.hash(&mut hasher);
    TextBindings::from_message(message).hash(&mut hasher);
    hasher.finish()
}

/// How the Attachments `bindings` bind present beneath the text, describing
/// each by what the Session carries for it.
fn attachment_rows(
    bindings: &TextBindings,
    described: &[AttachmentDescriptor],
    previews: &AttachmentPreviews,
) -> AttachmentRows {
    previews.rows(bindings, |id| {
        described.iter().find(|descriptor| &descriptor.id == id)
    })
}

/// The step an Activity's Fold rests at before the reader touches it. A
/// failed command or Tool Call opens to its Peek — the tail is where the error
/// lives — so the one output worth reading is on screen, while everything else
/// folds away. Whether a command exited says nothing of how it settled: one
/// that failed before it ran opens all the same. Work an interrupt cut off
/// settles Interrupted and folds like a success; the reader who was watching
/// it still gets a Peek, but through the interrupt-time override rather than
/// this default.
fn default_fold_step(activity: &Activity) -> FoldStep {
    match activity {
        Activity::Command {
            status: crate::protocol::ActivityStatus::Failed,
            ..
        }
        | Activity::ToolCall {
            status: crate::protocol::ActivityStatus::Failed,
            ..
        } => FoldStep::Peek,
        _ => FoldStep::Folded,
    }
}

/// Whether an Activity is still producing the output its Fold tails: an
/// Active command or Tool Call, which enters on its one-line shape and grows
/// into its live tail only by an override.
pub(super) fn streams_live_output(activity: &Activity) -> bool {
    matches!(
        activity,
        Activity::Command {
            status: crate::protocol::ActivityStatus::Active,
            ..
        } | Activity::ToolCall {
            status: crate::protocol::ActivityStatus::Active,
            ..
        }
    )
}

/// The step an Activity presents at under the client's Fold state.
fn resolved_fold_step(folds: &TranscriptFolds, activity: &Activity) -> FoldStep {
    if streams_live_output(activity) {
        folds.resolve_active_output(activity.id())
    } else {
        folds.resolve(activity.id(), default_fold_step(activity))
    }
}

/// Identifies an Activity's rendered form. The Fold step joins the content
/// signals because a folded entry renders different lines from the same
/// Activity.
fn activity_fingerprint(activity: &Activity, step: FoldStep) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    (step as u8).hash(&mut hasher);
    match activity {
        Activity::Approval {
            approval,
            tool_activity_id,
            detail_truncated,
            outcome,
            decision,
            follow_up_error,
            ..
        } => {
            (*outcome as u8).hash(&mut hasher);
            decision.map(|value| value as u8).hash(&mut hasher);
            serde_json::to_string(approval).ok().hash(&mut hasher);
            tool_activity_id.hash(&mut hasher);
            detail_truncated.hash(&mut hasher);
            follow_up_error.hash(&mut hasher);
        }
        Activity::Questionnaire { outcome, .. } => (*outcome as u8).hash(&mut hasher),
        Activity::Status { .. } | Activity::Error { .. } => {}
        Activity::Command {
            status,
            output,
            output_truncated,
            exit_status,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            output.len().hash(&mut hasher);
            output_truncated.hash(&mut hasher);
            exit_status.hash(&mut hasher);
        }
        // The input is replaced whole rather than appended to, so it is hashed
        // rather than measured; the output only grows, so its length will do.
        Activity::ToolCall {
            status,
            name,
            server,
            input,
            input_truncated,
            output,
            output_truncated,
            omitted_parts,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            name.hash(&mut hasher);
            server.hash(&mut hasher);
            input.hash(&mut hasher);
            input_truncated.hash(&mut hasher);
            output.len().hash(&mut hasher);
            output_truncated.hash(&mut hasher);
            omitted_parts.hash(&mut hasher);
        }
        Activity::Reasoning {
            status,
            title,
            content,
            content_truncated,
            duration_ms,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            title.hash(&mut hasher);
            content.len().hash(&mut hasher);
            content_truncated.hash(&mut hasher);
            duration_ms.hash(&mut hasher);
        }
        Activity::FileChange {
            status, changes, ..
        } => {
            (*status as u8).hash(&mut hasher);
            for change in changes {
                match change {
                    FileChange::Add { path } => {
                        0u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                    }
                    FileChange::Delete { path } => {
                        1u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                    }
                    FileChange::Update { path, moved_to } => {
                        2u8.hash(&mut hasher);
                        path.hash(&mut hasher);
                        moved_to.hash(&mut hasher);
                    }
                }
            }
        }
        Activity::Subagent {
            status,
            name,
            description,
            duration_ms,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            name.hash(&mut hasher);
            description.hash(&mut hasher);
            duration_ms.hash(&mut hasher);
        }
        Activity::WatchOutcome {
            status,
            description,
            summary,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            description.hash(&mut hasher);
            summary.hash(&mut hasher);
        }
        Activity::Compaction {
            status,
            trigger,
            instructions,
            before_tokens,
            after_tokens,
            error,
            summary,
            summary_truncated,
            ..
        } => {
            (*status as u8).hash(&mut hasher);
            (*trigger as u8).hash(&mut hasher);
            instructions.hash(&mut hasher);
            before_tokens.hash(&mut hasher);
            after_tokens.hash(&mut hasher);
            error.hash(&mut hasher);
            summary.hash(&mut hasher);
            summary_truncated.hash(&mut hasher);
        }
        Activity::Subsession { title, prompt, .. } => {
            title.hash(&mut hasher);
            prompt.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// A Group unit renders from its kind, its membership, and which way it is
/// flipped, the collapsed row and the expanded header being two drawings of
/// the same unit. What each member contributes is the kind's answer, since
/// that is what decides how much of a member the header speaks for. Folds are
/// deliberately absent: they act on the members, which key themselves as the
/// units they render as.
fn group_fingerprint(kind: GroupableKind, members: &[&Activity], expanded: bool) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    kind.hash(&mut hasher);
    expanded.hash(&mut hasher);
    members.len().hash(&mut hasher);
    for member in members {
        kind.member_fingerprint(member).hash(&mut hasher);
    }
    hasher.finish()
}

fn provisional_fingerprint(provisional: &[&PendingPrompt]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for pending in provisional {
        pending.prompt.id.hash(&mut hasher);
        pending.prompt.text.len().hash(&mut hasher);
        pending.author.hash(&mut hasher);
    }
    hasher.finish()
}

enum ContentToken {
    Text(String),
    Sgr(String),
    LinkStart(String),
    LinkEnd,
    Tab,
    LineBreak,
}

/// Reduces Session content to printable text, layout controls, and supported
/// style/link transitions. Every escape/control sequence omitted here stays
/// invisible in both the strip-only and styled rendering paths.
fn content_tokens(text: &str) -> impl Iterator<Item = ContentToken> {
    AnsiScanner::default()
        .feed(text)
        .into_iter()
        .filter_map(|fragment| match fragment.into_role() {
            FragmentRole::Text(text) => Some(ContentToken::Text(text)),
            FragmentRole::Sgr(sequence) => Some(ContentToken::Sgr(sequence)),
            FragmentRole::Hyperlink(hyperlink) => Some(match hyperlink.target() {
                Some(target) => ContentToken::LinkStart(target.to_owned()),
                None => ContentToken::LinkEnd,
            }),
            FragmentRole::Tab => Some(ContentToken::Tab),
            FragmentRole::LineBreak => Some(ContentToken::LineBreak),
            FragmentRole::CarriageReturn | FragmentRole::Invisible => None,
        })
}

/// Removes terminal control sequences from Session content before it becomes
/// cell symbols. Provider output can carry raw ANSI escapes (colored build
/// logs, cursor movement); written verbatim they desynchronize the terminal
/// from ratatui's cell model, leaving stale characters on screen.
fn sanitize_content(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .chars()
        .any(|character| character != '\n' && (character.is_control() || character == '\u{7f}'))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for token in content_tokens(text) {
        match token {
            ContentToken::Text(text) => out.push_str(&text),
            ContentToken::Tab => out.push_str("    "),
            ContentToken::LineBreak => out.push('\n'),
            ContentToken::Sgr(_) | ContentToken::LinkStart(_) | ContentToken::LinkEnd => {}
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Projects one Message. `parent` is the parent of the Session whose
/// Transcript holds it, which is what a Delegation names its sender against.
/// Projects a Message by its role, answering where a user Message's strip of
/// thumbnails begins among `lines`, where it has one.
fn render_message(
    lines: &mut Vec<StyledLine>,
    message: &Message,
    parent: Option<SessionId>,
    attachments: &AttachmentRows,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Option<usize> {
    match (&message.role, &message.author) {
        (MessageRole::User, None) => push_user_message(
            lines,
            &message.content,
            &TextBindings::from_message(message),
            attachments,
            theme,
            width,
        ),
        (MessageRole::User, Some(author)) => {
            push_authored_words(
                lines,
                &message.content,
                message.truncated,
                author,
                theme,
                width,
            );
            None
        }
        (MessageRole::Agent, _) => {
            push_agent_message(
                lines,
                &message.content,
                message.truncated,
                theme,
                width,
                hyperlinks,
            );
            None
        }
        (MessageRole::Delegation(delegator), _) => {
            push_attributed_message(
                lines,
                &message.content,
                message.truncated,
                &format!("Delegated by {}", delegation_sender(delegator, parent)),
                theme,
                width,
            );
            None
        }
    }
}

/// Projects words someone sent on the user's behalf — a Prompt a Sidekick
/// sent, delivered or not yet — as what an Agent was asked rather than as
/// what the user wrote. Such a Prompt carries its words alone, no Skill it
/// invokes and no Attachment it binds. Its heading names the author on one
/// row, cut short where it runs long, so that row is the whole of the way to
/// the author wherever a press lands on it.
fn push_authored_words(
    lines: &mut Vec<StyledLine>,
    content: &str,
    truncated: bool,
    author: &Author,
    theme: &Theme,
    width: u16,
) {
    let heading = match author {
        Author::Sidekick { title, .. } => {
            let title = sanitize_content(title)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if title.is_empty() {
                "Sent by a Sidekick".to_owned()
            } else {
                format!("Sent by Sidekick · {title}")
            }
        }
    };
    let (heading, _) = clamp_to_one_row(
        &heading,
        usize::from(width)
            .saturating_sub(DELEGATION_GUTTER.width() + USER_MESSAGE_RIGHT_MARGIN)
            .max(1),
    );
    push_attributed_message(lines, content, truncated, &heading, theme, width);
}

/// How a Delegation names the Agent that sent it to the reader of the
/// Subagent's Transcript: as its parent when the parent delegated, and by the
/// name its own Subagent rows carry wherever it has one. A sender that is
/// neither the parent nor a Subagent can only be a top-level Session's Agent
/// delegating to a Subagent it did not spawn.
fn delegation_sender(delegator: &Delegator, parent: Option<SessionId>) -> String {
    let from_parent = parent == Some(delegator.session_id);
    match (delegator.name.as_deref(), from_parent) {
        (Some(name), true) => format!("{name} (parent)"),
        (Some(name), false) => name.to_owned(),
        (None, true) => "parent".to_owned(),
        (None, false) => "the top-level Agent".to_owned(),
    }
}

/// Projects one Activity, reporting an anchor for the kinds a reader can fold.
/// `Status` is already one line and `Error` is a failure the reader must not
/// have to hunt for, so neither is ever folded and neither anchors a click.
/// Each kind projects through its own renderer, so what one kind shows never
/// depends on how another renders.
fn render_activity(
    projection: &mut ActivityProjection<'_>,
    activity: &Activity,
    step: FoldStep,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
    workspace: &Path,
) -> Option<UnitAnchor> {
    match activity {
        Activity::Approval {
            approval,
            tool_activity_id,
            detail_truncated,
            outcome,
            decision,
            follow_up_error,
            ..
        } => {
            let folded = step != FoldStep::Expanded;
            let detail = super::approval::detail_lines(approval, *tool_activity_id);
            let hidden = detail.len() + usize::from(*detail_truncated);
            let mut header = vec![
                StyledSpan::chrome(
                    format!("  {} ", if folded { "▸" } else { "▾" }),
                    theme.accent.primary,
                ),
                StyledSpan::text(
                    format!(
                        "Approval · {} · {}",
                        super::approval::subject_summary(&approval.subject),
                        super::approval::outcome_text(
                            *outcome,
                            *decision,
                            follow_up_error.as_deref(),
                        ),
                    ),
                    theme.accent.primary,
                ),
            ];
            if outcome.is_answerable() {
                header.push(StyledSpan::chrome(" · Ctrl+Y decide", theme.accent.primary));
            }
            if folded && hidden > 0 {
                header.push(StyledSpan::chrome(
                    format!(" · … {}", fold_marker_text(hidden, "lines")),
                    theme.accent.primary,
                ));
            }
            projection.lines.push(StyledLine::from(header));
            if !folded {
                for line in detail {
                    projection.lines.push(StyledLine::from(vec![
                        StyledSpan::chrome(OUTPUT_INDENT, Style::default()),
                        StyledSpan::text(line, Style::default()),
                    ]));
                }
                if *detail_truncated {
                    push_truncation_marker(
                        projection.lines,
                        CappedStream::ApprovalDetail,
                        OUTPUT_INDENT,
                        theme,
                    );
                }
            }
            Some(UnitAnchor::binary(1, folded && hidden > 0))
        }
        Activity::Questionnaire {
            questionnaire,
            outcome,
            answer,
            ..
        } => {
            let folded = step != FoldStep::Expanded;
            projection.lines.push(StyledLine::from(vec![
                StyledSpan::chrome(
                    format!("  {} ", if folded { "▸" } else { "▾" }),
                    theme.accent.primary,
                ),
                StyledSpan::text(
                    format!(
                        "Questionnaire · {} · {} question(s){}",
                        match outcome {
                            crate::protocol::QuestionnaireOutcome::SubmissionRejected =>
                                "Answer not delivered; review and retry",
                            crate::protocol::QuestionnaireOutcome::DeliveryUncertain =>
                                "Delivery uncertain; Answer will not be resent",
                            crate::protocol::QuestionnaireOutcome::Unavailable =>
                                "Unavailable; previous Provider request is no longer live",
                            crate::protocol::QuestionnaireOutcome::Pending => "Pending",
                            crate::protocol::QuestionnaireOutcome::Submitting => "Submitting",
                            crate::protocol::QuestionnaireOutcome::Answered => "Answered",
                            crate::protocol::QuestionnaireOutcome::Declined => "Declined",
                            crate::protocol::QuestionnaireOutcome::Withdrawn => "Withdrawn",
                            crate::protocol::QuestionnaireOutcome::TurnEnded => "TurnEnded",
                        },
                        questionnaire.questions.len(),
                        if outcome.is_answerable() {
                            " · Ctrl+Q answer"
                        } else {
                            ""
                        }
                    ),
                    theme.accent.primary,
                ),
            ]));
            if !folded {
                for (index, question) in questionnaire.questions.iter().enumerate() {
                    projection.lines.push(StyledLine::from(vec![
                        StyledSpan::chrome(OUTPUT_INDENT, Style::default()),
                        StyledSpan::text(question.text.clone(), Style::default()),
                    ]));
                    let value = answer
                        .as_ref()
                        .and_then(|answer| answer.questions.get(index));
                    projection.lines.push(StyledLine::from(vec![
                        StyledSpan::chrome(OUTPUT_INDENT, Style::default()),
                        StyledSpan::text(
                            super::questionnaire::answer_text(question, value),
                            Style::default(),
                        ),
                    ]));
                }
            }
            Some(UnitAnchor::binary(1, folded))
        }
        Activity::Status { text, .. } => {
            push_styled_prefixed_lines(
                projection,
                ContentGutter {
                    lead: "  ",
                    indent: "  ",
                },
                text,
                theme.text.subdued,
                theme,
            );
            None
        }
        Activity::Error { text, .. } => {
            push_styled_prefixed_lines(
                projection,
                ContentGutter {
                    lead: "  Error: ",
                    indent: "  ",
                },
                text,
                theme.feedback.error,
                theme,
            );
            None
        }
        Activity::Command {
            status,
            command,
            cwd,
            output,
            output_truncated,
            exit_status,
            ..
        } => Some(push_command_activity(
            projection,
            CommandActivity {
                status: *status,
                command,
                cwd: cwd.as_deref().map(|path| transcript_path(path, workspace)),
                output,
                output_truncated: *output_truncated,
                exit_status: *exit_status,
            },
            step,
            theme,
            width,
        )),
        Activity::ToolCall {
            status,
            name,
            server,
            input,
            input_truncated,
            output,
            output_truncated,
            omitted_parts,
            ..
        } => Some(push_tool_call_activity(
            projection,
            ToolCallActivity {
                status: *status,
                label: &tool_call_label(server.as_deref(), name, input),
                input_truncated: *input_truncated,
                output,
                output_truncated: *output_truncated,
                omitted_parts: *omitted_parts,
            },
            step,
            theme,
            width,
        )),
        Activity::FileChange {
            status, changes, ..
        } => Some(push_file_change_activity(
            projection.lines,
            *status,
            changes,
            step != FoldStep::Expanded,
            theme,
            width,
            workspace,
        )),
        Activity::Reasoning { .. } => ReasoningActivity::of(activity).map(|reasoning| {
            push_reasoning_activity(
                projection.lines,
                reasoning,
                step != FoldStep::Expanded,
                theme,
                width,
                hyperlinks,
            )
        }),
        Activity::Subagent {
            status,
            name,
            description,
            duration_ms,
            ..
        } => Some(push_subagent_activity(
            projection.lines,
            *status,
            name,
            description,
            *duration_ms,
            theme,
        )),
        Activity::WatchOutcome {
            status,
            description,
            summary,
            ..
        } => {
            push_watch_outcome_activity(
                projection.lines,
                *status,
                description,
                summary.as_deref(),
                theme,
            );
            None
        }
        Activity::Compaction {
            status,
            trigger,
            instructions,
            before_tokens,
            after_tokens,
            error,
            summary,
            summary_truncated,
            ..
        } => push_compaction_activity(
            projection.lines,
            CompactionActivity {
                status: *status,
                trigger: *trigger,
                instructions: instructions.as_deref(),
                before_tokens: *before_tokens,
                after_tokens: *after_tokens,
                error: error.as_deref(),
                summary: summary.as_deref(),
                summary_truncated: *summary_truncated,
            },
            step != FoldStep::Expanded,
            theme,
            width,
            hyperlinks,
        ),
        Activity::Subsession { title, prompt, .. } => Some(push_subsession_activity(
            projection.lines,
            title,
            prompt,
            theme,
        )),
    }
}

/// Draws client-local failures in the same register as a Transcript Error
/// Activity without manufacturing one. The caller owns where these lines
/// live; this function contributes presentation only.
pub(super) fn client_error_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut links = Vec::new();
    let mut projection = ActivityProjection {
        lines: &mut lines,
        links: &mut links,
        strips: &mut Vec::new(),
    };
    push_styled_prefixed_lines(
        &mut projection,
        ContentGutter {
            lead: "  Error: ",
            indent: "  ",
        },
        text,
        theme.feedback.error,
        theme,
    );
    lines.iter().map(StyledLine::to_line).collect()
}

/// Projects a Group, which each groupable kind words and opens in its own way.
/// What every kind shares is the shape: a header row that stands in for the
/// run while collapsed and heads it while expanded, being the one row the
/// Group re-collapses from. `members` is never empty.
fn render_group(
    lines: &mut Vec<StyledLine>,
    kind: GroupableKind,
    members: &[&Activity],
    expanded: bool,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> UnitAnchor {
    match kind {
        GroupableKind::Command => render_counted_group(
            lines,
            CountedNoun {
                running: "Running",
                settled: "Ran",
                one: "command",
                many: "commands",
            },
            members,
            expanded,
            theme,
            width,
        ),
        GroupableKind::ToolCall => render_counted_group(
            lines,
            CountedNoun {
                running: "Using",
                settled: "Used",
                one: "tool",
                many: "tools",
            },
            members,
            expanded,
            theme,
            width,
        ),
        GroupableKind::Reasoning => {
            render_reasoning_group(lines, members, expanded, theme, width, hyperlinks)
        }
    }
}

/// How a counted Group's header words its members: the verb in each tense and
/// the noun in each number, so `Running 1 command` and `Used 3 tools` come
/// out of one rule.
struct CountedNoun {
    running: &'static str,
    settled: &'static str,
    one: &'static str,
    many: &'static str,
}

/// The member a counted Group's header names while the run works: the latest
/// one still running, which is where the run is now.
fn running_member<'a>(members: &[&'a Activity]) -> Option<&'a Activity> {
    members
        .iter()
        .rev()
        .copied()
        .find(|member| member.status() == Some(crate::protocol::ActivityStatus::Active))
}

/// The one-line account of a command or Tool Call a Group's header gives for
/// its running member: the command, or the Tool and its input, exactly as the
/// member's own row leads with them, cut at the first line break.
fn member_summary(member: &Activity) -> String {
    let summary = match member {
        Activity::Command { command, .. } => sanitize_content(command).into_owned(),
        Activity::ToolCall {
            name,
            server,
            input,
            ..
        } => sanitize_content(&tool_call_label(server.as_deref(), name, input)).into_owned(),
        _ => String::new(),
    };
    summary.lines().next().unwrap_or_default().to_owned()
}

/// How many members of a Group settled short of success, told apart the way
/// ADR 0039 tells them apart: the ones that went wrong, and the ones a stop cut
/// off. A Group's Marker and the counts its header ends on both read this, so
/// every groupable kind reports its members' outcomes alike.
struct SettledTally {
    failed: usize,
    stopped: usize,
}

impl SettledTally {
    fn of(members: &[&Activity]) -> Self {
        use crate::protocol::ActivityStatus;

        let count = |status| {
            members
                .iter()
                .filter(|member| member.status() == Some(status))
                .count()
        };
        Self {
            failed: count(ActivityStatus::Failed),
            stopped: count(ActivityStatus::Interrupted),
        }
    }

    /// The Marker a settled Group wears for how its members settled: the
    /// failure × if any failed, otherwise the warning × a stopped Subagent
    /// wears if any was stopped, otherwise `None`, leaving the kind its own
    /// success Marker.
    fn cut_short_marker(&self, theme: &Theme) -> Option<(&'static str, Style)> {
        if self.failed > 0 {
            Some(("× ", theme.feedback.error))
        } else if self.stopped > 0 {
            Some(("× ", theme.feedback.warning))
        } else {
            None
        }
    }

    /// The counts a settled Group's header reports, each in the style of the
    /// outcome it counts, leaving out any that are zero.
    fn counts(&self, theme: &Theme) -> Vec<(String, Style)> {
        [
            (self.failed, "failed", theme.feedback.error),
            (self.stopped, "stopped", theme.feedback.warning),
        ]
        .into_iter()
        .filter(|(members, ..)| *members > 0)
        .map(|(members, word, style)| (format!("{members} {word}"), style))
        .collect()
    }
}

/// Projects the header of a Group that counts its members — `Ran 4 commands`,
/// `Used 1 tool`: one row in the Activity-header idiom, clamped to the width,
/// with the count styled as the toggle affordance it is. Collapsed, the row
/// stands in for every member and the count doubles as the hidden-ness
/// indicator, so no fold-marker line follows; expanded, the same header leads
/// the member units.
///
/// While any member runs the row speaks in the present tense behind a
/// Spinner, and names the running member in a dim suffix, so a reader
/// watching a collapsed Group still sees what it is doing. Once every member
/// has settled it speaks in the past tense and counts the members that failed
/// and the ones a stop cut off; its Marker is the worst of how they settled —
/// a failure over a stop, and either over success.
fn render_counted_group(
    lines: &mut Vec<StyledLine>,
    noun: CountedNoun,
    members: &[&Activity],
    expanded: bool,
    theme: &Theme,
    width: u16,
) -> UnitAnchor {
    let tally = SettledTally::of(members);
    let running = running_member(members);
    let (marker, marker_style) = match running {
        Some(_) => (spinner::MARKER, theme.accent.primary),
        None => tally
            .cut_short_marker(theme)
            .unwrap_or(("✓ ", theme.feedback.success)),
    };
    let verb = if running.is_some() {
        noun.running
    } else {
        noun.settled
    };
    let count = members.len();
    let noun = if count == 1 { noun.one } else { noun.many };
    let mut row = vec![
        SlotText::new(format!("  {marker}"), marker_style),
        SlotText::new(format!("{verb} {count} {noun}"), theme.action.primary),
    ];
    if let Some(member) = running {
        row.push(SlotText::new(
            format!(" · {}", member_summary(member)),
            theme.text.subdued,
        ));
    } else {
        for (count, style) in tally.counts(theme) {
            row.push(SlotText::new(" · ", theme.text.subdued));
            row.push(SlotText::new(count, style));
        }
    }
    lines.push(StyledLine::from(
        truncate_slot_text(row, usize::from(width))
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                if index == 0 {
                    StyledSpan::chrome(item.text, item.style)
                } else {
                    StyledSpan::text(item.text, item.style)
                }
            })
            .collect::<Vec<_>>(),
    ));
    UnitAnchor::binary(1, !expanded)
}

/// Projects a Reasoning Group: the run's single row, and — when the reader
/// opened it — every member's prose beneath it. The row exists in the same
/// place through the whole run, reading as live thinking while its latest
/// member streams and flipping to the settled account of it once that member
/// lands, so a reader watching a Turn work never sees the line move.
///
/// Settled, the row leads with the latest member's title, because what a
/// reader wants from a run of thinking is where it arrived rather than where it
/// set out; a latest member the Provider never headed leaves the row with no
/// description to give, and it gives none rather than reaching back for an
/// earlier heading. It then counts its steps — the toggle affordance, the way a
/// command Group's count is — counts the members that failed or were stopped,
/// and sums what its members spent. A latest member a Turn cut short reads as
/// interrupted, exactly as it would alone. A Group of one is the exception: it
/// keeps the header-and-fold row a lone block has instead of gaining a count
/// of one.
///
/// Live, the row is the running Marker and the topic alone: steps still being
/// taken are not a total to report, and thinking still going on has no duration
/// to state. Its description does reach back, because a section the Provider
/// has not headed yet is thinking that has not said where it is going — so the
/// row holds the last topic it knew rather than falling silent mid-run.
///
/// Expanded, the same row heads the sections it opened onto, live or settled.
fn render_reasoning_group(
    lines: &mut Vec<StyledLine>,
    members: &[&Activity],
    expanded: bool,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    // A Group of one reads exactly as the lone block it holds: its header and
    // its Fold's two steps, driven by the Group's disclosure instead.
    if let [member] = members
        && let Some(reasoning) = ReasoningActivity::of(member)
    {
        return push_reasoning_activity(lines, reasoning, !expanded, theme, width, hyperlinks);
    }
    // The words the row speaks are where the run arrived: still thinking
    // while its latest member is, thought once that member lands, and
    // interrupted when a Turn cut the latest member short — a block cut off
    // stays in its run, and being cut off is where that run ended. Its Marker
    // speaks for every member, so a block cut short earlier in the run still
    // shows once the run settles.
    let live = reasoning_group_is_live(members);
    let status =
        latest_reasoning(members).map_or(ActivityStatus::Completed, |latest| latest.status);
    let tally = SettledTally::of(members);
    let (marker, label, style) = match (live, tally.cut_short_marker(theme)) {
        (false, Some((marker, style))) => (marker, reasoning_marker(status, theme).1, style),
        _ => reasoning_marker(status, theme),
    };
    let heading = if live {
        held_reasoning_heading(members)
    } else {
        latest_reasoning_heading(members)
    };
    let header = reasoning_header_text(label, heading);
    push_prefixed_lines(lines, &format!("  {marker}"), &header, style);
    if !live {
        let header_line = lines
            .last_mut()
            .expect("a Reasoning Group always projects a header line");
        header_line.spans.push(StyledSpan::text(" · ", style));
        header_line.spans.push(StyledSpan::text(
            format!("{} steps", members.len()),
            theme.action.primary,
        ));
        for (count, count_style) in tally.counts(theme) {
            header_line.spans.push(StyledSpan::text(" · ", style));
            header_line.spans.push(StyledSpan::text(count, count_style));
        }
        if let Some(duration_ms) = summed_reasoning_duration(members) {
            header_line.spans.push(StyledSpan::text(
                format!(" · {}", humanized_duration(duration_ms)),
                style,
            ));
        }
    }
    let header_source_lines = lines.len();
    if !expanded {
        return UnitAnchor::binary(header_source_lines, true);
    }
    let mut opened_a_section = false;
    for member in members.iter().copied().filter_map(ReasoningActivity::of) {
        let mut section = Vec::new();
        push_reasoning_section(&mut section, member, theme, width, hyperlinks);
        // A member that has started without saying anything or being headed
        // projects nothing, and a section that is not there takes no blank row
        // to stand apart from the one before it.
        if section.is_empty() {
            continue;
        }
        if opened_a_section {
            lines.push(StyledLine::default());
        }
        lines.append(&mut section);
        opened_a_section = true;
    }
    UnitAnchor::binary(header_source_lines, false)
}

/// The member a Reasoning Group's row speaks for: the latest one, which is
/// where the run arrived and the only one it can still be in, since every
/// earlier member had to settle for the next to follow it.
fn latest_reasoning<'a>(members: &[&'a Activity]) -> Option<ReasoningActivity<'a>> {
    members.last().copied().and_then(ReasoningActivity::of)
}

/// Whether a Reasoning Group stands for thinking going on right now, which is
/// the one thing its row's whole wording turns on.
fn reasoning_group_is_live(members: &[&Activity]) -> bool {
    latest_reasoning(members)
        .is_some_and(|reasoning| reasoning.status == crate::protocol::ActivityStatus::Active)
}

/// Projects one section of an expanded Reasoning Group: the heading the
/// Provider led that section with, in bold so a reader scans the sections by
/// their titles, over the prose it heads. There is no per-member fold marker
/// because there is no per-member Fold: a Reasoning Group opens onto
/// everything its members hold in one step, so nothing here is holding
/// anything back.
fn push_reasoning_section(
    lines: &mut Vec<StyledLine>,
    reasoning: ReasoningActivity<'_>,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) {
    let style = theme.text.subdued;
    if let Some(title) = reasoning_heading(reasoning.title) {
        push_prefixed_lines(
            lines,
            OUTPUT_INDENT,
            title,
            style.add_modifier(Modifier::BOLD),
        );
    }
    lines.append(&mut reasoning_body_lines(
        &reasoning, theme, width, hyperlinks,
    ));
}

/// The description a settled Reasoning Group's row leads with, or `None` when
/// its latest member carries no heading. Read off the last member rather than
/// the last member that has one, because the row states where the thinking
/// ended up: a run whose final section the Provider left unheaded ended up
/// somewhere it did not name. A live row reaches back instead — see
/// [`held_reasoning_heading`].
fn latest_reasoning_heading<'a>(members: &[&'a Activity]) -> Option<&'a str> {
    reasoning_heading(latest_reasoning(members)?.title)
}

/// The description a live Reasoning Group's row leads with: the heading of the
/// latest member that has one. A settled row reads the last member alone
/// because the run ended there and has nothing further to say, but a live row
/// is naming a topic the reader is watching, and the section streaming under
/// an unwritten heading is still the same thinking the last heading announced.
fn held_reasoning_heading<'a>(members: &[&'a Activity]) -> Option<&'a str> {
    members
        .iter()
        .rev()
        .copied()
        .filter_map(ReasoningActivity::of)
        .find_map(|reasoning| reasoning_heading(reasoning.title))
}

/// The duration a Reasoning Group's row reports: the sum of what its members
/// each spent, not the wall-clock span they cover, so the row stands for the
/// thinking time it gathered rather than for the stretch of Transcript it
/// occupies. A run whose members carry no durations reports none, exactly as a
/// lone block without one does.
fn summed_reasoning_duration(members: &[&Activity]) -> Option<u64> {
    members
        .iter()
        .copied()
        .filter_map(ReasoningActivity::of)
        .filter_map(|reasoning| reasoning.duration_ms)
        .reduce(u64::saturating_add)
}

/// Projects a Turn Fold's marker: the row a settled Turn stands as, drawn in
/// the settled Activity-header idiom with the outcome word styled as the toggle
/// affordance it is. The marker only exists when its fold covers something, so
/// closed it reports as holding content back; open it is the header the Turn
/// folds back from, exactly as a Group's header is.
fn render_turn_fold(lines: &mut Vec<StyledLine>, marker: TurnMarker, theme: &Theme) -> UnitAnchor {
    let (glyph, style) = match marker.outcome {
        SettledTurn::Completed => ("✓ ", theme.feedback.success),
        SettledTurn::Interrupted => ("× ", theme.feedback.warning),
        SettledTurn::Failed => ("× ", theme.feedback.error),
    };
    lines.push(StyledLine::from(vec![
        StyledSpan::chrome(format!("  {glyph}"), style),
        StyledSpan::text(marker.label(), theme.action.primary),
    ]));
    UnitAnchor::binary(1, marker.folded)
}

struct ActivityProjection<'a> {
    lines: &'a mut Vec<StyledLine>,
    links: &'a mut Vec<TranscriptLink>,
    /// The strips of thumbnails the unit's user Messages reserved rows for.
    strips: &'a mut Vec<ProjectedStrip>,
}

/// A strip of thumbnails as a unit projected it: where its reserved rows
/// begin, and the columns of those rows it stands across.
#[derive(Clone, Debug)]
struct ProjectedStrip {
    /// The unit's line its reserved rows begin on while the unit is being
    /// projected, and the unit's row once it is laid out. A reserved row is
    /// one line that never wraps, so the one becomes the other unchanged in
    /// kind.
    start: usize,
    left: u16,
    width: u16,
    strip: AttachmentStrip,
}

/// What a command Activity contributes to the transcript, gathered so the
/// renderer reads one subject rather than a row of loose parameters.
struct CommandActivity<'a> {
    status: crate::protocol::ActivityStatus,
    command: &'a str,
    cwd: Option<&'a std::path::Path>,
    output: &'a str,
    output_truncated: bool,
    exit_status: Option<i32>,
}

/// Projects a command Activity through its staged Fold: its command heads it,
/// the directory it ran in opens beneath that, and a failed command keeps its
/// exit status on the header however the Fold clamps it.
fn push_command_activity(
    projection: &mut ActivityProjection<'_>,
    activity: CommandActivity<'_>,
    step: FoldStep,
    theme: &Theme,
    width: u16,
) -> UnitAnchor {
    let CommandActivity {
        status,
        command,
        cwd,
        output,
        output_truncated,
        exit_status,
    } = activity;
    let header_suffix = match (status, exit_status) {
        (crate::protocol::ActivityStatus::Failed, Some(exit_status)) => {
            format!(" (exit {exit_status})")
        }
        _ => String::new(),
    };
    let mut details = Vec::new();
    if let Some(cwd) = cwd {
        let cwd_prefix = format!("{OUTPUT_DETAIL_INDENT}in ");
        push_prefixed_lines(
            &mut details,
            &cwd_prefix,
            cwd.to_string_lossy().as_ref(),
            theme.text.subdued,
        );
    }
    push_output_fold(
        projection,
        OutputFold {
            status,
            header: command,
            header_suffix,
            details,
            output,
            output_truncation: output_truncated.then_some(CappedStream::CommandOutput),
            notes: Vec::new(),
        },
        step,
        theme,
        width,
    )
}

/// What a Tool Call contributes to the transcript, gathered as a command's is.
struct ToolCallActivity<'a> {
    status: crate::protocol::ActivityStatus,
    /// The Tool and its input, as [`tool_call_label`] reads them.
    label: &'a str,
    input_truncated: bool,
    output: &'a str,
    output_truncated: bool,
    omitted_parts: u32,
}

/// What a Tool Call's row reads: the Tool, qualified by the MCP server hosting
/// it where there is one, then its input.
fn tool_call_label(server: Option<&str>, name: &str, input: &str) -> String {
    let tool = server.map_or_else(|| name.to_owned(), |server| format!("{server}/{name}"));
    if input.is_empty() {
        tool
    } else {
        format!("{tool} {input}")
    }
}

/// Projects a Tool Call through the same staged Fold a command's output folds
/// in: its Tool and input head it, the truncation marker for an input the cap
/// cut follows them, and a note counting the parts of its result that were not
/// text ends it.
fn push_tool_call_activity(
    projection: &mut ActivityProjection<'_>,
    activity: ToolCallActivity<'_>,
    step: FoldStep,
    theme: &Theme,
    width: u16,
) -> UnitAnchor {
    let ToolCallActivity {
        status,
        label,
        input_truncated,
        output,
        output_truncated,
        omitted_parts,
    } = activity;
    let mut details = Vec::new();
    if input_truncated {
        push_truncation_marker(
            &mut details,
            CappedStream::ToolCallInput,
            OUTPUT_DETAIL_INDENT,
            theme,
        );
    }
    let mut notes = Vec::new();
    if omitted_parts > 0 {
        push_annotation(
            &mut notes,
            &omitted_parts_note(omitted_parts),
            OUTPUT_DETAIL_INDENT,
            theme,
        );
    }
    push_output_fold(
        projection,
        OutputFold {
            status,
            header: label,
            header_suffix: String::new(),
            details,
            output,
            output_truncation: output_truncated.then_some(CappedStream::ToolCallOutput),
            notes,
        },
        step,
        theme,
        width,
    )
}

/// What a Tool Call's opened Fold says of the parts of its result that were not
/// text. The Activity keeps only how many there were, so that is all it says.
fn omitted_parts_note(omitted_parts: u32) -> String {
    match omitted_parts {
        1 => "1 non-text part not shown".to_owned(),
        count => format!("{count} non-text parts not shown"),
    }
}

/// An Activity whose Fold stages over the output it produced — a command's or
/// a Tool Call's. Each kind gathers its own header and what it says around its
/// output, so the stages, the tail, and the markers stay one presentation
/// while what each kind says stays its own.
struct OutputFold<'a> {
    status: crate::protocol::ActivityStatus,
    /// Clamped to one row while the Fold is closed, and wrapped whole once it
    /// opens.
    header: &'a str,
    /// Chrome the header keeps past its clamp.
    header_suffix: String,
    /// Lines the opened Fold shows beneath the header, ahead of the output.
    details: Vec<StyledLine>,
    output: &'a str,
    /// The stream whose truncation marker follows the output, when Suru's cap
    /// cut it short.
    output_truncation: Option<CappedStream>,
    /// Lines the opened Fold ends on, after the output and its marker.
    notes: Vec<StyledLine>,
}

/// Projects an [`OutputFold`] through its stages. Folded, a settled entry
/// keeps a single end-clamped row; its Peek brings the full header back over a
/// tail of output behind the fold marker; Expanded shows everything stored. A
/// still-streaming entry shows a live tail instead and speaks the binary
/// grammar, so a click opens the whole stream. Output is projected in full
/// first so hyperlink targets survive, and only then clamped, so a Fold changes
/// presentation and nothing else.
fn push_output_fold(
    projection: &mut ActivityProjection<'_>,
    fold: OutputFold<'_>,
    step: FoldStep,
    theme: &Theme,
    width: u16,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    let OutputFold {
        status,
        header,
        header_suffix,
        mut details,
        output,
        output_truncation,
        mut notes,
    } = fold;
    let (marker, style) = match status {
        ActivityStatus::Active => (spinner::MARKER, theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", theme.feedback.success),
        ActivityStatus::Failed => ("× ", theme.feedback.error),
        // Cut off by an interrupt: the face an interrupted Turn wears, so the
        // stop reads as a stop rather than as the work going wrong.
        ActivityStatus::Interrupted => ("× ", theme.feedback.warning),
    };
    let mut output_lines = Vec::new();
    if !output.is_empty() {
        push_styled_prefixed_lines(
            &mut ActivityProjection {
                lines: &mut output_lines,
                links: projection.links,
                strips: projection.strips,
            },
            ContentGutter {
                lead: OUTPUT_DETAIL_INDENT,
                indent: OUTPUT_DETAIL_INDENT,
            },
            output,
            theme.text.subdued,
            theme,
        );
    }
    if step == FoldStep::Folded {
        let mut anchor = push_folded_output_row(
            projection.lines,
            &format!("  {marker}"),
            header,
            &header_suffix,
            style,
            width,
            !details.is_empty()
                || !output.is_empty()
                || output_truncation.is_some()
                || !notes.is_empty(),
        );
        if status == ActivityStatus::Active {
            // Active entries use the same binary click grammar whether they
            // are still on this one-line shape or showing live output.
            anchor.fold = FoldDisclosure::Binary { folded: true };
        }
        return anchor;
    }
    let tail_rows = match (status, step) {
        (_, FoldStep::Expanded) => None,
        (ActivityStatus::Active, _) => Some(LIVE_TAIL_ROWS),
        (_, FoldStep::Peek) => Some(PEEK_OUTPUT_ROWS),
        (_, FoldStep::Folded) => unreachable!("a folded entry returned above"),
    };
    let header_start = projection.lines.len();
    let header_prefix = format!("  {marker}");
    let header_indent = " ".repeat(header_prefix.width());
    push_prefixed_lines_with_indent(
        projection.lines,
        &header_prefix,
        &header_indent,
        header,
        style,
    );
    if !header_suffix.is_empty()
        && let Some(header_line) = projection.lines.last_mut()
    {
        header_line
            .spans
            .push(StyledSpan::chrome(header_suffix, style));
    }
    let header_source_lines = projection.lines.len() - header_start;
    projection.lines.append(&mut details);
    let mut hides_content = false;
    let mut marker_source_line = None;
    if !output_lines.is_empty() {
        if let Some(tail_rows) = tail_rows {
            let (folded_lines, marked) = fold_output_to_tail(output_lines, tail_rows, theme, width);
            output_lines = folded_lines;
            if marked {
                hides_content = true;
                marker_source_line = Some(projection.lines.len());
            }
        }
        projection.lines.append(&mut output_lines);
    }
    if let Some(stream) = output_truncation {
        push_truncation_marker(projection.lines, stream, OUTPUT_DETAIL_INDENT, theme);
    }
    projection.lines.append(&mut notes);
    UnitAnchor {
        header_source_lines,
        hides_content,
        // A still-streaming entry's tail is not a staged Fold the reader
        // chose, so one click still opens the whole stream.
        fold: if status == ActivityStatus::Active {
            FoldDisclosure::Binary {
                folded: step != FoldStep::Expanded,
            }
        } else {
            FoldDisclosure::Staged(step)
        },
        marker_source_line,
    }
}

/// The first line of `text` fitted to `budget` columns, cut short with an
/// ellipsis where it runs longer or more lines follow it, and whether it was.
fn clamp_to_one_row(text: &str, budget: usize) -> (String, bool) {
    let first_line = text.lines().next().unwrap_or_default();
    let has_more_lines = text.lines().nth(1).is_some();
    if !has_more_lines && first_line.width() <= budget {
        return (first_line.to_owned(), false);
    }
    let mut clipped = String::new();
    let mut used = 0;
    let target = budget.saturating_sub(1);
    for character in first_line.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns > target {
            break;
        }
        clipped.push(character);
        used += columns;
    }
    (format!("{}…", clipped.trim_end()), true)
}

/// Projects the single row a folded [`OutputFold`] keeps: its Marker and
/// header — a command, or a Tool Call's Tool and input — end-clamped to the
/// width with an ellipsis instead of wrapping. `suffix` — the exit status of a
/// failed command — keeps its place past the clamp, so failure stays legible
/// however long the header was. The clamp is presentation only — the Peek
/// brings the full header back — so it reports as hidden content like
/// everything else the Fold holds.
fn push_folded_output_row(
    lines: &mut Vec<StyledLine>,
    prefix: &str,
    command: &str,
    suffix: &str,
    style: Style,
    width: u16,
    hides_more: bool,
) -> UnitAnchor {
    let budget = usize::from(width).saturating_sub(prefix.width() + suffix.width());
    let (text, clamped) = clamp_to_one_row(&sanitize_content(command), budget);
    let mut row = vec![
        StyledSpan::chrome(prefix, style),
        StyledSpan::text(text, style),
    ];
    if !suffix.is_empty() {
        row.push(StyledSpan::chrome(suffix, style));
    }
    lines.push(StyledLine::from(row));
    UnitAnchor {
        header_source_lines: 1,
        hides_content: hides_more || clamped,
        fold: FoldDisclosure::Staged(FoldStep::Folded),
        marker_source_line: None,
    }
}

/// Clamps an entry's projected output to the tail its Fold shows, behind the
/// fold marker counting the source lines above it. Rows are counted after
/// wrapping so a handful of very long lines cannot flood the budget, while
/// the marker counts source lines, so the number a reader sees does not shift
/// when the terminal is resized. Reports whether a marker was drawn.
fn fold_output_to_tail(
    mut lines: Vec<StyledLine>,
    tail_rows: usize,
    theme: &Theme,
    width: u16,
) -> (Vec<StyledLine>, bool) {
    let rows_per_line = lines
        .iter()
        .map(|line| StyledLayout::new(line, width).row_count().max(1))
        .collect::<Vec<_>>();
    if rows_per_line.iter().sum::<usize>() <= tail_rows {
        return (lines, false);
    }
    let mut tail_start = lines.len();
    let mut tail_used = 0;
    while tail_start > 0 && tail_used + rows_per_line[tail_start - 1] <= tail_rows {
        tail_used += rows_per_line[tail_start - 1];
        tail_start -= 1;
    }
    let remaining_rows = tail_rows.saturating_sub(tail_used);
    let boundary_tail = if remaining_rows > 0 && tail_start > 0 {
        let source = &lines[tail_start - 1];
        let rows = StyledLayout::new(source, width).into_rows();
        let keep_from = rows.len().saturating_sub(remaining_rows);
        rows[keep_from..]
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut line = row_as_line(source, row);
                if index > 0 {
                    line.continuation = true;
                    line.omitted_prefix = source
                        .slice(rows[keep_from + index - 1].end..row.start)
                        .spans
                        .into_iter()
                        .filter(|span| !span.chrome)
                        .map(|span| span.content)
                        .collect();
                }
                line
            })
            .collect()
    } else {
        Vec::new()
    };
    let hidden = tail_start;
    let tail = lines.split_off(tail_start);
    lines.clear();
    lines.push(fold_marker_line(
        hidden,
        "lines",
        OUTPUT_DETAIL_INDENT,
        theme,
    ));
    lines.extend(boundary_tail);
    lines.extend(tail);
    (lines, true)
}

/// One wrapped row of `source` as a projected line of its own, for the rows
/// a Peek keeps of a wrapped line whose beginning it hides. The hanging
/// indent the row was drawn with becomes chrome, in the style of the leading
/// span it was copied from, and the row's text keeps its spans and chrome.
fn row_as_line(source: &StyledLine, row: &StyledRow) -> StyledLine {
    let mut spans = Vec::new();
    if row.indent > 0 {
        let style = source
            .spans
            .first()
            .map_or_else(Style::default, |span| span.style);
        spans.push(StyledSpan::chrome(" ".repeat(row.indent), style));
    }
    spans.extend(source.slice(row.start..row.end).spans);
    StyledLine::from(spans)
}

/// What a fold marker counts: how much this entry's Fold is holding back. Kept
/// apart from [`fold_marker_line`] so an entry whose Fold hides its whole body,
/// and so reports the count on its own header, still says it the one way.
fn fold_marker_text(hidden: usize, unit: &str) -> String {
    format!("+{hidden} {unit}")
}

/// Renders the fold marker: how much this entry's Fold hides, styled as the
/// affordance it is so a reader never reads it as the truncation marker, which
/// reports content Suru's storage cap dropped for good.
fn fold_marker_line(hidden: usize, unit: &str, indent: &str, theme: &Theme) -> StyledLine {
    // The whole marker is the Fold's affordance, indent included: nothing on
    // the row is text the reader would copy.
    StyledLine::chrome(
        format!("{indent}… {}", fold_marker_text(hidden, unit)),
        theme.action.primary,
    )
}

/// Renders the truncation marker as its own line, so a reader can tell Suru
/// dropped the rest rather than the Provider ending there.
fn push_truncation_marker(
    lines: &mut Vec<StyledLine>,
    stream: CappedStream,
    indent: &str,
    theme: &Theme,
) {
    push_annotation(lines, stream.truncation_marker(), indent, theme);
}

/// Renders a line Suru writes about an entry's stored content rather than any
/// of that content — a truncation marker, or a note of what a result left out
/// — styled from Suru's typed signal rather than from anything the stream
/// carried, so it never reads as the entry's own output.
fn push_annotation(lines: &mut Vec<StyledLine>, text: &str, indent: &str, theme: &Theme) {
    let style = theme.text.subdued.add_modifier(Modifier::ITALIC);
    lines.push(StyledLine::from(vec![
        StyledSpan::chrome(indent, style),
        StyledSpan::text(text, style),
    ]));
}

struct FileChangeActions {
    create: &'static str,
    delete: &'static str,
    rename: &'static str,
    edit: &'static str,
}

/// Shortens only paths spelled inside the Session's Workspace. No filesystem
/// reads: deleted files and symlink spellings retain the same presentation.
fn transcript_path<'a>(path: &'a Path, workspace: &Path) -> &'a Path {
    if !path.is_absolute() || !workspace.is_absolute() {
        return path;
    }
    let mut components = path.components();
    for base in workspace.components() {
        if !components
            .next()
            .is_some_and(|part| same_path_component(part, base))
        {
            return path;
        }
    }
    let relative = components.as_path();
    let mut depth = 0_usize;
    for component in relative.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                let Some(parent_depth) = depth.checked_sub(1) else {
                    return path;
                };
                depth = parent_depth;
            }
            _ => {}
        }
    }
    if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    }
}

/// Windows canonical roots use verbatim prefixes even when a Provider spells
/// the same drive or share ordinarily. Compare that prefix without filesystem IO.
fn same_path_component(left: Component<'_>, right: Component<'_>) -> bool {
    #[cfg(windows)]
    if let (Component::Prefix(left), Component::Prefix(right)) = (left, right) {
        use std::path::Prefix;

        let ordinary = |prefix| match prefix {
            Prefix::VerbatimDisk(drive) => Prefix::Disk(drive),
            Prefix::VerbatimUNC(server, share) => Prefix::UNC(server, share),
            other => other,
        };
        return ordinary(left.kind()) == ordinary(right.kind());
    }
    left == right
}

/// Projects a FileChange Activity as one compact row per changed file. A
/// folded entry lists the first few rows and reports the rest through the same
/// fold marker and the same per-entry Fold state a command Activity uses,
/// which is what makes those generic rather than specific to command output.
fn push_file_change_activity(
    lines: &mut Vec<StyledLine>,
    status: crate::protocol::ActivityStatus,
    changes: &[FileChange],
    folded: bool,
    theme: &Theme,
    width: u16,
    workspace: &Path,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    let (marker, style, actions) = match status {
        ActivityStatus::Active => (
            spinner::MARKER,
            theme.accent.primary,
            FileChangeActions {
                create: "Creating",
                delete: "Deleting",
                rename: "Renaming",
                edit: "Editing",
            },
        ),
        ActivityStatus::Completed => (
            "✓ ",
            theme.feedback.success,
            FileChangeActions {
                create: "Created",
                delete: "Deleted",
                rename: "Renamed",
                edit: "Edited",
            },
        ),
        ActivityStatus::Failed | ActivityStatus::Interrupted => (
            "× ",
            theme.feedback.error,
            FileChangeActions {
                create: "Failed to create",
                delete: "Failed to delete",
                rename: "Failed to rename",
                edit: "Failed to edit",
            },
        ),
    };
    let header_start = lines.len();
    let listed = if folded {
        changes.len().min(FOLDED_FILE_CHANGE_PATHS)
    } else {
        changes.len()
    };
    for change in &changes[..listed] {
        let (action, path) = match change {
            FileChange::Add { path } => (
                actions.create,
                transcript_path(path, workspace)
                    .to_string_lossy()
                    .into_owned(),
            ),
            FileChange::Delete { path } => (
                actions.delete,
                transcript_path(path, workspace)
                    .to_string_lossy()
                    .into_owned(),
            ),
            FileChange::Update {
                path,
                moved_to: Some(moved_to),
            } => (
                actions.rename,
                format!(
                    "{} → {}",
                    transcript_path(path, workspace).to_string_lossy(),
                    transcript_path(moved_to, workspace).to_string_lossy()
                ),
            ),
            FileChange::Update {
                path,
                moved_to: None,
            } => (
                actions.edit,
                transcript_path(path, workspace)
                    .to_string_lossy()
                    .into_owned(),
            ),
        };
        let marker_lead = format!("  {marker}");
        let path = sanitize_content(&path).replace('\n', " ");
        let row_width = usize::from(width);
        let row = if row_width == 0 {
            Vec::new()
        } else if row_width < marker_lead.width() {
            let mut compact = marker.trim_end().to_owned();
            if row_width > 1 {
                compact.push_str(&" ".repeat(row_width.saturating_sub(2)));
                compact.push('…');
            }
            vec![SlotText::new(compact, style)]
        } else {
            truncate_slot_text(
                vec![
                    SlotText::new(marker_lead.clone(), style),
                    SlotText::new(format!("{action} "), style),
                    SlotText::new(path, theme.text.subdued),
                ],
                row_width,
            )
        };
        // The Marker leads the row, so whatever the clamp left of it is the
        // row's first item and the only chrome on it.
        lines.push(StyledLine::from(
            row.into_iter()
                .enumerate()
                .map(|(index, item)| {
                    if index == 0
                        && (marker_lead.starts_with(&item.text) || row_width < marker_lead.width())
                    {
                        StyledSpan::chrome(item.text, item.style)
                    } else {
                        StyledSpan::text(item.text, item.style)
                    }
                })
                .collect::<Vec<_>>(),
        ));
    }
    let header_source_lines = lines.len() - header_start;
    let hidden = changes.len() - listed;
    if hidden > 0 {
        lines.push(fold_marker_line(hidden, "more", OUTPUT_INDENT, theme));
    }
    UnitAnchor::binary(header_source_lines, hidden > 0)
}

/// What a Reasoning Activity contributes to the transcript, gathered so the
/// renderer reads one subject rather than a row of loose parameters.
struct ReasoningActivity<'a> {
    status: crate::protocol::ActivityStatus,
    title: Option<&'a str>,
    content: &'a str,
    content_truncated: bool,
    duration_ms: Option<u64>,
}

impl<'a> ReasoningActivity<'a> {
    /// The Reasoning an Activity holds, or `None` for an Activity of another
    /// kind. Every member of a Reasoning Group is a Reasoning Activity,
    /// because the groupable kind is what gathered it; reading that back out
    /// as an Option keeps the guarantee a fact about the walk rather than a
    /// panic waiting in a renderer.
    fn of(activity: &'a Activity) -> Option<Self> {
        match activity {
            Activity::Reasoning {
                status,
                title,
                content,
                content_truncated,
                duration_ms,
                ..
            } => Some(Self {
                status: *status,
                title: title.as_deref(),
                content,
                content_truncated: *content_truncated,
                duration_ms: *duration_ms,
            }),
            _ => None,
        }
    }
}

/// The Marker cell a Reasoning row leads with, the word for the state it
/// stands in, and the style both are drawn in. A lone block's header reads its
/// own status here and a Group's row the state of the run it speaks for, so
/// the Marker keeps the one contract it is meant to have across both.
fn reasoning_marker(
    status: crate::protocol::ActivityStatus,
    theme: &Theme,
) -> (&'static str, &'static str, Style) {
    use crate::protocol::ActivityStatus;

    match status {
        ActivityStatus::Active => (
            spinner::MARKER,
            REASONING_ACTIVE_LABEL,
            theme.accent.primary,
        ),
        ActivityStatus::Completed => ("✓ ", REASONING_COMPLETED_LABEL, theme.text.subdued),
        ActivityStatus::Failed | ActivityStatus::Interrupted => {
            ("× ", REASONING_FAILED_LABEL, theme.feedback.error)
        }
    }
}

/// What a Reasoning header states before any count or duration: the word for
/// the state the block is in, and the heading the Provider led it with when
/// there is one. A lone block's header and a Group's row both open with this,
/// so the two cannot drift apart in wording.
fn reasoning_header_text(label: &str, title: Option<&str>) -> String {
    let mut header = label.to_owned();
    if let Some(title) = reasoning_heading(title) {
        header.push_str(": ");
        header.push_str(title);
    }
    header
}

/// The lines a Reasoning block's stored content projects: the summary the
/// Provider wrote, with subdued Markdown prose and flat Code Blocks in the
/// same subdued posture, followed by the truncation marker when the cap cut it
/// short. A lone block's Fold and a Group's expansion both open onto exactly this.
fn reasoning_body_lines(
    reasoning: &ReasoningActivity<'_>,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    subdued_prose_lines(
        reasoning.content,
        reasoning
            .content_truncated
            .then_some(CappedStream::Reasoning),
        theme,
        width,
        hyperlinks,
    )
}

/// Prose a Provider wrote beside the work rather than as the answer — a Reasoning block's
/// summary, a Compaction's — drawn in the Activity gutter as subdued Markdown, and followed by the
/// truncation marker of the stream it is when the cap cut it short.
fn subdued_prose_lines(
    content: &str,
    truncated: Option<CappedStream>,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    let content = sanitize_content(content);
    let content_width =
        width.saturating_sub(u16::try_from(OUTPUT_INDENT.width()).unwrap_or(u16::MAX));
    let mut body =
        markdown::render_reasoning_with_hyperlinks(&content, theme, content_width, hyperlinks)
            .into_iter()
            .map(|line| indented_reasoning_line(line, OUTPUT_INDENT, theme))
            .collect::<Vec<_>>();
    if let Some(stream) = truncated {
        push_truncation_marker(&mut body, stream, OUTPUT_INDENT, theme);
    }
    body
}

/// Projects a Reasoning Activity. Folded — the posture a Transcript leans to —
/// it is the single header line the reader skims past; expanded it opens into
/// the summary the Provider wrote, rendered as the Markdown it is but drained of
/// colour so Reasoning never competes with the answer it led to.
fn push_reasoning_activity(
    lines: &mut Vec<StyledLine>,
    activity: ReasoningActivity<'_>,
    folded: bool,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> UnitAnchor {
    let (marker, label, style) = reasoning_marker(activity.status, theme);
    let mut header = reasoning_header_text(label, activity.title);
    if let Some(duration_ms) = activity.duration_ms {
        header.push_str(" · ");
        header.push_str(&humanized_duration(duration_ms));
    }
    // The body is projected whether or not it will be shown, because a Fold
    // that hides all of it still has to say how many lines that is.
    let mut body = reasoning_body_lines(&activity, theme, width, hyperlinks);
    let header_start = lines.len();
    push_prefixed_lines(lines, &format!("  {marker}"), &header, style);
    // A Reasoning Fold hides the entry's whole body rather than the middle of
    // it, so its fold marker rides the header instead of standing on a line of
    // its own: the reader still learns how much is behind it, and the folded
    // form stays the single line it is meant to be.
    if folded && !body.is_empty() {
        let header_line = lines
            .last_mut()
            .expect("a Reasoning Activity always projects a header line");
        header_line.spans.push(StyledSpan::chrome(" · ", style));
        header_line.spans.push(StyledSpan::chrome(
            fold_marker_text(body.len(), "lines"),
            theme.action.primary,
        ));
    }
    let header_source_lines = lines.len() - header_start;
    if folded {
        return UnitAnchor::binary(header_source_lines, !body.is_empty());
    }
    lines.append(&mut body);
    UnitAnchor::binary(header_source_lines, false)
}

/// How much of a Subagent's description its row shows, in cells. A spawn
/// describes a Subagent in a few words, but a resume's row reads what the
/// resume asked for, which the delegating Agent may have written as a whole
/// paragraph where it gave no summary of it; the row shows the start and
/// leads into the child Session, where the whole Delegation stands.
const SUBAGENT_DESCRIPTION_CELLS: usize = 100;

/// Projects a Subagent Activity: the one row its spawner's Transcript carries
/// of the delegation. Its Marker leads — a Spinner while the Subagent works,
/// its outcome glyph once it settles — then the Subagent's name, what it was
/// asked to do — drawn as the one line the row is, however the Agent broke
/// it, and cut to [`SUBAGENT_DESCRIPTION_CELLS`] where it runs longer — and
/// its duration once the settle reported one. There is nothing to fold,
/// because the Subagent's work lives in the child Session the row stands for
/// rather than behind it.
fn push_subagent_activity(
    lines: &mut Vec<StyledLine>,
    status: crate::protocol::ActivityStatus,
    name: &str,
    description: &str,
    duration_ms: Option<u64>,
    theme: &Theme,
) -> UnitAnchor {
    let (marker, style) = subagent_marker(status, theme);
    let mut header = format!("Subagent: {name}");
    // Every run of whitespace in the description, line breaks included, is
    // one space: a break is no way past the row's allowance.
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if !description.is_empty() {
        header.push_str(": ");
        header.push_str(&super::slots::truncate_to_width(
            &description,
            SUBAGENT_DESCRIPTION_CELLS,
        ));
    }
    if let Some(duration_ms) = duration_ms {
        header.push_str(" · ");
        header.push_str(&humanized_duration(duration_ms));
    }
    let start = lines.len();
    push_prefixed_lines(lines, &format!("  {marker}"), &header, style);
    // The row hides nothing — it is the way into the child Session, so the
    // anchor exists to make its whole extent a press target.
    UnitAnchor::binary(lines.len() - start, false)
}

/// The Marker a Subsession's row wears: the way out of the Sidekick's
/// Transcript into a Session of its own.
const SUBSESSION_MARKER: &str = "↗ ";

/// Projects a Subsession Activity: the one row a Sidekick's Transcript
/// carries of a Session it began. Its Marker leads, then the Subsession's
/// Title and what it was first asked — drawn as the one line the row is,
/// however the Sidekick broke it, and cut to [`SUBAGENT_DESCRIPTION_CELLS`]
/// where it runs longer — which the row leaves out while the Title still
/// reads as the Prompt it began as. Nothing is folded and nothing settles:
/// the Subsession's work lives in its own Session, which the row leads into.
fn push_subsession_activity(
    lines: &mut Vec<StyledLine>,
    title: &str,
    prompt: &str,
    theme: &Theme,
) -> UnitAnchor {
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = one_line(title);
    let prompt = one_line(prompt);
    let mut header = format!("Subsession: {title}");
    if !prompt.is_empty() && prompt != title {
        header.push_str(": ");
        header.push_str(&super::slots::truncate_to_width(
            &prompt,
            SUBAGENT_DESCRIPTION_CELLS,
        ));
    }
    let start = lines.len();
    push_prefixed_lines(
        lines,
        &format!("  {SUBSESSION_MARKER}"),
        &header,
        theme.text.subdued,
    );
    // The row hides nothing — it is the way into the Subsession, so the
    // anchor exists to make its whole extent a press target.
    UnitAnchor::binary(lines.len() - start, false)
}

/// Projects a Watch Outcome: why the Agent started working again, told in the
/// Provider's own words where it gave some. Its Marker is the outcome glyph a
/// settled Subagent wears for the same result, and like a Status it is already
/// the whole of what it says, so there is nothing to fold. A Provider that gave
/// no summary still leaves the reader the Watch's description and how it
/// ended, rather than a Marker with nothing beside it.
fn push_watch_outcome_activity(
    lines: &mut Vec<StyledLine>,
    status: crate::protocol::WatchOutcomeStatus,
    description: &str,
    summary: Option<&str>,
    theme: &Theme,
) {
    use crate::protocol::{ActivityStatus, WatchOutcomeStatus};

    let (settled, ended) = match status {
        WatchOutcomeStatus::Completed => (ActivityStatus::Completed, "completed"),
        WatchOutcomeStatus::Failed => (ActivityStatus::Failed, "failed"),
        WatchOutcomeStatus::Stopped => (ActivityStatus::Interrupted, "stopped"),
    };
    let (marker, style) = subagent_marker(settled, theme);
    let summary = summary.filter(|summary| !summary.trim().is_empty());
    let text = match (summary, description.trim()) {
        (Some(summary), _) => summary.to_owned(),
        (None, "") => format!("Watch {ended}"),
        (None, description) => format!("\"{description}\" {ended}"),
    };
    push_prefixed_lines_with_indent(lines, &format!("  {marker}"), "    ", &text, style);
}

/// A Compaction as its row reads it.
struct CompactionActivity<'a> {
    status: crate::protocol::ActivityStatus,
    trigger: crate::protocol::CompactionTrigger,
    instructions: Option<&'a str>,
    before_tokens: Option<u64>,
    after_tokens: Option<u64>,
    error: Option<&'a str>,
    summary: Option<&'a str>,
    summary_truncated: bool,
}

/// Projects a Compaction: where the Provider replaced what the Agent remembers
/// with a summary. Its Marker is the one a Subagent wears for the same status,
/// and its text says how the context changed — the Context Fill before and
/// after, each side left out where nothing is known for it rather than
/// guessed at, and whether the Provider chose to compact on its own. What the
/// user asked it to keep, and what it left the Agent, stand behind a Fold:
/// folded, the posture a
/// Transcript leans to, the row stays the one line it is with its fold marker
/// riding it, as a Reasoning block's does. A Compaction with nothing behind
/// its row has no Fold, and reports no anchor, so a click on it records
/// nothing.
fn push_compaction_activity(
    lines: &mut Vec<StyledLine>,
    compaction: CompactionActivity<'_>,
    folded: bool,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Option<UnitAnchor> {
    use crate::protocol::{ActivityStatus, CompactionTrigger};

    let (marker, style) = subagent_marker(compaction.status, theme);
    let text = match compaction.status {
        ActivityStatus::Active => "Compacting context".to_owned(),
        ActivityStatus::Completed => {
            let mut text = "Compacted context".to_owned();
            if compaction.before_tokens.is_some() || compaction.after_tokens.is_some() {
                let side = |tokens: Option<u64>| tokens.map(super::usage::compact_count);
                let change = [
                    side(compaction.before_tokens),
                    Some("→".to_owned()),
                    side(compaction.after_tokens),
                ];
                text.push_str(" · ");
                text.push_str(&change.into_iter().flatten().collect::<Vec<_>>().join(" "));
            }
            if compaction.trigger == CompactionTrigger::Automatic {
                text.push_str(" (automatic)");
            }
            text
        }
        ActivityStatus::Failed => match compaction
            .error
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            Some(error) => format!("Compaction failed: {error}"),
            None => "Compaction failed".to_owned(),
        },
        ActivityStatus::Interrupted => "Compaction stopped".to_owned(),
    };
    let mut fold = compaction_fold_lines(&compaction, theme, width, hyperlinks);
    let header_start = lines.len();
    push_prefixed_lines_with_indent(lines, &format!("  {marker}"), "    ", &text, style);
    if fold.is_empty() {
        return None;
    }
    if folded {
        let header_line = lines
            .last_mut()
            .expect("a Compaction always projects its row");
        header_line.spans.push(StyledSpan::chrome(" · ", style));
        header_line.spans.push(StyledSpan::chrome(
            fold_marker_text(fold.len(), "lines"),
            theme.action.primary,
        ));
    }
    let header_source_lines = lines.len() - header_start;
    if !folded {
        lines.append(&mut fold);
    }
    Some(UnitAnchor::binary(header_source_lines, folded))
}

/// What a Compaction's Fold holds: the instructions the user asked it with,
/// in their own words and labelled as theirs, then — set apart by a blank
/// line where there are both — the summary the Provider left, drawn as the
/// subdued prose a Reasoning block's summary is and ended by its truncation
/// marker where the cap cut it. The Fold is everything the row stands for
/// beyond its one line, so a Compaction carrying either has one, and an
/// empty Fold is none at all.
fn compaction_fold_lines(
    compaction: &CompactionActivity<'_>,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) -> Vec<StyledLine> {
    let mut lines = compaction
        .instructions
        .filter(|instructions| !instructions.trim().is_empty())
        .map(|instructions| compaction_instructions_lines(instructions, theme))
        .unwrap_or_default();
    if let Some(summary) = compaction
        .summary
        .filter(|summary| !summary.trim().is_empty())
    {
        if !lines.is_empty() {
            lines.push(StyledLine::default());
        }
        lines.extend(subdued_prose_lines(
            summary,
            compaction
                .summary_truncated
                .then_some(CappedStream::CompactionSummary),
            theme,
            width,
            hyperlinks,
        ));
    }
    lines
}

/// The instructions a Compaction was asked with, as the user typed them: one
/// line per line they wrote, the first behind a label saying whose words
/// they are and the rest hung beneath it, so they never read as the
/// Provider's summary below them. They are the user's words rather than
/// Markdown, as a Prompt's text is.
fn compaction_instructions_lines(instructions: &str, theme: &Theme) -> Vec<StyledLine> {
    const LABEL: &str = "Instructions: ";
    let content = sanitize_content(instructions.trim());
    let hung = format!("{OUTPUT_INDENT}{}", " ".repeat(LABEL.width()));
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 {
                vec![
                    StyledSpan::chrome(OUTPUT_INDENT, theme.text.subdued),
                    StyledSpan::chrome(LABEL, theme.text.subdued),
                ]
            } else {
                vec![StyledSpan::chrome(hung.clone(), theme.text.subdued)]
            };
            let mut spans = lead;
            spans.push(StyledSpan::text(line, theme.text.primary));
            StyledLine::from(spans)
        })
        .collect()
}

/// The Marker a Subagent wears wherever it is listed — its Transcript row and
/// its Aside entry alike — with the style its row is drawn in: the Spinner
/// while it works, and its outcome glyph once it settles. A working Marker is
/// [`spinner::MARKER`], the Spinner's first frame; each draw patches the
/// current frame into it with [`spinner::overlay_frame`].
pub(super) fn subagent_marker(
    status: crate::protocol::ActivityStatus,
    theme: &Theme,
) -> (&'static str, Style) {
    use crate::protocol::ActivityStatus;

    match status {
        ActivityStatus::Active => (spinner::MARKER, theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", theme.text.subdued),
        ActivityStatus::Failed => ("× ", theme.feedback.error),
        // Stopped on request: the same face an interrupted Turn wears, so a
        // stop reads as a stop rather than as the Subagent going wrong.
        ActivityStatus::Interrupted => ("× ", theme.feedback.warning),
    }
}

/// Places already-styled Reasoning content in the Activity gutter.
fn indented_reasoning_line(mut line: StyledLine, indent: &str, theme: &Theme) -> StyledLine {
    if !line.spans.is_empty() {
        line.spans
            .insert(0, StyledSpan::chrome(indent, theme.text.subdued));
    }
    line
}

/// Renders a duration at the coarsest precision that still says something: a
/// span of seconds is not reported to the millisecond, and one of minutes is not
/// reported as hundreds of seconds. Every duration the Transcript states passes
/// through here — a Reasoning header and a Turn Fold marker alike — so the same
/// span never reads two ways.
pub(super) fn humanized_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    match seconds % 60 {
        0 => format!("{minutes}m"),
        remainder => format!("{minutes}m {remainder}s"),
    }
}

/// Projects a user Message, or a Prompt drawn as the one it will become: its
/// text with each binding in its kind's style, then its Attachments in the
/// same block — one dimmed line per Attachment, wrapped as its text is, or a
/// strip's reserved rows, which answer where they begin.
fn push_user_message(
    lines: &mut Vec<StyledLine>,
    content: &str,
    bindings: &TextBindings,
    attachments: &AttachmentRows,
    theme: &Theme,
    available_width: u16,
) -> Option<usize> {
    let content = sanitize_content(content);
    let surface = theme.surface.elevated.patch(theme.text.primary);
    let subdued = theme.surface.elevated.patch(theme.text.subdued);
    let accent = theme.surface.elevated.patch(theme.accent.primary);
    let bound = matches!(&content, std::borrow::Cow::Borrowed(_))
        .then(|| {
            bindings
                .recognized_in(&content)
                .map(|binding| {
                    let style = theme.surface.elevated.patch(binding.kind.style(theme));
                    (binding.span.clone(), style)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let block = MessageBlock {
        gutter: USER_MESSAGE_GUTTER,
        gutter_style: accent,
        surface,
    };
    let available_width = usize::from(available_width);
    push_message_block(
        lines,
        &content,
        block,
        |byte| {
            bound
                .iter()
                .find(|(span, _)| span.contains(&byte))
                .map_or(surface, |(_, style)| *style)
        },
        available_width,
    );
    match attachments {
        AttachmentRows::Lines(attachments) => {
            for line in attachments {
                push_message_block(
                    lines,
                    &sanitize_content(line),
                    block,
                    |_| subdued,
                    available_width,
                );
            }
            None
        }
        AttachmentRows::Strip(strip) => {
            let start = lines.len();
            // Until a thumbnail is ready their lines fill the strip's first
            // rows, cut to its height, and blank rows the rest; once one is,
            // every row is blank and the frame draws the strip over them.
            if !strip.draws_thumbnails() {
                for line in strip.lines() {
                    push_message_block(
                        lines,
                        &sanitize_content(line),
                        block,
                        |_| subdued,
                        available_width,
                    );
                }
                lines.truncate(start + usize::from(STRIP_ROWS));
            }
            while lines.len() < start + usize::from(STRIP_ROWS) {
                push_reserved_row(lines, available_width, block);
            }
            Some(start)
        }
    }
}

/// Projects what an Agent was asked by someone other than the user: a
/// Delegation, the instruction an Agent gave the Subagent whose Transcript
/// this is, or a Prompt a Sidekick sent on the user's behalf. It sits on the
/// surface a user Message does, being likewise what the Agent was asked, but
/// it opens with `heading`, a row naming who asked, and runs down a lighter
/// bar, so it is never mistaken for something the user said. It arrives
/// whole, so one the cap cut short ends with the same marker a capped agent
/// Message does.
fn push_attributed_message(
    lines: &mut Vec<StyledLine>,
    content: &str,
    truncated: bool,
    heading: &str,
    theme: &Theme,
    available_width: u16,
) {
    let content = sanitize_content(content);
    let surface = theme.surface.elevated.patch(theme.text.primary);
    let subdued = theme.surface.elevated.patch(theme.text.subdued);
    let block = MessageBlock {
        gutter: DELEGATION_GUTTER,
        gutter_style: subdued,
        surface,
    };
    let available_width = usize::from(available_width);
    push_message_block(
        lines,
        &sanitize_content(heading),
        block,
        |_| subdued,
        available_width,
    );
    push_message_block(lines, &content, block, |_| surface, available_width);
    if truncated {
        push_truncation_marker(lines, CappedStream::Message, "  ", theme);
    }
}

/// What a block of Message rows is drawn with: the bar every row opens with,
/// the style that bar is drawn in, and the surface the rows fill.
#[derive(Clone, Copy)]
struct MessageBlock {
    gutter: &'static str,
    gutter_style: Style,
    surface: Style,
}

/// Wraps `content` into rows of a Message block — the shape a user Message
/// and a Delegation share — styling each character by its byte offset, and
/// marking each row that wraps a written line rather than starting one, so a
/// copy joins what wrapping split.
fn push_message_block(
    lines: &mut Vec<StyledLine>,
    content: &str,
    block: MessageBlock,
    style_at: impl Fn(usize) -> Style,
    available_width: usize,
) {
    // The gutter takes two columns and the air at the right edge one, so what
    // is left is what a row of the Message is wrapped to.
    let content_width = available_width
        .saturating_sub(block.gutter.width() + USER_MESSAGE_RIGHT_MARGIN)
        .max(1);
    let layout = TextLayout::new(content, u16::try_from(content_width).unwrap_or(u16::MAX));
    let mut previous_end = 0;
    for row in layout.rows() {
        let mut segments = Vec::<(Style, String)>::new();
        for (offset, character) in row.text.char_indices() {
            let style = style_at(row.start + offset);
            match segments.last_mut() {
                Some((last_style, text)) if *last_style == style => text.push(character),
                _ => segments.push((style, character.to_string())),
            }
        }
        push_message_block_row(lines, segments, row.width, available_width, block);
        let projected = lines.last_mut().expect("the Message row was just pushed");
        projected.continuation = row.start > 0 && content.as_bytes()[row.start - 1] != b'\n';
        if projected.continuation {
            projected.omitted_prefix = content[previous_end..row.start].to_owned();
        }
        previous_end = row.start + row.text.len();
    }
}

/// One row a strip reserves: the block's gutter and surface, and nothing a
/// copy would take.
fn push_reserved_row(lines: &mut Vec<StyledLine>, available_width: usize, block: MessageBlock) {
    let padding = available_width.saturating_sub(block.gutter.width());
    lines.push(StyledLine::from(vec![
        StyledSpan::chrome(block.gutter, block.gutter_style),
        StyledSpan::chrome(" ".repeat(padding), block.surface),
    ]));
}

fn push_message_block_row(
    lines: &mut Vec<StyledLine>,
    segments: Vec<(Style, String)>,
    row_width: usize,
    available_width: usize,
    block: MessageBlock,
) {
    let padding = available_width.saturating_sub(block.gutter.width() + row_width);
    let mut spans = Vec::with_capacity(segments.len() + 2);
    spans.push(StyledSpan::chrome(block.gutter, block.gutter_style));
    if segments.is_empty() {
        spans.push(StyledSpan::text("", block.surface));
    }
    spans.extend(
        segments
            .into_iter()
            .map(|(style, text)| StyledSpan::text(text, style)),
    );
    spans.push(StyledSpan::chrome(" ".repeat(padding), block.surface));
    lines.push(StyledLine::from(spans));
}

fn push_agent_message(
    lines: &mut Vec<StyledLine>,
    content: &str,
    truncated: bool,
    theme: &Theme,
    width: u16,
    hyperlinks: bool,
) {
    let content = sanitize_content(content);
    let content_width = width.saturating_sub(2);
    for mut line in markdown::render_with_hyperlinks(&content, theme, content_width, hyperlinks) {
        if !line.spans.is_empty() {
            line.spans
                .insert(0, StyledSpan::chrome("  ", theme.text.primary));
        }
        lines.push(line);
    }
    if truncated {
        push_truncation_marker(lines, CappedStream::Message, "  ", theme);
    }
}

/// Projects `content` one line per source line, each opening with chrome:
/// `prefix` — a Marker or a gutter — on the first and `indent` on the rest.
fn push_prefixed_lines(lines: &mut Vec<StyledLine>, prefix: &str, content: &str, style: Style) {
    push_prefixed_lines_with_indent(lines, prefix, "  ", content, style);
}

fn push_prefixed_lines_with_indent(
    lines: &mut Vec<StyledLine>,
    prefix: &str,
    indent: &str,
    content: &str,
    style: Style,
) {
    let content = sanitize_content(content);
    for (index, line) in content.lines().enumerate() {
        let mut spans = vec![StyledSpan::chrome(
            if index == 0 { prefix } else { indent },
            style,
        )];
        spans.push(StyledSpan::text(line, style));
        lines.push(StyledLine::from(spans));
    }
}

/// What precedes an Activity's projected content: the label or gutter its
/// first line opens with, and the indent every line after it sits in.
#[derive(Clone, Copy, Debug)]
struct ContentGutter<'a> {
    lead: &'a str,
    indent: &'a str,
}

/// Projects Activity content one source line per rendered line, keeping the
/// style and link transitions the stream carried.
fn push_styled_prefixed_lines(
    projection: &mut ActivityProjection<'_>,
    gutter: ContentGutter<'_>,
    content: &str,
    base_style: Style,
    theme: &Theme,
) {
    let mut style = SgrStyle::new(base_style);
    let mut hyperlink_active: Option<String> = None;
    let mut spans = vec![StyledSpan::chrome(gutter.lead, base_style)];
    let mut line_has_content = false;

    for token in content_tokens(content) {
        match token {
            ContentToken::Text(text) if !text.is_empty() => {
                spans.push(
                    StyledSpan::text(
                        text,
                        activity_content_style(style.rendered, hyperlink_active.is_some(), theme),
                    )
                    .with_target(hyperlink_active.as_deref()),
                );
                line_has_content = true;
            }
            ContentToken::Sgr(sequence) => apply_sgr(&sequence, &mut style, base_style, theme),
            ContentToken::LinkStart(target) => {
                hyperlink_active =
                    super::clipboard::safe_hyperlink_target(&target).map(ToOwned::to_owned);
                if let Some(target) = &hyperlink_active {
                    projection.links.push(TranscriptLink {
                        target: target.clone(),
                    });
                }
            }
            ContentToken::LinkEnd => hyperlink_active = None,
            ContentToken::Tab => {
                spans.push(
                    StyledSpan::text(
                        "    ",
                        activity_content_style(style.rendered, hyperlink_active.is_some(), theme),
                    )
                    .with_target(hyperlink_active.as_deref()),
                );
                line_has_content = true;
            }
            ContentToken::LineBreak => {
                if !line_has_content {
                    spans.push(StyledSpan::text("", base_style));
                }
                projection
                    .lines
                    .push(StyledLine::from(std::mem::take(&mut spans)));
                spans.push(StyledSpan::chrome(gutter.indent, base_style));
                line_has_content = false;
            }
            ContentToken::Text(_) => {}
        }
    }
    if line_has_content {
        projection.lines.push(StyledLine::from(spans));
    }
}

#[derive(Clone, Copy, Debug)]
enum AnsiForeground {
    Normal(u16),
    Bright,
}

#[derive(Clone, Copy, Debug)]
struct SgrStyle {
    rendered: Style,
    ansi_foreground: Option<AnsiForeground>,
    bold_active: bool,
}

impl SgrStyle {
    fn new(base_style: Style) -> Self {
        Self {
            rendered: base_style,
            ansi_foreground: None,
            bold_active: false,
        }
    }

    fn reset(&mut self, base_style: Style) {
        *self = Self::new(base_style);
    }

    fn set_bold(&mut self, theme: &Theme) {
        self.bold_active = true;
        self.rendered = self.rendered.add_modifier(Modifier::BOLD);
        if let Some(AnsiForeground::Normal(index)) = self.ansi_foreground {
            self.rendered.fg = theme.ansi.color(index, true);
        }
    }

    fn reset_intensity(&mut self, theme: &Theme) {
        self.bold_active = false;
        self.rendered = self
            .rendered
            .remove_modifier(Modifier::BOLD | Modifier::DIM);
        if let Some(AnsiForeground::Normal(index)) = self.ansi_foreground {
            self.rendered.fg = theme.ansi.color(index, false);
        }
    }

    fn set_normal_foreground(&mut self, index: u16, theme: &Theme) {
        self.ansi_foreground = Some(AnsiForeground::Normal(index));
        self.rendered.fg = theme.ansi.color(index, self.bold_active);
    }

    fn set_bright_foreground(&mut self, index: u16, theme: &Theme) {
        self.ansi_foreground = Some(AnsiForeground::Bright);
        self.rendered.fg = theme.ansi.color(index, true);
    }

    fn clear_ansi_foreground(&mut self) {
        self.ansi_foreground = None;
    }
}

fn activity_content_style(style: Style, hyperlink_active: bool, theme: &Theme) -> Style {
    if hyperlink_active {
        style.patch(theme.markdown.link)
    } else {
        style
    }
}

fn apply_sgr(sequence: &str, style: &mut SgrStyle, base_style: Style, theme: &Theme) {
    let Some(parameters) = sgr_parameters(sequence) else {
        return;
    };
    let parameters: Vec<&str> = parameters.collect();
    let mut index = 0;
    while index < parameters.len() {
        if parameters[index].contains(':') {
            if parameters[index].starts_with("38:") {
                style.clear_ansi_foreground();
            }
            apply_colon_color(parameters[index], &mut style.rendered);
            index += 1;
            continue;
        }
        let Some(parameter) = sgr_parameter_code(parameters[index]) else {
            index += 1;
            continue;
        };
        match parameter {
            0 => style.reset(base_style),
            1 => style.set_bold(theme),
            2 => style.rendered = style.rendered.add_modifier(Modifier::DIM),
            3 => style.rendered = style.rendered.add_modifier(Modifier::ITALIC),
            4 => style.rendered = style.rendered.add_modifier(Modifier::UNDERLINED),
            7 => style.rendered = style.rendered.add_modifier(Modifier::REVERSED),
            22 => style.reset_intensity(theme),
            23 => style.rendered = style.rendered.remove_modifier(Modifier::ITALIC),
            24 => style.rendered = style.rendered.remove_modifier(Modifier::UNDERLINED),
            27 => style.rendered = style.rendered.remove_modifier(Modifier::REVERSED),
            30..=37 => style.set_normal_foreground(parameter - 30, theme),
            38 => {
                style.clear_ansi_foreground();
                index = apply_extended_color(&parameters, index, &mut style.rendered.fg);
                continue;
            }
            39 => {
                style.clear_ansi_foreground();
                style.rendered.fg = base_style.fg;
            }
            40..=47 => style.rendered.bg = theme.ansi.color(parameter - 40, false),
            48 => {
                index = apply_extended_color(&parameters, index, &mut style.rendered.bg);
                continue;
            }
            49 => style.rendered.bg = base_style.bg,
            90..=97 => style.set_bright_foreground(parameter - 90, theme),
            100..=107 => style.rendered.bg = theme.ansi.color(parameter - 100, true),
            _ => {}
        }
        index += 1;
    }
}

fn apply_extended_color(parameters: &[&str], index: usize, target: &mut Option<Color>) -> usize {
    match parameters
        .get(index + 1)
        .and_then(|parameter| sgr_parameter_code(parameter))
    {
        Some(5) => {
            if let Some(value) = parameters
                .get(index + 2)
                .and_then(|parameter| sgr_parameter_code(parameter))
                .and_then(|value| u8::try_from(value).ok())
            {
                *target = Some(Color::Indexed(value));
            }
            (index + 3).min(parameters.len())
        }
        Some(2) => {
            if let (Some(red), Some(green), Some(blue)) = (
                parameters.get(index + 2).copied().and_then(sgr_byte),
                parameters.get(index + 3).copied().and_then(sgr_byte),
                parameters.get(index + 4).copied().and_then(sgr_byte),
            ) {
                *target = Some(Color::Rgb(red, green, blue));
            }
            (index + 5).min(parameters.len())
        }
        Some(_) => (index + 2).min(parameters.len()),
        None => index + 1,
    }
}

fn apply_colon_color(parameter: &str, style: &mut Style) {
    let parameters: Vec<Option<u16>> = parameter
        .split(':')
        .map(|parameter| {
            if parameter.is_empty() {
                None
            } else {
                parameter.parse().ok()
            }
        })
        .collect();
    let [
        Some(color_target @ (38 | 48)),
        Some(color_kind),
        values @ ..,
    ] = parameters.as_slice()
    else {
        return;
    };
    let color = match color_kind {
        5 => values
            .first()
            .copied()
            .flatten()
            .and_then(|value| u8::try_from(value).ok())
            .map(Color::Indexed),
        2 => {
            let rgb = if values.len() >= 4 {
                &values[1..]
            } else {
                values
            };
            let (red, green, blue) = (
                rgb.first().copied().flatten(),
                rgb.get(1).copied().flatten(),
                rgb.get(2).copied().flatten(),
            );
            match (red, green, blue) {
                (Some(red), Some(green), Some(blue)) => {
                    match (u8::try_from(red), u8::try_from(green), u8::try_from(blue)) {
                        (Ok(red), Ok(green), Ok(blue)) => Some(Color::Rgb(red, green, blue)),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };
    match (*color_target, color) {
        (38, Some(color)) => style.fg = Some(color),
        (48, Some(color)) => style.bg = Some(color),
        _ => {}
    }
}

fn sgr_byte(parameter: &str) -> Option<u8> {
    parameter.parse().ok()
}

/// Lays out one projected source line at the view width. A line the cap
/// allows is one projected line with its rows; one wrapping past the cap is
/// split into pieces each within it, every piece after the first flagged as
/// the continuation it is.
fn layout_line(line: StyledLine, width: u16) -> Vec<(StyledLine, StyledLayout)> {
    let layout = StyledLayout::new(&line, width);
    if layout.row_count() <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        return vec![(line, layout)];
    }
    let continuation = line.continuation;
    let omitted_prefix = line.omitted_prefix.clone();
    let mut pieces = split_oversized_line(line, width);
    if let Some((first, _)) = pieces.first_mut() {
        first.omitted_prefix = omitted_prefix;
    }
    for (index, (piece, _)) in pieces.iter_mut().enumerate() {
        piece.continuation = continuation || index > 0;
    }
    pieces
}

/// Splits `line` into pieces each wrapping to at most
/// [`MAX_TRANSCRIPT_SOURCE_LINE_ROWS`] rows, each with its layout so nothing
/// has to be laid out again.
fn split_oversized_line(line: StyledLine, width: u16) -> Vec<(StyledLine, StyledLayout)> {
    let layout = StyledLayout::new(&line, width);
    if layout.row_count() <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        return vec![(line, layout)];
    }
    // Cut by column arithmetic in one pass, targeting half the row cap so
    // ordinary word-wrap waste still leaves each chunk under the cap. The
    // arithmetic is an estimate, so each chunk is verified once; a chunk a
    // pathological wrap pattern pushes past the cap falls back to bisection.
    let mut output = Vec::new();
    for chunk in split_line_at_column_budget(line, width) {
        let layout = StyledLayout::new(&chunk, width);
        if layout.row_count() <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
            output.push((chunk, layout));
        } else {
            bisect_oversized_line(chunk, width, &mut output);
        }
    }
    output
}

/// Splits a line at character boundaries whenever the running display width
/// reaches half the row cap's worth of columns. One pass over the content, so
/// the split stays linear in the line's length.
fn split_line_at_column_budget(line: StyledLine, width: u16) -> Vec<StyledLine> {
    let column_budget = (MAX_TRANSCRIPT_SOURCE_LINE_ROWS / 2)
        .saturating_mul(usize::from(width.max(1)))
        .max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    let mut columns = 0;
    for span in &line.spans {
        for character in span.content.chars() {
            let width = character.width().unwrap_or(0);
            if columns + width > column_budget && columns > 0 {
                chunks.push(line.slice(start..offset));
                start = offset;
                columns = 0;
            }
            offset += character.len_utf8();
            columns += width;
        }
    }
    if start < offset || chunks.is_empty() {
        chunks.push(line.slice(start..offset));
    }
    chunks
}

fn bisect_oversized_line(
    line: StyledLine,
    width: u16,
    output: &mut Vec<(StyledLine, StyledLayout)>,
) {
    let layout = StyledLayout::new(&line, width);
    if layout.row_count() <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push((line, layout));
        return;
    }
    let character_count = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    if character_count < 2 {
        output.push((line, layout));
        return;
    }
    let (left, right) = split_line_at_character_midpoint(line, character_count);
    bisect_oversized_line(left, width, output);
    bisect_oversized_line(right, width, output);
}

fn split_line_at_character_midpoint(
    line: StyledLine,
    character_count: usize,
) -> (StyledLine, StyledLine) {
    let split_byte = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars())
        .take(character_count / 2)
        .map(char::len_utf8)
        .sum();
    let bytes = line.spans.iter().map(|span| span.content.len()).sum();
    (line.slice(0..split_byte), line.slice(split_byte..bytes))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{
        style::{Color, Modifier, Style},
        text::Line,
    };
    use unicode_width::UnicodeWidthStr;

    use crate::{
        protocol::{
            Activity, ActivityId, ActivityStatus, Cost, FileChange, Message, MessageId,
            MessageRole, MessageStatus, ModelAvailability, PromptId, ReasoningVisibility, Session,
            SessionRevision, SessionSnapshot, SessionStatus, SessionTimestamp, ToolCallVisibility,
            TranscriptItem, Turn, TurnId, TurnStatus, Usage, Workspace,
        },
        theme::Theme,
    };

    use super::{
        ActivityProjection, ActivityVisibility, AttachmentPreviews, AttachmentRows, CappedStream,
        FoldStep, Grouping, MAX_TRANSCRIPT_SOURCE_LINE_ROWS, StyledLine, StyledSpan, TextBindings,
        TextPosition, TranscriptCache, TranscriptDisclosure, TranscriptFolds, TranscriptGroups,
        TranscriptTurnFolds, TranscriptView, UnitKey, UnitStart, layout_line, push_user_message,
        render_activity, render_message, split_oversized_line,
    };

    /// A reader who asked to see every kind a Setting may hide, so a test
    /// about how an entry renders is never about whether it renders at all.
    const SHOWING_EVERY_KIND: ActivityVisibility = ActivityVisibility {
        reasoning: ReasoningVisibility::Shown,
        tool_calls: ToolCallVisibility::Shown,
    };

    fn rendered_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| &*span.content).collect()
    }

    /// Everything a projected line draws, chrome and text alike.
    fn projected_text(line: &StyledLine) -> String {
        line.written_text()
    }

    /// The style a projected line's text is drawn in.
    fn line_style(line: &StyledLine) -> Style {
        line.spans
            .last()
            .map_or_else(Style::default, |span| span.style)
    }

    fn split_and_check(line: StyledLine, width: u16) -> Vec<StyledLine> {
        let original = projected_text(&line);
        let pieces = split_oversized_line(line, width);
        for (chunk, layout) in &pieces {
            assert!(
                layout.row_count() <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS,
                "a split chunk exceeds the row cap"
            );
            assert_eq!(
                layout.row_count(),
                super::StyledLayout::new(chunk, width).row_count(),
                "a chunk's layout is the layout of the chunk"
            );
        }
        let chunks: Vec<_> = pieces.into_iter().map(|(chunk, _)| chunk).collect();
        let reassembled = chunks.iter().map(projected_text).collect::<String>();
        assert_eq!(reassembled, original, "splitting must not lose content");
        chunks
    }

    #[test]
    fn hanging_indent_does_not_add_prefix_only_rows_to_a_long_word() {
        let laid_out = layout_line(
            StyledLine::text(
                format!("      line 4 {}", "x".repeat(200)),
                Style::default(),
            ),
            56,
        );
        assert_eq!(
            laid_out.len(),
            1,
            "a line within the row cap stays one line"
        );
        let rows = laid_out[0].1.rows();
        assert_eq!(
            rows.len(),
            5,
            "the first row has 56 columns and continuations have 50"
        );
        assert!(
            rows.iter().all(|row| {
                row.line.width() <= 56 && !rendered_text(&row.line).trim().is_empty()
            })
        );
    }

    #[test]
    fn oversized_unbroken_line_splits_into_chunks_under_the_row_cap() {
        let width = 26u16;
        let content = "x".repeat(usize::from(width) * (MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 4));
        let chunks = split_and_check(StyledLine::text(content, Style::default()), width);
        assert!(chunks.len() > 1, "an oversized line must split");
    }

    #[test]
    fn oversized_line_with_pathological_word_wrap_stays_under_the_row_cap() {
        // Alternating one-character and width-filling words maximize the rows
        // the wrap produces per column of content, stressing the arithmetic
        // estimate's margin.
        let width = 12u16;
        let word = "b".repeat(usize::from(width) - 1);
        let content = format!("a {word} ").repeat(MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 4);
        split_and_check(StyledLine::text(content, Style::default()), width);
    }

    #[test]
    fn oversized_wide_character_line_splits_at_character_boundaries() {
        let width = 13u16;
        let content = "\u{5b57}".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 2);
        split_and_check(StyledLine::text(content, Style::default()), width);
    }

    #[test]
    fn splitting_a_styled_oversized_line_preserves_span_styles_and_chrome() {
        let width = 20u16;
        let styled = Style::default().fg(Color::Rgb(1, 2, 3));
        let plain = "p".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS);
        let emphasized = "e".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS);
        let line = StyledLine::from(vec![
            StyledSpan::chrome("  ", Style::default()),
            StyledSpan::text(plain, Style::default()),
            StyledSpan::text(emphasized, styled),
        ]);
        let chunks = split_and_check(line, width);
        for chunk in &chunks {
            for span in &chunk.spans {
                if span.content.contains('p') {
                    assert_eq!((span.style, span.chrome), (Style::default(), false));
                }
                if span.content.contains('e') {
                    assert_eq!((span.style, span.chrome), (styled, false));
                }
            }
        }
        assert!(
            chunks[0].spans[0].chrome,
            "the indent the line opened with is still chrome after the split"
        );
    }

    #[test]
    fn pieces_the_cap_split_are_flagged_as_continuations_of_the_first() {
        let width = 26u16;
        let content = "x".repeat(usize::from(width) * (MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 4));
        let pieces = layout_line(StyledLine::text(content, Style::default()), width);
        assert!(pieces.len() > 1, "an oversized line must split");
        assert!(
            !pieces[0].0.continuation,
            "the first piece is the line's own beginning"
        );
        assert!(
            pieces[1..].iter().all(|(piece, _)| piece.continuation),
            "every later piece continues the line the cap split"
        );
        let unsplit = layout_line(StyledLine::text("short line", Style::default()), 80);
        assert!(
            unsplit.len() == 1 && !unsplit[0].0.continuation,
            "a line within the cap is not a continuation of anything"
        );
    }

    #[test]
    fn a_line_within_the_row_cap_is_not_split() {
        let chunks = split_and_check(StyledLine::text("short line", Style::default()), 80);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn activity_base_ansi_colors_follow_theme_palette() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show colors".to_owned(),
            cwd: None,
            output: "\x1b[31;44mnormal\x1b[91;104mbright".to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        };
        let mut theme = Theme::system();
        theme.ansi.normal.red = Color::Rgb(1, 2, 3);
        theme.ansi.normal.blue = Color::Rgb(4, 5, 6);
        theme.ansi.bright.red = Color::Rgb(7, 8, 9);
        theme.ansi.bright.blue = Color::Rgb(10, 11, 12);
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        let spans = &lines.last().expect("render command output").spans;
        let normal = spans
            .iter()
            .find(|span| span.content == "normal")
            .expect("render normal ANSI colors");
        assert_eq!(normal.style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(normal.style.bg, Some(Color::Rgb(4, 5, 6)));
        let bright = spans
            .iter()
            .find(|span| span.content == "bright")
            .expect("render bright ANSI colors");
        assert_eq!(bright.style.fg, Some(Color::Rgb(7, 8, 9)));
        assert_eq!(bright.style.bg, Some(Color::Rgb(10, 11, 12)));
    }

    #[test]
    fn bold_promotes_only_normal_ansi_foregrounds_to_bright_palette() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show bold colors".to_owned(),
            cwd: None,
            output: concat!(
                "\x1b[1;31mcolor after bold ",
                "\x1b[22mnormal after 22 ",
                "\x1b[1mbold after color ",
                "\x1b[22;91mexplicit bright\x1b[22m stays bright ",
                "\x1b[0;1;41mbold background"
            )
            .to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        };
        let mut theme = Theme::system();
        theme.ansi.normal.red = Color::Rgb(1, 0, 0);
        theme.ansi.bright.red = Color::Rgb(2, 0, 0);
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        let spans = &lines.last().expect("render command output").spans;
        let style_for = |content| {
            spans
                .iter()
                .find(|span| span.content == content)
                .unwrap_or_else(|| panic!("render {content:?}"))
                .style
        };
        assert_eq!(style_for("color after bold ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("normal after 22 ").fg, Some(Color::Rgb(1, 0, 0)));
        assert_eq!(style_for("bold after color ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("explicit bright").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for(" stays bright ").fg, Some(Color::Rgb(2, 0, 0)));
        assert_eq!(style_for("bold background").bg, Some(Color::Rgb(1, 0, 0)));
    }

    #[test]
    fn transcript_cache_recolors_answer_code_and_keeps_reasoning_subdued() {
        let content = "```rust\nlet value = 42;\n```";
        let activity = reasoning(ActivityStatus::Completed, None, content);
        let snapshot = transcript_snapshot(vec![
            Entry::Message(agent_message(content)),
            Entry::Activity(activity.clone()),
        ]);
        let cache = TranscriptCache::default();
        let mut folds = TranscriptFolds::default();
        folds.expand(activity.id());
        let mut first_theme = Theme::system();
        first_theme.syntax.keyword.fg = Some(Color::Rgb(1, 2, 3));
        let first = cache.view(
            0,
            &snapshot,
            &[],
            TranscriptDisclosure {
                folds: &folds,
                groups: &TranscriptGroups::default(),
                turns: &TranscriptTurnFolds::default(),
                visibility: SHOWING_EVERY_KIND,
                grouping: Grouping::Off,
            },
            &first_theme,
            80,
        );
        let first_lines = first.window(0, 40).rows;
        drop(first);
        let mut second_theme = first_theme;
        second_theme.syntax.keyword.fg = Some(Color::Rgb(4, 5, 6));

        let second = cache.view(
            0,
            &snapshot,
            &[],
            TranscriptDisclosure {
                folds: &folds,
                groups: &TranscriptGroups::default(),
                turns: &TranscriptTurnFolds::default(),
                visibility: SHOWING_EVERY_KIND,
                grouping: Grouping::Off,
            },
            &second_theme,
            80,
        );
        let second_lines = second.window(0, 40).rows;

        let keyword_colors = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.content == "let")
                .map(|span| span.style.fg)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keyword_colors(&first_lines),
            vec![Some(Color::Rgb(1, 2, 3))]
        );
        assert_eq!(
            keyword_colors(&second_lines),
            vec![Some(Color::Rgb(4, 5, 6))]
        );
        let reasoning_style = first_theme
            .text
            .subdued
            .add_modifier(first_theme.markdown.code_block.add_modifier);
        for lines in [&first_lines, &second_lines] {
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| span.content.contains("let value = 42;")
                        && span.style == reasoning_style)
            );
        }
    }

    #[test]
    fn transcript_cache_rebuilds_when_only_the_theme_changes() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show theme".to_owned(),
            cwd: None,
            output: "\x1b[31mthemed output".to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        };
        let snapshot = transcript_snapshot(vec![Entry::Activity(activity.clone())]);
        let cache = TranscriptCache::default();
        let mut folds = TranscriptFolds::default();
        folds.expand(activity.id());
        let mut first_theme = Theme::system();
        first_theme.ansi.normal.red = Color::Rgb(1, 2, 3);
        let first = cache.view(
            0,
            &snapshot,
            &[],
            TranscriptDisclosure {
                folds: &folds,
                groups: &TranscriptGroups::default(),
                turns: &TranscriptTurnFolds::default(),
                visibility: SHOWING_EVERY_KIND,
                grouping: Grouping::Off,
            },
            &first_theme,
            80,
        );
        let first_lines = first.window(0, 10).rows;
        drop(first);
        let mut second_theme = first_theme;
        second_theme.ansi.normal.red = Color::Rgb(4, 5, 6);

        let second = cache.view(
            0,
            &snapshot,
            &[],
            TranscriptDisclosure {
                folds: &folds,
                groups: &TranscriptGroups::default(),
                turns: &TranscriptTurnFolds::default(),
                visibility: SHOWING_EVERY_KIND,
                grouping: Grouping::Off,
            },
            &second_theme,
            80,
        );
        let second_lines = second.window(0, 10).rows;

        let themed_color = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "themed output")
                .expect("render themed output")
                .style
                .fg
        };
        assert_eq!(themed_color(&first_lines), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(themed_color(&second_lines), Some(Color::Rgb(4, 5, 6)));
    }

    #[test]
    fn transcript_cache_rebuilds_when_only_the_fold_state_changes() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "emit many lines".to_owned(),
            cwd: None,
            output: (1..=20)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            output_truncated: false,
            exit_status: Some(0),
        };
        let snapshot = transcript_snapshot(vec![Entry::Activity(activity.clone())]);
        let theme = Theme::system();
        let cache = TranscriptCache::default();
        let mut folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();

        let folded_rows = cache
            .view(
                0,
                &snapshot,
                &[],
                TranscriptDisclosure {
                    folds: &folds,
                    groups: &groups,
                    turns: &TranscriptTurnFolds::default(),
                    visibility: SHOWING_EVERY_KIND,
                    grouping: Grouping::Off,
                },
                &theme,
                80,
            )
            .row_count();
        folds.expand(activity.id());
        let expanded_rows = cache
            .view(
                0,
                &snapshot,
                &[],
                TranscriptDisclosure {
                    folds: &folds,
                    groups: &groups,
                    turns: &TranscriptTurnFolds::default(),
                    visibility: SHOWING_EVERY_KIND,
                    grouping: Grouping::Off,
                },
                &theme,
                80,
            )
            .row_count();

        assert!(
            expanded_rows > folded_rows,
            "the Fold state is a rendering input, so flipping it alone must rebuild the view: \
             {folded_rows} rows folded, {expanded_rows} rows expanded"
        );
    }

    #[test]
    fn flipping_the_fold_posture_drops_the_overrides_taken_against_the_previous_one() {
        let expanded_by_hand = ActivityId::new();
        let untouched = ActivityId::new();
        let mut folds = TranscriptFolds::default();
        folds.expand(expanded_by_hand);
        assert!(!folds.is_folded(expanded_by_hand));
        assert!(folds.is_folded(untouched));

        folds.toggle_posture();

        assert!(
            !folds.is_folded(expanded_by_hand) && !folds.is_folded(untouched),
            "the expanded posture shows every entry, whatever the reader flipped before"
        );

        folds.fold(untouched);
        assert!(folds.is_folded(untouched));
        folds.toggle_posture();
        assert!(
            folds.is_folded(expanded_by_hand) && folds.is_folded(untouched),
            "flipping back folds everything again"
        );
    }

    #[test]
    fn flipping_the_group_posture_drops_the_overrides_taken_against_the_previous_one() {
        let expanded_by_hand = ActivityId::new();
        let untouched = ActivityId::new();
        let mut groups = TranscriptGroups::default();
        groups.expand(expanded_by_hand);
        assert!(!groups.is_collapsed(expanded_by_hand));
        assert!(groups.is_collapsed(untouched));

        groups.toggle_posture();

        assert!(
            !groups.is_collapsed(expanded_by_hand) && !groups.is_collapsed(untouched),
            "the expanded posture opens every Group, whatever the reader flipped before"
        );

        groups.collapse(untouched);
        assert!(groups.is_collapsed(untouched));
        groups.toggle_posture();
        assert!(
            groups.is_collapsed(expanded_by_hand) && groups.is_collapsed(untouched),
            "flipping back collapses everything again"
        );
    }

    #[test]
    fn command_output_truncation_marker_renders_apart_from_the_output() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "emit oversized output".to_owned(),
            cwd: None,
            output: "\x1b[31mkept output\x1b[0m".to_owned(),
            output_truncated: true,
            exit_status: Some(0),
        };
        let theme = Theme::system();
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Peek,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        let marker = lines.last().expect("render the truncation marker");
        assert_eq!(projected_text(marker), "      [output truncated]");
        assert!(
            line_style(marker).add_modifier.contains(Modifier::ITALIC),
            "the marker carries a style command output cannot: {marker:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "kept output"),
            "output before the marker still renders: {lines:?}"
        );
    }

    #[test]
    fn tool_call_markers_and_note_render_apart_from_its_output() {
        let activity = Activity::ToolCall {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            name: "fetch".to_owned(),
            server: Some("web".to_owned()),
            input: "url=https://example.com".to_owned(),
            input_truncated: true,
            output: "kept output".to_owned(),
            output_truncated: true,
            omitted_parts: 2,
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut Vec::new(),
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        assert_eq!(
            lines.iter().map(projected_text).collect::<Vec<_>>(),
            [
                "  ✓ web/fetch url=https://example.com",
                "      [input truncated]",
                "      kept output",
                "      [output truncated]",
                "      2 non-text parts not shown",
            ]
        );
        let output_style = line_style(&lines[2]);
        for annotation in [&lines[1], &lines[3], &lines[4]] {
            let style = line_style(annotation);
            assert!(
                style.add_modifier.contains(Modifier::ITALIC) && style != output_style,
                "what Suru writes about the result carries a style its output cannot: {annotation:?}"
            );
        }
    }

    #[test]
    fn expanded_reasoning_subdues_code_and_prose() {
        let activity = reasoning(
            ActivityStatus::Completed,
            None,
            "Read **this**.\n\n```rust\nstruct Widget;\n```",
        );
        let theme = Theme::system();
        let mut lines = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut Vec::new(),
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        let code_style = theme
            .text
            .subdued
            .add_modifier(theme.markdown.code_block.add_modifier);
        assert!(
            spans
                .iter()
                .any(|span| span.content.contains("struct Widget;") && span.style == code_style)
        );
        assert!(spans.iter().any(|span| span.content == "this"
            && span.style == theme.text.subdued.add_modifier(Modifier::BOLD)));
    }

    #[test]
    fn an_expanded_reasoning_block_ends_in_its_own_truncation_marker() {
        let activity = Activity::Reasoning {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            title: Some("Inspecting the seam".to_owned()),
            content: "Reading **the** projection.".to_owned(),
            content_truncated: true,
            duration_ms: Some(4_200),
        };
        let theme = Theme::system();
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        assert_eq!(
            projected_text(&lines[0]),
            "  ✓ Thought: Inspecting the seam · 4s"
        );
        let marker = lines.last().expect("render the truncation marker");
        assert_eq!(projected_text(marker), "    [Reasoning truncated]");
        let body = lines[1..lines.len() - 1]
            .iter()
            .map(projected_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body, "    Reading the projection.");
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.style.fg == theme.text.subdued.fg),
            "an expanded Reasoning body reads as subdued prose: {:?}",
            lines[1]
        );
    }

    #[test]
    fn agent_message_truncation_marker_renders_outside_the_markdown_body() {
        let message = Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: "```\nfenced code\n".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: true,
            author: None,
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_message(
            &mut lines,
            &message,
            None,
            &no_attachments(),
            &theme,
            80,
            false,
        );

        let marker = lines
            .iter()
            .find(|line| projected_text(line).contains("truncated]"))
            .expect("render the truncation marker");
        assert_eq!(
            projected_text(marker),
            "  [Message truncated]",
            "a capped Message ends with a marker that names a Message"
        );
        assert!(
            line_style(marker).add_modifier.contains(Modifier::ITALIC),
            "the marker keeps its own style outside the rendered Markdown: {marker:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("fenced code")),
            "Message content before the marker still renders: {lines:?}"
        );
    }

    #[test]
    fn command_output_ending_in_the_marker_text_renders_as_ordinary_output() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "echo the marker".to_owned(),
            cwd: None,
            output: format!("{}\n", CappedStream::CommandOutput.truncation_marker()),
            output_truncated: false,
            exit_status: Some(0),
        };
        let theme = Theme::system();
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Peek,
            &theme,
            80,
            false,
            std::path::Path::new(""),
        );

        let marker_lines = lines
            .iter()
            .filter(|line| {
                projected_text(line).contains(CappedStream::CommandOutput.truncation_marker())
            })
            .collect::<Vec<_>>();
        let [marker] = marker_lines.as_slice() else {
            panic!("output that reads like the marker renders once: {lines:?}");
        };
        assert!(
            !line_style(marker).add_modifier.contains(Modifier::ITALIC),
            "output the Provider sent keeps the style of command output: {marker:?}"
        );
        assert_eq!(projected_text(marker), "      [output truncated]");
    }

    #[test]
    fn agent_message_ending_in_the_marker_text_renders_as_ordinary_content() {
        let message = Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: format!("prose\n\n{}\n", CappedStream::Message.truncation_marker()),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_message(
            &mut lines,
            &message,
            None,
            &no_attachments(),
            &theme,
            80,
            false,
        );

        let marker_lines = lines
            .iter()
            .filter(|line| projected_text(line).contains(CappedStream::Message.truncation_marker()))
            .collect::<Vec<_>>();
        let [marker] = marker_lines.as_slice() else {
            panic!("content that reads like the marker renders once: {lines:?}");
        };
        assert!(
            !line_style(marker).add_modifier.contains(Modifier::ITALIC),
            "content the Provider sent renders as Markdown: {marker:?}"
        );
    }

    #[test]
    fn activity_projection_retains_osc_8_link_targets() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: "show targets".to_owned(),
            cwd: None,
            output: concat!(
                "\x1b]8;id=first;https://example.com/first\x07first\x1b]8;;\x07 ",
                "\x1b]8;;file:///tmp/second\x1b\\second\x1b]8;;\x1b\\"
            )
            .to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        };
        let mut lines = Vec::new();
        let mut links = Vec::new();

        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Expanded,
            &Theme::system(),
            80,
            false,
            std::path::Path::new(""),
        );

        assert_eq!(
            links
                .into_iter()
                .map(|link| link.target)
                .collect::<Vec<_>>(),
            ["https://example.com/first"]
        );
        assert_eq!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "first")
                .and_then(|span| span.target.as_deref()),
            Some("https://example.com/first")
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == "second")
                .is_some_and(|span| span.target.is_none()),
            "unsafe tool-output schemes do not reach OSC 8 or the opener"
        );
    }

    /// One entry a spacing fixture puts in the Transcript, in presentation
    /// order.
    enum Entry {
        Message(Message),
        Activity(Activity),
    }

    /// A Message presenting no Attachments, as one binding none does.
    fn no_attachments() -> AttachmentRows {
        AttachmentRows::Lines(Vec::new())
    }

    fn user_message(content: &str) -> Message {
        Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: content.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        }
    }

    fn agent_message(content: &str) -> Message {
        Message {
            role: MessageRole::Agent,
            ..user_message(content)
        }
    }

    fn command(command: &str, output: &str) -> Activity {
        Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            command: command.to_owned(),
            cwd: None,
            output: output.to_owned(),
            output_truncated: false,
            exit_status: Some(0),
        }
    }

    fn reasoning(status: ActivityStatus, title: Option<&str>, content: &str) -> Activity {
        Activity::Reasoning {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status,
            title: title.map(ToOwned::to_owned),
            content: content.to_owned(),
            content_truncated: false,
            duration_ms: None,
        }
    }

    fn error(text: &str) -> Activity {
        Activity::Error {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            text: text.to_owned(),
        }
    }

    fn status(text: &str) -> Activity {
        Activity::Status {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            text: text.to_owned(),
        }
    }

    /// Builds a Session snapshot whose Transcript holds `entries` in order, so
    /// a spacing test reads as the conversation it is describing.
    fn transcript_snapshot(entries: Vec<Entry>) -> SessionSnapshot {
        let mut messages = Vec::new();
        let mut activities = Vec::new();
        let mut transcript = Vec::new();
        for entry in entries {
            match entry {
                Entry::Message(message) => {
                    transcript.push(TranscriptItem::Message {
                        message_id: message.id,
                    });
                    messages.push(message);
                }
                Entry::Activity(activity) => {
                    transcript.push(TranscriptItem::Activity {
                        activity_id: activity.id(),
                    });
                    activities.push(activity);
                }
            }
        }
        SessionSnapshot {
            title: String::new(),
            icon: None,
            session: Session {
                checkout: None,
                context_fill: None,
                id: crate::protocol::SessionId::new(),
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: PathBuf::from("/workspace"),
                },
                workspace: Workspace::directory(PathBuf::from("/workspace")),
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                approval_posture: None,
                status: SessionStatus::Idle,
                working_since: None,
                monitoring_since: None,
                parent: None,
                begun_by: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: Vec::new(),
            messages,
            activities,
            transcript,
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            waiting_on_subagents: None,
            subagent_usage: None,
            total_cost: None,
            own_cost: None,
            attachments: Vec::new(),
        }
    }

    /// The text of each projected row, trailing padding trimmed, which is what
    /// a spacing test is asserting about.
    fn row_text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(rendered_text)
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    /// Projects the whole Transcript as the text of each row, with every Turn
    /// at the posture a fresh view leans to.
    fn projected_rows(
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
    ) -> Vec<String> {
        projected_rows_through(snapshot, folds, groups, &TranscriptTurnFolds::default())
    }

    /// Projects the whole Transcript as the text of each row, through the Turn
    /// Fold state a test drove.
    fn projected_rows_through(
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
        turns: &TranscriptTurnFolds,
    ) -> Vec<String> {
        let cache = TranscriptCache::default();
        let view = projected_view_through(&cache, snapshot, folds, groups, turns);
        let row_count = view.row_count();
        row_text(&view.window(0, row_count).rows)
    }

    #[test]
    fn an_agent_message_takes_a_blank_row_after_a_folded_command() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(command("cargo build", "")),
            Entry::Message(agent_message("Build is green.")),
        ]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(rows, ["  ✓ cargo build", "", "  Build is green."]);
    }

    #[test]
    fn a_run_of_compact_activities_stays_a_tight_list() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(command("cargo build", "")),
            Entry::Activity(status("Done")),
        ]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["  Preparing the workspace", "  ✓ cargo build", "  Done"]
        );
    }

    #[test]
    fn an_activity_with_a_body_takes_air_below_it_and_none_above() {
        let noisy = command("cargo test", "running 2 tests\nall green");
        let mut folds = TranscriptFolds::default();
        folds.expand(noisy.id());
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(noisy),
            Entry::Activity(status("Done")),
        ]);

        let rows = projected_rows(&snapshot, &folds, &TranscriptGroups::default());

        assert_eq!(
            rows,
            [
                "  Preparing the workspace",
                "  ✓ cargo test",
                "      running 2 tests",
                "      all green",
                "",
                "  Done",
            ]
        );
    }

    #[test]
    fn no_step_of_a_fold_moves_the_row_the_reader_clicked() {
        let output = (1..=12)
            .map(|line| format!("output line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let noisy = command("cargo test", &output);
        let noisy_id = noisy.id();
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(noisy),
            Entry::Activity(status("Done")),
        ]);
        let mut folds = TranscriptFolds::default();
        let header_row = |folds: &TranscriptFolds| {
            projected_rows(&snapshot, folds, &TranscriptGroups::default())
                .iter()
                .position(|row| row.contains("cargo test"))
                .expect("the command heads its unit at every step")
        };

        let folded = header_row(&folds);
        folds.set_step(noisy_id, FoldStep::Peek);
        let peeking = header_row(&folds);
        folds.set_step(noisy_id, FoldStep::Expanded);
        let expanded = header_row(&folds);

        assert_eq!(
            (folded, peeking, expanded),
            (1, 1, 1),
            "each step a reader clicks through must leave the header on the row they clicked, \
             directly under the compact Activity above it"
        );
    }

    #[test]
    fn an_error_takes_air_even_between_compact_activities() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(error("the Provider dropped the Turn")),
            Entry::Activity(status("Done")),
        ]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "  Preparing the workspace",
                "",
                "  Error: the Provider dropped the Turn",
                "",
                "  Done",
            ]
        );
    }

    #[test]
    fn a_compact_activity_wrapping_at_the_view_width_still_counts_as_one_line() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(status(&"wordy ".repeat(30))),
            Entry::Activity(command("cargo build", "")),
        ]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert!(
            !rows.contains(&String::new()),
            "vertical rhythm is counted in projected lines, not in wrapped rows, so a Status \
             long enough to wrap must not gain a separator: {rows:?}"
        );
    }

    #[test]
    fn an_expanded_group_header_stays_tight_to_its_first_member() {
        let first = command("cargo fmt", "reformatted 3 files\nreformatted 1 file");
        let second = command("cargo clippy", "");
        let mut groups = TranscriptGroups::default();
        groups.expand(first.id());
        let mut folds = TranscriptFolds::default();
        folds.expand(first.id());
        let snapshot = transcript_snapshot(vec![Entry::Activity(first), Entry::Activity(second)]);

        let rows =
            grouped_rows_through(&snapshot, &folds, &groups, &TranscriptTurnFolds::default());

        assert_eq!(
            rows,
            [
                "  ✓ Ran 2 commands",
                "    ✓ cargo fmt",
                "        reformatted 3 files",
                "        reformatted 1 file",
                "",
                "    ✓ cargo clippy",
            ]
        );
    }

    /// Reassigns an Activity to a Turn, so a fixture can state which Turn its
    /// entries belong to without spelling out every Activity kind.
    fn set_turn(activity: &mut Activity, turn_id: TurnId) {
        match activity {
            Activity::Approval { turn_id: id, .. }
            | Activity::Questionnaire { turn_id: id, .. }
            | Activity::Status { turn_id: id, .. }
            | Activity::Error { turn_id: id, .. }
            | Activity::Command { turn_id: id, .. }
            | Activity::FileChange { turn_id: id, .. }
            | Activity::ToolCall { turn_id: id, .. }
            | Activity::Reasoning { turn_id: id, .. }
            | Activity::Subagent { turn_id: id, .. }
            | Activity::WatchOutcome { turn_id: id, .. }
            | Activity::Compaction { turn_id: id, .. }
            | Activity::Subsession { turn_id: id, .. } => *id = turn_id,
        }
    }

    /// Builds a Session snapshot whose Transcript holds each Turn's entries in
    /// order, reporting the Turns in the same order, so a Turn Fold test reads
    /// as the conversation it is describing.
    fn turn_snapshot(turns: Vec<(TurnStatus, Vec<Entry>)>) -> (SessionSnapshot, Vec<TurnId>) {
        let mut snapshot = transcript_snapshot(Vec::new());
        let mut turn_ids = Vec::new();
        for (status, entries) in turns {
            let turn_id = TurnId::new();
            turn_ids.push(turn_id);
            let mut turn = transcript_snapshot(entries);
            for message in &mut turn.messages {
                message.turn_id = turn_id;
            }
            for activity in &mut turn.activities {
                set_turn(activity, turn_id);
            }
            snapshot.turns.push(Turn {
                id: turn_id,
                prompt_id: Some(PromptId::new()),
                compaction_requested: false,
                agent: None,
                status,
                started_at: None,
                settled_at: None,
                last_output_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
                cost_details: None,
            });
            snapshot.messages.append(&mut turn.messages);
            snapshot.activities.append(&mut turn.activities);
            snapshot.transcript.append(&mut turn.transcript);
        }
        (snapshot, turn_ids)
    }

    /// The entries a Turn Fold test hides: enough work to be worth folding
    /// away, and nothing that stays outside a fold.
    fn hidden_work() -> Vec<Entry> {
        vec![
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(command("cargo test", "running 2 tests\nall green")),
        ]
    }

    #[test]
    fn a_user_message_moves_a_whole_word_to_the_next_row() {
        let mut lines = Vec::new();
        push_user_message(
            &mut lines,
            "aaaa bbbbbb",
            &TextBindings::default(),
            &no_attachments(),
            &Theme::system(),
            12,
        );
        assert_eq!(
            lines.iter().map(projected_text).collect::<Vec<_>>(),
            ["\u{2503} aaaa      ", "\u{2503} bbbbbb    "],
            "a word that does not fit the room left moves down whole"
        );
    }

    #[test]
    fn a_user_message_keeps_a_column_of_air_at_its_right_edge() {
        let mut lines = Vec::new();
        push_user_message(
            &mut lines,
            "aaaaaaaaaaaa",
            &TextBindings::default(),
            &no_attachments(),
            &Theme::system(),
            12,
        );
        let rows = lines.iter().map(projected_text).collect::<Vec<_>>();
        assert_eq!(rows, ["\u{2503} aaaaaaaaa ", "\u{2503} aaa       "]);
        for row in &rows {
            assert_eq!(row.width(), 12, "every row fills the width it was given");
            assert!(
                row.ends_with(' '),
                "text stops a column short of the right edge: {row:?}"
            );
        }
    }

    #[test]
    fn a_settled_turn_folds_to_one_marker_between_its_prompt_and_its_answer() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        entries.push(Entry::Message(agent_message("All green.")));
        let (snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["┃ run the tests", "", "  ✓ Worked", "", "  All green.",],
            "a settled Turn reads as question, marker, answer"
        );
    }

    #[test]
    fn a_settled_continuation_folds_to_a_marker_and_its_answer_like_any_turn() {
        let mut first = vec![Entry::Message(user_message("run the tests"))];
        first.extend(hidden_work());
        first.push(Entry::Message(agent_message("All green.")));
        let mut continuation = hidden_work();
        continuation.push(Entry::Message(agent_message("Late findings.")));
        let (mut snapshot, _) = turn_snapshot(vec![
            (TurnStatus::Completed, first),
            (TurnStatus::Completed, continuation),
        ]);
        // A Continuation is the one kind of Turn without a Prompt or an
        // opening user Message; its entries still fold behind its marker.
        snapshot.turns[1].prompt_id = None;

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "┃ run the tests",
                "",
                "  ✓ Worked",
                "",
                "  All green.",
                "",
                "  ✓ Worked",
                "",
                "  Late findings.",
            ],
            "a Continuation reads as marker and answer, with no user Message before it"
        );
    }

    /// Stamps every Turn in a snapshot as having begun and settled a span
    /// apart, so a Turn Fold test states the duration it is about rather than
    /// two wall-clock timestamps.
    fn stamp_turn_durations(snapshot: &mut SessionSnapshot, duration_ms: u64) {
        const STARTED_AT: u64 = 1_755_000_000_000;
        for turn in &mut snapshot.turns {
            turn.started_at = Some(SessionTimestamp(STARTED_AT));
            turn.settled_at = Some(SessionTimestamp(STARTED_AT + duration_ms));
        }
    }

    #[test]
    fn a_turn_fold_marker_says_how_long_its_turn_worked() {
        for (status, duration_ms, marker) in [
            (TurnStatus::Completed, 83_000, "  ✓ Worked for 1m 23s"),
            (TurnStatus::Interrupted, 47_000, "  × Stopped after 47s"),
            (TurnStatus::Failed, 12_000, "  × Failed after 12s"),
        ] {
            let mut entries = vec![Entry::Message(user_message("run the tests"))];
            entries.extend(hidden_work());
            let (mut snapshot, _) = turn_snapshot(vec![(status, entries)]);
            stamp_turn_durations(&mut snapshot, duration_ms);

            let rows = projected_rows(
                &snapshot,
                &TranscriptFolds::default(),
                &TranscriptGroups::default(),
            );

            assert_eq!(
                rows,
                ["┃ run the tests", "", marker],
                "the marker states how long a {status:?} Turn worked"
            );
        }
    }

    #[test]
    fn a_turn_fold_marker_omits_tokens_and_cost_when_known() {
        let mut entries = vec![Entry::Message(user_message("measure this work"))];
        entries.extend(hidden_work());
        let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        stamp_turn_durations(&mut snapshot, 12_000);
        snapshot.turns[0].usage = Some(Usage {
            fresh_input_tokens: Some(1_200),
            cache_read_tokens: Some(8_000),
            cache_write_tokens: Some(400),
            output_tokens: Some(900),
            reasoning_tokens: Some(2_100),
            native_meter: None,
        });
        snapshot.turns[0].cost = Cost::from_usd(0.03);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["┃ measure this work", "", "  ✓ Worked for 12s",],
            "the marker shows duration without token count or cost"
        );
    }

    #[test]
    fn a_turn_fold_marker_hides_a_reported_zero_cost() {
        let mut entries = vec![Entry::Message(user_message("measure free work"))];
        entries.extend(hidden_work());
        let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        stamp_turn_durations(&mut snapshot, 12_000);
        snapshot.turns[0].cost = Cost::from_usd(0.0);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(rows, ["┃ measure free work", "", "  ✓ Worked for 12s"]);
    }

    #[test]
    fn a_turn_fold_markers_duration_reads_as_the_transcript_formats_every_other_one() {
        // The spans are the ones humanized_duration changes precision at, so a
        // marker that stopped sharing that formatter would fail here rather
        // than only where the two happen to agree.
        for (duration_ms, humanized) in [
            (900, "900ms"),
            (4_000, "4s"),
            (60_000, "1m"),
            (83_000, "1m 23s"),
        ] {
            let mut entries = vec![Entry::Message(user_message("run the tests"))];
            entries.extend(hidden_work());
            let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
            stamp_turn_durations(&mut snapshot, duration_ms);

            let rows = projected_rows(
                &snapshot,
                &TranscriptFolds::default(),
                &TranscriptGroups::default(),
            );

            assert_eq!(
                rows[2],
                format!("  ✓ Worked for {humanized}"),
                "a marker humanizes its duration the way every other duration in the \
                 Transcript is humanized"
            );
            assert_eq!(
                humanized,
                super::humanized_duration(duration_ms),
                "and it is the Transcript's own formatter that says so"
            );
        }
    }

    #[test]
    fn a_turn_fold_marker_falls_back_to_the_bare_word_when_only_one_timestamp_is_known() {
        for missing in ["started_at", "settled_at"] {
            let mut entries = vec![Entry::Message(user_message("run the tests"))];
            entries.extend(hidden_work());
            let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
            stamp_turn_durations(&mut snapshot, 83_000);
            match missing {
                "started_at" => snapshot.turns[0].started_at = None,
                _ => snapshot.turns[0].settled_at = None,
            }

            let rows = projected_rows(
                &snapshot,
                &TranscriptFolds::default(),
                &TranscriptGroups::default(),
            );

            assert_eq!(
                rows,
                ["┃ run the tests", "", "  ✓ Worked"],
                "a Turn missing its {missing} has no duration to state"
            );
        }
    }

    #[test]
    fn a_turn_fold_marker_states_no_duration_for_a_turn_that_settled_before_it_began() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        stamp_turn_durations(&mut snapshot, 83_000);
        let started_at = snapshot.turns[0]
            .started_at
            .expect("a timed Turn carries a start");
        snapshot.turns[0].settled_at = Some(SessionTimestamp(started_at.0 - 1));

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["┃ run the tests", "", "  ✓ Worked"],
            "a span that runs backwards is not a duration the marker can state"
        );
    }

    #[test]
    fn a_turn_fold_marker_without_timing_says_only_how_its_turn_settled() {
        for (status, marker) in [
            (TurnStatus::Completed, "  ✓ Worked"),
            (TurnStatus::Interrupted, "  × Stopped"),
            (TurnStatus::Failed, "  × Failed"),
        ] {
            let mut entries = vec![Entry::Message(user_message("run the tests"))];
            entries.extend(hidden_work());
            let (snapshot, _) = turn_snapshot(vec![(status, entries)]);

            let rows = projected_rows(
                &snapshot,
                &TranscriptFolds::default(),
                &TranscriptGroups::default(),
            );

            assert_eq!(
                rows,
                ["┃ run the tests", "", marker],
                "the marker states the outcome of a {status:?} Turn"
            );
        }
    }

    #[test]
    fn a_failed_turns_terminal_error_stays_outside_its_fold() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        entries.push(Entry::Activity(error("the Provider disconnected")));
        let (snapshot, _) = turn_snapshot(vec![(TurnStatus::Failed, entries)]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "┃ run the tests",
                "",
                "  × Failed",
                "",
                "  Error: the Provider disconnected",
            ],
            "the Error a Turn ended on is its outcome, so a fold never hides it"
        );
    }

    #[test]
    fn a_turn_whose_hidden_work_trailed_its_answer_still_marks_it_above_the_answer() {
        let mut entries = vec![
            Entry::Message(user_message("run the tests")),
            Entry::Message(agent_message("All green.")),
        ];
        entries.extend(hidden_work());
        let (snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["┃ run the tests", "", "  ✓ Worked", "", "  All green."],
            "the marker never falls past the answer the hidden work led to"
        );
    }

    #[test]
    fn a_failed_turns_error_stays_outside_its_fold_even_when_a_message_trails_it() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        entries.extend([
            Entry::Activity(error("the Provider disconnected")),
            Entry::Message(agent_message("I lost the connection.")),
        ]);
        let (snapshot, _) = turn_snapshot(vec![(TurnStatus::Failed, entries)]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "┃ run the tests",
                "",
                "  × Failed",
                "",
                "  Error: the Provider disconnected",
                "",
                "  I lost the connection.",
            ],
            "the Error a Turn failed on is its outcome whatever the Provider said afterwards"
        );
    }

    #[test]
    fn a_turn_that_hides_nothing_shows_no_marker() {
        let (snapshot, turn_ids) = turn_snapshot(vec![(
            TurnStatus::Completed,
            vec![
                Entry::Message(user_message("say hello")),
                Entry::Message(agent_message("Hello.")),
            ],
        )]);
        let mut turns = TranscriptTurnFolds::default();

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            ["┃ say hello", "", "  Hello."],
            "a marker that discloses nothing is noise"
        );

        turns.toggle(turn_ids[0]);
        let expanded = projected_rows_through(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
            &turns,
        );

        assert_eq!(
            expanded, rows,
            "a suppressed marker stays suppressed however the reader left the axis: there is \
             nothing for it to open onto"
        );
    }

    #[test]
    fn an_active_turn_never_folds() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        let (snapshot, _) = turn_snapshot(vec![(TurnStatus::Active, entries)]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "┃ run the tests",
                "",
                "  Preparing the workspace",
                "  ✓ cargo test",
            ],
            "the Turn a reader is watching keeps every entry it has produced"
        );
    }

    #[test]
    fn a_turn_fold_keeps_every_user_message_and_only_the_final_agent_message() {
        let (snapshot, _) = turn_snapshot(vec![(
            TurnStatus::Completed,
            vec![
                Entry::Message(user_message("start the migration")),
                Entry::Activity(command("cargo test", "")),
                Entry::Message(user_message("skip the slow suite")),
                Entry::Activity(status("Skipping the slow suite")),
                Entry::Message(agent_message("Working on it.")),
                Entry::Message(agent_message("Migration done.")),
            ],
        )]);

        let rows = projected_rows(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        assert_eq!(
            rows,
            [
                "┃ start the migration",
                "",
                "  ✓ Worked",
                "",
                "┃ skip the slow suite",
                "",
                "  Migration done.",
            ],
            "steers stay outside the fold and one marker stands for all the work"
        );
    }

    #[test]
    fn an_expanded_turns_disclosed_work_sits_in_the_markers_member_gutter() {
        let (snapshot, turn_ids) = turn_snapshot(vec![(
            TurnStatus::Completed,
            vec![
                Entry::Message(user_message("start the migration")),
                Entry::Activity(command("cargo test", "")),
                Entry::Message(agent_message("Working on it.")),
                Entry::Message(agent_message("Migration done.")),
            ],
        )]);
        let mut turns = TranscriptTurnFolds::default();
        turns.expand(turn_ids[0]);

        let rows = projected_rows_through(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
            &turns,
        );

        assert_eq!(
            rows,
            [
                "┃ start the migration",
                "",
                "  ✓ Worked",
                "    ✓ cargo test",
                "",
                "    Working on it.",
                "",
                "  Migration done.",
            ],
            "everything the marker disclosed sits in its member gutter — the interim \
             Message included — while the answer outside the fold keeps its own"
        );
    }

    #[test]
    fn expanding_a_turn_leaves_its_entries_at_their_own_fold_and_group_state() {
        let first = command("cargo fmt", "reformatted 1 file");
        let second = command("cargo clippy", "");
        let mut groups = TranscriptGroups::default();
        groups.expand(first.id());
        let mut folds = TranscriptFolds::default();
        folds.expand(first.id());
        let (snapshot, turn_ids) = turn_snapshot(vec![(
            TurnStatus::Completed,
            vec![
                Entry::Message(user_message("tidy the tree")),
                Entry::Activity(first),
                Entry::Activity(second),
                Entry::Message(agent_message("Tidied.")),
            ],
        )]);
        let mut turns = TranscriptTurnFolds::default();
        turns.expand(turn_ids[0]);

        let rows = grouped_rows_through(&snapshot, &folds, &groups, &turns);

        assert_eq!(
            rows,
            [
                "┃ tidy the tree",
                "",
                "  ✓ Worked",
                "    ✓ Ran 2 commands",
                "      ✓ cargo fmt",
                "          reformatted 1 file",
                "",
                "      ✓ cargo clippy",
                "",
                "  Tidied.",
            ],
            "a Turn Fold opens onto the Groups and Folds the reader left behind it, each \
             drawn in the member gutter of the marker it folds back into"
        );
    }

    #[test]
    fn one_turns_fold_leaves_the_turns_around_it_alone() {
        let mut first = vec![Entry::Message(user_message("run the tests"))];
        first.extend(hidden_work());
        first.push(Entry::Message(agent_message("All green.")));
        let mut second = vec![Entry::Message(user_message("now ship it"))];
        second.extend(hidden_work());
        second.push(Entry::Message(agent_message("Shipped.")));
        let (snapshot, turn_ids) = turn_snapshot(vec![
            (TurnStatus::Completed, first),
            (TurnStatus::Completed, second),
        ]);
        let mut turns = TranscriptTurnFolds::default();
        turns.expand(turn_ids[1]);

        let rows = projected_rows_through(
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
            &turns,
        );

        assert_eq!(
            rows,
            [
                "┃ run the tests",
                "",
                "  ✓ Worked",
                "",
                "  All green.",
                "",
                "┃ now ship it",
                "",
                "  ✓ Worked",
                "    Preparing the workspace",
                "    ✓ cargo test",
                "",
                "  Shipped.",
            ],
            "Turn Folds are per-Turn state, so opening one leaves its neighbours folded"
        );
    }

    #[test]
    fn the_transcript_cache_rebuilds_when_only_the_turn_fold_state_changes() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        entries.push(Entry::Message(agent_message("All green.")));
        let (snapshot, turn_ids) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        let cache = TranscriptCache::default();
        let folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();
        let mut turns = TranscriptTurnFolds::default();

        let folded_rows =
            projected_view_through(&cache, &snapshot, &folds, &groups, &turns).row_count();
        turns.expand(turn_ids[0]);
        let expanded_rows =
            projected_view_through(&cache, &snapshot, &folds, &groups, &turns).row_count();

        assert!(
            expanded_rows > folded_rows,
            "the Turn Fold state is a rendering input, so flipping it alone must rebuild the \
             view: {folded_rows} rows folded, {expanded_rows} rows expanded"
        );
    }

    #[test]
    fn the_transcript_marker_stays_unchanged_when_usage_changes() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        let (mut snapshot, _) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        stamp_turn_durations(&mut snapshot, 12_000);
        let cache = TranscriptCache::default();
        let folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();
        let turns = TranscriptTurnFolds::default();

        let before = projected_view_through(&cache, &snapshot, &folds, &groups, &turns);
        let before_rows = row_text(&before.window(0, before.row_count()).rows);
        drop(before);

        snapshot.revision = SessionRevision(snapshot.revision.0 + 1);
        snapshot.turns[0].usage = Some(Usage {
            fresh_input_tokens: Some(4_000),
            output_tokens: Some(200),
            ..Usage::default()
        });
        snapshot.turns[0].cost = Cost::from_usd(0.03);
        let after = projected_view_through(&cache, &snapshot, &folds, &groups, &turns);
        let after_rows = row_text(&after.window(0, after.row_count()).rows);

        assert_eq!(before_rows[2], "  ✓ Worked for 12s");
        assert_eq!(
            after_rows, before_rows,
            "usage and cost updates do not change the fold marker"
        );
    }

    #[test]
    fn refolding_the_turns_the_reader_opened_never_opens_one_they_closed() {
        let opened_by_hand = TurnId::new();
        let mut turns = TranscriptTurnFolds::default();
        turns.expand(opened_by_hand);

        turns.refold_expanded_turns();

        assert!(
            turns.is_folded(opened_by_hand),
            "under the folded posture, moving on folds the Turn the reader opened"
        );

        let folded_by_hand = TurnId::new();
        turns.toggle_posture();
        turns.toggle(folded_by_hand);
        assert!(turns.is_folded(folded_by_hand));

        turns.refold_expanded_turns();

        assert!(
            turns.is_folded(folded_by_hand),
            "compressing past work never opens a Turn: under the expanded posture the only \
             flips the reader can have taken are foldings, and those stand"
        );
    }

    #[test]
    fn flipping_the_turn_fold_posture_drops_the_overrides_taken_against_the_previous_one() {
        let expanded_by_hand = TurnId::new();
        let untouched = TurnId::new();
        let mut turns = TranscriptTurnFolds::default();
        turns.expand(expanded_by_hand);
        assert!(!turns.is_folded(expanded_by_hand));
        assert!(turns.is_folded(untouched));

        turns.toggle_posture();

        assert!(
            !turns.is_folded(expanded_by_hand) && !turns.is_folded(untouched),
            "the expanded posture opens every Turn, whatever the reader flipped before"
        );

        turns.toggle(untouched);
        assert!(turns.is_folded(untouched));
        turns.toggle_posture();
        assert!(
            turns.is_folded(expanded_by_hand) && turns.is_folded(untouched),
            "flipping back folds every Turn again"
        );
    }

    #[test]
    fn an_expanded_turn_keeps_the_marker_row_that_folds_it_back() {
        let mut entries = vec![Entry::Message(user_message("run the tests"))];
        entries.extend(hidden_work());
        entries.push(Entry::Message(agent_message("All green.")));
        let (snapshot, turn_ids) = turn_snapshot(vec![(TurnStatus::Completed, entries)]);
        let cache = TranscriptCache::default();
        let folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();
        let mut turns = TranscriptTurnFolds::default();
        let folded = turn_fold_start(
            &projected_view_through(&cache, &snapshot, &folds, &groups, &turns),
            turn_ids[0],
        );

        turns.toggle(turn_ids[0]);
        let expanded = turn_fold_start(
            &projected_view_through(&cache, &snapshot, &folds, &groups, &turns),
            turn_ids[0],
        );

        assert!(
            folded.hides_content && !expanded.hides_content,
            "the marker holds the Turn's work back folded and heads it expanded: \
             {folded:?} then {expanded:?}"
        );
        assert_eq!(
            folded.row, expanded.row,
            "expanding reveals the Turn's entries at the marker's position, so the row the \
             reader clicked never moves under them"
        );
    }

    #[test]
    fn toggling_one_turn_fold_leaves_every_other_turn_where_it_was() {
        let clicked = TurnId::new();
        let untouched = TurnId::new();
        let mut turns = TranscriptTurnFolds::default();

        turns.toggle(clicked);

        assert!(
            !turns.is_folded(clicked) && turns.is_folded(untouched),
            "a toggle answers for the one Turn it names"
        );

        turns.toggle(clicked);

        assert!(
            turns.is_folded(clicked),
            "toggling the same Turn again folds it back"
        );
    }

    /// The click target a Turn's marker projects, for the tests that read what
    /// a pointer landing on it would find.
    fn turn_fold_start(view: &TranscriptView, turn_id: TurnId) -> UnitStart {
        view.unit_starts()
            .iter()
            .copied()
            .find(|start| start.key == UnitKey::TurnFold(turn_id))
            .expect("a settled Turn with hidden work projects its marker")
    }

    /// Projects a Transcript and hands back the view, for the tests that read
    /// its row accounting rather than its text.
    fn projected_view<'a>(
        cache: &'a TranscriptCache,
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
    ) -> std::cell::Ref<'a, TranscriptView> {
        projected_view_through(
            cache,
            snapshot,
            folds,
            groups,
            &TranscriptTurnFolds::default(),
        )
    }

    /// Projects a Transcript through every disclosure axis a test drove, with
    /// grouping off, so a test about how an entry renders or spaces is never
    /// about the Group it would otherwise stand in.
    fn projected_view_through<'a>(
        cache: &'a TranscriptCache,
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
        turns: &TranscriptTurnFolds,
    ) -> std::cell::Ref<'a, TranscriptView> {
        projected_view_grouped(cache, snapshot, folds, groups, turns, Grouping::Off)
    }

    /// Projects a Transcript through every disclosure axis a test drove, with
    /// runs gathering into Groups or not as `grouping` says.
    fn projected_view_grouped<'a>(
        cache: &'a TranscriptCache,
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
        turns: &TranscriptTurnFolds,
        grouping: Grouping,
    ) -> std::cell::Ref<'a, TranscriptView> {
        cache.view(
            0,
            snapshot,
            &[],
            TranscriptDisclosure {
                folds,
                groups,
                turns,
                visibility: SHOWING_EVERY_KIND,
                grouping,
            },
            &Theme::system(),
            80,
        )
    }

    /// Projects the whole Transcript as the text of each row, with runs
    /// gathered into Groups under the Group and Turn Fold state a test drove.
    fn grouped_rows_through(
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
        turns: &TranscriptTurnFolds,
    ) -> Vec<String> {
        let cache = TranscriptCache::default();
        let view = projected_view_grouped(&cache, snapshot, folds, groups, turns, Grouping::Formed);
        let row_count = view.row_count();
        row_text(&view.window(0, row_count).rows)
    }

    #[test]
    fn hyperlink_capability_reprojects_wrapped_targets_and_invalidates_selection() {
        let snapshot = transcript_snapshot(vec![Entry::Message(agent_message(
            "[界wide label](https://example.test/path)",
        ))]);
        let cache = TranscriptCache::default();
        let folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();
        let turns = TranscriptTurnFolds::default();
        let disclosure = || TranscriptDisclosure {
            folds: &folds,
            groups: &groups,
            turns: &turns,
            visibility: SHOWING_EVERY_KIND,
            grouping: Grouping::Off,
        };

        let fallback = cache.view_with_hyperlinks(
            0,
            &snapshot,
            &[],
            disclosure(),
            &AttachmentPreviews::default(),
            &Theme::system(),
            8,
            false,
        );
        let fallback_rows = row_text(&fallback.window(0, fallback.row_count()).rows);
        assert!(
            fallback_rows
                .join("")
                .replace(' ', "")
                .contains("example.test"),
            "fallback rows: {fallback_rows:?}"
        );
        let old_epoch = cache.selection_epoch();
        drop(fallback);

        let supported = cache.view_with_hyperlinks(
            0,
            &snapshot,
            &[],
            disclosure(),
            &AttachmentPreviews::default(),
            &Theme::system(),
            8,
            true,
        );
        let window = supported.window(0, supported.row_count());
        assert_eq!(row_text(&window.rows), ["  界wide", "  label"]);
        assert!(window.hyperlinks.len() >= 2, "wrapped link lost a target");
        assert!(
            window
                .hyperlinks
                .iter()
                .all(|link| link.target == "https://example.test/path")
        );
        assert!(cache.selection_epoch() > old_epoch);
        assert_eq!(
            supported.hyperlink_at(0, 2).as_deref(),
            Some("https://example.test/path")
        );
        assert_eq!(
            supported.hyperlink_at(1, 2).as_deref(),
            Some("https://example.test/path")
        );
        assert_eq!(
            supported.hyperlink_at(0, 0),
            None,
            "message gutter is not a link"
        );
    }

    #[test]
    fn a_separator_row_belongs_to_no_units_click_extent() {
        let noisy = command("cargo test", "running 2 tests\nall green");
        let noisy_id = noisy.id();
        let mut folds = TranscriptFolds::default();
        folds.expand(noisy_id);
        let snapshot = transcript_snapshot(vec![
            Entry::Message(user_message("run the tests")),
            Entry::Activity(noisy),
        ]);
        let cache = TranscriptCache::default();

        let view = projected_view(&cache, &snapshot, &folds, &TranscriptGroups::default());

        let start = view
            .unit_starts()
            .iter()
            .find(|start| start.key == UnitKey::Activity(noisy_id))
            .expect("the expanded command anchors a click");
        assert_eq!(
            view.window(0, view.row_count())
                .rows
                .get(1)
                .map(rendered_text)
                .as_deref(),
            Some(""),
            "row 1 is the separator the boundary above the command took"
        );
        assert_eq!(start.row, 2, "the unit starts past its own separator");
        assert!(
            !start.contains(1),
            "a click on the separator row must resolve to no unit"
        );
    }

    #[test]
    fn a_message_anchor_points_past_the_separator_above_it() {
        let spoke = agent_message("Build is green.");
        let spoke_id = spoke.id;
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(command("cargo build", "")),
            Entry::Message(spoke),
        ]);
        let cache = TranscriptCache::default();

        let view = projected_view(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        let start = view
            .message_starts()
            .iter()
            .find(|start| start.message_id == spoke_id)
            .expect("the agent Message anchors the scroll position");
        assert_eq!(
            start.row, 2,
            "scrolling to a Message lands on the Message, not on the blank above it"
        );
    }

    #[test]
    fn a_window_opening_on_a_separator_draws_it_once() {
        let noisy = command("cargo test", "running 2 tests\nall green");
        let mut folds = TranscriptFolds::default();
        folds.expand(noisy.id());
        let snapshot = transcript_snapshot(vec![
            Entry::Message(user_message("run the tests")),
            Entry::Activity(noisy),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view(&cache, &snapshot, &folds, &TranscriptGroups::default());

        let opening_on_it = view.window(1, 2).rows;
        let opening_past_it = view.window(2, 2).rows;

        assert_eq!(row_text(&opening_on_it), ["", "  ✓ cargo test"]);
        assert_eq!(
            row_text(&opening_past_it),
            ["  ✓ cargo test", "      running 2 tests"]
        );
    }

    #[test]
    fn every_projected_row_is_reachable_by_scrolling_to_it() {
        let noisy = command("cargo test", "running 2 tests\nall green");
        let mut folds = TranscriptFolds::default();
        folds.expand(noisy.id());
        let snapshot = transcript_snapshot(vec![
            Entry::Message(user_message("run the tests")),
            Entry::Activity(status("Preparing the workspace")),
            Entry::Activity(noisy),
            Entry::Message(agent_message("All green.")),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view(&cache, &snapshot, &folds, &TranscriptGroups::default());

        let whole = view.window(0, view.row_count()).rows;

        assert_eq!(
            whole.len(),
            view.row_count(),
            "the reported row count must cover exactly the rows the window draws"
        );
        for (row, drawn) in whole.iter().enumerate() {
            let window = view.window(row, 1);
            assert_eq!(
                rendered_text(&window.rows[0]),
                rendered_text(drawn),
                "scrolling to row {row} must land on the same row the whole window draws"
            );
        }
    }

    /// Projects a Transcript at a width of the test's choosing.
    fn projected_view_at<'a>(
        cache: &'a TranscriptCache,
        snapshot: &SessionSnapshot,
        width: u16,
    ) -> std::cell::Ref<'a, TranscriptView> {
        cache.view(
            0,
            snapshot,
            &[],
            TranscriptDisclosure {
                folds: &TranscriptFolds::default(),
                groups: &TranscriptGroups::default(),
                turns: &TranscriptTurnFolds::default(),
                visibility: SHOWING_EVERY_KIND,
                grouping: Grouping::Off,
            },
            &Theme::system(),
            width,
        )
    }

    #[test]
    fn a_cell_resolves_to_the_line_and_offset_the_wrap_put_there() {
        let snapshot = transcript_snapshot(vec![Entry::Message(agent_message(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa",
        ))]);
        let cache = TranscriptCache::default();
        let width = 24;
        let view = projected_view_at(&cache, &snapshot, width);
        let rows = view.window(0, view.row_count()).rows;
        assert!(rows.len() > 2, "the Message wraps: {rows:?}");
        let line = view
            .projected_line(0)
            .expect("the Message projects a line")
            .written_text();

        for (row, drawn) in rows.iter().enumerate() {
            let drawn = rendered_text(drawn);
            let shown = drawn.trim();
            let indent = drawn.len() - drawn.trim_start().len();
            let start = view.position_at(row, indent).expect("a drawn row resolves");
            assert_eq!(
                start.line, 0,
                "every row of the Message belongs to its one line"
            );
            assert!(
                line[start.offset..].starts_with(shown),
                "row {row} begins at the offset of the text it shows: {shown:?} at {}",
                start.offset
            );
            let end = view
                .position_at(row, usize::from(width))
                .expect("a column past the text resolves");
            assert!(
                line[..end.offset].ends_with(shown),
                "a column past the row's text resolves past its last character"
            );
            let inside = view
                .position_at(row, indent + 2)
                .expect("a column inside the text resolves");
            assert_eq!(
                inside.offset,
                start.offset + 2,
                "a column in the text resolves to the character drawn there"
            );
        }
    }

    #[test]
    fn both_cells_of_a_wide_character_resolve_to_that_character() {
        let snapshot =
            transcript_snapshot(vec![Entry::Message(agent_message("\u{5b57}\u{5b57} wide"))]);
        let cache = TranscriptCache::default();
        let view = projected_view_at(&cache, &snapshot, 40);
        let text = view
            .projected_line(0)
            .expect("the Message projects a line")
            .written_text();
        let first = text.find('\u{5b57}').expect("the line holds the character");
        assert_eq!(
            (
                view.position_at(0, 2).expect("resolves").offset,
                view.position_at(0, 3).expect("resolves").offset,
                view.position_at(0, 4).expect("resolves").offset,
            ),
            (first, first, first + '\u{5b57}'.len_utf8()),
            "the Message's indent takes two columns, then each wide character two"
        );
    }

    #[test]
    fn a_separator_row_resolves_to_an_empty_line_of_its_own() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(command("cargo build", "")),
            Entry::Message(agent_message("Build is green.")),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view_at(&cache, &snapshot, 80);
        let rows = row_text(&view.window(0, view.row_count()).rows);
        assert_eq!(rows, ["  ✓ cargo build", "", "  Build is green."]);

        assert_eq!(
            view.position_at(1, 7),
            Some(TextPosition { line: 1, offset: 0 }),
            "the blank row between units is a line of its own"
        );
        let separator = view.projected_line(1).expect("the separator is a line");
        assert!(separator.is_empty() && !separator.continuation);
        assert_eq!(
            view.position_at(2, 4),
            Some(TextPosition {
                line: 2,
                offset: "  Bu".len()
            }),
            "lines after the separator count it"
        );
        assert_eq!(
            view.projected_line(2).map(projected_text).as_deref(),
            Some("  Build is green.")
        );
        assert_eq!(
            view.position_at(3, 0),
            None,
            "a row past the Transcript resolves to nothing"
        );
        assert!(view.projected_line(3).is_none());
    }

    #[test]
    fn rows_of_a_line_the_cap_split_resolve_to_its_continuations() {
        let width = 26u16;
        let content = "x".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 3);
        let snapshot = transcript_snapshot(vec![Entry::Message(agent_message(&content))]);
        let cache = TranscriptCache::default();
        let view = projected_view_at(&cache, &snapshot, width);
        assert!(
            view.row_count() > MAX_TRANSCRIPT_SOURCE_LINE_ROWS,
            "the Message wraps past the cap"
        );

        let opening = view.position_at(0, 2).expect("the first row resolves");
        assert!(
            !view
                .projected_line(opening.line)
                .expect("a line")
                .continuation,
            "the first row opens the written line"
        );
        let deep = view
            .position_at(view.row_count() - 1, 2)
            .expect("the last row resolves");
        assert!(
            deep.line > opening.line,
            "the cap split the line into pieces"
        );
        assert!(
            view.projected_line(deep.line).expect("a line").continuation,
            "a later piece continues the line the cap split"
        );
        assert!(
            (opening.line + 1..=deep.line)
                .all(|line| view.projected_line(line).expect("a line").continuation),
            "every piece after the first is a continuation"
        );
    }

    /// Each span as (chrome, content), which is what a copy reads off a line.
    fn span_marks(line: &StyledLine) -> Vec<(bool, &str)> {
        line.spans
            .iter()
            .map(|span| (span.chrome, span.content.as_str()))
            .collect()
    }

    #[test]
    fn a_commands_marker_gutters_prefixes_and_fold_marker_are_chrome() {
        let activity = Activity::Command {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Failed,
            command: "cargo build".to_owned(),
            cwd: Some("/work".into()),
            output: (1..=10)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            output_truncated: true,
            exit_status: Some(101),
        };
        let mut lines = Vec::new();
        let mut links = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &activity,
            FoldStep::Peek,
            &Theme::system(),
            80,
            false,
            std::path::Path::new(""),
        );

        assert_eq!(
            span_marks(&lines[0]),
            [
                (true, "  × "),
                (false, "cargo build"),
                (true, " (exit 101)")
            ],
            "the Marker and the exit suffix decorate the command"
        );
        assert_eq!(
            span_marks(&lines[1]),
            [(true, "      in "), (false, "/work")],
            "the workspace prefix decorates its path"
        );
        assert!(
            lines[2].spans.iter().all(|span| span.chrome)
                && projected_text(&lines[2]).contains("… +"),
            "a fold marker is chrome through and through: {:?}",
            lines[2]
        );
        assert_eq!(
            span_marks(&lines[3]),
            [(true, "      "), (false, "line 5")],
            "output keeps its gutter as chrome and its text as text"
        );
        let truncation = lines
            .last()
            .expect("the truncation marker closes the block");
        assert_eq!(
            span_marks(truncation),
            [(true, "      "), (false, "[output truncated]")]
        );
    }

    #[test]
    fn a_folded_command_keeps_its_ellipsis_as_text() {
        let mut lines = Vec::new();
        let mut links = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &command("cargo build --release --workspace --all-targets", ""),
            FoldStep::Folded,
            &Theme::system(),
            20,
            false,
            std::path::Path::new(""),
        );
        assert_eq!(
            span_marks(&lines[0]),
            [(true, "  ✓ "), (false, "cargo build --r…")]
        );
    }

    #[test]
    fn a_delegation_names_its_sender_against_the_subagents_parent() {
        let theme = Theme::system();
        let parent = crate::protocol::SessionId::new();
        let sibling = crate::protocol::SessionId::new();
        let heading = |sender: crate::protocol::SessionId, name: Option<&str>| {
            let message = Message {
                role: MessageRole::Delegation(crate::protocol::Delegator {
                    session_id: sender,
                    name: name.map(str::to_owned),
                }),
                ..user_message("Map the seams.")
            };
            let mut lines = Vec::new();
            render_message(
                &mut lines,
                &message,
                Some(parent),
                &no_attachments(),
                &theme,
                40,
                false,
            );
            assert_eq!(
                span_marks(&lines[1])[..2],
                [(true, "│ "), (false, "Map the seams.")],
                "the Delegation's bar is chrome and what it asked is text"
            );
            projected_text(&lines[0]).trim_end().to_owned()
        };

        assert_eq!(heading(parent, None), "│ Delegated by parent");
        assert_eq!(
            heading(parent, Some("Explore")),
            "│ Delegated by Explore (parent)"
        );
        assert_eq!(
            heading(sibling, Some("Reviewer")),
            "│ Delegated by Reviewer"
        );
        assert_eq!(
            heading(sibling, None),
            "│ Delegated by the top-level Agent",
            "an unnamed sender that is not the parent can only be a top-level Session's Agent"
        );
    }

    #[test]
    fn a_delegation_the_cap_cut_short_ends_with_the_message_truncation_marker() {
        let message = Message {
            role: MessageRole::Delegation(crate::protocol::Delegator {
                session_id: crate::protocol::SessionId::new(),
                name: None,
            }),
            truncated: true,
            ..user_message("Map the seams")
        };
        let mut lines = Vec::new();

        render_message(
            &mut lines,
            &message,
            None,
            &no_attachments(),
            &Theme::system(),
            40,
            false,
        );

        assert_eq!(
            lines.last().map(projected_text).as_deref(),
            Some("  [Message truncated]")
        );
    }

    #[test]
    fn message_gutters_and_padding_are_chrome_and_prose_is_text() {
        let theme = Theme::system();
        let mut lines = Vec::new();
        render_message(
            &mut lines,
            &user_message("hello there"),
            None,
            &no_attachments(),
            &theme,
            20,
            false,
        );
        assert_eq!(
            span_marks(&lines[0]),
            [(true, "┃ "), (false, "hello there"), (true, "       ")],
            "a user Message's bar and the air at its right edge are chrome"
        );

        let mut lines = Vec::new();
        render_message(
            &mut lines,
            &agent_message("- item one\n\n```rust\nlet x = 1;\n```"),
            None,
            &no_attachments(),
            &theme,
            40,
            false,
        );
        assert_eq!(
            span_marks(&lines[0]),
            [(true, "  "), (true, "• "), (false, "item one")],
            "the rendered bullet is decoration; copying obtains its marker from Markdown structure"
        );
        assert_eq!(
            span_marks(&lines[2]),
            [(true, "  "), (true, "rust")],
            "a Code Block's language label names the fence rather than the code"
        );
        assert!(
            lines[3].spans.iter().skip(1).all(|span| !span.chrome)
                && projected_text(&lines[3]).contains("let x = 1;"),
            "the code itself is text: {:?}",
            lines[3]
        );
    }

    #[test]
    fn markdown_tables_fit_inside_agent_and_reasoning_gutters() {
        let theme = Theme::system();
        let table = "| Name | Value |\n| --- | --- |\n| alpha beta gamma | delta |";

        let mut agent_lines = Vec::new();
        render_message(
            &mut agent_lines,
            &agent_message(table),
            None,
            &no_attachments(),
            &theme,
            24,
            false,
        );
        assert!(agent_lines.iter().all(|line| line.width() <= 24));
        assert_eq!(projected_text(&agent_lines[0]), "  ┌────────────┬───────┐");

        let mut reasoning_lines = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut reasoning_lines,
                links: &mut Vec::new(),
                strips: &mut Vec::new(),
            },
            &reasoning(ActivityStatus::Completed, None, table),
            FoldStep::Expanded,
            &theme,
            26,
            false,
            std::path::Path::new(""),
        );
        let body = &reasoning_lines[1..];
        assert!(body.iter().all(|line| line.width() <= 26));
        assert_eq!(projected_text(&body[0]), "    ┌────────────┬───────┐");
        assert!(body.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.fg == theme.text.subdued.fg
                && (span.content != "Name" || span.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn a_reasoning_headers_marker_and_fold_affordance_are_chrome() {
        let mut lines = Vec::new();
        let mut links = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &reasoning(ActivityStatus::Completed, Some("Plan"), "First.\n\nSecond."),
            FoldStep::Folded,
            &Theme::system(),
            80,
            false,
            std::path::Path::new(""),
        );
        assert_eq!(
            span_marks(&lines[0]),
            [
                (true, "  ✓ "),
                (false, "Thought: Plan"),
                (true, " · "),
                (true, "+3 lines")
            ]
        );

        let mut lines = Vec::new();
        render_activity(
            &mut ActivityProjection {
                lines: &mut lines,
                links: &mut links,
                strips: &mut Vec::new(),
            },
            &reasoning(ActivityStatus::Completed, Some("Plan"), "First.\n\nSecond."),
            FoldStep::Expanded,
            &Theme::system(),
            80,
            false,
            std::path::Path::new(""),
        );
        assert_eq!(
            span_marks(&lines[1]),
            [(true, "    "), (false, "First.")],
            "an opened block's prose sits behind a chrome indent"
        );
    }

    #[test]
    fn the_window_records_spinner_lines_for_active_markers_only() {
        let mut running = command("cargo build", "compiling suru");
        let Activity::Command {
            status,
            exit_status,
            ..
        } = &mut running
        else {
            unreachable!("the command helper builds a Command Activity");
        };
        *status = ActivityStatus::Active;
        *exit_status = None;
        let running_id = running.id();
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(command("cargo check", "")),
            Entry::Activity(running),
        ]);
        let cache = TranscriptCache::default();
        let mut folds = TranscriptFolds::default();
        folds.expand(running_id);
        let view = projected_view_through(
            &cache,
            &snapshot,
            &folds,
            &TranscriptGroups::default(),
            &TranscriptTurnFolds::default(),
        );

        let whole = view.window(0, view.row_count());
        assert_eq!(
            whole.spinner_rows.len(),
            1,
            "only the running command animates; the settled one keeps its outcome glyph"
        );
        let spinner_row = rendered_text(&whole.rows[whole.spinner_rows[0]]);
        assert!(
            spinner_row.contains(super::spinner::MARKER) && spinner_row.contains("cargo build"),
            "the recorded line is the running command's header: {spinner_row}"
        );

        let past_it = view.window(view.row_count().saturating_sub(1), 1);
        assert!(
            past_it.spinner_rows.is_empty(),
            "a window opening past the Marker records nothing to patch"
        );
    }

    #[test]
    fn transcript_file_paths_are_relative_only_inside_the_sessions_workspace() {
        let workspace = if cfg!(windows) {
            PathBuf::from(r"C:\w")
        } else {
            PathBuf::from("/w")
        };
        let outside = workspace.with_file_name("other").join("external.rs");
        let sibling = workspace.with_file_name("w-other").join("sibling.rs");
        let escape = workspace.join("..").join("escaped.rs");
        let activity = Activity::FileChange {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Completed,
            changes: vec![
                FileChange::Add {
                    path: workspace.join("new.rs"),
                },
                FileChange::Delete {
                    path: workspace.join("deleted.rs"),
                },
                FileChange::Update {
                    path: workspace.join("edited.rs"),
                    moved_to: None,
                },
                FileChange::Update {
                    path: workspace.join("old.rs"),
                    moved_to: Some(workspace.join("renamed.rs")),
                },
                FileChange::Update {
                    path: workspace.join("export.rs"),
                    moved_to: Some(outside.clone()),
                },
                FileChange::Add {
                    path: sibling.clone(),
                },
                FileChange::Add {
                    path: escape.clone(),
                },
                FileChange::Add {
                    path: PathBuf::from("relative.rs"),
                },
            ],
        };
        let mut folds = TranscriptFolds::default();
        folds.expand(activity.id());
        let mut snapshot = transcript_snapshot(vec![Entry::Activity(activity)]);
        snapshot.session.execution_directory.path = workspace;
        let stored = snapshot.clone();
        let cache = TranscriptCache::default();
        let view = projected_view(&cache, &snapshot, &folds, &TranscriptGroups::default());
        let rows: Vec<_> = view
            .window(0, view.row_count())
            .rows
            .iter()
            .map(rendered_text)
            .collect();
        assert_eq!(
            rows,
            vec![
                "  ✓ Created new.rs".to_owned(),
                "  ✓ Deleted deleted.rs".to_owned(),
                "  ✓ Edited edited.rs".to_owned(),
                "  ✓ Renamed old.rs → renamed.rs".to_owned(),
                format!("  ✓ Renamed export.rs → {}", outside.display()),
                format!("  ✓ Created {}", sibling.display()),
                format!("  ✓ Created {}", escape.display()),
                "  ✓ Created relative.rs".to_owned(),
            ]
        );
        assert_eq!(snapshot, stored, "display must preserve stored paths");
    }

    #[test]
    fn transcript_command_directories_are_relative_without_rewriting_content() {
        let workspace = if cfg!(windows) {
            PathBuf::from(r"C:\w")
        } else {
            PathBuf::from("/w")
        };
        let outside = workspace.with_file_name("other");
        for (cwd, expected) in [
            (workspace.clone(), ".".to_owned()),
            (workspace.join("src"), "src".to_owned()),
            (outside.clone(), outside.to_string_lossy().into_owned()),
            (PathBuf::from("relative"), "relative".to_owned()),
        ] {
            let text = workspace.join("file.rs").to_string_lossy().into_owned();
            let mut activity = command(&text, &text);
            if let Activity::Command { cwd: directory, .. } = &mut activity {
                *directory = Some(cwd);
            }
            let mut folds = TranscriptFolds::default();
            folds.expand(activity.id());
            let mut snapshot = transcript_snapshot(vec![
                Entry::Activity(activity),
                Entry::Message(agent_message(&text)),
            ]);
            snapshot.session.execution_directory.path = workspace.clone();
            let cache = TranscriptCache::default();
            let view = projected_view(&cache, &snapshot, &folds, &TranscriptGroups::default());
            let rows: Vec<_> = view
                .window(0, view.row_count())
                .rows
                .iter()
                .map(rendered_text)
                .collect();
            assert!(
                rows.iter()
                    .any(|row| row.trim() == format!("in {expected}")),
                "{rows:?}"
            );
            assert_eq!(
                rows.iter().filter(|row| row.contains(&text)).count(),
                3,
                "command, output, and prose retain their paths: {rows:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn transcript_paths_match_canonical_windows_workspace_prefixes() {
        for (workspace, path) in [
            (r"\\?\C:\w", r"C:\w\edited.rs"),
            (r"C:\w", r"\\?\C:\w\edited.rs"),
            (r"\\?\UNC\server\share\w", r"\\server\share\w\edited.rs"),
            (r"\\server\share\w", r"\\?\UNC\server\share\w\edited.rs"),
        ] {
            let activity = Activity::FileChange {
                id: ActivityId::new(),
                turn_id: TurnId::new(),
                status: ActivityStatus::Completed,
                changes: vec![FileChange::Update {
                    path: path.into(),
                    moved_to: None,
                }],
            };
            let mut snapshot = transcript_snapshot(vec![Entry::Activity(activity)]);
            snapshot.session.execution_directory.path = workspace.into();
            let cache = TranscriptCache::default();
            let view = projected_view(
                &cache,
                &snapshot,
                &TranscriptFolds::default(),
                &TranscriptGroups::default(),
            );
            let rows: Vec<_> = view
                .window(0, view.row_count())
                .rows
                .iter()
                .map(rendered_text)
                .collect();
            assert_eq!(
                rows,
                ["  ✓ Edited edited.rs"],
                "Workspace {workspace}, file {path}"
            );
        }
    }

    #[test]
    fn an_active_file_change_records_every_visible_row_for_spinner_animation() {
        let activity = Activity::FileChange {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Active,
            changes: (1..=5)
                .map(|index| FileChange::Add {
                    path: format!("src/file{index}.rs").into(),
                })
                .collect(),
        };
        let snapshot = transcript_snapshot(vec![Entry::Activity(activity)]);
        let cache = TranscriptCache::default();
        let view = projected_view(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        let window = view.window(0, view.row_count());
        assert_eq!(
            window.spinner_rows.len(),
            4,
            "each of the four visible folded file rows repeats the Activity Spinner"
        );
        for spinner_line in window.spinner_rows {
            let row = rendered_text(&window.rows[spinner_line]);
            assert!(
                row.contains(super::spinner::MARKER) && row.contains("Creating src/file"),
                "every recorded row carries the repeated live Marker and action: {row}"
            );
        }
    }

    #[test]
    fn a_file_change_row_clamps_its_action_and_path_together_at_any_width() {
        let activity = Activity::FileChange {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            status: ActivityStatus::Failed,
            changes: vec![FileChange::Update {
                path: "src/old.rs".into(),
                moved_to: Some("src/new.rs".into()),
            }],
        };
        for width in [1, 2, 3, 10] {
            let mut lines = Vec::new();
            let mut links = Vec::new();
            render_activity(
                &mut ActivityProjection {
                    lines: &mut lines,
                    links: &mut links,
                    strips: &mut Vec::new(),
                },
                &activity,
                FoldStep::Folded,
                &Theme::system(),
                width,
                false,
                std::path::Path::new(""),
            );

            assert_eq!(lines.len(), 1, "one File Change stays one source line");
            let row = projected_text(&lines[0]);
            assert!(
                row.width() <= usize::from(width),
                "the complete row, including its action, respects width {width}: {row}"
            );
            assert!(
                row.contains('×'),
                "truncation never consumes the Activity Marker at width {width}: {row}"
            );
            if width > 1 {
                assert!(
                    row.ends_with('…'),
                    "a clipped File Change reports the missing tail at width {width}: {row}"
                );
            }
        }
    }

    #[test]
    fn an_opened_live_reasoning_group_holds_no_air_for_a_section_that_has_not_spoken() {
        let settled = reasoning(ActivityStatus::Completed, Some("Reading"), "Read the plan.");
        let mut groups = TranscriptGroups::default();
        groups.expand(settled.id());
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(settled),
            Entry::Activity(reasoning(ActivityStatus::Active, None, "")),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view_grouped(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &groups,
            &TranscriptTurnFolds::default(),
            Grouping::Formed,
        );

        let whole = view.window(0, view.row_count());
        let rows = whole
            .rows
            .iter()
            .map(|line| rendered_text(line).trim_end().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                format!("  {}Thinking: Reading", super::spinner::MARKER),
                "    Reading".to_owned(),
                "    Read the plan.".to_owned(),
            ],
            "a section that has neither said anything nor been headed is nothing to \
             read, so the expansion ends on the last one that spoke rather than on a \
             blank row held for it"
        );
    }

    #[test]
    fn a_live_reasoning_groups_header_is_the_line_recorded_to_animate() {
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(reasoning(
                ActivityStatus::Completed,
                Some("Reading"),
                "Read the plan.",
            )),
            Entry::Activity(reasoning(ActivityStatus::Active, Some("Settling"), "")),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view_grouped(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
            &TranscriptTurnFolds::default(),
            Grouping::Formed,
        );

        let whole = view.window(0, view.row_count());
        assert_eq!(
            whole.spinner_rows.len(),
            1,
            "the Group speaks for the member still thinking, so its header is the one \
             Marker on screen"
        );
        let spinner_row = rendered_text(&whole.rows[whole.spinner_rows[0]]);
        assert_eq!(
            spinner_row.trim_end(),
            format!("  {}Thinking: Settling", super::spinner::MARKER),
            "the recorded line is the Group's live header"
        );
        let rendered = whole
            .rows
            .iter()
            .map(rendered_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rendered.contains("Thought"),
            "the settled member is inside the live Group rather than beside it, so the \
             animated header is the run's only row: {rendered}"
        );
    }
}
