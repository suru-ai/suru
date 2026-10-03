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
    relay_overlay::RelayInputMode,
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
/// whichever list it is over — the Sidebar's over the Sidebar, the Transcript
/// anywhere else.
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

/// The Connect overlay owns the keys while it is visible.
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
            | ConnectInputMode::Ways,
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
/// is in the session picker.
pub(super) fn command_for_workspace_picker_event(event: InputEvent) -> Option<CommandId> {
    match event {
        event @ InputEvent::Mouse(_) => command_for_terminal_event(event),
        InputEvent::Key(key)
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('e')
                && key.modifiers == KeyModifiers::CONTROL =>
        {
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceDescriptionEdit,
            ))
        }
        event => command_for_picker_event(event, &WORKSPACE_PICKER_COMMANDS),
    }
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
/// opens the focused row onto whatever it stands for — a Provider's further
/// Settings, or the surface a value too rich to cycle is chosen at — and the
/// reset that takes a pin out. Enter opens and never edits: what it opens onto
/// is where a value the reader chooses is settled. Nothing else reaches the
/// composer while the panel is open.
///
/// The reader may also point at the panel, which the frame's own geometry
/// resolves. A click is the whole of it: the pointer asks for a tab or a
/// row, and no click carries a key's meaning.
pub(super) fn command_for_settings_panel_event(event: InputEvent) -> Option<CommandId> {
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
    let semantic = match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsPrevious
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            SemanticCommandId::SettingsNext
        }
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

/// Worktree navigation uses semantic actions. A directory below a Worktree is
/// reached through the Sidebar's path entry rather than from here.
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
