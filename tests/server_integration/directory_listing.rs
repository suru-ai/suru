//! The Directory Browser reads the Outlook's Server's directories one at a
//! time through that Server's listing route, and a Client turned toward a
//! Remote is answered by the Remote exactly as its own Server answers it.
use super::*;
use std::path::{Path, PathBuf};
use suru::{
    managed_client::OutlookClient,
    protocol::{
        CheckoutRevision, DRIVE_LIST, DirectoryListing, DirectorySourceControl,
        ListDirectoryRequest,
    },
};

/// The Serving Server asked as a Client's own Outlook, and the same Server
/// asked through a Pairing as the connecting side's Remote.
fn both_ways(pair: &PairedServers) -> [(&'static str, OutlookClient); 2] {
    [
        ("locally", pair.serving_client.outlook(Outlook::Local)),
        (
            "through a Remote",
            pair.connecting_client
                .outlook(Outlook::Remote("workstation".to_owned())),
        ),
    ]
}

async fn listed(
    client: &OutlookClient,
    path: impl Into<PathBuf>,
    base: Option<&Path>,
) -> DirectoryListing {
    let path = path.into();
    client
        .list_directory(ListDirectoryRequest {
            path: path.clone(),
            base: base.map(Path::to_owned),
        })
        .await
        .unwrap_or_else(|error| panic!("list {}: {error:#}", path.display()))
}

async fn refused(client: &OutlookClient, path: impl Into<PathBuf>) -> SessionError {
    let path = path.into();
    let error = client
        .list_directory(ListDirectoryRequest {
            path: path.clone(),
            base: None,
        })
        .await
        .expect_err("the listing is refused");
    error
        .downcast_ref::<SessionError>()
        .unwrap_or_else(|| panic!("a typed refusal for {}: {error:#}", path.display()))
        .clone()
}

fn names(listing: &DirectoryListing) -> Vec<&str> {
    listing
        .children
        .iter()
        .map(|child| child.name.as_str())
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).expect("read the fixture's canonical path")
}

/// Runs Git at `directory` as a fixture's author, answering what it printed.
fn git(directory: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Suru Test")
        .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
        .env("GIT_COMMITTER_NAME", "Suru Test")
        .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git prints Unicode here")
        .trim_end()
        .to_owned()
}

