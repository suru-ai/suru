//! Running a Relay as its operator does: on a configuration file and flags,
//! refused where they cannot be used; serving HTTPS from certificate files,
//! renewed by replacing them; and saying its own version.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData};
use suru_relay::{
    Admission, AdmissionRule, Identity, RelayConfig, SCRIPTED_VERIFICATION_URI, ScriptedProvider,
    TlsFiles,
};
use suru_relay_protocol::{Account, Bytes, RelayMessage, SPOKEN, ServerMessage, proof_message};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

/// The exit status of a command that could not do as it was asked.
const FAILED: i32 = 1;

const PUBLIC_ADDRESS: &str = "https://relay.example.com";

/// The Relay binary, as `arguments` say, with nothing it could reach beyond
/// what a test gives it.
fn binary(arguments: &[&std::ffi::OsStr]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"));
    command
        .args(arguments)
        .env_remove("RUST_LOG")
        .env_remove("SURU_RELAY_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn output(mut command: tokio::process::Command) -> Output {
    timeout(DEADLINE, command.output())
        .await
        .expect("the binary finishes in time")
        .expect("run the binary")
}

fn os(text: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(text)
}

/// A Relay binary, once it says it is ready, and the address it listens at.
async fn ready(mut command: tokio::process::Command) -> (tokio::process::Child, SocketAddr) {
    let mut binary = command.spawn().expect("run the Relay");
    let mut diagnostics = BufReader::new(binary.stderr.take().unwrap()).lines();
    let address = timeout(DEADLINE, async {
        loop {
            let line = diagnostics
                .next_line()
                .await
                .unwrap()
                .expect("the Relay says where it listens");
            let line = plain(&line);
            if line.contains("Relay ready")
                && let Some((_, address)) = line.split_once("address=")
            {
                return address.trim().parse().unwrap();
            }
        }
    })
    .await
    .expect("the Relay is ready in time");
    // The Relay never waits on a full pipe to write its diagnostics.
    tokio::spawn(async move { while let Ok(Some(_)) = diagnostics.next_line().await {} });
    (binary, address)
}

/// `line` without the escape sequences that colour it.
fn plain(line: &str) -> String {
    let mut plain = String::new();
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}

/// A certificate for `127.0.0.1` issued by an authority of the test's own,
/// in PEM, and the authority's certificate to trust it by.
struct Minted {
    authority: rustls::pki_types::CertificateDer<'static>,
    chain: String,
    key: String,
}

impl Minted {
    fn new(authority_name: &str) -> Self {
        let mut authority = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        authority.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        authority.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        authority
            .distinguished_name
            .push(rcgen::DnType::CommonName, authority_name);
        let authority =
            rcgen::CertifiedIssuer::self_signed(authority, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .signed_by(&key, &*authority)
            .unwrap();
        Self {
            authority: authority.der().clone(),
            chain: pem("CERTIFICATE", certificate.der()) + &pem("CERTIFICATE", authority.der()),
            key: pem("PRIVATE KEY", &key.serialize_der()),
        }
    }

    /// Writes the chain and the key to `directory`: the files they are in.
    fn write(&self, directory: &Path) -> TlsFiles {
        let (chain, key) = (directory.join("chain.pem"), directory.join("key.pem"));
        std::fs::write(&chain, &self.chain).unwrap();
        std::fs::write(&key, &self.key).unwrap();
        TlsFiles::new(chain, key)
    }

    /// A TLS client that trusts this authority alone.
    fn trusted_by(&self) -> TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.authority.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }
}

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let lines = encoded
        .as_bytes()
        .chunks(64)
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{lines}\n-----END {label}-----\n")
}

/// A Server's connection to a Relay, speaking the protocol as a test says.
struct Client<S> {
    socket: WebSocketStream<S>,
}

