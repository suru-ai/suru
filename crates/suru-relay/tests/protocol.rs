//! The Relay's side of the protocol, spoken by a minimal client that can say
//! what no real Server would: a wrong proof, a replayed one, one made for
//! another Relay, a version from before or after the Relay's own, and messages
//! of a later version.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{
    Identity, RelayConfig, RunningRelay, SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
use suru_relay_protocol::{
    Account, Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, Side, Version, proof_message,
};
use tokio::{
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

/// The address the Relays these tests start are known as, which every proof
/// made for one names.
const PUBLIC_ADDRESS: &str = "https://relay.example.com";

struct Relay {
    _directory: tempfile::TempDir,
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
    let running = suru_relay::start(
        configure(RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            directory.path().join("relay.db"),
            public_address,
        )),
        provider.clone(),
    )
    .await
    .expect("start the Relay");
    Relay {
        _directory: directory,
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
        let said = Arc::new(AtomicUsize::new(0));
        let counting = said.clone();
        let flooding = tokio::spawn(async move {
            let (mut asking, _unread) = self.socket.split();
            while asking
                .send(Message::Text(r#"{"type":"later_request"}"#.into()))
                .await
                .is_ok()
            {
                counting.fetch_add(1, Ordering::AcqRel);
            }
        });
        (said, flooding)
    }

    async fn connect_to(address: std::net::SocketAddr, known_as: &str) -> Self {
        let (socket, _) = timeout(
            DEADLINE,
            tokio_tungstenite::connect_async(format!("ws://{address}/connect")),
        )
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
        assert!(matches!(self.prove(key).await, RelayMessage::Proven { .. }));
        self.say(&ServerMessage::BeginLogin {
            hostname: "workstation".to_owned(),
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
        assert_eq!(
            self.hear().await,
            RelayMessage::LoginDone {
                account: Account {
                    provider: "scripted".to_owned(),
                    username: username.to_owned(),
                },
            }
        );
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
        let mut client = Self::connect(relay).await;
        client.log_in(relay, key, subject, username).await;
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
        let mut client = Self::connect(relay).await;
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
        let mut client = Self::connect(relay).await;
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
    let mut waiting = Client::waiting(relay, serving).await;
    let mut asking = Client::ask_to_join(relay, joining, serving).await;
    let join = waiting.reached().await;
    let (taken_up, answer) = Client::take_up(relay, serving, join).await;
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
