//! The scripted Claude Code CLI stand-in the Claude tests drive the real runtime against.
//!
//! The fixture is a `sh` program speaking the CLI's own wire — newline-delimited stream-json over
//! stdio — so a test exercises the transport, the harness process machinery, and the catalog
//! presentation exactly as a real CLI would. It answers control requests from `case` arms the test
//! supplies, matched against the request line, with `$request_id` standing for the envelope's
//! correlation ID.

use crate::server_support::PROGRESS_DEADLINE;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription},
    protocol::{
        CreateSessionRequest, InitialPrompt, Message, MessageRole, ModelCatalog, PromptId,
        ProviderId, ProviderModelCatalog, SessionId, SessionSnapshot, TurnId, TurnStatus,
    },
    provider::ClaudeRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

use crate::{
    scripted_binary_support::{captured_requests, write_executable},
    server_support::receive_initial_state,
};

/// The rows the fixture's picker advertises, in the CLI's own response shape: the recommended
/// `default` row, an alias row resolving to the same canonical model, a concrete model with effort
/// levels, and the cheap row without any effort metadata that Claude declares its Errands run at.
/// That last row is named as the live CLI names it, because a declaration is resolved against the
/// catalog by Model ID.
pub const CLAUDE_MODELS: &str = concat!(
    r#"[{"value":"default","resolvedModel":"claude-fixture-1","displayName":"Default (recommended)","#,
    r#""description":"Fixture 1 · Best for everyday tasks","supportsEffort":true,"#,
    r#""supportedEffortLevels":["low","medium","high","xhigh"]},"#,
    r#"{"value":"fixture[1m]","resolvedModel":"claude-fixture-1","displayName":"Fixture (1M context)","#,
    r#""description":"Fixture 1 · Best for everyday tasks","supportsEffort":true,"#,
    r#""supportedEffortLevels":["low","medium","high","xhigh"]},"#,
    r#"{"value":"middling","resolvedModel":"claude-fixture-2","displayName":"Middling","#,
    r#""description":"Fixture 2 · Efficient for routine tasks","supportsEffort":true,"#,
    r#""supportedEffortLevels":["low","medium"]},"#,
    r#"{"value":"haiku","resolvedModel":"claude-fixture-3","displayName":"Haiku","#,
    r#""description":"Fixture 3 · Fastest for quick answers"}]"#,
);

/// The same catalog with the row valued `value` withdrawn, for a test about a Model the CLI has
/// stopped serving. The row is dropped by reading the catalog rather than by re-spelling it, so a
/// change to the fixture's rows cannot leave the withdrawal silently doing nothing.
pub fn models_without(models: &str, value: &str) -> String {
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(models).expect("the fixture's Model rows are JSON");
    let before = rows.len();
    rows.retain(|row| row["value"] != value);
    assert_eq!(
        rows.len() + 1,
        before,
        "the catalog served exactly one row valued {value}"
    );
    serde_json::to_string(&rows).expect("the remaining rows re-serialize")
}

/// Records the launch and its arguments — one line per launch, so a test that restarts the runtime
/// or respawns a Session child can read each launch apart from the others — and defines `emit`.
///
/// Each launch is recorded twice: once whitespace-joined, which is what most assertions read, and
/// once as its working directory and its arguments joined by a separator no argument carries. The
/// second is what survives an argument that is empty or holds spaces, and it is one append of one
/// line, so two processes launched at the same moment cannot interleave their records.
///
/// `$attempt` counts the launches for the arms that answer the CLI's nth launch differently from
/// its first. It is a read-then-write of a shared file, so a fixture whose launches genuinely
/// overlap should not be scripted against it.
const SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CLAUDE_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CLAUDE_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CLAUDE_FIXTURE_ATTEMPTS"
printf '%s\n' "$*" >> "$CLAUDE_FIXTURE_ARGV"
( IFS=$(printf '\037'); printf '%s\t%s\n' "$PWD" "$*" >> "$CLAUDE_FIXTURE_LAUNCHES" )
trap 'printf "exited\n" >> "$CLAUDE_FIXTURE_EXITED"' EXIT

# One newline-delimited stream-json message on the CLI's stdout.
emit() {
  printf '%s\n' "$1"
}

"#;

