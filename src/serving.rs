//! Pairing formation and the opt-in Server-to-Server listener.
//!
//! Local callers use the small [`ServingController`] interface. Invite
//! encoding, durable identity and Pairing records, dialling a Serving Server's
//! ways — directly first on every new dial, through its Relays after a short
//! head start — and both sides of the pinned-key TLS transport remain private
//! to this module. The Serving side's acceptor takes the connections dialled
//! to its listener and those a Relay carries to it alike, and the redeeming
//! side runs the same pinned-key TLS over a connection from either kind of
//! way: dialled at an address, or joined to its Serving Server at a Relay
//! (ADR-0045). Over a direct way each request under way has a connection of
//! its own, speaking HTTP/1.1; over a Relay way everything asked travels
//! together on one joined stream, speaking HTTP/2 as the two Servers agree
//! inside that TLS, so a Remote in view costs its Relay one join.
//!
//! A Serving Server tells each Peer, over the Pairing, which Relays it Serves
//! through — as the Peer connects, and again as that changes while it is
//! connected — and the redeeming side's Remote keeps its Relay ways up with
//! what it is told, its direct ways staying as its Invite gave them. A Relay
//! way is dialled only at a Relay the redeeming Server's user has chosen by
//! adding it; one at any other is listed with the Remote and nothing more.

use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, OnceLock, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll, ready},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::{Body, Bytes, HttpBody},
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{HeaderMap, Method, Request, StatusCode, Uri, header, uri::PathAndQuery},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{
    FutureExt as _, Stream, StreamExt,
    future::{BoxFuture, Shared},
    stream,
    task::AtomicWaker,
};
use http_body_util::{BodyExt, Limited};
use hyper::{
    body::Incoming,
    client::conn::{http1, http2},
};
use hyper_util::{
    client::{
        legacy::connect::{HttpConnector, proxy::Tunnel},
        proxy::matcher::{Intercept, Matcher},
    },
    rt::{TokioExecutor, TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use rcgen::{
    CertificateParams, DistinguishedName as CertificateDistinguishedName, DnType, KeyPair,
};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError, RootCertStore,
    ServerConfig, SignatureScheme,
    client::danger::{
        HandshakeSignatureValid as ServerHandshakeSignatureValid, ServerCertVerifier,
    },
    client::{WebPkiServerVerifier, danger::ServerCertVerified},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::ClientCertVerifier,
    server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier, danger::ClientCertVerified},
    sign::CertifiedKey,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpListener,
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, server::TlsStream};
use uuid::Uuid;

use crate::{
    protocol::{
        ACT_HEADER, AUTHOR_HEADER, ActId, Author, InvitePreview, IssueInviteRequest, IssuedInvite,
        ListenerState, Peer, RedeemInviteRequest, Remote, RemoteHealth, RemoteRemoval,
        RemoteStatus, ServingSettings, SessionError, SessionErrorCode, UnreachableReason, Way,
    },
    runtime::{protect_current_user_file, replace_private_file},
};

mod identity;
mod identity_store;

use identity::IdentityMaterial;
pub(crate) use identity::{
    IDENTITY_STORE_TIMEOUT, IdentityKeeping, IdentityKey, IdentityKeyUnavailable,
};
#[cfg(test)]
pub(crate) use identity_store::FakeIdentityStore;
pub use identity_store::IdentityStoreChoice;
pub(crate) use identity_store::{Selection, platform_identity_store};
const PEERS_FILE: &str = "peers.json";
const REVOKED_PEERS_FILE: &str = "revoked-peers.json";
const REMOTES_FILE: &str = "remotes.json";
const PAIRING_PROTOCOL_HEADER: &str = "x-suru-protocol-version";
const PEER_API_PREFIX: &str = "/v1/pairing/proxy";
/// The header the Serving listener proves, to this Server's own Session API,
/// that it set the [`AUTHOR_HEADER`] beside it — naming the Peer it
/// authenticated — rather than a Client or a Peer: its value is a secret this
/// process mints and never shows anyone.
const FORWARDED_AUTHOR_PROOF_HEADER: &str = "x-suru-forwarded-author-proof";
/// Where a Peer says it is withdrawing, so removing a Remote can end the
/// Pairing on the Serving side as well as this one.
const PAIRING_WITHDRAWAL_PATH: &str = "/v1/pairing/withdrawal";
/// Where a Peer asks which Relays its Serving Server Serves through, and is
/// told so at once and again whenever that changes, for as long as it holds
/// the answer open.
const OFFERED_RELAYS_PATH: &str = "/v1/pairing/offered-relays";
/// The most Relays a Serving Server tells its Peers it Serves through, and a
/// Peer takes up from what it is told: so the most a Server Serves through.
pub(crate) const MAX_TOLD_RELAYS: usize = 16;
/// The longest address of a Relay a Serving Server tells its Peers of, and a
/// Peer takes up: so the longest a Relay the Server holds an entry for has.
pub(crate) const MAX_TOLD_RELAY_LEN: usize = 512;
/// The most this Server reads of an answer the Pairing's own exchanges give —
/// a health check, an enrollment, a refusal, a conflict the proxy looks into
/// — each a few small fields, so another Server saying more than this, faulty
/// or worse, is read no further rather than held in memory whole.
const PAIRING_ANSWER_BUDGET: usize = 64 * 1024;
/// How many connections may be finishing their TLS handshakes with the
/// Serving side at once; past it, no more are taken until one finishes.
const SERVING_HANDSHAKES_AT_ONCE: usize = 64;
/// How many connections Relays carried may wait for the Serving side's
/// acceptor to take them; past it, more are dropped until it does.
const CARRIED_ARRIVALS_QUEUED: usize = 16;
/// How many connections dialled to the Serving listener may wait for the
/// acceptor to take them: as few as may be, so that past it the listener
/// takes no more and the rest wait in its backlog.
const DIALLED_ARRIVALS_QUEUED: usize = 1;
/// What a Relay way's connection speaks inside the pinned-key TLS, agreed as
/// its handshake is: HTTP/2, so everything asked by that way travels together
/// and a Remote in view costs its Relay one join however much is open to it.
/// A direct way's connection agrees nothing, and speaks HTTP/1.1 as it always
/// has.
const MULTIPLEXED: &[u8] = b"h2";
/// How many requests and streams a joined stream carries at once beside the
/// one its Serving Server tells which Relays it Serves through over; more
/// wait their turn, each held meanwhile by whatever asked it of this Server.
const JOINED_STREAMS_AT_ONCE: u32 = 100;
/// How many a joined stream carries at once in all: those, and the one the
/// Relays are told over, which so never waits behind what is asked.
const JOINED_STREAMS: u32 = JOINED_STREAMS_AT_ONCE + 1;
/// How many requests this Server asks of one Remote at once, by default,
/// awaiting its answer — carried there, or waiting their turn on a joined
/// stream, a probe of its health among them: as many as a joined stream
/// carries at once, and as many again. One asked past them is refused at
/// once, as the Remote being too busy to ask just now, rather than waiting.
/// What the Server asks of a Remote on its own — which Relays it Serves
/// through, over the place kept for that — is not among them.
pub(crate) const AWAITED_AT_ONCE: usize = 2 * JOINED_STREAMS_AT_ONCE as usize;
/// How many bytes the bodies of the requests awaiting one Remote's answer
/// hold between them: as many of the largest the Session API reads as one
/// Prompt binds Attachments. A request holds room for its body before it is
/// read — as much as it says its body is, or as much as its route reads,
/// where it does not say — so one that would hold past them is refused at
/// once, its body unread.
const AWAITED_BODY_BYTES: usize =
    crate::attachments::MAX_ATTACHMENTS_PER_PROMPT * crate::attachments::UPLOAD_BODY_LIMIT;
/// Where the Session API takes an Attachment's upload, whose body it reads
/// further than any other route's.
const ATTACHMENTS_PATH: &str = "/v1/attachments";
/// How much of each request's or stream's body a Server lets the other send
/// it ahead of what reads it.
const JOINED_STREAM_WINDOW: u32 = 256 * 1024;
/// How much of all of them together: room for every stream at once, so those
/// whose readers have stalled never hold back one that is read. The windows
/// bound only what the other Server may send ahead of this one's readers —
/// at most this much, a little over 25 MiB, on a joined stream. Beside it
/// are what a Server holds to send, up to [`JOINED_STREAM_WINDOW`] for each
/// stream; each request's and answer's headers, up to the 16 KiB HTTP/2
/// here takes of a header list; the requests waiting their turn past
/// [`JOINED_STREAMS`], each with its body, which [`AWAITED_AT_ONCE`] and
/// [`AWAITED_BODY_BYTES`] bound; and what the TLS and the Relay's WebSocket
/// buffer.
const JOINED_CONNECTION_WINDOW: u32 = JOINED_STREAMS * JOINED_STREAM_WINDOW;
/// How each Server on a joined stream makes sure, inside the pinned-key TLS,
/// that the other still answers — and so that the Relay between them still
/// carries what either says: once it has taken nothing in for `interval`,
/// idle or not, it asks with an HTTP/2 PING, and gives the joined stream up
/// where that PING's answer has not come within `timeout`. Once the PING is
/// out, nothing else taken in counts toward that: only its answer does.
///
/// A reader that pauses holds back its own stream alone, and the connection
/// goes on being read, so a Server or a Relay falling silent is what leaves
/// a PING unanswered in the main. But a healthy joined stream is given up
/// too where its PING, or the answer, waits behind more than `timeout`'s
/// worth of what is already on its way — up to [`JOINED_CONNECTION_WINDOW`]
/// in each direction, as the windows allow — so `timeout` must let that much
/// cross the slowest link a joined stream is to survive. Giving up closes
/// the stream gracefully, and [`Liveness`] bounds how long that may take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinedKeepalive {
    pub(crate) interval: tokio::time::Duration,
    pub(crate) timeout: tokio::time::Duration,
}

/// What a redeeming Server calls the Serving Server it asks, wherever a name
/// is wanted: the name its identity certificate is minted for. A Serving
/// Server is known by its pinned key alone, so this tells no one apart, and
/// it is never sent in the clear.
const SERVING_IDENTITY_NAME: &str = "suru-server";
/// How long a socket a Pairing is carried over — dialled to a direct way, or
/// to the Relay a join is made at — may sit idle before it is probed, and how
/// long between probes, so a Serving Server or a Relay that vanished is found
/// out.
pub(crate) const SOCKET_KEEPALIVE: tokio::time::Duration = tokio::time::Duration::from_secs(15);
/// How many unanswered probes find what such a socket reaches gone.
pub(crate) const SOCKET_KEEPALIVE_PROBES: u32 = 3;
/// How long what such a socket sends may go unacknowledged before the
/// connection is given up, where the platform can be told.
#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
pub(crate) const SOCKET_USER_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// Where the connections Relays carry go during one stretch of Serving: into
/// that stretch's acceptor.
struct CarriedTo {
    stretch: u64,
    arrivals: mpsc::Sender<Arrival>,
}

/// Whether the Server is Serving, and which stretch of Serving it is in:
/// each time Serving starts again after it stopped, another stretch begins,
/// so what was begun in one is told apart from what is begun in the next.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ServingStretch {
    pub(crate) serving: bool,
    pub(crate) number: u64,
}

/// A byte stream a Pairing connection runs over, whatever carries it: the
/// pinned-key TLS runs over it on both sides, and the Pairing's HTTP inside
/// that.
pub(crate) trait ByteStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> ByteStream for T {}

/// Which proxy, if any, a Server dials a direct way through, chosen by the
/// way's address as HTTP clients choose one.
#[derive(Clone, Debug)]
pub struct DirectProxies {
    rules: Arc<Matcher>,
    /// Whether a loopback address is sent to a proxy as any other is.
    loopback: bool,
}

impl DirectProxies {
    /// The proxies the environment names, as HTTP clients read them:
    /// `HTTPS_PROXY`, or else `ALL_PROXY`, with any credentials it carries,
    /// for every address `NO_PROXY` does not exempt. A loopback address is
    /// never sent to one, since a proxy elsewhere could reach only its own.
    pub fn from_environment() -> Self {
        Self {
            // The environment alone: the operating system's own proxy
            // settings, which `from_system` reads wherever another dependency
            // has switched that on, are for reaching a Relay.
            rules: Arc::new(Matcher::from_env()),
            loopback: false,
        }
    }

    /// The proxies `HTTPS_PROXY` set to `https_proxy` and `NO_PROXY` to
    /// `no_proxy` would name, loopback addresses not excepted.
    pub fn given(https_proxy: &str, no_proxy: &str) -> Self {
        Self {
            rules: Arc::new(Matcher::builder().https(https_proxy).no(no_proxy).build()),
            loopback: true,
        }
    }

    /// The proxy a socket to `address` is tunnelled through, where one is.
    fn for_address(&self, address: SocketAddr) -> Option<Intercept> {
        if !self.loopback && address.ip().to_canonical().is_loopback() {
            return None;
        }
        self.rules
            .intercept(&format!("https://{address}").parse().ok()?)
    }
}

#[derive(Clone)]
pub(crate) struct ServingController {
    data_dir: PathBuf,
    local_api: LocalApi,
    active: Arc<Mutex<Option<ActiveServing>>>,
    /// How the listener stands, as the Server tells its Clients.
    listening: watch::Sender<ListenerState>,
    /// Whether the Server is Serving — accepting paired Servers at all, by
    /// whichever ways it is reached — and so whether it waits at the Relays
    /// it Serves through, and the stretch of Serving it is in.
    serving: watch::Sender<ServingStretch>,
    /// Where a connection a Relay carried to this Server goes while it is
    /// Serving: into the Serving side's acceptor, beside those dialled to its
    /// listener, for the stretch of Serving it is numbered with.
    carried: Arc<StdMutex<Option<CarriedTo>>>,
    /// Moves on with every change to the Remotes this Server is paired with —
    /// one paired, removed, or rolled back — so what follows a Remote under
    /// one Pairing hears at once that it may no longer stand.
    pairing_changes: Arc<watch::Sender<u64>>,
    /// How many Pairings were made since this Server started: the last one
    /// made's generation.
    pairings_made: Arc<AtomicU64>,
    invite_ttl: tokio::time::Duration,
    withdrawal_timeout: tokio::time::Duration,
    /// How long a connection of a Pairing may take to finish its TLS
    /// handshake before it is dropped: one to the Serving listener, or one
    /// this Server opens to a Remote.
    handshake_timeout: tokio::time::Duration,
    /// How long each new dial of a Serving Server tries its direct ways
    /// alone before starting its Relay ways beside them.
    direct_head_start: tokio::time::Duration,
    /// How long a direct connection kept for the next request may stand idle
    /// before it is closed.
    direct_idle_timeout: tokio::time::Duration,
    /// How long, at least, a Serving Server whose requests ride a joined
    /// stream goes between tries of its direct ways in the background.
    direct_retry_interval: tokio::time::Duration,
    /// How this Server makes sure the other on a joined stream still answers,
    /// on either side of it.
    joined_keepalive: JoinedKeepalive,
    invites: Arc<StdMutex<InviteLedger>>,
    protocol_version: u32,
    identity: IdentityKey,
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    remotes: Arc<RwLock<Vec<StoredRemote>>>,
    remote_clients: Arc<StdMutex<HashMap<String, Weak<PairingHttpClient>>>>,
    /// What awaits each Remote's answer, by the Remote's name, for as long as
    /// anything does.
    awaiting: Arc<StdMutex<HashMap<String, Weak<Awaiting>>>>,
    /// How many requests may await one Remote's answer at once.
    awaited_at_once: usize,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    awaiting_revocation: AwaitingRevocation,
    /// What the Serving listener proves it named an act's author with; see
    /// [`FORWARDED_AUTHOR_PROOF_HEADER`].
    forwarded_author_proof: Arc<str>,
    /// Which proxy, if any, each direct way of a Remote is dialled through.
    direct_proxies: DirectProxies,
    /// This Server's Relays, which a Relay way of a Remote is reached
    /// through and which an Invite may offer, once they are given.
    relays: GivenRelays,
    /// The addresses of the Relays this Server's user has chosen it Serve
    /// through and it has logged in at, as its Relays last said, which it
    /// tells its Peers of.
    offered_relays: Arc<watch::Sender<Vec<String>>>,
    /// How what Remotes tell of the Relays they Serve through is stored.
    told: Arc<ToldStore>,
}

/// How what a Server's Remotes tell of the Relays they Serve through is
/// stored: at most once each interval, however often they tell.
struct ToldStore {
    store_interval: tokio::time::Duration,
    /// Whether something told has yet to be stored.
    unstored: AtomicBool,
    /// Whether a store is waiting out its interval, or under way.
    storing: AtomicBool,
    /// Whether the Server has stopped, so nothing more is stored.
    stopped: AtomicBool,
}

impl ToldStore {
    /// Storing at most once each `store_interval`.
    fn every(store_interval: tokio::time::Duration) -> Self {
        Self {
            store_interval,
            unstored: AtomicBool::default(),
            storing: AtomicBool::default(),
            stopped: AtomicBool::default(),
        }
    }
}

/// This Server's Relays, as they are given to Serving once the Server has
/// them.
type GivenRelays = Arc<OnceLock<Arc<dyn RelayWays>>>;

/// This Server's Relays as Serving and the Pairings it redeemed use them
/// (ADR-0045): which it Serves through, so an Invite may offer them, and the
/// joins it asks at those it holds a Login at, so a Relay way reaches the
/// Serving Server it names.
pub(crate) trait RelayWays: Send + Sync {
    /// The address of the Relay `relay` names, written the one way a Relay's
    /// address is, where this Server Serves through it and holds a Login
    /// there not known to need renewing.
    fn served_through(&self, relay: &str) -> Option<String>;

    /// Whether this Server's user has chosen the Relay at `relay`, written
    /// the one way a Relay's address is: whether this Server holds an entry
    /// for it, logged in there or not. A Remote's Relay way at a Relay not
    /// chosen is listed with the Remote and never dialled.
    fn chosen(&self, relay: &str) -> bool;

    /// Joins this Server, at the Relay at `relay`, to the Serving Server
    /// whose identity key is `server`: the bytes the join carries, which the
    /// Pairing's pinned-key TLS runs over as it does over a direct way's
    /// socket. The join is asked only while the connection is still
    /// `wanted`. A refusal its user can do something about travels in the
    /// error as a [`RelayRefusal`].
    fn join(&self, relay: String, server: Vec<u8>, wanted: Wanted) -> RelayJoin;
}

/// Whether a connection being made to a Serving Server is still wanted:
/// whether anything is still asked of that Server by what it is made for.
#[derive(Clone)]
pub(crate) struct Wanted(watch::Receiver<()>);

impl Wanted {
    pub(crate) fn still(&self) -> bool {
        self.0.has_changed().is_ok()
    }

    /// A connection wanted for as long as `interest` stands.
    #[cfg(test)]
    pub(crate) fn while_held(interest: &watch::Sender<()>) -> Self {
        Self(interest.subscribe())
    }
}

/// The failure of a connection no longer wanted as it was being made.
pub(crate) fn no_longer_wanted() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "nothing is asked of the Serving Server any longer",
    )
}

/// A join a Server asks at a Relay, as it comes to be made or not.
pub(crate) type RelayJoin =
    Pin<Box<dyn Future<Output = std::io::Result<Box<dyn ByteStream>>> + Send>>;

/// Why a Relay way reached no Serving Server where its user can do something
/// about it — this Server holds no Login at the Relay, or the Relay will not
/// join the two Servers — rather than the way failing as any may. It travels
/// inside the I/O error the way fails with, so redeeming an Invite no way of
/// which reached its Serving Server can say so.
#[derive(Clone, Debug)]
pub(crate) struct RelayRefusal {
    pub(crate) code: SessionErrorCode,
    pub(crate) message: String,
    /// Why the Serving Server cannot be reached by the Relay way, where its
    /// user can do something about it rather than wait.
    pub(crate) unreachable: Option<UnreachableReason>,
}

impl std::fmt::Display for RelayRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RelayRefusal {}

/// The [`RelayRefusal`] `error` carries, however deeply it is wrapped. I/O
/// errors' own `source` passes over what they wrap, so each is looked inside
/// as well as past.
fn relay_refusal<'error>(
    error: &'error (dyn std::error::Error + 'static),
) -> Option<&'error RelayRefusal> {
    if let Some(refusal) = error.downcast_ref::<RelayRefusal>() {
        return Some(refusal);
    }
    let wrapped = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref);
    if let Some(refusal) = wrapped.and_then(|wrapped| relay_refusal(wrapped)) {
        return Some(refusal);
    }
    error.source().and_then(relay_refusal)
}

#[derive(Clone)]
struct LocalApi {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

/// Serving while it is on: its acceptor, taking the connections every way it
/// is reached hands it, and the listener among those ways.
struct ActiveServing {
    /// The Serving Settings last adopted.
    settings: ServingSettings,
    /// The listener, while the listener Setting has it on and it could be
    /// opened where the Settings ask.
    listener: Option<ServingListener>,
    /// Where the listener hands each connection dialled to it: into the
    /// acceptor, whichever listener is open.
    dialled: mpsc::Sender<Dialled>,
    task: JoinHandle<()>,
    connections: Arc<RevocableConnections>,
}

/// The Serving listener: the task taking each connection dialled to it into
/// the acceptor, and the revocation closing every one it took as it stops.
/// Opening and closing it leaves the acceptor, the Relays the Server waits
/// at and what they carry as they are.
struct ServingListener {
    /// Where the Settings asked it to listen.
    requested: SocketAddr,
    /// Where it listens: where it was asked to, its port chosen by the
    /// operating system where the Settings ask for none in particular.
    address: SocketAddr,
    task: JoinHandle<()>,
    /// The connections it has handed the acceptor that the acceptor has yet
    /// to take, closed as it stops however long the acceptor would take to
    /// come to them.
    waiting: Arc<StdMutex<Vec<Dialled>>>,
    /// Revoked as the listener stops, and every connection dialled to it
    /// with it.
    connections: Arc<ConnectionRevocation>,
}

/// A connection the listener took, waiting for the acceptor to take it in
/// turn — and closed there and then as the listener stops, which the
/// acceptor, taking no connection while it has as many handshakes under way
/// as it takes, may not come to for a while.
#[derive(Clone)]
struct Dialled(Arc<StdMutex<Option<Arrival>>>);

impl Dialled {
    fn new(arrival: Arrival) -> Self {
        Self(Arc::new(StdMutex::new(Some(arrival))))
    }

    /// The connection, for the acceptor to take, where its listener has not
    /// closed it.
    fn take(&self) -> Option<Arrival> {
        self.0
            .lock()
            .expect("dialled connection lock is not poisoned")
            .take()
    }

    /// Whether the acceptor has yet to take it, and its listener to close it.
    fn waiting(&self) -> bool {
        self.0
            .lock()
            .expect("dialled connection lock is not poisoned")
            .is_some()
    }
}

impl ServingListener {
    /// Listens on `listener`, opened at `address` as `requested`, handing
    /// each connection dialled to it to the acceptor through `dialled`, and
    /// telling `listening` should it stop on its own.
    fn open(
        listener: TcpListener,
        requested: SocketAddr,
        address: SocketAddr,
        dialled: mpsc::Sender<Dialled>,
        listening: watch::Sender<ListenerState>,
    ) -> Self {
        let connections = Arc::new(ConnectionRevocation::default());
        let waiting = Arc::default();
        let listened = listen(listener, dialled, Arc::clone(&waiting), connections.clone());
        let task = tokio::spawn(async move {
            let _ = std::panic::AssertUnwindSafe(listened).catch_unwind().await;
            // Closing the listener ends this before it gets here, so it gets
            // here only by stopping on its own: failing, or finding nothing
            // to hand on to.
            tracing::error!("Serving listener stopped on its own");
            listening.send_if_modified(|state| {
                let listened_here = *state == ListenerState::Open { address };
                if listened_here {
                    *state = ListenerState::Failed {
                        reason: "it stopped on its own".to_owned(),
                    };
                }
                listened_here
            });
        });
        Self {
            requested,
            address,
            task,
            waiting,
            connections,
        }
    }

