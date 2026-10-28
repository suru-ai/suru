//! Terminal input mapping: the keybinding tables and the per-mode functions
//! that translate a terminal event into a [`CommandId`].

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Position;

use super::{
    commands::{
        NumericDigit, SemanticCommandId, command_for_direct_semantic_key, command_for_leader_key,
        descriptor,
    },
    connect_overlay::ConnectInputMode,
    metrics::{
        FocusedRow, MetricsTab, MetricsTarget,
        calendar::MetricsRange,
        chart::MetricsMeasure,
        insights::{InsightsPanel, InsightsTarget},
    },
    relay_overlay::RelayInputMode,
    settings_panel::PanelKeys,
    state::{CommandId, ScrollDirection},
};

/// The terminal's own table: the composer's keys, and the pointer as every
/// surface that has no use of its own for it answers it (see
/// [`command_for_pointer`]).
pub fn command_for_terminal_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Mouse(mouse) => command_for_pointer(mouse),
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) if binding_for(key).is_some() => {
            binding_for(key).map(|binding| binding.command.clone())
        }
        InputEvent::Key(key) if command_for_direct_semantic_key(key).is_some() => {
            command_for_direct_semantic_key(key).map(CommandId::InvokeSemantic)
        }
        InputEvent::Key(key)
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            match key.code {
                KeyCode::Char(character) => Some(CommandId::InsertText(character.to_string())),
                _ => None,
            }
        }
        // Windows Terminal before 1.25 answers Ctrl+V over an image-only
        // clipboard with an empty bracketed paste, so an empty paste reads the
        // clipboard itself.
        InputEvent::Paste(text) if text.is_empty() => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ComposerClipboardPaste,
        )),
        InputEvent::Paste(text) => Some(CommandId::PasteText(text)),
        _ => None,
    }
}

/// The pointer, carried with the cell it stood on, because every gesture it
/// makes is resolved against the frame's geometry rather than against whoever
/// has the keys: a press lands on what is drawn under it, and the wheel moves
/// whichever list it is over — the Directory Browser's tree over the browser,
/// the Sidebar's over the Sidebar, the Transcript anywhere else.
fn command_for_pointer(mouse: MouseEvent) -> Option<CommandId> {
    let position = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp => Some(CommandId::WheelAt {
            position,
            direction: ScrollDirection::Up,
        }),
        MouseEventKind::ScrollDown => Some(CommandId::WheelAt {
            position,
            direction: ScrollDirection::Down,
        }),
        MouseEventKind::Up(MouseButton::Left) => Some(CommandId::ClickAt { position }),
        MouseEventKind::Down(MouseButton::Right) => Some(CommandId::OpenContextMenuAt { position }),
        _ => None,
    }
}

/// A Subagent's Session is read rather than conversed with, so its view keeps
/// only the reading keys: the pointer works as it does anywhere, the wheel
/// included, Escape returns to the parent Session, and the composer's keys —
/// text entry, history, submission, the interrupt Escape would otherwise mean
/// — reach nothing, which is what keeps Prompt delivery out of the view.
/// Ctrl+B keeps the Sidebar, so the reader can leave for any Session, and
/// Ctrl+C still exits. A Subagent's Session that is only Monitoring answers
/// Escape with [`command_for_monitoring_subagent_view_event`] instead.
pub(super) fn command_for_subagent_view_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Mouse(mouse) => command_for_pointer(mouse),
        InputEvent::Key(key) if key.kind == KeyEventKind::Press => {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, KeyModifiers::NONE) => {
                    Some(CommandId::InvokeSemantic(SemanticCommandId::SubagentLeave))
                }
                // Down has no composer to serve here, so it carries its one
                // remaining meaning: browsing the Subagents this Session has
                // working, which is how a reader walks a subtree level by
                // level.
                (KeyCode::Down, KeyModifiers::NONE) => {
                    Some(CommandId::InvokeSemantic(SemanticCommandId::SubagentBrowse))
                }
                (KeyCode::PageUp, KeyModifiers::NONE) => Some(CommandId::ScrollTranscriptPageUp),
                (KeyCode::PageDown, KeyModifiers::NONE) => {
                    Some(CommandId::ScrollTranscriptPageDown)
                }
                (KeyCode::End, KeyModifiers::CONTROL) => Some(CommandId::FollowLatest),
                (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                    Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarToggle))
                }
                // The Leader reaches the Aside, the way around this tree.
                (KeyCode::Char('x'), KeyModifiers::CONTROL) => Some(CommandId::BeginLeader),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(CommandId::ClearOrExit),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A Subagent's Session that is only Monitoring keeps the Subagent view's
/// keys, except that Escape arms stopping its Watches, as it does in any
/// Monitoring Session, rather than leaving.
pub(super) fn command_for_monitoring_subagent_view_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Key(key)
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Esc
                && key.modifiers == KeyModifiers::NONE =>
        {
            Some(CommandId::RequestInterrupt)
        }
        event => command_for_subagent_view_event(event),
    }
}

/// The Leader inside a Subagent's Session. That view is read, never prompted,
/// so the one Leader command it answers is the Aside's show/hide act — the way
/// around the tree the reader is in. Every other Leader command, choosing a
/// Model or beginning a Session among them, stays out of reach and the
/// chord simply ends.
pub(super) fn command_for_subagent_view_leader_event(event: InputEvent) -> Option<CommandId> {
    match command_for_leader_event(event) {
        command @ Some(CommandId::InvokeSemantic(SemanticCommandId::AsideToggle)) => command,
        _ => Some(CommandId::CloseCommandMode),
    }
}

/// The Icon Picker is the newest thing on screen while it is up, so it has
/// the keys: the arrows walk its grid in two dimensions, Enter chooses the
/// focused glyph, and Esc puts the picker away leaving its target's Icon
/// unchanged. Anything else typed narrows the grid by name and keyword, as
/// the Workspace Picker's own search does, and the pointer answers as it does
/// everywhere else — a press outside the grid is handled the way a press
/// outside the Subagent Picker is.
pub(super) fn command_for_icon_picker_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::IconPickerSearchInsert,
                text,
            ));
        }
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Left, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::IconPickerLeft))
        }
        (KeyCode::Right, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::IconPickerRight,
        )),
        (KeyCode::Up, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::IconPickerUp))
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::IconPickerDown))
        }
        (KeyCode::Enter, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::IconPickerChoose,
        )),
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::IconPickerClose,
        )),
        (KeyCode::Backspace, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::IconPickerSearchDelete,
        )),
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            Some(CommandId::InvokeSemanticText(
                SemanticCommandId::IconPickerSearchInsert,
                character.to_string(),
            ))
        }
        _ => None,
    }
}

