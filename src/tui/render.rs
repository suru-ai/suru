//! Frame rendering: the landing and Session screens, the pickers and overlays,
//! and the shared text and layout helpers they draw with.

use std::path::Path;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    managed_client::SessionProjection,
    protocol::{
        ModelAvailability, ModelDescriptor, ServerIdentity, SessionContentWidth, SessionSnapshot,
        SessionStatus, SessionTimestamp,
    },
    theme::Theme,
};

use super::{
    completion::CompletionRow,
    composer::{ComposerKey, ComposerSkillMarkers},
    keymap::binding_label,
    model_options::ModelOptionChoiceRow,
    model_picker::ModelPickerRow,
    session_picker::SessionPickerRow,
    settings_panel::{
        PanelLayout, RowAvailability, RowExpansion, RowValue, RowWindow, TabBar, TabSpan,
    },
    sidebar::{
        self, ADD_WORKSPACE, SIDEBAR_MENU_ITEMS, Sidebar, SidebarEntry, SidebarMenuGeometry,
        SidebarRow, SidebarScopeEntry, SidebarSelectorView, SidebarShelf, SidebarShowMore,
        SidebarSpan, SidebarTarget, SidebarWorkspaceEntryView,
    },
    slots::{
        LandingFooterSlotContext, LandingNoticeSlotContext, PromptContextSlotContext,
        PromptFooterSlotContext, PromptStatusSlotContext, RenderSlots, RenderedSlot,
        SessionComposerTopSlotContext, SlotText, truncate_to_width,
    },
    spinner,
    state::{CommandId, CommandMode, QueuedPrompt, TranscriptViewport, TuiState},
    subagent_picker::working_subagents,
    transcript::TranscriptDisclosure,
};

const NARROW_TERMINAL_WIDTH: u16 = 44;
const MINIMUM_TERMINAL_WIDTH: u16 = 28;
const MINIMUM_TERMINAL_HEIGHT: u16 = 5;
const LANDING_BRAND_MINIMUM_HEIGHT: u16 = 9;
const SESSION_HEADER_MINIMUM_HEIGHT: u16 = 8;
/// Rows of air the layout keeps below the Transcript, so its last entry never
/// abuts whatever is docked underneath.
/// Candidate setting: <https://github.com/jake-tucker/suru/issues/71>.
const TRANSCRIPT_BOTTOM_MARGIN: u16 = 1;

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

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    render_with_slots(frame, state, &RenderSlots::builtins());
}

