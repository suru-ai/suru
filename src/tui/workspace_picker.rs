//! Workspace Picker state: the Workspaces a Session listing puts on offer,
//! ordered for choosing and narrowed by what the reader types.

use std::{
    cell::RefCell,
    ops::Range,
    path::{Path, PathBuf},
};

use ratatui::layout::Position;

use crate::protocol::{
    MAX_WORKSPACE_DESCRIPTION_CHARS, Outlook, SessionListItem, WorkspaceId, WorkspacePaths,
    description_too_long, one_line_description,
};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface,
    commands::{SemanticCommandId, SemanticInvocation},
    fuzzy::fuzzy_matches,
    list_window::ListWindow,
    session_listing::SessionListing,
};

#[derive(Clone, Debug)]
pub(super) struct WorkspacePicker {
    paths: Option<WorkspacePaths>,
    open: bool,
    /// The Sessions the offered Workspaces are derived from, and the
    /// conversation with the server that keeps them true. It asks across every
    /// Workspace however narrow another surface's scope, because the
    /// Workspaces the reader is not in are the whole point of the picker.
    listing: SessionListing,
    /// What the reader has typed to narrow the Workspaces on offer, read
    /// against each one's name the way the session picker reads its own query
    /// against a Title.
    query: String,
    /// The row the reader is on, held by what it offers rather than as a row
    /// number so a listing landing beneath them leaves them on the Workspace
    /// they were choosing rather than on whatever now stands in its place.
    selected: Option<Offer>,
    /// Whether the picker has stepped aside for a Directory Browser opened
    /// from it, drawing nothing and taking no keys while it keeps its query,
    /// the row the reader was on, its scroll, and the listing it drew them
    /// from, so Esc from the browser brings it back as the reader left it
    /// (see [`Self::step_aside`] and [`Self::step_back`]).
    stepped_aside: bool,
    /// Why what the reader asked of the picker was refused before it reached
    /// the Server, said inside the picker because the picker stands over the
    /// Landing where a refusal is otherwise said.
    refusal: Option<String>,
    /// One row's own context menu, opened by a right press on it: editing
    /// that Workspace's Description always, and choosing its Icon while
    /// `appearance.showIcons` is on (see [`Self::open_menu_at`]). Once open it
    /// has the keys, and a press inside its box acts on the item drawn there
    /// (see [`Self::menu_hit`]); a press missing the box entirely never
    /// reaches this far; see `SemanticCommandId::PointerClick`.
    menu: Option<WorkspacePickerMenu>,
    /// The Description being written for one Workspace, while the reader
    /// edits it. It stands over the picker and its menu until its save lands
    /// or the reader cancels it, leaving them back on the picker.
    description_editor: Option<DescriptionEditor>,
    /// The draft last sent to a Workspace's Server, held until that Server
    /// answers, so a save that does not land gives the reader back what they
    /// wrote even after they closed the editor on it.
    submitted_description: Option<DescriptionEditor>,
    /// Where the last frame drew each row, recorded at draw time and resolved
    /// against a press the same way [`super::icon_picker::IconPicker`]
    /// records its own cells: a left press chooses the row (see
    /// [`Self::row_hit`]) and a right one opens its menu.
    row_geometry: RefCell<Vec<WorkspacePickerRowGeometry>>,
    /// Where the last frame drew the row menu's items, so a press inside its
    /// box resolves to the item drawn under it.
    menu_geometry: RefCell<Option<WorkspacePickerMenuGeometry>>,
    window: ListWindow,
}

/// What a Workspace Picker row offers, which is how the picker holds the row
/// the reader is on.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Offer {
    /// The Browse row, which opens the Directory Browser.
    Browse,
    /// A Workspace row, by the Workspace's own identity.
    Workspace(WorkspaceId),
}

/// What choosing the row the reader is on does.
#[derive(Clone, Debug)]
pub(super) enum WorkspacePickerChoice {
    /// Opens the Directory Browser, from the Browse row.
    Browse,
    /// Lands in the Workspace the row names.
    Workspace(crate::protocol::Workspace),
}

