//! Shared Session listing: the request sequencing, stale-reply dropping,
//! Workspace scope, and catalog reconciliation that every surface listing
//! Sessions needs, held in one place so no surface reimplements them.

use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
};

use crate::protocol::{Outlook, SessionId, SessionListItem, SessionSummary, SessionTimestamp};

use super::{SessionListRequest, SessionListScope, SessionListSurface};

/// One surface's view of the Sessions the server holds.
///
/// A listing owns the conversation with the server — it numbers each request
/// so a reply to a superseded one is dropped, remembers which Workspace scope
/// the reader asked for, and keeps what it holds true as the session-catalog
/// stream reports retitles, deletions, and reconciliations. What it does not
/// own is how those Sessions read: it keeps them in one order — the most
/// recently updated first — and a surface that wants another sections and
/// sorts what it derives from them.
#[derive(Clone, Debug)]
pub(super) struct SessionListing {
    /// The surface this listing belongs to, stamped on every request it makes
    /// so a reply lands on the listing that asked and on no other.
    surface: SessionListSurface,
    outlook: Outlook,
    /// The Workspace `CurrentWorkspace` scope means, kept so narrowing back to
    /// it needs nothing from the caller.
    current_workspace: PathBuf,
    scope: SessionListScope,
    request_sequence: u64,
    /// The request whose reply this listing is waiting for. Any other reply is
    /// a straggler from a listing the reader has already moved past.
    pending_request: Option<SessionListRequest>,
    sessions: Vec<SessionListItem>,
    loading: bool,
    error: Option<String>,
}

impl SessionListing {
    /// A listing scoped, as it opens, to the Workspace this client runs in.
    pub(super) fn new(surface: SessionListSurface, current_workspace: PathBuf) -> Self {
        let scope = SessionListScope::CurrentWorkspace(current_workspace.clone());
        Self::scoped(surface, current_workspace, scope)
    }

    /// A listing that opens on a scope of the caller's choosing, for a surface
    /// whose opening scope is not the Workspace the client runs in.
    pub(super) fn scoped(
        surface: SessionListSurface,
        current_workspace: PathBuf,
        scope: SessionListScope,
    ) -> Self {
        Self {
            surface,
            outlook: Outlook::Local,
            scope,
            current_workspace,
            request_sequence: 0,
            pending_request: None,
            sessions: Vec::new(),
            loading: false,
            error: None,
        }
    }

    /// Asks the server for the Sessions in scope, dropping what this listing
    /// holds: what it held answered a question the reader has moved past. The
    /// caller hands the returned request to the runtime and back to `load` or
    /// `fail` when the answer arrives.
    pub(super) fn refresh(&mut self) -> SessionListRequest {
        let request = self.catch_up();
        self.sessions.clear();
        self.loading = true;
        request
    }

    /// Asks again without dropping what this listing holds, which is how a
    /// surface already on screen catches up with a catalog another client
    /// moved: the rows the reader is reading stand until the answer arrives,
    /// and the answer replaces them whole. Numbering the request is what
    /// supersedes whatever this listing was waiting for before.
    pub(super) fn catch_up(&mut self) -> SessionListRequest {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let request = SessionListRequest::new(
            self.surface,
            self.request_sequence,
            self.outlook.clone(),
            self.scope.clone(),
        );
        self.pending_request = Some(request.clone());
        request
    }

    pub(super) fn outlook(&self) -> &Outlook {
        &self.outlook
    }

    /// Widens the listing to every Workspace, or narrows it back to this
    /// client's own, and asks again. A failure the previous scope reported
    /// goes with it, because the reader is no longer looking at that listing.
    pub(super) fn toggle_scope(&mut self) -> SessionListRequest {
        self.scope = self.scope.toggled(&self.current_workspace);
        self.error = None;
        self.refresh()
    }

