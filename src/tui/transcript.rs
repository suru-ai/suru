//! Cached projection of Session transcript content into renderable rows.
//!
//! The projection walks render units. A unit owns the transcript entries that
//! render as one block, and is what keying, memoization, and click hit-testing
//! address, so which entries share a unit is answered in one walk rather than
//! in each of those four places. Most units hold exactly one entry; a Group
//! holds a run of adjacent, successfully settled command Activities.
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
        Activity, ActivityId, FileChange, InitialPrompt, Message, MessageId, MessageRole, PromptId,
        SessionId, SessionRevision, SessionSnapshot, TranscriptItem,
    },
    theme::Theme,
};

use super::markdown;

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

/// One client's state for one disclosure axis of one Session's Transcript:
/// the posture the view leans to, plus the entries the reader flipped away
/// from it. Disclosure is presentation only, so this never reaches the
/// Session, never syncs between clients, and dies with the process.
#[derive(Clone, Debug, Default)]
struct DisclosureAxis {
    posture: DisclosurePosture,
    overrides: HashSet<ActivityId>,
}

impl DisclosureAxis {
    fn is_closed(&self, activity_id: ActivityId) -> bool {
        (self.posture == DisclosurePosture::Closed) != self.overrides.contains(&activity_id)
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

    fn set_closed(&mut self, activity_id: ActivityId, closed: bool) {
        if (self.posture == DisclosurePosture::Closed) == closed {
            self.overrides.remove(&activity_id);
        } else {
            self.overrides.insert(activity_id);
        }
    }

    /// Digest of the axis state, so the view cache rebuilds when it changes.
    fn fingerprint(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        (self.posture as u8).hash(&mut hasher);
        self.overrides.len().hash(&mut hasher);
        activity_id_set_digest(&self.overrides).hash(&mut hasher);
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

/// Order-independent digest of a set of Activity ids, so a view-state
/// fingerprint over one never depends on hash iteration order.
fn activity_id_set_digest(ids: &HashSet<ActivityId>) -> u64 {
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
pub(super) struct TranscriptGroups(DisclosureAxis);

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

/// The two disclosure axes a client renders a Transcript through: Groups
/// decide which rows exist, Folds how much of a row shows. Rendering reads
/// both, so they travel as one input.
#[derive(Clone, Copy)]
pub(super) struct TranscriptDisclosure<'a> {
    pub(super) folds: &'a TranscriptFolds,
    pub(super) groups: &'a TranscriptGroups,
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
            provisional_fingerprint: provisional_fingerprint(provisional),
            folds_fingerprint: disclosure.folds.fingerprint(),
            groups_fingerprint: disclosure.groups.fingerprint(),
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
    provisional_fingerprint: u64,
    /// Folds are a rendering input outside the Session snapshot, so ADR 0007
    /// requires them in the key or a flipped Fold would render a stale frame.
    folds_fingerprint: u64,
    /// Group state is the same kind of input, so a flipped Group rebuilds too.
    groups_fingerprint: u64,
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
    pub(super) fn window(
        &self,
        scroll_position: usize,
        viewport_rows: usize,
    ) -> (Vec<Line<'static>>, usize) {
        let first_line = self
            .line_starts
            .partition_point(|row| *row <= scroll_position)
            .saturating_sub(1);
        let window_start = self.line_starts.get(first_line).copied().unwrap_or(0);
        let local_scroll = scroll_position.saturating_sub(window_start);
        let rows_needed = local_scroll.saturating_add(viewport_rows);
        let mut lines = Vec::new();
        let mut rows = 0;
        let first_unit = self
            .units
            .partition_point(|unit| unit.start_line <= first_line)
            .saturating_sub(1);
        'units: for unit in &self.units[first_unit.min(self.units.len())..] {
            let skip = first_line.saturating_sub(unit.start_line);
            for (line, rows_of_line) in unit.lines.iter().zip(&unit.rows_per_line).skip(skip) {
                lines.push(line.clone());
                rows += rows_of_line;
                if rows >= rows_needed {
                    break 'units;
                }
            }
        }
        (lines, local_scroll)
    }
}

/// One block of the Transcript the projection renders as a whole: the entries
/// that share a cache key, a fingerprint, and a click target. A unit answers
/// for all three itself, so rendering, memoization, and hit-testing address
/// the unit rather than what it holds.
enum RenderUnit<'a> {
    Message(&'a Message),
    Activity(&'a Activity),
    /// A Group: a run of two or more adjacent command Activities, each settled
    /// Completed with exit status 0. Collapsed it is the run's single row;
    /// expanded it is the header the run re-collapses from, with its members
    /// following as their own units. Never empty and never a run of one;
    /// [`close_command_run`] holds that invariant.
    Group {
        members: Vec<&'a Activity>,
        expanded: bool,
    },
    /// One member of an expanded Group: an ordinary Activity drawn in the
    /// member gutter. A member is its own unit so its Fold, its cached lines,
    /// and its click target all work exactly as they do standalone.
    GroupMember(&'a Activity),
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
            Self::Group { members, expanded } => group_fingerprint(members, *expanded),
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
            // A provisional prompt's text only grows and carries no Fold, so
            // its length is the whole of its rendering input.
            Self::Provisional(prompt) => prompt.text.len() as u64,
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
            | Self::Provisional(_) => None,
        }
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
            Self::Group { members, expanded } => {
                Some(render_group(lines, members.len(), *expanded, theme))
            }
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
            Self::Provisional(prompt) => {
                push_user_message(lines, &prompt.text, theme, width);
                None
            }
        }
    }
}

/// Walks a Session's transcript into the units a view renders. Which entries
/// share a unit is decided here and nowhere else, so gathering a run of them
/// into one stays a change to this walk rather than to rendering or layout.
fn plan_units<'a>(
    snapshot: &'a SessionSnapshot,
    provisional: &[&'a InitialPrompt],
    groups: &TranscriptGroups,
) -> Vec<RenderUnit<'a>> {
    let messages: HashMap<MessageId, &Message> = snapshot
        .messages
        .iter()
        .map(|message| (message.id, message))
        .collect();
    let activities: HashMap<ActivityId, &Activity> = snapshot
        .activities
        .iter()
        .map(|activity| (activity.id(), activity))
        .collect();
    let mut units = Vec::with_capacity(snapshot.transcript.len() + provisional.len());
    let mut run: Vec<&Activity> = Vec::new();
    // A transcript entry naming content the snapshot does not carry projects
    // nothing rather than a gap, so it does not end a run either: the commands
    // around it are still adjacent as presented.
    for item in &snapshot.transcript {
        match item {
            TranscriptItem::Message { message_id } => {
                if let Some(message) = messages.get(message_id).copied() {
                    close_command_run(&mut units, &mut run, groups);
                    units.push(RenderUnit::Message(message));
                }
            }
            TranscriptItem::Activity { activity_id } => {
                if let Some(activity) = activities.get(activity_id).copied() {
                    if joins_command_run(activity) {
                        run.push(activity);
                    } else {
                        close_command_run(&mut units, &mut run, groups);
                        units.push(RenderUnit::Activity(activity));
                    }
                }
            }
        }
    }
    close_command_run(&mut units, &mut run, groups);
    units.extend(provisional.iter().copied().map(RenderUnit::Provisional));
    units
}

