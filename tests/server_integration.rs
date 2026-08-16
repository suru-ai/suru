use chidori::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{Health, LifecycleState},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn authenticated_health_describes_the_ready_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(ServerConfig::new(state_dir.path(), "health-test"))
        .await
        .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    assert!(descriptor.base_url.starts_with("http://127.0.0.1:"));
    assert_ne!(descriptor.base_url, "http://127.0.0.1:0");

    let missing_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .send()
        .await
        .expect("request health without authentication");
    assert_eq!(missing_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let wrong_auth = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("request health with incorrect authentication");
    assert_eq!(wrong_auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let health = client
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request authenticated health")
        .error_for_status()
        .expect("authenticated health succeeds")
        .json::<Health>()
        .await
        .expect("decode health response");

    assert_eq!(health.instance_id, descriptor.instance_id);
    assert_eq!(health.pid, std::process::id());
    assert_eq!(health.lifecycle, LifecycleState::Ready);
    assert_eq!(health.protocol_version, descriptor.protocol_version);
    assert_eq!(health.build_identity, descriptor.build_identity);

    let missing_event_auth = client
        .get(format!("{}/v1/events", descriptor.base_url))
        .send()
        .await
        .expect("request event stream without authentication");
    assert_eq!(
        missing_event_auth.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let descriptor_mode =
            std::fs::metadata(ServerConfig::new(state_dir.path(), "health-test").descriptor_path())
                .expect("read runtime descriptor metadata")
                .permissions()
                .mode()
                & 0o777;
        assert_eq!(descriptor_mode, 0o600);

        let directory_mode = std::fs::metadata(state_dir.path().join("health-test"))
            .expect("read runtime directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_receives_snapshot_before_absolute_counter_updates() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(ServerConfig::new(state_dir.path(), "events-test"))
        .await
        .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client =
        ManagedClient::connect(ManagedClientConfig::new(state_dir.path(), "events-test"))
            .await
            .expect("connect managed client");

    let connected = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("connected event arrives")
        .expect("managed client remains open");
    let ManagedEvent::Connected(identity) = connected else {
        panic!("expected connected event, got {connected:?}");
    };
    assert_eq!(identity.instance_id, descriptor.instance_id);
    assert_eq!(identity.pid, descriptor.pid);

    let snapshot = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("snapshot arrives")
        .expect("managed client remains open");
    let ManagedEvent::Snapshot(snapshot) = snapshot else {
        panic!("expected snapshot event, got {snapshot:?}");
    };
    assert_eq!(snapshot.instance_id, descriptor.instance_id);
    assert_eq!(snapshot.value, 0);
    assert_eq!(snapshot.revision, 0);

    let update = timeout(Duration::from_secs(2), client.next())
        .await
        .expect("counter update arrives")
        .expect("managed client remains open");
    let ManagedEvent::CounterUpdated(update) = update else {
        panic!("expected counter update event, got {update:?}");
    };
    assert_eq!(update.value, 1);
    assert_eq!(update.revision, 1);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn counter_advances_without_connected_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(ServerConfig::new(state_dir.path(), "idle-counter-test"))
        .await
        .expect("spawn server");

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let mut client = ManagedClient::connect(ManagedClientConfig::new(
        state_dir.path(),
        "idle-counter-test",
    ))
    .await
    .expect("connect after server has run without clients");
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::Connected(_))
    ));
    let snapshot = client.next().await.expect("receive counter snapshot");
    let ManagedEvent::Snapshot(snapshot) = snapshot else {
        panic!("expected snapshot event, got {snapshot:?}");
    };
    assert!(snapshot.value >= 1);
    assert_eq!(snapshot.value, snapshot.revision);

    drop(client);
    server.shutdown().await.expect("shut down server");
}
