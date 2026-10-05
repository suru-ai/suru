//! Pairing through a Relay: an Invite offering a Relay its Serving Server
//! Serves through, redeemed there by a Server logged in under the same
//! Account, and the Remote it pairs with reached through the Relay from then
//! on by keys alone, everything it offers working as it does directly
//! (ADR-0045, ADR-0046).

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use eventsource_stream::Eventsource as _;
use futures_util::{SinkExt, StreamExt};
use rcgen::PublicKeyData;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AdmitPromptRequest, AttachmentDescriptor, CreateSessionRequest, Health, InitialPrompt,
        IssueInviteRequest, Outlook, PROTOCOL_VERSION, PromptDelivery, PromptId,
        RedeemInviteRequest, RelayState, Remote, RemoteHealth, RemoteStatus,
        SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SessionChange, SessionError,
        SessionErrorCode, SessionSnapshot, SessionUpdate, SettingMutation, UnreachableReason, Way,
    },
    server::ServerTimings,
};
use suru_relay::RelayConfig;
use suru_relay_protocol::{Refusal, RelayMessage, ServerMessage};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};
use tokio_tungstenite::tungstenite::Message;

use super::{
    RelaySocket, TestRelay, TestServer, error_code, greet, heard, relay_timings, scripted_relay,
    serving::{Multiplexed, nothing_listens_at, paired_tls},
    tell,
};
use crate::support::PROGRESS_DEADLINE;

/// The name the laptop knows the workstation by.
pub(super) const REMOTE: &str = "workstation";

impl TestServer {
    /// An Invite to this Server offering `ways`.
    pub(super) async fn invite(&self, ways: Vec<Way>) -> String {
        self.client
            .issue_invite(IssueInviteRequest { ways })
            .await
            .expect("issue an Invite")
            .invite
    }

    /// Redeems `invite`, naming the Remote `name`.
    pub(super) async fn redeem_as(&self, invite: String, name: &str) -> anyhow::Result<Remote> {
        self.client
            .redeem_invite(RedeemInviteRequest {
                invite,
                name: Some(name.to_owned()),
                ways: Vec::new(),
            })
            .await
    }

    /// Waits until this Server waits at `relay` to be reached, as `asker`,
    /// logged in there under the same Account, finds by being joined to it
    /// there: Serving through a Relay is chosen at once, and waited on as the
    /// Server next connects there.
    async fn waiting_at(&self, relay: &TestRelay, asker: &TestServer) {
        relay
            .voice()
            .joined(
                &asker.identity(),
                &self.identity().subject_public_key_info(),
            )
            .await;
    }

