//! One Server's connection to the Relay: agreeing a version, proving its
//! identity key, and then whatever it asks — to log in, or to be forgotten.

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use futures_util::SinkExt;
use ring::rand::{SecureRandom, SystemRandom};
use suru_relay_protocol::{
    self as protocol, Bytes, MAX_MESSAGE_LEN, NONCE_LEN, Refusal, RelayMessage, ServerMessage,
    Side, Version,
};
use tokio::sync::{mpsc, watch};

use crate::{
    Clock,
    identity::{IdentityProvider, LoginRefusal},
    store::Store,
};

/// The most characters of the hostname a Server reports that the Relay keeps
/// as its Login's label.
const MAX_HOSTNAME_CHARS: usize = 255;

/// What every connection to the Relay shares.
pub(crate) struct Relay {
    /// The Relay's own canonical address, which every proof made for it
    /// names.
    pub(crate) public_address: String,
    /// How long a Server may take over each step of proving itself — saying
    /// hello, and answering the challenge — before the Relay stops waiting.
    pub(crate) greeting_timeout: Duration,
    /// How long a Server may take to take in what the Relay says to it
    /// before the Relay gives the connection up.
    pub(crate) send_timeout: Duration,
    pub(crate) store: Store,
    pub(crate) provider: Arc<dyn IdentityProvider>,
    pub(crate) versions: Vec<Version>,
    pub(crate) clock: Clock,
    /// Turns true as the Relay stops, ending every connection.
    pub(crate) stopping: watch::Receiver<bool>,
    /// Held by whatever holds the Relay — its router, and every connection
    /// to it — so a stopping Relay can wait until the last of them is gone.
    pub(crate) _held: mpsc::Sender<()>,
}

/// The connection ended: the Server went away, or said something no
/// conversation could go on from.
struct Ended;

pub(crate) async fn connect(
    State(relay): State<Arc<Relay>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // What a Server sends and what it has yet to take in are both held to a
    // bound, so one that pings without reading, or sends without end, costs
    // the Relay no more than that.
    upgrade
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE_LEN)
        .max_message_size(MAX_MESSAGE_LEN)
        .max_frame_size(MAX_MESSAGE_LEN)
        .on_upgrade(move |socket| converse(socket, relay))
}

async fn converse(socket: WebSocket, relay: Arc<Relay>) {
    let mut channel = Channel {
        socket,
        send_timeout: relay.send_timeout,
    };
    let mut stopping = relay.stopping.clone();
    let stopped = tokio::select! {
        _ = stopping.wait_for(|stopping| *stopping) => true,
        _ = serve(&mut channel, &relay) => false,
    };
    // A stopping Relay lets every connection go at once, however full it
    // is; one that ends otherwise says so, if the Server takes it in time.
    if !stopped {
        let _ = channel.deliver(Message::Close(None)).await;
    }
}

