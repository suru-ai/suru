//! Reading at an Origin: the Sessions and Workspaces this Server holds, and
//! a paired Remote's, read through one interface.
//!
//! A Sidekick never speaks to a Remote itself (ADR 0044). What it reads of
//! one, its own Server fetches through the Pairing exactly as a Client turned
//! toward that Remote fetches what it reads — the Remote's own Session API,
//! asked through [`ServingController::proxy_remote`], the route behind the
//! local Server's `/v1/remotes/{name}` — and answers here in the very types
//! this Server's own reads answer in. So a Broker Tool reads every Origin
//! alike, and projects what it read with the one projection whatever Server
//! it came from. What it does to one is carried the same way, by the
//! operations in [`super::acts_at`], through [`RemoteReach::act`].
//!
//! Nothing read from a Remote is kept. Each read asks the Remote afresh and
//! is answered only by what the Remote says to it then, so a Remote that does
//! not answer — one that cannot be reached, says nothing within the reach
//! timeout, refuses this Server's key, or speaks another protocol — is named
//! as not answering, saying why, and is never answered for from what it last
//! said. A listing ranging Everywhere asks this Server and every paired
//! Remote at once, and answers with what answered and the Remotes that did
//! not. What a Remote said stands only while its Pairing does: one unpaired,
//! or paired anew under its name, before a read answers takes what it said
//! with it.
//!
//! A Remote is read no further than the reach budget: an answer running past
//! it — faulty, or worse — is not taken into memory whole, and the Remote is
//! named as having said too much.

use std::{fmt, time::Duration};

use axum::{
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header::CONTENT_TYPE},
};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, future::join_all};
use serde::{Serialize, de::DeserializeOwned};

use super::SessionOperations;
use crate::{
    protocol::{
        Author, Outlook, Remote, RemoteStatus, SessionError, SessionErrorCode, SessionId,
        SessionListItem, SnapshotWithSummary, WorkspaceListing, WorkspacePaths,
    },
    serving::{PairingFailure, ServingController},
};

/// Where a Server's Session API lists its top-level Sessions.
pub(super) const SESSIONS_PATH: &str = "/v1/sessions";

/// Where a Server's Session API lists the Workspaces it knows.
pub(super) const WORKSPACES_PATH: &str = "/v1/workspaces";

/// How this Server reaches its Remotes for a read: through the Pairing, as a
/// Client's request turned toward one is carried, giving each Remote
/// `timeout` to answer and reading no more than `budget` bytes of it.
#[derive(Clone)]
pub(crate) struct RemoteReach {
    serving: ServingController,
    timeout: Duration,
    budget: usize,
}

impl RemoteReach {
    pub(crate) fn new(serving: ServingController, timeout: Duration, budget: usize) -> Self {
        Self {
            serving,
            timeout,
            budget,
        }
    }

    /// The Remotes this Server is paired with, in the order paired.
    fn paired(&self) -> Vec<Remote> {
        self.serving.list_remotes()
    }

    /// The Remote paired as `name`.
    pub(super) fn named(&self, name: &str) -> Result<Remote, OriginRefusal> {
        self.paired()
            .into_iter()
            .find(|remote| remote.name == name)
            .ok_or_else(|| OriginRefusal::UnknownRemote(name.to_owned()))
    }

    /// Whether `asked` is paired still as it was when it was asked — by the
    /// same name, with the same key — so what it said is still this Server's
    /// to give.
    pub(super) fn still_paired(&self, asked: &Remote) -> Result<(), OriginRefusal> {
        match self
            .paired()
            .into_iter()
            .find(|remote| remote.name == asked.name)
        {
            None => Err(OriginRefusal::UnknownRemote(asked.name.clone())),
            Some(remote) if remote.fingerprint == asked.fingerprint => Ok(()),
            Some(_) => Err(OriginRefusal::Silent(SilentRemote::new(
                &asked.name,
                Silence::Repaired,
            ))),
        }
    }

    /// Whether the Remote `name` answers now, asked as a Client's probe of it
    /// asks.
    async fn answers(&self, name: &str) -> Result<(), OriginRefusal> {
        let silence =
            match tokio::time::timeout(self.timeout, self.serving.probe_remote(name)).await {
                Err(_) => Silence::TimedOut(self.timeout),
                Ok(Err(failure)) if failure.code == SessionErrorCode::RemoteNotFound => {
                    return Err(OriginRefusal::UnknownRemote(name.to_owned()));
                }
                Ok(Err(failure)) => Silence::Failed(failure.message),
                Ok(Ok(health)) => match health.status {
                    RemoteStatus::Available => return Ok(()),
                    RemoteStatus::Unavailable => Silence::Unreachable,
                    RemoteStatus::Revoked => Silence::Revoked,
                    RemoteStatus::ProtocolMismatch => Silence::ProtocolMismatch,
                },
            };
        Err(OriginRefusal::Silent(SilentRemote::new(name, silence)))
    }