/// The Subagent Picker is the newest thing on screen while it is up, so it
/// has the keys: the arrows walk its entries, Enter opens the Session of the
/// one the reader is on, `x` stops it where the Provider allows — at once,
/// because interrupting never asks — and Esc puts the picker away leaving
/// everything beneath it exactly as it was. Any other letter typed at it
/// reaches nothing — the picker offers the working Subagents, not a search —
/// and the mouse answers as it does everywhere else, because a press outside
/// the picker is how a reader dismisses one.
pub(super) fn command_for_subagent_picker_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectPreviousSubagent)
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectNextSubagent)
        }
        (KeyCode::Enter, KeyModifiers::NONE) => Some(CommandId::OpenSelectedSubagent),
        (KeyCode::Char('x'), KeyModifiers::NONE) => Some(CommandId::StopSelectedSubagent),
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::CloseSubagentPicker),
        _ => None,
    }
}

pub(super) fn command_for_approval_posture_picker_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let command = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ApprovalPosturePrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ApprovalPostureNext
        }
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::ApprovalPostureSelect,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::ApprovalPostureClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// The Context overlay owns the keys while it is visible: it only scrolls,
/// and Esc or Enter dismisses it.
pub(super) fn command_for_context_overlay_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let command = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ContextScrollUp
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ContextScrollDown
        }
        (KeyCode::PageUp, KeyModifiers::NONE) => SemanticCommandId::ContextPageUp,
        (KeyCode::PageDown, KeyModifiers::NONE) => SemanticCommandId::ContextPageDown,
        (KeyCode::Esc | KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::ContextClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// What the Usage Metrics view's keys act on, beside the key itself.
#[derive(Clone, Copy, Debug)]
pub(super) struct MetricsKeys {
    pub(super) tab: MetricsTab,
    /// Whether Overview has a bucket selected.
    pub(super) bucket_selected: bool,
    /// The row of Overview's lists in focus.
    pub(super) focused_row: Option<FocusedRow>,
    /// The Insights panel in focus.
    pub(super) panel: Option<InsightsPanel>,
    /// Whether that panel has something selected within it.
    pub(super) panel_selected: bool,
}

/// The Usage Metrics view owns every key and click while it is open, since
/// it covers the whole frame. On either tab, Ctrl+←/→, `g` for Overview and
/// `i` for Insights — which work where the system takes Ctrl+←/→, as macOS
/// does for Spaces — or a click in the tab bar switch tab, `r` reads the tab
/// showing again, `1`–`5` or a click in
/// the range bar choose the range both tabs share, `[`/`]` step to the
/// range before or after, a click on its ✕ closes it, and Ctrl+C leaves
/// Suru, as it does from a composer with nothing in it — the view holds
/// nothing a press could clear, and a draft it covers is the reader's to
/// come back to. Everything else is the tab's own: see
/// [`command_for_overview_key`], [`command_for_overview_pointer`],
/// [`command_for_insights_key`], and [`command_for_insights_pointer`].
pub(super) fn command_for_metrics_view_event(
    event: InputEvent,
    keys: MetricsKeys,
    target_at: impl Fn(Position) -> Option<MetricsTarget>,
) -> Option<CommandId> {
    let command = match event {
        InputEvent::Key(key) if key.kind == KeyEventKind::Press => {
            match (key.code, key.modifiers) {
                (KeyCode::Left, KeyModifiers::CONTROL) => SemanticCommandId::MetricsTabPrevious,
                (KeyCode::Right, KeyModifiers::CONTROL) => SemanticCommandId::MetricsTabNext,
                (KeyCode::Char('g'), KeyModifiers::NONE) => {
                    SemanticCommandId::MetricsTabSelect(MetricsTab::Overview)
                }
                (KeyCode::Char('i'), KeyModifiers::NONE) => {
                    SemanticCommandId::MetricsTabSelect(MetricsTab::Insights)
                }
                (KeyCode::Char('r'), KeyModifiers::NONE) => SemanticCommandId::MetricsRefresh,
                (KeyCode::Char('['), KeyModifiers::NONE) => SemanticCommandId::MetricsRangePrevious,
                (KeyCode::Char(']'), KeyModifiers::NONE) => SemanticCommandId::MetricsRangeNext,
                (KeyCode::Char(digit @ '1'..='5'), KeyModifiers::NONE) => {
                    let place = usize::try_from(digit.to_digit(10)?).ok()? - 1;
                    SemanticCommandId::MetricsRangeSelect(MetricsRange::ALL[place])
                }
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => SemanticCommandId::ApplicationExit,
                _ => match keys.tab {
                    MetricsTab::Overview => command_for_overview_key(key, keys)?,
                    MetricsTab::Insights => command_for_insights_key(key, keys)?,
                },
            }
        }
        InputEvent::Mouse(mouse) => {
            let target = target_at(Position::new(mouse.column, mouse.row))?;
            // A click arrives as the release that completes it.
            let click = mouse.kind == MouseEventKind::Up(MouseButton::Left);
            match target {
                MetricsTarget::Close if click => SemanticCommandId::MetricsClose,
                MetricsTarget::Tab(tab) if click => SemanticCommandId::MetricsTabSelect(tab),
                MetricsTarget::Range(range) if click => {
                    SemanticCommandId::MetricsRangeSelect(range)
                }
                MetricsTarget::Insights(target) => command_for_insights_pointer(mouse, target)?,
                target => command_for_overview_pointer(mouse, target)?,
            }
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// Overview's own keys: ←/→ move a bucket selection and Home/End take it to
/// either end, as a click on a bar or a drag across them does; ↑/↓ focus a
/// row of its lists, on which Space hides or shows a legend row's series and
/// Shift+Space or `o` shows it alone, as a click on the row or a modified
/// one does, and Enter opens a listed Session, as a click on its row does;
/// Esc clears the bucket selection, as a click on the chart away from its
/// bars does, then the focus, and with neither left closes the view — the
/// selection first, since it changes what the legend reads, where a focus
/// only marks a row — `c`/`t`/`s` or a click on a measure choose what the
/// chart plots and Tab the next, and `b` splits the bars by the next
/// breakdown and a click on one by that.
fn command_for_overview_key(key: KeyEvent, keys: MetricsKeys) -> Option<SemanticCommandId> {
    let focused = keys.focused_row;
    Some(match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) if keys.bucket_selected => {
            SemanticCommandId::MetricsBucketClear
        }
        (KeyCode::Esc, KeyModifiers::NONE) if focused.is_some() => {
            SemanticCommandId::MetricsFocusClear
        }
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::MetricsClose,
        (KeyCode::Left, KeyModifiers::NONE) => SemanticCommandId::MetricsBucketPrevious,
        (KeyCode::Right, KeyModifiers::NONE) => SemanticCommandId::MetricsBucketNext,
        (KeyCode::Up, KeyModifiers::NONE) => SemanticCommandId::MetricsFocusPrevious,
        (KeyCode::Down, KeyModifiers::NONE) => SemanticCommandId::MetricsFocusNext,
        (KeyCode::Char(' '), KeyModifiers::NONE) => match focused? {
            FocusedRow::Series(series) => SemanticCommandId::MetricsSeriesToggle { series },
            FocusedRow::Session(_) => return None,
        },
        (KeyCode::Enter, KeyModifiers::NONE) => match focused? {
            FocusedRow::Session(session) => SemanticCommandId::MetricsSessionOpen { session },
            FocusedRow::Series(_) => return None,
        },
        // Most terminals send Shift+Space as Space alone, so `o`, for
        // "only", does the same wherever Shift+Space cannot.
        (KeyCode::Char(' '), KeyModifiers::SHIFT) | (KeyCode::Char('o'), KeyModifiers::NONE) => {
            match focused? {
                FocusedRow::Series(series) => SemanticCommandId::MetricsSeriesSolo { series },
                FocusedRow::Session(_) => return None,
            }
        }
        (KeyCode::Home, KeyModifiers::NONE) => SemanticCommandId::MetricsBucketFirst,
        (KeyCode::End, KeyModifiers::NONE) => SemanticCommandId::MetricsBucketLast,
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Cost)
        }
        (KeyCode::Char('t'), KeyModifiers::NONE) => {
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Tokens)
        }
        (KeyCode::Char('s'), KeyModifiers::NONE) => {
            SemanticCommandId::MetricsMeasureSelect(MetricsMeasure::Sessions)
        }
        (KeyCode::Tab, KeyModifiers::NONE) => SemanticCommandId::MetricsMeasureNext,
        (KeyCode::Char('b'), KeyModifiers::NONE) => SemanticCommandId::MetricsBreakdownNext,
        _ => return None,
    })
}