/// Whether an Activity extends a run of groupable commands: a command settled
/// Completed with exit status 0. Every other entry kind breaks the run, as
/// does a failed, interrupted, or still-running command, so anything worth
/// scanning for never hides behind a Group row.
fn joins_command_run(activity: &Activity) -> bool {
    matches!(
        activity,
        Activity::Command {
            status: crate::protocol::ActivityStatus::Completed,
            exit_status: Some(0),
            ..
        }
    )
}

/// Ends the current run of groupable commands: two or more become one Group,
/// while a run of one stays the ordinary command row it is, so grouping never
/// adds a layer where it saves nothing. A Group the reader expanded plans as
/// its header followed by each member as its own unit, so member Folds,
/// caching, and clicks need no Group-specific machinery.
fn close_command_run<'a>(
    units: &mut Vec<RenderUnit<'a>>,
    run: &mut Vec<&'a Activity>,
    groups: &TranscriptGroups,
) {
    if run.len() >= 2 {
        let members = std::mem::take(run);
        if groups.is_collapsed(members[0].id()) {
            units.push(RenderUnit::Group {
                members,
                expanded: false,
            });
        } else {
            units.push(RenderUnit::Group {
                members: members.clone(),
                expanded: true,
            });
            units.extend(members.into_iter().map(RenderUnit::GroupMember));
        }
    } else {
        units.extend(run.drain(..).map(RenderUnit::Activity));
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
    /// absorbs a command settling behind it, where a key over the member set
    /// would read the grown Group as a new unit and re-render it every settle.
    Group(ActivityId),
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
    start_line: usize,
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
    let mut units = plan_units(snapshot, provisional, disclosure.groups)
        .into_iter()
        .map(|unit| reuse_or_render(&mut reusable, &unit, disclosure.folds, theme, width))
        .collect::<Vec<_>>();

    let mut row_count = 0;
    let mut line_count = 0;
    let mut message_starts = Vec::new();
    let mut unit_starts = Vec::new();
    let mut line_starts = Vec::new();
    for unit in &mut units {
        unit.start_line = line_count;
        let unit_start_row = row_count;
        if let Some(message_id) = unit.message_id {
            message_starts.push(MessageStart {
                message_id,
                row: row_count,
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
        start_line: 0,
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

/// A Group unit renders from its membership and which way it is flipped, the
/// collapsed row and the expanded header being two drawings of the same unit.
/// Folds are deliberately absent: they act on the members, which render as
/// their own units when they render at all.
fn group_fingerprint(members: &[&Activity], expanded: bool) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    expanded.hash(&mut hasher);
    members.len().hash(&mut hasher);
    for member in members {
        member.id().hash(&mut hasher);
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
        Activity::Reasoning {
            status,
            title,
            content,
            content_truncated,
            duration_ms,
            ..
        } => Some(push_reasoning_activity(
            projection.lines,
            ReasoningActivity {
                status: *status,
                title: title.as_deref(),
                content,
                content_truncated: *content_truncated,
                duration_ms: *duration_ms,
            },
            step != FoldStep::Expanded,
            theme,
        )),
    }
}

/// Projects a Group's header: one row in the settled Activity-header idiom,
/// with the `Ran N commands` count styled as the toggle affordance it is.
/// Collapsed, the row stands in for its members and the count doubles as the
/// hidden-ness indicator, so no fold-marker line follows; expanded, the same
/// header leads the member units and is the one place the Group re-collapses
/// from. `member_count` is always at least two.
fn render_group(
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
        ActivityStatus::Active => ("$ ", theme.accent.primary),
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
        ActivityStatus::Active => ("… ", "Applying file changes", theme.accent.primary),
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
    use crate::protocol::ActivityStatus;

    let ReasoningActivity {
        status,
        title,
        content,
        content_truncated,
        duration_ms,
    } = activity;
    let (marker, label, style) = match status {
        ActivityStatus::Active => ("… ", REASONING_ACTIVE_LABEL, theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", REASONING_COMPLETED_LABEL, theme.text.subdued),
        ActivityStatus::Failed => ("× ", REASONING_FAILED_LABEL, theme.feedback.error),
    };
    let mut header = label.to_owned();
    if let Some(title) = title {
        header.push_str(": ");
        header.push_str(title);
    }
    if let Some(duration_ms) = duration_ms {
        header.push_str(" · ");
        header.push_str(&humanized_duration(duration_ms));
    }
    // The body is projected whether or not it will be shown, because a Fold
    // that hides all of it still has to say how many lines that is.
    let content = sanitize_content(content);
    let mut body = markdown::render(&content, theme)
        .into_iter()
        .map(|line| subdued_line(line, OUTPUT_INDENT, theme))
        .collect::<Vec<_>>();
    if content_truncated {
        push_truncation_marker(&mut body, CappedStream::Reasoning, OUTPUT_INDENT, theme);
    }
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
/// block that took seconds is not reported to the millisecond, and one that took
/// minutes is not reported as hundreds of seconds.
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
    lines.push(Line::default());
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
    if !content.is_empty() || truncated {
        lines.push(Line::default());
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
            ModelAvailability, Session, SessionRevision, SessionSnapshot, SessionStatus,
            TranscriptItem, TurnId, Workspace,
        },
        theme::Theme,
    };

    use super::{
        CappedStream, FoldStep, MAX_TRANSCRIPT_SOURCE_LINE_ROWS, TranscriptCache,
        TranscriptDisclosure, TranscriptFolds, TranscriptGroups, render_activity, render_message,
        split_oversized_line, wrapped_line_count,
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
        let snapshot = SessionSnapshot {
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
            messages: Vec::new(),
            activities: vec![activity.clone()],
            transcript: vec![TranscriptItem::Activity {
                activity_id: activity.id(),
            }],
        };
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
            },
            &first_theme,
            80,
        );
        let first_lines = first.window(0, 10).0;
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
            },
            &second_theme,
            80,
        );
        let second_lines = second.window(0, 10).0;

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
        let snapshot = SessionSnapshot {
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
            messages: Vec::new(),
            activities: vec![activity.clone()],
            transcript: vec![TranscriptItem::Activity {
                activity_id: activity.id(),
            }],
        };
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
}
