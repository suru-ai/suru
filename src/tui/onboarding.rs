//! View state for Onboarding: the guided pass a Client offers over the
//! Landing of its own Server until that Server has been onboarded, and that
//! `/onboarding` opens again at its first step whenever the reader asks.
//!
//! It walks three steps in a fixed order — Providers, Derivation, Icons —
//! under a "Step N of 3" header. Enter confirms the step on show and moves to
//! the next, finishing the flow on the last; Back returns to the step before;
//! Esc leaves the whole flow from any step. Leaving counts as taking it as
//! much as finishing does, so either asks the Server to mark itself
//! onboarded.
//!
//! The Client offers the flow while its own Server has not been onboarded,
//! and withdraws what it offered once the Server has been or the Client
//! turns to a Remote; a flow the reader asked for is theirs to leave.
//!
//! Which step is on show is the overlay's own business. Each step keeps its
//! state in a place of its own beside the others, so going Back and then Next
//! again finds a step as it was left, and opening the flow again starts every
//! step afresh. Each step is handed the keys first, and only what it leaves
//! is read as one of the keys every step shares.

use std::cell::Cell;

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    style::{Modifier, Style},
    text::Line,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{
        EffectiveSettings, Outlook, ProviderCatalogStatus, ProviderId, ProviderUnavailability,
        SettingMutation,
    },
    provider::{BuiltInProvider, built_in_providers},
    settings::provider_enablement,
    theme::Theme,
};

use super::{
    commands::SemanticCommandId,
    list_window::{ListWindow, WindowEntry},
    model_picker::{CatalogStatuses, ModelPicker},
    selection::WrappedLine,
    slots::truncate_to_width,
    state::CommandId,
    wrapped_text::{FIX_INDENT, WrappedText},
};

#[derive(Clone, Debug, Default)]
pub(super) struct Onboarding {
    /// The step on show, while the flow is open.
    step: Option<Step>,
    opening: Opening,
    providers: ProvidersStep,
    derivation: DerivationStep,
    icons: IconsStep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Providers,
    Derivation,
    Icons,
}

impl Step {
    const ALL: [Self; 3] = [Self::Providers, Self::Derivation, Self::Icons];

    /// Where the step stands in the flow, counted from one.
    fn number(self) -> usize {
        Self::ALL
            .iter()
            .position(|step| *step == self)
            .expect("every step is in the flow")
            + 1
    }

    fn next(self) -> Option<Self> {
        Self::ALL.get(self.number()).copied()
    }

    fn previous(self) -> Option<Self> {
        self.number()
            .checked_sub(2)
            .and_then(|index| Self::ALL.get(index).copied())
    }

    fn title(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Derivation => "Derivation",
            Self::Icons => "Icons",
        }
    }
}

/// How the flow came to be open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Opening {
    /// The Client offered it, its own Server not having been onboarded.
    #[default]
    Offered,
    /// The reader asked for it.
    Requested,
}

/// What confirming the step on show came to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Advance {
    /// The next step is on show.
    Moved,
    /// The last step was confirmed, so the flow is over and closed.
    Finished,
    /// The step on show may not be passed yet, and says why, so it stays.
    Held,
}

/// What the flow reads of the Client's own Server, of which it keeps no copy:
/// the Settings in force there and the Model Catalog last heard from it.
/// Onboarding is that Server's whichever Outlook is turned, so the catalog is
/// the Local Outlook's.
#[derive(Clone, Copy, Debug)]
pub(super) struct OwnServer<'a> {
    settings: &'a EffectiveSettings,
    catalog: CatalogStatuses<'a>,
}

impl<'a> OwnServer<'a> {
    pub(super) fn of(settings: &'a EffectiveSettings, model_picker: &'a ModelPicker) -> Self {
        Self {
            settings,
            catalog: model_picker.catalog_statuses(&Outlook::Local),
        }
    }
}

