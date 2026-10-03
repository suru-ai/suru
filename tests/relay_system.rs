//! A Server reaching its Relay the way its machine reaches the web — through
//! the system's HTTP proxy — and writing nothing of its Relay to its Log. The
//! proxy is named in the environment and the Log is set up once for the
//! whole process, so this binary holds this one test alone.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use suru::{
    logging::{self, Role},
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{RelayLoginOutcome, RelayState},
    server::{self, ServerConfig, ServerTimings},
};
use suru_relay::{Identity, RelayConfig, ScriptedProvider};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

#[allow(dead_code)]
mod support;

#[allow(dead_code)]
#[path = "support/failing_provider.rs"]
mod failing_provider_support;

use support::{PROGRESS_DEADLINE, receive_initial_state};

/// A name nothing resolves, so a Server can reach the Relay by it only
/// through the proxy, which knows where it is.
const PROXIED_HOST: &str = "relay-behind-the-proxy.invalid";

#[test]
fn a_server_reaches_its_relay_through_the_system_proxy_and_logs_nothing_of_it() {
    let proxy = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind the system's HTTP proxy");
    proxy.set_nonblocking(true).unwrap();
    // SAFETY: nothing else in this process reads or writes the environment
    // while it changes: the binary holds this one test, and no runtime has
    // started yet.
    unsafe {
        std::env::set_var(
            "HTTP_PROXY",
            format!("http://{}", proxy.local_addr().unwrap()),
        );
        for name in [
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ] {
            std::env::remove_var(name);
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(reach_the_relay_through(proxy));
}

async fn reach_the_relay_through(proxy: std::net::TcpListener) {
    let relay_directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new());
    let relay = suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            relay_directory.path().join("relay.db"),
        ),
        provider.clone(),
    )
    .await
    .expect("start the Relay");
    let relay_address = relay.address();
    let carried = Arc::new(Mutex::new(Vec::<String>::new()));
    let proxying = tokio::spawn(forward_proxy(
        TcpListener::from_std(proxy).unwrap(),
        relay_address,
        carried.clone(),
    ));

    let state = tempfile::tempdir().unwrap();
    let config = ServerConfig::new(state.path(), "relay-system-proxy").unwrap();
    let log = logging::init(&config, Role::Server).expect("initialize the Server's Log");
    let server = server::spawn_with_provider_and_timings(
        config.clone(),
        Arc::new(failing_provider_support::FailingProviderRuntime),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        }
        .with_relay_retry_backoff(Duration::from_millis(5), Duration::from_millis(25)),
    )
    .await
    .expect("spawn the Server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "relay-system-proxy").unwrap(),
    )
    .await
    .expect("attach a Client, which reaches its Server past the proxy");
    receive_initial_state(&mut client).await;

    let address = format!("http://{PROXIED_HOST}:{}", relay_address.port());
    client.add_relay(address.clone()).await.unwrap();
    let login = client
        .begin_relay_login(&address)
        .await
        .expect("reach the Relay through the system's proxy");
    assert!(provider.approve(
        &login.user_code,
        Identity {
            subject: "583231".to_owned(),
            username: "octocat".to_owned(),
        },
    ));
    let done = client.follow_relay_login(&address).await.unwrap();
    assert!(
        matches!(done.outcome, RelayLoginOutcome::Done { .. }),
        "{done:?}"
    );
    timeout(PROGRESS_DEADLINE, async {
        while client.list_relays().await.unwrap()[0].state != RelayState::LoggedIn {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Server keeps its connection through the proxy");
    assert!(
        carried
            .lock()
            .unwrap()
            .iter()
            .any(|target| target.contains(PROXIED_HOST)),
        "the proxy carried the Server to its Relay"
    );

    drop(client);
    server.shutdown().await.unwrap();
    relay.shutdown().await.unwrap();
    proxying.abort();
    drop(log);
    let logs = std::fs::read_dir(config.state_dir().join("log"))
        .expect("read the Server's Log directory")
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    for material in [
        PROXIED_HOST,
        login.user_code.as_str(),
        login.verification_uri.as_str(),
        "octocat",
    ] {
        assert!(
            !logs.contains(material),
            "the Server's Log holds nothing of its Relays, yet it holds {material:?}"
        );
    }
}

/// A forward HTTP proxy carrying every request for [`PROXIED_HOST`] to the
/// Relay at `relay`, noting the target each request named.
async fn forward_proxy(
    listener: TcpListener,
    relay: std::net::SocketAddr,
    carried: Arc<Mutex<Vec<String>>>,
) {
    while let Ok((mut inbound, _)) = listener.accept().await {
        let carried = carried.clone();
        tokio::spawn(async move {
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0_u8];
                if inbound.read_exact(&mut byte).await.is_err() {
                    return;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let Some((request_line, rest)) = head.split_once("\r\n") else {
                return;
            };
            let mut parts = request_line.split(' ');
            let (Some(method), Some(target), Some(version)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return;
            };
            carried.lock().unwrap().push(target.to_owned());
            let Some(path) = target
                .strip_prefix("http://")
                .and_then(|target| target.strip_prefix(PROXIED_HOST))
                .and_then(|target| target.find('/').map(|slash| &target[slash..]))
            else {
                let _ = inbound
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n")
                    .await;
                return;
            };
            let Ok(mut outbound) = TcpStream::connect(relay).await else {
                return;
            };
            let forwarded = format!("{method} {path} {version}\r\n{rest}");
            if outbound.write_all(forwarded.as_bytes()).await.is_ok() {
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}
