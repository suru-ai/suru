//! The Sidebar: the collapsible column beside the main view listing Sessions.

use std::{
    cell::Cell,
    cmp::Reverse,
    path::{Path, PathBuf},
};

use crate::protocol::{SessionId, SessionListItem, SessionTimestamp, SidebarVisibility};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface, session_listing::SessionListing,
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
    listing: SessionListing,
    /// The row the reader is on, held by Session rather than by position so a
    /// listing arriving underneath them leaves the selection where the work
    /// is rather than where the row was.
    selected: Option<SessionId>,
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
    /// The row the column's window opens on. Only a frame knows how many rows
    /// it holds, so the window is settled at draw time and remembered here: a
    /// selection moving to a row already in view leaves it where it is, and
    /// only a selection moving out of view carries it along.
    window_start: Cell<usize>,
}

/// One Session as the Sidebar draws it: three lines, the third of them saying
/// nothing until git awareness gives it something to say
/// (<https://github.com/jake-tucker/suru/issues/169>).
#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarRow<'a> {
    /// The Workspace this Session is rooted in, drawn by its last component. A
    /// Session Suru could not read may not know its Workspace at all.
    pub(super) workspace: Option<&'a Path>,
    /// How long ago this Session was last active, drawn in the row's right
    /// slot.
    // The right slot holds only a time today. Working with a ticking duration
    // arrives with <https://github.com/jake-tucker/suru/issues/182>, and the
    // remaining status labels with
    // <https://github.com/jake-tucker/suru/issues/168>.
    pub(super) updated_at: SessionTimestamp,
    pub(super) emoji: Option<&'a str>,
    pub(super) title: &'a str,
    /// Whether this is the Session the reader has open.
    pub(super) current: bool,
    /// Whether this is the row the reader is on, which is the one Enter acts
    /// on and the one the column draws highlighted.
    pub(super) selected: bool,
}

impl Sidebar {
    /// A Sidebar listing every Workspace's Sessions, which is the whole body of
    /// work a reader has. Narrowing to one Workspace is the selector's job
    /// (<https://github.com/jake-tucker/suru/issues/177>).
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
            listing: SessionListing::scoped(
                SessionListSurface::Sidebar,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            selected: None,
            attaching: None,
            awaiting_dispatch: None,
            // Until a frame says otherwise, which it does before anything the
            // reader types can reach a surface.
            on_screen: Cell::new(true),
            window_start: Cell::new(0),
        }
    }

    /// Puts the launch Setting in force, once. Returns nothing: a Sidebar that
    /// wants its Sessions leaves the request in [`Self::take_listing_request`].
    pub(super) fn seed(&mut self, visibility: SidebarVisibility) {
        if self.seeded {
            return;
        }
        self.seeded = true;
        self.reveal(visibility == SidebarVisibility::Shown);
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
        self.focused = self.revealed;
    }

    /// Hands the keys back to the composer and leaves the Sidebar standing,
    /// which is what Esc asks for: done choosing, not done looking.
    pub(super) fn leave(&mut self) {
        self.focused = false;
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
    }

    /// Records that this frame found the columns for the Sidebar and drew it.
    pub(super) fn record_drawn(&self) {
        self.on_screen.set(true);
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
            self.awaiting_dispatch = Some(self.listing.refresh());
        }
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
        // A listing the reader was already reading keeps them where they were;
        // a fresh one starts them on the Session they have open, and failing
        // that on the row nearest them.
        self.selected = self
            .selected
            .filter(|selected| self.listing.contains(*selected))
            .or_else(|| current.filter(|current| self.listing.contains(*current)))
            .or_else(|| self.first_listed());
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        self.listing.fail(request, error);
    }

    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        self.listing.retitle(session_id, title, emoji);
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

    /// Begins attaching the Session the reader is on, reporting the Session to
    /// attach where there is one to attach to. There is none when the row
    /// stands for a Session Suru could not read, and none when it stands for
    /// the Session already open — in which case Enter means only that the
    /// reader is done choosing, and the composer takes the keys back.
    pub(super) fn begin_attachment(&mut self, current: Option<SessionId>) -> Option<SessionId> {
        let selected = self.selected?;
        self.listing
            .sessions()
            .iter()
            .find(|summary| summary.id() == selected)?
            .readable()?;
        if current == Some(selected) {
            self.focused = false;
            return None;
        }
        self.listing.clear_error();
        self.attaching = Some(selected);
        Some(selected)
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
        self.focused = false;
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

    /// The rows a column this tall can hold: a list longer than the Sidebar is
    /// read through a window rather than being crammed into the rows
    /// available. The window moves only as far as it must to keep the row the
    /// reader is on in view, so moving within it leaves every other row where
    /// the reader last saw it.
    pub(super) fn visible_rows(
        &self,
        capacity: usize,
        current: Option<SessionId>,
    ) -> Vec<SidebarRow<'_>> {
        let rows = self.rows(current);
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        let start = window_start(self.window_start.get(), selected, rows.len(), capacity);
        self.window_start.set(start);
        rows.into_iter().skip(start).take(capacity).collect()
    }

    fn rows(&self, current: Option<SessionId>) -> Vec<SidebarRow<'_>> {
        self.ordered()
            .into_iter()
            .map(|session| SidebarRow {
                workspace: session
                    .workspace()
                    .map(|workspace| workspace.path.as_path()),
                updated_at: session.updated_at(),
                emoji: session.emoji(),
                title: session.title(),
                current: current == Some(session.id()),
                selected: self.selected == Some(session.id()),
            })
            .collect()
    }

    /// The Sessions in the order the Sidebar shows them: newest created first,
    /// and never reordered by activity, so a row a reader has their eye on
    /// holds its place while the work behind it moves.
    fn ordered(&self) -> Vec<&SessionListItem> {
        let mut sessions = self.listing.sessions().iter().collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(session.created_at()));
        sessions
    }

    fn first_listed(&self) -> Option<SessionId> {
        self.ordered().first().map(|session| session.id())
    }

    fn move_selection(&mut self, distance: isize) {
        let listed = self
            .ordered()
            .into_iter()
            .map(SessionListItem::id)
            .collect::<Vec<_>>();
        if listed.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| listed.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let len = listed.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = listed.get(next).copied();
    }

    /// Drops what the Sidebar was pointing at once the Session behind it has
    /// left the listing, so no row is attached twice and the reader lands back
    /// on a row that is there.
    fn forget_absent(&mut self) {
        if self
            .attaching
            .is_some_and(|attaching| !self.listing.contains(attaching))
        {
            self.attaching = None;
        }
        if self
            .selected
            .is_some_and(|selected| !self.listing.contains(selected))
        {
            self.selected = self.first_listed();
        }
    }
}

