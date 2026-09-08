//! Workspace Picker state: the Workspaces a Session listing puts on offer,
//! ordered for choosing and narrowed by what the reader types.

use std::path::{Path, PathBuf};

use crate::protocol::{Outlook, SessionListItem, WorkspacePaths};

use super::{
    SessionListRequest, SessionListScope, SessionListSurface, fuzzy::fuzzy_matches,
    session_listing::SessionListing, sidebar::workspace_name,
};

#[derive(Clone, Debug)]
pub(super) struct WorkspacePicker {
    paths: Option<WorkspacePaths>,
    open: bool,
    /// The Sessions the offered Workspaces are derived from, and the
    /// conversation with the server that keeps them true. It asks across every
    /// Workspace however narrow another surface's scope, because the
    /// Workspaces the reader is not in are the whole point of the picker.
    listing: SessionListing,
    /// What the reader has typed to narrow the Workspaces on offer, read
    /// against each one's name the way the session picker reads its own query
    /// against a Title.
    query: String,
    /// The Workspace the reader is on, held as its path rather than as a row
    /// number so a listing landing beneath them leaves them on the Workspace
    /// they were choosing rather than on whatever now stands in its place.
    selected: Option<PathBuf>,
    /// Why the selected Workspace could not be read when the reader chose it.
    /// It belongs to the picker rather than to the listing: the row is still
    /// true of past work even when its directory has since disappeared.
    refusal: Option<String>,
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
            paths: None,
            open: false,
            listing: SessionListing::scoped(
                SessionListSurface::WorkspacePicker,
                current_workspace,
                SessionListScope::AllWorkspaces,
            ),
            query: String::new(),
            selected: None,
            refusal: None,
        }
    }

    pub(super) fn open(&mut self) -> SessionListRequest {
        self.open = true;
        self.query.clear();
        self.selected = None;
        self.refusal = None;
        self.listing.clear_error();
        self.listing.refresh()
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = None;
        self.refusal = None;
        self.listing.clear();
    }

    pub(super) const fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn is_loading(&self) -> bool {
        self.listing.is_loading()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.listing.error()
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// Takes what the reader typed into the query, and leaves them on a
    /// Workspace the narrowed list still offers.
    ///
    /// Where the session picker sends the reader back to its first row at
    /// every keystroke, a Workspace they are already on survives their typing:
    /// a row here is a place rather than a Session, and a reader narrowing
    /// towards one they can already see should not be walked away from it.
    pub(super) fn insert(&mut self, text: &str) {
        self.query.push_str(text);
        self.refusal = None;
        self.keep_selection_offered();
    }

    /// Gives the last character of the query back, widening the list again.
    pub(super) fn delete_backward(&mut self) {
        self.query.pop();
        self.refusal = None;
        self.keep_selection_offered();
    }

    /// Takes the Workspace this client has moved to, so the picker marks as
    /// current — and stands first — where the reader now is.
    pub(super) fn adopt_workspace(&mut self, workspace: PathBuf) {
        self.listing.adopt_current_workspace(workspace);
    }

    pub(super) fn adopt_workspace_paths(&mut self, paths: WorkspacePaths) {
        self.paths = Some(paths);
        self.keep_selection_offered();
    }

    fn name(&self, path: &Path) -> String {
        self.paths
            .as_ref()
            .map_or_else(|| workspace_name(path), |paths| paths.name(path))
    }

    pub(super) fn adopt_outlook(&mut self, outlook: Outlook) {
        self.paths = None;
        self.listing.adopt_outlook(outlook);
        self.close();
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

    /// The Workspace the row the reader is on names, which is the one choosing
    /// takes. There is none while the listing is on its way: no row is marked,
    /// so Enter names nothing rather than naming whatever would stand first.
    pub(super) fn offer_selected(&mut self) -> Option<PathBuf> {
        self.selected.clone()
    }

    pub(super) fn fail_resolution(&mut self, error: String) {
        self.refusal = Some(error);
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
                name: self.name(&path),
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
    ///
    /// A query takes rows away and never rearranges the ones it leaves, so a
    /// reader narrowing the list goes on reading it in the order they learned
    /// it in.
    fn offered(&self) -> Vec<PathBuf> {
        let current = self.listing.current_workspace().to_owned();
        let mut offered = self
            .listing
            .workspaces()
            .into_iter()
            .filter(|path| fuzzy_matches(&self.query, &self.name(path)))
            .collect::<Vec<_>>();
        // A stable sort on "is this not where I am", so the current Workspace
        // takes the first row and the rest keep the order the listing derived
        // them in, which is already newest work first.
        offered.sort_by_key(|path| *path != current);
        offered
    }

    fn move_selection(&mut self, distance: isize) {
        self.refusal = None;
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
    /// where it stands, and the first row otherwise — which is the current
    /// Workspace until a query takes it away, and no row at all when a query
    /// leaves none.
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
