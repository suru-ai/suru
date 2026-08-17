//! Authenticated SSE transport and strict ordered protocol decoding.

use anyhow::{Context, Result, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::protocol::{
    COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, RuntimeDescriptor,
    SERVER_SHUTDOWN_EVENT, SNAPSHOT_EVENT, ServerShutdown, ShutdownReason,
};

use super::ManagedEvent;

pub(super) async fn open(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
) -> reqwest::Result<reqwest::Response> {
    http.get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await?
        .error_for_status()
}

pub(super) enum StreamOutcome {
    Disconnected,
    Lagged,
    ManualShutdown,
    Replacement { instance_id: uuid::Uuid },
    ReceiverClosed,
}

#[derive(Default)]
pub(super) struct StreamProtocolState {
    instance_id: Option<uuid::Uuid>,
    last_revision: Option<u64>,
}

impl StreamProtocolState {
    pub(super) async fn consume(
        &mut self,
        response: reqwest::Response,
        events: &mpsc::Sender<ManagedEvent>,
        expected_instance_id: uuid::Uuid,
    ) -> Result<StreamOutcome> {
        self.begin_stream(expected_instance_id);
        let mut stream = response.bytes_stream().eventsource();
        let mut saw_snapshot = false;
        while let Some(next) = stream.next().await {
            let event = match next {
                Ok(event) => event,
                Err(EventStreamError::Transport(_)) => return Ok(StreamOutcome::Disconnected),
                Err(error) => bail!("server event stream failed: {error}"),
            };
            let managed_event = decode_event(
                event,
                expected_instance_id,
                &mut saw_snapshot,
                &mut self.last_revision,
            )?;
            if let ManagedEvent::ServerShutdown(shutdown) = &managed_event
                && shutdown.reason == ShutdownReason::Replacement
            {
                return Ok(StreamOutcome::Replacement {
                    instance_id: shutdown.instance_id,
                });
            }
            let manual_shutdown = matches!(managed_event, ManagedEvent::ServerShutdown(_));
            if matches!(managed_event, ManagedEvent::CounterUpdated(_)) {
                match events.try_send(managed_event) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => return Ok(StreamOutcome::Lagged),
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Ok(StreamOutcome::ReceiverClosed);
                    }
                }
            } else if events.send(managed_event).await.is_err() {
                return Ok(StreamOutcome::ReceiverClosed);
            }
            if manual_shutdown {
                return Ok(StreamOutcome::ManualShutdown);
            }
        }
        Ok(StreamOutcome::Disconnected)
    }

    fn begin_stream(&mut self, instance_id: uuid::Uuid) {
        if self.instance_id != Some(instance_id) {
            self.instance_id = Some(instance_id);
            self.last_revision = None;
        }
    }
}

fn decode_event(
    event: Event,
    expected_instance_id: uuid::Uuid,
    saw_snapshot: &mut bool,
    last_revision: &mut Option<u64>,
) -> Result<ManagedEvent> {
    let event_revision = event
        .id
        .parse::<u64>()
        .context("server event has an invalid revision ID")?;
    match event.event.as_str() {
        SNAPSHOT_EVENT => {
            if *saw_snapshot {
                bail!("server sent more than one snapshot");
            }
            let snapshot: CounterSnapshot =
                serde_json::from_str(&event.data).context("decode counter snapshot")?;
            if snapshot.instance_id != expected_instance_id {
                bail!("counter snapshot came from an unexpected server instance");
            }
            if snapshot.revision != event_revision {
                bail!("counter snapshot revision does not match its SSE ID");
            }
            if last_revision.is_some_and(|previous| snapshot.revision < previous) {
                bail!("counter snapshot revision is not monotonic across recovery");
            }
            *saw_snapshot = true;
            *last_revision = Some(snapshot.revision);
            Ok(ManagedEvent::Snapshot(snapshot))
        }
        COUNTER_UPDATED_EVENT => {
            if !*saw_snapshot {
                bail!("server sent a counter update before its snapshot");
            }
            let update: CounterUpdate =
                serde_json::from_str(&event.data).context("decode counter update")?;
            if update.revision != event_revision {
                bail!("counter update revision does not match its SSE ID");
            }
            if last_revision.is_some_and(|previous| update.revision <= previous) {
                bail!("counter update revision is not monotonic");
            }
            *last_revision = Some(update.revision);
            Ok(ManagedEvent::CounterUpdated(update))
        }
        SERVER_SHUTDOWN_EVENT => {
            let shutdown: ServerShutdown =
                serde_json::from_str(&event.data).context("decode server shutdown intent")?;
            if shutdown.instance_id != expected_instance_id {
                bail!("shutdown intent came from an unexpected server instance");
            }
            Ok(ManagedEvent::ServerShutdown(shutdown))
        }
        name => bail!("server sent unknown event type '{name}'"),
    }
}
