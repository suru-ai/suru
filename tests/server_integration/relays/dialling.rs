//! Choosing a way to reach a Remote that offers more than one. Every new dial
//! tries the Remote's direct ways first and starts its Relay ways once a short
//! head start has passed with no direct way answering, or at once where every
//! direct way has failed; whichever answers first carries what is asked, and
//! the others are let go. What a connection already carries stays on it, and
//! the Remote is Unreachable only once every way has failed.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use suru::{
    managed_client::ManagedEvent,
    protocol::{Outlook, RemoteStatus, SessionErrorCode, Way},
    server::ServerTimings,
};
use suru_relay_protocol::{RelayMessage, ServerMessage};
use tokio::time::{Duration, timeout};

use super::{
    RelaySocket, TestRelay, TestServer, error_code, greet, heard,
    pairing::{
        REMOTE, RemoteApi, error_message, next_catalog_event, serving_through,
        serving_through_stand_in, until_recovered, until_recovering,
    },
    relay_timings, scripted_relay, tell,
};
use crate::support::{PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, relay_voice};

/// A workstation Serving at its listener and through a Relay, and a laptop
/// logged in there under the same Account and paired with the workstation by
/// an Invite offering both: the listener, by a route the test can take
/// offline, and then the Relay.
struct PairedBothWays {
    relay: TestRelay,
    workstation: TestServer,
    laptop: TestServer,
    /// The laptop's route to the workstation's listener.
    direct: ObservedTcpProxy,
    /// How many connections the Relay's route had opened once the two were
    /// paired: one from each Server, and those a test's own joins opened.
    unjoined: usize,
}

impl PairedBothWays {
    /// The two Servers so, each running by `timings`.
    async fn start(channel: &str, timings: ServerTimings) -> Self {
        let mut relay = TestRelay::start().await;
        let (workstation, laptop) = serving_through(&relay, channel, timings).await;
        let direct = ObservedTcpProxy::start(workstation.serving_address()).await;
        let invite = workstation
            .invite(vec![
                Way::Direct(direct.address),
                Way::Relay(relay.address()),
            ])
            .await;
        laptop
            .redeem_as(invite, REMOTE)
            .await
            .expect("pair by both ways");
        relay.route.wait_for_connections(2).await;
        let unjoined = relay.route.opened_connections();
        Self {
            relay,
            workstation,
            laptop,
            direct,
            unjoined,
        }
    }

    async fn probe(&self) -> RemoteStatus {
        self.laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote")
            .status
    }

    async fn shutdown(self) {
        self.laptop.shutdown().await;
        self.workstation.shutdown().await;
    }
}

/// A laptop pairs with a workstation at home, by an Invite offering both its
/// listener and the Relay; goes to an office where only the Relay reaches the
/// workstation; and comes home again. It is carried directly while the direct
/// way answers, through the Relay while it does not, and directly again on
/// the next new dial once it answers again — while the stream the Relay
/// carried meanwhile stays where it is — with nothing for its user to do.
#[tokio::test]
async fn a_remote_is_reached_directly_then_through_the_relay_then_directly_again() {
    // So long a head start that only a direct way failing outright, never
    // one slow to answer, lets the Relay carry a dial within the test.
    let mut paired = PairedBothWays::start(
        "relay-dialling-home-office-home",
        relay_timings().with_direct_head_start(Duration::from_secs(60)),
    )
    .await;
    assert_eq!(
        paired.laptop.client.list_remotes().await.unwrap()[0].ways,
        vec![
            Way::Direct(paired.direct.address),
            Way::Relay(paired.relay.address())
        ],
        "the Remote keeps every way its Invite offered"
    );

    // At home: carried directly, with nothing joined at the Relay.
    let remote = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()));
    let mut catalog = remote.subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    remote
        .list_sessions(None)
        .await
        .expect("the Remote answers directly");
    assert!(paired.direct.connections() >= 1);
    assert_eq!(
        paired.relay.route.opened_connections(),
        paired.unjoined,
        "no join is asked at the Relay while the direct way answers"
    );

    // At the office: the direct way fails, and the Relay carries the Remote
    // at once.
    paired.direct.set_online(false).await;
    until_recovering(&mut catalog).await;
    until_recovered(&mut catalog).await;
    assert!(
        paired.relay.route.opened_connections() > paired.unjoined,
        "the Relay carries the Remote once its direct way fails"
    );
    assert_eq!(paired.probe().await, RemoteStatus::Available);
    let joins_ended = paired.relay.joined_connections_logged();
    let relay_connections = paired.relay.route.connections();

    // Home again: the next new dial is carried directly, and the catalog's
    // stream stays on the join that carries it.
    paired.direct.set_online(true).await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let session = RemoteApi::of(&paired.laptop)
        .begin_session(workspace.path())
        .await;
    assert!(
        paired.direct.connections() >= 1,
        "the direct way carried the Session begun, and keeps its connection for the next"
    );
    let told = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::SessionCreated(created)) if created.session_id == session => {
                    return;
                }
                Some(ManagedEvent::Recovering(_)) => {
                    panic!("the catalog's stream was moved off the join carrying it")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await;
    told.expect("the catalog's stream, still through the Relay, tells of the Session begun");
    assert_eq!(
        (
            paired.relay.route.connections(),
            paired.relay.joined_connections_logged()
        ),
        (relay_connections, joins_ended),
        "the join carrying the catalog's stream stands where it was"
    );

    drop(catalog);
    paired.shutdown().await;
}

