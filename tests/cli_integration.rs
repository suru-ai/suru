use std::{
    convert::Infallible,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::get,
};
use chidori::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        BUILD_IDENTITY, CounterSnapshot, Health, LifecycleState, PROTOCOL_VERSION,
        RuntimeDescriptor, SNAPSHOT_EVENT,
    },
    server::{self, ServerConfig},
};
use futures_util::{StreamExt, stream};
use sysinfo::{Pid, System};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

mod support;

use support::{read_runtime_descriptor, receive_initial_state, write_runtime_descriptor};

#[tokio::test]
async fn simultaneous_launchers_converge_on_one_authenticated_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "concurrent-election-test";
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let client_launches = (0..4)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { ManagedClient::connect(config).await })
        })
        .collect::<Vec<_>>();
    let command_launches = (0..4)
        .map(|_| {
            let state_dir = state_dir.path().to_path_buf();
            tokio::task::spawn_blocking(move || {
                Command::new(env!("CARGO_BIN_EXE_chidori"))
                    .args(["server", "start"])
                    .env("CHIDORI_STATE_DIR", state_dir)
                    .env("CHIDORI_CHANNEL", channel)
                    .output()
                    .expect("run concurrent server start command")
            })
        })
        .collect::<Vec<_>>();

    let mut clients = Vec::new();
    for launch in client_launches {
        clients.push(
            launch
                .await
                .expect("managed client launch task does not panic")
                .expect("managed client launch succeeds"),
        );
    }
    let mut command_outputs = Vec::new();
    for launch in command_launches {
        let output = launch.await.expect("server start task does not panic");
        assert!(
            output.status.success(),
            "concurrent server start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        command_outputs.push(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let mut identities = Vec::new();
    for client in &mut clients {
        identities.push(receive_initial_state(client).await.0);
    }
    let winner = identities.first().expect("at least one server identity");
    assert!(identities.iter().all(|identity| {
        identity.pid == winner.pid && identity.instance_id == winner.instance_id
    }));
    assert!(command_outputs.iter().all(|output| {
        output.contains(&winner.pid.to_string()) && output.contains(&winner.instance_id.to_string())
    }));

    drop(clients);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn stale_descriptor_pid_is_never_used_to_terminate_an_unrelated_process() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let unrelated_channel = "unrelated-live-process";
    let mut unrelated = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_chidori"))
            .arg("__server")
            .arg("--state-dir")
            .arg(state_dir.path())
            .arg("--channel")
            .arg(unrelated_channel)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated live process"),
    );
    let unrelated_descriptor = state_dir
        .path()
        .join(unrelated_channel)
        .join("runtime.json");
    timeout(Duration::from_secs(2), async {
        while !unrelated_descriptor.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unrelated live process publishes its descriptor");

    let channel = "stale-live-pid-test";
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
    write_runtime_descriptor(
        runtime_dir.join("runtime.json"),
        &RuntimeDescriptor {
            base_url: "http://127.0.0.1:9".to_owned(),
            token: "stale-token".to_owned(),
            instance_id: Uuid::new_v4(),
            pid: unrelated.0.id(),
            protocol_version: PROTOCOL_VERSION,
            build_identity: BUILD_IDENTITY.to_owned(),
        },
    );

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
    )
    .await
    .expect("recover from stale descriptor");
    let (identity, _) = receive_initial_state(&mut client).await;

    assert_ne!(identity.pid, unrelated.0.id());
    assert!(
        unrelated
            .0
            .try_wait()
            .expect("inspect unrelated live process")
            .is_none(),
        "unrelated live process was terminated from stale metadata"
    );

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn managed_client_recovers_from_malformed_and_partially_written_descriptors() {
    for (channel, stale_contents) in [
        ("malformed-descriptor-test", b"not-json".as_slice()),
        (
            "partial-descriptor-test",
            br#"{"base_url":"http://127.0.0.1:9","token":"partial""#.as_slice(),
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let runtime_dir = state_dir.path().join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
        std::fs::write(runtime_dir.join("runtime.json"), stale_contents)
            .expect("seed invalid runtime descriptor");

        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel)
                .expect("configure managed client")
                .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
        )
        .await
        .expect("recover from invalid runtime descriptor");
        let (identity, _) = receive_initial_state(&mut client).await;
        let published = read_runtime_descriptor(runtime_dir.join("runtime.json"));
        assert_eq!(published.instance_id, identity.instance_id);
        assert_eq!(published.pid, identity.pid);

        drop(client);
        stop_test_server(state_dir.path(), channel);
    }
}

#[tokio::test]
async fn reuse_requires_an_authenticated_matching_server_identity() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let decoy = server::spawn(
        ServerConfig::new(state_dir.path(), "authenticated-decoy").expect("configure decoy server"),
    )
    .await
    .expect("spawn decoy server");

    for (channel, mutate) in [
        (
            "wrong-token-registration",
            mutate_wrong_token as fn(&mut RuntimeDescriptor),
        ),
        ("wrong-identity-registration", mutate_instance_id),
    ] {
        let runtime_dir = state_dir.path().join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
        let mut stale = decoy.descriptor().clone();
        mutate(&mut stale);
        write_runtime_descriptor(runtime_dir.join("runtime.json"), &stale);

        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel)
                .expect("configure managed client")
                .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
        )
        .await
        .expect("replace unauthenticated or identity-inconsistent registration");
        let (identity, _) = receive_initial_state(&mut client).await;
        assert_ne!(identity.instance_id, decoy.descriptor().instance_id);

        drop(client);
        stop_test_server(state_dir.path(), channel);
    }

    decoy.shutdown().await.expect("shut down decoy server");
}

