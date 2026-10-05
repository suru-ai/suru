//! The Server's Relays (ADR-0045, ADR-0048): the entries its user adds and
//! removes, the logins it carries out at them on that user's behalf, and the
//! connection it keeps to each Relay it has logged in at — on which, while
//! it is Serving and its user has chosen to Serve through that Relay, it
//! waits to be reached, taking up each join asked of it and handing what the
//! join carries to the Serving side. As the redeeming side of a Pairing, it
//! asks at a Relay it has logged in at to be joined to the Serving Server a
//! Relay way names, whenever that way is dialled. The Server proves itself
//! by its identity key every time it connects and holds no other credential
//! for a Relay; only the Relay ever speaks to an identity provider.
//!
//! Entries are kept beside the Server's Remotes and Peers, owner-only, and
//! nothing of them — an address, a code, an Account — reaches a Log
//! (ADR-0008).

use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll, ready},
    time::Duration,
};

use anyhow::Result;
use axum::http::StatusCode;
use futures_util::{Sink, SinkExt, StreamExt, task::AtomicWaker};
use reqwest::header;
use serde::{Deserialize, Serialize};
use suru_relay_protocol::{
    self as relay_protocol, Bytes, Cap, ENDPOINT_PATH, MAX_MESSAGE_LEN, Refusal, RelayMessage,
    SPOKEN, ServerMessage, Side,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex as AsyncMutex, Notify, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        self, Message,
        protocol::{Role, WebSocketConfig},
    },
};
use uuid::Uuid;

#[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
use crate::serving::SOCKET_USER_TIMEOUT;
use crate::{
    protocol::{
        Relay, RelayAccount, RelayListing, RelayLogin, RelayLoginOutcome, RelayLoginRefusal,
        RelayRemoval, RelaySide, RelayState, RelayUnreachable, SessionErrorCode, UnreachableReason,
    },
    runtime::replace_private_file,
    serving::{
        ByteStream, IdentityKey, MAX_TOLD_RELAY_LEN, MAX_TOLD_RELAYS, RelayJoin, RelayRefusal,
        RelayWays, SOCKET_KEEPALIVE, SOCKET_KEEPALIVE_PROBES, ServingController, ServingStretch,
        Wanted, machine_hostname, no_longer_wanted, read_records,
    },
};

const RELAYS_FILE: &str = "relays.json";

/// The longest the Server waits on a login, whatever its Relay says the login
/// may take: a device login lasts minutes.
const LONGEST_LOGIN: Duration = Duration::from_secs(60 * 60);

/// How many joins asked of the Server at one Relay it takes up at once; a
/// Relay asking more is ignored until one is taken up, so no Relay can have
/// the Server open connections without end.
const TAKE_UPS_AT_ONCE: usize = 16;

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
    /// How often the Server asks a Relay it is connected to whether it still
    /// answers.
    pub(crate) heartbeat_interval: Duration,
    /// How long the Relay may take to answer that before the Server holds it
    /// as having stopped answering.
    pub(crate) heartbeat_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct RelayController {
    data_dir: PathBuf,
    serving: ServingController,
    /// The identity key the Server proves itself by.
    identity: IdentityKey,
    dialer: Dialer,
    timings: RelayTimings,
    relays: Arc<StdMutex<Vec<HeldRelay>>>,
    /// The Relays as the Server's Clients are told of them, published anew,
    /// at the next revision, as any of them changes. Published with the
    /// Relays held, so each revision is one state of them.
    published: Arc<watch::Sender<RelayListing>>,
    /// Whether what the Server holds of its Relays has yet to be stored, the
    /// latest write of it having failed: it is written again as the Server
    /// next hears from a Relay, until a write succeeds. Read and written with
    /// the Relays held, as every write is made.
    unstored: Arc<AtomicBool>,
    /// The joins the Relays carry for the Server, which end as it stops.
    carrying: Arc<Carrying>,
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
    /// Whether the Relay was last found to refuse the Server's Login there,
    /// so that it needs renewing however the Relay answers meanwhile: set as
    /// the Relay refuses it, and cleared only as the Relay takes the Server's
    /// proof with a Login that stands, or a login there is done.
    #[serde(default)]
    login_needed: bool,
    /// Whether the Server Serves through the Relay, as its user chose.
    #[serde(default)]
    serve_through: bool,
    /// Where the Relay came to refuse a Login that stood there and no Client
    /// has raised its Notice of that yet, that lapse: one is made as the
    /// Relay comes to refuse the Login, and it goes as a Client says it
    /// raised the Notice of that very lapse, or as the Login stands again.
    /// Stored, so a Client opened later, or after the Server restarts,
    /// raises it once and only once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    untold_lapse: Option<Uuid>,
}

struct HeldRelay {
    stored: StoredRelay,
    /// Says whether the Server Serves through the Relay to the connection
    /// kept to it, and which making of that choice it is, as the stored
    /// choice changes.
    serve_through: watch::Sender<ServeThrough>,
    state: RelayState,
    unreachable: Option<RelayUnreachable>,
    account: Option<RelayAccount>,
    login: Option<HeldLogin>,
    connection: Option<KeptConnection>,
    /// Held across beginning a login and across a removal, so neither runs
    /// while the other does: no login can be begun once removal has, and
    /// none begun before it can finish once removal asks the Relay to forget.
    operations: Arc<AsyncMutex<()>>,
}

struct HeldLogin {
    progress: watch::Sender<RelayLogin>,
    /// Ends once the login has, and is taken by what waits on it.
    task: Option<JoinHandle<()>>,
    /// Has the login given up: its conversation ended, and the Relay heard
    /// out until it lets it go.
    give_up: Arc<Notify>,
}

/// The connection the Server keeps to a Relay it has logged in at.
struct KeptConnection {
    task: JoinHandle<()>,
    /// Has the connection tried again at once, rather than when its backoff
    /// next allows.
    retry_now: Arc<Notify>,
}

