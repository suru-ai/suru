//! A Relay: a server through which Servers that cannot reach each other
//! directly carry a Pairing (ADR-0045).
//!
//! Every Server connects outward to its Relay over a WebSocket and proves
//! itself by its identity key. The Relay admits by login: a Server's user logs
//! in through an identity provider only the Relay speaks to, and the Relay
//! ties the Account that identity answers to to the Server's key as a Login,
//! which stands until it is removed (ADR-0046, ADR-0048) — though it is
//! refused while its Account has lapsed, no longer satisfying the operator's
//! admission rules, until one fresh login from any of its Servers restores
//! it. A Server that Serves through the Relay waits there to be reached, and
//! the Relay joins it to another Server under the same Account that asks for
//! it, carrying the bytes between them unread. Its operator caps how many
//! Logins may stand under each Account and how many connections it joins for
//! each at once, so no one Account can exhaust it. The Relay holds no
//! Provider or Session of Suru's, and none of the trust a Pairing holds. What
//! it can tell is who connected what to what, which it writes to its
//! connection log, one line for each connection it joins. Its operator lists
//! and removes its Accounts and Logins from its command line ([`operate`]), a
//! process apart that reaches a running Relay through its records alone.

use std::{
    io::Write,
    net::SocketAddr,
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

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

mod admission;
mod caps;
mod clock;
mod connection;
mod connection_log;
mod forwarded;
mod github;
mod identity;
mod joiner;
mod operator;
mod running;
mod standing;
mod store;

pub use admission::{Admission, AdmissionRule, Undecided};
pub use caps::{JOINED_CONNECTIONS_PER_ACCOUNT, LOGINS_PER_ACCOUNT};
pub use clock::Clock;
pub use forwarded::{TrustedProxy, UnrecognizedProxy};
pub use github::{GitHub, GitHubApp, GitHubAppKey};
pub use identity::{
    DeviceLogin, Identity, IdentityProvider, LoginRefusal, LookUpFailed, NoIdentityProvider,
    Organization, OrganizationUnchecked, SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
pub use joiner::{JOINS_ASKED_PER_SERVER, WAITING_CONNECTIONS_PER_SERVER};
pub use operator::{
    AccountsCommand, Confirmation, Listing, LoginsCommand, OperatorCommand, Outcome,
    REMOVALS_CUT_AT_ONCE, operate,
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

/// How long a Relay lets a connection go with nothing sent on it before it
/// pings, unless its configuration says otherwise: well within the minute a
/// reverse proxy commonly lets a connection stand idle before it closes it.
pub const KEEPALIVE: Duration = Duration::from_secs(20);

/// How many lines the connection log may owe at once, unless the Relay's
/// configuration says otherwise: one for each joined connection the Relay
/// carries, and one for each ended whose line its reader has yet to take in.
const CONNECTION_LOG_CAPACITY: usize = 65_536;

/// How long a stopping Relay waits for its connection log to write the lines
/// it owes, and then for its diagnostic log to say how many it gave up,
/// unless its configuration says otherwise.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a running Relay looks for Logins its operator has removed, to
/// cut what stands on them, unless its configuration says otherwise: soon
/// enough that the operator's command line, waiting for the cut, returns
/// with no wait to speak of, and seldom enough to cost nothing, each look
/// asking only whether a table that is empty but for removals not yet cut is
/// empty, and taking nothing a Server's connection waits on unless it is not.
const REMOVAL_INTERVAL: Duration = Duration::from_millis(100);

/// How often a Relay checks its Accounts against its admission rules again,
/// unless its configuration says otherwise: often enough that someone removed
/// from an organization the rules name is cut off within a quarter of an
/// hour, and seldom enough that checking a few thousand Accounts against it,
/// about one request of GitHub's each, stays within the 5,000 to 12,500 an
/// hour GitHub allows an app's installation on an organization, by its size.
const ADMISSION_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long a Relay's admission rules may take to answer each asking before
/// the Relay takes them to be unable to tell, unless its configuration says
/// otherwise.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Dependencies that log what they carry at their verbose levels, and the most
/// verbose level each is let log at whatever `RUST_LOG` asks: tungstenite logs
/// every frame and message whole at trace, a login's code among them; and
/// reqwest, hyper and hyper-util carry the Relay's requests to its identity
/// provider, a login's device code and a user's token among them, and say
/// what they can of each at debug and trace.
const PAYLOAD_BEARING_TARGETS: [(&str, LevelFilter); 5] = [
    ("tungstenite", LevelFilter::WARN),
    ("tokio_tungstenite", LevelFilter::WARN),
    ("reqwest", LevelFilter::WARN),
    ("hyper", LevelFilter::WARN),
    ("hyper_util", LevelFilter::WARN),
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
    keepalive: Duration,
    versions: Vec<Version>,
    clock: Clock,
    trusted_proxies: Vec<TrustedProxy>,
    connection_log: connection_log::Writer,
    connection_log_capacity: usize,
    drain_timeout: Duration,
    removal_interval: Duration,
    admission: Admission,
    admission_interval: Duration,
    admission_timeout: Duration,
    fresh_login_every: Option<Duration>,
    logins_per_account: NonZeroU32,
    joined_connections_per_account: NonZeroU32,
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
            keepalive: KEEPALIVE,
            versions: SPOKEN.to_vec(),
            clock: Clock::system(),
            trusted_proxies: Vec::new(),
            connection_log: Arc::new(Mutex::new(std::io::stdout())),
            connection_log_capacity: CONNECTION_LOG_CAPACITY,
            drain_timeout: DRAIN_TIMEOUT,
            removal_interval: REMOVAL_INTERVAL,
            admission: Admission::nobody(),
            admission_interval: ADMISSION_INTERVAL,
            admission_timeout: ADMISSION_TIMEOUT,
            fresh_login_every: None,
            logins_per_account: LOGINS_PER_ACCOUNT,
            joined_connections_per_account: JOINED_CONNECTIONS_PER_ACCOUNT,
        }
    }

    /// Admits whoever `admission`'s rules admit, looking up the users and
    /// the organizations they name as it starts. Until it is given rules, a
    /// Relay admits nobody.
    pub fn with_admission(mut self, admission: Admission) -> Self {
        self.admission = admission;
        self
    }

    /// Has the Relay check every Account a Login stands under against its
    /// admission rules as it starts, and again each `interval`, from the end
    /// of one pass of them to the beginning of the next. It is the gap
    /// between passes, not the longest an Account the rules stop admitting
    /// goes on standing: a pass asks about its Accounts one after another,
    /// each for no longer than the admission timeout, so while the rules
    /// answer nothing a pass over N Accounts takes N such timeouts. Each pass
    /// begins at the first Account the one before could not tell about, so
    /// rules that answer for only so many Accounts at a time — an identity
    /// provider limiting how often it is asked — come to each in turn,
    /// whatever other rules decide of the Accounts after it. A login the
    /// rules refuse lapses its Account at once, whatever the schedule.
    pub fn with_admission_interval(mut self, interval: Duration) -> Self {
        self.admission_interval = interval;
        self
    }

    /// Bounds how long the admission rules may take to answer each asking
    /// before the Relay takes them to be unable to tell.
    pub fn with_admission_timeout(mut self, timeout: Duration) -> Self {
        self.admission_timeout = timeout;
        self
    }

    /// Requires an Account to have been logged in as, from any one of its
    /// Servers, within each `every`: an Account not logged in as for longer
    /// lapses until one of its Servers logs in afresh. Unless asked for, a
    /// Login stands however long ago it was formed.
    pub fn with_fresh_login_every(mut self, every: Duration) -> Self {
        self.fresh_login_every = Some(every);
        self
    }

    /// Caps how many Logins may stand under each Account — how many of its
    /// Servers may be logged in at the Relay — at `cap` rather than
    /// [`LOGINS_PER_ACCOUNT`]. A lapsed Account's Logins count against it
    /// still, since nothing of a lapsed Account is forgotten.
    pub fn with_logins_per_account(mut self, cap: NonZeroU32) -> Self {
        self.logins_per_account = cap;
        self
    }

    /// Caps how many connections the Relay joins for each Account at once at
    /// `cap` rather than [`JOINED_CONNECTIONS_PER_ACCOUNT`].
    pub fn with_joined_connections_per_account(mut self, cap: NonZeroU32) -> Self {
        self.joined_connections_per_account = cap;
        self
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

    /// Has the Relay ping each connection it has sent nothing on for
    /// `interval` rather than [`KEEPALIVE`] — a waiting Server's, one whose
    /// login is under way, and either side of a join carrying nothing — so a
    /// reverse proxy that closes connections idle for longer keeps them.
    /// Pinging holds nothing beyond what a connection already bounds: a
    /// Server that does not take a ping in within the send timeout is let
    /// go, as for anything else the Relay sends it.
    pub fn with_keepalive(mut self, interval: Duration) -> Self {
        self.keepalive = interval;
        self
    }

    /// Has the Relay speak `versions` of its protocol rather than this
    /// build's own, so a test can stand it beside a Server it disagrees with.
    pub fn with_protocol_versions(mut self, versions: Vec<Version>) -> Self {
        self.versions = versions;
        self
    }

    /// Has the Relay read the time it stamps its records, and its
    /// connection log, with from `clock`.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Has the Relay believe `proxies` — and no others — about the address
    /// a Server's connection comes from, which they name in their
    /// `X-Forwarded-For` header. Unless they are named, the Relay believes
    /// no proxy, and logs the address that connected to it.
    pub fn with_trusted_proxies(mut self, proxies: impl IntoIterator<Item = TrustedProxy>) -> Self {
        self.trusted_proxies = proxies.into_iter().collect();
        self
    }

    /// Has the Relay write its connection log to `writer` rather than to
    /// standard output.
    pub fn with_connection_log(mut self, writer: impl Write + Send + 'static) -> Self {
        self.connection_log = Arc::new(Mutex::new(writer));
        self
    }

    /// Bounds how many lines the connection log may owe at once — one for
    /// each joined connection the Relay carries, and one for each ended whose
    /// line is yet to be written. Past it, the Relay refuses joins until the
    /// log catches up, rather than carry a connection it could not record.
    pub fn with_connection_log_capacity(mut self, lines: usize) -> Self {
        self.connection_log_capacity = lines;
        self
    }

    /// Bounds how long a stopping Relay waits for its connection log to
    /// write the lines it owes, and then as long again for its diagnostic
    /// log to take in how many it gave up.
    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Has the running Relay look for Logins its operator has removed from
    /// its records every `interval`, cutting what stands on them and
    /// confirming the cut to the operator's command line, which waits for it.
    /// A Login removed is refused from the moment it is removed, however long
    /// until the Relay next looks; it is only what already stood on it that
    /// waits to be cut.
    pub fn with_removal_interval(mut self, interval: Duration) -> Self {
        self.removal_interval = interval;
        self
    }
}

/// A Relay serving Servers until it is shut down.
pub struct RunningRelay {
    address: SocketAddr,
    store: Store,
    /// What every connection to the Relay shares, while anything holds it.
    relay: Weak<connection::Relay>,
    /// The lock beside the Relay's records that says it runs on them, held
    /// until it has stopped.
    _running: std::fs::File,
    /// The connection log's writer, until it has written what it owes.
    writing: connection_log::Writing,
    drain_timeout: Duration,
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

    /// Looks at once for Logins the Relay's operator has removed, cutting
    /// what stands on them and confirming the cut, as the Relay does on its
    /// own every removal interval.
    pub async fn look_for_removals(&self) -> Result<()> {
        match self.relay.upgrade() {
            Some(relay) => operator::cut_removed(&relay).await,
            None => Ok(()),
        }
    }

    /// Stops the Relay, ending every Server's connection to it, and returns
    /// once the last of them has gone and the connection log has written
    /// the lines they were owed — or has taken longer than the Relay waits.
    pub async fn shutdown(mut self) -> Result<()> {
        self.stopping.send_replace(true);
        let served = self.task.await.context("the Relay's task panicked")?;
        while self.released.recv().await.is_some() {}
        self.writing.finish(self.drain_timeout).await;
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

/// Starts a Relay that logs Servers' users in through `provider`, once it
/// has checked every Account against its admission rules: refusing to start
/// where it cannot look up a user its rules name there, cannot check the
/// members of an organization they name there, or cannot record what they
/// call for.
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
    // One Relay runs on its records at a time, holding the lock beside them
    // from before it carries them forward until it has stopped.
    let running = running::run_on(&config.database).await?;
    let store = Store::open(&config.database)?;
    let admission = config
        .admission
        .looked_up(&store, &provider, config.clock.now())
        .await?;
    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listen at {}", config.listen))?;
    let address = listener.local_addr().context("read the Relay's address")?;
    let (connection_log, writing) = connection_log::ConnectionLog::start(
        config.connection_log,
        config.connection_log_capacity,
        config.clock.clone(),
    )?;
    let (stopping, stopping_rx) = watch::channel(false);
    let (held, released) = mpsc::channel(1);
    let checks = admission::Checks::default();
    let relay = Arc::new(connection::Relay {
        public_address,
        greeting_timeout: config.greeting_timeout,
        send_timeout: config.send_timeout,
        join_timeout: config.join_timeout,
        keepalive: config.keepalive,
        joiner: joiner::Joiner::new(),
        standing: tokio::sync::Mutex::new(checks.verdicts()),
        holdings: standing::Holdings::new(),
        _held: held,
        store: store.clone(),
        provider,
        admission,
        checks,
        admission_interval: config.admission_interval,
        resume_at: std::sync::atomic::AtomicI64::new(0),
        admission_timeout: config.admission_timeout,
        fresh_login_every: config.fresh_login_every,
        logins_per_account: config.logins_per_account,
        joined: caps::Joined::new(config.joined_connections_per_account),
        versions: config.versions,
        trusted_proxies: config.trusted_proxies,
        connection_log,
        clock: config.clock,
        stopping: stopping_rx.clone(),
    });
    // Before it serves anyone, the Relay applies its rules as they now stand
    // to every Account, so none they no longer admit — a name removed from
    // them, say — is served even once; and refuses to start where it cannot
    // record what they call for. Each Account is asked about for no longer
    // than the admission timeout, and one the rules cannot tell about stands.
    admission::check_every_account(&relay)
        .await
        .context("check every Account against the admission rules before serving")?;
    // Logins its operator removed while it was not running are gone from its
    // records already, with nothing standing on them to cut.
    operator::cut_removed(&relay)
        .await
        .context("look for Logins the operator removed before serving")?;
    // From then on it checks them on its own until it stops, letting go of
    // any asking of the rules under way.
    tokio::spawn({
        let relay = relay.clone();
        let mut stopping = stopping_rx.clone();
        async move {
            tokio::select! {
                _ = stopping.wait_for(|stopping| *stopping) => {}
                () = admission::keep_checking(&relay) => {}
            }
        }
    });
    // And it cuts what stands on each Login its operator removes, as it
    // finds them, until it stops.
    tokio::spawn({
        let relay = relay.clone();
        let mut stopping = stopping_rx.clone();
        let interval = config.removal_interval;
        async move {
            tokio::select! {
                _ = stopping.wait_for(|stopping| *stopping) => {}
                () = operator::keep_cutting_removed(&relay, interval) => {}
            }
        }
    });
    let shared = Arc::downgrade(&relay);
    let app = Router::new()
        .route(ENDPOINT_PATH, get(connection::connect))
        .with_state(relay)
        .into_make_service_with_connect_info::<SocketAddr>();
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
        relay: shared,
        _running: running,
        writing,
        drain_timeout: config.drain_timeout,
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
    /// outright — tungstenite, which logs every message whole, and the HTTP
    /// client that carries the Relay's requests to GitHub are held to their
    /// warnings, while the Relay's own lines are written as verbosely as asked.
    #[test]
    fn a_dependency_logging_what_it_carries_is_held_to_warnings_whatever_the_filter_asks() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = Registry::default().with(log_layer(
            Some(
                "trace,tungstenite=trace,tokio_tungstenite=trace,reqwest=trace,hyper=trace,\
                 hyper_util=trace",
            ),
            move || writer.clone(),
        ));
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "tungstenite::protocol", "Received message CODE-0001");
            tracing::debug!(target: "tokio_tungstenite", "frame CODE-0002");
            tracing::trace!(target: "reqwest::connect", "sending device_code=DEVICE-0003");
            tracing::trace!(target: "hyper::proto::h1", "authorization: Bearer TOKEN-0004");
            tracing::debug!(target: "hyper_util::client", "connecting to TOKEN-0005");
            tracing::warn!(target: "tungstenite::protocol", "warning kept");
            tracing::trace!("the Relay's own trace line");
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        for payload in [
            "CODE-0001",
            "CODE-0002",
            "DEVICE-0003",
            "TOKEN-0004",
            "TOKEN-0005",
        ] {
            assert!(!log.contains(payload), "{payload} reached the log: {log}");
        }
        assert!(log.contains("warning kept"), "{log}");
        assert!(log.contains("the Relay's own trace line"), "{log}");
    }
}
