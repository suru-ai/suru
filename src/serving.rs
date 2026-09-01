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
        InvitePreview, IssueInviteRequest, IssuedInvite, Peer, RedeemInviteRequest, Remote,
        RemoteHealth, RemoteStatus, ServingSettings, SessionError, SessionErrorCode,
    },
    runtime::protect_current_user_file,
};

const IDENTITY_FILE: &str = "server-identity.pk8";
const PEERS_FILE: &str = "peers.json";
const REVOKED_PEERS_FILE: &str = "revoked-peers.json";
const REMOTES_FILE: &str = "remotes.json";
const PAIRING_PROTOCOL_HEADER: &str = "x-suru-protocol-version";
const PEER_API_PREFIX: &str = "/v1/pairing/proxy";

#[derive(Clone)]
pub(crate) struct ServingController {
    data_dir: PathBuf,
    local_api: LocalApi,
    active: Arc<Mutex<Option<ActiveServing>>>,
    address: watch::Sender<Option<SocketAddr>>,
    invite_ttl: tokio::time::Duration,
    invites: Arc<StdMutex<InviteLedger>>,
    protocol_version: u32,
    identity: Arc<StdMutex<Option<IdentityMaterial>>>,
    peers: Arc<RwLock<Vec<StoredPeer>>>,
    remotes: Arc<RwLock<Vec<StoredRemote>>>,
    remote_clients: Arc<StdMutex<HashMap<String, Weak<PairingHttpClient>>>>,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
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
    connections: Arc<ServingConnections>,
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
    #[serde(default)]
    last_good_address: Option<SocketAddr>,
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
            invite_ttl,
            invites: Arc::new(StdMutex::new(InviteLedger::default())),
            protocol_version,
            identity: Arc::new(StdMutex::new(None)),
            peers: Arc::new(RwLock::new(peers)),
            remotes: Arc::new(RwLock::new(read_records(&data_dir.join(REMOTES_FILE))?)),
            remote_clients: Arc::new(StdMutex::new(HashMap::new())),
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

    pub(crate) async fn proxy_remote(
        &self,
        name: &str,
        mut request: Request<Body>,
    ) -> std::result::Result<Response, PairingFailure> {
        let remote = self.stored_remote(name)?;
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
        let response = first_remote_answer(&remote, &client, move |address| {
            let client = attempt_client.clone();
            let mut request = Request::new(Body::from(body.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.headers_mut() = parts.headers.clone();
            async move {
                match forward_request(
                    &client.http,
                    format!("https://{address}"),
                    request,
                    remote_forward_headers(self.protocol_version),
                    Some(client.clone()),
                )
                .await
                {
                    Ok(response) => RemoteAddressAttempt::Answered(response),
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
        let connections = Arc::new(ServingConnections::default());
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
                        match response.json::<PairingHealth>().await {
                            Ok(health) => RemoteAddressAttempt::Answered(health),
                            Err(_) => RemoteAddressAttempt::Rejected(PairingFailure::new(
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
        let body = axum::body::to_bytes(body, usize::MAX).await.map_err(|_| {
            PairingFailure::new(
                SessionErrorCode::PairingConnectionFailed,
                "Remote API response body could not be read",
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
        self.peers
            .read()
            .expect("Peer record lock is not poisoned")
            .iter()
            .any(|peer| bool::from(peer.public_key.as_slice().ct_eq(public_key)))
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
        let previous_peers = peers.clone();
        let was_existing = peers.iter().any(|peer| peer.id == id);
        if let Some(existing) = peers.iter_mut().find(|peer| peer.id == id) {
            existing.public_key = public_key;
        } else {
            peers.push(StoredPeer {
                id: id.clone(),
                public_key,
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
    connections: Arc<ServingConnections>,
    controller: ServingController,
) {
    let listener = PairingTlsListener {
        listener,
        acceptor: TlsAcceptor::from(tls),
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
    let authenticated = connection
        .peer_key
        .as_deref()
        .is_some_and(|key| state.controller.is_enrolled_peer(key));
    if !authenticated {
        return StatusCode::UNAUTHORIZED.into_response();
    }
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
    if peer_route_class(request.method(), &canonical_path) == PeerRouteClass::Administration {
        return StatusCode::FORBIDDEN.into_response();
    }
    match forward_request(
        &state.controller.local_api.http,
        state.controller.local_api.base_url.clone(),
        request,
        local_forward_headers(&state.controller.local_api.token),
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
}

fn peer_route_class(method: &Method, path: &str) -> PeerRouteClass {
    let settings_mutation = *method == Method::POST && path == "/v1/settings";
    let stop = *method == Method::POST && path == "/v1/server/stop";
    let pairing_management = path == "/v1/pairing" || path.starts_with("/v1/pairing/");
    if settings_mutation || stop || pairing_management {
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
    let response = forwarded.send().await.map_err(|_| {
        PairingFailure::new(
            SessionErrorCode::PairingConnectionFailed,
            "Remote API request failed",
        )
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

struct PairingTlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    revocations: Arc<RwLock<HashMap<String, Arc<ConnectionRevocation>>>>,
    connections: Arc<ServingConnections>,
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
                    return (
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

#[derive(Default)]
struct ServingConnections {
    revocations: StdMutex<Vec<Weak<ConnectionRevocation>>>,
}

impl ServingConnections {
    fn register(&self) -> Arc<ConnectionRevocation> {
        let revocation = Arc::new(ConnectionRevocation::default());
        let mut revocations = self
            .revocations
            .lock()
            .expect("Serving connection lock is not poisoned");
        revocations.retain(|revocation| revocation.strong_count() > 0);
        revocations.push(Arc::downgrade(&revocation));
        revocation
    }

    fn revoke_all(&self) {
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

#[derive(Default)]
struct ConnectionRevocation {
    revoked: AtomicBool,
    waker: AtomicWaker,
}

impl ConnectionRevocation {
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
