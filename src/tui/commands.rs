//! Typed semantic commands and slash autocomplete state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::protocol::TurnId;

const AUTOCOMPLETE_LIMIT: usize = 10;

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
    SettingsOpen,
    SettingsPrevious,
    SettingsNext,
    SettingsTabPrevious,
    SettingsTabNext,
    SettingsRowOpen,
    SettingsValueCycle,
    SettingsReset,
    SettingsClose,
    TranscriptFoldsToggle,
    TranscriptGroupsToggle,
    TranscriptTurnToggle,
    TranscriptTurnsToggle,
}

/// What a semantic command acts on. Most act on the view as a whole; one that
/// names a subject carries it here rather than letting the surface that
/// invoked it reach into view state itself, so a click, a keybinding, and a
/// future plugin all drive the very same command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SemanticSubject {
    View,
    Turn(TurnId),
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
            Self::SettingsOpen => "settings.open",
            Self::SettingsPrevious => "settings.previous",
            Self::SettingsNext => "settings.next",
            Self::SettingsTabPrevious => "settings.tab.previous",
            Self::SettingsTabNext => "settings.tab.next",
            Self::SettingsRowOpen => "settings.row.open",
            Self::SettingsValueCycle => "settings.value.cycle",
            Self::SettingsReset => "settings.reset",
            Self::SettingsClose => "settings.close",
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
];

pub(super) fn descriptor(id: SemanticCommandId) -> &'static SemanticCommandDescriptor {
    SEMANTIC_COMMANDS
        .iter()
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

#[derive(Clone, Debug, Default)]
pub(super) struct CommandAutocomplete {
    query: Option<String>,
    dismissed_text: Option<String>,
    matches: Vec<SemanticCommandId>,
    selected: usize,
}

impl CommandAutocomplete {
    pub(super) fn sync(&mut self, text: &str, cursor: usize) {
        if self.dismissed_text.as_deref() != Some(text) {
            self.dismissed_text = None;
        }
        let Some(query) = slash_query(text, cursor) else {
            self.hide();
            return;
        };
        if self.dismissed_text.as_deref() == Some(text) {
            self.hide();
            return;
        }
        if self.query.as_deref() != Some(query) {
            self.selected = 0;
        }
        self.query = Some(query.to_owned());
        self.matches = autocomplete_matches(query);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    pub(super) fn dismiss_for_text(&mut self, text: &str) {
        self.dismissed_text = Some(text.to_owned());
        self.hide();
    }

    pub(super) fn is_visible(&self) -> bool {
        !self.matches.is_empty()
    }

    pub(super) fn selected(&self) -> Option<SemanticCommandId> {
        self.matches.get(self.selected).copied()
    }

    pub(super) fn select_previous(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
        }
    }

    pub(super) fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub(super) fn rows(
        &self,
    ) -> impl ExactSizeIterator<Item = (bool, &'static SemanticCommandDescriptor)> + '_ {
        self.matches
            .iter()
            .enumerate()
            .map(|(index, id)| (index == self.selected, descriptor(*id)))
    }

    pub(super) fn visible_rows(
        &self,
        capacity: usize,
    ) -> impl Iterator<Item = (bool, &'static SemanticCommandDescriptor)> + '_ {
        let start = self.selected.saturating_add(1).saturating_sub(capacity);
        self.rows().skip(start).take(capacity)
    }

    fn hide(&mut self) {
        self.query = None;
        self.matches.clear();
        self.selected = 0;
    }
}

fn slash_query(text: &str, cursor: usize) -> Option<&str> {
    if cursor != text.len() || text.contains('\n') {
        return None;
    }
    let query = text.strip_prefix('/')?;
    (!query.chars().any(char::is_whitespace)).then_some(query)
}

fn autocomplete_matches(query: &str) -> Vec<SemanticCommandId> {
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

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
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
