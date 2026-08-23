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
    cell::{Ref, RefCell},
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    ansi::{AnsiScanner, FragmentRole, sgr_parameter_code, sgr_parameters},
    protocol::{
        Activity, ActivityId, FileChange, FoldPosture, InitialPrompt, Message, MessageId,
        MessageRole, PromptId, ReasoningVisibility, SessionId, SessionRevision, SessionSnapshot,
        TranscriptItem, Turn, TurnId, TurnStatus,
    },
    theme::Theme,
};

use super::markdown;
use super::spinner;

/// Source lines wrapping to more rows than this are split. The cap serves two
/// bounds: ratatui's u16-based scroll arithmetic stays in range, and the draw
/// re-wraps the first visible source line every frame, so the cap also limits
/// how much text that per-frame wrap can touch.
const MAX_TRANSCRIPT_SOURCE_LINE_ROWS: usize = 1_000;

/// Wrapped rows of output tail a settled command Activity's Peek shows below
/// its fold marker.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const FOLDED_COMMAND_OUTPUT_ROWS: usize = 6;

/// Wrapped rows of live tail an Active command Activity shows while it streams,
/// before it settles into its folded single row.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const LIVE_COMMAND_TAIL_ROWS: usize = 3;

/// The gutter an Activity's subordinate content sits in, so a fold or
/// truncation marker lines up with the lines it stands in for.
const OUTPUT_INDENT: &str = "    ";

/// The extra gutter an expanded Group's members sit in, so a member reads as
/// subordinate to the Group header above it.
const GROUP_MEMBER_INDENT: &str = "  ";

/// Paths a folded FileChange Activity lists before its fold marker.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const FOLDED_FILE_CHANGE_PATHS: usize = 4;

/// What a Reasoning Activity's header calls the block in each of its states.
/// Suru's own word for the concept is Reasoning, but the Transcript speaks the
/// reader's: an agent is thinking, and what it leaves behind is a thought.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
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
/// adds `Peek` between them — the tail of its output behind a fold marker.
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

/// One client's Fold state for one Session's Transcript: the posture the view
/// leans to, plus the step the reader put each entry at. Like every disclosure
/// axis this is presentation only, so it never reaches the Session, never
/// syncs between clients, and dies with the process.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptFolds {
    posture: DisclosurePosture,
    overrides: HashMap<ActivityId, FoldStep>,
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
    /// entry is — a failed command opens to its Peek where a successful one
    /// folds away.
    pub(super) fn resolve(&self, activity_id: ActivityId, default: FoldStep) -> FoldStep {
        if let Some(step) = self.overrides.get(&activity_id) {
            return *step;
        }
        match self.posture {
            DisclosurePosture::Closed => default,
            DisclosurePosture::Open => FoldStep::Expanded,
        }
    }

    /// The binary reading entries without a Peek use: anything short of
    /// `Expanded` is folded.
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
        self.overrides.insert(activity_id, step);
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
        for (id, step) in &self.overrides {
            let mut entry = std::hash::DefaultHasher::new();
            id.hash(&mut entry);
            (*step as u8).hash(&mut entry);
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

/// The disclosure axes a client renders a Transcript through: Reasoning
/// visibility and Turn Folds decide which entries reach the projection at all,
/// Groups which of the survivors share a row, and Folds how much of a row
/// shows. Rendering reads all four, so they travel as one input. Three are the
/// reader's own clicks, held per Session; the fourth is a Setting, which is
/// why it arrives by value from the effective settings rather than as view
/// state a Session keeps.
#[derive(Clone, Copy)]
pub(super) struct TranscriptDisclosure<'a> {
    pub(super) folds: &'a TranscriptFolds,
    pub(super) groups: &'a TranscriptGroups,
    pub(super) turns: &'a TranscriptTurnFolds,
    pub(super) reasoning_visibility: ReasoningVisibility,
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
    /// The summary of a Reasoning block, stored on its Activity.
    Reasoning,
}

