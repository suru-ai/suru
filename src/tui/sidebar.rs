//! The Sidebar: the collapsible column beside the main view listing Sessions.

use std::{
    cell::{Cell, RefCell},
    cmp::Reverse,
    ops::Range,
    path::{Path, PathBuf},
};

use ratatui::layout::Position;

use crate::protocol::{
    AutoSettle, SessionId, SessionListItem, SessionTimestamp, SidebarScope, SidebarSettings,
    SidebarVisibility,
};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface,
    commands::{SemanticCommandId, SemanticInvocation},
    session_listing::SessionListing,
};

/// The columns the Sidebar occupies, cloning t3 code's own fixed column. There
/// is no drag-resize and no Setting: the width is part of the clone.
const SIDEBAR_WIDTH: u16 = 32;

/// The narrowest main view the Sidebar will leave behind: the 50 columns a
/// reader is allowed to cap the Session Content Column at (ADR 0012), inside
/// the two columns of padding the frame insets it by. Below that the Sidebar
/// would be buying its own columns out of the conversation.
const MINIMUM_MAIN_WIDTH: u16 = 54;

/// The columns the Sidebar takes from a frame this wide, and `None` where the
/// frame cannot spare them. This is the whole of the squeeze: a terminal too
/// narrow for the Sidebar plus a usable main view keeps the main view, and the
/// reader's own show-or-hide choice is untouched, so widening the terminal
/// brings the Sidebar back exactly as they left it.
pub(super) const fn width_beside(frame_width: u16) -> Option<u16> {
    if frame_width < SIDEBAR_WIDTH + MINIMUM_MAIN_WIDTH {
        None
    } else {
        Some(SIDEBAR_WIDTH)
    }
}

/// What the selector's first entry says, standing for the whole body of work
/// rather than for any one Workspace.
const ALL_WORKSPACES: &str = "All Workspaces";

/// The affordance beside the selector, opening the path entry a reader names a
/// Workspace in. It shares the selector's line, so it is drawn — and pressed —
/// within its own columns of it.
pub(super) const ADD_WORKSPACE: &str = " + ";

/// What the path entry says when the reader offers it nothing.
const NAME_A_DIRECTORY: &str = "Name a directory";

/// What it says of a path nothing stands at.
const NO_DIRECTORY_THERE: &str = "No directory there";

/// What it says of a path standing at something other than a directory, which
/// a Workspace cannot be rooted at.
const NOT_A_DIRECTORY: &str = "Not a directory";

/// The Workspaces the Sidebar draws: every one the reader has work in, or a
/// single one of them.
///
/// This is the reader's own view of their work rather than a question for the
/// server, which is why it is not the [`SessionListScope`] a listing asks with:
/// the Sidebar asks for the whole body of work however narrow the scope,
/// because the selector's entries are read off that listing and would otherwise
/// vanish the moment the reader narrowed to one of them. The two must not be
/// the same type, or a later hand would be free to send this one — and the
/// selector would empty itself the first time it was used. The initial-scope
/// Setting seeds it and the selector moves it; nothing writes it back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceScope {
    AllWorkspaces,
    Workspace(PathBuf),
}

impl WorkspaceScope {
    /// What the selector calls this scope: the Workspace by the name a row
    /// gives it, or the words for all of them.
    fn label(&self) -> String {
        match self {
            Self::AllWorkspaces => ALL_WORKSPACES.to_owned(),
            Self::Workspace(workspace) => workspace_name(workspace),
        }
    }

    /// Whether this Session's work is rooted at the Workspace this scope names.
    /// At it rather than under it: a Workspace is where work is rooted, so a
    /// Session in a directory beneath one belongs to its own Workspace and not
    /// to the one above.
    ///
    /// A Session whose Workspace Suru could not read stands wherever the
    /// listing stands, narrowed or not. This is the reading the server gives a
    /// listing it narrows, and the two must agree: a Session nobody can place
    /// is one narrowing must not be the thing that hides.
    fn holds(&self, session: &SessionListItem) -> bool {
        match self {
            Self::AllWorkspaces => true,
            Self::Workspace(workspace) => session
                .workspace()
                .is_none_or(|rooted| rooted.path == *workspace),
        }
    }
}

/// The Sidebar's own state: whether the reader wants it, where the reader
/// works, and the Sessions it lists.
///
/// Visibility has two independent halves. The reader's choice — seeded once
/// from the initial-visibility Setting and flipped by the toggle — lives here
/// and is never
/// written back to configuration. Whether the frame can actually spare the
/// columns is decided at draw time by [`width_beside`], so a terminal that
/// squeezes the Sidebar out forgets nothing.
#[derive(Clone, Debug)]
pub(super) struct Sidebar {
    revealed: bool,
    /// Whether the reader is driving the Sidebar rather than the composer.
    /// Opening it themselves is what claims the keys; Esc, the toggle, and the
    /// Session they attach hand them back.
    focused: bool,
    /// Whether the Settings that seed the Sidebar have had their say. Only the
    /// first snapshot
    /// seeds: every later one carries some other Setting's edit, and a reader
    /// who toggled the Sidebar since should not have it flipped back under
    /// them.
    seeded: bool,
    /// When a Session settles without anyone saying so. Adopted from every
    /// snapshot rather than seeded from the first, because unlike the two
    /// Settings that seed the Sidebar this one governs what the Sidebar shows for as long as it is
    /// open: editing it reclassifies every listed Session on the next frame.
    auto_settle: AutoSettle,
    /// The Workspaces the Sidebar draws, seeded once from the initial-scope
    /// Setting and moved by the selector afterwards.
    scope: WorkspaceScope,
    /// Whether the selector's entries stand open under it. While they do they
    /// are the list: the reader is choosing a Workspace rather than a Session,
    /// so both shelves stand down as they do under a query.
    selector_open: bool,
    /// The path entry the affordance beside the selector opened, where one is
    /// open. It stands in place of the list for the same reason the selector's
    /// entries do — a reader saying where to work is not choosing what to open
    /// — and it takes what they type, because a path is not a query.
    workspace_entry: Option<WorkspaceEntry>,
    listing: SessionListing,
    /// How much of the settled shelf is on show. History is the longest part
    /// of a body of work and the least of what a reader is choosing between,
    /// so the shelf opens on its first rows and the tail stands behind an
    /// affordance they ask for.
    settled_on_show: usize,
    /// What the reader has typed into the search box. While it says anything
    /// both shelves stand down and the Sidebar answers with the Sessions whose
    /// Titles carry it; empty, it is the whole list again. It belongs to the
    /// look the reader is taking rather than to the Sidebar, so it is given up
    /// the moment they are done looking.
    query: String,
    /// The row the reader is on, held by what the row stands for rather than
    /// by position, so a listing arriving underneath them leaves the selection
    /// where the work is rather than where the row was.
    selected: Option<SidebarSelection>,
    attaching: Option<SessionId>,
    /// Whether the listing in flight is one the reader asked for by opening
    /// the Sidebar, rather than one it asked for on its own to catch up with a
    /// catalog another client moved. Their own ask is them looking again, so
    /// what they had left open is put away when it lands; a catch-up must
    /// leave them exactly where they are.
    asked_afresh: bool,
    /// A listing the Sidebar has asked for but has not yet handed to whoever
    /// dispatches it. Revealing the Sidebar is not always something a reader
    /// did — the initial-visibility Setting reveals it too — so the request
    /// waits here for
    /// the next caller able to carry it.
    awaiting_dispatch: Option<SessionListRequest>,
    /// Whether the last frame had the columns to draw the Sidebar. Only a
    /// frame can answer that, so each one records it here and input routing
    /// reads it back: a Sidebar squeezed off a narrow terminal keeps the
    /// reader's focus but cannot act on it.
    on_screen: Cell<bool>,
    /// The entry the column's window opens on. Only a frame knows how many
    /// lines it holds, so the window is settled at draw time and remembered
    /// here: a selection moving to a row already in view leaves it where it
    /// is, and only a selection moving out of view carries it along.
    window_start: Cell<usize>,
    /// Where the frame in force drew the rows, which is what a press resolves
    /// against. Rendering leaves it here, so it is held behind a cell rather
    /// than taken by an edit.
    geometry: RefCell<SidebarGeometry>,
    /// The context menu the reader opened on a row, where one is open.
    menu: Option<SidebarMenu>,
    /// The Session the Sidebar asked the server to take away, held so a
    /// refusal is drawn by the surface that asked rather than by whichever
    /// other one happens to be listing the same work.
    deleting: Option<SessionId>,
}

/// The lines one active Sidebar row takes, the third of them saying nothing
/// until git awareness gives it something to say
/// (<https://github.com/jake-tucker/suru/issues/169>).
pub(super) const ACTIVE_ROW_LINES: usize = 3;

/// The settled rows a shelf opens on. Recent history is what a reader looks
/// back for, so that much is on show and the rest is theirs to ask for.
const SETTLED_SHELF_OPENING: usize = 10;

/// The settled rows one ask brings up, which is enough that a reader walking
/// back through a long history is not asking over and over.
const SETTLED_SHELF_BATCH: usize = 25;

/// What the reader is on in the Sidebar. Almost always that is a Session,
/// named by its id so the selection follows the work rather than the row it
/// happened to be drawn on. The rest are the Sidebar's own affordances, which
/// stand for no Session at all: the settled shelf's next batch, the Workspace
/// selector above the list, and — while that stands open — one of the
/// Workspaces it offers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarSelection {
    /// The Workspace selector's own row, which stands above the list rather
    /// than in it, and which Enter opens instead of attaching anything.
    Selector,
    /// One entry of the open selector, named by the scope it stands for.
    Scope(WorkspaceScope),
    /// The affordance beside the selector, which Enter opens a path entry from
    /// rather than attaching anything.
    AddWorkspace,
    Session(SessionId),
    ShowMore,
}

/// The path a reader is naming a Workspace by, and how the last one they
/// offered was refused.
#[derive(Clone, Debug, Default)]
struct WorkspaceEntry {
    path: String,
    /// Why the path they last offered was refused, and `None` before they have
    /// offered one — or once they have typed anything since, because a refusal
    /// is about the path it read rather than about the entry it stands under.
    rejection: Option<&'static str>,
}

/// The path entry as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarWorkspaceEntryView<'a> {
    pub(super) path: &'a str,
    pub(super) rejection: Option<&'static str>,
}

/// One Session as the Sidebar draws it. The shelf it stands on decides its
/// shape, so a row carries its shelf alongside what every row says.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarRow<'a> {
    /// The Session this row stands for, which is what a frame records against
    /// the screen rows it draws so a press lands on the work rather than on
    /// the position.
    pub(super) session_id: SessionId,
    pub(super) emoji: Option<&'a str>,
    pub(super) title: &'a str,
    /// Whether this is the Session the reader has open.
    pub(super) current: bool,
    /// Whether this is the row the reader is on, which is the one Enter acts
    /// on and the one the column draws highlighted.
    pub(super) selected: bool,
    pub(super) shelf: SidebarShelf<'a>,
}

