//! Portable real Git layouts exercise the production source control adapter.
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use suru::{
    protocol::*,
    source_control::{CloneFailure, GitSourceControl, SourceControl},
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
    let root = suru::paths::canonical(temporary.path()).unwrap();
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
async fn reclaim_accepts_a_preparation_lock_only_with_git_owned_marker_and_ref_evidence() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let adapter = GitSourceControl::default();
    let source = adapter.discover(&main).await;
    let plan = adapter
        .plan_checkout(Default::default(), &source, "owned-lock", &[])
        .await
        .unwrap();
    adapter.prepare_checkout(&plan).await.unwrap();
    let target = removal_target(&adapter, &plan.destination.path).await;
    let pointer = std::fs::read_to_string(plan.destination.path.join(".git")).unwrap();
    let metadata = PathBuf::from(
        pointer
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("gitdir: ")
            .unwrap(),
    );
    let reason = std::fs::read_to_string(metadata.join("suru-preparation")).unwrap();
    git(
        &main,
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            plan.destination.path.to_str().unwrap(),
        ],
    );
    let inspection = adapter.inspect_removal(&target).await.unwrap();
    assert_eq!(inspection.lock.as_deref(), Some(reason.as_str()));

    assert_eq!(
        adapter
            .reclaim_checkout(&target, &inspection, CheckoutBranchOutcome::Retained, &[],)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained,
        "retired intents need not survive when Git's two ownership records agree"
    );
    assert!(!plan.destination.path.exists());
}

fn read_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn managed_branch_records_the_planned_source_commit_and_branch_when_claimed() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let adapter = GitSourceControl::default();
    let source = adapter.discover(&main).await;
    let plan = adapter
        .plan_checkout(Default::default(), &source, "remember-provenance", &[])
        .await
        .unwrap();
    let CheckoutPreparationPlan::Git {
        branch,
        source_commit,
        source_branch,
    } = &plan.plan;
    assert_eq!(source_branch.as_deref(), Some("main"));

    // Claim uses the source identity captured by planning even if that checkout
    // has since moved to another branch at the same commit.
    git(&main, &["checkout", "-b", "other"]);
    adapter.prepare_checkout(&plan).await.unwrap();

    assert_eq!(
        read_git(
            &main,
            &["config", "--get", &format!("branch.{branch}.suru-base")],
        ),
        *source_commit
    );
    assert_eq!(
        read_git(
            &main,
            &[
                "config",
                "--get",
                &format!("branch.{branch}.suru-base-branch"),
            ],
        ),
        "main"
    );
}

/// Hooks find the shell and the tools Git ships beside it, as they do when
/// Git for Windows' launcher sets up their environment.
#[tokio::test]
async fn hooks_run_with_the_shell_and_tools_their_git_ships() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    // Recorded under a name the hook must quote, as a home like
    // `C:\Users\O'Connor` would be.
    let ran = root.join("O'Connor").join("hook-ran");
    std::fs::create_dir(ran.parent().unwrap()).unwrap();
    let hook = main
        .join(".git")
        .join("hooks")
        .join("reference-transaction");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    // `cat` is no shell builtin: on Windows only Git's own PATH holds it.
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$1\" | cat >> '{}'\n",
            ran.to_str()
                .unwrap()
                .replace('\\', "/")
                .replace('\'', r"'\''")
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let adapter = GitSourceControl::default();
    let source = adapter.discover(&main).await;
    let plan = adapter
        .plan_checkout(Default::default(), &source, "hooked", &[])
        .await
        .unwrap();
    adapter.prepare_checkout(&plan).await.unwrap();

    let ran = std::fs::read_to_string(&ran).unwrap_or_default();
    assert!(
        ran.lines().any(|state| state == "committed"),
        "the branch Suru created ran the reference-transaction hook: {ran:?}"
    );
}

