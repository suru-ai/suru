//! Keeping the Workspace Registry in step with the disk and the Roots (ADR
//! 0051): an entry stands while its directory can be read or a Session
//! belongs to it. Each listing reads where every entry is presented. One
//! whose directory is gone while the directory holding it can be read is
//! dropped, its Icon and Description with it — unless a Session belongs to
//! it, which keeps it listed as unavailable. One whose directory, or the
//! directory holding it, cannot be read — a drive not mounted, say — is kept
//! and listed as unavailable, across restarts. Removing a Workspace Root drops
//! the entries discovered under it that no Session belongs to, and leaves
//! every other entry alone. These rules hold for every entry, however it came
//! to be known.
//!
//! Each test drives the Server as a Client does, with real directories made,
//! deleted, moved away and sealed, and asserts on what the Client's
//! Workspace listing answers, or on what the Registry left on disk.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use suru::{
    managed_client::ManagedClient,
    protocol::{
        SessionId, SettingMutation, Workspace, WorkspaceDescription, WorkspaceUnavailability,
    },
};
use tokio::time::timeout;

use crate::{
    server_support::PROGRESS_DEADLINE,
    workspace_discovery::{repository_id, server_with_roots, spelled},
    workspace_registry::{
        BY_SESSION, canonical, connected_client, entries_on_disk, landing_workspace, listed,
        quiet_request, repository, set,
    },
};

/// What a listing says of a Workspace's standing: where it is presented,
/// and why it cannot be opened, where it cannot.
fn standing(workspace: &Workspace) -> (PathBuf, Option<WorkspaceUnavailability>) {
    (workspace.path.clone(), workspace.unavailable)
}

/// What a listing says of each Workspace's standing, by path.
async fn standings(client: &ManagedClient) -> Vec<(PathBuf, Option<WorkspaceUnavailability>)> {
    let mut standings = listed(client)
        .await
        .iter()
        .map(standing)
        .collect::<Vec<_>>();
    standings.sort_by(|(left, _), (right, _)| left.cmp(right));
    standings
}

/// The Description the listing gives the Workspace presented at `path`.
async fn description_at(client: &ManagedClient, path: &Path) -> Option<WorkspaceDescription> {
    listed(client)
        .await
        .into_iter()
        .find(|workspace| workspace.path == path)
        .unwrap_or_else(|| panic!("{} is listed", path.display()))
        .description
}