/// Which of the Sidebar's two shelves a Session stands on, carrying what that
/// shelf gives its row to say.
#[derive(Clone, Copy, Debug)]
pub(super) enum SidebarShelf<'a> {
    /// Work still active, drawn in full so a reader can tell one Session from
    /// another at a glance.
    Active {
        /// The Workspace this Session is rooted in, drawn by its last
        /// component. A Session Suru could not read may not know its Workspace
        /// at all.
        workspace: Option<&'a Path>,
        /// How long ago this Session was last active, which is what the
        /// row's right slot reads when nothing else claims it.
        updated_at: SessionTimestamp,
        /// When the work this Session is running began, and `None` where it is
        /// running none. It is what the right slot says first, because live
        /// work is what a reader scanning the column is looking for.
        working_since: Option<SessionTimestamp>,
    },
    /// Work set aside as done for now, drawn slim: settled Sessions are
    /// history the reader keeps in view, not work they are choosing between.
    Settled {
        /// When this Session's work ended, drawn in the row's right slot. The
        /// shelf orders on the same reading, so what a row says can never
        /// disagree with where it sits.
        ended_at: SessionTimestamp,
    },
}

impl SidebarShelf<'_> {
    /// The lines a row on this shelf takes.
    const fn lines(&self) -> usize {
        match self {
            Self::Active { .. } => ACTIVE_ROW_LINES,
            Self::Settled { .. } => 1,
        }
    }
}

/// The Sidebar's body, top to bottom: the active Sessions, then — where there
/// is a settled shelf to open — the divider, then the settled ones.
#[derive(Clone, Debug)]
pub(super) enum SidebarEntry<'a> {
    Row(SidebarRow<'a>),
    /// One Workspace the open selector offers, which stands in place of the
    /// shelves while the reader is choosing between them.
    Scope(SidebarScopeEntry),
    /// The rule closing the active list and opening the settled shelf. It
    /// stands only where something is settled: a reader with nothing set aside
    /// is shown no shelf to set it on.
    Divider,
    /// The row at the foot of a settled shelf holding more than is on show,
    /// which brings up the next of it.
    ShowMore(SidebarShowMore),
}

/// One Workspace the selector offers, as a frame draws it.
#[derive(Clone, Debug)]
pub(super) struct SidebarScopeEntry {
    pub(super) label: String,
    /// Whether this is the scope the Sidebar is narrowed to, drawn the way the
    /// Session the reader has open is: it is where they already are.
    pub(super) chosen: bool,
    pub(super) selected: bool,
    /// The scope pressing this entry asks for.
    scope: WorkspaceScope,
}

/// The Workspace selector as a frame draws it, standing between the search box
/// and the list it governs.
#[derive(Clone, Debug)]
pub(super) struct SidebarSelectorView {
    /// What the Sidebar is narrowed to.
    pub(super) label: String,
    /// Whether the entries stand open beneath it.
    pub(super) open: bool,
    pub(super) selected: bool,
    /// Whether the reader is on the add-Workspace affordance beside it, which
    /// shares the selector's line and is highlighted within its own columns of
    /// it.
    pub(super) adding: bool,
}

/// The affordance closing a settled shelf with rows still under it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarShowMore {
    /// How many rows acting on it brings up: the batch, or the whole of what
    /// is left where that is less, so the affordance never offers rows that
    /// are not there.
    pub(super) count: usize,
    /// Whether this is the row the reader is on. The affordance is a row like
    /// any other in that respect: the arrows land on it and Enter acts on it.
    pub(super) selected: bool,
}

impl SidebarEntry<'_> {
    /// The lines this entry takes, which is what a column measures its window
    /// in: entries are not all the same height, so the window is settled in
    /// lines rather than in rows.
    const fn lines(&self) -> usize {
        match self {
            Self::Row(row) => row.shelf.lines(),
            Self::Divider | Self::ShowMore(_) | Self::Scope(_) => 1,
        }
    }

    /// Whether this is the entry the reader is on. The divider never is: it
    /// is a rule rather than a row, so the arrows step over it.
    const fn is_selected(&self) -> bool {
        match self {
            Self::Row(row) => row.selected,
            Self::ShowMore(more) => more.selected,
            Self::Scope(scope) => scope.selected,
            Self::Divider => false,
        }
    }

    /// What pressing this entry asks for, and `None` for the divider, which is
    /// a rule rather than a row and so answers no press.
    pub(super) fn target(&self) -> Option<SidebarTarget> {
        match self {
            Self::Row(row) => Some(SidebarTarget::Session(row.session_id)),
            Self::ShowMore(_) => Some(SidebarTarget::ShowMore),
            Self::Scope(scope) => Some(SidebarTarget::Scope(scope.scope.clone())),
            Self::Divider => None,
        }
    }
}

/// Where a frame drew the Sidebar, which is what a press resolves against.
/// Everything here is terminal geometry the drawing decided, which is why
/// drawing is what records it: a Sidebar no frame has drawn answers no press,
/// because geometry claimed rather than drawn would act on a row the reader
/// never pointed at.
#[derive(Clone, Debug, Default)]
pub(super) struct SidebarGeometry {
    /// The columns inside the Sidebar's own rule, so a press on the rule
    /// itself or out in the main view lands on nothing.
    columns: Range<u16>,
    /// The entries the body drew, top to bottom. The divider draws no span:
    /// it stands for nothing to press.
    rows: Vec<SidebarSpan>,
    /// Where the context menu drew its items, where one was open. It is drawn
    /// over the rows, so it is asked first.
    menu: Option<SidebarMenuGeometry>,
}

/// One drawn entry: the screen rows it filled, and what pressing it asks for.
#[derive(Clone, Debug)]
pub(super) struct SidebarSpan {
    pub(super) rows: Range<u16>,
    /// The columns this entry answers within, where it shares its line with
    /// another — the selector and the affordance beside it are the one such
    /// pair. `None` is the whole of the Sidebar, which is what every entry
    /// with its line to itself answers across.
    pub(super) columns: Option<Range<u16>>,
    pub(super) target: SidebarTarget,
}

/// What a drawn entry stands for. Most of them stand for a Session; the rest
/// stand for the Sidebar's own affordances — the settled shelf's next batch,
/// the Workspace selector, one of the Workspaces it offers, and the affordance
/// beside the selector that opens a path entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SidebarTarget {
    Session(SessionId),
    ShowMore,
    Selector,
    Scope(WorkspaceScope),
    AddWorkspace,
}

/// The context menu as one frame drew it: the columns its box holds and where
/// its items went, one line each.
#[derive(Clone, Debug)]
pub(super) struct SidebarMenuGeometry {
    pub(super) columns: Range<u16>,
    pub(super) top: u16,
    pub(super) count: u16,
}

impl SidebarGeometry {
    /// The entry drawn at this cell, and `None` for a cell the Sidebar drew
    /// nothing pressable on.
    fn hit(&self, position: Position) -> Option<SidebarTarget> {
        if !self.columns.contains(&position.x) {
            return None;
        }
        self.rows
            .iter()
            .find(|span| {
                span.rows.contains(&position.y)
                    && span
                        .columns
                        .as_ref()
                        .is_none_or(|columns| columns.contains(&position.x))
            })
            .map(|span| span.target.clone())
    }

    /// The menu item drawn at this cell, and `None` for a cell outside the
    /// menu's own box — including the rows behind it, because a menu is drawn
    /// over them.
    fn menu_hit(&self, position: Position) -> Option<usize> {
        let menu = self.menu.as_ref()?;
        if !menu.columns.contains(&position.x) {
            return None;
        }
        let offset = position.y.checked_sub(menu.top)?;
        (offset < menu.count).then_some(usize::from(offset))
    }
}

/// What one press of the Sidebar came to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum SidebarPress {
    /// The press asked for a behavior, which its caller invokes: a press
    /// mints no behavior of its own, it only says which one and on what.
    Invoke(SemanticInvocation),
    /// The Sidebar answered the press itself and there is nothing to invoke —
    /// a menu put away, or an item that asked to be confirmed rather than
    /// acting. Either way the press is spent and reaches nothing else.
    Answered,
    /// The press landed somewhere the Sidebar has not drawn, so whatever is
    /// drawn there answers it.
    Elsewhere,
}

/// What acting on the row the reader is on came to. Most rows stand for a
/// Session and are attached; the rest are the Sidebar's own affordances, which
/// it answers itself — except the one that names a Workspace, which is the
/// client's to take rather than the Sidebar's.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum SidebarActivation {
    /// The Sidebar answered it itself: the shelf opened further, the selector
    /// opened or narrowed, a path was refused, or the reader was already on
    /// the Session they asked for.
    Answered,
    Attach(SessionId),
    /// The reader named a directory to work in. It is the client's current
    /// Workspace from here: the root of the Sessions they make next, and what
    /// current-Workspace scope comes to mean.
    Workspace(PathBuf),
}

/// The items a Sidebar row's context menu offers. Which of the first two it
/// carries follows the shelf the row stands on: a settled Session is brought
/// back where an active one is set aside.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarMenuItem {
    Settle,
    Unsettle,
    Delete,
}

/// How many items a Sidebar row's menu offers: what its shelf asks for, and
/// Delete.
pub(super) const SIDEBAR_MENU_ITEMS: usize = 2;

/// The context menu a reader opened on one Sidebar row.
#[derive(Clone, Copy, Debug)]
struct SidebarMenu {
    /// The Session the menu stands on, named rather than positioned so a
    /// listing arriving underneath it acts on the work the reader pointed at.
    session: SessionId,
    /// Whether that row stands on the settled shelf, read when the menu was
    /// opened, which is what decides whether it offers to set the Session
    /// aside or to bring it back.
    settled: bool,
    selected: usize,
    /// Whether Delete has been asked for once. A Session and everything it
    /// owns is not something one stray press may take away, so the item asks
    /// again before it acts.
    confirming_delete: bool,
    /// The cell the reader pointed at, which is the corner the box is drawn
    /// from.
    anchor: Position,
}

impl SidebarMenuItem {
    /// What this item says on the row it is drawn on. Delete says something
    /// else while it is waiting to be confirmed, because the reader has to be
    /// able to see that pressing again is what acts.
    const fn label(self, confirming_delete: bool) -> &'static str {
        match self {
            Self::Settle => "Settle",
            Self::Unsettle => "Unsettle",
            Self::Delete if confirming_delete => "Delete — confirm",
            Self::Delete => "Delete",
        }
    }

    /// The command acting on this item names, which is the same command the
    /// slash and the keys reach and never one minted for the menu.
    const fn command(self) -> SemanticCommandId {
        match self {
            Self::Settle => SemanticCommandId::SessionSettle,
            Self::Unsettle => SemanticCommandId::SessionUnsettle,
            Self::Delete => SemanticCommandId::SessionDelete,
        }
    }
}

impl SidebarMenu {
    const fn items(&self) -> [SidebarMenuItem; SIDEBAR_MENU_ITEMS] {
        [
            if self.settled {
                SidebarMenuItem::Unsettle
            } else {
                SidebarMenuItem::Settle
            },
            SidebarMenuItem::Delete,
        ]
    }
}

/// The context menu as a frame draws it: where it is anchored and what each of
/// its items says.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarMenuView {
    pub(super) anchor: Position,
    pub(super) items: [SidebarMenuEntry; SIDEBAR_MENU_ITEMS],
}

/// One menu item as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarMenuEntry {
    pub(super) label: &'static str,
    pub(super) selected: bool,
    /// Whether acting on this item takes work away, which is drawn so a reader
    /// can tell the item that asks again from the ones that simply act.
    pub(super) destructive: bool,
}

