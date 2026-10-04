//! Choosing a way to reach a Remote that offers more than one. Every new dial
//! tries the Remote's direct ways first and starts its Relay ways once a short
//! head start has passed with no direct way answering, or at once where every
//! direct way has failed; whichever answers first carries what is asked, and
//! the others are let go. What a connection already carries stays on it, and
//! the Remote is Unreachable only once every way has failed.

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AttachmentDescriptor, Outlook, RelayState, RemoteStatus, SessionErrorCode,
        UnreachableReason, Way,
    },
    server::ServerTimings,
};
use suru_relay_protocol::{Refusal, RelayMessage, ServerMessage};
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
        Self::through(TestRelay::start().await, channel, timings).await
    }

    /// The two Servers so, with `relay` for their Relay.
    async fn through(mut relay: TestRelay, channel: &str, timings: ServerTimings) -> Self {
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

    /// How the Remote stands as a probe finds it, and why where it is
    /// Unavailable for a reason its user can act on.
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

/// A laptop pairs with a workstation at home, by an Invite offering both its
/// listener and the Relay; goes to an office where only the Relay reaches the
/// workstation; and comes home again. It is carried directly while the direct
/// way answers, through the Relay while it does not, and directly again once
/// a try of the direct way in the background finds it answering — while the
/// stream the Relay carried meanwhile stays where it is — with nothing for
/// its user to do.
#[tokio::test]
async fn a_remote_is_reached_directly_then_through_the_relay_then_directly_again() {
    // So long a head start that only a direct way failing outright, never
    // one slow to answer, lets the Relay carry a dial within the test.
    let mut paired = PairedBothWays::start(
        "relay-dialling-home-office-home",
        relay_timings()
            .with_direct_head_start(Duration::from_secs(60))
            .with_direct_retry_interval(Duration::from_millis(50)),
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

    // Home again: what is asked rides the join standing until a try of the
    // direct way in the background finds it answering, and is carried
    // directly from then on — as an Attachment's bytes all coming back by
    // the direct way show — while the catalog's stream stays on its join.
    paired.direct.set_online(true).await;
    const ATTACHMENT: usize = 256 * 1024;
    let api = RemoteApi::of(&paired.laptop);
    let attachment = api
        .post("/v1/attachments")
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(crate::padded_png(ATTACHMENT))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("upload an Attachment")
        .json::<AttachmentDescriptor>()
        .await
        .expect("decode the Remote's Attachment");
    let directly = timeout(PROGRESS_DEADLINE, async {
        loop {
            let answered = paired.direct.answered_bytes();
            let fetched = api
                .get(&format!("/v1/attachments/{}", attachment.id))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .expect("fetch the Attachment")
                .bytes()
                .await
                .expect("read the Attachment");
            assert_eq!(fetched.len(), ATTACHMENT);
            if paired.direct.answered_bytes() - answered >= ATTACHMENT as u64 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    directly
        .await
        .expect("the Remote is carried directly again, with nothing for its user to do");
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let session = api.begin_session(workspace.path()).await;
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

/// Once a joined stream stands, what is asked of the Remote rides it at once
/// while the direct way neither answers nor fails: the head start is waited
/// out only where a fresh connection is needed.
#[tokio::test]
async fn a_standing_joined_stream_carries_what_is_asked_at_once_while_the_direct_way_is_silent() {
    // So long a head start that waiting it out even once fails the test.
    let mut paired = PairedBothWays::start(
        "relay-dialling-standing-join",
        relay_timings()
            .with_direct_head_start(Duration::from_secs(20))
            .with_serving_handshake_timeout(Duration::from_secs(60)),
    )
    .await;
    // The joined stream comes to stand while the direct way fails outright.
    paired.direct.set_online(false).await;
    let remote = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()));
    let mut catalog = remote.subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    // Then the direct way stops answering at all.
    paired.direct.swallow_connections().await;
    let asked = tokio::time::Instant::now();
    for _ in 0..3 {
        remote
            .list_sessions(None)
            .await
            .expect("the Remote answers over the joined stream");
    }
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "nothing asked waits out the head start: {:?}",
        asked.elapsed()
    );

    drop(catalog);
    paired.shutdown().await;
}

/// While what is asked rides a joined stream, the Remote's direct ways are
/// tried again in the background, never more often than the gap they are
/// given however much is asked. The tries hold no interest in the Remote of
/// their own: once nothing else is asked of it, its joined stream ends,
/// though a try is under way, and no direct way is tried again.
#[tokio::test]
async fn the_direct_ways_are_tried_again_in_the_background_at_a_pace_and_only_while_asked() {
    const GAP: Duration = Duration::from_millis(300);
    let mut paired = PairedBothWays::start(
        "relay-dialling-background-tries",
        relay_timings()
            .with_direct_retry_interval(GAP)
            .with_serving_handshake_timeout(Duration::from_secs(60)),
    )
    .await;
    paired.direct.set_online(false).await;
    let remote = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()));
    let mut catalog = remote.subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    let dialled = paired.direct.opened_connections();
    let (began, mut asked) = (tokio::time::Instant::now(), 0_usize);
    while began.elapsed() < GAP * 5 {
        remote
            .list_sessions(None)
            .await
            .expect("the Remote answers over the joined stream");
        asked += 1;
    }
    let tries = paired.direct.opened_connections() - dialled;
    let paced = usize::try_from(began.elapsed().as_millis() / GAP.as_millis()).unwrap() + 1;
    assert!(
        tries >= 1,
        "the direct way is tried again in the background"
    );
    assert!(
        tries <= paced && tries < asked,
        "{tries} tries of the direct way for {asked} requests in {:?}",
        began.elapsed()
    );

    // A try under way as the last interest ends holds nothing open.
    paired.direct.swallow_connections().await;
    tokio::time::sleep(GAP).await;
    let dialled = paired.direct.opened_connections();
    remote
        .list_sessions(None)
        .await
        .expect("the Remote answers over the joined stream");
    paired.direct.wait_for_opened_connections(dialled + 1).await;
    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;
    let dialled = paired.direct.opened_connections();
    tokio::time::sleep(GAP * 3).await;
    assert_eq!(
        paired.direct.opened_connections(),
        dialled,
        "no direct way is tried once nothing is asked of the Remote"
    );

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
}

/// Waits until `count` says at least `expected`, as `what` says it will.
async fn until(count: &AtomicUsize, expected: usize, what: &str) {
    let reached = timeout(PROGRESS_DEADLINE, async {
        while count.load(Ordering::Acquire) < expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    reached.await.unwrap_or_else(|_| panic!("{what}"));
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
        until(
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
    until(
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

/// A Relay way refused for its cap on the Account is said of a Remote that
/// offers both kinds of way only once its direct way fails as well: while the
/// direct way answers, the Remote is reached by it, and nothing is said of the
/// cap.
#[tokio::test]
async fn a_relays_cap_is_said_of_a_remote_only_once_its_direct_way_fails_too() {
    let relay =
        TestRelay::configured(|config| config.with_joined_connections_per_account(NonZeroU32::MIN))
            .await;
    let mut paired = PairedBothWays::through(relay, "relay-dialling-cap", relay_timings()).await;
    let held = paired
        .relay
        .voice()
        .holding_a_join(&paired.relay.provider, "583231", "octocat")
        .await;

    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Available, None),
        "the direct way answers, and nothing is said of the cap"
    );
    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe_why().await,
        (
            RemoteStatus::Unavailable,
            Some(UnreachableReason::RelayCapReached {
                relay: paired.relay.address(),
                limit: 1,
            })
        ),
        "with the direct way failing too, the cap is why the Remote is out of reach"
    );

    drop(held);
    paired.direct.set_online(true).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;
    paired.shutdown().await;
}

/// A login needed at the Relay is said of a Remote that offers both kinds of
/// way only once its direct way fails as well: while the direct way answers,
/// the Remote is reached by it and nothing is said of a login, and the
/// Remote is tried again all the same while it is out of reach, so it
/// answers by its direct way as soon as that does.
#[tokio::test]
async fn a_login_needed_at_a_relay_is_said_of_a_remote_only_once_its_direct_way_fails_too() {
    let mut paired = PairedBothWays::start("relay-dialling-login", relay_timings()).await;
    let address = paired.relay.address();
    paired.relay.provider.set_admitted("583231", false);
    paired
        .laptop
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;

    assert_eq!(
        paired.probe_why().await,
        (RemoteStatus::Available, None),
        "the direct way answers, and nothing is said of a login"
    );
    paired.direct.set_online(false).await;
    assert_eq!(
        paired.probe_why().await,
        (
            RemoteStatus::Unavailable,
            Some(UnreachableReason::RelayLoginNeeded { relay: address })
        ),
        "with the direct way failing too, a login is why the Remote is out of reach"
    );

    paired.direct.set_online(true).await;
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

/// A stand-in Relay that logs every Server in and has a Server that waits
/// there wait, and that refuses every join asked of it as needing a fresh
/// login, counting the joins it refused: the address, and the count.
async fn refusing_joins() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let refused = Arc::new(AtomicUsize::new(0));
    let script = {
        let refused = refused.clone();
        move |mut socket: RelaySocket, relay: String, _: usize| {
            let refused = refused.clone();
            async move {
                if !greet(&mut socket, &relay).await {
                    return;
                }
                match heard(&mut socket).await {
                    Some(ServerMessage::Join { .. }) => {
                        let refusal = RelayMessage::Refused {
                            refusal: Refusal::LoginNeeded,
                            message: "log in again".to_owned(),
                        };
                        tell(&mut socket, &refusal).await;
                        refused.fetch_add(1, Ordering::AcqRel);
                    }
                    Some(said) => answer_as_a_relay(&mut socket, said).await,
                    None => return,
                }
                while heard(&mut socket).await.is_some() {}
            }
        }
    };
    let (address, answering) = scripted_relay(script).await;
    (address, refused, answering)
}

/// A Relay way refused for a reason its user can act on — a Login the Relay
/// no longer admits — holds up no redemption a direct way carries, though the
/// direct way answers only once the Relay has refused; and where no way
/// carries it, that refusal is what the redemption is refused with, rather
/// than the direct way's failing to answer.
#[tokio::test]
async fn a_refused_relay_way_holds_up_no_redemption_and_is_said_where_no_way_carries_it() {
    let (relay, refused, _answering) = refusing_joins().await;
    let (workstation, laptop, _) = serving_through_stand_in(
        &relay,
        "relay-dialling-refused",
        relay_timings().with_direct_head_start(Duration::from_millis(50)),
    )
    .await;
    let mut direct = ObservedTcpProxy::start(workstation.serving_address()).await;
    let invite = workstation
        .invite(vec![Way::Direct(direct.address), Way::Relay(relay.clone())])
        .await;

    direct.set_online(false).await;
    let refusal = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("no way carries the redemption");
    assert_eq!(error_code(&refusal), SessionErrorCode::RelayLoginNeeded);
    assert!(
        error_message(&refusal).contains(&relay),
        "the refusal names the Relay: {refusal:#}"
    );

    direct.set_online(true).await;
    direct.delay(true);
    let refusals = refused.load(Ordering::Acquire);
    let releasing = async {
        until(
            &refused,
            refusals + 1,
            "the Relay way is started, and refused, while the direct way is slow",
        )
        .await;
        direct.delay(false);
    };
    let (redeemed, ()) = tokio::join!(laptop.redeem_as(invite, REMOTE), releasing);
    redeemed.expect("the direct way carries the redemption the Relay way was refused");

    laptop.shutdown().await;
    workstation.shutdown().await;
}
