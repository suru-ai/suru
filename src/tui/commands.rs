//! Typed semantic commands and the Command completion mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;

use super::{
    metrics::{MetricsTab, calendar::MetricsRange, chart::MetricsMeasure, insights::InsightsPanel},
    text_layout::CursorTarget,
};

use crate::protocol::{
    ApprovalId, AttachmentId, Outlook, QuestionnaireId, SessionReference, TurnId,
    UsageMetricsBreakdown, WorkspaceId,
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
    /// Gives the composer back the keys a side column holds, which a press
    /// anywhere in the main view asks for.
    ComposerFocus,
    /// Pastes what the host clipboard holds into the composer: an image as an
    /// Attachment, text as a bracketed paste of it would, and nothing else.
    ComposerClipboardPaste,
    /// A click on the screen cell its subject names, with `modifiers` held,
    /// which the surface resolving it may read it differently by.
    PointerClick {
        modifiers: KeyModifiers,
    },
    PointerDrag,
    HyperlinkOpen,
    /// Opens an Attachment at full size from its thumbnail. Reserved, and
    /// bound to a thumbnail's click target, but it does nothing yet.
    AttachmentOpen,
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
    /// Opens the Landing in the Sidekick Workspace of the Server the Outlook
    /// is turned toward, asking that Server for it — and so having it made
    /// there — first.
    SessionSidekick,
    /// Opens the Session of the Sidekick that sent a Message, or gave a
    /// Questionnaire its Answer, on the user's behalf, from what names it.
    SidekickOpen,
    /// Opens a Subsession, a Session a Sidekick began, from the row in the
    /// Sidekick's Transcript that records beginning it.
    SubsessionOpen,
    /// Opens a top-level Session from an entry standing for it beneath a
    /// Sidekick's Session — in the Aside, or in the Subagent Picker — which
    /// says nothing of whether the Sidekick began it or only acted on it.
    SessionOpen,
    SessionSettle,
    SessionUnsettle,
    /// Asks the open Session's Provider to compact its context now, carrying
    /// what was typed after `/compact` as instructions for the summary.
    SessionCompact,
    /// Asks the open Session's Provider what occupies its context, and shows
    /// the answer over the main view.
    SessionContext,
    ContextScrollUp,
    ContextScrollDown,
    ContextPageUp,
    ContextPageDown,
    ContextClose,
    /// Opens the Usage Metrics view over the whole frame and reads the
    /// Usage Metrics of the Server the Outlook is turned toward.
    MetricsShow,
    /// Reads the Usage Metrics view's figures again.
    MetricsRefresh,
    /// Closes the Usage Metrics view, back to whatever it covered.
    MetricsClose,
    /// Shows one range in the Usage Metrics view, reading it anew.
    MetricsRangeSelect(MetricsRange),
    /// Shows the range after the Usage Metrics view's in its range bar.
    MetricsRangeNext,
    /// Shows the range before the Usage Metrics view's in its range bar.
    MetricsRangePrevious,
    /// Plots one measure in the Usage Metrics view, from the figures it
    /// already holds.
    MetricsMeasureSelect(MetricsMeasure),
    /// Plots the measure after the Usage Metrics view's, the last turning
    /// back to the first.
    MetricsMeasureNext,
    /// Splits the Usage Metrics view's bars by one breakdown, reading them
    /// anew.
    MetricsBreakdownSelect(UsageMetricsBreakdown),
    /// Splits the Usage Metrics view's bars by the breakdown after its own,
    /// the last turning back to the first.
    MetricsBreakdownNext,
    /// Selects the bucket before the Usage Metrics view's selected one, or
    /// the last where none is, stopping at the first.
    MetricsBucketPrevious,
    /// Selects the bucket after the Usage Metrics view's selected one, or
    /// the first where none is, stopping at the last.
    MetricsBucketNext,
    /// Selects the Usage Metrics view's first bucket.
    MetricsBucketFirst,
    /// Selects the Usage Metrics view's last bucket.
    MetricsBucketLast,
    /// Selects one of the Usage Metrics view's buckets by its place among
    /// them, counted from the first; a place past the last selects the last.
    MetricsBucketSelect {
        bucket: usize,
    },
    /// Clears the Usage Metrics view's bucket selection, so its legend reads
    /// the range again.
    MetricsBucketClear,
    /// Hides one of the Usage Metrics view's series from its chart, or shows
    /// it again, by its place among the legend's rows, counted from the
    /// first.
    MetricsSeriesToggle {
        series: usize,
    },
    /// Shows one of the Usage Metrics view's series alone, hiding every
    /// other, by its place among the legend's rows; where it is already shown
    /// alone, shows every series again.
    MetricsSeriesSolo {
        series: usize,
    },
    /// Focuses the row before the Usage Metrics view's focused one among its
    /// lists, or the last where none is focused.
    MetricsFocusPrevious,
    /// Focuses the row after the Usage Metrics view's focused one among its
    /// lists, or the first where none is focused.
    MetricsFocusNext,
    /// Lets go of the Usage Metrics view's list focus.
    MetricsFocusClear,
    /// Opens one of the Sessions the Usage Metrics view lists as costing the
    /// most, by its place among them, counted from the first, closing the
    /// view.
    MetricsSessionOpen {
        session: usize,
    },
    /// Shows one tab of the Usage Metrics view, reading it where it has no
    /// figures for the range.
    MetricsTabSelect(MetricsTab),
    /// Shows the Usage Metrics view's tab after the one showing.
    MetricsTabNext,
    /// Shows the Usage Metrics view's tab before the one showing.
    MetricsTabPrevious,
    /// Focuses one of the Insights tab's panels.
    MetricsPanelFocus(InsightsPanel),
    /// Focuses the Insights panel after the focused one, the last turning
    /// round to the first, or the first where none is focused.
    MetricsPanelFocusNext,
    /// Focuses the Insights panel before the focused one, the first turning
    /// round to the last, or the last where none is focused.
    MetricsPanelFocusPrevious,
    /// Leaves the focused Insights panel, and what it had selected.
    MetricsPanelFocusClear,
    /// Clears what the focused Insights panel has selected, keeping its
    /// focus.
    MetricsPanelSelectionClear,
    /// Selects the Turns panel's duration bin before its selected one, or
    /// the last where none is, focusing the panel.
    MetricsTurnsBinPrevious,
    /// Selects the Turns panel's duration bin after its selected one, or the
    /// first where none is, focusing the panel.
    MetricsTurnsBinNext,
    /// Selects one of the Turns panel's duration bins by its place among
    /// them, counted from the shortest, focusing the panel.
    MetricsTurnsBinSelect {
        bin: usize,
    },
    /// Opens the Icon Picker over a Session, which the picker only does while
    /// `appearance.showIcons` is on: inert with it off, so a header press or a
    /// menu item stays quiet rather than opening a picker no glyph could draw.
    SessionIconChoose,
    /// Opens the Icon Picker over a Workspace, on the same terms
    /// [`Self::SessionIconChoose`] opens it over a Session: inert while
    /// `appearance.showIcons` is off.
    WorkspaceIconChoose,
    /// Opens the rename dialog over a Session — the one its subject names, or
    /// the open one where it names none. `/rename <title>` names the Title
    /// itself, which is set at once where it can stand as written.
    SessionRename,
    RenameInsert,
    RenameDeleteBackward,
    /// Empties the rename dialog's field.
    RenameClear,
    /// Asks the Session's Origin for a Title proposal derived from its
    /// Prompts so far, which replaces the field's text when it arrives.
    RenameDerive,
    /// Renames the Session to what the field holds.
    RenameSave,
    /// Stops a proposal still being derived, giving the field its text back,
    /// or closes the dialog where none is.
    RenameCancel,
    IconPickerLeft,
    IconPickerRight,
    IconPickerUp,
    IconPickerDown,
    IconPickerChoose,
    IconPickerClose,
    IconPickerSearchInsert,
    IconPickerSearchDelete,
    /// Opens the Description editor over a Workspace the Workspace Picker
    /// offers — the one its subject names, or the row the reader is on —
    /// seeded with the Description it carries.
    WorkspaceDescriptionEdit,
    WorkspaceDescriptionInsert,
    WorkspaceDescriptionDeleteBackward,
    /// Empties the Description editor, which saved as it stands clears the
    /// Workspace's Description so Suru may derive one again.
    WorkspaceDescriptionClear,
    /// Sends the Description editor's text to its Workspace's own Origin.
    WorkspaceDescriptionSave,
    WorkspaceDescriptionCancel,
    WorkspacePickerMenuPrevious,
    WorkspacePickerMenuNext,
    /// Invokes the Workspace Picker row menu's selected item, the way Enter
    /// acts on the Sidebar's own row menu.
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
    /// Scrolls what overflows the Connect step the reader is on — an
    /// Invite's preview, or why a redemption was refused — a Row back, or a
    /// Row on, so every way an Invite offers is read before it is trusted.
    ConnectScrollUp,
    ConnectScrollDown,
    /// Scrolls it a page back, or a page on.
    ConnectPageUp,
    ConnectPageDown,
    ConnectClose,
    ServeOpen,
    ServePrevious,
    ServeNext,
    ServeToggleAddress,
    ServeConfirm,
    ServeCopyInvite,
    ServeRemovePeer,
    ServeClose,
    RelayOpen,
    RelayPrevious,
    RelayNext,
    /// Opens the address entry from the Relay list, and adds the Relay at
    /// the address typed there from the entry: adding is one act begun in one
    /// place and finished in the other.
    RelayAdd,
    RelayAddressInsert,
    RelayAddressDeleteBackward,
    /// Logs in at the selected Relay, or shows again the login already under
    /// way there. Naming a Relay, it opens the list on that one and logs in
    /// there — or, where the Server holds no entry for it, offers to add it —
    /// which is where a Notice of a Relay needing a login, and a Remote out
    /// of reach for want of one, lead their reader.
    RelayLogin,
    RelayCopyAddress,
    RelayCopyCode,
    /// Arms the selected Relay's removal, and removes it when it is already
    /// armed, as ending a Pairing is asked for twice.
    RelayRemove,
    /// Chooses that the Server Serves through the selected Relay where it
    /// does not, and that it does not where it does. The choice is stored
    /// with the Relay's entry whether or not the Server is logged in there,
    /// and opens nothing until it is, and Serving is on.
    RelayServeThroughToggle,
    /// Steps back to the Relay list from the address entry or the login
    /// display, and closes the list itself.
    RelayClose,
    /// Opens Onboarding at its first step over whatever is on screen, the
    /// Landing or a Session, every step showing the current values.
    OnboardingOpen,
    /// Moves the cursor of Onboarding's Providers step to the Provider above,
    /// or below, wrapping past either end.
    OnboardingProvidersPrevious,
    OnboardingProvidersNext,
    /// Turns the Provider under the cursor of Onboarding's Providers step on
    /// or off, writing its Enablement at once.
    OnboardingProvidersToggle,
    /// Re-reads every Provider's Availability from Onboarding's Providers
    /// step, for a reader who has just fixed one outside Suru.
    OnboardingProvidersRecheck,
    /// Moves Row Focus on Onboarding's Icons step to the answer above, or
    /// below, wrapping past either end; from neither answer, to the last, or
    /// the first.
    OnboardingIconsPrevious,
    OnboardingIconsNext,
    /// Confirms the Onboarding step on show and moves to the next, or
    /// finishes the flow on its last.
    OnboardingNext,
    OnboardingBack,
    /// Leaves Onboarding from any step, which counts as taking it.
    OnboardingLeave,
    /// Moves Row Focus on Onboarding's Derivation step to the row above.
    OnboardingDerivationPrevious,
    /// Moves Row Focus on Onboarding's Derivation step to the row below.
    OnboardingDerivationNext,
    OnboardingDerivationPageUp,
    OnboardingDerivationPageDown,
    /// Moves focus on Onboarding's Workspace Roots step to the row above, or
    /// below, wrapping past either end.
    OnboardingRootsPrevious,
    OnboardingRootsNext,
    /// Adds the typed or pasted text its invocation carries to the end of
    /// the focused row of Onboarding's Workspace Roots step.
    OnboardingRootsInsert,
    OnboardingRootsDeleteBackward,
    /// Removes the focused entry from Onboarding's Workspace Roots step.
    OnboardingRootsRemove,
    /// Opens the Directory Browser over Onboarding's Workspace Roots step,
    /// whose choice stands in the focused entry's place or is added at the
    /// row past the last entry.
    OnboardingRootsBrowse,
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
    /// Gives a shown Sidebar the keys without ever hiding it, which a press on
    /// its search box asks for.
    SidebarFocus,
    SidebarMenuPrevious,
    SidebarMenuNext,
    SidebarMenuSelect,
    SidebarMenuClose,
    AsideToggle,
    AsideWiden,
    AsideNarrow,
    AsideWidthSet {
        columns: u64,
    },
    AsideWidthReset,
    AsideLeave,
    AsidePrevious,
    AsideNext,
    AsideOpen,
    RemoteRetry,
    SettingsOpen,
    SettingsPrevious,
    SettingsNext,
    SettingsTabPrevious,
    SettingsTabNext,
    SettingsRowOpen,
    /// Steps the keys back out of a Provider's own Settings onto the list of
    /// Providers they were stepped into from.
    SettingsProviderLeave,
    SettingsValueCycle,
    SettingsReset,
    SettingsClose,
    SettingsNumericInsert(NumericDigit),
    SettingsNumericDeleteBackward,
    SettingsNumericApply,
    SettingsNumericCancel,
    SettingsListPrevious,
    SettingsListNext,
    /// Adds the typed or pasted text its invocation carries to the end of the
    /// list editor's focused row, which makes the row past the last entry an
    /// entry of its own.
    SettingsListInsert,
    SettingsListDeleteBackward,
    /// Removes the list editor's focused entry from the draft.
    SettingsListRemove,
    /// Opens the Directory Browser over the list editor of a list of
    /// directories, whose choice stands in the focused entry's place or is
    /// added at the row past the last entry.
    SettingsListBrowse,
    /// Pins the list editor's whole draft as one mutation, or names the entry
    /// its Setting refuses.
    SettingsListSave,
    SettingsListCancel,
    SubagentBrowse,
    SubagentOpen,
    SubagentStop,
    SubagentLeave,
    TranscriptFoldsToggle,
    TranscriptGroupsToggle,
    TranscriptTurnToggle,
    TranscriptTurnsToggle,
    WorkspaceList,
    /// Opens the Directory Browser over the Outlook's Server's directories,
    /// rooted at the Landing's Execution Directory.
    WorkspaceBrowse,
    DirectoryBrowserPrevious,
    DirectoryBrowserNext,
    /// Opens the Directory Browser's focused row where it is closed, and
    /// closes it where it is open.
    DirectoryBrowserRowToggle,
    /// Opens the Directory Browser's focused row and steps focus onto its
    /// first child.
    DirectoryBrowserRowEnter,
    /// Steps the Directory Browser's focus onto the focused row's parent, or
    /// on the root stands the tree on the root's parent.
    DirectoryBrowserRowParent,
    /// Adds the typed or pasted text its invocation carries to the end of the
    /// Directory Browser's path field.
    DirectoryBrowserPathInsert,
    DirectoryBrowserPathDeleteBackward,
    /// Completes the Directory Browser's path field to the focused row.
    DirectoryBrowserPathComplete,
    /// Chooses the Directory Browser's focused row as where the next Session
    /// works, as choosing a Workspace in the Workspace Picker does — or, for
    /// a browser the list editor opened, as an entry of its draft.
    DirectoryBrowserChoose,
    DirectoryBrowserClose,
    /// Shows the Directory Browser's hidden directories among the others, or
    /// leaves them out again, for the rest of the client run.
    DirectoryBrowserHiddenToggle,
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
    /// One Attachment, named by the thumbnail a reader pressed.
    Attachment(AttachmentId),
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
    /// One of the Client's own Server's Relays, by its address — what a
    /// Notice of it needing a login, or a Remote out of reach for want of
    /// one, names it by.
    Relay(String),
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

    /// This command invoked against one of the Client's own Server's Relays,
    /// which is what a Notice of it, or a Remote out of reach for want of a
    /// login there, names.
    pub(super) fn on_relay(self, address: String) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Relay(address),
        }
    }

    /// This command invoked against one Attachment, which is what a reader
    /// names by pressing its thumbnail.
    pub(super) fn on_attachment(self, attachment_id: AttachmentId) -> SemanticInvocation {
        SemanticInvocation {
            id: self,
            subject: SemanticSubject::Attachment(attachment_id),
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
            Self::PointerClick { .. } => "pointer.click",
            Self::PointerDrag => "pointer.drag",
            Self::HyperlinkOpen => "hyperlink.open",
            Self::AttachmentOpen => "attachment.open",
            Self::TextSelectionCopy => "text_selection.copy",
            Self::TextSelectionClear => "text_selection.clear",
            Self::TextSelectionWord => "text_selection.word",
            Self::TextSelectionLine => "text_selection.line",
            Self::ComposerPlaceCursor => "composer.cursor.place",
            Self::ComposerFocus => "composer.focus",
            Self::ComposerClipboardPaste => "composer.clipboard.paste",
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
            Self::SessionSidekick => "session.sidekick",
            Self::SidekickOpen => "sidekick.open",
            Self::SubsessionOpen => "subsession.open",
            Self::SessionOpen => "session.open",
            Self::SessionSettle => "session.settle",
            Self::SessionUnsettle => "session.unsettle",
            Self::SessionCompact => "session.compact",
            Self::SessionContext => "session.context",
            Self::ContextScrollUp => "context.scroll-up",
            Self::ContextScrollDown => "context.scroll-down",
            Self::ContextPageUp => "context.page-up",
            Self::ContextPageDown => "context.page-down",
            Self::ContextClose => "context.close",
            Self::MetricsShow => "metrics.show",
            Self::MetricsRefresh => "metrics.refresh",
            Self::MetricsClose => "metrics.close",
            Self::MetricsRangeSelect(range) => match range {
                MetricsRange::Today => "metrics.range.today",
                MetricsRange::SevenDays => "metrics.range.seven-days",
                MetricsRange::ThirtyDays => "metrics.range.thirty-days",
                MetricsRange::Year => "metrics.range.year",
                MetricsRange::AllTime => "metrics.range.all-time",
            },
            Self::MetricsRangeNext => "metrics.range.next",
            Self::MetricsRangePrevious => "metrics.range.previous",
            Self::MetricsMeasureSelect(measure) => match measure {
                MetricsMeasure::Cost => "metrics.measure.cost",
                MetricsMeasure::Tokens => "metrics.measure.tokens",
                MetricsMeasure::Sessions => "metrics.measure.sessions",
            },
            Self::MetricsMeasureNext => "metrics.measure.next",
            Self::MetricsBreakdownSelect(breakdown) => match breakdown {
                UsageMetricsBreakdown::Model => "metrics.breakdown.model",
                UsageMetricsBreakdown::Provider => "metrics.breakdown.provider",
                UsageMetricsBreakdown::Workspace => "metrics.breakdown.workspace",
            },
            Self::MetricsBreakdownNext => "metrics.breakdown.next",
            Self::MetricsBucketPrevious => "metrics.bucket.previous",
            Self::MetricsBucketNext => "metrics.bucket.next",
            Self::MetricsBucketFirst => "metrics.bucket.first",
            Self::MetricsBucketLast => "metrics.bucket.last",
            Self::MetricsBucketSelect { .. } => "metrics.bucket.select",
            Self::MetricsBucketClear => "metrics.bucket.clear",
            Self::MetricsSeriesToggle { .. } => "metrics.series.toggle",
            Self::MetricsSeriesSolo { .. } => "metrics.series.solo",
            Self::MetricsFocusPrevious => "metrics.focus.previous",
            Self::MetricsFocusNext => "metrics.focus.next",
            Self::MetricsFocusClear => "metrics.focus.clear",
            Self::MetricsSessionOpen { .. } => "metrics.session.open",
            Self::MetricsTabSelect(tab) => match tab {
                MetricsTab::Overview => "metrics.tab.overview",
                MetricsTab::Insights => "metrics.tab.insights",
            },
            Self::MetricsTabNext => "metrics.tab.next",
            Self::MetricsTabPrevious => "metrics.tab.previous",
            Self::MetricsPanelFocus(panel) => match panel {
                InsightsPanel::Turns => "metrics.panel.turns",
            },
            Self::MetricsPanelFocusNext => "metrics.panel.next",
            Self::MetricsPanelFocusPrevious => "metrics.panel.previous",
            Self::MetricsPanelFocusClear => "metrics.panel.clear",
            Self::MetricsPanelSelectionClear => "metrics.panel.selection.clear",
            Self::MetricsTurnsBinPrevious => "metrics.turns.bin.previous",
            Self::MetricsTurnsBinNext => "metrics.turns.bin.next",
            Self::MetricsTurnsBinSelect { .. } => "metrics.turns.bin.select",
            Self::SessionIconChoose => "session.icon.choose",
            Self::WorkspaceIconChoose => "workspace.icon.choose",
            Self::SessionRename => "session.rename",
            Self::RenameInsert => "rename.insert",
            Self::RenameDeleteBackward => "rename.delete-backward",
            Self::RenameClear => "rename.clear",
            Self::RenameDerive => "rename.derive",
            Self::RenameSave => "rename.save",
            Self::RenameCancel => "rename.cancel",
            Self::IconPickerLeft => "icon-picker.left",
            Self::IconPickerRight => "icon-picker.right",
            Self::IconPickerUp => "icon-picker.up",
            Self::IconPickerDown => "icon-picker.down",
            Self::IconPickerChoose => "icon-picker.choose",
            Self::IconPickerClose => "icon-picker.close",
            Self::IconPickerSearchInsert => "icon-picker.search.insert",
            Self::IconPickerSearchDelete => "icon-picker.search.delete-backward",
            Self::WorkspaceDescriptionEdit => "workspace.description.edit",
            Self::WorkspaceDescriptionInsert => "workspace-description.insert",
            Self::WorkspaceDescriptionDeleteBackward => "workspace-description.delete-backward",
            Self::WorkspaceDescriptionClear => "workspace-description.clear",
            Self::WorkspaceDescriptionSave => "workspace-description.save",
            Self::WorkspaceDescriptionCancel => "workspace-description.cancel",
            Self::WorkspacePickerMenuPrevious => "workspace-picker.menu.previous",
            Self::WorkspacePickerMenuNext => "workspace-picker.menu.next",
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
            Self::ConnectScrollUp => "connect.scroll.up",
            Self::ConnectScrollDown => "connect.scroll.down",
            Self::ConnectPageUp => "connect.scroll.page-up",
            Self::ConnectPageDown => "connect.scroll.page-down",
            Self::ConnectClose => "connect.close",
            Self::ServeOpen => "serve.open",
            Self::ServePrevious => "serve.previous",
            Self::ServeNext => "serve.next",
            Self::ServeToggleAddress => "serve.address.toggle",
            Self::ServeConfirm => "serve.confirm",
            Self::ServeCopyInvite => "serve.invite.copy",
            Self::ServeRemovePeer => "serve.peer.remove",
            Self::ServeClose => "serve.close",
            Self::RelayOpen => "relay.open",
            Self::RelayPrevious => "relay.previous",
            Self::RelayNext => "relay.next",
            Self::RelayAdd => "relay.add",
            Self::RelayAddressInsert => "relay.address.insert",
            Self::RelayAddressDeleteBackward => "relay.address.delete-backward",
            Self::RelayLogin => "relay.login",
            Self::RelayCopyAddress => "relay.login.copy-address",
            Self::RelayCopyCode => "relay.login.copy-code",
            Self::RelayRemove => "relay.remove",
            Self::RelayServeThroughToggle => "relay.serve-through.toggle",
            Self::RelayClose => "relay.close",
            Self::OnboardingOpen => "onboarding.open",
            Self::OnboardingProvidersPrevious => "onboarding.providers.previous",
            Self::OnboardingProvidersNext => "onboarding.providers.next",
            Self::OnboardingProvidersToggle => "onboarding.providers.toggle",
            Self::OnboardingProvidersRecheck => "onboarding.providers.recheck",
            Self::OnboardingIconsPrevious => "onboarding.icons.previous",
            Self::OnboardingIconsNext => "onboarding.icons.next",
            Self::OnboardingNext => "onboarding.next",
            Self::OnboardingBack => "onboarding.back",
            Self::OnboardingLeave => "onboarding.leave",
            Self::OnboardingDerivationPrevious => "onboarding.derivation.previous",
            Self::OnboardingDerivationNext => "onboarding.derivation.next",
            Self::OnboardingDerivationPageUp => "onboarding.derivation.page-up",
            Self::OnboardingDerivationPageDown => "onboarding.derivation.page-down",
            Self::OnboardingRootsPrevious => "onboarding.roots.previous",
            Self::OnboardingRootsNext => "onboarding.roots.next",
            Self::OnboardingRootsInsert => "onboarding.roots.insert",
            Self::OnboardingRootsDeleteBackward => "onboarding.roots.delete-backward",
            Self::OnboardingRootsRemove => "onboarding.roots.remove",
            Self::OnboardingRootsBrowse => "onboarding.roots.browse",
            Self::SidebarToggle => "sidebar.toggle",
            Self::SidebarWiden => "sidebar.width.widen",
            Self::SidebarNarrow => "sidebar.width.narrow",
            Self::SidebarWidthSet { .. } => "sidebar.width.set",
            Self::SidebarWidthReset => "sidebar.width.reset",
            Self::SidebarPrevious => "sidebar.previous",
            Self::SidebarNext => "sidebar.next",
            Self::SidebarAttach => "sidebar.attach",
            Self::SidebarLeave => "sidebar.leave",
            Self::SidebarFocus => "sidebar.focus",
            Self::SidebarMenuPrevious => "sidebar.menu.previous",
            Self::SidebarMenuNext => "sidebar.menu.next",
            Self::SidebarMenuSelect => "sidebar.menu.select",
            Self::SidebarMenuClose => "sidebar.menu.close",
            Self::AsideToggle => "aside.toggle",
            Self::AsideWiden => "aside.width.widen",
            Self::AsideNarrow => "aside.width.narrow",
            Self::AsideWidthSet { .. } => "aside.width.set",
            Self::AsideWidthReset => "aside.width.reset",
            Self::AsideLeave => "aside.leave",
            Self::AsidePrevious => "aside.previous",
            Self::AsideNext => "aside.next",
            Self::AsideOpen => "aside.open",
            Self::RemoteRetry => "remote.retry",
            Self::SettingsOpen => "settings.open",
            Self::SettingsPrevious => "settings.previous",
            Self::SettingsNext => "settings.next",
            Self::SettingsTabPrevious => "settings.tab.previous",
            Self::SettingsTabNext => "settings.tab.next",
            Self::SettingsRowOpen => "settings.row.open",
            Self::SettingsProviderLeave => "settings.provider.leave",
            Self::SettingsValueCycle => "settings.value.cycle",
            Self::SettingsReset => "settings.reset",
            Self::SettingsClose => "settings.close",
            Self::SettingsNumericInsert(digit) => digit.command_id(),
            Self::SettingsNumericDeleteBackward => "settings.numeric.delete-backward",
            Self::SettingsNumericApply => "settings.numeric.apply",
            Self::SettingsNumericCancel => "settings.numeric.cancel",
            Self::SettingsListPrevious => "settings.list.previous",
            Self::SettingsListNext => "settings.list.next",
            Self::SettingsListInsert => "settings.list.insert",
            Self::SettingsListDeleteBackward => "settings.list.delete-backward",
            Self::SettingsListRemove => "settings.list.remove",
            Self::SettingsListBrowse => "settings.list.browse",
            Self::SettingsListSave => "settings.list.save",
            Self::SettingsListCancel => "settings.list.cancel",
            Self::SubagentBrowse => "subagent.browse",
            Self::SubagentOpen => "subagent.open",
            Self::SubagentStop => "subagent.stop",
            Self::SubagentLeave => "subagent.leave",
            Self::TranscriptFoldsToggle => "transcript.folds.toggle",
            Self::TranscriptGroupsToggle => "transcript.groups.toggle",
            Self::TranscriptTurnToggle => "transcript.turn.fold.toggle",
            Self::TranscriptTurnsToggle => "transcript.turns.toggle",
            Self::WorkspaceList => "workspace.list",
            Self::WorkspaceBrowse => "workspace.browse",
            Self::DirectoryBrowserPrevious => "directory-browser.previous",
            Self::DirectoryBrowserNext => "directory-browser.next",
            Self::DirectoryBrowserRowToggle => "directory-browser.row.toggle",
            Self::DirectoryBrowserRowEnter => "directory-browser.row.enter",
            Self::DirectoryBrowserRowParent => "directory-browser.row.parent",
            Self::DirectoryBrowserPathInsert => "directory-browser.path.insert",
            Self::DirectoryBrowserPathDeleteBackward => "directory-browser.path.delete-backward",
            Self::DirectoryBrowserPathComplete => "directory-browser.path.complete",
            Self::DirectoryBrowserChoose => "directory-browser.choose",
            Self::DirectoryBrowserClose => "directory-browser.close",
            Self::DirectoryBrowserHiddenToggle => "directory-browser.hidden.toggle",
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
    /// Whether the command reads what is typed after its name as its own
    /// payload — `/compact <instructions>` — rather than the composer
    /// sending that line on as a literal Prompt.
    takes_text: bool,
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

/// Choosing a range only changes what the view shows, so it reaches the
/// Client; the read it brings is refused in the view where the Origin has
/// stopped answering.
const fn metrics_range_descriptor(
    range: MetricsRange,
    title: &'static str,
) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsRangeSelect(range),
        title,
        reach: SemanticReach::Client,
        description: "Show one range in the Usage Metrics view",
        slash: None,
        keybinding: None,
    }
}

