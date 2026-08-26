//! The Sidebar: the collapsible column beside the main view listing Sessions.

use std::{
    cell::{Cell, RefCell},
    cmp::Reverse,
    ops::Range,
    path::{Path, PathBuf},
};

use ratatui::layout::Position;

use crate::protocol::{
    AutoSettle, SessionId, SessionListItem, SessionTimestamp, SidebarSettings, SidebarVisibility,
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

/// The Sidebar's own state: whether the reader wants it, and the Sessions it
/// lists.
///
/// Visibility has two independent halves. The reader's choice — seeded once
/// from the launch Setting and flipped by the toggle — lives here and is never
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
    /// Whether the launch Setting has had its say. Only the first snapshot
    /// seeds: every later one carries some other Setting's edit, and a reader
    /// who toggled the Sidebar since should not have it flipped back under
    /// them.
    seeded: bool,
    /// When a Session settles without anyone saying so. Adopted from every
    /// snapshot rather than seeded from the first, because unlike the launch
    /// Setting this one governs what the Sidebar shows for as long as it is
    /// open: editing it reclassifies every listed Session on the next frame.
    auto_settle: AutoSettle,
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
    /// A listing the Sidebar has asked for but has not yet handed to whoever
    /// dispatches it. Revealing the Sidebar is not always something a reader
    /// did — the launch Setting reveals it too — so the request waits here for
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

/// What the reader is on in the Sidebar's body. Almost always that is a
/// Session, named by its id so the selection follows the work rather than the
/// row it happened to be drawn on. The one row standing for no Session at all
/// is the settled shelf's own affordance, which brings up more of the shelf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarSelection {
    Session(SessionId),
    ShowMore,
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
        /// How long ago this Session was last active, drawn in the row's right
        /// slot.
        // The right slot holds only a time today. Working with a ticking
        // duration arrives with
        // <https://github.com/jake-tucker/suru/issues/182>, and the remaining
        // status labels with
        // <https://github.com/jake-tucker/suru/issues/168>.
        updated_at: SessionTimestamp,
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
#[derive(Clone, Copy, Debug)]
pub(super) enum SidebarEntry<'a> {
    Row(SidebarRow<'a>),
    /// The rule closing the active list and opening the settled shelf. It
    /// stands only where something is settled: a reader with nothing set aside
    /// is shown no shelf to set it on.
    Divider,
    /// The row at the foot of a settled shelf holding more than is on show,
    /// which brings up the next of it.
    ShowMore(SidebarShowMore),
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
            Self::Divider | Self::ShowMore(_) => 1,
        }
    }

    /// Whether this is the entry the reader is on. The divider never is: it
    /// is a rule rather than a row, so the arrows step over it.
    const fn is_selected(&self) -> bool {
        match self {
            Self::Row(row) => row.selected,
            Self::ShowMore(more) => more.selected,
            Self::Divider => false,
        }
    }

    /// What pressing this entry asks for, and `None` for the divider, which is
    /// a rule rather than a row and so answers no press.
    pub(super) const fn target(&self) -> Option<SidebarTarget> {
        match self {
            Self::Row(row) => Some(SidebarTarget::Session(row.session_id)),
            Self::ShowMore(_) => Some(SidebarTarget::ShowMore),
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
    pub(super) target: SidebarTarget,
}

/// What a drawn entry stands for: a Session, or the settled shelf's own
/// affordance, which stands for no Session at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SidebarTarget {
    Session(SessionId),
    ShowMore,
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
            .find(|span| span.rows.contains(&position.y))
            .map(|span| span.target)
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
    /// work a reader has. Narrowing to one Workspace is the selector's job
    /// (<https://github.com/jake-tucker/suru/issues/179>).
    pub(super) fn new(current_workspace: PathBuf) -> Self {
        Self {
            // Down until the launch Setting raises it. A Sidebar with no
            // Settings in hand has not spoken to a server either, so it has
            // nothing to list; drawing one before the snapshot lands would put
            // an empty column on screen and take it away again for a reader
            // who configured it hidden.
            revealed: false,
            focused: false,
            seeded: false,
            auto_settle: AutoSettle::default(),
            listing: SessionListing::scoped(
                SessionListSurface::Sidebar,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            settled_on_show: SETTLED_SHELF_OPENING,
            query: String::new(),
            selected: None,
            attaching: None,
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
    /// governs every frame from here on, while the launch Setting has its say
    /// once and is then the reader's to overrule. Returns nothing: a Sidebar
    /// that wants its Sessions leaves the request in
    /// [`Self::take_listing_request`].
    pub(super) fn adopt_settings(&mut self, settings: &SidebarSettings) {
        self.auto_settle = settings.auto_settle;
        if self.seeded {
            return;
        }
        self.seeded = true;
        self.reveal(settings.launch_visibility == SidebarVisibility::Shown);
    }

    /// Shows the Sidebar, or hides it. This is view state and nothing more: the
    /// launch Setting is not rewritten.
    ///
    /// A reader who opens the Sidebar is asking to drive it, so it takes the
    /// keys; closing hands them back. The launch Setting's own reveal in
    /// [`Self::seed`] does neither, because a reader who has not touched the
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
    /// Esc asks for. A query in hand is the innermost step: it is given up
    /// first, and the reader goes on driving the whole list they are back to.
    /// Only from there do the keys go to the composer, leaving the Sidebar
    /// standing — done choosing, not done looking.
    pub(super) fn leave(&mut self) {
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

    /// Takes what the reader typed into the search box, narrowing the list to
    /// the Sessions whose Titles carry it.
    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.keep_selection_drawn();
    }

    /// Takes the query back a character, widening the results to match.
    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.keep_selection_drawn();
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
    /// takes the selection and is opened, or on the settled shelf's affordance,
    /// which brings up more of the shelf. Both are what Enter already does from
    /// that row, so the press mints no behavior of its own: it says which row,
    /// by a position only this frame can know, and the command does the rest.
    pub(super) fn press_at(&mut self, position: Position) -> SidebarPress {
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
    /// the launch Setting's and the toggle's alike — asks for them afresh.
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
            // rows again, and a menu left open on some earlier look is put
            // away with it.
            self.menu = None;
            self.clear_query();
            self.ask_for_sessions();
        }
    }

    /// Asks the server for the Sessions in scope and leaves the request for
    /// whoever can dispatch it.
    ///
    /// The settled shelf opens on its first rows again with every ask. The
    /// tail a reader brought up belongs to the listing they brought it up on,
    /// so a Sidebar coming back into view — or narrowed to another Workspace
    /// once the selector arrives
    /// (<https://github.com/jake-tucker/suru/issues/179>), which asks the same
    /// way — starts back at the top of the shelf rather than inheriting
    /// however deep they had walked into some other body of work. A listing
    /// re-asked only to catch up with the server
    /// (<https://github.com/jake-tucker/suru/issues/183>) is not the reader
    /// moving anywhere, and must leave their shelf where they left it.
    fn ask_for_sessions(&mut self) {
        self.settled_on_show = SETTLED_SHELF_OPENING;
        self.awaiting_dispatch = Some(self.listing.refresh());
    }

    /// Whether the reader wants the Sidebar on screen, which is not the same
    /// question as whether the frame has room for it.
    pub(super) const fn is_revealed(&self) -> bool {
        self.revealed
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
        self.attaching = None;
        // A menu stands on one row of the listing it was opened over. A fresh
        // listing is the reader looking again, so it is put away rather than
        // left pointing at whatever now stands where its row did.
        self.menu = None;
        // A listing the reader was already reading keeps them where they were;
        // a fresh one starts them on the Session they have open, and failing
        // that on the row nearest them.
        self.selected = self
            .selected
            .filter(|selected| self.holds(*selected))
            .or_else(|| {
                current
                    .filter(|current| self.draws(*current))
                    .map(SidebarSelection::Session)
            })
            .or_else(|| self.first_listed());
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
    /// Standing on the settled shelf's affordance, it asks for more of the
    /// shelf instead, and there is nothing to attach. Nor is there when the
    /// row stands for a Session Suru could not read, or for the Session
    /// already open — in which case Enter means only that the reader is done
    /// choosing, and the composer takes the keys back.
    pub(super) fn activate(&mut self, current: Option<SessionId>) -> Option<SessionId> {
        let SidebarSelection::Session(selected) = self.selected? else {
            self.show_more();
            return None;
        };
        self.listing
            .sessions()
            .iter()
            .find(|summary| summary.id() == selected)?
            .readable()?;
        if current == Some(selected) {
            self.hand_back_keys();
            return None;
        }
        self.listing.clear_error();
        self.attaching = Some(selected);
        Some(selected)
    }

    /// Brings up the next of the settled shelf. Where that was the whole of
    /// what was left, the affordance the reader was standing on goes with it,
    /// so they land on the last row it brought up rather than on nothing.
    fn show_more(&mut self) {
        self.settled_on_show = self.settled_on_show.saturating_add(SETTLED_SHELF_BATCH);
        let selectable = self.selectable();
        if !selectable.contains(&SidebarSelection::ShowMore) {
            self.selected = selectable.last().copied();
        }
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
        let selected = entries
            .iter()
            .position(SidebarEntry::is_selected)
            .unwrap_or(0);
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
    /// and as much of the settled shelf as is on show — or, while the reader
    /// is searching, the results in place of both. Everything the Sidebar has
    /// to say about what stands where is said here and nowhere else, so the
    /// rows the frame draws and the rows the arrows walk can never disagree.
    fn body(&self) -> Vec<BodyEntry<'_>> {
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

    /// The active Sessions: newest created first, and never reordered by
    /// activity, so a row a reader has their eye on holds its place while the
    /// work behind it moves.
    fn active(&self, settlement: Settlement) -> Vec<&SessionListItem> {
        let mut sessions = self
            .listing
            .sessions()
            .iter()
            .filter(|session| !settlement.settles(session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(session.created_at()));
        sessions
    }

    /// The Sessions set aside, ordered by when the work ended rather than by
    /// when it began, so what wrapped up most recently is nearest the divider.
    fn settled(&self, settlement: Settlement) -> Vec<&SessionListItem> {
        let mut sessions = self
            .listing
            .sessions()
            .iter()
            .filter(|session| settlement.settles(session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(ended_at(session)));
        sessions
    }

    /// Every row the reader can be on, in the order the Sidebar draws them,
    /// which is the order the arrows walk: the body without the divider, which
    /// is a rule rather than a row.
    fn selectable(&self) -> Vec<SidebarSelection> {
        self.body()
            .into_iter()
            .filter_map(|entry| match entry {
                BodyEntry::Session(session, _) => Some(SidebarSelection::Session(session.id())),
                BodyEntry::ShowMore(_) => Some(SidebarSelection::ShowMore),
                BodyEntry::Divider => None,
            })
            .collect()
    }

    /// Whether the row the reader is on is one the Sidebar still draws — which
    /// asks the body, so a Session dropped from the listing and one a query
    /// passed over are answered by the same reading.
    fn holds(&self, selection: SidebarSelection) -> bool {
        self.selectable().contains(&selection)
    }

    /// Whether the Sidebar would put the reader on this Session: it is one it
    /// lists, and one their query carries.
    ///
    /// This is the question [`Self::holds`] cannot answer, because the settled
    /// shelf shows the row the reader is on however deep it sits — so asking
    /// the body whether a Session they are not yet on is drawn would answer no
    /// for the very rows the shelf would have made room for.
    fn draws(&self, session_id: SessionId) -> bool {
        self.listing
            .sessions()
            .iter()
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
            .is_none_or(|selected| !selectable.contains(&selected))
        {
            self.selected = selectable.first().copied();
        }
    }

    fn first_listed(&self) -> Option<SidebarSelection> {
        self.selectable().first().copied()
    }

    fn move_selection(&mut self, distance: isize) {
        let selectable = self.selectable();
        if selectable.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| selectable.iter().position(|row| *row == selected))
            .unwrap_or(0);
        let len = selectable.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = selectable.get(next).copied();
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
#[derive(Clone, Copy, Debug)]
enum BodyEntry<'a> {
    Session(&'a SessionListItem, Standing),
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
fn window_start(last: usize, selected: usize, heights: &[usize], capacity: usize) -> usize {
    let furthest = earliest_opening(heights, heights.len().saturating_sub(1), capacity);
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
            MINIMUM_MAIN_WIDTH, SIDEBAR_WIDTH, Sidebar, SidebarEntry, width_beside, workspace_name,
        },
    };

    #[test]
    fn the_launch_setting_has_its_say_once_and_the_toggle_has_it_after() {
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
            "the launch Setting is what raises the Sidebar, so nothing is drawn before it lands"
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
    fn the_toggle_takes_the_keys_and_the_launch_setting_leaves_them_alone() {
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

        assert_eq!(sidebar.activate(Some(open)), None);
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
            launch_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Idle(1),
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

        sidebar.select_previous();
        assert_eq!(
            selected(&sidebar),
            None,
            "past the top of the list is the affordance, which stands for no Session"
        );

        assert_eq!(
            sidebar.activate(None),
            None,
            "asking for more of the shelf attaches nothing"
        );
        let revealed = drawn(&sidebar);
        assert_eq!(
            revealed.len(),
            37,
            "the divider, 35 rows, and the affordance"
        );
        assert_eq!(revealed[36], "Show 5 more");

        sidebar.activate(None);

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
        sidebar.select_previous();
        sidebar.activate(None);
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
            },
            title: title.to_owned(),
            emoji: None,
            settled_at: None,
            created_at: SessionTimestamp(created_at),
            updated_at: SessionTimestamp(updated_at),
        })
    }

    /// The Sidebar's Settings as a TUI launching under `launch_visibility`
    /// takes them, everything else left where its built-in default is.
    fn launching(launch_visibility: SidebarVisibility) -> SidebarSettings {
        SidebarSettings {
            launch_visibility,
            ..SidebarSettings::default()
        }
    }

    /// The Sidebar shown with nothing settling itself, which is what a test
    /// about the order or the shape of the list asks for: its fixtures stamp
    /// Sessions with ordinals rather than with moments, and every one of those
    /// reads as work left alone since the epoch.
    fn settling_nothing() -> SidebarSettings {
        SidebarSettings {
            launch_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Off,
        }
    }

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
