//! Lifecycle of the opt-in Server-to-Server listener.
//!
//! The loopback HTTP listener owns local Clients. This module owns the second
//! listener independently, so Settings adoption can start, replace, or stop
//! Serving without involving that local transport at all.

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair};
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error as TlsError, ServerConfig, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_rustls::TlsAcceptor;

use crate::{protocol::ServingSettings, runtime::protect_current_user_file};

const IDENTITY_FILE: &str = "server-identity.pk8";

#[derive(Clone)]
pub(crate) struct ServingController {
    data_dir: PathBuf,
    active: Arc<Mutex<Option<ActiveServing>>>,
    address: watch::Sender<Option<SocketAddr>>,
}

struct ActiveServing {
    settings: ServingSettings,
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl ServingController {
    pub(crate) fn new(data_dir: &Path) -> Self {
        let (address, _) = watch::channel(None);
        Self {
            data_dir: data_dir.to_path_buf(),
            active: Arc::new(Mutex::new(None)),
            address,
        }
    }

    pub(crate) fn address(&self) -> Option<SocketAddr> {
        *self.address.borrow()
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
            stop_active(&mut active, &self.address).await;
            return Ok(());
        }

        let requested = SocketAddr::new(settings.bind_address, settings.port);
        if let Some(running) = active.as_mut()
            && requested == running.address
        {
            // Port 0 may have been replaced with the assigned port in the Config Document.
            // That is already the listener in service, so adopting the more precise spelling
            // needs no transport churn.
            running.settings = settings;
            return Ok(());
        }
        let listener = TcpListener::bind(requested)
            .await
            .with_context(|| format!("bind Serving listener to {requested}"))?;
        let address = listener
            .local_addr()
            .context("read bound Serving address")?;
        let tls = Arc::new(server_tls_config(&self.data_dir, address)?);
        stop_active(&mut active, &self.address).await;
        let task = tokio::spawn(serve(listener, tls));
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

async fn serve(listener: TcpListener, tls: Arc<ServerConfig>) {
    let acceptor = TlsAcceptor::from(tls);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let acceptor = acceptor.clone();
                    connections.spawn(async move {
                        // No Peer is enrolled in this first Serving slice, so every handshake is
                        // refused. Keep credentials out of the Log: even at debug level only the
                        // network endpoint, never the verifier error or certificate, is recorded.
                        if acceptor.accept(stream).await.is_err() {
                            tracing::debug!(%peer, "Serving TLS handshake refused");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!("Serving listener could not accept a connection: {error}");
                    break;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

fn server_tls_config(data_dir: &Path, address: SocketAddr) -> Result<ServerConfig> {
    let key_der = load_or_generate_identity(data_dir)?;
    let signing_key = KeyPair::try_from(key_der.as_slice()).context("read Server identity key")?;
    let names = vec![address.ip().to_string(), "localhost".to_owned()];
    let certificate = CertificateParams::new(names)
        .context("describe Server identity certificate")?
        .self_signed(&signing_key)
        .context("mint Server identity certificate")?;
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("choose Serving TLS protocol versions")?
        .with_client_cert_verifier(Arc::new(NoEnrolledPeers))
        .with_single_cert(vec![certificate.der().clone()], private_key)
        .context("configure Serving TLS identity")
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

/// Until Pairing enrollment lands there are deliberately no recognized
/// certificates. Requiring client authentication while rejecting every
/// presented certificate makes the empty Peer set fail closed.
#[derive(Debug)]
struct NoEnrolledPeers;

impl ClientCertVerifier for NoEnrolledPeers {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Err(TlsError::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ))
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        reject_signature()
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        reject_signature()
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn reject_signature() -> Result<HandshakeSignatureValid, TlsError> {
    Err(TlsError::General("no enrolled Peer".to_owned()))
}