impl Sidebar {
    /// A Sidebar listing every Workspace's Sessions, which is the whole body of
    /// work a reader has. Narrowing to one of them is the selector's job, and
    /// the initial-scope Setting's before that.
    pub(super) fn new(current_workspace: PathBuf) -> Self {
        Self {
            // Down until the initial-visibility Setting raises it. A Sidebar with no
            // Settings in hand has not spoken to a server either, so it has
            // nothing to list; drawing one before the snapshot lands would put
            // an empty column on screen and take it away again for a reader
            // who configured it hidden.
            revealed: false,
            focused: false,
            seeded: false,
            auto_settle: AutoSettle::default(),
            scope: WorkspaceScope::AllWorkspaces,
            selector_open: false,
            workspace_entry: None,
            listing: SessionListing::scoped(
                SessionListSurface::Sidebar,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            settled_on_show: SETTLED_SHELF_OPENING,
            query: String::new(),
            selected: None,
            attaching: None,
            asked_afresh: false,
            awaiting_dispatch: None,
            // Until a frame says otherwise, which it does before anything the
            // reader types can reach a surface.
            on_screen: Cell::new(true),
            window_start: Cell::new(0),
            geometry: RefCell::default(),
            menu: None,
            deleting: None,
        }
    }

    /// Takes the Sidebar's own Settings, each on its own schedule: auto-settle
    /// governs every frame from here on, while the two initial Settings have
    /// their say once and are then the reader's to overrule. Returns nothing: a
    /// Sidebar that wants its Sessions leaves the request in
    /// [`Self::take_listing_request`].
    pub(super) fn adopt_settings(&mut self, settings: &SidebarSettings) {
        self.auto_settle = settings.auto_settle;
        if self.seeded {
            return;
        }
        self.seeded = true;
        self.scope = match settings.initial_scope {
            SidebarScope::AllWorkspaces => WorkspaceScope::AllWorkspaces,
            SidebarScope::CurrentWorkspace => {
                WorkspaceScope::Workspace(self.listing.current_workspace().to_owned())
            }
        };
        self.reveal(settings.initial_visibility == SidebarVisibility::Shown);
    }

    /// Shows the Sidebar, or hides it. This is view state and nothing more: the
    /// initial-visibility Setting is not rewritten.
    ///
    /// A reader who opens the Sidebar is asking to drive it, so it takes the
    /// keys; closing hands them back. The initial-visibility Setting's own
    /// reveal in [`Self::adopt_settings`] does neither, because a reader who has not touched the
    /// Sidebar is typing their first Prompt.
    pub(super) fn toggle(&mut self) {
        self.reveal(!self.revealed);
        if self.revealed {
            self.focused = true;
        } else {
            // Closing is one of the ways out of the Sidebar, so it leaves by
            // the same door the others do — and a Sidebar nobody can see is
            // not one holding a query on the reader's behalf.
            self.hand_back_keys();
        }
    }

    /// Backs the reader out of the Sidebar one step at a time, which is what
    /// Esc asks for. A path entry standing open is the innermost step, given up
    /// with the path in it; the selector's entries are the next, being the
    /// newest thing left that the reader opened; a query in hand is the next
    /// after that, given up so
    /// they go on driving the whole list they are back to. Only from there do
    /// the keys go to the composer, leaving the Sidebar standing — done
    /// choosing, not done looking.
    pub(super) fn leave(&mut self) {
        if self.workspace_entry.take().is_some() {
            return;
        }
        if self.selector_open {
            self.close_selector();
            return;
        }
        if !self.query.is_empty() {
            self.clear_query();
            return;
        }
        self.hand_back_keys();
    }

    /// What the reader has typed into the search box.
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    /// Takes what the reader typed into the line they are typing into: the
    /// path entry where one stands open, and the search box otherwise —
    /// narrowing the list to the Sessions whose Titles carry it.
    pub(super) fn insert(&mut self, text: &str) {
        if let Some(entry) = self.path_being_typed() {
            entry.path.push_str(text);
            return;
        }
        self.query.push_str(text);
        self.keep_selection_drawn();
    }

    /// Takes that line back a character, widening the results to match where
    /// it is the query.
    pub(super) fn delete_backward(&mut self) {
        if let Some(entry) = self.path_being_typed() {
            entry.path.pop();
            return;
        }
        self.query.pop();
        self.keep_selection_drawn();
    }

    /// The path entry to type into, where one stands open — with whatever it
    /// last refused given up, because a refusal is about the path it read and
    /// the reader is changing that path.
    fn path_being_typed(&mut self) -> Option<&mut WorkspaceEntry> {
        let entry = self.workspace_entry.as_mut()?;
        entry.rejection = None;
        Some(entry)
    }

    /// Gives up the query and the results with it, putting the reader back on
    /// the whole list. A widening list never drops a row, so the row they were
    /// on is still there and still theirs.
    fn clear_query(&mut self) {
        self.query.clear();
        self.keep_selection_drawn();
    }

    /// Hands the keys to the composer, and the query goes with them: it was a
    /// way of finding a Session, and the reader is no longer looking for one.
    /// So does any menu standing open: it was opened on a row the reader has
    /// since moved on from.
    fn hand_back_keys(&mut self) {
        self.focused = false;
        self.menu = None;
        // A path entry is a line the reader was typing into, and they have
        // stopped typing.
        self.workspace_entry = None;
        self.close_selector();
        self.clear_query();
    }

    /// Whether the reader is driving the Sidebar. A Sidebar the frame could not
    /// spare the columns for is not one they can be driving, whatever they last
    /// asked for, so the composer keeps the keys until the terminal widens.
    pub(super) fn has_focus(&self) -> bool {
        self.focused && self.on_screen.get()
    }

    /// Gives up what the last frame recorded, so the geometry input routing
    /// reads is always the one on screen.
    pub(super) fn forget_frame(&self) {
        self.on_screen.set(false);
        self.geometry.replace(SidebarGeometry::default());
    }

    /// Records that this frame found the columns for the Sidebar and drew it.
    pub(super) fn record_drawn(&self) {
        self.on_screen.set(true);
    }

    /// Takes the geometry the frame just drew its body in, which is the only
    /// account of the Sidebar a press can be resolved against.
    pub(super) fn record_geometry(&self, columns: Range<u16>, rows: Vec<SidebarSpan>) {
        let mut geometry = self.geometry.borrow_mut();
        geometry.columns = columns;
        geometry.rows = rows;
    }

    /// Takes the geometry the frame drew the context menu in, which is drawn
    /// after the body it stands over and so is recorded on its own.
    pub(super) fn record_menu_geometry(&self, menu: SidebarMenuGeometry) {
        self.geometry.borrow_mut().menu = Some(menu);
    }

    /// Answers a press at one cell of the frame.
    ///
    /// The layers are asked in the order they were drawn: a menu standing over
    /// the rows takes the press first, and a press outside it puts it away and
    /// is spent there — a reader dismissing a menu is not also acting on
    /// whatever it was covering. Otherwise the press lands on a row, which
    /// takes the selection and is opened, or on one of the Sidebar's own
    /// affordances — the settled shelf's next batch, the Workspace selector,
    /// one of the Workspaces it offers, or the affordance beside it that opens
    /// a path entry. Every one of them is what Enter already
    /// does from that row, so the press mints no behavior of its own: it says
    /// which row, by a position only this frame can know, and the command does
    /// the rest.
    pub(super) fn press_at(&mut self, position: Position) -> SidebarPress {
        // A path entry standing open is what the reader is doing, so a press on
        // the line that opened it is a press to be done with it — the
        // affordance because pointing at it twice is asking to be back where
        // they started, and the selector beside it because reaching for the
        // other control puts this one away. Either way the press is spent
        // there: they are out of the entry, and what they do next is theirs to
        // point at. The rest of the column draws nothing pressable while the
        // entry stands, so nothing else can be pointed at anyway.
        if self.workspace_entry.is_some() {
            let hit = self.geometry.borrow().hit(position);
            return match hit {
                Some(SidebarTarget::Selector | SidebarTarget::AddWorkspace) => {
                    self.workspace_entry = None;
                    SidebarPress::Answered
                }
                _ => SidebarPress::Elsewhere,
            };
        }
        if self.menu_is_open() {
            let item = self.geometry.borrow().menu_hit(position);
            return match item {
                Some(index) => self.act_on_menu_item(index),
                None => {
                    self.menu = None;
                    SidebarPress::Answered
                }
            };
        }
        let hit = self.geometry.borrow().hit(position);
        let Some(target) = hit else {
            return SidebarPress::Elsewhere;
        };
        self.select(target);
        SidebarPress::Invoke(SemanticCommandId::SidebarAttach.into())
    }

    /// Opens the context menu on the row the reader asked for one on, which
    /// also puts them on that row: a menu acts on the Session under it, and a
    /// selection drawn elsewhere would say otherwise.
    ///
    /// The settled shelf's affordance stands for no Session, so it offers no
    /// menu — and neither does a press out in the main view. Both put away
    /// whatever menu was up, because asking for a menu somewhere else is done
    /// with the one in hand.
    pub(super) fn open_menu_at(&mut self, position: Position) {
        // Asking for a menu inside the menu asks for nothing: the box stands
        // over a row it did not open on, and re-opening there would carry the
        // reader onto whichever row it happens to cover.
        if self.menu_is_open() && self.geometry.borrow().menu_hit(position).is_some() {
            return;
        }
        let hit = self.geometry.borrow().hit(position);
        self.menu = None;
        let Some(SidebarTarget::Session(session_id)) = hit else {
            return;
        };
        let Some(settled) = self
            .listing
            .sessions()
            .iter()
            .find(|session| session.id() == session_id)
            .map(|session| self.settlement().settles(session))
        else {
            return;
        };
        self.select(SidebarTarget::Session(session_id));
        self.menu = Some(SidebarMenu {
            session: session_id,
            settled,
            selected: 0,
            confirming_delete: false,
            anchor: position,
        });
    }

    /// Whether a context menu is up, which is what gives it the keys: it is
    /// the newest thing on screen, so the arrows walk it rather than the rows
    /// behind it.
    ///
    /// A menu is part of the column it was opened in, so a frame that could
    /// not spare the Sidebar's columns draws no menu either and holds none of
    /// the keys — the reader keeps the menu they opened, as they keep the
    /// focus, and both come back when the terminal widens.
    pub(super) fn menu_is_open(&self) -> bool {
        self.menu.is_some() && self.on_screen.get()
    }

    /// The menu as a frame draws it, and `None` where there is none to draw.
    pub(super) fn menu(&self) -> Option<SidebarMenuView> {
        if !self.on_screen.get() {
            return None;
        }
        let menu = self.menu.as_ref()?;
        let items = menu.items();
        Some(SidebarMenuView {
            anchor: menu.anchor,
            items: std::array::from_fn(|index| SidebarMenuEntry {
                label: items[index].label(menu.confirming_delete),
                selected: menu.selected == index,
                destructive: items[index] == SidebarMenuItem::Delete,
            }),
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
        let length = SIDEBAR_MENU_ITEMS as isize;
        menu.selected = (menu.selected as isize + distance).rem_euclid(length) as usize;
    }

    /// Acts on the item the reader is on, which is what Enter asks for.
    pub(super) fn activate_menu_item(&mut self) -> SidebarPress {
        let Some(menu) = &self.menu else {
            return SidebarPress::Answered;
        };
        self.act_on_menu_item(menu.selected)
    }

    /// Puts the menu away, leaving the row it stood on alone.
    pub(super) fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Acts on one menu item, answering with the command it asks for.
    ///
    /// Pointing at an item is choosing it, so the selection follows the press
    /// before the item acts. Settling and unsettling act at once and the menu
    /// is done; Delete asks again the first time and acts the second, so the
    /// menu stands until the reader has said it twice.
    fn act_on_menu_item(&mut self, index: usize) -> SidebarPress {
        let Some(menu) = &mut self.menu else {
            return SidebarPress::Answered;
        };
        let Some(item) = menu.items().get(index).copied() else {
            return SidebarPress::Answered;
        };
        menu.selected = index;
        if item == SidebarMenuItem::Delete && !menu.confirming_delete {
            menu.confirming_delete = true;
            return SidebarPress::Answered;
        }
        let session_id = menu.session;
        self.menu = None;
        SidebarPress::Invoke(item.command().on_session(session_id))
    }

    /// Puts the reader on the row a press landed on.
    fn select(&mut self, target: SidebarTarget) {
        self.selected = Some(match target {
            SidebarTarget::Session(session_id) => SidebarSelection::Session(session_id),
            SidebarTarget::ShowMore => SidebarSelection::ShowMore,
            SidebarTarget::Selector => SidebarSelection::Selector,
            SidebarTarget::Scope(scope) => SidebarSelection::Scope(scope),
            SidebarTarget::AddWorkspace => SidebarSelection::AddWorkspace,
        });
    }

    /// Notes the Session the Sidebar has asked the server to take away, so a
    /// refusal is drawn here rather than by some other surface listing the
    /// same work. The listing's own complaint goes with it: the reader is
    /// being answered afresh.
    pub(super) fn begin_deletion(&mut self, session_id: SessionId) {
        self.deleting = Some(session_id);
        self.listing.clear_error();
    }

    /// Takes the server's refusal to delete, where it was this Sidebar that
    /// asked. Answering `false` leaves the refusal for whichever surface did.
    pub(super) fn fail_deletion(&mut self, session_id: SessionId, error: String) -> bool {
        if self.deleting != Some(session_id) {
            return false;
        }
        self.deleting = None;
        self.listing.report_error(error);
        true
    }

    /// A Sidebar the reader can see wants Sessions to show, so every reveal —
    /// the initial-visibility Setting's and the toggle's alike — asks for them
    /// afresh.
    /// Hiding keeps what it holds: nothing is looking at it, and revealing
    /// again asks anyway.
    fn reveal(&mut self, revealed: bool) {
        self.revealed = revealed;
        if revealed {
            // A reader opening the Sidebar can type into it before the next
            // frame is drawn: the run loop takes a whole run of terminal events
            // at once, so the toggle and the arrow after it are handled with no
            // draw between them. The Sidebar assumes it has the columns until a
            // frame reports otherwise, so that run reaches the surface the
            // reader just opened.
            self.on_screen.set(true);
            // A query belongs to the look the reader was taking, and a Sidebar
            // coming into view is the start of another one — so it opens on
            // the whole body of work, as the settled shelf opens on its first
            // rows again, and a menu or a set of selector entries left open on
            // some earlier look is put away with it. The scope itself stands:
            // it is where the reader works rather than a look they were
            // taking.
            self.menu = None;
            self.workspace_entry = None;
            self.close_selector();
            self.clear_query();
            self.ask_for_sessions();
        }
    }

    /// Asks the server for the Sessions in scope and leaves the request for
    /// whoever can dispatch it.
    ///
    /// The scope is not part of the ask: the Sidebar always asks for the whole
    /// body of work, because the selector's entries are read off what comes
    /// back and narrowing the ask would take the other Workspaces off the
    /// selector along with their Sessions.
    ///
    /// The settled shelf opens on its first rows again with every ask. The
    /// tail a reader brought up belongs to the listing they brought it up on,
    /// so a Sidebar coming back into view starts back at the top of the shelf
    /// rather than inheriting however deep they had walked into some other
    /// body of work — as does one narrowed to another Workspace, which resets
    /// the shelf without asking again. A listing re-asked only to catch up
    /// with the server
    /// (<https://github.com/jake-tucker/suru/issues/183>) is not the reader
    /// moving anywhere, and must leave their shelf where they left it.
    fn ask_for_sessions(&mut self) {
        self.settled_on_show = SETTLED_SHELF_OPENING;
        self.asked_afresh = true;
        self.awaiting_dispatch = Some(self.listing.refresh());
    }

    /// Asks for the Sessions again because the session-catalog stream reported
    /// the body of work moving under this client — a Session made, retitled,
    /// deleted, set aside, brought back, or a whole catalog reconciled after a
    /// reconnection. What the stream says is taken in place first, so the
    /// frame is right before the answer lands; the ask is what carries
    /// everything the stream does not say, a new Session's Title and Workspace
    /// most of all.
    ///
    /// A Sidebar the reader has closed asks for nothing, because revealing it
    /// asks anyway — while one the frame merely has no columns for goes on
    /// asking, so that the columns coming back bring a Sidebar that is true. A
    /// catch-up is not the reader looking again either, so the rows stand
    /// until the answer arrives, the settled shelf stays as deep as they
    /// walked it, and the row they are on stays under them.
    pub(super) fn catch_up(&mut self) {
        if !self.revealed {
            return;
        }
        self.asked_afresh = false;
        self.awaiting_dispatch = Some(self.listing.catch_up());
    }

    /// Whether a listing the server answered with would move anything the
    /// Sidebar draws, per [`SessionListing::would_move`].
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

    /// Whether the reader wants the Sidebar on screen, which is not the same
    /// question as whether the frame has room for it.
    pub(super) const fn is_revealed(&self) -> bool {
        self.revealed
    }

    /// Whether a row the Sidebar has on screen is running work, which is what
    /// arms the tick its Working durations rise on. It asks the body rather
    /// than the listing, so a query the reader has narrowed to narrows what
    /// animates with it — and a Sidebar the reader closed, or one the frame
    /// could not spare the columns for, shows no live work whatever its
    /// listing holds.
    ///
    /// It answers from the listing in hand, which the session-catalog stream
    /// keeps true: a Turn starting or settling anywhere arrives as a working
    /// change and is taken in place by [`Self::set_working`], so the tick is
    /// armed exactly while something listed is live and an idle TUI schedules
    /// zero wakeups (ADR 0007, ADR 0009).
    pub(super) fn shows_live_work(&self) -> bool {
        self.revealed
            && self.on_screen.get()
            && self.body().into_iter().any(|entry| match entry {
                BodyEntry::Session(session, _) => session.working_since().is_some(),
                BodyEntry::Scope(_) | BodyEntry::Divider | BodyEntry::ShowMore(_) => false,
            })
    }

    /// The listing the Sidebar is waiting on, handed over exactly once so the
    /// caller that can dispatch it does so and no later caller repeats it.
    pub(super) fn take_listing_request(&mut self) -> Option<SessionListRequest> {
        self.awaiting_dispatch.take()
    }

    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        sessions: Vec<SessionListItem>,
        current: Option<SessionId>,
    ) {
        if !self.listing.load(request, sessions) {
            return;
        }
        if std::mem::take(&mut self.asked_afresh) {
            self.attaching = None;
            // A menu stands on one row of the listing it was opened over. A
            // listing the reader asked for is them looking again, so it is put
            // away rather than left pointing at whatever now stands where its
            // row did.
            self.menu = None;
        } else {
            // A catch-up leaves the reader in whatever they were in the middle
            // of, so long as the Session behind it survived the listing that
            // arrived.
            self.forget_absent();
        }
        // A listing the reader was already reading keeps them where they were;
        // a fresh one starts them on the Session they have open, and failing
        // that on the row nearest them.
        self.selected = self
            .selected
            .take()
            .filter(|selected| self.holds(selected))
            .or_else(|| {
                current
                    .filter(|current| self.draws(*current))
                    .map(SidebarSelection::Session)
            })
            .or_else(|| first_row(&self.selectable()));
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        self.listing.fail(request, error);
    }

    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        self.listing.retitle(session_id, title, emoji);
        // A Title is what a query is read against, so another client's retitle
        // can carry the row the reader is on out of the results under them.
        self.keep_selection_drawn();
    }

    pub(super) fn settle(&mut self, session_id: SessionId, settled_at: Option<SessionTimestamp>) {
        self.listing.settle(session_id, settled_at);
    }

    /// Takes a Turn the server reports starting or settling into the listing
    /// in hand, so the row's Working label — and the tick
    /// [`Self::shows_live_work`] arms off it — is true between listings.
    pub(super) fn set_working(
        &mut self,
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    ) {
        self.listing.set_working(session_id, working_since);
    }

    pub(super) fn remove(&mut self, session_id: SessionId) {
        self.listing.remove(session_id);
        self.forget_absent();
    }

    pub(super) fn retain_catalog(&mut self, session_ids: &[SessionId]) {
        self.listing.retain(session_ids);
        self.forget_absent();
    }

    /// Moves the reader one row up the list, wrapping past the top.
    pub(super) fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    /// Moves the reader one row down the list, wrapping past the end.
    pub(super) fn select_next(&mut self) {
        self.move_selection(1);
    }

    /// Acts on the row the reader is on, reporting the Session to attach where
    /// that is what the row asks for.
    ///
    /// A path entry standing open is what Enter acts on first, whatever row the
    /// reader came to it from: it is the line they are typing into, and
    /// offering the path is the only thing acting on it can mean.
    ///
    /// Standing on one of the Sidebar's own affordances it acts on that
    /// instead, and there is nothing to attach: the settled shelf's row brings
    /// up more of the shelf, the selector opens its entries, an entry narrows
    /// the Sidebar to the Workspace it names, and the affordance beside the
    /// selector opens the path entry. Nor is there anything to attach when the
    /// row stands for a Session Suru could not read, or for the Session already
    /// open — in which case Enter means only that the reader is done choosing,
    /// and the composer takes the keys back.
    pub(super) fn activate(&mut self, current: Option<SessionId>) -> SidebarActivation {
        if self.workspace_entry.is_some() {
            return self.offer_workspace();
        }
        let Some(selection) = self.selected.clone() else {
            return SidebarActivation::Answered;
        };
        let selected = match selection {
            SidebarSelection::Session(session_id) => session_id,
            SidebarSelection::ShowMore => {
                self.show_more();
                return SidebarActivation::Answered;
            }
            SidebarSelection::Selector => {
                self.toggle_selector();
                return SidebarActivation::Answered;
            }
            SidebarSelection::Scope(scope) => {
                self.choose_scope(scope);
                return SidebarActivation::Answered;
            }
            SidebarSelection::AddWorkspace => {
                self.open_workspace_entry();
                return SidebarActivation::Answered;
            }
        };
        let readable = self
            .listing
            .sessions()
            .iter()
            .find(|summary| summary.id() == selected)
            .and_then(SessionListItem::readable)
            .is_some();
        if !readable {
            return SidebarActivation::Answered;
        }
        if current == Some(selected) {
            self.hand_back_keys();
            return SidebarActivation::Answered;
        }
        self.listing.clear_error();
        self.attaching = Some(selected);
        SidebarActivation::Attach(selected)
    }

    /// Opens the path entry the affordance stands for.
    ///
    /// The selector's entries are put away: naming a Workspace and choosing
    /// between the ones already known are two answers to the same question, and
    /// only one of them is being asked. The keys come with it, however the
    /// reader asked — a pointer as readily as Enter — because an entry nobody
    /// can type into is no entry at all.
    fn open_workspace_entry(&mut self) {
        self.close_selector();
        self.focused = true;
        self.selected = Some(SidebarSelection::AddWorkspace);
        self.workspace_entry = Some(WorkspaceEntry::default());
    }

    /// Reads the path the reader offered.
    ///
    /// A path given relative is read from the Workspace they are working in
    /// rather than from wherever the process happened to be started, and an
    /// absolute one replaces it outright — which is one reading of a path on
    /// every platform, rather than a POSIX one dressed up as a general rule.
    ///
    /// What is taken is the canonical path rather than the spelling: the server
    /// canonicalizes the Workspace it roots a Session at and the Workspace it
    /// narrows a listing by, and this scope is compared against what comes back
    /// from that listing. A client holding some other spelling of the same
    /// directory would narrow to a Workspace none of its own Sessions matched
    /// and offer the reader two selector entries of the same name — and would
    /// do it on Windows always, where the canonical form carries a prefix no
    /// reader types.
    ///
    /// Only a directory is taken: a Workspace is rooted at one, so a path
    /// standing at a file or at nothing is refused where the reader can see it
    /// and the entry stands open for them to correct. Nothing else moves — the
    /// scope, the shelves, and the Sessions in them are as they were.
    fn offer_workspace(&mut self) -> SidebarActivation {
        let Some(entry) = self.workspace_entry.as_ref() else {
            return SidebarActivation::Answered;
        };
        let named = entry.path.trim();
        if named.is_empty() {
            return self.refuse_workspace(NAME_A_DIRECTORY);
        }
        let named = self.listing.current_workspace().join(named);
        let Ok(candidate) = std::fs::canonicalize(&named) else {
            return self.refuse_workspace(NO_DIRECTORY_THERE);
        };
        if !candidate.is_dir() {
            return self.refuse_workspace(NOT_A_DIRECTORY);
        }
        // The reader is done in the Sidebar: they came to say where the work
        // is, and the work itself is written in the composer.
        self.hand_back_keys();
        SidebarActivation::Workspace(candidate)
    }

    /// Draws the refusal under the entry, leaving it open on the path that
    /// earned it.
    fn refuse_workspace(&mut self, rejection: &'static str) -> SidebarActivation {
        if let Some(entry) = &mut self.workspace_entry {
            entry.rejection = Some(rejection);
        }
        SidebarActivation::Answered
    }

    /// Takes the Workspace this client has moved to, which the reader named at
    /// the path entry.
    ///
    /// The Sidebar narrows to it: a reader who has just said where they work is
    /// saying which work they mean, and the settled shelf opens on its first
    /// rows again as it does under any other narrowing. Nothing is asked of the
    /// server — the listing is the whole body of work either way.
    ///
    /// The row left marked is the selector, which now says where they are. It
    /// is the dim mark rather than the lit one, because taking the Workspace
    /// handed the keys back to the composer: it says where Enter would land
    /// were the reader to come back, which is what that mark says everywhere
    /// else in the column.
    pub(super) fn adopt_workspace(&mut self, workspace: PathBuf) {
        self.listing.adopt_current_workspace(workspace.clone());
        self.choose_scope(WorkspaceScope::Workspace(workspace));
        self.selected = Some(SidebarSelection::Selector);
    }

    /// The path entry as a frame draws it, and `None` where there is none to
    /// draw.
    pub(super) fn workspace_entry(&self) -> Option<SidebarWorkspaceEntryView<'_>> {
        self.workspace_entry
            .as_ref()
            .map(|entry| SidebarWorkspaceEntryView {
                path: &entry.path,
                rejection: entry.rejection,
            })
    }