/// One row of the picker as a frame draws it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspacePickerRow {
    pub(super) selected: bool,
    pub(super) kind: WorkspacePickerRowKind,
}

/// What a row stands for, which decides how it is drawn and what choosing it
/// does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkspacePickerRowKind {
    /// The picker's way out to a directory it does not list, standing above
    /// the Workspace rows while no query is typed.
    Browse,
    Workspace(WorkspaceRow),
}

/// What a Workspace row draws of the Workspace it names, and what a press on
/// it acts on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceRow {
    /// The Workspace by the name a row gives it, which is the directory a
    /// reader thinks of the work as being in.
    pub(super) name: String,
    /// Where it stands, spelled in full, so two Workspaces named alike stay
    /// told apart.
    pub(super) path: PathBuf,
    /// Whether this is the Workspace the client is working in, which is the
    /// one the picker stands first among its Workspaces.
    pub(super) current: bool,
    /// The Workspace's Icon, resolved to a glyph already, where the listing
    /// carries one and the reader keeps Icons on. `None` draws the plain
    /// folder glyph in its place.
    pub(super) icon: Option<char>,
    /// The Workspace's own identity, carried so a context menu opened on this
    /// row can name it precisely rather than re-deriving it from a name or
    /// path a query may already have narrowed away.
    pub(super) workspace_id: WorkspaceId,
    /// The Origin this Workspace stands on, carried the same way a listed
    /// Session's own reference carries it, so a chosen Icon or a written
    /// Description for a Remote Workspace routes to that Workspace's own
    /// Server rather than always this Client's local one.
    pub(super) origin: Outlook,
    /// The Workspace's Description, as the listing carries it — `None` for
    /// one with none, which the picker draws as readily as one with.
    pub(super) description: Option<String>,
}

/// One Workspace Picker row's own context menu: the target it names, what it
/// offers, which of that the reader is on, and where the reader opened it.
#[derive(Clone, Debug)]
struct WorkspacePickerMenu {
    origin: Outlook,
    workspace_id: WorkspaceId,
    items: Vec<WorkspacePickerMenuItem>,
    selected: usize,
    anchor: Position,
}

/// What a Workspace Picker row's menu offers, top to bottom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspacePickerMenuItem {
    /// Offered only while Icons are shown, since a glyph one cannot see is
    /// not one to choose.
    ChooseIcon,
    /// Always offered: a Description is words, which no Setting hides.
    EditDescription,
}

impl WorkspacePickerMenuItem {
    const fn label(self) -> &'static str {
        match self {
            Self::ChooseIcon => "Choose icon",
            Self::EditDescription => "Edit description",
        }
    }

    const fn command(self) -> SemanticCommandId {
        match self {
            Self::ChooseIcon => SemanticCommandId::WorkspaceIconChoose,
            Self::EditDescription => SemanticCommandId::WorkspaceDescriptionEdit,
        }
    }
}

/// The Workspace Picker row menu as a frame draws it: where it is anchored
/// and what each of its items says.
#[derive(Clone, Debug)]
pub(super) struct WorkspacePickerMenuView {
    pub(super) anchor: Position,
    pub(super) items: Vec<WorkspacePickerMenuEntry>,
}

/// One row menu item as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct WorkspacePickerMenuEntry {
    pub(super) label: &'static str,
    pub(super) selected: bool,
}

/// Where a frame drew the row menu's items: the columns its box holds, the
/// screen row of its first item, and how many items stand beneath it.
#[derive(Clone, Debug)]
pub(super) struct WorkspacePickerMenuGeometry {
    pub(super) columns: Range<u16>,
    pub(super) top: u16,
    pub(super) count: u16,
}

/// The Description a reader is writing for one Workspace: the Workspace it
/// describes, by Origin, identity, and where it is presented, the name it is
/// drawn by, what the reader has written so far — seeded with the
/// Description it carries — whether a save of it is on its way to the
/// Workspace's Server, and why the last one did not land.
#[derive(Clone, Debug)]
struct DescriptionEditor {
    origin: Outlook,
    workspace_id: WorkspaceId,
    path: PathBuf,
    name: String,
    text: String,
    saving: bool,
    error: Option<String>,
}