    /// Whether it listens where `requested` asks — where it was asked to
    /// before, or where it came to listen — and has not stopped on its own.
    fn listens_as(&self, requested: SocketAddr) -> bool {
        (requested == self.requested || requested == self.address) && !self.task.is_finished()
    }
}

#[derive(Default)]
struct InviteLedger {
    issued: Vec<IssuedToken>,
}

struct IssuedToken {
    token: [u8; 32],
    expires_at: tokio::time::Instant,
    state: TokenState,
}

#[derive(Clone, Copy)]
enum TokenState {
    Outstanding,
    Superseded,
    Spent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredPeer {
    id: String,
    public_key: Vec<u8>,
    /// The name the Peer gave itself when it redeemed its Invite.
    #[serde(default)]
    name: String,
}

impl StoredPeer {
    /// The name the Peer is known by here: the one it was given at
    /// enrollment (see [`peer_name`]), or one of its fingerprint where it was
    /// given none.
    fn name(&self) -> String {
        if self.name.is_empty() {
            format!("peer {}", short_fingerprint(&self.id))
        } else {
            self.name.clone()
        }
    }
}

/// The most characters a Peer's name runs to.
const MAX_PEER_NAME_CHARS: usize = 64;

/// The first characters of a key fingerprint: enough to tell two Peers
/// known by one name apart wherever they are named.
fn short_fingerprint(fingerprint: &str) -> &str {
    &fingerprint[..fingerprint.len().min(8)]
}

/// The name the Peer whose key fingerprint is `id` is known by here, from the
/// `hostname` it gave itself redeeming its Invite. It is kept as given, outer
/// whitespace aside and composed canonically (NFC), where a reader can be
/// shown it as a name: something but whitespace, no longer than
/// [`MAX_PEER_NAME_CHARS`], holding no control, formatting or otherwise
/// default-ignorable character — nothing a reader could not see — and not
/// `everywhere`; otherwise the Peer is named by its fingerprint. Where one of
/// `others`, the names the other enrolled Peers go by, is already that name
/// as a reader tells names apart — by Unicode case folding — its fingerprint
/// is added to tell the two apart, more of it where that is not yet enough,
/// the name giving way for it so it never runs past the longest name. So no
/// two Peers are known by one name, and what a Sidekick on each sends is
/// attributed to it alone.
fn peer_name<'a>(
    hostname: Option<&str>,
    id: &str,
    others: impl Iterator<Item = &'a str>,
) -> String {
    let others = others.map(folded).collect::<Vec<_>>();
    let shown = |name: &str| {
        !name.is_empty()
            && name.chars().count() <= MAX_PEER_NAME_CHARS
            && !name.chars().any(unseen)
            && !crate::protocol::names_everywhere(name)
    };
    let name = hostname
        .map(|hostname| composed(hostname.trim()))
        .filter(|name| shown(name))
        .unwrap_or_else(|| format!("peer {}", short_fingerprint(id)));
    if !others.contains(&folded(&name)) {
        return name;
    }
    let mut told_apart = String::new();
    for shown_of_key in (short_fingerprint(id).len()..=id.len()).step_by(4) {
        let suffix = format!(" ({})", &id[..shown_of_key.min(id.len())]);
        let room = MAX_PEER_NAME_CHARS.saturating_sub(suffix.chars().count());
        let kept = name.chars().take(room).collect::<String>();
        told_apart = format!("{}{suffix}", kept.trim_end());
        if !others.contains(&folded(&told_apart)) {
            break;
        }
    }
    told_apart
}

/// `name` composed canonically, so one name is spelled one way however it was
/// typed.
fn composed(name: &str) -> String {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(name)
        .into_owned()
}

/// `name` as names are told apart: composed, then case folded, so `Å` and
/// `å`, or `ß` and `ss`, are one.
fn folded(name: &str) -> String {
    composed(name).to_uppercase().to_lowercase()
}

/// Whether `character` is one a reader cannot see in a name: a control, a
/// formatting character, or one Unicode says is ignorable by default.
fn unseen(character: char) -> bool {
    use icu_properties::{
        CodePointMapData, CodePointSetData,
        props::{DefaultIgnorableCodePoint, GeneralCategory},
    };
    character.is_control()
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
        || CodePointMapData::<GeneralCategory>::new().get(character) == GeneralCategory::Format
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRemote {
    #[serde(flatten)]
    remote: Remote,
    public_key: Vec<u8>,
    /// The ways that have answered, the latest first: each kind of way is
    /// dialled in this order ahead of the rest of its kind.
    #[serde(default)]
    answered: Vec<Way>,
    /// Which of the Pairings made since this Server started this one is —
    /// none, for one it started with — so a name unpaired and paired again,
    /// even to the same key, is told apart from the Pairing before it.
    #[serde(skip)]
    generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InvitePayload {
    #[serde(rename = "w")]
    ways: Vec<Way>,
    #[serde(rename = "k")]
    server_key: String,
    #[serde(rename = "t")]
    token: String,
    #[serde(rename = "h")]
    hostname: String,
}

struct ParsedInvite {
    ways: Vec<Way>,
    server_key: Vec<u8>,
    token: [u8; 32],
    hostname: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentRequest {
    token: String,
    protocol_version: u32,
    phase: EnrollmentPhase,
    /// The name the redeeming Server gives itself — its machine's hostname —
    /// by which the Serving side attributes what a Sidekick on it sends.
    /// Said only when committing, so a preparation keeps the shape every
    /// version speaks and a Server of another version is told apart by its
    /// protocol version rather than by a request it cannot read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnrollmentPhase {
    Prepare,
    Commit,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentResponse {
    hostname: String,
    protocol_version: u32,
}

/// The compatibility handshake deliberately retains the v28 wire shape so a
/// newly upgraded Server can still identify an older paired Server as a
/// protocol mismatch rather than a transport failure.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PairingHealth {
    protocol_version: u32,
}

/// What a Serving Server tells a Peer of the Relays it Serves through, a
/// line of the answer each time it tells it: the whole of them each time.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OfferedRelays {
    relays: Vec<String>,
}

#[derive(Clone)]
struct ServingState {
    controller: ServingController,
    hostname: String,
    protocol_version: u32,
}

#[derive(Clone, Debug)]
struct ServingConnectionInfo {
    peer_key: Option<Vec<u8>>,
}

pub(crate) struct PairingFailure {
    pub(crate) code: SessionErrorCode,
    pub(crate) message: String,
    /// Why the Remote could not be reached, where its user can do something
    /// about it rather than wait.
    pub(crate) unreachable: Option<UnreachableReason>,
}

impl PairingFailure {
    fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            unreachable: None,
        }
    }

    /// The failure `refusal` says, a Relay way having been refused for it.
    fn refused(refusal: RelayRefusal) -> Self {
        Self {
            code: refusal.code,
            message: refusal.message,
            unreachable: refusal.unreachable,
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self.code {
            SessionErrorCode::InviteExpired | SessionErrorCode::InviteSpent => StatusCode::GONE,
            SessionErrorCode::InviteSuperseded | SessionErrorCode::RemoteNameConflict => {
                StatusCode::CONFLICT
            }
            SessionErrorCode::PairingConnectionFailed
            | SessionErrorCode::PairingAuthenticationFailed
            | SessionErrorCode::PairingOutcomeUnknown => StatusCode::BAD_GATEWAY,
            SessionErrorCode::PairingProtocolMismatch
            | SessionErrorCode::RelayLoginNeeded
            | SessionErrorCode::RelayDifferentAccounts => StatusCode::CONFLICT,
            // A place comes free once a connection joined for the Account
            // ends, or once what awaits the Remote's answer is answered, so
            // what was asked is asked again later, as of any Remote out of
            // reach.
            SessionErrorCode::RelayCapReached | SessionErrorCode::RemoteBusy => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            // The key comes to be got once its store answers again.
            SessionErrorCode::IdentityKeyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            SessionErrorCode::PeerNotFound | SessionErrorCode::RemoteNotFound => {
                StatusCode::NOT_FOUND
            }
            _ => StatusCode::BAD_REQUEST,
        }
    }

    fn response(self) -> Response {
        crate::server::session_error_response(self.status(), self.code, self.message)
    }
}

impl StoredRemote {
    /// The Remote's ways in the order each kind of way is dialled in: those
    /// that have answered, the latest first, and then the rest as the
    /// Invite's redeemer ordered them. However recently either kind answered,
    /// a new dial tries the direct ways first.
    fn dialling_order(&self) -> Vec<Way> {
        self.answered
            .iter()
            .filter(|way| self.remote.ways.contains(way))
            .chain(
                self.remote
                    .ways
                    .iter()
                    .filter(|way| !self.answered.contains(way)),
            )
            .cloned()
            .collect()
    }
}

impl ServingController {
    pub(crate) fn new(
        data_dir: &Path,
        invite_ttl: tokio::time::Duration,
        protocol_version: u32,
        local_base_url: String,
        local_token: String,
    ) -> Result<Self> {
        let peers: Vec<StoredPeer> = read_records(&data_dir.join(PEERS_FILE))?;
        let mut revocations = peers
            .iter()
            .map(|peer| (peer.id.clone(), Arc::new(ConnectionRevocation::default())))
            .collect::<HashMap<_, _>>();
        for id in read_records::<Vec<String>>(&data_dir.join(REVOKED_PEERS_FILE))? {
            let revocation = Arc::new(ConnectionRevocation::default());
            revocation.revoke();
            revocations.insert(id, revocation);
        }
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            local_api: LocalApi {
                base_url: local_base_url,
                token: local_token,
                http: crate::runtime::loopback_http_client(),
            },
            active: Arc::new(Mutex::new(None)),
            listening: watch::Sender::new(ListenerState::Off),
            serving: watch::channel(ServingStretch::default()).0,
            carried: Arc::default(),
            pairing_changes: Arc::new(watch::channel(0).0),
            pairings_made: Arc::default(),
            invite_ttl,
            withdrawal_timeout: crate::server::ServerTimings::default().remote_withdrawal_timeout,
            handshake_timeout: crate::server::ServerTimings::default().serving_handshake_timeout,
            direct_head_start: crate::server::ServerTimings::default().direct_head_start,
            direct_idle_timeout: crate::server::ServerTimings::default().direct_idle_timeout,
            direct_retry_interval: crate::server::ServerTimings::default().direct_retry_interval,
            joined_keepalive: {
                let timings = crate::server::ServerTimings::default();
                JoinedKeepalive {
                    interval: timings.joined_stream_keepalive_interval,
                    timeout: timings.joined_stream_keepalive_timeout,
                }
            },
            invites: Arc::new(StdMutex::new(InviteLedger::default())),
            protocol_version,
            identity: IdentityKey::new(data_dir),
            peers: Arc::new(RwLock::new(peers)),
            remotes: Arc::new(RwLock::new(read_records(&data_dir.join(REMOTES_FILE))?)),
            remote_clients: Arc::new(StdMutex::new(HashMap::new())),
            awaiting: Arc::new(StdMutex::new(HashMap::new())),
            awaited_at_once: AWAITED_AT_ONCE,
            revocations: Arc::new(RwLock::new(revocations)),
            awaiting_revocation: Arc::default(),
            forwarded_author_proof: URL_SAFE_NO_PAD.encode(new_token()).into(),
            direct_proxies: DirectProxies::from_environment(),
            relays: GivenRelays::default(),
            offered_relays: Arc::new(watch::channel(Vec::new()).0),
            told: Arc::new(ToldStore::every(
                crate::server::ServerTimings::default().told_relays_store_interval,
            )),
        })
    }

    /// Bounds how long removing a Remote waits for that Remote to drop its
    /// Peer record; injectable so tests need not wait out the default.
    pub(crate) fn with_withdrawal_timeout(mut self, timeout: tokio::time::Duration) -> Self {
        self.withdrawal_timeout = timeout;
        self
    }

    /// Sets which proxy, if any, each direct way of a Remote is dialled
    /// through.
    pub(crate) fn with_direct_proxies(mut self, proxies: DirectProxies) -> Self {
        self.direct_proxies = proxies;
        self
    }

    /// Bounds how long a connection of a Pairing may take to finish its TLS
    /// handshake before it is dropped.
    pub(crate) fn with_handshake_timeout(mut self, timeout: tokio::time::Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Sets how many requests may await one Remote's answer at once.
    pub(crate) fn with_awaited_at_once(mut self, awaited_at_once: usize) -> Self {
        self.awaited_at_once = awaited_at_once;
        self
    }

    /// Sets how long each new dial of a Serving Server tries its direct ways
    /// alone before starting its Relay ways beside them.
    pub(crate) fn with_direct_head_start(mut self, head_start: tokio::time::Duration) -> Self {
        self.direct_head_start = head_start;
        self
    }

    /// Sets how long a direct connection kept for the next request may stand
    /// idle before it is closed.
    pub(crate) fn with_direct_idle_timeout(mut self, timeout: tokio::time::Duration) -> Self {
        self.direct_idle_timeout = timeout;
        self
    }

    /// Sets how long, at least, a Serving Server whose requests ride a joined
    /// stream goes between tries of its direct ways in the background.
    pub(crate) fn with_direct_retry_interval(mut self, interval: tokio::time::Duration) -> Self {
        self.direct_retry_interval = interval;
        self
    }

    /// Sets how often, at most, what Remotes tell of the Relays they Serve
    /// through is stored.
    pub(crate) fn with_told_relays_store_interval(
        mut self,
        interval: tokio::time::Duration,
    ) -> Self {
        self.told = Arc::new(ToldStore::every(interval));
        self
    }

    /// Sets how this Server makes sure the other on a joined stream still
    /// answers.
    pub(crate) fn with_joined_keepalive(mut self, keepalive: JoinedKeepalive) -> Self {
        self.joined_keepalive = keepalive;
        self
    }

    /// Sets how this Server's identity key is kept.
    pub(crate) fn with_identity_keeping(mut self, keeping: IdentityKeeping) -> Self {
        self.identity = IdentityKey::kept_in(&self.data_dir, keeping);
        self
    }

    /// Where the listener listens, if anywhere.
    pub(crate) fn address(&self) -> Option<SocketAddr> {
        match *self.listening.borrow() {
            ListenerState::Open { address } => Some(address),
            ListenerState::Off | ListenerState::Failed { .. } => None,
        }
    }

    /// How the listener stands, moving on as that changes.
    pub(crate) fn listener(&self) -> watch::Receiver<ListenerState> {
        self.listening.subscribe()
    }

    pub(crate) async fn issue_invite(
        &self,
        request: IssueInviteRequest,
    ) -> std::result::Result<IssuedInvite, PairingFailure> {
        if self.active.lock().await.is_none() {
            return Err(PairingFailure::new(
                SessionErrorCode::ServingListenerFailed,
                "Serving is disabled",
            ));
        }
        let listening = self.listening.borrow().clone();
        let ways = request
            .ways
            .iter()
            .map(|way| self.offered(way, &listening))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !ways_are_unique_and_nonempty(&ways) {
            return Err(PairingFailure::new(
                SessionErrorCode::InvalidInviteWays,
                "an Invite needs at least one unique address",
            ));
        }

        let identity = self.identity().map_err(identity_failure)?;
        let token = new_token();
        let payload = InvitePayload {
            ways: ways.clone(),
            server_key: URL_SAFE_NO_PAD.encode(&identity.public_key),
            token: URL_SAFE_NO_PAD.encode(token),
            hostname: machine_hostname(),
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).map_err(|_| {
            internal_pairing_failure(anyhow::anyhow!("could not encode Invite payload"))
        })?);
        let mut invites = self
            .invites
            .lock()
            .expect("Invite ledger lock is not poisoned");
        for issued in &mut invites.issued {
            if matches!(issued.state, TokenState::Outstanding) {
                issued.state = TokenState::Superseded;
            }
        }
        invites.issued.push(IssuedToken {
            token,
            expires_at: tokio::time::Instant::now() + self.invite_ttl,
            state: TokenState::Outstanding,
        });
        Ok(IssuedInvite {
            invite: format!("suru-v1-{encoded}"),
            ways,
        })
    }

    /// `way` as an Invite offers it: a direct way as it is, where the
    /// listener stands open as `listening` says, and a Relay way by its
    /// Relay's address written the one way, where this Server Serves through
    /// that Relay and its Login there is not known to need renewing, so it
    /// waits there to be reached while it is Serving.
    fn offered(
        &self,
        way: &Way,
        listening: &ListenerState,
    ) -> std::result::Result<Way, PairingFailure> {
        match way {
            Way::Direct(address) => match listening {
                ListenerState::Open { .. } => Ok(way.clone()),
                ListenerState::Off => Err(PairingFailure::new(
                    SessionErrorCode::InvalidInviteWays,
                    format!(
                        "the Serving listener is off, so an Invite offers none of this \
                         Server's own addresses, {address} among them; offer the Relays it \
                         Serves through, or turn the listener on"
                    ),
                )),
                ListenerState::Failed { reason } => Err(PairingFailure::new(
                    SessionErrorCode::InvalidInviteWays,
                    format!(
                        "the Serving listener is not listening ({reason}), so an Invite offers \
                         none of this Server's own addresses, {address} among them; offer the \
                         Relays it Serves through"
                    ),
                )),
            },
            Way::Relay(relay) => self
                .relays
                .get()
                .and_then(|relays| relays.served_through(relay))
                .map(Way::Relay)
                .ok_or_else(|| {
                    PairingFailure::new(
                        SessionErrorCode::InvalidInviteWays,
                        format!(
                            "an Invite offers a Relay only where this Server Serves through it \
                             and is logged in there, and the Relay at {relay} is not one"
                        ),
                    )
                }),
        }
    }

    pub(crate) fn preview_invite(
        &self,
        invite: &str,
    ) -> std::result::Result<InvitePreview, PairingFailure> {
        let invite = parse_invite(invite)?;
        Ok(InvitePreview {
            hostname: invite.hostname,
            fingerprint: fingerprint(&invite.server_key),
            ways: invite.ways,
        })
    }

    pub(crate) async fn redeem_invite(
        &self,
        request: RedeemInviteRequest,
    ) -> std::result::Result<Remote, PairingFailure> {
        let invite = parse_invite(&request.invite)?;
        let ways = ordered_ways(&invite.ways, &request.ways)?;
        if let Some(name) = request.name.as_deref() {
            validate_remote_name(name)?;
            self.ensure_remote_name_available(name)?;
        }
        let identity = self.identity().map_err(identity_failure)?;
        let prepare = EnrollmentRequest {
            token: URL_SAFE_NO_PAD.encode(invite.token),
            protocol_version: self.protocol_version,
            phase: EnrollmentPhase::Prepare,
            hostname: None,
        };
        let enrolled = dial_enrollment(
            &ways,
            &invite.server_key,
            &identity,
            self.way_dialer(&invite.server_key),
            &prepare,
        )
        .await?;
        if enrolled.protocol_version != self.protocol_version {
            return Err(protocol_mismatch(
                self.protocol_version,
                enrolled.protocol_version,
            ));
        }
        let name = request.name.unwrap_or(enrolled.hostname).trim().to_owned();
        validate_remote_name(&name)?;
        self.ensure_remote_name_available(&name)?;
        let remote = Remote {
            name: name.clone(),
            fingerprint: fingerprint(&invite.server_key),
            ways,
            status: RemoteStatus::Available,
        };
        self.persist_remote(&remote, &invite.server_key)?;

        let commit = EnrollmentRequest {
            token: URL_SAFE_NO_PAD.encode(invite.token),
            protocol_version: self.protocol_version,
            phase: EnrollmentPhase::Commit,
            hostname: Some(machine_hostname()),
        };
        if let Err(error) = dial_enrollment(
            &remote.ways,
            &invite.server_key,
            &identity,
            self.way_dialer(&invite.server_key),
            &commit,
        )
        .await
        {
            self.rollback_remote(&remote);
            return Err(error);
        }
        Ok(remote)
    }

    pub(crate) fn list_peers(&self) -> Vec<Peer> {
        self.peers
            .read()
            .expect("Peer record lock is not poisoned")
            .iter()
            .map(|peer| Peer {
                id: peer.id.clone(),
                fingerprint: peer.id.clone(),
                name: peer.name(),
            })
            .collect()
    }

    pub(crate) fn list_remotes(&self) -> Vec<Remote> {
        self.remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .map(|stored| stored.remote.clone())
            .collect()
    }

    /// The Remotes this Server is paired with, in the order paired, each
    /// with the generation of its Pairing: which of those made since this
    /// Server started it is, none for one it started with.
    pub(crate) fn remote_pairings(&self) -> Vec<(Remote, u64)> {
        self.remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .map(|stored| (stored.remote.clone(), stored.generation))
            .collect()
    }

    /// The generation of the last Pairing made: every Pairing made after
    /// this answers has a later one.
    pub(crate) fn pairings_made(&self) -> u64 {
        self.pairings_made.load(Ordering::Acquire)
    }

    /// Asks the Remote `name` whether it answers, and as what: a request
    /// awaiting its answer as any carried to it does, refused at once where
    /// as many await it as may.
    pub(crate) async fn probe_remote(
        &self,
        name: &str,
    ) -> std::result::Result<RemoteHealth, PairingFailure> {
        let remote = self.stored_remote(name)?;
        let _awaiting = self.await_answer(name, 0)?;
        let health = match self.probe_remote_connection(&remote).await {
            Ok(health) => return Ok(health),
            Err(error) if error.code == SessionErrorCode::PairingAuthenticationFailed => {
                Ok(RemoteHealth {
                    protocol_version: None,
                    status: RemoteStatus::Revoked,
                    unreachable: None,
                })
            }
            // Unavailable as any Remote no way reaches is, saying why where
            // its user can do something about it.
            Err(error)
                if matches!(
                    error.code,
                    SessionErrorCode::PairingConnectionFailed | SessionErrorCode::RelayCapReached
                ) =>
            {
                Ok(RemoteHealth {
                    protocol_version: None,
                    status: RemoteStatus::Unavailable,
                    unreachable: error.unreachable,
                })
            }
            Err(error) => Err(error),
        }?;
        self.record_remote_status(&remote, health.status);
        Ok(health)
    }

