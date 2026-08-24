//! A Provider the user turned off: never consulted, absent from what Suru
//! offers, and legible when a Session bound to it is prompted anyway.
//!
//! These tests sit beside the unavailability tests in `multi_provider`, because
//! disabled behavior is defined largely by staying distinct from them: an
//! unavailable Provider keeps its place with its reason on show, while a
//! disabled one reports only that Suru never looked.
//!
//! The Provider doubles are hosted under the real Provider identities, because
//! the `enabled` Settings are a compile-time table keyed by them — a Provider
//! the schema does not name has no Setting to turn off.

use std::{path::Path, sync::Arc};

use crate::{
    provider_support::{ControlledProvider, ControlledProviderRuntime},
    support::{
        create_session, hosted_model, hosted_selection, list_catalog, read_session, refresh_catalog,
    },
};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, CreateSessionRequest,
        InitialPrompt, ModelCatalog, PromptDelivery, PromptId, ProviderCatalogStatus, ProviderId,
        RuntimeDescriptor, SessionId, SettingMutation, SettingsSnapshot, TurnStatus, Workspace,
    },
    provider::ProviderEvent,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

/// A Provider double serving one default Model, under a real Provider identity
/// so its `enabled` Setting exists.
fn provider(id: &str) -> (Arc<ControlledProviderRuntime>, ControlledProvider) {
    ControlledProvider::with_provider(
        ProviderId::new(id),
        vec![hosted_model(id, &format!("{id}-default"))],
    )
}

/// A config root whose Config Document turns `disabled` off before the server
/// ever starts, which is how a user who never wants a Provider consulted
/// configures Suru.
fn config_dir_disabling(disabled: &[&str]) -> tempfile::TempDir {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let pins = disabled
        .iter()
        .map(|provider| format!("    \"{provider}\": {{ \"enabled\": false }}"))
        .collect::<Vec<_>>()
        .join(",\n");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        format!("{{\n  \"provider\": {{\n{pins}\n  }}\n}}\n"),
    )
    .expect("write Config Document");
    config_dir
}

/// The catalog entry for one Provider, which stays present for every hosted
/// Provider so a client is never left guessing whether one vanished.
fn provider_catalog(catalog: &ModelCatalog, id: &str) -> suru::protocol::ProviderModelCatalog {
    catalog
        .providers
        .iter()
        .find(|entry| entry.provider == ProviderId::new(id))
        .unwrap_or_else(|| {
            panic!("the catalog keeps a row for every hosted Provider, missing {id}")
        })
        .clone()
}

/// Changes a Setting the way a client does: the typed mutation over the real
/// protocol, answered with the snapshot the edit left in force.
async fn mutate_setting(
    descriptor: &RuntimeDescriptor,
    mutation: SettingMutation,
) -> SettingsSnapshot {
    reqwest::Client::new()
        .post(format!("{}/v1/settings", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&mutation)
        .send()
        .await
        .expect("mutate a Setting")
        .error_for_status()
        .expect("the mutation is accepted")
        .json::<SettingsSnapshot>()
        .await
        .expect("decode the settings the edit left in force")
}

fn session_request(
    workspace: &Path,
    selection: Option<AgentSelection>,
    text: &str,
) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: selection,
        workspace: Workspace {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
        },
    }
}

async fn admit_prompt(descriptor: &RuntimeDescriptor, session_id: SessionId, text: &str) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt")
        .error_for_status()
        .expect("Prompt admission succeeds");
}

/// The text of the Error Activity a failed Turn carries, once it has settled.
async fn failed_turn_error(descriptor: &RuntimeDescriptor, session_id: SessionId) -> String {
    let snapshot = timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session(descriptor, session_id).await;
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
    snapshot
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the failed Turn carries an Error Activity")
}

async fn spawn(
    state_dir: &Path,
    config_dir: Option<&Path>,
    channel: &str,
    runtimes: Vec<Arc<ControlledProviderRuntime>>,
) -> RunningServer {
    let mut config = ServerConfig::new(state_dir, channel).expect("configure server");
    if let Some(config_dir) = config_dir {
        config = config.with_config_dir(config_dir);
    }
    server::spawn_with_providers(
        config,
        runtimes
            .into_iter()
            .map(|runtime| runtime as Arc<dyn suru::provider::ProviderRuntime>)
            .collect(),
    )
    .await
    .expect("spawn server")
}