    /// What the Remote `name`'s Session API answers `request` with, carried
    /// through the Pairing as the act of `author` where it names one, and read
    /// whole within the reach timeout and budget. A Remote that could not be
    /// asked, or did not answer whole, is refused for it.
    async fn exchange(
        &self,
        name: &str,
        request: Request<Body>,
        author: Option<&Author>,
    ) -> Result<Exchanged, OriginRefusal> {
        let exchange = async {
            let response = self.serving.proxy_remote(name, request, author).await?;
            let status = response.status();
            let headers = response.headers().clone();
            Ok::<_, PairingFailure>((
                status,
                headers,
                read_within(response.into_body(), self.budget).await,
            ))
        };
        let silent = |silence| OriginRefusal::Silent(SilentRemote::new(name, silence));
        match tokio::time::timeout(self.timeout, exchange).await {
            Err(_) => Err(silent(Silence::TimedOut(self.timeout))),
            Ok(Err(failure)) => Err(match failure.code {
                SessionErrorCode::RemoteNotFound => OriginRefusal::UnknownRemote(name.to_owned()),
                SessionErrorCode::PairingAuthenticationFailed => silent(Silence::Revoked),
                SessionErrorCode::PairingOutcomeUnknown => silent(Silence::BrokeOff),
                _ => silent(Silence::Unreachable),
            }),
            // The Remote stopped answering partway through what it said.
            Ok(Ok((_, _, Err(Unread::Broken)))) => Err(silent(Silence::BrokeOff)),
            Ok(Ok((_, _, Err(Unread::PastBudget)))) => {
                Err(silent(Silence::PastBudget(self.budget)))
            }
            Ok(Ok((status, headers, Ok(body)))) => {
                let error = (!status.is_success())
                    .then(|| serde_json::from_slice::<SessionError>(&body).ok())
                    .flatten();
                // The Pairing's own refusals say the Remote was never asked.
                match error.as_ref().map(|error| error.code) {
                    Some(SessionErrorCode::PairingProtocolMismatch) => {
                        return Err(silent(Silence::ProtocolMismatch));
                    }
                    Some(SessionErrorCode::PairingAuthenticationFailed) => {
                        return Err(silent(Silence::Revoked));
                    }
                    // The Remote could not reach its own Session API for the
                    // request.
                    Some(SessionErrorCode::PairingConnectionFailed) => {
                        return Err(silent(Silence::Unreachable));
                    }
                    // Its own Session API took the request, and its answer
                    // was lost.
                    Some(SessionErrorCode::PairingOutcomeUnknown) => {
                        return Err(silent(Silence::BrokeOff));
                    }
                    _ => {}
                }
                Ok(Exchanged {
                    remote: name.to_owned(),
                    status,
                    headers,
                    body,
                    error,
                })
            }
        }
    }

    /// What the Remote `name`'s Session API answers a `GET` of
    /// `path_and_query` with, asked through the Pairing and decoded as `T`.
    async fn get<T: DeserializeOwned>(
        &self,
        name: &str,
        path_and_query: &str,
    ) -> Result<T, RemoteReadFailure> {
        let request = Request::get(path_and_query)
            .body(Body::empty())
            .expect("a Session API path makes a request");
        let exchanged = self.exchange(name, request, None).await?;
        let silent = |silence| RemoteReadFailure::from(SilentRemote::new(name, silence));
        if exchanged.status.is_success() {
            return serde_json::from_slice(&exchanged.body)
                .map_err(|_| silent(unreadable_answer()));
        }
        Err(match exchanged.error.map(|error| error.code) {
            Some(SessionErrorCode::SessionNotFound) => RemoteReadFailure::SessionNotFound,
            Some(SessionErrorCode::SessionUnreadable) => RemoteReadFailure::SessionUnreadable,
            _ => silent(Silence::Failed(failed_with(exchanged.status))),
        })
    }