/// Reads one newline-delimited message at a time, records it, and dispatches it to the test's
/// arms with the control envelope's `request_id` extracted for their replies.
const SCRIPT_LOOP: &str = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CLAUDE_FIXTURE_LOG"
  request_id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$line" in
"#;

/// The version the fixture reports when nothing has downgraded it: the suggestion itself, which is
/// the version Suru's wire behavior is verified against.
pub const CLAUDE_SUGGESTED_VERSION: &str = "2.1.237";

/// A readable version below the suggestion, which remains usable with guidance.
pub const CLAUDE_VERSION_BELOW_SUGGESTION: &str = "2.1.236";

/// A version above the suggestion, standing in for the CLI the user updates to.
pub const CLAUDE_VERSION_ABOVE_SUGGESTION: &str = "2.2.0";

/// What the CLI reports about the account once a user is signed in to it.
pub const SIGNED_IN_ACCOUNT: &str = concat!(
    r#"{"email":"fixture@example.com","organization":"Fixture Org","#,
    r#""subscriptionType":"Claude Max","apiProvider":"firstParty"}"#,
);

/// What it reports while no one is, which is all a signed-out CLI has to say about it.
pub const SIGNED_OUT_ACCOUNT: &str = r#"{"tokenSource":"none","apiProvider":"firstParty"}"#;

/// A `get_binary_version` arm reporting `version`, the way a live CLI answers the probe's first
/// question.
pub fn version_arm(version: &str) -> String {
    format!(
        r#"    *'"subtype":"get_binary_version"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"version":"{version}","buildTime":"2026-08-19T22:15:38Z"}}}}}}'
      ;;
"#
    )
}

/// The same arm for a CLI below the suggestion — until [`ScriptedClaude::upgrade`] leaves the
/// marker that makes it report a version above it, which is the user updating their CLI
/// while Suru is running.
pub fn upgradable_version_arm() -> String {
    format!(
        r#"    *'"subtype":"get_binary_version"'*)
      if [ -e "$CLAUDE_FIXTURE_UPGRADED" ]; then
        printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"version":"{new}"}}}}}}'
      else
        printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"version":"{old}"}}}}}}'
      fi
      ;;
"#,
        new = CLAUDE_VERSION_ABOVE_SUGGESTION,
        old = CLAUDE_VERSION_BELOW_SUGGESTION,
    )
}

/// A `get_binary_version` arm from a CLI that has never heard of the request, which is how one too
/// old for the wire Suru speaks answers a question it predates.
pub fn unknown_version_request_arm() -> String {
    r#"    *'"subtype":"get_binary_version"'*)
      printf '%s\n' '{"type":"control_response","response":{"subtype":"error","request_id":"'"$request_id"'","error":"Unsupported control request subtype: get_binary_version"}}'
      ;;
"#
    .to_owned()
}

/// A `get_binary_version` arm that never answers, standing in for a CLI that has stopped responding
/// while its process still runs.
pub fn silent_version_arm() -> String {
    r#"    *'"subtype":"get_binary_version"'*)
      :
      ;;
"#
    .to_owned()
}

/// An `initialize` arm answering the account probe with `account`, alongside the rest of the
/// handshake a live CLI answers with.
pub fn initialize_arm(account: &str) -> String {
    format!(
        r#"    *'"subtype":"initialize"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"commands":[],"agents":[],"output_style":"default","account":{account}}}}}}}'
      ;;
"#
    )
}

/// The same arm for a CLI no one is signed in to — until [`ScriptedClaude::sign_in`] leaves the
/// marker that makes it report an account, which is the user signing in with the Claude Code CLI
/// while Suru is running.
pub fn signed_out_initialize_arm() -> String {
    format!(
        r#"    *'"subtype":"initialize"'*)
      if [ -e "$CLAUDE_FIXTURE_SIGNED_IN" ]; then
        printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"account":{signed_in}}}}}}}'
      else
        printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"account":{signed_out}}}}}}}'
      fi
      ;;
"#,
        signed_in = SIGNED_IN_ACCOUNT,
        signed_out = SIGNED_OUT_ACCOUNT,
    )
}

