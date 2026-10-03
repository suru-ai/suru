//! The Server's Relays (ADR-0045, ADR-0048): the entries its user adds and
//! removes, the logins it carries out at them on that user's behalf, and the
//! connection it keeps to each Relay it has logged in at. The Server proves
//! itself by its identity key every time it connects and holds no other
//! credential for a Relay; only the Relay ever speaks to an identity provider.
//!
//! Entries are kept beside the Server's Remotes and Peers, owner-only, and
//! nothing of them — an address, a code, an Account — reaches a Log
//! (ADR-0008).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use anyhow::Result;
use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use reqwest::header;
use serde::{Deserialize, Serialize};
use suru_relay_protocol::{
    self as relay_protocol, Bytes, ENDPOINT_PATH, Refusal, RelayMessage, SPOKEN, ServerMessage,
    Side,
};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, protocol::Role},
};

use crate::{
    protocol::{
        Relay, RelayAccount, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelayRemoval,
        RelaySide, RelayState, SessionErrorCode,
    },
    serving::{ServingController, machine_hostname, read_records, write_private_json},
};

const RELAYS_FILE: &str = "relays.json";

/// How long a Server waits on its Relays; injectable so tests see a Relay
/// that stops answering, and its recovery, without waiting out the defaults.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RelayTimings {
    /// How long a Relay may take to answer each thing asked of it before the
    /// Server stops waiting.
    pub(crate) answer_timeout: Duration,
    /// How long the Server first waits before trying a Relay again, doubling
    /// each time it still does not answer, up to `retry_max`.
    pub(crate) retry_initial: Duration,
    pub(crate) retry_max: Duration,
}

#[derive(Clone)]
pub(crate) struct RelayController {
    data_dir: PathBuf,
    /// Holds the identity key the Server proves itself by.
    serving: ServingController,
    dialer: Dialer,
    timings: RelayTimings,
    relays: Arc<StdMutex<Vec<HeldRelay>>>,
}

/// A Relay entry as it is stored. Beside the identity key, a Server's Logins
/// are the one part of what it stores that a Suru upgrade carries forward
/// (ADR-0047), so this tolerates fields a later Suru adds.
#[derive(Clone, Deserialize, Serialize)]
struct StoredRelay {
    address: String,
    /// Whether the Server has logged in at the Relay, and so connects to it on
    /// its own. A Login the Relay comes to refuse leaves this so, since one
    /// fresh login from any Server of its Account may restore it.
    #[serde(default)]
    logged_in: bool,
}

struct HeldRelay {
    stored: StoredRelay,
    state: RelayState,
    account: Option<RelayAccount>,
    login: Option<HeldLogin>,
    connection: Option<KeptConnection>,
}

struct HeldLogin {
    progress: watch::Sender<RelayLogin>,
    task: JoinHandle<()>,
}

/// The connection the Server keeps to a Relay it has logged in at.
struct KeptConnection {
    task: JoinHandle<()>,
    /// Has the connection tried again at once, rather than when its backoff
    /// next allows.
    retry_now: Arc<Notify>,
}

/// Why something asked of a Relay failed.
pub(crate) struct RelayFailure {
    pub(crate) code: SessionErrorCode,
    pub(crate) message: String,
}

