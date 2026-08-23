//! Copilot's Model catalog: discovery through the shared harness process, the Model Options each
//! Model advertises, and Copilot's place beside another hosted Provider.

use std::sync::Arc;

use crate::{
    provider_support::ControlledProvider,
    server_support,
    support::{ScriptedCopilot, connect_arm},
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionSelection, ModelOptionValue,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
    },
    provider::CopilotRuntime,
    server::{self, ServerConfig},
};
/// One Model of every shape the normalization has to tell apart: Copilot's routed default, a Model
/// whose context tiers ride its tiered pricing, one that declares its tiers outright, and one an
/// administrator's policy has disabled.
const COPILOT_MODELS: &str = concat!(
    r#"[{"id":"auto","name":"Auto","capabilities":{},"modelPickerCategory":"versatile"},"#,
    r#"{"id":"claude-fixture","name":"Claude Fixture","capabilities":{},"#,
    r#""modelPickerCategory":"powerful","supportedReasoningEfforts":["low","high"],"#,
    r#""defaultReasoningEffort":"high","#,
    r#""billing":{"tokenPrices":{"maxPromptTokens":128000,"#,
    r#""longContext":{"maxPromptTokens":512000}}}},"#,
    r#"{"id":"hosted-fixture","name":"Hosted Fixture","capabilities":{},"#,
    r#""supportedContextTiers":["default","long_context"]},"#,
    r#"{"id":"blocked-fixture","name":"Blocked Fixture","capabilities":{},"#,
    r#""policy":{"state":"disabled"}}]"#,
);

async fn connect(state_dir: &std::path::Path, name: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, name).expect("configure client"),
    )
    .await
    .expect("connect client");
    server_support::receive_initial_state(&mut client).await;
    client
}

fn copilot_catalog(catalog: &ModelCatalog) -> &ProviderModelCatalog {
    catalog
        .providers
        .iter()
        .find(|provider| provider.provider == ProviderId::new("copilot"))
        .expect("the catalog lists the Copilot Provider")
}

