//! GitHub as a Relay's identity provider, and the admission rules naming
//! GitHub users and organizations, spoken to through a stub of GitHub that
//! answers its device-login, user, users, app installation and membership
//! endpoints as GitHub does — never GitHub itself.

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
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use rustls::pki_types::pem::PemObject as _;
use serde_json::{Value, json};
use suru_relay::{
    Admission, AdmissionRule, GitHub, GitHubApp, GitHubAppKey, Identity, IdentityProvider,
    LoginRefusal, LookUpFailed, RelayConfig, RunningRelay, Undecided,
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

/// A host nothing is ever at, which a proxy is bypassed for where a test
/// must say it is bypassed for something.
const NO_HOST: &str = "no-proxy.invalid";

/// The user the stub logs in as, unless a test says otherwise.
const OCTOCAT: u64 = 583_231;

/// Another user, who is a member of nothing unless a test says so.
const MONA: u64 = 1;

/// The organization the stub knows as `acme`, unless a test says otherwise,
/// and the app's installation on it.
const ACME: u64 = 9_919;
const ACME_INSTALLATION: u64 = 4_242;

/// The private key of the stub's GitHub App, made for these tests alone, as
/// GitHub issues one — PKCS#1 — and as PKCS#8.
const APP_KEY: &str = include_str!("fixtures/github-app-key.pem");
const APP_KEY_PKCS8: &str = include_str!("fixtures/github-app-key-pkcs8.pem");

/// A line from the middle of the app's private key, which nothing the Relay
/// writes may hold.
fn app_key_line() -> &'static str {
    APP_KEY.lines().nth(5).unwrap()
}

fn app_key() -> GitHubAppKey {
    GitHubAppKey::from_pem(APP_KEY.as_bytes()).unwrap()
}

/// A request the stub was sent.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    form: HashMap<String, String>,
    body: String,
    at: Instant,
}

/// What the stub answers.
struct Script {
    /// What the device-code endpoint answers, but for the device code and
    /// the user code, which are each login's own.
    begun: (StatusCode, Value),
    /// How many logins it has begun.
    logins: u64,
    /// What the token endpoint answers to each asking after a login, in
    /// turn; the last answers every asking after it. A login with answers
    /// of its own, by its device code, is answered with those.
    polled: VecDeque<Answered>,
    polled_for: HashMap<String, VecDeque<Answered>>,
    /// Who the token it gives reads as.
    user: Value,
    /// Who goes by each name, by its name in lower case.
    users: HashMap<String, Value>,
    /// How it refuses to say who goes by a name for a limit on how often it
    /// is asked, where it does.
    rate_limited: Option<RateLimit>,
    /// Whether it never answers who goes by a name.
    hang: bool,
    /// How it answers who goes by a name with more than the Relay reads,
    /// where it does.
    oversized: Option<Oversized>,
    /// Each organization, by its name in lower case.
    organizations: HashMap<String, Org>,
    /// Whether it refuses every token the app signs for itself, as it does
    /// one a key it does not know the app by signed.
    app_refused: bool,
    /// Whether it fails every request the app makes as itself.
    app_down: bool,
    /// How long each token of an installation's it gives lasts.
    token_lifetime: Duration,
    /// The tokens of installations' it takes, each with the installation it
    /// is of.
    installation_tokens: HashMap<String, u64>,
    /// How many tokens of installations' it has given.
    tokens_given: u64,
    /// Until when it refuses to be asked about a membership, for a limit on
    /// how often it is asked that it says lifts in `retry_after` seconds.
    memberships_limited_until: Option<Instant>,
    /// How many more times it answers who is a member before it refuses
    /// for a limit that it says lifts in `retry_after` seconds, where a test
    /// hands out its answers one at a time.
    membership_quota: Option<usize>,
    /// Whether it refuses to be asked about a membership for a limit on
    /// asking too much at once that it says nothing of the end of, while it
    /// has plenty of its hourly allowance left, and a reset of it behind it.
    memberships_limited_unsaid: bool,
    /// Whether it fails every asking about a membership.
    memberships_down: bool,
    /// How many of the askings about a membership still to come wait for one
    /// another, and what they wait at.
    memberships_gate: Option<(usize, Arc<tokio::sync::Barrier>)>,
    /// The names it answered who is a member by, in turn: neither refused
    /// for a limit nor otherwise.
    memberships_answered: Vec<String>,
    /// How many askings about a membership it refused for its quota.
    memberships_refused: usize,
    retry_after: u64,
    seen: Vec<Seen>,
}

/// An organization, as the stub knows it.
#[derive(Clone)]
struct Org {
    id: u64,
    login: String,
    /// `Organization`, or `User` where it is a user's account.
    kind: &'static str,
    /// The app's installation on it, where the app is installed.
    installation: Option<u64>,
    /// Whether the installation may read who its members are.
    reads_members: bool,
    /// Whether the installation is suspended.
    suspended: bool,
    /// Whether a token of the installation's that may read who its members
    /// are is given, as it is where it may.
    gives_tokens: bool,
    /// Whether the token given says it may read who its members are, as it
    /// does where it may.
    tokens_read_members: bool,
    /// Each member's membership — `active`, or `pending` — by their id.
    members: HashMap<u64, &'static str>,
}

/// An answer of the token endpoint's.
#[derive(Clone)]
struct Answered {
    status: StatusCode,
    body: String,
}

impl From<Value> for Answered {
    fn from(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.to_string(),
        }
    }
}

/// `body` answered with `status`.
fn answered(status: StatusCode, body: impl ToString) -> Answered {
    Answered {
        status,
        body: body.to_string(),
    }
}

