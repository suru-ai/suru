use crate::support::PROGRESS_DEADLINE;
use base64::Engine as _;
use diesel::{
    Connection, QueryableByName, RunQueryDsl, SqliteConnection, connection::SimpleConnection,
    sql_types::BigInt,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use suru::{
    build_identity,
    logging::{self, Role},
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent, stop_server},
    protocol::{
        AdmitPromptRequest, AgentId, AgentIdentity, ApprovalPosture, CodexApprovalPolicy,
        CodexSandboxMode, CreateSessionRequest, Health, InitialPrompt, IssueInviteRequest,
        LifecycleState, ModelAvailability, ModelDescriptor, ModelId, Outlook, PROTOCOL_VERSION,
        PromptDelivery, PromptId, ProviderId, RedeemInviteRequest, RemoteRemoval, RemoteStatus,
        ResolveWorkspaceRequest, SERVER_SHUTDOWN_EVENT, SESSION_SNAPSHOT_EVENT,
        SESSION_UPDATED_EVENT, ServerIdentity, ServerShutdown, SessionError, SessionErrorCode,
        SessionSnapshot, SessionUpdate, SettingMutation, ShutdownReason,
        UpdateApprovalPostureRequest,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, timeout};

#[allow(dead_code)]
mod support;

#[path = "server_integration/checkouts.rs"]
mod checkout_observation;
#[path = "server_integration/preparation_recovery.rs"]
mod preparation_recovery;

#[allow(dead_code)]
#[path = "support/failing_provider.rs"]
mod failing_provider_support;
#[allow(dead_code)]
#[path = "support/provider.rs"]
mod provider_support;

use support::{
    read_runtime_descriptor, receive_initial_state, request_server_shutdown,
    write_runtime_descriptor,
};

#[derive(QueryableByName)]
struct SqliteCount {
    #[diesel(sql_type = BigInt)]
    value: i64,
}

#[test]
fn workspace_display_data_resolves_the_servers_home_and_handles_unknown_homes() {
    use suru::protocol::{PathStyle, WorkspacePaths};

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("child")).unwrap();
    let paths = WorkspacePaths::from_home(Some(&home.path().join("child").join("..")));
    assert_eq!(
        paths.home.as_deref(),
        suru::paths::canonical(home.path()).unwrap().to_str()
    );
    assert_eq!(
        paths.style,
        if cfg!(windows) {
            PathStyle::Windows
        } else {
            PathStyle::Unix
        }
    );
    assert_eq!(WorkspacePaths::from_home(None).home, None);
    assert_eq!(
        WorkspacePaths::from_home(Some(&home.path().join("missing"))).home,
        None
    );
}

fn seed_database(path: &std::path::Path, sql: &str) {
    std::fs::create_dir_all(path.parent().expect("database has a parent directory"))
        .expect("create fixture data directory");
    let mut connection =
        SqliteConnection::establish(path.to_str().expect("fixture database path is valid UTF-8"))
            .expect("open fixture database");
    connection
        .batch_execute(sql)
        .expect("seed fixture database");
}

fn sqlite_count(path: &std::path::Path, query: &str) -> i64 {
    let mut connection =
        SqliteConnection::establish(path.to_str().expect("fixture database path is valid UTF-8"))
            .expect("open fixture database");
    diesel::sql_query(query)
        .get_result::<SqliteCount>(&mut connection)
        .expect("query fixture database")
        .value
}

#[derive(Debug)]
struct CaptureServerCertificate {
    certificate: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

#[derive(Debug)]
struct PinnedTestServerCertificate {
    expected_public_key: Vec<u8>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedTestServerCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if public_key_from_certificate(end_entity.as_ref()) != self.expected_public_key {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl rustls::client::danger::ServerCertVerifier for CaptureServerCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self
            .certificate
            .lock()
            .expect("captured Server certificate lock is not poisoned") =
            Some(end_entity.as_ref().to_vec());
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn dial_with_unknown_certificate(
    address: std::net::SocketAddr,
) -> (std::io::Result<()>, Vec<u8>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["unknown-peer".to_owned()])
            .expect("generate unknown Peer identity");
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let verifier = CaptureServerCertificate {
        certificate: captured.clone(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("choose test TLS protocol versions")
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
    .with_client_auth_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            signing_key.serialize_der(),
        )),
    )
    .expect("configure unknown Peer identity");
    let stream = tokio::net::TcpStream::connect(address).await;
    let result = match stream {
        Ok(stream) => match tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
            .connect(
                rustls::pki_types::ServerName::try_from("localhost")
                    .expect("localhost is a valid TLS name"),
                stream,
            )
            .await
        {
            Err(error) => Err(error),
            Ok(mut stream) => match stream.write_all(b"GET /health HTTP/1.1\r\n\r\n").await {
                Err(error) => Err(error),
                Ok(()) => {
                    let mut byte = [0];
                    match timeout(PROGRESS_DEADLINE, stream.read(&mut byte)).await {
                        Ok(Err(error)) => Err(error),
                        Ok(Ok(0)) => Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "Serving listener closed the unknown Peer connection",
                        )),
                        Ok(Ok(_)) => Ok(()),
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Serving listener did not settle the unknown Peer connection",
                        )),
                    }
                }
            },
        },
        Err(error) => Err(error),
    };
    let certificate = captured
        .lock()
        .expect("captured Server certificate lock is not poisoned")
        .take()
        .expect("the Serving listener presented its identity before refusing the Peer");
    (result, certificate)
}

async fn tls12_handshake_succeeds(address: std::net::SocketAddr) -> bool {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["tls12-test-client".to_owned()]).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS12])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(CaptureServerCertificate {
        certificate: std::sync::Arc::new(std::sync::Mutex::new(None)),
    }))
    .with_client_auth_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            signing_key.serialize_der(),
        )),
    )
    .unwrap();
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .is_ok()
}

fn public_key_from_certificate(certificate: &[u8]) -> Vec<u8> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate)
        .expect("parse Serving identity certificate");
    certificate.public_key().raw.to_vec()
}

async fn open_paired_health_connection(
    address: std::net::SocketAddr,
    identity_path: &std::path::Path,
    server_public_key: Vec<u8>,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let private_key = std::fs::read(identity_path).expect("read connecting Server identity");
    let signing_key =
        rcgen::KeyPair::try_from(private_key.as_slice()).expect("parse connecting Server identity");
    let certificate = rcgen::CertificateParams::new(vec!["paired-test-client".to_owned()])
        .unwrap()
        .self_signed(&signing_key)
        .unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(PinnedTestServerCertificate {
        expected_public_key: server_public_key,
    }))
    .with_client_auth_cert(
        vec![certificate.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            private_key,
        )),
    )
    .unwrap();
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .expect("open authenticated Pairing connection");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let mut response = vec![0_u8; 4096];
    let read = timeout(PROGRESS_DEADLINE, stream.read(&mut response))
        .await
        .expect("paired health responds promptly")
        .expect("read paired health response");
    assert!(String::from_utf8_lossy(&response[..read]).contains("200 OK"));
    stream
}

async fn open_invited_enrollment_connection(
    address: std::net::SocketAddr,
    invite: &str,
) -> (
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    String,
) {
    let encoded = invite
        .strip_prefix("suru-v1-")
        .expect("test Invite uses the supported version");
    let payload: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("decode test Invite"),
    )
    .expect("parse test Invite payload");
    let server_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload["k"].as_str().expect("Invite carries a Server key"))
        .expect("decode invited Server key");
    let token = payload["t"]
        .as_str()
        .expect("Invite carries a token")
        .to_owned();
    let signing_key = rcgen::KeyPair::generate().expect("generate invited Peer identity");
    let mut certificate_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    certificate_params.distinguished_name = rcgen::DistinguishedName::new();
    certificate_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("suru-invite-{token}"));
    let certificate = certificate_params.self_signed(&signing_key).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(PinnedTestServerCertificate {
        expected_public_key: server_public_key,
    }))
    .with_client_auth_cert(
        vec![certificate.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            signing_key.serialize_der(),
        )),
    )
    .unwrap();
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await
        .expect("open invited enrollment connection");
    (stream, token)
}

async fn send_enrollment_phase(
    stream: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    token: &str,
    phase: &str,
) {
    let body = serde_json::to_vec(&serde_json::json!({
        "token": token,
        "protocol_version": PROTOCOL_VERSION,
        "phase": phase,
    }))
    .unwrap();
    stream
        .write_all(
            format!(
                "POST /v1/pairing/enroll HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();

    let mut response = Vec::new();
    let (header_end, content_length) = loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "enrollment response ended before its headers");
        response.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let header_end = header_end + 4;
            let headers = std::str::from_utf8(&response[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (header_end, content_length);
        }
    };
    while response.len() < header_end + content_length {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "enrollment response ended before its body");
        response.extend_from_slice(&chunk[..read]);
    }
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "enrollment phase failed: {}",
        String::from_utf8_lossy(&response)
    );
}

fn assert_logs_omit_key_material(log_dir: &std::path::Path, public_key: &[u8]) {
    let logs = read_logs(log_dir);
    let key_hex = public_key
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let key_debug = format!("{public_key:?}");
    assert!(
        !logs.contains(&key_hex) && !logs.contains(&key_debug),
        "Server Logs must never contain identity key material"
    );
}

fn assert_logs_omit_invite_material(log_dir: &std::path::Path, invite: &str) {
    let logs = read_logs(log_dir);
    let payload = invite.strip_prefix("suru-v1-").unwrap();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    for secret in [
        invite,
        payload["k"].as_str().unwrap(),
        payload["t"].as_str().unwrap(),
    ] {
        assert!(
            !logs.contains(secret),
            "Server Logs must never contain Invite, token, or key material"
        );
    }
}

fn read_logs(log_dir: &std::path::Path) -> String {
    std::fs::read_dir(log_dir)
        .expect("read Server Log directory")
        .map(|entry| {
            std::fs::read_to_string(entry.expect("read Server Log entry").path())
                .expect("read Server Log")
        })
        .collect::<String>()
}

#[tokio::test]
async fn serving_starts_and_stops_a_second_mtls_listener_without_disturbing_local_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let config = ServerConfig::new(state_dir.path(), "serving-lifecycle-test")
        .expect("configure server")
        .with_data_dir(data_dir.path())
        .with_config_dir(config_dir.path());
    let identity_path = config.data_dir().join("server-identity.pk8");
    let log_guard = logging::init(&config, Role::Server).expect("initialize Server Log");
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    };
    let server = server::spawn_with_timings(config.clone(), timings)
        .await
        .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut local_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "serving-lifecycle-test")
            .expect("configure local client"),
    )
    .await
    .expect("attach local client");
    receive_initial_state(&mut local_client).await;

    assert_eq!(server.serving_address(), None);
    assert!(
        !identity_path.exists(),
        "a local-only Server has no Serving identity"
    );
    local_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for the Serving port");
    local_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this lifecycle walk on the loopback address it dials");
    local_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");

    let first_address = server
        .serving_address()
        .expect("Serving listener has bound before the mutation answers");
    assert_eq!(first_address.ip(), std::net::Ipv4Addr::LOCALHOST);
    assert_ne!(first_address.port(), 0);
    assert!(identity_path.exists());
    assert!(
        !tls12_handshake_succeeds(first_address).await,
        "Pairing TLS excludes TLS 1.2 so enrollment credentials stay encrypted"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(&identity_path)
                .expect("read Server identity metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    assert_windows_current_user_only(&identity_path);

    let (unknown_peer, first_certificate) = dial_with_unknown_certificate(first_address).await;
    assert!(
        unknown_peer.is_err(),
        "a dialer presenting an unenrolled client certificate must fail the TLS handshake"
    );
    let first_public_key = public_key_from_certificate(&first_certificate);
    let invite = local_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![first_address],
        })
        .await
        .expect("issue Invite without logging its credentials");
    let (uninvited_peer, _) = dial_with_unknown_certificate(first_address).await;
    assert!(
        uninvited_peer.is_err(),
        "issuing an Invite does not admit a certificate that cannot present its token"
    );

    let mut serving_connection = tokio::net::TcpStream::connect(first_address)
        .await
        .expect("hold a connection on the Serving listener");
    local_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(false) })
        .await
        .expect("turn Serving off through the still-attached local Client");
    assert_eq!(server.serving_address(), None);
    let mut byte = [0];
    match timeout(PROGRESS_DEADLINE, serving_connection.read(&mut byte))
        .await
        .expect("Serving connection is dropped promptly")
    {
        Ok(0) | Err(_) => {}
        Ok(read) => panic!("disabled Serving connection produced {read} unexpected bytes"),
    }

    let health = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("local listener remains reachable after Serving stops");
    assert_eq!(health.status(), reqwest::StatusCode::OK);

    local_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving back on through the same local Client");
    let replacement_address = server
        .serving_address()
        .expect("replacement Serving listener is ready");
    let (unknown_peer, replacement_certificate) =
        dial_with_unknown_certificate(replacement_address).await;
    assert!(unknown_peer.is_err());
    assert_eq!(
        public_key_from_certificate(&replacement_certificate),
        first_public_key,
        "Serving reuses the Server identity generated on first Serve, as observed at the wire"
    );

    drop(local_client);
    server.shutdown().await.expect("shut down server");
    drop(log_guard);
    assert_logs_omit_key_material(&config.state_dir().join("log"), &first_public_key);
    assert_logs_omit_invite_material(&config.state_dir().join("log"), &invite.invite);
}