impl RelayFailure {
    fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self.code {
            SessionErrorCode::RelayNotFound | SessionErrorCode::RelayLoginNotFound => {
                StatusCode::NOT_FOUND
            }
            SessionErrorCode::RelayAlreadyAdded | SessionErrorCode::RelayProtocolMismatch => {
                StatusCode::CONFLICT
            }
            SessionErrorCode::RelayUnreachable | SessionErrorCode::RelayRefused => {
                StatusCode::BAD_GATEWAY
            }
            SessionErrorCode::InvalidRelayAddress => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

fn relay_not_found() -> RelayFailure {
    RelayFailure::new(SessionErrorCode::RelayNotFound, "Relay not found")
}

fn records_failure(_error: anyhow::Error) -> RelayFailure {
    // Paths and what the records hold are not copied into an outward error a
    // caller might later Log.
    RelayFailure::new(
        SessionErrorCode::RelayRecordsUnwritable,
        "the Server could not store its Relays",
    )
}

impl RelayController {
    pub(crate) fn new(
        data_dir: &Path,
        serving: ServingController,
        timings: RelayTimings,
    ) -> Result<Self> {
        let stored: Vec<StoredRelay> = read_records(&data_dir.join(RELAYS_FILE))?;
        let relays = stored
            .into_iter()
            .map(|stored| HeldRelay {
                // A Login last known to stand is taken to stand until the
                // Relay says otherwise or stops answering.
                state: if stored.logged_in {
                    RelayState::LoggedIn
                } else {
                    RelayState::LoginNeeded
                },
                stored,
                account: None,
                login: None,
                connection: None,
            })
            .collect();
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            serving,
            dialer: Dialer::default(),
            timings,
            relays: Arc::new(StdMutex::new(relays)),
        })
    }

    /// Connects to every Relay the Server has logged in at, and goes on
    /// connecting on its own whenever a connection ends.
    pub(crate) fn start(&self) {
        let mut relays = self.lock();
        for held in relays.iter_mut().filter(|held| held.stored.logged_in) {
            held.connection = Some(self.keep_connected(held.stored.address.clone()));
        }
    }

    /// Ends every connection to a Relay and every login under way.
    pub(crate) fn shutdown(&self) {
        for held in self.lock().iter_mut() {
            stop(held);
        }
    }

    pub(crate) fn list(&self) -> Vec<Relay> {
        self.lock().iter().map(HeldRelay::relay).collect()
    }

    pub(crate) fn add(&self, address: &str) -> std::result::Result<Relay, RelayFailure> {
        let address = relay_address(address)?;
        let mut relays = self.lock();
        if relays.iter().any(|held| held.stored.address == address) {
            return Err(RelayFailure::new(
                SessionErrorCode::RelayAlreadyAdded,
                format!("the Relay at {address} is already added"),
            ));
        }
        relays.push(HeldRelay {
            stored: StoredRelay {
                address,
                logged_in: false,
            },
            state: RelayState::LoginNeeded,
            account: None,
            login: None,
            connection: None,
        });
        if let Err(error) = self.persist(&relays) {
            relays.pop();
            return Err(records_failure(error));
        }
        Ok(relays.last().expect("the Relay was just added").relay())
    }

    /// Begins a login at the Relay at `address`, proving the Server's key and
    /// reporting its hostname there, and answers where its user goes to log
    /// in and what they enter. The Server waits for the login to end on its
    /// own, so it outlives the Client that began it.
    pub(crate) async fn begin_login(
        &self,
        address: &str,
    ) -> std::result::Result<RelayLogin, RelayFailure> {
        let address = self.known(address)?;
        let answer_timeout = self.timings.answer_timeout;
        let (mut conversation, _) = self
            .dialer
            .open(&address, &self.serving, answer_timeout)
            .await
            .map_err(DialFailure::into_failure)?;
        conversation
            .say(&ServerMessage::BeginLogin {
                hostname: machine_hostname(),
            })
            .await
            .map_err(|_| stopped_answering())?;
        let (verification_uri, user_code, expires_in) =
            match tokio::time::timeout(answer_timeout, conversation.hear()).await {
                Ok(Some(RelayMessage::LoginStarted {
                    verification_uri,
                    user_code,
                    expires_in_seconds,
                })) => (
                    verification_uri,
                    user_code,
                    Duration::from_secs(expires_in_seconds),
                ),
                Ok(Some(RelayMessage::Refused { message, .. })) => {
                    return Err(RelayFailure::new(SessionErrorCode::RelayRefused, message));
                }
                Ok(Some(_)) => {
                    return Err(RelayFailure::new(
                        SessionErrorCode::RelayRefused,
                        "the Relay answered a login with something else",
                    ));
                }
                Ok(None) | Err(_) => return Err(stopped_answering()),
            };
        let login = RelayLogin {
            verification_uri,
            user_code,
            outcome: RelayLoginOutcome::Pending,
        };
        let progress = watch::Sender::new(login.clone());
        let mut relays = self.lock();
        let Some(held) = relays
            .iter_mut()
            .find(|held| held.stored.address == address)
        else {
            return Err(relay_not_found());
        };
        if let Some(previous) = held.login.take() {
            previous.task.abort();
        }
        held.login = Some(HeldLogin {
            task: tokio::spawn(self.clone().finish_login(
                address,
                conversation,
                progress.clone(),
                expires_in + answer_timeout,
            )),
            progress,
        });
        Ok(login)
    }