    /// What the Remote `name`'s Session API answers the act `method` of
    /// `path` asks for, `body` sent with it as JSON where there is one: the
    /// act `author` performs, carried through the Pairing as a Client's act
    /// is and named there as a Sidekick's on this Peer. A Remote that could
    /// not be asked, did not answer, or refused the act is refused for it,
    /// and nothing is kept to ask it again.
    pub(super) async fn act(
        &self,
        name: &str,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
        author: &Author,
    ) -> Result<Exchanged, RemoteActRefusal> {
        let remote = self.named(name)?;
        let request = Request::builder().method(method).uri(path);
        let request = match body {
            Some(body) => request
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(body).expect("an act's terms always serialize"),
                )),
            None => request.body(Body::empty()),
        }
        .expect("a Session API path makes a request");
        let exchanged = self.exchange(name, request, Some(author)).await;
        self.still_paired(&remote)?;
        let exchanged = exchanged?;
        if exchanged.status.is_success() {
            return Ok(exchanged);
        }
        let code = exchanged.error.as_ref().map(|error| error.code);
        Err(RemoteActRefusal::Refused {
            remote: name.to_owned(),
            reason: match exchanged.error {
                Some(error) => Refusal::Said(error.message),
                None => Refusal::Failed(failed_with(exchanged.status)),
            },
            code,
        })
    }

    /// The events the Remote `name`'s Session API streams at `path`, opened
    /// through the Pairing as a Client's subscription to that Remote is. The
    /// whole of the opening — the Remote's answer, and the refusal it gives
    /// where it gives one — is given the reach timeout; after it, the
    /// events are read as they come for as long as the Remote goes on,
    /// each no longer than the reach budget, and the stream ends with an
    /// error once the Remote says nothing at all — not even the keep-alive
    /// its streams send — for `silence_limit`.
    pub(super) async fn events(
        &self,
        name: &str,
        path: &str,
        silence_limit: Duration,
    ) -> Result<RemoteEvents, OriginRefusal> {
        let request = Request::get(path)
            .body(Body::empty())
            .expect("a Session API path makes a request");
        let silent = |silence| OriginRefusal::Silent(SilentRemote::new(name, silence));
        let opening = async {
            let response = self.serving.proxy_remote(name, request, None).await?;
            if response.status().is_success() {
                return Ok(Ok(response));
            }
            Ok::<_, PairingFailure>(Err(read_within(response.into_body(), self.budget)
                .await
                .unwrap_or_default()))
        };
        match tokio::time::timeout(self.timeout, opening).await {
            Err(_) => Err(silent(Silence::TimedOut(self.timeout))),
            Ok(Err(failure)) => Err(match failure.code {
                SessionErrorCode::RemoteNotFound => OriginRefusal::UnknownRemote(name.to_owned()),
                SessionErrorCode::PairingAuthenticationFailed => silent(Silence::Revoked),
                _ => silent(Silence::Unreachable),
            }),
            Ok(Ok(Err(refused))) => Err(
                match serde_json::from_slice::<SessionError>(&refused).map(|error| error.code) {
                    Ok(SessionErrorCode::PairingProtocolMismatch) => {
                        silent(Silence::ProtocolMismatch)
                    }
                    Ok(SessionErrorCode::PairingAuthenticationFailed) => silent(Silence::Revoked),
                    _ => silent(Silence::Unreachable),
                },
            ),
            Ok(Ok(Ok(response))) => Ok(Box::pin(
                within_event_budget(
                    within_silence_limit(response.into_body().into_data_stream(), silence_limit),
                    self.budget,
                )
                .eventsource(),
            )),
        }
    }

    /// How long a Remote is given to answer.
    pub(super) fn timeout(&self) -> Duration {
        self.timeout
    }

    /// What moves on with every change to the Remotes this Server is paired
    /// with.
    pub(super) fn pairing_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.serving.pairing_changes()
    }

    /// The top-level Sessions the Remote `name` holds, as its own listing
    /// gives them now.
    pub(super) async fn sessions_of(
        &self,
        name: &str,
    ) -> Result<Vec<SessionListItem>, OriginRefusal> {
        let remote = self.named(name)?;
        let read = self.get(name, SESSIONS_PATH).await;
        self.still_paired(&remote)?;
        read.map_err(|failure| failure.of_listing(name))
    }

    /// What the Remote `name` answers a `GET` of `path` with, decoded as
    /// `T`, read on the way to an act: refused as the act would be.
    pub(super) async fn read_for_act<T: DeserializeOwned>(
        &self,
        name: &str,
        path: &str,
    ) -> Result<T, RemoteActRefusal> {
        let remote = self.named(name)?;
        let read = self.get(name, path).await;
        self.still_paired(&remote)?;
        read.map_err(|failure| failure.of_listing(name).into())
    }
}

/// A Remote's whole answer to one request: its status, its headers, its body,
/// and the Session error the body says where the status is one.
pub(super) struct Exchanged {
    remote: String,
    status: StatusCode,
    pub(super) headers: HeaderMap,
    body: Vec<u8>,
    error: Option<SessionError>,
}

impl Exchanged {
    /// Whether the Remote answered with nothing more to say than that it was
    /// done.
    pub(super) fn is_empty(&self) -> bool {
        self.status == StatusCode::NO_CONTENT || self.body.is_empty()
    }

