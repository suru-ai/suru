//! The Subagents Section: the tree of Sessions the open Session belongs to,
//! its top-level Session first and every Subagent beneath the Session that
//! spawned it — working branches ahead of settled ones, newest spawn first —
//! headed by how many Subagents there are and how many of them work.
//!
//! Where a Sidekick's Session heads the tree the Section answers for
//! everything that Sidekick has a hand in, and is headed **Sessions**: beneath
//! the Sidekick's own Subagents stand its Subsessions and the Sessions it
//! acted on, alike, each in three lines with its own Subagents beneath it. A
//! Session on a Remote names that Remote at the head of its second line, and
//! choosing it opens it there, turning the Outlook toward that Remote.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::protocol::{
    ActivityStatus, Outlook, SessionReference, SessionTimestamp, SubagentTreeEntry,
    SubagentTreeSession,
};
use crate::theme::Theme;

use super::super::{
    commands::{SemanticCommandId, SemanticInvocation},
    render::{shimmered_label_spans, working_duration},
    sidebar::workspace_name,
    slots::truncate_to_width,
    spinner,
    transcript::{humanized_duration, subagent_marker},
};
use super::{
    SubagentTreeReading, TreeEntry, TreeNode,
    section::{
        Section, SectionContext, SectionHeader, SectionRow, SectionRowKey, SectionView,
        SubagentTreeView,
    },
};

pub(in crate::tui) struct SubagentsSection;

/// What a settled Subagent's entry says in place of its time while its
/// Session is Monitoring.
const MONITORING: &str = "monitoring";

/// What the entry of a Session whose Remote does not answer says in place of
/// its Model and time, which it has nothing current to say of — and in place
/// of its Title, where the Remote never said one.
const NOT_ANSWERING: &str = "not answering";

/// What stands between a Remote's name and the Workspace it leads.
const REMOTE_SEPARATOR: &str = " · ";

/// What the Section is headed where a Sidekick's Session heads the tree.
const SESSIONS: &str = "Sessions";

/// The glyph a Workspace without an Icon is named beside: the plain folder
/// the Sidebar and the Workspace Picker draw. It mirrors
/// `crate::tui::render::NF_COD_FOLDER` by hand, as the Sidebar's does.
const FOLDER_GLYPH: char = '\u{ea83}';

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
            invocation: open_invocation(
                SemanticCommandId::SubagentOpen,
                tree,
                top_level.session_id,
                context.open,
            ),
            key: Some(entry_key(tree, top_level.session_id)),
        });
        if top_level_working && let Some(row) = rows.last_mut() {
            spinner::overlay_frame(&mut row.lines, &[0], context.spinner_frame / 3);
        }
        for entry in tree.depth_first() {
            if context.open.session_id == entry.node.session_id() {
                current = Some(rows.len());
            }
            let (row, working) = match entry.node {
                TreeNode::Subagent(subagent) => subagent_row(tree, &entry, subagent, context),
                TreeNode::Session(session) => session_row(tree, &entry, session, context),
            };
            animates |= working;
            rows.push(row);
            if working && let Some(row) = rows.last_mut() {
                // The Spinner turns at the pace the Transcript row's does.
                spinner::overlay_frame(&mut row.lines, &[0], context.spinner_frame / 3);
            }
        }
        Ok(SectionView {
            header: SectionHeader {
                name: if tree.is_sidekicks() {
                    SESSIONS
                } else {
                    self.name()
                },
                count: Some(tree.entry_count()),
                // Nothing working goes unsaid rather than counted as none.
                working: Some(tree.working_count()).filter(|working| *working > 0),
            },
            rows,
            current,
            animates,
        })
    }
}

/// A Subagent's entry, and whether it works. It takes two lines: its Marker
/// and Title, then its name and time beneath, so the Title has the width to
/// say what the Subagent was asked and the name still says which kind of
/// agent it was.
fn subagent_row(
    tree: &SubagentTreeReading,
    entry: &TreeEntry<'_>,
    subagent: &SubagentTreeEntry,
    context: &SectionContext<'_>,
) -> (SectionRow, bool) {
    let theme = context.theme;
    let width = usize::from(context.width);
    let open = context.open.session_id == subagent.session_id;
    let (marker, marker_style) = subagent_marker(subagent.status, theme);
    let working = subagent.status == ActivityStatus::Active;
    let time = work_time(
        working,
        subagent.worked_ms,
        subagent.working_since,
        subagent.monitoring_since.is_some(),
        context.now,
    );
    let row = SectionRow {
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
        invocation: open_invocation(
            SemanticCommandId::SubagentOpen,
            tree,
            subagent.session_id,
            context.open,
        ),
        key: Some(entry_key(tree, subagent.session_id)),
    };
    (row, working)
}