/// Overview's own clicks, drags, and wheel, over the measures, breakdowns,
/// chart, legend, and Sessions it drew.
fn command_for_overview_pointer(
    mouse: MouseEvent,
    target: MetricsTarget,
) -> Option<SemanticCommandId> {
    Some(match (mouse.kind, target) {
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Measure(measure)) => {
            SemanticCommandId::MetricsMeasureSelect(measure)
        }
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Breakdown(breakdown)) => {
            SemanticCommandId::MetricsBreakdownSelect(breakdown)
        }
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Bar(bucket)) => {
            SemanticCommandId::MetricsBucketSelect { bucket }
        }
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Chart { .. }) => {
            SemanticCommandId::MetricsBucketClear
        }
        // Any modifier will do, since terminals keep some for their
        // own: many take a Shift-click to select text, and macOS
        // Terminal makes a Ctrl-click a right one.
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Series(series))
            if mouse
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            SemanticCommandId::MetricsSeriesSolo { series }
        }
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Series(series)) => {
            SemanticCommandId::MetricsSeriesToggle { series }
        }
        (MouseEventKind::Up(MouseButton::Left), MetricsTarget::Session(session)) => {
            SemanticCommandId::MetricsSessionOpen { session }
        }
        // A drag scrubs: the selection follows the pointer along the
        // bars, staying at the end it passed beyond either.
        (
            MouseEventKind::Drag(MouseButton::Left),
            MetricsTarget::Bar(bucket)
            | MetricsTarget::Chart {
                toward: Some(bucket),
            },
        ) => SemanticCommandId::MetricsBucketSelect { bucket },
        // Down the wheel to the longer ranges, as a map zooms out.
        (MouseEventKind::ScrollDown, MetricsTarget::Bar(_) | MetricsTarget::Chart { .. }) => {
            SemanticCommandId::MetricsRangeNext
        }
        (MouseEventKind::ScrollUp, MetricsTarget::Bar(_) | MetricsTarget::Chart { .. }) => {
            SemanticCommandId::MetricsRangePrevious
        }
        _ => return None,
    })
}

/// Insights' own keys: Tab and Shift+Tab move focus among its panels, the
/// last turning round to the first, as a click on a panel focuses it; `f`
/// sorts the Tools panel by failures, or by uses again; Esc
/// clears what the focused panel has selected, then leaves the panel, and
/// with neither left closes the view; every other key is the focused
/// panel's, as [`command_for_insights_panel_key`] gives it.
fn command_for_insights_key(key: KeyEvent, keys: MetricsKeys) -> Option<SemanticCommandId> {
    Some(match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) if keys.panel_selected => {
            SemanticCommandId::MetricsPanelSelectionClear
        }
        (KeyCode::Esc, KeyModifiers::NONE) if keys.panel.is_some() => {
            SemanticCommandId::MetricsPanelFocusClear
        }
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::MetricsClose,
        (KeyCode::Tab, KeyModifiers::NONE) => SemanticCommandId::MetricsPanelFocusNext,
        // The one panel that sorts is the Tools panel, so `f` sorts it
        // whichever panel is focused, or none.
        (KeyCode::Char('f'), KeyModifiers::NONE) => SemanticCommandId::MetricsToolsSortToggle,
        // Terminals send Shift+Tab as BackTab, with Shift or without.
        (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
            SemanticCommandId::MetricsPanelFocusPrevious
        }
        _ => return command_for_insights_panel_key(keys.panel?, key),
    })
}

/// A key the focused Insights panel acts on: the keys that move along its
/// items the way they stand — on Turns, ←/→ along its sparkline's bins; on
/// When you work, ←/→ an hour along the week and ↑/↓ a day, the same hour;
/// on Tools, ↑/↓ a row, scrolling past either end of those in view.
fn command_for_insights_panel_key(
    panel: InsightsPanel,
    key: KeyEvent,
) -> Option<SemanticCommandId> {
    match (panel, key.code, key.modifiers) {
        (InsightsPanel::Turns | InsightsPanel::WhenYouWork, KeyCode::Left, KeyModifiers::NONE) => {
            Some(SemanticCommandId::MetricsPanelItemPrevious)
        }
        (InsightsPanel::Turns | InsightsPanel::WhenYouWork, KeyCode::Right, KeyModifiers::NONE) => {
            Some(SemanticCommandId::MetricsPanelItemNext)
        }
        (InsightsPanel::WhenYouWork | InsightsPanel::Tools, KeyCode::Up, KeyModifiers::NONE) => {
            Some(SemanticCommandId::MetricsPanelItemUp)
        }
        (InsightsPanel::WhenYouWork | InsightsPanel::Tools, KeyCode::Down, KeyModifiers::NONE) => {
            Some(SemanticCommandId::MetricsPanelItemDown)
        }
        _ => None,
    }
}

/// Insights' own clicks and wheel: a click on what a panel lets a reader
/// select selects it, focusing the panel, and one anywhere else on a panel
/// focuses it; the wheel is the panel's under the pointer, whether or not it
/// is focused, as [`command_for_insights_panel_wheel`] gives it.
fn command_for_insights_pointer(
    mouse: MouseEvent,
    target: InsightsTarget,
) -> Option<SemanticCommandId> {
    match (mouse.kind, target) {
        (MouseEventKind::Up(MouseButton::Left), InsightsTarget::Item { panel, item }) => {
            Some(SemanticCommandId::MetricsPanelItemSelect { panel, item })
        }
        (MouseEventKind::Up(MouseButton::Left), InsightsTarget::Panel(panel)) => {
            Some(SemanticCommandId::MetricsPanelFocus(panel))
        }
        (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown, target) => {
            command_for_insights_panel_wheel(target.panel(), mouse.kind)
        }
        _ => None,
    }
}

