//! Git's on-disk metadata, read without spawning Git, for the readings
//! checkout observation repeats on every poll and discovery repeats for every
//! Worktree a Repository lists. Each reading either answers as
//! Git would or declines with `None`, and a declined reading is taken by
//! running Git instead: anything this does not fully understand — refs kept
//! in a reftable, a branch that is itself symbolic, an include in the
//! configuration, a file Git would refuse as malformed — costs a spawn rather
//! than a wrong answer.
//!
//! Two differences are deliberate. Git confirms that the object a branch
//! names exists and peels it to a commit; this takes the object id as the
//! commit without opening the object database. Git refuses to point a branch
//! at anything but a commit, so the two differ only in a corrupted Repository.
//! And configuration is read only as far as it says how to read the
//! Repository: the Repository's own files are parsed whole and their
//! repository format and extensions checked, but user and system
//! configuration is not read, and a value given for any other setting is
//! not judged — though Git refuses to run at all where either is malformed,
//! as for `core.ignoreCase = maybe`.
//!
//! Git replaces a ref, `packed-refs`, and `HEAD` by renaming a finished file
//! into place, so no read here sees one half-written.
use super::{Entry, path_from_bytes};
use crate::protocol::CheckoutRevision;
use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

#[derive(Default)]
pub(super) struct OnDisk {
    /// Each Repository's packed branches, kept while its `packed-refs` is
    /// unchanged: a large one would otherwise be read whole on every poll.
    packed: Mutex<HashMap<PathBuf, Packed>>,
}
struct Packed {
    stamp: Stamp,
    /// The hash width the file was read at: a Repository whose format
    /// changes must not be answered from ids read at the old width.
    id_length: usize,
    branches: Arc<HashMap<String, String>>,
}
/// What tells a rewritten `packed-refs` from the one last read, as Git's own
/// cache of it tells them: its size, modification time, and on Unix the
/// file itself, which a rename into place always replaces, and its change
/// time, which no rewrite can set back.
#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl OnDisk {
    /// The branch and commit a Worktree root's HEAD names, as `symbolic-ref
    /// HEAD` and `rev-parse HEAD^{commit}` would read them.
    pub(super) fn revision(&self, root: &Path) -> Option<CheckoutRevision> {
        let metadata = metadata_directory(root)?;
        let common = common_directory(&metadata)?;
        let format = Format::of(&common, &metadata)?;
        let head = regular_file(&metadata.join("HEAD"))??;
        let head = trim_end(&head);
        let Some(target) = head.strip_prefix(b"ref:") else {
            return Some(CheckoutRevision::Detached {
                commit: format.object_id(head)?,
            });
        };
        let name = std::str::from_utf8(trim_start(target)).ok()?;
        let branch = name.strip_prefix("refs/heads/")?;
        if !valid_ref_name(name) {
            return None;
        }
        let commit = match loose_ref(&common, name, &format)? {
            Some(commit) => Some(commit),
            None => self.packed_branches(&common, &format)?.get(name).cloned(),
        };
        Some(CheckoutRevision::Branch {
            name: branch.to_owned(),
            commit,
        })
    }

    /// The Worktrees `git worktree list` names for the Repository whose
    /// shared metadata is `common`: its main Worktree first, then each linked
    /// one by path. They are named only; observation reads their revisions.
    pub(super) fn worktrees(&self, common: &Path) -> Option<Vec<Entry>> {
        let format = Format::of(common, common)?;
        recognized_head(common, &format)?;
        // Git reads each Worktree's HEAD for its listing, and fails it on
        // packed refs it would refuse.
        self.packed_branches(common, &format)?;
        // Git names the main Worktree for the metadata directory itself, as
        // its parent where that directory is a `.git`.
        let main = if common.file_name().is_some_and(|name| name == ".git") {
            common.parent()?.to_owned()
        } else {
            common.to_owned()
        };
        let mut linked = Vec::new();
        match std::fs::read_dir(common.join("worktrees")) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
            Ok(directory) => {
                for entry in directory {
                    let metadata = entry.ok()?.path();
                    if !metadata.is_dir() {
                        continue;
                    }
                    // Git passes over a Worktree whose pointer back to its
                    // root is missing or empty, but fails the whole listing
                    // on a lock it cannot read.
                    regular_file(&metadata.join("locked"))?;
                    let Some(pointer) = regular_file(&metadata.join("gitdir"))? else {
                        continue;
                    };
                    if pointer.is_empty() {
                        continue;
                    }
                    // One of nothing but space Git reads in a way of its own.
                    let pointer = trim_end(&pointer);
                    if pointer.is_empty() {
                        return None;
                    }
                    let root = pointer_path(pointer.strip_suffix(b"/.git").unwrap_or(pointer))?;
                    linked.push(Entry {
                        root: if root.is_absolute() {
                            root
                        } else {
                            metadata.join(root)
                        },
                        bare: false,
                        revision: None,
                    });
                }
            }
        }
        linked.sort_by(|left, right| left.root.cmp(&right.root));
        let mut entries = vec![Entry {
            root: main,
            // Left unsaid, Git decides bareness from where it runs.
            bare: format.bare?,
            revision: None,
        }];
        entries.extend(linked);
        Some(entries)
    }

    /// Whether `rev-parse --is-bare-repository` run at `common`, a
    /// Repository's shared metadata, would deny it is bare because the
    /// Repository's own configuration says it is not. Discovery reads Git
    /// failing to answer there as a denial too, so only configuration that
    /// calls the Repository bare, or leaves it unsaid, is Git's to judge.
    pub(super) fn configured_not_bare(&self, common: &Path) -> bool {
        Format::of(common, common).is_some_and(|format| format.bare == Some(false))
    }

    /// The root `rev-parse --show-toplevel` would answer at `path`, where
    /// `--git-common-dir` there would answer `common`: Git finds `path`'s own
    /// `.git` first, recognizes the metadata it leads to as sharing `common`,
    /// and takes `path` for that metadata's work tree. Only a root Git would
    /// accept outright is confirmed; one whose ownership only
    /// `safe.directory` could vouch for, or anything else less plain, is left
    /// to Git to judge.
    ///
    /// Git run there would refuse configuration it cannot accept, which is
    /// judged here only as far as the Git that discovered the Repository
    /// already ran on it: the shared, user and system configuration, but not
    /// what a Worktree has of its own, which only Git run in that Worktree
    /// reads. A Worktree with any is left to Git. What user or system
    /// configuration includes for some Worktrees alone, by their branch or
    /// metadata, is not read here at all.
    pub(super) fn worktree_root(&self, path: &Path, common: &Path) -> Option<PathBuf> {
        let root = crate::paths::canonical(path).ok()?;
        let metadata = crate::paths::canonical(metadata_directory(&root)?).ok()?;
        if crate::paths::canonical(common_directory(&metadata)?).ok()? != common {
            return None;
        }
        let format = Format::of(common, &metadata)?;
        if format.worktree_config && regular_file(&metadata.join("config.worktree"))?.is_some() {
            return None;
        }
        recognized_head(&metadata, &format)?;
        // A linked Worktree takes the shared configuration's bareness only
        // where each Worktree may have configuration of its own.
        let linked = regular_file(&metadata.join("commondir"))?.is_some();
        let bare = if linked && !format.worktree_config {
            None
        } else {
            format.bare
        };
        // Git runs in a Repository outright only where the root, its `.git`,
        // and the metadata it finds there are all the user's own.
        let owned = [&root, &root.join(".git"), &metadata]
            .into_iter()
            .all(|path| owned_by_current_user(path));
        (bare != Some(true) && owned).then_some(root)
    }

    fn packed_branches(
        &self,
        common: &Path,
        format: &Format,
    ) -> Option<Arc<HashMap<String, String>>> {
        let path = common.join("packed-refs");
        let stamp = match std::fs::metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(Arc::default()),
            Err(_) => return None,
            Ok(metadata) => Stamp::of(&metadata)?,
        };
        if let Some(packed) = self.packed.lock().unwrap().get(common)
            && packed.stamp == stamp
            && packed.id_length == format.id_length
        {
            return Some(packed.branches.clone());
        }
        // Stamped before reading: a file replaced in between is read again
        // next time, since its stamp no longer matches.
        let branches = match std::fs::read(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(Arc::default()),
            Err(_) => return None,
            Ok(bytes) => Arc::new(packed_branches(&bytes, format)?),
        };
        self.packed.lock().unwrap().insert(
            common.to_owned(),
            Packed {
                stamp,
                id_length: format.id_length,
                branches: branches.clone(),
            },
        );
        Some(branches)
    }
}