/// The flow as one frame draws it: its lines, and the lines among them it
/// wrapped itself, each placed by the line it begins on, which a selection
/// reads as they were written.
pub(super) struct Drawn<'a> {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) wrapped: Vec<WrappedLine<'a>>,
}

/// The Providers step: one row for each built-in Provider, in their fixed
/// order, ticked by its Enablement and beside its Availability. Space writes
/// the Enablement of the Provider under the cursor at once, and the step
/// draws only what the Server answers, so a row is never ticked that the
/// Config Document does not say is. The step is passed only while some
/// Provider is enabled and not found unavailable.
///
/// A screen too short for every row with what goes beneath it shows them
/// through a window that follows the cursor, so the Provider Space would
/// toggle is always on screen, and why Next is locked keeps its rows beneath
/// the window however few there are. A screen with no room left for the row
/// under the cursor is one Space and Enter act on nothing from.
///
/// `r` asks the Client's own Server to re-read every Provider, whichever
/// Outlook is on screen, as a request of the step's own.
#[derive(Clone, Debug, Default)]
struct ProvidersStep {
    /// The row the cursor is on.
    cursor: usize,
    window: ListWindow,
    /// Whether the last frame drew the row under the cursor, which is what
    /// Space and Enter may act on; nothing until a frame has drawn the step.
    cursor_drawn: Cell<Option<bool>>,
    recheck: Recheck,
}

/// Where the step's own re-read of every Provider stands.
#[derive(Clone, Debug, Default)]
enum Recheck {
    #[default]
    Idle,
    /// Asked for, under this request, and not yet answered.
    Awaiting(uuid::Uuid),
    /// The newest re-check was not answered, for this reason.
    Failed(String),
}

impl ProvidersStep {
    const INTRODUCTION: &'static str = "Which Providers Suru may use, and whether each works here.";

