//! Frame rendering: the landing and Session screens, the pickers and overlays,
//! and the shared text and layout helpers they draw with.

use std::time::{SystemTime, UNIX_EPOCH};

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
        ModelAvailability, ModelDescriptor, ServerIdentity, SessionSnapshot, SessionStatus,
        SessionTimestamp,
    },
    theme::Theme,
};

use super::{
    composer::ComposerKey,
    keymap::binding_label,
    model_options::ModelOptionChoiceRow,
    model_picker::ModelPickerRow,
    session_picker::SessionPickerRow,
    settings_panel::{RowExpansion, RowValue},
    slots::{
        LandingFooterSlotContext, LandingNoticeSlotContext, PromptContextSlotContext,
        PromptFooterSlotContext, PromptStatusSlotContext, RenderSlots, RenderedSlot,
        SessionComposerTopSlotContext, SlotText, truncate_to_width,
    },
    spinner,
    state::{CommandId, CommandMode, QueuedPrompt, TranscriptViewport, TuiState},
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
    if terminal_is_too_small(frame.area()) {
        render_terminal_too_small(frame, &theme);
        return;
    }
    let composer = if state.session.is_some() {
        render_session(frame, state, slots, &theme)
    } else {
        render_landing(frame, state, slots, &theme)
    };
    if state.command_autocomplete.is_visible() && !state.reconnect_overlay_visible {
        render_command_autocomplete(frame, state, composer.area, &theme);
    }
    if state.session_picker.is_open() && !state.reconnect_overlay_visible {
        render_session_picker(frame, state, &theme);
    }
    if state.model_picker.is_open() && !state.reconnect_overlay_visible {
        render_model_picker(frame, state, &theme);
    }
    if state.model_options.is_open() && !state.reconnect_overlay_visible {
        render_model_options(frame, state, &theme);
    }
    if state.settings_panel.is_open() && !state.reconnect_overlay_visible {
        render_settings_panel(frame, state, &theme);
    }
    if state.reconnect_overlay_visible {
        render_reconnect_overlay(frame, &theme);
    } else if !state.session_picker.is_open()
        && !state.model_picker.is_open()
        && !state.model_options.is_open()
        && !state.settings_panel.is_open()
        && state.composer_focused
        && matches!(state.command_mode, CommandMode::Composer)
    {
        frame.set_cursor_position(composer.cursor);
    }
}

