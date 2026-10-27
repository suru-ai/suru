//! Renaming a Session: setting its Title to a line the user writes, and asking
//! Suru to propose one derived anew from the Session's Prompts so far.

use crate::session_integration::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    server_support::{
        assert_no_title_reaches, config_root_pinning, next_catalog_change_matching,
        next_derived_title, open_catalog_stream,
    },
    support::{hosted_model, hosted_selection, receive_managed_client_initial_state},
};
use crate::support::PROGRESS_DEADLINE;
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        AdmitPromptRequest, AgentId, AgentIdentity, CreateSessionRequest, DerivationErrand,
        InitialPrompt, PromptDelivery, PromptId, ProviderId, SessionCatalogChange, SessionError,
        SessionErrorCode, SessionId, SessionTitleChanged,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";

fn create_request(
    workspace: &std::path::Path,
    prompt: &str,
    with_provider: bool,
) -> CreateSessionRequest {
    CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        agent_selection: with_provider.then(|| hosted_selection(PROVIDER, MODEL)),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    }
}

fn titling_provider() -> (
    std::sync::Arc<crate::support::provider::ControlledProviderRuntime>,
    ControlledProvider,
) {
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL)],
    )
}

async fn connected_client(state_dir: &std::path::Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    client
}

/// The Title one Session in a listing carries.
async fn listed_title(client: &ManagedClient, session_id: SessionId) -> String {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session remains listed")
        .title()
        .to_owned()
}

fn refusal_code(error: &anyhow::Error) -> SessionErrorCode {
    error
        .downcast_ref::<SessionError>()
        .unwrap_or_else(|| panic!("a Session refusal: {error:#}"))
        .code
}

async fn finish_initial_turn(provider: &mut ControlledProvider) {
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the Session reaches its Provider");
    let mut session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: hosted_selection(PROVIDER, MODEL),
    });
    timeout(PROGRESS_DEADLINE, session.next_turn())
        .await
        .expect("the initial Turn reaches its Provider")
        .succeed();
    session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
}

fn later_prompt(text: &str) -> AdmitPromptRequest {
    AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
        delivery: PromptDelivery::Queue,
    }
}

