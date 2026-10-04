//! A Server's Relays, driven through the Server's own API against a real
//! Relay in-process whose identity provider the test scripts.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use futures_util::{SinkExt, StreamExt};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Relay, RelayAccount, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelaySide,
        RelayState, SessionError, SessionErrorCode,
    },
    server::{self, ServerConfig, ServerTimings},
};
use suru_relay::{
    Admission, AdmissionRule, Clock, Identity, RelayConfig, RunningRelay,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
use suru_relay_protocol::{Bytes, RelayMessage, SPOKEN, ServerMessage, Version};
use tokio::{sync::Notify, time::timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::support::{
    PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, receive_initial_state,
};

#[path = "relays/pairing.rs"]
mod pairing;
#[path = "relays/serving.rs"]
mod serving;

/// How often a test's Relay checks its Accounts against its admission rules
/// again.
const ADMISSION_INTERVAL: Duration = Duration::from_millis(10);

/// A real Relay, reached through a route a test can take offline and point
/// at the Relay again once it restarts elsewhere, keeping its records, its
/// scripted identity provider, its configuration, and its public address —
/// the route's — across restarts. Its scripted provider admits whoever it
/// logs in, until the test says otherwise.
struct TestRelay {
    directory: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    /// How far past the operating system's time the Relay's clock reads.
    clock_ahead: Arc<AtomicU64>,
    /// What the test configures the Relay with beyond the usual.
    configure: fn(RelayConfig) -> RelayConfig,
    /// What the Relay writes to its connection log, across restarts.
    connection_log: ConnectionLog,
    running: Option<RunningRelay>,
    route: ObservedTcpProxy,
}

/// A Relay's connection log, as a test reads it.
#[derive(Clone, Default)]
struct ConnectionLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for ConnectionLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl TestRelay {
    async fn start() -> Self {
        Self::configured(|config| config).await
    }

    async fn speaking(versions: Vec<Version>) -> Self {
        Self::starting(versions, |config| config).await
    }

    /// A Relay configured further as `configure` says.
    async fn configured(configure: fn(RelayConfig) -> RelayConfig) -> Self {
        Self::starting(SPOKEN.to_vec(), configure).await
    }

    async fn starting(versions: Vec<Version>, configure: fn(RelayConfig) -> RelayConfig) -> Self {
        let directory = tempfile::tempdir().expect("create the Relay's directory");
        let provider = Arc::new(ScriptedProvider::new());
        let clock_ahead = Arc::new(AtomicU64::new(0));
        let connection_log = ConnectionLog::default();
        // The route comes first, since the Relay is known by the address
        // Servers reach it at, and is pointed at the Relay once it runs.
        let route = ObservedTcpProxy::start((std::net::Ipv4Addr::LOCALHOST, 9).into()).await;
        let running = run_relay(
            &directory,
            &provider,
            &clock_ahead,
            &connection_log,
            &format!("http://{}", route.address),
            versions,
            configure,
        )
        .await;
        route.retarget(running.address());
        Self {
            directory,
            provider,
            clock_ahead,
            configure,
            connection_log,
            running: Some(running),
            route,
        }
    }

    /// Where a Server reaches this Relay.
    fn address(&self) -> String {
        format!("http://{}", self.route.address)
    }

    fn running(&self) -> &RunningRelay {
        self.running.as_ref().expect("the Relay is running")
    }

    async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.shutdown().await.expect("stop the Relay");
        }
    }

    /// Starts the Relay again on its records, speaking `versions`, at an
    /// address of its own that the route then carries Servers to.
    async fn restart_speaking(&mut self, versions: Vec<Version>) {
        self.stop().await;
        let running = run_relay(
            &self.directory,
            &self.provider,
            &self.clock_ahead,
            &self.connection_log,
            &self.address(),
            versions,
            self.configure,
        )
        .await;
        self.route.retarget(running.address());
        self.running = Some(running);
    }

    async fn restart(&mut self) {
        self.restart_speaking(SPOKEN.to_vec()).await;
    }

    /// How many joined connections the Relay has logged: one for each, once
    /// it has ended.
    fn joined_connections_logged(&self) -> usize {
        let log = self.connection_log.0.lock().unwrap();
        log.split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count()
    }

    /// Waits until the Relay has logged `expected` joined connections.
    async fn wait_for_joined_connections_logged(&self, expected: usize) {
        let logging = timeout(PROGRESS_DEADLINE, async {
            while self.joined_connections_logged() < expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        logging.await.unwrap_or_else(|_| {
            panic!(
                "the Relay logged {} joined connections, never {expected}",
                self.joined_connections_logged()
            )
        });
    }

    fn advance_clock(&self, by: Duration) {
        self.clock_ahead.fetch_add(by.as_secs(), Ordering::AcqRel);
    }

    /// Logs in the login whose code is `user_code` as the identity
    /// `subject`, named `username`, as its user would at the provider.
    fn approve(&self, user_code: &str, subject: &str, username: &str) {
        assert!(
            self.provider.approve(
                user_code,
                Identity {
                    subject: subject.to_owned(),
                    username: username.to_owned(),
                },
            ),
            "a login with code {user_code} is waiting"
        );
    }
}

async fn run_relay(
    directory: &tempfile::TempDir,
    provider: &Arc<ScriptedProvider>,
    clock_ahead: &Arc<AtomicU64>,
    connection_log: &ConnectionLog,
    public_address: &str,
    versions: Vec<Version>,
    configure: fn(RelayConfig) -> RelayConfig,
) -> RunningRelay {
    let clock_ahead = clock_ahead.clone();
    suru_relay::start(
        configure(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                directory.path().join("relay.db"),
                public_address,
            )
            .with_protocol_versions(versions)
            .with_connection_log(connection_log.clone())
            .with_clock(Clock::from_fn(move || {
                SystemTime::now() + Duration::from_secs(clock_ahead.load(Ordering::Acquire))
            }))
            .with_admission(Admission::by([provider.clone() as Arc<dyn AdmissionRule>]))
            .with_admission_interval(ADMISSION_INTERVAL),
        ),
        provider.clone(),
    )
    .await
    .expect("start the Relay")
}

/// A Server and a Client attached to it, which a test may restart on the
/// same records.
struct TestServer {
    state: tempfile::TempDir,
    /// Where the Server pins the Settings its Client changes.
    _config_root: tempfile::TempDir,
    config: ServerConfig,
    timings: ServerTimings,
    channel: String,
    server: Option<server::RunningServer>,
    client: ManagedClient,
}

fn relay_timings() -> ServerTimings {
    ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    }
    .with_relay_retry_backoff(Duration::from_millis(5), Duration::from_millis(25))
}

