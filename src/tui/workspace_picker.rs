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
};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface,
    commands::{SemanticCommandId, SemanticInvocation},
    fuzzy::fuzzy_matches,
    list_window::ListWindow,
    session_listing::SessionListing,
    sidebar::workspace_name,
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
    /// The Workspace the reader is on, held by identity rather than as a row
    /// number so a listing landing beneath them leaves them on the Workspace
    /// they were choosing rather than on whatever now stands in its place.
    selected: Option<WorkspaceId>,
    /// Why the selected Workspace could not be read when the reader chose it,
    /// or why a Description the reader saved never landed. It belongs to the
    /// picker rather than to the listing: the row is still true of past work
    /// even when its directory has since disappeared.
    refusal: Option<String>,
    /// One row's own context menu, opened by a right press on it: editing
    /// that Workspace's Description always, and choosing its Icon while
    /// `appearance.showIcons` is on (see [`Self::open_menu_at`]). Once open it
    /// has the keys, and a press inside its box acts on the item drawn there
    /// (see [`Self::menu_hit`]); a press missing the box entirely never
    /// reaches this far; see `SemanticCommandId::PointerClick`.
    menu: Option<WorkspacePickerMenu>,
    /// The Description being written for one Workspace, while the reader
    /// edits it. It stands over the picker and its menu, and saving or
    /// cancelling it leaves the reader back on the picker.
    description_editor: Option<DescriptionEditor>,
    /// Where the last frame drew each row, recorded at draw time and resolved
    /// against a right press the same way [`super::icon_picker::IconPicker`]
    /// records its own cells.
    row_geometry: RefCell<Vec<WorkspacePickerRowGeometry>>,
    /// Where the last frame drew the row menu's items, so a press inside its
    /// box resolves to the item drawn under it.
    menu_geometry: RefCell<Option<WorkspacePickerMenuGeometry>>,
    window: ListWindow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspacePickerRow {
    /// The Workspace by the name a row gives it, which is the directory a
    /// reader thinks of the work as being in.
    pub(super) name: String,
    /// Where it stands, spelled in full, so two Workspaces named alike stay
    /// told apart.
    pub(super) path: PathBuf,
    pub(super) selected: bool,
    /// Whether this is the Workspace the client is working in, which is the
    /// one the picker stands first.
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
/// describes, by Origin and identity, the name it is drawn by, and what the
/// reader has written so far — seeded with the Description it carries.
#[derive(Clone, Debug)]
struct DescriptionEditor {
    origin: Outlook,
    workspace_id: WorkspaceId,
    name: String,
    text: String,
}

/// The Description editor as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct DescriptionEditorView<'a> {
    pub(super) name: &'a str,
    pub(super) text: &'a str,
}

/// A Description the reader saved, bound for its Workspace's own Origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DescriptionEdit {
    pub(super) origin: Outlook,
    pub(super) workspace_id: WorkspaceId,
    pub(super) text: String,
}

#[derive(Clone, Debug)]
struct WorkspacePickerRowGeometry {
    row: u16,
    columns: Range<u16>,
    workspace_id: WorkspaceId,
    /// The row's own Origin, carried from [`WorkspacePickerRow::origin`]
    /// rather than re-read off the listing at hit-testing time, so a right
    /// press always names the Origin the row it landed on was actually drawn
    /// for.
    origin: Outlook,
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
            refusal: None,
            menu: None,
            description_editor: None,
            row_geometry: RefCell::new(Vec::new()),
            menu_geometry: RefCell::new(None),
            window: ListWindow::default(),
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.query.clear();
        self.selected = None;
        self.refusal = None;
        self.menu = None;
        self.description_editor = None;
        self.window.open();
        self.listing.clear_error();
        self.listing.refresh()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = None;
        self.refusal = None;
        self.menu = None;
        self.description_editor = None;
        self.listing.clear();
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

