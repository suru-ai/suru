//! Shared Session listing: the request sequencing, stale-reply dropping,
//! Workspace scope, and catalog reconciliation that every surface listing
//! Sessions needs, held in one place so no surface reimplements them.

use std::{cmp::Reverse, collections::HashMap, ops::Deref};

use crate::protocol::{
    Outlook, Remote, RemoteStatus, SessionId, SessionListItem, SessionReference,
    SessionStandingInputs, SessionSummary, SessionTimestamp,
};

use super::{SessionListRequest, SessionListScope, SessionListSurface};

/// One surface's view of the Sessions its Origin servers hold.
///
/// A listing owns the conversation with the server — it numbers each request
/// so a reply to a superseded one is dropped, remembers which Workspace scope
/// the reader asked for, and keeps what it holds true as the session-catalog
/// stream reports retitles, deletions, and reconciliations. Each Origin owns
/// its request sequence, reply validity, rows, and loading result. What the
/// listing does not own is how those Sessions read: within each Origin it
/// keeps them most recently updated first, and a surface that wants another
/// order sections and sorts what it derives from them.
#[derive(Clone, Debug)]
pub(super) struct SessionListing {
    /// The surface this listing belongs to, stamped on every request it makes
    /// so a reply lands on the listing that asked and on no other.
    surface: SessionListSurface,
    outlook: Outlook,
    /// The Workspace `CurrentWorkspace` scope means, kept so narrowing back to
    /// it needs nothing from the caller.
    current_workspace: crate::protocol::Workspace,
    scope: SessionListScope,
    origins: HashMap<Outlook, OriginListing>,
}

/// One row held by a Client listing: the wire item together with the Origin
/// that gives its Session identity meaning. The server has no merged-listing
/// concept, so the Client stamps this reference when it accepts a reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ListedSession {
    reference: SessionReference,
    item: SessionListItem,
}

impl ListedSession {
    fn new(origin: Outlook, item: SessionListItem) -> Self {
        let reference = SessionReference::new(origin, item.id());
        Self { reference, item }
    }

    fn stamp_all(origin: &Outlook, sessions: Vec<SessionListItem>) -> Vec<Self> {
        sessions
            .into_iter()
            .map(|session| Self::new(origin.clone(), session))
            .collect()
    }

    pub(super) fn reference(&self) -> &SessionReference {
        &self.reference
    }
}

impl Deref for ListedSession {
    type Target = SessionListItem;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

/// The rows and request conversation belonging to one Origin. Keeping the
/// sequence beside the rows prevents a reply for one Server from superseding
/// or validating a reply for another.
#[derive(Clone, Debug, Default)]
struct OriginListing {
    request_sequence: u64,
    /// The request whose reply this listing is waiting for. Any other reply is
    /// a straggler from a listing the reader has already moved past.
    pending_request: Option<SessionListRequest>,
    sessions: Vec<ListedSession>,
    loading: bool,
    error: Option<String>,
}

/// The Origins an Everywhere surface asks, preserving the local Server first
/// and the durable Remote order after it. Ended Pairings cannot answer and do
/// not participate; an unavailable one may recover and remains included.
pub(super) fn everywhere_origins(remotes: Vec<Remote>) -> Vec<Outlook> {
    let mut origins = vec![Outlook::Local];
    origins.extend(remotes.into_iter().filter_map(|remote| {
        (!matches!(
            remote.status,
            RemoteStatus::Revoked | RemoteStatus::ProtocolMismatch
        ))
        .then_some(Outlook::Remote(remote.name))
    }));
    origins
}

impl SessionListing {
    /// A listing scoped, as it opens, to the Workspace this client runs in.
    pub(super) fn new(
        surface: SessionListSurface,
        current_workspace: impl Into<crate::protocol::Workspace>,
    ) -> Self {
        let current_workspace = current_workspace.into();
        let scope = SessionListScope::CurrentWorkspace(current_workspace.clone());
        Self::scoped(surface, current_workspace, scope)
    }

