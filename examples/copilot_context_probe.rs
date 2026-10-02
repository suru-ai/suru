//! Probes which of Copilot's context limits `session.usage_info`'s `tokenLimit` reports, the
//! capacity Suru's Context Fill takes from it.
//!
//! For each Model named, prints its catalog limits, runs one short Turn, and prints every
//! `session.usage_info` and `assistant.usage` report beside the Session's
//! `metadata.getContextAttribution` limits. Named no Model, it lists every Model's catalog limits
//! and runs no Turn:
//!
//! ```sh
//! cargo run --example copilot_context_probe                      # catalog limits only
//! cargo run --example copilot_context_probe -- gpt-5-mini claude-haiku-4.5
//! ```
//!
//! `SURU_COPILOT_PATH` names the CLI (default `copilot`). What the probe showed is recorded in
//! `docs/validation/0299-copilot-context-fill.md`.
use std::path::PathBuf;
use std::time::Duration;

use github_copilot_sdk::{CliProgram, Client, ClientOptions, MessageOptions, types::SessionConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let models: Vec<String> = std::env::args().skip(1).collect();
    let program = std::env::var_os("SURU_COPILOT_PATH").unwrap_or_else(|| "copilot".into());
    let client =
        Client::start(ClientOptions::new().with_program(CliProgram::Path(PathBuf::from(program))))
            .await?;
    let catalog = client.rpc().models().list().await?.models;
    if models.is_empty() {
        for listed in &catalog {
            println!(
                "[PROBE] {:<28} {}",
                listed.id,
                serde_json::to_string(&listed.capabilities.limits)?
            );
        }
    }

    for model in &models {
        match catalog.iter().find(|m| &m.id == model) {
            Some(listed) => println!(
                "[PROBE] {model} catalog limits: {}",
                serde_json::to_string(&listed.capabilities.limits)?
            ),
            None => println!("[PROBE] {model} is not in models.list"),
        }

        let session = client
            .create_session(SessionConfig::default().with_model(model))
            .await?;
        let mut events = session.subscribe();
        let watcher = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match event.event_type.as_str() {
                    "session.usage_info" => println!("[PROBE]   usage_info {}", event.data),
                    "assistant.usage" => println!(
                        "[PROBE]   assistant.usage model={} maxPromptTokens={} maxOutputTokens={}",
                        event.data["model"],
                        event.data["maxPromptTokens"],
                        event.data["maxOutputTokens"]
                    ),
                    _ => {}
                }
            }
        });
        let reply = session
            .send_and_wait(MessageOptions::new(
                "Reply with exactly the single word OK and nothing else.",
            ))
            .await;
        println!(
            "[PROBE]   turn -> {}",
            if reply.is_ok() { "Ok" } else { "Err" }
        );
        // Ephemeral reports may trail the idle that ends the Turn.
        tokio::time::sleep(Duration::from_secs(2)).await;
        match session.rpc().metadata().get_context_attribution().await {
            Ok(result) => match result.context_attribution {
                Some(a) => println!(
                    "[PROBE]   attribution model={} totalTokens={} promptTokenLimit={} limit={} \
                     bufferTokens={} compactionThreshold={}",
                    a.model_id,
                    a.total_tokens,
                    a.prompt_token_limit,
                    a.limit,
                    a.buffer_tokens,
                    a.compaction_threshold
                ),
                None => println!("[PROBE]   attribution uninitialized"),
            },
            Err(e) => println!("[PROBE]   attribution Err({e})"),
        }
        watcher.abort();
    }
    client.stop().await?;
    Ok(())
}
