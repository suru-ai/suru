//! A Server reaching its Relays the way its machine reaches the web — through
//! the system's HTTP proxy, and over HTTPS trusting what the machine's trust
//! store trusts — and writing nothing of its Relays to its Log, however
//! verbose the Log is asked to be, whether it logs in at them, Serves
//! through them, or refuses one whose certificate it cannot accept. The proxy
//! and the trust store are named in the environment, and the Log is set up
//! once for the whole process, so this binary holds this one test alone.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rcgen::PublicKeyData as _;
use suru::{
    logging::{self, Role},
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{RelayLoginOutcome, RelayState, SettingMutation},
    server::{self, ServerConfig, ServerTimings},
};
use suru_relay::{Admission, AdmissionRule, Identity, RelayConfig, ScriptedProvider};
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

use support::{
    PROGRESS_DEADLINE, observed_tcp_proxy::ObservedTcpProxy, receive_initial_state,
    relay_voice::RelayVoice,
};

/// A name nothing resolves, so a Server can reach the Relay by it only
/// through the proxy, which knows where it is.
const PROXIED_HOST: &str = "relay-behind-the-proxy.invalid";
const PROXIED_ADDRESS: &str = "http://relay-behind-the-proxy.invalid:8443";

/// A name nothing resolves, by which the Server knows a Relay serving HTTPS
/// under a certificate for another name: the proxy tunnels to it.
const MISNAMED_HOST: &str = "private-relay.company.example";
const MISNAMED_ADDRESS: &str = "https://private-relay.company.example:8443";

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
        // What could name another proxy, or exempt the Relay from this one,
        // goes first: on Windows a variable's name is matched regardless of
        // case, so clearing `https_proxy` after setting `HTTPS_PROXY` would
        // clear the proxy itself.
        for name in [
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ] {
            std::env::remove_var(name);
        }
        for name in ["HTTP_PROXY", "HTTPS_PROXY"] {
            std::env::set_var(name, format!("http://{}", proxy.local_addr().unwrap()));
        }
        // On Linux the operating system's trust store is the certificate
        // bundle this names, which a machine's own CA would be added to.
        std::env::set_var("SSL_CERT_FILE", &trust.bundle);
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
        std::fs::write(&bundle, pem("CERTIFICATE", authority.der())).unwrap();
        Self {
            _directory: directory,
            bundle,
            chain: vec![certificate.der().clone(), authority.der().clone()],
            key: key.serialize_der(),
        }
    }

    /// Writes the certificate's chain and its key to `directory`, in PEM, as
    /// a Relay serving HTTPS itself reads them: the files they are in.
    fn write(&self, directory: &std::path::Path) -> suru_relay::TlsFiles {
        let (chain, key) = (directory.join("chain.pem"), directory.join("key.pem"));
        std::fs::write(
            &chain,
            self.chain
                .iter()
                .map(|certificate| pem("CERTIFICATE", certificate))
                .collect::<String>(),
        )
        .unwrap();
        std::fs::write(&key, pem("PRIVATE KEY", &self.key)).unwrap();
        suru_relay::TlsFiles::new(chain, key)
    }
}

/// `der` in PEM, under `label`.
fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let lines = encoded
        .as_bytes()
        .chunks(64)
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{lines}\n-----END {label}-----\n")
}

/// Admission by the scripted provider, which admits whoever it logs in.
fn admitting(provider: &Arc<ScriptedProvider>) -> Admission {
    Admission::by([provider.clone() as Arc<dyn AdmissionRule>])
}