/// The control-request subtypes a fixture's log reads as when `subtypes` were asked of a CLI the
/// availability probe found usable: every probe leads with the two questions it puts to the CLI
/// before Suru asks it to do anything.
pub fn after_probe<const N: usize>(subtypes: [&str; N]) -> Vec<String> {
    ["get_binary_version", "initialize"]
        .into_iter()
        .chain(subtypes)
        .map(str::to_owned)
        .collect()
}

/// The arms an availability probe asks of every usable CLI: the version Suru's wire is verified
/// against, and an account a user is signed in to.
pub fn probe_arms() -> String {
    format!(
        "{}{}",
        version_arm(CLAUDE_SUGGESTED_VERSION),
        initialize_arm(SIGNED_IN_ACCOUNT)
    )
}

/// Everything a Model discovery asks of the CLI: the probe deciding whether Claude can be used at
/// all, and the rows it lists once it can.
pub fn discovery_arms(models: &str) -> String {
    format!("{}{}", probe_arms(), list_models_arm(models))
}

/// A `list_models` arm answering with `models`, a JSON array literal in the CLI's row shape.
pub fn list_models_arm(models: &str) -> String {
    format!(
        r#"    *'"subtype":"list_models"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"models":{models}}}}}}}'
      ;;
"#
    )
}

/// A `list_models` arm that leads its real answer with a control response of a subtype this build
/// of Suru has never heard of, standing in for a newer CLI whose wire has drifted.
pub fn drifting_list_models_arm(models: &str) -> String {
    format!(
        r#"    *'"subtype":"list_models"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"novel_subtype","request_id":"'"$request_id"'","novelty":true}}}}'
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"models":{models}}}}}}}'
      ;;
"#
    )
}

/// A `list_models` arm that never answers, standing in for a CLI that has stopped responding
/// while its process still runs.
pub fn silent_list_models_arm() -> String {
    r#"    *'"subtype":"list_models"'*)
      :
      ;;
"#
    .to_owned()
}

/// A `list_models` arm that takes the process down with it, standing in for a CLI that dies
/// mid-discovery.
pub fn crashing_list_models_arm() -> String {
    r#"    *'"subtype":"list_models"'*)
      exit 9
      ;;
"#
    .to_owned()
}

/// A `list_models` arm whose answer is not the response shape Suru expects.
pub fn malformed_list_models_arm() -> String {
    r#"    *'"subtype":"list_models"'*)
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"models":"not-a-list"}}}'
      ;;
"#
    .to_owned()
}

/// An `interrupt` arm that acknowledges the control request the way a live CLI does — an empty
/// interrupt receipt — and then plays `timeline`, the output the stopped loop ends with.
pub fn interrupt_arm(timeline: &str) -> String {
    format!(
        r#"    *'"subtype":"interrupt"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{{"still_queued":[]}}}}}}'
{timeline}      ;;
"#
    )
}

/// An `interrupt` arm that never answers, standing in for a CLI that takes the request and says
/// nothing back.
pub fn silent_interrupt_arm() -> String {
    r#"    *'"subtype":"interrupt"'*)
      :
      ;;
"#
    .to_owned()
}

/// A `stop_task` arm that acknowledges every task it is asked to stop.
pub fn stop_task_arm() -> String {
    r#"    *'"subtype":"stop_task"'*)
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{}}}'
      ;;
"#
    .to_owned()
}

pub struct ScriptedClaude {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    script: String,
    log: std::path::PathBuf,
    argv: std::path::PathBuf,
    /// One line per launch: its working directory, then its arguments exactly as it received them.
    launches: std::path::PathBuf,
    errand_prompt: std::path::PathBuf,
    exited: std::path::PathBuf,
    release: std::path::PathBuf,
    signed_in: std::path::PathBuf,
    upgraded: std::path::PathBuf,
}

/// One launch of the fixture, as a test that must tell one from another reads it.
#[derive(Debug)]
pub struct Launch {
    /// Every argument the process was launched with, exactly — empty values and values carrying
    /// spaces included.
    pub arguments: Vec<String>,
    /// The directory the process was started in.
    pub working_directory: std::path::PathBuf,
}

impl Launch {
    /// Whether the launch carried `flag`, which is how a test tells one kind of launch from
    /// another — only an Errand's print mode is given a JSON schema, only a Session's child is
    /// given a conversation to speak on.
    pub fn carries(&self, flag: &str) -> bool {
        self.arguments.iter().any(|argument| argument == flag)
    }

