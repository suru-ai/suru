//! Session switcher state and title matching.

use std::{cmp::Reverse, path::Path};

use crate::protocol::{SessionId, SessionListItem, SessionStatus, SessionTimestamp};

use super::{SessionListRequest, SessionListScope};

#[derive(Clone, Debug)]
pub(super) struct SessionPicker {
    open: bool,
    current_workspace: std::path::PathBuf,
    scope: SessionListScope,
    request_sequence: u64,
    pending_request: Option<SessionListRequest>,
    query: String,
    sessions: Vec<SessionListItem>,
    selected: Option<SessionId>,
    loading: bool,
    error: Option<String>,
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
            scope: SessionListScope::CurrentWorkspace(current_workspace.clone()),
            current_workspace,
            request_sequence: 0,
            pending_request: None,
            query: String::new(),
            sessions: Vec::new(),
            selected: None,
            loading: false,
            error: None,
            attaching: None,
            confirming_delete: None,
            deleting: None,
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.query.clear();
        self.error = None;
        self.begin_listing()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.sessions.clear();
        self.selected = None;
        self.loading = false;
        self.error = None;
        self.attaching = None;
        self.confirming_delete = None;
        self.deleting = None;
        self.pending_request = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.loading
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn scope(&self) -> &SessionListScope {
        &self.scope
    }

    pub(super) fn load(
        &mut self,
        request: &SessionListRequest,
        mut sessions: Vec<SessionListItem>,
        current: Option<SessionId>,
    ) {
        if !self.accepts(request) {
            return;
        }
        sessions.sort_unstable_by_key(|summary| Reverse(summary.updated_at()));
        self.sessions = sessions;
        self.loading = false;
        self.attaching = None;
        self.confirming_delete = None;
        self.pending_request = None;
        self.selected = current
            .filter(|current| self.visible_ids().contains(current))
            .or_else(|| self.visible_ids().first().copied());
    }

    /// Takes a Session's newly derived Title and Emoji into a listing already
    /// drawn, so a Title landing while the picker is open moves the row it is
    /// on rather than waiting for the reader to reopen the picker.
    pub(super) fn retitle(&mut self, session_id: SessionId, title: String, emoji: Option<String>) {
        for session in &mut self.sessions {
            if let SessionListItem::Readable(summary) = session
                && summary.session.id == session_id
            {
                summary.title = title;
                summary.emoji = emoji;
                return;
            }
        }
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        if !self.accepts(request) {
            return;
        }
        self.loading = false;
        self.error = Some(error);
        self.attaching = None;
        self.confirming_delete = None;
        self.pending_request = None;
    }

    pub(super) fn fail_attachment(&mut self, error: String) -> SessionListRequest {
        self.error = Some(error);
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
        self.confirming_delete = None;
        self.scope = match &self.scope {
            SessionListScope::CurrentWorkspace(_) => SessionListScope::AllWorkspaces,
            SessionListScope::AllWorkspaces => {
                SessionListScope::CurrentWorkspace(self.current_workspace.clone())
            }
        };
        self.error = None;
        self.begin_listing()
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
        self.sessions
            .iter()
            .find(|summary| summary.id() == selected)?
            .readable()?;
        self.attaching = Some(selected);
        self.error = None;
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
            self.error = None;
            return None;
        }
        self.confirming_delete = None;
        self.deleting = Some(selected);
        Some(selected)
    }

    pub(super) fn remove(&mut self, session_id: SessionId) {
        let selected = self.selected == Some(session_id);
        self.sessions.retain(|summary| summary.id() != session_id);
        if self.confirming_delete == Some(session_id) {
            self.confirming_delete = None;
        }
        if self.attaching == Some(session_id) {
            self.attaching = None;
        }
        if self.deleting == Some(session_id) {
            self.deleting = None;
        }
        if selected {
            self.select_first_visible();
        }
    }

    pub(super) fn retain_catalog(&mut self, session_ids: &[SessionId]) {
        self.sessions
            .retain(|summary| session_ids.contains(&summary.id()));
        if self
            .selected
            .is_some_and(|selected| !session_ids.contains(&selected))
        {
            self.select_first_visible();
        }
        if self
            .confirming_delete
            .is_some_and(|session_id| !session_ids.contains(&session_id))
        {
            self.confirming_delete = None;
        }
        if self
            .attaching
            .is_some_and(|session_id| !session_ids.contains(&session_id))
        {
            self.attaching = None;
        }
        if self
            .deleting
            .is_some_and(|session_id| !session_ids.contains(&session_id))
        {
            self.deleting = None;
        }
    }

    pub(super) fn fail_deletion(&mut self, session_id: SessionId, error: String) {
        if self.deleting != Some(session_id) {
            return;
        }
        self.deleting = None;
        self.error = Some(error);
    }

    pub(super) fn attaching_to(&self, session_id: SessionId) -> bool {
        self.attaching == Some(session_id)
    }

    fn rows(&self, current: Option<SessionId>) -> impl Iterator<Item = SessionPickerRow<'_>> {
        self.sessions
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
                    workspace: matches!(self.scope, SessionListScope::AllWorkspaces)
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
        self.sessions
            .iter()
            .filter(|summary| fuzzy_title_matches(&self.query, summary.title()))
            .map(SessionListItem::id)
            .collect()
    }

    fn begin_listing(&mut self) -> SessionListRequest {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let request = SessionListRequest::new(self.request_sequence, self.scope.clone());
        self.pending_request = Some(request.clone());
        self.sessions.clear();
        self.selected = None;
        self.loading = true;
        self.attaching = None;
        self.confirming_delete = None;
        self.deleting = None;
        request
    }

    fn accepts(&self, request: &SessionListRequest) -> bool {
        self.open && self.pending_request.as_ref() == Some(request)
    }
}

fn fuzzy_title_matches(query: &str, title: &str) -> bool {
    let mut title = title.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|character| title.by_ref().any(|candidate| candidate == character))
}
