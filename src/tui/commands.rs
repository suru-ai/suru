//! Typed semantic commands and the Command completion mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::protocol::{SessionId, TurnId};

pub(super) const AUTOCOMPLETE_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticCommandId {
    ApplicationExit,
    ModelList,
    ModelOptions,
    ModelOptionsPrevious,
    ModelOptionsNext,
    ModelOptionsSelect,
    ModelOptionsApply,
    ModelOptionsCancel,
    ModelOptionReasoningCycle,
    SessionList,
    SessionDelete,
    SessionNew,
    SessionSettle,
    SessionUnsettle,
    SidebarToggle,
    SidebarPrevious,
    SidebarNext,
    SidebarAttach,
    SidebarLeave,
    SidebarMenuPrevious,
    SidebarMenuNext,
    SidebarMenuSelect,
    SidebarMenuClose,
    SettingsOpen,
    SettingsPrevious,
    SettingsNext,
    SettingsTabPrevious,
    SettingsTabNext,
    SettingsRowOpen,
    SettingsValueCycle,
    SettingsReset,
    SettingsClose,
    SettingsNumericInsert(NumericDigit),
    SettingsNumericDeleteBackward,
    SettingsNumericApply,
    SettingsNumericCancel,
    TranscriptFoldsToggle,
    TranscriptGroupsToggle,
    TranscriptTurnToggle,
    TranscriptTurnsToggle,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NumericDigit {
    Zero,
    One,
    Two,
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
}

impl NumericDigit {
    const ALL: [Self; 10] = [
        Self::Zero,
        Self::One,
        Self::Two,
        Self::Three,
        Self::Four,
        Self::Five,
        Self::Six,
        Self::Seven,
        Self::Eight,
        Self::Nine,
    ];
    const CHARACTERS: [char; 10] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];
    const COMMAND_IDS: [&'static str; 10] = [
        "settings.numeric.insert.0",
        "settings.numeric.insert.1",
        "settings.numeric.insert.2",
        "settings.numeric.insert.3",
        "settings.numeric.insert.4",
        "settings.numeric.insert.5",
        "settings.numeric.insert.6",
        "settings.numeric.insert.7",
        "settings.numeric.insert.8",
        "settings.numeric.insert.9",
    ];
    const TITLES: [&'static str; 10] = [
        "Insert 0", "Insert 1", "Insert 2", "Insert 3", "Insert 4", "Insert 5", "Insert 6",
        "Insert 7", "Insert 8", "Insert 9",
    ];

    pub(super) fn from_char(character: char) -> Option<Self> {
        Self::CHARACTERS
            .iter()
            .position(|candidate| *candidate == character)
            .map(|index| Self::ALL[index])
    }

    pub const fn as_char(self) -> char {
        Self::CHARACTERS[self as usize]
    }

    const fn command_id(self) -> &'static str {
        Self::COMMAND_IDS[self as usize]
    }

    const fn title(self) -> &'static str {
        Self::TITLES[self as usize]
    }
}

/// What a semantic command acts on. Most act on the view as a whole; one that
/// names a subject carries it here rather than letting the surface that
/// invoked it reach into view state itself, so a click, a keybinding, and a
/// future plugin all drive the very same command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SemanticSubject {
    View,
    Turn(TurnId),
    /// One Session, which is what a reader names by acting on its row rather
    /// than on the Session they have open.
    Session(SessionId),
}

/// One invocation of a semantic command: which command, and what it acts on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SemanticInvocation {
    pub(super) id: SemanticCommandId,
    pub(super) subject: SemanticSubject,
}

/// A bare command ID is an invocation against the view as a whole, which is
/// what every command that names no subject means.
impl From<SemanticCommandId> for SemanticInvocation {
    fn from(id: SemanticCommandId) -> Self {
        Self {
            id,
            subject: SemanticSubject::View,
        }
    }
}

