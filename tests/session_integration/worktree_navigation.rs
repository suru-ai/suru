//! Existing checkout selection through the same authenticated Server route used by the TUI.
use crate::{
    failing_provider_support::spawn_with_failing_provider, provider_support::ControlledProvider,
    repositories::git, support,
};
use std::path::Path;
use suru::{
    protocol::*,
    server::{self, ServerConfig},
};

fn request(
    path: &Path,
    workspace: Option<WorkspaceId>,
    checkout: Option<CheckoutId>,
    remembered: Option<&Path>,
) -> ResolveWorkspaceRequest {
    ResolveWorkspaceRequest {
        checkout_id: checkout,
        remembered_execution_directory: remembered.map(|path| ExecutionDirectory {
            path: path.to_owned(),
        }),
        workspace_id: workspace,
        base: None,
        path: path.to_owned(),
    }
}
async fn resolve(
    descriptor: &RuntimeDescriptor,
    request: &ResolveWorkspaceRequest,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/resolve", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .unwrap()
}
async fn selected(
    descriptor: &RuntimeDescriptor,
    request: &ResolveWorkspaceRequest,
) -> ResolvedWorkspace {
    resolve(descriptor, request)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
fn committed(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
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

#[tokio::test]
async fn choosing_existing_detached_worktree_starts_multiple_agents_at_root_and_preserves_explicit_subdir()
 {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let linked = root.join("external detached");
    git(
        &main,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let nested = linked.join("packages");
    std::fs::create_dir(&nested).unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(root.join("state"), "worktree-navigation").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let initial = selected(server.descriptor(), &request(&nested, None, None, None)).await;
    assert_eq!(initial.execution_directory.as_ref().unwrap().path, nested);
    let association = initial.checkout.clone().unwrap();
    let choice = selected(
        server.descriptor(),
        &request(
            &main,
            Some(initial.workspace.id.clone()),
            Some(association.id.clone()),
            None,
        ),
    )
    .await;
    assert_eq!(choice.execution_directory.as_ref().unwrap().path, linked);
    for _ in 0..2 {
        let snapshot = support::create_session(
            server.descriptor(),
            &CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: choice.execution_directory.clone().unwrap(),
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Work in the selected checkout".to_owned(),
                    skill_invocations: vec![],
                },
            },
        )
        .await;
        let startup = provider.next_start().await;
        assert_eq!(startup.execution_directory(), linked);
        assert_eq!(
            snapshot.session.checkout.as_ref().unwrap().id,
            association.id
        );
        // The Provider request proves admission's actual destination. Dropping
        // this controlled reply settles the test Session as a startup failure.
        drop(startup);
    }
    let restored = selected(
        server.descriptor(),
        &request(
            &main,
            Some(initial.workspace.id.clone()),
            None,
            Some(&nested),
        ),
    )
    .await;
    assert_eq!(restored.execution_directory.unwrap().path, nested);
    assert_eq!(restored.workspace.id, initial.workspace.id);
    let default = selected(
        server.descriptor(),
        &request(&main, Some(initial.workspace.id), None, None),
    )
    .await;
    assert_eq!(default.execution_directory.unwrap().path, main);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn remembered_missing_and_replaced_directories_stay_selected_without_membership_or_main_fallback()
 {
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let linked = root.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let server = spawn_with_failing_provider(
        ServerConfig::new(root.join("state"), "remember-worktree").unwrap(),
    )
    .await
    .unwrap();
    let initial = selected(server.descriptor(), &request(&linked, None, None, None)).await;
    let remembered = request(
        &main,
        Some(initial.workspace.id.clone()),
        None,
        Some(&linked),
    );
    std::fs::remove_dir_all(&linked).unwrap();
    for replacement in [false, true] {
        if replacement {
            std::fs::create_dir(&linked).unwrap();
        }
        let missing = selected(server.descriptor(), &remembered).await;
        assert_eq!(missing.workspace.id, initial.workspace.id);
        assert_eq!(missing.execution_directory.unwrap().path, linked);
        assert!(matches!(
            missing.execution_status,
            ExecutionDirectoryStatus::Unavailable { .. }
        ));
    }
    committed(&linked);
    let replaced = selected(server.descriptor(), &remembered).await;
    assert!(matches!(
        replaced.execution_status,
        ExecutionDirectoryStatus::Unavailable { .. }
    ));
    assert_eq!(replaced.execution_directory.unwrap().path, linked);
    assert_eq!(
        resolve(
            server.descriptor(),
            &request(
                &main,
                Some(initial.workspace.id),
                Some(initial.checkout.unwrap().id),
                None
            )
        )
        .await
        .status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        resolve(
            server.descriptor(),
            &request(&root.join("missing explicit path"), None, None, None)
        )
        .await
        .status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn bare_and_unknown_main_workspaces_offer_existing_working_copies_without_executing_metadata()
{
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let source = root.join("source");
    committed(&source);
    let bare = root.join("bare.git");
    git(
        &root,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let bare_linked = root.join("bare linked");
    git(
        &bare,
        &["worktree", "add", bare_linked.to_str().unwrap(), "main"],
    );
    let metadata = root.join("separate metadata");
    let separate = root.join("separate main");
    git(
        &root,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            separate.to_str().unwrap(),
        ],
    );
    git(
        &separate,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let separate_linked = root.join("separate linked");
    git(
        &separate,
        &[
            "worktree",
            "add",
            "-b",
            "topic",
            separate_linked.to_str().unwrap(),
        ],
    );
    let server = spawn_with_failing_provider(
        ServerConfig::new(root.join("state"), "metadata-navigation").unwrap(),
    )
    .await
    .unwrap();
    for (grouping, linked) in [(&bare, &bare_linked), (&metadata, &separate_linked)] {
        let workspace = selected(server.descriptor(), &request(grouping, None, None, None)).await;
        assert!(workspace.execution_directory.is_none());
        assert_eq!(
            workspace.execution_status,
            ExecutionDirectoryStatus::RequiresWorkingCopy
        );
        let checkout = workspace
            .checkouts
            .iter()
            .find(|checkout| &checkout.association.root == linked)
            .unwrap();
        let destination = selected(
            server.descriptor(),
            &request(
                grouping,
                Some(workspace.workspace.id.clone()),
                Some(checkout.association.id.clone()),
                None,
            ),
        )
        .await;
        assert_eq!(destination.workspace.id, workspace.workspace.id);
        assert_eq!(destination.execution_directory.unwrap().path, *linked);
    }
    server.shutdown().await.unwrap();
}
