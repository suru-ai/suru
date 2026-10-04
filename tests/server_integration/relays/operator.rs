//! The operator's command line at a Relay Servers are paired through, seen
//! at the Servers' own API: removing a Login, or an Account, takes the
//! Servers it names off the running Relay at once — a live stream through it
//! cut — and ends no Pairing (ADR-0048).

use suru::{
    managed_client::ManagedEvent,
    protocol::{Outlook, RelayLoginOutcome, RelayState, RemoteStatus, Way},
};
use suru_relay::{AccountsCommand, LoginsCommand, OperatorCommand};
use tokio::time::Duration;

use super::{
    TestRelay, TestServer,
    pairing::{REMOTE, next_catalog_event, serving_through, until_recovered, until_recovering},
    relay_timings,
};

/// How often the Relays these tests start look for Logins their operator
/// has removed.
const REMOVAL_INTERVAL: Duration = Duration::from_millis(10);

/// A workstation Serving through a Relay, a laptop paired with it by an
/// Invite offering that Relay alone, and a tablet paired with it by one
/// offering its listener as well, all three logged in there under one
/// Account.
struct Paired {
    relay: TestRelay,
    workstation: TestServer,
    laptop: TestServer,
    tablet: TestServer,
}

impl Paired {
    async fn start(channel: &str) -> Self {
        let relay =
            TestRelay::configured(|config| config.with_removal_interval(REMOVAL_INTERVAL)).await;
        let (workstation, laptop) = serving_through(&relay, channel, relay_timings()).await;
        let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;
        laptop
            .redeem_as(invite, REMOTE)
            .await
            .expect("pair through the Relay");
        let tablet = TestServer::start(&format!("{channel}-tablet")).await;
        tablet.log_in(&relay, "583231", "octocat").await;
        let both = workstation
            .invite(vec![
                Way::Direct(workstation.serving_address()),
                Way::Relay(relay.address()),
            ])
            .await;
        tablet
            .redeem_as(both, REMOTE)
            .await
            .expect("pair the tablet by both ways");
        Self {
            relay,
            workstation,
            laptop,
            tablet,
        }
    }

    /// Runs the operator's command line on the Relay's records as it runs,
    /// as `command` says: what it printed.
    async fn operate(&self, command: OperatorCommand) -> String {
        let mut printed = Vec::new();
        suru_relay::operate(
            &self.relay.directory.path().join("relay.db"),
            command,
            &mut printed,
        )
        .await
        .expect("the operator's command line does as it is asked");
        String::from_utf8(printed).unwrap()
    }

    /// The laptop's view of the workstation's Session catalog, streamed
    /// through the Relay, once it has been read.
    async fn laptops_catalog(&self) -> suru::managed_client::SessionCatalogSubscription {
        let mut catalog = self
            .laptop
            .client
            .outlook(Outlook::Remote(REMOTE.to_owned()))
            .subscribe_catalog();
        assert!(matches!(
            next_catalog_event(&mut catalog).await,
            Some(ManagedEvent::SessionCatalogReconciled(_))
        ));
        catalog
    }

    /// Asserts that no Pairing has ended: the workstation's Peers are
    /// `peers`, and the laptop and the tablet each still have the
    /// workstation as a Remote.
    async fn assert_no_pairing_ended(&self, peers: &[suru::protocol::Peer]) {
        assert_eq!(
            self.workstation.client.list_peers().await.unwrap(),
            peers,
            "no Pairing ends"
        );
        for server in [&self.laptop, &self.tablet] {
            let remotes = server.client.list_remotes().await.unwrap();
            assert_eq!(remotes.len(), 1);
            assert_ne!(remotes[0].status, RemoteStatus::Revoked);
        }
    }

    async fn shutdown(self) {
        self.tablet.shutdown().await;
        self.laptop.shutdown().await;
        self.workstation.shutdown().await;
    }
}