    /// Takes a listing the server answered with, most recently updated Session
    /// first, reporting whether it was the one this listing awaited — a caller with selection of its own reads the
    /// answer to know whether the Sessions beneath it moved.
    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        mut sessions: Vec<SessionListItem>,
    ) -> bool {
        if !self.awaits(request) {
            return false;
        }
        Self::order(&mut sessions);
        self.sessions = sessions;
        self.loading = false;
        self.pending_request = None;
        true
    }

    /// Whether a reply the server sent would move anything this listing holds.
    /// A catch-up after a change the client had already taken in place is
    /// answered with the listing already drawn, and an idle TUI must not pay a
    /// frame for that (ADR 0007) — nor for a straggler, which lands nowhere.
    pub(super) fn would_move(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        if !self.awaits(request) {
            return false;
        }
        // A listing still on its way, or one that failed, says so on screen,
        // so the answer moves the frame whatever Sessions it carries.
        if self.loading || self.error.is_some() {
            return true;
        }
        let mut arriving = sessions.to_vec();
        Self::order(&mut arriving);
        arriving != self.sessions
    }

    /// The one order a listing keeps: the most recently updated Session first.
    /// It is stable, so two Sessions last updated at the same moment keep the
    /// order the server listed them in — and a reply carrying what the listing
    /// already holds is recognised as the same listing rather than a new one.
    fn order(sessions: &mut [SessionListItem]) {
        sessions.sort_by_key(|summary| Reverse(summary.updated_at()));
    }

    /// Takes the server's refusal of a listing, reporting whether it answered
    /// the request this listing awaited.
    pub(super) fn fail(&mut self, request: &SessionListRequest, error: String) -> bool {
        if !self.awaits(request) {
            return false;
        }
        self.loading = false;
        self.error = Some(error);
        self.pending_request = None;
        true
    }

    /// Drops everything this listing holds and everything it awaits, so a
    /// surface closing over it leaves no reply to land behind its back.
    pub(super) fn clear(&mut self) {
        self.sessions.clear();
        self.loading = false;
        self.error = None;
        self.pending_request = None;
    }

    /// Takes a Session's newly derived Title and Emoji into a listing already
    /// drawn, so a Title landing while a surface shows it moves the row it is
    /// on rather than waiting for the reader to ask for the listing again.
    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        if let Some(summary) = self.readable_mut(session_id) {
            summary.title = title;
            summary.emoji = emoji;
        }
    }

    /// Records a Session the server reports set aside as done for now, or
    /// brought back. The order this listing keeps is by when a Session was last
    /// updated, which settling does not touch, so nothing moves.
    pub(super) fn settle(&mut self, session_id: SessionId, settled_at: Option<SessionTimestamp>) {
        if let Some(summary) = self.readable_mut(session_id) {
            summary.settled_at = settled_at;
        }
    }

    /// Records when the Session the server names began its uninterrupted
    /// subtree Working interval, or `None` when the last descendant Settles —
    /// which keeps a Working label true while Subagents outlive their parent
    /// Turn. The order this listing keeps is by when a Session was last
    /// updated, which this change does not carry, so nothing moves until the
    /// next listing lands.
    pub(super) fn set_working(
        &mut self,
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    ) {
        if let Some(summary) = self.readable_mut(session_id) {
            summary.session.working_since = working_since;
        }
    }

    /// The listed summary a catalog change names, for the changes that revise
    /// one Session in place. A Session Suru could not read carries no summary
    /// to revise, so it is passed over rather than reported missing.
    fn readable_mut(&mut self, session_id: SessionId) -> Option<&mut SessionSummary> {
        self.sessions.iter_mut().find_map(|session| match session {
            SessionListItem::Readable(summary) if summary.session.id == session_id => {
                Some(summary.as_mut())
            }
            _ => None,
        })
    }

    /// Drops a Session the server reports deleted.
    pub(super) fn remove(&mut self, session_id: SessionId) {
        self.sessions.retain(|summary| summary.id() != session_id);
    }

    /// Keeps only the Sessions a catalog reconciliation names, which is how a
    /// listing catches up on deletions it missed while disconnected.
    pub(super) fn retain(&mut self, session_ids: &[SessionId]) {
        self.sessions
            .retain(|summary| session_ids.contains(&summary.id()));
    }

    pub(super) fn sessions(&self) -> &[SessionListItem] {
        &self.sessions
    }

    /// The Workspaces this listing puts on offer: every one its Sessions are
    /// rooted in, and the one this client works in — which stands whether or
    /// not there is work in it yet, being where the reader's next Session
    /// will be.
    ///
    /// Each stands once. Deduplication is by path alone because both spellings
    /// that reach here are already the server's canonical reading: it roots a
    /// Session at one, and the client takes its own the same way, so two
    /// spellings of one directory never make two Workspaces.
    ///
    /// They come out in the order the listing keeps — newest work first — so a
    /// Workspace stands where its newest Session puts it and a surface wanting
    /// recency has that order already. One wanting another sorts what it
    /// derives. The Workspace this client works in, having no Session of its
    /// own to date it, comes last where the listing does not already name it.
    pub(super) fn workspaces(&self) -> Vec<PathBuf> {
        let mut workspaces = Vec::new();
        for session in &self.sessions {
            let Some(workspace) = session.workspace() else {
                continue;
            };
            if !workspaces.contains(&workspace.path) {
                workspaces.push(workspace.path.clone());
            }
        }
        if !workspaces.contains(&self.current_workspace) {
            workspaces.push(self.current_workspace.clone());
        }
        workspaces
    }

    pub(super) fn contains(&self, session_id: SessionId) -> bool {
        self.sessions
            .iter()
            .any(|summary| summary.id() == session_id)
    }

    pub(super) fn scope(&self) -> &SessionListScope {
        &self.scope
    }

    /// The Workspace this client runs in, which is what `CurrentWorkspace`
    /// scope means and what a surface narrowing to "where I am" narrows to.
    pub(super) fn current_workspace(&self) -> &Path {
        &self.current_workspace
    }

    /// Takes the Workspace this client has moved to. A listing narrowed to the
    /// one it was made for follows the reader across, so "where I am" goes on
    /// meaning where they are; one asking for every Workspace is already asking
    /// for the new one and is left alone. Nothing is asked of the server here:
    /// a surface that wants its listing to answer for the new Workspace asks
    /// for it.
    pub(super) fn adopt_current_workspace(&mut self, workspace: PathBuf) {
        if matches!(self.scope, SessionListScope::CurrentWorkspace(_)) {
            self.scope = SessionListScope::CurrentWorkspace(workspace.clone());
        }
        self.current_workspace = workspace;
    }

    /// Turns this listing toward another Server. Rows and in-flight replies
    /// belong to the old Outlook and cannot cross into the new one.
    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        if self.outlook == outlook {
            return;
        }
        self.outlook = outlook;
        self.clear();
    }

    pub(super) const fn is_loading(&self) -> bool {
        self.loading
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Reports a failure that did not come from a listing request — an
    /// attachment the server refused, say — beside the Sessions on show.
    pub(super) fn report_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn clear_error(&mut self) {
        self.error = None;
    }

    /// Whether this listing is still waiting for the reply to `request`. Any
    /// other reply is a straggler from a listing the reader has moved past.
    pub(super) fn awaits(&self, request: &SessionListRequest) -> bool {
        self.pending_request.as_ref() == Some(request)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        protocol::{
            ModelAvailability, Session, SessionId, SessionListItem, SessionStatus, SessionSummary,
            SessionTimestamp, UnreadableSessionSummary, Workspace,
        },
        tui::{SessionListScope, SessionListSurface, session_listing::SessionListing},
    };

    #[test]
    fn a_reply_to_a_superseded_request_is_dropped_and_the_current_one_lands() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let superseded = listing.refresh();
        let current = listing.refresh();

        assert!(
            !listing.load(&superseded, vec![summary("Stale", 3)]),
            "a reply to a superseded request is not the listing the reader asked for"
        );
        assert!(listing.sessions().is_empty(), "the stale reply landed");
        assert!(listing.load(&current, vec![summary("Fresh", 3)]));
        assert_eq!(titles(&listing), vec!["Fresh"]);
        assert!(!listing.is_loading(), "the reply ended the wait");
    }

    #[test]
    fn a_listing_orders_its_sessions_most_recently_updated_first() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();

        listing.load(
            &request,
            vec![
                summary("Older", 1),
                summary("Newest", 9),
                summary("Older", 5),
            ],
        );

        assert_eq!(titles(&listing), vec!["Newest", "Older", "Older"]);
    }

    #[test]
    fn toggling_scope_widens_to_every_workspace_and_asks_again() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.fail(&request, "unreachable".to_owned());

        let widened = listing.toggle_scope();

        assert_eq!(listing.scope(), &SessionListScope::AllWorkspaces);
        assert_eq!(widened.scope(), &SessionListScope::AllWorkspaces);
        assert!(
            listing.error().is_none(),
            "asking again leaves the previous failure behind"
        );
        assert!(listing.is_loading());

        let narrowed = listing.toggle_scope();

        assert_eq!(
            narrowed.scope(),
            &SessionListScope::CurrentWorkspace(workspace.path().to_owned()),
            "narrowing returns to the Workspace the listing was made for"
        );
    }

    #[test]
    fn a_failure_reports_against_the_request_it_answers() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let superseded = listing.refresh();
        let current = listing.refresh();

        assert!(!listing.fail(&superseded, "stale".to_owned()));
        assert!(listing.error().is_none());
        assert!(listing.fail(&current, "unreachable".to_owned()));
        assert_eq!(listing.error(), Some("unreachable"));
        assert!(!listing.is_loading());
    }

    #[test]
    fn a_derived_title_and_emoji_land_on_the_session_they_name() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Ask about tests", 1)]);
        let session_id = listing.sessions()[0].id();

        listing.retitle(session_id, "Testing".to_owned(), Some("🧪".to_owned()));

        assert_eq!(titles(&listing), vec!["Testing"]);
        assert_eq!(listing.sessions()[0].emoji(), Some("🧪"));
    }

    #[test]
    fn a_settle_lands_on_the_session_it_names_without_moving_the_order() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Newer", 2), summary("Set aside", 1)]);
        let session_id = listing.sessions()[1].id();

        listing.settle(session_id, Some(SessionTimestamp(9)));

        assert_eq!(titles(&listing), vec!["Newer", "Set aside"]);
        assert_eq!(
            listing.sessions()[1].settled_at(),
            Some(SessionTimestamp(9))
        );
        assert_eq!(listing.sessions()[0].settled_at(), None);

        listing.settle(session_id, None);

        assert_eq!(listing.sessions()[1].settled_at(), None);
    }

    #[test]
    fn a_working_change_lands_on_the_session_it_names_without_moving_the_order() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Newer", 2), summary("Working", 1)]);
        let session_id = listing.sessions()[1].id();

        listing.set_working(session_id, Some(SessionTimestamp(9)));

        assert_eq!(titles(&listing), vec!["Newer", "Working"]);
        assert_eq!(
            listing.sessions()[1].working_since(),
            Some(SessionTimestamp(9)),
            "the row the change names says its work is live"
        );
        assert_eq!(listing.sessions()[0].working_since(), None);

        listing.set_working(session_id, None);

        assert_eq!(
            listing.sessions()[1].working_since(),
            None,
            "and the Turn settling clears the reading"
        );
    }

    #[test]
    fn a_deleted_session_leaves_the_listing() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Kept", 2), summary("Deleted", 1)]);
        let deleted = listing.sessions()[1].id();

        listing.remove(deleted);

        assert_eq!(titles(&listing), vec!["Kept"]);
        assert!(!listing.contains(deleted));
    }

    #[test]
    fn a_catalog_reconciliation_keeps_only_the_sessions_it_names() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Kept", 2), summary("Gone", 1)]);
        let kept = listing.sessions()[0].id();

        listing.retain(&[kept]);

        assert_eq!(titles(&listing), vec!["Kept"]);
    }

    #[test]
    fn clearing_a_listing_drops_what_it_held_and_what_it_awaited() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Listed", 1)]);
        listing.fail(&request, "unreachable".to_owned());
        let in_flight = listing.refresh();

        listing.clear();

        assert!(listing.sessions().is_empty());
        assert!(listing.error().is_none());
        assert!(!listing.is_loading());
        assert!(
            !listing.load(&in_flight, vec![summary("Late", 1)]),
            "a reply to a listing nobody is waiting for lands nowhere"
        );
    }

    #[test]
    fn a_failure_of_its_own_stands_beside_the_sessions_already_listed() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let request = listing.refresh();
        listing.load(&request, vec![summary("Listed", 1)]);

        listing.report_error("the server refused the attachment".to_owned());

        assert_eq!(listing.error(), Some("the server refused the attachment"));
        assert_eq!(
            titles(&listing),
            vec!["Listed"],
            "a failure of the reader's own making leaves the Sessions on show"
        );

        listing.clear_error();

        assert!(listing.error().is_none());
        assert_eq!(titles(&listing), vec!["Listed"]);
    }

    #[test]
    fn the_workspaces_a_listing_derives_stand_newest_work_first_and_include_this_client_s_own() {
        let mut listing = SessionListing::scoped(
            SessionListSurface::WorkspacePicker,
            root().join("here"),
            SessionListScope::AllWorkspaces,
        );
        let request = listing.refresh();

        listing.load(
            &request,
            vec![
                rooted("Older there", &root().join("there"), 2),
                rooted("Newest elsewhere", &root().join("elsewhere"), 9),
                rooted("Newer there", &root().join("there"), 7),
            ],
        );

        assert_eq!(
            listing.workspaces(),
            vec![
                root().join("elsewhere"),
                root().join("there"),
                root().join("here"),
            ],
            "each Workspace stands once, where its newest Session puts it, and the one \
             this client works in stands whether or not any work is rooted there yet"
        );
    }

    #[test]
    fn a_workspace_this_client_works_in_and_has_worked_in_stands_once() {
        let mut listing = SessionListing::scoped(
            SessionListSurface::WorkspacePicker,
            root().join("here"),
            SessionListScope::AllWorkspaces,
        );
        let request = listing.refresh();

        listing.load(&request, vec![rooted("Work here", &root().join("here"), 4)]);

        assert_eq!(listing.workspaces(), vec![root().join("here")]);
    }

    /// A Session listed without a Workspace says nothing about where it was
    /// rooted, so it puts no Workspace on offer — there is no path a reader
    /// could be taken to.
    #[test]
    fn a_session_listed_without_a_workspace_puts_none_on_offer() {
        let mut listing = SessionListing::scoped(
            SessionListSurface::WorkspacePicker,
            root().join("here"),
            SessionListScope::AllWorkspaces,
        );
        let request = listing.refresh();

        listing.load(
            &request,
            vec![SessionListItem::Unreadable(UnreadableSessionSummary {
                id: SessionId::new(),
                title: "Unreadable".to_owned(),
                created_at: SessionTimestamp(1),
                updated_at: SessionTimestamp(8),
                workspace: None,
            })],
        );

        assert_eq!(listing.workspaces(), vec![root().join("here")]);
    }

    fn titles(listing: &SessionListing) -> Vec<&str> {
        listing
            .sessions()
            .iter()
            .map(SessionListItem::title)
            .collect()
    }

    /// A listed Session rooted at the Workspace the caller names, which is
    /// what a listing spanning several Workspaces is made of.
    fn rooted(title: &str, workspace: &Path, updated_at: u64) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, updated_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.session.workspace = Workspace {
            path: workspace.to_owned(),
        };
        SessionListItem::Readable(listed)
    }

    fn summary(title: &str, updated_at: u64) -> SessionListItem {
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
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(updated_at),
        }))
    }

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