/// Hears the Server prove itself, then answers what it asks until it goes.
async fn serve(channel: &mut Channel, relay: &Relay) -> Result<(), Ended> {
    let (offered, key) = match greeting(channel, relay).await? {
        ServerMessage::Hello { versions, key } => (versions, key.0),
        _ => {
            return channel
                .refuse(Refusal::Unexpected, "a Server begins by saying hello")
                .await;
        }
    };
    let version = match protocol::agree(&offered, &relay.versions) {
        Ok(version) => version,
        Err(behind) => {
            let message = mismatch(&offered, &relay.versions, behind);
            return channel
                .refuse(
                    Refusal::VersionNotSupported {
                        versions: relay.versions.clone(),
                        behind,
                    },
                    message,
                )
                .await;
        }
    };
    if !protocol::supports_key(&key) {
        return channel
            .refuse(
                Refusal::UnsupportedKey,
                "the Server's identity key is of a kind this Relay cannot check",
            )
            .await;
    }
    let nonce = fresh_nonce();
    channel
        .send(&RelayMessage::Challenge {
            version,
            nonce: Bytes(nonce.to_vec()),
            relay: relay.public_address.clone(),
        })
        .await?;
    let signature = match greeting(channel, relay).await? {
        ServerMessage::Proof { signature } => signature.0,
        _ => {
            return channel
                .refuse(
                    Refusal::Unexpected,
                    "a Server proves its key before asking anything",
                )
                .await;
        }
    };
    if protocol::verify_proof(&relay.public_address, &key, &nonce, &signature).is_err() {
        let message = format!(
            "the proof is not a signature by the key the Server named over this challenge for \
             this Relay, known as {}",
            relay.public_address
        );
        return channel.refuse(Refusal::WrongProof, message).await;
    }
    let login = match relay.store.standing(&key).await {
        Ok(login) => login,
        Err(error) => return unreadable(channel, error).await,
    };
    channel.send(&RelayMessage::Proven { login }).await?;
    loop {
        match channel.receive().await? {
            ServerMessage::BeginLogin { hostname } => {
                log_in(channel, relay, &key, label(&hostname)).await?;
            }
            ServerMessage::Forget => {
                if let Err(error) = relay.store.forget(&key).await {
                    return unreadable(channel, error).await;
                }
                return channel.send(&RelayMessage::Forgotten).await;
            }
            ServerMessage::Hello { .. } | ServerMessage::Proof { .. } => {
                channel
                    .send(&refused(
                        Refusal::Unexpected,
                        "the Server has already proven its key",
                    ))
                    .await?;
            }
            ServerMessage::Unrecognized => {
                channel
                    .send(&refused(
                        Refusal::Unexpected,
                        "the Relay does not recognize what the Server asked",
                    ))
                    .await?;
            }
        }
    }
}

/// Logs in the Server whose key is `key` through the Relay's identity
/// provider, telling it where its user goes to log in and then how the login
/// ended. A Server that goes, or speaks, before it ends abandons it.
async fn log_in(
    channel: &mut Channel,
    relay: &Relay,
    key: &[u8],
    hostname: String,
) -> Result<(), Ended> {
    let login = match relay.provider.begin_login().await {
        Ok(login) => login,
        Err(refusal) => return channel.send(&login_refused(refusal)).await,
    };
    channel
        .send(&RelayMessage::LoginStarted {
            verification_uri: login.verification_uri.clone(),
            user_code: login.user_code.clone(),
            expires_in_seconds: u64::try_from(login.expires_in.as_millis().div_ceil(1000))
                .unwrap_or(u64::MAX),
        })
        .await?;
    let outcome = tokio::select! {
        outcome = tokio::time::timeout(login.expires_in, relay.provider.finish_login(&login)) => {
            outcome.unwrap_or(Err(LoginRefusal::Expired))
        }
        spoken = channel.receive() => {
            spoken?;
            return channel
                .refuse(Refusal::Unexpected, "a Server waits for its login to end")
                .await;
        }
    };
    let identity = match outcome {
        Ok(identity) => identity,
        Err(refusal) => return channel.send(&login_refused(refusal)).await,
    };
    match relay
        .store
        .record_login(
            relay.provider.name(),
            identity,
            key,
            hostname,
            relay.clock.now(),
        )
        .await
    {
        Ok(account) => channel.send(&RelayMessage::LoginDone { account }).await,
        Err(error) => unreadable(channel, error).await,
    }
}

/// Ends the connection on a failure to read or write the Relay's records,
/// which the operator learns of from the Relay's log.
async fn unreadable(channel: &mut Channel, error: anyhow::Error) -> Result<(), Ended> {
    tracing::error!("the Relay's records could not be used: {error:#}");
    channel
        .refuse(
            Refusal::LoginUnavailable,
            "the Relay could not use its records; its operator can see why in its log",
        )
        .await
}

/// What a Server says while proving itself, within the Relay's greeting
/// timeout.
async fn greeting(channel: &mut Channel, relay: &Relay) -> Result<ServerMessage, Ended> {
    tokio::time::timeout(relay.greeting_timeout, channel.receive())
        .await
        .unwrap_or(Err(Ended))
}