#[derive(Clone, Copy)]
enum RateLimit {
    /// The limit on requests from one address each hour, which says when it
    /// resets.
    Primary,
    /// A limit on asking too much at once, which says how long to wait.
    Secondary,
    /// A limit that says nothing of when it lifts.
    Unsaid,
    /// A limit on asking too much at once that says nothing of when it
    /// lifts, with much of the hourly limit left and its reset near.
    SecondaryBesideReset,
    /// The hourly limit spent, saying it reset at a time already past.
    PrimaryReset,
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
            logins: 0,
            polled: VecDeque::from([token().into()]),
            polled_for: HashMap::new(),
            user: user(OCTOCAT, "octocat"),
            users: HashMap::new(),
            rate_limited: None,
            hang: false,
            oversized: None,
            organizations: HashMap::new(),
            app_refused: false,
            app_down: false,
            token_lifetime: Duration::from_secs(60 * 60),
            installation_tokens: HashMap::new(),
            tokens_given: 0,
            memberships_limited_until: None,
            membership_quota: None,
            memberships_limited_unsaid: false,
            memberships_down: false,
            memberships_gate: None,
            memberships_answered: Vec::new(),
            memberships_refused: 0,
            retry_after: 60,
            seen: Vec::new(),
        }));
        let app = Router::new()
            .route("/login/device/code", post(device_code))
            .route("/login/oauth/access_token", post(access_token))
            .route("/user", get(authenticated_user))
            .route("/user/{id}", get(user_by_id))
            .route("/users/{name}", get(named_user))
            .route("/orgs/{org}/installation", get(organization_installation))
            .route("/orgs/{org}/memberships/{username}", get(membership))
            .route("/app/installations/{id}", get(installation_by_id))
            .route(
                "/app/installations/{id}/access_tokens",
                post(installation_token),
            )
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
    fn answer_polls<A: Into<Answered>>(&self, answers: impl IntoIterator<Item = A>) {
        self.script().polled = answers.into_iter().map(Into::into).collect();
    }

    /// Answers each asking after the login whose device code is
    /// `device_code` with `answers`, in turn.
    fn answer_polls_for(&self, device_code: &str, answers: impl IntoIterator<Item = Value>) {
        self.script().polled_for.insert(
            device_code.to_owned(),
            answers.into_iter().map(Into::into).collect(),
        );
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

    /// The GitHub App, reached at this stub, checking the members of
    /// organizations with its private key.
    fn app_with_key(&self) -> GitHubApp {
        self.app().with_private_key(app_key())
    }

    /// Has the app installed on the organization `login`, whose id is `id`,
    /// by the installation `installation`, reading who its members are; and
    /// makes no one a member of it.
    fn organization(&self, login: &str, id: u64, installation: u64) {
        self.script().organizations.insert(
            login.to_lowercase(),
            Org {
                id,
                login: login.to_owned(),
                kind: "Organization",
                installation: Some(installation),
                reads_members: true,
                suspended: false,
                gives_tokens: true,
                tokens_read_members: true,
                members: HashMap::new(),
            },
        );
    }

    /// Has the organization `login` be as `change` leaves it.
    fn change_organization(&self, login: &str, change: impl FnOnce(&mut Org)) {
        change(
            self.script()
                .organizations
                .get_mut(&login.to_lowercase())
                .unwrap(),
        );
    }

    /// Has the organization `login` go by `renamed` from now on.
    fn rename_organization(&self, login: &str, renamed: &str) {
        let mut script = self.script();
        let mut organization = script.organizations.remove(&login.to_lowercase()).unwrap();
        renamed.clone_into(&mut organization.login);
        script
            .organizations
            .insert(renamed.to_lowercase(), organization);
    }

    /// Has the user `id`'s membership of the organization `login` be
    /// `state` — `active`, or `pending` — or, with none, makes them no
    /// member.
    fn membership(&self, login: &str, id: u64, state: Option<&'static str>) {
        self.change_organization(login, |organization| match state {
            Some(state) => {
                organization.members.insert(id, state);
            }
            None => {
                organization.members.remove(&id);
            }
        });
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
    record_body(script, path, headers, form, String::new());
}

fn record_body(
    script: &Mutex<Script>,
    path: String,
    headers: HeaderMap,
    form: HashMap<String, String>,
    body: String,
) {
    script.lock().unwrap().seen.push(Seen {
        path,
        headers,
        form,
        body,
        at: Instant::now(),
    });
}

/// The bearer token `headers` carry.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The claims of the token the app signed for itself that `headers` carry,
/// where it is one, signed by the app's private key, and GitHub would take
/// it now: named by the app's client ID, made no later than now, and
/// expiring no more than ten minutes after it says it was made, and later
/// than now.
fn app_claims(headers: &HeaderMap) -> Option<Value> {
    let token = bearer(headers)?;
    let mut parts = token.split('.');
    let (header, claims, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let pem = rustls::pki_types::PrivateKeyDer::from_pem_slice(APP_KEY.as_bytes()).ok()?;
    let rustls::pki_types::PrivateKeyDer::Pkcs1(der) = pem else {
        return None;
    };
    let key = ring::signature::RsaKeyPair::from_der(der.secret_pkcs1_der()).ok()?;
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        key.public().as_ref(),
    )
    .verify(
        format!("{header}.{claims}").as_bytes(),
        &URL_SAFE_NO_PAD.decode(signature).ok()?,
    )
    .ok()?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
    if header["alg"] != "RS256" {
        return None;
    }
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (issued, expires) = (claims["iat"].as_u64()?, claims["exp"].as_u64()?);
    (claims["iss"] == CLIENT_ID && issued <= now && expires > now && expires - issued <= 600)
        .then_some(claims)
}

/// Why the stub refuses what the app asks as itself, where it does.
fn refused_the_app(script: &Script, headers: &HeaderMap) -> Option<Response> {
    if script.app_down {
        return Some((StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>").into_response());
    }
    if script.app_refused || app_claims(headers).is_none() {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "message": "A JSON web token could not be decoded" })),
            )
                .into_response(),
        );
    }
    None
}

/// The installation of the app's that the token `headers` carry is of,
/// where it is one the stub takes.
fn installation_of(script: &Script, headers: &HeaderMap) -> Option<u64> {
    script.installation_tokens.get(bearer(headers)?).copied()
}

fn bad_credentials() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "message": "Bad credentials" })),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "message": "Not Found" })),
    )
        .into_response()
}

/// The app's installation on `organization`, as GitHub tells of it.
fn installation_json(organization: &Org) -> Value {
    let mut permissions = json!({ "metadata": "read" });
    if organization.reads_members {
        permissions["members"] = json!("read");
    }
    json!({
        "id": organization.installation,
        "client_id": CLIENT_ID,
        "account": {
            "login": organization.login,
            "id": organization.id,
            "type": organization.kind,
        },
        "repository_selection": "selected",
        "target_type": organization.kind,
        "permissions": permissions,
        "events": [],
        "suspended_at": organization.suspended.then_some("2026-09-01T00:00:00Z"),
    })
}

async fn organization_installation(
    State(script): State<Arc<Mutex<Script>>>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Response {
    record(
        &script,
        format!("/orgs/{org}/installation"),
        headers.clone(),
        HashMap::new(),
    );
    let script = script.lock().unwrap();
    if let Some(refused) = refused_the_app(&script, &headers) {
        return refused;
    }
    match script.organizations.get(&org.to_lowercase()) {
        Some(organization) if organization.installation.is_some() => {
            axum::Json(installation_json(organization)).into_response()
        }
        _ => not_found(),
    }
}

async fn installation_by_id(
    State(script): State<Arc<Mutex<Script>>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    record(
        &script,
        format!("/app/installations/{id}"),
        headers.clone(),
        HashMap::new(),
    );
    let script = script.lock().unwrap();
    if let Some(refused) = refused_the_app(&script, &headers) {
        return refused;
    }
    match script
        .organizations
        .values()
        .find(|organization| organization.installation == Some(id))
    {
        Some(organization) => axum::Json(installation_json(organization)).into_response(),
        None => not_found(),
    }
}

async fn installation_token(
    State(script): State<Arc<Mutex<Script>>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    body: String,
) -> Response {
    record_body(
        &script,
        format!("/app/installations/{id}/access_tokens"),
        headers.clone(),
        HashMap::new(),
        body.clone(),
    );
    let mut script = script.lock().unwrap();
    if let Some(refused) = refused_the_app(&script, &headers) {
        return refused;
    }
    let Some(organization) = script
        .organizations
        .values()
        .find(|organization| organization.installation == Some(id))
        .cloned()
    else {
        return not_found();
    };
    if organization.suspended {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "message": "This installation has been suspended" })),
        )
            .into_response();
    }
    let asked: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !(organization.reads_members && organization.gives_tokens)
        || asked["permissions"]
            .as_object()
            .is_some_and(|asked| asked.keys().any(|permission| permission != "members"))
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "message": "The permissions requested are not granted to this installation."
            })),
        )
            .into_response();
    }
    script.tokens_given += 1;
    let token = format!("ghs_InstallationToken{:04}", script.tokens_given);
    script.installation_tokens.insert(token.clone(), id);
    let expires_at = time::OffsetDateTime::now_utc() + script.token_lifetime;
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "token": token,
            "expires_at": expires_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "permissions": if organization.tokens_read_members {
                json!({ "members": "read" })
            } else {
                json!({ "metadata": "read" })
            },
            "repository_selection": "selected",
        })),
    )
        .into_response()
}

async fn user_by_id(
    State(script): State<Arc<Mutex<Script>>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    record(
        &script,
        format!("/user/{id}"),
        headers.clone(),
        HashMap::new(),
    );
    let script = script.lock().unwrap();
    let Some(installation) = installation_of(&script, &headers) else {
        return bad_credentials();
    };
    if installed_on(&script, installation).is_some_and(|organization| organization.suspended) {
        return suspended();
    }
    match script
        .users
        .values()
        .find(|user| user["id"].as_u64() == Some(id))
    {
        Some(user) => axum::Json(user.clone()).into_response(),
        None => not_found(),
    }
}