/// The entry of a Session a Sidekick has a hand in, and whether it works,
/// drawn alike whether the Sidekick began it or only acted on it. It takes
/// three lines: its Marker and Title; where it works, its Workspace beside
/// its Icon; and the Model its Agent Selection names, with its outcome where
/// the Marker would not tell, and the time a Subagent's entry carries.
fn session_row(
    tree: &SubagentTreeReading,
    entry: &TreeEntry<'_>,
    session: &SubagentTreeSession,
    context: &SectionContext<'_>,
) -> (SectionRow, bool) {
    let theme = context.theme;
    let width = usize::from(context.width);
    let reference = session_reference(tree, session);
    let open = reference.as_ref() == Some(context.open);
    let marker = session.status.map(|status| subagent_marker(status, theme));
    let working = session.status == Some(ActivityStatus::Active);
    let time = work_time(
        working,
        session.worked_ms,
        session.working_since,
        session.monitoring_since.is_some(),
        context.now,
    );
    // A Remote's Session is named in its Remote's own paths, where the
    // client has heard how it spells them.
    let paths = match &session.origin {
        None => context.workspace_paths,
        Some(remote) => context
            .remote_workspace_paths
            .get(&Outlook::Remote(remote.clone())),
    };
    // One whose Remote does not answer keeps the Workspace it was last
    // known by, where it was known by one.
    let workspace = if session.workspace_path.as_os_str().is_empty() {
        String::new()
    } else {
        paths.map_or_else(
            || any_workspace_name(&session.workspace_path),
            |paths| paths.name(&session.workspace_path),
        )
    };
    let icon = (context.show_icons && !workspace.is_empty()).then(|| {
        session
            .workspace_icon
            .as_deref()
            .and_then(crate::icon_catalog::glyph)
            .unwrap_or(FOLDER_GLYPH)
    });
    let row = SectionRow {
        lines: vec![
            if session.unanswered {
                unanswered_title_line(entry.guides(), &session.title, open, width, context)
            } else {
                title_line(
                    TitleParts {
                        guides: entry.guides(),
                        marker: marker.map(|(marker, style)| (marker.to_owned(), style)),
                        title: &session.title,
                        right: None,
                    },
                    open,
                    width,
                    context,
                )
            },
            location_line(
                LocationParts {
                    guides: entry.continuation_guides(),
                    remote: session.origin.as_deref(),
                    icon,
                    workspace: &workspace,
                },
                width,
                context,
            ),
            if session.unanswered {
                not_answering_line(entry.continuation_guides(), width, context)
            } else {
                selection_line(
                    SelectionParts {
                        guides: entry.continuation_guides(),
                        model: session.model.as_ref().map(|model| model.as_str()),
                        outcome: session.status.and_then(|status| {
                            Some((outcome_word(status)?, subagent_marker(status, theme).1))
                        }),
                        right: right_slot(session.needs_intervention, time, theme),
                    },
                    width,
                    context,
                )
            },
        ],
        invocation: reference
            .as_ref()
            .filter(|reference| *reference != context.open)
            .map(|reference| SemanticCommandId::SessionOpen.on_session(reference.clone())),
        key: Some(SectionRowKey::Session(reference.unwrap_or_else(|| {
            SessionReference::new(tree.origin().clone(), session.session_id)
        }))),
    };
    (row, working)
}

/// The Session an entry beneath a Sidekick stands for, as this client
/// reaches it: on the tree's own Server, or on the Remote it names, which
/// opening it turns the Outlook toward. A Session on a Remote of a Remote is
/// nothing this client can reach, since a Pairing is one-way and goes no
/// further.
fn session_reference(
    tree: &SubagentTreeReading,
    session: &SubagentTreeSession,
) -> Option<SessionReference> {
    match (&session.origin, tree.origin()) {
        (None, origin) => Some(SessionReference::new(origin.clone(), session.session_id)),
        (Some(remote), Outlook::Local) => Some(SessionReference::new(
            Outlook::Remote(remote.clone()),
            session.session_id,
        )),
        (Some(_), Outlook::Remote(_)) => None,
    }
}

/// The name of the Workspace at `path` where the client has not heard how
/// its Server spells paths: the last part of it, on either separator, since a
/// Remote's paths may be another platform's.
fn any_workspace_name(path: &std::path::Path) -> String {
    let spelled = path.to_string_lossy();
    let trimmed = spelled.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => workspace_name(path),
    }
}

/// The first line of the entry of a Session whose Remote does not answer:
/// the guides and then, dimmed and with no Marker — nothing of its work being
/// current — the Title the Remote last gave it, or that it is not answering
/// where it never gave one.
fn unanswered_title_line(
    guides: String,
    title: &str,
    open: bool,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::beside(width, None);
    line.push_within(&guides, theme.text.subdued);
    let style = if open {
        theme.text.subdued.patch(theme.selection.open_title)
    } else {
        theme.text.subdued
    };
    let title = if title.trim().is_empty() {
        NOT_ANSWERING
    } else {
        title
    };
    let room = line.room_beside(&[]);
    line.push(truncate_to_width(title, room), style);
    Line::from(line.spans)
}

