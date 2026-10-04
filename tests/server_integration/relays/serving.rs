//! Serving through a Relay: a Server whose user chooses to waits there
//! to be reached, and a Server paired with it under the same Account that
//! asks for it is joined to it there, the two seeing only each other's keys
//! through it. The asking side is spoken by the test with that Server's own
//! identity key, so it can say what no real Server would; a real Server
//! asking is covered in [`super::pairing`].

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use http_body_util::{BodyExt as _, Full};
use hyper::{StatusCode, client::conn::http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{KeyPair, PublicKeyData};
use suru::{
    protocol::{
        IssueInviteRequest, PROTOCOL_VERSION, RedeemInviteRequest, Relay, RelayLoginOutcome,
        SettingMutation, Way,
    },
    server::ServerTimings,
};
use suru_relay_protocol::{Bytes, Refusal, RelayMessage, ServerMessage};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    sync::{Notify, oneshot},
    time::timeout,
};

use super::{TestRelay, TestServer, account, greet, heard, scripted_relay, tell};
use crate::support::{
    PROGRESS_DEADLINE,
    relay_voice::{RelayVoice, carried},
};

impl TestRelay {
    /// The Relay as the test speaks to it as a Server.
    pub(super) fn voice(&self) -> RelayVoice {
        RelayVoice {
            at: self.route.address,
            known_as: self.address(),
        }
    }
}

impl TestServer {
    /// Turns Serving on, at the loopback address the test dials and a port
    /// of the operating system's choosing.
    pub(super) async fn serve(&self) {
        for mutation in [
            SettingMutation::ServingPort { value: Some(0) },
            SettingMutation::ServingBindAddress {
                value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            },
            SettingMutation::ServingEnabled { value: Some(true) },
        ] {
            self.client
                .mutate_setting(mutation)
                .await
                .expect("turn Serving on");
        }
    }

    pub(super) async fn stop_serving(&self) {
        self.client
            .mutate_setting(SettingMutation::ServingEnabled { value: Some(false) })
            .await
            .expect("turn Serving off");
    }

    pub(super) fn serving_address(&self) -> SocketAddr {
        self.server
            .as_ref()
            .expect("the Server is running")
            .serving_address()
            .expect("the Server is Serving")
    }

    /// The identity key this Server pairs and proves itself by.
    pub(super) fn identity(&self) -> KeyPair {
        let key = std::fs::read(self.config.data_dir().join("server-identity.pk8"))
            .expect("read the Server's identity key");
        KeyPair::try_from(key.as_slice()).expect("decode the identity key")
    }

    pub(super) async fn serve_through(&self, relay: &TestRelay, serve_through: bool) -> Relay {
        self.client
            .set_relay_serve_through(&relay.address(), serve_through)
            .await
            .expect("choose whether the Server Serves through the Relay")
    }

    /// Pairs `redeeming` with this Serving Server by an Invite offering its
    /// listener, `redeeming` naming its Remote `workstation`.
    async fn pair(&self, redeeming: &TestServer) {
        let invite = self
            .client
            .issue_invite(IssueInviteRequest {
                ways: vec![Way::Direct(self.serving_address())],
            })
            .await
            .expect("issue an Invite");
        redeeming
            .client
            .redeem_invite(RedeemInviteRequest {
                invite: invite.invite,
                name: Some("workstation".to_owned()),
                ways: Vec::new(),
            })
            .await
            .expect("form a Pairing");
    }
}

/// A Serving Server and a Server paired with it, both logged in at a Relay
/// under one Account, the Serving one Serving through it.
struct ServingThrough {
    relay: TestRelay,
    workstation: TestServer,
    laptop: TestServer,
}

impl ServingThrough {
    async fn start(channel: &str) -> Self {
        Self::with_timings(channel, super::relay_timings()).await
    }

    /// The two Servers so, the workstation running by `timings`.
    async fn with_timings(channel: &str, timings: ServerTimings) -> Self {
        let relay = TestRelay::start().await;
        let workstation =
            TestServer::with_timings(&format!("{channel}-workstation"), timings).await;
        let laptop = TestServer::start(&format!("{channel}-laptop")).await;
        workstation.serve().await;
        workstation.pair(&laptop).await;
        for server in [&workstation, &laptop] {
            server.log_in(&relay, "583231", "octocat").await;
        }
        workstation.serve_through(&relay, true).await;
        Self {
            relay,
            workstation,
            laptop,
        }
    }