fn login_refused(refusal: LoginRefusal) -> RelayMessage {
    match refusal {
        LoginRefusal::Denied => refused(Refusal::LoginDenied, "the login was refused"),
        LoginRefusal::Expired => refused(
            Refusal::LoginExpired,
            "nobody finished the login before it expired",
        ),
        LoginRefusal::Unavailable(reason) => refused(
            Refusal::LoginUnavailable,
            format!("the Relay cannot log anyone in just now: {reason}"),
        ),
    }
}

fn refused(refusal: Refusal, message: impl Into<String>) -> RelayMessage {
    RelayMessage::Refused {
        refusal,
        message: message.into(),
    }
}

/// Says which side of a version mismatch is behind, and so which to upgrade.
fn mismatch(offered: &[Version], spoken: &[Version], behind: Side) -> String {
    let named = |versions: &[Version]| {
        versions
            .iter()
            .max()
            .map_or_else(|| "none".to_owned(), Version::to_string)
    };
    let (offered, spoken) = (named(offered), named(spoken));
    match behind {
        Side::Server => format!(
            "this Relay speaks Relay protocol {spoken} and the Server {offered}: the Server is \
             behind, so upgrade Suru on it"
        ),
        Side::Relay => format!(
            "this Relay speaks Relay protocol {spoken} and the Server {offered}: the Relay is \
             behind, so its operator must upgrade it"
        ),
    }
}

/// The hostname a Server reported, as the label its Login keeps: what a
/// reader can see of it, within [`MAX_HOSTNAME_CHARS`].
fn label(hostname: &str) -> String {
    hostname
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_HOSTNAME_CHARS)
        .collect()
}

fn fresh_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("the operating system supplies randomness");
    nonce
}

/// A Server's WebSocket, carrying one JSON message to a text frame.
struct Channel {
    socket: WebSocket,
    /// How long the Server may take to take in what is sent it.
    send_timeout: Duration,
}

impl Channel {
    async fn send(&mut self, message: &RelayMessage) -> Result<(), Ended> {
        let text = serde_json::to_string(message).expect("a Relay message always encodes");
        self.deliver(Message::Text(text.into())).await
    }

    /// Sends `message` once whatever is queued ahead of it — answers to
    /// pings among them — has gone, so it fits however much was, all within
    /// the send timeout.
    async fn deliver(&mut self, message: Message) -> Result<(), Ended> {
        let delivered = tokio::time::timeout(self.send_timeout, async {
            SinkExt::flush(&mut self.socket).await?;
            self.socket.send(message).await
        });
        match delivered.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(_) => Err(Ended),
        }
    }

    /// Refuses what the Server asked and ends the connection.
    async fn refuse(&mut self, refusal: Refusal, message: impl Into<String>) -> Result<(), Ended> {
        self.send(&refused(refusal, message)).await?;
        Err(Ended)
    }

    /// The next thing the Server says. A frame that is no message of this
    /// protocol is answered as one the Relay does not recognize.
    async fn receive(&mut self) -> Result<ServerMessage, Ended> {
        loop {
            match self.socket.recv().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(
                        serde_json::from_str(text.as_str()).unwrap_or(ServerMessage::Unrecognized)
                    );
                }
                Some(Ok(Message::Binary(_))) => return Ok(ServerMessage::Unrecognized),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(Ended),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_hostname_is_kept_as_a_reader_can_see_it() {
        assert_eq!(label("  laptop\u{7}\n "), "laptop");
        assert_eq!(label(&"x".repeat(300)).chars().count(), MAX_HOSTNAME_CHARS);
    }

    #[test]
    fn a_mismatch_says_which_side_is_behind() {
        let behind_server = mismatch(
            &[Version::Unstable(1)],
            &[Version::Unstable(2)],
            Side::Server,
        );
        assert!(
            behind_server.contains("the Server is behind"),
            "{behind_server}"
        );
        assert!(behind_server.contains("unstable-1") && behind_server.contains("unstable-2"));
        let behind_relay = mismatch(
            &[Version::Unstable(3)],
            &[Version::Unstable(2)],
            Side::Relay,
        );
        assert!(
            behind_relay.contains("the Relay is behind"),
            "{behind_relay}"
        );
    }
}
