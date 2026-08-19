use std::{
    convert::Infallible,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::{get, post},
};
use chidori::{
    build_identity,
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, start_server,
    },
    protocol::{
        Health, LifecycleState, PROTOCOL_VERSION, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT,
        ServerIdentity, ServerShutdown, ShutdownReason,
    },
    server::{self, ServerConfig},
};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use sysinfo::{Pid, System};
use tokio::sync::{oneshot, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

mod support;

use support::{
    read_runtime_descriptor, receive_initial_state, request_server_shutdown,
    write_runtime_descriptor,
};

fn chidori_binary_build_identity() -> String {
    build_identity::for_executable(env!("CARGO_BIN_EXE_chidori"))
        .expect("identify the tested Chidori executable")
}

fn inert_server_executable(state_dir: &std::path::Path) -> PathBuf {
    let executable = state_dir.join("must-not-spawn");
    if !executable.exists() {
        std::fs::write(&executable, b"inert server fixture executable")
            .expect("write inert server fixture executable");
    }
    executable
}

fn inert_server_build_identity(state_dir: &std::path::Path) -> String {
    build_identity::for_executable(inert_server_executable(state_dir))
        .expect("identify inert server fixture executable")
}

#[tokio::test]
async fn launching_current_build_replaces_an_authenticated_mismatched_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "build-replacement-test";
    let fixture =
        BuildReplacementFixture::spawn(state_dir.path(), channel, "chidori@old-build").await;
    let previous = fixture.descriptor();
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure replacement launcher")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let replacement = start_server(&config)
        .await
        .expect("replace mismatched server build");

    assert_ne!(replacement.instance_id, previous.instance_id);
    assert_ne!(replacement.pid, previous.pid);
    assert_eq!(replacement.build_identity, chidori_binary_build_identity());
    let shutdown = fixture
        .shutdown_request()
        .expect("old server receives replacement request");
    assert_eq!(shutdown.instance_id, previous.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Replacement);

    let mut client = ManagedClient::connect(config)
        .await
        .expect("connect to replacement server");
    let identity = receive_initial_state(&mut client).await;
    assert_eq!(identity.instance_id, replacement.instance_id);

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn attached_client_reconnects_to_the_replacement() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "attached-build-replacement-test";
    let old_executable = inert_server_executable(state_dir.path());
    let old_build_identity = inert_server_build_identity(state_dir.path());
    let fixture =
        BuildReplacementFixture::spawn(state_dir.path(), channel, &old_build_identity).await;
    let previous = fixture.descriptor();
    let old_config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure old-build managed client")
        .with_server_executable(&old_executable);
    let mut attached = ManagedClient::connect(old_config.clone())
        .await
        .expect("attach to the original server");
    let mut also_attached = ManagedClient::connect(old_config)
        .await
        .expect("attach another client to the original server");
    let original_identity = receive_initial_state(&mut attached).await;
    let also_original_identity = receive_initial_state(&mut also_attached).await;
    assert_eq!(original_identity.instance_id, previous.instance_id);
    assert_eq!(also_original_identity.instance_id, previous.instance_id);

    let current_config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure current-build managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));
    let replacement = start_server(&current_config)
        .await
        .expect("launch current build replacement");
    let (first_reconnected, second_reconnected) = tokio::join!(
        receive_recovered_state(&mut attached, previous.instance_id),
        receive_recovered_state(&mut also_attached, previous.instance_id),
    );
    let reconnected = first_reconnected;
    let also_reconnected = second_reconnected;

    assert_eq!(reconnected.instance_id, replacement.instance_id);
    assert_eq!(also_reconnected.instance_id, replacement.instance_id);
    assert_eq!(
        fixture
            .shutdown_request()
            .expect("old server receives shutdown")
            .reason,
        ShutdownReason::Replacement
    );

    let mut current = ManagedClient::connect(current_config)
        .await
        .expect("attach a current-build client to the replacement");
    let current_identity = receive_initial_state(&mut current).await;
    assert_eq!(current_identity.instance_id, replacement.instance_id);
    drop(also_attached);

    stop_test_server(state_dir.path(), channel);
    let (old_recovery, current_recovery) = tokio::join!(
        receive_recovered_state(&mut attached, replacement.instance_id),
        receive_recovered_state(&mut current, replacement.instance_id),
    );
    let restarted = old_recovery;
    let current_restarted = current_recovery;
    assert_ne!(restarted.instance_id, replacement.instance_id);
    assert_eq!(restarted.instance_id, current_restarted.instance_id);
    assert_eq!(restarted.build_identity, replacement.build_identity);

    drop(attached);
    drop(current);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn attached_old_client_surfaces_a_strict_fatal_error_for_an_incompatible_replacement() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "incompatible-build-replacement-test";
    let inert_build_identity = inert_server_build_identity(state_dir.path());
    let original =
        BuildReplacementFixture::spawn(state_dir.path(), channel, &inert_build_identity).await;
    let descriptor = original.descriptor();
    let mut attached = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure attached client")
            .with_server_executable(inert_server_executable(state_dir.path())),
    )
    .await
    .expect("attach to original server");
    receive_initial_state(&mut attached).await;

    let response = request_server_shutdown(&descriptor, ShutdownReason::Replacement).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    timeout(Duration::from_secs(1), async {
        while !original.is_stopped() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original server releases the channel");
    let incompatible = BuildReplacementFixture::spawn_with_protocol(
        state_dir.path(),
        channel,
        "chidori@incompatible-build",
        PROTOCOL_VERSION + 1,
    )
    .await;

    let recovering = timeout(Duration::from_secs(2), attached.next())
        .await
        .expect("old client processes replacement intent promptly")
        .expect("managed client remains open");
    assert!(matches!(
        recovering,
        ManagedEvent::Recovering(RecoveryStatus {
            attempt: 1,
            retry_in: Duration::ZERO,
        })
    ));

    let fatal = timeout(Duration::from_secs(2), async {
        loop {
            match attached.next().await.expect("managed client remains open") {
                ManagedEvent::Recovering(_) => {}
                ManagedEvent::Fatal(error) => break error,
                event => panic!("expected replacement recovery or fatal error, got {event:?}"),
            }
        }
    })
    .await
    .expect("incompatible replacement is rejected promptly");
    assert!(fatal.contains("replacement server protocol version"));
    assert!(fatal.contains("incompatible"));

    drop(incompatible);
}

#[tokio::test]
async fn launcher_replaces_a_build_and_protocol_mismatch_before_connecting() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "incompatible-registered-build-test";
    let mismatched = BuildReplacementFixture::spawn_with_protocol(
        state_dir.path(),
        channel,
        "chidori@old-incompatible-build",
        PROTOCOL_VERSION - 1,
    )
    .await;
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure current launcher")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let previous = mismatched.descriptor();
    let mut client = ManagedClient::connect(config)
        .await
        .expect("replace the stale build and connect the launching client");
    let replacement = receive_initial_state(&mut client).await;

    assert_ne!(replacement.instance_id, previous.instance_id);
    assert_ne!(replacement.pid, previous.pid);
    assert_eq!(replacement.build_identity, chidori_binary_build_identity());
    assert_eq!(replacement.protocol_version, PROTOCOL_VERSION);
    let shutdown = mismatched
        .shutdown_request()
        .expect("stale server receives authenticated replacement request");
    assert_eq!(shutdown.instance_id, previous.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Replacement);
    assert!(
        mismatched.is_stopped(),
        "launcher returned before the exact stale instance released its channel lock"
    );

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn launcher_rejects_a_protocol_incompatible_matching_build_without_replacing_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "incompatible-matching-build-test";
    let incompatible = BuildReplacementFixture::spawn_with_protocol(
        state_dir.path(),
        channel,
        &chidori_binary_build_identity(),
        PROTOCOL_VERSION - 1,
    )
    .await;
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure current launcher")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let error = start_server(&config)
        .await
        .expect_err("matching build with an incompatible protocol cannot be reused")
        .to_string();

    assert!(error.contains("registered Chidori server protocol version"));
    assert!(error.contains("incompatible"));
    assert!(incompatible.shutdown_request().is_none());
}