/// Opens a WebSocket to the Relay at `address` over HTTPS, trusting what
/// `trusting` trusts: the connection, or why it could not be made.
async fn connect_over_https(
    address: SocketAddr,
    trusting: &TlsConnector,
) -> Result<Client<TlsStream<TcpStream>>, String> {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|error| error.to_string())?;
    let stream = timeout(
        DEADLINE,
        trusting.connect(
            rustls::pki_types::ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()),
            stream,
        ),
    )
    .await
    .expect("the TLS handshake ends in time")
    .map_err(|error| error.to_string())?;
    let (socket, _) = timeout(
        DEADLINE,
        tokio_tungstenite::client_async(format!("wss://{address}/connect"), stream),
    )
    .await
    .expect("the Relay answers in time")
    .map_err(|error| error.to_string())?;
    Ok(Client { socket })
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    async fn say(&mut self, message: &ServerMessage) {
        self.socket
            .send(Message::Text(
                serde_json::to_string(message).unwrap().into(),
            ))
            .await
            .expect("say something to the Relay");
    }

    async fn hear(&mut self) -> RelayMessage {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay answers in time")
                .expect("the Relay keeps the connection open")
                .expect("read the Relay's answer");
            match frame {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("the Relay answered {other:?}"),
            }
        }
    }

    /// Says hello as `key`, hearing the Relay's challenge, which names the
    /// address it is known by.
    async fn challenged(&mut self, key: &KeyPair) -> (Bytes, String) {
        self.say(&ServerMessage::Hello {
            versions: SPOKEN.to_vec(),
            key: Bytes(key.subject_public_key_info()),
        })
        .await;
        match self.hear().await {
            RelayMessage::Challenge { nonce, relay, .. } => (nonce, relay),
            other => panic!("the Relay challenges a Server that says hello, not {other:?}"),
        }
    }

    /// Proves `key` to the Relay known as `known_as`: what it says then.
    async fn prove(&mut self, key: &KeyPair, known_as: &str) -> RelayMessage {
        let (nonce, relay) = self.challenged(key).await;
        assert_eq!(relay, known_as);
        let message = proof_message(known_as, &nonce.0, &key.subject_public_key_info());
        self.say(&ServerMessage::Proof {
            signature: Bytes(rcgen::SigningKey::sign(key, &message).unwrap()),
        })
        .await;
        self.hear().await
    }
}

