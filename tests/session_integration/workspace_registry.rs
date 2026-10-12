//! The Workspace Registry: one entry for every Workspace a Server has known,
//! made the first time it is seen — a Session created there, or a Session's
//! directory re-resolved at start, or a Description or Icon set for it — and
//! the one list the Server's Workspace listing reads (ADR 0051). Each test
//! drives the Server as a Client does and asserts on what the Client's
//! Workspace listing answers, or on what the Registry left on disk.

use std::path::{Path, PathBuf};

use crate::{failing_provider_support::spawn_with_failing_provider, support};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{CreateSessionRequest, ExecutionDirectory, InitialPrompt, PromptId, Workspace},
    server::ServerConfig,
};

/// A first Prompt naming no Agent Selection, so its Session asks for no
/// Errand and its Workspace carries only what a test gives it.
fn quiet_request(directory: &Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        agent_selection: None,
        execution_directory: ExecutionDirectory {
            path: directory.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    }
}

async fn connected_client(state_dir: &Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    support::receive_managed_client_initial_state(&mut client).await;
    client
}

/// The Workspaces the Client's Workspace listing answers with, in its order.
async fn listed(client: &ManagedClient) -> Vec<Workspace> {
    client
        .list_workspaces()
        .await
        .expect("list Workspaces")
        .workspaces
}

/// A directory as the Server reads it, so a path the listing gives compares
/// equal to it on every platform.
fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).expect("read the directory canonically")
}

/// A Workspace a Session began in is the Server's to list from then on: once
/// its every Session is deleted it is listed still, by the path it is
/// presented by, and after a restart it is listed from the Registry alone,
/// with no Session there whose directory could be re-resolved.
#[tokio::test]
async fn a_workspace_is_listed_after_its_sessions_are_deleted_and_the_server_restarts() {
    const CHANNEL: &str = "workspace-registry-outlives-sessions";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;

    let created = client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace");
    let workspace = created.session.workspace.clone();
    assert_eq!(workspace.path, canonical(notes.path()));
    client
        .delete_session(created.session.id)
        .await
        .expect("delete the Workspace's only Session");

    assert_eq!(
        listed(&client).await,
        [workspace.clone()],
        "a Workspace a Session began in is listed once no Session works there"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;

    assert!(
        client
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .is_empty(),
        "no Session is left to name the Workspace"
    );
    assert_eq!(
        listed(&client).await,
        [workspace],
        "the Registry lists the Workspace after a restart, by its presented root"
    );

    restarted.shutdown().await.expect("shut down server");
}