#[tokio::test]
async fn simultaneous_replacement_launchers_converge_on_one_new_instance() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "replacement-race-test";
    let fixture =
        BuildReplacementFixture::spawn(state_dir.path(), channel, "chidori@old-build").await;
    let previous_instance_id = fixture.descriptor().instance_id;
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure replacement launchers")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));
    let launchers = (0..8)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { start_server(&config).await })
        })
        .collect::<Vec<_>>();

    let mut replacements = Vec::new();
    for launcher in launchers {
        replacements.push(
            launcher
                .await
                .expect("replacement launcher does not panic")
                .expect("replacement launcher succeeds"),
        );
    }
    let winner = replacements.first().expect("at least one replacement");
    assert_ne!(winner.instance_id, previous_instance_id);
    assert!(replacements.iter().all(|replacement| {
        replacement.instance_id == winner.instance_id && replacement.pid == winner.pid
    }));

    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn launcher_retries_after_losing_election_to_a_mismatched_build() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "mixed-build-election-race-test";
    let runtime_dir = state_dir.path().join(channel);
    let lock = BuildReplacementFixture::acquire_channel_lock(state_dir.path(), channel);
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure current launcher")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));
    let launching = tokio::spawn({
        let config = config.clone();
        async move { start_server(&config).await }
    });
    let log_path = runtime_dir.join("server.log");
    timeout(Duration::from_secs(2), async {
        loop {
            if std::fs::read_to_string(&log_path)
                .is_ok_and(|log| log.contains("another server already owns this channel"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("current child loses the first election");

    let mismatched = BuildReplacementFixture::spawn_with_lock(
        state_dir.path(),
        channel,
        "chidori@other-racing-build",
        PROTOCOL_VERSION,
        lock,
    )
    .await;
    let replacement = timeout(Duration::from_secs(3), launching)
        .await
        .expect("launcher completes after replacing election winner")
        .expect("launcher task does not panic")
        .expect("launcher retries its current build after the transition");

    assert_ne!(replacement.instance_id, mismatched.descriptor().instance_id);
    assert_eq!(replacement.build_identity, chidori_binary_build_identity());
    assert_eq!(
        mismatched
            .shutdown_request()
            .expect("mismatched election winner is replaced")
            .reason,
        ShutdownReason::Replacement
    );

    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn mismatched_build_in_another_channel_is_not_replaced() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let old_channel = "isolated-old-build";
    let current_channel = "isolated-current-build";
    let old =
        BuildReplacementFixture::spawn(state_dir.path(), old_channel, "chidori@old-build").await;
    let current = start_server(
        &ManagedClientConfig::new(state_dir.path(), current_channel)
            .expect("configure isolated channel")
            .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
    )
    .await
    .expect("start current build in another channel");

    assert_eq!(current.build_identity, chidori_binary_build_identity());
    assert!(old.shutdown_request().is_none());
    let old_health = reqwest::Client::new()
        .get(format!("{}/health", old.descriptor().base_url))
        .bearer_auth(&old.descriptor().token)
        .send()
        .await
        .expect("old channel remains reachable");
    assert_eq!(old_health.status(), reqwest::StatusCode::OK);

    stop_test_server(state_dir.path(), current_channel);
}

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
                    .env("CHIDORI_STATE_DIR", &state_dir)
                    .env("CHIDORI_DATA_DIR", &state_dir)
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
        identities.push(receive_initial_state(client).await);
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
async fn managed_clients_recover_from_a_crash_and_converge_on_one_replacement() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "crash-recovery-test";
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));
    let mut first = ManagedClient::connect(config.clone())
        .await
        .expect("connect first managed client");
    let mut second = ManagedClient::connect(config)
        .await
        .expect("connect second managed client");
    let first_identity = receive_initial_state(&mut first).await;
    let second_identity = receive_initial_state(&mut second).await;
    assert_eq!(first_identity.instance_id, second_identity.instance_id);

    stop_test_server(state_dir.path(), channel);

    let (first_recovered, second_recovered) = tokio::join!(
        receive_recovered_state(&mut first, first_identity.instance_id),
        receive_recovered_state(&mut second, second_identity.instance_id),
    );
    assert_ne!(first_recovered.instance_id, first_identity.instance_id);
    assert_eq!(first_recovered.instance_id, second_recovered.instance_id);
    assert_eq!(first_recovered.pid, second_recovered.pid);

    drop(first);
    drop(second);
    stop_test_server(state_dir.path(), channel);
}

async fn receive_recovered_state(client: &mut ManagedClient, previous_instance_id: Uuid) -> Health {
    timeout(Duration::from_secs(10), async {
        let mut saw_recovering = false;
        loop {
            match client.next().await.expect("managed client remains open") {
                ManagedEvent::Recovering(_) => saw_recovering = true,
                ManagedEvent::Connected(identity) if saw_recovering => {
                    assert_ne!(identity.instance_id, previous_instance_id);
                    return identity;
                }
                ManagedEvent::Fatal(error) => panic!("managed client recovery failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("managed client recovers from the crashed server")
}

#[tokio::test]
async fn recovery_backoff_is_exponential_and_capped_at_five_seconds() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "recovery-backoff-test";
    let _fixture = ReadinessFixture::spawn_recovery_backoff(state_dir.path(), channel).await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path())),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut client).await;

    let mut previous_nonzero_wait = None;
    let mut observed_cap = false;
    for expected_attempt in 1..=10 {
        let event = timeout(
            previous_nonzero_wait.unwrap_or(Duration::ZERO) + Duration::from_secs(2),
            client.next(),
        )
        .await
        .expect("next recovery state arrives")
        .expect("managed client remains open");
        let ManagedEvent::Recovering(status) = event else {
            panic!("expected recovering event, got {event:?}");
        };
        assert_eq!(status.attempt, expected_attempt);
        assert!(status.retry_in <= Duration::from_secs(5));
        if expected_attempt == 1 {
            assert!(status.retry_in.is_zero());
        } else if let Some(previous) = previous_nonzero_wait {
            assert_eq!(
                status.retry_in,
                previous.saturating_mul(2).min(Duration::from_secs(5))
            );
        } else {
            assert!(!status.retry_in.is_zero());
        }
        if status.retry_in == Duration::from_secs(5) {
            observed_cap = true;
            break;
        }
        if !status.retry_in.is_zero() {
            previous_nonzero_wait = Some(status.retry_in);
        }
    }

    assert!(
        observed_cap,
        "recovery backoff never reached its five-second cap"
    );
}

#[tokio::test]
async fn dropping_a_recovering_client_cancels_its_next_network_attempt_promptly() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "recovery-cancellation-test";
    let fixture = ReadinessFixture::spawn_recovery_backoff(state_dir.path(), channel).await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path())),
    )
    .await
    .expect("connect managed client");
    receive_initial_state(&mut client).await;

    let first = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("first recovery state arrives")
        .expect("managed client remains open");
    let ManagedEvent::Recovering(status) = first else {
        panic!("expected recovering event, got {first:?}");
    };
    assert_eq!(status.attempt, 1);
    assert_eq!(status.retry_in, Duration::ZERO);
    let second = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("scheduled retry state arrives")
        .expect("managed client remains open");
    let ManagedEvent::Recovering(status) = second else {
        panic!("expected recovering event, got {second:?}");
    };
    assert_eq!(status.attempt, 2);
    assert_eq!(status.retry_in, Duration::from_millis(50));
    let requests_before_drop = fixture.event_requests();

    let drop_started = std::time::Instant::now();
    drop(client);
    assert!(drop_started.elapsed() < Duration::from_millis(100));
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(fixture.event_requests(), requests_before_drop);
}

