//! Claude's three unavailability conditions: a CLI that isn't installed, one below the version
//! floor the wire is verified against, and one no user is signed in to. Each reaches the catalog as
//! its own typed reason, each is found by a probe that never starts a Turn, and each clears on the
//! refresh after the user fixes it outside Suru.

use crate::support::{
    CLAUDE_MODELS, CLAUDE_SUGGESTED_VERSION, SIGNED_IN_ACCOUNT, SIGNED_OUT_ACCOUNT, ScriptedClaude,
    after_probe, claude_catalog, connect, crashing_list_models_arm, hosting, hosting_runtime,
    initialize_arm, list_models_arm, probe_arms, settled_session, signed_out_initialize_arm,
    silent_version_arm, unknown_version_request_arm, upgradable_version_arm, version_arm,
};
use std::sync::Arc;
use suru::{
    managed_client::ManagedClient,
    protocol::{
        Activity, CreateSessionRequest, InitialPrompt, PromptId, ProviderCatalogStatus,
        ProviderUnavailability, TurnStatus,
    },
    provider::ClaudeRuntime,
    server::{self, ServerConfig},
};
use tokio::time::Duration;

/// The condition the Claude catalog reports, which every one of these tests reads the same way.
fn condition(status: &ProviderCatalogStatus) -> (ProviderUnavailability, String) {
    let ProviderCatalogStatus::Unavailable { reason, message } = status else {
        panic!("the Claude catalog reports a typed unavailability, got {status:?}");
    };
    (*reason, message.clone())
}

/// Asserts the refresh after the user has fixed the condition outside Suru finds Claude usable
/// again, which is the recovery every one of these conditions shares.
async fn assert_the_refresh_clears_the_condition(client: &ManagedClient) {
    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    let refreshed = claude_catalog(&refreshed);
    assert_eq!(
        refreshed.status,
        ProviderCatalogStatus::Fresh,
        "the refresh re-checks the condition and finds it fixed, with no restart in between"
    );
    assert!(
        !refreshed.models.is_empty(),
        "the Provider that was unavailable now serves its Models"
    );
}

