//! Directory Browser state: the tree of the Outlook's Server's directories a
//! Client chooses a directory to work in from, read one directory at a time as
//! the reader opens it, and never from the Client's own disk.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use crate::protocol::{
    ChildDirectory, DirectoryListing, ListDirectoryRequest, ResolveWorkspaceRequest,
};

use super::list_window::{ListWindow, WindowEntry};

/// One listing request the Directory Browser sent the Outlook's Server, told
/// apart from every other so its answer reaches only the opening that asked
/// for it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryListingId(u64);

/// One listing the browser wants from the Outlook's Server: the identity its
/// answer will carry, and the directory it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DirectoryListingAsk {
    pub(super) listing_id: DirectoryListingId,
    pub(super) request: ListDirectoryRequest,
}

/// Where the browser was opened from, which is where Esc takes the reader.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum DirectoryBrowserProvenance {
    /// Opened by `/browse` or its chord, so Esc closes it outright.
    #[default]
    Standalone,
}

#[derive(Clone, Debug, Default)]
pub(super) struct DirectoryBrowser {
    open: bool,
    provenance: DirectoryBrowserProvenance,
    /// The text of the path field, which names the tree's root.
    field: String,
    /// The directory the tree stands on, its first row.
    root: PathBuf,
    /// The Landing's Execution Directory as the browser opened, which every
    /// listing it asks for is read from.
    base: PathBuf,
    /// What the Server has said of each directory the reader has opened, by
    /// the Server's own path for it, which every row standing for it shares:
    /// its children, that it is still being read, or why it cannot be.
    tree: HashMap<PathBuf, DirectoryEntries>,
    /// The rows whose children are drawn beneath them.
    opened: HashSet<RowKey>,
    /// The row focus stands on, held by where it stands rather than by row
    /// number, so a listing landing above it leaves the reader where they
    /// were.
    focused: RowKey,
    /// The directory each listing still awaited was asked for, so an answer
    /// to anything else — an earlier opening's request — moves nothing.
    awaiting: HashMap<DirectoryListingId, PathBuf>,
    /// Why the reader's choice was refused before it reached the Server,
    /// said inside the browser because the browser stands over the Landing
    /// where a refusal is otherwise said.
    refusal: Option<String>,
    sequence: u64,
    window: ListWindow,
}

/// Where a row stands in the tree: the directories from the root down to it,
/// each by the path the Server spelled it at. A directory's path alone does
/// not tell rows apart, because the Server reads a link where it leads and
/// spells what it lists beneath that, so two open branches linking to one
/// directory list it at one path beneath each.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(super) struct RowKey(Vec<PathBuf>);

impl RowKey {
    fn root(directory: PathBuf) -> Self {
        Self(vec![directory])
    }

    /// The row the Server's `directory` makes beneath this one.
    fn child(&self, directory: &Path) -> Self {
        let mut chain = self.0.clone();
        chain.push(directory.to_owned());
        Self(chain)
    }

    /// The directory the row stands for, by the Server's own path, which is
    /// what a listing of it asks for.
    fn directory(&self) -> &Path {
        self.0.last().map_or(Path::new(""), PathBuf::as_path)
    }

    fn is_root(&self) -> bool {
        self.0.len() == 1
    }
}

/// What the Server has said of one directory's children.
#[derive(Clone, Debug)]
enum DirectoryEntries {
    Loading,
    Listed(Vec<ChildDirectory>),
    Refused(String),
}

/// One row of the tree as a frame draws it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DirectoryBrowserRow {
    /// How deep beneath the root the row stands; the root is at 0.
    pub(super) depth: usize,
    pub(super) kind: DirectoryBrowserRowKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DirectoryBrowserRowKind {
    /// A directory the reader may stand on, open or closed.
    Directory {
        /// `None` for the root, which the frame names in the Server's own
        /// path syntax; every other row goes by the name the Server listed.
        name: Option<String>,
        key: RowKey,
        opened: bool,
        focused: bool,
    },
    /// Beneath an open directory still being read.
    Loading,
    /// Beneath an open directory the Server refused to read, saying why.
    Refused(String),
}