#[tokio::test]
async fn same_named_tag_cannot_hide_an_unmerged_local_source_branch() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    git(&main, &["-c", "tag.gpgsign=false", "tag", "main"]);
    let adapter = GitSourceControl::default();
    let source = adapter.discover(&main).await;
    let plan = adapter
        .plan_checkout(Default::default(), &source, "ambiguous-short-ref", &[])
        .await
        .unwrap();
    let CheckoutPreparationPlan::Git { source_branch, .. } = &plan.plan;
    assert_eq!(source_branch.as_deref(), Some("main"));
    let prepared = adapter.prepare_checkout(&plan).await.unwrap();
    let checkout = prepared.checkout.unwrap();
    commit(&checkout.root);
    let tip = read_git(&checkout.root, &["rev-parse", "HEAD"]);
    git(
        &main,
        &["update-ref", "refs/remotes/upstream/integration", &tip],
    );
    let target = removal_target(&adapter, &checkout.root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();

    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained,
        "the existing, unmerged local source branch prevents remote fallback"
    );
}

async fn removal_target(adapter: &GitSourceControl, path: &Path) -> CheckoutRemovalTarget {
    let resolved = adapter.discover(path).await;
    CheckoutRemovalTarget {
        repository: *resolved.workspace.repository.unwrap(),
        checkout: resolved.checkout.unwrap(),
    }
}

#[tokio::test]
async fn branch_outcome_is_deleted_only_when_the_managed_tip_is_in_its_local_source_branch() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();
    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained,
        "a branch without recorded provenance is always retained"
    );

    let base = read_git(&main, &["rev-parse", "main"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Deleted
    );

    commit(&linked_root);
    let inspection = adapter.inspect_removal(&target).await.unwrap();
    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained
    );
}

#[tokio::test]
async fn remote_tracking_refs_are_considered_only_after_the_source_branch_is_deleted() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    commit(&linked_root);
    let base = read_git(&main, &["rev-parse", "main"]);
    let managed = read_git(&main, &["rev-parse", "managed"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    git(
        &main,
        &["update-ref", "refs/remotes/upstream/integration", &managed],
    );
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();
    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained,
        "an existing local source branch is authoritative"
    );

    git(&main, &["checkout", "--detach"]);
    git(&main, &["branch", "-D", "main"]);
    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Deleted,
        "any local remote-tracking ref is a fallback, regardless of its branch name"
    );
}

