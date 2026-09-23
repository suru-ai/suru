//! The per-tree Subagent stream: a snapshot of the tree a top-level Session
//! heads, then every change to it, reconnecting on its own.

use anyhow::{Context, Result, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Duration,
};

use crate::protocol::{
    Outlook, RuntimeDescriptor, SUBAGENT_TREE_SNAPSHOT_EVENT, SUBAGENT_TREE_UPDATED_EVENT,
    SessionId, SubagentTreeChange, SubagentTreeRevision, SubagentTreeSnapshot, SubagentTreeUpdate,
};

use super::{
    RecoveryBackoff,
    remote_connection::{self, RemoteConnectionFailure},
    server_url,
};

/// What a per-tree subscription tells its reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubagentTreeEvent {
    /// The whole tree, naming its top-level Session. It arrives first and
    /// again after every reconnection, and replaces whatever tree the reader
    /// held — so a change missed while disconnected is already in it.
    Snapshot(SubagentTreeSnapshot),
    /// One change to the tree the latest snapshot named, in the order the
    /// Server made them.
    Changed(SubagentTreeChange),
    /// The tree cannot be read — its Server holds no such Session, or the
    /// Remote serving it ended the Pairing — and the subscription has ended.
    Failed(String),
}

/// A reconnecting subscription to the tree one Session belongs to, on the
/// Server one Outlook names. Its lifetime is the interest: dropping it aborts
/// the task and releases the connection.
pub struct SubagentTreeSubscription {
    events: mpsc::Receiver<SubagentTreeEvent>,
    task: JoinHandle<()>,
}

impl SubagentTreeSubscription {
    pub(super) fn open(
        http: reqwest::Client,
        descriptor: watch::Receiver<RuntimeDescriptor>,
        outlook: Outlook,
        session_id: SessionId,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(run(
            http,
            descriptor,
            outlook,
            session_id,
            events_tx,
            RecoveryBackoff::new(initial_backoff, max_backoff),
        ));
        Self {
            events: events_rx,
            task,
        }
    }

    /// The next event, or `None` once the subscription has ended.
    pub async fn next(&mut self) -> Option<SubagentTreeEvent> {
        self.events.recv().await
    }
}

impl Drop for SubagentTreeSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    http: reqwest::Client,
    mut descriptor: watch::Receiver<RuntimeDescriptor>,
    outlook: Outlook,
    session_id: SessionId,
    events: mpsc::Sender<SubagentTreeEvent>,
    mut backoff: RecoveryBackoff,
) {
    loop {
        if events.is_closed() {
            return;
        }
        let active_descriptor = descriptor.borrow().clone();
        match open(&http, &active_descriptor, &outlook, session_id).await {
            Ok(response) => match consume(response, &events).await {
                StreamOutcome::ReceiverClosed => return,
                StreamOutcome::Disconnected { hydrated } => {
                    if hydrated {
                        backoff.reset();
                    }
                }
            },
            Err(
                RemoteConnectionFailure::Terminal { message, .. }
                | RemoteConnectionFailure::Rejected(message),
            ) => {
                let _ = events.send(SubagentTreeEvent::Failed(message)).await;
                return;
            }
            Err(RemoteConnectionFailure::Transient) => {}
        }
        if wait_to_reconnect(&mut descriptor, backoff.next()).await {
            backoff.reset();
        }
    }
}

async fn open(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    outlook: &Outlook,
    session_id: SessionId,
) -> std::result::Result<reqwest::Response, RemoteConnectionFailure> {
    let url = server_url(
        &descriptor.base_url,
        outlook,
        &format!("/v1/sessions/{session_id}/subagent-tree"),
    )
    .expect("a validated runtime descriptor builds a Server URL");
    remote_connection::classify(http.get(url).bearer_auth(&descriptor.token).send().await).await
}

/// Waits for the retry delay or a replacement local Server descriptor,
/// answering whether the descriptor was replaced: a new one names a new
/// Server, which starts a fresh retry schedule.
async fn wait_to_reconnect(
    descriptor: &mut watch::Receiver<RuntimeDescriptor>,
    retry_in: Duration,
) -> bool {
    tokio::select! {
        changed = descriptor.changed(), if descriptor.has_changed().is_ok() => changed.is_ok(),
        _ = tokio::time::sleep(retry_in) => false,
    }
}

enum StreamOutcome {
    /// The connection ended, cleanly or not, or said something that cannot
    /// be trusted; a fresh connection's snapshot sets it right. `hydrated`
    /// says whether a snapshot arrived first.
    Disconnected {
        hydrated: bool,
    },
    ReceiverClosed,
}