/// Where a Worktree root keeps its own metadata: its `.git` directory, or
/// the directory its `.git` file points to.
fn metadata_directory(root: &Path) -> Option<PathBuf> {
    let entry = root.join(".git");
    let metadata = std::fs::metadata(&entry).ok()?;
    if metadata.is_dir() {
        return Some(entry);
    }
    if !metadata.is_file() {
        return None;
    }
    let pointer = std::fs::read(&entry).ok()?;
    let path = pointer_path(trim_line_ends(&pointer).strip_prefix(b"gitdir: ")?)?;
    Some(root.join(path))
}

/// The metadata a linked Worktree shares with its Repository, named by its
/// `commondir` file; a main Worktree's own metadata is the shared one.
fn common_directory(metadata: &Path) -> Option<PathBuf> {
    match regular_file(&metadata.join("commondir"))? {
        None => Some(metadata.to_owned()),
        Some(pointer) => Some(metadata.join(pointer_path(trim_line_ends(&pointer))?)),
    }
}

/// Whether Git would recognize `metadata` as metadata to run in, by a HEAD
/// of a ref's shape.
fn recognized_head(metadata: &Path, format: &Format) -> Option<()> {
    let head = regular_file(&metadata.join("HEAD"))??;
    let head = trim_end(&head);
    match head.strip_prefix(b"ref:") {
        Some(target) if trim_start(target).starts_with(b"refs/") => Some(()),
        Some(_) => None,
        None => format.object_id(head).map(|_| ()),
    }
}

/// Whether Git takes `path` for the user's own, as it requires of the
/// Repository it runs in: owned by the user Git runs as.
#[cfg(unix)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let user = unsafe { libc::geteuid() };
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.uid() == user)
}

/// Whether Git takes `path` for the user's own, as it requires of the
/// Repository it runs in: owned by the user Git runs as, or by the
/// Administrators group where that user is one of them.
#[cfg(windows)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, LocalFree},
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            CheckTokenMembership, IsValidSid, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, WinBuiltinAdministratorsSid,
        },
    };
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let read = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if read != ERROR_SUCCESS {
        return false;
    }
    let mut member = 0;
    let owned = !owner.is_null()
        && unsafe { IsValidSid(owner) } != 0
        && (current_user_is(owner)
            || (unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0
                && unsafe { CheckTokenMembership(ptr::null_mut(), owner, &mut member) } != 0
                && member != 0));
    // The owner lies within the descriptor, freed only once it is compared.
    unsafe { LocalFree(descriptor) };
    owned
}

/// Whether `sid` is the user this process runs as.
#[cfg(windows)]
fn current_user_is(sid: windows_sys::Win32::Security::PSID) -> bool {
    use windows_sys::Win32::Security::EqualSid;
    crate::runtime::CurrentWindowsUser::read()
        .is_ok_and(|user| unsafe { EqualSid(sid, user.sid()) } != 0)
}

/// Elsewhere ownership is left to Git to judge.
#[cfg(not(any(unix, windows)))]
fn owned_by_current_user(_path: &Path) -> bool {
    false
}

/// The path a pointer file names, unless it holds a NUL: Git would read the
/// path only as far as that.
fn pointer_path(bytes: &[u8]) -> Option<PathBuf> {
    (!bytes.contains(&0)).then(|| path_from_bytes(bytes))
}

/// A loose ref's object id: `Some(None)` where there is no loose ref by that
/// name, so that a packed one may stand; `None` where it cannot be read.
fn loose_ref(common: &Path, name: &str, format: &Format) -> Option<Option<String>> {
    let path = common.join(name);
    // A directory by that name holds other refs, never this one.
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return Some(None);
    }
    match regular_file(&path)? {
        None => Some(None),
        Some(contents) => format.object_id(trim_end(&contents)).map(Some),
    }
}

/// The `refs/heads/` entries of a `packed-refs` file by name, or `None` for a
/// file Git itself would refuse: one that is unterminated, carries its header
/// anywhere but first, has a blank or unrecognized line, peels nothing, or
/// names a ref twice. A file its header calls sorted is searched by Git as
/// though it were, so one out of order is refused too: Git would miss refs
/// in it that a reading in full would find.
fn packed_branches(bytes: &[u8], format: &Format) -> Option<HashMap<String, String>> {
    let mut branches = HashMap::new();
    let mut names = std::collections::HashSet::new();
    let Some(body) = bytes.strip_suffix(b"\n") else {
        return bytes.is_empty().then(HashMap::new);
    };
    let mut sorted = false;
    let mut previous: Option<&str> = None;
    let mut peelable = false;
    for (index, line) in body.split(|byte| *byte == b'\n').enumerate() {
        if line.starts_with(b"#") {
            // Only a header, and only first.
            let traits = line
                .strip_prefix(b"# pack-refs with: ")
                .filter(|_| index == 0)?;
            sorted = traits
                .split(|byte| *byte == b' ')
                .any(|name| name == b"sorted");
            continue;
        }
        // The object a tag peels to follows its tag, once.
        if let Some(peeled) = line.strip_prefix(b"^") {
            format.object_id(peeled)?;
            if !std::mem::take(&mut peelable) {
                return None;
            }
            continue;
        }
        let space = line.iter().position(|byte| *byte == b' ')?;
        let id = format.object_id(&line[..space])?;
        let name = std::str::from_utf8(&line[space + 1..]).ok()?;
        if !valid_ref_name(name)
            || !names.insert(name)
            || (sorted && previous.is_some_and(|previous| previous >= name))
        {
            return None;
        }
        previous = Some(name);
        if name.starts_with("refs/heads/") {
            branches.insert(name.to_owned(), id);
        }
        peelable = true;
    }
    Some(branches)
}

