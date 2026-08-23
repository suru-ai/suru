//! Claude's Model catalog: discovery through short-lived stream-json spawns, the CLI's rows
//! presented verbatim, and the conditions that leave the catalog empty.

use std::sync::Arc;

use crate::{
    provider_support::ControlledProvider,
    support::{
        CLAUDE_MODELS, ScriptedClaude, claude_catalog, drifting_list_models_arm, hosting,
        hosting_runtime, malformed_list_models_arm, silent_list_models_arm,
    },
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, ProviderUnavailability,
    },
    provider::ClaudeRuntime,
    server::{self, ServerConfig},
};
use tokio::time::Duration;

use crate::server_support::receive_initial_state;

fn model<'a>(catalog: &'a ProviderModelCatalog, id: &str) -> &'a ModelDescriptor {
    catalog
        .models
        .iter()
        .find(|model| model.id == ModelId::new(id))
        .unwrap_or_else(|| panic!("the Claude catalog lists Model `{id}`"))
}

fn choices(model: &ModelDescriptor, option: &str) -> (Vec<String>, String) {
    let descriptor = model
        .options
        .iter()
        .find(|descriptor| descriptor.id == ModelOptionId::new(option))
        .unwrap_or_else(|| panic!("Model `{}` advertises option `{option}`", model.id));
    let ModelOptionKind::Select { choices, default } = &descriptor.kind else {
        panic!("Model Option `{option}` is a Select option");
    };
    (
        choices.iter().map(|choice| choice.id.to_string()).collect(),
        default.to_string(),
    )
}