/// An Invite advertises the machine's concrete IPv4 and IPv6 addresses, so
/// the default bind must answer dialers of both families — including on
/// Windows, which binds `::` v6-only unless Serving asks otherwise.
#[tokio::test]
async fn the_default_serving_bind_accepts_dialers_of_both_address_families() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let config = ServerConfig::new(state_dir.path(), "serving-dual-stack-test")
        .expect("configure server")
        .with_config_dir(config_dir.path());
    let server = server::spawn_with_timings(
        config,
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "serving-dual-stack-test")
            .expect("configure local client"),
    )
    .await
    .expect("attach local client");
    receive_initial_state(&mut client).await;
    client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a Serving port");
    client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on with the default bind address");

    let address = server.serving_address().expect("Serving listener is ready");
    assert_eq!(
        address.ip(),
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        "an unpinned bind address listens on every interface of both stacks"
    );
    for loopback in [
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    ] {
        tokio::net::TcpStream::connect((loopback, address.port()))
            .await
            .unwrap_or_else(|error| {
                panic!("the default Serving bind refused a {loopback} dialer: {error}")
            });
    }

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_serving_server_issues_a_one_line_invite_with_its_chosen_addresses() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let config = ServerConfig::new(state_dir.path(), "invite-issue-test")
        .expect("configure server")
        .with_config_dir(config_dir.path());
    let server = server::spawn_with_timings(
        config,
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn Server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "invite-issue-test")
            .expect("configure local Client"),
    )
    .await
    .expect("attach local Client");
    receive_initial_state(&mut client).await;
    client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a Serving port");
    client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");
    let address = server.serving_address().expect("Serving listener is ready");

    let invite = client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .expect("issue Invite through the Server's local interface");

    assert!(invite.invite.starts_with("suru-v1-"));
    assert!(!invite.invite.contains(['\r', '\n']));
    assert_eq!(invite.addresses, vec![address]);

    drop(client);
    server.shutdown().await.expect("shut down Server");
}

#[tokio::test]
async fn two_servers_form_a_pairing_and_reconnect_using_only_their_keys() {
    let serving_state = tempfile::tempdir().expect("create Serving state directory");
    let serving_config_root = tempfile::tempdir().expect("create Serving config directory");
    let serving_config = ServerConfig::new(serving_state.path(), "pairing-serving-test")
        .expect("configure Serving Server")
        .with_config_dir(serving_config_root.path());
    let serving = server::spawn_with_timings(
        serving_config,
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn Serving Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "pairing-serving-test")
            .expect("configure Serving Client"),
    )
    .await
    .expect("attach Serving Client");
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a Serving port");
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");
    let serving_address = serving
        .serving_address()
        .expect("Serving listener is ready");
    let connecting_state = tempfile::tempdir().expect("create connecting state directory");
    let connecting_data = tempfile::tempdir().expect("create connecting data directory");
    let connecting_config = ServerConfig::new(connecting_state.path(), "pairing-connecting-test")
        .expect("configure connecting Server")
        .with_data_dir(connecting_data.path());
    let connecting = server::spawn_with_timings(
        connecting_config,
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn connecting Server");
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "pairing-connecting-test")
            .expect("configure connecting Client"),
    )
    .await
    .expect("attach connecting Client");
    receive_initial_state(&mut connecting_client).await;

    let unavailable = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("reserve an unavailable first offered address");
    let unavailable_address = unavailable.local_addr().unwrap();
    drop(unavailable);
    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![unavailable_address, serving_address],
        })
        .await
        .expect("issue Invite");
    let preview = connecting_client
        .preview_invite(invite.invite.clone())
        .await
        .expect("inspect the Invite through the local Server without redeeming it");
    assert!(!preview.hostname.is_empty());
    assert_eq!(preview.fingerprint.len(), 64);
    assert_eq!(
        preview.addresses,
        vec![unavailable_address, serving_address]
    );
    assert!(connecting_client.list_remotes().await.unwrap().is_empty());
    assert!(serving_client.list_peers().await.unwrap().is_empty());
    let remote = connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("workstation".to_owned()),
            addresses: vec![serving_address, unavailable_address],
        })
        .await
        .expect("redeem Invite through the connecting Server");

    assert_eq!(remote.name, "workstation");
    assert_eq!(remote.addresses, vec![serving_address, unavailable_address]);
    assert_eq!(
        connecting_client.list_remotes().await.unwrap(),
        vec![remote]
    );
    assert_eq!(serving_client.list_peers().await.unwrap().len(), 1);
    assert_eq!(
        connecting_client
            .probe_remote("workstation")
            .await
            .expect("reconnect to the Serving Server without the Invite")
            .protocol_version,
        Some(PROTOCOL_VERSION)
    );

    let default_name_invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![serving_address],
        })
        .await
        .unwrap();
    let default_named = connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: default_name_invite.invite,
            name: None,
            addresses: Vec::new(),
        })
        .await
        .expect("default the Remote name from the Serving hostname");
    assert!(!default_named.name.is_empty());
    let duplicate_name_invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![serving_address],
        })
        .await
        .unwrap();
    let reusable_invite = duplicate_name_invite.invite.clone();
    let error = connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: duplicate_name_invite.invite,
            name: None,
            addresses: Vec::new(),
        })
        .await
        .expect_err("a duplicate hostname default is rejected before enrollment commits");
    assert_eq!(
        pairing_error_code(&error),
        SessionErrorCode::RemoteNameConflict
    );
    connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: reusable_invite,
            name: Some("other-workstation".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("a rejected default name neither spends the Invite nor enrolls a Peer");

    drop(connecting_client);
    drop(serving_client);
    connecting.shutdown().await.expect("stop connecting Server");
    serving.shutdown().await.expect("stop Serving Server");
}

struct PairedServers {
    _serving_state: tempfile::TempDir,
    _serving_config_root: tempfile::TempDir,
    _connecting_state: tempfile::TempDir,
    serving_data_dir: std::path::PathBuf,
    connecting_data_dir: std::path::PathBuf,
    connecting_identity_path: std::path::PathBuf,
    serving: server::RunningServer,
    connecting: server::RunningServer,
    serving_client: ManagedClient,
    connecting_client: ManagedClient,
    wire: ObservedTcpProxy,
    alternate_wire: Option<ObservedTcpProxy>,
}

impl PairedServers {
    async fn shutdown(self) {
        drop(self.connecting_client);
        drop(self.serving_client);
        self.connecting
            .shutdown()
            .await
            .expect("stop connecting Server");
        self.serving.shutdown().await.expect("stop Serving Server");
    }
}

struct ObservedTcpProxy {
    address: std::net::SocketAddr,
    active_connections: tokio::sync::watch::Receiver<usize>,
    opened_connections: tokio::sync::watch::Receiver<usize>,
    online: tokio::sync::watch::Sender<bool>,
    hold: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ObservedTcpProxy {
    async fn start(target: std::net::SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind observed Pairing route");
        let address = listener.local_addr().expect("read observed Pairing route");
        let (active, active_connections) = tokio::sync::watch::channel(0_usize);
        let (opened, opened_connections) = tokio::sync::watch::channel(0_usize);
        let (online, online_rx) = tokio::sync::watch::channel(true);
        let (hold, hold_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            // A held connection is kept open and never forwarded, so a dialer
            // waits on it exactly as it would on a machine that accepts and
            // then says nothing.
            let mut held = Vec::new();
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    break;
                };
                opened.send_modify(|count| *count += 1);
                if !*online_rx.borrow() {
                    if *hold_rx.borrow() {
                        held.push(inbound);
                    }
                    continue;
                }
                active.send_modify(|count| *count += 1);
                let active = active.clone();
                let mut online = online_rx.clone();
                tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                        let transfer = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
                        tokio::pin!(transfer);
                        loop {
                            tokio::select! {
                                _ = &mut transfer => break,
                                changed = online.changed() => {
                                    if changed.is_err() || !*online.borrow() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    active.send_modify(|count| *count = count.saturating_sub(1));
                });
            }
        });
        Self {
            address,
            active_connections,
            opened_connections,
            online,
            hold,
            task,
        }
    }

    /// Accepts connections and answers nothing, so a dialer's own budget is
    /// the only thing that ends the attempt.
    async fn swallow_connections(&mut self) {
        self.hold.send_replace(true);
        self.set_online(false).await;
    }

    async fn set_online(&mut self, online: bool) {
        self.online.send_replace(online);
        if !online {
            self.wait_for_connections(0).await;
        }
    }

    fn opened_connections(&self) -> usize {
        *self.opened_connections.borrow()
    }

    async fn wait_for_opened_connections(&mut self, expected: usize) {
        wait_for_counter(
            &mut self.opened_connections,
            expected,
            |actual, expected| actual >= expected,
            "open at least",
        )
        .await;
    }

    async fn wait_for_connections(&mut self, expected: usize) {
        wait_for_counter(
            &mut self.active_connections,
            expected,
            |actual, expected| actual == expected,
            "settle at",
        )
        .await;
    }
}

async fn wait_for_counter(
    counter: &mut tokio::sync::watch::Receiver<usize>,
    expected: usize,
    reached: fn(usize, usize) -> bool,
    description: &str,
) {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if reached(*counter.borrow(), expected) {
                return;
            }
            counter
                .changed()
                .await
                .expect("observed Pairing route remains open");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Pairing route should {description} {expected} connections"));
}

impl Drop for ObservedTcpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn paired_servers(name: &str) -> PairedServers {
    paired_servers_with_alternate_route(name, false).await
}

async fn paired_servers_with_alternate_route(name: &str, alternate: bool) -> PairedServers {
    paired_servers_with_runtime(name, alternate, None).await
}

async fn paired_servers_with_runtime(
    name: &str,
    alternate: bool,
    runtime: Option<std::sync::Arc<dyn suru::provider::ProviderRuntime>>,
) -> PairedServers {
    paired_servers_with_source_control(name, alternate, runtime, None, None).await
}

/// Pairs two Servers whose redeeming side gives a Remote only `timeout` to
/// acknowledge its own removal.
async fn paired_servers_with_withdrawal_timeout(name: &str, timeout: Duration) -> PairedServers {
    paired_servers_with_source_control(name, false, None, None, Some(timeout)).await
}

async fn paired_servers_with_source_control(
    name: &str,
    alternate: bool,
    runtime: Option<std::sync::Arc<dyn suru::provider::ProviderRuntime>>,
    source_control: Option<std::sync::Arc<dyn suru::source_control::SourceControl>>,
    withdrawal_timeout: Option<Duration>,
) -> PairedServers {
    let serving_state = tempfile::tempdir().expect("create Serving state directory");
    let serving_config_root = tempfile::tempdir().expect("create Serving config directory");
    let serving_channel = format!("{name}-serving");
    let config = ServerConfig::new(serving_state.path(), &serving_channel)
        .expect("configure Serving Server")
        .with_config_dir(serving_config_root.path());
    let serving_data_dir = config.data_dir().to_path_buf();
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        checkout_observation_interval: Duration::from_millis(15),
        ..ServerTimings::default()
    };
    let serving = match (runtime, source_control) {
        (Some(runtime), Some(adapter)) => {
            server::spawn_with_source_control(config, vec![runtime], timings, adapter).await
        }
        (Some(runtime), None) => {
            server::spawn_with_provider_and_timings(config, runtime, timings).await
        }
        (None, None) => server::spawn_with_timings(config, timings).await,
        (None, Some(_)) => panic!("source control fixture requires an explicit Provider"),
    }
    .expect("spawn Serving Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), &serving_channel)
            .expect("configure Serving Client"),
    )
    .await
    .expect("attach Serving Client");
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a Serving port");
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");
    let serving_address = serving
        .serving_address()
        .expect("Serving listener is ready");
    let mut wire = ObservedTcpProxy::start(serving_address).await;
    let mut alternate_wire = if alternate {
        Some(ObservedTcpProxy::start(serving_address).await)
    } else {
        None
    };

    let connecting_state = tempfile::tempdir().expect("create connecting state directory");
    let connecting_channel = format!("{name}-connecting");
    let connecting_config = ServerConfig::new(connecting_state.path(), &connecting_channel)
        .expect("configure connecting Server");
    let connecting_data_dir = connecting_config.data_dir().to_path_buf();
    let connecting_identity_path = connecting_data_dir.join("server-identity.pk8");
    // A Provider double stands in for the built-in runtimes on this side too.
    // Those launch the developer's real harness CLIs, and a Codex Errand starts
    // `codex exec` in the Session's own Execution Directory. On Windows a
    // running process holds the directory it started in, so a Session created
    // on this Server against a Worktree checkout pins that checkout, and the
    // `git worktree remove` a test performs afterwards fails with a permission
    // error. No paired test drives a Provider on the connecting Server, so none
    // needs a real one.
    let connecting = server::spawn_with_provider_and_timings(
        connecting_config,
        std::sync::Arc::new(failing_provider_support::FailingProviderRuntime),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            remote_withdrawal_timeout: withdrawal_timeout
                .unwrap_or_else(|| ServerTimings::default().remote_withdrawal_timeout),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn connecting Server");
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), &connecting_channel)
            .expect("configure connecting Client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(10)),
    )
    .await
    .expect("attach connecting Client");
    receive_initial_state(&mut connecting_client).await;
    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: std::iter::once(wire.address)
                .chain(alternate_wire.as_ref().map(|wire| wire.address))
                .collect(),
        })
        .await
        .expect("issue Invite");
    connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("workstation".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("form Pairing");
    wire.wait_for_connections(0).await;
    if let Some(alternate_wire) = &mut alternate_wire {
        alternate_wire.wait_for_connections(0).await;
    }

    PairedServers {
        _serving_state: serving_state,
        _serving_config_root: serving_config_root,
        _connecting_state: connecting_state,
        serving_data_dir,
        connecting_data_dir,
        connecting_identity_path,
        serving,
        connecting,
        serving_client,
        connecting_client,
        wire,
        alternate_wire,
    }
}

