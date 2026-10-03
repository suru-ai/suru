//! One Server's connection to the Relay: agreeing a version, proving its
//! identity key, and then whatever it asks — to log in, to be forgotten, to
//! wait to be reached, to be joined to a Server that waits, or to take up a
//! join asked of it.

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use ring::rand::{SecureRandom, SystemRandom};
use suru_relay_protocol::{
    self as protocol, Bytes, MAX_MESSAGE_LEN, NONCE_LEN, Refusal, RelayMessage, ServerMessage,
    Side, Version,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    Clock,
    identity::{IdentityProvider, LoginRefusal},
    joiner::Joiner,
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
    /// How long a waiting Server may take to take up a join asked of it
    /// before the Relay gives the join up.
    pub(crate) join_timeout: Duration,
    /// The Servers waiting to be reached, and the joins asked of them, each
    /// handing on the connection it is taken up on.
    pub(crate) joiner: Joiner<Channel>,
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

/// What hands the connection a Server took a join up on to the Server that
/// asked for the join.
type Taker = oneshot::Sender<Channel>;

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
    // A connection that ends says so, if the Server takes it in time; a
    // stopping Relay lets every connection go at once, however full it is,
    // saying goodbye or not — the joins it carries among them.
    tokio::select! {
        _ = stopping.wait_for(|stopping| *stopping) => {}
        () = async move {
            match serve(&mut channel, &relay).await {
                Ok(Some(taker)) => hand_over(channel, taker).await,
                Ok(None) | Err(Ended) => {
                    let _ = channel.deliver(Message::Close(None)).await;
                }
            }
        } => {}
    }
}

/// Hands the connection a Server took a join up on to the Server that asked
/// for the join, which carries it from then on; where that Server has gone
/// meanwhile, the join is refused.
async fn hand_over(channel: Channel, taker: Taker) {
    if let Err(mut channel) = taker.send(channel) {
        let _ = channel
            .send(&refused(
                Refusal::Unexpected,
                "the Server that asked for this join has gone",
            ))
            .await;
        let _ = channel.deliver(Message::Close(None)).await;
    }
}

/// Hears the Server prove itself, then answers what it asks until it goes:
/// what takes over the connection where the Server takes a join up on it.
async fn serve(channel: &mut Channel, relay: &Relay) -> Result<Option<Taker>, Ended> {
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
                channel.send(&RelayMessage::Forgotten).await?;
                return Ok(None);
            }
            ServerMessage::Wait => match relay.store.account_of(&key).await {
                Ok(Some(_)) => return wait(channel, relay, key).await,
                Ok(None) => channel.send(&login_needed()).await?,
                Err(error) => return unreadable(channel, error).await,
            },
            ServerMessage::Join { server } => {
                if join(channel, relay, &key, &server.0).await? {
                    return Ok(None);
                }
            }
            ServerMessage::Accept { join } => match relay.joiner.take_up(&join.0, &key) {
                Some(taker) => return Ok(Some(taker)),
                None => {
                    channel
                        .send(&refused(
                            Refusal::Unexpected,
                            "no join by that name awaits this Server",
                        ))
                        .await?;
                }
            },
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

/// Has the Server whose key is `key` wait on this connection to be reached,
/// telling it of each join asked of it, until the connection ends. A waiting
/// connection does nothing else.
async fn wait(channel: &mut Channel, relay: &Relay, key: Vec<u8>) -> Result<Option<Taker>, Ended> {
    let mut waiting = relay.joiner.wait(key);
    channel.send(&RelayMessage::Waiting).await?;
    loop {
        tokio::select! {
            reach = waiting.reaches.recv() => {
                let Some(join) = reach else {
                    return Ok(None);
                };
                channel.send(&RelayMessage::Reach { join: Bytes(join) }).await?;
            }
            spoken = channel.receive() => {
                spoken?;
                channel
                    .send(&refused(
                        Refusal::Unexpected,
                        "a waiting Server asks nothing on the connection it waits on",
                    ))
                    .await?;
            }
        }
    }
}

