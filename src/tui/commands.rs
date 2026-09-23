//! Typed semantic commands and the Command completion mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;

use super::text_layout::CursorTarget;

use crate::protocol::{
    ApprovalId, Outlook, QuestionnaireId, SessionReference, TurnId, WorkspaceId,
};

pub(super) const AUTOCOMPLETE_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticCommandId {
    ApprovalOpen,
    ApprovalHide,
    ApprovalChoicePrevious,
    ApprovalChoiceNext,
    ApprovalChoose,
    ApprovalAccept,
    ApprovalAcceptForSession,
    ApprovalDecline,
    ApprovalDeclineAndInterrupt,
    ApprovalPostureCycle,
    ApprovalPostureOpen,
    ApprovalPosturePrevious,
    ApprovalPostureNext,
    ApprovalPostureSelect,
    ApprovalPostureClose,

    QuestionnaireScrollUp,
    QuestionnaireScrollDown,
    QuestionnaireRequestPrevious,
    QuestionnaireRequestNext,
    QuestionnaireBack,
    QuestionnaireNext,
    QuestionnaireOmit,
    QuestionnaireOpen,
    QuestionnaireHide,
    QuestionnaireChoicePrevious,
    QuestionnaireChoiceNext,
    QuestionnaireSelect,
    QuestionnaireReview,
    QuestionnaireSubmit,
    QuestionnaireDecline,

    ApplicationExit,
    ComposerPlaceCursor,
    PointerClick,
    PointerDrag,
    HyperlinkOpen,
    TextSelectionCopy,
    TextSelectionClear,
    TextSelectionWord,
    TextSelectionLine,
    ThemeList,
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
    /// Opens the Icon Picker over a Session, which the picker only does while
    /// `appearance.showIcons` is on: inert with it off, so a header press or a
    /// menu item stays quiet rather than opening a picker no glyph could draw.
    SessionIconChoose,
    /// Opens the Icon Picker over a Workspace, on the same terms
    /// [`Self::SessionIconChoose`] opens it over a Session: inert while
    /// `appearance.showIcons` is off.
    WorkspaceIconChoose,
    IconPickerLeft,
    IconPickerRight,
    IconPickerUp,
    IconPickerDown,
    IconPickerChoose,
    IconPickerClose,
    IconPickerSearchInsert,
    IconPickerSearchDelete,
    /// Invokes the Workspace Picker row menu's one item — Choose icon — the
    /// way Enter acts on the Sidebar's own row menu.
    WorkspacePickerMenuSelect,
    /// Dismisses the Workspace Picker row menu, leaving its row alone.
    WorkspacePickerMenuClose,
    ConnectOpen,
    /// Opens the Connect overlay straight on Invite entry, where a Pairing is
    /// formed, rather than on the picker that chooses among formed ones.
    PairOpen,
    ConnectConfirm,
    ConnectFocusNext,
    ConnectPrevious,
    ConnectNext,
    OutlookSelect,
    ConnectMoveAddressUp,
    ConnectMoveAddressDown,
    ConnectPairAnother,
    /// Arms the selected Remote's removal, and removes it when it is already
    /// armed: ending a Pairing is asked for twice.
    ConnectRemoveRemote,
    ConnectClose,
    ServeOpen,
    ServePrevious,
    ServeNext,
    ServeToggleAddress,
    ServeConfirm,
    ServeCopyInvite,
    ServeRemovePeer,
    ServeClose,
    SidebarToggle,
    SidebarWiden,
    SidebarNarrow,
    SidebarWidthSet {
        columns: u64,
    },
    SidebarWidthReset,
    SidebarPrevious,
    SidebarNext,
    SidebarAttach,
    SidebarLeave,
    SidebarMenuPrevious,
    SidebarMenuNext,
    SidebarMenuSelect,
    SidebarMenuClose,
    RemoteRetry,
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
    SubagentBrowse,
    SubagentOpen,
    SubagentStop,
    SubagentLeave,
    TranscriptFoldsToggle,
    TranscriptGroupsToggle,
    TranscriptTurnToggle,
    TranscriptTurnsToggle,
    WorkspaceList,
    WorktreeRemove,
    WorktreeForceRemove,
    WorktreeList,
    WorktreePrevious,
    WorktreeNext,
    WorktreeSelect,
    WorktreeClose,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SemanticSubject {
    View,
    ComposerCursor(CursorTarget),
    ScreenPosition(Position),
    Hyperlink(String),
    /// Typed text a command carries as its own payload rather than acting
    /// on anything already in view state — the Icon Picker's search insert is
    /// the first of these.
    Text(String),
    Turn(TurnId),
    Questionnaire(QuestionnaireId),
    Approval(ApprovalId),
    Origin(Outlook),
    /// One Session, which is what a reader names by acting on its row rather
    /// than on the Session they have open.
    Session(SessionReference),
    /// One Workspace, named by the Origin it stands on and its own identity —
    /// what a Sidebar selector entry or a Workspace Picker row names it by,
    /// since neither carries a Session to name it through.
    Workspace {
        origin: Outlook,
        workspace_id: WorkspaceId,
    },
}

/// One invocation of a semantic command: which command, and what it acts on.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    pub(super) const fn on_approval(self, id: ApprovalId) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Approval(id),
        }
    }

    pub(super) const fn on_questionnaire(self, id: QuestionnaireId) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Questionnaire(id),
        }
    }

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
    pub(super) fn on_session(self, session: SessionReference) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Session(session),
        }
    }

    /// This command invoked against one Workspace, which is what a Sidebar
    /// selector entry's or a Workspace Picker row's own context menu names it
    /// by.
    pub(super) fn on_workspace(
        self,
        origin: Outlook,
        workspace_id: WorkspaceId,
    ) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Workspace {
                origin,
                workspace_id,
            },
        }
    }

    /// This command invoked against one Origin, which is what an unreachable
    /// Remote row names when the reader retries it.
    pub(super) fn on_origin(self, outlook: Outlook) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Origin(outlook),
        }
    }

    pub(super) fn on_hyperlink(self, target: String) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Hyperlink(target),
        }
    }

    /// This command invoked with typed text as its own payload, which is what
    /// the Icon Picker's search insert carries in place of a target.
    pub(super) fn on_text(self, text: String) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Text(text),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalOpen => "approval.open",
            Self::ApprovalHide => "approval.hide",
            Self::ApprovalChoicePrevious => "approval.choice.previous",
            Self::ApprovalChoiceNext => "approval.choice.next",
            Self::ApprovalChoose => "approval.choose",
            Self::ApprovalAccept => "approval.decision.accept",
            Self::ApprovalAcceptForSession => "approval.decision.accept-for-session",
            Self::ApprovalDecline => "approval.decision.decline",
            Self::ApprovalDeclineAndInterrupt => "approval.decision.decline-and-interrupt",
            Self::ApprovalPostureCycle => "approval-posture.cycle",
            Self::ApprovalPostureOpen => "approval-posture.open",
            Self::ApprovalPosturePrevious => "approval-posture.previous",
            Self::ApprovalPostureNext => "approval-posture.next",
            Self::ApprovalPostureSelect => "approval-posture.select",
            Self::ApprovalPostureClose => "approval-posture.close",
            Self::QuestionnaireScrollUp => "questionnaire.scroll.up",
            Self::QuestionnaireScrollDown => "questionnaire.scroll.down",
            Self::QuestionnaireRequestPrevious => "questionnaire.request.previous",
            Self::QuestionnaireRequestNext => "questionnaire.request.next",
            Self::QuestionnaireBack => "questionnaire.back",
            Self::QuestionnaireNext => "questionnaire.next",
            Self::QuestionnaireOmit => "questionnaire.omit",
            Self::QuestionnaireOpen => "questionnaire.open",
            Self::QuestionnaireHide => "questionnaire.hide",
            Self::QuestionnaireChoicePrevious => "questionnaire.choice.previous",
            Self::QuestionnaireChoiceNext => "questionnaire.choice.next",
            Self::QuestionnaireSelect => "questionnaire.select",
            Self::QuestionnaireReview => "questionnaire.review",
            Self::QuestionnaireSubmit => "questionnaire.submit",
            Self::QuestionnaireDecline => "questionnaire.decline",
            Self::PointerClick => "pointer.click",
            Self::PointerDrag => "pointer.drag",
            Self::HyperlinkOpen => "hyperlink.open",
            Self::TextSelectionCopy => "text_selection.copy",
            Self::TextSelectionClear => "text_selection.clear",
            Self::TextSelectionWord => "text_selection.word",
            Self::TextSelectionLine => "text_selection.line",
            Self::ComposerPlaceCursor => "composer.cursor.place",
            Self::ApplicationExit => "application.exit",
            Self::ThemeList => "theme.list",
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
            Self::SessionIconChoose => "session.icon.choose",
            Self::WorkspaceIconChoose => "workspace.icon.choose",
            Self::IconPickerLeft => "icon-picker.left",
            Self::IconPickerRight => "icon-picker.right",
            Self::IconPickerUp => "icon-picker.up",
            Self::IconPickerDown => "icon-picker.down",
            Self::IconPickerChoose => "icon-picker.choose",
            Self::IconPickerClose => "icon-picker.close",
            Self::IconPickerSearchInsert => "icon-picker.search.insert",
            Self::IconPickerSearchDelete => "icon-picker.search.delete-backward",
            Self::WorkspacePickerMenuSelect => "workspace-picker.menu.select",
            Self::WorkspacePickerMenuClose => "workspace-picker.menu.close",
            Self::ConnectOpen => "connect.open",
            Self::PairOpen => "pair.open",
            Self::ConnectConfirm => "connect.confirm",
            Self::ConnectFocusNext => "connect.focus.next",
            Self::ConnectPrevious => "connect.previous",
            Self::ConnectNext => "connect.next",
            Self::OutlookSelect => "outlook.select",
            Self::ConnectMoveAddressUp => "connect.address.move-up",
            Self::ConnectMoveAddressDown => "connect.address.move-down",
            Self::ConnectPairAnother => "connect.pair-another",
            Self::ConnectRemoveRemote => "connect.remove",
            Self::ConnectClose => "connect.close",
            Self::ServeOpen => "serve.open",
            Self::ServePrevious => "serve.previous",
            Self::ServeNext => "serve.next",
            Self::ServeToggleAddress => "serve.address.toggle",
            Self::ServeConfirm => "serve.confirm",
            Self::ServeCopyInvite => "serve.invite.copy",
            Self::ServeRemovePeer => "serve.peer.remove",
            Self::ServeClose => "serve.close",
            Self::SidebarToggle => "sidebar.toggle",
            Self::SidebarWiden => "sidebar.width.widen",
            Self::SidebarNarrow => "sidebar.width.narrow",
            Self::SidebarWidthSet { .. } => "sidebar.width.set",
            Self::SidebarWidthReset => "sidebar.width.reset",
            Self::SidebarPrevious => "sidebar.previous",
            Self::SidebarNext => "sidebar.next",
            Self::SidebarAttach => "sidebar.attach",
            Self::SidebarLeave => "sidebar.leave",
            Self::SidebarMenuPrevious => "sidebar.menu.previous",
            Self::SidebarMenuNext => "sidebar.menu.next",
            Self::SidebarMenuSelect => "sidebar.menu.select",
            Self::SidebarMenuClose => "sidebar.menu.close",
            Self::RemoteRetry => "remote.retry",
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
            Self::SubagentBrowse => "subagent.browse",
            Self::SubagentOpen => "subagent.open",
            Self::SubagentStop => "subagent.stop",
            Self::SubagentLeave => "subagent.leave",
            Self::TranscriptFoldsToggle => "transcript.folds.toggle",
            Self::TranscriptGroupsToggle => "transcript.groups.toggle",
            Self::TranscriptTurnToggle => "transcript.turn.fold.toggle",
            Self::TranscriptTurnsToggle => "transcript.turns.toggle",
            Self::WorkspaceList => "workspace.list",
            Self::WorktreeRemove => "worktree.remove",
            Self::WorktreeForceRemove => "worktree.force_remove",
            Self::WorktreeList => "worktree.list",
            Self::WorktreePrevious => "worktree.previous",
            Self::WorktreeNext => "worktree.next",
            Self::WorktreeSelect => "worktree.select",
            Self::WorktreeClose => "worktree.close",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SemanticCommandDescriptor {
    pub(super) id: SemanticCommandId,
    pub(super) title: &'static str,
    pub(super) description: &'static str,
    pub(super) reach: SemanticReach,
    pub(super) slash: Option<SlashCommand>,
    pub(super) keybinding: Option<SemanticKeybinding>,
}

