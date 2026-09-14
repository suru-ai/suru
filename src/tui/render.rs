//! Frame rendering: the landing and Session screens, the pickers and overlays,
//! and the shared text and layout helpers they draw with.

use super::selection::{SelectionFrame, SelectionSurface};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    managed_client::SessionProjection,
    protocol::{
        LandingPage, ModelAvailability, ModelDescriptor, ServerIdentity, SessionContentWidth,
        SessionSnapshot, SessionStatus, SessionTimestamp,
    },
    provider::built_in_providers,
    theme::Theme,
};

use super::{
    approval_posture_picker::ApprovalPostureChoice,
    completion::CompletionRow,
    composer::{ComposerKey, ComposerMemory, ComposerSkillMarkers},
    keymap::binding_label,
    model_options::ModelOptionChoiceRow,
    model_picker::ModelPickerRow,
    session_picker::SessionPickerRow,
    settings_panel::{
        PanelLayout, RowAvailability, RowExpansion, RowValue, RowWindow, TabBar, TabSpan,
    },
    shimmer,
    sidebar::{
        self, ADD_WORKSPACE, SessionStanding, Sidebar, SidebarEntry, SidebarMenuGeometry,
        SidebarRow, SidebarScopeEntry, SidebarSelectorView, SidebarShelf, SidebarShowMore,
        SidebarSpan, SidebarTarget, SidebarUnreachable, SidebarWorkspaceEntryView,
    },
    slots::{
        ApplicationNoticeSlotContext, LandingFooterSlotContext, PromptContextSlotContext,
        PromptFooterSlotContext, RenderSlots, RenderedSlot, SessionComposerTopSlotContext,
        SlotText, WorkingIndicatorInterrupt, WorkingIndicatorSlotContext, WorkingIndicatorState,
        truncate_slot_text, truncate_to_width,
    },
    spinner,
    state::{
        ClaimStanding, CommandId, CommandMode, QueuedPrompt, SessionInteraction,
        TranscriptViewport, TuiState,
    },
    subagent_picker::working_subagents,
    text_layout::{TextLayout, draw_row},
    theme_picker::ThemePickerRow,
    transcript::{TranscriptView, client_error_lines},
    usage::{compact_cost, compact_count},
    workspace_picker::WorkspacePickerRow,
};

const NARROW_TERMINAL_WIDTH: u16 = 44;
const MINIMUM_TERMINAL_WIDTH: u16 = 28;
const MINIMUM_TERMINAL_HEIGHT: u16 = 5;
const LANDING_DETAILS_MINIMUM_HEIGHT: u16 = 9;
const LANDING_LOGO: [&str; 7] = [
    "        █          ▀▀▀▀▀▀▀█",
    "▀▀▀▀▀▀▀▀█▀▀▀▀▀          ▄▀",
    "     ▄▀▀█             ▄▀",
    "     ▀▄▄█           ▄▀▀▀▀▀▄",
    "        █          █       █",
    "       ▄▀           ▄▀▀▄   █",
    "     ▀▀             ▀▄▄▀▄▄▀",
];
/// Nerd Fonts `nf-cod-folder` (Codicons folder).
const NF_COD_FOLDER: char = '\u{ea83}';
/// Nerd Fonts `nf-fa-folder_tree` (Font Awesome folder tree).
const NF_FA_FOLDER_TREE: char = '\u{ef81}';
/// Nerd Fonts `nf-md-monitor` (Material Design monitor).
const NF_MD_MONITOR: char = '\u{f0379}';
/// Nerd Fonts `nf-cod-git_branch` (Codicons git-branch).
const NF_COD_GIT_BRANCH: char = '\u{ec6f}';
/// Nerd Fonts `nf-cod-worktree` (Codicons worktree).
const NF_COD_WORKTREE: char = '\u{ec7e}';
/// Nerd Fonts `nf-cod-git_commit` (Codicons git-commit).
const NF_COD_GIT_COMMIT: char = '\u{eafc}';
const SESSION_HEADER_MINIMUM_HEIGHT: u16 = 8;
/// Rows of air the layout keeps below the Transcript, so its last entry never
/// abuts whatever is docked underneath.
const TRANSCRIPT_BOTTOM_MARGIN: u16 = 1;
/// Columns of air the composer keeps between its border and the Prompt being
/// typed, so text never abuts the box it is written in.
const COMPOSER_TEXT_MARGIN: u16 = 1;
/// The row the Session view's composer footer is drawn on. The shell an
/// opening Session is drawn as holds it empty rather than closing the gap, so
/// the composer stands where hydration will leave it.
const SESSION_FOOTER_HEIGHT: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponsiveDetail {
    CoreOnly,
    Secondary,
}

impl ResponsiveDetail {
    fn for_width(width: u16) -> Self {
        if width < NARROW_TERMINAL_WIDTH {
            Self::CoreOnly
        } else {
            Self::Secondary
        }
    }

    fn secondary_only_when(self, visible: bool) -> Self {
        if visible { self } else { Self::CoreOnly }
    }

    fn shows_secondary(self) -> bool {
        self == Self::Secondary
    }
}

pub(super) fn render_with_slots(
    frame: &mut Frame<'_>,
    state: &TuiState,
    slots: &RenderSlots,
    theme: &Theme,
    truecolor: bool,
) {
    // A named Theme owns the entire canvas. For System and transparent Themes
    // the base uses Reset, so this same fill deliberately exposes the
    // terminal's background instead.
    frame.render_widget(
        Block::default().style(theme.surface.base.patch(theme.text.primary)),
        frame.area(),
    );
    // The settings panel is pointable, and only a frame that drew it can say
    // where. Every frame starts by giving up what the last one recorded, so the
    // geometry a click resolves against is always the one on screen.
    state.selection_frames.borrow_mut().clear();
    state.selection_overlay_area.set(None);
    state.settings_panel.forget_layout();
    // The Sidebar's own frame record, given up for the same reason: whether it
    // has the keys depends on whether the frame had the columns to draw it.
    state.sidebar.forget_frame();
    // A frame without an editable composer must leave no pointer target.
    state.composers.forget_frame();
    // The Subagent Picker's rows are pointable, so its record of where they
    // were drawn starts over with the frame as well.
    state.subagent_picker.forget_frame();
    // Current-Session animation is likewise a fact about this frame, not the
    // Session in the abstract: its transient tail may have scrolled away.
    state.session_animation_on_screen.set(false);
    if terminal_is_too_small(frame.area()) {
        render_terminal_too_small(frame, theme);
        return;
    }
    let main = render_sidebar(frame, state, theme, truecolor);
    let main = render_application_notice(frame, state, main, slots, theme);
    let composer = if state.session.is_some() {
        render_session(frame, state, main, slots, theme, truecolor)
    } else if state.route.is_some() {
        render_opening_session(frame, state, main, theme, truecolor)
    } else if state.provisional.is_some() {
        render_provisional_session(frame, state, main, slots, theme, truecolor)
    } else {
        render_landing(frame, state, main, slots, theme)
    };
    state.composers.highlight_selection(frame.buffer_mut());
    if let Some(surface) = state.composers.selection_frame() {
        state.selection_frames.borrow_mut().push(surface);
    }
    let pending_approvals: Vec<_> = state.pending_approvals().collect();
    let pending: Vec<_> = state.pending_questionnaires().collect();
    if let Some(approval) = state.open_approval() {
        state
            .approvals
            .render(frame, composer.area, approval, theme);
    } else if let Some(questionnaire) = state.open_questionnaire() {
        state.questionnaires.render(
            frame,
            composer.area,
            questionnaire,
            theme,
            (
                pending
                    .iter()
                    .position(|q| q.id == questionnaire.id)
                    .unwrap_or(0)
                    + 1,
                pending.len(),
            ),
        );
    } else {
        // Allocate every intervention notice as one stack so a descendant
        // notice cannot paint over the owning Session's controls.
        let subagent_questions = state.session.as_ref().map_or(0, |session| {
            session.snapshot().subagent_questionnaire_count()
        });
        let subagent_approvals = state
            .session
            .as_ref()
            .map_or(0, |session| session.snapshot().subagent_approval_count());
        let mut intervention_notices = Vec::new();
        if subagent_questions > 0 || subagent_approvals > 0 {
            let pending = match (subagent_questions, subagent_approvals) {
                (questions, 0) => format!("{questions} Subagent questionnaires pending"),
                (0, approvals) => format!("{approvals} Subagent Approvals pending"),
                (questions, approvals) => {
                    format!(
                        "{questions} Subagent questionnaire{} · {approvals} Approval{} pending",
                        if questions == 1 { "" } else { "s" },
                        if approvals == 1 { "" } else { "s" },
                    )
                }
            };
            intervention_notices.push(format!("{pending} · ↓ browse Subagents"));
        }
        if !pending_approvals.is_empty() {
            intervention_notices.push(format!(
                "Approval pending ({}) · Ctrl+Y decide",
                pending_approvals.len()
            ));
        }
        if !pending.is_empty() {
            intervention_notices.push(format!(
                "Questionnaire pending ({}) · Ctrl+Q answer",
                pending.len()
            ));
        }
        let available_rows = usize::from(composer.area.y.saturating_sub(frame.area().y));
        let first_visible = intervention_notices.len().saturating_sub(available_rows);
        let visible = &intervention_notices[first_visible..];
        let first_y = composer.area.y.saturating_sub(visible.len() as u16);
        for (offset, notice) in visible.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(notice.as_str()).style(theme.feedback.warning),
                Rect::new(
                    composer.area.x,
                    first_y.saturating_add(offset as u16),
                    composer.area.width,
                    1,
                ),
            );
        }
    }
    if state.composer_completion.is_visible() && !state.reconnect_overlay_visible {
        render_composer_completion(frame, state, composer.area, theme);
    }
    // The Subagent Picker docks over the composer the way the completion list
    // does: it belongs to the composer's place on screen, not to the main
    // view's center.
    if state.subagent_picker.is_open() && !state.reconnect_overlay_visible {
        render_subagent_picker(frame, state, composer.area, theme);
    }
    // The Sidebar's own context menu, drawn over the column and whatever of
    // the main view it runs into, because it stands in front of the row it was
    // opened on.
    if !state.reconnect_overlay_visible {
        render_sidebar_menu(frame, state, theme);
    }
    // Every overlay is centered on the main view rather than the whole frame:
    // the Sidebar sits beside them and is neither opened over nor obscured.
    if state.session_picker.is_open() && !state.reconnect_overlay_visible {
        render_session_picker(frame, state, main, theme);
    }
    if state.workspace_picker.is_open() && !state.reconnect_overlay_visible {
        render_workspace_picker(frame, state, main, theme);
    }
    if state.worktree_picker.open && !state.reconnect_overlay_visible {
        render_worktree_picker(frame, state, main, theme);
    }
    if state.settings_panel.is_open() && !state.reconnect_overlay_visible {
        render_settings_panel(frame, state, main, theme);
        if state.settings_panel.numeric_editor().is_some() {
            render_numeric_editor(frame, state, main, theme);
        }
    }
    // Choice pickers can be opened by a settings row, so they are drawn over
    // the panel that asked and take the reader's answer first.
    if state.model_options.is_open() && !state.reconnect_overlay_visible {
        render_model_options(frame, state, main, theme);
    }
    if state.model_picker.is_open() && !state.reconnect_overlay_visible {
        render_model_picker(frame, state, main, theme);
    }
    if state.approval_posture_picker.is_open() && !state.reconnect_overlay_visible {
        render_approval_posture_picker(frame, state, main, theme);
    }
    if state.theme_picker.is_open() && !state.reconnect_overlay_visible {
        render_theme_picker(frame, state, main, theme);
    }
    if state.serve_overlay.is_open() && !state.reconnect_overlay_visible {
        render_serve_overlay(frame, state, main, theme);
    }
    if state.connect_overlay.is_open() && !state.reconnect_overlay_visible {
        render_connect_overlay(frame, state, main, theme);
    }
    if state.reconnect_overlay_visible {
        state.selection_frames.borrow_mut().clear();
        render_reconnect_overlay(frame, theme);
    } else if !state.session_picker.is_open()
        && !state.workspace_picker.is_open()
        && !state.model_picker.is_open()
        && !state.approval_posture_picker.is_open()
        && !state.theme_picker.is_open()
        && !state.model_options.is_open()
        && !state.settings_panel.is_open()
        && !state.serve_overlay.is_open()
        && !state.connect_overlay.is_open()
        && !state.sidebar.menu_is_open()
        && !state.subagent_picker.is_open()
        && state.composer_focused()
        && matches!(state.command_mode, CommandMode::Composer)
        // The frame may have drawn no composer at all — a Subagent's Session
        // stands one down — and then there is no caret to place.
        && let Some(cursor) = composer.cursor
    {
        frame.set_cursor_position(cursor);
    }
    if let Some(selection) = state.text_selection.get()
        && selection.surface != SelectionSurface::Transcript
    {
        if let Some(surface) = state.selection_frames.borrow().iter().find(|surface| {
            surface.surface == selection.surface && surface.epoch() == selection.epoch
        }) {
            surface.highlight(selection, frame.buffer_mut());
        } else {
            state.text_selection.set(None);
        }
    }
}

/// Draws configuration and runtime fallback Notices above whichever view is
/// open. A Notice must stay visible until the reader has had a frame in which
/// to see it; keeping it outside the route renderers prevents a Session from
/// hiding a Theme fallback that arrived with a settings snapshot.
fn render_application_notice(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
) -> Rect {
    let inset = horizontal_padding(area.width);
    let width = area.width.saturating_sub(inset.saturating_mul(2));
    let notice = slots.application_notice(&ApplicationNoticeSlotContext {
        width,
        notice: state.application_notice().map(|notice| {
            notice.mark_shown();
            SlotText::new(notice.text(width), notice.style(theme))
        }),
    });
    let [notice_area, main] =
        Layout::vertical([Constraint::Length(notice.height()), Constraint::Min(1)]).areas(area);
    render_slot(frame, horizontally_inset(notice_area, inset), notice, theme);
    main
}

fn render_connect_overlay(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let overlay = &state.connect_overlay;
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(88),
        main.height.saturating_sub(2).min(18),
    );
    if let Some(label) = overlay.loading_label() {
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Connect,
            area,
            vec![Line::styled(label, theme.text.subdued)],
            " Connect ",
            theme,
        );
        return;
    }
    if let Some((invite, error)) = overlay.invite_entry() {
        let mut lines = vec![Line::styled(
            "Paste Invite",
            theme.text.primary.add_modifier(Modifier::BOLD),
        )];
        lines.push(Line::styled(
            if invite.is_empty() {
                "▏".to_owned()
            } else {
                invite.to_owned()
            },
            theme.text.primary,
        ));
        if let Some(error) = error {
            lines.push(Line::styled(error.to_owned(), theme.feedback.error));
        }
        lines.push(Line::styled(
            "Enter inspect · Esc close",
            theme.text.subdued,
        ));
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Connect,
            area,
            lines,
            " Connect ",
            theme,
        );
        return;
    }
    if let Some(preview) = overlay.confirmation() {
        let mut lines = vec![Line::styled(
            "Confirm Serving Server",
            theme.text.primary.add_modifier(Modifier::BOLD),
        )];
        lines.push(Line::styled("Fingerprint", theme.text.subdued));
        lines.extend(
            TextLayout::new(&preview.fingerprint, area.width.saturating_sub(2))
                .rows()
                .map(|row| Line::styled(row.text.to_owned(), theme.text.primary)),
        );
        lines.push(Line::styled("Enter trust · Esc cancel", theme.text.subdued));
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Connect,
            area,
            lines,
            " Connect ",
            theme,
        );
        return;
    }
    if let Some(details) = overlay.details() {
        let mut lines = vec![Line::styled(
            "Configure Remote",
            theme.text.primary.add_modifier(Modifier::BOLD),
        )];
        lines.push(Line::styled("Remote name", theme.text.subdued));
        lines.push(Line::styled(
            format!("> {}", details.name),
            if details.name_focused {
                theme.selection.focused
            } else {
                theme.text.primary
            },
        ));
        lines.push(Line::styled("Address priority", theme.text.subdued));
        let content_height = usize::from(area.height.saturating_sub(2));
        let error_rows = usize::from(details.error.is_some());
        let address_capacity = content_height.saturating_sub(lines.len() + error_rows + 1);
        let addresses = details
            .addresses
            .iter()
            .enumerate()
            .map(|(index, address)| {
                let selected = index == details.selected;
                Line::styled(
                    format!(
                        "{}{}. {address}",
                        if selected { "› " } else { "  " },
                        index + 1
                    ),
                    selection_style(selected, !details.name_focused, theme)
                        .unwrap_or(theme.text.primary),
                )
            })
            .collect::<Vec<_>>();
        lines.extend(visible_window(
            addresses,
            details.selected,
            address_capacity,
        ));
        if let Some(error) = details.error {
            lines.push(Line::styled(error.to_owned(), theme.feedback.error));
        }
        lines.push(Line::styled(
            "↑↓ move · Shift+↑↓ reorder · Tab field · Enter pair · Esc cancel",
            theme.text.subdued,
        ));
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Connect,
            area,
            lines,
            " Connect ",
            theme,
        );
        return;
    }
    let mut lines = vec![Line::styled(
        "Paired Remotes",
        theme.text.primary.add_modifier(Modifier::BOLD),
    )];
    let remote_capacity = usize::from(area.height.saturating_sub(2)).saturating_sub(2);
    let mut remote_rows = vec![Line::styled(
        if overlay.selected() == 0 {
            "› Local"
        } else {
            "  Local"
        },
        if overlay.selected() == 0 {
            theme.selection.focused
        } else {
            theme.text.primary
        },
    )];
    remote_rows.extend(overlay.remotes().iter().enumerate().map(|(index, remote)| {
        let row_index = index + 1;
        let prefix = if row_index == overlay.selected() {
            "› "
        } else {
            "  "
        };
        Line::styled(
            format!(
                "{prefix}{}  {}",
                remote.name,
                overlay.status_label(&remote.name)
            ),
            if row_index == overlay.selected() {
                theme.selection.focused
            } else {
                theme.text.primary
            },
        )
    }));
    lines.extend(visible_window(
        remote_rows,
        overlay.selected(),
        remote_capacity,
    ));
    lines.push(Line::styled(
        "↑↓ choose · a pair another · Esc close",
        theme.text.subdued,
    ));
    render_overlay_box(
        frame,
        state,
        SelectionSurface::Connect,
        area,
        lines,
        " Connect ",
        theme,
    );
}

fn render_serve_overlay(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(96),
        main.height.saturating_sub(2).min(18),
    );
    if state.serve_overlay.is_preparing() {
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Serve,
            area,
            vec![Line::styled("Preparing Serving…", theme.text.subdued)],
            " Serve ",
            theme,
        );
        return;
    }

    if let Some(invite) = state.serve_overlay.invite() {
        let content_width = area.width.saturating_sub(2);
        let content_height = usize::from(area.height.saturating_sub(2));
        let invite_lines = TextLayout::new(&invite.invite, content_width)
            .rows()
            .map(|row| Line::styled(row.text.to_owned(), theme.text.primary))
            .collect::<Vec<_>>();
        let mut lines = vec![Line::styled(
            "Fresh Invite",
            theme.text.primary.add_modifier(Modifier::BOLD),
        )];
        lines.extend(invite_lines);
        lines.push(Line::styled("Enrolled Peers", theme.text.primary));
        if state.serve_overlay.peers().is_empty() {
            lines.push(Line::styled("No Peers enrolled", theme.text.subdued));
        } else {
            let error_rows = usize::from(state.serve_overlay.error().is_some());
            let peer_capacity = content_height.saturating_sub(lines.len() + error_rows + 1);
            let peer_groups = state
                .serve_overlay
                .peers()
                .iter()
                .enumerate()
                .map(|(index, peer)| {
                    serve_peer_lines(
                        &peer.fingerprint,
                        index == state.serve_overlay.selected(),
                        content_width,
                        theme,
                    )
                })
                .collect::<Vec<_>>();
            lines.extend(visible_line_groups(
                peer_groups,
                state.serve_overlay.selected(),
                peer_capacity,
            ));
        }
        if let Some(error) = state.serve_overlay.error() {
            lines.push(Line::styled(error.to_owned(), theme.feedback.error));
        }
        lines.push(Line::styled(
            "Ctrl+C copy · ↑↓ choose Peer · x remove · Esc close",
            theme.text.subdued,
        ));
        render_overlay_box(
            frame,
            state,
            SelectionSurface::Serve,
            area,
            lines,
            " Serve ",
            theme,
        );
        return;
    }

    let mut lines = vec![Line::styled(
        "Choose Invite addresses",
        theme.text.primary.add_modifier(Modifier::BOLD),
    )];
    if state.serve_overlay.candidates().is_empty() {
        lines.push(Line::styled(
            "No non-loopback addresses found",
            theme.feedback.error,
        ));
    } else {
        let content_height = usize::from(area.height.saturating_sub(2));
        let error_rows = usize::from(state.serve_overlay.error().is_some());
        let capacity = content_height.saturating_sub(lines.len() + error_rows + 1);
        let rows = state
            .serve_overlay
            .candidates()
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let marker = if candidate.chosen { "[x]" } else { "[ ]" };
                Line::styled(
                    format!("{marker} {}", candidate.address),
                    if index == state.serve_overlay.selected() {
                        theme.selection.focused
                    } else {
                        theme.text.primary
                    },
                )
            })
            .collect::<Vec<_>>();
        lines.extend(visible_window(
            rows,
            state.serve_overlay.selected(),
            capacity,
        ));
    }
    if let Some(error) = state.serve_overlay.error() {
        lines.push(Line::styled(error.to_owned(), theme.feedback.error));
    }
    lines.push(Line::styled(
        "↑↓ move · Space toggle · Enter issue Invite · Esc close",
        theme.text.subdued,
    ));
    render_overlay_box(
        frame,
        state,
        SelectionSurface::Serve,
        area,
        lines,
        " Serve ",
        theme,
    );
}

