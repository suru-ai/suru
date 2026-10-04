//! GitHub as a Relay's identity provider (ADR-0048). A user logs in through
//! the GitHub App the Relay's operator registers, by GitHub's device flow,
//! which needs no client secret; the Relay reads who logged in — their
//! numeric id, by which they are known, and their username, a label — and
//! keeps nothing of the token GitHub gives it to read that with. A username
//! the operator's admission rules name is looked up through GitHub's public
//! API, which needs no credential either.
//!
//! The members of an organization the rules name are checked through the
//! app's installation on that organization, which an owner of it installs,
//! and which sees its private members as well as its public ones. The Relay
//! speaks as the app itself, by a short-lived token its private key signs, to
//! find the installation and to be given a token of the installation's that
//! may read who the organization's members are and nothing else, which it
//! keeps in memory until shortly before it expires; and asks with that
//! whether a user is a member, by their numeric id — never taking an answer
//! about a name to be about them unless it names their id, nor a name to be
//! one nobody they are goes by until it is read afresh by their id. An
//! organization is known by its numeric id too, so an answer about another
//! organization that has come to go by its name is never taken for its own.
//! An invitation to the organization not yet accepted makes nobody a member.
//! No token of a user's is needed for any of it, or kept.
//!
//! Every request is bounded in how long it may take and how much of the
//! answer is read, goes through the system's HTTP proxy, and trusts what the
//! operating system's trust store trusts, as a Server's requests to its
//! Relays do. Where GitHub says it limits how often it is asked, it is not
//! asked through that installation again until it says the limit lifts.
//! Nothing GitHub hands the Relay to act with — a login's device code, a
//! user's token, the app's own tokens and its installations' — is ever
//! written anywhere, an error included, and neither is the app's private key.

use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, Url, header};
use ring::{rand::SystemRandom, signature::RsaKeyPair};
use serde::Deserialize;

use crate::{
    admission::Undecided,
    identity::{
        DeviceLogin, Identity, IdentityProvider, LoginRefusal, LookUpFailed, Organization,
        OrganizationUnchecked,
    },
};

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

/// The longest a username, or an organization's name, can be at GitHub.
const MAX_NAME_CHARS: usize = 39;

/// How long before it is made each token the app signs for itself says it
/// was, so a clock of GitHub's a little behind the Relay's takes it.
const APP_TOKEN_BACKDATED: Duration = Duration::from_secs(60);

/// How long after it is made each token the app signs for itself expires:
/// short of the ten minutes GitHub allows, counted from when it says it was
/// made.
const APP_TOKEN_LIFETIME: Duration = Duration::from_secs(9 * 60);

/// How long before an installation's token expires the Relay gets a new one.
const INSTALLATION_TOKEN_RENEWED_BEFORE: Duration = Duration::from_secs(5 * 60);

/// How many seconds the Relay leaves GitHub unasked when it says it limits
/// how often it is asked without saying for how long, as GitHub asks.
const UNSAID_LIMIT_SECONDS: u64 = 60;

/// Why GitHub refuses the app's own tokens, in words for the Relay's
/// operator.
const CREDENTIALS_REFUSED: &str = "GitHub refused the GitHub App's credentials: the client ID \
                                   and the private key the Relay was given must be the same \
                                   app's, and this machine's clock right";

/// Why an installation cannot be asked who its organization's members are,
/// in words for the Relay's operator.
const MEMBERS_NOT_GRANTED: &str = "the app's installation on it may not read who its members \
                                   are: the app must ask for the Members organization \
                                   permission, read-only, and an owner of the organization must \
                                   accept the app asking for it";

/// Why a Relay without its app's private key cannot check an organization's
/// members.
const NO_PRIVATE_KEY: &str = "the Relay was given no private key for its GitHub App, which it \
                              checks an organization's members with";