impl DescriptionEditor {
    fn is_for(&self, origin: &Outlook, workspace_id: &WorkspaceId) -> bool {
        &self.origin == origin && &self.workspace_id == workspace_id
    }
}

/// The Description editor as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct DescriptionEditorView<'a> {
    pub(super) name: &'a str,
    pub(super) text: &'a str,
    /// How long the Description is as it will be kept, which is what the
    /// limit is measured against.
    pub(super) kept_chars: usize,
    pub(super) saving: bool,
    pub(super) error: Option<&'a str>,
}

/// A Description the reader saved, bound for its Workspace's own Origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DescriptionEdit {
    pub(super) origin: Outlook,
    pub(super) workspace_id: WorkspaceId,
    pub(super) path: PathBuf,
    pub(super) text: String,
}

#[derive(Clone, Debug)]
struct WorkspacePickerRowGeometry {
    row: u16,
    columns: Range<u16>,
    target: RowTarget,
}

/// What a row drawn in the last frame stands for, as a press on it resolves.
#[derive(Clone, Debug)]
enum RowTarget {
    Browse,
    /// A Workspace, by its identity and by its own Origin, carried from
    /// [`WorkspaceRow::origin`] rather than re-read off the listing at
    /// hit-testing time, so a right press always names the Origin the row it
    /// landed on was actually drawn for.
    Workspace {
        origin: Outlook,
        workspace_id: WorkspaceId,
    },
}

impl RowTarget {
    fn offer(&self) -> Offer {
        match self {
            Self::Browse => Offer::Browse,
            Self::Workspace { workspace_id, .. } => Offer::Workspace(workspace_id.clone()),
        }
    }
}

impl WorkspacePicker {
    pub(super) fn new(current_workspace: impl Into<crate::protocol::Workspace>) -> Self {
        Self {
            paths: None,
            open: false,
            listing: SessionListing::scoped(
                SessionListSurface::WorkspacePicker,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            query: String::new(),
            selected: None,
            stepped_aside: false,
            refusal: None,
            menu: None,
            description_editor: None,
            submitted_description: None,
            row_geometry: RefCell::new(Vec::new()),
            menu_geometry: RefCell::new(None),
            window: ListWindow::default(),
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.stepped_aside = false;
        self.refusal = None;
        self.query.clear();
        self.selected = None;
        self.menu = None;
        self.description_editor = None;
        self.submitted_description = None;
        self.window.open();
        self.listing.clear_error();
        self.listing.refresh()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.stepped_aside = false;
        self.refusal = None;
        self.query.clear();
        self.selected = None;
        self.menu = None;
        self.description_editor = None;
        self.submitted_description = None;
        self.listing.clear();
    }

    /// Stands the picker aside for the Directory Browser opened from it: it
    /// draws nothing and takes no keys, but keeps everything the reader left
    /// it with for [`Self::step_back`]. Only its row menu is put away, being
    /// no part of where the reader was, and any refusal it was saying, which
    /// a browser that opened has answered.
    pub(super) fn step_aside(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.stepped_aside = true;
        self.menu = None;
        self.refusal = None;
    }

    /// Brings back a picker standing aside for the Directory Browser, as the
    /// reader left it; one that has since closed stays closed.
    pub(super) fn step_back(&mut self) {
        if std::mem::take(&mut self.stepped_aside) {
            self.open = true;
        }
    }

    pub(super) const fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    /// Why what the reader last asked of the picker was refused before it
    /// reached the Server.
    pub(super) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// Says `refusal` inside the picker until it closes, leaving its query
    /// and the row the reader is on as they were.
    pub(super) fn refuse(&mut self, refusal: String) {
        self.refusal = Some(refusal);
    }

    /// Takes what the reader typed into the query, and leaves them on a
    /// Workspace the narrowed list still offers.
    ///
    /// Where the session picker sends the reader back to its first row at
    /// every keystroke, a Workspace they are already on survives their typing:
    /// a row here is a place rather than a Session, and a reader narrowing
    /// towards one they can already see should not be walked away from it.
    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.keep_selection_offered();
        self.window.open();
    }

    /// Gives the last character of the query back, widening the list again.
    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.keep_selection_offered();
        self.window.open();
    }

