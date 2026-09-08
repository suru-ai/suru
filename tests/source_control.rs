//! Portable real Git layouts exercise the production source control adapter.
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use suru::{
    protocol::*,
    source_control::{GitSourceControl, SourceControl},
};

fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Suru Test")
        .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
        .env("GIT_COMMITTER_NAME", "Suru Test")
        .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn root() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    (temporary, root)
}
fn init(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
}
fn commit(root: &Path) {
    git(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}
fn linked(main: &Path, path: &Path, branch: &str) {
    git(
        main,
        &["worktree", "add", "-b", branch, path.to_str().unwrap()],
    );
}

#[tokio::test]
async fn shared_metadata_groups_external_worktrees_and_subdirs_but_not_clones_or_nested_repositories()
 {
    let (_temporary, root) = root();
    let main = root.join("main with spaces λ");
    init(&main);
    commit(&main);
    git(
        &main,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/shared.git",
        ],
    );
    let external = root.join("elsewhere linked λ");
    linked(&main, &external, "feature");
    let subdir = external.join("deep").join("inside");
    std::fs::create_dir_all(&subdir).unwrap();
    let adapter = GitSourceControl::default();
    let a = adapter.discover(&main).await;
    let b = adapter.discover(&subdir).await;
    assert_eq!(a.workspace.id, b.workspace.id);
    assert_eq!(b.workspace.path, main);
    assert_eq!(b.execution_directory.as_ref().unwrap().path, subdir);
    assert_eq!(
        adapter.reuse_discovery(&subdir, &b).unwrap().checkout,
        b.checkout
    );
    assert_eq!(b.checkout.as_ref().unwrap().root, external);
    assert_eq!(b.checkout.as_ref().unwrap().kind, CheckoutKind::Linked);
    assert_eq!(a.checkouts.len(), 2);
    assert!(
        matches!(a.checkouts[0].revision, Some(CheckoutRevision::Branch { ref name, commit: Some(_) }) if name == "main")
    );
    assert!(matches!(
        a.workspace.repository.unwrap().capabilities.create_checkout,
        SourceControlCapability::Available
    ));
    let clone = root.join("clone");
    git(
        &root,
        &["clone", main.to_str().unwrap(), clone.to_str().unwrap()],
    );
    git(
        &clone,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/shared.git",
        ],
    );
    assert_ne!(adapter.discover(&clone).await.workspace.id, b.workspace.id);
    let nested = external.join("nested");
    init(&nested);
    assert_ne!(adapter.discover(&nested).await.workspace.id, b.workspace.id);
    assert!(adapter.reuse_discovery(&nested, &b).is_none());
    let child = nested.join("child");
    std::fs::create_dir(&child).unwrap();
    assert_eq!(
        adapter.discover(&child).await.workspace.id,
        adapter.discover(&nested).await.workspace.id
    );
    // Submodule .git-file layout must choose the nested shared metadata too.
    git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            clone.to_str().unwrap(),
            "module",
        ],
    );
    let module = adapter.discover(&main.join("module")).await;
    assert_ne!(module.workspace.id, b.workspace.id);
    assert_eq!(module.checkout.unwrap().root, main.join("module"));
}

#[tokio::test]
async fn unborn_and_detached_checkouts_are_independent_from_checkout_kind() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    let adapter = GitSourceControl::default();
    let unborn = adapter.discover(&main).await;
    assert_eq!(unborn.checkouts.len(), 1);
    assert_eq!(unborn.checkouts[0].association.kind, CheckoutKind::Main);
    assert!(
        matches!(unborn.checkouts[0].revision, Some(CheckoutRevision::Branch { ref name, commit: None }) if name == "main")
    );
    commit(&main);
    let detached = root.join("detached");
    git(
        &main,
        &["worktree", "add", "--detach", detached.to_str().unwrap()],
    );
    let result = adapter.discover(&detached).await;
    assert_eq!(result.checkout.unwrap().kind, CheckoutKind::Linked);
    assert!(
        result
            .checkouts
            .iter()
            .any(|checkout| checkout.association.root == detached
                && matches!(checkout.revision, Some(CheckoutRevision::Detached { .. })))
    );
}

