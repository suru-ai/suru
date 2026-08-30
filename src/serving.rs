//! Pairing formation and the opt-in Server-to-Server listener.
//!
//! Local callers use the small [`ServingController`] interface. Invite
//! encoding, durable identity and Pairing records, ordered dialing, and both
//! sides of the pinned-key TLS transport remain private to this module.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    serve::{IncomingStream, Listener},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::task::AtomicWaker;
use rcgen::{CertificateParams, KeyPair, PublicKeyData};
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
        IssueInviteRequest, IssuedInvite, Peer, RedeemInviteRequest, Remote, RemoteHealth,
        ServingSettings, SessionError, SessionErrorCode,
    },
    runtime::protect_current_user_file,
};

const IDENTITY_FILE: &str = "server-identity.pk8";
const PEERS_FILE: &str = "peers.json";
const REMOTES_FILE: &str = "remotes.json";

#[derive(Clone)]
pub(crate) struct ServingController {
    data_dir: PathBuf,
    active: Arc<Mutex<Option<ActiveServing>>>,
    address: watch::Sender<Option<SocketAddr>>,
    invite_ttl: tokio::time::Duration,
    invites: Arc<StdMutex<InviteLedger>>,
    protocol_version: u32,
    identity: Arc<StdMutex<Option<IdentityMaterial>>>,
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    remotes: Arc<RwLock<Vec<StoredRemote>>>,
    revocations: Arc<RwLock<HashMap<String, Arc<PeerRevocation>>>>,
}

struct ActiveServing {
    settings: ServingSettings,
    address: SocketAddr,
    task: JoinHandle<()>,
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
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRemote {
    #[serde(flatten)]
    remote: Remote,
    public_key: Vec<u8>,
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
}

struct ParsedInvite {
    addresses: Vec<SocketAddr>,
    server_key: Vec<u8>,
    token: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentRequest {
    token: String,
    protocol_version: u32,
    phase: EnrollmentPhase,
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
            | SessionErrorCode::PairingAuthenticationFailed => StatusCode::BAD_GATEWAY,
            SessionErrorCode::PairingProtocolMismatch => StatusCode::CONFLICT,
            SessionErrorCode::PeerNotFound | SessionErrorCode::RemoteNotFound => {
                StatusCode::NOT_FOUND
            }
            _ => StatusCode::BAD_REQUEST,
        }
    }

