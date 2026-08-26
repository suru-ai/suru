//! Session switcher state and title matching.

use std::path::Path;

use crate::protocol::{SessionId, SessionListItem, SessionStatus, SessionTimestamp};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface, session_listing::SessionListing,
};

#[derive(Clone, Debug)]
pub(super) struct SessionPicker {
    open: bool,
    /// The Sessions on offer, and the conversation with the server that keeps
    /// them true. The picker holds only what it does with them: the query it
    /// filters by and the row the reader is on.
    listing: SessionListing,
    query: String,
    selected: Option<SessionId>,
    attaching: Option<SessionId>,
    confirming_delete: Option<SessionId>,
    deleting: Option<SessionId>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SessionPickerRow<'a> {
    pub(super) title: &'a str,
    /// The Emoji standing for this Session, carried beside the Title rather
    /// than within it so the query never meets it. A Session whose derivation
    /// was skipped, failed, or abandoned has none, and its row is drawn as
    /// readily without one.
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
            listing: SessionListing::new(SessionListSurface::Picker, current_workspace),
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
        current: Option<SessionId>,
    ) {
        if !self.listing.load(request, sessions) {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
        self.selected = current
            .filter(|current| self.visible_ids().contains(current))
            .or_else(|| self.visible_ids().first().copied());
    }

    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        self.listing.retitle(session_id, title, emoji);
    }

    pub(super) fn settle(&mut self, session_id: SessionId, settled_at: Option<SessionTimestamp>) {
        self.listing.settle(session_id, settled_at);
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        if !self.listing.fail(request, error) {
            return;
        }
        self.attaching = None;
        self.confirming_delete = None;
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

    pub(super) fn begin_attachment(&mut self) -> Option<SessionId> {
        self.confirming_delete = None;
        let selected = self.selected?;
        self.listing
            .sessions()
            .iter()
            .find(|summary| summary.id() == selected)?
            .readable()?;
        self.attaching = Some(selected);
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

    pub(super) fn begin_deletion(&mut self) -> Option<SessionId> {
        if self.deleting.is_some() {
            return None;
        }
        let selected = self.selected?;
        if self.confirming_delete != Some(selected) {
            self.confirming_delete = Some(selected);
            self.listing.clear_error();
            return None;
        }
        self.confirming_delete = None;
        self.deleting = Some(selected);
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

    pub(super) fn fail_deletion(&mut self, session_id: SessionId, error: String) {
        if self.deleting != Some(session_id) {
            return;
        }
        self.deleting = None;
        self.listing.report_error(error);
    }

    pub(super) fn attaching_to(&self, session_id: SessionId) -> bool {
        self.attaching == Some(session_id)
    }

    fn rows(&self, current: Option<SessionId>) -> impl Iterator<Item = SessionPickerRow<'_>> {
        let all_workspaces = matches!(self.listing.scope(), SessionListScope::AllWorkspaces);
        self.listing
            .sessions()
            .iter()
            .filter(|summary| fuzzy_title_matches(&self.query, summary.title()))
            .map(move |summary| {
                let readable = summary.readable();
                SessionPickerRow {
                    title: summary.title(),
                    emoji: summary.emoji(),
                    selected: self.selected == Some(summary.id()),
                    current: readable.is_some() && current == Some(summary.id()),
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
                    confirming_delete: self.confirming_delete == Some(summary.id()),
                }
            })
    }

    pub(super) fn visible_rows(
        &self,
        capacity: usize,
        current: Option<SessionId>,
    ) -> impl Iterator<Item = SessionPickerRow<'_>> {
        let rows = self.rows(current).collect::<Vec<_>>();
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        rows.into_iter().skip(start).take(capacity)
    }

    fn select_first_visible(&mut self) {
        self.selected = self.visible_ids().first().copied();
    }

    fn move_selection(&mut self, distance: isize) {
        self.confirming_delete = None;
        let visible = self.visible_ids();
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| visible.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let len = visible.len() as isize;
        let next = (current as isize + distance).rem_euclid(len) as usize;
        self.selected = Some(visible[next]);
    }

    fn visible_ids(&self) -> Vec<SessionId> {
        self.listing
            .sessions()
            .iter()
            .filter(|summary| fuzzy_title_matches(&self.query, summary.title()))
            .map(SessionListItem::id)
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
        for session_id in [
            &mut self.confirming_delete,
            &mut self.attaching,
            &mut self.deleting,
        ] {
            if session_id.is_some_and(|session_id| !self.listing.contains(session_id)) {
                *session_id = None;
            }
        }
        if self
            .selected
            .is_some_and(|selected| !self.listing.contains(selected))
        {
            self.select_first_visible();
        }
    }
}

fn fuzzy_title_matches(query: &str, title: &str) -> bool {
    let mut title = title.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|character| title.by_ref().any(|candidate| candidate == character))
}