#[tokio::test]
async fn claude_models_reach_the_catalog_verbatim_with_the_clis_default_row_marked() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-model-catalog", state_dir.path()).await;

    let catalog = client.list_models().await.expect("discover Claude Models");
    let claude_models = claude_catalog(&catalog);
    assert_eq!(claude_models.status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        claude_models
            .models
            .iter()
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>(),
        ["default", "fixture[1m]", "middling", "tiny"],
        "every row the CLI lists is presented, in the CLI's own order, \
         alias values included verbatim"
    );
    assert_eq!(
        claude_models
            .models
            .iter()
            .filter(|model| model.is_default)
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>(),
        ["default"],
        "the CLI's own recommended row is the default selection"
    );

    let recommended = model(claude_models, "default");
    assert_eq!(recommended.display_name, "Default (recommended)");
    assert_eq!(
        recommended.description,
        "Fixture 1 · Best for everyday tasks"
    );
    assert_eq!(recommended.availability, ModelAvailability::Available);
    assert_eq!(
        recommended
            .options
            .iter()
            .map(|option| option.role)
            .collect::<Vec<_>>(),
        [ModelOptionRole::ReasoningEffort],
        "reasoning effort is the only Model Option this Provider advertises"
    );
    assert_eq!(
        choices(recommended, "reasoning_effort"),
        (
            vec![
                "low".to_owned(),
                "medium".to_owned(),
                "high".to_owned(),
                "xhigh".to_owned(),
            ],
            "high".to_owned(),
        ),
        "the levels are exactly what the row advertised, defaulting to the CLI's own default effort"
    );
    assert_eq!(
        recommended.default_agent_selection().options,
        vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("high"),
            },
        }]
    );

    let alias = model(claude_models, "fixture[1m]");
    assert_eq!(
        alias.display_name, "Fixture (1M context)",
        "an alias row resolving to the same canonical model is still its own row"
    );
    assert!(!alias.is_default, "exactly one row is the default");

    assert_eq!(
        choices(model(claude_models, "middling"), "reasoning_effort"),
        (
            vec!["low".to_owned(), "medium".to_owned()],
            "low".to_owned()
        ),
        "a row not offering the CLI's default effort defaults to its first level instead"
    );

    assert!(
        model(claude_models, "tiny").options.is_empty(),
        "a row without effort metadata carries no Model Options"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn each_discovery_launches_its_own_short_lived_stream_json_process() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-short-lived-process", state_dir.path()).await;

    client.list_models().await.expect("discover Claude Models");
    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    assert_eq!(
        claude_catalog(&refreshed).models.len(),
        4,
        "the refreshed catalog is asked of the CLI again, not replayed from a cache"
    );

    assert_eq!(
        claude.launches(),
        2,
        "every discovery is its own short-lived process rather than a shared server"
    );
    assert_eq!(
        claude.control_subtypes(),
        ["list_models", "list_models"],
        "discovery asks the CLI for its models and nothing else"
    );
    let arguments = claude.arguments();
    assert_eq!(
        arguments,
        [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--setting-sources",
        ],
        "the CLI is launched in stream-json mode with filesystem settings left unloaded \
         (the empty settings-sources value is invisible to the fixture's argv capture)"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn a_catalog_without_the_clis_default_row_marks_the_first_row_as_default() {
    let claude = ScriptedClaude::with_models(concat!(
        r#"[{"value":"first","displayName":"First","description":"First fixture"},"#,
        r#"{"value":"second","displayName":"Second","description":"Second fixture"}]"#,
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-first-row-default", state_dir.path()).await;

    let catalog = client.list_models().await.expect("discover Claude Models");
    assert_eq!(
        claude_catalog(&catalog)
            .models
            .iter()
            .filter(|model| model.is_default)
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>(),
        ["first"],
        "without the CLI's recommended row, the first row stands in as the default"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn claude_models_are_listed_beside_another_hosted_provider() {
    let claude = ScriptedClaude::with_models(CLAUDE_MODELS);
    let (other, _other_control) = ControlledProvider::with_provider(
        ProviderId::new("other"),
        vec![ModelDescriptor {
            provider: ProviderId::new("other"),
            id: ModelId::new("other-model"),
            display_name: "Other Fixture".to_owned(),
            description: String::new(),
            is_default: true,
            availability: ModelAvailability::Available,
            options: Vec::new(),
        }],
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "claude-side-by-side").expect("configure server"),
        vec![other, Arc::new(ClaudeRuntime::new(claude.executable()))],
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "claude-side-by-side")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let catalog = client.list_models().await.expect("discover both catalogs");
    assert_eq!(
        catalog
            .providers
            .iter()
            .map(|provider| provider.provider.to_string())
            .collect::<Vec<_>>(),
        ["other", "claude"],
        "every hosted Provider is listed, in the hosted order"
    );
    assert!(
        !claude_catalog(&catalog).models.is_empty(),
        "Claude's Models are listed alongside the other Provider's"
    );

    drop(client);
    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

/// Issue #126: a Claude Code CLI that isn't installed is a condition the user fixes outside Suru,
/// so the catalog reports it as typed unavailability and the next refresh clears it without a
/// restart.
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
    let ProviderCatalogStatus::Unavailable { reason, message } = &claude_catalog(&catalog).status
    else {
        panic!(
            "a missing Claude binary is typed unavailability, got {:?}",
            claude_catalog(&catalog).status
        );
    };
    assert_eq!(*reason, ProviderUnavailability::NotInstalled);
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
    let installed = client
        .refresh_models()
        .await
        .expect("refresh once the CLI is installed");
    assert_eq!(
        claude_catalog(&installed).status,
        ProviderCatalogStatus::Fresh,
        "the refresh re-checks the condition and finds it fixed, with no restart in between"
    );
    assert!(
        !claude_catalog(&installed).models.is_empty(),
        "the installed Provider serves its Models without a restart"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn a_discovery_the_cli_never_answers_fails_after_the_injected_timeout() {
    let claude = ScriptedClaude::new(&silent_list_models_arm());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let runtime = ClaudeRuntime::new(claude.executable())
        .with_control_request_timeout(Duration::from_millis(100))
        .with_process_exit_grace(Duration::from_millis(50));
    let (server, client) =
        hosting_runtime(runtime, "claude-silent-discovery", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let ProviderCatalogStatus::Failed { message } = &claude_catalog(&catalog).status else {
        panic!(
            "a CLI that never answers fails the Claude catalog, got {:?}",
            claude_catalog(&catalog).status
        );
    };
    assert!(
        message.contains("Claude Model discovery failed"),
        "the failure names the operation that timed out, got: {message}"
    );

    server.shutdown().await.expect("shut the server down");
}

/// The stream-json wire is an SDK implementation detail, so drift is Suru's to absorb (ADR 0010):
/// a control response of a subtype this build has never heard of is ridden out, not a failure.
#[tokio::test]
async fn a_control_response_subtype_suru_does_not_know_is_ridden_out() {
    let claude = ScriptedClaude::new(&drifting_list_models_arm(CLAUDE_MODELS));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-drifted-wire", state_dir.path()).await;

    let catalog = client.list_models().await.expect("discover Claude Models");
    assert_eq!(
        claude_catalog(&catalog).status,
        ProviderCatalogStatus::Fresh,
        "the discovery reads past the drifted response to the answer it was waiting for"
    );
    assert_eq!(claude_catalog(&catalog).models.len(), 4);

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn a_malformed_list_models_response_fails_the_catalog() {
    let claude = ScriptedClaude::new(&malformed_list_models_arm());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-malformed-models", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let ProviderCatalogStatus::Failed { message } = &claude_catalog(&catalog).status else {
        panic!(
            "a malformed response fails the Claude catalog, got {:?}",
            claude_catalog(&catalog).status
        );
    };
    assert!(
        message.contains("invalid list_models response"),
        "the failure says what the CLI got wrong, got: {message}"
    );
    assert!(claude_catalog(&catalog).models.is_empty());

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

/// The catalog after a recovery keeps answering, which [`list_models_arm`] alone would not show if
/// a first failed launch left the runtime's process registry wedged.
#[tokio::test]
async fn a_discovery_after_a_crashed_one_launches_a_fresh_process() {
    let claude = ScriptedClaude::new(&format!(
        r#"    *'"subtype":"list_models"'*)
      if [ "$attempt" -gt 1 ]; then
{}      else
        exit 9
      fi
      ;;
"#,
        r#"        printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"models":[{"value":"revived","displayName":"Revived","description":"Back after a crash"}]}}}'
"#
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&claude, "claude-crash-recovery", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    assert!(
        matches!(
            &claude_catalog(&catalog).status,
            ProviderCatalogStatus::Failed { .. }
        ),
        "a CLI that dies mid-discovery fails the catalog, got {:?}",
        claude_catalog(&catalog).status
    );

    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    assert_eq!(
        claude_catalog(&refreshed).status,
        ProviderCatalogStatus::Fresh,
        "the demand after a crash launches a fresh process, with no restart in between"
    );
    assert_eq!(claude.launches(), 2);

    server.shutdown().await.expect("shut the server down");
}
