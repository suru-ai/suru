use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use suru::{
    logging,
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
        #[arg(long = "config-dir")]
        config_dir: Option<PathBuf>,
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
            config_dir,
            channel,
        }) => {
            let mut config =
                ServerConfig::new(state_base_dir, channel)?.with_data_dir(data_base_dir);
            if let Some(config_dir) = config_dir {
                config = config.with_config_dir(config_dir);
            }
            let _log_guard = logging::init(&config, logging::Role::Server)
                .context("initialize server logging")?;
            server::spawn(config).await?.run_until_ctrl_c().await
        }
        None => {
            let config = default_client_config()?;
            let _log_guard = logging::init(config.runtime(), logging::Role::Client)
                .context("initialize client logging")?;
            let client = ManagedClient::connect(config)
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
    let config_dir = suru::settings::resolve_config_root(
        std::env::var_os("SURU_CONFIG_DIR").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        dirs::home_dir().as_deref(),
    );
    let channel = std::env::var("SURU_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        }
    });
    let mut config =
        ManagedClientConfig::new(state_base_dir, channel)?.with_data_dir(data_base_dir);
    if let Some(config_dir) = config_dir {
        config = config.with_config_dir(config_dir);
    }
    Ok(config)
}