/// The third line of the entry of a Session whose Remote does not answer:
/// the guides carried on beneath its first, then, dimmed, that it is not
/// answering, in place of its Model and time.
fn not_answering_line(guides: String, width: usize, context: &SectionContext<'_>) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::beside(width, None);
    line.push_within(&guides, theme.text.subdued);
    let room = line.room_beside(&[]);
    line.push(truncate_to_width(NOT_ANSWERING, room), theme.text.subdued);
    Line::from(line.spans)
}

/// What an entry's time slot says of its work: counting up while it works,
/// from what its settled Turns worked and the moment the work it does now
/// began; **monitoring** for a settled entry whose Watches outlive it, since
/// they may wake it yet; and otherwise the time all its Turns took, or
/// nothing where Suru never learned when its work ended.
fn work_time(
    working: bool,
    worked_ms: Option<u64>,
    working_since: Option<SessionTimestamp>,
    monitoring: bool,
    now: SessionTimestamp,
) -> Option<String> {
    if working {
        working_since.map(|since| {
            // Counted from as long before this work began as the earlier
            // work took.
            let earlier = worked_ms.unwrap_or(0);
            ticking(SessionTimestamp(since.0.saturating_sub(earlier)), now)
        })
    } else if monitoring {
        Some(MONITORING.to_owned())
    } else {
        worked_ms.map(humanized_duration)
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
                working: None,
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
                working: None,
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

/// What choosing an entry does: `opening` its Session, or nothing for the
/// Session already open. A Subagent's entry opens through the same route its
/// Transcript row and the Subagent Picker take; the entry of a Session a
/// Sidekick has a hand in opens it as the top-level Session it is.
fn open_invocation(
    opening: SemanticCommandId,
    tree: &SubagentTreeReading,
    session_id: crate::protocol::SessionId,
    open: &SessionReference,
) -> Option<SemanticInvocation> {
    let reference = SessionReference::new(tree.origin().clone(), session_id);
    (reference != *open).then(|| opening.on_session(reference))
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

struct LocationParts<'a> {
    guides: String,
    /// The Remote the Session lives on, where it lives on one.
    remote: Option<&'a str>,
    /// The glyph the Workspace is named beside, where Icons are drawn.
    icon: Option<char>,
    /// The Workspace's name.
    workspace: &'a str,
}

struct SelectionParts<'a> {
    guides: String,
    /// The Model the Session's Agent Selection names, where it names one.
    model: Option<&'a str>,
    /// How the Session's work ended where its Marker alone would not say,
    /// in the Marker's style.
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
    let mut line = Pieces::beside(width, parts.right.as_ref());
    line.push_within(&parts.guides, theme.text.subdued);
    if let Some((marker, style)) = parts.marker {
        line.push_within(&marker, style);
    }
    let title_style = if open {
        theme.text.primary.patch(theme.selection.open_title)
    } else {
        theme.text.primary
    };
    let room = line.room_beside(&[]);
    line.push(truncate_to_width(parts.title, room), title_style);
    line.finish_with(parts.right, width, theme);
    Line::from(line.spans)
}

/// A Subagent entry's second line: the guides carried on beneath its first,
/// the name dimmed and its Model after it where the Provider confirmed one,
/// its outcome where its Marker does not say it, and the right slot
/// right-aligned. Where the name and Model do not fit together, the name is
/// left out; then the Model, or a name with no Model, gives way to the slot,
/// and after it the outcome and the guides — never the slot.
fn detail_line(
    parts: DetailParts<'_>,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::beside(width, parts.right.as_ref());
    line.push_within(&parts.guides, theme.text.subdued);
    let outcome = parts
        .outcome
        .map(|(word, style)| (format!(" · {word}"), style))
        .filter(|(word, _)| line.fits(word));
    let room = line.room_beside(&[&outcome]);
    // The Model tells apart Subagents the name alone would not, so where the
    // two do not fit together the name is left out and the Model keeps the
    // room, cut only where it alone is wider than the line.
    match parts.model {
        Some(model) if parts.name.width() + " · ".width() + model.width() <= room => {
            line.push(parts.name.to_owned(), theme.text.subdued);
            line.push(format!(" · {model}"), theme.text.subdued);
        }
        Some(model) => line.push(truncate_to_width(model, room), theme.text.subdued),
        None => line.push(truncate_to_width(parts.name, room), theme.text.subdued),
    }
    if let Some((word, style)) = outcome {
        line.push(word, style);
    }
    line.finish_with(parts.right, width, theme);
    Line::from(line.spans)
}

/// The second line of a Session's entry beneath a Sidekick: the guides
/// carried on beneath its first, then, dimmed, the Remote the Session lives
/// on where it lives on one, and the Workspace it works in beside its Icon.
/// Where the line runs short the Remote's name gives way first — cut short
/// with an ellipsis, then left out with what parts it from the Workspace —
/// and only then is the Workspace's name cut short, its Icon kept.
fn location_line(
    parts: LocationParts<'_>,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::beside(width, None);
    line.push_within(&parts.guides, theme.text.subdued);
    let icon = parts.icon.map(|icon| format!("{icon} "));
    let workspace_width = icon.as_ref().map_or(0, |icon| icon.width()) + parts.workspace.width();
    match parts.remote {
        // A Session with nothing current to say of its Workspace is named by
        // its Remote alone.
        Some(remote) if workspace_width == 0 => {
            let room = line.room_beside(&[]);
            line.push(truncate_to_width(remote, room), theme.text.subdued);
        }
        // The Remote's name keeps at least a character and the ellipsis
        // where it is cut, or gives way altogether.
        Some(remote) => {
            let room = line
                .room_beside(&[])
                .saturating_sub(workspace_width + REMOTE_SEPARATOR.width());
            if room >= remote.width().min(2) {
                line.push(truncate_to_width(remote, room), theme.text.subdued);
                line.push(REMOTE_SEPARATOR.to_owned(), theme.text.subdued);
            }
        }
        None => {}
    }
    if let Some(icon) = icon {
        line.push_within(&icon, theme.text.subdued);
    }
    let room = line.room_beside(&[]);
    line.push(truncate_to_width(parts.workspace, room), theme.text.subdued);
    Line::from(line.spans)
}

/// The third line of a Session's entry beneath a Sidekick: the guides
/// carried on beneath its first, the Model its Agent Selection names,
/// dimmed, its outcome where its Marker does not say it, and the right slot
/// right-aligned. Where the line runs short the Model gives way first, cut
/// short with an ellipsis, then the outcome, then the guides — never the
/// slot.
fn selection_line(
    parts: SelectionParts<'_>,
    width: usize,
    context: &SectionContext<'_>,
) -> Line<'static> {
    let theme = context.theme;
    let mut line = Pieces::beside(width, parts.right.as_ref());
    line.push_within(&parts.guides, theme.text.subdued);
    let outcome = parts
        .outcome
        .map(|(word, style)| {
            let word = match parts.model {
                Some(_) => format!(" · {word}"),
                None => word.to_owned(),
            };
            (word, style)
        })
        .filter(|(word, _)| line.fits(word));
    let room = line.room_beside(&[&outcome]);
    if let Some(model) = parts.model {
        line.push(truncate_to_width(model, room), theme.text.subdued);
    }
    if let Some((word, style)) = outcome {
        line.push(word, style);
    }
    line.finish_with(parts.right, width, theme);
    Line::from(line.spans)
}