    /// A listing that opens on a scope of the caller's choosing, for a surface
    /// whose opening scope is not the Workspace the client runs in.
    pub(super) fn scoped(
        surface: SessionListSurface,
        current_workspace: impl Into<crate::protocol::Workspace>,
        scope: SessionListScope,
    ) -> Self {
        Self {
            surface,
            outlook: Outlook::Local,
            scope,
            current_workspace: current_workspace.into(),
            origins: HashMap::new(),
        }
    }

    /// Asks the server for the Sessions in scope, dropping what this listing
    /// holds: what it held answered a question the reader has moved past. The
    /// caller hands the returned request to the runtime and back to `load` or
    /// `fail` when the answer arrives.
    pub(super) fn refresh(&mut self) -> SessionListRequest {
        self.refresh_origin(self.outlook.clone())
    }

    /// Asks one named Origin for its Sessions, leaving every other Origin's
    /// request conversation and rows alone.
    pub(super) fn refresh_origin(&mut self, outlook: Outlook) -> SessionListRequest {
        let request = self.catch_up_origin(outlook);
        let origin = self.origin_mut(request.outlook().clone());
        origin.sessions.clear();
        origin.loading = true;
        request
    }

    /// Asks again without dropping what this listing holds, which is how a
    /// surface already on screen catches up with a catalog another client
    /// moved: the rows the reader is reading stand until the answer arrives,
    /// and the answer replaces them whole. Numbering the request is what
    /// supersedes whatever this listing was waiting for before.
    /// Asks one named Origin again without disturbing rows held for any
    /// Origin, including the one being refreshed.
    pub(super) fn catch_up_origin(&mut self, outlook: Outlook) -> SessionListRequest {
        let surface = self.surface;
        let scope = self.scope.clone();
        let origin = self.origin_mut(outlook.clone());
        origin.request_sequence = origin.request_sequence.wrapping_add(1);
        let request = SessionListRequest::new(surface, origin.request_sequence, outlook, scope);
        origin.pending_request = Some(request.clone());
        request
    }

    /// Asks one named Origin again as the reader turns back toward it. The
    /// rows held from their last visit are still that Origin's own answer, so
    /// they stand until the reply replaces them whole rather than blanking a
    /// column that was already true. An Origin holding nothing says it is
    /// loading, as a first visit should.
    pub(super) fn revisit_origin(&mut self, outlook: Outlook) -> SessionListRequest {
        let request = self.catch_up_origin(outlook);
        let origin = self.origin_mut(request.outlook().clone());
        origin.loading = origin.sessions.is_empty();
        request
    }

    /// Starts one fresh listing conversation per Origin. The caller dispatches
    /// the returned requests independently, so a slow Server cannot hold up or
    /// invalidate any other Server's answer.
    pub(super) fn refresh_origins(&mut self, origins: &[Outlook]) -> Vec<SessionListRequest> {
        origins
            .iter()
            .cloned()
            .map(|origin| self.refresh_origin(origin))
            .collect()
    }

    /// Widens the listing to every Workspace, or narrows it back to this
    /// client's own, and asks again. A failure the previous scope reported
    /// goes with it, because the reader is no longer looking at that listing.
    #[cfg(test)]
    pub(super) fn toggle_scope(&mut self) -> SessionListRequest {
        self.scope = self.scope.toggled(&self.current_workspace);
        self.clear_error();
        self.refresh()
    }

    /// Moves to an explicitly chosen server listing scope and asks the
    /// Outlook again. Surfaces with more than two presentation scopes use
    /// this instead of the listing's two-state convenience toggle.
    pub(super) fn refresh_in(&mut self, scope: SessionListScope) -> SessionListRequest {
        self.scope = scope;
        self.refresh()
    }