    /// Probes the Remote `workstation` until it stands as `status`.
    pub(super) async fn wait_for_remote(&self, status: RemoteStatus) -> RemoteHealth {
        let probing = timeout(PROGRESS_DEADLINE, async {
            loop {
                let health = self
                    .client
                    .probe_remote(REMOTE)
                    .await
                    .expect("probe the Remote");
                if health.status == status {
                    return health;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        probing.unwrap_or_else(|_| panic!("the Remote never came to stand {status:?}"))
    }
}

/// What a refusal says to a reader.
pub(super) fn error_message(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<suru::protocol::SessionError>()
        .unwrap_or_else(|| panic!("a typed Session error, not {error:#}"))
        .message
        .clone()
}

/// Why a refusal says the Remote could not be reached, where its user can do
/// something about it — what a Client acts on, rather than the words.
fn error_reason(error: &anyhow::Error) -> Option<UnreachableReason> {
    error
        .downcast_ref::<SessionError>()
        .unwrap_or_else(|| panic!("a typed Session error, not {error:#}"))
        .unreachable
        .clone()
}

/// A workstation Serving through a Relay, and a laptop paired with it by an
/// Invite offering that Relay alone, both logged in there under one Account.
struct PairedThrough {
    relay: TestRelay,
    workstation: TestServer,
    laptop: TestServer,
}

impl PairedThrough {
    async fn start(channel: &str) -> Self {
        Self::with_timings(channel, relay_timings()).await
    }

    /// Two Servers paired so, each running by `timings`.
    async fn with_timings(channel: &str, timings: ServerTimings) -> Self {
        let relay = TestRelay::start().await;
        let (workstation, laptop) = serving_through(&relay, channel, timings).await;
        let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;
        laptop
            .redeem_as(invite, REMOTE)
            .await
            .expect("pair through the Relay");
        Self {
            relay,
            workstation,
            laptop,
        }
    }

    async fn shutdown(self) {
        self.laptop.shutdown().await;
        self.workstation.shutdown().await;
    }
}

/// A workstation Serving through `relay`, and a laptop, both running by
/// `timings` and logged in there under one Account, and paired with nothing
/// yet.
pub(super) async fn serving_through(
    relay: &TestRelay,
    channel: &str,
    timings: ServerTimings,
) -> (TestServer, TestServer) {
    let workstation =
        TestServer::with_timings(&format!("{channel}-workstation"), timings.clone()).await;
    let laptop = TestServer::with_timings(&format!("{channel}-laptop"), timings).await;
    workstation.serve().await;
    for server in [&workstation, &laptop] {
        server.log_in(relay, "583231", "octocat").await;
    }
    workstation.serve_through(relay, true).await;
    workstation.waiting_at(relay, &laptop).await;
    (workstation, laptop)
}

/// The Session catalog's next event, skipping the Model Catalog's.
pub(super) async fn next_catalog_event(
    catalog: &mut suru::managed_client::SessionCatalogSubscription,
) -> Option<ManagedEvent> {
    timeout(
        PROGRESS_DEADLINE,
        crate::next_session_catalog_event(catalog),
    )
    .await
    .expect("the Remote's catalog says something in time")
}

#[tokio::test]
async fn a_relay_the_server_serves_through_is_offered_by_an_invite_and_redeemed_into_a_pairing() {
    let mut relay = TestRelay::start().await;
    let workstation = TestServer::start("relay-pairing-workstation").await;
    let laptop = TestServer::start("relay-pairing-laptop").await;
    workstation.serve().await;
    for server in [&workstation, &laptop] {
        server.log_in(&relay, "583231", "octocat").await;
    }
    let through = Way::Relay(relay.address());

    let refused = workstation
        .client
        .issue_invite(IssueInviteRequest {
            ways: vec![through.clone()],
        })
        .await
        .expect_err("a Relay the Server does not Serve through is no way an Invite offers");
    assert_eq!(error_code(&refused), SessionErrorCode::InvalidInviteWays);
    assert!(
        error_message(&refused).contains(&relay.address()),
        "{refused:#}"
    );

    workstation.serve_through(&relay, true).await;
    workstation.waiting_at(&relay, &laptop).await;
    let issued = workstation
        .client
        .issue_invite(IssueInviteRequest {
            ways: vec![Way::Relay(format!("{}/", relay.address().to_uppercase()))],
        })
        .await
        .expect("a Relay the Server Serves through is a way an Invite offers");
    assert_eq!(
        issued.ways,
        vec![through.clone()],
        "the Invite offers the Relay by the address it is known by"
    );
    let preview = laptop
        .client
        .preview_invite(issued.invite.clone())
        .await
        .expect("preview the Invite");
    assert_eq!(preview.ways, vec![through.clone()]);
    assert_eq!(preview.fingerprint, workstation.fingerprint());

    relay.route.wait_for_connections(2).await;
    let opened = relay.route.opened_connections();
    let remote = laptop
        .redeem_as(issued.invite, REMOTE)
        .await
        .expect("redeem the Invite through the Relay");
    assert!(
        relay.route.opened_connections() > opened,
        "the redemption went through the Relay"
    );
    assert_eq!(remote.ways, vec![through]);
    assert_eq!(
        remote.fingerprint,
        workstation.fingerprint(),
        "the laptop pins the workstation's key"
    );
    let peers = workstation.client.list_peers().await.unwrap();
    assert_eq!(
        peers
            .iter()
            .map(|peer| peer.fingerprint.as_str())
            .collect::<Vec<_>>(),
        [laptop.fingerprint()],
        "the workstation pins the laptop's key"
    );
    let health = laptop
        .client
        .probe_remote(REMOTE)
        .await
        .expect("reach the Remote through the Relay");
    assert_eq!(health.status, RemoteStatus::Available);
    assert_eq!(health.protocol_version, Some(PROTOCOL_VERSION));

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A Server Serving through a Relay with its listener off opens no port at
/// all: an Invite offers none of its own addresses, and it is paired with
/// and reached through the Relay alone, as one listening too would be. With
/// Serving off, nothing reaches it by any way.
#[tokio::test]
async fn a_server_serving_with_its_listener_off_is_paired_and_reached_through_its_relay_alone() {
    let relay = TestRelay::start().await;
    let workstation = TestServer::start("relay-listener-off-workstation").await;
    let laptop = TestServer::start("relay-listener-off-laptop").await;
    // Where the listener would listen were it on: a port nothing holds.
    let unheld = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .expect("find a port nothing holds");
    for mutation in [
        SettingMutation::ServingListener { value: Some(false) },
        SettingMutation::ServingPort {
            value: Some(unheld.port()),
        },
        SettingMutation::ServingBindAddress {
            value: Some(unheld.ip()),
        },
        SettingMutation::ServingEnabled { value: Some(true) },
    ] {
        workstation
            .client
            .mutate_setting(mutation)
            .await
            .expect("Serve with the listener off");
    }
    assert_eq!(workstation.listening_at(), None);
    nothing_listens_at(unheld).await;
    for server in [&workstation, &laptop] {
        server.log_in(&relay, "583231", "octocat").await;
    }
    workstation.serve_through(&relay, true).await;
    workstation.waiting_at(&relay, &laptop).await;

    let through = Way::Relay(relay.address());
    let refused = workstation
        .client
        .issue_invite(IssueInviteRequest {
            ways: vec![Way::Direct(unheld), through.clone()],
        })
        .await
        .expect_err("an Invite offers no address of a Server that listens at none");
    assert_eq!(error_code(&refused), SessionErrorCode::InvalidInviteWays);
    assert!(
        error_message(&refused).contains(&format!(
            "the Serving listener is off, so an Invite offers none of this Server's own \
             addresses, {unheld} among them"
        )),
        "{refused:#}"
    );
    let invite = workstation.invite(vec![through.clone()]).await;
    let remote = laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair through the Relay with the listener off");
    assert_eq!(remote.ways, vec![through]);
    assert_eq!(remote.fingerprint, workstation.fingerprint());
    let health = laptop
        .client
        .probe_remote(REMOTE)
        .await
        .expect("reach the Remote through the Relay");
    assert_eq!(health.status, RemoteStatus::Available);
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    RemoteApi::of(&laptop).begin_session(workspace.path()).await;
    assert_eq!(workstation.listening_at(), None);
    nothing_listens_at(unheld).await;

    workstation.stop_serving().await;
    relay
        .voice()
        .no_longer_waiting(
            &laptop.identity(),
            &workstation.identity().subject_public_key_info(),
        )
        .await;
    laptop.wait_for_remote(RemoteStatus::Unavailable).await;
    nothing_listens_at(unheld).await;

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn redeeming_through_a_relay_this_server_holds_no_login_at_is_refused_naming_the_relay() {
    let relay = TestRelay::start().await;
    let workstation = TestServer::start("relay-pairing-no-login-workstation").await;
    let laptop = TestServer::start("relay-pairing-no-login-laptop").await;
    workstation.serve().await;
    workstation.log_in(&relay, "583231", "octocat").await;
    workstation.serve_through(&relay, true).await;
    let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;

    let unknown = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("a Relay the laptop holds no entry for");
    laptop.client.add_relay(relay.address()).await.unwrap();
    let not_logged_in = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("a Relay the laptop has not logged in at");
    let needed = Some(UnreachableReason::RelayLoginNeeded {
        relay: relay.address(),
    });
    for refused in [unknown, not_logged_in] {
        assert_eq!(error_code(&refused), SessionErrorCode::RelayLoginNeeded);
        assert!(
            error_message(&refused).contains(&relay.address()),
            "the refusal names the Relay: {refused:#}"
        );
        assert_eq!(
            error_reason(&refused),
            needed,
            "and says which Relay to log in at in a reason a Client can lead its user by"
        );
    }
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());
    assert!(workstation.client.list_peers().await.unwrap().is_empty());

    laptop.log_in(&relay, "583231", "octocat").await;
    relay.voice().forget(&laptop.identity()).await;
    let forgotten = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("a Login the Relay no longer holds");
    assert_eq!(error_code(&forgotten), SessionErrorCode::RelayLoginNeeded);
    assert!(
        error_message(&forgotten).contains(&relay.address()),
        "{forgotten:#}"
    );
    assert_eq!(error_reason(&forgotten), needed);

    laptop.log_in(&relay, "583231", "octocat").await;
    workstation.waiting_at(&relay, &laptop).await;
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("the Invite refused for want of a Login is redeemed once there is one");

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn redeeming_through_a_relay_under_another_account_is_refused_saying_what_to_do() {
    let relay = TestRelay::start().await;
    let workstation = TestServer::start("relay-pairing-accounts-workstation").await;
    let laptop = TestServer::start("relay-pairing-accounts-laptop").await;
    workstation.serve().await;
    workstation.log_in(&relay, "583231", "octocat").await;
    laptop.log_in(&relay, "99", "someone-else").await;
    workstation.serve_through(&relay, true).await;
    let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;

    let refused = laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect_err("the Relay joins no Servers of two Accounts");
    assert_eq!(
        error_code(&refused),
        SessionErrorCode::RelayDifferentAccounts
    );
    let message = error_message(&refused);
    assert!(
        message.contains(&relay.address())
            && message.contains("someone-else")
            && message.contains("log this Server in"),
        "the refusal says where, as whom, and what to do: {message}"
    );
    assert_eq!(
        error_reason(&refused),
        None,
        "logging in again where this Server already stands is no way past it"
    );
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());
    assert!(workstation.client.list_peers().await.unwrap().is_empty());

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A Relay joining one connection at once for each Account.
fn joining_one_at_once(config: RelayConfig) -> RelayConfig {
    config.with_joined_connections_per_account(NonZeroU32::MIN)
}

/// Whether `message` says the Relay at `relay` would join no more
/// connections for the Account, naming the cap and who can raise it.
fn names_the_cap_on_joined_connections(message: &str, relay: &TestRelay) -> bool {
    message.contains(&relay.address())
        && message.contains("1 connection joined at once")
        && message.contains("operator")
}

#[tokio::test]
async fn redeeming_through_a_relay_whose_account_has_its_joined_connections_names_the_cap() {
    let relay = TestRelay::configured(joining_one_at_once).await;
    let (workstation, laptop) =
        serving_through(&relay, "relay-pairing-joins-cap", relay_timings()).await;
    let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;
    let held = relay
        .voice()
        .holding_a_join(&relay.provider, "583231", "octocat")
        .await;

    let refused = laptop
        .redeem_as(invite.clone(), REMOTE)
        .await
        .expect_err("the Relay joins no more for the Account");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayCapReached);
    let message = error_message(&refused);
    assert!(
        names_the_cap_on_joined_connections(&message, &relay),
        "{message}"
    );
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());

    // Once the join holding the place ends, the Invite is redeemed.
    drop(held);
    timeout(PROGRESS_DEADLINE, async {
        loop {
            match laptop.redeem_as(invite.clone(), REMOTE).await {
                Ok(_) => return,
                Err(refused) if error_code(&refused) == SessionErrorCode::RelayCapReached => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(refused) => panic!("the redemption failed: {refused:#}"),
            }
        }
    })
    .await
    .expect("the place is given back as the join ends");

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// A Remote reached only through a Relay that refuses the Login its Server
/// holds there reads Unreachable like any other no way reaches — answering
/// as one does, so it is tried again on the same schedule — saying a login is
/// needed at that Relay: on the health a probe answers, on anything carried
/// to it, and to a Client keeping it in view. Once the Login stands again it
/// answers, and nothing more is said of a login.
#[tokio::test]
async fn a_remote_out_of_reach_for_want_of_a_login_reads_unreachable_saying_a_login_is_needed() {
    let paired = PairedThrough::start("relay-pairing-login-needed").await;
    let address = paired.relay.address();
    let needed = Some(UnreachableReason::RelayLoginNeeded {
        relay: address.clone(),
    });
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    paired.relay.provider.set_admitted("583231", false);
    paired
        .laptop
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;
    assert_eq!(
        paired
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote"),
        RemoteHealth {
            protocol_version: None,
            status: RemoteStatus::Unavailable,
            unreachable: needed.clone(),
        }
    );
    let answer = RemoteApi::of(&paired.laptop)
        .get("/health")
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), reqwest::StatusCode::BAD_GATEWAY);
    let error = answer.json::<SessionError>().await.unwrap();
    assert_eq!(
        (error.code, &error.unreachable),
        (SessionErrorCode::PairingConnectionFailed, &needed),
        "Unreachable like any Remote no way reaches, saying why"
    );
    let recovering = timeout(PROGRESS_DEADLINE, async {
        loop {
            match next_catalog_event(&mut catalog).await {
                Some(ManagedEvent::Recovering(status)) if status.unreachable.is_some() => {
                    return status;
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await
    .expect("the Remote's catalog recovers saying why");
    assert_eq!(recovering.unreachable, needed);

    paired.relay.provider.set_admitted("583231", true);
    paired
        .laptop
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    recovered(&mut catalog).await;
    assert_eq!(
        paired
            .laptop
            .wait_for_remote(RemoteStatus::Available)
            .await
            .unreachable,
        None
    );

    drop(catalog);
    paired.shutdown().await;
}

/// A Remote offering two Relays, each refusing it for a reason of its own,
/// is said to need a login where one of them needs it, whichever is dialled
/// first: a login is the user's own to do, where a cap is the operator's to
/// raise.
#[tokio::test]
async fn a_login_needed_at_one_relay_is_said_over_a_cap_reached_at_another_dialled_first() {
    let capped = TestRelay::configured(joining_one_at_once).await;
    let lapsing = TestRelay::start().await;
    let (workstation, laptop) =
        serving_through(&capped, "relay-pairing-two-refusals", relay_timings()).await;
    for server in [&workstation, &laptop] {
        server.log_in(&lapsing, "583231", "octocat").await;
    }
    workstation.serve_through(&lapsing, true).await;
    workstation.waiting_at(&lapsing, &laptop).await;
    let invite = workstation
        .invite(vec![
            Way::Relay(capped.address()),
            Way::Relay(lapsing.address()),
        ])
        .await;
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair through both Relays");
    let held = capped
        .voice()
        .holding_a_join(&capped.provider, "583231", "octocat")
        .await;
    lapsing.provider.set_admitted("583231", false);
    laptop
        .wait_for_state(&lapsing.address(), RelayState::LoginNeeded)
        .await;

    let health = laptop
        .client
        .probe_remote(REMOTE)
        .await
        .expect("probe the Remote");
    assert_eq!(
        (health.status, health.unreachable),
        (
            RemoteStatus::Unavailable,
            Some(UnreachableReason::RelayLoginNeeded {
                relay: lapsing.address(),
            })
        )
    );

    drop(held);
    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_remote_its_relay_joins_nothing_more_for_reads_unreachable_naming_the_cap() {
    let relay = TestRelay::configured(joining_one_at_once).await;
    let (workstation, laptop) =
        serving_through(&relay, "relay-pairing-remote-joins-cap", relay_timings()).await;
    let invite = workstation.invite(vec![Way::Relay(relay.address())]).await;
    laptop
        .redeem_as(invite, REMOTE)
        .await
        .expect("pair through the Relay");
    let held = relay
        .voice()
        .holding_a_join(&relay.provider, "583231", "octocat")
        .await;
    let capped = Some(UnreachableReason::RelayCapReached {
        relay: relay.address(),
        limit: 1,
    });

    // Probing the Remote, as the Remote picker does, finds it Unavailable
    // and says why, and the Remote is remembered as Unavailable.
    assert_eq!(
        laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote"),
        RemoteHealth {
            protocol_version: None,
            status: RemoteStatus::Unavailable,
            unreachable: capped.clone(),
        }
    );
    assert_eq!(
        laptop.client.list_remotes().await.unwrap()[0].status,
        RemoteStatus::Unavailable
    );
    // So does whatever a Client asks of the Remote meanwhile, failing as a
    // Remote that cannot be reached fails, so it is tried again later.
    let answer = RemoteApi::of(&laptop).get("/health").send().await.unwrap();
    assert!(answer.status().is_server_error(), "{}", answer.status());
    let error = answer.json::<SessionError>().await.unwrap();
    assert_eq!(
        (error.code, &error.unreachable),
        (SessionErrorCode::RelayCapReached, &capped)
    );
    assert!(
        names_the_cap_on_joined_connections(&error.message, &relay),
        "{}",
        error.message
    );
    // And a Client keeping the Remote in view recovers from it saying why.
    let mut catalog = laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    let Some(ManagedEvent::Recovering(recovering)) = next_catalog_event(&mut catalog).await else {
        panic!("the Remote's catalog recovers");
    };
    assert_eq!(recovering.unreachable, capped);

    // Once the join holding the place ends, the Remote answers again, and
    // nothing more is said of the cap.
    drop(held);
    recovered(&mut catalog).await;
    assert_eq!(
        laptop
            .wait_for_remote(RemoteStatus::Available)
            .await
            .unreachable,
        None
    );

    drop(catalog);
    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_remote_paired_through_a_relay_is_reached_there_by_keys_alone_across_restarts() {
    let mut paired = PairedThrough::start("relay-pairing-restarts").await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    paired.laptop.restart().await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    paired.workstation.restart().await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    paired.relay.restart().await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;
    assert_eq!(
        paired.laptop.client.list_remotes().await.unwrap()[0].ways,
        vec![Way::Relay(paired.relay.address())],
        "the Remote is reached by the way its Invite offered and nothing more"
    );

    paired.shutdown().await;
}

#[tokio::test]
async fn a_session_is_created_prompted_and_streamed_through_a_relay_as_directly() {
    let mut paired = PairedThrough::start("relay-pairing-session").await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let descriptor = paired.laptop.server.as_ref().unwrap().descriptor().clone();
    let http = reqwest::Client::new();
    let remote_api = format!("{}/v1/remotes/{REMOTE}", descriptor.base_url);

    let health = http
        .get(format!("{remote_api}/health"))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read the Remote's health through the Relay")
        .error_for_status()
        .expect("the Remote answers its health")
        .json::<Health>()
        .await
        .expect("decode the Remote's health");
    assert_eq!(
        health.instance_id,
        paired
            .workstation
            .server
            .as_ref()
            .unwrap()
            .descriptor()
            .identity
            .instance_id
    );
    let created = http
        .post(format!("{remote_api}/v1/sessions"))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Map the Remote workspace".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create a Session through the Relay")
        .error_for_status()
        .expect("the Remote creates the Session")
        .json::<SessionSnapshot>()
        .await
        .expect("decode the Remote's Session");
    assert!(
        paired
            .laptop
            .client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty(),
        "the laptop creates no Session of its own"
    );
    assert_eq!(
        paired.workstation.client.list_sessions(None).await.unwrap()[0].id(),
        created.session.id
    );

    let mut events = http
        .get(format!(
            "{remote_api}/v1/sessions/{}/events",
            created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Session's stream through the Relay")
        .error_for_status()
        .expect("the Remote streams the Session")
        .bytes_stream()
        .eventsource();
    let snapshot = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("the Session's snapshot arrives")
        .expect("the stream stays open")
        .expect("decode the snapshot");
    assert_eq!(snapshot.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(
        serde_json::from_str::<SessionSnapshot>(&snapshot.data)
            .unwrap()
            .session
            .id,
        created.session.id
    );

    let admitted = http
        .post(format!(
            "{remote_api}/v1/sessions/{}/prompts",
            created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue on the Remote".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt through the Relay")
        .error_for_status()
        .expect("the Remote admits the Prompt")
        .json::<suru::protocol::Prompt>()
        .await
        .expect("decode the admitted Prompt");
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the stream stays open")
                .expect("decode the Session's update");
            if event.event != SESSION_UPDATED_EVENT {
                continue;
            }
            let update = serde_json::from_str::<SessionUpdate>(&event.data).unwrap();
            if update.changes.iter().any(|change| {
                matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == admitted.id)
            }) {
                break;
            }
        }
    })
    .await
    .expect("the admitted Prompt is streamed through the Relay");

    drop(events);
    paired.relay.route.wait_for_connections(2).await;
    paired.shutdown().await;
}

#[tokio::test]
async fn an_attachment_is_uploaded_and_fetched_through_a_relay_as_directly() {
    let paired = PairedThrough::start("relay-pairing-attachment").await;
    let descriptor = paired.laptop.server.as_ref().unwrap().descriptor().clone();
    let http = reqwest::Client::new();
    let remote_api = format!("{}/v1/remotes/{REMOTE}", descriptor.base_url);

    // Far past what one frame a Relay carries holds.
    let image = crate::padded_png(3 * 1024 * 1024);
    let uploaded = http
        .post(format!("{remote_api}/v1/attachments"))
        .bearer_auth(&descriptor.token)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(image.clone())
        .send()
        .await
        .expect("upload through the Relay");
    assert_eq!(uploaded.status(), reqwest::StatusCode::CREATED);
    let attachment = uploaded
        .json::<AttachmentDescriptor>()
        .await
        .expect("decode the Remote's Attachment");
    assert_eq!(attachment.byte_length, image.len() as u64);
    let fetched = http
        .get(format!("{remote_api}/v1/attachments/{}", attachment.id))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("fetch through the Relay");
    assert_eq!(fetched.status(), reqwest::StatusCode::OK);
    assert_eq!(
        fetched.headers()[reqwest::header::CONTENT_TYPE],
        "image/png"
    );
    assert_eq!(fetched.bytes().await.unwrap(), image);

    paired.shutdown().await;
}

/// An Attachment fetched through a Relay whose reader pauses — longer than
/// the heartbeat gives the Relay to answer, and far shorter than the Relay
/// waits on a reader — is held up, and goes on whole once its reader takes
/// it in again.
#[tokio::test]
async fn an_attachment_whose_reader_pauses_past_the_heartbeat_is_fetched_whole() {
    let heartbeat_timeout = Duration::from_millis(100);
    let paired = PairedThrough::with_timings(
        "relay-pairing-paused-reader",
        relay_timings().with_relay_heartbeat(Duration::from_millis(20), heartbeat_timeout),
    )
    .await;
    let descriptor = paired.laptop.server.as_ref().unwrap().descriptor().clone();
    let http = reqwest::Client::new();
    let remote_api = format!("{}/v1/remotes/{REMOTE}", descriptor.base_url);
    let image = crate::padded_png(5 * 1024 * 1024);
    let attachment = http
        .post(format!("{remote_api}/v1/attachments"))
        .bearer_auth(&descriptor.token)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(image.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("upload through the Relay")
        .json::<AttachmentDescriptor>()
        .await
        .expect("decode the Remote's Attachment");

    let mut fetched = http
        .get(format!("{remote_api}/v1/attachments/{}", attachment.id))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("fetch through the Relay");
    let mut body = fetched
        .chunk()
        .await
        .expect("the fetch begins")
        .expect("the Attachment has bytes")
        .to_vec();
    tokio::time::sleep(5 * heartbeat_timeout).await;
    let rest = timeout(PROGRESS_DEADLINE, async {
        while let Some(chunk) = fetched.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>(())
    })
    .await
    .expect("the fetch finishes once its reader takes it in again");
    rest.expect("the fetch goes on past its reader's pause");
    assert!(body == image, "the Attachment arrives whole");

    paired.shutdown().await;
}

#[tokio::test]
async fn a_pairing_protocol_mismatch_through_a_relay_is_reported_as_it_is_directly() {
    let mut paired = PairedThrough::start("relay-pairing-mismatch").await;
    let newer = PROTOCOL_VERSION + 1;
    paired.workstation.timings.pairing_protocol_version = newer;
    paired.workstation.restart().await;

    let health = paired
        .laptop
        .wait_for_remote(RemoteStatus::ProtocolMismatch)
        .await;
    assert_eq!(health.protocol_version, Some(newer));

    let through_relay = paired
        .workstation
        .invite(vec![Way::Relay(paired.relay.address())])
        .await;
    let refused = paired
        .laptop
        .redeem_as(through_relay, "workstation-again")
        .await
        .expect_err("a Serving Server of another Pairing protocol");
    let direct = paired
        .workstation
        .invite(vec![Way::Direct(paired.workstation.serving_address())])
        .await;
    let refused_directly = paired
        .laptop
        .redeem_as(direct, "workstation-again")
        .await
        .expect_err("a Serving Server of another Pairing protocol");
    for refused in [&refused, &refused_directly] {
        assert_eq!(
            error_code(refused),
            SessionErrorCode::PairingProtocolMismatch
        );
    }
    // The Serving Server refuses the enrollment in its own words, saying
    // its version first.
    assert_eq!(
        error_message(&refused),
        format!("Pairing protocol mismatch: local v{newer}, remote v{PROTOCOL_VERSION}")
    );
    assert_eq!(error_message(&refused), error_message(&refused_directly));

    paired.shutdown().await;
}

#[tokio::test]
async fn removing_a_remote_paired_through_a_relay_withdraws_through_the_relay() {
    let paired = PairedThrough::start("relay-pairing-withdrawal").await;
    assert_eq!(
        paired.workstation.client.list_peers().await.unwrap().len(),
        1
    );

    let removal = paired
        .laptop
        .client
        .remove_remote(REMOTE)
        .await
        .expect("remove the Remote");
    assert!(
        removal.acknowledged,
        "the workstation answered the withdrawal through the Relay"
    );
    assert!(
        paired
            .laptop
            .client
            .list_remotes()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        paired
            .workstation
            .client
            .list_peers()
            .await
            .unwrap()
            .is_empty(),
        "the workstation dropped the Peer that withdrew"
    );

    paired.shutdown().await;
}

#[tokio::test]
async fn removing_the_peer_ends_the_pairing_and_closes_what_the_relay_carried() {
    let mut paired = PairedThrough::start("relay-pairing-revocation").await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    let session = api.begin_session(workspace.path()).await;
    let mut events = api.session_events(&session).await;
    paired.relay.route.wait_for_connections_at_least(4).await;

    let peer = paired.laptop.fingerprint();
    paired.workstation.client.remove_peer(&peer).await.unwrap();
    let ended = timeout(PROGRESS_DEADLINE, async {
        while let Some(Ok(_)) = events.next().await {}
    })
    .await;
    assert!(
        ended.is_ok(),
        "the Session's stream ends with the Peer's removal"
    );
    let failure = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::RemoteFailed { status, .. }) => return status,
                Some(_) => {}
                None => panic!("the Remote's catalog ended without saying why"),
            }
        }
    })
    .await
    .expect("the removal is felt through the Relay");
    assert_eq!(failure, RemoteStatus::Revoked);
    assert_eq!(
        paired.laptop.client.list_remotes().await.unwrap()[0].status,
        RemoteStatus::Revoked
    );
    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;

    paired.shutdown().await;
}

#[tokio::test]
async fn a_join_is_asked_only_while_a_client_holds_interest_in_the_remote() {
    let mut paired = PairedThrough::start("relay-pairing-interest").await;
    // The laptop's own connection to the Relay, and the one the workstation
    // waits on: nothing is joined while nobody looks at the Remote.
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        paired.relay.route.opened_connections(),
        opened,
        "no join is asked while no Client holds interest in the Remote"
    );

    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    paired.relay.route.wait_for_connections_at_least(4).await;

    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        paired.relay.route.opened_connections(),
        opened,
        "no join is asked once the Client lets the Remote go"
    );

    paired.shutdown().await;
}

#[tokio::test]
async fn a_remote_reached_only_through_a_relay_that_stops_answering_answers_again_on_its_own() {
    let mut paired = PairedThrough::start("relay-pairing-relay-silent").await;
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    paired.relay.route.set_online(false).await;
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::Recovering(_))
    ));
    assert_eq!(
        paired
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote")
            .status,
        RemoteStatus::Unavailable,
        "the Remote is Unreachable while its one way is"
    );

