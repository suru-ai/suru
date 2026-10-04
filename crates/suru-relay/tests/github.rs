//! GitHub as a Relay's identity provider, and the admission rule naming
//! GitHub users, spoken to through a stub of GitHub that answers its
//! device-login, user and users endpoints as GitHub does — never GitHub
//! itself.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::SocketAddr,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Form, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use serde_json::{Value, json};
use suru_relay::{
    Admission, GitHub, GitHubApp, IdentityProvider, LoginRefusal, LookUpFailed, RelayConfig,
    RunningRelay,
};
use suru_relay_protocol::{
    Account, Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, proof_message,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

/// How long each second GitHub speaks of lasts in these tests, unless a test
/// says otherwise.
const SECOND: Duration = Duration::from_millis(1);

const PUBLIC_ADDRESS: &str = "https://relay.example.com";

/// What the stub's GitHub App is known by.
const CLIENT_ID: &str = "Iv23liStubClientId";

/// The device code the stub hands out for each login, which never leaves the
/// Relay.
const DEVICE_CODE: &str = "3584d83530557fdd1f46af8289938c8ef79f9dc5";

/// The token the stub gives each login done, and the refresh token with it,
/// neither of which the Relay may keep.
const TOKEN: &str = "ghu_16C7e42F292c6912E7710c838347Ae178B4a";
const REFRESH_TOKEN: &str = "ghr_1B4a2e77838347a7E420ce178F2E7c6912E169";

/// The user the stub logs in as, unless a test says otherwise.
const OCTOCAT: u64 = 583_231;

/// A request the stub was sent.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    form: HashMap<String, String>,
    at: Instant,
}

/// What the stub answers.
struct Script {
    /// What the device-code endpoint answers.
    begun: (StatusCode, Value),
    /// What the token endpoint answers to each asking after a login, in
    /// turn; the last answers every asking after it.
    polled: VecDeque<Value>,
    /// Who the token it gives reads as.
    user: Value,
    /// Who goes by each name, by its name in lower case.
    users: HashMap<String, Value>,
    /// Whether it refuses to say who goes by a name for its rate limit.
    rate_limited: bool,
    /// Whether it never answers who goes by a name.
    hang: bool,
    /// How it answers who goes by a name with more than the Relay reads,
    /// where it does.
    oversized: Option<Oversized>,
    seen: Vec<Seen>,
}

#[derive(Clone, Copy)]
enum Oversized {
    /// Saying how much it sends.
    Measured,
    /// In chunks, never saying how much.
    Streamed,
}

fn user(id: u64, login: &str) -> Value {
    json!({
        "login": login,
        "id": id,
        "node_id": "MDQ6VXNlcjU4MzIzMQ==",
        "type": "User",
        "site_admin": false,
        "name": "The Octocat",
    })
}

fn pending() -> Value {
    json!({
        "error": "authorization_pending",
        "error_description": "The authorization request is still pending.",
        "error_uri": "https://docs.github.com/developers/apps/authorizing-oauth-apps#error-codes-for-the-device-flow",
    })
}

fn polled_error(error: &str) -> Value {
    json!({ "error": error, "error_description": "described", "error_uri": "https://docs.github.com" })
}

fn token() -> Value {
    json!({
        "access_token": TOKEN,
        "expires_in": 28800,
        "refresh_token": REFRESH_TOKEN,
        "refresh_token_expires_in": 15_811_200,
        "token_type": "bearer",
        "scope": "",
    })
}

fn begun(expires_in: u64, interval: u64) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({
            "device_code": DEVICE_CODE,
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "expires_in": expires_in,
            "interval": interval,
        }),
    )
}

/// A stub of GitHub, at an address of its own.
#[derive(Clone)]
struct Stub {
    address: SocketAddr,
    script: Arc<Mutex<Script>>,
}

