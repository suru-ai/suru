//! A test speaking to a Relay as a Server whose identity key it holds — a
//! real Server's, or one no real Server holds — for what no real Server would
//! say there: asking to be joined to a Serving Server and running whatever
//! TLS the test chooses over the join, or forgetting a Login behind its
//! Server's back.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{Identity, ScriptedProvider};
use suru_relay_protocol::{Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, proof_message};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::TcpStream,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use super::PROGRESS_DEADLINE;

/// A Relay as a test reaches it to speak as a Server: where it listens, and
/// the address it is known by, which every proof made for it names.
pub struct RelayVoice {
    pub at: std::net::SocketAddr,
    pub known_as: String,
}

impl RelayVoice {
    /// A connection to the Relay on which `key` is proven, as the Server
    /// holding it would prove it.
    pub async fn proven(&self, key: &KeyPair) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(self.at).await.expect("reach the Relay");
        let (mut socket, _) =
            tokio_tungstenite::client_async(format!("ws://{}/connect", self.at), stream)
                .await
                .expect("open a WebSocket to the Relay");
        let public_key = key.subject_public_key_info();
        say(
            &mut socket,
            &ServerMessage::Hello {
                versions: SPOKEN.to_vec(),
                key: Bytes(public_key.clone()),
            },
        )
        .await;
        let Some(RelayMessage::Challenge { nonce, .. }) = hear(&mut socket).await else {
            panic!("the Relay challenges a Server that says hello");
        };
        let proof = proof_message(&self.known_as, &nonce.0, &public_key);
        say(
            &mut socket,
            &ServerMessage::Proof {
                signature: Bytes(key.sign(&proof).unwrap()),
            },
        )
        .await;
        let Some(RelayMessage::Proven { .. }) = hear(&mut socket).await else {
            panic!("the Relay takes the proof");
        };
        socket
    }

    /// Logs `key` in through `provider` as the identity `subject`, named
    /// `username`.
    pub async fn log_in(
        &self,
        provider: &ScriptedProvider,
        key: &KeyPair,
        subject: &str,
        username: &str,
    ) {
        let mut socket = self.proven(key).await;
        say(
            &mut socket,
            &ServerMessage::BeginLogin {
                hostname: "elsewhere".to_owned(),
            },
        )
        .await;
        let Some(RelayMessage::LoginStarted { user_code, .. }) = hear(&mut socket).await else {
            panic!("the Relay begins a login");
        };
        assert!(provider.approve(
            &user_code,
            Identity {
                subject: subject.to_owned(),
                username: username.to_owned(),
            },
        ));
        assert!(matches!(
            hear(&mut socket).await,
            Some(RelayMessage::LoginDone { .. })
        ));
    }

    /// Has the Relay forget the Login of the Server whose identity key is
    /// `key`, as that Server asks once its user removes the Relay.
    pub async fn forget(&self, key: &KeyPair) {
        let mut socket = self.proven(key).await;
        say(&mut socket, &ServerMessage::Forget).await;
        assert!(matches!(
            hear(&mut socket).await,
            Some(RelayMessage::Forgotten)
        ));
    }

    /// Asks the Relay, as the Server whose identity key is `key`, to be
    /// joined to the Serving Server whose identity key is `server`: the join
    /// as a byte stream, or why the Relay refused it.
    pub async fn join(&self, key: &KeyPair, server: &[u8]) -> Result<DuplexStream, Refusal> {
        let mut socket = self.proven(key).await;
        say(
            &mut socket,
            &ServerMessage::Join {
                server: Bytes(server.to_vec()),
            },
        )
        .await;
        match hear(&mut socket).await {
            Some(RelayMessage::Joined) => Ok(carried(socket)),
            Some(RelayMessage::Refused { refusal, .. }) => Err(refusal),
            other => panic!("the Relay answers a join by making it or refusing it, not {other:?}"),
        }
    }

    /// Joins `key`'s Server to the one whose identity key is `server`,
    /// asking again for as long as that one is not yet waiting at the Relay.
    pub async fn joined(&self, key: &KeyPair, server: &[u8]) -> DuplexStream {
        let joining = timeout(PROGRESS_DEADLINE, async {
            loop {
                match self.join(key, server).await {
                    Ok(stream) => return stream,
                    Err(Refusal::NotWaiting) => tokio::time::sleep(Duration::from_millis(5)).await,
                    Err(refusal) => panic!("the Relay refused the join: {refusal:?}"),
                }
            }
        })
        .await;
        joining.expect("the Serving Server comes to wait at the Relay")
    }

    /// Asks to be joined to the Server whose identity key is `server` until
    /// the Relay says it is not waiting there.
    pub async fn no_longer_waiting(&self, key: &KeyPair, server: &[u8]) {
        let leaving = timeout(PROGRESS_DEADLINE, async {
            loop {
                match self.join(key, server).await {
                    Err(Refusal::NotWaiting) => return,
                    Ok(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                    Err(refusal) => panic!("the Relay refused the join: {refusal:?}"),
                }
            }
        })
        .await;
        leaving.expect("the Serving Server stops waiting at the Relay");
    }
}

async fn say(socket: &mut WebSocketStream<TcpStream>, message: &ServerMessage) {
    socket
        .send(Message::Text(
            serde_json::to_string(message).unwrap().into(),
        ))
        .await
        .expect("say something to the Relay");
}

/// The next thing the Relay says, or `None` once it has gone.
async fn hear(socket: &mut WebSocketStream<TcpStream>) -> Option<RelayMessage> {
    loop {
        let frame = timeout(PROGRESS_DEADLINE, socket.next())
            .await
            .expect("the Relay answers in time")?;
        match frame {
            Ok(Message::Text(text)) => return serde_json::from_str(text.as_str()).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// The bytes of the join `socket` carries, as a byte stream a test runs TLS
/// over: what is written goes to the other side as binary frames, and what
/// it carries back is read in turn, until either side ends. Either side of a
/// join may be spoken so, a Relay's as well as a Server's.
pub fn carried(socket: WebSocketStream<TcpStream>) -> DuplexStream {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let (mut reading, mut writing) = tokio::io::split(theirs);
        let (mut sending, mut receiving) = socket.split();
        let outward = async {
            let mut buffer = vec![0; 16 * 1024];
            while let Ok(read @ 1..) = reading.read(&mut buffer).await {
                let frame = Message::Binary(buffer[..read].to_vec().into());
                if sending.send(frame).await.is_err() {
                    break;
                }
            }
            let _ = sending.close().await;
        };
        let inward = async {
            while let Some(Ok(frame)) = receiving.next().await {
                match frame {
                    Message::Binary(bytes) => {
                        if writing.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            let _ = writing.shutdown().await;
        };
        tokio::select! {
            () = outward => {}
            () = inward => {}
        }
    });
    ours
}