    /// Brings up the next of the settled shelf. Where that was the whole of
    /// what was left, the affordance the reader was standing on goes with it,
    /// so they land on the last row it brought up rather than on nothing.
    fn show_more(&mut self) {
        self.settled_on_show = self.settled_on_show.saturating_add(SETTLED_SHELF_BATCH);
        let selectable = self.selectable();
        if !selectable.contains(&SidebarSelection::ShowMore) {
            self.selected = selectable.last().cloned();
        }
    }

    /// Opens the selector's entries, putting the reader on the scope in force:
    /// a list of Workspaces opens where the reader already is, so choosing to
    /// stay is one keystroke and choosing to move is the arrows.
    ///
    /// Acting on the selector a second time puts the entries away again. Only
    /// a pointer can ask that — the arrows do not reach the row that opened
    /// them — and a reader who points at the same affordance twice is asking
    /// to be back where they started.
    fn toggle_selector(&mut self) {
        if self.selector_open {
            self.close_selector();
            return;
        }
        self.selector_open = true;
        self.selected = Some(SidebarSelection::Scope(self.scope.clone()));
    }

    /// Puts the entries away, leaving the scope where it was and the reader on
    /// the selector that opened them.
    fn close_selector(&mut self) {
        if !self.selector_open {
            return;
        }
        self.selector_open = false;
        self.selected = Some(SidebarSelection::Selector);
    }