impl Stub {
    /// A stub that begins each login as GitHub does, logs it in as octocat
    /// at the first asking after it, and knows nobody by name.
    async fn start() -> Self {
        let script = Arc::new(Mutex::new(Script {
            begun: begun(900, 5),
            polled: VecDeque::from([token()]),
            user: user(OCTOCAT, "octocat"),
            users: HashMap::new(),
            rate_limited: false,
            hang: false,
            oversized: None,
            seen: Vec::new(),
        }));
        let app = Router::new()
            .route("/login/device/code", post(device_code))
            .route("/login/oauth/access_token", post(access_token))
            .route("/user", get(authenticated_user))
            .route("/users/{name}", get(named_user))
            .with_state(script.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { address, script }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The GitHub App, reached at this stub.
    fn app(&self) -> GitHubApp {
        GitHubApp::new(CLIENT_ID)
            .with_addresses(self.url(), self.url())
            .with_second(SECOND)
            .with_request_timeout(DEADLINE)
    }

    fn github(&self) -> GitHub {
        GitHub::new(self.app()).unwrap()
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap()
    }

    /// Answers each asking after a login with `answers`, in turn.
    fn answer_polls(&self, answers: impl IntoIterator<Item = Value>) {
        self.script().polled = answers.into_iter().collect();
    }

    /// Has the token it gives read as the user `login`, whose id is `id`.
    fn log_in_as(&self, id: u64, login: &str) {
        self.script().user = user(id, login);
    }

    /// Has `login` be the name of the user `id`, from now on.
    fn name(&self, id: u64, login: &str) {
        self.script()
            .users
            .insert(login.to_lowercase(), user(id, login));
    }

    /// The requests it has been sent to `path`.
    fn seen(&self, path: &str) -> Vec<Seen> {
        self.script()
            .seen
            .iter()
            .filter(|seen| seen.path == path)
            .cloned()
            .collect()
    }
}

fn record(script: &Mutex<Script>, path: String, headers: HeaderMap, form: HashMap<String, String>) {
    script.lock().unwrap().seen.push(Seen {
        path,
        headers,
        form,
        at: Instant::now(),
    });
}

async fn device_code(
    State(script): State<Arc<Mutex<Script>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    record(&script, "/login/device/code".to_owned(), headers, form);
    let (status, body) = script.lock().unwrap().begun.clone();
    (status, axum::Json(body)).into_response()
}

async fn access_token(
    State(script): State<Arc<Mutex<Script>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    record(
        &script,
        "/login/oauth/access_token".to_owned(),
        headers,
        form,
    );
    let mut script = script.lock().unwrap();
    let answer = if script.polled.len() > 1 {
        script.polled.pop_front().unwrap()
    } else {
        script.polled.front().cloned().unwrap_or_else(pending)
    };
    axum::Json(answer).into_response()
}

async fn authenticated_user(
    State(script): State<Arc<Mutex<Script>>>,
    headers: HeaderMap,
) -> Response {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .is_some_and(|value| value == format!("Bearer {TOKEN}").as_str());
    record(&script, "/user".to_owned(), headers, HashMap::new());
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "message": "Bad credentials" })),
        )
            .into_response();
    }
    axum::Json(script.lock().unwrap().user.clone()).into_response()
}

async fn named_user(
    State(script): State<Arc<Mutex<Script>>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    record(&script, format!("/users/{name}"), headers, HashMap::new());
    let (hang, oversized, rate_limited, found) = {
        let script = script.lock().unwrap();
        (
            script.hang,
            script.oversized,
            script.rate_limited,
            script.users.get(&name.to_lowercase()).cloned(),
        )
    };
    if hang {
        std::future::pending::<()>().await;
    }
    match oversized {
        Some(Oversized::Measured) => return vec![b' '; 1024 * 1024].into_response(),
        Some(Oversized::Streamed) => {
            let chunks = futures_util::stream::iter(
                (0..64).map(|_| Ok::<_, std::io::Error>(vec![b' '; 16 * 1024])),
            );
            return Body::from_stream(chunks).into_response();
        }
        None => {}
    }
    if rate_limited {
        return (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1800000000"),
            ],
            axum::Json(json!({ "message": "API rate limit exceeded for 127.0.0.1." })),
        )
            .into_response();
    }
    match found {
        Some(user) => axum::Json(user).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "message": "Not Found" })),
        )
            .into_response(),
    }
}