/// Writes `text` as a configuration file in `directory`: where it is.
fn configuration(directory: &Path, text: &str) -> PathBuf {
    let path = directory.join("suru-relay.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// `text` as TOML writes a string literally: a path, on every platform.
fn literal(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[tokio::test]
async fn the_relay_refuses_to_start_on_a_configuration_it_cannot_use_saying_what_is_wrong() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    let mismatched = tempfile::tempdir().unwrap();
    let (one, other) = (Minted::new("One"), Minted::new("Other"));
    let files = one.write(mismatched.path());
    std::fs::write(files.private_key(), &other.key).unwrap();
    let empty = directory.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let missing = directory.path().join("missing.pem");
    let http = "listen_http = \"127.0.0.1:0\"\n";
    let named = format!("public_address = \"{PUBLIC_ADDRESS}\"\n");

    let cases: Vec<(String, Vec<&str>, Vec<String>)> = vec![
        (
            http.to_owned(),
            vec![],
            vec![
                "not told its public address".to_owned(),
                "--public-address".to_owned(),
            ],
        ),
        (
            format!("{named}{http}public_address_ = \"x\"\n"),
            vec![],
            vec![
                "unknown field".to_owned(),
                "public_address_".to_owned(),
                "line 3".to_owned(),
            ],
        ),
        (
            format!("{named}{http}admit_user = [\"octocat\"]\n"),
            vec![],
            vec!["unknown field".to_owned(), "`admit_user`".to_owned()],
        ),
        (
            format!("{named}{http}logins_per_account = \"64\"\n"),
            vec![],
            vec!["logins_per_account".to_owned(), "invalid type".to_owned()],
        ),
        (
            format!("{named}{http}joined_connections_per_account = 0\n"),
            vec![],
            vec![
                "joined_connections_per_account".to_owned(),
                "nonzero".to_owned(),
            ],
        ),
        (
            format!("{named}{http}trusted_proxies = [\"10.0.0.0/33\"]\n"),
            vec![],
            vec!["`10.0.0.0/33`".to_owned(), "trusted_proxies".to_owned()],
        ),
        (
            format!("{named}{http}"),
            vec!["--listen-https", "127.0.0.1:0"],
            vec![
                "`--listen-https`".to_owned(),
                "`listen_http` in ".to_owned(),
            ],
        ),
        (
            named.clone(),
            vec![],
            vec!["not told where to listen".to_owned()],
        ),
        (
            format!(
                "{named}{http}tls_certificate_chain_file = {}\n",
                literal(files.certificate_chain())
            ),
            vec![],
            vec![
                "`tls_certificate_chain_file` in ".to_owned(),
                "plain HTTP".to_owned(),
            ],
        ),
        (
            format!(
                "{named}listen_https = \"127.0.0.1:0\"\ntls_certificate_chain_file = {}\n\
                 tls_private_key_file = {}\n",
                literal(&missing),
                literal(files.private_key())
            ),
            vec![],
            vec![format!("{missing:?}"), "certificate chain".to_owned()],
        ),
        (
            format!(
                "{named}listen_https = \"127.0.0.1:0\"\ntls_certificate_chain_file = {}\n\
                 tls_private_key_file = {}\n",
                literal(&empty),
                literal(files.private_key())
            ),
            vec![],
            vec![format!("{empty:?}"), "holds no certificate".to_owned()],
        ),
        (
            format!(
                "{named}listen_https = \"127.0.0.1:0\"\ntls_certificate_chain_file = {}\n\
                 tls_private_key_file = {}\n",
                literal(files.certificate_chain()),
                literal(files.private_key())
            ),
            vec![],
            vec![
                "is not the key of the first certificate".to_owned(),
                format!("{:?}", files.private_key()),
            ],
        ),
        (
            format!(
                "{named}{http}admit_organizations = [\"acme\"]\ngithub_client_id = \"Iv23li\"\n"
            ),
            vec![],
            vec![
                "`admit_organizations` in ".to_owned(),
                "--github-private-key-file".to_owned(),
            ],
        ),
        (
            format!("public_address = \"relay example\"\n{http}"),
            vec![],
            vec![
                "`public_address` in ".to_owned(),
                "`relay example`".to_owned(),
            ],
        ),
        (
            "listen_http = \n".to_owned(),
            vec![],
            vec!["cannot be used".to_owned(), "line 1".to_owned()],
        ),
    ];
    for (text, flags, said) in cases {
        let path = configuration(directory.path(), &text);
        let mut arguments = vec![
            os("--config"),
            path.as_os_str(),
            os("--database"),
            database.as_os_str(),
            os("run"),
        ];
        arguments.extend(flags.iter().map(|flag| os(flag)));
        let ran = output(binary(&arguments)).await;
        let stderr = String::from_utf8_lossy(&ran.stderr);
        assert_eq!(ran.status.code(), Some(FAILED), "{text}: {stderr}");
        for said in &said {
            assert!(
                stderr.contains(said.as_str()),
                "{text}: {said} not in {stderr}"
            );
        }
        assert!(ran.stdout.is_empty(), "{text}");
        assert!(
            !database.exists(),
            "a Relay refusing its configuration keeps no records: {text}"
        );
    }

    let missing_file = directory.path().join("nowhere.toml");
    let ran = output(binary(&[
        os("--config"),
        missing_file.as_os_str(),
        os("run"),
    ]))
    .await;
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(FAILED), "{stderr}");
    assert!(
        stderr.contains("read the configuration file")
            && stderr.contains(&missing_file.display().to_string()),
        "{stderr}"
    );
}

#[tokio::test]
async fn the_relay_runs_on_its_configuration_its_flags_overriding_it_and_its_commands_read_it_too()
{
    let directory = tempfile::tempdir().unwrap();
    // The database is named relative to the file, which is not the working
    // directory.
    let path = configuration(
        directory.path(),
        "database = \"records/relay.db\"\npublic_address = \"https://relay.example.org\"\n\
         listen_http = \"127.0.0.1:0\"\nkeepalive_seconds = 1\n",
    );
    std::fs::create_dir(directory.path().join("records")).unwrap();
    let database = directory.path().join("records").join("relay.db");
    let (mut relay, address) = ready(binary(&[
        os("--config"),
        path.as_os_str(),
        os("run"),
        os("--public-address"),
        os(PUBLIC_ADDRESS),
    ]))
    .await;
    assert!(
        database.exists(),
        "the Relay keeps its records where its file says"
    );
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/connect"))
        .await
        .expect("open a WebSocket to the Relay");
    client
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::Hello {
                versions: SPOKEN.to_vec(),
                key: Bytes(KeyPair::generate().unwrap().subject_public_key_info()),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let challenge = timeout(DEADLINE, client.next())
        .await
        .expect("the Relay challenges in time")
        .unwrap()
        .unwrap();
    let Message::Text(challenge) = challenge else {
        panic!("the Relay challenges in a text frame, not {challenge:?}");
    };
    match serde_json::from_str(challenge.as_str()).unwrap() {
        RelayMessage::Challenge { relay, .. } => {
            assert_eq!(relay, PUBLIC_ADDRESS, "the flag overrides the file");
        }
        other => panic!("the Relay challenges, not {other:?}"),
    }
    // The file's keepalive holds, the flags giving none: a Server saying
    // nothing more is pinged within it.
    let pinged = timeout(DEADLINE, client.next())
        .await
        .expect("the Relay pings in time")
        .unwrap()
        .unwrap();
    assert!(matches!(pinged, Message::Ping(_)), "{pinged:?}");

    // The operator's commands find the same database through the file, named
    // by --config or by the environment.
    for mut command in [
        binary(&[
            os("--config"),
            path.as_os_str(),
            os("logins"),
            os("list"),
            os("--json"),
        ]),
        {
            let mut command = binary(&[os("accounts"), os("list"), os("--json")]);
            command.env("SURU_RELAY_CONFIG", &path);
            command
        },
    ] {
        command.current_dir(std::env::temp_dir());
        let listed = output(command).await;
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&listed.stdout).trim(), "[]");
    }
    relay.kill().await.unwrap();
}