impl CappedStream {
    /// What the transcript shows in place of the content a cap cut short. Both
    /// markers are decided here so the two stream kinds cannot drift apart in
    /// wording. A marker is drawn from the stored truncation signal rather than
    /// read out of stored content, so content that ends with these characters
    /// stays ordinary text.
    const fn truncation_marker(self) -> &'static str {
        match self {
            Self::Message => "[Message truncated]",
            Self::CommandOutput => "[output truncated]",
            Self::Reasoning => "[Reasoning truncated]",
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
    /// The step a staged Fold was drawn at, present only for units speaking
    /// the staged grammar (settled commands), so a click knows which step
    /// comes next without re-deriving the projection.
    pub(super) step: Option<FoldStep>,
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
}

impl TranscriptCache {
    /// Returns the transcript view for the given content, rebuilding only the
    /// parts whose inputs changed since the previous frame.
    pub(super) fn view(
        &self,
        generation: u64,
        snapshot: &SessionSnapshot,
        provisional: &[&InitialPrompt],
        disclosure: TranscriptDisclosure<'_>,
        theme: &Theme,
        width: u16,
    ) -> Ref<'_, TranscriptView> {
        let key = ViewKey {
            generation,
            session_id: snapshot.session.id,
            revision: snapshot.revision,
            theme: *theme,
            width,
            reasoning_visibility: disclosure.reasoning_visibility,
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
            *slot = Some(rebuild(
                previous,
                key,
                snapshot,
                provisional,
                disclosure,
                theme,
                width,
            ));
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
    /// Whether Reasoning is drawn at all, which the Settings decide rather than
    /// the reader's clicks. It is a rendering input outside the Session
    /// snapshot all the same, so ADR 0007 puts it in the key: a reader asking
    /// for Reasoning mid-Session moves the Transcript already on screen, where
    /// a default Fold posture only decides where a fresh view starts.
    reasoning_visibility: ReasoningVisibility,
    provisional_fingerprint: u64,
    /// Folds are a rendering input outside the Session snapshot, so ADR 0007
    /// requires them in the key or a flipped Fold would render a stale frame.
    folds_fingerprint: u64,
    /// Group state is the same kind of input, so a flipped Group rebuilds too.
    groups_fingerprint: u64,
    /// Turn Fold state is the same kind of input again, and the one that
    /// decides which entries are projected at all.
    turns_fingerprint: u64,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptView {
    key: ViewKey,
    units: Vec<UnitView>,
    row_count: usize,
    message_starts: Vec<MessageStart>,
    unit_starts: Vec<UnitStart>,
    /// Row offset of every source line across all units, for scroll math.
    line_starts: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TranscriptLink {
    pub(super) target: String,
}

impl TranscriptView {
    pub(super) fn row_count(&self) -> usize {
        self.row_count
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

    /// Extracts the lines needed to render `viewport_rows` rows starting at
    /// `scroll_position`, along with the residual scroll offset into the first
    /// returned line. The result is bounded by the viewport, not the
    /// transcript.
    pub(super) fn window(&self, scroll_position: usize, viewport_rows: usize) -> TranscriptWindow {
        let first_line = self
            .line_starts
            .partition_point(|row| *row <= scroll_position)
            .saturating_sub(1);
        let window_start = self.line_starts.get(first_line).copied().unwrap_or(0);
        let local_scroll = scroll_position.saturating_sub(window_start);
        let rows_needed = local_scroll.saturating_add(viewport_rows);
        let mut lines = Vec::new();
        let mut spinner_lines = Vec::new();
        let mut rows = 0;
        let first_unit = self
            .units
            .partition_point(|unit| unit.start_line <= first_line)
            .saturating_sub(1);
        'units: for unit in &self.units[first_unit.min(self.units.len())..] {
            let mut skip = first_line.saturating_sub(unit.start_line);
            if unit.leading_separator {
                // The separator is the unit's first line for indexing, so a
                // window opening on it draws it and one opening past it skips
                // it along with the lines before `first_line`.
                if skip == 0 {
                    lines.push(Line::default());
                    rows += 1;
                    if rows >= rows_needed {
                        break 'units;
                    }
                } else {
                    skip -= 1;
                }
            }
            for (index, (line, rows_of_line)) in unit
                .lines
                .iter()
                .zip(&unit.rows_per_line)
                .enumerate()
                .skip(skip)
            {
                if unit.spinner_line == Some(index) {
                    spinner_lines.push(lines.len());
                }
                lines.push(line.clone());
                rows += rows_of_line;
                if rows >= rows_needed {
                    break 'units;
                }
            }
        }
        TranscriptWindow {
            lines,
            local_scroll,
            spinner_lines,
        }
    }
}

/// The viewport-sized slice of a [`TranscriptView`] one frame draws: its
/// lines, the residual scroll into the first one, and which of them carry a
/// Spinner for the draw-time overlay (ADR 0009) to patch.
pub(super) struct TranscriptWindow {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) local_scroll: usize,
    /// Indices into `lines` whose Marker cell holds a Spinner.
    pub(super) spinner_lines: Vec<usize>,
}

/// One block of the Transcript the projection renders as a whole: the entries
/// that share a cache key, a fingerprint, and a click target. A unit answers
/// for all three itself, so rendering, memoization, and hit-testing address
/// the unit rather than what it holds.
enum RenderUnit<'a> {
    Message(&'a Message),
    Activity(&'a Activity),
    /// A Group: a run of two or more adjacent Activities of one groupable
    /// kind. Collapsed it is the run's single row; expanded it is the header
    /// the run re-collapses from, followed by whatever its kind opens onto.
    /// Never empty and never a run of one; [`close_run`] holds that invariant.
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
    /// A settled Turn's fold: the single marker row standing where the work
    /// the fold hides happened. The entries behind it are absent from the
    /// plan rather than held by this unit, because a Turn Fold hides entries
    /// that are not adjacent — a steer Message and the final agent Message
    /// stay outside a fold whose hidden work surrounds them.
    TurnFold(TurnMarker),
    /// A prompt this client sent that the Session has not echoed back yet.
    Provisional(&'a InitialPrompt),
}

impl RenderUnit<'_> {
    fn key(&self) -> UnitKey {
        match self {
            Self::Message(message) => UnitKey::Message(message.id),
            Self::Activity(activity) => UnitKey::Activity(activity.id()),
            Self::Group { members, .. } => UnitKey::Group(members[0].id()),
            Self::GroupMember(activity) => UnitKey::Activity(activity.id()),
            Self::TurnFold(marker) => UnitKey::TurnFold(marker.turn_id),
            Self::Provisional(prompt) => UnitKey::Provisional(prompt.id),
        }
    }

    /// Identifies everything the unit's rendering reads, so it re-renders when
    /// any entry it holds changes and reuses its lines when none did.
    fn fingerprint(&self, folds: &TranscriptFolds) -> u64 {
        match self {
            Self::Message(message) => message_fingerprint(message),
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
            // A Turn Fold's marker says how the Turn settled and how long it
            // took, and answers a click by how its fold stands, so those three
            // are the whole of its rendering input; which Turn it stands for is
            // already its key.
            Self::TurnFold(marker) => {
                let mut hasher = std::hash::DefaultHasher::new();
                (marker.outcome as u64).hash(&mut hasher);
                marker.duration_ms.hash(&mut hasher);
                marker.folded.hash(&mut hasher);
                hasher.finish()
            }
            // A provisional prompt's text only grows and carries no Fold, so
            // its length is the whole of its rendering input.
            Self::Provisional(prompt) => prompt.text.len() as u64,
        }
    }

    /// The unit-local line carrying a Spinner in its Marker cell, so the
    /// draw-time overlay (ADR 0009) knows where to patch the current frame.
    /// An Active Activity animates, its Marker leading its first line; a Group
    /// animates only if its kind's header ever stands for live work.
    fn spinner_line(&self) -> Option<usize> {
        match self {
            Self::Activity(activity) | Self::GroupMember(activity) => {
                (activity.status() == Some(crate::protocol::ActivityStatus::Active)).then_some(0)
            }
            Self::Group { kind, members, .. } => kind.header_spinner_line(members),
            Self::Message(_) | Self::TurnFold(_) | Self::Provisional(_) => None,
        }
    }

    /// The Message a scroll anchor holds onto when it lands on this unit.
    /// Anchoring tracks conversation, so only a unit holding one answers.
    fn message_id(&self) -> Option<MessageId> {
        match self {
            Self::Message(message) => Some(message.id),
            Self::Activity(_)
            | Self::Group { .. }
            | Self::GroupMember(_)
            | Self::TurnFold(_)
            | Self::Provisional(_) => None,
        }
    }

    const fn spacing_kind(&self) -> SpacingKind {
        match self {
            Self::Message(_) | Self::Provisional(_) => SpacingKind::Message,
            Self::Activity(activity) | Self::GroupMember(activity) => match activity {
                Activity::Error { .. } => SpacingKind::Error,
                _ => SpacingKind::Activity,
            },
            Self::Group { .. } | Self::TurnFold(_) => SpacingKind::Activity,
        }
    }

    /// Whether this unit is the first member of the expanded Group headed by
    /// `previous`, the one seam the separator rule never opens.
    const fn heads_the_group(&self, previous: &Self) -> bool {
        matches!(self, Self::GroupMember(_))
            && matches!(previous, Self::Group { expanded: true, .. })
    }

    /// Projects the unit's lines, reporting the anchor a click acts on when
    /// the unit has one.
    fn render(
        &self,
        lines: &mut Vec<Line<'static>>,
        links: &mut Vec<TranscriptLink>,
        folds: &TranscriptFolds,
        theme: &Theme,
        width: u16,
    ) -> Option<UnitAnchor> {
        match self {
            Self::Message(message) => {
                render_message(lines, message, theme, width);
                None
            }
            Self::Activity(activity) => render_activity(
                lines,
                links,
                activity,
                resolved_fold_step(folds, activity),
                theme,
                width,
            ),
            Self::Group {
                kind,
                members,
                expanded,
            } => Some(render_group(lines, *kind, members, *expanded, theme)),
            Self::GroupMember(activity) => {
                let start = lines.len();
                let anchor = render_activity(
                    lines,
                    links,
                    activity,
                    resolved_fold_step(folds, activity),
                    theme,
                    width.saturating_sub(GROUP_MEMBER_INDENT.len() as u16),
                );
                for line in &mut lines[start..] {
                    line.spans.insert(0, Span::raw(GROUP_MEMBER_INDENT));
                }
                anchor
            }
            Self::TurnFold(marker) => Some(render_turn_fold(lines, *marker, theme)),
            Self::Provisional(prompt) => {
                push_user_message(lines, &prompt.text, theme, width);
                None
            }
        }
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
    /// A Message the agent authored. Only the Turn's last one survives its
    /// fold: that is the answer the Turn arrived at, and the ones before it
    /// are work.
    AgentMessage,
    /// An Error Activity. The last one a failed Turn recorded is its
    /// outcome and stays outside the fold; the ones it worked past are work.
    Error,
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
    /// Whether Reasoning is one of the things this reader is shown, which the
    /// lookup answers alongside every other reason an entry draws nothing.
    reasoning_visibility: ReasoningVisibility,
}

impl<'a> TranscriptContent<'a> {
    fn of(snapshot: &'a SessionSnapshot, reasoning_visibility: ReasoningVisibility) -> Self {
        Self {
            reasoning_visibility,
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
                .filter(|activity| !transcript_shows_nothing(activity, self.reasoning_visibility))
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
fn transcript_shows_nothing(activity: &Activity, visibility: ReasoningVisibility) -> bool {
    match activity {
        Activity::Reasoning { .. } if visibility == ReasoningVisibility::Hidden => true,
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
                },
            },
            Self::Activity(activity) => TurnEntry {
                turn_id: activity.turn_id(),
                role: match activity {
                    Activity::Error { .. } => TurnEntryRole::Error,
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
                if role_at(position) == Some(TurnEntryRole::UserMessage)
                    || Some(position) == final_agent_message
                    || Some(position) == terminal_error
                {
                    continue;
                }
                // What the fold covers is the same whichever way the reader
                // left it; only a closed one hides what it covers.
                hidden[position] = turn_marker.folded;
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
        Self { hidden, markers }
    }

    fn hides(&self, position: usize) -> bool {
        self.hidden.get(position).copied().unwrap_or(false)
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
    /// Commands, which join a run only once they settle successfully.
    Command,
    /// Reasoning blocks, which join a run from the moment they start.
    Reasoning,
}

impl GroupableKind {
    /// The kind of run an Activity extends, or `None` when it groups with
    /// nothing and so ends whatever run it follows. A command joins only once
    /// it settles Completed with exit status 0, so a failed, interrupted, or
    /// still-running one — anything worth scanning for — never hides behind a
    /// Group row. A Reasoning block joins from the moment it starts, so the
    /// Group forms live and the run a reader is watching is the same row it
    /// reads afterwards; only a block a Turn interrupted stands outside,
    /// ending the run, because a Group row only ever summarizes thinking that
    /// finished or is still going. A block the reader sees nothing for never
    /// reaches here at all, because the walk resolves it to no entry.
    const fn joined_by(activity: &Activity) -> Option<Self> {
        use crate::protocol::ActivityStatus;

        match activity {
            Activity::Command {
                status: ActivityStatus::Completed,
                exit_status: Some(0),
                ..
            } => Some(Self::Command),
            Activity::Reasoning {
                status: ActivityStatus::Completed | ActivityStatus::Active,
                ..
            } => Some(Self::Reasoning),
            _ => None,
        }
    }

    /// The line of a Group's header carrying a Spinner in its Marker cell, or
    /// `None` when the Group stands for no work in progress. A command joins
    /// only once it has settled, so a command Group never speaks for live
    /// work; a Reasoning Group does whenever its latest member is still
    /// thinking, and its Marker leads the first line of its header.
    fn header_spinner_line(self, members: &[&Activity]) -> Option<usize> {
        match self {
            Self::Command => None,
            Self::Reasoning => reasoning_group_is_live(members).then_some(0),
        }
    }

    /// What a Group's header reads off one member, and so what its rendering
    /// must re-key on when the member changes (ADR 0007). A command Group's
    /// header counts its members, so which Activities they are is the whole of
    /// it; a kind whose header speaks for its members' content keys on that
    /// content here instead.
    fn member_fingerprint(self, member: &Activity) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        match self {
            Self::Command => member.id().hash(&mut hasher),
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

    /// The units an expanded Group plans after its header. A command Group
    /// opens onto its members, each as its own unit, so a member's Fold, its
    /// cached lines, and its click target need no Group-specific machinery. A
    /// kind whose expansion is the header's own content plans nothing here and
    /// renders it in [`render_group`] instead.
    fn expansion_units<'a>(self, members: &[&'a Activity]) -> Vec<RenderUnit<'a>> {
        match self {
            Self::Command => members
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
    members: Vec<&'a Activity>,
}

/// Walks a Session's transcript into the units a view renders. Which entries
/// share a unit is decided here and nowhere else, so gathering a run of them
/// into one stays a change to this walk rather than to rendering or layout.
fn plan_units<'a>(
    snapshot: &'a SessionSnapshot,
    provisional: &[&'a InitialPrompt],
    groups: &TranscriptGroups,
    turns: &TranscriptTurnFolds,
    reasoning_visibility: ReasoningVisibility,
) -> Vec<RenderUnit<'a>> {
    let content = TranscriptContent::of(snapshot, reasoning_visibility);
    let folding = TurnFolding::plan(snapshot, &content, turns);
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
        match content.entry(item) {
            Some(TranscriptEntry::Message(message)) => {
                close_run(&mut units, &mut run, groups);
                units.push(RenderUnit::Message(message));
            }
            Some(TranscriptEntry::Activity(activity)) => match GroupableKind::joined_by(activity) {
                Some(kind) => {
                    if run.as_ref().is_some_and(|open| open.kind != kind) {
                        close_run(&mut units, &mut run, groups);
                    }
                    run.get_or_insert(GroupRun {
                        kind,
                        members: Vec::new(),
                    })
                    .members
                    .push(activity);
                }
                None => {
                    close_run(&mut units, &mut run, groups);
                    units.push(RenderUnit::Activity(activity));
                }
            },
            None => {}
        }
    }
    close_run(&mut units, &mut run, groups);
    units.extend(provisional.iter().copied().map(RenderUnit::Provisional));
    units
}

/// Ends the run in progress: two or more members become one Group, while a run
/// of one stays the ordinary row it is, so grouping never adds a layer where it
/// saves nothing. A Group the reader expanded plans as its header followed by
/// whatever its kind opens onto.
fn close_run<'a>(
    units: &mut Vec<RenderUnit<'a>>,
    run: &mut Option<GroupRun<'a>>,
    groups: &TranscriptGroups,
) {
    let Some(GroupRun { kind, members }) = run.take() else {
        return;
    };
    if members.len() < 2 {
        units.extend(members.into_iter().map(RenderUnit::Activity));
        return;
    }
    if groups.is_collapsed(members[0].id()) {
        units.push(RenderUnit::Group {
            kind,
            members,
            expanded: false,
        });
    } else {
        let expansion = kind.expansion_units(&members);
        units.push(RenderUnit::Group {
            kind,
            members,
            expanded: true,
        });
        units.extend(expansion);
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
    lines: Vec<Line<'static>>,
    links: Vec<TranscriptLink>,
    /// Wrapped row count per line at the view width, so layout is a prefix sum
    /// instead of a re-wrap.
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
    /// separator as that first line so a window opening mid-unit lands by
    /// simple subtraction.
    start_line: usize,
    /// The unit-local line whose Marker cell carries a Spinner, recorded so
    /// the draw-time overlay (ADR 0009) can patch the current frame in
    /// without the projection ever depending on it.
    spinner_line: Option<usize>,
    message_id: Option<MessageId>,
    anchor: Option<LaidOutAnchor>,
}

/// A unit's anchor once layout resolved it: how many of the unit's laid-out
/// lines form its header, whether it held content back, and where its staged
/// Fold stands.
#[derive(Clone, Copy, Debug)]
struct LaidOutAnchor {
    header_lines: usize,
    hides_content: bool,
    step: Option<FoldStep>,
    /// Index of the fold-marker line among the unit's laid-out lines.
    marker_line: Option<usize>,
}

fn rebuild(
    previous: Option<TranscriptView>,
    key: ViewKey,
    snapshot: &SessionSnapshot,
    provisional: &[&InitialPrompt],
    disclosure: TranscriptDisclosure<'_>,
    theme: &Theme,
    width: u16,
) -> TranscriptView {
    let mut reusable: HashMap<UnitKey, UnitView> = previous
        .filter(|view| {
            view.key.generation == key.generation
                && view.key.session_id == key.session_id
                && view.key.theme == key.theme
                && view.key.width == key.width
        })
        .map(|view| {
            view.units
                .into_iter()
                .map(|unit| (unit.key, unit))
                .collect()
        })
        .unwrap_or_default();
    let planned = plan_units(
        snapshot,
        provisional,
        disclosure.groups,
        disclosure.turns,
        disclosure.reasoning_visibility,
    );
    let mut units = planned
        .iter()
        .map(|unit| reuse_or_render(&mut reusable, unit, disclosure.folds, theme, width))
        .collect::<Vec<_>>();
    assign_separators(&planned, &mut units);

    let mut row_count = 0;
    let mut line_count = 0;
    let mut message_starts = Vec::new();
    let mut unit_starts = Vec::new();
    let mut line_starts = Vec::new();
    for unit in &mut units {
        unit.start_line = line_count;
        if unit.leading_separator {
            line_starts.push(row_count);
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
        for rows_of_line in &unit.rows_per_line {
            line_starts.push(row_count);
            row_count += rows_of_line;
        }
        line_count += unit.lines.len();
        if let Some(anchor) = unit.anchor {
            unit_starts.push(UnitStart {
                key: unit.key,
                row: unit_start_row,
                header_rows: unit.rows_per_line[..anchor.header_lines].iter().sum(),
                row_count: row_count - unit_start_row,
                hides_content: anchor.hides_content,
                step: anchor.step,
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
        line_starts,
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
    let rendered_anchor = unit.render(&mut rendered, &mut links, folds, theme, width);
    let source_lines = rendered.len();
    let mut measured = Vec::with_capacity(rendered.len());
    let mut header_lines = 0;
    let mut marker_line = None;
    for (index, line) in rendered.into_iter().enumerate() {
        if rendered_anchor.is_some_and(|anchor| anchor.marker_source_line == Some(index)) {
            marker_line = Some(measured.len());
        }
        split_oversized_line(line, width, &mut measured);
        if rendered_anchor.is_some_and(|anchor| index + 1 == anchor.header_source_lines) {
            header_lines = measured.len();
        }
    }
    let (lines, rows_per_line): (Vec<_>, Vec<_>) = measured.into_iter().unzip();
    UnitView {
        key,
        fingerprint,
        lines,
        links,
        rows_per_line,
        source_lines,
        leading_separator: false,
        start_line: 0,
        spinner_line: unit.spinner_line(),
        message_id: unit.message_id(),
        anchor: rendered_anchor.map(|anchor| LaidOutAnchor {
            header_lines,
            hides_content: anchor.hides_content,
            step: anchor.step,
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
    /// The step a staged Fold was drawn at; `None` for units whose Fold is
    /// binary, which includes a still-streaming command.
    step: Option<FoldStep>,
    /// Index of the fold-marker source line within the unit's lines.
    marker_source_line: Option<usize>,
}

impl UnitAnchor {
    /// The anchor a binary Fold reports: no staged step and no marker target.
    const fn binary(header_source_lines: usize, hides_content: bool) -> Self {
        Self {
            header_source_lines,
            hides_content,
            step: None,
            marker_source_line: None,
        }
    }
}

/// Message content is append-only, so its length identifies it within a
/// Session once the truncation signal, which flips without lengthening the
/// content, is folded in.
fn message_fingerprint(message: &Message) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    message.content.len().hash(&mut hasher);
    message.truncated.hash(&mut hasher);
    hasher.finish()
}

/// The step an Activity's Fold rests at before the reader touches it. A
/// failed command opens to its Peek — the tail is where the error lives — so
/// the one output worth reading is on screen, while everything else folds
/// away. A command that settled Failed without an exit status was interrupted
/// rather than refused, so it folds like a success; the reader who was
/// watching it still gets a Peek, but through the interrupt-time override
/// rather than this default.
fn default_fold_step(activity: &Activity) -> FoldStep {
    match activity {
        Activity::Command {
            status: crate::protocol::ActivityStatus::Failed,
            exit_status: Some(_),
            ..
        } => FoldStep::Peek,
        _ => FoldStep::Folded,
    }
}

/// The step an Activity presents at under the client's Fold state.
fn resolved_fold_step(folds: &TranscriptFolds, activity: &Activity) -> FoldStep {
    folds.resolve(activity.id(), default_fold_step(activity))
}

/// Identifies an Activity's rendered form. The Fold step joins the content
/// signals because a folded entry renders different lines from the same
/// Activity.
fn activity_fingerprint(activity: &Activity, step: FoldStep) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    (step as u8).hash(&mut hasher);
    match activity {
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

fn provisional_fingerprint(provisional: &[&InitialPrompt]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for prompt in provisional {
        prompt.id.hash(&mut hasher);
        prompt.text.len().hash(&mut hasher);
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

fn render_message(lines: &mut Vec<Line<'static>>, message: &Message, theme: &Theme, width: u16) {
    match message.role {
        MessageRole::User => push_user_message(lines, &message.content, theme, width),
        MessageRole::Agent => push_agent_message(lines, &message.content, message.truncated, theme),
    }
}

/// Projects one Activity, reporting an anchor for the kinds a reader can fold.
/// `Status` is already one line and `Error` is a failure the reader must not
/// have to hunt for, so neither is ever folded and neither anchors a click.
/// Each kind projects through its own renderer, so what one kind shows never
/// depends on how another renders.
fn render_activity(
    lines: &mut Vec<Line<'static>>,
    links: &mut Vec<TranscriptLink>,
    activity: &Activity,
    step: FoldStep,
    theme: &Theme,
    width: u16,
) -> Option<UnitAnchor> {
    let mut projection = ActivityProjection { lines, links };
    match activity {
        Activity::Status { text, .. } => {
            push_styled_prefixed_lines(
                &mut projection,
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
                &mut projection,
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
            &mut projection,
            CommandActivity {
                status: *status,
                command,
                cwd: cwd.as_deref(),
                output,
                output_truncated: *output_truncated,
                exit_status: *exit_status,
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
        )),
        Activity::Reasoning { .. } => ReasoningActivity::of(activity).map(|reasoning| {
            push_reasoning_activity(
                projection.lines,
                reasoning,
                step != FoldStep::Expanded,
                theme,
            )
        }),
    }
}

/// Projects a Group, which each groupable kind words and opens in its own way.
/// What every kind shares is the shape: a header row that stands in for the
/// run while collapsed and heads it while expanded, being the one row the
/// Group re-collapses from. `members` always holds at least two.
fn render_group(
    lines: &mut Vec<Line<'static>>,
    kind: GroupableKind,
    members: &[&Activity],
    expanded: bool,
    theme: &Theme,
) -> UnitAnchor {
    match kind {
        GroupableKind::Command => render_command_group(lines, members.len(), expanded, theme),
        GroupableKind::Reasoning => render_reasoning_group(lines, members, expanded, theme),
    }
}

/// Projects a command Group's header: one row in the settled Activity-header
/// idiom, with the `Ran N commands` count styled as the toggle affordance it
/// is. Collapsed, the row stands in for its members and the count doubles as
/// the hidden-ness indicator, so no fold-marker line follows; expanded, the
/// same header leads the member units.
fn render_command_group(
    lines: &mut Vec<Line<'static>>,
    member_count: usize,
    expanded: bool,
    theme: &Theme,
) -> UnitAnchor {
    lines.push(Line::from(vec![
        Span::styled("  ✓ ", theme.feedback.success),
        Span::styled(format!("Ran {member_count} commands"), theme.action.primary),
    ]));
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
/// command Group's count is — and sums what its members spent. The count always
/// speaks for more than one member: a run of one is never a Group, so a lone
/// block keeps the header-and-fold row it has alone instead of gaining a count
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
    lines: &mut Vec<Line<'static>>,
    members: &[&Activity],
    expanded: bool,
    theme: &Theme,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    // The state the row speaks for is the run's, not any one member's: still
    // thinking while its latest member is, and thought once that member lands.
    // A Group never stands for thinking that was interrupted, because a block
    // a Turn cut short leaves the run rather than ending it inside one.
    let live = reasoning_group_is_live(members);
    let status = if live {
        ActivityStatus::Active
    } else {
        ActivityStatus::Completed
    };
    let (marker, label, style) = reasoning_marker(status, theme);
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
        header_line.spans.push(Span::styled(" · ", style));
        header_line.spans.push(Span::styled(
            format!("{} steps", members.len()),
            theme.action.primary,
        ));
        if let Some(duration_ms) = summed_reasoning_duration(members) {
            header_line.spans.push(Span::styled(
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
        push_reasoning_section(&mut section, member, theme);
        // A member that has started without saying anything or being headed
        // projects nothing, and a section that is not there takes no blank row
        // to stand apart from the one before it.
        if section.is_empty() {
            continue;
        }
        if opened_a_section {
            lines.push(Line::default());
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
    lines: &mut Vec<Line<'static>>,
    reasoning: ReasoningActivity<'_>,
    theme: &Theme,
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
    lines.append(&mut reasoning_body_lines(&reasoning, theme));
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
fn render_turn_fold(
    lines: &mut Vec<Line<'static>>,
    marker: TurnMarker,
    theme: &Theme,
) -> UnitAnchor {
    let (glyph, style) = match marker.outcome {
        SettledTurn::Completed => ("✓ ", theme.feedback.success),
        SettledTurn::Interrupted => ("× ", theme.feedback.warning),
        SettledTurn::Failed => ("× ", theme.feedback.error),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {glyph}"), style),
        Span::styled(marker.label(), theme.action.primary),
    ]));
    UnitAnchor::binary(1, marker.folded)
}

struct ActivityProjection<'a> {
    lines: &'a mut Vec<Line<'static>>,
    links: &'a mut Vec<TranscriptLink>,
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

/// Projects a command Activity through its staged Fold. Folded, a settled
/// command keeps a single end-clamped row; its Peek brings the full header
/// back over a tail of output behind the fold marker; Expanded shows
/// everything stored. A still-streaming command shows a live tail instead and
/// speaks the binary grammar, so a click opens the whole stream. Output is
/// projected in full first so hyperlink targets survive, and only then
/// clamped, so a Fold changes presentation and nothing else.
fn push_command_activity(
    projection: &mut ActivityProjection<'_>,
    activity: CommandActivity<'_>,
    step: FoldStep,
    theme: &Theme,
    width: u16,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    let CommandActivity {
        status,
        command,
        cwd,
        output,
        output_truncated,
        exit_status,
    } = activity;
    let (marker, style) = match status {
        ActivityStatus::Active => (spinner::MARKER, theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", theme.feedback.success),
        ActivityStatus::Failed => ("× ", theme.feedback.error),
    };
    let exit_suffix = match (status, exit_status) {
        (ActivityStatus::Failed, Some(exit_status)) => format!(" (exit {exit_status})"),
        _ => String::new(),
    };
    let mut output_lines = Vec::new();
    if !output.is_empty() {
        push_styled_prefixed_lines(
            &mut ActivityProjection {
                lines: &mut output_lines,
                links: projection.links,
            },
            ContentGutter {
                lead: OUTPUT_INDENT,
                indent: OUTPUT_INDENT,
            },
            output,
            theme.text.subdued,
            theme,
        );
    }
    if status != ActivityStatus::Active && step == FoldStep::Folded {
        return push_folded_command_row(
            projection.lines,
            &format!("  {marker}"),
            command,
            &exit_suffix,
            style,
            width,
            cwd.is_some() || !output.is_empty() || output_truncated,
        );
    }
    let tail_rows = match (status, step) {
        (_, FoldStep::Expanded) => None,
        (ActivityStatus::Active, _) => Some(LIVE_COMMAND_TAIL_ROWS),
        (_, FoldStep::Peek) => Some(FOLDED_COMMAND_OUTPUT_ROWS),
        (_, FoldStep::Folded) => unreachable!("a settled folded command returned above"),
    };
    let command = format!("{command}{exit_suffix}");
    let header_start = projection.lines.len();
    push_prefixed_lines(projection.lines, &format!("  {marker}"), &command, style);
    let header_source_lines = projection.lines.len() - header_start;
    if let Some(cwd) = cwd {
        push_prefixed_lines(
            projection.lines,
            "    in ",
            cwd.to_string_lossy().as_ref(),
            theme.text.subdued,
        );
    }
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
    if output_truncated {
        push_truncation_marker(
            projection.lines,
            CappedStream::CommandOutput,
            OUTPUT_INDENT,
            theme,
        );
    }
    UnitAnchor {
        header_source_lines,
        hides_content,
        // A still-streaming command's tail is not a step the reader chose, so
        // it keeps the binary grammar: one click opens the whole stream.
        step: (status != ActivityStatus::Active).then_some(step),
        marker_source_line,
    }
}

/// Projects the single row a folded settled command keeps: its status marker
/// and command, end-clamped to the width with an ellipsis instead of
/// wrapping. `suffix` — the exit status of a failed command — keeps its place
/// past the clamp, so failure stays legible however long the command was. The
/// clamp is presentation only — the Peek brings the full header back — so it
/// reports as hidden content like everything else the Fold holds.
fn push_folded_command_row(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    command: &str,
    suffix: &str,
    style: Style,
    width: u16,
    hides_more: bool,
) -> UnitAnchor {
    let command = sanitize_content(command);
    let first_line = command.lines().next().unwrap_or_default();
    let has_more_lines = command.lines().nth(1).is_some();
    let budget = usize::from(width).saturating_sub(prefix.width() + suffix.width());
    let clamped = has_more_lines || first_line.width() > budget;
    let text = if clamped {
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
        format!("{}…", clipped.trim_end())
    } else {
        first_line.to_owned()
    };
    lines.push(Line::styled(format!("{prefix}{text}{suffix}"), style));
    UnitAnchor {
        header_source_lines: 1,
        hides_content: hides_more || clamped,
        step: Some(FoldStep::Folded),
        marker_source_line: None,
    }
}

/// Clamps a command's projected output to the tail its Fold shows, behind the
/// fold marker counting the source lines above it. Rows are counted after
/// wrapping so a handful of very long lines cannot flood the budget, while
/// the marker counts source lines, so the number a reader sees does not shift
/// when the terminal is resized. Reports whether a marker was drawn.
fn fold_output_to_tail(
    mut lines: Vec<Line<'static>>,
    tail_rows: usize,
    theme: &Theme,
    width: u16,
) -> (Vec<Line<'static>>, bool) {
    let rows_per_line = lines
        .iter()
        .map(|line| wrapped_line_count(line, width).max(1))
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
    let hidden = tail_start;
    let tail = lines.split_off(tail_start);
    lines.clear();
    lines.push(fold_marker_line(hidden, "lines", OUTPUT_INDENT, theme));
    lines.extend(tail);
    (lines, true)
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
fn fold_marker_line(hidden: usize, unit: &str, indent: &str, theme: &Theme) -> Line<'static> {
    Line::styled(
        format!("{indent}… {}", fold_marker_text(hidden, unit)),
        theme.action.primary,
    )
}

/// Renders the truncation marker as its own line, styled from Suru's typed
/// signal rather than from anything the stream carried, so a reader can tell
/// Suru dropped the rest rather than the Provider ending there.
fn push_truncation_marker(
    lines: &mut Vec<Line<'static>>,
    stream: CappedStream,
    indent: &str,
    theme: &Theme,
) {
    lines.push(Line::styled(
        format!("{indent}{}", stream.truncation_marker()),
        theme.text.subdued.add_modifier(Modifier::ITALIC),
    ));
}

/// Projects a FileChange Activity. A folded entry lists the first few paths
/// and reports the rest through the same fold marker and the same per-entry
/// Fold state a command Activity uses, which is what makes those generic
/// rather than specific to command output.
fn push_file_change_activity(
    lines: &mut Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    changes: &[FileChange],
    folded: bool,
    theme: &Theme,
) -> UnitAnchor {
    use crate::protocol::ActivityStatus;

    let (marker, label, style) = match status {
        ActivityStatus::Active => (
            spinner::MARKER,
            "Applying file changes",
            theme.accent.primary,
        ),
        ActivityStatus::Completed => ("✓ ", "Applied file changes", theme.feedback.success),
        ActivityStatus::Failed => ("× ", "Failed to apply file changes", theme.feedback.error),
    };
    let header_start = lines.len();
    push_prefixed_lines(lines, &format!("  {marker}"), label, style);
    let header_source_lines = lines.len() - header_start;
    let listed = if folded {
        changes.len().min(FOLDED_FILE_CHANGE_PATHS)
    } else {
        changes.len()
    };
    for change in &changes[..listed] {
        let summary = match change {
            FileChange::Add { path } => format!("A {}", path.to_string_lossy()),
            FileChange::Delete { path } => format!("D {}", path.to_string_lossy()),
            FileChange::Update {
                path,
                moved_to: Some(moved_to),
            } => format!(
                "R {} → {}",
                path.to_string_lossy(),
                moved_to.to_string_lossy()
            ),
            FileChange::Update {
                path,
                moved_to: None,
            } => format!("M {}", path.to_string_lossy()),
        };
        push_prefixed_lines(lines, OUTPUT_INDENT, &summary, theme.text.subdued);
    }
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
        ActivityStatus::Failed => ("× ", REASONING_FAILED_LABEL, theme.feedback.error),
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
/// Provider wrote, rendered as the Markdown it is but drained of colour so
/// Reasoning never competes with the answer it led to, followed by the
/// truncation marker when the cap cut it short. A lone block's Fold and a
/// Group's expansion both open onto exactly this.
fn reasoning_body_lines(reasoning: &ReasoningActivity<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let content = sanitize_content(reasoning.content);
    let mut body = markdown::render(&content, theme)
        .into_iter()
        .map(|line| subdued_line(line, OUTPUT_INDENT, theme))
        .collect::<Vec<_>>();
    if reasoning.content_truncated {
        push_truncation_marker(&mut body, CappedStream::Reasoning, OUTPUT_INDENT, theme);
    }
    body
}

/// Projects a Reasoning Activity. Folded — the posture a Transcript leans to —
/// it is the single header line the reader skims past; expanded it opens into
/// the summary the Provider wrote, rendered as the Markdown it is but drained of
/// colour so Reasoning never competes with the answer it led to.
fn push_reasoning_activity(
    lines: &mut Vec<Line<'static>>,
    activity: ReasoningActivity<'_>,
    folded: bool,
    theme: &Theme,
) -> UnitAnchor {
    let (marker, label, style) = reasoning_marker(activity.status, theme);
    let mut header = reasoning_header_text(label, activity.title);
    if let Some(duration_ms) = activity.duration_ms {
        header.push_str(" · ");
        header.push_str(&humanized_duration(duration_ms));
    }
    // The body is projected whether or not it will be shown, because a Fold
    // that hides all of it still has to say how many lines that is.
    let mut body = reasoning_body_lines(&activity, theme);
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
        header_line.spans.push(Span::styled(" · ", style));
        header_line.spans.push(Span::styled(
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

/// Re-styles a rendered Markdown line as subdued prose in the Activity gutter.
/// The Markdown renderer's own emphasis survives as modifiers; only its colours
/// are dropped, which is what makes the body read as an aside rather than as a
/// second Message.
fn subdued_line(line: Line<'static>, indent: &str, theme: &Theme) -> Line<'static> {
    if line.spans.is_empty() {
        return line;
    }
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(indent.to_owned(), theme.text.subdued));
    spans.extend(line.spans.into_iter().map(|span| {
        let modifiers = span.style.add_modifier;
        Span::styled(span.content, theme.text.subdued.add_modifier(modifiers))
    }));
    Line::from(spans)
}

/// Renders a duration at the coarsest precision that still says something: a
/// span of seconds is not reported to the millisecond, and one of minutes is not
/// reported as hundreds of seconds. Every duration the Transcript states passes
/// through here — a Reasoning header and a Turn Fold marker alike — so the same
/// span never reads two ways.
fn humanized_duration(duration_ms: u64) -> String {
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

fn push_user_message(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    theme: &Theme,
    available_width: u16,
) {
    let content = sanitize_content(content);
    let surface = theme.surface.elevated.patch(theme.text.primary);
    let accent = theme.surface.elevated.patch(theme.accent.primary);
    let available_width = usize::from(available_width);
    let content_width = available_width.saturating_sub(2).max(1);
    for content_line in wrapped_content_lines(&content, content_width) {
        let padding = available_width.saturating_sub(2 + content_line.width());
        lines.push(Line::from(vec![
            Span::styled("┃ ", accent),
            Span::styled(content_line, surface),
            Span::styled(" ".repeat(padding), surface),
        ]));
    }
}

fn wrapped_content_lines(content: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for source_line in content.split('\n') {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for character in source_line.chars() {
            let character_width = character.width().unwrap_or(1);
            if line_width > 0 && line_width + character_width > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
        wrapped.push(line);
    }
    wrapped
}

fn push_agent_message(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    truncated: bool,
    theme: &Theme,
) {
    let content = sanitize_content(content);
    for mut line in markdown::render(&content, theme) {
        if !line.spans.is_empty() {
            line.spans.insert(0, Span::styled("  ", theme.text.primary));
        }
        lines.push(line);
    }
    if truncated {
        push_truncation_marker(lines, CappedStream::Message, "  ", theme);
    }
}

fn push_prefixed_lines(lines: &mut Vec<Line<'static>>, prefix: &str, content: &str, style: Style) {
    let content = sanitize_content(content);
    for (index, line) in content.lines().enumerate() {
        lines.push(Line::styled(
            format!("{}{line}", if index == 0 { prefix } else { "  " }),
            style,
        ));
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
    let mut hyperlink_active = false;
    let mut spans = vec![Span::styled(gutter.lead.to_owned(), base_style)];
    let mut line_has_content = false;

    for token in content_tokens(content) {
        match token {
            ContentToken::Text(text) if !text.is_empty() => {
                spans.push(Span::styled(
                    text,
                    activity_content_style(style.rendered, hyperlink_active, theme),
                ));
                line_has_content = true;
            }
            ContentToken::Sgr(sequence) => apply_sgr(&sequence, &mut style, base_style, theme),
            ContentToken::LinkStart(target) => {
                projection.links.push(TranscriptLink { target });
                hyperlink_active = true;
            }
            ContentToken::LinkEnd => hyperlink_active = false,
            ContentToken::Tab => {
                spans.push(Span::styled(
                    "    ",
                    activity_content_style(style.rendered, hyperlink_active, theme),
                ));
                line_has_content = true;
            }
            ContentToken::LineBreak => {
                projection
                    .lines
                    .push(Line::from(std::mem::take(&mut spans)));
                spans.push(Span::styled(gutter.indent.to_owned(), base_style));
                line_has_content = false;
            }
            ContentToken::Text(_) => {}
        }
    }
    if line_has_content {
        projection.lines.push(Line::from(spans));
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

fn wrapped_line_count(line: &Line<'static>, width: u16) -> usize {
    // A line whose display width fits the wrap width renders as exactly one
    // row, so the Paragraph wrap machinery only runs for lines that actually
    // wrap. The width sum uses the same unicode-width tables ratatui's reflow
    // does; a span-embedded newline would break a line the sum cannot see, so
    // it falls back to the measured count.
    if width > 0
        && line.width() <= usize::from(width)
        && line.spans.iter().all(|span| !span.content.contains('\n'))
    {
        return 1;
    }
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Splits `line` into pieces each wrapping to at most
/// [`MAX_TRANSCRIPT_SOURCE_LINE_ROWS`] rows, pushing every piece with its
/// measured row count so layout does not have to measure again.
fn split_oversized_line(line: Line<'static>, width: u16, output: &mut Vec<(Line<'static>, usize)>) {
    let rows = wrapped_line_count(&line, width);
    if rows <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push((line, rows));
        return;
    }
    // Cut by column arithmetic in one pass, targeting half the row cap so
    // ordinary word-wrap waste still leaves each chunk under the cap. The
    // arithmetic is an estimate, so each chunk is verified once; a chunk a
    // pathological wrap pattern pushes past the cap falls back to bisection.
    for chunk in split_line_at_column_budget(line, width) {
        let rows = wrapped_line_count(&chunk, width);
        if rows <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
            output.push((chunk, rows));
        } else {
            bisect_oversized_line(chunk, width, output);
        }
    }
}

/// Splits a line at character boundaries whenever the running display width
/// reaches half the row cap's worth of columns. One pass over the content, so
/// the split stays linear in the line's length.
fn split_line_at_column_budget(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let column_budget = (MAX_TRANSCRIPT_SOURCE_LINE_ROWS / 2)
        .saturating_mul(usize::from(width.max(1)))
        .max(1);
    let Line {
        style,
        alignment,
        spans,
    } = line;
    let mut chunks = Vec::new();
    let mut chunk_spans: Vec<Span<'static>> = Vec::new();
    let mut chunk_columns = 0usize;
    for span in spans {
        let span_columns = span.content.width();
        if chunk_columns + span_columns <= column_budget {
            chunk_columns += span_columns;
            chunk_spans.push(span);
            continue;
        }
        let span_style = span.style;
        let content = span.content.into_owned();
        let mut piece = String::new();
        for character in content.chars() {
            let character_columns = character.width().unwrap_or(0);
            if chunk_columns + character_columns > column_budget && chunk_columns > 0 {
                if !piece.is_empty() {
                    chunk_spans.push(Span::styled(std::mem::take(&mut piece), span_style));
                }
                chunks.push(Line {
                    style,
                    alignment,
                    spans: std::mem::take(&mut chunk_spans),
                });
                chunk_columns = 0;
            }
            piece.push(character);
            chunk_columns += character_columns;
        }
        if !piece.is_empty() {
            chunk_spans.push(Span::styled(piece, span_style));
        }
    }
    if !chunk_spans.is_empty() || chunks.is_empty() {
        chunks.push(Line {
            style,
            alignment,
            spans: chunk_spans,
        });
    }
    chunks
}

fn bisect_oversized_line(
    line: Line<'static>,
    width: u16,
    output: &mut Vec<(Line<'static>, usize)>,
) {
    let rows = wrapped_line_count(&line, width);
    if rows <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push((line, rows));
        return;
    }
    let character_count = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    if character_count < 2 {
        output.push((line, rows));
        return;
    }
    let (left, right) = split_line_at_character_midpoint(line, character_count);
    bisect_oversized_line(left, width, output);
    bisect_oversized_line(right, width, output);
}

fn split_line_at_character_midpoint(
    line: Line<'static>,
    character_count: usize,
) -> (Line<'static>, Line<'static>) {
    let Line {
        style,
        alignment,
        spans,
    } = line;
    let mut remaining_left = character_count / 2;
    let mut left_spans = Vec::new();
    let mut right_spans = Vec::new();
    for span in spans {
        if remaining_left == 0 {
            right_spans.push(span);
            continue;
        }
        let span_character_count = span.content.chars().count();
        if span_character_count <= remaining_left {
            remaining_left -= span_character_count;
            left_spans.push(span);
            continue;
        }

        let content = span.content.into_owned();
        let split_byte = content
            .char_indices()
            .nth(remaining_left)
            .map_or(content.len(), |(index, _)| index);
        let (left, right) = content.split_at(split_byte);
        if !left.is_empty() {
            left_spans.push(Span::styled(left.to_owned(), span.style));
        }
        if !right.is_empty() {
            right_spans.push(Span::styled(right.to_owned(), span.style));
        }
        remaining_left = 0;
    }

    (
        Line {
            style,
            alignment,
            spans: left_spans,
        },
        Line {
            style,
            alignment,
            spans: right_spans,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };

    use crate::{
        protocol::{
            Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
            ModelAvailability, PromptId, ReasoningVisibility, Session, SessionRevision,
            SessionSnapshot, SessionStatus, SessionTimestamp, TranscriptItem, Turn, TurnId,
            TurnStatus, Workspace,
        },
        theme::Theme,
    };

    use super::{
        CappedStream, FoldStep, MAX_TRANSCRIPT_SOURCE_LINE_ROWS, TranscriptCache,
        TranscriptDisclosure, TranscriptFolds, TranscriptGroups, TranscriptTurnFolds,
        TranscriptView, UnitKey, UnitStart, render_activity, render_message, split_oversized_line,
        wrapped_line_count,
    };

    fn rendered_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| &*span.content).collect()
    }

    fn paragraph_line_count(line: &Line<'static>, width: u16) -> usize {
        use ratatui::widgets::{Paragraph, Wrap};
        Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
    }

    fn split_and_check(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
        let original = rendered_text(&line);
        let mut measured = Vec::new();
        split_oversized_line(line, width, &mut measured);
        for (chunk, rows) in &measured {
            assert!(
                *rows <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS,
                "a split chunk exceeds the row cap"
            );
            assert_eq!(
                *rows,
                paragraph_line_count(chunk, width),
                "a chunk's reported row count must match ratatui's wrapping"
            );
        }
        let chunks: Vec<_> = measured.into_iter().map(|(chunk, _)| chunk).collect();
        let reassembled = chunks.iter().map(rendered_text).collect::<String>();
        assert_eq!(reassembled, original, "splitting must not lose content");
        chunks
    }

    #[test]
    fn wrapped_line_count_fast_path_matches_paragraph_wrapping() {
        let corpus = vec![
            Line::from(""),
            Line::from("x"),
            Line::from("word"),
            Line::from("exactly-tw"),
            Line::from("just-over-w"),
            Line::from("\u{5b57}".repeat(6)),
            Line::from("\t\tindented content"),
            Line::from("word ".repeat(40)),
            Line::from(vec![
                Span::raw("styled "),
                Span::styled("span pieces", Style::default().fg(Color::Rgb(9, 8, 7))),
            ]),
            Line::from("a \u{5b57}\u{5b57} mixed width content line"),
        ];
        for width in [1u16, 2, 5, 10, 11, 26, 80] {
            for line in &corpus {
                assert_eq!(
                    wrapped_line_count(line, width),
                    paragraph_line_count(line, width),
                    "fast path diverged for {:?} at width {width}",
                    rendered_text(line)
                );
            }
        }
    }

    #[test]
    fn oversized_unbroken_line_splits_into_chunks_under_the_row_cap() {
        let width = 26u16;
        let content = "x".repeat(usize::from(width) * (MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 4));
        let chunks = split_and_check(Line::from(content), width);
        assert!(chunks.len() > 1, "an oversized line must split");
    }

    #[test]
    fn oversized_line_with_pathological_word_wrap_stays_under_the_row_cap() {
        // Alternating one-character and width-filling words maximize the rows
        // ratatui produces per column of content, stressing the arithmetic
        // estimate's margin.
        let width = 12u16;
        let word = "b".repeat(usize::from(width) - 1);
        let content = format!("a {word} ").repeat(MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 4);
        split_and_check(Line::from(content), width);
    }

    #[test]
    fn oversized_wide_character_line_splits_at_character_boundaries() {
        let width = 13u16;
        let content = "\u{5b57}".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS * 2);
        split_and_check(Line::from(content), width);
    }

    #[test]
    fn splitting_a_styled_oversized_line_preserves_span_styles() {
        let width = 20u16;
        let styled = Style::default().fg(Color::Rgb(1, 2, 3));
        let plain = "p".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS);
        let emphasized = "e".repeat(usize::from(width) * MAX_TRANSCRIPT_SOURCE_LINE_ROWS);
        let line = Line::from(vec![Span::raw(plain), Span::styled(emphasized, styled)]);
        let chunks = split_and_check(line, width);
        for chunk in &chunks {
            for span in &chunk.spans {
                if span.content.contains('p') {
                    assert_eq!(span.style, Style::default());
                }
                if span.content.contains('e') {
                    assert_eq!(span.style, styled);
                }
            }
        }
    }

    #[test]
    fn a_line_within_the_row_cap_is_not_split() {
        let chunks = split_and_check(Line::from("short line"), 80);
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
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
                reasoning_visibility: ReasoningVisibility::Shown,
            },
            &first_theme,
            80,
        );
        let first_lines = first.window(0, 10).lines;
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
                reasoning_visibility: ReasoningVisibility::Shown,
            },
            &second_theme,
            80,
        );
        let second_lines = second.window(0, 10).lines;

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
                    reasoning_visibility: ReasoningVisibility::Shown,
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
                    reasoning_visibility: ReasoningVisibility::Shown,
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Peek,
            &theme,
            80,
        );

        let marker = lines.last().expect("render the truncation marker");
        assert_eq!(rendered_text(marker), "    [output truncated]");
        assert!(
            marker.style.add_modifier.contains(Modifier::ITALIC),
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Expanded,
            &theme,
            80,
        );

        assert_eq!(
            rendered_text(&lines[0]),
            "  ✓ Thought: Inspecting the seam · 4s"
        );
        let marker = lines.last().expect("render the truncation marker");
        assert_eq!(rendered_text(marker), "    [Reasoning truncated]");
        let body = lines[1..lines.len() - 1]
            .iter()
            .map(rendered_text)
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
            truncated: true,
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_message(&mut lines, &message, &theme, 80);

        let marker = lines
            .iter()
            .find(|line| rendered_text(line).contains("truncated]"))
            .expect("render the truncation marker");
        assert_eq!(
            rendered_text(marker),
            "  [Message truncated]",
            "a capped Message ends with a marker that names a Message"
        );
        assert!(
            marker.style.add_modifier.contains(Modifier::ITALIC),
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Peek,
            &theme,
            80,
        );

        let marker_lines = lines
            .iter()
            .filter(|line| {
                rendered_text(line).contains(CappedStream::CommandOutput.truncation_marker())
            })
            .collect::<Vec<_>>();
        let [marker] = marker_lines.as_slice() else {
            panic!("output that reads like the marker renders once: {lines:?}");
        };
        assert!(
            !marker.style.add_modifier.contains(Modifier::ITALIC),
            "output the Provider sent keeps the style of command output: {marker:?}"
        );
        assert_eq!(rendered_text(marker), "    [output truncated]");
    }

    #[test]
    fn agent_message_ending_in_the_marker_text_renders_as_ordinary_content() {
        let message = Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: format!("prose\n\n{}\n", CappedStream::Message.truncation_marker()),
            truncated: false,
        };
        let theme = Theme::system();
        let mut lines = Vec::new();

        render_message(&mut lines, &message, &theme, 80);

        let marker_lines = lines
            .iter()
            .filter(|line| rendered_text(line).contains(CappedStream::Message.truncation_marker()))
            .collect::<Vec<_>>();
        let [marker] = marker_lines.as_slice() else {
            panic!("content that reads like the marker renders once: {lines:?}");
        };
        assert!(
            !marker.style.add_modifier.contains(Modifier::ITALIC),
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
            &mut lines,
            &mut links,
            &activity,
            FoldStep::Folded,
            &Theme::system(),
            80,
        );

        assert_eq!(
            links
                .into_iter()
                .map(|link| link.target)
                .collect::<Vec<_>>(),
            ["https://example.com/first", "file:///tmp/second"]
        );
    }

    /// One entry a spacing fixture puts in the Transcript, in presentation
    /// order.
    enum Entry {
        Message(Message),
        Activity(Activity),
    }

    fn user_message(content: &str) -> Message {
        Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: content.to_owned(),
            truncated: false,
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
            session: Session {
                id: crate::protocol::SessionId::new(),
                workspace: Workspace {
                    path: PathBuf::from("/workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
            },
            revision: SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: Vec::new(),
            messages,
            activities,
            transcript,
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
        row_text(&view.window(0, row_count).lines)
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
                "    running 2 tests",
                "    all green",
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

        let rows = projected_rows(&snapshot, &folds, &groups);

        assert_eq!(
            rows,
            [
                "  ✓ Ran 2 commands",
                "    ✓ cargo fmt",
                "      reformatted 3 files",
                "      reformatted 1 file",
                "",
                "    ✓ cargo clippy",
            ]
        );
    }

    /// Reassigns an Activity to a Turn, so a fixture can state which Turn its
    /// entries belong to without spelling out every Activity kind.
    fn set_turn(activity: &mut Activity, turn_id: TurnId) {
        match activity {
            Activity::Status { turn_id: id, .. }
            | Activity::Error { turn_id: id, .. }
            | Activity::Command { turn_id: id, .. }
            | Activity::FileChange { turn_id: id, .. }
            | Activity::Reasoning { turn_id: id, .. } => *id = turn_id,
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
                prompt_id: PromptId::new(),
                agent: None,
                status,
                started_at: None,
                settled_at: None,
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

        let rows = projected_rows_through(&snapshot, &folds, &groups, &turns);

        assert_eq!(
            rows,
            [
                "┃ tidy the tree",
                "",
                "  ✓ Worked",
                "  ✓ Ran 2 commands",
                "    ✓ cargo fmt",
                "      reformatted 1 file",
                "",
                "    ✓ cargo clippy",
                "",
                "  Tidied.",
            ],
            "a Turn Fold opens onto the Groups and Folds the reader left behind it"
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
                "  Preparing the workspace",
                "  ✓ cargo test",
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

    /// Projects a Transcript through every disclosure axis a test drove.
    fn projected_view_through<'a>(
        cache: &'a TranscriptCache,
        snapshot: &SessionSnapshot,
        folds: &TranscriptFolds,
        groups: &TranscriptGroups,
        turns: &TranscriptTurnFolds,
    ) -> std::cell::Ref<'a, TranscriptView> {
        cache.view(
            0,
            snapshot,
            &[],
            TranscriptDisclosure {
                folds,
                groups,
                turns,
                reasoning_visibility: ReasoningVisibility::Shown,
            },
            &Theme::system(),
            80,
        )
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
                .lines
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

        let opening_on_it = view.window(1, 2).lines;
        let opening_past_it = view.window(2, 2).lines;

        assert_eq!(row_text(&opening_on_it), ["", "  ✓ cargo test"]);
        assert_eq!(
            row_text(&opening_past_it),
            ["  ✓ cargo test", "    running 2 tests"]
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

        let whole = view.window(0, view.row_count()).lines;

        assert_eq!(
            whole.len(),
            view.row_count(),
            "the reported row count must cover exactly the rows the window draws"
        );
        for (row, drawn) in whole.iter().enumerate() {
            let window = view.window(row, 1);
            assert_eq!(
                rendered_text(&window.lines[window.local_scroll]),
                rendered_text(drawn),
                "scrolling to row {row} must land on the same row the whole window draws"
            );
        }
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
        let snapshot = transcript_snapshot(vec![
            Entry::Activity(command("cargo check", "")),
            Entry::Activity(running),
        ]);
        let cache = TranscriptCache::default();
        let view = projected_view_through(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
            &TranscriptTurnFolds::default(),
        );

        let whole = view.window(0, view.row_count());
        assert_eq!(
            whole.spinner_lines.len(),
            1,
            "only the running command animates; the settled one keeps its outcome glyph"
        );
        let spinner_row = rendered_text(&whole.lines[whole.spinner_lines[0]]);
        assert!(
            spinner_row.contains(super::spinner::MARKER) && spinner_row.contains("cargo build"),
            "the recorded line is the running command's header: {spinner_row}"
        );

        let past_it = view.window(view.row_count().saturating_sub(1), 1);
        assert!(
            past_it.spinner_lines.is_empty(),
            "a window opening past the Marker records nothing to patch"
        );
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
        let view = projected_view(&cache, &snapshot, &TranscriptFolds::default(), &groups);

        let whole = view.window(0, view.row_count());
        let rows = whole
            .lines
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
        let view = projected_view(
            &cache,
            &snapshot,
            &TranscriptFolds::default(),
            &TranscriptGroups::default(),
        );

        let whole = view.window(0, view.row_count());
        assert_eq!(
            whole.spinner_lines.len(),
            1,
            "the Group speaks for the member still thinking, so its header is the one \
             Marker on screen"
        );
        let spinner_row = rendered_text(&whole.lines[whole.spinner_lines[0]]);
        assert_eq!(
            spinner_row.trim_end(),
            format!("  {}Thinking: Settling", super::spinner::MARKER),
            "the recorded line is the Group's live header"
        );
        let rendered = whole
            .lines
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
