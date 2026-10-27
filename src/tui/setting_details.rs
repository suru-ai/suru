//! What the settings panel tells about the row focus stands on, beyond the row
//! that lists it: whatever asks something of the reader first — an edit that
//! never reached the Config Document, a Provider's condition and what fixes
//! it, a pin Suru ignored — then what the Setting does, every value it accepts
//! with the one in force and the built-in default marked, and where it applies
//! and what a Sidekick may do with it. A wide panel draws all of it in a pane
//! beside its rows; a narrow one keeps a headline above them instead.

use std::borrow::Cow;

use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{EffectiveSettings, SettingScope, SettingsDiagnostic},
    settings::{SettingChoiceSurface, SettingDescriptor, SettingValues, SidekickAccess},
    theme::Theme,
};

use super::{
    drawn_pane::DrawnPane,
    settings_panel::{RowAvailability, SettingsPanel},
};

/// The marks a value the Setting accepts leads with: whether it is the one in
/// force. A list's entries after its first, and what else an Open Setting
/// accepts, stand in as far.
const IN_FORCE: &str = "● ";
const NOT_IN_FORCE: &str = "○ ";
const VALUE_INDENT: &str = "  ";

/// What the built-in default is tagged with among the values, and the fewest
/// columns standing between the tag and the value it tags.
const DEFAULT_TAG: &str = "default";
const DEFAULT_TAG_GAP: usize = 3;

/// The labels of the facts the pane ends on, and how far each fact's value
/// stands in past them, wrapped rows and all: the widest label, and two
/// columns more.
const APPLIES: &str = "Applies";
const SIDEKICKS: &str = "Sidekicks";
const FACT_INDENT: &str = "           ";

/// The pane a wide panel details its focused row in. It holds each line it
/// wraps as the pane writes it, so a selection across a wrapped description or
/// fix reads it back as the one line it was written as (ADR-0020).
///
/// The pane is an account of what the row stands for, headed by its name,
/// with whatever asks something of the reader standing between that heading
/// and the rest. The leads are the panel's to say of whichever row focus
/// stands on; the account is the Setting's, and a Provider's row stands for
/// its Enablement, so for now it is accounted for as that Setting is.
pub(super) struct DetailsPane<'a> {
    leads: Vec<Lead<'a>>,
    account: SettingAccount,
}

/// Something the pane leads with because it asks something of the reader
/// before anything about the Setting does.
struct Lead<'a> {
    text: Cow<'a, str>,
    tone: Tone,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Tone {
    /// Something went wrong, the reader's edit or Suru's read of a Provider.
    Error,
    /// Something the reader would do well to fix.
    Warning,
    /// What fixes the condition above it, which it stands beneath unparted.
    Fix,
}

/// What the pane says of the Setting itself.
struct SettingAccount {
    descriptor: &'static SettingDescriptor,
    values: Vec<AcceptedValue>,
    /// What else an Open Setting accepts beyond the values the pane lists,
    /// phrased to follow them.
    accepts: Option<String>,
    pinned: bool,
    sidekicks: Cow<'static, str>,
}

/// One value the pane lists the Setting as accepting.
struct AcceptedValue {
    /// The value as its row spells it, or a list's entries one to a row.
    rows: Vec<String>,
    in_force: bool,
    default: bool,
}