fn serve_peer_lines(
    fingerprint: &str,
    selected: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let prefix = if selected { "› " } else { "  " };
    let style = if selected {
        theme.selection.focused
    } else {
        theme.text.primary
    };
    TextLayout::new(fingerprint, width.saturating_sub(2))
        .rows()
        .map(|row| Line::styled(format!("{prefix}{}", row.text), style))
        .collect()
}

/// A variable-height list window that always keeps the complete focused group
/// on screen where it fits, then fills the remaining room with its neighbors.
fn visible_line_groups(
    groups: Vec<Vec<Line<'static>>>,
    selected: usize,
    capacity: usize,
) -> Vec<Line<'static>> {
    let Some(selected) = (selected < groups.len()).then_some(selected) else {
        return Vec::new();
    };
    let mut start = selected;
    let mut end = selected + 1;
    let mut used = groups[selected].len();
    while start > 0 && used + groups[start - 1].len() <= capacity {
        start -= 1;
        used += groups[start].len();
    }
    while end < groups.len() && used + groups[end].len() <= capacity {
        used += groups[end].len();
        end += 1;
    }
    groups
        .into_iter()
        .skip(start)
        .take(end - start)
        .flatten()
        .take(capacity)
        .collect()
}

fn render_numeric_editor(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let editor = state
        .settings_panel
        .numeric_editor()
        .expect("the numeric editor is open when it is rendered");
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(44),
        main.height.saturating_sub(2).min(6),
    );
    let lines = vec![
        Line::styled(editor.label, theme.text.subdued),
        Line::styled(format!("› {}", editor.input), theme.form_field.text),
        Line::styled(editor.error.unwrap_or(""), theme.feedback.error),
        Line::styled("Enter apply · Esc cancel", theme.text.subdued),
    ];
    render_overlay_box(
        frame,
        state,
        SelectionSurface::NumericEditor,
        area,
        lines,
        " Number ",
        theme,
    );
}

