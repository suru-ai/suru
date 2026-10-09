//! Directory Browser state: the tree of the Outlook's Server's directories a
//! Client chooses a directory to work in from, read one directory at a time as
//! the reader opens it, and never from the Client's own disk.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use crate::protocol::{
    DirectoryListing, DirectorySourceControl, ListDirectoryRequest, PathStyle,
    ResolveWorkspaceRequest,
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
    /// The text of the path field, in the Server's own syntax, which names the
    /// tree's root by its leading part and narrows the root's children by its
    /// tail. What the reader typed stays as they typed it.
    field: String,
    /// The path field's leading part as the field last read.
    leading: String,
    /// The directory each leading part this opening has read names, by the
    /// Server's own path for it: a leading part not here is asked of the
    /// Server before the tree stands on it.
    roots: HashMap<String, PathBuf>,
    /// The partial name the root's children are narrowed to: the field's tail
    /// while its leading part names the root, and what it last was while the
    /// Server has yet to say, or would not say, where a newer leading part is.
    filter: String,
    /// Why the Server would not read the field's leading part, which leaves
    /// the tree on the last root it could read.
    field_refusal: Option<String>,
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
    /// What each listing still awaited was asked for, so an answer to
    /// anything else — an earlier opening's request, or a leading part the
    /// reader has since typed over — moves nothing.
    awaiting: HashMap<DirectoryListingId, Asked>,
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
    pub(super) fn directory(&self) -> &Path {
        self.0.last().map_or(Path::new(""), PathBuf::as_path)
    }

    fn is_root(&self) -> bool {
        self.0.len() == 1
    }
}

/// What one listing the browser awaits was asked for.
#[derive(Clone, Debug)]
enum Asked {
    /// A directory by the Server's own path, which its answer is kept under.
    Directory(PathBuf),
    /// The directory the path field's leading part names, as the reader typed
    /// it, which only the Server's answer says the path of.
    Root(String),
}

/// What the Server has said of one directory's children.
#[derive(Clone, Debug)]
enum DirectoryEntries {
    Loading,
    /// The directory as the Server read it: its children, and what it is
    /// itself to source control, which the root's row draws.
    Listed(DirectoryListing),
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
        /// What the Server read the directory to be: as it listed it beneath
        /// its parent, or for the root as it listed the root itself, which
        /// is `None` until that listing arrives.
        source_control: Option<DirectorySourceControl>,
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
    /// root open and focused, and asks for the root's children. The path
    /// field names the root, ready for a name beneath it. Nothing is kept
    /// from an earlier opening, including any answer it still awaits.
    pub(super) fn open(
        &mut self,
        execution_directory: PathBuf,
        provenance: DirectoryBrowserProvenance,
    ) -> DirectoryListingAsk {
        self.close();
        self.open = true;
        self.provenance = provenance;
        self.base.clone_from(&execution_directory);
        self.field = self.beneath(&execution_directory.to_string_lossy());
        self.leading = self.split_field().0.to_owned();
        // An empty leading part is the relative path naming the directory
        // every listing is read from.
        for leading in [String::new(), self.leading.clone()] {
            self.roots.insert(leading, execution_directory.clone());
        }
        self.root.clone_from(&execution_directory);
        self.focused = RowKey::root(execution_directory);
        self.window.open();
        self.open_row(self.focused.clone())
            .expect("a fresh tree has listed nothing yet")
    }

