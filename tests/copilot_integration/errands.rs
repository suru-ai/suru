//! Copilot's own Errands: the Provider-side Session one is run through, what that Session is
//! allowed to do, and the derived Title it leaves behind.
//!
//! The Copilot SDK offers no one-shot mode and no way to hand a Provider a schema, so every Errand
//! here takes ADR 0011's escape hatch: the runtime opens a Copilot Session of its own, delivers the
//! one Prompt with the schema written into it, drains for the reply, and discards the Session. What
//! these tests hold it to is that nothing of that Session survives — not in Suru's own Session
//! listing, and not on the CLI as something still open.

use crate::server_support::PROGRESS_DEADLINE;
use std::sync::Arc;

use crate::{
    server_support::{catalog_changes_through_title, open_catalog_stream},
    support::{
        ScriptedCopilot, connect, connect_arm, connect_in, create_session_arm, current_model_arm,
        delete_session_arm, detach_session_arm, models_arm, permission_decision_arm,
        resume_session_arm, send_arm, session_models_arm, settled_session, settled_session_on,
        signed_in_arm, skills_reload_arm, switch_model_arm,
    },
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        AdmitPromptRequest, AgentSelection, CreateSessionRequest, InitialPrompt, ModelId,
        PromptDelivery, PromptId, ProviderId, SessionCatalogChange, SessionId, SessionListItem,
        SessionTitleChanged,
    },
    provider::CopilotRuntime,
    server::{self, RunningServer, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

/// The Model Copilot declares its own Errands run at, and the effort it declares them at: the
/// cheapest, fastest Model in its catalog, thinking as little as it offers to. Named here rather
/// than read off the runtime, because the declaration is the thing under test.
const ERRAND_MODEL: &str = "gpt-5.6-luna";
const ERRAND_EFFORT: &str = "none";

/// A catalog carrying the Model Copilot declares its Errands run at alongside the ones a user
/// converses with, so a declaration the live catalog admits is what an Errand resolves to.
const ERRAND_MODELS: &str = concat!(
    r#"[{"id":"auto","name":"Auto","capabilities":{}},"#,
    r#"{"id":"claude-fixture","name":"Claude Fixture","capabilities":{},"#,
    r#""supportedReasoningEfforts":["low","high"],"defaultReasoningEffort":"high"},"#,
    r#"{"id":"gpt-5.6-luna","name":"Luna","capabilities":{},"#,
    r#""supportedReasoningEfforts":["none","low","high"],"defaultReasoningEffort":"high"}]"#,
);

/// The first Prompt every Session here is started with, and the Title an Errand derives from it:
/// deliberately not each other's words, so a Title that arrives is one Copilot wrote.
const FIRST_PROMPT: &str = "Work out why the Copilot harness drops its second Turn";
const DERIVED_TITLE: &str = "Explain the Copilot seam";
/// The Icon Catalog name the Errand answers with, beside the Title it chose.
const DERIVED_ICON: &str = "md-bug";

/// What the Errand's Prompt says whatever else it carries, which is how the fixture tells an Errand
/// apart from the Turn the user started.
const ERRAND_MARKER: &str = "Name the piece of work";

/// The Turn's own timeline: one agent Message and the idle that settles it.
const TURN_TIMELINE: &str = r#"        event t1 assistant.message '{"messageId":"t1","content":"Working on it"}'
        event t2 session.idle '{}'
"#;

/// An Errand answered exactly as it asked to be: one JSON object and nothing else.
fn answered_errand() -> String {
    format!(
        r#"        event e1 assistant.message '{{"messageId":"e1","content":"{{\"title\":\"{DERIVED_TITLE}\",\"icon\":\"{DERIVED_ICON}\"}}"}}'
        event e2 session.idle '{{}}'
"#
    )
}

/// An Errand answered with prose, which is a Model ignoring the shape it was asked for.
const UNSHAPED_ERRAND: &str = r#"        event e1 assistant.message '{"messageId":"e1","content":"Sure! I can help you name that work."}'
        event e2 session.idle '{}'
"#;

/// An Errand the CLI never finishes: the Prompt is accepted and the loop never goes idle, so the
/// Errand's own deadline is what ends it.
const UNANSWERED_ERRAND: &str = "        :\n";

/// A `session.send` arm playing one timeline for an Errand's Prompt and another for the Turn the
/// user started, so a test can answer each on its own terms.
fn errand_send_arm(errand: &str, turn: &str) -> String {
    send_arm(&format!(
        r#"      case "$body" in
        *'{ERRAND_MARKER}'*)
{errand}          ;;
        *)
{turn}          ;;
      esac
"#
    ))
}

