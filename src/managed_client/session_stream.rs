//! Authenticated Session stream transport and provider-neutral event decoding.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::protocol::{
    Outlook, RemoteStatus, RuntimeDescriptor, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT,
    SessionId, SessionSnapshot, SessionUpdate,
};

use super::{
    RecoveryBackoff,
    remote_connection::{self, RemoteConnectionFailure},
    server_url,
};

#[derive(Debug, Eq, PartialEq)]
pub struct SessionStreamError {
    kind: SessionStreamErrorKind,
    message: String,
}

#[derive(Debug, Eq, PartialEq)]
enum SessionStreamErrorKind {
    Transport,
    Protocol,
    Remote(RemoteStatus),
}

impl SessionStreamError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: SessionStreamErrorKind::Transport,
            message: message.into(),
        }
    }

    fn protocol(error: anyhow::Error) -> Self {
        Self {
            kind: SessionStreamErrorKind::Protocol,
            message: error.to_string(),
        }
    }

    fn event_source(error: EventStreamError<reqwest::Error>) -> Self {
        let message = format!("Session event stream failed: {error}");
        match error {
            EventStreamError::Transport(_) => Self::transport(message),
            EventStreamError::Utf8(_) | EventStreamError::Parser(_) => Self {
                kind: SessionStreamErrorKind::Protocol,
                message,
            },
        }
    }

    pub(crate) fn remote(status: RemoteStatus, message: impl Into<String>) -> Self {
        Self {
            kind: SessionStreamErrorKind::Remote(status),
            message: message.into(),
        }
    }

    pub fn is_recoverable(&self) -> bool {
        self.kind == SessionStreamErrorKind::Transport
    }

    pub fn remote_status(&self) -> Option<RemoteStatus> {
        match self.kind {
            SessionStreamErrorKind::Remote(status) => Some(status),
            SessionStreamErrorKind::Transport | SessionStreamErrorKind::Protocol => None,
        }
    }
}

impl std::fmt::Display for SessionStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionStreamError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Snapshot(Box<SessionSnapshot>),
    Updated(SessionUpdate),
}

impl SessionEvent {
    pub fn snapshot(snapshot: SessionSnapshot) -> Self {
        Self::Snapshot(Box::new(snapshot))
    }
}

pub struct SessionSubscription {
    events: mpsc::Receiver<Result<SessionEvent, SessionStreamError>>,
    task: JoinHandle<()>,
}

impl SessionSubscription {
    pub(super) async fn open(
        http: &reqwest::Client,
        descriptor: &RuntimeDescriptor,
        outlook: &Outlook,
        session_id: SessionId,
    ) -> Result<Self> {
        let response = open_response(http, descriptor, outlook, session_id)
            .await
            .context("server rejected the Session event stream")?;
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(consume_once(response, session_id, events_tx));
        Ok(Self {
            events: events_rx,
            task,
        })
    }

