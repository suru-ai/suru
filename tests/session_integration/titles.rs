//! Deriving a Session's Title and Emoji from its first Prompt, through an Errand.

use crate::{
    provider_support::ControlledProvider,
    server_support::{
        assert_no_title_reaches, catalog_changes_through_title, config_root_pinning,
        next_derived_title, open_catalog_stream,
    },
    support::{hosted_model, hosted_selection},
};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        AgentId, AgentIdentity, AgentSelection, CreateSessionRequest, InitialPrompt,
        ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole,
        ModelOptionSelection, ModelOptionValue, PromptId, ProviderId, SessionCatalogChange,
        SessionId, SessionListItem, SessionTitleChanged, TitleErrand, Workspace,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";
/// The Model this Provider declares its Errands run at — never the one a
/// Session converses with, and never the Provider's default.
const ERRAND_MODEL: &str = "controlled-errand";
const EFFORT_OPTION: &str = "reasoning_effort";

fn create_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: Some(hosted_selection(PROVIDER, MODEL)),
        workspace: Workspace {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

fn titling_provider() -> (
    std::sync::Arc<crate::provider_support::ControlledProviderRuntime>,
    ControlledProvider,
) {
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL)],
    )
}

/// A Provider serving both the Model a Session converses with and the one it
/// declares its Errands run at. That second Model publishes its reasoning
/// efforts in the Provider's own order, so nothing about them says which is the
/// least — which is why the runtime declares the whole Selection rather than a
/// Model identifier.
fn errand_titling_provider() -> (
    std::sync::Arc<crate::provider_support::ControlledProviderRuntime>,
    ControlledProvider,
) {
    let effort = |id: &str| ModelOptionChoice {
        id: ModelOptionChoiceId::new(id),
        label: id.to_owned(),
        description: None,
        availability: ModelAvailability::Available,
    };
    let errand_model = ModelDescriptor {
        provider: ProviderId::new(PROVIDER),
        id: ModelId::new(ERRAND_MODEL),
        display_name: ERRAND_MODEL.to_owned(),
        description: String::new(),
        // The default Model is the one a user converses with, and this is not
        // it: the two questions are kept apart.
        is_default: false,
        availability: ModelAvailability::Available,
        options: vec![ModelOptionDescriptor {
            id: ModelOptionId::new(EFFORT_OPTION),
            label: "Reasoning effort".to_owned(),
            description: None,
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices: vec![effort("thorough"), effort("brisk")],
                default: ModelOptionChoiceId::new("thorough"),
            },
        }],
    };
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL), errand_model],
    )
}

/// The Errand Selection that Provider declares: its Errand Model at the least
/// effort it publishes, which only the Provider itself can name.
fn declared_errand_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new(PROVIDER),
        model: ModelId::new(ERRAND_MODEL),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new(EFFORT_OPTION),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("brisk"),
            },
        }],
    }
}

/// The Title one Session in a listing carries, alongside the Emoji beside it.
async fn listed_title(client: &ManagedClient, session_id: SessionId) -> (String, Option<String>) {
    let listed = client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session remains listed");
    (
        listed.title().to_owned(),
        listed.emoji().map(ToOwned::to_owned),
    )
}

async fn connected_client(state_dir: &std::path::Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    client
}

