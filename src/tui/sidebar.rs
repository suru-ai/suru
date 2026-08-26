//! The Sidebar: the collapsible column beside the main view listing Sessions.

use std::{
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
    /// Whether the launch Setting has had its say. Only the first snapshot
    /// seeds: every later one carries some other Setting's edit, and a reader
    /// who toggled the Sidebar since should not have it flipped back under
    /// them.
    seeded: bool,
    listing: SessionListing,
    /// A listing the Sidebar has asked for but has not yet handed to whoever
    /// dispatches it. Revealing the Sidebar is not always something a reader
    /// did — the launch Setting reveals it too — so the request waits here for
    /// the next caller able to carry it.
    awaiting_dispatch: Option<SessionListRequest>,
}

/// One Session as the Sidebar draws it: three lines, the third of them blank
/// until git awareness fills it (<https://github.com/jake-tucker/suru/issues/169>).
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
            seeded: false,
            listing: SessionListing::scoped(
                SessionListSurface::Sidebar,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            awaiting_dispatch: None,
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
    pub(super) fn toggle(&mut self) {
        self.reveal(!self.revealed);
    }

    /// A Sidebar the reader can see wants Sessions to show, so every reveal —
    /// the launch Setting's and the toggle's alike — asks for them afresh.
    /// Hiding keeps what it holds: nothing is looking at it, and revealing
    /// again asks anyway.
    fn reveal(&mut self, revealed: bool) {
        self.revealed = revealed;
        if revealed {
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

    pub(super) fn load(&mut self, request: &SessionListRequest, sessions: Vec<SessionListItem>) {
        self.listing.load(request, sessions);
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
    }

    pub(super) fn retain_catalog(&mut self, session_ids: &[SessionId]) {
        self.listing.retain(session_ids);
    }

    pub(super) const fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    /// The Sessions in the order the Sidebar shows them: newest created first,
    /// and never reordered by activity, so a row a reader has their eye on
    /// holds its place while the work behind it moves.
    pub(super) fn rows(&self, current: Option<SessionId>) -> Vec<SidebarRow<'_>> {
        let mut sessions = self.listing.sessions().iter().collect::<Vec<_>>();
        sessions.sort_by_key(|session| Reverse(session.created_at()));
        sessions
            .into_iter()
            .map(|session| SidebarRow {
                workspace: session
                    .workspace()
                    .map(|workspace| workspace.path.as_path()),
                updated_at: session.updated_at(),
                emoji: session.emoji(),
                title: session.title(),
                current: current == Some(session.id()),
            })
            .collect()
    }
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