/// A dial whose direct way neither answers nor fails holds its Relay way
/// back for the head start alone: it is carried through the Relay once that
/// has passed, long before the direct way's own handshake would be given up.
#[tokio::test]
async fn a_direct_way_that_does_not_answer_holds_the_relay_back_for_the_head_start_alone() {
    const HEAD_START: Duration = Duration::from_millis(300);
    let mut paired = PairedBothWays::start(
        "relay-dialling-head-start",
        relay_timings()
            .with_direct_head_start(HEAD_START)
            .with_serving_handshake_timeout(Duration::from_secs(60)),
    )
    .await;

    paired.direct.swallow_connections().await;
    let asked = tokio::time::Instant::now();
    let status = paired.probe().await;
    let took = asked.elapsed();
    assert_eq!(
        status,
        RemoteStatus::Available,
        "the Relay carries the dial"
    );
    assert!(
        took >= HEAD_START,
        "the Relay way waits out the head start: {took:?}"
    );
    assert!(
        took < Duration::from_secs(10),
        "and no longer, the direct way's handshake far from given up: {took:?}"
    );

    paired.shutdown().await;
}

/// A direct connection kept for the next request is closed once it has stood
/// idle for the time it is given, though what else is open to the Remote —
/// its catalog's stream — keeps the Remote in view meanwhile.
#[tokio::test]
async fn a_direct_connection_kept_for_the_next_request_is_closed_once_idle() {
    let mut paired = PairedBothWays::start(
        "relay-dialling-idle-kept",
        relay_timings().with_direct_idle_timeout(Duration::from_millis(200)),
    )
    .await;
    let remote = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()));
    let mut catalog = remote.subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    remote
        .list_sessions(None)
        .await
        .expect("the Remote answers directly");
    // The catalog's stream, and the connection kept from the listing.
    paired.direct.wait_for_connections(2).await;
    paired.direct.wait_for_connections(1).await;

    drop(catalog);
    paired.direct.wait_for_connections(0).await;
    paired.shutdown().await;
}

/// A stand-in Relay that logs every Server in and has a Server that waits
/// there wait, and that holds every join asked of it unanswered for as long
/// as the Server asking holds on, counting the joins asked and those let go.
struct HeldJoins {
    address: String,
    asked: Arc<AtomicUsize>,
    let_go: Arc<AtomicUsize>,
    _answering: tokio::task::JoinHandle<()>,
}

