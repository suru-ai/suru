//! The Subagents Section: the tree of Sessions the open Session belongs to,
//! its top-level Session first and every Subagent beneath the Session that
//! spawned it, in spawn order.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::protocol::{ActivityStatus, SessionReference, SessionTimestamp};
use crate::theme::Theme;

use super::super::{
    commands::SemanticCommandId,
    render::{shimmered_label_spans, working_duration},
    slots::truncate_to_width,
    spinner,
    transcript::{humanized_duration, subagent_marker},
};
use super::{
    SubagentTreeReading,
    section::{
        Section, SectionContext, SectionHeader, SectionRow, SectionRowKey, SectionView,
        SubagentTreeView,
    },
};

pub(in crate::tui) struct SubagentsSection;

/// What a settled Subagent's entry says in place of its time while its
/// Session is Monitoring.
const MONITORING: &str = "monitoring";

impl Section for SubagentsSection {
    fn name(&self) -> &'static str {
        "Subagents"
    }

    fn view(&self, context: &SectionContext<'_>) -> Result<SectionView, String> {
        let theme = context.theme;
        let tree = match context.subagent_tree {
            SubagentTreeView::Ready(tree) => tree,
            // A tree not yet in hand is drawn blank, then says Loading in the
            // Working Indicator's shimmer once the quiet period has passed.
            SubagentTreeView::Arriving { loading } => {
                let rows = if loading {
                    vec![unpointable(Line::from(shimmered_label_spans(
                        "Loading",
                        context.shimmer.frame("Loading", context.spinner_frame),
                        theme.text.primary,
                        theme.text.subdued,
                        context.truecolor,
                    )))]
                } else {
                    Vec::new()
                };
                return Ok(self.without_tree(rows, loading));
            }
            // A tree that could not be read says so where it would stand.
            SubagentTreeView::Failed(message) => {
                let rows = wrapped(
                    &format!("Error: Could not load Subagents: {message}"),
                    usize::from(context.width),
                )
                .into_iter()
                .map(|line| unpointable(Line::styled(line, theme.feedback.error)))
                .collect();
                return Ok(self.without_tree(rows, false));
            }
            // A deleted tree has nothing left to show, and nothing went wrong.
            SubagentTreeView::Gone => return Ok(self.without_tree(Vec::new(), false)),
            // The client's own top-level entry, standing until the tree
            // replaces it in place: the open Session alone, Working as far as
            // the client can say, with no time because only the Server knows
            // when Working began.
            SubagentTreeView::StandIn { title, working } => {
                return Ok(self.stand_in(title, working, context));
            }
        };
        let width = usize::from(context.width);
        let mut rows = Vec::new();
        let mut current = None;
        let mut animates = false;
        let top_level = tree.top_level();
        let open_top_level = context.open.session_id == top_level.session_id;
        if open_top_level {
            current = Some(rows.len());
        }
        // The top-level entry wears the Working Marker and its elapsed time
        // only while it is Working or Monitoring, as its Sidebar row tells
        // its duration: Monitoring's counted from when Monitoring began.
        let top_level_since = top_level.working_since.or(top_level.monitoring_since);
        let top_level_working = top_level_since.is_some();
        animates |= top_level_working;
        rows.push(SectionRow {
            lines: vec![title_line(
                TitleParts {
                    guides: String::new(),
                    marker: top_level_working
                        .then(|| (spinner::MARKER.to_owned(), theme.accent.primary)),
                    title: &top_level.title,
                    right: right_slot(
                        top_level.needs_intervention,
                        top_level_since.map(|since| ticking(since, context.now)),
                        theme,
                    ),
                },
                open_top_level,
                width,
                context,
            )],
            invocation: open_invocation(tree, top_level.session_id, context.open),
            key: Some(entry_key(tree, top_level.session_id)),
        });
        if top_level_working && let Some(row) = rows.last_mut() {
            spinner::overlay_frame(&mut row.lines, &[0], context.spinner_frame / 3);
        }
        for entry in tree.depth_first() {
            let subagent = entry.entry;
            let open = context.open.session_id == subagent.session_id;
            if open {
                current = Some(rows.len());
            }
            let (marker, marker_style) = subagent_marker(subagent.status, theme);
            let working = subagent.status == ActivityStatus::Active;
            animates |= working;
            // A working entry counts up from what its settled Turns worked,
            // from the moment the Turn it works in began; a settled one stands
            // at the time all its Turns took, or says nothing where Suru never
            // learned when its work ended.
            let time = if working {
                subagent.working_since.map(|since| {
                    // Counted from as long before this Turn began as its
                    // earlier Turns worked.
                    let earlier = subagent.worked_ms.unwrap_or(0);
                    ticking(
                        SessionTimestamp(since.0.saturating_sub(earlier)),
                        context.now,
                    )
                })
            } else if subagent.monitoring_since.is_some() {
                // A settled Subagent whose Watches outlive it waits on them
                // to wake it, and says so where its time would stand.
                Some(MONITORING.to_owned())
            } else {
                subagent.worked_ms.map(humanized_duration)
            };
            // A Subagent's entry takes two lines: its Marker and Title, then
            // its name and time beneath, so the Title has the width to say
            // what the Subagent was asked and the name still says which kind
            // of agent it was.
            rows.push(SectionRow {
                lines: vec![
                    title_line(
                        TitleParts {
                            guides: entry.guides(),
                            marker: Some((marker.to_owned(), marker_style)),
                            title: &subagent.title,
                            right: None,
                        },
                        open,
                        width,
                        context,
                    ),
                    detail_line(
                        DetailParts {
                            guides: entry.continuation_guides(),
                            name: &subagent.name,
                            model: subagent.model.as_ref().map(|model| model.as_str()),
                            outcome: outcome_word(subagent.status).map(|word| (word, marker_style)),
                            right: right_slot(subagent.needs_intervention, time, theme),
                        },
                        width,
                        context,
                    ),
                ],
                invocation: open_invocation(tree, subagent.session_id, context.open),
                key: Some(entry_key(tree, subagent.session_id)),
            });
            if working && let Some(row) = rows.last_mut() {
                // The Spinner turns at the pace the Transcript row's does.
                spinner::overlay_frame(&mut row.lines, &[0], context.spinner_frame / 3);
            }
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

impl SubagentsSection {
    fn stand_in(&self, title: &str, working: bool, context: &SectionContext<'_>) -> SectionView {
        let mut line = title_line(
            TitleParts {
                guides: String::new(),
                marker: working.then(|| (spinner::MARKER.to_owned(), context.theme.accent.primary)),
                title,
                right: None,
            },
            true,
            usize::from(context.width),
            context,
        );
        if working {
            spinner::overlay_frame(
                std::slice::from_mut(&mut line),
                &[0],
                context.spinner_frame / 3,
            );
        }
        SectionView {
            header: SectionHeader {
                name: self.name(),
                count: Some(0),
            },
            rows: vec![unpointable(line)],
            current: Some(0),
            animates: working,
        }
    }

    /// The Section with no tree to list: its header uncounted, and whatever
    /// stands in the tree's place.
    fn without_tree(&self, rows: Vec<SectionRow>, animates: bool) -> SectionView {
        SectionView {
            header: SectionHeader {
                name: self.name(),
                count: None,
            },
            rows,
            current: None,
            animates,
        }
    }
}

fn unpointable(line: Line<'static>) -> SectionRow {
    SectionRow {
        lines: vec![line],
        invocation: None,
        key: None,
    }
}

/// The key an entry is followed by: the Session it stands for.
fn entry_key(tree: &SubagentTreeReading, session_id: crate::protocol::SessionId) -> SectionRowKey {
    SectionRowKey::Session(SessionReference::new(tree.origin().clone(), session_id))
}

/// `text` broken at spaces into lines no wider than `width`, a word wider
/// than a line being cut.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let needed = if line.is_empty() {
            word.width()
        } else {
            line.width() + 1 + word.width()
        };
        if needed > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while line.width() > width {
            let cut = truncate_to_width(&line, width);
            let kept = cut.trim_end_matches('…').to_owned();
            if kept.is_empty() {
                break;
            }
            line = line[kept.len()..].to_owned();
            lines.push(kept);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
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

struct TitleParts<'a> {
    guides: String,
    marker: Option<(String, Style)>,
    title: &'a str,
    /// What the line's right-aligned slot says, in its style.
    right: Option<(String, Style)>,
}

struct DetailParts<'a> {
    guides: String,
    /// Which kind of agent the Subagent was.
    name: &'a str,
    /// The Model the Provider confirmed for the Subagent, where it has
    /// confirmed one.
    model: Option<&'a str>,
    /// How the Subagent's work ended where its Marker alone would not say —
    /// failed and stopped share a glyph — in the Marker's style.
    outcome: Option<(&'static str, Style)>,
    /// What the line's right-aligned slot says, in its style.
    right: Option<(String, Style)>,
}

/// How long live work has been running, read the way the Sidebar's Working
/// duration reads it, so both columns tick alike.
fn ticking(since: SessionTimestamp, now: SessionTimestamp) -> String {
    working_duration(since, now.0)
}

/// An entry's right slot, the space that holds it off the text before it
/// included: its time, unless its own Session waits on an Intervention,
/// which it then says in the time's place.
fn right_slot(
    needs_intervention: bool,
    time: Option<String>,
    theme: &Theme,
) -> Option<(String, Style)> {
    if needs_intervention {
        Some((" Needs Intervention".to_owned(), theme.feedback.warning))
    } else {
        time.map(|time| (format!(" {time}"), theme.text.subdued))
    }
}

/// The word a settled Subagent's detail line adds to its Marker, where the
/// Marker's glyph alone would not tell its outcome apart.
const fn outcome_word(status: ActivityStatus) -> Option<&'static str> {
    match status {
        ActivityStatus::Active | ActivityStatus::Completed => None,
        ActivityStatus::Failed => Some("Failed"),
        ActivityStatus::Interrupted => Some("Stopped"),
    }
}

