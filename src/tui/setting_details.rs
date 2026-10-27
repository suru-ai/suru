//! What the settings panel tells about the row focus stands on, beyond the row
//! that lists it: whatever asks something of the reader first — an edit that
//! never reached the Config Document, a Provider's condition and what fixes
//! it, a pin Suru ignored — then what the Setting does, every value it accepts
//! with the one in force and the built-in default marked, and where it applies
//! and what a Sidekick may do with it. A Provider's row stands for more than
//! its Enablement, so on the Providers tab the pane accounts for the Provider
//! instead: whether it is on, its own Settings at a glance, and the one among
//! them focus picks out, in the same detail. A wide panel draws all of it in a
//! pane beside its rows; a narrow one keeps a headline above them instead.

use std::{borrow::Cow, ops::Range};

use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    protocol::{EffectiveSettings, SettingScope, SettingsDiagnostic, WorkspacePaths},
    settings::{SettingChoiceSurface, SettingDescriptor, SettingValues, SidekickAccess},
    theme::Theme,
};

use super::{
    drawn_pane::DrawnPane,
    settings_panel::{PanelFocus, ProviderFocus, RowList, SettingRow},
    slots::truncate_to_width,
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

/// The labels of the facts the pane ends on. Each is ASCII, so it takes as
/// many columns as it has bytes.
const APPLIES: &str = "Applies";
const SIDEKICKS: &str = "Sidekicks";

/// The columns standing a fact's value off the widest label.
const FACT_GAP: usize = 2;

/// How far each fact's value stands in past the labels, wrapped rows and all:
/// the widest label, and the gap after it.
const FACT_INDENT: &str = blanks(widest(&[APPLIES, SIDEKICKS]) + FACT_GAP);

/// The widest of `labels`, each of them ASCII.
const fn widest(labels: &[&str]) -> usize {
    let mut widest = 0;
    let mut index = 0;
    while index < labels.len() {
        if labels[index].len() > widest {
            widest = labels[index].len();
        }
        index += 1;
    }
    widest
}

/// `width` blank columns.
const fn blanks(width: usize) -> &'static str {
    const BLANKS: &str = "                                ";
    BLANKS.split_at(width).0
}

/// The pane a wide panel details its focused row in. It holds each line it
/// wraps as the pane writes it, so a selection across a wrapped description or
/// fix reads it back as the one line it was written as (ADR-0020).
///
/// The pane is an account of what the row stands for, headed by its name,
/// with whatever asks something of the reader standing between that heading
/// and the rest. The leads are the panel's to say of whichever row focus
/// stands on; the account is the Setting's, or the Provider's where the row
/// stands for one.
pub(super) struct DetailsPane<'a> {
    /// Why the reader's last edit never reached the Config Document, which
    /// leads the pane and holds beneath its heading however far the rest
    /// scrolls: a wide panel has no headline to say it in instead.
    refusal: Option<&'a str>,
    leads: Vec<Lead<'a>>,
    account: Account,
}

/// What the pane accounts for beneath its leads.
enum Account {
    Setting(SettingAccount),
    Provider(ProviderAccount),
}

/// What the pane says of a Provider: its name and whether it is on, its own
/// Settings listed as the panel lists rows, and the one among them focus
/// picks out, detailed as any Setting is.
struct ProviderAccount {
    name: &'static str,
    enablement: &'static SettingDescriptor,
    enabled: bool,
    /// What a disabled Provider's pane says of it before its Settings.
    disabled: Option<String>,
    settings: Vec<SettingRow>,
    /// Whether the keys have stepped in onto those Settings, which is what
    /// the pane draws as focus.
    holds_keys: bool,
    picked: Option<SettingAccount>,
}

/// What a pane drew, and where among its rows it listed a Provider's own
/// Settings, which a pointer resolves against.
pub(super) struct DrawnDetails<'a> {
    pub(super) pane: DrawnPane<'a>,
    pub(super) provider_settings: Option<ShownSettings>,
}

/// Where a pane shows a Provider's own Settings: the row of the pane the first
/// shown stands on, which of the Settings that is, and how many show.
pub(super) struct ShownSettings {
    pub(super) row: usize,
    pub(super) first: usize,
    pub(super) count: usize,
}

/// Everything a pane has to say at one spacing, however many rows it takes.
struct LaidOut<'p> {
    drawn: DrawnPane<'p>,
    /// The rows that hold still while the rest scrolls beneath them: the
    /// heading the account drew, and the refusal of the last edit beneath it.
    held: usize,
    /// Where it listed a Provider's own Settings.
    listed: Option<Range<usize>>,
    /// The rows the reader must see: from the Setting the keys are on to the
    /// foot, while they are on one, and none otherwise.
    keep: Range<usize>,
}

