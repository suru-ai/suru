use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::Parser;
use suru_relay::{NoIdentityProvider, RelayConfig};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

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
    /// The address Servers reach this Relay at, as its users add it — such as
    /// `https://relay.example.com`. Every Server's proof names it.
    #[arg(long)]
    public_address: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(suru_relay::log_layer(
            std::env::var("RUST_LOG").ok().as_deref(),
            std::io::stderr,
        ))
        .init();
    let arguments = Arguments::parse();
    // No identity provider can be configured yet, so this Relay logs nobody
    // in.
    suru_relay::start(
        RelayConfig::new(
            arguments.listen,
            arguments.database,
            arguments.public_address,
        ),
        Arc::new(NoIdentityProvider),
    )
    .await?
    .run_until_ctrl_c()
    .await
}
