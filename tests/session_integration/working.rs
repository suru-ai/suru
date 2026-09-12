//! Working as the server derives it from an admitted Prompt: a Session owes a
//! Turn from the moment its Prompt is admitted, before any Provider has been
//! reached, and stops owing it when that Prompt is delivered, fails, or is
//! withdrawn (ADR 0024).

use crate::provider_support::ControlledProvider;
use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{open_catalog_stream_with_snapshot, read_session_until};
use suru::{
    protocol::{
        CreateSessionRequest, InitialPrompt, InterruptOutcome, PromptId, PromptStatus,
        RuntimeDescriptor, SessionCatalogChange, SessionId, SessionListItem, SessionSnapshot,
        SessionStatus,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

async fn list_sessions(descriptor: &RuntimeDescriptor) -> Vec<SessionListItem> {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("the listing answers")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Session listing")
}

fn creation(workspace: &std::path::Path, prompt: PromptId, text: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: None,
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: prompt,
            text: text.to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

/// Asks the server to interrupt `session_id`, answering with the raw response
/// so what the interrupt did stays assertable.
async fn interrupt(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
    client
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
}

#[tokio::test]
async fn a_restart_cancels_the_prompt_no_one_is_left_to_deliver() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config =
        ServerConfig::new(state_dir.path(), "stranded-prompt-test").expect("configure server");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .expect("spawn server");
    let client = reqwest::Client::new();
    let created = crate::support::create_session(
        server.descriptor(),
        &creation(workspace.path(), PromptId::new(), "Map the provider seams"),
    )
    .await;
    // The Provider never answers, so the Prompt is still owed its Turn when
    // the process that owed it goes away.
    let held = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);
    assert!(created.session.working_since.is_some());
    server.shutdown().await.expect("shut down server");
    drop(held);

    let (restarted_runtime, mut restarted_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config.clone(), restarted_runtime)
        .await
        .expect("respawn server");
    let restored = crate::support::read_session(restarted.descriptor(), created.session.id).await;
    assert_eq!(
        restored.prompts[0].status,
        PromptStatus::Cancelled,
        "nothing in the new process owes this Prompt a Turn, so it is withdrawn \
         rather than left standing as a Message no Agent will ever answer"
    );
    assert_eq!(restored.session.status, SessionStatus::Idle);
    assert_eq!(restored.session.working_since, None);
    assert!(restored.turns.is_empty());
    assert!(restored.messages.is_empty());
    assert!(
        restarted_provider.try_next_start().is_none(),
        "a restart starts no Provider for a Prompt it just withdrew"
    );
    let listing = list_sessions(restarted.descriptor()).await;
    let summary = listing[0]
        .readable()
        .expect("the restored Session is readable");
    assert_eq!(summary.session.working_since, None);
    assert_eq!(summary.session.status, SessionStatus::Idle);
    restarted.shutdown().await.expect("shut down server");

    // The withdrawal is durable: the next process reads it back rather than
    // deciding it again.
    let (final_runtime, _final_provider) = ControlledProvider::new();
    let final_server = server::spawn_with_provider(config, final_runtime)
        .await
        .expect("respawn server once more");
    let again = crate::support::read_session(final_server.descriptor(), created.session.id).await;
    assert_eq!(again.prompts[0].status, PromptStatus::Cancelled);
    assert_eq!(
        again.revision, restored.revision,
        "the Prompt is withdrawn once"
    );
    assert_eq!(again.session.working_since, None);
    drop(client);
    final_server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn creation_begins_working_before_the_provider_is_reached() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "working-at-admission-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let (listed, mut catalog) = open_catalog_stream_with_snapshot(&descriptor).await;
    assert!(listed.is_empty(), "the catalog opens on an empty listing");

    let created = crate::support::create_session(
        &descriptor,
        &creation(workspace.path(), PromptId::new(), "Map the provider seams"),
    )
    .await;

    assert!(
        created.session.working_since.is_some(),
        "a Session owed a Turn is Working from the moment its Prompt was admitted"
    );
    assert_eq!(
        created.session.status,
        SessionStatus::Active,
        "a Session owed a Turn reads as Active, not Idle"
    );
    assert!(created.turns.is_empty(), "no Turn has begun yet");
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);

    // The Provider has not been answered, so the listing the catalog's own
    // announcement sends a reader back for already says the Session is
    // Working, with no Turn anywhere to have said it.
    let announced = crate::server_support::next_catalog_change(&mut catalog).await;
    assert!(
        matches!(
            announced,
            SessionCatalogChange::Created { session_id } if session_id == created.session.id
        ),
        "the catalog announces the Session as it is made: {announced:?}"
    );
    let listing = list_sessions(&descriptor).await;
    let summary = listing[0]
        .readable()
        .expect("the created Session is readable");
    assert_eq!(summary.session.working_since, created.session.working_since);
    assert_eq!(summary.session.status, SessionStatus::Active);

    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    drop(start);
    let _: SessionSnapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        created.session.id,
        "the abandoned startup fails the Turn",
        |snapshot| !snapshot.turns.is_empty(),
    )
    .await;
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_session_owed_a_turn_withdraws_the_prompt() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "withdraw-prompt-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let prompt_id = PromptId::new();
    let created = crate::support::create_session(
        &descriptor,
        &creation(workspace.path(), prompt_id, "Map the provider seams"),
    )
    .await;
    // The Provider is mid-startup: its connection has been asked for and not
    // yet answered, which is the whole of the window this interrupt is for.
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    let (_, mut catalog) = open_catalog_stream_with_snapshot(&descriptor).await;

    let response = interrupt(&client, &descriptor, created.session.id).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let outcome = response
        .json::<InterruptOutcome>()
        .await
        .expect("decode the interrupt outcome");
    let InterruptOutcome::WithdrewPrompt { prompt } = outcome else {
        panic!("interrupting a Session owed a Turn withdraws its Prompt: {outcome:?}")
    };
    assert_eq!(prompt.id, prompt_id);
    assert_eq!(prompt.text, "Map the provider seams");
    assert_eq!(prompt.status, PromptStatus::Cancelled);

    let withdrawn = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the withdrawn Prompt leaves the Session standing with no Turn",
        |snapshot| snapshot.prompts[0].status == PromptStatus::Cancelled,
    )
    .await;
    assert_eq!(withdrawn.session.working_since, None);
    assert_eq!(withdrawn.session.status, SessionStatus::Idle);
    assert!(withdrawn.turns.is_empty());
    assert!(withdrawn.messages.is_empty());
    assert_eq!(
        crate::server_support::next_catalog_change_matching(&mut catalog, |change| matches!(
            change,
            SessionCatalogChange::WorkingChanged { .. }
        ))
        .await,
        SessionCatalogChange::WorkingChanged {
            session_id: created.session.id,
            working_since: None,
        },
        "every listing hears that the Session stopped Working"
    );

    // The startup that was already under way is abandoned rather than run:
    // answering it now begins no Turn.
    let mut provider_session = start.succeed(suru::protocol::AgentIdentity {
        agent: suru::protocol::AgentId::new("controlled-agent"),
        selection: crate::support::controlled_selection("gpt-withdrawn", "high", "fast"),
    });
    let abandoned = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the abandoned startup reaches the Session without beginning a Turn",
        |snapshot| snapshot.session.agent_selection.is_some(),
    )
    .await;
    assert!(abandoned.turns.is_empty(), "no Turn was begun");
    assert_eq!(abandoned.session.working_since, None);
    assert!(
        provider_session.try_next_turn().is_none(),
        "the withdrawn Prompt never reaches the Provider"
    );

    // Retrying the creation that made this Session finds the Session it
    // already made, Cancelled Prompt and all: the Prompt id is what makes a
    // creation idempotent, and withdrawing its Prompt neither deletes the
    // Session nor frees the id to make another.
    let retried = crate::support::create_session(
        &descriptor,
        &creation(workspace.path(), prompt_id, "Map the provider seams"),
    )
    .await;
    assert_eq!(retried.session.id, created.session.id);
    assert_eq!(retried.prompts[0].status, PromptStatus::Cancelled);
    assert_eq!(retried.session.working_since, None);
    assert!(retried.turns.is_empty());
    assert!(
        provider_session.try_next_turn().is_none(),
        "a retry starts no work the withdrawal ended"
    );

    drop(provider_session);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_follow_up_prompt_on_an_idle_session_is_working_and_withdrawable() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "withdraw-follow-up-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = crate::support::create_session(
        &descriptor,
        &creation(workspace.path(), PromptId::new(), "Map the provider seams"),
    )
    .await;
    // The first startup fails, which leaves the Session idle, with a Failed
    // Turn and no Provider connection — so the follow-up below must start one
    // of its own, and waits where this test can see it.
    timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins")
        .fail("the Provider is not installed");
    let idle = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the failed startup leaves the Session standing",
        |snapshot| !snapshot.turns.is_empty(),
    )
    .await;
    assert_eq!(
        idle.session.working_since, None,
        "a startup that fails ends the Working its admission began"
    );
    assert_eq!(idle.session.status, SessionStatus::Idle);

    let follow_up = PromptId::new();
    let admitted = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&suru::protocol::AdmitPromptRequest {
            delivery: suru::protocol::PromptDelivery::Steer,
            prompt: InitialPrompt {
                id: follow_up,
                text: "Try that again".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("admit a follow-up Prompt");
    assert_eq!(admitted.status(), reqwest::StatusCode::CREATED);
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the follow-up starts the Provider");
    let working = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the follow-up Prompt puts the Session back to work",
        |snapshot| snapshot.session.working_since.is_some(),
    )
    .await;
    assert_eq!(working.session.status, SessionStatus::Active);
    assert_eq!(working.turns.len(), 1, "no second Turn has begun");

    let response = interrupt(&client, &descriptor, created.session.id).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let outcome = response
        .json::<InterruptOutcome>()
        .await
        .expect("decode the interrupt outcome");
    let InterruptOutcome::WithdrewPrompt { prompt } = outcome else {
        panic!("one rule covers both Prompts owed a Turn: {outcome:?}")
    };
    assert_eq!(prompt.id, follow_up);
    assert_eq!(prompt.text, "Try that again");

    let withdrawn = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the withdrawn follow-up leaves the Session standing",
        |snapshot| snapshot.session.working_since.is_none(),
    )
    .await;
    assert_eq!(withdrawn.session.status, SessionStatus::Idle);
    assert_eq!(withdrawn.prompts[1].status, PromptStatus::Cancelled);
    assert_eq!(withdrawn.turns.len(), 1);

    let mut provider_session = start.succeed(suru::protocol::AgentIdentity {
        agent: suru::protocol::AgentId::new("controlled-agent"),
        selection: crate::support::controlled_selection("gpt-withdrawn", "high", "fast"),
    });
    let abandoned = read_session_until(
        &client,
        &descriptor,
        created.session.id,
        "the abandoned startup reaches the Session without beginning a Turn",
        |snapshot| snapshot.session.agent_selection.is_some(),
    )
    .await;
    assert_eq!(abandoned.turns.len(), 1, "no Turn was begun");
    assert!(
        provider_session.try_next_turn().is_none(),
        "the withdrawn Prompt never reaches the Provider"
    );

    drop(provider_session);
    server.shutdown().await.expect("shut down server");
}
