//! Git's on-disk metadata, read without spawning Git, for the readings
//! checkout observation repeats on every poll. Each reading either answers as
//! Git would or declines with `None`, and a declined reading is taken by
//! running Git instead: anything this does not fully understand — refs kept
//! in a reftable, a branch that is itself symbolic, configuration that could
//! be continued elsewhere — costs a spawn rather than a wrong answer.
//!
//! One difference is deliberate. Git confirms that the object a branch names
//! exists and peels it to a commit; this takes the object id as the commit
//! without opening the object database. Git refuses to point a branch at
//! anything but a commit, so the two differ only in a corrupted Repository.
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
    branches: Arc<HashMap<String, String>>,
}
/// What tells a rewritten `packed-refs` from the one last read, as Git's own
/// cache of it tells them: its size, modification time, and on Unix the
/// file itself, which a rename into place always replaces.
#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    inode: u64,
}
impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(metadata),
        })
    }
}

impl OnDisk {
    /// The branch and commit a Worktree root's HEAD names, as `symbolic-ref
    /// HEAD` and `rev-parse HEAD^{commit}` would read them.
    pub(super) fn revision(&self, root: &Path) -> Option<CheckoutRevision> {
        let metadata = metadata_directory(root)?;
        let common = common_directory(&metadata)?;
        // A reftable Repository leaves a decoy HEAD and ref files that would
        // only mislead; its refs are read by Git.
        if !absent(&common.join("reftable"))? {
            return None;
        }
        let head = regular_file(&metadata.join("HEAD"))??;
        let head = trim_end(&head);
        let Some(target) = head.strip_prefix(b"ref:") else {
            return Some(CheckoutRevision::Detached {
                commit: object_id(head)?,
            });
        };
        let name = std::str::from_utf8(target).ok()?.trim_start();
        let branch = name.strip_prefix("refs/heads/")?;
        if !valid_branch(branch) {
            return None;
        }
        let commit = match loose_ref(&common, name)? {
            Some(commit) => Some(commit),
            None => self.packed_branches(&common)?.get(name).cloned(),
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
        let mut config = Config::default();
        config.read(&common.join("config"))?;
        if config.worktree_config && !absent(&common.join("config.worktree"))? {
            config.read(&common.join("config.worktree"))?;
        }
        if config.work_tree || !config.file_refs {
            return None;
        }
        // Git names the main Worktree for the metadata directory itself, as
        // its parent where that directory is a `.git`.
        let main = if common.file_name().is_some_and(|name| name == ".git") {
            common.parent()?.to_owned()
        } else {
            common.to_owned()
        };
        let mut linked = Vec::new();
        let worktrees = common.join("worktrees");
        match std::fs::read_dir(&worktrees) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
            Ok(directory) => {
                for entry in directory {
                    let metadata = entry.ok()?.path();
                    // Git passes over a Worktree whose pointer back to its
                    // root is missing or empty.
                    let Ok(pointer) = std::fs::read(metadata.join("gitdir")) else {
                        continue;
                    };
                    let pointer = trim_end(&pointer);
                    if pointer.is_empty() {
                        continue;
                    }
                    let root = path_from_bytes(pointer.strip_suffix(b"/.git").unwrap_or(pointer));
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
            bare: config.bare?,
            revision: None,
        }];
        entries.extend(linked);
        Some(entries)
    }

    fn packed_branches(&self, common: &Path) -> Option<Arc<HashMap<String, String>>> {
        let path = common.join("packed-refs");
        let stamp = match std::fs::metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(Arc::default()),
            Err(_) => return None,
            Ok(metadata) => Stamp::of(&metadata)?,
        };
        if let Some(packed) = self.packed.lock().unwrap().get(common)
            && packed.stamp == stamp
        {
            return Some(packed.branches.clone());
        }
        // Stamped before reading: a file replaced in between is read again
        // next time, since its stamp no longer matches.
        let branches = match std::fs::read(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Some(Arc::default()),
            Err(_) => return None,
            Ok(bytes) => Arc::new(packed_branches(&bytes)?),
        };
        self.packed.lock().unwrap().insert(
            common.to_owned(),
            Packed {
                stamp,
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
    let pointer = std::fs::read(&entry).ok()?;
    let path = path_from_bytes(trim_end(&pointer).strip_prefix(b"gitdir: ")?);
    Some(root.join(path))
}

/// The metadata a linked Worktree shares with its Repository, named by its
/// `commondir` file; a main Worktree's own metadata is the shared one.
fn common_directory(metadata: &Path) -> Option<PathBuf> {
    match regular_file(&metadata.join("commondir"))? {
        None => Some(metadata.to_owned()),
        Some(pointer) => Some(metadata.join(path_from_bytes(trim_end(&pointer)))),
    }
}

/// A loose ref's object id: `Some(None)` where there is no loose ref by that
/// name, so that a packed one may stand; `None` where it cannot be read.
fn loose_ref(common: &Path, name: &str) -> Option<Option<String>> {
    let path = common.join(name);
    // A directory by that name holds other refs, never this one.
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        return Some(None);
    }
    match regular_file(&path)? {
        None => Some(None),
        Some(contents) => object_id(trim_end(&contents)).map(Some),
    }
}

/// The `refs/heads/` entries of a `packed-refs` file by name, or `None` for a
/// file Git itself would refuse.
fn packed_branches(bytes: &[u8]) -> Option<HashMap<String, String>> {
    let mut branches = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        // The object a tag peels to follows its tag.
        if let Some(peeled) = line.strip_prefix(b"^") {
            object_id(peeled)?;
            continue;
        }
        let space = line.iter().position(|byte| *byte == b' ')?;
        let id = object_id(&line[..space])?;
        let name = std::str::from_utf8(&line[space + 1..]).ok()?;
        if name.starts_with("refs/heads/") {
            branches.insert(name.to_owned(), id);
        }
    }
    Some(branches)
}

/// A whole object id in either of Git's hash formats.
fn object_id(bytes: &[u8]) -> Option<String> {
    let hex = bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    let null = bytes.iter().all(|byte| *byte == b'0');
    (hex && !null && matches!(bytes.len(), 40 | 64))
        .then(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Whether `name` is a branch name Git would accept, so a HEAD naming
/// anything else is left to Git to judge.
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with(['/', '.'])
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

/// Whether nothing stands at `path`, or `None` where that cannot be told.
fn absent(path: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Some(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Some(true),
        Err(_) => None,
    }
}

fn trim_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

/// The few settings of a Repository's own configuration that decide how its
/// Worktrees are listed. Reading declines anything that could carry a setting
/// past where a line-by-line reading would see it: an include, a continued
/// line, or a quoted value where one of these settings is concerned.
struct Config {
    bare: Option<bool>,
    work_tree: bool,
    worktree_config: bool,
    file_refs: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            bare: None,
            work_tree: false,
            worktree_config: false,
            file_refs: true,
        }
    }
}
impl Config {
    fn read(&mut self, path: &Path) -> Option<()> {
        let contents = std::fs::read(path).ok()?;
        let contents = std::str::from_utf8(&contents).ok()?;
        // A section with a subsection never holds these settings.
        let mut section: Option<String> = None;
        for line in contents.lines() {
            let mut line = line.trim();
            if line.ends_with('\\') {
                return None;
            }
            if let Some(header) = line.strip_prefix('[') {
                let (header, rest) = header.split_once(']')?;
                let name = header
                    .split(['"', '.', ' ', '\t'])
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if name == "include" || name == "includeif" {
                    return None;
                }
                section = (!header.contains(['"', '.'])).then_some(name);
                // A setting may follow its header on the same line.
                line = rest.trim();
            }
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            let Some(section) = section.as_deref() else {
                continue;
            };
            let (key, value) = match line.split_once('=') {
                Some((key, value)) => (key.trim(), Some(value)),
                None => (line, None),
            };
            match (section, key.to_ascii_lowercase().as_str()) {
                ("core", "bare") => self.bare = Some(boolean(value)?),
                ("core", "worktree") => self.work_tree = true,
                ("extensions", "worktreeconfig") => self.worktree_config = boolean(value)?,
                ("extensions", "refstorage") => {
                    self.file_refs = plain(value?)?.eq_ignore_ascii_case("files")
                }
                _ => {}
            }
        }
        Some(())
    }
}

/// A value without the comment that may follow it, declining a quoted one.
fn plain(value: &str) -> Option<&str> {
    let value = value.split(['#', ';']).next().unwrap_or_default().trim();
    (!value.contains('"')).then_some(value)
}

/// A boolean as Git spells one; a key without a value is true.
fn boolean(value: Option<&str>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    match plain(value)?.to_ascii_lowercase().as_str() {
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
        fn path<'a>(&self, path: &'a Path) -> &'a str {
            path.to_str().unwrap()
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
        async fn git_listing(&self, common: &Path) -> Vec<Entry> {
            let output = self
                .adapter
                .command(common, &["worktree", "list", "--porcelain", "-z"])
                .await
                .unwrap();
            assert!(output.status.success());
            parse_worktrees(&output.stdout)
        }
        /// Asserts the disk names the same Worktrees Git lists, in the same
        /// roles; a listing only names them, so it reads no revision.
        async fn agreed_listing(&self, on_disk: &OnDisk, common: &Path) {
            let read = on_disk
                .worktrees(common)
                .unwrap_or_else(|| panic!("{} was not listed from disk", common.display()));
            let listed = self.git_listing(common).await;
            let named = |entries: &[Entry]| {
                let mut named = entries
                    .iter()
                    .map(|entry| (canonical_checkout_path(&entry.root), entry.bare))
                    .collect::<Vec<_>>();
                named[1..].sort();
                named
            };
            assert_eq!(named(&read), named(&listed), "{}", common.display());
            assert!(read.iter().all(|entry| entry.revision.is_none()));
        }
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
        let topic = fixture.root.join("topic");
        fixture.git(
            &main,
            &["worktree", "add", "-b", "topic", fixture.path(&topic)],
        );
        assert!(matches!(
            fixture.agreed_revision(&on_disk, &topic).await,
            CheckoutRevision::Branch { name, commit: Some(_) } if name == "topic"
        ));
        let detached = fixture.root.join("detached");
        fixture.git(
            &main,
            &["worktree", "add", "--detach", fixture.path(&detached)],
        );
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
                fixture.path(&relative),
            ],
        ) {
            fixture.agreed_revision(&on_disk, &relative).await;
        }
        let separate = fixture.root.join("separate");
        let metadata = fixture.root.join("separate-metadata");
        fixture.git(
            &fixture.root,
            &[
                "init",
                "-b",
                "main",
                "--separate-git-dir",
                fixture.path(&metadata),
                fixture.path(&separate),
            ],
        );
        fixture.commit(&separate);
        fixture.agreed_revision(&on_disk, &separate).await;
        let sha256 = fixture.root.join("sha256");
        if fixture.supports(
            &fixture.root,
            &[
                "init",
                "-b",
                "main",
                "--object-format=sha256",
                fixture.path(&sha256),
            ],
        ) {
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
            b"not a revision\n",
            b"0000000000000000000000000000000000000000\n",
        ] {
            std::fs::write(metadata.join("HEAD"), unreadable).unwrap();
            assert_eq!(
                on_disk.revision(&main),
                None,
                "{}",
                String::from_utf8_lossy(unreadable)
            );
        }
        std::fs::remove_file(metadata.join("HEAD")).unwrap();
        assert_eq!(fixture.declined_revision(&on_disk, &main).await, None);
        std::fs::write(metadata.join("HEAD"), &head).unwrap();
        let reference = metadata.join("refs").join("heads").join("main");
        let tip = std::fs::read_to_string(&reference).unwrap();
        std::fs::write(&reference, format!("{} trailing\n", tip.trim_end())).unwrap();
        fixture.declined_revision(&on_disk, &main).await;
        std::fs::write(&reference, tip).unwrap();
        fixture.agreed_revision(&on_disk, &main).await;
        // Refs kept in a reftable are not files at all.
        let reftable = fixture.root.join("reftable");
        if fixture.supports(
            &fixture.root,
            &[
                "init",
                "-b",
                "main",
                "--ref-format=reftable",
                fixture.path(&reftable),
            ],
        ) {
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
    async fn lists_the_worktrees_git_lists() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        let common = main.join(".git");
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.commit(&main);
        let topic = fixture.root.join("topic");
        fixture.git(
            &main,
            &["worktree", "add", "-b", "topic", fixture.path(&topic)],
        );
        let detached = fixture.root.join("detached");
        fixture.git(
            &main,
            &["worktree", "add", "--detach", fixture.path(&detached)],
        );
        let locked = fixture.root.join("locked");
        fixture.git(
            &main,
            &["worktree", "add", "-b", "locked", fixture.path(&locked)],
        );
        fixture.git(&main, &["worktree", "lock", fixture.path(&locked)]);
        let relative = fixture.root.join("relative");
        fixture.supports(
            &main,
            &[
                "worktree",
                "add",
                "--relative-paths",
                "-b",
                "relative",
                fixture.path(&relative),
            ],
        );
        fixture.agreed_listing(&on_disk, &common).await;
        // A Worktree whose directory has gone is still listed until pruned.
        std::fs::remove_dir_all(&topic).unwrap();
        fixture.agreed_listing(&on_disk, &common).await;
        fixture.git(&main, &["worktree", "prune"]);
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
                fixture.path(&main),
                fixture.path(&bare),
            ],
        );
        fixture.agreed_listing(&on_disk, &bare).await;
        let from_bare = fixture.root.join("from-bare");
        fixture.git(
            &bare,
            &[
                "worktree",
                "add",
                "-b",
                "from-bare",
                fixture.path(&from_bare),
            ],
        );
        fixture.agreed_listing(&on_disk, &bare).await;
    }

    #[tokio::test]
    async fn declines_to_list_where_configuration_could_say_more_than_it_reads() {
        let fixture = Fixture::new();
        let on_disk = OnDisk::default();
        let main = fixture.repository("main");
        let common = main.join(".git");
        let config = std::fs::read_to_string(common.join("config")).unwrap();
        assert!(on_disk.worktrees(&common).is_some());
        for (case, addition) in [
            ("an included file", "[include]\n\tpath = elsewhere\n"),
            (
                "a conditional include",
                "[includeIf \"gitdir:/\"]\n\tpath = elsewhere\n",
            ),
            ("a separate work tree", "[core]\n\tworktree = /elsewhere\n"),
            ("a continued line", "[user]\n\tname = a \\\n\tbare = true\n"),
            ("reftable refs", "[extensions]\n\trefStorage = reftable\n"),
            ("a quoted bare", "[core]\n\tbare = \"false\"\n"),
        ] {
            std::fs::write(common.join("config"), format!("{config}{addition}")).unwrap();
            assert!(on_disk.worktrees(&common).is_none(), "{case}");
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
        std::fs::remove_file(common.join("config")).unwrap();
        assert!(on_disk.worktrees(&common).is_none());
    }
}