    /// Asks the Relay, as the laptop, to be joined to the workstation, until
    /// the workstation waits there and takes the join up.
    async fn join(&self) -> DuplexStream {
        self.relay
            .voice()
            .joined(
                &self.laptop.identity(),
                &self.workstation.identity().subject_public_key_info(),
            )
            .await
    }

    async fn shutdown(self) {
        self.laptop.shutdown().await;
        self.workstation.shutdown().await;
    }
}

fn client_certificate(name: &str) -> rcgen::CertificateParams {
    rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap()
}

/// Runs the Pairing's TLS over `stream` in `versions` alone, as the Server
/// whose identity key is `key` presenting a certificate of `certificate`,
/// pinning the Serving Server's identity key `server`, and asking to speak
/// one of `protocols` over it.
async fn pinned_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &KeyPair,
    certificate: rcgen::CertificateParams,
    server: Vec<u8>,
    versions: &[&'static rustls::SupportedProtocolVersion],
    protocols: &[&[u8]],
) -> std::io::Result<tokio_rustls::client::TlsStream<S>> {
    let certificate = certificate.self_signed(key).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(crate::PinnedTestServerCertificate {
        expected_public_key: server,
    }))
    .with_client_auth_cert(
        vec![certificate.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            key.serialize_der(),
        )),
    )
    .unwrap();
    tls.alpn_protocols = protocols.iter().map(|protocol| protocol.to_vec()).collect();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
}

/// Runs the Pairing's pinned-key TLS over `stream` as the Server whose
/// identity key is `key`, pinning `server`'s, in TLS 1.3 as Suru does and
/// asking to speak HTTP/2 over it, as a Relay way's connection does.
pub(super) async fn paired_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &KeyPair,
    server: &KeyPair,
) -> std::io::Result<tokio_rustls::client::TlsStream<S>> {
    pinned_tls(
        stream,
        key,
        client_certificate("paired-test-client"),
        server.subject_public_key_info(),
        &[&rustls::version::TLS13],
        &[b"h2"],
    )
    .await
}

/// The same, asking to speak nothing in particular, as a direct way's
/// connection does.
async fn direct_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    key: &KeyPair,
    server: &KeyPair,
) -> std::io::Result<tokio_rustls::client::TlsStream<S>> {
    pinned_tls(
        stream,
        key,
        client_certificate("paired-test-client"),
        server.subject_public_key_info(),
        &[&rustls::version::TLS13],
        &[],
    )
    .await
}

/// A paired Server's HTTP/2 connection to a Serving Server, over the pinned
/// TLS a join carries, as a Relay way's connection is.
pub(super) struct Multiplexed {
    sender: http2::SendRequest<Full<axum::body::Bytes>>,
    running: tokio::task::JoinHandle<()>,
}

impl Multiplexed {
    /// Speaks HTTP/2 over `tls`.
    pub(super) async fn over<S>(tls: S) -> std::io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (sender, connection) = http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(tls))
            .await
            .map_err(std::io::Error::other)?;
        let running = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { sender, running })
    }

    /// Asks `request` of the Serving Server: the status it answers, and what
    /// it says.
    async fn ask(
        &mut self,
        request: hyper::Request<Full<axum::body::Bytes>>,
    ) -> std::io::Result<(StatusCode, axum::body::Bytes)> {
        let asking = async {
            self.sender.ready().await.map_err(std::io::Error::other)?;
            let answer = self
                .sender
                .send_request(request)
                .await
                .map_err(std::io::Error::other)?;
            let status = answer.status();
            let said = answer
                .into_body()
                .collect()
                .await
                .map_err(std::io::Error::other)?
                .to_bytes();
            Ok((status, said))
        };
        timeout(PROGRESS_DEADLINE, asking)
            .await
            .expect("the Serving Server settles the request in time")
    }

    /// Asks the Serving Server for its health, as a paired Server does: the
    /// status it answers.
    pub(super) async fn health(&mut self) -> std::io::Result<StatusCode> {
        let health = hyper::Request::get("https://localhost/health")
            .body(Full::default())
            .unwrap();
        Ok(self.ask(health).await?.0)
    }

    /// Takes the enrollment `phase` of the Invite whose token is `token` to
    /// the Serving Server, as a redeeming Server does.
    async fn enroll(&mut self, token: &str, phase: &str) {
        let enrollment = serde_json::to_vec(&serde_json::json!({
            "token": token,
            "protocol_version": PROTOCOL_VERSION,
            "phase": phase,
        }))
        .unwrap();
        let enrollment = hyper::Request::post("https://localhost/v1/pairing/enroll")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(enrollment.into()))
            .unwrap();
        let (status, said) = self.ask(enrollment).await.expect("enroll");
        assert_eq!(
            status,
            StatusCode::OK,
            "enrollment phase failed: {}",
            String::from_utf8_lossy(&said)
        );
    }

    /// Whether the connection has ended, before the deadline.
    async fn ended(&mut self) -> bool {
        timeout(PROGRESS_DEADLINE, &mut self.running).await.is_ok()
    }
}