#[tokio::test]
async fn an_answered_errand_updates_the_open_session_title_and_emoji() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-derivation-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-derivation-test").await;

    let created = client
        .create_session(create_request(
            workspace.path(),
            "the reasoning group flickers when a block settles mid-run",
        ))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    assert_eq!(
        created.title,
        "the reasoning group flickers when a block settles mid-run"
    );
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe Session");
    let initial = timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("initial snapshot arrives")
        .expect("stream open")
        .expect("valid snapshot");
    assert!(matches!(
        initial,
        suru::managed_client::SessionEvent::Snapshot(_)
    ));

    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");
    assert!(
        errand
            .prompt()
            .contains("the reasoning group flickers when a block settles mid-run"),
        "the Errand carries the first Prompt: {:?}",
        errand.prompt()
    );
    assert_eq!(
        errand.selection(),
        &hosted_selection(PROVIDER, MODEL),
        "the Session's own Provider runs the Errand, at the default Model it declares nothing cheaper than"
    );
    assert_eq!(
        errand.workspace(),
        std::fs::canonicalize(workspace.path())
            .expect("canonicalize Workspace")
            .as_path(),
        "the Errand runs in the Session's Workspace"
    );
    let schema = errand.schema();
    assert!(
        schema["properties"]["title"].is_object() && schema["properties"]["emoji"].is_object(),
        "one Errand asks for both the Title and the Emoji: {schema}"
    );
    errand.succeed(json!({
        "title": "Fix reasoning group flicker",
        "emoji": "\u{1F41B}",
    }));

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Fix reasoning group flicker".to_owned(),
            emoji: Some("\u{1F41B}".to_owned()),
        }
    );
    assert_eq!(
        listed_title(&client, session_id).await,
        (
            "Fix reasoning group flicker".to_owned(),
            Some("\u{1F41B}".to_owned())
        )
    );
    let titled = client
        .read_session(session_id)
        .await
        .expect("read the Session");
    assert_eq!(titled.title, "Fix reasoning group flicker");
    assert_eq!(titled.emoji.as_deref(), Some("🐛"));
    assert!(titled.revision.0 > created.revision.0);
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscription
                .next()
                .await
                .expect("stream remains open")
                .expect("valid update");
            if let suru::managed_client::SessionEvent::Updated(update) = event
                && update.changes.iter().any(|change| matches!(change,
                    suru::protocol::SessionChange::TitleChanged { title, emoji }
                        if title == "Fix reasoning group flicker" && emoji.as_deref() == Some("🐛")
                )) {
                break;
            }
        }
    })
    .await
    .expect("Title update reaches the open Session stream");

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_runs_at_the_providers_declared_errand_selection() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = errand_titling_provider();
    runtime.declare_errand_selection(declared_errand_selection());
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-errand-selection-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-errand-selection-test").await;

    // The Session converses at the Provider's default Model, which is not the
    // Model that writes six words.
    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");
    assert_eq!(
        errand.selection(),
        &declared_errand_selection(),
        "the Errand runs at the Model and effort the Provider declared for its own Errands"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_withdrawn_errand_model_falls_back_to_the_providers_default_model() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = errand_titling_provider();
    runtime.declare_errand_selection(declared_errand_selection());
    let catalog = std::sync::Arc::clone(&runtime);
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-errand-withdrawal-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-errand-withdrawal-test").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .succeed(json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }));

    // The Provider drops the declared Model from its catalog, as a Provider
    // changing its catalog under a running server does.
    catalog.withdraw_model(&ModelId::new(ERRAND_MODEL));
    client.refresh_models().await.expect("refresh the catalog");

    client
        .create_session(create_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("a second Errand reaches the Provider");
    assert_eq!(
        errand.selection(),
        &hosted_selection(PROVIDER, MODEL),
        "a declaration naming a Model that has gone gives way to the Provider's default Model"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_provider_with_no_model_to_run_an_errand_at_is_asked_for_none() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    // A Provider serving nothing: neither a declared Errand Selection to
    // resolve nor a default Model to fall back to.
    let (runtime, mut provider) =
        ControlledProvider::with_provider(ProviderId::new(PROVIDER), Vec::new());
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-no-errand-model-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-no-errand-model-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    work_the_first_turn(&mut provider).await;

    assert!(
        provider.try_next_errand().is_none(),
        "an Errand with no Model to run it at is skipped rather than sent"
    );
    assert_eq!(
        listed_title(&client, created.session.id).await,
        ("Explain the seam".to_owned(), None),
        "the Prompt-derived Title stands"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_long_first_prompt_reaches_the_errand_as_its_opening_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-truncation-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-truncation-test").await;

    // A first Prompt states its intent up front and trails off into detail, so
    // the opening is what an Errand is given.
    let prompt = format!("{}{}", "o".repeat(2_000), "z".repeat(500));
    client
        .create_session(create_request(workspace.path(), &prompt))
        .await
        .expect("create Session");

    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");
    assert!(errand.prompt().contains(&"o".repeat(2_000)));
    assert!(
        !errand.prompt().contains('z'),
        "nothing past the first 2,000 characters is carried"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_errand_leaves_the_prompt_derived_title_standing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-failure-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-failure-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "  Explain the seam  "))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .fail("the Provider is signed out");

    assert_eq!(
        listed_title(&client, session_id).await,
        ("Explain the seam".to_owned(), None),
        "the Prompt-derived Title stands and no Emoji is invented"
    );
    assert_no_title_reaches(
        &mut client,
        session_id,
        "no Title reached the client ahead of the deletion",
    )
    .await;

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_that_never_answers_leaves_the_prompt_derived_title_standing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), "title-timeout-test").expect("configure server"),
        runtime,
        ServerTimings::default().with_errand_timeout(Duration::from_millis(20)),
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-timeout-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    // Held rather than answered: a wedged Provider is one that takes the
    // request and says nothing.
    let _wedged = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");

    assert_eq!(
        listed_title(&client, session_id).await,
        ("Explain the seam".to_owned(), None)
    );
    assert_no_title_reaches(
        &mut client,
        session_id,
        "no Title reached the client ahead of the deletion",
    )
    .await;

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_answering_outside_its_schema_yields_no_partial_title() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-schema-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-schema-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .succeed(json!({ "emoji": "\u{1F680}" }));

    assert_eq!(
        listed_title(&client, session_id).await,
        ("Explain the seam".to_owned(), None),
        "a reply Suru cannot read is discarded whole rather than half-applied"
    );
    assert_no_title_reaches(
        &mut client,
        session_id,
        "no Title reached the client ahead of the deletion",
    )
    .await;

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_badly_behaved_title_is_cleaned_up_before_it_is_stored() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-sanitizing-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-sanitizing-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .succeed(json!({
            "title": format!("\n  \"Fix   the {} flicker\"  \nand a second line", "very ".repeat(40)),
            "emoji": ":-)",
        }));

    let (title, emoji) = timeout(Duration::from_secs(2), async {
        loop {
            let listed = listed_title(&client, created.session.id).await;
            if listed.0 != "Explain the seam" {
                return listed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the derived Title lands");

    assert!(title.starts_with("Fix the very very"), "{title:?}");
    assert!(!title.contains('"'), "quotes around a Title are stripped");
    assert!(
        !title.contains("  "),
        "whitespace inside a Title is collapsed"
    );
    assert!(
        !title.contains("second line"),
        "only the first line is kept"
    );
    assert_eq!(
        title.chars().count(),
        80,
        "the Title is capped at 80 characters, ellipsis included: {title:?}"
    );
    assert!(title.ends_with('\u{2026}'));
    assert_eq!(emoji, None, "an Emoji that is not one is discarded");

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_session_with_no_agent_selection_asks_for_no_errand_at_all() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    // No Models, so a Session created without a selection acquires none.
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-no-selection-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-no-selection-test").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the seam".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    assert_eq!(created.session.agent_selection, None);

    work_the_first_turn(&mut provider).await;

    assert!(
        provider.try_next_errand().is_none(),
        "Suru asks no Errand of a Provider the user never selected"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_the_first_turn_still_yields_a_derived_title() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-interruption-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-interruption-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    let mut session = timeout(Duration::from_secs(1), provider.next_start())
        .await
        .expect("the first Turn starts a Provider Session")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: hosted_selection(PROVIDER, MODEL),
        });
    timeout(Duration::from_secs(1), session.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    // The interrupt needs the Turn on the record before it can name something
    // to stop, so the wait stays even though nothing reads the id any more.
    timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client.read_session(session_id).await.expect("read Session");
            if let Some(turn) = snapshot.turns.first() {
                return turn.id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first Turn is recorded");
    let (acknowledged, ()) = tokio::join!(client.interrupt_session(session_id), async {
        session.next_interrupt().await.succeed();
    });
    acknowledged.expect("the Provider acknowledges the interruption");
    session.emit(ProviderEvent::TurnInterrupted);

    // The Prompt was still written, so it still deserves a Title.
    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .succeed(json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }));

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Explain the Provider seam".to_owned(),
            emoji: Some("\u{1F9F5}".to_owned()),
        }
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_run_through_a_provider_side_session_creates_no_suru_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    // A harness with no one-shot mode: ADR 0011's escape hatch, exercised
    // rather than asserted in prose.
    runtime.run_errands_through_a_session();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-fallback-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;
    let client = connected_client(state_dir.path(), "title-fallback-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    let errand_start = timeout(Duration::from_secs(1), provider.next_errand_start())
        .await
        .expect("the Errand opens a Provider-side Session");
    assert!(
        errand_start.resume_state().is_none(),
        "an Errand is handed nothing to resume from, so it can continue nothing"
    );
    let mut errand_session = errand_start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: hosted_selection(PROVIDER, MODEL),
    });
    let delivered = timeout(Duration::from_secs(1), errand_session.next_turn())
        .await
        .expect("the Errand's one Prompt is delivered");
    assert!(delivered.prompt().contains("Explain the seam"));
    delivered.succeed();
    errand_session.emit(ProviderEvent::AgentMessageDelta {
        content: json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }).to_string(),
    });
    errand_session.emit(ProviderEvent::TurnCompleted);

    let changes = catalog_changes_through_title(&mut catalog).await;
    assert_eq!(
        changes
            .iter()
            .filter(|change| matches!(change, SessionCatalogChange::Created { .. }))
            .count(),
        1,
        "only the user's own Session was ever created: {changes:?}"
    );
    assert_eq!(
        listed_title(&client, session_id).await,
        (
            "Explain the Provider seam".to_owned(),
            Some("\u{1F9F5}".to_owned())
        )
    );
    let listed = client.list_sessions(None).await.expect("list Sessions");
    assert_eq!(listed.len(), 1, "the Errand left no Session behind");
    assert!(matches!(&listed[0], SessionListItem::Readable(_)));
    assert!(
        client
            .read_session(session_id)
            .await
            .expect("read the Session")
            .transcript
            .iter()
            .all(|item| !format!("{item:?}").contains("Name the piece of work")),
        "an Errand appears in no Transcript"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_derived_title_outlives_a_restart_and_is_never_derived_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-restart-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-restart-test").await;

    let derived = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the derived Session");
    // A second Session whose derivation never lands, standing for one shut down
    // mid-derivation.
    let undecided = client
        .create_session(create_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the undecided Session");
    for _ in 0..2 {
        let errand = timeout(Duration::from_secs(1), provider.next_errand())
            .await
            .expect("an Errand reaches the Provider");
        if errand.prompt().contains("Explain the seam") {
            errand.succeed(json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }));
        } else {
            errand.fail("the Provider went away mid-derivation");
        }
    }
    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id: derived.session.id,
            title: "Explain the Provider seam".to_owned(),
            emoji: Some("\u{1F9F5}".to_owned()),
        }
    );
    drop(client);
    server.shutdown().await.expect("shut down server");

    let (runtime, mut provider) = titling_provider();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-restart-test").expect("reconfigure server"),
        runtime,
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "title-restart-test").await;

    assert_eq!(
        listed_title(&client, derived.session.id).await,
        (
            "Explain the Provider seam".to_owned(),
            Some("\u{1F9F5}".to_owned())
        ),
        "a derived Title and Emoji survive a restart"
    );
    let reopened = client
        .read_session(derived.session.id)
        .await
        .expect("reopen titled Session");
    assert_eq!(reopened.title, "Explain the Provider seam");
    assert_eq!(reopened.emoji.as_deref(), Some("🧵"));
    assert_eq!(
        listed_title(&client, undecided.session.id).await,
        ("Ship the picker".to_owned(), None),
        "a Session whose derivation failed keeps its Prompt-derived Title for good"
    );
    assert!(
        provider.try_next_errand().is_none(),
        "restarting derives no Titles again, for either Session"
    );

    restarted.shutdown().await.expect("shut down server");
}