    /// The body, read as `T`: the act was done, so one this Server cannot
    /// read is refused as a Remote that may have done it.
    pub(super) fn read<T: DeserializeOwned>(&self) -> Result<T, RemoteActRefusal> {
        serde_json::from_slice(&self.body).map_err(|_| {
            OriginRefusal::Silent(SilentRemote::new(&self.remote, unreadable_answer())).into()
        })
    }
}

/// What a Remote that answered with what this Server cannot read is said to
/// have done.
fn unreadable_answer() -> Silence {
    Silence::Failed("it answered with what this server could not read".to_owned())
}

/// The events a Remote streams, as they come.
pub(super) type RemoteEvents = std::pin::Pin<
    Box<
        dyn futures_util::Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<std::io::Error>,
                >,
            > + Send,
    >,
>;

/// `body`, ended with an error once nothing at all arrives of it for
/// `silence_limit`.
fn within_silence_limit(
    body: impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send + 'static,
    silence_limit: Duration,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static {
    futures_util::stream::unfold(Some(Box::pin(body)), move |body| async move {
        let mut body = body?;
        match tokio::time::timeout(silence_limit, body.next()).await {
            Err(_) => Some((
                Err(std::io::Error::other(
                    "the Remote said nothing for too long",
                )),
                None,
            )),
            Ok(None) => None,
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(body))),
            Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), None)),
        }
    })
}

/// `body`, ended with an error where any one event of it — its lines up to
/// the blank line ending it, however the chunks it arrives in fall — runs
/// past `budget` bytes, so no event is held whole that a Remote — faulty, or
/// worse — says too much in.
fn within_event_budget(
    body: impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static,
    budget: usize,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static {
    body.scan(EventFraming::default(), move |framing, chunk| {
        futures_util::future::ready(Some(chunk.and_then(|chunk| {
            if framing.take(&chunk, budget) {
                Ok(chunk)
            } else {
                Err(std::io::Error::other("an event ran past the reach budget"))
            }
        })))
    })
}

/// Where an event stream stands between events: how many bytes of the event
/// under way have arrived, and whether the last of them ended a line.
#[derive(Debug, Default)]
struct EventFraming {
    event: usize,
    after_line: bool,
}

impl EventFraming {
    /// Takes `chunk` in, answering whether every event it completes or
    /// carries on stays within `budget`. A line ends at a line feed, a
    /// carriage return before one being part of it, and a line ending with
    /// nothing on it ends the event.
    fn take(&mut self, chunk: &[u8], budget: usize) -> bool {
        for byte in chunk {
            match byte {
                b'\n' if self.after_line => {
                    self.event = 0;
                    self.after_line = false;
                    continue;
                }
                b'\n' => self.after_line = true,
                b'\r' => {}
                _ => self.after_line = false,
            }
            self.event += 1;
            if self.event > budget {
                return false;
            }
        }
        true
    }
}

/// Why the whole of an answer was not read.
#[derive(Debug, Eq, PartialEq)]
enum Unread {
    /// It broke off before it ended.
    Broken,
    /// It ran past the budget, and was read no further.
    PastBudget,
}

/// The whole of `body`, read no further than `budget` bytes: one running past
/// it is never held whole.
async fn read_within(body: Body, budget: usize) -> Result<Vec<u8>, Unread> {
    let mut chunks = body.into_data_stream();
    let mut read = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| Unread::Broken)?;
        if chunk.len() > budget - read.len() {
            return Err(Unread::PastBudget);
        }
        read.extend_from_slice(&chunk);
    }
    Ok(read)
}

/// What a Remote that answered with `status` rather than what was asked is
/// said to have answered.
fn failed_with(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("it answered with an error, {} {reason}", status.as_u16()),
        None => format!("it answered with an error, {}", status.as_u16()),
    }
}

/// The Servers a listing ranges over: one Origin, or Everywhere — this Server
/// and every Remote it is paired with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Origins {
    One(Outlook),
    Everywhere,
}

/// What a read gathered from the Origins it ranged over.
#[derive(Debug)]
pub(crate) struct Gathered<T> {
    /// What each Origin that answered gave: this Server's first, then each
    /// Remote's in the order they were paired.
    pub(crate) answered: Vec<(Outlook, T)>,
    /// Every Remote that did not answer, saying why.
    pub(crate) unanswered: Vec<SilentRemote>,
}

/// A paired Remote that gave a read nothing, and why, said in a sentence the
/// reader can pass on.
#[derive(Debug)]
pub(crate) struct SilentRemote {
    pub(crate) name: String,
    silence: Silence,
}