async fn consume(
    response: reqwest::Response,
    events: &mpsc::Sender<SubagentTreeEvent>,
) -> StreamOutcome {
    let mut stream = response.bytes_stream().eventsource();
    let mut revision = None;
    while let Some(next) = stream.next().await {
        let hydrated = revision.is_some();
        let event = match next {
            Ok(event) => event,
            Err(EventStreamError::Transport(_)) => return StreamOutcome::Disconnected { hydrated },
            Err(error) => {
                tracing::warn!(%error, "Subagent tree stream is unreadable");
                return StreamOutcome::Disconnected { hydrated };
            }
        };
        let event = match decode_event(event, &mut revision) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "Subagent tree stream broke its protocol");
                return StreamOutcome::Disconnected { hydrated };
            }
        };
        if events.send(event).await.is_err() {
            return StreamOutcome::ReceiverClosed;
        }
    }
    StreamOutcome::Disconnected {
        hydrated: revision.is_some(),
    }
}

fn decode_event(
    event: eventsource_stream::Event,
    revision: &mut Option<SubagentTreeRevision>,
) -> Result<SubagentTreeEvent> {
    match event.event.as_str() {
        SUBAGENT_TREE_SNAPSHOT_EVENT => {
            if revision.is_some() {
                bail!("Subagent tree stream sent more than one snapshot");
            }
            let snapshot: SubagentTreeSnapshot =
                serde_json::from_str(&event.data).context("decode Subagent tree snapshot")?;
            validate_event_id(&event.id, snapshot.revision)?;
            *revision = Some(snapshot.revision);
            Ok(SubagentTreeEvent::Snapshot(snapshot))
        }
        SUBAGENT_TREE_UPDATED_EVENT => {
            let Some(previous) = *revision else {
                bail!("Subagent tree stream sent an update before its snapshot");
            };
            let update: SubagentTreeUpdate =
                serde_json::from_str(&event.data).context("decode Subagent tree update")?;
            validate_event_id(&event.id, update.revision)?;
            if !update.revision.immediately_follows(previous) {
                bail!("Subagent tree revision sequence is discontinuous");
            }
            *revision = Some(update.revision);
            Ok(SubagentTreeEvent::Changed(update.change))
        }
        name => bail!("server sent unknown Subagent tree event type '{name}'"),
    }
}

fn validate_event_id(id: &str, revision: SubagentTreeRevision) -> Result<()> {
    if id == revision.0.to_string() {
        Ok(())
    } else {
        bail!("Subagent tree event id does not match its revision")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ActivityStatus, SubagentTreeTopLevel};

    fn event(name: &str, id: u64, data: &impl serde::Serialize) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: name.to_owned(),
            data: serde_json::to_string(data).expect("encode event data"),
            id: id.to_string(),
            retry: None,
        }
    }

    fn snapshot(revision: u64) -> SubagentTreeSnapshot {
        SubagentTreeSnapshot {
            revision: SubagentTreeRevision(revision),
            top_level: SubagentTreeTopLevel {
                session_id: SessionId::new(),
                title: "Delegate".to_owned(),
            },
            subagents: Vec::new(),
        }
    }

    fn settle(revision: u64) -> SubagentTreeUpdate {
        SubagentTreeUpdate {
            revision: SubagentTreeRevision(revision),
            change: SubagentTreeChange::SubagentSettled {
                session_id: SessionId::new(),
                status: ActivityStatus::Completed,
                duration_ms: Some(5),
            },
        }
    }

    #[test]
    fn changes_follow_their_snapshot_without_a_gap() {
        let mut revision = None;
        let opened = snapshot(4);
        assert_eq!(
            decode_event(
                event(SUBAGENT_TREE_SNAPSHOT_EVENT, 4, &opened),
                &mut revision
            )
            .expect("the snapshot opens the stream"),
            SubagentTreeEvent::Snapshot(opened)
        );
        let next = settle(5);
        assert_eq!(
            decode_event(event(SUBAGENT_TREE_UPDATED_EVENT, 5, &next), &mut revision)
                .expect("the next revision follows"),
            SubagentTreeEvent::Changed(next.change)
        );
        assert!(
            decode_event(
                event(SUBAGENT_TREE_UPDATED_EVENT, 7, &settle(7)),
                &mut revision
            )
            .is_err(),
            "a skipped revision is a missed change, which only a fresh snapshot recovers"
        );
    }

    #[test]
    fn a_change_before_any_snapshot_is_refused() {
        assert!(
            decode_event(event(SUBAGENT_TREE_UPDATED_EVENT, 2, &settle(2)), &mut None).is_err()
        );
    }
}