    paired.relay.route.set_online(true).await;
    let recovered = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::RemoteRecovered) => return,
                Some(ManagedEvent::RemoteFailed { status, message }) => {
                    panic!("the Remote failed for good: {status:?}: {message}")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await;
    recovered.expect("the Remote answers again once the Relay does, with nobody asking");
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    paired.shutdown().await;
}

/// Waits until the Remote whose stream `catalog` follows answers again, with
/// nobody asking.
async fn recovered(catalog: &mut suru::managed_client::SessionCatalogSubscription) {
    let recovering = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(catalog).await {
                Some(ManagedEvent::RemoteRecovered) => return,
                Some(ManagedEvent::RemoteFailed { status, message }) => {
                    panic!("the Remote failed for good: {status:?}: {message}")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    });
    recovering
        .await
        .expect("the Remote answers again on its own");
}

/// The scripted identity provider is driven through admitted, not admitted,
/// and admitted again, with a stream open through the Relay: the Account
/// lapsing cuts the stream at once and refuses both Servers' Logins, a Remote
/// reached only through the Relay reads Unreachable while one with a direct
/// way goes on, no Pairing ends and the Relay forgets nothing — and one
/// fresh login from the laptop restores both Logins, the workstation and the
/// Remote recovering on their own.
#[tokio::test]
async fn a_lapsed_account_cuts_a_live_stream_through_its_relay_and_one_fresh_login_restores_it_all()
{
    let mut paired = PairedThrough::start("relay-pairing-lapse").await;
    let address = paired.relay.address();
    // A tablet of the same Account is paired with the workstation by an
    // Invite offering its listener as well as the Relay.
    let tablet = TestServer::start("relay-pairing-lapse-tablet").await;
    tablet.log_in(&paired.relay, "583231", "octocat").await;
    let both = paired
        .workstation
        .invite(vec![
            Way::Direct(paired.workstation.serving_address()),
            Way::Relay(address.clone()),
        ])
        .await;
    tablet
        .redeem_as(both, REMOTE)
        .await
        .expect("pair the tablet by both ways");
    let peers = paired.workstation.client.list_peers().await.unwrap();
    assert_eq!(peers.len(), 2);
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    paired.relay.route.wait_for_connections_at_least(5).await;

    // Not admitted.
    paired.relay.provider.set_admitted("583231", false);
    assert!(
        matches!(
            next_catalog_event(&mut catalog).await,
            Some(ManagedEvent::Recovering(_))
        ),
        "the stream through the Relay is cut at once"
    );
    for server in [&paired.workstation, &paired.laptop, &tablet] {
        let lapsed = server
            .wait_for_state(&address, RelayState::LoginNeeded)
            .await;
        assert_eq!(lapsed.unreachable, None);
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
        tablet.client.probe_remote(REMOTE).await.unwrap().status,
        RemoteStatus::Available,
        "a Remote with a direct way goes on working"
    );
    assert_eq!(
        paired.workstation.client.list_peers().await.unwrap(),
        peers,
        "no Pairing ends"
    );
    for server in [&paired.laptop, &tablet] {
        let remotes = server.client.list_remotes().await.unwrap();
        assert_eq!(remotes.len(), 1);
        assert_ne!(remotes[0].status, RemoteStatus::Revoked);
    }
    let store = paired.relay.running().store();
    assert!(store.accounts().await.unwrap()[0].lapsed);
    assert_eq!(
        store.logins().await.unwrap().len(),
        3,
        "the Relay forgets no Login"
    );

    // Admitted again: one fresh login from the laptop restores every Login,
    // and the workstation and the Remote through the Relay recover with
    // nobody at them.
    paired.relay.provider.set_admitted("583231", true);
    let login = paired
        .laptop
        .log_in(&paired.relay, "583231", "octocat")
        .await;
    assert!(
        matches!(
            login.outcome,
            suru::protocol::RelayLoginOutcome::Done { .. }
        ),
        "{login:?}"
    );
    for server in [&paired.workstation, &tablet] {
        server.wait_for_state(&address, RelayState::LoggedIn).await;
    }
    recovered(&mut catalog).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    tablet.shutdown().await;
    paired.shutdown().await;
}

