//! The settings panel: the tabs it splits its rows across, one row per Setting
//! or Provider, and the typed mutation an edit of one emits.
//!
//! The panel holds no copy of what a Setting is worth. Rows read their values
//! off the latest effective-settings snapshot, so an edit's round trip through
//! the server is what moves a row — the panel never shows a value the Config
//! Document does not yet carry.
//!
//! Five tabs split the listing. Appearance, General, Source Control and Experimental are the
//! schema filtered to their own group, so a Setting added to any appears with
//! no work. Providers is hand-built from the built-in Provider list rather than
//! from the schema: one row per Provider, always and in the built-in order,
//! each row standing for that Provider's Enablement. Which tab a Setting lands
//! on is the Setting's own declaration in the schema, so the panel maps groups
//! to tabs and invents no grouping of its own.
//!
//! Everything else a Provider is configured by is the Provider's own, so it
//! never joins the list: the reader steps in from the Provider's row onto its
//! Settings and back out again, and the list of Providers stays one line
//! apiece. Where the keys stand is view state and nothing more — the panel
//! forgets it on close — and it is one state however the panel is drawn: a
//! wide panel lists the Provider's Settings in the pane beside the Providers,
//! and a narrow one, with no pane to spare, lists them in the Providers' place.
//!
//! The Providers tab is a surface presenting the Providers themselves, so
//! arriving on it re-reads their Availability through the same Model catalog
//! listing the picker asks for. What that read finds is the panel's own to
//! keep, alongside the rest of its ephemeral state: a Provider row reads it,
//! and each arrival asks again rather than trusting the last answer. Enablement
//! decides who is asked — a Provider the reader turned off is never consulted,
//! so its row reports the choice and never a condition Suru did not look for.
//!
//! The reader may also point at the panel. Drawing is the only thing that knows
//! how tall the box came out and which rows the window put on screen, so the
//! frame leaves its geometry here and a click resolves against that, the way a
//! click in the transcript resolves against the viewport it drew. What a
//! pointer can reach stops there: a tab label shows that tab and a row takes
//! the focus, because every edit runs through the focused Setting and pointing
//! at a row is not asking for one.

use std::{borrow::Cow, cell::RefCell, ops::Range};

use crate::{
    protocol::{
        EffectiveSettings, ModelCatalog, ProviderCatalogStatus, ProviderId, ProviderUnavailability,
        SettingMutation,
    },
    provider::built_in_providers,
    settings::{
        NumericSettingChoice, SCHEMA, SettingChoiceSurface, SettingDescriptor, SettingGroup,
        TextListSettingChoice, provider_enablement, provider_settings,
    },
};

use super::{
    ModelListRequest,
    commands::NumericDigit,
    list_editor::{ListEditor, ListEditorView},
    list_window::ListWindow,
};

/// What a row reports about a Provider the read in force asked about and the
/// answer passed over. Nothing came back for it, which is Suru's problem to
/// report rather than a condition of the Provider — and reporting it is what
/// ends the read, because a row may not wait for an answer already given.
const UNANSWERED_PROVIDER: &str = "the Model catalog answered for no such Provider";

/// Stands in for a value the effective settings hold but the Setting can
/// neither name nor spell, so a row says what it knows rather than claiming a
/// wrong value.
const UNNAMED_VALUE: &str = "unknown";

/// One tab of the panel, which is one group of Settings under the name the
/// reader picks it out by.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SettingsTab {
    #[default]
    General,
    Appearance,
    Transcript,
    Providers,
    SourceControl,
    Experimental,
}

impl SettingsTab {
    /// Every tab, in the order the tab bar draws them and Left and Right walk
    /// them. The default is where an opening panel lands.
    pub(super) const ALL: &'static [Self] = &[
        Self::General,
        Self::Providers,
        Self::Appearance,
        Self::Transcript,
        Self::SourceControl,
        Self::Experimental,
    ];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Transcript => "Transcript",
            Self::Providers => "Providers",
            Self::SourceControl => "Source Control",
            Self::Experimental => "Experimental",
        }
    }

    /// The group of Settings this tab is the surface for.
    fn group(self) -> SettingGroup {
        match self {
            Self::General => SettingGroup::General,
            Self::Appearance => SettingGroup::Appearance,
            Self::Transcript => SettingGroup::Transcript,
            Self::Providers => SettingGroup::Providers,
            Self::SourceControl => SettingGroup::SourceControl,
            Self::Experimental => SettingGroup::Experimental,
        }
    }

    /// Where this tab sits in the bar, which is also where the panel keeps the
    /// row the reader left focused on it.
    fn position(self) -> usize {
        Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("every tab is one of ALL")
    }
}

/// Where the panel is and what it last heard, which is all the state an edit
/// needs: the values themselves live in the snapshot.
#[derive(Clone, Debug, Default)]
pub(super) struct SettingsPanel {
    open: bool,
    /// The tab the panel is showing.
    tab: SettingsTab,
    /// Where the reader stands on each tab, kept apart so hopping to the
    /// Providers tab and back does not cost them their place in General.
    /// Ephemeral by design: an opening panel always starts at the first tab's
    /// top row.
    places: [TabPlace; SettingsTab::ALL.len()],
    /// Where the reader stands among the focused Provider's own Settings.
    /// Ephemeral like the tab places: an opening panel has the keys on the
    /// Providers themselves, so the reader always meets the same short list.
    provider_place: ProviderPlace,
    /// What the Availability read in force found, one entry per Provider it
    /// asked about — which is every enabled Provider and no other. A Provider
    /// with no entry here is one nothing has been read about, and its row says
    /// as much by saying nothing.
    availability: Vec<ProviderReading>,
    /// The catalog listing those readings are waiting on. Answers to any other
    /// listing are somebody else's: a reader who left the tab and came back has
    /// asked again, and the older answer must not settle the newer question.
    awaited_listing: Option<ModelListRequest>,
    /// Why the last edit never reached the Config Document. Cleared by the
    /// next edit, so a stale complaint never outlives the attempt that earned
    /// it.
    error: Option<String>,
    /// Where the frame in force drew the tab bar and the rows, which is what a
    /// pointer resolves against. Rendering leaves it here, so it is held behind
    /// a cell rather than taken by an edit.
    layout: RefCell<PanelLayout>,
    numeric_editor: Option<NumericEditor>,
    list_editor: Option<ListEditor>,
}

/// Where the reader stands on one tab: the row focus is on, and the window
/// over the tab's rows, which a tab the reader comes back to keeps as they
/// left it.
#[derive(Clone, Debug, Default)]
struct TabPlace {
    focus: usize,
    window: ListWindow,
}

