//! Scripted Codex programs and fixtures shared by more than one area of the tests.

use std::sync::Arc;

use crate::scripted_binary_support::{captured_methods, captured_requests, write_executable};
use crate::server_support::PROGRESS_DEADLINE;
use serde_json::Value;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, SessionId, SessionSnapshot, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
};
use sysinfo::{Pid, System};
use tokio::time::{Duration, timeout};

const MULTIPROCESS_SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CODEX_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CODEX_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CODEX_FIXTURE_ATTEMPTS"

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
"#;

/// What a scripted Codex with nothing to say about Errands does when Suru
/// invokes it as `codex exec`: refuse before reading a byte, so the Prompt an
/// Errand delivers never reaches the app-server request log the rest of these
/// fixtures are read through. A Provider that will not run an Errand leaves the
/// Prompt-derived Title standing, which is exactly what those tests expect.
const ONE_SHOT_REFUSAL: &str = r#"if [ "$1" = "exec" ]; then
  printf '%s\n' 'this scripted Codex runs no Errands' >&2
  exit 1
fi
"#;

/// The shebang every scripted Codex opens with, and so the line the refusal
/// above is spliced in behind.
const SHEBANG: &str = "#!/bin/sh\n";

/// The app-server arms of a conversation on thread `native-thread` whose one Turn, `native-turn`,
/// streams `__TURN_EVENTS__` — the native lines a test splices in — once Suru starts it.
const CONVERSATION: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
__TURN_EVENTS__      ;;
"#;

/// A scripted Codex holding one conversation whose Turn streams `turn_events`: shell lines
/// printing the app-server's own notifications on `native-thread` under `native-turn`, ending with
/// the `turn/completed` that settles it.
pub fn conversation_codex(turn_events: &str) -> ScriptedCodex {
    ScriptedCodex::new_multiprocess(&CONVERSATION.replace("__TURN_EVENTS__", turn_events))
}

/// A server hosting a scripted Codex, a client connected past its initial state, and a Session
/// opened on a Prompt, with the directories the Session lives in held for the fixture's lifetime.
pub struct OpenedSession {
    pub server: RunningServer,
    pub client: ManagedClient,
    pub session_id: SessionId,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

/// Opens a Session on `prompt` against `codex`. `channel` is the client channel, so each test
/// needs its own.
pub async fn opened_session(codex: &ScriptedCodex, channel: &str, prompt: &str) -> OpenedSession {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
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
                attachments: Vec::new(),
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

/// The Session once `predicate` holds of it, re-read on every published change. `what` names what
/// was being waited for, so a wait that runs out says which one did.
pub async fn session_where(
    client: &ManagedClient,
    session_id: SessionId,
    what: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Session while it streams");
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

/// The Session once the Turn `turn_index` names has settled.
pub async fn settled_session(
    client: &ManagedClient,
    session_id: SessionId,
    turn_index: usize,
) -> SessionSnapshot {
    session_where(
        client,
        session_id,
        &format!("Codex Turn {turn_index} settles"),
        |snapshot| {
            snapshot
                .turns
                .get(turn_index)
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        },
    )
    .await
}

pub async fn receive_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await,
        Ok(Some(ManagedEvent::Connecting))
    ));
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await,
        Ok(Some(ManagedEvent::Connected(_)))
    ));
}