impl SilentRemote {
    fn new(name: &str, silence: Silence) -> Self {
        Self {
            name: name.to_owned(),
            silence,
        }
    }

    /// Whether its name was paired anew, to another key, while it was asked.
    pub(crate) fn is_repaired(&self) -> bool {
        matches!(self.silence, Silence::Repaired)
    }

    /// Whether an act asked of the Remote may have been done there all the
    /// same: it was asked, and then did not answer, or answered so it could
    /// not be read. An act never carried there was never done.
    pub(crate) fn may_have_acted(&self) -> bool {
        match self.silence {
            Silence::Unreachable | Silence::Revoked | Silence::ProtocolMismatch => false,
            Silence::BrokeOff
            | Silence::TimedOut(_)
            | Silence::PastBudget(_)
            | Silence::Repaired
            | Silence::Failed(_) => true,
        }
    }
}

/// Why a Remote gave a read nothing.
#[derive(Debug)]
enum Silence {
    /// It could not be reached at any address it was paired at, so nothing
    /// asked of it reached it.
    Unreachable,
    /// It was asked, and stopped answering before its answer was whole.
    BrokeOff,
    /// It was reached, but said nothing within the reach timeout.
    TimedOut(Duration),
    /// It refused this Server's key: its user ended the Pairing on their
    /// side.
    Revoked,
    /// It speaks another version of the protocol Servers speak to each other.
    ProtocolMismatch,
    /// It answered with more than the reach budget, in bytes.
    PastBudget(usize),
    /// Its name was paired anew, to another key, while it was asked.
    Repaired,
    /// It answered, but not with what was asked, for the reason given.
    Failed(String),
}

impl fmt::Display for SilentRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = &self.name;
        match &self.silence {
            Silence::Unreachable => write!(
                formatter,
                "The Remote `{name}` is not answering: Suru could not reach it at any address it \
                 was paired at."
            ),
            Silence::BrokeOff => write!(
                formatter,
                "The Remote `{name}` stopped answering once it had been asked."
            ),
            Silence::TimedOut(timeout) => write!(
                formatter,
                "The Remote `{name}` is not answering: it said nothing within {}.",
                spelled_duration(*timeout)
            ),
            Silence::Revoked => write!(
                formatter,
                "The Remote `{name}` refused this Suru server's key: its user has ended the \
                 Pairing on their side, and reaching it again takes a new Invite."
            ),
            Silence::ProtocolMismatch => write!(
                formatter,
                "The Remote `{name}` runs a version of Suru that speaks another protocol than this \
                 server's, so nothing can be asked of it until one of them is updated."
            ),
            Silence::PastBudget(budget) => write!(
                formatter,
                "The Remote `{name}` answered with more than the {} this server reads of one \
                 answer from a Remote, so nothing it said was read.",
                spelled_bytes(*budget)
            ),
            Silence::Repaired => write!(
                formatter,
                "The Remote `{name}` was paired anew while it was asked, so what it said is not \
                 this Pairing's; ask again."
            ),
            Silence::Failed(reason) => {
                write!(formatter, "The Remote `{name}` could not answer: {reason}.")
            }
        }
    }
}

/// `duration` as a sentence says it: in whole seconds, or in milliseconds
/// under a second.
fn spelled_duration(duration: Duration) -> String {
    match duration.as_secs() {
        0 => format!("{} milliseconds", duration.as_millis()),
        1 => "a second".to_owned(),
        seconds => format!("{seconds} seconds"),
    }
}

/// `bytes` as a sentence says it: in whole MiB or KiB where it is some.
fn spelled_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Why a read at an Origin was refused before anything was read.
#[derive(Debug)]
pub(crate) enum OriginRefusal {
    /// This Server is paired with no Remote by the name given.
    UnknownRemote(String),
    /// The Remote named did not answer.
    Silent(SilentRemote),
}

/// Why an act at a Remote was refused, before or after the Remote was asked.
/// Either way nothing is kept to ask it again: a Sidekick acting on a Remote
/// that does not answer is told so, and asks again once it does.
#[derive(Debug)]
pub(crate) enum RemoteActRefusal {
    /// The Remote could not be asked, or did not answer.
    Origin(OriginRefusal),
    /// The Remote refused the act, saying why, by the code it gave where it
    /// gave one.
    Refused {
        remote: String,
        reason: Refusal,
        code: Option<SessionErrorCode>,
    },
}

/// What a Remote that refused an act said of why.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// It said why, in its own words.
    Said(String),
    /// It answered with an error and no words, as given.
    Failed(String),
}