    /// Changes the server scope without beginning a listing conversation.
    /// Everywhere surfaces use this while paired-Remote discovery is the
    /// first request still in flight.
    pub(super) fn adopt_scope(&mut self, scope: SessionListScope) {
        self.scope = scope;
    }

    /// Takes a listing the server answered with, stamping every row with the
    /// request's Outlook and keeping the most recently updated Session first.
    /// Reports whether the accepted reply belongs to the Origin on show — a
    /// caller with selection of its own reads that to know whether the rows
    /// beneath it moved.
    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        mut sessions: Vec<SessionListItem>,
    ) -> bool {
        let is_current = request.outlook() == &self.outlook;
        let Some(origin) = self.awaited_origin_mut(request) else {
            return false;
        };
        Self::preserve_newer_availability(&origin.sessions, &mut sessions);
        Self::order(&mut sessions);
        origin.sessions = ListedSession::stamp_all(request.outlook(), sessions);
        origin.loading = false;
        origin.pending_request = None;
        is_current
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
        if request.outlook() != &self.outlook {
            return false;
        }
        self.would_move_origin(request, sessions)
    }

    /// The same movement reading for a surface currently drawing more than
    /// one Origin at once.
    pub(super) fn would_move_across(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        self.awaits(request) && self.would_move_origin(request, sessions)
    }

    fn would_move_origin(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        let origin = self.origins.get(request.outlook());
        // A listing still on its way, or one that failed, says so on screen,
        // so the answer moves the frame whatever Sessions it carries.
        if origin.is_some_and(|origin| origin.loading || origin.error.is_some()) {
            return true;
        }
        let mut arriving = sessions.to_vec();
        if let Some(origin) = origin {
            Self::preserve_newer_availability(&origin.sessions, &mut arriving);
        }
        Self::order(&mut arriving);
        let arriving = ListedSession::stamp_all(request.outlook(), arriving);
        origin.is_none_or(|origin| arriving != origin.sessions)
    }

    /// The one order a listing keeps: the most recently updated Session first.
    /// It is stable, so two Sessions last updated at the same moment keep the
    /// order the server listed them in — and a reply carrying what the listing
    /// already holds is recognised as the same listing rather than a new one.
    fn order(sessions: &mut [SessionListItem]) {
        sessions.sort_by_key(|summary| Reverse(summary.updated_at()));
    }

    /// Takes the server's refusal of a listing, reporting whether it answered
    /// the request for the Origin on show.
    pub(super) fn fail(&mut self, request: &SessionListRequest, error: String) -> bool {
        let is_current = request.outlook() == &self.outlook;
        let Some(origin) = self.awaited_origin_mut(request) else {
            return false;
        };
        origin.loading = false;
        origin.error = Some(error);
        origin.pending_request = None;
        is_current
    }

    /// Drops every Origin's rows, result, and pending request, so a surface
    /// closing over it leaves no reply to land behind its back. Request
    /// sequences survive: reopening must never make a pre-close request valid
    /// again.
    pub(super) fn clear(&mut self) {
        for origin in self.origins.values_mut() {
            origin.sessions.clear();
            origin.loading = false;
            origin.error = None;
            origin.pending_request = None;
        }
    }