impl TestServer {
    async fn start(channel: &str) -> Self {
        Self::with_timings(channel, relay_timings()).await
    }

    async fn with_timings(channel: &str, timings: ServerTimings) -> Self {
        let state = tempfile::tempdir().expect("create the Server's state directory");
        let config_root = tempfile::tempdir().expect("create the Server's config root");
        let config = ServerConfig::new(state.path(), channel)
            .expect("configure the Server")
            .with_config_dir(config_root.path());
        let (server, client) = run_server(&config, &timings, state.path(), channel).await;
        Self {
            state,
            _config_root: config_root,
            config,
            timings,
            channel: channel.to_owned(),
            server: Some(server),
            client,
        }
    }

    async fn restart(&mut self) {
        self.server
            .take()
            .expect("the Server is running")
            .shutdown()
            .await
            .expect("stop the Server");
        let (server, client) = run_server(
            &self.config,
            &self.timings,
            self.state.path(),
            &self.channel,
        )
        .await;
        self.server = Some(server);
        self.client = client;
    }

    async fn shutdown(mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().await.expect("stop the Server");
        }
    }

    async fn relay(&self, address: &str) -> Option<Relay> {
        self.client
            .list_relays()
            .await
            .expect("list the Server's Relays")
            .into_iter()
            .find(|relay| relay.address == address)
    }

    /// Waits until the Relay at `address` stands as `reached` says.
    async fn wait_for_relay(&self, address: &str, reached: impl Fn(&Relay) -> bool) -> Relay {
        let reaching = timeout(PROGRESS_DEADLINE, async {
            loop {
                if let Some(relay) = self.relay(address).await
                    && reached(&relay)
                {
                    return relay;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        match reaching {
            Ok(relay) => relay,
            Err(_) => panic!(
                "the Relay at {address} never came to stand as expected; it stands as {:?}",
                self.relay(address).await
            ),
        }
    }

    async fn wait_for_state(&self, address: &str, state: RelayState) -> Relay {
        self.wait_for_relay(address, |relay| relay.state == state)
            .await
    }

    /// Adds `relay` and logs in there as the identity `subject`, named
    /// `username`.
    async fn log_in(&self, relay: &TestRelay, subject: &str, username: &str) -> RelayLogin {
        let address = relay.address();
        if self.relay(&address).await.is_none() {
            self.client
                .add_relay(address.clone())
                .await
                .expect("add the Relay");
        }
        let login = self
            .client
            .begin_relay_login(&address)
            .await
            .expect("begin a login at the Relay");
        relay.approve(&login.user_code, subject, username);
        self.client
            .follow_relay_login(&address)
            .await
            .expect("follow the login to its end")
    }

    /// The fingerprint of this Server's identity key.
    fn fingerprint(&self) -> String {
        let key = std::fs::read(self.config.data_dir().join("server-identity.pk8"))
            .expect("read the Server's identity key");
        let key = rcgen::KeyPair::try_from(key.as_slice()).expect("decode the identity key");
        suru_relay_protocol::fingerprint(&rcgen::PublicKeyData::subject_public_key_info(&key))
    }
}

async fn run_server(
    config: &ServerConfig,
    timings: &ServerTimings,
    state: &std::path::Path,
    channel: &str,
) -> (server::RunningServer, ManagedClient) {
    let server = server::spawn_with_provider_and_timings(
        config.clone(),
        Arc::new(crate::failing_provider_support::FailingProviderRuntime),
        timings.clone(),
    )
    .await
    .expect("spawn the Server");
    // A Remote the Client looks at through the Server is tried again at the
    // pace of a test.
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state, channel)
            .expect("configure the Client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(25)),
    )
    .await
    .expect("attach the Client");
    receive_initial_state(&mut client).await;
    (server, client)
}

/// The name the Server reports itself by: its machine's hostname.
fn machine_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "remote".to_owned())
}

fn error_code(error: &anyhow::Error) -> SessionErrorCode {
    error
        .downcast_ref::<SessionError>()
        .unwrap_or_else(|| panic!("a typed Session error, not {error:#}"))
        .code
}

fn account(username: &str) -> RelayAccount {
    RelayAccount {
        provider: "scripted".to_owned(),
        username: username.to_owned(),
    }
}