/// A fixture carrying a Session from creation through its first Turn while an Errand runs beside
/// it, answering the Errand with `errand`.
fn errand_fixture(errand: &str) -> ScriptedCopilot {
    ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        models_arm(ERRAND_MODELS),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        session_models_arm(),
        switch_model_arm(),
        permission_decision_arm(),
        detach_session_arm(),
        delete_session_arm(),
        errand_send_arm(errand, TURN_TIMELINE),
    ))
}

/// The Agent Selection a Session converses under, which is never the one its Errand runs at.
fn conversation_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("copilot"),
        model: ModelId::new("claude-fixture"),
        options: Vec::new(),
    }
}

fn create_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: None,
        agent_selection: Some(conversation_selection()),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

/// A server hosting the scripted Copilot alone, with `errand_timeout` bounding how long an Errand
/// may take — injected in milliseconds so a test never waits out the real deadline.
async fn hosting_with_errand_timeout(
    copilot: &ScriptedCopilot,
    name: &'static str,
    state_dir: &std::path::Path,
    errand_timeout: Duration,
) -> (RunningServer, ManagedClient) {
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir, name).expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
        ServerTimings::default().with_errand_timeout(errand_timeout),
    )
    .await
    .expect("spawn server");
    (server, connect(state_dir, name).await)
}

/// The first request the runtime made that `wanted` recognizes, once it has made one. `what` names
/// what was being waited for, so a wait that runs out says which one did.
async fn request_where(
    copilot: &ScriptedCopilot,
    what: &str,
    wanted: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(request) = copilot.requests().into_iter().find(&wanted) {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}; captured methods: {:?}", copilot.methods()))
}

/// The `session.create` the Errand made, once it has: the one naming the Model Copilot runs its own
/// Errands at, which is never the Model the Session converses under.
async fn errand_creation(copilot: &ScriptedCopilot) -> serde_json::Value {
    request_where(
        copilot,
        &format!("the Errand opens a Copilot Session at `{ERRAND_MODEL}`"),
        |request| {
            request["method"] == "session.create" && request["params"]["model"] == ERRAND_MODEL
        },
    )
    .await
}

/// The `session.send` the Errand made, once it has: the one carrying the Errand's own Prompt rather
/// than the Prompt the user wrote.
async fn errand_send(copilot: &ScriptedCopilot) -> serde_json::Value {
    request_where(copilot, "the Errand delivers its one Prompt", |request| {
        request["method"] == "session.send"
            && request["params"]["prompt"]
                .as_str()
                .is_some_and(|prompt| prompt.contains(ERRAND_MARKER))
    })
    .await
}

/// The Session Copilot was asked to close and then to delete, once it has been asked to do both.
///
/// Both halves are the discard: closing severs the connection, and deleting is what stops Copilot
/// offering the Session back in its own picker — which it does even with its session store turned
/// off, so a Title-per-Session left behind in the CLI the user works in outside Suru is exactly
/// what closing alone would leave.
async fn discarded_session(copilot: &ScriptedCopilot) -> String {
    let closed = copilot.wait_for_request("session.detach").await["params"]["sessionId"]
        .as_str()
        .expect("the closed Session is named")
        .to_owned();
    let deleted = copilot.wait_for_request("session.delete").await["params"]["sessionId"]
        .as_str()
        .expect("the deleted Session is named")
        .to_owned();
    assert_eq!(
        closed, deleted,
        "the Copilot Session an Errand closed is the one it deletes"
    );
    deleted
}