fn render_session_picker(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(72),
        frame.area().height.saturating_sub(2).min(12),
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
        let now = current_time_millis();
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

fn render_model_picker(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(76),
        frame.area().height.saturating_sub(2).min(14),
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

/// The settings panel: the tab bar, the rows of the tab being shown, what each
/// is worth right now, and whether that value is the reader's own pin or the
/// built-in default. Rows come straight from the latest effective-settings
/// snapshot, so an edit moves a row only once the server has answered for it.
fn render_settings_panel(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let rows = state
        .settings_panel
        .rows(state.settings(), state.pinned_settings());
    // A tab lists a known number of rows, so the panel is exactly as tall as it
    // needs to be: two borders around the tab bar and the headline, one row per
    // Setting, and the controls.
    let wanted = u16::try_from(rows.len().saturating_add(5)).unwrap_or(u16::MAX);
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(76),
        frame.area().height.saturating_sub(2).min(wanted),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let content_height = usize::from(area.height.saturating_sub(2));
    let mut lines = Vec::with_capacity(content_height);
    // Both tabs, always, so the reader can see what the panel holds without
    // visiting it; the active one is drawn as the accent. A box too short for
    // its own content gives this line up first, because a tab bar over no rows
    // says nothing about the Settings the reader came for.
    if content_height >= 4 {
        let mut spans = Vec::new();
        for tab in state.settings_panel.tabs() {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                tab.title,
                if tab.active {
                    theme.accent.primary.add_modifier(Modifier::BOLD)
                } else {
                    theme.text.subdued
                },
            ));
        }
        lines.push(Line::from(spans));
    }
    if content_height >= 2 {
        // What the focused Setting does, or why the last edit of it never
        // reached the Config Document — a failed edit is the more urgent of
        // the two, so it takes the line.
        let (headline, style) = match (
            state.settings_panel.error(),
            state.settings_panel.selected_descriptor(),
        ) {
            (Some(error), _) => (error.to_owned(), theme.feedback.error),
            (None, Some(descriptor)) => (
                format!("{} · {}", descriptor.key, descriptor.description),
                theme.text.subdued,
            ),
            (None, None) => (String::new(), theme.text.subdued),
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
    // worth teaching.
    let expands = rows
        .iter()
        .any(|row| row.selected && row.expansion.expands());
    for row in visible_window(rows, selected, capacity) {
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
        let value = match row.value {
            RowValue::Choice(value) => format!(" · {value}"),
            RowValue::ProviderEnabled => String::new(),
            RowValue::ProviderDisabled => " · disabled".to_owned(),
        };
        lines.push(Line::styled(
            truncate_to_width(
                &format!("{marker}{expansion}{}{value} [{origin}]", row.label),
                content_width,
            ),
            match (row.selected, row.value) {
                (true, _) => theme.selection.focused,
                (false, RowValue::ProviderDisabled) => theme.text.subdued,
                (false, _) => theme.text.primary,
            },
        ));
    }
    if footer_rows > 0 && lines.len() < content_height {
        // Enter is taught only where it does something, so a reader focused on
        // a row that does not expand is never offered a dead key.
        let expand = if expands { "Enter expand · " } else { "" };
        lines.push(Line::styled(
            truncate_to_width(
                &format!("{expand}Left/Right tabs · Space change · Ctrl+D reset · Esc close"),
                content_width,
            ),
            theme.text.subdued,
        ));
    }
    render_overlay_box(frame, area, lines, " Settings ", theme);
}

fn render_model_options(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(76),
        frame.area().height.saturating_sub(2).min(14),
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
    let start = selected.saturating_add(1).saturating_sub(capacity);
    rows.into_iter().skip(start).take(capacity)
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
    let marker_width = marker.width();
    let available = width.saturating_sub(marker_width);
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
                workspace.to_string_lossy().as_ref(),
                path_budget,
            ));
        }
    }
    let metadata = metadata.join(separator);
    let title_width = available
        .saturating_sub(metadata.width())
        .saturating_sub(separator.width());
    let title = truncate_to_width(row.title, title_width);
    truncate_to_width(&format!("{marker}{title}{separator}{metadata}"), width)
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

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
    cursor: Position,
}

