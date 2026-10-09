//! Directory Browser state: the tree of the Outlook's Server's directories a
//! Client chooses a directory to work in from, read one directory at a time as
//! the reader opens it, and never from the Client's own disk.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use crate::protocol::{ChildDirectory, DirectoryListing, ListDirectoryRequest};

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
    /// the path its row stands for: its children, that it is still being
    /// read, or why it cannot be.
    tree: HashMap<PathBuf, DirectoryEntries>,
    /// The directories whose children are drawn beneath their rows.
    opened: HashSet<PathBuf>,
    /// The directory row focus stands on, held by path rather than by row so
    /// a listing landing above it leaves the reader where they were.
    focused: PathBuf,
    /// The directory each listing still awaited was asked for, so an answer
    /// to anything else — an earlier opening's request — moves nothing.
    awaiting: HashMap<DirectoryListingId, PathBuf>,
    sequence: u64,
    window: ListWindow,
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
        path: PathBuf,
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
        self.focused.clone_from(&execution_directory);
        self.window.open();
        self.open_directory(execution_directory)
            .expect("a fresh tree has listed nothing yet")
    }

    /// Closes the browser and forgets its tree, answering where it was opened
    /// from, which is where the reader goes back to.
    pub(super) fn close(&mut self) -> DirectoryBrowserProvenance {
        self.open = false;
        self.tree.clear();
        self.opened.clear();
        self.awaiting.clear();
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

    pub(super) fn select_previous(&mut self) {
        self.move_focus(-1);
    }

    pub(super) fn select_next(&mut self) {
        self.move_focus(1);
    }

    /// Opens the focused row where it is closed and closes it where it is
    /// open, answering the listing opening it asks for.
    pub(super) fn toggle_focused(&mut self) -> Option<DirectoryListingAsk> {
        if self.opened.contains(&self.focused) {
            self.opened.remove(&self.focused);
            return None;
        }
        self.open_directory(self.focused.clone())
    }

    /// Opens the focused row, answering the listing that asks for.
    pub(super) fn open_focused(&mut self) -> Option<DirectoryListingAsk> {
        self.open_directory(self.focused.clone())
    }

    /// Closes the focused row, unless it is the root: Left there is to stand
    /// the tree on the root's parent rather than to fold the tree away.
    pub(super) fn close_focused(&mut self) {
        if self.focused != self.root {
            self.opened.remove(&self.focused);
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

    /// Marks `directory` open and asks for its children, unless the Server
    /// has listed them already or is reading them still. A directory it
    /// refused is asked for again, since opening it is asking again.
    fn open_directory(&mut self, directory: PathBuf) -> Option<DirectoryListingAsk> {
        self.opened.insert(directory.clone());
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
    /// every open directory's children beneath it, depth first.
    fn rows(&self) -> Vec<DirectoryBrowserRow> {
        let mut rows = vec![self.directory_row(0, None, &self.root)];
        let mut ancestry = vec![self.root.as_path()];
        self.push_children(&self.root, 1, &mut ancestry, &mut rows);
        rows
    }

    fn push_children<'a>(
        &'a self,
        directory: &Path,
        depth: usize,
        ancestry: &mut Vec<&'a Path>,
        rows: &mut Vec<DirectoryBrowserRow>,
    ) {
        if !self.opened.contains(directory) {
            return;
        }
        match self.tree.get(directory) {
            None | Some(DirectoryEntries::Loading) => rows.push(DirectoryBrowserRow {
                depth,
                kind: DirectoryBrowserRowKind::Loading,
            }),
            // The root's refusal is said by the path field instead.
            Some(DirectoryEntries::Refused(_)) if directory == self.root => {}
            Some(DirectoryEntries::Refused(reason)) => rows.push(DirectoryBrowserRow {
                depth,
                kind: DirectoryBrowserRowKind::Refused(reason.clone()),
            }),
            Some(DirectoryEntries::Listed(children)) => {
                for child in children {
                    rows.push(self.directory_row(depth, Some(&child.name), &child.path));
                    // A Server naming a directory among its own ancestors
                    // would otherwise draw it beneath itself without end.
                    if ancestry.contains(&child.path.as_path()) {
                        continue;
                    }
                    ancestry.push(&child.path);
                    self.push_children(&child.path, depth + 1, ancestry, rows);
                    ancestry.pop();
                }
            }
        }
    }

    fn directory_row(&self, depth: usize, name: Option<&str>, path: &Path) -> DirectoryBrowserRow {
        DirectoryBrowserRow {
            depth,
            kind: DirectoryBrowserRowKind::Directory {
                name: name.map(str::to_owned),
                path: path.to_owned(),
                opened: self.opened.contains(path),
                focused: self.focused == path,
            },
        }
    }

    /// Walks focus `distance` directories through the rows drawn, wrapping
    /// past either end; the lines beneath a directory still being read or
    /// refused are not rows to stand on.
    fn move_focus(&mut self, distance: isize) {
        let directories = self
            .rows()
            .into_iter()
            .filter_map(|row| match row.kind {
                DirectoryBrowserRowKind::Directory { path, .. } => Some(path),
                _ => None,
            })
            .collect::<Vec<_>>();
        let current = directories
            .iter()
            .position(|path| *path == self.focused)
            .unwrap_or(0);
        let length = directories.len() as isize;
        let next = (current as isize + distance).rem_euclid(length) as usize;
        self.focused.clone_from(&directories[next]);
        self.window.reveal();
    }
}