impl<'a> DetailsPane<'a> {
    /// The pane for the row focus stands on in an open `panel`, and none for a
    /// panel focused on nothing. Leading it is everything the narrow panel's
    /// headline and fix choose between, whole: the last edit's failure, then
    /// the focused Provider's condition and what fixes it, then whatever the
    /// Config Documents' diagnostics say of the Setting's own pin.
    pub(super) fn of(
        panel: &'a SettingsPanel,
        settings: &EffectiveSettings,
        pinned: &[String],
        diagnostics: &'a [SettingsDiagnostic],
    ) -> Option<Self> {
        let descriptor = panel.selected_descriptor()?;
        let mut leads = Vec::new();
        if let Some(error) = panel.error() {
            leads.push(Lead {
                text: Cow::Borrowed(error),
                tone: Tone::Error,
            });
        }
        if let Some(message) = panel.selected_message(settings) {
            leads.push(Lead {
                text: Cow::Borrowed(message),
                tone: match panel.selected_availability(settings) {
                    Some(RowAvailability::Failed) => Tone::Error,
                    _ => Tone::Warning,
                },
            });
        }
        if let Some(fix) = panel.selected_fix(settings) {
            leads.push(Lead {
                text: Cow::Borrowed(fix),
                tone: Tone::Fix,
            });
        }
        leads.extend(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.key.as_deref() == Some(descriptor.key))
                .map(|diagnostic| Lead {
                    text: Cow::Owned(format!(
                        "{}: {}",
                        diagnostic.file.display(),
                        diagnostic.message
                    )),
                    tone: Tone::Warning,
                }),
        );
        Some(Self {
            leads,
            account: SettingAccount::new(
                descriptor,
                settings,
                pinned.iter().any(|key| key == descriptor.key),
            ),
        })
    }

    /// The pane in `height` rows `width` columns wide. A pane with more to
    /// say than it has rows for ends on an ellipsis rather than silently.
    pub(super) fn draw(&self, width: u16, height: usize, theme: &Theme) -> DrawnPane<'_> {
        let mut drawn = DrawnPane::default();
        self.account.draw_heading(&mut drawn, width, theme);
        for (index, lead) in self.leads.iter().enumerate() {
            if index == 0 || lead.tone != Tone::Fix {
                drawn.lines.push(Line::default());
            }
            let style = match lead.tone {
                Tone::Error => theme.feedback.error,
                Tone::Warning => theme.feedback.warning,
                Tone::Fix => theme.text.primary,
            };
            drawn.wrap(&lead.text, "", width, style);
        }
        self.account.draw(&mut drawn, width, theme);
        drawn.fit(height, theme);
        drawn
    }
}

impl SettingAccount {
    fn new(
        descriptor: &'static SettingDescriptor,
        settings: &EffectiveSettings,
        pinned: bool,
    ) -> Self {
        let in_force = descriptor.spelling(settings);
        let default = descriptor.default_spelling();
        let named = descriptor.values.named();
        let mut values = named
            .iter()
            .map(|choice| AcceptedValue {
                rows: vec![choice.value.to_owned()],
                in_force: in_force.as_deref() == Some(choice.value),
                default: default.as_deref() == Some(choice.value),
            })
            .collect::<Vec<_>>();
        // An Open Setting may hold a value the schema never named: its
        // built-in default, and the value in force where that is neither.
        let unnamed = |spelling: &Option<Cow<'static, str>>| {
            spelling
                .clone()
                .filter(|spelling| named.iter().all(|choice| choice.value != spelling))
        };
        if let Some(spelling) = unnamed(&default) {
            values.push(AcceptedValue {
                rows: rows_of(descriptor, &EffectiveSettings::default(), spelling.as_ref()),
                in_force: in_force.as_ref() == Some(&spelling),
                default: true,
            });
        }
        if let Some(spelling) =
            unnamed(&in_force).filter(|spelling| default.as_ref() != Some(spelling))
        {
            values.push(AcceptedValue {
                rows: rows_of(descriptor, settings, spelling.as_ref()),
                in_force: true,
                default: false,
            });
        }
        let accepts = match descriptor.values {
            SettingValues::Fixed(_) => None,
            SettingValues::Open { accepts, .. } => Some(format!("or {accepts}")),
        };
        let sidekicks = match descriptor.sidekick {
            SidekickAccess::ReadAndChange => Cow::Borrowed("read and change"),
            SidekickAccess::ReadOnly { why } => Cow::Owned(format!("read only, since {why}")),
            SidekickAccess::Hidden => Cow::Borrowed("hidden from them"),
        };
        Self {
            descriptor,
            values,
            accepts,
            pinned,
            sidekicks,
        }
    }

    /// The Setting's name, and the key a Config Document pins it by.
    fn draw_heading(&self, drawn: &mut DrawnPane<'_>, width: u16, theme: &Theme) {
        drawn.line(
            self.descriptor.label,
            width,
            theme.text.primary.add_modifier(Modifier::BOLD),
        );
        drawn.line(self.descriptor.key, width, theme.text.subdued);
    }

    /// What the Setting does, every value it accepts, and then whether the
    /// value in force is the reader's pin, where it applies, and what a
    /// Sidekick may do with it.
    fn draw<'a>(&'a self, drawn: &mut DrawnPane<'a>, width: u16, theme: &Theme) {
        drawn.lines.push(Line::default());
        drawn.wrap(self.descriptor.description, "", width, theme.text.primary);
        drawn.lines.push(Line::default());
        self.draw_values(drawn, width, theme);
        drawn.lines.push(Line::default());
        drawn.line(
            if self.pinned {
                "Pinned"
            } else {
                "Built-in default"
            },
            width,
            theme.text.primary,
        );
        let applies = match self.descriptor.scope {
            SettingScope::Client => "in every TUI",
            SettingScope::Server => "on the Server",
        };
        draw_fact(drawn, APPLIES, applies, width, theme);
        draw_fact(drawn, SIDEKICKS, &self.sidekicks, width, theme);
    }

    /// Every value the pane lists, each marked as in force or not, the
    /// built-in default tagged in a column of its own so the tag holds one
    /// place whichever value carries it; then what else an Open Setting takes.
    fn draw_values<'a>(&'a self, drawn: &mut DrawnPane<'a>, width: u16, theme: &Theme) {
        let room = usize::from(width).saturating_sub(IN_FORCE.width() + DEFAULT_TAG.width());
        let tag_column = self
            .values
            .iter()
            .filter_map(|value| match value.rows.as_slice() {
                [row] => Some(row.width() + DEFAULT_TAG_GAP),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .min(room);
        for value in &self.values {
            let (mark, style) = if value.in_force {
                (IN_FORCE, theme.text.primary)
            } else {
                (NOT_IN_FORCE, theme.text.subdued)
            };
            let [row] = value.rows.as_slice() else {
                // A list's entries, one to a row beneath the one mark.
                for (index, entry) in value.rows.iter().enumerate() {
                    let lead = if index == 0 { mark } else { VALUE_INDENT };
                    drawn.wrap_led(lead, entry, VALUE_INDENT, width, style);
                }
                continue;
            };
            let column = tag_column.max(row.width() + DEFAULT_TAG_GAP);
            if value.default && column <= room {
                drawn.lines.push(Line::from(vec![
                    Span::styled(format!("{mark}{row}"), style),
                    Span::styled(
                        format!("{}{DEFAULT_TAG}", " ".repeat(column - row.width())),
                        theme.text.subdued,
                    ),
                ]));
            } else {
                drawn.line(&format!("{mark}{row}"), width, style);
            }
        }
        if let Some(accepts) = &self.accepts {
            drawn.wrap(accepts, VALUE_INDENT, width, theme.text.subdued);
        }
    }
}