/// Asks the Serving Server at the far end of `stream` for its health over
/// HTTP/1.1, as a paired Server does over a direct way: the status line it
/// answered, or why it answered nothing.
async fn health<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> std::io::Result<String> {
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
        .await?;
    let mut response = vec![0_u8; 4096];
    let read = timeout(PROGRESS_DEADLINE, stream.read(&mut response))
        .await
        .expect("the Serving Server settles the request in time")?;
    if read == 0 {
        return Err(std::io::ErrorKind::ConnectionAborted.into());
    }
    Ok(String::from_utf8_lossy(&response[..read])
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// Whether the Serving Server refused, in the TLS handshake, a connection
/// over `stream`: the handshake failing, or — TLS 1.3 finishing the client's
/// side of it before the Serving side has judged its key — the connection
/// ending at once, unanswered in whichever protocol the handshake agreed.
async fn refused_in_handshake<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(
    tls: std::io::Result<tokio_rustls::client::TlsStream<S>>,
) -> bool {
    let Ok(mut stream) = tls else {
        return true;
    };
    if stream.get_ref().1.alpn_protocol() != Some(b"h2") {
        return health(&mut stream).await.is_err();
    }
    match Multiplexed::over(stream).await {
        Ok(mut multiplexed) => multiplexed.health().await.is_err(),
        Err(_) => true,
    }
}

/// Whether the connection `stream` stands for has ended.
async fn ended<S: AsyncRead + Unpin>(stream: &mut S) -> bool {
    let mut byte = [0_u8];
    matches!(
        timeout(PROGRESS_DEADLINE, stream.read(&mut byte)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[tokio::test]
async fn serving_through_a_relay_is_off_until_chosen_and_holding_a_login_opens_nothing() {
    let mut relay = TestRelay::start().await;
    let mut workstation = TestServer::start("relay-serve-through-workstation").await;
    let laptop = TestServer::start("relay-serve-through-laptop").await;
    workstation.serve().await;
    workstation.pair(&laptop).await;
    for server in [&workstation, &laptop] {
        server.log_in(&relay, "583231", "octocat").await;
    }
    let address = relay.address();
    let (key, server) = (laptop.identity(), workstation.identity());

    let listed = workstation
        .wait_for_state(&address, suru::protocol::RelayState::LoggedIn)
        .await;
    assert!(
        !listed.serve_through,
        "Serving through a Relay is off at first"
    );
    relay.route.wait_for_connections(2).await;
    assert_eq!(
        relay
            .voice()
            .join(&key, &server.subject_public_key_info())
            .await
            .err(),
        Some(Refusal::NotWaiting),
        "a Serving Server holding a Login at a Relay does not wait there for that alone"
    );

    let chosen = workstation.serve_through(&relay, true).await;
    assert!(chosen.serve_through);
    assert_eq!(chosen.account, Some(account("octocat")));
    assert!(workstation.relay(&address).await.unwrap().serve_through);
    let stored: serde_json::Value = serde_json::from_slice(
        &std::fs::read(workstation.config.data_dir().join("relays.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        stored,
        serde_json::json!([{ "address": address, "logged_in": true, "login_needed": false, "serve_through": true }]),
        "the choice is kept with the Relay's entry"
    );
    let mut stream = Multiplexed::over(
        paired_tls(
            relay
                .voice()
                .joined(&key, &server.subject_public_key_info())
                .await,
            &key,
            &server,
        )
        .await
        .expect("a paired key opens the pinned TLS through the Relay"),
    )
    .await
    .unwrap();
    assert_eq!(stream.health().await.unwrap(), StatusCode::OK);

    workstation.serve_through(&relay, false).await;
    relay
        .voice()
        .no_longer_waiting(&key, &server.subject_public_key_info())
        .await;
    workstation.serve_through(&relay, true).await;
    relay
        .voice()
        .joined(&key, &server.subject_public_key_info())
        .await;

    workstation.stop_serving().await;
    relay
        .voice()
        .no_longer_waiting(&key, &server.subject_public_key_info())
        .await;
    workstation.serve().await;
    relay
        .voice()
        .joined(&key, &server.subject_public_key_info())
        .await;

    workstation.restart().await;
    workstation.serve().await;
    assert!(
        workstation.relay(&address).await.unwrap().serve_through,
        "the choice outlasts a restart"
    );
    let mut stream = Multiplexed::over(
        paired_tls(
            relay
                .voice()
                .joined(&key, &server.subject_public_key_info())
                .await,
            &key,
            &server,
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(stream.health().await.unwrap(), StatusCode::OK);

    let refused = workstation
        .client
        .set_relay_serve_through("https://elsewhere.example.com", true)
        .await
        .expect_err("a Relay the Server holds no entry for");
    assert_eq!(
        super::error_code(&refused),
        suru::protocol::SessionErrorCode::RelayNotFound
    );

    laptop.shutdown().await;
    workstation.shutdown().await;
}

#[tokio::test]
async fn a_paired_key_completes_a_health_check_through_the_relay_and_an_unknown_key_is_refused() {
    let serving = ServingThrough::start("relay-carried-health").await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());

    // The Relay only carries bytes: the pinned TLS runs end to end inside
    // the join, the Serving Server presenting its own key and judging the
    // laptop's.
    let mut stream = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .expect("the Serving Server presents its own pinned key through the Relay"),
    )
    .await
    .unwrap();
    let status = stream.health().await.expect("a paired key is answered");
    assert_eq!(status, StatusCode::OK);
    let status = stream.health().await.expect("the connection is kept");
    assert_eq!(status, StatusCode::OK);

    let stranger = KeyPair::generate().unwrap();
    serving
        .relay
        .voice()
        .log_in(&serving.relay.provider, &stranger, "583231", "octocat")
        .await;
    let carried = serving
        .relay
        .voice()
        .joined(&stranger, &workstation.subject_public_key_info())
        .await;
    assert!(
        refused_in_handshake(paired_tls(carried, &stranger, &workstation).await).await,
        "a key no Pairing pins is refused through the Relay as on the listener, \
         whatever Account it is logged in under"
    );

    let other_account = TestServer::start("relay-carried-health-other-account").await;
    other_account
        .log_in(&serving.relay, "99", "someone-else")
        .await;
    assert_eq!(
        serving
            .relay
            .voice()
            .join(
                &other_account.identity(),
                &workstation.subject_public_key_info()
            )
            .await
            .err(),
        Some(Refusal::DifferentAccounts)
    );
    assert_eq!(
        serving
            .relay
            .voice()
            .join(
                &laptop,
                &KeyPair::generate().unwrap().subject_public_key_info()
            )
            .await
            .err(),
        Some(Refusal::UnknownServer)
    );
    assert_eq!(
        serving
            .relay
            .voice()
            .join(&workstation, &laptop.subject_public_key_info())
            .await
            .err(),
        Some(Refusal::NotWaiting),
        "a Server that does not Serve through the Relay is not waiting there"
    );

    other_account.shutdown().await;
    serving.shutdown().await;
}

#[tokio::test]
async fn a_carried_connection_meets_the_acceptor_the_listener_feeds() {
    let serving = ServingThrough::start("relay-carried-acceptor").await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());

    let tls12 = pinned_tls(
        serving.join().await,
        &laptop,
        client_certificate("paired-test-client"),
        workstation.subject_public_key_info(),
        &[&rustls::version::TLS12],
        &[b"h2"],
    )
    .await;
    assert!(
        refused_in_handshake(tls12).await,
        "TLS 1.2 is refused through the Relay as on the listener"
    );

    // An Invite's token, carried in a redeeming Server's certificate, enrolls
    // that Server through the Relay as it does on the listener.
    let invite = serving
        .workstation
        .client
        .issue_invite(IssueInviteRequest {
            ways: vec![Way::Direct(serving.workstation.serving_address())],
        })
        .await
        .unwrap();
    let token = invite_token(&invite.invite);
    let redeeming = KeyPair::generate().unwrap();
    serving
        .relay
        .voice()
        .log_in(&serving.relay.provider, &redeeming, "583231", "octocat")
        .await;
    let mut certificate = rcgen::CertificateParams::new(Vec::new()).unwrap();
    certificate.distinguished_name = rcgen::DistinguishedName::new();
    certificate
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("suru-invite-{token}"));
    let mut enrolling = Multiplexed::over(
        pinned_tls(
            serving
                .relay
                .voice()
                .joined(&redeeming, &workstation.subject_public_key_info())
                .await,
            &redeeming,
            certificate,
            workstation.subject_public_key_info(),
            &[&rustls::version::TLS13],
            &[b"h2"],
        )
        .await
        .expect("an Invite's token opens the pinned TLS through the Relay"),
    )
    .await
    .unwrap();
    enrolling.enroll(&token, "prepare").await;
    enrolling.enroll(&token, "commit").await;
    let fingerprint = suru_relay_protocol::fingerprint(&redeeming.subject_public_key_info());
    let peers = serving.workstation.client.list_peers().await.unwrap();
    assert!(
        peers.iter().any(|peer| peer.id == fingerprint),
        "the key enrolled through the Relay is a Peer"
    );
    let mut stream = Multiplexed::over(
        paired_tls(
            serving
                .relay
                .voice()
                .joined(&redeeming, &workstation.subject_public_key_info())
                .await,
            &redeeming,
            &workstation,
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(stream.health().await.unwrap(), StatusCode::OK);

    // A revoked key is answered through the Relay as on the listener: let
    // through as a tombstone, so its Server tells revocation from a dropped
    // connection, and refused what it asks.
    let laptop_peer = suru_relay_protocol::fingerprint(&laptop.subject_public_key_info());
    serving
        .workstation
        .client
        .remove_peer(&laptop_peer)
        .await
        .unwrap();
    let dialled = tokio::net::TcpStream::connect(serving.workstation.serving_address())
        .await
        .unwrap();
    let mut direct = direct_tls(dialled, &laptop, &workstation).await.unwrap();
    let on_the_listener = health(&mut direct).await.unwrap();
    assert!(on_the_listener.contains("401"), "{on_the_listener}");
    let mut carried = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(carried.health().await.unwrap(), StatusCode::UNAUTHORIZED);

    serving.shutdown().await;
}

/// A connection a Relay carries is taken only to speak HTTP/2, as a Relay
/// way's always asks, so the keepalive a joined stream is judged by end to
/// end holds for every connection that comes through a Relay: one asking to
/// speak anything else, or nothing in particular, is refused in its
/// handshake. One dialled to the listener speaks HTTP/1.1 as ever.
#[tokio::test]
async fn a_connection_a_relay_carries_is_taken_only_to_speak_http2() {
    let serving = ServingThrough::start("relay-carried-http2-only").await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());

    for protocols in [&[][..], &[&b"http/1.1"[..]]] {
        let tls = pinned_tls(
            serving.join().await,
            &laptop,
            client_certificate("paired-test-client"),
            workstation.subject_public_key_info(),
            &[&rustls::version::TLS13],
            protocols,
        )
        .await;
        assert!(
            refused_in_handshake(tls).await,
            "a carried connection asking to speak {protocols:?} is refused"
        );
    }
    let mut multiplexed = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .expect("a carried connection asking to speak HTTP/2 is taken"),
    )
    .await
    .unwrap();
    assert_eq!(multiplexed.health().await.unwrap(), StatusCode::OK);

    let dialled = tokio::net::TcpStream::connect(serving.workstation.serving_address())
        .await
        .unwrap();
    let mut direct = direct_tls(dialled, &laptop, &workstation).await.unwrap();
    let status = health(&mut direct).await.unwrap();
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "the listener speaks HTTP/1.1 to a connection asking nothing in particular: {status}"
    );

    serving.shutdown().await;
}

/// A connection a Relay carries that finishes its handshake agreeing to
/// speak HTTP/2 and then never begins to is let go within the handshake
/// timeout, as one that never finishes its handshake is: no keepalive judges
/// it until HTTP/2 is under way. One that begins HTTP/2 carries on past that
/// time.
#[tokio::test]
async fn a_carried_connection_that_never_begins_http2_is_let_go_within_the_handshake_timeout() {
    let serving = ServingThrough::with_timings(
        "relay-carried-http2-startup",
        super::relay_timings().with_serving_handshake_timeout(Duration::from_millis(200)),
    )
    .await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());

    let mut silent = paired_tls(serving.join().await, &laptop, &workstation)
        .await
        .expect("the pinned TLS is finished, HTTP/2 agreed");
    assert!(
        crate::ends(&mut silent).await,
        "a carried connection that never begins HTTP/2 is let go"
    );

    let mut begun = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        begun.health().await.unwrap(),
        StatusCode::OK,
        "one that begins HTTP/2 carries on past the handshake timeout"
    );

    serving.shutdown().await;
}

/// The token an Invite carries.
fn invite_token(invite: &str) -> String {
    use base64::Engine as _;
    let payload: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(invite.strip_prefix("suru-v1-").unwrap())
            .unwrap(),
    )
    .unwrap();
    payload["t"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn removing_a_peer_closes_its_relay_carried_connections_as_it_closes_its_listener_ones() {
    let serving = ServingThrough::start("relay-carried-revocation").await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());
    let mut carried = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(carried.health().await.unwrap(), StatusCode::OK);
    let mut direct = crate::open_paired_health_connection(
        serving.workstation.serving_address(),
        &serving.laptop.config.data_dir().join("server-identity.pk8"),
        workstation.subject_public_key_info(),
    )
    .await;

    let peer = suru_relay_protocol::fingerprint(&laptop.subject_public_key_info());
    serving.workstation.client.remove_peer(&peer).await.unwrap();
    assert!(
        carried.ended().await,
        "removing the Peer closes what the Relay carried for it"
    );
    assert!(ended(&mut direct).await);

    serving.shutdown().await;
}

#[tokio::test]
async fn a_server_serving_through_a_relay_waits_there_again_on_its_own_after_the_relay_restarts() {
    let mut serving = ServingThrough::start("relay-carried-relay-restart").await;
    let (laptop, workstation) = (serving.laptop.identity(), serving.workstation.identity());
    let mut stream = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(stream.health().await.unwrap(), StatusCode::OK);

    serving.relay.restart().await;
    assert!(
        stream.ended().await,
        "a restarting Relay drops what it carried"
    );
    let mut stream = Multiplexed::over(
        paired_tls(serving.join().await, &laptop, &workstation)
            .await
            .expect("the Serving Server waits at the restarted Relay with nobody at it"),
    )
    .await
    .unwrap();
    assert_eq!(stream.health().await.unwrap(), StatusCode::OK);

    serving.shutdown().await;
}

/// A stand-in Relay that logs a Serving Server in, hears it wait, asks one
/// join of it, and holds the connection it takes the join up on until the
/// test releases it; then makes the join and runs the Pairing's TLS over it
/// as a Server would, telling the test whether the Serving side took the
/// connection.
struct HeldTakeUp {
    address: String,
    /// Says the Server has taken the join up, and waits to be told it is
    /// made.
    accepting: Arc<Notify>,
    release: Arc<Notify>,
    taken: oneshot::Receiver<bool>,
    _answering: tokio::task::JoinHandle<()>,
}

impl HeldTakeUp {
    async fn start(serving_key: Vec<u8>) -> Self {
        let accepting = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (taken, taken_rx) = oneshot::channel();
        let taken = Arc::new(Mutex::new(Some(taken)));
        // Each time the Server waits it is asked a join of its own, so the
        // first is told apart from any asked of it as it waits again.
        let waits = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let script = {
            let (accepting, release) = (accepting.clone(), release.clone());
            move |mut socket: super::RelaySocket, relay: String, _: usize| {
                let (accepting, release, taken, waits) = (
                    accepting.clone(),
                    release.clone(),
                    taken.clone(),
                    waits.clone(),
                );
                let serving_key = serving_key.clone();
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
                            while heard(&mut socket).await.is_some() {}
                        }
                        Some(ServerMessage::Wait) => {
                            let wait = waits.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            tell(&mut socket, &RelayMessage::Waiting).await;
                            tell(
                                &mut socket,
                                &RelayMessage::Reach {
                                    join: Bytes(vec![wait; 32]),
                                },
                            )
                            .await;
                            while heard(&mut socket).await.is_some() {}
                        }
                        // A join asked as the Server waits again is held
                        // and never made.
                        Some(ServerMessage::Accept { join }) if join.0 != [0; 32] => {
                            while heard(&mut socket).await.is_some() {}
                        }
                        Some(ServerMessage::Accept { .. }) => {
                            accepting.notify_one();
                            release.notified().await;
                            tell(&mut socket, &RelayMessage::Joined).await;
                            let tls = pinned_tls(
                                carried(socket),
                                &KeyPair::generate().unwrap(),
                                client_certificate("held-take-up"),
                                serving_key,
                                &[&rustls::version::TLS13],
                                &[b"h2"],
                            )
                            .await;
                            if let Some(taken) = taken.lock().unwrap().take() {
                                let _ = taken.send(tls.is_ok());
                            }
                        }
                        Some(ServerMessage::Forget) => {
                            tell(&mut socket, &RelayMessage::Forgotten).await;
                        }
                        _ => {}
                    }
                }
            }
        };
        let (address, answering) = scripted_relay(script).await;
        Self {
            address,
            accepting,
            release,
            taken: taken_rx,
            _answering: answering,
        }
    }

    /// Whether the Serving side took the connection the join carried, once
    /// the test releases it.
    async fn released(self) -> bool {
        self.release.notify_one();
        timeout(PROGRESS_DEADLINE, self.taken)
            .await
            .expect("the held join is tried in time")
            .expect("the stand-in Relay tries the held join")
    }
}