/// Something the pane leads with because it asks something of the reader
/// before anything about the Setting does.
struct Lead<'a> {
    text: Cow<'a, str>,
    tone: Tone,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Tone {
    /// Something went wrong: Suru's read of a Provider.
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
    /// The pane for where `focus` stands. Leading it is everything the narrow
    /// panel's headline and fix choose between, whole: the last edit's
    /// failure, then the focused Provider's condition and what fixes it, then
    /// whatever the Config Documents' diagnostics say of a pin among the
    /// Settings focus speaks for, each naming its Config Document as `paths`
    /// — the reader's own Server's — spell it: from the reader's home, so a
    /// path says where without a run of rows to say it in.
    pub(super) fn of(
        focus: PanelFocus<'a>,
        settings: &EffectiveSettings,
        diagnostics: &'a [SettingsDiagnostic],
        paths: &WorkspacePaths,
    ) -> Self {
        let PanelFocus {
            error,
            condition,
            setting,
            pinned,
            provider,
            speaks_for,
        } = focus;
        let mut leads = Vec::new();
        if let Some(condition) = condition {
            leads.push(Lead {
                text: Cow::Borrowed(condition.message),
                tone: if condition.failed {
                    Tone::Error
                } else {
                    Tone::Warning
                },
            });
            leads.extend(condition.fix.map(|fix| Lead {
                text: Cow::Borrowed(fix),
                tone: Tone::Fix,
            }));
        }
        leads.extend(
            diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic
                        .key
                        .as_deref()
                        .is_some_and(|key| speaks_for.contains(&key))
                })
                .map(|diagnostic| Lead {
                    text: Cow::Owned(format!(
                        "{}: {}",
                        paths.label(&diagnostic.file),
                        diagnostic.message
                    )),
                    tone: Tone::Warning,
                }),
        );
        let account = match provider {
            Some(provider) => Account::Provider(ProviderAccount::new(provider, settings)),
            None => Account::Setting(SettingAccount::new(setting, settings, pinned)),
        };
        Self {
            refusal: error,
            leads,
            account,
        }
    }

    /// The pane in `height` rows `width` columns wide. A pane with more to
    /// say than it has rows for ends on an ellipsis rather than silently —
    /// save one whose keys are on a Provider's own Settings, which keeps its
    /// heading and scrolls the rest beneath it until the Setting the keys are
    /// on shows with all of its detail, because that Setting is what an edit
    /// would change.
    pub(super) fn draw(&self, width: u16, height: usize, theme: &Theme) -> DrawnDetails<'_> {
        // A pane too tall while the keys are on a Provider's own Settings
        // first gives up the blank rows standing the picked Setting's detail
        // apart, and scrolls only where that is not enough.
        let spaced = self.lay_out(width, Spacing::Spaced, theme);
        let LaidOut {
            mut drawn,
            held,
            listed,
            keep,
        } = if !spaced.keep.is_empty() && spaced.drawn.lines.len() > height {
            self.lay_out(width, Spacing::Packed, theme)
        } else {
            spaced
        };
        let window = drawn.fit_showing(height, held, keep, theme);
        // A Setting the pane had no room for, or scrolled past, is no row a
        // pointer can land on, and nor is an ellipsis standing for it.
        let provider_settings = listed.and_then(|listed| {
            let mut shown = listed
                .clone()
                .filter_map(|row| Some((row, window.row(row)?)));
            let (first, row) = shown.next()?;
            Some(ShownSettings {
                row,
                first: first - listed.start,
                count: 1 + shown.count(),
            })
        });
        DrawnDetails {
            pane: drawn,
            provider_settings,
        }
    }

    /// Everything the pane has to say, `width` columns wide however many rows
    /// it takes.
    fn lay_out(&self, width: u16, spacing: Spacing, theme: &Theme) -> LaidOut<'_> {
        let mut drawn = DrawnPane::default();
        match &self.account {
            Account::Setting(account) => account.draw_heading(&mut drawn, width, theme),
            Account::Provider(account) => account.draw_heading(&mut drawn, width, theme),
        }
        if let Some(refusal) = self.refusal {
            drawn.blank();
            drawn.wrap(refusal, "", width, theme.feedback.error);
        }
        // Whatever the heading came to, and the refusal beneath it.
        let held = drawn.lines.len();
        for lead in &self.leads {
            if lead.tone != Tone::Fix {
                drawn.blank();
            }
            let style = match lead.tone {
                Tone::Error => theme.feedback.error,
                Tone::Warning => theme.feedback.warning,
                Tone::Fix => theme.text.primary,
            };
            drawn.wrap(&lead.text, "", width, style);
        }
        let (listed, keep) = match &self.account {
            Account::Setting(account) => {
                account.draw(&mut drawn, width, theme);
                (None, 0..0)
            }
            Account::Provider(account) => {
                let listed = account.draw(&mut drawn, width, spacing, theme);
                let picked = account
                    .settings
                    .iter()
                    .position(|row| row.selected)
                    .map_or(listed.start, |index| listed.start + index);
                let keep = if account.holds_keys {
                    picked..drawn.lines.len()
                } else {
                    0..0
                };
                (Some(listed), keep)
            }
        };
        LaidOut {
            drawn,
            held,
            listed,
            keep,
        }
    }
}

/// Whether the detail of a Provider's picked Setting stands its parts apart
/// with blank rows, or gives them up for a pane too short for them.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Spacing {
    Spaced,
    Packed,
}