async fn membership(
    State(script): State<Arc<Mutex<Script>>>,
    Path((org, username)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    record(
        &script,
        format!("/orgs/{org}/memberships/{username}"),
        headers.clone(),
        HashMap::new(),
    );
    let gate = match &mut script.lock().unwrap().memberships_gate {
        Some((waiting, gate)) if *waiting > 0 => {
            *waiting -= 1;
            Some(gate.clone())
        }
        _ => None,
    };
    if let Some(gate) = gate {
        gate.wait().await;
    }
    let mut script = script.lock().unwrap();
    let Some(installation) = installation_of(&script, &headers) else {
        return bad_credentials();
    };
    if script.memberships_down {
        return (StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>").into_response();
    }
    let limited = |retry_after: u64| {
        (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "4000".to_owned()),
                ("retry-after", retry_after.to_string()),
            ],
            axum::Json(json!({ "message": "You have exceeded a secondary rate limit." })),
        )
            .into_response()
    };
    let retry_after = script.retry_after;
    if script
        .memberships_limited_until
        .is_some_and(|until| Instant::now() < until)
    {
        return limited(retry_after);
    }
    if script.memberships_limited_unsaid {
        let reset = time::OffsetDateTime::now_utc().unix_timestamp() - 10;
        return (
            StatusCode::FORBIDDEN,
            [
                ("x-ratelimit-remaining", "4000".to_owned()),
                ("x-ratelimit-reset", reset.to_string()),
            ],
            axum::Json(json!({ "message": "You have exceeded a secondary rate limit." })),
        )
            .into_response();
    }
    if let Some(quota) = &mut script.membership_quota {
        if *quota == 0 {
            script.memberships_refused += 1;
            return limited(retry_after);
        }
        *quota -= 1;
    }
    script.memberships_answered.push(username.to_lowercase());
    // An installation suspended, or that may not read who its
    // organization's members are, is told nothing of them — the latter in a
    // 404, as GitHub answers what it will not show.
    match installed_on(&script, installation) {
        Some(installed) if installed.suspended => return suspended(),
        Some(installed) if !installed.reads_members => return not_found(),
        _ => {}
    }
    let Some(organization) = script.organizations.get(&org.to_lowercase()) else {
        return not_found();
    };
    if organization.installation != Some(installation) {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "message": "Resource not accessible by integration" })),
        )
            .into_response();
    }
    let Some(user) = script.users.get(&username.to_lowercase()) else {
        return not_found();
    };
    match user["id"]
        .as_u64()
        .and_then(|id| organization.members.get(&id))
    {
        Some(state) => axum::Json(json!({
            "state": state,
            "role": "member",
            "organization": {
                "login": organization.login,
                "id": organization.id,
            },
            "user": user,
        }))
        .into_response(),
        None => not_found(),
    }
}

/// The organization the installation `installation` is on.
fn installed_on(script: &Script, installation: u64) -> Option<&Org> {
    script
        .organizations
        .values()
        .find(|organization| organization.installation == Some(installation))
}

fn suspended() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({ "message": "This installation has been suspended" })),
    )
        .into_response()
}

async fn device_code(
    State(script): State<Arc<Mutex<Script>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    record(&script, "/login/device/code".to_owned(), headers, form);
    let mut script = script.lock().unwrap();
    let (status, mut body) = script.begun.clone();
    // Each login after the first is told apart by codes of its own.
    let login = script.logins;
    script.logins += 1;
    if login > 0 && body.get("device_code").is_some() {
        body["device_code"] = json!(format!("{DEVICE_CODE}{login}"));
        body["user_code"] = json!(format!("WDJB-{login:04}"));
    }
    (status, axum::Json(body)).into_response()
}

async fn access_token(
    State(script): State<Arc<Mutex<Script>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let device_code = form.get("device_code").cloned().unwrap_or_default();
    record(
        &script,
        "/login/oauth/access_token".to_owned(),
        headers,
        form,
    );
    let mut script = script.lock().unwrap();
    let script = &mut *script;
    let answers = script
        .polled_for
        .get_mut(&device_code)
        .unwrap_or(&mut script.polled);
    let answer = if answers.len() > 1 {
        answers.pop_front().unwrap()
    } else {
        answers.front().cloned().unwrap_or_else(|| pending().into())
    };
    (
        answer.status,
        [(header::CONTENT_TYPE, "application/json")],
        answer.body,
    )
        .into_response()
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
    match rate_limited {
        Some(RateLimit::Primary) => {
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
        Some(RateLimit::Secondary) => {
            return (
                StatusCode::FORBIDDEN,
                [("x-ratelimit-remaining", "41"), ("retry-after", "60")],
                axum::Json(json!({ "message": "You have exceeded a secondary rate limit." })),
            )
                .into_response();
        }
        Some(RateLimit::SecondaryBesideReset) => {
            let reset = time::OffsetDateTime::now_utc().unix_timestamp() + 10;
            return (
                StatusCode::FORBIDDEN,
                [
                    ("x-ratelimit-remaining", "4000".to_owned()),
                    ("x-ratelimit-reset", reset.to_string()),
                ],
                axum::Json(json!({ "message": "You have exceeded a secondary rate limit." })),
            )
                .into_response();
        }
        Some(RateLimit::PrimaryReset) => {
            let reset = time::OffsetDateTime::now_utc().unix_timestamp() - 10;
            return (
                StatusCode::FORBIDDEN,
                [
                    ("x-ratelimit-remaining", "0".to_owned()),
                    ("x-ratelimit-reset", reset.to_string()),
                ],
                axum::Json(json!({ "message": "API rate limit exceeded for 127.0.0.1." })),
            )
                .into_response();
        }
        Some(RateLimit::Unsaid) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({ "message": "Too many requests." })),
            )
                .into_response();
        }
        None => {}
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
        Identity {
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
async fn how_a_login_went_is_read_from_what_github_says_whatever_status_it_says_it_with() {
    for (answers, expected) in [
        // As the device flow's standard has it, an error comes with a 400.
        (
            vec![
                answered(StatusCode::BAD_REQUEST, pending()),
                answered(StatusCode::BAD_REQUEST, polled_error("access_denied")),
            ],
            Err(LoginRefusal::Denied),
        ),
        (
            vec![answered(StatusCode::BAD_REQUEST, pending()), token().into()],
            Ok("583231"),
        ),
    ] {
        let stub = Stub::start().await;
        stub.answer_polls(answers);
        let github = stub.github();
        let login = github.begin_login().await.unwrap();
        let finished = github.finish_login(&login).await;
        match expected {
            Ok(subject) => assert_eq!(finished.unwrap().subject, subject),
            Err(refusal) => assert_eq!(finished.unwrap_err(), refusal),
        }
    }
}

#[tokio::test]
async fn a_login_github_answers_with_a_failure_or_nonsense_while_asked_after_is_unavailable() {
    for (answer, said) in [
        (
            answered(StatusCode::BAD_GATEWAY, "<html>502 Bad Gateway</html>"),
            "502",
        ),
        (
            answered(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "message": "oops" }),
            ),
            "does not understand",
        ),
        (
            json!({ "token": "nothing the device flow says" }).into(),
            "does not understand",
        ),
        (answered(StatusCode::OK, "not json"), "200"),
    ] {
        let stub = Stub::start().await;
        stub.answer_polls([pending().into(), answer]);
        let github = stub.github();
        let login = github.begin_login().await.unwrap();
        let Err(LoginRefusal::Unavailable(reason)) = github.finish_login(&login).await else {
            panic!("a login GitHub answers {said} about is unavailable");
        };
        assert!(reason.contains(said), "{reason}");
        assert!(!reason.contains(DEVICE_CODE), "{reason}");
        assert_eq!(stub.seen("/user").len(), 0);
    }
}