    /// Takes the Workspace this client has moved to, so the picker marks as
    /// current — and stands first — where the reader now is.
    pub(super) fn adopt_workspace(&mut self, workspace: impl Into<crate::protocol::Workspace>) {
        self.listing.adopt_current_workspace(workspace);
    }

    pub(super) fn adopt_workspace_paths(&mut self, paths: WorkspacePaths) {
        self.paths = Some(paths);
        self.keep_selection_offered();
    }

    fn name(&self, path: &Path) -> String {
        self.paths().name(path)
    }

    /// The Server's own path syntax, or this Client's while the Server has
    /// yet to say what its is.
    fn paths(&self) -> WorkspacePaths {
        self.paths.clone().unwrap_or_default()
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.paths = None;
        self.listing.adopt_outlook(outlook);
        self.close();
    }

    /// Takes a listing the server answered with, leaving the reader on the
    /// Workspace they were on where it is still offered and on the first row
    /// — the current Workspace — otherwise.
    pub(super) fn load(&mut self, request: &SessionListRequest, sessions: Vec<SessionListItem>) {
        if !self.listing.load(request, sessions) {
            return;
        }
        self.keep_selection_offered();
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        self.listing.fail(request, error);
    }

    pub(super) fn set_workspace_icon_origin(
        &mut self,
        outlook: Outlook,
        workspace_id: &crate::protocol::WorkspaceId,
        icon: Option<String>,
    ) {
        self.listing
            .set_workspace_icon_origin(outlook, workspace_id, icon);
    }

    pub(super) fn set_workspace_description_origin(
        &mut self,
        outlook: Outlook,
        workspace_id: &crate::protocol::WorkspaceId,
        description: Option<crate::protocol::WorkspaceDescription>,
    ) {
        self.listing
            .set_workspace_description_origin(outlook, workspace_id, description);
    }

    /// The Outlook's Workspaces the picker's listing has heard of from its
    /// Server: every one its Sessions are rooted in, for as long as it holds
    /// that listing — open, or standing aside for the Directory Browser.
    pub(super) fn listed_workspaces(&self) -> Vec<crate::protocol::Workspace> {
        self.listing.listed_workspaces()
    }

    /// Whether a listing the server answered with would move anything the
    /// picker draws.
    ///
    /// It is answered off the Sessions rather than off the Workspaces derived
    /// from them, which can only over-report — and cannot here: the picker
    /// asks for one listing, when it opens, and is loading until that listing
    /// lands, so the one reply it ever awaits always has a loading line to
    /// replace.
    pub(super) fn would_move(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        self.listing.would_move(request, sessions)
    }

    /// Whether a reply the server sent answers the listing this surface is
    /// still waiting for.
    pub(super) fn awaits_listing(&self, request: &SessionListRequest) -> bool {
        self.listing.awaits(request)
    }

    /// What choosing the row the reader is on does: open the Directory
    /// Browser from the Browse row, or land in the Workspace a Workspace row
    /// names. There is nothing to choose while the listing is on its way: no
    /// row is marked, so Enter names nothing rather than naming whatever
    /// would stand first.
    pub(super) fn offer_selected(&self) -> Option<WorkspacePickerChoice> {
        match self.selected.as_ref()? {
            Offer::Browse => self
                .offers_browse()
                .then_some(WorkspacePickerChoice::Browse),
            Offer::Workspace(_) => self
                .selected_workspace()
                .map(WorkspacePickerChoice::Workspace),
        }
    }

    /// Whether the row the reader is on is the Browse row.
    pub(super) fn browse_is_selected(&self) -> bool {
        self.offers_browse() && self.selected == Some(Offer::Browse)
    }

    pub(super) fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub(super) fn page_previous(&mut self) {
        self.move_selection(-10);
    }