/// Joins the Server whose key is `key` to the one whose key is `server`,
/// where both Logins stand under one Account and that one waits to be
/// reached: once it takes the join up, carries the bytes between the two
/// connections until either ends, answering whether it did. A refused
/// Server may ask again.
async fn join(
    channel: &mut Channel,
    relay: &Relay,
    key: &[u8],
    server: &[u8],
) -> Result<bool, Ended> {
    let joining = match relay.store.account_of(key).await {
        Ok(Some(account)) => account,
        Ok(None) => return channel.send(&login_needed()).await.map(|()| false),
        Err(error) => return unreadable(channel, error).await,
    };
    let refusal = match relay.store.account_of(server).await {
        Ok(None) => Some(refused(
            Refusal::UnknownServer,
            "this Relay knows no Server by the identity key named",
        )),
        Ok(Some(serving)) if serving != joining => Some(refused(
            Refusal::DifferentAccounts,
            "the Server named is logged in under another Account, and this Relay joins only \
             Servers logged in under the same one",
        )),
        Ok(Some(_)) => None,
        Err(error) => return unreadable(channel, error).await,
    };
    if let Some(refusal) = refusal {
        return channel.send(&refusal).await.map(|()| false);
    }
    let not_waiting = |message| refused(Refusal::NotWaiting, message);
    let Some(mut asking) = relay.joiner.ask(server) else {
        return channel
            .send(&not_waiting(
                "the Server named is not waiting to be reached at this Relay",
            ))
            .await
            .map(|()| false);
    };
    let taken_up = tokio::select! {
        taken_up = tokio::time::timeout(relay.join_timeout, &mut asking.taken_up) => taken_up,
        spoken = channel.receive() => {
            spoken?;
            return channel
                .refuse(Refusal::Unexpected, "a Server waits for its join to be made")
                .await;
        }
    };
    drop(asking);
    let Ok(Ok(mut serving)) = taken_up else {
        return channel
            .send(&not_waiting(
                "the Server named did not take the join up in time",
            ))
            .await
            .map(|()| false);
    };
    if serving.send(&RelayMessage::Joined).await.is_err() {
        return channel
            .send(&not_waiting("the Server named went as it took the join up"))
            .await
            .map(|()| false);
    }
    channel.send(&RelayMessage::Joined).await?;
    carry(channel, serving, relay.send_timeout).await;
    Ok(true)
}

/// Carries the bytes of two joined connections between them, each binary
/// frame passed on as it came, until either side closes, says anything but
/// bytes, or does not take in what is carried to it within `send_timeout`;
/// then closes the serving side, leaving the joining side to its own
/// connection to close.
async fn carry(joining: &mut Channel, mut serving: Channel, send_timeout: Duration) {
    {
        let (to_joining, from_joining) = (&mut joining.socket).split();
        let (to_serving, from_serving) = (&mut serving.socket).split();
        // Each way runs on its own, so neither side's backlog stalls what
        // the other sends.
        tokio::select! {
            () = forward(from_joining, to_serving, send_timeout) => {}
            () = forward(from_serving, to_joining, send_timeout) => {}
        }
    }
    let _ = serving.deliver(Message::Close(None)).await;
}

/// Passes each binary frame `from` carries on `to`, unread, until `from`
/// ends or says anything else, or `to` does not take one in within
/// `send_timeout`.
async fn forward(
    mut from: impl Stream<Item = Result<Message, axum::Error>> + Unpin,
    mut to: impl Sink<Message> + Unpin,
    send_timeout: Duration,
) {
    while let Some(Ok(message)) = from.next().await {
        match message {
            Message::Binary(bytes) => {
                let sent = tokio::time::timeout(send_timeout, to.send(Message::Binary(bytes)));
                if !matches!(sent.await, Ok(Ok(()))) {
                    return;
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Text(_) | Message::Close(_) => return,
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
async fn unreadable<T>(channel: &mut Channel, error: anyhow::Error) -> Result<T, Ended> {
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

fn login_needed() -> RelayMessage {
    refused(
        Refusal::LoginNeeded,
        "this Server holds no Login at this Relay that stands; log in to it",
    )
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

/// A Server's WebSocket, carrying one JSON message to a text frame until it
/// carries a join.
pub(crate) struct Channel {
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
    async fn refuse<T>(
        &mut self,
        refusal: Refusal,
        message: impl Into<String>,
    ) -> Result<T, Ended> {
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
