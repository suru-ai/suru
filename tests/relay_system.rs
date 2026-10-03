//! A Server reaching its Relays the way its machine reaches the web — through
//! the system's HTTP proxy, and over HTTPS trusting what the machine's trust
//! store trusts — and writing nothing of its Relays to its Log, however
//! verbose the Log is asked to be. The proxy and the trust store are named in
//! the environment, and the Log is set up once for the whole process, so this
//! binary holds this one test alone.

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

use support::{PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, receive_initial_state};

/// A name nothing resolves, so a Server can reach the Relay by it only
/// through the proxy, which knows where it is.
const PROXIED_HOST: &str = "relay-behind-the-proxy.invalid";
const PROXIED_ADDRESS: &str = "http://relay-behind-the-proxy.invalid:8443";

#[test]
fn a_server_reaches_its_relays_through_the_system_proxy_and_trust_and_logs_nothing_of_them() {
    let proxy = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind the system's HTTP proxy");
    proxy.set_nonblocking(true).unwrap();
    let trust = TrustedCertificate::mint();
    // SAFETY: nothing else in this process reads or writes the environment
    // while it changes: the binary holds this one test, and no runtime has
    // started yet.
    unsafe {
        std::env::set_var(
            "HTTP_PROXY",
            format!("http://{}", proxy.local_addr().unwrap()),
        );
        // On Linux the operating system's trust store is the certificate
        // bundle this names, which a machine's own CA would be added to.
        std::env::set_var("SSL_CERT_FILE", &trust.bundle);
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
        .block_on(reach_the_relays(proxy, trust));
}

/// A certificate for `127.0.0.1` issued by a CA of the test's own, and the
/// bundle holding that CA as a trust store would.
struct TrustedCertificate {
    _directory: tempfile::TempDir,
    bundle: std::path::PathBuf,
    chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: Vec<u8>,
}

impl TrustedCertificate {
    fn mint() -> Self {
        use base64::Engine as _;

        let mut authority = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        authority.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        authority.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        authority
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Relay test authority");
        let authority =
            rcgen::CertifiedIssuer::self_signed(authority, rcgen::KeyPair::generate().unwrap())
                .unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .signed_by(&key, &*authority)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("trusted.pem");
        let encoded = base64::engine::general_purpose::STANDARD.encode(authority.der());
        let lines = encoded
            .as_bytes()
            .chunks(64)
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            &bundle,
            format!("-----BEGIN CERTIFICATE-----\n{lines}\n-----END CERTIFICATE-----\n"),
        )
        .unwrap();
        Self {
            _directory: directory,
            bundle,
            chain: vec![certificate.der().clone(), authority.der().clone()],
            key: key.serialize_der(),
        }
    }
}

async fn reach_the_relays(proxy: std::net::TcpListener, trust: TrustedCertificate) {
    let relay_directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new());
    let relay = suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            relay_directory.path().join("relay.db"),
            PROXIED_ADDRESS,
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
    let log = logging::init_with_filter_directives(&config, Role::Server, Some("trace".to_owned()))
        .expect("initialize the Server's Log, as verbose as it can be asked to be");
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

    let address = PROXIED_ADDRESS.to_owned();
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

    // A Relay on this machine's loopback is reached directly: a proxy
    // elsewhere could not reach it.
    let loopback_route = ObservedTcpProxy::start((std::net::Ipv4Addr::LOCALHOST, 9).into()).await;
    let loopback_address = format!("http://{}", loopback_route.address);
    let loopback_relay_directory = tempfile::tempdir().unwrap();
    let loopback_relay = suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            loopback_relay_directory.path().join("relay.db"),
            loopback_address.clone(),
        ),
        provider.clone(),
    )
    .await
    .expect("start a Relay on the loopback");
    loopback_route.retarget(loopback_relay.address());
    client.add_relay(loopback_address.clone()).await.unwrap();
    let loopback_login = client
        .begin_relay_login(&loopback_address)
        .await
        .expect("reach a Relay on the loopback past the proxy");
    assert!(provider.approve(
        &loopback_login.user_code,
        Identity {
            subject: "583231".to_owned(),
            username: "octocat".to_owned(),
        },
    ));
    assert!(matches!(
        client
            .follow_relay_login(&loopback_address)
            .await
            .unwrap()
            .outcome,
        RelayLoginOutcome::Done { .. }
    ));
    assert!(
        !carried
            .lock()
            .unwrap()
            .iter()
            .any(|target| target.contains(&loopback_route.address.to_string())),
        "a Relay on the loopback was handed to the proxy"
    );
    loopback_relay.shutdown().await.unwrap();

    // Over HTTPS, a Relay whose certificate the machine's trust store
    // trusts is logged in at as any other. Only Linux names its trust store
    // in the environment, so only there can a test add to it.
    let mut other_material = vec![loopback_address, loopback_login.user_code];
    if cfg!(target_os = "linux") {
        let terminating = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let https_address = format!("https://{}", terminating.local_addr().unwrap());
        let https_relay_directory = tempfile::tempdir().unwrap();
        let https_relay = suru_relay::start(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                https_relay_directory.path().join("relay.db"),
                https_address.clone(),
            ),
            provider.clone(),
        )
        .await
        .expect("start the Relay behind HTTPS");
        let terminator = tokio::spawn(terminate_tls(
            terminating,
            trust.chain.clone(),
            trust.key.clone(),
            https_relay.address(),
        ));
        client.add_relay(https_address.clone()).await.unwrap();
        let login = client
            .begin_relay_login(&https_address)
            .await
            .expect("reach a Relay whose certificate the trust store trusts");
        assert!(provider.approve(
            &login.user_code,
            Identity {
                subject: "583231".to_owned(),
                username: "octocat".to_owned(),
            },
        ));
        let done = client.follow_relay_login(&https_address).await.unwrap();
        assert!(
            matches!(done.outcome, RelayLoginOutcome::Done { .. }),
            "{done:?}"
        );
        assert_eq!(https_relay.store().logins().await.unwrap().len(), 1);
        other_material.push(https_address);
        other_material.push(login.user_code);
        https_relay.shutdown().await.unwrap();
        terminator.abort();
    }

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
    ]
    .into_iter()
    .chain(other_material.iter().map(String::as_str))
    {
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

/// Serves HTTPS with `chain` and `key` at `listener`, carrying each
/// connection's plain bytes on to the Relay at `relay`, as an operator's
/// reverse proxy does.
async fn terminate_tls(
    listener: TcpListener,
    chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: Vec<u8>,
    relay: std::net::SocketAddr,
) {
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()))
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    while let Ok((inbound, _)) = listener.accept().await {
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(mut inbound) = acceptor.accept(inbound).await else {
                return;
            };
            let Ok(mut outbound) = TcpStream::connect(relay).await else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        });
    }
}