/// Where the reader stands among the focused Provider's own Settings: the one
/// the pane picks out and details, the window a narrow panel lists them
/// through, and whether the keys have stepped in onto them from the
/// Provider's row. It belongs to the Provider focus stands on, so focus moving
/// to another Provider starts that Provider's Settings afresh at the first.
#[derive(Clone, Debug, Default)]
struct ProviderPlace {
    entered: bool,
    focus: usize,
    window: ListWindow,
}

impl ProviderPlace {
    /// A place at the first Setting, the keys still on the Providers, and a
    /// window that opens at the head of the list.
    fn fresh() -> Self {
        let place = Self::default();
        place.window.open();
        place
    }
}

/// Which keys the panel answers to, which is the one thing the keymap has to
/// know about it: stepped in onto a Provider's own Settings, the keys that
/// walk the tabs and close the panel step back out to the Providers instead.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum PanelKeys {
    /// The rows of the tab being shown.
    #[default]
    Rows,
    /// The focused Provider's own Settings.
    ProviderSettings,
}

#[derive(Clone, Debug)]
struct NumericEditor {
    choice: NumericSettingChoice,
    input: String,
    error: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NumericEditorView<'a> {
    pub(super) label: &'static str,
    pub(super) input: &'a str,
    pub(super) error: Option<&'static str>,
}

/// One tab as the tab bar draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SettingsTabLabel {
    pub(super) tab: SettingsTab,
    pub(super) title: &'static str,
    pub(super) active: bool,
}

/// Where a frame drew the panel's pointable surfaces. Everything here is
/// terminal geometry the drawing decided, which is why drawing is what records
/// it; a panel nothing has drawn yet answers no click, because a layout that
/// claimed a geometry it had not drawn would move the focus somewhere the
/// reader never pointed.
#[derive(Clone, Debug, Default)]
pub(super) struct PanelLayout {
    /// The columns inside the box's border, so a click on the border or on the
    /// screen the overlay covers lands on nothing.
    columns: Range<u16>,
    /// The tab bar, where the box had the room to draw one.
    tabs: Option<TabBar>,
    /// The rows of the tab being shown, where any were drawn at all.
    rows: Option<RowWindow>,
    /// The focused Provider's own Settings, where any were drawn: in the pane
    /// beside the Providers, or in their place.
    provider_settings: Option<RowWindow>,
}

/// The tab bar as one frame drew it: the row it went on, and the columns each
/// label fills.
#[derive(Clone, Debug)]
pub(super) struct TabBar {
    pub(super) row: u16,
    pub(super) labels: Vec<TabSpan>,
}

/// One tab label as the bar drew it: the tab it names and the columns it fills.
#[derive(Clone, Debug)]
pub(super) struct TabSpan {
    pub(super) tab: SettingsTab,
    pub(super) columns: Range<u16>,
}

/// The rows as one frame drew them: where they began, which row of the tab the
/// first of them was, how many followed, and the columns they were drawn
/// across. A tab is longer than the box has room for, so a pointer means
/// nothing without knowing which of its rows the window put on screen; and a
/// wide panel draws a pane beside them, so a pointer at the same height is not
/// necessarily on a row.
#[derive(Clone, Debug)]
pub(super) struct RowWindow {
    pub(super) top: u16,
    pub(super) first: usize,
    pub(super) count: u16,
    pub(super) columns: Range<u16>,
}

/// What the pointer landed on, which is a tab to show or a row to focus and
/// never anything a reader has to undo.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelHit {
    Tab(SettingsTab),
    Row(usize),
    ProviderSetting(usize),
}

impl PanelLayout {
    /// The geometry one frame drew, out of the parts the drawing decided: what
    /// a valid layout is stays here rather than with whoever assembled it.
    pub(super) fn new(
        columns: Range<u16>,
        tabs: Option<TabBar>,
        rows: Option<RowWindow>,
        provider_settings: Option<RowWindow>,
    ) -> Self {
        Self {
            columns,
            tabs,
            rows,
            provider_settings,
        }
    }

    fn hit(&self, column: u16, row: u16) -> Option<PanelHit> {
        if !self.columns.contains(&column) {
            return None;
        }
        if let Some(bar) = &self.tabs
            && bar.row == row
            && let Some(span) = bar
                .labels
                .iter()
                .find(|span| span.columns.contains(&column))
        {
            return Some(PanelHit::Tab(span.tab));
        }
        if let Some(index) = self.rows.as_ref().and_then(|rows| rows.hit(column, row)) {
            return Some(PanelHit::Row(index));
        }
        self.provider_settings
            .as_ref()
            .and_then(|rows| rows.hit(column, row))
            .map(PanelHit::ProviderSetting)
    }
}

impl RowWindow {
    /// Which of the listed rows a pointer at `column` and `row` is on, and
    /// none for a pointer beside or past them.
    fn hit(&self, column: u16, row: u16) -> Option<usize> {
        if !self.columns.contains(&column) {
            return None;
        }
        let offset = row.checked_sub(self.top)?;
        (offset < self.count).then(|| self.first.saturating_add(usize::from(offset)))
    }
}

/// One row as the panel presents it: what it is called, what it is worth, and
/// whether that value is the reader's own choice or the built-in default.
#[derive(Clone, Debug)]
pub(super) struct SettingRow {
    pub(super) label: &'static str,
    /// The Provider this row presents, where the row is a Provider rather
    /// than one of its Settings. Rendering uses the identity for Provider
    /// presentation such as an optional icon without inferring it from the
    /// display name.
    pub(super) provider: Option<&'static ProviderId>,
    pub(super) value: RowValue,
    pub(super) pinned: bool,
    /// Whether this is the row focus stands on in its list, which is the one
    /// the keys act on while that list holds them and the one they come back
    /// to while it does not.
    pub(super) selected: bool,
    /// What opening this row leads to, which is what Enter does on it and so
    /// what a footer has to teach.
    pub(super) opens: RowOpens,
    pub(super) availability: RowAvailability,
}

/// What opening a row leads to: nothing, a surface where its value is chosen,
/// or the Provider's own Settings it stands at the head of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RowOpens {
    Nothing,
    Choice,
    ProviderSettings,
}

/// What one Provider's Availability read has come to, held beside the message
/// the reader gets in full when the row is theirs to act on.
#[derive(Clone, Debug)]
struct ProviderReading {
    provider: ProviderId,
    availability: RowAvailability,
    /// What the Provider or the read itself said, whole. `None` where there is
    /// nothing to say beyond the row.
    message: Option<String>,
}

