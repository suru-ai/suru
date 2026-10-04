//! HTTPS from certificate files its operator gives a Relay, for one with no
//! reverse proxy in front of it to serve HTTPS for it. The Relay obtains no
//! certificate of its own: it serves the certificate chain and private key in
//! two PEM files, as a certificate authority, or a tool that renews
//! certificates from one, writes them. TLS versions and cipher suites are
//! rustls's defaults.
//!
//! A certificate is renewed by replacing the files, with no restart and on
//! every platform alike: the Relay reads them again every so often
//! ([`crate::RelayConfig::with_certificate_check_interval`]), and where what
//! they hold has changed, serves it to every connection made from then on —
//! once both files can be read, hold what they should, and match. Until then
//! it goes on serving what it served before, saying on its diagnostic log
//! why it passed what it read over, so a renewal caught half-written is taken
//! up once it is whole. Connections already made keep the certificate they
//! were made with.

use std::{
    fmt,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rustls::{
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

/// How many connections whose TLS handshake has completed may wait for the
/// Relay to take them up, before more wait to be handed on.
const HANDED_ON: usize = 64;

/// The files a Relay serving HTTPS reads its certificate from: the
/// certificate chain, its own certificate first, and that certificate's
/// private key, each in PEM.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    certificate_chain: PathBuf,
    private_key: PathBuf,
}

impl TlsFiles {
    pub fn new(certificate_chain: impl Into<PathBuf>, private_key: impl Into<PathBuf>) -> Self {
        Self {
            certificate_chain: certificate_chain.into(),
            private_key: private_key.into(),
        }
    }

    /// The certificate chain file.
    pub fn certificate_chain(&self) -> &Path {
        &self.certificate_chain
    }

    /// The private key file.
    pub fn private_key(&self) -> &Path {
        &self.private_key
    }

    /// What the two files hold.
    fn read(&self) -> Result<Read> {
        let read = |path: &Path, what: &str| {
            std::fs::read(path).with_context(|| format!("read the Relay's {what} file {path:?}"))
        };
        Ok(Read {
            certificate_chain: read(&self.certificate_chain, "certificate chain")?,
            private_key: read(&self.private_key, "private key")?,
        })
    }

    /// The certificate `read` holds, where it holds one the Relay can serve.
    fn certified(&self, read: &Read) -> Result<CertifiedKey> {
        let chain = CertificateDer::pem_slice_iter(&read.certificate_chain)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                anyhow!(
                    "the Relay's certificate chain file {:?} is not well-formed PEM: {error}",
                    self.certificate_chain
                )
            })?;
        if chain.is_empty() {
            return Err(anyhow!(
                "the Relay's certificate chain file {:?} holds no certificate in PEM form",
                self.certificate_chain
            ));
        }
        let key =
            PrivateKeyDer::from_pem_slice(&read.private_key).map_err(|error| match error {
                rustls::pki_types::pem::Error::NoItemsFound => anyhow!(
                    "the Relay's private key file {:?} holds no private key in PEM form",
                    self.private_key
                ),
                error => anyhow!(
                    "the Relay's private key file {:?} is not well-formed PEM: {error}",
                    self.private_key
                ),
            })?;
        CertifiedKey::from_der(chain, key, &provider()).map_err(|error| match error {
            rustls::Error::InconsistentKeys(_) => anyhow!(
                "the private key in {:?} is not the key of the first certificate in {:?}, which \
                 must be the Relay's own",
                self.private_key,
                self.certificate_chain
            ),
            error => anyhow!(
                "the Relay cannot serve the certificate in {:?} with the private key in {:?}: \
                 {error}",
                self.certificate_chain,
                self.private_key
            ),
        })
    }
}

/// What a Relay's certificate files held when it read them.
#[derive(Eq, PartialEq)]
struct Read {
    certificate_chain: Vec<u8>,
    private_key: Vec<u8>,
}

/// The certificate a Relay serves: what its files held when last read whole.
pub(crate) struct Certificate {
    files: TlsFiles,
    serving: RwLock<Arc<CertifiedKey>>,
    /// What the files held when last read, served or passed over, or why
    /// they could not be read, so each change is acted on, and said, once.
    last_read: Mutex<Result<Read, String>>,
}

