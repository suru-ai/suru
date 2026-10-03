//! Records a live Copilot timeline in which the main agent runs a shell in the background.
//!
//! Drives the installed Copilot CLI through the SDK the way Suru's Copilot Provider does and
//! writes every `session.event` but the deltas to stdout as one JSON line, in stream order, each
//! stamped with `_elapsed_ms`. Each `session.background_tasks_changed` is followed by a
//! `_tasks_list` line holding what `session.tasks.list` answered at that point. The capture ends at
//! `session.idle`, which Copilot holds back until no attached background work is left.
//!
//! ```sh
//! cargo run --example copilot_background_shell_capture -- attached > attached.jsonl # left running
//! cargo run --example copilot_background_shell_capture -- detached > detached.jsonl # detach: true
//! cargo run --example copilot_background_shell_capture -- waited   > waited.jsonl   # read_bash
//! cargo run --example copilot_background_shell_capture -- stopped  > stopped.jsonl  # stop_bash
//! cargo run --example copilot_background_shell_capture -- cancel   > cancel.jsonl   # tasks.cancel
//! cargo run --example copilot_background_shell_capture -- abort    > abort.jsonl    # session.abort
//! ```
//!
//! The `cancel` mode cancels the shell through the task roster once the loop is idle, the way Suru
//! stops a Watch; `abort` aborts a loop waiting on a synchronous shell. `SURU_COPILOT_PATH` names
//! the CLI (default `copilot`), `CAPTURE_MODEL` the Model (default `gpt-5-mini`). On 1.0.91 these
//! showed what ADR 0030 records of Copilot's attached background shells (#379).
use std::{path::PathBuf, sync::Arc, time::Duration};

use github_copilot_sdk::{
    CliProgram, Client, ClientOptions,
    handler::ApproveAllHandler,
    rpc::TasksCancelRequest,
    types::{MessageOptions, SessionConfig},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "attached".to_owned());
    let prompt = match mode.as_str() {
        "attached" => {
            "Run `sleep 20 && echo BG-DONE` with your bash tool using mode \"async\" (do NOT set \
             detach). Do not wait for it, do not read its output. Immediately reply with the \
             single word STARTED and end your turn."
        }
        "detached" => {
            "Run `sleep 20 && echo BG-DONE` with your bash tool using mode \"async\" and detach: \
             true. Do not wait for it. Immediately reply with the single word STARTED and end \
             your turn."
        }
        "waited" => {
            "Run `sleep 5 && echo BG-DONE` with your bash tool using mode \"async\". Then call \
             read_bash on that shell with delay 15 to wait for it to finish. Then reply DONE."
        }
        "stopped" => {
            "Run `sleep 60 && echo BG-DONE` with your bash tool using mode \"async\". Then \
             immediately call stop_bash on that shell. Then reply STOPPED."
        }
        "cancel" => {
            "Run `sleep 60 && echo BG-DONE` with your bash tool using mode \"async\" (do NOT set \
             detach). Do not wait for it. Immediately reply with the single word STARTED and end \
             your turn."
        }
        "abort" => {
            "Run `sleep 30 && echo DONE` with your bash tool (normal, synchronous mode) and wait \
             for it."
        }
        other => return Err(format!("unknown mode {other}").into()),
    };
    let program = std::env::var_os("SURU_COPILOT_PATH").unwrap_or_else(|| "copilot".into());
    let model = std::env::var("CAPTURE_MODEL").unwrap_or_else(|_| "gpt-5-mini".to_owned());
    let client =
        Client::start(ClientOptions::new().with_program(CliProgram::Path(PathBuf::from(program))))
            .await?;
    let session = client
        .create_session(
            SessionConfig::default()
                .with_model(model)
                .with_streaming(true)
                .with_permission_handler(Arc::new(ApproveAllHandler)),
        )
        .await?;
    let mut events = session.subscribe();
    let started = std::time::Instant::now();
    let elapsed = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    session.send(MessageOptions::new(prompt)).await?;
    let mut last_shell = None;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(90), events.recv()).await {
        let kind = event.event_type.clone();
        if kind.ends_with("_delta") {
            continue;
        }
        let mut line = serde_json::to_value(&event)?;
        line["_elapsed_ms"] = elapsed().into();
        println!("{line}");
        match kind.as_str() {
            "session.background_tasks_changed" => {
                let listed = session.rpc().tasks().list().await?;
                if let Some(shell) = listed
                    .tasks
                    .iter()
                    .rev()
                    .find(|task| task["type"] == "shell")
                {
                    last_shell = shell["id"].as_str().map(str::to_owned);
                }
                println!(
                    "{}",
                    serde_json::json!({ "_tasks_list": listed.tasks, "_elapsed_ms": elapsed() })
                );
            }
            "tool.execution_start" if mode == "abort" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                eprintln!("session.abort -> {:?}", session.abort().await);
            }
            "assistant.idle" if mode == "cancel" => {
                if let Some(shell) = last_shell.clone() {
                    let cancelled = session
                        .rpc()
                        .tasks()
                        .cancel(TasksCancelRequest { id: shell })
                        .await;
                    println!(
                        "{}",
                        serde_json::json!({ "_cancel": format!("{cancelled:?}"), "_elapsed_ms": elapsed() })
                    );
                }
            }
            "session.idle" => break,
            _ => {}
        }
    }
    session.disconnect().await?;
    Ok(())
}