/// What the wheel does over an Insights panel, whether up or down by
/// `kind`: over the Tools panel, whose rows run past its border, it scrolls
/// them a row; over the others, whose figures all stand in view, nothing;
/// and never a step of the range, which the wheel over Overview's chart
/// takes.
fn command_for_insights_panel_wheel(
    panel: InsightsPanel,
    kind: MouseEventKind,
) -> Option<SemanticCommandId> {
    match (panel, kind) {
        (
            InsightsPanel::WhenYouWork
            | InsightsPanel::Turns
            | InsightsPanel::Compactions
            | InsightsPanel::Approvals,
            _,
        ) => None,
        (InsightsPanel::Tools, MouseEventKind::ScrollUp) => {
            Some(SemanticCommandId::MetricsPanelScrollUp { panel })
        }
        (InsightsPanel::Tools, MouseEventKind::ScrollDown) => {
            Some(SemanticCommandId::MetricsPanelScrollDown { panel })
        }
        (InsightsPanel::Tools, _) => None,
    }
}

/// The Serve overlay owns the keys while it is visible. Its picker of the
/// ways an Invite offers follows the other picker surfaces, with Space changing membership rather
/// than typing and Enter issuing the Invite for the complete chosen set.
pub(super) fn command_for_serve_overlay_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let command = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ServePrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::ServeNext
        }
        (KeyCode::Char(' '), KeyModifiers::NONE) => SemanticCommandId::ServeToggleAddress,
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::ServeConfirm,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => SemanticCommandId::ServeCopyInvite,
        (KeyCode::Char('x'), KeyModifiers::NONE) => SemanticCommandId::ServeRemovePeer,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::ServeClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// The Relay overlay owns the keys while it is visible: the list walks its
/// Relays and acts on the one the reader is on, the address entry takes what
/// is typed or pasted there, and the login display copies where to go and
/// the code to enter, each with a key of its own. Esc steps back to the list,
/// and from the list closes it.
pub(super) fn command_for_relay_overlay_event(
    event: InputEvent,
    mode: RelayInputMode,
) -> Option<CommandId> {
    let key = match event {
        InputEvent::Paste(text) if mode == RelayInputMode::Address => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::RelayAddressInsert,
                text,
            ));
        }
        InputEvent::Key(key) => key,
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let command = match (mode, key.code, key.modifiers) {
        (_, KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::RelayClose,
        (RelayInputMode::List, KeyCode::Up, KeyModifiers::NONE)
        | (RelayInputMode::List, KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::RelayPrevious
        }
        (RelayInputMode::List, KeyCode::Down, KeyModifiers::NONE)
        | (RelayInputMode::List, KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::RelayNext
        }
        (RelayInputMode::List, KeyCode::Char('a'), KeyModifiers::NONE)
        | (RelayInputMode::Address, KeyCode::Enter, KeyModifiers::NONE) => {
            SemanticCommandId::RelayAdd
        }
        (RelayInputMode::List, KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::RelayLogin,
        (RelayInputMode::List, KeyCode::Char('x'), KeyModifiers::NONE) => {
            SemanticCommandId::RelayRemove
        }
        (RelayInputMode::List, KeyCode::Char('s'), KeyModifiers::NONE) => {
            SemanticCommandId::RelayServeThroughToggle
        }
        (RelayInputMode::Address, KeyCode::Backspace, KeyModifiers::NONE) => {
            SemanticCommandId::RelayAddressDeleteBackward
        }
        (RelayInputMode::Address, KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::RelayAddressInsert,
                character.to_string(),
            ));
        }
        (RelayInputMode::Login, KeyCode::Char('a'), KeyModifiers::NONE) => {
            SemanticCommandId::RelayCopyAddress
        }
        (RelayInputMode::Login, KeyCode::Char('c'), KeyModifiers::NONE) => {
            SemanticCommandId::RelayCopyCode
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// The Connect overlay owns the keys while it is visible. The login a
/// redemption waits on copies where to go and the code to enter with the
/// Relay login's own keys, and Enter there tries a stopped redemption again.
pub(super) fn command_for_connect_overlay_event(
    event: InputEvent,
    mode: ConnectInputMode,
) -> Option<CommandId> {
    let key = match event {
        InputEvent::Paste(text)
            if matches!(mode, ConnectInputMode::Invite | ConnectInputMode::Name) =>
        {
            return Some(CommandId::InsertConnectText(text));
        }
        InputEvent::Key(key) => key,
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        return Some(CommandId::InvokeSemantic(SemanticCommandId::ConnectClose));
    }
    match (mode, key.code, key.modifiers) {
        // What overflows the step scrolls: the preview, Row by Row with the
        // arrows as nothing else there takes them, and why a redemption was
        // refused, page by page, beneath the name and ways the arrows walk.
        (ConnectInputMode::Confirm, KeyCode::Up, KeyModifiers::NONE)
        | (ConnectInputMode::Confirm, KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ConnectScrollUp),
        ),
        (ConnectInputMode::Confirm, KeyCode::Down, KeyModifiers::NONE)
        | (ConnectInputMode::Confirm, KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ConnectScrollDown),
        ),
        (
            ConnectInputMode::Confirm | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::PageUp,
            KeyModifiers::NONE,
        ) => Some(CommandId::InvokeSemantic(SemanticCommandId::ConnectPageUp)),
        (
            ConnectInputMode::Confirm | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::PageDown,
            KeyModifiers::NONE,
        ) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ConnectPageDown,
        )),
        (ConnectInputMode::Login, KeyCode::Char('a'), KeyModifiers::NONE) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::RelayCopyAddress),
        ),
        (ConnectInputMode::Login, KeyCode::Char('c'), KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::RelayCopyCode))
        }
        (ConnectInputMode::Picker, KeyCode::Char('a'), KeyModifiers::NONE) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ConnectPairAnother),
        ),
        (ConnectInputMode::Picker, KeyCode::Char('x'), KeyModifiers::NONE) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ConnectRemoveRemote),
        ),
        (ConnectInputMode::Picker, KeyCode::Enter, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::OutlookSelect))
        }
        // The name field takes the arrows too, because on the Configure Remote
        // screen it is where the keys start: a reader who reaches for Down to
        // get at the ways would otherwise press a dead key.
        (
            ConnectInputMode::Picker | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::Up,
            KeyModifiers::NONE,
        )
        | (
            ConnectInputMode::Picker | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ConnectPrevious,
        )),
        (
            ConnectInputMode::Picker | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::Down,
            KeyModifiers::NONE,
        )
        | (
            ConnectInputMode::Picker | ConnectInputMode::Name | ConnectInputMode::Ways,
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        ) => Some(CommandId::InvokeSemantic(SemanticCommandId::ConnectNext)),
        (ConnectInputMode::Name | ConnectInputMode::Ways, KeyCode::Tab, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::ConnectFocusNext,
            ))
        }
        // Reordering reaches the marked way from the name field too: the
        // reader who opened the screen to fix the priority order should not
        // have to arrive at the list before the keys that reorder it answer.
        (ConnectInputMode::Name | ConnectInputMode::Ways, KeyCode::Up, KeyModifiers::SHIFT) => {
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::ConnectMoveAddressUp,
            ))
        }
        (ConnectInputMode::Name | ConnectInputMode::Ways, KeyCode::Down, KeyModifiers::SHIFT) => {
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::ConnectMoveAddressDown,
            ))
        }
        (
            ConnectInputMode::Invite
            | ConnectInputMode::Confirm
            | ConnectInputMode::Name
            | ConnectInputMode::Ways
            | ConnectInputMode::Login,
            KeyCode::Enter,
            KeyModifiers::NONE,
        ) => Some(CommandId::InvokeSemantic(SemanticCommandId::ConnectConfirm)),
        (
            ConnectInputMode::Invite | ConnectInputMode::Name,
            KeyCode::Backspace,
            KeyModifiers::NONE,
        ) => Some(CommandId::DeleteConnectTextBackward),
        (
            ConnectInputMode::Invite | ConnectInputMode::Name,
            KeyCode::Char(character),
            KeyModifiers::NONE | KeyModifiers::SHIFT,
        ) => Some(CommandId::InsertConnectText(character.to_string())),
        _ => None,
    }
}

