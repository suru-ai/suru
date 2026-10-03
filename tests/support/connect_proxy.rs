//! A proxy on this machine's loopback that tunnels with HTTP CONNECT, as the
//! proxies `HTTPS_PROXY` names do, for a client presenting its credentials,
//! so a test sees each tunnel a Server opens through it.

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The most a CONNECT request's head may run to before the proxy stops
/// reading it.
const REQUEST_HEAD_BUDGET: usize = 8 * 1024;

pub struct ConnectProxy {
    pub address: std::net::SocketAddr,
    user: String,
    password: String,
    tunnelled_to: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    refused: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ConnectProxy {
    /// Starts a proxy that tunnels only for a client presenting `user` and
    /// `password` as its Basic proxy credentials.
    pub async fn start(user: &str, password: &str) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the CONNECT proxy");
        let address = listener
            .local_addr()
            .expect("read the CONNECT proxy's address");
        let credentials = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        );
        let tunnelled_to = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn({
            let tunnelled_to = tunnelled_to.clone();
            let refused = refused.clone();
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    tokio::spawn(tunnel(
                        client,
                        credentials.clone(),
                        tunnelled_to.clone(),
                        refused.clone(),
                    ));
                }
            }
        });
        Self {
            address,
            user: user.to_owned(),
            password: password.to_owned(),
            tunnelled_to,
            refused,
            task,
        }
    }

    /// The proxy as `HTTPS_PROXY` names it, credentials and all.
    pub fn url(&self) -> String {
        format!("http://{}:{}@{}", self.user, self.password, self.address)
    }

    /// Where each tunnel opened so far went, in the order opened.
    pub fn tunnelled_to(&self) -> Vec<String> {
        self.tunnelled_to
            .lock()
            .expect("tunnel record lock is not poisoned")
            .clone()
    }

    /// How many clients were refused for want of the proxy's credentials.
    pub fn refused(&self) -> usize {
        self.refused.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers one client's CONNECT request: refused without the proxy's
/// `credentials`, and otherwise a tunnel to where it asked, carrying bytes
/// both ways until either side ends.
async fn tunnel(
    mut client: tokio::net::TcpStream,
    credentials: String,
    tunnelled_to: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    refused: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    // Read byte by byte, so nothing past the request's head is taken from
    // the tunnel's first bytes.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        if head.len() > REQUEST_HEAD_BUDGET || client.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.split("\r\n");
    let Some(target) = lines
        .next()
        .and_then(|line| line.strip_prefix("CONNECT "))
        .and_then(|rest| rest.split(' ').next())
        .map(str::to_owned)
    else {
        return;
    };
    let authorized = lines
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization") && value.trim() == credentials
        });
    if !authorized {
        refused.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = client
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
            )
            .await;
        return;
    }
    let Ok(mut upstream) = tokio::net::TcpStream::connect(target.as_str()).await else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
        return;
    };
    tunnelled_to
        .lock()
        .expect("tunnel record lock is not poisoned")
        .push(target);
    if client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .is_ok()
    {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
}
