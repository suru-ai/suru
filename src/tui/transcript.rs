//! Cached projection of Session transcript content into renderable rows.
//!
//! The projection walks render units. A unit owns the transcript entries that
//! render as one block, and is what keying, memoization, and click hit-testing
//! address, so which entries share a unit is answered in one walk rather than
//! in each of those four places. Every unit holds exactly one entry today; a
//! Group will hold a run of adjacent command Activities.
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
/// how much text that per-frame wrap can touch. Splitting is recursive
/// bisection that re-measures each half, so a lower cap trades one-time
/// projection cost on pathological lines for a lower per-frame ceiling.
const MAX_TRANSCRIPT_SOURCE_LINE_ROWS: usize = 1_000;

/// Wrapped rows a settled command Activity's output occupies while folded,
/// split into a head and a tail around the fold marker.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const FOLDED_COMMAND_OUTPUT_ROWS: usize = 6;

/// Wrapped rows of live tail an Active command Activity shows while it streams,
/// before it settles into the head-and-tail form.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const LIVE_COMMAND_TAIL_ROWS: usize = 3;

/// The gutter an Activity's subordinate content sits in, so a fold or
/// truncation marker lines up with the lines it stands in for.
const OUTPUT_INDENT: &str = "    ";

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

/// Which way a Session's Transcript leans before any per-entry override.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FoldPosture {
    /// Entries are folded unless the reader expanded that one.
    #[default]
    Folded,
    /// Entries are expanded unless the reader folded that one.
    Expanded,
}

/// One client's Fold state for one Session's Transcript: the posture the view
/// leans to, plus the entries the reader flipped away from it. Folds are
/// presentation only, so this never reaches the Session, never syncs between
/// clients, and dies with the process.
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptFolds {
    posture: FoldPosture,
    overrides: HashSet<ActivityId>,
}

impl TranscriptFolds {
    pub(super) fn is_folded(&self, activity_id: ActivityId) -> bool {
        (self.posture == FoldPosture::Folded) != self.overrides.contains(&activity_id)
    }

    /// Flips the whole view between folded-by-default and expanded-by-default.
    /// Per-entry overrides are dropped so one invocation always reaches a
    /// posture the reader can predict.
    pub(super) fn toggle_posture(&mut self) {
        self.posture = match self.posture {
            FoldPosture::Folded => FoldPosture::Expanded,
            FoldPosture::Expanded => FoldPosture::Folded,
        };
        self.overrides.clear();
    }

    pub(super) fn expand(&mut self, activity_id: ActivityId) {
        self.set_folded(activity_id, false);
    }

    pub(super) fn fold(&mut self, activity_id: ActivityId) {
        self.set_folded(activity_id, true);
    }

    fn set_folded(&mut self, activity_id: ActivityId, folded: bool) {
        if (self.posture == FoldPosture::Folded) == folded {
            self.overrides.remove(&activity_id);
        } else {
            self.overrides.insert(activity_id);
        }
    }

