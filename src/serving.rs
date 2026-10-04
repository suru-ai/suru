//! Pairing formation and the opt-in Server-to-Server listener.
//!
//! Local callers use the small [`ServingController`] interface. Invite
//! encoding, durable identity and Pairing records, ordered dialing, and both
//! sides of the pinned-key TLS transport remain private to this module. The
//! Serving side's acceptor takes the connections dialled to its listener and
//! those a Relay carries to it alike, and the redeeming side runs the same
//! pinned-key TLS over a connection from either kind of way: dialled at an
//! address, or joined to its Serving Server at a Relay (ADR-0045). Over a
//! direct way each request under way has a connection of its own, speaking
//! HTTP/1.1; over a Relay way everything asked travels together on one joined
//! stream, speaking HTTP/2 as the two Servers agree inside that TLS, so a
//! Remote in view costs its Relay one join.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
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
use hyper::{body::Incoming, client::conn::http2};
use hyper_util::{
    client::{
        legacy::{
            Client as HttpClient, Error as HttpClientError,
            connect::{Connected, Connection, HttpConnector, proxy::Tunnel},
        },
        proxy::matcher::{Intercept, Matcher},
    },
    rt::{TokioExecutor, TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use rcgen::{
    CertificateParams, DistinguishedName as CertificateDistinguishedName, DnType, KeyPair,
    PublicKeyData,
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
        Peer, RedeemInviteRequest, Remote, RemoteHealth, RemoteRemoval, RemoteStatus,
        ServingSettings, SessionError, SessionErrorCode, Way,
    },
    runtime::protect_current_user_file,
};

const IDENTITY_FILE: &str = "server-identity.pk8";
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
/// What a Relay way's connection speaks inside the pinned-key TLS, agreed as
/// its handshake is: HTTP/2, so everything asked by that way travels together
/// and a Remote in view costs its Relay one join however much is open to it.
/// A direct way's connection agrees nothing, and speaks HTTP/1.1 as it always
/// has.
const MULTIPLEXED: &[u8] = b"h2";
/// How many requests and streams a joined stream carries at once; more wait
/// their turn, each held meanwhile by whatever asked it of this Server.
const JOINED_STREAMS_AT_ONCE: u32 = 100;
/// How much of each request's or stream's body a Server lets the other send
/// it ahead of what reads it.
const JOINED_STREAM_WINDOW: u32 = 256 * 1024;
/// How much of all of them together: room for every stream at once, so those
/// whose readers have stalled never hold back one that is read. The windows
/// bound only what the other Server may send ahead of this one's readers —
/// at most this much, 25 MiB, on a joined stream. Beside it are what a
/// Server holds to send, up to [`JOINED_STREAM_WINDOW`] for each stream;
/// each request's and answer's headers, up to the 16 KiB HTTP/2 here takes
/// of a header list; the requests waiting their turn past
/// [`JOINED_STREAMS_AT_ONCE`], each with its body, which nothing here
/// bounds; and what the TLS and the Relay's WebSocket buffer.
const JOINED_CONNECTION_WINDOW: u32 = JOINED_STREAMS_AT_ONCE * JOINED_STREAM_WINDOW;
/// How each Server on a joined stream makes sure, inside the pinned-key TLS,
/// that the other still answers — and so that the Relay between them still
/// carries what either says: once it has taken nothing in for `interval`,
/// idle or not, it asks with an HTTP/2 PING, and gives the joined stream up
/// where no answer comes within `timeout`. A reader that pauses holds back
/// its own stream alone, and the connection goes on being read, so only the
/// other Server or the Relay falling silent leaves a PING unanswered; the
/// answer may come back behind whatever is already on its way, which the
/// stream windows bound, so `timeout` allows for that over a slow link.
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
    address: watch::Sender<Option<SocketAddr>>,
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
    /// How this Server makes sure the other on a joined stream still answers,
    /// on either side of it.
    joined_keepalive: JoinedKeepalive,
    invites: Arc<StdMutex<InviteLedger>>,
    protocol_version: u32,
    identity: IdentityKey,
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    remotes: Arc<RwLock<Vec<StoredRemote>>>,
    remote_clients: Arc<StdMutex<HashMap<String, Weak<PairingHttpClient>>>>,
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

    /// A connection wanted by nothing any longer.
    #[cfg(test)]
    pub(crate) fn gone() -> Self {
        Self(watch::Sender::new(()).subscribe())
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

/// This Server's identity key: what its Pairings pin, and what it proves
/// itself to a Relay by. It is read from the data directory — or made there,
/// the first time — at its first use, and held from then on. Its private key
/// never leaves this module.
#[derive(Clone)]
pub(crate) struct IdentityKey {
    data_dir: PathBuf,
    material: Arc<StdMutex<Option<IdentityMaterial>>>,
}

impl IdentityKey {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            material: Arc::default(),
        }
    }

    /// The key, as the DER SubjectPublicKeyInfo its Pairings pin.
    pub(crate) fn public_key(&self) -> Result<Vec<u8>> {
        Ok(self.material()?.public_key)
    }

    /// Signs `message` with the key, as the Server proves it to a Relay.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let identity = self.material()?;
        let signing_key = KeyPair::try_from(identity.private_key.as_slice())
            .context("read Server identity key")?;
        rcgen::SigningKey::sign(&signing_key, message).context("sign with Server identity key")
    }

    fn material(&self) -> Result<IdentityMaterial> {
        let mut identity = self
            .material
            .lock()
            .expect("Server identity lock is not poisoned");
        if let Some(identity) = identity.as_ref() {
            return Ok(identity.clone());
        }
        let private_key = load_or_generate_identity(&self.data_dir)?;
        let signing_key =
            KeyPair::try_from(private_key.as_slice()).context("read Server identity key")?;
        let certificate = CertificateParams::new(vec![SERVING_IDENTITY_NAME.to_owned()])
            .context("describe Server identity certificate")?
            .self_signed(&signing_key)
            .context("mint Server identity certificate")?;
        let material = IdentityMaterial {
            public_key: signing_key.subject_public_key_info(),
            private_key,
            certificate: certificate.der().as_ref().to_vec(),
        };
        *identity = Some(material.clone());
        Ok(material)
    }
}