impl RemoteActRefusal {
    /// Whether the act may have been done at the Remote all the same: it was
    /// carried there, and the Remote's answer to it was never read.
    pub(crate) fn may_have_acted(&self) -> bool {
        match self {
            Self::Origin(OriginRefusal::Silent(silent)) => silent.may_have_acted(),
            Self::Origin(OriginRefusal::UnknownRemote(_)) | Self::Refused { .. } => false,
        }
    }

    /// Whether the Remote refused the act for holding no such Session.
    pub(crate) fn is_session_not_found(&self) -> bool {
        matches!(
            self,
            Self::Refused {
                code: Some(SessionErrorCode::SessionNotFound),
                ..
            }
        )
    }
}

impl From<OriginRefusal> for RemoteActRefusal {
    fn from(refusal: OriginRefusal) -> Self {
        Self::Origin(refusal)
    }
}

/// Why a Remote gave a read of it nothing.
#[derive(Debug)]
enum RemoteReadFailure {
    Origin(OriginRefusal),
    /// The Remote holds no Session by the identity asked for.
    SessionNotFound,
    /// The Remote holds the Session asked for, but could not read what it
    /// stored of it.
    SessionUnreadable,
}

impl From<OriginRefusal> for RemoteReadFailure {
    fn from(refusal: OriginRefusal) -> Self {
        Self::Origin(refusal)
    }
}

impl From<SilentRemote> for OriginRefusal {
    fn from(silent: SilentRemote) -> Self {
        Self::Silent(silent)
    }
}

impl From<SilentRemote> for RemoteReadFailure {
    fn from(silent: SilentRemote) -> Self {
        Self::Origin(OriginRefusal::Silent(silent))
    }
}

impl RemoteReadFailure {
    /// The failure as one of a listing, which asks for no Session by its
    /// identity and so is never told of one.
    fn of_listing(self, name: &str) -> OriginRefusal {
        match self {
            Self::Origin(refusal) => refusal,
            Self::SessionNotFound | Self::SessionUnreadable => OriginRefusal::Silent(
                SilentRemote::new(name, Silence::Failed("it answered of a Session".to_owned())),
            ),
        }
    }
}

/// Why reading one Session at its Origin was refused.
#[derive(Debug)]
pub(crate) enum SessionReadRefusal {
    Origin(OriginRefusal),
    /// The Origin holds no Session by that identity.
    NotFound,
    /// The Origin holds the Session, but could not read what it stored of it.
    Unreadable,
    /// This Server could not bring the Session into memory from its own
    /// storage.
    Unloadable,
}

impl SessionOperations {
    /// Every Remote this Server is paired with, by name, in the order paired,
    /// and whether each answers now, saying why not of one that does not.
    /// Every Remote is asked at once, each given the reach timeout, and one
    /// unpaired while they were asked is not named at all.
    pub(crate) async fn remote_answers(&self) -> Vec<(String, Result<(), SilentRemote>)> {
        let remotes = self.remotes.paired();
        let answers = join_all(
            remotes
                .iter()
                .map(|remote| self.remotes.answers(&remote.name)),
        )
        .await;
        remotes
            .into_iter()
            .zip(answers)
            .filter_map(
                |(remote, answer)| match self.remotes.still_paired(&remote).and(answer) {
                    Ok(()) => Some((remote.name, Ok(()))),
                    Err(OriginRefusal::Silent(silent)) => Some((remote.name, Err(silent))),
                    Err(OriginRefusal::UnknownRemote(_)) => None,
                },
            )
            .collect()
    }

    /// The top-level Sessions at `origins`, as each Origin lists them.
    pub(crate) async fn sessions_in(
        &self,
        origins: &Origins,
    ) -> Result<Gathered<Vec<SessionListItem>>, OriginRefusal> {
        let asked_at = self.sessions.moment();
        let gathered = self
            .gather(origins, || self.sessions.list(None), SESSIONS_PATH)
            .await?;
        // What a Remote lists now confirms what it no longer holds.
        for (origin, listed) in &gathered.answered {
            if let Outlook::Remote(name) = origin {
                self.sessions.remote_listed(name, listed, asked_at);
            }
        }
        Ok(gathered)
    }

    /// The Workspaces known at `origins`, as each Origin lists them.
    pub(crate) async fn workspaces_in(
        &self,
        origins: &Origins,
    ) -> Result<Gathered<WorkspaceListing>, OriginRefusal> {
        self.gather(origins, || self.workspace_listing(), WORKSPACES_PATH)
            .await
    }

    /// Every Workspace this Server knows, spelled in its own paths: what its
    /// `GET /v1/workspaces` answers, a Peer included.
    pub(crate) fn workspace_listing(&self) -> WorkspaceListing {
        WorkspaceListing {
            workspace_paths: WorkspacePaths::discover(),
            workspaces: self.sessions.listed_workspaces(),
        }
    }

