use std::{net::SocketAddr, num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use suru_relay::{NoIdentityProvider, RelayConfig, TrustedProxy};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// A Relay for Suru: carries Pairings between Servers that cannot reach each
/// other directly, for Servers logged in under one Account.
///
/// It writes one JSON line to standard output for each connection it joins,
/// naming the Account and each Server, and its own diagnostics to standard
/// error.
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
    /// A reverse proxy whose `X-Forwarded-For` header the Relay believes
    /// about the address a Server connects from, named by its address, such
    /// as `10.0.0.5`, or by a network it is among, such as `10.0.0.0/8`.
    /// Give it once for each proxy. With none, the header is ignored.
    #[arg(long = "trusted-proxy", value_name = "ADDRESS")]
    trusted_proxies: Vec<TrustedProxy>,
    /// Requires every Account to have been logged in as, from any one of its
    /// Servers, within this many days: an Account not logged in as for
    /// longer is refused until one of its Servers logs in again, which
    /// restores them all. Unless given, a Login stands however long ago it
    /// was formed.
    #[arg(long, value_name = "DAYS")]
    fresh_login_days: Option<NonZeroU32>,
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
    let mut config = RelayConfig::new(
        arguments.listen,
        arguments.database,
        arguments.public_address,
    )
    .with_trusted_proxies(arguments.trusted_proxies);
    if let Some(days) = arguments.fresh_login_days {
        config = config
            .with_fresh_login_every(Duration::from_secs(u64::from(days.get()) * 24 * 60 * 60));
    }
    // No identity provider or admission rule can be configured yet, so this
    // Relay logs nobody in, and would admit nobody if it did.
    suru_relay::start(config, Arc::new(NoIdentityProvider))
        .await?
        .run_until_ctrl_c()
        .await
}
