//! One Server's connection to the Relay: agreeing a version, proving its
//! identity key, and then whatever it asks — to log in, to be forgotten, to
//! wait to be reached, to be joined to a Server that waits, or to take up a
//! join asked of it. Whatever it does on the strength of its Login is cut the
//! moment that Login stops standing.

use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{
        ConnectInfo, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
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
    admission::{self, Admission, Checks, Verdict, Verdicts},
    connection_log::{ConnectionLog, Entry, Party},
    forwarded::{self, TrustedProxy},
    identity::{IdentityProvider, LoginRefusal},
    joiner::{Joiner, NotAsked, Waiting},
    standing::{Cut, Held, Holdings},
    store::{Parties, Store},
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
    pub(crate) joiner: Joiner<Accepted>,
    /// Held across each change to a Login or to the standing of its
    /// Account, and across each decision taken on the strength of a Login —
    /// to say it stands, to wait, or to ask or make a join — so none is taken
    /// across a change to a Login it rests on. It guards which asking of the
    /// admission rules last took effect for each identity.
    ///
    /// It is held across the reading or writing of the records each of those
    /// rests on, and across nothing else — never the network, nor the
    /// admission rules — because the reading and what is held or cut on its
    /// strength must be one step: a Login read as standing, and lapsed before
    /// what stands on it is held, would escape the cut. That costs no more
    /// than the store's one connection to its database already does, which
    /// takes its steps one at a time whoever asks, each waiting on SQLite's
    /// own locks no longer than the store's busy timeout.
    pub(crate) standing: tokio::sync::Mutex<Verdicts>,
    /// Everything standing on a Login, to be cut once it stops standing.
    pub(crate) holdings: Holdings,
    pub(crate) store: Store,
    pub(crate) provider: Arc<dyn IdentityProvider>,
    /// The rules the Relay admits by.
    pub(crate) admission: Admission,
    /// Numbers each asking of the rules as it begins.
    pub(crate) checks: Checks,
    /// How often the Relay checks its Accounts against its rules again.
    pub(crate) admission_interval: Duration,
    /// How long the rules may take to answer each asking.
    pub(crate) admission_timeout: Duration,
    /// How recently an Account must have been logged in as, where its
    /// operator requires a fresh login every so often.
    pub(crate) fresh_login_every: Option<Duration>,
    pub(crate) versions: Vec<Version>,
    pub(crate) clock: Clock,
    /// The reverse proxies whose word the Relay takes for the address a
    /// Server's connection comes from.
    pub(crate) trusted_proxies: Vec<TrustedProxy>,
    /// Where each connection the Relay joins is logged.
    pub(crate) connection_log: ConnectionLog,
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
type Taker = oneshot::Sender<Accepted>;

/// The connection a Server took a join up on, with the two Logins the join
/// is between and the Account they stood under as it did, and the join held
/// as standing on them.
pub(crate) struct Accepted {
    channel: Channel,
    parties: Parties,
    held: Held,
}

/// A join a Server took up, with what hands the connection it took it up on
/// to the Server that asked for it.
struct Handover {
    taker: Taker,
    parties: Parties,
    held: Held,
}

impl Relay {
    /// When an Account must have been logged in as since for its Logins to
    /// stand, where the Relay requires a fresh login every so often.
    pub(crate) fn fresh_since(&self) -> Option<SystemTime> {
        self.fresh_login_every
            .map(|every| self.clock.now().checked_sub(every).unwrap_or(UNIX_EPOCH))
    }

    /// Cuts, for `why`, everything standing on the Logins tied to `keys`, as
    /// they stood, while the standing lock is held, as `standing` shows: the
    /// joins asked between them, the joins carried, and the connections their
    /// Servers hold on the strength of them.
    pub(crate) fn cut(&self, standing: &Verdicts, keys: &[Vec<u8>], why: Cut) {
        for key in keys {
            self.joiner.give_up_joins_of(key);
        }
        self.holdings.cut(standing, keys, why);
    }
}

pub(crate) async fn connect(
    State(relay): State<Arc<Relay>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let address = forwarded::network_address(peer.ip(), &headers, &relay.trusted_proxies);
    // What a Server sends and what it has yet to take in are both held to a
    // bound, so one that pings without reading, or sends without end, costs
    // the Relay no more than that.
    upgrade
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE_LEN)
        .max_message_size(MAX_MESSAGE_LEN)
        .max_frame_size(MAX_MESSAGE_LEN)
        .on_upgrade(move |socket| converse(socket, relay, address))
}