    /// The keys this step teaches before the ones every step shares, the
    /// most needed first, since a narrow screen gives up the last of them
    /// first.
    const KEYS: [&'static str; 3] = ["Space Toggle", "r Re-check", "↑↓ Move"];

    fn command_for_key(&self, key: KeyEvent) -> Option<CommandId> {
        let command = match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                SemanticCommandId::OnboardingProvidersPrevious
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                SemanticCommandId::OnboardingProvidersNext
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                SemanticCommandId::OnboardingProvidersToggle
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                SemanticCommandId::OnboardingProvidersRecheck
            }
            _ => return None,
        };
        Some(CommandId::InvokeSemantic(command))
    }

    /// The Providers the step lists. A Provider whose Enablement the schema
    /// does not define has no row, as on the settings panel, because the row
    /// is that Setting's surface.
    fn providers() -> impl Iterator<Item = &'static BuiltInProvider> {
        built_in_providers()
            .iter()
            .filter(|provider| provider_enablement(&provider.id).is_some())
    }

    /// Moves the cursor `distance` rows, wrapping past either end.
    fn move_cursor(&mut self, distance: isize) {
        let rows = Self::providers().count();
        if rows == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + distance).rem_euclid(rows as isize) as usize;
        self.window.reveal();
    }

    /// Whether the row under the cursor is one the reader can see: the last
    /// frame drew it, or no frame has drawn the step yet.
    fn cursor_seen(&self) -> bool {
        self.cursor_drawn.get() != Some(false)
    }

    /// The edit turning the Provider under the cursor off where it is on and
    /// on where it is off, measured against the Settings in force.
    fn toggle(&self, settings: &EffectiveSettings) -> Option<SettingMutation> {
        if !self.cursor_seen() {
            return None;
        }
        let provider = Self::providers().nth(self.cursor)?;
        Some(
            provider_enablement(&provider.id)?
                .next_choice(settings)?
                .mutation(),
        )
    }

    /// Why the step may not be passed yet, or nothing where some Provider is
    /// enabled and not found unavailable.
    fn lock(server: OwnServer<'_>) -> Option<&'static str> {
        let standings = Self::providers()
            .map(|provider| Standing::of(&provider.id, server))
            .collect::<Vec<_>>();
        if standings.iter().any(|standing| standing.functions()) {
            None
        } else if standings
            .iter()
            .all(|standing| matches!(standing, Standing::Disabled))
        {
            Some("Next is locked: enable at least one Provider.")
        } else {
            Some("Next is locked: no enabled Provider is available.")
        }
    }

    /// Each Provider's row as drawn within `width` columns, with what goes
    /// beneath it.
    fn entries<'a>(
        &self,
        width: usize,
        theme: &Theme,
        server: OwnServer<'a>,
    ) -> Vec<ProviderEntry<'a>> {
        Self::providers()
            .enumerate()
            .map(|(row, provider)| {
                let selected = row == self.cursor;
                let standing = Standing::of(&provider.id, server);
                let marker = if selected { "› " } else { "  " };
                let tick = if matches!(standing, Standing::Disabled) {
                    "[ ]"
                } else {
                    "[x]"
                };
                let line = Line::styled(
                    truncate_to_width(
                        &format!(
                            "{marker}{tick} {} · {}",
                            provider.display_name,
                            standing.label()
                        ),
                        width,
                    ),
                    match (selected, standing) {
                        (true, _) => theme.selection.focused,
                        (false, Standing::Disabled) => theme.text.subdued,
                        (false, _) => theme.text.primary,
                    },
                );
                // What fixes an unavailable Provider is drawn whole beneath
                // its row, as every surface naming one draws it; what a
                // warning or failed read says is a line.
                let beneath = match standing {
                    Standing::Unavailable { fix, .. } => Beneath::Fix(
                        WrappedText::new(fix, FIX_INDENT, u16::try_from(width).unwrap_or(u16::MAX)),
                        if selected {
                            theme.selection.focused
                        } else {
                            theme.text.primary
                        },
                    ),
                    Standing::Warning(message) | Standing::Failed(message) => {
                        Beneath::Message(Line::styled(
                            format!(
                                "{FIX_INDENT}{}",
                                truncate_to_width(
                                    message,
                                    width.saturating_sub(FIX_INDENT.width())
                                )
                            ),
                            theme.text.subdued,
                        ))
                    }
                    Standing::Disabled
                    | Standing::Checking
                    | Standing::Available
                    | Standing::Refreshing => Beneath::Nothing,
                };
                ProviderEntry { line, beneath }
            })
            .collect()
    }

    /// What the step says beneath its rows, each line laid out within
    /// `width` columns in the style it is drawn in, the first said the most
    /// needed: why Next is locked, and how the step's own re-check stands.
    fn notes<'a>(
        &'a self,
        width: usize,
        theme: &Theme,
        server: OwnServer<'_>,
    ) -> Vec<(WrappedText<'a>, Style)> {
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        let recheck = match &self.recheck {
            Recheck::Idle => None,
            Recheck::Awaiting(_) => Some(("Re-checking every Provider…", theme.text.subdued)),
            Recheck::Failed(failure) => Some((failure.as_str(), theme.feedback.error)),
        };
        Self::lock(server)
            .map(|lock| (lock, theme.feedback.warning))
            .into_iter()
            .chain(recheck)
            .map(|(text, style)| (WrappedText::new(text, "", width), style))
            .collect()
    }

    /// How many lines the step draws within `width` columns with nothing
    /// given way.
    fn rows_wanted(&self, width: usize, theme: &Theme, server: OwnServer<'_>) -> usize {
        let notes = self
            .notes(width, theme, server)
            .iter()
            .map(|(note, _)| note.height())
            .sum::<usize>();
        let rows = self
            .entries(width, theme, server)
            .iter()
            .map(ProviderEntry::rows)
            .sum::<usize>();
        // The gap beneath the header, the introduction and the gap beneath
        // it, the rows, and the gap above what is said beneath them.
        3 + rows + usize::from(notes > 0) + notes
    }

    /// The step drawn within `width` columns and `room` lines beneath the
    /// header: the introduction, the Providers' rows through the window, and
    /// what is said beneath them, which keeps its lines before any row does.
    /// For the rows to be drawn whole, the gap above what is said beneath
    /// them gives way first, then the gap beneath the introduction, then the
    /// gap beneath the header, then the introduction; past that the window
    /// shows the rows around the cursor. Records whether the row under the
    /// cursor was drawn.
    fn lines<'a>(
        &'a self,
        width: usize,
        room: usize,
        theme: &Theme,
        server: OwnServer<'a>,
    ) -> (Vec<Line<'static>>, Vec<WrappedLine<'a>>) {
        let entries = self.entries(width, theme, server);
        let mut said = 0;
        let notes = self
            .notes(width, theme, server)
            .into_iter()
            .map(|(note, style)| {
                let note = note.first(room.saturating_sub(said));
                said += note.height();
                (note, style)
            })
            .collect::<Vec<_>>();
        let rows = entries.iter().map(ProviderEntry::rows).sum::<usize>();
        let available = room.saturating_sub(said);
        // The lines each of what gives way takes, in the order it gives way.
        let gives_way = [usize::from(said > 0), 1, 1, 1];
        let given = (0..=gives_way.len())
            .find(|given| rows + gives_way[*given..].iter().sum::<usize>() <= available)
            .unwrap_or(gives_way.len());
        let notes_gap = given < 1 && said > 0;
        let introduction_gap = given < 2;
        let header_gap = given < 3;
        let introduction = given < 4;

        let mut lines = Vec::new();
        if header_gap {
            lines.push(Line::default());
        }
        if introduction {
            lines.push(Line::styled(
                truncate_to_width(Self::INTRODUCTION, width),
                theme.text.subdued,
            ));
        }
        if introduction_gap {
            lines.push(Line::default());
        }
        let capacity = available.saturating_sub(lines.len() + usize::from(notes_gap));
        let window = entries
            .iter()
            .map(|entry| WindowEntry::focusable(entry.rows()))
            .collect::<Vec<_>>();
        let shown = self.window.settle(&window, capacity, Some(self.cursor));
        let end = lines.len() + capacity;
        let mut wrapped = Vec::new();
        let mut cursor_drawn = false;
        for (row, entry) in entries
            .into_iter()
            .enumerate()
            .skip(shown.start)
            .take(shown.len())
        {
            // Only a row taller than the whole window is cut short, and then
            // from what goes beneath it.
            let room = end.saturating_sub(lines.len());
            if room == 0 {
                break;
            }
            cursor_drawn |= row == self.cursor;
            lines.push(entry.line);
            match entry.beneath {
                Beneath::Fix(fix, style) => {
                    let fix = fix.last(room - 1);
                    if fix.height() > 0 {
                        wrapped.push(fix.wrapped(lines.len()));
                        lines.extend(fix.lines(style));
                    }
                }
                Beneath::Message(line) if room > 1 => lines.push(line),
                Beneath::Message(_) | Beneath::Nothing => {}
            }
        }
        self.cursor_drawn.set(Some(cursor_drawn));
        if notes_gap {
            lines.push(Line::default());
        }
        for (note, style) in notes {
            if note.height() > 0 {
                wrapped.push(note.wrapped(lines.len()));
                lines.extend(note.lines(style));
            }
        }
        (lines, wrapped)
    }
}