fn render_session_picker(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(72),
        main.height.saturating_sub(2).min(12),
    );
    let content_width = area.width.saturating_sub(2);
    let content_height = area.height.saturating_sub(2);
    let mut lines = Vec::with_capacity(usize::from(content_height));
    let shows_search_and_footer = content_height >= 3;
    let error_in_title = content_height <= 3
        && !state.session_picker.is_loading()
        && state.session_picker.error().is_some();
    if shows_search_and_footer {
        lines.push(Line::styled(
            picker_search_line(state.session_picker.query(), usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    if let Some(error) = state.session_picker.error()
        && !error_in_title
        && lines.len() < usize::from(content_height)
    {
        lines.push(Line::styled(
            truncate_to_width(&format!("Error: {error}"), usize::from(content_width)),
            theme.feedback.error,
        ));
    }
    if state.session_picker.is_loading() && lines.len() < usize::from(content_height) {
        lines.push(Line::styled("Loading Sessions…", theme.text.subdued));
    } else {
        let current = state.open_session_reference();
        let footer_rows = usize::from(shows_search_and_footer);
        let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
        let now = SessionTimestamp::now().0;
        let rows = state
            .session_picker
            .visible_rows(row_capacity, current)
            .map(|row| session_picker_row_line(row, usize::from(content_width), now, theme, state))
            .collect::<Vec<_>>();
        if rows.is_empty() && lines.len() < usize::from(content_height).saturating_sub(footer_rows)
        {
            lines.push(Line::styled("No Sessions found", theme.text.subdued));
        } else {
            lines.extend(rows);
        }
    }
    if shows_search_and_footer && lines.len() < usize::from(content_height) {
        let scope = state.session_picker.scope_label();
        let status = if state.session_picker.is_attaching() {
            "Attaching…"
        } else if state.session_picker.is_deleting() {
            "Deleting…"
        } else {
            "Ctrl+A scope · Enter attach · Ctrl+D delete · Esc close"
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{scope} · {status}"), usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    let title = if error_in_title {
        format!(
            " Sessions · Error: {} ",
            state
                .session_picker
                .error()
                .expect("error title requires a picker error")
        )
    } else {
        " Sessions ".to_owned()
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
    record_overlay_selection(frame, state, area, SelectionSurface::Sessions, true);
}

/// The Worktree Selector: a centered box over the Landing that says where the
/// next Session stands today on a line of its own, then offers a new Worktree
/// ahead of every Worktree the Repository already has, each named by its
/// Checkout State and where it stands.
fn render_worktree_picker(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let picker = &state.worktree_picker;
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(88),
        main.height.saturating_sub(2).min(14),
    );
    let capacity = usize::from(area.height.saturating_sub(2));
    let width = usize::from(area.width.saturating_sub(2));
    let mut lines = Vec::new();
    if let Some(preview) = &picker.removal {
        lines.push(Line::styled(
            format!(
                "Remove linked Worktree on {}?",
                match &state.outlook {
                    crate::protocol::Outlook::Local => "Local Server",
                    crate::protocol::Outlook::Remote(alias) => alias.as_str(),
                }
            ),
            theme.text.primary,
        ));
        lines.push(Line::raw(
            state.workspace_label(&state.outlook, &preview.target.checkout.root),
        ));
        lines.push(Line::raw(format!(
            "{} affected Sessions on this Server · {} Working",
            preview.affected_sessions, preview.working_sessions
        )));
        let facts = &preview.inspection;
        lines.push(Line::raw(format!(
            "Tracked changes: {} · untracked: {} · ignored: {}",
            facts.tracked.len(),
            facts.untracked.len(),
            facts.ignored.len()
        )));
        if !facts.ignored.is_empty() {
            lines.push(Line::raw(
                "Ignored contents will also be deleted by ordinary removal",
            ));
        }
        if let Some(lock) = &facts.lock {
            lines.push(Line::raw(format!("Locked: {lock}")));
        }
        if !facts.initialized_submodules.is_empty() {
            lines.push(Line::raw("Initialized submodules will be removed"));
        }
        lines.push(Line::raw(
            "Branch and Session histories retained; uncommitted files cannot be recovered",
        ));
        lines.push(Line::raw(if picker.loading {
            "Removing…"
        } else if preview.working_sessions != 0 {
            "Working Sessions block removal · Esc cancel"
        } else if facts.requires_force() {
            "F Force remove · Esc cancel"
        } else {
            "Enter Remove · Esc cancel"
        }));
    } else if picker.loading {
        lines.push(Line::styled("Loading Worktrees…", theme.text.subdued));
    } else if let Some(context) = &picker.context {
        // Where the next Session stands today, which the reader reads rather
        // than chooses: the Worktree it is in has a row of its own below.
        let presentation = context
            .workspace
            .repository
            .as_ref()
            .map(|repository| repository.presentation_path());
        let current = context
            .execution_directory
            .as_ref()
            .map(|directory| state.worktree_location(&state.outlook, &directory.path, presentation))
            .unwrap_or_else(|| "No working copy selected".to_owned());
        lines.push(Line::styled(
            truncate_to_width(
                &format!("Current: {current}{}", picker.current_status()),
                width,
            ),
            theme.text.subdued,
        ));
        let reason = context
            .workspace
            .repository
            .as_ref()
            .map(|r| &r.capabilities.create_checkout);
        let mut rows = vec![match reason {
            Some(crate::protocol::SourceControlCapability::Available) => "New Worktree".to_owned(),
            Some(crate::protocol::SourceControlCapability::Unsupported { reason }) => {
                format!("New Worktree unavailable · {reason}")
            }
            None => "New Worktree unavailable · no Repository".to_owned(),
        }];
        rows.extend(context.checkouts.iter().map(|checkout| {
            // The Worktree the reader is already in is marked rather than
            // moved, so the rows keep the order the Repository gives them.
            let marker = if picker.current_checkout() == Some(&checkout.association.id) {
                "* "
            } else {
                ""
            };
            let location =
                state.worktree_location(&state.outlook, &checkout.association.root, presentation);
            match CheckoutStateLabel::read_in(checkout, CheckoutStateForm::Choosing) {
                Some(label) => format!("{marker}{} · {location}", label.text()),
                None => format!("{marker}{location}"),
            }
        }));
        let available_rows = capacity.saturating_sub(2).max(1);
        let start = picker
            .selected
            .saturating_add(1)
            .saturating_sub(available_rows);
        for (index, row) in rows
            .into_iter()
            .enumerate()
            .skip(start)
            .take(available_rows)
        {
            lines.push(Line::styled(
                truncate_to_width(
                    &format!("{} {row}", if index == picker.selected { "›" } else { " " }),
                    width,
                ),
                if index == picker.selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                },
            ));
        }
        lines.push(Line::styled(
            "Enter select · d Remove linked Worktree · Esc close",
            theme.text.subdued,
        ));
    }
    if let Some(error) = &picker.error {
        lines.truncate(capacity.saturating_sub(1));
        lines.push(Line::styled(
            truncate_to_width(error, width),
            theme.feedback.error,
        ));
    }
    lines.truncate(capacity);
    render_overlay_box(
        frame,
        state,
        SelectionSurface::Worktrees,
        area,
        lines,
        " Worktrees ",
        theme,
    );
}

fn render_workspace_picker(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(72),
        main.height
            .saturating_sub(2)
            .max(4)
            .min(main.height)
            .min(12),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let content_height = usize::from(area.height.saturating_sub(2));
    let mut lines = Vec::with_capacity(content_height);
    let shows_search = content_height >= 3;
    let shows_footer = content_height >= 2;
    if shows_search {
        lines.push(Line::styled(
            picker_search_line(state.workspace_picker.query(), content_width),
            theme.text.subdued,
        ));
    }
    if let Some(error) = state.workspace_picker.error()
        && lines.len() < content_height
    {
        lines.push(Line::styled(
            truncate_to_width(&format!("Error: {error}"), content_width),
            theme.feedback.error,
        ));
    }
    if state.workspace_picker.is_loading() && lines.len() < content_height {
        lines.push(Line::styled("Loading Workspaces…", theme.text.subdued));
    } else {
        let footer_rows = usize::from(shows_footer);
        let capacity = content_height.saturating_sub(lines.len() + footer_rows);
        let rows = state
            .workspace_picker
            .visible_rows(capacity)
            .into_iter()
            .map(|row| {
                let style = if row.selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                };
                Line::styled(workspace_picker_row_text(&row, content_width, state), style)
            })
            .collect::<Vec<_>>();
        if rows.is_empty() && lines.len() < content_height.saturating_sub(footer_rows) {
            // Only a query can empty the list — the Workspace the client works
            // in always stands otherwise — and it is said in words, because an
            // empty box would read as the reader having no Workspaces at all.
            lines.push(Line::styled("No Workspaces found", theme.text.subdued));
        } else {
            lines.extend(rows);
        }
    }
    if shows_footer && lines.len() < content_height {
        // Named in full where the box can hold it, and by the keys alone where
        // it cannot — the same trade the rows make of "[current]" for "C", so
        // a narrow terminal loses wording rather than an affordance.
        let (footer, style) = if let Some(refusal) = state.workspace_picker.refusal() {
            (refusal, theme.feedback.error)
        } else if content_width < usize::from(NARROW_TERMINAL_WIDTH) {
            ("Enter · Esc", theme.text.subdued)
        } else {
            ("Enter switch · Esc close", theme.text.subdued)
        };
        lines.push(Line::styled(
            truncate_to_width(footer, content_width),
            style,
        ));
    }
    render_overlay_box(
        frame,
        state,
        SelectionSurface::Workspaces,
        area,
        lines,
        " Workspaces ",
        theme,
    );
}

/// One Workspace Picker row: the name the Workspace goes by, whether it is
/// where the client is working, and the path spelled in full — truncated from
/// the left where the row cannot hold it, so the directories that tell two
/// Workspaces of the same name apart are what survives.
fn workspace_picker_row_text(row: &WorkspacePickerRow, width: usize, state: &TuiState) -> String {
    let marker = if row.selected { "› " } else { "  " };
    let compact = width < usize::from(NARROW_TERMINAL_WIDTH);
    let separator = if compact { " " } else { " · " };
    let mut fields = vec![row.name.clone()];
    if row.current {
        fields.push((if compact { "C" } else { "[current]" }).to_owned());
    }
    let path_budget = width
        .saturating_sub(marker.width())
        .saturating_sub(fields.join(separator).width())
        .saturating_sub(separator.width());
    if path_budget > 0 {
        fields.push(truncate_from_left_to_width(
            &state.workspace_label(&state.outlook, &row.path),
            path_budget,
        ));
    }
    truncate_to_width(&format!("{marker}{}", fields.join(separator)), width)
}

fn render_model_picker(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(76),
        main.height.saturating_sub(2).min(14),
    );
    let content_width = area.width.saturating_sub(2);
    let content_height = area.height.saturating_sub(2);
    let mut lines = Vec::with_capacity(usize::from(content_height));
    if content_height >= 3 {
        lines.push(Line::styled(
            picker_search_line(state.model_picker.query(), usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    if content_height >= 3
        && let Some(provider) = state.model_picker.provider_scope()
        && lines.len() < usize::from(content_height)
    {
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "Session Provider {} · use /new to change Provider",
                    state.model_picker.provider_display_name(provider)
                ),
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    if state.model_picker.is_loading() && lines.len() < usize::from(content_height) {
        lines.push(Line::styled("Loading Models…", theme.text.subdued));
    } else {
        let footer_rows = usize::from(content_height >= 3);
        let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
        let current = state.agent_selection();
        let rows = state
            .model_picker
            .visible_rows(row_capacity, current)
            .map(|row| match row {
                ModelPickerRow::Provider { name, refreshing } => Line::styled(
                    truncate_to_width(
                        &format!(
                            "Provider {name}{}",
                            if refreshing { " · refreshing" } else { "" }
                        ),
                        usize::from(content_width),
                    ),
                    theme.accent.primary.add_modifier(Modifier::BOLD),
                ),
                ModelPickerRow::Model {
                    model,
                    selected,
                    current,
                } => Line::styled(
                    model_picker_row_text(model, selected, current, usize::from(content_width)),
                    if selected {
                        theme.selection.focused
                    } else if model.availability == ModelAvailability::Unavailable {
                        theme.text.subdued
                    } else {
                        theme.text.primary
                    },
                ),
                // A failing Provider and an unavailable one are the same row:
                // the retry the reader may take, with what it is about to
                // re-check. Only the account of the condition differs.
                ModelPickerRow::Error {
                    name,
                    message,
                    selected,
                } => model_picker_retry_row(
                    &format!("Retry {name}: {message}"),
                    selected,
                    content_width,
                    theme,
                ),
                ModelPickerRow::Unavailable {
                    name,
                    reason,
                    message,
                    selected,
                } => model_picker_retry_row(
                    &format!("Retry {name}: {} · {message}", reason.label()),
                    selected,
                    content_width,
                    theme,
                ),
            })
            .collect::<Vec<_>>();
        lines.extend(rows);
        if !state.model_picker.has_rows() && lines.len() < usize::from(content_height) {
            lines.push(Line::styled("No Models found", theme.text.subdued));
        }
    }
    if content_height >= 3 && lines.len() < usize::from(content_height) {
        lines.push(Line::styled(
            truncate_to_width(
                "Type to search · Enter select/retry · Esc close",
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Models ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
    record_overlay_selection(frame, state, area, SelectionSurface::Models, true);
}

fn render_approval_posture_picker(
    frame: &mut Frame<'_>,
    state: &TuiState,
    main: Rect,
    theme: &Theme,
) {
    let rows = state.approval_posture_picker.rows().collect::<Vec<_>>();
    let height = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(main.height.saturating_sub(2));
    let area = centered_rect(main, main.width.saturating_sub(8).min(58), height);
    let width = usize::from(area.width.saturating_sub(2));
    let capacity = usize::from(area.height.saturating_sub(2));
    let selected = rows
        .iter()
        .position(|(_, selected)| *selected)
        .unwrap_or_default();
    let lines = visible_window(rows, selected, capacity)
        .map(|(row, selected)| {
            let label = match row {
                ApprovalPostureChoice::Value { label, .. } => label,
                ApprovalPostureChoice::Reset => "Follow Server Settings (reset)",
            };
            let marker = if selected { "› " } else { "  " };
            Line::styled(
                truncate_to_width(&format!("{marker}{label}"), width),
                if selected {
                    theme.text.primary
                } else {
                    theme.text.subdued
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Approval Posture ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
}

fn render_theme_picker(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(64),
        main.height.saturating_sub(2).min(14),
    );
    let content_width = area.width.saturating_sub(2);
    let content_height = area.height.saturating_sub(2);
    let mut lines = Vec::with_capacity(usize::from(content_height));
    if content_height >= 3 {
        lines.push(Line::styled(
            picker_search_line(state.theme_picker.query(), usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    let footer_rows = usize::from(content_height >= 3);
    let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
    lines.extend(state.theme_picker.visible_rows(row_capacity).map(
        |ThemePickerRow {
             name,
             source,
             selected,
             current,
         }| {
            let marker = if selected { "› " } else { "  " };
            let current = if current { " · [current]" } else { "" };
            let source = if matches!(source, crate::theme::ThemeSource::User) {
                " · [user]"
            } else {
                ""
            };
            let display_name = if name == "system" { "System" } else { name };
            Line::styled(
                truncate_to_width(
                    &format!("{marker}{display_name}{source}{current}"),
                    usize::from(content_width),
                ),
                if selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                },
            )
        },
    ));
    if !state.theme_picker.has_rows() && lines.len() < usize::from(content_height) {
        lines.push(Line::styled("No Themes found", theme.text.subdued));
    }
    if content_height >= 3 && lines.len() < usize::from(content_height) {
        lines.push(Line::styled(
            truncate_to_width(
                "Type to search · Enter select · Esc cancel",
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Themes ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
    record_overlay_selection(frame, state, area, SelectionSurface::Themes, true);
}

/// What the tab bar puts between two labels, which the hit test steps over as
/// the drawing does.
const SETTINGS_TAB_GAP: &str = "  ";

/// The settings panel: the tab bar, the rows of the tab being shown, what each
/// is worth right now, and whether that value is the reader's own pin or the
/// built-in default. Rows come straight from the latest effective-settings
/// snapshot, so an edit moves a row only once the server has answered for it.
///
/// Drawing is also what tells the panel where it ended up, because the box's
/// height, the tab labels' columns, and the window of rows that fit are all
/// decided here. A pointer is resolved against that record, so the reader can
/// only ever click something this frame actually drew.
fn render_settings_panel(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let show_icons = state.settings().appearance.show_icons;
    let rows = state
        .settings_panel
        .rows(state.settings(), state.pinned_settings());
    // A tab lists a known number of rows, so the panel is exactly as tall as it
    // needs to be: two borders around the tab bar and the headline, one row per
    // Setting, and the controls.
    let wanted = u16::try_from(rows.len().saturating_add(5)).unwrap_or(u16::MAX);
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(76),
        main.height.saturating_sub(2).min(wanted),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let content_height = usize::from(area.height.saturating_sub(2));
    let content_left = area.x.saturating_add(1);
    let content_top = area.y.saturating_add(1);
    // Inside the border on both sides, so the box's own frame and the screen it
    // covers are surfaces a click passes through.
    let content_columns = content_left
        ..area
            .x
            .saturating_add(area.width)
            .saturating_sub(1)
            .max(content_left);
    let mut tab_bar = None;
    let mut lines = Vec::with_capacity(content_height);
    // Every tab, always, so the reader can see what the panel holds without
    // visiting it; the active one is drawn as the accent. A box too short for
    // its own content gives this line up first, because a tab bar over no rows
    // says nothing about the Settings the reader came for.
    if content_height >= 4 {
        let mut spans = Vec::new();
        let mut labels = Vec::new();
        let mut column = content_left;
        for tab in state.settings_panel.tabs() {
            if !spans.is_empty() {
                spans.push(Span::raw(SETTINGS_TAB_GAP));
                column = column
                    .saturating_add(u16::try_from(SETTINGS_TAB_GAP.width()).unwrap_or(u16::MAX));
            }
            let width = u16::try_from(tab.title.width()).unwrap_or(u16::MAX);
            labels.push(TabSpan {
                tab: tab.tab,
                columns: column..column.saturating_add(width),
            });
            column = column.saturating_add(width);
            spans.push(Span::styled(
                tab.title,
                if tab.active {
                    theme.accent.primary.add_modifier(Modifier::BOLD)
                } else {
                    theme.text.subdued
                },
            ));
        }
        tab_bar = Some(TabBar {
            row: content_top,
            labels,
        });
        lines.push(Line::from(spans));
    }
    if content_height >= 2 {
        // What the focused Setting does, what its Provider has to say for
        // itself, or why the last edit never reached the Config Document — a
        // failed edit is the most urgent of the three, so it takes the line,
        // and a Provider's own condition outranks the description of a Setting
        // the reader can read off the row anyway.
        let (headline, style) = match (
            state.settings_panel.error(),
            state.settings_panel.selected_message(state.settings()),
            state.settings_panel.selected_descriptor(),
        ) {
            (Some(error), _, _) => (error.to_owned(), theme.feedback.error),
            (None, Some(message), _) => (message.to_owned(), theme.text.subdued),
            (None, None, Some(descriptor)) => (
                format!("{} · {}", descriptor.key, descriptor.description),
                theme.text.subdued,
            ),
            (None, None, None) => (String::new(), theme.text.subdued),
        };
        lines.push(Line::styled(
            truncate_to_width(&headline, content_width),
            style,
        ));
    }
    let footer_rows = usize::from(content_height >= 3);
    let capacity = content_height.saturating_sub(lines.len() + footer_rows);
    let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
    // Read before the window narrows the rows, and read off the focused row:
    // Enter acts on that row alone, so that row decides whether the key is
    // worth teaching and which of the two things it does it would do.
    let enter_hint = rows.iter().find(|row| row.selected).map_or("", |row| {
        if row.chooses {
            "Enter choose · "
        } else if row.expansion.expands() {
            "Enter expand · "
        } else {
            ""
        }
    });
    // The rows begin under whatever has been drawn above them, and the window
    // decides which of the tab's rows those are — so this frame is the only
    // thing that can say what a pointer over them landed on.
    let rows_top = content_top.saturating_add(u16::try_from(lines.len()).unwrap_or(u16::MAX));
    let first_row = window_start(selected, capacity);
    let mut drawn_rows: u16 = 0;
    for row in visible_window(rows, selected, capacity) {
        drawn_rows = drawn_rows.saturating_add(1);
        let marker = if row.selected { "› " } else { "  " };
        // The affordance says what Enter would do to this row and, on a tab of
        // Providers, holds its column even for the Provider Enter passes over,
        // so the names line up. A revealed Setting steps in past both.
        let expansion = match row.expansion {
            RowExpansion::Absent => "",
            RowExpansion::Unexpandable => "  ",
            RowExpansion::Collapsed => "▸ ",
            RowExpansion::Expanded => "▾ ",
            RowExpansion::Revealed => "    ",
        };
        let origin = if row.pinned { "pinned" } else { "default" };
        // A Provider Suru has been told to leave alone is the one row that
        // reads as its own condition rather than as a value; an enabled
        // Provider claims nothing, because the quiet state is the good one.
        let value = match &row.value {
            RowValue::Choice(value) => format!(" · {value}"),
            RowValue::ProviderEnabled => String::new(),
            RowValue::ProviderDisabled => " · disabled".to_owned(),
        };
        // What Suru has found out about the Provider, which is a word at most:
        // a Spinner while the read runs, the condition's name once it lands,
        // and nothing at all from a Provider still serving its catalog.
        let availability = match row.availability {
            RowAvailability::Quiet => String::new(),
            RowAvailability::Reading => {
                format!(" · {}", spinner::frame(state.spinner_frame / 3))
            }
            RowAvailability::Warning => " · warning".to_owned(),
            RowAvailability::Unavailable(reason) => format!(" · {}", reason.label()),
            RowAvailability::Failed => " · error".to_owned(),
        };
        let label = row.provider.map_or_else(
            || row.label.to_owned(),
            |provider| provider_label(show_icons, provider, row.label),
        );
        lines.push(Line::styled(
            truncate_to_width(
                &format!("{marker}{expansion}{label}{value}{availability} [{origin}]"),
                content_width,
            ),
            match (row.selected, &row.value) {
                (true, _) => theme.selection.focused,
                (false, RowValue::ProviderDisabled) => theme.text.subdued,
                (false, _) => theme.text.primary,
            },
        ));
    }
    if footer_rows > 0 && lines.len() < content_height {
        // Enter is taught only where it does something, so a reader focused on
        // a row that opens onto nothing is never offered a dead key.
        lines.push(Line::styled(
            truncate_to_width(
                &format!("{enter_hint}Left/Right tabs · Space change · Ctrl+D reset · Esc close"),
                content_width,
            ),
            theme.text.subdued,
        ));
    }
    state.settings_panel.record_layout(PanelLayout::new(
        content_columns,
        tab_bar,
        (drawn_rows > 0).then_some(RowWindow {
            top: rows_top,
            first: first_row,
            count: drawn_rows,
        }),
    ));
    render_overlay_box(
        frame,
        state,
        SelectionSurface::Settings,
        area,
        lines,
        " Settings ",
        theme,
    );
}

fn render_model_options(frame: &mut Frame<'_>, state: &TuiState, main: Rect, theme: &Theme) {
    let area = centered_rect(
        main,
        main.width.saturating_sub(4).min(76),
        main.height.saturating_sub(2).min(14),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let content_height = usize::from(area.height.saturating_sub(2));
    let mut lines = Vec::with_capacity(content_height);
    let model = state
        .model_options
        .model()
        .expect("an open options screen has a Model");
    if content_height >= 3 || (state.model_options.is_choice_picker_open() && content_height >= 2) {
        let unavailable = if model.availability == ModelAvailability::Unavailable {
            " · unavailable"
        } else {
            ""
        };
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "{} · Provider {}{unavailable}",
                    model.display_name,
                    state.model_picker.provider_display_name(&model.provider)
                ),
                content_width,
            ),
            theme.accent.primary.add_modifier(Modifier::BOLD),
        ));
    }

    if state.model_options.is_choice_picker_open() {
        let descriptor = state
            .model_options
            .selected_descriptor()
            .expect("an open choice picker has a descriptor");
        if content_height >= 3 {
            lines.push(Line::styled(
                truncate_to_width(
                    descriptor
                        .description
                        .as_deref()
                        .unwrap_or("Choose a value"),
                    content_width,
                ),
                theme.text.subdued,
            ));
        }
        let footer_rows = usize::from(content_height >= 3);
        let capacity = content_height.saturating_sub(lines.len() + footer_rows);
        let rows = state.model_options.choice_rows();
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        lines.extend(
            visible_window(rows, selected, capacity)
                .map(|row| model_option_choice_line(row, content_width, theme)),
        );
        if footer_rows > 0 && lines.len() < content_height {
            lines.push(Line::styled(
                truncate_to_width("Enter choose · Esc cancel all edits", content_width),
                theme.text.subdued,
            ));
        }
        render_overlay_box(
            frame,
            state,
            SelectionSurface::ModelOptions,
            area,
            lines,
            &format!(" {} Choices ", descriptor.label),
            theme,
        );
        return;
    }

    let footer_rows = usize::from(content_height >= 4);
    // With only one content row, show whichever action owns the focus.
    let confirm_rows = usize::from(
        content_height > lines.len()
            && (content_height > 1 || state.model_options.is_confirm_selected()),
    );
    let capacity = content_height.saturating_sub(lines.len() + footer_rows + confirm_rows);
    let rows = state.model_options.rows();
    let selected = rows
        .iter()
        .position(|row| row.selected)
        .unwrap_or(rows.len().saturating_sub(1));
    for row in visible_window(rows, selected, capacity) {
        let marker = if row.selected { "› " } else { "  " };
        let unavailable = if row.available { "" } else { " [unavailable]" };
        let description = row
            .description
            .map_or_else(String::new, |description| format!(" · {description}"));
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "{marker}{} · {}{unavailable}{description}",
                    row.label, row.value
                ),
                content_width,
            ),
            if row.selected {
                theme.selection.focused
            } else if row.available {
                theme.text.primary
            } else {
                theme.text.subdued
            },
        ));
    }
    if confirm_rows > 0 {
        while lines.len() < content_height.saturating_sub(footer_rows + confirm_rows) {
            lines.push(Line::default());
        }
        let selected = state.model_options.is_confirm_selected();
        let valid = state.model_options.is_valid();
        let marker = if selected { "› " } else { "  " };
        let label = if valid {
            "Confirm"
        } else {
            "Confirm [unavailable]"
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{marker}{label}"), content_width),
            if selected {
                theme.selection.focused
            } else if valid {
                theme.text.primary
            } else {
                theme.text.subdued
            },
        ));
    }
    if footer_rows > 0 && lines.len() < content_height {
        let controls = if state.model_options.is_valid() {
            if state.model_options.is_confirm_selected() {
                "Enter confirm · Ctrl+Enter apply · Esc cancel"
            } else {
                "Enter configure · Ctrl+Enter apply · Esc cancel"
            }
        } else {
            "Enter configure · Apply unavailable · Esc cancel"
        };
        lines.push(Line::styled(
            truncate_to_width(controls, content_width),
            theme.text.subdued,
        ));
    }
    render_overlay_box(
        frame,
        state,
        SelectionSurface::ModelOptions,
        area,
        lines,
        " Model Options ",
        theme,
    );
}

fn model_option_choice_line(
    row: ModelOptionChoiceRow,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = if row.selected { "› " } else { "  " };
    let current = if row.current { " [current]" } else { "" };
    let unavailable = if row.available { "" } else { " [unavailable]" };
    let description = row
        .description
        .map_or_else(String::new, |description| format!(" · {description}"));
    Line::styled(
        truncate_to_width(
            &format!("{marker}{}{current}{unavailable}{description}", row.label),
            width,
        ),
        if row.selected {
            theme.selection.focused
        } else if row.available {
            theme.text.primary
        } else {
            theme.text.subdued
        },
    )
}

/// The slice of a list to draw when the box is shorter than the list: enough
/// rows to fill it, ending on the focused one, so a selection moving past the
/// bottom scrolls the list rather than leaving the frame.
fn visible_window<T>(rows: Vec<T>, selected: usize, capacity: usize) -> impl Iterator<Item = T> {
    rows.into_iter()
        .skip(window_start(selected, capacity))
        .take(capacity)
}

/// The first row a window of `capacity` rows shows while `selected` has to be
/// in it, which a surface a reader can point at needs as well as the drawing
/// does: it is what says which row the pointer landed on.
fn window_start(selected: usize, capacity: usize) -> usize {
    selected.saturating_add(1).saturating_sub(capacity)
}

/// The line a picker heads its rows with: what the reader has typed to narrow
/// them, spelled the same way in every picker so one is read as readily as the
/// next.
fn picker_search_line(query: &str, width: usize) -> String {
    truncate_to_width(&format!("Search: {query}"), width)
}

fn record_overlay_selection(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    surface: SelectionSurface,
    bordered: bool,
) {
    let mut content = if bordered {
        area.inner(ratatui::layout::Margin::new(1, 1))
    } else {
        area
    };
    if surface == SelectionSurface::Settings && content.height >= 4 {
        content.y += 1;
        content.height -= 1;
    }
    state.selection_overlay_area.set(Some(area));
    let mut surfaces = state.selection_frames.borrow_mut();
    surfaces.clear();
    surfaces.push(SelectionFrame::painted(
        surface,
        frame.buffer_mut(),
        content,
    ));
}

fn render_overlay_box(
    frame: &mut Frame<'_>,
    state: &TuiState,
    surface: SelectionSurface,
    area: Rect,
    lines: Vec<Line<'static>>,
    title: &str,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_owned())
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
    record_overlay_selection(frame, state, area, surface, true);
}

/// The row standing for a Provider's condition, which choosing re-checks.
fn model_picker_retry_row<'a>(
    text: &str,
    selected: bool,
    content_width: u16,
    theme: &Theme,
) -> Line<'a> {
    Line::styled(
        truncate_to_width(
            &format!("{}{text}", if selected { "› " } else { "  " }),
            usize::from(content_width),
        ),
        if selected {
            theme.selection.focused
        } else {
            theme.feedback.error
        },
    )
}

fn model_picker_row_text(
    model: &ModelDescriptor,
    selected: bool,
    current: bool,
    width: usize,
) -> String {
    let marker = if selected { "› " } else { "  " };
    let native = (model.display_name != model.id.as_str()).then_some(model.id.as_str());
    let mut states = Vec::new();
    let mut compact_states = Vec::new();
    if current {
        states.push("current");
        compact_states.push("C");
    }
    if model.is_default {
        states.push("default");
        compact_states.push("D");
    }
    if model.availability == ModelAvailability::Unavailable {
        states.push("unavailable");
        compact_states.push("U");
    }
    let marker_width = marker.width();
    let available = width.saturating_sub(marker_width);
    let field_count = 1 + usize::from(native.is_some()) + usize::from(!states.is_empty());
    let wide_separator = " · ";
    let compact_separator = " ";
    let full_state = (!states.is_empty()).then(|| format!("[{}]", states.join(", ")));
    let compact_state =
        (!compact_states.is_empty()).then(|| format!("[{}]", compact_states.join(",")));
    let minimum_text_width = 4 * (1 + usize::from(native.is_some()));
    let full_fixed = full_state.as_ref().map_or(0, |state| state.width())
        + wide_separator.width() * field_count.saturating_sub(1);
    let (separator, state) = if available >= full_fixed.saturating_add(minimum_text_width) {
        (wide_separator, full_state)
    } else {
        (compact_separator, compact_state)
    };
    let fixed = state.as_ref().map_or(0, |state| state.width())
        + separator.width() * field_count.saturating_sub(1);
    let flexible = available.saturating_sub(fixed);
    let (display_width, native_width) = native.map_or((flexible, 0), |native| {
        let native_width = native.width().min((flexible / 2).max(1));
        (flexible.saturating_sub(native_width), native_width)
    });
    let mut fields = vec![truncate_to_width(&model.display_name, display_width)];
    if let Some(native) = native {
        fields.push(truncate_to_width(native, native_width));
    }
    if let Some(state) = state {
        fields.push(state);
    }
    truncate_to_width(&format!("{marker}{}", fields.join(separator)), width)
}

fn session_picker_row_text(
    row: SessionPickerRow<'_>,
    width: usize,
    now: u64,
    state: &TuiState,
) -> (String, Option<(usize, usize)>) {
    let marker = if row.selected { "› " } else { "  " };
    if row.confirming_delete {
        return (
            truncate_to_width(&format!("{marker}Press Ctrl+D again to confirm"), width),
            None,
        );
    }
    let compact = width < usize::from(NARROW_TERMINAL_WIDTH);
    let status = if row.unreadable {
        Some(if compact {
            "U".to_owned()
        } else {
            "[unreadable]".to_owned()
        })
    } else {
        match (compact, row.current, row.active) {
            (_, false, false) => None,
            (true, true, true) => Some("CA".to_owned()),
            (true, true, false) => Some("C".to_owned()),
            (true, false, true) => Some("A".to_owned()),
            (false, true, true) => Some("[current, active]".to_owned()),
            (false, true, false) => Some("[current]".to_owned()),
            (false, false, true) => Some("[active]".to_owned()),
        }
    };
    let age = if compact {
        relative_update_time_compact(row.updated_at, now)
    } else {
        relative_update_time(row.updated_at, now)
    };
    let separator = if compact { " " } else { " · " };
    let remote_tag = row.remote.map(|remote| format!("[{remote}]"));
    let mut metadata = remote_tag
        .iter()
        .cloned()
        .into_iter()
        .chain(status)
        .chain([age])
        .collect::<Vec<_>>();
    // The Emoji's own columns, and the space parting it from the Title, come
    // out of what the Title has to spend. A Session with no Emoji holds no cell
    // open in front of its Title and spends the lot.
    let emoji = row
        .emoji
        .map(|emoji| format!("{emoji} "))
        .unwrap_or_default();
    let marker_width = marker.width();
    let available = width
        .saturating_sub(marker_width)
        .saturating_sub(emoji.width());
    if row.pending_questionnaires > 0 {
        metadata.push(format!("{} pending questions", row.pending_questionnaires));
    }
    if row.pending_approvals > 0 {
        metadata.push(format!("{} pending Approvals", row.pending_approvals));
    }
    let fixed_metadata_width = metadata.join(separator).width();
    if let Some(workspace) = row.workspace {
        let minimum_title_width = usize::from(available > 0);
        let path_budget = available
            .saturating_sub(minimum_title_width)
            .saturating_sub(separator.width())
            .saturating_sub(fixed_metadata_width)
            .saturating_sub(separator.width());
        if path_budget > 0 {
            metadata.push(truncate_from_left_to_width(
                &state.workspace_label(row.origin, workspace),
                path_budget,
            ));
        }
    }
    let metadata = metadata.join(separator);
    let title_width = available
        .saturating_sub(metadata.width())
        .saturating_sub(separator.width());
    let title = truncate_to_width(row.title, title_width);
    let tag_start = remote_tag
        .as_ref()
        .map(|_| marker.len() + emoji.len() + title.len() + separator.len());
    let content = truncate_to_width(
        &format!("{marker}{emoji}{title}{separator}{metadata}"),
        width,
    );
    let tag_range = tag_start.and_then(|start| {
        let end = start + remote_tag.as_ref()?.len();
        (content.get(start..end) == remote_tag.as_deref()).then_some((start, end))
    });
    (content, tag_range)
}

fn session_picker_row_line(
    row: SessionPickerRow<'_>,
    width: usize,
    now: u64,
    theme: &Theme,
    state: &TuiState,
) -> Line<'static> {
    let (content, remote_tag) = session_picker_row_text(row, width, now, state);
    let row_style = if row.selected {
        theme.selection.focused
    } else if row.unreadable {
        theme.text.subdued
    } else {
        theme.text.primary
    };
    let Some((start, end)) = remote_tag else {
        return Line::styled(content, row_style);
    };
    let tag_style = row_style.patch(theme.text.subdued);
    Line::from(vec![
        Span::styled(content[..start].to_owned(), row_style),
        Span::styled(content[start..end].to_owned(), tag_style),
        Span::styled(content[end..].to_owned(), row_style),
    ])
}

fn truncate_from_left_to_width(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let suffix_width = width - 1;
    let mut suffix = String::new();
    let mut used = 0_usize;
    for character in value.chars().rev() {
        let character_width = character.width().unwrap_or(1);
        if used.saturating_add(character_width) > suffix_width {
            break;
        }
        suffix.insert(0, character);
        used += character_width;
    }
    format!("…{suffix}")
}

fn relative_update_time(updated_at: SessionTimestamp, now: u64) -> String {
    let elapsed_seconds = now.saturating_sub(updated_at.0) / 1_000;
    match elapsed_seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h ago", elapsed_seconds / 3_600),
        _ => format!("{}d ago", elapsed_seconds / 86_400),
    }
}

fn relative_update_time_compact(updated_at: SessionTimestamp, now: u64) -> String {
    let elapsed_seconds = now.saturating_sub(updated_at.0) / 1_000;
    match elapsed_seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h", elapsed_seconds / 3_600),
        _ => format!("{}d", elapsed_seconds / 86_400),
    }
}

#[derive(Clone, Copy, Debug)]
struct RenderedComposer {
    area: Rect,
    /// Where the composer's caret sits, and `None` where no composer was
    /// drawn — a Subagent's Session offers nowhere to type, so no caret may
    /// invite it.
    cursor: Option<Position>,
}

/// Where a surface docks over the composer: bordered while the rows above the
/// composer can spare a box, borderless when they are scarce, and centered on
/// the composer's own columns. The completion list and the Subagent Picker
/// dock through the one account, so the two surfaces cannot drift apart.
#[derive(Clone, Copy, Debug)]
struct ComposerDock {
    area: Rect,
    bordered: bool,
}

impl ComposerDock {
    /// Docks `desired_rows` of content over the composer, giving up rows from
    /// the bottom when the frame cannot spare them all.
    fn over(frame: &Frame<'_>, composer_area: Rect, desired_rows: u16) -> Self {
        let width = composer_area.width.clamp(1, 72);
        let room_above = composer_area.y.saturating_sub(frame.area().y);
        let bordered = room_above >= 3;
        let height = if bordered {
            desired_rows.saturating_add(2).min(room_above)
        } else {
            desired_rows.min(room_above.max(1))
        };
        let x = composer_area
            .x
            .saturating_add(composer_area.width.saturating_sub(width) / 2);
        let y = composer_area.y.saturating_sub(height).max(frame.area().y);
        Self {
            area: Rect::new(x, y, width, height),
            bordered,
        }
    }
}

fn render_composer_completion(
    frame: &mut Frame<'_>,
    state: &TuiState,
    composer_area: Rect,
    theme: &Theme,
) {
    let row_count = state.composer_completion.rows().len() as u16;
    let ComposerDock { area, bordered } = ComposerDock::over(frame, composer_area, row_count);
    let row_capacity = area.height.saturating_sub(if bordered { 2 } else { 0 });
    let content_width = area.width.saturating_sub(if bordered { 2 } else { 0 });
    let rows = state
        .composer_completion
        .visible_rows(usize::from(row_capacity))
        .into_iter()
        .map(|(selected, row)| {
            let content = match row {
                CompletionRow::Command(command) => {
                    let slash = command
                        .slash
                        .expect("Command completion only contains slash-enabled commands");
                    truncate_to_width(
                        &format!(
                            "/{}  {} · {}",
                            slash.name, command.title, command.description
                        ),
                        usize::from(content_width),
                    )
                }
                CompletionRow::Insertion(canonical) => {
                    truncate_to_width(canonical, usize::from(content_width))
                }
                CompletionRow::Skill(skill) => {
                    let scope = skill
                        .scope
                        .as_deref()
                        .map_or(String::new(), |scope| format!(" · {scope}"));
                    truncate_to_width(
                        &format!("${}  {}{}", skill.name, skill.description, scope),
                        usize::from(content_width),
                    )
                }
                CompletionRow::StaleSkill(skill) => {
                    let scope = skill
                        .scope
                        .as_deref()
                        .map_or(String::new(), |scope| format!(" · {scope}"));
                    truncate_to_width(
                        &format!("${}  {}{} · stale", skill.name, skill.description, scope),
                        usize::from(content_width),
                    )
                }
                CompletionRow::DisabledSkill(skill) => truncate_to_width(
                    &format!("${}  {} · limit reached", skill.name, skill.description),
                    usize::from(content_width),
                ),
                CompletionRow::Message(message) => {
                    truncate_to_width(message, usize::from(content_width))
                }
            };
            Line::styled(
                content,
                if matches!(row, CompletionRow::DisabledSkill(_)) {
                    theme.action.disabled
                } else if selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                },
            )
        })
        .collect::<Vec<_>>();
    let paragraph = if bordered {
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(state.composer_completion.title())
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        )
    } else {
        Paragraph::new(rows).style(theme.surface.overlay)
    };
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
    record_overlay_selection(frame, state, area, SelectionSurface::Completions, bordered);
}

/// Draws the Subagent Picker docked over the composer, as the completion list
/// docks: bordered while the rows above the composer allow it, borderless when
/// they are scarce. The working Subagents stand as the tree they spawned in,
/// the Spinner ticking on every entry, and each row's place is recorded as it
/// lands so the pointer answers exactly what is on screen.
fn render_subagent_picker(
    frame: &mut Frame<'_>,
    state: &TuiState,
    composer_area: Rect,
    theme: &Theme,
) {
    let Some(snapshot) = state.session.as_ref().map(SessionProjection::snapshot) else {
        return;
    };
    let entries = working_subagents(snapshot);
    if entries.is_empty() {
        return;
    }
    let selected = state.subagent_picker.selected();
    // One footer line naming the keys, kept only while every entry fits
    // beside it: the rows are what the picker is for.
    let desired_rows = (entries.len() as u16).saturating_add(1);
    let ComposerDock { area, bordered } = ComposerDock::over(frame, composer_area, desired_rows);
    let content_height = usize::from(area.height.saturating_sub(if bordered { 2 } else { 0 }));
    let shows_footer = content_height > entries.len();
    let row_capacity = content_height
        .saturating_sub(usize::from(shows_footer))
        .max(1);
    let content_width = area.width.saturating_sub(if bordered { 2 } else { 0 });
    let content_x = area.x.saturating_add(u16::from(bordered));
    let content_y = area.y.saturating_add(u16::from(bordered));
    // The window slides to keep the entry the reader is on in view when the
    // tree outgrows the rows above the composer.
    let selected_index = entries
        .iter()
        .position(|entry| Some(entry.session_id) == selected)
        .unwrap_or(0);
    let start = selected_index
        .saturating_add(1)
        .saturating_sub(row_capacity);
    let mut lines = Vec::with_capacity(content_height);
    for (index, entry) in entries.iter().enumerate().skip(start).take(row_capacity) {
        let guide = if index + 1 == entries.len() {
            "└"
        } else {
            "├"
        };
        let model = entry
            .model
            .map(|model| {
                snapshot
                    .session
                    .agent_selection
                    .as_ref()
                    .or_else(|| {
                        snapshot
                            .turns
                            .last()
                            .and_then(|turn| turn.agent.as_ref())
                            .map(|agent| &agent.selection)
                    })
                    .map_or_else(
                        || model.as_str().to_owned(),
                        |selection| {
                            state.model_picker.selection_summary(
                                &crate::protocol::AgentSelection {
                                    provider: selection.provider.clone(),
                                    model: model.clone(),
                                    options: Vec::new(),
                                },
                                false,
                            )
                        },
                    )
            })
            .map(|model| format!(" · {model}"))
            .unwrap_or_default();
        let mut interventions = Vec::new();
        if entry.pending_questionnaires > 0 {
            interventions.push(format!(
                "{} pending questions",
                entry.pending_questionnaires
            ));
        }
        if entry.pending_approvals > 0 {
            interventions.push(format!("{} pending Approvals", entry.pending_approvals));
        }
        let interventions = if interventions.is_empty() {
            String::new()
        } else {
            format!(" · {}", interventions.join(" · "))
        };
        let content = truncate_to_width(
            &format!(
                "{guide} {} {}{model}{interventions}: {}",
                spinner::frame(state.spinner_frame / 3),
                entry.name,
                entry.description
            ),
            usize::from(content_width),
        );
        state.subagent_picker.record_row(
            content_y.saturating_add(lines.len() as u16),
            content_x..content_x.saturating_add(content_width),
            entry.session_id,
        );
        lines.push(Line::styled(
            content,
            if Some(entry.session_id) == selected {
                theme.selection.focused
            } else {
                theme.text.primary
            },
        ));
    }
    if shows_footer {
        // The stop key stands in the footer only where the Provider offers
        // the stop, so the picker never names a key that would do nothing.
        let footer = if state.subagent_stop_offered() {
            "Enter open · x stop · Esc close"
        } else {
            "Enter open · Esc close"
        };
        lines.push(Line::styled(
            truncate_to_width(footer, usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    let paragraph = if bordered {
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Subagents ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        )
    } else {
        Paragraph::new(lines).style(theme.surface.overlay)
    };
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
    record_overlay_selection(frame, state, area, SelectionSurface::Subagents, bordered);
}

/// Draws the Sidebar down the left of the frame and reports what is left for
/// the main view — the whole frame when the reader has the Sidebar hidden, or
/// when the terminal cannot spare its columns.
fn render_sidebar(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme, truecolor: bool) -> Rect {
    let frame_area = frame.area();
    if !state.sidebar.is_revealed() {
        return frame_area;
    }
    let Some(width) = sidebar::width_beside(state.sidebar.chosen_width(), frame_area.width) else {
        return frame_area;
    };
    state.sidebar.record_drawn(
        width,
        sidebar::width_limit(frame_area.width).unwrap_or(width),
    );
    let [column, main] =
        Layout::horizontal([Constraint::Length(width), Constraint::Min(1)]).areas(frame_area);
    // Holding the edge is pointer feedback of its own. At rest the rule keeps
    // the same key-ownership paint it had before it became draggable.
    let block = Block::default()
        .style(theme.surface.elevated)
        .borders(Borders::RIGHT)
        .border_style(if state.sidebar.edge_is_held() {
            theme.accent.primary
        } else if state.sidebar_owns_input() {
            theme.border.default
        } else {
            theme.border.subdued
        });
    let inside = block.inner(column);
    let content = horizontally_inset(inside, 1);
    frame.render_widget(block, column);
    let (lines, rows, rails) = sidebar_lines(state, content, theme);
    frame.render_widget(Paragraph::new(lines).style(theme.surface.elevated), content);
    paint_standing_rails(frame, &rails, inside.x, theme, state, truecolor);
    state
        .selection_frames
        .borrow_mut()
        .push(SelectionFrame::painted(
            SelectionSurface::Sidebar,
            frame.buffer_mut(),
            content,
        ));
    // The columns inside the rule rather than the content's own, so the
    // padding a row is inset by presses the row it insets. The spans the body
    // narrows to a run of one line are measured from the same edges, in
    // `sidebar_lines`.
    let edge = column.right().saturating_sub(2)..column.right().saturating_add(1);
    state
        .sidebar
        .record_geometry(inside.x..inside.right(), edge, rows);
    main
}

/// Paints each active row's Standing Rail over the column of padding at its
/// left. The glyph and feedback foreground sit over the row's existing
/// background, so focused rows keep their block while still saying what their
/// work is doing. Where the feedback colour is the focus block's own, the
/// glyph takes the block's text foreground instead, so the Rail is never
/// painted invisibly. Settled rows contribute no span here.
///
fn paint_standing_rails(
    frame: &mut Frame<'_>,
    rails: &[StandingRail],
    column: u16,
    theme: &Theme,
    state: &TuiState,
    truecolor: bool,
) {
    let mut origins = state.rail_origins.borrow_mut();
    origins.retain(|reference, _| {
        rails.iter().any(|rail| {
            rail.working
                .as_ref()
                .is_some_and(|(working, _)| working == reference)
        })
    });
    let buffer = frame.buffer_mut();
    for rail in rails {
        let feedback = rail.standing.presentation().feedback.style(theme);
        let style = if rail.focused && feedback.fg == theme.selection.focused.bg {
            Style::default().fg(theme.selection.focused.fg.unwrap_or(Color::Reset))
        } else {
            feedback
        };
        let animation_frame = rail.working.as_ref().map(|(reference, since)| {
            let (previous_since, origin) = origins
                .entry(reference.clone())
                .or_insert((*since, state.spinner_frame));
            if previous_since != since {
                *previous_since = *since;
                *origin = state.spinner_frame;
            }
            state.spinner_frame.wrapping_sub(*origin)
        });
        for row in rail.rows.clone() {
            if let Some(cell) = buffer.cell_mut(Position::new(column, row)) {
                if rail.focused {
                    cell.set_style(theme.selection.focused);
                }
                cell.set_symbol("▎");
                cell.set_style(style);
                if let Some(animation_frame) = animation_frame {
                    cell.set_style(shimmer::fade_style(
                        cell.style(),
                        animation_frame,
                        truecolor,
                    ));
                }
            }
        }
    }
}

struct StandingRail {
    working: Option<(crate::protocol::SessionReference, SessionTimestamp)>,
    rows: std::ops::Range<u16>,
    standing: SessionStanding,
    focused: bool,
}

#[derive(Clone, Copy)]
struct StandingPresentation {
    feedback: StandingFeedback,
    slot: StandingSlot,
}

#[derive(Clone, Copy)]
enum StandingFeedback {
    Warning,
    Info,
    Error,
    Success,
}

impl StandingFeedback {
    const fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Warning => theme.feedback.warning,
            Self::Info => theme.feedback.info,
            Self::Error => theme.feedback.error,
            Self::Success => theme.feedback.success,
        }
    }
}

#[derive(Clone, Copy)]
enum StandingSlot {
    WorkingDuration,
    Word(&'static str),
}

impl SessionStanding {
    const fn presentation(self) -> StandingPresentation {
        match self {
            Self::NeedsIntervention => StandingPresentation {
                feedback: StandingFeedback::Warning,
                slot: StandingSlot::Word("Needs Intervention"),
            },
            Self::Working => StandingPresentation {
                feedback: StandingFeedback::Info,
                slot: StandingSlot::WorkingDuration,
            },
            Self::Failed => StandingPresentation {
                feedback: StandingFeedback::Error,
                slot: StandingSlot::Word("Failed"),
            },
            Self::Done => StandingPresentation {
                feedback: StandingFeedback::Success,
                slot: StandingSlot::Word("Done"),
            },
        }
    }
}

/// Draws the context menu a reader opened on a Sidebar row, anchored at the
/// cell they pointed at and pulled back inside the frame where the box would
/// otherwise run off it. Drawn after the main view, because a menu stands over
/// whatever it was opened in front of.
fn render_sidebar_menu(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let Some(menu) = state.sidebar.menu() else {
        return;
    };
    let widest = menu
        .items
        .iter()
        .map(|item| item.label.width())
        .max()
        .unwrap_or_default();
    // The label, a column of padding either side of it, and the box's own two
    // borders; likewise two borders around the items down the box.
    let width = u16::try_from(widest.saturating_add(4)).unwrap_or(u16::MAX);
    let height = u16::try_from(menu.items.len().saturating_add(2)).unwrap_or(u16::MAX);
    let frame_area = frame.area();
    if frame_area.width < width || frame_area.height < height {
        return;
    }
    let area = Rect {
        x: menu.anchor.x.min(frame_area.right().saturating_sub(width)),
        y: menu
            .anchor
            .y
            .min(frame_area.bottom().saturating_sub(height)),
        width,
        height,
    };
    let lines = menu
        .items
        .iter()
        .map(|item| {
            Line::styled(
                pad_to_width(
                    &format!(" {}", item.label),
                    usize::from(width.saturating_sub(2)),
                ),
                if item.selected {
                    theme.selection.focused
                } else if item.destructive {
                    theme.feedback.error
                } else {
                    theme.text.primary
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
    state.sidebar.record_menu_geometry(SidebarMenuGeometry {
        columns: area.x + 1..area.right().saturating_sub(1),
        top: area.y + 1,
        count: height.saturating_sub(2),
    });
    record_overlay_selection(frame, state, area, SelectionSurface::SidebarMenu, true);
}

/// The Sidebar's whole body: the search box, then what the server last
/// refused, then the Workspace selector, then the active Sessions, then the
/// divider and the settled ones — as many of them as the column is tall — or
/// the one line that stands in for a list there is nothing to draw.
///
/// Reports the geometry the body came out at alongside the lines themselves,
/// because only the pass that lays the rows out knows which screen rows each
/// one took: rows are not all the same height, and the window decides how many
/// of them there are.
fn sidebar_lines(
    state: &TuiState,
    content: Rect,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<SidebarSpan>, Vec<StandingRail>) {
    let width = usize::from(content.width);
    let driving = state.sidebar_owns_input();
    let mut lines = vec![sidebar_search_line(state.sidebar.query(), width, theme)];
    let error = state.sidebar.error();
    if let Some(error) = error {
        lines.push(Line::styled(
            truncate_to_width(error, width),
            theme.feedback.error,
        ));
    }
    // The selector stands between the search box and the list it governs, and
    // is a row the reader can press, so it is the first span this frame
    // records — along with the affordance sharing its line, which answers the
    // columns at the right of it.
    let selector_row = content
        .y
        .saturating_add(u16::try_from(lines.len()).unwrap_or_default());
    lines.push(sidebar_selector_line(
        &state
            .sidebar
            .selector(&|path| state.workspace_name(&state.outlook, path)),
        width,
        driving,
        theme,
    ));
    let selector_rows = selector_row..selector_row.saturating_add(1);
    // Where the label gives way to the affordance, and the Sidebar's own edges
    // either side of the pair: the padding a row is inset by presses the row it
    // insets, so the spans run out to the rule rather than to the content.
    let split = content
        .right()
        .saturating_sub(u16::try_from(ADD_WORKSPACE.width()).unwrap_or_default());
    let mut rows = vec![
        SidebarSpan {
            rows: selector_rows.clone(),
            columns: Some(content.x.saturating_sub(1)..split),
            target: SidebarTarget::Selector,
        },
        SidebarSpan {
            rows: selector_rows,
            columns: Some(split..content.right().saturating_add(1)),
            target: SidebarTarget::AddWorkspace,
        },
    ];
    // A path entry stands in place of the list, so nothing below the selector's
    // line is pressable while it stands — but the line itself goes on answering
    // the pointer, because a reader who opened the entry by pointing has to be
    // able to be done with it the same way.
    if let Some(entry) = state.sidebar.workspace_entry() {
        lines.extend(sidebar_workspace_entry_lines(&entry, width, theme));
        return (lines, rows, Vec::new());
    }
    let capacity = usize::from(content.height).saturating_sub(lines.len());
    let entries =
        state
            .sidebar
            .visible_entries(capacity, state.open_session_reference(), &|path| {
                state.workspace_name(&state.outlook, path)
            });
    if entries.is_empty() {
        lines.extend(
            sidebar_empty_reading(&state.sidebar)
                .map(|reading| Line::styled(reading, theme.text.subdued)),
        );
        return (lines, rows, Vec::new());
    }
    let now = SessionTimestamp::now().0;
    let mut top = content
        .y
        .saturating_add(u16::try_from(lines.len()).unwrap_or_default());
    let mut rails = Vec::new();
    for entry in entries {
        let working = match &entry {
            SidebarEntry::Row(row) if row.standing == Some(SessionStanding::Working) => {
                match row.shelf {
                    SidebarShelf::Active {
                        working_since: Some(since),
                        ..
                    } => Some((row.reference.clone(), since)),
                    _ => None,
                }
            }
            _ => None,
        };
        let standing = entry.standing();
        let focused = driving && entry.is_focused();
        let target = entry.target();
        let drawn = sidebar_entry_lines(entry, width, now, driving, theme, state);
        let bottom = top.saturating_add(u16::try_from(drawn.len()).unwrap_or_default());
        if let Some(standing) = standing {
            rails.push(StandingRail {
                working,
                rows: top..bottom,
                standing,
                focused,
            });
        }
        if let Some(target) = target {
            rows.push(SidebarSpan {
                rows: top..bottom,
                columns: None,
                target,
            });
        }
        top = bottom;
        lines.extend(drawn);
    }
    (lines, rows, rails)
}

/// The Sidebar's search box, standing at the top of the column whether or not
/// the reader is searching, so the way to narrow a long list is always in
/// view. It is labelled the way the session picker's search line is, because
/// it is the same act on the same body of work — but the query itself is
/// drawn plainly rather than subdued, because unlike the picker's the Sidebar
/// stands open while the reader works elsewhere, and what it is narrowed to
/// has to be legible at a glance.
fn sidebar_search_line(query: &str, width: usize, theme: &Theme) -> Line<'static> {
    const LABEL: &str = "Search: ";
    Line::from(vec![
        Span::styled(LABEL, theme.text.subdued),
        Span::styled(
            tail_to_width(query, width.saturating_sub(LABEL.width())),
            theme.text.primary,
        ),
    ])
}

/// The last `width` columns of `text`, and the whole of it where it fits. This
/// is what a line being typed into shows, rather than the leading columns
/// every other line shows: a reader watches the end of what they are writing,
/// and the Sidebar's box is narrow enough that a query of any length would
/// otherwise run off where they cannot see it.
fn tail_to_width(text: &str, width: usize) -> String {
    let mut tail = String::new();
    let mut taken = 0;
    for character in text.chars().rev() {
        taken += UnicodeWidthChar::width(character).unwrap_or(0);
        if taken > width {
            break;
        }
        tail.insert(0, character);
    }
    tail
}

/// What the Sidebar says in place of a list there is nothing to draw: that it
/// is still asking the server, that the reader's query matched nothing, or
/// that there is no work yet. An error already drawn is its own account of an
/// empty column, and it comes first: a listing that failed has told the reader
/// nothing about whether their query matches anything.
fn sidebar_empty_reading(sidebar: &Sidebar) -> Option<&'static str> {
    if sidebar.is_loading() {
        return Some("Loading Sessions…");
    }
    if sidebar.error().is_some() {
        return None;
    }
    Some(match (sidebar.query().is_empty(), sidebar.is_narrowed()) {
        (false, _) => "No Sessions match",
        // A reader who narrowed to one Workspace is told that Workspace is
        // empty rather than that they have no work, which would be a lie about
        // the rest of it.
        (true, true) => "No Sessions in this Workspace",
        (true, false) => "No Sessions yet",
    })
}

/// One entry of the Sidebar's body: a Session, recovering Remote, affordance,
/// or the rule between the two shelves.
fn sidebar_entry_lines(
    entry: SidebarEntry<'_>,
    width: usize,
    now: u64,
    driving: bool,
    theme: &Theme,
    state: &TuiState,
) -> Vec<Line<'static>> {
    match entry {
        SidebarEntry::Spacer => vec![Line::default()],
        SidebarEntry::Divider => vec![sidebar_divider_line(width, theme)],
        SidebarEntry::Unreachable(remote) => {
            vec![sidebar_unreachable_line(remote, width, driving, theme)]
        }
        SidebarEntry::ShowMore(more) => vec![sidebar_show_more_line(more, width, driving, theme)],
        SidebarEntry::Scope(scope) => vec![sidebar_scope_line(&scope, width, driving, theme)],
        SidebarEntry::Row(row) => match row.shelf {
            SidebarShelf::Active {
                workspace,
                updated_at,
                working_since,
                ..
            } => sidebar_active_row_lines(
                row,
                workspace
                    .map(|path| state.workspace_name(&row.reference.origin, path))
                    .unwrap_or_default(),
                sidebar_active_slot(row.standing, working_since, updated_at, now),
                width,
                driving,
                theme,
            )
            .to_vec(),
            SidebarShelf::Settled { ended_at } => {
                vec![sidebar_settled_row_line(
                    row, ended_at, width, now, driving, theme,
                )]
            }
        },
    }
}

fn sidebar_unreachable_line(
    remote: SidebarUnreachable<'_>,
    width: usize,
    driving: bool,
    theme: &Theme,
) -> Line<'static> {
    sidebar_plain_line(
        &format!("{} [unreachable]", remote.name),
        width,
        sidebar_focus_style(remote.focused, driving, theme),
        theme.text.subdued,
    )
}

/// The Workspace selector: what the Sidebar is narrowed to, with the affordance
/// that opens its entries — the same one the settings panel opens a row's
/// choices with, because it is the same gesture on the same kind of list — and,
/// at the right of the line, the affordance that opens a path entry for a
/// Workspace the Sidebar has never listed.
///
/// The two share the line and are highlighted apart, each within its own
/// columns, because the reader is on one or the other and the frame has to say
/// which.
fn sidebar_selector_line(
    selector: &SidebarSelectorView,
    width: usize,
    driving: bool,
    theme: &Theme,
) -> Line<'static> {
    let affordance = if selector.open { "▾ " } else { "▸ " };
    Line::from(vec![
        sidebar_plain_span(
            &format!("{affordance}{}", selector.label),
            width.saturating_sub(ADD_WORKSPACE.width()),
            sidebar_focus_style(selector.focused, driving, theme),
            theme.text.subdued,
        ),
        sidebar_plain_span(
            ADD_WORKSPACE,
            ADD_WORKSPACE.width(),
            sidebar_focus_style(selector.adding, driving, theme),
            theme.text.subdued,
        ),
    ])
}

/// The path entry the add-Workspace affordance opens, standing in place of the
/// list.
///
/// It is drawn as the search box is — a label and what the reader has typed,
/// held to its end so the part of a long path that says which directory it is
/// stays in view — with what their last path was refused for beneath it.
fn sidebar_workspace_entry_lines(
    entry: &SidebarWorkspaceEntryView<'_>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    const LABEL: &str = "Workspace: ";
    let mut lines = vec![Line::from(vec![
        Span::styled(LABEL, theme.text.subdued),
        Span::styled(
            tail_to_width(entry.path, width.saturating_sub(LABEL.width())),
            theme.text.primary,
        ),
    ])];
    lines.extend(
        entry.rejection.map(|rejection| {
            Line::styled(truncate_to_width(rejection, width), theme.feedback.error)
        }),
    );
    lines
}

/// One Workspace the open selector offers, stepped in past the affordance that
/// opened it. The scope in force is accented, the way the Session the reader
/// has open is: both say "you are already here".
fn sidebar_scope_line(
    scope: &SidebarScopeEntry,
    width: usize,
    driving: bool,
    theme: &Theme,
) -> Line<'static> {
    sidebar_plain_line(
        &format!("  {}", scope.label),
        width,
        sidebar_focus_style(scope.focused, driving, theme),
        if scope.chosen {
            theme.accent.primary
        } else {
            theme.text.primary
        },
    )
}

/// The rule closing the active list and opening the settled shelf, named so a
/// reader knows what the slim rows below it are.
fn sidebar_divider_line(width: usize, theme: &Theme) -> Line<'static> {
    const LABEL: &str = "Settled";
    let rule = "\u{2500}".repeat(width.saturating_sub(LABEL.width() + 1));
    Line::styled(
        truncate_to_width(&format!("{LABEL} {rule}"), width),
        theme.text.subdued,
    )
}

/// The row closing a settled shelf with more under it, saying how much one ask
/// would bring into view. It is drawn as a row rather than as a rule, because
/// it is one the reader can stand on and act on.
fn sidebar_show_more_line(
    more: SidebarShowMore,
    width: usize,
    driving: bool,
    theme: &Theme,
) -> Line<'static> {
    sidebar_plain_line(
        &format!("Show {} more", more.count),
        width,
        sidebar_focus_style(more.focused, driving, theme),
        theme.text.subdued,
    )
}

/// One run of a Sidebar line carrying nothing but a label: padded out to the
/// columns it is given, so a row the reader is on reads as one block rather
/// than as lit text, and cut rather than wrapped where the label is longer than
/// those columns.
fn sidebar_plain_span(
    label: &str,
    width: usize,
    selected: Option<Style>,
    unselected: Style,
) -> Span<'static> {
    Span::styled(
        pad_to_width(&truncate_to_width(label, width), width),
        selected.unwrap_or(unselected),
    )
}

