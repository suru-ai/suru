//! The scripted Copilot CLI stand-in the Copilot tests drive the real runtime against.
//!
//! The fixture is a `sh` program speaking the Copilot CLI's own wire — Content-Length-framed
//! JSON-RPC over stdio — so a test exercises the SDK transport, the shared-harness machinery, and
//! the catalog normalization exactly as a real CLI would. It answers requests from `case` arms the
//! test supplies, matched against the request body, with `$COPILOT_FIXTURE_ID` standing for the
//! request's JSON-RPC ID and `reply` framing a response body.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription},
    protocol::{
        CreateSessionRequest, InitialPrompt, Message, MessageRole, PromptId, SessionId,
        SessionSnapshot, TurnId, TurnStatus, Workspace,
    },
    provider::CopilotRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

use crate::{
    scripted_binary_support::{captured_methods, captured_requests, write_executable},
    server_support::receive_initial_state,
};

/// The catalog the picker — and the Agent Selection a test names — draws on.
pub const COPILOT_MODELS: &str = concat!(
    r#"[{"id":"auto","name":"Auto","capabilities":{}},"#,
    r#"{"id":"claude-fixture","name":"Claude Fixture","capabilities":{},"#,
    r#""supportedReasoningEfforts":["low","high"],"defaultReasoningEffort":"high","#,
    r#""supportedContextTiers":["default","long_context"]}]"#,
);

/// Reads one Content-Length-framed request at a time, records it, and dispatches it to the test's
/// arms. `read` on a pipe consumes a byte at a time, so `dd` picks up exactly where the blank line
/// after the header left off.
const SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$COPILOT_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$COPILOT_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$COPILOT_FIXTURE_ATTEMPTS"
printf '%s\n' "$*" > "$COPILOT_FIXTURE_ARGV"
trap 'printf exited > "$COPILOT_FIXTURE_EXITED"' EXIT

reply() {
  printf 'Content-Length: %s\r\n\r\n%s' "$(printf '%s' "$1" | wc -c | tr -d ' ')" "$1"
}

# One entry on the Session's timeline: `event <id> <type> <data-object>`. The Session it belongs to
# is the one the create arm recorded, so the SDK routes it to the Session Suru is driving.
event() {
  reply '{"jsonrpc":"2.0","method":"session.event","params":{"sessionId":"'"$sid"'","event":{"id":"'"$1"'","timestamp":"2026-01-01T00:00:00Z","type":"'"$2"'","data":'"$3"'}}}'
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

/// A `session.create` arm answering with the identifier Suru generated for the Session, and holding
/// on to it so the arms that follow can address their events at that Session.
pub fn create_session_arm() -> String {
    r#"    *'"method":"session.create"'*)
      sid=$(printf '%s' "$body" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"sessionId":"'"$sid"'"}}'
      ;;
"#
    .to_owned()
}

/// A `session.model.getCurrent` arm reporting the Model the Session resolved to, with the reasoning
/// effort and context tier in force on it.
pub fn current_model_arm(model: &str, effort: &str, tier: &str) -> String {
    format!(
        r#"    *'"method":"session.model.getCurrent"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"modelId":"{model}","reasoningEffort":"{effort}","contextTier":"{tier}"}}}}'
      ;;
"#
    )
}

/// A `session.model.switchTo` arm that accepts whatever Model the Turn asks for.
pub fn switch_model_arm() -> String {
    r#"    *'"method":"session.model.switchTo"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"modelId":"switched"}}'
      ;;
"#
    .to_owned()
}

/// A `session.send` arm that accepts the Prompt and then plays `timeline` — `event` lines, and
/// whatever else the test wants the CLI to do while the Turn runs.
pub fn send_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.send"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"messageId":"fixture-message"}}}}'
{timeline}      ;;
"#
    )
}

/// A `session.abort` arm that acknowledges the whole-loop abort and then plays `timeline` — the
/// aborted idle a real CLI reports once its loop has stopped, and whatever else the test wants.
pub fn abort_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.abort"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{}}}}'
{timeline}      ;;
"#
    )
}

/// A `session.abort` arm that never answers, standing in for a CLI that has stopped acknowledging
/// while its read loop still runs.
pub fn silent_abort_arm() -> String {
    r#"    *'"method":"session.abort"'*)
      :
      ;;
"#
    .to_owned()
}

