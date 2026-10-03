//! A Relay: a server through which Servers that cannot reach each other
//! directly carry a Pairing (ADR-0045).
//!
//! Every Server connects outward to its Relay over a WebSocket and proves
//! itself by its identity key. The Relay admits by login: a Server's user logs
//! in through an identity provider only the Relay speaks to, and the Relay
//! ties the Account that identity answers to to the Server's key as a Login,
//! which stands until it is removed (ADR-0046, ADR-0048). The Relay holds no
//! Provider or Session of Suru's, and none of the trust a Pairing holds.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use suru_relay_protocol::{ENDPOINT_PATH, SPOKEN, Version};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

mod clock;
mod connection;
mod identity;
mod store;

pub use clock::Clock;
pub use identity::{
    DeviceLogin, Identity, IdentityProvider, LoginRefusal, NoIdentityProvider,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
pub use store::{Account, Login, Store};

/// How a Relay is run.
#[derive(Clone)]
pub struct RelayConfig {
    listen: SocketAddr,
    database: PathBuf,
    versions: Vec<Version>,
    clock: Clock,
}

impl RelayConfig {
    /// A Relay listening for plain HTTP at `listen` and keeping its records in
    /// the SQLite database at `database`.
    pub fn new(listen: SocketAddr, database: impl Into<PathBuf>) -> Self {
        Self {
            listen,
            database: database.into(),
            versions: SPOKEN.to_vec(),
            clock: Clock::system(),
        }
    }

    /// Has the Relay speak `versions` of its protocol rather than this
    /// build's own, so a test can stand it beside a Server it disagrees with.
    pub fn with_protocol_versions(mut self, versions: Vec<Version>) -> Self {
        self.versions = versions;
        self
    }

    /// Has the Relay read the time it stamps its records with from `clock`.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// A Relay serving Servers until it is shut down.
pub struct RunningRelay {
    address: SocketAddr,
    store: Store,
    stopping: watch::Sender<bool>,
    task: JoinHandle<Result<()>>,
}

impl RunningRelay {
    /// The address the Relay listens at.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The Relay's records.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Stops the Relay, ending every Server's connection to it.
    pub async fn shutdown(self) -> Result<()> {
        self.stopping.send_replace(true);
        self.task.await.context("the Relay's task panicked")?
    }

    /// Runs the Relay until it fails or the process is interrupted.
    pub async fn run_until_ctrl_c(mut self) -> Result<()> {
        tokio::select! {
            served = &mut self.task => served.context("the Relay's task panicked")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                self.shutdown().await
            }
        }
    }
}

/// Starts a Relay that logs Servers' users in through `provider`.
pub async fn start(
    config: RelayConfig,
    provider: Arc<dyn IdentityProvider>,
) -> Result<RunningRelay> {
    let store = Store::open(&config.database)?;
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listen at {}", config.listen))?;
    let address = listener.local_addr().context("read the Relay's address")?;
    let (stopping, stopping_rx) = watch::channel(false);
    let relay = Arc::new(connection::Relay {
        store: store.clone(),
        provider,
        versions: config.versions,
        clock: config.clock,
        stopping: stopping_rx.clone(),
    });
    let app = Router::new()
        .route(ENDPOINT_PATH, get(connection::connect))
        .with_state(relay);
    let mut stopped = stopping_rx;
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.wait_for(|stopping| *stopping).await;
            })
            .await
            .context("serve Servers")
    });
    tracing::info!(%address, "Relay ready");
    Ok(RunningRelay {
        address,
        store,
        stopping,
        task,
    })
}