fn mutate_wrong_token(descriptor: &mut RuntimeDescriptor) {
    descriptor.token = "incorrect-token".to_owned();
}

fn mutate_instance_id(descriptor: &mut RuntimeDescriptor) {
    descriptor.instance_id = Uuid::new_v4();
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn server_start_returns_after_a_detached_server_is_ready() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "detached-start-test";
    let output = Command::new(env!("CARGO_BIN_EXE_chidori"))
        .args(["server", "start"])
        .env("CHIDORI_STATE_DIR", state_dir.path())
        .env("CHIDORI_CHANNEL", channel)
        .output()
        .expect("run server start command");

    assert!(
        output.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let descriptor_path = state_dir.path().join(channel).join("runtime.json");
    let first_descriptor = read_runtime_descriptor(&descriptor_path);

    let repeated = Command::new(env!("CARGO_BIN_EXE_chidori"))
        .args(["server", "start"])
        .env("CHIDORI_STATE_DIR", state_dir.path())
        .env("CHIDORI_CHANNEL", channel)
        .output()
        .expect("repeat server start command");
    assert!(
        repeated.status.success(),
        "repeated server start failed: {}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated_descriptor = read_runtime_descriptor(descriptor_path);
    assert_eq!(repeated_descriptor.pid, first_descriptor.pid);
    assert_eq!(
        repeated_descriptor.instance_id,
        first_descriptor.instance_id
    );

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure managed client"),
    )
    .await
    .expect("connect after start command has exited");
    let (identity, _) = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());
    assert_eq!(identity.pid, first_descriptor.pid);
    assert_eq!(identity.instance_id, first_descriptor.instance_id);

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn managed_client_starts_a_missing_server_before_streaming_initial_state() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "managed-auto-start-test";
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let mut client = ManagedClient::connect(config)
        .await
        .expect("connect through managed startup");

    let (identity, _) = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn sequential_managed_clients_reuse_the_persistent_advancing_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "managed-reuse-test";
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let mut first = ManagedClient::connect(config.clone())
        .await
        .expect("connect first managed client");
    let (first_identity, first_snapshot) = receive_initial_state(&mut first).await;
    drop(first);

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    let mut second = ManagedClient::connect(config)
        .await
        .expect("connect second managed client");
    let (second_identity, second_snapshot) = receive_initial_state(&mut second).await;

    assert_eq!(second_identity.pid, first_identity.pid);
    assert_eq!(second_identity.instance_id, first_identity.instance_id);
    assert_eq!(second_snapshot.instance_id, first_snapshot.instance_id);
    assert!(second_snapshot.value > first_snapshot.value);

    drop(second);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn managed_client_waits_through_transitional_lifecycle_before_opening_events() {
    for (channel, initial_lifecycle) in [
        ("starting-readiness-test", LifecycleState::Starting),
        ("stopping-readiness-test", LifecycleState::Stopping),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let fixture = ReadinessFixture::spawn(state_dir.path(), channel, initial_lifecycle).await;
        let config = ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(state_dir.path().join("must-not-spawn"));

        let connecting = tokio::spawn(ManagedClient::connect(config));
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(!connecting.is_finished());
        assert!(!fixture.events_opened.load(Ordering::SeqCst));

        *fixture.lifecycle.lock().expect("lock lifecycle") = LifecycleState::Ready;
        let mut client = timeout(Duration::from_secs(1), connecting)
            .await
            .expect("managed client notices readiness")
            .expect("managed client task does not panic")
            .expect("managed client connects");

        assert!(fixture.events_opened.load(Ordering::SeqCst));
        let (identity, _) = receive_initial_state(&mut client).await;
        assert_eq!(identity.lifecycle, LifecycleState::Ready);
    }
}

#[tokio::test]
async fn managed_client_reports_a_registered_failed_lifecycle() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "failed-readiness-test";
    let fixture = ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Failed).await;
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(state_dir.path().join("must-not-spawn"));

    let result = timeout(Duration::from_secs(1), ManagedClient::connect(config))
        .await
        .expect("failed lifecycle is reported promptly");
    let error = match result {
        Ok(_) => panic!("managed client unexpectedly connected"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("registered Chidori server reported failed startup"));
    assert!(!fixture.events_opened.load(Ordering::SeqCst));
}