/// A report carries every measure, so choosing one reads nothing and
/// reaches only the Client.
const fn metrics_measure_descriptor(
    measure: MetricsMeasure,
    title: &'static str,
) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsMeasureSelect(measure),
        title,
        reach: SemanticReach::Client,
        description: "Plot one measure in the Usage Metrics view",
        slash: None,
        keybinding: None,
    }
}

/// Choosing a breakdown changes only what the view shows, so it reaches the
/// Client, as choosing a range does; the read it brings is refused in the
/// view where the Origin has stopped answering.
const fn metrics_breakdown_descriptor(
    breakdown: UsageMetricsBreakdown,
    title: &'static str,
) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBreakdownSelect(breakdown),
        title,
        reach: SemanticReach::Client,
        description: "Split the Usage Metrics view's bars by one breakdown",
        slash: None,
        keybinding: None,
    }
}

/// Switching tab changes only what the view shows, so it reaches the
/// Client, as choosing a range does; the read it brings is refused in the
/// view where the Origin has stopped answering.
const fn metrics_tab_descriptor(tab: MetricsTab, title: &'static str) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsTabSelect(tab),
        title,
        reach: SemanticReach::Client,
        description: "Show one tab of the Usage Metrics view",
        slash: None,
        keybinding: None,
    }
}