pub(super) fn command_for_completion_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectPreviousCompletion)
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectNextCompletion)
        }
        (KeyCode::Enter | KeyCode::Tab, KeyModifiers::NONE) => {
            Some(CommandId::ConfirmSelectedCompletion)
        }
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::DismissCompletion),
        _ => None,
    }
}

pub(super) fn command_for_session_picker_event(event: InputEvent) -> Option<CommandId> {
    if let InputEvent::Key(key) = &event
        && key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('d')
        && key.modifiers == KeyModifiers::CONTROL
    {
        return Some(CommandId::InvokeSemantic(SemanticCommandId::SessionDelete));
    }
    command_for_picker_event(event, &SESSION_PICKER_COMMANDS)
}

pub(super) fn command_for_model_picker_event(event: InputEvent) -> Option<CommandId> {
    command_for_picker_event(event, &MODEL_PICKER_COMMANDS)
}

pub(super) fn command_for_theme_picker_event(event: InputEvent) -> Option<CommandId> {
    command_for_picker_event(event, &THEME_PICKER_COMMANDS)
}

/// A mouse event answers through the terminal's own table rather than the
/// picker's keyboard bindings, which is what turns a right press into
/// [`CommandId::OpenContextMenuAt`] — nothing else here has ever needed the
/// pointer before, since the picker's rows are chosen by the keyboard alone.
///
/// Ctrl+E describes the Workspace the reader is on: a letter would only
/// narrow the list, so the one key that acts on a row is a chord, as Ctrl+D
/// is in the session picker. Ctrl+O opens the Directory Browser, as the
/// Browse row does, from anywhere in the picker — a query typed or not, since
/// a query takes that row away — and as a chord for the same reason; bare
/// Ctrl+N stays "down", as it is in every list.
pub(super) fn command_for_workspace_picker_event(event: InputEvent) -> Option<CommandId> {
    match event {
        event @ InputEvent::Mouse(_) => command_for_terminal_event(event),
        InputEvent::Key(key)
            if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::CONTROL =>
        {
            match key.code {
                KeyCode::Char('e') => Some(CommandId::InvokeSemantic(
                    SemanticCommandId::WorkspaceDescriptionEdit,
                )),
                KeyCode::Char('o') => Some(CommandId::InvokeSemantic(
                    SemanticCommandId::WorkspaceBrowse,
                )),
                _ => command_for_picker_event(InputEvent::Key(key), &WORKSPACE_PICKER_COMMANDS),
            }
        }
        event => command_for_picker_event(event, &WORKSPACE_PICKER_COMMANDS),
    }
}

/// The Directory Browser walks its tree with the keys every picker walks its
/// rows with — the arrows, and Ctrl+P and Ctrl+N — while Space opens or
/// closes the focused row, Right steps into it, Left steps up to its parent
/// or walks the root up to its own, Enter chooses it, Alt+H shows or hides
/// hidden directories, and Esc closes the browser; a terminal sending Alt as
/// an Escape before the letter is read as Alt all the same. Everything else the
/// reader types or pastes goes to the end of the path field, which has no
/// cursor for Left and Right to move: Backspace takes a character back, and
/// Tab completes the field to the focused row. The pointer answers as it does
/// everywhere else, resolved against where the frame drew the tree: a press
/// on a row is a Space on it and a second one there an Enter, the wheel over
/// the box moves the tree, and a press outside the box is already an Escape
/// by the time it gets here. The browser offers no menu, so a right press
/// reaches nothing, beneath it or otherwise.
pub(super) fn command_for_directory_browser_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::DirectoryBrowserPathInsert,
                text,
            ));
        }
        InputEvent::Mouse(mouse) if mouse.kind != MouseEventKind::Down(MouseButton::Right) => {
            return command_for_pointer(mouse);
        }
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::DirectoryBrowserPrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::DirectoryBrowserNext
        }
        (KeyCode::Char(' '), KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserRowToggle,
        (KeyCode::Right, KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserRowEnter,
        (KeyCode::Left, KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserRowParent,
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserChoose,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserClose,
        (KeyCode::Char('h'), KeyModifiers::ALT) => SemanticCommandId::DirectoryBrowserHiddenToggle,
        (KeyCode::Tab, KeyModifiers::NONE) => SemanticCommandId::DirectoryBrowserPathComplete,
        (KeyCode::Backspace, KeyModifiers::NONE) => {
            SemanticCommandId::DirectoryBrowserPathDeleteBackward
        }
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::DirectoryBrowserPathInsert,
                character.to_string(),
            ));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

/// The rename dialog has the keys while it stands: what the reader types or
/// pastes is the Title, Backspace takes a character back, Ctrl+U empties the
/// field, Ctrl+G asks for a derived proposal, Enter saves, and Esc stops a
/// proposal on its way or closes the dialog. A press on its derive button
/// asks for a proposal; one outside its box is already an Escape by the time
/// it gets here.
pub(super) fn command_for_rename_dialog_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::RenameInsert,
                text,
            ));
        }
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::RenameSave,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::RenameCancel,
        (KeyCode::Backspace, KeyModifiers::NONE) => SemanticCommandId::RenameDeleteBackward,
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => SemanticCommandId::RenameClear,
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => SemanticCommandId::RenameDerive,
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::RenameInsert,
                character.to_string(),
            ));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

/// The Description editor has the keys while it stands over the Workspace
/// Picker: what the reader types or pastes is the Description, Backspace
/// takes a character back, Ctrl+U empties it, Enter saves it, and Esc closes
/// it without saving. A press outside its box is already an Escape by the
/// time it gets here, and one inside it moves nothing.
pub(super) fn command_for_workspace_description_editor_event(
    event: InputEvent,
) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::WorkspaceDescriptionInsert,
                text,
            ));
        }
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::WorkspaceDescriptionSave,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::WorkspaceDescriptionCancel,
        (KeyCode::Backspace, KeyModifiers::NONE) => {
            SemanticCommandId::WorkspaceDescriptionDeleteBackward
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => SemanticCommandId::WorkspaceDescriptionClear,
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::WorkspaceDescriptionInsert,
                character.to_string(),
            ));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

