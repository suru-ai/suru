use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::Parser;
use suru_relay::{NoIdentityProvider, RelayConfig};

/// A Relay for Suru: carries Pairings between Servers that cannot reach each
/// other directly, for Servers logged in under one Account.
#[derive(Parser)]
#[command(version)]
struct Arguments {
    /// Where to listen for plain HTTP, behind a reverse proxy that serves
    /// HTTPS.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// The SQLite database the Relay keeps its Accounts and Logins in.
    #[arg(long, default_value = "suru-relay.db")]
    database: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let arguments = Arguments::parse();
    // No identity provider can be configured yet, so this Relay logs nobody
    // in.
    suru_relay::start(
        RelayConfig::new(arguments.listen, arguments.database),
        Arc::new(NoIdentityProvider),
    )
    .await?
    .run_until_ctrl_c()
    .await
}