impl SemanticCommandId {
    /// This command invoked against one Turn, which is what a reader asks for
    /// by clicking that Turn's Fold marker.
    pub(super) const fn on_turn(self, turn_id: TurnId) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Turn(turn_id),
        }
    }

    /// This command invoked against one Session, which is what a reader asks
    /// for from that Session's own row in the Sidebar.
    pub(super) const fn on_session(self, session_id: SessionId) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Session(session_id),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApplicationExit => "application.exit",
            Self::ModelList => "model.list",
            Self::ModelOptions => "model.options",
            Self::ModelOptionsPrevious => "model.options.previous",
            Self::ModelOptionsNext => "model.options.next",
            Self::ModelOptionsSelect => "model.options.select",
            Self::ModelOptionsApply => "model.options.apply",
            Self::ModelOptionsCancel => "model.options.cancel",
            Self::ModelOptionReasoningCycle => "model.option.reasoning.cycle",
            Self::SessionList => "session.list",
            Self::SessionDelete => "session.delete",
            Self::SessionNew => "session.new",
            Self::SessionSettle => "session.settle",
            Self::SessionUnsettle => "session.unsettle",
            Self::SidebarToggle => "sidebar.toggle",
            Self::SidebarPrevious => "sidebar.previous",
            Self::SidebarNext => "sidebar.next",
            Self::SidebarAttach => "sidebar.attach",
            Self::SidebarLeave => "sidebar.leave",
            Self::SidebarMenuPrevious => "sidebar.menu.previous",
            Self::SidebarMenuNext => "sidebar.menu.next",
            Self::SidebarMenuSelect => "sidebar.menu.select",
            Self::SidebarMenuClose => "sidebar.menu.close",
            Self::SettingsOpen => "settings.open",
            Self::SettingsPrevious => "settings.previous",
            Self::SettingsNext => "settings.next",
            Self::SettingsTabPrevious => "settings.tab.previous",
            Self::SettingsTabNext => "settings.tab.next",
            Self::SettingsRowOpen => "settings.row.open",
            Self::SettingsValueCycle => "settings.value.cycle",
            Self::SettingsReset => "settings.reset",
            Self::SettingsClose => "settings.close",
            Self::SettingsNumericInsert(digit) => digit.command_id(),
            Self::SettingsNumericDeleteBackward => "settings.numeric.delete-backward",
            Self::SettingsNumericApply => "settings.numeric.apply",
            Self::SettingsNumericCancel => "settings.numeric.cancel",
            Self::TranscriptFoldsToggle => "transcript.folds.toggle",
            Self::TranscriptGroupsToggle => "transcript.groups.toggle",
            Self::TranscriptTurnToggle => "transcript.turn.fold.toggle",
            Self::TranscriptTurnsToggle => "transcript.turns.toggle",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SemanticCommandDescriptor {
    pub(super) id: SemanticCommandId,
    pub(super) title: &'static str,
    pub(super) description: &'static str,
    pub(super) slash: Option<SlashCommand>,
    pub(super) keybinding: Option<SemanticKeybinding>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SlashCommand {
    pub(super) name: &'static str,
    aliases: &'static [&'static str],
}

/// A semantic keybinding either follows the `Ctrl+X` leader prefix or fires
/// directly from the composer when `prefix` is `None`.
#[derive(Clone, Copy, Debug)]
pub(super) struct SemanticKeybinding {
    prefix: Option<(KeyCode, KeyModifiers)>,
    code: KeyCode,
    modifiers: KeyModifiers,
    pub(super) label: &'static str,
}

const LEADER_PREFIX: (KeyCode, KeyModifiers) = (KeyCode::Char('x'), KeyModifiers::CONTROL);

const fn numeric_insert_descriptor(
    id: SemanticCommandId,
    title: &'static str,
) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id,
        title,
        description: "Insert one digit in the open numeric Setting editor",
        slash: None,
        keybinding: None,
    }
}

const fn numeric_insert_descriptors() -> [SemanticCommandDescriptor; 10] {
    let mut descriptors = [numeric_insert_descriptor(
        SemanticCommandId::SettingsNumericInsert(NumericDigit::Zero),
        NumericDigit::Zero.title(),
    ); 10];
    let mut index = 0;
    while index < NumericDigit::ALL.len() {
        let digit = NumericDigit::ALL[index];
        descriptors[index] = numeric_insert_descriptor(
            SemanticCommandId::SettingsNumericInsert(digit),
            digit.title(),
        );
        index += 1;
    }
    descriptors
}

const NUMERIC_INSERT_COMMANDS: [SemanticCommandDescriptor; 10] = numeric_insert_descriptors();