    /// Follows the latest login begun at the Relay at `address`.
    pub(crate) fn follow_login(
        &self,
        address: &str,
    ) -> std::result::Result<watch::Receiver<RelayLogin>, RelayFailure> {
        let address = relay_address(address)?;
        let relays = self.lock();
        let held = relays
            .iter()
            .find(|held| held.stored.address == address)
            .ok_or_else(relay_not_found)?;
        held.login
            .as_ref()
            .map(|login| login.progress.subscribe())
            .ok_or_else(|| {
                RelayFailure::new(
                    SessionErrorCode::RelayLoginNotFound,
                    "no login has been begun at this Relay",
                )
            })
    }

    /// Removes the Relay at `address`. The Relay is asked to forget the
    /// Server's Login first; the entry then goes whether or not it answered,
    /// since a Relay that cannot be reached is forgotten here all the same.
    pub(crate) async fn remove(
        &self,
        address: &str,
    ) -> std::result::Result<RelayRemoval, RelayFailure> {
        let address = self.known(address)?;
        if let Some(held) = self
            .lock()
            .iter_mut()
            .find(|held| held.stored.address == address)
        {
            // Nothing reconnects while the Relay is asked to forget.
            stop(held);
        }
        let acknowledged =
            tokio::time::timeout(self.timings.answer_timeout, self.ask_to_forget(&address))
                .await
                .unwrap_or(false);
        let mut relays = self.lock();
        let Some(index) = relays
            .iter()
            .position(|held| held.stored.address == address)
        else {
            return Err(relay_not_found());
        };
        let mut removed = relays.remove(index);
        if let Err(error) = self.persist(&relays) {
            relays.insert(index, removed);
            return Err(records_failure(error));
        }
        // Whatever was begun while the Relay was asked goes with the entry.
        stop(&mut removed);
        Ok(RelayRemoval {
            address,
            acknowledged,
        })
    }

    async fn ask_to_forget(&self, address: &str) -> bool {
        let Ok((mut conversation, _)) = self
            .dialer
            .open(address, &self.serving, self.timings.answer_timeout)
            .await
        else {
            return false;
        };
        conversation.say(&ServerMessage::Forget).await.is_ok()
            && matches!(conversation.hear().await, Some(RelayMessage::Forgotten))
    }