/// The value of `name` among `headers`.
fn header_of<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .map(|value| value.to_str().unwrap())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_login_is_begun_with_the_apps_client_id_alone_and_reads_who_logged_in_by_their_numeric_id()
 {
    let stub = Stub::start().await;
    stub.answer_polls([pending(), pending(), token()]);
    let github = stub.github();
    assert_eq!(github.name(), "github");

    let login = github.begin_login().await.unwrap();
    assert_eq!(login.verification_uri, "https://github.com/login/device");
    assert_eq!(login.user_code, "WDJB-MJHT");
    assert_eq!(login.expires_in, SECOND * 900);
    assert_eq!(login.interval, SECOND * 5);
    let begun = stub.seen("/login/device/code");
    assert_eq!(
        begun[0].form,
        HashMap::from([("client_id".to_owned(), CLIENT_ID.to_owned())]),
        "a login is begun by the app's client ID, and no secret"
    );
    assert_eq!(header_of(&begun[0].headers, "accept"), "application/json");

    assert_eq!(
        github.finish_login(&login).await.unwrap(),
        suru_relay::Identity {
            subject: "583231".to_owned(),
            username: "octocat".to_owned(),
        }
    );
    let polls = stub.seen("/login/oauth/access_token");
    assert_eq!(polls.len(), 3, "asked after until it was done");
    for poll in &polls {
        assert_eq!(
            poll.form,
            HashMap::from([
                ("client_id".to_owned(), CLIENT_ID.to_owned()),
                ("device_code".to_owned(), DEVICE_CODE.to_owned()),
                (
                    "grant_type".to_owned(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_owned()
                ),
            ])
        );
        assert_eq!(header_of(&poll.headers, "accept"), "application/json");
    }
    let read = stub.seen("/user");
    assert_eq!(read.len(), 1, "who logged in is read once");
    assert_eq!(
        header_of(&read[0].headers, "authorization"),
        format!("Bearer {TOKEN}")
    );
    assert_eq!(
        header_of(&read[0].headers, "accept"),
        "application/vnd.github+json"
    );
    assert_eq!(
        header_of(&read[0].headers, "x-github-api-version"),
        "2022-11-28"
    );
    for seen in [&begun[0], &polls[0], &read[0]] {
        assert!(
            header_of(&seen.headers, "user-agent").starts_with("suru-relay/"),
            "{:?}",
            seen.headers
        );
    }
}

#[tokio::test]
async fn a_pending_login_is_asked_after_no_more_often_than_github_asks_and_less_often_once_told_to_slow_down()
 {
    // Long enough a second that the gaps between askings are told apart.
    let second = Duration::from_millis(10);
    let stub = Stub::start().await;
    stub.script().begun = begun(900, 1);
    let slow_down_to = |interval: Option<u64>| {
        let mut answer = polled_error("slow_down");
        if let Some(interval) = interval {
            answer["interval"] = json!(interval);
        }
        answer
    };
    stub.answer_polls([
        pending(),
        slow_down_to(None),
        pending(),
        slow_down_to(Some(15)),
        token(),
    ]);
    let github = GitHub::new(stub.app().with_second(second)).unwrap();
    let login = github.begin_login().await.unwrap();
    let begun_at = stub.seen("/login/device/code")[0].at;
    github.finish_login(&login).await.unwrap();

    let mut at = vec![begun_at];
    at.extend(
        stub.seen("/login/oauth/access_token")
            .iter()
            .map(|seen| seen.at),
    );
    let gaps = at
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    assert_eq!(gaps.len(), 5);
    // One second between askings as GitHub asks; five more once it says to
    // slow down; and as many as it says, where it says more.
    for (gap, seconds) in gaps.iter().zip([1, 1, 6, 6, 15]) {
        assert!(*gap >= second * seconds, "{gaps:?}");
    }
}

#[tokio::test]
async fn a_login_denied_expired_or_refused_for_the_app_ends_saying_so_and_naming_no_secret() {
    for (answer, expected) in [
        ("access_denied", Err(LoginRefusal::Denied)),
        ("expired_token", Err(LoginRefusal::Expired)),
        ("device_flow_disabled", Ok("device login is not enabled")),
        (
            "incorrect_client_credentials",
            Ok("GitHub knows no GitHub App by the client ID"),
        ),
        ("unsupported_grant_type", Ok("unsupported_grant_type")),
    ] {
        let stub = Stub::start().await;
        stub.answer_polls([pending(), polled_error(answer)]);
        let github = stub.github();
        let login = github.begin_login().await.unwrap();
        let refusal = github.finish_login(&login).await.unwrap_err();
        match expected {
            Err(expected) => assert_eq!(refusal, expected, "{answer}"),
            Ok(said) => {
                let LoginRefusal::Unavailable(reason) = &refusal else {
                    panic!("{answer} reads as {refusal:?}");
                };
                assert!(reason.contains(said), "{answer}: {reason}");
                assert!(!reason.contains(DEVICE_CODE) && !reason.contains(CLIENT_ID));
            }
        }
        assert_eq!(stub.seen("/user").len(), 0, "{answer}");
    }

    // An app without device login is refused at the beginning, as GitHub
    // refuses it there.
    let stub = Stub::start().await;
    stub.script().begun = (
        StatusCode::BAD_REQUEST,
        polled_error("device_flow_disabled"),
    );
    let Err(LoginRefusal::Unavailable(reason)) = stub.github().begin_login().await else {
        panic!("a login GitHub will not begin is not begun");
    };
    assert!(reason.contains("device login is not enabled"), "{reason}");
}

#[tokio::test]
async fn a_username_is_looked_up_to_the_numeric_id_of_whoever_goes_by_it() {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.script().users.insert(
        "github".to_owned(),
        json!({ "login": "github", "id": 9_919, "type": "Organization" }),
    );
    let github = stub.github();

    assert_eq!(
        github.look_up("OctoCat").await,
        Ok(Some(suru_relay::Identity {
            subject: "583231".to_owned(),
            username: "octocat".to_owned(),
        }))
    );
    let asked = stub.seen("/users/OctoCat");
    assert_eq!(asked.len(), 1);
    assert_eq!(header_of(&asked[0].headers, "authorization"), "");
    assert_eq!(
        header_of(&asked[0].headers, "x-github-api-version"),
        "2022-11-28"
    );
    assert_eq!(github.look_up("nobody-here").await, Ok(None));
    let Err(LookUpFailed(why)) = github.look_up("github").await else {
        panic!("an organization's name is no user's");
    };
    assert!(why.contains("organization"), "{why}");

    let before = stub.script().seen.len();
    assert_eq!(github.look_up("octo/../user").await, Ok(None));
    assert_eq!(
        stub.script().seen.len(),
        before,
        "what cannot be a username is never asked after"
    );

    stub.script().rate_limited = true;
    let Err(LookUpFailed(why)) = github.look_up("octocat").await else {
        panic!("a lookup GitHub refuses for its rate limit fails");
    };
    assert!(why.contains("limiting"), "{why}");
}

#[tokio::test]
async fn github_answering_too_slowly_or_too_much_or_not_at_all_is_given_up_on() {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.script().hang = true;
    let github = GitHub::new(stub.app().with_request_timeout(Duration::from_millis(50))).unwrap();
    let looked_up = timeout(DEADLINE, github.look_up("octocat"))
        .await
        .expect("a request is given up in time");
    assert_eq!(
        looked_up,
        Err(LookUpFailed("GitHub did not answer in time".to_owned()))
    );

    for oversized in [Oversized::Measured, Oversized::Streamed] {
        let stub = Stub::start().await;
        stub.name(OCTOCAT, "octocat");
        stub.script().oversized = Some(oversized);
        assert_eq!(
            stub.github().look_up("octocat").await,
            Err(LookUpFailed(
                "GitHub answered with more than the Relay reads".to_owned()
            ))
        );
    }

    let closed = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let nowhere = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let github = GitHub::new(GitHubApp::new(CLIENT_ID).with_addresses(&nowhere, &nowhere)).unwrap();
    assert_eq!(
        github.begin_login().await.unwrap_err(),
        LoginRefusal::Unavailable("GitHub could not be reached".to_owned())
    );
    assert_eq!(
        github.look_up("octocat").await,
        Err(LookUpFailed("GitHub could not be reached".to_owned()))
    );
}

/// A connection to a Relay speaking as a Server whose identity key a test
/// holds.
struct Server {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Server {
    /// Connects to `relay` and proves `key` there: the connection, and the
    /// Account the Relay says its Login stands under, where it stands.
    async fn proven(relay: &RunningRelay, key: &KeyPair) -> (Self, Option<Account>) {
        let (socket, _) = timeout(
            DEADLINE,
            tokio_tungstenite::connect_async(format!("ws://{}/connect", relay.address())),
        )
        .await
        .expect("the Relay answers in time")
        .expect("open a WebSocket to the Relay");
        let mut server = Self { socket };
        let public_key = key.subject_public_key_info();
        server
            .say(&ServerMessage::Hello {
                versions: SPOKEN.to_vec(),
                key: Bytes(public_key.clone()),
            })
            .await;
        let RelayMessage::Challenge { nonce, .. } = server.hear().await else {
            panic!("the Relay challenges a Server that says hello");
        };
        let proof = proof_message(PUBLIC_ADDRESS, &nonce.0, &public_key);
        server
            .say(&ServerMessage::Proof {
                signature: Bytes(key.sign(&proof).unwrap()),
            })
            .await;
        let RelayMessage::Proven { login } = server.hear().await else {
            panic!("the Relay takes the proof");
        };
        (server, login)
    }

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

    /// Begins a login, as the Server's user asks it to: where its user is
    /// sent, and the code they enter there.
    async fn begin_login(&mut self) -> (String, String) {
        self.say(&ServerMessage::BeginLogin {
            hostname: "workstation".to_owned(),
        })
        .await;
        let RelayMessage::LoginStarted {
            verification_uri,
            user_code,
            ..
        } = self.hear().await
        else {
            panic!("the Relay begins a login");
        };
        (verification_uri, user_code)
    }

    /// Logs `key` in at `relay`, on a connection of its own, as whoever the
    /// stub logs in: how the Relay says the login ended.
    async fn logging_in(relay: &RunningRelay, key: &KeyPair) -> RelayMessage {
        let (mut server, _) = Self::proven(relay, key).await;
        server.begin_login().await;
        server.hear().await
    }
}

fn key() -> KeyPair {
    KeyPair::generate().expect("generate an identity key as a Server does")
}

fn github_account(username: &str) -> Account {
    Account {
        provider: "github".to_owned(),
        username: username.to_owned(),
    }
}

fn done_as(username: &str) -> RelayMessage {
    RelayMessage::LoginDone {
        account: github_account(username),
    }
}

fn refusal(answer: &RelayMessage) -> Option<&Refusal> {
    match answer {
        RelayMessage::Refused { refusal, .. } => Some(refusal),
        _ => None,
    }
}

/// Starts a Relay on the records in `directory`, logging its users in
/// through `app` and admitting the GitHub users `named`, configured further
/// as `configure` says.
async fn start_relay(
    directory: &tempfile::TempDir,
    app: GitHubApp,
    named: &[&str],
    configure: impl FnOnce(RelayConfig) -> RelayConfig,
) -> anyhow::Result<RunningRelay> {
    suru_relay::start(
        configure(
            RelayConfig::new(
                (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                directory.path().join("relay.db"),
                PUBLIC_ADDRESS,
            )
            .with_connection_log(std::io::sink())
            .with_admission(Admission::nobody().with_named_users(named.iter().copied())),
        ),
        Arc::new(GitHub::new(app)?),
    )
    .await
}

/// Everything the Relay keeps on disk in `directory`, by file name.
fn database_files(directory: &tempfile::TempDir) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A writer a test reads what the Relay's own log wrote from.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_server_logs_in_with_github_under_an_account_kept_by_numeric_id_and_nothing_of_the_token_is_kept()
 {
    // The Relay's own log, as verbose as it can be asked to be.
    let captured = Captured::default();
    let writer = captured.clone();
    let _logging = tracing_subscriber::registry()
        .with(suru_relay::log_layer(Some("trace"), move || writer.clone()))
        .set_default();
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.answer_polls([pending(), token()]);
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay(&directory, stub.app(), &["octocat"], |config| config)
        .await
        .unwrap();
    let (workstation, laptop) = (key(), key());

    let (mut server, login) = Server::proven(&relay, &workstation).await;
    assert_eq!(login, None);
    assert_eq!(
        server.begin_login().await,
        (
            "https://github.com/login/device".to_owned(),
            "WDJB-MJHT".to_owned()
        ),
        "the address and the code GitHub gave reach the Server"
    );
    assert_eq!(server.hear().await, done_as("octocat"));
    let accounts = relay.store().accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        (
            accounts[0].provider.as_str(),
            accounts[0].subject.as_str(),
            accounts[0].username.as_str()
        ),
        ("github", "583231", "octocat")
    );

    // Renamed at GitHub, the user logs in from another Server and is the
    // same Account, known by their new name from then on.
    stub.log_in_as(OCTOCAT, "octocat-renamed");
    assert_eq!(
        Server::logging_in(&relay, &laptop).await,
        done_as("octocat-renamed")
    );
    let accounts = relay.store().accounts().await.unwrap();
    assert_eq!(accounts.len(), 1, "a renamed user keeps their Account");
    assert_eq!(accounts[0].username, "octocat-renamed");
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat-renamed"))
    );
    relay.shutdown().await.unwrap();

    for (file, bytes) in database_files(&directory) {
        for secret in [TOKEN, REFRESH_TOKEN, DEVICE_CODE] {
            assert!(!contains(&bytes, secret), "{file} holds {secret}");
        }
    }
    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for secret in [TOKEN, REFRESH_TOKEN, DEVICE_CODE] {
        assert!(!log.contains(secret), "the log holds {secret}: {log}");
    }
}

#[tokio::test]
async fn a_login_its_server_abandons_is_asked_after_at_github_no_more() {
    let stub = Stub::start().await;
    stub.answer_polls([pending()]);
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay(&directory, stub.app(), &[], |config| config)
        .await
        .unwrap();
    let (mut server, _) = Server::proven(&relay, &key()).await;
    server.begin_login().await;
    let polls = || stub.seen("/login/oauth/access_token").len();
    timeout(DEADLINE, async {
        while polls() < 3 {
            tokio::time::sleep(SECOND).await;
        }
    })
    .await
    .expect("the Relay asks after the login");

    drop(server);
    // Many times the interval between askings, once the Relay has had time
    // to see the Server go, and as many again.
    let settled = SECOND * 5 * 40;
    tokio::time::sleep(settled).await;
    let after_going = polls();
    tokio::time::sleep(settled).await;
    assert_eq!(polls(), after_going);
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_login_nobody_finishes_before_its_code_expires_is_refused_as_expired() {
    let stub = Stub::start().await;
    stub.script().begun = begun(30, 1);
    stub.answer_polls([pending()]);
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay(&directory, stub.app(), &[], |config| config)
        .await
        .unwrap();
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::LoginExpired), "{answer:?}");

    stub.answer_polls([pending(), polled_error("expired_token")]);
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::LoginExpired), "{answer:?}");
    stub.answer_polls([polled_error("access_denied")]);
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::LoginDenied), "{answer:?}");
    assert!(relay.store().accounts().await.unwrap().is_empty());
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_named_user_is_admitted_by_the_id_their_name_had_when_first_looked_up_whoever_takes_it_later()
 {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay(&directory, stub.app(), &[" OctoCat "], |config| config)
        .await
        .unwrap();
    assert_eq!(
        stub.seen("/users/octocat").len(),
        1,
        "the name is looked up as the Relay first starts naming it"
    );
    let workstation = key();
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat")
    );

    // Someone else, by another name, is not admitted.
    stub.log_in_as(1, "mona");
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");

    // The user gives the name up and someone else takes it: the Relay goes
    // on admitting the user, under their new name, and not who took it,
    // however often it starts again.
    stub.name(OCTOCAT, "octocat-renamed");
    stub.name(66_666, "octocat");
    relay.shutdown().await.unwrap();
    let relay = start_relay(&directory, stub.app(), &["octocat"], |config| config)
        .await
        .unwrap();
    stub.log_in_as(66_666, "octocat");
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    stub.log_in_as(OCTOCAT, "octocat-renamed");
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat-renamed")
    );
    assert_eq!(
        stub.seen("/users/octocat").len(),
        1,
        "a name is looked up once, and never again while it is named"
    );
    relay.shutdown().await.unwrap();
}

