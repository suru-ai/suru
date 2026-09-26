//! Records a live Copilot timeline in which a message reaches a working background Subagent.
//!
//! Drives the installed Copilot CLI through the SDK the way Suru's Copilot Provider does, asks the
//! main agent to start a background agent that runs three separate slow shell commands, and has
//! something send that agent a message while it works. Every `session.event` is written to stdout
//! as one JSON line, in stream order.
//!
//! ```sh
//! cargo run --example copilot_steer_capture -- parent  > parent.jsonl  # main agent's write_agent
//! cargo run --example copilot_steer_capture -- sibling > sibling.jsonl # a sibling's write_agent
//! cargo run --example copilot_steer_capture -- rpc     > rpc.jsonl     # tasks.sendMessage, mid-run
//! ```
//!
//! `SURU_COPILOT_PATH` names the CLI (default `copilot`), `CAPTURE_MODEL` the Model (default
//! `gpt-5-mini`). Scrub account details from the output before committing it. What the captures
//! showed is recorded in `docs/validation/0397-copilot-subagent-steer.md`.
use std::{path::PathBuf, sync::Arc, time::Duration};

use github_copilot_sdk::{
    CliProgram, Client, ClientOptions,
    handler::ApproveAllHandler,
    rpc::TasksSendMessageRequest,
    types::{MessageOptions, SessionConfig},
};

const WORKER: &str = "Make THREE SEPARATE bash tool calls, one at a time, waiting for each to \
    finish before making the next (never combine them): first `sleep 15 && echo one`, then \
    `sleep 15 && echo two`, then `sleep 15 && echo three`. Then write a one-paragraph final \
    report of what they printed.";
const STEER: &str = "STEER-MARKER: also say the word PINEAPPLE in your final report";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "parent".to_owned());
    let prompt = match mode.as_str() {
        "parent" => format!(
            "Step 1: use the task tool (mode: background) to start an agent with exactly this \
             prompt: '{WORKER}' Step 2: immediately after it starts, use write_agent to send it: \
             '{STEER}'. Step 3: wait for it with read_agent (wait: true) and report what it said."
        ),
        "sibling" => format!(
            "Step 1: use the task tool (mode: background) to start agent B with exactly this \
             prompt: '{WORKER}' Step 2: right after, use the task tool (mode: background) to \
             start agent A with exactly this prompt: 'Use list_agents to find your sibling \
             agent B, then use write_agent with its agent_id to send it this message: {STEER}. \
             Then reply done.' Step 3: wait for both with read_agent (wait: true) and report \
             what B said."
        ),
        "rpc" => format!(
            "Use the task tool (mode: background) to start an agent with exactly this prompt: \
             '{WORKER}' Then wait for it with read_agent (wait: true) and report what it said."
        ),
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
                .with_include_sub_agent_streaming_events(true)
                .with_permission_handler(Arc::new(ApproveAllHandler)),
        )
        .await?;
    let mut events = session.subscribe();
    session.send(MessageOptions::new(prompt)).await?;
    let mut sent = mode != "rpc";
    let mut subagents_working = 0_i32;
    let mut subagents_started = false;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(300), events.recv()).await {
        println!("{}", serde_json::to_string(&event)?);
        match event.event_type.as_str() {
            "subagent.started" => {
                subagents_started = true;
                subagents_working += 1;
            }
            "subagent.completed" | "subagent.failed" => subagents_working -= 1,
            "session.idle" if subagents_started && subagents_working <= 0 => break,
            // The host's own send lands while the agent's first command runs.
            "tool.execution_start" if !sent => {
                if let Some(agent) = event.agent_id.clone() {
                    sent = true;
                    let result = session
                        .rpc()
                        .tasks()
                        .send_message(TasksSendMessageRequest {
                            from_agent_id: None,
                            id: agent,
                            message: STEER.to_owned(),
                        })
                        .await;
                    eprintln!("tasks.sendMessage -> {result:?}");
                }
            }
            _ => {}
        }
    }
    session.disconnect().await?;
    Ok(())
}
