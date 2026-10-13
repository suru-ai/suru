//! A Server knows whether it has been onboarded, says so in its health, and
//! tells every Client attached to it the moment a Client marks it so. These
//! tests take their Servers as they come, never onboarded, rather than as the
//! rest of the suite takes them.
use super::*;
use crate::support::PROGRESS_DEADLINE;
use suru::protocol::{ONBOARDED_EVENT, RuntimeDescriptor};

async fn health(descriptor: &RuntimeDescriptor) -> Health {
    reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read health")
        .error_for_status()
        .expect("health succeeds")
        .json::<Health>()
        .await
        .expect("decode health")
}

async fn attach(state_dir: &std::path::Path, channel: &str) -> (ManagedClient, Health) {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    let health = receive_initial_state(&mut client).await;
    (client, health)
}

/// The next time `client` hears that its Server has been onboarded.
async fn hears_onboarded(client: &mut ManagedClient) {
    loop {
        let event = timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("the onboarded broadcast arrives")
            .expect("managed client remains open");
        match event {
            ManagedEvent::Onboarded(true) => return,
            ManagedEvent::ModelCatalog(_)
            | ManagedEvent::Relays(_)
            | ManagedEvent::ServingListener(_) => continue,
            event => panic!("expected the onboarded broadcast, got {event:?}"),
        }
    }
}

#[tokio::test]
async fn a_fresh_server_is_onboarded_once_a_client_marks_it_and_stays_so_across_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let channel = "onboarding-marked";
    let config = ServerConfig::new(state_dir.path(), channel)
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let original = server::spawn_not_onboarded(config.clone())
        .await
        .expect("spawn server");
    let (mut marking, opening) = attach(state_dir.path(), channel).await;
    let (mut onlooker, _) = attach(state_dir.path(), channel).await;
    assert!(!opening.onboarded, "a fresh Server has not been onboarded");

    marking
        .mark_onboarded()
        .await
        .expect("mark the Server onboarded");
    hears_onboarded(&mut marking).await;
    hears_onboarded(&mut onlooker).await;
    assert!(
        health(original.descriptor()).await.onboarded,
        "health says so once it is marked"
    );

    drop(marking);
    drop(onlooker);
    original.shutdown().await.expect("stop original server");

    let replacement = server::spawn_not_onboarded(config)
        .await
        .expect("respawn server");
    let (client, reopening) = attach(state_dir.path(), channel).await;
    assert!(reopening.onboarded, "the fact survives a restart");
    client
        .mark_onboarded()
        .await
        .expect("marking an onboarded Server again is no error");
    drop(client);
    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

/// A Client reads its Server's health before it opens its event stream, so
/// another Client may mark the Server onboarded between the two; the stream
/// it then opens says so, so it closes what it offered all the same.
#[tokio::test]
async fn a_client_whose_health_was_read_before_another_marked_the_server_hears_it_on_attaching() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_not_onboarded(
        ServerConfig::new(state_dir.path(), "onboarding-late-attach").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    assert!(
        !health(&descriptor).await.onboarded,
        "the late Client reads that the Server is not onboarded"
    );

    crate::support::mark_onboarded(&descriptor).await;

    let stream = reqwest::Client::new()
        .get(format!("{}/v1/events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the late Client's event stream")
        .error_for_status()
        .expect("the event stream opens");
    let mut events = stream.bytes_stream().eventsource();
    let told = timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = events
                .next()
                .await
                .expect("the event stream remains open")
                .expect("the event is well formed");
            if event.event == ONBOARDED_EVENT {
                return event.data;
            }
        }
    })
    .await
    .expect("the stream says whether the Server has been onboarded");
    assert_eq!(told, "true", "the late Client hears it was onboarded");

    drop(events);
    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn an_unreadable_onboarded_record_falls_back_to_not_onboarded_without_blocking_startup() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config = ServerConfig::new(state_dir.path(), "unreadable-onboarded-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let database_path = config.data_dir().join("suru.db");
    let original = server::spawn_not_onboarded(config.clone())
        .await
        .expect("spawn server to migrate database");
    original.shutdown().await.expect("stop original server");
    seed_database(
        &database_path,
        "INSERT INTO onboarded (singleton, record) VALUES (1, 'not-json');",
    );

    let replacement = server::spawn_not_onboarded(config)
        .await
        .expect("an unreadable record must not block startup");
    assert!(
        !health(replacement.descriptor()).await.onboarded,
        "an unreadable record counts as never onboarded"
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}
