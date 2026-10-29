//! Codex running Errands through its own one-shot mode.
//!
//! An Errand is one Provider call Suru makes for its own purposes, and Codex
//! fulfils it with `codex exec` rather than through the app-server every Session
//! is driven over: the flags that make one run ephemeral, read-only, and
//! schema-constrained are the whole reason ADR 0011 prefers a native one-shot
//! mode. What that invocation is made of is only observable here, against the
//! scripted binary, so this is where it is asserted — alongside the Title that
//! arrives, or fails to, at the far end.

use crate::codex_integration::{
    server_support::{
        assert_no_title_reaches, next_derived_title, next_skill_catalog, receive_initial_state,
    },
    support::{ScriptedCodex, assert_process_exited},
    usage::priced_lookup,
};
use axum::{Json, Router, routing::get};
use serde_json::json;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    pricing::PricingSource,
    protocol::{
        AgentSelection, Cost, CostBasis, CreateSessionRequest, InitialPrompt, ModelId, PromptId,
        ProviderId, SessionId, SessionTitleChanged, SkillCatalogRequest, SkillCatalogStatus,
        SkillInvocation, TextSpan, Usage,
    },
    provider::{
        CodexRuntime, ErrandAnswer, ErrandConsumption, ErrandFailed, ProviderErrand,
        ProviderRuntime,
    },
    server::{self, ServerConfig, ServerTimings},
};
use tokio::{
    net::TcpListener,
    time::{Duration, timeout},
};

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
/// all is the marker a test waits on, so nothing is read half-written. The PID
/// is recorded first of all, with nothing but a builtin ahead of it, so a test
/// whose Errand deadline may cut the rest short still learns which process it
/// has to see taken down.
const ERRAND_SCRIPT_PREFIX: &str = r#"#!/bin/sh
if [ "$1" = "exec" ]; then
  printf '%s\n' "$$" > "$CODEX_FIXTURE_ERRAND-pid"
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
            "--json",
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
    let codex = scripted_codex(CATALOG_WITH_ERRAND_MODEL, "  idle_forever");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, mut client, _state_dir) = running_server(
        &codex,
        "codex-errand-timeout",
        // The deadline runs from before Codex is launched, so it must outlast
        // a loaded runner starting a shell: the test waits for the Errand's
        // process to name itself, and one cut short before it could would
        // leave that wait nothing to see.
        ServerTimings::default().with_errand_timeout(Duration::from_secs(1)),
    )
    .await;

    let session_id = create_session(&client, workspace.path()).await;
    // Only the PID: the deadline may take the run down before it has recorded
    // the rest of what it was asked, which this test has no use for.
    let errand_pid = codex.wait_for_errand_launch().await;

    // Abandoning the wait is not enough on its own: a Codex left running would
    // hold the Workspace open and the Model's clock ticking for a Title nobody
    // is waiting for any more.
    assert_process_exited(errand_pid).await;
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

/// The Model the fixture rate table prices, and one it does not.
const PRICED_MODEL: &str = "priced-fixture";
const UNPRICED_MODEL: &str = "unpriced-fixture";

/// What `codex exec --json` prints for one Errand, shaped as the Codex
/// reference's own JSONL event processor and SDK tests show it: the thread, its
/// one Turn, the structured final message, and the Turn's completion carrying
/// the thread's usage — cached and cache-write input inside `input_tokens`,
/// Reasoning inside `output_tokens`.
const ANSWERED_EVENTS: &str = r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{\"title\":\"Fix reasoning group flicker\"}"}}
{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":3,"cache_write_input_tokens":4,"output_tokens":29,"reasoning_output_tokens":7}}"#;

/// What those events' usage is in Suru's own disjoint parts.
fn answered_usage() -> Usage {
    Usage {
        fresh_input_tokens: Some(3),
        cache_read_tokens: Some(3),
        cache_write_tokens: Some(4),
        output_tokens: Some(22),
        reasoning_tokens: Some(7),
        native_meter: None,
    }
}

/// The body of a `codex exec` run that prints `events` on stdout, writes
/// `answer` as its final message where it was asked to, and exits `status`.
fn exec_printing(events: &str, answer: Option<&str>, status: u8) -> String {
    let answer = answer
        .map(|answer| format!("  printf '%s' '{answer}' > \"$answer\"\n"))
        .unwrap_or_default();
    format!("  cat <<'EVENTS'\n{events}\nEVENTS\n{answer}  exit {status}")
}

/// Runs one Errand at `model` straight through Codex's runtime, against the
/// fixture rate table, with `codex exec` answering as `body` says.
async fn run_errand(model: &str, body: &str) -> Result<ErrandAnswer, ErrandFailed> {
    let (pricing, _pricing_cache) = priced_lookup().await;
    run_errand_with_pricing(model, body, pricing).await
}

async fn run_errand_with_pricing(
    model: &str,
    body: &str,
    pricing: Arc<PricingSource>,
) -> Result<ErrandAnswer, ErrandFailed> {
    let codex = scripted_codex(CATALOG_WITH_ERRAND_MODEL, body);
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    CodexRuntime::new(codex.executable())
        .with_pricing_source(pricing)
        .run_errand(ProviderErrand {
            prompt: "Title this Session".to_owned(),
            schema: json!({"type": "object"}),
            selection: AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new(model),
                options: Vec::new(),
            },
            execution_directory: workspace.path().to_owned(),
        })
        .await
}