#[tokio::test]
async fn a_missing_claude_binary_is_reported_as_not_installed_until_it_is_installed() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    claude.uninstall();
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-not-installed", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let (reason, message) = condition(&claude_catalog(&catalog).status);
    assert_eq!(reason, ProviderUnavailability::NotInstalled);
    assert!(
        message.contains("could not launch"),
        "the condition names the CLI Suru could not launch, got: {message}"
    );
    assert_eq!(
        claude.launches(),
        0,
        "a CLI that isn't there is answered without a process"
    );
    assert!(claude_catalog(&catalog).models.is_empty());

    claude.install();
    assert_the_refresh_clears_the_condition(&client).await;

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn a_cli_below_the_suggested_version_warns_without_withholding_models() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        upgradable_version_arm(),
        initialize_arm(SIGNED_IN_ACCOUNT),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-old-cli", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("a readable older CLI is an answer, not a crash");
    let claude_models = claude_catalog(&catalog);
    let ProviderCatalogStatus::Warning { message } = &claude_models.status else {
        panic!(
            "the older Claude CLI reports advisory guidance, got {:?}",
            claude_models.status
        );
    };
    assert!(
        message.contains("Claude Code CLI 2.1.236")
            && message.contains("may have compatibility issues")
            && message.contains(CLAUDE_SUGGESTED_VERSION),
        "the warning names the reported and suggested versions, got: {message}"
    );
    assert_eq!(
        claude.control_subtypes(),
        after_probe(["list_models"]),
        "a readable older CLI is still checked for sign-in and asked for its Models"
    );
    assert!(
        !claude_models.models.is_empty(),
        "the compatibility warning does not disable Claude"
    );

    // No TTL is injected: the refresh clearing the warning shows advisory verdicts are not kept,
    // so updating the CLI needs no restart and nothing to wait out.
    claude.upgrade();
    assert_the_refresh_clears_the_condition(&client).await;

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn a_cli_that_cannot_tell_suru_its_version_is_reported_as_incompatible() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        unknown_version_request_arm(),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-versionless-cli", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("a CLI that refuses the question is an answer, not a crash");
    let (reason, _) = condition(&claude_catalog(&catalog).status);
    assert_eq!(
        reason,
        ProviderUnavailability::IncompatibleVersion,
        "a CLI that will not say which version it is, is one Suru cannot vouch for"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

/// A CLI that answers nothing at all is not one the user updates away from: it is a Provider that
/// has stopped working, and telling the user to update their CLI would send them after the wrong
/// thing.
#[tokio::test]
async fn a_cli_that_answers_nothing_at_all_fails_rather_than_naming_a_condition() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        silent_version_arm(),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let runtime = ClaudeRuntime::new(claude.executable())
        .with_control_request_timeout(Duration::from_millis(100))
        .with_process_exit_grace(Duration::from_millis(50));
    let (server, client) = hosting_runtime(runtime, "claude-silent-probe", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let ProviderCatalogStatus::Failed { message } = &claude_catalog(&catalog).status else {
        panic!(
            "a CLI that never answers the probe fails the catalog rather than naming a condition \
             the user fixes, got {:?}",
            claude_catalog(&catalog).status
        );
    };
    assert!(
        message.contains("timed out"),
        "the failure says the CLI stopped answering, got: {message}"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn a_cli_no_one_is_signed_in_to_is_reported_as_such_until_the_user_signs_in() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        version_arm(CLAUDE_SUGGESTED_VERSION),
        signed_out_initialize_arm(),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-not-signed-in", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let (reason, message) = condition(&claude_catalog(&catalog).status);
    assert_eq!(reason, ProviderUnavailability::NotSignedIn);
    assert!(
        message.contains("sign in"),
        "the condition says what the user does about it, got: {message}"
    );
    assert_eq!(
        claude.control_subtypes(),
        ["get_binary_version", "initialize"],
        "the account is read from the init handshake, and a signed-out CLI is never asked for Models"
    );
    assert!(
        claude_catalog(&catalog).models.is_empty(),
        "an unusable Provider offers no Model for a Session to be started on"
    );

    // No TTL is injected here either, so signing in and refreshing is the whole recovery.
    claude.sign_in();
    assert_the_refresh_clears_the_condition(&client).await;

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn the_account_probe_starts_no_turn_and_persists_no_session() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-probe-shape", state_dir.path()).await;

    client.list_models().await.expect("discover Claude Models");

    let probe = claude
        .launch_arguments()
        .first()
        .cloned()
        .expect("the probe is the first process the runtime launches");
    assert_eq!(
        probe,
        [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--setting-sources",
            "--strict-mcp-config",
            "--no-session-persistence",
        ],
        "the probe runs the CLI with its filesystem settings and hooks unloaded, no MCP servers \
         started, and nothing written to the conversation store"
    );
    assert!(
        !probe.iter().any(|argument| argument == "--session-id"
            || argument == "--resume"
            || argument == "--model"),
        "the probe opens no conversation of its own, got: {probe:?}"
    );
    assert!(
        !claude
            .requests()
            .iter()
            .any(|request| request.get("type").and_then(serde_json::Value::as_str) == Some("user")),
        "the probe never delivers a Prompt, so it can never start a billable Turn"
    );
    assert_eq!(
        claude.control_subtypes(),
        after_probe(["list_models"]),
        "the probe asks the CLI its version and who is signed in, and nothing more"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn catalog_reads_within_the_ttl_reuse_the_probes_verdict() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-probe-cached", state_dir.path()).await;

    client.list_models().await.expect("discover Claude Models");
    client.refresh_models().await.expect("refresh the catalog");
    client
        .refresh_models()
        .await
        .expect("refresh the catalog again");

    assert_eq!(
        claude.control_subtypes(),
        after_probe(["list_models", "list_models", "list_models"]),
        "a usable Claude is probed once and asked for its Models on every read"
    );
    assert_eq!(
        claude.launches(),
        4,
        "the probe's own short-lived process is launched once, the discoveries' once each"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn a_verdict_older_than_the_injected_ttl_is_probed_again() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let runtime = ClaudeRuntime::new(claude.executable())
        .with_availability_ttl(Duration::from_millis(1))
        .with_process_exit_grace(Duration::from_millis(50));
    let (server, client) = hosting_runtime(runtime, "claude-probe-expiry", state_dir.path()).await;

    client.list_models().await.expect("discover Claude Models");
    tokio::time::sleep(Duration::from_millis(10)).await;
    client.refresh_models().await.expect("refresh the catalog");

    assert_eq!(
        claude.control_subtypes(),
        [after_probe(["list_models"]), after_probe(["list_models"])].concat(),
        "a verdict older than the TTL is asked of the CLI again rather than replayed"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

#[tokio::test]
async fn a_turn_reaching_an_unavailable_claude_fails_with_the_condition_leading_the_message() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        version_arm(CLAUDE_SUGGESTED_VERSION),
        initialize_arm(SIGNED_OUT_ACCOUNT),
        list_models_arm(CLAUDE_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-unavailable-turn").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-unavailable-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let text = settled
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the failed Turn carries an Error Activity");
    assert!(
        text.starts_with("Provider `claude` is not signed in"),
        "the failure leads with the condition the user fixes, got {text:?}"
    );
    assert!(
        text.contains("sign in"),
        "the Provider's own account of the condition survives, got {text:?}"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exits(claude.launches()).await;
}

/// A probe answered by a fixture that only ever answers the probe: no `list_models` arm at all, so
/// the discovery that follows a good verdict is what fails, not the probe.
#[tokio::test]
async fn a_usable_cli_whose_discovery_fails_is_a_failure_rather_than_a_condition() {
    let claude = ScriptedClaude::new(&format!("{}{}", probe_arms(), crashing_list_models_arm()));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-probe-then-failure", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    assert!(
        matches!(
            claude_catalog(&catalog).status,
            ProviderCatalogStatus::Failed { .. }
        ),
        "a Claude the probe found usable fails as a Provider that broke, not as a condition the \
         user fixes, got {:?}",
        claude_catalog(&catalog).status
    );

    server.shutdown().await.expect("shut the server down");
}