#[tokio::test]
async fn unreadable_ref_store_does_not_authorize_remote_fallback() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    commit(&linked_root);
    let base = read_git(&main, &["rev-parse", "main"]);
    let managed = read_git(&main, &["rev-parse", "managed"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    git(
        &main,
        &["update-ref", "refs/remotes/upstream/integration", &managed],
    );
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();
    std::fs::write(main.join(".git/packed-refs"), "invalid packed ref\n").unwrap();

    assert_eq!(
        adapter
            .removal_branch_outcome(&target, &inspection)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained,
        "a fatal source-ref lookup is ambiguous, not proof that the branch is absent"
    );
}

#[tokio::test]
async fn removing_a_fully_merged_worktree_deletes_its_branch() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    let base = read_git(&main, &["rev-parse", "main"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();

    assert_eq!(
        adapter
            .remove_checkout(&target, &inspection, false, CheckoutBranchOutcome::Deleted,)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Deleted
    );
    assert!(!linked_root.exists());
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/managed"])
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn removal_never_broadens_a_confirmed_retained_branch_outcome() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    let base = read_git(&main, &["rev-parse", "main"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();

    assert_eq!(
        adapter
            .remove_checkout(&target, &inspection, false, CheckoutBranchOutcome::Retained,)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained
    );
    assert_eq!(read_git(&main, &["rev-parse", "managed"]), base);
}

#[tokio::test]
async fn branch_checked_out_elsewhere_is_retained_after_its_selected_worktree_is_removed() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "managed");
    let second = root.join("second");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--force",
            second.to_str().unwrap(),
            "managed",
        ],
    );
    let base = read_git(&main, &["rev-parse", "main"]);
    git(&main, &["config", "branch.managed.suru-base", &base]);
    git(
        &main,
        &["config", "branch.managed.suru-base-branch", "main"],
    );
    let adapter = GitSourceControl::default();
    let target = removal_target(&adapter, &linked_root).await;
    let inspection = adapter.inspect_removal(&target).await.unwrap();

    assert_eq!(
        adapter
            .remove_checkout(&target, &inspection, false, CheckoutBranchOutcome::Deleted,)
            .await
            .unwrap(),
        CheckoutBranchOutcome::Retained
    );
    assert!(!linked_root.exists());
    assert_eq!(read_git(&second, &["branch", "--show-current"]), "managed");
    assert_eq!(read_git(&main, &["rev-parse", "managed"]), base);
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

#[cfg(unix)]
#[tokio::test]
async fn paths_holding_line_breaks_are_discovered_like_any_other() {
    let (_temporary, root) = root();
    let main = root.join("main\nline");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked\nline");
    linked(&main, &linked_root, "topic");
    let adapter = GitSourceControl::default();
    for directory in [&main, &linked_root] {
        let resolved = adapter.discover(directory).await;
        assert_eq!(resolved.workspace.path, main);
        assert_eq!(&resolved.checkout.unwrap().root, directory);
        let repository = resolved.workspace.repository.unwrap();
        assert_eq!(
            repository.location,
            RepositoryLocation::Main { root: main.clone() }
        );
        assert_eq!(
            repository.capabilities.create_checkout,
            SourceControlCapability::Available
        );
        assert_eq!(resolved.checkouts.len(), 2);
        assert!(
            resolved
                .checkouts
                .iter()
                .all(|checkout| checkout.availability == SourceControlAvailability::Available)
        );
    }
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

#[cfg(unix)]
#[tokio::test]
async fn repeated_observation_and_listing_of_settled_checkouts_spawn_no_git() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "linked");
    let log = root.join("git-invocations.log");
    let shim = root.join("git-shim");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec git \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let invocations = |log: &Path| -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let adapter = GitSourceControl::new(&shim);
    let discovered = adapter.discover(&main).await;
    let checkout = discovered.checkout.unwrap();
    let repository = discovered.workspace.repository.unwrap();
    let _ = std::fs::remove_file(&log);
    let first = adapter.observe(&checkout).await;
    assert_eq!(first.availability, SourceControlAvailability::Available);
    let cold = invocations(&log);
    assert!(
        !cold.is_empty(),
        "first observation validates identity with Git: {cold:?}"
    );
    std::fs::remove_file(&log).unwrap();
    git(&main, &["checkout", "-b", "topic"]);
    let second = adapter.observe(&checkout).await;
    assert!(
        matches!(second.revision, Some(CheckoutRevision::Branch { ref name, .. }) if name == "topic")
    );
    git(&main, &["pack-refs", "--all"]);
    git(&main, &["checkout", "--detach"]);
    assert!(matches!(
        adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Detached { .. })
    ));
    let mut listed = adapter
        .list_checkouts(&repository)
        .await
        .unwrap()
        .into_iter()
        .map(|association| association.root)
        .collect::<Vec<_>>();
    listed.sort();
    assert_eq!(listed, vec![linked_root, main]);
    let warm = invocations(&log);
    assert!(
        warm.is_empty(),
        "a settled checkout is listed and re-read from disk, without Git: {warm:?}"
    );
}

#[tokio::test]
async fn validated_linked_worktree_replaced_by_another_repository_is_unavailable() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "topic");
    let adapter = GitSourceControl::default();
    let checkout = adapter.discover(&linked_root).await.checkout.unwrap();
    assert_eq!(checkout.root, linked_root);
    assert_eq!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Available
    );
    // A different Repository's checkout at the same path keeps the old
    // Repository's metadata intact; the identity Git confirmed no longer holds.
    std::fs::remove_dir_all(&linked_root).unwrap();
    let other = root.join("other");
    init(&other);
    commit(&other);
    linked(&other, &linked_root, "topic");
    let replaced = adapter.observe(&checkout).await;
    assert!(
        matches!(
            replaced.availability,
            SourceControlAvailability::Unavailable { .. }
        ),
        "{replaced:?}"
    );
    assert_eq!(replaced.revision, None);
    // A plain directory, then a clone, at the same path are equally unrelated.
    std::fs::remove_dir_all(&linked_root).unwrap();
    init(&linked_root);
    assert!(matches!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
    // Restoring the Repository's own Worktree at that path is observed again.
    std::fs::remove_dir_all(&linked_root).unwrap();
    git(&main, &["worktree", "prune"]);
    linked(&main, &linked_root, "topic-again");
    assert!(matches!(adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Branch { name, .. }) if name == "topic-again"));
    // A validated Worktree whose HEAD becomes unreadable is reported as such,
    // and reads again once it is restored.
    let head = main
        .join(".git")
        .join("worktrees")
        .join("linked")
        .join("HEAD");
    let contents = std::fs::read(&head).unwrap();
    std::fs::remove_file(&head).unwrap();
    assert!(matches!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
    std::fs::write(&head, contents).unwrap();
    assert!(matches!(adapter.observe(&checkout).await.revision,
        Some(CheckoutRevision::Branch { name, .. }) if name == "topic-again"));
}

