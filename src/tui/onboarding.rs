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

use crate::theme::Theme;

use super::{commands::SemanticCommandId, slots::truncate_to_width, state::CommandId};

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
}

/// The Providers step: which Providers Suru may use, each beside whether it
/// is available.
#[derive(Clone, Debug, Default)]
struct ProvidersStep;

impl ProvidersStep {
    fn command_for_key(&self, _key: KeyEvent) -> Option<CommandId> {
        None
    }

    fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        vec![Line::styled(
            truncate_to_width(
                "Which Providers Suru may use, and whether each is available here.",
                width,
            ),
            theme.text.subdued,
        )]
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
    /// finishes the flow and closes it.
    pub(super) fn next(&mut self) -> Advance {
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
    pub(super) fn lines(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(step) = self.step else {
            return Vec::new();
        };
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
            Step::Providers => self.providers.lines(width, theme),
            Step::Derivation => self.derivation.lines(width, theme),
            Step::Icons => self.icons.lines(width, theme),
        });
        let footer = footer(step);
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
}

/// The keys at the foot of `step`: Next, or Finish on the last step, Back
/// where there is a step before, and how to leave.
fn footer(step: Step) -> String {
    let next = if step.next().is_some() {
        "Enter Next"
    } else {
        "Enter Finish"
    };
    let back = step.previous().map(|_| " · ← Back").unwrap_or_default();
    format!("{next}{back} · Esc Leave")
}
