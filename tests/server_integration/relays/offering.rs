//! Peers learning the ways a Serving Server offers: a Serving Server tells
//! each Peer, over the Pairing itself, which Relays it Serves through — as
//! the Peer connects, and again whenever that changes while it is connected
//! — and the Remote entry on the Peer keeps up, taking up a Relay newly
//! offered and dropping one no longer offered, its direct ways staying as its
//! Invite gave them. A Relay learned so that the Peer holds no entry for is
//! listed with the Remote and never dialled.

use suru::{
    managed_client::ManagedEvent,
    protocol::{Outlook, RelayState, RemoteStatus, UnreachableReason, Way},
    server::ServerTimings,
};
use suru_relay_protocol::{Refusal, RelayMessage, ServerMessage};
use tokio::{
    sync::watch,
    time::{Duration, timeout},
};

use super::{
    RelaySocket, TestRelay, TestServer, challenge,
    dialling::answer_as_a_relay,
    heard,
    pairing::{
        REMOTE, RemoteApi, next_catalog_event, serving_through_stand_in, until_recovered,
        until_recovering,
    },
    relay_timings, scripted_relay, tell,
};
use crate::support::{PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, relay_voice};

/// How long, at most, a laptop here goes before storing what its Remote told
/// of the Relays it Serves through.
const STORED_SOON: Duration = Duration::from_millis(10);

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
        Self::with_timings(
            channel,
            logged_in,
            relay_timings().with_told_relays_store_interval(STORED_SOON),
        )
        .await
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
    pub(super) async fn remote_ways(&self) -> Vec<Way> {
        self.client
            .list_remotes()
            .await
            .expect("list the Remotes")
            .into_iter()
            .find(|remote| remote.name == REMOTE)
            .expect("the Remote stands")
            .ways
    }

    /// The ways this Server stores of its Remote `workstation` under `field`:
    /// its `ways`, or those that have `answered`.
    fn stored_ways(&self, field: &str) -> Vec<Way> {
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
        serde_json::from_value(remote[field].clone()).expect("decode the stored ways")
    }

    /// Waits until the ways this Server stores of its Remote `workstation`
    /// under `field` are as `stored` says.
    async fn wait_for_stored_ways(&self, field: &str, stored: impl Fn(&[Way]) -> bool) {
        let storing = timeout(PROGRESS_DEADLINE, async {
            while !stored(&self.stored_ways(field)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        if storing.await.is_err() {
            panic!(
                "the Remote's {field} were never stored as expected; they are stored as {:?}",
                self.stored_ways(field)
            );
        }
    }

    /// Waits until this Server's Remote `workstation` is listed with `ways`.
    pub(super) async fn wait_for_remote_ways(&self, ways: &[Way]) {
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
        paired.laptop.stored_ways("answered").contains(&relayed),
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
    paired
        .laptop
        .wait_for_stored_ways("answered", |answered| !answered.contains(&relayed))
        .await;
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

/// A Login of the workstation's that its Relay comes to refuse — its Account
/// lapsing — withdraws no Relay way: the workstation goes on telling the
/// laptop that it Serves through the Relay, so a laptop that then loses its
/// direct way still holds the Relay way, and once one fresh login from
/// another Server of the Account restores every Login there, the Remote
/// answers through the Relay again with nobody at either Server.
#[tokio::test]
async fn a_login_needing_renewal_withdraws_no_relay_way_and_the_remote_recovers_through_it() {
    let mut paired = PairedDirectly::start("relay-offering-lapse", true).await;
    let (direct, relayed, address) = (
        paired.direct_way(),
        paired.relay_way(),
        paired.relay.address(),
    );
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

    paired.relay.provider.set_admitted("583231", false);
    for server in [&paired.workstation, &paired.laptop] {
        server
            .wait_for_state(&address, RelayState::LoginNeeded)
            .await;
    }
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Available, None),
        "the direct way carries the Remote meanwhile"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        paired.laptop.remote_ways().await,
        [direct, relayed],
        "a Login needing renewal withdraws no Relay way"
    );

    paired.direct.set_online(false).await;
    until_recovering(&mut catalog).await;
    let tablet = TestServer::start("relay-offering-lapse-tablet").await;
    paired.relay.provider.set_admitted("583231", true);
    let login = tablet.log_in(&paired.relay, "583231", "octocat").await;
    assert!(
        matches!(
            login.outcome,
            suru::protocol::RelayLoginOutcome::Done { .. }
        ),
        "{login:?}"
    );
    until_recovered(&mut catalog).await;
    assert_eq!(paired.probe_why().await, (RemoteStatus::Available, None));

    drop(catalog);
    tablet.shutdown().await;
    paired.shutdown().await;
}

/// A stand-in Relay that logs every Server in, has a Server that waits there
/// wait, and carries each join asked there to the listener `to` names just
/// then — until `cut` moves on. While `lapsed` says so it refuses every
/// Login, telling a Server waiting there so at once, as a Relay whose
/// Account lapses does — and it tells it before it cuts any join, which a
/// real Relay may do too.
struct LapsingRelay {
    address: String,
    to: watch::Sender<std::net::SocketAddr>,
    lapsed: watch::Sender<bool>,
    cut: watch::Sender<u64>,
    _answering: tokio::task::JoinHandle<()>,
}

impl LapsingRelay {
    async fn start() -> Self {
        let to = watch::Sender::new(std::net::SocketAddr::from(([127, 0, 0, 1], 9)));
        let lapsed = watch::Sender::new(false);
        let cut = watch::Sender::new(0_u64);
        let script = {
            let (to, lapsed, cut) = (to.clone(), lapsed.clone(), cut.clone());
            move |mut socket: RelaySocket, relay: String, _: usize| {
                let to = *to.borrow();
                let (mut lapsed, mut cut) = (lapsed.subscribe(), cut.subscribe());
                async move {
                    let standing = !*lapsed.borrow_and_update();
                    if !matches!(
                        challenge(&mut socket, &relay).await,
                        Some(ServerMessage::Proof { .. })
                    ) {
                        return;
                    }
                    let login = standing.then(|| suru_relay_protocol::Account {
                        provider: "scripted".to_owned(),
                        username: "octocat".to_owned(),
                    });
                    if !tell(&mut socket, &RelayMessage::Proven { login }).await || !standing {
                        while heard(&mut socket).await.is_some() {}
                        return;
                    }
                    match heard(&mut socket).await {
                        Some(ServerMessage::Wait) => {
                            tell(&mut socket, &RelayMessage::Waiting).await;
                            tokio::select! {
                                _ = async { lapsed.wait_for(|lapsed| *lapsed).await.is_ok() } => {
                                    let refusal = RelayMessage::Refused {
                                        refusal: Refusal::LoginNeeded,
                                        message: "the Account lapsed".to_owned(),
                                    };
                                    tell(&mut socket, &refusal).await;
                                }
                                () = async { while heard(&mut socket).await.is_some() {} } => return,
                            }
                        }
                        Some(ServerMessage::Join { .. }) => {
                            if !tell(&mut socket, &RelayMessage::Joined).await {
                                return;
                            }
                            let Ok(mut listener) = tokio::net::TcpStream::connect(to).await else {
                                return;
                            };
                            cut.borrow_and_update();
                            let mut carried = relay_voice::carried(socket);
                            tokio::select! {
                                _ = tokio::io::copy_bidirectional(&mut carried, &mut listener) => {}
                                _ = cut.changed() => {}
                            }
                            return;
                        }
                        Some(said) => answer_as_a_relay(&mut socket, said).await,
                        None => return,
                    }
                    while heard(&mut socket).await.is_some() {}
                }
            }
        };
        let (address, answering) = scripted_relay(script).await;
        Self {
            address,
            to,
            lapsed,
            cut,
            _answering: answering,
        }
    }
}

/// A Remote reached only through a Relay keeps its way through a lapse of
/// its workstation's Login there that reaches the workstation before the
/// join carrying the Remote is cut: the workstation tells nothing of it over
/// that join, so the laptop still holds the Relay way once the join is cut,
/// and the Remote answers through the Relay again on its own as soon as the
/// Login stands again.
#[tokio::test]
async fn a_lapse_told_before_the_join_is_cut_leaves_a_relay_only_remote_its_way() {
    let relay = LapsingRelay::start().await;
    let (workstation, laptop, invite) =
        serving_through_stand_in(&relay.address, "relay-offering-relay-only", relay_timings())
            .await;
    relay.to.send_replace(workstation.serving_address());
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair through the Relay");
    let relayed = Way::Relay(relay.address.clone());
    let mut catalog = laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    // Told over the join carrying the Remote: the workstation stops Serving
    // through the Relay, and Serves through it again.
    workstation
        .client
        .set_relay_serve_through(&relay.address, false)
        .await
        .unwrap();
    laptop.wait_for_remote_ways(&[]).await;
    workstation
        .client
        .set_relay_serve_through(&relay.address, true)
        .await
        .unwrap();
    laptop
        .wait_for_remote_ways(std::slice::from_ref(&relayed))
        .await;

    relay.lapsed.send_replace(true);
    workstation
        .wait_for_state(&relay.address, RelayState::LoginNeeded)
        .await;
    assert_eq!(
        laptop.client.probe_remote(REMOTE).await.unwrap().status,
        RemoteStatus::Available,
        "the join still carries the Remote"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        laptop.remote_ways().await,
        std::slice::from_ref(&relayed),
        "nothing told over the join withdraws the way"
    );

    relay.cut.send_modify(|cut| *cut += 1);
    until_recovering(&mut catalog).await;
    relay.lapsed.send_replace(false);
    until_recovered(&mut catalog).await;

    drop(catalog);
    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A request a Client asked of a Remote whose Pairing then ended — removed
/// while the Remote could not be told, and another Server paired under the
/// same name — reaches neither: it is refused as asked of a Remote no longer
/// paired, and what is asked of the Remote paired in its place reaches that
/// Remote alone.
#[tokio::test]
async fn a_request_asked_of_a_pairing_since_ended_reaches_neither_it_nor_the_one_paired_after() {
    let mut paired = PairedDirectly::start("relay-offering-paired-again", false).await;
    let api = RemoteApi::of(&paired.laptop);
    let mut asked = api.withholding("/v1/session-events").await;

    paired.direct.set_online(false).await;
    let removal = paired
        .laptop
        .client
        .remove_remote(REMOTE)
        .await
        .expect("remove the Remote");
    assert!(
        !removal.acknowledged,
        "the workstation holds the laptop as its Peer still"
    );
    paired.direct.set_online(true).await;
    let elsewhere = TestServer::start("relay-offering-paired-again-elsewhere").await;
    elsewhere.serve().await;
    let invite = elsewhere
        .invite(vec![Way::Direct(elsewhere.serving_address())])
        .await;
    paired
        .laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair another Server under the same name");

    assert_eq!(
        RemoteApi::finish_withheld(&mut asked).await,
        404,
        "what was asked of the Pairing since ended is refused"
    );
    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Available, None),
        "the Remote paired in its place answers"
    );
    let health = api.health().await.expect("ask the Remote for its health");
    assert_eq!(
        health.instance_id,
        elsewhere.server.as_ref().unwrap().descriptor().instance_id,
        "and is the one that answers"
    );

    drop(asked);
    elsewhere.shutdown().await;
    paired.shutdown().await;
}

/// What a Remote tells of the Relays it Serves through is stored no more
/// often than the store interval allows, however often it changes: the
/// Remote keeps up with each change at once, and the latest of them is
/// stored once the interval passes, or as the Server stops.
#[tokio::test]
async fn what_a_remote_tells_is_stored_no_more_often_than_the_store_interval() {
    let mut paired = PairedDirectly::with_timings(
        "relay-offering-stored",
        true,
        relay_timings().with_told_relays_store_interval(Duration::from_secs(60)),
    )
    .await;
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

    for _ in 0..3 {
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
    }
    paired.workstation.serve_through(&paired.relay, true).await;
    paired
        .laptop
        .wait_for_remote_ways(&[direct.clone(), relayed.clone()])
        .await;
    assert_eq!(
        paired.laptop.stored_ways("ways"),
        std::slice::from_ref(&direct),
        "nothing told is stored before the interval passes"
    );

    drop(catalog);
    paired.laptop.restart().await;
    assert_eq!(
        paired.laptop.remote_ways().await,
        [direct, relayed],
        "the latest told is stored as the Server stops"
    );

    paired.shutdown().await;
}