/// Why something asked of a Relay failed.
#[derive(Debug)]
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
            SessionErrorCode::RelayAlreadyAdded
            | SessionErrorCode::RelayProtocolMismatch
            | SessionErrorCode::RelayServeThroughLimitReached => StatusCode::CONFLICT,
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
    /// The Server's Relays, as they are stored under `data_dir`, published
    /// to its Clients as those of `instance`, the run of the Server it is.
    pub(crate) fn new(
        data_dir: &Path,
        serving: ServingController,
        timings: RelayTimings,
        instance: Uuid,
    ) -> Result<Self> {
        let stored: Vec<StoredRelay> = read_records(&data_dir.join(RELAYS_FILE))?;
        let relays: Vec<HeldRelay> = stored
            .into_iter()
            .map(|stored| HeldRelay {
                // A Login last known to stand is taken to stand until the
                // Relay says otherwise or stops answering.
                state: if stored.logged_in && !stored.login_needed {
                    RelayState::LoggedIn
                } else {
                    RelayState::LoginNeeded
                },
                serve_through: watch::Sender::new(ServeThrough::first(stored.serve_through)),
                stored,
                unreachable: None,
                account: None,
                login: None,
                connection: None,
                operations: Arc::default(),
            })
            .collect();
        let listing = RelayListing {
            instance,
            revision: 0,
            relays: relays.iter().map(HeldRelay::relay).collect(),
        };
        let controller = Self {
            data_dir: data_dir.to_path_buf(),
            identity: serving.identity_key(),
            serving,
            dialer: Dialer::default(),
            timings,
            relays: Arc::new(StdMutex::new(relays)),
            published: Arc::new(watch::Sender::new(listing)),
            unstored: Arc::default(),
            carrying: Arc::default(),
        };
        controller.serving.reach_relays_through(Arc::new(Joining {
            relays: controller.relays.clone(),
            identity: controller.identity.clone(),
            dialer: controller.dialer.clone(),
            timings,
            carrying: controller.carrying.clone(),
        }));
        controller.serving.offer_relays(told(&controller.lock()));
        Ok(controller)
    }

    /// Connects to every Relay the Server has logged in at, and goes on
    /// connecting on its own whenever a connection ends.
    pub(crate) fn start(&self) {
        let mut relays = self.lock();
        for held in relays.iter_mut().filter(|held| held.stored.logged_in) {
            held.connection = Some(self.keep_connected(held));
        }
    }

    /// Ends every connection to a Relay, every join a Relay carries for the
    /// Server, and every login under way.
    pub(crate) fn shutdown(&self) {
        self.carrying.stop_all();
        for held in self.lock().iter_mut() {
            stop(held);
        }
    }

    /// The Server's Relays as they stand now, at the revision they are
    /// published at.
    pub(crate) fn list(&self) -> RelayListing {
        let relays = self.lock();
        self.publish(&relays);
        self.published.borrow().clone()
    }

    /// The Relays as they are published: as they stand now, then each
    /// revision of them.
    pub(crate) fn subscribe(&self) -> watch::Receiver<RelayListing> {
        self.published.subscribe()
    }

    /// Records that a Client has raised its Notice of the Relay at `address`
    /// coming to need a login in `lapse`, so no Client raises it again. Said
    /// of a lapse other than the one the Notice is asked for — an earlier one,
    /// said late — it changes nothing. Nothing changes where it cannot be
    /// stored.
    pub(crate) fn notice_login_needed(
        &self,
        address: &str,
        lapse: Uuid,
    ) -> std::result::Result<Relay, RelayFailure> {
        let address = relay_address(address)?;
        let mut relays = self.lock();
        let index = relays
            .iter()
            .position(|held| held.stored.address == address)
            .ok_or_else(relay_not_found)?;
        if relays[index].stored.untold_lapse == Some(lapse) {
            let mut stored = relays
                .iter()
                .map(|held| held.stored.clone())
                .collect::<Vec<_>>();
            stored[index].untold_lapse = None;
            self.write(&stored).map_err(records_failure)?;
            relays[index].stored.untold_lapse = None;
            self.publish(&relays);
        }
        Ok(relays[index].relay())
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
                login_needed: false,
                serve_through: false,
                untold_lapse: None,
            },
            serve_through: watch::Sender::new(ServeThrough::first(false)),
            state: RelayState::LoginNeeded,
            unreachable: None,
            account: None,
            login: None,
            connection: None,
            operations: Arc::default(),
        });
        if let Err(error) = self.persist(&relays) {
            relays.pop();
            return Err(records_failure(error));
        }
        self.publish(&relays);
        Ok(relays.last().expect("the Relay was just added").relay())
    }

    /// Chooses whether the Server Serves through the Relay at `address`: while
    /// it is Serving and its Login there stands, it waits at the Relay to be
    /// reached by the Servers paired with it. Nothing changes where the
    /// choice cannot be stored.
    pub(crate) fn set_serve_through(
        &self,
        address: &str,
        serve_through: bool,
    ) -> std::result::Result<Relay, RelayFailure> {
        let address = relay_address(address)?;
        let mut relays = self.lock();
        let index = relays
            .iter()
            .position(|held| held.stored.address == address)
            .ok_or_else(relay_not_found)?;
        // Every Relay it Serves through is told to its Peers, whole.
        if serve_through
            && !relays[index].stored.serve_through
            && relays
                .iter()
                .filter(|held| held.stored.serve_through)
                .count()
                >= MAX_TOLD_RELAYS
        {
            return Err(RelayFailure::new(
                SessionErrorCode::RelayServeThroughLimitReached,
                format!(
                    "this Server already Serves through {MAX_TOLD_RELAYS} Relays, as many as it \
                     tells the Servers paired with it of; stop Serving through one of them first"
                ),
            ));
        }
        if relays[index].stored.serve_through != serve_through {
            let mut stored = relays
                .iter()
                .map(|held| held.stored.clone())
                .collect::<Vec<_>>();
            stored[index].serve_through = serve_through;
            self.write(&stored).map_err(records_failure)?;
            let held = &mut relays[index];
            held.stored.serve_through = serve_through;
            held.serve_through
                .send_modify(|choice| *choice = choice.made_again(serve_through));
            self.publish(&relays);
        }
        Ok(relays[index].relay())
    }

    /// Begins a login at the Relay at `address`, proving the Server's key and
    /// reporting its hostname there, and answers where its user goes to log
    /// in and what they enter. The Server waits for the login to end on its
    /// own, so it outlives the Client that began it.
    pub(crate) async fn begin_login(
        &self,
        address: &str,
    ) -> std::result::Result<RelayLogin, RelayFailure> {
        let address = relay_address(address)?;
        let operations = self.operations(&address)?;
        self.begin_login_holding(address, operations).await
    }

    /// Begins a login at the Relay at `address` once nothing else holds the
    /// entry's `operations`.
    async fn begin_login_holding(
        &self,
        address: String,
        operations: Arc<AsyncMutex<()>>,
    ) -> std::result::Result<RelayLogin, RelayFailure> {
        let _serialised = operations.lock().await;
        // A removal this waited on has taken the entry with it, whatever has
        // been added at its address since.
        if owning(&mut self.lock(), &address, &operations).is_none() {
            return Err(relay_not_found());
        }
        let answer_timeout = self.timings.answer_timeout;
        let (mut conversation, _) = self
            .dialer
            .open(&address, &self.identity, answer_timeout)
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
                    Duration::from_secs(expires_in_seconds).min(LONGEST_LOGIN),
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
        let budget = expires_in.saturating_add(answer_timeout);
        let progress = watch::Sender::new(login.clone());
        let give_up = Arc::new(Notify::new());
        let mut relays = self.lock();
        let Some(held) = owning(&mut relays, &address, &operations) else {
            return Err(relay_not_found());
        };
        if let Some(previous) = held.login.take() {
            if let Some(task) = previous.task {
                task.abort();
            }
            settle_abandoned(
                &previous.progress,
                "the login was given up for a later one at the same Relay",
            );
        }
        held.login = Some(HeldLogin {
            task: Some(tokio::spawn(self.clone().finish_login(
                address,
                conversation,
                progress.clone(),
                budget,
                give_up.clone(),
            ))),
            progress,
            give_up,
        });
        self.publish(&relays);
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

    /// Removes the Relay at `address`. Any login under way there is given up
    /// first, and heard out until the Relay lets it go, so nothing it finishes
    /// can follow what comes next: the Relay is asked to forget the Server's
    /// Login. The entry then goes whether or not it answered, since a Relay
    /// that cannot be reached is forgotten here all the same; where the
    /// entry cannot be forgotten here, it stays and goes on as before.
    pub(crate) async fn remove(
        &self,
        address: &str,
    ) -> std::result::Result<RelayRemoval, RelayFailure> {
        let address = relay_address(address)?;
        let operations = self.operations(&address)?;
        self.remove_holding(address, operations).await
    }

    /// Removes the Relay at `address` once nothing else holds the entry's
    /// `operations`.
    async fn remove_holding(
        &self,
        address: String,
        operations: Arc<AsyncMutex<()>>,
    ) -> std::result::Result<RelayRemoval, RelayFailure> {
        let _serialised = operations.lock().await;
        let login = {
            let mut relays = self.lock();
            let held = owning(&mut relays, &address, &operations).ok_or_else(relay_not_found)?;
            held.login
                .as_mut()
                .and_then(|login| Some((login.give_up.clone(), login.task.take()?)))
        };
        if let Some((give_up, task)) = login {
            give_up.notify_one();
            let _ = task.await;
        }
        // Nothing reconnects while the Relay is asked to forget, and nothing
        // taken up there is handed on, however far it has got.
        if let Some(held) = owning(&mut self.lock(), &address, &operations) {
            held.serve_through
                .send_modify(|choice| *choice = choice.made_again(false));
            if let Some(connection) = held.connection.take() {
                connection.task.abort();
            }
        }
        let acknowledged =
            tokio::time::timeout(self.timings.answer_timeout, self.ask_to_forget(&address))
                .await
                .unwrap_or(false);
        let mut relays = self.lock();
        let Some(index) = relays.iter().position(|held| {
            held.stored.address == address && Arc::ptr_eq(&held.operations, &operations)
        }) else {
            return Err(relay_not_found());
        };
        let removed = relays.remove(index);
        if let Err(error) = self.persist(&relays) {
            relays.insert(index, removed);
            let held = &mut relays[index];
            let serve_through = held.stored.serve_through;
            held.serve_through
                .send_modify(|choice| *choice = choice.made_again(serve_through));
            if acknowledged {
                held.refused();
            }
            if held.stored.logged_in {
                held.connection = Some(self.keep_connected(held));
            }
            self.publish(&relays);
            return Err(records_failure(error));
        }
        self.publish(&relays);
        Ok(RelayRemoval {
            address,
            acknowledged,
        })
    }

    async fn ask_to_forget(&self, address: &str) -> bool {
        let Ok((mut conversation, _)) = self
            .dialer
            .open(address, &self.identity, self.timings.answer_timeout)
            .await
        else {
            return false;
        };
        conversation.say(&ServerMessage::Forget).await.is_ok()
            && matches!(conversation.hear().await, Some(RelayMessage::Forgotten))
    }

    /// Waits for the login begun over `conversation` to end, within `budget`,
    /// and reports how it did — or, told to give up, ends the conversation
    /// and hears the Relay out until it lets it go.
    async fn finish_login(
        self,
        address: String,
        mut conversation: Conversation,
        progress: watch::Sender<RelayLogin>,
        budget: Duration,
        give_up: Arc<Notify>,
    ) {
        let heard = tokio::select! {
            heard = tokio::time::timeout(budget, conversation.hear()) => heard,
            () = give_up.notified() => {
                conversation.end(self.timings.answer_timeout).await;
                settle_abandoned(&progress, "the login was given up as its Relay was being removed");
                self.publish(&self.lock());
                return;
            }
        };
        let outcome = match heard {
            Ok(Some(RelayMessage::LoginDone { account })) => {
                let account = RelayAccount {
                    provider: account.provider,
                    username: account.username,
                };
                match self.logged_in(&address, account.clone()) {
                    Ok(()) => RelayLoginOutcome::Done { account },
                    Err(error) => {
                        tracing::warn!("could not record a Relay's Login: {error:#}");
                        RelayLoginOutcome::Refused {
                            reason: RelayLoginRefusal::Unrecorded,
                            message: "the Relay logged this Server in, but the Server could not \
                                      record its Login there; log in again"
                                .to_owned(),
                        }
                    }
                }
            }
            Ok(Some(RelayMessage::Refused {
                refusal:
                    Refusal::CapReached {
                        cap: Cap::Logins,
                        limit,
                    },
                ..
            })) => RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::LoginsCapReached {
                    limit: u32::try_from(limit).unwrap_or(u32::MAX),
                },
                message: logins_capped(&address, limit),
            },
            Ok(Some(RelayMessage::Refused { refusal, message })) => RelayLoginOutcome::Refused {
                reason: match refusal {
                    Refusal::LoginDenied => RelayLoginRefusal::Denied,
                    Refusal::NotAdmitted => RelayLoginRefusal::NotAdmitted,
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
        // Settled with the Relays held, so the revision published with it
        // says how the login ended and how the Relay then stood together.
        let relays = self.lock();
        progress.send_modify(|login| login.outcome = outcome);
        self.publish(&relays);
    }

    /// Records that the Server's Login at the Relay at `address` stands under
    /// `account`, and connects there from now on. Nothing changes where the
    /// Login cannot be stored, so a later login stores it.
    fn logged_in(&self, address: &str, account: RelayAccount) -> Result<()> {
        let mut relays = self.lock();
        let Some(index) = relays
            .iter()
            .position(|held| held.stored.address == address)
        else {
            return Ok(());
        };
        if !relays[index].stored.logged_in
            || relays[index].stored.login_needed
            || relays[index].stored.untold_lapse.is_some()
        {
            let mut stored = relays
                .iter()
                .map(|held| held.stored.clone())
                .collect::<Vec<_>>();
            stored[index].logged_in = true;
            stored[index].login_needed = false;
            stored[index].untold_lapse = None;
            self.write(&stored)?;
            relays[index].stored.logged_in = true;
            relays[index].stored.login_needed = false;
            relays[index].stored.untold_lapse = None;
        }
        let held = &mut relays[index];
        held.state = RelayState::LoggedIn;
        held.unreachable = None;
        held.account = Some(account);
        match &held.connection {
            Some(connection) => connection.retry_now.notify_one(),
            None => held.connection = Some(self.keep_connected(held)),
        }
        Ok(())
    }

    /// Keeps a connection to the Relay `held` names, trying again with
    /// backoff whenever it ends or cannot be made — even while the Relay
    /// refuses the Server's Login, since one fresh login elsewhere may
    /// restore it — and waiting on it to be reached while the Server Serves
    /// through that Relay.
    fn keep_connected(&self, held: &HeldRelay) -> KeptConnection {
        let address = held.stored.address.clone();
        let mut wish = WaitingWish {
            serve_through: held.serve_through.subscribe(),
            serving: self.serving.serving(),
        };
        let retry_now = Arc::new(Notify::new());
        let controller = self.clone();
        let retry = retry_now.clone();
        let task = tokio::spawn(async move {
            let RelayTimings {
                answer_timeout,
                retry_initial,
                retry_max,
                ..
            } = controller.timings;
            let mut backoff = retry_initial;
            loop {
                let waiting = wish.now();
                let rewished = match controller
                    .dialer
                    .open(&address, &controller.identity, answer_timeout)
                    .await
                {
                    Ok((conversation, Some(account))) => {
                        controller.observe(
                            &address,
                            Observed::LoggedIn(RelayAccount {
                                provider: account.provider,
                                username: account.username,
                            }),
                        );
                        backoff = retry_initial;
                        controller
                            .attend(&address, conversation, waiting, &mut wish)
                            .await
                    }
                    Ok((conversation, None)) => {
                        conversation.close().await;
                        controller.observe(&address, Observed::LoginNeeded);
                        false
                    }
                    Err(failure) => {
                        controller.observe(&address, Observed::Unreachable(failure.unreachable()));
                        false
                    }
                };
                // A connection ended because the Server now is, or is no
                // longer, to wait is opened again at once.
                if rewished {
                    continue;
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

    /// Keeps `conversation`, on which the Relay at `address` has taken the
    /// Server's proof and its Login stands, until it ends: waiting on it to
    /// be reached, where `waiting` says to, and taking up each join asked
    /// there, which ends with the waiting. Answers whether it ended because
    /// `wish` no longer agrees with `waiting`.
    async fn attend(
        &self,
        address: &str,
        mut conversation: Conversation,
        waiting: Option<Waited>,
        wish: &mut WaitingWish,
    ) -> bool {
        let RelayTimings {
            answer_timeout,
            heartbeat_interval,
            heartbeat_timeout,
            ..
        } = self.timings;
        if waiting.is_some()
            && let Err(observed) = conversation.wait(answer_timeout).await
        {
            conversation.close().await;
            self.observe(address, observed);
            return false;
        }
        let mut session =
            waiting.map(|waited| WaitingSession::new(waited, wish.serve_through.clone()));
        let refused = Notify::new();
        let mut heard_refusal = false;
        let ending = tokio::select! {
            ending = conversation.attend(heartbeat_interval, heartbeat_timeout, |heard| match heard {
                // Nothing the Relay says once it has refused the Login is
                // heard, however much of it arrives at once.
                _ if heard_refusal => {}
                RelayMessage::Reach { join } => {
                    if let Some(session) = session.as_mut() {
                        self.take_up(address, join.0, session);
                    }
                }
                // The Relay cuts the connection as it comes to refuse the
                // Login — its Account lapsing, say — and says so, so the
                // Login reads as needing renewal at once, not at the next
                // try.
                RelayMessage::Refused {
                    refusal: Refusal::LoginNeeded,
                    ..
                } => {
                    heard_refusal = true;
                    self.observe(address, Observed::LoginNeeded);
                    refused.notify_one();
                }
                _ => {}
            }) => ending,
            // A Relay that goes on talking after its refusal is let go, so it
            // is heard again only as the Server's backoff tries it again.
            () = refused.notified() => {
                drop(session);
                conversation.close().await;
                return false;
            }
            () = wish.departs_from(waiting) => {
                // The waiting ends before anything is awaited, so nothing
                // taken up during it is handed on while the connection
                // takes its time to close.
                drop(session);
                conversation.close().await;
                return true;
            }
        };
        drop(session);
        // A Relay that went away is tried again before it is called
        // Unreachable; one that fell silent already is.
        if ending == Ending::Silent {
            self.observe(
                address,
                Observed::Unreachable(RelayUnreachable {
                    behind: None,
                    message: "the Relay stopped answering".to_owned(),
                }),
            );
        }
        false
    }

    /// Takes up, on a connection of its own, the join the Relay at `address`
    /// named `join` as it told the Server of it during `session`, unless as
    /// many as may be are being taken up in it already.
    fn take_up(&self, address: &str, join: Vec<u8>, session: &mut WaitingSession) {
        while session.take_ups.try_join_next().is_some() {}
        if session.take_ups.len() >= TAKE_UPS_AT_ONCE {
            tracing::debug!("a Relay asked more joins of this Server than it takes up at once");
            return;
        }
        let controller = self.clone();
        let address = address.to_owned();
        let hand_off = session.hand_off();
        session.take_ups.spawn(async move {
            controller.accept_join(&address, join, hand_off).await;
        });
    }

    /// Opens a connection to the Relay at `address`, proving the Server's
    /// key, takes up the join named `join` on it, and hands what the join
    /// then carries to the Serving side, whose acceptor judges it as it
    /// does a connection dialled to its listener — where `hand_off` still
    /// stands once the join is made.
    async fn accept_join(&self, address: &str, join: Vec<u8>, hand_off: HandOff) {
        let answer_timeout = self.timings.answer_timeout;
        let Ok((mut conversation, _)) = self
            .dialer
            .open(address, &self.identity, answer_timeout)
            .await
        else {
            tracing::debug!("a join a Relay asked of this Server could not be taken up");
            return;
        };
        if conversation
            .say(&ServerMessage::Accept { join: Bytes(join) })
            .await
            .is_err()
        {
            return;
        }
        match tokio::time::timeout(answer_timeout, conversation.hear()).await {
            Ok(Some(RelayMessage::Joined)) => {
                // Judged and handed on while the Relays are held, so no change
                // to the choice to Serve through the Relay comes between.
                let _relays = self.lock();
                if hand_off.stands() {
                    let carried = conversation.carried(self.carrying.stop());
                    self.serving
                        .accept_carried(carried, hand_off.waited.stretch);
                } else {
                    tracing::debug!(
                        "a join was made through a Relay this Server no longer waits at"
                    );
                }
            }
            _ => tracing::debug!("a Relay did not make the join this Server took up"),
        }
    }

    /// Records how the Relay at `address` now stands. Whether its Login there
    /// needs renewing is kept apart from whether the Relay answers, and
    /// stored, so a Relay found to refuse the Login reads **login needed** —
    /// and is offered by no Invite — however it fails to answer afterwards,
    /// across a restart too, until it takes the Server's proof with a Login
    /// that stands again.
    fn observe(&self, address: &str, observed: Observed) {
        let mut relays = self.lock();
        let Some(index) = relays
            .iter()
            .position(|held| held.stored.address == address)
        else {
            return;
        };
        let login_needed = match &observed {
            Observed::LoggedIn(_) => false,
            Observed::LoginNeeded => true,
            Observed::Unreachable(_) => relays[index].stored.login_needed,
        };
        // A Login that stood and is now refused is a lapse a Client raises
        // its Notice of — not a Relay merely Unreachable, nor one never
        // logged in at — once, until the Login stands again and a later
        // refusal makes another.
        let stored = &relays[index].stored;
        let untold_lapse = match &observed {
            Observed::LoggedIn(_) => None,
            Observed::LoginNeeded if stored.logged_in && !stored.login_needed => {
                Some(Uuid::new_v4())
            }
            Observed::LoginNeeded | Observed::Unreachable(_) => stored.untold_lapse,
        };
        let changed = stored.login_needed != login_needed || stored.untold_lapse != untold_lapse;
        relays[index].stored.login_needed = login_needed;
        relays[index].stored.untold_lapse = untold_lapse;
        // What the Relay last said governs this run whether or not it can be
        // stored. Where it cannot, it is stored as the Server next hears from
        // a Relay — as one that refuses the Login goes on refusing it each
        // time the Server tries it — until it is, so a Login refused is never
        // taken to stand across a restart.
        if (changed || self.unstored.load(Ordering::Acquire))
            && let Err(error) = self.persist(&relays)
        {
            self.unstored.store(true, Ordering::Release);
            tracing::warn!("could not store whether a Relay needs a login: {error:#}");
        }
        let held = &mut relays[index];
        match observed {
            Observed::LoggedIn(account) => {
                held.state = RelayState::LoggedIn;
                held.unreachable = None;
                held.account = Some(account);
            }
            Observed::LoginNeeded => held.refused(),
            // A Login known to need renewing still does while its Relay
            // cannot be spoken to, which only a login can change; any other
            // stands meanwhile.
            Observed::Unreachable(_) if login_needed => {}
            Observed::Unreachable(why) => {
                held.state = RelayState::Unreachable;
                held.unreachable = Some(why);
            }
        }
        self.publish(&relays);
    }

    /// What serialises beginning a login and removal at the Relay at
    /// `address`, where the Server holds an entry for it.
    fn operations(&self, address: &str) -> std::result::Result<Arc<AsyncMutex<()>>, RelayFailure> {
        self.lock()
            .iter()
            .find(|held| held.stored.address == address)
            .map(|held| held.operations.clone())
            .ok_or_else(relay_not_found)
    }

    /// Publishes `relays`, which the caller holds, at the next revision,
    /// where they differ from what was last published; and has Serving tell
    /// the Server's Peers which of them it Serves through, where that
    /// differs from what they were last told.
    fn publish(&self, relays: &[HeldRelay]) {
        self.serving.offer_relays(told(relays));
        let relays = relays.iter().map(HeldRelay::relay).collect::<Vec<_>>();
        self.published.send_if_modified(|listing| {
            if listing.relays == relays {
                return false;
            }
            listing.revision += 1;
            listing.relays = relays;
            true
        });
    }

    fn persist(&self, relays: &[HeldRelay]) -> Result<()> {
        self.write(
            &relays
                .iter()
                .map(|held| held.stored.clone())
                .collect::<Vec<_>>(),
        )
    }

    /// Stores `stored` as the Server's Relays, replacing what was stored
    /// whole or not at all. What is stored is all the Server holds of them,
    /// so once it is, nothing it holds is yet to be stored.
    fn write(&self, stored: &[StoredRelay]) -> Result<()> {
        let mut contents = serde_json::to_vec(stored)?;
        contents.push(b'\n');
        replace_private_file(&self.data_dir.join(RELAYS_FILE), &contents)?;
        self.unstored.store(false, Ordering::Release);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<HeldRelay>> {
        self.relays
            .lock()
            .expect("Relay record lock is not poisoned")
    }
}

/// What reaches the Serving Servers this Server is paired with through its
/// Relays, as Serving and its Pairings ask: the Relays it Serves through, and
/// the joins it asks at those it holds a Login at. It holds the Server's
/// Relays and never Serving itself, which holds it.
#[derive(Clone)]
struct Joining {
    relays: Arc<StdMutex<Vec<HeldRelay>>>,
    identity: IdentityKey,
    dialer: Dialer,
    timings: RelayTimings,
    carrying: Arc<Carrying>,
}

impl RelayWays for Joining {
    fn served_through(&self, relay: &str) -> Option<String> {
        let address = relay_protocol::canonical_address(relay)?;
        self.lock()
            .iter()
            .any(|held| held.stored.address == address && held.offered())
            .then_some(address)
    }

    fn chosen(&self, relay: &str) -> bool {
        self.lock().iter().any(|held| held.stored.address == relay)
    }

    fn join(&self, relay: String, server: Vec<u8>, wanted: Wanted) -> RelayJoin {
        let joining = self.clone();
        Box::pin(async move {
            let carried = joining.join(&relay, server, &wanted).await?;
            Ok(Box::new(carried) as Box<dyn ByteStream>)
        })
    }
}

impl Joining {
    /// Joins this Server, at the Relay at `relay`, to the Serving Server whose
    /// identity key is `server`, proving the Server's key there, each step
    /// within the answer timeout: what the join then carries. The Server must
    /// hold a Login at the Relay as it asks, and still hold it as the join is
    /// made, or nothing is carried; and the join is asked only while the
    /// connection is still `wanted`.
    async fn join(
        &self,
        relay: &str,
        server: Vec<u8>,
        wanted: &Wanted,
    ) -> std::io::Result<CarriedStream> {
        let Some(entry) = self.login_at(relay, None) else {
            return Err(no_login(relay));
        };
        let (mut conversation, login) = self
            .dialer
            .open(relay, &self.identity, self.timings.answer_timeout)
            .await
            .map_err(|failure| std::io::Error::other(failure.into_failure().message))?;
        let refused = match login {
            None => Some(login_refused(relay)),
            // Let go of while the Relay took the proof, however soon it
            // answered: no join is asked for a connection nothing wants.
            Some(_) if !wanted.still() => Some(no_longer_wanted()),
            Some(account) => {
                if conversation
                    .say(&ServerMessage::Join {
                        server: Bytes(server),
                    })
                    .await
                    .is_err()
                {
                    return Err(std::io::Error::other("the Relay stopped answering"));
                }
                match tokio::time::timeout(self.timings.answer_timeout, conversation.hear()).await {
                    Ok(Some(RelayMessage::Joined)) => None,
                    Ok(Some(RelayMessage::Refused {
                        refusal: Refusal::LoginNeeded,
                        ..
                    })) => Some(login_refused(relay)),
                    Ok(Some(RelayMessage::Refused {
                        refusal: Refusal::DifferentAccounts,
                        ..
                    })) => Some(different_accounts(relay, &account)),
                    Ok(Some(RelayMessage::Refused {
                        refusal: Refusal::CapReached { cap, limit },
                        message,
                    })) => Some(cap_reached(relay, cap, limit, &message)),
                    Ok(Some(RelayMessage::Refused { message, .. })) => {
                        Some(std::io::Error::other(message))
                    }
                    Ok(Some(_)) => Some(std::io::Error::other(
                        "the Relay answered a join with something else",
                    )),
                    Ok(None) | Err(_) => Some(std::io::Error::other("the Relay stopped answering")),
                }
            }
        };
        if let Some(refused) = refused {
            conversation.close().await;
            return Err(refused);
        }
        // A Login given up as the join was made — the Relay removed, however
        // soon it was added again — has the join carry nothing.
        if self.login_at(relay, Some(&entry)).is_none() {
            return Err(no_login(relay));
        }
        Ok(conversation.carried(self.carrying.stop()))
    }

    /// What serialises what is asked of the entry for the Relay at `relay`,
    /// where the Server has logged in there — and, where `entry` is given,
    /// where that entry is the one it serialises.
    fn login_at(
        &self,
        relay: &str,
        entry: Option<&Arc<AsyncMutex<()>>>,
    ) -> Option<Arc<AsyncMutex<()>>> {
        self.lock()
            .iter()
            .find(|held| {
                held.stored.address == relay
                    && held.stored.logged_in
                    && entry.is_none_or(|entry| Arc::ptr_eq(&held.operations, entry))
            })
            .map(|held| held.operations.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<HeldRelay>> {
        self.relays
            .lock()
            .expect("Relay record lock is not poisoned")
    }
}

/// The refusal of a join at the Relay at `relay`, where the Server has not
/// logged in.
fn no_login(relay: &str) -> std::io::Error {
    std::io::Error::other(RelayRefusal {
        code: SessionErrorCode::RelayLoginNeeded,
        message: format!(
            "this Server holds no Login at the Relay at {relay}; log in there, then try again"
        ),
        unreachable: Some(UnreachableReason::RelayLoginNeeded {
            relay: relay.to_owned(),
        }),
    })
}

/// The refusal of a join at the Relay at `relay`, which refuses the Login the
/// Server holds there.
fn login_refused(relay: &str) -> std::io::Error {
    std::io::Error::other(RelayRefusal {
        code: SessionErrorCode::RelayLoginNeeded,
        message: format!(
            "the Relay at {relay} no longer admits this Server's Login there; log in there \
             again, then try again"
        ),
        unreachable: Some(UnreachableReason::RelayLoginNeeded {
            relay: relay.to_owned(),
        }),
    })
}

/// The refusal of a join at the Relay at `relay`, where the Server's Login
/// stands under `account` and the Serving Server's under another.
fn different_accounts(relay: &str, account: &relay_protocol::Account) -> std::io::Error {
    let relay_protocol::Account { provider, username } = account;
    std::io::Error::other(RelayRefusal {
        code: SessionErrorCode::RelayDifferentAccounts,
        message: format!(
            "this Server is logged in at the Relay at {relay} as {username} ({provider}), and \
             the Server it would reach there under another Account; a Relay joins only Servers \
             logged in under the same one, so log this Server in there as the user that Server \
             is logged in as, or pair the two directly"
        ),
        unreachable: Some(UnreachableReason::RelayDifferentAccounts {
            relay: relay.to_owned(),
            account: RelayAccount {
                provider: provider.clone(),
                username: username.clone(),
            },
        }),
    })
}

/// What a login at the Relay at `address` is told where the Account it would
/// stand under already has as many Servers logged in there as `limit`.
fn logins_capped(address: &str, limit: u64) -> String {
    let servers = if limit == 1 { "Server" } else { "Servers" };
    format!(
        "the Relay at {address} allows {limit} {servers} logged in under one Account, and \
         yours has as many; remove the Relay from a Server of yours that no longer needs it, \
         or ask the Relay's operator to remove a Login or to raise the cap"
    )
}

/// The refusal of a join at the Relay at `relay`, where the Account the
/// Server's Login stands under there has reached `cap`, which allows `limit`,
/// as the Relay says in `message`: named first, ahead of where, so whatever
/// shows only the start of it still says which cap.
fn cap_reached(relay: &str, cap: Cap, limit: u64, message: &str) -> std::io::Error {
    let (message, unreachable) = match cap {
        Cap::JoinedConnections => {
            let connections = if limit == 1 {
                "connection"
            } else {
                "connections"
            };
            (
                format!(
                    "this Server's Account has reached the cap of {limit} {connections} joined \
                     at once that the Relay at {relay} sets for each Account, so it joins no \
                     more until one ends; if that is too few, ask the Relay's operator to raise \
                     the cap"
                ),
                Some(UnreachableReason::RelayCapReached {
                    relay: relay.to_owned(),
                    limit: u32::try_from(limit).unwrap_or(u32::MAX),
                }),
            )
        }
        Cap::Logins | Cap::Unrecognized => (
            format!(
                "the Relay at {relay} joins no more for this Server's Account, which has reached \
                 a cap its operator sets: {message}"
            ),
            None,
        ),
    };
    std::io::Error::other(RelayRefusal {
        code: SessionErrorCode::RelayCapReached,
        message,
        unreachable,
    })
}

/// What the Server found of a Relay as it tried to speak to it.
enum Observed {
    LoggedIn(RelayAccount),
    LoginNeeded,
    Unreachable(RelayUnreachable),
}

/// The user's choice to Serve through a Relay, and which making of that
/// choice it is: each change makes another, so what began under one is told
/// apart from what begins under the next, however soon the choice returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ServeThrough {
    chosen: bool,
    making: u64,
}

impl ServeThrough {
    fn first(chosen: bool) -> Self {
        Self { chosen, making: 0 }
    }

    fn made_again(self, chosen: bool) -> Self {
        Self {
            chosen,
            making: self.making + 1,
        }
    }
}

/// The waiting the Server does at a Relay: under one making of its user's
/// choice to Serve through it, in one stretch of Serving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Waited {
    making: u64,
    stretch: u64,
}

/// One stretch of the Server waiting at a Relay on one connection: the joins
/// it takes up there, which end with it, and what says it still stands.
struct WaitingSession {
    take_ups: JoinSet<()>,
    /// Lowered as the waiting ends, so a join taken up during it that has
    /// yet to be handed on is handed on to nothing.
    standing: Arc<AtomicBool>,
    waited: Waited,
    serve_through: watch::Receiver<ServeThrough>,
}

impl WaitingSession {
    fn new(waited: Waited, serve_through: watch::Receiver<ServeThrough>) -> Self {
        Self {
            take_ups: JoinSet::new(),
            standing: Arc::new(AtomicBool::new(true)),
            waited,
            serve_through,
        }
    }

    fn hand_off(&self) -> HandOff {
        HandOff {
            session: self.standing.clone(),
            waited: self.waited,
            serve_through: self.serve_through.clone(),
        }
    }
}

impl Drop for WaitingSession {
    fn drop(&mut self) {
        self.standing.store(false, Ordering::Release);
    }
}

/// What a join taken up is handed to the Serving side under: the waiting it
/// was taken up during.
struct HandOff {
    session: Arc<AtomicBool>,
    waited: Waited,
    serve_through: watch::Receiver<ServeThrough>,
}

impl HandOff {
    /// Whether that waiting still stands: it has not ended, and the choice
    /// to Serve through the Relay it was made under is still the choice,
    /// never since turned off however soon it was turned on again. Serving
    /// is judged on its own as the join is handed on.
    fn stands(&self) -> bool {
        let choice = *self.serve_through.borrow();
        self.session.load(Ordering::Acquire) && choice.chosen && choice.making == self.waited.making
    }
}

/// What decides whether the Server waits at one of its Relays to be reached:
/// its user's choice to Serve through that Relay, and its Serving at all.
struct WaitingWish {
    serve_through: watch::Receiver<ServeThrough>,
    serving: watch::Receiver<ServingStretch>,
}

impl WaitingWish {
    /// The waiting the Server is to do at the Relay now, if any.
    fn now(&mut self) -> Option<Waited> {
        let choice = *self.serve_through.borrow_and_update();
        let serving = *self.serving.borrow_and_update();
        (choice.chosen && serving.serving).then_some(Waited {
            making: choice.making,
            stretch: serving.number,
        })
    }

    /// Returns once the waiting the Server is to do at the Relay is no
    /// longer `waiting`: none where there was some, some where there was
    /// none, or another — the choice turned off and on again, or Serving
    /// stopped and started again, however soon.
    async fn departs_from(&mut self, waiting: Option<Waited>) {
        while self.now() == waiting {
            // Each is said by what outlives the connection kept to the Relay
            // — its entry, and the Serving side — so neither ends while it
            // is kept.
            tokio::select! {
                changed = self.serve_through.changed() => if changed.is_err() {
                    std::future::pending::<()>().await;
                },
                changed = self.serving.changed() => if changed.is_err() {
                    std::future::pending::<()>().await;
                },
            }
        }
    }
}

/// The addresses of the Relays among `relays` the Server tells its Peers it
/// Serves through.
fn told(relays: &[HeldRelay]) -> Vec<String> {
    relays
        .iter()
        .filter(|held| held.told())
        .map(|held| held.stored.address.clone())
        .collect()
}

impl HeldRelay {
    /// Whether an Invite may offer the Relay: where its user has chosen that
    /// the Server Serves through it and its Login there stands. A Login the
    /// Relay was last found to refuse is offered to nobody, however the
    /// Relay has answered since, though the Server goes on connecting there
    /// in case it is restored; one merely Unreachable still is, as the Server
    /// goes on waiting there.
    fn offered(&self) -> bool {
        self.told() && !self.stored.login_needed
    }

    /// Whether the Server tells its Peers it Serves through the Relay: where
    /// its user has chosen that it does, and it has logged in there. A Login
    /// the Relay comes to refuse withdraws nothing, since one fresh login
    /// from any Server of its Account restores it, and the Peers that rely
    /// on the Relay then recover through it on their own; only its user's
    /// choice withdraws it — no longer to Serve through it, or to remove it.
    fn told(&self) -> bool {
        self.stored.serve_through && self.stored.logged_in
    }

    fn relay(&self) -> Relay {
        Relay {
            address: self.stored.address.clone(),
            state: self.state,
            unreachable: self.unreachable.clone(),
            account: self.account.clone(),
            login: self
                .login
                .as_ref()
                .map(|login| login.progress.borrow().clone()),
            serve_through: self.stored.serve_through,
            login_needed_notice: self.stored.untold_lapse,
        }
    }

    /// Holds that the Relay refuses the Server's Login there, which needs
    /// renewing from now on, whatever the Relay answers meanwhile.
    fn refused(&mut self) {
        self.stored.login_needed = true;
        self.state = RelayState::LoginNeeded;
        self.unreachable = None;
        self.account = None;
    }
}

/// The entry at `address` that `operations` serialises. An entry removed and
/// added again at the same address is another, with operations of its own,
/// so what was asked of the one before finds no entry here.
fn owning<'relays>(
    relays: &'relays mut [HeldRelay],
    address: &str,
    operations: &Arc<AsyncMutex<()>>,
) -> Option<&'relays mut HeldRelay> {
    relays
        .iter_mut()
        .find(|held| held.stored.address == address && Arc::ptr_eq(&held.operations, operations))
}

/// Ends the connection kept to `held` and any login under way there.
fn stop(held: &mut HeldRelay) {
    if let Some(connection) = held.connection.take() {
        connection.task.abort();
    }
    if let Some(task) = held.login.as_mut().and_then(|login| login.task.take()) {
        task.abort();
    }
}

/// Says a login still pending was given up, and why, so whatever follows it
/// learns it has ended.
fn settle_abandoned(progress: &watch::Sender<RelayLogin>, message: &str) {
    progress.send_if_modified(|login| {
        if login.outcome.is_settled() {
            return false;
        }
        login.outcome = RelayLoginOutcome::Refused {
            reason: RelayLoginRefusal::Interrupted,
            message: message.to_owned(),
        };
        true
    });
}

fn stopped_answering() -> RelayFailure {
    RelayFailure::new(
        SessionErrorCode::RelayUnreachable,
        "the Relay stopped answering",
    )
}

/// The address `address` names a Relay by, written the one way a Relay's
/// address is (see [`relay_protocol::canonical_address`]).
fn relay_address(address: &str) -> std::result::Result<String, RelayFailure> {
    let address = relay_protocol::canonical_address(address).ok_or_else(|| {
        RelayFailure::new(
            SessionErrorCode::InvalidRelayAddress,
            "a Relay's address is an https:// or http:// address naming its host",
        )
    })?;
    // A Relay the Server Serves through is told to its Peers by its address,
    // whole.
    if address.len() > MAX_TOLD_RELAY_LEN {
        return Err(RelayFailure::new(
            SessionErrorCode::InvalidRelayAddress,
            format!("a Relay's address runs to at most {MAX_TOLD_RELAY_LEN} characters"),
        ));
    }
    Ok(address)
}

/// Opens connections to Relays: WebSockets over HTTP or HTTPS, taken through
/// the system's HTTP proxy and verified against the operating system's trust
/// store. Its HTTP clients are made at their first dial, so a Server holding
/// no Relay never reads the trust store.
#[derive(Clone, Default)]
struct Dialer {
    /// Reaches a Relay elsewhere, through the system's HTTP proxy.
    proxied: Arc<OnceLock<reqwest::Client>>,
    /// Reaches a Relay on this machine's loopback, which no proxy elsewhere
    /// could reach.
    direct: Arc<OnceLock<reqwest::Client>>,
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
    /// Why the Relay reads Unreachable, failing so.
    fn unreachable(self) -> RelayUnreachable {
        match self {
            Self::Unreachable(message) | Self::Refused(message) => RelayUnreachable {
                behind: None,
                message,
            },
            Self::Mismatch { behind, message } => RelayUnreachable {
                behind: Some(match behind {
                    Side::Server => RelaySide::Server,
                    Side::Relay => RelaySide::Relay,
                }),
                message,
            },
        }
    }

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
    /// The HTTP client the Relay at `address` is reached with.
    fn http(&self, address: &str) -> &reqwest::Client {
        if on_loopback(address) {
            self.direct.get_or_init(|| relay_http_client(false))
        } else {
            self.proxied.get_or_init(|| relay_http_client(true))
        }
    }

    /// Opens a connection to the Relay at `address` and proves the Server's
    /// identity key there, for that Relay alone, each step within
    /// `answer_timeout`: what the Relay then says of the Server's Login — the
    /// Account it stands under, where it stands. Nothing is signed for a Relay
    /// that chose a version the Server did not offer, or that names itself by
    /// another address than the Server knows it at.
    async fn open(
        &self,
        address: &str,
        identity: &IdentityKey,
        answer_timeout: Duration,
    ) -> std::result::Result<(Conversation, Option<relay_protocol::Account>), DialFailure> {
        let unanswered = || DialFailure::Unreachable("the Relay did not answer in time".to_owned());
        let identity_failure =
            |_| DialFailure::Refused("the Server could not use its identity key".to_owned());
        let key = identity.public_key().map_err(identity_failure)?;
        let mut conversation =
            tokio::time::timeout(answer_timeout, self.websocket(address, answer_timeout))
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
            Ok(Some(RelayMessage::Challenge {
                version,
                nonce,
                relay,
            })) => {
                if !SPOKEN.contains(&version) {
                    let behind = if SPOKEN.iter().max().is_some_and(|ours| version > *ours) {
                        Side::Server
                    } else {
                        Side::Relay
                    };
                    return Err(DialFailure::Mismatch {
                        behind,
                        message: mismatch(&[version], behind),
                    });
                }
                if relay_protocol::canonical_address(&relay).as_deref() != Some(address) {
                    return Err(DialFailure::Refused(format!(
                        "the Relay at {address} names itself {relay}, so this Server proves \
                         nothing to it; add the Relay by the address it names"
                    )));
                }
                nonce.0
            }
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
        let signature = identity
            .sign(&relay_protocol::proof_message(address, &nonce, &key))
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
    async fn websocket(
        &self,
        address: &str,
        send_timeout: Duration,
    ) -> std::result::Result<Conversation, DialFailure> {
        let key = tungstenite::handshake::client::generate_key();
        let response = self
            .http(address)
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
        // What the Relay sends and what it has yet to take in are both held to
        // a bound, so one that pings without reading, or sends without end,
        // costs the Server no more than that.
        let limits = WebSocketConfig::default()
            .write_buffer_size(0)
            .max_write_buffer_size(2 * MAX_MESSAGE_LEN)
            .max_message_size(Some(MAX_MESSAGE_LEN))
            .max_frame_size(Some(MAX_MESSAGE_LEN));
        Ok(Conversation {
            socket: WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(limits)).await,
            send_timeout,
        })
    }
}

/// Whether the Relay at `address` is on this machine's loopback.
fn on_loopback(address: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(address) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The HTTP client a Server reaches its Relays with: through the system's HTTP
/// proxy, as reqwest finds it, where `proxied`, and trusting what the operating
/// system's trust store trusts. A trust store that cannot be read trusts
/// nothing, so every HTTPS Relay is refused rather than the Server failing.
fn relay_http_client(proxied: bool) -> reqwest::Client {
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
    // A Relay that vanished, with whatever join it carried, is found out as a
    // direct way's Serving Server is.
    // A Relay is known by its address, so one answering with a redirect is
    // not followed anywhere else: where it points is nowhere its user chose,
    // and is asked nothing. A Relay that moves is added again where it went.
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .tcp_keepalive(SOCKET_KEEPALIVE)
        .tcp_keepalive_interval(SOCKET_KEEPALIVE)
        .tcp_keepalive_retries(SOCKET_KEEPALIVE_PROBES);
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    let client = client.tcp_user_timeout(SOCKET_USER_TIMEOUT);
    if proxied { client } else { client.no_proxy() }
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

/// How a connection to a Relay ended.
#[derive(Debug, Eq, PartialEq)]
enum Ending {
    /// It closed.
    Closed,
    /// The Relay stopped answering while it stood open.
    Silent,
}

/// A WebSocket to a Relay, carrying one JSON message to a text frame.
struct Conversation {
    socket: WebSocketStream<reqwest::Upgraded>,
    /// How long the Relay may take to take in what is sent it.
    send_timeout: Duration,
}

impl Conversation {
    /// Sends `message` once whatever is queued ahead of it — answers to the
    /// Relay's pings among them — has gone, so it fits however much was, all
    /// within the send timeout.
    async fn say(&mut self, message: &ServerMessage) -> std::result::Result<(), ()> {
        let text = serde_json::to_string(message).expect("a Server message always encodes");
        let said = tokio::time::timeout(self.send_timeout, async {
            self.socket.flush().await?;
            self.socket.send(Message::Text(text.into())).await
        });
        match said.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(_) => Err(()),
        }
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

    /// Asks the Relay to have the Server wait on this connection to be
    /// reached: where the Relay does not take that up within
    /// `answer_timeout`, how it then stands for the Server.
    async fn wait(&mut self, answer_timeout: Duration) -> std::result::Result<(), Observed> {
        let unreachable = |message: &str| {
            Observed::Unreachable(RelayUnreachable {
                behind: None,
                message: message.to_owned(),
            })
        };
        if self.say(&ServerMessage::Wait).await.is_err() {
            return Err(unreachable("the Relay stopped answering"));
        }
        match tokio::time::timeout(answer_timeout, self.hear()).await {
            Ok(Some(RelayMessage::Waiting)) => Ok(()),
            Ok(Some(RelayMessage::Refused {
                refusal: Refusal::LoginNeeded,
                ..
            })) => Err(Observed::LoginNeeded),
            Ok(Some(RelayMessage::Refused { message, .. })) => Err(unreachable(&message)),
            Ok(Some(_)) => Err(unreachable(
                "the Relay answered the Server's waiting with something else",
            )),
            Ok(None) | Err(_) => Err(unreachable("the Relay stopped answering")),
        }
    }

    /// Waits for the connection to end, handing `heard` each thing the Relay
    /// says that this Server recognizes and asking every `interval` that the
    /// Relay answer within `timeout`: how it ended. Only the Relay echoing
    /// what this Server asked shows it answers — it cannot without reading —
    /// so a Relay that keeps talking and never reads is found silent all the
    /// same.
    async fn attend(
        &mut self,
        interval: Duration,
        timeout: Duration,
        mut heard: impl FnMut(RelayMessage),
    ) -> Ending {
        let mut ask_at = tokio::time::Instant::now() + interval;
        let mut asked: Option<(Vec<u8>, tokio::time::Instant)> = None;
        loop {
            let wake = asked.as_ref().map_or(ask_at, |(_, answer_by)| *answer_by);
            tokio::select! {
                frame = self.socket.next() => match frame {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Ending::Closed,
                    Some(Ok(Message::Pong(echo)))
                        if asked.as_ref().is_some_and(|(asking, _)| echo[..] == asking[..]) =>
                    {
                        asked = None;
                        ask_at = tokio::time::Instant::now() + interval;
                    }
                    Some(Ok(Message::Text(text))) => match serde_json::from_str(text.as_str()) {
                        Ok(RelayMessage::Unrecognized) | Err(_) => {}
                        Ok(message) => heard(message),
                    },
                    Some(Ok(_)) => {}
                },
                () = tokio::time::sleep_until(wake) => {
                    if asked.is_some() {
                        return Ending::Silent;
                    }
                    let asking = uuid::Uuid::new_v4().as_bytes().to_vec();
                    let ping = Message::Ping(asking.clone().into());
                    let sent = tokio::time::timeout(timeout, async {
                        self.socket.flush().await?;
                        self.socket.send(ping).await
                    });
                    if !matches!(sent.await, Ok(Ok(()))) {
                        return Ending::Silent;
                    }
                    asked = Some((asking, tokio::time::Instant::now() + timeout));
                }
            }
        }
    }

    /// Ends the conversation and hears the Relay out until it lets it go, so
    /// whatever the Relay was doing for it is done, all within `timeout`.
    async fn end(mut self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, async {
            let _ = self.socket.close(None).await;
            while self.hear().await.is_some() {}
        })
        .await;
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(self.send_timeout, self.socket.close(None)).await;
    }

    /// The join this conversation carries, once the Relay has made it, ended
    /// by `stop`.
    fn carried(self, stop: Arc<Stop>) -> CarriedStream {
        CarriedStream::new(self.socket, stop)
    }
}

/// The joins Relays carry for this Server, each ended as the Server stops.
#[derive(Default)]
struct Carrying {
    carried: StdMutex<Carried>,
}

#[derive(Default)]
struct Carried {
    stopped: bool,
    stops: Vec<Weak<Stop>>,
}

impl Carrying {
    /// What ends one more join as the Server stops — at once, where it
    /// already has.
    fn stop(&self) -> Arc<Stop> {
        let stop = Arc::new(Stop::default());
        let mut carried = self
            .carried
            .lock()
            .expect("carried join lock is not poisoned");
        if carried.stopped {
            stop.stop();
        } else {
            carried.stops.retain(|stop| stop.strong_count() > 0);
            carried.stops.push(Arc::downgrade(&stop));
        }
        stop
    }

    /// Ends every join carried, and every one carried from now on.
    fn stop_all(&self) {
        let stops = {
            let mut carried = self
                .carried
                .lock()
                .expect("carried join lock is not poisoned");
            carried.stopped = true;
            std::mem::take(&mut carried.stops)
        };
        for stop in stops.iter().filter_map(Weak::upgrade) {
            stop.stop();
        }
    }
}

/// Says one join a Relay carries is to end, waking whatever reads or writes
/// it.
#[derive(Default)]
struct Stop {
    stopped: AtomicBool,
    waker: AtomicWaker,
}

impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn poll(&self, context: &TaskContext<'_>) -> bool {
        self.waker.register(context.waker());
        self.stopped.load(Ordering::Acquire)
    }
}

/// A join a Relay carries, as the byte stream the Pairing's pinned-key TLS
/// runs over: what is written goes to the Relay in binary frames of at most
/// [`MAX_MESSAGE_LEN`] bytes, and the bytes of each binary frame the Relay
/// carries back are read as they come, no more than one frame held unread at
/// once. The Relay closing the join or saying anything else ends it, and so
/// does the Server stopping.
///
/// Whether the Relay, and the Server it joins this one to, still answer is
/// not judged here: what a Relay echoes of a join shows only that it read
/// this far of what this Server sent, which waits on its passing on all
/// before, however healthy it is. The two Servers judge it end to end, inside
/// their pinned-key TLS, as [`crate::serving::JoinedKeepalive`] says — until
/// that is under way, their TLS handshake and then HTTP/2's start are each
/// bounded by the handshake timeout — and either drops the join, whatever it
/// was still sending, once it has stalled past that keepalive's verdict.
struct CarriedStream<Socket = reqwest::Upgraded> {
    socket: WebSocketStream<Socket>,
    /// What is left unread of the latest frame the Relay carried.
    unread: tungstenite::Bytes,
    /// Whether the Relay has closed the join.
    ended: bool,
    stop: Arc<Stop>,
}

impl<Socket: AsyncRead + AsyncWrite + Unpin> CarriedStream<Socket> {
    /// The join `socket` carries, ended by `stop`.
    fn new(socket: WebSocketStream<Socket>, stop: Arc<Stop>) -> Self {
        Self {
            socket,
            unread: tungstenite::Bytes::new(),
            ended: false,
            stop,
        }
    }

    /// Takes in what the Relay has carried until bytes are held unread:
    /// `true` where they are, `false` once the join has ended, and pending
    /// where nothing more has come yet. No more than one frame is held at
    /// once.
    fn poll_take_in(&mut self, context: &mut TaskContext<'_>) -> Poll<std::io::Result<bool>> {
        loop {
            if !self.unread.is_empty() {
                return Poll::Ready(Ok(true));
            }
            if self.ended {
                return Poll::Ready(Ok(false));
            }
            match ready!(self.socket.poll_next_unpin(context)) {
                Some(Ok(Message::Binary(bytes))) => self.unread = bytes,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => self.ended = true,
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "the Relay said something that is no part of the join it carries",
                    )));
                }
                Some(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
            }
        }
    }

    /// An error where the Server is stopping, which ends every join.
    fn stopping(&self, context: &TaskContext<'_>) -> std::io::Result<()> {
        if self.stop.poll(context) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "the Server is stopping",
            ));
        }
        Ok(())
    }
}