/// A stand-in Relay that logs every Server in, has a Server that waits there
/// wait, and holds each join asked there until the test releases it, then
/// answers it with `answer` — that it is made, carrying nothing, or why not.
async fn holding_joins(
    joining: Arc<Notify>,
    release: Arc<Notify>,
    answer: RelayMessage,
) -> (String, tokio::task::JoinHandle<()>) {
    let script = move |mut socket: RelaySocket, relay: String, _: usize| {
        let (joining, release, answer) = (joining.clone(), release.clone(), answer.clone());
        async move {
            if !greet(&mut socket, &relay).await {
                return;
            }
            match heard(&mut socket).await {
                Some(ServerMessage::BeginLogin { .. }) => {
                    tell(
                        &mut socket,
                        &RelayMessage::LoginStarted {
                            verification_uri: "https://login.example.com".to_owned(),
                            user_code: "CODE-HELD".to_owned(),
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
                }
                Some(ServerMessage::Wait) => {
                    tell(&mut socket, &RelayMessage::Waiting).await;
                }
                Some(ServerMessage::Join { .. }) => {
                    joining.notify_one();
                    release.notified().await;
                    tell(&mut socket, &answer).await;
                }
                Some(ServerMessage::Forget) => {
                    tell(&mut socket, &RelayMessage::Forgotten).await;
                }
                _ => {}
            }
            while heard(&mut socket).await.is_some() {}
        }
    };
    scripted_relay(script).await
}

/// A workstation Serving through the stand-in Relay at `address`, and a
/// laptop running by `timings`, both logged in there, and an Invite to the
/// workstation offering that Relay alone.
pub(super) async fn serving_through_stand_in(
    address: &str,
    channel: &str,
    timings: ServerTimings,
) -> (TestServer, TestServer, String) {
    let workstation = TestServer::start(&format!("{channel}-workstation")).await;
    let laptop = TestServer::with_timings(&format!("{channel}-laptop"), timings).await;
    workstation.serve().await;
    for server in [&workstation, &laptop] {
        server.client.add_relay(address.to_owned()).await.unwrap();
        server.client.begin_relay_login(address).await.unwrap();
        let login = server.client.follow_relay_login(address).await.unwrap();
        assert!(
            matches!(
                login.outcome,
                suru::protocol::RelayLoginOutcome::Done { .. }
            ),
            "{login:?}"
        );
    }
    workstation
        .client
        .set_relay_serve_through(address, true)
        .await
        .unwrap();
    let invite = workstation
        .invite(vec![Way::Relay(address.to_owned())])
        .await;
    (workstation, laptop, invite)
}

#[tokio::test]
async fn a_join_made_once_its_relay_is_removed_carries_nothing() {
    let (joining, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (address, _answering) =
        holding_joins(joining.clone(), release.clone(), RelayMessage::Joined).await;
    // A join wrongly carried on is given up once its handshake has had a
    // moment, rather than held until the test's deadline.
    let (workstation, laptop, invite) = serving_through_stand_in(
        &address,
        "relay-pairing-held-join",
        relay_timings().with_serving_handshake_timeout(Duration::from_millis(50)),
    )
    .await;

    let redemption = timeout(PROGRESS_DEADLINE, laptop.redeem_as(invite, REMOTE));
    let removal = async {
        timeout(PROGRESS_DEADLINE, joining.notified())
            .await
            .expect("the laptop asks the join");
        laptop
            .client
            .remove_relay(&address)
            .await
            .expect("remove the Relay while the join is asked");
        release.notify_one();
    };
    let (redeemed, ()) = tokio::join!(redemption, removal);
    let refused = redeemed
        .expect("the redemption ends")
        .expect_err("a join made once its Relay was removed carries nothing");
    assert_eq!(error_code(&refused), SessionErrorCode::RelayLoginNeeded);
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_join_its_relay_carries_nothing_of_holds_up_no_redemption() {
    let (joining, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    release.notify_one();
    let (address, _answering) = holding_joins(joining.clone(), release, RelayMessage::Joined).await;
    let (workstation, laptop, invite) = serving_through_stand_in(
        &address,
        "relay-pairing-silent-join",
        relay_timings().with_serving_handshake_timeout(Duration::from_millis(50)),
    )
    .await;

    let joined = async {
        timeout(PROGRESS_DEADLINE, joining.notified())
            .await
            .expect("the laptop asks the join");
        tokio::time::Instant::now()
    };
    let (redeemed, joined_at) = tokio::join!(
        timeout(PROGRESS_DEADLINE, laptop.redeem_as(invite, REMOTE)),
        joined
    );
    let refused = redeemed
        .expect("the redemption is given up once its handshake has had its time")
        .expect_err("nothing answered the Pairing's handshake through the Relay");
    assert!(
        joined_at.elapsed() < Duration::from_secs(1),
        "the join is given up within the handshake time it was given, far short of the \
         production one: {:?}",
        joined_at.elapsed()
    );
    assert_eq!(
        error_code(&refused),
        SessionErrorCode::PairingConnectionFailed
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn an_invite_offers_no_relay_whose_login_there_needs_renewing() {
    let mut relay = TestRelay::start().await;
    let mut workstation = TestServer::start("relay-pairing-login-needed-workstation").await;
    let address = relay.address();
    workstation.serve().await;
    workstation.log_in(&relay, "583231", "octocat").await;
    workstation.serve_through(&relay, true).await;
    workstation.invite(vec![Way::Relay(address.clone())]).await;

    // The Relay forgets the workstation's Login, which it learns as it next
    // connects there.
    relay.voice().forget(&workstation.identity()).await;
    relay.route.set_online(false).await;
    relay.route.set_online(true).await;
    workstation
        .wait_for_state(&address, RelayState::LoginNeeded)
        .await;
    let refused_ways = |refused: anyhow::Result<suru::protocol::IssuedInvite>| {
        let refused = refused
            .expect_err("a Relay whose Login there needs renewing is no way an Invite offers");
        assert_eq!(error_code(&refused), SessionErrorCode::InvalidInviteWays);
        assert!(error_message(&refused).contains(&address), "{refused:#}");
    };
    let relay_way = IssueInviteRequest {
        ways: vec![Way::Relay(address.clone())],
    };
    refused_ways(workstation.client.issue_invite(relay_way.clone()).await);

    // The Relay then stops answering. Nothing has proven the Login stands
    // since it was refused, so it still needs renewing — the entry reads so,
    // telling its user to act rather than wait — and no Invite offers the
    // Relay, however its Server fails to reach it, and across a restart.
    let tried = relay.route.opened_connections();
    relay.route.set_online(false).await;
    relay.route.wait_for_opened_connections(tried + 2).await;
    assert_eq!(
        workstation.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );
    refused_ways(workstation.client.issue_invite(relay_way.clone()).await);
    workstation.restart().await;
    let tried = relay.route.opened_connections();
    relay.route.wait_for_opened_connections(tried + 2).await;
    assert_eq!(
        workstation.relay(&address).await.unwrap().state,
        RelayState::LoginNeeded
    );
    refused_ways(workstation.client.issue_invite(relay_way).await);

    relay.route.set_online(true).await;
    workstation.log_in(&relay, "583231", "octocat").await;
    workstation.invite(vec![Way::Relay(address)]).await;

    workstation.shutdown().await;
}

#[tokio::test]
async fn a_stream_through_a_relay_that_falls_silent_ends_and_the_remote_answers_again_on_its_own() {
    let mut paired = PairedThrough::with_timings(
        "relay-pairing-relay-stalls",
        relay_timings()
            .with_relay_answer_timeout(Duration::from_secs(1))
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100))
            .with_joined_stream_keepalive(KEEPALIVE.0, KEEPALIVE.1),
    )
    .await;
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    paired.relay.route.wait_for_connections_at_least(4).await;

    // Every connection through the Relay stays open and carries nothing, so
    // only asking the Relay finds it silent.
    paired.relay.route.stall().await;
    assert!(
        matches!(
            next_catalog_event(&mut catalog).await,
            Some(ManagedEvent::Recovering(_))
        ),
        "the Remote's stream through the silent Relay ends"
    );

    paired.relay.route.set_online(false).await;
    paired.relay.route.set_online(true).await;
    let recovered = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::RemoteRecovered) => return,
                Some(ManagedEvent::RemoteFailed { status, message }) => {
                    panic!("the Remote failed for good: {status:?}: {message}")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await;
    recovered.expect("the Remote answers again once the Relay does, with nobody asking");

    drop(catalog);
    paired.shutdown().await;
}

/// What a man in the middle of the Servers' connections to a Relay has seen
/// and been told to do. Once armed, it knows the laptop's connections and
/// the workstation's by the keys they say hello with; it holds the Relay's
/// taking of each of the laptop's proofs until the test releases it, where
/// told to hold; it counts the joins the laptop asks and keeps what each
/// carries from it; it numbers the joins the workstation takes up, in turn,
/// and notes those the Relay made; and, while told to, it plays the Relay's
/// part in a join badly, as `silencing`, `pausing` and `starving` say.
#[derive(Default)]
struct Interception {
    armed: AtomicBool,
    holding: AtomicBool,
    joins: AtomicUsize,
    /// Says a proof of the laptop's is being held.
    holding_proof: Notify,
    release: Notify,
    /// Says the laptop let a connection go while its proof was held.
    let_go: Notify,
    /// What each of the laptop's connections carried from it once joined.
    carried: std::sync::Mutex<Vec<Vec<u8>>>,
    /// How many joins the workstation has taken up.
    take_ups: AtomicUsize,
    /// The joins the workstation took up that the Relay made, by number, in
    /// the order made.
    made: std::sync::Mutex<Vec<usize>>,
    /// While it holds, every join the workstation takes up carries nothing
    /// either way, as from a Serving Server fallen silent behind a Relay
    /// that answers; each is let go once it no longer holds.
    silencing: tokio::sync::watch::Sender<bool>,
    /// The joins, by number, the workstation let go while they were
    /// silenced.
    serving_let_go: tokio::sync::watch::Sender<Vec<usize>>,
    /// While it holds, nothing the workstation sends on a join it took up is
    /// taken in, as by a Relay held up by the other side of the join.
    pausing: tokio::sync::watch::Sender<bool>,
    /// Says something the workstation sent on a paused join waits untaken.
    held_up: Notify,
    /// While it holds, nothing the laptop sends on a join it asked is taken
    /// in, and nothing is carried to it but empty frames; each such join is
    /// let go once it no longer holds.
    starving: tokio::sync::watch::Sender<bool>,
}

impl Interception {
    /// The number of the join the workstation took up that the Relay made
    /// last: the one carrying what is open to the Remote.
    fn established(&self) -> usize {
        *self
            .made
            .lock()
            .unwrap()
            .last()
            .expect("the workstation took up a join the Relay made")
    }

    /// Waits until the workstation has let go of the join it took up as
    /// `take_up` while that join was silenced.
    async fn serving_lets_go_of(&self, take_up: usize) {
        let mut let_go = self.serving_let_go.subscribe();
        timeout(
            PROGRESS_DEADLINE,
            let_go.wait_for(|let_go| let_go.contains(&take_up)),
        )
        .await
        .expect("the Serving Server lets that very join go")
        .expect("the interception goes on");
    }
}

/// The identity keys a man in the middle knows the two Servers by.
#[derive(Clone)]
struct Keys {
    laptop: Vec<u8>,
    workstation: Vec<u8>,
}

/// Stands between every Server and the Relay listening at `relay`, passing
/// each message on as it came but as `interception` says, and knowing the
/// Servers by `keys`: where it listens.
async fn intercepting(
    relay: std::net::SocketAddr,
    keys: Keys,
    interception: Arc<Interception>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let answering = tokio::spawn(async move {
        while let Ok((server, _)) = listener.accept().await {
            tokio::spawn(intercept(server, relay, keys.clone(), interception.clone()));
        }
    });
    (address, answering)
}

async fn intercept(
    server: tokio::net::TcpStream,
    relay: std::net::SocketAddr,
    keys: Keys,
    interception: Arc<Interception>,
) {
    let Ok(server) = tokio_tungstenite::accept_async(server).await else {
        return;
    };
    let mut server = server.peekable();
    let Ok(stream) = tokio::net::TcpStream::connect(relay).await else {
        return;
    };
    let Ok((mut relay, _)) =
        tokio_tungstenite::client_async(format!("ws://{relay}/connect"), stream).await
    else {
        return;
    };
    // Whose connection this is since the interception was armed, where it is
    // either Server's, whether it carries a join the laptop asked or one the
    // workstation took up, and where among the laptop's what it carries is
    // kept.
    let (mut laptops, mut workstations) = (false, false);
    let (mut asked, mut taken_up) = (false, false);
    let (mut take_up, mut held_up) = (None, false);
    let mut carrying = None;
    let mut silencing = interception.silencing.subscribe();
    let mut pausing = interception.pausing.subscribe();
    let mut starving = interception.starving.subscribe();
    let mut empty_frames = tokio::time::interval(Duration::from_millis(5));
    loop {
        if taken_up && *silencing.borrow_and_update() {
            // Hears only whether the workstation lets the join go before the
            // silence ends; either way, it ends with it.
            tokio::select! {
                () = async { drop(silencing.wait_for(|silent| !*silent).await) } => return,
                said = server.next() => {
                    if !matches!(said, Some(Ok(_))) {
                        interception
                            .serving_let_go
                            .send_modify(|let_go| let_go.extend(take_up));
                        return;
                    }
                    continue;
                }
            }
        }
        if asked && *starving.borrow_and_update() {
            tokio::select! {
                () = async { drop(starving.wait_for(|starving| !*starving).await) } => return,
                _ = empty_frames.tick() => {
                    if server.send(Message::Binary(Vec::new().into())).await.is_err() {
                        return;
                    }
                }
                answered = relay.next() => {
                    if !matches!(answered, Some(Ok(_))) {
                        return;
                    }
                }
            }
            continue;
        }
        if taken_up && *pausing.borrow_and_update() {
            // Takes in nothing the workstation sends while the pause lasts,
            // and says once something it sent waits untaken.
            tokio::select! {
                () = async { drop(pausing.wait_for(|paused| !*paused).await) } => held_up = false,
                waiting = std::pin::Pin::new(&mut server).peek(), if !held_up => {
                    if !matches!(waiting, Some(Ok(_))) {
                        return;
                    }
                    held_up = true;
                    interception.held_up.notify_one();
                }
                answered = relay.next() => {
                    let Some(Ok(message)) = answered else { return };
                    if server.send(message).await.is_err() {
                        return;
                    }
                }
            }
            continue;
        }
        tokio::select! {
            _ = silencing.changed(), if taken_up => {}
            _ = pausing.changed(), if taken_up => {}
            _ = starving.changed(), if asked => {}
            said = server.next() => {
                let Some(Ok(message)) = said else { return };
                match &message {
                    Message::Text(text) => match serde_json::from_str::<ServerMessage>(text.as_str()) {
                        Ok(ServerMessage::Hello { key, .. })
                            if interception.armed.load(Ordering::Acquire) =>
                        {
                            laptops = key.0 == keys.laptop;
                            workstations = key.0 == keys.workstation;
                        }
                        Ok(ServerMessage::Join { .. }) if laptops => {
                            interception.joins.fetch_add(1, Ordering::AcqRel);
                            asked = true;
                        }
                        Ok(ServerMessage::Accept { .. }) if workstations => {
                            taken_up = true;
                            take_up =
                                Some(interception.take_ups.fetch_add(1, Ordering::AcqRel));
                        }
                        _ => {}
                    },
                    Message::Binary(bytes) if laptops => {
                        let mut carried = interception.carried.lock().unwrap();
                        let place = *carrying.get_or_insert_with(|| {
                            carried.push(Vec::new());
                            carried.len() - 1
                        });
                        carried[place].extend_from_slice(bytes);
                    }
                    _ => {}
                }
                if relay.send(message).await.is_err() {
                    return;
                }
            }
            answered = relay.next() => {
                let Some(Ok(message)) = answered else { return };
                let made = matches!(
                    &message,
                    Message::Text(text)
                        if matches!(serde_json::from_str(text.as_str()), Ok(RelayMessage::Joined))
                );
                if made && let Some(take_up) = take_up {
                    interception.made.lock().unwrap().push(take_up);
                }
                let proven = matches!(
                    &message,
                    Message::Text(text)
                        if matches!(
                            serde_json::from_str(text.as_str()),
                            Ok(RelayMessage::Proven { .. })
                        )
                );
                if proven && laptops && interception.holding.load(Ordering::Acquire) {
                    interception.holding_proof.notify_one();
                    tokio::select! {
                        () = interception.release.notified() => {}
                        said = server.next() => {
                            if !matches!(said, Some(Ok(Message::Text(_) | Message::Binary(_)))) {
                                interception.let_go.notify_one();
                            }
                            return;
                        }
                    }
                }
                if server.send(message).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// `paired`'s Relay, reached from now on through a man in the middle armed
/// as `holding` says.
async fn intercepted(
    paired: &PairedThrough,
    holding: bool,
) -> (Arc<Interception>, tokio::task::JoinHandle<()>) {
    let interception = Arc::new(Interception::default());
    interception.holding.store(holding, Ordering::Release);
    let keys = Keys {
        laptop: paired.laptop.identity().subject_public_key_info(),
        workstation: paired.workstation.identity().subject_public_key_info(),
    };
    let (intercepting_at, answering) =
        intercepting(paired.relay.running().address(), keys, interception.clone()).await;
    paired.relay.route.retarget(intercepting_at);
    interception.armed.store(true, Ordering::Release);
    (interception, answering)
}

#[tokio::test]
async fn no_join_is_asked_for_interest_let_go_while_the_relay_takes_the_proof() {
    let paired = PairedThrough::start("relay-pairing-let-go-join").await;
    let (interception, _intercepting) = intercepted(&paired, true).await;

    // A Client looking at the Remote has the laptop connect to the Relay to
    // ask a join, and lets the Remote go while the Relay takes its proof.
    let catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    timeout(PROGRESS_DEADLINE, interception.holding_proof.notified())
        .await
        .expect("the laptop proves itself to the Relay to ask a join");
    drop(catalog);
    let let_go = timeout(PROGRESS_DEADLINE, interception.let_go.notified())
        .await
        .is_ok();
    interception.release.notify_one();
    let joined = timeout(Duration::from_millis(300), async {
        while interception.joins.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        !joined,
        "no join is asked once nothing is asked of the Remote"
    );
    assert!(
        let_go,
        "the connection begun for the interest is let go with it"
    );

    paired.shutdown().await;
}

/// A Relay that cannot take a join just now — its connection log full, say —
/// leaves the way it was asked through failing as any way may, so the Remote
/// reads Unreachable and is tried again, never as a refusal its user must
/// act on.
#[tokio::test]
async fn a_join_the_relay_cannot_take_just_now_fails_as_any_way_may() {
    let (joining, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    release.notify_one();
    let (address, _answering) = holding_joins(
        joining,
        release,
        RelayMessage::Refused {
            refusal: Refusal::Unavailable,
            message: "the Relay cannot record another joined connection; ask again later"
                .to_owned(),
        },
    )
    .await;
    let (workstation, laptop, invite) =
        serving_through_stand_in(&address, "relay-pairing-join-unavailable", relay_timings()).await;

    let refused = timeout(PROGRESS_DEADLINE, laptop.redeem_as(invite, REMOTE))
        .await
        .expect("the redemption ends")
        .expect_err("the Relay took no join");
    assert_eq!(
        (error_code(&refused), error_message(&refused)),
        (
            SessionErrorCode::PairingConnectionFailed,
            "could not reach an offered address with the Invite's pinned key".to_owned()
        )
    );
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());

    laptop.shutdown().await;
    workstation.shutdown().await;
}

/// The laptop's own API, asking the Remote through it as a Client would.
pub(super) struct RemoteApi {
    http: reqwest::Client,
    remote: String,
    token: String,
}

/// A Session's stream of events, as a Client reads it.
type SessionEvents = std::pin::Pin<
    Box<
        dyn futures_util::Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            > + Send,
    >,
>;

impl RemoteApi {
    pub(super) fn of(laptop: &TestServer) -> Self {
        let descriptor = laptop.server.as_ref().unwrap().descriptor().clone();
        Self {
            http: reqwest::Client::new(),
            remote: format!("{}/v1/remotes/{REMOTE}", descriptor.base_url),
            token: descriptor.token,
        }
    }

    pub(super) fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.remote))
            .bearer_auth(&self.token)
    }

    pub(super) fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.remote))
            .bearer_auth(&self.token)
    }

    /// Asks the Remote for `path` on a connection that takes in no more than
    /// the start of the answer and then reads nothing — its own buffer kept
    /// small, so what is not read backs up toward the Remote at once —
    /// answering with that connection, to be held.
    async fn fetch_unread(&self, path: &str) -> tokio::net::TcpStream {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let url = reqwest::Url::parse(&format!("{}{path}", self.remote)).unwrap();
        let address = url.socket_addrs(|| None).unwrap()[0];
        let socket = if address.is_ipv4() {
            tokio::net::TcpSocket::new_v4()
        } else {
            tokio::net::TcpSocket::new_v6()
        }
        .unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let mut stream = socket.connect(address).await.unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\r\n",
            url.path(),
            self.token
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut start = [0_u8; 512];
        let read = timeout(PROGRESS_DEADLINE, stream.read(&mut start))
            .await
            .expect("the Remote begins its answer")
            .unwrap();
        assert!(
            start[..read].starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&start[..read])
        );
        stream
    }

    /// Asks the Remote for `path`, sending all of what is asked but the last
    /// byte of its body, so the local Server holds it unfinished until
    /// [`Self::finish_withheld`] sends that byte: the connection it is asked
    /// on.
    pub(super) async fn withholding(&self, path: &str) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt as _;
        let url = reqwest::Url::parse(&format!("{}{path}", self.remote)).unwrap();
        let address = url.socket_addrs(|| None).unwrap()[0];
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\
             Content-Length: 2\r\n\r\n{{",
            url.path(),
            self.token
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream
    }

    /// Sends the last byte of what was asked on `stream`, as
    /// [`Self::withholding`] began asking it: the status the answer comes
    /// with.
    pub(super) async fn finish_withheld(stream: &mut tokio::net::TcpStream) -> u16 {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        stream.write_all(b"}").await.unwrap();
        let mut start = [0_u8; 12];
        timeout(PROGRESS_DEADLINE, stream.read_exact(&mut start))
            .await
            .expect("the answer begins")
            .expect("read the start of the answer");
        std::str::from_utf8(&start[9..12])
            .ok()
            .and_then(|status| status.parse().ok())
            .unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(&start)))
    }

    pub(super) async fn health(&self) -> reqwest::Result<Health> {
        self.get("/health")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    /// Begins a Session on the Remote working in `workspace` there.
    pub(super) async fn begin_session(
        &self,
        workspace: &std::path::Path,
    ) -> suru::protocol::SessionId {
        self.post("/v1/sessions")
            .json(&CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Map the Remote workspace".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("the Remote begins a Session")
            .json::<SessionSnapshot>()
            .await
            .expect("decode the Remote's Session")
            .session
            .id
    }

    /// The stream of `session`'s events, once its snapshot has come.
    pub(super) async fn session_events(
        &self,
        session: &suru::protocol::SessionId,
    ) -> SessionEvents {
        let mut events: SessionEvents = Box::pin(
            self.get(&format!("/v1/sessions/{session}/events"))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .expect("the Remote streams the Session")
                .bytes_stream()
                .eventsource(),
        );
        let snapshot = timeout(PROGRESS_DEADLINE, events.next())
            .await
            .expect("the Session's snapshot arrives")
            .expect("the stream stays open")
            .expect("decode the snapshot");
        assert_eq!(snapshot.event, SESSION_SNAPSHOT_EVENT);
        events
    }

    /// Queues a Prompt saying `text` on `session`.
    async fn prompt(
        &self,
        session: &suru::protocol::SessionId,
        text: &str,
    ) -> suru::protocol::Prompt {
        self.post(&format!("/v1/sessions/{session}/prompts"))
            .json(&AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            })
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("the Remote admits the Prompt")
            .json()
            .await
            .expect("decode the admitted Prompt")
    }
}

/// Waits until `events` says `prompt` was added to its Session.
async fn prompt_added(events: &mut SessionEvents, prompt: &suru::protocol::Prompt) {
    let streamed = timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the stream stays open")
                .expect("decode the Session's update");
            if event.event != SESSION_UPDATED_EVENT {
                continue;
            }
            let update = serde_json::from_str::<SessionUpdate>(&event.data).unwrap();
            if update.changes.iter().any(|change| {
                matches!(change, SessionChange::PromptAdded { prompt: added } if added.id == prompt.id)
            }) {
                return;
            }
        }
    });
    streamed
        .await
        .expect("the added Prompt is streamed through the Relay");
}