#[tokio::test]
async fn remote_proxy_creates_prompts_and_streams_a_session_on_the_serving_server() {
    let mut pair = paired_servers("remote-proxy").await;

    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let descriptor = pair.connecting.descriptor();
    let http = reqwest::Client::new();
    let remote_api = format!("{}/v1/remotes/workstation", descriptor.base_url);
    let remote_health = http
        .get(format!("{remote_api}/health"))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read the Remote's full health API through the local proxy")
        .error_for_status()
        .expect("Remote health succeeds")
        .json::<Health>()
        .await
        .expect("decode the Remote's ordinary health response");
    assert_eq!(
        remote_health.instance_id,
        pair.serving.descriptor().instance_id
    );
    pair.wire.wait_for_connections(0).await;
    let created = http
        .post(format!("{remote_api}/v1/sessions"))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Map the Remote workspace".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Remote Session through local proxy")
        .error_for_status()
        .expect("Remote Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Remote Session");
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty(),
        "the connecting Server must not create the proxied Session locally"
    );
    assert_eq!(
        pair.serving_client.list_sessions(None).await.unwrap()[0].id(),
        created.session.id
    );
    pair.wire.wait_for_connections(0).await;

    let response = http
        .get(format!(
            "{remote_api}/v1/sessions/{}/events",
            created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open Remote Session stream through local proxy")
        .error_for_status()
        .expect("Remote Session stream succeeds");
    let mut events = response.bytes_stream().eventsource();
    pair.wire.wait_for_connections(1).await;
    let snapshot = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Remote Session snapshot arrives")
        .expect("Remote Session stream remains open")
        .expect("decode Remote Session snapshot event");
    assert_eq!(snapshot.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(
        serde_json::from_str::<SessionSnapshot>(&snapshot.data)
            .expect("decode streamed Remote Session")
            .session
            .id,
        created.session.id
    );

    let admitted = http
        .post(format!(
            "{remote_api}/v1/sessions/{}/prompts",
            created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue on the Remote".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit Remote Prompt through local proxy")
        .error_for_status()
        .expect("Remote Prompt admission succeeds")
        .json::<suru::protocol::Prompt>()
        .await
        .expect("decode admitted Remote Prompt");
    let update = timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("Remote Session stream remains open")
                .expect("decode Remote Session update");
            if event.event == SESSION_UPDATED_EVENT {
                let update = serde_json::from_str::<SessionUpdate>(&event.data)
                    .expect("decode Remote Session update body");
                if update.changes.iter().any(|change| {
                    matches!(
                        change,
                        suru::protocol::SessionChange::PromptAdded { prompt }
                            if prompt.id == admitted.id
                    )
                }) {
                    break update;
                }
            }
        }
    })
    .await
    .expect("admitted Remote Prompt reaches the proxied SSE stream");
    assert!(update.revision > created.revision);

    drop(events);
    pair.wire.wait_for_connections(0).await;
    pair.shutdown().await;
}

#[tokio::test]
async fn disabling_serving_ends_a_live_peer_stream_without_disturbing_local_clients() {
    let mut pair = paired_servers("serving-disable-live-peer").await;
    let serving_address = pair
        .serving
        .serving_address()
        .expect("Serving listener is ready");
    pair.serving_client
        .mutate_setting(SettingMutation::ServingPort {
            value: Some(serving_address.port()),
        })
        .await
        .expect("keep the Serving address stable across re-enabling");
    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let created = pair
        .serving_client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Hold this Session stream open".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create a Session on the Serving Server");
    let descriptor = pair.connecting.descriptor();
    let remote_api = format!("{}/v1/remotes/workstation", descriptor.base_url);
    let http = reqwest::Client::new();
    let response = http
        .get(format!(
            "{remote_api}/v1/sessions/{}/events",
            created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the enrolled Peer's proxied Session stream")
        .error_for_status()
        .expect("the proxied Session stream succeeds");
    let mut events = response.bytes_stream().eventsource();
    let snapshot = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Remote Session snapshot arrives")
        .expect("Remote Session stream remains open")
        .expect("decode Remote Session snapshot event");
    assert_eq!(snapshot.event, SESSION_SNAPSHOT_EVENT);
    pair.wire.wait_for_connections(1).await;
    let (_, serving_certificate) = dial_with_unknown_certificate(serving_address).await;
    let mut in_flight_request = open_paired_health_connection(
        serving_address,
        &pair.connecting_identity_path,
        public_key_from_certificate(&serving_certificate),
    )
    .await;
    in_flight_request
        .write_all(
            format!(
                "POST /v1/pairing/proxy/v1/sessions HTTP/1.1\r\nHost: localhost\r\nx-suru-protocol-version: {PROTOCOL_VERSION}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
            )
            .as_bytes(),
        )
        .await
        .expect("begin an ordinary proxied request with an incomplete body");
    let mut byte = [0_u8];
    assert!(
        timeout(Duration::from_millis(25), in_flight_request.read(&mut byte))
            .await
            .is_err(),
        "the incomplete request body keeps the ordinary request in flight"
    );

    pair.serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(false) })
        .await
        .expect("turn Serving off through the local Client");
    match timeout(Duration::from_millis(100), events.next()).await {
        Ok(None | Some(Err(_))) => {}
        Ok(Some(Ok(event))) => panic!(
            "disabled Serving produced an unexpected {} event",
            event.event
        ),
        Err(_) => panic!("disabled Serving left the proxied Session request in flight"),
    }
    match timeout(
        Duration::from_millis(100),
        in_flight_request.read(&mut byte),
    )
    .await
    .expect("disabled Serving ends the ordinary proxied request")
    {
        Ok(0) | Err(_) => {}
        Ok(read) => panic!("disabled Serving produced {read} unexpected response bytes"),
    }
    pair.wire.wait_for_connections(0).await;
    assert_eq!(
        pair.serving_client
            .list_sessions(None)
            .await
            .expect("the local Client remains attached after Serving stops")[0]
            .id(),
        created.session.id
    );

    pair.serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving back on through the same local Client");
    assert_eq!(pair.serving.serving_address(), Some(serving_address));
    pair.connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()))
        .list_sessions(None)
        .await
        .expect("the existing Pairing authenticates against the reused Server identity");

    pair.shutdown().await;
}

#[tokio::test]
async fn outlook_client_runs_session_commands_and_streams_against_its_remote() {
    let (runtime, mut provider) = provider_support::ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![ModelDescriptor {
            provider: ProviderId::new("codex"),
            id: ModelId::new("gpt-test"),
            display_name: "Codex fixture".into(),
            description: String::new(),
            is_default: true,
            availability: ModelAvailability::Available,
            options: Vec::new(),
        }],
    );
    let pair = paired_servers_with_runtime("remote-outlook-client", false, Some(runtime)).await;
    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let mut catalog = remote.subscribe_catalog();
    let initial = timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("Remote catalog snapshot arrives")
        .expect("Remote catalog stream remains open");
    let ManagedEvent::SessionCatalogReconciled(snapshot) = initial else {
        panic!("Remote catalog starts with a snapshot");
    };
    let serving_descriptor = pair.serving.descriptor();
    let health = reqwest::Client::new()
        .get(format!("{}/health", serving_descriptor.base_url))
        .bearer_auth(&serving_descriptor.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Health>()
        .await
        .unwrap();
    assert_eq!(
        snapshot.workspace_paths, health.workspace_paths,
        "the proxied catalog carries the owning Server's workspace display data"
    );

    let created = remote
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Begin through the Outlook-aware Client".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create a Session on the Remote");
    let announced = timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("Remote catalog update arrives")
        .expect("Remote catalog stream remains open");
    assert!(matches!(
        announced,
        ManagedEvent::SessionCreated(created_event)
            if created_event.session_id == created.session.id
    ));
    let listed = remote
        .list_sessions(None)
        .await
        .expect("list the Remote's Sessions");
    assert_eq!(listed[0].id(), created.session.id);
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty()
    );

    let start = provider.next_start().await;
    let mut native = start.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: suru::protocol::AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-test"),
            options: Vec::new(),
        },
    });
    native.next_turn().await.succeed();
    let pinned = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Never,
        sandbox_mode: CodexSandboxMode::ReadOnly,
    };
    let updated = remote
        .update_approval_posture(
            created.session.id,
            UpdateApprovalPostureRequest {
                posture: Some(pinned),
            },
        )
        .await
        .expect("pin Approval Posture through the Remote Outlook");
    assert_eq!(updated.value, pinned);
    assert!(updated.pinned);
    native.emit(ProviderEvent::TurnCompleted);

    let mut stream = remote
        .subscribe_session(created.session.id)
        .await
        .expect("open the Remote Session stream");
    let snapshot = timeout(PROGRESS_DEADLINE, stream.next())
        .await
        .expect("Remote Session snapshot arrives")
        .expect("Remote Session stream remains open")
        .expect("decode Remote Session snapshot");
    assert!(matches!(
        snapshot,
        suru::managed_client::SessionEvent::Snapshot(snapshot)
            if snapshot.session.id == created.session.id
    ));

    drop(stream);
    drop(catalog);
    pair.shutdown().await;
}

#[tokio::test]
async fn successive_remote_requests_reuse_transport_while_an_outlook_holds_interest() {
    let mut pair = paired_servers("remote-transport-reuse").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let mut catalog = remote.subscribe_catalog();
    timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("Remote catalog snapshot arrives")
        .expect("Remote catalog interest remains live");
    pair.wire.wait_for_connections(1).await;

    remote
        .list_sessions(None)
        .await
        .expect("first Remote listing succeeds");
    let connections_after_first_listing = pair.wire.opened_connections();
    remote
        .list_sessions(None)
        .await
        .expect("second Remote listing succeeds");
    assert_eq!(
        pair.wire.opened_connections(),
        connections_after_first_listing,
        "successive requests reuse the Outlook's Pairing transport"
    );

    drop(catalog);
    pair.wire.wait_for_connections(0).await;
    pair.shutdown().await;
}

