//! Reading at an Origin: the Sessions and Workspaces this Server holds, and
//! a paired Remote's, read through one interface.
//!
//! A Sidekick never speaks to a Remote itself (ADR 0044). What it reads of
//! one, its own Server fetches through the Pairing exactly as it fetches what
//! a Client turned toward that Remote reads — the Remote's own Session API,
//! asked through [`ServingController::proxy_remote`], the route behind the
//! local Server's `/v1/remotes/{name}` — and answers here in the very types
//! this Server's own reads answer in. So a Broker Tool reads every Origin
//! alike, and projects what it read with the one projection whatever Server
//! it came from. Only reads reach a Remote here.
//!
//! Nothing read from a Remote is kept. Each read asks the Remote afresh and
//! is answered only by what the Remote says to it then, so a Remote that does
//! not answer — one that cannot be reached, says nothing within the reach
//! timeout, refuses this Server's key, or speaks another protocol — is named
//! as not answering, saying why, and is never answered for from what it last
//! said. A listing ranging Everywhere asks this Server and every paired
//! Remote at once, and answers with what answered and the Remotes that did
//! not.

use std::{fmt, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::future::join_all;
use serde::de::DeserializeOwned;

use super::SessionOperations;
use crate::{
    protocol::{
        Outlook, RemoteStatus, SessionError, SessionErrorCode, SessionId, SessionListItem,
        SessionSnapshot, WorkspaceListing, WorkspacePaths,
    },
    serving::{PairingFailure, ServingController},
};

/// Where a Remote's Session API lists its top-level Sessions.
const SESSIONS_PATH: &str = "/v1/sessions";

/// Where a Server's Session API lists the Workspaces it knows.
const WORKSPACES_PATH: &str = "/v1/workspaces";

/// How this Server reaches its Remotes for a read: through the Pairing, as a
/// Client's request turned toward one is carried, giving each Remote
/// `timeout` to answer.
#[derive(Clone)]
pub(crate) struct RemoteReach {
    serving: ServingController,
    timeout: Duration,
}

impl RemoteReach {
    pub(crate) fn new(serving: ServingController, timeout: Duration) -> Self {
        Self { serving, timeout }
    }

    /// The Remotes this Server is paired with, by name, in the order paired.
    fn names(&self) -> Vec<String> {
        self.serving
            .list_remotes()
            .into_iter()
            .map(|remote| remote.name)
            .collect()
    }

    /// Whether the Remote `name` answers now, asked as a Client's probe of it
    /// asks; `None` where it is no longer paired at all.
    async fn answers(&self, name: &str) -> Option<Result<(), SilentRemote>> {
        let silence = match tokio::time::timeout(self.timeout, self.serving.probe_remote(name))
            .await
        {
            Err(_) => Silence::TimedOut(self.timeout),
            Ok(Err(failure)) if failure.code == SessionErrorCode::RemoteNotFound => return None,
            Ok(Err(failure)) => Silence::Failed(failure.message),
            Ok(Ok(health)) => match health.status {
                RemoteStatus::Available => return Some(Ok(())),
                RemoteStatus::Unavailable => Silence::Unreachable,
                RemoteStatus::Revoked => Silence::Revoked,
                RemoteStatus::ProtocolMismatch => Silence::ProtocolMismatch,
            },
        };
        Some(Err(SilentRemote::new(name, silence)))
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
        let exchange = async {
            let response = self.serving.proxy_remote(name, request).await?;
            let status = response.status();
            Ok::<_, PairingFailure>((
                status,
                axum::body::to_bytes(response.into_body(), usize::MAX).await,
            ))
        };
        let silent = |silence| RemoteReadFailure::from(SilentRemote::new(name, silence));
        let (status, body) = match tokio::time::timeout(self.timeout, exchange).await {
            Err(_) => return Err(silent(Silence::TimedOut(self.timeout))),
            Ok(Err(failure)) => {
                return Err(match failure.code {
                    SessionErrorCode::RemoteNotFound => {
                        OriginRefusal::UnknownRemote(name.to_owned()).into()
                    }
                    SessionErrorCode::PairingAuthenticationFailed => silent(Silence::Revoked),
                    _ => silent(Silence::Unreachable),
                });
            }
            // The Remote stopped answering partway through what it said.
            Ok(Ok((_, Err(_)))) => return Err(silent(Silence::Unreachable)),
            Ok(Ok((status, Ok(body)))) => (status, body),
        };
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|_| {
                silent(Silence::Failed(
                    "it answered with what this server could not read".to_owned(),
                ))
            });
        }
        let code = serde_json::from_slice::<SessionError>(&body)
            .ok()
            .map(|error| error.code);
        Err(match code {
            Some(SessionErrorCode::SessionNotFound) => RemoteReadFailure::SessionNotFound,
            Some(SessionErrorCode::PairingProtocolMismatch) => silent(Silence::ProtocolMismatch),
            Some(SessionErrorCode::PairingAuthenticationFailed) => silent(Silence::Revoked),
            // The Remote could not reach its own Session API for the request.
            Some(SessionErrorCode::PairingConnectionFailed) => silent(Silence::Unreachable),
            _ => silent(Silence::Failed(failed_with(status))),
        })
    }
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
}