/// How far a semantic command's work has to travel. It is declared beside the
/// command itself, so the one fact that decides whether an Origin which has
/// stopped answering refuses a command lives where the command is defined
/// rather than in a separate list that has to be kept in step with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SemanticReach {
    /// Moves only what the Client already holds: reading, scrolling, opening
    /// and closing surfaces, choosing a Theme, turning the Outlook, leaving.
    /// Nothing has to answer for it.
    Client,
    /// Goes to a Server — a Prompt, an Intervention answered, a Session
    /// settled, unsettled, deleted or begun, a picker or catalog that has to
    /// fetch. An Origin that cannot be reached refuses these and no others.
    Origin,
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
        reach: SemanticReach::Client,
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
        id: SemanticCommandId::ApprovalPostureCycle,
        title: "Cycle Approval Posture",
        reach: SemanticReach::Origin,
        description: "Advance this Session's Provider-native Approval Posture",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+G",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalPostureOpen,
        title: "Choose Approval Posture",
        reach: SemanticReach::Origin,
        description: "Choose every Provider-native Approval Posture value or follow Settings",
        slash: Some(SlashCommand {
            name: "posture",
            aliases: &["approvals"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X P",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalPosturePrevious,
        title: "Previous Approval Posture",
        reach: SemanticReach::Client,
        description: "Focus the previous Approval Posture choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalPostureNext,
        title: "Next Approval Posture",
        reach: SemanticReach::Client,
        description: "Focus the next Approval Posture choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalPostureSelect,
        title: "Select Approval Posture",
        reach: SemanticReach::Origin,
        description: "Apply the focused Approval Posture choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalPostureClose,
        title: "Close Approval Posture Picker",
        reach: SemanticReach::Client,
        description: "Close the Approval Posture picker",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalOpen,
        title: "Open Approval",
        reach: SemanticReach::Origin,
        description: "Focus a pending Approval and choose a Decision",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+Y",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalHide,
        title: "Hide Approval",
        reach: SemanticReach::Client,
        description: "Return focus to the composer without deciding",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalChoicePrevious,
        title: "Previous Decision",
        reach: SemanticReach::Client,
        description: "Focus the previous Decision without submitting it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalChoiceNext,
        title: "Next Decision",
        reach: SemanticReach::Client,
        description: "Focus the next Decision without submitting it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalChoose,
        title: "Choose Decision",
        reach: SemanticReach::Origin,
        description: "Submit the focused Decision",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalAccept,
        title: "Accept Approval",
        reach: SemanticReach::Origin,
        description: "Accept this request once",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalAcceptForSession,
        title: "Accept Approval for Session",
        reach: SemanticReach::Origin,
        description: "Accept this kind of request for the Session",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalDecline,
        title: "Decline Approval",
        reach: SemanticReach::Origin,
        description: "Decline this request and let the Turn continue",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApprovalDeclineAndInterrupt,
        title: "Decline Approval and Interrupt",
        reach: SemanticReach::Origin,
        description: "Decline this request and interrupt the Turn",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireRequestNext,
        title: "Next pending Questionnaire",
        reach: SemanticReach::Client,
        description: "Switch pending Questionnaires without losing the Answer draft",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireRequestPrevious,
        title: "Previous pending Questionnaire",
        reach: SemanticReach::Client,
        description: "Switch pending Questionnaires without losing the Answer draft",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireScrollUp,
        title: "Scroll Questionnaire Up",
        reach: SemanticReach::Client,
        description: "Read more of the Question",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireScrollDown,
        title: "Scroll Questionnaire Down",
        reach: SemanticReach::Client,
        description: "Read more of the Question",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireBack,
        title: "Previous Question",
        reach: SemanticReach::Client,
        description: "Return to the previous Question without discarding edits",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireNext,
        title: "Next Question",
        reach: SemanticReach::Client,
        description: "Validate this Question and advance toward review",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireOmit,
        title: "Omit Question",
        reach: SemanticReach::Client,
        description: "Leave a Question unanswered only when the Provider permits it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireOpen,
        title: "Open Questionnaire",
        reach: SemanticReach::Origin,
        description: "Open the structured Answer panel",
        slash: Some(SlashCommand {
            name: "questions",
            aliases: &["answer"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+Q",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireHide,
        title: "Hide Questionnaire",
        reach: SemanticReach::Client,
        description: "Hide the structured Answer panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireChoicePrevious,
        title: "Previous choice",
        reach: SemanticReach::Client,
        description: "Highlight the previous choice without selecting it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireChoiceNext,
        title: "Next choice",
        reach: SemanticReach::Client,
        description: "Highlight the next choice without selecting it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireSelect,
        title: "Select Questionnaire",
        reach: SemanticReach::Client,
        description: "Select the structured Answer panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireReview,
        title: "Review Questionnaire",
        reach: SemanticReach::Client,
        description: "Review the structured Answer panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireSubmit,
        title: "Submit Questionnaire",
        reach: SemanticReach::Origin,
        description: "Submit the structured Answer panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::QuestionnaireDecline,
        title: "Decline Questionnaire",
        reach: SemanticReach::Origin,
        description: "Decline the structured Answer panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ApplicationExit,
        title: "Exit Suru",
        reach: SemanticReach::Client,
        description: "Close this TUI",
        slash: Some(SlashCommand {
            name: "exit",
            aliases: &["quit"],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ThemeList,
        title: "Choose Theme",
        reach: SemanticReach::Client,
        description: "Search and preview Themes",
        slash: Some(SlashCommand {
            name: "themes",
            aliases: &["theme"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X T",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelList,
        title: "Choose Model",
        reach: SemanticReach::Origin,
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
        reach: SemanticReach::Origin,
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
        reach: SemanticReach::Client,
        description: "Focus the previous Model Option or choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsNext,
        title: "Next Model Option",
        reach: SemanticReach::Client,
        description: "Focus the next Model Option or choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsSelect,
        title: "Select Model Option",
        reach: SemanticReach::Client,
        description: "Open or select the focused Model Option choice",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsApply,
        title: "Apply Model Options",
        reach: SemanticReach::Origin,
        description: "Apply the complete staged Agent Selection",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionsCancel,
        title: "Cancel Model Options",
        reach: SemanticReach::Client,
        description: "Discard every staged Model Option edit",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ModelOptionReasoningCycle,
        title: "Cycle Reasoning Effort",
        reach: SemanticReach::Origin,
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
        reach: SemanticReach::Client,
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
        id: SemanticCommandId::WorktreeRemove,
        title: "Remove selected Worktree",
        reach: SemanticReach::Origin,
        description: "Remove selected Worktree",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreeForceRemove,
        title: "Force remove selected Worktree",
        reach: SemanticReach::Origin,
        description: "Force remove selected Worktree",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreePrevious,
        title: "Previous Worktree",
        reach: SemanticReach::Client,
        description: "Previous Worktree",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreeNext,
        title: "Next Worktree",
        reach: SemanticReach::Client,
        description: "Next Worktree",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreeSelect,
        title: "Select Worktree",
        reach: SemanticReach::Origin,
        description: "Select Worktree",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreeClose,
        title: "Close Worktree chooser",
        reach: SemanticReach::Client,
        description: "Close Worktree chooser",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorktreeList,
        title: "Choose Worktree",
        reach: SemanticReach::Origin,
        description: "Choose an existing Worktree, or a new one, for a new Session",
        slash: Some(SlashCommand {
            name: "worktree",
            aliases: &["checkout"],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceList,
        title: "Switch Workspace",
        reach: SemanticReach::Origin,
        description: "Choose the Workspace to work in",
        // "Project" is vocabulary Suru avoids, so it stands only as a typed
        // synonym: an alias is matchable and never displayed, which lets
        // whichever word a reader's fingers reach for find the one command.
        slash: Some(SlashCommand {
            name: "workspace",
            aliases: &["project"],
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X W",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionDelete,
        title: "Delete Session",
        reach: SemanticReach::Origin,
        description: "Delete the selected Session and everything it owns",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TranscriptFoldsToggle,
        title: "Toggle Transcript Folds",
        reach: SemanticReach::Client,
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
        reach: SemanticReach::Client,
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
        reach: SemanticReach::Client,
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
        reach: SemanticReach::Client,
        description: "Open every settled Turn's Fold, or fold them back down",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionNew,
        title: "New Session",
        reach: SemanticReach::Origin,
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
        reach: SemanticReach::Origin,
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
        reach: SemanticReach::Origin,
        description: "Take a Session back off the settled shelf",
        slash: Some(SlashCommand {
            name: "unsettle",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionIconChoose,
        title: "Choose Session Icon",
        reach: SemanticReach::Origin,
        // The command names the Session it acts on, so it is invoked from a
        // Sidebar row's context menu or a press on that Session's header Icon
        // rather than from a key or a slash that would have no way to say
        // which Session it meant.
        description: "Open the Icon Picker over a Session, while Icons are shown",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceIconChoose,
        title: "Choose Workspace Icon",
        reach: SemanticReach::Origin,
        // The command names the Workspace it acts on by Origin and identity,
        // so it is invoked from a Sidebar selector entry's context menu or a
        // Workspace Picker row's context menu rather than from a key or a
        // slash that would have no way to say which Workspace it meant.
        description: "Open the Icon Picker over a Workspace, while Icons are shown",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerLeft,
        title: "Icon Picker Left",
        reach: SemanticReach::Client,
        description: "Move the Icon Picker's focus one cell left",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerRight,
        title: "Icon Picker Right",
        reach: SemanticReach::Client,
        description: "Move the Icon Picker's focus one cell right",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerUp,
        title: "Icon Picker Up",
        reach: SemanticReach::Client,
        description: "Move the Icon Picker's focus one row up",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerDown,
        title: "Icon Picker Down",
        reach: SemanticReach::Client,
        description: "Move the Icon Picker's focus one row down",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerChoose,
        title: "Choose Icon",
        reach: SemanticReach::Origin,
        description: "Set the target's Icon to the Icon Picker's focused glyph and close it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerClose,
        title: "Close Icon Picker",
        reach: SemanticReach::Client,
        description: "Dismiss the Icon Picker without choosing",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerSearchInsert,
        title: "Insert Icon Search Text",
        reach: SemanticReach::Client,
        description: "Narrow the Icon Picker's grid by typed text",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::IconPickerSearchDelete,
        title: "Delete Icon Search Text",
        reach: SemanticReach::Client,
        description: "Widen the Icon Picker's grid by deleting the last typed character",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspacePickerMenuSelect,
        title: "Invoke Workspace Picker Menu Item",
        reach: SemanticReach::Client,
        description: "Act on the Workspace Picker row menu's selected item",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspacePickerMenuClose,
        title: "Close Workspace Picker Menu",
        reach: SemanticReach::Client,
        description: "Dismiss the Workspace Picker's row menu, leaving its row alone",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectOpen,
        title: "Choose a Remote",
        reach: SemanticReach::Client,
        description: "Turn the Outlook toward a paired Remote or Local",
        slash: Some(SlashCommand {
            name: "connect",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::PairOpen,
        title: "Pair with a Remote",
        reach: SemanticReach::Client,
        description: "Redeem an Invite from another machine",
        slash: Some(SlashCommand {
            name: "pair",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectConfirm,
        title: "Confirm Connect Step",
        reach: SemanticReach::Client,
        description: "Inspect the Invite or trust its fingerprint",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectFocusNext,
        title: "Next Connect Field",
        reach: SemanticReach::Client,
        description: "Move between the Remote name and address priorities",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectPrevious,
        title: "Previous Connect Item",
        reach: SemanticReach::Client,
        description: "Focus the previous Remote or address",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectNext,
        title: "Next Connect Item",
        reach: SemanticReach::Client,
        description: "Focus the next Remote or address",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OutlookSelect,
        title: "Turn Outlook",
        reach: SemanticReach::Client,
        description: "Present the selected local or Remote Server",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectMoveAddressUp,
        title: "Raise Address Priority",
        reach: SemanticReach::Client,
        description: "Move the focused address earlier in the dialing order",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectMoveAddressDown,
        title: "Lower Address Priority",
        reach: SemanticReach::Client,
        description: "Move the focused address later in the dialing order",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectRemoveRemote,
        title: "Remove Remote",
        reach: SemanticReach::Client,
        description: "End the Pairing with the selected Remote",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectPairAnother,
        title: "Pair Another Remote",
        reach: SemanticReach::Client,
        description: "Open Invite entry from the paired Remotes picker",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectClose,
        title: "Close Connect Overlay",
        reach: SemanticReach::Client,
        description: "Dismiss the Remote Pairing surface",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeOpen,
        title: "Serve this machine",
        reach: SemanticReach::Client,
        description: "Issue an Invite and manage enrolled Peers",
        slash: Some(SlashCommand {
            name: "serve",
            aliases: &[],
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServePrevious,
        title: "Previous Serve Item",
        reach: SemanticReach::Client,
        description: "Focus the previous address or Peer",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeNext,
        title: "Next Serve Item",
        reach: SemanticReach::Client,
        description: "Focus the next address or Peer",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeToggleAddress,
        title: "Toggle Invite Address",
        reach: SemanticReach::Client,
        description: "Include or exclude the focused address from the Invite",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeConfirm,
        title: "Confirm Serve Selection",
        reach: SemanticReach::Client,
        description: "Issue an Invite for the chosen addresses",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeCopyInvite,
        title: "Copy Invite",
        reach: SemanticReach::Client,
        description: "Copy the fresh Invite through the terminal",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeRemovePeer,
        title: "Remove Peer",
        reach: SemanticReach::Client,
        description: "Revoke the focused Peer and end its live connections",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ServeClose,
        title: "Close Serve Overlay",
        reach: SemanticReach::Client,
        description: "Dismiss the Serving management surface",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarToggle,
        title: "Toggle Sidebar",
        reach: SemanticReach::Client,
        description: "Show the Sidebar and give it the keys, give a shown Sidebar the keys, or hide one holding them",
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
        id: SemanticCommandId::SidebarWiden,
        title: "Widen Sidebar",
        reach: SemanticReach::Client,
        description: "Move the Sidebar's chosen edge one column to the right",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarNarrow,
        title: "Narrow Sidebar",
        reach: SemanticReach::Client,
        description: "Move the Sidebar's chosen edge one column to the left",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The column count is invocation data. This representative lets the
        // one descriptor cover every explicit width without enumerating an
        // open integer command in the registry.
        id: SemanticCommandId::SidebarWidthSet { columns: 0 },
        title: "Set Sidebar Width",
        reach: SemanticReach::Client,
        description: "Set the Sidebar's chosen width to an explicit column count",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarWidthReset,
        title: "Reset Sidebar Width",
        reach: SemanticReach::Client,
        description: "Restore the Sidebar width from its current launch Setting",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarPrevious,
        title: "Previous Sidebar Session",
        reach: SemanticReach::Client,
        description: "Move the Sidebar's selection to the row above, wrapping past the top",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarNext,
        title: "Next Sidebar Session",
        reach: SemanticReach::Client,
        description: "Move the Sidebar's selection to the row below, wrapping past the end",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarAttach,
        title: "Attach Selected Session",
        reach: SemanticReach::Client,
        // Every one of the Sidebar's own affordances is acted on from the row
        // the reader is on, so this is what Enter comes to there as well as on
        // a Session, and the account of the command names the rest of them.
        description: "Open the Session the Sidebar has selected, show more of the settled shelf, \
                      retry an unreachable Remote, or act on the Workspace selector",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::PointerClick,
        title: "Click",
        reach: SemanticReach::Client,
        description: "Act on a screen cell in the active surface",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::HyperlinkOpen,
        title: "Open Hyperlink",
        reach: SemanticReach::Client,
        description: "Open the hyperlink named by a rendered Transcript cell",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TextSelectionCopy,
        title: "Copy Text Selection",
        reach: SemanticReach::Client,
        description: "Copy the selected Transcript text",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TextSelectionClear,
        title: "Clear Text Selection",
        reach: SemanticReach::Client,
        description: "Clear the standing Text Selection",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TextSelectionWord,
        title: "Select Word",
        reach: SemanticReach::Client,
        description: "Select the word under a Transcript cell as a Text Selection",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::TextSelectionLine,
        title: "Select Line",
        reach: SemanticReach::Client,
        description: "Select the whole Line under a Transcript cell as a Text Selection",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::PointerDrag,
        title: "Drag",
        reach: SemanticReach::Client,
        description: "Move the held pointer without invoking a click",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ComposerPlaceCursor,
        title: "Place Composer Cursor",
        reach: SemanticReach::Client,
        description: "Focus the composer and place its insertion point at a text offset",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarLeave,
        title: "Leave Sidebar",
        reach: SemanticReach::Client,
        // Backing out of a path entry, a set of selector entries, or a search
        // is an inner step of backing out of the Sidebar, so the account of the
        // command says it takes one step rather than all of them.
        description: "Back out of the Sidebar a step: close what is open, clear its search, \
                      or hand the keys back to the composer",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuPrevious,
        title: "Previous Sidebar Menu Item",
        reach: SemanticReach::Client,
        description: "Move the Sidebar menu's selection to the item above, wrapping past the top",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuNext,
        title: "Next Sidebar Menu Item",
        reach: SemanticReach::Client,
        description: "Move the Sidebar menu's selection to the item below, wrapping past the end",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuSelect,
        title: "Invoke Sidebar Menu Item",
        reach: SemanticReach::Client,
        // Delete asks again rather than acting, so the account of the command
        // says that acting on an item is not always the end of it.
        description: "Act on the Sidebar menu's selected item, or ask it to confirm",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarMenuClose,
        title: "Close Sidebar Menu",
        reach: SemanticReach::Client,
        description: "Dismiss the Sidebar's context menu, leaving its row alone",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RemoteRetry,
        title: "Retry Remote",
        reach: SemanticReach::Client,
        description: "Restart an unreachable Remote's catalog stream and refresh its Sessions",
        slash: None,
        // Naming no Remote, the key means the one the Outlook is turned
        // toward — the very Remote the banner above the composer offers to
        // try again, through this same command.
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X R",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsOpen,
        title: "Settings",
        reach: SemanticReach::Client,
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
        reach: SemanticReach::Client,
        description: "Focus the previous Setting",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNext,
        title: "Next Setting",
        reach: SemanticReach::Client,
        description: "Focus the next Setting",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsTabPrevious,
        title: "Previous Settings Tab",
        reach: SemanticReach::Client,
        description: "Show the settings tab before this one, wrapping past the first",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsTabNext,
        title: "Next Settings Tab",
        reach: SemanticReach::Client,
        description: "Show the settings tab after this one, wrapping past the last",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsRowOpen,
        title: "Open Settings Row",
        reach: SemanticReach::Client,
        description: "Open what the focused row stands for: a Provider's further Settings, or the surface its value is chosen at",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsValueCycle,
        title: "Cycle Setting Value",
        reach: SemanticReach::Client,
        description: "Pin the focused Setting's next value, wrapping past the last",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsReset,
        title: "Reset Setting",
        reach: SemanticReach::Client,
        description: "Unpin the focused Setting so its built-in default resumes",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsClose,
        title: "Close Settings",
        reach: SemanticReach::Client,
        description: "Leave the settings panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SubagentBrowse,
        title: "Browse Subagents",
        reach: SemanticReach::Client,
        description: "Open the Subagent Picker over the open Session's working Subagents",
        // The picker opens from the down arrow's one free meaning — Down with
        // no caret movement or history walk left to make — which only the
        // surface holding the key can tell, so no direct binding or slash
        // stands here.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SubagentOpen,
        title: "Open Subagent",
        reach: SemanticReach::Client,
        description: "Open a Subagent's Session from the row that names it",
        // The command names the child Session it opens, so it is invoked from
        // the Subagent's row — its Transcript row today, a Picker entry later
        // — rather than from a key or a slash that would have no way to say
        // which Subagent it meant.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SubagentStop,
        title: "Stop Subagent",
        reach: SemanticReach::Origin,
        description: "Stop a working Subagent from the row that names it, where its Provider allows",
        // Like opening, the command names the child Session it stops, so it
        // is invoked from the Subagent's Picker row rather than from a key or
        // a slash that would have no way to say which Subagent it meant.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SubagentLeave,
        title: "Leave Subagent",
        reach: SemanticReach::Client,
        description: "Return from a Subagent's Session to its parent",
        // Bound to Escape only while a Subagent's Session is open, so the
        // binding lives in that view's own key table rather than here.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericDeleteBackward,
        title: "Delete Numeric Digit",
        reach: SemanticReach::Client,
        description: "Delete the last digit in the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericApply,
        title: "Apply Numeric Setting",
        reach: SemanticReach::Client,
        description: "Validate and apply the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsNumericCancel,
        title: "Cancel Numeric Setting",
        reach: SemanticReach::Client,
        description: "Discard the open numeric Setting editor",
        slash: None,
        keybinding: None,
    },
];

pub(super) fn descriptor(id: SemanticCommandId) -> &'static SemanticCommandDescriptor {
    SEMANTIC_COMMANDS
        .iter()
        .chain(NUMERIC_INSERT_COMMANDS.iter())
        .find(|command| {
            command.id == id
                || matches!(
                    (command.id, id),
                    (
                        SemanticCommandId::SidebarWidthSet { .. },
                        SemanticCommandId::SidebarWidthSet { .. }
                    )
                )
        })
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
    // Keep the established short-terminal browse window stable when an empty
    // slash gives every command the same score. Pairing follows the existing
    // Model Options action there, choosing a Remote before forming a Pairing
    // with one; any typed part of `connect` or `pair` ranks normally.
    if query.is_empty() {
        for (index, id) in [SemanticCommandId::ConnectOpen, SemanticCommandId::PairOpen]
            .into_iter()
            .enumerate()
        {
            let Some(position) = matches.iter().position(|(_, _, _, held)| *held == id) else {
                continue;
            };
            let command = matches.remove(position);
            let after_options = matches
                .iter()
                .position(|(_, _, _, held)| *held == SemanticCommandId::ModelOptions)
                .map_or(matches.len(), |options| options + 1);
            matches.insert((after_options + index).min(matches.len()), command);
        }
    }
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
