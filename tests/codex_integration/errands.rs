//! Codex running Errands through its own one-shot mode.
//!
//! An Errand is one Provider call Suru makes for its own purposes, and Codex
//! fulfils it with `codex exec` rather than through the app-server every Session
//! is driven over: the flags that make one run ephemeral, read-only, and
//! schema-constrained are the whole reason ADR 0011 prefers a native one-shot
//! mode. What that invocation is made of is only observable here, against the
//! scripted binary, so this is where it is asserted — alongside the Title that
//! arrives, or fails to, at the far end.

use crate::{
    server_support::receive_initial_state,
    support::{ScriptedCodex, assert_process_exited},
};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, SessionDeleted, SessionId,
        SessionTitleChanged, Workspace,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

/// The first Prompt every Session here is started with, and so the Title each
/// one carries until an Errand replaces it.
const FIRST_PROMPT: &str = "the reasoning group flickers when a block settles mid-run";

/// The Model this fixture's Codex serves as its default — the one a user
/// converses with, and never the one an Errand runs at.
const DEFAULT_MODEL: &str = "gpt-fixture";

/// The cheap, fast Model Codex declares its own Errands run at. Published by
/// the fixture catalog so the declaration is one the live catalog admits.
const ERRAND_MODEL: &str = "gpt-5.6-luna";

/// A catalog serving both the Model a Session converses with and the one Codex
/// declares its Errands run at. The Errand Model publishes its efforts in the
/// Provider's own order, and its own default is *not* the least of them, so an
/// Errand arriving at `low` can only have come from the runtime's declaration.
const CATALOG_WITH_ERRAND_MODEL: &str = r#"{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Primary fixture model","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"medium","description":"Balanced"},{"reasoningEffort":"high","description":"Deepest"}],"defaultReasoningEffort":"high","serviceTiers":[],"defaultServiceTier":null,"isDefault":true},{"id":"gpt-5.6-luna","displayName":"Luna","description":"Fast and affordable","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Faster"},{"reasoningEffort":"medium","description":"Balanced"}],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":false}],"nextCursor":null}"#;

/// A catalog that has withdrawn the Model Codex declares its Errands run at,
/// as a Provider changing its catalog does. What is left carries a service tier
/// as well as an effort, so the fallback is watched relaying a whole Selection
/// rather than only the part Codex declared.
const CATALOG_WITHOUT_ERRAND_MODEL: &str = r#"{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Primary fixture model","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"medium","description":"Balanced"},{"reasoningEffort":"high","description":"Deepest"}],"defaultReasoningEffort":"high","serviceTiers":[{"id":"flex","name":"Flex","description":"Flexible processing"}],"defaultServiceTier":"flex","isDefault":true}],"nextCursor":null}"#;

/// What the fixture does when it is invoked as `codex exec`: record everything
/// about the invocation, then run the body the test gave it. Recorded last of
/// all is the marker a test waits on, so nothing is read half-written.
const ERRAND_SCRIPT_PREFIX: &str = r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  : > "$CODEX_FIXTURE_ERRAND-arguments"
  answer=
  schema=
  previous=
  for argument in "$@"; do
    printf '%s\n' "$argument" >> "$CODEX_FIXTURE_ERRAND-arguments"
    case "$previous" in
      --output-last-message) answer="$argument" ;;
      --output-schema) schema="$argument" ;;
    esac
    previous="$argument"
  done
  pwd -P > "$CODEX_FIXTURE_ERRAND-cwd"
  printf '%s\n' "$$" > "$CODEX_FIXTURE_ERRAND-pid"
  cat > "$CODEX_FIXTURE_ERRAND-prompt"
  cp "$schema" "$CODEX_FIXTURE_ERRAND-schema"
  printf 'recorded\n' > "$CODEX_FIXTURE_ERRAND-recorded"
"#;

/// A Codex that serves `catalog`, drives one ordinary Turn, and answers a
/// one-shot Errand run with `errand`. The Errand body ends the invocation
/// itself, so a fixture that fails or never answers is written the same way one
/// that succeeds is.
fn scripted_codex(catalog: &str, errand: &str) -> ScriptedCodex {
    ScriptedCodex::new_running_errands(&format!(
        r#"{ERRAND_SCRIPT_PREFIX}{errand}
fi
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{{"id":2,"result":{catalog}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"native-thread"}},"model":"{DEFAULT_MODEL}"}}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{{"id":3,"result":{{"turn":{{"id":"native-turn"}}}}}}'
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"native-turn","status":"completed","items":[]}}}}}}'
      ;;
  esac