/// A Repository made at `directory` on `main`, with one commit, answering
/// that commit.
fn committed_repository(directory: &Path) -> String {
    std::fs::create_dir(directory).expect("create the Repository's root");
    git(directory, &["init", "-b", "main"]);
    git(
        directory,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    git(directory, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn a_root_lists_its_parent_and_child_directories_in_natural_order_without_files() {
    let pair = paired_servers("directory-listing-children").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    // Byte order would put `Beta` and `Zeta` first and `a10` before `a2`.
    for name in ["Zeta", "a10", "alpha", "a2", "Beta", "a1"] {
        std::fs::create_dir(fixture.path().join(name)).expect("create a child directory");
    }
    std::fs::create_dir(fixture.path().join("a1").join("grandchild"))
        .expect("create a grandchild directory");
    for name in ["a3.txt", "README"] {
        std::fs::write(fixture.path().join(name), "a file").expect("create a file");
    }
    let root = canonical(fixture.path());

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, fixture.path(), None).await;

        assert_eq!(listing.root, root, "{way}");
        assert_eq!(listing.parent.as_deref(), root.parent(), "{way}");
        assert_eq!(
            names(&listing),
            ["a1", "a2", "a10", "alpha", "Beta", "Zeta"],
            "{way}: only the root's own directories, case set aside and numbers read whole"
        );
        for child in &listing.children {
            assert_eq!(child.path, root.join(&child.name), "{way}");
        }
    }
    pair.shutdown().await;
}

#[tokio::test]
async fn a_relative_path_is_read_from_the_base_and_a_tilde_from_the_servers_home() {
    let home = tempfile::tempdir().expect("create the Serving Server's home");
    std::fs::create_dir_all(home.path().join("projects").join("suru"))
        .expect("create a directory in the home");
    let pair = paired_servers_at_home("directory-listing-relative", home.path()).await;
    let workspace = tempfile::tempdir().expect("create the Landing's Workspace");
    for name in ["left", "right"] {
        std::fs::create_dir(workspace.path().join(name)).expect("create a sibling");
    }
    std::fs::create_dir(workspace.path().join("right").join("inner"))
        .expect("create a directory in the sibling");
    let base = workspace.path().join("left");

    for (way, client) in both_ways(&pair) {
        let sibling = listed(&client, Path::new("..").join("right"), Some(&base)).await;
        assert_eq!(
            sibling.root,
            canonical(&workspace.path().join("right")),
            "{way}"
        );
        assert_eq!(names(&sibling), ["inner"], "{way}");

        let current = listed(&client, ".", None).await;
        assert_eq!(
            current.root,
            canonical(&std::env::current_dir().expect("read the current directory")),
            "{way}: with no base a relative path is read from the Server's current directory"
        );

        let at_home = listed(&client, "~", Some(&base)).await;
        assert_eq!(at_home.root, canonical(home.path()), "{way}");
        assert_eq!(names(&at_home), ["projects"], "{way}");

        let beneath_home = listed(&client, Path::new("~").join("projects"), Some(&base)).await;
        assert_eq!(
            beneath_home.root,
            canonical(&home.path().join("projects")),
            "{way}"
        );
        assert_eq!(names(&beneath_home), ["suru"], "{way}");
    }
    pair.shutdown().await;
}

#[tokio::test]
async fn a_missing_path_and_a_file_are_refused_with_a_reason() {
    let pair = paired_servers("directory-listing-refusals").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    let file = fixture.path().join("notes.txt");
    std::fs::write(&file, "a file").expect("create a file");

    for (way, client) in both_ways(&pair) {
        let missing = refused(&client, fixture.path().join("missing")).await;
        assert_eq!(missing.code, SessionErrorCode::InvalidWorkspace, "{way}");
        assert_eq!(missing.message, "No directory there", "{way}");

        let beneath_missing = refused(&client, fixture.path().join("missing").join("deeper")).await;
        assert_eq!(beneath_missing.message, "No directory there", "{way}");

        let a_file = refused(&client, &file).await;
        assert_eq!(a_file.code, SessionErrorCode::InvalidWorkspace, "{way}");
        assert_eq!(a_file.message, "Not a directory", "{way}");

        let beneath_a_file = refused(&client, file.join("deeper")).await;
        assert_eq!(beneath_a_file.message, "No directory there", "{way}");
    }
    pair.shutdown().await;
}

/// A hidden directory is still listed, in its place among the others, and
/// flagged hidden so the Directory Browser can leave it out itself without
/// asking again: on every platform a dot-named one is hidden, and a name
/// merely holding a dot is not.
#[tokio::test]
async fn a_dot_named_directory_is_listed_and_flagged_hidden() {
    let pair = paired_servers("directory-listing-hidden").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    for name in [".config", "dotted.name", "plain"] {
        std::fs::create_dir(fixture.path().join(name)).expect("create a child directory");
    }

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, fixture.path(), None).await;
        assert_eq!(
            hidden(&listing),
            [(".config", true), ("dotted.name", false), ("plain", false)],
            "{way}"
        );
    }
    pair.shutdown().await;
}

/// Windows marks a directory hidden with an attribute rather than a name, and
/// a directory carrying it is flagged hidden as a dot-named one is.
#[cfg(windows)]
#[tokio::test]
async fn a_directory_carrying_the_windows_hidden_attribute_is_flagged_hidden() {
    let pair = paired_servers("directory-listing-hidden-attribute").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    for name in ["marked", "unmarked"] {
        std::fs::create_dir(fixture.path().join(name)).expect("create a child directory");
    }
    mark_hidden(&fixture.path().join("marked"));

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, fixture.path(), None).await;
        assert_eq!(
            hidden(&listing),
            [("marked", true), ("unmarked", false)],
            "{way}"
        );
    }
    pair.shutdown().await;
}

fn hidden(listing: &DirectoryListing) -> Vec<(&str, bool)> {
    listing
        .children
        .iter()
        .map(|child| (child.name.as_str(), child.hidden))
        .collect()
}

