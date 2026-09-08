use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use suru::{
    managed_client::{ManagedClient, ManagedEvent},
    protocol::{
        Health, ModelCatalog, RuntimeDescriptor, SESSION_CATALOG_UPDATED_EVENT, ServerShutdown,
        SessionCatalogChange, SessionCatalogUpdate, SessionId, SessionTitleChanged, ShutdownReason,
        SkillCatalog, TitleErrand,
    },
};
use tokio::time::{Duration, timeout};

pub fn write_runtime_descriptor(path: impl AsRef<std::path::Path>, descriptor: &RuntimeDescriptor) {
    let path = path.as_ref();
    serde_json::to_writer(
        std::fs::File::create(path)
            .unwrap_or_else(|error| panic!("create runtime descriptor {path:?}: {error}")),
        descriptor,
    )
    .unwrap_or_else(|error| panic!("write runtime descriptor {path:?}: {error}"));
}

pub fn read_runtime_descriptor(path: impl AsRef<std::path::Path>) -> RuntimeDescriptor {
    let path = path.as_ref();
    serde_json::from_reader(
        std::fs::File::open(path)
            .unwrap_or_else(|error| panic!("open runtime descriptor {path:?}: {error}")),
    )
    .unwrap_or_else(|error| panic!("decode runtime descriptor {path:?}: {error}"))
}

pub async fn receive_initial_state(client: &mut ManagedClient) -> Health {
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next())
            .await
            .expect("connecting event arrives"),
        Some(ManagedEvent::Connecting)
    ));
    let connected = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("connected event arrives")
        .expect("managed client remains open");
    let ManagedEvent::Connected(identity) = connected else {
        panic!("expected connected event, got {connected:?}");
    };
    let settings = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("settings snapshot arrives")
        .expect("managed client remains open");
    assert!(
        matches!(settings, ManagedEvent::SettingsSnapshot(_)),
        "expected settings snapshot event, got {settings:?}"
    );
    receive_model_catalog(client).await;
    identity
}

/// Reads the Model Catalog that follows every connect's Settings snapshot.
/// A managed client connecting asks nothing of any Provider — a TUI does, by
/// a request of its own — so this is the whole of the connect-time catalog.
pub async fn receive_model_catalog(client: &mut ManagedClient) -> ModelCatalog {
    let event = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("Model Catalog arrives")
        .expect("managed client remains open");
    let ManagedEvent::ModelCatalog(catalog) = event else {
        panic!("expected Model Catalog event, got {event:?}");
    };
    catalog
}

pub async fn next_skill_catalog(client: &mut ManagedClient) -> SkillCatalog {
    timeout(Duration::from_secs(1), async {
        loop {
            if let Some(ManagedEvent::SkillCatalogUpdated(catalog)) = client.next().await {
                return catalog;
            }
        }
    })
    .await
    .expect("Skill Catalog update reaches attached client")
}

/// The next derived Title to reach this client, past whatever else the catalog announced first —
/// the Session being made, most of all.
pub async fn next_derived_title(client: &mut ManagedClient) -> SessionTitleChanged {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(ManagedEvent::SessionTitleChanged(changed)) = client.next().await {
                return changed;
            }
        }
    })
    .await
    .expect("a derived Title reaches the client")
}

/// Proves nothing retitled `session_id` without waiting out a deadline: the Session is deleted, and
/// the deletion is the last the client hears of it, so a Title change would have arrived in front
/// of it — among whatever else the catalog announced about a Session being made and taken away.
pub async fn assert_no_title_reaches(
    client: &mut ManagedClient,
    session_id: SessionId,
    what: &str,
) {
    client
        .delete_session(session_id)
        .await
        .expect("delete the Session");
    timeout(Duration::from_secs(10), async {
        loop {
            match client.next().await.expect("the client stays connected") {
                ManagedEvent::SessionTitleChanged(_) => panic!("{what}"),
                ManagedEvent::SessionDeleted(deleted) if deleted.session_id == session_id => break,
                _ => {}
            }
        }
    })
    .await
    .expect("the deletion reaches the client");
}

pub async fn request_server_shutdown(
    descriptor: &RuntimeDescriptor,
    reason: ShutdownReason,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/server/stop", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ServerShutdown {
            instance_id: descriptor.instance_id,
            reason,
        })
        .send()
        .await
        .expect("request authenticated server shutdown")
}

/// The raw catalog stream, so a test can count what fired on it rather than only what a client made
/// of it.
pub async fn open_catalog_stream(
    descriptor: &RuntimeDescriptor,
) -> impl futures_util::Stream<Item = SessionCatalogUpdate> + Unpin {
    let response = reqwest::Client::new()
        .get(format!("{}/v1/session-events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Session catalog stream")
        .error_for_status()
        .expect("the catalog stream authenticates");
    Box::pin(
        response
            .bytes_stream()
            .eventsource()
            .filter_map(|event| async move {
                let event = event.expect("the catalog stream stays open");
                (event.event == SESSION_CATALOG_UPDATED_EVENT).then(|| {
                    serde_json::from_str::<SessionCatalogUpdate>(&event.data)
                        .expect("decode a catalog update")
                })
            }),
    )
}

/// The next change the Session catalog stream announces, however long the
/// commits between it stream for.
pub async fn next_catalog_change(
    catalog: &mut (impl futures_util::Stream<Item = SessionCatalogUpdate> + Unpin),
) -> SessionCatalogChange {
    timeout(Duration::from_secs(5), catalog.next())
        .await
        .expect("a catalog change arrives")
        .expect("the catalog stream stays open")
        .change
}

/// Every catalog change up to and including the derived Title.
pub async fn catalog_changes_through_title(
    catalog: &mut (impl futures_util::Stream<Item = SessionCatalogUpdate> + Unpin),
) -> Vec<SessionCatalogChange> {
    timeout(Duration::from_secs(10), async {
        let mut changes = Vec::new();
        while let Some(update) = catalog.next().await {
            let done = matches!(update.change, SessionCatalogChange::TitleChanged { .. });
            changes.push(update.change);
            if done {
                return changes;
            }
        }
        changes
    })
    .await
    .expect("the derived Title reaches the catalog stream")
}

/// A config root holding one Config Document that pins `session.title.errand` to `errand`, which is
/// how the Setting reaches a spawned server.
pub fn config_root_pinning(errand: &TitleErrand) -> tempfile::TempDir {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let document = serde_json::json!({ "session": { "title": { "errand": errand } } });
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        serde_json::to_string_pretty(&document).expect("serialize the Config Document"),
    )
    .expect("write Config Document");
    config_dir
}