#[tokio::test]
async fn catalog_subscriptions_hold_independent_interest_in_two_remotes() {
    let mut pair = paired_servers("independent-remote-catalog-interest").await;

    let laptop_state = tempfile::tempdir().expect("create laptop state directory");
    let laptop_config_root = tempfile::tempdir().expect("create laptop config directory");
    let laptop_channel = "independent-remote-catalog-interest-laptop";
    let laptop = server::spawn_with_timings(
        ServerConfig::new(laptop_state.path(), laptop_channel)
            .expect("configure laptop Server")
            .with_config_dir(laptop_config_root.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn laptop Server");
    let mut laptop_client = ManagedClient::connect(
        ManagedClientConfig::new(laptop_state.path(), laptop_channel)
            .expect("configure laptop Client"),
    )
    .await
    .expect("attach laptop Client");
    receive_initial_state(&mut laptop_client).await;
    laptop_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a laptop Serving port");
    laptop_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep the laptop route on loopback");
    laptop_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn laptop Serving on");
    let mut laptop_wire = ObservedTcpProxy::start(
        laptop
            .serving_address()
            .expect("laptop Serving listener is ready"),
    )
    .await;
    let invite = laptop_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![laptop_wire.address],
        })
        .await
        .expect("issue laptop Invite");
    pair.connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("laptop".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("pair the connecting Server with the laptop");
    laptop_wire.wait_for_connections(0).await;

    let mut workstation_catalog = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()))
        .subscribe_catalog();
    let laptop_outlook = pair
        .connecting_client
        .outlook(Outlook::Remote("laptop".to_owned()));
    let mut laptop_catalog = laptop_outlook.subscribe_catalog();
    for mut catalog in [&mut workstation_catalog, &mut laptop_catalog] {
        assert!(matches!(
            timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
                .await
                .expect("Remote catalog snapshot arrives"),
            Some(ManagedEvent::SessionCatalogReconciled(_))
        ));
    }
    pair.wire.wait_for_connections(1).await;
    laptop_wire.wait_for_connections(1).await;

    pair.wire.set_online(false).await;
    assert!(matches!(
        timeout(
            PROGRESS_DEADLINE,
            next_session_catalog_event(&mut workstation_catalog)
        )
        .await
        .expect("workstation catalog announces recovery"),
        Some(ManagedEvent::Recovering(_))
    ));
    laptop_wire.wait_for_connections(1).await;
    let workspace = tempfile::tempdir().expect("create laptop Workspace");
    let created = laptop_outlook
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep the second catalog live".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create a Session while the other Remote recovers");
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut laptop_catalog))
            .await
            .expect("the undisturbed laptop catalog reports its update"),
        Some(ManagedEvent::SessionCreated(event)) if event.session_id == created.session.id
    ));

    pair.wire.set_online(true).await;
    let recovered = timeout(PROGRESS_DEADLINE, async {
        loop {
            match next_session_catalog_event(&mut workstation_catalog).await {
                Some(ManagedEvent::Recovering(_)) => {}
                event => return event,
            }
        }
    })
    .await
    .expect("workstation catalog reconnects independently");
    assert!(matches!(
        recovered,
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    pair.wire.wait_for_connections(1).await;

    drop(workstation_catalog);
    pair.wire.wait_for_connections(0).await;
    drop(laptop_catalog);
    laptop_wire.wait_for_connections(0).await;

    drop(laptop_client);
    laptop.shutdown().await.expect("stop laptop Server");
    pair.shutdown().await;
}

#[tokio::test]
async fn remote_requests_remember_the_last_route_that_answered() {
    let mut pair = paired_servers_with_alternate_route("remote-last-good-route", true).await;
    pair.wire.set_online(false).await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));

    remote
        .list_sessions(None)
        .await
        .expect("Remote listing falls back to its alternate route");
    assert!(
        pair.alternate_wire
            .as_ref()
            .expect("the test Pairing has an alternate route")
            .opened_connections()
            > 0,
        "the fallback route answered the first listing"
    );
    let preferred_attempts_after_fallback = pair.wire.opened_connections();
    pair.wire.set_online(true).await;
    remote
        .list_sessions(None)
        .await
        .expect("Remote listing keeps using the last route that answered");
    assert_eq!(
        pair.wire.opened_connections(),
        preferred_attempts_after_fallback,
        "restoring an earlier route does not displace the last-known-good route"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_remote_catalog_interest_retries_a_transient_drop_with_injected_backoff() {
    let mut pair = paired_servers("remote-transient-recovery").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let mut catalog = remote.subscribe_catalog();
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
            .await
            .expect("Remote catalog snapshot arrives"),
        Some(ManagedEvent::SessionCatalogReconciled(_))
    ));
    pair.wire.wait_for_connections(1).await;

    pair.wire.set_online(false).await;
    assert_eq!(
        timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
            .await
            .expect("transient drop announces recovery")
            .expect("Remote catalog interest remains live"),
        ManagedEvent::Recovering(suru::managed_client::RecoveryStatus {
            attempt: 1,
            retry_in: Duration::from_millis(5),
        })
    );

    pair.wire.set_online(true).await;
    let first_restored_event = timeout(PROGRESS_DEADLINE, async {
        loop {
            match next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::Recovering(_)) => {}
                event => return event,
            }
        }
    })
    .await
    .expect("Remote catalog reconnects when the route returns");
    assert!(
        matches!(
            first_restored_event,
            Some(ManagedEvent::SessionCatalogReconciled(_))
        ),
        "the recovered catalog snapshot lands before reconnect presentation clears"
    );
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
            .await
            .expect("reconnect presentation clears after catalog hydration"),
        Some(ManagedEvent::RemoteRecovered)
    ));
    pair.wire.wait_for_connections(1).await;

    drop(catalog);
    pair.wire.wait_for_connections(0).await;
    pair.shutdown().await;
}

#[tokio::test]
async fn dropping_remote_catalog_interest_stops_its_retry_loop() {
    let mut pair = paired_servers("remote-interest-release").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let mut catalog = remote.subscribe_catalog();
    timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("Remote catalog snapshot arrives")
        .expect("Remote catalog interest remains live");
    pair.wire.wait_for_connections(1).await;

    pair.wire.set_online(false).await;
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
            .await
            .expect("transient drop announces recovery"),
        Some(ManagedEvent::Recovering(_))
    ));
    drop(catalog);
    tokio::time::sleep(Duration::from_millis(15)).await;
    let attempts_after_release = pair.wire.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        pair.wire.opened_connections(),
        attempts_after_release,
        "no Remote dial continues after the Client releases its interest"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn revocation_stops_remote_catalog_retries_and_surfaces_a_terminal_status() {
    let mut pair = paired_servers("remote-live-revocation").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let mut catalog = remote.subscribe_catalog();
    timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("Remote catalog snapshot arrives")
        .expect("Remote catalog interest remains live");
    pair.wire.wait_for_connections(1).await;

    let peer = pair.serving_client.list_peers().await.unwrap().remove(0);
    pair.serving_client.remove_peer(&peer.id).await.unwrap();
    let mut saw_recovery = false;
    let failure = timeout(PROGRESS_DEADLINE, async {
        loop {
            match next_session_catalog_event(&mut catalog).await {
                Some(ManagedEvent::Recovering(_)) => saw_recovery = true,
                Some(ManagedEvent::RemoteFailed { status, message }) => return (status, message),
                Some(_) => {}
                None => panic!("terminal Remote failure closed without a status"),
            }
        }
    })
    .await
    .expect("revocation is classified without an unbounded retry loop");
    assert!(
        saw_recovery,
        "the dropped live link uses reconnect presentation first"
    );
    assert_eq!(failure.0, RemoteStatus::Revoked);
    assert!(failure.1.contains("revoked"));
    assert_eq!(
        pair.connecting_client.list_remotes().await.unwrap()[0].status,
        RemoteStatus::Revoked,
        "terminal failure marks the durable Remote record"
    );
    assert_eq!(
        pair.connecting_client
            .probe_remote("workstation")
            .await
            .expect("read the marked Remote status")
            .status,
        RemoteStatus::Revoked
    );
    let attempts_after_failure = pair.wire.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(pair.wire.opened_connections(), attempts_after_failure);

    drop(catalog);
    pair.shutdown().await;
}

#[tokio::test]
async fn revocation_stops_retries_when_a_remote_session_is_the_only_interest() {
    let mut pair = paired_servers("remote-session-revocation").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let created = remote
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Hold only this Remote Session".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Remote Session");
    let mut session = remote
        .attach_session(created.session.id)
        .await
        .expect("attach Remote Session");
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, session.next())
            .await
            .expect("Remote Session snapshot arrives")
            .expect("Remote Session interest remains live")
            .expect("decode Remote Session snapshot"),
        suru::managed_client::SessionEvent::Snapshot(_)
    ));
    pair.wire.wait_for_connections(1).await;

    let peer = pair.serving_client.list_peers().await.unwrap().remove(0);
    pair.serving_client.remove_peer(&peer.id).await.unwrap();
    let error = timeout(PROGRESS_DEADLINE, session.next())
        .await
        .expect("revocation settles the Remote Session retry loop")
        .expect("terminal failure is delivered before the stream closes")
        .expect_err("revoked Remote Session cannot recover");
    assert_eq!(error.remote_status(), Some(RemoteStatus::Revoked));
    assert!(
        timeout(Duration::from_millis(50), session.next())
            .await
            .expect("terminal Remote Session stream closes")
            .is_none()
    );
    let attempts_after_failure = pair.wire.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(pair.wire.opened_connections(), attempts_after_failure);

    drop(session);
    pair.shutdown().await;
}

#[tokio::test]
async fn a_remote_session_as_the_only_interest_recovers_with_the_injected_backoff() {
    let mut pair = paired_servers("remote-session-recovery").await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let created = remote
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Recover this Remote Session".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Remote Session");
    let mut session = remote
        .attach_session(created.session.id)
        .await
        .expect("attach Remote Session");
    timeout(PROGRESS_DEADLINE, session.next())
        .await
        .expect("Remote Session snapshot arrives")
        .expect("Remote Session interest remains live")
        .expect("decode Remote Session snapshot");
    pair.wire.wait_for_connections(1).await;

    pair.wire.set_online(false).await;
    let opened_before_recovery = pair.wire.opened_connections();
    pair.wire
        .wait_for_opened_connections(opened_before_recovery + 2)
        .await;
    pair.wire.set_online(true).await;
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, session.next())
            .await
            .expect("Remote Session reconnects when its route returns")
            .expect("Remote Session interest remains live")
            .expect("decode rehydrated Remote Session snapshot"),
        suru::managed_client::SessionEvent::Snapshot(_)
    ));
    pair.wire.wait_for_connections(1).await;

    drop(session);
    pair.wire.wait_for_connections(0).await;
    let attempts_after_release = pair.wire.opened_connections();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(pair.wire.opened_connections(), attempts_after_release);
    pair.shutdown().await;
}

#[tokio::test]
async fn outlook_client_resolves_workspace_paths_on_its_remote() {
    let pair = paired_servers("remote-workspace-resolution").await;
    let root = tempfile::tempdir().expect("create Remote Workspace root");
    let initialized = std::process::Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(["init", "-b", "main"])
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).expect("create nested Remote Workspace");
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));

    let resolved = remote
        .resolve_workspace(ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: Some(root.path().to_owned()),
            path: "nested".into(),
        })
        .await
        .expect("resolve the path on the Remote");

    assert_eq!(
        resolved.workspace.path,
        suru::paths::canonical(root.path()).expect("read canonical Repository root")
    );
    assert_eq!(
        resolved.execution_directory.unwrap().path,
        suru::paths::canonical(nested).unwrap()
    );
    assert!(resolved.workspace.repository.is_some());
    assert!(matches!(
        resolved.checkouts[0].revision,
        Some(suru::protocol::CheckoutRevision::Branch { commit: None, .. })
    ));
    let selected = remote
        .resolve_workspace(ResolveWorkspaceRequest {
            workspace_id: Some(resolved.workspace.id.clone()),
            checkout_id: Some(resolved.checkouts[0].association.id.clone()),
            remembered_execution_directory: None,
            base: None,
            path: resolved.workspace.path.clone(),
        })
        .await
        .expect("select the Remote's main Worktree");
    assert_eq!(
        selected.execution_directory.as_ref().unwrap().path,
        resolved.workspace.path
    );
    let created = remote
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: selected.execution_directory.unwrap(),
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use selected Remote checkout".to_owned(),
                skill_invocations: vec![],
            },
        })
        .await
        .unwrap();
    assert_eq!(created.session.workspace.id, resolved.workspace.id);
    assert_eq!(
        created.session.execution_directory.path,
        resolved.workspace.path
    );
    assert_eq!(remote.list_sessions(None).await.unwrap().len(), 1);
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty()
    );
    pair.shutdown().await;
}

#[tokio::test]
async fn remote_proxy_refuses_server_administration_routes_to_peers() {
    let pair = paired_servers("remote-admin").await;

    let descriptor = pair.connecting.descriptor();
    let remote_api = format!("{}/v1/remotes/workstation", descriptor.base_url);
    let http = reqwest::Client::new();
    let settings = http
        .post(format!("{remote_api}/v1/settings"))
        .bearer_auth(&descriptor.token)
        .json(&SettingMutation::ServingEnabled { value: Some(false) })
        .send()
        .await
        .expect("attempt Remote Settings mutation");
    assert_eq!(settings.status(), reqwest::StatusCode::FORBIDDEN);

    let stop = http
        .post(format!("{remote_api}/v1/server/stop"))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: pair.serving.descriptor().instance_id,
            reason: ShutdownReason::Manual,
        })
        .send()
        .await
        .expect("attempt Remote Server stop");
    assert_eq!(stop.status(), reqwest::StatusCode::FORBIDDEN);

    let peers = http
        .get(format!("{remote_api}/v1/pairing/peers"))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("attempt Remote Peer listing");
    assert_eq!(peers.status(), reqwest::StatusCode::FORBIDDEN);
    let invites = http
        .post(format!("{remote_api}/v1/pairing/invites"))
        .bearer_auth(&descriptor.token)
        .json(&IssueInviteRequest {
            addresses: Vec::new(),
        })
        .send()
        .await
        .expect("attempt Remote Invite issuance");
    assert_eq!(invites.status(), reqwest::StatusCode::FORBIDDEN);
    let remotes = http
        .get(format!("{remote_api}/v1/pairing/remotes"))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("attempt Remote Pairing management");
    assert_eq!(remotes.status(), reqwest::StatusCode::FORBIDDEN);
    let normalized_settings = http
        .post(format!("{remote_api}/ordinary/%2e%2e/v1/settings"))
        .bearer_auth(&descriptor.token)
        .json(&SettingMutation::ServingEnabled { value: Some(false) })
        .send()
        .await
        .expect("attempt encoded Remote Settings mutation");
    assert_eq!(normalized_settings.status(), reqwest::StatusCode::FORBIDDEN);
    let peer_id = &pair.serving_client.list_peers().await.unwrap()[0].id;
    let removal = http
        .delete(format!("{remote_api}/v1/pairing/peers/{peer_id}"))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("attempt Remote Peer removal");
    assert_eq!(removal.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        pair.serving_client
            .probe_remote("missing")
            .await
            .expect_err("the Serving Server has no Remotes")
            .downcast_ref::<SessionError>()
            .expect("the local API error remains typed")
            .code,
        SessionErrorCode::RemoteNotFound,
        "the refused stop must leave the Serving Server running"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_paired_server_protocol_mismatch_is_status_and_refuses_remote_api_use() {
    let serving_state = tempfile::tempdir().expect("create Serving state directory");
    let serving_data = tempfile::tempdir().expect("create Serving data directory");
    let serving_config_root = tempfile::tempdir().expect("create Serving config directory");
    let serving_config = ServerConfig::new(serving_state.path(), "remote-version-serving")
        .expect("configure Serving Server")
        .with_data_dir(serving_data.path())
        .with_config_dir(serving_config_root.path());
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    };
    let serving = server::spawn_with_timings(serving_config.clone(), timings)
        .await
        .expect("spawn Serving Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "remote-version-serving")
            .expect("configure Serving Client"),
    )
    .await
    .expect("attach Serving Client");
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for a Serving port");
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");
    let serving_address = serving
        .serving_address()
        .expect("Serving listener is ready");
    serving_client
        .mutate_setting(SettingMutation::ServingPort {
            value: Some(serving_address.port()),
        })
        .await
        .expect("pin the bound port for the incompatible restart");

    let connecting_state = tempfile::tempdir().expect("create connecting state directory");
    let connecting = server::spawn_with_timings(
        ServerConfig::new(connecting_state.path(), "remote-version-connecting")
            .expect("configure connecting Server"),
        timings,
    )
    .await
    .expect("spawn connecting Server");
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "remote-version-connecting")
            .expect("configure connecting Client"),
    )
    .await
    .expect("attach connecting Client");
    receive_initial_state(&mut connecting_client).await;
    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![serving_address],
        })
        .await
        .expect("issue Invite");
    connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("workstation".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("form Pairing");

    drop(serving_client);
    serving
        .shutdown()
        .await
        .expect("stop original Serving Server");
    let incompatible = server::spawn_with_timings(
        serving_config,
        ServerTimings {
            pairing_protocol_version: PROTOCOL_VERSION + 1,
            ..timings
        },
    )
    .await
    .expect("restart Serving Server with a newer Pairing protocol");

    let descriptor = connecting.descriptor();
    let http = reqwest::Client::new();
    let refused = http
        .get(format!(
            "{}/v1/remotes/workstation/v1/sessions",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("attempt to use mismatched Remote API");
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        refused
            .json::<SessionError>()
            .await
            .expect("decode mismatch refusal")
            .code,
        SessionErrorCode::PairingProtocolMismatch
    );
    assert_eq!(
        connecting_client.list_remotes().await.unwrap()[0].status,
        RemoteStatus::ProtocolMismatch,
        "the refused API response marks the durable Remote status"
    );

    let status = connecting_client
        .probe_remote("workstation")
        .await
        .expect("the Remotes picker can still probe a mismatched Remote");
    assert_eq!(status.protocol_version, Some(PROTOCOL_VERSION + 1));
    assert_eq!(status.status, RemoteStatus::ProtocolMismatch);

    let mut catalog = connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()))
        .subscribe_catalog();
    let terminal = timeout(PROGRESS_DEADLINE, next_session_catalog_event(&mut catalog))
        .await
        .expect("protocol mismatch answers without retrying")
        .expect("Remote catalog reports its terminal status");
    assert!(matches!(
        terminal,
        ManagedEvent::RemoteFailed {
            status: RemoteStatus::ProtocolMismatch,
            ..
        }
    ));
    assert!(
        timeout(
            Duration::from_millis(30),
            next_session_catalog_event(&mut catalog)
        )
        .await
        .expect("terminal Remote catalog closes promptly")
        .is_none(),
        "a terminal protocol mismatch schedules no retry event"
    );

    drop(connecting_client);
    connecting.shutdown().await.expect("stop connecting Server");
    incompatible
        .shutdown()
        .await
        .expect("stop incompatible Server");
}