/// Waits until the Account of the user whose GitHub id is `id` reads as
/// lapsed, or as not, as `lapsed` says.
async fn until_lapsed(relay: &RunningRelay, id: u64, lapsed: bool) {
    timeout(DEADLINE, async {
        loop {
            let accounts = relay.store().accounts().await.unwrap();
            if accounts
                .iter()
                .any(|account| account.subject == id.to_string() && account.lapsed == lapsed)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the Account comes to stand as it should");
}

#[tokio::test]
async fn removing_a_name_lapses_the_account_it_admitted_as_the_relay_starts() {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.name(1, "mona");
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay(&directory, stub.app(), &["octocat", "mona"], |config| {
        config
    })
    .await
    .unwrap();
    let (workstation, desktop) = (key(), key());
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat")
    );
    stub.log_in_as(1, "mona");
    assert_eq!(Server::logging_in(&relay, &desktop).await, done_as("mona"));
    relay.shutdown().await.unwrap();

    // Started again without octocat's name, the Relay lapses octocat's
    // Account, and no other.
    let relay = start_relay(&directory, stub.app(), &["mona"], |config| config)
        .await
        .unwrap();
    until_lapsed(&relay, OCTOCAT, true).await;
    assert_eq!(Server::proven(&relay, &workstation).await.1, None);
    assert_eq!(
        Server::proven(&relay, &desktop).await.1,
        Some(github_account("mona"))
    );
    stub.log_in_as(OCTOCAT, "octocat");
    let answer = Server::logging_in(&relay, &workstation).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    relay.shutdown().await.unwrap();

    // Named once more, the name is looked up afresh, as the operator writes
    // it anew.
    let relay = start_relay(&directory, stub.app(), &["octocat", "mona"], |config| {
        config
    })
    .await
    .unwrap();
    assert_eq!(stub.seen("/users/octocat").len(), 2);
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat")
    );
    until_lapsed(&relay, OCTOCAT, false).await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_relay_refuses_to_start_naming_a_user_it_cannot_look_up_and_starts_without_github_once_it_has()
 {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.script().users.insert(
        "github".to_owned(),
        json!({ "login": "github", "id": 9_919, "type": "Organization" }),
    );
    let directory = tempfile::tempdir().unwrap();
    start_relay(&directory, stub.app(), &["octocat"], |config| config)
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    for (named, said) in [
        (&["octocat", "nobody-here"][..], "nobody-here"),
        (&["octocat", "github"][..], "organization"),
    ] {
        let refused = start_relay(&directory, stub.app(), named, |config| config)
            .await
            .err()
            .expect("the Relay refuses to start");
        let refused = format!("{refused:#}");
        assert!(refused.contains(said), "{refused}");
    }
    stub.script().rate_limited = true;
    let refused = start_relay(&directory, stub.app(), &["octocat", "mona"], |config| {
        config
    })
    .await
    .err()
    .expect("the Relay refuses to start naming a user GitHub will not look up");
    assert!(format!("{refused:#}").contains("`mona`"), "{refused:#}");

    // octocat was looked up as the Relay first started naming them, and is
    // kept: a Relay naming only users it has looked up starts though GitHub
    // cannot be reached.
    let closed = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let nowhere = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let unreachable = GitHubApp::new(CLIENT_ID).with_addresses(&nowhere, &nowhere);
    let relay = start_relay(&directory, unreachable.clone(), &["octocat"], |config| {
        config
    })
    .await
    .expect("a Relay that has looked up every name it names starts without GitHub");
    relay.shutdown().await.unwrap();
    let refused = start_relay(&directory, unreachable, &["octocat", "mona"], |config| {
        config
    })
    .await
    .err()
    .expect("the Relay refuses to start naming a user GitHub cannot be asked about");
    let refused = format!("{refused:#}");
    assert!(
        refused.contains("`mona`") && refused.contains("GitHub could not be reached"),
        "{refused}"
    );
}

#[tokio::test]
async fn github_that_cannot_be_reached_lapses_nobody_and_admits_nobody_new() {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    let directory = tempfile::tempdir().unwrap();
    let interval = Duration::from_millis(5);
    let relay = start_relay(&directory, stub.app(), &["octocat"], |config| {
        config.with_admission_interval(interval)
    })
    .await
    .unwrap();
    let workstation = key();
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat")
    );
    relay.shutdown().await.unwrap();

    let closed = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let nowhere = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let relay = start_relay(
        &directory,
        GitHubApp::new(CLIENT_ID).with_addresses(&nowhere, &nowhere),
        &["octocat"],
        |config| config.with_admission_interval(interval),
    )
    .await
    .unwrap();
    // Many passes of the Relay's own checks come and go.
    tokio::time::sleep(interval * 20).await;
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    let answer = Server::logging_in_unbegun(&relay, &key()).await;
    let RelayMessage::Refused {
        refusal: Refusal::LoginUnavailable,
        message,
    } = &answer
    else {
        panic!("a login GitHub cannot be asked about is refused, not {answer:?}");
    };
    assert!(message.contains("GitHub could not be reached"), "{message}");
    assert_eq!(relay.store().accounts().await.unwrap().len(), 1);
    relay.shutdown().await.unwrap();
}

impl Server {
    /// Asks `relay` to begin a login for `key`, on a connection of its own:
    /// what the Relay answers.
    async fn logging_in_unbegun(relay: &RunningRelay, key: &KeyPair) -> RelayMessage {
        let (mut server, _) = Self::proven(relay, key).await;
        server
            .say(&ServerMessage::BeginLogin {
                hostname: "workstation".to_owned(),
            })
            .await;
        server.hear().await
    }
}

#[tokio::test]
async fn the_relay_binary_refuses_to_start_naming_a_user_it_cannot_look_up_saying_who() {
    let directory = tempfile::tempdir().unwrap();
    // GitHub is reached through a proxy that is not there, so the binary
    // cannot ask GitHub itself.
    let closed = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let mut binary = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"));
    binary
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--database")
        .arg(directory.path().join("relay.db"))
        .arg("--public-address")
        .arg(PUBLIC_ADDRESS)
        .args(["--github-client-id", CLIENT_ID, "--admit-user", "mona"]);
    for name in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        binary.env(name, &proxy);
    }
    let ran = timeout(
        DEADLINE,
        binary
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the binary stops in time")
    .unwrap();
    assert!(!ran.status.success());
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(
        said.contains("`mona`") && said.contains("GitHub could not be reached"),
        "{said}"
    );
    assert!(ran.stdout.is_empty());

    // Naming a user without the GitHub App to look them up through is
    // refused before anything is tried.
    let ran = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"))
        .args(["--public-address", PUBLIC_ADDRESS, "--admit-user", "mona"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!ran.status.success());
    assert!(
        String::from_utf8_lossy(&ran.stderr).contains("--github-client-id"),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}