impl TestRelay {
    /// How many joined connections the Relay has logged once those that
    /// ended have all been: the count no longer moving.
    async fn settled_joined_connections(&self) -> usize {
        let mut logged = self.joined_connections_logged();
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = self.joined_connections_logged();
            if now == logged {
                return logged;
            }
            logged = now;
        }
    }
}

/// Everything a Client holds open to a Remote reached through a Relay — the
/// Remote's catalog, a burst of requests well past what a Relay asks of a
/// Server at once, all begun together while nothing was yet open to it, and
/// then a Session's stream — travels together, so the Relay joins one
/// connection for the Remote in view however much is open to it.
#[tokio::test]
async fn everything_open_to_a_remote_through_a_relay_shares_one_joined_connection() {
    let mut paired = PairedThrough::start("relay-pairing-one-join").await;
    // The laptop's own connection to the Relay, and the one the workstation
    // waits on.
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();
    let logged = paired.relay.settled_joined_connections().await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);

    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    let (reconciled, burst) = tokio::join!(
        next_catalog_event(&mut catalog),
        futures_util::future::join_all((0..40).map(|_| api.health()))
    );
    assert!(matches!(
        reconciled,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    let unanswered = burst.iter().filter(|answer| answer.is_err()).count();
    assert_eq!(unanswered, 0, "every request of the burst is answered");
    let session = api.begin_session(workspace.path()).await;
    let mut events = api.session_events(&session).await;
    let prompt = api.prompt(&session, "Continue on the Remote").await;
    prompt_added(&mut events, &prompt).await;

    assert_eq!(
        (
            paired.relay.route.connections(),
            paired.relay.route.opened_connections() - opened
        ),
        (4, 2),
        "one join carries it all: a connection to the Relay from each Server"
    );
    drop(events);
    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;
    paired
        .relay
        .wait_for_joined_connections_logged(logged + 1)
        .await;
    assert_eq!(
        paired.relay.settled_joined_connections().await,
        logged + 1,
        "the Relay joined one connection for the Remote in view"
    );

    paired.shutdown().await;
}

/// The one join a Remote in view costs goes on while anything is still open
/// to the Remote, and ends with the last of it; none is asked after.
#[tokio::test]
async fn the_joined_connection_ends_with_the_last_interest_in_the_remote() {
    let mut paired = PairedThrough::start("relay-pairing-last-interest").await;
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);

    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    let session = api.begin_session(workspace.path()).await;
    let events = api.session_events(&session).await;
    assert_eq!(paired.relay.route.connections(), 4);

    drop(events);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        (
            paired.relay.route.connections(),
            paired.relay.route.opened_connections() - opened
        ),
        (4, 2),
        "the join goes on while the catalog is still open, and none other is asked"
    );

    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        paired.relay.route.opened_connections(),
        opened,
        "no join is asked once the last interest in the Remote ends"
    );

    paired.shutdown().await;
}

