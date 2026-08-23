//! The scripted Copilot CLI stand-in the Copilot tests drive the real runtime against.
//!
//! The fixture is a `sh` program speaking the Copilot CLI's own wire — Content-Length-framed
//! JSON-RPC over stdio — so a test exercises the SDK transport, the shared-harness machinery, and
//! the catalog normalization exactly as a real CLI would. It answers requests from `case` arms the
//! test supplies, matched against the request body, with `$COPILOT_FIXTURE_ID` standing for the
//! request's JSON-RPC ID and `reply` framing a response body.

use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tokio::time::{Duration, timeout};

/// Reads one Content-Length-framed request at a time, records it, and dispatches it to the test's
/// arms. `read` on a pipe consumes a byte at a time, so `dd` picks up exactly where the blank line
/// after the header left off.
const SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$COPILOT_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$COPILOT_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$COPILOT_FIXTURE_ATTEMPTS"
printf '%s\n' "$$" > "$COPILOT_FIXTURE_PID"
printf '%s\n' "$*" > "$COPILOT_FIXTURE_ARGV"
trap 'printf exited > "$COPILOT_FIXTURE_EXITED"' EXIT

reply() {
  printf 'Content-Length: %s\r\n\r\n%s' "$(printf '%s' "$1" | wc -c | tr -d ' ')" "$1"
}

while IFS= read -r header; do
  case "$header" in
    Content-Length:*) ;;
    *) continue ;;
  esac
  length=$(printf '%s' "$header" | tr -dc '0-9')
  IFS= read -r blank
  body=$(dd bs=1 count="$length" 2>/dev/null)
  printf '%s\n' "$body" >> "$COPILOT_FIXTURE_LOG"
  id=$(printf '%s' "$body" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\).*/\1/p')
  case "$body" in
"#;

/// Answers the protocol-version handshake with the version this SDK build speaks, so the fixture
/// stays compatible when the SDK is upgraded.
pub fn connect_arm() -> String {
    format!(
        r#"    *'"method":"connect"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"ok":true,"protocolVersion":{},"version":"0.0.0-fixture"}}}}'
      ;;
"#,
        github_copilot_sdk::SDK_PROTOCOL_VERSION
    )
}

/// A `models.list` arm answering with `models`, a JSON array literal.
pub fn models_arm(models: &str) -> String {
    format!(
        r#"    *'"method":"models.list"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"models":{models}}}}}'
      ;;
"#
    )
}

pub struct ScriptedCopilot {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    log: std::path::PathBuf,
    attempts: std::path::PathBuf,
    argv: std::path::PathBuf,
    exited: std::path::PathBuf,
}

impl ScriptedCopilot {
    /// A fixture that handles the handshake and answers `models.list` with `models`.
    pub fn with_models(models: &str) -> Self {
        Self::new(&format!("{}{}", connect_arm(), models_arm(models)))
    }

    /// A fixture whose request handling is exactly `case_arms`, each an `sh` `case` arm over the
    /// request body.
    pub fn new(case_arms: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Copilot directory");
        let path = |name: &str| directory.path().join(name);
        let executable = path("copilot");
        let log = path("requests.jsonl");
        let attempts = path("attempts");
        let argv = path("argv");
        let exited = path("exited");
        let script = format!("{SCRIPT_PREFIX}{case_arms}  esac\ndone\n")
            .replace("$COPILOT_FIXTURE_LOG", fixture_path(&log))
            .replace("$COPILOT_FIXTURE_ATTEMPTS", fixture_path(&attempts))
            .replace("$COPILOT_FIXTURE_ARGV", fixture_path(&argv))
            .replace("$COPILOT_FIXTURE_PID", fixture_path(&path("pid")))
            .replace("$COPILOT_FIXTURE_EXITED", fixture_path(&exited));
        std::fs::write(&executable, script).expect("write scripted Copilot executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("read scripted Copilot metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("make scripted Copilot executable");
        Self {
            _directory: directory,
            executable,
            log,
            attempts,
            argv,
            exited,
        }
    }

    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    /// How many times the fixture has been launched — one per shared-harness process.
    pub fn launches(&self) -> usize {
        std::fs::read_to_string(&self.attempts)
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// The arguments the runtime launched the CLI with.
    pub fn arguments(&self) -> Vec<String> {
        std::fs::read_to_string(&self.argv)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("decode captured Copilot request"))
            .collect()
    }

    pub fn methods(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter_map(|request| {
                request
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    pub async fn wait_for_exit(&self) {
        timeout(Duration::from_secs(2), async {
            while !self.exited.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Copilot exits cooperatively");
    }
}

fn fixture_path(path: &std::path::Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}