    /// The Session `session_id` at `origin`, brought into memory there as
    /// the Session API brings one, with the summary its listing reads it by,
    /// both as they stood in one moment: what this Server's
    /// `GET /v1/sessions/{session_id}/with-summary` answers, and for a Remote
    /// what the Remote's answers through the Pairing.
    pub(crate) async fn session_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
    ) -> Result<SnapshotWithSummary, SessionReadRefusal> {
        match origin {
            Outlook::Local => self.session_here(session_id).await,
            Outlook::Remote(name) => self.remote_session(name, session_id).await,
        }
    }

    async fn session_here(
        &self,
        session_id: SessionId,
    ) -> Result<SnapshotWithSummary, SessionReadRefusal> {
        if let Err(error) = self.sessions.hydrate(session_id).await {
            tracing::warn!(%session_id, "a Session read with its summary could not be loaded: {error}");
            return Err(SessionReadRefusal::Unloadable);
        }
        let Some((snapshot, summary)) = self.sessions.snapshot_and_summary(session_id) else {
            let unreadable = self
                .sessions
                .list(None)
                .iter()
                .any(|listed| listed.id() == session_id && listed.readable().is_none());
            return Err(if unreadable {
                SessionReadRefusal::Unreadable
            } else {
                SessionReadRefusal::NotFound
            });
        };
        Ok(SnapshotWithSummary { snapshot, summary })
    }

    /// The Session `session_id` on the Remote `name`, as the Remote holds it
    /// in one moment, asked of it in one request.
    async fn remote_session(
        &self,
        name: &str,
        session_id: SessionId,
    ) -> Result<SnapshotWithSummary, SessionReadRefusal> {
        let remote = self
            .remotes
            .named(name)
            .map_err(SessionReadRefusal::Origin)?;
        let read: Result<SnapshotWithSummary, _> = self
            .remotes
            .get(name, &format!("{SESSIONS_PATH}/{session_id}/with-summary"))
            .await;
        self.remotes
            .still_paired(&remote)
            .map_err(SessionReadRefusal::Origin)?;
        match &read {
            Ok(read) => self.settle_uncertain_acts(name, &read.snapshot),
            // Read and found gone: nothing it was acted on stands any more.
            Err(RemoteReadFailure::SessionNotFound) => {
                self.sessions.forget_remote_session(name, session_id);
            }
            Err(_) => {}
        }
        read.map_err(|failure| match failure {
            RemoteReadFailure::Origin(refusal) => SessionReadRefusal::Origin(refusal),
            RemoteReadFailure::SessionNotFound => SessionReadRefusal::NotFound,
            RemoteReadFailure::SessionUnreadable => SessionReadRefusal::Unreadable,
        })
    }

    /// What `read` gives at each Origin `origins` ranges over: `here` for this
    /// Server, and for a Remote what its Session API answers a `GET` of
    /// `path` with. A read at one Origin that cannot be read is refused,
    /// saying why; one ranging Everywhere asks every Remote at once and names
    /// each that did not answer.
    async fn gather<T: DeserializeOwned>(
        &self,
        origins: &Origins,
        here: impl FnOnce() -> T,
        path: &str,
    ) -> Result<Gathered<T>, OriginRefusal> {
        let remotes = match origins {
            Origins::One(Outlook::Local) => {
                return Ok(Gathered {
                    answered: vec![(Outlook::Local, here())],
                    unanswered: Vec::new(),
                });
            }
            Origins::One(Outlook::Remote(name)) => {
                let remote = self.remotes.named(name)?;
                let read = self.remotes.get(name, path).await;
                self.remotes.still_paired(&remote)?;
                return Ok(Gathered {
                    answered: vec![(
                        Outlook::Remote(name.clone()),
                        read.map_err(|failure| failure.of_listing(name))?,
                    )],
                    unanswered: Vec::new(),
                });
            }
            Origins::Everywhere => self.remotes.paired(),
        };
        let mut gathered = Gathered {
            answered: vec![(Outlook::Local, here())],
            unanswered: Vec::new(),
        };
        let reads = join_all(
            remotes
                .iter()
                .map(|remote| self.remotes.get(&remote.name, path)),
        )
        .await;
        // Each Pairing is asked after for every Remote at once, so one that
        // answered and was then unpaired while another was still being asked
        // takes what it said with it, as one whose Pairing has ended leaves
        // Everywhere.
        for (remote, read) in remotes.into_iter().zip(reads) {
            let read = self
                .remotes
                .still_paired(&remote)
                .and_then(|()| read.map_err(|failure| failure.of_listing(&remote.name)));
            match read {
                Ok(read) => gathered.answered.push((Outlook::Remote(remote.name), read)),
                Err(OriginRefusal::Silent(silent)) => gathered.unanswered.push(silent),
                Err(OriginRefusal::UnknownRemote(_)) => {}
            }
        }
        Ok(gathered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_remote_is_named_with_why_in_a_sentence_to_pass_on() {
        let said = |silence| SilentRemote::new("laptop", silence).to_string();
        assert_eq!(
            said(Silence::TimedOut(Duration::from_secs(10))),
            "The Remote `laptop` is not answering: it said nothing within 10 seconds."
        );
        assert_eq!(
            said(Silence::TimedOut(Duration::from_millis(150))),
            "The Remote `laptop` is not answering: it said nothing within 150 milliseconds."
        );
        assert!(said(Silence::Unreachable).contains("could not reach it at any address"));
        assert!(said(Silence::Revoked).contains("reaching it again takes a new Invite"));
        assert!(said(Silence::ProtocolMismatch).contains("until one of them is updated"));
        assert_eq!(
            said(Silence::PastBudget(64 * 1024 * 1024)),
            "The Remote `laptop` answered with more than the 64 MiB this server reads of one \
             answer from a Remote, so nothing it said was read."
        );
        assert!(said(Silence::Repaired).contains("was paired anew while it was asked"));
        assert_eq!(
            said(Silence::Failed(failed_with(
                StatusCode::INTERNAL_SERVER_ERROR
            ))),
            "The Remote `laptop` could not answer: it answered with an error, 500 Internal Server \
             Error."
        );
        assert_eq!(
            said(Silence::BrokeOff),
            "The Remote `laptop` stopped answering once it had been asked."
        );
        for silence in [
            Silence::Unreachable,
            Silence::BrokeOff,
            Silence::TimedOut(Duration::from_secs(1)),
            Silence::Revoked,
            Silence::ProtocolMismatch,
            Silence::PastBudget(1),
            Silence::Repaired,
        ] {
            let sentence = said(silence);
            assert!(
                sentence.starts_with("The Remote `laptop`") && sentence.ends_with('.'),
                "{sentence}"
            );
        }
    }

    #[test]
    fn a_budget_is_said_in_the_largest_whole_unit() {
        assert_eq!(spelled_bytes(64 * 1024 * 1024), "64 MiB");
        assert_eq!(spelled_bytes(16 * 1024), "16 KiB");
        assert_eq!(spelled_bytes(1_500), "1500 bytes");
    }

    /// A body arriving in `chunks`.
    fn arriving(chunks: Vec<&'static [u8]>) -> Body {
        Body::from_stream(futures_util::stream::iter(chunks.into_iter().map(
            |chunk| Ok::<_, std::io::Error>(axum::body::Bytes::from_static(chunk)),
        )))
    }

    #[test]
    fn every_event_is_held_to_the_budget_however_its_chunks_fall() {
        let mut framing = EventFraming::default();
        assert!(framing.take(b"data: abc\n", 16));
        assert!(
            framing.take(b"\ndata: de", 16),
            "a blank line split across two chunks ends the event before it"
        );
        assert!(framing.take(b"fghij\n\n", 16), "the next stays within it");
        let mut framing = EventFraming::default();
        assert!(
            !framing.take(b"data: 0123456789\n\ndata: x\n\n", 16),
            "an event past the budget is caught though a later one in the chunk is small"
        );
        let mut framing = EventFraming::default();
        assert!(framing.take(b"data: 0123456\r\n\r\n", 16));
        assert!(
            framing.take(b"data: 0123456\r\n\r\n", 16),
            "a carriage return before a line feed is part of the line"
        );
        let mut framing = EventFraming::default();
        assert!(framing.take(b"data: 01234", 16));
        assert!(
            !framing.take(b"567890", 16),
            "an event under way is held to it across chunks"
        );
    }

    #[tokio::test]
    async fn an_answer_is_read_whole_up_to_the_budget_and_no_further() {
        assert_eq!(
            read_within(arriving(vec![b"abc", b"def"]), 6).await,
            Ok(b"abcdef".to_vec()),
            "an answer as long as the budget is read whole"
        );
        assert_eq!(
            read_within(arriving(vec![b"abc", b"def"]), 5).await,
            Err(Unread::PastBudget),
            "and one a byte past it is not"
        );
        assert_eq!(
            read_within(arriving(vec![b"abcdef"]), 5).await,
            Err(Unread::PastBudget),
            "however it arrives"
        );
        let broken = Body::from_stream(futures_util::stream::iter([
            Ok(axum::body::Bytes::from_static(b"abc")),
            Err(std::io::Error::other("the Remote stopped answering")),
        ]));
        assert_eq!(read_within(broken, 64).await, Err(Unread::Broken));
    }
}