    /// The value the launch gave `flag`, which is the argument after it.
    pub fn value(&self, flag: &str) -> &str {
        self.arguments
            .iter()
            .position(|argument| argument == flag)
            .and_then(|position| self.arguments.get(position + 1))
            .map(String::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "the CLI was launched with {flag} and a value, got: {:?}",
                    self.arguments
                )
            })
    }
}

impl ScriptedClaude {
    /// A fixture a probe finds usable, answering `list_models` with `models`.
    pub fn with_models(models: &str) -> Self {
        Self::new(&discovery_arms(models))
    }

    /// A fixture whose request handling is exactly `case_arms`, each an `sh` `case` arm over the
    /// request line.
    pub fn new(case_arms: &str) -> Self {
        Self::with_preamble("", case_arms)
    }

    /// The same fixture with `preamble` — `sh` the launched process runs before it reads anything,
    /// so a test can stand in for a CLI that answers its launch flags rather than its input.
    pub fn with_preamble(preamble: &str, case_arms: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Claude directory");
        let path = |name: &str| directory.path().join(name);
        let executable = path("claude");
        let log = path("requests.jsonl");
        let argv = path("argv");
        let attempts = path("attempts");
        let launches = path("launches");
        let errand_prompt = path("errand-prompt");
        let exited = path("exited");
        let release = path("release");
        let signed_in = path("signed-in");
        let upgraded = path("upgraded");
        let script = format!("{SCRIPT_PREFIX}{preamble}{SCRIPT_LOOP}{case_arms}  esac\ndone\n")
            .replace("$CLAUDE_FIXTURE_LOG", fixture_path(&log))
            .replace("$CLAUDE_FIXTURE_ARGV", fixture_path(&argv))
            .replace("$CLAUDE_FIXTURE_ATTEMPTS", fixture_path(&attempts))
            .replace("$CLAUDE_FIXTURE_LAUNCHES", fixture_path(&launches))
            .replace(
                "$CLAUDE_FIXTURE_ERRAND_PROMPT",
                fixture_path(&errand_prompt),
            )
            .replace("$CLAUDE_FIXTURE_EXITED", fixture_path(&exited))
            .replace("$CLAUDE_FIXTURE_RELEASE", fixture_path(&release))
            .replace("$CLAUDE_FIXTURE_SIGNED_IN", fixture_path(&signed_in))
            .replace("$CLAUDE_FIXTURE_UPGRADED", fixture_path(&upgraded));
        write_executable(&executable, &script);
        Self {
            _directory: directory,
            executable,
            script,
            log,
            argv,
            launches,
            errand_prompt,
            exited,
            release,
            signed_in,
            upgraded,
        }
    }