pub(super) fn render_with_slots(frame: &mut Frame<'_>, state: &TuiState, slots: &RenderSlots) {
    let theme = Theme::system();
    // The settings panel is pointable, and only a frame that drew it can say
    // where. Every frame starts by giving up what the last one recorded, so the
    // geometry a click resolves against is always the one on screen.
    state.settings_panel.forget_layout();
    // The Sidebar's own frame record, given up for the same reason: whether it
    // has the keys depends on whether the frame had the columns to draw it.
    state.sidebar.forget_frame();
    // The Subagent Picker's rows are pointable, so its record of where they
    // were drawn starts over with the frame as well.
    state.subagent_picker.forget_frame();
    if terminal_is_too_small(frame.area()) {
        render_terminal_too_small(frame, &theme);
        return;
    }
    let main = render_sidebar(frame, state, &theme);
    let composer = if state.session.is_some() {
        render_session(frame, state, main, slots, &theme)
    } else {
        render_landing(frame, state, main, slots, &theme)
    };
    if state.composer_completion.is_visible() && !state.reconnect_overlay_visible {
        render_composer_completion(frame, state, composer.area, &theme);
    }
    // The Subagent Picker docks over the composer the way the completion list
    // does: it belongs to the composer's place on screen, not to the main
    // view's center.
    if state.subagent_picker.is_open() && !state.reconnect_overlay_visible {
        render_subagent_picker(frame, state, composer.area, &theme);
    }
    // The Sidebar's own context menu, drawn over the column and whatever of
    // the main view it runs into, because it stands in front of the row it was
    // opened on.
    if !state.reconnect_overlay_visible {
        render_sidebar_menu(frame, state, &theme);
    }
    // Every overlay is centered on the main view rather than the whole frame:
    // the Sidebar sits beside them and is neither opened over nor obscured.
    if state.session_picker.is_open() && !state.reconnect_overlay_visible {
        render_session_picker(frame, state, main, &theme);
    }
    if state.model_options.is_open() && !state.reconnect_overlay_visible {
        render_model_options(frame, state, main, &theme);
    }
    if state.settings_panel.is_open() && !state.reconnect_overlay_visible {
        render_settings_panel(frame, state, main, &theme);
        if state.settings_panel.numeric_editor().is_some() {
            render_numeric_editor(frame, state, main, &theme);
        }
    }
    // Last of the overlays, because a settings panel row opens it: the picker
    // is what the reader is answering, so it is drawn over whatever asked.
    if state.model_picker.is_open() && !state.reconnect_overlay_visible {
        render_model_picker(frame, state, main, &theme);
    }
    if state.reconnect_overlay_visible {
        render_reconnect_overlay(frame, &theme);
    } else if !state.session_picker.is_open()
        && !state.model_picker.is_open()
        && !state.model_options.is_open()
        && !state.settings_panel.is_open()
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
    render_overlay_box(frame, area, lines, " Number ", theme);
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
            truncate_to_width(
                &format!("Search: {}", state.session_picker.query()),
                usize::from(content_width),
            ),
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
        let current = state.session.as_ref().map(SessionProjection::session_id);
        let footer_rows = usize::from(shows_search_and_footer);
        let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
        let now = SessionTimestamp::now().0;
        let rows = state
            .session_picker
            .visible_rows(row_capacity, current)
            .map(|row| {
                let content = session_picker_row_text(row, usize::from(content_width), now);
                Line::styled(
                    content,
                    if row.selected {
                        theme.selection.focused
                    } else if row.unreadable {
                        theme.text.subdued
                    } else {
                        theme.text.primary
                    },
                )
            })
            .collect::<Vec<_>>();
        if rows.is_empty() && lines.len() < usize::from(content_height).saturating_sub(footer_rows)
        {
            lines.push(Line::styled("No Sessions found", theme.text.subdued));
        } else {
            lines.extend(rows);
        }
    }
    if shows_search_and_footer && lines.len() < usize::from(content_height) {
        let scope = state.session_picker.scope().label();
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
            truncate_to_width(
                &format!("Search: {}", state.model_picker.query()),
                usize::from(content_width),
            ),
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
    // Both tabs, always, so the reader can see what the panel holds without
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
                format!(" · {}", spinner::frame(state.spinner_frame))
            }
            RowAvailability::Unavailable(reason) => format!(" · {}", reason.label()),
            RowAvailability::Failed => " · error".to_owned(),
        };
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "{marker}{expansion}{}{value}{availability} [{origin}]",
                    row.label
                ),
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
    render_overlay_box(frame, area, lines, " Settings ", theme);
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
    if content_height >= 2 {
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
            area,
            lines,
            &format!(" {} Choices ", descriptor.label),
            theme,
        );
        return;
    }

    let footer_rows = usize::from(content_height >= 3);
    let capacity = content_height.saturating_sub(lines.len() + footer_rows);
    let rows = state.model_options.rows();
    let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
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
    if footer_rows > 0 && lines.len() < content_height {
        let controls = if state.model_options.is_valid() {
            "Enter configure · Ctrl+Enter apply · Esc cancel"
        } else {
            "Enter configure · Apply unavailable · Esc cancel"
        };
        lines.push(Line::styled(
            truncate_to_width(controls, content_width),
            theme.text.subdued,
        ));
    }
    render_overlay_box(frame, area, lines, " Model Options ", theme);
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

fn render_overlay_box(
    frame: &mut Frame<'_>,
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

fn session_picker_row_text(row: SessionPickerRow<'_>, width: usize, now: u64) -> String {
    let marker = if row.selected { "› " } else { "  " };
    if row.confirming_delete {
        return truncate_to_width(&format!("{marker}Press Ctrl+D again to confirm"), width);
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
    let mut metadata = status.into_iter().chain([age]).collect::<Vec<_>>();
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
                &legible_workspace(workspace),
                path_budget,
            ));
        }
    }
    let metadata = metadata.join(separator);
    let title_width = available
        .saturating_sub(metadata.width())
        .saturating_sub(separator.width());
    let title = truncate_to_width(row.title, title_width);
    truncate_to_width(
        &format!("{marker}{emoji}{title}{separator}{metadata}"),
        width,
    )
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