/// The Workspace Picker row menu is the newest thing on screen while it
/// stands open, on the same terms the Sidebar's own row menu is: the arrows
/// walk its items, Enter acts on the one the reader is on, Esc puts it away
/// leaving the row alone, and the pointer
/// answers as it does everywhere else — a press inside the menu's own box
/// reaches the click handler as an ordinary click, and one outside it is
/// already an Escape by the time it gets here (see
/// `SemanticCommandId::PointerClick`'s own outside-overlay handling).
pub(super) fn command_for_workspace_picker_menu_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) => SemanticCommandId::WorkspacePickerMenuPrevious,
        (KeyCode::Down, KeyModifiers::NONE) => SemanticCommandId::WorkspacePickerMenuNext,
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::WorkspacePickerMenuSelect,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::WorkspacePickerMenuClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

/// The Sidebar has the keyboard and not the mouse. Up and Down move through
/// the rows, Enter opens the Session the reader is on, Esc backs out of the
/// Sidebar a step at a time, and the toggle closes it from inside as readily
/// as from outside. Everything else they type goes to the search box, as it
/// does in the pickers, so a reader looking for a Session by name simply types
/// its name — and none of it reaches the composer, which does not have the
/// keys.
///
/// A paste goes to the search box whole, as it does in the pickers: a Title
/// carried in from somewhere else is as good a way to find a Session as one
/// the reader types out.
///
/// The mouse is the exception, and answers as it does from the composer: the
/// Sidebar stands beside the main view rather than over it, so the pointer is
/// resolved against the whole frame's geometry rather than against this
/// surface alone. A press lands on whatever is drawn under it, and the wheel
/// moves the list it is over — this one over the Sidebar, and the Transcript
/// over the main view, which is still the reader's to read while the keys are
/// here.
pub(super) fn command_for_sidebar_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => return Some(CommandId::InsertSidebarText(text)),
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::SidebarPrevious),
        ),
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarNext))
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarAttach))
        }
        (KeyCode::Esc, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarLeave))
        }
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarToggle))
        }
        // The Leader stays reachable, so Ctrl+X A can hand the keys to the
        // Aside from here.
        (KeyCode::Char('x'), KeyModifiers::CONTROL) => Some(CommandId::BeginLeader),
        _ => command_for_search_key(
            key,
            &CommandId::DeleteSidebarTextBackward,
            CommandId::InsertSidebarText,
        ),
    }
}

/// The Aside's keys while it holds them: Up/Down and Ctrl+P/Ctrl+N walk its
/// row focus, Enter opens the focused entry, and Esc hands the keys back. The
/// Leader stays reachable so the same chord that brought the reader in takes
/// them out, and Ctrl+B passes the keys to the Sidebar. The pointer answers
/// as it does everywhere else.
pub(super) fn command_for_aside_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::AsidePrevious))
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::AsideNext))
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::AsideOpen))
        }
        (KeyCode::Esc, KeyModifiers::NONE) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::AsideLeave))
        }
        (KeyCode::Char('x'), KeyModifiers::CONTROL) => Some(CommandId::BeginLeader),
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
            Some(CommandId::InvokeSemantic(SemanticCommandId::SidebarToggle))
        }
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(CommandId::ClearOrExit),
        _ => None,
    }
}

/// The keys that make a search box: the character the reader typed, and the
/// Backspace taking one back. Every surface that narrows a list by what a
/// reader types reads them here, so a query behaves the same wherever they
/// type it — and so the surface's own keys, read first, keep whatever letters
/// they have claimed.
fn command_for_search_key(
    key: KeyEvent,
    delete_backward: &CommandId,
    insert: fn(String) -> CommandId,
) -> Option<CommandId> {
    match (key.code, key.modifiers) {
        (KeyCode::Backspace, KeyModifiers::NONE) => Some(delete_backward.clone()),
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            Some(insert(character.to_string()))
        }
        _ => None,
    }
}

/// A Sidebar row's context menu is the newest thing on screen while it is up,
/// so it has the keys whether or not the Sidebar itself does: the arrows walk
/// its items, Enter acts on the one the reader is on, and Esc puts the menu
/// away leaving the row alone. Nothing else reaches the rows behind it — a
/// letter typed at a menu is not a query — and the mouse answers as it does
/// everywhere else, because a press outside a menu is how a reader dismisses
/// one. The wheel is the one gesture the rows do not answer while it stands,
/// since they would move out from under the menu opened on them; over the
/// main view it goes on moving the Transcript.
pub(super) fn command_for_sidebar_menu_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        event @ InputEvent::Mouse(_) => return command_for_terminal_event(event),
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SidebarMenuPrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SidebarMenuNext
        }
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::SidebarMenuSelect,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::SidebarMenuClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

pub(super) fn command_for_model_options_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ModelOptionsPrevious),
        ),
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ModelOptionsNext),
        ),
        (KeyCode::Enter, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsSelect,
        )),
        (KeyCode::Enter, KeyModifiers::CONTROL) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsApply,
        )),
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsCancel,
        )),
        _ => None,
    }
}

/// The settings panel edits one Setting at a time and has no search line, so
/// its keys are Up and Down between the rows of a tab, Left and Right between
/// the tabs themselves, the Space that cycles the focused value, the Enter that
/// opens the focused row onto whatever it stands for — a Provider's own
/// Settings, or the surface a value too rich to cycle is chosen at — and the
/// reset that takes a pin out. Enter opens and never edits: what it opens onto
/// is where a value the reader chooses is settled. Nothing else reaches the
/// composer while the panel is open.
///
/// Stepped in onto a Provider's own Settings, Up and Down walk those, Space,
/// Enter and the reset act on the one focused among them, and every key that
/// would take the reader away — Esc, Left, Tab — steps back out to the
/// Providers instead, so Esc there never closes the panel. Right has nowhere
/// further in to go and does nothing.
///
/// The reader may also point at the panel, which the frame's own geometry
/// resolves. A click is the whole of it: the pointer asks for a tab or a
/// row, and no click carries a key's meaning.
pub(super) fn command_for_settings_panel_event(
    event: InputEvent,
    keys: PanelKeys,
) -> Option<CommandId> {
    if let InputEvent::Mouse(mouse) = &event {
        return matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)).then_some(
            CommandId::FocusSettingsPanelAt {
                column: mouse.column,
                screen_row: mouse.row,
            },
        );
    }
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let within = keys == PanelKeys::ProviderSettings;
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsPrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsNext
        }
        (KeyCode::Esc | KeyCode::Left | KeyCode::Tab, KeyModifiers::NONE) if within => {
            SemanticCommandId::SettingsProviderLeave
        }
        (KeyCode::Right, KeyModifiers::NONE) if within => return None,
        (KeyCode::Left, KeyModifiers::NONE) => SemanticCommandId::SettingsTabPrevious,
        (KeyCode::Right, KeyModifiers::NONE) => SemanticCommandId::SettingsTabNext,
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::SettingsRowOpen,
        (KeyCode::Char(' '), KeyModifiers::NONE) => SemanticCommandId::SettingsValueCycle,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => SemanticCommandId::SettingsReset,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::SettingsClose,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