/// Whether `name` is a ref name Git would accept, so anything else is left
/// to Git to judge.
fn valid_ref_name(name: &str) -> bool {
    !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("@{")
        && name != "@"
        && !name
            .bytes()
            .any(|byte| byte < 0x20 || byte == 0x7f || b" ~^:?*[\\".contains(&byte))
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// A regular file's contents: `Some(None)` where there is none, and `None`
/// where something else stands there — a symlinked ref is an old spelling of
/// a symbolic one — or it cannot be read.
fn regular_file(path: &Path) -> Option<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Some(None),
        Ok(metadata) if metadata.is_file() => {}
        _ => return None,
    }
    match std::fs::read(path) {
        Ok(contents) => Some(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

/// Whitespace as Git's own `isspace` knows it, which is narrower than
/// Unicode's or even ASCII's.
fn git_space(byte: &u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}
fn trim_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !git_space(byte))
        .map_or(0, |last| last + 1);
    &bytes[..end]
}
fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !git_space(byte))
        .unwrap_or(bytes.len());
    &bytes[start..]
}
/// A pointer file's path, which keeps any spaces it ends with.
fn trim_line_ends(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

/// What a Repository's layout and configuration say about reading it, known
/// only where Git would read it too: its metadata has the objects and refs
/// Git recognizes it by, its refs are files, and its configuration parses and
/// names no repository format or extension this does not understand.
struct Format {
    /// The length of an object id in hexadecimal, by the Repository's hash.
    id_length: usize,
    bare: Option<bool>,
    /// Whether each Worktree has configuration of its own.
    worktree_config: bool,
}
impl Format {
    /// The format of the Repository whose shared metadata is `common`, read
    /// as from the Worktree whose own metadata is `metadata`.
    fn of(common: &Path, metadata: &Path) -> Option<Self> {
        let directory = |name| std::fs::metadata(common.join(name)).is_ok_and(|m| m.is_dir());
        // A reftable Repository leaves a decoy HEAD and ref files that would
        // only mislead; its refs are read by Git.
        if !directory("objects") || !directory("refs") || directory("reftable") {
            return None;
        }
        let mut config = Config::default();
        config.read(&regular_file(&common.join("config"))??)?;
        let mut format = config.format()?;
        // Per-Worktree configuration may settle bareness or move the work
        // tree, but Git takes the format from the shared file alone.
        if config.worktree_config
            && let Some(worktree) = regular_file(&metadata.join("config.worktree"))?
        {
            let mut overrides = Config::default();
            overrides.read(&worktree)?;
            if overrides.work_tree {
                return None;
            }
            format.bare = overrides.bare.or(format.bare);
        }
        Some(format)
    }

    /// A whole object id in this Repository's hash, spelled as Git spells it.
    fn object_id(&self, bytes: &[u8]) -> Option<String> {
        let hex = bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
        let null = bytes.iter().all(|byte| *byte == b'0');
        (hex && !null && bytes.len() == self.id_length)
            .then(|| String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The settings of a Repository's own configuration that decide how it is
/// read. Reading declines a file Git would refuse to parse, and one whose
/// settings could come from elsewhere: an include, or a continued line.
#[derive(Default)]
struct Config {
    bare: Option<bool>,
    work_tree: bool,
    worktree_config: bool,
    version: Option<i64>,
    extensions: Vec<(String, Option<String>)>,
}
impl Config {
    fn read(&mut self, contents: &[u8]) -> Option<()> {
        let contents = std::str::from_utf8(contents).ok()?;
        let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
        // `None` before any section; `Some(None)` within one with a
        // subsection, which never holds these settings.
        let mut section: Option<Option<String>> = None;
        for line in contents.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut rest = line.trim_start_matches([' ', '\t']);
            if let Some(header) = rest.strip_prefix('[') {
                let (name, after) = section_header(header)?;
                section = Some(name);
                // A setting may follow its header on the same line.
                rest = after.trim_start_matches([' ', '\t']);
            }
            if rest.is_empty() || rest.starts_with(['#', ';']) {
                continue;
            }
            let (key, value) = setting(rest)?;
            if let Some(name) = section.as_ref()? {
                self.set(name, &key, value.as_deref())?;
            }
        }
        Some(())
    }

    fn set(&mut self, section: &str, key: &str, value: Option<&str>) -> Option<()> {
        match (section, key) {
            ("core", "bare") => self.bare = Some(boolean(value)?),
            ("core", "worktree") => self.work_tree = true,
            ("core", "repositoryformatversion") => self.version = Some(value?.parse().ok()?),
            ("extensions", name) => {
                // Git refuses an extension whose value it cannot read.
                match name {
                    "worktreeconfig" => self.worktree_config = boolean(value)?,
                    "preciousobjects" | "relativeworktrees" => {
                        boolean(value)?;
                    }
                    "partialclone" | "objectformat" | "refstorage" => {
                        value?;
                    }
                    _ => {}
                }
                self.extensions
                    .push((name.to_owned(), value.map(str::to_owned)));
            }
            _ => {}
        }
        Some(())
    }

    /// The Repository's format, where Git would accept it and its refs are
    /// files: version 1 Repositories must understand every extension they
    /// name, and version 0 ones honor only a few.
    fn format(&self) -> Option<Format> {
        if self.work_tree {
            return None;
        }
        let version = self.version.unwrap_or(0);
        if version != 0 && version != 1 {
            return None;
        }
        let mut id_length = 40;
        for (name, value) in &self.extensions {
            match (version, name.as_str(), value.as_deref()) {
                (_, "noop" | "preciousobjects" | "partialclone" | "worktreeconfig", _) => {}
                (1, "noop-v1" | "relativeworktrees", _) => {}
                (1, "refstorage", Some("files")) => {}
                (1, "objectformat", Some("sha1")) => id_length = 40,
                (1, "objectformat", Some("sha256")) => id_length = 64,
                _ => return None,
            }
        }
        Some(Format {
            id_length,
            bare: self.bare,
            worktree_config: self.worktree_config,
        })
    }
}

/// A section header after its `[`: the section's name where it has no
/// subsection, and what follows the `]`. Includes are declined here, since
/// they would carry settings from another file.
fn section_header(header: &str) -> Option<(Option<String>, &str)> {
    let end = header
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
        .unwrap_or(header.len());
    let (name, rest) = header.split_at(end);
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if base.is_empty() || base == "include" || base == "includeif" {
        return None;
    }
    if let Some(rest) = rest.strip_prefix(']') {
        return Some(((!name.contains('.')).then_some(base), rest));
    }
    // `[section "subsection"]`, its subsection quoted with escapes.
    let quoted = rest
        .strip_prefix([' ', '\t'])?
        .trim_start_matches([' ', '\t'])
        .strip_prefix('"')?;
    let mut chars = quoted.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next()?;
            }
            '"' => return Some((None, quoted[index + 1..].strip_prefix(']')?)),
            _ => {}
        }
    }
    None
}