/// Why a Remote gave a read nothing.
#[derive(Debug)]
enum Silence {
    /// It could not be reached at any address it was paired at.
    Unreachable,
    /// It was reached, but said nothing within the reach timeout.
    TimedOut(Duration),
    /// It refused this Server's key: its user ended the Pairing on their
    /// side.
    Revoked,
    /// It speaks another version of the protocol Servers speak to each other.
    ProtocolMismatch,
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

/// Why a read at an Origin was refused before anything was read.
#[derive(Debug)]
pub(crate) enum OriginRefusal {
    /// This Server is paired with no Remote by the name given.
    UnknownRemote(String),
    /// The Remote named did not answer.
    Silent(SilentRemote),
}

/// Why a Remote gave a read of it nothing.
#[derive(Debug)]
enum RemoteReadFailure {
    Origin(OriginRefusal),
    /// The Remote holds no Session by the identity asked for.
    SessionNotFound,
}

impl From<OriginRefusal> for RemoteReadFailure {
    fn from(refusal: OriginRefusal) -> Self {
        Self::Origin(refusal)
    }
}

impl From<SilentRemote> for RemoteReadFailure {
    fn from(silent: SilentRemote) -> Self {
        Self::Origin(OriginRefusal::Silent(silent))
    }
}

impl RemoteReadFailure {
    /// The failure as one of a listing, which asks for no Session by its
    /// identity and so is never told there is none.
    fn of_listing(self, name: &str) -> OriginRefusal {
        match self {
            Self::Origin(refusal) => refusal,
            Self::SessionNotFound => OriginRefusal::Silent(SilentRemote::new(
                name,
                Silence::Failed("it said it holds no such Session".to_owned()),
            )),
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

/// One Session as its Origin holds it.
#[derive(Debug)]
pub(crate) struct SessionAtOrigin {
    pub(crate) snapshot: SessionSnapshot,
    /// The Session as its Origin lists it: every Session this Server holds,
    /// and a Remote's top-level Session. `None` for a Session a Remote lists
    /// nowhere, as a Subagent's, which only its own snapshot describes.
    pub(crate) listed: Option<SessionListItem>,
}

impl SessionOperations {
    /// Every Remote this Server is paired with, by name, in the order paired,
    /// and whether each answers now, saying why not of one that does not.
    /// Every Remote is asked at once, each given the reach timeout.
    pub(crate) async fn remote_answers(&self) -> Vec<(String, Result<(), SilentRemote>)> {
        let names = self.remotes.names();
        let answers = join_all(names.iter().map(|name| self.remotes.answers(name))).await;
        names
            .into_iter()
            .zip(answers)
            // A Remote removed while it was asked is no longer paired.
            .filter_map(|(name, answer)| answer.map(|answer| (name, answer)))
            .collect()
    }

    /// The top-level Sessions at `origins`, as each Origin lists them.
    pub(crate) async fn sessions_in(
        &self,
        origins: &Origins,
    ) -> Result<Gathered<Vec<SessionListItem>>, OriginRefusal> {
        self.gather(origins, || self.sessions.list(None), SESSIONS_PATH)
            .await
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
    /// the Session API brings one — a Remote's by its own Session API, as a
    /// Client turned toward it reads it — with how that Origin lists it.
    pub(crate) async fn session_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
    ) -> Result<SessionAtOrigin, SessionReadRefusal> {
        match origin {
            Outlook::Local => self.session_here(session_id).await,
            Outlook::Remote(name) => self.remote_session(name, session_id).await,
        }
    }

    async fn session_here(
        &self,
        session_id: SessionId,
    ) -> Result<SessionAtOrigin, SessionReadRefusal> {
        if let Err(error) = self.sessions.hydrate(session_id).await {
            tracing::warn!(%session_id, "a Session read for a Sidekick could not be loaded: {error}");
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
        Ok(SessionAtOrigin {
            snapshot,
            listed: Some(SessionListItem::Readable(Box::new(summary))),
        })
    }

    /// The Session `session_id` on the Remote `name`: its snapshot, as the
    /// Remote's Session API answers a Client opening it, and its row in the
    /// Remote's own listing, both asked at once.
    async fn remote_session(
        &self,
        name: &str,
        session_id: SessionId,
    ) -> Result<SessionAtOrigin, SessionReadRefusal> {
        let session_path = format!("{SESSIONS_PATH}/{session_id}");
        let (snapshot, listing) = tokio::join!(
            self.remotes.get::<SessionSnapshot>(name, &session_path),
            self.remotes
                .get::<Vec<SessionListItem>>(name, SESSIONS_PATH),
        );
        let snapshot = match snapshot {
            Ok(snapshot) => Some(snapshot),
            Err(RemoteReadFailure::SessionNotFound) => None,
            Err(RemoteReadFailure::Origin(refusal)) => {
                return Err(SessionReadRefusal::Origin(refusal));
            }
        };
        let listed = listing
            .map_err(|failure| SessionReadRefusal::Origin(failure.of_listing(name)))?
            .into_iter()
            .find(|listed| listed.id() == session_id);
        match (snapshot, listed) {
            (Some(snapshot), listed) => Ok(SessionAtOrigin {
                snapshot,
                listed: listed.filter(|listed| listed.readable().is_some()),
            }),
            (None, Some(listed)) if listed.readable().is_none() => {
                Err(SessionReadRefusal::Unreadable)
            }
            (None, _) => Err(SessionReadRefusal::NotFound),
        }
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
        let names = match origins {
            Origins::One(Outlook::Local) => {
                return Ok(Gathered {
                    answered: vec![(Outlook::Local, here())],
                    unanswered: Vec::new(),
                });
            }
            Origins::One(Outlook::Remote(name)) => {
                let read = self
                    .remotes
                    .get(name, path)
                    .await
                    .map_err(|failure| failure.of_listing(name))?;
                return Ok(Gathered {
                    answered: vec![(Outlook::Remote(name.clone()), read)],
                    unanswered: Vec::new(),
                });
            }
            Origins::Everywhere => self.remotes.names(),
        };
        let mut gathered = Gathered {
            answered: vec![(Outlook::Local, here())],
            unanswered: Vec::new(),
        };
        let reads = join_all(names.iter().map(|name| self.remotes.get(name, path))).await;
        for (name, read) in names.into_iter().zip(reads) {
            match read.map_err(|failure| failure.of_listing(&name)) {
                Ok(read) => gathered.answered.push((Outlook::Remote(name), read)),
                Err(OriginRefusal::Silent(silent)) => gathered.unanswered.push(silent),
                // A Remote removed while it was asked takes its rows with it,
                // as one whose Pairing has ended leaves Everywhere.
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
            said(Silence::Failed(failed_with(
                StatusCode::INTERNAL_SERVER_ERROR
            ))),
            "The Remote `laptop` could not answer: it answered with an error, 500 Internal Server \
             Error."
        );
        for silence in [
            Silence::Unreachable,
            Silence::TimedOut(Duration::from_secs(1)),
            Silence::Revoked,
            Silence::ProtocolMismatch,
        ] {
            let sentence = said(silence);
            assert!(
                sentence.starts_with("The Remote `laptop`") && sentence.ends_with('.'),
                "{sentence}"
            );
        }
    }
}