#[derive(Clone)]
struct LocalApi {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

struct ActiveServing {
    settings: ServingSettings,
    address: SocketAddr,
    task: JoinHandle<()>,
    connections: Arc<RevocableConnections>,
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

#[derive(Clone)]
struct IdentityMaterial {
    private_key: Vec<u8>,
    certificate: Vec<u8>,
    public_key: Vec<u8>,
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
    /// The way that last answered, tried first on the next dial.
    #[serde(default)]
    last_answered: Option<Way>,
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
}

impl PairingFailure {
    fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
    /// The Remote's ways in the order to dial them: the one that last
    /// answered, then the rest as the Invite's redeemer ordered them.
    fn ways_by_recency(&self) -> Vec<Way> {
        self.last_answered
            .iter()
            .chain(
                self.remote
                    .ways
                    .iter()
                    .filter(|way| Some(*way) != self.last_answered.as_ref()),
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
        let (address, _) = watch::channel(None);
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
            address,
            serving: watch::channel(ServingStretch::default()).0,
            carried: Arc::default(),
            pairing_changes: Arc::new(watch::channel(0).0),
            pairings_made: Arc::default(),
            invite_ttl,
            withdrawal_timeout: crate::server::ServerTimings::default().remote_withdrawal_timeout,
            handshake_timeout: crate::server::ServerTimings::default().serving_handshake_timeout,
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
            revocations: Arc::new(RwLock::new(revocations)),
            awaiting_revocation: Arc::default(),
            forwarded_author_proof: URL_SAFE_NO_PAD.encode(new_token()).into(),
            direct_proxies: DirectProxies::from_environment(),
            relays: GivenRelays::default(),
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

    /// Sets how this Server makes sure the other on a joined stream still
    /// answers.
    pub(crate) fn with_joined_keepalive(mut self, keepalive: JoinedKeepalive) -> Self {
        self.joined_keepalive = keepalive;
        self
    }

    pub(crate) fn address(&self) -> Option<SocketAddr> {
        *self.address.borrow()
    }

    pub(crate) async fn issue_invite(
        &self,
        request: IssueInviteRequest,
    ) -> std::result::Result<IssuedInvite, PairingFailure> {
        if self.address().is_none() {
            return Err(PairingFailure::new(
                SessionErrorCode::ServingListenerFailed,
                "Serving is disabled",
            ));
        }
        let ways = request
            .ways
            .iter()
            .map(|way| self.offered(way))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !ways_are_unique_and_nonempty(&ways) {
            return Err(PairingFailure::new(
                SessionErrorCode::InvalidInviteWays,
                "an Invite needs at least one unique address",
            ));
        }

        let identity = self.identity().map_err(internal_pairing_failure)?;
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

    /// `way` as an Invite offers it: a direct way as it is, and a Relay way
    /// by its Relay's address written the one way, where this Server Serves
    /// through that Relay and its Login there is not known to need renewing,
    /// so it waits there to be reached while it is Serving.
    fn offered(&self, way: &Way) -> std::result::Result<Way, PairingFailure> {
        match way {
            Way::Direct(_) => Ok(way.clone()),
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
        let identity = self.identity().map_err(internal_pairing_failure)?;
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

    pub(crate) async fn probe_remote(
        &self,
        name: &str,
    ) -> std::result::Result<RemoteHealth, PairingFailure> {
        let remote = self.stored_remote(name)?;
        let health = match self.probe_remote_connection(&remote).await {
            Ok(connection) => {
                self.record_remote_connection(name, connection.health.status, connection.way);
                return Ok(connection.health);
            }
            Err(error) if error.code == SessionErrorCode::PairingAuthenticationFailed => {
                Ok(RemoteHealth {
                    protocol_version: None,
                    status: RemoteStatus::Revoked,
                })
            }
            Err(error) if error.code == SessionErrorCode::PairingConnectionFailed => {
                Ok(RemoteHealth {
                    protocol_version: None,
                    status: RemoteStatus::Unavailable,
                })
            }
            Err(error) => Err(error),
        }?;
        self.record_remote_status(name, health.status);
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
        let body = axum::body::to_bytes(body, usize::MAX).await.map_err(|_| {
            PairingFailure::new(
                SessionErrorCode::PairingConnectionFailed,
                "Remote API request body could not be read",
            )
        })?;
        let client = self.pairing_client(&remote)?;
        let attempt_client = client.clone();
        // A request that may have reached the Remote is never asked again by
        // another way unless asking twice changes nothing: only one that was
        // never delivered is.
        let repeatable = parts.method.is_safe();
        let response = first_remote_answer(&remote, &client, move |way| {
            let client = attempt_client.clone();
            let mut request = Request::new(Body::from(body.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.headers_mut() = parts.headers.clone();
            let added = added.clone();
            async move {
                match forward_to_remote(client, &way, request, added).await {
                    Ok(response) => WayAttempt::Answered(response),
                    Err(failure)
                        if failure.code == SessionErrorCode::PairingOutcomeUnknown
                            && !repeatable =>
                    {
                        WayAttempt::Rejected(failure)
                    }
                    Err(_) => WayAttempt::TryNext,
                }
            }
        })
        .await;
        match response {
            Ok((way, response)) => self.classify_remote_response(name, way, response).await,
            Err(error) => {
                if error.code == SessionErrorCode::PairingAuthenticationFailed {
                    self.record_remote_status(name, RemoteStatus::Revoked);
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
        let attempt_client = client.clone();
        let withdrawal = first_remote_answer(remote, &client, move |way| {
            let client = attempt_client.clone();
            async move {
                let withdrawal = Request::post(PAIRING_WITHDRAWAL_PATH)
                    .body(Body::empty())
                    .expect("a withdrawal is well formed");
                match client.send(&way, withdrawal).await {
                    Ok(response) if response.status().is_success() => WayAttempt::Answered(()),
                    Ok(_) | Err(_) => WayAttempt::TryNext,
                }
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

    /// Reconciles the second listener to the effective Settings before an
    /// adoption answers. A changed address replaces only this listener; an
    /// unchanged configuration is left alone.
    pub(crate) async fn adopt(&self, settings: ServingSettings) -> Result<()> {
        let mut active = self.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|running| running.settings == settings)
        {
            return Ok(());
        }
        if !settings.enabled {
            self.stop_carrying();
            self.discard_invites();
            stop_active(&mut active, &self.address).await;
            return Ok(());
        }

        let requested = SocketAddr::new(settings.bind_address, settings.port);
        if let Some(running) = active.as_mut()
            && requested == running.address
        {
            running.settings = settings;
            return Ok(());
        }
        let listener = bind_listener(requested)
            .with_context(|| format!("bind Serving listener to {requested}"))?;
        let address = listener
            .local_addr()
            .context("read bound Serving address")?;
        let tls = self.serving_tls()?;
        self.discard_invites();
        stop_active(&mut active, &self.address).await;
        let connections = Arc::new(RevocableConnections::default());
        let (carried, carried_arrivals) = mpsc::channel(CARRIED_ARRIVALS_QUEUED);
        let task = tokio::spawn(serve(
            Box::pin(stream::select(
                dialled_to(listener),
                carried_to(carried_arrivals),
            )),
            tls,
            connections.clone(),
            self.clone(),
        ));
        *active = Some(ActiveServing {
            settings,
            address,
            task,
            connections,
        });
        // Serving that starts again after it stopped is another stretch of
        // it; a listener that moves is the same stretch.
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
        self.address.send_replace(Some(address));
        self.serving
            .send_if_modified(|serving| std::mem::replace(serving, stretch) != stretch);
        tracing::info!(%address, "Serving listener ready");
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let mut active = self.active.lock().await;
        self.stop_carrying();
        stop_active(&mut active, &self.address).await;
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
        Ok(fingerprint(&self.identity()?.public_key))
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

    fn record_remote_status(&self, name: &str, status: RemoteStatus) {
        self.record_remote_state(name, status, None);
    }

    fn record_remote_connection(&self, name: &str, status: RemoteStatus, way: Way) {
        self.record_remote_state(name, status, Some(way));
    }

    fn record_remote_state(&self, name: &str, status: RemoteStatus, last_answered: Option<Way>) {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        let Some(index) = remotes.iter().position(|stored| stored.remote.name == name) else {
            return;
        };
        let previous_status = remotes[index].remote.status;
        if previous_status == status
            && last_answered
                .as_ref()
                .is_none_or(|way| remotes[index].last_answered.as_ref() == Some(way))
        {
            return;
        }
        let previous_answered = remotes[index].last_answered.clone();
        remotes[index].remote.status = status;
        if let Some(way) = last_answered {
            remotes[index].last_answered = Some(way);
        }
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes[index].remote.status = previous_status;
            remotes[index].last_answered = previous_answered;
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
    ) -> std::result::Result<RemoteConnection, PairingFailure> {
        let client = self.pairing_client(remote)?;
        let attempt_client = client.clone();
        let (way, pairing_health) = first_remote_answer(remote, &client, move |way| {
            let client = attempt_client.clone();
            async move {
                let health = Request::get("/health")
                    .body(Body::empty())
                    .expect("a health check is well formed");
                let response = client.send(&way, health).await;
                match response {
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
        };
        Ok(RemoteConnection { way, health })
    }

    fn pairing_client(
        &self,
        remote: &StoredRemote,
    ) -> std::result::Result<Arc<PairingHttpClient>, PairingFailure> {
        let mut clients = self
            .remote_clients
            .lock()
            .expect("Remote client lock is not poisoned");
        if let Some(client) = clients.get(&remote.remote.name).and_then(Weak::upgrade) {
            return Ok(client);
        }
        let identity = self.identity().map_err(internal_pairing_failure)?;
        let client = Arc::new(
            paired_http_client(
                &remote.public_key,
                &identity,
                None,
                self.way_dialer(&remote.public_key),
            )
            .map_err(internal_pairing_failure)?,
        );
        clients.insert(remote.remote.name.clone(), Arc::downgrade(&client));
        Ok(client)
    }

    async fn classify_remote_response(
        &self,
        name: &str,
        way: Way,
        response: Response,
    ) -> std::result::Result<Response, PairingFailure> {
        if response.status() == StatusCode::UNAUTHORIZED {
            self.record_remote_connection(name, RemoteStatus::Revoked, way);
            return Err(PairingFailure::new(
                SessionErrorCode::PairingAuthenticationFailed,
                "Remote refused this Server's key",
            ));
        }
        if response.status() != StatusCode::CONFLICT {
            self.record_remote_connection(name, RemoteStatus::Available, way);
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
        self.record_remote_connection(name, status, way);
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
            last_answered: None,
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

async fn stop_active(
    active: &mut Option<ActiveServing>,
    address: &watch::Sender<Option<SocketAddr>>,
) {
    address.send_replace(None);
    if let Some(running) = active.take() {
        running.task.abort();
        let _ = running.task.await;
        running.connections.revoke_all();
        tracing::info!("Serving listener stopped");
    }
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
    let service = TowerToHyperService::new(app.layer(axum::Extension(ConnectInfo(connection))));
    // However the connection ends — closed, revoked, failing, or given up —
    // there is nothing more to do with it.
    if multiplexed {
        let (transport, liveness) = Liveness::watch(stream, Preface::of_client());
        let mut server = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
        server
            .timer(TokioTimer::new())
            .keep_alive_interval(keepalive.interval)
            .keep_alive_timeout(keepalive.timeout)
            .max_concurrent_streams(JOINED_STREAMS_AT_ONCE)
            .initial_stream_window_size(JOINED_STREAM_WINDOW)
            .initial_connection_window_size(JOINED_CONNECTION_WINDOW)
            .max_send_buf_size(JOINED_STREAM_WINDOW as usize);
        tokio::select! {
            _ = server.serve_connection(TokioIo::new(transport), service) => {}
            () = liveness.lost(startup) => {}
        }
    } else {
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await;
    }
}

/// What a joined stream's transport shows of the Server at its far end, as
/// the Server on either side watches it, and the transport itself, which is
/// given up — dropped there and then, whatever it was still sending — once
/// that Server is judged gone. HTTP/2's keepalive judges the stream only
/// once it is under way, so until the other Server's HTTP/2 preface has come
/// the stream is given up where that has not come within the startup time
/// it is given.
struct Liveness<S> {
    transport: StdMutex<Option<S>>,
    /// Wakes whatever last used the transport, so it finds it given up.
    waker: AtomicWaker,
    /// Whether the other Server's HTTP/2 preface has all come.
    begun: watch::Sender<bool>,
}

impl<S> Liveness<S> {
    /// `transport` as HTTP/2 is run over it, the other Server's `preface`
    /// expected first, and what watches it.
    fn watch(transport: S, preface: Preface) -> (Watched<S>, Arc<Self>) {
        let liveness = Arc::new(Self {
            transport: StdMutex::new(Some(transport)),
            waker: AtomicWaker::new(),
            begun: watch::Sender::new(false),
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
    /// up: never, while it stands.
    async fn lost(&self, startup: tokio::time::Duration) {
        if self.begun_within(startup).await.is_ok() {
            std::future::pending::<()>().await;
        }
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

impl<S: Unpin> Watched<S> {
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
        if !this.preface.done() && this.preface.take_in(&buffer.filled()[before..]) {
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
        self.poll_transport(context, |transport, context| {
            transport.poll_write(context, buffer)
        })
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_transport(context, |transport, context| {
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
        self.poll_transport(context, |transport, context| transport.poll_flush(context))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_transport(context, |transport, context| {
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
    let Some(peer_protocol_version) = request
        .headers()
        .get(PAIRING_PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return PairingFailure::new(
            SessionErrorCode::PairingProtocolMismatch,
            "Peer did not state a valid Pairing protocol version",
        )
        .response();
    };
    if peer_protocol_version != state.protocol_version {
        return protocol_mismatch(state.protocol_version, peer_protocol_version).response();
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
    if crate::broker::is_broker_path(path) {
        PeerRouteClass::LoopbackOnly
    } else if settings_mutation || stop || pairing_management || relay_management {
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

/// Carries `request` on to the Remote reached by `way` through `client`,
/// with `added` set after all it carries. The answer holds `client` as its
/// interest lease until its body ends.
async fn forward_to_remote(
    client: Arc<PairingHttpClient>,
    way: &Way,
    request: Request<Body>,
    added: HeaderMap,
) -> std::result::Result<Response, PairingFailure> {
    let response = client
        .send(way, passed_on(request, added))
        .await
        .map_err(|error| undelivered_or_lost(error.never_delivered()))?;
    let (parts, body) = response.into_parts();
    Ok(passed_back(
        parts.status,
        parts.headers,
        body.into_data_stream(),
        Some(client),
    ))
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

/// A connection come to the Serving side to be accepted, before its TLS
/// handshake.
struct Arrival {
    stream: Box<dyn ByteStream>,
    from: ArrivedFrom,
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
    handshakes: tokio::task::JoinSet<(Handshake, ArrivedFrom)>,
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
    /// arrive meanwhile while there is room, and where it came from.
    async fn handshaken(&mut self) -> (TlsStream<Box<dyn ByteStream>>, ArrivedFrom) {
        loop {
            let room = self.handshakes.len() < SERVING_HANDSHAKES_AT_ONCE;
            tokio::select! {
                Some(arrival) = self.arrivals.next(), if room => {
                    let acceptor = self.tls.over(arrival.from).clone();
                    let handshake_timeout = self.handshake_timeout;
                    self.handshakes.spawn(async move {
                        (
                            tokio::time::timeout(
                                handshake_timeout,
                                acceptor.accept(arrival.stream),
                            )
                            .await,
                            arrival.from,
                        )
                    });
                }
                Some(finished) = self.handshakes.join_next() => match finished {
                    Ok((Ok(Ok(stream)), from)) => return (stream, from),
                    Ok((_, from)) => {
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
    /// its Peer's revocation, and what it is known by.
    async fn accept(&mut self) -> (RevocableTlsStream, ServingConnectionInfo) {
        let (stream, from) = self.handshaken().await;
        let connection_revocation = self.connections.register();
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
    /// Revoked as every connection is when Serving stops, and with its
    /// Peer's revocation, which it answers to from the moment there is one.
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
    /// The revocations revoked with this one: a Peer's revokes each of its
    /// connections'.
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

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.waker.wake();
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
/// `enrollment` by each of `ways` in turn, as `dialer` dials them, until one
/// answers. Where none does, a way that presented another key is said first,
/// and then the first Relay way refused for a reason the user can act on.
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
    let mut refused = None;
    for way in ways {
        let request = Request::post("/v1/pairing/enroll")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(enrollment.clone()))
            .expect("an enrollment request is well formed");
        match client.send(way, request).await {
            Ok(response) if response.status().is_success() => {
                return small_answer(response).await.ok_or_else(|| {
                    PairingFailure::new(
                        SessionErrorCode::PairingConnectionFailed,
                        "Serving Server returned an invalid enrollment response",
                    )
                });
            }
            Ok(response) => return Err(decode_pairing_response(response).await),
            Err(error) => {
                if refused.is_none() {
                    refused = relay_refusal(&error).cloned();
                }
            }
        }
    }
    if client.server_key_rejections.load(Ordering::Acquire) > 0 {
        return Err(PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "offered address presented a key other than the Invite's pinned key",
        ));
    }
    if let Some(refusal) = refused {
        return Err(PairingFailure::new(refusal.code, refusal.message));
    }
    Err(PairingFailure::new(
        SessionErrorCode::PairingConnectionFailed,
        "could not reach an offered address with the Invite's pinned key",
    ))
}

/// The pinned-key client a Serving Server is asked through. Over each way it
/// is asked by, it obtains connections from that way and runs the pinned-key
/// TLS over each before asking anything: over a direct way, a connection for
/// each request under way, as HTTP/1.1; over a Relay way, one joined stream
/// for them all.
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
    /// it — among them those its HTTP clients go on making in the background
    /// for a request since answered over another connection — so none is
    /// made, and no join asked at a Relay, once nothing is asked of the
    /// Serving Server. What makes a connection listens for it without
    /// holding the client.
    interest: watch::Sender<()>,
}

/// How a Serving Server is asked by one of its ways.
#[derive(Clone)]
enum OverWay {
    /// Over a direct way, by an HTTP client keeping the connection it last
    /// opened for the next request.
    Direct(Arc<HttpClient<WayConnector, Body>>),
    /// Over a Relay way, on one joined stream.
    Joined(Arc<JoinedStream>),
}

impl PairingHttpClient {
    /// Asks the Serving Server reached by `way` `request`, whose target is a
    /// path on that Server.
    async fn send(
        &self,
        way: &Way,
        mut request: Request<Body>,
    ) -> std::result::Result<hyper::Response<Incoming>, Unanswered> {
        let path_and_query = request
            .uri()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| PathAndQuery::from_static("/"));
        *request.uri_mut() = Uri::builder()
            .scheme("https")
            .authority(SERVING_IDENTITY_NAME)
            .path_and_query(path_and_query)
            .build()
            .expect("a path on the Serving Server is a target");
        let over_way = self
            .over_ways
            .lock()
            .expect("Pairing client lock is not poisoned")
            .entry(way.clone())
            .or_insert_with(|| match way {
                Way::Direct(_) => OverWay::Direct(Arc::new(
                    HttpClient::builder(TokioExecutor::new())
                        // A proxied response holds the shared client as its
                        // interest lease, so the pool and connections
                        // disappear when the Remote's last response or SSE
                        // stream ends.
                        .pool_max_idle_per_host(1)
                        .pool_timer(TokioTimer::new())
                        .build(self.connector(way, &self.tls)),
                )),
                Way::Relay(_) => OverWay::Joined(Arc::new(JoinedStream {
                    connector: self.connector(way, &self.joined_tls),
                    current: StdMutex::default(),
                })),
            })
            .clone();
        match over_way {
            OverWay::Direct(http) => http.request(request).await.map_err(|error| Unanswered {
                delivered: !error.is_connect(),
                cause: UnansweredCause::Direct(error),
            }),
            OverWay::Joined(joined) => joined.send(request).await,
        }
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

/// Why something asked of a Serving Server got no answer: whether it may
/// have been delivered, and what went wrong.
#[derive(Debug)]
struct Unanswered {
    delivered: bool,
    cause: UnansweredCause,
}

#[derive(Debug)]
enum UnansweredCause {
    /// Asking by a direct way failed.
    Direct(HttpClientError),
    /// The joined stream asked over could not be made.
    Joining(Arc<std::io::Error>),
    /// The joined stream asked over failed.
    Joined(hyper::Error),
}

impl Unanswered {
    /// Whether what was asked never reached the Serving Server.
    fn never_delivered(&self) -> bool {
        !self.delivered
    }
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            UnansweredCause::Direct(error) => error.fmt(formatter),
            UnansweredCause::Joining(error) => error.fmt(formatter),
            UnansweredCause::Joined(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Unanswered {
    /// What went wrong — for a joined stream that could not be made, the I/O
    /// error itself, so a [`RelayRefusal`] it carries is found.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match &self.cause {
            UnansweredCause::Direct(error) => error,
            UnansweredCause::Joining(error) => error.as_ref(),
            UnansweredCause::Joined(error) => error,
        })
    }
}

/// The one HTTP/2 connection to a Serving Server over a Relay way, which
/// everything asked by that way travels over together, so a Remote in view
/// costs its Relay one join however many requests and streams are open to
/// it. Whatever is asked while the join is being made waits on that join,
/// sharing whatever becomes of it, and a join is asked again only once the
/// connection it carried has ended.
struct JoinedStream {
    connector: WayConnector,
    /// The connection made, or being made, for whatever is asked next.
    current: StdMutex<Option<JoinedConnection>>,
}

/// A joined stream's connection as it comes to be made or not, shared by
/// everything waiting on it.
type JoinedConnection =
    Shared<BoxFuture<'static, std::result::Result<http2::SendRequest<Body>, Arc<std::io::Error>>>>;

impl JoinedStream {
    async fn send(
        &self,
        request: Request<Body>,
    ) -> std::result::Result<hyper::Response<Incoming>, Unanswered> {
        let mut connection = self.connection().await.map_err(|error| Unanswered {
            delivered: false,
            cause: UnansweredCause::Joining(error),
        })?;
        connection
            .try_send_request(request)
            .await
            .map_err(|mut error| Unanswered {
                delivered: error.take_message().is_none(),
                cause: UnansweredCause::Joined(error.into_error()),
            })
    }

    /// The connection the next request goes over: the one made, while it
    /// stands, or the one being made; and otherwise one made afresh.
    fn connection(&self) -> JoinedConnection {
        let mut current = self
            .current
            .lock()
            .expect("joined stream lock is not poisoned");
        if let Some(connection) = current.as_ref().filter(|connection| {
            connection.peek().is_none_or(|made| {
                made.as_ref()
                    .is_ok_and(|connection| !connection.is_closed())
            })
        }) {
            return connection.clone();
        }
        // Made on its own, so it goes on being made for whatever is asked
        // next though whatever first asked has gone; given up, and asking
        // nothing more of the Relay, once nothing is asked of the Serving
        // Server at all.
        let making = tokio::spawn(join_stream(self.connector.clone()));
        let connection = async move {
            making
                .await
                .unwrap_or_else(|error| Err(Arc::new(std::io::Error::other(error))))
        }
        .boxed()
        .shared();
        *current = Some(connection.clone());
        connection
    }
}

/// Makes the connection of a joined stream as `connector` connects — joined
/// at its Relay, with the pinned-key TLS run over the join — and has it carry
/// HTTP/2, as the Serving Server agreed in that handshake.
async fn join_stream(
    mut connector: WayConnector,
) -> std::result::Result<http2::SendRequest<Body>, Arc<std::io::Error>> {
    let mut interest = connector.interest.clone();
    let joining = async {
        let paired =
            tower_service::Service::call(&mut connector, Uri::from_static("https://suru-server/"))
                .await?
                .into_inner();
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
        // what it carries has ended, or as it fails.
        tokio::spawn(connection);
        // It stands once the Serving Server has begun HTTP/2 over it too.
        liveness.begun_within(startup).await?;
        unless_standing.keep();
        Ok(sender)
    };
    tokio::select! {
        biased;
        () = async { while interest.changed().await.is_ok() {} } => Err(no_longer_wanted()),
        joined = joining => joined,
    }
    .map_err(Arc::new)
}

/// How the HTTP client over one way connects: by a connection obtained from
/// the way, with the pinned-key TLS run over it.
#[derive(Clone)]
struct WayConnector {
    way: Way,
    tls: TlsConnector,
    dialer: WayDialer,
    /// Ends once the client the connection is made for has gone.
    interest: watch::Receiver<()>,
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
    /// How a joined stream over a Relay way makes sure the Serving Server
    /// still answers.
    keepalive: JoinedKeepalive,
}

impl tower_service::Service<Uri> for WayConnector {
    type Response = TokioIo<PairedConnection>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _context: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _target: Uri) -> Self::Future {
        let (way, tls, dialer) = (self.way.clone(), self.tls.clone(), self.dialer.clone());
        let mut interest = self.interest.clone();
        let wanted = Wanted(self.interest.clone());
        Box::pin(async move {
            let connecting = async {
                let connection = open_connection(&way, &dialer, wanted).await?;
                let server = ServerName::try_from(SERVING_IDENTITY_NAME)
                    .expect("the Serving identity's name is a TLS server name");
                let paired =
                    tokio::time::timeout(dialer.handshake_timeout, tls.connect(server, connection))
                        .await
                        .map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "the Pairing's TLS handshake did not finish in time",
                            )
                        })??;
                Ok(TokioIo::new(PairedConnection(paired)))
            };
            // Letting go is heard first, so a connection whose interest has
            // gone makes no more progress, however much is ready to be made.
            tokio::select! {
                biased;
                () = async { while interest.changed().await.is_ok() {} } => Err(no_longer_wanted()),
                connected = connecting => connected,
            }
        })
    }
}

/// A connection to a Serving Server over which the pinned-key TLS has been
/// established.
struct PairedConnection(tokio_rustls::client::TlsStream<Box<dyn ByteStream>>);

impl Connection for PairedConnection {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

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

/// How asking a Remote something by one of its ways went.
enum WayAttempt<T> {
    Answered(T),
    TryNext,
    Rejected(PairingFailure),
}

/// Asks `remote` by each of its ways in turn, the one that last answered
/// first, until one answers or refuses, answering with that way.
async fn first_remote_answer<T, F, Fut>(
    remote: &StoredRemote,
    client: &PairingHttpClient,
    mut attempt: F,
) -> std::result::Result<(Way, T), PairingFailure>
where
    F: FnMut(Way) -> Fut,
    Fut: Future<Output = WayAttempt<T>>,
{
    let rejected_before = client.server_key_rejections.load(Ordering::Acquire);
    for way in remote.ways_by_recency() {
        match attempt(way.clone()).await {
            WayAttempt::Answered(response) => return Ok((way, response)),
            WayAttempt::TryNext => {}
            WayAttempt::Rejected(error) => return Err(error),
        }
    }
    if client.server_key_rejections.load(Ordering::Acquire) != rejected_before {
        return Err(PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "Remote presented a key other than its pinned key",
        ));
    }
    Err(PairingFailure::new(
        SessionErrorCode::PairingConnectionFailed,
        "could not reach Remote at any paired address",
    ))
}

struct RemoteConnection {
    way: Way,
    health: RemoteHealth,
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

async fn decode_pairing_response(response: hyper::Response<Incoming>) -> PairingFailure {
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
        Way::Relay(relay) => {
            suru_relay_protocol::canonical_address(relay).as_deref() == Some(relay.as_str())
        }
    }
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

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open Pairing records {path:?}"))?;
    serde_json::to_writer(&mut file, value).context("encode Pairing records")?;
    file.write_all(b"\n").context("finish Pairing records")?;
    file.sync_all().context("flush Pairing records")?;
    protect_current_user_file(path)?;
    Ok(())
}

fn load_or_generate_identity(data_dir: &Path) -> Result<Vec<u8>> {
    let path = data_dir.join(IDENTITY_FILE);
    match fs::read(&path) {
        Ok(identity) => {
            protect_current_user_file(&path)?;
            return Ok(identity);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read Server identity {path:?}")),
    }

    let identity = KeyPair::generate()
        .context("generate Server identity")?
        .serialize_der();
    let mut temporary = tempfile::Builder::new()
        .prefix(".server-identity-")
        .suffix(".tmp")
        .tempfile_in(data_dir)
        .with_context(|| format!("create temporary Server identity in {data_dir:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("protect temporary Server identity")?;
    }
    temporary
        .as_file_mut()
        .write_all(&identity)
        .context("write Server identity")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("flush Server identity")?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish Server identity {path:?}"))?;
    protect_current_user_file(&path)?;
    Ok(identity)
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
            keepalive: JoinedKeepalive {
                interval: tokio::time::Duration::from_secs(15),
                timeout: tokio::time::Duration::from_secs(30),
            },
        };
        let client = paired_http_client(&identity.public_key, &identity, None, dialer).unwrap();
        let interest = client.interest.subscribe();
        let way = Way::Relay("http://relay.invalid".to_owned());
        let mut connector = WayConnector {
            way,
            tls: client.tls.clone(),
            dialer: client.dialer.clone(),
            interest,
        };
        drop(client);

        for _ in 0..64 {
            let connected = tower_service::Service::call(
                &mut connector,
                Uri::from_static("https://suru-server/"),
            )
            .await;
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
        })
        .expect("encode a refusal");
        assert!(
            controller
                .classify_remote_response("workstation", way.clone(), conflict(mismatch))
                .await
                .is_ok_and(|response| response.status() == StatusCode::CONFLICT),
            "a conflict within the budget is passed on"
        );
        let refusal = controller
            .classify_remote_response(
                "workstation",
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
}