/// One Provider's row as the step lays it out, and what goes beneath it.
struct ProviderEntry<'a> {
    line: Line<'static>,
    beneath: Beneath<'a>,
}

impl ProviderEntry<'_> {
    /// The lines the row takes with what goes beneath it.
    fn rows(&self) -> usize {
        1 + match &self.beneath {
            Beneath::Nothing => 0,
            Beneath::Fix(fix, _) => fix.height(),
            Beneath::Message(_) => 1,
        }
    }
}

/// What a Provider's row has beneath it.
enum Beneath<'a> {
    Nothing,
    /// What fixes it, drawn whole in the style given.
    Fix(WrappedText<'a>, Style),
    /// What a warning or a failed read says, on a line.
    Message(Line<'static>),
}

/// Where one Provider stands on the Providers step: off, or on and as the
/// Model Catalog last heard from the Client's own Server has it.
#[derive(Clone, Copy, Debug)]
enum Standing<'a> {
    /// Turned off, so Suru looks for nothing and the row claims nothing.
    Disabled,
    /// On, with nothing heard of it since: no catalog yet, or only one from
    /// before it was turned on, which the Server is reading anew.
    Checking,
    /// Serving its catalog, whether or not the newest refresh went through.
    Available,
    Refreshing,
    /// Usable, with a compatibility condition worth fixing.
    Warning(&'a str),
    /// The read itself failed, which is Suru's to report rather than a
    /// condition of the Provider.
    Failed(&'a str),
    /// Unusable until the reader does what `fix` says outside Suru.
    Unavailable {
        reason: ProviderUnavailability,
        fix: &'a str,
    },
}

impl<'a> Standing<'a> {
    fn of(provider: &ProviderId, server: OwnServer<'a>) -> Self {
        if !server.settings.provider_enabled(provider) {
            return Self::Disabled;
        }
        match server.catalog.status(provider) {
            None | Some(ProviderCatalogStatus::Disabled) => Self::Checking,
            Some(ProviderCatalogStatus::Fresh | ProviderCatalogStatus::Stale { .. }) => {
                Self::Available
            }
            Some(ProviderCatalogStatus::Refreshing) => Self::Refreshing,
            Some(ProviderCatalogStatus::Warning { message }) => Self::Warning(message),
            Some(ProviderCatalogStatus::Failed { message }) => Self::Failed(message),
            Some(ProviderCatalogStatus::Unavailable { reason, fix, .. }) => Self::Unavailable {
                reason: *reason,
                fix,
            },
        }
    }

    /// Whether a Provider standing so is one a Turn may begin with: enabled
    /// and not found unavailable. A warning or a refresh under way is no bar.
    fn functions(self) -> bool {
        !matches!(self, Self::Disabled | Self::Unavailable { .. })
    }

    /// What the row says of it.
    fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Checking => "checking",
            Self::Available => "available",
            Self::Refreshing => "refreshing",
            Self::Warning(_) => "warning",
            Self::Failed(_) => "error",
            Self::Unavailable { reason, .. } => reason.label(),
        }
    }
}