fn render_composer_completion(
    frame: &mut Frame<'_>,
    state: &TuiState,
    composer_area: Rect,
    theme: &Theme,
) {
    let available_width = composer_area.width;
    let width = available_width.clamp(1, 72);
    let room_above = composer_area.y.saturating_sub(frame.area().y);
    let bordered = room_above >= 3;
    let row_count = state.composer_completion.rows().len() as u16;
    let height = if bordered {
        row_count.saturating_add(2).min(room_above)
    } else {
        row_count.min(room_above.max(1))
    };
    let row_capacity = height.saturating_sub(if bordered { 2 } else { 0 });
    let x = composer_area
        .x
        .saturating_add(composer_area.width.saturating_sub(width) / 2);
    let y = composer_area.y.saturating_sub(height).max(frame.area().y);
    let area = Rect::new(x, y, width, height);
    let content_width = width.saturating_sub(if bordered { 2 } else { 0 });
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
    let width = composer_area.width.clamp(1, 72);
    let room_above = composer_area.y.saturating_sub(frame.area().y);
    let bordered = room_above >= 3;
    // One footer line naming the keys, kept only while every entry fits
    // beside it: the rows are what the picker is for.
    let desired_rows = (entries.len() as u16).saturating_add(1);
    let height = if bordered {
        desired_rows.saturating_add(2).min(room_above)
    } else {
        desired_rows.min(room_above.max(1))
    };
    let content_height = usize::from(height.saturating_sub(if bordered { 2 } else { 0 }));
    let shows_footer = content_height > entries.len();
    let row_capacity = content_height
        .saturating_sub(usize::from(shows_footer))
        .max(1);
    let x = composer_area
        .x
        .saturating_add(composer_area.width.saturating_sub(width) / 2);
    let y = composer_area.y.saturating_sub(height).max(frame.area().y);
    let area = Rect::new(x, y, width, height);
    let content_width = width.saturating_sub(if bordered { 2 } else { 0 });
    let content_x = x.saturating_add(u16::from(bordered));
    let content_y = y.saturating_add(u16::from(bordered));
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
        let content = truncate_to_width(
            &format!(
                "{guide} {} {}: {}",
                spinner::frame(state.spinner_frame),
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
        lines.push(Line::styled(
            truncate_to_width("Enter open · Esc close", usize::from(content_width)),
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
}

/// Draws the Sidebar down the left of the frame and reports what is left for
/// the main view — the whole frame when the reader has the Sidebar hidden, or
/// when the terminal cannot spare its columns.
fn render_sidebar(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) -> Rect {
    let frame_area = frame.area();
    if !state.sidebar.is_revealed() {
        return frame_area;
    }
    let Some(width) = sidebar::width_beside(frame_area.width) else {
        return frame_area;
    };
    state.sidebar.record_drawn();
    let [column, main] =
        Layout::horizontal([Constraint::Length(width), Constraint::Min(1)]).areas(frame_area);
    // The Sidebar's edge stands out while the reader is driving it, which is
    // the same account of focus the composer's own border gives.
    let block =
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(if state.sidebar.has_focus() {
                theme.border.default
            } else {
                theme.border.subdued
            });
    let inside = block.inner(column);
    let content = horizontally_inset(inside, 1);
    frame.render_widget(block, column);
    let (lines, rows) = sidebar_lines(state, content, theme);
    // The columns inside the rule rather than the content's own, so the
    // padding a row is inset by presses the row it insets. The spans the body
    // narrows to a run of one line are measured from the same edges, in
    // `sidebar_lines`.
    state
        .sidebar
        .record_geometry(inside.x..inside.right(), rows);
    frame.render_widget(Paragraph::new(lines), content);
    main
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
    let height = u16::try_from(SIDEBAR_MENU_ITEMS.saturating_add(2)).unwrap_or(u16::MAX);
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
) -> (Vec<Line<'static>>, Vec<SidebarSpan>) {
    let width = usize::from(content.width);
    let focused = state.sidebar.has_focus();
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
        &state.sidebar.selector(),
        width,
        focused,
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
        return (lines, rows);
    }
    let capacity = usize::from(content.height).saturating_sub(lines.len());
    let entries = state.sidebar.visible_entries(
        capacity,
        state.session.as_ref().map(SessionProjection::session_id),
    );
    if entries.is_empty() {
        lines.extend(
            sidebar_empty_reading(&state.sidebar)
                .map(|reading| Line::styled(reading, theme.text.subdued)),
        );
        return (lines, rows);
    }
    let now = SessionTimestamp::now().0;
    let mut top = content
        .y
        .saturating_add(u16::try_from(lines.len()).unwrap_or_default());
    for entry in entries {
        let target = entry.target();
        let drawn = sidebar_entry_lines(entry, width, now, focused, theme);
        let bottom = top.saturating_add(u16::try_from(drawn.len()).unwrap_or_default());
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
    (lines, rows)
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

/// One entry of the Sidebar's body: a Session on either shelf, or the rule
/// between the two.
fn sidebar_entry_lines(
    entry: SidebarEntry<'_>,
    width: usize,
    now: u64,
    focused: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match entry {
        SidebarEntry::Divider => vec![sidebar_divider_line(width, theme)],
        SidebarEntry::ShowMore(more) => vec![sidebar_show_more_line(more, width, focused, theme)],
        SidebarEntry::Scope(scope) => vec![sidebar_scope_line(&scope, width, focused, theme)],
        SidebarEntry::Row(row) => match row.shelf {
            SidebarShelf::Active {
                workspace,
                updated_at,
                working_since,
            } => sidebar_active_row_lines(
                row,
                workspace,
                sidebar_active_slot(working_since, updated_at, now),
                width,
                focused,
                theme,
            )
            .to_vec(),
            SidebarShelf::Settled { ended_at } => {
                vec![sidebar_settled_row_line(
                    row, ended_at, width, now, focused, theme,
                )]
            }
        },
    }
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
    focused: bool,
    theme: &Theme,
) -> Line<'static> {
    let affordance = if selector.open { "▾ " } else { "▸ " };
    Line::from(vec![
        sidebar_plain_span(
            &format!("{affordance}{}", selector.label),
            width.saturating_sub(ADD_WORKSPACE.width()),
            sidebar_selection_style(selector.selected, focused, theme),
            theme.text.subdued,
        ),
        sidebar_plain_span(
            ADD_WORKSPACE,
            ADD_WORKSPACE.width(),
            sidebar_selection_style(selector.adding, focused, theme),
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
/// A Workspace path as a frame says it. On Windows the canonical form — the
/// one the client launches with and the server roots Sessions at — is
/// verbatim (`\\?\C:\…`, `\\?\UNC\server\share\…`), a prefix no reader types
/// and no shell needs, so what is said drops it while the path everything
/// compares stays canonical. Everywhere else the display is the path.
fn legible_workspace(path: &Path) -> String {
    let spelled = path.to_string_lossy();
    if cfg!(windows)
        && let Some(dropped) = verbatim_prefix_dropped(&spelled)
    {
        return dropped;
    }
    spelled.into_owned()
}

/// The spelling with Windows's verbatim prefix dropped, and `None` where it
/// carries none. Split from [`legible_workspace`]'s platform gate so every
/// platform's tests exercise the dropping itself.
fn verbatim_prefix_dropped(spelled: &str) -> Option<String> {
    if let Some(share) = spelled.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{share}"));
    }
    spelled.strip_prefix(r"\\?\").map(str::to_owned)
}

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
    focused: bool,
    theme: &Theme,
) -> Line<'static> {
    sidebar_plain_line(
        &format!("  {}", scope.label),
        width,
        sidebar_selection_style(scope.selected, focused, theme),
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
    focused: bool,
    theme: &Theme,
) -> Line<'static> {
    sidebar_plain_line(
        &format!("Show {} more", more.count),
        width,
        sidebar_selection_style(more.selected, focused, theme),
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
/// right slot reads, then what the work is, then a line saying nothing until
/// the git awareness of <https://github.com/jake-tucker/suru/issues/169> gives
/// it something to say.
fn sidebar_active_row_lines(
    row: SidebarRow<'_>,
    workspace: Option<&Path>,
    slot: String,
    width: usize,
    focused: bool,
    theme: &Theme,
) -> [Line<'static>; sidebar::ACTIVE_ROW_LINES] {
    let selected = sidebar_selection_style(row.selected, focused, theme);
    let workspace = workspace.map(sidebar::workspace_name).unwrap_or_default();
    let label_style = selected.unwrap_or(theme.text.subdued);
    [
        sidebar_slotted_line(&workspace, label_style, &slot, width, selected, theme),
        sidebar_plain_line(
            &sidebar_title(row),
            width,
            None,
            sidebar_title_style(row, selected, theme),
        ),
        Line::styled(" ".repeat(width), selected.unwrap_or_default()),
    ]
}

/// One Session set aside, as the single slim line the settled shelf gives it:
/// what the work was, and how long ago it ended.
fn sidebar_settled_row_line(
    row: SidebarRow<'_>,
    ended_at: SessionTimestamp,
    width: usize,
    now: u64,
    focused: bool,
    theme: &Theme,
) -> Line<'static> {
    let selected = sidebar_selection_style(row.selected, focused, theme);
    sidebar_slotted_line(
        &sidebar_title(row),
        sidebar_title_style(row, selected, theme),
        &relative_update_time_compact(ended_at, now),
        width,
        selected,
        theme,
    )
}

/// What an active row's right slot reads. Working comes first, because live
/// work is what a reader scanning the column is looking for; under it stands
/// the compact time since the Session last moved, which is what a row says
/// when there is nothing louder to say.
///
/// t3 resolves this slot through a fuller precedence — Working, Monitoring,
/// Approval, Input, Failed, Woke, Done, then the time. The rest hang off state
/// Suru does not track yet, so each label lands here when its backing state
/// does: <https://github.com/jake-tucker/suru/issues/168>.
fn sidebar_active_slot(
    working_since: Option<SessionTimestamp>,
    updated_at: SessionTimestamp,
    now: u64,
) -> String {
    match working_since {
        Some(since) => format!("Working {}", working_duration(since, now)),
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

/// How the row the reader is on is drawn, and `None` for every other row. A
/// selected row is drawn whole, so it reads as one block rather than as lines
/// that happen to be lit. It keeps its highlight when the keys are elsewhere,
/// dimmed, because it is still the row Enter would act on once they come back.
fn sidebar_selection_style(selected: bool, focused: bool, theme: &Theme) -> Option<Style> {
    selected.then_some(if focused {
        theme.selection.focused
    } else {
        theme.selection.unfocused
    })
}

/// What a Session is called in the Sidebar: its Emoji, where it has one, and
/// then its Title.
fn sidebar_title(row: SidebarRow<'_>) -> String {
    match row.emoji {
        Some(emoji) => format!("{emoji} {}", row.title),
        None => row.title.to_owned(),
    }
}

/// How a Session's name is drawn: highlighted where the reader is on it,
/// accented where it is the Session they have open, and plain otherwise.
fn sidebar_title_style(row: SidebarRow<'_>, selected: Option<Style>, theme: &Theme) -> Style {
    selected.unwrap_or(if row.current {
        theme.accent.primary
    } else {
        theme.text.primary
    })
}

/// `text` with enough trailing spaces to fill `width` columns, so a line that
/// carries a background carries it the whole way across.
fn pad_to_width(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    padded.push_str(&" ".repeat(width.saturating_sub(text.width())));
    padded
}

fn render_landing(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let detail = ResponsiveDetail::for_width(area.width);
    let show_brand = area.height >= LANDING_BRAND_MINIMUM_HEIGHT;
    let footer_detail = detail.secondary_only_when(show_brand);
    let footer_width = area
        .width
        .saturating_sub(horizontal_padding(area.width).saturating_mul(2));
    let agent = agent_selection_context(state, footer_detail);
    let context = if footer_detail.shows_secondary() {
        format!(
            "{agent} · Workspace {}",
            legible_workspace(&state.workspace)
        )
    } else {
        agent
    };
    let notice = slots.landing_notice(&LandingNoticeSlotContext {
        width: footer_width,
        notice: state
            .landing_notice()
            .map(|notice| SlotText::new(notice.text(footer_width), notice.style(theme))),
    });
    let footer = slots.landing_footer(&LandingFooterSlotContext {
        width: footer_width,
        context: SlotText::new(context, theme.text.subdued),
        connection: SlotText::new(
            connection_status_text(state, footer_detail),
            status_style(state, theme),
        ),
    });
    let [notice_area, main, footer_area] = Layout::vertical([
        Constraint::Length(notice.height()),
        Constraint::Min(1),
        Constraint::Length(footer.height()),
    ])
    .areas(area);
    let content = horizontally_inset(main, horizontal_padding(area.width));
    let key = ComposerKey::Landing;
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let skill_markers = state.composers.skill_markers(key);
    let composer_height = composer_block_height(
        area.height,
        72_u16.min(content.width),
        composer_text,
        composer_cursor,
    );
    let error_height = u16::from(state.submission_error.is_some());
    let show_question = state.submission_error.is_none()
        || main.height
            >= composer_height
                .saturating_add(error_height)
                .saturating_add(1);
    let panel_height = composer_height
        .saturating_add(u16::from(show_question))
        .saturating_add(u16::from(show_brand))
        .saturating_add(error_height);
    let panel = centered_rect(content, 72, panel_height);
    let mut row = panel.y;
    if show_brand {
        frame.render_widget(
            Paragraph::new("Suru")
                .alignment(Alignment::Center)
                .style(theme.accent.primary.add_modifier(Modifier::BOLD)),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    if show_question {
        frame.render_widget(
            Paragraph::new("What would you like to work on?").alignment(Alignment::Center),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    if let Some(error) = &state.submission_error {
        frame.render_widget(
            Paragraph::new(error.as_str())
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
            text: composer_text,
            cursor: composer_cursor,
            skill_markers: &skill_markers,
        },
        state.composer_border_style(theme),
        detail,
        theme,
    ));

    render_slot(
        frame,
        horizontally_inset(notice_area, horizontal_padding(area.width)),
        notice,
        theme,
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

fn render_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let snapshot = state
        .session
        .as_ref()
        .expect("Session renderer requires a Session")
        .snapshot();
    let padding = horizontal_padding(area.width);
    let normally_padded = horizontally_inset(area, padding);
    let content_column =
        session_content_column(normally_padded, state.settings().session.content_width);
    let content_width = content_column.width;
    let content_detail = ResponsiveDetail::for_width(content_width);
    let header_detail = ResponsiveDetail::for_width(normally_padded.width);
    let show_header = area.height >= SESSION_HEADER_MINIMUM_HEIGHT;
    let session_id = snapshot.session.id;
    // A Subagent's Session is read rather than conversed with: the composer
    // stands down for a one-line way back, and Escape means leaving rather
    // than interrupting.
    let subagent_view = snapshot.session.parent.is_some();
    let key = ComposerKey::Session(session_id);
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let skill_markers = state.composers.skill_markers(key);
    let desired_composer_height = if subagent_view {
        1
    } else {
        composer_block_height(area.height, content_width, composer_text, composer_cursor)
    };
    let composer_top = slots.session_composer_top(&SessionComposerTopSlotContext {
        session_id,
        width: content_width,
    });
    let status = match snapshot.session.status {
        SessionStatus::Idle => "idle".to_owned(),
        // Escape leaves a Subagent's Session instead of interrupting it, so
        // its status offers no interrupt gesture to mislead with.
        SessionStatus::Active if subagent_view => {
            format!("{} active", spinner::frame(state.spinner_frame))
        }
        SessionStatus::Active => {
            let interrupt = binding_label(&CommandId::RequestInterrupt);
            let glyph = spinner::frame(state.spinner_frame);
            if matches!(
                state.command_mode,
                CommandMode::InterruptConfirmation { .. }
            ) {
                format!("{glyph} active · {interrupt} again to interrupt")
            } else {
                format!("{glyph} active · {interrupt} interrupt")
            }
        }
    };
    let activity_style = if snapshot.session.status == SessionStatus::Active {
        theme.feedback.warning
    } else {
        theme.text.subdued
    };
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
        (String::new(), activity_style)
    } else {
        (
            agent_selection_context(state, content_detail),
            activity_style,
        )
    };
    let footer = slots.prompt_footer(
        &PromptFooterSlotContext {
            session_id,
            width: content_width,
        },
        &PromptStatusSlotContext {
            session_id,
            status: SlotText::new(status, activity_style),
        },
        &PromptContextSlotContext {
            session_id,
            agent: SlotText::new(agent, agent_style),
            connection: SlotText::new(
                connection_status_text(state, ResponsiveDetail::CoreOnly),
                status_style(state, theme),
            ),
        },
    );
    let queued_prompts = state.queued_prompts(session_id);
    let desired_pending_height = if queued_prompts.is_empty() {
        0
    } else {
        (queued_prompts.len() as u16).min(3).saturating_add(2)
    };
    let core_height = u16::from(show_header)
        .saturating_add(desired_composer_height)
        .saturating_add(composer_top.height())
        .saturating_add(footer.height())
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
        .saturating_add(footer.height())
        .saturating_add(TRANSCRIPT_BOTTOM_MARGIN)
        .saturating_add(1);
    let composer_height =
        desired_composer_height.min(area.height.saturating_sub(reserved_height).max(1));
    let provisional_prompts = state.provisional_prompts(session_id);
    let interaction = state
        .session_interaction(session_id)
        .expect("Session interaction is initialized with its snapshot");
    let folds = interaction.folds.borrow();
    let groups = interaction.groups.borrow();
    let turns = interaction.turns.borrow();
    let transcript_view = state.transcript_cache.view(
        state.transcript_generation,
        snapshot,
        &provisional_prompts,
        TranscriptDisclosure {
            folds: &folds,
            groups: &groups,
            turns: &turns,
            reasoning_visibility: state.settings().transcript.reasoning_visibility,
        },
        theme,
        content_width,
    );
    let [_, transcript_without_latest, _, _, _, _, _, _] = session_areas(
        area,
        u16::from(show_header),
        pending_height,
        0,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    let viewport_without_latest = transcript_viewport_height(transcript_without_latest);
    let [_, transcript_with_latest, _, _, _, _, _, _] = session_areas(
        area,
        u16::from(show_header),
        pending_height,
        1,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    let viewport_with_latest = transcript_viewport_height(transcript_with_latest);
    let (away_from_bottom, viewport_height, maximum_scroll, scroll_position) =
        if interaction.follow_latest.get() {
            let maximum_scroll = transcript_view
                .row_count()
                .saturating_sub(viewport_without_latest);
            (
                false,
                viewport_without_latest,
                maximum_scroll,
                maximum_scroll,
            )
        } else {
            let viewport_height = viewport_with_latest;
            let maximum_scroll = transcript_view.row_count().saturating_sub(viewport_height);
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
                let maximum_scroll = transcript_view
                    .row_count()
                    .saturating_sub(viewport_without_latest);
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
        footer.height(),
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
    let mut window = transcript_view.window(scroll_position, usize::from(transcript_area.height));
    spinner::overlay_frame(
        &mut window.lines,
        &window.spinner_lines,
        state.spinner_frame,
    );
    let local_scroll = window
        .local_scroll
        .min(usize::from(u16::MAX.saturating_sub(transcript_area.height)))
        as u16;
    let has_top_border = transcript_area.height > 1;
    // The top border pushes projected rows down one, so a pointer maps back to
    // a transcript row through the same offset the widget draws with.
    let border_rows = u16::from(has_top_border);
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
    let transcript_widget = Paragraph::new(Text::from(window.lines)).wrap(Wrap { trim: false });
    let transcript_widget = if has_top_border {
        transcript_widget.block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme.border.subdued),
        )
    } else {
        transcript_widget
    };
    frame.render_widget(transcript_widget.scroll((local_scroll, 0)), transcript_area);
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
                text: composer_text,
                cursor: composer_cursor,
                skill_markers: &skill_markers,
            },
            state.composer_border_style(theme),
            content_detail,
            theme,
        ))
    };
    render_slot(frame, footer_area, footer, theme);
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

fn render_session_header(
    frame: &mut Frame<'_>,
    state: &TuiState,
    snapshot: &SessionSnapshot,
    area: Rect,
    detail: ResponsiveDetail,
    theme: &Theme,
) {
    let connection = connection_status_text(state, ResponsiveDetail::CoreOnly);
    let connection_width = connection.width().min(usize::from(area.width));
    let left_width = usize::from(area.width).saturating_sub(connection_width.saturating_add(2));
    let brand = truncate_to_width("Suru", left_width);
    let orientation_width = left_width.saturating_sub(brand.width());
    let orientation = if detail.shows_secondary() {
        truncate_to_width(
            &format!(
                " · Workspace {}",
                legible_workspace(&snapshot.session.workspace.path)
            ),
            orientation_width,
        )
    } else {
        String::new()
    };
    let spacing = " ".repeat(
        usize::from(area.width)
            .saturating_sub(brand.width() + orientation.width() + connection_width),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(brand, theme.accent.primary.add_modifier(Modifier::BOLD)),
            Span::styled(orientation, theme.text.subdued),
            Span::raw(spacing),
            Span::styled(connection, status_style(state, theme)),
        ])),
        area,
    );
}

fn agent_selection_context(state: &TuiState, detail: ResponsiveDetail) -> String {
    match state.agent_selection() {
        None => "Agent unavailable".to_owned(),
        Some(selection) => state
            .model_picker
            .selection_summary(selection, detail.shows_secondary()),
    }
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
    text: &'a str,
    cursor: usize,
    skill_markers: &'a ComposerSkillMarkers,
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    content: ComposerContent<'_>,
    style: Style,
    detail: ResponsiveDetail,
    theme: &Theme,
) -> Position {
    let ComposerContent {
        text,
        cursor,
        skill_markers,
    } = content;
    let submit = binding_label(&CommandId::SubmitSteer);
    let queue = binding_label(&CommandId::SubmitQueue);
    let newline = binding_label(&CommandId::InsertNewline);
    let title = if detail.shows_secondary() {
        format!(" Prompt · {submit} submit · {queue} queue · {newline} newline ")
    } else {
        " Prompt ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style);
    let content_width = area.width.saturating_sub(2).max(1);
    let content_height = area.height.saturating_sub(2).max(1);
    let (cursor_row, cursor_column) = visual_cursor_position(text, cursor, content_width);
    let scroll = cursor_row.saturating_sub(content_height.saturating_sub(1));
    let paragraph = if text.is_empty() {
        Paragraph::new(Span::styled(
            "Type a Prompt and press Enter",
            theme.form_field.placeholder,
        ))
    } else {
        Paragraph::new(wrapped_composer_lines(
            text,
            content_width,
            skill_markers,
            theme,
        ))
        .style(theme.form_field.text)
    };
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
    Position::new(
        area.x.saturating_add(1).saturating_add(cursor_column),
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
    let content_width = width.saturating_sub(2).max(1);
    let cursor_rows = visual_cursor_position(text, cursor, content_width)
        .0
        .saturating_add(1);
    let desired = visual_row_count(text, content_width)
        .max(cursor_rows)
        .max(1);
    let cap = (terminal_height / 3).max(1);
    desired.min(cap).saturating_add(2)
}

fn visual_row_count(text: &str, width: u16) -> u16 {
    visual_text_end(text, width).0.saturating_add(1)
}

fn visual_cursor_position(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = width.max(1);
    let (row, column) = visual_text_end(&text[..cursor], width);
    if column >= width {
        (row.saturating_add(1), 0)
    } else {
        (row, column)
    }
}

fn visual_text_end(text: &str, width: u16) -> (u16, u16) {
    let width = width.max(1);
    let mut row = 0_u16;
    let mut column = 0_u16;
    for character in text.chars() {
        if character == '\n' {
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if column > 0 && column.saturating_add(character_width) > width {
            row = row.saturating_add(1);
            column = 0;
        }
        column = column.saturating_add(character_width);
    }
    (row, column)
}

fn wrapped_composer_lines(
    text: &str,
    width: u16,
    skill_markers: &ComposerSkillMarkers,
    theme: &Theme,
) -> Text<'static> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = Vec::<(Style, String)>::new();
    let mut line_width = 0_u16;
    for (offset, character) in text.char_indices() {
        if character == '\n' {
            lines.push(styled_composer_line(std::mem::take(&mut line)));
            line_width = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if line_width > 0 && line_width.saturating_add(character_width) > width {
            lines.push(styled_composer_line(std::mem::take(&mut line)));
            line_width = 0;
        }
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
        line_width = line_width.saturating_add(character_width);
    }
    lines.push(styled_composer_line(line));
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
    let rows = slot
        .failures
        .into_iter()
        .map(|failure| {
            Line::styled(
                format!("Extension error · {}: {}", failure.slot, failure.message),
                theme.feedback.error,
            )
        })
        .chain(slot.content)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows), area);
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
    let connection = match &state.identity {
        Some(identity) => format!("Connected | {}", server_identity_text(identity)),
        None => "Connecting to Suru server...".to_owned(),
    };
    if matches!(
        state
            .session
            .as_ref()
            .map(|session| session.snapshot().session.status),
        Some(SessionStatus::Active)
    ) {
        let interrupt = binding_label(&CommandId::RequestInterrupt);
        let active = if matches!(
            state.command_mode,
            CommandMode::InterruptConfirmation { .. }
        ) {
            format!("Active · {interrupt} again to interrupt")
        } else {
            format!("Active · {interrupt} interrupt")
        };
        return format!("{active} | {connection}");
    }
    connection
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

    use super::super::{
        slots::{Placement, RenderSlots, TestContribution},
        state::{Application, ApplicationEvent},
    };
    use super::{legible_workspace, verbatim_prefix_dropped};
    use crate::{
        managed_client::{ManagedEvent, SessionEvent},
        protocol::{
            EffectiveSettings, ModelAvailability, Session, SessionContentWidth, SessionId,
            SessionRevision, SessionSettings, SessionSnapshot, SessionStatus, SettingsSnapshot,
            Workspace,
        },
    };

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

    /// The canonical form a client and the server hold on Windows is verbatim,
    /// and the prefix is display noise: no reader types it and no shell needs
    /// it, so what a frame says drops it while the path everything compares
    /// stays canonical.
    #[test]
    fn a_windows_verbatim_prefix_is_dropped_from_what_a_frame_says() {
        assert_eq!(
            verbatim_prefix_dropped(r"\\?\C:\Users\reader\suru"),
            Some(r"C:\Users\reader\suru".to_owned()),
            "a canonical drive path reads as the path a reader would type"
        );
        assert_eq!(
            verbatim_prefix_dropped(r"\\?\UNC\server\share\suru"),
            Some(r"\\server\share\suru".to_owned()),
            "a canonical UNC path reads as the share a reader would type"
        );
        assert_eq!(
            verbatim_prefix_dropped(r"C:\Users\reader\suru"),
            None,
            "a path with nothing to drop is left to read as it is"
        );
    }

    /// Only Windows canonicalizes into verbatim form, so only there is the
    /// prefix dropped: on every other platform a leading `\\?\` is just a
    /// strange directory name, and the display is the path.
    #[test]
    fn a_workspace_reads_verbatim_everywhere_the_canonical_form_is_not() {
        let spelled = if cfg!(windows) {
            r"\\?\C:\Users\reader\suru"
        } else {
            "/home/reader/suru"
        };
        let expected = if cfg!(windows) {
            r"C:\Users\reader\suru"
        } else {
            "/home/reader/suru"
        };
        assert_eq!(legible_workspace(std::path::Path::new(spelled)), expected);
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
    fn the_landing_notice_slot_takes_contributions_even_when_startup_was_clean() {
        let slots = RenderSlots::testing([
            TestContribution::landing_notice(Placement::Prepend, Ok("notice from an extension")),
            TestContribution::landing_notice(Placement::Append, Err("notice failed")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };

        let rows = rendered_rows(&application);
        let failure = rows
            .iter()
            .position(|row| row.contains("Extension error · landing.notice"))
            .unwrap();
        let notice = rows
            .iter()
            .position(|row| row.contains("notice from an extension"))
            .unwrap();
        let question = rows
            .iter()
            .position(|row| row.contains("What would you like to work on?"))
            .unwrap();

        assert!(
            failure < notice,
            "a failed contribution reports above the slot"
        );
        assert!(
            notice < question,
            "the Notice sits above the Landing rather than over it"
        );
    }

    #[test]
    fn session_slots_render_with_their_typed_session_context() {
        let session_id = SessionId::new();
        let slots = RenderSlots::testing([
            TestContribution::session_composer_top(Placement::Append, Ok("composer top")),
            TestContribution::prompt_footer_status(Placement::Prepend, Ok("status extension"))
                .styled(Style::default().fg(Color::LightMagenta)),
            TestContribution::prompt_footer_status(Placement::Append, Err("status failed")),
            TestContribution::prompt_footer_context(Placement::Append, Ok("context extension")),
            TestContribution::prompt_footer(Placement::Append, Ok("footer extension")),
        ]);
        let mut application = Application {
            slots,
            ..Application::default()
        };
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
                SessionSnapshot {
                    session: Session {
                        id: session_id,
                        workspace: Workspace {
                            path: PathBuf::from("/workspace"),
                        },
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        status: SessionStatus::Idle,
                        parent: None,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: Vec::new(),
                    turns: Vec::new(),
                    messages: Vec::new(),
                    activities: Vec::new(),
                    transcript: Vec::new(),
                },
            )))
            .expect("hydrate test Application");

        let buffer = rendered_buffer(&application);
        let screen = rendered_rows_from_buffer(&buffer).join("\n");
        assert!(screen.contains("composer top"));
        assert!(screen.contains("status extension · idle"));
        assert!(screen.contains("Agent unavailable · context extension"));
        assert!(screen.contains("footer extension"));
        assert!(screen.contains("Extension error · prompt.footer.status: status failed"));
        assert_eq!(
            text_cell(&buffer, "status extension").fg,
            Color::LightMagenta
        );
        assert_ne!(text_cell(&buffer, "idle").fg, Color::LightMagenta);
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
                        ..EffectiveSettings::default()
                    },
                    pinned: vec!["session.contentWidth".to_owned()],
                    diagnostics: Vec::new(),
                },
            )))
            .expect("receive Session content width");
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
                SessionSnapshot {
                    session: Session {
                        id: session_id,
                        workspace: Workspace {
                            path: PathBuf::from("/workspace"),
                        },
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        status: SessionStatus::Idle,
                        parent: None,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: Vec::new(),
                    turns: Vec::new(),
                    messages: Vec::new(),
                    activities: Vec::new(),
                    transcript: Vec::new(),
                },
            )))
            .expect("hydrate test Application");

        let screen = rendered_rows(&application).join("\n");
        assert!(screen.contains("composer extension width 60"));
        assert!(screen.contains("footer extension width 60"));
    }
}
