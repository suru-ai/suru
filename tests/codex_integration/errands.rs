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
    server_support::{
        assert_no_title_reaches, next_derived_title, next_skill_catalog, receive_initial_state,
    },
    support::{ScriptedCodex, assert_process_exited},
};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, ProviderId, SessionId, SessionTitleChanged,
        SkillCatalogRequest, SkillCatalogStatus, SkillInvocation, TextSpan,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::Duration;

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
    scripted_codex_with_app_server_arms(catalog, errand, "")
}

fn scripted_codex_with_app_server_arms(
    catalog: &str,
    errand: &str,
    additional_arms: &str,
) -> ScriptedCodex {
    ScriptedCodex::new_running_errands(&format!(
        r#"{ERRAND_SCRIPT_PREFIX}{errand}
fi
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{{"id":2,"result":{{"config":{{}},"origins":{{}}}}}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{{"id":2,"result":{catalog}}}'
      ;;
{additional_arms}
    *'"method":"thread/start"'*)
      printf '%s\n' '{{"id":3,"result":{{"thread":{{"id":"native-thread"}},"model":"{DEFAULT_MODEL}"}}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{{"id":4,"result":{{"turn":{{"id":"native-turn"}}}}}}'
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
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: FIRST_PROMPT.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
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

/// The Title one Session in a listing carries.
async fn listed_title(client: &ManagedClient, session_id: SessionId) -> String {
    let listed = client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session remains listed");
    listed.title().to_owned()
}

/// The whole of Codex's Errand: one non-interactive run that persists nothing,
/// sandboxes itself as tightly as the harness allows, is handed the schema
/// natively, and answers in the Session's own Workspace — with the Title it
/// writes landing on the Session that asked for it.
#[tokio::test]
async fn codex_derives_a_title_through_its_own_one_shot_mode() {
    let codex = scripted_codex(
        CATALOG_WITH_ERRAND_MODEL,
        r#"  printf '%s' '{"title":"Fix reasoning group flicker","icon":"md-bug"}' > "$answer"
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
    assert!(
        codex
            .errand_arguments()
            .iter()
            .all(|argument| !argument.contains("mcp_servers")),
        "an Errand is handed no MCP server, the Broker's included, so its locked flags stand"
    );
    assert_eq!(
        codex.errand_cwd(),
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace"),
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
            && schema["properties"]["icon"].is_object()
            && schema["additionalProperties"] == false,
        "the schema Suru asked for is handed to Codex natively: {schema}"
    );

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Fix reasoning group flicker".to_owned(),
            icon: Some("md-bug".to_owned()),
        }
    );
    assert_eq!(
        listed_title(&client, session_id).await,
        "Fix reasoning group flicker".to_owned()
    );

    server.shutdown().await.expect("shut down server");
}

/// A Skill selected for the user's first Prompt belongs only to that Prompt.
/// The Title Errand still needs the Skill's visible name to understand the
/// request, but handing Codex its `$` marker would make `codex exec` select the
/// Skill again from the Workspace and run its instructions instead of the
/// small, schema-constrained Errand Suru asked for.
#[tokio::test]
async fn a_bound_skill_marker_is_plain_text_in_a_codex_title_errand() {
    const PROMPT: &str = "$implement fix $titles";

    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let canonical_workspace =
        suru::paths::canonical(workspace.path()).expect("canonicalize Workspace");
    let workspace_json =
        serde_json::to_string(&canonical_workspace).expect("encode Workspace path");
    let skill_arm = format!(
        r#"    *'"method":"skills/list"'*)
      printf '%s\n' '{{"id":2,"result":{{"data":[{{"cwd":{workspace_json},"skills":[{{"name":"implement","description":"Implement the requested work","path":"/private/codex/skills/implement/SKILL.md","scope":"repo","enabled":true}}],"errors":[]}}]}}}}'
      ;;"#
    );
    let codex = scripted_codex_with_app_server_arms(
        CATALOG_WITH_ERRAND_MODEL,
        r#"  printf '%s' '{"title":"Fix automatic titles","icon":"md-bug"}' > "$answer"
  exit 0"#,
        &skill_arm,
    );
    let (server, mut client, _state_dir) =
        running_server(&codex, "codex-skill-title-errand", ServerTimings::default()).await;

    let loading = client
        .list_skills(SkillCatalogRequest {
            provider: ProviderId::new("codex"),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
        })
        .await
        .expect("list Codex Skills");
    assert!(matches!(loading.status, SkillCatalogStatus::Loading));
    let catalog = next_skill_catalog(&mut client).await;
    let skill = catalog.skills.first().expect("Codex offers implement");
    let invocation = SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        span: TextSpan { start: 0, end: 10 },
    };

    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: PROMPT.to_owned(),
                skill_invocations: vec![invocation.clone()],
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Codex Skill Session");
    assert_eq!(created.prompts[0].text, PROMPT);
    assert_eq!(created.prompts[0].skill_invocations, [invocation]);

    codex.wait_for_errand().await;
    assert!(
        codex
            .errand_prompt()
            .ends_with("The request:\nimplement fix $titles"),
        "a bound Skill is described without reinvoking it: {:?}",
        codex.errand_prompt()
    );
    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id: created.session.id,
            title: "Fix automatic titles".to_owned(),
            icon: Some("md-bug".to_owned()),
        }
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
        FIRST_PROMPT.to_owned(),
        "the Title the first Prompt gave the Session stands"
    );
    assert_no_title_reaches(
        &mut client,
        session_id,
        "a failed Errand leaves the Prompt-derived Title standing and says nothing",
    )
    .await;

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
        listed_title(&client, session_id).await,
        FIRST_PROMPT,
        "the Title the first Prompt gave the Session stands"
    );
    assert_no_title_reaches(
        &mut client,
        session_id,
        "an abandoned Errand leaves the Prompt-derived Title standing and says nothing",
    )
    .await;

    server.shutdown().await.expect("shut down server");
}