/// Converses with the Server whose connection comes from `address`.
async fn converse(socket: WebSocket, relay: Arc<Relay>, address: IpAddr) {
    let mut channel = Channel {
        socket,
        send_timeout: relay.send_timeout,
        address,
        closed: false,
    };
    let mut stopping = relay.stopping.clone();
    // A connection that ends says so, if the Server takes it in time; a
    // stopping Relay lets every connection go at once, however full it is,
    // saying goodbye or not — the joins it carries among them.
    tokio::select! {
        _ = stopping.wait_for(|stopping| *stopping) => {}
        () = async move {
            match serve(&mut channel, &relay).await {
                Ok(Some(handover)) => hand_over(channel, handover).await,
                Ok(None) | Err(Ended) => {
                    channel.close(None).await;
                }
            }
        } => {}
    }
}

/// Hands the connection a Server took a join up on to the Server that asked
/// for the join, which carries it from then on; where that Server has gone
/// meanwhile, the join is refused.
async fn hand_over(
    channel: Channel,
    Handover {
        taker,
        parties,
        held,
    }: Handover,
) {
    if let Err(Accepted { mut channel, .. }) = taker.send(Accepted {
        channel,
        parties,
        held,
    }) {
        let _ = channel
            .send(&refused(
                Refusal::Unexpected,
                "the Server that asked for this join has gone",
            ))
            .await;
        channel.close(None).await;
    }
}