/// A setting's lowercased key and its value: `None` for a key without one,
/// which Git reads as true and lets nothing follow, not even a comment.
fn setting(line: &str) -> Option<(String, Option<String>)> {
    let end = line
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(line.len());
    let (key, rest) = line.split_at(end);
    if !key.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let value = match rest.strip_prefix('=') {
        Some(value) => Some(config_value(value)?),
        None if rest.is_empty() => None,
        None => return None,
    };
    Some((key.to_ascii_lowercase(), value))
}

/// A value as Git reads one: quotes joined and removed, escapes resolved,
/// a trailing comment and unquoted outer spaces dropped. A backslash ending
/// the line continues the value onto the next, which is declined.
fn config_value(value: &str) -> Option<String> {
    let mut read = String::new();
    let mut quoted = false;
    let mut spaces = 0;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if !quoted && (c == ' ' || c == '\t') {
            if !read.is_empty() {
                spaces += 1;
            }
            continue;
        }
        if !quoted && (c == '#' || c == ';') {
            break;
        }
        read.extend(std::iter::repeat_n(' ', std::mem::take(&mut spaces)));
        match c {
            '\\' => read.push(match chars.next()? {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                't' => '\t',
                'b' => '\u{8}',
                _ => return None,
            }),
            '"' => quoted = !quoted,
            c => read.push(c),
        }
    }
    (!quoted).then_some(read)
}

