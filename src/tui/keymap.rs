//! Terminal input mapping: the keybinding tables and the per-mode functions
//! that translate a terminal event into a [`CommandId`].

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Position;

use super::{
    commands::{
        NumericDigit, SemanticCommandId, command_for_direct_semantic_key, command_for_leader_key,
        descriptor,
    },
    state::CommandId,
};

pub fn command_for_terminal_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(CommandId::ScrollTranscriptLinesUp),
            MouseEventKind::ScrollDown => Some(CommandId::ScrollTranscriptLinesDown),
            // A press, not a release, so a Fold answers the click the reader
            // just made rather than trailing a drag that ends elsewhere.
            MouseEventKind::Down(MouseButton::Left) => {
                Some(CommandId::ToggleTranscriptDisclosureAt {
                    position: Position::new(mouse.column, mouse.row),
                })
            }
            _ => None,
        },
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
        InputEvent::Paste(text) => Some(CommandId::PasteText(text)),
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
/// Sidebar stands beside the main view rather than over it, so the wheel is
/// still the reader's way through a Transcript. The Sidebar's own rows take
/// the mouse in <https://github.com/jake-tucker/suru/issues/181>.
pub(super) fn command_for_sidebar_event(event: InputEvent) -> Option<CommandId> {
    let key = match event {
        InputEvent::Key(key) => key,
        InputEvent::Paste(text) => return Some(CommandId::InsertSidebarSearch(text)),
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
        _ => command_for_search_key(
            key,
            &CommandId::DeleteSidebarSearchBackward,
            CommandId::InsertSidebarSearch,
        ),
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
/// resolves. A left press is the whole of it: the pointer asks for a tab or a
/// row, and no click carries a key's meaning.
pub(super) fn command_for_settings_panel_event(event: InputEvent) -> Option<CommandId> {
    if let InputEvent::Mouse(mouse) = &event {
        // A press, not a release, so the panel answers the click the reader
        // just made rather than trailing a drag that ends elsewhere.
        return matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)).then_some(
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
    delete_backward: CommandId,
    insert: fn(String) -> CommandId,
    toggle_scope: Option<CommandId>,
}

const SESSION_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousSession,
    next: CommandId::SelectNextSession,
    page_previous: CommandId::PagePreviousSessions,
    page_next: CommandId::PageNextSessions,
    select: CommandId::SelectSession,
    close: CommandId::CloseSessionPicker,
    delete_backward: CommandId::DeleteSessionSearchBackward,
    insert: CommandId::InsertSessionSearch,
    toggle_scope: Some(CommandId::ToggleSessionScope),
};

const MODEL_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousModel,
    next: CommandId::SelectNextModel,
    page_previous: CommandId::PagePreviousModels,
    page_next: CommandId::PageNextModels,
    select: CommandId::SelectModel,
    close: CommandId::CloseModelPicker,
    delete_backward: CommandId::DeleteModelSearchBackward,
    insert: CommandId::InsertModelSearch,
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
            _ => command_for_search_key(key, &bindings.delete_backward, bindings.insert),
        },
        InputEvent::Paste(text) => Some((bindings.insert)(text)),
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
        command: CommandId::FollowLatest,
        label: "End",
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