    /// Waits for the login begun over `conversation` to end, within `budget`,
    /// and reports how it did.
    async fn finish_login(
        self,
        address: String,
        mut conversation: Conversation,
        progress: watch::Sender<RelayLogin>,
        budget: Duration,
    ) {
        let outcome = match tokio::time::timeout(budget, conversation.hear()).await {
            Ok(Some(RelayMessage::LoginDone { account })) => {
                let account = RelayAccount {
                    provider: account.provider,
                    username: account.username,
                };
                self.logged_in(&address, account.clone());
                RelayLoginOutcome::Done { account }
            }
            Ok(Some(RelayMessage::Refused { refusal, message })) => RelayLoginOutcome::Refused {
                reason: match refusal {
                    Refusal::LoginDenied => RelayLoginRefusal::Denied,
                    Refusal::LoginExpired => RelayLoginRefusal::Expired,
                    _ => RelayLoginRefusal::Unavailable,
                },
                message,
            },
            Ok(Some(_)) => RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Unavailable,
                message: "the Relay answered the login with something else".to_owned(),
            },
            Ok(None) | Err(_) => RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::Interrupted,
                message: "the Relay stopped answering before the login ended".to_owned(),
            },
        };
        conversation.close().await;
        progress.send_modify(|login| login.outcome = outcome);
    }

    /// Records that the Server's Login at the Relay at `address` stands under
    /// `account`, and connects there from now on.
    fn logged_in(&self, address: &str, account: RelayAccount) {
        let mut relays = self.lock();
        let Some(index) = relays
            .iter()
            .position(|held| held.stored.address == address)
        else {
            return;
        };
        if !relays[index].stored.logged_in {
            relays[index].stored.logged_in = true;
            if let Err(error) = self.persist(&relays) {
                tracing::warn!("could not store a Relay's Login: {error:#}");
            }
        }
        let held = &mut relays[index];
        held.state = RelayState::LoggedIn;
        held.account = Some(account);
        match &held.connection {
            Some(connection) => connection.retry_now.notify_one(),
            None => held.connection = Some(self.keep_connected(held.stored.address.clone())),
        }
    }

    /// Keeps a connection to the Relay at `address`, trying again with
    /// backoff whenever it ends or cannot be made — even while the Relay
    /// refuses the Server's Login, since one fresh login elsewhere may
    /// restore it.
    fn keep_connected(&self, address: String) -> KeptConnection {
        let retry_now = Arc::new(Notify::new());
        let controller = self.clone();
        let retry = retry_now.clone();
        let task = tokio::spawn(async move {
            let RelayTimings {
                answer_timeout,
                retry_initial,
                retry_max,
            } = controller.timings;
            let mut backoff = retry_initial;
            loop {
                match controller
                    .dialer
                    .open(&address, &controller.serving, answer_timeout)
                    .await
                {
                    Ok((mut conversation, Some(account))) => {
                        controller.observe(
                            &address,
                            RelayState::LoggedIn,
                            Some(RelayAccount {
                                provider: account.provider,
                                username: account.username,
                            }),
                        );
                        backoff = retry_initial;
                        conversation.ended().await;
                    }
                    Ok((conversation, None)) => {
                        conversation.close().await;
                        controller.observe(&address, RelayState::LoginNeeded, None);
                    }
                    Err(DialFailure::Mismatch { behind, .. }) => {
                        let behind = match behind {
                            Side::Server => RelaySide::Server,
                            Side::Relay => RelaySide::Relay,
                        };
                        controller.observe(&address, RelayState::ProtocolMismatch { behind }, None);
                    }
                    Err(DialFailure::Unreachable(_) | DialFailure::Refused(_)) => {
                        controller.observe(&address, RelayState::Unreachable, None);
                    }
                }
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = retry.notified() => {}
                }
                backoff = (backoff * 2).min(retry_max);
            }
        });
        KeptConnection { task, retry_now }
    }

    /// Records how the Relay at `address` now stands, and, where the Server's
    /// Login there stands, the Account it stands under.
    fn observe(&self, address: &str, state: RelayState, account: Option<RelayAccount>) {
        let mut relays = self.lock();
        let Some(held) = relays
            .iter_mut()
            .find(|held| held.stored.address == address)
        else {
            return;
        };
        held.state = state;
        match state {
            RelayState::LoggedIn => held.account = account,
            RelayState::LoginNeeded => held.account = None,
            RelayState::Unreachable | RelayState::ProtocolMismatch { .. } => {}
        }
    }

    /// The address of the Relay `address` names, where the Server holds an
    /// entry for it.
    fn known(&self, address: &str) -> std::result::Result<String, RelayFailure> {
        let address = relay_address(address)?;
        if self
            .lock()
            .iter()
            .any(|held| held.stored.address == address)
        {
            Ok(address)
        } else {
            Err(relay_not_found())
        }
    }

    fn persist(&self, relays: &[HeldRelay]) -> Result<()> {
        let stored = relays
            .iter()
            .map(|held| held.stored.clone())
            .collect::<Vec<_>>();
        write_private_json(&self.data_dir.join(RELAYS_FILE), &stored)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<HeldRelay>> {
        self.relays
            .lock()
            .expect("Relay record lock is not poisoned")
    }
}

