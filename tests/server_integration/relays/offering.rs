//! Peers learning the ways a Serving Server offers: a Serving Server tells
//! each Peer, over the Pairing itself, which Relays it Serves through — as
//! the Peer connects, and again whenever that changes while it is connected
//! — and the Remote entry on the Peer keeps up, taking up a Relay newly
//! offered and dropping one no longer offered, its direct ways staying as its
//! Invite gave them. A Relay learned so that the Peer holds no entry for is
//! listed with the Remote and never dialled.

use suru::{
    managed_client::ManagedEvent,
    protocol::{Outlook, RemoteStatus, UnreachableReason, Way},
};
use tokio::time::{Duration, timeout};

use super::{
    TestRelay, TestServer,
    pairing::{REMOTE, RemoteApi, next_catalog_event, until_recovered, until_recovering},
    relay_timings,
};
use crate::support::{PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy};
use suru::server::ServerTimings;

/// A workstation Serving at its listener and a laptop paired with it by an
/// Invite offering that alone, by a route the test can take offline, and a
/// Relay the workstation has logged in at but does not yet Serve through.
struct PairedDirectly {
    relay: TestRelay,
    workstation: TestServer,
    laptop: TestServer,
    /// The laptop's route to the workstation's listener.
    direct: ObservedTcpProxy,
}

impl PairedDirectly {
    /// The two Servers so, the laptop logged in at the Relay under the
    /// workstation's Account where `logged_in`, and holding no entry for it
    /// otherwise.
    async fn start(channel: &str, logged_in: bool) -> Self {
        Self::with_timings(channel, logged_in, relay_timings()).await
    }

    /// The same, the laptop running by `timings`.
    async fn with_timings(channel: &str, logged_in: bool, timings: ServerTimings) -> Self {
        let relay = TestRelay::start().await;
        let workstation = TestServer::start(&format!("{channel}-workstation")).await;
        let laptop = TestServer::with_timings(&format!("{channel}-laptop"), timings).await;
        workstation.serve().await;
        let direct = ObservedTcpProxy::start(workstation.serving_address()).await;
        let invite = workstation.invite(vec![Way::Direct(direct.address)]).await;
        laptop
            .redeem_as(invite, REMOTE)
            .await
            .expect("pair directly");
        workstation.log_in(&relay, "583231", "octocat").await;
        if logged_in {
            laptop.log_in(&relay, "583231", "octocat").await;
        }
        Self {
            relay,
            workstation,
            laptop,
            direct,
        }
    }

    fn direct_way(&self) -> Way {
        Way::Direct(self.direct.address)
    }

    fn relay_way(&self) -> Way {
        Way::Relay(self.relay.address())
    }

    async fn probe_why(&self) -> (RemoteStatus, Option<UnreachableReason>) {
        let health = self
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote");
        (health.status, health.unreachable)
    }

    async fn shutdown(self) {
        self.laptop.shutdown().await;
        self.workstation.shutdown().await;
    }
}

impl TestServer {
    /// The ways this Server's Remote `workstation` is listed with.
    async fn remote_ways(&self) -> Vec<Way> {
        self.client
            .list_remotes()
            .await
            .expect("list the Remotes")
            .into_iter()
            .find(|remote| remote.name == REMOTE)
            .expect("the Remote stands")
            .ways
    }

