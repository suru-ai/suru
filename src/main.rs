use std::path::PathBuf;

use anyhow::{Context, Result};
use chidori::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ServerStatus, server_status, start_server, stop_server,
    },
    server::{self, ServerConfig},
    tui,
};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "chidori", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    #[command(name = "__server", hide = true)]
    InternalServer {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        channel: String,
    },
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    Start,
    Status,
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Some(CliCommand::Server {
            command: ServerCommand::Start,
        }) => {
            let config = default_client_config()?;
            let health = start_server(&config).await?;
            println!(
                "Chidori server ready (pid {}, instance {})",
                health.identity.pid, health.identity.instance_id
            );
            Ok(())
        }
        Some(CliCommand::Server {
            command: ServerCommand::Status,
        }) => {
            let status = server_status(&default_client_config()?).await?;
            if matches!(status, ServerStatus::Ready(_)) {
                println!("{status}");
                Ok(())
            } else {
                anyhow::bail!(status)
            }
        }
        Some(CliCommand::Server {
            command: ServerCommand::Stop,
        }) => {
            let health = stop_server(&default_client_config()?).await?;
            println!(
                "Chidori server stopped (pid {}, instance {})",
                health.identity.pid, health.identity.instance_id
            );
            Ok(())
        }
        Some(CliCommand::InternalServer { state_dir, channel }) => {
            server::spawn(ServerConfig::new(state_dir, channel)?)
                .await?
                .run_until_ctrl_c()
                .await
        }
        None => {
            let client = ManagedClient::connect(default_client_config()?)
                .await
                .context("prepare the managed Chidori server connection")?;
            tui::run(client).await
        }
    }
}

fn default_client_config() -> Result<ManagedClientConfig> {
    let state_dir = match std::env::var_os("CHIDORI_STATE_DIR") {
        Some(path) => PathBuf::from(path),
        None => dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("determine the current user's state directory")?
            .join("chidori"),
    };
    let channel = std::env::var("CHIDORI_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        }
    });
    ManagedClientConfig::new(state_dir, channel)
}