fn model<'a>(catalog: &'a ProviderModelCatalog, id: &str) -> &'a ModelDescriptor {
    catalog
        .models
        .iter()
        .find(|model| model.id == ModelId::new(id))
        .unwrap_or_else(|| panic!("the Copilot catalog lists Model `{id}`"))
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
async fn copilot_models_reach_the_catalog_with_reasoning_effort_and_context_tier_options() {
    let copilot = ScriptedCopilot::with_models(COPILOT_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-model-catalog").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-model-catalog").await;

    let catalog = client.list_models().await.expect("discover Copilot Models");
    let copilot_models = copilot_catalog(&catalog);
    assert_eq!(copilot_models.status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        copilot_models
            .models
            .iter()
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>(),
        [
            "auto",
            "claude-fixture",
            "hosted-fixture",
            "blocked-fixture"
        ]
    );

    let routed = model(copilot_models, "auto");
    assert!(routed.is_default, "Copilot's routed Model is the default");
    assert_eq!(routed.display_name, "Auto");
    assert!(
        routed.options.is_empty(),
        "a Model that advertises no dimension carries no Model Options"
    );

    let claude = model(copilot_models, "claude-fixture");
    assert!(!claude.is_default, "exactly one Model is the default");
    assert_eq!(
        claude
            .options
            .iter()
            .map(|option| option.role)
            .collect::<Vec<_>>(),
        [ModelOptionRole::ReasoningEffort, ModelOptionRole::Context],
        "context tier carries its own typed role rather than riding the untyped one"
    );
    assert_eq!(
        choices(claude, "reasoning_effort"),
        (vec!["low".to_owned(), "high".to_owned()], "high".to_owned())
    );
    assert_eq!(
        choices(claude, "context_tier"),
        (
            vec!["default".to_owned(), "long_context".to_owned()],
            "default".to_owned()
        ),
        "tiered pricing is what tells Suru the Model offers an extended context tier"
    );
    assert_eq!(
        claude.default_agent_selection().options,
        vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("context_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("default"),
                },
            },
        ]
    );

    let hosted = model(copilot_models, "hosted-fixture");
    assert_eq!(
        hosted
            .options
            .iter()
            .map(|option| option.id.to_string())
            .collect::<Vec<_>>(),
        ["context_tier"],
        "a Model that declares its tiers outright still gets the context-tier Option"
    );

    let blocked = model(copilot_models, "blocked-fixture");
    assert_eq!(
        blocked.availability,
        ModelAvailability::Unavailable,
        "a Model policy disables stays visible but unselectable"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn discovery_and_refresh_share_one_stdio_server_process() {
    let copilot = ScriptedCopilot::with_models(COPILOT_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-shared-process").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-shared-process").await;

    client.list_models().await.expect("discover Copilot Models");
    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    assert_eq!(
        copilot_catalog(&refreshed).models.len(),
        4,
        "the refreshed catalog is asked of the CLI again, not replayed from a cache"
    );

    assert_eq!(
        copilot.launches(),
        1,
        "discovery reuses the shared harness process rather than launching a throwaway one"
    );
    assert_eq!(
        copilot.arguments(),
        ["--server", "--stdio", "--no-auto-update"],
        "the CLI is launched in its stdio server mode"
    );
    assert_eq!(
        copilot.methods(),
        ["connect", "models.list", "models.list"],
        "one handshake serves both discoveries"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn copilot_models_are_listed_beside_another_hosted_provider() {
    let copilot = ScriptedCopilot::with_models(COPILOT_MODELS);
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
        ServerConfig::new(state_dir.path(), "copilot-side-by-side").expect("configure server"),
        vec![other, Arc::new(CopilotRuntime::new(copilot.executable()))],
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-side-by-side").await;

    let catalog = client.list_models().await.expect("discover both catalogs");
    assert_eq!(
        catalog
            .providers
            .iter()
            .map(|provider| provider.provider.to_string())
            .collect::<Vec<_>>(),
        ["other", "copilot"],
        "every hosted Provider is listed, in the built-in order"
    );
    assert!(
        !copilot_catalog(&catalog).models.is_empty(),
        "Copilot's Models are listed alongside the other Provider's"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

/// A catalog Copilot's routed Model is missing from, whose first Model is policy-disabled, and
/// whose second names a default reasoning effort it does not actually offer.
const AWKWARD_MODELS: &str = concat!(
    r#"[{"id":"blocked-fixture","name":"Blocked Fixture","capabilities":{},"#,
    r#""policy":{"state":"disabled"},"supportedReasoningEfforts":["low"],"#,
    r#""defaultReasoningEffort":"low"},"#,
    r#"{"id":"first-usable","name":"First Usable","capabilities":{},"#,
    r#""supportedReasoningEfforts":["low","high"],"defaultReasoningEffort":"withdrawn"},"#,
    r#"{"id":"later-fixture","name":"Later Fixture","capabilities":{}}]"#,
);

#[tokio::test]
async fn a_catalog_without_the_routed_model_defaults_to_the_first_usable_one() {
    let copilot = ScriptedCopilot::with_models(AWKWARD_MODELS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-awkward-catalog").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-awkward-catalog").await;

    let catalog = client.list_models().await.expect("discover Copilot Models");
    let copilot_models = copilot_catalog(&catalog);
    assert_eq!(copilot_models.status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        copilot_models
            .models
            .iter()
            .filter(|model| model.is_default)
            .map(|model| model.id.to_string())
            .collect::<Vec<_>>(),
        ["first-usable"],
        "the default skips the Model policy disabled"
    );
    assert_eq!(
        choices(model(copilot_models, "first-usable"), "reasoning_effort"),
        (vec!["low".to_owned(), "high".to_owned()], "low".to_owned()),
        "a default effort Copilot no longer offers falls back to one it does, \
         rather than rejecting the catalog"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_harness_that_dies_mid_discovery_fails_the_catalog_and_the_next_refresh_relaunches() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}",
        connect_arm(),
        r#"    *'"method":"models.list"'*)
      if [ "$attempt" -gt 1 ]; then
        reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"models":[{"id":"auto","name":"Auto","capabilities":{}}]}}'
      else
        exit 9
      fi
      ;;
"#,
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-harness-crash").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-harness-crash").await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let ProviderCatalogStatus::Failed { message } = &copilot_catalog(&catalog).status else {
        panic!(
            "a harness that dies mid-discovery fails the Copilot catalog, got {:?}",
            copilot_catalog(&catalog).status
        );
    };
    assert!(
        message.contains("Copilot Model discovery failed"),
        "the failure names the operation that lost the process, got: {message}"
    );

    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    assert_eq!(
        copilot_catalog(&refreshed).status,
        ProviderCatalogStatus::Fresh,
        "the demand after a crash launches a fresh process, with no restart in between"
    );
    assert_eq!(copilot.launches(), 2);

    server.shutdown().await.expect("shut the server down");
}
