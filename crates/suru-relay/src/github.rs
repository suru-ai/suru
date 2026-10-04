//! GitHub as a Relay's identity provider (ADR-0048). A user logs in through
//! the GitHub App the Relay's operator registers, by GitHub's device flow,
//! which needs no client secret; the Relay reads who logged in — their
//! numeric id, by which they are known, and their username, a label — and
//! keeps nothing of the token GitHub gives it to read that with. A username
//! the operator's admission rules name is looked up through GitHub's public
//! API, which needs no credential either.
//!
//! Every request is bounded in how long it may take and how much of the
//! answer is read, goes through the system's HTTP proxy, and trusts what the
//! operating system's trust store trusts, as a Server's requests to its
//! Relays do. Nothing GitHub hands the Relay to act with — a login's device
//! code, a user's token — is ever written anywhere, an error included.

use std::{fmt, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::{StatusCode, Url, header};
use serde::Deserialize;

use crate::identity::{DeviceLogin, Identity, IdentityProvider, LoginRefusal, LookUpFailed};

/// The provider's name, which keys every identity logged in through it.
const PROVIDER: &str = "github";

/// Where a user logs in at GitHub.
const WEB: &str = "https://github.com";

/// GitHub's REST API.
const API: &str = "https://api.github.com";

/// The version of GitHub's REST API the Relay speaks, which GitHub keeps
/// answering in the same shape however its API moves on.
const API_VERSION: &str = "2022-11-28";

/// What the Relay calls itself to GitHub, which refuses a request that does
/// not say.
const USER_AGENT: &str = concat!("suru-relay/", env!("CARGO_PKG_VERSION"));

/// How long each request to GitHub may take, answer and all, unless the
/// Relay's configuration says otherwise.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of any one answer of GitHub's the Relay reads: many times the
/// largest it gives to anything the Relay asks.
const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// How many seconds to leave between one asking after a login and the next
/// where GitHub does not say, as the device flow's own standard has it.
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// How many seconds longer to leave between askings each time GitHub says
/// the Relay asks too often.
const SLOW_DOWN_SECONDS: u64 = 5;

/// The grant a login's device code is exchanged for a token by.
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The longest a username can be at GitHub.
const MAX_USERNAME_CHARS: usize = 39;

/// The GitHub App a Relay's operator registers for it, with device login
/// enabled, which the Relay logs users in through: known by its client ID,
/// which is not a secret.
#[derive(Clone, Debug)]
pub struct GitHubApp {
    client_id: String,
    web: String,
    api: String,
    request_timeout: Duration,
    second: Duration,
}

impl GitHubApp {
    /// The GitHub App whose client ID is `client_id`.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            web: WEB.to_owned(),
            api: API.to_owned(),
            request_timeout: REQUEST_TIMEOUT,
            second: Duration::from_secs(1),
        }
    }

    /// Reaches GitHub where users log in at `web` and its REST API at
    /// `api`, rather than at GitHub's own addresses: a stub of GitHub, in a
    /// test.
    pub fn with_addresses(mut self, web: impl Into<String>, api: impl Into<String>) -> Self {
        self.web = web.into();
        self.api = api.into();
        self
    }

    /// Bounds how long each request to GitHub may take, answer and all.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Takes each second GitHub speaks of — between askings after a login,
    /// before a login expires — to last `second`, so a test can run GitHub's
    /// clock fast.
    pub fn with_second(mut self, second: Duration) -> Self {
        self.second = second;
        self
    }
}

/// GitHub, logging users in through the operator's GitHub App.
pub struct GitHub {
    client_id: String,
    web: Url,
    api: Url,
    http: reqwest::Client,
    request_timeout: Duration,
    second: Duration,
}

impl GitHub {
    /// GitHub, reached as `app` says, refusing an app it could not be
    /// reached for.
    pub fn new(app: GitHubApp) -> Result<Self> {
        let client_id = app.client_id.trim();
        if client_id.is_empty() || client_id.chars().any(char::is_control) {
            bail!("the GitHub App's client ID `{client_id}` is not one GitHub could know");
        }
        let web = base(&app.web).context("where users log in at GitHub")?;
        let api = base(&app.api).context("GitHub's API")?;
        Ok(Self {
            client_id: client_id.to_owned(),
            http: http_client(!(on_loopback(&web) && on_loopback(&api))),
            web,
            api,
            request_timeout: app.request_timeout,
            second: app.second,
        })
    }

    /// `seconds` of GitHub's.
    fn seconds(&self, seconds: u64) -> Duration {
        self.second
            .saturating_mul(u32::try_from(seconds).unwrap_or(u32::MAX))
    }