    pub(super) async fn open_attached(
        http: &reqwest::Client,
        descriptor: watch::Receiver<RuntimeDescriptor>,
        outlook: Outlook,
        session_id: SessionId,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self> {
        let initial_descriptor = descriptor.borrow().clone();
        let response = open_response(http, &initial_descriptor, &outlook, session_id)
            .await
            .context("server rejected the Session event stream")?;
        let (events_tx, events_rx) = mpsc::channel(32);
        let target = AttachedSession {
            outlook,
            instance_id: initial_descriptor.instance_id,
            session_id,
        };
        let task = tokio::spawn(run_attached(
            http.clone(),
            descriptor,
            target,
            response,
            events_tx,
            RecoveryBackoff::new(initial_backoff, max_backoff),
        ));
        Ok(Self {
            events: events_rx,
            task,
        })
    }

    pub async fn next(&mut self) -> Option<Result<SessionEvent, SessionStreamError>> {
        self.events.recv().await
    }
}

async fn open_response(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    outlook: &Outlook,
    session_id: SessionId,
) -> std::result::Result<reqwest::Response, SessionStreamError> {
    let url = server_url(
        &descriptor.base_url,
        outlook,
        &format!("/v1/sessions/{session_id}/events"),
    )
    .expect("a validated runtime descriptor builds a Server URL");
    let response = http.get(url).bearer_auth(&descriptor.token).send().await;
    if matches!(outlook, Outlook::Remote(_)) {
        return remote_connection::classify(response)
            .await
            .map_err(|error| match error {
                RemoteConnectionFailure::Transient => {
                    SessionStreamError::transport("Remote Session stream is unavailable")
                }
                RemoteConnectionFailure::Terminal { status, message } => {
                    SessionStreamError::remote(status, message)
                }
                RemoteConnectionFailure::Rejected(message) => {
                    SessionStreamError::protocol(anyhow::anyhow!(message))
                }
            });
    }
    let response = response.map_err(|error| SessionStreamError::transport(error.to_string()))?;
    if response.status().is_success() {
        Ok(response)
    } else if response.status().is_server_error() {
        Err(SessionStreamError::transport(format!(
            "server rejected the Session event stream with {}",
            response.status()
        )))
    } else {
        Err(SessionStreamError::protocol(anyhow::anyhow!(
            "server rejected the Session event stream with {}",
            response.status()
        )))
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum StreamOutcome {
    Disconnected {
        hydrated: bool,
    },
    ReceiverClosed,
    Failed {
        error: SessionStreamError,
        hydrated: bool,
    },
}

struct AttachedSession {
    outlook: Outlook,
    instance_id: uuid::Uuid,
    session_id: SessionId,
}

async fn consume_once(
    response: reqwest::Response,
    session_id: SessionId,
    events: mpsc::Sender<Result<SessionEvent, SessionStreamError>>,
) {
    let mut last_revision = None;
    if let StreamOutcome::Failed { error, .. } =
        consume(response, session_id, &events, &mut last_revision).await
    {
        let _ = events.send(Err(error)).await;
    }
}

async fn run_attached(
    http: reqwest::Client,
    mut descriptor: watch::Receiver<RuntimeDescriptor>,
    target: AttachedSession,
    initial_response: reqwest::Response,
    events: mpsc::Sender<Result<SessionEvent, SessionStreamError>>,
    mut backoff: RecoveryBackoff,
) {
    let mut response = Some(initial_response);
    let mut last_revision = None;
    loop {
        let active_descriptor = descriptor.borrow().clone();
        if active_descriptor.instance_id != target.instance_id {
            return;
        }
        let next_response = match response.take() {
            Some(response) => Ok(response),
            None => {
                open_response(
                    &http,
                    &active_descriptor,
                    &target.outlook,
                    target.session_id,
                )
                .await
            }
        };
        let next_response = match next_response {
            Ok(response) => response,
            Err(error) if !error.is_recoverable() => {
                let _ = events.send(Err(error)).await;
                return;
            }
            Err(_) => {
                if !wait_to_reconnect(&mut descriptor, target.instance_id, backoff.next()).await {
                    return;
                }
                continue;
            }
        };
        let hydrated = match consume(
            next_response,
            target.session_id,
            &events,
            &mut last_revision,
        )
        .await
        {
            StreamOutcome::Disconnected { hydrated } => hydrated,
            StreamOutcome::ReceiverClosed => return,
            StreamOutcome::Failed { error, hydrated } if error.is_recoverable() => hydrated,
            StreamOutcome::Failed { error, .. } => {
                let _ = events.send(Err(error)).await;
                return;
            }
        };
        if hydrated {
            backoff.reset();
        }
        if !wait_to_reconnect(&mut descriptor, target.instance_id, backoff.next()).await {
            return;
        }
    }
}

async fn wait_to_reconnect(
    descriptor: &mut watch::Receiver<RuntimeDescriptor>,
    attached_instance_id: uuid::Uuid,
    retry_in: Duration,
) -> bool {
    tokio::select! {
        changed = descriptor.changed() => {
            changed.is_ok() && descriptor.borrow().instance_id == attached_instance_id
        }
        _ = tokio::time::sleep(retry_in) => {
            descriptor.borrow().instance_id == attached_instance_id
        }
    }
}

async fn consume(
    response: reqwest::Response,
    session_id: SessionId,
    events: &mpsc::Sender<Result<SessionEvent, SessionStreamError>>,
    last_revision: &mut Option<crate::protocol::SessionRevision>,
) -> StreamOutcome {
    let mut stream = response.bytes_stream().eventsource();
    let mut saw_snapshot = false;
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(event) => match decode_event(event, session_id, &mut saw_snapshot, last_revision) {
                Ok(event) => event,
                Err(error) => {
                    return StreamOutcome::Failed {
                        error: SessionStreamError::protocol(error),
                        hydrated: saw_snapshot,
                    };
                }
            },
            Err(error) => {
                return StreamOutcome::Failed {
                    error: SessionStreamError::event_source(error),
                    hydrated: saw_snapshot,
                };
            }
        };
        if events.send(Ok(event)).await.is_err() {
            return StreamOutcome::ReceiverClosed;
        }
    }
    StreamOutcome::Disconnected {
        hydrated: saw_snapshot,
    }
}

fn decode_event(
    event: eventsource_stream::Event,
    session_id: SessionId,
    saw_snapshot: &mut bool,
    last_revision: &mut Option<crate::protocol::SessionRevision>,
) -> Result<SessionEvent> {
    let event_revision = event
        .id
        .parse::<u64>()
        .context("Session event has an invalid revision ID")?;
    match event.event.as_str() {
        SESSION_SNAPSHOT_EVENT => {
            if *saw_snapshot {
                bail!("Session stream sent more than one snapshot");
            }
            let snapshot: SessionSnapshot =
                serde_json::from_str(&event.data).context("decode Session snapshot")?;
            if snapshot.session.id != session_id {
                bail!("Session snapshot came from an unexpected Session");
            }
            if snapshot.revision.0 != event_revision {
                bail!("Session snapshot revision does not match its SSE ID");
            }
            if last_revision.is_some_and(|previous| snapshot.revision < previous) {
                bail!("Session snapshot revision is not monotonic across recovery");
            }
            *saw_snapshot = true;
            *last_revision = Some(snapshot.revision);
            Ok(SessionEvent::snapshot(snapshot))
        }
        SESSION_UPDATED_EVENT => {
            if !*saw_snapshot {
                bail!("Session stream sent an update before its snapshot");
            }
            let update: SessionUpdate =
                serde_json::from_str(&event.data).context("decode Session update")?;
            if update.session_id != session_id {
                bail!("Session update came from an unexpected Session");
            }
            if update.revision.0 != event_revision {
                bail!("Session update revision does not match its SSE ID");
            }
            if !last_revision.is_some_and(|previous| update.revision.immediately_follows(previous))
            {
                bail!("Session update revision is not monotonic");
            }
            *last_revision = Some(update.revision);
            Ok(SessionEvent::Updated(update))
        }
        name => bail!("Session stream sent unknown event type '{name}'"),
    }
}
