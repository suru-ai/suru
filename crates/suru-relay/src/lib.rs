//! A Relay: a server through which Servers that cannot reach each other
//! directly carry a Pairing (ADR-0045).
//!
//! Every Server connects outward to its Relay over a WebSocket and proves
//! itself by its identity key. The Relay admits by login: a Server's user logs
//! in through an identity provider only the Relay speaks to, and the Relay
//! ties the Account that identity answers to to the Server's key as a Login,
//! which stands until it is removed (ADR-0046, ADR-0048). A Server that
//! Serves through the Relay waits there to be reached, and the Relay joins it
//! to another Server under the same Account that asks for it, carrying the
//! bytes between them unread. The Relay holds no Provider or Session of
//! Suru's, and none of the trust a Pairing holds.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use suru_relay_protocol::{ENDPOINT_PATH, SPOKEN, Version, canonical_address};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing_subscriber::{
    Layer, Registry,
    filter::{EnvFilter, FilterExt, LevelFilter, Targets},
};

mod clock;
mod connection;
mod identity;
mod joiner;
mod store;

pub use clock::Clock;
pub use identity::{
    DeviceLogin, Identity, IdentityProvider, LoginRefusal, NoIdentityProvider,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
pub use store::{Account, Login, Store};

/// How long a Server may take over each step of proving itself before a
/// Relay stops waiting, unless its configuration says otherwise.
const GREETING_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a Server may take to take in what a Relay says before the Relay
/// gives the connection up, unless its configuration says otherwise.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a waiting Server may take to take up a join asked of it before
/// the Relay gives the join up, unless its configuration says otherwise.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

/// Dependencies that log what they carry at their verbose levels, and the most
/// verbose level each is let log at whatever `RUST_LOG` asks: tungstenite logs
/// every frame and message whole at trace, a login's code among them.
const PAYLOAD_BEARING_TARGETS: [(&str, LevelFilter); 2] = [
    ("tungstenite", LevelFilter::WARN),
    ("tokio_tungstenite", LevelFilter::WARN),
];

/// How a Relay is run.
#[derive(Clone)]
pub struct RelayConfig {
    listen: SocketAddr,
    database: PathBuf,
    public_address: String,
    greeting_timeout: Duration,
    send_timeout: Duration,
    join_timeout: Duration,
    versions: Vec<Version>,
    clock: Clock,
}

impl RelayConfig {
    /// A Relay listening for plain HTTP at `listen`, keeping its records in
    /// the SQLite database at `database`, and known as `public_address`: the
    /// address Servers reach it at, which every proof made for it names. A
    /// Relay is known by its address, so it is told its own rather than
    /// taking it from what a connection claims.
    pub fn new(
        listen: SocketAddr,
        database: impl Into<PathBuf>,
        public_address: impl Into<String>,
    ) -> Self {
        Self {
            listen,
            database: database.into(),
            public_address: public_address.into(),
            greeting_timeout: GREETING_TIMEOUT,
            send_timeout: SEND_TIMEOUT,
            join_timeout: JOIN_TIMEOUT,
            versions: SPOKEN.to_vec(),
            clock: Clock::system(),
        }
    }

    /// Bounds how long a Server may take over each step of proving itself.
    pub fn with_greeting_timeout(mut self, timeout: Duration) -> Self {
        self.greeting_timeout = timeout;
        self
    }

    /// Bounds how long a Server may take to take in what the Relay says
    /// before the Relay gives the connection up.
    pub fn with_send_timeout(mut self, timeout: Duration) -> Self {
        self.send_timeout = timeout;
        self
    }

    /// Bounds how long a waiting Server may take to take up a join asked of
    /// it before the Relay gives the join up.
    pub fn with_join_timeout(mut self, timeout: Duration) -> Self {
        self.join_timeout = timeout;
        self
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
    /// Ends once nothing holds the Relay: its router gone, and every
    /// connection to it ended.
    released: mpsc::Receiver<()>,
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

    /// Stops the Relay, ending every Server's connection to it, and returns
    /// once the last of them has gone.
    pub async fn shutdown(mut self) -> Result<()> {
        self.stopping.send_replace(true);
        let served = self.task.await.context("the Relay's task panicked")?;
        while self.released.recv().await.is_some() {}
        served
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
    let public_address = canonical_address(&config.public_address).with_context(|| {
        format!(
            "the Relay's public address `{}` is not an https:// or http:// address naming a host",
            config.public_address
        )
    })?;
    let store = Store::open(&config.database)?;
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listen at {}", config.listen))?;
    let address = listener.local_addr().context("read the Relay's address")?;
    let (stopping, stopping_rx) = watch::channel(false);
    let (held, released) = mpsc::channel(1);
    let relay = Arc::new(connection::Relay {
        public_address,
        greeting_timeout: config.greeting_timeout,
        send_timeout: config.send_timeout,
        join_timeout: config.join_timeout,
        joiner: joiner::Joiner::new(),
        _held: held,
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
        released,
    })
}

/// How a Relay's own log is filtered: as `directives` — `RUST_LOG`'s — ask,
/// `info` where they ask nothing that can be read, and never letting
/// [`PAYLOAD_BEARING_TARGETS`] log more verbosely than they are allowed,
/// whatever the directives say.
pub fn log_filter(
    directives: Option<&str>,
) -> impl tracing_subscriber::layer::Filter<Registry> + use<> {
    let directives = directives
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    directives.and(
        Targets::new()
            .with_default(LevelFilter::TRACE)
            .with_targets(PAYLOAD_BEARING_TARGETS),
    )
}

/// The Relay's own log layer, writing to `writer` through [`log_filter`].
pub fn log_layer<W>(directives: Option<&str>, writer: W) -> impl Layer<Registry> + use<W>
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_filter(log_filter(directives))
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// However verbose `RUST_LOG` asks the log to be — naming the dependency
    /// outright — tungstenite, which logs every message whole, is held to its
    /// warnings, while the Relay's own lines are written as verbosely as asked.
    #[test]
    fn a_dependency_logging_what_it_carries_is_held_to_warnings_whatever_the_filter_asks() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = Registry::default().with(log_layer(
            Some("trace,tungstenite=trace,tokio_tungstenite=trace"),
            move || writer.clone(),
        ));
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "tungstenite::protocol", "Received message CODE-0001");
            tracing::debug!(target: "tokio_tungstenite", "frame CODE-0002");
            tracing::warn!(target: "tungstenite::protocol", "warning kept");
            tracing::trace!("the Relay's own trace line");
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        for payload in ["CODE-0001", "CODE-0002"] {
            assert!(!log.contains(payload), "{payload} reached the log: {log}");
        }
        assert!(log.contains("warning kept"), "{log}");
        assert!(log.contains("the Relay's own trace line"), "{log}");
    }
}