    /// Carries `request` to the Remote `name`'s Session API through the
    /// Pairing, answering what the Remote answered. `author` names who
    /// performs the act it asks for on the user's behalf — this Server's own
    /// Sidekick — and travels with it; a request carried for a Client names
    /// none, whatever it says itself.
    pub(crate) async fn proxy_remote(
        &self,
        name: &str,
        mut request: Request<Body>,
        author: Option<&Author>,
    ) -> std::result::Result<Response, PairingFailure> {
        let remote = self.stored_remote(name)?;
        // Its body is read no further than the route it names reads one, and
        // only once there is room for it among what awaits the Remote's
        // answer.
        let upload = *request.method() == Method::POST
            && canonical_forward_path(request.uri()).as_deref() == Some(ATTACHMENTS_PATH);
        let (limit, too_large): (usize, fn() -> Response) = if upload {
            (
                crate::attachments::UPLOAD_BODY_LIMIT,
                crate::server::attachment_too_large_response,
            )
        } else {
            (crate::server::COMMAND_BODY_LIMIT, || {
                StatusCode::PAYLOAD_TOO_LARGE.into_response()
            })
        };
        let declared = hyper::body::Body::size_hint(request.body())
            .exact()
            .map(|length| usize::try_from(length).unwrap_or(usize::MAX));
        if declared.is_some_and(|length| length > limit) {
            return Ok(too_large());
        }
        let _awaiting = self.await_answer(name, declared.unwrap_or(limit))?;
        // What the request said of its author goes, and so does every
        // hop-by-hop header, before the author this Server names is added
        // last, where nothing the request carried can remove it.
        let headers = request.headers_mut();
        headers.remove(AUTHOR_HEADER);
        headers.remove(FORWARDED_AUTHOR_PROOF_HEADER);
        remove_hop_by_hop_headers(headers);
        let mut added = remote_forward_headers(self.protocol_version);
        if let Some(author) = author {
            added.insert(AUTHOR_HEADER, author_header(author));
        }
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or("/", axum::http::uri::PathAndQuery::as_str);
        *request.uri_mut() = format!("/v1/pairing/proxy{path_and_query}")
            .parse()
            .map_err(|_| {
                PairingFailure::new(
                    SessionErrorCode::PairingConnectionFailed,
                    "Remote API path is invalid",
                )
            })?;
        let (parts, body) = request.into_parts();
        let body = match axum::body::to_bytes(body, limit).await {
            Ok(body) => body,
            Err(error) => {
                if error.into_inner().is::<http_body_util::LengthLimitError>() {
                    return Ok(too_large());
                }
                return Err(PairingFailure::new(
                    SessionErrorCode::PairingConnectionFailed,
                    "Remote API request body could not be read",
                ));
            }
        };
        let client = self.pairing_client(&remote)?;
        // A request that may have reached the Remote is never asked again by
        // another way unless asking twice changes nothing: only one that was
        // never delivered is.
        let repeatable = parts.method.is_safe();
        let asking = || {
            let mut request = Request::new(Body::from(body.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.headers_mut() = parts.headers.clone();
            passed_on(request, added.clone())
        };
        let response = first_remote_answer(&remote, &client, asking, |answer| {
            let client = client.clone();
            async move { judge_proxied(answer, repeatable, client) }
        })
        .await;
        match response {
            Ok((way, response)) => {
                self.classify_remote_response(&remote, &client, way, response)
                    .await
            }
            Err(error) => {
                if error.code == SessionErrorCode::PairingAuthenticationFailed {
                    self.record_remote_status(&remote, RemoteStatus::Revoked);
                }
                Err(error)
            }
        }
    }

    pub(crate) fn remove_peer(&self, id: &str) -> std::result::Result<(), PairingFailure> {
        let mut peers = self
            .peers
            .write()
            .expect("Peer record lock is not poisoned");
        let Some(index) = peers.iter().position(|peer| peer.id == id) else {
            return Err(PairingFailure::new(
                SessionErrorCode::PeerNotFound,
                "Peer not found",
            ));
        };
        let removed = peers.remove(index);
        if let Err(error) = write_private_json(&self.data_dir.join(PEERS_FILE), &*peers) {
            peers.insert(index, removed);
            return Err(internal_pairing_failure(error));
        }
        if let Some(revoked) = self
            .revocations
            .read()
            .expect("Peer revocation lock is not poisoned")
            .get(id)
            .cloned()
        {
            revoked.revoke();
        }
        self.persist_revoked_peer_ids()
            .map_err(internal_pairing_failure)?;
        Ok(())
    }

    /// Ends a Pairing from the redeeming side. The Remote is asked to drop its
    /// Peer record first, while the Pairing that authenticates the request is
    /// still in place; the local record then goes whether or not that Remote
    /// answered, because a Remote that cannot be reached is forgotten here all
    /// the same and reaching it again takes a new Invite.
    pub(crate) async fn remove_remote(
        &self,
        name: &str,
    ) -> std::result::Result<RemoteRemoval, PairingFailure> {
        let remote = self.stored_remote(name)?;
        let acknowledged = self.ask_remote_to_withdraw(&remote).await;
        self.delete_remote(name)?;
        Ok(RemoteRemoval {
            name: name.to_owned(),
            acknowledged,
        })
    }

    /// Drops the calling Peer's own record. Withdrawal is not revocation: the
    /// Peer leaves no tombstone, so its user may pair again with a new Invite.
    fn withdraw_peer(&self, public_key: &[u8]) -> std::result::Result<(), PairingFailure> {
        let id = fingerprint(public_key);
        let mut peers = self
            .peers
            .write()
            .expect("Peer record lock is not poisoned");
        let Some(index) = peers.iter().position(|peer| peer.id == id) else {
            return Err(PairingFailure::new(
                SessionErrorCode::PeerNotFound,
                "Peer not found",
            ));
        };
        let withdrawn = peers.remove(index);
        if let Err(error) = write_private_json(&self.data_dir.join(PEERS_FILE), &*peers) {
            peers.insert(index, withdrawn);
            return Err(internal_pairing_failure(error));
        }
        Ok(())
    }

    /// Dials the Remote over the Pairing listener and asks it to withdraw this
    /// Server's Peer record, within the withdrawal budget. Every dial,
    /// authentication, protocol, and timeout failure alike simply leaves the
    /// removal unacknowledged.
    async fn ask_remote_to_withdraw(&self, remote: &StoredRemote) -> bool {
        let Ok(client) = self.pairing_client(remote) else {
            return false;
        };
        let asking = || {
            Request::post(PAIRING_WITHDRAWAL_PATH)
                .body(Body::empty())
                .expect("a withdrawal is well formed")
        };
        let withdrawal = first_remote_answer(remote, &client, asking, |answer| async move {
            match answer {
                Ok(response) if response.status().is_success() => WayAttempt::Answered(()),
                Ok(_) | Err(_) => WayAttempt::TryNext,
            }
        });
        matches!(
            tokio::time::timeout(self.withdrawal_timeout, withdrawal).await,
            Ok(Ok(_))
        )
    }

    fn delete_remote(&self, name: &str) -> std::result::Result<(), PairingFailure> {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        let Some(index) = remotes.iter().position(|stored| stored.remote.name == name) else {
            return Err(PairingFailure::new(
                SessionErrorCode::RemoteNotFound,
                "Remote not found",
            ));
        };
        let removed = remotes.remove(index);
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes.insert(index, removed);
            return Err(internal_pairing_failure(error));
        }
        drop(remotes);
        // A name freed here may be paired again to a different key, so its
        // pinned client must not outlive the record it was built from.
        self.remote_clients
            .lock()
            .expect("Remote client lock is not poisoned")
            .remove(name);
        self.note_pairing_change();
        Ok(())
    }

    /// Tells whatever follows a Remote that the Remotes paired have changed.
    fn note_pairing_change(&self) {
        self.pairing_changes.send_modify(|changes| *changes += 1);
    }

    /// What moves on with every change to the Remotes this Server is paired
    /// with.
    pub(crate) fn pairing_changes(&self) -> watch::Receiver<u64> {
        self.pairing_changes.subscribe()
    }

    /// Reconciles Serving to the effective Settings before an adoption
    /// answers. Serving starting or stopping starts or stops every way this
    /// Server is reached, and starting it begins another stretch of it. The
    /// listener among those ways opens, closes or moves on its own, as the
    /// listener Setting and the address it is asked to listen at say,
    /// leaving the acceptor, the Relays the Server waits at and what they
    /// carry as they are; a listener that cannot open where it is asked to
    /// fails the adoption, leaving the one already listening, if any, where
    /// it is, and Serving going on through the Relays either way. An
    /// unchanged configuration is left alone.
    pub(crate) async fn adopt(&self, settings: ServingSettings) -> Result<()> {
        let mut active = self.active.lock().await;
        if !settings.enabled {
            self.stop_carrying();
            self.discard_invites();
            stop_active(&mut active, &self.listening).await;
            return Ok(());
        }
        if active.is_none() {
            *active = Some(self.start_serving(settings)?);
        }
        let running = active.as_mut().expect("Serving was started above");
        running.settings = settings;
        self.adopt_listener(running).await
    }

    /// Starts Serving, with no listener yet: the acceptor, taking the
    /// connections the listener and the Relays hand it, and another stretch
    /// of Serving for the Relays to wait in.
    fn start_serving(&self, settings: ServingSettings) -> Result<ActiveServing> {
        let tls = self.serving_tls()?;
        let connections = Arc::new(RevocableConnections::default());
        let (dialled, dialled_arrivals) = mpsc::channel(DIALLED_ARRIVALS_QUEUED);
        let (carried, carried_arrivals) = mpsc::channel(CARRIED_ARRIVALS_QUEUED);
        let task = tokio::spawn(serve(
            Box::pin(stream::select(
                dialled_on(dialled_arrivals),
                carried_to(carried_arrivals),
            )),
            tls,
            connections.clone(),
            self.clone(),
        ));
        // Serving that starts again after it stopped is another stretch of
        // it; the listener opening, closing or moving within it is not.
        let stretch = {
            let current = *self.serving.borrow();
            ServingStretch {
                serving: true,
                number: current.number + u64::from(!current.serving),
            }
        };
        *self
            .carried
            .lock()
            .expect("carried connection lock is not poisoned") = Some(CarriedTo {
            stretch: stretch.number,
            arrivals: carried,
        });
        self.serving
            .send_if_modified(|serving| std::mem::replace(serving, stretch) != stretch);
        Ok(ActiveServing {
            settings,
            listener: None,
            dialled,
            task,
            connections,
        })
    }

    /// Opens, closes or moves the listener of `running` Serving to what its
    /// Settings say of it, telling the Server's Clients how it then stands. A
    /// listener moved elsewhere strands the Invites that offered it where it
    /// was, so they are discarded; one closed or opened again leaves them,
    /// since they may offer Relays besides. One that stopped on its own is
    /// opened anew.
    async fn adopt_listener(&self, running: &mut ActiveServing) -> Result<()> {
        let settings = running.settings;
        if !settings.listener {
            stop_listening(&mut running.listener).await;
            tell(&self.listening, ListenerState::Off);
            return Ok(());
        }
        let requested = SocketAddr::new(settings.bind_address, settings.port);
        if running
            .listener
            .as_ref()
            .is_some_and(|listening| listening.listens_as(requested))
        {
            return Ok(());
        }
        if running
            .listener
            .as_ref()
            .is_some_and(|listening| listening.task.is_finished())
        {
            stop_listening(&mut running.listener).await;
        }
        let opened = bind_listener(requested)
            .with_context(|| format!("bind Serving listener to {requested}"))
            .and_then(|listener| {
                let address = listener
                    .local_addr()
                    .context("read bound Serving address")?;
                Ok((listener, address))
            });
        let (listener, address) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                // One already listening goes on where it is; with none, the
                // listener stands failed, saying why.
                if running.listener.is_none() {
                    tell(
                        &self.listening,
                        ListenerState::Failed {
                            reason: format!("{error:#}"),
                        },
                    );
                }
                return Err(error);
            }
        };
        if running.listener.is_some() {
            self.discard_invites();
        }
        stop_listening(&mut running.listener).await;
        running.listener = Some(ServingListener::open(
            listener,
            requested,
            address,
            running.dialled.clone(),
            self.listening.clone(),
        ));
        tell(&self.listening, ListenerState::Open { address });
        tracing::info!(%address, "Serving listener ready");
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let mut active = self.active.lock().await;
        self.stop_carrying();
        stop_active(&mut active, &self.listening).await;
        // What Remotes told that has yet to be stored is stored as the
        // Server stops, and nothing after.
        self.told.stopped.store(true, Ordering::SeqCst);
        self.store_told().await;
    }

    /// Whether the Server is Serving, and the stretch of Serving it is in,
    /// moving on as either changes.
    pub(crate) fn serving(&self) -> watch::Receiver<ServingStretch> {
        self.serving.subscribe()
    }

    /// Hands the Serving side's acceptor a connection a Relay carried to this
    /// Server, to be taken as one dialled to its listener is: through the
    /// same TLS 1.3 handshake, pinned Peer keys, enrollment and revocation.
    /// It is dropped unless the Server is still in the stretch of Serving
    /// numbered `stretch`, which the join it carries was taken up in, or
    /// where the acceptor has more waiting on it than it takes.
    pub(crate) fn accept_carried(
        &self,
        stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        stretch: u64,
    ) {
        let carried = self
            .carried
            .lock()
            .expect("carried connection lock is not poisoned")
            .as_ref()
            .filter(|carried| carried.stretch == stretch)
            .map(|carried| carried.arrivals.clone());
        let arrival = Arrival {
            stream: Box::new(stream),
            from: ArrivedFrom::Relay,
            closes_with: None,
        };
        if carried.is_none_or(|carried| carried.try_send(arrival).is_err()) {
            tracing::debug!("a connection a Relay carried was dropped untaken");
        }
    }

    /// Stops the Server waiting at its Relays and takes no more connections
    /// they carry, ahead of no longer Serving.
    fn stop_carrying(&self) {
        *self
            .carried
            .lock()
            .expect("carried connection lock is not poisoned") = None;
        self.serving
            .send_if_modified(|serving| std::mem::replace(&mut serving.serving, false));
    }

    /// This Server's own key fingerprint: what a Remote it is paired with
    /// knows it by as a Peer, and names its Sidekicks' acts by.
    pub(crate) fn own_fingerprint(&self) -> Result<String> {
        self.identity.fingerprint()
    }

    /// This Server's identity key, by which it also proves itself to a
    /// Relay.
    pub(crate) fn identity_key(&self) -> IdentityKey {
        self.identity.clone()
    }

    /// Has this Server reach Serving Servers by Relay ways, and offer in an
    /// Invite the Relays it Serves through, through `relays`: its own, given
    /// once as the Server starts, before it is asked anything.
    pub(crate) fn reach_relays_through(&self, relays: Arc<dyn RelayWays>) {
        let _ = self.relays.set(relays);
    }

    /// Has this Server tell its Peers that it Serves through the Relays at
    /// `relays`, each written the one way a Relay's address is: those its
    /// user has chosen it Serve through and it has logged in at, a Login
    /// needing renewal withdrawing none, as its Relays say whenever any of
    /// them changes — no more of them than [`MAX_TOLD_RELAYS`], none longer
    /// than [`MAX_TOLD_RELAY_LEN`]. Each Peer connected is told at once where
    /// they differ from what it was last told.
    pub(crate) fn offer_relays(&self, relays: Vec<String>) {
        self.offered_relays.send_if_modified(|offered| {
            let changed = *offered != relays;
            *offered = relays;
            changed
        });
    }

    fn identity(&self) -> Result<IdentityMaterial> {
        self.identity.material()
    }

    /// What dials the ways of the Serving Server whose identity key is
    /// `server_key`.
    fn way_dialer(&self, server_key: &[u8]) -> WayDialer {
        WayDialer {
            proxies: self.direct_proxies.clone(),
            relays: self.relays.clone(),
            server: server_key.into(),
            handshake_timeout: self.handshake_timeout,
            direct_head_start: self.direct_head_start,
            direct_idle_timeout: self.direct_idle_timeout,
            direct_retry_interval: self.direct_retry_interval,
            keepalive: self.joined_keepalive,
        }
    }

    /// The pinned-key TLS the Serving side runs over each connection it
    /// accepts, by where the connection came from.
    fn serving_tls(&self) -> Result<ServingTls> {
        let identity = self.identity()?;
        let certified = Arc::new(
            CertifiedKey::from_der(
                vec![CertificateDer::from(identity.certificate)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key)),
                &crypto_provider(),
            )
            .context("configure Serving TLS identity")?,
        );
        let tls = |identity: Arc<dyn ResolvesServerCert>, protocols: &[&[u8]]| {
            let mut tls = ServerConfig::builder_with_provider(crypto_provider())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .context("choose Serving TLS protocol versions")?
                .with_client_cert_verifier(Arc::new(PinnedPeers {
                    peers: self.peers.clone(),
                    invites: self.invites.clone(),
                    revocations: self.revocations.clone(),
                }))
                .with_cert_resolver(identity);
            tls.alpn_protocols = protocols.iter().map(|protocol| protocol.to_vec()).collect();
            anyhow::Ok(TlsAcceptor::from(Arc::new(tls)))
        };
        Ok(ServingTls {
            // A connection dialled to the listener that asks for HTTP/2 is
            // carried together; one that asks for nothing speaks HTTP/1.1,
            // as ever.
            listener: tls(
                Arc::new(rustls::sign::SingleCertAndKey::from(certified.clone())),
                &[MULTIPLEXED, b"http/1.1"],
            )?,
            carried: tls(Arc::new(MultiplexedOnly(certified)), &[MULTIPLEXED])?,
        })
    }

    fn ensure_remote_name_available(&self, name: &str) -> std::result::Result<(), PairingFailure> {
        if self
            .remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .any(|stored| stored.remote.name == name)
        {
            return Err(remote_name_conflict(name));
        }
        Ok(())
    }

    fn stored_remote(&self, name: &str) -> std::result::Result<StoredRemote, PairingFailure> {
        self.remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .find(|stored| stored.remote.name == name)
            .cloned()
            .ok_or_else(|| {
                PairingFailure::new(SessionErrorCode::RemoteNotFound, "Remote not found")
            })
    }

    /// Takes room among what awaits the Remote `name`'s answer for a request
    /// whose body holds as much as `body` bytes, held until it is answered or
    /// fails — refused at once where there is none, past as many requests as
    /// may await it at once or [`AWAITED_BODY_BYTES`].
    fn await_answer(
        &self,
        name: &str,
        body: usize,
    ) -> std::result::Result<AwaitingAnswer, PairingFailure> {
        let awaiting = {
            let mut awaiting = self
                .awaiting
                .lock()
                .expect("awaited answers lock is not poisoned");
            awaiting.retain(|_, held| held.strong_count() > 0);
            if let Some(held) = awaiting.get(name).and_then(Weak::upgrade) {
                held
            } else {
                let held = Arc::new(Awaiting::new(self.awaited_at_once));
                awaiting.insert(name.to_owned(), Arc::downgrade(&held));
                held
            }
        };
        let busy = || {
            PairingFailure::new(
                SessionErrorCode::RemoteBusy,
                "as many requests await the Remote's answer as it is asked at once; try again \
                 shortly",
            )
        };
        let request = awaiting
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let body = u32::try_from(body)
            .ok()
            .and_then(|bytes| awaiting.bytes.clone().try_acquire_many_owned(bytes).ok())
            .ok_or_else(busy)?;
        Ok(AwaitingAnswer {
            _request: request,
            _body: body,
            _awaiting: awaiting,
        })
    }

    fn record_remote_status(&self, remote: &StoredRemote, status: RemoteStatus) {
        self.record_remote_state(remote, status, None);
    }

    fn record_remote_connection(&self, remote: &StoredRemote, status: RemoteStatus, way: Way) {
        self.record_remote_state(remote, status, Some(way));
    }

    /// Notes that `remote` answered what was asked of it through `client` by
    /// `way`, standing as `status`; and, where it answers as a Remote paired
    /// with this Server does, has what it tells of the Relays it Serves
    /// through followed over `client` from now on.
    fn answered(
        &self,
        remote: &StoredRemote,
        client: &Arc<PairingHttpClient>,
        way: Way,
        status: RemoteStatus,
    ) {
        self.record_remote_connection(remote, status, way);
        if status == RemoteStatus::Available {
            self.keep_up_with(remote, client);
        }
    }

    /// Follows over `client`, where nothing follows it there yet, what the
    /// Remote `remote` tells of the Relays it Serves through: its Relay ways
    /// keep up with what it tells, as it is connected to and then each time
    /// that changes, for as long as anything asks the Remote through
    /// `client`. Where following ends because the answer it rode ended, or
    /// no way carried it, it is followed again once the Remote next answers,
    /// at once where it has answered meanwhile.
    fn keep_up_with(&self, remote: &StoredRemote, client: &Arc<PairingHttpClient>) {
        client.answers.fetch_add(1, Ordering::SeqCst);
        if client.keeping_up.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(self.clone().keep_up(remote.clone(), client.clone()));
    }

    async fn keep_up(self, remote: StoredRemote, client: Arc<PairingHttpClient>) {
        let held = Arc::downgrade(&client);
        let mut interest = client.interest.subscribe();
        let mut asking = Some(client);
        while let Some(client) = asking.take() {
            let answers = client.answers.load(Ordering::SeqCst);
            match self
                .follow_told(&remote, client, &held, &mut interest)
                .await
            {
                Followed::LetGo | Followed::Refused => return,
                Followed::Lost => {}
            }
            let Some(client) = held.upgrade() else {
                return;
            };
            client.keeping_up.store(false, Ordering::SeqCst);
            // Followed again at once where the Remote has answered meanwhile,
            // unless what noted that answer follows it already.
            if client.answers.load(Ordering::SeqCst) != answers
                && !client.keeping_up.swap(true, Ordering::SeqCst)
            {
                asking = Some(client);
            }
        }
    }

    /// Asks the Remote `remote` through `client` which Relays it Serves
    /// through, and takes up what it tells: first holding `client` until the
    /// Remote has told it once — as it does at once, so within the handshake
    /// timeout — so what it tells as it is connected to is taken up however
    /// soon what connected lets it go; and then, holding nothing, each change
    /// it tells, until nothing asks the Remote through the client any longer
    /// (`interest`) or its Pairing no longer stands as it did.
    async fn follow_told(
        &self,
        remote: &StoredRemote,
        client: Arc<PairingHttpClient>,
        held: &Weak<PairingHttpClient>,
        interest: &mut watch::Receiver<()>,
    ) -> Followed {
        let protocol_version = self.protocol_version;
        // `client`, and with it the interest in the Remote that keeps its
        // connections, is held no longer than this: a probe that lets the
        // Remote go at once leaves it in view for the handshake timeout at
        // most, and over a connection already standing, for no longer than
        // its answer takes.
        let first = tokio::time::timeout(self.handshake_timeout, async {
            let asking = || {
                let mut request = Request::get(OFFERED_RELAYS_PATH)
                    .body(Body::empty())
                    .expect("asking which Relays are offered is well formed");
                request
                    .headers_mut()
                    .extend(remote_forward_headers(protocol_version));
                request
            };
            let answer = first_remote_answer(remote, &client, asking, |answer| async move {
                match answer {
                    Ok(response) => WayAttempt::Answered(response),
                    Err(_) => WayAttempt::TryNext,
                }
            })
            .await;
            let Ok((_, response)) = answer else {
                return Err(Followed::Lost);
            };
            if !response.status().is_success() {
                return Err(Followed::Refused);
            }
            let mut told = Telling::new(response.into_body());
            let relays = told.next().await?;
            Ok((told, relays))
        })
        .await;
        let (mut told, relays) = match first {
            Ok(Ok(first)) => first,
            Ok(Err(followed)) => return followed,
            Err(_) => return Followed::Lost,
        };
        if !self.take_up_told(remote, held, &relays) {
            return Followed::LetGo;
        }
        drop(client);
        let mut pairings = self.pairing_changes();
        loop {
            tokio::select! {
                biased;
                () = async { while interest.changed().await.is_ok() {} } => return Followed::LetGo,
                changed = pairings.changed() => {
                    if changed.is_err() || !self.still_followed(remote, held) {
                        return Followed::LetGo;
                    }
                }
                relays = told.next() => match relays {
                    Ok(relays) if self.take_up_told(remote, held, &relays) => {}
                    Ok(_) => return Followed::LetGo,
                    Err(followed) => return followed,
                },
            }
        }
    }

    /// Takes up `relays` as the Relays the Remote `followed` Serves through,
    /// as it told over `client`: its Relay ways come to be those — each it
    /// had already where it was, and the rest after them — its direct ways
    /// stay as its Invite gave them, and the ways that last answered are
    /// those of them that still stand. Taken up only while `client` is the
    /// one this Server asks the Remote through and the Remote's Pairing
    /// stands as it did, answering whether they do: what was told over an
    /// earlier client, or under an earlier Pairing, is never taken up after
    /// what was told since, since following over one client ends before the
    /// next is made. It holds at once, and is stored soon after
    /// ([`Self::store_told_soon`]).
    fn take_up_told(
        &self,
        followed: &StoredRemote,
        client: &Weak<PairingHttpClient>,
        relays: &[String],
    ) -> bool {
        let clients = self
            .remote_clients
            .lock()
            .expect("Remote client lock is not poisoned");
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        let Some(index) = followed_at(&clients, &remotes, followed, client) else {
            return false;
        };
        let ways = kept_up(&remotes[index].remote.ways, relays);
        if ways == remotes[index].remote.ways {
            return true;
        }
        remotes[index].answered.retain(|way| ways.contains(way));
        remotes[index].remote.ways = ways;
        drop((remotes, clients));
        self.store_told_soon();
        true
    }

    /// Has the Remotes' records stored once the store interval has passed,
    /// where what they told of the Relays they Serve through has yet to be:
    /// so however often they tell, the records are stored at most once each
    /// interval, and never on the async workers. What was told meanwhile
    /// holds at once, and is stored with whatever else is stored first.
    fn store_told_soon(&self) {
        self.told.unstored.store(true, Ordering::SeqCst);
        if self.told.storing.swap(true, Ordering::SeqCst) {
            return;
        }
        let controller = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(controller.told.store_interval).await;
                if controller.told.stopped.load(Ordering::SeqCst) {
                    return;
                }
                controller.store_told().await;
                controller.told.storing.store(false, Ordering::SeqCst);
                if !controller.told.unstored.load(Ordering::SeqCst)
                    || controller.told.storing.swap(true, Ordering::SeqCst)
                {
                    return;
                }
            }
        });
    }

    /// Stores the Remotes' records as they stand where what they told of the
    /// Relays they Serve through has yet to be, on a thread of its own; one
    /// that cannot be stored is tried again an interval later.
    async fn store_told(&self) {
        if !self.told.unstored.swap(false, Ordering::SeqCst) {
            return;
        }
        let controller = self.clone();
        let stored = tokio::task::spawn_blocking(move || {
            let remotes = controller
                .remotes
                .read()
                .expect("Remote record lock is not poisoned");
            write_private_json(&controller.data_dir.join(REMOTES_FILE), &*remotes)
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("the store was given up: {error}")));
        if let Err(error) = stored {
            self.told.unstored.store(true, Ordering::SeqCst);
            tracing::warn!("could not store the Relays a Remote Serves through: {error:#}");
        }
    }

    /// Whether the Remote `followed` is still asked through `client`, under
    /// the Pairing it was followed under.
    fn still_followed(&self, followed: &StoredRemote, client: &Weak<PairingHttpClient>) -> bool {
        let clients = self
            .remote_clients
            .lock()
            .expect("Remote client lock is not poisoned");
        let remotes = self
            .remotes
            .read()
            .expect("Remote record lock is not poisoned");
        followed_at(&clients, &remotes, followed, client).is_some()
    }

    /// Records how `remote` stands, and the way it last answered by where it
    /// did — of the Pairing it was asked under alone, so nothing that Pairing
    /// answers is recorded of another made since under its name.
    fn record_remote_state(
        &self,
        remote: &StoredRemote,
        status: RemoteStatus,
        answered: Option<Way>,
    ) {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        let Some(index) = remotes
            .iter()
            .position(|stored| same_pairing(stored, remote))
        else {
            return;
        };
        // A way the Remote no longer offers — dropped as it answered — is
        // remembered as answering no longer.
        let answered = answered.filter(|way| remotes[index].remote.ways.contains(way));
        let previous_status = remotes[index].remote.status;
        if previous_status == status
            && answered
                .as_ref()
                .is_none_or(|way| remotes[index].answered.first() == Some(way))
        {
            return;
        }
        let previous_answered = remotes[index].answered.clone();
        remotes[index].remote.status = status;
        if let Some(way) = answered {
            let answered = &mut remotes[index].answered;
            answered.retain(|known| *known != way);
            answered.insert(0, way);
        }
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes[index].remote.status = previous_status;
            remotes[index].answered = previous_answered;
            tracing::warn!("could not persist Remote status: {error:#}");
        }
    }

    fn persist_revoked_peer_ids(&self) -> Result<()> {
        let revoked = self
            .revocations
            .read()
            .expect("Peer revocation lock is not poisoned")
            .iter()
            .filter(|(_, revocation)| revocation.revoked.load(Ordering::Acquire))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        write_private_json(&self.data_dir.join(REVOKED_PEERS_FILE), &revoked)
    }

    async fn probe_remote_connection(
        &self,
        remote: &StoredRemote,
    ) -> std::result::Result<RemoteHealth, PairingFailure> {
        let client = self.pairing_client(remote)?;
        let asking = || {
            Request::get("/health")
                .body(Body::empty())
                .expect("a health check is well formed")
        };
        let (way, pairing_health) = first_remote_answer(remote, &client, asking, |answer| async {
            match answer {
                Ok(response) if response.status().is_success() => {
                    match small_answer::<PairingHealth>(response).await {
                        Some(health) => WayAttempt::Answered(health),
                        None => WayAttempt::Rejected(PairingFailure::new(
                            SessionErrorCode::PairingConnectionFailed,
                            "Remote returned an invalid health response",
                        )),
                    }
                }
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    WayAttempt::Rejected(PairingFailure::new(
                        SessionErrorCode::PairingAuthenticationFailed,
                        "Remote refused this Server's key",
                    ))
                }
                Ok(_) | Err(_) => WayAttempt::TryNext,
            }
        })
        .await?;
        let health = RemoteHealth {
            protocol_version: Some(pairing_health.protocol_version),
            status: if pairing_health.protocol_version == self.protocol_version {
                RemoteStatus::Available
            } else {
                RemoteStatus::ProtocolMismatch
            },
            unreachable: None,
        };
        self.answered(remote, &client, way, health.status);
        Ok(health)
    }

    /// The client `remote` is asked through, pinning its key: the one asked
    /// through already where it was made for that very Pairing, and
    /// otherwise one made afresh. Where the Pairing `remote` was read under
    /// no longer stands — removed since, or another paired under its name —
    /// nothing is asked of it, nor of the Pairing in its place, as it was
    /// asked: it is not found. Judged as the client is chosen, so no client
    /// is made for a Pairing that has ended.
    fn pairing_client(
        &self,
        remote: &StoredRemote,
    ) -> std::result::Result<Arc<PairingHttpClient>, PairingFailure> {
        let mut clients = self
            .remote_clients
            .lock()
            .expect("Remote client lock is not poisoned");
        let standing = self
            .remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .any(|stored| same_pairing(stored, remote));
        if !standing {
            return Err(PairingFailure::new(
                SessionErrorCode::RemoteNotFound,
                "Remote not found",
            ));
        }
        if let Some(client) = clients
            .get(&remote.remote.name)
            .and_then(Weak::upgrade)
            .filter(|client| client.pins(remote))
        {
            return Ok(client);
        }
        let identity = self.identity().map_err(identity_failure)?;
        let client = Arc::new(PairingHttpClient {
            generation: remote.generation,
            ..paired_http_client(
                &remote.public_key,
                &identity,
                None,
                self.way_dialer(&remote.public_key),
            )
            .map_err(internal_pairing_failure)?
        });
        clients.insert(remote.remote.name.clone(), Arc::downgrade(&client));
        Ok(client)
    }

    async fn classify_remote_response(
        &self,
        remote: &StoredRemote,
        client: &Arc<PairingHttpClient>,
        way: Way,
        response: Response,
    ) -> std::result::Result<Response, PairingFailure> {
        if response.status() == StatusCode::UNAUTHORIZED {
            self.record_remote_connection(remote, RemoteStatus::Revoked, way);
            return Err(PairingFailure::new(
                SessionErrorCode::PairingAuthenticationFailed,
                "Remote refused this Server's key",
            ));
        }
        if response.status() != StatusCode::CONFLICT {
            self.answered(remote, client, way, RemoteStatus::Available);
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, PAIRING_ANSWER_BUDGET)
            .await
            .map_err(|_| {
                PairingFailure::new(
                    SessionErrorCode::PairingOutcomeUnknown,
                    "Remote API conflict response could not be read",
                )
            })?;
        let protocol_mismatch = serde_json::from_slice::<SessionError>(&body)
            .is_ok_and(|error| error.code == SessionErrorCode::PairingProtocolMismatch);
        let status = if protocol_mismatch {
            RemoteStatus::ProtocolMismatch
        } else {
            RemoteStatus::Available
        };
        self.answered(remote, client, way, status);
        Ok(Response::from_parts(parts, Body::from(body)))
    }

    fn is_enrolled_peer(&self, public_key: &[u8]) -> bool {
        self.enrolled_peer(public_key).is_some()
    }

    /// The Peer enrolled with `public_key`, by its key fingerprint and the
    /// name it is known by, where one is.
    fn enrolled_peer(&self, public_key: &[u8]) -> Option<(String, String)> {
        self.peers
            .read()
            .expect("Peer record lock is not poisoned")
            .iter()
            .find(|peer| bool::from(peer.public_key.as_slice().ct_eq(public_key)))
            .map(|peer| (peer.id.clone(), peer.name()))
    }

    /// Who performs the act a request to this Server's own Session API asks
    /// for, where it names anyone: a Sidekick on a Peer, as the Serving
    /// listener named it on carrying the Peer's request here. A request
    /// naming an author the listener did not set — a Client's, whatever it
    /// claims — is refused, since a Client acts as the user.
    pub(crate) fn forwarded_author(
        &self,
        headers: &HeaderMap,
    ) -> std::result::Result<Option<Author>, ForgedAuthor> {
        let Some(named) = headers.get(AUTHOR_HEADER) else {
            return Ok(None);
        };
        let proven = headers
            .get(FORWARDED_AUTHOR_PROOF_HEADER)
            .is_some_and(|proof| {
                bool::from(
                    proof
                        .as_bytes()
                        .ct_eq(self.forwarded_author_proof.as_bytes()),
                )
            });
        if !proven {
            return Err(ForgedAuthor);
        }
        read_author_header(named).map(Some).ok_or(ForgedAuthor)
    }

    fn discard_invites(&self) {
        self.invites
            .lock()
            .expect("Invite ledger lock is not poisoned")
            .issued
            .clear();
    }

    fn persist_remote(
        &self,
        remote: &Remote,
        public_key: &[u8],
    ) -> std::result::Result<(), PairingFailure> {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        if remotes.iter().any(|known| known.remote.name == remote.name) {
            return Err(remote_name_conflict(&remote.name));
        }
        remotes.push(StoredRemote {
            remote: remote.clone(),
            public_key: public_key.to_vec(),
            answered: Vec::new(),
            generation: self.pairings_made.fetch_add(1, Ordering::AcqRel) + 1,
        });
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes.pop();
            return Err(internal_pairing_failure(error));
        }
        drop(remotes);
        self.note_pairing_change();
        Ok(())
    }

    fn rollback_remote(&self, remote: &Remote) {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        remotes.retain(|known| {
            known.remote.name != remote.name || known.remote.fingerprint != remote.fingerprint
        });
        let _ = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes);
        drop(remotes);
        self.note_pairing_change();
    }

    fn enroll(
        &self,
        request: EnrollmentRequest,
        public_key: Vec<u8>,
    ) -> std::result::Result<(), PairingFailure> {
        if request.protocol_version != self.protocol_version {
            return Err(protocol_mismatch(
                self.protocol_version,
                request.protocol_version,
            ));
        }
        let token = decode_token(&request.token)?;
        let mut invites = self
            .invites
            .lock()
            .expect("Invite ledger lock is not poisoned");
        let Some(issued) = invites
            .issued
            .iter_mut()
            .find(|issued| bool::from(issued.token.ct_eq(&token)))
        else {
            return Err(PairingFailure::new(
                SessionErrorCode::InviteSpent,
                "Invite token is unknown or already discarded",
            ));
        };
        match issued.state {
            TokenState::Superseded => {
                return Err(PairingFailure::new(
                    SessionErrorCode::InviteSuperseded,
                    "Invite was superseded by a newer Invite",
                ));
            }
            TokenState::Spent => {
                return Err(PairingFailure::new(
                    SessionErrorCode::InviteSpent,
                    "Invite has already been spent",
                ));
            }
            TokenState::Outstanding if tokio::time::Instant::now() >= issued.expires_at => {
                return Err(PairingFailure::new(
                    SessionErrorCode::InviteExpired,
                    "Invite has expired",
                ));
            }
            TokenState::Outstanding => {}
        }

        if matches!(request.phase, EnrollmentPhase::Prepare) {
            return Ok(());
        }

        let id = fingerprint(&public_key);
        let mut peers = self
            .peers
            .write()
            .expect("Peer record lock is not poisoned");
        let others = peers
            .iter()
            .filter(|peer| peer.id != id)
            .map(StoredPeer::name)
            .collect::<Vec<_>>();
        let name = peer_name(
            request.hostname.as_deref(),
            &id,
            others.iter().map(String::as_str),
        );
        let previous_peers = peers.clone();
        if let Some(existing) = peers.iter_mut().find(|peer| peer.id == id) {
            existing.public_key = public_key;
            existing.name = name;
        } else {
            peers.push(StoredPeer {
                id: id.clone(),
                public_key,
                name,
            });
        }
        if let Err(error) = write_private_json(&self.data_dir.join(PEERS_FILE), &*peers) {
            *peers = previous_peers;
            return Err(internal_pairing_failure(error));
        }
        // A key's revocation stands until it is revoked, so the connections
        // that answer to it — those kept across a withdrawal among them —
        // answer to it however often the key enrolls again; only a removed
        // key's tombstone gives way to a fresh one. The connections awaiting
        // the key's next revocation answer to it from now on, used since or
        // not.
        let mut revocations = self
            .revocations
            .write()
            .expect("Peer revocation lock is not poisoned");
        let revocation = match revocations
            .get(&id)
            .filter(|revocation| revocation.is_live())
        {
            Some(revocation) => revocation.clone(),
            None => {
                let fresh = Arc::new(ConnectionRevocation::default());
                revocations.insert(id.clone(), fresh.clone());
                fresh
            }
        };
        let awaiting = self
            .awaiting_revocation
            .lock()
            .expect("awaiting revocation lock is not poisoned")
            .remove(&id);
        for connection in awaiting.into_iter().flatten().filter_map(|c| c.upgrade()) {
            revocation.revokes_with_it(&connection);
        }
        drop(revocations);
        if let Err(error) = self.persist_revoked_peer_ids() {
            tracing::warn!("could not persist revoked Peer tombstones: {error:#}");
        }
        issued.state = TokenState::Spent;
        Ok(())
    }
}

/// Binds the Serving listener. An IPv6 bind clears `IPV6_V6ONLY` first so the
/// default `::` accepts IPv4 dialers too: Linux and macOS leave the flag off,
/// but Windows sets it, which would strand every IPv4 address an Invite
/// offers.
fn bind_listener(requested: SocketAddr) -> Result<TcpListener> {
    let domain = match requested {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
        .context("open Serving socket")?;
    if requested.is_ipv6() {
        socket
            .set_only_v6(false)
            .context("open the Serving socket to both stacks")?;
    }
    // Unix rebinds race the previous listener's TIME_WAIT; Windows reuse has
    // different semantics, and its default already allows the rebind.
    #[cfg(unix)]
    socket
        .set_reuse_address(true)
        .context("allow Serving listener rebinds")?;
    socket
        .bind(&requested.into())
        .context("bind Serving socket")?;
    socket.listen(1024).context("listen on Serving socket")?;
    socket
        .set_nonblocking(true)
        .context("prepare Serving socket for the async runtime")?;
    TcpListener::from_std(socket.into()).context("adopt Serving socket into the async runtime")
}

/// Stops Serving: the listener, the acceptor, and every connection either
/// took, whichever way it came, telling the Server's Clients the listener is
/// off.
async fn stop_active(active: &mut Option<ActiveServing>, listening: &watch::Sender<ListenerState>) {
    if let Some(mut running) = active.take() {
        stop_listening(&mut running.listener).await;
        running.task.abort();
        let _ = running.task.await;
        running.connections.revoke_all();
        tracing::info!("Serving stopped");
    }
    tell(listening, ListenerState::Off);
}

/// Closes `listener`, where it is open, its port released before this
/// answers, and every connection dialled to it with it — those it has handed
/// on that the acceptor has yet to take among them. Nothing a Relay carried
/// is touched.
async fn stop_listening(listener: &mut Option<ServingListener>) {
    if let Some(listening) = listener.take() {
        listening.task.abort();
        let _ = listening.task.await;
        let waiting = std::mem::take(
            &mut *listening
                .waiting
                .lock()
                .expect("waiting connection lock is not poisoned"),
        );
        for dialled in waiting {
            drop(dialled.take());
        }
        listening.connections.revoke();
        tracing::info!("Serving listener stopped");
    }
}

/// Tells the Server's Clients the listener stands as `state`, where that is
/// news to them.
fn tell(listening: &watch::Sender<ListenerState>, state: ListenerState) {
    listening.send_if_modified(|told| {
        let news = *told != state;
        if news {
            *told = state;
        }
        news
    });
}

/// Serves the Pairing's routes over each connection from `arrivals`, once
/// it has passed the pinned-key TLS handshake.
async fn serve(
    arrivals: Arrivals,
    tls: ServingTls,
    connections: Arc<RevocableConnections>,
    controller: ServingController,
) {
    let mut acceptor = PairingAcceptor {
        arrivals: arrivals.fuse(),
        tls,
        handshake_timeout: controller.handshake_timeout,
        handshakes: tokio::task::JoinSet::new(),
        revocations: controller.revocations.clone(),
        awaiting_revocation: controller.awaiting_revocation.clone(),
        connections,
    };
    let protocol_version = controller.protocol_version;
    let keepalive = controller.joined_keepalive;
    let startup = controller.handshake_timeout;
    let state = ServingState {
        controller,
        hostname: machine_hostname(),
        protocol_version,
    };
    let app = Router::new()
        .route("/health", get(serving_health))
        .route("/v1/pairing/enroll", post(enroll_peer))
        .route(PAIRING_WITHDRAWAL_PATH, post(withdraw_peer))
        .route(OFFERED_RELAYS_PATH, get(tell_offered_relays))
        .route("/v1/pairing/proxy/{*path}", any(forward_peer_api))
        .with_state(state);
    loop {
        let (stream, connection) = acceptor.accept().await;
        tokio::spawn(serve_connection(
            stream,
            connection,
            app.clone(),
            keepalive,
            startup,
        ));
    }
}

/// Serves `app` over `stream`, the connection the Server `connection` names
/// made: as HTTP/2 where its TLS handshake agreed it — a Relay way's, carrying
/// everything asked by that way together, and making sure as `keepalive`
/// says that the redeeming Server still answers once that Server has begun
/// HTTP/2 within `startup` — and as HTTP/1.1 otherwise, as a direct way's
/// always has. A connection a Relay carried is only ever served as HTTP/2.
async fn serve_connection(
    stream: RevocableTlsStream,
    connection: ServingConnectionInfo,
    app: Router,
    keepalive: JoinedKeepalive,
    startup: tokio::time::Duration,
) {
    let Some(multiplexed) = stream.multiplexed() else {
        return;
    };
    let app = app.layer(axum::Extension(ConnectInfo(connection)));
    // However the connection ends — closed, revoked, failing, or given up —
    // there is nothing more to do with it.
    if multiplexed {
        serve_joined(stream, app, keepalive, startup).await;
    } else {
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), TowerToHyperService::new(app))
            .with_upgrades()
            .await;
    }
}