const SEMANTIC_COMMANDS: &[SemanticCommandDescriptor] = &[
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApplicationExit,
        title: "Exit Suru",
        description: "Close this TUI",
        slash: Some(SlashCommand {
            name: "exit",
            aliases: &["quit"],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelList,
        title: "Choose Model",
        description: "Search available Provider Models",
        slash: Some(SlashCommand {
            name: "models",
            aliases: &["mo"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('m'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X M",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptions,
        title: "Configure Model Options",
        description: "Configure the current Model's options",
        slash: Some(SlashCommand {
            name: "options",
            aliases: &["variants"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('o'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X O",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsPrevious,
        title: "Previous Model Option",
        description: "Focus the previous Model Option or choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsNext,
        title: "Next Model Option",
        description: "Focus the next Model Option or choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsSelect,
        title: "Select Model Option",
        description: "Open or select the focused Model Option choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsApply,
        title: "Apply Model Options",
        description: "Apply the complete staged Agent Selection",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsCancel,
        title: "Cancel Model Options",
        description: "Discard every staged Model Option edit",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionReasoningCycle,
        title: "Cycle Reasoning Effort",
        description: "Advance the Reasoning Effort to the next advertised choice",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+T",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionList,
        title: "Switch Session",
        description: "Search and attach to a live Session",
        slash: Some(SlashCommand {
            name: "sessions",
            aliases: &["resume", "continue"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('l'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X L",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionDelete,
        title: "Delete Session",
        description: "Delete the selected Session and everything it owns",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TranscriptFoldsToggle,
        title: "Toggle Transcript Folds",
        description: "Show every Transcript entry in full, or fold them back down",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('f'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X F",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TranscriptGroupsToggle,
        title: "Toggle Transcript Groups",
        description: "Expand every command Group into its members, or collapse them back down",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X G",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TranscriptTurnToggle,
        title: "Toggle Turn Fold",
        description: "Fold one settled Turn to its marker, or open the work behind it",
        // The command names the Turn it acts on, so it is invoked from that
        // Turn's marker rather than from a key or a slash that would have no
        // way to say which Turn it meant.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TranscriptTurnsToggle,
        title: "Toggle Turn Folds",
        description: "Open every settled Turn's Fold, or fold them back down",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X T",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionNew,
        title: "New Session",
        description: "Open a fresh landing composer without ending the current Session",
        slash: Some(SlashCommand {
            name: "new",
            aliases: &["clear"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X N",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionSettle,
        title: "Settle Session",
        // The command acts on the Session it names, and names the open one
        // when nothing else says otherwise — which is what the slash means and
        // what a Sidebar row overrules.
        description: "Set a Session aside as done for now",
        slash: Some(SlashCommand {
            name: "settle",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionUnsettle,
        title: "Unsettle Session",
        description: "Take a Session back off the settled shelf",
        slash: Some(SlashCommand {
            name: "unsettle",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarToggle,
        title: "Toggle Sidebar",
        description: "Show the Sidebar beside the main view, or reclaim its columns",
        slash: Some(SlashCommand {
            name: "sidebar",
            aliases: &[],
        }),
        // Direct rather than behind the leader, because t3 code's own Ctrl+B is
        // the binding a reader arrives with.
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+B",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarPrevious,
        title: "Previous Sidebar Session",
        description: "Move the Sidebar's selection to the row above, wrapping past the top",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarNext,
        title: "Next Sidebar Session",
        description: "Move the Sidebar's selection to the row below, wrapping past the end",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarAttach,
        title: "Attach Selected Session",
        // Acting on the settled shelf's own affordance shows more of the shelf
        // rather than opening anything, so the account of the command says so.
        description: "Open the Session the Sidebar has selected, or show more of the settled shelf",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarLeave,
        title: "Leave Sidebar",
        // Backing out of a search is the inner step of backing out of the
        // Sidebar, so the account of the command says which one it takes.
        description: "Clear the Sidebar's search, or hand the keys back to the composer",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuPrevious,
        title: "Previous Sidebar Menu Item",
        description: "Move the Sidebar menu's selection to the item above, wrapping past the top",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuNext,
        title: "Next Sidebar Menu Item",
        description: "Move the Sidebar menu's selection to the item below, wrapping past the end",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuSelect,
        title: "Invoke Sidebar Menu Item",
        // Delete asks again rather than acting, so the account of the command
        // says that acting on an item is not always the end of it.
        description: "Act on the Sidebar menu's selected item, or ask it to confirm",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuClose,
        title: "Close Sidebar Menu",
        description: "Dismiss the Sidebar's context menu, leaving its row alone",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsOpen,
        title: "Settings",
        description: "View and edit every Setting",
        slash: Some(SlashCommand {
            name: "settings",
            aliases: &["config", "preferences"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char(','),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X ,",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsPrevious,
        title: "Previous Setting",
        description: "Focus the previous Setting",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNext,
        title: "Next Setting",
        description: "Focus the next Setting",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsTabPrevious,
        title: "Previous Settings Tab",
        description: "Show the settings tab before this one, wrapping past the first",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsTabNext,
        title: "Next Settings Tab",
        description: "Show the settings tab after this one, wrapping past the last",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsRowOpen,
        title: "Open Settings Row",
        description: "Open what the focused row stands for: a Provider's further Settings, or the surface its value is chosen at",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsValueCycle,
        title: "Cycle Setting Value",
        description: "Pin the focused Setting's next value, wrapping past the last",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsReset,
        title: "Reset Setting",
        description: "Unpin the focused Setting so its built-in default resumes",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsClose,
        title: "Close Settings",
        description: "Leave the settings panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericDeleteBackward,
        title: "Delete Numeric Digit",
        description: "Delete the last digit in the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericApply,
        title: "Apply Numeric Setting",
        description: "Validate and apply the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericCancel,
        title: "Cancel Numeric Setting",
        description: "Discard the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
];

pub(super) fn descriptor(id: SemanticCommandId) -> &'static SemanticCommandDescriptor {
    SEMANTIC_COMMANDS
        .iter()
        .chain(NUMERIC_INSERT_COMMANDS.iter())
        .find(|command| command.id == id)
        .expect("every semantic command ID has one descriptor")
}

pub(super) fn command_for_leader_key(key: KeyEvent) -> Option<SemanticCommandId> {
    command_for_semantic_binding(key, Some(LEADER_PREFIX))
}

pub(super) fn command_for_direct_semantic_key(key: KeyEvent) -> Option<SemanticCommandId> {
    command_for_semantic_binding(key, None)
}

fn command_for_semantic_binding(
    key: KeyEvent,
    prefix: Option<(KeyCode, KeyModifiers)>,
) -> Option<SemanticCommandId> {
    SEMANTIC_COMMANDS.iter().find_map(|command| {
        command.keybinding.and_then(|binding| {
            (binding.prefix == prefix
                && binding.code == key.code
                && binding.modifiers == key.modifiers)
                .then_some(command.id)
        })
    })
}

pub(super) fn slash_trigger(text: &str, cursor: usize) -> Option<(&str, std::ops::Range<usize>)> {
    if cursor != text.len() || text.contains('\n') {
        return None;
    }
    let query = text.strip_prefix('/')?;
    (!query.chars().any(char::is_whitespace)).then_some((query, 0..text.len()))
}

pub(super) fn command_matches(query: &str) -> Vec<SemanticCommandId> {
    let mut matches = SEMANTIC_COMMANDS
        .iter()
        .filter_map(|command| {
            let slash = command.slash?;
            let score = fuzzy_score(query, slash.name)
                .into_iter()
                .chain(
                    slash
                        .aliases
                        .iter()
                        .filter_map(|alias| fuzzy_score(query, alias).map(|score| score + 10)),
                )
                .chain(fuzzy_score(query, command.title).map(|score| score + 20))
                .chain(fuzzy_score(query, command.description).map(|score| score + 20))
                .min()?;
            Some((score, slash.name, command.id.as_str(), command.id))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable_by_key(|(score, slash, id, _)| (*score, *slash, *id));
    matches
        .into_iter()
        .take(AUTOCOMPLETE_LIMIT)
        .map(|(_, _, _, id)| id)
        .collect()
}

pub(super) fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    if candidate == query {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(100 + candidate.len().saturating_sub(query.len()));
    }
    if let Some(index) = candidate.find(&query) {
        return Some(200 + index);
    }

    let mut positions = candidate.char_indices();
    let mut previous = None;
    let mut gaps = 0;
    for query_character in query.chars() {
        let (position, _) =
            positions.find(|(_, candidate_character)| *candidate_character == query_character)?;
        if let Some(previous) = previous {
            gaps += position.saturating_sub(previous + 1);
        }
        previous = Some(position);
    }
    Some(300 + gaps)
}
