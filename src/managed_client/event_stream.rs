//! Authenticated SSE transport and strict ordered protocol decoding.

use anyhow::{Context, Result, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::protocol::{
    MODEL_CATALOG_EVENT, ModelCatalog, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT,
    SETTINGS_SNAPSHOT_EVENT, ServerShutdown, SettingsSnapshot, ShutdownReason,
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
    ManualShutdown,
    Replacement { instance_id: uuid::Uuid },
    ReceiverClosed,
}

pub(super) async fn consume(
    response: reqwest::Response,
    events: &mpsc::Sender<ManagedEvent>,
    expected_instance_id: uuid::Uuid,
) -> Result<StreamOutcome> {
    let mut stream = response.bytes_stream().eventsource();
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(event) => event,
            Err(EventStreamError::Transport(_)) => return Ok(StreamOutcome::Disconnected),
            Err(error) => bail!("server event stream failed: {error}"),
        };
        let managed_event = decode_event(event, expected_instance_id)?;
        if let ManagedEvent::ServerShutdown(shutdown) = &managed_event
            && shutdown.reason == ShutdownReason::Replacement
        {
            return Ok(StreamOutcome::Replacement {
                instance_id: shutdown.instance_id,
            });
        }
        let manual_shutdown = matches!(managed_event, ManagedEvent::ServerShutdown(_));
        if events.send(managed_event).await.is_err() {
            return Ok(StreamOutcome::ReceiverClosed);
        }
        if manual_shutdown {
            return Ok(StreamOutcome::ManualShutdown);
        }
    }
    Ok(StreamOutcome::Disconnected)
}

fn decode_event(event: Event, expected_instance_id: uuid::Uuid) -> Result<ManagedEvent> {
    match event.event.as_str() {
        SETTINGS_SNAPSHOT_EVENT => {
            let snapshot: SettingsSnapshot =
                serde_json::from_str(&event.data).context("decode settings snapshot")?;
            Ok(ManagedEvent::SettingsSnapshot(snapshot))
        }
        MODEL_CATALOG_EVENT => {
            let catalog: ModelCatalog =
                serde_json::from_str(&event.data).context("decode Model Catalog")?;
            Ok(ManagedEvent::ModelCatalog(catalog))
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
