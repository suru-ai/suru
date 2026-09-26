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

#[tokio::test]
async fn same_named_tag_cannot_hide_an_unmerged_local_source_branch() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
    git(&main, &["tag", "main"]);
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
async fn repeated_observation_of_a_settled_checkout_reads_only_branch_and_commit() {
    let (_temporary, root) = root();
    let main = root.join("main");
    init(&main);
    commit(&main);
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
    let checkout = adapter.discover(&main).await.checkout.unwrap();
    let _ = std::fs::remove_file(&log);
    let first = adapter.observe(&checkout).await;
    assert_eq!(first.availability, SourceControlAvailability::Available);
    let cold = invocations(&log);
    assert!(
        cold.len() > 2,
        "first observation validates identity: {cold:?}"
    );
    std::fs::remove_file(&log).unwrap();
    git(&main, &["checkout", "-b", "topic"]);
    let second = adapter.observe(&checkout).await;
    assert!(
        matches!(second.revision, Some(CheckoutRevision::Branch { ref name, .. }) if name == "topic")
    );
    let warm = invocations(&log);
    assert_eq!(
        warm.len(),
        2,
        "a settled checkout is re-read without re-validating its identity: {warm:?}"
    );
    assert!(
        warm[0].contains("symbolic-ref") && warm[1].contains("rev-parse"),
        "{warm:?}"
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