/// A boolean as Git spells one; a key without a value is true.
fn boolean(value: Option<&str>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    // Git also reads a number, in bases and with units and bounds of its own;
    // any but these plainest are left to it.
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::OnDisk;
    use crate::protocol::CheckoutRevision;
    use crate::source_control::git::{
        Entry, GitSourceControl, canonical_checkout_path, parse_worktrees,
    };
    use std::path::{Path, PathBuf};

    /// Real Repositories, made by a Git that reads no configuration but its own.
    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        configuration: PathBuf,
        adapter: GitSourceControl,
    }
    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = crate::paths::canonical(temporary.path()).unwrap();
            let configuration = root.join("gitconfig");
            std::fs::write(&configuration, "").unwrap();
            let adapter = GitSourceControl::default().with_configuration_file(&configuration);
            Self {
                _temporary: temporary,
                root,
                configuration,
                adapter,
            }
        }
        fn run(&self, directory: &Path, args: &[&str]) -> std::process::Output {
            let mut command = std::process::Command::new("git");
            command.arg("-C").arg(directory).args(args);
            for name in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_COMMON_DIR",
                "GIT_INDEX_FILE",
            ] {
                command.env_remove(name);
            }
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &self.configuration)
                .env("GIT_AUTHOR_NAME", "Suru Test")
                .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
                .env("GIT_COMMITTER_NAME", "Suru Test")
                .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
                .output()
                .unwrap()
        }
        fn git(&self, directory: &Path, args: &[&str]) {
            let output = self.run(directory, args);
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        /// Whether this Git offers what `args` asks of it; older ones lack
        /// some of the layouts below, which are then passed over.
        fn supports(&self, directory: &Path, args: &[&str]) -> bool {
            self.run(directory, args).status.success()
        }
        fn repository(&self, name: &str) -> PathBuf {
            let root = self.root.join(name);
            std::fs::create_dir_all(&root).unwrap();
            self.git(&root, &["init", "-b", "main"]);
            root
        }
        fn commit(&self, root: &Path) {
            self.git(
                root,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "commit",
                ],
            );
        }
        /// A linked Worktree of `repository` named `name`, added with `args`.
        fn worktree(&self, repository: &Path, name: &str, args: &[&str]) -> PathBuf {
            let root = self.root.join(name);
            let mut add = vec!["worktree", "add"];
            add.extend(args);
            add.push(root.to_str().unwrap());
            self.git(repository, &add);
            root
        }
        /// Whether this Git can initialize a Repository at `name` with `args`.
        fn initializes(&self, name: &str, args: &[&str]) -> Option<PathBuf> {
            let root = self.root.join(name);
            let mut init = vec!["init", "-b", "main"];
            init.extend(args);
            init.push(root.to_str().unwrap());
            self.supports(&self.root, &init).then_some(root)
        }
        /// What the disk says of a root, after asserting Git says the same.
        async fn agreed_revision(&self, on_disk: &OnDisk, root: &Path) -> CheckoutRevision {
            let read = on_disk.revision(root);
            assert!(read.is_some(), "{} was not read from disk", root.display());
            assert_eq!(
                read,
                self.adapter.git_revision(root).await,
                "{}",
                root.display()
            );
            read.unwrap()
        }
        /// A root the disk declines, still read through Git as before.
        async fn declined_revision(
            &self,
            on_disk: &OnDisk,
            root: &Path,
        ) -> Option<CheckoutRevision> {
            assert_eq!(on_disk.revision(root), None, "{}", root.display());
            let read = self.adapter.read_revision(root).await;
            assert_eq!(read, self.adapter.git_revision(root).await);
            read
        }
        /// The root the disk confirms `path` as a Worktree of the Repository
        /// whose shared metadata is `common` at, after asserting Git
        /// confirms the same.
        async fn agreed_root(&self, on_disk: &OnDisk, path: &Path, common: &Path) -> PathBuf {
            let read = on_disk.worktree_root(path, common);
            assert!(
                read.is_some(),
                "{} was not confirmed from disk",
                path.display()
            );
            assert_eq!(
                read,
                self.adapter.valid_root(path, common).await,
                "{}",
                path.display()
            );
            read.unwrap()
        }
        /// What Git makes of a root the disk declines to confirm.
        async fn declined_root(
            &self,
            on_disk: &OnDisk,
            path: &Path,
            common: &Path,
        ) -> Option<PathBuf> {
            assert_eq!(
                on_disk.worktree_root(path, common),
                None,
                "{}",
                path.display()
            );
            self.adapter.valid_root(path, common).await
        }
        async fn git_listing(&self, common: &Path) -> Option<Vec<Entry>> {
            let output = self
                .adapter
                .command(common, &["worktree", "list", "--porcelain", "-z"])
                .await
                .unwrap();
            output
                .status
                .success()
                .then(|| parse_worktrees(&output.stdout))
        }
        /// Asserts the disk names the same Worktrees Git lists, in the same
        /// roles.
        async fn agreed_listing(&self, on_disk: &OnDisk, common: &Path) {
            let read = on_disk
                .worktrees(common)
                .unwrap_or_else(|| panic!("{} was not listed from disk", common.display()));
            let listed = self.git_listing(common).await.unwrap();
            let named = |entries: &[Entry]| {
                let mut named = entries
                    .iter()
                    .map(|entry| (canonical_checkout_path(&entry.root), entry.bare))
                    .collect::<Vec<_>>();
                named[1..].sort();
                named
            };
            assert_eq!(named(&read), named(&listed), "{}", common.display());
        }
    }

    /// What `act` answers, and how many Git commands it ran, by the Log each
    /// one leaves.
    async fn counting_git<T>(act: impl std::future::Future<Output = T>) -> (T, usize) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("git.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::sync::Arc::new(std::fs::File::create(&path).unwrap()))
            .finish();
        let answered = {
            let _log = tracing::subscriber::set_default(subscriber);
            act.await
        };
        let log = std::fs::read_to_string(path).unwrap();
        let commands = log
            .lines()
            .filter(|line| line.contains("Git command"))
            .count();
        (answered, commands)
    }

    fn branch(name: &str, commit: Option<&str>) -> CheckoutRevision {
        CheckoutRevision::Branch {
            name: name.to_owned(),
            commit: commit.map(str::to_owned),
        }
    }
    fn commit_of(revision: &CheckoutRevision) -> &str {
        match revision {
            CheckoutRevision::Branch { commit, .. } => commit.as_deref().unwrap(),
            CheckoutRevision::Detached { commit } => commit,
        }
    }

    #[tokio::test]
    async fn reads_branch_and_commit_as_git_does_whether_loose_or_packed() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        let heads = main.join(".git").join("refs").join("heads");
        assert_eq!(
            fixture.agreed_revision(&on_disk, &main).await,
            branch("main", None),
            "an unborn branch has no commit"
        );
        fixture.commit(&main);
        let loose = fixture.agreed_revision(&on_disk, &main).await;
        fixture.git(&main, &["pack-refs", "--all"]);
        assert!(!heads.join("main").exists());
        assert_eq!(fixture.agreed_revision(&on_disk, &main).await, loose);
        // A loose ref stands over the packed one it has moved on from.
        fixture.commit(&main);
        assert!(heads.join("main").exists());
        let moved = fixture.agreed_revision(&on_disk, &main).await;
        assert_ne!(commit_of(&moved), commit_of(&loose));
        // Packing again rewrites the packed refs the reader has already read.
        fixture.git(&main, &["pack-refs", "--all"]);
        assert_eq!(fixture.agreed_revision(&on_disk, &main).await, moved);
        // An annotated tag is packed with the commit it peels to.
        fixture.git(&main, &["tag", "-a", "-m", "tag", "v1"]);
        fixture.git(&main, &["pack-refs", "--all"]);
        assert_eq!(fixture.agreed_revision(&on_disk, &main).await, moved);
        fixture.git(&main, &["checkout", "-q", "-b", "suru/nested-name"]);
        assert_eq!(
            fixture.agreed_revision(&on_disk, &main).await,
            branch("suru/nested-name", Some(commit_of(&moved)))
        );
        fixture.git(&main, &["checkout", "-q", "--detach"]);
        assert_eq!(
            fixture.agreed_revision(&on_disk, &main).await,
            CheckoutRevision::Detached {
                commit: commit_of(&moved).to_owned()
            }
        );
    }

    #[tokio::test]
    async fn reads_linked_and_separately_kept_worktrees_as_git_does() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let topic = fixture.worktree(&main, "topic", &["-b", "topic"]);
        assert!(matches!(
            fixture.agreed_revision(&on_disk, &topic).await,
            CheckoutRevision::Branch { name, commit: Some(_) } if name == "topic"
        ));
        let detached = fixture.worktree(&main, "detached", &["--detach"]);
        assert!(matches!(
            fixture.agreed_revision(&on_disk, &detached).await,
            CheckoutRevision::Detached { .. }
        ));
        let relative = fixture.root.join("relative");
        if fixture.supports(
            &main,
            &[
                "worktree",
                "add",
                "--relative-paths",
                "-b",
                "relative",
                relative.to_str().unwrap(),
            ],
        ) {
            fixture.agreed_revision(&on_disk, &relative).await;
        }
        let metadata = fixture.root.join("separate-metadata");
        let separate = fixture
            .initializes(
                "separate",
                &["--separate-git-dir", metadata.to_str().unwrap()],
            )
            .unwrap();
        fixture.commit(&separate);
        fixture.agreed_revision(&on_disk, &separate).await;
        if let Some(sha256) = fixture.initializes("sha256", &["--object-format=sha256"]) {
            fixture.commit(&sha256);
            let revision = fixture.agreed_revision(&on_disk, &sha256).await;
            assert_eq!(commit_of(&revision).len(), 64);
        }
    }

    #[tokio::test]
    async fn declines_what_it_cannot_read_as_git_would_and_leaves_it_to_git() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let metadata = main.join(".git");
        let committed = fixture.agreed_revision(&on_disk, &main).await;
        // A branch that is itself symbolic is resolved by Git alone, which
        // follows it to the branch it names.
        fixture.git(
            &main,
            &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
        );
        fixture.git(&main, &["symbolic-ref", "HEAD", "refs/heads/alias"]);
        assert_eq!(
            fixture.declined_revision(&on_disk, &main).await,
            Some(committed.clone())
        );
        fixture.git(&main, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let head = std::fs::read(metadata.join("HEAD")).unwrap();
        for unreadable in [
            &b"ref: refs/tags/v1\n"[..],
            b"ref: refs/heads/.hidden\n",
            b"ref: refs/heads/a..b\n",
            "ref:\u{a0}refs/heads/main\n".as_bytes(),
            b"not a revision\n",
            b"0000000000000000000000000000000000000000\n",
        ] {
            std::fs::write(metadata.join("HEAD"), unreadable).unwrap();
            fixture.declined_revision(&on_disk, &main).await;
        }
        std::fs::remove_file(metadata.join("HEAD")).unwrap();
        assert_eq!(fixture.declined_revision(&on_disk, &main).await, None);
        std::fs::write(metadata.join("HEAD"), &head).unwrap();
        // A loose ref Git would refuse, including one of another hash's width.
        let reference = metadata.join("refs").join("heads").join("main");
        let tip = std::fs::read_to_string(&reference).unwrap();
        let tip = tip.trim_end();
        for unreadable in [
            format!("{tip} trailing\n"),
            format!("{tip}{}\n", &tip[..24]),
        ] {
            std::fs::write(&reference, unreadable).unwrap();
            fixture.declined_revision(&on_disk, &main).await;
        }
        std::fs::write(&reference, format!("{tip}\n")).unwrap();
        fixture.agreed_revision(&on_disk, &main).await;
        // Refs kept in a reftable are not files at all.
        if let Some(reftable) = fixture.initializes("reftable", &["--ref-format=reftable"]) {
            assert_eq!(
                fixture.declined_revision(&on_disk, &reftable).await,
                Some(branch("main", None))
            );
            fixture.commit(&reftable);
            assert!(matches!(
                fixture.declined_revision(&on_disk, &reftable).await,
                Some(CheckoutRevision::Branch {
                    commit: Some(_),
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn declines_packed_refs_git_would_refuse() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        fixture.commit(&main);
        fixture.git(&main, &["branch", "aaa", "HEAD~1"]);
        fixture.git(&main, &["branch", "zzz"]);
        fixture.git(&main, &["pack-refs", "--all"]);
        let common = main.join(".git");
        let packed = common.join("packed-refs");
        let contents = std::fs::read_to_string(&packed).unwrap();
        let (header, refs) = contents.split_once('\n').unwrap();
        assert!(header.contains(" sorted"), "{header}");
        let tip = refs.split(' ').next().unwrap();
        let reversed = refs.lines().rev().collect::<Vec<_>>().join("\n");
        for (case, unreadable) in [
            (
                "a sorted file out of order",
                format!("{header}\n{reversed}\n"),
            ),
            ("an unterminated line", contents.trim_end().to_owned()),
            ("a blank line", format!("{contents}\n")),
            ("a header out of place", format!("{refs}{header}\n")),
            ("a comment", format!("{header}\n# note\n{refs}")),
            ("a peel of nothing", format!("{header}\n^{tip}\n{refs}")),
            ("a peel of a peel", format!("{contents}^{tip}\n^{tip}\n")),
            ("a ref named twice", format!("{contents}{refs}")),
        ] {
            std::fs::write(&packed, unreadable).unwrap();
            assert_eq!(on_disk.revision(&main), None, "{case}");
            fixture.declined_revision(&on_disk, &main).await;
            // Git's listing reads every Worktree's HEAD through them too.
            assert!(on_disk.worktrees(&common).is_none(), "{case}");
        }
        std::fs::write(&packed, &contents).unwrap();
        let read = fixture.agreed_revision(&on_disk, &main).await;
        // A file rewritten in place with its modification time set back is
        // still told apart by the time of its change, where there is one.
        #[cfg(unix)]
        {
            let modified = std::fs::metadata(&packed).unwrap().modified().unwrap();
            let rewritten = contents.replace(
                &format!("{} refs/heads/main", commit_of(&read)),
                &format!("{tip} refs/heads/main"),
            );
            assert_ne!(rewritten, contents);
            std::fs::write(&packed, &rewritten).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&packed)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            assert_eq!(
                fixture.agreed_revision(&on_disk, &main).await,
                branch("main", Some(tip))
            );
            std::fs::write(&packed, &contents).unwrap();
        }
        // Packed refs already read are not taken at another hash's width.
        let config = std::fs::read_to_string(common.join("config")).unwrap();
        std::fs::write(
            common.join("config"),
            format!(
                "{config}[core]\n\trepositoryformatversion = 1\n\
                 [extensions]\n\tobjectFormat = sha256\n"
            ),
        )
        .unwrap();
        assert_eq!(on_disk.revision(&main), None);
    }

    #[tokio::test]
    async fn declines_configuration_git_would_refuse_or_read_from_elsewhere() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let common = main.join(".git");
        let config = std::fs::read_to_string(common.join("config")).unwrap();
        for (case, addition) in [
            ("an included file", "[include]\n\tpath = elsewhere\n"),
            (
                "a conditional include",
                "[includeIf \"gitdir:/\"]\n\tpath = elsewhere\n",
            ),
            ("a separate work tree", "[core]\n\tworktree = elsewhere\n"),
            ("a continued line", "[user]\n\tname = a \\\n\tbare = true\n"),
            ("an unterminated quote", "[user]\n\tname = \"a\n"),
            ("an unknown escape", "[user]\n\tname = a\\q\n"),
            ("an unparsable line", "[user]\n\tname : a\n"),
            ("an unparsable boolean", "[core]\n\tbare = maybe\n"),
            ("reftable refs", "[extensions]\n\trefStorage = reftable\n"),
            (
                "an unknown extension",
                "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tunknown = true\n",
            ),
            (
                "an unknown repository format",
                "[core]\n\trepositoryformatversion = 2\n",
            ),
            ("a comment after a bare key", "[core]\n\tbare # comment\n"),
            (
                "an unreadable extension",
                "[extensions]\n\tpreciousObjects = maybe\n",
            ),
            (
                "an extension wanting a value",
                "[extensions]\n\tpartialClone\n",
            ),
            ("a number Git reads its own way", "[core]\n\tbare = 08\n"),
        ] {
            std::fs::write(common.join("config"), format!("{config}{addition}")).unwrap();
            assert_eq!(on_disk.revision(&main), None, "{case}");
            assert!(on_disk.worktrees(&common).is_none(), "{case}");
        }
        // What Git reads alike, however it is spelled, the disk reads too.
        for addition in [
            "[core] bare = \"fal\"se ; a comment\n",
            "[user \"sub\\\"section\"]\n\tname = \"a # b\"\n",
            "[Core]\n\tBare\n[core]\n\tbare = off\n",
        ] {
            std::fs::write(common.join("config"), format!("{config}{addition}")).unwrap();
            fixture.agreed_revision(&on_disk, &main).await;
            fixture.agreed_listing(&on_disk, &common).await;
        }
        // Where the configuration leaves bareness unsaid, Git decides it from
        // where it runs, so the disk does not.
        std::fs::write(common.join("config"), &config).unwrap();
        fixture.git(&main, &["config", "--unset", "core.bare"]);
        assert!(on_disk.worktrees(&common).is_none());
        // Per-Worktree configuration may settle it instead.
        fixture.git(&main, &["config", "extensions.worktreeConfig", "true"]);
        std::fs::write(common.join("config.worktree"), "[core]\n\tbare = false\n").unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        std::fs::write(common.join("config.worktree"), "[core]\n\tbare = true\n").unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        std::fs::write(common.join("config.worktree"), "[core\n").unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        // But never the repository format, which Git reads from the shared
        // configuration alone.
        let shared = std::fs::read_to_string(common.join("config")).unwrap();
        std::fs::write(
            common.join("config"),
            format!("{shared}[core]\n\trepositoryformatversion = 999\n"),
        )
        .unwrap();
        std::fs::write(
            common.join("config.worktree"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        assert_eq!(on_disk.revision(&main), None);
        std::fs::remove_file(common.join("config")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
    }

    #[tokio::test]
    async fn lists_the_worktrees_git_lists() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        let common = main.join(".git");
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.commit(&main);
        let topic = fixture.worktree(&main, "topic", &["-b", "topic"]);
        fixture.worktree(&main, "detached", &["--detach"]);
        let locked = fixture.worktree(&main, "locked", &["-b", "locked"]);
        fixture.git(&main, &["worktree", "lock", locked.to_str().unwrap()]);
        fixture.supports(
            &main,
            &[
                "worktree",
                "add",
                "--relative-paths",
                "-b",
                "relative",
                fixture.root.join("relative").to_str().unwrap(),
            ],
        );
        fixture.agreed_listing(&on_disk, &common).await;
        // A Worktree whose directory has gone is still listed until pruned.
        std::fs::remove_dir_all(&topic).unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.git(&main, &["worktree", "prune"]);
        fixture.agreed_listing(&on_disk, &common).await;
        // A stray file among the Worktrees' metadata names none.
        std::fs::write(common.join("worktrees").join("stray"), "").unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.git(&main, &["config", "core.bare", "true"]);
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.git(&common, &["config", "core.bare", "false"]);
        let bare = fixture.root.join("bare.git");
        fixture.git(
            &fixture.root,
            &[
                "clone",
                "-q",
                "--bare",
                main.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        fixture.agreed_listing(&on_disk, &bare).await;
        fixture.worktree(&bare, "from-bare", &["-b", "from-bare"]);
        fixture.agreed_listing(&on_disk, &bare).await;
    }

    #[tokio::test]
    async fn declines_to_list_what_git_could_not_list() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let common = main.join(".git");
        let locked = fixture.worktree(&main, "locked", &["-b", "locked"]);
        let metadata = common.join("worktrees").join("locked");
        fixture.agreed_listing(&on_disk, &common).await;
        // A lock that cannot be read fails Git's listing; one standing as a
        // directory cannot be read on any platform.
        std::fs::create_dir(metadata.join("locked")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        assert!(fixture.git_listing(&common).await.is_none());
        std::fs::remove_dir(metadata.join("locked")).unwrap();
        // A pointer that cannot be read is left for Git to judge.
        let pointer = std::fs::read(metadata.join("gitdir")).unwrap();
        std::fs::remove_file(metadata.join("gitdir")).unwrap();
        std::fs::create_dir(metadata.join("gitdir")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        std::fs::remove_dir(metadata.join("gitdir")).unwrap();
        // So is one Git would read only in part, or as no path at all.
        for unreadable in [&b"\n"[..], b"/elsewhere\0/.git\n"] {
            std::fs::write(metadata.join("gitdir"), unreadable).unwrap();
            assert!(on_disk.worktrees(&common).is_none());
        }
        std::fs::write(metadata.join("gitdir"), pointer).unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        // Without a HEAD, or objects, Git does not know the metadata at all.
        let head = std::fs::read(common.join("HEAD")).unwrap();
        std::fs::remove_file(common.join("HEAD")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        std::fs::write(common.join("HEAD"), "ref: elsewhere\n").unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        std::fs::write(common.join("HEAD"), head).unwrap();
        std::fs::rename(common.join("objects"), main.join("objects")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
        assert_eq!(fixture.declined_revision(&on_disk, &locked).await, None);
        std::fs::rename(main.join("objects"), common.join("objects")).unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
    }

    #[tokio::test]
    async fn confirms_worktree_roots_as_git_does() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        let common = main.join(".git");
        assert_eq!(
            fixture.agreed_root(&on_disk, &main, &common).await,
            main,
            "an unborn main Worktree"
        );
        fixture.commit(&main);
        let topic = fixture.worktree(&main, "topic", &["-b", "topic"]);
        assert_eq!(fixture.agreed_root(&on_disk, &topic, &common).await, topic);
        let detached = fixture.worktree(&main, "detached", &["--detach"]);
        fixture.agreed_root(&on_disk, &detached, &common).await;
        let relative = fixture.root.join("relative");
        if fixture.supports(
            &main,
            &[
                "worktree",
                "add",
                "--relative-paths",
                "-b",
                "relative",
                relative.to_str().unwrap(),
            ],
        ) {
            assert_eq!(
                fixture.agreed_root(&on_disk, &relative, &common).await,
                relative
            );
        }
        // A pointer with Windows line ends, or a root spelled another way,
        // names the same Worktree.
        let pointer = std::fs::read_to_string(topic.join(".git")).unwrap();
        std::fs::write(topic.join(".git"), format!("{}\r\n", pointer.trim_end())).unwrap();
        fixture.agreed_root(&on_disk, &topic, &common).await;
        let waypoint = topic.join("..").join("topic");
        assert_eq!(
            fixture.agreed_root(&on_disk, &waypoint, &common).await,
            topic
        );
        // A bare Repository's linked Worktree is a work tree all the same,
        // though the Repository's own metadata is none.
        let bare = fixture.root.join("bare.git");
        fixture.git(
            &fixture.root,
            &[
                "clone",
                "-q",
                "--bare",
                main.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let from_bare = fixture.worktree(&bare, "from-bare", &["-b", "from-bare"]);
        fixture.agreed_root(&on_disk, &from_bare, &bare).await;
        assert_eq!(fixture.declined_root(&on_disk, &bare, &bare).await, None);
        // So with separately kept metadata, which Git lists in place of the
        // main Worktree it cannot name.
        let metadata = fixture.root.join("separate-metadata");
        let separate = fixture
            .initializes(
                "separate",
                &["--separate-git-dir", metadata.to_str().unwrap()],
            )
            .unwrap();
        fixture.agreed_root(&on_disk, &separate, &metadata).await;
        assert_eq!(
            fixture.declined_root(&on_disk, &metadata, &metadata).await,
            None
        );
        // Neither a directory within a Worktree, another Repository's root,
        // nor a root of another Repository than the one asked about is one.
        let inside = topic.join("inside");
        std::fs::create_dir(&inside).unwrap();
        assert_eq!(
            fixture.declined_root(&on_disk, &inside, &common).await,
            None
        );
        let other = fixture.repository("other");
        assert_eq!(fixture.declined_root(&on_disk, &other, &common).await, None);
        assert_eq!(fixture.declined_root(&on_disk, &topic, &bare).await, None);
        // Nor is a Worktree gone, or replaced by a plain directory.
        std::fs::remove_dir_all(&detached).unwrap();
        assert_eq!(
            fixture.declined_root(&on_disk, &detached, &common).await,
            None
        );
        std::fs::create_dir(&detached).unwrap();
        assert_eq!(
            fixture.declined_root(&on_disk, &detached, &common).await,
            None
        );
    }

    #[tokio::test]
    async fn leaves_worktree_roots_to_git_where_it_reads_them_its_own_way() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let common = main.join(".git");
        let topic = fixture.worktree(&main, "topic", &["-b", "topic"]);
        let config = std::fs::read_to_string(common.join("config")).unwrap();
        // Configuration the disk cannot read whole is Git's to judge.
        std::fs::write(
            common.join("config"),
            format!("{config}[include]\n\tpath = elsewhere\n"),
        )
        .unwrap();
        assert_eq!(
            fixture.declined_root(&on_disk, &main, &common).await,
            Some(main.clone())
        );
        assert_eq!(
            fixture.declined_root(&on_disk, &topic, &common).await,
            Some(topic.clone())
        );
        std::fs::write(common.join("config"), &config).unwrap();
        // A Repository configured bare has no main Worktree, though its
        // linked ones keep theirs: the setting is not theirs to read.
        fixture.git(&main, &["config", "core.bare", "true"]);
        assert_eq!(fixture.declined_root(&on_disk, &main, &common).await, None);
        fixture.agreed_root(&on_disk, &topic, &common).await;
        // Unless each Worktree reads configuration of its own, where the
        // shared setting settles it for a Worktree without any.
        fixture.git(&common, &["config", "extensions.worktreeConfig", "true"]);
        assert_eq!(fixture.declined_root(&on_disk, &topic, &common).await, None);
        fixture.git(&common, &["config", "core.bare", "false"]);
        fixture.agreed_root(&on_disk, &main, &common).await;
        fixture.agreed_root(&on_disk, &topic, &common).await;
        // A Worktree's own configuration is read only by Git run in that
        // Worktree, so discovery from another never judged it: whatever it
        // says, of bareness or of anything Git would refuse, is Git's to judge.
        let topic_own = common
            .join("worktrees")
            .join("topic")
            .join("config.worktree");
        let main_own = common.join("config.worktree");
        for (root, own, elsewhere) in [(&topic, &topic_own, &main), (&main, &main_own, &topic)] {
            for (contents, valid) in [
                ("[core]\n\tfilemode = invalid\n", false),
                ("[core]\n\tbare = true\n", false),
                ("[core]\n\tbare = false\n", true),
                ("[core]\n\tfilemode = false\n", true),
            ] {
                std::fs::write(own, contents).unwrap();
                let passed = fixture.adapter.valid_root(elsewhere, &common).await;
                assert_eq!(
                    passed.as_ref(),
                    Some(elsewhere),
                    "Git run elsewhere passes over {}",
                    own.display()
                );
                assert_eq!(
                    fixture.declined_root(&on_disk, root, &common).await,
                    valid.then(|| root.clone()),
                    "{contents:?} in {}",
                    own.display()
                );
            }
            std::fs::remove_file(own).unwrap();
            fixture.agreed_root(&on_disk, root, &common).await;
        }
        std::fs::write(common.join("config"), &config).unwrap();
        // Metadata whose HEAD Git would not recognize is no Worktree's.
        let head = common.join("worktrees").join("topic").join("HEAD");
        let contents = std::fs::read(&head).unwrap();
        std::fs::write(&head, "not a revision\n").unwrap();
        assert_eq!(fixture.declined_root(&on_disk, &topic, &common).await, None);
        std::fs::write(&head, contents).unwrap();
        fixture.agreed_root(&on_disk, &topic, &common).await;
    }

    #[tokio::test]
    async fn discovery_confirms_listed_worktrees_from_disk_and_the_rest_with_git() {
        use crate::{
            protocol::{RepositoryLocation, ResolvedWorkspace, SourceControlAvailability},
            source_control::SourceControl,
        };
        let readings = |resolved: &ResolvedWorkspace| {
            resolved
                .checkouts
                .iter()
                .map(|checkout| {
                    (
                        checkout.association.root.clone(),
                        checkout.availability.clone(),
                        checkout.association.recovery_revision.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let fixture = Fixture::new();
        let main = fixture.repository("main");
        fixture.commit(&main);
        let (_, alone) = counting_git(fixture.adapter.discover(&main)).await;
        let linked = ["a", "b", "c"].map(|name| fixture.worktree(&main, name, &["-b", name]));
        let (resolved, commands) = counting_git(fixture.adapter.discover(&main)).await;
        assert_eq!(commands, alone, "no Git command reads a listed Worktree");
        let read = readings(&resolved);
        assert_eq!(read.len(), 4);
        for (root, availability, recovery) in &read {
            assert_eq!(
                availability,
                &SourceControlAvailability::Available,
                "{}",
                root.display()
            );
            assert!(recovery.is_some(), "{}", root.display());
        }
        // Discovered from a linked Worktree, the main one is established from
        // disk too.
        let (from_linked, commands) = counting_git(fixture.adapter.discover(&linked[0])).await;
        assert_eq!(commands, alone);
        assert_eq!(
            from_linked.workspace.repository.as_ref().unwrap().location,
            RepositoryLocation::Main { root: main.clone() }
        );
        assert_eq!(readings(&from_linked), read);
        // Where the disk cannot confirm them, or the Repository's bareness,
        // Git confirms each, to the same effect.
        let config = main.join(".git").join("config");
        let contents = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            format!("{contents}[include]\n\tpath = elsewhere\n"),
        )
        .unwrap();
        let (confirmed, commands) = counting_git(fixture.adapter.discover(&main)).await;
        assert_eq!(readings(&confirmed), read);
        assert_eq!(commands, alone + 1 + 2 * read.len());
        // A Worktree whose own configuration Git refuses is unavailable,
        // though discovery from another Worktree never reads it.
        std::fs::write(&config, &contents).unwrap();
        fixture.git(&main, &["config", "extensions.worktreeConfig", "true"]);
        let own = main
            .join(".git")
            .join("worktrees")
            .join("a")
            .join("config.worktree");
        std::fs::write(&own, "[core]\n\tfilemode = invalid\n").unwrap();
        let (refused, commands) = counting_git(fixture.adapter.discover(&main)).await;
        assert_eq!(commands, alone + 1, "Git confirms that Worktree alone");
        for (root, availability, recovery) in readings(&refused) {
            let available = root != linked[0];
            assert_eq!(
                availability == SourceControlAvailability::Available,
                available,
                "{}",
                root.display()
            );
            assert_eq!(recovery.is_some(), available, "{}", root.display());
        }
    }

    #[tokio::test]
    async fn discovery_reads_a_repository_with_one_rev_parse_and_its_listing() {
        use crate::{
            protocol::{
                RepositoryLocation, ResolvedWorkspace, SourceControlAvailability,
                SourceControlCapability,
            },
            source_control::SourceControl,
        };
        let creation = |resolved: &ResolvedWorkspace| {
            let repository = resolved.workspace.repository.as_ref().unwrap();
            (
                repository.location.clone(),
                repository.capabilities.create_checkout.clone(),
            )
        };
        let fixture = Fixture::new();
        let main = fixture.repository("main");
        let (unborn, commands) = counting_git(fixture.adapter.discover(&main)).await;
        assert_eq!(commands, 2);
        assert!(matches!(
            creation(&unborn).1,
            SourceControlCapability::Unsupported { reason } if reason.contains("unborn")
        ));
        fixture.commit(&main);
        let topic = fixture.worktree(&main, "topic", &["-b", "topic"]);
        let inside = topic.join("inside");
        std::fs::create_dir(&inside).unwrap();
        for directory in [&main, &topic, &inside] {
            let (resolved, commands) = counting_git(fixture.adapter.discover(directory)).await;
            assert_eq!(commands, 2, "{}", directory.display());
            assert_eq!(
                creation(&resolved),
                (
                    RepositoryLocation::Main { root: main.clone() },
                    SourceControlCapability::Available
                ),
                "{}",
                directory.display()
            );
        }
        // A bare Repository's bareness is Git's to read at its metadata.
        let bare = fixture.root.join("bare.git");
        fixture.git(
            &fixture.root,
            &[
                "clone",
                "-q",
                "--bare",
                main.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let from_bare = fixture.worktree(&bare, "from-bare", &["-b", "from-bare"]);
        for directory in [&bare, &from_bare] {
            let (resolved, commands) = counting_git(fixture.adapter.discover(directory)).await;
            assert_eq!(commands, 3, "{}", directory.display());
            assert_eq!(
                creation(&resolved),
                (
                    RepositoryLocation::Bare { root: bare.clone() },
                    SourceControlCapability::Available
                ),
                "{}",
                directory.display()
            );
        }
        // A directory in no Repository, or in one Git cannot read, is told by
        // the one command.
        let plain = fixture.root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        let (resolved, commands) = counting_git(fixture.adapter.discover(&plain)).await;
        assert_eq!(commands, 1);
        assert_eq!(
            resolved.workspace.source_control,
            SourceControlAvailability::NotDetected
        );
        std::fs::write(plain.join(".git"), "gitdir: missing\n").unwrap();
        let (resolved, commands) = counting_git(fixture.adapter.discover(&plain)).await;
        assert_eq!(commands, 1);
        assert!(
            matches!(
                &resolved.workspace.source_control,
                SourceControlAvailability::Unavailable { reason }
                    if reason.starts_with("Git Repository discovery failed: fatal:")
            ),
            "{:?}",
            resolved.workspace.source_control
        );
    }
}