/// Sets the platform's hidden attribute on `path`, as Explorer's Hidden box
/// or `attrib +h` does.
#[cfg(windows)]
fn mark_hidden(path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let marked = unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
    assert_ne!(
        marked,
        0,
        "mark {} hidden: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// A directory whose permissions deny reading its entries. Windows denies
/// reading through an access control list rather than a mode, so this is
/// pinned where a mode can say it.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_directory_is_refused_with_a_reason() {
    use std::os::unix::fs::PermissionsExt as _;

    let pair = paired_servers("directory-listing-unreadable").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    let sealed = fixture.path().join("sealed");
    std::fs::create_dir(&sealed).expect("create the directory to seal");
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000))
        .expect("seal the directory");
    // A superuser reads past any mode, so there is nothing to refuse.
    let sealed_for_this_user = std::fs::read_dir(&sealed).is_err();

    if sealed_for_this_user {
        for (way, client) in both_ways(&pair) {
            let unreadable = refused(&client, &sealed).await;
            assert_eq!(unreadable.code, SessionErrorCode::InvalidWorkspace, "{way}");
            assert!(
                unreadable
                    .message
                    .starts_with("Could not read this directory: "),
                "{way}: {}",
                unreadable.message
            );
        }
    }
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755))
        .expect("unseal the directory so it can be removed");
    pair.shutdown().await;
}

/// Symlinked directories need no special handling: one is listed as the
/// directory it names, and one naming a file or nothing is not listed.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_to_a_directory_is_listed_and_one_to_anything_else_is_not() {
    let pair = paired_servers("directory-listing-symlinks").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    std::fs::create_dir(fixture.path().join("real")).expect("create a directory");
    std::fs::write(fixture.path().join("file"), "a file").expect("create a file");
    for (target, link) in [
        ("real", "linked"),
        ("file", "linked-file"),
        ("missing", "dangling"),
    ] {
        std::os::unix::fs::symlink(fixture.path().join(target), fixture.path().join(link))
            .expect("create a symlink");
    }

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, fixture.path(), None).await;
        assert_eq!(names(&listing), ["linked", "real"], "{way}");
    }
    pair.shutdown().await;
}

/// Every path crosses the wire as Unicode text, so a directory whose name is
/// not Unicode can be neither named nor chosen: it is left out of its
/// parent's children without costing its siblings their listing, and a
/// symlink leading into it is refused as a root with the reason. Windows
/// names are UTF-16 and cannot be spelled this way, and macOS refuses such a
/// name outright, so this is pinned wherever a filesystem keeps one.
#[cfg(unix)]
#[tokio::test]
async fn a_directory_whose_name_is_not_unicode_is_left_out_and_refused_as_a_root() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

    let fixture = tempfile::tempdir().expect("create the directory to list");
    let unnameable = fixture.path().join(OsStr::from_bytes(b"caf\xe9"));
    match std::fs::create_dir(&unnameable) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => return,
        Err(error) => panic!("create a directory whose name is not Unicode: {error}"),
    }
    std::fs::create_dir(fixture.path().join("plain")).expect("create a sibling");
    std::os::unix::fs::symlink(&unnameable, fixture.path().join("aliased"))
        .expect("create a symlink into the directory");
    let pair = paired_servers("directory-listing-not-unicode").await;

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, fixture.path(), None).await;
        assert_eq!(names(&listing), ["aliased", "plain"], "{way}");

        let aliased = refused(&client, fixture.path().join("aliased")).await;
        assert_eq!(aliased.code, SessionErrorCode::InvalidWorkspace, "{way}");
        assert_eq!(
            aliased.message, "This directory's path is not Unicode",
            "{way}"
        );
    }
    pair.shutdown().await;
}

/// The root of the filesystem the fixtures live on.
fn filesystem_root(fixture: &Path) -> PathBuf {
    canonical(fixture)
        .ancestors()
        .last()
        .expect("a path has a root")
        .to_owned()
}

/// Off Windows there are no drives, so the filesystem's root has no parent
/// and no drive list is offered: the path the drive list goes by is read as
/// any empty relative path is, naming the base.
#[cfg(not(windows))]
#[tokio::test]
async fn the_filesystem_root_answers_with_no_parent() {
    let pair = paired_servers("directory-listing-filesystem-root").await;
    let fixture = tempfile::tempdir().expect("create a directory on the filesystem");
    let filesystem_root = filesystem_root(fixture.path());
    assert_eq!(filesystem_root, Path::new("/"));

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, &filesystem_root, None).await;
        assert_eq!(listing.root, filesystem_root, "{way}");
        assert_eq!(listing.parent, None, "{way}");

        let base = listed(&client, DRIVE_LIST, Some(fixture.path())).await;
        assert_eq!(base.root, canonical(fixture.path()), "{way}");
    }
    pair.shutdown().await;
}

