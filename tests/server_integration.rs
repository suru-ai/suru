use chidori::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{Health, LifecycleState},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

mod support;

use support::{read_runtime_descriptor, receive_initial_state, write_runtime_descriptor};

#[tokio::test]
async fn authenticated_health_describes_the_ready_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "health-test").expect("configure server"),
    )
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

        let descriptor_mode = std::fs::metadata(
            ServerConfig::new(state_dir.path(), "health-test")
                .expect("configure server")
                .descriptor_path(),
        )
        .expect("read runtime descriptor metadata")
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(descriptor_mode, 0o600);

        let lock_mode = std::fs::metadata(state_dir.path().join("health-test/server.lock"))
            .expect("read server lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600);

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
async fn server_recovers_from_an_abandoned_partial_publication() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "partial-publication-test";
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    std::fs::write(
        runtime_dir.join(format!("runtime.{}.tmp", std::process::id())),
        b"{\"base_url\":",
    )
    .expect("seed abandoned partial publication");

    let server =
        server::spawn(ServerConfig::new(state_dir.path(), channel).expect("configure server"))
            .await
            .expect("recover from abandoned partial publication");

    let published = read_runtime_descriptor(runtime_dir.join("runtime.json"));
    assert_eq!(published.instance_id, server.descriptor().instance_id);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn server_holds_the_channel_election_lock_for_its_lifetime() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "lifetime-lock-test";
    let config = ServerConfig::new(state_dir.path(), channel).expect("configure server");
    let first = server::spawn(config.clone())
        .await
        .expect("spawn election winner");
    let first_token = first.descriptor().token.clone();

    let contender = server::spawn(config.clone())
        .await
        .err()
        .expect("a second server cannot own the same channel");
    assert!(
        contender
            .to_string()
            .contains("another server already owns")
    );

    first.shutdown().await.expect("shut down election winner");
    assert!(
        !config.descriptor_path().exists(),
        "the election winner removes its own descriptor"
    );
    let successor = server::spawn(config)
        .await
        .expect("elect a successor after the winner exits");
    assert_ne!(successor.descriptor().token, first_token);
    successor.shutdown().await.expect("shut down successor");
}

#[tokio::test]
async fn shutdown_does_not_remove_a_descriptor_owned_by_another_instance() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "ownership-cleanup-test").expect("configure server");
    let server = server::spawn(config.clone()).await.expect("spawn server");
    let mut replacement = server.descriptor().clone();
    replacement.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &replacement);

    server.shutdown().await.expect("shut down original server");

    let remaining = read_runtime_descriptor(config.descriptor_path());
    assert_eq!(remaining.instance_id, replacement.instance_id);
}

#[tokio::test]
async fn descriptor_replacement_never_exposes_a_partial_publication() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "atomic-publication-test").expect("configure server");
    let runtime_dir = state_dir.path().join("atomic-publication-test");
    std::fs::create_dir_all(&runtime_dir).expect("create runtime directory");
    let descriptor_path = config.descriptor_path();
    let stale = chidori::protocol::RuntimeDescriptor {
        base_url: "http://127.0.0.1:9".to_owned(),
        token: "stale-token".to_owned(),
        instance_id: uuid::Uuid::new_v4(),
        pid: 1,
        protocol_version: chidori::protocol::PROTOCOL_VERSION,
        build_identity: chidori::protocol::BUILD_IDENTITY.to_owned(),
    };
    write_runtime_descriptor(&descriptor_path, &stale);

    let ready = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let descriptor_path = descriptor_path.clone();
        let ready = ready.clone();
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<Vec<uuid::Uuid>, String> {
            let mut observed = Vec::new();
            let first: chidori::protocol::RuntimeDescriptor = serde_json::from_reader(
                std::fs::File::open(&descriptor_path)
                    .map_err(|error| format!("open initial descriptor: {error}"))?,
            )
            .map_err(|error| format!("decode initial descriptor: {error}"))?;
            observed.push(first.instance_id);
            ready.wait();
            while !stop.load(Ordering::SeqCst) {
                let descriptor: chidori::protocol::RuntimeDescriptor = serde_json::from_reader(
                    std::fs::File::open(&descriptor_path)
                        .map_err(|error| format!("open descriptor during publication: {error}"))?,
                )
                .map_err(|error| format!("decode descriptor during publication: {error}"))?;
                observed.push(descriptor.instance_id);
                std::thread::yield_now();
            }
            Ok(observed)
        })
    };
    ready.wait();

    let server = server::spawn(config)
        .await
        .expect("replace stale descriptor");
    tokio::time::sleep(Duration::from_millis(25)).await;
    stop.store(true, Ordering::SeqCst);
    let observed = reader
        .join()
        .expect("descriptor reader does not panic")
        .expect("every observed descriptor is complete");

    assert!(observed.contains(&stale.instance_id));
    assert!(observed.contains(&server.descriptor().instance_id));
    assert!(observed.iter().all(|instance_id| {
        *instance_id == stale.instance_id || *instance_id == server.descriptor().instance_id
    }));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_receives_snapshot_before_absolute_counter_updates() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "events-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "events-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");

    let (identity, snapshot) = receive_initial_state(&mut client).await;
    assert_eq!(identity.instance_id, descriptor.instance_id);
    assert_eq!(identity.pid, descriptor.pid);
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
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "idle-counter-test").expect("configure server"),
    )
    .await
    .expect("spawn server");

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "idle-counter-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect after server has run without clients");
    let (_, snapshot) = receive_initial_state(&mut client).await;
    assert!(snapshot.value >= 1);
    assert_eq!(snapshot.value, snapshot.revision);

    drop(client);
    server.shutdown().await.expect("shut down server");
}