    /// Lets a fixture holding at `$CLAUDE_FIXTURE_RELEASE` carry on, so a test can arrange the
    /// Session it wants while the Turn is still running.
    pub fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release the scripted Claude timeline");
    }

    /// Takes the program off disk, standing in for a Claude Code CLI the user has not installed.
    /// The runtime already holds the path, so [`install`](Self::install) is the user installing it
    /// while Suru runs.
    pub fn uninstall(&self) {
        std::fs::remove_file(&self.executable).expect("uninstall the scripted Claude");
    }

    /// Puts the program back at the path the runtime resolves.
    pub fn install(&self) {
        write_executable(&self.executable, &self.script);
    }

    /// Signs the fixture's user in, which is what [`signed_out_initialize_arm`] answers the account
    /// probe with from here on — the user signing in through the CLI while Suru runs.
    pub fn sign_in(&self) {
        std::fs::write(&self.signed_in, b"signed in").expect("sign the scripted Claude in");
    }

    /// Moves the fixture above the suggested version, which is what [`upgradable_version_arm`] reports
    /// from here on — the user updating their CLI while Suru runs.
    pub fn upgrade(&self) {
        std::fs::write(&self.upgraded, b"upgraded").expect("upgrade the scripted Claude");
    }

    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    /// How many times the fixture has been launched — one per short-lived CLI process.
    pub fn launches(&self) -> usize {
        self.exact_launches().len()
    }

    /// The arguments the runtime launched the CLI with, most recent launch last. A trailing empty
    /// argument is invisible here — the fixture records `$*` — so assertions read the flags, not
    /// the values after them.
    pub fn launch_arguments(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.argv)
            .unwrap_or_default()
            .lines()
            .map(|launch| launch.split_whitespace().map(str::to_owned).collect())
            .collect()
    }

    /// The arguments of the most recent launch.
    pub fn arguments(&self) -> Vec<String> {
        self.launch_arguments().pop().unwrap_or_default()
    }

    /// The value the most recent launch gave `flag`, for a test reading one flag out of a launch
    /// it already knows carries it.
    pub fn argument_value(&self, flag: &str) -> String {
        flag_value(&self.arguments(), flag)
    }

    /// Every launch the fixture has recorded, oldest first, with each one's arguments exactly as
    /// it received them. Unlike [`launch_arguments`](Self::launch_arguments), an empty argument and
    /// one carrying spaces both survive here.
    pub fn exact_launches(&self) -> Vec<Launch> {
        std::fs::read_to_string(&self.launches)
            .unwrap_or_default()
            .lines()
            .filter_map(|record| record.split_once('\t'))
            .map(|(working_directory, arguments)| Launch {
                arguments: arguments.split('\u{1f}').map(str::to_owned).collect(),
                working_directory: std::path::PathBuf::from(working_directory),
            })
            .collect()
    }

    /// The one launch that carried `flag`, for a test reading a launch apart from every other by
    /// what only it asks the CLI for.
    pub fn launch_carrying(&self, flag: &str) -> Launch {
        let mut carrying = self
            .exact_launches()
            .into_iter()
            .filter(|launch| launch.carries(flag))
            .collect::<Vec<_>>();
        assert_eq!(
            carrying.len(),
            1,
            "exactly one launch carries {flag}, got {carrying:?}"
        );
        carrying.pop().expect("the launch carrying the flag")
    }

    /// The Prompts the fixture was given on stdin across every Errand it answered.
    pub fn errand_prompts(&self) -> String {
        std::fs::read_to_string(&self.errand_prompt).unwrap_or_default()
    }

    /// Everything the runtime sent the CLI, in the order it arrived.
    pub fn requests(&self) -> Vec<serde_json::Value> {
        captured_requests(&self.log)
    }

    /// The control-request subtypes the runtime asked the CLI for, in the order they arrived.
    pub fn control_subtypes(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter_map(|request| {
                request
                    .pointer("/request/subtype")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    pub async fn wait_for_exit(&self) {
        self.wait_for_exits(1).await;
    }

    /// Waits until `count` launched processes have exited cooperatively, for a test that must
    /// tell one process's exit from another's — the short-lived discovery's from the Session
    /// child's.
    pub async fn wait_for_exits(&self, count: usize) {
        tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
            loop {
                let exited = std::fs::read_to_string(&self.exited)
                    .unwrap_or_default()
                    .lines()
                    .count();
                if exited >= count {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} scripted Claude processes exit cooperatively"));
    }
}

fn fixture_path(path: &std::path::Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

/// The value `arguments` gives `flag`, which is the argument after it.
pub fn flag_value(arguments: &[String], flag: &str) -> String {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|position| arguments.get(position + 1))
        .cloned()
        .unwrap_or_else(|| {
            panic!("the CLI was launched with {flag} and a value, got: {arguments:?}")
        })
}

/// A preamble answering the print-mode launch an Errand runs through: it records the Prompt it was
/// handed on stdin, then does `answer` — `sh` of the caller's own — without ever reaching the
/// request loop, the way the CLI's own one-shot does.
///
/// The launch is told apart from every other by `--json-schema`, which only an Errand asks for.
fn errand_arm(answer: &str) -> String {
    format!(
        r#"case "$*" in
  *--json-schema*)
    cat >> "$CLAUDE_FIXTURE_ERRAND_PROMPT"
    {answer}
    exit 0
    ;;
esac

"#
    )
}

/// The preamble for an Errand the CLI answers with `envelope`, the single result object print mode
/// prints.
pub fn errand_preamble(envelope: &str) -> String {
    errand_arm(&format!(r#"printf '%s\n' '{envelope}'"#))
}

/// A print-mode envelope answering an Errand with `title` and `icon`, in the shape the CLI shapes
/// a schema-validated answer into: the prose in `result`, and the parsed answer beside it.
pub fn derived_title_envelope(title: &str, icon: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"result","subtype":"success","is_error":false,"#,
            r#""result":"a title","structured_output":{{"title":"{title}","icon":"{icon}"}}}}"#,
        ),
        title = title,
        icon = icon,
    )
}

/// The preamble for an Errand the CLI takes and never answers: it holds the process open and says
/// nothing, standing in for a wedged CLI.
pub fn silent_errand_preamble() -> String {
    errand_arm("sleep 30")
}

/// The preamble for an Errand launch that dies before printing anything, standing in for a CLI that
/// cannot run at all — one no user is signed in to, or one whose flags it does not understand.
pub fn crashing_errand_preamble() -> String {
    errand_arm(
        r#"printf 'the fixture will not run\n' >&2
    exit 9"#,
    )
}

/// A print-mode envelope from a CLI that could not answer at all, which is how an Errand fails
/// without the process ever failing.
pub fn failed_errand_envelope() -> String {
    concat!(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"#,
        r#""errors":["the fixture would not write a title"]}"#,
    )
    .to_owned()
}

/// A preamble standing in for a CLI asked to resume a conversation it does not have: it reports
/// the conversation is missing and exits, which is how a live 2.1.237 CLI answers `--resume` for
/// a session id it cannot find — a terminal `result` naming the session, then a failing exit.
pub fn rejecting_resume_preamble() -> String {
    r#"resumed=$(printf '%s' "$*" | sed -n 's/.*--resume \([^ ]*\).*/\1/p')
if [ -n "$resumed" ]; then
  emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":0,"num_turns":0,"session_id":"'"$resumed"'","errors":["No conversation found with session ID: '"$resumed"'"]}'
  exit 1
fi

"#
    .to_owned()
}

/// A server hosting Claude alone, driven against `claude`, and a client connected to it. `name`
/// is the client channel, so each test needs its own.
pub async fn hosting(
    claude: &ScriptedClaude,
    name: &'static str,
    state_dir: &std::path::Path,
) -> (RunningServer, ManagedClient) {
    hosting_runtime(ClaudeRuntime::new(claude.executable()), name, state_dir).await
}

/// The same server over a runtime the caller has already tuned, for a test injecting timings.
pub async fn hosting_runtime(
    runtime: ClaudeRuntime,
    name: &'static str,
    state_dir: &std::path::Path,
) -> (RunningServer, ManagedClient) {
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir, name).expect("configure server"),
        std::sync::Arc::new(runtime),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    (server, client)
}

/// A client connected to the server at `state_dir` under `name`, past its initial state.
pub async fn connect(state_dir: &std::path::Path, name: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
}

/// Claude's own place in a catalog covering every hosted Provider.
pub fn claude_catalog(catalog: &ModelCatalog) -> &ProviderModelCatalog {
    catalog
        .providers
        .iter()
        .find(|provider| provider.provider == ProviderId::new("claude"))
        .expect("the catalog lists the Claude Provider")
}

/// A user-message arm that plays `timeline` — `emit` lines of stream-json output — once a Prompt
/// is delivered into the running loop.
pub fn user_turn_arm(timeline: &str) -> String {
    format!(
        r#"    *'"type":"user"'*)
{timeline}      ;;
"#
    )
}

/// A user-message arm that plays nothing at all, for a Turn a test only needs to have started.
pub fn silent_user_turn_arm() -> String {
    user_turn_arm("      :\n")
}

/// The arms that carry a Session from creation through a Turn: the discovery its startup runs, and
/// `timeline` played for every Prompt the spawned conversation receives.
pub fn conversation_arms(timeline: &str) -> String {
    format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(timeline)
    )
}