#[tokio::test]
async fn managed_client_surfaces_protocol_corruption_as_ordered_fatal_events() {
    for (channel, violation, expected_error) in [
        (
            "malformed-event-json-test",
            ProtocolViolation::MalformedJson,
            "decode server shutdown intent",
        ),
        (
            "unknown-event-tag-test",
            ProtocolViolation::UnknownEvent,
            "unknown event type 'future_event'",
        ),
        (
            "event-instance-identity-test",
            ProtocolViolation::UnexpectedInstance,
            "unexpected server instance",
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let _fixture =
            ReadinessFixture::spawn_protocol_violation(state_dir.path(), channel, violation).await;
        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel)
                .expect("configure managed client")
                .with_server_executable(inert_server_executable(state_dir.path())),
        )
        .await
        .expect("connect managed client before fixture corruption is decoded");

        assert!(matches!(
            client.next().await,
            Some(ManagedEvent::Connecting)
        ));
        assert!(matches!(
            client.next().await,
            Some(ManagedEvent::Connected(_))
        ));
        let fatal = match timeout(Duration::from_secs(1), client.next())
            .await
            .expect("protocol corruption becomes a prompt fatal event")
            .expect("managed client remains open")
        {
            ManagedEvent::Fatal(error) => error,
            ManagedEvent::Recovering(status) => {
                panic!("protocol corruption was retried: {status:?}")
            }
            event => panic!("unexpected event before fatal failure: {event:?}"),
        };

        assert!(
            fatal.contains(expected_error),
            "expected {expected_error:?} in fatal error, got {fatal:?}"
        );
        assert!(client.next().await.is_none());
    }
}