#[cfg(unix)]
#[tokio::test]
async fn validated_worktree_moved_behind_a_symlink_at_its_root_is_unavailable() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    let linked_root = root.join("linked");
    linked(&main, &linked_root, "topic");
    let adapter = GitSourceControl::default();
    let checkout = adapter.discover(&linked_root).await.checkout.unwrap();
    assert_eq!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Available
    );
    std::fs::rename(&linked_root, root.join("real")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), &linked_root).unwrap();
    assert!(matches!(
        adapter.observe(&checkout).await.availability,
        SourceControlAvailability::Unavailable { .. }
    ));
}

/// A bare Repository at `within`/atlas.git, holding one commit on `main`, as
/// the server a Repository URL names would hold it; answers its path.
fn bare_origin(within: &Path) -> PathBuf {
    let work = within.join("atlas-work");
    init(&work);
    commit(&work);
    let bare = within.join("atlas.git");
    git(
        within,
        &[
            "clone",
            "--bare",
            "--quiet",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

/// How long a Clone these tests expect to finish may take: far longer than
/// any does, so only a Clone left waiting on a prompt reaches it.
const UNREACHED: std::time::Duration = std::time::Duration::from_secs(120);

#[tokio::test]
async fn a_clone_answers_the_workspace_it_made_as_discovery_names_it() {
    let (_temporary, root) = root();
    let origin = bare_origin(&root);
    let projects = root.join("projects");
    std::fs::create_dir(&projects).unwrap();
    let destination = projects.join("atlas");
    let adapter = GitSourceControl::default();
    assert_eq!(
        adapter.clone_capability(),
        SourceControlCapability::Available,
        "Git reports that it Clones"
    );

    let cloned = adapter
        .clone_repository(origin.to_str().unwrap(), &destination, None, UNREACHED)
        .await
        .unwrap();
    let discovered = adapter.discover(&destination).await.workspace;
    assert_eq!(
        (&cloned.id, &cloned.path),
        (&discovered.id, &discovered.path),
        "the Clone knows the Workspace it made as discovery names it, without reading it"
    );
    assert_eq!(cloned.path, destination);
    assert!(matches!(
        &cloned.repository.as_deref().unwrap().location,
        RepositoryLocation::Main { root } if root == &destination
    ));
    assert_eq!(
        read_git(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
}

#[tokio::test]
async fn a_clone_never_touches_what_stands_at_its_destination() {
    let (_temporary, root) = root();
    let origin = bare_origin(&root);
    let destination = root.join("atlas");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("notes.txt"), "Mine.").unwrap();
    let adapter = GitSourceControl::default();

    assert_eq!(
        adapter
            .clone_repository(origin.to_str().unwrap(), &destination, None, UNREACHED)
            .await,
        Err(CloneFailure::Occupied)
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("notes.txt")).unwrap(),
        "Mine."
    );
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
}

#[tokio::test]
async fn a_failed_clone_answers_in_gits_words_and_leaves_nothing_at_its_destination() {
    let (_temporary, root) = root();
    let origin = bare_origin(&root);
    let destination = root.join("atlas");
    let adapter = GitSourceControl::default();

    let missing = root.join("missing.git");
    let Err(CloneFailure::Failed(said)) = adapter
        .clone_repository(missing.to_str().unwrap(), &destination, None, UNREACHED)
        .await
    else {
        panic!("a Repository URL naming nothing fails the Clone");
    };
    assert!(said.starts_with("fatal: "), "Git's own words: {said}");
    assert!(!destination.exists(), "nothing is left at the destination");

    let Err(CloneFailure::Failed(said)) = adapter
        .clone_repository(
            origin.to_str().unwrap(),
            &destination,
            Some("nowhere"),
            UNREACHED,
        )
        .await
    else {
        panic!("a branch the Repository lacks fails the Clone");
    };
    assert!(said.contains("nowhere"), "Git's own words: {said}");
    assert!(
        !destination.exists(),
        "nor where Git had made the Repository before failing"
    );
}

#[tokio::test]
async fn a_clone_outlasting_its_timeout_is_stopped_and_leaves_nothing_at_its_destination() {
    let (_temporary, root) = root();
    let destination = root.join("atlas");
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "git://127.0.0.1:{}/atlas.git",
        silent.local_addr().unwrap().port()
    );
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((connection, _)) = silent.accept().await {
            held.push(connection);
        }
    });
    let adapter = GitSourceControl::default();

    assert_eq!(
        adapter
            .clone_repository(
                &url,
                &destination,
                None,
                std::time::Duration::from_millis(300)
            )
            .await,
        Err(CloneFailure::TimedOut)
    );
    assert!(!destination.exists(), "nothing is left at the destination");
    held.abort();
}