/// An Errand answers with what its final message said and with what its Turn
/// consumed, priced from the rate table Codex Turns are: Codex states tokens and
/// never dollars, so the Cost is an estimate.
#[tokio::test]
async fn a_codex_errand_answers_with_its_usage_and_an_estimated_cost() {
    let answer = run_errand(
        PRICED_MODEL,
        &exec_printing(
            ANSWERED_EVENTS,
            Some(r#"{"title":"Fix reasoning group flicker"}"#),
            0,
        ),
    )
    .await
    .expect("the Errand answers");

    assert_eq!(
        answer.reply,
        json!({"title": "Fix reasoning group flicker"})
    );
    assert_eq!(answer.consumption.usage, Some(answered_usage()));
    let cost = answer.consumption.cost.expect("a priced Model has a Cost");
    // 3 fresh at $2, 3 cache reads at $0.50, 4 cache writes at $0.25, and 22
    // output plus 7 Reasoning at $4, per million tokens.
    assert_eq!(Some(cost.cost()), Cost::from_usd(0.000_124_5));
    assert_eq!(cost.basis(), CostBasis::Estimated);
}

/// The reply is the output file's, not the event stream's: a final message
/// the events disagree with does not replace what Codex wrote as its answer.
#[tokio::test]
async fn a_codex_errand_answers_from_its_output_file_rather_than_its_events() {
    let answer = run_errand(
        PRICED_MODEL,
        &exec_printing(ANSWERED_EVENTS, Some(r#"{"title":"From the file"}"#), 0),
    )
    .await
    .expect("the Errand answers");

    assert_eq!(answer.reply, json!({"title": "From the file"}));
}

/// A Model the rate table does not price still says what it consumed; it is
/// only the Cost that is left unknown.
#[tokio::test]
async fn a_codex_errand_at_an_unpriced_model_carries_usage_without_a_cost() {
    let answer = run_errand(
        UNPRICED_MODEL,
        &exec_printing(ANSWERED_EVENTS, Some(r#"{"title":"Unpriced"}"#), 0),
    )
    .await
    .expect("the Errand answers");

    assert_eq!(
        answer.consumption,
        ErrandConsumption {
            usage: Some(answered_usage()),
            cost: None,
        }
    );
}

/// What Codex printed about its consumption is a measurement, not the answer:
/// none at all, a line that is not JSON, or a completion whose usage is not a
/// usage leaves the Usage unknown and the Errand answered.
#[tokio::test]
async fn a_codex_errand_without_a_readable_usage_event_answers_unmeasured() {
    for events in [
        "",
        r#"{"type":"thread.started","thread_id":"t"}
{"type":"turn.started"}"#,
        r#"{"type":"turn.completed","usage":"#,
        r#"{"type":"turn.completed","usage":"plenty"}"#,
        r#"{"type":"turn.completed"}"#,
    ] {
        let answer = run_errand(
            PRICED_MODEL,
            &exec_printing(events, Some(r#"{"title":"Still answered"}"#), 0),
        )
        .await
        .unwrap_or_else(|error| panic!("{events:?} still answers: {error}"));

        assert_eq!(
            answer.reply,
            json!({"title": "Still answered"}),
            "{events:?}"
        );
        assert_eq!(
            answer.consumption,
            ErrandConsumption::default(),
            "{events:?} says nothing of what the Errand consumed"
        );
    }
}

/// Codex states a zeroed usage when it never received a reading, which is a
/// figure nobody measured rather than an Errand that consumed nothing.
#[tokio::test]
async fn a_codex_errand_whose_usage_is_all_zero_answers_unmeasured() {
    let answer = run_errand(
        PRICED_MODEL,
        &exec_printing(
            r#"{"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}"#,
            Some(r#"{"title":"Unmetered"}"#),
            0,
        ),
    )
    .await
    .expect("the Errand answers");

    assert_eq!(answer.consumption, ErrandConsumption::default());
}

/// A run that completed its Turn but left no answer behind still consumed
/// what its usage event says, and the failure keeps it.
#[tokio::test]
async fn a_codex_errand_that_answers_nothing_keeps_the_usage_it_printed() {
    let failed = run_errand(PRICED_MODEL, &exec_printing(ANSWERED_EVENTS, None, 0))
        .await
        .expect_err("no answer file is no answer");

    assert_eq!(failed.consumption.usage, Some(answered_usage()));
    assert_eq!(
        failed.consumption.cost.map(|cost| cost.basis()),
        Some(CostBasis::Estimated)
    );
}

/// A run that exits badly after printing its usage keeps that usage too, and
/// says why it failed in the words Codex's own event stream used.
#[tokio::test]
async fn a_failed_codex_errand_keeps_its_usage_and_says_why_it_failed() {
    let events = format!(
        "{ANSWERED_EVENTS}\n{}",
        r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion"}}"#
    );
    let failed = run_errand(PRICED_MODEL, &exec_printing(&events, None, 1))
        .await
        .expect_err("a failed run is no answer");

    assert_eq!(failed.consumption.usage, Some(answered_usage()));
    assert!(
        failed
            .to_string()
            .contains("stream disconnected before completion"),
        "the event stream's own reason is carried through: {failed}"
    );
}

/// A run that failed before any Turn completed says why without measuring
/// anything, whether Codex said so in an event or only on its stderr.
#[tokio::test]
async fn a_codex_errand_failing_before_its_turn_completes_is_unmeasured() {
    let failed = run_errand(
        PRICED_MODEL,
        &exec_printing(
            r#"{"type":"thread.started","thread_id":"t"}
{"type":"error","message":"unexpected status 401 Unauthorized"}
{"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}}"#,
            None,
            1,
        ),
    )
    .await
    .expect_err("a failed run is no answer");
    assert_eq!(*failed.consumption, ErrandConsumption::default());
    assert!(
        failed.to_string().contains("401 Unauthorized"),
        "the event stream's reason is carried through: {failed}"
    );

    let failed = run_errand(
        PRICED_MODEL,
        "  printf '%s\\n' 'not signed in' >&2\n  exit 3",
    )
    .await
    .expect_err("a failed run is no answer");
    assert!(
        failed.to_string().contains("not signed in"),
        "stderr is carried through where no event says why: {failed}"
    );
}

/// A rate table whose source answers `served` fetches with the fixture's
/// rates and then hangs every later fetch for far longer than any Errand
/// waits, so a lookup that went to the network would be seen waiting.
async fn rates_that_stall_after(served: usize) -> (Arc<PricingSource>, tempfile::TempDir) {
    let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = Router::new().route(
        "/api.json",
        get(move || {
            let fetches = fetches.clone();
            async move {
                if fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < served {
                    return Json(json!({"openai":{"models":{"priced-fixture":{"cost":{
                        "input":2,"output":4,"cache_read":0.5,"cache_write":0.25
                    }}}}}));
                }
                std::future::pending::<Json<serde_json::Value>>().await
            }
        }),
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind pricing fixture");
    let endpoint = format!("http://{}/api.json", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let cache = tempfile::tempdir().expect("create pricing cache directory");
    let pricing = PricingSource::new(cache.path())
        .with_source_endpoint(endpoint)
        .with_refresh_interval(Duration::from_millis(20))
        .with_fetch_timeout(Duration::from_secs(60));
    (Arc::new(pricing), cache)
}

/// An Errand that has its answer never waits on the rate table: one no fetch
/// has filled, or one overdue for a refresh, leaves the Cost absent rather
/// than holding the answer — and the Usage with it — past the Errand's
/// deadline while models.dev is asked.
#[tokio::test]
async fn a_cold_or_stale_rate_table_leaves_a_codex_errands_cost_absent_without_waiting() {
    for served in [0, 1] {
        let (pricing, _cache) = rates_that_stall_after(served).await;
        if served > 0 {
            pricing.prime().await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let answer = timeout(
            Duration::from_secs(5),
            run_errand_with_pricing(
                PRICED_MODEL,
                &exec_printing(ANSWERED_EVENTS, Some(r#"{"title":"Unpriced today"}"#), 0),
                pricing,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("a table served {served} times holds up the Errand"))
        .expect("the Errand answers");

        assert_eq!(
            answer.consumption,
            ErrandConsumption {
                usage: Some(answered_usage()),
                cost: None,
            },
            "a table served {served} times prices nothing"
        );
    }
}

/// Codex reports transient errors it goes on to retry as `error` events, so
/// one is the least of what says why a run failed: the Turn's own failure
/// comes first, then stderr, and the last such error only where neither said.
#[tokio::test]
async fn a_failed_codex_errand_is_explained_by_its_turn_then_stderr_then_its_last_error() {
    let retried = r#"{"type":"error","message":"Reconnecting... 1/5"}"#;
    let cases = [
        (
            format!(
                "{retried}\n{}",
                r#"{"type":"turn.failed","error":{"message":"usage limit reached"}}"#
            ),
            "  printf '%s\\n' 'a warning' >&2\n",
            "usage limit reached",
        ),
        (
            retried.to_owned(),
            "  printf '%s\\n' 'not signed in' >&2\n",
            "not signed in",
        ),
        (retried.to_owned(), "", "Reconnecting... 1/5"),
    ];
    for (events, stderr, reason) in cases {
        let body = format!("{stderr}{}", exec_printing(&events, None, 1));
        let failed = run_errand(PRICED_MODEL, &body)
            .await
            .expect_err("a failed run is no answer");
        assert!(
            failed.to_string().contains(reason),
            "{events:?} with stderr {stderr:?} is explained by {reason:?}: {failed}"
        );
    }
}
