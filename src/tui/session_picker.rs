//! Session switcher state and the query that narrows it.

use std::{
    cmp::Reverse,
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::protocol::{
    Outlook, Remote, SessionId, SessionListItem, SessionReference, SessionStatus, SessionTimestamp,
};

use super::{
    EverywhereListRequest, SessionListRequest, SessionListScope, SessionListSurface,
    fuzzy::fuzzy_matches,
    session_listing::{ListedSession, SessionListing, everywhere_origins},
};

const CURRENT_WORKSPACE: &str = "Current Workspace";
const ALL_WORKSPACES: &str = "All Workspaces";
const EVERYWHERE: &str = "Everywhere";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPickerScope {
    CurrentWorkspace,
    AllWorkspaces,
    Everywhere,
}

impl SessionPickerScope {
    fn next(self) -> Self {
        match self {
            Self::CurrentWorkspace => Self::AllWorkspaces,
            Self::AllWorkspaces => Self::Everywhere,
            Self::Everywhere => Self::CurrentWorkspace,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CurrentWorkspace => CURRENT_WORKSPACE,
            Self::AllWorkspaces => ALL_WORKSPACES,
            Self::Everywhere => EVERYWHERE,
        }
    }
}

pub(super) enum SessionPickerListing {
    Origin(SessionListRequest),
    Everywhere(EverywhereListRequest),
}

#[derive(Clone, Debug)]
pub(super) struct SessionPicker {
    open: bool,
    /// The Sessions on offer, and the conversation with the server that keeps
    /// them true. The picker holds only what it does with them: the query it
    /// filters by and the row the reader is on.
    listing: SessionListing,
    scope: SessionPickerScope,
    /// The Origins participating in Everywhere, local first and followed by
    /// each paired non-terminal Remote in the local Server's order.
    everywhere_origins: Vec<Outlook>,
    everywhere_remote_sequence: u64,
    pending_everywhere_remotes: Option<u64>,
    awaiting_dispatch: Vec<SessionListRequest>,
    query: String,
    selected: Option<SessionReference>,
    attaching: Option<SessionReference>,
    confirming_delete: Option<SessionReference>,
    deleting: Option<SessionReference>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SessionPickerRow<'a> {
    pub(super) origin: &'a Outlook,
    pub(super) title: &'a str,
    /// The Icon Catalog name standing beside this row's Title, carried raw
    /// rather than resolved so drawing it stays gated in one place — the
    /// renderer, which is what already resolves every other Icon in the TUI.
    /// A Session whose derivation was skipped, failed, or abandoned has none.
    pub(super) icon: Option<&'a str>,
    pub(super) selected: bool,
    pub(super) current: bool,
    pub(super) active: bool,
    pub(super) pending_questionnaires: usize,
    pub(super) pending_approvals: usize,
    pub(super) unreadable: bool,
    pub(super) updated_at: SessionTimestamp,
    pub(super) workspace: Option<&'a Path>,
    pub(super) remote: Option<&'a str>,
    pub(super) confirming_delete: bool,
}

impl SessionPicker {
    pub(super) fn new(current_workspace: impl Into<crate::protocol::Workspace>) -> Self {
        Self {
            open: false,
            listing: SessionListing::new(SessionListSurface::SessionPicker, current_workspace),
            scope: SessionPickerScope::CurrentWorkspace,
            everywhere_origins: Vec::new(),
            everywhere_remote_sequence: 0,
            pending_everywhere_remotes: None,
            awaiting_dispatch: Vec::new(),
            query: String::new(),
            selected: None,
            attaching: None,
            confirming_delete: None,
            deleting: None,
        }
    }

    pub(super) fn open(&mut self) -> SessionPickerListing {
        self.open = true;
        self.query.clear();
        self.listing.clear_error();
        self.begin_listing()
    }

    /// Takes the Workspace this client has moved to, so the picker's own
    /// narrowing to "where I am" narrows to where the reader now is.
    pub(super) fn adopt_workspace(&mut self, workspace: impl Into<crate::protocol::Workspace>) {
        self.listing.adopt_current_workspace(workspace);
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.listing.adopt_outlook(outlook);
        self.close();
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        if self.scope != SessionPickerScope::Everywhere {
            self.listing.clear();
        }
        self.pending_everywhere_remotes = None;
        self.awaiting_dispatch.clear();
        self.forget_selection();
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        if self.scope == SessionPickerScope::Everywhere {
            self.pending_everywhere_remotes.is_some()
                || self.listing.is_loading_across(&self.everywhere_origins)
        } else {
            self.listing.is_loading()
        }
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn error(&self) -> Option<&str> {
        if self.scope == SessionPickerScope::Everywhere {
            self.listing
                .error_across(&self.everywhere_origins)
                .or_else(|| self.listing.error())
        } else {
            self.listing.error()
        }
    }

    pub(super) fn scope_label(&self) -> &'static str {
        self.scope.label()
    }

    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        sessions: Vec<SessionListItem>,
        current: Option<&SessionReference>,
    ) {
        let shown = self.scope == SessionPickerScope::Everywhere
            || request.outlook() == self.listing.outlook();
        if !self.listing.awaits(request) {
            return;
        }
        self.listing.load(request, sessions);
        if !shown {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
        self.selected = current
            .filter(|current| self.visible_references().contains(current))
            .cloned()
            .or_else(|| self.visible_references().first().cloned());
    }

    pub(super) fn set_standing_inputs(
        &mut self,
        origin: crate::protocol::Outlook,
        session_id: SessionId,
        inputs: crate::protocol::SessionStandingInputs,
    ) {
        self.listing
            .set_standing_inputs_origin(origin, session_id, inputs);
    }

    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, icon: Option<String>) {
        self.listing.retitle(session_id, title, icon);
    }

    pub(super) fn settle(&mut self, session_id: SessionId, settled_at: Option<SessionTimestamp>) {
        self.listing.settle(session_id, settled_at);
    }

    /// Whether a listing the server answered with would move anything the
    /// picker draws.
    pub(super) fn would_move(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        if self.scope == SessionPickerScope::Everywhere {
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

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        let shown = self.scope == SessionPickerScope::Everywhere
            || request.outlook() == self.listing.outlook();
        if !self.listing.awaits(request) {
            return;
        }
        self.listing.fail(request, error);
        if !shown {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
    }

    /// The client left the Session the picker was opening. The attach
    /// behind it has been let go of, so the picker stops waiting on an answer
    /// that is never coming.
    pub(super) fn abandon_attach(&mut self) {
        self.attaching = None;
    }

    pub(super) fn fail_attach(&mut self, error: String) -> SessionPickerListing {
        self.listing.report_error(error);
        self.begin_listing()
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.confirming_delete = None;
        self.query.push_str(text);
        self.select_first_visible();
    }

    pub(super) fn delete_backward(&mut self) {
        self.confirming_delete = None;
        self.query.pop();
        self.select_first_visible();
    }

    pub(super) fn toggle_scope(&mut self) -> SessionPickerListing {
        self.forget_selection();
        self.listing.clear_error();
        self.scope = self.scope.next();
        self.begin_listing()
    }

    pub(super) fn accepts_everywhere_remotes(&self, request: EverywhereListRequest) -> bool {
        request.surface() == SessionListSurface::SessionPicker
            && self.scope == SessionPickerScope::Everywhere
            && self.pending_everywhere_remotes == Some(request.id())
    }

    pub(super) fn load_everywhere_remotes(
        &mut self,
        request: EverywhereListRequest,
        remotes: Vec<Remote>,
    ) -> Option<Vec<SessionListRequest>> {
        if !self.accepts_everywhere_remotes(request) {
            return None;
        }
        self.pending_everywhere_remotes = None;
        let origins = everywhere_origins(remotes);
        self.listing.retain_origins(&origins);
        self.everywhere_origins = origins;
        Some(self.listing.refresh_origins(&self.everywhere_origins))
    }

    pub(super) fn fail_everywhere_remotes(
        &mut self,
        request: EverywhereListRequest,
        error: String,
    ) -> bool {
        if !self.accepts_everywhere_remotes(request) {
            return false;
        }
        self.pending_everywhere_remotes = None;
        self.listing.report_error(error);
        true
    }

    pub(super) fn catalog_origins(&self) -> HashSet<Outlook> {
        if self.scope != SessionPickerScope::Everywhere {
            return HashSet::new();
        }
        self.everywhere_origins
            .iter()
            .filter(|outlook| matches!(outlook, Outlook::Remote(_)))
            .cloned()
            .collect()
    }

    pub(super) fn is_everywhere(&self) -> bool {
        self.scope == SessionPickerScope::Everywhere
    }

    pub(super) fn includes_origin(&self, outlook: &Outlook) -> bool {
        self.scope == SessionPickerScope::Everywhere && self.everywhere_origins.contains(outlook)
    }

    /// Re-asks only the Origin whose catalog moved, keeping the rows already
    /// held until its replacement arrives. Everywhere is a chosen scope even
    /// while the overlay is hidden, so its listing stays current for the next
    /// time the reader shows it.
    pub(super) fn catch_up_origin(&mut self, outlook: Outlook) {
        let participates = if self.scope == SessionPickerScope::Everywhere {
            self.everywhere_origins.contains(&outlook)
        } else {
            outlook == *self.listing.outlook()
        };
        if participates && (self.open || self.scope == SessionPickerScope::Everywhere) {
            self.awaiting_dispatch = vec![self.listing.catch_up_origin(outlook)];
        }
    }

    /// Ends one Remote's participation and takes all of its rows away without
    /// allowing a delayed reply from that membership to validate if the same
    /// Remote name is paired again later.
    pub(super) fn end_origin(&mut self, outlook: &Outlook) {
        if !self.includes_origin(outlook) {
            return;
        }
        self.everywhere_origins.retain(|origin| origin != outlook);
        self.listing.retain_origins(&self.everywhere_origins);
        self.forget_absent();
    }

    pub(super) fn take_listing_requests(&mut self) -> Vec<SessionListRequest> {
        std::mem::take(&mut self.awaiting_dispatch)
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

    pub(super) fn begin_attach(&mut self) -> Option<SessionReference> {
        if self.is_loading() {
            return None;
        }
        self.confirming_delete = None;
        let selected = self.selected.clone()?;
        self.sessions()
            .into_iter()
            .find(|summary| summary.reference() == &selected)?
            .readable()?;
        self.attaching = Some(selected.clone());
        self.listing.clear_error();
        Some(selected)
    }

    pub(super) fn context_of(
        &self,
        reference: &SessionReference,
    ) -> Option<(crate::protocol::Workspace, PathBuf)> {
        self.sessions()
            .into_iter()
            .find(|session| session.reference() == reference)
            .and_then(|session| session.readable())
            .map(|summary| {
                (
                    summary.session.workspace.clone(),
                    summary.session.execution_directory.path.clone(),
                )
            })
    }

    pub(super) fn is_attaching(&self) -> bool {
        self.attaching.is_some()
    }

    pub(super) fn is_deleting(&self) -> bool {
        self.deleting.is_some()
    }

    pub(super) fn is_busy(&self) -> bool {
        self.is_attaching() || self.is_deleting()
    }

    pub(super) fn begin_deletion(&mut self) -> Option<SessionReference> {
        if self.is_loading() || self.deleting.is_some() {
            return None;
        }
        let selected = self.selected.clone()?;
        if self.confirming_delete.as_ref() != Some(&selected) {
            self.confirming_delete = Some(selected.clone());
            self.listing.clear_error();
            return None;
        }
        self.confirming_delete = None;
        self.deleting = Some(selected.clone());
        Some(selected)
    }

    pub(super) fn remove(&mut self, session_id: SessionId) {
        self.listing.remove(session_id);
        self.forget_absent();
    }

    pub(super) fn remove_origin(&mut self, outlook: Outlook, session_id: SessionId) {
        self.listing.remove_origin(outlook, session_id);
        self.forget_absent();
    }

    pub(super) fn retain_catalog(&mut self, session_ids: &[SessionId]) {
        self.listing.retain(session_ids);
        self.forget_absent();
    }

    pub(super) fn retain_origin_catalog(&mut self, outlook: Outlook, session_ids: &[SessionId]) {
        self.listing.retain_origin(outlook, session_ids);
        self.forget_absent();
    }

    pub(super) fn retitle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        title: String,
        icon: Option<String>,
    ) {
        self.listing
            .retitle_origin(outlook, session_id, title, icon);
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

    pub(super) fn settle_origin(
        &mut self,
        outlook: Outlook,
        session_id: SessionId,
        settled_at: Option<SessionTimestamp>,
    ) {
        self.listing.settle_origin(outlook, session_id, settled_at);
    }

    pub(super) fn fail_deletion(&mut self, reference: &SessionReference, error: String) {
        if self.deleting.as_ref() != Some(reference) {
            return;
        }
        self.deleting = None;
        self.listing.report_error(error);
    }

    pub(super) fn attaching_to(&self, reference: &SessionReference) -> bool {
        self.attaching.as_ref() == Some(reference)
    }

    fn rows(
        &self,
        current: Option<&SessionReference>,
    ) -> impl Iterator<Item = SessionPickerRow<'_>> {
        let wide = self.scope != SessionPickerScope::CurrentWorkspace;
        let everywhere = self.scope == SessionPickerScope::Everywhere;
        self.sessions()
            .into_iter()
            .filter(|summary| fuzzy_matches(&self.query, summary.title()))
            .map(move |summary| {
                let readable = summary.readable();
                SessionPickerRow {
                    origin: &summary.reference().origin,
                    pending_questionnaires: readable.map_or(0, |summary| {
                        summary.standing_inputs.pending_questionnaire_count()
                    }),
                    pending_approvals: readable.map_or(0, |summary| {
                        summary.standing_inputs.pending_approval_count()
                    }),
                    title: summary.title(),
                    icon: summary.icon(),
                    selected: self.selected.as_ref() == Some(summary.reference()),
                    current: readable.is_some() && current == Some(summary.reference()),
                    active: readable
                        .is_some_and(|summary| summary.session.status == SessionStatus::Active),
                    unreadable: readable.is_none(),
                    updated_at: summary.updated_at(),
                    workspace: wide
                        .then(|| {
                            summary
                                .workspace()
                                .map(|workspace| workspace.path.as_path())
                        })
                        .flatten(),
                    remote: everywhere
                        .then(|| summary.reference().origin.remote_name())
                        .flatten(),
                    confirming_delete: self.confirming_delete.as_ref() == Some(summary.reference()),
                }
            })
    }

    pub(super) fn visible_rows(
        &self,
        capacity: usize,
        current: Option<&SessionReference>,
    ) -> impl Iterator<Item = SessionPickerRow<'_>> {
        let rows = self.rows(current).collect::<Vec<_>>();
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        rows.into_iter().skip(start).take(capacity)
    }

    fn select_first_visible(&mut self) {
        self.selected = self.visible_references().first().cloned();
    }

    fn move_selection(&mut self, distance: isize) {
        self.confirming_delete = None;
        let visible = self.visible_references();
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| visible.iter().position(|reference| reference == selected))
            .unwrap_or(0);
        let len = visible.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = Some(visible[next].clone());
    }

    fn visible_references(&self) -> Vec<SessionReference> {
        self.sessions()
            .into_iter()
            .filter(|summary| fuzzy_matches(&self.query, summary.title()))
            .map(|summary| summary.reference().clone())
            .collect()
    }

    fn sessions(&self) -> Vec<&ListedSession> {
        let mut sessions = if self.scope == SessionPickerScope::Everywhere {
            self.listing.sessions_across(&self.everywhere_origins)
        } else {
            self.listing.sessions().iter().collect()
        };
        sessions.sort_by_key(|summary| Reverse(summary.updated_at()));
        sessions
    }

    /// Asks the listing again and puts the picker back where a fresh listing
    /// leaves it: nothing selected, nothing in flight.
    fn begin_listing(&mut self) -> SessionPickerListing {
        self.forget_selection();
        self.awaiting_dispatch.clear();
        match self.scope {
            SessionPickerScope::CurrentWorkspace => {
                let scope =
                    SessionListScope::CurrentWorkspace(self.listing.current_workspace().to_owned());
                SessionPickerListing::Origin(self.listing.refresh_in(scope))
            }
            SessionPickerScope::AllWorkspaces => SessionPickerListing::Origin(
                self.listing.refresh_in(SessionListScope::AllWorkspaces),
            ),
            SessionPickerScope::Everywhere => {
                self.listing.adopt_scope(SessionListScope::AllWorkspaces);
                self.everywhere_remote_sequence = self.everywhere_remote_sequence.wrapping_add(1);
                self.pending_everywhere_remotes = Some(self.everywhere_remote_sequence);
                SessionPickerListing::Everywhere(EverywhereListRequest::new(
                    SessionListSurface::SessionPicker,
                    self.everywhere_remote_sequence,
                ))
            }
        }
    }

    fn forget_selection(&mut self) {
        self.selected = None;
        self.attaching = None;
        self.confirming_delete = None;
        self.deleting = None;
    }

    /// Drops what the picker was pointing at once the Sessions behind it have
    /// left the listing, so no row is confirmed, attached, or deleted twice.
    fn forget_absent(&mut self) {
        for reference in [
            &mut self.confirming_delete,
            &mut self.attaching,
            &mut self.deleting,
        ] {
            if reference
                .as_ref()
                .is_some_and(|reference| !self.listing.contains(reference))
            {
                *reference = None;
            }
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !self.listing.contains(selected))
        {
            self.select_first_visible();
        }
    }
}
