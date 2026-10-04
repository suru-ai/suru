//! Pairing formation and the opt-in Server-to-Server listener.
//!
//! Local callers use the small [`ServingController`] interface. Invite
//! encoding, durable identity and Pairing records, ordered dialing, and both
//! sides of the pinned-key TLS transport remain private to this module.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    serve::{IncomingStream, Listener},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{StreamExt, stream, task::AtomicWaker};
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
    server::{WebPkiClientVerifier, danger::ClientCertVerified},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, watch},
    task::JoinHandle,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use uuid::Uuid;

use crate::{
    protocol::{
        ACT_HEADER, AUTHOR_HEADER, ActId, Author, InvitePreview, IssueInviteRequest, IssuedInvite,
        Peer, RedeemInviteRequest, Remote, RemoteHealth, RemoteRemoval, RemoteStatus,
        ServingSettings, SessionError, SessionErrorCode,
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
/// Serving listener at once; past it, no more are accepted until one
/// finishes.
const SERVING_HANDSHAKES_AT_ONCE: usize = 64;

#[derive(Clone)]
pub(crate) struct ServingController {
    data_dir: PathBuf,
    local_api: LocalApi,
    active: Arc<Mutex<Option<ActiveServing>>>,
    address: watch::Sender<Option<SocketAddr>>,
    /// Moves on with every change to the Remotes this Server is paired with —
    /// one paired, removed, or rolled back — so what follows a Remote under
    /// one Pairing hears at once that it may no longer stand.
    pairing_changes: Arc<watch::Sender<u64>>,
    /// How many Pairings were made since this Server started: the last one
    /// made's generation.
    pairings_made: Arc<AtomicU64>,
    invite_ttl: tokio::time::Duration,
    withdrawal_timeout: tokio::time::Duration,
    /// How long a connection to the Serving listener may take to finish its
    /// TLS handshake before it is dropped.
    handshake_timeout: tokio::time::Duration,
    invites: Arc<StdMutex<InviteLedger>>,
    protocol_version: u32,
    identity: Arc<StdMutex<Option<IdentityMaterial>>>,
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    remotes: Arc<RwLock<Vec<StoredRemote>>>,
    remote_clients: Arc<StdMutex<HashMap<String, Weak<PairingHttpClient>>>>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    /// What the Serving listener proves it named an act's author with; see
    /// [`FORWARDED_AUTHOR_PROOF_HEADER`].
    forwarded_author_proof: Arc<str>,
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
    #[serde(default)]
    last_good_address: Option<SocketAddr>,
    /// Which of the Pairings made since this Server started this one is —
    /// none, for one it started with — so a name unpaired and paired again,
    /// even to the same key, is told apart from the Pairing before it.
    #[serde(skip)]
    generation: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InvitePayload {
    #[serde(rename = "a")]
    addresses: Vec<SocketAddr>,
    #[serde(rename = "k")]
    server_key: String,
    #[serde(rename = "t")]
    token: String,
    #[serde(rename = "h")]
    hostname: String,
}

struct ParsedInvite {
    addresses: Vec<SocketAddr>,
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
    _network_address: SocketAddr,
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
            SessionErrorCode::PairingProtocolMismatch => StatusCode::CONFLICT,
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
    fn addresses_by_recency(&self) -> Vec<SocketAddr> {
        self.last_good_address
            .into_iter()
            .chain(
                self.remote
                    .addresses
                    .iter()
                    .copied()
                    .filter(|address| Some(*address) != self.last_good_address),
            )
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
                http: reqwest::Client::new(),
            },
            active: Arc::new(Mutex::new(None)),
            address,
            pairing_changes: Arc::new(watch::channel(0).0),
            pairings_made: Arc::default(),
            invite_ttl,
            withdrawal_timeout: crate::server::ServerTimings::default().remote_withdrawal_timeout,
            handshake_timeout: crate::server::ServerTimings::default().serving_handshake_timeout,
            invites: Arc::new(StdMutex::new(InviteLedger::default())),
            protocol_version,
            identity: Arc::new(StdMutex::new(None)),
            peers: Arc::new(RwLock::new(peers)),
            remotes: Arc::new(RwLock::new(read_records(&data_dir.join(REMOTES_FILE))?)),
            remote_clients: Arc::new(StdMutex::new(HashMap::new())),
            revocations: Arc::new(RwLock::new(revocations)),
            forwarded_author_proof: URL_SAFE_NO_PAD.encode(new_token()).into(),
        })
    }

    /// Bounds how long removing a Remote waits for that Remote to drop its
    /// Peer record; injectable so tests need not wait out the default.
    pub(crate) fn with_withdrawal_timeout(mut self, timeout: tokio::time::Duration) -> Self {
        self.withdrawal_timeout = timeout;
        self
    }

    /// Bounds how long a connection to the Serving listener may take to
    /// finish its TLS handshake before it is dropped.
    pub(crate) fn with_handshake_timeout(mut self, timeout: tokio::time::Duration) -> Self {
        self.handshake_timeout = timeout;
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
        if !addresses_are_unique_and_nonempty(&request.addresses) {
            return Err(PairingFailure::new(
                SessionErrorCode::InvalidInviteAddresses,
                "an Invite needs at least one unique address",
            ));
        }

        let identity = self.identity().map_err(internal_pairing_failure)?;
        let token = new_token();
        let payload = InvitePayload {
            addresses: request.addresses.clone(),
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
            addresses: request.addresses,
        })
    }

    pub(crate) fn preview_invite(
        &self,
        invite: &str,
    ) -> std::result::Result<InvitePreview, PairingFailure> {
        let invite = parse_invite(invite)?;
        Ok(InvitePreview {
            hostname: invite.hostname,
            fingerprint: fingerprint(&invite.server_key),
            addresses: invite.addresses,
        })
    }

    pub(crate) async fn redeem_invite(
        &self,
        request: RedeemInviteRequest,
    ) -> std::result::Result<Remote, PairingFailure> {
        let invite = parse_invite(&request.invite)?;
        let addresses = ordered_addresses(&invite.addresses, &request.addresses)?;
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
        let enrolled = dial_enrollment(&addresses, &invite.server_key, &identity, &prepare).await?;
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
            addresses,
            status: RemoteStatus::Available,
        };
        self.persist_remote(&remote, &invite.server_key)?;

        let commit = EnrollmentRequest {
            token: URL_SAFE_NO_PAD.encode(invite.token),
            protocol_version: self.protocol_version,
            phase: EnrollmentPhase::Commit,
            hostname: Some(machine_hostname()),
        };
        if let Err(error) =
            dial_enrollment(&remote.addresses, &invite.server_key, &identity, &commit).await
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
                self.record_remote_connection(name, connection.health.status, connection.address);
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
        // A request that may have reached the Remote is never asked again at
        // another address unless asking twice changes nothing: only one that
        // was never delivered is.
        let repeatable = parts.method.is_safe();
        let response = first_remote_answer(&remote, &client, move |address| {
            let client = attempt_client.clone();
            let mut request = Request::new(Body::from(body.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.headers_mut() = parts.headers.clone();
            let added = added.clone();
            async move {
                match forward_request(
                    &client.http,
                    format!("https://{address}"),
                    request,
                    added,
                    Some(client.clone()),
                )
                .await
                {
                    Ok(response) => RemoteAddressAttempt::Answered(response),
                    Err(failure)
                        if failure.code == SessionErrorCode::PairingOutcomeUnknown
                            && !repeatable =>
                    {
                        RemoteAddressAttempt::Rejected(failure)
                    }
                    Err(_) => RemoteAddressAttempt::TryNext,
                }
            }
        })
        .await;
        match response {
            Ok((address, response)) => self.classify_remote_response(name, address, response).await,
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
        let withdrawal = first_remote_answer(remote, &client, move |address| {
            let client = attempt_client.clone();
            async move {
                match client
                    .http
                    .post(format!("https://{address}{PAIRING_WITHDRAWAL_PATH}"))
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        RemoteAddressAttempt::Answered(())
                    }
                    Ok(_) | Err(_) => RemoteAddressAttempt::TryNext,
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
        let tls = Arc::new(self.server_tls_config()?);
        self.discard_invites();
        stop_active(&mut active, &self.address).await;
        let connections = Arc::new(RevocableConnections::default());
        let task = tokio::spawn(serve(listener, tls, connections.clone(), self.clone()));
        *active = Some(ActiveServing {
            settings,
            address,
            task,
            connections,
        });
        self.address.send_replace(Some(address));
        tracing::info!(%address, "Serving listener ready");
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let mut active = self.active.lock().await;
        stop_active(&mut active, &self.address).await;
    }

    /// This Server's own key fingerprint: what a Remote it is paired with
    /// knows it by as a Peer, and names its Sidekicks' acts by.
    pub(crate) fn own_fingerprint(&self) -> Result<String> {
        Ok(fingerprint(&self.identity()?.public_key))
    }

    fn identity(&self) -> Result<IdentityMaterial> {
        let mut identity = self
            .identity
            .lock()
            .expect("Server identity lock is not poisoned");
        if let Some(identity) = identity.as_ref() {
            return Ok(identity.clone());
        }
        let private_key = load_or_generate_identity(&self.data_dir)?;
        let signing_key =
            KeyPair::try_from(private_key.as_slice()).context("read Server identity key")?;
        let certificate = CertificateParams::new(vec!["suru-server".to_owned()])
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

    fn server_tls_config(&self) -> Result<ServerConfig> {
        let identity = self.identity()?;
        ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .context("choose Serving TLS protocol versions")?
            .with_client_cert_verifier(Arc::new(PinnedPeers {
                peers: self.peers.clone(),
                invites: self.invites.clone(),
                revocations: self.revocations.clone(),
            }))
            .with_single_cert(
                vec![CertificateDer::from(identity.certificate)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key)),
            )
            .context("configure Serving TLS identity")
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

    fn record_remote_connection(&self, name: &str, status: RemoteStatus, address: SocketAddr) {
        self.record_remote_state(name, status, Some(address));
    }

    fn record_remote_state(
        &self,
        name: &str,
        status: RemoteStatus,
        last_good_address: Option<SocketAddr>,
    ) {
        let mut remotes = self
            .remotes
            .write()
            .expect("Remote record lock is not poisoned");
        let Some(index) = remotes.iter().position(|stored| stored.remote.name == name) else {
            return;
        };
        let previous_status = remotes[index].remote.status;
        let previous_address = remotes[index].last_good_address;
        if previous_status == status
            && last_good_address.is_none_or(|address| previous_address == Some(address))
        {
            return;
        }
        remotes[index].remote.status = status;
        if let Some(address) = last_good_address {
            remotes[index].last_good_address = Some(address);
        }
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes[index].remote.status = previous_status;
            remotes[index].last_good_address = previous_address;
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
        let (address, pairing_health) = first_remote_answer(remote, &client, move |address| {
            let client = attempt_client.clone();
            async move {
                let response = client
                    .http
                    .get(format!("https://{address}/health"))
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status().is_success() => {
                        match small_answer::<PairingHealth>(response).await {
                            Some(health) => RemoteAddressAttempt::Answered(health),
                            None => RemoteAddressAttempt::Rejected(PairingFailure::new(
                                SessionErrorCode::PairingConnectionFailed,
                                "Remote returned an invalid health response",
                            )),
                        }
                    }
                    Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                        RemoteAddressAttempt::Rejected(PairingFailure::new(
                            SessionErrorCode::PairingAuthenticationFailed,
                            "Remote refused this Server's key",
                        ))
                    }
                    Ok(_) | Err(_) => RemoteAddressAttempt::TryNext,
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
        Ok(RemoteConnection { address, health })
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
            paired_http_client(&remote.public_key, &identity, None)
                .map_err(internal_pairing_failure)?,
        );
        clients.insert(remote.remote.name.clone(), Arc::downgrade(&client));
        Ok(client)
    }

    async fn classify_remote_response(
        &self,
        name: &str,
        address: SocketAddr,
        response: Response,
    ) -> std::result::Result<Response, PairingFailure> {
        if response.status() == StatusCode::UNAUTHORIZED {
            self.record_remote_connection(name, RemoteStatus::Revoked, address);
            return Err(PairingFailure::new(
                SessionErrorCode::PairingAuthenticationFailed,
                "Remote refused this Server's key",
            ));
        }
        if response.status() != StatusCode::CONFLICT {
            self.record_remote_connection(name, RemoteStatus::Available, address);
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
        self.record_remote_connection(name, status, address);
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
            last_good_address: None,
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
        let was_existing = peers.iter().any(|peer| peer.id == id);
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
        let mut revocations = self
            .revocations
            .write()
            .expect("Peer revocation lock is not poisoned");
        if was_existing {
            revocations
                .entry(id)
                .or_insert_with(|| Arc::new(ConnectionRevocation::default()));
        } else {
            revocations.insert(id, Arc::new(ConnectionRevocation::default()));
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

async fn serve(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    connections: Arc<RevocableConnections>,
    controller: ServingController,
) {
    let listener = PairingTlsListener {
        listener,
        acceptor: TlsAcceptor::from(tls),
        handshake_timeout: controller.handshake_timeout,
        handshakes: tokio::task::JoinSet::new(),
        revocations: controller.revocations.clone(),
        connections,
    };
    let protocol_version = controller.protocol_version;
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
    let _ = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<ServingConnectionInfo>(),
    )
    .await;
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
    match forward_request(
        &state.controller.local_api.http,
        state.controller.local_api.base_url.clone(),
        request,
        vouched,
        None,
    )
    .await
    {
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
    if crate::broker::is_broker_path(path) {
        PeerRouteClass::LoopbackOnly
    } else if settings_mutation || stop || pairing_management {
        PeerRouteClass::Administration
    } else {
        PeerRouteClass::Api
    }
}

async fn forward_request(
    http: &reqwest::Client,
    base_url: String,
    request: Request<Body>,
    added_headers: HeaderMap,
    interest: Option<Arc<PairingHttpClient>>,
) -> std::result::Result<Response, PairingFailure> {
    let (parts, body) = request.into_parts();
    let target = format!(
        "{}{}",
        base_url,
        parts
            .uri
            .path_and_query()
            .map_or("/", axum::http::uri::PathAndQuery::as_str)
    );
    let mut headers = parts.headers;
    remove_hop_by_hop_headers(&mut headers);
    headers.remove(header::HOST);
    headers.remove(header::AUTHORIZATION);
    headers.remove(PAIRING_PROTOCOL_HEADER);
    headers.extend(added_headers);
    let forwarded = http
        .request(parts.method, target)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    // A request that never connected was never delivered; once connected,
    // it may have been, whatever went wrong after.
    let response = forwarded.send().await.map_err(|error| {
        if error.is_connect() {
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
    })?;
    let status = response.status();
    let mut headers = response.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let body = Box::pin(response.bytes_stream());
    let body = stream::unfold((body, interest), |(mut body, interest)| async move {
        body.next().await.map(|chunk| (chunk, (body, interest)))
    });
    let mut forwarded = Response::new(Body::from_stream(body));
    *forwarded.status_mut() = status;
    *forwarded.headers_mut() = headers;
    Ok(forwarded)
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

/// How one connection's TLS handshake with the Serving listener ended: done,
/// refused, or past its handshake timeout.
type Handshake =
    std::result::Result<std::io::Result<TlsStream<TcpStream>>, tokio::time::error::Elapsed>;

/// The Serving listener: each connection it accepts finishes its TLS
/// handshake on its own, within its handshake timeout, so one that
/// never does — a dialer that says nothing, or whose answers never reach it —
/// holds up no other. The handshakes under way end with the listener.
struct PairingTlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    /// How long each connection may take to finish its handshake.
    handshake_timeout: tokio::time::Duration,
    handshakes: tokio::task::JoinSet<(Handshake, SocketAddr)>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    connections: Arc<RevocableConnections>,
}

impl PairingTlsListener {
    /// The next connection to finish its TLS handshake, and the address it
    /// came from, accepting more meanwhile while there is room.
    async fn handshaken(&mut self) -> (TlsStream<TcpStream>, SocketAddr) {
        loop {
            let room = self.handshakes.len() < SERVING_HANDSHAKES_AT_ONCE;
            tokio::select! {
                accepted = self.listener.accept(), if room => match accepted {
                    Ok((stream, network_address)) => {
                        let acceptor = self.acceptor.clone();
                        let handshake_timeout = self.handshake_timeout;
                        self.handshakes.spawn(async move {
                            (
                                tokio::time::timeout(
                                    handshake_timeout,
                                    acceptor.accept(stream),
                                )
                                .await,
                                network_address,
                            )
                        });
                    }
                    Err(error) => {
                        tracing::warn!("Serving listener could not accept a connection: {error}");
                        tokio::task::yield_now().await;
                    }
                },
                Some(finished) = self.handshakes.join_next() => match finished {
                    Ok((Ok(Ok(stream)), network_address)) => return (stream, network_address),
                    Ok((_, network_address)) => {
                        tracing::debug!(peer = %network_address, "Serving TLS handshake refused");
                    }
                    Err(_) => {}
                },
            }
        }
    }
}

impl Listener for PairingTlsListener {
    type Io = RevocableTlsStream;
    type Addr = ServingConnectionInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, network_address) = self.handshaken().await;
        let connection_revocation = self.connections.register();
        let peer_key = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .and_then(|certificate| public_key_from_certificate(certificate).ok());
        let peer_id = peer_key.as_deref().map(fingerprint);
        let revocable_peer_id = peer_id.filter(|peer_id| {
            self.revocations
                .read()
                .expect("Peer revocation lock is not poisoned")
                .get(peer_id)
                .is_none_or(|revocation| !revocation.revoked.load(Ordering::Acquire))
        });
        (
            RevocableTlsStream {
                stream,
                peer_id: revocable_peer_id,
                revocations: self.revocations.clone(),
                connection_revocation,
            },
            ServingConnectionInfo {
                _network_address: network_address,
                peer_key,
            },
        )
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(ServingConnectionInfo {
            _network_address: self.listener.local_addr()?,
            peer_key: None,
        })
    }
}

struct RevocableTlsStream {
    stream: TlsStream<TcpStream>,
    peer_id: Option<String>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    connection_revocation: Arc<ConnectionRevocation>,
}

impl RevocableTlsStream {
    fn poll_revoked(&self, context: &mut TaskContext<'_>) -> bool {
        self.connection_revocation.poll(context)
            || self
                .peer_id
                .as_deref()
                .and_then(|peer_id| {
                    self.revocations
                        .read()
                        .expect("Peer revocation lock is not poisoned")
                        .get(peer_id)
                        .cloned()
                })
                .is_some_and(|revoked| revoked.poll(context))
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
}

impl ConnectionRevocation {
    /// Whether the connection has been cut, waking the task polling it once
    /// it is.
    pub(crate) fn poll(&self, context: &TaskContext<'_>) -> bool {
        self.waker.register(context.waker());
        self.revoked.load(Ordering::Acquire)
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.waker.wake();
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

impl axum::extract::connect_info::Connected<IncomingStream<'_, PairingTlsListener>>
    for ServingConnectionInfo
{
    fn connect_info(target: IncomingStream<'_, PairingTlsListener>) -> Self {
        target.remote_addr().clone()
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

async fn dial_enrollment(
    addresses: &[SocketAddr],
    server_key: &[u8],
    identity: &IdentityMaterial,
    enrollment: &EnrollmentRequest,
) -> std::result::Result<EnrollmentResponse, PairingFailure> {
    let client = paired_http_client(server_key, identity, Some(&enrollment.token))
        .map_err(internal_pairing_failure)?;
    for address in addresses {
        match client
            .http
            .post(format!("https://{address}/v1/pairing/enroll"))
            .json(enrollment)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                return small_answer(response).await.ok_or_else(|| {
                    PairingFailure::new(
                        SessionErrorCode::PairingConnectionFailed,
                        "Serving Server returned an invalid enrollment response",
                    )
                });
            }
            Ok(response) => return Err(decode_pairing_response(response).await),
            Err(_) => {}
        }
    }
    if client.server_key_rejections.load(Ordering::Acquire) > 0 {
        return Err(PairingFailure::new(
            SessionErrorCode::PairingAuthenticationFailed,
            "offered address presented a key other than the Invite's pinned key",
        ));
    }
    Err(PairingFailure::new(
        SessionErrorCode::PairingConnectionFailed,
        "could not reach an offered address with the Invite's pinned key",
    ))
}

struct PairingHttpClient {
    http: reqwest::Client,
    server_key_rejections: Arc<AtomicU64>,
}

enum RemoteAddressAttempt<T> {
    Answered(T),
    TryNext,
    Rejected(PairingFailure),
}

async fn first_remote_answer<T, F, Fut>(
    remote: &StoredRemote,
    client: &PairingHttpClient,
    mut attempt: F,
) -> std::result::Result<(SocketAddr, T), PairingFailure>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = RemoteAddressAttempt<T>>,
{
    let rejected_before = client.server_key_rejections.load(Ordering::Acquire);
    for address in remote.addresses_by_recency() {
        match attempt(address).await {
            RemoteAddressAttempt::Answered(response) => return Ok((address, response)),
            RemoteAddressAttempt::TryNext => {}
            RemoteAddressAttempt::Rejected(error) => return Err(error),
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
    address: SocketAddr,
    health: RemoteHealth,
}

fn paired_http_client(
    server_key: &[u8],
    identity: &IdentityMaterial,
    enrollment_token: Option<&str>,
) -> Result<PairingHttpClient> {
    let server_key_rejections = Arc::new(AtomicU64::new(0));
    let certificate = match enrollment_token {
        Some(token) => enrollment_certificate(identity, token)?,
        None => identity.certificate.clone(),
    };
    let tls = ClientConfig::builder_with_provider(crypto_provider())
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
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        // A proxied response holds the shared client as its interest lease, so
        // the pool and sockets disappear when the Remote's last response or
        // SSE stream ends.
        .pool_max_idle_per_host(1)
        .build()
        .context("build Pairing HTTP client")?;
    Ok(PairingHttpClient {
        http,
        server_key_rejections,
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

async fn decode_pairing_response(response: reqwest::Response) -> PairingFailure {
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
    if !addresses_are_unique_and_nonempty(&payload.addresses) {
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
        addresses: payload.addresses,
        server_key,
        token: decode_token(&payload.token)?,
        hostname: payload.hostname,
    })
}

fn addresses_are_unique_and_nonempty(addresses: &[SocketAddr]) -> bool {
    !addresses.is_empty() && addresses.iter().collect::<HashSet<_>>().len() == addresses.len()
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

fn ordered_addresses(
    offered: &[SocketAddr],
    chosen: &[SocketAddr],
) -> std::result::Result<Vec<SocketAddr>, PairingFailure> {
    if chosen.is_empty() {
        return Ok(offered.to_vec());
    }
    let offered_set = offered.iter().copied().collect::<HashSet<_>>();
    let chosen_set = chosen.iter().copied().collect::<HashSet<_>>();
    if offered_set != chosen_set || chosen_set.len() != chosen.len() {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidInviteAddresses,
            "ordered addresses must contain each offered address exactly once",
        ));
    }
    Ok(chosen.to_vec())
}

/// What `response` says, decoded as `T` — `None` where it says anything
/// else, or more than [`PAIRING_ANSWER_BUDGET`], which is read no further.
async fn small_answer<T: DeserializeOwned>(mut response: reqwest::Response) -> Option<T> {
    let mut read = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > PAIRING_ANSWER_BUDGET - read.len() {
            return None;
        }
        read.extend_from_slice(&chunk);
    }
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

fn machine_hostname() -> String {
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

fn read_records<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
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

    fn answering(status: StatusCode, body: Vec<u8>) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(status)
                .body(body)
                .expect("an answer"),
        )
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
                    _network_address: SocketAddr::from(([127, 0, 0, 1], 4000)),
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
        let address = SocketAddr::from(([127, 0, 0, 1], 9));
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
                .classify_remote_response("workstation", address, conflict(mismatch))
                .await
                .is_ok_and(|response| response.status() == StatusCode::CONFLICT),
            "a conflict within the budget is passed on"
        );
        let refusal = controller
            .classify_remote_response(
                "workstation",
                address,
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