/// The GitHub App a Relay's operator registers for it, with device login
/// enabled, which the Relay logs users in through: known by its client ID,
/// which is not a secret. Given the app's private key, the Relay checks the
/// members of organizations the app is installed on through it too.
#[derive(Clone, Debug)]
pub struct GitHubApp {
    client_id: String,
    key: Option<GitHubAppKey>,
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
            key: None,
            web: WEB.to_owned(),
            api: API.to_owned(),
            request_timeout: REQUEST_TIMEOUT,
            second: Duration::from_secs(1),
        }
    }

    /// Has the Relay speak as the app itself, as it checks the members of an
    /// organization the app is installed on, by tokens `key` signs: the
    /// app's private key, as GitHub issued it.
    pub fn with_private_key(mut self, key: GitHubAppKey) -> Self {
        self.key = Some(key);
        self
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

/// The private key of the GitHub App a Relay's operator registers, as GitHub
/// issued it, which the Relay signs the app's own tokens with: never written
/// anywhere, by `Debug` least of all.
#[derive(Clone)]
pub struct GitHubAppKey(Arc<RsaKeyPair>);

impl GitHubAppKey {
    /// The key in the PEM file at `path`, read once: PKCS#1, as GitHub
    /// issues it, or PKCS#8. Fails, saying which file and why — never what it
    /// holds — where it cannot be read, or holds no RSA private key the Relay
    /// can sign with.
    pub fn from_pem_file(path: &Path) -> Result<Self> {
        let pem = std::fs::read(path)
            .with_context(|| format!("read the GitHub App's private key file {path:?}"))?;
        Self::from_pem(&pem)
            .with_context(|| format!("the GitHub App's private key file {path:?} cannot be used"))
    }

    /// The key PEM encodes in `pem`: PKCS#1, as GitHub issues it, or PKCS#8.
    /// Fails, saying why — never what it holds — where `pem` holds no RSA
    /// private key the Relay can sign with.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        use rustls::pki_types::{PrivateKeyDer, pem::PemObject as _};

        let key = PrivateKeyDer::from_pem_slice(pem).map_err(|error| match error {
            rustls::pki_types::pem::Error::NoItemsFound => {
                anyhow!("it holds no private key in PEM form, as GitHub issues them")
            }
            _ => anyhow!("it is not well-formed PEM, as GitHub issues private keys in"),
        })?;
        let pair = match &key {
            PrivateKeyDer::Pkcs1(key) => RsaKeyPair::from_der(key.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(key) => RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()),
            _ => bail!("it holds a private key of another kind than the RSA keys GitHub issues"),
        }
        .map_err(|rejected| {
            anyhow!(
                "it holds no RSA private key the Relay can sign with, as GitHub issues them \
                 ({rejected})"
            )
        })?;
        Ok(Self(Arc::new(pair)))
    }

    /// `message` signed by RSASSA-PKCS1-v1_5 over SHA-256, as GitHub checks
    /// the app's own tokens.
    fn sign(&self, message: &[u8]) -> Option<Vec<u8>> {
        let mut signature = vec![0; self.0.public().modulus_len()];
        self.0
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                message,
                &mut signature,
            )
            .ok()?;
        Some(signature)
    }
}

impl fmt::Debug for GitHubAppKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubAppKey(..)")
    }
}

/// GitHub, logging users in through the operator's GitHub App, and checking
/// the members of organizations the app is installed on.
pub struct GitHub {
    client_id: String,
    key: Option<GitHubAppKey>,
    web: Url,
    api: Url,
    http: reqwest::Client,
    request_timeout: Duration,
    second: Duration,
    /// The app's installation on each organization the Relay has checked
    /// the members of, by the organization's numeric id.
    installations: Mutex<HashMap<u64, Arc<Installed>>>,
}

/// The app's installation on one organization, as the Relay deals with it.
struct Installed {
    /// The organization's numeric id, by which it is known.
    organization: u64,
    /// The name the admission rules name it by.
    name: String,
    /// What the Relay holds of the installation, held while it is renewed,
    /// so it is renewed once however many ask at once.
    held: tokio::sync::Mutex<Holding>,
}

/// What the Relay holds of an app's installation on an organization.
#[derive(Default)]
struct Holding {
    /// The installation, once found.
    installation: Option<Installation>,
    /// A token of the installation's, and when to get a new one.
    token: Option<(Token, Instant)>,
    /// Until when the Relay asks nothing through the installation — GitHub
    /// limiting how often it is asked, or having said the installation
    /// cannot be asked — and why.
    held_off: Option<(Instant, String)>,
}