/// Serves `app` as HTTP/2 over `transport`, a joined stream's: making sure
/// as `keepalive` says that the redeeming Server still answers once it has
/// begun HTTP/2 within `startup`, and letting the transport go once that
/// Server is judged gone, as [`Liveness`] says.
async fn serve_joined(
    transport: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    app: Router,
    keepalive: JoinedKeepalive,
    startup: tokio::time::Duration,
) {
    let (transport, liveness) = Liveness::watch(transport, Preface::of_client());
    let mut server = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
    server
        .timer(TokioTimer::new())
        .keep_alive_interval(keepalive.interval)
        .keep_alive_timeout(keepalive.timeout)
        .max_concurrent_streams(JOINED_STREAMS)
        .initial_stream_window_size(JOINED_STREAM_WINDOW)
        .initial_connection_window_size(JOINED_CONNECTION_WINDOW)
        .max_send_buf_size(JOINED_STREAM_WINDOW as usize);
    tokio::select! {
        _ = server.serve_connection(TokioIo::new(transport), TowerToHyperService::new(app)) => {}
        () = liveness.lost(startup, keepalive) => {}
    }
}

/// What a joined stream's transport shows of the Server at its far end, as
/// the Server on either side watches it, and the transport itself, which is
/// given up — dropped there and then, whatever it was still sending — once
/// that Server is judged gone.
///
/// HTTP/2's keepalive judges the stream only once it is under way, so until
/// the other Server's HTTP/2 preface has come the stream is given up where
/// that has not come within the startup time it is given. From then on the
/// keepalive judges it ([`JoinedKeepalive`]), but its verdict closes the
/// stream gracefully, flushing what is queued first, and a transport whose
/// far end takes nothing in never flushes. So the transport is given up
/// regardless once it has stalled — the other Server sending nothing, or
/// what is sent it going nowhere — for the keepalive's `interval` and
/// `timeout`, by when the keepalive has given the stream up, and `timeout`
/// again for the close.
struct Liveness<S> {
    transport: StdMutex<Option<S>>,
    /// Wakes whatever last used the transport, so it finds it given up.
    waker: AtomicWaker,
    /// Whether the other Server's HTTP/2 preface has all come.
    begun: watch::Sender<bool>,
    /// What has last moved over the transport.
    moved: StdMutex<Moved>,
    /// Whether the transport has gone: given up, or let go by what used it.
    gone: watch::Sender<bool>,
}

/// What has last moved over a joined stream's transport.
#[derive(Clone, Copy)]
struct Moved {
    /// When the other Server last sent anything.
    heard: tokio::time::Instant,
    /// Since when what is sent it has gone nowhere, while it has not.
    held_since: Option<tokio::time::Instant>,
}

impl<S> Liveness<S> {
    /// `transport` as HTTP/2 is run over it, the other Server's `preface`
    /// expected first, and what watches it.
    fn watch(transport: S, preface: Preface) -> (Watched<S>, Arc<Self>) {
        let liveness = Arc::new(Self {
            transport: StdMutex::new(Some(transport)),
            waker: AtomicWaker::new(),
            begun: watch::Sender::new(false),
            moved: StdMutex::new(Moved {
                heard: tokio::time::Instant::now(),
                held_since: None,
            }),
            gone: watch::Sender::new(false),
        });
        let watched = Watched {
            liveness: liveness.clone(),
            preface,
        };
        (watched, liveness)
    }

    /// Waits for the other Server to begin HTTP/2, giving the transport up
    /// where it has not within `startup`.
    async fn begun_within(&self, startup: tokio::time::Duration) -> std::io::Result<()> {
        let mut begun = self.begun.subscribe();
        if let Ok(Ok(_)) = tokio::time::timeout(startup, begun.wait_for(|begun| *begun)).await {
            return Ok(());
        }
        self.give_up();
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the other Server did not begin HTTP/2 over the joined stream in time",
        ))
    }

    /// Resolves once the stream is judged gone, having given its transport
    /// up, or once the transport has gone otherwise: the other Server not
    /// beginning HTTP/2 within `startup`, or, once it has, the transport
    /// stalling for as long as `keepalive` gives the stream and its close.
    async fn lost(&self, startup: tokio::time::Duration, keepalive: JoinedKeepalive) {
        let judging = async {
            if self.begun_within(startup).await.is_err() {
                return;
            }
            let bound = keepalive.interval + keepalive.timeout * 2;
            loop {
                let given_up_at = self.stalled_since() + bound;
                if tokio::time::Instant::now() >= given_up_at {
                    self.give_up();
                    return;
                }
                tokio::time::sleep_until(given_up_at).await;
            }
        };
        let mut gone = self.gone.subscribe();
        tokio::select! {
            () = judging => {}
            _ = gone.wait_for(|gone| *gone) => {}
        }
    }

    /// Since when the transport has stalled, as far as can be told: since
    /// the other Server last sent anything, or since what is sent it last
    /// went anywhere, whichever is longer ago.
    fn stalled_since(&self) -> tokio::time::Instant {
        let moved = *self
            .moved
            .lock()
            .expect("joined stream movement lock is not poisoned");
        moved
            .held_since
            .map_or(moved.heard, |held_since| held_since.min(moved.heard))
    }

    /// Notes that what is sent the other Server is going nowhere just now
    /// where `held`, and that it is moving otherwise.
    fn note_sending(&self, held: bool) {
        let mut moved = self
            .moved
            .lock()
            .expect("joined stream movement lock is not poisoned");
        moved.held_since = held.then(|| moved.held_since.unwrap_or_else(tokio::time::Instant::now));
    }

    /// Drops the transport, and has whatever uses it find it gone.
    fn give_up(&self) {
        drop(
            self.transport
                .lock()
                .expect("joined stream transport lock is not poisoned")
                .take(),
        );
        self.waker.wake();
        self.gone.send_replace(true);
    }
}

/// Gives a joined stream's transport up as it goes, unless it is kept.
struct GivenUpUnlessKept<S>(Option<Arc<Liveness<S>>>);

impl<S> GivenUpUnlessKept<S> {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl<S> Drop for GivenUpUnlessKept<S> {
    fn drop(&mut self) {
        if let Some(liveness) = self.0.take() {
            liveness.give_up();
        }
    }
}

/// A joined stream's transport as HTTP/2 runs over it, watched as
/// [`Liveness`] says. Once given up, it fails whatever is asked of it.
struct Watched<S> {
    liveness: Arc<Liveness<S>>,
    /// What is still to come of the other Server's preface.
    preface: Preface,
}

impl<S> Drop for Watched<S> {
    /// The transport goes with what used it.
    fn drop(&mut self) {
        self.liveness.give_up();
    }
}

impl<S: Unpin> Watched<S> {
    /// Sends over the transport as `poll` does, noting whether what is sent
    /// is going anywhere.
    fn poll_send<T>(
        &self,
        context: &mut TaskContext<'_>,
        poll: impl FnOnce(Pin<&mut S>, &mut TaskContext<'_>) -> Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        let sent = self.poll_transport(context, poll);
        self.liveness.note_sending(sent.is_pending());
        sent
    }

    /// Polls the transport as `poll` does, or fails where it has been given
    /// up.
    fn poll_transport<T>(
        &self,
        context: &mut TaskContext<'_>,
        poll: impl FnOnce(Pin<&mut S>, &mut TaskContext<'_>) -> Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        self.liveness.waker.register(context.waker());
        let mut transport = self
            .liveness
            .transport
            .lock()
            .expect("joined stream transport lock is not poisoned");
        match transport.as_mut() {
            Some(transport) => poll(Pin::new(transport), context),
            None => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "the joined stream was given up",
            ))),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        ready!(self.poll_transport(context, |transport, context| {
            transport.poll_read(context, buffer)
        }))?;
        let this = &mut *self;
        let read = &buffer.filled()[before..];
        if !read.is_empty() {
            this.liveness
                .moved
                .lock()
                .expect("joined stream movement lock is not poisoned")
                .heard = tokio::time::Instant::now();
        }
        if !this.preface.done() && this.preface.take_in(read) {
            this.liveness.begun.send_replace(true);
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_send(context, |transport, context| {
            transport.poll_write(context, buffer)
        })
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_send(context, |transport, context| {
            transport.poll_write_vectored(context, buffers)
        })
    }

    fn is_write_vectored(&self) -> bool {
        self.liveness
            .transport
            .lock()
            .expect("joined stream transport lock is not poisoned")
            .as_ref()
            .is_some_and(AsyncWrite::is_write_vectored)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_send(context, |transport, context| transport.poll_flush(context))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_send(context, |transport, context| {
            transport.poll_shutdown(context)
        })
    }
}

/// What is still to come of the HTTP/2 connection preface the other Server
/// on a joined stream sends first: from the redeeming Server, the client's
/// magic and a SETTINGS frame; from the Serving Server, a SETTINGS frame.
/// What it says is HTTP/2's to judge; this only counts it in.
struct Preface {
    /// How much of the client's magic is still to come.
    magic: usize,
    /// The SETTINGS frame's header, as much of it as has come.
    header: [u8; 9],
    header_read: usize,
    /// How much of the SETTINGS frame's payload is still to come, once its
    /// header has.
    payload: usize,
}

impl Preface {
    /// The redeeming Server's preface.
    fn of_client() -> Self {
        Self {
            magic: b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".len(),
            ..Self::of_server()
        }
    }

    /// The Serving Server's preface.
    fn of_server() -> Self {
        Self {
            magic: 0,
            header: [0; 9],
            header_read: 0,
            payload: 0,
        }
    }

    fn done(&self) -> bool {
        self.magic == 0 && self.header_read == self.header.len() && self.payload == 0
    }

    /// Counts in `read`, the next bytes the other Server sent: whether the
    /// preface has now all come.
    fn take_in(&mut self, mut read: &[u8]) -> bool {
        let magic = self.magic.min(read.len());
        self.magic -= magic;
        read = &read[magic..];
        if self.header_read < self.header.len() {
            let header = (self.header.len() - self.header_read).min(read.len());
            self.header[self.header_read..self.header_read + header]
                .copy_from_slice(&read[..header]);
            self.header_read += header;
            read = &read[header..];
            if self.header_read == self.header.len() {
                let [high, middle, low, ..] = self.header;
                self.payload =
                    usize::from(high) << 16 | usize::from(middle) << 8 | usize::from(low);
            }
        }
        if self.header_read == self.header.len() {
            self.payload -= self.payload.min(read.len());
        }
        self.done()
    }
}

async fn forward_peer_api(
    State(state): State<ServingState>,
    ConnectInfo(connection): ConnectInfo<ServingConnectionInfo>,
    AxumPath(_path): AxumPath<String>,
    mut request: Request<Body>,
) -> Response {
    let Some((fingerprint, peer)) = connection
        .peer_key
        .as_deref()
        .and_then(|key| state.controller.enrolled_peer(key))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Err(refusal) = peer_speaks(request.headers(), state.protocol_version) {
        return refusal.response();
    }
    let Some(path_and_query) = request
        .uri()
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(remote_path_and_query) = path_and_query.strip_prefix(PEER_API_PREFIX) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(uri) = remote_path_and_query.parse() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;
    let Some(canonical_path) = canonical_forward_path(request.uri()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match peer_route_class(request.method(), &canonical_path) {
        PeerRouteClass::Api => {}
        PeerRouteClass::Administration => return StatusCode::FORBIDDEN.into_response(),
        PeerRouteClass::LoopbackOnly => return StatusCode::NOT_FOUND.into_response(),
    }
    // A Sidekick on the Peer performs the act: whatever the Peer said of the
    // Sidekick's own Session is nothing this Server can follow back to, so it
    // is believed of nothing but that a Sidekick sent it, and named by the
    // Peer that was authenticated sending it (ADR 0044).
    //
    // The Peer's claim is taken before anything it sent can strip it, then
    // what it sent is made safe to pass on — hop-by-hop headers, and any it
    // names in `Connection`, gone — and only then are the author and the
    // proof this listener vouches for added, after every header of the
    // Peer's, so nothing it sent can remove or shadow them.
    let takes_an_author = takes_an_author(request.method(), &canonical_path);
    let headers = request.headers_mut();
    let claimed = headers.remove(AUTHOR_HEADER);
    // The act the Peer names is believed of nothing but that the Peer chose
    // to call it so, which is all it is for: telling its acts apart.
    let act = headers
        .remove(ACT_HEADER)
        .and_then(|act| act.to_str().ok()?.parse::<uuid::Uuid>().ok())
        .map(ActId::from_uuid);
    headers.remove(FORWARDED_AUTHOR_PROOF_HEADER);
    remove_hop_by_hop_headers(headers);
    let mut vouched = local_forward_headers(&state.controller.local_api.token);
    if let Some(claimed) = claimed {
        // Only an act whose operation judges its author takes one: the
        // Sidekick Workspace's refusal stands where every such act passes,
        // and nothing else is a Sidekick's to do here.
        if !takes_an_author {
            return PairingFailure::new(
                SessionErrorCode::InvalidCommand,
                "A Sidekick on a Peer does not perform this act",
            )
            .response();
        }
        if read_author_header(&claimed).is_none() {
            return PairingFailure::new(
                SessionErrorCode::InvalidCommand,
                "Peer named an act's author in a shape this Server cannot read",
            )
            .response();
        }
        vouched.insert(
            AUTHOR_HEADER,
            author_header(&Author::PeerSidekick {
                peer,
                fingerprint,
                act,
            }),
        );
        vouched.insert(
            FORWARDED_AUTHOR_PROOF_HEADER,
            header::HeaderValue::from_str(&state.controller.forwarded_author_proof)
                .expect("a base64 proof is a valid header value"),
        );
    }
    match forward_to_local_api(&state.controller.local_api, request, vouched).await {
        Ok(response) => response,
        Err(error) => error.response(),
    }
}

fn canonical_forward_path(uri: &axum::http::Uri) -> Option<String> {
    reqwest::Url::parse(&format!("http://suru.invalid{uri}"))
        .ok()
        .map(|url| url.path().to_owned())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PeerRouteClass {
    Api,
    Administration,
    /// Served on the loopback listener alone, and to a Peer not at all: the
    /// Broker, whose tokens name this machine's own Provider Sessions.
    LoopbackOnly,
}

/// Whether the Session API's route `method` `path` is an act a Sidekick may
/// perform on the user's behalf, whose operation judges its author: beginning
/// a Session, preparing a Worktree for one, admitting a Prompt, interrupting,
/// settling or unsettling, answering a Questionnaire, or setting a
/// Workspace's Description.
fn takes_an_author(method: &Method, path: &str) -> bool {
    if *method != Method::POST {
        return false;
    }
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    matches!(
        segments.as_slice(),
        ["v1", "sessions"]
            | ["v1", "checkouts", "prepare"]
            | ["v1", "workspaces", "description"]
            | ["v1", "sessions", _, "prompts" | "interrupt" | "settlement"]
            | ["v1", "sessions", _, "questionnaires", _]
    )
}

fn peer_route_class(method: &Method, path: &str) -> PeerRouteClass {
    let settings_mutation = *method == Method::POST && path == "/v1/settings";
    let stop = *method == Method::POST && path == "/v1/server/stop";
    let pairing_management = path == "/v1/pairing" || path.starts_with("/v1/pairing/");
    let relay_management = path == "/v1/relays" || path.starts_with("/v1/relays/");
    // The Server's own event stream tells its Clients of its Settings and its
    // Relays, which are its administration.
    let own_events = path == "/v1/events";
    if crate::broker::is_broker_path(path) {
        PeerRouteClass::LoopbackOnly
    } else if settings_mutation || stop || pairing_management || relay_management || own_events {
        PeerRouteClass::Administration
    } else {
        PeerRouteClass::Api
    }
}

/// Carries a Peer's `request` on to this Server's own Session API, with
/// `added` set after all it carries.
async fn forward_to_local_api(
    api: &LocalApi,
    request: Request<Body>,
    added: HeaderMap,
) -> std::result::Result<Response, PairingFailure> {
    let (parts, body) = passed_on(request, added).into_parts();
    let target = format!(
        "{}{}",
        api.base_url,
        parts.uri.path_and_query().map_or("/", PathAndQuery::as_str)
    );
    let response = api
        .http
        .request(parts.method, target)
        .headers(parts.headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
        .map_err(|error| undelivered_or_lost(error.is_connect()))?;
    let status = response.status();
    let headers = response.headers().clone();
    Ok(passed_back(status, headers, response.bytes_stream(), None))
}

/// How what came of a request carried on to a Remote through `client` is
/// judged: an answer is passed back as it came; and where none came, the
/// request is asked by another way only where it never reached the Remote
/// or asking it twice changes nothing (`repeatable`) — one that may have
/// reached it is otherwise refused as such.
fn judge_proxied(
    answer: std::result::Result<Answer, Unanswered>,
    repeatable: bool,
    client: Arc<PairingHttpClient>,
) -> WayAttempt<Response> {
    match answer {
        Ok(response) => WayAttempt::Answered(remote_answer(response, client)),
        Err(unanswered) if unanswered.delivered && !repeatable => {
            WayAttempt::Rejected(undelivered_or_lost(false))
        }
        Err(_) => WayAttempt::TryNext,
    }
}

/// What the Remote answered a request carried on to it through `client`,
/// passed back: the answer holds `client` as its interest lease until its
/// body ends.
fn remote_answer(response: Answer, client: Arc<PairingHttpClient>) -> Response {
    let (parts, body) = response.into_parts();
    passed_back(
        parts.status,
        parts.headers,
        body.into_data_stream(),
        Some(client),
    )
}

/// `request` made safe to carry on: rid of every hop-by-hop header, and of
/// the host, credentials and Pairing protocol version it came with, and with
/// `added` set after all it carries.
fn passed_on(request: Request<Body>, added: HeaderMap) -> Request<Body> {
    let (mut parts, body) = request.into_parts();
    remove_hop_by_hop_headers(&mut parts.headers);
    parts.headers.remove(header::HOST);
    parts.headers.remove(header::AUTHORIZATION);
    parts.headers.remove(PAIRING_PROTOCOL_HEADER);
    parts.headers.extend(added);
    Request::from_parts(parts, body)
}

/// The failure of a carried request that got no answer. A request that never
/// connected was never delivered; once connected, it may have been, whatever
/// went wrong after.
fn undelivered_or_lost(never_connected: bool) -> PairingFailure {
    if never_connected {
        PairingFailure::new(
            SessionErrorCode::PairingConnectionFailed,
            "Remote API request could not be delivered",
        )
    } else {
        PairingFailure::new(
            SessionErrorCode::PairingOutcomeUnknown,
            "Remote API request was delivered, and its answer was lost",
        )
    }
}

/// The answer to a carried request, passed back as it came, hop-by-hop
/// headers aside, its body holding `interest` until it ends.
fn passed_back<S, E>(
    status: StatusCode,
    mut headers: HeaderMap,
    body: S,
    interest: Option<Arc<PairingHttpClient>>,
) -> Response
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
    E: Into<axum::BoxError>,
{
    remove_hop_by_hop_headers(&mut headers);
    let body = stream::unfold(
        (Box::pin(body), interest),
        |(mut body, interest)| async move { body.next().await.map(|chunk| (chunk, (body, interest))) },
    );
    let mut passed_back = Response::new(Body::from_stream(body));
    *passed_back.status_mut() = status;
    *passed_back.headers_mut() = headers;
    passed_back
}

/// What a request to this Server's own Session API was refused for: it named
/// an author the Serving listener did not set.
#[derive(Debug)]
pub(crate) struct ForgedAuthor;

/// `author` as the [`AUTHOR_HEADER`] carries it: its JSON, encoded so that
/// whatever its words are it stands in a header.
fn author_header(author: &Author) -> header::HeaderValue {
    let json = serde_json::to_vec(author).expect("an author always serializes");
    header::HeaderValue::from_str(&URL_SAFE_NO_PAD.encode(json))
        .expect("base64 is a valid header value")
}

/// The author an [`AUTHOR_HEADER`] carries, where it carries one.
fn read_author_header(value: &header::HeaderValue) -> Option<Author> {
    let json = URL_SAFE_NO_PAD.decode(value.as_bytes()).ok()?;
    serde_json::from_slice(&json).ok()
}

fn local_forward_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("runtime descriptor tokens are valid header values"),
    );
    headers
}

fn remote_forward_headers(protocol_version: u32) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        PAIRING_PROTOCOL_HEADER,
        header::HeaderValue::from_str(&protocol_version.to_string())
            .expect("protocol versions are valid header values"),
    );
    headers
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let named_by_connection = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect::<Vec<_>>();
    for name in named_by_connection {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

async fn serving_health(
    State(state): State<ServingState>,
    ConnectInfo(connection): ConnectInfo<ServingConnectionInfo>,
) -> Response {
    let authenticated = connection
        .peer_key
        .as_deref()
        .is_some_and(|key| state.controller.is_enrolled_peer(key));
    if !authenticated {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(PairingHealth {
        protocol_version: state.protocol_version,
    })
    .into_response()
}

async fn enroll_peer(
    State(state): State<ServingState>,
    ConnectInfo(connection): ConnectInfo<ServingConnectionInfo>,
    Json(request): Json<EnrollmentRequest>,
) -> Response {
    let Some(public_key) = connection.peer_key else {
        return PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "Invite redemption requires proof of the connector's key",
        )
        .response();
    };
    match state.controller.enroll(request, public_key) {
        Ok(()) => Json(EnrollmentResponse {
            hostname: state.hostname,
            protocol_version: state.protocol_version,
        })
        .into_response(),
        Err(error) => error.response(),
    }
}

/// A Peer saying its user has removed this Server as their Remote. It is
/// authenticated by the caller's own mutual-TLS key and ends only that Peer's
/// own record, so no Peer can withdraw another.
async fn withdraw_peer(
    State(state): State<ServingState>,
    ConnectInfo(connection): ConnectInfo<ServingConnectionInfo>,
) -> Response {
    let Some(public_key) = connection
        .peer_key
        .filter(|key| state.controller.is_enrolled_peer(key))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.controller.withdraw_peer(&public_key) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.response(),
    }
}

/// Refuses what a Peer asks, carrying `headers`, unless it states the
/// Pairing protocol version this Server speaks, `protocol_version`.
fn peer_speaks(
    headers: &HeaderMap,
    protocol_version: u32,
) -> std::result::Result<(), PairingFailure> {
    let Some(peer_protocol_version) = headers
        .get(PAIRING_PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Err(PairingFailure::new(
            SessionErrorCode::PairingProtocolMismatch,
            "Peer did not state a valid Pairing protocol version",
        ));
    };
    if peer_protocol_version != protocol_version {
        return Err(protocol_mismatch(protocol_version, peer_protocol_version));
    }
    Ok(())
}

/// A Peer asking which Relays this Server Serves through: told at once, and
/// again each time that changes, a line of the answer each time, for as long
/// as the Peer holds the answer open and stays enrolled. Only Relays are
/// told — which of its addresses to disclose stays the Serving user's choice
/// as they issue each Invite — and only over the pinned-key TLS, which is
/// what has a Peer believe what it is told.
async fn tell_offered_relays(
    State(state): State<ServingState>,
    ConnectInfo(connection): ConnectInfo<ServingConnectionInfo>,
    headers: HeaderMap,
) -> Response {
    let Some(peer_key) = connection
        .peer_key
        .filter(|key| state.controller.is_enrolled_peer(key))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Err(refusal) = peer_speaks(&headers, state.protocol_version) {
        return refusal.response();
    }
    let offered = state.controller.offered_relays.subscribe();
    let telling = stream::unfold(
        (offered, state.controller, peer_key, true),
        |(mut offered, controller, peer_key, first)| async move {
            if !first && offered.changed().await.is_err() {
                return None;
            }
            // A Peer removed or withdrawn since is told nothing more.
            if !controller.is_enrolled_peer(&peer_key) {
                return None;
            }
            let relays = offered.borrow_and_update().clone();
            let mut told =
                serde_json::to_vec(&OfferedRelays { relays }).expect("Relays told always encode");
            told.push(b'\n');
            let told = Ok::<_, std::convert::Infallible>(Bytes::from(told));
            Some((told, (offered, controller, peer_key, false)))
        },
    );
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(telling),
    )
        .into_response()
}

/// A connection come to the Serving side to be accepted, before its TLS
/// handshake.
struct Arrival {
    stream: Box<dyn ByteStream>,
    from: ArrivedFrom,
    /// What closes the connection beside its Peer's revocation and Serving
    /// stopping: for one dialled to the listener, that listener's stopping.
    closes_with: Option<Arc<ConnectionRevocation>>,
}

/// Where a connection the Serving side accepts came from, as what is logged
/// of it names it.
#[derive(Clone, Copy, Debug)]
enum ArrivedFrom {
    /// Dialled to the Serving listener from this network address.
    Direct(SocketAddr),
    /// Carried by a Relay, which the log does not name (ADR-0008).
    Relay,
}

impl std::fmt::Display for ArrivedFrom {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(address) => address.fmt(formatter),
            Self::Relay => formatter.write_str("a Relay"),
        }
    }
}

/// Where the Serving side takes the connections it accepts from: its
/// listener, and the Relays it Serves through.
type Arrivals = Pin<Box<dyn Stream<Item = Arrival> + Send>>;

/// The Serving listener as a source of connections: each one dialled to it.
fn dialled_to(listener: TcpListener) -> Arrivals {
    Box::pin(stream::unfold(listener, |listener| async move {
        loop {
            match listener.accept().await {
                Ok((stream, network_address)) => {
                    let arrival = Arrival {
                        stream: Box::new(stream),
                        from: ArrivedFrom::Direct(network_address),
                        closes_with: None,
                    };
                    return Some((arrival, listener));
                }
                Err(error) => {
                    tracing::warn!("Serving listener could not accept a connection: {error}");
                    tokio::task::yield_now().await;
                }
            }
        }
    }))
}

/// The connections Relays carried to this Server as a source of connections,
/// each as it is handed on.
fn carried_to(arrivals: mpsc::Receiver<Arrival>) -> Arrivals {
    Box::pin(stream::unfold(arrivals, |mut arrivals| async move {
        let arrival = arrivals.recv().await?;
        Some((arrival, arrivals))
    }))
}

/// The connections the listener took as a source of connections, each as
/// the acceptor comes to it, passing over one its listener closed meanwhile.
fn dialled_on(dialled: mpsc::Receiver<Dialled>) -> Arrivals {
    Box::pin(stream::unfold(dialled, |mut dialled| async move {
        loop {
            if let Some(arrival) = dialled.recv().await?.take() {
                return Some((arrival, dialled));
            }
        }
    }))
}

/// Hands each connection dialled to `listener` on to the acceptor through
/// `dialled`, noting it among those `waiting` for the acceptor and closing it
/// as `closes_with` is revoked, and waiting while the acceptor has as many
/// as it takes.
async fn listen(
    listener: TcpListener,
    dialled: mpsc::Sender<Dialled>,
    waiting: Arc<StdMutex<Vec<Dialled>>>,
    closes_with: Arc<ConnectionRevocation>,
) {
    let mut arrivals = dialled_to(listener);
    while let Some(arrival) = arrivals.next().await {
        let arrival = Dialled::new(Arrival {
            closes_with: Some(closes_with.clone()),
            ..arrival
        });
        {
            let mut waiting = waiting
                .lock()
                .expect("waiting connection lock is not poisoned");
            waiting.retain(Dialled::waiting);
            waiting.push(arrival.clone());
        }
        if dialled.send(arrival).await.is_err() {
            return;
        }
    }
}

/// The pinned-key TLS the Serving side runs over the connections it accepts.
#[derive(Clone)]
struct ServingTls {
    /// Over one dialled to its listener.
    listener: TlsAcceptor,
    /// Over one a Relay carried: see [`MultiplexedOnly`].
    carried: TlsAcceptor,
}

impl ServingTls {
    fn over(&self, from: ArrivedFrom) -> &TlsAcceptor {
        match from {
            ArrivedFrom::Direct(_) => &self.listener,
            ArrivedFrom::Relay => &self.carried,
        }
    }
}