    /// Narrows the Sidebar to one Workspace, or widens it to all of them.
    ///
    /// Nothing is asked of the server: the listing is the whole body of work
    /// either way, and the scope decides which of it the shelves draw. The
    /// settled shelf opens on its first rows again, because how deep a reader
    /// walked into one Workspace's history says nothing about another's.
    fn choose_scope(&mut self, scope: WorkspaceScope) {
        self.close_selector();
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.settled_on_show = SETTLED_SHELF_OPENING;
    }

    /// The selector as a frame draws it.
    pub(super) fn selector(&self) -> SidebarSelectorView {
        SidebarSelectorView {
            label: self.scope.label(),
            open: self.selector_open,
            selected: self.selected == Some(SidebarSelection::Selector),
            adding: self.selected == Some(SidebarSelection::AddWorkspace),
        }
    }

    /// Whether the Sidebar is answering for one Workspace rather than for the
    /// reader's whole body of work, which is what an empty column means by
    /// nothing being here.
    pub(super) fn is_narrowed(&self) -> bool {
        self.scope != WorkspaceScope::AllWorkspaces
    }

    pub(super) fn attaching_to(&self, session_id: SessionId) -> bool {
        self.attaching == Some(session_id)
    }

    pub(super) const fn is_attaching(&self) -> bool {
        self.attaching.is_some()
    }

    /// The Session the Sidebar asked for is on screen. The reader is done
    /// choosing, so the composer takes the keys back and they can prompt what
    /// they just opened.
    pub(super) fn finish_attachment(&mut self) {
        self.attaching = None;
        self.hand_back_keys();
    }

    /// The server refused the attachment. The reader keeps the keys and the
    /// list, and the refusal is drawn above it.
    pub(super) fn fail_attachment(&mut self, error: String) {
        self.attaching = None;
        self.listing.report_error(error);
    }

