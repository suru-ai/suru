//! Several Provider runtimes hosted side by side: catalog listing, per-Session
//! routing, hosted-set selection validation, and the built-in default order.

use std::sync::Arc;

use crate::{
    failing_provider_support::FailingProviderRuntime,
    provider_support::ControlledProvider,
    support::{
        create_session, hosted_model, hosted_selection, list_catalog, read_session, refresh_catalog,
    },
};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelectionOperationId,
        CreateSessionRequest, InitialPrompt, ModelCatalog, PromptDelivery, PromptId,
        ProviderCatalogStatus, ProviderId, ProviderUnavailability, SessionError, SessionErrorCode,
        TurnStatus, UpdateAgentSelectionRequest,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

fn create_session_request(
    workspace: &std::path::Path,
    provider: &str,
    model: &str,
) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: Some(hosted_selection(provider, model)),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: format!("Work on the {provider} Provider"),
            skill_invocations: Vec::new(),
        },
    }
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
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start from the built-in default".to_owned(),
                skill_invocations: Vec::new(),
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
        alpha.next_start().await.execution_directory()
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
                skill_invocations: Vec::new(),
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

#[tokio::test]
async fn a_stale_landing_selection_on_an_unhosted_provider_yields_to_the_built_in_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "stale-landing-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let (alpha_runtime, _alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let original = server::spawn_with_providers(config.clone(), vec![alpha_runtime])
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    reqwest::Client::new()
        .put(format!(
            "{}/v1/landing-agent-selection",
            original_descriptor.base_url
        ))
        .bearer_auth(&original_descriptor.token)
        .json(&hosted_selection("alpha", "alpha-default"))
        .send()
        .await
        .expect("confirm the Landing selection")
        .error_for_status()
        .expect("Landing selection confirmation succeeds");
    original.shutdown().await.expect("stop original server");

    let (beta_runtime, _beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let replacement = server::spawn_with_providers(config, vec![beta_runtime])
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    reqwest::Client::new()
        .post(format!(
            "{}/v1/models/refresh",
            replacement_descriptor.base_url
        ))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("refresh Model catalog")
        .error_for_status()
        .expect("Model refresh succeeds");

    let created = create_session(
        &replacement_descriptor,
        &CreateSessionRequest {
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start despite the stale Landing selection".to_owned(),
                skill_invocations: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("beta", "beta-default")),
        "a Landing selection naming an unhosted Provider yields to the hosted default"
    );

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}

#[tokio::test]
async fn the_landing_default_falls_to_the_next_provider_when_the_first_has_no_catalog() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (beta_runtime, _beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "default-fallthrough-test").expect("configure server"),
        vec![Arc::new(FailingProviderRuntime), beta_runtime],
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
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Default past a Provider with no catalog".to_owned(),
                skill_invocations: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("beta", "beta-default")),
        "the Landing default falls past a Provider whose catalog holds no default"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_unavailable_provider_is_listed_with_its_reason_until_a_refresh_finds_it_fixed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (alpha_runtime, _alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let alpha_handle = Arc::clone(&alpha_runtime);
    alpha_handle.set_unavailable(Some(ProviderUnavailability::NotInstalled));
    let (beta_runtime, _beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "provider-unavailability-test")
            .expect("configure server"),
        vec![alpha_runtime, beta_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let catalog = refresh_catalog(&descriptor).await;
    assert_eq!(
        catalog.providers[0].provider,
        ProviderId::new("alpha"),
        "an unavailable Provider keeps its place in the catalog"
    );
    let ProviderCatalogStatus::Unavailable { reason, message } = &catalog.providers[0].status
    else {
        panic!(
            "the unavailable Provider carries a typed reason, got {:?}",
            catalog.providers[0].status
        );
    };
    assert_eq!(*reason, ProviderUnavailability::NotInstalled);
    assert!(
        message.contains("alpha"),
        "the reason keeps the Provider's own account of the condition, got {message:?}"
    );
    assert_eq!(catalog.providers[1].status, ProviderCatalogStatus::Fresh);

    // A listing with nothing cached to serve waits for its discovery, so this
    // reports the condition the settled re-check found. The listing path that
    // answers from cache is where a re-check could hand the Models back out;
    // `an_unavailable_provider_keeps_its_condition_through_a_re_check_of_its_cached_models`
    // holds that line.
    let listed = list_catalog(&descriptor).await;
    assert!(
        matches!(
            listed.providers[0].status,
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotInstalled,
                ..
            }
        ),
        "a re-check in flight does not clear the condition, got {:?}",
        listed.providers[0].status
    );

    // The user installs the missing CLI and refreshes; no restart involved.
    alpha_handle.set_unavailable(None);
    let repaired = refresh_catalog(&descriptor).await;
    assert_eq!(repaired.providers[0].status, ProviderCatalogStatus::Fresh);
    assert_eq!(
        repaired.providers[0].models,
        vec![hosted_model("alpha", "alpha-default")],
        "the fixed Provider serves its Models again"
    );

    server.shutdown().await.expect("shut down server");
}