/// An entry's first line: tree guides, the Marker, then the Title, with the
/// right slot right-aligned where there is one. The Title gives way to the
/// slot.
fn title_line(
    parts: TitleParts<'_>,
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
    let title_style = if open {
        theme.text.primary.patch(theme.selection.open_title)
    } else {
        theme.text.primary
    };
    let room = line.room_beside(width, &[&parts.right]);
    line.push(truncate_to_width(parts.title, room), title_style);
    line.finish_with(parts.right, width, theme);
    Line::from(line.spans)
}

/// A Subagent entry's second line: the guides carried on beneath its first,
/// the name dimmed and its Model after it where the Provider confirmed one,
/// its outcome where its Marker does not say it, and the right slot
/// right-aligned. Space runs out on the Model first, which is dropped whole
/// rather than cut; then the name gives way to the slot.
fn detail_line(
    parts: DetailParts<'_>,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::default();
    line.push(parts.guides, theme.text.subdued);
    let outcome = parts
        .outcome
        .map(|(word, style)| (format!(" · {word}"), style));
    let room = line.room_beside(width, &[&outcome, &parts.right]);
    // The Model is drawn whole or not at all, so the name keeps what room
    // there is; where the Model fits, the name fits whole beside it.
    let model = parts
        .model
        .map(|model| format!(" · {model}"))
        .filter(|model| parts.name.width() + model.width() <= room);
    line.push(truncate_to_width(parts.name, room), theme.text.subdued);
    if let Some(model) = model {
        line.push(model, theme.text.subdued);
    }
    if let Some((word, style)) = outcome {
        line.push(word, style);
    }
    line.finish_with(parts.right, width, theme);
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

    /// The columns left of `width` for the text that comes next, once the
    /// pieces still to follow it are set aside.
    fn room_beside(&self, width: usize, following: &[&Option<(String, Style)>]) -> usize {
        following
            .iter()
            .filter_map(|piece| piece.as_ref())
            .fold(width.saturating_sub(self.used), |room, (text, _)| {
                room.saturating_sub(text.width())
            })
    }

    /// Ends the line with `right` against its right edge, where there is one.
    fn finish_with(&mut self, right: Option<(String, Style)>, width: usize, theme: &Theme) {
        if let Some((text, style)) = right {
            let gap = width.saturating_sub(self.used).saturating_sub(text.width());
            self.push(" ".repeat(gap), theme.text.subdued);
            self.push(text, style);
        }
    }
}