/// Has `server`, Serving through a stand-in Relay, take up a join it asks
/// and holds; does `meanwhile` with the server and the Relay's address while
/// the join is held; and answers whether the Serving side took the
/// connection once the join was made.
async fn hold_a_take_up<F, Fut>(channel: &str, meanwhile: F) -> bool
where
    F: FnOnce(TestServer, String) -> Fut,
    Fut: std::future::Future<Output = TestServer>,
{
    let server = TestServer::start(channel).await;
    server.serve().await;
    let held = HeldTakeUp::start(server.identity().subject_public_key_info()).await;
    server.client.add_relay(held.address.clone()).await.unwrap();
    server
        .client
        .set_relay_serve_through(&held.address, true)
        .await
        .unwrap();
    server
        .client
        .begin_relay_login(&held.address)
        .await
        .expect("begin a login at the stand-in Relay");
    let login = server
        .client
        .follow_relay_login(&held.address)
        .await
        .unwrap();
    assert!(
        matches!(login.outcome, RelayLoginOutcome::Done { .. }),
        "{login:?}"
    );
    timeout(PROGRESS_DEADLINE, held.accepting.notified())
        .await
        .expect("the Server takes the join up");

    let server = meanwhile(server, held.address.clone()).await;
    let taken = held.released().await;
    server.shutdown().await;
    taken
}