/// Where a column holding `capacity` rows opens, given the window it last
/// opened on and the row the reader is now on. The window holds still while
/// the selection is inside it and is carried only as far as the selection
/// takes it, and never so far that it runs off the end of a list that has
/// since grown shorter.
fn window_start(last: usize, selected: usize, listed: usize, capacity: usize) -> usize {
    let furthest = listed.saturating_sub(capacity);
    let earliest = selected.saturating_add(1).saturating_sub(capacity);
    last.min(selected).min(furthest).max(earliest)
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
            ModelAvailability, Session, SessionId, SessionListItem, SessionStatus, SessionSummary,
            SessionTimestamp, SidebarVisibility, Workspace,
        },
        tui::sidebar::{MINIMUM_MAIN_WIDTH, SIDEBAR_WIDTH, Sidebar, width_beside, workspace_name},
    };

    #[test]
    fn the_launch_setting_has_its_say_once_and_the_toggle_has_it_after() {
        let mut sidebar = Sidebar::new(root());
        sidebar.seed(SidebarVisibility::Hidden);

        assert!(!sidebar.is_revealed());
        assert!(
            sidebar.take_listing_request().is_none(),
            "a Sidebar nobody can see asks for nothing"
        );

        sidebar.toggle();

        assert!(sidebar.is_revealed());
        assert!(sidebar.take_listing_request().is_some());

        sidebar.seed(SidebarVisibility::Hidden);

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
        sidebar.seed(SidebarVisibility::Shown);

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
        sidebar.seed(SidebarVisibility::Shown);
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

        assert_eq!(
            sidebar
                .rows(None)
                .into_iter()
                .map(|row| row.title)
                .collect::<Vec<_>>(),
            vec!["Newest", "Middle", "Oldest"]
        );
    }

    #[test]
    fn the_toggle_takes_the_keys_and_the_launch_setting_leaves_them_alone() {
        let mut sidebar = Sidebar::new(root());
        sidebar.seed(SidebarVisibility::Shown);

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
        sidebar.seed(SidebarVisibility::Hidden);
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

        sidebar.toggle();
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

        assert_eq!(sidebar.begin_attachment(Some(open)), None);
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

    /// The Title of the row the reader is on, which is what a selection is
    /// for.
    fn selected(sidebar: &Sidebar) -> Option<&str> {
        sidebar
            .rows(None)
            .into_iter()
            .find(|row| row.selected)
            .map(|row| row.title)
    }

    fn identified(session_id: SessionId, title: &str, created_at: u64) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, created_at, created_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.session.id = session_id;
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

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