/// An app's installation on an organization, as GitHub last told of it.
#[derive(Clone)]
struct Installation {
    id: u64,
    /// The name the organization went by as GitHub told of it.
    login: String,
}

/// Why GitHub could not be asked something about an organization's
/// installation.
enum Unasked {
    /// It could not be asked just now, saying why, and — where it limits
    /// how often it is asked — how long until it may be again.
    Unavailable(String, Option<Duration>),
    /// It answered, and what was asked cannot be had, saying why.
    Refused(String),
}

impl From<Unasked> for OrganizationUnchecked {
    fn from(unasked: Unasked) -> Self {
        match unasked {
            Unasked::Unavailable(why, _) => Self::Unavailable(why),
            Unasked::Refused(why) => Self::Refused(why),
        }
    }
}

impl Unasked {
    fn unavailable(why: String) -> Self {
        Self::Unavailable(why, None)
    }

    fn into_why(self) -> String {
        match self {
            Self::Unavailable(why, _) | Self::Refused(why) => why,
        }
    }
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
            key: app.key,
            http: http_client(!(on_loopback(&web) && on_loopback(&api))),
            web,
            api,
            request_timeout: app.request_timeout,
            second: app.second,
            installations: Mutex::default(),
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

impl GitHub {
    /// A token the app signs for itself, which GitHub takes as the app's
    /// word for a few minutes: named by the app's client ID, which GitHub
    /// prefers to its app ID.
    fn app_token(&self) -> Result<Token, Unasked> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| Unasked::Refused(NO_PRIVATE_KEY.to_owned()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let claims = serde_json::json!({
            "iat": now.saturating_sub(APP_TOKEN_BACKDATED).as_secs(),
            "exp": (now + APP_TOKEN_LIFETIME).as_secs(),
            "iss": self.client_id,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = key.sign(signed.as_bytes()).ok_or_else(|| {
            Unasked::unavailable("the GitHub App's private key could not sign".to_owned())
        })?;
        Ok(Token(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }

    /// Sends `request` to GitHub's REST API as the app itself.
    async fn as_app(&self, request: reqwest::RequestBuilder) -> Result<Answer, Unasked> {
        let token = self.app_token()?;
        self.send(self.api_request(request).bearer_auth(&token.0))
            .await
            .map_err(Unasked::unavailable)
    }

    /// Why GitHub refused what the app asked it `about`: for a limit on how
    /// often it is asked, which says when it may be asked again; or as its
    /// status says.
    fn refused_the_app(&self, answer: &Answer, about: &str) -> Unasked {
        if let Some(limit) = answer.limit(self.second) {
            return Unasked::Unavailable(
                format!(
                    "GitHub is limiting how often this Relay may ask it {about}; it may be asked \
                     again {}",
                    limit.when
                ),
                Some(limit.lifts_in),
            );
        }
        match answer.status {
            StatusCode::UNAUTHORIZED => Unasked::Refused(CREDENTIALS_REFUSED.to_owned()),
            status if status.is_server_error() => {
                Unasked::unavailable(format!("GitHub answered {status} when asked {about}"))
            }
            status => Unasked::Refused(format!("GitHub answered {status} when asked {about}")),
        }
    }

    /// The app's installation on the organization that goes by `name` now,
    /// and the organization's numeric id, where the installation may be
    /// asked who the organization's members are.
    async fn installation_named(&self, name: &str) -> Result<(Installation, u64), Unasked> {
        if !could_be_name(name) {
            return Err(Unasked::Refused(
                "GitHub's names are letters, digits and hyphens, as this is not".to_owned(),
            ));
        }
        let answer = self
            .as_app(
                self.http
                    .get(at(&self.api, &["orgs", name, "installation"])),
            )
            .await?;
        match answer.status {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => {
                return Err(Unasked::Refused(
                    "this Relay's GitHub App is not installed on it, or GitHub knows no \
                     organization by that name; an owner of the organization must install the \
                     app on it"
                        .to_owned(),
                ));
            }
            _ => return Err(self.refused_the_app(&answer, "about its app's installations")),
        }
        let installed = answer.read::<AppInstallation>().ok_or_else(|| {
            Unasked::unavailable(
                "GitHub told of the app's installation on it in a way the Relay does not \
                 understand"
                    .to_owned(),
            )
        })?;
        if installed.account.kind.as_deref() != Some("Organization") {
            return Err(Unasked::Refused(
                "it is the name of a GitHub user, not of an organization".to_owned(),
            ));
        }
        if installed.suspended_at.is_some() {
            return Err(Unasked::Refused(
                "the app's installation on it is suspended; an owner of the organization must \
                 unsuspend it"
                    .to_owned(),
            ));
        }
        if !matches!(
            installed.permissions.get("members").map(String::as_str),
            Some("read" | "write")
        ) {
            return Err(Unasked::Refused(MEMBERS_NOT_GRANTED.to_owned()));
        }
        Ok((
            Installation {
                id: installed.id,
                login: installed.account.login,
            },
            installed.account.id,
        ))
    }

    /// A token of the installation `installation`'s that may read who its
    /// organization's members are, and nothing else, and when to get a new
    /// one: shortly before it expires.
    async fn issue_token(&self, installation: u64) -> Result<(Token, Instant), Unasked> {
        let request = self
            .http
            .post(at(
                &self.api,
                &[
                    "app",
                    "installations",
                    &installation.to_string(),
                    "access_tokens",
                ],
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"permissions":{"members":"read"}}"#);
        let answer = self.as_app(request).await?;
        match answer.status {
            StatusCode::CREATED | StatusCode::OK => {}
            StatusCode::NOT_FOUND => {
                return Err(Unasked::Refused(
                    "the app is no longer installed on it; an owner of the organization must \
                     install the app on it"
                        .to_owned(),
                ));
            }
            StatusCode::UNPROCESSABLE_ENTITY => {
                return Err(Unasked::Refused(MEMBERS_NOT_GRANTED.to_owned()));
            }
            _ => {
                return Err(self.refused_the_app(&answer, "for a token of its app's installation"));
            }
        }
        let issued = answer.read::<IssuedToken>().and_then(|issued| {
            let expires_at = time::OffsetDateTime::parse(
                &issued.expires_at,
                &time::format_description::well_known::Rfc3339,
            )
            .ok()?;
            Some((issued.token, expires_at))
        });
        let Some((token, expires_at)) = issued else {
            return Err(Unasked::unavailable(
                "GitHub gave a token of the app's installation in a way the Relay does not \
                 understand"
                    .to_owned(),
            ));
        };
        let lasts = (expires_at - time::OffsetDateTime::now_utc())
            .try_into()
            .unwrap_or(Duration::ZERO);
        Ok((
            token,
            Instant::now() + lasts.saturating_sub(INSTALLATION_TOKEN_RENEWED_BEFORE),
        ))
    }

    /// The app's installation on `organization`, as the Relay deals with it.
    fn installed(&self, organization: u64, name: &str) -> Arc<Installed> {
        self.installations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(organization)
            .or_insert_with(|| {
                Arc::new(Installed {
                    organization,
                    name: name.to_owned(),
                    held: tokio::sync::Mutex::default(),
                })
            })
            .clone()
    }

    /// A token of the app's installation on `installed`'s organization, and
    /// the name the organization went by as GitHub last told of it: the one
    /// held, unless it is near its end, or `renewed` is asked for; a new
    /// one otherwise, found by the organization's name where the
    /// installation is not known, and taken only where that name still
    /// names the organization.
    async fn installation_token(
        &self,
        installed: &Installed,
        renewed: bool,
    ) -> Result<(Token, String), String> {
        let mut holding = installed.held.lock().await;
        if let Some((until, why)) = &holding.held_off {
            if Instant::now() < *until {
                return Err(why.clone());
            }
            holding.held_off = None;
        }
        let got = async {
            let installation = match &holding.installation {
                Some(installation) => installation.clone(),
                None => {
                    let (installation, organization) =
                        self.installation_named(&installed.name).await?;
                    if organization != installed.organization {
                        return Err(Unasked::Refused(
                            "that name no longer names the organization the Relay first named by \
                             it, which may have taken another name, and another organization \
                             this one"
                                .to_owned(),
                        ));
                    }
                    holding.installation = Some(installation.clone());
                    installation
                }
            };
            let token = match &holding.token {
                Some((token, renew_at)) if !renewed && Instant::now() < *renew_at => token.clone(),
                _ => {
                    let issued = self.issue_token(installation.id).await;
                    if let Err(Unasked::Refused(_)) = &issued {
                        // Found again by name, it may be installed anew.
                        holding.installation = None;
                    }
                    let (token, renew_at) = issued?;
                    holding.token = Some((token.clone(), renew_at));
                    token
                }
            };
            Ok((token, installation.login))
        }
        .await;
        got.map_err(|unasked| self.hold_off(&mut holding, unasked))
    }

    /// Says why what was asked through an installation, as `holding` holds
    /// it, went unanswered — `unasked` — holding off asking anything through
    /// it until GitHub's limit lifts, where it limits how often it is asked,
    /// and for a minute of GitHub's, where it says the installation cannot
    /// be asked, rather than ask it again for every Account checked.
    fn hold_off(&self, holding: &mut Holding, unasked: Unasked) -> String {
        let lifts_in = match &unasked {
            Unasked::Unavailable(_, lifts_in) => *lifts_in,
            Unasked::Refused(_) => Some(self.seconds(UNSAID_LIMIT_SECONDS)),
        };
        let why = unasked.into_why();
        if let Some(lifts_in) = lifts_in {
            holding.held_off = Some((Instant::now() + lifts_in, why.clone()));
        }
        why
    }

    /// Sends what `request` makes, for the name the organization goes by, to
    /// GitHub's REST API with a token of the app's installation on
    /// `installed`'s organization, renewing the token once where GitHub no
    /// longer takes it.
    async fn as_installation(
        &self,
        installed: &Installed,
        request: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> Result<Answer, String> {
        let mut renewed = false;
        loop {
            let (token, login) = self.installation_token(installed, renewed).await?;
            let answer = self
                .send(self.api_request(request(&login)).bearer_auth(&token.0))
                .await?;
            if answer.status == StatusCode::UNAUTHORIZED && !renewed {
                renewed = true;
                continue;
            }
            if let Some(limit) = answer.limit(self.second) {
                let unasked = Unasked::Unavailable(
                    format!(
                        "GitHub is limiting how often this Relay may ask it about an \
                         organization's members; it may be asked again {}",
                        limit.when
                    ),
                    Some(limit.lifts_in),
                );
                return Err(self.hold_off(&mut *installed.held.lock().await, unasked));
            }
            return Ok(answer);
        }
    }

    /// Reads afresh the name `installed`'s organization goes by, through the
    /// app's installation on it, answering whether it has changed.
    async fn organization_renamed(&self, installed: &Installed) -> Result<bool, String> {
        let mut holding = installed.held.lock().await;
        let Some(installation) = holding.installation.clone() else {
            // Not yet found, it is found by name as the next token is got.
            return Ok(true);
        };
        let read = async {
            let answer = self
                .as_app(self.http.get(at(
                    &self.api,
                    &["app", "installations", &installation.id.to_string()],
                )))
                .await?;
            match answer.status {
                StatusCode::OK => {}
                StatusCode::NOT_FOUND => {
                    return Err(Unasked::Refused(
                        "the app is no longer installed on it; an owner of the organization \
                         must install the app on it"
                            .to_owned(),
                    ));
                }
                _ => return Err(self.refused_the_app(&answer, "about its app's installations")),
            }
            let read = answer.read::<AppInstallation>().ok_or_else(|| {
                Unasked::unavailable(
                    "GitHub told of the app's installation on it in a way the Relay does not \
                     understand"
                        .to_owned(),
                )
            })?;
            if read.account.id != installed.organization {
                return Err(Unasked::unavailable(
                    "GitHub told of the app's installation on another organization".to_owned(),
                ));
            }
            Ok(read.account.login)
        }
        .await;
        match read {
            Ok(login) => {
                let renamed = !login.eq_ignore_ascii_case(&installation.login);
                holding.installation = Some(Installation {
                    login,
                    ..installation
                });
                Ok(renamed)
            }
            Err(unasked) => {
                if let Unasked::Refused(_) = unasked {
                    holding.installation = None;
                    holding.token = None;
                }
                Err(self.hold_off(&mut holding, unasked))
            }
        }
    }

    /// The name the user whose numeric id is `id` goes by now, read through
    /// the app's installation on `installed`'s organization: `None` where
    /// GitHub knows nobody by that id any longer.
    async fn login_of(&self, installed: &Installed, id: u64) -> Result<Option<String>, String> {
        let answer = self
            .as_installation(installed, |_| {
                self.http.get(at(&self.api, &["user", &id.to_string()]))
            })
            .await?;
        match answer.status {
            StatusCode::OK => match answer.read::<User>() {
                Some(user) if user.id == id => Ok(Some(user.login)),
                _ => Err(
                    "GitHub said who a user is in a way the Relay does not understand".to_owned(),
                ),
            },
            StatusCode::NOT_FOUND => Ok(None),
            status => Err(format!("GitHub answered {status} when asked who a user is")),
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
        if !could_be_name(name) {
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
            status => Err(LookUpFailed(answer.limit(self.second).map_or_else(
                || format!("GitHub answered {status} when asked who goes by it"),
                |limit| {
                    format!(
                        "GitHub is limiting how often this machine may ask it who goes by a \
                         name; it may be asked again {}",
                        limit.when
                    )
                },
            ))),
        }
    }

    async fn look_up_organization(&self, name: &str) -> Result<String, OrganizationUnchecked> {
        let (installation, organization) = self.installation_named(name).await?;
        // A token is got at once, so the app's private key and the
        // installation's permission are seen to work before the Relay starts.
        let token = self.issue_token(installation.id).await?;
        let installed = self.installed(organization, name);
        *installed.held.lock().await = Holding {
            installation: Some(installation),
            token: Some(token),
            held_off: None,
        };
        Ok(organization.to_string())
    }

    async fn is_member(
        &self,
        organization: &Organization,
        identity: &Identity,
    ) -> Result<bool, Undecided> {
        let undecided = |why: String| {
            Undecided(format!(
                "whether {} is a member of the organization `{}` cannot be told just now: {why}",
                identity.username, organization.name
            ))
        };
        let (Ok(organization_id), Ok(id)) = (
            organization.id.parse::<u64>(),
            identity.subject.parse::<u64>(),
        ) else {
            return Ok(false);
        };
        let installed = self.installed(organization_id, &organization.name);
        // Asked first by the name they went by as they last logged in, which
        // is taken to be theirs only where the answer names their id; and
        // where it does not, or the answer is that nobody by it is a member,
        // asked again by the names they and the organization go by now, read
        // afresh by their ids, unless neither has changed.
        let mut name = identity.username.clone();
        for read_afresh in [false, true] {
            let mut someone_else = false;
            if could_be_name(&name) {
                let answer = self
                    .as_installation(&installed, |organization| {
                        self.http
                            .get(at(&self.api, &["orgs", organization, "memberships", &name]))
                    })
                    .await
                    .map_err(undecided)?;
                match answer.status {
                    StatusCode::OK => {
                        let membership = answer.read::<Membership>().ok_or_else(|| {
                            undecided(
                                "GitHub told of a membership in a way the Relay does not \
                                 understand"
                                    .to_owned(),
                            )
                        })?;
                        if membership.organization.id != organization_id {
                            return Err(undecided(
                                "GitHub told of a membership of another organization".to_owned(),
                            ));
                        }
                        if membership.user.id == id {
                            // An invitation not yet accepted is pending.
                            return Ok(membership.state == "active");
                        }
                        someone_else = true;
                    }
                    StatusCode::NOT_FOUND => {}
                    status => {
                        return Err(undecided(format!(
                            "GitHub answered {status} when asked about a membership"
                        )));
                    }
                }
            }
            if read_afresh {
                if someone_else {
                    break;
                }
                return Ok(false);
            }
            let Some(now_named) = self.login_of(&installed, id).await.map_err(undecided)? else {
                // A user GitHub no longer knows is a member of nothing.
                return Ok(false);
            };
            let renamed = self
                .organization_renamed(&installed)
                .await
                .map_err(undecided)?;
            if !someone_else && !renamed && now_named.eq_ignore_ascii_case(&name) {
                return Ok(false);
            }
            name = now_named;
        }
        Err(undecided(
            "their name, or the organization's, changed as they were asked about".to_owned(),
        ))
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
    /// too much at once, which it answers with a 429, or a 403 saying so, or
    /// by saying how long to wait: as it says, each of its seconds lasting
    /// `second`, or a minute of them where it does not.
    fn limit(&self, second: Duration) -> Option<Limit> {
        let header = |name| self.headers.get(name).and_then(|value| value.to_str().ok());
        let retry_after = header("retry-after");
        let limited = retry_after.is_some()
            || self.status == StatusCode::TOO_MANY_REQUESTS
            || (self.status == StatusCode::FORBIDDEN
                && (header("x-ratelimit-remaining").is_some_and(|left| left.trim() == "0")
                    || String::from_utf8_lossy(&self.body)
                        .to_lowercase()
                        .contains("rate limit")));
        if !limited {
            return None;
        }
        let seconds =
            |seconds: u64| second.saturating_mul(u32::try_from(seconds).unwrap_or(u32::MAX));
        if let Some(after) = retry_after.and_then(|after| after.trim().parse::<u64>().ok()) {
            return Some(Limit {
                lifts_in: seconds(after),
                when: format!("in {after} seconds"),
            });
        }
        let reset = header("x-ratelimit-reset")
            .and_then(|reset| reset.trim().parse::<i64>().ok())
            .and_then(|reset| time::OffsetDateTime::from_unix_timestamp(reset).ok());
        Some(
            match reset.and_then(|reset| {
                let at = reset
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()?;
                Some((reset, at))
            }) {
                Some((reset, at)) => Limit {
                    lifts_in: (reset - time::OffsetDateTime::now_utc())
                        .try_into()
                        .unwrap_or(Duration::ZERO),
                    when: format!("at {at}"),
                },
                None => Limit {
                    lifts_in: seconds(UNSAID_LIMIT_SECONDS),
                    when: "later".to_owned(),
                },
            },
        )
    }
}

/// A limit GitHub sets on how often it is asked.
struct Limit {
    /// How long until it lifts.
    lifts_in: Duration,
    /// When it lifts, in words.
    when: String,
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

/// The app's installation on an account, as GitHub tells of it.
#[derive(Deserialize)]
struct AppInstallation {
    id: u64,
    account: User,
    #[serde(default)]
    permissions: HashMap<String, String>,
    suspended_at: Option<String>,
}

/// A token of an installation's, as GitHub gives it.
#[derive(Deserialize)]
struct IssuedToken {
    token: Token,
    expires_at: String,
}

/// A user's membership of an organization, as GitHub tells of it.
#[derive(Deserialize)]
struct Membership {
    /// `active`, or `pending` while an invitation is not yet accepted.
    state: String,
    user: User,
    organization: User,
}

/// A user, or an organization, as GitHub tells of them.
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

/// A token GitHub takes as someone's word — the one it gives a login to read
/// who logged in with, read with once and dropped; one the app signs for
/// itself; one of an installation's — never written anywhere.
#[derive(Clone, Deserialize)]
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

/// Whether `name` could be a GitHub username, or an organization's name:
/// letters, digits and hyphens, as many as GitHub allows, so it is looked up
/// as a name alone.
fn could_be_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
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
        for name in ["octocat", "Octo-Cat", "a", &"x".repeat(MAX_NAME_CHARS)] {
            assert!(could_be_name(name), "{name}");
        }
        for name in [
            "",
            "-octo",
            "octo/../user",
            "octo cat",
            "octo?",
            &"x".repeat(MAX_NAME_CHARS + 1),
        ] {
            assert!(!could_be_name(name), "{name}");
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