/// A joined connection that drops fails everything it carried at once, and
/// the Remote is tried again on its own until it answers, carried again on
/// one join.
#[tokio::test]
async fn a_joined_connection_that_drops_fails_all_it_carried_and_the_remote_answers_again() {
    let mut paired = PairedThrough::start("relay-pairing-join-drops").await;
    paired.relay.route.wait_for_connections(2).await;
    let before_joining = paired.relay.route.opened_connections();
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    let session = api.begin_session(workspace.path()).await;
    let mut events = api.session_events(&session).await;
    assert_eq!(paired.relay.route.connections(), 4);

    paired.relay.route.cut_opened_after(before_joining);
    let ended = timeout(PROGRESS_DEADLINE, async {
        while let Some(Ok(_)) = events.next().await {}
    })
    .await;
    assert!(
        ended.is_ok(),
        "the Session's stream ends with the join that carried it"
    );
    // What the catalog said of the Session begun comes first.
    let recovering = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::Recovering(_)) => return,
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await;
    recovering.expect("the catalog's stream ends with the join that carried it");
    let recovered = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::RemoteRecovered) => return,
                Some(ManagedEvent::RemoteFailed { status, message }) => {
                    panic!("the Remote failed for good: {status:?}: {message}")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    })
    .await;
    recovered.expect("the Remote answers again, with nobody asking");
    let mut events = api.session_events(&session).await;
    let prompt = api.prompt(&session, "Carry on").await;
    prompt_added(&mut events, &prompt).await;
    assert_eq!(
        paired.relay.route.connections(),
        4,
        "everything is carried again on one join"
    );

    drop(events);
    drop(catalog);
    paired.shutdown().await;
}

