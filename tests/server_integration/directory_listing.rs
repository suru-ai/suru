//! The Directory Browser reads the Outlook's Server's directories one at a
//! time through that Server's listing route, and a Client turned toward a
//! Remote is answered by the Remote exactly as its own Server answers it.
use super::*;
use std::path::{Path, PathBuf};
use suru::{
    managed_client::OutlookClient,
    protocol::{DirectoryListing, ListDirectoryRequest},
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

/// The root of the filesystem the fixtures live on: `/` on Unix, and on
/// Windows the drive root the temporary directory stands under.
#[tokio::test]
async fn the_filesystem_root_answers_with_no_parent() {
    let pair = paired_servers("directory-listing-filesystem-root").await;
    let fixture = tempfile::tempdir().expect("create a directory on the filesystem");
    let filesystem_root = canonical(fixture.path())
        .ancestors()
        .last()
        .expect("a path has a root")
        .to_owned();
    if cfg!(windows) {
        assert!(
            filesystem_root.to_string_lossy().ends_with(":\\"),
            "a drive root, not {}",
            filesystem_root.display()
        );
    } else {
        assert_eq!(filesystem_root, Path::new("/"));
    }

    for (way, client) in both_ways(&pair) {
        let listing = listed(&client, &filesystem_root, None).await;
        assert_eq!(listing.root, filesystem_root, "{way}");
        assert_eq!(listing.parent, None, "{way}");
    }
    pair.shutdown().await;
}