#[tokio::test]
async fn malformed_foreign_superseded_spent_and_expired_invites_have_precise_errors() {
    let serving_state = tempfile::tempdir().expect("create Serving state directory");
    let serving_config_root = tempfile::tempdir().expect("create Serving config directory");
    let serving = server::spawn_with_timings(
        ServerConfig::new(serving_state.path(), "invite-errors-serving")
            .expect("configure Serving Server")
            .with_config_dir(serving_config_root.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn Serving Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "invite-errors-serving")
            .expect("configure Serving Client"),
    )
    .await
    .expect("attach Serving Client");
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let address = serving.serving_address().unwrap();

    let connecting_state = tempfile::tempdir().expect("create connecting state directory");
    let connecting = server::spawn_with_timings(
        ServerConfig::new(connecting_state.path(), "invite-errors-connecting")
            .expect("configure connecting Server"),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn connecting Server");
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "invite-errors-connecting")
            .expect("configure connecting Client"),
    )
    .await
    .expect("attach connecting Client");
    receive_initial_state(&mut connecting_client).await;

    for (invite, expected) in [
        ("not-an-invite", SessionErrorCode::InvalidInvite),
        ("suru-v2-e30", SessionErrorCode::UnsupportedInviteVersion),
    ] {
        let preview_error = connecting_client
            .preview_invite(invite)
            .await
            .expect_err("invalid Invite preview is rejected");
        assert_eq!(pairing_error_code(&preview_error), expected);
        let error = connecting_client
            .redeem_invite(RedeemInviteRequest {
                invite: invite.to_owned(),
                name: Some("unused".to_owned()),
                addresses: Vec::new(),
            })
            .await
            .expect_err("invalid Invite is rejected");
        assert_eq!(pairing_error_code(&error), expected);
    }

    let superseded = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();
    let live = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();
    let error = connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: superseded.invite,
            name: Some("superseded".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("older Invite is superseded");
    assert_eq!(
        pairing_error_code(&error),
        SessionErrorCode::InviteSuperseded
    );

    connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: live.invite.clone(),
            name: Some("first".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("first redemption spends Invite");
    let fresh_state = tempfile::tempdir().expect("create fresh connecting state directory");
    let fresh = server::spawn_with_timings(
        ServerConfig::new(fresh_state.path(), "invite-errors-fresh").unwrap(),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn fresh connecting Server");
    let mut fresh_client = ManagedClient::connect(
        ManagedClientConfig::new(fresh_state.path(), "invite-errors-fresh").unwrap(),
    )
    .await
    .expect("attach fresh connecting Client");
    receive_initial_state(&mut fresh_client).await;
    let error = fresh_client
        .redeem_invite(RedeemInviteRequest {
            invite: live.invite,
            name: Some("second".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("second redemption is rejected");
    assert_eq!(pairing_error_code(&error), SessionErrorCode::InviteSpent);

    // Expiry is the one error here that wants a short-lived Invite, and it wants
    // one of its own. Every other Invite in this test has to outlive the
    // redemptions and Server spawns between issuing it and using it, which takes
    // longer on a loaded machine than any lifetime short enough to wait out, so
    // pinning them all to the same brief `invite_ttl` made whichever redemption
    // lost the race report an expired Invite instead of the error it names. This
    // Server exists only to issue an Invite already past its lifetime; waiting
    // longer than intended can only make it more expired.
    let expiring_state = tempfile::tempdir().expect("create expiring state directory");
    let expiring_config_root = tempfile::tempdir().expect("create expiring config directory");
    let expiring = server::spawn_with_timings(
        ServerConfig::new(expiring_state.path(), "invite-errors-expiring")
            .expect("configure expiring Server")
            .with_config_dir(expiring_config_root.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            invite_ttl: Duration::from_millis(20),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn expiring Server");
    let mut expiring_client = ManagedClient::connect(
        ManagedClientConfig::new(expiring_state.path(), "invite-errors-expiring")
            .expect("configure expiring Client"),
    )
    .await
    .expect("attach expiring Client");
    receive_initial_state(&mut expiring_client).await;
    expiring_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    expiring_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    expiring_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let expired = expiring_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![expiring.serving_address().unwrap()],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    let error = fresh_client
        .redeem_invite(RedeemInviteRequest {
            invite: expired.invite,
            name: Some("expired".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("expired Invite is rejected");
    assert_eq!(pairing_error_code(&error), SessionErrorCode::InviteExpired);

    let incompatible = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();
    let incompatible_state = tempfile::tempdir().unwrap();
    let incompatible_server = server::spawn_with_timings(
        ServerConfig::new(incompatible_state.path(), "invite-errors-incompatible").unwrap(),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            pairing_protocol_version: PROTOCOL_VERSION + 1,
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut incompatible_client = ManagedClient::connect(
        ManagedClientConfig::new(incompatible_state.path(), "invite-errors-incompatible").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut incompatible_client).await;
    let error = incompatible_client
        .redeem_invite(RedeemInviteRequest {
            invite: incompatible.invite,
            name: Some("incompatible".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("a protocol mismatch refuses the Pairing");
    assert_eq!(
        pairing_error_code(&error),
        SessionErrorCode::PairingProtocolMismatch
    );
    assert!(incompatible_client.list_remotes().await.unwrap().is_empty());
    drop(incompatible_client);
    incompatible_server.shutdown().await.unwrap();

    drop(connecting_client);
    drop(fresh_client);
    drop(expiring_client);
    drop(serving_client);
    connecting.shutdown().await.unwrap();
    fresh.shutdown().await.unwrap();
    expiring.shutdown().await.unwrap();
    serving.shutdown().await.unwrap();
}

fn pairing_error_code(error: &anyhow::Error) -> SessionErrorCode {
    error
        .downcast_ref::<SessionError>()
        .expect("Pairing error remains typed at the local Client interface")
        .code
}

#[tokio::test]
async fn serving_persistence_failure_does_not_leave_an_authorized_peer() {
    let serving_state = tempfile::tempdir().unwrap();
    let serving_data = tempfile::tempdir().unwrap();
    let serving_config_root = tempfile::tempdir().unwrap();
    let serving = server::spawn_with_timings(
        ServerConfig::new(serving_state.path(), "pairing-persistence-serving")
            .unwrap()
            .with_data_dir(serving_data.path())
            .with_config_dir(serving_config_root.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "pairing-persistence-serving").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let address = serving.serving_address().unwrap();

    let connecting_state = tempfile::tempdir().unwrap();
    let connecting = server::spawn_with_timings(
        ServerConfig::new(connecting_state.path(), "pairing-persistence-connecting").unwrap(),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "pairing-persistence-connecting")
            .unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut connecting_client).await;

    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();
    std::fs::create_dir(
        serving_data
            .path()
            .join("pairing-persistence-serving")
            .join("peers.json"),
    )
    .expect("make the Peer record path unwritable on every platform");
    let error = connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("must-not-pair".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("Serving-side persistence failure aborts enrollment");

    assert_eq!(
        pairing_error_code(&error),
        SessionErrorCode::PairingConnectionFailed
    );
    assert!(serving_client.list_peers().await.unwrap().is_empty());
    assert!(connecting_client.list_remotes().await.unwrap().is_empty());

    drop(connecting_client);
    drop(serving_client);
    connecting.shutdown().await.unwrap();
    serving.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_a_peer_closes_the_connection_that_enrolled_it() {
    let serving_state = tempfile::tempdir().unwrap();
    let serving_config_root = tempfile::tempdir().unwrap();
    let serving = server::spawn_with_timings(
        ServerConfig::new(serving_state.path(), "enrollment-revocation-serving")
            .unwrap()
            .with_config_dir(serving_config_root.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "enrollment-revocation-serving").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let address = serving.serving_address().unwrap();
    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();

    let (mut enrollment_connection, token) =
        open_invited_enrollment_connection(address, &invite.invite).await;
    send_enrollment_phase(&mut enrollment_connection, &token, "prepare").await;
    send_enrollment_phase(&mut enrollment_connection, &token, "commit").await;
    let peer = serving_client.list_peers().await.unwrap().remove(0);
    serving_client.remove_peer(&peer.id).await.unwrap();

    let mut byte = [0_u8];
    match timeout(PROGRESS_DEADLINE, enrollment_connection.read(&mut byte))
        .await
        .expect("removing the Peer promptly closes its enrollment connection")
    {
        Ok(0) | Err(_) => {}
        Ok(read) => panic!("revoked enrollment connection produced {read} unexpected bytes"),
    }

    drop(serving_client);
    serving.shutdown().await.unwrap();
}

#[tokio::test]
async fn pairing_records_survive_restart_and_removing_the_peer_ends_the_pairing() {
    let reserved = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("reserve a stable Serving port");
    let serving_port = reserved.local_addr().unwrap().port();
    drop(reserved);

    let serving_state = tempfile::tempdir().expect("create Serving state directory");
    let serving_data = tempfile::tempdir().expect("create Serving data directory");
    let serving_config_root = tempfile::tempdir().expect("create Serving config directory");
    let serving_config = ServerConfig::new(serving_state.path(), "durable-pairing-serving")
        .expect("configure Serving Server")
        .with_data_dir(serving_data.path())
        .with_config_dir(serving_config_root.path());
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    };
    let serving = server::spawn_with_timings(serving_config.clone(), timings)
        .await
        .expect("spawn Serving Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "durable-pairing-serving")
            .expect("configure Serving Client"),
    )
    .await
    .expect("attach Serving Client");
    receive_initial_state(&mut serving_client).await;
    serving_client
        .mutate_setting(SettingMutation::ServingPort {
            value: Some(serving_port),
        })
        .await
        .unwrap();
    serving_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    serving_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let address = serving.serving_address().unwrap();
    let (unknown_peer, serving_certificate) = dial_with_unknown_certificate(address).await;
    assert!(unknown_peer.is_err());
    let serving_public_key = public_key_from_certificate(&serving_certificate);

    let connecting_state = tempfile::tempdir().expect("create connecting state directory");
    let connecting_data = tempfile::tempdir().expect("create connecting data directory");
    let connecting_config =
        ServerConfig::new(connecting_state.path(), "durable-pairing-connecting")
            .expect("configure connecting Server")
            .with_data_dir(connecting_data.path());
    let connecting = server::spawn_with_timings(connecting_config.clone(), timings)
        .await
        .expect("spawn connecting Server");
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "durable-pairing-connecting")
            .expect("configure connecting Client"),
    )
    .await
    .expect("attach connecting Client");
    receive_initial_state(&mut connecting_client).await;

    let invite = serving_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![address],
        })
        .await
        .unwrap();
    connecting_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("durable".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect("form Pairing");
    let peer = serving_client.list_peers().await.unwrap().remove(0);

    drop(connecting_client);
    drop(serving_client);
    connecting.shutdown().await.unwrap();
    serving.shutdown().await.unwrap();

    let serving = server::spawn_with_timings(serving_config.clone(), timings)
        .await
        .expect("restart Serving Server");
    let connecting = server::spawn_with_timings(connecting_config.clone(), timings)
        .await
        .expect("restart connecting Server");
    let mut serving_client = ManagedClient::connect(
        ManagedClientConfig::new(serving_state.path(), "durable-pairing-serving")
            .expect("configure restarted Serving Client"),
    )
    .await
    .unwrap();
    receive_initial_state(&mut serving_client).await;
    let mut connecting_client = ManagedClient::connect(
        ManagedClientConfig::new(connecting_state.path(), "durable-pairing-connecting")
            .expect("configure restarted connecting Client"),
    )
    .await
    .unwrap();
    receive_initial_state(&mut connecting_client).await;

    assert_eq!(
        serving_client.list_peers().await.unwrap(),
        vec![peer.clone()]
    );
    assert_eq!(
        connecting_client.list_remotes().await.unwrap()[0].name,
        "durable"
    );
    connecting_client
        .probe_remote("durable")
        .await
        .expect("reconnect after both Servers restart using keys alone");
    let mut live_connection = open_paired_health_connection(
        serving.serving_address().unwrap(),
        &connecting_config.data_dir().join("server-identity.pk8"),
        serving_public_key,
    )
    .await;

    for path in [
        serving_config.data_dir().join("server-identity.pk8"),
        serving_config.data_dir().join("peers.json"),
        connecting_config.data_dir().join("server-identity.pk8"),
        connecting_config.data_dir().join("remotes.json"),
    ] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        #[cfg(windows)]
        assert_windows_current_user_only(path);
    }

    serving_client
        .remove_peer(&peer.id)
        .await
        .expect("remove Peer through the Serving Server");
    assert!(serving_client.list_peers().await.unwrap().is_empty());
    let mut byte = [0_u8];
    match timeout(PROGRESS_DEADLINE, live_connection.read(&mut byte))
        .await
        .expect("removing a Peer promptly ends its live connections")
    {
        Ok(0) | Err(_) => {}
        Ok(read) => panic!("revoked Pairing produced {read} unexpected bytes"),
    }

    let revoked_peers = serving_config.data_dir().join("revoked-peers.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&revoked_peers)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[cfg(windows)]
    assert_windows_current_user_only(revoked_peers);

    drop(serving_client);
    serving.shutdown().await.unwrap();
    let serving = server::spawn_with_timings(serving_config.clone(), timings)
        .await
        .expect("restart Serving Server after revocation");
    assert_eq!(
        connecting_client
            .probe_remote("durable")
            .await
            .expect("persisted revocation remains a terminal status")
            .status,
        RemoteStatus::Revoked,
        "deleting the Peer ends its Pairing across a Serving restart"
    );

    drop(connecting_client);
    connecting.shutdown().await.unwrap();
    serving.shutdown().await.unwrap();
}

fn pairing_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("Pairing records are a JSON array"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read Pairing records {path:?}: {error}"),
    }
}

fn revoked_peer_ids(data_dir: &std::path::Path) -> Vec<String> {
    pairing_records(&data_dir.join("revoked-peers.json"))
        .iter()
        .map(|id| {
            id.as_str()
                .expect("revoked Peer tombstones are identifiers")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn removing_a_remote_withdraws_its_peer_record_without_revoking_it() {
    let pair = paired_servers("remote-removal").await;
    let peer = pair.serving_client.list_peers().await.unwrap().remove(0);

    let removal = pair
        .connecting_client
        .remove_remote("workstation")
        .await
        .expect("remove the Remote through the local Server");

    assert_eq!(
        removal,
        RemoteRemoval {
            name: "workstation".to_owned(),
            acknowledged: true
        },
        "a Remote that answers ends the Pairing on its side too"
    );
    assert!(
        pair.connecting_client
            .list_remotes()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(pairing_records(&pair.connecting_data_dir.join("remotes.json")).is_empty());
    assert!(pair.serving_client.list_peers().await.unwrap().is_empty());
    assert!(pairing_records(&pair.serving_data_dir.join("peers.json")).is_empty());
    assert!(
        !revoked_peer_ids(&pair.serving_data_dir).contains(&peer.id),
        "withdrawing is not revocation: pairing again takes only a new Invite"
    );
    let error = pair
        .connecting_client
        .probe_remote("workstation")
        .await
        .expect_err("a removed Remote is no longer known");
    assert_eq!(pairing_error_code(&error), SessionErrorCode::RemoteNotFound);

    pair.shutdown().await;
}

#[tokio::test]
async fn removing_a_remote_that_never_answers_forgets_it_here_all_the_same() {
    let mut pair = paired_servers_with_withdrawal_timeout(
        "remote-removal-unanswered",
        Duration::from_millis(50),
    )
    .await;
    pair.wire.swallow_connections().await;

    let removal = pair
        .connecting_client
        .remove_remote("workstation")
        .await
        .expect("an unreachable Remote is not an error to the caller");

    assert!(
        !removal.acknowledged,
        "a Remote that says nothing within the budget goes unacknowledged"
    );
    assert!(
        pair.connecting_client
            .list_remotes()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(pairing_records(&pair.connecting_data_dir.join("remotes.json")).is_empty());
    assert_eq!(
        pair.serving_client.list_peers().await.unwrap().len(),
        1,
        "a Serving side that never heard the withdrawal keeps its Peer"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn removing_an_unknown_remote_is_a_not_found_error() {
    let pair = paired_servers("remote-removal-unknown").await;

    let error = pair
        .connecting_client
        .remove_remote("no-such-machine")
        .await
        .expect_err("an unknown name names no Pairing to end");

    assert_eq!(pairing_error_code(&error), SessionErrorCode::RemoteNotFound);
    assert_eq!(
        pair.connecting_client.list_remotes().await.unwrap().len(),
        1,
        "a refused removal leaves every known Remote standing"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_revoked_peer_may_not_withdraw_and_leaves_its_revocation_standing() {
    let pair = paired_servers("remote-removal-revoked").await;
    let peer = pair.serving_client.list_peers().await.unwrap().remove(0);
    pair.serving_client
        .remove_peer(&peer.id)
        .await
        .expect("remove the Peer from the Serving side");
    assert_eq!(
        revoked_peer_ids(&pair.serving_data_dir),
        vec![peer.id.clone()]
    );

    let removal = pair
        .connecting_client
        .remove_remote("workstation")
        .await
        .expect("removal answers whether or not the Remote accepts the withdrawal");

    assert!(
        !removal.acknowledged,
        "a Server that is no longer a Peer is refused like any other stranger"
    );
    assert_eq!(
        revoked_peer_ids(&pair.serving_data_dir),
        vec![peer.id],
        "a refused withdrawal disturbs neither the Peer list nor its tombstones"
    );
    assert!(pairing_records(&pair.serving_data_dir.join("peers.json")).is_empty());
    assert!(pairing_records(&pair.connecting_data_dir.join("remotes.json")).is_empty());

    pair.shutdown().await;
}

#[tokio::test]
async fn an_invite_address_presenting_the_wrong_server_key_fails_closed() {
    let inviter_state = tempfile::tempdir().unwrap();
    let inviter_config = tempfile::tempdir().unwrap();
    let inviter = server::spawn_with_timings(
        ServerConfig::new(inviter_state.path(), "wrong-key-inviter")
            .unwrap()
            .with_config_dir(inviter_config.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut inviter_client = ManagedClient::connect(
        ManagedClientConfig::new(inviter_state.path(), "wrong-key-inviter").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut inviter_client).await;
    inviter_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    inviter_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    inviter_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();

    let impostor_state = tempfile::tempdir().unwrap();
    let impostor_config = tempfile::tempdir().unwrap();
    let impostor = server::spawn_with_timings(
        ServerConfig::new(impostor_state.path(), "wrong-key-impostor")
            .unwrap()
            .with_config_dir(impostor_config.path()),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut impostor_client = ManagedClient::connect(
        ManagedClientConfig::new(impostor_state.path(), "wrong-key-impostor").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut impostor_client).await;
    impostor_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .unwrap();
    impostor_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    impostor_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .unwrap();
    let impostor_address = impostor.serving_address().unwrap();

    let connector_state = tempfile::tempdir().unwrap();
    let connector = server::spawn_with_timings(
        ServerConfig::new(connector_state.path(), "wrong-key-connector").unwrap(),
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await
    .unwrap();
    let mut connector_client = ManagedClient::connect(
        ManagedClientConfig::new(connector_state.path(), "wrong-key-connector").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut connector_client).await;

    let invite = inviter_client
        .issue_invite(IssueInviteRequest {
            addresses: vec![impostor_address],
        })
        .await
        .unwrap();
    let error = connector_client
        .redeem_invite(RedeemInviteRequest {
            invite: invite.invite,
            name: Some("impostor".to_owned()),
            addresses: Vec::new(),
        })
        .await
        .expect_err("address with the wrong pinned key is refused");

    assert_eq!(
        pairing_error_code(&error),
        SessionErrorCode::PairingAuthenticationFailed
    );
    assert!(inviter_client.list_peers().await.unwrap().is_empty());
    assert!(impostor_client.list_peers().await.unwrap().is_empty());
    assert!(connector_client.list_remotes().await.unwrap().is_empty());

    drop(connector_client);
    drop(impostor_client);
    drop(inviter_client);
    connector.shutdown().await.unwrap();
    impostor.shutdown().await.unwrap();
    inviter.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_pinned_serving_setting_is_adopted_at_each_startup_with_the_same_identity() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "serving": {
                "enabled": true,
                "port": 0,
                "bindAddress": "127.0.0.1"
            }
        }"#,
    )
    .expect("write Serving Config Document");
    let config = ServerConfig::new(state_dir.path(), "serving-startup-test")
        .expect("configure server")
        .with_data_dir(data_dir.path())
        .with_config_dir(config_dir.path());
    let identity_path = config.data_dir().join("server-identity.pk8");
    assert!(!identity_path.exists());

    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    };
    let first = server::spawn_with_timings(config.clone(), timings)
        .await
        .expect("spawn first Serving server");
    let first_address = first
        .serving_address()
        .expect("startup adopts the pinned Serving setting");
    assert_eq!(first_address.ip(), std::net::Ipv4Addr::LOCALHOST);
    assert_ne!(first_address.port(), 0);
    assert!(identity_path.exists());
    let (unknown_peer, first_certificate) = dial_with_unknown_certificate(first_address).await;
    assert!(unknown_peer.is_err());
    let first_public_key = public_key_from_certificate(&first_certificate);
    first.shutdown().await.expect("stop first Server");

    let replacement = server::spawn_with_timings(config, timings)
        .await
        .expect("spawn replacement Serving server");
    let replacement_address = replacement
        .serving_address()
        .expect("replacement Serving listener is ready");
    let (unknown_peer, replacement_certificate) =
        dial_with_unknown_certificate(replacement_address).await;
    assert!(unknown_peer.is_err());
    assert_eq!(
        public_key_from_certificate(&replacement_certificate),
        first_public_key,
        "a durable Serving Setting reuses the per-channel Server identity at the wire"
    );
    replacement
        .shutdown()
        .await
        .expect("stop replacement Server");
}

#[tokio::test]
async fn a_failed_serving_rebind_keeps_the_listener_already_in_service() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let config = ServerConfig::new(state_dir.path(), "serving-rebind-test")
        .expect("configure server")
        .with_config_dir(config_dir.path());
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..ServerTimings::default()
    };
    let server = server::spawn_with_timings(config, timings)
        .await
        .expect("spawn server");
    let mut local_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "serving-rebind-test")
            .expect("configure local client"),
    )
    .await
    .expect("attach local client");
    receive_initial_state(&mut local_client).await;
    local_client
        .mutate_setting(SettingMutation::ServingPort { value: Some(0) })
        .await
        .expect("ask the operating system for the first Serving port");
    local_client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        })
        .await
        .expect("keep this test on the loopback address it dials");
    local_client
        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
        .await
        .expect("turn Serving on");
    let original = server.serving_address().expect("Serving listener is ready");

    let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("occupy another loopback port");
    let occupied_port = occupied.local_addr().expect("read occupied port").port();
    let error = local_client
        .mutate_setting(SettingMutation::ServingPort {
            value: Some(occupied_port),
        })
        .await
        .expect_err("an occupied Serving port cannot be adopted");
    assert!(
        format!("{error:#}").contains("Serving listener"),
        "the mutation explains which live Setting could not be adopted: {error:#}"
    );
    assert_eq!(
        server.serving_address(),
        Some(original),
        "a failed replacement leaves the listener already serving untouched"
    );
    let connection = tokio::net::TcpStream::connect(original)
        .await
        .expect("original Serving listener still accepts connections");
    drop(connection);

    drop(occupied);
    drop(local_client);
    server.shutdown().await.expect("shut down server");
}

#[test]
fn build_identity_changes_with_executable_contents() {
    let directory = tempfile::tempdir().expect("create build identity fixture directory");
    let executable = directory.path().join("suru-fixture");
    std::fs::write(&executable, b"first executable").expect("write first executable contents");
    let first = suru::build_identity::for_executable(&executable)
        .expect("identify first executable contents");

    let modified = std::fs::metadata(&executable).unwrap().modified().unwrap();
    std::fs::write(&executable, b"other executable").expect("write rebuilt executable contents");
    std::fs::File::options()
        .write(true)
        .open(&executable)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let rebuilt = suru::build_identity::for_executable(&executable)
        .expect("identify rebuilt executable contents");

    assert_ne!(first, rebuilt);
    assert!(first.starts_with("blake3:"));
}

#[tokio::test]
async fn release_channel_server_uses_private_base_state_and_data_roots() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for root in [state_dir.path(), data_dir.path()] {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))
                .expect("make runtime root permissions permissive");
        }
    }
    let config = ServerConfig::new(state_dir.path(), "release")
        .expect("configure release server")
        .with_data_dir(data_dir.path());

    let server = server::spawn(config.clone())
        .await
        .expect("spawn release server");

    assert_eq!(config.state_dir(), state_dir.path());
    assert_eq!(config.data_dir(), data_dir.path());
    assert!(state_dir.path().join("runtime.json").exists());
    assert!(!state_dir.path().join("release").exists());
    assert!(!data_dir.path().join("release").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for root in [state_dir.path(), data_dir.path()] {
            let mode = std::fs::metadata(root)
                .expect("read runtime root metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(windows)]
    {
        assert_windows_current_user_only(state_dir.path());
        assert_windows_current_user_only(data_dir.path());
    }

    server.shutdown().await.expect("shut down release server");
}

#[tokio::test]
async fn server_refuses_a_database_schema_newer_than_the_binary() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config = ServerConfig::new(state_dir.path(), "newer-schema-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let database_path = config.data_dir().join("suru.db");
    seed_database(
        &database_path,
        "
        CREATE TABLE __diesel_schema_migrations (
            version VARCHAR(50) PRIMARY KEY NOT NULL,
            run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        INSERT INTO __diesel_schema_migrations (version) VALUES ('99999999999999');
        ",
    );

    let error = server::spawn(config)
        .await
        .err()
        .expect("newer database schema must stop server startup");
    let message = format!("{error:#}");
    assert!(
        message.contains("schema 99999999999999 is newer than this Suru binary"),
        "startup error should explain the unsafe downgrade: {message}"
    );
}

#[tokio::test]
async fn a_failed_embedded_database_migration_leaves_no_partial_schema() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config = ServerConfig::new(state_dir.path(), "migration-atomicity-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let database_path = config.data_dir().join("suru.db");
    seed_database(
        &database_path,
        "
        CREATE TABLE migration_collision (id BIGINT NOT NULL);
        CREATE INDEX sessions_updated_at_idx ON migration_collision(id);
        ",
    );

    let error = server::spawn(config)
        .await
        .err()
        .expect("broken migration must stop server startup");
    let message = format!("{error:#}");
    assert!(
        message.contains("apply Session database migrations"),
        "startup error should identify migration failure: {message}"
    );
    assert_eq!(
        sqlite_count(
            &database_path,
            "SELECT COUNT(*) AS value FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
        ),
        0,
        "the table created before the failing statement must be rolled back"
    );
    assert_eq!(
        sqlite_count(
            &database_path,
            "SELECT COUNT(*) AS value FROM __diesel_schema_migrations WHERE version = '20260820000000'",
        ),
        0,
        "a failed migration must not be recorded as applied"
    );
}

#[tokio::test]
async fn an_unreadable_landing_agent_selection_falls_back_without_blocking_startup() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config = ServerConfig::new(state_dir.path(), "unreadable-landing-selection-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let database_path = config.data_dir().join("suru.db");
    let original = server::spawn(config.clone())
        .await
        .expect("spawn server to migrate database");
    original.shutdown().await.expect("stop original server");
    seed_database(
        &database_path,
        "INSERT INTO landing_agent_selection (singleton, selection) VALUES (1, 'not-json');",
    );

    let replacement = server::spawn(config)
        .await
        .expect("unreadable preference must not block startup");
    let descriptor = replacement.descriptor().clone();
    let health = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read replacement health")
        .error_for_status()
        .expect("replacement health succeeds")
        .json::<Health>()
        .await
        .expect("decode replacement health");
    assert_eq!(health.landing_agent_selection, None);

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn authenticated_health_describes_the_ready_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "health-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    assert!(descriptor.base_url.starts_with("http://127.0.0.1:"));
    assert_ne!(descriptor.base_url, "http://127.0.0.1:0");

    let missing_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .send()
        .await
        .expect("request health without authentication");
    assert_eq!(missing_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let wrong_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("request health with incorrect authentication");
    assert_eq!(wrong_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let health = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request authenticated health")
        .error_for_status()
        .expect("authenticated health succeeds")
        .json::<Health>()
        .await
        .expect("decode health response");

    assert_eq!(health.instance_id, descriptor.instance_id);
    assert_eq!(health.pid, std::process::id());
    assert_eq!(health.lifecycle, LifecycleState::Ready);
    assert_eq!(health.protocol_version, descriptor.protocol_version);
    assert_eq!(health.build_identity, descriptor.build_identity);
    assert_eq!(
        descriptor.build_identity,
        build_identity::for_current_executable().expect("identify current test executable")
    );

    let missing_event_auth = client
        .get(format!("{}/v1/events", descriptor.base_url))
        .send()
        .await
        .expect("request event stream without authentication");
    assert_eq!(
        missing_event_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let stop_request = ServerShutdown {
        instance_id: descriptor.instance_id,
        reason: ShutdownReason::Manual,
    };
    let missing_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .json(&stop_request)
        .send()
        .await
        .expect("request stop without authentication");
    assert_eq!(
        missing_stop_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let malformed_missing_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("not-json")
        .send()
        .await
        .expect("request malformed stop without authentication");
    assert_eq!(
        malformed_missing_stop_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let wrong_stop_auth = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth("wrong-token")
        .json(&stop_request)
        .send()
        .await
        .expect("request stop with incorrect authentication");
    assert_eq!(wrong_stop_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let descriptor_mode = std::fs::metadata(
            ServerConfig::new(state_dir.path(), "health-test")
                .expect("configure server")
                .descriptor_path(),
        )
        .expect("read runtime descriptor metadata")
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(descriptor_mode, 0o600);

        let lock_mode = std::fs::metadata(state_dir.path().join("health-test/server.lock"))
            .expect("read server lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600);

        let directory_mode = std::fs::metadata(state_dir.path().join("health-test"))
            .expect("read runtime directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }

    #[cfg(windows)]
    {
        assert_windows_current_user_only(state_dir.path().join("health-test"));
        assert_windows_current_user_only(
            ServerConfig::new(state_dir.path(), "health-test")
                .expect("configure server")
                .descriptor_path(),
        );
        assert_windows_current_user_only(state_dir.path().join("health-test/server.lock"));
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stop_refuses_a_mismatched_instance_without_affecting_the_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "mismatched-stop-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: uuid::Uuid::new_v4(),
            reason: ShutdownReason::Manual,
        })
        .send()
        .await
        .expect("request shutdown for the wrong instance");

    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let health = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request health after rejected shutdown")
        .error_for_status()
        .expect("server remains reachable after rejected shutdown")
        .json::<Health>()
        .await
        .expect("decode health after rejected shutdown");
    assert_eq!(health.instance_id, descriptor.instance_id);
    assert_eq!(health.lifecycle, LifecycleState::Ready);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn authenticated_manual_stop_notifies_clients_and_removes_its_registration() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config = ServerConfig::new(state_dir.path(), "manual-stop-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut managed = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "manual-stop-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut managed).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: descriptor.instance_id,
            reason: ShutdownReason::Manual,
        })
        .send()
        .await
        .expect("request manual shutdown");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    let stopping = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request health during graceful shutdown")
        .error_for_status()
        .expect("health remains available during graceful shutdown")
        .json::<Health>()
        .await
        .expect("decode stopping health");
    assert_eq!(stopping.lifecycle, LifecycleState::Stopping);

    let shutdown = match timeout(PROGRESS_DEADLINE, managed.next())
        .await
        .expect("managed client receives manual shutdown intent")
    {
        Some(ManagedEvent::ServerShutdown(shutdown)) => shutdown,
        Some(ManagedEvent::Recovering(status)) => {
            panic!("manual shutdown triggered recovery: {status:?}")
        }
        Some(event) => panic!("expected manual shutdown intent, got {event:?}"),
        None => panic!("managed client closed before shutdown intent"),
    };
    assert_eq!(shutdown.instance_id, descriptor.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Manual);
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, managed.next()).await,
        Ok(None)
    ));

    timeout(PROGRESS_DEADLINE, async {
        while config.descriptor_path().exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stopping server removes its own registration");
    server.shutdown().await.expect("join stopped server");
}

#[tokio::test]
async fn stop_waits_for_its_target_when_the_registration_is_replaced() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "stop-target-wait-test").expect("configure server");
    let server = server::spawn_with_timings(
        config.clone(),
        ServerTimings {
            shutdown_grace: Duration::from_millis(10),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let stop_config = ManagedClientConfig::new(state_dir.path(), "stop-target-wait-test")
        .expect("configure stop client")
        .with_health_check_timeout(Duration::from_millis(50))
        .with_stop_timeout(Duration::from_millis(500));
    let stopping = tokio::spawn(async move { stop_server(&stop_config).await });
    let client = reqwest::Client::new();

    timeout(PROGRESS_DEADLINE, async {
        loop {
            let health = client
                .get(format!("{}/health", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("request health while stop begins")
                .error_for_status()
                .expect("health remains available while stop begins")
                .json::<Health>()
                .await
                .expect("decode health while stop begins");
            if health.lifecycle == LifecycleState::Stopping {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server enters stopping before registration replacement");

    let mut replacement = descriptor.clone();
    replacement.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &replacement);

    stopping
        .await
        .expect("stop task does not panic")
        .expect("stop waits for the original instance");
    assert!(
        matches!(
            timeout(
                Duration::from_millis(100),
                client
                    .get(format!("{}/health", descriptor.base_url))
                    .bearer_auth(&descriptor.token)
                    .send(),
            )
            .await,
            Err(_) | Ok(Err(_))
        ),
        "stop returned while the authenticated target was still reachable"
    );
    let remaining = read_runtime_descriptor(config.descriptor_path());
    assert_eq!(remaining.instance_id, replacement.instance_id);

    server.shutdown().await.expect("join stopped server");
}

#[cfg(windows)]
fn assert_windows_current_user_only(path: impl AsRef<std::path::Path>) {
    use std::{mem, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, SE_DACL_PROTECTED, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct LocalSecurityDescriptor(*mut std::ffi::c_void);
    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: GetNamedSecurityInfoW allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: OpenProcessToken returned this owned handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let path = path.as_ref();
    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: path is NUL-terminated and the requested output pointers are writable.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_utf16.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS, "read DACL for {path:?}");
    assert!(!descriptor.is_null(), "security descriptor for {path:?}");
    assert!(!dacl.is_null(), "DACL for {path:?}");
    let descriptor = LocalSecurityDescriptor(descriptor);

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is live and both output pointers are writable.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
        0,
        "read DACL control flags for {path:?}"
    );
    assert_ne!(
        control & SE_DACL_PROTECTED,
        0,
        "DACL inherits broader access for {path:?}"
    );

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl is owned by the live descriptor and acl_info is writable.
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                ptr::from_mut(&mut acl_info).cast(),
                mem::size_of_val(&acl_info) as u32,
                AclSizeInformation,
            )
        },
        0,
        "inspect DACL for {path:?}"
    );
    assert_eq!(
        acl_info.AceCount, 1,
        "DACL grants access to more than the current user for {path:?}"
    );
    let mut ace = ptr::null_mut();
    // SAFETY: the DACL reports one ACE and ace points to writable storage.
    assert_ne!(
        unsafe { GetAce(dacl, 0, &mut ace) },
        0,
        "read DACL entry for {path:?}"
    );
    // SAFETY: the sole ACE was created as an ACCESS_ALLOWED_ACE by the runtime SDDL.
    let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    assert_eq!(
        ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE,
        "sole DACL entry does not grant access for {path:?}"
    );

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token is writable.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0,
        "open current process token"
    );
    let token = OwnedHandle(token);
    let mut required_bytes = 0;
    // SAFETY: a null buffer with length zero is the documented size-query operation.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_bytes);
    }
    assert!(required_bytes > 0, "size current user token data");
    let mut token_data = vec![0usize; (required_bytes as usize).div_ceil(mem::size_of::<usize>())];
    // SAFETY: token_data is aligned, writable, and at least required_bytes long.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_data.as_mut_ptr().cast(),
                required_bytes,
                &mut required_bytes,
            )
        },
        0,
        "read current user token data"
    );
    // SAFETY: GetTokenInformation initialized the buffer with TOKEN_USER.
    let token_user = unsafe { &*token_data.as_ptr().cast::<TOKEN_USER>() };
    let ace_sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
    // SAFETY: both pointers refer to valid SIDs owned by live allocations.
    assert_ne!(
        unsafe { EqualSid(ace_sid, token_user.User.Sid) },
        0,
        "DACL is not restricted to the current user for {path:?}"
    );
}

#[tokio::test]
async fn server_recovers_from_an_abandoned_partial_publication() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "partial-publication-test";
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    std::fs::write(
        runtime_dir.join(format!("runtime.{}.tmp", std::process::id())),
        b"{\"base_url\":",
    )
    .expect("seed abandoned partial publication");

    let server =
        server::spawn(ServerConfig::new(state_dir.path(), channel).expect("configure server"))
            .await
            .expect("recover from abandoned partial publication");

    let published = read_runtime_descriptor(runtime_dir.join("runtime.json"));
    assert_eq!(published.instance_id, server.descriptor().instance_id);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn server_holds_the_channel_election_lock_for_its_lifetime() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "lifetime-lock-test";
    let config = ServerConfig::new(state_dir.path(), channel).expect("configure server");
    let first = server::spawn(config.clone())
        .await
        .expect("spawn election winner");
    let first_token = first.descriptor().token.clone();

    let contender = server::spawn(config.clone())
        .await
        .err()
        .expect("a second server cannot own the same channel");
    assert!(
        contender
            .to_string()
            .contains("another server already owns")
    );

    first.shutdown().await.expect("shut down election winner");
    assert!(
        !config.descriptor_path().exists(),
        "the election winner removes its own descriptor"
    );
    let successor = server::spawn(config)
        .await
        .expect("elect a successor after the winner exits");
    assert_ne!(successor.descriptor().token, first_token);
    successor.shutdown().await.expect("shut down successor");
}

#[tokio::test]
async fn shutdown_does_not_remove_a_descriptor_owned_by_another_instance() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "ownership-cleanup-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let mut replacement = server.descriptor().clone();
    replacement.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &replacement);

    server.shutdown().await.expect("shut down original server");

    let remaining = read_runtime_descriptor(config.descriptor_path());
    assert_eq!(remaining.instance_id, replacement.instance_id);
}

#[tokio::test]
async fn descriptor_replacement_never_exposes_a_partial_publication() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "atomic-publication-test").expect("configure server");
    let runtime_dir = state_dir.path().join("atomic-publication-test");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    let descriptor_path = config.descriptor_path();
    let stale = suru::protocol::RuntimeDescriptor {
        base_url: "http://127.0.0.1:9".to_owned(),
        token: "stale-token".to_owned(),
        identity: ServerIdentity {
            instance_id: uuid::Uuid::new_v4(),
            pid: 1,
            protocol_version: suru::protocol::PROTOCOL_VERSION,
            build_identity: "stale-build".to_owned(),
        },
    };
    write_runtime_descriptor(&descriptor_path, &stale);

    let ready = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let descriptor_path = descriptor_path.clone();
        let ready = ready.clone();
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<Vec<uuid::Uuid>, String> {
            let mut observed = Vec::new();
            let first: suru::protocol::RuntimeDescriptor = serde_json::from_reader(
                std::fs::File::open(&descriptor_path)
                    .map_err(|error| format!("open initial descriptor: {error}"))?,
            )
            .map_err(|error| format!("decode initial descriptor: {error}"))?;
            observed.push(first.instance_id);
            ready.wait();
            while !stop.load(Ordering::SeqCst) {
                let descriptor: suru::protocol::RuntimeDescriptor = serde_json::from_reader(
                    std::fs::File::open(&descriptor_path)
                        .map_err(|error| format!("open descriptor during publication: {error}"))?,
                )
                .map_err(|error| format!("decode descriptor during publication: {error}"))?;
                observed.push(descriptor.instance_id);
                std::thread::yield_now();
            }
            Ok(observed)
        })
    };
    ready.wait();

    let server = server::spawn(config)
        .await
        .expect("replace stale descriptor");
    tokio::time::sleep(Duration::from_millis(25)).await;
    stop.store(true, Ordering::SeqCst);
    let observed = reader
        .join()
        .expect("descriptor reader does not panic")
        .expect("every observed descriptor is complete");

    assert!(observed.contains(&stale.instance_id));
    assert!(observed.contains(&server.descriptor().instance_id));
    assert!(observed.iter().all(|instance_id| {
        *instance_id == stale.instance_id || *instance_id == server.descriptor().instance_id
    }));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_connects_without_periodic_domain_events() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "events-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "events-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");

    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("connecting event arrives"),
        Some(ManagedEvent::Connecting)
    ));
    let connected = timeout(PROGRESS_DEADLINE, client.next())
        .await
        .expect("connected event arrives")
        .expect("managed client remains open");
    let ManagedEvent::Connected(identity) = connected else {
        panic!("expected connected event, got {connected:?}");
    };
    assert_eq!(identity.instance_id, descriptor.instance_id);
    assert_eq!(identity.pid, descriptor.pid);
    assert!(
        matches!(
            timeout(PROGRESS_DEADLINE, client.next())
                .await
                .expect("settings snapshot arrives"),
            Some(ManagedEvent::SettingsSnapshot(_))
        ),
        "the one-time settings snapshot follows the connection"
    );
    assert!(
        matches!(
            timeout(PROGRESS_DEADLINE, client.next())
                .await
                .expect("Model Catalog arrives"),
            Some(ManagedEvent::ModelCatalog(_))
        ),
        "the Model Catalog follows the settings snapshot"
    );
    assert!(
        timeout(Duration::from_millis(1_100), client.next())
            .await
            .is_err(),
        "the lifecycle stream must stay quiet while the server remains ready"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn sse_keepalive_comments_are_periodic_and_event_neutral() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_timings(
        ServerConfig::new(state_dir.path(), "keepalive-test").expect("configure server"),
        ServerTimings {
            sse_keepalive_interval: Duration::from_millis(100),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let response = reqwest::Client::new()
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open event stream")
        .error_for_status()
        .expect("event stream authenticates");
    let mut chunks = response.bytes_stream();
    let mut raw = Vec::new();

    timeout(PROGRESS_DEADLINE, async {
        loop {
            let chunk = chunks
                .next()
                .await
                .expect("event stream remains open")
                .expect("read event stream bytes");
            raw.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&raw);
            if text.matches(": keep-alive\n\n").count() >= 2 {
                break;
            }
        }
    })
    .await
    .expect("periodic keepalive comments arrive");

    let text = String::from_utf8(raw).expect("SSE response is UTF-8");
    assert!(text.contains(": connected\n\n"));
    assert!(text.contains(": keep-alive\n\n"));
    assert!(
        !text.contains("event: keep-alive"),
        "keepalives stay comment-only"
    );
    assert!(text.contains("event: settings_snapshot\n"));
    assert!(text.contains("event: model_catalog\n"));
    assert!(!text.contains("id:"));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn graceful_server_shutdown_emits_intent_without_starting_crash_recovery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "shutdown-intent-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let instance_id = server.descriptor().instance_id;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shutdown-intent-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut client).await;

    let observe_shutdown = async {
        match timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("shutdown intent arrives")
        {
            Some(ManagedEvent::ServerShutdown(shutdown)) => {
                assert_eq!(shutdown.instance_id, instance_id);
                assert_eq!(shutdown.reason, ShutdownReason::Manual);
            }
            Some(ManagedEvent::Recovering(status)) => {
                panic!("graceful shutdown triggered recovery: {status:?}")
            }
            Some(event) => panic!("expected shutdown intent, got {event:?}"),
            None => panic!("managed client closed without shutdown intent"),
        }
        assert!(matches!(
            timeout(PROGRESS_DEADLINE, client.next()).await,
            Ok(None)
        ));
    };
    let (shutdown_result, ()) = tokio::join!(server.shutdown(), observe_shutdown);
    shutdown_result.expect("shut down server gracefully");
}

#[tokio::test]
async fn authenticated_replacement_stop_emits_replacement_intent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "replacement-intent-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let http = reqwest::Client::new();
    let response = http
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open authenticated event stream")
        .error_for_status()
        .expect("event stream opens");
    let mut events = response.bytes_stream().eventsource();
    let response = request_server_shutdown(&descriptor, ShutdownReason::Replacement).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    let event = timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("event stream remains open")
                .expect("shutdown event is valid");
            if event.event == SERVER_SHUTDOWN_EVENT {
                break event;
            }
        }
    })
    .await
    .expect("replacement intent arrives before transport closure");
    let shutdown: ServerShutdown =
        serde_json::from_str(&event.data).expect("decode replacement intent");
    assert_eq!(shutdown.instance_id, descriptor.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Replacement);

    server
        .run_until_ctrl_c()
        .await
        .expect("join replaced server");
}

// These tests assert Session movement and connection lifecycle; Models are
// independently pushed on the same subscription and may arrive between them.
async fn next_session_catalog_event(
    subscription: &mut suru::managed_client::SessionCatalogSubscription,
) -> Option<ManagedEvent> {
    loop {
        match subscription.next().await {
            Some(ManagedEvent::ModelCatalog(_)) => continue,
            event => return event,
        }
    }
}

#[tokio::test]
async fn remote_catalog_delivers_model_names_and_updates_without_listing_models() {
    use suru::protocol::{
        ModelAvailability, ModelDescriptor, ModelId, ProviderCatalogStatus, ProviderId,
    };

    let (runtime, _provider) = provider_support::ControlledProvider::with_provider(
        ProviderId::new("controlled"),
        vec![ModelDescriptor {
            provider: ProviderId::new("controlled"),
            id: ModelId::new("native-id"),
            display_name: "Friendly Remote Model".into(),
            description: String::new(),
            is_default: true,
            availability: ModelAvailability::Available,
            options: Vec::new(),
        }],
    );
    let pair =
        paired_servers_with_runtime("remote-model-catalog", false, Some(runtime.clone())).await;
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".into()));
    let mut subscription = remote.subscribe_catalog();
    let catalog = timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(ManagedEvent::ModelCatalog(catalog)) = subscription.next().await
                && catalog.providers[0].status == ProviderCatalogStatus::Fresh
            {
                break catalog;
            }
        }
    })
    .await
    .expect("connecting to a Remote discovers and pushes its Model names");
    assert_eq!(
        catalog.providers[0].models[0].display_name,
        "Friendly Remote Model"
    );
    assert_eq!(runtime.model_discoveries(), 1);

    drop(subscription);
    let mut subscription = remote.subscribe_catalog();
    let cached = timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(ManagedEvent::ModelCatalog(catalog)) = subscription.next().await {
                break catalog;
            }
        }
    })
    .await
    .expect("reconnecting receives the catalog already discovered");
    assert_eq!(
        cached.providers[0].models[0].display_name,
        "Friendly Remote Model"
    );
    assert_eq!(
        runtime.model_discoveries(),
        1,
        "reconnecting does not rediscover Models"
    );

    runtime.set_unavailable(Some(suru::protocol::ProviderUnavailability::NotSignedIn));
    pair.serving_client.refresh_models().await.unwrap();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(ManagedEvent::ModelCatalog(catalog)) = subscription.next().await
                && matches!(
                    catalog.providers[0].status,
                    ProviderCatalogStatus::Unavailable { .. }
                )
            {
                break;
            }
        }
    })
    .await
    .expect("the Remote's subsequent Model Catalog changes are pushed");
    drop(subscription);
    pair.shutdown().await;
}

