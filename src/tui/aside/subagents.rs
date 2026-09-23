//! The Subagents Section: the tree of Sessions the open Session belongs to,
//! its top-level Session first and every Subagent beneath the Session that
//! spawned it, in spawn order.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::protocol::{ActivityStatus, SessionReference};

use super::super::{
    commands::SemanticCommandId, slots::truncate_to_width, spinner, transcript::humanized_duration,
};
use super::{
    SubagentTreeReading,
    section::{Section, SectionContext, SectionHeader, SectionRow, SectionView},
};

pub(in crate::tui) struct SubagentsSection;

impl Section for SubagentsSection {
    fn name(&self) -> &'static str {
        "Subagents"
    }

    fn view(&self, context: &SectionContext<'_>) -> Result<SectionView, String> {
        let Some(tree) = context.subagent_tree else {
            // A tree not yet in hand is drawn blank.
            return Ok(SectionView {
                header: SectionHeader {
                    name: self.name(),
                    count: None,
                },
                rows: Vec::new(),
                current: None,
                animates: false,
            });
        };
        let theme = context.theme;
        let width = usize::from(context.width);
        let mut rows = Vec::new();
        let mut current = None;
        let mut animates = false;
        let top_level = tree.top_level();
        let open_top_level = context.open.session_id == top_level.session_id;
        if open_top_level {
            current = Some(rows.len());
        }
        rows.push(SectionRow {
            line: entry_line(
                EntryParts {
                    guides: String::new(),
                    marker: None,
                    name: None,
                    title: &top_level.title,
                    duration: None,
                },
                open_top_level,
                width,
                context,
            ),
            invocation: open_invocation(tree, top_level.session_id, context.open),
        });
        for entry in tree.depth_first() {
            let subagent = entry.entry;
            let open = context.open.session_id == subagent.session_id;
            if open {
                current = Some(rows.len());
            }
            let marker = match subagent.status {
                ActivityStatus::Active => {
                    animates = true;
                    (
                        format!("{} ", spinner::frame(context.spinner_frame / 3)),
                        theme.accent.primary,
                    )
                }
                ActivityStatus::Completed => ("✓ ".to_owned(), theme.text.subdued),
                ActivityStatus::Failed => ("× ".to_owned(), theme.feedback.error),
                ActivityStatus::Interrupted => ("× ".to_owned(), theme.feedback.warning),
            };
            rows.push(SectionRow {
                line: entry_line(
                    EntryParts {
                        guides: entry.guides(),
                        marker: Some(marker),
                        name: Some(&subagent.name),
                        title: &subagent.title,
                        duration: subagent.duration_ms.map(humanized_duration),
                    },
                    open,
                    width,
                    context,
                ),
                invocation: open_invocation(tree, subagent.session_id, context.open),
            });
        }
        Ok(SectionView {
            header: SectionHeader {
                name: self.name(),
                count: Some(tree.subagent_count()),
            },
            rows,
            current,
            animates,
        })
    }
}

/// What choosing an entry does: opening its Session through the same route a
/// Subagent's Transcript row and the Subagent Picker take, or nothing for the
/// Session already open.
fn open_invocation(
    tree: &SubagentTreeReading,
    session_id: crate::protocol::SessionId,
    open: &SessionReference,
) -> Option<super::super::commands::SemanticInvocation> {
    let reference = SessionReference::new(tree.origin().clone(), session_id);
    (reference != *open).then(|| SemanticCommandId::SubagentOpen.on_session(reference))
}

struct EntryParts<'a> {
    guides: String,
    marker: Option<(String, Style)>,
    name: Option<&'a str>,
    title: &'a str,
    duration: Option<String>,
}

/// One entry's line: tree guides, the Marker, the name dimmed, then the
/// Title, with the time right-aligned. Space runs out on the Title first, so
/// the name — which kind of agent it was — stays readable longest.
fn entry_line(
    parts: EntryParts<'_>,
    open: bool,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::default();
    line.push(parts.guides, theme.text.subdued);
    if let Some((marker, style)) = parts.marker {
        line.push(marker, style);
    }
    let right = parts
        .duration
        .map(|duration| format!(" {duration}"))
        .unwrap_or_default();
    let mut room = width
        .saturating_sub(line.used)
        .saturating_sub(right.width());
    if let Some(name) = parts.name {
        let name = truncate_to_width(name, room);
        room = room.saturating_sub(name.width());
        line.push(name, theme.text.subdued);
        if room > 1 && !parts.title.is_empty() {
            line.push(" ".to_owned(), theme.text.subdued);
            room -= 1;
        } else {
            room = 0;
        }
    }
    let title_style = if open {
        theme.text.primary.patch(theme.selection.open_title)
    } else {
        theme.text.primary
    };
    line.push(truncate_to_width(parts.title, room), title_style);
    if !right.is_empty() {
        let gap = width
            .saturating_sub(line.used)
            .saturating_sub(right.width());
        line.push(" ".repeat(gap), theme.text.subdued);
        line.push(right, theme.text.subdued);
    }
    Line::from(line.spans)
}

/// A line being built from the left, counting the columns it has taken.
#[derive(Default)]
struct Pieces {
    spans: Vec<Span<'static>>,
    used: usize,
}

impl Pieces {
    fn push(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        self.used += text.width();
        self.spans.push(Span::styled(text, style));
    }
}
