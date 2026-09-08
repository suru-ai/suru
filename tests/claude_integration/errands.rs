//! Claude's Errands: a Session started on Claude titled by a print-mode call, at the Model Claude
//! declares its own Errands run at, carrying no Tools and leaving nothing resumable behind.

use std::sync::Arc;

use crate::{
    server_support::{assert_no_title_reaches, next_derived_title},
    support::{
        CLAUDE_MODELS, ScriptedClaude, connect, conversation_arms, crashing_errand_preamble,
        derived_title_envelope, errand_preamble, failed_errand_envelope, models_without,
        silent_errand_preamble,
    },
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        AgentSelection, CreateSessionRequest, InitialPrompt, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionSelection, ModelOptionValue, PromptId, ProviderId, SessionId,
        SessionListItem, SessionTitleChanged,
    },
    provider::ClaudeRuntime,
    server::{self, RunningServer, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

/// The first Prompt every test here starts its Session with, long enough that a Title echoing it
/// would be the wrong Title.
const FIRST_PROMPT: &str = "the reasoning group flickers when a block settles mid-run";

/// What the Session's own Turn plays: nothing. These tests are about the Errand beside the Turn,
/// not the Turn.
const SILENT_TIMELINE: &str = "      :\n";

/// The Agent Selection the Session converses under, chosen outright so the Model an Errand runs at
/// is legibly not the Model the Session itself uses.
fn conversation_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("middling"),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("low"),
            },
        }],
    }
}

/// A server hosting Claude alone against `claude`, a client attached to it, and a Session opened on
/// the Prompt above — which is what sets an Errand going.
async fn titled_session(
    claude: &ScriptedClaude,
    name: &'static str,
    state_dir: &std::path::Path,
    workspace: &std::path::Path,
) -> (RunningServer, ManagedClient, SessionId) {
    titled_session_with_timings(claude, name, state_dir, workspace, ServerTimings::default()).await
}

/// The same, over timings the caller has tuned — an Errand deadline short enough to watch expire.
async fn titled_session_with_timings(
    claude: &ScriptedClaude,
    name: &'static str,
    state_dir: &std::path::Path,
    workspace: &std::path::Path,
    timings: ServerTimings,
) -> (RunningServer, ManagedClient, SessionId) {
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir, name).expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
        timings,
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir, name).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(conversation_selection()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: FIRST_PROMPT.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    (server, client, created.session.id)
}