/// Numeric editor input is semantic at the first seam: the terminal only
/// translates keys, while the same commands remain available to future
/// pointers and plugins without reproducing editor behavior.
pub(super) fn command_for_numeric_editor_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let command = match (key.code, key.modifiers) {
        (KeyCode::Char(character), KeyModifiers::NONE) => {
            SemanticCommandId::SettingsNumericInsert(NumericDigit::from_char(character)?)
        }
        (KeyCode::Backspace, KeyModifiers::NONE) => {
            SemanticCommandId::SettingsNumericDeleteBackward
        }
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::SettingsNumericApply,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::SettingsNumericCancel,
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(command))
}

/// The list editor has the keys while it stands over the settings panel: the
/// arrows walk its rows, what the reader types or pastes lands on the focused
/// row, Backspace takes a character back, Ctrl+D removes the focused entry,
/// Enter saves the whole list, and Esc closes it without saving.
pub(super) fn command_for_list_editor_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::SettingsListInsert,
                text,
            ));
        }
        _ => return None,
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsListPrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsListNext
        }
        (KeyCode::Backspace, KeyModifiers::NONE) => SemanticCommandId::SettingsListDeleteBackward,
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => SemanticCommandId::SettingsListRemove,
        (KeyCode::Tab, KeyModifiers::NONE) => SemanticCommandId::SettingsListBrowse,
        (KeyCode::Enter, KeyModifiers::NONE) => SemanticCommandId::SettingsListSave,
        (KeyCode::Esc, KeyModifiers::NONE) => SemanticCommandId::SettingsListCancel,
        (KeyCode::Char(character), modifiers)
            if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            return Some(CommandId::InvokeSemanticText(
                SemanticCommandId::SettingsListInsert,
                character.to_string(),
            ));
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

struct PickerCommandBindings {
    previous: CommandId,
    next: CommandId,
    page_previous: CommandId,
    page_next: CommandId,
    select: CommandId,
    close: CommandId,
    /// The keys that narrow the list by what the reader types, where the
    /// picker narrows that way at all. A picker that offers a fixed list binds
    /// none, and a letter typed at it reaches nothing.
    search: Option<PickerSearchBindings>,
    toggle_scope: Option<CommandId>,
}

struct PickerSearchBindings {
    delete_backward: CommandId,
    insert: fn(String) -> CommandId,
}

const SESSION_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousSession,
    next: CommandId::SelectNextSession,
    page_previous: CommandId::PagePreviousSessions,
    page_next: CommandId::PageNextSessions,
    select: CommandId::SelectSession,
    close: CommandId::CloseSessionPicker,
    search: Some(PickerSearchBindings {
        delete_backward: CommandId::DeleteSessionSearchBackward,
        insert: CommandId::InsertSessionSearch,
    }),
    toggle_scope: Some(CommandId::ToggleSessionScope),
};

const MODEL_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousModel,
    next: CommandId::SelectNextModel,
    page_previous: CommandId::PagePreviousModels,
    page_next: CommandId::PageNextModels,
    select: CommandId::SelectModel,
    close: CommandId::CloseModelPicker,
    search: Some(PickerSearchBindings {
        delete_backward: CommandId::DeleteModelSearchBackward,
        insert: CommandId::InsertModelSearch,
    }),
    toggle_scope: None,
};

const THEME_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousTheme,
    next: CommandId::SelectNextTheme,
    page_previous: CommandId::PagePreviousThemes,
    page_next: CommandId::PageNextThemes,
    select: CommandId::SelectTheme,
    close: CommandId::CloseThemePicker,
    search: Some(PickerSearchBindings {
        delete_backward: CommandId::DeleteThemeSearchBackward,
        insert: CommandId::InsertThemeSearch,
    }),
    toggle_scope: None,
};

/// The Workspace Picker answers the keys every picker answers — the arrows and
/// paging keys walk its rows, what the reader types narrows them, Enter
/// switches to the row the reader is on where there is one, and Esc closes —
/// and the pointer as the session picker's rows answer it. It has no second
/// scope to offer: it always asks across every Workspace.
const WORKSPACE_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousWorkspace,
    next: CommandId::SelectNextWorkspace,
    page_previous: CommandId::PagePreviousWorkspaces,
    page_next: CommandId::PageNextWorkspaces,
    select: CommandId::SelectWorkspace,
    close: CommandId::CloseWorkspacePicker,
    search: Some(PickerSearchBindings {
        delete_backward: CommandId::DeleteWorkspaceSearchBackward,
        insert: CommandId::InsertWorkspaceSearch,
    }),
    toggle_scope: None,
};

fn command_for_picker_event(
    event: InputEvent,
    bindings: &PickerCommandBindings,
) -> Option<CommandId> {
    match event {
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) => match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                Some(bindings.previous.clone())
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                Some(bindings.next.clone())
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => Some(bindings.page_previous.clone()),
            (KeyCode::PageDown, KeyModifiers::NONE) => Some(bindings.page_next.clone()),
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => bindings.toggle_scope.clone(),
            (KeyCode::Enter, KeyModifiers::NONE) => Some(bindings.select.clone()),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(bindings.close.clone()),
            _ => {
                let search = bindings.search.as_ref()?;
                command_for_search_key(key, &search.delete_backward, search.insert)
            }
        },
        InputEvent::Paste(text) => bindings.search.as_ref().map(|search| (search.insert)(text)),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct CommandBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
    command: CommandId,
    label: &'static str,
}