/// The rows a value the Setting holds is listed on: a list Setting's entries,
/// one to a row, and any other value, or an empty list, as its row spells it.
fn rows_of(
    descriptor: &SettingDescriptor,
    settings: &EffectiveSettings,
    spelling: &str,
) -> Vec<String> {
    match descriptor.chosen_at() {
        Some(SettingChoiceSurface::TextList(list)) => {
            let entries = list.seed(settings);
            if entries.is_empty() {
                vec![spelling.to_owned()]
            } else {
                entries
            }
        }
        _ => vec![spelling.to_owned()],
    }
}

/// A labelled fact, its value standing in past the label on every row it
/// takes.
fn draw_fact<'a>(
    drawn: &mut DrawnPane<'a>,
    label: &str,
    value: &'a str,
    width: u16,
    theme: &Theme,
) {
    let lead = format!(
        "{label:<label_width$}  ",
        label_width = FACT_INDENT.len() - 2
    );
    if lead.width() + value.width() <= usize::from(width) {
        drawn.line(&format!("{lead}{value}"), width, theme.text.primary);
    } else {
        drawn.wrap_led(&lead, value, FACT_INDENT, width, theme.text.primary);
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::SCHEMA;

    use super::*;

    /// The default a pane tags is read off the schema rather than kept beside
    /// it, so every Setting tags exactly one value as its built-in default,
    /// and a reader who pinned nothing finds that value the one in force.
    #[test]
    fn every_setting_tags_one_built_in_default_which_is_in_force_until_pinned() {
        let defaults = EffectiveSettings::default();
        for descriptor in SCHEMA {
            let account = SettingAccount::new(descriptor, &defaults, false);
            let tagged = account
                .values
                .iter()
                .filter(|value| value.default)
                .collect::<Vec<_>>();
            assert_eq!(
                tagged.len(),
                1,
                "{} tags {} values as its default",
                descriptor.key,
                tagged.len()
            );
            assert!(
                tagged[0].in_force,
                "{} lists its default as not in force under the built-in settings",
                descriptor.key
            );
            assert_eq!(
                account.values.iter().filter(|value| value.in_force).count(),
                1,
                "{} marks other than one value in force",
                descriptor.key
            );
        }
    }
}
