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

use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{style::Modifier, text::Line};

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
    model_picker::{CatalogStatuses, FIX_INDENT, ModelPicker, fix_rows},
    slots::truncate_to_width,
    state::CommandId,
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

/// The Providers step: one row for each built-in Provider, in their fixed
/// order, ticked by its Enablement and beside its Availability. Space writes
/// the Enablement of the Provider under the cursor at once, and the step
/// draws only what the Server answers, so a row is never ticked that the
/// Config Document does not say is. The step is passed only while some
/// Provider is enabled and not found unavailable.
#[derive(Clone, Debug, Default)]
struct ProvidersStep {
    /// The row the cursor is on.
    cursor: usize,
}

impl ProvidersStep {
    /// The keys this step teaches before the ones every step shares.
    const KEYS: &'static str = "↑↓ Move · Space Toggle · r Re-check";

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
    }

    /// The edit turning the Provider under the cursor off where it is on and
    /// on where it is off, measured against the Settings in force.
    fn toggle(&self, settings: &EffectiveSettings) -> Option<SettingMutation> {
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

    fn lines(&self, width: usize, theme: &Theme, server: OwnServer<'_>) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
                truncate_to_width(
                    "Which Providers Suru may use, and whether each works here.",
                    width,
                ),
                theme.text.subdued,
            ),
            Line::default(),
        ];
        for (row, provider) in Self::providers().enumerate() {
            let selected = row == self.cursor;
            let standing = Standing::of(&provider.id, server);
            let marker = if selected { "› " } else { "  " };
            let tick = if matches!(standing, Standing::Disabled) {
                "[ ]"
            } else {
                "[x]"
            };
            lines.push(Line::styled(
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
            ));
            // What fixes an unavailable Provider is drawn whole beneath its
            // row, broken between words, as the Model picker draws it beneath
            // its retry row; what a warning or failed read says is a line.
            match standing {
                Standing::Unavailable { fix, .. } => {
                    let style = if selected {
                        theme.selection.focused
                    } else {
                        theme.text.primary
                    };
                    let columns = u16::try_from(width).unwrap_or(u16::MAX);
                    lines.extend(
                        fix_rows(fix, columns)
                            .into_iter()
                            .map(|part| Line::styled(format!("{FIX_INDENT}{}", &fix[part]), style)),
                    );
                }
                Standing::Warning(message) | Standing::Failed(message) => {
                    lines.push(Line::styled(
                        format!(
                            "{FIX_INDENT}{}",
                            truncate_to_width(message, width.saturating_sub(FIX_INDENT.len()))
                        ),
                        theme.text.subdued,
                    ));
                }
                Standing::Disabled
                | Standing::Checking
                | Standing::Available
                | Standing::Refreshing => {}
            }
        }
        if let Some(lock) = Self::lock(server) {
            lines.push(Line::default());
            lines.push(Line::styled(
                truncate_to_width(lock, width),
                theme.feedback.warning,
            ));
        }
        lines
    }
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
            Some(Step::Providers) => ProvidersStep::lock(server).is_some(),
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
    pub(super) fn shows_providers(&self) -> bool {
        self.step == Some(Step::Providers)
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
    /// foot.
    pub(super) fn lines(
        &self,
        width: usize,
        height: usize,
        theme: &Theme,
        server: OwnServer<'_>,
    ) -> Vec<Line<'static>> {
        let Some(step) = self.step else {
            return Vec::new();
        };
        let mut lines = self.body(step, width, theme, server);
        let own_keys = match step {
            Step::Providers => Some(ProvidersStep::KEYS),
            Step::Derivation | Step::Icons => None,
        };
        let footer = footer(step, own_keys);
        // The keys stay at the foot however much the step draws, and are
        // what gives way last.
        lines.truncate(height.saturating_sub(1));
        lines.resize(height.saturating_sub(1), Line::default());
        lines.push(Line::styled(
            truncate_to_width(&footer, width),
            theme.text.subdued,
        ));
        lines
    }

    /// How many rows the flow draws within `width` columns with nothing cut
    /// short, the keys at the foot included.
    pub(super) fn rows_wanted(&self, width: usize, theme: &Theme, server: OwnServer<'_>) -> usize {
        self.step
            .map_or(0, |step| self.body(step, width, theme, server).len() + 1)
    }

    /// The header naming `step`, and what the step draws beneath it.
    fn body(
        &self,
        step: Step,
        width: usize,
        theme: &Theme,
        server: OwnServer<'_>,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(
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
            ),
            Line::default(),
        ];
        lines.extend(match step {
            Step::Providers => self.providers.lines(width, theme, server),
            Step::Derivation => self.derivation.lines(width, theme),
            Step::Icons => self.icons.lines(width, theme),
        });
        lines
    }
}

/// The keys at the foot of `step`: the step's own keys first, where it has
/// any, then Next, or Finish on the last step, Back where there is a step
/// before, and how to leave.
fn footer(step: Step, own_keys: Option<&str>) -> String {
    let own = own_keys
        .map(|keys| format!("{keys} · "))
        .unwrap_or_default();
    let next = if step.next().is_some() {
        "Enter Next"
    } else {
        "Enter Finish"
    };
    let back = step.previous().map(|_| " · ← Back").unwrap_or_default();
    format!("{own}{next}{back} · Esc Leave")
}