    pub(super) const fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    /// What a column this many lines tall shows: a list longer than the Sidebar
    /// is read through a window rather than being crammed into the lines
    /// available. The window moves only as far as it must to keep the row the
    /// reader is on in view, so moving within it leaves every other row where
    /// the reader last saw it, and an entry the last line cannot hold whole is
    /// left off rather than cut in half.
    pub(super) fn visible_entries(
        &self,
        capacity: usize,
        current: Option<SessionId>,
    ) -> Vec<SidebarEntry<'_>> {
        let entries = self.entries(current);
        let heights = entries.iter().map(SidebarEntry::lines).collect::<Vec<_>>();
        let selected = entries.iter().position(SidebarEntry::is_selected);
        let start = window_start(self.window_start.get(), selected, &heights, capacity);
        self.window_start.set(start);
        let mut remaining = capacity;
        entries
            .into_iter()
            .skip(start)
            .take_while(|entry| {
                let Some(left) = remaining.checked_sub(entry.lines()) else {
                    return false;
                };
                remaining = left;
                true
            })
            .collect()
    }

    /// The Sidebar's body as the frame draws it, which is [`Self::body`] with
    /// each entry given what its row says.
    fn entries(&self, current: Option<SessionId>) -> Vec<SidebarEntry<'_>> {
        self.body()
            .into_iter()
            .map(|entry| match entry {
                BodyEntry::Divider => SidebarEntry::Divider,
                BodyEntry::Scope(scope) => SidebarEntry::Scope(SidebarScopeEntry {
                    label: scope.label(),
                    chosen: scope == self.scope,
                    selected: self.selected == Some(SidebarSelection::Scope(scope.clone())),
                    scope,
                }),
                BodyEntry::ShowMore(count) => SidebarEntry::ShowMore(SidebarShowMore {
                    count,
                    selected: self.selected == Some(SidebarSelection::ShowMore),
                }),
                BodyEntry::Session(session, Standing::Active) => self.row(
                    session,
                    current,
                    SidebarShelf::Active {
                        workspace: session
                            .workspace()
                            .map(|workspace| workspace.path.as_path()),
                        updated_at: session.updated_at(),
                        working_since: session.working_since(),
                    },
                ),
                BodyEntry::Session(session, Standing::Settled) => self.row(
                    session,
                    current,
                    SidebarShelf::Settled {
                        ended_at: ended_at(session),
                    },
                ),
            })
            .collect()
    }

    /// The Sidebar's body top to bottom: the active Sessions, then the divider
    /// and as much of the settled shelf as is on show — or, while the reader is
    /// searching, the results in place of both; while they are choosing a
    /// Workspace, the selector's own entries in place of everything; and while
    /// they are naming one, nothing at all, the path entry the frame draws
    /// there standing for no work.
    ///
    /// Everything the Sidebar has to say about what stands where is said here
    /// and nowhere else, so the rows the frame draws and the rows the arrows
    /// walk can never disagree.
    fn body(&self) -> Vec<BodyEntry<'_>> {
        // A path entry stands in place of the whole list, for the same reason
        // the selector's entries do and more so: a reader saying where to work
        // is not choosing what to open, and what they type is a path rather
        // than a query the list could answer. Standing for no rows also stands
        // the tick down, because [`Self::shows_live_work`] reads this body: a
        // column drawing no work has none to animate.
        if self.workspace_entry.is_some() {
            return Vec::new();
        }
        // The entries stand in place of both shelves, as the results of a query
        // do: a reader choosing where to look is not choosing what to open.
        if self.selector_open {
            return self.scopes().into_iter().map(BodyEntry::Scope).collect();
        }
        let settlement = self.settlement();
        if !self.query.is_empty() {
            return self.results(settlement);
        }
        let mut body = self
            .active(settlement)
            .into_iter()
            .map(|session| BodyEntry::Session(session, Standing::Active))
            .collect::<Vec<_>>();
        let shelf = self.shelf(settlement);
        if shelf.on_show.is_empty() {
            return body;
        }
        body.push(BodyEntry::Divider);
        body.extend(
            shelf
                .on_show
                .into_iter()
                .map(|session| BodyEntry::Session(session, Standing::Settled)),
        );
        body.extend(shelf.batch.map(BodyEntry::ShowMore));
        body
    }

    /// The Workspaces the selector offers: all of them first, because the whole
    /// body of work is what a Sidebar opens on, then every Workspace the
    /// listing has work rooted in and the one this client itself runs in —
    /// which stands whether or not there is work in it yet, being where the
    /// reader's next Session will be.
    ///
    /// They are read off the whole listing rather than off the Sessions in
    /// scope, so narrowing to one Workspace never takes the others off the
    /// selector: a reader who narrowed has to be able to widen again, and to
    /// step straight across to a third.
    fn scopes(&self) -> Vec<WorkspaceScope> {
        let mut workspaces = self
            .listing
            .sessions()
            .iter()
            .filter_map(SessionListItem::workspace)
            .map(|workspace| workspace.path.clone())
            .chain(std::iter::once(self.listing.current_workspace().to_owned()))
            .collect::<Vec<_>>();
        // Ordered by path and deduplicated, so the entries hold their places
        // between one listing and the next.
        workspaces.sort_unstable();
        workspaces.dedup();
        std::iter::once(WorkspaceScope::AllWorkspaces)
            .chain(workspaces.into_iter().map(WorkspaceScope::Workspace))
            .collect()
    }

    /// What a query narrows the Sidebar to: the Sessions whose Titles carry it,
    /// as one flat list.
    ///
    /// Searching is a look across the whole body of work rather than down one
    /// shelf, so both shelves stand down: no divider parts the results, and
    /// nothing stands behind the affordance — a reader who narrowed the list
    /// themselves is shown the whole of what they narrowed it to. The results
    /// keep the order the shelves would have drawn them in, so a Session sits
    /// where the reader would have gone looking for it, and each keeps the
    /// shape its shelf gives it, so they can still tell live work from history.
    fn results(&self, settlement: Settlement) -> Vec<BodyEntry<'_>> {
        self.active(settlement)
            .into_iter()
            .map(|session| (session, Standing::Active))
            .chain(
                self.settled(settlement)
                    .into_iter()
                    .map(|session| (session, Standing::Settled)),
            )
            .filter(|(session, _)| title_carries(&self.query, session.title()))
            .map(|(session, standing)| BodyEntry::Session(session, standing))
            .collect()
    }

    /// The settled shelf as one pass over the listing draws it: the rows on
    /// show, and what the affordance under them offers. Both readings are
    /// settled here and nowhere else, so the rows the arrows walk and the rows
    /// the frame draws can never disagree about where the shelf ends.
    fn shelf(&self, settlement: Settlement) -> DrawnShelf<'_> {
        let settled = self.settled(settlement);
        let mut on_show = settled
            .iter()
            .take(self.settled_on_show)
            .copied()
            .collect::<Vec<_>>();
        // The row the reader is on stands whatever the cap says. They reach a
        // Session from elsewhere than the shelf — the session picker, or the
        // one they had open when it settled — and a selection drawn nowhere is
        // one the arrows cannot step off and Enter cannot act on. The clone
        // makes the same exception for the same reason. It stands at the foot
        // of the shelf rather than in the order the rest keep, because it is
        // there on the reader's account rather than on its work's.
        if let Some(SidebarSelection::Session(selected)) = self.selected
            && let Some(deeper) = settled
                .iter()
                .skip(self.settled_on_show)
                .find(|session| session.id() == selected)
                .copied()
        {
            on_show.push(deeper);
        }
        let hidden = settled.len() - on_show.len();
        DrawnShelf {
            batch: (hidden > 0).then(|| hidden.min(SETTLED_SHELF_BATCH)),
            on_show,
        }
    }

    fn row<'a>(
        &self,
        session: &'a SessionListItem,
        current: Option<SessionId>,
        shelf: SidebarShelf<'a>,
    ) -> SidebarEntry<'a> {
        SidebarEntry::Row(SidebarRow {
            session_id: session.id(),
            emoji: session.emoji(),
            title: session.title(),
            current: current == Some(session.id()),
            selected: self.selected == Some(SidebarSelection::Session(session.id())),
            shelf,
        })
    }

    /// What settles a Session as of now. Each pass over the listing takes one
    /// reading and hands it to both shelves, so a Session whose threshold falls
    /// between two readings of the clock cannot come out on both of them or on
    /// neither.
    fn settlement(&self) -> Settlement {
        Settlement {
            auto: self.auto_settle,
            now: SessionTimestamp::now(),
        }
    }

    /// The active Sessions in scope: newest created first, and never reordered
    /// by activity, so a row a reader has their eye on holds its place while
    /// the work behind it moves.
    fn active(&self, settlement: Settlement) -> Vec<&SessionListItem> {
        let mut sessions = self
            .in_scope()
            .filter(|session| !settlement.settles(session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(session.created_at()));
        sessions
    }

    /// The Sessions in scope that are set aside, ordered by when the work ended
    /// rather than by when it began, so what wrapped up most recently is
    /// nearest the divider.
    fn settled(&self, settlement: Settlement) -> Vec<&SessionListItem> {
        let mut sessions = self
            .in_scope()
            .filter(|session| settlement.settles(session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(ended_at(session)));
        sessions
    }

    /// The Sessions the selector's scope draws, which is every one the listing
    /// holds until the reader narrows to a Workspace.
    fn in_scope(&self) -> impl Iterator<Item = &SessionListItem> {
        self.listing
            .sessions()
            .iter()
            .filter(|session| self.scope.holds(session))
    }

    /// Every row the reader can be on, in the order the Sidebar draws them,
    /// which is the order the arrows walk: the selector standing above the
    /// list and the affordance sharing its line, left to right, then the body
    /// without the divider, which is a rule rather than a row.
    ///
    /// The selector is not among them while its own entries are open: the
    /// reader is inside the control rather than on it, and Esc is the way back
    /// out.
    fn selectable(&self) -> Vec<SidebarSelection> {
        // A path entry is the one thing a reader with one open is doing, so the
        // affordance that opened it is the one place they can be: the arrows
        // have nowhere to walk while they are saying where to work.
        if self.workspace_entry.is_some() {
            return vec![SidebarSelection::AddWorkspace];
        }
        let rows = self.body().into_iter().filter_map(|entry| match entry {
            BodyEntry::Session(session, _) => Some(SidebarSelection::Session(session.id())),
            BodyEntry::ShowMore(_) => Some(SidebarSelection::ShowMore),
            BodyEntry::Scope(scope) => Some(SidebarSelection::Scope(scope)),
            BodyEntry::Divider => None,
        });
        if self.selector_open {
            return rows.collect();
        }
        [SidebarSelection::Selector, SidebarSelection::AddWorkspace]
            .into_iter()
            .chain(rows)
            .collect()
    }

    /// Whether the row the reader is on is one the Sidebar still draws — which
    /// asks the body, so a Session dropped from the listing and one a query
    /// passed over are answered by the same reading.
    fn holds(&self, selection: &SidebarSelection) -> bool {
        self.selectable().contains(selection)
    }

    /// Whether the Sidebar would put the reader on this Session: it is one it
    /// lists, and one their query carries.
    ///
    /// This is the question [`Self::holds`] cannot answer, because the settled
    /// shelf shows the row the reader is on however deep it sits — so asking
    /// the body whether a Session they are not yet on is drawn would answer no
    /// for the very rows the shelf would have made room for.
    fn draws(&self, session_id: SessionId) -> bool {
        self.in_scope()
            .find(|session| session.id() == session_id)
            .is_some_and(|session| {
                self.query.is_empty() || title_carries(&self.query, session.title())
            })
    }

    /// Puts the reader back on a row that is drawn, where the one they were on
    /// no longer is. Narrowing the list is the ordinary way that happens: they
    /// type another letter and the row under them steps aside. The rows are
    /// read once and asked both questions, because reading them means walking
    /// and sorting the whole listing.
    fn keep_selection_drawn(&mut self) {
        let selectable = self.selectable();
        if self
            .selected
            .as_ref()
            .is_none_or(|selected| !selectable.contains(selected))
        {
            self.selected = first_row(&selectable);
        }
    }

    fn move_selection(&mut self, distance: isize) {
        let selectable = self.selectable();
        if selectable.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| selectable.iter().position(|row| row == selected))
            .unwrap_or(0);
        let len = selectable.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = selectable.get(next).cloned();
    }

    /// Drops what the Sidebar was pointing at once the Session behind it has
    /// left the listing, so no row is attached twice, no menu offers to act on
    /// work that is gone, and the reader lands back on a row that is there.
    fn forget_absent(&mut self) {
        if self
            .attaching
            .is_some_and(|attaching| !self.listing.contains(attaching))
        {
            self.attaching = None;
        }
        if self
            .deleting
            .is_some_and(|deleting| !self.listing.contains(deleting))
        {
            self.deleting = None;
        }
        if self
            .menu
            .is_some_and(|menu| !self.listing.contains(menu.session))
        {
            self.menu = None;
        }
        self.keep_selection_drawn();
    }
}

/// One entry of the Sidebar's body, before a frame gives it anything to say.
#[derive(Clone, Debug)]
enum BodyEntry<'a> {
    Session(&'a SessionListItem, Standing),
    /// One Workspace the open selector offers.
    Scope(WorkspaceScope),
    Divider,
    /// The affordance closing a capped settled shelf, and how many rows acting
    /// on it brings up.
    ShowMore(usize),
}

/// Which of the Sidebar's two shelves a Session stands on, which is what
/// decides the shape of its row. A query puts the shelves themselves away but
/// not this: a result still reads as live work or as history.
#[derive(Clone, Copy, Debug)]
enum Standing {
    Active,
    Settled,
}

/// The settled shelf as the Sidebar draws it on one pass.
#[derive(Debug)]
struct DrawnShelf<'a> {
    /// The rows on show, top to bottom.
    on_show: Vec<&'a SessionListItem>,
    /// How many rows the affordance under them brings up, and `None` where the
    /// whole shelf is up and there is no affordance to draw.
    batch: Option<usize>,
}

/// What settles a Session, read at the moment the Sidebar lists one.
///
/// Two things settle one and only the first is written down. The reader's own
/// say-so is stamped on the Session by the server and always wins. Settling on
/// its own is derived here and nowhere else, from the Session's last activity
/// and the two auto-settle Settings: nothing is stored for it, no clock has to
/// fire for it, and work that moves is active again on the very next frame.
#[derive(Clone, Copy, Debug)]
struct Settlement {
    auto: AutoSettle,
    now: SessionTimestamp,
}

impl Settlement {
    /// Whether this Session stands on the settled shelf.
    fn settles(&self, session: &SessionListItem) -> bool {
        session.settled_at().is_some() || self.left_alone(session)
    }

    /// Whether this Session has been left alone long enough to settle itself.
    ///
    /// The idle is measured from the Session's last activity, so this asks
    /// after work there was: a Session nothing has moved since it was made has
    /// set nothing aside — the reader made it and it is theirs to prompt — and
    /// a Session Suru could not read has no activity it can see, which is the
    /// same reason it is never settled by the marker either.
    fn left_alone(&self, session: &SessionListItem) -> bool {
        let Some(idle) = self.auto.idle_millis() else {
            return false;
        };
        let last_activity = session.updated_at();
        if session.readable().is_none() || last_activity == session.created_at() {
            return false;
        }
        self.now.0.saturating_sub(last_activity.0) >= idle
    }
}