#[tokio::test]
async fn a_server_logs_in_to_a_relay_through_its_own_api_and_the_relay_records_the_login() {
    let relay = TestRelay::start().await;
    let server = TestServer::start("relay-login").await;
    let address = relay.address();

    let added = server
        .client
        .add_relay(address.clone())
        .await
        .expect("add a Relay by its address");
    assert_eq!(added.address, address);
    assert_eq!(added.state, RelayState::LoginNeeded);
    assert_eq!(added.account, None);
    assert_eq!(server.client.list_relays().await.unwrap(), vec![added]);

    let login = server
        .client
        .begin_relay_login(&address)
        .await
        .expect("begin a login at the Relay");
    assert_eq!(login.verification_uri, SCRIPTED_VERIFICATION_URI);
    assert!(!login.user_code.is_empty());
    assert_eq!(login.outcome, RelayLoginOutcome::Pending);
    assert_eq!(
        server.relay(&address).await.unwrap().login,
        Some(login.clone()),
        "where the login stands is listed with the Relay"
    );
    assert!(
        relay.running().store().logins().await.unwrap().is_empty(),
        "nothing is recorded before the user logs in"
    );

    relay.approve(&login.user_code, "583231", "octocat");
    let done = server
        .client
        .follow_relay_login(&address)
        .await
        .expect("follow the login to its end");
    assert_eq!(
        done.outcome,
        RelayLoginOutcome::Done {
            account: account("octocat"),
        }
    );
    let listed = server.relay(&address).await.unwrap();
    assert_eq!(listed.state, RelayState::LoggedIn);
    assert_eq!(listed.account, Some(account("octocat")));
    assert_eq!(listed.login, Some(done));

    let accounts = relay.running().store().accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        (accounts[0].provider.as_str(), accounts[0].subject.as_str()),
        ("scripted", "583231"),
        "the Account is keyed by the provider and its stable subject id"
    );
    let logins = relay.running().store().logins().await.unwrap();
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0].account, accounts[0].id);
    assert_eq!(
        logins[0].fingerprint,
        server.fingerprint(),
        "the Login is tied to the Server's own identity key"
    );
    assert_eq!(
        logins[0].hostname,
        machine_hostname(),
        "the Server reports its hostname, which labels the Login"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_login_the_identity_provider_refuses_forms_no_login_and_says_why() {
    let relay = TestRelay::start().await;
    let server = TestServer::start("relay-login-refused").await;
    let address = relay.address();
    server.client.add_relay(address.clone()).await.unwrap();

    let login = server.client.begin_relay_login(&address).await.unwrap();
    assert!(relay.provider.deny(&login.user_code));
    let refused = server.client.follow_relay_login(&address).await.unwrap();
    let RelayLoginOutcome::Refused { reason, message } = refused.outcome else {
        panic!("a denied login is refused, not {:?}", refused.outcome);
    };
    assert_eq!(reason, RelayLoginRefusal::Denied);
    assert!(!message.is_empty());
    assert_eq!(
        server.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );
    assert!(relay.running().store().logins().await.unwrap().is_empty());
    assert!(relay.running().store().accounts().await.unwrap().is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn a_login_the_relay_does_not_admit_is_told_so_and_forms_no_login() {
    let relay = TestRelay::configured(|config| config.with_admission(Admission::nobody())).await;
    let server = TestServer::start("relay-login-not-admitted").await;
    let address = relay.address();

    let login = server.log_in(&relay, "583231", "octocat").await;
    let RelayLoginOutcome::Refused { reason, message } = login.outcome else {
        panic!(
            "a Relay with no rules admits nobody, not {:?}",
            login.outcome
        );
    };
    assert_eq!(
        reason,
        RelayLoginRefusal::NotAdmitted,
        "the refusal says the user is not admitted, which only the Relay's operator can change"
    );
    assert!(message.contains("octocat"), "{message}");
    let listed = server.relay(&address).await.unwrap();
    assert_eq!(
        (listed.state, listed.account),
        (RelayState::LoginNeeded, None)
    );
    assert!(relay.running().store().accounts().await.unwrap().is_empty());
    assert!(relay.running().store().logins().await.unwrap().is_empty());

    server.shutdown().await;
}

/// The Relay refuses the Login of a Server whose Account has lapsed the
/// moment it lapses, so its entry reads login needed — which only a login
/// can change — at once, rather than at the next of its tries, and never
/// Unreachable.
#[tokio::test]
async fn a_relay_whose_account_lapses_reads_login_needed_at_once_rather_than_unreachable() {
    let relay = TestRelay::start().await;
    let mut server = TestServer::with_timings(
        "relay-lapsed-login-needed",
        relay_timings()
            .with_relay_retry_backoff(Duration::from_secs(600), Duration::from_secs(600)),
    )
    .await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    // Restarted, the Server learns its Account anew only as the connection
    // it keeps to the Relay is proven there.
    server.restart().await;
    server
        .wait_for_relay(&address, |relay| relay.account.is_some())
        .await;

    relay.provider.set_admitted("583231", false);
    let lapsed = server
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;
    assert_eq!((lapsed.unreachable, lapsed.account), (None, None));
    let accounts = relay.running().store().accounts().await.unwrap();
    assert!(accounts[0].lapsed);
    assert_eq!(
        relay.running().store().logins().await.unwrap().len(),
        1,
        "the Relay forgets nothing of the Login it refuses"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_fresh_login_a_relay_requires_every_so_many_days_is_met_from_any_one_server() {
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let relay = TestRelay::configured(|config| config.with_fresh_login_every(7 * DAY)).await;
    let workstation = TestServer::start("relay-fresh-login-workstation").await;
    let laptop = TestServer::start("relay-fresh-login-laptop").await;
    let address = relay.address();
    for server in [&workstation, &laptop] {
        server.log_in(&relay, "583231", "octocat").await;
    }
    let workstation_login = workstation.relay(&address).await.unwrap().login;

    relay.advance_clock(8 * DAY);
    for server in [&workstation, &laptop] {
        server
            .wait_for_state(&address, RelayState::LoginNeeded)
            .await;
    }
    let login = laptop.log_in(&relay, "583231", "octocat").await;
    assert_eq!(
        login.outcome,
        RelayLoginOutcome::Done {
            account: account("octocat"),
        }
    );
    let recovered = workstation
        .wait_for_state(&address, RelayState::LoggedIn)
        .await;
    assert_eq!(
        (recovered.account, recovered.login),
        (Some(account("octocat")), workstation_login),
        "the workstation recovers on its own, with nobody logging in there"
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_relay_that_stops_answering_reads_unreachable_until_it_answers_again() {
    let mut relay = TestRelay::start().await;
    let server = TestServer::start("relay-unreachable").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    relay.route.wait_for_connections(1).await;

    relay.stop().await;
    let unreachable = server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    assert_eq!(
        unreachable.account,
        Some(account("octocat")),
        "the Login stands while its Relay does not answer"
    );
    let tried = relay.route.opened_connections();
    relay.route.wait_for_opened_connections(tried + 3).await;

    relay.restart().await;
    server.wait_for_state(&address, RelayState::LoggedIn).await;
    relay.route.wait_for_connections(1).await;
    assert_eq!(
        relay.running().store().logins().await.unwrap().len(),
        1,
        "the restarted Relay kept the Login in its records"
    );

    relay.route.set_online(false).await;
    server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    relay.route.set_online(true).await;
    server.wait_for_state(&address, RelayState::LoggedIn).await;

    server.shutdown().await;
}

#[tokio::test]
async fn a_logged_in_server_connects_again_on_its_own_after_it_restarts() {
    let mut relay = TestRelay::start().await;
    let mut server = TestServer::start("relay-server-restart").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    relay.route.wait_for_connections(1).await;
    let opened = relay.route.opened_connections();

    server.restart().await;
    relay.route.wait_for_opened_connections(opened + 1).await;
    let reconnected = server
        .wait_for_relay(&address, |relay| relay.account.is_some())
        .await;
    assert_eq!(reconnected.state, RelayState::LoggedIn);
    assert_eq!(
        reconnected.account,
        Some(account("octocat")),
        "the Relay named the Account again once the restarted Server proved its key"
    );
    assert_eq!(
        reconnected.login, None,
        "no login was begun since it restarted"
    );
    relay.route.wait_for_connections(1).await;

    // Restarted while the Relay is away, it reads Unreachable, and connects
    // once the Relay answers.
    relay.stop().await;
    server.restart().await;
    server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    relay.restart().await;
    server.wait_for_state(&address, RelayState::LoggedIn).await;

    server.shutdown().await;
}

#[tokio::test]
async fn a_login_stands_however_much_time_passes() {
    let mut relay = TestRelay::start().await;
    let server = TestServer::start("relay-login-stands").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    relay.route.wait_for_connections(1).await;

    relay.advance_clock(Duration::from_secs(100 * 365 * 24 * 60 * 60));
    relay.route.set_online(false).await;
    server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    relay.route.set_online(true).await;
    let relay_now = server.wait_for_state(&address, RelayState::LoggedIn).await;
    assert_eq!(relay_now.account, Some(account("octocat")));

    server.shutdown().await;
}

#[tokio::test]
async fn removing_a_relay_asks_it_to_forget_the_login_and_forgets_the_entry_either_way() {
    let mut relay = TestRelay::start().await;
    let server = TestServer::start("relay-removal").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;

    let removal = server
        .client
        .remove_relay(&address)
        .await
        .expect("remove the Relay");
    assert_eq!(removal.address, address);
    assert!(
        removal.acknowledged,
        "the Relay answered and forgot the Login"
    );
    assert!(server.client.list_relays().await.unwrap().is_empty());
    assert!(relay.running().store().logins().await.unwrap().is_empty());
    relay.route.wait_for_connections(0).await;

    server.log_in(&relay, "583231", "octocat").await;
    relay.stop().await;
    let removal = server
        .client
        .remove_relay(&address)
        .await
        .expect("remove a Relay that does not answer");
    assert!(!removal.acknowledged);
    assert!(
        server.client.list_relays().await.unwrap().is_empty(),
        "the entry goes whether or not the Relay answers"
    );
    relay.restart().await;
    assert_eq!(
        relay.running().store().logins().await.unwrap().len(),
        1,
        "a Relay that never heard the Server keeps its Login"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn removing_a_relay_that_takes_connections_and_says_nothing_waits_only_its_answer_budget() {
    let server = TestServer::with_timings(
        "relay-removal-silent",
        relay_timings().with_relay_answer_timeout(Duration::from_millis(100)),
    )
    .await;
    let unanswered = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let mut silent = ObservedTcpProxy::start(unanswered.local_addr().unwrap()).await;
    silent.swallow_connections().await;
    let address = format!("http://{}", silent.address);
    server.client.add_relay(address.clone()).await.unwrap();

    let removal = server
        .client
        .remove_relay(&address)
        .await
        .expect("remove a Relay that never answers");
    assert!(!removal.acknowledged);
    assert!(server.client.list_relays().await.unwrap().is_empty());
    assert!(
        silent.opened_connections() > 0,
        "the Server asked the Relay"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn each_server_holds_its_own_logins_and_one_may_hold_logins_at_several_relays() {
    let first_relay = TestRelay::start().await;
    let second_relay = TestRelay::start().await;
    let laptop = TestServer::start("relay-own-logins-laptop").await;
    let workstation = TestServer::start("relay-own-logins-workstation").await;

    laptop.log_in(&first_relay, "583231", "octocat").await;
    laptop.log_in(&second_relay, "583231", "octocat").await;
    for relay in [&first_relay, &second_relay] {
        laptop
            .wait_for_state(&relay.address(), RelayState::LoggedIn)
            .await;
    }

    workstation
        .client
        .add_relay(first_relay.address())
        .await
        .unwrap();
    assert_eq!(
        workstation
            .relay(&first_relay.address())
            .await
            .unwrap()
            .state,
        RelayState::LoginNeeded,
        "another Server's Login on this machine is not this one's"
    );
    workstation.log_in(&first_relay, "583231", "octocat").await;

    let accounts = first_relay.running().store().accounts().await.unwrap();
    assert_eq!(accounts.len(), 1, "both Servers log in as one Account");
    let mut fingerprints = first_relay
        .running()
        .store()
        .logins()
        .await
        .unwrap()
        .into_iter()
        .map(|login| {
            assert_eq!(login.account, accounts[0].id);
            login.fingerprint
        })
        .collect::<Vec<_>>();
    fingerprints.sort();
    let mut expected = vec![laptop.fingerprint(), workstation.fingerprint()];
    expected.sort();
    assert_ne!(expected[0], expected[1]);
    assert_eq!(
        fingerprints, expected,
        "each Server holds a Login of its own"
    );
    assert_eq!(
        second_relay.running().store().logins().await.unwrap().len(),
        1
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_relay_speaking_another_protocol_version_is_refused_saying_which_side_is_behind() {
    let Version::Unstable(spoken) = SPOKEN[0] else {
        panic!("the Relay protocol is unstable until its first release");
    };
    let server = TestServer::start("relay-version-mismatch").await;
    for (relay_version, behind) in [
        (Version::Unstable(spoken + 1), "this Server is behind"),
        (Version::Unstable(spoken - 1), "the Relay is behind"),
    ] {
        let relay = TestRelay::speaking(vec![relay_version]).await;
        server.client.add_relay(relay.address()).await.unwrap();
        let refused = server
            .client
            .begin_relay_login(&relay.address())
            .await
            .expect_err("a Relay speaking another version refuses the Server");
        assert_eq!(
            error_code(&refused),
            SessionErrorCode::RelayProtocolMismatch
        );
        assert!(refused.to_string().contains(behind), "{refused:#}");
    }

    let mut relay = TestRelay::start().await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    relay
        .restart_speaking(vec![Version::Unstable(spoken + 1)])
        .await;
    let unreachable = server
        .wait_for_relay(&address, |relay| {
            relay.state == RelayState::Unreachable
                && relay
                    .unreachable
                    .as_ref()
                    .is_some_and(|why| why.behind == Some(RelaySide::Server))
        })
        .await;
    let why = unreachable.unreachable.unwrap();
    assert!(why.message.contains("this Server is behind"), "{why:?}");
    assert_eq!(
        unreachable.account,
        Some(account("octocat")),
        "the Login stands while the Server cannot speak to its Relay"
    );
    relay.restart().await;
    server.wait_for_state(&address, RelayState::LoggedIn).await;

    server.shutdown().await;
}

#[tokio::test]
async fn relay_entries_are_kept_owner_only_beside_remotes_and_peers_and_hold_no_credential() {
    let relay = TestRelay::start().await;
    let server = TestServer::start("relay-records").await;
    server.log_in(&relay, "583231", "octocat").await;

    let path = server.config.data_dir().join("relays.json");
    assert_eq!(
        path.parent(),
        Some(server.config.data_dir()),
        "kept with the Server's Remotes and Peers"
    );
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the stored Relays")).unwrap();
    assert_eq!(
        stored,
        serde_json::json!([{ "address": relay.address(), "logged_in": true, "login_needed": false, "serve_through": false }]),
        "the Server proves its key each time and stores no credential for the Relay"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    crate::assert_windows_current_user_only(&path);

    server.shutdown().await;
}

#[tokio::test]
async fn what_cannot_be_done_with_a_relay_is_refused_saying_why() {
    let server = TestServer::start("relay-refusals").await;
    for address in [
        "",
        "ftp://relay.example.com",
        "https://user:secret@relay.example.com",
    ] {
        let refused = server
            .client
            .add_relay(address)
            .await
            .expect_err("an address no Relay is reached at is refused");
        assert_eq!(error_code(&refused), SessionErrorCode::InvalidRelayAddress);
    }

    let unanswering = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = format!("http://{}", unanswering.local_addr().unwrap());
    drop(unanswering);
    server.client.add_relay(address.clone()).await.unwrap();
    let refused = server
        .client
        .add_relay(format!("{address}/"))
        .await
        .expect_err("a Relay is added once, however its address is written");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayAlreadyAdded);
    let refused = server
        .client
        .follow_relay_login(&address)
        .await
        .expect_err("no login has been begun");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayLoginNotFound);
    let refused = server
        .client
        .begin_relay_login(&address)
        .await
        .expect_err("a Relay that does not answer cannot log anyone in");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayUnreachable);
    for refused in [
        server
            .client
            .begin_relay_login("https://elsewhere.example.com")
            .await
            .expect_err("an unknown Relay"),
        server
            .client
            .remove_relay("https://elsewhere.example.com")
            .await
            .expect_err("an unknown Relay"),
    ] {
        assert_eq!(error_code(&refused), SessionErrorCode::RelayNotFound);
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_relay_whose_certificate_this_machine_does_not_trust_is_not_reached() {
    let certificate =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
            .expect("mint a certificate no trust store holds");
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certificate.cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(certificate.signing_key.serialize_der().into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = format!("https://{}", listener.local_addr().unwrap());
    let handshakes = Arc::new(AtomicU64::new(0));
    let attempted = handshakes.clone();
    let accepting = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            attempted.fetch_add(1, Ordering::AcqRel);
            let _ = acceptor.accept(stream).await;
        }
    });

    let server = TestServer::start("relay-untrusted-certificate").await;
    server.client.add_relay(address.clone()).await.unwrap();
    let refused = server
        .client
        .begin_relay_login(&address)
        .await
        .expect_err("a certificate the trust store does not hold is refused");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayUnreachable);
    assert!(
        refused.to_string().contains("certificate"),
        "the refusal says the certificate is not trusted: {refused:#}"
    );
    assert!(
        handshakes.load(Ordering::Acquire) > 0,
        "the Server did dial"
    );

    accepting.abort();
    server.shutdown().await;
}

#[tokio::test]
async fn a_relay_that_takes_connections_and_falls_silent_reads_unreachable_until_it_answers() {
    let mut relay = TestRelay::start().await;
    let mut server = TestServer::with_timings(
        "relay-silent",
        relay_timings()
            .with_relay_answer_timeout(Duration::from_secs(1))
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100)),
    )
    .await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    // A restarted Server learns its Account only once the connection it
    // keeps has proven its key, so from then on that connection stands open.
    server.restart().await;
    server
        .wait_for_relay(&address, |relay| relay.account.is_some())
        .await;

    relay.route.stall().await;
    let silent = server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    let why = silent.unreachable.expect("an Unreachable Relay says why");
    assert_eq!(why.behind, None);
    assert!(
        why.message.contains("stopped answering"),
        "the connection standing open is found silent, not one being made: {why:?}"
    );
    assert_eq!(
        silent.account,
        Some(account("octocat")),
        "the Login stands while its Relay says nothing"
    );

    relay.route.set_online(false).await;
    relay.route.set_online(true).await;
    let answering = server.wait_for_state(&address, RelayState::LoggedIn).await;
    assert_eq!(answering.unreachable, None);

    server.shutdown().await;
}

#[tokio::test]
async fn a_login_the_server_cannot_record_is_not_reported_done_and_a_later_one_records_it() {
    let relay = TestRelay::start().await;
    let mut server = TestServer::start("relay-unrecorded").await;
    let address = relay.address();
    server.client.add_relay(address.clone()).await.unwrap();
    // Where the Server keeps its Relays now holds something no file can
    // replace.
    let records = server.config.data_dir().join("relays.json");
    std::fs::remove_file(&records).unwrap();
    std::fs::create_dir(&records).unwrap();

    let login = server.client.begin_relay_login(&address).await.unwrap();
    relay.approve(&login.user_code, "583231", "octocat");
    let unrecorded = server.client.follow_relay_login(&address).await.unwrap();
    let RelayLoginOutcome::Refused { reason, message } = unrecorded.outcome else {
        panic!(
            "a Login the Server could not record is not done: {:?}",
            unrecorded.outcome
        );
    };
    assert_eq!(reason, RelayLoginRefusal::Unrecorded);
    assert!(message.contains("log in again"), "{message}");
    assert_eq!(
        server.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );

    std::fs::remove_dir(&records).unwrap();
    let recorded = server.log_in(&relay, "583231", "octocat").await;
    assert_eq!(
        recorded.outcome,
        RelayLoginOutcome::Done {
            account: account("octocat"),
        }
    );
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&records).unwrap()).unwrap();
    assert_eq!(
        stored,
        serde_json::json!([{ "address": address, "logged_in": true, "login_needed": false, "serve_through": false }])
    );
    server.restart().await;
    server
        .wait_for_relay(&address, |relay| relay.account.is_some())
        .await;

    server.shutdown().await;
}

/// The Relay refuses the Server's Login while the Server cannot store its
/// Relays. The refusal holds at once, and is stored once the Server can
/// store again, as the Relay goes on refusing — so a restart while the Relay
/// cannot be reached still reads login needed.
#[tokio::test]
async fn a_refused_login_the_server_could_not_store_at_first_is_stored_once_it_can() {
    let mut relay = TestRelay::start().await;
    let mut server = TestServer::start("relay-login-needed-unstored").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    relay.route.wait_for_connections(1).await;
    let records = server.config.data_dir().join("relays.json");
    std::fs::remove_file(&records).unwrap();
    std::fs::create_dir(&records).unwrap();

    relay.provider.set_admitted("583231", false);
    server
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;
    std::fs::remove_dir(&records).unwrap();
    let stored = timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Ok(stored) = std::fs::read(&records) {
                return serde_json::from_slice::<serde_json::Value>(&stored).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the refusal is stored once the Server can store again");
    assert_eq!(
        stored,
        serde_json::json!([{ "address": address, "logged_in": true, "login_needed": true, "serve_through": false }])
    );

    relay.route.set_online(false).await;
    server.restart().await;
    let tried = relay.route.opened_connections();
    relay.route.wait_for_opened_connections(tried + 2).await;
    assert_eq!(
        server.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_client_following_a_pending_login_holds_up_no_shutdown() {
    let relay = TestRelay::start().await;
    let mut server = TestServer::start("relay-follower-shutdown").await;
    let address = relay.address();
    server.client.add_relay(address.clone()).await.unwrap();
    server.client.begin_relay_login(&address).await.unwrap();

    let running = server.server.take().expect("the Server is running");
    let descriptor = running.descriptor().clone();
    let mut url = reqwest::Url::parse(&descriptor.base_url).unwrap();
    url.path_segments_mut()
        .unwrap()
        .extend(["v1", "relays", &address, "login"]);
    let mut following = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("follow the login");
    assert!(following.status().is_success());
    let first = timeout(PROGRESS_DEADLINE, following.chunk())
        .await
        .expect("the login's progress arrives")
        .unwrap()
        .expect("the stream stands open");
    assert!(String::from_utf8_lossy(&first).contains("pending"));

    timeout(PROGRESS_DEADLINE, running.shutdown())
        .await
        .expect("a Client following a login the user has yet to finish holds up no shutdown")
        .expect("stop the Server");
    timeout(PROGRESS_DEADLINE, async {
        while let Ok(Some(_)) = following.chunk().await {}
    })
    .await
    .expect("the follower is let go");
}

#[tokio::test]
async fn a_relay_choosing_a_version_the_server_never_offered_is_proven_nothing() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let proofs = Arc::new(AtomicU64::new(0));
    let proven = proofs.clone();
    let named = address.clone();
    let answering = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                continue;
            };
            let _hello = socket.next().await;
            let challenge = RelayMessage::Challenge {
                version: Version::Stable(999),
                nonce: Bytes(vec![0; 32]),
                relay: named.clone(),
            };
            let _ = socket
                .send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await;
            while let Some(Ok(message)) = socket.next().await {
                if message
                    .to_text()
                    .is_ok_and(|text| text.contains("\"proof\""))
                {
                    proven.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
    });

    let server = TestServer::start("relay-unoffered-version").await;
    server.client.add_relay(address.clone()).await.unwrap();
    let refused = server
        .client
        .begin_relay_login(&address)
        .await
        .expect_err("a Relay that chose a version never offered is refused");
    assert_eq!(
        error_code(&refused),
        SessionErrorCode::RelayProtocolMismatch
    );
    assert!(
        refused.to_string().contains("this Server is behind"),
        "{refused:#}"
    );
    assert_eq!(
        proofs.load(Ordering::Acquire),
        0,
        "nothing was signed for it"
    );

    answering.abort();
    server.shutdown().await;
}

#[tokio::test]
async fn a_relay_naming_itself_by_another_address_is_proven_nothing() {
    let proved = Arc::new(AtomicBool::new(false));
    let heard_out = Arc::new(Notify::new());
    let (proving, hearing_out) = (proved.clone(), heard_out.clone());
    let (address, relay) = scripted_relay(move |mut socket, _named, _| {
        let (proving, hearing_out) = (proving.clone(), hearing_out.clone());
        async move {
            let answer = challenge(&mut socket, "https://relay-elsewhere.example.com").await;
            if matches!(answer, Some(ServerMessage::Proof { .. })) {
                proving.store(true, Ordering::Release);
            }
            hearing_out.notify_one();
        }
    })
    .await;
    let server = TestServer::start("relay-named-otherwise").await;
    server.client.add_relay(address.clone()).await.unwrap();

    let refused = server
        .client
        .begin_relay_login(&address)
        .await
        .expect_err("a Relay known by another address is proven nothing");
    timeout(PROGRESS_DEADLINE, heard_out.notified())
        .await
        .expect("the Relay hears the Server out");
    assert!(
        !proved.load(Ordering::Acquire),
        "nothing was signed for a Relay naming itself otherwise"
    );
    assert_eq!(error_code(&refused), SessionErrorCode::RelayRefused);
    assert!(
        refused
            .to_string()
            .contains("https://relay-elsewhere.example.com"),
        "the refusal names the address the Relay names itself by: {refused:#}"
    );

    relay.abort();
    server.shutdown().await;
}

type RelaySocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// A stand-in for a Relay at a loopback address, known by that address,
/// that answers each connection as `script` says: handed the socket, the
/// address, and how many connections came before it.
async fn scripted_relay<F, Fut>(script: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: Fn(RelaySocket, String, usize) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let named = address.clone();
    let answering = tokio::spawn(async move {
        let mut connections = 0;
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
                continue;
            };
            tokio::spawn(script(socket, named.clone(), connections));
            connections += 1;
        }
    });
    (address, answering)
}

/// The next thing the Server says, or `None` once it has gone.
async fn heard(socket: &mut RelaySocket) -> Option<ServerMessage> {
    loop {
        match socket.next().await? {
            Ok(Message::Text(text)) => return serde_json::from_str(text.as_str()).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn tell(socket: &mut RelaySocket, message: &RelayMessage) -> bool {
    socket
        .send(Message::Text(
            serde_json::to_string(message).unwrap().into(),
        ))
        .await
        .is_ok()
}

/// Hears a Server's hello and challenges it as the Relay known as `relay`:
/// what the Server says next.
async fn challenge(socket: &mut RelaySocket, relay: &str) -> Option<ServerMessage> {
    let ServerMessage::Hello { .. } = heard(socket).await? else {
        return None;
    };
    tell(
        socket,
        &RelayMessage::Challenge {
            version: SPOKEN[0],
            nonce: Bytes(vec![0; 32]),
            relay: relay.to_owned(),
        },
    )
    .await;
    heard(socket).await
}

/// Takes a Server's hello and proof, the proof unchecked, and says its Login
/// stands: whether the Server proved anything.
async fn greet(socket: &mut RelaySocket, relay: &str) -> bool {
    matches!(
        challenge(socket, relay).await,
        Some(ServerMessage::Proof { .. })
    ) && tell(
        socket,
        &RelayMessage::Proven {
            login: Some(suru_relay_protocol::Account {
                provider: "scripted".to_owned(),
                username: "octocat".to_owned(),
            }),
        },
    )
    .await
}

/// Answers what a Server asks once it has proven itself, writing down each
/// thing it hears: a login is begun, and lasts until the Server ends it —
/// the Relay letting go of it only once `let_go` says so, where it is given —
/// and a Login is forgotten, the answer held back until `release` says so,
/// where it is given.
async fn note_what_is_asked(
    mut socket: RelaySocket,
    relay: String,
    notes: Arc<Mutex<Vec<&'static str>>>,
    release: Option<Arc<Notify>>,
    let_go: Option<Arc<Notify>>,
) {
    if !greet(&mut socket, &relay).await {
        return;
    }
    match heard(&mut socket).await {
        Some(ServerMessage::BeginLogin { .. }) => {
            notes.lock().unwrap().push("begin login");
            tell(
                &mut socket,
                &RelayMessage::LoginStarted {
                    verification_uri: SCRIPTED_VERIFICATION_URI.to_owned(),
                    user_code: "CODE-HELD".to_owned(),
                    expires_in_seconds: 900,
                },
            )
            .await;
            while heard(&mut socket).await.is_some() {}
            notes.lock().unwrap().push("login ended");
            if let Some(let_go) = let_go {
                let_go.notified().await;
                notes.lock().unwrap().push("login let go");
            }
        }
        Some(ServerMessage::Forget) => {
            notes.lock().unwrap().push("forget");
            if let Some(release) = release {
                release.notified().await;
            }
            tell(&mut socket, &RelayMessage::Forgotten).await;
        }
        _ => {}
    }
}

#[tokio::test]
async fn a_relay_that_pings_without_reading_reads_unreachable() {
    let let_go = Arc::new(Notify::new());
    let letting_go = let_go.clone();
    let (address, relay) = scripted_relay(move |mut socket, named, connection| {
        let letting_go = letting_go.clone();
        async move {
            if !greet(&mut socket, &named).await {
                return;
            }
            if connection == 0 {
                if let Some(ServerMessage::BeginLogin { .. }) = heard(&mut socket).await {
                    tell(
                        &mut socket,
                        &RelayMessage::LoginStarted {
                            verification_uri: SCRIPTED_VERIFICATION_URI.to_owned(),
                            user_code: "CODE-PINGING".to_owned(),
                            expires_in_seconds: 900,
                        },
                    )
                    .await;
                    tell(
                        &mut socket,
                        &RelayMessage::LoginDone {
                            account: suru_relay_protocol::Account {
                                provider: "scripted".to_owned(),
                                username: "octocat".to_owned(),
                            },
                        },
                    )
                    .await;
                    while heard(&mut socket).await.is_some() {}
                }
                return;
            }
            // The connection the Server keeps is pinged without end, and
            // nothing it sends is read.
            while socket
                .send(Message::Ping(vec![1; 125].into()))
                .await
                .is_ok()
            {}
            letting_go.notify_one();
        }
    })
    .await;
    let server = TestServer::with_timings(
        "relay-pinging",
        relay_timings()
            .with_relay_answer_timeout(Duration::from_secs(1))
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100)),
    )
    .await;
    server.client.add_relay(address.clone()).await.unwrap();
    server.client.begin_relay_login(&address).await.unwrap();
    assert_eq!(
        server
            .client
            .follow_relay_login(&address)
            .await
            .unwrap()
            .outcome,
        RelayLoginOutcome::Done {
            account: account("octocat"),
        }
    );

    let silent = server
        .wait_for_state(&address, RelayState::Unreachable)
        .await;
    let why = silent.unreachable.expect("an Unreachable Relay says why");
    assert!(
        why.message.contains("stopped answering"),
        "a Relay that talks without reading is found silent all the same: {why:?}"
    );
    timeout(PROGRESS_DEADLINE, let_go.notified())
        .await
        .expect("the Server lets go of a Relay that never reads what it asks");

    relay.abort();
    server.shutdown().await;
}

#[tokio::test]
async fn a_relay_saying_its_login_lasts_forever_upsets_none_of_the_servers_relays() {
    let (address, relay) = scripted_relay(|mut socket, named, _| async move {
        if !greet(&mut socket, &named).await {
            return;
        }
        match heard(&mut socket).await {
            Some(ServerMessage::BeginLogin { .. }) => {
                tell(
                    &mut socket,
                    &RelayMessage::LoginStarted {
                        verification_uri: SCRIPTED_VERIFICATION_URI.to_owned(),
                        user_code: "CODE-FOREVER".to_owned(),
                        expires_in_seconds: u64::MAX,
                    },
                )
                .await;
                while heard(&mut socket).await.is_some() {}
            }
            Some(ServerMessage::Forget) => {
                tell(&mut socket, &RelayMessage::Forgotten).await;
            }
            _ => {}
        }
    })
    .await;
    let server = TestServer::start("relay-forever-login").await;
    server.client.add_relay(address.clone()).await.unwrap();

    let login = server
        .client
        .begin_relay_login(&address)
        .await
        .expect("a login said to last forever is begun");
    assert_eq!(login.outcome, RelayLoginOutcome::Pending);
    assert_eq!(
        server.relay(&address).await.unwrap().login,
        Some(login),
        "the Server's Relays are listed as before"
    );
    assert!(
        server
            .client
            .remove_relay(&address)
            .await
            .expect("the Relay is removed as before")
            .acknowledged
    );
    assert!(server.client.list_relays().await.unwrap().is_empty());

    relay.abort();
    server.shutdown().await;
}

#[tokio::test]
async fn a_removal_that_cannot_be_stored_leaves_the_relay_going_on_as_before() {
    let mut relay = TestRelay::start().await;
    let server = TestServer::start("relay-removal-unstored").await;
    let address = relay.address();
    server.log_in(&relay, "583231", "octocat").await;
    server.client.begin_relay_login(&address).await.unwrap();
    let records = server.config.data_dir().join("relays.json");
    std::fs::remove_file(&records).unwrap();
    std::fs::create_dir(&records).unwrap();

    let (followed, removal) = timeout(PROGRESS_DEADLINE, async {
        tokio::join!(
            server.client.follow_relay_login(&address),
            server.client.remove_relay(&address),
        )
    })
    .await
    .expect("neither the removal nor a Client following the login waits forever");
    let refused = removal.expect_err("a removal that cannot be stored is refused");
    assert_eq!(
        error_code(&refused),
        SessionErrorCode::RelayRecordsUnwritable
    );
    let followed = followed.expect("the login given up is settled");
    assert!(
        matches!(
            followed.outcome,
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Interrupted,
                ..
            }
        ),
        "{followed:?}"
    );
    let kept = server.relay(&address).await.expect("the entry stays");
    assert_eq!(
        kept.state,
        RelayState::LoginNeeded,
        "the Relay did forget the Login"
    );
    let opened = relay.route.opened_connections();
    relay.route.wait_for_opened_connections(opened + 2).await;

    std::fs::remove_dir(&records).unwrap();
    assert_eq!(
        server.log_in(&relay, "583231", "octocat").await.outcome,
        RelayLoginOutcome::Done {
            account: account("octocat"),
        }
    );
    server
        .wait_for_relay(&address, |relay| {
            relay.state == RelayState::LoggedIn && relay.account.is_some()
        })
        .await;

    server.shutdown().await;
}

#[tokio::test]
async fn no_login_begun_while_a_relay_is_removed_outlives_the_removal() {
    let notes = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let (noting, releasing) = (notes.clone(), release.clone());
    let (address, relay) = scripted_relay(move |socket, named, _| {
        note_what_is_asked(socket, named, noting.clone(), Some(releasing.clone()), None)
    })
    .await;
    let server = TestServer::start("relay-removal-racing").await;
    server.client.add_relay(address.clone()).await.unwrap();

    let racing = async {
        timeout(PROGRESS_DEADLINE, async {
            while !notes.lock().unwrap().contains(&"forget") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the Relay is asked to forget");
        // A login asked for while the Relay has yet to answer has every
        // chance to reach it before the answer is let through.
        let (attempt, ()) = tokio::join!(server.client.begin_relay_login(&address), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            release.notify_one();
        });
        attempt
    };
    let (removal, attempt) = timeout(PROGRESS_DEADLINE, async {
        tokio::join!(server.client.remove_relay(&address), racing)
    })
    .await
    .expect("the removal and the login both end");
    assert!(removal.expect("remove the Relay").acknowledged);
    let refused = attempt.expect_err("no login is begun at a Relay being removed");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayNotFound);
    assert_eq!(
        *notes.lock().unwrap(),
        vec!["forget"],
        "no login reached the Relay once it was asked to forget"
    );

    relay.abort();
    server.shutdown().await;
}

#[tokio::test]
async fn a_login_under_way_is_let_go_at_the_relay_before_it_is_asked_to_forget() {
    let notes = Arc::new(Mutex::new(Vec::new()));
    let let_go = Arc::new(Notify::new());
    let (noting, letting_go) = (notes.clone(), let_go.clone());
    let (address, relay) = scripted_relay(move |socket, named, _| {
        note_what_is_asked(
            socket,
            named,
            noting.clone(),
            None,
            Some(letting_go.clone()),
        )
    })
    .await;
    let server = TestServer::start("relay-removal-under-way").await;
    server.client.add_relay(address.clone()).await.unwrap();
    server.client.begin_relay_login(&address).await.unwrap();

    let holding_on = async {
        timeout(PROGRESS_DEADLINE, async {
            while !notes.lock().unwrap().contains(&"login ended") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the Server ends the login under way");
        // The Relay holds on to the login a while: whatever it might yet
        // record for it, it has not finished.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let heard_meanwhile = notes.lock().unwrap().clone();
        let_go.notify_one();
        heard_meanwhile
    };
    let (removal, heard_meanwhile) = timeout(PROGRESS_DEADLINE, async {
        tokio::join!(server.client.remove_relay(&address), holding_on)
    })
    .await
    .expect("the removal ends once the Relay lets the login go");
    assert!(removal.expect("remove the Relay").acknowledged);
    assert!(
        !heard_meanwhile.contains(&"forget"),
        "the Relay was asked to forget while it still held the login: {heard_meanwhile:?}"
    );
    assert_eq!(
        *notes.lock().unwrap(),
        vec!["begin login", "login ended", "login let go", "forget"]
    );

    relay.abort();
    server.shutdown().await;
}