/// A line being built from the left, counting the columns it has taken,
/// whose right-aligned slot has its columns set aside before anything else
/// is drawn: whatever precedes it gives way, and never the slot.
struct Pieces {
    spans: Vec<Span<'static>>,
    used: usize,
    /// The columns the line may take before its slot.
    budget: usize,
}

impl Pieces {
    /// A line `width` columns wide, setting aside the columns `right` takes
    /// where it has a slot.
    fn beside(width: usize, right: Option<&(String, Style)>) -> Self {
        Self {
            spans: Vec::new(),
            used: 0,
            budget: width.saturating_sub(right.map_or(0, |(text, _)| text.width())),
        }
    }

    fn push(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        self.used += text.width();
        self.spans.push(Span::styled(text, style));
    }

    /// Pushes as much of `text` as the room before the slot holds, cut at a
    /// character with no ellipsis: what the tree's guides and an entry's
    /// Marker are cut to where nothing else is left to give way.
    fn push_within(&mut self, text: &str, style: Style) {
        let mut room = self.budget.saturating_sub(self.used);
        let kept = text
            .chars()
            .take_while(|character| {
                let columns = character.width().unwrap_or(0);
                let fits = columns <= room;
                if fits {
                    room -= columns;
                }
                fits
            })
            .collect::<String>();
        self.push(kept, style);
    }

    /// Whether `text` fits whole in the room left before the slot.
    fn fits(&self, text: &str) -> bool {
        self.used + text.width() <= self.budget
    }

    /// The columns left before the slot for the text that comes next, once
    /// the pieces still to follow it are set aside.
    fn room_beside(&self, following: &[&Option<(String, Style)>]) -> usize {
        following
            .iter()
            .filter_map(|piece| piece.as_ref())
            .fold(self.budget.saturating_sub(self.used), |room, (text, _)| {
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