    fn response(self) -> Response {
        (
            self.status(),
            Json(SessionError {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

impl ServingController {
    pub(crate) fn new(
        data_dir: &Path,
        invite_ttl: tokio::time::Duration,
        protocol_version: u32,
    ) -> Result<Self> {
        let (address, _) = watch::channel(None);
        let peers: Vec<StoredPeer> = read_records(&data_dir.join(PEERS_FILE))?;
        let revocations = peers
            .iter()
            .map(|peer| (peer.id.clone(), Arc::new(PeerRevocation::default())))
            .collect();
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            active: Arc::new(Mutex::new(None)),
            address,
            invite_ttl,
            invites: Arc::new(StdMutex::new(InviteLedger::default())),
            protocol_version,
            identity: Arc::new(StdMutex::new(None)),
            peers: Arc::new(RwLock::new(peers)),
            remotes: Arc::new(RwLock::new(read_records(&data_dir.join(REMOTES_FILE))?)),
            revocations: Arc::new(RwLock::new(revocations)),
        })
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
        };
        self.persist_remote(&remote, &invite.server_key)?;

        let commit = EnrollmentRequest {
            token: URL_SAFE_NO_PAD.encode(invite.token),
            protocol_version: self.protocol_version,
            phase: EnrollmentPhase::Commit,
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

    pub(crate) async fn probe_remote(
        &self,
        name: &str,
    ) -> std::result::Result<RemoteHealth, PairingFailure> {
        let remote = self
            .remotes
            .read()
            .expect("Remote record lock is not poisoned")
            .iter()
            .find(|stored| stored.remote.name == name)
            .cloned()
            .ok_or_else(|| {
                PairingFailure::new(SessionErrorCode::RemoteNotFound, "Remote not found")
            })?;
        let identity = self.identity().map_err(internal_pairing_failure)?;
        let client =
            paired_http_client(&remote.public_key, &identity).map_err(internal_pairing_failure)?;
        for address in &remote.remote.addresses {
            let response = client
                .http
                .get(format!("https://{address}/health"))
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    return response.json().await.map_err(|_| {
                        PairingFailure::new(
                            SessionErrorCode::PairingConnectionFailed,
                            "Remote returned an invalid health response",
                        )
                    });
                }
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    return Err(PairingFailure::new(
                        SessionErrorCode::PairingAuthenticationFailed,
                        "Remote refused this Server's key",
                    ));
                }
                Ok(_) | Err(_) => {}
            }
        }
        if client.server_key_rejected.load(Ordering::Acquire) {
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
            .write()
            .expect("Peer revocation lock is not poisoned")
            .remove(id)
        {
            revoked.revoke();
        }
        Ok(())
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
            self.supersede_outstanding_invites();
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
        let listener = TcpListener::bind(requested)
            .await
            .with_context(|| format!("bind Serving listener to {requested}"))?;
        let address = listener
            .local_addr()
            .context("read bound Serving address")?;
        let tls = Arc::new(self.server_tls_config()?);
        self.supersede_outstanding_invites();
        stop_active(&mut active, &self.address).await;
        let task = tokio::spawn(serve(listener, tls, self.clone()));
        *active = Some(ActiveServing {
            settings,
            address,
            task,
        });
        self.address.send_replace(Some(address));
        tracing::info!(%address, "Serving listener ready");
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let mut active = self.active.lock().await;
        stop_active(&mut active, &self.address).await;
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
            .with_safe_default_protocol_versions()
            .context("choose Serving TLS protocol versions")?
            .with_client_cert_verifier(Arc::new(PinnedPeers {
                peers: self.peers.clone(),
                invites: self.invites.clone(),
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

    fn is_enrolled_peer(&self, public_key: &[u8]) -> bool {
        self.peers
            .read()
            .expect("Peer record lock is not poisoned")
            .iter()
            .any(|peer| bool::from(peer.public_key.as_slice().ct_eq(public_key)))
    }

    fn supersede_outstanding_invites(&self) {
        for invite in &mut self
            .invites
            .lock()
            .expect("Invite ledger lock is not poisoned")
            .issued
        {
            if matches!(invite.state, TokenState::Outstanding) {
                invite.state = TokenState::Superseded;
            }
        }
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
        });
        if let Err(error) = write_private_json(&self.data_dir.join(REMOTES_FILE), &*remotes) {
            remotes.pop();
            return Err(internal_pairing_failure(error));
        }
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
        if let Some(existing) = peers.iter_mut().find(|peer| peer.id == id) {
            existing.public_key = public_key;
        } else {
            peers.push(StoredPeer {
                id: id.clone(),
                public_key,
            });
        }
        if let Err(error) = write_private_json(&self.data_dir.join(PEERS_FILE), &*peers) {
            return Err(internal_pairing_failure(error));
        }
        self.revocations
            .write()
            .expect("Peer revocation lock is not poisoned")
            .entry(id)
            .or_insert_with(|| Arc::new(PeerRevocation::default()));
        issued.state = TokenState::Spent;
        Ok(())
    }
}

async fn stop_active(
    active: &mut Option<ActiveServing>,
    address: &watch::Sender<Option<SocketAddr>>,
) {
    address.send_replace(None);
    if let Some(running) = active.take() {
        running.task.abort();
        let _ = running.task.await;
        tracing::info!("Serving listener stopped");
    }
}

async fn serve(listener: TcpListener, tls: Arc<ServerConfig>, controller: ServingController) {
    let listener = PairingTlsListener {
        listener,
        acceptor: TlsAcceptor::from(tls),
        revocations: controller.revocations.clone(),
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
        .with_state(state);
    let _ = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<ServingConnectionInfo>(),
    )
    .await;
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
    Json(RemoteHealth {
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

struct PairingTlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    revocations: Arc<RwLock<HashMap<String, Arc<PeerRevocation>>>>,
}

impl Listener for PairingTlsListener {
    type Io = RevocableTlsStream;
    type Addr = ServingConnectionInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, network_address) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!("Serving listener could not accept a connection: {error}");
                    tokio::task::yield_now().await;
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(stream) => {
                    let peer_key = stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .and_then(|certificates| certificates.first())
                        .and_then(|certificate| public_key_from_certificate(certificate).ok());
                    let revoked = peer_key.as_deref().and_then(|key| {
                        self.revocations
                            .read()
                            .expect("Peer revocation lock is not poisoned")
                            .get(&fingerprint(key))
                            .cloned()
                    });
                    return (
                        RevocableTlsStream { stream, revoked },
                        ServingConnectionInfo {
                            _network_address: network_address,
                            peer_key,
                        },
                    );
                }
                Err(_) => {
                    tracing::debug!(peer = %network_address, "Serving TLS handshake refused");
                }
            }
        }
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
    revoked: Option<Arc<PeerRevocation>>,
}

impl RevocableTlsStream {
    fn poll_revoked(&self, context: &mut TaskContext<'_>) -> bool {
        self.revoked
            .as_ref()
            .is_some_and(|revoked| revoked.poll(context))
    }

    fn revoked_error() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "Peer was removed")
    }
}

#[derive(Default)]
struct PeerRevocation {
    revoked: AtomicBool,
    waker: AtomicWaker,
}

impl PeerRevocation {
    fn poll(&self, context: &TaskContext<'_>) -> bool {
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
        let enrollment_open = self
            .invites
            .lock()
            .expect("Invite ledger lock is not poisoned")
            .issued
            .iter()
            .any(|invite| {
                matches!(invite.state, TokenState::Outstanding)
                    && tokio::time::Instant::now() < invite.expires_at
            });
        if known || enrollment_open {
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
    rejected: Arc<AtomicBool>,
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
            self.rejected.store(true, Ordering::Release);
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
    let client = paired_http_client(server_key, identity).map_err(internal_pairing_failure)?;
    for address in addresses {
        match client
            .http
            .post(format!("https://{address}/v1/pairing/enroll"))
            .json(enrollment)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                return response.json().await.map_err(|_| {
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
    if client.server_key_rejected.load(Ordering::Acquire) {
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
    server_key_rejected: Arc<AtomicBool>,
}

fn paired_http_client(server_key: &[u8], identity: &IdentityMaterial) -> Result<PairingHttpClient> {
    let server_key_rejected = Arc::new(AtomicBool::new(false));
    let tls = ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .context("choose Pairing TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServerKey {
            expected: server_key.to_vec(),
            rejected: server_key_rejected.clone(),
        }))
        .with_client_auth_cert(
            vec![CertificateDer::from(identity.certificate.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key.clone())),
        )
        .context("configure Pairing client identity")?;
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .context("build Pairing HTTP client")?;
    Ok(PairingHttpClient {
        http,
        server_key_rejected,
    })
}

async fn decode_pairing_response(response: reqwest::Response) -> PairingFailure {
    let status = response.status();
    match response.json::<SessionError>().await {
        Ok(error) => PairingFailure::new(error.code, error.message),
        Err(_) => PairingFailure::new(
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

fn validate_remote_name(name: &str) -> std::result::Result<(), PairingFailure> {
    if name.trim().is_empty() || name != name.trim() {
        return Err(PairingFailure::new(
            SessionErrorCode::InvalidRemoteName,
            "Remote name must contain non-whitespace characters without outer whitespace",
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
