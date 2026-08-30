//! Workspace Picker state: the Workspaces a Session listing puts on offer,
//! ordered for choosing.

use std::path::PathBuf;

use crate::protocol::SessionListItem;

use super::{
    SessionListRequest, SessionListScope, SessionListSurface, session_listing::SessionListing,
    sidebar::workspace_name,
};

#[derive(Clone, Debug)]
pub(super) struct WorkspacePicker {
    open: bool,
    /// The Sessions the offered Workspaces are derived from, and the
    /// conversation with the server that keeps them true. It asks across every
    /// Workspace however narrow another surface's scope, because the
    /// Workspaces the reader is not in are the whole point of the picker.
    listing: SessionListing,
    /// The Workspace the reader is on, held as its path rather than as a row
    /// number so a listing landing beneath them leaves them on the Workspace
    /// they were choosing rather than on whatever now stands in its place.
    selected: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspacePickerRow {
    /// The Workspace by the name a row gives it, which is the directory a
    /// reader thinks of the work as being in.
    pub(super) name: String,
    /// Where it stands, spelled in full, so two Workspaces named alike stay
    /// told apart.
    pub(super) path: PathBuf,
    pub(super) selected: bool,
    /// Whether this is the Workspace the client is working in, which is the
    /// one the picker stands first.
    pub(super) current: bool,
}

impl WorkspacePicker {
    pub(super) fn new(current_workspace: PathBuf) -> Self {
        Self {
            open: false,
            listing: SessionListing::scoped(
                SessionListSurface::WorkspacePicker,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            selected: None,
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.selected = None;
        self.listing.clear_error();
        self.listing.refresh()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.selected = None;
        self.listing.clear();
    }

    pub(super) const fn is_open(&self) -> bool {
        self.open
    }

    pub(super) const fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    /// Takes the Workspace this client has moved to, so the picker marks as
    /// current — and stands first — where the reader now is.
    pub(super) fn adopt_workspace(&mut self, workspace: PathBuf) {
        self.listing.adopt_current_workspace(workspace);
    }

    /// Takes a listing the server answered with, leaving the reader on the
    /// Workspace they were on where it is still offered and on the first row
    /// — the current Workspace — otherwise.
    pub(super) fn load(&mut self, request: &SessionListRequest, sessions: Vec<SessionListItem>) {
        if !self.listing.load(request, sessions) {
            return;
        }
        self.keep_selection_offered();
    }

    pub(super) fn fail_listing(&mut self, request: &SessionListRequest, error: String) {
        self.listing.fail(request, error);
    }

    /// Whether a listing the server answered with would move anything the
    /// picker draws.
    ///
    /// It is answered off the Sessions rather than off the Workspaces derived
    /// from them, which can only over-report — and cannot here: the picker
    /// asks for one listing, when it opens, and is loading until that listing
    /// lands, so the one reply it ever awaits always has a loading line to
    /// replace.
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

    fn rows(&self) -> Vec<WorkspacePickerRow> {
        let current = self.listing.current_workspace().to_owned();
        self.offered()
            .into_iter()
            .map(|path| WorkspacePickerRow {
                name: workspace_name(&path),
                current: path == current,
                selected: self.selected.as_ref() == Some(&path),
                path,
            })
            .collect()
    }

    /// The rows a picker `capacity` rows tall shows, wound on far enough to
    /// keep the row the reader is on in view.
    pub(super) fn visible_rows(&self, capacity: usize) -> Vec<WorkspacePickerRow> {
        let rows = self.rows();
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        rows.into_iter().skip(start).take(capacity).collect()
    }

    /// The Workspaces on offer in the order the picker stands them: the one
    /// the client is working in first, because a reader has to see where they
    /// already are, then the rest by which held work most recently, because
    /// that is where the next pick is likeliest to go.
    fn offered(&self) -> Vec<PathBuf> {
        let current = self.listing.current_workspace().to_owned();
        let mut offered = self.listing.workspaces();
        // A stable sort on "is this not where I am", so the current Workspace
        // takes the first row and the rest keep the order the listing derived
        // them in, which is already newest work first.
        offered.sort_by_key(|path| *path != current);
        offered
    }

    fn move_selection(&mut self, distance: isize) {
        let offered = self.offered();
        if offered.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| offered.iter().position(|path| path == selected))
            .unwrap_or(0);
        let length = offered.len() as isize;
        let next = (current as isize + distance).rem_euclid(length) as usize;
        self.selected = Some(offered[next].clone());
    }

    /// Puts the reader on a row that is still offered: the one they were on
    /// where it stands, and the first row — the current Workspace — otherwise.
    fn keep_selection_offered(&mut self) {
        let offered = self.offered();
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| offered.contains(selected))
        {
            return;
        }
        self.selected = offered.first().cloned();
    }
}