    pub(super) fn page_next(&mut self) {
        self.move_selection(10);
    }

    fn rows(&self) -> Vec<WorkspacePickerRow> {
        let current = self.listing.current_workspace().to_owned();
        let origin = self.listing.outlook().clone();
        let browse = self.offers_browse().then(|| WorkspacePickerRow {
            selected: self.selected == Some(Offer::Browse),
            kind: WorkspacePickerRowKind::Browse,
        });
        let selected_workspace = match &self.selected {
            Some(Offer::Workspace(selected)) => Some(selected),
            _ => None,
        };
        let workspaces = self
            .offered()
            .into_iter()
            .map(|workspace| WorkspacePickerRow {
                selected: selected_workspace == Some(&workspace.id),
                kind: WorkspacePickerRowKind::Workspace(WorkspaceRow {
                    name: self.paths().workspace_name(&workspace),
                    current: workspace.id == current.id,
                    icon: workspace
                        .icon
                        .as_deref()
                        .and_then(crate::icon_catalog::glyph),
                    path: workspace.path,
                    workspace_id: workspace.id,
                    origin: origin.clone(),
                    description: workspace.description.map(|description| description.text),
                }),
            });
        browse.into_iter().chain(workspaces).collect()
    }

    /// The Description of the Workspace the reader is on, which is the one
    /// the picker draws beneath its rows — `None` where it carries none, or
    /// where no row is the reader's.
    pub(super) fn selected_description(&self) -> Option<String> {
        self.selected_workspace()?
            .description
            .map(|description| description.text)
    }

    /// The Workspace the row the reader is on names, by Origin and identity,
    /// which is what a key that acts on "this row" acts on.
    pub(super) fn selected_target(&self) -> Option<(Outlook, WorkspaceId)> {
        let workspace = self.selected_workspace()?;
        Some((self.listing.outlook().clone(), workspace.id))
    }

    fn selected_workspace(&self) -> Option<crate::protocol::Workspace> {
        let Some(Offer::Workspace(selected)) = &self.selected else {
            return None;
        };
        self.offered()
            .into_iter()
            .find(|workspace| &workspace.id == selected)
    }

    /// The rows a picker `capacity` rows tall shows, wound on far enough to
    /// keep the row the reader is on in view.
    pub(super) fn visible_rows(&self, capacity: usize) -> Vec<WorkspacePickerRow> {
        let rows = self.rows();
        let selected = rows.iter().position(|row| row.selected);
        self.window.show(rows, capacity, selected).collect()
    }

    /// The Workspaces on offer in the order the picker stands them: the one
    /// the client is working in first, because a reader has to see where they
    /// already are, then the rest by which held work most recently, because
    /// that is where the next pick is likeliest to go.
    ///
    /// A query takes rows away and never rearranges the ones it leaves, so a
    /// reader narrowing the list goes on reading it in the order they learned
    /// it in.
    ///
    /// The Sidekick Workspace is offered only while the reader is in it: it
    /// holds no body of work to choose among, and `/sidekick` is the way into
    /// it, but a reader in it still sees where they are and may change its
    /// Icon and Description.
    fn offered(&self) -> Vec<crate::protocol::Workspace> {
        let current = self.listing.current_workspace().to_owned();
        let paths = self.paths();
        let mut offered = self
            .listing
            .workspaces()
            .into_iter()
            .filter(|path| path.id == current.id || !paths.is_sidekick_workspace(&path.path))
            .filter(|path| fuzzy_matches(&self.query, &self.name(&path.path)))
            .collect::<Vec<_>>();
        // A stable sort on "is this not where I am", so the current Workspace
        // takes the first row and the rest keep the order the listing derived
        // them in, which is already newest work first.
        offered.sort_by_key(|path| path.id != current.id);
        offered
    }

    /// Whether the Browse row stands above the Workspaces, which it does only
    /// while no query is typed, so search results are only Workspaces.
    fn offers_browse(&self) -> bool {
        self.query.is_empty()
    }