/// A whole line of the Sidebar drawn that way, the column wide. Every row with
/// no right slot to lay out and nothing sharing its line is drawn here; the
/// selector's line, which is shared, lays out two spans of its own.
fn sidebar_plain_line(
    label: &str,
    width: usize,
    selected: Option<Style>,
    unselected: Style,
) -> Line<'static> {
    Line::from(sidebar_plain_span(label, width, selected, unselected))
}

/// One active Session, as three lines: where the work lives beside what its
/// right slot reads, then what the work is, then its current checkout.
fn sidebar_active_row_lines(
    row: SidebarRow<'_>,
    workspace: String,
    slot: String,
    width: usize,
    driving: bool,
    theme: &Theme,
) -> [Line<'static>; sidebar::ACTIVE_ROW_LINES] {
    let highlight = sidebar_row_style(row, driving, theme);
    let label_style = highlight.unwrap_or(theme.text.subdued);
    [
        sidebar_slotted_line(&workspace, label_style, &slot, width, highlight, theme),
        sidebar_title_line(row, width, highlight, theme),
        sidebar_plain_line(
            &sidebar_checkout_label(row.shelf, width),
            width,
            highlight,
            theme.text.subdued,
        ),
    ]
}

fn sidebar_checkout_label(shelf: SidebarShelf<'_>, width: usize) -> String {
    let SidebarShelf::Active {
        checkout_state: Some(reading),
        ..
    } = shelf
    else {
        return String::new();
    };
    CheckoutStateLabel::read(reading)
        .map(|label| label.truncated(width))
        .unwrap_or_default()
}