/// Waits for the process `pid` names to be gone, and fails if it outlives the
/// deadline. Used wherever Suru claims to have taken a Codex process down —
/// server shutdown, and a wait abandoned at an Errand's deadline.
pub async fn assert_process_exited(pid: u32) {
    if timeout(PROGRESS_DEADLINE, async {
        loop {
            if System::new_all().process(Pid::from_u32(pid)).is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_err()
    {
        let system = System::new_all();
        if let Some(process) = system.process(Pid::from_u32(pid)) {
            let _ = process.kill();
        }
        panic!("scripted Codex process {pid} outlived the wait Suru gave it");
    }
}

pub struct ScriptedCodex {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    log: std::path::PathBuf,
    release: std::path::PathBuf,
    pid: std::path::PathBuf,
    pub exited: std::path::PathBuf,
    ready: std::path::PathBuf,
    child_pid: std::path::PathBuf,
    /// The stem every file a one-shot Errand run records is named from, so one
    /// substituted placeholder covers what Codex was invoked with, what it was
    /// asked, and where it was asked it.
    errand: std::path::PathBuf,
}

impl ScriptedCodex {
    pub fn new_multiprocess(case_arms: &str) -> Self {
        Self::new(&format!(
            "{MULTIPROCESS_SCRIPT_PREFIX}{case_arms}  esac\ndone\n"
        ))
    }

    /// A scripted Codex that answers over the app-server and runs no Errands.
    pub fn new(script: &str) -> Self {
        let body = script
            .strip_prefix(SHEBANG)
            .expect("a scripted Codex opens with a shebang");
        Self::new_running_errands(&format!("{SHEBANG}{ONE_SHOT_REFUSAL}{body}"))
    }

    /// A scripted Codex whose script answers a one-shot `codex exec` run itself.
    pub fn new_running_errands(script: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Codex directory");
        let executable = directory.path().join("codex");
        let log = directory.path().join("requests.jsonl");
        let release = directory.path().join("release");
        let pid = directory.path().join("pid");
        let exited = directory.path().join("exited");
        let ready = directory.path().join("ready");
        let child_pid = directory.path().join("child-pid");
        let attempts = directory.path().join("attempts");
        let errand = directory.path().join("errand");
        let script = script
            .replace(
                "$CODEX_FIXTURE_LOG",
                log.to_str().expect("fixture log path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_RELEASE",
                release.to_str().expect("fixture release path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_PID",
                pid.to_str().expect("fixture PID path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_EXITED",
                exited.to_str().expect("fixture exit path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_READY",
                ready.to_str().expect("fixture ready path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_CHILD_PID",
                child_pid.to_str().expect("fixture child PID path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_ATTEMPTS",
                attempts.to_str().expect("fixture attempts path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_ERRAND",
                errand.to_str().expect("fixture Errand path is UTF-8"),
            );
        write_executable(&executable, &script);
        Self {
            _directory: directory,
            executable,
            log,
            release,
            pid,
            exited,
            ready,
            child_pid,
            errand,
        }
    }

    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    pub async fn wait_for_method(&self, expected: &str) {
        self.wait_for_method_count(expected, 1).await;
    }

    pub async fn wait_for_method_count(&self, expected: &str, count: usize) {
        timeout(PROGRESS_DEADLINE, async {
            loop {
                if self
                    .requests()
                    .iter()
                    .filter(|request| {
                        request.get("method").and_then(Value::as_str) == Some(expected)
                    })
                    .count()
                    >= count
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "scripted Codex receives {expected}; captured requests: {:?}",
                self.requests()
            )
        });
    }

    pub fn release_turn(&self, turn_index: usize) {
        std::fs::write(
            format!("{}-{turn_index}", self.release.display()),
            b"release",
        )
        .expect("release scripted Codex Turn");
    }

    pub fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release scripted Codex events");
    }

    pub async fn wait_for_exit(&self) {
        timeout(PROGRESS_DEADLINE, async {
            while !self.exited.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex exits cooperatively");
    }

    pub async fn wait_until_ready(&self) {
        timeout(PROGRESS_DEADLINE, async {
            while !self.ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex reports its native Turn ready");
    }

    pub fn pid(&self) -> u32 {
        std::fs::read_to_string(&self.pid)
            .expect("read scripted Codex PID")
            .trim()
            .parse()
            .expect("scripted Codex PID is numeric")
    }

    pub fn child_pid(&self) -> u32 {
        std::fs::read_to_string(&self.child_pid)
            .expect("read scripted Codex child PID")
            .trim()
            .parse()
            .expect("scripted Codex child PID is numeric")
    }

    /// Waits until a one-shot Errand run has recorded everything about itself,
    /// so a test may read all of it back without racing the fixture's writes.
    pub async fn wait_for_errand(&self) {
        timeout(PROGRESS_DEADLINE, async {
            while !self.errand_file("recorded").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex is asked to run an Errand");
    }

    /// What Codex was invoked with for its one-shot Errand run, one argument
    /// per element and in the order they were passed.
    pub fn errand_arguments(&self) -> Vec<String> {
        std::fs::read_to_string(self.errand_file("arguments"))
            .expect("read the Errand's arguments")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The Prompt the one-shot Errand run was handed on its stdin.
    pub fn errand_prompt(&self) -> String {
        std::fs::read_to_string(self.errand_file("prompt")).expect("read the Errand's Prompt")
    }

    /// The output schema the one-shot Errand run was handed, as the file Codex
    /// was pointed at held it.
    pub fn errand_schema(&self) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(self.errand_file("schema"))
                .expect("read the Errand's output schema"),
        )
        .expect("decode the Errand's output schema")
    }

    /// The process the one-shot Errand run was carried out by.
    pub fn errand_pid(&self) -> u32 {
        std::fs::read_to_string(self.errand_file("pid"))
            .expect("read the Errand's PID")
            .trim()
            .parse()
            .expect("the Errand's PID is numeric")
    }

    /// The directory the one-shot Errand run was started in.
    pub fn errand_cwd(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(
            std::fs::read_to_string(self.errand_file("cwd"))
                .expect("read the Errand's working directory")
                .trim(),
        )
    }

    fn errand_file(&self, name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}-{name}", self.errand.display()))
    }

    pub fn requests(&self) -> Vec<Value> {
        captured_requests(&self.log)
    }

    pub fn methods(&self) -> Vec<String> {
        captured_methods(&self.log)
    }
}
