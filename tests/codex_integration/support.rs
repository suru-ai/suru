//! Scripted Codex programs and fixtures shared by more than one area of the tests.

use crate::scripted_binary_support::{captured_methods, captured_requests, write_executable};
use serde_json::Value;
use suru::managed_client::{ManagedClient, ManagedEvent};
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

pub async fn receive_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connecting))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connected(_)))
    ));
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
}

impl ScriptedCodex {
    pub fn new_multiprocess(case_arms: &str) -> Self {
        Self::new(&format!(
            "{MULTIPROCESS_SCRIPT_PREFIX}{case_arms}  esac\ndone\n"
        ))
    }

    pub fn new(script: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Codex directory");
        let executable = directory.path().join("codex");
        let log = directory.path().join("requests.jsonl");
        let release = directory.path().join("release");
        let pid = directory.path().join("pid");
        let exited = directory.path().join("exited");
        let ready = directory.path().join("ready");
        let child_pid = directory.path().join("child-pid");
        let attempts = directory.path().join("attempts");
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
        }
    }

    pub fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    pub async fn wait_for_method(&self, expected: &str) {
        self.wait_for_method_count(expected, 1).await;
    }

    pub async fn wait_for_method_count(&self, expected: &str, count: usize) {
        timeout(Duration::from_secs(2), async {
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
        timeout(Duration::from_secs(2), async {
            while !self.exited.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex exits cooperatively");
    }

    pub async fn wait_until_ready(&self) {
        timeout(Duration::from_secs(2), async {
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

    pub fn requests(&self) -> Vec<Value> {
        captured_requests(&self.log)
    }

    pub fn methods(&self) -> Vec<String> {
        captured_methods(&self.log)
    }
}