#[tokio::test]
async fn the_relay_binary_serves_https_from_its_certificate_files() {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new("Relay test authority");
    let files = minted.write(directory.path());
    let path = configuration(
        directory.path(),
        &format!(
            "public_address = \"{PUBLIC_ADDRESS}\"\nlisten_https = \"127.0.0.1:0\"\n\
             tls_certificate_chain_file = \"chain.pem\"\ntls_private_key_file = \"key.pem\"\n\
             database = \"relay.db\"\n"
        ),
    );
    assert!(files.certificate_chain().starts_with(directory.path()));
    let (mut relay, address) = ready(binary(&[os("run"), os("--config"), path.as_os_str()])).await;

    let mut client = connect_over_https(address, &minted.trusted_by())
        .await
        .expect("reach the Relay over HTTPS, trusting the authority that issued its certificate");
    let (_, relay_named) = client.challenged(&KeyPair::generate().unwrap()).await;
    assert_eq!(relay_named, PUBLIC_ADDRESS);
    assert!(
        connect_over_https(address, &Minted::new("Another authority").trusted_by())
            .await
            .is_err(),
        "a client that trusts another authority is refused the Relay's certificate"
    );
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{address}/connect"))
            .await
            .is_err(),
        "the Relay serves HTTPS alone"
    );
    relay.kill().await.unwrap();
}