/// Waits until the Errand has reached the CLI, so a test that goes on to prove no Title arrived is
/// proving something about an Errand Suru actually made.
async fn errand_reaches_the_cli(claude: &ScriptedClaude) {
    timeout(Duration::from_secs(10), async {
        while !claude
            .exact_launches()
            .iter()
            .any(|launch| launch.carries("--json-schema"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Errand reaches the CLI");
}

/// A Session opened on Claude is retitled by an Errand Claude ran through its own print mode: the
/// Prompt goes in on stdin, the schema goes in as a flag the CLI enforces itself, and the answer
/// comes back as one object — with no Tools, no permission bypass, and nothing left to resume.
#[tokio::test]
async fn a_session_on_claude_is_titled_by_a_print_mode_errand() {
    let claude = ScriptedClaude::with_preamble(
        &errand_preamble(&derived_title_envelope(
            "Fix reasoning group flicker",
            "\u{1F41B}",
        )),
        &conversation_arms(SILENT_TIMELINE),
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, session_id) = titled_session(
        &claude,
        "claude-title-errand",
        state_dir.path(),
        workspace.path(),
    )
    .await;

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Fix reasoning group flicker".to_owned(),
            emoji: Some("\u{1F41B}".to_owned()),
        },
        "the Errand's answer becomes the Session's Title and the Emoji beside it"
    );

    let errand = claude.launch_carrying("--json-schema");
    assert_eq!(
        errand.arguments[..3],
        ["--print", "--output-format", "json"],
        "the Errand runs through the CLI's own one-shot rather than a conversation: {:?}",
        errand.arguments
    );
    assert_eq!(
        errand.value("--tools"),
        "",
        "an Errand carries no Tools: {:?}",
        errand.arguments
    );
    assert!(
        !errand.carries("--dangerously-skip-permissions"),
        "an Errand skips no permissions: {:?}",
        errand.arguments
    );
    assert!(
        errand.carries("--no-session-persistence"),
        "an Errand leaves nothing resumable behind: {:?}",
        errand.arguments
    );
    assert!(
        !errand.carries("--session-id") && !errand.carries("--resume"),
        "an Errand neither mints a conversation nor continues one: {:?}",
        errand.arguments
    );
    assert!(
        errand.carries("--setting-sources") && errand.value("--setting-sources").is_empty(),
        "an Errand remains isolated from Claude's personal and project setting sources: {:?}",
        errand.arguments
    );
    assert_eq!(
        errand.value("--model"),
        "haiku",
        "the Errand runs at the cheap Model Claude declares for its own Errands, \
         not the one the Session converses with: {:?}",
        errand.arguments
    );
    assert!(
        !errand.carries("--effort"),
        "the declared row publishes no reasoning effort, so the Errand asks for none: {:?}",
        errand.arguments
    );
    assert_eq!(
        errand.working_directory,
        std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace"),
        "the Errand runs in the Session's Workspace"
    );

    let schema: serde_json::Value =
        serde_json::from_str(errand.value("--json-schema")).expect("the schema is JSON");
    assert!(
        schema["properties"]["title"].is_object() && schema["properties"]["emoji"].is_object(),
        "the schema reaches the CLI natively rather than as prose: {schema}"
    );
    let prompt = claude.errand_prompts();
    assert!(
        prompt.contains(FIRST_PROMPT),
        "the Errand's Prompt is delivered on stdin: {prompt:?}"
    );

    let listed = client.list_sessions(None).await.expect("list Sessions");
    assert_eq!(
        listed.iter().map(SessionListItem::id).collect::<Vec<_>>(),
        [session_id],
        "the Errand left no Session of its own behind, resumable or otherwise"
    );

    server.shutdown().await.expect("shut the server down");
}

/// An Errand the CLI answers with a failure leaves the Prompt-derived Title exactly where it was,
/// and says nothing to the user about it.
#[tokio::test]
async fn a_failed_claude_errand_leaves_the_prompt_derived_title_standing() {
    let claude = ScriptedClaude::with_preamble(
        &errand_preamble(&failed_errand_envelope()),
        &conversation_arms(SILENT_TIMELINE),
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, session_id) = titled_session(
        &claude,
        "claude-failed-title-errand",
        state_dir.path(),
        workspace.path(),
    )
    .await;

    // The Errand reaches the CLI and is refused there, so the failure is one Suru met rather than
    // one it never asked for.
    errand_reaches_the_cli(&claude).await;
    assert_no_title_reaches(
        &mut client,
        session_id,
        "a failed Errand leaves the Prompt-derived Title standing and says nothing",
    )
    .await;

    server.shutdown().await.expect("shut the server down");
}

/// An Errand the CLI takes and never answers is given up on at the deadline, and the Session keeps
/// the Title its Prompt gave it — a wedged CLI costs a Title rather than leaving work outstanding.
#[tokio::test]
async fn a_claude_errand_that_is_never_answered_leaves_the_prompt_derived_title_standing() {
    let claude = ScriptedClaude::with_preamble(
        &silent_errand_preamble(),
        &conversation_arms(SILENT_TIMELINE),
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, session_id) = titled_session_with_timings(
        &claude,
        "claude-silent-title-errand",
        state_dir.path(),
        workspace.path(),
        // Short enough that the test watches the deadline expire rather than waiting one out.
        ServerTimings::default().with_errand_timeout(Duration::from_millis(50)),
    )
    .await;

    errand_reaches_the_cli(&claude).await;
    assert_no_title_reaches(
        &mut client,
        session_id,
        "an Errand Suru stopped waiting on leaves the Prompt-derived Title standing",
    )
    .await;

    server.shutdown().await.expect("shut the server down");
}

/// An Errand whose CLI dies before printing anything leaves the Title standing too — the failure
/// path where there is no result object to read the reason out of.
#[tokio::test]
async fn a_claude_errand_whose_cli_dies_leaves_the_prompt_derived_title_standing() {
    let claude = ScriptedClaude::with_preamble(
        &crashing_errand_preamble(),
        &conversation_arms(SILENT_TIMELINE),
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, session_id) = titled_session(
        &claude,
        "claude-crashing-title-errand",
        state_dir.path(),
        workspace.path(),
    )
    .await;

    errand_reaches_the_cli(&claude).await;
    assert_no_title_reaches(
        &mut client,
        session_id,
        "a CLI that died mid-Errand leaves the Prompt-derived Title standing",
    )
    .await;

    server.shutdown().await.expect("shut the server down");
}

/// A Model discovery that no longer serves the declared row sends the Errand to Claude's default
/// Model rather than giving up on the Title.
#[tokio::test]
async fn an_errand_falls_back_to_claudes_default_model_once_the_cheap_row_is_withdrawn() {
    let withdrawn = models_without(CLAUDE_MODELS, "haiku");
    let claude = ScriptedClaude::with_preamble(
        &errand_preamble(&derived_title_envelope("Fix the flicker", "\u{1F41B}")),
        &conversation_arms_over(&withdrawn),
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, _) = titled_session(
        &claude,
        "claude-withdrawn-errand-model",
        state_dir.path(),
        workspace.path(),
    )
    .await;

    assert_eq!(
        next_derived_title(&mut client).await.title,
        "Fix the flicker"
    );
    assert_eq!(
        claude.launch_carrying("--json-schema").value("--model"),
        "default",
        "a declared Model the catalog no longer serves gives way to Claude's default"
    );

    server.shutdown().await.expect("shut the server down");
}

/// The Session arms over a catalog of the caller's own, for a test whose whole point is a catalog
/// that differs from the fixture's usual one.
fn conversation_arms_over(models: &str) -> String {
    format!(
        "{}{}",
        crate::support::discovery_arms(models),
        crate::support::user_turn_arm(SILENT_TIMELINE)
    )
}