impl<Socket: AsyncRead + AsyncWrite + Unpin> AsyncRead for CarriedStream<Socket> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        this.stopping(context)?;
        if ready!(this.poll_take_in(context))? {
            let length = this.unread.len().min(buffer.remaining());
            let read = this.unread.split_to(length);
            buffer.put_slice(&read);
        }
        Poll::Ready(Ok(()))
    }
}

impl<Socket: AsyncRead + AsyncWrite + Unpin> AsyncWrite for CarriedStream<Socket> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = &mut *self;
        this.stopping(context)?;
        ready!(Pin::new(&mut this.socket).poll_ready(context)).map_err(std::io::Error::other)?;
        let length = buffer.len().min(MAX_MESSAGE_LEN);
        let frame = Message::Binary(tungstenite::Bytes::copy_from_slice(&buffer[..length]));
        Pin::new(&mut this.socket)
            .start_send(frame)
            .map_err(std::io::Error::other)?;
        Poll::Ready(Ok(length))
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        this.stopping(context)?;
        Pin::new(&mut this.socket)
            .poll_flush(context)
            .map_err(std::io::Error::other)
    }

    /// Closes the join, waiting on none once the Server stops.
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.stop.poll(context) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.socket)
            .poll_close(context)
            .map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(directory: &Path) -> RelayController {
        controller_answering_within(directory, Duration::from_secs(1))
    }

    /// The same, waiting `answer_timeout` for each answer a Relay owes.
    fn controller_answering_within(directory: &Path, answer_timeout: Duration) -> RelayController {
        let serving = ServingController::new(
            directory,
            Duration::from_secs(60),
            crate::protocol::PROTOCOL_VERSION,
            "http://127.0.0.1:1".to_owned(),
            "token".to_owned(),
        )
        .unwrap();
        RelayController::new(
            directory,
            serving,
            RelayTimings {
                answer_timeout,
                retry_initial: Duration::from_millis(5),
                retry_max: Duration::from_millis(25),
                heartbeat_interval: Duration::from_secs(30),
                heartbeat_timeout: Duration::from_secs(10),
            },
            Uuid::new_v4(),
        )
        .unwrap()
    }

    /// An address at which nothing listens, so whatever is asked of a Relay
    /// there fails at once.
    fn unanswered_address() -> String {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    /// A removal or a login queued behind a removal holds the operations of
    /// the entry it was asked of. Where that entry has gone and its address
    /// has been added again by the time it runs, it is of no Relay the
    /// Server holds: the entry added again is another, and is left alone.
    #[tokio::test]
    async fn what_was_asked_of_a_relay_removed_and_added_again_leaves_the_new_entry_alone() {
        let directory = tempfile::tempdir().unwrap();
        let relays = controller(directory.path());
        let address = unanswered_address();
        relays.add(&address).unwrap();
        let removed_entry = relays.operations(&address).unwrap();
        relays.remove(&address).await.expect("remove the Relay");
        relays.add(&address).expect("add the Relay again");

        let stale_removal = relays
            .remove_holding(address.clone(), removed_entry.clone())
            .await;
        assert_eq!(
            stale_removal.err().map(|failure| failure.code),
            Some(SessionErrorCode::RelayNotFound)
        );
        let stale_login = relays
            .begin_login_holding(address.clone(), removed_entry)
            .await;
        assert_eq!(
            stale_login.err().map(|failure| failure.code),
            Some(SessionErrorCode::RelayNotFound)
        );
        assert_eq!(
            relays.list().relays.len(),
            1,
            "the entry added again stands, whatever was queued on the one before"
        );
    }

    /// However many joins a Relay asks of the Server at once, it takes up no
    /// more than a bound of them at a time, and takes up more as those end.
    /// The test's runtime runs nothing it spawns until the test waits, so
    /// none of the take-ups gets anywhere meanwhile.
    #[tokio::test]
    async fn a_server_takes_up_a_bounded_number_of_joins_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let relays = controller(directory.path());
        let address = unanswered_address();
        let mut session = WaitingSession::new(
            Waited {
                making: 0,
                stretch: 1,
            },
            watch::Sender::new(ServeThrough::first(true)).subscribe(),
        );
        for join in 0..TAKE_UPS_AT_ONCE + 4 {
            relays.take_up(&address, vec![u8::try_from(join).unwrap()], &mut session);
        }
        assert_eq!(session.take_ups.len(), TAKE_UPS_AT_ONCE);

        session.take_ups.abort_all();
        while session.take_ups.join_next().await.is_some() {}
        relays.take_up(&address, vec![0], &mut session);
        assert_eq!(
            session.take_ups.len(),
            1,
            "joins are taken up again once those under way have ended"
        );
    }

    /// A join taken up while the Server waited under its user's choice to
    /// Serve through a Relay is handed on under that making of the choice
    /// alone: turned off and on again before the join is made — however
    /// soon, and whether or not that waiting has yet been seen to end — it
    /// is handed on to nothing.
    #[test]
    fn a_choice_turned_off_and_on_again_hands_on_nothing_taken_up_under_it() {
        let directory = tempfile::tempdir().unwrap();
        let relays = controller(directory.path());
        let address = unanswered_address();
        relays.add(&address).unwrap();
        relays.set_serve_through(&address, true).unwrap();
        let serve_through = relays
            .lock()
            .iter()
            .find(|held| held.stored.address == address)
            .unwrap()
            .serve_through
            .subscribe();
        let making = serve_through.borrow().making;
        let session = WaitingSession::new(Waited { making, stretch: 1 }, serve_through);
        let hand_off = session.hand_off();
        assert!(hand_off.stands());

        relays.set_serve_through(&address, false).unwrap();
        relays.set_serve_through(&address, true).unwrap();
        assert!(
            !hand_off.stands(),
            "the waiting it was taken up during ended as the choice was turned off"
        );
        drop(session);
    }

    /// The waiting ends with the choice it was done under, or the stretch
    /// of Serving it was done in, though both are back as they were by the
    /// time the change is looked at.
    #[test]
    fn a_change_and_its_undoing_still_end_the_waiting_done_before() {
        use futures_util::FutureExt as _;

        let choice = watch::Sender::new(ServeThrough::first(true));
        let serving = watch::Sender::new(ServingStretch {
            serving: true,
            number: 1,
        });
        let mut wish = WaitingWish {
            serve_through: choice.subscribe(),
            serving: serving.subscribe(),
        };
        let waiting = wish.now();
        assert!(waiting.is_some());
        assert!(wish.departs_from(waiting).now_or_never().is_none());

        choice.send_modify(|choice| *choice = choice.made_again(false));
        choice.send_modify(|choice| *choice = choice.made_again(true));
        assert!(wish.departs_from(waiting).now_or_never().is_some());

        let waiting = wish.now();
        serving.send_modify(|serving| serving.serving = false);
        serving.send_modify(|serving| {
            *serving = ServingStretch {
                serving: true,
                number: serving.number + 1,
            }
        });
        assert!(wish.departs_from(waiting).now_or_never().is_some());
    }

    #[test]
    fn an_address_no_relay_is_reached_at_is_refused_as_such() {
        for written in [
            "",
            "ftp://relay.example.com",
            "https://relay.example.com/?x=1",
        ] {
            assert_eq!(
                relay_address(written).err().map(|failure| failure.code),
                Some(SessionErrorCode::InvalidRelayAddress),
                "{written:?}"
            );
        }
        assert_eq!(
            relay_address("Relay.Example.com/").ok().as_deref(),
            Some("https://relay.example.com")
        );
    }

    /// A stand-in Relay that proves every Server it is reached by, under an
    /// Account, without looking at its proof — saying it has the proof, and
    /// holding its answer until it is released — and tells the test the
    /// first thing each Server says once proven, `None` where it says nothing
    /// more.
    struct ProvingRelay {
        address: String,
        heard: tokio::sync::mpsc::UnboundedReceiver<Option<ServerMessage>>,
        /// Says the stand-in has a Server's proof, and holds its answer.
        proving: Arc<tokio::sync::Notify>,
        /// Releases the answer held.
        release: Arc<tokio::sync::Notify>,
    }

    async fn proving_relay() -> ProvingRelay {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (heard, hearing) = tokio::sync::mpsc::unbounded_channel();
        let proving = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let relay = address.clone();
        let (proved, released) = (proving.clone(), release.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (heard, relay) = (heard.clone(), relay.clone());
                let (proved, released) = (proved.clone(), released.clone());
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    let said = |message: RelayMessage| {
                        Message::Text(serde_json::to_string(&message).unwrap().into())
                    };
                    let Some(ServerMessage::Hello { .. }) = next_said(&mut socket).await else {
                        return;
                    };
                    let challenge = said(RelayMessage::Challenge {
                        version: SPOKEN[0],
                        nonce: Bytes(vec![0; relay_protocol::NONCE_LEN]),
                        relay,
                    });
                    if socket.send(challenge).await.is_err() {
                        return;
                    }
                    let Some(ServerMessage::Proof { .. }) = next_said(&mut socket).await else {
                        return;
                    };
                    proved.notify_one();
                    released.notified().await;
                    let proven = said(RelayMessage::Proven {
                        login: Some(relay_protocol::Account {
                            provider: "scripted".to_owned(),
                            username: "octocat".to_owned(),
                        }),
                    });
                    if socket.send(proven).await.is_err() {
                        return;
                    }
                    let _ = heard.send(next_said(&mut socket).await);
                });
            }
        });
        ProvingRelay {
            address,
            heard: hearing,
            proving,
            release,
        }
    }

    /// The next thing a Server says to the stand-in Relay on `socket`, or
    /// `None` once it says nothing more.
    async fn next_said(
        socket: &mut WebSocketStream<tokio::net::TcpStream>,
    ) -> Option<ServerMessage> {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).ok(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    }

    /// A connection let go of while the Relay takes the Server's proof asks
    /// no join there, however soon the Relay answers: whether it is still
    /// wanted is asked again just before the join would be. The Relay holds
    /// its answer until the connection has been let go, and the Server waits
    /// for it far longer than the test could take, so nothing but the letting
    /// go decides what comes of the join.
    #[tokio::test]
    async fn no_join_is_asked_for_a_connection_no_longer_wanted_once_proven() {
        // Waited on only where the Server or the stand-in fails.
        const STALLED: Duration = Duration::from_secs(60);
        let directory = tempfile::tempdir().unwrap();
        let relays = controller_answering_within(directory.path(), Duration::from_secs(600));
        let mut relay = proving_relay().await;
        relays.add(&relay.address).unwrap();
        relays
            .lock()
            .iter_mut()
            .find(|held| held.stored.address == relay.address)
            .unwrap()
            .stored
            .logged_in = true;
        let joining = Joining {
            relays: relays.relays.clone(),
            identity: relays.identity.clone(),
            dialer: relays.dialer.clone(),
            timings: relays.timings,
            carrying: relays.carrying.clone(),
        };
        let interest = tokio::sync::watch::Sender::new(());
        let wanted = Wanted::while_held(&interest);
        let address = relay.address.clone();
        let joined =
            tokio::spawn(async move { joining.join(&address, vec![7; 32], &wanted).await });

        tokio::time::timeout(STALLED, relay.proving.notified())
            .await
            .expect("the Server proves itself to the stand-in");
        drop(interest);
        relay.release.notify_one();
        let joined = tokio::time::timeout(STALLED, joined)
            .await
            .expect("the join ends once the Relay answers")
            .expect("the join runs to its end");
        assert_eq!(
            joined.as_ref().err().map(std::io::Error::kind),
            Some(std::io::ErrorKind::Interrupted),
            "the join ends as no longer wanted, not as {:?}",
            joined.as_ref().err()
        );
        let after_proof = tokio::time::timeout(STALLED, relay.heard.recv())
            .await
            .expect("the stand-in proved the Server")
            .expect("the stand-in is still listening");
        assert!(
            !matches!(after_proof, Some(ServerMessage::Join { .. })),
            "no join is asked for a connection nothing wants: {after_proof:?}"
        );
    }

    /// A join over one end of a pipe, ended by `stop`; and the Relay's end.
    async fn carried_pair(
        stop: Arc<Stop>,
    ) -> (
        CarriedStream<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        (
            CarriedStream::new(
                WebSocketStream::from_raw_socket(ours, Role::Client, None).await,
                stop,
            ),
            WebSocketStream::from_raw_socket(theirs, Role::Server, None).await,
        )
    }

    /// The error reading `carried` ends in, once it ends within a second.
    async fn ending(carried: &mut CarriedStream<tokio::io::DuplexStream>) -> std::io::Error {
        use tokio::io::AsyncReadExt as _;
        let read = tokio::time::timeout(Duration::from_secs(1), async {
            let mut buffer = [0_u8; 64];
            loop {
                if let Err(error) = carried.read(&mut buffer).await {
                    return error;
                }
            }
        });
        read.await.expect("the join ends in time")
    }

    /// What `inner` carries, taken in no more than a kilobyte at a time and
    /// a while apart.
    struct Slowly<S> {
        inner: S,
        next: Pin<Box<tokio::time::Sleep>>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Slowly<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            ready!(self.next.as_mut().poll(context));
            let mut chunk = [0_u8; 1024];
            let room = chunk.len().min(buffer.remaining());
            let mut chunk = ReadBuf::new(&mut chunk[..room]);
            ready!(Pin::new(&mut self.inner).poll_read(context, &mut chunk))?;
            buffer.put_slice(chunk.filled());
            let next = tokio::time::Instant::now() + Duration::from_millis(5);
            self.next.as_mut().reset(next);
            Poll::Ready(Ok(()))
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Slowly<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(context, buffer)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            context: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

    /// A Relay that takes in what this Server sends slowly but steadily,
    /// each frame taking it a while, ends no transfer: how long a frame
    /// takes to go is no sign the Relay has stopped answering. What the
    /// Relay takes in is counted as the bytes the frames carry, not those
    /// framing them.
    #[tokio::test]
    async fn a_relay_taking_in_slowly_but_steadily_ends_no_transfer() {
        use tokio::io::AsyncWriteExt as _;
        const SENT: usize = 128 * 1024;
        let (ours, theirs) = tokio::io::duplex(1024);
        let mut carried = CarriedStream::new(
            WebSocketStream::from_raw_socket(ours, Role::Client, None).await,
            Arc::default(),
        );
        let taking_in = tokio::spawn(async move {
            let slowly = Slowly {
                inner: theirs,
                next: Box::pin(tokio::time::sleep(Duration::ZERO)),
            };
            let mut relay = WebSocketStream::from_raw_socket(slowly, Role::Server, None).await;
            let mut taken = 0;
            while taken < SENT {
                match relay.next().await {
                    Some(Ok(Message::Binary(carried))) => taken += carried.len(),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            (relay, taken)
        });

        let sent = tokio::time::timeout(Duration::from_secs(10), async {
            carried.write_all(&vec![7; SENT]).await?;
            carried.flush().await
        })
        .await
        .expect("the transfer finishes in time");
        sent.expect("the transfer goes on however slowly the Relay takes it in");
        let (_relay, taken) = taking_in.await.unwrap();
        assert_eq!(taken, SENT, "the Relay took in all that was sent");
    }

    #[tokio::test]
    async fn every_join_carried_ends_as_the_server_stops_and_any_carried_after() {
        let carrying = Carrying::default();
        let (mut carried, _relay) = carried_pair(carrying.stop()).await;
        carrying.stop_all();
        assert_eq!(
            ending(&mut carried).await.kind(),
            std::io::ErrorKind::ConnectionAborted
        );
        let (mut late, _relay) = carried_pair(carrying.stop()).await;
        assert_eq!(
            ending(&mut late).await.kind(),
            std::io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn a_relay_on_this_machines_loopback_is_told_apart_from_one_elsewhere() {
        for address in [
            "http://127.0.0.1:8080",
            "https://127.4.5.6",
            "http://[::1]:8080",
            "https://LocalHost",
        ] {
            assert!(on_loopback(address), "{address}");
        }
        for address in [
            "https://relay.example.com",
            "http://10.0.0.8:8080",
            "https://[2001:db8::1]",
            "https://localhost.example.com",
        ] {
            assert!(!on_loopback(address), "{address}");
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