/// When a Session's work ended, which is both where it sits on the settled
/// shelf and what its row says. Both read this one function, so the order and
/// the label can never disagree.
///
/// Settling by the reader's say-so is stamped with the moment it happened.
/// Settling on its own carries no such stamp — nothing is stored for it at all
/// — so the reading falls back to when the Session was last active, which is
/// the moment the idle it settled for began.
fn ended_at(session: &SessionListItem) -> SessionTimestamp {
    session.settled_at().unwrap_or_else(|| session.updated_at())
}

/// Where a column holding `capacity` lines opens, given the entry it last
/// opened on and the entry the reader is now on. Entries are not all one
/// height — an active Session takes three lines, a settled one and the divider
/// take one — so the window is measured in lines and reported as the entry it
/// starts at. It holds still while the selection is inside it, is carried only
/// as far as the selection takes it, and never so far that it trails blank
/// lines below a list that has since grown shorter.
///
/// A reader standing on the selector above the list is on no entry of the body
/// at all, and the body holds where they left it: stepping off the top of a
/// list is not asking to be carried back to its head.
fn window_start(last: usize, selected: Option<usize>, heights: &[usize], capacity: usize) -> usize {
    let furthest = earliest_opening(heights, heights.len().saturating_sub(1), capacity);
    let Some(selected) = selected else {
        return last.min(furthest);
    };
    let earliest = earliest_opening(heights, selected, capacity);
    last.min(selected).min(furthest).max(earliest)
}

/// The earliest entry a column holding `capacity` lines can open on while
/// still showing every line of the entry at `last_shown`. An entry too tall
/// for the column at all is opened on regardless, because a column that showed
/// nothing would be worse than one that shows what it can.
fn earliest_opening(heights: &[usize], last_shown: usize, capacity: usize) -> usize {
    let mut used = 0;
    let mut opening = last_shown;
    for (index, height) in heights.iter().enumerate().take(last_shown + 1).rev() {
        used += height;
        if used > capacity {
            break;
        }
        opening = index;
    }
    opening
}

/// Where the Sidebar puts a reader with nowhere to stand — the row they were on
/// having gone, or their never having stood anywhere yet: the first row of the
/// list itself.
///
/// Never the selector above it or the affordance beside that, which are ways
/// of choosing what to list rather than places in the listing: a reader is put
/// on their work, and reaches either by asking for it. A Sidebar listing
/// nothing leaves them on nothing, and the arrows land them on the selector's
/// own line, which is all there is.
fn first_row(selectable: &[SidebarSelection]) -> Option<SidebarSelection> {
    selectable
        .iter()
        .find(|selection| {
            !matches!(
                selection,
                SidebarSelection::Selector | SidebarSelection::AddWorkspace
            )
        })
        .cloned()
}

/// Whether this Title carries the query the reader typed: a plain
/// case-insensitive substring, because they are searching by words they
/// remember rather than spelling out a pattern.
///
/// The session picker matches the same Titles more loosely, by subsequence,
/// and rightly: it is a jump-to that a reader opens, narrows, and closes in one
/// breath. The Sidebar is a list they read, and a match they cannot see the
/// reason for is a row standing in the way of the ones they wanted.
fn title_carries(query: &str, title: &str) -> bool {
    title.to_lowercase().contains(&query.to_lowercase())
}