/// A Worktree's Checkout State as every surface presenting one spells it: the
/// branch it stands on, the commit a detached head stands at, or the plain
/// fact that the reading could not be taken. The Sidebar, the Landing, the
/// Worktree Selector, and a Session's header all read one Checkout State, so
/// they say it in one voice.
struct CheckoutStateLabel {
    state: String,
    /// Kept apart from the state so a long branch name is what truncation
    /// takes, leaving a linked Worktree's own label standing.
    suffix: &'static str,
}

/// How fully a surface spells a Checkout State out.
///
/// The Sidebar, the Landing, and a Session header present the Worktree their
/// reader is already standing in, where the branch is orientation and the rest
/// is noise. The Worktree Selector presents Worktrees the reader is choosing
/// between, where a branch with nothing committed to it yet and a head that is
/// on no branch at all are exactly what tells two rows apart, so it says both
/// in words and gives a detached commit a column more to be distinct by.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CheckoutStateForm {
    Standing,
    Choosing,
}

impl CheckoutStateLabel {
    /// Nothing at all where the Worktree's source control was never detected:
    /// a directory outside source control has no Checkout State to present.
    fn read(reading: &crate::protocol::CheckoutSummary) -> Option<Self> {
        Self::read_in(reading, CheckoutStateForm::Standing)
    }

    fn read_in(
        reading: &crate::protocol::CheckoutSummary,
        form: CheckoutStateForm,
    ) -> Option<Self> {
        use crate::protocol::{CheckoutKind, CheckoutRevision, SourceControlAvailability};
        let unavailable = || Self {
            state: "[unavailable]".to_owned(),
            suffix: "",
        };
        Some(match &reading.availability {
            SourceControlAvailability::NotDetected => return None,
            SourceControlAvailability::Unavailable { .. } => unavailable(),
            SourceControlAvailability::Available => match &reading.revision {
                // The main working copy needs no label of its own.
                Some(CheckoutRevision::Branch { name, commit }) => Self {
                    state: match form {
                        CheckoutStateForm::Choosing if commit.is_none() => {
                            format!("{name} (unborn)")
                        }
                        _ => name.clone(),
                    },
                    suffix: match reading.association.kind {
                        CheckoutKind::Main => "",
                        CheckoutKind::Linked => " (worktree)",
                    },
                },
                Some(CheckoutRevision::Detached { commit }) => Self {
                    state: match form {
                        CheckoutStateForm::Standing => commit.chars().take(7).collect(),
                        CheckoutStateForm::Choosing => {
                            format!("detached {}", commit.chars().take(8).collect::<String>())
                        }
                    },
                    suffix: "",
                },
                None => unavailable(),
            },
        })
    }

    fn text(&self) -> String {
        format!("{}{}", self.state, self.suffix)
    }

    fn truncated(&self, width: usize) -> String {
        format!(
            "{}{}",
            truncate_to_width(&self.state, width.saturating_sub(self.suffix.width())),
            self.suffix
        )
    }
}

fn icon_label(show_icons: bool, icon: char, label: &str) -> String {
    if show_icons {
        format!("{icon} {label}")
    } else {
        label.to_owned()
    }
}

fn checkout_state_context(
    reading: &crate::protocol::CheckoutSummary,
    show_icons: bool,
) -> Option<String> {
    use crate::protocol::{CheckoutKind, CheckoutRevision, SourceControlAvailability};

    let label = CheckoutStateLabel::read(reading)?;
    if !show_icons || reading.availability != SourceControlAvailability::Available {
        return Some(label.text());
    }
    let icon = match &reading.revision {
        Some(CheckoutRevision::Branch { .. }) => match reading.association.kind {
            CheckoutKind::Main => NF_COD_GIT_BRANCH,
            CheckoutKind::Linked => NF_COD_WORKTREE,
        },
        Some(CheckoutRevision::Detached { .. }) => NF_COD_GIT_COMMIT,
        None => return Some(label.text()),
    };
    Some(icon_label(true, icon, &label.state))
}

/// One Session set aside, as the single slim line the settled shelf gives it:
/// what the work was, and how long ago it ended.
fn sidebar_settled_row_line(
    row: SidebarRow<'_>,
    ended_at: SessionTimestamp,
    width: usize,
    now: u64,
    driving: bool,
    theme: &Theme,
) -> Line<'static> {
    let highlight = sidebar_row_style(row, driving, theme);
    let slot = relative_update_time_compact(ended_at, now);
    sidebar_slotted_title_line(row, &slot, width, highlight, theme)
}

/// What an active row's right slot reads. Working comes first, because live
/// work is what a reader scanning the column is looking for; under it stands
/// the compact time since the Session last moved, which is what a row says
/// when there is nothing louder to say.
///
/// Viewed suppresses settled outcomes already seen by any Client. Approval and
/// Input remain tied to the reserved Needs Intervention variant under issue #168
/// (<https://github.com/jake-tucker/suru/issues/168>).
fn sidebar_active_slot(
    standing: Option<SessionStanding>,
    working_since: Option<SessionTimestamp>,
    updated_at: SessionTimestamp,
    now: u64,
) -> String {
    match standing
        .map(SessionStanding::presentation)
        .map(|reading| reading.slot)
    {
        Some(StandingSlot::WorkingDuration) => working_since.map_or_else(
            || relative_update_time_compact(updated_at, now),
            |since| format!("Working {}", working_duration(since, now)),
        ),
        Some(StandingSlot::Word(word)) => word.to_owned(),
        None => relative_update_time_compact(updated_at, now),
    }
}

/// How long work has been running, read the way t3 reads it: seconds until
/// there is a minute to say, then minutes, then hours and the minutes past
/// them. Unlike the compact time beside it this is a duration rather than an
/// age, so it is granular enough to be seen moving.
fn working_duration(since: SessionTimestamp, now: u64) -> String {
    let seconds = now.saturating_sub(since.0) / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

/// A Sidebar line with a label down its left and its slot's reading in the
/// right, the two held apart by the whole of what the column has left. Both
/// shelves lay their right slot out this way, so both lay it out here: an
/// active row's Workspace beside what its work is doing, a settled row's Title
/// beside when its work ended.
fn sidebar_slotted_line(
    label: &str,
    label_style: Style,
    slot: &str,
    width: usize,
    selected: Option<Style>,
    theme: &Theme,
) -> Line<'static> {
    let label = truncate_to_width(label, width.saturating_sub(slot.width() + 1));
    let gap = " ".repeat(width.saturating_sub(label.width() + slot.width()));
    Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(gap, selected.unwrap_or_default()),
        Span::styled(slot.to_owned(), selected.unwrap_or(theme.text.subdued)),
    ])
}

/// How a Sidebar entry the keys are on is drawn, and `None` for every other
/// entry — including that same entry once the keys have gone.
///
/// Row focus is drawn whole, so it reads as one block rather than as lines
/// that happen to be lit, and it is drawn only while the Sidebar is the
/// surface the keys reach. There is no dim second state: a mark left standing
/// after the reader went back to writing would be the column claiming
/// something Enter no longer means.
fn sidebar_focus_style(focused: bool, driving: bool, theme: &Theme) -> Option<Style> {
    (focused && driving).then_some(theme.selection.focused)
}

/// How the selected row of a surface that keeps a selection is drawn, and
/// `None` for every other row. It is drawn whole, so it reads as one block
/// rather than as lit text, and it keeps a dimmed highlight while the keys are
/// on some other part of the surface, because it is still the row they would
/// act on once they come back — which is what tells a list apart from one the
/// arrows cannot reach.
///
/// The Sidebar keeps no such selection: its row focus goes with the keys, and
/// it is drawn through [`sidebar_focus_style`] instead.
fn selection_style(selected: bool, focused: bool, theme: &Theme) -> Option<Style> {
    selected.then_some(if focused {
        theme.selection.focused
    } else {
        theme.selection.unfocused
    })
}

/// What a row says after the Title of a Session the client could not read.
const UNREADABLE_MARKER: &str = "[unreadable]";

/// What a Session is called in the Sidebar: its Emoji, where it has one, and
/// then its Title — followed, where the client could not read the Session, by
/// the marker saying so. The marker's columns are held back before the Title
/// is cut, so however long the Title the reason the row cannot be opened stays
/// on show. `width` is the columns the whole name has to spend.
fn sidebar_title_parts(row: SidebarRow<'_>, width: usize) -> (String, Vec<String>) {
    let title = match row.emoji {
        Some(emoji) => format!("{emoji} {}", row.title),
        None => row.title.to_owned(),
    };
    let mut tags = row
        .remote
        .map(|remote| format!("[{remote}]"))
        .into_iter()
        .collect::<Vec<_>>();
    if row.unreadable {
        tags.push(UNREADABLE_MARKER.to_owned());
    }
    let tags_width = tags.iter().map(|tag| tag.width() + 1).sum::<usize>();
    (
        truncate_to_width(&title, width.saturating_sub(tags_width)),
        tags,
    )
}

/// A Session Title and its fixed tags. Tags spend their columns before the
/// Title, so narrowing a frame can shorten the name but never erase the
/// Origin or unreadable marker that explains the row.
fn sidebar_title_line(
    row: SidebarRow<'_>,
    width: usize,
    highlight: Option<Style>,
    theme: &Theme,
) -> Line<'static> {
    let title_style = sidebar_title_style(row, highlight, theme);
    let tag_style = highlight.unwrap_or_default().patch(theme.text.subdued);
    let (title, tags) = sidebar_title_parts(row, width);
    let used = title.width() + tags.iter().map(|tag| tag.width() + 1).sum::<usize>();
    let mut spans = vec![Span::styled(title, title_style)];
    spans.extend(
        tags.into_iter()
            .map(|tag| Span::styled(format!(" {tag}"), tag_style)),
    );
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        highlight.unwrap_or_default(),
    ));
    Line::from(spans)
}

fn sidebar_slotted_title_line(
    row: SidebarRow<'_>,
    slot: &str,
    width: usize,
    highlight: Option<Style>,
    theme: &Theme,
) -> Line<'static> {
    let label_width = width.saturating_sub(slot.width() + 1);
    let mut line = sidebar_title_line(row, label_width, highlight, theme);
    let label_width = line.width();
    line.spans.push(Span::styled(
        " ".repeat(width.saturating_sub(label_width + slot.width())),
        highlight.unwrap_or_default(),
    ));
    line.spans.push(Span::styled(
        slot.to_owned(),
        highlight.unwrap_or(theme.text.subdued),
    ));
    line
}

/// How a Sidebar row is drawn. Only focus paints the row itself; which Session
/// is open is carried by its Title instead.
fn sidebar_row_style(row: SidebarRow<'_>, driving: bool, theme: &Theme) -> Option<Style> {
    sidebar_focus_style(row.focused, driving, theme)
}

/// How a Session's name is drawn where no highlight covers the row: subdued
/// for one the client could not read or whose Remote is recovering, and plain
/// otherwise.
///
/// Row focus supplies the background. A readable open Session patches the
/// accent foreground over either base, while unreadable and recovering rows
/// stay subdued so their warning is never disguised as ordinary work. Where
/// the accent is the focus block's own colour, the open Title keeps the
/// block's text foreground instead of vanishing into it.
fn sidebar_title_style(row: SidebarRow<'_>, highlight: Option<Style>, theme: &Theme) -> Style {
    let unavailable = row.unreadable || row.recovering;
    let style = highlight.unwrap_or(if unavailable {
        theme.text.subdued
    } else {
        theme.text.primary
    });
    let accent_vanishes = theme
        .selection
        .open_title
        .fg
        .is_some_and(|fg| style.bg == Some(fg));
    if row.open && !unavailable && !accent_vanishes {
        style.patch(theme.selection.open_title)
    } else {
        style
    }
}