/// The Serving side's identity, shown in the handshake of a connection a
/// Relay carried only where it asks to carry everything together over
/// HTTP/2, as a Relay way's connection always does: every joined stream is
/// then judged end to end by its keepalive ([`JoinedKeepalive`]), and one
/// that would speak anything else is refused there and then, before the
/// Serving side proves anything to it.
#[derive(Debug)]
struct MultiplexedOnly(Arc<CertifiedKey>);

impl ResolvesServerCert for MultiplexedOnly {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let multiplexed = client_hello
            .alpn()
            .is_some_and(|mut protocols| protocols.any(|protocol| protocol == MULTIPLEXED));
        multiplexed.then(|| self.0.clone())
    }
}

/// How one connection's TLS handshake with the Serving side ended: done,
/// refused, or past its handshake timeout.
type Handshake = std::result::Result<
    std::io::Result<TlsStream<Box<dyn ByteStream>>>,
    tokio::time::error::Elapsed,
>;

/// The Serving side's pinned-key acceptor, whatever source a connection
/// arrives from: each finishes its TLS handshake on its own, within its
/// handshake timeout, so one that never does — a dialer that says nothing,
/// or whose answers never reach it — holds up no other. The handshakes under
/// way end with the acceptor.
struct PairingAcceptor {
    arrivals: stream::Fuse<Arrivals>,
    tls: ServingTls,
    /// How long each connection may take to finish its handshake.
    handshake_timeout: tokio::time::Duration,
    /// Each under way, with where its connection came from and what closes
    /// it beside its Peer's revocation and Serving stopping.
    handshakes: tokio::task::JoinSet<(Handshake, ArrivedFrom, Option<Arc<ConnectionRevocation>>)>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    awaiting_revocation: AwaitingRevocation,
    connections: Arc<RevocableConnections>,
}

/// The connections whose key has no live revocation — removed, or yet to
/// enroll — by the key's fingerprint: each answers to the key's next
/// revocation from the moment it is made.
type AwaitingRevocation = Arc<StdMutex<HashMap<String, Vec<Weak<ConnectionRevocation>>>>>;

impl PairingAcceptor {
    /// Has `connection`, by the key whose fingerprint is `peer_id`, answer to
    /// that key's revocation: at once, where the key is enrolled or has
    /// withdrawn, or else from the key's next enrollment. A removed key's
    /// connection is let in under its tombstone, so its Server can be told it
    /// is revoked rather than find the connection dropped, and answers to
    /// whatever revocation follows rather than to that tombstone.
    fn answer_to_peer(&self, peer_id: String, connection: &Arc<ConnectionRevocation>) {
        let revocations = self
            .revocations
            .read()
            .expect("Peer revocation lock is not poisoned");
        if let Some(revocation) = revocations
            .get(&peer_id)
            .filter(|revocation| revocation.is_live())
        {
            revocation.revokes_with_it(connection);
            return;
        }
        // Waiting is noted while the revocations are held, so no enrollment
        // can make the next one between seeing there is none and noting it.
        let mut awaiting = self
            .awaiting_revocation
            .lock()
            .expect("awaiting revocation lock is not poisoned");
        awaiting.retain(|_, connections| {
            connections.retain(|connection| connection.strong_count() > 0);
            !connections.is_empty()
        });
        awaiting
            .entry(peer_id)
            .or_default()
            .push(Arc::downgrade(connection));
    }

    /// The next connection to finish its TLS handshake, taking more as they
    /// arrive meanwhile while there is room, where it came from, and what
    /// closes it beside its Peer's revocation and Serving stopping.
    async fn handshaken(
        &mut self,
    ) -> (
        TlsStream<Box<dyn ByteStream>>,
        ArrivedFrom,
        Option<Arc<ConnectionRevocation>>,
    ) {
        loop {
            let room = self.handshakes.len() < SERVING_HANDSHAKES_AT_ONCE;
            tokio::select! {
                Some(arrival) = self.arrivals.next(), if room => {
                    let acceptor = self.tls.over(arrival.from).clone();
                    let handshake_timeout = self.handshake_timeout;
                    self.handshakes.spawn(async move {
                        let handshake = tokio::time::timeout(
                            handshake_timeout,
                            acceptor.accept(arrival.stream),
                        );
                        // One dialled to a listener that stops meanwhile is let
                        // go there and then, as that listener's are.
                        let handshake = match &arrival.closes_with {
                            Some(closing) => tokio::select! {
                                handshake = handshake => handshake,
                                () = closing.revoked() => Ok(Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionAborted,
                                    "the Serving listener stopped",
                                ))),
                            },
                            None => handshake.await,
                        };
                        (handshake, arrival.from, arrival.closes_with)
                    });
                }
                Some(finished) = self.handshakes.join_next() => match finished {
                    Ok((Ok(Ok(stream)), from, closes_with)) => {
                        return (stream, from, closes_with);
                    }
                    Ok((_, from, _)) => {
                        tracing::debug!(peer = %from, "Serving TLS handshake refused");
                    }
                    Err(_) => {}
                },
                // Nothing more will arrive, and no handshake is under way.
                else => std::future::pending().await,
            }
        }
    }
}

impl PairingAcceptor {
    /// The next connection to pass the pinned-key TLS handshake, answering to
    /// its Peer's revocation — and, dialled to the listener, to that
    /// listener's stopping, at once where it has stopped since — and what it
    /// is known by.
    async fn accept(&mut self) -> (RevocableTlsStream, ServingConnectionInfo) {
        let (stream, from, closes_with) = self.handshaken().await;
        let connection_revocation = self.connections.register();
        if let Some(closes_with) = closes_with {
            closes_with.revokes_with_it(&connection_revocation);
        }
        let peer_key = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .and_then(|certificate| public_key_from_certificate(certificate).ok());
        if let Some(peer_id) = peer_key.as_deref().map(fingerprint) {
            self.answer_to_peer(peer_id, &connection_revocation);
        }
        (
            RevocableTlsStream {
                stream,
                from,
                connection_revocation,
            },
            ServingConnectionInfo { peer_key },
        )
    }
}

struct RevocableTlsStream {
    stream: TlsStream<Box<dyn ByteStream>>,
    /// Where the connection came from.
    from: ArrivedFrom,
    /// Revoked as every connection is when Serving stops, with its Peer's
    /// revocation, which it answers to from the moment there is one, and,
    /// dialled to the listener, as that listener stops.
    connection_revocation: Arc<ConnectionRevocation>,
}

impl RevocableTlsStream {
    /// Whether this connection carries everything asked over it together, as
    /// its TLS handshake agreed: `None` for one a Relay carried that would
    /// not, which is never served.
    fn multiplexed(&self) -> Option<bool> {
        let multiplexed = self.stream.get_ref().1.alpn_protocol() == Some(MULTIPLEXED);
        match self.from {
            ArrivedFrom::Direct(_) => Some(multiplexed),
            ArrivedFrom::Relay => multiplexed.then_some(true),
        }
    }

    /// Whether this connection is revoked. A Peer holds connections through
    /// every way it reaches this Server at once, so each answers to the
    /// Peer's revocation through its own, which wakes it alone.
    fn poll_revoked(&self, context: &mut TaskContext<'_>) -> bool {
        self.connection_revocation.poll(context)
    }

    fn revoked_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "Pairing connection was revoked",
        )
    }
}

/// The connections a listener has accepted, each of which can be cut at once:
/// the Serving listener's as it stops, and the local API's as a Server's stop
/// outlasts its deadline.
#[derive(Default)]
pub(crate) struct RevocableConnections {
    revocations: StdMutex<Vec<Weak<ConnectionRevocation>>>,
}

impl RevocableConnections {
    /// Registers a connection just accepted, answering what cuts it.
    pub(crate) fn register(&self) -> Arc<ConnectionRevocation> {
        let revocation = Arc::new(ConnectionRevocation::default());
        let mut revocations = self
            .revocations
            .lock()
            .expect("Serving connection lock is not poisoned");
        revocations.retain(|revocation| revocation.strong_count() > 0);
        revocations.push(Arc::downgrade(&revocation));
        revocation
    }

    /// Cuts every connection registered so far.
    pub(crate) fn revoke_all(&self) {
        let revocations = std::mem::take(
            &mut *self
                .revocations
                .lock()
                .expect("Serving connection lock is not poisoned"),
        );
        for revocation in revocations {
            if let Some(revocation) = revocation.upgrade() {
                revocation.revoke();
            }
        }
    }
}

/// Whether a connection has been cut, waking the task serving it as it is.
#[derive(Default)]
pub(crate) struct ConnectionRevocation {
    revoked: AtomicBool,
    waker: AtomicWaker,
    /// Wakes everything waiting on the revocation as it comes: each of a
    /// listener's handshakes still under way.
    waiting: tokio::sync::Notify,
    /// The revocations revoked with this one: a Peer's revokes each of its
    /// connections', and a listener's each connection dialled to it.
    dependents: StdMutex<Vec<Weak<ConnectionRevocation>>>,
}

impl ConnectionRevocation {
    /// Whether the connection has been cut, waking the task polling it once
    /// it is.
    pub(crate) fn poll(&self, context: &TaskContext<'_>) -> bool {
        self.waker.register(context.waker());
        self.revoked.load(Ordering::Acquire)
    }

    /// Whether this is not yet revoked: for a Peer, whether its key is
    /// enrolled or has withdrawn rather than been removed.
    fn is_live(&self) -> bool {
        !self.revoked.load(Ordering::Acquire)
    }

    /// Waits until this is revoked.
    async fn revoked(&self) {
        let revoked = self.waiting.notified();
        let mut revoked = std::pin::pin!(revoked);
        // Waiting from before the flag is read, so a revocation between the
        // two is not missed.
        revoked.as_mut().enable();
        if !self.is_live() {
            return;
        }
        revoked.await;
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.waker.wake();
        self.waiting.notify_waiters();
        let dependents = std::mem::take(
            &mut *self
                .dependents
                .lock()
                .expect("dependent revocation lock is not poisoned"),
        );
        for dependent in dependents {
            if let Some(dependent) = dependent.upgrade() {
                dependent.revoke();
            }
        }
    }

    /// Has `dependent` revoked with this, at once where this already is.
    fn revokes_with_it(&self, dependent: &Arc<ConnectionRevocation>) {
        let mut dependents = self
            .dependents
            .lock()
            .expect("dependent revocation lock is not poisoned");
        if self.revoked.load(Ordering::Acquire) {
            drop(dependents);
            dependent.revoke();
            return;
        }
        dependents.retain(|dependent| dependent.strong_count() > 0);
        dependents.push(Arc::downgrade(dependent));
    }
}

impl AsyncRead for RevocableTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.poll_revoked(context) {
            return Poll::Ready(Err(Self::revoked_error()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for RevocableTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.poll_revoked(context) {
            return Poll::Ready(Err(Self::revoked_error()));
        }
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.poll_revoked(context) {
            return Poll::Ready(Err(Self::revoked_error()));
        }
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.poll_revoked(context) {
            return Poll::Ready(Err(Self::revoked_error()));
        }
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

struct PinnedPeers {
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    invites: Arc<StdMutex<InviteLedger>>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
}

impl std::fmt::Debug for PinnedPeers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PinnedPeers")
    }
}

impl ClientCertVerifier for PinnedPeers {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, TlsError> {
        let key = public_key_from_certificate(end_entity)?;
        let known = self
            .peers
            .read()
            .expect("Peer record lock is not poisoned")
            .iter()
            .any(|peer| bool::from(peer.public_key.as_slice().ct_eq(&key)));
        let invited = enrollment_token_from_certificate(end_entity).is_some_and(|token| {
            self.invites
                .lock()
                .expect("Invite ledger lock is not poisoned")
                .issued
                .iter()
                .any(|issued| bool::from(issued.token.ct_eq(&token)))
        });
        // A just-revoked key remains pinned only as a tombstone, allowing the
        // authenticated Serving endpoint to answer 401 on the next health
        // check. That makes revocation distinguishable from a network drop;
        // ordinary unknown keys still fail during mutual TLS.
        let revoked = self
            .revocations
            .read()
            .expect("Peer revocation lock is not poisoned")
            .get(&fingerprint(&key))
            .is_some_and(|revocation| revocation.revoked.load(Ordering::Acquire));
        if known || invited || revoked {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<ServerHandshakeSignatureValid, TlsError> {
        client_signature_verifier(cert)?.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<ServerHandshakeSignatureValid, TlsError> {
        client_signature_verifier(cert)?.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        crypto_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct PinnedServerKey {
    expected: Vec<u8>,
    rejected: Arc<AtomicU64>,
}

impl std::fmt::Debug for PinnedServerKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PinnedServerKey")
    }
}

impl ServerCertVerifier for PinnedServerKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let actual = public_key_from_certificate(end_entity)?;
        if bool::from(actual.as_slice().ct_eq(&self.expected)) {
            Ok(ServerCertVerified::assertion())
        } else {
            self.rejected.fetch_add(1, Ordering::AcqRel);
            Err(TlsError::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<ServerHandshakeSignatureValid, TlsError> {
        server_signature_verifier(cert)?.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<ServerHandshakeSignatureValid, TlsError> {
        server_signature_verifier(cert)?.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        crypto_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_signature_verifier(
    certificate: &CertificateDer<'_>,
) -> std::result::Result<Arc<dyn ClientCertVerifier>, TlsError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(certificate.clone().into_owned())
        .map_err(|error| TlsError::General(error.to_string()))?;
    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), crypto_provider())
        .allow_unauthenticated()
        .build()
        .map_err(|error| TlsError::General(error.to_string()))
}

fn server_signature_verifier(
    certificate: &CertificateDer<'_>,
) -> std::result::Result<Arc<WebPkiServerVerifier>, TlsError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(certificate.clone().into_owned())
        .map_err(|error| TlsError::General(error.to_string()))?;
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), crypto_provider())
        .build()
        .map_err(|error| TlsError::General(error.to_string()))
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Obtains a connection to the Serving Server `way` reaches, as `dialer`
/// dials it: the byte stream the Pairing's pinned-key TLS, and everything
/// asked over it, run over. A direct way is dialled at its address, through
/// a tunnel the proxy named for it opens, where one is. Only a plain `http`
/// proxy is tunnelled through: a way named a proxy of any other kind fails
/// before anything is dialled. A Relay way is joined to the Serving Server
/// at its Relay, while the connection is `wanted`.
async fn open_connection(
    way: &Way,
    dialer: &WayDialer,
    wanted: Wanted,
) -> std::io::Result<Box<dyn ByteStream>> {
    match way {
        Way::Direct(address) => {
            let target = format!("https://{address}")
                .parse::<Uri>()
                .map_err(std::io::Error::other)?;
            let socket = match dialer.proxies.for_address(*address) {
                Some(proxy) => {
                    // TLS to a proxy is never built, so an `https` proxy would
                    // be sent its credentials in the clear, and a SOCKS proxy
                    // speaks no CONNECT. Neither is dialled, and the refusal
                    // names the scheme alone, never the proxy's URL.
                    let scheme = proxy.uri().scheme_str().unwrap_or_default();
                    if scheme != "http" {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            format!(
                                "a direct way is tunnelled through an `http` proxy alone, and \
                                 the one named for it is `{scheme}`"
                            ),
                        ));
                    }
                    let mut tunnel = Tunnel::new(proxy.uri().clone(), direct_dialer());
                    if let Some(credentials) = proxy.basic_auth() {
                        tunnel = tunnel.with_auth(credentials.clone());
                    }
                    connected(tunnel, target).await?
                }
                None => connected(direct_dialer(), target).await?,
            };
            Ok(Box::new(socket.into_inner()))
        }
        Way::Relay(relay) => match dialer.relays.get() {
            Some(relays) => {
                relays
                    .join(relay.clone(), dialer.server.to_vec(), wanted)
                    .await
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this Server reaches no Relay",
            )),
        },
    }
}

/// What `connector` connects to `target`, once it is ready to.
async fn connected<C>(mut connector: C, target: Uri) -> std::io::Result<C::Response>
where
    C: tower_service::Service<Uri>,
    C::Error: Into<axum::BoxError>,
{
    std::future::poll_fn(|context| connector.poll_ready(context))
        .await
        .map_err(std::io::Error::other)?;
    connector.call(target).await.map_err(std::io::Error::other)
}

/// What dials a direct way's socket, or the socket to the proxy tunnelling
/// to it: an HTTP client's dialer, which sets the socket up alike on every
/// platform — sending without delay, probed while idle, and given up on where
/// what it sends goes unacknowledged.
fn direct_dialer() -> HttpConnector {
    let mut dialer = HttpConnector::new();
    dialer.enforce_http(false);
    dialer.set_nodelay(true);
    dialer.set_keepalive(Some(SOCKET_KEEPALIVE));
    dialer.set_keepalive_interval(Some(SOCKET_KEEPALIVE));
    dialer.set_keepalive_retries(Some(SOCKET_KEEPALIVE_PROBES));
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    dialer.set_tcp_user_timeout(Some(SOCKET_USER_TIMEOUT));
    dialer
}

/// Asks the Serving Server whose identity key is `server_key` for
/// `enrollment` by `ways`, dialled as `dialer` dials them and as every new
/// dial is ([`PairingHttpClient::carrier`]), until one answers. Where none
/// does, a way that presented another key is said first, and then the first
/// Relay way refused for a reason the user can act on.
async fn dial_enrollment(
    ways: &[Way],
    server_key: &[u8],
    identity: &IdentityMaterial,
    dialer: WayDialer,
    enrollment: &EnrollmentRequest,
) -> std::result::Result<EnrollmentResponse, PairingFailure> {
    let client = paired_http_client(server_key, identity, Some(&enrollment.token), dialer)
        .map_err(internal_pairing_failure)?;
    let enrollment = serde_json::to_vec(enrollment).expect("an enrollment request always encodes");
    let asking = || {
        Request::post("/v1/pairing/enroll")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(enrollment.clone()))
            .expect("an enrollment request is well formed")
    };
    let enrolled = first_answer(&client, ways, asking, |answer| async move {
        match answer {
            Ok(response) if response.status().is_success() => match small_answer(response).await {
                Some(enrolled) => WayAttempt::Answered(enrolled),
                None => WayAttempt::Rejected(PairingFailure::new(
                    SessionErrorCode::PairingConnectionFailed,
                    "Serving Server returned an invalid enrollment response",
                )),
            },
            Ok(response) => WayAttempt::Rejected(decode_pairing_response(response).await),
            Err(_) => WayAttempt::TryNext,
        }
    })
    .await;
    match enrolled {
        Ok((_, enrolled)) => Ok(enrolled),
        Err(NoAnswer::Refused(failure)) => Err(failure),
        Err(NoAnswer::Unreached {
            other_key: true, ..
        }) => Err(PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "offered address presented a key other than the Invite's pinned key",
        )),
        Err(NoAnswer::Unreached {
            refused: Some(refusal),
            ..
        }) => Err(PairingFailure::refused(refusal)),
        Err(NoAnswer::Unreached { .. }) => Err(PairingFailure::new(
            SessionErrorCode::PairingConnectionFailed,
            "could not reach an offered address with the Invite's pinned key",
        )),
    }
}

/// The pinned-key client a Serving Server is asked through. Over each way it
/// is asked by, it obtains connections from that way and runs the pinned-key
/// TLS over each before asking anything: over a direct way, a connection for
/// each request under way, as HTTP/1.1; over a Relay way, one joined stream
/// for them all. Which way carries each request is chosen as
/// [`PairingHttpClient::carrier`] says.
struct PairingHttpClient {
    /// The pinned-key TLS run over a direct way's connections.
    tls: TlsConnector,
    /// The same, run over a Relay way's, asking the Serving Server to carry
    /// everything asked by that way together.
    joined_tls: TlsConnector,
    dialer: WayDialer,
    /// How each way asked by so far is asked.
    over_ways: StdMutex<HashMap<Way, OverWay>>,
    server_key_rejections: Arc<AtomicU64>,
    /// Lets go, as the client goes, of every connection still being made for
    /// it, so none is made, and no join asked at a Relay, once nothing is
    /// asked of the Serving Server. What makes a connection listens for it
    /// without holding the client.
    interest: watch::Sender<()>,
    /// How the direct ways are being tried again in the background.
    direct_retry: Arc<StdMutex<DirectRetry>>,
    /// Whether what the Serving Server tells of the Relays it Serves through
    /// is followed through this client.
    keeping_up: AtomicBool,
    /// How many times the Serving Server has answered, as one paired with
    /// this Server does, what was asked through this client: so following
    /// what it tells, once that ends, can say whether it has answered since.
    answers: AtomicU64,
    /// Which of the Pairings made since this Server started the client was
    /// made for, as [`StoredRemote::generation`] says.
    generation: u64,
}

impl PairingHttpClient {
    /// Whether this client was made for the Pairing `remote` is a record of:
    /// it pins that Pairing's key, and was made for that very Pairing.
    fn pins(&self, remote: &StoredRemote) -> bool {
        *self.dialer.server == *remote.public_key && self.generation == remote.generation
    }
}

/// How a Serving Server's direct ways are being tried again in the
/// background, while what is asked of it rides a joined stream.
#[derive(Default)]
struct DirectRetry {
    /// When they were last tried.
    tried: Option<tokio::time::Instant>,
    /// Whether they are being tried just now.
    trying: bool,
}

/// How a Serving Server is asked by one of its ways.
#[derive(Clone)]
enum OverWay {
    /// Over a direct way, on a connection of its own for each request, the
    /// last to come free kept for the next.
    Direct(Arc<DirectConnections>),
    /// Over a Relay way, on one joined stream.
    Joined(Arc<JoinedStream>),
}

impl PairingHttpClient {
    /// What carries one request to the Serving Server by one of `ways`, which
    /// are in the order each kind of way is dialled in: a connection a direct
    /// way kept from an earlier request, where one stands; or else a joined
    /// stream standing, at once, the direct ways tried again meanwhile in
    /// the background so the next request finds one kept where a direct way
    /// answers again; and otherwise a new dial.
    ///
    /// A new dial tries the direct ways first, one after another, and starts
    /// the Relay ways beside them, one after another, once the head start
    /// has passed with no direct way answering — or as soon as every direct
    /// way has failed. A way answers once the pinned-key TLS over a
    /// connection it gave is done, the Serving Server having proven its
    /// pinned key, and, for a Relay way, HTTP/2 is under way over it; a
    /// joined stream already standing answers at once. A way presenting
    /// another key never answers. Whichever answers first carries the
    /// request, and the others are let go: a direct dial under way is
    /// dropped, and a join being made, or made, for nothing else is given up
    /// unasked or let go. What a connection already carries stays on it.
    ///
    /// Ways `asked` has seen fail are not dialled again, and each that fails
    /// now is noted there. `None` once every way has failed.
    async fn carrier(&self, ways: &[Way], asked: &mut Asked) -> Option<(Way, Carrier)> {
        let untried = |kind: fn(&Way) -> bool| {
            ways.iter()
                .filter(|way| kind(way) && !asked.failed.contains(way))
                .cloned()
                .collect::<Vec<_>>()
        };
        let direct = untried(|way| matches!(way, Way::Direct(_)));
        let relayed = untried(|way| matches!(way, Way::Relay(_)));
        for way in &direct {
            let OverWay::Direct(connections) = self.over(way) else {
                continue;
            };
            if let Some(connection) = connections.kept() {
                let kept = Carrier::Direct {
                    connection,
                    kept: true,
                    connections,
                };
                return Some((way.clone(), kept));
            }
        }
        for way in &relayed {
            let OverWay::Joined(joined) = self.over(way) else {
                continue;
            };
            if let Some(connection) = joined.standing() {
                self.retry_direct(&direct);
                return Some((way.clone(), Carrier::Joined(connection)));
            }
        }
        let (mut direct_failed, mut relayed_failed) = (Vec::new(), Vec::new());
        let dialling = async {
            for way in &direct {
                let OverWay::Direct(connections) = self.over(way) else {
                    continue;
                };
                match connections.dial().await {
                    Ok(connection) => {
                        let dialled = Carrier::Direct {
                            connection,
                            kept: false,
                            connections,
                        };
                        return Some((way.clone(), dialled));
                    }
                    Err(_) => direct_failed.push(way.clone()),
                }
            }
            None
        };
        let joining = async {
            for way in &relayed {
                let OverWay::Joined(joined) = self.over(way) else {
                    continue;
                };
                match joined.carrier().await {
                    Ok(carrier) => return Some((way.clone(), carrier)),
                    Err(error) => {
                        relayed_failed.push((way.clone(), relay_refusal(error.as_ref()).cloned()));
                    }
                }
            }
            None
        };
        let carrier = race(dialling, joining, self.dialer.direct_head_start).await;
        asked.failed.extend(direct_failed);
        for (way, refused) in relayed_failed {
            asked.failed.push(way);
            asked.refused_also(refused);
        }
        carrier
    }

    /// Tries the direct ways `direct` again in the background, one after
    /// another, where none was tried within the retry interval and none is
    /// being tried just now: the first whose pinned-key TLS is done is kept
    /// for the next request, which goes directly. A way presenting another
    /// key is never kept, and nothing already carried is moved. The tries
    /// hold no interest in the Serving Server of their own, so they end with
    /// the last of it.
    fn retry_direct(&self, direct: &[Way]) {
        {
            let mut retry = self
                .direct_retry
                .lock()
                .expect("direct retry lock is not poisoned");
            let now = tokio::time::Instant::now();
            let lately = retry
                .tried
                .is_some_and(|tried| now < tried + self.dialer.direct_retry_interval);
            if direct.is_empty() || retry.trying || lately {
                return;
            }
            *retry = DirectRetry {
                tried: Some(now),
                trying: true,
            };
        }
        let trying = direct
            .iter()
            .filter_map(|way| match self.over(way) {
                OverWay::Direct(connections) => Some(connections),
                OverWay::Joined(_) => None,
            })
            .collect::<Vec<_>>();
        let (retry, handshake_timeout) = (self.direct_retry.clone(), self.dialer.handshake_timeout);
        tokio::spawn(async move {
            for connections in trying {
                let dialled = tokio::time::timeout(handshake_timeout, async {
                    let mut connection = connections.dial().await?;
                    connection.ready().await.map_err(std::io::Error::other)?;
                    std::io::Result::Ok(connection)
                });
                if let Ok(Ok(connection)) = dialled.await {
                    connections.keep(connection);
                    break;
                }
            }
            retry
                .lock()
                .expect("direct retry lock is not poisoned")
                .trying = false;
        });
    }

    /// How the Serving Server is asked by `way`.
    fn over(&self, way: &Way) -> OverWay {
        self.over_ways
            .lock()
            .expect("Pairing client lock is not poisoned")
            .entry(way.clone())
            .or_insert_with(|| match way {
                Way::Direct(_) => OverWay::Direct(Arc::new(DirectConnections {
                    connector: self.connector(way, &self.tls),
                    kept: StdMutex::default(),
                })),
                Way::Relay(_) => OverWay::Joined(Arc::new(JoinedStream {
                    connector: self.connector(way, &self.joined_tls),
                    current: StdMutex::default(),
                    joins: AtomicU64::default(),
                })),
            })
            .clone()
    }

    /// What connects to the Serving Server by `way`, running `tls` over each
    /// connection, for as long as anything is asked of it.
    fn connector(&self, way: &Way, tls: &TlsConnector) -> WayConnector {
        WayConnector {
            way: way.clone(),
            tls: tls.clone(),
            dialer: self.dialer.clone(),
            interest: self.interest.subscribe(),
        }
    }
}

/// Whichever of `direct` and `relayed` first comes to something, the other
/// let go: `relayed` is started only once `head_start` has passed, or as soon
/// as `direct` has come to nothing; nothing where both do.
async fn race<T>(
    direct: impl Future<Output = Option<T>>,
    relayed: impl Future<Output = Option<T>>,
    head_start: tokio::time::Duration,
) -> Option<T> {
    let (mut direct, mut relayed) = (std::pin::pin!(direct), std::pin::pin!(relayed));
    let mut head_start = std::pin::pin!(tokio::time::sleep(head_start));
    let (mut direct_done, mut relayed_begun, mut relayed_done) = (false, false, false);
    loop {
        tokio::select! {
            biased;
            carried = &mut direct, if !direct_done => match carried {
                Some(carried) => return Some(carried),
                None => (direct_done, relayed_begun) = (true, true),
            },
            () = &mut head_start, if !relayed_begun => relayed_begun = true,
            carried = &mut relayed, if relayed_begun && !relayed_done => match carried {
                Some(carried) => return Some(carried),
                None => relayed_done = true,
            },
            else => return None,
        }
    }
}

/// What a Serving Server answers over a Pairing connection.
type Answer = hyper::Response<AnswerBody>;