/// The Title a Session carries in a listing, alongside the Icon Catalog name beside it.
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
        listed.icon().map(ToOwned::to_owned),
    )
}

#[tokio::test]
async fn a_session_started_on_copilot_is_titled_by_an_errand_that_leaves_no_session_behind() {
    let copilot = errand_fixture(&answered_errand());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, client) = hosting_with_errand_timeout(
        &copilot,
        "copilot-errand-title",
        state_dir.path(),
        Duration::from_secs(10),
    )
    .await;
    let descriptor = server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;

    let created = client
        .create_session(create_request(workspace.path(), FIRST_PROMPT))
        .await
        .expect("create Session");
    let session_id = created.session.id;

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
        (DERIVED_TITLE.to_owned(), Some(DERIVED_ICON.to_owned()))
    );
    let listed = client.list_sessions(None).await.expect("list Sessions");
    assert_eq!(listed.len(), 1, "the Errand left no Session behind");
    assert!(matches!(&listed[0], SessionListItem::Readable(_)));

    let errand_session = errand_creation(&copilot).await["params"]["sessionId"]
        .as_str()
        .expect("the Errand names the Copilot Session it opens")
        .to_owned();
    assert_eq!(
        discarded_session(&copilot).await,
        errand_session,
        "the Copilot Session an Errand opened is the one it discards"
    );
    assert!(
        !client
            .read_session(session_id)
            .await
            .expect("read the Session")
            .transcript
            .iter()
            .any(|item| format!("{item:?}").contains(ERRAND_MARKER)),
        "an Errand appears in no Transcript"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn an_errand_runs_at_copilots_declared_selection_carrying_no_tools_and_storing_nothing() {
    let copilot = errand_fixture(&answered_errand());
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, client) = hosting_with_errand_timeout(
        &copilot,
        "copilot-errand-selection",
        state_dir.path(),
        Duration::from_secs(10),
    )
    .await;

    client
        .create_session(create_request(workspace.path(), FIRST_PROMPT))
        .await
        .expect("create Session");

    let creation = errand_creation(&copilot).await;
    let params = &creation["params"];
    assert_eq!(params["reasoningEffort"], ERRAND_EFFORT);
    assert_eq!(
        params["availableTools"],
        serde_json::json!([]),
        "an Errand carries no Tools"
    );
    assert_eq!(
        params["enableSessionStore"],
        serde_json::json!(false),
        "an Errand takes the least-persistent mode the CLI offers"
    );
    assert!(
        params.get("mcpServers").is_none(),
        "an Errand is handed no MCP server, the Broker's included: {params}"
    );
    let conversation = request_where(
        &copilot,
        "the Session's own Copilot Session is created",
        |request| {
            request["method"] == "session.create" && request["params"]["model"] != ERRAND_MODEL
        },
    )
    .await;
    assert!(
        conversation["params"]["mcpServers"]["suru"].is_object(),
        "while the Session's own Copilot Session beside it is handed the Broker: {conversation}"
    );
    assert_eq!(
        params["workingDirectory"].as_str(),
        suru::paths::canonical(workspace.path())
            .expect("canonicalize Workspace")
            .to_str(),
        "an Errand runs in the Session's own Workspace"
    );
    let sent = errand_send(&copilot).await;
    let prompt = sent["params"]["prompt"]
        .as_str()
        .expect("the Prompt is text");
    assert!(
        prompt.contains(FIRST_PROMPT),
        "the Errand carries the first Prompt: {prompt}"
    );
    assert!(
        prompt.contains("\"title\"") && prompt.contains("\"icon\""),
        "a harness that cannot be handed a schema is told it in the Prompt: {prompt}"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn an_errand_answered_outside_its_schema_leaves_the_prompt_derived_title_standing() {
    let copilot = errand_fixture(UNSHAPED_ERRAND);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client) = hosting_with_errand_timeout(
        &copilot,
        "copilot-errand-unshaped",
        state_dir.path(),
        Duration::from_secs(10),
    )
    .await;

    let created = client
        .create_session(create_request(workspace.path(), FIRST_PROMPT))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    // The discarded Session is what proves the Errand ran and ended; the Title standing afterwards
    // is what proves its answer was thrown away.
    discarded_session(&copilot).await;
    // A Working Session cannot be deleted, and the Turn beside the Errand settles on its own time.
    settled_session(&client, session_id, 0).await;
    client
        .delete_session(session_id)
        .await
        .expect("delete the Session");
    assert!(
        !matches!(
            timeout(PROGRESS_DEADLINE, client.next())
                .await
                .expect("the deletion reaches the client"),
            Some(ManagedEvent::SessionTitleChanged(
                SessionTitleChanged { .. }
            ))
        ),
        "no Title reached the client ahead of the deletion"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn an_errand_that_runs_out_of_time_still_discards_the_session_it_opened() {
    let copilot = errand_fixture(UNANSWERED_ERRAND);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, client) = hosting_with_errand_timeout(
        &copilot,
        "copilot-errand-timeout",
        state_dir.path(),
        Duration::from_millis(200),
    )
    .await;

    let created = client
        .create_session(create_request(workspace.path(), FIRST_PROMPT))
        .await
        .expect("create Session");

    let errand_session = errand_creation(&copilot).await["params"]["sessionId"]
        .as_str()
        .expect("the Errand names the Copilot Session it opens")
        .to_owned();
    assert_eq!(
        discarded_session(&copilot).await,
        errand_session,
        "an Errand that ran out of time discards its Copilot Session all the same"
    );
    assert_eq!(
        listed_title(&client, created.session.id).await,
        (FIRST_PROMPT.to_owned(), None),
        "the Prompt-derived Title stands"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn an_errand_leaves_a_restart_nothing_of_its_own_to_resume() {
    // A CLI that answers a resume as well as a creation, so a Copilot Session Suru had filed a
    // Resume State for would be picked back up here — and the Errand's, if one existed, with it.
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        models_arm(ERRAND_MODELS),
        create_session_arm(),
        resume_session_arm(),
        skills_reload_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        session_models_arm(),
        switch_model_arm(),
        permission_decision_arm(),
        detach_session_arm(),
        delete_session_arm(),
        errand_send_arm(&answered_errand(), TURN_TIMELINE),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "copilot-errand-restart")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let client_config = || {
        ManagedClientConfig::new(state_dir.path(), "copilot-errand-restart")
            .expect("configure client")
            .with_data_dir(data_dir.path())
    };
    let original = server::spawn_with_provider(
        config.clone(),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect_in(client_config()).await;

    let created = client
        .create_session(create_request(workspace.path(), FIRST_PROMPT))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the Session");
    settled_session_on(&client, &mut feed, session_id, 0).await;
    let errand_session = errand_creation(&copilot).await["params"]["sessionId"]
        .as_str()
        .expect("the Errand names the Copilot Session it opens")
        .to_owned();
    discarded_session(&copilot).await;
    drop(feed);
    drop(client);
    original.shutdown().await.expect("stop the original server");
    copilot.wait_for_exit().await;

    let server =
        server::spawn_with_provider(config, Arc::new(CopilotRuntime::new(copilot.executable())))
            .await
            .expect("spawn the replacement server");
    let client = connect_in(client_config()).await;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the reopened Session");
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Carry on".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt to the reopened Session");
    settled_session_on(&client, &mut feed, session_id, 1).await;

    let resumed = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.resume")
        .map(|request| {
            request["params"]["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        resumed.len(),
        1,
        "the restart resumes the Session the user was working in and nothing else: {resumed:?}"
    );
    assert_ne!(
        resumed[0], errand_session,
        "the Copilot Session an Errand ran on is never continued"
    );
    assert_eq!(
        client
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .len(),
        1,
        "the Errand left no Session for the restart to find"
    );

    drop(feed);
    drop(client);
    server
        .shutdown()
        .await
        .expect("shut the replacement server down");
}