/// `text` with enough trailing spaces to fill `width` columns, so a line that
/// carries a background carries it the whole way across.
fn pad_to_width(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    padded.push_str(&" ".repeat(width.saturating_sub(text.width())));
    padded
}

/// Where the Session written here would execute, said exactly as the Landing
/// says it: the path, its Checkout State as a Sidebar row reads it, and — while
/// the intent is pending — that a new checkout will be made on submit instead.
///
/// The Landing and the Provisional Session share it word for word, because the
/// Provisional Session is drawn from the very reading the Landing was showing
/// when the Prompt was submitted.
fn execution_context(state: &TuiState, show_icons: bool) -> String {
    match state.execution_directory.as_deref() {
        Some(_) => {
            let label = workspace_context(state, &state.workspace.path, true);
            let status = if matches!(
                state.execution_status,
                crate::protocol::ExecutionDirectoryStatus::Unavailable { .. }
            ) {
                " · unavailable"
            } else {
                ""
            };
            // What the next Session would begin on, said exactly as a
            // Sidebar row says it, and live: the catalog's reading
            // stands in front of the one the resolution carried.
            let checkout = state
                .execution_checkout_state()
                .and_then(|checkout| checkout_state_context(checkout, show_icons))
                .map(|checkout| format!(" · {checkout}"))
                .unwrap_or_default();
            let subdirectory = state
                .execution_subdirectory()
                .map(|path| format!(" · {}", icon_label(show_icons, NF_FA_FOLDER_TREE, &path)))
                .unwrap_or_default();
            // A pending intent is only ever said where there is a
            // Repository to make it in, and only until it has been carried
            // out: a Worktree already made is said by the path above and the
            // Checkout State beside it.
            let intent = if state.new_worktree.is_some()
                && state.workspace.repository.is_some()
                && !state.intent_already_prepared()
            {
                format!(
                    " · {}",
                    icon_label(show_icons, NF_COD_WORKTREE, "New Worktree on submit")
                )
            } else {
                String::new()
            };
            format!("{label}{status}{checkout}{subdirectory}{intent}")
        }
        None => {
            let intent = if state.new_worktree.is_some() {
                icon_label(show_icons, NF_COD_WORKTREE, "New Worktree on submit")
            } else {
                "Choose a working copy to start a Session".to_owned()
            };
            format!(
                "{} · {intent}",
                workspace_context(state, &state.workspace.path, true)
            )
        }
    }
}

fn render_landing(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let show_icons = state.settings().appearance.show_icons;
    let detail = ResponsiveDetail::for_width(area.width);
    let footer_detail = detail.secondary_only_when(area.height >= LANDING_DETAILS_MINIMUM_HEIGHT);
    let footer_width = area
        .width
        .saturating_sub(horizontal_padding(area.width).saturating_mul(2));
    let agent = agent_selection_context(state, footer_detail);
    // At minimum height, the composer and Workspace need all four rows above
    // the footer. Carry submission errors in the footer to keep input usable.
    let footer_error = state
        .submission_error
        .as_deref()
        .filter(|_| area.height == MINIMUM_TERMINAL_HEIGHT);
    let context = match footer_error {
        Some(error) => SlotText::new(format!("Error: {error}"), theme.form_field.invalid),
        None => SlotText::new(agent, theme.text.subdued),
    };
    let footer = slots.landing_footer(&LandingFooterSlotContext {
        width: footer_width,
        context,
        connection: SlotText::new(
            connection_status_text(state, footer_detail),
            status_style(state, theme),
        ),
    });
    let [main, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer.height())]).areas(area);
    let content = horizontally_inset(main, horizontal_padding(area.width));
    let key = ComposerKey::Landing;
    let composer_text = state.composers.text(key.clone());
    let composer_cursor = state.composers.cursor(key.clone());
    let skill_markers = state.composers.skill_markers(key.clone());
    let composer_height = composer_block_height(
        area.height,
        72_u16.min(content.width),
        composer_text,
        composer_cursor,
    );
    let error = state
        .submission_error
        .as_deref()
        .filter(|_| footer_error.is_none());
    let error_height = u16::from(error.is_some());
    let workspace_height = 1;
    let composer_height = composer_height.min(
        content
            .height
            .saturating_sub(error_height + workspace_height)
            .max(1),
    );
    let logo_width = LANDING_LOGO
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(0) as u16;
    let logo_height = LANDING_LOGO.len() as u16;
    let brand_height = logo_height + 1;
    let show_brand = state.settings().appearance.landing_page == LandingPage::Fancy
        && content.width >= logo_width
        && content.height >= composer_height + error_height + workspace_height + brand_height;
    let panel_height = composer_height
        .saturating_add(if show_brand { brand_height } else { 0 })
        .saturating_add(error_height)
        .saturating_add(workspace_height);
    let panel = centered_rect(content, 72, panel_height);
    let mut row = panel.y;
    if show_brand {
        // Offset the logo's heavier right character for visual balance.
        let logo_offset = ((panel.width - logo_width) / 2).saturating_sub(1);
        frame.render_widget(
            Paragraph::new(LANDING_LOGO.join("\n"))
                .style(theme.accent.primary.add_modifier(Modifier::BOLD)),
            Rect::new(panel.x + logo_offset, row, logo_width, logo_height),
        );
        row = row.saturating_add(brand_height);
    }
    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(error)
                .alignment(Alignment::Center)
                .style(theme.form_field.invalid),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    let composer_area = Rect::new(panel.x, row, panel.width, composer_height);
    let cursor = Some(render_composer(
        frame,
        composer_area,
        ComposerContent {
            memory: &state.composers,
            key,
            text: composer_text,
            cursor: composer_cursor,
            skill_markers: &skill_markers,
        },
        state.composer_border_style(theme),
        theme,
    ));

    frame.render_widget(
        Paragraph::new(truncate_to_width(
            &execution_context(state, show_icons),
            usize::from(panel.width),
        ))
        .style(theme.text.subdued),
        Rect::new(
            panel.x,
            composer_area.bottom(),
            panel.width,
            workspace_height,
        ),
    );

    render_slot(
        frame,
        horizontally_inset(footer_area, horizontal_padding(area.width)),
        footer,
        theme,
    );
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

/// The shell a Session the reader has been carried into is drawn as until its
/// snapshot lands.
///
/// It holds the target's composer, standing where the Session view docks one,
/// and nothing else. There is no header, Transcript, Working state, usage, or
/// footer, because every one of those is read off a Session and the only
/// Session in hand is the one the reader has left. Nor is anything built from
/// the listing row that named the target: a summary is not a Session, and a
/// shell furnished from one would be telling the reader something it does not
/// know. It stays quiet for the opening threshold; after that, the loading
/// label uses the Working Indicator's shimmer until the snapshot lands.
fn render_opening_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    theme: &Theme,
    truecolor: bool,
) -> RenderedComposer {
    let content_column = session_content_area(area, state);
    let content_width = content_column.width;
    let key = ComposerKey::Session(
        state
            .route
            .clone()
            .expect("the opening shell is drawn for the Session the route names"),
    );
    let composer_text = state.composers.text(key.clone());
    let composer_cursor = state.composers.cursor(key.clone());
    let skill_markers = state.composers.skill_markers(key.clone());
    let desired_composer_height =
        composer_block_height(area.height, content_width, composer_text, composer_cursor);
    // What the Session view keeps below its Transcript and above its composer:
    // the footer's row and the Transcript's bottom margin, plus the one row of
    // Transcript the layout never squeezes away. The shell holds them all
    // empty, so a composer that grows as the reader types stops where it would
    // in the view, and the draft in it does not jump a line when the snapshot
    // lands.
    let reserved_height = SESSION_FOOTER_HEIGHT
        .saturating_add(TRANSCRIPT_BOTTOM_MARGIN)
        .saturating_add(1);
    let composer_height =
        desired_composer_height.min(area.height.saturating_sub(reserved_height).max(1));
    let [_, transcript_area, _, _, _, _, composer_area, _] = session_areas(
        area,
        0,
        0,
        0,
        0,
        composer_height,
        SESSION_FOOTER_HEIGHT.min(area.height.saturating_sub(composer_height)),
    );
    let transcript_area = in_column(transcript_area, content_column);
    let composer_area = in_column(composer_area, content_column);
    if let Some(error) = state.opening_error.as_deref() {
        frame.render_widget(
            Paragraph::new(client_error_lines(error, theme)).wrap(Wrap { trim: false }),
            transcript_area,
        );
    } else if state.opening_loading_is_visible() {
        frame.render_widget(
            Paragraph::new(Line::from(shimmered_label_spans(
                "Loading",
                state.shimmer_clock.frame("Loading", state.spinner_frame),
                theme.text.primary,
                theme.text.subdued,
                truecolor,
            ))),
            transcript_area,
        );
        state.session_animation_on_screen.set(true);
    }
    let cursor = Some(render_composer(
        frame,
        composer_area,
        ComposerContent {
            memory: &state.composers,
            key,
            text: composer_text,
            cursor: composer_cursor,
            skill_markers: &skill_markers,
        },
        state.composer_border_style(theme),
        theme,
    ));
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

/// The Session view a client draws for its own claim on a Session it has asked
/// for and not been answered about.
///
/// Everything in it is drawn from what the client already knows: the Prompt as
/// the user Message it will become, the Title that Prompt gives, where it would
/// execute, the Agent it would begin under, and a Working Indicator with no
/// elapsed time, because only the Server knows when Working began. It uses the
/// Session view's own layout so the Session replaces it in place rather than
/// shifting under the reader when it arrives.
///
/// Its composer is the Landing's, which is what the draft migration onto the
/// created Session's key reads from, so a draft typed while the Server is
/// answering survives the handoff.
///
/// A refusal leaves every one of those readings standing and puts a
/// client-local, transcript-shaped Error row where the Working Indicator was.
/// What a Session view is drawn from: the Session a Server answered for, or the
/// claim a client stands on until it does.
///
/// Everything the view reads that a claim cannot answer for itself is named
/// here, so both are drawn by one body and a claim is replaced in place rather
/// than giving way to a differently built view.
struct SessionSurface<'a> {
    snapshot: &'a SessionSnapshot,
    /// The composer the keys write into: the Session's own, or the Landing's
    /// while a claim waits for one, which is where the draft migration onto the
    /// created Session's key reads from.
    composer_key: ComposerKey,
    interaction: &'a SessionInteraction,
    transcript: std::cell::Ref<'a, TranscriptView>,
    tail: SessionTail,
    /// Whether the view names where it would execute, as the Landing does. Only
    /// a claim does: a Session has a header that says so.
    execution_line: bool,
}

/// What stands immediately after the latest Transcript row.
enum SessionTail {
    /// Nothing: the Session is not Working and nothing was refused.
    Quiet,
    Working {
        state: WorkingIndicatorState,
        /// Absent in a claim, whose Working began at a moment only the Server
        /// knows.
        working_since: Option<SessionTimestamp>,
        interrupt: Option<WorkingIndicatorInterrupt>,
    },
    /// A refusal this client draws itself: transcript-shaped, and never part of
    /// the Transcript.
    Refused(String),
}

/// What the Working Indicator says about interrupting while nothing has been
/// confirmed: armed for as long as the reader's first Escape stands, and ready
/// to take one otherwise.
fn interrupt_guidance(state: &TuiState) -> WorkingIndicatorInterrupt {
    if matches!(
        state.command_mode,
        CommandMode::InterruptConfirmation { .. }
    ) {
        WorkingIndicatorInterrupt::Armed
    } else {
        WorkingIndicatorInterrupt::Ready
    }
}

/// The Session view a client draws for its own claim on a Session it has asked
/// for and not been answered about.
///
/// Everything in it is drawn from what the client already knows: the Prompt as
/// the user Message it will become, the Title that Prompt gives, where it would
/// execute, the Agent it would begin under, and a Working Indicator with no
/// elapsed time, because only the Server knows when Working began. A refusal
/// leaves every one of those readings standing and puts a client-local,
/// transcript-shaped Error row where the Working Indicator was.
fn render_provisional_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
    truecolor: bool,
) -> RenderedComposer {
    let claim = state
        .provisional
        .as_ref()
        .expect("the Provisional Session renderer requires the claim it draws");
    let snapshot = state
        .provisional_snapshot()
        .expect("a Provisional Session draws the Session it claims");
    let interaction = state
        .provisional_interaction()
        .expect("a claim keeps its own view state while it stands");
    let content_width = session_content_area(area, state).width;
    let transcript = state
        .provisional_transcript_view(&snapshot, theme, content_width)
        .expect("a Provisional Session projects the Prompt it stands for");
    let tail = match &claim.standing {
        ClaimStanding::Refused { error } => SessionTail::Refused(format!(
            "Could not create Session: {error} · Enter to retry, or type a new prompt"
        )),
        ClaimStanding::Claimed { interrupt_intent } => SessionTail::Working {
            state: match claim.phase {
                super::state::ProvisionalSessionPhase::PreparingWorktree => {
                    WorkingIndicatorState::CreatingWorktree
                }
                super::state::ProvisionalSessionPhase::CreatingSession => {
                    WorkingIndicatorState::Working
                }
            },
            working_since: None,
            // An interrupt the reader has already confirmed says so until the
            // Session it is waiting for arrives to take it.
            interrupt: Some(if *interrupt_intent {
                WorkingIndicatorInterrupt::Requested
            } else {
                interrupt_guidance(state)
            }),
        },
    };
    render_session_surface(
        frame,
        state,
        area,
        slots,
        theme,
        truecolor,
        SessionSurface {
            snapshot: &snapshot,
            composer_key: ComposerKey::Landing,
            interaction,
            transcript,
            tail,
            execution_line: true,
        },
    )
}

fn render_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
    truecolor: bool,
) -> RenderedComposer {
    let snapshot = state
        .session
        .as_ref()
        .expect("Session renderer requires a Session")
        .snapshot();
    let session_reference = state
        .session_reference
        .as_ref()
        .expect("Session renderer requires its origin-qualified reference");
    let interaction = state
        .session_interaction(session_reference)
        .expect("Session interaction is initialized with its snapshot");
    let content_width = session_content_area(area, state).width;
    let transcript = state
        .transcript_view(theme, content_width)
        .expect("the rendered Session has its projection and interaction");
    // A Subagent's Session is read rather than conversed with: Escape leaves it
    // instead of interrupting, so its indicator carries elapsed work but no
    // false gesture.
    let subagent_view = snapshot.session.parent.is_some();
    let tail = snapshot
        .working_since()
        .map_or(SessionTail::Quiet, |since| SessionTail::Working {
            state: if snapshot.session.status == SessionStatus::Active {
                WorkingIndicatorState::Working
            } else {
                WorkingIndicatorState::WaitingForSubagents
            },
            working_since: Some(since),
            interrupt: (!subagent_view).then(|| interrupt_guidance(state)),
        });
    render_session_surface(
        frame,
        state,
        area,
        slots,
        theme,
        truecolor,
        SessionSurface {
            snapshot,
            composer_key: ComposerKey::Session(session_reference.clone()),
            interaction,
            transcript,
            tail,
            execution_line: false,
        },
    )
}

