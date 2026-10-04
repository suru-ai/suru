//! Pairing through a Relay: an Invite offering a Relay its Serving Server
//! Serves through, redeemed there by a Server logged in under the same
//! Account, and the Remote it pairs with reached through the Relay from then
//! on by keys alone, everything it offers working as it does directly
//! (ADR-0045, ADR-0046).

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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
        SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SessionChange, SessionErrorCode,
        SessionSnapshot, SessionUpdate, Way,
    },
    server::ServerTimings,
};
use suru_relay_protocol::{Refusal, RelayMessage, ServerMessage};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};
use tokio_tungstenite::tungstenite::Message;

use super::{
    RelaySocket, TestRelay, TestServer, error_code, greet, heard, relay_timings, scripted_relay,
    tell,
};
use crate::support::PROGRESS_DEADLINE;

/// The name the laptop knows the workstation by.
const REMOTE: &str = "workstation";

impl TestServer {
    /// An Invite to this Server offering `ways`.
    async fn invite(&self, ways: Vec<Way>) -> String {
        self.client
            .issue_invite(IssueInviteRequest { ways })
            .await
            .expect("issue an Invite")
            .invite
    }

    /// Redeems `invite`, naming the Remote `name`.
    async fn redeem_as(&self, invite: String, name: &str) -> anyhow::Result<Remote> {
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
    async fn wait_for_remote(&self, status: RemoteStatus) -> RemoteHealth {
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
fn error_message(error: &anyhow::Error) -> String {
    error
        .downcast_ref::<suru::protocol::SessionError>()
        .unwrap_or_else(|| panic!("a typed Session error, not {error:#}"))
        .message
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
async fn serving_through(
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
async fn next_catalog_event(
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
    for refused in [unknown, not_logged_in] {
        assert_eq!(error_code(&refused), SessionErrorCode::RelayLoginNeeded);
        assert!(
            error_message(&refused).contains(&relay.address()),
            "the refusal names the Relay: {refused:#}"
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
    assert!(laptop.client.list_remotes().await.unwrap().is_empty());
    assert!(workstation.client.list_peers().await.unwrap().is_empty());

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

    let peer = paired.laptop.fingerprint();
    paired.workstation.client.remove_peer(&peer).await.unwrap();
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
async fn serving_through_stand_in(
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
            .with_relay_heartbeat(Duration::from_millis(20), Duration::from_millis(100)),
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
/// and been told to do: once armed, it numbers the laptop's connections as
/// they say hello, holds the Relay's taking of the first one's proof until
/// the second's proof is said — so the two are made at once — and the
/// second's until the test releases it, and counts the joins the laptop asks.
#[derive(Default)]
struct Interception {
    armed: AtomicBool,
    opened: AtomicUsize,
    joins: AtomicUsize,
    second_proving: Notify,
    release: Notify,
    /// Says the laptop let its second connection go while its proof was
    /// held.
    let_go: Notify,
}

/// Stands between every Server and the Relay listening at `relay`, passing
/// each message on as it came but as `interception` says, and knowing the
/// laptop by its identity key `laptop`: where it listens.
async fn intercepting(
    relay: std::net::SocketAddr,
    laptop: Vec<u8>,
    interception: Arc<Interception>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let answering = tokio::spawn(async move {
        while let Ok((server, _)) = listener.accept().await {
            tokio::spawn(intercept(
                server,
                relay,
                laptop.clone(),
                interception.clone(),
            ));
        }
    });
    (address, answering)
}

async fn intercept(
    server: tokio::net::TcpStream,
    relay: std::net::SocketAddr,
    laptop: Vec<u8>,
    interception: Arc<Interception>,
) {
    let Ok(mut server) = tokio_tungstenite::accept_async(server).await else {
        return;
    };
    let Ok(stream) = tokio::net::TcpStream::connect(relay).await else {
        return;
    };
    let Ok((mut relay, _)) =
        tokio_tungstenite::client_async(format!("ws://{relay}/connect"), stream).await
    else {
        return;
    };
    // This connection's place among the laptop's since the interception
    // was armed, where it is one of them.
    let mut place = None;
    loop {
        tokio::select! {
            said = server.next() => {
                let Some(Ok(message)) = said else { return };
                if let Message::Text(text) = &message {
                    match serde_json::from_str::<ServerMessage>(text.as_str()) {
                        Ok(ServerMessage::Hello { key, .. })
                            if key.0 == laptop && interception.armed.load(Ordering::Acquire) =>
                        {
                            place = Some(interception.opened.fetch_add(1, Ordering::AcqRel) + 1);
                        }
                        Ok(ServerMessage::Proof { .. }) if place == Some(2) => {
                            interception.second_proving.notify_one();
                        }
                        Ok(ServerMessage::Join { .. }) if place.is_some() => {
                            interception.joins.fetch_add(1, Ordering::AcqRel);
                        }
                        _ => {}
                    }
                }
                if relay.send(message).await.is_err() {
                    return;
                }
            }
            answered = relay.next() => {
                let Some(Ok(message)) = answered else { return };
                let proven = matches!(
                    &message,
                    Message::Text(text)
                        if matches!(
                            serde_json::from_str(text.as_str()),
                            Ok(RelayMessage::Proven { .. })
                        )
                );
                if proven && place == Some(1) {
                    interception.second_proving.notified().await;
                }
                if proven && place == Some(2) {
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

#[tokio::test]
async fn no_join_is_asked_for_a_connection_begun_for_interest_since_let_go() {
    let paired = PairedThrough::start("relay-pairing-speculative-join").await;
    let interception = Arc::new(Interception::default());
    let (intercepting_at, _intercepting) = intercepting(
        paired.relay.running().address(),
        paired.laptop.identity().subject_public_key_info(),
        interception.clone(),
    )
    .await;
    paired.relay.route.retarget(intercepting_at);
    interception.armed.store(true, Ordering::Release);

    // Two requests at once each begin a connection through the Relay. The
    // first made carries one, then the other once it is done with, so the
    // second connection is left to finish for nobody.
    let descriptor = paired.laptop.server.as_ref().unwrap().descriptor().clone();
    let http = reqwest::Client::new();
    let health = || async {
        http.get(format!(
            "{}/v1/remotes/{REMOTE}/health",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .expect("the Remote answers its health through the Relay")
        .bytes()
        .await
        .expect("read the Remote's health")
    };
    let (first, second) = timeout(PROGRESS_DEADLINE, async {
        tokio::join!(health(), health())
    })
    .await
    .expect("both requests are answered over the one connection made");
    assert!(!first.is_empty() && !second.is_empty());
    assert_eq!(interception.joins.load(Ordering::Acquire), 1);

    let let_go = timeout(Duration::from_secs(1), interception.let_go.notified())
        .await
        .is_ok();
    interception.release.notify_one();
    let joined_again = timeout(Duration::from_millis(300), async {
        while interception.joins.load(Ordering::Acquire) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        !joined_again,
        "no join is asked once nothing is asked of the Remote"
    );
    assert!(
        let_go,
        "the connection begun for a request answered otherwise is let go with the interest"
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