/// The laptop is lost, and its Login is removed at the Relay as it runs: the
/// stream through the Relay is cut at once and logged, the laptop reads
/// login needed while every other Server stays logged in, a Remote it
/// reaches only through the Relay reads Unreachable, and no Pairing ends.
/// Logging in again forms a new Login, and the Remote answers again with
/// nobody asking.
#[tokio::test]
async fn removing_a_login_at_a_relay_cuts_its_live_stream_and_logging_in_again_forms_a_new_one() {
    let paired = Paired::start("relay-operator-login").await;
    let address = paired.relay.address();
    let peers = paired.workstation.client.list_peers().await.unwrap();
    assert_eq!(peers.len(), 2);
    let mut catalog = paired.laptops_catalog().await;
    let logged = paired.relay.joined_connections_logged();

    let laptops = paired.laptop.fingerprint();
    let printed = paired
        .operate(OperatorCommand::Logins(LoginsCommand::Remove {
            fingerprint: laptops.clone(),
        }))
        .await;
    assert!(
        printed.starts_with("Removed the Login of ") && printed.contains(&laptops),
        "{printed}"
    );
    until_recovering(&mut catalog).await;
    paired
        .relay
        .wait_for_joined_connections_logged(logged + 1)
        .await;
    let removed = paired
        .laptop
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;
    assert_eq!(removed.unreachable, None);
    for server in [&paired.workstation, &paired.tablet] {
        assert_eq!(
            server.relay(&address).await.unwrap().state,
            RelayState::LoggedIn,
            "a Login not removed stands"
        );
    }
    assert_eq!(
        paired
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .unwrap()
            .status,
        RemoteStatus::Unavailable,
        "a Remote reached only through the Relay reads Unreachable"
    );
    paired.assert_no_pairing_ended(&peers).await;
    let store = paired.relay.running().store();
    let logins = store.logins().await.unwrap();
    assert_eq!(logins.len(), 2);
    assert!(logins.iter().all(|login| login.fingerprint != laptops));

    let login = paired
        .laptop
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    assert!(
        matches!(login.outcome, RelayLoginOutcome::Done { .. }),
        "{login:?}"
    );
    assert!(
        store
            .logins()
            .await
            .unwrap()
            .iter()
            .any(|login| login.fingerprint == laptops),
        "logging in again forms a new Login"
    );
    until_recovered(&mut catalog).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    paired.shutdown().await;
}

/// octocat has gone, and their Account is removed at the Relay as it runs:
/// every Server logged in under it reads login needed at once, the stream
/// through the Relay cut, a Remote with a direct way going on working, and no
/// Pairing ends. octocat logging in again from one Server forms a new
/// Account, which restores no Login removed with the one before — unlike a
/// lapsed Account — so each Server logs in again of its own.
#[tokio::test]
async fn removing_an_account_at_a_relay_cuts_every_login_under_it_and_ends_no_pairing() {
    let paired = Paired::start("relay-operator-account").await;
    let address = paired.relay.address();
    let peers = paired.workstation.client.list_peers().await.unwrap();
    let mut catalog = paired.laptops_catalog().await;
    let logged = paired.relay.joined_connections_logged();

    let printed = paired
        .operate(OperatorCommand::Accounts(AccountsCommand::Remove {
            provider: "scripted".to_owned(),
            id: "583231".to_owned(),
        }))
        .await;
    assert!(
        printed.starts_with("Removed the Account octocat (scripted 583231) and the 3 Logins"),
        "{printed}"
    );
    until_recovering(&mut catalog).await;
    paired
        .relay
        .wait_for_joined_connections_logged(logged + 1)
        .await;
    for server in [&paired.workstation, &paired.laptop, &paired.tablet] {
        let removed = server
            .wait_for_state(&address, RelayState::LoginNeeded)
            .await;
        assert_eq!(removed.unreachable, None);
    }
    assert_eq!(
        paired
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .unwrap()
            .status,
        RemoteStatus::Unavailable,
        "a Remote reached only through the Relay reads Unreachable"
    );
    assert_eq!(
        paired
            .tablet
            .client
            .probe_remote(REMOTE)
            .await
            .unwrap()
            .status,
        RemoteStatus::Available,
        "a Remote with a direct way goes on working"
    );
    paired.assert_no_pairing_ended(&peers).await;
    let store = paired.relay.running().store();
    assert!(store.accounts().await.unwrap().is_empty());
    assert!(store.logins().await.unwrap().is_empty());

    let login = paired
        .laptop
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    assert!(
        matches!(login.outcome, RelayLoginOutcome::Done { .. }),
        "{login:?}"
    );
    assert_eq!(
        store.logins().await.unwrap().len(),
        1,
        "the new Account restores no Login removed with the one before"
    );
    assert_eq!(
        paired.workstation.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );
    let login = paired
        .workstation
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    assert!(
        matches!(login.outcome, RelayLoginOutcome::Done { .. }),
        "{login:?}"
    );
    until_recovered(&mut catalog).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    paired.shutdown().await;
}