/// On Windows a drive root's parent is the drive list: asked for, it answers
/// with no parent of its own and a child for each drive the Server can see,
/// named and spelled as that drive's root, among them the drive the fixtures
/// live on and the system's own.
#[cfg(windows)]
#[tokio::test]
async fn a_drive_root_names_the_drive_list_as_its_parent_which_lists_every_drive() {
    use suru::protocol::ChildDirectory;

    let pair = paired_servers("directory-listing-drive-list").await;
    let fixture = tempfile::tempdir().expect("create a directory on a drive");
    let drive_root = filesystem_root(fixture.path());
    assert!(
        drive_root.to_string_lossy().ends_with(":\\"),
        "a drive root, not {}",
        drive_root.display()
    );
    let system_drive = std::env::var("SystemDrive").expect("Windows names its system drive");
    let drive = |root: String| ChildDirectory {
        name: root.clone(),
        path: PathBuf::from(root),
        source_control: DirectorySourceControl::Plain,
        hidden: false,
    };

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, &drive_root, None).await;
        assert_eq!(listing.root, drive_root, "{way}");
        assert_eq!(
            listing.parent.as_deref(),
            Some(Path::new(DRIVE_LIST)),
            "{way}: a drive's parent is the drive list"
        );

        let drives = listed(&client, DRIVE_LIST, Some(fixture.path())).await;
        assert_eq!(drives.root, Path::new(DRIVE_LIST), "{way}");
        assert_eq!(
            drives.parent, None,
            "{way}: nothing stands above the drive list"
        );
        assert_eq!(
            drives.source_control,
            DirectorySourceControl::Plain,
            "{way}"
        );
        for root in [
            drive_root.to_string_lossy().into_owned(),
            format!("{system_drive}\\"),
        ] {
            assert!(
                drives.children.contains(&drive(root.clone())),
                "{way}: {root} is among {:?}",
                drives.children
            );
        }
        assert!(
            drives
                .children
                .windows(2)
                .all(|adjacent| adjacent[0].name < adjacent[1].name),
            "{way}: the drives come in the order of their letters: {:?}",
            drives.children
        );
    }
    pair.shutdown().await;
}

/// What each child is to source control is read from Git's own metadata on
/// the Server's disk: a Repository's main root and a linked Worktree's root
/// carry the Checkout State their HEAD stands on — a branch, named alone
/// since HEAD is all a listing reads, or a detached commit — or nothing
/// where that HEAD cannot be read there; a bare Repository is told apart
/// from both, and a directory outside source control is plain.
#[tokio::test]
async fn each_child_says_what_it_is_to_source_control() {
    let pair = paired_servers("directory-listing-source-control").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    let root = canonical(fixture.path());
    let layout = SourceControlLayout::at(&root);

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, &root, None).await;
        let source_control = |name: &str| {
            listing
                .children
                .iter()
                .find(|child| child.name == name)
                .unwrap_or_else(|| panic!("{way}: {name} is listed"))
                .source_control
                .clone()
        };

        assert_eq!(
            listing.source_control,
            DirectorySourceControl::Plain,
            "{way}"
        );
        for (name, expected) in layout.expected() {
            assert_eq!(source_control(name), expected, "{way}: {name}");
        }
    }
    pair.shutdown().await;
}

/// The root is read as each child is, so a Directory Browser opened on a
/// Worktree's root or a bare Repository says so of its first row.
#[tokio::test]
async fn the_root_says_what_it_is_to_source_control() {
    let pair = paired_servers("directory-listing-root-source-control").await;
    let fixture = tempfile::tempdir().expect("create the directories to list");
    let root = canonical(fixture.path());
    let layout = SourceControlLayout::at(&root);

    for (way, client) in both_ways(&pair) {
        for (name, expected) in layout.expected() {
            let listing = listed(&client, root.join(name), None).await;
            assert_eq!(listing.source_control, expected, "{way}: {name}");
        }
    }
    pair.shutdown().await;
}