    /// Takes a Session's newly derived Title and Emoji into a listing already
    /// drawn, so a Title landing while a surface shows it moves the row it is
    /// on rather than waiting for the reader to ask for the listing again.
    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        self.retitle_origin(self.outlook.clone(), session_id, title, emoji);
    }

    pub(super) fn retitle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        title: String,
        emoji: Option<String>,
    ) {
        if let Some(summary) = self.readable_mut(&outlook, session_id) {
            summary.title = title;
            summary.emoji = emoji;
        }
    }

    /// Records a Session the server reports set aside as done for now, or
    /// brought back. The order this listing keeps is by when a Session was last
    /// updated, which settling does not touch, so nothing moves.
    pub(super) fn settle(&mut self, session_id: SessionId, settled_at: Option<SessionTimestamp>) {
        self.settle_origin(self.outlook.clone(), session_id, settled_at);
    }

    pub(super) fn settle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        settled_at: Option<SessionTimestamp>,
    ) {
        if let Some(summary) = self.readable_mut(&outlook, session_id) {
            summary.settled_at = settled_at;
        }
    }

    /// Records when the Session the server names began its uninterrupted
    /// subtree Working interval, or `None` when the last descendant Settles —
    /// which keeps a Working label true while Subagents outlive their parent
    /// Turn. The order this listing keeps is by when a Session was last
    /// updated, which this change does not carry, so nothing moves until the
    /// next listing lands.
    pub(super) fn set_working_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    ) {
        if let Some(summary) = self.readable_mut(&outlook, session_id) {
            summary.session.working_since = working_since;
        }
    }

    /// Replaces the server facts from which this Session's Standing is read.
    /// The catalog carries them whole so a row already drawn can change its
    /// Rail and right slot before the catch-up listing arrives.
    pub(super) fn set_standing_inputs_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        mut standing_inputs: SessionStandingInputs,
    ) {
        if let Some(summary) = self.readable_mut(&outlook, session_id) {
            Self::preserve_newer_standing_inputs(&summary.standing_inputs, &mut standing_inputs);
            summary.standing_inputs = standing_inputs;
        }
    }

    /// A listing reply may have been captured before a newer catalog event
    /// reached this Client. Preserve the newer typed availability while still
    /// taking every unrelated field from the reply.
    fn preserve_newer_availability(known: &[ListedSession], arriving: &mut [SessionListItem]) {
        for item in arriving {
            let Some(known) = known.iter().find(|known| known.id() == item.id()) else {
                continue;
            };
            let (Some(known), SessionListItem::Readable(arriving)) = (known.readable(), item)
            else {
                continue;
            };
            Self::preserve_newer_standing_inputs(
                &known.standing_inputs,
                &mut arriving.standing_inputs,
            );
        }
    }

    fn preserve_newer_standing_inputs(
        known: &SessionStandingInputs,
        arriving: &mut SessionStandingInputs,
    ) {
        if arriving.pending_questionnaires_revision.0 < known.pending_questionnaires_revision.0 {
            arriving
                .pending_questionnaires
                .clone_from(&known.pending_questionnaires);
            arriving
                .submitting_questionnaires
                .clone_from(&known.submitting_questionnaires);
            arriving.pending_questionnaires_revision = known.pending_questionnaires_revision;
        }
        if arriving.pending_approvals_revision.0 < known.pending_approvals_revision.0 {
            arriving
                .pending_approvals
                .clone_from(&known.pending_approvals);
            arriving
                .submitting_approvals
                .clone_from(&known.submitting_approvals);
            arriving.pending_approvals_revision = known.pending_approvals_revision;
        }
        for known in &known.subagent_interventions {
            match arriving
                .subagent_interventions
                .iter_mut()
                .find(|entry| entry.session_id == known.session_id)
            {
                Some(incoming) if incoming.revision.0 < known.revision.0 => {
                    *incoming = known.clone()
                }
                None => arriving.subagent_interventions.push(known.clone()),
                _ => {}
            }
        }
    }

    /// The listed summary a catalog change names, for the changes that revise
    /// one Session in place. A Session Suru could not read carries no summary
    /// to revise, so it is passed over rather than reported missing.
    fn readable_mut(
        &mut self,
        outlook: &Outlook,
        session_id: SessionId,
    ) -> Option<&mut SessionSummary> {
        self.origin_mut(outlook.clone())
            .sessions
            .iter_mut()
            .find_map(|session| match &mut session.item {
                SessionListItem::Readable(summary) if summary.session.id == session_id => {
                    Some(summary.as_mut())
                }
                _ => None,
            })
    }

    /// Drops a Session the server reports deleted.
    pub(super) fn remove(&mut self, session_id: SessionId) {
        self.remove_origin(self.outlook.clone(), session_id);
    }

    pub(super) fn remove_origin(&mut self, outlook: Outlook, session_id: SessionId) {
        self.origin_mut(outlook)
            .sessions
            .retain(|summary| summary.id() != session_id);
    }

    /// Keeps only the Sessions a catalog reconciliation names, which is how a
    /// listing catches up on deletions it missed while disconnected.
    pub(super) fn retain(&mut self, session_ids: &[SessionId]) {
        self.retain_origin(self.outlook.clone(), session_ids);
    }

    pub(super) fn retain_origin(&mut self, outlook: Outlook, session_ids: &[SessionId]) {
        self.origin_mut(outlook)
            .sessions
            .retain(|summary| session_ids.contains(&summary.id()));
    }

    pub(super) fn sessions(&self) -> &[ListedSession] {
        self.current_origin()
            .map_or(&[], |origin| origin.sessions.as_slice())
    }

    /// Rows held for the named Origins, in Origin order. Surfaces merging
    /// these rows impose their own shelf order afterwards.
    pub(super) fn sessions_across(&self, origins: &[Outlook]) -> Vec<&ListedSession> {
        origins
            .iter()
            .filter_map(|origin| self.origins.get(origin))
            .flat_map(|origin| origin.sessions.iter())
            .collect()
    }

    /// Forgets the rows and in-flight reply validity of Origins no longer in a
    /// merged listing, while retaining their monotonically increasing request
    /// sequence. A Remote can leave and later rejoin discovery; its next ask
    /// must not reuse an id that a delayed reply from its previous membership
    /// still carries.
    pub(super) fn retain_origins(&mut self, origins: &[Outlook]) {
        for (outlook, origin) in &mut self.origins {
            if origins.contains(outlook) {
                continue;
            }
            origin.pending_request = None;
            origin.sessions.clear();
            origin.loading = false;
            origin.error = None;
        }
    }

    /// Forgets one Origin's rows and any request still in flight for it.
    pub(super) fn remove_origin_catalog(&mut self, outlook: &Outlook) {
        self.origins.remove(outlook);
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
    pub(super) fn workspaces(&self) -> Vec<crate::protocol::Workspace> {
        let mut workspaces: Vec<crate::protocol::Workspace> = Vec::new();
        for session in self.sessions() {
            let Some(workspace) = session.workspace() else {
                continue;
            };
            if !workspaces.iter().any(|known| known.id == workspace.id) {
                workspaces.push(workspace.clone());
            }
        }
        if !workspaces
            .iter()
            .any(|known| known.id == self.current_workspace.id)
        {
            workspaces.push(self.current_workspace.clone());
        }
        workspaces
    }

    pub(super) fn contains(&self, reference: &SessionReference) -> bool {
        self.origins.get(&reference.origin).is_some_and(|origin| {
            origin
                .sessions
                .iter()
                .any(|summary| summary.reference() == reference)
        })
    }

    #[cfg(test)]
    pub(super) fn scope(&self) -> &SessionListScope {
        &self.scope
    }

    pub(super) fn outlook(&self) -> &Outlook {
        &self.outlook
    }

    /// The Workspace this client runs in, which is what `CurrentWorkspace`
    /// scope means and what a surface narrowing to "where I am" narrows to.
    pub(super) fn current_workspace(&self) -> &crate::protocol::Workspace {
        &self.current_workspace
    }

    /// Takes the Workspace this client has moved to. A listing narrowed to the
    /// one it was made for follows the reader across, so "where I am" goes on
    /// meaning where they are; one asking for every Workspace is already asking
    /// for the new one and is left alone. Nothing is asked of the server here:
    /// a surface that wants its listing to answer for the new Workspace asks
    /// for it.
    pub(super) fn adopt_current_workspace(
        &mut self,
        workspace: impl Into<crate::protocol::Workspace>,
    ) {
        let workspace = workspace.into();
        if matches!(self.scope, SessionListScope::CurrentWorkspace(_)) {
            self.scope = SessionListScope::CurrentWorkspace(workspace.clone());
        }
        self.current_workspace = workspace;
    }

    /// Turns this listing toward another Origin. Its rows and request state
    /// become the ones the surface reads; every other Origin's state remains
    /// held separately and no reply can cross between them.
    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.outlook = outlook;
    }

    pub(super) fn is_loading(&self) -> bool {
        self.current_origin().is_some_and(|origin| origin.loading)
    }

    pub(super) fn is_loading_across(&self, origins: &[Outlook]) -> bool {
        origins.iter().any(|origin| {
            self.origins
                .get(origin)
                .is_some_and(|listing| listing.loading)
        })
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.current_origin()
            .and_then(|origin| origin.error.as_deref())
    }

    pub(super) fn error_across(&self, origins: &[Outlook]) -> Option<&str> {
        origins.iter().find_map(|origin| {
            self.origins
                .get(origin)
                .and_then(|listing| listing.error.as_deref())
        })
    }

    /// Reports a failure that did not come from a listing request — an
    /// attachment the server refused, say — beside the Sessions on show.
    pub(super) fn report_error(&mut self, error: String) {
        self.current_origin_mut().error = Some(error);
    }

    pub(super) fn clear_error(&mut self) {
        self.current_origin_mut().error = None;
    }

    /// Whether this listing is still waiting for the reply to `request`. Any
    /// other reply is a straggler from a listing the reader has moved past.
    pub(super) fn awaits(&self, request: &SessionListRequest) -> bool {
        self.origins
            .get(request.outlook())
            .and_then(|origin| origin.pending_request.as_ref())
            == Some(request)
    }

    fn awaited_origin_mut(&mut self, request: &SessionListRequest) -> Option<&mut OriginListing> {
        self.origins
            .get_mut(request.outlook())
            .filter(|origin| origin.pending_request.as_ref() == Some(request))
    }

    fn current_origin(&self) -> Option<&OriginListing> {
        self.origins.get(&self.outlook)
    }

    fn current_origin_mut(&mut self) -> &mut OriginListing {
        self.origins.entry(self.outlook.clone()).or_default()
    }

    fn origin_mut(&mut self, outlook: Outlook) -> &mut OriginListing {
        self.origins.entry(outlook).or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        protocol::{
            ModelAvailability, Outlook, Session, SessionId, SessionListItem, SessionReference,
            SessionStatus, SessionSummary, SessionTimestamp, UnreadableSessionSummary, Workspace,
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
    fn each_origin_keeps_its_own_rows_and_reply_sequence() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let local = listing.refresh();
        let studio = crate::protocol::Outlook::Remote("studio".to_owned());
        listing.adopt_outlook(studio.clone());
        let remote = listing.refresh();

        assert!(listing.load(&remote, vec![summary("Remote", 2)]));
        assert_eq!(titles(&listing), vec!["Remote"]);
        assert_eq!(listing.sessions()[0].reference().origin, studio);

        assert!(
            !listing.load(&local, vec![summary("Local", 1)]),
            "a valid reply for another Origin does not move the Origin on show"
        );
        assert_eq!(titles(&listing), vec!["Remote"]);

        listing.adopt_outlook(crate::protocol::Outlook::Local);
        assert_eq!(titles(&listing), vec!["Local"]);
        assert_eq!(
            listing.sessions()[0].reference().origin,
            crate::protocol::Outlook::Local
        );
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
            &SessionListScope::CurrentWorkspace((workspace.path().to_owned()).into()),
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

        listing.set_working_origin(Outlook::Local, session_id, Some(SessionTimestamp(9)));

        assert_eq!(titles(&listing), vec!["Newer", "Working"]);
        assert_eq!(
            listing.sessions()[1].working_since(),
            Some(SessionTimestamp(9)),
            "the row the change names says its work is live"
        );
        assert_eq!(listing.sessions()[0].working_since(), None);

        listing.set_working_origin(Outlook::Local, session_id, None);

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
        assert!(!listing.contains(&SessionReference::new(Outlook::Local, deleted)));
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
        let reopened = listing.refresh();
        assert!(
            !listing.load(&in_flight, vec![summary("Late", 1)]),
            "a pre-close reply cannot collide with the request made after reopening"
        );
        assert!(listing.load(&reopened, vec![summary("Reopened", 2)]));
        assert_eq!(titles(&listing), vec!["Reopened"]);
    }

    #[test]
    fn reopening_does_not_reuse_the_request_sequence_from_before_clear() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let before_close = listing.refresh();

        listing.clear();
        let after_reopen = listing.refresh();

        assert!(
            !listing.load(&before_close, vec![summary("Late", 1)]),
            "the old request ID is not valid again after reopening"
        );
        assert!(listing.load(&after_reopen, vec![summary("Current", 2)]));
    }

    #[test]
    fn an_origin_returning_to_everywhere_does_not_reuse_its_old_request_sequence() {
        let workspace = tempfile::tempdir().expect("create Workspace");
        let mut listing = SessionListing::new(
            SessionListSurface::SessionPicker,
            workspace.path().to_owned(),
        );
        let remote = Outlook::Remote("studio".to_owned());
        let stale = listing.refresh_origin(remote.clone());

        listing.retain_origins(&[Outlook::Local]);
        let current = listing.refresh_origin(remote);

        assert!(
            !listing.load(&stale, vec![summary("Stale", 1)]),
            "a reply from before the Origin left Everywhere cannot validate again"
        );
        listing.load(&current, vec![summary("Current", 2)]);
        assert!(
            !listing.awaits(&current),
            "the current reply lands even though another Origin is on show"
        );
        listing.adopt_outlook(Outlook::Remote("studio".to_owned()));
        assert_eq!(titles(&listing), vec!["Current"]);
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
            listing
                .workspaces()
                .into_iter()
                .map(|workspace| workspace.path)
                .collect::<Vec<_>>(),
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

        assert_eq!(
            listing
                .workspaces()
                .into_iter()
                .map(|workspace| workspace.path)
                .collect::<Vec<_>>(),
            vec![root().join("here")]
        );
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

        assert_eq!(
            listing
                .workspaces()
                .into_iter()
                .map(|workspace| workspace.path)
                .collect::<Vec<_>>(),
            vec![root().join("here")]
        );
    }

    fn titles(listing: &SessionListing) -> Vec<&str> {
        listing
            .sessions()
            .iter()
            .map(|session| session.title())
            .collect()
    }

    /// A listed Session rooted at the Workspace the caller names, which is
    /// what a listing spanning several Workspaces is made of.
    fn rooted(title: &str, workspace: &Path, updated_at: u64) -> SessionListItem {
        let SessionListItem::Readable(mut listed) = summary(title, updated_at) else {
            unreachable!("the fixture builds a readable Session");
        };
        listed.session.workspace = Workspace::directory(workspace.to_owned());
        SessionListItem::Readable(listed)
    }

    fn summary(title: &str, updated_at: u64) -> SessionListItem {
        SessionListItem::Readable(Box::new(SessionSummary {
            checkout_state: None,
            session: Session {
                checkout: None,
                context_fill: None,
                id: SessionId::new(),
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: root().join("workspace"),
                },
                workspace: Workspace::directory(root().join("workspace")),
                agent_selection: None,
                agent_selection_availability: ModelAvailability::Available,
                status: SessionStatus::Idle,
                working_since: None,
                parent: None,
            },
            title: title.to_owned(),
            emoji: None,
            settled_at: None,
            standing_inputs: Default::default(),
            total_usage: None,
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(updated_at),
        }))
    }

    fn root() -> PathBuf {
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned()
    }
}