#[tokio::test]
async fn renaming_a_session_sets_its_title_everywhere_and_keeps_its_icon() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "rename-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "rename-test").await;
    let mut catalog = open_catalog_stream(server.descriptor()).await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", false))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    client
        .set_session_icon(session_id, "md-bug")
        .await
        .expect("choose an Icon");
    next_derived_title(&mut client).await;

    let renamed = client
        .set_session_title(session_id, "  Map the Provider\nseam  ")
        .await
        .expect("rename the Session");
    assert_eq!(
        renamed.title, "Map the Provider seam",
        "a Title is one line, trimmed, its breaks folded into spaces"
    );
    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Map the Provider seam".to_owned(),
            icon: Some("md-bug".to_owned()),
        },
        "renaming leaves the Icon standing"
    );
    let change = next_catalog_change_matching(&mut catalog, |change| {
        matches!(change, SessionCatalogChange::TitleChanged { session_id: id, title, .. }
            if *id == session_id && title == "Map the Provider seam")
    })
    .await;
    assert!(matches!(change, SessionCatalogChange::TitleChanged { .. }));
    assert_eq!(
        listed_title(&client, session_id).await,
        "Map the Provider seam"
    );

    drop(catalog);
    drop(client);
    server.shutdown().await.expect("shut down server");

    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "rename-test").expect("reconfigure server"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "rename-test").await;
    assert_eq!(
        client
            .read_session(session_id)
            .await
            .expect("reopen the Session")
            .title,
        "Map the Provider seam",
        "a rename outlives a restart"
    );
    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_blank_or_overlong_title_is_refused_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "rename-refusal-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "rename-refusal-test").await;
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", false))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    let blank = client
        .set_session_title(session_id, " \n\t ")
        .await
        .expect_err("a blank Title is refused");
    assert_eq!(refusal_code(&blank), SessionErrorCode::InvalidTitle);
    let overlong = client
        .set_session_title(session_id, &"x".repeat(81))
        .await
        .expect_err("a Title past 80 characters is refused");
    assert_eq!(refusal_code(&overlong), SessionErrorCode::InvalidTitle);
    client
        .set_session_title(session_id, &"é".repeat(80))
        .await
        .expect("80 characters is a Title, however many bytes they take");

    let unknown = client
        .set_session_title(SessionId::new(), "Anything")
        .await
        .expect_err("an unknown Session is refused");
    assert_eq!(refusal_code(&unknown), SessionErrorCode::SessionNotFound);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_rename_stands_against_a_derivation_still_in_flight() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "rename-precedence-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "rename-precedence-test").await;
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", true))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the first derivation reaches the Provider");
    client
        .set_session_title(session_id, "My own Title")
        .await
        .expect("rename while the derivation is outstanding");
    assert_eq!(next_derived_title(&mut client).await.title, "My own Title");

    errand.succeed(json!({ "title": "Explain the Provider seam", "icon": "md-bug" }));
    finish_initial_turn(&mut provider).await;
    assert_no_title_reaches(
        &mut client,
        session_id,
        "a derivation answering after a rename lands nothing",
    )
    .await;

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_proposal_is_derived_from_the_prompts_so_far_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "proposal-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "proposal-test").await;
    let created = client
        .create_session(create_request(
            workspace.path(),
            "the reasoning group flickers",
            true,
        ))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the first derivation reaches the Provider")
        .fail("no Title this time");
    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace's own Errand follows it")
        .fail("no Workspace Icon this time");
    finish_initial_turn(&mut provider).await;
    client
        .admit_prompt(session_id, later_prompt("now port the fix to the Aside"))
        .await
        .expect("admit a later Prompt");

    assert!(
        client
            .session_title_proposal_offered(session_id)
            .await
            .expect("ask whether a proposal is offered"),
        "a Session with a Provider is offered a proposal"
    );
    let proposing = client.propose_session_title(session_id);
    let answering = async {
        let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
            .await
            .expect("the proposal's Errand reaches the Provider");
        assert!(
            errand.prompt().contains("the reasoning group flickers")
                && errand.prompt().contains("now port the fix to the Aside"),
            "the Errand carries the Prompts so far: {:?}",
            errand.prompt()
        );
        let schema = errand.schema();
        assert!(
            schema["properties"]["title"].is_object()
                && schema["properties"]["icon"].is_null()
                && schema["properties"]["branch"].is_null(),
            "a proposal asks for the Title alone: {schema}"
        );
        errand.succeed(json!({ "title": "\"Fix reasoning flicker everywhere\"" }));
    };
    let (proposal, ()) = tokio::join!(proposing, answering);
    assert_eq!(
        proposal.expect("a proposal arrives"),
        "Fix reasoning flicker everywhere",
        "a proposal is cleaned up like any derived Title"
    );
    assert_eq!(
        listed_title(&client, session_id).await,
        "the reasoning group flickers",
        "a proposal is only a proposal until the user keeps it"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_proposal_is_refused_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "proposal-failure-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "proposal-failure-test").await;
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", true))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the first derivation reaches the Provider")
        .fail("no Title this time");
    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace's own Errand follows it")
        .fail("no Workspace Icon this time");

    let proposing = client.propose_session_title(session_id);
    let failing = async {
        timeout(PROGRESS_DEADLINE, provider.next_errand())
            .await
            .expect("the proposal's Errand reaches the Provider")
            .fail("the Provider is having a day");
    };
    let (proposal, ()) = tokio::join!(proposing, failing);
    let error = proposal.expect_err("a failed Errand proposes nothing");
    assert_eq!(refusal_code(&error), SessionErrorCode::TitleProposalFailed);
    assert_eq!(listed_title(&client, session_id).await, "Explain the seam");

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn no_proposal_is_offered_while_derivation_is_off() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_root_pinning(&DerivationErrand::Off);
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "proposal-off-test")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "proposal-off-test").await;
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", true))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    assert!(
        !client
            .session_title_proposal_offered(session_id)
            .await
            .expect("ask whether a proposal is offered"),
        "a user who turned derivation off is offered none"
    );
    let error = client
        .propose_session_title(session_id)
        .await
        .expect_err("asking anyway is refused");
    assert_eq!(
        refusal_code(&error),
        SessionErrorCode::TitleProposalWithheld
    );
    assert!(
        provider.try_next_errand().is_none(),
        "no Provider call is made on Suru's behalf"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn no_proposal_is_offered_for_a_session_with_no_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "proposal-no-provider-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "proposal-no-provider-test").await;
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam", false))
        .await
        .expect("create Session");

    assert!(
        !client
            .session_title_proposal_offered(created.session.id)
            .await
            .expect("ask whether a proposal is offered"),
        "following the Session, one with no Provider has nothing to derive with"
    );

    server.shutdown().await.expect("shut down server");
}