#[tokio::test]
async fn a_provider_pinned_disabled_is_never_consulted_and_reports_that_it_is_disabled() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_dir_disabling(&["codex"]);
    let (codex_runtime, mut codex) = provider("codex");
    let (copilot_runtime, _copilot) = provider("copilot");
    let codex_handle = Arc::clone(&codex_runtime);
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-never-consulted",
        vec![codex_runtime, copilot_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    for catalog in [
        list_catalog(&descriptor).await,
        refresh_catalog(&descriptor).await,
        list_catalog(&descriptor).await,
    ] {
        let codex = provider_catalog(&catalog, "codex");
        assert_eq!(
            codex.status,
            ProviderCatalogStatus::Disabled,
            "a Provider the user turned off says so rather than naming a condition Suru never looked for"
        );
        assert_eq!(
            codex.models,
            [],
            "a Provider Suru never asked for Models offers none"
        );
        let copilot = provider_catalog(&catalog, "copilot");
        assert_ne!(
            copilot.status,
            ProviderCatalogStatus::Disabled,
            "turning one Provider off leaves the others alone"
        );
        assert_eq!(
            copilot.models,
            vec![hosted_model("copilot", "copilot-default")],
            "the Provider the user kept still serves its Models"
        );
    }

    assert_eq!(
        codex_handle.model_discoveries(),
        0,
        "a disabled Provider is never asked for its Models, so no process starts on its behalf"
    );
    assert!(
        codex.try_next_start().is_none(),
        "a disabled Provider is never asked to begin a Session"
    );

    server.shutdown().await.expect("shut down server");
}