    /// Closes the browser and forgets its tree, answering where it was opened
    /// from, which is where the reader goes back to.
    pub(super) fn close(&mut self) -> DirectoryBrowserProvenance {
        self.open = false;
        self.roots.clear();
        self.filter.clear();
        self.field_refusal = None;
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

    /// Why the Server would not read the directory the path field's leading
    /// part names, or the root itself, which the frame says where the path
    /// field names it rather than beneath the root's row.
    pub(super) fn root_refusal(&self) -> Option<&str> {
        self.field_refusal
            .as_deref()
            .or_else(|| match self.tree.get(&self.root) {
                Some(DirectoryEntries::Refused(reason)) => Some(reason),
                _ => None,
            })
    }

    /// Whether the Server is still reading the directory the path field's
    /// leading part names, which the tree is not yet standing on.
    pub(super) fn root_is_loading(&self) -> bool {
        self.awaiting
            .values()
            .any(|asked| matches!(asked, Asked::Root(_)))
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

    /// Closes the focused row, unless it is the root: Left there stands the
    /// tree on the root's parent, as the Server named it, rather than folding
    /// the tree away, leaving the former root open and focused beneath it,
    /// and answers the listing the parent asks for. A root without a parent
    /// stands where it is.
    pub(super) fn close_focused(&mut self) -> Option<DirectoryListingAsk> {
        if !self.focused.is_root() {
            self.opened.remove(&self.focused);
            return None;
        }
        let Some(DirectoryEntries::Listed(DirectoryListing {
            parent: Some(parent),
            ..
        })) = self.tree.get(&self.root)
        else {
            return None;
        };
        let parent = parent.clone();
        let former = std::mem::replace(&mut self.root, parent.clone());
        self.field = self.beneath(&parent.to_string_lossy());
        self.leading_changed();
        self.roots.insert(self.leading.clone(), parent.clone());
        self.filter.clear();
        self.opened = std::mem::take(&mut self.opened)
            .into_iter()
            .filter(|row| row.0.first() == Some(&former))
            .map(|row| RowKey([vec![parent.clone()], row.0].concat()))
            .collect();
        self.focused = RowKey(vec![parent.clone(), former]);
        self.window.reveal();
        self.open_row(RowKey::root(parent))
    }

    /// Adds what the reader typed or pasted to the end of the path field,
    /// leaving out what no path holds — a line break carried in by a paste —
    /// and answers the listing the root it now names asks for.
    pub(super) fn type_into_field(&mut self, text: &str) -> Option<DirectoryListingAsk> {
        let typed = text
            .chars()
            .filter(|character| !character.is_control())
            .collect::<String>();
        if typed.is_empty() {
            return None;
        }
        self.field.push_str(&typed);
        self.follow_field()
    }

    /// Takes the path field's last character back, answering the listing the
    /// root it now names asks for.
    pub(super) fn delete_from_field(&mut self) -> Option<DirectoryListingAsk> {
        self.field.pop()?;
        self.follow_field()
    }

    /// Completes the path field to the focused row and a separator, so the
    /// tree stands on that row, answering the listing it asks for. The field
    /// keeps the leading part the reader typed where that names the root, and
    /// otherwise begins at the Server's own path for the root.
    pub(super) fn complete_field(&mut self) -> Option<DirectoryListingAsk> {
        let names = self.names_beneath_root(&self.focused)?;
        let mut field = if self.roots.get(&self.leading) == Some(&self.root) {
            self.leading.clone()
        } else {
            self.root.to_string_lossy().into_owned()
        };
        for name in names {
            field = self.beneath(&field);
            field.push_str(&name);
        }
        if !field.is_empty() {
            field = self.beneath(&field);
        }
        if field == self.field {
            return None;
        }
        self.field = field;
        let directory = self.focused.directory().to_owned();
        let leading = self.split_field().0.to_owned();
        self.roots.insert(leading, directory);
        self.follow_field()
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
    /// opening still awaits. An answer for the field's leading part stands
    /// the tree on the root the Server read it as; a refusal of it leaves the
    /// tree on the last root it could read, with every child shown, since the
    /// tail typed after the refused part says nothing of that root.
    pub(super) fn load(
        &mut self,
        listing_id: DirectoryListingId,
        result: Result<DirectoryListing, String>,
    ) {
        let Some(asked) = self.awaiting.remove(&listing_id) else {
            return;
        };
        match (asked, result) {
            (Asked::Directory(directory), result) => {
                let lands_on_root = directory == self.root;
                let entries = match result {
                    Ok(listing) => DirectoryEntries::Listed(listing),
                    Err(reason) => DirectoryEntries::Refused(reason),
                };
                self.tree.insert(directory, entries);
                if lands_on_root {
                    self.settle_focus();
                }
            }
            (Asked::Root(leading), Ok(listing)) => {
                let root = listing.root.clone();
                self.tree
                    .insert(root.clone(), DirectoryEntries::Listed(listing));
                self.roots.insert(leading, root);
                let listed = self.follow_field();
                debug_assert!(listed.is_none(), "the root was listed just now");
            }
            (Asked::Root(_), Err(reason)) => {
                self.field_refusal = Some(reason);
                self.filter.clear();
                self.settle_focus();
            }
        }
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
        self.tree
            .insert(directory.clone(), DirectoryEntries::Loading);
        Some(self.ask(Asked::Directory(directory)))
    }

    /// The listing `asked` names, awaited under an identity of its own and
    /// read from the Execution Directory.
    fn ask(&mut self, asked: Asked) -> DirectoryListingAsk {
        self.sequence = self.sequence.wrapping_add(1);
        let listing_id = DirectoryListingId(self.sequence);
        let path = match &asked {
            Asked::Directory(directory) => directory.clone(),
            Asked::Root(leading) => PathBuf::from(leading),
        };
        self.awaiting.insert(listing_id, asked);
        DirectoryListingAsk {
            listing_id,
            request: ListDirectoryRequest {
                path,
                base: Some(self.base.clone()),
            },
        }
    }

    /// Takes the path field as it now reads. Where its leading part names a
    /// directory this opening knows, the tree stands there at once, asking
    /// for its children if they are not had, and the tail narrows them; a
    /// leading part not read yet is asked of the Server, the tree standing
    /// where it was until the answer. Only a change of leading part asks.
    fn follow_field(&mut self) -> Option<DirectoryListingAsk> {
        let tail = self.split_field().1.to_owned();
        let changed = self.split_field().0 != self.leading;
        if changed {
            self.leading_changed();
        }
        match self.roots.get(&self.leading).cloned() {
            Some(root) => {
                let listed = (root != self.root).then(|| self.reroot(root)).flatten();
                self.filter = tail;
                self.settle_focus();
                listed
            }
            None if changed => Some(self.ask(Asked::Root(self.leading.clone()))),
            None => None,
        }
    }

    /// Takes the path field's leading part as it now reads, letting go of
    /// what was said or asked of the one it had until now: its refusal, and
    /// an answer for it still on its way.
    fn leading_changed(&mut self) {
        self.leading = self.split_field().0.to_owned();
        self.field_refusal = None;
        self.awaiting
            .retain(|_, asked| !matches!(asked, Asked::Root(_)));
    }

    /// Stands the tree afresh on `root`, open and focused, answering the
    /// listing it asks for where its children are not had.
    fn reroot(&mut self, root: PathBuf) -> Option<DirectoryListingAsk> {
        self.root.clone_from(&root);
        self.opened.clear();
        self.focused = RowKey::root(root);
        self.window.reveal();
        self.open_row(self.focused.clone())
    }

    /// Puts focus where the field's tail leaves it. While the tail narrows the
    /// root's children, focus standing on the root or on a row narrowed away
    /// moves to the first child left — the one Tab completes to — or to the
    /// root where none is; otherwise it stays where it stands while that row
    /// is still drawn.
    fn settle_focus(&mut self) {
        let directories = self.directories();
        let drawn = directories.contains(&self.focused);
        if !self.filter.is_empty() && (!drawn || self.focused.is_root()) {
            self.focused = directories
                .into_iter()
                .nth(1)
                .unwrap_or_else(|| RowKey::root(self.root.clone()));
        } else if !drawn {
            self.focused = RowKey::root(self.root.clone());
        }
        self.window.reveal();
    }

    /// The path field split at its last separator: the leading part naming
    /// the root, and the partial name after it narrowing the root's children.
    /// A separator standing for the filesystem's root or a drive's stays with
    /// the leading part, which would name somewhere else without it; a field
    /// without a separator is all tail, beneath the directory a relative path
    /// is read from.
    fn split_field(&self) -> (&str, &str) {
        let style = self.style();
        let Some((index, separator)) = self
            .field
            .char_indices()
            .rfind(|(_, character)| style.is_separator(*character))
        else {
            return ("", &self.field);
        };
        let after = index + separator.len_utf8();
        let head = &self.field[..index];
        let names_a_root = head.chars().all(|character| style.is_separator(character))
            || (style == PathStyle::Windows && head.ends_with(':'));
        let leading = if names_a_root {
            &self.field[..after]
        } else {
            head
        };
        (leading, &self.field[after..])
    }

    /// `path` in the Server's syntax with a separator after it, ready for a
    /// name beneath it; one ending in a separator already, or empty, is left
    /// as it is.
    fn beneath(&self, path: &str) -> String {
        let style = self.style();
        let mut path = path.to_owned();
        if path
            .chars()
            .last()
            .is_some_and(|last| !style.is_separator(last))
        {
            path.push(style.separator());
        }
        path
    }

    /// The Outlook's Server's path syntax, read from the Execution Directory
    /// as that Server spelled it rather than from this Client's platform,
    /// which a Remote need not share.
    fn style(&self) -> PathStyle {
        PathStyle::of_absolute(&self.base)
    }

    /// The names the Server listed `row` and each directory above it beneath
    /// the root under, root first; `None` where any of them is no longer
    /// listed.
    fn names_beneath_root(&self, row: &RowKey) -> Option<Vec<String>> {
        row.0
            .windows(2)
            .map(|pair| match self.tree.get(&pair[0]) {
                Some(DirectoryEntries::Listed(listing)) => listing
                    .children
                    .iter()
                    .find(|child| child.path == pair[1])
                    .map(|child| child.name.clone()),
                _ => None,
            })
            .collect()
    }

    /// The directories drawn, in the order the tree draws them.
    fn directories(&self) -> Vec<RowKey> {
        self.rows()
            .into_iter()
            .filter_map(|row| match row.kind {
                DirectoryBrowserRowKind::Directory { key, .. } => Some(key),
                _ => None,
            })
            .collect()
    }

    /// The tree projected from what the Server has said: the root first, then
    /// every open row's children beneath it, depth first.
    ///
    /// Only an open row is descended into, and every row beneath one is a row
    /// of its own, so a link leading back up the tree is walked only as far as
    /// the reader keeps opening it.
    fn rows(&self) -> Vec<DirectoryBrowserRow> {
        let root = RowKey::root(self.root.clone());
        let read = match self.tree.get(root.directory()) {
            Some(DirectoryEntries::Listed(listing)) => Some(&listing.source_control),
            _ => None,
        };
        let mut rows = vec![self.directory_row(0, None, read, root.clone())];
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
            Some(DirectoryEntries::Listed(listing)) => {
                // Only the root's children are narrowed, by how their names
                // begin with the field's tail, case set aside.
                let narrowed = row.is_root().then(|| self.filter.to_lowercase());
                for child in listing.children.iter().filter(|child| {
                    narrowed
                        .as_ref()
                        .is_none_or(|filter| child.name.to_lowercase().starts_with(filter.as_str()))
                }) {
                    let child_row = row.child(&child.path);
                    rows.push(self.directory_row(
                        depth,
                        Some(&child.name),
                        Some(&child.source_control),
                        child_row.clone(),
                    ));
                    self.push_children(&child_row, depth + 1, rows);
                }
            }
        }
    }

    fn directory_row(
        &self,
        depth: usize,
        name: Option<&str>,
        source_control: Option<&DirectorySourceControl>,
        key: RowKey,
    ) -> DirectoryBrowserRow {
        DirectoryBrowserRow {
            depth,
            kind: DirectoryBrowserRowKind::Directory {
                name: name.map(str::to_owned),
                opened: self.opened.contains(&key),
                focused: self.focused == key,
                source_control: source_control.cloned(),
                key,
            },
        }
    }

    /// Walks focus `distance` directories through the rows drawn, wrapping
    /// past either end; the lines beneath a directory still being read or
    /// refused are not rows to stand on.
    fn move_focus(&mut self, distance: isize) {
        let mut directories = self.directories();
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