/// Runs a Session's first Turn through to completion, which is every chance a
/// derivation would have had to reach the Provider. What follows can then say
/// no Errand was asked for without waiting out a deadline.
async fn work_the_first_turn(provider: &mut ControlledProvider) {
    let mut session = timeout(Duration::from_secs(1), provider.next_start())
        .await
        .expect("the first Turn starts a Provider Session")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: hosted_selection(PROVIDER, MODEL),
        });
    timeout(Duration::from_secs(1), session.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    session.emit(ProviderEvent::TurnCompleted);
}

#[tokio::test]
async fn title_derivation_turned_off_asks_for_no_errand_at_all() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_root_pinning(&TitleErrand::Off);
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-off-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-off-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    work_the_first_turn(&mut provider).await;

    assert!(
        provider.try_next_errand().is_none(),
        "a user who turned titling off is charged for no Provider call on Suru's behalf"
    );
    assert_eq!(
        listed_title(&client, created.session.id).await,
        ("Explain the seam".to_owned(), None),
        "the Prompt-derived Title stands"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_pinned_selection_titles_a_session_whatever_that_session_converses_at() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    // The Session converses at the Provider's default Model; the pin names the
    // other one, so the Model the Errand runs at can only have come from it.
    let pinned = AgentSelection {
        provider: ProviderId::new(PROVIDER),
        model: ModelId::new(ERRAND_MODEL),
        options: Vec::new(),
    };
    let config_dir = config_root_pinning(&TitleErrand::Pinned(pinned));
    let (runtime, mut provider) = errand_titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-pinned-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "title-pinned-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");
    assert_eq!(
        errand.selection(),
        &AgentSelection {
            provider: ProviderId::new(PROVIDER),
            model: ModelId::new(ERRAND_MODEL),
            // The pin names a Model and leaves its Options alone, so the
            // Model's own defaults fill them in like any other declaration.
            options: vec![ModelOptionSelection {
                id: ModelOptionId::new(EFFORT_OPTION),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("thorough"),
                },
            }],
        },
        "the pinned Provider and Model derive the Title, not the Session's own"
    );
    errand.succeed(json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }));

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id: created.session.id,
            title: "Explain the Provider seam".to_owned(),
            emoji: Some("\u{1F9F5}".to_owned()),
        }
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_pinned_selection_titles_a_session_that_has_selected_no_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    // A Provider serving one Model and defaulting to none, so a Session created
    // without a Selection acquires none — and the pin is the only thing in the
    // running server naming a Model an Errand could run at.
    let mut only_model = hosted_model(PROVIDER, ERRAND_MODEL);
    only_model.is_default = false;
    let pinned = AgentSelection {
        provider: ProviderId::new(PROVIDER),
        model: ModelId::new(ERRAND_MODEL),
        options: Vec::new(),
    };
    let config_dir = config_root_pinning(&TitleErrand::Pinned(pinned.clone()));
    let (runtime, mut provider) =
        ControlledProvider::with_provider(ProviderId::new(PROVIDER), vec![only_model]);
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-pinned-unselected-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-pinned-unselected-test").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the seam".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    assert_eq!(created.session.agent_selection, None);

    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");
    assert_eq!(
        errand.selection(),
        &pinned,
        "a pin is the user choosing a Provider up front, so a Session that chose none is still titled"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_pinned_model_that_has_gone_falls_back_to_its_providers_default_model() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_root_pinning(&TitleErrand::Pinned(AgentSelection {
        provider: ProviderId::new(PROVIDER),
        model: ModelId::new(ERRAND_MODEL),
        options: Vec::new(),
    }));
    let (runtime, mut provider) = errand_titling_provider();
    let catalog = std::sync::Arc::clone(&runtime);
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "title-pinned-withdrawal-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "title-pinned-withdrawal-test").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("an Errand reaches the Provider")
        .succeed(json!({ "title": "Explain the Provider seam", "emoji": "\u{1F9F5}" }));

    catalog.withdraw_model(&ModelId::new(ERRAND_MODEL));
    client.refresh_models().await.expect("refresh the catalog");

    client
        .create_session(create_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    let errand = timeout(Duration::from_secs(1), provider.next_errand())
        .await
        .expect("a second Errand reaches the Provider");
    assert_eq!(
        errand.selection(),
        &hosted_selection(PROVIDER, MODEL),
        "a pin naming a Model that has gone falls back rather than costing the Errand its answer"
    );

    server.shutdown().await.expect("shut down server");
}