    /// A request to GitHub's REST API.
    fn api_request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    /// Sends `request` and reads its answer, within the request timeout and
    /// no more of it than [`MAX_ANSWER_BYTES`]: the answer, or why there was
    /// none, in words that say nothing of what was sent.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Answer, String> {
        let answering = async {
            let mut response = request
                .send()
                .await
                .map_err(|error| unreachable_reason(&error))?;
            let status = response.status();
            let headers = response.headers().clone();
            let too_much = || "GitHub answered with more than the Relay reads".to_owned();
            if response
                .content_length()
                .is_some_and(|length| length > MAX_ANSWER_BYTES as u64)
            {
                return Err(too_much());
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "GitHub's answer was cut off".to_owned())?
            {
                if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                    return Err(too_much());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(Answer {
                status,
                headers,
                body,
            })
        };
        tokio::time::timeout(self.request_timeout, answering)
            .await
            .unwrap_or_else(|_| Err("GitHub did not answer in time".to_owned()))
    }

    /// Who the token `token` is GitHub's for, read with it once, after which
    /// it is dropped.
    async fn user(&self, token: Token) -> Result<Identity, LoginRefusal> {
        let request = self
            .api_request(self.http.get(at(&self.api, &["user"])))
            .bearer_auth(&token.0);
        drop(token);
        let answer = self
            .send(request)
            .await
            .map_err(LoginRefusal::Unavailable)?;
        match answer.status {
            StatusCode::OK => answer.read::<User>().map(User::identity).ok_or_else(|| {
                LoginRefusal::Unavailable(
                    "GitHub said who logged in in a way the Relay does not understand".to_owned(),
                )
            }),
            status => Err(LoginRefusal::Unavailable(format!(
                "GitHub answered {status} when asked who logged in"
            ))),
        }
    }
}

#[async_trait]
impl IdentityProvider for GitHub {
    fn name(&self) -> &str {
        PROVIDER
    }

    async fn begin_login(&self) -> Result<DeviceLogin, LoginRefusal> {
        let request = self
            .http
            .post(at(&self.web, &["login", "device", "code"]))
            .header(header::ACCEPT, "application/json")
            .form(&[("client_id", self.client_id.as_str())]);
        let answer = self
            .send(request)
            .await
            .map_err(LoginRefusal::Unavailable)?;
        if let Some(Refused { error }) = answer.read::<Refused>() {
            return Err(LoginRefusal::Unavailable(refused_because(&error)));
        }
        if !answer.status.is_success() {
            return Err(LoginRefusal::Unavailable(format!(
                "GitHub answered {} when asked to begin a login",
                answer.status
            )));
        }
        let begun = answer.read::<Begun>().ok_or_else(|| {
            LoginRefusal::Unavailable(
                "GitHub began a login in a way the Relay does not understand".to_owned(),
            )
        })?;
        Ok(DeviceLogin {
            verification_uri: begun.verification_uri,
            user_code: begun.user_code,
            device_code: begun.device_code,
            expires_in: self.seconds(begun.expires_in),
            interval: self.seconds(begun.interval.unwrap_or(DEFAULT_INTERVAL_SECONDS).max(1)),
        })
    }