    /// The ways this Server stores, of its Remote `workstation`, as having
    /// answered.
    fn answered_ways(&self) -> Vec<Way> {
        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.config.data_dir().join("remotes.json"))
                .expect("read the stored Remotes"),
        )
        .expect("decode the stored Remotes");
        let remote = stored
            .as_array()
            .expect("the Remotes are stored as a list")
            .iter()
            .find(|remote| remote["name"] == REMOTE)
            .expect("the Remote is stored");
        serde_json::from_value(remote["answered"].clone()).expect("decode the ways that answered")
    }

    /// Waits until this Server's Remote `workstation` is listed with `ways`.
    async fn wait_for_remote_ways(&self, ways: &[Way]) {
        let keeping_up = timeout(PROGRESS_DEADLINE, async {
            while self.remote_ways().await != ways {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        if keeping_up.await.is_err() {
            panic!(
                "the Remote never came to be listed with {ways:?}; it is listed with {:?}",
                self.remote_ways().await
            );
        }
    }
}

/// A laptop paired with a workstation at home, by an Invite offering only
/// the workstation's listener, keeps the workstation in view while the
/// workstation comes to Serve through a Relay: the laptop is told of it, and
/// told again each time that changes, over the Pairing it holds open, and its
/// Remote keeps up — gaining the Relay way with no new Invite, remembered
/// across a restart. Once the direct way fails the Relay carries the Remote.
#[tokio::test]
async fn a_pairing_formed_directly_gains_a_relay_way_and_is_reached_through_it_once_direct_fails() {
    let mut paired = PairedDirectly::start("relay-offering-gained", true).await;
    let (direct, relayed) = (paired.direct_way(), paired.relay_way());
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    assert_eq!(
        paired.laptop.remote_ways().await,
        std::slice::from_ref(&direct)
    );

    paired.workstation.serve_through(&paired.relay, true).await;
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;
    paired.workstation.serve_through(&paired.relay, false).await;
    paired
        .laptop
        .wait_for_remote_ways(std::slice::from_ref(&direct))
        .await;
    paired.workstation.serve_through(&paired.relay, true).await;
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;

    drop(catalog);
    paired.laptop.restart().await;
    assert_eq!(
        paired.laptop.remote_ways().await,
        [direct, relayed],
        "the Relay way gained is remembered across a restart"
    );

    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Available, None),
        "the Relay way the Remote gained carries it once the direct way fails"
    );

    paired.shutdown().await;
}

/// A Remote reached through a Relay its Serving Server stops Serving through
/// drops that way as it is told, over the joined stream already carrying the
/// Remote — which stands where it is — and no longer dials it: nothing more is
/// asked at the Relay. Serving through it again is told the same way, and the
/// Remote takes the way up again.
#[tokio::test]
async fn a_relay_way_no_longer_offered_is_dropped_and_no_longer_dialled() {
    let mut paired = PairedDirectly::start("relay-offering-dropped", true).await;
    let (direct, relayed) = (paired.direct_way(), paired.relay_way());
    paired.workstation.serve_through(&paired.relay, true).await;
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;
    paired.direct.set_online(false).await;
    until_recovering(&mut catalog).await;
    until_recovered(&mut catalog).await;
    assert_eq!(paired.probe_why().await, (RemoteStatus::Available, None));
    assert!(
        paired.laptop.answered_ways().contains(&relayed),
        "the Relay way is remembered as having answered"
    );

    // The workstation connects to the Relay again, now not to wait there.
    let opened = paired.relay.route.opened_connections();
    paired.workstation.serve_through(&paired.relay, false).await;
    paired
        .relay
        .route
        .wait_for_opened_connections(opened + 1)
        .await;
    paired
        .laptop
        .wait_for_remote_ways(std::slice::from_ref(&direct))
        .await;
    assert!(
        !paired.laptop.answered_ways().contains(&relayed),
        "a way dropped is no longer remembered as having answered"
    );
    let asked = paired.relay.route.opened_connections();
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Unavailable, None),
        "a Relay way dropped carries nothing new"
    );
    assert_eq!(
        paired.relay.route.opened_connections(),
        asked,
        "nothing is asked at a Relay the Remote no longer offers"
    );

    paired.workstation.serve_through(&paired.relay, true).await;
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    paired.shutdown().await;
}