/// A server that asks for credentials, as one serving a private Repository
/// does, is answered at once that none could be had: Git asks no one, so the
/// Clone fails in its own words rather than waiting on a prompt nobody sees.
#[tokio::test]
async fn a_clone_needing_a_credential_git_cannot_find_fails_at_once() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (_temporary, root) = root();
    let destination = root.join("private");
    let asking = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://127.0.0.1:{}/private.git",
        asking.local_addr().unwrap().port()
    );
    let server = tokio::spawn(async move {
        while let Ok((mut connection, _)) = asking.accept().await {
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                match connection.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let _ = connection
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"suru\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    });
    let configuration = root.join("gitconfig");
    std::fs::write(&configuration, "").unwrap();
    let adapter = GitSourceControl::default().with_configuration_file(&configuration);

    let failure = adapter
        .clone_repository(&url, &destination, None, UNREACHED)
        .await;
    let Err(CloneFailure::Failed(said)) = failure else {
        panic!("a credential Git cannot find fails the Clone at once: {failure:?}");
    };
    assert!(
        said.contains("terminal prompts disabled"),
        "Git says it asked no one: {said}"
    );
    assert!(!destination.exists(), "nothing is left at the destination");
    server.abort();
}

/// What stands directly within `directory`, by name, sorted.
fn names_within(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// A Clone is made out of sight, in a hidden directory beside where it is
/// going, so nothing stands under its destination's name until it is whole;
/// and one abandoned by whoever awaited it — Git stopped with it — leaves
/// nothing behind.
#[tokio::test]
async fn a_clone_is_made_out_of_sight_and_one_abandoned_leaves_nothing_behind() {
    let (_temporary, root) = root();
    let projects = root.join("projects");
    std::fs::create_dir(&projects).unwrap();
    let destination = projects.join("atlas");
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "git://127.0.0.1:{}/atlas.git",
        silent.local_addr().unwrap().port()
    );
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((connection, _)) = silent.accept().await {
            held.push(connection);
        }
    });
    let adapter = GitSourceControl::default();

    {
        let cloning = adapter.clone_repository(&url, &destination, None, UNREACHED);
        tokio::pin!(cloning);
        let seen = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                tokio::select! {
                    finished = &mut cloning => panic!("the Clone waits on its silent server: {finished:?}"),
                    () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                        let names = names_within(&projects);
                        if !names.is_empty() {
                            return names;
                        }
                    }
                }
            }
        })
        .await
        .expect("Git begins the Clone");
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].starts_with(".atlas."),
            "Git makes the Clone in a hidden directory beside its destination: {seen:?}"
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !names_within(&projects).is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("an abandoned Clone leaves nothing behind");
    held.abort();
}