    /// Every row on offer, top to bottom: the Browse row where it stands, then
    /// the Workspaces in the order [`Self::offered`] stands them.
    fn offers(&self) -> Vec<Offer> {
        self.offers_browse()
            .then_some(Offer::Browse)
            .into_iter()
            .chain(
                self.offered()
                    .into_iter()
                    .map(|workspace| Offer::Workspace(workspace.id)),
            )
            .collect()
    }

    fn move_selection(&mut self, distance: isize) {
        let mut offers = self.offers();
        if offers.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| offers.iter().position(|offer| offer == selected))
            .unwrap_or(0);
        let length = offers.len() as isize;
        let next = (current as isize + distance).rem_euclid(length) as usize;
        self.selected = Some(offers.swap_remove(next));
        self.window.reveal();
    }

    /// Puts the reader on a row that is still offered: the one they were on
    /// where it stands, and the first Workspace row otherwise — which is the
    /// current Workspace until a query takes it away, and no row at all when
    /// a query leaves none. The Browse row is somewhere the reader walks to
    /// and never where they are put, so the picker opens on the Workspace
    /// they are in and Enter goes on choosing it.
    fn keep_selection_offered(&mut self) {
        let offers = self.offers();
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| offers.contains(selected))
        {
            return;
        }
        self.selected = offers
            .into_iter()
            .find(|offer| matches!(offer, Offer::Workspace(_)));
    }

    /// Opens a row's own context menu at `position`, naming the Workspace the
    /// row it landed on stands for. It offers editing that Workspace's
    /// Description, and choosing its Icon while Icons are shown. A press
    /// outside every Workspace row — the Browse row stands for none — opens
    /// nothing, leaving whatever menu already stood there put away
    /// regardless.
    pub(super) fn open_menu_at(&mut self, position: Position, show_icons: bool) {
        self.menu = None;
        let Some((origin, workspace_id)) = self.hit_row(position) else {
            return;
        };
        let items = show_icons
            .then_some(WorkspacePickerMenuItem::ChooseIcon)
            .into_iter()
            .chain([WorkspacePickerMenuItem::EditDescription])
            .collect();
        self.menu = Some(WorkspacePickerMenu {
            origin,
            workspace_id,
            items,
            selected: 0,
            anchor: position,
        });
    }

    pub(super) fn menu_is_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Puts the row menu away, leaving the row it stood on alone.
    pub(super) fn close_menu(&mut self) {
        self.menu = None;
    }

    /// The row menu as a frame draws it, and `None` while none stands open.
    pub(super) fn menu(&self) -> Option<WorkspacePickerMenuView> {
        self.menu.as_ref().map(|menu| WorkspacePickerMenuView {
            anchor: menu.anchor,
            items: menu
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| WorkspacePickerMenuEntry {
                    label: item.label(),
                    selected: index == menu.selected,
                })
                .collect(),
        })
    }

    pub(super) fn menu_select_previous(&mut self) {
        self.move_menu_selection(-1);
    }

    pub(super) fn menu_select_next(&mut self) {
        self.move_menu_selection(1);
    }

    fn move_menu_selection(&mut self, distance: isize) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        let length = menu.items.len() as isize;
        if length == 0 {
            return;
        }
        menu.selected = (menu.selected as isize + distance).rem_euclid(length) as usize;
    }

    /// The invocation the row menu's selected item asks for, or `None` while
    /// no menu stands open. Closing the menu is left to the caller, the same
    /// way [`super::sidebar::Sidebar::activate_menu_item`] leaves it.
    pub(super) fn activate_menu(&self) -> Option<SemanticInvocation> {
        let menu = self.menu.as_ref()?;
        let item = menu.items.get(menu.selected)?;
        Some(
            item.command()
                .on_workspace(menu.origin.clone(), menu.workspace_id.clone()),
        )
    }

    /// Puts the reader on the menu item drawn at `position`, answering
    /// whether one is drawn there — so a press acts on the item it landed
    /// on, through the same path Enter takes.
    pub(super) fn menu_hit(&mut self, position: Position) -> bool {
        let hit = self.menu_geometry.borrow().as_ref().and_then(|geometry| {
            if !geometry.columns.contains(&position.x) {
                return None;
            }
            let offset = position.y.checked_sub(geometry.top)?;
            (offset < geometry.count).then_some(usize::from(offset))
        });
        match (hit, &mut self.menu) {
            (Some(index), Some(menu)) if index < menu.items.len() => {
                menu.selected = index;
                true
            }
            _ => false,
        }
    }

    /// Records where the frame in force drew the row menu's items.
    pub(super) fn record_menu_geometry(&self, geometry: WorkspacePickerMenuGeometry) {
        *self.menu_geometry.borrow_mut() = Some(geometry);
    }

    /// Opens the Description editor over one Workspace the picker offers,
    /// seeded with the Description it carries. A Workspace the picker does
    /// not offer — none it lists, or one a query has narrowed away — opens
    /// nothing, since the editor names the Workspace it describes by the row
    /// that stands for it.
    pub(super) fn open_description_editor(&mut self, origin: Outlook, workspace_id: WorkspaceId) {
        if !self.open {
            return;
        }
        let Some(workspace) = self
            .offered()
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return;
        };
        self.menu = None;
        self.description_editor = Some(DescriptionEditor {
            origin,
            workspace_id,
            name: self.name(&workspace.path),
            path: workspace.path,
            text: workspace
                .description
                .map(|description| description.text)
                .unwrap_or_default(),
            saving: false,
            error: None,
        });
    }

    pub(super) fn description_editor_is_open(&self) -> bool {
        self.description_editor.is_some()
    }

    /// The Description editor as a frame draws it, and `None` while none
    /// stands open.
    pub(super) fn description_editor(&self) -> Option<DescriptionEditorView<'_>> {
        self.description_editor
            .as_ref()
            .map(|editor| DescriptionEditorView {
                name: &editor.name,
                text: &editor.text,
                kept_chars: one_line_description(&editor.text).chars().count(),
                saving: editor.saving,
                error: editor.error.as_deref(),
            })
    }

    /// The editor the reader may write in: one stands open, and no save of
    /// it is on its way.
    fn writable_description(&mut self) -> Option<&mut DescriptionEditor> {
        self.description_editor
            .as_mut()
            .filter(|editor| !editor.saving)
    }

    /// Takes typed or pasted text into the Description whole, a line break
    /// written as the space it will be kept as. Nothing is cut: a Description
    /// running past the limit says so as it is written, and saving one is
    /// refused, rather than any of it vanishing unseen.
    pub(super) fn insert_description(&mut self, text: &str) {
        let Some(editor) = self.writable_description() else {
            return;
        };
        editor.error = None;
        editor.text.extend(
            text.chars()
                .map(|character| {
                    if character.is_whitespace() {
                        ' '
                    } else {
                        character
                    }
                })
                .filter(|character| !character.is_control()),
        );
    }

    pub(super) fn delete_description_backward(&mut self) {
        if let Some(editor) = self.writable_description() {
            editor.error = None;
            editor.text.pop();
        }
    }

    /// Empties the Description, which saved as it stands clears it, so Suru
    /// may derive one again.
    pub(super) fn clear_description(&mut self) {
        if let Some(editor) = self.writable_description() {
            editor.error = None;
            editor.text.clear();
        }
    }

    /// Answers with what the reader saved, kept on one line, bound for the
    /// Workspace's own Origin — and holds it, with the editor standing, until
    /// that Server answers (see [`Self::description_saved`] and
    /// [`Self::description_save_failed`]). A Description longer than the
    /// limit as it will be kept is not sent at all: the editor says the limit
    /// instead, in the words its Server would refuse it with.
    pub(super) fn save_description(&mut self) -> Option<DescriptionEdit> {
        let editor = self.writable_description()?;
        let text = one_line_description(&editor.text);
        let kept_chars = text.chars().count();
        if kept_chars > MAX_WORKSPACE_DESCRIPTION_CHARS {
            editor.error = Some(description_too_long(kept_chars));
            return None;
        }
        editor.saving = true;
        editor.error = None;
        let edit = DescriptionEdit {
            origin: editor.origin.clone(),
            workspace_id: editor.workspace_id.clone(),
            path: editor.path.clone(),
            text,
        };
        self.submitted_description = self.description_editor.clone();
        Some(edit)
    }

    /// Takes a Server's word that the Description it was sent landed: the
    /// draft is let go, and an editor still waiting on it closes.
    pub(super) fn description_saved(&mut self, origin: &Outlook, workspace_id: &WorkspaceId) {
        if self
            .submitted_description
            .as_ref()
            .is_some_and(|submitted| submitted.is_for(origin, workspace_id))
        {
            self.submitted_description = None;
        }
        if self
            .description_editor
            .as_ref()
            .is_some_and(|editor| editor.saving && editor.is_for(origin, workspace_id))
        {
            self.description_editor = None;
        }
    }

    /// Takes the reason a Description sent to its Server did not land, and
    /// gives the reader their draft back beside it: in the editor still
    /// waiting on it, or — where they closed that editor while the picker
    /// stands — in one opened again on it. Answers `false` where there is no
    /// draft here to give back, which leaves the reason to the caller.
    pub(super) fn description_save_failed(
        &mut self,
        origin: &Outlook,
        workspace_id: &WorkspaceId,
        error: String,
    ) -> bool {
        let submitted = self
            .submitted_description
            .take_if(|submitted| submitted.is_for(origin, workspace_id));
        if let Some(editor) = self
            .description_editor
            .as_mut()
            .filter(|editor| editor.saving && editor.is_for(origin, workspace_id))
        {
            editor.saving = false;
            editor.error = Some(error);
            return true;
        }
        match submitted {
            Some(mut draft) if self.open && self.description_editor.is_none() => {
                draft.saving = false;
                draft.error = Some(error);
                self.menu = None;
                self.description_editor = Some(draft);
                true
            }
            _ => false,
        }
    }

    pub(super) fn cancel_description(&mut self) {
        self.description_editor = None;
    }

    /// Gives up the last frame's row geometry, called as every frame begins,
    /// so a press resolves only against cells actually on screen.
    pub(super) fn forget_frame(&self) {
        self.row_geometry.borrow_mut().clear();
        *self.menu_geometry.borrow_mut() = None;
    }

    /// Records where the frame in force drew one row, so a left press over it
    /// can choose it and a right press open that row's own menu.
    pub(super) fn record_row(&self, row: u16, columns: Range<u16>, kind: &WorkspacePickerRowKind) {
        let target = match kind {
            WorkspacePickerRowKind::Browse => RowTarget::Browse,
            WorkspacePickerRowKind::Workspace(workspace) => RowTarget::Workspace {
                origin: workspace.origin.clone(),
                workspace_id: workspace.workspace_id.clone(),
            },
        };
        self.row_geometry
            .borrow_mut()
            .push(WorkspacePickerRowGeometry {
                row,
                columns,
                target,
            });
    }

    /// Puts the reader on the row drawn at `position`, answering whether a
    /// row the picker still offers is drawn there — so a press chooses the
    /// row it landed on through the same path Enter takes.
    pub(super) fn row_hit(&mut self, position: Position) -> bool {
        let Some(offer) = self.target_at(position).map(|target| target.offer()) else {
            return false;
        };
        if !self.offers().contains(&offer) {
            return false;
        }
        self.selected = Some(offer);
        true
    }

    /// The Workspace drawn at `position`, by Origin and identity.
    fn hit_row(&self, position: Position) -> Option<(Outlook, WorkspaceId)> {
        match self.target_at(position)? {
            RowTarget::Browse => None,
            RowTarget::Workspace {
                origin,
                workspace_id,
            } => Some((origin, workspace_id)),
        }
    }

    fn target_at(&self, position: Position) -> Option<RowTarget> {
        self.row_geometry
            .borrow()
            .iter()
            .find(|cell| cell.row == position.y && cell.columns.contains(&position.x))
            .map(|cell| cell.target.clone())
    }
}