#[tokio::test]
async fn the_subagent_tree_streams_through_a_remote_outlook() {
    let (runtime, mut provider) = provider_support::ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![ModelDescriptor {
            provider: ProviderId::new("codex"),
            id: ModelId::new("gpt-test"),
            display_name: "Codex fixture".into(),
            description: String::new(),
            is_default: true,
            availability: ModelAvailability::Available,
            options: Vec::new(),
        }],
    );
    let pair = paired_servers_with_runtime("remote-subagent-tree", false, Some(runtime)).await;
    let workspace = tempfile::tempdir().expect("create Serving Workspace");
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".to_owned()));
    let created = remote
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate through the Remote".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create a Session on the Remote");
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the Remote's Provider starts");
    let mut native = start.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: suru::protocol::AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-test"),
            options: Vec::new(),
        },
    });
    timeout(PROGRESS_DEADLINE, native.next_turn())
        .await
        .expect("the first Turn reaches the Remote's Provider")
        .succeed();

    let mut tree = remote.subscribe_subagent_tree(created.session.id);
    let suru::managed_client::SubagentTreeEvent::Snapshot(snapshot) =
        next_remote_tree_event(&mut tree).await
    else {
        panic!("the Remote tree opens with its snapshot");
    };
    assert_eq!(snapshot.top_level.session_id, created.session.id);
    assert!(snapshot.subagents.is_empty());

    native
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: suru::provider::ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the Remote's seams".to_owned(),
        })
        .await;
    let suru::managed_client::SubagentTreeEvent::Changed(
        suru::protocol::SubagentTreeChange::SubagentSpawned { entry },
    ) = next_remote_tree_event(&mut tree).await
    else {
        panic!("a spawn on the Remote arrives as a change through the proxy");
    };
    assert_eq!(entry.name, "Explore");
    assert_eq!(entry.parent_session_id, created.session.id);

    drop(tree);
    drop(native);
    pair.shutdown().await;
}

async fn next_remote_tree_event(
    tree: &mut suru::managed_client::SubagentTreeSubscription,
) -> suru::managed_client::SubagentTreeEvent {
    timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("a tree event arrives through the Remote")
        .expect("the Remote tree subscription stays open")
}