impl fmt::Debug for Certificate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Certificate")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl Certificate {
    /// The certificate `files` hold, refusing files the Relay cannot serve
    /// from, saying which and why.
    pub(crate) fn load(files: TlsFiles) -> Result<Self> {
        let read = files.read()?;
        let certified = files.certified(&read)?;
        Ok(Self {
            files,
            serving: RwLock::new(Arc::new(certified)),
            last_read: Mutex::new(Ok(read)),
        })
    }

    /// Reads the files again, serving what they hold from now on where it
    /// has changed and can be served.
    pub(crate) fn reload(&self) {
        let read = self.files.read().map_err(|error| format!("{error:#}"));
        let mut last_read = self
            .last_read
            .lock()
            .expect("certificate lock is not poisoned");
        if *last_read == read {
            return;
        }
        match &read {
            Ok(changed) => match self.files.certified(changed) {
                Ok(certified) => {
                    *self
                        .serving
                        .write()
                        .expect("certificate lock is not poisoned") = Arc::new(certified);
                    tracing::info!(
                        "serving the certificate in {:?} from now on, as it has changed",
                        self.files.certificate_chain
                    );
                }
                Err(error) => tracing::warn!(
                    "the Relay goes on serving the certificate it had, as the one its files now \
                     hold cannot be served: {error:#}"
                ),
            },
            Err(error) => tracing::warn!(
                "the Relay goes on serving the certificate it had, as its files cannot be read \
                 again: {error}"
            ),
        }
        *last_read = read;
    }
}

impl ResolvesServerCert for Certificate {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.serving
                .read()
                .expect("certificate lock is not poisoned")
                .clone(),
        )
    }
}

/// Reads `certificate`'s files again every `interval`, for as long as it is
/// awaited.
pub(crate) async fn keep_reloading(certificate: &Arc<Certificate>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let certificate = certificate.clone();
        let _ = tokio::task::spawn_blocking(move || certificate.reload()).await;
    }
}

fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// How a Relay serves `certificate`: as rustls does by default, to a Server
/// opening a WebSocket, which it does over HTTP/1.1.
pub(crate) fn server_config(certificate: Arc<Certificate>) -> Arc<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(provider()))
        .with_safe_default_protocol_versions()
        .expect("ring speaks every TLS version rustls deems safe")
        .with_no_client_auth()
        .with_cert_resolver(certificate);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// A listener serving HTTPS: it hands on each connection made to it once its
/// TLS handshake has completed, each handshake made on its own and given up
/// past a timeout, so none slow to make one holds up the rest.
pub(crate) struct TlsListener {
    address: SocketAddr,
    handshaken: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    /// Serves HTTPS as `config` says on the connections `listener` accepts,
    /// each handshake given `handshake_timeout`, until the listener returned
    /// is dropped.
    pub(crate) fn new(
        listener: TcpListener,
        config: Arc<rustls::ServerConfig>,
        handshake_timeout: Duration,
    ) -> std::io::Result<Self> {
        let address = listener.local_addr()?;
        let (hand_on, handshaken) = mpsc::channel(HANDED_ON);
        let acceptor = TlsAcceptor::from(config);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    () = hand_on.closed() => return,
                };
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        pause_after(&error).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let hand_on = hand_on.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await
                    {
                        let _ = hand_on.send((stream, peer)).await;
                    }
                });
            }
        });
        Ok(Self {
            address,
            handshaken,
        })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.handshaken.recv().await {
            Some(handshaken) => handshaken,
            // What accepts connections runs for as long as this listener
            // stands.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.address)
    }
}

/// Waits, after a failure to accept a connection, before trying again: not
/// at all where that one connection failed, and a second where the listener
/// itself did — out of file descriptors, say — as axum's own listener does.
async fn pause_after(error: &std::io::Error) {
    if matches!(
        error.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!("could not accept a connection: {error}");
    tokio::time::sleep(Duration::from_secs(1)).await;
}
