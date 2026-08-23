//! Several Provider runtimes hosted side by side: catalog listing, per-Session
//! routing, hosted-set selection validation, and the built-in default order.

use std::sync::Arc;

use crate::{
    failing_provider_support::FailingProviderRuntime, provider_support::ControlledProvider,
};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
        AgentSelectionOperationId, CreateSessionRequest, InitialPrompt, ModelAvailability,
        ModelCatalog, ModelDescriptor, ModelId, PromptDelivery, PromptId, ProviderCatalogStatus,
        ProviderId, SessionError, SessionErrorCode, SessionId, SessionSnapshot, TurnStatus,
        UpdateAgentSelectionRequest, Workspace,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

fn hosted_model(provider: &str, model: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(model),
        display_name: model.to_owned(),
        description: String::new(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: Vec::new(),
    }
}

fn hosted_selection(provider: &str, model: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
        options: Vec::new(),
    }
}

fn create_session_request(
    workspace: &std::path::Path,
    provider: &str,
    model: &str,
) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: Some(hosted_selection(provider, model)),
        workspace: Workspace {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: format!("Work on the {provider} Provider"),
        },
    }
}

async fn create_session(
    descriptor: &suru::protocol::RuntimeDescriptor,
    request: &CreateSessionRequest,
) -> SessionSnapshot {
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session")
}

async fn read_session(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
) -> SessionSnapshot {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session")
        .error_for_status()
        .expect("Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session")
}

