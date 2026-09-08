//! The Model Catalog as a connecting client meets it: pushed on the events
//! stream, discovered live once per server process when a client asks on
//! connect, and remembered across restarts so Models are served by name
//! before any Provider has answered.

use std::sync::Arc;

use crate::{
    provider_support::{ControlledProvider, ControlledProviderRuntime},
    support::list_catalog,
};
use eventsource_stream::{Event, Eventsource};
use futures_util::StreamExt;
use suru::{
    protocol::{
        MODEL_CATALOG_EVENT, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId,
        ProviderCatalogStatus, ProviderId, RuntimeDescriptor, SETTINGS_SNAPSHOT_EVENT,
    },
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

fn named_model(model: &str, display_name: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new("controlled"),
        id: ModelId::new(model),
        display_name: display_name.to_owned(),
        description: String::new(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: Vec::new(),
    }
}

fn provider(models: Vec<ModelDescriptor>) -> (Arc<ControlledProviderRuntime>, ControlledProvider) {
    ControlledProvider::with_provider(ProviderId::new("controlled"), models)
}

async fn spawn(config: ServerConfig, runtime: Arc<ControlledProviderRuntime>) -> RunningServer {
    server::spawn_with_provider(config, runtime)
        .await
        .expect("spawn server")
}

async fn open_events(
    descriptor: &RuntimeDescriptor,
) -> impl futures_util::Stream<Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>>
{
    reqwest::Client::new()
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open event stream")
        .error_for_status()
        .expect("event stream authenticates")
        .bytes_stream()
        .eventsource()
}

/// What a TUI sends as it connects: the ask that starts discovery for every
/// Provider this server process has not yet heard from.
async fn warm(descriptor: &RuntimeDescriptor) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/models/warm", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("ask the server to warm its Model Catalog");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
}

async fn next_event(
    events: &mut (
             impl futures_util::Stream<
        Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>,
    > + Unpin
         ),
) -> Event {
    timeout(Duration::from_secs(2), events.next())
        .await
        .expect("an event arrives")
        .expect("the stream stays open")
        .expect("the event decodes")
}

async fn next_model_catalog(
    events: &mut (
             impl futures_util::Stream<
        Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>,
    > + Unpin
         ),
) -> ModelCatalog {
    loop {
        let event = next_event(events).await;
        if event.event == MODEL_CATALOG_EVENT {
            return serde_json::from_str(&event.data).expect("decode pushed Model Catalog");
        }
    }
}

#[tokio::test]
async fn connecting_pushes_the_catalog_and_asks_each_provider_once_per_process() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (runtime, _provider) = provider(vec![named_model("fast", "Fast Model")]);
    let server = spawn(
        ServerConfig::new(state_dir.path(), "catalog-push-test").expect("configure server"),
        runtime.clone(),
    )
    .await;
    let descriptor = server.descriptor().clone();

    let mut events = open_events(&descriptor).await;
    let settings = next_event(&mut events).await;
    assert_eq!(
        settings.event, SETTINGS_SNAPSHOT_EVENT,
        "the Settings snapshot still leads every stream"
    );
    let first = next_event(&mut events).await;
    assert_eq!(
        first.event, MODEL_CATALOG_EVENT,
        "the Model Catalog follows the Settings snapshot before anything else"
    );
    assert_eq!(
        runtime.model_discoveries(),
        0,
        "opening the stream alone asks nothing of a Provider"
    );
    warm(&descriptor).await;
    let settled = loop {
        let catalog = next_model_catalog(&mut events).await;
        if catalog.providers[0].status != ProviderCatalogStatus::Refreshing {
            break catalog;
        }
    };
    assert_eq!(settled.providers[0].status, ProviderCatalogStatus::Fresh);
    assert_eq!(settled.providers[0].models[0].display_name, "Fast Model");
    assert_eq!(
        runtime.model_discoveries(),
        1,
        "connecting to a server that has not asked a Provider yet asks it"
    );

    let mut again = open_events(&descriptor).await;
    let catalog = next_model_catalog(&mut again).await;
    assert_eq!(
        catalog.providers[0].status,
        ProviderCatalogStatus::Fresh,
        "a later client is shown the catalog already discovered, not a refresh"
    );
    warm(&descriptor).await;
    tokio::task::yield_now().await;
    assert_eq!(
        runtime.model_discoveries(),
        1,
        "a Provider discovered live this process is not asked again on connect"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restarted_server_serves_the_remembered_catalog_before_the_provider_answers() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let (runtime, _provider) = provider(vec![named_model("fast", "Fast Model")]);
    let server = spawn(
        ServerConfig::new(state_dir.path(), "catalog-restart-test")
            .expect("configure original server")
            .with_data_dir(data_dir.path()),
        runtime,
    )
    .await;
    let descriptor = server.descriptor().clone();
    let mut events = open_events(&descriptor).await;
    warm(&descriptor).await;
    loop {
        let catalog = next_model_catalog(&mut events).await;
        if catalog.providers[0].status == ProviderCatalogStatus::Fresh {
            break;
        }
    }
    drop(events);
    server.shutdown().await.expect("shut down original server");

    let (runtime, _provider) = provider(vec![named_model("fast", "Renamed Model")]);
    let release = runtime.block_next_model_discovery();
    let restarted = spawn(
        ServerConfig::new(state_dir.path(), "catalog-restart-test")
            .expect("configure restarted server")
            .with_data_dir(data_dir.path()),
        runtime.clone(),
    )
    .await;
    let descriptor = restarted.descriptor().clone();

    let mut events = open_events(&descriptor).await;
    let remembered = next_model_catalog(&mut events).await;
    assert_eq!(
        remembered.providers[0].models[0].display_name, "Fast Model",
        "the catalog remembered from the last process is served by name at once"
    );
    assert_eq!(remembered.providers[0].status, ProviderCatalogStatus::Fresh);
    warm(&descriptor).await;
    let listed = list_catalog(&descriptor).await;
    assert_eq!(
        listed.providers[0].status,
        ProviderCatalogStatus::Refreshing,
        "a client connecting asks the Provider for its live answer"
    );
    assert_eq!(
        listed.providers[0].models[0].display_name, "Fast Model",
        "the remembered Models are served while that answer is awaited"
    );

    release.send(()).expect("release the blocked discovery");
    let live = loop {
        let catalog = next_model_catalog(&mut events).await;
        if catalog.providers[0].status == ProviderCatalogStatus::Fresh {
            break catalog;
        }
    };
    assert_eq!(
        live.providers[0].models[0].display_name, "Renamed Model",
        "the Provider's live answer replaces what was remembered"
    );

    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}