impl DirectoryBrowserRowKind {
    const fn is_directory(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

impl DirectoryBrowser {
    /// Opens the browser afresh, rooted at `execution_directory` with the
    /// root open and focused, and asks for the root's children. Nothing is
    /// kept from an earlier opening, including any answer it still awaits.
    pub(super) fn open(
        &mut self,
        execution_directory: PathBuf,
        provenance: DirectoryBrowserProvenance,
    ) -> DirectoryListingAsk {
        self.close();
        self.open = true;
        self.provenance = provenance;
        self.field = execution_directory.to_string_lossy().into_owned();
        self.root.clone_from(&execution_directory);
        self.base.clone_from(&execution_directory);
        self.focused = RowKey::root(execution_directory);
        self.window.open();
        self.open_row(self.focused.clone())
            .expect("a fresh tree has listed nothing yet")
    }

    /// Closes the browser and forgets its tree, answering where it was opened
    /// from, which is where the reader goes back to.
    pub(super) fn close(&mut self) -> DirectoryBrowserProvenance {
        self.open = false;
        self.tree.clear();
        self.opened.clear();
        self.awaiting.clear();
        self.refusal = None;
        std::mem::take(&mut self.provenance)
    }

    pub(super) const fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn field(&self) -> &str {
        &self.field
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// Why the Server would not read the root, which the frame says where the
    /// path field names it rather than beneath the root's row.
    pub(super) fn root_refusal(&self) -> Option<&str> {
        match self.tree.get(&self.root) {
            Some(DirectoryEntries::Refused(reason)) => Some(reason),
            _ => None,
        }
    }

    /// Why the reader's choice was refused before it reached the Server.
    pub(super) fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// Says `refusal` inside the browser until it closes.
    pub(super) fn refuse(&mut self, refusal: String) {
        self.refusal = Some(refusal);
    }

    pub(super) fn select_previous(&mut self) {
        self.move_focus(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_focus(1);
    }

    /// Opens the focused row where it is closed and closes it where it is
    /// open, answering the listing opening it asks for.
    pub(super) fn toggle_focused(&mut self) -> Option<DirectoryListingAsk> {
        if self.opened.remove(&self.focused) {
            return None;
        }
        self.open_row(self.focused.clone())
    }

    /// Opens the focused row, answering the listing that asks for.
    pub(super) fn open_focused(&mut self) -> Option<DirectoryListingAsk> {
        self.open_row(self.focused.clone())
    }

    /// Closes the focused row, unless it is the root: Left there is to stand
    /// the tree on the root's parent rather than to fold the tree away.
    pub(super) fn close_focused(&mut self) {
        if !self.focused.is_root() {
            self.opened.remove(&self.focused);
        }
    }

    /// Chooses the focused row, closing the browser since a choice is done
    /// with it, and answers the Workspace resolution that lands the reader in
    /// the row's directory: named by the Server's own path and read from the
    /// Landing's Execution Directory, with no Workspace or remembered
    /// directory beside it, because the directory chosen is itself where the
    /// next Session works.
    pub(super) fn choose_focused(&mut self) -> ResolveWorkspaceRequest {
        let path = self.focused.directory().to_owned();
        let base = self.base.clone();
        self.close();
        ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: Some(base),
            path,
        }
    }

    /// Takes the Server's answer to `listing_id`, where it is one this
    /// opening still awaits.
    pub(super) fn load(
        &mut self,
        listing_id: DirectoryListingId,
        result: Result<DirectoryListing, String>,
    ) {
        let Some(directory) = self.awaiting.remove(&listing_id) else {
            return;
        };
        let entries = match result {
            Ok(listing) => DirectoryEntries::Listed(listing.children),
            Err(reason) => DirectoryEntries::Refused(reason),
        };
        self.tree.insert(directory, entries);
    }

    /// The rows a tree `capacity` rows tall shows, wound on far enough to
    /// keep the focused row in view.
    pub(super) fn visible_rows(&self, capacity: usize) -> Vec<DirectoryBrowserRow> {
        let rows = self.rows();
        let entries = rows
            .iter()
            .map(|row| {
                if row.kind.is_directory() {
                    WindowEntry::ROW
                } else {
                    WindowEntry::passive(1)
                }
            })
            .collect::<Vec<_>>();
        let focus = rows.iter().position(|row| {
            matches!(
                &row.kind,
                DirectoryBrowserRowKind::Directory { focused: true, .. }
            )
        });
        let shown = self.window.settle(&entries, capacity, focus);
        rows.into_iter()
            .skip(shown.start)
            .take(shown.len())
            .collect()
    }

    /// Marks `row` open and asks for its directory's children, unless the
    /// Server has listed them already or is reading them still — for this
    /// row or another standing for the same directory. A directory it refused
    /// is asked for again, since opening it is asking again.
    fn open_row(&mut self, row: RowKey) -> Option<DirectoryListingAsk> {
        let directory = row.directory().to_owned();
        self.opened.insert(row);
        if matches!(
            self.tree.get(&directory),
            Some(DirectoryEntries::Listed(_) | DirectoryEntries::Loading)
        ) {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let listing_id = DirectoryListingId(self.sequence);
        self.tree
            .insert(directory.clone(), DirectoryEntries::Loading);
        self.awaiting.insert(listing_id, directory.clone());
        Some(DirectoryListingAsk {
            listing_id,
            request: ListDirectoryRequest {
                path: directory,
                base: Some(self.base.clone()),
            },
        })
    }

    /// The tree projected from what the Server has said: the root first, then
    /// every open row's children beneath it, depth first.
    ///
    /// Only an open row is descended into, and every row beneath one is a row
    /// of its own, so a link leading back up the tree is walked only as far as
    /// the reader keeps opening it.
    fn rows(&self) -> Vec<DirectoryBrowserRow> {
        let root = RowKey::root(self.root.clone());
        let mut rows = vec![self.directory_row(0, None, root.clone())];
        self.push_children(&root, 1, &mut rows);
        rows
    }

    fn push_children(&self, row: &RowKey, depth: usize, rows: &mut Vec<DirectoryBrowserRow>) {
        if !self.opened.contains(row) {
            return;
        }
        match self.tree.get(row.directory()) {
            None | Some(DirectoryEntries::Loading) => rows.push(DirectoryBrowserRow {
                depth,
                kind: DirectoryBrowserRowKind::Loading,
            }),
            // The root's refusal is said by the path field instead.
            Some(DirectoryEntries::Refused(_)) if row.is_root() => {}
            Some(DirectoryEntries::Refused(reason)) => rows.push(DirectoryBrowserRow {
                depth,
                kind: DirectoryBrowserRowKind::Refused(reason.clone()),
            }),
            Some(DirectoryEntries::Listed(children)) => {
                for child in children {
                    let child_row = row.child(&child.path);
                    rows.push(self.directory_row(depth, Some(&child.name), child_row.clone()));
                    self.push_children(&child_row, depth + 1, rows);
                }
            }
        }
    }

    fn directory_row(&self, depth: usize, name: Option<&str>, key: RowKey) -> DirectoryBrowserRow {
        DirectoryBrowserRow {
            depth,
            kind: DirectoryBrowserRowKind::Directory {
                name: name.map(str::to_owned),
                opened: self.opened.contains(&key),
                focused: self.focused == key,
                key,
            },
        }
    }

    /// Walks focus `distance` directories through the rows drawn, wrapping
    /// past either end; the lines beneath a directory still being read or
    /// refused are not rows to stand on.
    fn move_focus(&mut self, distance: isize) {
        let mut directories = self
            .rows()
            .into_iter()
            .filter_map(|row| match row.kind {
                DirectoryBrowserRowKind::Directory { key, .. } => Some(key),
                _ => None,
            })
            .collect::<Vec<_>>();
        let current = directories
            .iter()
            .position(|key| *key == self.focused)
            .unwrap_or(0);
        let length = directories.len() as isize;
        let next = (current as isize + distance).rem_euclid(length) as usize;
        self.focused = directories.swap_remove(next);
        self.window.reveal();
    }
}
