//! The scripted Claude Code CLI stand-in the Claude tests drive the real runtime against.
//!
//! The fixture is a `sh` program speaking the CLI's own wire — newline-delimited stream-json over
//! stdio — so a test exercises the transport, the harness process machinery, and the catalog
//! presentation exactly as a real CLI would. It answers control requests from `case` arms the test
//! supplies, matched against the request line, with `$request_id` standing for the envelope's
//! correlation ID.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription},
    protocol::{
        Message, MessageRole, ModelCatalog, ProviderId, ProviderModelCatalog, SessionId,
        SessionSnapshot, TurnStatus,
    },
    provider::ClaudeRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

use crate::{
    scripted_binary_support::{captured_requests, write_executable},
    server_support::receive_initial_state,
};

/// The rows the fixture's picker advertises, in the CLI's own response shape: the recommended
/// `default` row, an alias row resolving to the same canonical model, a concrete model with effort
/// levels, and one without any effort metadata.
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
    r#"{"value":"tiny","resolvedModel":"claude-fixture-3","displayName":"Tiny","#,
    r#""description":"Fixture 3 · Fastest for quick answers"}]"#,
);

/// Reads one newline-delimited message at a time, records it, and dispatches it to the test's
/// arms with the control envelope's `request_id` extracted for their replies.
const SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CLAUDE_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CLAUDE_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CLAUDE_FIXTURE_ATTEMPTS"
printf '%s\n' "$*" > "$CLAUDE_FIXTURE_ARGV"
trap 'printf "exited\n" >> "$CLAUDE_FIXTURE_EXITED"' EXIT

# One newline-delimited stream-json message on the CLI's stdout.
emit() {
  printf '%s\n' "$1"
}

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CLAUDE_FIXTURE_LOG"
  request_id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$line" in
"#;

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

/// A `list_models` arm whose answer is not the response shape Suru expects.
pub fn malformed_list_models_arm() -> String {
    r#"    *'"subtype":"list_models"'*)
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"models":"not-a-list"}}}'
      ;;
"#
    .to_owned()
}

pub struct ScriptedClaude {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    script: String,
    log: std::path::PathBuf,
    attempts: std::path::PathBuf,
    argv: std::path::PathBuf,
    exited: std::path::PathBuf,
}

impl ScriptedClaude {
    /// A fixture that answers `list_models` with `models`.
    pub fn with_models(models: &str) -> Self {
        Self::new(&list_models_arm(models))
    }

    /// A fixture whose request handling is exactly `case_arms`, each an `sh` `case` arm over the
    /// request line.
    pub fn new(case_arms: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Claude directory");
        let path = |name: &str| directory.path().join(name);
        let executable = path("claude");
        let log = path("requests.jsonl");
        let attempts = path("attempts");
        let argv = path("argv");
        let exited = path("exited");
        let script = format!("{SCRIPT_PREFIX}{case_arms}  esac\ndone\n")
            .replace("$CLAUDE_FIXTURE_LOG", fixture_path(&log))
            .replace("$CLAUDE_FIXTURE_ATTEMPTS", fixture_path(&attempts))
            .replace("$CLAUDE_FIXTURE_ARGV", fixture_path(&argv))
            .replace("$CLAUDE_FIXTURE_EXITED", fixture_path(&exited));
        write_executable(&executable, &script);
        Self {
            _directory: directory,
            executable,
            script,
            log,
            attempts,
            argv,
            exited,
        }
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

    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    /// How many times the fixture has been launched — one per short-lived CLI process.
    pub fn launches(&self) -> usize {
        std::fs::read_to_string(&self.attempts)
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// The arguments the runtime launched the CLI with. A trailing empty argument is invisible
    /// here — the fixture records `$*` — so assertions read the flags, not the values after them.
    pub fn arguments(&self) -> Vec<String> {
        std::fs::read_to_string(&self.argv)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
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

/// A fixture that carries a Session from creation through a Turn: it answers the discovery the
/// Session startup runs, and plays `timeline` for every Prompt the spawned conversation receives.
pub fn conversation_fixture(timeline: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}",
        list_models_arm(CLAUDE_MODELS),
        user_turn_arm(timeline)
    ))
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
    timeout(Duration::from_secs(10), async {
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