#[tokio::test]
async fn managed_client_reports_a_bounded_log_tail_when_startup_fails() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "startup-failure-test";
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(runtime_dir.join("server.lock"))
        .expect("create invalid server lock directory");
    std::fs::write(
        runtime_dir.join("server.log"),
        format!("discarded-prefix\n{}", "x".repeat(16 * 1024)),
    )
    .expect("seed oversized server log");
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let result = timeout(Duration::from_secs(2), ManagedClient::connect(config))
        .await
        .expect("startup failure is reported without waiting for the full deadline");
    let error = match result {
        Ok(_) => panic!("managed client unexpectedly connected"),
        Err(error) => format!("{error:#}"),
    };

    assert!(error.contains("detached Chidori server exited before becoming ready"));
    assert!(error.contains("Recent server log"));
    assert!(error.contains("open server election lock"));
    assert!(!error.contains("discarded-prefix"));
    assert!(error.len() < 10 * 1024, "startup error was not bounded");
}

#[tokio::test]
async fn managed_client_bounds_the_initial_event_stream_handshake() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "event-handshake-timeout-test";
    let fixture = ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
    fixture.events_ready.store(false, Ordering::SeqCst);
    std::fs::write(
        state_dir.path().join(channel).join("server.log"),
        "event stream fixture stalled\n",
    )
    .expect("write fixture server log");
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(state_dir.path().join("must-not-spawn"));

    let result = timeout(Duration::from_secs(17), ManagedClient::connect(config))
        .await
        .expect("initial event stream uses the startup deadline");
    let error = match result {
        Ok(_) => panic!("managed client unexpectedly connected"),
        Err(error) => error.to_string(),
    };

    assert!(fixture.events_opened.load(Ordering::SeqCst));
    assert!(error.contains("initial event stream did not open within 15s"));
    assert!(error.contains("event stream fixture stalled"));
}

#[derive(Clone)]
struct ReadinessState {
    descriptor: RuntimeDescriptor,
    lifecycle: Arc<Mutex<LifecycleState>>,
    events_opened: Arc<AtomicBool>,
    events_ready: Arc<AtomicBool>,
}

struct ReadinessFixture {
    lifecycle: Arc<Mutex<LifecycleState>>,
    events_opened: Arc<AtomicBool>,
    events_ready: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl ReadinessFixture {
    async fn spawn(
        state_dir: &std::path::Path,
        channel: &str,
        initial_lifecycle: LifecycleState,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind readiness fixture");
        let instance_id = Uuid::new_v4();
        let descriptor = RuntimeDescriptor {
            base_url: format!(
                "http://{}",
                listener.local_addr().expect("read fixture address")
            ),
            token: "readiness-fixture-token".to_owned(),
            instance_id,
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            build_identity: BUILD_IDENTITY.to_owned(),
        };
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        write_runtime_descriptor(runtime_dir.join("runtime.json"), &descriptor);

        let lifecycle = Arc::new(Mutex::new(initial_lifecycle));
        let events_opened = Arc::new(AtomicBool::new(false));
        let events_ready = Arc::new(AtomicBool::new(true));
        let state = ReadinessState {
            descriptor,
            lifecycle: lifecycle.clone(),
            events_opened: events_opened.clone(),
            events_ready: events_ready.clone(),
        };
        let app = Router::new()
            .route("/health", get(readiness_health))
            .route("/v1/events", get(readiness_events))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve readiness fixture");
        });

        Self {
            lifecycle,
            events_opened,
            events_ready,
            task,
        }
    }
}

impl Drop for ReadinessFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn readiness_health(State(state): State<ReadinessState>, headers: HeaderMap) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let lifecycle = state.lifecycle.lock().expect("lock lifecycle").clone();
    Json(Health {
        instance_id: state.descriptor.instance_id,
        pid: state.descriptor.pid,
        lifecycle,
        protocol_version: state.descriptor.protocol_version,
        build_identity: state.descriptor.build_identity.clone(),
    })
    .into_response()
}

async fn readiness_events(State(state): State<ReadinessState>, headers: HeaderMap) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.events_opened.store(true, Ordering::SeqCst);
    if !state.events_ready.load(Ordering::SeqCst) {
        return std::future::pending().await;
    }
    if *state.lifecycle.lock().expect("lock lifecycle") != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let snapshot = CounterSnapshot {
        instance_id: state.descriptor.instance_id,
        value: 0,
        revision: 0,
    };
    let first = stream::once(async move {
        Ok::<_, Infallible>(
            Event::default()
                .event(SNAPSHOT_EVENT)
                .id("0")
                .json_data(snapshot)
                .expect("serialize fixture snapshot"),
        )
    });
    Sse::new(first.chain(stream::pending())).into_response()
}

fn fixture_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

fn stop_test_server(state_dir: &std::path::Path, channel: &str) {
    let descriptor_path = state_dir.join(channel).join("runtime.json");
    let descriptor = read_runtime_descriptor(descriptor_path);
    let mut system = System::new_all();
    system.refresh_all();
    let process = system
        .process(Pid::from_u32(descriptor.pid))
        .expect("find detached test server");
    assert!(process.kill(), "stop detached test server");
}