/// Focusing a panel reads nothing: the report shown holds every panel's
/// figures.
const fn metrics_panel_descriptor(
    panel: InsightsPanel,
    title: &'static str,
) -> SemanticCommandDescriptor {
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsPanelFocus(panel),
        title,
        reach: SemanticReach::Client,
        description: "Focus one of the Usage Metrics view's Insights panels",
        slash: None,
        keybinding: None,
    }
}

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
            takes_text: false,
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
            takes_text: false,
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
        description: "Submit the reviewed Answer, or advance toward review",
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
            takes_text: false,
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
            takes_text: false,
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
            takes_text: false,
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
            takes_text: false,
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
            takes_text: false,
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
            takes_text: false,
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
            takes_text: false,
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X W",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceBrowse,
        title: "Browse for a Workspace",
        // Everything the browser lists is read from the Outlook's Server, so
        // an Origin that has stopped answering refuses it like any other
        // start.
        reach: SemanticReach::Origin,
        description: "Choose a directory of the Outlook's Server to work in from a tree of its directories",
        slash: Some(SlashCommand {
            name: "browse",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X B",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserPrevious,
        title: "Directory Browser Previous",
        reach: SemanticReach::Client,
        description: "Move the Directory Browser's focus to the directory drawn above",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserNext,
        title: "Directory Browser Next",
        reach: SemanticReach::Client,
        description: "Move the Directory Browser's focus to the directory drawn below",
        slash: None,
        keybinding: None,
    },
    // Opening a row asks the Server for its children, but these stay with the
    // Client: closing one asks nothing, and a listing the Server cannot give
    // is said beneath the row it was asked for.
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserRowToggle,
        title: "Open or Close Directory",
        reach: SemanticReach::Client,
        description: "Open the Directory Browser's focused directory, or close it where it is open",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserRowEnter,
        title: "Enter Directory",
        reach: SemanticReach::Client,
        description: "Open the Directory Browser's focused directory and step onto its first \
                      child",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserRowParent,
        title: "Go to Parent Directory",
        reach: SemanticReach::Client,
        description: "Step the Directory Browser's focus onto the focused directory's parent, or \
                      on the root stand the tree on its parent",
        slash: None,
        keybinding: None,
    },
    // Editing the path field may ask the Server for the root it now names,
    // as opening a row asks for its children, and stays with the Client on
    // the same terms.
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserPathInsert,
        title: "Insert Path Text",
        reach: SemanticReach::Client,
        description: "Add typed or pasted text to the end of the Directory Browser's path field",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserPathDeleteBackward,
        title: "Delete Path Text",
        reach: SemanticReach::Client,
        description: "Take the last character back from the Directory Browser's path field",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserPathComplete,
        title: "Complete Path",
        reach: SemanticReach::Client,
        description: "Complete the Directory Browser's path field to the focused directory",
        slash: None,
        keybinding: None,
    },
    // Choosing asks the Outlook's Server to resolve the directory, as the
    // Workspace Picker's choice does, so an Origin that has stopped answering
    // refuses it as it refuses that choice.
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserChoose,
        title: "Choose Directory",
        reach: SemanticReach::Origin,
        description: "Open the Landing in the Directory Browser's focused directory",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserClose,
        title: "Close Directory Browser",
        reach: SemanticReach::Client,
        description: "Dismiss the Directory Browser without choosing",
        slash: None,
        keybinding: None,
    },
    // Every listing carries its hidden directories already, so showing or
    // hiding them asks the Server for nothing.
    SemanticCommandDescriptor {
        id: SemanticCommandId::DirectoryBrowserHiddenToggle,
        title: "Show or Hide Hidden Directories",
        reach: SemanticReach::Client,
        description: "Show the Directory Browser's hidden directories among the others, or leave them out again",
        slash: None,
        keybinding: None,
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
            takes_text: false,
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X N",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionSidekick,
        title: "Sidekick",
        // It asks the Outlook's Server for its Sidekick Workspace, so an
        // Origin that has stopped answering refuses it like any other start.
        reach: SemanticReach::Origin,
        description: "Begin a Session with a Sidekick, an Agent that works across Suru itself",
        slash: Some(SlashCommand {
            name: "sidekick",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidekickOpen,
        title: "Open Sidekick",
        reach: SemanticReach::Client,
        description: "Open the Session of the Sidekick that sent a Message or gave an Answer",
        // The command names the Sidekick's Session it opens, so it is invoked
        // from the Message that Sidekick sent, or the Answer it gave, rather
        // than from a key or a slash that would have no way to say which
        // Sidekick it meant.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SubsessionOpen,
        title: "Open Subsession",
        reach: SemanticReach::Client,
        description: "Open a Session a Sidekick began, from the row that records beginning it",
        // The command names the Subsession it opens, so it is invoked from
        // the Sidekick's row for it rather than from a key or a slash that
        // would have no way to say which Subsession it meant.
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionOpen,
        title: "Open Session",
        reach: SemanticReach::Client,
        description: "Open a Session a Sidekick has a hand in, from its entry beneath the \
                      Sidekick's Session",
        // The command names the Session it opens, so it is invoked from the
        // entry standing for it rather than from a key or a slash that would
        // have no way to say which Session it meant.
        slash: None,
        keybinding: None,
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
            takes_text: false,
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
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionCompact,
        title: "Compact Context",
        reach: SemanticReach::Origin,
        // Listed whatever the open Session's Provider is: one that compacts
        // only when it chooses has the request explained rather than hidden.
        description: "Ask the open Session's Provider to compact its context now",
        slash: Some(SlashCommand {
            name: "compact",
            aliases: &[],
            takes_text: true,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SessionContext,
        title: "Show Context",
        reach: SemanticReach::Origin,
        // Listed whatever the open Session's Provider is: one that attributes
        // nothing has that explained beside the Context Fill it does report.
        description: "Show what fills the open Session's context",
        slash: Some(SlashCommand {
            name: "context",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ContextScrollUp,
        title: "Scroll Context Up",
        reach: SemanticReach::Client,
        description: "Scroll the Context Breakdown up a row",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ContextScrollDown,
        title: "Scroll Context Down",
        reach: SemanticReach::Client,
        description: "Scroll the Context Breakdown down a row",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ContextPageUp,
        title: "Page Context Up",
        reach: SemanticReach::Client,
        description: "Scroll the Context Breakdown up a page",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ContextPageDown,
        title: "Page Context Down",
        reach: SemanticReach::Client,
        description: "Scroll the Context Breakdown down a page",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ContextClose,
        title: "Close Context",
        reach: SemanticReach::Client,
        description: "Dismiss the Context Breakdown",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsShow,
        title: "Show Metrics",
        // It reads the Server the Outlook is turned toward, so an Origin
        // that has stopped answering refuses it.
        reach: SemanticReach::Origin,
        description: "Show the Usage Metrics of every Session on the Outlook's Server",
        slash: Some(SlashCommand {
            name: "metrics",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsRefresh,
        title: "Refresh Metrics",
        reach: SemanticReach::Origin,
        description: "Read the Usage Metrics view's figures again",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsClose,
        title: "Close Metrics",
        reach: SemanticReach::Client,
        description: "Close the Usage Metrics view",
        slash: None,
        keybinding: None,
    },
    metrics_range_descriptor(MetricsRange::Today, "Show Today's Metrics"),
    metrics_range_descriptor(MetricsRange::SevenDays, "Show 7 Days of Metrics"),
    metrics_range_descriptor(MetricsRange::ThirtyDays, "Show 30 Days of Metrics"),
    metrics_range_descriptor(MetricsRange::Year, "Show a Year of Metrics"),
    metrics_range_descriptor(MetricsRange::AllTime, "Show All Metrics"),
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsRangeNext,
        title: "Next Metrics Range",
        reach: SemanticReach::Client,
        description: "Show the next, longer range in the Usage Metrics view",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsRangePrevious,
        title: "Previous Metrics Range",
        reach: SemanticReach::Client,
        description: "Show the previous, shorter range in the Usage Metrics view",
        slash: None,
        keybinding: None,
    },
    metrics_measure_descriptor(MetricsMeasure::Cost, "Show Cost Metrics"),
    metrics_measure_descriptor(MetricsMeasure::Tokens, "Show Token Metrics"),
    metrics_measure_descriptor(MetricsMeasure::Sessions, "Show Session Metrics"),
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsMeasureNext,
        title: "Next Metrics Measure",
        reach: SemanticReach::Client,
        description: "Plot the next measure in the Usage Metrics view",
        slash: None,
        keybinding: None,
    },
    metrics_breakdown_descriptor(UsageMetricsBreakdown::Model, "Show Metrics by Model"),
    metrics_breakdown_descriptor(UsageMetricsBreakdown::Provider, "Show Metrics by Provider"),
    metrics_breakdown_descriptor(
        UsageMetricsBreakdown::Workspace,
        "Show Metrics by Workspace",
    ),
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBreakdownNext,
        title: "Next Metrics Breakdown",
        reach: SemanticReach::Client,
        description: "Split the Usage Metrics view's bars by the next breakdown",
        slash: None,
        keybinding: None,
    },
    // Selecting a bucket reads nothing: the report holds every bucket's
    // figures, which the legend shows in place of the range's.
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBucketPrevious,
        title: "Previous Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Select the bucket before the Usage Metrics view's selected one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBucketNext,
        title: "Next Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Select the bucket after the Usage Metrics view's selected one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBucketFirst,
        title: "First Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Select the Usage Metrics view's first bucket",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBucketLast,
        title: "Last Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Select the Usage Metrics view's last bucket",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The bucket's place is invocation data, as a Sidebar width is.
        id: SemanticCommandId::MetricsBucketSelect { bucket: 0 },
        title: "Select Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Select one of the Usage Metrics view's buckets by its place",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsBucketClear,
        title: "Clear Metrics Bucket",
        reach: SemanticReach::Client,
        description: "Clear the Usage Metrics view's bucket selection",
        slash: None,
        keybinding: None,
    },
    // Hiding a series changes only the chart, never the figures read, so it
    // reads nothing either.
    SemanticCommandDescriptor {
        // The series' place is invocation data, as a bucket's is.
        id: SemanticCommandId::MetricsSeriesToggle { series: 0 },
        title: "Toggle Metrics Series",
        reach: SemanticReach::Client,
        description: "Hide or show one of the Usage Metrics view's series",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsSeriesSolo { series: 0 },
        title: "Show Metrics Series Alone",
        reach: SemanticReach::Client,
        description: "Show one of the Usage Metrics view's series alone, or every series again",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsFocusPrevious,
        title: "Previous Metrics Row",
        reach: SemanticReach::Client,
        description: "Focus the row before the Usage Metrics view's focused one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsFocusNext,
        title: "Next Metrics Row",
        reach: SemanticReach::Client,
        description: "Focus the row after the Usage Metrics view's focused one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsFocusClear,
        title: "Clear Metrics Focus",
        reach: SemanticReach::Client,
        description: "Let go of the Usage Metrics view's focused row",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The Session's place among those listed is invocation data, as a
        // series' is; opening it is the open-Session path every list takes.
        id: SemanticCommandId::MetricsSessionOpen { session: 0 },
        title: "Open Metrics Session",
        reach: SemanticReach::Client,
        description: "Open one of the Sessions the Usage Metrics view lists as costing the most",
        slash: None,
        keybinding: None,
    },
    metrics_tab_descriptor(MetricsTab::Overview, "Show Metrics Overview"),
    metrics_tab_descriptor(MetricsTab::Insights, "Show Metrics Insights"),
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsTabNext,
        title: "Next Metrics Tab",
        reach: SemanticReach::Client,
        description: "Show the Usage Metrics view's next tab",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsTabPrevious,
        title: "Previous Metrics Tab",
        reach: SemanticReach::Client,
        description: "Show the Usage Metrics view's previous tab",
        slash: None,
        keybinding: None,
    },
    metrics_panel_descriptor(InsightsPanel::Turns, "Focus Turns Insights"),
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsPanelFocusNext,
        title: "Next Insights Panel",
        reach: SemanticReach::Client,
        description: "Focus the Insights panel after the focused one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsPanelFocusPrevious,
        title: "Previous Insights Panel",
        reach: SemanticReach::Client,
        description: "Focus the Insights panel before the focused one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsPanelFocusClear,
        title: "Leave Insights Panel",
        reach: SemanticReach::Client,
        description: "Leave the focused Insights panel",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsPanelSelectionClear,
        title: "Clear Insights Selection",
        reach: SemanticReach::Client,
        description: "Clear what the focused Insights panel has selected",
        slash: None,
        keybinding: None,
    },
    // Selecting a bin reads nothing: the report holds every bin's count.
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsTurnsBinPrevious,
        title: "Previous Turn Duration Bin",
        reach: SemanticReach::Client,
        description: "Select the Turns panel's duration bin before its selected one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::MetricsTurnsBinNext,
        title: "Next Turn Duration Bin",
        reach: SemanticReach::Client,
        description: "Select the Turns panel's duration bin after its selected one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The bin's place is invocation data, as a bucket's is.
        id: SemanticCommandId::MetricsTurnsBinSelect { bin: 0 },
        title: "Select Turn Duration Bin",
        reach: SemanticReach::Client,
        description: "Select one of the Turns panel's duration bins by its place",
        slash: None,
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
        id: SemanticCommandId::SessionRename,
        title: "Rename Session",
        reach: SemanticReach::Origin,
        // A Sidebar row's context menu and a press on the header's Title name
        // their Session; the slash names the open one.
        description: "Rename a Session, by hand or from a derived proposal",
        slash: Some(SlashCommand {
            name: "rename",
            aliases: &[],
            takes_text: true,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameInsert,
        title: "Insert Title Text",
        reach: SemanticReach::Client,
        description: "Add typed or pasted text to the Title being written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameDeleteBackward,
        title: "Delete Title Text",
        reach: SemanticReach::Client,
        description: "Take the last character back from the Title being written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameClear,
        title: "Clear Title",
        reach: SemanticReach::Client,
        description: "Empty the Title being written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameDerive,
        title: "Derive Title",
        reach: SemanticReach::Origin,
        description: "Propose a Title derived from the Session's Prompts so far",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameSave,
        title: "Save Title",
        reach: SemanticReach::Origin,
        description: "Rename the Session to the Title written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RenameCancel,
        title: "Cancel Rename",
        reach: SemanticReach::Client,
        description: "Stop deriving a proposal, or close the rename dialog",
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
        id: SemanticCommandId::WorkspaceDescriptionEdit,
        title: "Describe Workspace",
        reach: SemanticReach::Origin,
        // Bound inside the Workspace Picker, where "this Workspace" is the
        // row the reader is on, and offered by a row's context menu; there is
        // no slash, which would have no way to say which Workspace it meant.
        description: "Write the Description of a Workspace the Workspace Picker offers",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceDescriptionInsert,
        title: "Insert Description Text",
        reach: SemanticReach::Client,
        description: "Add typed or pasted text to the Description being written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceDescriptionDeleteBackward,
        title: "Delete Description Text",
        reach: SemanticReach::Client,
        description: "Take the last character back from the Description being written",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceDescriptionClear,
        title: "Clear Description",
        reach: SemanticReach::Client,
        description: "Empty the Description being written, so saving lets Suru derive one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceDescriptionSave,
        title: "Save Description",
        reach: SemanticReach::Origin,
        description: "Set the Workspace's Description to what was written, or clear it if blank",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspaceDescriptionCancel,
        title: "Cancel Description",
        reach: SemanticReach::Client,
        description: "Close the Description editor without saving",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspacePickerMenuPrevious,
        title: "Previous Workspace Picker Menu Item",
        reach: SemanticReach::Client,
        description: "Move to the Workspace Picker row menu's previous item",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::WorkspacePickerMenuNext,
        title: "Next Workspace Picker Menu Item",
        reach: SemanticReach::Client,
        description: "Move to the Workspace Picker row menu's next item",
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
            takes_text: false,
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
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectConfirm,
        title: "Confirm Connect Step",
        reach: SemanticReach::Client,
        description: "Inspect the Invite, trust its fingerprint, or try again a pairing a Relay login stopped",
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
        id: SemanticCommandId::ConnectScrollUp,
        title: "Scroll Connect Step Up",
        reach: SemanticReach::Client,
        description: "Scroll the Invite's preview, or why pairing was refused, a row back",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectScrollDown,
        title: "Scroll Connect Step Down",
        reach: SemanticReach::Client,
        description: "Scroll the Invite's preview, or why pairing was refused, a row on",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectPageUp,
        title: "Page Connect Step Up",
        reach: SemanticReach::Client,
        description: "Scroll the Invite's preview, or why pairing was refused, a page back",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectPageDown,
        title: "Page Connect Step Down",
        reach: SemanticReach::Client,
        description: "Scroll the Invite's preview, or why pairing was refused, a page on",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ConnectClose,
        title: "Close Connect Overlay",
        reach: SemanticReach::Client,
        description: "Step back from the Relay login a pairing waits on, or dismiss the Remote Pairing surface",
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
            takes_text: false,
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
        id: SemanticCommandId::RelayOpen,
        title: "Relays",
        // The Relays are the Client's own Server's whichever way the Outlook
        // is turned, so a Remote that has stopped answering refuses nothing.
        reach: SemanticReach::Client,
        description: "List this Server's Relays, add one, log in there, or remove one",
        slash: Some(SlashCommand {
            name: "relay",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayPrevious,
        title: "Previous Relay",
        reach: SemanticReach::Client,
        description: "Focus the previous Relay",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayNext,
        title: "Next Relay",
        reach: SemanticReach::Client,
        description: "Focus the next Relay",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayAdd,
        title: "Add Relay",
        reach: SemanticReach::Client,
        description: "Type a Relay's address and add it to this Server",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayAddressInsert,
        title: "Type Relay Address",
        reach: SemanticReach::Client,
        description: "Insert text into the address of the Relay being added",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayAddressDeleteBackward,
        title: "Delete Relay Address Backward",
        reach: SemanticReach::Client,
        description: "Delete the character before the end of the Relay address",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayLogin,
        title: "Log In at Relay",
        reach: SemanticReach::Client,
        description: "Have this Server log in at the focused Relay, or the one named, showing where to go and the code",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayCopyAddress,
        title: "Copy Login Address",
        reach: SemanticReach::Client,
        description: "Copy the address a Relay login is finished at",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayCopyCode,
        title: "Copy Login Code",
        reach: SemanticReach::Client,
        description: "Copy the code a Relay login is finished with",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayRemove,
        title: "Remove Relay",
        reach: SemanticReach::Client,
        description: "Remove the focused Relay, which forgets this Server's Login there",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayServeThroughToggle,
        title: "Serve Through Relay",
        reach: SemanticReach::Client,
        description: "Choose whether this Server Serves through the focused Relay, which it does only while logged in there and Serving",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::RelayClose,
        title: "Close Relays",
        reach: SemanticReach::Client,
        description: "Step back to the Relay list, or dismiss it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingOpen,
        title: "Onboarding",
        // The flow onboards the Client's own Server whichever way the Outlook
        // is turned, and opening it asks nothing of any Server.
        reach: SemanticReach::Client,
        description: "Take Onboarding again: Providers, derivation, Workspace Roots, and Icons",
        slash: Some(SlashCommand {
            name: "onboarding",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingProvidersPrevious,
        title: "Previous Onboarding Provider",
        reach: SemanticReach::Client,
        description: "Move to the Provider above on Onboarding's Providers step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingProvidersNext,
        title: "Next Onboarding Provider",
        reach: SemanticReach::Client,
        description: "Move to the Provider below on Onboarding's Providers step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingProvidersToggle,
        title: "Toggle Onboarding Provider",
        // Like the settings panel's edits, it writes the Client's own Server's
        // Setting, never the Origin's, so an Origin out of reach does not
        // refuse it.
        reach: SemanticReach::Client,
        description: "Enable or disable the Provider under the cursor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingProvidersRecheck,
        title: "Re-check Providers",
        // It asks the Client's own Server, whose Onboarding this is, whichever
        // Origin is on screen, so no Origin out of reach refuses it.
        reach: SemanticReach::Client,
        description: "Read every Provider's Availability again",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingIconsPrevious,
        title: "Previous Onboarding Icons Answer",
        reach: SemanticReach::Client,
        description: "Move to the answer above on Onboarding's Icons step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingIconsNext,
        title: "Next Onboarding Icons Answer",
        reach: SemanticReach::Client,
        description: "Move to the answer below on Onboarding's Icons step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingNext,
        title: "Next Onboarding Step",
        reach: SemanticReach::Client,
        description: "Confirm this step of Onboarding and go on to the next, or finish",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingBack,
        title: "Previous Onboarding Step",
        reach: SemanticReach::Client,
        description: "Go back to the step of Onboarding before this one",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingLeave,
        title: "Leave Onboarding",
        reach: SemanticReach::Client,
        description: "Leave Onboarding, which counts as taking it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingDerivationPrevious,
        title: "Previous Derivation Choice",
        reach: SemanticReach::Client,
        description: "Move to the choice above in Onboarding's derivation step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingDerivationNext,
        title: "Next Derivation Choice",
        reach: SemanticReach::Client,
        description: "Move to the choice below in Onboarding's derivation step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingDerivationPageUp,
        title: "Page Derivation Choices Up",
        reach: SemanticReach::Client,
        description: "Move a page up the choices in Onboarding's derivation step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingDerivationPageDown,
        title: "Page Derivation Choices Down",
        reach: SemanticReach::Client,
        description: "Move a page down the choices in Onboarding's derivation step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsPrevious,
        title: "Previous Onboarding Root",
        reach: SemanticReach::Client,
        description: "Focus the row above on Onboarding's Workspace Roots step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsNext,
        title: "Next Onboarding Root",
        reach: SemanticReach::Client,
        description: "Focus the row below on Onboarding's Workspace Roots step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsInsert,
        title: "Insert Onboarding Root Text",
        reach: SemanticReach::Client,
        description: "Add typed or pasted text to the focused row on Onboarding's Workspace Roots step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsDeleteBackward,
        title: "Delete Onboarding Root Text",
        reach: SemanticReach::Client,
        description: "Take the last character back from the focused row on Onboarding's Workspace Roots step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsRemove,
        title: "Remove Onboarding Root",
        reach: SemanticReach::Client,
        description: "Remove the focused entry on Onboarding's Workspace Roots step",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::OnboardingRootsBrowse,
        title: "Browse for an Onboarding Root",
        // The browser lists the Client's own Server's directories, whose
        // Onboarding this is, so no Remote's loss refuses it.
        reach: SemanticReach::Client,
        description: "Choose the focused Workspace Root on Onboarding's step in the Directory Browser",
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
            takes_text: false,
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
                      retry an unreachable Remote, act on the Workspace selector, or open the \
                      Landing from the new-Session affordance",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The modifiers held are invocation data, as a bucket's place is.
        id: SemanticCommandId::PointerClick {
            modifiers: KeyModifiers::NONE,
        },
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
        id: SemanticCommandId::AttachmentOpen,
        title: "Open Attachment",
        reach: SemanticReach::Client,
        description: "Open the Attachment a thumbnail shows (reserved: does nothing yet)",
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
        id: SemanticCommandId::ComposerFocus,
        title: "Focus Composer",
        reach: SemanticReach::Client,
        description: "Give the composer the keys the Sidebar or the Aside holds",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::ComposerClipboardPaste,
        title: "Paste from Clipboard",
        // Reading the clipboard is the Client's own; an image's upload that
        // cannot reach its Server fails with a Notice like any other refusal,
        // and text still pastes.
        reach: SemanticReach::Client,
        description: "Paste the clipboard into the composer, attaching an image",
        slash: None,
        keybinding: Some(SemanticKeybinding {
            prefix: None,
            code: KeyCode::Char('v'),
            modifiers: KeyModifiers::CONTROL,
            label: "Ctrl+V",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarLeave,
        title: "Leave Sidebar",
        reach: SemanticReach::Client,
        // Backing out of a set of selector entries or a search is an inner
        // step of backing out of the Sidebar, so the account of the command
        // says it takes one step rather than all of them.
        description: "Back out of the Sidebar a step: close what is open, clear its search, \
                      or hand the keys back to the composer",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SidebarFocus,
        title: "Focus Sidebar",
        reach: SemanticReach::Client,
        description: "Give a shown Sidebar the keys, so what is typed searches it",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideToggle,
        title: "Toggle Aside",
        reach: SemanticReach::Client,
        description: "Show the Aside and give it the keys, give a shown Aside the keys, or hide one holding them",
        slash: Some(SlashCommand {
            name: "aside",
            aliases: &[],
            takes_text: false,
        }),
        keybinding: Some(SemanticKeybinding {
            prefix: Some(LEADER_PREFIX),
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            label: "Ctrl+X A",
        }),
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideWiden,
        title: "Widen Aside",
        reach: SemanticReach::Client,
        description: "Move the Aside's chosen edge one column to the left",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideNarrow,
        title: "Narrow Aside",
        reach: SemanticReach::Client,
        description: "Move the Aside's chosen edge one column to the right",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        // The column count is invocation data, as it is for the Sidebar's.
        id: SemanticCommandId::AsideWidthSet { columns: 0 },
        title: "Set Aside Width",
        reach: SemanticReach::Client,
        description: "Set the Aside's chosen width to an explicit column count",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideWidthReset,
        title: "Reset Aside Width",
        reach: SemanticReach::Client,
        description: "Restore the Aside width it began at",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsidePrevious,
        title: "Previous Aside Entry",
        reach: SemanticReach::Client,
        description: "Move the Aside's row focus to the entry above, wrapping past the top",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideNext,
        title: "Next Aside Entry",
        reach: SemanticReach::Client,
        description: "Move the Aside's row focus to the entry below, wrapping past the end",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideOpen,
        title: "Open Aside Entry",
        reach: SemanticReach::Client,
        description: "Open the Session the Aside's focused entry stands for",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::AsideLeave,
        title: "Leave Aside",
        reach: SemanticReach::Client,
        description: "Hand the keys back from the Aside to the composer, leaving it shown",
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
        description: "Restart an unreachable Remote's catalog stream and refresh its Sessions, or log in at the Relay it needs a login at",
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
            takes_text: false,
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
        description: "Open what focus stands on: a Provider's own Settings, or the surface a value is chosen at",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsProviderLeave,
        title: "Leave Provider Settings",
        reach: SemanticReach::Client,
        description: "Return from a Provider's own Settings to the list of Providers",
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
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListPrevious,
        title: "Previous List Row",
        reach: SemanticReach::Client,
        description: "Focus the previous row of the open list Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListNext,
        title: "Next List Row",
        reach: SemanticReach::Client,
        description: "Focus the next row of the open list Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListInsert,
        title: "Insert List Text",
        reach: SemanticReach::Client,
        description: "Add typed or pasted text to the focused row of the open list Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListDeleteBackward,
        title: "Delete List Text",
        reach: SemanticReach::Client,
        description: "Take the last character back from the focused row of the open list editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListRemove,
        title: "Remove List Row",
        reach: SemanticReach::Client,
        description: "Remove the focused entry from the open list Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListBrowse,
        title: "Browse for a List Entry",
        // The browser lists the Client's own Server's directories, whose
        // Settings the panel edits, so no Remote's loss refuses it.
        reach: SemanticReach::Client,
        description: "Choose the focused entry of the open list of directories in the Directory Browser",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListSave,
        title: "Save List Setting",
        reach: SemanticReach::Client,
        description: "Validate and pin the whole list in the open list Setting editor",
        slash: None,
        keybinding: None,
    },
    SemanticCommandDescriptor {
        id: SemanticCommandId::SettingsListCancel,
        title: "Cancel List Setting",
        reach: SemanticReach::Client,
        description: "Discard the open list Setting editor",
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
                    ) | (
                        SemanticCommandId::AsideWidthSet { .. },
                        SemanticCommandId::AsideWidthSet { .. }
                    ) | (
                        SemanticCommandId::PointerClick { .. },
                        SemanticCommandId::PointerClick { .. }
                    ) | (
                        SemanticCommandId::MetricsBucketSelect { .. },
                        SemanticCommandId::MetricsBucketSelect { .. }
                    ) | (
                        SemanticCommandId::MetricsSeriesToggle { .. },
                        SemanticCommandId::MetricsSeriesToggle { .. }
                    ) | (
                        SemanticCommandId::MetricsSeriesSolo { .. },
                        SemanticCommandId::MetricsSeriesSolo { .. }
                    ) | (
                        SemanticCommandId::MetricsSessionOpen { .. },
                        SemanticCommandId::MetricsSessionOpen { .. }
                    ) | (
                        SemanticCommandId::MetricsTurnsBinSelect { .. },
                        SemanticCommandId::MetricsTurnsBinSelect { .. }
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

/// The command a submitted composer draft invokes, where the draft opens with
/// the name — or an alias — of a slash command that takes text, spelled out
/// whole and followed by nothing but whitespace or by its text: `/compact`,
/// `/compact `, `/compact keep the parser notes`, or that over several lines.
/// Such a command owns its whole draft, which is never a Prompt for the Agent,
/// so a dismissed suggestion still sends the command. What follows the name,
/// trimmed, is its text, and a draft with nothing after the name carries none.
/// Every other draft is the composer's ordinary business.
pub(super) fn slash_text_invocation(text: &str) -> Option<(SemanticCommandId, Option<String>)> {
    let draft = text.strip_prefix('/')?;
    let (name, rest) = draft.split_once(char::is_whitespace).unwrap_or((draft, ""));
    let rest = rest.trim();
    SEMANTIC_COMMANDS.iter().find_map(|command| {
        let slash = command.slash.filter(|slash| slash.takes_text)?;
        (slash.name == name || slash.aliases.contains(&name))
            .then(|| (command.id, (!rest.is_empty()).then(|| rest.to_owned())))
    })
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
    // Onboarding is taken once, and again only on purpose, so it waits past
    // that window rather than pushing Pairing out of it, and so does the
    // Usage Metrics view, looked in on now and then rather than worked in.
    // Each command moves to just after the one it follows, in this order.
    if query.is_empty() {
        for (id, follows) in [
            (
                SemanticCommandId::ConnectOpen,
                SemanticCommandId::ModelOptions,
            ),
            (SemanticCommandId::PairOpen, SemanticCommandId::ConnectOpen),
            (
                SemanticCommandId::OnboardingOpen,
                SemanticCommandId::PairOpen,
            ),
            (
                SemanticCommandId::MetricsShow,
                SemanticCommandId::OnboardingOpen,
            ),
        ] {
            let Some(position) = matches.iter().position(|(_, _, _, held)| *held == id) else {
                continue;
            };
            let command = matches.remove(position);
            let after = matches
                .iter()
                .position(|(_, _, _, held)| *held == follows)
                .map_or(matches.len(), |followed| followed + 1);
            matches.insert(after, command);
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