    /// Order-independent digest of the Fold state, so the view cache rebuilds
    /// when a Fold changes without depending on hash iteration order.
    fn fingerprint(&self) -> u64 {
        let mut overrides = 0u64;
        for activity_id in &self.overrides {
            let mut hasher = std::hash::DefaultHasher::new();
            activity_id.hash(&mut hasher);
            overrides ^= hasher.finish();
        }
        let mut hasher = std::hash::DefaultHasher::new();
        (self.posture as u8).hash(&mut hasher);
        self.overrides.len().hash(&mut hasher);
        overrides.hash(&mut hasher);
        hasher.finish()
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
        folds: &TranscriptFolds,
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
            folds_fingerprint: folds.fingerprint(),
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
                folds,
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
    /// A prompt this client sent that the Session has not echoed back yet.
    Provisional(&'a InitialPrompt),
}

impl RenderUnit<'_> {
    fn key(&self) -> UnitKey {
        match self {
            Self::Message(message) => UnitKey::Message(message.id),
            Self::Activity(activity) => UnitKey::Activity(activity.id()),
            Self::Provisional(prompt) => UnitKey::Provisional(prompt.id),
        }
    }

    /// Identifies everything the unit's rendering reads, so it re-renders when
    /// any entry it holds changes and reuses its lines when none did.
    fn fingerprint(&self, folds: &TranscriptFolds) -> u64 {
        match self {
            Self::Message(message) => message_fingerprint(message),
            Self::Activity(activity) => {
                activity_fingerprint(activity, folds.is_folded(activity.id()))
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
            Self::Activity(_) | Self::Provisional(_) => None,
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
                folds.is_folded(activity.id()),
                theme,
                width,
            ),
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
    // A transcript entry naming content the snapshot does not carry projects
    // nothing rather than a gap.
    for item in &snapshot.transcript {
        match item {
            TranscriptItem::Message { message_id } => {
                if let Some(message) = messages.get(message_id).copied() {
                    units.push(RenderUnit::Message(message));
                }
            }
            TranscriptItem::Activity { activity_id } => {
                if let Some(activity) = activities.get(activity_id).copied() {
                    units.push(RenderUnit::Activity(activity));
                }
            }
        }
    }
    units.extend(provisional.iter().copied().map(RenderUnit::Provisional));
    units
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
/// lines form its header, and whether it held content back.
#[derive(Clone, Copy, Debug)]
struct LaidOutAnchor {
    header_lines: usize,
    hides_content: bool,
}

fn rebuild(
    previous: Option<TranscriptView>,
    key: ViewKey,
    snapshot: &SessionSnapshot,
    provisional: &[&InitialPrompt],
    folds: &TranscriptFolds,
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
    let mut units = plan_units(snapshot, provisional)
        .into_iter()
        .map(|unit| reuse_or_render(&mut reusable, &unit, folds, theme, width))
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
    let mut lines = Vec::with_capacity(rendered.len());
    let mut header_lines = 0;
    for (index, line) in rendered.into_iter().enumerate() {
        split_oversized_line(line, width, &mut lines);
        if rendered_anchor.is_some_and(|anchor| index + 1 == anchor.header_source_lines) {
            header_lines = lines.len();
        }
    }
    let rows_per_line = lines
        .iter()
        .map(|line| wrapped_line_count(line, width))
        .collect::<Vec<_>>();
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
        }),
    }
}

/// What a projected unit reports about the block it pushed: how many of those
/// lines form the header a reader clicks to close it again, and whether it
/// held any content back. The count is in pre-split source lines, which layout
/// resolves to rows.
#[derive(Clone, Copy, Debug)]
struct UnitAnchor {
    header_source_lines: usize,
    hides_content: bool,
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

/// Identifies an Activity's rendered form. The Fold state joins the content
/// signals because a folded entry renders different lines from the same
/// Activity.
fn activity_fingerprint(activity: &Activity, folded: bool) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    folded.hash(&mut hasher);
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
    folded: bool,
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
            folded,
            theme,
            width,
        )),
        Activity::FileChange {
            status, changes, ..
        } => Some(push_file_change_activity(
            projection.lines,
            *status,
            changes,
            folded,
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
            folded,
            theme,
        )),
    }
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