/// The Derivation step: which Agent Selection derives Titles, Icons, and
/// branch names.
#[derive(Clone, Debug, Default)]
struct DerivationStep;

impl DerivationStep {
    fn command_for_key(&self, _key: KeyEvent) -> Option<CommandId> {
        None
    }

    fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        vec![Line::styled(
            truncate_to_width(
                "Which Agent Selection derives Titles, Icons, and branch names.",
                width,
            ),
            theme.text.subdued,
        )]
    }
}

/// The Icons step: whether this terminal draws Icons.
#[derive(Clone, Debug, Default)]
struct IconsStep;

impl IconsStep {
    fn command_for_key(&self, _key: KeyEvent) -> Option<CommandId> {
        None
    }

    fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        vec![Line::styled(
            truncate_to_width("Whether this terminal draws Icons.", width),
            theme.text.subdued,
        )]
    }
}

impl Onboarding {
    pub(super) fn is_open(&self) -> bool {
        self.step.is_some()
    }

    /// Opens the flow at its first step, every step afresh.
    pub(super) fn open(&mut self, opening: Opening) {
        *self = Self {
            step: Some(Step::Providers),
            opening,
            ..Self::default()
        };
    }

    pub(super) fn close(&mut self) {
        *self = Self::default();
    }

    /// Closes the flow where the Client offered it, untaken; one the reader
    /// asked for stays open.
    pub(super) fn withdraw(&mut self) {
        if self.opening == Opening::Offered {
            self.close();
        }
    }