    pub(super) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
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
        self.refusal = None;
        self.keep_selection_offered();
        self.window.open();
    }

    /// Gives the last character of the query back, widening the list again.
    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.refusal = None;
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
        self.paths
            .as_ref()
            .map_or_else(|| workspace_name(path), |paths| paths.name(path))
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

    /// The Workspace the row the reader is on names, which is the one choosing
    /// takes. There is none while the listing is on its way: no row is marked,
    /// so Enter names nothing rather than naming whatever would stand first.
    pub(super) fn offer_selected(&mut self) -> Option<crate::protocol::Workspace> {
        self.selected_workspace()
    }

    pub(super) fn fail_resolution(&mut self, error: String) {
        self.refusal = Some(error);
    }

    /// Takes the reason a saved Description never landed, said in the
    /// footer where the reader who saved it is still looking.
    pub(super) fn fail_description(&mut self, error: String) {
        self.refusal = Some(error);
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
        self.offered()
            .into_iter()
            .map(|workspace| WorkspacePickerRow {
                name: if workspace.main_unknown() {
                    format!("{} (main checkout unknown)", self.name(&workspace.path))
                } else {
                    self.name(&workspace.path)
                },
                current: workspace.id == current.id,
                selected: self.selected.as_ref() == Some(&workspace.id),
                icon: workspace
                    .icon
                    .as_deref()
                    .and_then(crate::icon_catalog::glyph),
                path: workspace.path,
                workspace_id: workspace.id,
                origin: origin.clone(),
                description: workspace.description.map(|description| description.text),
            })
            .collect()
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
        let selected = self.selected.as_ref()?;
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
    fn offered(&self) -> Vec<crate::protocol::Workspace> {
        let current = self.listing.current_workspace().to_owned();
        let mut offered = self
            .listing
            .workspaces()
            .into_iter()
            .filter(|path| fuzzy_matches(&self.query, &self.name(&path.path)))
            .collect::<Vec<_>>();
        // A stable sort on "is this not where I am", so the current Workspace
        // takes the first row and the rest keep the order the listing derived
        // them in, which is already newest work first.
        offered.sort_by_key(|path| path.id != current.id);
        offered
    }

    fn move_selection(&mut self, distance: isize) {
        self.refusal = None;
        let offered = self.offered();
        if offered.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| offered.iter().position(|path| &path.id == selected))
            .unwrap_or(0);
        let length = offered.len() as isize;
        let next = (current as isize + distance).rem_euclid(length) as usize;
        self.selected = Some(offered[next].id.clone());
        self.window.reveal();
    }

    /// Puts the reader on a row that is still offered: the one they were on
    /// where it stands, and the first row otherwise — which is the current
    /// Workspace until a query takes it away, and no row at all when a query
    /// leaves none.
    fn keep_selection_offered(&mut self) {
        let offered = self.offered();
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| offered.iter().any(|workspace| &workspace.id == selected))
        {
            return;
        }
        self.selected = offered.first().map(|workspace| workspace.id.clone());
    }

    /// Opens a row's own context menu at `position`, naming the Workspace the
    /// row it landed on stands for. It offers editing that Workspace's
    /// Description, and choosing its Icon while Icons are shown. A press
    /// outside every row opens nothing, leaving whatever menu already stood
    /// there put away regardless.
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
        self.refusal = None;
        self.description_editor = Some(DescriptionEditor {
            origin,
            workspace_id,
            name: self.name(&workspace.path),
            text: workspace
                .description
                .map(|description| description.text)
                .unwrap_or_default(),
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
            })
    }

    /// Takes typed or pasted text into the Description, kept on one line —
    /// a line break pasted in reads as the space between two words — and
    /// stopped at the most a Description may run to, so what is saved is
    /// never refused for its length.
    pub(super) fn insert_description(&mut self, text: &str) {
        let Some(editor) = &mut self.description_editor else {
            return;
        };
        let room = MAX_WORKSPACE_DESCRIPTION_CHARS.saturating_sub(editor.text.chars().count());
        editor.text.extend(
            text.chars()
                .map(|character| {
                    if character.is_whitespace() {
                        ' '
                    } else {
                        character
                    }
                })
                .filter(|character| !character.is_control())
                .take(room),
        );
    }

    pub(super) fn delete_description_backward(&mut self) {
        if let Some(editor) = &mut self.description_editor {
            editor.text.pop();
        }
    }

    /// Empties the Description, which saved as it stands clears it, so Suru
    /// may derive one again.
    pub(super) fn clear_description(&mut self) {
        if let Some(editor) = &mut self.description_editor {
            editor.text.clear();
        }
    }

    /// Closes the editor and answers with what the reader saved, bound for
    /// the Workspace's own Origin.
    pub(super) fn save_description(&mut self) -> Option<DescriptionEdit> {
        let editor = self.description_editor.take()?;
        Some(DescriptionEdit {
            origin: editor.origin,
            workspace_id: editor.workspace_id,
            text: editor.text.trim().to_owned(),
        })
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

    /// Records where the frame in force drew one row, so a right press over
    /// it can open that row's own menu.
    pub(super) fn record_row(
        &self,
        row: u16,
        columns: Range<u16>,
        workspace_id: WorkspaceId,
        origin: Outlook,
    ) {
        self.row_geometry
            .borrow_mut()
            .push(WorkspacePickerRowGeometry {
                row,
                columns,
                workspace_id,
                origin,
            });
    }

    fn hit_row(&self, position: Position) -> Option<(Outlook, WorkspaceId)> {
        self.row_geometry
            .borrow()
            .iter()
            .find(|cell| cell.row == position.y && cell.columns.contains(&position.x))
            .map(|cell| (cell.origin.clone(), cell.workspace_id.clone()))
    }
}