/// Projects a command Activity. Output is projected in full first so hyperlink
/// targets survive, and only then clamped, so a Fold changes presentation and
/// nothing else.
fn push_command_activity(
    projection: &mut ActivityProjection<'_>,
    activity: CommandActivity<'_>,
    folded: bool,
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
    let command = match (status, exit_status) {
        (ActivityStatus::Failed, Some(exit_status)) => {
            format!("{command} (exit {exit_status})")
        }
        _ => command.to_owned(),
    };
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
    if !output.is_empty() {
        let mut output_lines = Vec::new();
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
        if folded {
            let projected = output_lines.len();
            output_lines = fold_command_output(output_lines, status, theme, width);
            hides_content = output_lines.len() != projected;
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
    }
}

/// Clamps a command's projected output to its Fold budget, splitting it into a
/// head and a tail joined by the fold marker. An Active command keeps only a
/// live tail of what it is writing now; a settled one keeps both ends.
///
/// Rows are counted after wrapping so a handful of very long lines cannot
/// flood the budget, while the marker counts the source lines it replaced, so
/// the number a reader sees does not shift when the terminal is resized.
fn fold_command_output(
    mut lines: Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    use crate::protocol::ActivityStatus;

    let (head_rows, tail_rows) = match status {
        ActivityStatus::Active => (0, LIVE_COMMAND_TAIL_ROWS),
        ActivityStatus::Completed | ActivityStatus::Failed => (
            FOLDED_COMMAND_OUTPUT_ROWS / 2,
            FOLDED_COMMAND_OUTPUT_ROWS - FOLDED_COMMAND_OUTPUT_ROWS / 2,
        ),
    };
    let rows_per_line = lines
        .iter()
        .map(|line| wrapped_line_count(line, width).max(1))
        .collect::<Vec<_>>();
    if rows_per_line.iter().sum::<usize>() <= head_rows + tail_rows {
        return lines;
    }
    let mut head_end = 0;
    let mut head_used = 0;
    while head_end < lines.len() && head_used + rows_per_line[head_end] <= head_rows {
        head_used += rows_per_line[head_end];
        head_end += 1;
    }
    let mut tail_start = lines.len();
    let mut tail_used = 0;
    while tail_start > head_end && tail_used + rows_per_line[tail_start - 1] <= tail_rows {
        tail_used += rows_per_line[tail_start - 1];
        tail_start -= 1;
    }
    let hidden = tail_start - head_end;
    if hidden == 0 {
        return lines;
    }
    let tail = lines.split_off(tail_start);
    lines.truncate(head_end);
    lines.push(fold_marker_line(hidden, "lines", OUTPUT_INDENT, theme));
    lines.extend(tail);
    lines
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
    UnitAnchor {
        header_source_lines,
        hides_content: hidden > 0,
    }
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
        return UnitAnchor {
            header_source_lines,
            hides_content: !body.is_empty(),
        };
    }
    lines.append(&mut body);
    UnitAnchor {
        header_source_lines,
        hides_content: false,
    }
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
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn split_oversized_line(line: Line<'static>, width: u16, output: &mut Vec<Line<'static>>) {
    if wrapped_line_count(&line, width) <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push(line);
        return;
    }
    let character_count = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    if character_count < 2 {
        output.push(line);
        return;
    }
    let (left, right) = split_line_at_character_midpoint(line, character_count);
    split_oversized_line(left, width, output);
    split_oversized_line(right, width, output);
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
        style::{Color, Modifier},
        text::Line,
    };

    use crate::{
        protocol::{
            Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
            ModelAvailability, Session, SessionRevision, SessionSnapshot, SessionStatus,
            TranscriptItem, TurnId, Workspace,
        },
        theme::Theme,
    };

    use super::{CappedStream, TranscriptCache, TranscriptFolds, render_activity, render_message};

    fn rendered_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| &*span.content).collect()
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

        render_activity(&mut lines, &mut links, &activity, true, &theme, 80);

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

        render_activity(&mut lines, &mut links, &activity, true, &theme, 80);

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
        let mut first_theme = Theme::system();
        first_theme.ansi.normal.red = Color::Rgb(1, 2, 3);
        let first = cache.view(
            0,
            &snapshot,
            &[],
            &TranscriptFolds::default(),
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
            &TranscriptFolds::default(),
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

        let folded_rows = cache
            .view(0, &snapshot, &[], &folds, &theme, 80)
            .row_count();
        folds.expand(activity.id());
        let expanded_rows = cache
            .view(0, &snapshot, &[], &folds, &theme, 80)
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

        render_activity(&mut lines, &mut links, &activity, true, &theme, 80);

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

        render_activity(&mut lines, &mut links, &activity, false, &theme, 80);

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

        render_activity(&mut lines, &mut links, &activity, true, &theme, 80);

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
            true,
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
