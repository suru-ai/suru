//! Session switcher state and the query that narrows it.

use std::path::Path;

use crate::protocol::{
    EffectiveSettings, EmojiVisibility, Outlook, SessionId, SessionListItem, SessionReference,
    SessionStatus, SessionTimestamp,
};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface, fuzzy::fuzzy_matches,
    session_listing::SessionListing,
};

#[derive(Clone, Debug)]
pub(super) struct SessionPicker {
    open: bool,
    /// The Sessions on offer, and the conversation with the server that keeps
    /// them true. The picker holds only what it does with them: the query it
    /// filters by and the row the reader is on.
    listing: SessionListing,
    /// Whether a row draws the Emoji derived beside its Session's Title, which
    /// governs every frame from the moment the Setting lands.
    emoji: EmojiVisibility,
    query: String,
    selected: Option<SessionReference>,
    attaching: Option<SessionReference>,
    confirming_delete: Option<SessionReference>,
    deleting: Option<SessionReference>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SessionPickerRow<'a> {
    pub(super) title: &'a str,
    /// The Emoji this row draws for its Session, carried beside the Title
    /// rather than within it so the query never meets it. A Session whose
    /// derivation was skipped, failed, or abandoned has none, and so has every
    /// Session while the reader keeps Emojis hidden; a row is drawn as readily
    /// without one either way.
    pub(super) emoji: Option<&'a str>,
    pub(super) selected: bool,
    pub(super) current: bool,
    pub(super) active: bool,
    pub(super) unreadable: bool,
    pub(super) updated_at: SessionTimestamp,
    pub(super) workspace: Option<&'a Path>,
    pub(super) confirming_delete: bool,
}

impl SessionPicker {
    pub(super) fn new(current_workspace: std::path::PathBuf) -> Self {
        Self {
            open: false,
            listing: SessionListing::new(SessionListSurface::SessionPicker, current_workspace),
            emoji: EmojiVisibility::default(),
            query: String::new(),
            selected: None,
            attaching: None,
            confirming_delete: None,
            deleting: None,
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.query.clear();
        self.listing.clear_error();
        self.begin_listing()
    }

    /// Takes the Settings the picker draws under, which is how a Session is
    /// named: everything else about a row is the listing's own.
    pub(super) fn adopt_settings(&mut self, settings: &EffectiveSettings) {
        self.emoji = settings.session.title.emoji;
    }

    /// Takes the Workspace this client has moved to, so the picker's own
    /// narrowing to "where I am" narrows to where the reader now is.
    pub(super) fn adopt_workspace(&mut self, workspace: std::path::PathBuf) {
        self.listing.adopt_current_workspace(workspace);
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.listing.adopt_outlook(outlook);
        self.close();
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.listing.clear();
        self.forget_selection();
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    pub(super) fn scope(&self) -> &SessionListScope {
        self.listing.scope()
    }

    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        sessions: Vec<SessionListItem>,
        current: Option<&SessionReference>,
    ) {
        if !self.listing.load(request, sessions) {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
        self.selected = current
            .filter(|current| self.visible_references().contains(current))
            .cloned()
            .or_else(|| self.visible_references().first().cloned());
    }

    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        self.listing.retitle(session_id, title, emoji);
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
        self.listing.would_move(request, sessions)
    }

    /// Whether a reply the server sent answers the listing this surface is
    /// still waiting for.
    pub(super) fn awaits_listing(&self, request: &SessionListRequest) -> bool {
        self.listing.awaits(request)
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        if !self.listing.fail(request, error) {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
    }

    /// The client left the Session the picker was opening. The attachment
    /// behind it has been let go of, so the picker stops waiting on an answer
    /// that is never coming.
    pub(super) fn abandon_attachment(&mut self) {
        self.attaching = None;
    }

    pub(super) fn fail_attachment(&mut self, error: String) -> SessionListRequest {
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

    pub(super) fn toggle_scope(&mut self) -> SessionListRequest {
        self.forget_selection();
        self.listing.toggle_scope()
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

    pub(super) fn begin_attachment(&mut self) -> Option<SessionReference> {
        self.confirming_delete = None;
        let selected = self.selected.clone()?;
        self.listing
            .sessions()
            .iter()
            .find(|summary| summary.reference() == &selected)?
            .readable()?;
        self.attaching = Some(selected.clone());
        self.listing.clear_error();
        Some(selected)
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
        if self.deleting.is_some() {
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

    pub(super) fn retain_catalog(&mut self, session_ids: &[SessionId]) {
        self.listing.retain(session_ids);
        self.forget_absent();
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
        let all_workspaces = matches!(self.listing.scope(), SessionListScope::AllWorkspaces);
        self.listing
            .sessions()
            .iter()
            .filter(|summary| fuzzy_matches(&self.query, summary.title()))
            .map(move |summary| {
                let readable = summary.readable();
                SessionPickerRow {
                    title: summary.title(),
                    emoji: self.emoji.drawn_emoji(summary.emoji()),
                    selected: self.selected.as_ref() == Some(summary.reference()),
                    current: readable.is_some() && current == Some(summary.reference()),
                    active: readable
                        .is_some_and(|summary| summary.session.status == SessionStatus::Active),
                    unreadable: readable.is_none(),
                    updated_at: summary.updated_at(),
                    workspace: all_workspaces
                        .then(|| {
                            summary
                                .workspace()
                                .map(|workspace| workspace.path.as_path())
                        })
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
        self.listing
            .sessions()
            .iter()
            .filter(|summary| fuzzy_matches(&self.query, summary.title()))
            .map(|summary| summary.reference().clone())
            .collect()
    }

    /// Asks the listing again and puts the picker back where a fresh listing
    /// leaves it: nothing selected, nothing in flight.
    fn begin_listing(&mut self) -> SessionListRequest {
        self.forget_selection();
        self.listing.refresh()
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