/// The body of what a Serving Server answers, holding the place what was
/// asked took among the streams a joined stream carries at once until the
/// body is read to its end or let go.
struct AnswerBody {
    body: Incoming,
    _place: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl HttpBody for AnswerBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        Pin::new(&mut self.body).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

/// What carries one request to a Serving Server: a connection one of its
/// ways answered by.
enum Carrier {
    /// A direct way's, for this request alone, kept for the next once its
    /// answer has been read; `kept` where it was kept from an earlier one.
    Direct {
        connection: http1::SendRequest<Body>,
        kept: bool,
        connections: Arc<DirectConnections>,
    },
    /// A Relay way's joined stream, carrying this request beside whatever
    /// else it carries.
    Joined(JoinedCarrier),
}

impl Carrier {
    /// Asks `request`, whose target is a path on the Serving Server.
    async fn send(self, mut request: Request<Body>) -> std::result::Result<Answer, Unanswered> {
        let path_and_query = request
            .uri()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| PathAndQuery::from_static("/"));
        let telling = path_and_query.path() == OFFERED_RELAYS_PATH;
        match self {
            Self::Direct {
                mut connection,
                kept,
                connections,
            } => {
                // HTTP/1.1 names the path alone, and the Server asked in the
                // Host header.
                *request.uri_mut() = Uri::from(path_and_query);
                request.headers_mut().insert(
                    header::HOST,
                    header::HeaderValue::from_static(SERVING_IDENTITY_NAME),
                );
                match connection.try_send_request(request).await {
                    Ok(response) => {
                        connections.keep_once_free(connection);
                        Ok(response.map(|body| AnswerBody { body, _place: None }))
                    }
                    Err(mut error) => {
                        let delivered = error.take_message().is_none();
                        Err(Unanswered {
                            delivered,
                            stale: kept && !delivered,
                        })
                    }
                }
            }
            Self::Joined(JoinedCarrier { mut sender, places }) => {
                // The stream the Relays are told over takes the place kept
                // for it; anything else waits its turn for one of the rest,
                // and is never asked once the connection has ended.
                let place = if telling {
                    None
                } else {
                    let Ok(place) = places.acquire_owned().await else {
                        return Err(Unanswered {
                            delivered: false,
                            stale: false,
                        });
                    };
                    Some(place)
                };
                *request.uri_mut() = Uri::builder()
                    .scheme("https")
                    .authority(SERVING_IDENTITY_NAME)
                    .path_and_query(path_and_query)
                    .build()
                    .expect("a path on the Serving Server is a target");
                sender
                    .try_send_request(request)
                    .await
                    .map(|response| {
                        response.map(|body| AnswerBody {
                            body,
                            _place: place,
                        })
                    })
                    .map_err(|mut error| Unanswered {
                        delivered: error.take_message().is_none(),
                        stale: false,
                    })
            }
        }
    }
}

/// The room there is among what awaits one Remote's answer: for the requests
/// asked of it, and for their bodies.
struct Awaiting {
    requests: Arc<tokio::sync::Semaphore>,
    bytes: Arc<tokio::sync::Semaphore>,
}

impl Awaiting {
    /// Room for `requests` at once, and [`AWAITED_BODY_BYTES`] of their
    /// bodies.
    fn new(requests: usize) -> Self {
        Self {
            requests: Arc::new(tokio::sync::Semaphore::new(requests)),
            bytes: Arc::new(tokio::sync::Semaphore::new(AWAITED_BODY_BYTES)),
        }
    }
}

/// A request's room among what awaits its Remote's answer, and its body's,
/// given back as this goes.
struct AwaitingAnswer {
    _request: tokio::sync::OwnedSemaphorePermit,
    _body: tokio::sync::OwnedSemaphorePermit,
    /// What it is room among, held so another asking meanwhile shares it.
    _awaiting: Arc<Awaiting>,
}

/// Why something asked of a Serving Server over a connection got no answer.
struct Unanswered {
    /// Whether it may have reached the Serving Server.
    delivered: bool,
    /// Whether it went over a connection kept from an earlier request that
    /// had gone meanwhile, never reaching the Serving Server: asked over
    /// another, it may well be answered.
    stale: bool,
}

/// The connections to a Serving Server over one direct way: one for each
/// request under way, the last to come free kept for the next, and closed
/// once it has stood idle for as long as one may, as HTTP clients close
/// theirs.
struct DirectConnections {
    connector: WayConnector,
    kept: StdMutex<Kept>,
}

/// The connection a direct way keeps for the next request.
#[derive(Default)]
struct Kept {
    /// The connection kept, and since when.
    connection: Option<(http1::SendRequest<Body>, tokio::time::Instant)>,
    /// Whether something waits to close the connection kept once it has
    /// stood idle too long.
    expiring: bool,
}

impl DirectConnections {
    /// The connection kept for the next request, where one stands.
    fn kept(&self) -> Option<http1::SendRequest<Body>> {
        let (connection, kept_since) = self.lock_kept().connection.take()?;
        (kept_since.elapsed() < self.idle_timeout() && connection.is_ready()).then_some(connection)
    }

    /// Keeps `connection` for the next request, where none is kept already.
    fn keep(self: &Arc<Self>, connection: http1::SendRequest<Body>) {
        let mut kept = self.lock_kept();
        if kept.connection.is_some() {
            return;
        }
        let kept_since = tokio::time::Instant::now();
        kept.connection = Some((connection, kept_since));
        if !std::mem::replace(&mut kept.expiring, true) {
            tokio::spawn(Self::expire(
                Arc::downgrade(self),
                kept_since + self.idle_timeout(),
            ));
        }
    }

    /// Closes the connection kept once it has stood idle for as long as one
    /// may, from `at` on: one kept since in its place is given its own time.
    async fn expire(connections: Weak<Self>, mut at: tokio::time::Instant) {
        loop {
            tokio::time::sleep_until(at).await;
            let Some(connections) = connections.upgrade() else {
                return;
            };
            let mut kept = connections.lock_kept();
            match kept.connection.as_ref() {
                Some((_, kept_since))
                    if *kept_since + connections.idle_timeout() > tokio::time::Instant::now() =>
                {
                    at = *kept_since + connections.idle_timeout();
                }
                _ => {
                    *kept = Kept::default();
                    return;
                }
            }
        }
    }

    /// How long a kept connection may stand idle.
    fn idle_timeout(&self) -> tokio::time::Duration {
        self.connector.dialer.direct_idle_timeout
    }

    fn lock_kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept
            .lock()
            .expect("kept connection lock is not poisoned")
    }

    /// A connection dialled afresh.
    async fn dial(&self) -> std::io::Result<http1::SendRequest<Body>> {
        let paired = self.connector.connect().await?;
        let (connection, carrying) = http1::handshake(TokioIo::new(paired))
            .await
            .map_err(std::io::Error::other)?;
        // It ends once nothing can ask over it any longer and what it
        // carries has ended, or as it fails. Nothing asked of a Serving
        // Server asks to upgrade the connection — what is carried on loses
        // its `Upgrade` with every other hop-by-hop header — so none is
        // driven for.
        tokio::spawn(carrying);
        Ok(connection)
    }

    /// Keeps `connection` for the next request once the answer it carries
    /// has been read, where none is kept already; one whose answer is never
    /// read to its end is let go.
    fn keep_once_free(self: &Arc<Self>, mut connection: http1::SendRequest<Body>) {
        let connections = Arc::downgrade(self);
        tokio::spawn(async move {
            if connection.ready().await.is_err() {
                return;
            }
            if let Some(connections) = connections.upgrade() {
                connections.keep(connection);
            }
        });
    }
}

/// The one HTTP/2 connection to a Serving Server over a Relay way, which
/// everything asked by that way travels over together, so a Remote in view
/// costs its Relay one join however many requests and streams are open to
/// it. Whatever is asked while the join is being made waits on that join,
/// sharing whatever becomes of it, and a join is asked again only once the
/// connection it carried has ended. A join is made only for as long as
/// something waits on it, and one made that has carried nothing is let go
/// once nothing does — as when a direct way answered first.
struct JoinedStream {
    connector: WayConnector,
    /// The connection made, or being made, for whatever is asked next.
    current: StdMutex<Option<CurrentJoin>>,
    /// How many joins have been asked, so each is told apart from the next.
    joins: AtomicU64,
}

/// A joined stream's connection as it comes to be made or not, shared by
/// everything waiting on it.
type JoinedConnection =
    Shared<BoxFuture<'static, std::result::Result<JoinedCarrier, Arc<std::io::Error>>>>;

/// A joined stream's connection, once it stands: what asks over it, and the
/// places among the streams it carries at once that what is asked takes.
#[derive(Clone)]
struct JoinedCarrier {
    sender: http2::SendRequest<Body>,
    /// One for each of [`JOINED_STREAMS_AT_ONCE`], each held by what is
    /// asked over the connection until its answer is read or let go, and
    /// closed as the connection ends. The stream the Relays are told over
    /// takes none, so the one place more the Serving Server allows is always
    /// free for it.
    places: Arc<tokio::sync::Semaphore>,
}

/// The join a joined stream is made over, made or being made.
struct CurrentJoin {
    /// Which of the joined stream's joins it is.
    number: u64,
    connection: JoinedConnection,
    /// Held by each thing waiting on the join as it is made, which is given
    /// up, asking nothing more of the Relay, once nothing holds it.
    waiting: Weak<watch::Sender<()>>,
    /// Whether anything has been asked over it.
    carried: bool,
}

impl JoinedStream {
    /// The connection standing, where one is, to carry a request at once.
    fn standing(&self) -> Option<JoinedCarrier> {
        let mut current = self
            .current
            .lock()
            .expect("joined stream lock is not poisoned");
        let join = current.as_mut()?;
        let connection = match join.connection.peek() {
            Some(Ok(connection)) if !connection.sender.is_closed() => connection.clone(),
            _ => return None,
        };
        join.carried = true;
        Some(connection)
    }

    /// What carries a request over this joined stream, once it stands: the
    /// connection standing, or the one being made, or one made afresh.
    async fn carrier(self: &Arc<Self>) -> std::result::Result<Carrier, Arc<std::io::Error>> {
        let (number, connection, waiting) = self.wait_on();
        let _waiting = WaitingOnJoin {
            joined: self.clone(),
            number,
            waiting,
        };
        let connection = connection.await?;
        self.carry(number);
        Ok(Carrier::Joined(connection))
    }

    /// The join the next request goes over, its number, and what to hold
    /// while it is made: the join standing, or the one being made, and
    /// otherwise one asked afresh.
    fn wait_on(&self) -> (u64, JoinedConnection, Option<Arc<watch::Sender<()>>>) {
        let mut current = self
            .current
            .lock()
            .expect("joined stream lock is not poisoned");
        if let Some(join) = current.as_ref() {
            match join.connection.peek() {
                Some(Ok(connection)) if !connection.sender.is_closed() => {
                    return (join.number, join.connection.clone(), None);
                }
                None => {
                    if let Some(waiting) = join.waiting.upgrade() {
                        return (join.number, join.connection.clone(), Some(waiting));
                    }
                }
                Some(_) => {}
            }
        }
        // Made on its own, so it goes on being made for whatever waits on it
        // though whatever first asked has gone; given up, and asking nothing
        // more of the Relay, once nothing does.
        let waiting = Arc::new(watch::Sender::new(()));
        let connector = WayConnector {
            interest: waiting.subscribe(),
            ..self.connector.clone()
        };
        let making = tokio::spawn(join_stream(connector));
        let connection = async move {
            making
                .await
                .unwrap_or_else(|error| Err(Arc::new(std::io::Error::other(error))))
        }
        .boxed()
        .shared();
        let number = self.joins.fetch_add(1, Ordering::AcqRel);
        *current = Some(CurrentJoin {
            number,
            connection: connection.clone(),
            waiting: Arc::downgrade(&waiting),
            carried: false,
        });
        (number, connection, Some(waiting))
    }

    /// Notes that the join numbered `number` carries something, so it stands
    /// for whatever is asked next.
    fn carry(&self, number: u64) {
        if let Some(join) = self
            .current
            .lock()
            .expect("joined stream lock is not poisoned")
            .as_mut()
            .filter(|join| join.number == number)
        {
            join.carried = true;
        }
    }

    /// Lets the join numbered `number` go where it has carried nothing and
    /// nothing waits on it any longer: given up as it is made, or dropped
    /// once made.
    fn release(&self, number: u64) {
        let mut current = self
            .current
            .lock()
            .expect("joined stream lock is not poisoned");
        if current.as_ref().is_some_and(|join| {
            join.number == number && !join.carried && join.waiting.strong_count() == 0
        }) {
            *current = None;
        }
    }
}

/// Something waiting on a joined stream's join, which lets the join go as it
/// stops waiting, where nothing else holds it and it carries nothing.
struct WaitingOnJoin {
    joined: Arc<JoinedStream>,
    number: u64,
    waiting: Option<Arc<watch::Sender<()>>>,
}

impl Drop for WaitingOnJoin {
    fn drop(&mut self) {
        drop(self.waiting.take());
        self.joined.release(self.number);
    }
}

/// Makes the connection of a joined stream as `connector` connects — joined
/// at its Relay, with the pinned-key TLS run over the join — and has it carry
/// HTTP/2, as the Serving Server agreed in that handshake.
async fn join_stream(
    connector: WayConnector,
) -> std::result::Result<JoinedCarrier, Arc<std::io::Error>> {
    let mut interest = connector.interest.clone();
    let joining = async {
        let paired = connector.connect().await?;
        if paired.0.get_ref().1.alpn_protocol() != Some(MULTIPLEXED) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the Serving Server would not carry what is asked of it together",
            ));
        }
        let (keepalive, startup) = (
            connector.dialer.keepalive,
            connector.dialer.handshake_timeout,
        );
        let (transport, liveness) = Liveness::watch(paired, Preface::of_server());
        // Given up, whatever it holds, unless it comes to stand.
        let unless_standing = GivenUpUnlessKept(Some(liveness.clone()));
        let mut client = http2::Builder::new(TokioExecutor::new());
        client
            .timer(TokioTimer::new())
            .keep_alive_interval(keepalive.interval)
            .keep_alive_timeout(keepalive.timeout)
            .keep_alive_while_idle(true)
            .initial_stream_window_size(JOINED_STREAM_WINDOW)
            .initial_connection_window_size(JOINED_CONNECTION_WINDOW)
            .max_send_buf_size(JOINED_STREAM_WINDOW as usize);
        let (sender, connection) =
            tokio::time::timeout(startup, client.handshake(TokioIo::new(transport)))
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "HTTP/2 over the joined stream did not begin in time",
                    )
                })?
                .map_err(std::io::Error::other)?;
        // The connection ends once nothing can ask over it any longer and
        // what it carries has ended, or as it fails, and its places close
        // with it, so nothing waits on one any longer.
        let places = Arc::new(tokio::sync::Semaphore::new(JOINED_STREAMS_AT_ONCE as usize));
        let closing = places.clone();
        tokio::spawn(async move {
            let _ = connection.await;
            closing.close();
        });
        // It stands once the Serving Server has begun HTTP/2 over it too, and
        // from then on is let go once the Serving Server is judged gone.
        liveness.begun_within(startup).await?;
        unless_standing.keep();
        tokio::spawn(async move { liveness.lost(startup, keepalive).await });
        Ok(JoinedCarrier { sender, places })
    };
    tokio::select! {
        biased;
        () = async { while interest.changed().await.is_ok() {} } => Err(no_longer_wanted()),
        joined = joining => joined,
    }
    .map_err(Arc::new)
}

/// How a connection to a Serving Server is made by one way: obtained from
/// the way, with the pinned-key TLS run over it.
#[derive(Clone)]
struct WayConnector {
    way: Way,
    tls: TlsConnector,
    dialer: WayDialer,
    /// Ends once nothing wants the connection any longer.
    interest: watch::Receiver<()>,
}

impl WayConnector {
    /// A connection to the Serving Server by the way, the pinned-key TLS
    /// done over it, made only for as long as it is wanted.
    async fn connect(&self) -> std::io::Result<PairedConnection> {
        let mut interest = self.interest.clone();
        let connecting = async {
            let connection =
                open_connection(&self.way, &self.dialer, Wanted(self.interest.clone())).await?;
            let server = ServerName::try_from(SERVING_IDENTITY_NAME)
                .expect("the Serving identity's name is a TLS server name");
            let paired = tokio::time::timeout(
                self.dialer.handshake_timeout,
                self.tls.connect(server, connection),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the Pairing's TLS handshake did not finish in time",
                )
            })??;
            Ok(PairedConnection(paired))
        };
        // Letting go is heard first, so a connection whose interest has gone
        // makes no more progress, however much is ready to be made.
        tokio::select! {
            biased;
            () = async { while interest.changed().await.is_ok() {} } => Err(no_longer_wanted()),
            connected = connecting => connected,
        }
    }
}

/// What dials a Serving Server's ways: a direct way through the proxy named
/// for its address, where one is, and a Relay way through this Server's
/// Relays, to the Serving Server whose identity key is `server`.
#[derive(Clone)]
struct WayDialer {
    proxies: DirectProxies,
    relays: GivenRelays,
    server: Arc<[u8]>,
    /// How long the Pairing's TLS handshake over a connection a way gave may
    /// take before the way is given up, so a Serving Server — or a Relay
    /// carrying a join — that never finishes it holds nothing up.
    handshake_timeout: tokio::time::Duration,
    /// How long each new dial tries the direct ways alone before starting
    /// the Relay ways beside them.
    direct_head_start: tokio::time::Duration,
    /// How long a direct connection kept for the next request may stand idle
    /// before it is closed.
    direct_idle_timeout: tokio::time::Duration,
    /// How long, at least, requests riding a joined stream go between tries
    /// of the direct ways in the background.
    direct_retry_interval: tokio::time::Duration,
    /// How a joined stream over a Relay way makes sure the Serving Server
    /// still answers.
    keepalive: JoinedKeepalive,
}

impl WayDialer {
    /// Whether a Remote's `way` is dialled: a direct way always, and a Relay
    /// way only at a Relay this Server's user has chosen by adding it. A
    /// Relay way at any other — one the Remote told of, say — is listed with
    /// the Remote, and nothing is asked of that Relay.
    fn dials(&self, way: &Way) -> bool {
        match way {
            Way::Direct(_) => true,
            Way::Relay(relay) => self.relays.get().is_some_and(|relays| relays.chosen(relay)),
        }
    }
}

/// A connection to a Serving Server over which the pinned-key TLS has been
/// established.
struct PairedConnection(tokio_rustls::client::TlsStream<Box<dyn ByteStream>>);

impl AsyncRead for PairedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(context, buffer)
    }
}

impl AsyncWrite for PairedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(context)
    }
}

/// How what was asked of a Serving Server over one way went.
enum WayAttempt<T> {
    Answered(T),
    TryNext,
    Rejected(PairingFailure),
}

/// What has become of asking a Serving Server by its ways so far: the ways
/// that have failed it, not to be dialled again, and why a Relay way was
/// refused, where its user can act on it.
#[derive(Default)]
struct Asked {
    failed: Vec<Way>,
    refused: Option<RelayRefusal>,
}

impl Asked {
    /// Holds `refused` as why a Relay way was refused where nothing is held
    /// yet — or where it is more for the user to do themselves than what is
    /// held: a login, then logging in under one Account, before a cap the
    /// Relay's operator alone can lift — so that is what is said of a
    /// Serving Server no way reached, whichever way was dialled first.
    fn refused_also(&mut self, refused: Option<RelayRefusal>) {
        let Some(refused) = refused else {
            return;
        };
        let precedence = |refusal: &RelayRefusal| match refusal.code {
            SessionErrorCode::RelayLoginNeeded => 0,
            SessionErrorCode::RelayDifferentAccounts => 1,
            _ => 2,
        };
        if self
            .refused
            .as_ref()
            .is_none_or(|held| precedence(&refused) < precedence(held))
        {
            self.refused = Some(refused);
        }
    }
}

/// How asking a Serving Server by its ways came to nothing.
enum NoAnswer {
    /// It answered with a refusal.
    Refused(PairingFailure),
    /// No way reached it: whether one presented a key other than its pinned
    /// key, and the first Relay way refused for a reason its user can act on.
    Unreached {
        other_key: bool,
        refused: Option<RelayRefusal>,
    },
}

/// What came of following what a Serving Server tells of the Relays it
/// Serves through.
enum Followed {
    /// Nothing asks it through the client any longer, or its Pairing no
    /// longer stands as it did: nothing more is followed.
    LetGo,
    /// It would not tell, or told what no Serving Server says: nothing more
    /// is followed through the client.
    Refused,
    /// No way carried what was asked, or the answer it rode ended: followed
    /// again once the Serving Server answers.
    Lost,
}

/// What a Serving Server tells of the Relays it Serves through, over the
/// answer it holds open: a line for each telling.
struct Telling<B = AnswerBody> {
    answer: B,
    /// What has come of the next telling so far.
    coming: Vec<u8>,
}

impl<B: HttpBody<Data = Bytes> + Unpin> Telling<B> {
    fn new(answer: B) -> Self {
        Self {
            answer,
            coming: Vec::new(),
        }
    }

    /// The Relays told next, once the whole of the telling has come. Let go
    /// of before then, it loses nothing of what is coming.
    async fn next(&mut self) -> std::result::Result<Vec<String>, Followed> {
        loop {
            // A telling ends within the budget, or not at all.
            let within = self.coming.len().min(PAIRING_ANSWER_BUDGET + 1);
            if let Some(end) = self.coming[..within].iter().position(|byte| *byte == b'\n') {
                let told = self.coming.drain(..=end).collect::<Vec<_>>();
                return told_relays(&told[..end]).ok_or(Followed::Refused);
            }
            if self.coming.len() > PAIRING_ANSWER_BUDGET {
                return Err(Followed::Refused);
            }
            match self.answer.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.coming.extend_from_slice(&data);
                    }
                }
                Some(Err(_)) | None => return Err(Followed::Lost),
            }
        }
    }
}

/// The Relays `told` says a Serving Server Serves through, where it says so
/// as one does: no more than [`MAX_TOLD_RELAYS`] of them, each once, each
/// address no longer than [`MAX_TOLD_RELAY_LEN`] and written the one way a
/// Relay's address is.
fn told_relays(told: &[u8]) -> Option<Vec<String>> {
    let OfferedRelays { relays } = serde_json::from_slice(told).ok()?;
    let well_told = relays.len() <= MAX_TOLD_RELAYS
        && relays.iter().collect::<HashSet<_>>().len() == relays.len()
        && relays
            .iter()
            .all(|relay| relay.len() <= MAX_TOLD_RELAY_LEN && canonical_relay(relay));
    well_told.then_some(relays)
}

/// A Remote's `ways` keeping up with `relays`, the Relays its Serving Server
/// says it Serves through: its direct ways as they are, each Relay way still
/// told where it was, and those told newly after them all, as told.
fn kept_up(ways: &[Way], relays: &[String]) -> Vec<Way> {
    let told = relays.iter().cloned().map(Way::Relay).collect::<Vec<_>>();
    let mut kept = ways
        .iter()
        .filter(|way| matches!(way, Way::Direct(_)) || told.contains(way))
        .cloned()
        .collect::<Vec<_>>();
    kept.extend(told.into_iter().filter(|way| !ways.contains(way)));
    kept
}

/// Where among `remotes` the Remote `followed` stands under the Pairing it
/// was followed under, while `client`, still asked through, is the one
/// `clients` say this Server asks it through, and pins that very Pairing.
fn followed_at(
    clients: &HashMap<String, Weak<PairingHttpClient>>,
    remotes: &[StoredRemote],
    followed: &StoredRemote,
    client: &Weak<PairingHttpClient>,
) -> Option<usize> {
    let current = clients.get(&followed.remote.name)?;
    let pinned = Weak::ptr_eq(current, client)
        && current
            .upgrade()
            .is_some_and(|current| current.pins(followed));
    if !pinned {
        return None;
    }
    remotes
        .iter()
        .position(|stored| same_pairing(stored, followed))
}

/// Whether `stored` and `other` are records of the one Pairing: under one
/// name, with one key, and made at one time.
fn same_pairing(stored: &StoredRemote, other: &StoredRemote) -> bool {
    stored.remote.name == other.remote.name
        && stored.public_key == other.public_key
        && stored.generation == other.generation
}

/// Asks the Serving Server `client` reaches what `asking` makes, carried as
/// [`PairingHttpClient::carrier`] chooses from `ways`, judging each answer as
/// `judging` does, until a way answers or refuses: the way that answered, and
/// what came of it. A way whose answer is judged one to try past is not
/// dialled again, and what went over a connection kept from before that had
/// gone meanwhile is asked again over another.
async fn first_answer<T, F, Fut>(
    client: &PairingHttpClient,
    ways: &[Way],
    mut asking: impl FnMut() -> Request<Body>,
    mut judging: F,
) -> std::result::Result<(Way, T), NoAnswer>
where
    F: FnMut(std::result::Result<Answer, Unanswered>) -> Fut,
    Fut: Future<Output = WayAttempt<T>>,
{
    let rejected_before = client.server_key_rejections.load(Ordering::Acquire);
    let mut asked = Asked::default();
    while let Some((way, carrier)) = client.carrier(ways, &mut asked).await {
        let answer = carrier.send(asking()).await;
        if answer.as_ref().is_err_and(|unanswered| unanswered.stale) {
            continue;
        }
        match judging(answer).await {
            WayAttempt::Answered(answer) => return Ok((way, answer)),
            WayAttempt::TryNext => asked.failed.push(way),
            WayAttempt::Rejected(failure) => return Err(NoAnswer::Refused(failure)),
        }
    }
    Err(NoAnswer::Unreached {
        other_key: client.server_key_rejections.load(Ordering::Acquire) != rejected_before,
        refused: asked.refused,
    })
}

/// Asks `remote` as [`first_answer`] does, by those of its ways this Server
/// dials ([`WayDialer::dials`]), in the order each kind is dialled in.
async fn first_remote_answer<T, F, Fut>(
    remote: &StoredRemote,
    client: &PairingHttpClient,
    asking: impl FnMut() -> Request<Body>,
    judging: F,
) -> std::result::Result<(Way, T), PairingFailure>
where
    F: FnMut(std::result::Result<Answer, Unanswered>) -> Fut,
    Fut: Future<Output = WayAttempt<T>>,
{
    let ways = remote
        .dialling_order()
        .into_iter()
        .filter(|way| client.dialer.dials(way))
        .collect::<Vec<_>>();
    match first_answer(client, &ways, asking, judging).await {
        Ok(answered) => Ok(answered),
        Err(NoAnswer::Refused(failure)) => Err(failure),
        Err(NoAnswer::Unreached {
            other_key: true, ..
        }) => Err(PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "Remote presented a key other than its pinned key",
        )),
        // A Relay way refused for its cap on the Account says why no way
        // reached the Remote, where its user can do something about it.
        Err(NoAnswer::Unreached {
            refused: Some(refusal),
            ..
        }) if refusal.code == SessionErrorCode::RelayCapReached => {
            Err(PairingFailure::refused(refusal))
        }
        // So does one refused for want of a Login there, or for the two
        // Servers' Logins there standing under different Accounts, though the
        // Remote reads Unreachable like any other no way reached, and is
        // tried again as one is: another of its ways may answer meanwhile,
        // and a login from any Server of either Account may join them.
        Err(NoAnswer::Unreached {
            refused: Some(refusal),
            ..
        }) if matches!(
            refusal.code,
            SessionErrorCode::RelayLoginNeeded | SessionErrorCode::RelayDifferentAccounts
        ) =>
        {
            Err(PairingFailure {
                code: SessionErrorCode::PairingConnectionFailed,
                message: refusal.message,
                unreachable: refusal.unreachable,
            })
        }
        Err(NoAnswer::Unreached { .. }) => Err(PairingFailure::new(
            SessionErrorCode::PairingConnectionFailed,
            "could not reach Remote at any paired address",
        )),
    }
}

fn paired_http_client(
    server_key: &[u8],
    identity: &IdentityMaterial,
    enrollment_token: Option<&str>,
    dialer: WayDialer,
) -> Result<PairingHttpClient> {
    let server_key_rejections = Arc::new(AtomicU64::new(0));
    let certificate = match enrollment_token {
        Some(token) => enrollment_certificate(identity, token)?,
        None => identity.certificate.clone(),
    };
    let mut tls = ClientConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("choose Pairing TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerKey {
            expected: server_key.to_vec(),
            rejected: server_key_rejections.clone(),
        }))
        .with_client_auth_cert(
            vec![CertificateDer::from(certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key.clone())),
        )
        .context("configure Pairing client identity")?;
    // The Serving Server is known by its pinned key, not by a name, so none
    // is said in the clear to whatever carries the connection.
    tls.enable_sni = false;
    let mut joined_tls = tls.clone();
    joined_tls.alpn_protocols = vec![MULTIPLEXED.to_vec()];
    Ok(PairingHttpClient {
        tls: TlsConnector::from(Arc::new(tls)),
        joined_tls: TlsConnector::from(Arc::new(joined_tls)),
        dialer,
        over_ways: StdMutex::default(),
        server_key_rejections,
        interest: watch::Sender::new(()),
        direct_retry: Arc::default(),
        keeping_up: AtomicBool::default(),
        answers: AtomicU64::default(),
        generation: 0,
    })
}

fn enrollment_certificate(identity: &IdentityMaterial, token: &str) -> Result<Vec<u8>> {
    let signing_key =
        KeyPair::try_from(identity.private_key.as_slice()).context("read Server identity key")?;
    let mut params = CertificateParams::new(Vec::new()).context("describe enrollment identity")?;
    params.distinguished_name = CertificateDistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("suru-invite-{token}"));
    Ok(params
        .self_signed(&signing_key)
        .context("mint enrollment identity certificate")?
        .der()
        .as_ref()
        .to_vec())
}

async fn decode_pairing_response(response: Answer) -> PairingFailure {
    let status = response.status();
    match small_answer::<SessionError>(response).await {
        Some(error) => PairingFailure::new(error.code, error.message),
        None => PairingFailure::new(
            SessionErrorCode::PairingConnectionFailed,
            format!("Serving Server refused enrollment with HTTP {status}"),
        ),
    }
}

