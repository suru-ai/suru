//! The scripted Claude Code CLI stand-in the Claude tests drive the real runtime against.
//!
//! The fixture is a `sh` program speaking the CLI's own wire — newline-delimited stream-json over
//! stdio — so a test exercises the transport, the harness process machinery, and the catalog
//! presentation exactly as a real CLI would. It answers control requests from `case` arms the test
//! supplies, matched against the request line, with `$request_id` standing for the envelope's
//! correlation ID.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{ModelCatalog, ProviderId, ProviderModelCatalog},
    provider::ClaudeRuntime,
    server::{self, RunningServer, ServerConfig},
};

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
trap 'printf exited > "$CLAUDE_FIXTURE_EXITED"' EXIT

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
        tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
            while !self.exited.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Claude exits cooperatively");
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

/// Claude's own place in a catalog covering every hosted Provider.
pub fn claude_catalog(catalog: &ModelCatalog) -> &ProviderModelCatalog {
    catalog
        .providers
        .iter()
        .find(|provider| provider.provider == ProviderId::new("claude"))
        .expect("the catalog lists the Claude Provider")
}