const COMMAND_BINDINGS: &[CommandBinding] = &[
    CommandBinding {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::SelectAll,
        label: "Ctrl+A",
    },
    CommandBinding {
        code: KeyCode::Left,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionLeft,
        label: "Shift+Left",
    },
    CommandBinding {
        code: KeyCode::Right,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionRight,
        label: "Shift+Right",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionUp,
        label: "Shift+Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionDown,
        label: "Shift+Down",
    },
    CommandBinding {
        code: KeyCode::Home,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionLineStart,
        label: "Shift+Home",
    },
    CommandBinding {
        code: KeyCode::End,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::ExtendSelectionLineEnd,
        label: "Shift+End",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SubmitSteer,
        label: "Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::ALT,
        command: CommandId::SubmitQueue,
        label: "Alt+Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::InsertNewline,
        label: "Shift+Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::InsertNewline,
        label: "Ctrl+Enter",
    },
    CommandBinding {
        code: KeyCode::Char('j'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::InsertNewline,
        label: "Ctrl+J",
    },
    CommandBinding {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::ClearOrExit,
        label: "Ctrl+C",
    },
    CommandBinding {
        code: KeyCode::Backspace,
        modifiers: KeyModifiers::NONE,
        command: CommandId::DeleteBackward,
        label: "Backspace",
    },
    CommandBinding {
        code: KeyCode::Delete,
        modifiers: KeyModifiers::NONE,
        command: CommandId::DeleteForward,
        label: "Delete",
    },
    CommandBinding {
        code: KeyCode::Left,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorLeft,
        label: "Left",
    },
    CommandBinding {
        code: KeyCode::Right,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorRight,
        label: "Right",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        command: CommandId::HistoryPrevious,
        label: "Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        command: CommandId::HistoryNext,
        label: "Down",
    },
    CommandBinding {
        code: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
        command: CommandId::ScrollTranscriptPageUp,
        label: "PageUp",
    },
    CommandBinding {
        code: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
        command: CommandId::ScrollTranscriptPageDown,
        label: "PageDown",
    },
    CommandBinding {
        code: KeyCode::End,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorLineEnd,
        label: "End",
    },
    CommandBinding {
        code: KeyCode::Home,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorLineStart,
        label: "Home",
    },
    CommandBinding {
        code: KeyCode::End,
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::FollowLatest,
        label: "Ctrl+End",
    },
    CommandBinding {
        code: KeyCode::Char('x'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::BeginLeader,
        label: "Ctrl+X",
    },
    CommandBinding {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        command: CommandId::RequestInterrupt,
        label: "Esc",
    },
];

const LEADER_BINDINGS: &[CommandBinding] = &[CommandBinding {
    code: KeyCode::Char('q'),
    modifiers: KeyModifiers::NONE,
    command: CommandId::OpenQueuedPrompts,
    label: "q",
}];

const QUEUED_PROMPT_BINDINGS: &[CommandBinding] = &[
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        command: CommandId::PromoteSelectedPrompt,
        label: "Enter",
    },
    CommandBinding {
        code: KeyCode::Char('d'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::CancelSelectedPrompt,
        label: "Ctrl+D",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SelectPreviousQueuedPrompt,
        label: "Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SelectNextQueuedPrompt,
        label: "Down",
    },
    CommandBinding {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        command: CommandId::CloseCommandMode,
        label: "Esc",
    },
];

const INTERRUPT_CONFIRMATION_BINDINGS: &[CommandBinding] = &[CommandBinding {
    code: KeyCode::Esc,
    modifiers: KeyModifiers::NONE,
    command: CommandId::ConfirmInterrupt,
    label: "Esc",
}];

fn binding_for(key: KeyEvent) -> Option<&'static CommandBinding> {
    COMMAND_BINDINGS
        .iter()
        .find(|binding| binding.code == key.code && binding.modifiers == key.modifiers)
}

pub(super) fn binding_label(command: &CommandId) -> &'static str {
    if let CommandId::InvokeSemantic(command) = command {
        return descriptor(*command)
            .keybinding
            .map_or("", |binding| binding.label);
    }
    COMMAND_BINDINGS
        .iter()
        .chain(LEADER_BINDINGS)
        .chain(QUEUED_PROMPT_BINDINGS)
        .chain(INTERRUPT_CONFIRMATION_BINDINGS)
        .find(|binding| &binding.command == command)
        .map_or("", |binding| binding.label)
}

pub(super) fn command_for_leader_event(event: InputEvent) -> Option<CommandId> {
    let semantic = match &event {
        InputEvent::Key(key) if key.kind == KeyEventKind::Press => {
            command_for_leader_key(*key).map(CommandId::InvokeSemantic)
        }
        _ => None,
    };
    semantic
        .or_else(|| command_from_scoped_bindings(event, LEADER_BINDINGS))
        .or(Some(CommandId::CloseCommandMode))
}

pub(super) fn command_for_queued_prompt_event(event: InputEvent) -> Option<CommandId> {
    command_from_scoped_bindings(event, QUEUED_PROMPT_BINDINGS)
}

pub(super) fn command_for_interrupt_confirmation_event(event: InputEvent) -> Option<CommandId> {
    command_from_scoped_bindings(event, INTERRUPT_CONFIRMATION_BINDINGS)
        .or(Some(CommandId::CloseCommandMode))
}

fn command_from_scoped_bindings(
    event: InputEvent,
    bindings: &'static [CommandBinding],
) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    bindings
        .iter()
        .find(|binding| binding.code == key.code && binding.modifiers == key.modifiers)
        .map(|binding| binding.command.clone())
}

/// Worktree navigation uses semantic actions, and offers Worktrees alone: a
/// directory below a Worktree is not chosen from here.
pub(super) fn command_for_worktree_picker_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let semantic = match key.code {
        KeyCode::Char('d') => SemanticCommandId::WorktreeRemove,
        KeyCode::Char('F') => SemanticCommandId::WorktreeForceRemove,
        KeyCode::Esc => SemanticCommandId::WorktreeClose,
        KeyCode::Enter => SemanticCommandId::WorktreeSelect,
        KeyCode::Up => SemanticCommandId::WorktreePrevious,
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            SemanticCommandId::WorktreePrevious
        }
        KeyCode::Down => SemanticCommandId::WorktreeNext,
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            SemanticCommandId::WorktreeNext
        }
        _ => return None,
    };
    Some(CommandId::InvokeSemantic(semantic))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_command(code: KeyCode, keys: PanelKeys) -> Option<CommandId> {
        command_for_settings_panel_event(
            InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            keys,
        )
    }

    /// On a Provider's own Settings, every key that would take the reader away
    /// steps back out to the Providers instead, so Esc there never closes the
    /// panel; the keys that walk and edit mean what they mean on any row.
    #[test]
    fn the_keys_that_leave_a_providers_settings_step_back_out_rather_than_away() {
        for code in [KeyCode::Esc, KeyCode::Left, KeyCode::Tab] {
            assert_eq!(
                settings_command(code, PanelKeys::ProviderSettings),
                Some(CommandId::InvokeSemantic(
                    SemanticCommandId::SettingsProviderLeave
                )),
                "{code:?} on a Provider's own Settings"
            );
        }
        assert_eq!(
            settings_command(KeyCode::Right, PanelKeys::ProviderSettings),
            None
        );
        for (code, command) in [
            (KeyCode::Up, SemanticCommandId::SettingsPrevious),
            (KeyCode::Down, SemanticCommandId::SettingsNext),
            (KeyCode::Enter, SemanticCommandId::SettingsRowOpen),
            (KeyCode::Char(' '), SemanticCommandId::SettingsValueCycle),
        ] {
            assert_eq!(
                settings_command(code, PanelKeys::ProviderSettings),
                Some(CommandId::InvokeSemantic(command)),
                "{code:?} on a Provider's own Settings"
            );
        }

        for (code, command) in [
            (KeyCode::Esc, Some(SemanticCommandId::SettingsClose)),
            (KeyCode::Left, Some(SemanticCommandId::SettingsTabPrevious)),
            (KeyCode::Right, Some(SemanticCommandId::SettingsTabNext)),
            (KeyCode::Tab, None),
        ] {
            assert_eq!(
                settings_command(code, PanelKeys::Rows),
                command.map(CommandId::InvokeSemantic),
                "{code:?} on the rows"
            );
        }
    }
}