    async fn finish_login(&self, login: &DeviceLogin) -> Result<Identity, LoginRefusal> {
        let mut interval = login.interval;
        loop {
            tokio::time::sleep(interval).await;
            let request = self
                .http
                .post(at(&self.web, &["login", "oauth", "access_token"]))
                .header(header::ACCEPT, "application/json")
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("device_code", login.device_code.as_str()),
                    ("grant_type", DEVICE_CODE_GRANT),
                ]);
            let answer = self
                .send(request)
                .await
                .map_err(LoginRefusal::Unavailable)?;
            let Some(polled) = answer.read::<Polled>() else {
                return Err(LoginRefusal::Unavailable(format!(
                    "GitHub answered {} when asked how a login went",
                    answer.status
                )));
            };
            drop(answer);
            if let Some(token) = polled.access_token {
                return self.user(token).await;
            }
            match polled.error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    interval = (interval + self.seconds(SLOW_DOWN_SECONDS)).max(
                        polled
                            .interval
                            .map_or(Duration::ZERO, |asked| self.seconds(asked)),
                    );
                }
                Some("expired_token") => return Err(LoginRefusal::Expired),
                Some("access_denied") => return Err(LoginRefusal::Denied),
                Some(error) => return Err(LoginRefusal::Unavailable(refused_because(error))),
                None => {
                    return Err(LoginRefusal::Unavailable(
                        "GitHub said how a login went in a way the Relay does not understand"
                            .to_owned(),
                    ));
                }
            }
        }
    }

    async fn look_up(&self, name: &str) -> Result<Option<Identity>, LookUpFailed> {
        if !could_be_username(name) {
            return Ok(None);
        }
        let request = self.api_request(self.http.get(at(&self.api, &["users", name])));
        let answer = self.send(request).await.map_err(LookUpFailed)?;
        match answer.status {
            StatusCode::OK => {
                let user = answer.read::<User>().ok_or_else(|| {
                    LookUpFailed(
                        "GitHub said who it is in a way the Relay does not understand".to_owned(),
                    )
                })?;
                match user.kind.as_deref() {
                    None | Some("User") => Ok(Some(user.identity())),
                    Some("Organization") => Err(LookUpFailed(
                        "it is the name of a GitHub organization, not of a user".to_owned(),
                    )),
                    Some(_) => Err(LookUpFailed(
                        "it is the name of a GitHub account no one logs in as".to_owned(),
                    )),
                }
            }
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(LookUpFailed(answer.limited_until().map_or_else(
                || format!("GitHub answered {status} when asked who goes by it"),
                |when| {
                    format!(
                        "GitHub is limiting how often this machine may ask it who goes by a \
                         name; it may be asked again {when}"
                    )
                },
            ))),
        }
    }
}

/// GitHub's answer to a request, read whole.
struct Answer {
    status: StatusCode,
    headers: header::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    /// The answer, read as `T`, where it is one.
    fn read<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_slice(&self.body).ok()
    }

    /// When GitHub may be asked again, where it refused the request for a
    /// limit on how often it is asked — its hourly limit, or one on asking
    /// too much at once, which it answers with a 403 or a 429, or by saying
    /// how long to wait: as it says, or `later` where it does not.
    fn limited_until(&self) -> Option<String> {
        let header = |name| self.headers.get(name).and_then(|value| value.to_str().ok());
        let retry_after = header("retry-after");
        if retry_after.is_none()
            && !matches!(
                self.status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return None;
        }
        if let Some(seconds) = retry_after.and_then(|after| after.trim().parse::<u64>().ok()) {
            return Some(format!("in {seconds} seconds"));
        }
        let reset = header("x-ratelimit-reset")
            .and_then(|reset| reset.trim().parse::<i64>().ok())
            .and_then(|reset| time::OffsetDateTime::from_unix_timestamp(reset).ok())
            .and_then(|reset| {
                reset
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()
            });
        Some(reset.map_or_else(|| "later".to_owned(), |at| format!("at {at}")))
    }
}

/// A login GitHub began.
#[derive(Deserialize)]
struct Begun {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

/// What GitHub says of a login asked after: a token where it is done, or the
/// error saying how it stands.
#[derive(Deserialize)]
struct Polled {
    access_token: Option<Token>,
    error: Option<String>,
    /// How long GitHub asks to be left between askings from now on, where it
    /// says the Relay asks too often.
    interval: Option<u64>,
}

/// A request GitHub refused, saying why.
#[derive(Deserialize)]
struct Refused {
    error: String,
}

/// A user, as GitHub tells of them.
#[derive(Deserialize)]
struct User {
    id: u64,
    login: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl User {
    fn identity(self) -> Identity {
        Identity {
            subject: self.id.to_string(),
            username: self.login,
        }
    }
}

/// The token GitHub gives a login to read who logged in with: read with once
/// and dropped, and never written anywhere.
#[derive(Deserialize)]
#[serde(transparent)]
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(..)")
    }
}

/// Why GitHub refused a login, by the error it named, in words for the
/// Server's user and the Relay's operator alike.
fn refused_because(error: &str) -> String {
    match error {
        "device_flow_disabled" => "device login is not enabled for this Relay's GitHub App; its \
                                   operator must enable it in the app's settings"
            .to_owned(),
        "incorrect_client_credentials" => "GitHub knows no GitHub App by the client ID this \
                                           Relay was given; its operator must check it"
            .to_owned(),
        error
            if !error.is_empty()
                && error.len() <= 64
                && error
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_') =>
        {
            format!("GitHub refused the login ({error})")
        }
        _ => "GitHub refused the login".to_owned(),
    }
}

/// Whether `name` could be a GitHub username: letters, digits and hyphens,
/// as many as GitHub allows, so it is looked up as a name alone.
fn could_be_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_USERNAME_CHARS
        && !name.starts_with('-')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