/// Attachments fetched through a Relay and left unread hold back their own
/// fetches and nothing else carried beside them on the same join: once the
/// fetches have stopped short, a Session's stream goes on.
#[tokio::test]
async fn attachments_left_unread_hold_up_no_session_stream_through_the_relay() {
    const ATTACHMENT: usize = 5 * 1024 * 1024;
    const FETCHES: usize = 4;
    let mut paired = PairedThrough::start("relay-pairing-unread-attachments").await;
    paired.relay.route.wait_for_connections(2).await;
    let logged = paired.relay.settled_joined_connections().await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    let session = api.begin_session(workspace.path()).await;
    let mut events = api.session_events(&session).await;
    let attachment = api
        .post("/v1/attachments")
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(crate::padded_png(ATTACHMENT))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("upload through the Relay")
        .json::<AttachmentDescriptor>()
        .await
        .expect("decode the Remote's Attachment");

    let before_fetching = paired.relay.route.answered_bytes();
    let mut unread = Vec::new();
    for _ in 0..FETCHES {
        unread.push(
            api.fetch_unread(&format!("/v1/attachments/{}", attachment.id))
                .await,
        );
    }
    let fetched = paired
        .relay
        .route
        .wait_until_answers_stop(Duration::from_millis(100))
        .await
        - before_fetching;
    assert!(
        fetched < (FETCHES * ATTACHMENT) as u64,
        "the fetches stopped short, {fetched} bytes carried"
    );
    let beside = async {
        let prompt = api.prompt(&session, "Carry on beside the fetches").await;
        prompt_added(&mut events, &prompt).await;
        api.health().await
    };
    timeout(PROGRESS_DEADLINE, beside)
        .await
        .expect("the Session goes on beside the fetches")
        .expect("the Remote answers beside the fetches");
    assert_eq!(
        paired.relay.route.connections(),
        4,
        "the fetches and the Session share one join"
    );

    drop(unread);
    drop(events);
    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;
    paired
        .relay
        .wait_for_joined_connections_logged(logged + 1)
        .await;
    assert_eq!(
        paired.relay.settled_joined_connections().await,
        logged + 1,
        "the Relay joined one connection for it all"
    );
    paired.shutdown().await;
}

/// What a Relay carries for a Remote in view, however much travels
/// together, is the pinned-key TLS alone: a handshake, and then nothing it
/// can read.
#[tokio::test]
async fn a_relay_carries_nothing_of_a_remote_in_view_but_the_pinned_tls() {
    let mut paired = PairedThrough::start("relay-pairing-carried-tls").await;
    let (interception, _intercepting) = intercepted(&paired, false).await;
    let api = RemoteApi::of(&paired.laptop);

    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    api.health()
        .await
        .expect("the Remote answers through the Relay");
    drop(catalog);
    paired.relay.route.wait_for_connections(2).await;

    const HANDSHAKE: u8 = 22;
    const CHANGE_CIPHER_SPEC: u8 = 20;
    const APPLICATION_DATA: u8 = 23;
    let carried = interception.carried.lock().unwrap().clone();
    assert!(!carried.is_empty(), "the laptop was joined to the Remote");
    for carried in carried {
        let mut records = Vec::new();
        let mut rest = carried.as_slice();
        while let [kind, _, _, high, low, after @ ..] = rest {
            let length = usize::from(u16::from_be_bytes([*high, *low]));
            records.push(*kind);
            rest = after.get(length..).unwrap_or_default();
        }
        assert_eq!(
            records.first(),
            Some(&HANDSHAKE),
            "a join begins with the TLS handshake"
        );
        assert!(
            records[1..]
                .iter()
                .all(|kind| [CHANGE_CIPHER_SPEC, APPLICATION_DATA].contains(kind)),
            "everything after the handshake is sealed: {records:?}"
        );
        assert!(records.contains(&APPLICATION_DATA));
        for clear in [&b"PRI * HTTP/2.0"[..], b"HTTP/1.1", b"/health", b"/v1/"] {
            assert!(
                !carried.windows(clear.len()).any(|window| window == clear),
                "{:?} crosses the Relay in the clear",
                String::from_utf8_lossy(clear)
            );
        }
    }

    paired.shutdown().await;
}

/// How each Server on a joined stream makes sure, in a test, that the other
/// still answers: soon after the stream carries nothing in, and giving it
/// well past any pause a busy test machine takes.
const KEEPALIVE: (Duration, Duration) = (Duration::from_millis(50), Duration::from_millis(500));

/// Waits until `catalog`, a Remote's, says the Remote went away and is being
/// tried again, past whatever it says before.
pub(super) async fn until_recovering(
    catalog: &mut suru::managed_client::SessionCatalogSubscription,
) {
    let recovering = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(catalog).await {
                Some(ManagedEvent::Recovering(_)) => return,
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    });
    recovering
        .await
        .expect("the Remote's catalog loses its stream");
}

/// Waits until `catalog`, a Remote's, says the Remote answers again.
pub(super) async fn until_recovered(
    catalog: &mut suru::managed_client::SessionCatalogSubscription,
) {
    let recovered = timeout(PROGRESS_DEADLINE, async {
        loop {
            match crate::next_session_catalog_event(catalog).await {
                Some(ManagedEvent::RemoteRecovered) => return,
                Some(ManagedEvent::RemoteFailed { status, message }) => {
                    panic!("the Remote failed for good: {status:?}: {message}")
                }
                Some(_) => {}
                None => panic!("the Remote's catalog ended"),
            }
        }
    });
    recovered
        .await
        .expect("the Remote answers again, with nobody asking");
}

/// A Remote whose leg of a join falls silent behind a Relay that goes on
/// answering — nothing carried either way between the Relay and the Serving
/// Server, while all else the Relay does goes on — is found out by both
/// Servers, each letting the join go, though nothing was asked over it
/// meanwhile. The Remote reads Unreachable while it stays silent and answers
/// again on its own once it speaks.
#[tokio::test]
async fn a_remote_silent_behind_a_relay_that_answers_reads_unreachable_and_answers_again() {
    let paired = PairedThrough::with_timings(
        "relay-pairing-silent-serving-leg",
        relay_timings()
            .with_serving_handshake_timeout(Duration::from_millis(200))
            .with_joined_stream_keepalive(KEEPALIVE.0, KEEPALIVE.1),
    )
    .await;
    let (interception, _intercepting) = intercepted(&paired, false).await;
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    // The Serving Server lets go of the very join that carried the Remote
    // in view, not merely of another it took up as it was tried again.
    let established = interception.established();
    interception.silencing.send_replace(true);
    until_recovering(&mut catalog).await;
    interception.serving_lets_go_of(established).await;
    assert_eq!(
        paired
            .laptop
            .client
            .probe_remote(REMOTE)
            .await
            .expect("probe the Remote")
            .status,
        RemoteStatus::Unavailable,
        "the Remote is Unreachable while it stays silent"
    );

    interception.silencing.send_replace(false);
    until_recovered(&mut catalog).await;
    paired.laptop.wait_for_remote(RemoteStatus::Available).await;

    drop(catalog);
    paired.shutdown().await;
}