/// A laptop that holds no entry for the Relay its Remote comes to Serve
/// through learns of the Relay as it next connects and lists it with the
/// Remote, but never dials it: once the direct way fails the Remote is out of
/// reach, nothing is asked at the Relay, no entry is added for it, and
/// nothing says a login is needed there. Only once the laptop's user adds the
/// Relay does the Remote say a login is needed there, still asking it
/// nothing; and once they log in there the Relay carries the Remote.
#[tokio::test]
async fn a_relay_learned_that_this_server_holds_no_login_at_is_listed_and_never_dialled() {
    let mut paired = PairedDirectly::start("relay-offering-unchosen", false).await;
    let (direct, relayed) = (paired.direct_way(), paired.relay_way());
    paired.workstation.serve_through(&paired.relay, true).await;
    assert_eq!(
        paired.laptop.remote_ways().await,
        std::slice::from_ref(&direct),
        "a Peer not connected is told nothing"
    );

    assert_eq!(paired.probe_why().await, (RemoteStatus::Available, None));
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;

    paired.relay.route.wait_for_connections(1).await;
    let asked = paired.relay.route.opened_connections();
    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Unavailable, None),
        "a Relay the laptop holds no entry for carries nothing, and needs no login of it"
    );
    assert_eq!(
        paired.relay.route.opened_connections(),
        asked,
        "the laptop never connects to a Relay it holds no entry for"
    );
    assert!(
        paired
            .laptop
            .client
            .list_relays()
            .await
            .unwrap()
            .relays
            .is_empty(),
        "learning of a Relay adds no entry for it"
    );
    assert_eq!(paired.laptop.remote_ways().await, [direct, relayed.clone()]);

    paired
        .laptop
        .client
        .add_relay(paired.relay.address())
        .await
        .expect("add the Relay");
    assert_eq!(
        paired.probe_why().await,
        (
            RemoteStatus::Unavailable,
            Some(UnreachableReason::RelayLoginNeeded {
                relay: paired.relay.address()
            })
        ),
        "a Relay the laptop's user chose says a login is needed there"
    );
    assert_eq!(
        paired.relay.route.opened_connections(),
        asked,
        "and is asked nothing until the laptop logs in there"
    );

    paired
        .laptop
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    paired.shutdown().await;
}

/// The telling ends with the Pairing on either side: a Remote its user
/// removes is no longer told anything, its told stream closing though
/// something else open to the Remote holds on, and a Peer the Serving user
/// removes learns nothing more.
#[tokio::test]
async fn the_telling_ends_with_the_pairing_on_either_side() {
    // A connection kept once the request it carried is answered is closed
    // soon after, so what stays open is what the test holds.
    let mut paired = PairedDirectly::with_timings(
        "relay-offering-removed",
        false,
        relay_timings().with_direct_idle_timeout(Duration::from_millis(50)),
    )
    .await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let session = api.begin_session(workspace.path()).await;
    let events = api.session_events(&session).await;
    // The Session's stream, and the one the workstation tells its Relays
    // over.
    paired.direct.wait_for_connections(2).await;

    paired
        .laptop
        .client
        .remove_remote(REMOTE)
        .await
        .expect("remove the Remote");
    paired.direct.wait_for_connections(1).await;
    drop(events);
    paired.direct.wait_for_connections(0).await;

    // Paired again, and then removed as a Peer on the workstation's side.
    let invite = paired
        .workstation
        .invite(vec![Way::Direct(paired.direct.address)])
        .await;
    paired
        .laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair again");
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    paired
        .workstation
        .client
        .remove_peer(&paired.laptop.fingerprint())
        .await
        .expect("remove the laptop as a Peer");
    paired.direct.wait_for_connections(0).await;
    paired.workstation.serve_through(&paired.relay, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Revoked, None),
        "the Peer removed is refused"
    );
    assert_eq!(
        paired.laptop.remote_ways().await,
        [paired.direct_way()],
        "and told nothing more"
    );

    drop(catalog);
    paired.shutdown().await;
}