/// `address` as the base of the addresses beneath it.
fn base(address: &str) -> Result<Url> {
    let url = Url::parse(address).with_context(|| format!("`{address}` is not an address"))?;
    if !matches!(url.scheme(), "https" | "http") || url.cannot_be_a_base() {
        bail!("`{address}` is not an https:// or http:// address");
    }
    Ok(url)
}

/// The address `segments` beneath `base`.
fn at(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("a base address has a path")
        .pop_if_empty()
        .extend(segments);
    url
}

/// Whether `url` is on this machine's loopback, which no proxy elsewhere
/// could reach.
fn on_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The HTTP client the Relay reaches GitHub with: through the system's HTTP
/// proxy, as reqwest finds it, where `proxied`, and trusting what the
/// operating system's trust store trusts. A trust store that cannot be read
/// trusts nothing, so GitHub is refused rather than the Relay failing.
fn http_client(proxied: bool) -> reqwest::Client {
    use rustls_platform_verifier::BuilderVerifierExt as _;

    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring speaks every TLS version rustls deems safe");
    let mut tls = match builder.clone().with_platform_verifier() {
        Ok(verified) => verified.with_no_client_auth(),
        Err(error) => {
            tracing::warn!("GitHub is not trusted: the trust store is unreadable: {error}");
            builder
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth()
        }
    };
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .user_agent(USER_AGENT);
    if proxied { client } else { client.no_proxy() }
        .build()
        .expect("an HTTP client over rustls needs nothing that can fail")
}

/// Says why GitHub could not be reached, telling a certificate this machine
/// does not trust apart from GitHub not answering at all.
fn unreachable_reason(error: &reqwest::Error) -> String {
    if untrusted_certificate(error) {
        "GitHub's certificate is not one this machine's trust store trusts".to_owned()
    } else {
        "GitHub could not be reached".to_owned()
    }
}

/// Whether `error` came of a certificate the TLS handshake refused. The TLS
/// error lies wrapped in I/O errors, whose own `source` passes over what they
/// wrap, so each one is looked inside as well as past.
fn untrusted_certificate(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(rustls::Error::InvalidCertificate(_)) = error.downcast_ref::<rustls::Error>() {
        return true;
    }
    let wrapped = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref);
    if let Some(wrapped) = wrapped
        && untrusted_certificate(wrapped)
    {
        return true;
    }
    error.source().is_some_and(untrusted_certificate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_beneath_a_base_keep_its_path_and_name_nothing_but_a_username() {
        let stub = base("http://127.0.0.1:8080/github").unwrap();
        assert_eq!(
            at(&stub, &["login", "device", "code"]).as_str(),
            "http://127.0.0.1:8080/github/login/device/code"
        );
        assert_eq!(
            at(&base(API).unwrap(), &["users", "octocat"]).as_str(),
            "https://api.github.com/users/octocat"
        );
        for name in ["octocat", "Octo-Cat", "a", &"x".repeat(MAX_USERNAME_CHARS)] {
            assert!(could_be_username(name), "{name}");
        }
        for name in [
            "",
            "-octo",
            "octo/../user",
            "octo cat",
            "octo?",
            &"x".repeat(MAX_USERNAME_CHARS + 1),
        ] {
            assert!(!could_be_username(name), "{name}");
        }
        assert!(base("ftp://github.com").is_err());
        assert!(on_loopback(&stub) && !on_loopback(&base(WEB).unwrap()));
    }

    #[test]
    fn a_refusal_github_names_is_described_without_repeating_anything_unexpected() {
        assert!(refused_because("device_flow_disabled").contains("device login is not enabled"));
        assert_eq!(
            refused_because("unsupported_grant_type"),
            "GitHub refused the login (unsupported_grant_type)"
        );
        assert_eq!(
            refused_because("<script>secret</script>"),
            "GitHub refused the login"
        );
    }

    #[test]
    fn neither_a_token_nor_a_device_code_is_ever_written_by_debug() {
        let token: Token = serde_json::from_str(r#""ghu_secret""#).unwrap();
        assert!(!format!("{token:?}").contains("ghu_secret"));
        let login = DeviceLogin {
            verification_uri: "https://github.com/login/device".to_owned(),
            user_code: "WDJB-MJHT".to_owned(),
            device_code: "3584d83530557fdd1f46af8289938c8ef79f9dc5".to_owned(),
            expires_in: Duration::from_secs(900),
            interval: Duration::from_secs(5),
        };
        let written = format!("{login:?}");
        assert!(written.contains("WDJB-MJHT"), "{written}");
        assert!(!written.contains("3584d835"), "{written}");
    }
}