/// An arm accepting the permission decision the harness answers a request with, so a fixture that
/// asks for one is not left waiting.
pub fn permission_decision_arm() -> String {
    r#"    *'"method":"session.permissions.handlePendingPermissionRequest"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
"#
    .to_owned()
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
    release: std::path::PathBuf,
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
        let release = path("release");
        let script = format!("{SCRIPT_PREFIX}{case_arms}  esac\ndone\n")
            .replace("$COPILOT_FIXTURE_LOG", fixture_path(&log))
            .replace("$COPILOT_FIXTURE_ATTEMPTS", fixture_path(&attempts))
            .replace("$COPILOT_FIXTURE_ARGV", fixture_path(&argv))
            .replace("$COPILOT_FIXTURE_EXITED", fixture_path(&exited))
            .replace("$COPILOT_FIXTURE_RELEASE", fixture_path(&release));
        write_executable(&executable, &script);
        Self {
            _directory: directory,
            executable,
            log,
            attempts,
            argv,
            exited,
            release,
        }
    }

    /// Lets a fixture holding at `$COPILOT_FIXTURE_RELEASE` carry on, so a test can arrange the
    /// Session it wants before the CLI plays the rest of its timeline.
    pub fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release the scripted Copilot timeline");
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

    /// The methods the runtime asked the CLI for, in the order they arrived.
    pub fn methods(&self) -> Vec<String> {
        captured_methods(&self.log)
    }

    /// Everything the runtime sent the CLI, in the order it arrived.
    pub fn requests(&self) -> Vec<serde_json::Value> {
        captured_requests(&self.log)
    }

    /// The first request the runtime made for `method`, once it has.
    pub async fn wait_for_request(&self, method: &str) -> serde_json::Value {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(request) = self.requests().into_iter().find(|request| {
                    request.get("method").and_then(serde_json::Value::as_str) == Some(method)
                }) {
                    return request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "scripted Copilot receives {method}; captured methods: {:?}",
                self.methods()
            )
        })
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

/// The arms every Copilot conversation needs before a Prompt reaches it — the handshake, the
/// catalog, Session creation, and the Model the Session runs under — for a test that adds arms of
/// its own beside them.
pub fn conversation_arms() -> String {
    format!(
        "{}{}{}{}{}{}",
        connect_arm(),
        models_arm(COPILOT_MODELS),
        create_session_arm(),
        current_model_arm("claude-fixture", "high", "default"),
        switch_model_arm(),
        permission_decision_arm(),
    )
}

/// A fixture that carries a Session from creation through one Turn, playing `timeline` while it
/// runs.
pub fn conversation_fixture(timeline: &str) -> ScriptedCopilot {
    ScriptedCopilot::new(&format!("{}{}", conversation_arms(), send_arm(timeline)))
}

/// A Copilot Session whose first Turn is running, which is what a steer or an interrupt needs
/// something to act on. Holds everything the Turn runs on for as long as the test does — the state
/// directory and Workspace included, which are only alive while this is.
pub struct LiveTurn {
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    pub client: ManagedClient,
    pub feed: SessionSubscription,
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

impl LiveTurn {
    /// Opens a Session on `runtime` under `name`, delivers `prompt`, and comes back once the Turn
    /// it began is running. `name` is the client channel, so each test needs its own.
    pub async fn start(runtime: CopilotRuntime, name: &'static str, prompt: &str) -> Self {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let server = server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), name).expect("configure server"),
            std::sync::Arc::new(runtime),
        )
        .await
        .expect("spawn server");
        let client = connect(state_dir.path(), name).await;
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: prompt.to_owned(),
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
            "the Prompt begins a Turn Copilot is running",
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

pub async fn connect(state_dir: &std::path::Path, name: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
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
    settled_session_on(client, &mut feed, session_id, turn_index).await
}

/// The same wait over a feed the caller already holds, for a test that must be subscribed before
/// it delivers the Prompt it is waiting on.
pub async fn settled_session_on(
    client: &ManagedClient,
    feed: &mut SessionSubscription,
    session_id: SessionId,
    turn_index: usize,
) -> SessionSnapshot {
    session_where(
        client,
        feed,
        session_id,
        &format!("Copilot Turn {turn_index} settles"),
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