/// A fixture answering exactly those arms.
pub fn conversation_fixture(timeline: &str) -> ScriptedClaude {
    ScriptedClaude::new(&conversation_arms(timeline))
}

/// A server hosting the scripted Claude, a client connected past its initial state, and the
/// Session `prompt` opened, with the directories the Session lives in held for the fixture's
/// lifetime. `name` is the client channel, so each test needs its own.
pub struct OpenedSession {
    pub server: RunningServer,
    pub client: ManagedClient,
    pub session_id: SessionId,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

pub async fn opened_session(
    claude: &ScriptedClaude,
    name: &'static str,
    prompt: &str,
) -> OpenedSession {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), name).expect("configure server"),
        std::sync::Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), name).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: prompt.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    OpenedSession {
        server,
        client,
        session_id: created.session.id,
        _state_dir: state_dir,
        _workspace: workspace,
    }
}

/// The agent Messages in `snapshot`, in Transcript order — the Prompt's own Message is a Message
/// too, and it is never what a Provider produced.
pub fn agent_messages(snapshot: &SessionSnapshot) -> Vec<&Message> {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Agent)
        .collect()
}

/// A Session whose first Turn is running, with everything a test needs to steer or interrupt it.
pub struct LiveTurn {
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    _config_dir: Option<tempfile::TempDir>,
    server: RunningServer,
    pub client: ManagedClient,
    pub feed: SessionSubscription,
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

impl LiveTurn {
    /// Opens a Session on `runtime` under `name`, delivers `prompt`, and comes back once the Turn
    /// it began is running. `name` is the client channel, so each test needs its own.
    pub async fn start(runtime: ClaudeRuntime, name: &'static str, prompt: &str) -> Self {
        Self::start_with_config(runtime, name, prompt, None).await
    }