/// Several Repositories side by side, one with several linked Worktrees
/// beside it and its branches packed away so no loose ref names them, are
/// each read for themselves: HEAD alone names the branch each stands on.
#[tokio::test]
async fn sibling_repositories_and_worktrees_are_each_read_for_themselves() {
    let pair = paired_servers("directory-listing-siblings").await;
    let fixture = tempfile::tempdir().expect("create the directory to list");
    let root = canonical(fixture.path());
    committed_repository(&root.join("alpha"));
    for branch in ["one", "two", "three"] {
        git(
            &root.join("alpha"),
            &[
                "worktree",
                "add",
                "-b",
                branch,
                root.join(format!("alpha-{branch}"))
                    .to_str()
                    .expect("a Unicode fixture path"),
            ],
        );
    }
    git(&root.join("alpha"), &["pack-refs", "--all"]);
    committed_repository(&root.join("beta"));
    git(&root.join("beta"), &["checkout", "-b", "feature"]);
    committed_repository(&root.join("gamma"));

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, &root, None).await;
        let read = listing
            .children
            .iter()
            .map(|child| (child.name.as_str(), child.source_control.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            read,
            [
                (
                    "alpha",
                    DirectorySourceControl::RepositoryRoot {
                        revision: on("main")
                    }
                ),
                (
                    "alpha-one",
                    DirectorySourceControl::LinkedWorktreeRoot {
                        revision: on("one")
                    }
                ),
                (
                    "alpha-three",
                    DirectorySourceControl::LinkedWorktreeRoot {
                        revision: on("three")
                    }
                ),
                (
                    "alpha-two",
                    DirectorySourceControl::LinkedWorktreeRoot {
                        revision: on("two")
                    }
                ),
                (
                    "beta",
                    DirectorySourceControl::RepositoryRoot {
                        revision: on("feature")
                    }
                ),
                (
                    "gamma",
                    DirectorySourceControl::RepositoryRoot {
                        revision: on("main")
                    }
                ),
            ],
            "{way}"
        );
    }
    pair.shutdown().await;
}

/// A branch as a listing names it: from HEAD alone, without its commit.
fn on(branch: &str) -> Option<CheckoutRevision> {
    Some(CheckoutRevision::Branch {
        name: branch.to_owned(),
        commit: None,
    })
}

/// One directory of every kind a listing tells apart, made with Git beneath
/// a root, and what each is expected to read as.
struct SourceControlLayout {
    detached_commit: String,
}

impl SourceControlLayout {
    fn at(root: &Path) -> Self {
        committed_repository(&root.join("main"));
        git(
            &root.join("main"),
            &[
                "worktree",
                "add",
                "-b",
                "topic",
                root.join("linked")
                    .to_str()
                    .expect("a Unicode fixture path"),
            ],
        );
        let detached_commit = committed_repository(&root.join("detached"));
        git(&root.join("detached"), &["checkout", "--detach"]);
        committed_repository(&root.join("unreadable"));
        std::fs::write(
            root.join("unreadable").join(".git").join("HEAD"),
            "neither a ref nor a commit\n",
        )
        .expect("spoil the Repository's HEAD");
        git(root, &["init", "--bare", "-b", "main", "bare.git"]);
        std::fs::create_dir(root.join("plain")).expect("create a plain directory");
        Self { detached_commit }
    }

    fn expected(&self) -> [(&'static str, DirectorySourceControl); 6] {
        [
            (
                "main",
                DirectorySourceControl::RepositoryRoot {
                    revision: on("main"),
                },
            ),
            (
                "linked",
                DirectorySourceControl::LinkedWorktreeRoot {
                    revision: on("topic"),
                },
            ),
            (
                "detached",
                DirectorySourceControl::RepositoryRoot {
                    revision: Some(CheckoutRevision::Detached {
                        commit: self.detached_commit.clone(),
                    }),
                },
            ),
            (
                "unreadable",
                DirectorySourceControl::RepositoryRoot { revision: None },
            ),
            ("bare.git", DirectorySourceControl::BareRepository),
            ("plain", DirectorySourceControl::Plain),
        ]
    }
}