/// Admission by the scripted provider, which admits whoever it logs in.
fn admitting(provider: &Arc<ScriptedProvider>) -> Admission {
    Admission::by([provider.clone() as Arc<dyn AdmissionRule>])
}

#[tokio::test]
async fn a_server_logs_in_over_the_relays_own_https_and_a_renewed_certificate_is_served_as_its_files_change()
 {
    let directory = tempfile::tempdir().unwrap();
    let certificates = tempfile::tempdir().unwrap();
    let first = Minted::new("First authority");
    let files = first.write(certificates.path());
    let provider = Arc::new(ScriptedProvider::new());
    let relay = suru_relay::start(
        RelayConfig::new(
            (Ipv4Addr::LOCALHOST, 0).into(),
            directory.path().join("relay.db"),
            PUBLIC_ADDRESS,
        )
        .with_tls(files.clone())
        .with_certificate_check_interval(Duration::from_millis(20))
        .with_connection_log(std::io::sink())
        .with_admission(admitting(&provider)),
        provider.clone(),
    )
    .await
    .expect("start the Relay serving HTTPS");
    let address = relay.address();

    let key = KeyPair::generate().unwrap();
    let mut before = connect_over_https(address, &first.trusted_by())
        .await
        .expect("reach the Relay over HTTPS");
    assert_eq!(
        before.prove(&key, PUBLIC_ADDRESS).await,
        RelayMessage::Proven { login: None }
    );
    before
        .say(&ServerMessage::BeginLogin {
            hostname: "workstation".to_owned(),
        })
        .await;
    let RelayMessage::LoginStarted {
        verification_uri,
        user_code,
        ..
    } = before.hear().await
    else {
        panic!("the Relay begins a login over HTTPS");
    };
    assert_eq!(verification_uri, SCRIPTED_VERIFICATION_URI);

    // The certificate is renewed — by another authority, so the test can
    // tell which is served — while the login is under way.
    let renewed = Minted::new("Renewing authority");
    std::fs::write(files.certificate_chain(), &renewed.chain).unwrap();
    std::fs::write(files.private_key(), &renewed.key).unwrap();
    timeout(DEADLINE, async {
        while connect_over_https(address, &renewed.trusted_by())
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Relay serves the renewed certificate without restarting");
    assert!(
        connect_over_https(address, &first.trusted_by())
            .await
            .is_err(),
        "a new connection is served the renewed certificate alone"
    );
    // A connection made before keeps going: its login ends as any other.
    assert!(provider.approve(
        &user_code,
        Identity {
            subject: "583231".to_owned(),
            username: "octocat".to_owned(),
        },
    ));
    assert_eq!(
        before.hear().await,
        RelayMessage::LoginDone {
            account: Account {
                provider: "scripted".to_owned(),
                username: "octocat".to_owned(),
            },
        }
    );

    // Files caught half-written are passed over, the Relay serving what it
    // had until they are whole again.
    std::fs::write(files.certificate_chain(), "-----BEGIN CERTIFICATE-----\n").unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut after = connect_over_https(address, &renewed.trusted_by())
        .await
        .expect("the Relay goes on serving the certificate it had");
    assert_eq!(
        after.prove(&key, PUBLIC_ADDRESS).await,
        RelayMessage::Proven {
            login: Some(Account {
                provider: "scripted".to_owned(),
                username: "octocat".to_owned(),
            }),
        },
        "the Login formed over HTTPS stands"
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_relay_says_its_own_version_and_the_protocol_it_speaks() {
    let ran = output(binary(&[os("--version")])).await;
    assert!(ran.status.success());
    let said = String::from_utf8(ran.stdout).unwrap();
    let manifest = include_str!("../Cargo.toml");
    let version = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("the Relay's manifest names its own version");
    assert_eq!(
        said.trim(),
        format!(
            "suru-relay {version} (Relay protocol {})",
            SPOKEN
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
}