#[tokio::test]
async fn authenticated_shutdown_intent_does_not_trigger_crash_recovery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "shutdown-intent-test";
    let fixture = ReadinessFixture::spawn_shutdown_intent(state_dir.path(), channel).await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path())),
    )
    .await
    .expect("connect managed client");
    let identity = receive_initial_state(&mut client).await;

    let shutdown = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("shutdown intent arrives")
        .expect("managed client reports shutdown intent");
    let ManagedEvent::ServerShutdown(shutdown) = shutdown else {
        panic!("expected shutdown intent, got {shutdown:?}");
    };
    assert_eq!(shutdown.instance_id, identity.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Manual);
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(None)
    ));
    assert_eq!(fixture.event_requests(), 1);
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
            .arg("--data-dir")
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
            identity: ServerIdentity {
                instance_id: Uuid::new_v4(),
                pid: unrelated.0.id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity: "stale-build".to_owned(),
            },
        },
    );

    let stop = run_server_cli(state_dir.path(), channel, "stop").await;
    assert!(!stop.status.success());
    assert!(
        unrelated
            .0
            .try_wait()
            .expect("inspect unrelated process after refused stop")
            .is_none(),
        "server stop terminated a process based only on stale metadata"
    );

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
    )
    .await
    .expect("recover from stale descriptor");
    let identity = receive_initial_state(&mut client).await;

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
        let identity = receive_initial_state(&mut client).await;
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
        let identity = receive_initial_state(&mut client).await;
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
async fn server_status_reports_authenticated_ready_and_missing_states() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "status-ready-test";

    let missing = run_server_cli(state_dir.path(), channel, "status").await;
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing"),
        "missing status was not identified: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let started = run_server_cli(state_dir.path(), channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let descriptor = read_runtime_descriptor(state_dir.path().join(channel).join("runtime.json"));

    let ready = run_server_cli(state_dir.path(), channel, "status").await;
    assert!(
        ready.status.success(),
        "ready status failed: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(stdout.contains("ready"));
    assert!(stdout.contains(&descriptor.pid.to_string()));
    assert!(stdout.contains(&descriptor.instance_id.to_string()));

    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn server_status_distinguishes_lifecycle_stale_and_unreachable_registrations() {
    for (channel, lifecycle, expected) in [
        ("status-starting-test", LifecycleState::Starting, "starting"),
        ("status-stopping-test", LifecycleState::Stopping, "stopping"),
        ("status-failed-test", LifecycleState::Failed, "failed"),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let _fixture = ReadinessFixture::spawn(state_dir.path(), channel, lifecycle).await;

        let output = run_server_cli(state_dir.path(), channel, "status").await;
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "expected {expected} status, got: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stale_state_dir = tempfile::tempdir().expect("create isolated state directory");
    let stale_channel = "status-stale-test";
    let _fixture =
        ReadinessFixture::spawn(stale_state_dir.path(), stale_channel, LifecycleState::Ready).await;
    let stale_path = stale_state_dir
        .path()
        .join(stale_channel)
        .join("runtime.json");
    let mut stale = read_runtime_descriptor(&stale_path);
    stale.instance_id = Uuid::new_v4();
    write_runtime_descriptor(&stale_path, &stale);
    let stale_output = run_server_cli(stale_state_dir.path(), stale_channel, "status").await;
    assert!(!stale_output.status.success());
    assert!(
        String::from_utf8_lossy(&stale_output.stderr).contains("stale"),
        "stale registration was not identified: {}",
        String::from_utf8_lossy(&stale_output.stderr)
    );

    let unreachable_state_dir = tempfile::tempdir().expect("create isolated state directory");
    let unreachable_channel = "status-unreachable-test";
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("reserve an unused loopback address");
    let unreachable_url = format!(
        "http://{}",
        listener.local_addr().expect("read unused address")
    );
    drop(listener);
    let runtime_dir = unreachable_state_dir.path().join(unreachable_channel);
    std::fs::create_dir_all(&runtime_dir).expect("create unreachable runtime directory");
    write_runtime_descriptor(
        runtime_dir.join("runtime.json"),
        &RuntimeDescriptor {
            base_url: unreachable_url,
            token: "unreachable-token".to_owned(),
            identity: ServerIdentity {
                instance_id: Uuid::new_v4(),
                pid: u32::MAX,
                protocol_version: PROTOCOL_VERSION,
                build_identity: "unreachable-build".to_owned(),
            },
        },
    );
    let unreachable_output =
        run_server_cli(unreachable_state_dir.path(), unreachable_channel, "status").await;
    assert!(!unreachable_output.status.success());
    assert!(
        String::from_utf8_lossy(&unreachable_output.stderr).contains("unreachable"),
        "unreachable registration was not identified: {}",
        String::from_utf8_lossy(&unreachable_output.stderr)
    );
}

#[tokio::test]
async fn server_stop_notifies_attached_clients_and_remains_stopped() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "cli-manual-stop-test";
    let started = run_server_cli(state_dir.path(), channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let descriptor_path = state_dir.path().join(channel).join("runtime.json");
    let descriptor = read_runtime_descriptor(&descriptor_path);
    let mut managed = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure attached managed client")
            .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
    )
    .await
    .expect("attach managed client before manual stop");
    receive_initial_state(&mut managed).await;

    let stopped = run_server_cli(state_dir.path(), channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let stdout = String::from_utf8_lossy(&stopped.stdout);
    assert!(stdout.contains("stopped"));
    assert!(stdout.contains(&descriptor.pid.to_string()));
    assert!(stdout.contains(&descriptor.instance_id.to_string()));

    let shutdown = match timeout(Duration::from_secs(1), managed.next())
        .await
        .expect("attached client receives manual stop intent")
    {
        Some(ManagedEvent::ServerShutdown(shutdown)) => shutdown,
        Some(ManagedEvent::Recovering(status)) => {
            panic!("manual stop triggered recovery: {status:?}")
        }
        Some(event) => panic!("expected manual shutdown intent, got {event:?}"),
        None => panic!("attached client closed before manual intent"),
    };
    assert_eq!(shutdown.instance_id, descriptor.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Manual);
    assert!(matches!(
        timeout(Duration::from_secs(1), managed.next()).await,
        Ok(None)
    ));

    assert!(!descriptor_path.exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !descriptor_path.exists(),
        "manual stop was undone by immediate recovery"
    );
    let status = run_server_cli(state_dir.path(), channel, "status").await;
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("missing"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn fatal_protocol_error_restores_the_terminal_and_exits_without_input() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "fatal-tui-protocol-test";
    let fixture = ReadinessFixture::spawn_binary_protocol_violation(
        state_dir.path(),
        channel,
        ProtocolViolation::UnknownEvent,
    )
    .await;
    let binary = env!("CARGO_BIN_EXE_chidori").replace('\'', "'\\''");
    let tui_command = format!(
        "stty rows 24 cols 80; original_stty=$(stty -g); '{binary}'; chidori_status=$?; restored_stty=$(stty -g); if [ \"$original_stty\" = \"$restored_stty\" ]; then printf '\\n__CHIDORI_STTY_RESTORED__\\n'; else printf '\\n__CHIDORI_STTY_CHANGED__:%s:%s\\n' \"$original_stty\" \"$restored_stty\"; fi; exit $chidori_status"
    );
    let mut tui = AttachedTuiGuard(Some(
        Command::new("script")
            .args(["-qef", "/dev/null", "-c", &tui_command])
            .env("CHIDORI_STATE_DIR", state_dir.path())
            .env("CHIDORI_DATA_DIR", state_dir.path())
            .env("CHIDORI_CHANNEL", channel)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch attached TUI in a pseudo-terminal"),
    ));

    timeout(Duration::from_secs(2), async {
        while !fixture.events_opened.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TUI opens the corrupt event stream");
    timeout(Duration::from_secs(2), async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fatal TUI exits promptly without user input");

    let output = tui.wait_with_output();
    assert!(!output.status.success(), "fatal TUI reports failure");
    let screen = String::from_utf8_lossy(&output.stdout);
    let alternate_screen_exit_position = screen
        .find("\u{1b}[?1049l")
        .expect("fatal TUI leaves the alternate screen");
    let cursor_restore_position = screen
        .find("\u{1b}[?25h")
        .expect("fatal TUI restores the cursor");
    let bracketed_paste_enable_position = screen
        .find("\u{1b}[?2004h")
        .expect("fatal TUI enabled bracketed paste while it was active");
    let bracketed_paste_disable_position = screen
        .find("\u{1b}[?2004l")
        .expect("fatal TUI disables bracketed paste on exit");
    let error_detail_position = screen
        .find("unknown event type 'future_event'")
        .expect("fatal protocol detail remains visible");
    assert!(
        alternate_screen_exit_position < error_detail_position,
        "fatal error was reported before leaving the alternate screen: {screen:?}"
    );
    assert!(
        cursor_restore_position < error_detail_position,
        "fatal error was reported before restoring the cursor: {screen:?}"
    );
    assert!(
        bracketed_paste_enable_position < bracketed_paste_disable_position
            && bracketed_paste_disable_position < error_detail_position,
        "fatal TUI did not bracket its active lifetime with paste mode: {screen:?}"
    );
    assert!(
        screen.contains("__CHIDORI_STTY_RESTORED__"),
        "fatal TUI did not restore its original terminal mode: {screen:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn attached_tui_restores_its_terminal_and_exits_on_manual_stop() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "attached-tui-manual-stop-test";
    let started = run_server_cli(state_dir.path(), channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let mut tui = AttachedTuiGuard::spawn(state_dir.path(), channel);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tui.is_running(),
        "attached TUI exited before the manual stop"
    );

    let stopped = run_server_cli(state_dir.path(), channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    timeout(Duration::from_secs(2), async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("attached TUI exits after manual stop");

    let output = tui.wait_with_output();
    assert!(
        output.status.success(),
        "attached TUI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let screen = String::from_utf8_lossy(&output.stdout);
    assert!(
        screen.contains("Shared server stopped intentionally"),
        "attached TUI did not render the manual-stop state: {screen:?}"
    );
    assert!(
        screen.contains("\u{1b}[?1049l"),
        "attached TUI did not leave the alternate screen"
    );
    assert!(
        screen.contains("\u{1b}[?25h"),
        "attached TUI did not restore the cursor"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn clean_tui_exit_restores_the_terminal() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "clean-tui-exit-test";
    let started = run_server_cli(state_dir.path(), channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let binary = AttachedTuiGuard::escaped_binary();
    let tui_command = format!(
        "stty rows 24 cols 80; original_stty=$(stty -g); '{binary}'; chidori_status=$?; restored_stty=$(stty -g); if [ \"$original_stty\" = \"$restored_stty\" ]; then printf '\\n__CHIDORI_STTY_RESTORED__\\n'; else printf '\\n__CHIDORI_STTY_CHANGED__:%s:%s\\n' \"$original_stty\" \"$restored_stty\"; fi; exit $chidori_status"
    );
    let mut tui = AttachedTuiGuard::spawn_shell_command(state_dir.path(), channel, &tui_command);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(tui.is_running(), "TUI exited before clean-exit input");
    tui.child_mut()
        .stdin
        .as_mut()
        .expect("TUI stdin remains open")
        .write_all(b"\x03")
        .expect("send Ctrl+C to the empty composer");
    timeout(Duration::from_secs(2), async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TUI exits cleanly after Ctrl+C");

    let output = tui.wait_with_output();
    assert!(
        output.status.success(),
        "clean TUI exit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let screen = String::from_utf8_lossy(&output.stdout);
    for sequence in [
        "\u{1b}[?1049h",
        "\u{1b}[?1049l",
        "\u{1b}[?25l",
        "\u{1b}[?25h",
    ] {
        assert!(
            screen.contains(sequence),
            "missing terminal sequence {sequence:?}"
        );
    }
    assert!(screen.contains("\u{1b}[?2004h"));
    assert!(screen.contains("\u{1b}[?2004l"));
    assert!(
        screen.contains("__CHIDORI_STTY_RESTORED__"),
        "clean exit did not restore raw mode: {screen:?}"
    );

    stop_test_server(state_dir.path(), channel);
}

#[cfg(target_os = "linux")]
struct AttachedTuiGuard(Option<Child>);

#[cfg(target_os = "linux")]
impl AttachedTuiGuard {
    fn spawn(state_dir: &std::path::Path, channel: &str) -> Self {
        let binary = Self::escaped_binary();
        let tui_command = format!("stty rows 24 cols 80; exec '{binary}'");
        Self::spawn_shell_command(state_dir, channel, &tui_command)
    }

    fn escaped_binary() -> String {
        env!("CARGO_BIN_EXE_chidori").replace('\'', "'\\''")
    }

    fn spawn_shell_command(state_dir: &std::path::Path, channel: &str, tui_command: &str) -> Self {
        Self(Some(
            Command::new("script")
                .args(["-qef", "/dev/null", "-c", tui_command])
                .env("CHIDORI_STATE_DIR", state_dir)
                .env("CHIDORI_DATA_DIR", state_dir)
                .env("CHIDORI_CHANNEL", channel)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("launch attached TUI in a pseudo-terminal"),
        ))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("attached TUI process")
    }

    fn is_running(&mut self) -> bool {
        self.child_mut()
            .try_wait()
            .expect("inspect attached TUI")
            .is_none()
    }

    fn wait_with_output(&mut self) -> std::process::Output {
        self.0
            .take()
            .expect("attached TUI process")
            .wait_with_output()
            .expect("collect attached TUI output")
    }
}

#[cfg(target_os = "linux")]
impl Drop for AttachedTuiGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn run_server_cli(
    state_dir: &std::path::Path,
    channel: &str,
    command: &str,
) -> std::process::Output {
    let state_dir = state_dir.to_path_buf();
    let channel = channel.to_owned();
    let command = command.to_owned();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_chidori"))
            .args(["server", &command])
            .env("CHIDORI_STATE_DIR", &state_dir)
            .env("CHIDORI_DATA_DIR", &state_dir)
            .env("CHIDORI_CHANNEL", channel)
            .output()
            .expect("run server CLI command")
    })
    .await
    .expect("server CLI command task does not panic")
}

#[tokio::test]
async fn server_start_returns_after_a_detached_server_is_ready() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "detached-start-test";
    let output = Command::new(env!("CARGO_BIN_EXE_chidori"))
        .args(["server", "start"])
        .env("CHIDORI_STATE_DIR", state_dir.path())
        .env("CHIDORI_DATA_DIR", state_dir.path())
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
        .env("CHIDORI_DATA_DIR", state_dir.path())
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
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(env!("CARGO_BIN_EXE_chidori")),
    )
    .await
    .expect("connect after start command has exited");
    let identity = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());
    assert_eq!(identity.pid, first_descriptor.pid);
    assert_eq!(identity.instance_id, first_descriptor.instance_id);

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[test]
fn build_profile_selects_isolated_default_state_and_data_roots() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let (expected_channel, other_channel) = if cfg!(debug_assertions) {
        ("debug", "release")
    } else {
        ("release", "debug")
    };
    let output = Command::new(env!("CARGO_BIN_EXE_chidori"))
        .args(["server", "start"])
        .env("CHIDORI_STATE_DIR", state_dir.path())
        .env("CHIDORI_DATA_DIR", data_dir.path())
        .env_remove("CHIDORI_CHANNEL")
        .output()
        .expect("start server on the build profile's default channel");

    assert!(
        output.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected_state_root = test_runtime_root(state_dir.path(), expected_channel);
    let expected_data_root = test_runtime_root(data_dir.path(), expected_channel);
    assert!(expected_state_root.join("runtime.json").exists());
    assert!(expected_data_root.exists());
    assert!(!state_dir.path().join(other_channel).exists());
    assert!(!data_dir.path().join(other_channel).exists());

    stop_test_server(state_dir.path(), expected_channel);
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

    let identity = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());

    drop(client);
    stop_test_server(state_dir.path(), channel);
}

#[tokio::test]
async fn sequential_managed_clients_reuse_the_persistent_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "managed-reuse-test";
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(env!("CARGO_BIN_EXE_chidori"));

    let mut first = ManagedClient::connect(config.clone())
        .await
        .expect("connect first managed client");
    let first_identity = receive_initial_state(&mut first).await;
    drop(first);

    let mut second = ManagedClient::connect(config)
        .await
        .expect("connect second managed client");
    let second_identity = receive_initial_state(&mut second).await;

    assert_eq!(second_identity.pid, first_identity.pid);
    assert_eq!(second_identity.instance_id, first_identity.instance_id);

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
            .with_server_executable(inert_server_executable(state_dir.path()));

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
        let identity = receive_initial_state(&mut client).await;
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
        .with_server_executable(inert_server_executable(state_dir.path()));

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
        .with_server_executable(inert_server_executable(state_dir.path()));

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
    event_requests: Arc<AtomicUsize>,
    event_behavior: FixtureEventBehavior,
}

#[derive(Clone, Copy)]
enum FixtureEventBehavior {
    StayConnected,
    DisconnectAfterConnected,
    ShutdownAfterConnected,
    ProtocolViolation(ProtocolViolation),
}

#[derive(Clone, Copy)]
enum ProtocolViolation {
    MalformedJson,
    UnknownEvent,
    UnexpectedInstance,
}

struct ReadinessFixture {
    lifecycle: Arc<Mutex<LifecycleState>>,
    events_opened: Arc<AtomicBool>,
    events_ready: Arc<AtomicBool>,
    event_requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ReadinessFixture {
    async fn spawn(
        state_dir: &std::path::Path,
        channel: &str,
        initial_lifecycle: LifecycleState,
    ) -> Self {
        Self::spawn_with_event_behavior(
            state_dir,
            channel,
            initial_lifecycle,
            FixtureEventBehavior::StayConnected,
        )
        .await
    }

    async fn spawn_recovery_backoff(state_dir: &std::path::Path, channel: &str) -> Self {
        Self::spawn_with_event_behavior(
            state_dir,
            channel,
            LifecycleState::Ready,
            FixtureEventBehavior::DisconnectAfterConnected,
        )
        .await
    }

    async fn spawn_shutdown_intent(state_dir: &std::path::Path, channel: &str) -> Self {
        Self::spawn_with_event_behavior(
            state_dir,
            channel,
            LifecycleState::Ready,
            FixtureEventBehavior::ShutdownAfterConnected,
        )
        .await
    }

    async fn spawn_protocol_violation(
        state_dir: &std::path::Path,
        channel: &str,
        violation: ProtocolViolation,
    ) -> Self {
        Self::spawn_with_event_behavior(
            state_dir,
            channel,
            LifecycleState::Ready,
            FixtureEventBehavior::ProtocolViolation(violation),
        )
        .await
    }

    async fn spawn_binary_protocol_violation(
        state_dir: &std::path::Path,
        channel: &str,
        violation: ProtocolViolation,
    ) -> Self {
        Self::spawn_with_event_behavior_and_build(
            state_dir,
            channel,
            LifecycleState::Ready,
            FixtureEventBehavior::ProtocolViolation(violation),
            chidori_binary_build_identity(),
        )
        .await
    }

    async fn spawn_with_event_behavior(
        state_dir: &std::path::Path,
        channel: &str,
        initial_lifecycle: LifecycleState,
        event_behavior: FixtureEventBehavior,
    ) -> Self {
        Self::spawn_with_event_behavior_and_build(
            state_dir,
            channel,
            initial_lifecycle,
            event_behavior,
            inert_server_build_identity(state_dir),
        )
        .await
    }

    async fn spawn_with_event_behavior_and_build(
        state_dir: &std::path::Path,
        channel: &str,
        initial_lifecycle: LifecycleState,
        event_behavior: FixtureEventBehavior,
        build_identity: String,
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
            identity: ServerIdentity {
                instance_id,
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity,
            },
        };
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        write_runtime_descriptor(runtime_dir.join("runtime.json"), &descriptor);

        let lifecycle = Arc::new(Mutex::new(initial_lifecycle));
        let events_opened = Arc::new(AtomicBool::new(false));
        let events_ready = Arc::new(AtomicBool::new(true));
        let event_requests = Arc::new(AtomicUsize::new(0));
        let state = ReadinessState {
            descriptor,
            lifecycle: lifecycle.clone(),
            events_opened: events_opened.clone(),
            events_ready: events_ready.clone(),
            event_requests: event_requests.clone(),
            event_behavior,
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
            event_requests,
            task,
        }
    }

    fn event_requests(&self) -> usize {
        self.event_requests.load(Ordering::SeqCst)
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
    Json(state.descriptor.health(lifecycle)).into_response()
}

async fn readiness_events(State(state): State<ReadinessState>, headers: HeaderMap) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.events_opened.store(true, Ordering::SeqCst);
    let request_index = state.event_requests.fetch_add(1, Ordering::SeqCst);
    if !state.events_ready.load(Ordering::SeqCst) {
        return std::future::pending().await;
    }
    if *state.lifecycle.lock().expect("lock lifecycle") != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let instance_id = state.descriptor.instance_id;
    let connected = || {
        stream::once(std::future::ready(Ok::<_, Infallible>(
            Event::default().comment("connected"),
        )))
    };
    match state.event_behavior {
        FixtureEventBehavior::StayConnected => {
            Sse::new(connected().chain(stream::pending())).into_response()
        }
        FixtureEventBehavior::DisconnectAfterConnected if request_index > 0 => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        FixtureEventBehavior::DisconnectAfterConnected => Sse::new(connected()).into_response(),
        FixtureEventBehavior::ShutdownAfterConnected => {
            let shutdown = ServerShutdown {
                instance_id,
                reason: ShutdownReason::Manual,
            };
            let shutdown_event = stream::once(async move {
                Ok::<_, Infallible>(
                    Event::default()
                        .event(SERVER_SHUTDOWN_EVENT)
                        .json_data(shutdown)
                        .expect("serialize shutdown intent"),
                )
            });
            Sse::new(connected().chain(shutdown_event)).into_response()
        }
        FixtureEventBehavior::ProtocolViolation(violation) => {
            let events = protocol_violation_events(&state, violation)
                .into_iter()
                .map(Ok::<_, Infallible>);
            Sse::new(stream::iter(events)).into_response()
        }
    }
}

fn protocol_violation_events(_state: &ReadinessState, violation: ProtocolViolation) -> Vec<Event> {
    match violation {
        ProtocolViolation::MalformedJson => {
            vec![Event::default().event(SERVER_SHUTDOWN_EVENT).data("{")]
        }
        ProtocolViolation::UnknownEvent => {
            vec![Event::default().event("future_event").data("{}")]
        }
        ProtocolViolation::UnexpectedInstance => vec![
            Event::default()
                .event(SERVER_SHUTDOWN_EVENT)
                .json_data(ServerShutdown {
                    instance_id: Uuid::new_v4(),
                    reason: ShutdownReason::Manual,
                })
                .expect("serialize shutdown intent for an unexpected server"),
        ],
    }
}

fn fixture_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

#[derive(Clone)]
struct BuildReplacementState {
    descriptor: Arc<Mutex<RuntimeDescriptor>>,
    lifecycle: Arc<Mutex<LifecycleState>>,
    shutdown_intent: watch::Sender<Option<ServerShutdown>>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    shutdown_request: Arc<Mutex<Option<ServerShutdown>>>,
}

struct BuildReplacementFixture {
    descriptor: Arc<Mutex<RuntimeDescriptor>>,
    shutdown_request: Arc<Mutex<Option<ServerShutdown>>>,
    task: tokio::task::JoinHandle<()>,
}

impl BuildReplacementFixture {
    async fn spawn(state_dir: &std::path::Path, channel: &str, build_identity: &str) -> Self {
        Self::spawn_with_protocol(state_dir, channel, build_identity, PROTOCOL_VERSION).await
    }

    async fn spawn_with_protocol(
        state_dir: &std::path::Path,
        channel: &str,
        build_identity: &str,
        protocol_version: u32,
    ) -> Self {
        let lock = Self::acquire_channel_lock(state_dir, channel);
        Self::spawn_with_lock(state_dir, channel, build_identity, protocol_version, lock).await
    }

    fn acquire_channel_lock(state_dir: &std::path::Path, channel: &str) -> File {
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(runtime_dir.join("server.lock"))
            .expect("open old-build channel lock");
        lock.try_lock_exclusive()
            .expect("old-build fixture owns the channel");
        lock
    }

    async fn spawn_with_lock(
        state_dir: &std::path::Path,
        channel: &str,
        build_identity: &str,
        protocol_version: u32,
        lock: File,
    ) -> Self {
        let runtime_dir = state_dir.join(channel);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind old-build fixture");
        let descriptor = RuntimeDescriptor {
            base_url: format!(
                "http://{}",
                listener.local_addr().expect("read old-build address")
            ),
            token: "old-build-fixture-token".to_owned(),
            identity: ServerIdentity {
                instance_id: Uuid::new_v4(),
                pid: u32::MAX - 1,
                protocol_version,
                build_identity: build_identity.to_owned(),
            },
        };
        let descriptor_path = runtime_dir.join("runtime.json");
        write_runtime_descriptor(&descriptor_path, &descriptor);

        let descriptor = Arc::new(Mutex::new(descriptor));
        let lifecycle = Arc::new(Mutex::new(LifecycleState::Ready));
        let (shutdown_intent, _) = watch::channel(None);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let shutdown = Arc::new(Mutex::new(Some(shutdown_tx)));
        let shutdown_request = Arc::new(Mutex::new(None));
        let state = BuildReplacementState {
            descriptor: descriptor.clone(),
            lifecycle,
            shutdown_intent,
            shutdown,
            shutdown_request: shutdown_request.clone(),
        };
        let app = Router::new()
            .route("/health", get(build_replacement_health))
            .route("/v1/events", get(build_replacement_events))
            .route("/v1/server/stop", post(build_replacement_stop))
            .with_state(state);
        let instance_id = descriptor
            .lock()
            .expect("lock old-build descriptor")
            .instance_id;
        let task_descriptor_path = descriptor_path.clone();
        let task = tokio::spawn(async move {
            let _lock = lock;
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve old-build fixture");
            if task_descriptor_path.exists()
                && read_runtime_descriptor(&task_descriptor_path).instance_id == instance_id
            {
                std::fs::remove_file(task_descriptor_path)
                    .expect("remove old-build runtime descriptor");
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        });

        Self {
            descriptor,
            shutdown_request,
            task,
        }
    }

    fn descriptor(&self) -> RuntimeDescriptor {
        self.descriptor
            .lock()
            .expect("lock old-build descriptor")
            .clone()
    }

    fn shutdown_request(&self) -> Option<ServerShutdown> {
        self.shutdown_request
            .lock()
            .expect("lock old-build shutdown request")
            .clone()
    }

    fn is_stopped(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for BuildReplacementFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn build_replacement_health(
    State(state): State<BuildReplacementState>,
    headers: HeaderMap,
) -> Response {
    let descriptor = state
        .descriptor
        .lock()
        .expect("lock old-build descriptor")
        .clone();
    if !fixture_authenticated(&headers, &descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(descriptor.health(state.lifecycle.lock().expect("lock lifecycle").clone())).into_response()
}

async fn build_replacement_events(
    State(state): State<BuildReplacementState>,
    headers: HeaderMap,
) -> Response {
    let descriptor = state
        .descriptor
        .lock()
        .expect("lock old-build descriptor")
        .clone();
    if !fixture_authenticated(&headers, &descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let first =
        stream::once(async move { Ok::<_, Infallible>(Event::default().comment("connected")) });
    let shutdowns = stream::unfold(
        state.shutdown_intent.subscribe(),
        |mut shutdown_intent| async move {
            shutdown_intent.changed().await.ok()?;
            let shutdown = shutdown_intent.borrow_and_update().clone()?;
            let event = Event::default()
                .event(SERVER_SHUTDOWN_EVENT)
                .json_data(shutdown)
                .expect("serialize old-build shutdown intent");
            Some((Ok::<_, Infallible>(event), shutdown_intent))
        },
    );
    Sse::new(first.chain(shutdowns)).into_response()
}

async fn build_replacement_stop(
    State(state): State<BuildReplacementState>,
    headers: HeaderMap,
    Json(request): Json<ServerShutdown>,
) -> StatusCode {
    let descriptor = state
        .descriptor
        .lock()
        .expect("lock old-build descriptor")
        .clone();
    if !fixture_authenticated(&headers, &descriptor.token) {
        return StatusCode::UNAUTHORIZED;
    }
    if request.instance_id != descriptor.instance_id {
        return StatusCode::CONFLICT;
    }
    *state
        .shutdown_request
        .lock()
        .expect("lock old-build shutdown request") = Some(request.clone());
    *state.lifecycle.lock().expect("lock lifecycle") = LifecycleState::Stopping;
    state.shutdown_intent.send_replace(Some(request));
    if let Some(shutdown) = state
        .shutdown
        .lock()
        .expect("lock old-build shutdown sender")
        .take()
    {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = shutdown.send(());
        });
    }
    StatusCode::ACCEPTED
}

fn stop_test_server(state_dir: &std::path::Path, channel: &str) {
    let descriptor_path = test_runtime_root(state_dir, channel).join("runtime.json");
    let descriptor = read_runtime_descriptor(descriptor_path);
    let mut system = System::new_all();
    system.refresh_all();
    let process = system
        .process(Pid::from_u32(descriptor.pid))
        .expect("find detached test server");
    assert!(process.kill(), "stop detached test server");
}

fn test_runtime_root(base_dir: &std::path::Path, channel: &str) -> std::path::PathBuf {
    if channel == "release" {
        base_dir.to_path_buf()
    } else {
        base_dir.join(channel)
    }
}