done
"#
    ))
}

/// A server driving `codex`, and a client already past its opening events.
async fn running_server(
    codex: &ScriptedCodex,
    channel: &str,
    timings: ServerTimings,
) -> (server::RunningServer, ManagedClient, tempfile::TempDir) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
        timings,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    // Session creation without an Agent Selection takes the catalog's advertised
    // defaults, so the catalog is read before the first Session is started.
    client.list_models().await.expect("request Model catalog");
    (server, client, state_dir)
}

/// Starts a Session on Codex with the first Prompt every test here uses.
async fn create_session(client: &ManagedClient, workspace: &std::path::Path) -> SessionId {
    client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: FIRST_PROMPT.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session")
        .session
        .id
}

/// What Codex was invoked with, with the two temporary paths it was pointed at
/// replaced by a marker: what those files are called is Suru's own business,
/// but that a schema goes in and an answer comes back is not.
fn errand_invocation(codex: &ScriptedCodex) -> Vec<String> {
    let mut previous = String::new();
    codex
        .errand_arguments()
        .into_iter()
        .map(|argument| {
            let recorded = match previous.as_str() {
                "--output-schema" | "--output-last-message" => "<file>".to_owned(),
                _ => argument.clone(),
            };
            previous = argument;
            recorded
        })
        .collect()
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

/// Proves nothing retitled `session_id` without waiting out a deadline: the
/// Session is deleted, and the deletion is the very next thing the client hears
/// about the catalog. A Title change would have arrived in front of it.
async fn assert_no_title_reaches(client: &mut ManagedClient, session_id: SessionId) {
    client
        .delete_session(session_id)
        .await
        .expect("delete the Session");
    assert_eq!(
        timeout(Duration::from_secs(1), client.next())
            .await
            .expect("the deletion reaches the client"),
        Some(ManagedEvent::SessionDeleted(SessionDeleted { session_id })),
        "no Title reached the client ahead of the deletion"
    );
}

/// The whole of Codex's Errand: one non-interactive run that persists nothing,
/// sandboxes itself as tightly as the harness allows, is handed the schema
/// natively, and answers in the Session's own Workspace — with the Title and
/// Emoji it writes landing on the Session that asked for them.
#[tokio::test]
async fn codex_derives_a_title_through_its_own_one_shot_mode() {
    let codex = scripted_codex(
        CATALOG_WITH_ERRAND_MODEL,
        r#"  printf '%s' '{"title":"Fix reasoning group flicker","emoji":"🐛"}' > "$answer"
  exit 0"#,
    );
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, _state_dir) =
        running_server(&codex, "codex-errand-one-shot", ServerTimings::default()).await;

    let session_id = create_session(&client, workspace.path()).await;
    codex.wait_for_errand().await;

    // Every flag here is load-bearing, and why each one is passed is recorded
    // beside it in `provider::codex::errand`. What this asserts is that the
    // whole invocation is exactly that and nothing more: no trace left behind,
    // no permission taken, no Tool reaching past the sandbox, and no Prompt in
    // the argument list.
    assert_eq!(
        errand_invocation(&codex),
        [
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
            "--sandbox",
            "read-only",
            "--config",
            "web_search=\"disabled\"",
            "--model",
            ERRAND_MODEL,
            "--config",
            "model_reasoning_effort=\"low\"",
            "--output-schema",
            "<file>",
            "--output-last-message",
            "<file>",
            "-",
        ],
        "Codex runs an Errand as one ephemeral, read-only, schema-constrained run"
    );
    assert_eq!(
        codex.errand_cwd(),
        std::fs::canonicalize(workspace.path()).expect("canonicalize Workspace"),
        "the Errand runs in the Session's Workspace"
    );
    assert!(
        codex.errand_prompt().contains(FIRST_PROMPT),
        "the Errand carries the Session's first Prompt: {:?}",
        codex.errand_prompt()
    );
    let schema = codex.errand_schema();
    assert!(
        schema["properties"]["title"].is_object()
            && schema["properties"]["emoji"].is_object()
            && schema["additionalProperties"] == false,
        "the schema Suru asked for is handed to Codex natively: {schema}"
    );

    assert_eq!(
        timeout(Duration::from_secs(2), client.next())
            .await
            .expect("the derived Title reaches the client"),
        Some(ManagedEvent::SessionTitleChanged(SessionTitleChanged {
            session_id,
            title: "Fix reasoning group flicker".to_owned(),
            emoji: Some("\u{1F41B}".to_owned()),
        }))
    );
    assert_eq!(
        listed_title(&client, session_id).await,
        (
            "Fix reasoning group flicker".to_owned(),
            Some("\u{1F41B}".to_owned())
        )
    );

    server.shutdown().await.expect("shut down server");
}