fn render_session_surface(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
    truecolor: bool,
    surface: SessionSurface<'_>,
) -> RenderedComposer {
    let SessionSurface {
        snapshot,
        composer_key: key,
        interaction,
        transcript: transcript_view,
        tail,
        execution_line,
    } = surface;
    let padding = horizontal_padding(area.width);
    let normally_padded = horizontally_inset(area, padding);
    let content_column = session_content_area(area, state);
    let content_width = content_column.width;
    let content_detail = ResponsiveDetail::for_width(content_width);
    let header_detail = ResponsiveDetail::for_width(normally_padded.width);
    let show_header = area.height >= SESSION_HEADER_MINIMUM_HEIGHT;
    let session_id = snapshot.session.id;
    // A Subagent's Session is read rather than conversed with: the composer
    // stands down for a one-line way back, and Escape means leaving rather
    // than interrupting.
    let subagent_view = snapshot.session.parent.is_some();
    let composer_text = state.composers.text(key.clone());
    let composer_cursor = state.composers.cursor(key.clone());
    let skill_markers = state.composers.skill_markers(key.clone());
    let desired_composer_height = if subagent_view {
        1
    } else {
        composer_block_height(area.height, content_width, composer_text, composer_cursor)
    };
    let composer_top = slots.session_composer_top(&SessionComposerTopSlotContext {
        session_id,
        width: content_width,
    });
    let working_indicator = match &tail {
        SessionTail::Quiet | SessionTail::Refused(_) => RenderedSlot::empty(),
        SessionTail::Working {
            state: indicator_state,
            working_since,
            interrupt,
        } => {
            let context = WorkingIndicatorSlotContext {
                session_id,
                width: content_width,
                state: *indicator_state,
                working_since: *working_since,
                interrupt: *interrupt,
            };
            let default = working_indicator_line(
                &context,
                SessionTimestamp::now().0,
                binding_label(&CommandId::RequestInterrupt),
                state
                    .shimmer_clock
                    .frame(working_indicator_label(context.state), state.spinner_frame),
                theme,
                truecolor,
            );
            slots.working_indicator(&context, default)
        }
    };
    let mut working_indicator_lines = rendered_slot_lines(working_indicator, content_width, theme);
    if !working_indicator_lines.is_empty() {
        working_indicator_lines.insert(0, Line::default());
    }
    let (agent, agent_style) = if let Some(error) = state.submission_error.as_ref() {
        (
            format!(
                "Error: {error} · {}",
                agent_selection_context(state, ResponsiveDetail::CoreOnly)
            ),
            theme.feedback.error,
        )
    } else if snapshot.session.status == SessionStatus::Active && !content_detail.shows_secondary()
    {
        (String::new(), theme.text.subdued)
    } else {
        (
            agent_selection_context(state, content_detail),
            theme.text.subdued,
        )
    };
    let usage = session_footer_metrics_text(snapshot, content_width)
        .map(|text| SlotText::new(text, theme.text.subdued));
    let footer = slots.prompt_footer(
        &PromptFooterSlotContext {
            session_id,
            width: content_width,
        },
        &PromptContextSlotContext {
            session_id,
            agent: SlotText::new(agent, agent_style),
            usage,
        },
    );
    // The execution line the Landing drew stays drawn, one row above the footer
    // the Session view puts its Agent Selection in.
    let execution_height = u16::from(execution_line);
    let footer_height = footer.height().saturating_add(execution_height);
    let queued_prompts = state.queued_prompts(session_id);
    let desired_pending_height = if queued_prompts.is_empty() {
        0
    } else {
        (queued_prompts.len() as u16).min(3).saturating_add(2)
    };
    let core_height = u16::from(show_header)
        .saturating_add(desired_composer_height)
        .saturating_add(composer_top.height())
        .saturating_add(footer_height)
        .saturating_add(TRANSCRIPT_BOTTOM_MARGIN)
        .saturating_add(1);
    let pending_room = area.height.saturating_sub(core_height);
    let pending_height = if pending_room >= 3 {
        desired_pending_height.min(pending_room)
    } else {
        0
    };
    let reserved_height = u16::from(show_header)
        .saturating_add(pending_height)
        .saturating_add(composer_top.height())
        .saturating_add(footer_height)
        .saturating_add(TRANSCRIPT_BOTTOM_MARGIN)
        .saturating_add(1);
    let composer_height =
        desired_composer_height.min(area.height.saturating_sub(reserved_height).max(1));
    let transcript_rows = transcript_view.row_count_with_tail(&working_indicator_lines);
    let [_, transcript_without_latest, _, _, _, _, _, _] = session_areas(
        area,
        u16::from(show_header),
        pending_height,
        0,
        composer_top.height(),
        composer_height,
        footer_height,
    );
    let viewport_without_latest = transcript_viewport_height(transcript_without_latest);
    let [_, transcript_with_latest, _, _, _, _, _, _] = session_areas(
        area,
        u16::from(show_header),
        pending_height,
        1,
        composer_top.height(),
        composer_height,
        footer_height,
    );
    let viewport_with_latest = transcript_viewport_height(transcript_with_latest);
    let (away_from_bottom, viewport_height, maximum_scroll, scroll_position) =
        if interaction.follow_latest.get() {
            let maximum_scroll = transcript_rows.saturating_sub(viewport_without_latest);
            (
                false,
                viewport_without_latest,
                maximum_scroll,
                maximum_scroll,
            )
        } else {
            let viewport_height = viewport_with_latest;
            let maximum_scroll = transcript_rows.saturating_sub(viewport_height);
            let scroll_position = interaction.anchor.get().map_or(maximum_scroll, |anchor| {
                transcript_view
                    .message_starts()
                    .iter()
                    .find(|start| start.message_id == anchor.message_id)
                    .map_or(maximum_scroll, |start| {
                        (start.row as isize)
                            .saturating_sub(anchor.screen_row)
                            .clamp(0, maximum_scroll as isize) as usize
                    })
            });
            if scroll_position >= maximum_scroll {
                interaction.follow_latest.set(true);
                interaction.anchor.set(None);
                let maximum_scroll = transcript_rows.saturating_sub(viewport_without_latest);
                (
                    false,
                    viewport_without_latest,
                    maximum_scroll,
                    maximum_scroll,
                )
            } else {
                (true, viewport_height, maximum_scroll, scroll_position)
            }
        };
    let latest_height = u16::from(away_from_bottom);
    let [
        header_area,
        transcript_area,
        _transcript_margin,
        pending_area,
        latest_area,
        composer_top_area,
        composer_area,
        status_area,
    ] = session_areas(
        area,
        u16::from(show_header),
        pending_height,
        latest_height,
        composer_top.height(),
        composer_height,
        footer_height,
    );
    if show_header {
        render_session_header(
            frame,
            state,
            snapshot,
            horizontally_inset(header_area, padding),
            header_detail,
            theme,
        );
    }

    let transcript_area = in_column(transcript_area, content_column);
    let pending_area = in_column(pending_area, content_column);
    let latest_area = in_column(latest_area, content_column);
    let composer_top_area = in_column(composer_top_area, content_column);
    let composer_area = in_column(composer_area, content_column);
    let footer_area = in_column(status_area, content_column);
    let has_top_border = transcript_area.height > 1;
    // The top border pushes projected rows down one, so a pointer maps back to
    // a transcript row through the same offset the rows are drawn with.
    let border_rows = u16::from(has_top_border);
    let visible_rows = usize::from(transcript_area.height.saturating_sub(border_rows));
    let mut window =
        transcript_view.window_with_tail(&working_indicator_lines, scroll_position, visible_rows);
    spinner::overlay_frame(
        &mut window.rows,
        &window.spinner_rows,
        state.spinner_frame / 3,
    );
    let spinner_visible = window.spinner_rows.iter().any(|row| *row < visible_rows);
    let tail_start = transcript_view.row_count();
    let tail_visible = !working_indicator_lines.is_empty()
        && scroll_position < transcript_rows
        && tail_start < scroll_position.saturating_add(viewport_height);
    state
        .session_animation_on_screen
        .set(spinner_visible || tail_visible);
    interaction.viewport.replace(Some(TranscriptViewport {
        height: viewport_height,
        scroll_position,
        maximum_scroll,
        message_starts: transcript_view.message_starts().to_vec(),
        unit_starts: transcript_view.unit_starts().to_vec(),
        content_top: transcript_area.y.saturating_add(border_rows),
        content_rows: transcript_area.height.saturating_sub(border_rows),
        content_left: transcript_area.x,
        content_width: transcript_area.width,
    }));
    if has_top_border {
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme.border.subdued),
            transcript_area,
        );
    }
    // The rows come wrapped from the projection, so the draw places each one
    // on its own terminal row rather than handing them to a wrapping widget
    // (ADR 0020): the row a cell is drawn on is the row a pointer resolves.
    let content_top = transcript_area.y.saturating_add(border_rows);
    let buffer = frame.buffer_mut();
    for (index, row) in window.rows.iter().take(visible_rows).enumerate() {
        draw_row(
            buffer,
            transcript_area.x,
            content_top.saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
            transcript_area.width,
            row,
        );
    }
    if let Some(selection) = state.text_selection.get()
        && selection.surface == super::selection::SelectionSurface::Transcript
        && !state.overlay_owns_input()
    {
        if selection.epoch == state.transcript_cache.selection_epoch() {
            transcript_view.highlight_selection(
                selection,
                buffer,
                Rect::new(
                    transcript_area.x,
                    content_top,
                    transcript_area.width,
                    transcript_area.height.saturating_sub(border_rows),
                ),
                scroll_position,
            );
        } else {
            state.text_selection.set(None);
        }
    }
    for link in &window.hyperlinks {
        super::event_loop::frame_backend::register_hyperlink(
            buffer,
            transcript_area
                .x
                .saturating_add(u16::try_from(link.column).unwrap_or(u16::MAX)),
            content_top.saturating_add(u16::try_from(link.row).unwrap_or(u16::MAX)),
            u16::try_from(link.width).unwrap_or(u16::MAX),
            &link.target,
        );
    }
    // The refusal is drawn after the projected rows rather than among them: it
    // resembles Transcript content without ever becoming part of it.
    if let SessionTail::Refused(error) = &tail {
        let top = content_top
            .saturating_add(
                u16::try_from(transcript_view.row_count().saturating_sub(scroll_position))
                    .unwrap_or(u16::MAX),
            )
            .saturating_add(1);
        if top < transcript_area.bottom() {
            frame.render_widget(
                Paragraph::new(client_error_lines(error, theme)).wrap(Wrap { trim: false }),
                Rect::new(
                    transcript_area.x,
                    top,
                    transcript_area.width,
                    transcript_area.bottom().saturating_sub(top),
                ),
            );
        }
    }
    if pending_height > 0 {
        render_pending_prompts(
            frame,
            pending_area,
            state,
            &queued_prompts,
            content_detail,
            theme,
        );
    }
    if away_from_bottom {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("Latest ↓ · {}", binding_label(&CommandId::FollowLatest)),
                theme.action.primary,
            ))
            .alignment(Alignment::Right),
            latest_area,
        );
    }
    render_slot(frame, composer_top_area, composer_top, theme);
    let cursor = if subagent_view {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Subagent Session · Esc returns to the parent",
                theme.text.subdued,
            )),
            composer_area,
        );
        None
    } else {
        Some(render_composer(
            frame,
            composer_area,
            ComposerContent {
                memory: &state.composers,
                key,
                text: composer_text,
                cursor: composer_cursor,
                skill_markers: &skill_markers,
            },
            state.composer_border_style(theme),
            theme,
        ))
    };
    if execution_height > 0 && footer_area.height >= execution_height {
        frame.render_widget(
            Paragraph::new(truncate_to_width(
                &execution_context(state, state.settings().appearance.show_icons),
                usize::from(footer_area.width),
            ))
            .style(theme.text.subdued),
            Rect::new(footer_area.x, footer_area.y, footer_area.width, 1),
        );
    }
    render_slot(
        frame,
        Rect::new(
            footer_area.x,
            footer_area.y.saturating_add(execution_height),
            footer_area.width,
            footer_area.height.saturating_sub(execution_height),
        ),
        footer,
        theme,
    );
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

/// Context occupancy has priority over accumulated Cost under width pressure.
fn session_footer_metrics_text(snapshot: &SessionSnapshot, width: u16) -> Option<String> {
    let cost = snapshot.total_usage().and_then(|total| {
        total
            .cost
            .filter(|cost| !cost.is_zero() || total.cost_is_partial)
            .map(|cost| {
                if cost.is_zero() {
                    "$0.00".to_owned()
                } else {
                    compact_cost(cost)
                }
            })
    });
    let (full, minimal) = match snapshot.session.context_fill {
        Some(fill) => {
            let count = compact_count(fill.occupied_tokens);
            match fill.capacity_tokens.filter(|capacity| *capacity > 0) {
                Some(capacity) => {
                    // Integer arithmetic rounds halves up without losing large
                    // counts to floating-point precision or capping the result.
                    let rounded = (u128::from(fill.occupied_tokens) * 100
                        + u128::from(capacity) / 2)
                        / u128::from(capacity);
                    let percent = format!("{rounded}%");
                    (Some(format!("{count} ({percent})")), Some(percent))
                }
                None => (Some(count.clone()), Some(count)),
            }
        }
        None => (None, None),
    };
    let combined = match (&full, cost) {
        (Some(fill), Some(cost)) => Some(format!("{fill} · {cost}")),
        (None, cost) => cost,
        _ => None,
    };
    [combined, full, minimal]
        .into_iter()
        .flatten()
        .find(|text| UnicodeWidthStr::width(text.as_str()) <= usize::from(width))
}

fn working_indicator_label(state: WorkingIndicatorState) -> &'static str {
    match state {
        WorkingIndicatorState::CreatingWorktree => "Creating worktree",
        WorkingIndicatorState::Working => "Working",
        WorkingIndicatorState::WaitingForSubagents => "Waiting for subagents",
    }
}

fn working_indicator_line(
    context: &WorkingIndicatorSlotContext,
    now: u64,
    interrupt_binding: &str,
    animation_frame: usize,
    theme: &Theme,
    truecolor: bool,
) -> Line<'static> {
    let label = working_indicator_label(context.state);
    // A Provisional Session carries no elapsed time: only the Server knows when
    // Working began, and it has not said so yet. The guidance stands either way,
    // so elapsed time is a prefix to it rather than a case of its own.
    let elapsed = context
        .working_since
        .map(|since| working_indicator_elapsed(since, now));
    let guidance = context.interrupt.map(|interrupt| match interrupt {
        WorkingIndicatorInterrupt::Ready => format!("{interrupt_binding} to interrupt"),
        WorkingIndicatorInterrupt::Armed => format!("{interrupt_binding} again to interrupt"),
        WorkingIndicatorInterrupt::Requested => "interrupting…".to_owned(),
    });
    let metadata = match (elapsed, guidance) {
        (None, None) => String::new(),
        (Some(said), None) | (None, Some(said)) => format!(" ({said})"),
        (Some(elapsed), Some(guidance)) => format!(" ({elapsed} • {guidance})"),
    };
    let label = shimmered_label_spans(
        label,
        animation_frame,
        theme.text.primary,
        theme.text.subdued,
        truecolor,
    )
    .into_iter()
    .map(|span| SlotText::new(span.content.into_owned(), span.style));
    Line::from(
        truncate_slot_text(
            label
                .chain(std::iter::once(SlotText::new(metadata, theme.text.subdued)))
                .collect(),
            usize::from(context.width),
        )
        .into_iter()
        .map(|item| Span::styled(item.text, item.style))
        .collect::<Vec<_>>(),
    )
}

fn shimmered_label_spans(
    label: &str,
    animation_frame: usize,
    primary: Style,
    subdued: Style,
    truecolor: bool,
) -> Vec<Span<'static>> {
    label
        .graphemes(true)
        .zip(shimmer::styles(
            label,
            animation_frame,
            primary,
            subdued,
            truecolor,
        ))
        .map(|(character, style)| Span::styled(character.to_string(), style))
        .collect()
}

/// Codex's compact elapsed form: seconds, then zero-padded seconds below an
/// hour, then zero-padded minutes and seconds once hours are present.
fn working_indicator_elapsed(since: SessionTimestamp, now: u64) -> String {
    let seconds = now.saturating_sub(since.0) / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        minutes / 60,
        minutes % 60,
        seconds % 60
    )
}

fn render_session_header(
    frame: &mut Frame<'_>,
    state: &TuiState,
    snapshot: &SessionSnapshot,
    area: Rect,
    detail: ResponsiveDetail,
    theme: &Theme,
) {
    let show_icons = state.settings().appearance.show_icons;
    let connection = connection_status_text(state, ResponsiveDetail::CoreOnly);
    let connection_width = connection.width().min(usize::from(area.width));
    let left_width = usize::from(area.width).saturating_sub(connection_width.saturating_add(2));
    let mut orientation = if detail.shows_secondary() {
        icon_label(
            show_icons,
            NF_COD_FOLDER,
            &state.workspace_name(&state.outlook, &snapshot.session.workspace.path),
        )
    } else {
        String::new()
    };
    if let Some(remote) = state.outlook.remote_name() {
        let remote = icon_label(show_icons, NF_MD_MONITOR, remote);
        orientation = [remote.as_str(), orientation.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
    }
    if detail.shows_secondary()
        && let Some(checkout) = state
            .session_reference
            .as_ref()
            .zip(snapshot.session.checkout.as_ref())
            .and_then(|(reference, checkout)| state.checkout_state(&reference.origin, &checkout.id))
        && let Some(branch) = checkout_branch_context(checkout, show_icons)
    {
        orientation.push_str(" · ");
        orientation.push_str(&branch);
    }
    let orientation = if !orientation.is_empty() {
        truncate_to_width(&orientation, left_width)
    } else {
        String::new()
    };
    // Reserve the same space on both sides to keep the Title at the exact
    // center, while leaving both indicators and a separating cell intact.
    let side_width = orientation.width().max(connection_width) + 1;
    let title_width = usize::from(area.width).saturating_sub(side_width.saturating_mul(2));
    let title = snapshot
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = match state
        .settings()
        .session
        .title
        .emoji
        .drawn_emoji(snapshot.emoji.as_deref())
    {
        Some(emoji) if !title.is_empty() => format!("{emoji} {title}"),
        _ => title,
    };
    let title = truncate_to_width(&title, title_width);
    let spacing =
        " ".repeat(usize::from(area.width).saturating_sub(orientation.width() + connection_width));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(orientation, theme.text.subdued),
            Span::raw(spacing),
            Span::styled(connection, status_style(state, theme)),
        ])),
        area,
    );
    if !title.is_empty() {
        let width = title.width() as u16;
        let title_area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y,
            width,
            area.height,
        );
        frame.render_widget(Paragraph::new(title).style(theme.text.primary), title_area);
    }
}

/// A Session header names the branch its Worktree stands on beside the
/// Workspace, and stays quiet where there is no branch to name: a detached
/// commit or an unreadable Worktree is not orientation.
fn checkout_branch_context(
    checkout: &crate::protocol::CheckoutSummary,
    show_icons: bool,
) -> Option<String> {
    use crate::protocol::{CheckoutRevision, SourceControlAvailability};

    if checkout.availability != SourceControlAvailability::Available
        || !matches!(checkout.revision, Some(CheckoutRevision::Branch { .. }))
    {
        return None;
    }
    checkout_state_context(checkout, show_icons)
}

fn agent_selection_context(state: &TuiState, detail: ResponsiveDetail) -> String {
    match state.agent_selection() {
        None => "Agent unavailable".to_owned(),
        Some(selection) => {
            let detailed = detail.shows_secondary();
            let summary = state.model_picker.selection_summary(selection, detailed);
            if detailed {
                provider_label(
                    state.settings().appearance.show_icons,
                    &selection.provider,
                    &summary,
                )
            } else {
                summary
            }
        }
    }
}

/// A Provider's Nerd Font glyph where Suru knows one. Unknown Provider IDs
/// keep their text-only presentation, which lets future Providers participate
/// in the shared surfaces before choosing an icon of their own.
fn provider_icon(provider: &crate::protocol::ProviderId) -> Option<char> {
    built_in_providers()
        .iter()
        .find(|candidate| &candidate.id == provider)
        .and_then(|provider| provider.nerd_font_icon)
}

fn provider_label(show_icons: bool, provider: &crate::protocol::ProviderId, label: &str) -> String {
    if show_icons && let Some(icon) = provider_icon(provider) {
        format!("{icon} {label}")
    } else {
        label.to_owned()
    }
}

/// The columns a Session's content is drawn in: the frame's own padding, then
/// the Session Content Width the reader has chosen. The Session view and the
/// shell one opens into read it the same way, so a composer keeps its columns
/// across hydration as well as its row.
fn session_content_area(area: Rect, state: &TuiState) -> Rect {
    session_content_column(
        horizontally_inset(area, horizontal_padding(area.width)),
        state.settings().session.content_width,
    )
}

fn session_areas(
    area: Rect,
    header_height: u16,
    pending_height: u16,
    latest_height: u16,
    composer_top_height: u16,
    composer_height: u16,
    footer_height: u16,
) -> [Rect; 8] {
    Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        // The Transcript's bottom margin. Vertical rhythm inside the
        // Transcript separates its own entries; this row keeps the last of
        // them off the composer below, and is layout's to draw rather than a
        // trailing blank the projection pads itself with.
        Constraint::Length(TRANSCRIPT_BOTTOM_MARGIN),
        Constraint::Length(pending_height),
        Constraint::Length(latest_height),
        Constraint::Length(composer_top_height),
        Constraint::Length(composer_height),
        Constraint::Length(footer_height),
    ])
    .areas(area)
}

fn transcript_viewport_height(area: Rect) -> usize {
    usize::from(if area.height > 1 {
        area.height.saturating_sub(1)
    } else {
        area.height
    })
}

struct ComposerContent<'a> {
    memory: &'a ComposerMemory,
    key: ComposerKey,
    text: &'a str,
    cursor: usize,
    skill_markers: &'a ComposerSkillMarkers,
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    content: ComposerContent<'_>,
    style: Style,
    theme: &Theme,
) -> Position {
    let ComposerContent {
        memory,
        key,
        text,
        cursor,
        skill_markers,
    } = content;
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(COMPOSER_TEXT_MARGIN))
        .border_style(style);
    let content_area = block.inner(area);
    let content_width = composer_content_width(area.width);
    let content_height = area.height.saturating_sub(2).max(1);
    let layout = TextLayout::new(text, content_width);
    let (cursor_row, cursor_column) =
        layout.cursor_position_with_affinity(cursor, memory.prefers_previous_row(key.clone()));
    let scroll = cursor_row.saturating_sub(content_height.saturating_sub(1));
    memory.record_frame(key, content_area, scroll);
    let paragraph = if text.is_empty() {
        Paragraph::new(Span::styled(
            "Type a prompt, run a /command, use a $skill",
            theme.form_field.placeholder,
        ))
    } else {
        Paragraph::new(wrapped_composer_lines(&layout, skill_markers, theme))
            .style(theme.form_field.text)
    };
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
    Position::new(
        area.x
            .saturating_add(1)
            .saturating_add(COMPOSER_TEXT_MARGIN)
            .saturating_add(cursor_column),
        area.y
            .saturating_add(1)
            .saturating_add(cursor_row.saturating_sub(scroll)),
    )
}

fn render_pending_prompts(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiState,
    prompts: &[QueuedPrompt<'_>],
    detail: ResponsiveDetail,
    theme: &Theme,
) {
    let leader = binding_label(&CommandId::BeginLeader);
    let queue = binding_label(&CommandId::OpenQueuedPrompts);
    let managing = matches!(state.command_mode, CommandMode::QueuedPrompts { .. });
    let title = if !detail.shows_secondary() {
        " Pending ".to_owned()
    } else if managing {
        format!(
            " Pending · {} steer · {} cancel ",
            binding_label(&CommandId::PromoteSelectedPrompt),
            binding_label(&CommandId::CancelSelectedPrompt)
        )
    } else {
        format!(" Pending · {leader} {queue} manage ")
    };
    let selected = match state.command_mode {
        CommandMode::QueuedPrompts { selected } => Some(selected),
        _ => None,
    };
    let lines = prompts
        .iter()
        .map(|prompt| {
            let text = prompt.text.replace('\n', " ");
            Line::styled(
                format!(
                    "{}{}",
                    if selected == Some(prompt.id) {
                        "› "
                    } else {
                        "  "
                    },
                    text
                ),
                if selected == Some(prompt.id) {
                    theme.selection.focused
                } else {
                    theme.text.subdued
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if managing {
                    theme.border.default
                } else {
                    theme.border.subdued
                }),
        ),
        area,
    );
}

fn render_reconnect_overlay(frame: &mut Frame<'_>, theme: &Theme) {
    frame.render_widget(Block::default().style(theme.surface.overlay), frame.area());
    let area = centered_rect(frame.area(), 48, 5);
    frame.render_widget(Clear, area);
    let details = if area.width >= 42 {
        vec![
            Line::styled("Reconnecting to Suru…", theme.feedback.warning),
            Line::default(),
            Line::styled("Your Session will resume automatically", theme.text.subdued),
        ]
    } else {
        vec![
            Line::styled("Reconnecting to Suru…", theme.feedback.warning),
            Line::styled("Your Session will", theme.text.subdued),
            Line::styled("resume automatically", theme.text.subdued),
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(details))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border.default)
                    .style(theme.surface.elevated),
            ),
        area,
    );
}