impl HeldRelay {
    fn relay(&self) -> Relay {
        Relay {
            address: self.stored.address.clone(),
            state: self.state,
            account: self.account.clone(),
            login: self
                .login
                .as_ref()
                .map(|login| login.progress.borrow().clone()),
        }
    }
}

/// Ends the connection kept to `held` and any login under way there.
fn stop(held: &mut HeldRelay) {
    if let Some(connection) = held.connection.take() {
        connection.task.abort();
    }
    if let Some(login) = &held.login {
        login.task.abort();
    }
}

fn stopped_answering() -> RelayFailure {
    RelayFailure::new(
        SessionErrorCode::RelayUnreachable,
        "the Relay stopped answering",
    )
}

/// The address `address` names a Relay by: an `http` or `https` URL naming a
/// host, `https` where no scheme is given, with no credentials, query or
/// fragment, and no trailing slash.
fn relay_address(address: &str) -> std::result::Result<String, RelayFailure> {
    let invalid = || {
        RelayFailure::new(
            SessionErrorCode::InvalidRelayAddress,
            "a Relay's address is an https:// or http:// address naming its host",
        )
    };
    let address = address.trim();
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let address = if address.contains("://") {
        address.to_owned()
    } else {
        format!("https://{address}")
    };
    let url = reqwest::Url::parse(&address).map_err(|_| invalid())?;
    let usable = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if !usable {
        return Err(invalid());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Opens connections to Relays: WebSockets over HTTP or HTTPS, taken through
/// the system's HTTP proxy and verified against the operating system's trust
/// store. Its HTTP client is made at its first dial, so a Server holding no
/// Relay never reads the trust store.
#[derive(Clone, Default)]
struct Dialer {
    http: Arc<OnceLock<reqwest::Client>>,
}

/// Why a connection to a Relay could not be made.
enum DialFailure {
    /// The Relay did not answer, saying why.
    Unreachable(String),
    /// The Relay and the Server share no version, and `behind` is the side
    /// to upgrade.
    Mismatch { behind: Side, message: String },
    /// The Relay refused the Server, saying why.
    Refused(String),
}

impl DialFailure {
    fn into_failure(self) -> RelayFailure {
        match self {
            Self::Unreachable(message) => {
                RelayFailure::new(SessionErrorCode::RelayUnreachable, message)
            }
            Self::Mismatch { message, .. } => {
                RelayFailure::new(SessionErrorCode::RelayProtocolMismatch, message)
            }
            Self::Refused(message) => RelayFailure::new(SessionErrorCode::RelayRefused, message),
        }
    }
}

impl Dialer {
    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(relay_http_client)
    }

    /// Opens a connection to the Relay at `address` and proves the Server's
    /// identity key there, each step within `answer_timeout`: what the Relay
    /// then says of the Server's Login — the Account it stands under, where
    /// it stands.
    async fn open(
        &self,
        address: &str,
        serving: &ServingController,
        answer_timeout: Duration,
    ) -> std::result::Result<(Conversation, Option<relay_protocol::Account>), DialFailure> {
        let unanswered = || DialFailure::Unreachable("the Relay did not answer in time".to_owned());
        let identity_failure =
            |_| DialFailure::Refused("the Server could not use its identity key".to_owned());
        let key = serving.identity_public_key().map_err(identity_failure)?;
        let mut conversation = tokio::time::timeout(answer_timeout, self.websocket(address))
            .await
            .map_err(|_| unanswered())??;
        conversation
            .say(&ServerMessage::Hello {
                versions: SPOKEN.to_vec(),
                key: Bytes(key.clone()),
            })
            .await
            .map_err(|_| unanswered())?;
        let nonce = match tokio::time::timeout(answer_timeout, conversation.hear()).await {
            Ok(Some(RelayMessage::Challenge { nonce, .. })) => nonce.0,
            Ok(Some(RelayMessage::Refused {
                refusal: Refusal::VersionNotSupported { versions, behind },
                ..
            })) => {
                return Err(DialFailure::Mismatch {
                    behind,
                    message: mismatch(&versions, behind),
                });
            }
            Ok(Some(RelayMessage::Refused { message, .. })) => {
                return Err(DialFailure::Refused(message));
            }
            Ok(Some(_)) => {
                return Err(DialFailure::Refused(
                    "the Relay did not challenge the Server to prove its key".to_owned(),
                ));
            }
            Ok(None) | Err(_) => return Err(unanswered()),
        };
        let signature = serving
            .sign_with_identity(&relay_protocol::proof_message(&nonce, &key))
            .map_err(identity_failure)?;
        conversation
            .say(&ServerMessage::Proof {
                signature: Bytes(signature),
            })
            .await
            .map_err(|_| unanswered())?;
        match tokio::time::timeout(answer_timeout, conversation.hear()).await {
            Ok(Some(RelayMessage::Proven { login })) => Ok((conversation, login)),
            Ok(Some(RelayMessage::Refused { message, .. })) => Err(DialFailure::Refused(message)),
            Ok(Some(_)) => Err(DialFailure::Refused(
                "the Relay answered the Server's proof with something else".to_owned(),
            )),
            Ok(None) | Err(_) => Err(unanswered()),
        }
    }

    /// Opens a WebSocket to the Relay at `address` by upgrading an HTTP
    /// request, so it goes wherever the HTTP client's proxy and trust send
    /// it.
    async fn websocket(&self, address: &str) -> std::result::Result<Conversation, DialFailure> {
        let key = tungstenite::handshake::client::generate_key();
        let response = self
            .http()
            .get(format!("{address}{ENDPOINT_PATH}"))
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, &key)
            .send()
            .await
            .map_err(|error| DialFailure::Unreachable(unreachable_reason(&error)))?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(DialFailure::Unreachable(format!(
                "the Relay's address answered HTTP {} rather than opening a connection",
                response.status()
            )));
        }
        let accepted = response
            .headers()
            .get(header::SEC_WEBSOCKET_ACCEPT)
            .is_some_and(|accept| {
                accept.as_bytes()
                    == tungstenite::handshake::derive_accept_key(key.as_bytes()).as_bytes()
            });
        if !accepted {
            return Err(DialFailure::Unreachable(
                "the Relay's address did not open a WebSocket".to_owned(),
            ));
        }
        let upgraded = response.upgrade().await.map_err(|_| {
            DialFailure::Unreachable("the Relay's connection ended as it opened".to_owned())
        })?;
        Ok(Conversation {
            socket: WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await,
        })
    }
}