#[tokio::test]
async fn logins_under_way_at_once_are_each_asked_after_by_their_own_device_code() {
    let stub = Stub::start().await;
    let github = stub.github();
    let (first, second) = (
        github.begin_login().await.unwrap(),
        github.begin_login().await.unwrap(),
    );
    assert_ne!(first.device_code, second.device_code);
    assert_ne!(first.user_code, second.user_code);
    stub.answer_polls_for(
        &first.device_code,
        [pending(), pending(), polled_error("access_denied")],
    );
    stub.answer_polls_for(&second.device_code, [pending(), token()]);

    let (denied, done) = tokio::join!(github.finish_login(&first), github.finish_login(&second));
    assert_eq!(denied, Err(LoginRefusal::Denied));
    assert_eq!(done.unwrap().subject, "583231");
    let asked_after = |login: &suru_relay::DeviceLogin| {
        stub.seen("/login/oauth/access_token")
            .iter()
            .filter(|seen| seen.form["device_code"] == login.device_code)
            .count()
    };
    assert_eq!((asked_after(&first), asked_after(&second)), (3, 2));
    assert_eq!(stub.seen("/login/oauth/access_token").len(), 5);
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
        Ok(Some(Identity {
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

    // Refused for a limit on how often GitHub is asked, the lookup says so,
    // and when GitHub may be asked again, where GitHub says.
    for (limit, when) in [
        (RateLimit::Primary, "at 2027-01-15T08:00:00Z"),
        (RateLimit::Secondary, "in 60 seconds"),
        // Saying nothing of when a limit lifts, or only when one it is not
        // limiting by does, GitHub asks to be left a minute at least.
        (RateLimit::Unsaid, "in 60 seconds"),
        (RateLimit::SecondaryBesideReset, "in 60 seconds"),
        (RateLimit::PrimaryReset, "in 60 seconds"),
    ] {
        stub.script().rate_limited = Some(limit);
        let Err(LookUpFailed(why)) = github.look_up("octocat").await else {
            panic!("a lookup GitHub refuses for a rate limit fails");
        };
        assert!(why.contains("limiting") && why.contains(when), "{why}");
        assert!(!why.contains("hour"), "{why}");
    }
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
    start_relay_admitting(directory, app, named, &[], configure).await
}

/// Starts a Relay as [`start_relay`] does, admitting as well the members of
/// the GitHub organizations `organizations`.
async fn start_relay_admitting(
    directory: &tempfile::TempDir,
    app: GitHubApp,
    named: &[&str],
    organizations: &[&str],
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
            .with_admission(
                Admission::nobody()
                    .with_named_users(named.iter().copied())
                    .with_organizations(organizations.iter().copied()),
            ),
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

/// A rule admitting nobody, which takes its time to say so.
struct AdmitsNobodySlowly(Duration);

#[async_trait::async_trait]
impl AdmissionRule for AdmitsNobodySlowly {
    async fn admits(&self, _provider: &str, _identity: &Identity) -> Result<bool, Undecided> {
        tokio::time::sleep(self.0).await;
        Ok(false)
    }
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
    // Account, and no other, before it serves anyone — though another of
    // its rules takes its time to say it does not admit octocat either.
    let relay = start_relay(&directory, stub.app(), &[], |config| {
        config.with_admission(
            Admission::by([
                Arc::new(AdmitsNobodySlowly(Duration::from_millis(200))) as Arc<dyn AdmissionRule>
            ])
            .with_named_users(["mona"]),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        None,
        "the very first connection after the Relay starts is refused"
    );
    assert_eq!(
        Server::proven(&relay, &desktop).await.1,
        Some(github_account("mona"))
    );
    stub.log_in_as(OCTOCAT, "octocat");
    let answer = Server::logging_in(&relay, &workstation).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    relay.shutdown().await.unwrap();

    // While the name is not named, its user gives it up and someone else
    // takes it. Named once more, it admits the user it admitted before, by
    // their new name, and not who took it: it was looked up once, and is not
    // looked up again.
    stub.name(OCTOCAT, "octocat-renamed");
    stub.name(66_666, "octocat");
    let relay = start_relay(&directory, stub.app(), &["octocat", "mona"], |config| {
        config
    })
    .await
    .unwrap();
    assert_eq!(stub.seen("/users/octocat").len(), 1);
    stub.log_in_as(66_666, "octocat");
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    stub.log_in_as(OCTOCAT, "octocat-renamed");
    assert_eq!(
        Server::logging_in(&relay, &workstation).await,
        done_as("octocat-renamed")
    );
    until_lapsed(&relay, OCTOCAT, false).await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_relay_that_refuses_to_start_keeps_every_name_as_it_was() {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.name(1, "mona");
    let directory = tempfile::tempdir().unwrap();
    start_relay(&directory, stub.app(), &["octocat"], |config| config)
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();

    // The user gives the name up and someone else takes it; then the
    // operator mistypes it, beside a name the Relay has yet to look up, and
    // the Relay refuses to start.
    stub.name(OCTOCAT, "octocat-renamed");
    stub.name(66_666, "octocat");
    let refused = start_relay(&directory, stub.app(), &["otcocat", "mona"], |config| {
        config
    })
    .await
    .err()
    .expect("the Relay refuses to start naming a user GitHub does not know");
    assert!(format!("{refused:#}").contains("`otcocat`"), "{refused:#}");

    // Put right, the name admits the user it admitted before and not who
    // took it, and the name a refused start looked up is looked up again.
    let relay = start_relay(&directory, stub.app(), &["octocat", "mona"], |config| {
        config
    })
    .await
    .unwrap();
    assert_eq!(stub.seen("/users/octocat").len(), 1);
    assert_eq!(stub.seen("/users/mona").len(), 2);
    stub.log_in_as(66_666, "octocat");
    let answer = Server::logging_in(&relay, &key()).await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    stub.log_in_as(OCTOCAT, "octocat-renamed");
    assert_eq!(
        Server::logging_in(&relay, &key()).await,
        done_as("octocat-renamed")
    );
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
    stub.script().rate_limited = Some(RateLimit::Primary);
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
    // cannot ask GitHub itself — bypassed for no host GitHub is at, so no
    // bypass the operating system names (Windows' ProxyOverride, say) is
    // consulted in its place.
    let closed = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let mut binary = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"));
    binary
        .arg("run")
        .arg("--listen-http")
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
            .env("NO_PROXY", NO_HOST)
            .env("no_proxy", NO_HOST)
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
        .args([
            "run",
            "--listen-http",
            "127.0.0.1:0",
            "--public-address",
            PUBLIC_ADDRESS,
            "--admit-user",
            "mona",
        ])
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

impl Server {
    /// Proves `key`, whose Login stands, and waits to be reached on this
    /// connection.
    async fn waiting(relay: &RunningRelay, key: &KeyPair) -> Self {
        let (mut server, login) = Self::proven(relay, key).await;
        assert!(login.is_some(), "the Login stands");
        server.say(&ServerMessage::Wait).await;
        assert_eq!(server.hear().await, RelayMessage::Waiting);
        server
    }

    /// Joins `joining` to the waiting `serving`, both of one Account: the
    /// two ends of the join.
    async fn joined(relay: &RunningRelay, serving: &KeyPair, joining: &KeyPair) -> (Self, Self) {
        let mut waiting = Self::waiting(relay, serving).await;
        let (mut asking, _) = Self::proven(relay, joining).await;
        asking
            .say(&ServerMessage::Join {
                server: Bytes(serving.subject_public_key_info()),
            })
            .await;
        let RelayMessage::Reach { join } = waiting.hear().await else {
            panic!("the Relay tells the waiting Server of the join");
        };
        let (mut taken_up, _) = Self::proven(relay, serving).await;
        taken_up.say(&ServerMessage::Accept { join }).await;
        assert_eq!(taken_up.hear().await, RelayMessage::Joined);
        assert_eq!(asking.hear().await, RelayMessage::Joined);
        (asking, taken_up)
    }

    /// Sends `bytes` over a joined connection.
    async fn carry(&mut self, bytes: &[u8]) {
        self.socket
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .expect("send bytes over the join");
    }

    /// The next bytes carried to this side of a join.
    async fn carried(&mut self) -> Vec<u8> {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay carries bytes in time")
                .expect("the join stays open")
                .expect("read what the Relay carried");
            match frame {
                Message::Binary(bytes) => return bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("a join carries bytes alone, not {other:?}"),
            }
        }
    }

    /// Whether the Relay has ended the connection.
    async fn ended(&mut self) -> bool {
        loop {
            match timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay ends the connection in time")
            {
                None | Some(Err(_) | Ok(Message::Close(_))) => return true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(_)) => return false,
            }
        }
    }
}

/// How often the Relay checks its Accounts again in the tests that watch it.
const RECHECK: Duration = Duration::from_millis(5);

/// Starts a Relay on the records in `directory` admitting the members of
/// `acme`, checking them every [`RECHECK`].
async fn start_admitting_acme(directory: &tempfile::TempDir, stub: &Stub) -> RunningRelay {
    start_relay_admitting(directory, stub.app_with_key(), &[], &["acme"], |config| {
        config.with_admission_interval(RECHECK)
    })
    .await
    .unwrap()
}

/// A stub knowing octocat and mona, and the organization `acme`, which the
/// app is installed on, and octocat is a member of.
async fn acme() -> Stub {
    let stub = Stub::start().await;
    stub.name(OCTOCAT, "octocat");
    stub.name(MONA, "mona");
    stub.organization("acme", ACME, ACME_INSTALLATION);
    stub.membership("acme", OCTOCAT, Some("active"));
    stub
}

/// Logs `key` in at `relay` as the user `id`, who goes by `login`: how the
/// Relay says the login ended.
async fn logging_in_as(
    stub: &Stub,
    relay: &RunningRelay,
    key: &KeyPair,
    id: u64,
    login: &str,
) -> RelayMessage {
    stub.log_in_as(id, login);
    Server::logging_in(relay, key).await
}

/// Waits until the stub has been asked about memberships `more` more times,
/// so that many checks have been made since.
async fn asked_about_memberships(stub: &Stub, more: usize) {
    let asked = || {
        stub.script()
            .seen
            .iter()
            .filter(|seen| seen.path.contains("/memberships/"))
            .count()
    };
    let before = asked();
    timeout(DEADLINE, async {
        while asked() < before + more {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the Relay checks its Accounts on its own");
}

/// Waits until the stub has been asked about the app's installation on
/// `acme` `more` more times.
async fn asked_about_the_installation(stub: &Stub, more: usize) {
    let asked = || {
        stub.script()
            .seen
            .iter()
            .filter(|seen| {
                seen.path == "/orgs/acme/installation"
                    || seen.path == format!("/app/installations/{ACME_INSTALLATION}")
            })
            .count()
    };
    let before = asked();
    timeout(DEADLINE, async {
        while asked() < before + more {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the Relay asks after the installation on its own");
}

#[tokio::test]
async fn the_members_of_an_organization_are_admitted_through_the_apps_installation_and_nobody_else_is()
 {
    // The Relay's own log, as verbose as it can be asked to be.
    let captured = Captured::default();
    let writer = captured.clone();
    let _logging = tracing_subscriber::registry()
        .with(suru_relay::log_layer(Some("trace"), move || writer.clone()))
        .set_default();
    let stub = acme().await;
    stub.name(2, "hubot");
    stub.membership("acme", 2, Some("pending"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(&directory, stub.app_with_key(), &[], &["ACME "], |config| {
        config
    })
    .await
    .unwrap();

    // The app found its installation on the organization, and was given a
    // token of it that may read who its members are and nothing else, all
    // as the app itself, by a token its private key signed.
    assert_eq!(stub.seen("/orgs/acme/installation").len(), 1);
    let issued = stub.seen(&format!(
        "/app/installations/{ACME_INSTALLATION}/access_tokens"
    ));
    assert_eq!(issued.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&issued[0].body).unwrap(),
        json!({ "permissions": { "members": "read" } })
    );
    let claims = app_claims(&issued[0].headers).expect("a token GitHub takes as the app's");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let issued_at = claims["iat"].as_u64().unwrap();
    assert!(
        (now - 90..=now - 50).contains(&issued_at),
        "made a minute before it was, for GitHub's clock: {claims}"
    );

    // A member logs in, a private member as much as any; a user who is not
    // one, and one invited who has yet to accept, are not admitted.
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await,
        done_as("octocat")
    );
    for (id, login) in [(MONA, "mona"), (2, "hubot")] {
        let answer = logging_in_as(&stub, &relay, &key(), id, login).await;
        assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{login}");
    }
    let asked = stub.seen("/orgs/acme/memberships/octocat");
    assert_eq!(asked.len(), 1);
    assert_eq!(
        header_of(&asked[0].headers, "authorization"),
        "Bearer ghs_InstallationToken0001",
        "membership is asked through the installation, never by a token of the user's"
    );
    assert_eq!(
        stub.seen(&format!(
            "/app/installations/{ACME_INSTALLATION}/access_tokens"
        ))
        .len(),
        1,
        "the installation's token is kept while it lasts"
    );
    relay.shutdown().await.unwrap();

    let mut secrets = vec![
        TOKEN.to_owned(),
        REFRESH_TOKEN.to_owned(),
        "ghs_InstallationToken".to_owned(),
        app_key_line().to_owned(),
    ];
    secrets.extend(
        stub.script()
            .seen
            .iter()
            .filter_map(|seen| bearer(&seen.headers).map(str::to_owned)),
    );
    for (file, bytes) in database_files(&directory) {
        for secret in &secrets {
            assert!(!contains(&bytes, secret), "{file} holds {secret}");
        }
    }
    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for secret in &secrets {
        assert!(!log.contains(secret), "the log holds {secret}: {log}");
    }
}

#[tokio::test]
async fn a_member_removed_from_the_organization_lapses_at_the_next_check_with_their_joined_connections_cut_at_once()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let (workstation, laptop, desktop) = (key(), key(), key());
    for server in [&workstation, &laptop] {
        assert_eq!(
            logging_in_as(&stub, &relay, server, OCTOCAT, "octocat").await,
            done_as("octocat")
        );
    }
    assert_eq!(
        logging_in_as(&stub, &relay, &desktop, MONA, "mona").await,
        done_as("mona")
    );
    let (mut asking, mut taken_up) = Server::joined(&relay, &workstation, &laptop).await;
    asking.carry(b"before").await;
    assert_eq!(taken_up.carried().await, b"before");

    // Many checks find the member still is one.
    asked_about_memberships(&stub, 10).await;
    asking.carry(b"still").await;
    assert_eq!(taken_up.carried().await, b"still");

    stub.membership("acme", OCTOCAT, None);
    assert!(
        asking.ended().await && taken_up.ended().await,
        "the join carried for the Account is cut, on both sides"
    );
    until_lapsed(&relay, OCTOCAT, true).await;
    assert_eq!(Server::proven(&relay, &workstation).await.1, None);
    assert_eq!(
        Server::proven(&relay, &desktop).await.1,
        Some(github_account("mona")),
        "another member's Account stands as it did"
    );

    // Back in the organization, the user restores every Login of theirs by
    // logging in from any one Server.
    stub.membership("acme", OCTOCAT, Some("active"));
    assert_eq!(
        logging_in_as(&stub, &relay, &laptop, OCTOCAT, "octocat").await,
        done_as("octocat")
    );
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    relay.shutdown().await.unwrap();
}

/// Why the Relay refused to start, as its operator reads it.
async fn refused_to_start(
    directory: &tempfile::TempDir,
    app: GitHubApp,
    named: &[&str],
    organizations: &[&str],
) -> String {
    let refused = start_relay_admitting(directory, app, named, organizations, |config| config)
        .await
        .err()
        .expect("the Relay refuses to start");
    format!("{refused:#}")
}

/// An address nothing answers at.
async fn nowhere() -> String {
    let closed = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let nowhere = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    nowhere
}

#[tokio::test]
async fn a_relay_refuses_to_start_with_an_organization_rule_it_cannot_check_saying_which_and_why() {
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    /// What has the organization's members go unchecked, and what the
    /// Relay says of it.
    type Unchecked = (&'static str, fn(&mut Org), &'static str);
    let unchecked: [Unchecked; 6] = [
        (
            "not installed",
            |org| org.installation = None,
            "GitHub App is not installed on it",
        ),
        (
            "no members permission",
            |org| org.reads_members = false,
            "Members organization permission",
        ),
        (
            "members permission not granted",
            |org| org.gives_tokens = false,
            "Members organization permission",
        ),
        (
            "a token that may not read members",
            |org| org.tokens_read_members = false,
            "Members organization permission",
        ),
        ("suspended", |org| org.suspended = true, "suspended"),
        (
            "a user's",
            |org| org.kind = "User",
            "not of an organization",
        ),
    ];
    for (case, change, said) in unchecked {
        stub.organization("acme", ACME, ACME_INSTALLATION);
        stub.change_organization("acme", change);
        let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
        assert!(
            refused.contains("`acme`") && refused.contains(said),
            "{case}: {refused}"
        );
    }
    stub.organization("acme", ACME, ACME_INSTALLATION);
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["nowhere-inc"]).await;
    assert!(
        refused.contains("`nowhere-inc`") && refused.contains("knows no organization"),
        "{refused}"
    );
    let refused = refused_to_start(&directory, stub.app(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("no private key"),
        "{refused}"
    );
    stub.script().app_refused = true;
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("refused the GitHub App's credentials"),
        "{refused}"
    );
    stub.script().app_refused = false;
    let nowhere = nowhere().await;
    let unreachable = GitHubApp::new(CLIENT_ID)
        .with_addresses(&nowhere, &nowhere)
        .with_private_key(app_key());
    let refused = refused_to_start(&directory, unreachable, &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("GitHub could not be reached"),
        "{refused}"
    );
    stub.script().app_down = true;
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("502"),
        "{refused}"
    );
    stub.script().app_down = false;

    // A start refused for one organization keeps nothing it found of
    // another it looked up first, or of a user it named beside them: once
    // `acme` names another organization, and `mona` another user — no
    // member of it, so admitted by the name alone — the Relay starts naming
    // them as it finds them then.
    stub.organization("zenith", 4_000, 4_001);
    stub.change_organization("zenith", |org| org.installation = None);
    let looked_up = stub.seen("/orgs/acme/installation").len();
    let refused = refused_to_start(
        &directory,
        stub.app_with_key(),
        &["mona"],
        &["zenith", "acme"],
    )
    .await;
    assert!(refused.contains("`zenith`"), "{refused}");
    assert_eq!(
        stub.seen("/orgs/acme/installation").len(),
        looked_up + 1,
        "`acme` was looked up before `zenith` refused the start"
    );
    stub.organization("acme", 5_000, 5_001);
    stub.name(66_666, "mona");
    let relay = start_relay_admitting(
        &directory,
        stub.app_with_key(),
        &["mona"],
        &["acme"],
        |config| config,
    )
    .await
    .expect("nothing was kept of what a refused start found");
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), 66_666, "mona").await,
        done_as("mona")
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_relay_refuses_to_start_with_an_organization_github_cannot_be_asked_about_though_checked_on_an_earlier_start()
 {
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    start_admitting_acme(&directory, &stub)
        .await
        .shutdown()
        .await
        .unwrap();

    stub.script().app_down = true;
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("502"),
        "{refused}"
    );
    stub.script().app_down = false;
    let nowhere = nowhere().await;
    let unreachable = GitHubApp::new(CLIENT_ID)
        .with_addresses(&nowhere, &nowhere)
        .with_private_key(app_key());
    let refused = refused_to_start(&directory, unreachable, &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("GitHub could not be reached"),
        "{refused}"
    );
    stub.change_organization("acme", |org| org.installation = None);
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("not installed"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_running_relay_keeps_its_accounts_while_github_cannot_be_asked_about_members_and_admits_nobody_new_until_it_can()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    // GitHub fails whatever it is asked about the organization, and many of
    // the Relay's checks come and go, lapsing nobody and admitting nobody
    // new.
    stub.script().app_down = true;
    stub.script().memberships_down = true;
    asked_about_memberships(&stub, 10).await;
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    let answer = logging_in_as(&stub, &relay, &key(), MONA, "mona").await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::LoginUnavailable),
        "{answer:?}"
    );

    stub.script().app_down = false;
    stub.script().memberships_down = false;
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), MONA, "mona").await,
        done_as("mona")
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_installation_suspended_or_kept_from_its_organizations_members_while_the_relay_runs_lapses_nobody()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    /// What keeps the installation from the organization's members — or,
    /// undone, lets it see them again — and what it is called.
    type Change = (&'static str, fn(&mut Org, bool));
    let changes: [Change; 2] = [
        ("suspended", |org, suspended| org.suspended = suspended),
        ("kept from its members", |org, kept| {
            org.reads_members = !kept;
        }),
    ];
    for (case, change) in changes {
        stub.change_organization("acme", |org| change(org, true));
        // Found unable to be asked, the installation is asked after alone,
        // each time the Relay has held off asking through it a while.
        asked_about_the_installation(&stub, 3).await;
        assert_eq!(
            Server::proven(&relay, &workstation).await.1,
            Some(github_account("octocat")),
            "{case}: a member stands"
        );
        let answer = logging_in_as(&stub, &relay, &key(), MONA, "mona").await;
        assert_eq!(
            refusal(&answer),
            Some(&Refusal::LoginUnavailable),
            "{case}: {answer:?}"
        );

        stub.change_organization("acme", |org| change(org, false));
        asked_about_memberships(&stub, 1).await;
        assert_eq!(
            logging_in_as(&stub, &relay, &key(), MONA, "mona").await,
            done_as("mona"),
            "{case}"
        );
    }
    assert!(
        relay
            .store()
            .accounts()
            .await
            .unwrap()
            .iter()
            .all(|account| !account.lapsed)
    );
    relay.shutdown().await.unwrap();
}
#[tokio::test]
async fn an_organizations_name_another_organization_takes_admits_nobody_new() {
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    start_admitting_acme(&directory, &stub)
        .await
        .shutdown()
        .await
        .unwrap();

    // The organization takes another name, and another organization, which
    // mona is a member of, takes its own and installs the app.
    stub.rename_organization("acme", "acme-corp");
    stub.organization("acme", 7_777, 7_778);
    stub.membership("acme", MONA, Some("active"));
    let refused = refused_to_start(&directory, stub.app_with_key(), &[], &["acme"]).await;
    assert!(
        refused.contains("`acme`") && refused.contains("no longer names the organization"),
        "{refused}"
    );

    // Named by its new name, the organization admits its members, and
    // nobody else.
    let relay = start_relay_admitting(
        &directory,
        stub.app_with_key(),
        &[],
        &["acme-corp"],
        |config| config,
    )
    .await
    .unwrap();
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await,
        done_as("octocat")
    );
    let answer = logging_in_as(&stub, &relay, &key(), MONA, "mona").await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_organization_that_takes_another_name_while_the_relay_runs_goes_on_admitting_its_members()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    stub.rename_organization("acme", "acme-corp");
    asked_about_memberships(&stub, 10).await;
    assert!(!stub.seen("/orgs/acme-corp/memberships/octocat").is_empty());
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat")),
        "the member's Account stands"
    );
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), MONA, "mona").await,
        done_as("mona")
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_organization_whose_old_name_another_organization_takes_while_the_relay_runs_goes_on_admitting_its_members_alone()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    // The organization takes another name, and another, which hubot is a
    // member of, takes its old one: asked about it, GitHub refuses the
    // installation's token.
    stub.rename_organization("acme", "acme-corp");
    stub.organization("acme", 7_777, 7_778);
    stub.name(2, "hubot");
    stub.membership("acme", 2, Some("active"));
    asked_about_memberships(&stub, 10).await;
    assert!(!stub.seen("/orgs/acme-corp/memberships/octocat").is_empty());
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat")),
        "the member's Account stands"
    );
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), MONA, "mona").await,
        done_as("mona")
    );
    let answer = logging_in_as(&stub, &relay, &key(), 2, "hubot").await;
    assert_eq!(refusal(&answer), Some(&Refusal::NotAdmitted), "{answer:?}");

    // A member who leaves it lapses.
    stub.membership("acme-corp", OCTOCAT, None);
    until_lapsed(&relay, OCTOCAT, true).await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_member_is_checked_by_their_numeric_id_whoever_comes_to_go_by_their_name() {
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    let relay = start_admitting_acme(&directory, &stub).await;
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    // The member takes another name, and someone who is no member takes
    // theirs: the member goes on being admitted.
    stub.name(OCTOCAT, "octocat-renamed");
    stub.name(66_666, "octocat");
    asked_about_memberships(&stub, 10).await;
    assert!(
        !stub
            .seen("/orgs/acme/memberships/octocat-renamed")
            .is_empty()
    );
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat")),
        "the member's Account stands"
    );

    // The member leaves, and whoever took their old name joins: the
    // member's Account lapses.
    stub.membership("acme", OCTOCAT, None);
    stub.membership("acme", 66_666, Some("active"));
    until_lapsed(&relay, OCTOCAT, true).await;
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn github_limiting_how_often_it_is_asked_about_members_lapses_nobody_and_is_not_asked_again_until_the_limit_lifts()
 {
    // Long enough a second that the limit is told apart from the checks.
    let second = Duration::from_millis(20);
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(
        &directory,
        stub.app_with_key().with_second(second),
        &[],
        &["acme"],
        |config| config.with_admission_interval(RECHECK),
    )
    .await
    .unwrap();
    let workstation = key();
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    let limited_for = second * 60;
    stub.script().memberships_limited_until = Some(Instant::now() + limited_for);
    asked_about_memberships(&stub, 1).await;
    let limited = stub.script().seen.len();
    // Many checks come and go while the limit holds.
    tokio::time::sleep(limited_for / 4).await;
    assert_eq!(
        stub.script().seen.len(),
        limited,
        "GitHub is not asked again before it said it might be"
    );
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    let answer = logging_in_as(&stub, &relay, &key(), MONA, "mona").await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::LoginUnavailable),
        "{answer:?}"
    );

    // Once it lifts, GitHub is asked again.
    asked_about_memberships(&stub, 1).await;
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), MONA, "mona").await,
        done_as("mona")
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn checks_github_answers_only_so_many_of_at_a_time_come_to_every_account_in_turn_whatever_a_name_admits()
 {
    let stub = acme().await;
    // Two members of the organization, and a user admitted by name alone,
    // whose Accounts are checked in that order.
    let users = [(11, "eleven"), (12, "twelve"), (13, "thirteen")];
    for (id, login) in users {
        stub.name(id, login);
    }
    stub.membership("acme", 11, Some("active"));
    stub.membership("acme", 12, Some("active"));
    // Refused, the Relay holds off asking for the rest of a pass.
    stub.script().retry_after = 2;
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(
        &directory,
        stub.app_with_key(),
        &["thirteen"],
        &["acme"],
        |config| config.with_admission_interval(RECHECK),
    )
    .await
    .unwrap();
    for (id, login) in users {
        assert_eq!(
            logging_in_as(&stub, &relay, &key(), id, login).await,
            done_as(login)
        );
    }

    // GitHub answers who is a member once each time the test lets it, and
    // the second member leaves the organization.
    stub.script().membership_quota = Some(0);
    stub.membership("acme", 12, None);
    asked_about_memberships(&stub, 3).await;
    stub.script().memberships_answered.clear();
    for _ in 0..3 {
        // One answer, taken by the first Account a pass asks about, and a
        // refusal of the next, after which the pass asks nothing more.
        let refused = stub.script().memberships_refused;
        stub.script().membership_quota = Some(1);
        timeout(DEADLINE, async {
            while stub.script().membership_quota != Some(0)
                || stub.script().memberships_refused == refused
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the Relay asks GitHub again");
        if stub
            .script()
            .memberships_answered
            .last()
            .map(String::as_str)
            == Some("twelve")
        {
            break;
        }
    }
    assert_eq!(
        stub.script().memberships_answered,
        ["eleven", "twelve"],
        "the member who left is checked in turn, not only the one before them"
    );
    until_lapsed(&relay, 12, true).await;
    relay.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_token_of_the_installations_is_kept_until_shortly_before_it_expires_or_github_no_longer_takes_it()
 {
    let stub = acme().await;
    // Each token lasts less than the Relay keeps one for.
    stub.script().token_lifetime = Duration::from_secs(4 * 60);
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(&directory, stub.app_with_key(), &[], &["acme"], |config| {
        config
    })
    .await
    .unwrap();
    let given = || stub.script().tokens_given;
    assert_eq!(given(), 1);
    for login in 0..3 {
        assert_eq!(
            logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await,
            done_as("octocat")
        );
        assert_eq!(given(), 2 + login, "a token near its end is renewed");
    }
    stub.script().token_lifetime = Duration::from_secs(60 * 60);
    for _ in 0..3 {
        logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await;
    }
    assert_eq!(given(), 5, "a token is kept while it lasts");

    // GitHub stops taking the token: another is got, and asked with.
    stub.script().installation_tokens.clear();
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await,
        done_as("octocat")
    );
    assert_eq!(given(), 6);
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn checks_all_finding_at_once_that_github_no_longer_takes_the_token_get_one_new_token() {
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(&directory, stub.app_with_key(), &[], &["acme"], |config| {
        config
    })
    .await
    .unwrap();
    assert_eq!(stub.script().tokens_given, 1);

    // GitHub stops taking the token, and four logins ask about a member at
    // once with it.
    let logins = 4;
    stub.script().installation_tokens.clear();
    stub.script().memberships_gate = Some((logins, Arc::new(tokio::sync::Barrier::new(logins))));
    stub.log_in_as(OCTOCAT, "octocat");
    let keys = (0..logins).map(|_| key()).collect::<Vec<_>>();
    let answers =
        futures_util::future::join_all(keys.iter().map(|key| Server::logging_in(&relay, key)))
            .await;
    for answer in answers {
        assert_eq!(answer, done_as("octocat"));
    }
    assert_eq!(
        stub.script().tokens_given,
        2,
        "one token replaces the one GitHub no longer takes, for every check"
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_limit_github_says_nothing_of_the_end_of_holds_asking_off_a_minute_and_longer_each_time_it_recurs()
 {
    let second = Duration::from_millis(2);
    let stub = acme().await;
    let directory = tempfile::tempdir().unwrap();
    let relay = start_relay_admitting(
        &directory,
        stub.app_with_key().with_second(second),
        &[],
        &["acme"],
        |config| config.with_admission_interval(RECHECK),
    )
    .await
    .unwrap();
    assert_eq!(
        logging_in_as(&stub, &relay, &key(), OCTOCAT, "octocat").await,
        done_as("octocat")
    );

    // GitHub limits how often it is asked about members, saying nothing of
    // when that limit lifts, but only of a reset of another, already past.
    stub.script().memberships_limited_unsaid = true;
    let asked = || {
        stub.script()
            .seen
            .iter()
            .filter(|seen| seen.path.contains("/memberships/"))
            .map(|seen| seen.at)
            .collect::<Vec<_>>()
    };
    let before = asked().len();
    asked_about_memberships(&stub, 3).await;
    let at = asked()[before..].to_vec();
    let gaps = at
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    assert!(gaps[0] >= second * 60, "{gaps:?}");
    assert!(gaps[1] >= second * 120, "{gaps:?}");
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_an_organization_lapses_the_accounts_it_alone_admitted_and_a_user_it_and_a_name_admit_stands_while_either_does()
 {
    let stub = acme().await;
    stub.membership("acme", MONA, Some("active"));
    let directory = tempfile::tempdir().unwrap();
    let start = |named: &'static [&'static str], organizations: &'static [&'static str]| {
        start_relay_admitting(
            &directory,
            stub.app_with_key(),
            named,
            organizations,
            |config| config.with_admission_interval(RECHECK),
        )
    };
    let relay = start(&["octocat"], &["acme"]).await.unwrap();
    let (workstation, desktop) = (key(), key());
    assert_eq!(
        logging_in_as(&stub, &relay, &workstation, OCTOCAT, "octocat").await,
        done_as("octocat")
    );
    assert_eq!(
        logging_in_as(&stub, &relay, &desktop, MONA, "mona").await,
        done_as("mona")
    );
    relay.shutdown().await.unwrap();

    // Started again without the organization, the Relay lapses the Account
    // it alone admitted before it serves anyone, and not the one a name
    // admits as well.
    let relay = start(&["octocat"], &[]).await.unwrap();
    assert_eq!(
        Server::proven(&relay, &desktop).await.1,
        None,
        "the very first connection after the Relay starts is refused"
    );
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    relay.shutdown().await.unwrap();

    // Named again, the organization admits its members once more; the user
    // a name admits as well leaves it, and stands by the name.
    let relay = start(&["octocat"], &["acme"]).await.unwrap();
    assert_eq!(
        logging_in_as(&stub, &relay, &desktop, MONA, "mona").await,
        done_as("mona")
    );
    stub.membership("acme", OCTOCAT, None);
    asked_about_memberships(&stub, 10).await;
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    relay.shutdown().await.unwrap();

    // Without the name, the user stands while the organization admits them.
    stub.membership("acme", OCTOCAT, Some("active"));
    let relay = start(&[], &["acme"]).await.unwrap();
    assert_eq!(
        Server::proven(&relay, &workstation).await.1,
        Some(github_account("octocat"))
    );
    stub.membership("acme", OCTOCAT, None);
    until_lapsed(&relay, OCTOCAT, true).await;
    assert_eq!(
        Server::proven(&relay, &desktop).await.1,
        Some(github_account("mona"))
    );
    relay.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_apps_private_key_is_read_as_github_issues_it_or_as_pkcs8_and_a_file_holding_none_is_refused_naming_it()
 {
    let directory = tempfile::tempdir().unwrap();
    let file = |name: &str, contents: &str| {
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    };
    let pkcs1 = file("app.pem", APP_KEY);
    let pkcs8 = file("app-pkcs8.pem", APP_KEY_PKCS8);
    for path in [&pkcs1, &pkcs8] {
        let key = GitHubAppKey::from_pem_file(path).unwrap();
        let app = GitHubApp::new(CLIENT_ID).with_private_key(key.clone());
        for written in [format!("{key:?}"), format!("{app:?}")] {
            assert!(!written.contains(app_key_line()), "{written}");
        }
    }
    // A key read as PKCS#8 signs as GitHub takes it.
    let stub = acme().await;
    let app = stub
        .app()
        .with_private_key(GitHubAppKey::from_pem_file(&pkcs8).unwrap());
    start_relay_admitting(&directory, app, &[], &["acme"], |config| config)
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();

    // An ECDSA key, as a Server's identity key is, in PKCS#8.
    let ecdsa = {
        let der = base64::engine::general_purpose::STANDARD
            .encode(KeyPair::generate().unwrap().serialize_der());
        let lines = der
            .as_bytes()
            .chunks(64)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        format!("-----BEGIN PRIVATE KEY-----\n{lines}\n-----END PRIVATE KEY-----\n")
    };
    let truncated = APP_KEY.lines().take(8).collect::<Vec<_>>().join("\n");
    for (path, said) in [
        (
            directory.path().join("missing.pem"),
            "read the GitHub App's private key file",
        ),
        (file("empty.pem", ""), "no private key in PEM form"),
        (
            file("text.pem", "not a key at all"),
            "no private key in PEM form",
        ),
        (file("truncated.pem", &truncated), "not well-formed PEM"),
        (file("ecdsa.pem", &ecdsa), "no RSA private key"),
    ] {
        let refused = format!("{:#}", GitHubAppKey::from_pem_file(&path).unwrap_err());
        assert!(
            refused.contains(&format!("{path:?}")) && refused.contains(said),
            "{refused}"
        );
        for line in truncated.lines().chain(ecdsa.lines()).skip(1) {
            if !line.starts_with("-----") {
                assert!(!refused.contains(line), "{refused}");
            }
        }
    }
}

#[tokio::test]
async fn the_relay_binary_refuses_to_start_with_an_organization_it_cannot_check_saying_which_and_why()
 {
    let directory = tempfile::tempdir().unwrap();
    let key_file = directory.path().join("app.pem");
    std::fs::write(&key_file, APP_KEY).unwrap();
    // GitHub is reached through a proxy that is not there, as in
    // `the_relay_binary_refuses_to_start_naming_a_user_it_cannot_look_up_saying_who`.
    let closed = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let run = |arguments: &[&std::ffi::OsStr]| {
        let mut binary = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"));
        binary
            .arg("run")
            .arg("--listen-http")
            .arg("127.0.0.1:0")
            .arg("--database")
            .arg(directory.path().join("relay.db"))
            .args([
                "--public-address",
                PUBLIC_ADDRESS,
                "--recheck-minutes",
                "60",
            ])
            .args(arguments);
        for name in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            binary.env(name, &proxy);
        }
        binary
            .env("NO_PROXY", NO_HOST)
            .env("no_proxy", NO_HOST)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
    };
    let refused = |ran: std::process::Output| {
        assert!(!ran.status.success());
        assert!(ran.stdout.is_empty());
        String::from_utf8_lossy(&ran.stderr).into_owned()
    };
    let os = |text: &'static str| std::ffi::OsStr::new(text);

    let said = refused(
        timeout(
            DEADLINE,
            run(&[
                os("--github-client-id"),
                os(CLIENT_ID),
                os("--github-private-key-file"),
                key_file.as_os_str(),
                os("--admit-organization"),
                os("acme"),
            ]),
        )
        .await
        .expect("the binary stops in time")
        .unwrap(),
    );
    assert!(
        said.contains("`acme`") && said.contains("GitHub could not be reached"),
        "{said}"
    );
    assert!(!said.contains(app_key_line()), "{said}");

    let missing = directory.path().join("missing.pem");
    let said = refused(
        run(&[
            os("--github-client-id"),
            os(CLIENT_ID),
            os("--github-private-key-file"),
            missing.as_os_str(),
            os("--admit-organization"),
            os("acme"),
        ])
        .await
        .unwrap(),
    );
    assert!(
        said.contains(&format!("{missing:?}")) && said.contains("private key"),
        "{said}"
    );

    // Naming an organization without the app's private key to check it with
    // is refused before anything is tried.
    let said = refused(
        run(&[
            os("--github-client-id"),
            os(CLIENT_ID),
            os("--admit-organization"),
            os("acme"),
        ])
        .await
        .unwrap(),
    );
    assert!(said.contains("--github-private-key-file"), "{said}");
}