/// A joined connection the Serving Server holds with nothing open on it —
/// its one request answered, and the connection kept — is judged all the
/// same: once its far end falls silent behind a Relay that goes on
/// answering, the Serving Server lets that very join go. The test asks as
/// the laptop with a client that makes sure of nothing itself, so only the
/// Serving Server's own judging can end it.
#[tokio::test]
async fn an_idle_joined_connection_whose_far_end_falls_silent_is_let_go_by_the_serving_server() {
    let paired = PairedThrough::with_timings(
        "relay-pairing-idle-silent",
        relay_timings()
            .with_serving_handshake_timeout(Duration::from_millis(200))
            .with_joined_stream_keepalive(KEEPALIVE.0, KEEPALIVE.1),
    )
    .await;
    let (interception, _intercepting) = intercepted(&paired, false).await;
    let (laptop, workstation) = (paired.laptop.identity(), paired.workstation.identity());
    let joined = paired
        .relay
        .voice()
        .joined(&laptop, &workstation.subject_public_key_info())
        .await;
    let mut idle = Multiplexed::over(
        paired_tls(joined, &laptop, &workstation)
            .await
            .expect("the pinned TLS runs through the Relay"),
    )
    .await
    .unwrap();
    assert_eq!(
        idle.health().await.unwrap(),
        hyper::StatusCode::OK,
        "the one request is answered, and nothing is left open"
    );

    let established = interception.established();
    interception.silencing.send_replace(true);
    interception.serving_lets_go_of(established).await;

    drop(idle);
    paired.shutdown().await;
}

/// A Relay that takes in nothing a Server sends on a join, and carries it
/// nothing but empty frames, is found silent all the same: the join is given
/// up, and the Remote answers again once the Relay carries it.
#[tokio::test]
async fn a_relay_carrying_only_empty_frames_and_taking_in_nothing_is_found_silent() {
    let paired = PairedThrough::with_timings(
        "relay-pairing-empty-frames",
        relay_timings()
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100))
            .with_serving_handshake_timeout(Duration::from_millis(200))
            .with_joined_stream_keepalive(KEEPALIVE.0, KEEPALIVE.1),
    )
    .await;
    let (interception, _intercepting) = intercepted(&paired, false).await;
    let mut catalog = paired
        .laptop
        .client
        .outlook(Outlook::Remote(REMOTE.to_owned()))
        .subscribe_catalog();
    assert!(matches!(
        next_catalog_event(&mut catalog).await,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));

    interception.starving.send_replace(true);
    until_recovering(&mut catalog).await;
    interception.starving.send_replace(false);
    until_recovered(&mut catalog).await;

    drop(catalog);
    paired.shutdown().await;
}

/// An Attachment fetched through a Relay that, for a while, takes in nothing
/// the Serving Server sends — held up by the join's other side — is fetched
/// whole: the pause outlasts what the Relay's heartbeat gives an echo, and is
/// far short of what the joined stream's keepalive or the Relay give a side
/// that takes nothing in.
#[tokio::test]
async fn an_attachment_its_relay_holds_up_for_a_while_is_fetched_whole() {
    let paired = PairedThrough::with_timings(
        "relay-pairing-relay-held-up",
        relay_timings()
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100))
            .with_joined_stream_keepalive(KEEPALIVE.0, Duration::from_secs(5)),
    )
    .await;
    let (interception, _intercepting) = intercepted(&paired, false).await;
    let api = RemoteApi::of(&paired.laptop);
    let image = crate::padded_png(5 * 1024 * 1024);
    let attachment = api
        .post("/v1/attachments")
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(image.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("upload through the Relay")
        .json::<AttachmentDescriptor>()
        .await
        .expect("decode the Remote's Attachment");

    let mut fetched = api
        .get(&format!("/v1/attachments/{}", attachment.id))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("fetch through the Relay");
    let mut body = fetched
        .chunk()
        .await
        .expect("the fetch begins")
        .expect("the Attachment has bytes")
        .to_vec();
    // The pause is timed from when it has taken hold: once something the
    // workstation sent waits untaken.
    interception.pausing.send_replace(true);
    timeout(PROGRESS_DEADLINE, interception.held_up.notified())
        .await
        .expect("the workstation is held up by the Relay's pause");
    tokio::time::sleep(Duration::from_millis(500)).await;
    interception.pausing.send_replace(false);
    let rest = timeout(PROGRESS_DEADLINE, async {
        while let Some(chunk) = fetched.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>(())
    })
    .await
    .expect("the fetch finishes once the Relay takes it in again");
    rest.expect("the fetch goes on past the Relay's pause");
    assert!(body == image, "the Attachment arrives whole");

    paired.shutdown().await;
}

/// The joined stream filled to what it carries at once: a hundred streams
/// of `session`'s events, each open once its snapshot has come.
async fn filled(api: &RemoteApi, session: &suru::protocol::SessionId) -> Vec<SessionEvents> {
    timeout(
        PROGRESS_DEADLINE,
        futures_util::future::join_all((0..100).map(|_| api.session_events(session))),
    )
    .await
    .expect("a hundred streams open over the joined stream at once")
}

/// More requests than a joined stream carries at once wait their turn on it,
/// and are answered as the streams ahead of them end.
#[tokio::test]
async fn requests_past_what_a_joined_stream_carries_at_once_wait_their_turn() {
    let mut paired = PairedThrough::start("relay-pairing-requests-wait").await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let session = api.begin_session(workspace.path()).await;
    paired.relay.route.wait_for_connections(2).await;
    let opened = paired.relay.route.opened_connections();

    let streams = filled(&api, &session).await;
    let mut waiting = Box::pin(futures_util::future::join_all(
        (0..30).map(|_| api.health()),
    ));
    assert!(
        timeout(Duration::from_millis(200), &mut waiting)
            .await
            .is_err(),
        "requests past what the joined stream carries at once wait their turn"
    );
    drop(streams);
    let answered = timeout(PROGRESS_DEADLINE, waiting)
        .await
        .expect("the requests waiting are answered as the streams ahead end");
    assert_eq!(answered.iter().filter(|answer| answer.is_err()).count(), 0);
    assert_eq!(
        paired.relay.route.opened_connections() - opened,
        2,
        "everything went over one join"
    );

    paired.shutdown().await;
}

/// More requests than a joined stream carries at once, all asked over it as
/// it comes to stand, before the Remote can have told which Relays it Serves
/// through, leave the stream it tells that over its place: the Remote is
/// told when it comes to Serve through another Relay meanwhile, and the
/// request past them waits its turn.
#[tokio::test]
async fn the_relays_are_told_beside_as_much_as_a_joined_stream_carries_at_once() {
    let mut paired = PairedThrough::start("relay-pairing-told-beside").await;
    let elsewhere = TestRelay::start().await;
    paired
        .workstation
        .log_in(&elsewhere, "583231", "octocat")
        .await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let session = api.begin_session(workspace.path()).await;

    // The join is held at the Relay until every request waits on it, so all
    // are asked over it together as it comes to stand.
    paired.relay.route.wait_for_connections(2).await;
    paired.relay.route.delay(true);
    let mut opening = (0..101)
        .map(|_| api.session_events(&session))
        .collect::<futures_util::stream::FuturesUnordered<_>>();
    assert!(
        timeout(Duration::from_millis(500), opening.next())
            .await
            .is_err(),
        "nothing is carried while the join is held"
    );
    paired.relay.route.delay(false);
    let mut open = Vec::new();
    timeout(PROGRESS_DEADLINE, async {
        while open.len() < 100 {
            open.push(opening.next().await.expect("a stream opens"));
        }
    })
    .await
    .expect("a hundred streams open over the joined stream at once");
    assert!(
        timeout(Duration::from_millis(200), opening.next())
            .await
            .is_err(),
        "a request past what the joined stream carries at once waits its turn"
    );
    paired.workstation.serve_through(&elsewhere, true).await;
    paired
        .laptop
        .wait_for_remote_ways(&[
            Way::Relay(paired.relay.address()),
            Way::Relay(elsewhere.address()),
        ])
        .await;

    drop(open);
    let waited = timeout(PROGRESS_DEADLINE, opening.next())
        .await
        .expect("the request waiting is answered as the streams ahead end")
        .expect("a stream opens");

    drop(waited);
    paired.shutdown().await;
}

/// Requests waiting their turn on a joined stream that drops fail at once,
/// with everything it carried, and the Remote answers again over a join made
/// afresh.
#[tokio::test]
async fn requests_waiting_on_a_joined_stream_that_drops_fail_at_once() {
    let mut paired = PairedThrough::start("relay-pairing-waiting-dropped").await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the workstation");
    let api = RemoteApi::of(&paired.laptop);
    let session = api.begin_session(workspace.path()).await;
    paired.relay.route.wait_for_connections(2).await;
    let before_joining = paired.relay.route.opened_connections();

    let mut streams = filled(&api, &session).await;
    let mut waiting = Box::pin(futures_util::future::join_all(
        (0..30).map(|_| api.health()),
    ));
    assert!(
        timeout(Duration::from_millis(200), &mut waiting)
            .await
            .is_err(),
        "requests past what the joined stream carries at once wait their turn"
    );

    paired.relay.route.cut_opened_after(before_joining);
    let failed = timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the requests waiting fail at once with the join");
    assert!(
        failed.iter().all(Result::is_err),
        "no request waiting on a join that dropped is answered over it"
    );
    for events in &mut streams {
        let ended = timeout(PROGRESS_DEADLINE, async {
            while let Some(Ok(_)) = events.next().await {}
        })
        .await;
        assert!(ended.is_ok(), "every stream ends with the join");
    }
    drop(streams);
    timeout(PROGRESS_DEADLINE, api.health())
        .await
        .expect("the Remote answers in time")
        .expect("the Remote answers again through the Relay");

    paired.shutdown().await;
}