impl HeldJoins {
    async fn start() -> Self {
        let (asked, let_go) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let script = {
            let (asked, let_go) = (asked.clone(), let_go.clone());
            move |mut socket: RelaySocket, relay: String, _: usize| {
                let (asked, let_go) = (asked.clone(), let_go.clone());
                async move {
                    if !greet(&mut socket, &relay).await {
                        return;
                    }
                    match heard(&mut socket).await {
                        Some(ServerMessage::Join { .. }) => {
                            asked.fetch_add(1, Ordering::AcqRel);
                            while heard(&mut socket).await.is_some() {}
                            let_go.fetch_add(1, Ordering::AcqRel);
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
            asked,
            let_go,
            _answering: answering,
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }

    /// Waits until `count` says at least `expected`.
    async fn until(&self, count: &AtomicUsize, expected: usize, what: &str) {
        let reached = timeout(PROGRESS_DEADLINE, async {
            while count.load(Ordering::Acquire) < expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        reached.await.unwrap_or_else(|_| panic!("{what}"));
    }
}

/// Answers `said` as a Relay that logs every Server in under one Account,
/// has a Server that waits there wait, and forgets a Login when asked.
async fn answer_as_a_relay(socket: &mut RelaySocket, said: ServerMessage) {
    match said {
        ServerMessage::BeginLogin { .. } => {
            tell(
                socket,
                &RelayMessage::LoginStarted {
                    verification_uri: "https://login.example.com".to_owned(),
                    user_code: "CODE-HELD".to_owned(),
                    expires_in_seconds: 900,
                },
            )
            .await;
            tell(
                socket,
                &RelayMessage::LoginDone {
                    account: suru_relay_protocol::Account {
                        provider: "scripted".to_owned(),
                        username: "octocat".to_owned(),
                    },
                },
            )
            .await;
        }
        ServerMessage::Wait => {
            tell(socket, &RelayMessage::Waiting).await;
        }
        ServerMessage::Forget => {
            tell(socket, &RelayMessage::Forgotten).await;
        }
        _ => {}
    }
}

/// A dial whose direct way is slow to answer starts its Relay way once the
/// head start has passed; the direct way answering then carries what was
/// asked, and the join asked meanwhile is let go at once, unanswered.
#[tokio::test]
async fn a_join_asked_while_the_direct_way_is_slow_is_let_go_once_it_answers() {
    let joins = HeldJoins::start().await;
    let (workstation, laptop, _) = serving_through_stand_in(
        &joins.address,
        "relay-dialling-held-join",
        relay_timings().with_direct_head_start(Duration::from_millis(300)),
    )
    .await;
    let direct = ObservedTcpProxy::start(workstation.serving_address()).await;
    let invite = workstation
        .invite(vec![
            Way::Direct(direct.address),
            Way::Relay(joins.address.clone()),
        ])
        .await;
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair directly");
    let (asked, let_go) = (joins.asked(), joins.let_go.load(Ordering::Acquire));

    direct.delay(true);
    let releasing = async {
        joins
            .until(
                &joins.asked,
                asked + 1,
                "the Relay way is started once the head start has passed",
            )
            .await;
        direct.delay(false);
    };
    let (probed, ()) = tokio::join!(laptop.client.probe_remote(REMOTE), releasing);
    assert_eq!(
        probed.expect("probe the Remote").status,
        RemoteStatus::Available,
        "the direct way carries the dial once it answers"
    );
    joins
        .until(
            &joins.let_go,
            let_go + 1,
            "the join asked meanwhile is let go once the direct way answers",
        )
        .await;
    assert_eq!(joins.asked(), asked + 1, "one join was asked, and no more");

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A Remote that offers both kinds of way reads Unreachable only once every
/// way has failed: while either answers, the Remote is reached by it.
#[tokio::test]
async fn a_remote_is_unreachable_only_once_every_way_has_failed() {
    let mut paired = PairedBothWays::start("relay-dialling-unreachable", relay_timings()).await;

    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe().await,
        RemoteStatus::Available,
        "the Relay answers while the direct way does not"
    );
    paired.direct.set_online(true).await;
    paired.relay.route.set_online(false).await;
    assert_eq!(
        paired.probe().await,
        RemoteStatus::Available,
        "the direct way answers while the Relay does not"
    );
    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe().await,
        RemoteStatus::Unavailable,
        "only once neither answers is the Remote Unreachable"
    );

    paired.direct.set_online(true).await;
    paired.relay.route.set_online(true).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;
    paired.shutdown().await;
}

/// A stand-in Relay that logs every Server in and has a Server that waits
/// there wait, and that makes every join asked of it by carrying it to the
/// listener `to` names just then, whoever's that is: the address, and where
/// it carries joins.
async fn carrying_joins_to(
    to: std::net::SocketAddr,
) -> (
    String,
    tokio::sync::watch::Sender<std::net::SocketAddr>,
    tokio::task::JoinHandle<()>,
) {
    let carrying_to = tokio::sync::watch::Sender::new(to);
    let script = {
        let carrying_to = carrying_to.clone();
        move |mut socket: RelaySocket, relay: String, _: usize| {
            let to = *carrying_to.borrow();
            async move {
                if !greet(&mut socket, &relay).await {
                    return;
                }
                match heard(&mut socket).await {
                    Some(ServerMessage::Join { .. }) => {
                        if !tell(&mut socket, &RelayMessage::Joined).await {
                            return;
                        }
                        let Ok(mut listener) = tokio::net::TcpStream::connect(to).await else {
                            return;
                        };
                        let mut carried = relay_voice::carried(socket);
                        let _ = tokio::io::copy_bidirectional(&mut carried, &mut listener).await;
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
    (address, carrying_to, answering)
}

/// A way presenting a key other than the Remote's pinned one fails closed,
/// whichever kind of way it is: nothing is asked over it, and the Remote is
/// reached by a way that presents the pinned key, or not at all.
#[tokio::test]
async fn a_way_presenting_another_key_fails_closed_whichever_kind_it_is() {
    let impostor = TestServer::start("relay-dialling-impostor").await;
    impostor.serve().await;
    let (relay, carrying_to, _answering) = carrying_joins_to(impostor.serving_address()).await;
    let (workstation, laptop, _) =
        serving_through_stand_in(&relay, "relay-dialling-other-key", relay_timings()).await;
    let mut direct = ObservedTcpProxy::start(workstation.serving_address()).await;
    let invite = workstation
        .invite(vec![Way::Direct(direct.address), Way::Relay(relay.clone())])
        .await;
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair directly");
    let probe = || async {
        laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote")
            .status
    };

    // The Relay way joins the laptop to another Server.
    direct.set_online(false).await;
    assert_eq!(
        probe().await,
        RemoteStatus::Revoked,
        "a Relay way presenting another key fails closed"
    );

    // The direct way reaches another Server, and the Relay way the pinned
    // one.
    carrying_to.send_replace(workstation.serving_address());
    direct.retarget(impostor.serving_address());
    direct.set_online(true).await;
    assert_eq!(
        probe().await,
        RemoteStatus::Available,
        "a direct way presenting another key fails closed, and the Relay way presenting the \
         pinned key carries the Remote"
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
    impostor.shutdown().await;
}

/// Redeeming an Invite that offers both kinds of way dials them as every new
/// dial does: directly first, however the Invite orders them, so a direct way
/// that answers carries the redemption and nothing is asked at the Relay.
#[tokio::test]
async fn an_invite_offering_both_kinds_is_redeemed_directly_where_a_direct_way_answers() {
    let joins = HeldJoins::start().await;
    let (workstation, laptop, _) = serving_through_stand_in(
        &joins.address,
        "relay-dialling-redeem-direct",
        relay_timings().with_direct_head_start(Duration::from_secs(10)),
    )
    .await;
    let ways = vec![
        Way::Relay(joins.address.clone()),
        Way::Direct(workstation.serving_address()),
    ];
    let invite = workstation.invite(ways.clone()).await;

    let remote = timeout(PROGRESS_DEADLINE, laptop.redeem_as(invite, REMOTE))
        .await
        .expect("the redemption ends in time")
        .expect("the direct way carries the redemption");
    assert_eq!(remote.ways, ways, "the Remote keeps every way, as offered");
    assert_eq!(
        joins.asked(),
        0,
        "no join is asked at the Relay while a direct way answers"
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A Relay way refused for a reason its user can act on — no Login at the
/// Relay — holds up no redemption a direct way carries, though the direct way
/// answers only after the refusal; and where no way carries it, that refusal
/// is what the redemption is refused with, rather than the direct way's
/// failing to answer.
#[tokio::test]
async fn a_refused_relay_way_holds_up_no_redemption_and_is_said_where_no_way_carries_it() {
    let relay = TestRelay::start().await;
    let workstation = TestServer::start("relay-dialling-refused-workstation").await;
    let laptop = TestServer::with_timings(
        "relay-dialling-refused-laptop",
        relay_timings().with_direct_head_start(Duration::from_millis(50)),
    )
    .await;
    workstation.serve().await;
    workstation.log_in(&relay, "583231", "octocat").await;
    workstation.serve_through(&relay, true).await;
    let mut direct = ObservedTcpProxy::start(workstation.serving_address()).await;
    let invite = workstation
        .invite(vec![
            Way::Direct(direct.address),
            Way::Relay(relay.address()),
        ])
        .await;

    direct.set_online(false).await;
    let refused = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("no way carries the redemption");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayLoginNeeded);
    assert!(
        error_message(&refused).contains(&relay.address()),
        "the refusal names the Relay: {refused:#}"
    );

    direct.set_online(true).await;
    direct.delay(true);
    let releasing = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        direct.delay(false);
    };
    let (redeemed, ()) = tokio::join!(laptop.redeem_as(invite, REMOTE), releasing);
    redeemed.expect("the direct way carries the redemption the Relay way was refused");

    laptop.shutdown().await;
    workstation.shutdown().await;
}