fn parse_invite(invite: &str) -> std::result::Result<ParsedInvite, PairingFailure> {
    let Some(rest) = invite.strip_prefix("suru-") else {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInvite,
            "Invite is malformed",
        ));
    };
    let Some((version, encoded)) = rest.split_once('-') else {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInvite,
            "Invite is malformed",
        ));
    };
    if version != "v1" {
        return Err(PairingFailure::new(
            SessionErrorCode::UnsupportedInviteVersion,
            format!("Invite version `{version}` is not supported"),
        ));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InvitePayload>(&bytes).ok())
        .ok_or_else(|| {
            PairingFailure::new(
                SessionErrorCode::InvalidInvite,
                "Invite payload is malformed",
            )
        })?;
    if !ways_are_unique_and_nonempty(&payload.ways) || !payload.ways.iter().all(well_formed) {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInvite,
            "Invite addresses are malformed",
        ));
    }
    if payload.hostname.trim().is_empty()
        || payload.hostname != payload.hostname.trim()
        || payload.hostname.len() > 255
        || payload.hostname.chars().any(char::is_control)
    {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInvite,
            "Invite hostname is malformed",
        ));
    }
    let server_key = URL_SAFE_NO_PAD.decode(payload.server_key).map_err(|_| {
        PairingFailure::new(SessionErrorCode::InvalidInvite, "Invite key is malformed")
    })?;
    if server_key.is_empty() || server_key.len() > 4096 {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInvite,
            "Invite key is malformed",
        ));
    }
    Ok(ParsedInvite {
        ways: payload.ways,
        server_key,
        token: decode_token(&payload.token)?,
        hostname: payload.hostname,
    })
}

fn ways_are_unique_and_nonempty(ways: &[Way]) -> bool {
    !ways.is_empty() && ways.iter().collect::<HashSet<_>>().len() == ways.len()
}

/// Whether `way` is written as an Invite carries one: a Relay way names its
/// Relay by the one way a Relay's address is written.
fn well_formed(way: &Way) -> bool {
    match way {
        Way::Direct(_) => true,
        Way::Relay(relay) => canonical_relay(relay),
    }
}

/// Whether `relay` is a Relay's address written the one way one is.
fn canonical_relay(relay: &str) -> bool {
    suru_relay_protocol::canonical_address(relay).as_deref() == Some(relay)
}

fn decode_token(encoded: &str) -> std::result::Result<[u8; 32], PairingFailure> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            PairingFailure::new(SessionErrorCode::InvalidInvite, "Invite token is malformed")
        })
}

fn ordered_ways(offered: &[Way], chosen: &[Way]) -> std::result::Result<Vec<Way>, PairingFailure> {
    if chosen.is_empty() {
        return Ok(offered.to_vec());
    }
    let offered_set = offered.iter().collect::<HashSet<_>>();
    let chosen_set = chosen.iter().collect::<HashSet<_>>();
    if offered_set != chosen_set || chosen_set.len() != chosen.len() {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInviteWays,
            "ordered addresses must contain each offered address exactly once",
        ));
    }
    Ok(chosen.to_vec())
}

/// What `response` says, decoded as `T` — `None` where it says anything
/// else, or more than [`PAIRING_ANSWER_BUDGET`], which is read no further.
async fn small_answer<T: DeserializeOwned>(
    response: hyper::Response<impl HttpBody<Error: Into<axum::BoxError>>>,
) -> Option<T> {
    let read = Limited::new(response.into_body(), PAIRING_ANSWER_BUDGET)
        .collect()
        .await
        .ok()?
        .to_bytes();
    serde_json::from_slice(&read).ok()
}

/// Refuses a Remote's name with nothing in it, with anything around it, or
/// that is [`EVERYWHERE`](crate::protocol::EVERYWHERE) in any case — the word
/// that names every Server at once wherever an Origin is named, so a Remote
/// by that name, named back as its Origin, would name every Server instead.
fn validate_remote_name(name: &str) -> std::result::Result<(), PairingFailure> {
    if name.trim().is_empty() || name != name.trim() {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidRemoteName,
            "Remote name must contain non-whitespace characters without outer whitespace",
        ));
    }
    if crate::protocol::names_everywhere(name) {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidRemoteName,
            format!(
                "`{}` names every Server at once, so no Remote may be named so; choose another \
                 name",
                crate::protocol::EVERYWHERE
            ),
        ));
    }
    Ok(())
}

fn remote_name_conflict(name: &str) -> PairingFailure {
    PairingFailure::new(
        SessionErrorCode::RemoteNameConflict,
        format!("a Remote named `{name}` already exists"),
    )
}

fn protocol_mismatch(expected: u32, actual: u32) -> PairingFailure {
    PairingFailure::new(
        SessionErrorCode::PairingProtocolMismatch,
        format!("Pairing protocol mismatch: local v{expected}, remote v{actual}"),
    )
}

/// The failure of a Pairing operation that needed this Server's identity
/// key and could not get it: worded for the user where the key could not be
/// got from where it is kept, and as any failure of the operation otherwise.
fn identity_failure(error: anyhow::Error) -> PairingFailure {
    match error.downcast_ref::<IdentityKeyUnavailable>() {
        Some(unavailable) => PairingFailure::new(
            SessionErrorCode::IdentityKeyUnavailable,
            unavailable.to_string(),
        ),
        None => internal_pairing_failure(error),
    }
}

fn internal_pairing_failure(_error: anyhow::Error) -> PairingFailure {
    // Credential-bearing inputs and filesystem contents are deliberately not
    // copied into an outward error which a caller might later Log.
    PairingFailure::new(
        SessionErrorCode::PairingConnectionFailed,
        "Pairing operation failed",
    )
}

fn new_token() -> [u8; 32] {
    let first = *Uuid::new_v4().as_bytes();
    let second = *Uuid::new_v4().as_bytes();
    let mut token = [0_u8; 32];
    token[..16].copy_from_slice(&first);
    token[16..].copy_from_slice(&second);
    token
}

fn fingerprint(public_key: &[u8]) -> String {
    blake3::hash(public_key).to_hex().to_string()
}

fn public_key_from_certificate(
    certificate: &CertificateDer<'_>,
) -> std::result::Result<Vec<u8>, TlsError> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate.as_ref())
        .map_err(|_| TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    Ok(certificate.public_key().raw.to_vec())
}

fn enrollment_token_from_certificate(certificate: &CertificateDer<'_>) -> Option<[u8; 32]> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate.as_ref()).ok()?;
    let encoded = certificate
        .subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()?
        .strip_prefix("suru-invite-")?;
    URL_SAFE_NO_PAD.decode(encoded).ok()?.try_into().ok()
}

pub(crate) fn machine_hostname() -> String {
    let hostname = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "remote".to_owned());
    let hostname = hostname.trim();
    if hostname.is_empty() {
        "remote".to_owned()
    } else {
        hostname.to_owned()
    }
}

