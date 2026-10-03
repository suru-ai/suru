//! The Relay's side of the protocol, spoken by a minimal client that can say
//! what no real Server would: a wrong proof, a replayed one, a version from
//! before or after the Relay's own, and messages of a later version.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{
    Identity, RelayConfig, RunningRelay, SCRIPTED_VERIFICATION_URI, ScriptedProvider,
};
use suru_relay_protocol::{
    Account, Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, Side, Version, proof_message,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

struct Relay {
    _directory: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    running: RunningRelay,
}

async fn relay() -> Relay {
    relay_with(ScriptedProvider::new(), |config| config).await
}

async fn relay_with(
    provider: ScriptedProvider,
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> Relay {
    let directory = tempfile::tempdir().expect("create the Relay's directory");
    let provider = Arc::new(provider);
    let running = suru_relay::start(
        configure(RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            directory.path().join("relay.db"),
        )),
        provider.clone(),
    )
    .await
    .expect("start the Relay");
    Relay {
        _directory: directory,
        provider,
        running,
    }
}

/// A connection to the Relay that says exactly what a test tells it to.
struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn connect(relay: &Relay) -> Self {
        let (socket, _) = timeout(
            DEADLINE,
            tokio_tungstenite::connect_async(format!("ws://{}/connect", relay.running.address())),
        )
        .await
        .expect("the Relay answers in time")
        .expect("open a WebSocket to the Relay");
        Self { socket }
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

    /// Says hello as `key` and answers the challenge with the signature
    /// `prove` makes over what a proof signs, returning what the Relay says
    /// to it.
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
        let RelayMessage::Challenge { version, nonce } = self.hear().await else {
            panic!("the Relay challenges a Server that says hello");
        };
        assert_eq!(version, SPOKEN[0]);
        let message = proof_message(&nonce.0, &key.subject_public_key_info());
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
        assert_eq!(self.prove(key).await, RelayMessage::Proven { login: None });
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
                key.sign(&proof_message(&nonce.0, &key.subject_public_key_info()))
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