/// An ssh the user names of their own is still told to ask nothing, first,
/// so the Clone fails at once in its words rather than waiting out its
/// timeout: one run under a name Git does not know, as a wrapper is, and
/// one whose command tells it `-o BatchMode=no`, which the option the
/// Clone gives first stands over. The ssh here asks for the passphrase of a
/// locked key and waits on a terminal nobody watches unless the first
/// `BatchMode` it is given is `yes`, as OpenSSH keeps the first value it is
/// given; it answers Git's `-G` probe as OpenSSH does.
#[tokio::test]
async fn a_clone_through_an_ssh_the_user_names_asks_nothing_either() {
    let (_temporary, root) = root();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let script = "#!/bin/sh\n\
         batch=\n\
         for argument in \"$@\"; do\n\
         \x20 case \"$argument\" in\n\
         \x20   -G) exit 0 ;;\n\
         \x20   BatchMode=*) [ -n \"$batch\" ] || batch=\"${argument#BatchMode=}\" ;;\n\
         \x20 esac\n\
         done\n\
         if [ \"$batch\" = yes ]; then\n\
         \x20 echo 'Permission denied (publickey).' >&2\n\
         \x20 exit 255\n\
         fi\n\
         echo 'Enter passphrase for key locked_key:' >&2\n\
         sleep 60\n";
    for (name, options) in [
        ("vpn-wrapper", "-i locked_key"),
        ("ssh", "-o BatchMode=no -i locked_key"),
    ] {
        let ssh = bin.join(name);
        std::fs::write(&ssh, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let configuration = root.join(format!("{name}.gitconfig"));
        std::fs::write(
            &configuration,
            format!(
                "[core]\n\tsshCommand = {} {options}\n",
                ssh.to_str().unwrap().replace('\\', "/")
            ),
        )
        .unwrap();
        let adapter = GitSourceControl::default().with_configuration_file(&configuration);
        let destination = root.join("atlas");

        let failure = adapter
            .clone_repository(
                "ssh://git@example.invalid/atlas.git",
                &destination,
                None,
                std::time::Duration::from_secs(30),
            )
            .await;
        let Err(CloneFailure::Failed(said)) = failure else {
            panic!("{name} {options} fails the Clone at once: {failure:?}");
        };
        assert!(
            said.contains("Permission denied (publickey)."),
            "{name} {options}: {said}"
        );
        assert!(!destination.exists());
    }
    let mut left = names_within(&root);
    left.retain(|name| name.starts_with(".atlas"));
    assert!(left.is_empty(), "nothing of the Clones is left: {left:?}");
}

/// A Repository URL naming Git's `ext::` transport, which runs a command
/// the URL spells out, is refused in Git's words however the user's
/// configuration allows it — and however the environment the Server was
/// started in does: `GIT_ALLOW_PROTOCOL`, which stands over every
/// configuration, and configuration given in the environment alike — and
/// the command never runs.
#[tokio::test]
async fn a_clone_runs_no_command_a_repository_url_spells_out() {
    let (_temporary, root) = root();
    let configuration = root.join("gitconfig");
    std::fs::write(&configuration, "[protocol \"ext\"]\n\tallow = always\n").unwrap();
    let adapter = GitSourceControl::default()
        .with_configuration_file(&configuration)
        .with_environment("GIT_ALLOW_PROTOCOL", "ext")
        .with_environment("GIT_CONFIG_PARAMETERS", "'protocol.ext.allow'='always'")
        .with_environment("GIT_CONFIG_COUNT", "1")
        .with_environment("GIT_CONFIG_KEY_0", "protocol.allow")
        .with_environment("GIT_CONFIG_VALUE_0", "always");
    let ran = root.join("ran");
    let destination = root.join("atlas");

    let failure = adapter
        .clone_repository(
            &format!(
                "ext::sh -c touch% {}",
                ran.to_str().unwrap().replace('\\', "/")
            ),
            &destination,
            None,
            UNREACHED,
        )
        .await;
    let Err(CloneFailure::Failed(said)) = failure else {
        panic!("an ext:: URL is refused: {failure:?}");
    };
    assert!(said.contains("not allowed"), "in Git's own words: {said}");
    assert!(!ran.exists(), "the URL's command never ran");
    assert!(!destination.exists());
}
