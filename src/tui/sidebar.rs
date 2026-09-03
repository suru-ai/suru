//! The Sidebar: the collapsible column beside the main view listing Sessions.

use std::{
    cell::{Cell, RefCell},
    cmp::Reverse,
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
};

use ratatui::layout::Position;

use crate::protocol::{
    AutoSettle, EffectiveSettings, EmojiVisibility, Outlook, Remote, RemoteStatus,
    ResolveWorkspaceRequest, SessionId, SessionListItem, SessionReference, SessionTimestamp,
    SidebarScope as InitialSidebarScope, SidebarVisibility,
};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface,
    commands::{SemanticCommandId, SemanticInvocation},
    session_listing::{ListedSession, SessionListing},
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

/// What the selector calls every Workspace on the current Outlook, as distinct
/// from Everywhere's wider set of Origin servers.
const ALL_WORKSPACES: &str = "All Workspaces";
const EVERYWHERE: &str = "Everywhere";

/// The affordance beside the selector, opening the path entry a reader names a
/// Workspace in. It shares the selector's line, so it is drawn — and pressed —
/// within its own columns of it.
pub(super) const ADD_WORKSPACE: &str = " + ";

/// What the path entry says when the reader offers it nothing.
const NAME_A_DIRECTORY: &str = "Name a directory";

/// The population the Sidebar draws: every reachable Origin, every Workspace
/// on the Outlook, or one Workspace on it.
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
pub(super) enum SidebarListingScope {
    Everywhere,
    AllWorkspaces,
    Workspace(PathBuf),
}

impl SidebarListingScope {
    /// What the selector calls this scope: the Workspace by the name a row
    /// gives it, or the words for all of them.
    fn label(&self) -> String {
        match self {
            Self::Everywhere => EVERYWHERE.to_owned(),
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
            Self::Everywhere | Self::AllWorkspaces => true,
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
    /// Whether a row draws the Emoji derived beside its Session's Title, which
    /// governs every frame from the moment the Setting lands.
    emoji: EmojiVisibility,
    /// The Session population the Sidebar draws, seeded once from the
    /// initial-scope Setting and moved by the selector afterwards.
    scope: SidebarListingScope,
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
    /// The Origins participating in Everywhere, local first and followed by
    /// each paired non-terminal Remote in the order the local Server named it.
    everywhere_origins: Vec<Outlook>,
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
    /// by position, so a listing arriving underneath them leaves the focus
    /// where the work is rather than where the row was.
    ///
    /// This is row focus and nothing else: it says what Enter would act on,
    /// and it exists only while the Sidebar has the keys. Entering seeds it,
    /// leaving gives it up, and nothing here says which Session the main view
    /// is showing — that is the open Session, which the route decides and the
    /// Sidebar is only told.
    focus: Option<SidebarFocus>,
    attaching: Option<SessionReference>,
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
    awaiting_dispatch: Vec<SessionListRequest>,
    /// Sequence used to supersede paired-Remote discovery when Everywhere is
    /// chosen again before an older answer lands.
    everywhere_remote_sequence: u64,
    /// Whether the current discovery request still needs handing to the run
    /// loop. Its identity remains in `pending_everywhere_remotes` afterwards
    /// so only that request's answer can alter the merged listing.
    everywhere_remote_dispatch_pending: bool,
    pending_everywhere_remotes: Option<u64>,
    /// Whether the last frame had the columns to draw the Sidebar. Only a
    /// frame can answer that, so each one records it here and input routing
    /// reads it back: a Sidebar squeezed off a narrow terminal keeps the
    /// reader's focus but cannot act on it.
    on_screen: Cell<bool>,
    /// The entry the column's window opens on. Only a frame knows how many
    /// lines it holds, so the window is settled at draw time and remembered
    /// here: an anchor moving to a row already in view leaves it where it
    /// is, and only an anchor moving out of view carries it along.
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
    deleting: Option<SessionReference>,
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

/// What the reader's keys are on in the Sidebar. Almost always that is a
/// Session, named by its id so the focus follows the work rather than the row
/// it happened to be drawn on. The rest are the Sidebar's own affordances,
/// which stand for no Session at all: the settled shelf's next batch, the Workspace
/// selector above the list, and — while that stands open — one of the
/// Workspaces it offers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarFocus {
    /// The Workspace selector's own row, which stands above the list rather
    /// than in it, and which Enter opens instead of attaching anything.
    Selector,
    /// One entry of the open selector, named by the scope it stands for.
    Scope(SidebarListingScope),
    /// The affordance beside the selector, which Enter opens a path entry from
    /// rather than attaching anything.
    AddWorkspace,
    Session(SessionReference),
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
    rejection: Option<String>,
}

/// The path entry as a frame draws it.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarWorkspaceEntryView<'a> {
    pub(super) path: &'a str,
    pub(super) rejection: Option<&'a str>,
}

/// One Session as the Sidebar draws it. The shelf it stands on decides its
/// shape, so a row carries its shelf alongside what every row says.
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarRow<'a> {
    /// The Session this row stands for, which is what a frame records against
    /// the screen rows it draws so a press lands on the work rather than on
    /// the position.
    pub(super) reference: &'a SessionReference,
    /// The Emoji this row draws for its Session: none where the derivation
    /// left it none, and none while the reader keeps Emojis hidden.
    pub(super) emoji: Option<&'a str>,
    pub(super) title: &'a str,
    /// The dim Origin tag following a foreign row's Title. Local rows carry
    /// none so the ordinary one-machine reading stays quiet.
    pub(super) remote: Option<&'a str>,
    /// Whether this is the Session the main view is showing. It is true
    /// whoever holds the keys, because it says what the reader is looking at
    /// rather than what they are choosing.
    pub(super) open: bool,
    /// Whether this is the row the keys are on, which is the one Enter acts
    /// on. It is drawn only while the Sidebar has them: a Sidebar that has
    /// given the keys up is pointing at nothing.
    pub(super) focused: bool,
    /// Whether this Session is one the client could not read. Such a row is
    /// drawn subdued and marked, because it stands for work the reader can see
    /// and delete but never open.
    pub(super) unreadable: bool,
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
    /// Whether this is the scope the Sidebar is narrowed to, accented because
    /// it is where the reader already is.
    pub(super) chosen: bool,
    pub(super) focused: bool,
    /// The scope pressing this entry asks for.
    scope: SidebarListingScope,
}

/// The Workspace selector as a frame draws it, standing between the search box
/// and the list it governs.
#[derive(Clone, Debug)]
pub(super) struct SidebarSelectorView {
    /// What the Sidebar is narrowed to.
    pub(super) label: String,
    /// Whether the entries stand open beneath it.
    pub(super) open: bool,
    pub(super) focused: bool,
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
    /// Whether this is the row the keys are on. The affordance is a row like
    /// any other in that respect: the arrows land on it and Enter acts on it.
    pub(super) focused: bool,
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

    /// Whether this entry stands for the Session the main view has open, which
    /// only a Session row ever does.
    const fn is_open(&self) -> bool {
        match self {
            Self::Row(row) => row.open,
            Self::Scope(_) | Self::Divider | Self::ShowMore(_) => false,
        }
    }

    /// Whether this is the entry the keys are on. The divider never is: it
    /// is a rule rather than a row, so the arrows step over it.
    const fn is_focused(&self) -> bool {
        match self {
            Self::Row(row) => row.focused,
            Self::ShowMore(more) => more.focused,
            Self::Scope(scope) => scope.focused,
            Self::Divider => false,
        }
    }