    /// Confirms the step on show: moves to the next, or on the last step
    /// finishes the flow and closes it, unless the step may not be passed yet.
    pub(super) fn next(&mut self, server: OwnServer<'_>) -> Advance {
        let held = match self.step {
            // Nor is a step passed whose rows the screen had no room for.
            Some(Step::Providers) => {
                ProvidersStep::lock(server).is_some() || !self.providers.cursor_seen()
            }
            Some(Step::Derivation | Step::Icons) | None => false,
        };
        if held {
            return Advance::Held;
        }
        match self.step.and_then(Step::next) {
            Some(next) => {
                self.step = Some(next);
                Advance::Moved
            }
            None => {
                self.close();
                Advance::Finished
            }
        }
    }

    /// Returns to the step before the one on show, where there is one.
    pub(super) fn back(&mut self) {
        if let Some(previous) = self.step.and_then(Step::previous) {
            self.step = Some(previous);
        }
    }

    /// Whether the Providers step is on show, which is where its own commands
    /// act and nowhere else.
    fn shows_providers(&self) -> bool {
        self.step == Some(Step::Providers)
    }

    /// Begins a re-read of every Provider from the Providers step, while it
    /// is on show, answering the request the Client's own Server is to be
    /// asked under. A newer re-check takes the place of one still awaited.
    pub(super) fn begin_recheck(&mut self) -> Option<uuid::Uuid> {
        self.shows_providers().then(|| {
            let recheck = uuid::Uuid::new_v4();
            self.providers.recheck = Recheck::Awaiting(recheck);
            recheck
        })
    }

    /// Takes the Server's answer to `recheck`: what it read reaches the rows
    /// through the catalog, and here only whether the newest re-check failed.
    /// An answer to one since superseded settles nothing.
    pub(super) fn settle_recheck(&mut self, recheck: uuid::Uuid, failure: Option<String>) {
        if !matches!(self.providers.recheck, Recheck::Awaiting(awaited) if awaited == recheck) {
            return;
        }
        self.providers.recheck = failure.map_or(Recheck::Idle, |error| {
            Recheck::Failed(format!("Could not re-check the Providers: {error}"))
        });
    }

    /// Moves the Providers step's cursor to the Provider above.
    pub(super) fn select_previous_provider(&mut self) {
        if self.shows_providers() {
            self.providers.move_cursor(-1);
        }
    }

    /// Moves the Providers step's cursor to the Provider below.
    pub(super) fn select_next_provider(&mut self) {
        if self.shows_providers() {
            self.providers.move_cursor(1);
        }
    }

    /// The edit that turns the Provider under the Providers step's cursor on
    /// or off, while that step is on show.
    pub(super) fn toggle_provider(&self, settings: &EffectiveSettings) -> Option<SettingMutation> {
        self.shows_providers()
            .then(|| self.providers.toggle(settings))
            .flatten()
    }

