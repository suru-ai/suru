//! The Relay's side of the protocol, spoken by a minimal client that can say
//! what no real Server would: a wrong proof, a replayed one, one made for
//! another Relay, a version from before or after the Relay's own, and messages
//! of a later version; and what the Relay writes of each connection it joins.

use std::{
    collections::BTreeMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{
    Admission, AdmissionRule, Clock, Identity, RelayConfig, RunningRelay,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider, TrustedProxy,
};
use suru_relay_protocol::{
    Account, Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, Side, Version, proof_message,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::timeout,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing_subscriber::layer::SubscriberExt as _;

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

/// The address the Relays these tests start are known as, which every proof
/// made for one names.
const PUBLIC_ADDRESS: &str = "https://relay.example.com";

struct Relay {
    directory: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    public_address: String,
    running: RunningRelay,
}

async fn relay() -> Relay {
    relay_with(ScriptedProvider::new(), |config| config).await
}

async fn relay_with(
    provider: ScriptedProvider,
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> Relay {
    relay_known_as(PUBLIC_ADDRESS, provider, configure).await
}

async fn relay_known_as(
    public_address: &str,
    provider: ScriptedProvider,
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> Relay {
    let directory = tempfile::tempdir().expect("create the Relay's directory");
    let provider = Arc::new(provider);
    // A test that reads the connection log configures where it goes; the
    // scripted provider admits whoever it logs in, unless the test says
    // otherwise.
    let running = suru_relay::start(
        configure(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                directory.path().join("relay.db"),
                public_address,
            )
            .with_connection_log(std::io::sink())
            .with_admission(Admission::by([provider.clone() as Arc<dyn AdmissionRule>])),
        ),
        provider.clone(),
    )
    .await
    .expect("start the Relay");
    Relay {
        directory,
        provider,
        public_address: public_address.to_owned(),
        running,
    }
}

/// A connection to a Relay that says exactly what a test tells it to, from a
/// Server that knows the Relay as `known_as`.
struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    known_as: String,
}

impl Client {
    async fn connect(relay: &Relay) -> Self {
        Self::connect_to(relay.running.address(), &relay.public_address).await
    }

    /// Connects with as small a receive buffer as the system allows, so what
    /// the Relay sends and this does not read backs up into the Relay soon.
    async fn connect_reading_little(relay: &Relay) -> Self {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let address = relay.running.address();
        let stream = socket.connect(address).await.expect("reach the Relay");
        let (socket, _) = timeout(
            DEADLINE,
            tokio_tungstenite::client_async(
                format!("ws://{address}/connect"),
                MaybeTlsStream::Plain(stream),
            ),
        )
        .await
        .expect("the Relay answers in time")
        .expect("open a WebSocket to the Relay");
        Self {
            socket,
            known_as: relay.public_address.clone(),
        }
    }

    /// Says what the Relay must answer, over and over and as fast as it can,
    /// reading nothing, until what it says stops getting through: how much it
    /// has said, and what ends once the connection has.
    fn flood(self) -> (Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        self.flood_with(r#"{"type":"later_request"}"#.to_owned())
    }

    /// Says `text` as [`Self::flood`] says what the Relay must answer.
    fn flood_with(self, text: String) -> (Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let said = Arc::new(AtomicUsize::new(0));
        let counting = said.clone();
        let flooding = tokio::spawn(async move {
            let (mut asking, _unread) = self.socket.split();
            while asking
                .send(Message::Text(text.clone().into()))
                .await
                .is_ok()
            {
                counting.fetch_add(1, Ordering::AcqRel);
            }
        });
        (said, flooding)
    }

    async fn connect_to(address: std::net::SocketAddr, known_as: &str) -> Self {
        Self::connect_forwarded_to(address, known_as, &[]).await
    }

    /// Connects as though through a reverse proxy, saying it forwards for
    /// the addresses in `forwarded_for`: one `X-Forwarded-For` line each.
    async fn connect_forwarded_for(relay: &Relay, forwarded_for: &[&str]) -> Self {
        Self::connect_forwarded_to(
            relay.running.address(),
            &relay.public_address,
            forwarded_for,
        )
        .await
    }

    async fn connect_forwarded_to(
        address: std::net::SocketAddr,
        known_as: &str,
        forwarded_for: &[&str],
    ) -> Self {
        let mut request = format!("ws://{address}/connect")
            .into_client_request()
            .unwrap();
        for line in forwarded_for {
            request
                .headers_mut()
                .append("x-forwarded-for", line.parse().unwrap());
        }
        let (socket, _) = timeout(DEADLINE, tokio_tungstenite::connect_async(request))
            .await
            .expect("the Relay answers in time")
            .expect("open a WebSocket to the Relay");
        Self {
            socket,
            known_as: known_as.to_owned(),
        }
    }

    async fn say(&mut self, message: &ServerMessage) {
        self.say_raw(serde_json::to_string(message).unwrap()).await;
    }

    async fn say_raw(&mut self, text: String) {
        self.socket
            .send(Message::Text(text.into()))
            .await
            .expect("say something to the Relay");
    }

    async fn hear(&mut self) -> RelayMessage {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay answers in time")
                .expect("the Relay keeps the connection open")
                .expect("read the Relay's answer");
            match frame {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("the Relay answered {other:?}"),
            }
        }
    }

    /// Whether the Relay has ended the connection.
    async fn ended(&mut self) -> bool {
        loop {
            match timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay ends the connection in time")
            {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(_)) => return false,
            }
        }
    }

    /// Says hello as `key` and answers the challenge, as a Server does, with
    /// the signature `prove` makes over what a proof for the Relay it knows
    /// signs, returning what the Relay says to it.
    async fn prove_with(
        &mut self,
        key: &KeyPair,
        prove: impl FnOnce(&[u8]) -> Vec<u8>,
    ) -> RelayMessage {
        self.say(&ServerMessage::Hello {
            versions: SPOKEN.to_vec(),
            key: Bytes(key.subject_public_key_info()),
        })
        .await;
        let RelayMessage::Challenge {
            version,
            nonce,
            relay,
        } = self.hear().await
        else {
            panic!("the Relay challenges a Server that says hello");
        };
        assert_eq!(version, SPOKEN[0]);
        assert_eq!(
            relay, self.known_as,
            "a Server answers only a challenge naming the Relay it knows"
        );
        let message = proof_message(&self.known_as, &nonce.0, &key.subject_public_key_info());
        self.say(&ServerMessage::Proof {
            signature: Bytes(prove(&message)),
        })
        .await;
        self.hear().await
    }

    async fn prove(&mut self, key: &KeyPair) -> RelayMessage {
        self.prove_with(key, |message| key.sign(message).unwrap())
            .await
    }

    /// Logs in as `key` through the scripted provider as the identity
    /// `subject`, named `username`.
    async fn log_in(&mut self, relay: &Relay, key: &KeyPair, subject: &str, username: &str) {
        self.log_in_from(relay, key, subject, username, "workstation")
            .await;
    }

    /// Logs in as `key`, reporting `hostname`, through the scripted provider
    /// as the identity `subject`, named `username`.
    async fn log_in_from(
        &mut self,
        relay: &Relay,
        key: &KeyPair,
        subject: &str,
        username: &str,
        hostname: &str,
    ) {
        assert_eq!(
            self.logging_in_from(relay, key, subject, username, hostname)
                .await,
            RelayMessage::LoginDone {
                account: Account {
                    provider: "scripted".to_owned(),
                    username: username.to_owned(),
                },
            }
        );
    }

    /// Logs in as `key` through the scripted provider as the identity
    /// `subject`, named `username`: how the Relay says the login ended.
    async fn logging_in(
        &mut self,
        relay: &Relay,
        key: &KeyPair,
        subject: &str,
        username: &str,
    ) -> RelayMessage {
        self.logging_in_from(relay, key, subject, username, "workstation")
            .await
    }

    async fn logging_in_from(
        &mut self,
        relay: &Relay,
        key: &KeyPair,
        subject: &str,
        username: &str,
        hostname: &str,
    ) -> RelayMessage {
        assert!(matches!(self.prove(key).await, RelayMessage::Proven { .. }));
        self.say(&ServerMessage::BeginLogin {
            hostname: hostname.to_owned(),
        })
        .await;
        let RelayMessage::LoginStarted {
            verification_uri,
            user_code,
            ..
        } = self.hear().await
        else {
            panic!("the Relay begins a login");
        };
        assert_eq!(verification_uri, SCRIPTED_VERIFICATION_URI);
        assert!(relay.provider.approve(
            &user_code,
            Identity {
                subject: subject.to_owned(),
                username: username.to_owned(),
            },
        ));
        self.hear().await
    }
}

fn key() -> KeyPair {
    KeyPair::generate().expect("generate an identity key as a Server does")
}

#[tokio::test]
async fn a_server_that_proves_its_key_logs_in_and_is_known_by_that_key_from_then_on() {
    let relay = relay().await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    client.log_in(&relay, &key, "17", "octo").await;

    let accounts = relay.running.store().accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        (accounts[0].provider.as_str(), accounts[0].subject.as_str()),
        ("scripted", "17")
    );
    let logins = relay.running.store().logins().await.unwrap();
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0].account, accounts[0].id);
    assert_eq!(
        logins[0].fingerprint,
        suru_relay_protocol::fingerprint(&key.subject_public_key_info())
    );
    assert_eq!(logins[0].hostname, "workstation");

    let mut reconnected = Client::connect(&relay).await;
    assert_eq!(
        reconnected.prove(&key).await,
        RelayMessage::Proven {
            login: Some(Account {
                provider: "scripted".to_owned(),
                username: "octo".to_owned(),
            }),
        },
        "the key alone proves the Login on every later connection"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_proof_by_another_key_is_refused_and_ends_the_connection() {
    let relay = relay().await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    client.log_in(&relay, &key, "17", "octo").await;

    let impostor = self::key();
    let mut client = Client::connect(&relay).await;
    let answer = client
        .prove_with(&key, |message| impostor.sign(message).unwrap())
        .await;
    assert!(
        matches!(
            answer,
            RelayMessage::Refused {
                refusal: Refusal::WrongProof,
                ..
            }
        ),
        "{answer:?}"
    );
    assert!(client.ended().await);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_proof_recorded_from_one_connection_proves_nothing_on_another() {
    let relay = relay().await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    client.log_in(&relay, &key, "17", "octo").await;

    let mut recorded = Vec::new();
    let mut observed = Client::connect(&relay).await;
    observed
        .prove_with(&key, |message| {
            recorded = key.sign(message).unwrap();
            recorded.clone()
        })
        .await;
    let mut replaying = Client::connect(&relay).await;
    let answer = replaying.prove_with(&key, |_| recorded.clone()).await;
    assert!(
        matches!(
            answer,
            RelayMessage::Refused {
                refusal: Refusal::WrongProof,
                ..
            }
        ),
        "each challenge is fresh, so no proof is a credential: {answer:?}"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_key_of_a_kind_the_relay_cannot_check_is_refused() {
    let relay = relay().await;
    let mut client = Client::connect(&relay).await;
    client
        .say(&ServerMessage::Hello {
            versions: SPOKEN.to_vec(),
            key: Bytes(b"no key at all".to_vec()),
        })
        .await;
    assert!(matches!(
        client.hear().await,
        RelayMessage::Refused {
            refusal: Refusal::UnsupportedKey,
            ..
        }
    ));
    assert!(client.ended().await);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_version_mismatch_is_refused_saying_which_side_is_behind() {
    let relay = relay().await;
    let Version::Unstable(spoken) = SPOKEN[0] else {
        panic!("the protocol is unstable until its first release");
    };
    for (offered, behind) in [
        (Version::Unstable(spoken - 1), Side::Server),
        (Version::Unstable(spoken + 1), Side::Relay),
        (Version::Stable(1), Side::Relay),
    ] {
        let mut client = Client::connect(&relay).await;
        client
            .say(&ServerMessage::Hello {
                versions: vec![offered],
                key: Bytes(key().subject_public_key_info()),
            })
            .await;
        let RelayMessage::Refused {
            refusal:
                Refusal::VersionNotSupported {
                    versions,
                    behind: said,
                },
            message,
        } = client.hear().await
        else {
            panic!("a Server offering only {offered} is refused");
        };
        assert_eq!(versions, SPOKEN);
        assert_eq!(said, behind, "offering {offered}");
        let named = match behind {
            Side::Server => "the Server is behind",
            Side::Relay => "the Relay is behind",
        };
        assert!(message.contains(named), "{message}");
        assert!(client.ended().await);
    }
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn what_a_later_server_says_beyond_this_protocol_is_tolerated() {
    let relay = relay().await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    client
        .say_raw(
            serde_json::json!({
                "type": "hello",
                "versions": ["unstable-999-preview", SPOKEN[0].to_string()],
                "key": Bytes(key.subject_public_key_info()),
                "capabilities": ["multiplexing"],
            })
            .to_string(),
        )
        .await;
    let RelayMessage::Challenge { nonce, .. } = client.hear().await else {
        panic!("unknown fields and versions are passed over");
    };
    client
        .say(&ServerMessage::Proof {
            signature: Bytes(
                key.sign(&proof_message(
                    PUBLIC_ADDRESS,
                    &nonce.0,
                    &key.subject_public_key_info(),
                ))
                .unwrap(),
            ),
        })
        .await;
    assert_eq!(client.hear().await, RelayMessage::Proven { login: None });

    client
        .say_raw(r#"{"type":"wait_to_be_reached","relay_id":3}"#.to_owned())
        .await;
    assert!(matches!(
        client.hear().await,
        RelayMessage::Refused {
            refusal: Refusal::Unexpected,
            ..
        }
    ));
    client.say(&ServerMessage::Forget).await;
    assert_eq!(
        client.hear().await,
        RelayMessage::Forgotten,
        "a message the Relay does not recognize is refused, and the connection goes on"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_asking_to_be_forgotten_holds_no_login_from_then_on() {
    let relay = relay().await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    client.log_in(&relay, &key, "17", "octo").await;
    client.say(&ServerMessage::Forget).await;
    assert_eq!(client.hear().await, RelayMessage::Forgotten);
    assert!(client.ended().await);

    assert!(relay.running.store().logins().await.unwrap().is_empty());
    assert_eq!(
        relay.running.store().accounts().await.unwrap().len(),
        1,
        "forgetting a Login keeps its Account"
    );
    let mut reconnected = Client::connect(&relay).await;
    assert_eq!(
        reconnected.prove(&key).await,
        RelayMessage::Proven { login: None }
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_login_refused_at_the_provider_or_left_unfinished_forms_no_login() {
    let relay = relay_with(
        ScriptedProvider::new().with_expiry(Duration::from_millis(50)),
        |config| config,
    )
    .await;
    let key = key();
    let mut client = Client::connect(&relay).await;
    assert_eq!(
        client.prove(&key).await,
        RelayMessage::Proven { login: None }
    );

    client
        .say(&ServerMessage::BeginLogin {
            hostname: "workstation".to_owned(),
        })
        .await;
    let RelayMessage::LoginStarted { user_code, .. } = client.hear().await else {
        panic!("the Relay begins a login");
    };
    assert!(relay.provider.deny(&user_code));
    assert!(matches!(
        client.hear().await,
        RelayMessage::Refused {
            refusal: Refusal::LoginDenied,
            ..
        }
    ));

    client
        .say(&ServerMessage::BeginLogin {
            hostname: "workstation".to_owned(),
        })
        .await;
    assert!(matches!(
        client.hear().await,
        RelayMessage::LoginStarted { .. }
    ));
    assert!(matches!(
        client.hear().await,
        RelayMessage::Refused {
            refusal: Refusal::LoginExpired,
            ..
        }
    ));
    assert!(relay.running.store().logins().await.unwrap().is_empty());
    assert!(relay.running.store().accounts().await.unwrap().is_empty());
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutting_the_relay_down_ends_every_connection() {
    let relay = relay().await;
    let mut client = Client::connect(&relay).await;
    assert_eq!(
        client.prove(&key()).await,
        RelayMessage::Proven { login: None }
    );
    relay.running.shutdown().await.unwrap();
    assert!(client.ended().await);
}

/// A Relay gone bad, which hands every challenge the Relay at `honest` sets
/// on to the Servers that connect to it — renamed as its own, so a Server
/// that knows it as `known_as` agrees to answer — and every answer back, as
/// live as a connection carries them.
async fn forwarding_relay(
    honest: std::net::SocketAddr,
    known_as: &'static str,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind the forwarding Relay");
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let Ok((mut relay, _)) =
                    tokio_tungstenite::connect_async(format!("ws://{honest}/connect")).await
                else {
                    return;
                };
                loop {
                    tokio::select! {
                        said = server.next() => match said {
                            Some(Ok(message)) => {
                                if relay.send(message).await.is_err() {
                                    return;
                                }
                            }
                            _ => return,
                        },
                        answered = relay.next() => match answered {
                            Some(Ok(Message::Text(text))) => {
                                let mut answer: RelayMessage =
                                    serde_json::from_str(text.as_str()).unwrap();
                                if let RelayMessage::Challenge { relay, .. } = &mut answer {
                                    *relay = known_as.to_owned();
                                }
                                let text = serde_json::to_string(&answer).unwrap();
                                if server.send(Message::Text(text.into())).await.is_err() {
                                    return;
                                }
                            }
                            Some(Ok(message)) => {
                                if server.send(message).await.is_err() {
                                    return;
                                }
                            }
                            _ => return,
                        },
                    }
                }
            });
        }
    });
    address
}

#[tokio::test]
async fn a_relay_handing_on_another_relays_challenge_gains_no_proof_it_can_use_there() {
    let honest = relay_known_as(
        "https://relay-b.example.com",
        ScriptedProvider::new(),
        |config| config,
    )
    .await;
    let victim = key();
    Client::connect(&honest)
        .await
        .log_in(&honest, &victim, "17", "octo")
        .await;
    let gone_bad = forwarding_relay(honest.running.address(), "https://relay-a.example.com").await;

    let mut victim_at_gone_bad = Client::connect_to(gone_bad, "https://relay-a.example.com").await;
    let answer = victim_at_gone_bad.prove(&victim).await;
    assert!(
        matches!(
            answer,
            RelayMessage::Refused {
                refusal: Refusal::WrongProof,
                ..
            }
        ),
        "a proof made for the Relay that handed the challenge on proves nothing at the Relay \
         that set it: {answer:?}"
    );
    assert!(victim_at_gone_bad.ended().await);
    let logins = honest.running.store().logins().await.unwrap();
    assert_eq!(
        logins.len(),
        1,
        "the victim's Login stands, neither forgotten nor moved"
    );
    assert_eq!(
        logins[0].fingerprint,
        suru_relay_protocol::fingerprint(&victim.subject_public_key_info())
    );
    assert_eq!(
        Client::connect(&honest).await.prove(&victim).await,
        RelayMessage::Proven {
            login: Some(Account {
                provider: "scripted".to_owned(),
                username: "octo".to_owned(),
            }),
        }
    );
    honest.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_that_does_not_prove_itself_in_time_is_let_go() {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_greeting_timeout(Duration::from_millis(50))
    })
    .await;
    let mut silent = Client::connect(&relay).await;
    assert!(
        silent.ended().await,
        "a Server that never says hello is let go"
    );

    let mut unanswering = Client::connect(&relay).await;
    unanswering
        .say(&ServerMessage::Hello {
            versions: SPOKEN.to_vec(),
            key: Bytes(key().subject_public_key_info()),
        })
        .await;
    assert!(matches!(
        unanswering.hear().await,
        RelayMessage::Challenge { .. }
    ));
    assert!(
        unanswering.ended().await,
        "a Server that never answers its challenge is let go"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_relay_refuses_to_start_known_by_an_address_no_relay_is_reached_at() {
    let directory = tempfile::tempdir().unwrap();
    for public_address in [
        "",
        "ftp://relay.example.com",
        "https://relay.example.com/?x=1",
    ] {
        let refused = suru_relay::start(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                directory.path().join("relay.db"),
                public_address,
            ),
            Arc::new(ScriptedProvider::new()),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("a Relay known as {public_address:?} starts"));
        assert!(
            refused.to_string().contains("public address"),
            "{refused:#}"
        );
    }
}

/// Waits until `said` stops growing: what a flooding client says no longer
/// gets through, because the Relay, stuck telling it what it does not read,
/// has stopped reading it in turn.
async fn wait_until_stalled(said: &AtomicUsize) {
    timeout(DEADLINE, async {
        let mut last = usize::MAX;
        loop {
            let now = said.load(Ordering::Acquire);
            if now == last {
                return;
            }
            last = now;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .expect("a client that reads nothing stalls");
}

#[tokio::test]
async fn a_server_that_pings_without_reading_holds_no_more_of_the_relay_than_a_bound() {
    let relay = relay().await;
    let mut client = Client::connect_reading_little(&relay).await;
    assert_eq!(
        client.prove(&key()).await,
        RelayMessage::Proven { login: None },
        "any key that proves itself gets this far, with no Account"
    );
    const PINGS: usize = 150_000;
    for _ in 0..PINGS {
        client
            .socket
            .feed(Message::Ping(vec![7; 125].into()))
            .await
            .expect("ping the Relay");
    }
    client.socket.flush().await.expect("ping the Relay");
    client
        .say_raw(r#"{"type":"later_request"}"#.to_owned())
        .await;
    let mut pongs = 0_usize;
    let answer = loop {
        let frame = timeout(DEADLINE, client.socket.next())
            .await
            .expect("the Relay answers in time")
            .expect("the Relay keeps the connection open")
            .expect("read the Relay's answer");
        match frame {
            Message::Pong(_) => pongs += 1,
            Message::Text(text) => break serde_json::from_str::<RelayMessage>(text.as_str()),
            other => panic!("the Relay answered {other:?}"),
        }
    };
    assert!(matches!(
        answer,
        Ok(RelayMessage::Refused {
            refusal: Refusal::Unexpected,
            ..
        })
    ));
    assert!(pongs > 0);
    assert!(
        pongs < PINGS,
        "a Relay that kept every answer to {PINGS} pings nothing read holds all of them: {pongs}"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_that_will_not_take_in_what_the_relay_says_is_let_go() {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_send_timeout(Duration::from_millis(100))
    })
    .await;
    let mut client = Client::connect_reading_little(&relay).await;
    assert_eq!(
        client.prove(&key()).await,
        RelayMessage::Proven { login: None }
    );
    let (_said, flooding) = client.flood();
    timeout(DEADLINE, flooding)
        .await
        .expect("the Relay lets go of a Server that takes nothing in")
        .unwrap();
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stopping_relay_lets_go_of_a_server_that_takes_nothing_in() {
    let relay = relay().await;
    let mut client = Client::connect_reading_little(&relay).await;
    assert_eq!(
        client.prove(&key()).await,
        RelayMessage::Proven { login: None }
    );
    let (said, flooding) = client.flood();
    wait_until_stalled(&said).await;

    timeout(DEADLINE, relay.running.shutdown())
        .await
        .expect("a Relay stops however stuck a connection to it is")
        .unwrap();
    timeout(DEADLINE, flooding)
        .await
        .expect("a stopped Relay holds no connection open")
        .unwrap();
}

#[tokio::test]
async fn a_stopping_relay_lets_go_of_a_server_it_is_saying_goodbye_to() {
    // Long past the test's own deadline, so a Relay that waited it out to say
    // goodbye would be seen to hang.
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_send_timeout(Duration::from_secs(120))
    })
    .await;
    let mut client = Client::connect_reading_little(&relay).await;
    assert_eq!(
        client.prove(&key()).await,
        RelayMessage::Proven { login: None }
    );
    // Pinged without reading, the Relay's answers back up until it can send
    // nothing more...
    for _ in 0..150_000 {
        client
            .socket
            .feed(Message::Ping(vec![7; 125].into()))
            .await
            .expect("ping the Relay");
    }
    client.socket.flush().await.expect("ping the Relay");
    // ...and the Server falling quiet leaves the Relay saying goodbye to a
    // connection that takes nothing in.
    let MaybeTlsStream::Plain(stream) = client.socket.get_mut() else {
        panic!("the test speaks plain WebSocket");
    };
    tokio::io::AsyncWriteExt::shutdown(stream)
        .await
        .expect("fall quiet");
    tokio::time::sleep(Duration::from_secs(1)).await;

    timeout(DEADLINE, relay.running.shutdown())
        .await
        .expect("a Relay stops even while it says goodbye to a Server that takes nothing in")
        .unwrap();
    timeout(DEADLINE, async {
        while let Some(Ok(_)) = client.socket.next().await {}
    })
    .await
    .expect("a stopped Relay holds no connection open");
}

impl Client {
    /// Logs in as `key` on a connection of its own, as the identity `subject`,
    /// named `username`.
    async fn logged_in(relay: &Relay, key: &KeyPair, subject: &str, username: &str) {
        Self::logged_in_from(relay, key, subject, username, "workstation").await;
    }

    /// Logs in as `key`, reporting `hostname`, on a connection of its own, as
    /// the identity `subject`, named `username`.
    async fn logged_in_from(
        relay: &Relay,
        key: &KeyPair,
        subject: &str,
        username: &str,
        hostname: &str,
    ) {
        let mut client = Self::connect(relay).await;
        client
            .log_in_from(relay, key, subject, username, hostname)
            .await;
    }

    /// Proves `key`, whose Login stands, and waits to be reached on this
    /// connection.
    async fn waiting(relay: &Relay, key: &KeyPair) -> Self {
        let mut client = Self::connect(relay).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { login: Some(_) }
        ));
        client.say(&ServerMessage::Wait).await;
        assert_eq!(client.hear().await, RelayMessage::Waiting);
        client
    }

    /// Proves `key` and asks to be joined to the Server whose identity key is
    /// `server`, without hearing the answer.
    async fn ask_to_join(relay: &Relay, key: &KeyPair, server: &KeyPair) -> Self {
        Self::ask_to_join_forwarded_for(relay, key, server, &[]).await
    }

    /// Asks to be joined as [`Self::ask_to_join`] does, through a reverse
    /// proxy saying it forwards for `forwarded_for`.
    async fn ask_to_join_forwarded_for(
        relay: &Relay,
        key: &KeyPair,
        server: &KeyPair,
        forwarded_for: &[&str],
    ) -> Self {
        let mut client = Self::connect_forwarded_for(relay, forwarded_for).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { .. }
        ));
        client
            .say(&ServerMessage::Join {
                server: Bytes(server.subject_public_key_info()),
            })
            .await;
        client
    }

    /// The name of the next join the Relay asks this waiting Server to take
    /// up.
    async fn reached(&mut self) -> Bytes {
        match self.hear().await {
            RelayMessage::Reach { join } => join,
            other => panic!("the Relay tells a waiting Server of a join, not {other:?}"),
        }
    }

    /// Takes up the join named `join` as `key`, on a connection of its own:
    /// what the Relay answers.
    async fn take_up(relay: &Relay, key: &KeyPair, join: Bytes) -> (Self, RelayMessage) {
        Self::take_up_forwarded_for(relay, key, join, &[]).await
    }

    /// Takes up a join as [`Self::take_up`] does, through a reverse proxy
    /// saying it forwards for `forwarded_for`.
    async fn take_up_forwarded_for(
        relay: &Relay,
        key: &KeyPair,
        join: Bytes,
        forwarded_for: &[&str],
    ) -> (Self, RelayMessage) {
        let mut client = Self::connect_forwarded_for(relay, forwarded_for).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { .. }
        ));
        client.say(&ServerMessage::Accept { join }).await;
        let answer = client.hear().await;
        (client, answer)
    }

    /// Sends `bytes` over a joined connection.
    async fn carry(&mut self, bytes: &[u8]) {
        self.socket
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .expect("send bytes over the join");
    }

    /// The next bytes carried to this side of a join.
    async fn carried(&mut self) -> Vec<u8> {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay carries bytes in time")
                .expect("the join stays open")
                .expect("read what the Relay carried");
            match frame {
                Message::Binary(bytes) => return bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("a join carries bytes alone, not {other:?}"),
            }
        }
    }
}

/// What the Relay refused, and why, where it refused.
fn refusal(answer: &RelayMessage) -> Option<&Refusal> {
    match answer {
        RelayMessage::Refused { refusal, .. } => Some(refusal),
        _ => None,
    }
}

/// Joins `joining` to the waiting `serving`, both of one Account: the two
/// ends of the join.
async fn joined(relay: &Relay, serving: &KeyPair, joining: &KeyPair) -> (Client, Client) {
    joined_forwarded_for(relay, serving, joining, &[], &[]).await
}

/// Joins `joining` to the waiting `serving` as [`joined`] does, the joining
/// Server asking through a reverse proxy saying it forwards for
/// `joining_forwarded_for`, and the serving one taking the join up through
/// one saying it forwards for `serving_forwarded_for`.
async fn joined_forwarded_for(
    relay: &Relay,
    serving: &KeyPair,
    joining: &KeyPair,
    joining_forwarded_for: &[&str],
    serving_forwarded_for: &[&str],
) -> (Client, Client) {
    let mut waiting = Client::waiting(relay, serving).await;
    let mut asking =
        Client::ask_to_join_forwarded_for(relay, joining, serving, joining_forwarded_for).await;
    let join = waiting.reached().await;
    let (taken_up, answer) =
        Client::take_up_forwarded_for(relay, serving, join, serving_forwarded_for).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    (asking, taken_up)
}

#[tokio::test]
async fn a_server_waiting_to_be_reached_is_joined_to_one_of_its_account_and_bytes_alone_pass() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;

    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let (mut taken_up, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);

    // What either side sends arrives at the other exactly as it was sent,
    // a frame of the largest size included.
    asking.carry(b"\x16\x03\x01 a ClientHello").await;
    assert_eq!(taken_up.carried().await, b"\x16\x03\x01 a ClientHello");
    let largest = vec![0xA5; suru_relay_protocol::MAX_MESSAGE_LEN];
    taken_up.carry(&largest).await;
    taken_up.carry(b"").await;
    taken_up.carry(b"and more").await;
    assert_eq!(asking.carried().await, largest);
    assert_eq!(asking.carried().await, b"");
    assert_eq!(asking.carried().await, b"and more");

    // The waiting connection goes on waiting, so a second join reaches it
    // while the first is carried.
    let mut again = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let (mut second, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(again.hear().await, RelayMessage::Joined);
    again.carry(b"second").await;
    assert_eq!(second.carried().await, b"second");
    asking.carry(b"first").await;
    assert_eq!(taken_up.carried().await, b"first");

    // Either side closing ends the join on the other.
    asking.socket.close(None).await.unwrap();
    assert!(taken_up.ended().await);
    second.socket.close(None).await.unwrap();
    assert!(again.ended().await);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_is_refused_for_each_reason_with_a_refusal_of_its_own() {
    let relay = relay().await;
    let (workstation, laptop, strangers, unknown, idle) = (key(), key(), key(), key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    Client::logged_in(&relay, &idle, "17", "octo").await;
    Client::logged_in(&relay, &strangers, "99", "someone-else").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;

    let mut asking = Client::ask_to_join(&relay, &strangers, &workstation).await;
    assert_eq!(
        refusal(&asking.hear().await),
        Some(&Refusal::DifferentAccounts),
        "the Relay joins only Servers of one Account"
    );
    let mut asking = Client::ask_to_join(&relay, &laptop, &unknown).await;
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::UnknownServer));
    let mut asking = Client::ask_to_join(&relay, &laptop, &idle).await;
    assert_eq!(
        refusal(&asking.hear().await),
        Some(&Refusal::NotWaiting),
        "a Server logged in but not waiting is not reached"
    );
    let mut asking = Client::ask_to_join(&relay, &unknown, &workstation).await;
    assert_eq!(
        refusal(&asking.hear().await),
        Some(&Refusal::LoginNeeded),
        "a Server holding no Login asks for nothing"
    );

    // A refused Server may go on asking on the same connection.
    asking.say(&ServerMessage::Forget).await;
    assert_eq!(asking.hear().await, RelayMessage::Forgotten);
    let mut nobody = Client::connect(&relay).await;
    assert_eq!(
        nobody.prove(&unknown).await,
        RelayMessage::Proven { login: None }
    );
    nobody.say(&ServerMessage::Wait).await;
    assert_eq!(
        refusal(&nobody.hear().await),
        Some(&Refusal::LoginNeeded),
        "a Server holding no Login waits for nothing"
    );

    // None of that reached the waiting Server, which is joined still.
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let (_taken_up, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_stops_waiting_once_its_waiting_connection_ends() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;
    waiting.socket.close(None).await.unwrap();
    assert!(waiting.ended().await);

    let answer = timeout(DEADLINE, async {
        loop {
            let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
            let answer = asking.hear().await;
            if refusal(&answer) == Some(&Refusal::NotWaiting) {
                return answer;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(answer.is_ok(), "the Relay forgets a Server that went");
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_the_waiting_server_does_not_take_up_in_time_is_refused_as_not_waiting() {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_join_timeout(Duration::from_millis(50))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;

    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::NotWaiting));
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "a join given up is taken up by nobody"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_is_taken_up_only_by_the_server_it_was_asked_of_and_only_once() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;

    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let (_impostor, answer) = Client::take_up(&relay, &laptop, join.clone()).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "only the Server asked takes a join up, whatever key overhears its name"
    );
    let (mut taken_up, answer) = Client::take_up(&relay, &workstation, join.clone()).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    let (_again, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(refusal(&answer), Some(&Refusal::Unexpected));

    asking.carry(b"still carried").await;
    assert_eq!(taken_up.carried().await, b"still carried");
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutting_the_relay_down_ends_every_join_it_carries() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;

    timeout(DEADLINE, relay.running.shutdown())
        .await
        .expect("a Relay stops however many joins it carries")
        .unwrap();
    assert!(asking.ended().await);
    assert!(taken_up.ended().await);
}

#[tokio::test]
async fn a_join_one_side_of_which_takes_nothing_in_is_let_go() {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_send_timeout(Duration::from_millis(100))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, "17", "octo").await;
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let mut taken_up = Client::connect_reading_little(&relay).await;
    assert!(matches!(
        taken_up.prove(&workstation).await,
        RelayMessage::Proven { .. }
    ));
    taken_up.say(&ServerMessage::Accept { join }).await;
    assert_eq!(taken_up.hear().await, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);

    // The side that reads nothing backs up what is carried to it until the
    // Relay gives the join up, ending it for the side still sending.
    let flooding = tokio::spawn(async move {
        let chunk = vec![0; 16 * 1024];
        while asking
            .socket
            .send(Message::Binary(chunk.clone().into()))
            .await
            .is_ok()
        {}
    });
    timeout(DEADLINE, flooding)
        .await
        .expect("the Relay lets go of a join one side of which takes nothing in")
        .unwrap();
    drop(taken_up);
    relay.running.shutdown().await.unwrap();
}

/// Forgets `key`'s Login on a connection of its own.
async fn forget(relay: &Relay, key: &KeyPair) {
    let mut client = Client::connect(relay).await;
    client.prove(key).await;
    client.say(&ServerMessage::Forget).await;
    assert_eq!(client.hear().await, RelayMessage::Forgotten);
}

#[tokio::test]
async fn a_join_asked_is_given_up_once_either_login_is_forgotten_before_it_is_taken_up() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;

    // The asking Server's Login is forgotten after the waiting one is told.
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    forget(&relay, &laptop).await;
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "a join whose asking Server's Login was forgotten is made for nobody"
    );
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::LoginNeeded));

    // The waiting Server's Login is forgotten after it is told; it cannot
    // take the join up even on a connection proven since.
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    forget(&relay, &workstation).await;
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(refusal(&answer), Some(&Refusal::Unexpected));
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::UnknownServer));
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_asked_is_given_up_once_either_login_moves_to_another_account() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;

    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    Client::logged_in(&relay, &laptop, "99", "someone-else").await;
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "a join between Servers no longer of one Account is made for nobody"
    );
    assert_eq!(
        refusal(&asking.hear().await),
        Some(&Refusal::DifferentAccounts)
    );

    Client::logged_in(&relay, &laptop, "17", "octo").await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    Client::logged_in(&relay, &workstation, "99", "someone-else").await;
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(refusal(&answer), Some(&Refusal::Unexpected));
    assert_eq!(
        refusal(&asking.hear().await),
        Some(&Refusal::DifferentAccounts)
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_server_waits_on_a_bounded_number_of_connections_the_oldest_giving_way() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Vec::new();
    for _ in 0..suru_relay::WAITING_CONNECTIONS_PER_SERVER {
        waiting.push(Client::waiting(&relay, &workstation).await);
    }
    // Waiting connections a Server left behind — on a network that dropped
    // them unannounced, say — cannot keep it from waiting again.
    let mut latest = Client::waiting(&relay, &workstation).await;
    assert!(
        waiting[0].ended().await,
        "the oldest waiting connection gives way"
    );

    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = latest.reached().await;
    let (_taken_up, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_waiting_server_has_a_bounded_number_of_joins_asked_of_it_at_once() {
    // No join is given up for want of being taken up while the test runs.
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_join_timeout(Duration::from_secs(600))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asked = Vec::new();
    let mut joins = Vec::new();
    for _ in 0..suru_relay::JOINS_ASKED_PER_SERVER {
        asked.push(Client::ask_to_join(&relay, &laptop, &workstation).await);
        joins.push(waiting.reached().await);
    }

    let mut one_too_many = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(
        refusal(&one_too_many.hear().await),
        Some(&Refusal::NotWaiting),
        "a Server with as many joins asked of it as it may have is not waiting for more"
    );

    // One taken up makes room for another.
    let (_taken_up, answer) = Client::take_up(&relay, &workstation, joins.remove(0)).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asked[0].hear().await, RelayMessage::Joined);
    let _room = Client::ask_to_join(&relay, &laptop, &workstation).await;
    waiting.reached().await;
    relay.running.shutdown().await.unwrap();
}

/// Where a Relay writes its connection log in these tests: each write handed
/// on as it is made.
struct LogWriter(mpsc::UnboundedSender<Vec<u8>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a Relay writes to its connection log, read a line at a time.
struct ConnectionLog {
    written: mpsc::UnboundedReceiver<Vec<u8>>,
    unread: Vec<u8>,
}

impl ConnectionLog {
    /// A log to read, and where a Relay writes it.
    fn new() -> (LogWriter, Self) {
        let (writer, written) = mpsc::unbounded_channel();
        (
            LogWriter(writer),
            Self {
                written,
                unread: Vec::new(),
            },
        )
    }

    /// Every line the Relay has written and the test has yet to read, read
    /// as JSON, without waiting: a Relay that has stopped has written every
    /// line it will.
    fn remaining(&mut self) -> Vec<serde_json::Value> {
        loop {
            match self.written.try_recv() {
                Ok(bytes) => self.unread.extend(bytes),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {
                    panic!("the Relay stopped before its connection log had let go")
                }
            }
        }
        let unread = std::mem::take(&mut self.unread);
        let lines = unread.strip_suffix(b"\n").unwrap_or(&unread);
        assert!(
            unread.is_empty() || unread.ends_with(b"\n"),
            "the log ends on a whole line"
        );
        lines
            .split(|&byte| byte == b'\n')
            .filter(|line| !unread.is_empty() || !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("each line is one JSON object"))
            .collect()
    }

    /// The next line the Relay writes, read as JSON; `None` once the Relay
    /// has stopped, having written no more.
    async fn line(&mut self) -> Option<serde_json::Value> {
        loop {
            if let Some(end) = self.unread.iter().position(|&byte| byte == b'\n') {
                let line = self.unread.drain(..=end).collect::<Vec<_>>();
                return Some(
                    serde_json::from_slice(&line).expect("each line of the log is one JSON object"),
                );
            }
            match timeout(DEADLINE, self.written.recv())
                .await
                .expect("the Relay writes its connection log in time")
            {
                Some(bytes) => self.unread.extend(bytes),
                None => {
                    assert!(
                        self.unread.is_empty(),
                        "the log ends on a whole line: {}",
                        String::from_utf8_lossy(&self.unread)
                    );
                    return None;
                }
            }
        }
    }
}

/// The fingerprint a Server's identity key is known by.
fn fingerprint(key: &KeyPair) -> String {
    suru_relay_protocol::fingerprint(&key.subject_public_key_info())
}

/// The network addresses a line names for the joining Server and for the
/// serving one.
fn addresses(line: &serde_json::Value) -> (&str, &str) {
    (
        line["joining"]["address"].as_str().unwrap(),
        line["serving"]["address"].as_str().unwrap(),
    )
}

/// The bytes a line says the joining Server and the serving one each sent.
fn bytes_sent(line: &serde_json::Value) -> (u64, u64) {
    (
        line["joining"]["bytes_sent"].as_u64().unwrap(),
        line["serving"]["bytes_sent"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn each_joined_connection_is_logged_once_it_ends_naming_who_connected_what_to_what() {
    let (writer, mut log) = ConnectionLog::new();
    let now = Arc::new(std::sync::Mutex::new(
        UNIX_EPOCH + Duration::from_millis(1_790_000_000_123),
    ));
    let clock = {
        let now = now.clone();
        Clock::from_fn(move || *now.lock().unwrap())
    };
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_connection_log(writer).with_clock(clock)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in_from(&relay, &workstation, "583231", "octocat", "workstation").await;
    Client::logged_in_from(&relay, &laptop, "583231", "octocat", "laptop").await;
    let account = relay.running.store().accounts().await.unwrap()[0].id;

    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(&[1; 300]).await;
    asking.carry(&[2; 5]).await;
    taken_up.carry(&[3; 70]).await;
    assert_eq!(taken_up.carried().await.len(), 300);
    assert_eq!(taken_up.carried().await.len(), 5);
    assert_eq!(asking.carried().await.len(), 70);
    *now.lock().unwrap() = UNIX_EPOCH + Duration::from_millis(1_790_000_754_456);
    asking.socket.close(None).await.unwrap();

    assert_eq!(
        log.line().await,
        Some(serde_json::json!({
            "event": "joined_connection",
            "start": "2026-09-21T14:13:20.123Z",
            "end": "2026-09-21T14:25:54.456Z",
            "account": {
                "id": account,
                "provider": "scripted",
                "subject": "583231",
                "username": "octocat",
            },
            "joining": {
                "fingerprint": fingerprint(&laptop),
                "hostname": "laptop",
                "address": "127.0.0.1",
                "bytes_sent": 305,
            },
            "serving": {
                "fingerprint": fingerprint(&workstation),
                "hostname": "workstation",
                "address": "127.0.0.1",
                "bytes_sent": 70,
            },
        }))
    );
    assert!(taken_up.ended().await);
    relay.running.shutdown().await.unwrap();
    assert_eq!(
        log.remaining(),
        Vec::<serde_json::Value>::new(),
        "a joined connection is logged once"
    );
}

#[tokio::test]
async fn a_joined_connection_is_logged_once_however_it_ends() {
    let (writer, mut log) = ConnectionLog::new();
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config
            .with_connection_log(writer)
            .with_send_timeout(Duration::from_millis(100))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }

    // The Server joined to closes.
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    taken_up.carry(b"bye").await;
    assert_eq!(asking.carried().await, b"bye");
    taken_up.socket.close(None).await.unwrap();
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (0, 3));
    assert!(asking.ended().await);

    // One side takes in nothing carried to it, until the Relay lets the join
    // go.
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let mut unread = Client::connect_reading_little(&relay).await;
    assert!(matches!(
        unread.prove(&workstation).await,
        RelayMessage::Proven { .. }
    ));
    unread.say(&ServerMessage::Accept { join }).await;
    assert_eq!(unread.hear().await, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    let flooding = tokio::spawn(async move {
        let chunk = vec![0; 16 * 1024];
        while asking
            .socket
            .send(Message::Binary(chunk.clone().into()))
            .await
            .is_ok()
        {}
    });
    assert_eq!(bytes_sent(&log.line().await.unwrap()).1, 0);
    timeout(DEADLINE, flooding)
        .await
        .expect("the Relay lets go of a join one side of which takes nothing in")
        .unwrap();
    drop(unread);

    // The Relay stops while it carries a join.
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(b"one").await;
    assert_eq!(taken_up.carried().await, b"one");
    taken_up.carry(b"four").await;
    assert_eq!(asking.carried().await, b"four");
    relay.running.shutdown().await.unwrap();
    let written = log.remaining();
    assert_eq!(
        written.len(),
        1,
        "each joined connection is logged once, before the Relay has stopped"
    );
    assert_eq!(bytes_sent(&written[0]), (3, 4));
}

/// The line a Relay configured by `configure` writes for one connection it
/// joins, the joining Server asking through a reverse proxy saying it
/// forwards for `joining_forwarded_for`, and the serving one taking the join
/// up through one saying it forwards for `serving_forwarded_for`.
async fn logged_join(
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
    joining_forwarded_for: &[&str],
    serving_forwarded_for: &[&str],
) -> serde_json::Value {
    let (writer, mut log) = ConnectionLog::new();
    let relay = relay_with(ScriptedProvider::new(), |config| {
        configure(config.with_connection_log(writer))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let (mut asking, _taken_up) = joined_forwarded_for(
        &relay,
        &workstation,
        &laptop,
        joining_forwarded_for,
        serving_forwarded_for,
    )
    .await;
    asking.socket.close(None).await.unwrap();
    let line = log.line().await.expect("the join is logged");
    relay.running.shutdown().await.unwrap();
    line
}

fn proxies(named: &[&str]) -> Vec<TrustedProxy> {
    named.iter().map(|proxy| proxy.parse().unwrap()).collect()
}

#[tokio::test]
async fn a_forwarded_address_is_believed_only_from_a_proxy_the_operator_names() {
    let line = logged_join(|config| config, &["203.0.113.7"], &["198.51.100.2"]).await;
    assert_eq!(
        addresses(&line),
        ("127.0.0.1", "127.0.0.1"),
        "no proxy is believed unless named"
    );

    let line = logged_join(
        |config| config.with_trusted_proxies(proxies(&["192.0.2.1", "10.0.0.0/8"])),
        &["203.0.113.7"],
        &["198.51.100.2"],
    )
    .await;
    assert_eq!(
        addresses(&line),
        ("127.0.0.1", "127.0.0.1"),
        "a header from anywhere but a named proxy is ignored"
    );

    // Each named proxy is believed about the address it was reached from,
    // so what lies beyond the nearest address no named proxy is at — written
    // by the Server itself, say — is passed over.
    let line = logged_join(
        |config| config.with_trusted_proxies(proxies(&["127.0.0.1", "10.0.0.0/8"])),
        &["198.51.100.9, 203.0.113.7"],
        &["192.0.2.1", "[2001:db8::2]:4711, 10.1.2.3"],
    )
    .await;
    assert_eq!(addresses(&line), ("203.0.113.7", "2001:db8::2"));
}

/// The Relay's database as it stands on disk: each of its files but SQLite's
/// shared-memory index, which reading alone changes.
fn database_files(relay: &Relay) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(relay.directory.path())
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.ends_with("-shm"))
        .map(|name| {
            let contents = std::fs::read(relay.directory.path().join(&name)).unwrap();
            (name, contents)
        })
        .collect()
}

#[tokio::test]
async fn the_relay_keeps_no_history_of_connections_in_its_database() {
    let (writer, mut log) = ConnectionLog::new();
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_connection_log(writer)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let before = database_files(&relay);

    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(b"carried").await;
    assert_eq!(taken_up.carried().await, b"carried");
    taken_up.socket.close(None).await.unwrap();
    log.line().await.expect("the join is logged");
    assert!(asking.ended().await);
    assert!(
        database_files(&relay) == before,
        "a joined connection leaves nothing in the Relay's database"
    );
    relay.running.shutdown().await.unwrap();
}

/// What the Relay's diagnostic log writes, kept.
#[derive(Clone, Default)]
struct Diagnostics(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Diagnostics {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn nothing_a_joined_connection_carried_appears_in_either_log() {
    let diagnostics = Diagnostics::default();
    let diagnostic_writer = diagnostics.clone();
    let _logging = tracing::subscriber::set_default(tracing_subscriber::Registry::default().with(
        suru_relay::log_layer(Some("trace"), move || diagnostic_writer.clone()),
    ));
    let (writer, mut log) = ConnectionLog::new();
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_connection_log(writer)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }

    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let (mut taken_up, answer) = Client::take_up(&relay, &workstation, join.clone()).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    let (asked, answered) = (
        b"CARRIED-ASKED-6b1f0c".as_slice(),
        b"CARRIED-ANSWERED-93ce7d".as_slice(),
    );
    asking.carry(asked).await;
    assert_eq!(taken_up.carried().await, asked);
    taken_up.carry(answered).await;
    assert_eq!(asking.carried().await, answered);
    asking.socket.close(None).await.unwrap();
    let line = log.line().await.expect("the join is logged").to_string();
    relay.running.shutdown().await.unwrap();

    let diagnostics = String::from_utf8_lossy(&diagnostics.0.lock().unwrap()).into_owned();
    assert!(
        diagnostics.contains("Relay ready"),
        "the diagnostic log is read: {diagnostics}"
    );
    let join = serde_json::to_value(&join).unwrap();
    for (written, log) in [(&line, "connection"), (&diagnostics, "diagnostic")] {
        for carried in [asked, answered] {
            let hex = carried
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let listed = format!("{:?}", &carried[..8]);
            for form in [
                String::from_utf8_lossy(carried).into_owned(),
                hex,
                listed.trim_matches(['[', ']']).to_owned(),
            ] {
                assert!(
                    !written.contains(&form),
                    "{form} reached the {log} log: {written}"
                );
            }
        }
        assert!(
            !written.contains(join.as_str().unwrap()),
            "the join's name reached the {log} log: {written}"
        );
    }
}

/// A line of the Relay's diagnostic log without its terminal colours.
fn plain(line: &str) -> String {
    let mut plain = String::new();
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // An escape sequence runs to its final letter.
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}

/// Whether `time` is written as the connection log writes its times: RFC
/// 3339, in UTC, to the millisecond.
fn is_timestamp(time: &serde_json::Value) -> bool {
    time.as_str().is_some_and(|time| {
        time.len() == 24
            && time.char_indices().all(|(at, character)| match at {
                4 | 7 => character == '-',
                10 => character == 'T',
                13 | 16 => character == ':',
                19 => character == '.',
                23 => character == 'Z',
                _ => character.is_ascii_digit(),
            })
    })
}

/// Runs the Relay binary on the records at `database`, with `arguments`
/// besides those it needs, and joins `joining` to `serving` through it, each
/// connecting through a reverse proxy saying it forwards for an address of
/// its own: everything the binary writes to standard output, read as JSON
/// lines, until it is stopped once the join's line is written.
async fn written_by_the_binary(
    database: &std::path::Path,
    arguments: &[&str],
    serving: &KeyPair,
    joining: &KeyPair,
) -> Vec<serde_json::Value> {
    let mut binary = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"))
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--database")
        .arg(database)
        .arg("--public-address")
        .arg(PUBLIC_ADDRESS)
        .args(arguments)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("run the Relay");
    let mut diagnostics = BufReader::new(binary.stderr.take().unwrap()).lines();
    let address: std::net::SocketAddr = timeout(DEADLINE, async {
        loop {
            let line = diagnostics
                .next_line()
                .await
                .unwrap()
                .expect("the Relay says where it listens");
            let line = plain(&line);
            if line.contains("Relay ready")
                && let Some((_, address)) = line.split_once("address=")
            {
                return address.trim().parse().unwrap();
            }
        }
    })
    .await
    .expect("the Relay is ready in time");
    // The Relay never waits on a full pipe to write its diagnostics.
    tokio::spawn(async move { while let Ok(Some(_)) = diagnostics.next_line().await {} });

    let mut waiting = Client::connect_to(address, PUBLIC_ADDRESS).await;
    assert!(matches!(
        waiting.prove(serving).await,
        RelayMessage::Proven { login: Some(_) }
    ));
    waiting.say(&ServerMessage::Wait).await;
    assert_eq!(waiting.hear().await, RelayMessage::Waiting);
    let mut asking = Client::connect_forwarded_to(address, PUBLIC_ADDRESS, &["203.0.113.7"]).await;
    assert!(matches!(
        asking.prove(joining).await,
        RelayMessage::Proven { login: Some(_) }
    ));
    asking
        .say(&ServerMessage::Join {
            server: Bytes(serving.subject_public_key_info()),
        })
        .await;
    let join = waiting.reached().await;
    let mut taken_up =
        Client::connect_forwarded_to(address, PUBLIC_ADDRESS, &["198.51.100.2"]).await;
    assert!(matches!(
        taken_up.prove(serving).await,
        RelayMessage::Proven { .. }
    ));
    taken_up.say(&ServerMessage::Accept { join }).await;
    assert_eq!(taken_up.hear().await, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    asking.carry(b"carried").await;
    assert_eq!(taken_up.carried().await, b"carried");
    taken_up.carry(b"ack").await;
    assert_eq!(asking.carried().await, b"ack");
    asking.socket.close(None).await.unwrap();

    let mut written = BufReader::new(binary.stdout.take().unwrap()).lines();
    let mut lines = vec![
        timeout(DEADLINE, written.next_line())
            .await
            .expect("the Relay logs the join in time")
            .unwrap()
            .expect("the Relay logs the join"),
    ];
    binary.kill().await.unwrap();
    while let Some(line) = written.next_line().await.unwrap() {
        lines.push(line);
    }
    lines
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .expect("the Relay writes nothing to standard output but its connection log")
        })
        .collect()
}

#[tokio::test]
async fn the_relay_binary_logs_to_standard_output_believing_forwarded_addresses_from_named_proxies_alone()
 {
    // Both Servers log in at a Relay keeping its records where the binary
    // will keep them.
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in_from(&relay, &workstation, "583231", "octocat", "workstation").await;
    Client::logged_in_from(&relay, &laptop, "583231", "octocat", "laptop").await;
    let account = relay.running.store().accounts().await.unwrap()[0].id;
    let Relay {
        directory, running, ..
    } = relay;
    running.shutdown().await.unwrap();
    let database = directory.path().join("relay.db");

    for (arguments, (joining, serving), why) in [
        (
            &[][..],
            ("127.0.0.1", "127.0.0.1"),
            "no proxy is believed unless named",
        ),
        (
            &[
                "--trusted-proxy",
                "192.0.2.1",
                "--trusted-proxy",
                "10.0.0.0/8",
            ],
            ("127.0.0.1", "127.0.0.1"),
            "a header from anywhere but a named proxy is ignored",
        ),
        (
            &[
                "--trusted-proxy",
                "192.0.2.1",
                "--trusted-proxy",
                "127.0.0.1",
            ],
            ("203.0.113.7", "198.51.100.2"),
            "a named proxy is believed",
        ),
    ] {
        let mut written = written_by_the_binary(&database, arguments, &workstation, &laptop).await;
        assert_eq!(written.len(), 1, "one line for the one join: {written:?}");
        let mut line = written.remove(0);
        let line = line.as_object_mut().unwrap();
        let (start, end) = (line.remove("start").unwrap(), line.remove("end").unwrap());
        assert!(is_timestamp(&start) && is_timestamp(&end), "{start} {end}");
        assert!(start.as_str() <= end.as_str(), "{start} {end}");
        assert_eq!(
            serde_json::Value::Object(line.clone()),
            serde_json::json!({
                "event": "joined_connection",
                "account": {
                    "id": account,
                    "provider": "scripted",
                    "subject": "583231",
                    "username": "octocat",
                },
                "joining": {
                    "fingerprint": fingerprint(&laptop),
                    "hostname": "laptop",
                    "address": joining,
                    "bytes_sent": 7,
                },
                "serving": {
                    "fingerprint": fingerprint(&workstation),
                    "hostname": "workstation",
                    "address": serving,
                    "bytes_sent": 3,
                },
            }),
            "{why}"
        );
    }
}

#[tokio::test]
async fn a_frame_passed_on_as_a_join_closes_is_counted_and_the_join_ends_once_it_has_gone() {
    let (writer, mut log) = ConnectionLog::new();
    let now = Arc::new(std::sync::Mutex::new(
        UNIX_EPOCH + Duration::from_millis(1_790_000_000_123),
    ));
    let clock = {
        let now = now.clone();
        Clock::from_fn(move || *now.lock().unwrap())
    };
    // The Relay waits as long as the test needs on a Server that takes
    // nothing in.
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config
            .with_connection_log(writer)
            .with_clock(clock)
            .with_send_timeout(Duration::from_secs(600))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;
    let mut stalled = Client::connect_reading_little(&relay).await;
    assert!(matches!(
        stalled.prove(&workstation).await,
        RelayMessage::Proven { .. }
    ));
    stalled.say(&ServerMessage::Accept { join }).await;
    assert_eq!(stalled.hear().await, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);

    // The joining Server sends more than the serving one takes in, until
    // the Relay, waiting to pass a frame on, stops taking in more.
    let said = Arc::new(AtomicUsize::new(0));
    let (mut sending, mut hearing) = asking.socket.split();
    let flooding = tokio::spawn({
        let said = said.clone();
        async move {
            let chunk = vec![7; 16 * 1024];
            while sending
                .send(Message::Binary(chunk.clone().into()))
                .await
                .is_ok()
            {
                said.fetch_add(1, Ordering::AcqRel);
            }
        }
    });
    wait_until_stalled(&said).await;

    // The serving Server, still taking nothing in, closes the join; the
    // Relay closes both sides, telling the joining Server so.
    stalled.socket.send(Message::Close(None)).await.unwrap();
    timeout(DEADLINE, async {
        while let Some(Ok(heard)) = hearing.next().await {
            if let Message::Close(_) = heard {
                return;
            }
        }
    })
    .await
    .expect("the Relay closes the join");

    // As the Relay closes it, the serving Server takes in everything it was
    // sent, the frame the Relay was waiting to pass on among it.
    *now.lock().unwrap() = UNIX_EPOCH + Duration::from_millis(1_790_000_754_456);
    let delivered = timeout(DEADLINE, async {
        let mut delivered = 0;
        loop {
            match stalled.socket.next().await {
                Some(Ok(Message::Binary(bytes))) => delivered += bytes.len(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return delivered,
            }
        }
    })
    .await
    .expect("the stalled Server takes in what it was sent");
    let line = log.line().await.expect("the join is logged");
    assert_eq!(
        bytes_sent(&line),
        (u64::try_from(delivered).unwrap(), 0),
        "every whole frame passed on is counted"
    );
    assert_eq!(
        line["end"], "2026-09-21T14:25:54.456Z",
        "the join ends once what it carried has gone"
    );
    timeout(DEADLINE, flooding).await.unwrap().unwrap();
    relay.running.shutdown().await.unwrap();
}

/// A connection log's reader, which a test can have stop taking lines in and
/// later go on.
#[derive(Clone)]
struct Gate(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

impl Gate {
    fn shut() -> Self {
        Self(Arc::new((
            std::sync::Mutex::new(false),
            std::sync::Condvar::new(),
        )))
    }

    fn opened() -> Self {
        let gate = Self::shut();
        gate.open();
        gate
    }

    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    fn close(&self) {
        *self.0.0.lock().unwrap() = false;
    }

    /// Returns once the gate is open.
    fn pass(&self) {
        let (open, opened) = &*self.0;
        let _open = opened
            .wait_while(open.lock().unwrap(), |open| !*open)
            .unwrap();
    }
}

/// Where a Relay writes its connection log through a reader that takes
/// nothing in while its gate is shut, saying each time it is written to.
struct GatedWriter {
    gate: Gate,
    writing: mpsc::UnboundedSender<()>,
    written: LogWriter,
}

impl std::io::Write for GatedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.writing.send(());
        self.gate.pass();
        self.written.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A connection log written through a reader that takes nothing in until
/// its gate opens: where a Relay writes it, what tells the test of each
/// write the Relay begins, and the log itself.
fn gated_connection_log(gate: &Gate) -> (GatedWriter, mpsc::UnboundedReceiver<()>, ConnectionLog) {
    let (written, log) = ConnectionLog::new();
    let (writing, writes) = mpsc::unbounded_channel();
    (
        GatedWriter {
            gate: gate.clone(),
            writing,
            written,
        },
        writes,
        log,
    )
}

/// Joins `joining` to `serving`, carries `bytes` from the joining Server,
/// and ends the join.
async fn join_carrying(relay: &Relay, serving: &KeyPair, joining: &KeyPair, bytes: &[u8]) {
    let (mut asking, mut taken_up) = joined(relay, serving, joining).await;
    asking.carry(bytes).await;
    assert_eq!(taken_up.carried().await, bytes);
    asking.socket.close(None).await.unwrap();
    assert!(taken_up.ended().await);
}

#[tokio::test]
async fn a_connection_log_whose_reader_stops_holds_up_nothing_but_joins_it_has_no_room_to_record() {
    let gate = Gate::shut();
    let (writer, mut writes, mut log) = gated_connection_log(&gate);
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config
            .with_connection_log(writer)
            .with_connection_log_capacity(2)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }

    // The log's reader stops taking in the first line, and the log has room
    // for two more: one joined connection ended, and one carried.
    join_carrying(&relay, &workstation, &laptop, &[1]).await;
    timeout(DEADLINE, writes.recv())
        .await
        .expect("the Relay writes the first line in time");
    join_carrying(&relay, &workstation, &laptop, &[2; 2]).await;
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;

    // A join it could not record is refused, as such, and the Server may ask
    // again.
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut refused = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(
        refusal(&refused.hear().await),
        Some(&Refusal::Unavailable),
        "a Relay joins no connection it has no room to record"
    );

    // Everything else goes on: the join carried, and logins.
    asking.carry(&[3; 3]).await;
    assert_eq!(taken_up.carried().await, [3; 3]);
    taken_up.carry(b"back").await;
    assert_eq!(asking.carried().await, b"back");
    Client::logged_in(&relay, &key(), "99", "someone-else").await;
    assert!(
        log.written.try_recv().is_err(),
        "nothing is written meanwhile"
    );

    // Once its reader goes on, the log writes what it held back, in order,
    // and has room again.
    gate.open();
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (1, 0));
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (2, 0));
    refused
        .say(&ServerMessage::Join {
            server: Bytes(workstation.subject_public_key_info()),
        })
        .await;
    let join = waiting.reached().await;
    let (mut taken_up_again, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(refused.hear().await, RelayMessage::Joined);
    refused.carry(&[4; 4]).await;
    assert_eq!(taken_up_again.carried().await, [4; 4]);
    refused.socket.close(None).await.unwrap();
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (4, 0));
    asking.socket.close(None).await.unwrap();
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (3, 4));
    relay.running.shutdown().await.unwrap();
    assert_eq!(
        log.remaining(),
        Vec::<serde_json::Value>::new(),
        "each joined connection is logged once"
    );
}

#[tokio::test]
async fn a_stopping_relay_waits_on_a_connection_log_that_takes_nothing_in_no_longer_than_its_bound()
{
    let gate = Gate::shut();
    let (writer, mut writes, _log) = gated_connection_log(&gate);
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config
            .with_connection_log(writer)
            .with_drain_timeout(Duration::from_millis(50))
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    join_carrying(&relay, &workstation, &laptop, b"held").await;
    timeout(DEADLINE, writes.recv())
        .await
        .expect("the Relay writes the line in time");
    join_carrying(&relay, &workstation, &laptop, b"queued").await;

    timeout(DEADLINE, relay.running.shutdown())
        .await
        .expect("a Relay stops though its connection log takes nothing in")
        .unwrap();
    gate.open();
}

/// Where a Relay writes its connection log through a reader that takes in
/// the first few bytes of the first line and then has gone, keeping all it
/// was given and counting every write after it went.
#[derive(Clone, Default)]
struct BrokenWriter {
    taken: Arc<std::sync::Mutex<Vec<u8>>>,
    after: Arc<AtomicUsize>,
}

impl std::io::Write for BrokenWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut taken = self.taken.lock().unwrap();
        if taken.is_empty() {
            taken.extend_from_slice(&bytes[..10]);
            return Ok(10);
        }
        self.after.fetch_add(1, Ordering::AcqRel);
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether the diagnostic log has come to say `needle`, waiting until it
/// does.
async fn diagnosed(diagnostics: &Diagnostics, needle: &str) {
    timeout(DEADLINE, async {
        while !String::from_utf8_lossy(&diagnostics.0.lock().unwrap()).contains(needle) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the diagnostic log says {needle}"));
}

/// Has a Relay write its connection log to `writer`, which cannot write the
/// first line it is given, and shows the failure and the lines the log owes
/// go to the diagnostic log, and that the Relay joins nothing more.
async fn a_relay_whose_connection_log_fails(writer: impl std::io::Write + Send + 'static) {
    let diagnostics = Diagnostics::default();
    let diagnostic_writer = diagnostics.clone();
    let _logging = tracing::subscriber::set_default(tracing_subscriber::Registry::default().with(
        suru_relay::log_layer(Some("info"), move || diagnostic_writer.clone()),
    ));
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_connection_log(writer)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }

    // The first line cannot be written: the failure, and the line, go to the
    // diagnostic log.
    let (mut carried_asking, _carried) = joined(&relay, &workstation, &laptop).await;
    join_carrying(&relay, &workstation, &laptop, &[1; 11]).await;
    diagnosed(&diagnostics, "could not be written").await;
    diagnosed(&diagnostics, r#""bytes_sent":11"#).await;

    // From then on the Relay joins nothing it could not record.
    let _waiting = Client::waiting(&relay, &workstation).await;
    let mut refused = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(refusal(&refused.hear().await), Some(&Refusal::Unavailable));

    // A join carried as the log failed is recorded in the diagnostic log as
    // it ends.
    carried_asking.carry(&[2; 22]).await;
    carried_asking.socket.close(None).await.unwrap();
    diagnosed(&diagnostics, r#""bytes_sent":22"#).await;
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_connection_log_that_cannot_be_written_stops_joins_and_leaves_its_lines_in_the_diagnostic_log()
 {
    let broken = BrokenWriter::default();
    a_relay_whose_connection_log_fails(broken.clone()).await;
    assert_eq!(
        broken.after.load(Ordering::Acquire),
        1,
        "nothing is written after a line the log could not write"
    );
    assert_eq!(broken.taken.lock().unwrap().len(), 10);
}

/// Where a Relay writes its connection log through a reader that panics at
/// the first line it is given.
struct PanickingWriter;

impl std::io::Write for PanickingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        panic!("the connection log's reader gives way");
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_connection_log_whose_writer_panics_stops_joins_and_leaves_its_lines_in_the_diagnostic_log()
 {
    a_relay_whose_connection_log_fails(PanickingWriter).await;
}

/// A diagnostic log whose reader takes nothing in while its gate is shut,
/// saying each time it is written to.
#[derive(Clone)]
struct GatedDiagnostics {
    gate: Gate,
    writing: mpsc::UnboundedSender<()>,
    kept: Diagnostics,
}

impl std::io::Write for GatedDiagnostics {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.writing.send(());
        self.gate.pass();
        self.kept.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_stopping_relay_waits_no_longer_than_its_bound_though_neither_of_its_logs_is_read() {
    for connection_log_fails in [false, true] {
        let kept = Diagnostics::default();
        let diagnostic_gate = Gate::opened();
        let (writing, mut diagnostic_writes) = mpsc::unbounded_channel();
        let diagnostics = GatedDiagnostics {
            gate: diagnostic_gate.clone(),
            writing,
            kept: kept.clone(),
        };
        let _logging =
            tracing::subscriber::set_default(tracing_subscriber::Registry::default().with(
                suru_relay::log_layer(Some("info"), move || diagnostics.clone()),
            ));
        let connection_gate = Gate::shut();
        let (writer, mut writes, _log) = gated_connection_log(&connection_gate);
        let relay = relay_with(ScriptedProvider::new(), |config| {
            let config = config.with_drain_timeout(Duration::from_millis(50));
            if connection_log_fails {
                config.with_connection_log(BrokenWriter::default())
            } else {
                config.with_connection_log(writer)
            }
        })
        .await;
        let (workstation, laptop) = (key(), key());
        for key in [&workstation, &laptop] {
            Client::logged_in(&relay, key, "17", "octo").await;
        }

        // From here the diagnostic log's reader takes nothing in either, and
        // the connection log's writer is held up: on the connection log, or
        // on the diagnostic log once the connection log has failed.
        diagnostic_gate.close();
        while diagnostic_writes.try_recv().is_ok() {}
        join_carrying(&relay, &workstation, &laptop, b"held").await;
        let held_up = if connection_log_fails {
            timeout(DEADLINE, diagnostic_writes.recv()).await
        } else {
            timeout(DEADLINE, writes.recv()).await
        };
        held_up.expect("the connection log's writer is held up in time");

        timeout(DEADLINE, relay.running.shutdown())
            .await
            .expect("a Relay stops though neither of its logs is read")
            .unwrap();

        // Once the diagnostic log is read again it says what the Relay gave
        // up on.
        diagnostic_gate.open();
        connection_gate.open();
        diagnosed(&kept, "unwritten").await;
    }
}

impl Client {
    /// Proves `key` on a connection the test reads nothing more of, and
    /// fills it with the Relay's answers to pings, so whatever the Relay says
    /// on it next waits on the test.
    async fn backed_up(relay: &Relay, key: &KeyPair) -> Self {
        let mut client = Self::connect_reading_little(relay).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { .. }
        ));
        for _ in 0..150_000 {
            client
                .socket
                .feed(Message::Ping(vec![7; 125].into()))
                .await
                .expect("ping the Relay");
        }
        client.socket.flush().await.expect("ping the Relay");
        client
    }
}

/// A Relay whose connection log has room for one line, and which waits on a
/// Server that takes nothing in for longer than any test runs, with
/// `joining` and `serving` logged in under one Account and `serving` waiting
/// on what this answers.
async fn relay_with_room_for_one(
    serving: &KeyPair,
    joining: &KeyPair,
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> (Relay, Client) {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        configure(
            config
                .with_connection_log_capacity(1)
                .with_send_timeout(Duration::from_secs(600)),
        )
    })
    .await;
    for key in [serving, joining] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let waiting = Client::waiting(&relay, serving).await;
    (relay, waiting)
}

/// Joins `joining` to `serving`, which waits on `waiting`, asking again for
/// as long as the Relay has no room to record the join or it is not taken up
/// in time: the two ends of the join.
async fn joined_once_there_is_room(
    relay: &Relay,
    waiting: &mut Client,
    serving: &KeyPair,
    joining: &KeyPair,
) -> (Client, Client) {
    timeout(DEADLINE, async {
        loop {
            let mut asking = Client::ask_to_join(relay, joining, serving).await;
            tokio::select! {
                answer = asking.hear() => match refusal(&answer) {
                    Some(Refusal::Unavailable | Refusal::NotWaiting) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    _ => panic!("the Relay answered {answer:?}"),
                },
                join = waiting.reached() => {
                    let (taken_up, answer) = Client::take_up(relay, serving, join).await;
                    if answer == RelayMessage::Joined {
                        assert_eq!(asking.hear().await, RelayMessage::Joined);
                        return (asking, taken_up);
                    }
                }
            }
        }
    })
    .await
    .expect("the Relay has room to join and record a connection")
}

#[tokio::test]
async fn joins_asked_by_a_server_holding_no_login_hold_no_room_in_the_connection_log() {
    let (workstation, laptop) = (key(), key());
    let (relay, mut waiting) = relay_with_room_for_one(&workstation, &laptop, |config| {
        config.with_connection_log(std::io::sink())
    })
    .await;

    // A Server holding no Login asks to be joined, over and over, reading
    // none of the refusals, until the Relay waits on it to take one in.
    let stranger = Client::connect_reading_little(&relay).await;
    let mut stranger = stranger;
    assert_eq!(
        stranger.prove(&key()).await,
        RelayMessage::Proven { login: None }
    );
    let (said, _flooding) = stranger.flood_with(
        serde_json::to_string(&ServerMessage::Join {
            server: Bytes(workstation.subject_public_key_info()),
        })
        .unwrap(),
    );
    wait_until_stalled(&said).await;

    joined_once_there_is_room(&relay, &mut waiting, &workstation, &laptop).await;
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_its_server_abandons_gives_its_room_back_at_once() {
    let (workstation, laptop, tablet) = (key(), key(), key());
    let (relay, mut waiting) = relay_with_room_for_one(&workstation, &laptop, |config| {
        config.with_connection_log(std::io::sink())
    })
    .await;
    Client::logged_in(&relay, &tablet, "17", "octo").await;

    // A Server whose connection is full asks to be joined, and then, its
    // join asked, says something else instead of waiting for it.
    let mut abandoning = Client::backed_up(&relay, &tablet).await;
    abandoning
        .say(&ServerMessage::Join {
            server: Bytes(workstation.subject_public_key_info()),
        })
        .await;
    waiting.reached().await;
    abandoning.say(&ServerMessage::Forget).await;

    joined_once_there_is_room(&relay, &mut waiting, &workstation, &laptop).await;
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_not_taken_up_in_time_gives_its_room_back_at_once() {
    let (workstation, laptop, tablet) = (key(), key(), key());
    let (relay, mut waiting) = relay_with_room_for_one(&workstation, &laptop, |config| {
        config
            .with_connection_log(std::io::sink())
            .with_join_timeout(Duration::from_millis(100))
    })
    .await;
    Client::logged_in(&relay, &tablet, "17", "octo").await;

    // A Server whose connection is full asks to be joined to one that does
    // not take the join up.
    let mut timed_out = Client::backed_up(&relay, &tablet).await;
    timed_out
        .say(&ServerMessage::Join {
            server: Bytes(workstation.subject_public_key_info()),
        })
        .await;
    waiting.reached().await;

    joined_once_there_is_room(&relay, &mut waiting, &workstation, &laptop).await;
    relay.running.shutdown().await.unwrap();
}

/// How often the Relays of the tests below check their Accounts against
/// their admission rules again.
const ADMISSION_INTERVAL: Duration = Duration::from_millis(10);

/// A Relay checking its Accounts against its rules every
/// [`ADMISSION_INTERVAL`], configured further as `configure` says.
async fn checking_relay(configure: impl FnOnce(RelayConfig) -> RelayConfig) -> Relay {
    relay_with(ScriptedProvider::new(), |config| {
        configure(config.with_admission_interval(ADMISSION_INTERVAL))
    })
    .await
}

impl Client {
    /// Proves `key` on a connection kept open idle, as a Server keeps one to
    /// its Relay: the connection, and what the Relay says of its Login.
    async fn kept(relay: &Relay, key: &KeyPair) -> (Self, Option<Account>) {
        let mut client = Self::connect(relay).await;
        let RelayMessage::Proven { login } = client.prove(key).await else {
            panic!("the Relay takes a Server's proof");
        };
        (client, login)
    }

    /// Whether the Relay refuses this Server's Login, cutting the connection
    /// standing on it, and says so.
    async fn cut_for_login_needed(&mut self) -> bool {
        refusal(&self.hear().await) == Some(&Refusal::LoginNeeded) && self.ended().await
    }
}

/// The Account the Login tied to `key` stands under, as a Server proving it
/// is told, where it stands.
async fn standing(relay: &Relay, key: &KeyPair) -> Option<Account> {
    Client::kept(relay, key).await.1
}

/// Waits until `relay`'s scripted provider has been asked whether someone is
/// admitted `more` more times, so that many checks have been made since.
async fn asked_more(relay: &Relay, more: u64) {
    let asked = relay.provider.admissions_asked();
    timeout(DEADLINE, async {
        while relay.provider.admissions_asked() < asked + more {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the Relay checks its Accounts on its own");
}

#[tokio::test]
async fn a_relay_with_no_rules_admits_nobody_and_tells_whoever_logs_in_why() {
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_admission(Admission::nobody())
    })
    .await;
    let key = key();

    let mut client = Client::connect(&relay).await;
    let refused = client.logging_in(&relay, &key, "17", "octo").await;
    let RelayMessage::Refused {
        refusal: Refusal::NotAdmitted,
        message,
    } = refused
    else {
        panic!("a Relay with no rules refuses every login as not admitted, not {refused:?}");
    };
    assert!(
        message.contains("octo") && message.contains("operator"),
        "{message}"
    );
    assert!(relay.running.store().accounts().await.unwrap().is_empty());
    assert!(relay.running.store().logins().await.unwrap().is_empty());
    assert_eq!(standing(&relay, &key).await, None);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_login_the_rules_do_not_admit_forms_no_login_and_takes_nothing_from_the_one_held() {
    let relay = relay().await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;

    relay.provider.set_admitted("99", false);
    let mut client = Client::connect(&relay).await;
    assert_eq!(
        refusal(
            &client
                .logging_in(&relay, &laptop, "99", "someone-else")
                .await
        ),
        Some(&Refusal::NotAdmitted)
    );
    assert_eq!(
        standing(&relay, &laptop)
            .await
            .map(|account| account.username),
        Some("octo".to_owned()),
        "the Login the Server held stands as it did"
    );
    assert_eq!(relay.running.store().accounts().await.unwrap().len(), 1);
    let (_taken_up, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        answer,
        RelayMessage::Joined,
        "a join asked on the strength of the Login held is made all the same"
    );
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_account_the_rules_no_longer_admit_lapses_cutting_all_that_stands_on_it_and_forgetting_nothing()
 {
    let (writer, mut log) = ConnectionLog::new();
    let relay = checking_relay(|config| config.with_connection_log(writer)).await;
    let (workstation, laptop, tablet, stranger) = (key(), key(), key(), key());
    for key in [&workstation, &laptop, &tablet] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    Client::logged_in(&relay, &stranger, "99", "someone-else").await;
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(&[1; 40]).await;
    assert_eq!(taken_up.carried().await.len(), 40);
    taken_up.carry(&[2; 9]).await;
    assert_eq!(asking.carried().await.len(), 9);
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let (mut idle, _) = Client::kept(&relay, &tablet).await;

    relay.provider.set_admitted("17", false);
    assert!(
        asking.ended().await && taken_up.ended().await,
        "the join carried for the Account is cut at once, on both sides"
    );
    assert_eq!(
        bytes_sent(&log.line().await.unwrap()),
        (40, 9),
        "the join cut is logged, counting all it carried"
    );
    assert!(waiting.cut_for_login_needed().await);
    assert!(idle.cut_for_login_needed().await);
    for key in [&workstation, &laptop, &tablet] {
        assert_eq!(standing(&relay, key).await, None);
    }
    let mut refused = Client::connect(&relay).await;
    refused.prove(&workstation).await;
    refused.say(&ServerMessage::Wait).await;
    assert_eq!(refusal(&refused.hear().await), Some(&Refusal::LoginNeeded));
    let mut refused = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(refusal(&refused.hear().await), Some(&Refusal::LoginNeeded));
    assert!(
        standing(&relay, &stranger).await.is_some(),
        "another Account stands as it did"
    );
    let accounts = relay.running.store().accounts().await.unwrap();
    assert_eq!(
        accounts
            .iter()
            .map(|account| (account.subject.as_str(), account.lapsed))
            .collect::<Vec<_>>(),
        [("17", true), ("99", false)]
    );
    assert_eq!(
        relay.running.store().logins().await.unwrap().len(),
        4,
        "no Login is forgotten"
    );

    // Admitted again, the Account stands only once one of its Servers logs
    // in afresh — and then every Login under it does.
    relay.provider.set_admitted("17", true);
    asked_more(&relay, 3).await;
    assert_eq!(standing(&relay, &workstation).await, None);
    Client::logged_in(&relay, &tablet, "17", "octo").await;
    for key in [&workstation, &laptop] {
        assert!(standing(&relay, key).await.is_some());
    }
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(b"again").await;
    assert_eq!(taken_up.carried().await, b"again");
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_join_asked_as_its_account_lapses_is_given_up() {
    let relay = checking_relay(|config| config).await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;

    relay.provider.set_admitted("17", false);
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::LoginNeeded));
    assert!(waiting.cut_for_login_needed().await);
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "a join given up is taken up by nobody"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn rules_that_cannot_tell_lapse_nobody_and_admit_nobody_new() {
    let relay = checking_relay(|config| config).await;
    let (workstation, laptop, newcomer) = (key(), key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;

    relay.provider.set_admission_undecided(true);
    asked_more(&relay, 3).await;
    asking.carry(b"still").await;
    assert_eq!(
        taken_up.carried().await,
        b"still",
        "the Account stands while the rules cannot tell"
    );
    assert!(standing(&relay, &workstation).await.is_some());
    let mut client = Client::connect(&relay).await;
    assert_eq!(
        refusal(&client.logging_in(&relay, &newcomer, "99", "newcomer").await),
        Some(&Refusal::LoginUnavailable),
        "a login the rules cannot tell about is refused until they can"
    );
    assert_eq!(relay.running.store().accounts().await.unwrap().len(), 1);
    assert_eq!(standing(&relay, &newcomer).await, None);
    relay.running.shutdown().await.unwrap();
}

/// A rule admitting everyone that holds each asking about `subject`, past
/// the first, until the test opens its gate — counting the askings, and
/// how many are under way at once.
struct Gated {
    subject: &'static str,
    gate: tokio::sync::watch::Sender<bool>,
    asked: AtomicUsize,
    under_way: AtomicUsize,
    most_under_way: AtomicUsize,
}

impl Gated {
    fn new(subject: &'static str) -> Arc<Self> {
        Arc::new(Self {
            subject,
            gate: tokio::sync::watch::Sender::new(false),
            asked: AtomicUsize::new(0),
            under_way: AtomicUsize::new(0),
            most_under_way: AtomicUsize::new(0),
        })
    }

    /// Waits until the rule has been asked about its subject `times` times.
    async fn asked(&self, times: usize) {
        timeout(DEADLINE, async {
            while self.asked.load(Ordering::Acquire) < times {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the Relay asks the rule on its own");
    }
}

/// One asking under way, counted as such until it ends or is given up.
struct UnderWay<'rule>(&'rule AtomicUsize);

impl Drop for UnderWay<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl AdmissionRule for Gated {
    async fn admits(
        &self,
        _provider: &str,
        identity: &Identity,
    ) -> Result<bool, suru_relay::Undecided> {
        if identity.subject == self.subject && self.asked.fetch_add(1, Ordering::AcqRel) > 0 {
            let under_way = self.under_way.fetch_add(1, Ordering::AcqRel) + 1;
            let _under_way = UnderWay(&self.under_way);
            self.most_under_way.fetch_max(under_way, Ordering::AcqRel);
            let _ = self.gate.subscribe().wait_for(|open| *open).await;
        }
        Ok(true)
    }
}

#[tokio::test]
async fn checking_the_rules_holds_up_nothing_else_and_never_overlaps_itself() {
    let gated = Gated::new("slow");
    let relay = checking_relay(|config| {
        config.with_admission(Admission::by([gated.clone() as Arc<dyn AdmissionRule>]))
    })
    .await;
    let (slow, workstation, laptop) = (key(), key(), key());
    Client::logged_in(&relay, &slow, "slow", "slowpoke").await;
    gated.asked(2).await;

    // While the rules take their time over one Account, the Relay goes on
    // logging Servers in, saying whose Logins stand, and joining them.
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    assert!(standing(&relay, &slow).await.is_some());
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    asking.carry(b"meanwhile").await;
    assert_eq!(taken_up.carried().await, b"meanwhile");

    // No check begins while one is under way, however many intervals pass.
    tokio::time::sleep(ADMISSION_INTERVAL * 10).await;
    assert_eq!(gated.asked.load(Ordering::Acquire), 2);
    gated.gate.send_replace(true);
    gated.asked(4).await;
    assert_eq!(gated.most_under_way.load(Ordering::Acquire), 1);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn rules_that_do_not_answer_in_time_lapse_nobody() {
    let gated = Gated::new("slow");
    let relay = checking_relay(|config| {
        config
            .with_admission(Admission::by([gated.clone() as Arc<dyn AdmissionRule>]))
            .with_admission_timeout(Duration::from_millis(20))
    })
    .await;
    let slow = key();
    Client::logged_in(&relay, &slow, "slow", "slowpoke").await;

    gated.asked(4).await;
    assert!(
        standing(&relay, &slow).await.is_some(),
        "an Account stands while the rules do not answer about it in time"
    );
    assert_eq!(
        gated.most_under_way.load(Ordering::Acquire),
        1,
        "each asking not answered in time is given up before the next"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_account_not_logged_in_as_as_often_as_the_operator_requires_lapses_until_one_server_logs_in_afresh()
 {
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let ahead = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock = {
        let ahead = ahead.clone();
        Clock::from_fn(move || {
            std::time::SystemTime::now() + Duration::from_secs(ahead.load(Ordering::Acquire))
        })
    };
    let pass = |days: u64| {
        ahead.fetch_add(days * DAY.as_secs(), Ordering::AcqRel);
    };
    let relay =
        checking_relay(|config| config.with_clock(clock).with_fresh_login_every(7 * DAY)).await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }

    pass(6);
    asked_more(&relay, 2).await;
    assert!(standing(&relay, &workstation).await.is_some());
    let (mut idle, _) = Client::kept(&relay, &workstation).await;

    pass(2);
    assert!(
        idle.cut_for_login_needed().await,
        "what stands on a Login is cut as its Account comes due"
    );
    for key in [&workstation, &laptop] {
        assert_eq!(standing(&relay, key).await, None);
    }
    assert!(relay.running.store().accounts().await.unwrap()[0].lapsed);

    // One fresh login from either Server restores both, and the period runs
    // from it.
    Client::logged_in(&relay, &laptop, "17", "octo").await;
    assert!(standing(&relay, &workstation).await.is_some());
    pass(6);
    asked_more(&relay, 2).await;
    assert!(standing(&relay, &workstation).await.is_some());
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn forgetting_a_login_cuts_the_joins_it_stands_in() {
    let (writer, mut log) = ConnectionLog::new();
    let relay = relay_with(ScriptedProvider::new(), |config| {
        config.with_connection_log(writer)
    })
    .await;
    let (workstation, laptop) = (key(), key());
    for key in [&workstation, &laptop] {
        Client::logged_in(&relay, key, "17", "octo").await;
    }
    let (mut asking, mut taken_up) = joined(&relay, &workstation, &laptop).await;
    taken_up.carry(&[7; 12]).await;
    assert_eq!(asking.carried().await.len(), 12);

    forget(&relay, &laptop).await;
    assert!(asking.ended().await && taken_up.ended().await);
    assert_eq!(bytes_sent(&log.line().await.unwrap()), (0, 12));
    relay.running.shutdown().await.unwrap();
}