/// A listing serves the cached catalog and re-arms a background refresh, so it
/// is the path on which a Provider that has gone unavailable could hand its
/// Models back out — for the length of every re-check — if the refresh in
/// flight were allowed to outrank the condition.
#[tokio::test]
async fn an_unavailable_provider_keeps_its_condition_through_a_re_check_of_its_cached_models() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (alpha_runtime, _alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let alpha_handle = Arc::clone(&alpha_runtime);
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "unavailable-re-check-test").expect("configure server"),
        vec![alpha_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    refresh_catalog(&descriptor).await;
    alpha_handle.set_unavailable(Some(ProviderUnavailability::NotSignedIn));
    refresh_catalog(&descriptor).await;

    let listed = list_catalog(&descriptor).await;
    assert!(
        matches!(
            listed.providers[0].status,
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotSignedIn,
                ..
            }
        ),
        "the re-check a listing arms must not clear the condition, got {:?}",
        listed.providers[0].status
    );
    assert_eq!(
        listed.providers[0].models,
        vec![hosted_model("alpha", "alpha-default")],
        "the Models stay on show — unselectable — rather than disappearing"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_fresh_landing_default_skips_an_unavailable_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (alpha_runtime, _alpha) = ControlledProvider::with_provider(
        ProviderId::new("alpha"),
        vec![hosted_model("alpha", "alpha-default")],
    );
    let alpha_handle = Arc::clone(&alpha_runtime);
    let (beta_runtime, mut beta) = ControlledProvider::with_provider(
        ProviderId::new("beta"),
        vec![hosted_model("beta", "beta-default")],
    );
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "unavailable-landing-default-test")
            .expect("configure server"),
        vec![alpha_runtime, beta_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    // Alpha's Models are cached from a refresh that found it healthy; the user
    // then signs out of it, so the catalog still holds Models the Landing
    // default must nonetheless pass over.
    refresh_catalog(&descriptor).await;
    alpha_handle.set_unavailable(Some(ProviderUnavailability::NotSignedIn));
    let catalog = refresh_catalog(&descriptor).await;
    assert!(
        matches!(
            catalog.providers[0].status,
            ProviderCatalogStatus::Unavailable {
                reason: ProviderUnavailability::NotSignedIn,
                ..
            }
        ),
        "a Provider that goes unavailable reports the condition over its stale catalog, got {:?}",
        catalog.providers[0].status
    );
    assert_eq!(
        catalog.providers[0].models,
        vec![hosted_model("alpha", "alpha-default")],
        "an unavailable Provider keeps the Models it last served on show"
    );

    let created = create_session(
        &descriptor,
        &CreateSessionRequest {
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start on a Provider that can actually work".to_owned(),
                skill_invocations: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("beta", "beta-default")),
        "a fresh Landing defaults past the Provider the user cannot use yet"
    );
    assert!(
        beta.next_start().await.execution_directory()
            == workspace
                .path()
                .canonicalize()
                .expect("canonicalize Workspace"),
        "the defaulted Session routes to the first available Provider"
    );

    server.shutdown().await.expect("shut down server");
}

/// Issue #116: a Session that opens on a Provider the user cannot use yet still
/// fails legibly, and the typed condition leads the failure so the Turn says
/// what to do about it rather than only that a launch went wrong.
#[tokio::test]
async fn a_turn_that_starts_on_an_unavailable_provider_fails_with_the_typed_condition() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (alpha_runtime, mut alpha) =
        ControlledProvider::with_provider(ProviderId::new("alpha"), Vec::new());
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), "unavailable-startup-test").expect("configure server"),
        vec![alpha_runtime],
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let session = create_session(
        &descriptor,
        &create_session_request(workspace.path(), "alpha", "alpha-model"),
    )
    .await;
    alpha.next_start().await.fail_unavailable(
        ProviderUnavailability::NotInstalled,
        "could not launch the alpha CLI `alpha`: No such file or directory",
    );

    let failed = timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session(&descriptor, session.session.id).await;
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
    .expect("the Turn settles as failed");
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
        error_text.starts_with("Provider `alpha` is not installed"),
        "the failure leads with the condition the user fixes, got {error_text:?}"
    );
    assert!(
        error_text.contains("No such file or directory"),
        "the Provider's own account of the condition survives, got {error_text:?}"
    );

    server.shutdown().await.expect("shut down server");
}