    /// What `event` asks of the flow: whatever the step on show makes of it
    /// first, and otherwise one of the keys every step shares — Enter for
    /// Next, Left or Shift+Tab for Back, and Esc to leave.
    pub(super) fn command_for_event(&self, event: InputEvent) -> Option<CommandId> {
        let step = self.step?;
        let InputEvent::Key(key) = event else {
            return None;
        };
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let own = match step {
            Step::Providers => self.providers.command_for_key(key),
            Step::Derivation => self.derivation.command_for_key(key),
            Step::Icons => self.icons.command_for_key(key),
        };
        own.or_else(|| {
            let shared = match (key.code, key.modifiers) {
                (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::OnboardingNext,
                (KeyCode::Left, KeyModifiers::NONE)
                | (KeyCode::BackTab, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    SemanticCommandId::OnboardingBack
                }
                (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::OnboardingLeave,
                _ => return None,
            };
            Some(CommandId::InvokeSemantic(shared))
        })
    }

    /// The flow as drawn within `width` columns and `height` rows: the header
    /// naming the step, what the step on show draws, and the keys at the
    /// foot, which stay there however much the step draws.
    pub(super) fn lines<'a>(
        &'a self,
        width: usize,
        height: usize,
        theme: &Theme,
        server: OwnServer<'a>,
    ) -> Drawn<'a> {
        let Some(step) = self.step else {
            return Drawn {
                lines: Vec::new(),
                wrapped: Vec::new(),
            };
        };
        let mut lines = vec![Line::styled(
            truncate_to_width(
                &format!(
                    "Step {} of {} · {}",
                    step.number(),
                    Step::ALL.len(),
                    step.title()
                ),
                width,
            ),
            theme.accent.primary.add_modifier(Modifier::BOLD),
        )];
        let mut wrapped = Vec::new();
        match step {
            // The Providers step draws the gap beneath the header itself, as
            // one of what gives way on a short screen before its rows.
            Step::Providers => {
                let room = height.saturating_sub(lines.len() + 1);
                let (step_lines, step_wrapped) = self.providers.lines(width, room, theme, server);
                let top = lines.len();
                wrapped.extend(step_wrapped.into_iter().map(|line| WrappedLine {
                    row: top + line.row,
                    ..line
                }));
                lines.extend(step_lines);
            }
            Step::Derivation => {
                lines.push(Line::default());
                lines.extend(self.derivation.lines(width, theme));
            }
            Step::Icons => {
                lines.push(Line::default());
                lines.extend(self.icons.lines(width, theme));
            }
        }
        let own_keys: &[&str] = match step {
            Step::Providers => &ProvidersStep::KEYS,
            Step::Derivation | Step::Icons => &[],
        };
        let footer = footer(step, own_keys, width);
        // The keys stay at the foot however much the step draws, and are
        // what gives way last.
        lines.truncate(height.saturating_sub(1));
        lines.resize(height.saturating_sub(1), Line::default());
        lines.push(Line::styled(
            truncate_to_width(&footer, width),
            theme.text.subdued,
        ));
        Drawn { lines, wrapped }
    }

    /// How many rows the flow draws within `width` columns with nothing
    /// given way, the header and the keys at the foot included.
    pub(super) fn rows_wanted(&self, width: usize, theme: &Theme, server: OwnServer<'_>) -> usize {
        let Some(step) = self.step else {
            return 0;
        };
        let step_rows = match step {
            Step::Providers => self.providers.rows_wanted(width, theme, server),
            Step::Derivation => 1 + self.derivation.lines(width, theme).len(),
            Step::Icons => 1 + self.icons.lines(width, theme).len(),
        };
        1 + step_rows + 1
    }
}

/// The keys at the foot of `step` within `width` columns: the step's own
/// keys first, then Next, or Finish on the last step, Back where there is a
/// step before, and how to leave. Where they do not all fit, the step's own
/// give way first, the last of them first, then Back, and then Enter is
/// named by its glyph, since going on and leaving are what the reader cannot
/// do without.
fn footer(step: Step, own_keys: &[&str], width: usize) -> String {
    let (next, glyph_next) = if step.next().is_some() {
        ("Enter Next", "↵ Next")
    } else {
        ("Enter Finish", "↵ Finish")
    };
    let back = step.previous().map(|_| "← Back");
    let keys = |own: &[&str], next: &str, back: Option<&str>| {
        own.iter()
            .copied()
            .chain([next])
            .chain(back)
            .chain(["Esc Leave"])
            .collect::<Vec<_>>()
            .join(" · ")
    };
    (0..=own_keys.len())
        .rev()
        .map(|own| keys(&own_keys[..own], next, back))
        .chain([keys(&[], next, None)])
        .find(|keys| keys.width() <= width)
        .unwrap_or_else(|| keys(&[], glyph_next, None))
}