/// What arriving on a tab asks of Suru. Availability is a fact about the
/// environment rather than a choice, so the surface presenting the Providers
/// re-reads it on arrival and every other arrival asks nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum AvailabilityRead {
    /// The panel is now waiting on a catalog listing, which its caller owes it.
    Begun,
    /// Nothing was asked, so nothing is owed.
    None,
}

/// What a Provider row says about Availability. Every state but the quiet one
/// asks something of the reader, which is why the quiet one is what a Provider
/// still serving its catalog gets: Suru reports a condition, not a heartbeat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RowAvailability {
    /// Nothing to report: a catalog that answered, a Provider nothing has been
    /// read about, and every row that is not a Provider's.
    Quiet,
    /// Being read right now, which is live work and so wears a Spinner.
    Reading,
    /// Usable, with a compatibility condition worth fixing.
    Warning,
    /// Unusable until the reader fixes `reason` outside Suru, which they do
    /// as the Provider's own `fix` says.
    Unavailable {
        reason: ProviderUnavailability,
        fix: String,
    },
    /// The read itself failed, which is Suru's problem to report rather than a
    /// condition of the Provider.
    Failed,
}

/// What a row says it is worth, which reads differently for a Setting the
/// reader cycles through values and for a Provider whose row is its Enablement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RowValue {
    /// The Setting's value, spelled the way the Setting itself spells it —
    /// borrowed for one of the values the schema names, owned for a value an
    /// Open Setting had to spell for itself.
    Choice(Cow<'static, str>),
    /// A Provider the reader has left on. The quiet state is the good one, so
    /// the row says nothing beyond the Provider's name.
    ProviderEnabled,
    /// A Provider the reader turned off — the one thing a row may claim about
    /// a Provider Suru has not consulted.
    ProviderDisabled,
}

/// The Provider focus stands on, as a panel showing the Providers accounts
/// for it: its own row, the Enablement that row stands for, and its further
/// Settings valued from the snapshot, the one the pane picks out marked as
/// selected among them.
pub(super) struct ProviderFocus {
    pub(super) row: SettingRow,
    pub(super) enablement: &'static SettingDescriptor,
    pub(super) enabled: bool,
    pub(super) settings: Vec<SettingRow>,
    /// The Setting among them the pane picks out, which it details.
    pub(super) picked: Option<&'static SettingDescriptor>,
}

/// One row of a tab before a snapshot values it: what it is called, the Setting
/// an edit of it acts on, and whether it stands for a Provider rather than for
/// a value to read.
struct PanelEntry {
    label: &'static str,
    descriptor: &'static SettingDescriptor,
    provider: Option<&'static ProviderId>,
}

impl PanelEntry {
    /// Where this row's value is chosen, for a row that leads to such a
    /// surface at all. A Provider's row stands for its Enablement, which is
    /// cycled rather than chosen, so it leads nowhere however the Setting
    /// behind it is declared.
    fn choice_surface(&self) -> Option<SettingChoiceSurface> {
        self.provider
            .is_none()
            .then(|| self.descriptor.chosen_at())?
    }

    /// What opening this row leads to: a Provider's row to the Settings the
    /// Provider holds beyond its Enablement, where it holds any, and any other
    /// row to the surface its value is chosen at, where it has one.
    fn opens(&self) -> RowOpens {
        match self.provider {
            Some(provider) if !provider_settings(provider).is_empty() => RowOpens::ProviderSettings,
            Some(_) => RowOpens::Nothing,
            None if self.choice_surface().is_some() => RowOpens::Choice,
            None => RowOpens::Nothing,
        }
    }
}

impl SettingsPanel {
    /// Opens the panel fresh every time: the first tab, its top row, and no
    /// complaint carried over from the last time the reader was here.
    pub(super) fn open(&mut self) {
        *self = Self {
            open: true,
            provider_place: ProviderPlace::fresh(),
            ..Self::default()
        };
        for place in &self.places {
            place.window.open();
        }
    }

    pub(super) fn close(&mut self) {
        *self = Self::default();
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn open_numeric_editor(
        &mut self,
        choice: NumericSettingChoice,
        settings: &EffectiveSettings,
    ) {
        self.numeric_editor = Some(NumericEditor {
            choice,
            input: choice.seed(settings),
            error: None,
        });
    }

    pub(super) fn numeric_editor(&self) -> Option<NumericEditorView<'_>> {
        self.numeric_editor
            .as_ref()
            .map(|editor| NumericEditorView {
                label: editor.choice.label(),
                input: &editor.input,
                error: editor.error,
            })
    }

    pub(super) fn numeric_editor_is_open(&self) -> bool {
        self.numeric_editor.is_some()
    }

    pub(super) fn insert_numeric_digit(&mut self, digit: NumericDigit) {
        let Some(editor) = &mut self.numeric_editor else {
            return;
        };
        editor.input.push(digit.as_char());
        editor.error = None;
    }

    pub(super) fn delete_numeric_backward(&mut self) {
        if let Some(editor) = &mut self.numeric_editor {
            editor.input.pop();
            editor.error = None;
        }
    }

    pub(super) fn apply_numeric_edit(&mut self) -> Option<SettingMutation> {
        let editor = self.numeric_editor.as_mut()?;
        match editor.choice.accept(&editor.input) {
            Ok(mutation) => {
                self.numeric_editor = None;
                Some(mutation)
            }
            Err(error) => {
                editor.error = Some(error);
                None
            }
        }
    }

    pub(super) fn cancel_numeric_edit(&mut self) {
        self.numeric_editor = None;
    }

    pub(super) fn open_list_editor(
        &mut self,
        choice: TextListSettingChoice,
        settings: &EffectiveSettings,
    ) {
        self.list_editor = Some(ListEditor::open(choice, settings));
    }