#[tokio::test]
async fn model_catalog_lists_every_hosted_provider_and_refreshes_each_independently() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (alpha_runtime, _alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let (beta_runtime, _beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "multi-provider-catalog-test")
            .expect("configure server"),
        vec![
            alpha_runtime,
            beta_runtime,
            Arc::new(FailingProviderRuntime),
        ],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let catalog = reqwest::Client::new()
        .get(format!("{}/v1/models", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Models")
        .error_for_status()
        .expect("Model listing succeeds")
        .json::<ModelCatalog>()
        .await
        .expect("decode Model catalog");

    let providers: Vec<_> = catalog
        .providers
        .iter()
        .map(|provider| provider.provider.as_str())
        .collect();
    assert_eq!(
        providers,
        ["alpha", "beta", "failing"],
        "the catalog lists every hosted Provider in the built-in order"
    );
    assert_eq!(catalog.providers[0].status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        catalog.providers[0].models,
        vec![hosted_model("alpha", "alpha-default")]
    );
    assert_eq!(catalog.providers[1].status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        catalog.providers[1].models,
        vec![hosted_model("beta", "beta-default")]
    );
    assert!(
        matches!(
            &catalog.providers[2].status,
            ProviderCatalogStatus::Failed { message } if message.contains("Model discovery is unavailable")
        ),
        "one Provider's failed refresh must not degrade the others, got {:?}",
        catalog.providers[2].status
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn sessions_on_different_providers_coexist_and_each_keeps_its_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (alpha_runtime, mut alpha) =
        ControlledProvider::with_provider(ProviderId::new("alpha"), Vec::new());
    let (beta_runtime, mut beta) =
        ControlledProvider::with_provider(ProviderId::new("beta"), Vec::new());
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "multi-provider-session-test")
            .expect("configure server"),
        vec![alpha_runtime, beta_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let alpha_session = create_session(
        &descriptor,
        &create_session_request(workspace.path(), "alpha", "alpha-model"),
    )
    .await;
    let mut alpha_provider_session = alpha.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("alpha-agent"),
        selection: hosted_selection("alpha", "alpha-model"),
    });
    let alpha_turn = alpha_provider_session.next_turn().await;
    assert_eq!(
        alpha_turn.selection(),
        &hosted_selection("alpha", "alpha-model"),
        "the alpha Session's Turn runs under the alpha Provider's selection"
    );
    alpha_turn.succeed();

    let beta_session = create_session(
        &descriptor,
        &create_session_request(workspace.path(), "beta", "beta-model"),
    )
    .await;
    let mut beta_provider_session = beta.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("beta-agent"),
        selection: hosted_selection("beta", "beta-model"),
    });
    let beta_turn = beta_provider_session.next_turn().await;
    assert_eq!(
        beta_turn.selection(),
        &hosted_selection("beta", "beta-model"),
        "the beta Session's Turn runs under the beta Provider's selection"
    );
    beta_turn.succeed();

    alpha_provider_session.emit(ProviderEvent::TurnCompleted);
    beta_provider_session.emit(ProviderEvent::TurnCompleted);

    let alpha_snapshot = read_session(&descriptor, alpha_session.session.id).await;
    let beta_snapshot = read_session(&descriptor, beta_session.session.id).await;
    assert_eq!(
        alpha_snapshot.session.agent_selection,
        Some(hosted_selection("alpha", "alpha-model"))
    );
    assert_eq!(
        beta_snapshot.session.agent_selection,
        Some(hosted_selection("beta", "beta-model"))
    );

    // A Session keeps the Provider it was selected with even though the other
    // Provider is hosted right here (ADR-0005).
    let rejection = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/agent-selection",
            descriptor.base_url, alpha_session.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&UpdateAgentSelectionRequest {
            operation_id: AgentSelectionOperationId::new(),
            selection: hosted_selection("beta", "beta-model"),
        })
        .send()
        .await
        .expect("attempt a cross-Provider selection");
    assert_eq!(rejection.status(), reqwest::StatusCode::CONFLICT);
    let rejection = rejection
        .json::<SessionError>()
        .await
        .expect("decode cross-Provider rejection");
    assert_eq!(
        rejection.code,
        SessionErrorCode::AgentSelectionProviderConflict
    );

    drop(alpha_provider_session);
    drop(beta_provider_session);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn agent_selections_naming_an_unhosted_provider_are_rejected() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (alpha_runtime, _alpha) =
        ControlledProvider::with_provider(ProviderId::new("alpha"), Vec::new());
    let (beta_runtime, _beta) =
        ControlledProvider::with_provider(ProviderId::new("beta"), Vec::new());
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "unhosted-provider-test").expect("configure server"),
        vec![alpha_runtime, beta_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let creation = reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&create_session_request(
            workspace.path(),
            "gamma",
            "gamma-model",
        ))
        .send()
        .await
        .expect("attempt Session creation on an unhosted Provider");
    assert_eq!(creation.status(), reqwest::StatusCode::CONFLICT);
    let creation = creation
        .json::<SessionError>()
        .await
        .expect("decode Session creation rejection");
    assert_eq!(
        creation.code,
        SessionErrorCode::AgentSelectionProviderConflict
    );

    let landing = reqwest::Client::new()
        .put(format!(
            "{}/v1/landing-agent-selection",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&hosted_selection("gamma", "gamma-model"))
        .send()
        .await
        .expect("attempt a Landing selection on an unhosted Provider");
    assert_eq!(landing.status(), reqwest::StatusCode::CONFLICT);
    let landing = landing
        .json::<SessionError>()
        .await
        .expect("decode Landing selection rejection");
    assert_eq!(
        landing.code,
        SessionErrorCode::AgentSelectionProviderConflict
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_fresh_landing_defaults_to_the_first_provider_in_the_built_in_order() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (alpha_runtime, mut alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let (beta_runtime, _beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "landing-default-test").expect("configure server"),
        vec![alpha_runtime, beta_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    reqwest::Client::new()
        .post(format!("{}/v1/models/refresh", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("refresh Model catalog")
        .error_for_status()
        .expect("Model refresh succeeds");

    let created = create_session(
        &descriptor,
        &CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start from the built-in default".to_owned(),
            },
        },
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("alpha", "alpha-default")),
        "a fresh Landing defaults to the first hosted Provider's default Model"
    );
    assert!(
        alpha.next_start().await.workspace()
            == workspace
                .path()
                .canonicalize()
                .expect("canonicalize Workspace"),
        "the defaulted Session routes to the first hosted Provider"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_stored_session_on_an_unhosted_provider_fails_its_next_prompt_legibly() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "unhosted-restart-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let (alpha_runtime, mut alpha) =
        ControlledProvider::with_provider(ProviderId::new("alpha"), Vec::new());
    let original = server::spawn_with_providers(config.clone(), vec![alpha_runtime])
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let session = create_session(
        &original_descriptor,
        &create_session_request(workspace.path(), "alpha", "alpha-model"),
    )
    .await;
    let mut provider_session = alpha.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("alpha-agent"),
        selection: hosted_selection("alpha", "alpha-model"),
    });
    provider_session.next_turn().await.succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session(&original_descriptor, session.session.id).await;
            if snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the alpha Turn settles before the restart");
    drop(provider_session);
    original.shutdown().await.expect("stop original server");

    let (beta_runtime, _beta) =
        ControlledProvider::with_provider(ProviderId::new("beta"), Vec::new());
    let replacement = server::spawn_with_providers(config, vec![beta_runtime])
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            replacement_descriptor.base_url, session.session.id
        ))
        .bearer_auth(&replacement_descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue on a Provider this server no longer hosts".to_owned(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt after the restart")
        .error_for_status()
        .expect("Prompt admission succeeds");

    let failed = timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session(&replacement_descriptor, session.session.id).await;
            if snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Prompt's Turn settles as failed");
    let error_text = failed
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the failed Turn carries an Error Activity");
    assert!(
        error_text.contains("`alpha`") && error_text.contains("not hosted"),
        "the failure names the missing Provider, got {error_text:?}"
    );

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}