fn render_command_autocomplete(
    frame: &mut Frame<'_>,
    state: &TuiState,
    composer_area: Rect,
    theme: &Theme,
) {
    let available_width = frame
        .area()
        .width
        .saturating_sub(horizontal_padding(frame.area().width).saturating_mul(2));
    let width = available_width.clamp(1, 72);
    let room_above = composer_area.y.saturating_sub(frame.area().y);
    let bordered = room_above >= 3;
    let row_count = state.command_autocomplete.rows().len() as u16;
    let height = if bordered {
        row_count.saturating_add(2).min(room_above)
    } else {
        row_count.min(room_above.max(1))
    };
    let row_capacity = height.saturating_sub(if bordered { 2 } else { 0 });
    let x = frame
        .area()
        .x
        .saturating_add(frame.area().width.saturating_sub(width) / 2);
    let y = composer_area.y.saturating_sub(height).max(frame.area().y);
    let area = Rect::new(x, y, width, height);
    let content_width = width.saturating_sub(if bordered { 2 } else { 0 });
    let rows = state
        .command_autocomplete
        .visible_rows(usize::from(row_capacity))
        .map(|(selected, command)| {
            let slash = command
                .slash
                .expect("autocomplete only contains commands with slash metadata");
            let content = truncate_to_width(
                &format!(
                    "/{}  {} · {}",
                    slash.name, command.title, command.description
                ),
                usize::from(content_width),
            );
            Line::styled(
                content,
                if selected {
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
                .title(" Commands ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        )
    } else {
        Paragraph::new(rows).style(theme.surface.overlay)
    };
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn render_landing(
    frame: &mut Frame<'_>,
    state: &TuiState,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let detail = ResponsiveDetail::for_width(frame.area().width);
    let show_brand = frame.area().height >= LANDING_BRAND_MINIMUM_HEIGHT;
    let footer_detail = detail.secondary_only_when(show_brand);
    let footer_width = frame
        .area()
        .width
        .saturating_sub(horizontal_padding(frame.area().width).saturating_mul(2));
    let agent = agent_selection_context(state, footer_detail);
    let context = if footer_detail.shows_secondary() {
        format!("{agent} · Workspace {}", state.workspace.to_string_lossy())
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
    .areas(frame.area());
    let content = horizontally_inset(main, horizontal_padding(frame.area().width));
    let key = ComposerKey::Landing;
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let composer_height = composer_block_height(
        frame.area().height,
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
    let cursor = render_composer(
        frame,
        composer_area,
        composer_text,
        composer_cursor,
        state.composer_border_style(theme),
        detail,
        theme,
    );

    render_slot(
        frame,
        horizontally_inset(notice_area, horizontal_padding(frame.area().width)),
        notice,
        theme,
    );
    render_slot(
        frame,
        horizontally_inset(footer_area, horizontal_padding(frame.area().width)),
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
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let snapshot = state
        .session
        .as_ref()
        .expect("Session renderer requires a Session")
        .snapshot();
    let detail = ResponsiveDetail::for_width(frame.area().width);
    let padding = horizontal_padding(frame.area().width);
    let show_header = frame.area().height >= SESSION_HEADER_MINIMUM_HEIGHT;
    let session_id = snapshot.session.id;
    let key = ComposerKey::Session(session_id);
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let content_width = frame.area().width.saturating_sub(padding.saturating_mul(2));
    let desired_composer_height = composer_block_height(
        frame.area().height,
        content_width,
        composer_text,
        composer_cursor,
    );
    let composer_top = slots.session_composer_top(&SessionComposerTopSlotContext { session_id });
    let status = match snapshot.session.status {
        SessionStatus::Idle => "idle".to_owned(),
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
    } else if snapshot.session.status == SessionStatus::Active && !detail.shows_secondary() {
        (String::new(), activity_style)
    } else {
        (agent_selection_context(state, detail), activity_style)
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
    let pending_room = frame.area().height.saturating_sub(core_height);
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
        desired_composer_height.min(frame.area().height.saturating_sub(reserved_height).max(1));
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
        frame.area(),
        u16::from(show_header),
        pending_height,
        0,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    let viewport_without_latest = transcript_viewport_height(transcript_without_latest);
    let [_, transcript_with_latest, _, _, _, _, _, _] = session_areas(
        frame.area(),
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
        frame.area(),
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
            detail,
            theme,
        );
    }

    let transcript_area = horizontally_inset(transcript_area, padding);
    let pending_area = horizontally_inset(pending_area, padding);
    let latest_area = horizontally_inset(latest_area, padding);
    let composer_top_area = horizontally_inset(composer_top_area, padding);
    let composer_area = horizontally_inset(composer_area, padding);
    let footer_area = horizontally_inset(status_area, padding);
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
        render_pending_prompts(frame, pending_area, state, &queued_prompts, detail, theme);
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
    let cursor = render_composer(
        frame,
        composer_area,
        composer_text,
        composer_cursor,
        state.composer_border_style(theme),
        detail,
        theme,
    );
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
                snapshot.session.workspace.path.to_string_lossy()
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

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    cursor: usize,
    style: Style,
    detail: ResponsiveDetail,
    theme: &Theme,
) -> Position {
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
        Paragraph::new(wrapped_composer_lines(text, content_width)).style(theme.form_field.text)
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

fn wrapped_composer_lines(text: &str, width: u16) -> Text<'static> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0_u16;
    for character in text.chars() {
        if character == '\n' {
            lines.push(Line::from(std::mem::take(&mut line)));
            line_width = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if line_width > 0 && line_width.saturating_add(character_width) > width {
            lines.push(Line::from(std::mem::take(&mut line)));
            line_width = 0;
        }
        line.push(character);
        line_width = line_width.saturating_add(character_width);
    }
    lines.push(Line::from(line));
    Text::from(lines)
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
    use crate::{
        managed_client::SessionEvent,
        protocol::{
            ModelAvailability, Session, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
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
}