impl ProviderAccount {
    fn new(provider: ProviderFocus, settings: &EffectiveSettings) -> Self {
        let ProviderFocus {
            row,
            enablement,
            enabled,
            settings: rows,
            picked,
            holds_keys,
        } = provider;
        let picked_pinned = rows.iter().any(|row| row.selected && row.pinned);
        Self {
            name: row.label,
            enablement,
            enabled,
            disabled: (!enabled).then(|| {
                format!(
                    "Suru leaves {} entirely alone until you enable it.",
                    row.label
                )
            }),
            settings: rows,
            holds_keys,
            picked: picked
                .map(|descriptor| SettingAccount::new(descriptor, settings, picked_pinned)),
        }
    }

    /// The Provider's name, with whether it is on standing at the far end of
    /// its line, and the key a Config Document turns it on or off by. The name
    /// takes the accent while the keys are on the pane, which is how the pane
    /// reads as the side that has them.
    fn draw_heading(&self, drawn: &mut DrawnPane<'_>, width: u16, theme: &Theme) {
        let state = if self.enabled { "enabled" } else { "disabled" };
        let name = truncate_to_width(
            self.name,
            usize::from(width).saturating_sub(state.width() + 1),
        );
        let gap = usize::from(width).saturating_sub(name.width() + state.width());
        let name_style = if self.holds_keys {
            theme.accent.primary
        } else {
            theme.text.primary
        };
        drawn.lines.push(Line::from(vec![
            Span::styled(name, name_style.add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(gap)),
            Span::styled(state, theme.text.subdued),
        ]));
        drawn.line(self.enablement.key, width, theme.text.subdued);
    }

    /// What a disabled Provider is left as, then its own Settings, each on a
    /// row ending on its value and a mark for a pin, then the one focus picks
    /// out among them in detail. Answers with the rows the Settings took.
    fn draw<'a>(
        &'a self,
        drawn: &mut DrawnPane<'a>,
        width: u16,
        spacing: Spacing,
        theme: &Theme,
    ) -> Range<usize> {
        let space = |drawn: &mut DrawnPane<'a>| {
            if spacing == Spacing::Spaced {
                drawn.blank();
            }
        };
        if let Some(disabled) = &self.disabled {
            drawn.blank();
            drawn.wrap(disabled, "", width, theme.text.subdued);
        }
        drawn.blank();
        let first = drawn.lines.len();
        drawn.lines.extend(
            RowList::new(&self.settings, self.holds_keys)
                .draw(None, usize::from(width), self.settings.len(), theme)
                .lines,
        );
        let listed = first..drawn.lines.len();
        // Whether the picked Setting's value is a pin is already its row's
        // mark, so its detail goes straight from its values to where it
        // applies.
        if let Some(picked) = &self.picked {
            space(drawn);
            drawn.line(picked.descriptor.key, width, theme.text.subdued);
            drawn.wrap(picked.descriptor.description, "", width, theme.text.primary);
            space(drawn);
            picked.draw_values(drawn, width, theme);
            space(drawn);
            picked.draw_reach(drawn, width, theme);
        }
        listed
    }
}

impl SettingAccount {
    fn new(
        descriptor: &'static SettingDescriptor,
        settings: &EffectiveSettings,
        pinned: bool,
    ) -> Self {
        let defaults = EffectiveSettings::default();
        let in_force = descriptor.spelling(settings);
        let default = descriptor.spelling(&defaults);
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
                rows: rows_of(descriptor, &defaults, spelling.as_ref()),
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
        drawn.blank();
        drawn.wrap(self.descriptor.description, "", width, theme.text.primary);
        drawn.blank();
        self.draw_values(drawn, width, theme);
        drawn.blank();
        drawn.line(
            if self.pinned {
                "Pinned"
            } else {
                "Built-in default"
            },
            width,
            theme.text.primary,
        );
        self.draw_reach(drawn, width, theme);
    }

    /// Where the Setting applies, and what a Sidekick may do with it.
    fn draw_reach<'a>(&'a self, drawn: &mut DrawnPane<'a>, width: u16, theme: &Theme) {
        let applies = match self.descriptor.scope {
            SettingScope::Client => "in every Client",
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
            // A value, or each of a list's entries, copies with the mark it
            // leads with, whether it fits its row or wraps over several.
            let [row] = value.rows.as_slice() else {
                // A list's entries, one to a row beneath the one mark.
                for (index, entry) in value.rows.iter().enumerate() {
                    let lead = if index == 0 { mark } else { VALUE_INDENT };
                    drawn.wrap_labelled(lead, entry, VALUE_INDENT, width, style);
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
                drawn.wrap_labelled(mark, row, VALUE_INDENT, width, style);
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
/// takes, and copying with its label however many that is.
fn draw_fact(drawn: &mut DrawnPane<'_>, label: &str, value: &str, width: u16, theme: &Theme) {
    let label = format!("{label:<0$}", FACT_INDENT.len());
    drawn.wrap_labelled(&label, value, FACT_INDENT, width, theme.text.primary);
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