    pub(super) fn list_editor(&self) -> Option<ListEditorView<'_>> {
        self.list_editor.as_ref().map(ListEditor::view)
    }

    pub(super) fn list_editor_is_open(&self) -> bool {
        self.list_editor.is_some()
    }

    /// The open list editor, for an edit of the draft it holds.
    pub(super) fn edit_list(&mut self) -> Option<&mut ListEditor> {
        self.list_editor.as_mut()
    }

    /// Saves the open list editor's draft as one mutation, closing it, or
    /// leaves it open saying which entry the Setting refuses.
    pub(super) fn save_list_edit(&mut self) -> Option<SettingMutation> {
        let mutation = self.list_editor.as_mut()?.save()?;
        self.list_editor = None;
        Some(mutation)
    }

    pub(super) fn cancel_list_edit(&mut self) {
        self.list_editor = None;
    }

    /// Takes the reason an edit never landed, so the reader learns the Config
    /// Document did not change rather than watching a row silently refuse.
    pub(super) fn report_failure(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub(super) fn select_previous_tab(&mut self, settings: &EffectiveSettings) -> AvailabilityRead {
        self.move_tab(-1, settings)
    }

    pub(super) fn select_next_tab(&mut self, settings: &EffectiveSettings) -> AvailabilityRead {
        self.move_tab(1, settings)
    }

    /// The window over the rows of the tab being shown.
    pub(super) fn window(&self) -> &ListWindow {
        &self.places[self.tab.position()].window
    }

    /// The window a narrow panel lists the focused Provider's own Settings
    /// through.
    pub(super) fn provider_window(&self) -> &ListWindow {
        &self.provider_place.window
    }

    /// Which keys the panel answers to: its rows, or the focused Provider's
    /// own Settings once the reader has stepped in onto them.
    pub(super) fn keys(&self) -> PanelKeys {
        if self.open && self.tab == SettingsTab::Providers && self.provider_place.entered {
            PanelKeys::ProviderSettings
        } else {
            PanelKeys::Rows
        }
    }

    /// Steps the keys back out of the focused Provider's own Settings onto the
    /// Providers, with focus on the Provider they stepped in from and the pane
    /// still picking out the Setting they left, so stepping in again lands
    /// where they were. A move like Up and Down, it takes the standing
    /// complaint away as they do; keys already on the rows stay there.
    pub(super) fn leave_provider(&mut self) {
        if self.keys() == PanelKeys::ProviderSettings {
            self.error = None;
            self.provider_place.entered = false;
        }
    }

    /// Takes the geometry the frame just drew, which is the only account of the
    /// panel a pointer can be resolved against.
    pub(super) fn record_layout(&self, layout: PanelLayout) {
        self.layout.replace(layout);
    }

    /// Gives up the geometry a frame recorded. A frame that draws no panel —
    /// because none is open, or because the terminal came out too small for one
    /// — leaves nothing to point at, so a click cannot land on where the panel
    /// used to be.
    pub(super) fn forget_layout(&self) {
        self.record_layout(PanelLayout::default());
    }

    /// Moves the panel to whatever the reader pointed at: a tab label shows
    /// that tab, a row takes the focus — one of a Provider's own Settings
    /// taking the keys along with it — and a click on anything else — the
    /// headline, the rest of the pane beside the rows, the footer, the border,
    /// the screen the box is drawn over — changes nothing. A click never edits
    /// a Setting: the focused row is the one surface an edit runs through, so
    /// pointing at a row asks for the focus and never for the value.
    ///
    /// Like every other way into the panel, this does nothing at all while the
    /// panel is closed, so a click resolved somewhere the panel is not can
    /// neither move a focus the reader cannot see nor send Suru off to consult
    /// the Providers.
    ///
    /// The pointer mints no semantic command of its own, because it introduces
    /// no behavior: showing a tab and focusing a row are what the tab and row
    /// keys already do, and a plugin reaches both through those. It only says
    /// which one, by position — and a position is this frame's to know rather
    /// than something a command could name, which is why the hit test ends
    /// here rather than in a subject a command could carry.
    pub(super) fn focus_at(
        &mut self,
        column: u16,
        row: u16,
        settings: &EffectiveSettings,
    ) -> AvailabilityRead {
        if !self.open {
            return AvailabilityRead::None;
        }
        let hit = self.layout.borrow().hit(column, row);
        match hit {
            Some(PanelHit::Tab(tab)) => self.show_tab(tab, settings),
            Some(PanelHit::Row(row)) => {
                self.focus_row(row);
                AvailabilityRead::None
            }
            Some(PanelHit::ProviderSetting(row)) => {
                self.focus_provider_setting(row);
                AvailabilityRead::None
            }
            None => AvailabilityRead::None,
        }
    }

    /// Notes the listing the read in force is waiting on, which is the one
    /// answer that may settle it.
    pub(super) fn await_listing(&mut self, request: ModelListRequest) {
        self.awaited_listing = Some(request);
    }

    /// Takes what the awaited listing found, for the Providers this panel asked
    /// about and no others: an answer naming a Provider nothing was read about
    /// — one the reader has turned off — changes nothing here, because a row
    /// may not claim what Suru never looked for. A Provider the answer passes
    /// over has still been answered, badly, and says so rather than waiting on.
    pub(super) fn adopt_catalog(&mut self, request: &ModelListRequest, catalog: &ModelCatalog) {
        if self.awaited_listing.as_ref() != Some(request) {
            return;
        }
        for reading in &mut self.availability {
            let Some(found) = catalog
                .providers
                .iter()
                .find(|listed| listed.provider == reading.provider)
            else {
                reading.availability = RowAvailability::Failed;
                reading.message = Some(UNANSWERED_PROVIDER.to_owned());
                continue;
            };
            let (availability, message) = match &found.status {
                // A refresh the server armed is this read still running, so
                // the row keeps its Spinner until the settled answer lands.
                ProviderCatalogStatus::Refreshing => (RowAvailability::Reading, None),
                ProviderCatalogStatus::Warning { message } => {
                    (RowAvailability::Warning, Some(message.clone()))
                }
                ProviderCatalogStatus::Unavailable {
                    reason,
                    message,
                    fix,
                } => (
                    RowAvailability::Unavailable {
                        reason: *reason,
                        fix: fix.clone(),
                    },
                    Some(message.clone()),
                ),
                ProviderCatalogStatus::Failed { message } => {
                    (RowAvailability::Failed, Some(message.clone()))
                }
                // A catalog still serving what it last read asks nothing of
                // the reader, whether or not the newest refresh went through.
                ProviderCatalogStatus::Fresh
                | ProviderCatalogStatus::Stale { .. }
                | ProviderCatalogStatus::Disabled => (RowAvailability::Quiet, None),
            };
            reading.availability = availability;
            reading.message = message;
        }
    }

    /// Takes the reason the awaited listing never came back. It was the one
    /// thing asked of every Provider at once, so its failure is every
    /// asked-about Provider's failure.
    pub(super) fn report_read_failure(&mut self, request: &ModelListRequest, error: &str) {
        if self.awaited_listing.as_ref() != Some(request) {
            return;
        }
        for reading in &mut self.availability {
            reading.availability = RowAvailability::Failed;
            reading.message = Some(error.to_owned());
        }
    }

    /// Whether a read is still out on the tab showing it, which is what keeps
    /// the Spinner animating. A reader who has walked away from the Providers
    /// is watching nothing spin, whatever is still in flight for them.
    pub(super) fn is_reading(&self, settings: &EffectiveSettings) -> bool {
        self.open
            && self.tab == SettingsTab::Providers
            && built_in_providers().iter().any(|provider| {
                self.reading(&provider.id, settings)
                    .is_some_and(|reading| reading.availability == RowAvailability::Reading)
            })
    }

    /// What the focused Provider's Availability says in full, which the
    /// headline or the pane carries because a row has room only for the
    /// condition's name.
    pub(super) fn selected_message(&self, settings: &EffectiveSettings) -> Option<&str> {
        self.selected_reading(settings)?.message.as_deref()
    }

    /// What fixes the focused Provider's condition, in full, which goes
    /// beneath the headline, or beneath the condition in the pane, because the
    /// row it is named on may have room for only the start of it.
    pub(super) fn selected_fix(&self, settings: &EffectiveSettings) -> Option<&str> {
        match self.selected_availability(settings)? {
            RowAvailability::Unavailable { fix, .. } => Some(fix),
            RowAvailability::Quiet
            | RowAvailability::Reading
            | RowAvailability::Warning
            | RowAvailability::Failed => None,
        }
    }

    /// What the read in force found about the focused Provider, for focus on
    /// a Provider anything was read about, which says how urgently the pane
    /// tells its condition.
    pub(super) fn selected_availability(
        &self,
        settings: &EffectiveSettings,
    ) -> Option<&RowAvailability> {
        Some(&self.selected_reading(settings)?.availability)
    }

    fn selected_reading(&self, settings: &EffectiveSettings) -> Option<&ProviderReading> {
        if !self.open {
            return None;
        }
        let entries = self.entries();
        let provider = entries.get(self.selected_row(entries.len()))?.provider?;
        self.reading(provider, settings)
    }

    /// The tab bar: every tab, always, with the one being shown marked as such.
    pub(super) fn tabs(&self) -> Vec<SettingsTabLabel> {
        SettingsTab::ALL
            .iter()
            .map(|tab| SettingsTabLabel {
                tab: *tab,
                title: tab.title(),
                active: *tab == self.tab,
            })
            .collect()
    }

    /// The Setting an edit acts on: the focused row of an open panel, or the
    /// focused Provider's own Setting the keys have stepped in onto, and
    /// nothing at all while the panel is closed. Every edit runs through here,
    /// so a command invoked from somewhere the panel is not — a plugin, a
    /// click — cannot reach a Setting the reader never focused.
    pub(super) fn selected_descriptor(&self) -> Option<&'static SettingDescriptor> {
        if !self.open {
            return None;
        }
        match self.keys() {
            PanelKeys::Rows => {
                let entries = self.entries();
                entries
                    .get(self.selected_row(entries.len()))
                    .map(|entry| entry.descriptor)
            }
            PanelKeys::ProviderSettings => self.picked_provider_setting(),
        }
    }

    /// The active tab's rows, valued from the snapshot.
    pub(super) fn rows(&self, settings: &EffectiveSettings, pinned: &[String]) -> Vec<SettingRow> {
        let entries = self.entries();
        let selected = self.selected_row(entries.len());
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| self.row(entry, index == selected, settings, pinned))
            .collect()
    }

    /// The Provider focus stands on, for a panel showing the Providers, with
    /// its own further Settings; none on any other tab.
    pub(super) fn provider_focus(
        &self,
        settings: &EffectiveSettings,
        pinned: &[String],
    ) -> Option<ProviderFocus> {
        if !self.open || self.tab != SettingsTab::Providers {
            return None;
        }
        let entries = self.entries();
        let entry = entries.get(self.selected_row(entries.len()))?;
        let provider = entry.provider?;
        let further = provider_settings(provider);
        let picked = self.picked_row(further.len());
        Some(ProviderFocus {
            row: self.row(entry, true, settings, pinned),
            enablement: entry.descriptor,
            enabled: settings.provider_enabled(provider),
            settings: further
                .iter()
                .enumerate()
                .map(|(index, descriptor)| {
                    let entry = PanelEntry {
                        label: descriptor.label,
                        descriptor,
                        provider: None,
                    };
                    self.row(&entry, index == picked, settings, pinned)
                })
                .collect(),
            picked: further.get(picked).copied(),
        })
    }

    /// One row valued from the snapshot.
    fn row(
        &self,
        entry: &PanelEntry,
        selected: bool,
        settings: &EffectiveSettings,
        pinned: &[String],
    ) -> SettingRow {
        let (value, availability) = match entry.provider {
            Some(provider) if settings.provider_enabled(provider) => (
                RowValue::ProviderEnabled,
                self.reading(provider, settings)
                    .map_or(RowAvailability::Quiet, |reading| {
                        reading.availability.clone()
                    }),
            ),
            // A Provider the reader turned off is one Suru leaves entirely
            // alone, so its row reports that choice and never a condition
            // nothing looked for.
            Some(_) => (RowValue::ProviderDisabled, RowAvailability::Quiet),
            // A Setting that can put no words to what it holds is a schema
            // that fell behind its own types, not a reason to refuse the
            // reader the rest of the panel.
            None => (
                RowValue::Choice(
                    entry
                        .descriptor
                        .spelling(settings)
                        .unwrap_or(Cow::Borrowed(UNNAMED_VALUE)),
                ),
                RowAvailability::Quiet,
            ),
        };
        SettingRow {
            label: entry.label,
            provider: entry.provider,
            value,
            pinned: pinned.iter().any(|key| key == entry.descriptor.key),
            selected,
            opens: entry.opens(),
            availability,
        }
    }

    /// Opens what focus stands on. That is one meaning rather than two: a
    /// Provider stands for the further Settings it holds, so opening it steps
    /// the keys in onto them; a Setting whose value is chosen rather than
    /// cycled stands for the surface that chooses it, so opening it answers
    /// with that surface for the caller to show — the focused row's, or the
    /// Provider's own Setting the keys have stepped in onto. Anything standing
    /// for neither is left exactly as it was.
    ///
    /// Like an edit, this reaches a row only through the focus, so a command
    /// arriving while the panel is closed opens nothing.
    pub(super) fn open_row(&mut self) -> Option<SettingChoiceSurface> {
        if !self.open {
            return None;
        }
        // The complaint is cleared only once the key has something to do, so
        // a row Enter passes over does not quietly take away the complaint the
        // reader is reading.
        match self.keys() {
            PanelKeys::Rows => {
                let entries = self.entries();
                let entry = entries.get(self.selected_row(entries.len()))?;
                match entry.opens() {
                    RowOpens::Nothing => None,
                    RowOpens::Choice => {
                        self.error = None;
                        entry.choice_surface()
                    }
                    RowOpens::ProviderSettings => {
                        self.error = None;
                        self.provider_place.entered = true;
                        self.provider_place.window.reveal();
                        None
                    }
                }
            }
            PanelKeys::ProviderSettings => {
                let surface = self.picked_provider_setting()?.chosen_at()?;
                self.error = None;
                Some(surface)
            }
        }
    }

    /// Moves the focused Setting one choice on from the value in force,
    /// wrapping past the last, and pins where it lands. On a Provider's row
    /// that Setting is its Enablement, so the same key that changes a value
    /// turns a Provider on or off. The step is measured against the snapshot
    /// rather than anything staged, because a Setting is not a transaction:
    /// each press is a whole edit that the server answers with the snapshot the
    /// next press steps from.
    pub(super) fn cycle(&mut self, settings: &EffectiveSettings) -> Option<SettingMutation> {
        self.error = None;
        let descriptor = self.selected_descriptor()?;
        Some(descriptor.next_choice(settings)?.mutation())
    }

    /// Takes the focused Setting's pin out of the Config Document. The reset
    /// goes out whether or not the panel believes the Setting is pinned: the
    /// file is the server's to know, and a reset that second-guessed a stale
    /// pin list would silently refuse the one thing the reader asked for. An
    /// unset of something never pinned leaves the document untouched anyway.
    pub(super) fn reset(&mut self) -> Option<SettingMutation> {
        self.error = None;
        Some(self.selected_descriptor()?.reset.clone())
    }

    /// The rows of the tab being shown. A tab presenting Settings alone is the
    /// schema filtered to its group; Providers is built from the built-in
    /// Provider list so that every Provider holds its place whatever the
    /// reader has done to it.
    fn entries(&self) -> Vec<PanelEntry> {
        let tab = self.tab;
        match tab {
            SettingsTab::General
            | SettingsTab::Appearance
            | SettingsTab::Transcript
            | SettingsTab::SourceControl
            | SettingsTab::Experimental => SCHEMA
                .iter()
                .filter(|descriptor| descriptor.group == tab.group())
                .map(|descriptor| PanelEntry {
                    label: descriptor.label,
                    descriptor,
                    provider: None,
                })
                .collect(),
            // A Provider whose Enablement the schema does not define has no
            // row to offer, because the row is that Setting's surface. The
            // guard beside the Provider list is what keeps that from
            // shipping.
            SettingsTab::Providers => built_in_providers()
                .iter()
                .filter_map(|provider| {
                    Some(PanelEntry {
                        label: provider.display_name.as_str(),
                        descriptor: provider_enablement(&provider.id)?,
                        provider: Some(&provider.id),
                    })
                })
                .collect(),
        }
    }

    /// Where the focus sits on the tab being shown, held inside a listing of
    /// `rows`. The focus is clamped where it is read rather than only where it
    /// is moved, so a listing that ever grows shorter under it lands the
    /// reader on its last row instead of on nothing at all.
    fn selected_row(&self, rows: usize) -> usize {
        self.places[self.tab.position()]
            .focus
            .min(rows.saturating_sub(1))
    }

    /// Which of the focused Provider's `rows` own Settings the pane picks
    /// out, clamped as [`Self::selected_row`] clamps.
    fn picked_row(&self, rows: usize) -> usize {
        self.provider_place.focus.min(rows.saturating_sub(1))
    }

    /// The focused Provider's own Setting the pane picks out, which is the
    /// one an edit acts on once the keys have stepped in.
    fn picked_provider_setting(&self) -> Option<&'static SettingDescriptor> {
        let further = self.focused_provider_settings();
        further.get(self.picked_row(further.len())).copied()
    }

    /// The focused Provider's own further Settings, and none where focus
    /// stands on no Provider.
    fn focused_provider_settings(&self) -> Vec<&'static SettingDescriptor> {
        if self.tab != SettingsTab::Providers {
            return Vec::new();
        }
        let entries = self.entries();
        entries
            .get(self.selected_row(entries.len()))
            .and_then(|entry| entry.provider)
            .map(provider_settings)
            .unwrap_or_default()
    }

    /// Walks focus through whichever list holds the keys, wrapping at either
    /// end: the tab's rows, or the focused Provider's own Settings, which the
    /// keys never walk out of — stepping out is a key of its own. Walking off
    /// one Provider onto another starts the next one's Settings afresh.
    fn move_selection(&mut self, distance: isize) {
        self.error = None;
        let step = |focus: usize, rows: usize| {
            (focus as isize + distance).rem_euclid(rows as isize) as usize
        };
        if self.keys() == PanelKeys::ProviderSettings {
            let rows = self.focused_provider_settings().len();
            if rows > 0 {
                let place = &mut self.provider_place;
                place.focus = step(place.focus, rows);
                place.window.reveal();
            }
            return;
        }
        let rows = self.entries().len();
        if rows == 0 {
            return;
        }
        let before = self.selected_row(rows);
        let place = &mut self.places[self.tab.position()];
        place.focus = step(before, rows);
        place.window.reveal();
        if self.tab == SettingsTab::Providers && place.focus != before {
            self.provider_place = ProviderPlace::fresh();
        }
    }

    /// Walks the tab bar, wrapping at either end so neither is a dead end. The
    /// tab arrived at is where the reader left it, not its top row. A closed
    /// panel walks nowhere: a command invoked from somewhere the panel is not
    /// must not send Suru off to consult the Providers.
    fn move_tab(&mut self, distance: isize, settings: &EffectiveSettings) -> AvailabilityRead {
        if !self.open {
            return AvailabilityRead::None;
        }
        let position = (self.tab.position() as isize + distance)
            .rem_euclid(SettingsTab::ALL.len() as isize) as usize;
        self.show_tab(SettingsTab::ALL[position], settings)
    }

    /// Shows one tab, however the reader asked for it. Asking for the tab that
    /// presents the Providers themselves re-reads their Availability, which is
    /// what turning to that surface means — including from the reader already
    /// on it, because pointing at the Providers is asking Suru to consult them,
    /// and that gesture is how a reader who has just signed in outside Suru
    /// watches the row come good. Walking the bar never lands on the tab in
    /// view, so only a pointer can ask this.
    ///
    /// The keys come back out of a Provider's own Settings when the reader
    /// leaves the Providers, so whatever brings them back meets the list.
    fn show_tab(&mut self, tab: SettingsTab, settings: &EffectiveSettings) -> AvailabilityRead {
        self.error = None;
        if tab != self.tab {
            self.provider_place.entered = false;
        }
        self.tab = tab;
        if self.tab != SettingsTab::Providers {
            return AvailabilityRead::None;
        }
        self.begin_availability_read(settings);
        AvailabilityRead::Begun
    }

    /// Focuses one row of the tab being shown, which is a move like Up and Down
    /// and takes the standing complaint away as they do. It is the rows the
    /// reader pointed at, so the keys go to them, out of any Provider's own
    /// Settings; and a Provider other than the one focused starts its
    /// Settings afresh, as walking onto it would.
    fn focus_row(&mut self, row: usize) {
        self.error = None;
        let before = self.selected_row(self.entries().len());
        self.places[self.tab.position()].focus = row;
        self.provider_place.entered = false;
        if self.tab == SettingsTab::Providers && row != before {
            self.provider_place = ProviderPlace::fresh();
        }
    }

    /// Focuses one of the focused Provider's own Settings, stepping the keys
    /// in onto them as Enter would, with the Setting pointed at picked out.
    fn focus_provider_setting(&mut self, row: usize) {
        self.error = None;
        self.provider_place.entered = true;
        self.provider_place.focus = row;
    }

    /// Puts every Provider that is about to be asked about back to waiting.
    /// Only the ones the reader has left on: a disabled Provider is one Suru
    /// leaves entirely alone, so nothing is asked on its behalf and its row has
    /// nothing to wait for.
    fn begin_availability_read(&mut self, settings: &EffectiveSettings) {
        self.availability = built_in_providers()
            .iter()
            .filter(|provider| settings.provider_enabled(&provider.id))
            .map(|provider| ProviderReading {
                provider: provider.id.clone(),
                availability: RowAvailability::Reading,
                message: None,
            })
            .collect();
    }

    /// What the read in force found about one Provider, and nothing at all
    /// about one the reader has turned off since. Enablement decides who is
    /// asked, so it decides who may be reported on — and it decides here once,
    /// so a row and the headline above it can never disagree about whether a
    /// Provider has anything to say.
    fn reading(
        &self,
        provider: &ProviderId,
        settings: &EffectiveSettings,
    ) -> Option<&ProviderReading> {
        if !settings.provider_enabled(provider) {
            return None;
        }
        self.availability
            .iter()
            .find(|reading| &reading.provider == provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Setting reaches the reader only through the tab its group maps to, so
    /// a group no tab lists is a Setting that has silently left the panel. The
    /// second half is what makes this bite: a tab must list the group it claims
    /// and nothing else, so a Setting cannot surface under a heading that does
    /// not describe it.
    #[test]
    fn every_setting_declares_a_group_some_tab_lists() {
        for descriptor in SCHEMA {
            assert!(
                SettingsTab::ALL
                    .iter()
                    .any(|tab| tab.group() == descriptor.group),
                "{} declares a group no settings panel tab lists",
                descriptor.key
            );
        }
        for tab in SettingsTab::ALL {
            let panel = SettingsPanel {
                open: true,
                tab: *tab,
                ..SettingsPanel::default()
            };
            for entry in panel.entries() {
                assert_eq!(
                    entry.descriptor.group,
                    tab.group(),
                    "the {} tab lists {}, which declares another group",
                    tab.title(),
                    entry.descriptor.key
                );
            }
        }
    }

    /// A panel open on the Providers tab, focused on its first Provider.
    fn on_the_providers() -> SettingsPanel {
        let mut panel = SettingsPanel::default();
        panel.open();
        let _ = panel.show_tab(SettingsTab::Providers, &EffectiveSettings::default());
        panel
    }

    /// The key of the Setting an edit would act on, which is how these tests
    /// read where the keys stand.
    fn editing(panel: &SettingsPanel) -> &'static str {
        panel
            .selected_descriptor()
            .expect("an open panel always has a Setting an edit would act on")
            .key
    }

    /// A Provider-scoped Setting reaches the reader as the Provider's own row
    /// or among the Settings stepped into from it, and the two must not
    /// overlap: one keyed on no built-in Provider would have left the panel
    /// silently, and an Enablement repeated among the Provider's own Settings
    /// would give the reader two rows for one value.
    #[test]
    fn every_provider_setting_is_reached_exactly_once_from_the_providers() {
        let mut panel = on_the_providers();
        let mut keys = Vec::new();
        for _ in built_in_providers() {
            keys.push(editing(&panel));
            let further = panel
                .provider_focus(&EffectiveSettings::default(), &[])
                .expect("every row of the Providers tab is a Provider")
                .settings
                .len();
            let _ = panel.open_row();
            if panel.keys() == PanelKeys::ProviderSettings {
                for _ in 0..further {
                    keys.push(editing(&panel));
                    panel.select_next();
                }
                panel.leave_provider();
            }
            panel.select_next();
        }
        for descriptor in SCHEMA
            .iter()
            .filter(|descriptor| descriptor.group == SettingGroup::Providers)
        {
            assert_eq!(
                keys.iter().filter(|key| **key == descriptor.key).count(),
                1,
                "{} is not reached exactly once from the Providers tab: {keys:?}",
                descriptor.key
            );
        }
    }

    /// The Providers tab lists one row per built-in Provider and nothing else:
    /// a Provider's own Settings are stepped into, never listed among them.
    #[test]
    fn the_providers_tab_lists_one_row_per_built_in_provider_in_order() {
        let panel = on_the_providers();
        let rows = panel.rows(&EffectiveSettings::default(), &[]);
        assert_eq!(
            rows.iter().map(|row| row.provider).collect::<Vec<_>>(),
            built_in_providers()
                .iter()
                .map(|provider| Some(&provider.id))
                .collect::<Vec<_>>(),
        );
    }

    /// Enter steps the keys from a Provider's row onto its own Settings, where
    /// they walk and wrap without ever leaving them; stepping out puts them
    /// back on the Provider, and stepping in again lands where they left.
    #[test]
    fn the_keys_step_into_a_providers_settings_walk_them_and_step_back_out() {
        let mut panel = on_the_providers();
        assert_eq!(panel.keys(), PanelKeys::Rows);
        assert_eq!(editing(&panel), "provider.codex.enabled");

        assert!(panel.open_row().is_none(), "stepping in shows no surface");
        assert_eq!(panel.keys(), PanelKeys::ProviderSettings);
        assert_eq!(editing(&panel), "provider.codex.reasoningSummary");
        panel.select_next();
        assert_eq!(editing(&panel), "provider.codex.approvalPolicy");
        panel.select_previous();
        panel.select_previous();
        assert_eq!(
            editing(&panel),
            "provider.codex.sandboxMode",
            "walking off the first wraps to the last of the Provider's own"
        );
        panel.select_next();
        assert_eq!(
            editing(&panel),
            "provider.codex.reasoningSummary",
            "and off the last back to the first, never onto another Provider"
        );
        panel.select_next();

        panel.leave_provider();
        assert_eq!(panel.keys(), PanelKeys::Rows);
        assert_eq!(
            editing(&panel),
            "provider.codex.enabled",
            "stepping out lands on the Provider stepped in from"
        );
        assert_eq!(
            panel
                .provider_focus(&EffectiveSettings::default(), &[])
                .and_then(|focus| focus.picked)
                .map(|descriptor| descriptor.key),
            Some("provider.codex.approvalPolicy"),
            "with the Setting left still picked out"
        );
        let _ = panel.open_row();
        assert_eq!(
            editing(&panel),
            "provider.codex.approvalPolicy",
            "so stepping in again lands where the reader was"
        );
    }

    /// The place among a Provider's Settings belongs to that Provider: moving
    /// focus to another one starts its Settings at the first.
    #[test]
    fn another_provider_starts_its_own_settings_at_the_first() {
        let mut panel = on_the_providers();
        let _ = panel.open_row();
        panel.select_next();
        panel.leave_provider();

        panel.select_next();
        assert_eq!(editing(&panel), "provider.copilot.enabled");
        let _ = panel.open_row();
        assert_eq!(editing(&panel), "provider.copilot.permissions");
        panel.leave_provider();

        panel.select_previous();
        let _ = panel.open_row();
        assert_eq!(
            editing(&panel),
            "provider.codex.reasoningSummary",
            "and coming back to the first Provider starts it afresh too"
        );
    }

    /// Where the keys stand is view state: closing the panel forgets it, and so
    /// does leaving the Providers for another tab.
    #[test]
    fn the_keys_come_back_to_the_providers_on_close_and_on_leaving_the_tab() {
        let mut panel = on_the_providers();
        let _ = panel.open_row();
        let _ = panel.show_tab(SettingsTab::General, &EffectiveSettings::default());
        assert_eq!(panel.keys(), PanelKeys::Rows);
        let _ = panel.show_tab(SettingsTab::Providers, &EffectiveSettings::default());
        assert_eq!(
            panel.keys(),
            PanelKeys::Rows,
            "coming back meets the list of Providers"
        );

        let _ = panel.open_row();
        let _ = panel.show_tab(SettingsTab::Providers, &EffectiveSettings::default());
        assert_eq!(
            panel.keys(),
            PanelKeys::ProviderSettings,
            "asking again for the tab in view re-reads the Providers and keeps the keys"
        );

        panel.close();
        panel.open();
        let _ = panel.show_tab(SettingsTab::Providers, &EffectiveSettings::default());
        assert_eq!(panel.keys(), PanelKeys::Rows);
        assert_eq!(editing(&panel), "provider.codex.enabled");
    }

    /// Stepping in is the Providers' own: a Setting's row on any other tab has
    /// no Settings of its own to step into, and stepping out of nothing leaves
    /// the panel as it was.
    #[test]
    fn only_a_provider_is_stepped_into() {
        let mut panel = SettingsPanel::default();
        panel.open();
        let before = editing(&panel);
        panel.leave_provider();
        assert_eq!(editing(&panel), before);
        for _ in SCHEMA
            .iter()
            .filter(|descriptor| descriptor.group == SettingGroup::General)
        {
            let _ = panel.open_row();
            assert_eq!(panel.keys(), PanelKeys::Rows);
            panel.select_next();
        }
    }

    /// A pointer reaches every list the frame drew and nothing beside it: a
    /// row of the Providers takes the keys back to them, and one of the
    /// focused Provider's own Settings takes them in onto it.
    #[test]
    fn a_pointer_focuses_a_provider_or_one_of_its_settings_and_never_edits() {
        let mut panel = on_the_providers();
        let settings = EffectiveSettings::default();
        panel.record_layout(PanelLayout::new(
            0..100,
            None,
            Some(RowWindow {
                top: 2,
                first: 0,
                count: 3,
                columns: 0..50,
            }),
            Some(RowWindow {
                top: 6,
                first: 0,
                count: 3,
                columns: 51..100,
            }),
        ));

        assert_eq!(panel.focus_at(60, 7, &settings), AvailabilityRead::None);
        assert_eq!(panel.keys(), PanelKeys::ProviderSettings);
        assert_eq!(editing(&panel), "provider.codex.approvalPolicy");

        for (column, row) in [(60, 9), (60, 5), (50, 7), (100, 7)] {
            let _ = panel.focus_at(column, row, &settings);
            assert_eq!(
                editing(&panel),
                "provider.codex.approvalPolicy",
                "a pointer at ({column}, {row}) lands on nothing"
            );
        }

        let _ = panel.focus_at(10, 3, &settings);
        assert_eq!(panel.keys(), PanelKeys::Rows);
        assert_eq!(editing(&panel), "provider.copilot.enabled");
        let _ = panel.open_row();
        assert_eq!(
            editing(&panel),
            "provider.copilot.permissions",
            "pointing at another Provider starts its Settings afresh"
        );
        let _ = panel.focus_at(10, 3, &settings);
        assert_eq!(
            panel.keys(),
            PanelKeys::Rows,
            "pointing at the Provider already focused takes the keys back to the list"
        );
    }

    /// The Providers tab is the one tab a Setting's key decides: it is built
    /// from the Providers themselves, so a Provider Setting grouped elsewhere
    /// would vanish from under the Provider it configures, and a Setting
    /// configuring no Provider grouped there would surface under one it has
    /// nothing to do with. Which of the remaining tabs a Setting lands on is
    /// the schema's own call, which is what makes moving one a one-line
    /// change.
    #[test]
    fn a_setting_is_grouped_with_the_providers_exactly_when_it_configures_one() {
        for descriptor in SCHEMA {
            let configures_a_provider = descriptor.key.starts_with("provider.");
            assert_eq!(
                descriptor.group == SettingGroup::Providers,
                configures_a_provider,
                "{} is grouped as {:?} while it configures {}",
                descriptor.key,
                descriptor.group,
                if configures_a_provider {
                    "a Provider"
                } else {
                    "no Provider"
                }
            );
        }
    }
}