/// A listing serves the cached catalog and re-arms a background refresh, so it
/// is the path on which a Provider that has just been turned off could hand its
/// Models back out — for the length of every re-check — if the refresh in
/// flight were allowed to outrank the Setting.
#[tokio::test]
async fn disabling_outranks_a_refresh_a_listing_arms_over_the_cached_models() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (codex_runtime, _codex) = provider("codex");
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-outranks-refresh",
        vec![codex_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    refresh_catalog(&descriptor).await;
    mutate_setting(
        &descriptor,
        SettingMutation::ProviderCodexEnabled { value: Some(false) },
    )
    .await;

    let listed = provider_catalog(&list_catalog(&descriptor).await, "codex");
    assert_eq!(listed.status, ProviderCatalogStatus::Disabled);
    assert_eq!(
        listed.models,
        [],
        "the Models a disabled Provider once served are not on offer while it is off"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_fresh_landing_default_passes_over_a_disabled_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_dir_disabling(&["codex"]);
    let (codex_runtime, _codex) = provider("codex");
    let (copilot_runtime, mut copilot) = provider("copilot");
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-landing-default",
        vec![codex_runtime, copilot_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();
    refresh_catalog(&descriptor).await;

    let created = create_session(
        &descriptor,
        &session_request(workspace.path(), None, "Start on a Provider I actually use"),
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("copilot", "copilot-default")),
        "the built-in default order takes the first enabled Provider"
    );
    assert_eq!(
        copilot.next_start().await.workspace(),
        workspace
            .path()
            .canonicalize()
            .expect("canonicalize Workspace"),
        "the defaulted Session routes to the Provider the user left on"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_persisted_landing_selection_naming_a_disabled_provider_yields_to_the_built_in_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let base = ServerConfig::new(state_dir.path(), "enablement-stale-landing")
        .expect("configure server")
        .with_data_dir(data_dir.path())
        .with_config_dir(config_dir.path());

    let (codex_runtime, _codex) = provider("codex");
    let (copilot_runtime, _copilot) = provider("copilot");
    let original = server::spawn_with_providers(
        base.clone(),
        vec![
            codex_runtime as Arc<dyn suru::provider::ProviderRuntime>,
            copilot_runtime,
        ],
    )
    .await
    .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    reqwest::Client::new()
        .put(format!(
            "{}/v1/landing-agent-selection",
            original_descriptor.base_url
        ))
        .bearer_auth(&original_descriptor.token)
        .json(&hosted_selection("codex", "codex-default"))
        .send()
        .await
        .expect("confirm the Landing selection")
        .error_for_status()
        .expect("Landing selection confirmation succeeds");
    // Turned off after the Landing already remembered it, which is the state a
    // stale selection has to survive.
    mutate_setting(
        &original_descriptor,
        SettingMutation::ProviderCodexEnabled { value: Some(false) },
    )
    .await;
    original.shutdown().await.expect("stop original server");

    let (codex_runtime, _codex) = provider("codex");
    let (copilot_runtime, _copilot) = provider("copilot");
    let replacement = server::spawn_with_providers(
        base,
        vec![
            codex_runtime as Arc<dyn suru::provider::ProviderRuntime>,
            copilot_runtime,
        ],
    )
    .await
    .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    refresh_catalog(&replacement_descriptor).await;

    let created = create_session(
        &replacement_descriptor,
        &session_request(
            workspace.path(),
            None,
            "Start despite the stale Landing selection",
        ),
    )
    .await;
    assert_eq!(
        created.session.agent_selection,
        Some(hosted_selection("copilot", "copilot-default")),
        "a Landing selection naming a disabled Provider yields rather than stranding the user"
    );

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}

/// A Session that has no Agent Selection yet routes to the built-in order's
/// head, and Enablement governs that order rather than merely filtering what it
/// produced.
#[tokio::test]
async fn a_session_with_no_agent_selection_routes_to_the_first_enabled_runtime() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_dir_disabling(&["codex"]);
    // Neither double serves a Model, so nothing supplies a default Agent
    // Selection and the Session reaches the orchestrator without one.
    let (codex_runtime, mut codex) =
        ControlledProvider::with_provider(ProviderId::new("codex"), Vec::new());
    let (copilot_runtime, mut copilot) =
        ControlledProvider::with_provider(ProviderId::new("copilot"), Vec::new());
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-unselected-session",
        vec![codex_runtime, copilot_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    let created = create_session(
        &descriptor,
        &session_request(workspace.path(), None, "Route me somewhere that works"),
    )
    .await;
    assert_eq!(
        created.session.agent_selection, None,
        "this Session reaches the orchestrator with no Agent Selection to route by"
    );
    assert_eq!(
        copilot.next_start().await.workspace(),
        workspace
            .path()
            .canonicalize()
            .expect("canonicalize Workspace"),
        "the Session takes the first enabled runtime, not the first hosted one"
    );
    assert!(
        codex.try_next_start().is_none(),
        "the disabled Provider at the head of the order is never asked to begin a Session"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn prompting_a_session_bound_to_a_disabled_provider_fails_its_turn_naming_the_setting() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_dir_disabling(&["codex"]);
    let (codex_runtime, mut codex) = provider("codex");
    let codex_handle = Arc::clone(&codex_runtime);
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-prompt-fails",
        vec![codex_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    let created = create_session(
        &descriptor,
        &session_request(
            workspace.path(),
            Some(hosted_selection("codex", "codex-default")),
            "Work on a Provider I turned off",
        ),
    )
    .await;

    let error = failed_turn_error(&descriptor, created.session.id).await;
    assert!(
        error.contains("`codex` is disabled"),
        "the failure says the Provider is disabled rather than unavailable, got {error:?}"
    );
    assert!(
        error.contains("provider.codex.enabled"),
        "the failure names the Setting, so the fix reads as one inside Suru, got {error:?}"
    );
    assert!(
        codex.try_next_start().is_none(),
        "the failing Prompt never reached the Provider"
    );
    assert_eq!(codex_handle.model_discoveries(), 0);

    // Disabling a Provider never costs the user their history.
    let snapshot = read_session(&descriptor, created.session.id).await;
    assert_eq!(snapshot.session.id, created.session.id);
    assert_eq!(snapshot.messages.len(), 1, "the Prompt is still on record");

    server.shutdown().await.expect("shut down server");
}

/// Disabling is prospective: it governs what Suru does next rather than what it
/// is doing, so a settings toggle never destroys work in progress.
#[tokio::test]
async fn disabling_mid_turn_lets_the_running_turn_settle_and_the_next_prompt_is_what_fails() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (codex_runtime, mut codex) = provider("codex");
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-mid-turn",
        vec![codex_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    let created = create_session(
        &descriptor,
        &session_request(
            workspace.path(),
            Some(hosted_selection("codex", "codex-default")),
            "Start work I do not want destroyed",
        ),
    )
    .await;
    let mut session = codex.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: hosted_selection("codex", "codex-default"),
    });
    session.next_turn().await.succeed();

    mutate_setting(
        &descriptor,
        SettingMutation::ProviderCodexEnabled { value: Some(false) },
    )
    .await;

    // The Turn that was already running settles as it always would.
    session.emit(ProviderEvent::TurnCompleted);
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session(&descriptor, created.session.id).await;
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
    .expect("the Turn under way when the Provider was disabled runs to Settle");

    admit_prompt(&descriptor, created.session.id, "Now begin another Turn").await;
    let error = failed_turn_error(&descriptor, created.session.id).await;
    assert!(
        error.contains("provider.codex.enabled"),
        "the Turn after the toggle is the one that fails, got {error:?}"
    );

    drop(session);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn enabling_a_provider_disabled_at_startup_produces_its_models_without_a_manual_refresh() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_dir_disabling(&["codex"]);
    let (codex_runtime, _codex) = provider("codex");
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-enable-discovers",
        vec![codex_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    assert_eq!(
        provider_catalog(&list_catalog(&descriptor).await, "codex").status,
        ProviderCatalogStatus::Disabled
    );

    mutate_setting(
        &descriptor,
        SettingMutation::ProviderCodexEnabled { value: Some(true) },
    )
    .await;

    let models = timeout(Duration::from_secs(1), async {
        loop {
            let codex = provider_catalog(&list_catalog(&descriptor).await, "codex");
            if !codex.models.is_empty() {
                return codex.models;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a Provider turned back on discovers its Models");
    assert_eq!(
        models,
        vec![hosted_model("codex", "codex-default")],
        "enabling a Provider and using it are one step"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_disable_enable_round_trip_costs_a_provider_that_already_discovered_its_models_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (codex_runtime, _codex) = provider("codex");
    let codex_handle = Arc::clone(&codex_runtime);
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-round-trip",
        vec![codex_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    refresh_catalog(&descriptor).await;
    let discovered = codex_handle.model_discoveries();
    assert!(discovered > 0, "the Provider served its catalog once");

    mutate_setting(
        &descriptor,
        SettingMutation::ProviderCodexEnabled { value: Some(false) },
    )
    .await;
    assert_eq!(
        provider_catalog(&list_catalog(&descriptor).await, "codex").status,
        ProviderCatalogStatus::Disabled
    );

    mutate_setting(
        &descriptor,
        SettingMutation::ProviderCodexEnabled { value: None },
    )
    .await;
    let restored = timeout(Duration::from_secs(1), async {
        loop {
            let codex = provider_catalog(&list_catalog(&descriptor).await, "codex");
            if !codex.models.is_empty() {
                return codex.models;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the round trip restores the Provider");
    assert_eq!(restored, vec![hosted_model("codex", "codex-default")]);

    server.shutdown().await.expect("shut down server");
}

/// The server's requirement of at least one Provider runtime is about what the
/// build hosts, not what the user enabled, so disabling every one of them is
/// permitted and leaves a Landing with nothing to select.
#[tokio::test]
async fn every_provider_disabled_leaves_nothing_selectable_and_no_startup_failure() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_dir_disabling(&["codex", "copilot"]);
    let (codex_runtime, _codex) = provider("codex");
    let (copilot_runtime, _copilot) = provider("copilot");
    let server = spawn(
        state_dir.path(),
        Some(config_dir.path()),
        "enablement-all-disabled",
        vec![codex_runtime, copilot_runtime],
    )
    .await;
    let descriptor = server.descriptor().clone();

    let catalog = refresh_catalog(&descriptor).await;
    for id in ["codex", "copilot"] {
        let entry = provider_catalog(&catalog, id);
        assert_eq!(entry.status, ProviderCatalogStatus::Disabled);
        assert_eq!(entry.models, []);
    }

    let created = create_session(
        &descriptor,
        &session_request(workspace.path(), None, "There is nowhere for this to go"),
    )
    .await;
    assert_eq!(
        created.session.agent_selection, None,
        "nothing is selectable, so a new Session starts without an Agent Selection"
    );
    let error = failed_turn_error(&descriptor, created.session.id).await;
    assert!(
        error.contains("every Provider is disabled"),
        "the state is recoverable from inside the app rather than fatal, got {error:?}"
    );

    server.shutdown().await.expect("shut down server");
}