/// The Workspace as a Sidebar row names it: its last path component, which is
/// the directory a reader thinks of the work as being in. A root path has no
/// such component, so it stands for itself.
pub(super) fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map_or_else(
            || workspace.as_os_str().to_string_lossy(),
            |name| name.to_string_lossy(),
        )
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        protocol::{
            AutoSettle, ModelAvailability, Session, SessionId, SessionListItem, SessionStatus,
            SessionSummary, SessionTimestamp, SidebarSettings, SidebarVisibility, Workspace,
        },
        tui::sidebar::{
            MINIMUM_MAIN_WIDTH, SIDEBAR_WIDTH, Sidebar, SidebarActivation, SidebarEntry,
            width_beside, workspace_name,
        },
    };

    #[test]
    fn the_initial_visibility_setting_has_its_say_once_and_the_toggle_has_it_after() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&launching(SidebarVisibility::Hidden));

        assert!(!sidebar.is_revealed());
        assert!(
            sidebar.take_listing_request().is_none(),
            "a Sidebar nobody can see asks for nothing"
        );

        sidebar.toggle();

        assert!(sidebar.is_revealed());
        assert!(sidebar.take_listing_request().is_some());

        sidebar.adopt_settings(&launching(SidebarVisibility::Hidden));

        assert!(
            sidebar.is_revealed(),
            "a later settings snapshot carries some other Setting's edit and leaves the reader's choice alone"
        );
    }

    #[test]
    fn a_sidebar_with_no_setting_in_hand_is_down_and_asks_for_nothing() {
        let mut sidebar = Sidebar::new(root());

        assert!(
            !sidebar.is_revealed(),
            "the initial-visibility Setting is what raises the Sidebar, so nothing is drawn before it lands"
        );
        assert!(sidebar.take_listing_request().is_none());
    }

    #[test]
    fn a_sidebar_seeded_shown_asks_for_its_sessions() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&launching(SidebarVisibility::Shown));

        assert!(sidebar.is_revealed());
        assert!(sidebar.take_listing_request().is_some());
        assert!(
            sidebar.take_listing_request().is_none(),
            "the request is handed over once, so nobody dispatches it twice"
        );
    }

    #[test]
    fn rows_are_ordered_by_creation_and_activity_never_moves_them() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&settling_nothing());
        let request = sidebar
            .take_listing_request()
            .expect("a revealed Sidebar asks for its Sessions");
        sidebar.load(
            &request,
            vec![
                summary("Oldest", 1, 90),
                summary("Newest", 3, 10),
                summary("Middle", 2, 50),
            ],
            None,
        );

        assert_eq!(drawn(&sidebar), vec!["Newest", "Middle", "Oldest"]);
    }

    #[test]
    fn the_toggle_takes_the_keys_and_the_initial_visibility_setting_leaves_them_alone() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&launching(SidebarVisibility::Shown));

        assert!(
            !sidebar.has_focus(),
            "a reader who has not touched the Sidebar is typing their first Prompt"
        );

        sidebar.toggle();
        assert!(
            !sidebar.has_focus(),
            "the toggle that closes it holds nothing"
        );

        sidebar.toggle();
        assert!(
            sidebar.has_focus(),
            "opening the Sidebar is the reader asking to drive it"
        );

        sidebar.leave();
        assert!(
            sidebar.is_revealed() && !sidebar.has_focus(),
            "Esc is done choosing, not done looking"
        );
    }

    #[test]
    fn a_sidebar_the_frame_could_not_draw_holds_no_keys() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&launching(SidebarVisibility::Hidden));
        sidebar.toggle();
        assert!(sidebar.has_focus());

        sidebar.forget_frame();

        assert!(
            !sidebar.has_focus(),
            "a Sidebar squeezed off a narrow terminal cannot act on the focus it keeps"
        );

        sidebar.record_drawn();

        assert!(
            sidebar.has_focus(),
            "widening the terminal gives the reader back the Sidebar they were driving"
        );
    }

    #[test]
    fn a_fresh_listing_starts_the_reader_on_the_session_they_have_open() {
        let mut sidebar = Sidebar::new(root());
        let open = SessionId::new();
        let listing = vec![summary("Newest", 3, 30), identified(open, "Open", 1)];

        sidebar.toggle();
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(&request, listing, Some(open));

        assert_eq!(selected(&sidebar), Some("Open"));
    }

    #[test]
    fn the_selection_follows_its_session_through_a_listing_that_lands_under_it() {
        let mut sidebar = Sidebar::new(root());
        let wanted = SessionId::new();

        sidebar.adopt_settings(&settling_nothing());
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(
            &request,
            vec![summary("Newest", 3, 30), identified(wanted, "Wanted", 1)],
            None,
        );
        sidebar.select_next();
        assert_eq!(selected(&sidebar), Some("Wanted"));

        sidebar.load(
            &request,
            vec![
                summary("Newer still", 4, 40),
                identified(wanted, "Wanted", 1),
            ],
            None,
        );

        assert_eq!(
            selected(&sidebar),
            Some("Wanted"),
            "a listing arriving underneath the reader leaves them on the work, not on the row"
        );
    }

    #[test]
    fn a_session_that_leaves_the_listing_takes_the_selection_off_it() {
        let mut sidebar = Sidebar::new(root());
        let doomed = SessionId::new();

        sidebar.toggle();
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(
            &request,
            vec![summary("Survivor", 2, 20), identified(doomed, "Doomed", 1)],
            None,
        );
        sidebar.select_next();
        sidebar.remove(doomed);

        assert_eq!(
            selected(&sidebar),
            Some("Survivor"),
            "a Session deleted elsewhere lands the reader back on a row that is there"
        );
    }

    #[test]
    fn enter_on_the_session_already_open_only_hands_the_keys_back() {
        let mut sidebar = Sidebar::new(root());
        let open = SessionId::new();

        sidebar.toggle();
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(&request, vec![identified(open, "Open", 1)], Some(open));

        assert_eq!(sidebar.activate(Some(open)), SidebarActivation::Answered);
        assert!(
            !sidebar.has_focus(),
            "the reader is already in this Session, so Enter means only that they are done"
        );
        assert!(!sidebar.is_attaching());
    }

    #[test]
    fn a_frame_too_narrow_for_a_usable_main_view_spares_no_columns() {
        assert_eq!(width_beside(SIDEBAR_WIDTH + MINIMUM_MAIN_WIDTH), Some(32));
        assert_eq!(width_beside(SIDEBAR_WIDTH + MINIMUM_MAIN_WIDTH - 1), None);
        assert_eq!(width_beside(0), None);
    }

    #[test]
    fn a_workspace_is_named_by_the_directory_the_work_is_in() {
        assert_eq!(workspace_name(&root().join("suru")), "suru");
        assert_eq!(
            workspace_name(&root()),
            root().as_os_str().to_string_lossy()
        );
    }

    #[test]
    fn the_settled_stand_below_the_divider_in_the_order_their_work_ended() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&settling_nothing());
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(
            &request,
            vec![
                set_aside("Ended first", 4, 90, 30),
                summary("Older, still going", 1, 20),
                set_aside("Ended last", 2, 10, 70),
                summary("Newer, still going", 3, 80),
            ],
            None,
        );

        assert_eq!(
            drawn(&sidebar),
            vec![
                "Newer, still going",
                "Older, still going",
                DIVIDER,
                "Ended last",
                "Ended first"
            ],
            "the active list keeps its creation order and the shelf takes the order work ended in"
        );
    }

    /// The threshold is a moment a Session reaches rather than one it has to
    /// pass, and it is read against the clock each time the Sidebar lists:
    /// nothing here was stored, and nobody said any of it.
    #[test]
    fn a_session_settles_itself_the_moment_its_idle_reaches_the_threshold() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Idle(1),
            ..SidebarSettings::default()
        });
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        let a_day = AutoSettle::Idle(1)
            .idle_millis()
            .expect("a threshold in days is a threshold");
        let now = SessionTimestamp::now().0;
        sidebar.load(
            &request,
            vec![
                summary("Reached it", 1, now - a_day),
                summary("A minute short of it", 2, now - a_day + 60_000),
            ],
            None,
        );

        assert_eq!(
            drawn(&sidebar),
            vec!["A minute short of it", DIVIDER, "Reached it"]
        );
    }

    #[test]
    fn a_column_windows_in_lines_because_the_two_shelves_are_not_one_height() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&settling_nothing());
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        sidebar.load(
            &request,
            vec![
                summary("Still going", 2, 20),
                set_aside("Ended last", 1, 10, 9),
                set_aside("Ended first", 3, 8, 7),
            ],
            None,
        );

        assert_eq!(
            drawn_within(&sidebar, 5),
            vec!["Still going", DIVIDER, "Ended last"],
            "three lines for the active Session, one for the divider, one for the shelf"
        );
        assert_eq!(
            drawn_within(&sidebar, 6),
            vec!["Still going", DIVIDER, "Ended last", "Ended first"],
            "a column measuring in rows would have wound past what the sixth line holds"
        );
        assert!(
            drawn_within(&sidebar, 2).is_empty(),
            "an entry the column cannot hold whole is left off rather than cut in half"
        );
    }

    #[test]
    fn the_settled_shelf_opens_on_ten_rows_and_offers_what_is_under_them() {
        let sidebar = showing(set_aside_shelf(12));

        let drawn = drawn(&sidebar);

        assert_eq!(
            drawn.len(),
            12,
            "the divider, ten rows of the shelf, and the affordance: {drawn:?}"
        );
        assert_eq!(drawn[0], DIVIDER);
        assert_eq!(drawn[10], "Settled 9", "the tenth row is the last on show");
        assert_eq!(
            drawn[11], "Show 2 more",
            "the affordance offers what is left rather than a batch that is not there"
        );
    }

    #[test]
    fn a_shelf_no_longer_than_it_opens_on_is_shown_whole_and_offers_nothing() {
        let sidebar = showing(set_aside_shelf(10));

        assert_eq!(
            drawn(&sidebar).len(),
            11,
            "the divider and every row of the shelf, with nothing left to ask for"
        );
    }

    #[test]
    fn the_affordance_brings_up_a_batch_at_a_time_to_the_end_of_the_shelf() {
        let mut sidebar = showing(set_aside_shelf(40));

        // Past the top of the list is the selector's own line — the affordance
        // beside it, then the selector — and past that the affordance at the
        // shelf's foot. None of them stands for a Session; which one the reader
        // is on is what activating says.
        sidebar.select_previous();
        sidebar.select_previous();
        sidebar.select_previous();
        assert_eq!(selected(&sidebar), None);

        assert_eq!(
            sidebar.activate(None),
            SidebarActivation::Answered,
            "asking for more of the shelf attaches nothing"
        );
        let revealed = drawn(&sidebar);
        assert_eq!(
            revealed.len(),
            37,
            "the divider, 35 rows, and the affordance"
        );
        assert_eq!(revealed[36], "Show 5 more");

        let _ = sidebar.activate(None);

        assert_eq!(
            drawn(&sidebar).len(),
            41,
            "the divider and the whole shelf, with no affordance left to draw"
        );
        assert_eq!(
            selected(&sidebar),
            Some("Settled 39"),
            "the affordance the reader was on is gone, so they land on the last row it uncovered"
        );
    }

    /// A reader reaches a Session from somewhere other than the shelf — the
    /// session picker, or the Session they had open when it settled — and the
    /// cap must not then hide the row they are on: a selection drawn nowhere
    /// is one the arrows cannot step off and Enter cannot act on.
    #[test]
    fn the_shelf_shows_the_row_the_reader_is_on_however_deep_it_sits() {
        let deep = SessionId::new();
        let mut shelf = set_aside_shelf(12);
        let SessionListItem::Readable(deepest) = &mut shelf[11] else {
            unreachable!("the fixture builds readable Sessions");
        };
        deepest.session.id = deep;

        let sidebar = showing_on(shelf, Some(deep));

        let drawn = drawn(&sidebar);
        assert_eq!(
            drawn[11], "Settled 11",
            "the row the reader is on stands at the foot of the shelf, on their account rather than its work's: {drawn:?}"
        );
        assert_eq!(
            drawn[12], "Show 1 more",
            "and is no longer one of the rows the affordance offers: {drawn:?}"
        );
        assert_eq!(
            selected(&sidebar),
            Some("Settled 11"),
            "so the reader can see the row they are on, and step off it"
        );
    }

    #[test]
    fn a_sidebar_asked_for_afresh_opens_the_shelf_on_its_first_rows_again() {
        let mut sidebar = showing(set_aside_shelf(12));
        // Up off the list onto the selector's own line — the add-Workspace
        // affordance, then the selector — and up again onto the affordance at
        // the shelf's foot.
        sidebar.select_previous();
        sidebar.select_previous();
        sidebar.select_previous();
        let _ = sidebar.activate(None);
        assert_eq!(drawn(&sidebar).len(), 13, "the whole shelf is on show");

        sidebar.toggle();
        sidebar.toggle();
        let request = sidebar
            .take_listing_request()
            .expect("a Sidebar coming back into view asks for its Sessions");
        sidebar.load(&request, set_aside_shelf(12), None);

        assert_eq!(
            drawn(&sidebar).last().map(String::as_str),
            Some("Show 2 more"),
            "the tail belongs to the listing it was revealed on, so a fresh one opens on the first rows"
        );
    }

    /// What stands in for the divider where the Titles the Sidebar draws are
    /// read out in order.
    const DIVIDER: &str = "<divider>";

    /// A shelf of Sessions the reader set aside, the first of them the most
    /// recently ended and so the nearest the divider.
    fn set_aside_shelf(count: u64) -> Vec<SessionListItem> {
        (0..count)
            .map(|ordinal| {
                set_aside(
                    &format!("Settled {ordinal}"),
                    ordinal + 1,
                    ordinal + 1,
                    count - ordinal,
                )
            })
            .collect()
    }

    #[test]
    fn live_work_ticks_only_while_the_sidebar_is_showing_it() {
        assert!(
            !showing(vec![summary("Idle", 1, 10)]).shows_live_work(),
            "a listing with nothing running animates nothing"
        );

        let mut sidebar = showing(vec![working("Working", 1, 10)]);

        assert!(sidebar.shows_live_work());

        sidebar.insert("nothing matches this");

        assert!(
            !sidebar.shows_live_work(),
            "a query the live row falls outside of takes it off screen with the rest"
        );

        sidebar.leave();

        assert!(
            sidebar.shows_live_work(),
            "and giving the query up puts it back"
        );

        sidebar.forget_frame();

        assert!(
            !sidebar.shows_live_work(),
            "a Sidebar the frame found no columns for animates nothing"
        );

        sidebar.record_drawn();
        sidebar.toggle();

        assert!(
            !sidebar.shows_live_work(),
            "and neither does one the reader closed"
        );
    }

    /// The catalog stream reporting a Turn starting and settling is what keeps
    /// the tick honest between listings: a row that settles takes the tick
    /// down with it, and one that starts working arms it, without waiting for
    /// the reader to ask for the listing again.
    #[test]
    fn a_working_change_arms_the_tick_and_the_turn_settling_drops_it() {
        let idle = SessionId::new();
        let mut sidebar = showing(vec![identified(idle, "Quiet", 1)]);
        assert!(!sidebar.shows_live_work());

        sidebar.set_working(idle, Some(SessionTimestamp(5)));

        assert!(
            sidebar.shows_live_work(),
            "a Turn starting in a listed Session arms the tick its Working duration rises on"
        );

        sidebar.set_working(idle, None);

        assert!(
            !sidebar.shows_live_work(),
            "and the last live row settling stands the tick down, so an idle \
             TUI schedules zero wakeups again"
        );
    }

    /// A Sidebar open on the Sessions given, with nothing settling itself, so
    /// the shelf holds what the listing marked settled and no more.
    fn showing(sessions: Vec<SessionListItem>) -> Sidebar {
        showing_on(sessions, None)
    }

    /// The same, for a reader who has one of those Sessions open.
    fn showing_on(sessions: Vec<SessionListItem>, current: Option<SessionId>) -> Sidebar {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&settling_nothing());
        let request = sidebar
            .take_listing_request()
            .expect("a revealed Sidebar asks for its Sessions");
        sidebar.load(&request, sessions, current);
        sidebar
    }

    /// The Sidebar's whole body, top to bottom, as the Titles it draws — and,
    /// for the rows standing for no Session, what they say instead.
    fn drawn(sidebar: &Sidebar) -> Vec<String> {
        entry_titles(sidebar.entries(None))
    }

    /// The Sidebar's body as a column `capacity` lines tall shows it.
    fn drawn_within(sidebar: &Sidebar, capacity: usize) -> Vec<String> {
        entry_titles(sidebar.visible_entries(capacity, None))
    }

    fn entry_titles(entries: Vec<SidebarEntry<'_>>) -> Vec<String> {
        entries
            .into_iter()
            .map(|entry| match entry {
                SidebarEntry::Row(row) => row.title.to_owned(),
                SidebarEntry::Divider => DIVIDER.to_owned(),
                SidebarEntry::ShowMore(more) => format!("Show {} more", more.count),
                SidebarEntry::Scope(scope) => scope.label,
            })
            .collect()
    }

    /// The Title of the row the reader is on, where they are on a Session. The
    /// settled shelf's affordance stands for none, so a reader on it is on no
    /// Title at all.
    fn selected(sidebar: &Sidebar) -> Option<&str> {
        sidebar
            .entries(None)
            .into_iter()
            .find_map(|entry| match entry {
                SidebarEntry::Row(row) if row.selected => Some(row.title),
                _ => None,
            })
    }

    fn identified(session_id: SessionId, title: &str, created_at: u64) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, created_at, created_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.session.id = session_id;
        SessionListItem::Readable(listed)
    }

    /// A Session the reader has set aside as done for now.
    fn set_aside(
        title: &str,
        created_at: u64,
        updated_at: u64,
        settled_at: u64,
    ) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, created_at, updated_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.settled_at = Some(SessionTimestamp(settled_at));
        SessionListItem::Readable(listed)
    }

    /// A listed Session whose latest Turn began and has not Settled, which is
    /// what a listing reports of a Session running work now.
    fn working(title: &str, created_at: u64, updated_at: u64) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, created_at, updated_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.session.status = SessionStatus::Active;
        listed.working_since = Some(SessionTimestamp(updated_at));
        SessionListItem::Readable(listed)
    }

    fn summary(title: &str, created_at: u64, updated_at: u64) -> SessionListItem {
        SessionListItem::Readable(SessionSummary {
            session: Session {
                id: SessionId::new(),
                workspace: Workspace {
                    path: root().join("workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
                parent: None,
            },
            title: title.to_owned(),
            emoji: None,
            settled_at: None,
            working_since: None,
            created_at: SessionTimestamp(created_at),
            updated_at: SessionTimestamp(updated_at),
        })
    }

    /// The Sidebar's Settings as a TUI launching under `initial_visibility`
    /// takes them, everything else left where its built-in default is.
    fn launching(initial_visibility: SidebarVisibility) -> SidebarSettings {
        SidebarSettings {
            initial_visibility,
            ..SidebarSettings::default()
        }
    }

    /// The Sidebar shown with nothing settling itself, which is what a test
    /// about the order or the shape of the list asks for: its fixtures stamp
    /// Sessions with ordinals rather than with moments, and every one of those
    /// reads as work left alone since the epoch.
    fn settling_nothing() -> SidebarSettings {
        SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Off,
            ..SidebarSettings::default()
        }
    }

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