/// Deletes the Session `session_id` once it has stopped working, which a
/// Session whose Provider fails it does at once.
async fn delete_once_settled(client: &ManagedClient, session_id: SessionId) {
    timeout(PROGRESS_DEADLINE, async {
        while client
            .read_session(session_id)
            .await
            .expect("read the Session")
            .session
            .working_since
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Session stops working");
    client
        .delete_session(session_id)
        .await
        .expect("delete the Session");
}

/// A directory deleted while the directory holding it can still be read
/// leaves no ghost: the next listing drops its entry, discovered or
/// described, with the Icon and Description it carried, so a Repository
/// made there again is discovered afresh carrying nothing.
#[tokio::test]
async fn a_directory_deleted_drops_its_entry_with_its_icon_and_description() {
    const CHANNEL: &str = "workspace-standing-deleted";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let drafts = home.join("drafts");
    std::fs::create_dir(&drafts).expect("create the drafts directory");
    let (config, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let atlas_id = repository_id(&atlas.join(".git"));
    client
        .set_workspace_description(&atlas_id, Some(&atlas), "Maps of everything.")
        .await
        .expect("describe the discovered Repository");
    client
        .set_workspace_icon(&atlas_id, "dev-rust")
        .await
        .expect("choose the discovered Repository's Icon");
    let drafts_id = landing_workspace(&client, &drafts).await.id;
    client
        .set_workspace_description(&drafts_id, Some(&drafts), "Where drafts wait.")
        .await
        .expect("describe the drafts directory");
    assert_eq!(
        standings(&client).await,
        [(drafts.clone(), None), (atlas.clone(), None)],
        "both are listed, and either can be opened"
    );

    std::fs::remove_dir_all(&atlas).expect("delete the Repository");
    std::fs::remove_dir_all(&drafts).expect("delete the drafts directory");

    assert_eq!(
        standings(&client).await,
        [],
        "a directory deleted from one that can be read is dropped at the next listing"
    );

    repository(&atlas);
    let listing = listed(&client).await;
    assert_eq!(
        listing
            .iter()
            .map(|workspace| (
                workspace.id.clone(),
                workspace.path.clone(),
                workspace.icon.clone(),
                workspace.description.clone(),
                workspace.unavailable,
            ))
            .collect::<Vec<_>>(),
        [(atlas_id, atlas.clone(), None, None, None)],
        "a Repository made there again is discovered afresh, carrying nothing it carried before"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    assert!(
        !entries.contains_key(&drafts_id),
        "the entry is dropped from disk, its Description with it: {entries:?}"
    );
}

/// An entry a Session belongs to stands though its directory is gone,
/// listed as unavailable and saying why, so the Session keeps its
/// Workspace; once no Session belongs to it, the next listing drops it.
#[tokio::test]
async fn an_entry_a_session_belongs_to_is_kept_unavailable_once_its_directory_is_gone() {
    const CHANNEL: &str = "workspace-standing-session-belongs";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let notes = home.join("notes");
    std::fs::create_dir(&notes).expect("create the notes directory");
    let (config, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let session = client
        .create_session(quiet_request(&notes, "Keep the notes"))
        .await
        .expect("begin a Session in the notes directory")
        .session;

    std::fs::remove_dir_all(&notes).expect("delete the notes directory");

    for listing in ["the next listing", "every listing after it"] {
        assert_eq!(
            standings(&client).await,
            [(notes.clone(), Some(WorkspaceUnavailability::Gone))],
            "{listing} keeps the entry a Session belongs to, unavailable"
        );
    }

    delete_once_settled(&client, session.id).await;
    assert_eq!(
        standings(&client).await,
        [],
        "once no Session belongs to it, the entry follows the disk"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    assert!(
        !entries_on_disk(&config).contains_key(&session.workspace.id),
        "and is dropped from disk"
    );
}

/// An entry whose directory's holder cannot be read — here the drive it is
/// on moved away, as an unmounted drive goes — is kept and listed as
/// unavailable with what it carries, across a restart, and is listed as
/// ever once the drive is back.
#[tokio::test]
async fn an_entry_whose_holder_is_moved_away_is_kept_unavailable_across_restarts() {
    const CHANNEL: &str = "workspace-standing-holder-gone";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let drive = home.join("drive");
    let projects = drive.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let (_, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    client
        .set_workspace_description(
            &repository_id(&atlas.join(".git")),
            Some(&atlas),
            "Maps of everything.",
        )
        .await
        .expect("describe the discovered Repository");
    assert_eq!(standings(&client).await, [(atlas.clone(), None)]);

    let unplugged = home.join("unplugged");
    std::fs::rename(&drive, &unplugged).expect("take the drive away");

    let away = [(
        atlas.clone(),
        Some(WorkspaceUnavailability::HolderUnreadable),
    )];
    assert_eq!(
        standings(&client).await,
        away,
        "the entry is kept, unavailable, while the directory holding it cannot be read"
    );
    assert_eq!(
        description_at(&client, &atlas).await,
        set("Maps of everything."),
        "with what it carries"
    );
    drop(client);
    server.shutdown().await.expect("shut down server");

    let (_, restarted) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    assert_eq!(
        standings(&client).await,
        away,
        "a restart drops nothing whose holder cannot be read"
    );

    std::fs::rename(&unplugged, &drive).expect("bring the drive back");
    assert_eq!(
        standings(&client).await,
        [(atlas.clone(), None)],
        "once the drive is back, the Workspace can be opened again"
    );
    assert_eq!(
        description_at(&client, &atlas).await,
        set("Maps of everything."),
        "carrying all it did"
    );

    drop(client);
    restarted.shutdown().await.expect("shut down server");
}

/// A directory whose permissions deny reading it, or deny reading the
/// directory holding it, is listed as unavailable and never dropped. Windows
/// denies reading through an access control list rather than a mode, so this
/// is pinned where a mode can say it; the holder moved away stands for it
/// everywhere.
#[cfg(unix)]
#[tokio::test]
async fn an_entry_sealed_or_whose_holder_is_sealed_is_kept_unavailable() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives a directory back its permissions however the test ends, so it
    /// can be removed.
    struct Unseal(PathBuf);
    impl Drop for Unseal {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    fn seal(directory: &Path) -> Unseal {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o000))
            .expect("seal the directory");
        Unseal(directory.to_owned())
    }

    const CHANNEL: &str = "workspace-standing-sealed";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let ledger = projects.join("ledger");
    repository(&ledger);
    let (_, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    assert_eq!(
        standings(&client).await,
        [(atlas.clone(), None), (ledger.clone(), None)]
    );

    let sealed_ledger = seal(&ledger);
    // A superuser reads past any mode, so there is nothing to keep apart.
    if std::fs::read_dir(&ledger).is_ok() {
        drop(client);
        server.shutdown().await.expect("shut down server");
        return;
    }
    assert_eq!(
        standings(&client).await,
        [
            (atlas.clone(), None),
            (ledger.clone(), Some(WorkspaceUnavailability::Unreadable))
        ],
        "a directory that cannot be read is listed as unavailable"
    );

    let sealed_projects = seal(&projects);
    assert_eq!(
        standings(&client).await,
        [
            (
                atlas.clone(),
                Some(WorkspaceUnavailability::HolderUnreadable)
            ),
            (
                ledger.clone(),
                Some(WorkspaceUnavailability::HolderUnreadable)
            )
        ],
        "an entry whose holder cannot be read is listed as unavailable, and kept"
    );

    drop(sealed_projects);
    drop(sealed_ledger);
    assert_eq!(
        standings(&client).await,
        [(atlas.clone(), None), (ledger.clone(), None)],
        "both can be opened again once they can be read"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Removing a Workspace Root drops the entries discovered under it that no
/// Session belongs to, Icon and Description with them, so an over-broad
/// Root can be undone; one a Session belongs to is kept, until no Session
/// does. An entry first known by a Session or by its Description is left
/// alone though it lies within the Root, and so is everything discovered
/// under another Root.
#[tokio::test]
async fn removing_a_root_drops_what_was_discovered_under_it_but_what_a_session_belongs_to() {
    const CHANNEL: &str = "workspace-standing-root-removed";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let ledger = projects.join("ledger");
    repository(&ledger);
    let notes = projects.join("notes");
    repository(&notes);
    let drafts = projects.join("drafts");
    std::fs::create_dir(&drafts).expect("create the drafts directory");
    let archive = home.join("archive");
    let old = archive.join("old");
    repository(&old);
    let (config, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let noted = client
        .create_session(quiet_request(&notes, "Keep the notes"))
        .await
        .expect("begin a Session in the notes before its Root is named")
        .session;
    delete_once_settled(&client, noted.id).await;
    let drafts_id = landing_workspace(&client, &drafts).await.id;
    client
        .set_workspace_description(&drafts_id, Some(&drafts), "Where drafts wait.")
        .await
        .expect("describe the drafts directory");
    let name = |roots: &[&Path]| SettingMutation::WorkspaceRoots {
        value: Some(roots.iter().map(|root| spelled(root).to_owned()).collect()),
    };
    client
        .mutate_setting(name(&[&projects, &archive]))
        .await
        .expect("name both Roots");
    let atlas_id = repository_id(&atlas.join(".git"));
    client
        .set_workspace_description(&atlas_id, Some(&atlas), "Maps of everything.")
        .await
        .expect("describe a discovered Repository");
    let balanced = client
        .create_session(quiet_request(&ledger, "Balance the ledger"))
        .await
        .expect("begin a Session in a discovered Repository")
        .session;
    let every = [
        (old.clone(), None),
        (atlas.clone(), None),
        (drafts.clone(), None),
        (ledger.clone(), None),
        (notes.clone(), None),
    ];
    assert_eq!(standings(&client).await, every);

    client
        .mutate_setting(name(&[&archive]))
        .await
        .expect("take the projects Root away");

    assert_eq!(
        standings(&client).await,
        [
            (old.clone(), None),
            (drafts.clone(), None),
            (ledger.clone(), None),
            (notes.clone(), None),
        ],
        "only what was discovered under the Root taken away, with no Session, is dropped"
    );
    delete_once_settled(&client, balanced.id).await;
    assert_eq!(
        standings(&client).await,
        [
            (old.clone(), None),
            (drafts.clone(), None),
            (notes.clone(), None),
        ],
        "once no Session belongs to it, an entry discovered under a Root taken away goes with it"
    );

    client
        .mutate_setting(name(&[&projects, &archive]))
        .await
        .expect("name the projects Root again");
    assert_eq!(
        description_at(&client, &atlas).await,
        None,
        "a Repository discovered again under the Root carries nothing it carried before"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    assert_eq!(
        entries.get(&noted.workspace.id),
        Some(&(Some(notes.clone()), BY_SESSION.to_owned())),
        "an entry known by a Session is left alone though it lies within the Root"
    );
    assert_eq!(
        entries.get(&drafts_id),
        Some(&(Some(drafts.clone()), r#""description""#.to_owned())),
        "and so is one known by its Description"
    );
    let archive_spelled = spelled(&archive).replace('\\', "\\\\");
    assert_eq!(
        entries.get(&repository_id(&old.join(".git"))),
        Some(&(
            Some(old.clone()),
            format!(r#"{{"discovered":{{"root":"{archive_spelled}"}}}}"#)
        )),
        "and so is one discovered under another Root"
    );
}