/// The HTTP client a Server reaches its Relays with: through the system's HTTP
/// proxy, as reqwest finds it, and trusting what the operating system's trust
/// store trusts. A trust store that cannot be read trusts nothing, so every
/// HTTPS Relay is refused rather than the Server failing.
fn relay_http_client() -> reqwest::Client {
    use rustls_platform_verifier::BuilderVerifierExt as _;

    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring speaks every TLS version rustls deems safe");
    let mut tls = match builder.clone().with_platform_verifier() {
        Ok(verified) => verified.with_no_client_auth(),
        Err(error) => {
            tracing::warn!("no HTTPS Relay is trusted: the trust store is unreadable: {error}");
            builder
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth()
        }
    };
    // A WebSocket is opened by upgrading an HTTP/1.1 request.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .http1_only()
        .build()
        .expect("a Relay HTTP client over rustls needs nothing that can fail")
}

/// Says why a Relay could not be reached, telling a certificate this machine
/// does not trust apart from a Relay that does not answer at all.
fn unreachable_reason(error: &reqwest::Error) -> String {
    if untrusted_certificate(error) {
        "the Relay's certificate is not one this machine's trust store trusts".to_owned()
    } else {
        "could not reach the Relay".to_owned()
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

/// Says which side of a version mismatch is behind, and so which to upgrade.
fn mismatch(relay_versions: &[relay_protocol::Version], behind: Side) -> String {
    let ours = SPOKEN
        .iter()
        .max()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let theirs = relay_versions
        .iter()
        .max()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    match behind {
        Side::Server => format!(
            "this Server speaks Relay protocol {ours} and the Relay {theirs}: this Server is \
             behind, so upgrade Suru"
        ),
        Side::Relay => format!(
            "this Server speaks Relay protocol {ours} and the Relay {theirs}: the Relay is \
             behind, so its operator must upgrade it"
        ),
    }
}

/// A WebSocket to a Relay, carrying one JSON message to a text frame.
struct Conversation {
    socket: WebSocketStream<reqwest::Upgraded>,
}

impl Conversation {
    async fn say(&mut self, message: &ServerMessage) -> std::result::Result<(), ()> {
        let text = serde_json::to_string(message).expect("a Server message always encodes");
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| ())
    }

    /// The next thing the Relay says that this Server recognizes, or `None`
    /// once the connection has ended.
    async fn hear(&mut self) -> Option<RelayMessage> {
        loop {
            match self.socket.next().await? {
                Ok(Message::Text(text)) => match serde_json::from_str(text.as_str()) {
                    Ok(RelayMessage::Unrecognized) | Err(_) => {}
                    Ok(message) => return Some(message),
                },
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// Waits for the connection to end, passing over whatever the Relay says.
    async fn ended(&mut self) {
        while self.hear().await.is_some() {}
    }

    async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_is_known_by_its_address_written_one_way() {
        for (written, address) in [
            ("relay.example.com", "https://relay.example.com"),
            ("https://Relay.Example.com/", "https://relay.example.com"),
            ("https://relay.example.com:443", "https://relay.example.com"),
            ("  http://127.0.0.1:8080  ", "http://127.0.0.1:8080"),
            ("relay.example.com:8443", "https://relay.example.com:8443"),
            (
                "https://example.com/suru/relay/",
                "https://example.com/suru/relay",
            ),
        ] {
            assert_eq!(
                relay_address(written).ok().as_deref(),
                Some(address),
                "{written:?}"
            );
        }
    }

    #[test]
    fn an_address_no_relay_is_reached_at_is_refused() {
        for written in [
            "",
            "   ",
            "ftp://relay.example.com",
            "wss://relay.example.com",
            "https://user:secret@relay.example.com",
            "https://relay.example.com/?token=1",
            "https://relay.example.com/#here",
            "https://relay example.com",
            "https://",
        ] {
            assert_eq!(
                relay_address(written).err().map(|failure| failure.code),
                Some(SessionErrorCode::InvalidRelayAddress),
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_refused_certificate_is_told_apart_however_deeply_it_is_wrapped() {
        let refused = std::io::Error::other(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        ));
        assert!(untrusted_certificate(&refused));
        assert!(!untrusted_certificate(&std::io::Error::other(
            "connection refused"
        )));
        assert!(!untrusted_certificate(&std::io::Error::other(
            rustls::Error::HandshakeNotComplete
        )));
    }

    #[test]
    fn a_mismatch_says_which_side_is_behind() {
        let behind_server = mismatch(&[relay_protocol::Version::Unstable(99)], Side::Server);
        assert!(
            behind_server.contains("this Server is behind")
                && behind_server.contains("unstable-99"),
            "{behind_server}"
        );
        let behind_relay = mismatch(&[relay_protocol::Version::Unstable(0)], Side::Relay);
        assert!(
            behind_relay.contains("the Relay is behind"),
            "{behind_relay}"
        );
    }
}