/// Hears the Server prove itself, then answers what it asks until it goes:
/// what takes over the connection where the Server takes a join up on it.
async fn serve(channel: &mut Channel, relay: &Relay) -> Result<Option<Handover>, Ended> {
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
    // The connection stands on the Server's Login while the Login stands,
    // and is cut the moment it stops.
    let (login, mut held) = {
        let standing = relay.standing.lock().await;
        match relay.store.standing(&key, relay.fresh_since()).await {
            Ok(login) => {
                let held = login
                    .is_some()
                    .then(|| relay.holdings.hold(&standing, vec![key.clone()]));
                (login, held)
            }
            Err(error) => {
                drop(standing);
                return unreadable(channel, error).await;
            }
        }
    };
    channel.send(&RelayMessage::Proven { login }).await?;
    loop {
        let asked = tokio::select! {
            asked = channel.receive() => asked?,
            why = cut(&mut held) => return cut_off(channel, why).await,
        };
        match asked {
            ServerMessage::BeginLogin { hostname } => {
                log_in(channel, relay, &key, label(&hostname), &mut held).await?;
            }
            ServerMessage::Forget => {
                let forgotten = {
                    let standing = relay.standing.lock().await;
                    let forgotten = relay.store.forget(&key).await;
                    relay.cut(&standing, std::slice::from_ref(&key), Cut::Refused);
                    forgotten
                };
                if let Err(error) = forgotten {
                    return unreadable(channel, error).await;
                }
                channel.send(&RelayMessage::Forgotten).await?;
                return Ok(None);
            }
            ServerMessage::Wait => {
                let waiting = {
                    let standing = relay.standing.lock().await;
                    relay
                        .store
                        .account_of(&key, relay.fresh_since())
                        .await
                        .map(|account| {
                            account.map(|_| {
                                (
                                    relay.joiner.wait(key.clone()),
                                    relay.holdings.hold(&standing, vec![key.clone()]),
                                )
                            })
                        })
                };
                match waiting {
                    Ok(Some((waiting, held))) => return wait(channel, waiting, held).await,
                    Ok(None) => channel.send(&login_needed()).await?,
                    Err(error) => return unreadable(channel, error).await,
                }
            }
            ServerMessage::Join { server } => {
                if join(channel, relay, &key, &server.0).await? {
                    return Ok(None);
                }
            }
            ServerMessage::Accept { join } => match take_up(relay, &join.0, &key).await {
                Ok(Some(handover)) => return Ok(Some(handover)),
                Ok(None) => {
                    channel
                        .send(&refused(
                            Refusal::Unexpected,
                            "no join by that name awaits this Server",
                        ))
                        .await?;
                }
                Err(error) => return unreadable(channel, error).await,
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

/// Has a Server wait on this connection to be reached, as `waiting`, telling
/// it of each join asked of it, until the connection ends or its Login stops
/// standing, which `held` says. A waiting connection does nothing else.
async fn wait(
    channel: &mut Channel,
    mut waiting: Waiting<Accepted>,
    mut held: Held,
) -> Result<Option<Handover>, Ended> {
    channel.send(&RelayMessage::Waiting).await?;
    loop {
        tokio::select! {
            why = held.cut() => return cut_off(channel, why).await,
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
/// where both Logins stand under one Account, that one waits to be reached,
/// and the connection log has room to record the join: once it takes the
/// join up, carries the bytes between the two connections until either
/// ends, answering whether it did. A refused Server may ask again.
async fn join(
    channel: &mut Channel,
    relay: &Relay,
    key: &[u8],
    server: &[u8],
) -> Result<bool, Ended> {
    let not_waiting = |message| refused(Refusal::NotWaiting, message);
    // Room for the join's line in the connection log is held from just
    // before the join is asked, so a join made is always recorded, and is
    // given back before anything is refused, so no refusal a Server leaves
    // unread holds it.
    let asked = {
        let _standing = relay.standing.lock().await;
        match one_account(relay, key, server).await {
            Ok(Ok(account)) => Ok(match relay.connection_log.room() {
                Some(room) => relay
                    .joiner
                    .ask(server, key, account)
                    .map(|asking| (asking, room))
                    .map_err(|not_asked| match not_asked {
                        NotAsked::NotWaiting => not_waiting(
                            "the Server named is not waiting to be reached at this Relay",
                        ),
                        NotAsked::Busy => not_waiting(
                            "the Server named has as many joins asked of it as it may; ask \
                                 again",
                        ),
                    }),
                None => Err(refused(
                    Refusal::Unavailable,
                    "this Relay cannot record another joined connection just now, so it \
                         joins none; ask again later",
                )),
            }),
            Ok(Err(refusal)) => Ok(Err(refusal)),
            Err(error) => Err(error),
        }
    };
    let (mut asking, room) = match asked {
        Ok(Ok(asked)) => asked,
        Ok(Err(refusal)) => return channel.send(&refusal).await.map(|()| false),
        Err(error) => return unreadable(channel, error).await,
    };
    let taken_up = tokio::select! {
        taken_up = tokio::time::timeout(relay.join_timeout, &mut asking.taken_up) => taken_up,
        spoken = channel.receive() => {
            spoken?;
            drop(room);
            return channel
                .refuse(Refusal::Unexpected, "a Server waits for its join to be made")
                .await;
        }
    };
    drop(asking);
    let Accepted {
        channel: mut serving,
        parties,
        mut held,
    } = match taken_up {
        Ok(Ok(accepted)) => accepted,
        // Given up: a Login it was between changed, or no longer stood under
        // its Account as the Server named took it up.
        Ok(Err(_)) => {
            drop(room);
            let refusal = match one_account(relay, key, server).await {
                Ok(Err(refusal)) => refusal,
                Ok(Ok(_)) => not_waiting("the Server named did not take the join up"),
                Err(error) => return unreadable(channel, error).await,
            };
            return channel.send(&refusal).await.map(|()| false);
        }
        Err(_) => {
            drop(room);
            return channel
                .send(&not_waiting(
                    "the Server named did not take the join up in time",
                ))
                .await
                .map(|()| false);
        }
    };
    if serving.send(&RelayMessage::Joined).await.is_err() {
        drop(room);
        return channel
            .send(&not_waiting("the Server named went as it took the join up"))
            .await
            .map(|()| false);
    }
    // The join is made: from here it is owed its line in the connection log,
    // however it ends.
    let entry = relay
        .connection_log
        .begin(room, parties, channel.address, serving.address);
    channel.send(&RelayMessage::Joined).await?;
    let cut = async {
        held.cut().await;
    };
    carry(channel, serving, relay.send_timeout, entry, cut).await;
    Ok(true)
}

/// The Account the Logins of the Server whose key is `key` and the one whose
/// key is `server` both stand under, or why the Relay would join them under
/// none.
async fn one_account(
    relay: &Relay,
    key: &[u8],
    server: &[u8],
) -> anyhow::Result<Result<i64, RelayMessage>> {
    let fresh_since = relay.fresh_since();
    let Some(joining) = relay.store.account_of(key, fresh_since).await? else {
        return Ok(Err(login_needed()));
    };
    Ok(match relay.store.account_of(server, fresh_since).await? {
        None => Err(refused(
            Refusal::UnknownServer,
            "this Relay knows no Server by the identity key named",
        )),
        Some(serving) if serving != joining => Err(refused(
            Refusal::DifferentAccounts,
            "the Server named is logged in under another Account, and this Relay joins only \
             Servers logged in under the same one",
        )),
        Some(account) => Ok(account),
    })
}

/// Takes up the join named `name` for the Server whose key is `key`, where it
/// was asked of that Server and both Logins it is between still stand under
/// the Account it was asked under: what hands the taker's connection on to
/// the Server that asked for it, with those Logins as they stand, and the
/// join held as standing on them from then on.
async fn take_up(relay: &Relay, name: &[u8], key: &[u8]) -> anyhow::Result<Option<Handover>> {
    let standing = relay.standing.lock().await;
    let Some(taken_up) = relay.joiner.take_up(name, key) else {
        return Ok(None);
    };
    let parties = relay
        .store
        .parties(
            &taken_up.asker_key,
            key,
            taken_up.account,
            relay.fresh_since(),
        )
        .await?;
    Ok(parties.map(|parties| Handover {
        taker: taken_up.taker,
        parties,
        held: relay
            .holdings
            .hold(&standing, vec![taken_up.asker_key, key.to_vec()]),
    }))
}

/// Carries the bytes of two joined connections between them, each binary
/// frame passed on as it came, until either side closes, says anything but
/// bytes, or does not take in what is carried to it within `send_timeout`,
/// or `cut` says a Login the join stands on no longer does; then closes
/// both, passing on what either still had queued as it takes that in, and
/// hands `entry`, the join's line in the connection log, to the log once the
/// Relay has let go of all the join carried.
async fn carry<S: Socket>(
    joining: &mut Channel<S>,
    mut serving: Channel<S>,
    send_timeout: Duration,
    entry: Entry<'_>,
    cut: impl Future<Output = ()>,
) {
    {
        let (to_joining, from_joining) = (&mut joining.socket).split();
        let (to_serving, from_serving) = (&mut serving.socket).split();
        // Each way runs on its own, so neither side's backlog stalls what
        // the other sends.
        tokio::select! {
            () = forward(from_joining, to_serving, send_timeout, &entry.joining) => {}
            () = forward(from_serving, to_joining, send_timeout, &entry.serving) => {}
            () = cut => {}
        }
    }
    // A frame given up on as the join ended may yet go out as its connection
    // closes, counted the moment it does, so the join ends only once both
    // have closed.
    tokio::join!(
        joining.close(Some(&entry.serving)),
        serving.close(Some(&entry.joining))
    );
    drop(entry);
}

/// Passes each binary frame `from` carries on `to`, unread, until `from`
/// ends or says anything else, or `to` does not take one in within
/// `send_timeout`, telling `sender`, the Server `from` comes from, of each
/// frame as it is queued on `to` and as it is written out of it whole.
async fn forward(
    mut from: impl Stream<Item = Result<Message, axum::Error>> + Unpin,
    mut to: impl Sink<Message> + Unpin,
    send_timeout: Duration,
    sender: &Party,
) {
    while let Some(Ok(message)) = from.next().await {
        match message {
            Message::Binary(bytes) => {
                let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let deadline = tokio::time::Instant::now() + send_timeout;
                // Fed, and then ready for more: only then has the connection
                // itself queued the frame, whole, rather than whatever feeds
                // it holding it back.
                let queued = tokio::time::timeout_at(deadline, async {
                    to.feed(Message::Binary(bytes)).await?;
                    std::future::poll_fn(|context| to.poll_ready_unpin(context)).await
                });
                if !matches!(queued.await, Ok(Ok(()))) {
                    return;
                }
                sender.queue(length);
                if !matches!(
                    tokio::time::timeout_at(deadline, to.flush()).await,
                    Ok(Ok(()))
                ) {
                    return;
                }
                sender.pass_on();
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Text(_) | Message::Close(_) => return,
        }
    }
}

/// Logs in the Server whose key is `key` through the Relay's identity
/// provider, telling it where its user goes to log in and then how the login
/// ended: forming its Login only where the Relay's rules admit who logged in,
/// asked as the login ends. A Server that goes, or speaks, before it ends
/// abandons it. The connection, as `held` says, stands on the Login formed.
async fn log_in(
    channel: &mut Channel,
    relay: &Relay,
    key: &[u8],
    hostname: String,
    held: &mut Option<Held>,
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
    let provider = relay.provider.name();
    // The rules are asked with nothing held, and what they find takes effect
    // under the standing lock only where no asking begun later has found
    // otherwise since.
    let check = relay.checks.begin();
    match relay
        .admission
        .decide(provider, &identity, relay.admission_timeout)
        .await
    {
        Verdict::Admitted => {}
        // A refusal is news of the identity's Account as well: it lapses at
        // once, as it would at the Relay's next check.
        Verdict::NotAdmitted => {
            return match admission::refuse(relay, provider, &identity.subject, check).await {
                Ok(()) => {
                    channel
                        .send(&not_admitted(provider, &identity.username))
                        .await
                }
                Err(error) => unreadable(channel, error).await,
            };
        }
        Verdict::Undecided(why) => {
            tracing::warn!("a login was refused, as the admission rules could not tell: {why}");
            return channel
                .send(&refused(
                    Refusal::LoginUnavailable,
                    "the Relay could not tell just now whether it admits you; log in again later",
                ))
                .await;
        }
    }
    let (subject, username) = (identity.subject.clone(), identity.username.clone());
    let recorded = {
        let mut standing = relay.standing.lock().await;
        if standing.may_admit(provider, &subject, check) {
            let recorded = relay
                .store
                .record_login(provider, identity, key, hostname, relay.clock.now())
                .await;
            if let Ok(recorded) = &recorded {
                standing.admitted(provider, &subject, check);
                relay.joiner.give_up_joins_of(key);
                // A Login moved to another Account carries nothing that stood
                // on it under the one before.
                if recorded.moved() {
                    relay.cut(&standing, &[key.to_vec()], Cut::Moved);
                }
                *held = Some(relay.holdings.hold(&standing, vec![key.to_vec()]));
            }
            recorded.map(Some)
        } else {
            Ok(None)
        }
    };
    match recorded {
        Ok(Some(recorded)) => {
            channel
                .send(&RelayMessage::LoginDone {
                    account: recorded.account,
                })
                .await
        }
        // An asking of the rules begun after this one found they do not
        // admit the identity, which outranks what this one found.
        Ok(None) => channel.send(&not_admitted(provider, &username)).await,
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

/// What a connection standing on a Login that stops standing is told as it
/// is cut.
const STOPPED_STANDING: &str =
    "this Server's Login at this Relay no longer stands; log in to it again";

fn not_admitted(provider: &str, username: &str) -> RelayMessage {
    refused(
        Refusal::NotAdmitted,
        format!(
            "this Relay's rules do not admit {username} ({provider}); only its operator can \
             change that"
        ),
    )
}

/// Returns once `held` has been cut, saying why, where the connection stands
/// on a Login; never, where it stands on none.
async fn cut(held: &mut Option<Held>) -> Cut {
    match held {
        Some(held) => held.cut().await,
        None => std::future::pending().await,
    }
}

/// Ends a connection that stood on a Login, cut for `why`: telling its Server
/// the Login needs renewing, where it stands no longer, or saying nothing,
/// where it now stands under another Account, so the Server connects again
/// and is told which.
async fn cut_off(channel: &mut Channel, why: Cut) -> Result<Option<Handover>, Ended> {
    match why {
        Cut::Refused => channel.refuse(Refusal::LoginNeeded, STOPPED_STANDING).await,
        Cut::Moved => Ok(None),
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

/// What a Server's connection runs over: its WebSocket, or in a test a
/// stand-in for one.
pub(crate) trait Socket:
    Stream<Item = Result<Message, axum::Error>> + Sink<Message, Error = axum::Error> + Unpin
{
}

impl<S> Socket for S where
    S: Stream<Item = Result<Message, axum::Error>> + Sink<Message, Error = axum::Error> + Unpin
{
}

/// A Server's WebSocket, carrying one JSON message to a text frame until it
/// carries a join.
pub(crate) struct Channel<S = WebSocket> {
    socket: S,
    /// How long the Server may take to take in what is sent it.
    send_timeout: Duration,
    /// The network address the Server's connection comes from.
    address: IpAddr,
    /// Whether the Relay has closed the connection.
    closed: bool,
}

impl<S: Socket> Channel<S> {
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

    /// Closes the connection once whatever is queued ahead of the close has
    /// gone, all within the send timeout, passing on whatever `sender`, the
    /// Server at the other end of a join, had queued on it the moment that
    /// goes. A frame cut off part-way reaches the Server as no frame at all,
    /// and is not passed on. A connection is closed once; closing it again
    /// does nothing.
    async fn close(&mut self, sender: Option<&Party>) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        let deadline = tokio::time::Instant::now() + self.send_timeout;
        let flushed = tokio::time::timeout_at(deadline, SinkExt::flush(&mut self.socket)).await;
        if matches!(flushed, Ok(Ok(()))) {
            if let Some(sender) = sender {
                sender.pass_on();
            }
            let _ = tokio::time::timeout_at(deadline, self.socket.send(Message::Close(None))).await;
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
            match self.socket.next().await {
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
    use std::{
        net::Ipv4Addr,
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::store::{Account, Login};

    /// How long the Relay waits on a stand-in Server that takes nothing in.
    const SEND_TIMEOUT: Duration = Duration::from_secs(30);

    /// What a stand-in for a Server's WebSocket has been sent: queued, as a
    /// WebSocket's own buffer holds it, until its Server takes it in.
    #[derive(Default)]
    struct Sent {
        taking_in: bool,
        queued: Vec<Message>,
        taken_in: Vec<Message>,
        waker: Option<Waker>,
    }

    /// A stand-in for a Server's WebSocket, on which what its Server says
    /// arrives from `said`.
    struct Stub {
        said: mpsc::UnboundedReceiver<Message>,
        sent: Arc<Mutex<Sent>>,
    }

    /// The Server at the far end of a [`Stub`].
    struct StubServer {
        says: mpsc::UnboundedSender<Message>,
        sent: Arc<Mutex<Sent>>,
    }

    impl StubServer {
        fn say(&self, bytes: &[u8]) {
            self.says
                .send(Message::Binary(bytes.to_vec().into()))
                .unwrap();
        }

        /// Takes in what it is sent, from here on.
        fn take_in(&self) {
            let mut sent = self.sent.lock().unwrap();
            sent.taking_in = true;
            if let Some(waker) = sent.waker.take() {
                waker.wake();
            }
        }

        /// Whether the last it took in was the Relay closing its connection.
        fn closed(&self) -> bool {
            matches!(
                self.sent.lock().unwrap().taken_in.last(),
                Some(Message::Close(_))
            )
        }

        /// The length of each frame it has taken in.
        fn frames(&self) -> Vec<usize> {
            let sent = self.sent.lock().unwrap();
            sent.taken_in
                .iter()
                .filter_map(|message| match message {
                    Message::Binary(bytes) => Some(bytes.len()),
                    _ => None,
                })
                .collect()
        }
    }

    impl Stream for Stub {
        type Item = Result<Message, axum::Error>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.said.poll_recv(context).map(|said| said.map(Ok))
        }
    }

    impl Sink<Message> for Stub {
        type Error = axum::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
            self.sent.lock().unwrap().queued.push(message);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            let mut sent = self.sent.lock().unwrap();
            if sent.taking_in {
                let queued = std::mem::take(&mut sent.queued);
                sent.taken_in.extend(queued);
                Poll::Ready(Ok(()))
            } else {
                sent.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }

        fn poll_close(
            self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.poll_flush(context)
        }
    }

    /// A connection to a stand-in Server that takes nothing in until told
    /// to.
    fn stub() -> (Channel<Stub>, StubServer) {
        let (says, said) = mpsc::unbounded_channel();
        let sent = Arc::new(Mutex::new(Sent::default()));
        (
            Channel {
                socket: Stub {
                    said,
                    sent: sent.clone(),
                },
                send_timeout: SEND_TIMEOUT,
                address: Ipv4Addr::LOCALHOST.into(),
                closed: false,
            },
            StubServer { says, sent },
        )
    }

    /// Where a connection log writes, each line handed on as it is written.
    struct Lines(std::sync::mpsc::Sender<Vec<u8>>);

    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A connection log, what it writes, and the time it reads.
    fn connection_log() -> (
        ConnectionLog,
        crate::connection_log::Writing,
        std::sync::mpsc::Receiver<Vec<u8>>,
        Arc<Mutex<SystemTime>>,
    ) {
        let (lines, written) = std::sync::mpsc::channel();
        let now = Arc::new(Mutex::new(UNIX_EPOCH));
        let clock = {
            let now = now.clone();
            Clock::from_fn(move || *now.lock().unwrap())
        };
        let (log, writing) =
            ConnectionLog::start(Arc::new(Mutex::new(Lines(lines))), 4, clock).unwrap();
        (log, writing, written, now)
    }

    /// Two Logins under one Account, the laptop's joining the
    /// workstation's.
    fn parties() -> Parties {
        let login = |fingerprint: &str, hostname: &str| Login {
            account: 1,
            fingerprint: fingerprint.to_owned(),
            hostname: hostname.to_owned(),
            formed_at: UNIX_EPOCH,
        };
        Parties {
            account: Account {
                id: 1,
                provider: "scripted".to_owned(),
                subject: "17".to_owned(),
                username: "octo".to_owned(),
                lapsed: false,
            },
            joining: login("laptop-key", "laptop"),
            serving: login("workstation-key", "workstation"),
        }
    }

    /// The next line the log writes.
    fn logged(written: &std::sync::mpsc::Receiver<Vec<u8>>) -> serde_json::Value {
        let line = written
            .recv_timeout(Duration::from_secs(30))
            .expect("the join is logged");
        serde_json::from_slice(&line).unwrap()
    }

    fn bytes_sent(line: &serde_json::Value) -> (u64, u64) {
        (
            line["joining"]["bytes_sent"].as_u64().unwrap(),
            line["serving"]["bytes_sent"].as_u64().unwrap(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn frames_passed_on_as_a_join_closes_are_counted_and_it_ends_once_they_have_gone() {
        let (log, _writing, written, now) = connection_log();
        let (mut joining, laptop) = stub();
        let (serving, workstation) = stub();
        laptop.say(&[1; 10]);
        workstation.say(&[2; 20]);
        let entry = log.begin(
            log.room().unwrap(),
            parties(),
            Ipv4Addr::LOCALHOST.into(),
            Ipv4Addr::LOCALHOST.into(),
        );
        let carrying = carry(
            &mut joining,
            serving,
            SEND_TIMEOUT,
            entry,
            std::future::pending(),
        );
        tokio::pin!(carrying);

        // Neither Server takes in what is carried to it, so the Relay gives
        // the join up and closes both sides.
        assert!(
            tokio::time::timeout(SEND_TIMEOUT * 3 / 2, &mut carrying)
                .await
                .is_err()
        );
        // Each takes in what was queued for it as its side closes.
        *now.lock().unwrap() = UNIX_EPOCH + Duration::from_secs(60);
        workstation.take_in();
        laptop.take_in();
        carrying.await;
        assert_eq!(
            (workstation.frames(), laptop.frames()),
            (vec![10], vec![20])
        );
        let line = logged(&written);
        assert_eq!(bytes_sent(&line), (10, 20));
        assert_eq!(line["end"], "1970-01-01T00:01:00.000Z");
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_passed_on_as_one_side_closes_is_counted_though_the_relay_stops_before_the_other_has()
     {
        let (log, _writing, written, _now) = connection_log();
        let (mut joining, laptop) = stub();
        let (serving, workstation) = stub();
        laptop.say(&[1; 10]);
        workstation.say(&[2; 20]);
        let entry = log.begin(
            log.room().unwrap(),
            parties(),
            Ipv4Addr::LOCALHOST.into(),
            Ipv4Addr::LOCALHOST.into(),
        );
        {
            let carrying = carry(
                &mut joining,
                serving,
                SEND_TIMEOUT,
                entry,
                std::future::pending(),
            );
            tokio::pin!(carrying);
            assert!(
                tokio::time::timeout(SEND_TIMEOUT * 3 / 2, &mut carrying)
                    .await
                    .is_err()
            );
            // The serving Server takes in the frame queued for it as its side
            // closes; the joining Server's side has yet to close when the
            // Relay stops, letting go of the join.
            workstation.take_in();
            assert!(
                tokio::time::timeout(SEND_TIMEOUT / 4, &mut carrying)
                    .await
                    .is_err()
            );
            assert_eq!(workstation.frames(), [10]);
        }
        assert_eq!(bytes_sent(&logged(&written)), (10, 0));
        assert!(laptop.frames().is_empty());
    }

    /// A join cut as a Login it stands on stops standing closes both sides
    /// at once, its line counting everything either passed on.
    #[tokio::test(start_paused = true)]
    async fn a_join_cut_closes_both_sides_at_once_counting_what_each_passed_on() {
        let (log, _writing, written, _now) = connection_log();
        let (mut joining, laptop) = stub();
        let (serving, workstation) = stub();
        laptop.take_in();
        workstation.take_in();
        let entry = log.begin(
            log.room().unwrap(),
            parties(),
            Ipv4Addr::LOCALHOST.into(),
            Ipv4Addr::LOCALHOST.into(),
        );
        let (cut, cut_off) = oneshot::channel::<()>();
        let carrying = carry(&mut joining, serving, SEND_TIMEOUT, entry, async {
            let _ = cut_off.await;
        });
        tokio::pin!(carrying);
        laptop.say(&[1; 10]);
        workstation.say(&[2; 20]);
        assert!(
            tokio::time::timeout(SEND_TIMEOUT / 4, &mut carrying)
                .await
                .is_err(),
            "the join is carried until it is cut"
        );

        cut.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(1), &mut carrying)
            .await
            .expect("a cut join ends at once");
        assert!(laptop.closed() && workstation.closed());
        assert_eq!(
            (workstation.frames(), laptop.frames()),
            (vec![10], vec![20])
        );
        assert_eq!(bytes_sent(&logged(&written)), (10, 20));
    }

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
