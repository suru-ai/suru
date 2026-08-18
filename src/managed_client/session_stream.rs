//! Authenticated Session stream transport and provider-neutral event decoding.

use anyhow::{Context, Result, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::protocol::{
    RuntimeDescriptor, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SessionId, SessionSnapshot,
    SessionUpdate,
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

    pub fn is_recoverable(&self) -> bool {
        self.kind == SessionStreamErrorKind::Transport
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
    Snapshot(SessionSnapshot),
    Updated(SessionUpdate),
}

pub struct SessionSubscription {
    events: mpsc::Receiver<Result<SessionEvent, SessionStreamError>>,
    task: JoinHandle<()>,
}

impl SessionSubscription {
    pub(super) async fn open(
        http: &reqwest::Client,
        descriptor: &RuntimeDescriptor,
        session_id: SessionId,
    ) -> Result<Self> {
        let response = http
            .get(format!(
                "{}/v1/sessions/{session_id}/events",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send()
            .await?
            .error_for_status()
            .context("server rejected the Session event stream")?;
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(consume(response, session_id, events_tx));
        Ok(Self {
            events: events_rx,
            task,
        })
    }

    pub async fn next(&mut self) -> Option<Result<SessionEvent, SessionStreamError>> {
        self.events.recv().await
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn consume(
    response: reqwest::Response,
    session_id: SessionId,
    events: mpsc::Sender<Result<SessionEvent, SessionStreamError>>,
) {
    let mut stream = response.bytes_stream().eventsource();
    let mut saw_snapshot = false;
    let mut last_revision = None;
    while let Some(next) = stream.next().await {
        let decoded = match next {
            Ok(event) => decode_event(event, session_id, &mut saw_snapshot, &mut last_revision)
                .map_err(SessionStreamError::protocol),
            Err(error) => Err(SessionStreamError::event_source(error)),
        };
        let failed = decoded.is_err();
        if events.send(decoded).await.is_err() || failed {
            return;
        }
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
            *saw_snapshot = true;
            *last_revision = Some(snapshot.revision);
            Ok(SessionEvent::Snapshot(snapshot))
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
