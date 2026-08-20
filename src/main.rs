use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use suru::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ServerStatus, server_status, start_server, stop_server,
    },
    server::{self, ServerConfig},
    tui,
};

#[derive(Debug, Parser)]
#[command(name = "suru", version, about)]
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
        #[arg(long = "state-dir")]
        state_base_dir: PathBuf,
        #[arg(long = "data-dir")]
        data_base_dir: PathBuf,
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
                "Suru server ready (pid {}, instance {})",
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
                "Suru server stopped (pid {}, instance {})",
                health.identity.pid, health.identity.instance_id
            );
            Ok(())
        }
        Some(CliCommand::InternalServer {
            state_base_dir,
            data_base_dir,
            channel,
        }) => {
            server::spawn(ServerConfig::new(state_base_dir, channel)?.with_data_dir(data_base_dir))
                .await?
                .run_until_ctrl_c()
                .await
        }
        None => {
            let client = ManagedClient::connect(default_client_config()?)
                .await
                .context("prepare the managed Suru server connection")?;
            tui::run(client).await
        }
    }
}

fn default_client_config() -> Result<ManagedClientConfig> {
    let state_base_dir = match std::env::var_os("SURU_STATE_DIR") {
        Some(path) => PathBuf::from(path),
        None => dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .context("determine the current user's state directory")?
            .join("suru"),
    };
    let data_base_dir = match std::env::var_os("SURU_DATA_DIR") {
        Some(path) => PathBuf::from(path),
        None => dirs::data_local_dir()
            .context("determine the current user's data directory")?
            .join("suru"),
    };
    let channel = std::env::var("SURU_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        }
    });
    Ok(ManagedClientConfig::new(state_base_dir, channel)?.with_data_dir(data_base_dir))
}