    /// What pressing this entry asks for, and `None` for the divider, which is
    /// a rule rather than a row and so answers no press.
    pub(super) fn target(&self) -> Option<SidebarTarget> {
        match self {
            Self::Row(row) => Some(SidebarTarget::Session(row.reference.clone())),
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
    Session(SessionReference),
    ShowMore,
    Selector,
    Scope(SidebarListingScope),
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
    ListEverywhereRemotes,
    /// Narrowing from Everywhere changes which catalog streams the Client
    /// owns even though the listing already in hand needs no fresh request.
    CatalogOriginsChanged,
    Attach(SessionReference),
    /// The reader named a directory to work in. It is the client's current
    /// Workspace from here: the root of the Sessions they make next, and what
    /// current-Workspace scope comes to mean.
    ResolveWorkspace(ResolveWorkspaceRequest),
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

/// The context menu a reader opened on one Sidebar row.
#[derive(Clone, Debug)]
struct SidebarMenu {
    /// The Session the menu stands on, named rather than positioned so a
    /// listing arriving underneath it acts on the work the reader pointed at.
    session: SessionReference,
    /// Whether that row stands on the settled shelf, read when the menu was
    /// opened, which is what decides whether it offers to set the Session
    /// aside or to bring it back.
    settled: bool,
    /// Whether the client could not read the Session, read when the menu was
    /// opened. Such a row keeps Delete and loses the rest: settling work that
    /// cannot be opened is an operation that cannot succeed, and the menu does
    /// not offer it.
    unreadable: bool,
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
    /// What the menu offers, top to bottom: what the row's shelf asks for —
    /// except on a row the client could not read, which no shelf operation can
    /// act on — and Delete, which every row keeps.
    fn items(&self) -> Vec<SidebarMenuItem> {
        if self.unreadable {
            return vec![SidebarMenuItem::Delete];
        }
        vec![
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
#[derive(Clone, Debug)]
pub(super) struct SidebarMenuView {
    pub(super) anchor: Position,
    pub(super) items: Vec<SidebarMenuEntry>,
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
            emoji: EmojiVisibility::default(),
            scope: SidebarListingScope::AllWorkspaces,
            selector_open: false,
            workspace_entry: None,
            listing: SessionListing::scoped(
                SessionListSurface::Sidebar,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            everywhere_origins: Vec::new(),
            settled_on_show: SETTLED_SHELF_OPENING,
            query: String::new(),
            focus: None,
            attaching: None,
            asked_afresh: false,
            awaiting_dispatch: Vec::new(),
            everywhere_remote_sequence: 0,
            everywhere_remote_dispatch_pending: false,
            pending_everywhere_remotes: None,
            // Until a frame says otherwise, which it does before anything the
            // reader types can reach a surface.
            on_screen: Cell::new(true),
            window_start: Cell::new(0),
            geometry: RefCell::default(),
            menu: None,
            deleting: None,
        }
    }

    /// Takes the Settings the Sidebar draws under, each on its own schedule:
    /// auto-settle and how a Session is named govern every frame from here on,
    /// while the two initial Settings have their say once and are then the
    /// reader's to overrule. Returns nothing: a Sidebar that wants its Sessions
    /// leaves the requests in [`Self::take_listing_requests`].
    pub(super) fn adopt_settings(&mut self, settings: &EffectiveSettings) {
        self.auto_settle = settings.sidebar.auto_settle;
        self.emoji = settings.session.title.emoji;
        if self.seeded {
            return;
        }
        self.seeded = true;
        self.scope = match settings.sidebar.initial_scope {
            InitialSidebarScope::AllWorkspaces => SidebarListingScope::AllWorkspaces,
            InitialSidebarScope::CurrentWorkspace => {
                SidebarListingScope::Workspace(self.listing.current_workspace().to_owned())
            }
        };
        self.reveal(settings.sidebar.initial_visibility == SidebarVisibility::Shown);
    }

    /// Shows the Sidebar, or hides it. This is view state and nothing more: the
    /// initial-visibility Setting is not rewritten.
    ///
    /// A reader who opens the Sidebar is asking to drive it, so it takes the
    /// keys — and with them the row focus that says what Enter would act on,
    /// seeded from the Session the main view has open. Closing hands both
    /// back. The initial-visibility Setting's own
    /// reveal in [`Self::adopt_settings`] does neither, because a reader who has not touched the
    /// Sidebar is typing their first Prompt.
    pub(super) fn toggle(&mut self, open: Option<&SessionReference>) {
        self.reveal(!self.revealed);
        if self.revealed {
            self.enter(open);
        } else {
            // Closing is one of the ways out of the Sidebar, so it leaves by
            // the same door the others do — and a Sidebar nobody can see is
            // not one holding a query on the reader's behalf.
            self.hand_back_keys();
        }
    }

    /// Gives the Sidebar the keys and puts row focus where the reader is: on
    /// the open Session where the column draws its row, and on the Workspace
    /// selector otherwise, so entering has a starting point without the
    /// Sidebar having to pretend some Session is selected.
    fn enter(&mut self, open: Option<&SessionReference>) {
        self.focused = true;
        self.seed_focus(open);
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
        let before = self.focus_order_before_change();
        self.query.push_str(text);
        self.keep_focus_drawn(&before);
    }

    /// Takes that line back a character, widening the results to match where
    /// it is the query.
    pub(super) fn delete_backward(&mut self) {
        if let Some(entry) = self.path_being_typed() {
            entry.path.pop();
            return;
        }
        let before = self.focus_order_before_change();
        self.query.pop();
        self.keep_focus_drawn(&before);
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
        let before = self.focus_order_before_change();
        self.query.clear();
        self.keep_focus_drawn(&before);
    }

    /// Hands the keys to the composer, and the query goes with them: it was a
    /// way of finding a Session, and the reader is no longer looking for one.
    /// So does any menu standing open: it was opened on a row the reader has
    /// since moved on from.
    ///
    /// Row focus goes too. It says what Enter would act on, and nothing here
    /// answers to Enter any more — a mark left standing would be the Sidebar
    /// claiming a Session the reader is not in.
    fn hand_back_keys(&mut self) {
        self.focused = false;
        self.menu = None;
        // A path entry is a line the reader was typing into, and they have
        // stopped typing.
        self.workspace_entry = None;
        self.close_selector();
        self.clear_query();
        // Last, because putting the selector's entries away and giving up the
        // query are both moves that would otherwise put focus somewhere.
        self.focus = None;
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
    /// takes row focus and is opened, or on one of the Sidebar's own
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
        // A row the client could not read spends the press without answering
        // it: opening the Session could only fail, and carrying row focus onto
        // a row the arrows cannot leave would strand it there.
        if let SidebarTarget::Session(reference) = &target
            && !self.is_readable(reference)
        {
            return SidebarPress::Answered;
        }
        self.focus_on(target);
        SidebarPress::Invoke(SemanticCommandId::SidebarAttach.into())
    }

    /// Whether this Session is one the listing in hand could read, which is
    /// what decides whether its row can be opened at all.
    fn is_readable(&self, reference: &SessionReference) -> bool {
        self.listing
            .sessions()
            .iter()
            .find(|session| session.reference() == reference)
            .and_then(|session| session.readable())
            .is_some()
    }

    /// Opens the context menu on the row the reader asked for one on, which
    /// also puts row focus on that row where the reader is driving the
    /// Sidebar: a menu acts on the Session under it, and focus standing
    /// elsewhere would say otherwise. A reader who is not driving it is shown
    /// no focus either way, and the menu carries the Session it acts on.
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
        let Some(SidebarTarget::Session(reference)) = hit else {
            return;
        };
        let Some(session) = self
            .listing
            .sessions()
            .iter()
            .find(|session| session.reference() == &reference)
        else {
            return;
        };
        let settled = self.settlement().settles(session);
        let unreadable = session.readable().is_none();
        // A row the client could not read still gets its menu — deletion is
        // how damaged work leaves the list — but never row focus, which stands
        // only on rows the arrows can reach and Enter can open.
        if !unreadable {
            self.focus_on(SidebarTarget::Session(reference.clone()));
            self.release_borrowed_focus();
        }
        self.menu = Some(SidebarMenu {
            session: reference,
            settled,
            unreadable,
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
        Some(SidebarMenuView {
            anchor: menu.anchor,
            items: menu
                .items()
                .into_iter()
                .enumerate()
                .map(|(index, item)| SidebarMenuEntry {
                    label: item.label(menu.confirming_delete),
                    selected: menu.selected == index,
                    destructive: item == SidebarMenuItem::Delete,
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
        let length = menu.items().len() as isize;
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
    /// Pointing at an item is choosing it, so the menu's own selection follows
    /// the press before the item acts. Settling and unsettling act at once and
    /// the menu is done; Delete asks again the first time and acts the second,
    /// so the menu stands until the reader has said it twice.
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
        let session = menu.session.clone();
        self.menu = None;
        SidebarPress::Invoke(item.command().on_session(session))
    }

    /// Notes the entry a press landed on, which is how a pointer says which
    /// entry the command it invokes should act on: activation reads row focus
    /// and nothing else, so the two gestures act through one path.
    ///
    /// A pointer only borrows focus this way. Row focus belongs to the keys,
    /// so a Sidebar that does not have them gives it back the moment the press
    /// has been answered — see [`Self::release_borrowed_focus`].
    fn focus_on(&mut self, target: SidebarTarget) {
        self.focus = Some(match target {
            SidebarTarget::Session(reference) => SidebarFocus::Session(reference),
            SidebarTarget::ShowMore => SidebarFocus::ShowMore,
            SidebarTarget::Selector => SidebarFocus::Selector,
            SidebarTarget::Scope(scope) => SidebarFocus::Scope(scope),
            SidebarTarget::AddWorkspace => SidebarFocus::AddWorkspace,
        });
    }

    /// Gives row focus back where a press borrowed it on a Sidebar the reader
    /// is not driving. Focus says where the keys are; a Sidebar that has not
    /// got them must point at nothing once the press it answered is done, or
    /// the reader would come back to a mark a pointer left rather than to the
    /// Session they have open.
    ///
    /// A press that takes the keys — the affordance opening a path entry is
    /// the one such — keeps what it set, because by then the reader is
    /// driving the Sidebar after all.
    fn release_borrowed_focus(&mut self) {
        if !self.focused {
            self.focus = None;
        }
    }

    /// Notes the Session the Sidebar has asked the server to take away, so a
    /// refusal is drawn here rather than by some other surface listing the
    /// same work. The listing's own complaint goes with it: the reader is
    /// being answered afresh.
    pub(super) fn begin_deletion(&mut self, reference: SessionReference) {
        self.deleting = Some(reference);
        self.listing.clear_error();
    }

    /// Takes the server's refusal to delete, where it was this Sidebar that
    /// asked. Answering `false` leaves the refusal for whichever surface did.
    pub(super) fn fail_deletion(&mut self, reference: &SessionReference, error: String) -> bool {
        if self.deleting.as_ref() != Some(reference) {
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
        if self.scope == SidebarListingScope::Everywhere {
            self.awaiting_dispatch.clear();
            self.everywhere_remote_sequence = self.everywhere_remote_sequence.wrapping_add(1);
            self.pending_everywhere_remotes = Some(self.everywhere_remote_sequence);
            self.everywhere_remote_dispatch_pending = true;
            return;
        }
        self.everywhere_remote_dispatch_pending = false;
        self.pending_everywhere_remotes = None;
        self.awaiting_dispatch = vec![self.listing.refresh()];
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
    /// Re-asks only the Origin whose catalog moved. Everywhere owns its
    /// interest independently of visibility, so a hidden Sidebar keeps the
    /// merged listing warm; an ordinary hidden Sidebar still refreshes when
    /// it is next revealed.
    pub(super) fn catch_up_origin(&mut self, outlook: Outlook) {
        let participates = if self.scope == SidebarListingScope::Everywhere {
            self.everywhere_origins.contains(&outlook)
        } else {
            outlook == *self.listing.outlook()
        };
        if !participates || (!self.revealed && self.scope != SidebarListingScope::Everywhere) {
            return;
        }
        self.asked_afresh = false;
        self.awaiting_dispatch = vec![self.listing.catch_up_origin(outlook)];
    }

    /// Whether a listing the server answered with would move anything the
    /// Sidebar draws, per [`SessionListing::would_move`].
    pub(super) fn would_move(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        if self.scope == SidebarListingScope::Everywhere {
            self.listing.would_move_across(request, sessions)
        } else {
            self.listing.would_move(request, sessions)
        }
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

    /// The listings the Sidebar is waiting on, handed over exactly once so the
    /// caller that can dispatch them does so and no later caller repeats them.
    pub(super) fn take_listing_requests(&mut self) -> Vec<SessionListRequest> {
        std::mem::take(&mut self.awaiting_dispatch)
    }

    #[cfg(test)]
    fn take_listing_request(&mut self) -> Option<SessionListRequest> {
        let requests = self.take_listing_requests();
        assert!(requests.len() <= 1, "expected at most one listing request");
        requests.into_iter().next()
    }

    pub(super) fn take_everywhere_remote_request(&mut self) -> Option<u64> {
        std::mem::take(&mut self.everywhere_remote_dispatch_pending)
            .then_some(self.pending_everywhere_remotes?)
    }

    /// Whether a paired-Remote reply still belongs to the scope on show.
    pub(super) fn accepts_everywhere_remotes(&self, request_id: u64) -> bool {
        self.scope == SidebarListingScope::Everywhere
            && self.pending_everywhere_remotes == Some(request_id)
    }

    /// Begins one fresh listing per reachable Origin from the durable Remote
    /// list returned by the local Server. Revoked and incompatible Pairings
    /// are terminal and contribute neither requests nor stale rows.
    pub(super) fn load_everywhere_remotes(
        &mut self,
        request_id: u64,
        remotes: Vec<Remote>,
    ) -> Option<Vec<SessionListRequest>> {
        if !self.accepts_everywhere_remotes(request_id) {
            return None;
        }
        self.pending_everywhere_remotes = None;
        let mut origins = vec![Outlook::Local];
        origins.extend(remotes.into_iter().filter_map(|remote| {
            (!matches!(
                remote.status,
                RemoteStatus::Revoked | RemoteStatus::ProtocolMismatch
            ))
            .then_some(Outlook::Remote(remote.name))
        }));
        self.listing.retain_origins(&origins);
        self.everywhere_origins = origins;
        Some(self.listing.refresh_origins(&self.everywhere_origins))
    }

    /// Remote streams the run loop must own for the chosen scope. The local
    /// Server's catalog is already carried by the ManagedClient itself.
    pub(super) fn catalog_origins(&self) -> HashSet<Outlook> {
        if self.scope == SidebarListingScope::Everywhere {
            return self
                .everywhere_origins
                .iter()
                .filter(|outlook| matches!(outlook, Outlook::Remote(_)))
                .cloned()
                .collect();
        }
        match self.listing.outlook() {
            Outlook::Local => HashSet::new(),
            outlook @ Outlook::Remote(_) => HashSet::from([outlook.clone()]),
        }
    }

    pub(super) fn includes_origin(&self, outlook: &Outlook) -> bool {
        self.scope == SidebarListingScope::Everywhere && self.everywhere_origins.contains(outlook)
    }

    pub(super) fn fail_everywhere_remotes(&mut self, request_id: u64, error: String) -> bool {
        if !self.accepts_everywhere_remotes(request_id) {
            return false;
        }
        self.pending_everywhere_remotes = None;
        self.listing.report_error(error);
        true
    }

    /// Takes the Sessions the server answered with, told which one the main
    /// view has open so a fresh listing can seed row focus the way entering
    /// the Sidebar does.
    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        sessions: Vec<SessionListItem>,
        open: Option<&SessionReference>,
    ) {
        let before = self.focus_order_before_change();
        let awaited = self.listing.awaits(request);
        let shown = self.scope == SidebarListingScope::Everywhere
            || request.outlook() == self.listing.outlook();
        self.listing.load(request, sessions);
        if !awaited || !shown {
            return;
        }
        if std::mem::take(&mut self.asked_afresh) {
            self.attaching = None;
            // A menu stands on one row of the listing it was opened over. A
            // listing the reader asked for is them looking again, so it is put
            // away rather than left pointing at whatever now stands where its
            // row did.
            self.menu = None;
            // A listing the reader asked for by opening the Sidebar is the
            // answer to that opening, and it lands after the seeding did: the
            // rows the Sidebar would have started them on were not there yet.
            // So row focus is seeded again — where the reader is driving the
            // column. A Sidebar revealed by its Setting is driving nothing and
            // is left pointing at nothing.
            if self.focused {
                self.seed_focus(open);
            }
            return;
        }
        // A catch-up leaves the reader in whatever they were in the middle
        // of, so long as the Session behind it survived the listing that
        // arrived.
        self.forget_absent(&before);
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        self.listing.fail(request, error);
    }

    pub(super) fn retitle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        title: String,
        emoji: Option<String>,
    ) {
        let before = self.focus_order_before_change();
        self.listing
            .retitle_origin(outlook, session_id, title, emoji);
        // A Title is what a query is read against, so another client's retitle
        // can carry the row the keys are on out of the results under them.
        self.keep_focus_drawn(&before);
    }

    pub(super) fn settle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        settled_at: Option<SessionTimestamp>,
    ) {
        self.listing.settle_origin(outlook, session_id, settled_at);
    }

    /// Takes a Turn the server reports starting or settling into the listing
    /// in hand, so the row's Working label — and the tick
    /// [`Self::shows_live_work`] arms off it — is true between listings.
    pub(super) fn set_working_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    ) {
        self.listing
            .set_working_origin(outlook, session_id, working_since);
    }

    pub(super) fn remove_origin(&mut self, outlook: Outlook, session_id: SessionId) {
        let before = self.focus_order_before_change();
        self.listing.remove_origin(outlook, session_id);
        self.forget_absent(&before);
    }

    pub(super) fn retain_origin_catalog(&mut self, outlook: Outlook, session_ids: &[SessionId]) {
        let before = self.focus_order_before_change();
        self.listing.retain_origin(outlook, session_ids);
        self.forget_absent(&before);
    }

    /// Moves the reader one row up the list, wrapping past the top.
    pub(super) fn focus_previous(&mut self) {
        self.move_focus(-1);
    }

    /// Moves the reader one row down the list, wrapping past the end.
    pub(super) fn focus_next(&mut self) {
        self.move_focus(1);
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
    pub(super) fn activate(
        &mut self,
        open: Option<&SessionReference>,
        retry_open: bool,
    ) -> SidebarActivation {
        let activation = self.act_on_focus(open, retry_open);
        self.release_borrowed_focus();
        activation
    }

    /// What acting on the entry row focus stands over comes to, which is the
    /// whole of [`Self::activate`] but the borrowed-focus bookkeeping around
    /// it.
    fn act_on_focus(
        &mut self,
        open: Option<&SessionReference>,
        retry_open: bool,
    ) -> SidebarActivation {
        if self.workspace_entry.is_some() {
            return self.offer_workspace();
        }
        let Some(focus) = self.focus.clone() else {
            return SidebarActivation::Answered;
        };
        let wanted = match focus {
            SidebarFocus::Session(reference) => reference,
            SidebarFocus::ShowMore => {
                self.show_more();
                return SidebarActivation::Answered;
            }
            SidebarFocus::Selector => {
                self.toggle_selector();
                return SidebarActivation::Answered;
            }
            SidebarFocus::Scope(scope) => {
                let enters_everywhere = scope == SidebarListingScope::Everywhere
                    && self.scope != SidebarListingScope::Everywhere;
                let leaves_everywhere = self.scope == SidebarListingScope::Everywhere
                    && scope != SidebarListingScope::Everywhere;
                self.choose_scope(scope);
                if enters_everywhere {
                    self.ask_for_sessions();
                    return SidebarActivation::ListEverywhereRemotes;
                }
                if leaves_everywhere {
                    return SidebarActivation::CatalogOriginsChanged;
                }
                return SidebarActivation::Answered;
            }
            SidebarFocus::AddWorkspace => {
                self.open_workspace_entry();
                return SidebarActivation::Answered;
            }
        };
        if !self.is_readable(&wanted) {
            return SidebarActivation::Answered;
        }
        if open == Some(&wanted) && !retry_open {
            self.hand_back_keys();
            return SidebarActivation::Answered;
        }
        self.listing.clear_error();
        self.attaching = Some(wanted.clone());
        // The reader is done choosing the moment they choose: opening is
        // optimistic, so the keys go to the Session's composer now rather
        // than when its snapshot lands.
        self.hand_back_keys();
        SidebarActivation::Attach(wanted)
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
        self.focus = Some(SidebarFocus::AddWorkspace);
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
        SidebarActivation::ResolveWorkspace(ResolveWorkspaceRequest {
            base: Some(self.listing.current_workspace().to_owned()),
            path: PathBuf::from(named),
        })
    }

    /// Draws the refusal under the entry, leaving it open on the path that
    /// earned it.
    fn refuse_workspace(&mut self, rejection: impl Into<String>) -> SidebarActivation {
        if let Some(entry) = &mut self.workspace_entry {
            entry.rejection = Some(rejection.into());
        }
        SidebarActivation::Answered
    }

    pub(super) fn accept_workspace(&mut self, workspace: PathBuf) -> SidebarActivation {
        let left_everywhere = self.scope == SidebarListingScope::Everywhere;
        self.hand_back_keys();
        self.adopt_workspace(workspace.clone());
        self.narrow_to_workspace(workspace);
        if left_everywhere {
            SidebarActivation::CatalogOriginsChanged
        } else {
            SidebarActivation::Answered
        }
    }

    pub(super) fn fail_workspace_resolution(&mut self, error: String) {
        let _ = self.refuse_workspace(error);
    }

    /// Takes the Workspace this client has moved to, however it moved.
    ///
    /// Only what "where I am" means moves with it: the selector's entries and
    /// a scope narrowed to the current Workspace are derived from the
    /// listing's reading of it. The scope the reader chose is left exactly
    /// where they put it — switching Workspaces is navigation, and narrowing
    /// the column is a view they configured — so re-pointing it is the
    /// separate act [`Self::narrow_to_workspace`] is for.
    pub(super) fn adopt_workspace(&mut self, workspace: PathBuf) {
        self.listing.adopt_current_workspace(workspace);
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.listing.adopt_outlook(outlook);
        self.query.clear();
        self.focus = None;
        self.attaching = None;
        self.deleting = None;
        self.menu = None;
        self.workspace_entry = None;
        self.awaiting_dispatch.clear();
        self.everywhere_remote_dispatch_pending = false;
        self.pending_everywhere_remotes = None;
        if self.revealed {
            self.ask_for_sessions();
        }
    }

    /// Re-asks after a newly chosen Outlook has named its canonical Workspace.
    /// The first ask may have used the temporary `.` reading while the Remote
    /// resolved it, so a visible Sidebar must replace that answer.
    pub(super) fn refresh_after_outlook_workspace(&mut self) {
        if self.revealed {
            self.ask_for_sessions();
        }
    }

    /// Narrows the Sidebar to the Workspace the reader named at its own path
    /// entry, which is the one switch that re-points the column with it: a
    /// reader who has just said where they work, in the Sidebar, is saying
    /// which work they mean. The settled shelf opens on its first rows again
    /// as it does under any other narrowing, and nothing is asked of the
    /// server — the listing is the whole body of work either way.
    ///
    /// Nothing is left marked. Taking the Workspace handed the keys back to
    /// the composer, and row focus went with them: what the column says now is
    /// which Session is open and which Workspace it is narrowed to, neither of
    /// which is a claim about where Enter would land.
    pub(super) fn narrow_to_workspace(&mut self, workspace: PathBuf) {
        self.choose_scope(SidebarListingScope::Workspace(workspace));
    }

    /// The path entry as a frame draws it, and `None` where there is none to
    /// draw.
    pub(super) fn workspace_entry(&self) -> Option<SidebarWorkspaceEntryView<'_>> {
        self.workspace_entry
            .as_ref()
            .map(|entry| SidebarWorkspaceEntryView {
                path: &entry.path,
                rejection: entry.rejection.as_deref(),
            })
    }

    /// Brings up the next of the settled shelf. Where that was the whole of
    /// what was left, the affordance the reader was standing on goes with it,
    /// so they land on the last row it brought up rather than on nothing.
    fn show_more(&mut self) {
        self.settled_on_show = self.settled_on_show.saturating_add(SETTLED_SHELF_BATCH);
        let focusable = self.focusable();
        if !focusable.contains(&SidebarFocus::ShowMore) {
            self.focus = focusable.last().cloned();
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
        self.focus = Some(SidebarFocus::Scope(self.scope.clone()));
    }

    /// Puts the entries away, leaving the scope where it was and the reader on
    /// the selector that opened them.
    fn close_selector(&mut self) {
        if !self.selector_open {
            return;
        }
        self.selector_open = false;
        self.focus = Some(SidebarFocus::Selector);
    }

    /// Narrows the Sidebar to one Workspace, or widens it to all of them.
    ///
    /// Nothing is asked of the server: the listing is the whole body of work
    /// either way, and the scope decides which of it the shelves draw. The
    /// settled shelf opens on its first rows again, because how deep a reader
    /// walked into one Workspace's history says nothing about another's.
    fn choose_scope(&mut self, scope: SidebarListingScope) {
        self.close_selector();
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        if self.scope != SidebarListingScope::Everywhere {
            self.everywhere_remote_dispatch_pending = false;
            self.pending_everywhere_remotes = None;
        }
        self.settled_on_show = SETTLED_SHELF_OPENING;
    }

    /// The selector as a frame draws it.
    pub(super) fn selector(&self) -> SidebarSelectorView {
        SidebarSelectorView {
            label: self.scope.label(),
            open: self.selector_open,
            focused: self.focus == Some(SidebarFocus::Selector),
            adding: self.focus == Some(SidebarFocus::AddWorkspace),
        }
    }

    /// Whether the Sidebar is answering for one Workspace rather than for the
    /// reader's whole body of work, which is what an empty column means by
    /// nothing being here.
    pub(super) fn is_narrowed(&self) -> bool {
        matches!(self.scope, SidebarListingScope::Workspace(_))
    }

    pub(super) fn attaching_to(&self, reference: &SessionReference) -> bool {
        self.attaching.as_ref() == Some(reference)
    }

    pub(super) const fn is_attaching(&self) -> bool {
        self.attaching.is_some()
    }

    /// The Session the Sidebar asked for is on screen. The keys went to its
    /// composer when the reader chose it, so nothing moves here — and a
    /// reader who has since come back to the column keeps them.
    pub(super) fn finish_attachment(&mut self) {
        self.attaching = None;
    }

    /// The client left the Session the Sidebar was opening, for the Landing or
    /// for another Workspace. The attachment behind it has been let go of, so
    /// the Sidebar stops waiting on an answer that is never coming, without
    /// reporting a refusal that never happened. Turning toward another Outlook
    /// leaves it through [`Self::adopt_outlook`], which puts the whole column
    /// down rather than only what it was opening.
    pub(super) fn abandon_attachment(&mut self) {
        self.attaching = None;
    }

    /// The server refused the attachment. The reader keeps the keys and the
    /// list; the open shell owns the refusal because it is where the reader
    /// went, while the listing remains a valid route back to a retry.
    pub(super) fn fail_attachment(&mut self) {
        self.attaching = None;
    }

    pub(super) fn is_loading(&self) -> bool {
        if self.scope == SidebarListingScope::Everywhere {
            self.pending_everywhere_remotes.is_some()
                || self.listing.is_loading_across(&self.everywhere_origins)
        } else {
            self.listing.is_loading()
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    /// What a column this many lines tall shows: a list longer than the Sidebar
    /// is read through a window rather than being crammed into the lines
    /// available. The window moves only as far as it must to keep the row it
    /// is anchored on in view, so moving within it leaves every other row
    /// where the reader last saw it, and an entry the last line cannot hold
    /// whole is left off rather than cut in half.
    ///
    /// The anchor is the row the keys are on while the Sidebar has them, and
    /// the open Session's row otherwise: a column standing beside the Session
    /// it lists opens on that Session rather than wherever the reader last
    /// scrolled to. Neither anchor changes what the body holds, so a Session
    /// with no row simply leaves the window where it was.
    pub(super) fn visible_entries(
        &self,
        capacity: usize,
        open: Option<&SessionReference>,
    ) -> Vec<SidebarEntry<'_>> {
        let entries = self.entries(open);
        let heights = entries.iter().map(SidebarEntry::lines).collect::<Vec<_>>();
        let anchor = entries
            .iter()
            .position(SidebarEntry::is_focused)
            .or_else(|| entries.iter().position(SidebarEntry::is_open));
        let start = window_start(self.window_start.get(), anchor, &heights, capacity);
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
    fn entries(&self, open: Option<&SessionReference>) -> Vec<SidebarEntry<'_>> {
        self.body()
            .into_iter()
            .map(|entry| match entry {
                BodyEntry::Divider => SidebarEntry::Divider,
                BodyEntry::Scope(scope) => SidebarEntry::Scope(SidebarScopeEntry {
                    label: scope.label(),
                    chosen: scope == self.scope,
                    focused: self.focus == Some(SidebarFocus::Scope(scope.clone())),
                    scope,
                }),
                BodyEntry::ShowMore(count) => SidebarEntry::ShowMore(SidebarShowMore {
                    count,
                    focused: self.focus == Some(SidebarFocus::ShowMore),
                }),
                BodyEntry::Session(session, Standing::Active) => self.row(
                    session,
                    open,
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
                    open,
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

    /// The selector offers Everywhere first, then every Workspace on the
    /// Outlook collectively, then each such Workspace the listing has work
    /// rooted in and the one this client itself runs in — which stands whether
    /// or not there is work in it yet, being where the next Session will be.
    ///
    /// They are read off the whole listing rather than off the Sessions in
    /// scope, so narrowing to one Workspace never takes the others off the
    /// selector: a reader who narrowed has to be able to widen again, and to
    /// step straight across to a third.
    fn scopes(&self) -> Vec<SidebarListingScope> {
        let mut workspaces = self.listing.workspaces();
        // Ordered by path, so the entries hold their places between one
        // listing and the next. The Workspace Picker orders the same
        // population by which held work most recently; the divergence is
        // deliberate — a persistent list wants entries that stay put, a
        // choose-and-dismiss picker wants the likeliest target near the top.
        workspaces.sort_unstable();
        [
            SidebarListingScope::Everywhere,
            SidebarListingScope::AllWorkspaces,
        ]
        .into_iter()
        .chain(workspaces.into_iter().map(SidebarListingScope::Workspace))
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
        // The row the keys are on stands whatever the cap says: focus that
        // moved onto a row the shelf has since capped away would be one the
        // arrows cannot step off and Enter cannot act on. It stands at the
        // foot of the shelf rather than in the order the rest keep, because it
        // is there on the reader's account rather than on its work's.
        //
        // Only focus earns this. The open Session does not: the shelf's cap is
        // the reader's own reading of their history, and opening a Session
        // deep in it must not quietly grow the shelf to say so.
        if let Some(SidebarFocus::Session(focused)) = &self.focus
            && let Some(deeper) = settled
                .iter()
                .skip(self.settled_on_show)
                .find(|session| session.reference() == focused)
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
        session: &'a ListedSession,
        open: Option<&SessionReference>,
        shelf: SidebarShelf<'a>,
    ) -> SidebarEntry<'a> {
        SidebarEntry::Row(SidebarRow {
            reference: session.reference(),
            emoji: self.emoji.drawn_emoji(session.emoji()),
            title: session.title(),
            remote: if self.scope == SidebarListingScope::Everywhere {
                session.reference().origin.remote_name()
            } else {
                None
            },
            open: open == Some(session.reference()),
            focused: self.focus.as_ref()
                == Some(&SidebarFocus::Session(session.reference().clone())),
            unreadable: session.readable().is_none(),
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
    fn active(&self, settlement: Settlement) -> Vec<&ListedSession> {
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
    fn settled(&self, settlement: Settlement) -> Vec<&ListedSession> {
        let mut sessions = self
            .in_scope()
            .filter(|session| settlement.settles(session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(ended_at(session)));
        sessions
    }

    /// The Sessions the selector's scope draws, which is every one the listing
    /// holds until the reader narrows to a Workspace.
    fn in_scope(&self) -> impl Iterator<Item = &ListedSession> {
        let sessions = if self.scope == SidebarListingScope::Everywhere {
            self.listing.sessions_across(&self.everywhere_origins)
        } else {
            self.listing.sessions().iter().collect()
        };
        sessions
            .into_iter()
            .filter(|session| self.scope.holds(session))
    }

    /// Every row the reader can be on, in the order the Sidebar draws them,
    /// which is the order the arrows walk: the selector standing above the
    /// list and the affordance sharing its line, left to right, then the body
    /// without the divider, which is a rule rather than a row. A Session the
    /// client could not read is not among them either: every row the arrows
    /// can land on is one Enter could open, and its drawn place in the list
    /// is what the arrows step over.
    ///
    /// The selector is not among them while its own entries are open: the
    /// reader is inside the control rather than on it, and Esc is the way back
    /// out.
    fn focusable(&self) -> Vec<SidebarFocus> {
        // A path entry is the one thing a reader with one open is doing, so the
        // affordance that opened it is the one place they can be: the arrows
        // have nowhere to walk while they are saying where to work.
        if self.workspace_entry.is_some() {
            return vec![SidebarFocus::AddWorkspace];
        }
        let rows = self.body().into_iter().filter_map(|entry| match entry {
            BodyEntry::Session(session, _) => session
                .readable()
                .map(|_| SidebarFocus::Session(session.reference().clone())),
            BodyEntry::ShowMore(_) => Some(SidebarFocus::ShowMore),
            BodyEntry::Scope(scope) => Some(SidebarFocus::Scope(scope)),
            BodyEntry::Divider => None,
        });
        if self.selector_open {
            return rows.collect();
        }
        [SidebarFocus::Selector, SidebarFocus::AddWorkspace]
            .into_iter()
            .chain(rows)
            .collect()
    }

    /// The entries the arrows can reach as the Sidebar draws them now, taken
    /// before a change so that focus landing nowhere can be carried to the
    /// nearest entry that survived. A Sidebar pointing at nothing has nothing
    /// to reconcile, and is spared walking its whole listing to say so.
    fn focus_order_before_change(&self) -> Vec<SidebarFocus> {
        if self.focus.is_none() {
            return Vec::new();
        }
        self.focusable()
    }

    /// Puts row focus back on a row that is drawn, where the one it was on no
    /// longer is. Narrowing the list is the ordinary way that happens: the
    /// reader types another letter and the row under them steps aside.
    ///
    /// Where the entry survives, focus stays on it however far the rows moved:
    /// it is held by what it stands for. Where it does not, focus goes to the
    /// nearest entry that did survive, read off `before` — the order the
    /// Sidebar drew before the change — so a live update moves the reader by
    /// one row rather than throwing them back to the top of the column.
    ///
    /// A Sidebar with no row focus is left with none: focus belongs to the
    /// keys, and a listing arriving is not the reader picking them up. One
    /// that has them is never left pointing at nothing, so a change that takes
    /// the last row with it lands focus where entering does — on the selector
    /// above the list.
    fn keep_focus_drawn(&mut self, before: &[SidebarFocus]) {
        let Some(focus) = self.focus.clone() else {
            return;
        };
        let focusable = self.focusable();
        if focusable.contains(&focus) {
            return;
        }
        self.focus = nearest_surviving(before, &focus, &focusable)
            .or_else(|| first_row(&focusable))
            .or_else(|| focusable.first().cloned());
    }

    /// Puts row focus where entering the Sidebar puts it: on the open
    /// Session's row where the Sidebar draws one, and on the Workspace
    /// selector otherwise.
    ///
    /// Nothing here reveals the open Session. A Session the query, the
    /// Workspace scope, the settled shelf's cap, or the listing itself leaves
    /// out has no row to stand on, and the reader starts at the top of the
    /// column instead — the Sidebar never invents a row, and never puts focus
    /// on some other Session as a stand-in.
    ///
    /// The same reading answers a listing that reports the open Session as
    /// unreadable: such a row is not one the arrows can reach, so it is not
    /// one focus can be seeded onto either.
    fn seed_focus(&mut self, open: Option<&SessionReference>) {
        // Read with nothing focused, so the settled shelf's exception for the
        // focused row cannot conjure the very row this is asking after.
        self.focus = None;
        let focusable = self.focusable();
        self.focus = open
            .cloned()
            .map(SidebarFocus::Session)
            .filter(|open| focusable.contains(open))
            .or_else(|| {
                focusable
                    .contains(&SidebarFocus::Selector)
                    .then_some(SidebarFocus::Selector)
            })
            .or_else(|| focusable.first().cloned());
    }

    fn move_focus(&mut self, distance: isize) {
        let focusable = self.focusable();
        if focusable.is_empty() {
            self.focus = None;
            return;
        }
        let standing = self
            .focus
            .as_ref()
            .and_then(|focus| focusable.iter().position(|entry| entry == focus))
            .unwrap_or(0);
        let len = focusable.len() as isize;
        let next = (standing as isize + distance).rem_euclid(len) as usize;
        self.focus = focusable.get(next).cloned();
    }

    /// Drops what the Sidebar was pointing at once the Session behind it has
    /// left the listing, so no row is attached twice, no menu offers to act on
    /// work that is gone, and row focus lands back on a row that is there.
    fn forget_absent(&mut self, before: &[SidebarFocus]) {
        if self
            .attaching
            .as_ref()
            .is_some_and(|attaching| !self.listing.contains(attaching))
        {
            self.attaching = None;
        }
        if self
            .deleting
            .as_ref()
            .is_some_and(|deleting| !self.listing.contains(deleting))
        {
            self.deleting = None;
        }
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| !self.listing.contains(&menu.session))
        {
            self.menu = None;
        }
        self.keep_focus_drawn(before);
    }
}

/// One entry of the Sidebar's body, before a frame gives it anything to say.
#[derive(Clone, Debug)]
enum BodyEntry<'a> {
    Session(&'a ListedSession, Standing),
    /// One Workspace the open selector offers.
    Scope(SidebarListingScope),
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
    on_show: Vec<&'a ListedSession>,
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
/// starts at. It holds still while its `anchor` is inside it, is carried only
/// as far as the anchor takes it, and never so far that it trails blank
/// lines below a list that has since grown shorter.
///
/// A reader standing on the selector above the list is on no entry of the body
/// at all, and the body holds where they left it: stepping off the top of a
/// list is not asking to be carried back to its head.
fn window_start(last: usize, anchor: Option<usize>, heights: &[usize], capacity: usize) -> usize {
    let furthest = earliest_opening(heights, heights.len().saturating_sub(1), capacity);
    let Some(anchor) = anchor else {
        return last.min(furthest);
    };
    let earliest = earliest_opening(heights, anchor, capacity);
    last.min(anchor).min(furthest).max(earliest)
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

/// The entry nearest where a lost one stood, read off `before` — the order the
/// arrows walked before whatever change dropped it. The walk steps outward a
/// row at a time, down before up at equal distance, because the list reads
/// downward: a row deleted under the reader hands focus to the one that took
/// its place rather than to the one above it.
///
/// `None` where the lost entry was not in that order at all, which leaves the
/// caller to fall back on the head of the list.
fn nearest_surviving(
    before: &[SidebarFocus],
    lost: &SidebarFocus,
    focusable: &[SidebarFocus],
) -> Option<SidebarFocus> {
    let stood = before.iter().position(|entry| entry == lost)?;
    (1..=before.len())
        .flat_map(|step| [stood.checked_add(step), stood.checked_sub(step)])
        .flatten()
        .filter_map(|index| before.get(index))
        .find(|entry| focusable.contains(entry))
        .cloned()
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
fn first_row(focusable: &[SidebarFocus]) -> Option<SidebarFocus> {
    focusable
        .iter()
        .find(|entry| !matches!(entry, SidebarFocus::Selector | SidebarFocus::AddWorkspace))
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
            AutoSettle, EffectiveSettings, ModelAvailability, Outlook, Session, SessionId,
            SessionListItem, SessionReference, SessionStatus, SessionSummary, SessionTimestamp,
            SidebarSettings, SidebarVisibility, Workspace,
        },
        tui::{
            SessionListRequest,
            commands::SemanticCommandId,
            sidebar::{
                MINIMUM_MAIN_WIDTH, SIDEBAR_WIDTH, Sidebar, SidebarActivation, SidebarEntry,
                SidebarPress, SidebarSpan, SidebarTarget, width_beside, workspace_name,
            },
        },
    };

    #[test]
    fn a_row_highlights_and_acts_on_its_own_origin() {
        let session_id = SessionId::new();
        let studio = Outlook::Remote("studio".to_owned());
        let reference = SessionReference::new(studio.clone(), session_id);
        let local_twin = SessionReference::new(Outlook::Local, session_id);
        let (mut sidebar, _) = driven();
        sidebar.adopt_outlook(studio);
        let request = sidebar
            .take_listing_request()
            .expect("turning the visible Sidebar asks its new Origin");
        sidebar.load(
            &request,
            vec![identified(session_id, "Remote work", 1)],
            None,
        );

        let local_entry = sidebar.entries(Some(&local_twin));
        assert!(
            matches!(&local_entry[0], SidebarEntry::Row(row) if !row.open),
            "an equal Session ID from another Origin is not the open row"
        );
        let remote_entry = sidebar.entries(Some(&reference));
        assert!(matches!(
            &remote_entry[0],
            SidebarEntry::Row(row) if row.open && row.remote.is_none()
        ));

        sidebar.record_geometry(
            0..32,
            vec![SidebarSpan {
                rows: 1..4,
                columns: None,
                target: SidebarTarget::Session(reference.clone()),
            }],
        );
        sidebar.open_menu_at(ratatui::layout::Position::new(1, 1));
        assert_eq!(
            sidebar.activate_menu_item(),
            SidebarPress::Invoke(SemanticCommandId::SessionSettle.on_session(reference.clone())),
            "the context menu carries the row's Origin"
        );

        sidebar.focus_on(SidebarTarget::Session(reference.clone()));
        assert_eq!(
            sidebar.activate(None, false),
            SidebarActivation::Attach(reference),
            "row activation carries the same Origin"
        );
    }

    #[test]
    fn the_initial_visibility_setting_has_its_say_once_and_the_toggle_has_it_after() {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&launching(SidebarVisibility::Hidden));

        assert!(!sidebar.is_revealed());
        assert!(
            sidebar.take_listing_request().is_none(),
            "a Sidebar nobody can see asks for nothing"
        );

        sidebar.toggle(None);

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

        sidebar.toggle(None);
        assert!(
            !sidebar.has_focus(),
            "the toggle that closes it holds nothing"
        );

        sidebar.toggle(None);
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
        sidebar.toggle(None);
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

        sidebar.toggle(None);
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        let open = SessionReference::new(Outlook::Local, open);
        sidebar.load(&request, listing, Some(&open));

        assert_eq!(focused(&sidebar), Some("Open"));
    }

    #[test]
    fn the_selection_follows_its_session_through_a_listing_that_lands_under_it() {
        let wanted = SessionId::new();
        let (mut sidebar, request) = driven();

        sidebar.load(
            &request,
            vec![summary("Newest", 3, 30), identified(wanted, "Wanted", 1)],
            None,
        );
        // Down the selector's line and past the first row onto the second.
        sidebar.focus_next();
        sidebar.focus_next();
        sidebar.focus_next();
        assert_eq!(focused(&sidebar), Some("Wanted"));

        let again = caught_up(&mut sidebar);
        sidebar.load(
            &again,
            vec![
                summary("Newer still", 4, 40),
                identified(wanted, "Wanted", 1),
            ],
            None,
        );

        assert_eq!(
            focused(&sidebar),
            Some("Wanted"),
            "a listing arriving underneath the reader leaves them on the work, not on the row"
        );
    }

    #[test]
    fn a_session_that_leaves_the_listing_takes_the_focus_off_it() {
        let doomed = SessionId::new();
        let (mut sidebar, request) = driven();

        sidebar.load(
            &request,
            vec![summary("Survivor", 2, 20), identified(doomed, "Doomed", 1)],
            None,
        );
        // Down the selector's line and the first row onto the doomed one.
        sidebar.focus_next();
        sidebar.focus_next();
        sidebar.focus_next();
        assert_eq!(focused(&sidebar), Some("Doomed"));

        sidebar.remove_origin(Outlook::Local, doomed);

        assert_eq!(
            focused(&sidebar),
            Some("Survivor"),
            "a Session deleted elsewhere lands the keys on the nearest row that is there"
        );
    }

    #[test]
    fn enter_on_the_session_already_open_only_hands_the_keys_back() {
        let mut sidebar = Sidebar::new(root());
        let open = SessionId::new();

        sidebar.toggle(None);
        let request = sidebar.take_listing_request().expect("ask for Sessions");
        let reference = SessionReference::new(Outlook::Local, open);
        sidebar.load(
            &request,
            vec![identified(open, "Open", 1)],
            Some(&reference),
        );

        assert_eq!(
            sidebar.activate(Some(&reference), false),
            SidebarActivation::Answered
        );
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
        sidebar.adopt_settings(&under(SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Idle(1),
            ..SidebarSettings::default()
        }));
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

        // Opening with no Session open starts the keys on the selector, and up
        // off the top of the column wraps to the last entry of all: the
        // affordance at the shelf's foot. It stands for no Session, which is
        // what activating it says.
        sidebar.focus_previous();
        assert_eq!(focused(&sidebar), None);

        assert_eq!(
            sidebar.activate(None, false),
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

        let _ = sidebar.activate(None, false);

        assert_eq!(
            drawn(&sidebar).len(),
            41,
            "the divider and the whole shelf, with no affordance left to draw"
        );
        assert_eq!(
            focused(&sidebar),
            Some("Settled 39"),
            "the affordance the reader was on is gone, so they land on the last row it uncovered"
        );
    }

    /// Work the reader had the keys on can sink past the cap underneath them —
    /// another client settles two Sessions, and the row they were on is the
    /// eleventh of a shelf showing ten. The cap must not then hide it: focus
    /// drawn nowhere is focus the arrows cannot step off and Enter cannot act
    /// on.
    #[test]
    fn the_shelf_goes_on_showing_the_row_the_keys_are_on_however_deep_it_sinks() {
        let deep = SessionId::new();
        let mut whole = set_aside_shelf(12);
        let SessionListItem::Readable(deepest) = &mut whole[11] else {
            unreachable!("the fixture builds readable Sessions");
        };
        deepest.session.id = deep;
        // The shelf as it stood when the reader put the keys on its last row:
        // ten rows, which is exactly what the cap shows.
        let shallower = whole[2..].to_vec();

        let (mut sidebar, request) = driven();
        let deep = SessionReference::new(Outlook::Local, deep);
        sidebar.load(&request, shallower, Some(&deep));
        assert_eq!(focused(&sidebar), Some("Settled 11"));

        let again = caught_up(&mut sidebar);
        sidebar.load(&again, whole, Some(&deep));

        let drawn = drawn(&sidebar);
        assert_eq!(
            drawn[11], "Settled 11",
            "the row the keys are on stands at the foot of the shelf, on the reader's account rather than its work's: {drawn:?}"
        );
        assert_eq!(
            drawn[12], "Show 1 more",
            "and is no longer one of the rows the affordance offers: {drawn:?}"
        );
        assert_eq!(
            focused(&sidebar),
            Some("Settled 11"),
            "so the reader can see the row they are on, and step off it"
        );
    }

    /// The open Session earns no such exception. How deep a reader's history
    /// is on show is their own reading of it, and opening something out of
    /// sight must not quietly grow the shelf to say so.
    #[test]
    fn a_deeply_settled_open_session_is_left_under_the_shelf_cap() {
        let deep = SessionId::new();
        let mut shelf = set_aside_shelf(12);
        let SessionListItem::Readable(deepest) = &mut shelf[11] else {
            unreachable!("the fixture builds readable Sessions");
        };
        deepest.session.id = deep;

        let sidebar = showing_on(shelf, Some(deep));

        let drawn = drawn(&sidebar);
        assert!(
            !drawn.contains(&"Settled 11".to_owned()),
            "the shelf shows the ten rows it opens on, whatever is open: {drawn:?}"
        );
        assert_eq!(
            drawn.last().map(String::as_str),
            Some("Show 2 more"),
            "and goes on offering both the rows under it: {drawn:?}"
        );
        assert_eq!(
            focused(&sidebar),
            None,
            "with no row for the open Session, the keys start on the selector rather than on some              other Session standing in for it"
        );
    }

    #[test]
    fn a_sidebar_asked_for_afresh_opens_the_shelf_on_its_first_rows_again() {
        let mut sidebar = showing(set_aside_shelf(12));
        // Up off the selector wraps to the affordance at the shelf's foot.
        sidebar.focus_previous();
        let _ = sidebar.activate(None, false);
        assert_eq!(drawn(&sidebar).len(), 13, "the whole shelf is on show");

        sidebar.toggle(None);
        sidebar.toggle(None);
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
        sidebar.toggle(None);

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

        sidebar.set_working_origin(Outlook::Local, idle, Some(SessionTimestamp(5)));

        assert!(
            sidebar.shows_live_work(),
            "a Turn starting in a listed Session arms the tick its Working duration rises on"
        );

        sidebar.set_working_origin(Outlook::Local, idle, None);

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
    fn showing_on(sessions: Vec<SessionListItem>, open: Option<SessionId>) -> Sidebar {
        let (mut sidebar, request) = driven();
        let open = open.map(|session_id| SessionReference::new(Outlook::Local, session_id));
        sidebar.load(&request, sessions, open.as_ref());
        sidebar
    }

    /// The listing a Sidebar already on screen asks for to catch up with a
    /// catalog that moved under it, which is the ask every question about
    /// reconciling live updates is answered through: a listing already
    /// answered cannot be answered twice.
    fn caught_up(sidebar: &mut Sidebar) -> SessionListRequest {
        sidebar.catch_up_origin(Outlook::Local);
        sidebar
            .take_listing_request()
            .expect("a revealed Sidebar catching up asks again")
    }

    /// A Sidebar the reader opened themselves, with the listing it asked for
    /// on opening still to be answered. It is the reader's own opening that
    /// gives the Sidebar the keys, and so the row focus every question about
    /// the arrows is asked of.
    fn driven() -> (Sidebar, SessionListRequest) {
        let mut sidebar = Sidebar::new(root());
        sidebar.adopt_settings(&settling_nothing());
        // The Setting revealed it without taking the keys, so the reader
        // closes it and opens it themselves.
        sidebar.toggle(None);
        sidebar.toggle(None);
        let request = sidebar
            .take_listing_request()
            .expect("a revealed Sidebar asks for its Sessions");
        (sidebar, request)
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
    fn focused(sidebar: &Sidebar) -> Option<&str> {
        sidebar
            .entries(None)
            .into_iter()
            .find_map(|entry| match entry {
                SidebarEntry::Row(row) if row.focused => Some(row.title),
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
        listed.session.working_since = Some(SessionTimestamp(updated_at));
        SessionListItem::Readable(listed)
    }

    fn summary(title: &str, created_at: u64, updated_at: u64) -> SessionListItem {
        SessionListItem::Readable(Box::new(SessionSummary {
            session: Session {
                id: SessionId::new(),
                workspace: Workspace {
                    path: root().join("workspace"),
                },
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
                working_since: None,
                parent: None,
            },
            title: title.to_owned(),
            emoji: None,
            settled_at: None,
            total_usage: None,
            created_at: SessionTimestamp(created_at),
            updated_at: SessionTimestamp(updated_at),
        }))
    }

    /// The Sidebar's Settings as a TUI launching under `initial_visibility`
    /// takes them, everything else left where its built-in default is.
    fn launching(initial_visibility: SidebarVisibility) -> EffectiveSettings {
        under(SidebarSettings {
            initial_visibility,
            ..SidebarSettings::default()
        })
    }

    /// The Sidebar shown with nothing settling itself, which is what a test
    /// about the order or the shape of the list asks for: its fixtures stamp
    /// Sessions with ordinals rather than with moments, and every one of those
    /// reads as work left alone since the epoch.
    fn settling_nothing() -> EffectiveSettings {
        under(SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Off,
            ..SidebarSettings::default()
        })
    }

    /// Effective settings whose only departure from the built-in defaults is
    /// what the reader asked of the Sidebar itself.
    fn under(sidebar: SidebarSettings) -> EffectiveSettings {
        EffectiveSettings {
            sidebar,
            ..EffectiveSettings::default()
        }
    }

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