async fn reach_the_relays(proxy: std::net::TcpListener, trust: TrustedCertificate) {
    let relay_directory = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new());
    let relay = suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            relay_directory.path().join("relay.db"),
            PROXIED_ADDRESS,
        )
        .with_connection_log(std::io::sink())
        .with_admission(admitting(&provider)),
        provider.clone(),
    )
    .await
    .expect("start the Relay");
    let relay_address = relay.address();
    let carried = Arc::new(Mutex::new(Vec::<String>::new()));
    let tunnelled = Arc::new(Mutex::new(None));
    let proxying = tokio::spawn(forward_proxy(
        TcpListener::from_std(proxy).unwrap(),
        relay_address,
        tunnelled.clone(),
        carried.clone(),
    ));

    let state = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = ServerConfig::new(state.path(), "relay-system-proxy")
        .unwrap()
        .with_config_dir(config_root.path());
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
        while client.list_relays().await.unwrap().relays[0].state != RelayState::LoggedIn {
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

    // Serving through the Relay, the Server waits there through the proxy,
    // and takes up a join asked of it on a connection of its own there,
    // handing what the join carries to its acceptor.
    for mutation in [
        SettingMutation::ServingPort { value: Some(0) },
        SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        },
        SettingMutation::ServingEnabled { value: Some(true) },
    ] {
        client.mutate_setting(mutation).await.unwrap();
    }
    let proxied_before = carried.lock().unwrap().len();
    client
        .set_relay_serve_through(&address, true)
        .await
        .expect("Serve through the Relay");
    let voice = RelayVoice {
        at: relay_address,
        known_as: PROXIED_ADDRESS.to_owned(),
    };
    let asking = rcgen::KeyPair::generate().unwrap();
    voice.log_in(&provider, &asking, "583231", "octocat").await;
    let identity = std::fs::read(config.data_dir().join("server-identity.pk8")).unwrap();
    let identity = rcgen::KeyPair::try_from(identity.as_slice()).unwrap();
    drop(
        voice
            .joined(&asking, &identity.subject_public_key_info())
            .await,
    );
    assert!(
        carried.lock().unwrap().len() >= proxied_before + 2,
        "the Server waits at its Relay, and takes a join up there, through the proxy"
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
        )
        .with_admission(admitting(&provider)),
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

    // A Relay serving HTTPS itself, from certificate files, whose
    // certificate the machine's trust store trusts, is logged in at as any
    // other. Only Linux names its trust store in the environment, so only
    // there can a test add to it.
    let mut other_material = vec![loopback_address, loopback_login.user_code];
    if cfg!(target_os = "linux") {
        // The Relay is known by the address of a route to it that carries
        // its TLS untouched, which listens before the Relay does.
        let route = ObservedTcpProxy::start((std::net::Ipv4Addr::LOCALHOST, 9).into()).await;
        let https_address = format!("https://{}", route.address);
        let https_relay_directory = tempfile::tempdir().unwrap();
        let https_relay = suru_relay::start(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                https_relay_directory.path().join("relay.db"),
                https_address.clone(),
            )
            .with_tls(trust.write(https_relay_directory.path()))
            .with_admission(admitting(&provider)),
            provider.clone(),
        )
        .await
        .expect("start the Relay serving HTTPS");
        route.retarget(https_relay.address());
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
    }

    // A Relay known by a name its certificate is not for — or, where the
    // machine's trust store cannot be added to, whose certificate it does
    // not trust — is refused, its certificate checked against the name the
    // Server knows it by, through the proxy, which tunnels to it. However
    // the check fails, nothing of that name reaches the Log.
    let misnamed_relay_directory = tempfile::tempdir().unwrap();
    let misnamed_relay = suru_relay::start(
        RelayConfig::new(
            (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            misnamed_relay_directory.path().join("relay.db"),
            MISNAMED_ADDRESS,
        )
        .with_tls(trust.write(misnamed_relay_directory.path()))
        .with_admission(admitting(&provider)),
        provider.clone(),
    )
    .await
    .expect("start the Relay serving HTTPS under another name");
    *tunnelled.lock().unwrap() = Some(misnamed_relay.address());
    client.add_relay(MISNAMED_ADDRESS.to_owned()).await.unwrap();
    let refused = client
        .begin_relay_login(MISNAMED_ADDRESS)
        .await
        .expect_err("a certificate not for the Relay's name is refused");
    assert!(
        refused.to_string().contains("certificate"),
        "the refusal says the certificate is not accepted: {refused:#}"
    );
    assert!(
        carried
            .lock()
            .unwrap()
            .iter()
            .any(|target| target.starts_with(MISNAMED_HOST)),
        "the proxy tunnelled the Server to the Relay, whose certificate it checked"
    );
    other_material.push(MISNAMED_HOST.to_owned());
    misnamed_relay.shutdown().await.unwrap();

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
        let holding = logs
            .lines()
            .filter(|line| line.contains(material))
            .collect::<Vec<_>>();
        assert!(
            holding.is_empty(),
            "the Server's Log holds nothing of its Relays, yet it holds {material:?}: \
             {holding:#?}"
        );
    }
}

/// A forward HTTP proxy carrying every request for [`PROXIED_HOST`] to the
/// Relay at `relay`, and tunnelling every connection asked for to
/// [`MISNAMED_HOST`] to wherever `tunnelled` says, noting the target each
/// request named.
async fn forward_proxy(
    listener: TcpListener,
    relay: std::net::SocketAddr,
    tunnelled: Arc<Mutex<Option<std::net::SocketAddr>>>,
    carried: Arc<Mutex<Vec<String>>>,
) {
    while let Ok((mut inbound, _)) = listener.accept().await {
        let carried = carried.clone();
        let tunnelled = tunnelled.clone();
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
            if method == "CONNECT" {
                let to = target
                    .strip_prefix(MISNAMED_HOST)
                    .and_then(|_| *tunnelled.lock().unwrap());
                let Some(to) = to else {
                    let _ = inbound
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let Ok(mut outbound) = TcpStream::connect(to).await else {
                    return;
                };
                if inbound
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .is_ok()
                {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                return;
            }
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