pub(crate) fn read_records<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => {
            protect_current_user_file(path)?;
            serde_json::from_slice(&bytes).with_context(|| format!("read Pairing records {path:?}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("read Pairing records {path:?}")),
    }
}

/// Stores `value` as the Pairing records at `path`, replacing what was there
/// whole or not at all.
fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut records = serde_json::to_vec(value).context("encode Pairing records")?;
    records.push(b'\n');
    replace_private_file(path, &records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relays that count the joins asked of them and make each at once, its
    /// far end held, so a join asked is a join made.
    #[derive(Default)]
    struct CountingRelays {
        asked: AtomicU64,
        far_ends: StdMutex<Vec<tokio::io::DuplexStream>>,
    }

    impl RelayWays for CountingRelays {
        fn served_through(&self, _relay: &str) -> Option<String> {
            None
        }

        fn chosen(&self, _relay: &str) -> bool {
            true
        }

        fn join(&self, _relay: String, _server: Vec<u8>, _wanted: Wanted) -> RelayJoin {
            self.asked.fetch_add(1, Ordering::AcqRel);
            let (near, far) = tokio::io::duplex(64 * 1024);
            self.far_ends.lock().unwrap().push(far);
            Box::pin(async move { Ok(Box::new(near) as Box<dyn ByteStream>) })
        }
    }

    /// A connection begun for interest since let go asks nothing of its way,
    /// though the way would answer at once: letting go wins over whatever
    /// progress is ready to be made. Each attempt is a fresh chance for the
    /// two to be taken in either order.
    #[tokio::test]
    async fn no_join_is_asked_for_interest_let_go_though_the_relay_would_join_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let identity = IdentityKey::new(directory.path()).material().unwrap();
        let relays = Arc::new(CountingRelays::default());
        let given = GivenRelays::default();
        let _ = given.set(relays.clone());
        let dialer = WayDialer {
            proxies: DirectProxies::given("", ""),
            relays: given,
            server: identity.public_key.clone().into(),
            handshake_timeout: tokio::time::Duration::from_secs(60),
            direct_head_start: tokio::time::Duration::from_millis(250),
            direct_idle_timeout: tokio::time::Duration::from_secs(90),
            direct_retry_interval: tokio::time::Duration::from_secs(5),
            keepalive: JoinedKeepalive {
                interval: tokio::time::Duration::from_secs(15),
                timeout: tokio::time::Duration::from_secs(30),
            },
        };
        let client = paired_http_client(&identity.public_key, &identity, None, dialer).unwrap();
        let interest = client.interest.subscribe();
        let way = Way::Relay("http://relay.invalid".to_owned());
        let connector = WayConnector {
            way,
            tls: client.tls.clone(),
            dialer: client.dialer.clone(),
            interest,
        };
        drop(client);

        for _ in 0..64 {
            let connected = connector.connect().await;
            assert_eq!(
                connected.err().map(|error| error.kind()),
                Some(std::io::ErrorKind::Interrupted)
            );
            let joined = join_stream(connector.clone()).await;
            assert_eq!(
                joined.err().map(|error| error.kind()),
                Some(std::io::ErrorKind::Interrupted),
                "a joined stream begun for interest let go asks nothing either"
            );
        }
        assert_eq!(
            relays.asked.load(Ordering::Acquire),
            0,
            "no join is asked once nothing is asked of the Serving Server"
        );
    }

    /// What accepts the pinned-key TLS a stand-in Serving Server holding
    /// `identity` runs, agreeing HTTP/2 with whoever asks for it.
    fn standing_in(identity: &IdentityMaterial) -> TlsAcceptor {
        let mut tls = ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(identity.certificate.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key.clone())),
            )
            .unwrap();
        tls.alpn_protocols = vec![MULTIPLEXED.to_vec()];
        TlsAcceptor::from(Arc::new(tls))
    }

    /// Relays that join this Server, each time, to a stand-in Serving Server
    /// that finishes the pinned-key TLS handshake as `tls` accepts it and
    /// then says nothing, holding the connection open.
    struct SilentServing {
        tls: TlsAcceptor,
    }

    impl RelayWays for SilentServing {
        fn served_through(&self, _relay: &str) -> Option<String> {
            None
        }

        fn chosen(&self, _relay: &str) -> bool {
            true
        }

        fn join(&self, _relay: String, _server: Vec<u8>, _wanted: Wanted) -> RelayJoin {
            let (near, far) = tokio::io::duplex(64 * 1024);
            let tls = self.tls.clone();
            tokio::spawn(async move {
                let _accepted = tls.accept(far).await;
                std::future::pending::<()>().await;
            });
            Box::pin(async move { Ok(Box::new(near) as Box<dyn ByteStream>) })
        }
    }

    /// What dials the Serving Server whose identity is `identity` through
    /// `relays`, each handshake given `handshake_timeout`.
    fn dialling(
        identity: &IdentityMaterial,
        relays: Arc<dyn RelayWays>,
        handshake_timeout: tokio::time::Duration,
    ) -> WayDialer {
        let given = GivenRelays::default();
        let _ = given.set(relays);
        WayDialer {
            proxies: DirectProxies::given("", ""),
            relays: given,
            server: identity.public_key.clone().into(),
            handshake_timeout,
            direct_head_start: tokio::time::Duration::from_millis(250),
            direct_idle_timeout: tokio::time::Duration::from_secs(90),
            direct_retry_interval: tokio::time::Duration::from_secs(5),
            keepalive: JoinedKeepalive {
                interval: tokio::time::Duration::from_secs(15),
                timeout: tokio::time::Duration::from_secs(30),
            },
        }
    }

    /// A joined stream whose Serving Server finishes the pinned-key TLS
    /// handshake and then never begins HTTP/2 is given up within the
    /// handshake timeout, rather than stood on until its keepalive gives up.
    #[tokio::test]
    async fn a_joined_stream_whose_serving_server_never_begins_http2_is_given_up_in_time() {
        let directory = tempfile::tempdir().unwrap();
        let identity = IdentityKey::new(directory.path()).material().unwrap();
        let relays = Arc::new(SilentServing {
            tls: standing_in(&identity),
        });
        let dialer = dialling(&identity, relays, tokio::time::Duration::from_millis(100));
        let client = paired_http_client(&identity.public_key, &identity, None, dialer).unwrap();
        let connector = client.connector(
            &Way::Relay("http://relay.invalid".to_owned()),
            &client.joined_tls,
        );

        let joined =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), join_stream(connector))
                .await
                .expect("the joined stream is settled long before its keepalive would give it up");
        assert_eq!(
            joined.err().map(|error| error.kind()),
            Some(std::io::ErrorKind::TimedOut),
            "a joined stream over which HTTP/2 never begins does not stand"
        );
    }

    /// A transport that, once shut, takes nothing in and gives nothing out,
    /// though it stays open — as a Relay or a proxy that stops reading while
    /// its own end goes on answering, its window closed, would.
    struct Valve<S> {
        inner: S,
        shut: Arc<AtomicBool>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Valve<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.shut.load(Ordering::Acquire) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Valve<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.shut.load(Ordering::Acquire) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_write(context, buffer)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.shut.load(Ordering::Acquire) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.shut.load(Ordering::Acquire) {
                return Poll::Pending;
            }
            Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

    /// How a joined stream's two Servers make sure of each other in a test.
    const TEST_KEEPALIVE: JoinedKeepalive = JoinedKeepalive {
        interval: tokio::time::Duration::from_millis(50),
        timeout: tokio::time::Duration::from_millis(200),
    };

    /// The Serving side of a joined stream lets its transport go once the
    /// redeeming Server has fallen silent, though what it was sending — an
    /// Attachment's bytes, filling everything on the way — can never be
    /// flushed: HTTP/2's keepalive gives the stream up and then waits on a
    /// graceful close the transport cannot carry, so the transport is dropped
    /// regardless, a bound past that verdict.
    #[tokio::test]
    async fn a_joined_stream_the_serving_side_cannot_flush_is_let_go_once_judged_gone() {
        let (serving_end, redeeming_end) = tokio::io::duplex(64 * 1024);
        let app = Router::new().route("/attachment", get(|| async { vec![7_u8; 8 * 1024 * 1024] }));
        let serving = tokio::spawn(serve_joined(
            serving_end,
            app,
            TEST_KEEPALIVE,
            tokio::time::Duration::from_secs(5),
        ));
        let shut = Arc::new(AtomicBool::new(false));
        let valve = Valve {
            inner: redeeming_end,
            shut: shut.clone(),
        };
        let (mut sender, connection) = http2::Builder::new(TokioExecutor::new())
            .handshake::<_, Body>(TokioIo::new(valve))
            .await
            .unwrap();
        tokio::spawn(connection);
        let fetching = sender
            .send_request(
                Request::get("https://suru-server/attachment")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fetching.status(), StatusCode::OK);

        shut.store(true, Ordering::Release);
        let let_go = tokio::time::timeout(tokio::time::Duration::from_secs(5), serving).await;
        assert!(
            let_go.is_ok(),
            "the Serving side lets go of a joined stream it can neither hear nor flush"
        );
        drop((sender, fetching));
    }

    /// A transport that says when it has been let go.
    struct Noticed<S> {
        inner: S,
        let_go: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl<S> Drop for Noticed<S> {
        fn drop(&mut self) {
            if let Some(let_go) = self.let_go.take() {
                let _ = let_go.send(());
            }
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Noticed<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Noticed<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(context, buffer)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

    /// Relays that join this Server, once, to a stand-in Serving Server that
    /// begins HTTP/2 as `tls` accepts it and then answers nothing, behind a
    /// valve the test shuts; the join says when it is let go.
    struct ValvedServing {
        tls: TlsAcceptor,
        shut: Arc<AtomicBool>,
        let_go: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl RelayWays for ValvedServing {
        fn served_through(&self, _relay: &str) -> Option<String> {
            None
        }

        fn chosen(&self, _relay: &str) -> bool {
            true
        }

        fn join(&self, _relay: String, _server: Vec<u8>, _wanted: Wanted) -> RelayJoin {
            let (near, far) = tokio::io::duplex(64 * 1024);
            let (tls, far) = (
                self.tls.clone(),
                Valve {
                    inner: far,
                    shut: self.shut.clone(),
                },
            );
            tokio::spawn(async move {
                let Ok(accepted) = tls.accept(far).await else {
                    return;
                };
                let answering = hyper::service::service_fn(|_: Request<Incoming>| {
                    std::future::pending::<std::result::Result<Response, std::convert::Infallible>>(
                    )
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(accepted), answering)
                    .await;
            });
            let near = Noticed {
                inner: near,
                let_go: self.let_go.lock().unwrap().take(),
            };
            Box::pin(async move { Ok(Box::new(near) as Box<dyn ByteStream>) })
        }
    }

    /// The redeeming side of a joined stream lets its transport go once the
    /// Serving Server has fallen silent, though what it was sending — an
    /// upload, filling everything on the way — can never be flushed, as the
    /// Serving side does.
    #[tokio::test]
    async fn a_joined_stream_the_redeeming_side_cannot_flush_is_let_go_once_judged_gone() {
        let directory = tempfile::tempdir().unwrap();
        let identity = IdentityKey::new(directory.path()).material().unwrap();
        let shut = Arc::new(AtomicBool::new(false));
        let (let_go, transport_let_go) = tokio::sync::oneshot::channel();
        let relays = Arc::new(ValvedServing {
            tls: standing_in(&identity),
            shut: shut.clone(),
            let_go: StdMutex::new(Some(let_go)),
        });
        let mut dialer = dialling(&identity, relays, tokio::time::Duration::from_secs(5));
        dialer.keepalive = TEST_KEEPALIVE;
        let client = paired_http_client(&identity.public_key, &identity, None, dialer).unwrap();
        let connector = client.connector(
            &Way::Relay("http://relay.invalid".to_owned()),
            &client.joined_tls,
        );
        let JoinedCarrier { mut sender, .. } = join_stream(connector)
            .await
            .expect("the joined stream stands");

        shut.store(true, Ordering::Release);
        let _uploading = tokio::spawn(
            sender.send_request(
                Request::post("https://suru-server/v1/attachments")
                    .body(Body::from(vec![7_u8; 8 * 1024 * 1024]))
                    .unwrap(),
            ),
        );
        let let_go =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), transport_let_go).await;
        assert!(
            let_go.is_ok(),
            "the redeeming side lets go of a joined stream it can neither hear nor flush"
        );
    }

    /// How a stand-in Serving Server at a direct way treats the requests it
    /// takes in.
    #[derive(Clone, Copy)]
    enum Treating {
        /// Answers each.
        Answering,
        /// Takes each in whole and drops its connection unanswered.
        Swallowing,
    }

    /// A stand-in Serving Server at a direct way, holding the identity it is
    /// given, which speaks HTTP/1.1 and treats requests as it is told,
    /// counting the connections dialled to it and the requests taken in.
    struct DirectStandIn {
        address: SocketAddr,
        dialled: Arc<AtomicU64>,
        asked: Arc<AtomicU64>,
    }

    impl DirectStandIn {
        async fn start(identity: &IdentityMaterial, treating: Treating) -> Self {
            use tokio::io::AsyncWriteExt as _;
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let tls = standing_in(identity);
            let (dialled, asked) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            let (counting_dials, counting_asks) = (dialled.clone(), asked.clone());
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    counting_dials.fetch_add(1, Ordering::AcqRel);
                    let (tls, asked) = (tls.clone(), counting_asks.clone());
                    tokio::spawn(async move {
                        let Ok(mut stream) = tls.accept(socket).await else {
                            return;
                        };
                        while took_in_a_request(&mut stream).await {
                            asked.fetch_add(1, Ordering::AcqRel);
                            if matches!(treating, Treating::Swallowing) {
                                return;
                            }
                            let answer = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                            if stream.write_all(answer).await.is_err()
                                || stream.flush().await.is_err()
                            {
                                return;
                            }
                        }
                    });
                }
            });
            Self {
                address,
                dialled,
                asked,
            }
        }

        fn counts(&self) -> (u64, u64) {
            (
                self.dialled.load(Ordering::Acquire),
                self.asked.load(Ordering::Acquire),
            )
        }
    }

    /// Takes in one HTTP/1.1 request whole from `stream`: whether one came.
    async fn took_in_a_request(stream: &mut (impl AsyncRead + Unpin)) -> bool {
        use tokio::io::AsyncReadExt as _;
        let (mut read, mut chunk) = (Vec::new(), [0_u8; 1024]);
        let mut head = None;
        loop {
            if head.is_none() {
                head = read
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|end| end + 4);
            }
            if let Some(head) = head {
                let length = String::from_utf8_lossy(&read[..head])
                    .lines()
                    .find_map(|line| {
                        let line = line.to_ascii_lowercase();
                        line.strip_prefix("content-length:")
                            .and_then(|length| length.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if read.len() >= head + length {
                    return true;
                }
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(taken) => read.extend_from_slice(&chunk[..taken]),
            }
        }
    }

    /// What asks the Serving Server whose identity is `identity` by direct
    /// ways alone.
    fn asking_directly(identity: &IdentityMaterial) -> Arc<PairingHttpClient> {
        let dialer = WayDialer {
            proxies: DirectProxies::given("", ""),
            relays: GivenRelays::default(),
            server: identity.public_key.clone().into(),
            handshake_timeout: tokio::time::Duration::from_secs(5),
            direct_head_start: tokio::time::Duration::from_millis(250),
            direct_idle_timeout: tokio::time::Duration::from_secs(90),
            direct_retry_interval: tokio::time::Duration::from_secs(5),
            keepalive: TEST_KEEPALIVE,
        };
        Arc::new(paired_http_client(&identity.public_key, identity, None, dialer).unwrap())
    }

    /// Judges an answer as answered where it says it succeeded, reading it
    /// whole, and as one to try past otherwise.
    async fn answered_if_successful(
        answer: std::result::Result<Answer, Unanswered>,
    ) -> WayAttempt<()> {
        match answer {
            Ok(response) if response.status().is_success() => {
                let _ = response.into_body().collect().await;
                WayAttempt::Answered(())
            }
            Ok(_) | Err(_) => WayAttempt::TryNext,
        }
    }

    /// A request handed to a direct connection kept from an earlier one,
    /// whose driver has gone with the request still queued — so nothing of it
    /// began to reach the Serving Server — is asked again over a fresh
    /// connection by the same way, which is not given up for it.
    #[tokio::test]
    async fn a_request_on_a_kept_connection_since_gone_is_asked_again_over_a_fresh_one() {
        let directory = tempfile::tempdir().unwrap();
        let identity = IdentityKey::new(directory.path()).material().unwrap();
        let stand_in = DirectStandIn::start(&identity, Treating::Answering).await;
        let client = asking_directly(&identity);
        let way = [Way::Direct(stand_in.address)];
        let OverWay::Direct(connections) = client.over(&way[0]) else {
            unreachable!("a direct way is asked directly");
        };
        // A connection kept from before, ready for the next request, whose
        // driver is driven here by hand and goes before taking that request
        // up.
        let (_far, near) = tokio::io::duplex(64 * 1024);
        let (kept, driving) = http1::handshake::<_, Body>(TokioIo::new(near))
            .await
            .unwrap();
        let mut driving = Box::pin(driving);
        assert!(futures_util::poll!(driving.as_mut()).is_pending());
        assert!(
            kept.is_ready(),
            "the kept connection is ready for a request"
        );
        connections.keep(kept);

        let asking = || Request::get("/health").body(Body::empty()).unwrap();
        let (asked, ()) = tokio::join!(
            first_answer(&client, &way, asking, answered_if_successful),
            async move {
                tokio::task::yield_now().await;
                drop(driving);
            },
        );
        assert!(
            asked.is_ok(),
            "the request is asked again over a fresh connection by the same way"
        );
        assert_eq!(
            stand_in.counts(),
            (1, 1),
            "one connection dialled afresh, and the request taken in once over it"
        );
    }

    /// A request that may have reached the Serving Server — sent whole, its
    /// answer lost as the connection dropped — is asked by another way only
    /// where asking it twice changes nothing: a POST is refused as one whose
    /// outcome is unknown, taken in once, while a GET is asked by the next
    /// way.
    #[tokio::test]
    async fn a_request_that_may_have_been_delivered_is_asked_again_only_where_that_changes_nothing()
    {
        let directory = tempfile::tempdir().unwrap();
        let identity = IdentityKey::new(directory.path()).material().unwrap();
        let first = DirectStandIn::start(&identity, Treating::Swallowing).await;
        let second = DirectStandIn::start(&identity, Treating::Swallowing).await;
        let client = asking_directly(&identity);
        let ways = [Way::Direct(first.address), Way::Direct(second.address)];
        let judging = |repeatable| {
            let client = client.clone();
            move |answer| {
                let client = client.clone();
                async move { judge_proxied(answer, repeatable, client) }
            }
        };

        let posting = || {
            Request::post("/v1/sessions")
                .body(Body::from("{}"))
                .unwrap()
        };
        let posted = first_answer(&client, &ways, posting, judging(false)).await;
        assert!(
            matches!(
                posted,
                Err(NoAnswer::Refused(PairingFailure {
                    code: SessionErrorCode::PairingOutcomeUnknown,
                    ..
                }))
            ),
            "a POST that may have been delivered is refused as such"
        );
        assert_eq!(
            (first.counts(), second.counts()),
            ((1, 1), (0, 0)),
            "and is not asked again by any way"
        );

        let getting = || Request::get("/v1/sessions").body(Body::empty()).unwrap();
        let got = first_answer(&client, &ways, getting, judging(true)).await;
        assert!(matches!(got, Err(NoAnswer::Unreached { .. })));
        assert_eq!(
            (first.counts(), second.counts()),
            ((2, 2), (1, 1)),
            "a GET is asked by the next way"
        );
    }

    /// A connection a Relay carried for a join taken up in one stretch of
    /// Serving is dropped once Serving has stopped and started again, while
    /// one carried for the stretch under way is taken.
    #[tokio::test]
    async fn a_connection_carried_for_a_stretch_of_serving_since_ended_is_dropped() {
        use futures_util::FutureExt as _;
        use tokio::io::AsyncReadExt as _;

        let directory = tempfile::tempdir().unwrap();
        let serving = ServingController::new(
            directory.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:1".to_owned(),
            "token".to_owned(),
        )
        .unwrap();
        let settings = |enabled| ServingSettings {
            enabled,
            listener: true,
            port: 0,
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        };
        // Whether the Serving side took the connection whose far end is
        // `far`: a connection it drops ends there and then.
        let taken = |far: &mut tokio::io::DuplexStream| {
            let mut byte = [0];
            far.read(&mut byte).now_or_never().is_none()
        };

        serving.adopt(settings(true)).await.unwrap();
        let first = serving.serving().borrow().number;
        let (mut far, near) = tokio::io::duplex(64);
        serving.accept_carried(near, first);
        assert!(taken(&mut far));

        serving.adopt(settings(false)).await.unwrap();
        serving.adopt(settings(true)).await.unwrap();
        let (mut far, near) = tokio::io::duplex(64);
        serving.accept_carried(near, first);
        assert!(
            !taken(&mut far),
            "a join taken up before Serving stopped is not handed on once it starts again"
        );
        let (mut far, near) = tokio::io::duplex(64);
        serving.accept_carried(near, serving.serving().borrow().number);
        assert!(taken(&mut far));

        serving.shutdown().await;
    }

    /// The listener opens and closes on its own Setting while Serving
    /// stands: closed, nothing listens where it was by the time the adoption
    /// answers, and every connection dialled to it is let go — one still
    /// handshaking among them — while the stretch of Serving the Relays wait
    /// in, and a connection a Relay carried, stand as they were; opened
    /// again, it listens anew in the same stretch.
    #[tokio::test]
    async fn the_listener_opens_and_closes_on_its_own_leaving_what_relays_carry() {
        use futures_util::FutureExt as _;
        use tokio::io::AsyncReadExt as _;

        let directory = tempfile::tempdir().unwrap();
        let serving = ServingController::new(
            directory.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:1".to_owned(),
            "token".to_owned(),
        )
        .unwrap();
        let settings = |listener| ServingSettings {
            enabled: true,
            listener,
            port: 0,
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        };
        // Whether the far end of a connection the Serving side took is still
        // open: a connection it lets go ends there and then.
        let open = |far: &mut tokio::io::DuplexStream| {
            let mut byte = [0];
            far.read(&mut byte).now_or_never().is_none()
        };

        serving.adopt(settings(false)).await.unwrap();
        assert_eq!(serving.address(), None, "nothing listens");
        let stretch = *serving.serving().borrow();
        assert!(stretch.serving, "and the Server is Serving all the same");
        let (mut carried, near) = tokio::io::duplex(64);
        serving.accept_carried(near, stretch.number);
        assert!(open(&mut carried), "a connection a Relay carries is taken");

        serving.adopt(settings(true)).await.unwrap();
        let address = serving.address().expect("the listener opens");
        let mut handshaking = tokio::net::TcpStream::connect(address).await.unwrap();

        serving.adopt(settings(false)).await.unwrap();
        assert_eq!(serving.address(), None);
        tokio::net::TcpStream::connect(address)
            .await
            .expect_err("nothing listens where the listener was");
        drop(
            tokio::net::TcpListener::bind(address)
                .await
                .expect("its port is free again"),
        );
        let mut byte = [0];
        assert!(
            matches!(
                tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    handshaking.read(&mut byte)
                )
                .await
                .expect("a connection dialled to the listener is let go with it"),
                Ok(0) | Err(_)
            ),
            "and says nothing"
        );
        assert_eq!(*serving.serving().borrow(), stretch, "Serving stands");
        assert!(
            open(&mut carried),
            "and so does what the Relay carried, still handshaking"
        );

        serving.adopt(settings(true)).await.unwrap();
        let reopened = serving.address().expect("the listener opens again");
        tokio::net::TcpStream::connect(reopened)
            .await
            .expect("and listens");
        assert_eq!(*serving.serving().borrow(), stretch);
        assert!(open(&mut carried));

        serving.shutdown().await;
    }

    /// A connection the listener took and handed on is closed as the
    /// listener closes, though the acceptor — with as many handshakes under
    /// way as it takes, each over a connection a Relay carried that says
    /// nothing — has yet to come to it; what the Relays carried stands.
    #[tokio::test]
    async fn the_listener_closes_a_connection_the_acceptor_has_no_room_for_yet() {
        use futures_util::FutureExt as _;
        use tokio::io::AsyncReadExt as _;

        // Waited on only where the listener or the acceptor fails.
        const STALLED: tokio::time::Duration = tokio::time::Duration::from_secs(60);
        let directory = tempfile::tempdir().unwrap();
        let serving = ServingController::new(
            directory.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:1".to_owned(),
            "token".to_owned(),
        )
        .unwrap()
        // So long that a handshake ends within the test only with its
        // connection, and makes no room for another.
        .with_handshake_timeout(tokio::time::Duration::from_secs(600));
        let settings = |listener| ServingSettings {
            enabled: true,
            listener,
            port: 0,
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        };
        let open = |far: &mut tokio::io::DuplexStream| {
            let mut byte = [0];
            far.read(&mut byte).now_or_never().is_none()
        };
        serving.adopt(settings(true)).await.unwrap();
        let address = serving.address().expect("the listener opens");
        let stretch = serving.serving().borrow().number;
        let mut carried = Vec::new();
        for _ in 0..SERVING_HANDSHAKES_AT_ONCE {
            let (far, near) = tokio::io::duplex(64);
            serving.accept_carried(near, stretch);
            carried.push(far);
            // The acceptor takes each up before the next arrives.
            tokio::task::yield_now().await;
        }

        let mut dialled = tokio::net::TcpStream::connect(address).await.unwrap();
        let handed_on = async {
            loop {
                let waiting = serving
                    .active
                    .lock()
                    .await
                    .as_ref()
                    .and_then(|running| running.listener.as_ref())
                    .is_some_and(|listening| {
                        listening
                            .waiting
                            .lock()
                            .unwrap()
                            .iter()
                            .any(Dialled::waiting)
                    });
                if waiting {
                    return;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(STALLED, handed_on)
            .await
            .expect("the listener takes the connection and hands it on, untaken");

        serving.adopt(settings(false)).await.unwrap();
        let mut byte = [0];
        assert!(
            matches!(
                tokio::time::timeout(STALLED, dialled.read(&mut byte))
                    .await
                    .expect("the connection closes with the listener"),
                Ok(0) | Err(_)
            ),
            "and says nothing"
        );
        assert!(
            carried.iter_mut().all(open),
            "what the Relays carried stands"
        );

        serving.shutdown().await;
    }

    /// A listener that stops on its own — here finding nothing to hand what
    /// is dialled to it on to — is told as not listening, and why, rather
    /// than left told as listening, and the next adoption of the Settings
    /// opens it anew.
    #[tokio::test]
    async fn a_listener_that_stops_on_its_own_is_told_so_and_opened_anew() {
        // Waited on only where the listener fails to say it stopped.
        const STALLED: tokio::time::Duration = tokio::time::Duration::from_secs(60);
        let directory = tempfile::tempdir().unwrap();
        let serving = ServingController::new(
            directory.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:1".to_owned(),
            "token".to_owned(),
        )
        .unwrap();
        let settings = ServingSettings {
            enabled: true,
            listener: true,
            port: 0,
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        };
        serving.adopt(settings).await.unwrap();
        let address = serving.address().expect("the listener opens");
        let mut told = serving.listener();
        assert_eq!(*told.borrow_and_update(), ListenerState::Open { address });

        // The acceptor gone, the listener has nothing to hand on to.
        {
            let active = serving.active.lock().await;
            let running = active.as_ref().expect("Serving runs");
            running.task.abort();
            while !running.task.is_finished() {
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        }
        let _dialled = tokio::net::TcpStream::connect(address).await.unwrap();
        let stopped = tokio::time::timeout(
            STALLED,
            told.wait_for(|state| *state != ListenerState::Open { address }),
        )
        .await
        .expect("the listener says it stopped")
        .expect("the Server still tells how its listener stands")
        .clone();
        assert_eq!(
            stopped,
            ListenerState::Failed {
                reason: "it stopped on its own".to_owned()
            }
        );
        assert_eq!(serving.address(), None);

        serving.adopt(settings).await.unwrap();
        let reopened = serving.address().expect("the next adoption opens it anew");
        assert_eq!(
            *serving.listener().borrow(),
            ListenerState::Open { address: reopened }
        );

        serving.shutdown().await;
    }

    #[test]
    fn a_peer_is_named_as_it_names_itself_where_a_reader_can_be_shown_that_and_no_other_is() {
        let id = "ab12cd34ef567890";
        let named = |hostname: Option<&str>, others: &[&str]| {
            peer_name(hostname, id, others.iter().copied())
        };
        assert_eq!(named(Some("  laptop "), &[]), "laptop");
        for unshown in [
            None,
            Some(""),
            Some("   "),
            Some("everywhere"),
            Some("Everywhere"),
            Some("lap\ttop"),
            Some("lap\u{202e}pot"),
            Some("lap\u{200b}top"),
        ] {
            assert_eq!(named(unshown, &[]), "peer ab12cd34", "{unshown:?}");
        }
        assert_eq!(
            named(Some(&"n".repeat(MAX_PEER_NAME_CHARS)), &[]),
            "n".repeat(MAX_PEER_NAME_CHARS)
        );
        assert_eq!(
            named(Some(&"n".repeat(MAX_PEER_NAME_CHARS + 1)), &[]),
            "peer ab12cd34",
            "a name too long to show is no name"
        );
        assert_eq!(
            named(Some("laptop"), &["desktop", "LAPTOP"]),
            "laptop (ab12cd34)",
            "a name another Peer goes by, in any case, is told apart by the fingerprint"
        );
        // Nothing a reader cannot see stands in a name: no formatting or
        // otherwise default-ignorable character, however unusual.
        for unshown in [
            "lap\u{ad}top",
            "lap\u{2064}top",
            "lap\u{e0001}top",
            "lap\u{180e}top",
        ] {
            assert_eq!(named(Some(unshown), &[]), "peer ab12cd34", "{unshown:?}");
        }
        // Names are told apart as a reader tells them apart: by Unicode case
        // folding, however the letters are composed.
        assert_eq!(named(Some("Åsa"), &["åsa"]), "Åsa (ab12cd34)");
        assert_eq!(
            named(Some("Stra\u{df}e"), &["STRASSE"]),
            "Stra\u{df}e (ab12cd34)"
        );
        assert_eq!(
            named(Some("cafe\u{301}"), &["caf\u{e9}"]),
            "caf\u{e9} (ab12cd34)"
        );
        // A told-apart name another Peer already goes by is told apart
        // further, by more of the fingerprint.
        assert_eq!(
            named(Some("laptop"), &["laptop", "laptop (ab12cd34)"]),
            "laptop (ab12cd34ef56)"
        );
        // And told apart, it never runs past the longest name: the name gives
        // way, never what tells it apart.
        let long = "n".repeat(MAX_PEER_NAME_CHARS);
        let told_apart = named(Some(&long), &[long.as_str()]);
        assert_eq!(told_apart.chars().count(), MAX_PEER_NAME_CHARS);
        assert!(told_apart.ends_with(" (ab12cd34)"), "{told_apart}");
    }

    #[test]
    fn only_an_act_whose_operation_judges_its_author_takes_one_from_a_peer() {
        for (method, path) in [
            (Method::POST, "/v1/sessions"),
            (Method::POST, "/v1/checkouts/prepare"),
            (Method::POST, "/v1/workspaces/description"),
            (Method::POST, "/v1/sessions/0198b27e/prompts"),
            (Method::POST, "/v1/sessions/0198b27e/interrupt"),
            (Method::POST, "/v1/sessions/0198b27e/settlement"),
            (
                Method::POST,
                "/v1/sessions/0198b27e/questionnaires/0198b27f",
            ),
        ] {
            assert!(takes_an_author(&method, path), "{method} {path}");
        }
        for (method, path) in [
            (Method::GET, "/v1/sessions"),
            (Method::DELETE, "/v1/sessions/0198b27e"),
            (
                Method::POST,
                "/v1/sessions/0198b27e/approvals/0198b27f/decision",
            ),
            (Method::POST, "/v1/sessions/0198b27e/approval-posture"),
            (Method::POST, "/v1/sessions/0198b27e/agent-selection"),
            (
                Method::POST,
                "/v1/sessions/0198b27e/prompts/0198b27f/cancel",
            ),
            (Method::POST, "/v1/workspaces/icon"),
            (Method::POST, "/v1/settings"),
            (Method::POST, "/v1/checkouts/remove"),
        ] {
            assert!(!takes_an_author(&method, path), "{method} {path}");
        }
    }

    /// An Invite names a Relay it offers by the one way a Relay's address is
    /// written, as the Server that issued it wrote it; one written any other
    /// way was written by nothing that issues Invites.
    #[test]
    fn an_invite_naming_a_relay_otherwise_than_by_its_address_is_malformed() {
        let invite = |relay: &str| {
            let payload = InvitePayload {
                ways: vec![Way::Relay(relay.to_owned())],
                server_key: URL_SAFE_NO_PAD.encode([7_u8; 91]),
                token: URL_SAFE_NO_PAD.encode([1_u8; 32]),
                hostname: "workstation".to_owned(),
            };
            format!(
                "suru-v1-{}",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
            )
        };
        let parsed = parse_invite(&invite("https://relay.example.com"))
            .ok()
            .expect("a Relay named by its address");
        assert_eq!(
            parsed.ways,
            vec![Way::Relay("https://relay.example.com".to_owned())]
        );
        for written in [
            "relay.example.com",
            "https://Relay.example.com/",
            "ftp://relay.example.com",
            "",
        ] {
            let refused = parse_invite(&invite(written))
                .err()
                .map(|refused| (refused.code, refused.message));
            assert_eq!(
                refused,
                Some((
                    SessionErrorCode::InvalidInvite,
                    "Invite addresses are malformed".to_owned()
                )),
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_remote_keeps_its_direct_ways_and_its_relay_ways_keep_up_with_those_told() {
        let direct = Way::Direct(SocketAddr::from(([192, 0, 2, 1], 7000)));
        let relay = |address: &str| Way::Relay(address.to_owned());
        let ways = vec![
            relay("https://dropped.example.com"),
            direct.clone(),
            relay("https://kept.example.com"),
        ];
        assert_eq!(
            kept_up(
                &ways,
                &[
                    "https://new.example.com".to_owned(),
                    "https://kept.example.com".to_owned()
                ]
            ),
            [
                direct.clone(),
                relay("https://kept.example.com"),
                relay("https://new.example.com")
            ],
            "a Relay way still told stays where it was, and one told newly comes after"
        );
        assert_eq!(kept_up(&ways, &[]), [direct], "the direct ways stay");
    }

    #[test]
    fn only_relays_told_as_a_serving_server_tells_them_are_taken_up() {
        let told = |relays: serde_json::Value| {
            told_relays(&serde_json::to_vec(&serde_json::json!({ "relays": relays })).unwrap())
        };
        assert_eq!(
            told(serde_json::json!(["https://relay.example.com"])),
            Some(vec!["https://relay.example.com".to_owned()])
        );
        assert_eq!(told(serde_json::json!([])), Some(Vec::new()));
        for unwritten in [
            "relay.example.com",
            "https://Relay.example.com",
            "https://relay.example.com/",
            "https://someone:secret@relay.example.com",
            "https://relay.example.com?next=elsewhere",
            "ftp://relay.example.com",
            "",
        ] {
            assert_eq!(
                told(serde_json::json!([unwritten])),
                None,
                "{unwritten:?} is no Relay's address written the one way"
            );
        }
        assert_eq!(
            told(serde_json::json!([
                "https://relay.example.com",
                "https://relay.example.com"
            ])),
            None,
            "a Relay told twice"
        );
        let many = (0..=MAX_TOLD_RELAYS)
            .map(|relay| format!("https://relay{relay}.example.com"))
            .collect::<Vec<_>>();
        assert!(told(serde_json::json!(many[..MAX_TOLD_RELAYS])).is_some());
        assert_eq!(told(serde_json::json!(many)), None, "too many Relays told");
        let long = format!(
            "https://relay.example.com/{}",
            "a".repeat(MAX_TOLD_RELAY_LEN)
        );
        assert_eq!(told(serde_json::json!([long])), None, "too long an address");
        assert_eq!(
            told_relays(br#"{"relays":[],"ways":["192.0.2.1:7000"]}"#),
            None,
            "nothing but Relays is taken up"
        );
        assert_eq!(told_relays(b"not what a Serving Server says"), None);
    }

    /// Nothing a Serving Server tells of its Relays is taken in past the
    /// budget, however it comes: in one piece or across several, ending its
    /// line or not.
    #[tokio::test]
    async fn no_telling_is_taken_in_past_the_budget() {
        let telling = |pieces: Vec<Vec<u8>>| {
            Telling::new(http_body_util::StreamBody::new(stream::iter(
                pieces.into_iter().map(|piece| {
                    Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(Bytes::from(piece)))
                }),
            )))
        };
        let told = br#"{"relays":["https://relay.example.com"]}"#;
        let mut whole = vec![b' '; PAIRING_ANSWER_BUDGET - told.len()];
        whole.extend_from_slice(told);
        whole.push(b'\n');
        let (first, rest) = whole.split_at(PAIRING_ANSWER_BUDGET / 2);
        assert_eq!(
            telling(vec![first.to_vec(), rest.to_vec()])
                .next()
                .await
                .ok(),
            Some(vec!["https://relay.example.com".to_owned()]),
            "a telling as long as the budget, in two pieces"
        );

        let mut past = vec![b' '; PAIRING_ANSWER_BUDGET];
        past.extend_from_slice(b"{\"relays\":[]}\n");
        for pieces in [
            vec![
                vec![b' '; PAIRING_ANSWER_BUDGET],
                b"{\"relays\":[]}\n".to_vec(),
            ],
            vec![past],
            vec![vec![b' '; PAIRING_ANSWER_BUDGET + 1]],
        ] {
            assert!(
                matches!(telling(pieces).next().await, Err(Followed::Refused)),
                "a telling past the budget is refused"
            );
        }
    }

    #[test]
    fn no_remote_may_be_named_everywhere_in_any_case() {
        for reserved in ["everywhere", "Everywhere", "EVERYWHERE"] {
            let refusal = validate_remote_name(reserved).expect_err("the name is reserved");
            assert_eq!(refusal.code, SessionErrorCode::InvalidRemoteName);
            assert_eq!(
                refusal.message,
                "`everywhere` names every Server at once, so no Remote may be named so; choose \
                 another name"
            );
        }
        for named in ["workstation", "everywhere-else", "not everywhere"] {
            assert!(validate_remote_name(named).is_ok(), "{named}");
        }
    }

    fn answering(status: StatusCode, body: Vec<u8>) -> Response {
        Response::builder()
            .status(status)
            .body(Body::from(body))
            .expect("an answer")
    }

    /// The local Session API as the Serving listener forwards to it, which
    /// answers every request by recording the headers it arrived with.
    async fn recording_local_api() -> (String, tokio::sync::mpsc::UnboundedReceiver<HeaderMap>) {
        let (seen, arrived) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().fallback(move |request: Request<Body>| {
            let seen = seen.clone();
            async move {
                let _ = seen.send(request.headers().clone());
                StatusCode::OK
            }
        });
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the local API");
        let address = listener.local_addr().expect("read the local API's address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), arrived)
    }

    /// Nothing a Peer sends removes, shadows or forges the author the
    /// listener names its Sidekick's act by: not the headers it names
    /// hop-by-hop in `Connection`, in any case, nor a second author or a
    /// proof of its own. The local Session API is handed the authenticated
    /// Peer's Sidekick, proven, or the act is refused.
    #[tokio::test]
    async fn a_peers_act_reaches_the_session_api_authored_by_that_peer_whatever_it_sends() {
        let data = tempfile::tempdir().expect("create a data directory");
        let (base_url, mut arrived) = recording_local_api().await;
        let controller = ServingController::new(
            data.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            base_url,
            "token".to_owned(),
        )
        .expect("make a Serving controller");
        let public_key = b"the peer's public key".to_vec();
        controller
            .peers
            .write()
            .expect("Peer record lock is not poisoned")
            .push(StoredPeer {
                id: "ab12cd34ef567890".to_owned(),
                public_key: public_key.clone(),
                name: "laptop".to_owned(),
            });
        let state = ServingState {
            controller: controller.clone(),
            hostname: "workstation".to_owned(),
            protocol_version: crate::protocol::PROTOCOL_VERSION,
        };
        let claimed = author_header(&Author::Sidekick {
            session_id: crate::protocol::SessionId::new(),
            title: "Plan the week".to_owned(),
        });
        let path = "/v1/sessions/0198b27e-26ec-7c4c-a83b-a83a4787453f/prompts";
        for connection in [
            "x-suru-author",
            "X-Suru-Author, x-suru-forwarded-author-proof",
            "keep-alive, X-SURU-FORWARDED-AUTHOR-PROOF",
        ] {
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("{PEER_API_PREFIX}{path}"))
                .header(PAIRING_PROTOCOL_HEADER, crate::protocol::PROTOCOL_VERSION)
                .header(header::CONNECTION, connection)
                .header(AUTHOR_HEADER, claimed.clone())
                .header("X-Suru-Author", claimed.clone())
                .header(FORWARDED_AUTHOR_PROOF_HEADER, "a proof of the Peer's own")
                .body(Body::empty())
                .expect("a Peer's request");
            let response = forward_peer_api(
                State(state.clone()),
                ConnectInfo(ServingConnectionInfo {
                    peer_key: Some(public_key.clone()),
                }),
                AxumPath(path.to_owned()),
                request,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{connection}");
            let headers = arrived
                .recv()
                .await
                .expect("the act reaches the Session API");
            assert_eq!(
                headers.get_all(AUTHOR_HEADER).iter().count(),
                1,
                "one author, the listener's: {connection}"
            );
            assert_eq!(
                controller.forwarded_author(&headers).ok().flatten(),
                Some(Author::PeerSidekick {
                    peer: "laptop".to_owned(),
                    fingerprint: "ab12cd34ef567890".to_owned(),
                    act: None,
                }),
                "the act stands as the authenticated Peer's Sidekick's, proven: {connection}"
            );
        }
    }

    #[tokio::test]
    async fn a_small_answer_is_read_no_further_than_the_budget() {
        let health = serde_json::to_vec(&PairingHealth {
            protocol_version: 7,
        })
        .expect("encode a health answer");
        assert!(
            small_answer::<PairingHealth>(answering(StatusCode::OK, health))
                .await
                .is_some_and(|health| health.protocol_version == 7)
        );
        let mut padded = b"{\"protocol_version\": 7".to_vec();
        padded.extend(std::iter::repeat_n(b' ', PAIRING_ANSWER_BUDGET));
        padded.push(b'}');
        assert!(
            small_answer::<PairingHealth>(answering(StatusCode::OK, padded))
                .await
                .is_none(),
            "an answer past the budget is not read, however well it would decode"
        );
    }

    /// A socket dialled to a direct way is set up as HTTP clients set theirs
    /// up: it sends without delay, is probed while idle, and — where the
    /// platform has it — is given up on once what it sends goes
    /// unacknowledged for long.
    #[tokio::test]
    async fn a_direct_way_is_dialled_as_http_clients_dial() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a listener to dial");
        let target = format!(
            "https://{}",
            listener.local_addr().expect("read the listener's address")
        )
        .parse::<Uri>()
        .expect("a socket address is a target");
        let socket = connected(direct_dialer(), target)
            .await
            .expect("dial the listener")
            .into_inner();
        let socket = socket2::SockRef::from(&socket);
        assert!(socket.tcp_nodelay().expect("read TCP_NODELAY"));
        assert!(socket.keepalive().expect("read SO_KEEPALIVE"));
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        assert_eq!(
            socket.tcp_user_timeout().expect("read TCP_USER_TIMEOUT"),
            Some(tokio::time::Duration::from_secs(30))
        );
    }

    /// A proxy the environment names is never sent a loopback address, which
    /// a proxy elsewhere could reach only its own of; one a test gives is.
    #[test]
    fn no_proxy_the_environment_names_is_sent_a_loopback_address() {
        let rules = Arc::new(
            Matcher::builder()
                .https("http://proxy.invalid:3128")
                .build(),
        );
        let from_environment = DirectProxies {
            rules: rules.clone(),
            loopback: false,
        };
        for loopback in [
            "127.0.0.1:7443",
            "127.8.9.10:7443",
            "[::1]:7443",
            "[::ffff:127.0.0.1]:7443",
        ] {
            let address = loopback.parse().expect("a loopback address");
            assert!(
                from_environment.for_address(address).is_none(),
                "{loopback}"
            );
        }
        for elsewhere in ["192.0.2.24:7443", "[2001:db8::24]:7443"] {
            let address = elsewhere.parse().expect("an address elsewhere");
            assert!(
                from_environment.for_address(address).is_some(),
                "{elsewhere}"
            );
        }
        let given = DirectProxies {
            rules,
            loopback: true,
        };
        assert!(
            given
                .for_address(SocketAddr::from(([127, 0, 0, 1], 7443)))
                .is_some()
        );
    }

    /// A proxy of any kind but plain `http` is refused before anything is
    /// dialled: the way fails, naming the proxy's scheme and never the
    /// credentials its URL carried.
    #[tokio::test]
    async fn a_direct_way_is_never_dialled_through_a_proxy_but_an_http_one() {
        let stand_in = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the proxy's stand-in");
        let proxy = stand_in.local_addr().expect("read the stand-in's address");
        let received = Arc::new(StdMutex::new(Vec::new()));
        let noted = received.clone();
        let standing_in = tokio::spawn(async move {
            while let Ok((mut connection, _)) = stand_in.accept().await {
                let mut read = vec![0_u8; 4096];
                let length = tokio::io::AsyncReadExt::read(&mut connection, &mut read)
                    .await
                    .unwrap_or(0);
                noted
                    .lock()
                    .expect("received bytes lock is not poisoned")
                    .push(read[..length].to_vec());
            }
        });
        let way = Way::Direct(SocketAddr::from(([127, 0, 0, 1], 9)));
        for scheme in ["https", "socks4", "socks4a", "socks5", "socks5h"] {
            let dialer = WayDialer {
                proxies: DirectProxies::given(&format!("{scheme}://suru:proxy-secret@{proxy}"), ""),
                relays: GivenRelays::default(),
                server: Arc::from(Vec::new()),
                handshake_timeout: tokio::time::Duration::from_secs(10),
                direct_head_start: tokio::time::Duration::from_millis(250),
                direct_idle_timeout: tokio::time::Duration::from_secs(90),
                direct_retry_interval: tokio::time::Duration::from_secs(5),
                keepalive: JoinedKeepalive {
                    interval: tokio::time::Duration::from_secs(15),
                    timeout: tokio::time::Duration::from_secs(30),
                },
            };
            let interest = watch::Sender::new(());
            let refusal = open_connection(&way, &dialer, Wanted(interest.subscribe()))
                .await
                .err()
                .unwrap_or_else(|| panic!("a `{scheme}` proxy is not dialled through"));
            assert_eq!(
                refusal.kind(),
                std::io::ErrorKind::Unsupported,
                "{scheme}: {refusal}"
            );
            let said = refusal.to_string();
            assert!(said.contains(&format!("`{scheme}`")), "{said}");
            assert!(
                !said.contains("proxy-secret") && !said.contains("suru:"),
                "{said}"
            );
        }
        standing_in.abort();
        assert!(
            received
                .lock()
                .expect("received bytes lock is not poisoned")
                .is_empty(),
            "nothing reached the proxy"
        );
    }

    #[tokio::test]
    async fn the_proxy_looks_no_further_into_a_conflict_than_the_budget() {
        let data = tempfile::tempdir().expect("create a data directory");
        let controller = ServingController::new(
            data.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:9".to_owned(),
            "token".to_owned(),
        )
        .expect("make a Serving controller");
        let way = Way::Direct(SocketAddr::from(([127, 0, 0, 1], 9)));
        let conflict = |body: Vec<u8>| {
            Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from(body))
                .expect("a conflict")
        };
        let mismatch = serde_json::to_vec(&SessionError {
            code: SessionErrorCode::PairingProtocolMismatch,
            message: "Pairing protocol mismatch".to_owned(),
            unreachable: None,
        })
        .expect("encode a refusal");
        let remote = StoredRemote {
            remote: Remote {
                name: "workstation".to_owned(),
                fingerprint: String::new(),
                ways: vec![way.clone()],
                status: RemoteStatus::Available,
            },
            public_key: Vec::new(),
            answered: Vec::new(),
            generation: 0,
        };
        let client = Arc::new(
            paired_http_client(
                &remote.public_key,
                &controller.identity().expect("make an identity"),
                None,
                controller.way_dialer(&remote.public_key),
            )
            .expect("make a client for the Remote"),
        );
        assert!(
            controller
                .classify_remote_response(&remote, &client, way.clone(), conflict(mismatch))
                .await
                .is_ok_and(|response| response.status() == StatusCode::CONFLICT),
            "a conflict within the budget is passed on"
        );
        let refusal = controller
            .classify_remote_response(
                &remote,
                &client,
                way,
                conflict(vec![b' '; PAIRING_ANSWER_BUDGET + 1]),
            )
            .await
            .expect_err("a conflict past the budget is not read");
        assert_eq!(
            refusal.code,
            SessionErrorCode::PairingOutcomeUnknown,
            "the Remote answered, so what it was asked may have been done"
        );
    }

    /// Redeeming an Invite while the platform credential store this
    /// Server's identity key is kept in does not answer fails, telling its
    /// user why, and nothing is made in the key's place; so does Serving.
    /// The controller knows its Server by its key all the while, and
    /// redeems the same Invite with it, and Serves, once the store answers
    /// again.
    #[tokio::test]
    async fn an_invite_redeemed_while_the_identity_store_does_not_answer_fails_visibly() {
        let (serving_data, redeeming_data) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let settings = ServingSettings {
            enabled: true,
            listener: true,
            port: 0,
            bind_address: std::net::Ipv4Addr::LOCALHOST.into(),
        };
        let serving = ServingController::new(
            serving_data.path(),
            tokio::time::Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:9".to_owned(),
            "token".to_owned(),
        )
        .unwrap();
        serving.adopt(settings).await.unwrap();
        let invite = serving
            .issue_invite(IssueInviteRequest {
                ways: vec![Way::Direct(serving.address().unwrap())],
            })
            .await
            .unwrap_or_else(|failure| panic!("{}", failure.message))
            .invite;
        let store = Arc::new(FakeIdentityStore::default());
        let redeeming = || {
            ServingController::new(
                redeeming_data.path(),
                tokio::time::Duration::from_secs(60),
                crate::protocol::PROTOCOL_VERSION,
                "http://127.0.0.1:9".to_owned(),
                "token".to_owned(),
            )
            .unwrap()
            .with_identity_keeping(IdentityKeeping {
                store: store.clone(),
                selection: Selection::ReleaseBuild,
                channel: "test".to_owned(),
                store_timeout: tokio::time::Duration::from_secs(10),
            })
        };
        let own = redeeming().own_fingerprint().unwrap();

        store.set_available(false);
        let redeeming = redeeming();
        let redeem = || {
            redeeming.redeem_invite(RedeemInviteRequest {
                invite: invite.clone(),
                name: Some("workstation".to_owned()),
                ways: Vec::new(),
            })
        };
        let failure = redeem()
            .await
            .expect_err("no Invite is redeemed without the identity key");
        assert_eq!(failure.code, SessionErrorCode::IdentityKeyUnavailable);
        assert_eq!(failure.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            failure.message.contains("platform credential store")
                && failure.message.contains("did not answer")
                && failure.message.contains("no new key was made"),
            "{}",
            failure.message
        );
        let not_serving = redeeming
            .adopt(settings)
            .await
            .expect_err("nothing Serves without the identity key");
        assert_eq!(not_serving.to_string(), failure.message);
        assert_eq!(redeeming.own_fingerprint().unwrap(), own);
        assert!(!redeeming_data.path().join("server-identity.pk8").exists());

        store.set_available(true);
        let remote = redeem()
            .await
            .unwrap_or_else(|failure| panic!("{}", failure.message));
        assert_eq!(remote.fingerprint, serving.own_fingerprint().unwrap());
        redeeming.adopt(settings).await.unwrap();
        assert!(redeeming.address().is_some(), "Serving starts");
        let peers = serving.peers.read().unwrap().clone();
        assert_eq!(
            peers
                .iter()
                .map(|peer| fingerprint(&peer.public_key))
                .collect::<Vec<_>>(),
            [own],
            "the Serving Server pins the key the redeeming one kept all along"
        );

        serving.shutdown().await;
        redeeming.shutdown().await;
    }
}
