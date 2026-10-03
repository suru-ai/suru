//! A Server's Relays, driven through the Server's own API against a real
//! Relay in-process whose identity provider the test scripts.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Relay, RelayAccount, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelaySide,
        RelayState, SessionError, SessionErrorCode,
    },
    server::{self, ServerConfig, ServerTimings},
};
use suru_relay::{
    Clock, Identity, RelayConfig, RunningRelay, SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
use suru_relay_protocol::{SPOKEN, Version};
use tokio::time::timeout;

use crate::support::{
    PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, receive_initial_state,
};

/// A real Relay, reached through a route a test can take offline and point
/// at the Relay again once it restarts elsewhere, keeping its records and
/// its scripted identity provider across restarts.
struct TestRelay {
    directory: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    /// How far past the operating system's time the Relay's clock reads.
    clock_ahead: Arc<AtomicU64>,
    running: Option<RunningRelay>,
    route: ObservedTcpProxy,
}

impl TestRelay {
    async fn start() -> Self {
        Self::speaking(SPOKEN.to_vec()).await
    }

    async fn speaking(versions: Vec<Version>) -> Self {
        let directory = tempfile::tempdir().expect("create the Relay's directory");
        let provider = Arc::new(ScriptedProvider::new());
        let clock_ahead = Arc::new(AtomicU64::new(0));
        let running = run_relay(&directory, &provider, &clock_ahead, versions).await;
        let route = ObservedTcpProxy::start(running.address()).await;
        Self {
            directory,
            provider,
            clock_ahead,
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
        let running = run_relay(&self.directory, &self.provider, &self.clock_ahead, versions).await;
        self.route.retarget(running.address());
        self.running = Some(running);
    }

    async fn restart(&mut self) {
        self.restart_speaking(SPOKEN.to_vec()).await;
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
    versions: Vec<Version>,
) -> RunningRelay {
    let clock_ahead = clock_ahead.clone();
    suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            directory.path().join("relay.db"),
        )
        .with_protocol_versions(versions)
        .with_clock(Clock::from_fn(move || {
            SystemTime::now() + Duration::from_secs(clock_ahead.load(Ordering::Acquire))
        })),
        provider.clone(),
    )
    .await
    .expect("start the Relay")
}

/// A Server and a Client attached to it, which a test may restart on the
/// same records.
struct TestServer {
    state: tempfile::TempDir,
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
        let config = ServerConfig::new(state.path(), channel).expect("configure the Server");
        let (server, client) = run_server(&config, &timings, state.path(), channel).await;
        Self {
            state,
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
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state, channel).expect("configure the Client"),
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
    server
        .wait_for_state(
            &address,
            RelayState::ProtocolMismatch {
                behind: RelaySide::Server,
            },
        )
        .await;
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
        serde_json::json!([{ "address": relay.address(), "logged_in": true }]),
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