#[tokio::test]
async fn a_join_taken_up_is_handed_to_the_serving_side_once_made() {
    assert!(
        hold_a_take_up("relay-held-take-up", |server, _| async { server }).await,
        "a join taken up and then made reaches the Serving side's acceptor"
    );
}

#[tokio::test]
async fn a_join_taken_up_before_serve_through_is_turned_off_is_not_handed_on_after() {
    let taken = hold_a_take_up(
        "relay-held-take-up-serve-through",
        |server, address| async move {
            server
                .client
                .set_relay_serve_through(&address, false)
                .await
                .unwrap();
            server
        },
    )
    .await;
    assert!(
        !taken,
        "nothing is taken through a Relay the Server no longer Serves through"
    );
}

#[tokio::test]
async fn a_join_taken_up_before_its_relay_is_removed_is_not_handed_on_after() {
    let taken = hold_a_take_up("relay-held-take-up-removal", |server, address| async move {
        server.client.remove_relay(&address).await.unwrap();
        server
    })
    .await;
    assert!(
        !taken,
        "nothing is taken through a Relay the Server no longer holds"
    );
}

#[tokio::test]
async fn a_join_taken_up_before_serve_through_is_turned_off_and_on_again_is_not_handed_on() {
    let taken = hold_a_take_up(
        "relay-held-take-up-off-and-on",
        |server, address| async move {
            for serve_through in [false, true] {
                server
                    .client
                    .set_relay_serve_through(&address, serve_through)
                    .await
                    .unwrap();
            }
            server
        },
    )
    .await;
    assert!(
        !taken,
        "a join taken up before Serving through the Relay was turned off is not handed on once \
         it is turned on again"
    );
}