    pub async fn start_configured(
        runtime: ClaudeRuntime,
        name: &'static str,
        prompt: &str,
        document: &str,
    ) -> Self {
        Self::start_with_config(runtime, name, prompt, Some(document)).await
    }

    async fn start_with_config(
        runtime: ClaudeRuntime,
        name: &'static str,
        prompt: &str,
        document: Option<&str>,
    ) -> Self {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let config_dir = document.map(|document| {
            let directory = tempfile::tempdir().expect("create isolated config directory");
            std::fs::write(directory.path().join("suru.jsonc"), document)
                .expect("write Config Document");
            directory
        });
        let mut config = ServerConfig::new(state_dir.path(), name).expect("configure server");
        if let Some(directory) = &config_dir {
            config = config.with_config_dir(directory.path());
        }
        let server = server::spawn_with_provider(config, std::sync::Arc::new(runtime))
            .await
            .expect("spawn server");
        let client = connect(state_dir.path(), name).await;
        let created = client
            .create_session(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: prompt.to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create Session");
        let mut feed = client
            .subscribe_session(created.session.id)
            .await
            .expect("subscribe to Session SSE");
        let running = session_where(
            &client,
            &mut feed,
            created.session.id,
            "the Prompt begins a Turn Claude is running",
            |snapshot| {
                snapshot
                    .turns
                    .first()
                    .is_some_and(|turn| turn.status == TurnStatus::Active)
            },
        )
        .await;
        Self {
            _state_dir: state_dir,
            _workspace: workspace,
            _config_dir: config_dir,
            server,
            client,
            feed,
            session_id: created.session.id,
            turn_id: running.turns[0].id,
        }
    }

    /// The Session once `predicate` holds of it, over this fixture's own feed.
    pub async fn wait_for(
        &mut self,
        what: &str,
        predicate: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        session_where(
            &self.client,
            &mut self.feed,
            self.session_id,
            what,
            predicate,
        )
        .await
    }

    pub async fn shutdown(self) {
        let Self {
            server,
            client,
            feed,
            ..
        } = self;
        drop(feed);
        drop(client);
        server.shutdown().await.expect("shut the server down");
    }
}

/// The Session once the Turn at `turn_index` has stopped running, whatever it settled as, read
/// from a Session feed opened for the wait.
pub async fn settled_session(
    client: &ManagedClient,
    session_id: SessionId,
    turn_index: usize,
) -> SessionSnapshot {
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    session_where(
        client,
        &mut feed,
        session_id,
        &format!("Claude Turn {turn_index} settles"),
        |snapshot| {
            snapshot
                .turns
                .get(turn_index)
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        },
    )
    .await
}

/// The Session once `predicate` holds of it, read from a feed the caller already holds. `what`
/// names what was being waited for, so a wait that runs out says which one did.
pub async fn session_where(
    client: &ManagedClient,
    feed: &mut SessionSubscription,
    session_id: SessionId,
    what: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Session while its Turn runs");
            if predicate(&snapshot) {
                return snapshot;
            }
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"))
}