fn composer_block_height(terminal_height: u16, width: u16, text: &str, cursor: usize) -> u16 {
    let layout = TextLayout::new(text, composer_content_width(width));
    let cursor_rows = layout.cursor_position(cursor).0.saturating_add(1);
    let desired = layout.row_count().max(cursor_rows).max(1);
    let cap = (terminal_height / 3).max(1);
    desired.min(cap).saturating_add(2)
}

/// The columns a Prompt's text is laid out over inside a composer block of
/// `width`: its two borders, less the margin that keeps typed text off them.
fn composer_content_width(width: u16) -> u16 {
    width
        .saturating_sub(2)
        .saturating_sub(COMPOSER_TEXT_MARGIN.saturating_mul(2))
        .max(1)
}

fn wrapped_composer_lines(
    layout: &TextLayout<'_>,
    skill_markers: &ComposerSkillMarkers,
    theme: &Theme,
) -> Text<'static> {
    let lines = layout
        .rows()
        .map(|row| {
            let mut line = Vec::<(Style, String)>::new();
            for (offset, character) in row.text.char_indices() {
                let offset = row.start + offset;
                let style = if skill_markers
                    .invalid
                    .iter()
                    .any(|range| range.contains(&offset))
                {
                    theme.feedback.error
                } else if skill_markers
                    .recognized
                    .iter()
                    .any(|range| range.contains(&offset))
                {
                    theme.accent.primary
                } else {
                    theme.form_field.text
                };
                match line.last_mut() {
                    Some((current, text)) if *current == style => text.push(character),
                    _ => line.push((style, character.to_string())),
                }
            }
            styled_composer_line(line)
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn styled_composer_line(segments: Vec<(Style, String)>) -> Line<'static> {
    Line::from(
        segments
            .into_iter()
            .map(|(style, text)| Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

fn terminal_is_too_small(area: Rect) -> bool {
    area.width < MINIMUM_TERMINAL_WIDTH || area.height < MINIMUM_TERMINAL_HEIGHT
}

fn render_terminal_too_small(frame: &mut Frame<'_>, theme: &Theme) {
    let full_message = "Terminal too small";
    let message = if frame.area().width < full_message.width() as u16 {
        "Too small"
    } else {
        full_message
    };
    let message_height = if frame.area().width < message.width() as u16 {
        2
    } else {
        1
    };
    let area = centered_rect(frame.area(), frame.area().width, message_height);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(theme.feedback.warning),
        area,
    );
}

fn horizontal_padding(width: u16) -> u16 {
    if width < NARROW_TERMINAL_WIDTH { 1 } else { 2 }
}

fn horizontally_inset(area: Rect, padding: u16) -> Rect {
    let padding = padding.min(area.width / 2);
    Rect::new(
        area.x.saturating_add(padding),
        area.y,
        area.width.saturating_sub(padding.saturating_mul(2)),
        area.height,
    )
}

fn session_content_column(available: Rect, configured: SessionContentWidth) -> Rect {
    let width = match configured {
        SessionContentWidth::Fill => available.width,
        SessionContentWidth::Maximum(maximum) => u64::from(available.width).min(maximum) as u16,
    };
    Rect::new(
        available
            .x
            .saturating_add(available.width.saturating_sub(width) / 2),
        available.y,
        width,
        available.height,
    )
}

fn in_column(area: Rect, column: Rect) -> Rect {
    Rect::new(column.x, area.y, column.width, area.height)
}

fn render_slot(
    frame: &mut Frame<'_>,
    area: Rect,
    slot: RenderedSlot<Line<'static>>,
    theme: &Theme,
) {
    frame.render_widget(
        Paragraph::new(rendered_slot_lines(slot, area.width, theme)),
        area,
    );
}

fn rendered_slot_lines(
    slot: RenderedSlot<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    slot.failures
        .into_iter()
        .map(|failure| {
            Line::styled(
                truncate_to_width(
                    &format!("Extension error · {}: {}", failure.slot, failure.message),
                    usize::from(width),
                ),
                theme.feedback.error,
            )
        })
        .chain(slot.content)
        .collect()
}

fn connection_status_text(state: &TuiState, detail: ResponsiveDetail) -> String {
    if detail.shows_secondary() {
        return status_text(state);
    }
    if state.fatal_error.is_some() {
        "Connection failed".to_owned()
    } else if state.manually_stopped {
        "Server stopped".to_owned()
    } else if state.recovery.is_some() {
        "Recovering".to_owned()
    } else if state.identity.is_some() {
        "Connected".to_owned()
    } else {
        "Connecting".to_owned()
    }
}

fn workspace_context(state: &TuiState, path: &std::path::Path, show_path: bool) -> String {
    let show_icons = state.settings().appearance.show_icons;
    let remote = state
        .outlook
        .remote_name()
        .map(|remote| icon_label(show_icons, NF_MD_MONITOR, remote));
    let path = show_path.then(|| {
        icon_label(
            show_icons,
            NF_COD_FOLDER,
            &state.workspace_label(&state.outlook, path),
        )
    });
    remote
        .as_deref()
        .into_iter()
        .chain(path.as_deref())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn status_text(state: &TuiState) -> String {
    if let Some(error) = &state.fatal_error {
        return format!("Connection failed: {error}");
    }
    if state.manually_stopped {
        return state.identity.as_ref().map_or_else(
            || "Shared server stopped intentionally".to_owned(),
            |identity| {
                format!(
                    "Shared server stopped intentionally | {}",
                    server_identity_text(identity)
                )
            },
        );
    }
    if let Some(recovery) = state.recovery {
        let last_server = state.identity.as_ref().map_or_else(
            || "no previous server".to_owned(),
            |identity| format!("last server pid {}", identity.pid),
        );
        return format!(
            "Recovering (attempt {}, retry in {:?}) | {last_server}",
            recovery.attempt, recovery.retry_in
        );
    }
    match &state.identity {
        Some(identity) => format!("Connected | {}", server_identity_text(identity)),
        None => "Connecting to Suru server...".to_owned(),
    }
}

fn server_identity_text(identity: &ServerIdentity) -> String {
    format!(
        "pid {} | server {}",
        identity.pid,
        &identity.instance_id.to_string()[..8]
    )
}

fn status_style(state: &TuiState, theme: &Theme) -> Style {
    if state.fatal_error.is_some() {
        theme.feedback.error
    } else if state.identity.is_some() && state.recovery.is_none() && !state.manually_stopped {
        theme.feedback.success
    } else {
        theme.feedback.warning
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::{Buffer, Cell},
        style::{Color, Style},
    };
    use uuid::Uuid;

    use super::super::{
        slots::{
            Placement, PromptContextSlotContext, PromptFooterSlotContext, RenderSlots, SlotText,
            TestContribution,
        },
        state::{Application, ApplicationEvent},
    };
    use super::{session_footer_metrics_text, working_indicator_elapsed};
    use crate::{
        managed_client::{ManagedEvent, SessionEvent},
        protocol::{
            EffectiveSettings, Health, LifecycleState, ModelAvailability, ServerIdentity, Session,
            SessionContentWidth, SessionId, SessionRevision, SessionSettings, SessionSnapshot,
            SessionStatus, SessionTimestamp, SettingsSnapshot, SidebarSettings, SidebarVisibility,
            Workspace,
        },
    };

    /// Exercise the Session renderer directly: the enclosing terminal shell
    /// shows its size notice below 28 columns, but slots must also behave in
    /// smaller content areas supplied by a layout.
    #[test]
    fn context_fill_footer_drops_cost_then_count_and_hides_unfit_minimum() {
        use crate::protocol::{ContextFill, Cost, CostTotal, UsageTotal};
        for capacity in [Some(200_000), None] {
            let workspace = tempfile::tempdir().unwrap();
            let snapshot = SessionSnapshot {
                title: String::new(),
                emoji: None,
                session: Session {
                    checkout: None,
                    context_fill: Some(ContextFill {
                        occupied_tokens: 12_400,
                        capacity_tokens: capacity,
                    }),
                    id: SessionId::new(),
                    execution_directory: crate::protocol::ExecutionDirectory {
                        path: workspace.path().to_owned(),
                    },
                    workspace: Workspace::directory(workspace.path().to_owned()),
                    agent_selection: None,
                    agent_selection_availability: ModelAvailability::Available,
                    approval_posture: None,
                    status: SessionStatus::Active,
                    working_since: None,
                    parent: None,
                },
                revision: SessionRevision::INITIAL,
                prompts: Vec::new(),
                turns: Vec::new(),
                messages: Vec::new(),
                activities: Vec::new(),
                transcript: Vec::new(),
                subagent_interventions: Vec::new(),
                pending_approvals: Vec::new(),
                submitting_approvals: Vec::new(),
                pending_approvals_revision: crate::protocol::SessionRevision(0),
                subagent_usage: Some(UsageTotal {
                    cost: Cost::from_usd(0.42),
                    ..UsageTotal::default()
                }),
                total_cost: None,
            };
            let mut partial_zero = snapshot.clone();
            partial_zero.session.context_fill = None;
            partial_zero.total_cost = Some(CostTotal {
                cost: Cost::from_nano_usd(0),
                is_partial: true,
            });
            assert_eq!(
                session_footer_metrics_text(&partial_zero, 30).as_deref(),
                Some("$0.00"),
                "a known-zero partial total remains visible"
            );
            let mut application = Application::default();
            application
                .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
                .unwrap();
            let cases: &[(u16, &str)] = if capacity.is_some() {
                &[
                    (80, "12.4K (6%) · $0.42"),
                    (17, "12.4K (6%)"),
                    (9, "6%"),
                    (3, ""),
                ]
            } else {
                &[(80, "12.4K · $0.42"), (10, "12.4K"), (6, "")]
            };
            for &(width, expected) in cases {
                let mut terminal = Terminal::new(TestBackend::new(width, 25)).unwrap();
                terminal
                    .draw(|frame| {
                        super::render_session(
                            frame,
                            &application.state,
                            frame.area(),
                            &application.slots,
                            &application.theme,
                            false,
                        );
                    })
                    .unwrap();
                let rows = rendered_rows_from_buffer(terminal.backend().buffer());
                let footer = rows.last().unwrap();
                assert!(
                    footer.contains(expected),
                    "width {width}: expected {expected}, got {footer}"
                );
                if width < 20 {
                    assert!(!footer.contains('$'), "Cost drops first: {footer}");
                }
                if expected == "6%" {
                    assert!(!footer.contains('K'));
                }
                if expected.is_empty() {
                    assert!(
                        !footer.contains('%') && !footer.contains('K'),
                        "partial measurements stay hidden: {footer}"
                    );
                }
            }
        }
    }

    fn rendered_buffer(application: &Application) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("create test terminal");
        terminal
            .draw(|frame| application.render(frame))
            .expect("render headless TUI application");
        terminal.backend().buffer().clone()
    }

    fn rendered_rows(application: &Application) -> Vec<String> {
        rendered_rows_from_buffer(&rendered_buffer(application))
    }

    fn text_cell<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
        for (y, row) in rendered_rows_from_buffer(buffer).into_iter().enumerate() {
            if let Some(byte_offset) = row.find(needle) {
                let x = row[..byte_offset].chars().count() as u16;
                return buffer.cell((x, y as u16)).expect("text cell is in bounds");
            }
        }
        panic!("rendered frame did not contain {needle:?}");
    }

    fn rendered_rows_from_buffer(buffer: &Buffer) -> Vec<String> {
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn fatal_protocol_error_is_rendered_visibly_with_the_last_known_state() {
        let instance_id = Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002")
            .expect("parse fixture instance ID");
        let mut application = Application::default();
        application.state.apply(ManagedEvent::Connected(Health::new(
            ServerIdentity {
                instance_id,
                pid: 42_424,
                protocol_version: 1,
                build_identity: "suru@test".to_owned(),
            },
            LifecycleState::Ready,
        )));
        application.state.apply(ManagedEvent::Fatal(
            "server sent unknown event type 'future_event'".to_owned(),
        ));

        let screen = rendered_rows(&application).join("\n");
        assert!(!screen.contains("▀▀▀▀▀▀▀▀█▀▀▀▀▀"));
        assert!(screen.contains("Connection failed"));
        assert!(screen.contains("unknown event type 'future_event'"));
    }

    #[test]
    fn named_slots_compose_in_order_and_isolate_failed_contributions() {
        let slots = RenderSlots::testing([
            TestContribution::landing_footer(Placement::Prepend, Ok("prepend")),
            TestContribution::landing_footer(Placement::Replace, Err("replacement failed")),
            TestContribution::landing_footer(Placement::Append, Ok("append one")),
            TestContribution::landing_footer(Placement::Append, Ok("append two")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };

        let rows = rendered_rows(&application);
        let prepend = rows.iter().position(|row| row.contains("prepend")).unwrap();
        let default = rows
            .iter()
            .position(|row| row.contains("Agent unavailable"))
            .unwrap();
        let append_one = rows
            .iter()
            .position(|row| row.contains("append one"))
            .unwrap();
        let append_two = rows
            .iter()
            .position(|row| row.contains("append two"))
            .unwrap();
        let failure = rows
            .iter()
            .position(|row| row.contains("Extension error · landing.footer"))
            .unwrap();

        assert!(prepend < default);
        assert!(default < append_one);
        assert!(append_one < append_two);
        assert!(failure < prepend);

        let slots = RenderSlots::testing([
            TestContribution::landing_footer(Placement::Prepend, Ok("prepend")),
            TestContribution::landing_footer(Placement::Replace, Ok("replacement one")),
            TestContribution::landing_footer(Placement::Replace, Ok("replacement two")),
            TestContribution::landing_footer(Placement::Append, Ok("append")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };
        let screen = rendered_rows(&application).join("\n");
        assert!(screen.contains("prepend"));
        assert!(screen.contains("replacement two"));
        assert!(screen.contains("append"));
        assert!(!screen.contains("replacement one"));
        assert!(!screen.contains("Agent unavailable"));
    }

    #[test]
    fn the_application_notice_slot_takes_contributions_even_when_startup_was_clean() {
        let slots = RenderSlots::testing([
            TestContribution::application_notice(
                Placement::Prepend,
                Ok("notice from an extension"),
            ),
            TestContribution::application_notice(Placement::Append, Err("notice failed")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };

        let rows = rendered_rows(&application);
        let failure = rows
            .iter()
            .position(|row| row.contains("Extension error · application.notice"))
            .unwrap();
        let notice = rows
            .iter()
            .position(|row| row.contains("notice from an extension"))
            .unwrap();
        let composer = rows
            .iter()
            .position(|row| row.contains("Type a prompt"))
            .unwrap();

        assert!(
            failure < notice,
            "a failed contribution reports above the slot"
        );
        assert!(
            notice < composer,
            "the Notice sits above the Landing rather than over it"
        );
    }

    #[test]
    fn session_slots_render_with_their_typed_session_context() {
        let session_id = SessionId::new();
        let slots = RenderSlots::testing([
            TestContribution::session_composer_top(Placement::Append, Ok("composer top")),
            TestContribution::working_indicator(Placement::Prepend, Ok("indicator extension"))
                .styled(Style::default().fg(Color::LightMagenta)),
            TestContribution::working_indicator(Placement::Append, Err("indicator failed")),
            TestContribution::prompt_footer_context(Placement::Append, Ok("context extension")),
            TestContribution::prompt_footer(Placement::Append, Ok("footer extension")),
        ]);
        let mut application = Application {
            slots,
            ..Application::default()
        };
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
                SessionSnapshot {
                    title: String::new(),
                    emoji: None,
                    session: Session {
                        checkout: None,
                        context_fill: None,
                        id: session_id,
                        execution_directory: crate::protocol::ExecutionDirectory {
                            path: PathBuf::from("/workspace"),
                        },
                        workspace: Workspace::directory(PathBuf::from("/workspace")),
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        approval_posture: None,
                        status: SessionStatus::Active,
                        working_since: Some(SessionTimestamp(1)),
                        parent: None,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: Vec::new(),
                    turns: Vec::new(),
                    messages: Vec::new(),
                    activities: Vec::new(),
                    transcript: Vec::new(),
                    subagent_interventions: Vec::new(),
                    pending_approvals: Vec::new(),
                    submitting_approvals: Vec::new(),
                    pending_approvals_revision: crate::protocol::SessionRevision(0),
                    subagent_usage: None,
                    total_cost: None,
                },
            )))
            .expect("hydrate test Application");

        let buffer = rendered_buffer(&application);
        let screen = rendered_rows_from_buffer(&buffer).join("\n");
        assert!(screen.contains("composer top"));
        assert!(screen.contains("indicator extension"));
        assert!(screen.contains("Working"));
        assert!(screen.contains("Agent unavailable · context extension"));
        assert!(screen.contains("footer extension"));
        assert!(screen.contains("Extension error · session.working_indicator: indicator failed"));
        assert_eq!(
            text_cell(&buffer, "indicator extension").fg,
            Color::LightMagenta
        );
        assert_ne!(text_cell(&buffer, "Working").fg, Color::LightMagenta);
    }

    #[test]
    fn session_extension_contexts_receive_the_effective_column_width() {
        let session_id = SessionId::new();
        let mut application = Application {
            slots: RenderSlots::testing_session_column_widths(),
            ..Application::default()
        };
        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
                SettingsSnapshot {
                    settings: EffectiveSettings {
                        session: SessionSettings {
                            content_width: SessionContentWidth::Maximum(60),
                            ..SessionSettings::default()
                        },
                        sidebar: SidebarSettings {
                            initial_visibility: SidebarVisibility::Hidden,
                            ..SidebarSettings::default()
                        },
                        ..EffectiveSettings::default()
                    },
                    pinned: vec!["session.contentWidth".to_owned()],
                    diagnostics: Vec::new(),
                },
            )))
            .expect("receive Session content width");
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
                SessionSnapshot {
                    title: String::new(),
                    emoji: None,
                    session: Session {
                        checkout: None,
                        context_fill: None,
                        id: session_id,
                        execution_directory: crate::protocol::ExecutionDirectory {
                            path: PathBuf::from("/workspace"),
                        },
                        workspace: Workspace::directory(PathBuf::from("/workspace")),
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        approval_posture: None,
                        status: SessionStatus::Idle,
                        working_since: None,
                        parent: None,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: Vec::new(),
                    turns: Vec::new(),
                    messages: Vec::new(),
                    activities: Vec::new(),
                    transcript: Vec::new(),
                    subagent_interventions: Vec::new(),
                    pending_approvals: Vec::new(),
                    submitting_approvals: Vec::new(),
                    pending_approvals_revision: crate::protocol::SessionRevision(0),
                    subagent_usage: None,
                    total_cost: None,
                },
            )))
            .expect("hydrate test Application");

        let screen = rendered_rows(&application).join("\n");
        assert!(screen.contains("composer extension width 60"));
        assert!(screen.contains("footer extension width 60"));
    }

    #[test]
    fn working_elapsed_uses_codex_compact_units() {
        let since = SessionTimestamp(10_000);
        assert_eq!(working_indicator_elapsed(since, 10_000), "0s");
        assert_eq!(working_indicator_elapsed(since, 69_000), "59s");
        assert_eq!(working_indicator_elapsed(since, 78_000), "1m 08s");
        assert_eq!(working_indicator_elapsed(since, 3_733_000), "1h 02m 03s");
    }

    #[test]
    fn empty_session_context_collapses_the_prompt_footer() {
        let session_id = SessionId::new();
        let footer = RenderSlots::builtins().prompt_footer(
            &PromptFooterSlotContext {
                session_id,
                width: 80,
            },
            &PromptContextSlotContext {
                session_id,
                agent: SlotText::new("", Style::default()),
                usage: None,
            },
        );
        assert_eq!(footer.height(), 0);
    }
}