/// A Provider changing its catalog does not stop Suru titling Sessions: the
/// declared Model having gone, the Errand runs at the Model Codex defaults to,
/// under that Model's own Options rather than the withdrawn one's.
#[tokio::test]
async fn a_withdrawn_errand_model_leaves_codex_running_errands_at_its_default() {
    let codex = scripted_codex(
        CATALOG_WITHOUT_ERRAND_MODEL,
        r#"  printf '%s' '{"title":"Fix reasoning group flicker"}' > "$answer"
  exit 0"#,
    );
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, client, _state_dir) =
        running_server(&codex, "codex-errand-fallback", ServerTimings::default()).await;

    create_session(&client, workspace.path()).await;
    codex.wait_for_errand().await;

    let arguments = errand_invocation(&codex);
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--model", DEFAULT_MODEL]),
        "the Errand falls back to the Model Codex defaults to: {arguments:?}"
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--config", "model_reasoning_effort=\"high\""]),
        "the fallback Model brings its own effort rather than the withdrawn one's: {arguments:?}"
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--config", "service_tier=\"flex\""]),
        "every Option of the resolved Selection reaches Codex: {arguments:?}"
    );

    server.shutdown().await.expect("shut down server");
}

/// A Codex that will not run the Errand — signed out, misconfigured, or simply
/// failing — costs the user nothing but a Log line.
#[tokio::test]
async fn a_failing_codex_errand_leaves_the_prompt_derived_title_standing() {
    let codex = scripted_codex(
        CATALOG_WITH_ERRAND_MODEL,
        r#"  printf '%s\n' 'not signed in' >&2
  exit 3"#,
    );
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, _state_dir) =
        running_server(&codex, "codex-errand-failure", ServerTimings::default()).await;

    let session_id = create_session(&client, workspace.path()).await;
    codex.wait_for_errand().await;

    assert_eq!(
        listed_title(&client, session_id).await,
        (FIRST_PROMPT.to_owned(), None),
        "the Title the first Prompt gave the Session stands"
    );
    assert_no_title_reaches(&mut client, session_id).await;

    server.shutdown().await.expect("shut down server");
}

/// A Codex that never answers is abandoned at the Errand's deadline, and the
/// run it left behind is taken down with the wait rather than left running.
#[tokio::test]
async fn a_codex_errand_that_never_answers_leaves_the_prompt_derived_title_standing() {
    let codex = scripted_codex(CATALOG_WITH_ERRAND_MODEL, r#"  while :; do sleep 1; done"#);
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, _state_dir) = running_server(
        &codex,
        "codex-errand-timeout",
        ServerTimings::default().with_errand_timeout(Duration::from_millis(50)),
    )
    .await;

    let session_id = create_session(&client, workspace.path()).await;
    codex.wait_for_errand().await;

    // Abandoning the wait is not enough on its own: a Codex left running would
    // hold the Workspace open and the Model's clock ticking for a Title nobody
    // is waiting for any more.
    assert_process_exited(codex.errand_pid()).await;
    assert_eq!(
        listed_title(&client, session_id).await.0,
        FIRST_PROMPT,
        "the Title the first Prompt gave the Session stands"
    );
    assert_no_title_reaches(&mut client, session_id).await;

    server.shutdown().await.expect("shut down server");
}