#[tokio::test]
async fn bare_and_separate_metadata_never_masquerade_as_main_working_copies() {
    let (_temporary, root) = root();
    let adapter = GitSourceControl::default();
    let bare = root.join("bare.git");
    git(
        &root,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let bare_context = adapter.discover(&bare).await;
    assert!(bare_context.execution_directory.is_none());
    assert!(bare_context.checkouts.is_empty());
    assert!(matches!(
        bare_context.workspace.repository.as_ref().unwrap().location,
        RepositoryLocation::Bare { .. }
    ));
    let seed = root.join("seed");
    init(&seed);
    commit(&seed);
    git(&seed, &["push", bare.to_str().unwrap(), "main"]);
    let linked_root = root.join("bare working copy");
    git(
        &bare,
        &["worktree", "add", linked_root.to_str().unwrap(), "main"],
    );
    let checkout = adapter.discover(&linked_root).await;
    assert_eq!(checkout.workspace.id, bare_context.workspace.id);
    assert_eq!(checkout.checkout.unwrap().kind, CheckoutKind::Linked);
    assert_eq!(checkout.execution_directory.unwrap().path, linked_root);
    let main = root.join("separate main");
    let metadata = root.join("metadata");
    git(
        &root,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            main.to_str().unwrap(),
        ],
    );
    commit(&main);
    let linked_root = root.join("separate linked");
    linked(&main, &linked_root, "topic");
    let unknown = adapter.discover(&linked_root).await;
    assert!(unknown.workspace.main_unknown());
    assert_eq!(unknown.workspace.path, metadata);
    assert!(
        unknown
            .checkouts
            .iter()
            .all(|checkout| checkout.association.root != metadata)
    );
    let known = adapter.discover(&main).await;
    assert_eq!(known.workspace.id, unknown.workspace.id);
    assert_eq!(known.workspace.path, main);
    assert_eq!(known.checkouts.len(), 2);
    assert!(
        adapter
            .discover(&metadata)
            .await
            .execution_directory
            .is_none()
    );
}

#[tokio::test]
async fn deleted_worktree_replaced_by_plain_directory_is_unavailable_and_git_absence_is_distinct() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "topic");
    std::fs::remove_dir_all(&linked_root).unwrap();
    std::fs::create_dir(&linked_root).unwrap();
    let adapter = GitSourceControl::default();
    let discovered = adapter.discover(&main).await;
    let linked = discovered
        .checkouts
        .iter()
        .find(|checkout| checkout.association.root == linked_root)
        .unwrap();
    assert!(matches!(
        linked.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
    assert_eq!(
        linked.association.recovery_revision, None,
        "unavailable discovery must not promote retained Git listing data to recovery facts"
    );
    assert_eq!(
        discovered.workspace.source_control,
        SourceControlAvailability::Available
    );
    assert_eq!(
        adapter.discover(&root).await.workspace.source_control,
        SourceControlAvailability::NotDetected
    );
    let missing = GitSourceControl::new(root.join("missing-git-executable"))
        .discover(&main)
        .await;
    assert!(
        matches!(missing.workspace.source_control, SourceControlAvailability::Unavailable { ref reason } if reason.contains("not installed"))
    );
    assert!(missing.execution_directory.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_working_copy_canonicalizes_shared_identity() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    let link = root.join("alias");
    std::os::unix::fs::symlink(&main, &link).unwrap();
    let adapter = GitSourceControl::default();
    assert_eq!(
        adapter.discover(&main).await.workspace.id,
        adapter.discover(&link).await.workspace.id
    );
}

#[tokio::test]
async fn checkout_observation_tracks_unborn_branch_detachment_missing_and_unreadable() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    let adapter = GitSourceControl::default();
    let checkout = adapter.discover(&main).await.checkout.unwrap();
    assert_eq!(
        adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Branch {
            name: "main".into(),
            commit: None
        })
    );
    commit(&main);
    git(&main, &["checkout", "-b", "external"]);
    assert!(matches!(adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Branch { name, commit: Some(_) }) if name == "external"));
    git(&main, &["checkout", "--detach"]);
    assert!(matches!(
        adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Detached { .. })
    ));
    std::fs::rename(&main, root.join("moved")).unwrap();
    let missing = adapter.observe(&checkout).await;
    assert!(matches!(
        missing.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
    assert_eq!(missing.revision, None);
    init(&main);
    // Existing directories with unreadable Git metadata remain explicitly unavailable.
    std::fs::remove_file(main.join(".git").join("HEAD")).unwrap();
    assert!(matches!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
}
