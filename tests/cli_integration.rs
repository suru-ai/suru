use crate::support::PROGRESS_DEADLINE;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::{
    convert::Infallible,
    fs::{File, OpenOptions},
    path::PathBuf,
    process::{Command, Stdio},
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
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use suru::{
    build_identity,
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, RecoveryStatus, StoppedDuringLaunch,
        start_server, stop_server,
    },
    protocol::{
        Health, LifecycleState, MODEL_CATALOG_EVENT, ModelCatalog, PROTOCOL_VERSION,
        RuntimeDescriptor, SERVER_SHUTDOWN_EVENT, SESSION_CATALOG_SNAPSHOT_EVENT,
        SETTINGS_SNAPSHOT_EVENT, ServerIdentity, ServerShutdown, SessionCatalogRevision,
        SessionCatalogSnapshot, SettingsSnapshot, ShutdownReason,
    },
    server::{self, ServerConfig},
};
use sysinfo::{Pid, System};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::time::{Duration, timeout, timeout_at};
use uuid::Uuid;

#[allow(dead_code)]
mod support;

use support::{
    detached_servers::DetachedServers, read_runtime_descriptor, receive_initial_state,
    request_server_shutdown, write_runtime_descriptor,
};

fn suru_binary_build_identity() -> String {
    build_identity::for_executable(env!("CARGO_BIN_EXE_suru"))
        .expect("identify the tested Suru executable")
}

fn inert_server_executable(state_dir: &std::path::Path) -> PathBuf {
    let executable = state_dir.join(format!("must-not-spawn{}", std::env::consts::EXE_SUFFIX));
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
    let servers = DetachedServers::new();
    let channel = "build-replacement-test";
    let fixture =
        BuildReplacementFixture::spawn(servers.state_dir(), channel, "suru@old-build").await;
    let previous = fixture.descriptor();
    let config = servers.client_config(channel);

    let replacement = start_server(&config)
        .await
        .expect("replace mismatched server build");

    assert_ne!(replacement.instance_id, previous.instance_id);
    assert_ne!(replacement.pid, previous.pid);
    assert_eq!(replacement.build_identity, suru_binary_build_identity());
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
}

#[tokio::test]
async fn configured_executable_replaced_at_the_same_path_replaces_then_reuses_server() {
    let servers = DetachedServers::new();
    let channel = "custom-path-replacement";
    let executable = inert_server_executable(servers.state_dir());
    let old_identity = build_identity::for_executable(&executable).unwrap();
    let fixture = BuildReplacementFixture::spawn(servers.state_dir(), channel, &old_identity).await;
    let config = servers.isolate(
        ManagedClientConfig::new(servers.state_dir(), channel)
            .unwrap()
            .with_server_executable(&executable),
    );
    let original = start_server(&config).await.expect("reuse configured build");
    assert_eq!(original.instance_id, fixture.descriptor().instance_id);

    let modified = std::fs::metadata(&executable).unwrap().modified().unwrap();
    let replacement_file = tempfile::NamedTempFile::new_in(servers.state_dir()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_suru"), replacement_file.path()).unwrap();
    replacement_file.as_file().set_modified(modified).unwrap();
    drop(
        replacement_file
            .persist(&executable)
            .expect("atomically replace configured executable"),
    );

    let replacement = start_server(&config)
        .await
        .expect("launch rebuilt configured executable");
    assert_ne!(replacement.instance_id, original.instance_id);
    assert_eq!(replacement.build_identity, suru_binary_build_identity());
    assert_eq!(
        fixture.shutdown_request().unwrap().reason,
        ShutdownReason::Replacement
    );
    let reused = start_server(&config)
        .await
        .expect("reuse rebuilt configured executable");
    assert_eq!(reused.instance_id, replacement.instance_id);
}

#[tokio::test]
async fn attached_client_reconnects_to_the_replacement() {
    let servers = DetachedServers::new();
    let channel = "attached-build-replacement-test";
    let old_executable = inert_server_executable(servers.state_dir());
    let old_build_identity = inert_server_build_identity(servers.state_dir());
    let fixture =
        BuildReplacementFixture::spawn(servers.state_dir(), channel, &old_build_identity).await;
    let previous = fixture.descriptor();
    let old_config = ManagedClientConfig::new(servers.state_dir(), channel)
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

    let current_config = servers.client_config(channel);
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

    crash_registered_server(servers.state_dir(), channel);
    let (old_recovery, current_recovery) = tokio::join!(
        receive_recovered_state(&mut attached, replacement.instance_id),
        receive_recovered_state(&mut current, replacement.instance_id),
    );
    let restarted = old_recovery;
    let current_restarted = current_recovery;
    assert_ne!(restarted.instance_id, replacement.instance_id);
    assert_eq!(restarted.instance_id, current_restarted.instance_id);
    assert_eq!(restarted.build_identity, replacement.build_identity);
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
    timeout(PROGRESS_DEADLINE, async {
        while !original.is_stopped() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original server releases the channel");
    let incompatible = BuildReplacementFixture::spawn_with_protocol(
        state_dir.path(),
        channel,
        "suru@incompatible-build",
        PROTOCOL_VERSION + 1,
    )
    .await;

    let recovering = timeout(PROGRESS_DEADLINE, attached.next())
        .await
        .expect("old client processes replacement intent promptly")
        .expect("managed client remains open");
    assert!(matches!(
        recovering,
        ManagedEvent::Recovering(RecoveryStatus {
            attempt: 1,
            retry_in: Duration::ZERO,
            unreachable: None,
        })
    ));

    let fatal = timeout(PROGRESS_DEADLINE, async {
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
    let servers = DetachedServers::new();
    let channel = "incompatible-registered-build-test";
    let mismatched = BuildReplacementFixture::spawn_with_protocol(
        servers.state_dir(),
        channel,
        "suru@old-incompatible-build",
        PROTOCOL_VERSION - 1,
    )
    .await;
    let config = servers.client_config(channel);

    let previous = mismatched.descriptor();
    let mut client = ManagedClient::connect(config)
        .await
        .expect("replace the stale build and connect the launching client");
    let replacement = receive_initial_state(&mut client).await;

    assert_ne!(replacement.instance_id, previous.instance_id);
    assert_ne!(replacement.pid, previous.pid);
    assert_eq!(replacement.build_identity, suru_binary_build_identity());
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
}

#[tokio::test]
async fn launcher_rejects_a_protocol_incompatible_matching_build_without_replacing_it() {
    let servers = DetachedServers::new();
    let channel = "incompatible-matching-build-test";
    let incompatible = BuildReplacementFixture::spawn_with_protocol(
        servers.state_dir(),
        channel,
        &suru_binary_build_identity(),
        PROTOCOL_VERSION - 1,
    )
    .await;
    let config = servers.client_config(channel);

    let error = start_server(&config)
        .await
        .expect_err("matching build with an incompatible protocol cannot be reused")
        .to_string();

    assert!(error.contains("registered Suru server protocol version"));
    assert!(error.contains("incompatible"));
    assert!(incompatible.shutdown_request().is_none());
}

#[tokio::test]
async fn simultaneous_replacement_launchers_converge_on_one_new_instance() {
    let servers = DetachedServers::new();
    let channel = "replacement-race-test";
    let fixture =
        BuildReplacementFixture::spawn(servers.state_dir(), channel, "suru@old-build").await;
    let previous_instance_id = fixture.descriptor().instance_id;
    let config = servers
        .client_config(channel)
        .with_startup_timeout(PROGRESS_DEADLINE)
        .with_health_check_timeout(Duration::from_millis(100));
    let launchers = (0..8)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { start_server(&config).await })
        })
        .collect::<Vec<_>>();

    let mut replacements = Vec::new();
    let deadline = tokio::time::Instant::now() + LAUNCHER_SETTLE_DEADLINE;
    for (index, launcher) in launchers.into_iter().enumerate() {
        replacements.push(
            timeout_at(deadline, launcher)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "replacement launcher {index} did not settle within {LAUNCHER_SETTLE_DEADLINE:?}; {}",
                        describe_test_registration(servers.state_dir(), channel)
                    )
                })
                .expect("replacement launcher does not panic")
                .expect("replacement launcher succeeds"),
        );
    }
    let winner = replacements.first().expect("at least one replacement");
    assert_ne!(winner.instance_id, previous_instance_id);
    assert!(replacements.iter().all(|replacement| {
        replacement.instance_id == winner.instance_id && replacement.pid == winner.pid
    }));
}

#[tokio::test]
async fn launcher_retries_after_losing_election_to_a_mismatched_build() {
    let servers = DetachedServers::new();
    let channel = "mixed-build-election-race-test";
    let runtime_dir = servers.state_dir().join(channel);
    let lock = BuildReplacementFixture::acquire_channel_lock(servers.state_dir(), channel);
    let config = servers.client_config(channel);
    let launching = tokio::spawn({
        let config = config.clone();
        async move { start_server(&config).await }
    });
    let log_path = runtime_dir.join("server.log");
    timeout(PROGRESS_DEADLINE, async {
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
        servers.state_dir(),
        channel,
        "suru@other-racing-build",
        PROTOCOL_VERSION,
        lock,
    )
    .await;
    let replacement = timeout(PROGRESS_DEADLINE, launching)
        .await
        .expect("launcher completes after replacing election winner")
        .expect("launcher task does not panic")
        .expect("launcher retries its current build after the transition");

    assert_ne!(replacement.instance_id, mismatched.descriptor().instance_id);
    assert_eq!(replacement.build_identity, suru_binary_build_identity());
    assert_eq!(
        mismatched
            .shutdown_request()
            .expect("mismatched election winner is replaced")
            .reason,
        ShutdownReason::Replacement
    );
}

#[tokio::test]
async fn mismatched_build_in_another_channel_is_not_replaced() {
    let servers = DetachedServers::new();
    let old_channel = "isolated-old-build";
    let current_channel = "isolated-current-build";
    let old =
        BuildReplacementFixture::spawn(servers.state_dir(), old_channel, "suru@old-build").await;
    let current = start_server(&servers.client_config(current_channel))
        .await
        .expect("start current build in another channel");

    assert_eq!(current.build_identity, suru_binary_build_identity());
    assert!(old.shutdown_request().is_none());
    let old_health = reqwest::Client::new()
        .get(format!("{}/health", old.descriptor().base_url))
        .bearer_auth(&old.descriptor().token)
        .send()
        .await
        .expect("old channel remains reachable");
    assert_eq!(old_health.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn simultaneous_launchers_converge_on_one_authenticated_server() {
    let servers = DetachedServers::new();
    let channel = "concurrent-election-test";
    let config = servers
        .client_config(channel)
        .with_startup_timeout(PROGRESS_DEADLINE)
        .with_health_check_timeout(Duration::from_millis(100));

    let client_launches = (0..4)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { ManagedClient::connect(config).await })
        })
        .collect::<Vec<_>>();
    let command_launches = (0..4)
        .map(|_| {
            let command = server_cli(&servers, channel, "start");
            let state_dir = servers.state_dir().to_path_buf();
            tokio::spawn(
                async move { settle_server_cli(command, &state_dir, channel, "start").await },
            )
        })
        .collect::<Vec<_>>();

    let mut clients = Vec::new();
    let deadline = tokio::time::Instant::now() + LAUNCHER_SETTLE_DEADLINE;
    for (index, launch) in client_launches.into_iter().enumerate() {
        clients.push(
            timeout_at(deadline, launch)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "managed client launcher {index} did not settle within {LAUNCHER_SETTLE_DEADLINE:?}; {}",
                        describe_test_registration(servers.state_dir(), channel)
                    )
                })
                .expect("managed client launch task does not panic")
                .expect("managed client launch succeeds"),
        );
    }
    let mut command_outputs = Vec::new();
    for (index, launch) in command_launches.into_iter().enumerate() {
        let output = timeout_at(deadline, launch)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "server start launcher {index} did not settle within {LAUNCHER_SETTLE_DEADLINE:?}; {}",
                    describe_test_registration(servers.state_dir(), channel)
                )
            })
            .expect("server start task does not panic");
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
}

#[tokio::test]
async fn managed_clients_recover_from_a_crash_and_converge_on_one_replacement() {
    let servers = DetachedServers::new();
    let channel = "crash-recovery-test";
    let config = servers.client_config(channel);
    let mut first = ManagedClient::connect(config.clone())
        .await
        .expect("connect first managed client");
    let mut second = ManagedClient::connect(config)
        .await
        .expect("connect second managed client");
    let first_identity = receive_initial_state(&mut first).await;
    let second_identity = receive_initial_state(&mut second).await;
    assert_eq!(first_identity.instance_id, second_identity.instance_id);

    crash_registered_server(servers.state_dir(), channel);

    let (first_recovered, second_recovered) = tokio::join!(
        receive_recovered_state(&mut first, first_identity.instance_id),
        receive_recovered_state(&mut second, second_identity.instance_id),
    );
    assert_ne!(first_recovered.instance_id, first_identity.instance_id);
    assert_eq!(first_recovered.instance_id, second_recovered.instance_id);
    assert_eq!(first_recovered.pid, second_recovered.pid);
}

async fn receive_recovered_state(client: &mut ManagedClient, previous_instance_id: Uuid) -> Health {
    timeout(PROGRESS_DEADLINE, async {
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
async fn recovery_backoff_is_exponential_and_capped() {
    const INITIAL_BACKOFF: Duration = Duration::from_millis(5);
    const MAX_BACKOFF: Duration = Duration::from_millis(80);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "recovery-backoff-test";
    let _fixture = ReadinessFixture::spawn_recovery_backoff(state_dir.path(), channel).await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path()))
            .with_recovery_backoff(INITIAL_BACKOFF, MAX_BACKOFF),
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
        assert!(status.retry_in <= MAX_BACKOFF);
        if expected_attempt == 1 {
            assert!(status.retry_in.is_zero());
        } else if let Some(previous) = previous_nonzero_wait {
            assert_eq!(status.retry_in, previous.saturating_mul(2).min(MAX_BACKOFF));
        } else {
            assert_eq!(status.retry_in, INITIAL_BACKOFF);
        }
        if status.retry_in == MAX_BACKOFF {
            observed_cap = true;
            break;
        }
        if !status.retry_in.is_zero() {
            previous_nonzero_wait = Some(status.retry_in);
        }
    }

    assert!(observed_cap, "recovery backoff never reached its cap");
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

    let first = timeout(PROGRESS_DEADLINE, client.next())
        .await
        .expect("first recovery state arrives")
        .expect("managed client remains open");
    let ManagedEvent::Recovering(status) = first else {
        panic!("expected recovering event, got {first:?}");
    };
    assert_eq!(status.attempt, 1);
    assert_eq!(status.retry_in, Duration::ZERO);
    let second = timeout(PROGRESS_DEADLINE, client.next())
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
        let fatal = match timeout(PROGRESS_DEADLINE, client.next())
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

    let shutdown = timeout(PROGRESS_DEADLINE, client.next())
        .await
        .expect("shutdown intent arrives")
        .expect("managed client reports shutdown intent");
    let ManagedEvent::ServerShutdown(shutdown) = shutdown else {
        panic!("expected shutdown intent, got {shutdown:?}");
    };
    assert_eq!(shutdown.instance_id, identity.instance_id);
    assert_eq!(shutdown.reason, ShutdownReason::Manual);
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await,
        Ok(None)
    ));
    assert_eq!(fixture.event_requests(), 1);
}

#[tokio::test]
async fn stale_descriptor_pid_is_never_used_to_terminate_an_unrelated_process() {
    let servers = DetachedServers::new();
    let unrelated_channel = "unrelated-live-process";
    // Made first, as a managed client makes them before it launches a server.
    ServerConfig::new(servers.state_dir(), unrelated_channel)
        .expect("configure unrelated server")
        .create_private_runtime_dir()
        .expect("make the unrelated server's directories");
    let mut unrelated = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru"));
    unrelated
        .arg("__server")
        .arg("--state-dir")
        .arg(servers.state_dir())
        .arg("--data-dir")
        .arg(servers.state_dir())
        .arg("--channel")
        .arg(unrelated_channel)
        .envs(servers.isolated_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut unrelated = unrelated.spawn().expect("spawn unrelated live process");
    let unrelated_pid = unrelated.id().expect("unrelated process has a PID");
    let unrelated_descriptor = servers
        .state_dir()
        .join(unrelated_channel)
        .join("runtime.json");
    timeout(PROGRESS_DEADLINE, async {
        while !unrelated_descriptor.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unrelated live process publishes its descriptor");

    let channel = "stale-live-pid-test";
    let runtime_dir = servers.state_dir().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
    write_runtime_descriptor(
        runtime_dir.join("runtime.json"),
        &RuntimeDescriptor {
            base_url: "http://127.0.0.1:9".to_owned(),
            token: "stale-token".to_owned(),
            identity: ServerIdentity {
                instance_id: Uuid::new_v4(),
                pid: unrelated_pid,
                protocol_version: PROTOCOL_VERSION,
                build_identity: "stale-build".to_owned(),
            },
        },
    );

    let stop = run_server_cli(&servers, channel, "stop").await;
    assert!(!stop.status.success());
    assert!(
        unrelated
            .try_wait()
            .expect("inspect unrelated process after refused stop")
            .is_none(),
        "server stop terminated a process based only on stale metadata"
    );

    let mut client = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(PROGRESS_DEADLINE)
            .with_health_check_timeout(Duration::from_millis(100)),
    )
    .await
    .expect("recover from stale descriptor");
    let identity = receive_initial_state(&mut client).await;

    assert_ne!(identity.pid, unrelated_pid);
    assert!(
        unrelated
            .try_wait()
            .expect("inspect unrelated live process")
            .is_none(),
        "unrelated live process was terminated from stale metadata"
    );

    drop(client);
    unrelated
        .start_kill()
        .expect("terminate unrelated process fixture");
    timeout(PROGRESS_DEADLINE, unrelated.wait())
        .await
        .expect("unrelated process fixture exits within 2s")
        .expect("wait for unrelated process fixture");
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
        let servers = DetachedServers::new();
        let runtime_dir = servers.state_dir().join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
        std::fs::write(runtime_dir.join("runtime.json"), stale_contents)
            .expect("seed invalid runtime descriptor");

        let mut client = ManagedClient::connect(servers.client_config(channel))
            .await
            .expect("recover from invalid runtime descriptor");
        let identity = receive_initial_state(&mut client).await;
        let published = read_runtime_descriptor(runtime_dir.join("runtime.json"));
        assert_eq!(published.instance_id, identity.instance_id);
        assert_eq!(published.pid, identity.pid);
    }
}

#[tokio::test]
async fn reuse_requires_an_authenticated_matching_server_identity() {
    let servers = DetachedServers::new();
    let decoy = server::spawn(
        ServerConfig::new(servers.state_dir(), "authenticated-decoy")
            .expect("configure decoy server"),
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
        let runtime_dir = servers.state_dir().join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create stale runtime directory");
        let mut stale = decoy.descriptor().clone();
        mutate(&mut stale);
        write_runtime_descriptor(runtime_dir.join("runtime.json"), &stale);

        let mut client = ManagedClient::connect(servers.client_config(channel))
            .await
            .expect("replace unauthenticated or identity-inconsistent registration");
        let identity = receive_initial_state(&mut client).await;
        assert_ne!(identity.instance_id, decoy.descriptor().instance_id);
    }

    decoy.shutdown().await.expect("shut down decoy server");
}

fn mutate_wrong_token(descriptor: &mut RuntimeDescriptor) {
    descriptor.token = "incorrect-token".to_owned();
}

fn mutate_instance_id(descriptor: &mut RuntimeDescriptor) {
    descriptor.instance_id = Uuid::new_v4();
}

/// `SURU_IDENTITY_STORE`, as the client launching a Server reads it, is
/// where that Server keeps a new identity key: one Serving from its start
/// makes its key then, and its Log says the variable chose where it is
/// kept. A value naming no store fails the client, which launches nothing.
#[tokio::test]
async fn suru_identity_store_chooses_where_a_launched_server_keeps_its_key() {
    let servers = DetachedServers::new();
    let channel = "identity-store-test";
    std::fs::write(
        servers.state_dir().join("suru.jsonc"),
        r#"{ "serving": { "enabled": true, "port": 0, "bindAddress": "127.0.0.1" } }"#,
    )
    .expect("write a Serving Config Document");
    let refused = run_server_cli_with_env(
        &servers,
        channel,
        "start",
        "SURU_IDENTITY_STORE",
        "keychain",
    )
    .await;
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("the identity store is `system` or `file`, not \"keychain\""),
        "{stderr}"
    );
    assert!(
        !servers
            .state_dir()
            .join(channel)
            .join("runtime.json")
            .exists(),
        "nothing is launched"
    );

    let started =
        run_server_cli_with_env(&servers, channel, "start", "SURU_IDENTITY_STORE", "file").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let logs = servers.state_dir().join(channel).join("log");
    let chosen = "Server identity key is made and kept in an owner-only file in the data \
                  directory: SURU_IDENTITY_STORE=file keeps it there";
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let logged = std::fs::read_dir(&logs)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .any(|log| log.contains(chosen));
            if logged {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Server Logs that SURU_IDENTITY_STORE chose the file");

    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

#[tokio::test]
async fn server_status_reports_authenticated_ready_and_missing_states() {
    let servers = DetachedServers::new();
    let channel = "status-ready-test";

    let missing = run_server_cli(&servers, channel, "status").await;
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing"),
        "missing status was not identified: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let started = run_server_cli(&servers, channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let descriptor =
        read_runtime_descriptor(servers.state_dir().join(channel).join("runtime.json"));

    let ready = run_server_cli(&servers, channel, "status").await;
    assert!(
        ready.status.success(),
        "ready status failed: {}",
        String::from_utf8_lossy(&ready.stderr)
    );
    let stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(stdout.contains("ready"));
    assert!(stdout.contains(&descriptor.pid.to_string()));
    assert!(stdout.contains(&descriptor.instance_id.to_string()));

    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

#[tokio::test]
async fn server_status_distinguishes_lifecycle_stale_and_unreachable_registrations() {
    for (channel, lifecycle, expected) in [
        ("status-starting-test", LifecycleState::Starting, "starting"),
        ("status-stopping-test", LifecycleState::Stopping, "stopping"),
        ("status-failed-test", LifecycleState::Failed, "failed"),
    ] {
        let servers = DetachedServers::new();
        let _fixture = ReadinessFixture::spawn(servers.state_dir(), channel, lifecycle).await;

        let output = run_server_cli(&servers, channel, "status").await;
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "expected {expected} status, got: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stale_servers = DetachedServers::new();
    let stale_channel = "status-stale-test";
    let _fixture = ReadinessFixture::spawn(
        stale_servers.state_dir(),
        stale_channel,
        LifecycleState::Ready,
    )
    .await;
    let stale_path = stale_servers
        .state_dir()
        .join(stale_channel)
        .join("runtime.json");
    let mut stale = read_runtime_descriptor(&stale_path);
    stale.instance_id = Uuid::new_v4();
    write_runtime_descriptor(&stale_path, &stale);
    let stale_output = run_server_cli(&stale_servers, stale_channel, "status").await;
    assert!(!stale_output.status.success());
    assert!(
        String::from_utf8_lossy(&stale_output.stderr).contains("stale"),
        "stale registration was not identified: {}",
        String::from_utf8_lossy(&stale_output.stderr)
    );

    let unreachable_servers = DetachedServers::new();
    let unreachable_channel = "status-unreachable-test";
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("reserve an unused loopback address");
    let unreachable_url = format!(
        "http://{}",
        listener.local_addr().expect("read unused address")
    );
    drop(listener);
    let runtime_dir = unreachable_servers.state_dir().join(unreachable_channel);
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
        run_server_cli(&unreachable_servers, unreachable_channel, "status").await;
    assert!(!unreachable_output.status.success());
    assert!(
        String::from_utf8_lossy(&unreachable_output.stderr).contains("unreachable"),
        "unreachable registration was not identified: {}",
        String::from_utf8_lossy(&unreachable_output.stderr)
    );
}

#[tokio::test]
async fn server_stop_notifies_attached_clients_and_remains_stopped() {
    let servers = DetachedServers::new();
    let channel = "cli-manual-stop-test";
    let started = run_server_cli(&servers, channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let descriptor_path = servers.state_dir().join(channel).join("runtime.json");
    let descriptor = read_runtime_descriptor(&descriptor_path);
    let mut managed = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(Duration::from_millis(500))
            .with_recovery_backoff(Duration::from_millis(10), Duration::from_millis(20)),
    )
    .await
    .expect("attach managed client before manual stop");
    receive_initial_state(&mut managed).await;

    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let stdout = String::from_utf8_lossy(&stopped.stdout);
    assert!(stdout.contains("stopped"));
    assert!(stdout.contains(&descriptor.pid.to_string()));
    assert!(stdout.contains(&descriptor.instance_id.to_string()));

    let shutdown = match timeout(PROGRESS_DEADLINE, managed.next())
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
        timeout(PROGRESS_DEADLINE, managed.next()).await,
        Ok(None)
    ));

    assert!(!descriptor_path.exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !descriptor_path.exists(),
        "manual stop was undone by immediate recovery"
    );
    let status = run_server_cli(&servers, channel, "status").await;
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("missing"));
}

/// Waits until `count` servers launched against `servers` are running.
async fn wait_for_servers_running(servers: &DetachedServers, count: usize, why: &str) {
    timeout(PROGRESS_DEADLINE, async {
        while servers.servers_running() != count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{why}: {} servers running", servers.servers_running()));
}

/// Records a Manual stop of `instance_id` as `channel`'s last, as an
/// election's winner stopped with `suru server stop` does before it lets the
/// channel go — for a test standing in for that winner by holding the lock.
fn record_manual_stop(state_dir: &std::path::Path, channel: &str, instance_id: Uuid) {
    std::fs::write(
        test_runtime_root(state_dir, channel).join("last-stop.json"),
        format!("{{\"instance_id\":\"{instance_id}\"}}\n"),
    )
    .expect("record a manual stop");
}

/// Eight `suru server start`s launched together each launch a server, and
/// the seven that lose the election wait out the winner's hold — here for as
/// long as the test needs, so the stop certainly finds them waiting. A
/// `suru server stop` of the winner is final for all seven: each ends as it
/// takes the lock rather than serving the channel, and a start after the
/// stop serves it again.
#[tokio::test]
async fn a_manual_stop_is_final_for_every_server_concurrent_starts_left_waiting() {
    let servers = DetachedServers::new();
    let channel = "stop-after-concurrent-starts-test";
    let descriptor_path = servers.state_dir().join(channel).join("runtime.json");
    // Held until every start has launched its server, so all eight enter
    // the election and none finds another already serving.
    let hold = BuildReplacementFixture::acquire_channel_lock(servers.state_dir(), channel);
    let handoff_ms = PROGRESS_DEADLINE.as_millis().to_string();
    let starts = (0..8)
        .map(|_| {
            let mut command = server_cli(&servers, channel, "start");
            command.arg("--election-handoff-ms").arg(&handoff_ms);
            let state_dir = servers.state_dir().to_path_buf();
            tokio::spawn(
                async move { settle_server_cli(command, &state_dir, channel, "start").await },
            )
        })
        .collect::<Vec<_>>();
    wait_for_servers_running(&servers, 8, "every start launches a server").await;
    drop(hold);

    let mut outputs = Vec::new();
    for start in starts {
        let output = start.await.expect("server start task does not panic");
        assert!(
            output.status.success(),
            "concurrent server start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let winner = read_runtime_descriptor(&descriptor_path);
    assert!(
        outputs
            .iter()
            .all(|output| output.contains(&winner.instance_id.to_string())),
        "every start reports the one elected server: {outputs:?}"
    );
    assert_eq!(
        servers.servers_running(),
        8,
        "seven servers wait in the election behind the winner"
    );

    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for_servers_running(
        &servers,
        0,
        "every server waiting when the winner was stopped ends rather than serving",
    )
    .await;
    assert!(
        !descriptor_path.exists(),
        "a server waiting in the election took the stopped channel over"
    );
    let status = run_server_cli(&servers, channel, "status").await;
    assert!(String::from_utf8_lossy(&status.stderr).contains("missing"));

    let restarted = run_server_cli(&servers, channel, "start").await;
    assert!(
        restarted.status.success(),
        "a start after the stop failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let successor = read_runtime_descriptor(&descriptor_path);
    assert_ne!(successor.instance_id, winner.instance_id);
    let status = run_server_cli(&servers, channel, "status").await;
    assert!(
        status.status.success(),
        "the server started after the stop is not serving: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A launch whose server is still waiting in the election when the winner is
/// stopped manually answers with the stop, promptly, rather than waiting out
/// its deadline for a server that is not coming or launching another.
#[tokio::test]
async fn a_launch_a_manual_stop_overtakes_reports_the_stop() {
    let servers = DetachedServers::new();
    let channel = "stop-overtakes-launch-test";
    let hold = BuildReplacementFixture::acquire_channel_lock(servers.state_dir(), channel);
    let config = servers
        .client_config(channel)
        .with_startup_timeout(PROGRESS_DEADLINE)
        .with_election_handoff(PROGRESS_DEADLINE);
    let launching = tokio::spawn(async move { start_server(&config).await });
    wait_for_servers_running(&servers, 1, "the launch launches a server").await;

    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    drop(hold);

    let error = timeout(PROGRESS_DEADLINE, launching)
        .await
        .expect("the launch settles once the stop is recorded")
        .expect("the launch task does not panic")
        .expect_err("a launch overtaken by a manual stop starts no server");
    let stop = error
        .downcast_ref::<StoppedDuringLaunch>()
        .unwrap_or_else(|| panic!("expected the stop, got {error:#}"));
    assert_eq!(stop.instance_id, stopped);
    wait_for_servers_running(&servers, 0, "the launched server ends").await;
    assert!(
        !test_runtime_root(servers.state_dir(), channel)
            .join("runtime.json")
            .exists()
    );
}

/// A managed client recovering its server, whose relaunch is still waiting
/// in the election when the channel is stopped manually, leaves as the
/// clients attached to the stopped server do — on the stop's own intent —
/// rather than launching again and undoing the stop.
#[tokio::test]
async fn a_recovering_client_a_manual_stop_overtakes_leaves_as_on_the_stop() {
    let servers = DetachedServers::new();
    let channel = "stop-overtakes-recovery-test";
    let hold = BuildReplacementFixture::acquire_channel_lock(servers.state_dir(), channel);
    let _fixture = ReadinessFixture::spawn_with_event_behavior_and_build(
        servers.state_dir(),
        channel,
        LifecycleState::Ready,
        FixtureEventBehavior::DisconnectAfterConnected,
        suru_binary_build_identity(),
    )
    .await;
    let mut client = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(PROGRESS_DEADLINE)
            .with_election_handoff(PROGRESS_DEADLINE)
            .with_recovery_backoff(Duration::from_millis(10), Duration::from_millis(20)),
    )
    .await
    .expect("connect to the fixture");
    // The fixture drops its event stream and refuses another, so the client
    // recovers; with its registration gone it launches a server to recover to.
    std::fs::remove_file(test_runtime_root(servers.state_dir(), channel).join("runtime.json"))
        .expect("remove the fixture's registration");
    wait_for_servers_running(&servers, 1, "the recovering client launches a server").await;

    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    drop(hold);

    let shutdown = timeout(PROGRESS_DEADLINE, async {
        loop {
            match client.next().await {
                Some(ManagedEvent::ServerShutdown(shutdown)) => return shutdown,
                Some(ManagedEvent::Fatal(error)) => panic!("recovery failed: {error}"),
                Some(_) => {}
                None => panic!("the client closed without the stop's intent"),
            }
        }
    })
    .await
    .expect("the recovering client learns of the stop");
    assert_eq!(
        shutdown,
        ServerShutdown {
            instance_id: stopped,
            reason: ShutdownReason::Manual,
        }
    );
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await,
        Ok(None)
    ));
    wait_for_servers_running(&servers, 0, "the relaunched server ends").await;
}

/// A recovering managed client that finds a server ready, which is then
/// stopped manually before the client's streams to it open, leaves on the
/// stop rather than retrying: every attempt it makes holds to the stop it
/// set out from, so it never takes the newer stop for its own and launches
/// a server past it.
#[tokio::test]
async fn a_recovering_client_whose_found_server_is_stopped_before_its_streams_open_leaves() {
    let servers = DetachedServers::new();
    let channel = "stop-before-streams-test";
    let fixture = ReadinessFixture::spawn_with_event_behavior_and_build(
        servers.state_dir(),
        channel,
        LifecycleState::Ready,
        FixtureEventBehavior::DisconnectAfterConnected,
        suru_binary_build_identity(),
    )
    .await;
    let mut client = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(PROGRESS_DEADLINE)
            .with_recovery_backoff(Duration::from_millis(10), Duration::from_millis(20)),
    )
    .await
    .expect("connect to the fixture");
    // The client's task has not run since it connected, so the next event
    // stream it opens — its recovery's, once the first one ends — is held.
    fixture.lifecycle_handshake.hold();
    fixture.lifecycle_handshake.wait_requested().await;
    fixture.lifecycle_handshake.wait_requested().await;

    // The server the recovery found ready is stopped while its stream opens:
    // its stop is recorded and its registration goes, so a client that
    // retried would launch a server of its own.
    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    std::fs::remove_file(test_runtime_root(servers.state_dir(), channel).join("runtime.json"))
        .expect("remove the stopped server's registration");
    fixture
        .lifecycle_handshake
        .respond(StatusCode::SERVICE_UNAVAILABLE);

    expect_to_leave_on_the_stop(&mut client, stopped, false).await;
    assert_eq!(
        servers.servers_running(),
        0,
        "the client launched no server"
    );
}

/// A recovering managed client whose attempt failed for some other reason,
/// and whose channel is stopped manually before its next attempt, leaves on
/// that stop rather than taking it for the one it set out from and
/// launching a server past it.
#[tokio::test]
async fn a_recovering_client_stopped_between_attempts_leaves() {
    let servers = DetachedServers::new();
    let channel = "stop-between-attempts-test";
    let _fixture = ReadinessFixture::spawn_with_event_behavior_and_build(
        servers.state_dir(),
        channel,
        LifecycleState::Ready,
        FixtureEventBehavior::DisconnectAfterConnected,
        suru_binary_build_identity(),
    )
    .await;
    let mut client = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(PROGRESS_DEADLINE)
            .with_recovery_backoff(Duration::from_millis(10), Duration::from_millis(20)),
    )
    .await
    .expect("connect to the fixture");
    // The fixture refuses every event stream after the first, so the first
    // recovery attempt fails with no stop recorded. The client announces its
    // second attempt before it waits to make it, and cannot make it while
    // this test runs, so the stop lands between the two.
    timeout(PROGRESS_DEADLINE, async {
        loop {
            match client.next().await.expect("the client stays open") {
                ManagedEvent::Recovering(status) if status.attempt == 2 => break,
                ManagedEvent::Fatal(error) => panic!("recovery failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the first recovery attempt fails");
    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    std::fs::remove_file(test_runtime_root(servers.state_dir(), channel).join("runtime.json"))
        .expect("remove the stopped server's registration");

    expect_to_leave_on_the_stop(&mut client, stopped, true).await;
    assert_eq!(
        servers.servers_running(),
        0,
        "the client launched no server"
    );
}

/// A managed client waiting for the successor a replacement promised holds
/// to the stop it set out from: the channel stopped manually while it
/// waits, it leaves on that stop rather than attaching to a server its user
/// starts again afterwards.
#[tokio::test]
async fn a_client_awaiting_a_replacement_leaves_on_a_manual_stop() {
    let servers = DetachedServers::new();
    let channel = "stop-during-replacement-test";
    let _replaced = ReadinessFixture::spawn_with_event_behavior_and_build(
        servers.state_dir(),
        channel,
        LifecycleState::Ready,
        FixtureEventBehavior::ReplacementAfterConnected,
        suru_binary_build_identity(),
    )
    .await;
    let mut client = ManagedClient::connect(
        servers
            .client_config(channel)
            .with_startup_timeout(PROGRESS_DEADLINE),
    )
    .await
    .expect("connect to the server to be replaced");
    timeout(PROGRESS_DEADLINE, async {
        loop {
            match client.next().await.expect("the client stays open") {
                ManagedEvent::Recovering(_) => break,
                ManagedEvent::Fatal(error) => panic!("the client failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("the client waits for the replacement");

    // The successor is stopped manually, and a server started again after.
    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    let _restarted = ReadinessFixture::spawn_with_event_behavior_and_build(
        servers.state_dir(),
        channel,
        LifecycleState::Ready,
        FixtureEventBehavior::StayConnected,
        suru_binary_build_identity(),
    )
    .await;

    expect_to_leave_on_the_stop(&mut client, stopped, true).await;
    assert_eq!(
        servers.servers_running(),
        0,
        "the client launched no server"
    );
}

/// A launcher set out before a Manual stop, which then finds a server of
/// another build its user started again since, neither replaces that server
/// — leaving the channel with no server at all — nor attaches to it: the
/// stop is final for the launch, and decides before anything it finds.
#[tokio::test]
async fn a_launch_overtaken_by_a_stop_leaves_a_restarted_build_alone() {
    let servers = DetachedServers::new();
    let channel = "stop-before-replacement-test";
    let hold = BuildReplacementFixture::acquire_channel_lock(servers.state_dir(), channel);
    let config = servers
        .client_config(channel)
        .with_startup_timeout(PROGRESS_DEADLINE)
        .with_election_handoff(PROGRESS_DEADLINE)
        .with_health_check_timeout(Duration::from_millis(100));
    let launching = tokio::spawn(async move { start_server(&config).await });
    wait_for_servers_running(&servers, 1, "the launch launches a server").await;

    let stopped = Uuid::new_v4();
    record_manual_stop(servers.state_dir(), channel, stopped);
    let restarted = BuildReplacementFixture::spawn_with_lock(
        servers.state_dir(),
        channel,
        "suru@restarted-build",
        PROTOCOL_VERSION,
        hold,
    )
    .await;

    let error = timeout(PROGRESS_DEADLINE, launching)
        .await
        .expect("the launch settles")
        .expect("the launch task does not panic")
        .expect_err("a launch overtaken by a manual stop starts nothing");
    let stop = error
        .downcast_ref::<StoppedDuringLaunch>()
        .unwrap_or_else(|| panic!("expected the stop, got {error:#}"));
    assert_eq!(stop.instance_id, stopped);
    assert!(
        restarted.shutdown_request().is_none(),
        "the launch replaced a server started after the stop"
    );
}

/// `suru server stop` is bounded by its deadline even where the server
/// answers that the stop failed and then never finishes saying why.
#[tokio::test]
async fn a_failed_stop_whose_reason_stalls_ends_at_the_stop_deadline() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "stalled-stop-reason-test";
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind stalling fixture");
    let descriptor = RuntimeDescriptor {
        base_url: format!(
            "http://{}",
            listener.local_addr().expect("read fixture address")
        ),
        token: "stalling-fixture-token".to_owned(),
        identity: ServerIdentity {
            instance_id: Uuid::new_v4(),
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            build_identity: suru_binary_build_identity(),
        },
    };
    let runtime_dir = state_dir.path().join(channel);
    std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
    write_runtime_descriptor(runtime_dir.join("runtime.json"), &descriptor);
    let health = descriptor.health(LifecycleState::Ready);
    let app = Router::new()
        .route("/health", get(move || async move { Json(health) }))
        .route(
            "/v1/server/stop",
            post(|| async {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::from_stream(stream::pending::<
                        Result<axum::body::Bytes, Infallible>,
                    >()))
                    .expect("build a stalled answer")
            }),
        );
    let fixture = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve stalling fixture");
    });

    let error = timeout(
        PROGRESS_DEADLINE,
        stop_server(
            &ManagedClientConfig::new(state_dir.path(), channel)
                .expect("configure stop client")
                .with_stop_timeout(Duration::from_millis(100))
                .with_health_check_timeout(Duration::from_millis(100)),
        ),
    )
    .await
    .expect("the stop ends at its own deadline, not when the body does")
    .expect_err("a failed stop is not a success");
    assert!(
        error.to_string().contains("manual stop failed"),
        "unexpected error: {error:#}"
    );
    fixture.abort();
}

/// Waits for `client` to report the Manual stop of `stopped` and close,
/// failing should it recover to any server instead — `already_recovering`
/// where the test has already seen it set out to.
async fn expect_to_leave_on_the_stop(
    client: &mut ManagedClient,
    stopped: Uuid,
    already_recovering: bool,
) {
    let shutdown = timeout(PROGRESS_DEADLINE, async {
        let mut recovering = already_recovering;
        loop {
            match client.next().await {
                Some(ManagedEvent::ServerShutdown(shutdown)) => return shutdown,
                Some(ManagedEvent::Recovering(_)) => recovering = true,
                Some(ManagedEvent::Connected(health)) if recovering => {
                    panic!(
                        "the client recovered past the stop to {}",
                        health.instance_id
                    )
                }
                Some(ManagedEvent::Fatal(error)) => panic!("recovery failed: {error}"),
                Some(_) => {}
                None => panic!("the client closed without the stop's intent"),
            }
        }
    })
    .await
    .expect("the recovering client learns of the stop");
    assert_eq!(
        shutdown,
        ServerShutdown {
            instance_id: stopped,
            reason: ShutdownReason::Manual,
        }
    );
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await,
        Ok(None)
    ));
}

/// Once a channel has been stopped manually, servers launched afterwards
/// still hand it on as they always have: two clients whose server crashes
/// recover to one replacement, and launchers replacing a mismatched build
/// converge on one new instance.
#[tokio::test]
async fn crash_recovery_and_replacement_converge_after_a_manual_stop() {
    let servers = DetachedServers::new();
    let channel = "handoffs-after-stop-test";
    let started = run_server_cli(&servers, channel, "start").await;
    assert!(started.status.success());
    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        test_runtime_root(servers.state_dir(), channel)
            .join("last-stop.json")
            .exists(),
        "the manual stop was recorded"
    );

    let config = servers.client_config(channel);
    let mut first = ManagedClient::connect(config.clone())
        .await
        .expect("connect first managed client after the stop");
    let mut second = ManagedClient::connect(config.clone())
        .await
        .expect("connect second managed client after the stop");
    let first_identity = receive_initial_state(&mut first).await;
    receive_initial_state(&mut second).await;
    crash_registered_server(servers.state_dir(), channel);
    let (first_recovered, second_recovered) = tokio::join!(
        receive_recovered_state(&mut first, first_identity.instance_id),
        receive_recovered_state(&mut second, first_identity.instance_id),
    );
    assert_eq!(first_recovered.instance_id, second_recovered.instance_id);
    drop((first, second));
    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(stopped.status.success());
    // A server the recovering clients launched that lost their election may
    // still be waiting in it, and ends rather than serving once the stop lets
    // the lock go.
    wait_for_servers_running(&servers, 0, "every server ends after the second stop").await;

    let fixture =
        BuildReplacementFixture::spawn(servers.state_dir(), channel, "suru@old-build").await;
    let config = config
        .with_startup_timeout(PROGRESS_DEADLINE)
        .with_health_check_timeout(Duration::from_millis(100));
    let launchers = (0..8)
        .map(|_| {
            let config = config.clone();
            tokio::spawn(async move { start_server(&config).await })
        })
        .collect::<Vec<_>>();
    let mut replacements = Vec::new();
    for launcher in launchers {
        replacements.push(
            timeout(LAUNCHER_SETTLE_DEADLINE, launcher)
                .await
                .expect("replacement launcher settles")
                .expect("replacement launcher does not panic")
                .expect("replacement launcher succeeds"),
        );
    }
    let winner = replacements.first().expect("at least one replacement");
    assert_ne!(winner.instance_id, fixture.descriptor().instance_id);
    assert!(
        replacements
            .iter()
            .all(|replacement| replacement.instance_id == winner.instance_id)
    );
}

const TERMINAL_MODE_RESTORED: &str = "__SURU_TERMINAL_MODE_RESTORED__";
const TERMINAL_MODE_CHANGED: &str = "__SURU_TERMINAL_MODE_CHANGED__";

/// Not a test of its own: [`AttachedTui`] re-executes this binary to reach this
/// function from inside a pseudo-terminal. The mode a TUI puts a terminal into
/// belongs to that terminal, and only a process running inside it can read the
/// mode back, so the check that the TUI returned the mode it was handed has to
/// run here rather than in the test that opened the terminal. Ignored so an
/// ordinary run never reaches it.
#[test]
#[ignore = "re-executed inside a pseudo-terminal by the attached TUI tests"]
fn attached_tui_terminal_mode_probe() {
    let before = read_terminal_mode();
    let status = Command::new(env!("CARGO_BIN_EXE_suru"))
        .status()
        .expect("run the TUI inside the pseudo-terminal");
    let after = read_terminal_mode();
    if before == after {
        println!("\n{TERMINAL_MODE_RESTORED}");
    } else {
        println!("\n{TERMINAL_MODE_CHANGED}:{before}:{after}");
    }
    // The harness reports on the TUI's own status, so the probe exits with it
    // rather than letting the test harness report on the probe.
    std::io::Write::flush(&mut std::io::stdout()).expect("flush the probe report");
    std::process::exit(status.code().unwrap_or(1));
}

/// The console mode of the pseudo-console, which is a property of the console
/// itself rather than of the handle used to reach it.
#[cfg(windows)]
fn read_terminal_mode() -> String {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE]
        .iter()
        .map(|handle| {
            let mut mode = 0u32;
            // SAFETY: both handles belong to the pseudo-console this process was
            // spawned into, and `mode` outlives the call that fills it.
            let read = unsafe { GetConsoleMode(GetStdHandle(*handle), &mut mode) };
            assert!(read != 0, "read the mode of the pseudo-console");
            format!("{mode:08x}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The terminal attributes `stty -g` reports, read the way `stty` reads them.
///
/// `PENDIN` is left out: it is the terminal driver's own note that input awaits
/// reprocessing, not a mode a program chooses. A BSD kernel such as macOS's
/// raises it whenever a terminal returns to canonical mode — pending input or
/// not — so a TUI that restores exactly what it was given still leaves it set.
#[cfg(unix)]
fn read_terminal_mode() -> String {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: stdin is the pseudo-terminal this process was spawned into, and
    // `tcgetattr` fills the attributes it is handed on success.
    let read = unsafe { libc::tcgetattr(libc::STDIN_FILENO, attributes.as_mut_ptr()) };
    assert_eq!(read, 0, "read the mode of the pseudo-terminal");
    // SAFETY: `tcgetattr` reported success, so the attributes are initialized.
    let attributes = unsafe { attributes.assume_init() };
    format!(
        "{:x}:{:x}:{:x}:{:x}:{:?}",
        attributes.c_iflag,
        attributes.c_oflag,
        attributes.c_cflag,
        attributes.c_lflag & !libc::PENDIN,
        attributes.c_cc
    )
}

/// Answers the cursor position report a terminal is expected to answer. A
/// ConPTY pseudo-console asks for one while it is starting up and waits for the
/// reply before it will carry anything the child writes, so a harness that only
/// reads never sees a byte of the TUI it launched. The position reported is the
/// home position, which is where the pseudo-console starts its screen.
fn answer_cursor_position_report(shown: &[u8], terminal: &Mutex<Box<dyn std::io::Write + Send>>) {
    const REQUEST: &[u8] = b"\x1b[6n";
    const HOME: &[u8] = b"\x1b[1;1R";

    if !shown.windows(REQUEST.len()).any(|window| window == REQUEST) {
        return;
    }
    let mut terminal = terminal.lock().expect("answer the pseudo-terminal");
    let _ = terminal.write_all(HOME);
    let _ = terminal.flush();
}

/// Answers only after the first frame, with a complete light-terminal palette.
/// The real pseudo-terminal is deliberately otherwise inert, so this is the
/// terminal-emulator half of the startup boundary under test.
#[cfg(unix)]
fn answer_terminal_color_queries(
    shown: &[u8],
    terminal: &Mutex<Box<dyn std::io::Write + Send>>,
    answered: &mut bool,
) {
    const LAST_QUERY: &[u8] = b"\x1b]11;?\x1b\\";
    if *answered
        || !shown
            .windows(b"\x1b[?25h".len())
            .any(|window| window == b"\x1b[?25h")
        || !shown
            .windows(LAST_QUERY.len())
            .any(|window| window == LAST_QUERY)
    {
        return;
    }
    *answered = true;
    let mut reply = Vec::new();
    for index in 0..16 {
        reply.extend_from_slice(
            format!("\x1b]4;{index};rgb:{index:04x}/{index:04x}/{index:04x}\x07").as_bytes(),
        );
    }
    reply.extend_from_slice(b"\x1b]10;rgb:2222/2222/2222\x1b\\\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
    let mut terminal = terminal.lock().expect("answer the pseudo-terminal");
    let _ = terminal.write_all(&reply);
    let _ = terminal.flush();
}

/// A TUI attached to a real pseudo-terminal: a ConPTY pseudo-console on Windows
/// and an `openpty` pair everywhere else. The TUI is launched through
/// [`attached_tui_terminal_mode_probe`] so that the terminal mode is sampled
/// from inside the terminal, on both sides of the TUI's lifetime.
struct AttachedTui {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    input: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    screen: Arc<Mutex<Vec<u8>>>,
}

/// What the terminal saw, and how the TUI left it.
struct AttachedTuiOutcome {
    succeeded: bool,
    screen: String,
}

impl AttachedTui {
    /// A pseudo-terminal wide enough that the lines the assertions look for are
    /// not wrapped: ConPTY renders what the TUI writes into a screen buffer of
    /// exactly this width before the harness sees any of it, so a narrow
    /// terminal would split those lines with cursor movement.
    const SIZE: portable_pty::PtySize = portable_pty::PtySize {
        rows: 24,
        cols: 200,
        pixel_width: 0,
        pixel_height: 0,
    };

    fn spawn(servers: &DetachedServers, channel: &str) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(Self::SIZE)
            .expect("open a pseudo-terminal");
        let mut command = portable_pty::CommandBuilder::new(
            std::env::current_exe().expect("locate the running test binary"),
        );
        command.args([
            "--exact",
            "attached_tui_terminal_mode_probe",
            "--ignored",
            "--nocapture",
        ]);
        command.env("SURU_STATE_DIR", servers.state_dir());
        command.env("SURU_DATA_DIR", servers.state_dir());
        command.env("SURU_CONFIG_DIR", servers.state_dir());
        command.env("SURU_CHANNEL", channel);
        // A TUI asks every Provider for its Models as it connects, so the
        // server it reaches must find none of the real ones.
        for (key, value) in servers.isolated_environment() {
            command.env(key, value);
        }
        command.env_remove("NO_COLOR");
        command.env("COLORTERM", "truecolor");
        let child = pair
            .slave
            .spawn_command(command)
            .expect("launch the attached TUI in a pseudo-terminal");
        // The harness keeps no terminal handle of its own, so the terminal
        // reports end-of-file once the TUI and its probe are gone.
        drop(pair.slave);

        // The reader accumulates into a buffer the harness can read at any time
        // rather than reporting at end-of-file. A ConPTY pseudo-console reports
        // end-of-file only once every handle to it is closed, and the reader is
        // itself such a handle, so waiting for the end would wait forever.
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("read the pseudo-terminal");
        let input: Arc<Mutex<Box<dyn std::io::Write + Send>>> = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .expect("write to the pseudo-terminal"),
        ));
        let screen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&screen);
        let responder = Arc::clone(&input);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            #[cfg(unix)]
            let mut shown_so_far = Vec::new();
            #[cfg(unix)]
            let mut answered_colors = false;
            while let Ok(read) = std::io::Read::read(&mut reader, &mut chunk) {
                if read == 0 {
                    break;
                }
                let shown = &chunk[..read];
                answer_cursor_position_report(shown, &responder);
                #[cfg(unix)]
                if !answered_colors {
                    shown_so_far.extend_from_slice(shown);
                    answer_terminal_color_queries(&shown_so_far, &responder, &mut answered_colors);
                }
                recorder
                    .lock()
                    .expect("record what the terminal showed")
                    .extend_from_slice(shown);
            }
        });

        Self {
            _master: pair.master,
            input,
            child,
            screen,
        }
    }

    fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("inspect the attached TUI")
            .is_none()
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut input = self.input.lock().expect("the terminal accepts input");
        input.write_all(bytes).expect("send input to the TUI");
        input.flush().expect("flush input to the TUI");
    }

    /// Waits for the TUI to exit and collects everything the terminal showed.
    fn finish(&mut self) -> AttachedTuiOutcome {
        let status = self.child.wait().expect("wait for the attached TUI");
        AttachedTuiOutcome {
            succeeded: status.success(),
            screen: self.settled_screen(),
        }
    }

    /// Everything the terminal has shown once it stops showing anything new.
    /// The last of what a process wrote on its way out reaches the terminal
    /// after the process itself is gone, so a screen read the instant the TUI
    /// exits is missing its own restore sequences.
    fn settled_screen(&self) -> String {
        const SETTLE: Duration = Duration::from_millis(100);
        const LIMIT: Duration = Duration::from_secs(2);

        let deadline = std::time::Instant::now() + LIMIT;
        let mut shown = self.shown();
        loop {
            std::thread::sleep(SETTLE);
            let settled = self.shown();
            if settled == shown || std::time::Instant::now() >= deadline {
                return String::from_utf8_lossy(&settled).into_owned();
            }
            shown = settled;
        }
    }

    fn shown(&self) -> Vec<u8> {
        self.screen
            .lock()
            .expect("read what the terminal showed")
            .clone()
    }
}

impl Drop for AttachedTui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AttachedTuiOutcome {
    fn assert_terminal_mode_restored(&self) {
        assert!(
            !self.screen.contains(TERMINAL_MODE_CHANGED),
            "the TUI left the terminal in a mode it was not given: {:?}",
            self.screen
        );
        assert!(
            self.screen.contains(TERMINAL_MODE_RESTORED),
            "the TUI never reported on the terminal mode it was given: {:?}",
            self.screen
        );
    }

    /// A ConPTY pseudo-console is a terminal emulator rather than a conduit: it
    /// parses what the TUI writes into a screen buffer and re-renders sequences
    /// of its own, so the TUI's own escape sequences survive to be asserted on
    /// only where the pseudo-terminal passes bytes through untouched. The order
    /// the TUI writes them in is pinned on every platform by the unit tests over
    /// `enter_terminal_display` and `leave_terminal_display`.
    #[cfg(unix)]
    fn assert_display_was_restored(&self) {
        for sequence in [
            "\u{1b}[?1049h",
            "\u{1b}[?1049l",
            "\u{1b}[?25l",
            "\u{1b}[?25h",
            "\u{1b}[?2004h",
            "\u{1b}[?2004l",
        ] {
            assert!(
                self.screen.contains(sequence),
                "missing terminal sequence {sequence:?}: {:?}",
                self.screen
            );
        }
    }

    #[cfg(not(unix))]
    fn assert_display_was_restored(&self) {}

    fn position_of(&self, sequence: &str) -> usize {
        self.screen
            .find(sequence)
            .unwrap_or_else(|| panic!("{sequence:?} is missing from {:?}", self.screen))
    }
}

#[tokio::test]
async fn fatal_protocol_error_restores_the_terminal_and_exits_without_input() {
    let servers = DetachedServers::new();
    let channel = "fatal-tui-protocol-test";
    let fixture = ReadinessFixture::spawn_binary_protocol_violation(
        servers.state_dir(),
        channel,
        ProtocolViolation::UnknownEvent,
    )
    .await;
    let mut tui = AttachedTui::spawn(&servers, channel);

    timeout(PROGRESS_DEADLINE, async {
        while !fixture.events_opened.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TUI opens the corrupt event stream");
    timeout(PROGRESS_DEADLINE, async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fatal TUI exits promptly without user input");

    let outcome = tui.finish();
    assert!(!outcome.succeeded, "fatal TUI reports failure");
    outcome.assert_display_was_restored();
    outcome.assert_terminal_mode_restored();
    let error_detail = outcome.position_of("unknown event type 'future_event'");

    // The report has to reach the terminal the user is left looking at, not the
    // alternate screen that is about to be torn down with it.
    #[cfg(unix)]
    {
        assert!(
            outcome.position_of("\u{1b}[?1049l") < error_detail,
            "fatal error was reported before leaving the alternate screen: {:?}",
            outcome.screen
        );
        assert!(
            outcome.position_of("\u{1b}[?25h") < error_detail,
            "fatal error was reported before restoring the cursor: {:?}",
            outcome.screen
        );
        let paste_enabled = outcome.position_of("\u{1b}[?2004h");
        let paste_disabled = outcome.position_of("\u{1b}[?2004l");
        assert!(
            paste_enabled < paste_disabled && paste_disabled < error_detail,
            "fatal TUI did not bracket its active lifetime with paste mode: {:?}",
            outcome.screen
        );
    }
    // The probe reports on the terminal mode only once the TUI has exited, so a
    // report that follows the error detail is the ordering evidence that a
    // re-rendering pseudo-console can still give.
    assert!(
        error_detail < outcome.position_of(TERMINAL_MODE_RESTORED),
        "fatal error was reported after the TUI had already exited: {:?}",
        outcome.screen
    );
}

#[tokio::test]
async fn attached_tui_restores_its_terminal_and_exits_on_manual_stop() {
    let servers = DetachedServers::new();
    let channel = "attached-tui-manual-stop-test";
    let started = run_server_cli(&servers, channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let mut tui = AttachedTui::spawn(&servers, channel);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        tui.is_running(),
        "attached TUI exited before the manual stop"
    );

    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    timeout(PROGRESS_DEADLINE, async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("attached TUI exits after manual stop");

    let outcome = tui.finish();
    outcome.assert_display_was_restored();
    outcome.assert_terminal_mode_restored();
}

#[tokio::test]
async fn clean_tui_exit_restores_the_terminal() {
    let servers = DetachedServers::new();
    let channel = "clean-tui-exit-test";
    let started = run_server_cli(&servers, channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let mut tui = AttachedTui::spawn(&servers, channel);
    // Raw mode is taken before the TUI draws anything, so a frame on the screen
    // is what says Ctrl+C will reach the composer as a keystroke rather than
    // reaching the process as a signal. A re-rendering pseudo-console keeps
    // none of the escape sequences that entering the display writes, so the
    // frame is what the wait can look for on every platform.
    timeout(PROGRESS_DEADLINE, async {
        loop {
            assert!(tui.is_running(), "TUI exited before clean-exit input");
            if String::from_utf8_lossy(&tui.shown()).contains("Type a prompt") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TUI draws its first frame before clean-exit input");
    // Unix PTYs pass OSC through, so the harness controls when color replies
    // arrive. ConPTY emulates colors itself; Application rendering tests cover
    // the same fallback-to-light transition on every platform.
    #[cfg(unix)]
    timeout(PROGRESS_DEADLINE, async {
        while !String::from_utf8_lossy(&tui.shown()).contains("48;2;238;238;238") {
            assert!(tui.is_running(), "TUI exited before the color repaint");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("late colors repaint without reader input");
    tui.send(b"\x03");
    timeout(PROGRESS_DEADLINE, async {
        while tui.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TUI exits cleanly after Ctrl+C");

    let outcome = tui.finish();
    assert!(
        outcome.succeeded,
        "clean TUI exit failed: {:?}",
        outcome.screen
    );
    outcome.assert_display_was_restored();
    outcome.assert_terminal_mode_restored();
    #[cfg(unix)]
    {
        let query = outcome.position_of("\u{1b}]11;?\u{1b}\\");
        let first_frame = outcome.position_of("\u{1b}[?25h");
        assert!(
            query < first_frame,
            "the first frame preceded the terminal probe"
        );
        assert!(
            !outcome.screen[..first_frame].contains("48;2;238;238;238"),
            "the first frame must precede color replies: {:?}",
            outcome.screen
        );
        assert!(
            outcome.position_of("48;2;238;238;238") > first_frame,
            "late colors must reach a subsequent frame"
        );
    }
}

/// The longest a `suru server` command a test runs is waited on. Only a wedged
/// subprocess reaches this; it sits beyond the deadlines [`server_cli`] hands
/// the command so the CLI's own diagnosis wins whenever the CLI is still
/// answering.
const SERVER_CLI_PROCESS_TIMEOUT: Duration =
    PROGRESS_DEADLINE.saturating_add(Duration::from_secs(10));

/// How long a test waits for launchers it began together to settle. Each is
/// bounded by a deadline of its own — a managed client by its startup timeout
/// of `PROGRESS_DEADLINE`, a `suru server start` by
/// [`SERVER_CLI_PROCESS_TIMEOUT`] — so this sits beyond both, and a launcher
/// that fails reports its own diagnosis rather than this deadline.
const LAUNCHER_SETTLE_DEADLINE: Duration =
    SERVER_CLI_PROCESS_TIMEOUT.saturating_add(Duration::from_secs(5));

/// `suru server <command>` against `channel` under `servers`, with the
/// environment that keeps any server it launches from the real Providers.
fn server_cli(servers: &DetachedServers, channel: &str, command: &str) -> tokio::process::Command {
    // The CLI's own start and stop deadlines are failure deadlines in exactly
    // the sense `PROGRESS_DEADLINE` describes: the command returns the moment
    // the server settles, so a generous value costs a passing run nothing. A
    // literal second was enough on Linux but not on Windows, where a server
    // with a Client attached needs a little longer to release its registration
    // than the stop deadline allowed — and the CLI then reported a settled
    // shutdown as a failure.
    let settle_ms = PROGRESS_DEADLINE.as_millis().to_string();
    // The health probe is the opposite case. It is spent in full on every
    // iteration that finds the endpoint still reachable, so it keeps a short
    // literal: deciding "unreachable" quickly is what makes the wait above end
    // early rather than late.
    let timing_args: Vec<&str> = match command {
        "start" => vec!["--startup-timeout-ms", &settle_ms],
        "stop" => vec![
            "--stop-timeout-ms",
            &settle_ms,
            "--health-check-timeout-ms",
            "100",
        ],
        _ => vec![],
    };
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru"));
    process
        .arg("server")
        .arg(command)
        .args(timing_args)
        .env("SURU_STATE_DIR", servers.state_dir())
        .env("SURU_DATA_DIR", servers.state_dir())
        .env("SURU_CONFIG_DIR", servers.state_dir())
        .env("SURU_CHANNEL", channel)
        .envs(servers.isolated_environment())
        .kill_on_drop(true);
    process
}

/// Runs a `suru server` command, failing the test with the registration it
/// left behind should the command wedge rather than settle.
async fn settle_server_cli(
    mut process: tokio::process::Command,
    state_dir: &std::path::Path,
    channel: &str,
    command: &str,
) -> std::process::Output {
    match timeout(SERVER_CLI_PROCESS_TIMEOUT, process.output()).await {
        Ok(output) => output.expect("run server CLI command"),
        Err(_) => panic!(
            "server {command} process boundary did not settle within {SERVER_CLI_PROCESS_TIMEOUT:?}; {}",
            describe_test_registration(state_dir, channel)
        ),
    }
}

/// Runs a `suru server` command as [`run_server_cli`] does, with `key` set
/// to `value` in its environment.
async fn run_server_cli_with_env(
    servers: &DetachedServers,
    channel: &str,
    command: &str,
    key: &str,
    value: &str,
) -> std::process::Output {
    let mut process = server_cli(servers, channel, command);
    process.env(key, value);
    settle_server_cli(process, servers.state_dir(), channel, command).await
}

async fn run_server_cli(
    servers: &DetachedServers,
    channel: &str,
    command: &str,
) -> std::process::Output {
    settle_server_cli(
        server_cli(servers, channel, command),
        servers.state_dir(),
        channel,
        command,
    )
    .await
}

#[tokio::test]
async fn server_start_returns_after_a_detached_server_is_ready() {
    let servers = DetachedServers::new();
    let channel = "detached-start-test";
    let output = run_server_cli(&servers, channel, "start").await;

    assert!(
        output.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let descriptor_path = servers.state_dir().join(channel).join("runtime.json");
    let first_descriptor = read_runtime_descriptor(&descriptor_path);

    let repeated = run_server_cli(&servers, channel, "start").await;
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
        servers
            .client_config(channel)
            .with_startup_timeout(Duration::from_millis(500)),
    )
    .await
    .expect("connect after start command has exited");
    let identity = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());
    assert_eq!(identity.pid, first_descriptor.pid);
    assert_eq!(identity.instance_id, first_descriptor.instance_id);

    drop(client);
    let stopped = run_server_cli(&servers, channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

#[tokio::test]
async fn build_profile_selects_isolated_default_state_and_data_roots() {
    // Declared before the servers so it outlives them: the server launched here
    // keeps its data under it until the servers are stopped.
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let servers = DetachedServers::new();
    let (expected_channel, other_channel) = if cfg!(debug_assertions) {
        ("debug", "release")
    } else {
        ("release", "debug")
    };
    let mut process = server_cli(&servers, expected_channel, "start");
    process
        .env("SURU_DATA_DIR", data_dir.path())
        .env_remove("SURU_CHANNEL");
    let output = settle_server_cli(process, servers.state_dir(), expected_channel, "start").await;

    assert!(
        output.status.success(),
        "server start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected_state_root = test_runtime_root(servers.state_dir(), expected_channel);
    let expected_data_root = test_runtime_root(data_dir.path(), expected_channel);
    assert!(expected_state_root.join("runtime.json").exists());
    assert!(expected_data_root.exists());
    assert!(!servers.state_dir().join(other_channel).exists());
    assert!(!data_dir.path().join(other_channel).exists());

    let stopped = run_server_cli(&servers, expected_channel, "stop").await;
    assert!(
        stopped.status.success(),
        "server stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
}

#[tokio::test]
async fn managed_client_starts_a_missing_server_before_streaming_initial_state() {
    let servers = DetachedServers::new();
    let channel = "managed-auto-start-test";
    let config = servers.client_config(channel);

    let mut client = ManagedClient::connect(config)
        .await
        .expect("connect through managed startup");

    let identity = receive_initial_state(&mut client).await;
    assert_ne!(identity.pid, std::process::id());
}

#[tokio::test]
async fn sequential_managed_clients_reuse_the_persistent_server() {
    let servers = DetachedServers::new();
    let channel = "managed-reuse-test";
    let config = servers.client_config(channel);

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
}

#[tokio::test]
async fn managed_client_recognizes_readiness_just_after_a_probe() {
    let mut recognition_times = Vec::new();
    for fixed in [true, false] {
        let state_dir = tempfile::tempdir().unwrap();
        let fixture = ReadinessFixture::spawn(
            state_dir.path(),
            "prompt-readiness",
            LifecycleState::Starting,
        )
        .await;
        fixture.ready_after_probe.store(true, Ordering::SeqCst);
        let mut config = ManagedClientConfig::new(state_dir.path(), "prompt-readiness")
            .unwrap()
            .with_server_executable(inert_server_executable(state_dir.path()));
        if fixed {
            config = config
                .with_readiness_polling(Duration::from_millis(250), Duration::from_millis(250));
        }

        let health = start_server(&config).await.unwrap();
        let recognized = tokio::time::Instant::now();
        let probes = fixture.health_probes.lock().unwrap();
        assert_eq!(health.lifecycle, LifecycleState::Ready);
        assert_eq!(probes.len(), 2);
        recognition_times.push(recognized.duration_since(probes[0]));
    }
    // Compare against an injected slow cadence rather than imposing a tight
    // absolute wall-clock limit on HTTP and the platform scheduler.
    assert!(
        recognition_times[1] * 2 < recognition_times[0],
        "adaptive recognition should beat fixed polling: {recognition_times:?}"
    );
}

#[tokio::test]
async fn managed_client_bounds_readiness_sleep_by_the_startup_deadline() {
    let state_dir = tempfile::tempdir().unwrap();
    let fixture = ReadinessFixture::spawn(
        state_dir.path(),
        "readiness-deadline",
        LifecycleState::Starting,
    )
    .await;
    let config = ManagedClientConfig::new(state_dir.path(), "readiness-deadline")
        .unwrap()
        .with_server_executable(inert_server_executable(state_dir.path()))
        .with_startup_timeout(Duration::from_millis(150))
        .with_readiness_polling(Duration::from_millis(1000), Duration::from_millis(1000));
    let result = timeout(Duration::from_millis(500), start_server(&config))
        .await
        .expect("sleep must end at the startup deadline");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("did not become ready")
    );
    assert_eq!(
        fixture.health_probes.lock().unwrap().len(),
        1,
        "do not issue another probe after the deadline"
    );
}

#[tokio::test]
async fn managed_client_backs_off_readiness_probes_for_a_slow_server() {
    let state_dir = tempfile::tempdir().unwrap();
    let fixture =
        ReadinessFixture::spawn(state_dir.path(), "slow-readiness", LifecycleState::Starting).await;
    let config = ManagedClientConfig::new(state_dir.path(), "slow-readiness")
        .unwrap()
        .with_server_executable(inert_server_executable(state_dir.path()))
        .with_startup_timeout(Duration::from_millis(180));
    let error = start_server(&config).await.unwrap_err();
    assert!(error.to_string().contains("did not become ready"));
    let probes = fixture.health_probes.lock().unwrap();
    // A final probe can reach the deadline without retaining the previous status text.
    assert!(
        !probes.is_empty(),
        "the slow server was probed before the deadline"
    );
    assert!(
        probes.len() <= 7,
        "unbounded startup probe rate: {}",
        probes.len()
    );
    for pair in probes.windows(2).skip(4) {
        assert!(pair[1].duration_since(pair[0]) >= Duration::from_millis(50));
    }
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
        let mut client = timeout(PROGRESS_DEADLINE, connecting)
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

    let result = timeout(PROGRESS_DEADLINE, ManagedClient::connect(config))
        .await
        .expect("failed lifecycle is reported promptly");
    let error = match result {
        Ok(_) => panic!("managed client unexpectedly connected"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("registered Suru server reported failed startup"));
    assert!(!fixture.events_opened.load(Ordering::SeqCst));
}

#[tokio::test]
async fn managed_client_reports_a_bounded_log_tail_when_startup_fails() {
    let servers = DetachedServers::new();
    let channel = "startup-failure-test";
    let runtime_dir = servers.state_dir().join(channel);
    std::fs::create_dir_all(runtime_dir.join("server.lock"))
        .expect("create invalid server lock directory");
    std::fs::write(
        runtime_dir.join("server.log"),
        format!("discarded-prefix\n{}", "x".repeat(16 * 1024)),
    )
    .expect("seed oversized server log");
    let config = servers.client_config(channel);

    let result = timeout(PROGRESS_DEADLINE, ManagedClient::connect(config))
        .await
        .expect("startup failure is reported without waiting for the full deadline");
    let error = match result {
        Ok(_) => panic!("managed client unexpectedly connected"),
        Err(error) => format!("{error:#}"),
    };

    assert!(error.contains("detached Suru server exited before becoming ready"));
    assert!(error.contains("Recent server log"));
    assert!(error.contains("open server election lock"));
    assert!(!error.contains("discarded-prefix"));
    assert!(error.len() < 10 * 1024, "startup error was not bounded");
}

#[tokio::test]
async fn managed_client_bounds_either_initial_stream_handshake_and_drops_the_sibling() {
    for stall_catalog in [false, true] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let channel = "event-handshake-timeout-test";
        let fixture =
            ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
        let (stalled, sibling, name) = fixture.handshake_and_sibling(stall_catalog);
        stalled.hold();
        std::fs::write(
            state_dir.path().join(channel).join("server.log"),
            format!(
                "discarded-prefix\n{}\nstream fixture stalled\n",
                "x".repeat(16 * 1024)
            ),
        )
        .expect("write fixture server log");
        let config = ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path()))
            .with_startup_timeout(Duration::from_millis(200));

        let error = timeout(PROGRESS_DEADLINE, ManagedClient::connect(config))
            .await
            .expect("initial streams use the startup deadline")
            .err()
            .expect("a stalled stream prevents connection")
            .to_string();

        stalled.wait_requested().await;
        sibling.wait_requested().await;
        stalled.wait_closed().await;
        sibling.wait_closed().await;
        assert!(
            error.contains(&format!("initial {name} did not open within 200ms")),
            "{error}"
        );
        assert!(error.contains("stream fixture stalled"));
        assert!(!error.contains("discarded-prefix"));
        assert!(
            error.len() < 10 * 1024,
            "startup diagnostics remain bounded"
        );
    }
}

#[tokio::test]
async fn managed_client_rejects_either_failed_handshake_and_drops_the_sibling() {
    for reject_catalog in [false, true] {
        for sibling_open in [false, true] {
            let state_dir = tempfile::tempdir().expect("create isolated state directory");
            let channel = "rejected-handshake-test";
            let fixture =
                ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
            fixture.lifecycle_handshake.hold();
            fixture.catalog_handshake.hold();
            let (rejected, sibling, name) = fixture.handshake_and_sibling(reject_catalog);
            let config = ManagedClientConfig::new(state_dir.path(), channel)
                .expect("configure managed client")
                .with_server_executable(inert_server_executable(state_dir.path()))
                .with_startup_timeout(Duration::from_secs(2));
            let mut connecting = Box::pin(ManagedClient::connect(config));
            tokio::select! {
                _ = &mut connecting => panic!("both responses are held"),
                _ = async {
                    rejected.wait_requested().await;
                    sibling.wait_requested().await;
                } => {}
            }
            if sibling_open {
                sibling.respond(StatusCode::OK);
                tokio::select! {
                    _ = &mut connecting => panic!("the rejected response is still held"),
                    _ = sibling.wait_streaming() => {}
                }
                assert!(
                    timeout(Duration::from_millis(20), &mut connecting)
                        .await
                        .is_err()
                );
            }
            rejected.respond(StatusCode::UNAUTHORIZED);
            let error = timeout(Duration::from_millis(500), connecting)
                .await
                .expect("rejection cancels the sibling before the startup deadline")
                .err()
                .expect("a rejected stream prevents connection")
                .to_string();
            assert!(
                error.contains(&format!("server rejected the initial {name}")),
                "{error}"
            );
            assert!(error.contains("401 Unauthorized"), "{error}");
            sibling.wait_closed().await;
        }
    }
}

#[tokio::test]
async fn managed_client_reports_either_broken_handshake_and_closes_the_waiting_sibling() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for break_catalog in [false, true] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let channel = "broken-handshake-test";
        let _fixture =
            ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
        let descriptor_path = state_dir.path().join(channel).join("runtime.json");
        let mut descriptor = read_runtime_descriptor(&descriptor_path);
        let upstream = descriptor.base_url.trim_start_matches("http://").to_owned();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind handshake fault proxy");
        descriptor.base_url = format!("http://{}", listener.local_addr().unwrap());
        write_runtime_descriptor(&descriptor_path, &descriptor);
        let (requests, mut received) = tokio::sync::mpsc::channel(2);
        // Forward the real readiness fixture's health response, but hand both
        // SSE sockets to the test so it can close one before sending headers.
        let proxy = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let upstream = upstream.clone();
                let requests = requests.clone();
                connections.spawn(async move {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.unwrap());
                    }
                    let path = std::str::from_utf8(&request)
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    if path == "/health" {
                        let mut server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                        server.write_all(&request).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut server).await;
                    } else {
                        requests
                            .send((path == "/v1/session-events", socket))
                            .await
                            .unwrap();
                    }
                });
            }
        });
        // Aborting this owner also drops its JoinSet and any forwarded sockets.
        let _proxy = AbortOnDrop(proxy);
        let config = ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path()))
            .with_startup_timeout(Duration::from_secs(2));
        let mut connecting = Box::pin(ManagedClient::connect(config));
        let sockets = tokio::select! {
            _ = &mut connecting => panic!("both handshake responses are held"),
            sockets = timeout(PROGRESS_DEADLINE, async {
                (received.recv().await.unwrap(), received.recv().await.unwrap())
            }) => sockets.expect("both handshake sockets reach the proxy"),
        };
        let (broken, mut sibling) = if sockets.0.0 == break_catalog {
            (sockets.0.1, sockets.1.1)
        } else {
            (sockets.1.1, sockets.0.1)
        };
        drop(broken);
        let error = timeout(Duration::from_millis(500), connecting)
            .await
            .expect("transport failure promptly cancels the sibling")
            .err()
            .expect("a broken handshake prevents connection")
            .to_string();
        let name = if break_catalog {
            "Session catalog stream"
        } else {
            "event stream"
        };
        assert!(
            error.contains(&format!("could not open the initial {name}")),
            "{error}"
        );
        let mut byte = [0];
        let closed = timeout(PROGRESS_DEADLINE, sibling.read(&mut byte))
            .await
            .expect("the sibling TCP connection closes");
        assert!(
            matches!(closed, Ok(0) | Err(_)),
            "unexpected sibling traffic: {closed:?}"
        );
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn managed_client_handshakes_share_the_deadline_spent_ensuring_the_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "shared-handshake-deadline-test";
    let fixture =
        ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Starting).await;
    fixture.catalog_handshake.hold();
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(inert_server_executable(state_dir.path()))
        .with_startup_timeout(Duration::from_millis(500));
    let started = tokio::time::Instant::now();
    let mut connecting = Box::pin(ManagedClient::connect(config));
    tokio::select! {
        _ = &mut connecting => panic!("server is still initializing"),
        _ = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    assert!(!fixture.health_probes.lock().unwrap().is_empty());
    *fixture.lifecycle.lock().unwrap() = LifecycleState::Ready;
    let error = timeout_at(started + Duration::from_millis(650), connecting)
        .await
        .expect("handshakes do not get a fresh 500ms after readiness")
        .err()
        .expect("the catalog handshake stalls")
        .to_string();
    assert!(
        error.contains("initial Session catalog stream did not open within 500ms"),
        "{error}"
    );
    fixture.lifecycle_handshake.wait_requested().await;
    fixture.catalog_handshake.wait_requested().await;
    fixture.lifecycle_handshake.wait_closed().await;
}

#[tokio::test]
async fn managed_client_opens_both_handshakes_before_adopting_either_response() {
    for release_catalog_first in [false, true] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let channel = "concurrent-handshakes-test";
        let fixture =
            ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
        fixture.lifecycle_handshake.hold();
        fixture.catalog_handshake.hold();
        let config = ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_server_executable(inert_server_executable(state_dir.path()))
            .with_startup_timeout(Duration::from_secs(2));
        let mut connecting = Box::pin(ManagedClient::connect(config));
        tokio::select! {
            result = &mut connecting => panic!("connected before responses were released: {:?}", result.err()),
            _ = async {
                fixture.lifecycle_handshake.wait_requested().await;
                fixture.catalog_handshake.wait_requested().await;
            } => {}
        }
        let (first, second) = if release_catalog_first {
            (&fixture.catalog_handshake, &fixture.lifecycle_handshake)
        } else {
            (&fixture.lifecycle_handshake, &fixture.catalog_handshake)
        };
        first.respond(StatusCode::OK);
        assert!(
            timeout(Duration::from_millis(20), &mut connecting)
                .await
                .is_err(),
            "both responses must succeed before adopting the connection"
        );
        second.respond(StatusCode::OK);
        let mut client = connecting.await.expect("both handshakes succeed");
        receive_initial_state(&mut client).await;
    }
}

#[tokio::test]
async fn managed_client_hydrates_catalog_before_connected_and_settings() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "catalog-hydration-order-test";
    let fixture = ReadinessFixture::spawn(state_dir.path(), channel, LifecycleState::Ready).await;
    fixture.catalog_handshake.body_ready.send_replace(false);
    let config = ManagedClientConfig::new(state_dir.path(), channel)
        .expect("configure managed client")
        .with_server_executable(inert_server_executable(state_dir.path()))
        .with_startup_timeout(Duration::from_millis(500));
    let mut client = ManagedClient::connect(config)
        .await
        .expect("both HTTP handshakes succeed");
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::Connecting)
    ));
    assert!(
        timeout(Duration::from_millis(20), client.next())
            .await
            .is_err(),
        "Connected and Settings wait for the catalog snapshot body"
    );
    fixture.catalog_handshake.body_ready.send_replace(true);
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await.unwrap(),
        Some(ManagedEvent::Connected(_))
    ));
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next()).await.unwrap(),
        Some(ManagedEvent::SettingsSnapshot(_))
    ));
}

/// Controls the HTTP boundary and observes cancellation on the server side,
/// whether the client is still waiting for headers or already owns the body.
#[derive(Clone)]
struct FixtureHandshake {
    response: watch::Sender<Option<StatusCode>>,
    body_ready: watch::Sender<bool>,
    streaming: watch::Sender<bool>,
    requested: Arc<Semaphore>,
    closed: Arc<Semaphore>,
}

impl FixtureHandshake {
    fn new() -> Self {
        Self {
            response: watch::channel(Some(StatusCode::OK)).0,
            body_ready: watch::channel(true).0,
            streaming: watch::channel(false).0,
            requested: Arc::new(Semaphore::new(0)),
            closed: Arc::new(Semaphore::new(0)),
        }
    }

    fn hold(&self) {
        self.response.send_replace(None);
    }

    fn respond(&self, status: StatusCode) {
        self.response.send_replace(Some(status));
    }

    async fn wait_requested(&self) {
        timeout(PROGRESS_DEADLINE, self.requested.acquire())
            .await
            .expect("server observes the handshake request")
            .expect("request signal remains open")
            .forget();
    }

    async fn wait_streaming(&self) {
        timeout(
            PROGRESS_DEADLINE,
            self.streaming.subscribe().wait_for(|started| *started),
        )
        .await
        .expect("server begins streaming the successful response")
        .expect("streaming signal remains open");
    }

    async fn wait_closed(&self) {
        timeout(PROGRESS_DEADLINE, self.closed.acquire())
            .await
            .expect("client drops the sibling request or response")
            .expect("close signal remains open")
            .forget();
    }

    async fn open(&self) -> Result<HandshakeConnection, StatusCode> {
        let connection = HandshakeConnection {
            closed: self.closed.clone(),
            body_ready: self.body_ready.subscribe(),
            streaming: self.streaming.clone(),
        };
        self.requested.add_permits(1);
        let mut response = self.response.subscribe();
        let status = *response
            .wait_for(Option::is_some)
            .await
            .expect("handshake response control remains open");
        match status.expect("response was released") {
            StatusCode::OK => Ok(connection),
            status => Err(status),
        }
    }
}

struct HandshakeConnection {
    closed: Arc<Semaphore>,
    body_ready: watch::Receiver<bool>,
    streaming: watch::Sender<bool>,
}

impl HandshakeConnection {
    fn track(self, response: Response) -> Response {
        let (parts, body) = response.into_parts();
        let body = stream::unfold(
            (body.into_data_stream(), self),
            |(mut body, mut connection)| async move {
                connection.body_ready.wait_for(|ready| *ready).await.ok()?;
                let chunk = body.next().await?;
                connection.streaming.send_replace(true);
                Some((chunk, (body, connection)))
            },
        );
        Response::from_parts(parts, axum::body::Body::from_stream(body))
    }
}

impl Drop for HandshakeConnection {
    fn drop(&mut self) {
        self.closed.add_permits(1);
    }
}

#[derive(Clone)]
struct ReadinessState {
    descriptor: RuntimeDescriptor,
    lifecycle: Arc<Mutex<LifecycleState>>,
    health_probes: Arc<Mutex<Vec<tokio::time::Instant>>>,
    ready_after_probe: Arc<AtomicBool>,
    events_opened: Arc<AtomicBool>,
    lifecycle_handshake: FixtureHandshake,
    catalog_handshake: FixtureHandshake,
    event_requests: Arc<AtomicUsize>,
    event_behavior: FixtureEventBehavior,
}

#[derive(Clone, Copy)]
enum FixtureEventBehavior {
    StayConnected,
    DisconnectAfterConnected,
    ShutdownAfterConnected,
    /// Announces, once connected, that this server is being replaced.
    ReplacementAfterConnected,
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
    health_probes: Arc<Mutex<Vec<tokio::time::Instant>>>,
    ready_after_probe: Arc<AtomicBool>,
    events_opened: Arc<AtomicBool>,
    lifecycle_handshake: FixtureHandshake,
    catalog_handshake: FixtureHandshake,
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
            suru_binary_build_identity(),
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
        let health_probes = Arc::new(Mutex::new(Vec::new()));
        let ready_after_probe = Arc::new(AtomicBool::new(false));
        let events_opened = Arc::new(AtomicBool::new(false));
        let lifecycle_handshake = FixtureHandshake::new();
        let catalog_handshake = FixtureHandshake::new();
        let event_requests = Arc::new(AtomicUsize::new(0));
        let state = ReadinessState {
            descriptor,
            lifecycle: lifecycle.clone(),
            health_probes: health_probes.clone(),
            ready_after_probe: ready_after_probe.clone(),
            events_opened: events_opened.clone(),
            lifecycle_handshake: lifecycle_handshake.clone(),
            catalog_handshake: catalog_handshake.clone(),
            event_requests: event_requests.clone(),
            event_behavior,
        };
        let app = Router::new()
            .route("/health", get(readiness_health))
            .route("/v1/events", get(readiness_events))
            .route("/v1/session-events", get(readiness_catalog_events))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve readiness fixture");
        });

        Self {
            lifecycle,
            health_probes,
            ready_after_probe,
            events_opened,
            lifecycle_handshake,
            catalog_handshake,
            event_requests,
            task,
        }
    }

    fn handshake_and_sibling(
        &self,
        catalog: bool,
    ) -> (&FixtureHandshake, &FixtureHandshake, &'static str) {
        if catalog {
            (
                &self.catalog_handshake,
                &self.lifecycle_handshake,
                "Session catalog stream",
            )
        } else {
            (
                &self.lifecycle_handshake,
                &self.catalog_handshake,
                "event stream",
            )
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
    state
        .health_probes
        .lock()
        .unwrap()
        .push(tokio::time::Instant::now());
    if state.ready_after_probe.swap(false, Ordering::SeqCst) {
        *state.lifecycle.lock().unwrap() = LifecycleState::Ready;
    }
    Json(state.descriptor.health(lifecycle)).into_response()
}

async fn readiness_events(State(state): State<ReadinessState>, headers: HeaderMap) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.events_opened.store(true, Ordering::SeqCst);
    let request_index = state.event_requests.fetch_add(1, Ordering::SeqCst);
    let connection = match state.lifecycle_handshake.open().await {
        Ok(connection) => connection,
        Err(status) => return status.into_response(),
    };
    if *state.lifecycle.lock().expect("lock lifecycle") != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let instance_id = state.descriptor.instance_id;
    let connected = || {
        stream::once(std::future::ready(Ok::<_, Infallible>(
            Event::default().comment("connected"),
        )))
        .chain(stream::once(std::future::ready(Ok::<_, Infallible>(
            fixture_settings_snapshot_event(),
        ))))
        .chain(stream::once(std::future::ready(Ok::<_, Infallible>(
            fixture_model_catalog_event(),
        ))))
        .chain(stream::once(std::future::ready(Ok::<_, Infallible>(
            fixture_relays_event(),
        ))))
        .chain(stream::once(std::future::ready(Ok::<_, Infallible>(
            fixture_serving_listener_event(),
        ))))
    };
    let response = match state.event_behavior {
        FixtureEventBehavior::StayConnected => {
            Sse::new(connected().chain(stream::pending())).into_response()
        }
        FixtureEventBehavior::DisconnectAfterConnected if request_index > 0 => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        FixtureEventBehavior::DisconnectAfterConnected => Sse::new(connected()).into_response(),
        FixtureEventBehavior::ShutdownAfterConnected
        | FixtureEventBehavior::ReplacementAfterConnected => {
            let shutdown = ServerShutdown {
                instance_id,
                reason: if matches!(
                    state.event_behavior,
                    FixtureEventBehavior::ReplacementAfterConnected
                ) {
                    ShutdownReason::Replacement
                } else {
                    ShutdownReason::Manual
                },
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
    };
    connection.track(response)
}

async fn readiness_catalog_events(
    State(state): State<ReadinessState>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if *state.lifecycle.lock().expect("lock lifecycle") != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match state.catalog_handshake.open().await {
        Ok(connection) => connection.track(empty_catalog_stream()),
        Err(status) => status.into_response(),
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

fn fixture_settings_snapshot_event() -> Event {
    Event::default()
        .event(SETTINGS_SNAPSHOT_EVENT)
        .json_data(SettingsSnapshot::default())
        .expect("serialize fixture settings snapshot")
}

fn fixture_model_catalog_event() -> Event {
    Event::default()
        .event(MODEL_CATALOG_EVENT)
        .json_data(ModelCatalog {
            providers: Vec::new(),
        })
        .expect("serialize fixture Model Catalog")
}

/// The Server's Relays, which follow its Model Catalog on every connect: a
/// fixture holds none.
fn fixture_relays_event() -> Event {
    Event::default()
        .event(suru::protocol::RELAYS_EVENT)
        .json_data(suru::protocol::RelayListing {
            instance: uuid::Uuid::from_u128(1),
            revision: 1,
            relays: Vec::new(),
        })
        .expect("serialize fixture Relays")
}

/// How the Server's Serving listener stands, which follows its Relays on
/// every connect: a fixture is not Serving.
fn fixture_serving_listener_event() -> Event {
    Event::default()
        .event(suru::protocol::SERVING_LISTENER_EVENT)
        .json_data(suru::protocol::ListenerState::Off)
        .expect("serialize fixture Serving listener state")
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
            .route("/v1/session-events", get(build_replacement_catalog_events))
            .route("/v1/server/stop", post(build_replacement_stop))
            .with_state(state);
        let instance_id = descriptor
            .lock()
            .expect("lock old-build descriptor")
            .instance_id;
        let task_descriptor_path = descriptor_path.clone();
        let task = tokio::spawn(async move {
            let _lock = lock;
            let server = async { axum::serve(listener, app).await };
            tokio::pin!(server);
            tokio::select! {
                result = &mut server => result.expect("serve old-build fixture"),
                _ = shutdown_rx => {}
            }
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
        stream::once(async move { Ok::<_, Infallible>(Event::default().comment("connected")) })
            .chain(stream::once(async move {
                Ok::<_, Infallible>(fixture_settings_snapshot_event())
            }))
            .chain(stream::once(async move {
                Ok::<_, Infallible>(fixture_model_catalog_event())
            }))
            .chain(stream::once(async move {
                Ok::<_, Infallible>(fixture_relays_event())
            }))
            .chain(stream::once(async move {
                Ok::<_, Infallible>(fixture_serving_listener_event())
            }));
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

async fn build_replacement_catalog_events(
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
    catalog_stream_until_shutdown(state.shutdown_intent.subscribe())
}

fn empty_catalog_stream() -> Response {
    Sse::new(empty_catalog_snapshot().chain(stream::pending())).into_response()
}

fn empty_catalog_snapshot() -> impl futures_util::Stream<Item = Result<Event, Infallible>> {
    let snapshot = SessionCatalogSnapshot {
        workspace_paths: Default::default(),
        revision: SessionCatalogRevision::INITIAL,
        session_ids: Vec::new(),
        checkout_states: Vec::new(),
    };
    stream::once(async move {
        Ok::<_, Infallible>(
            Event::default()
                .event(SESSION_CATALOG_SNAPSHOT_EVENT)
                .id(snapshot.revision.0.to_string())
                .json_data(snapshot)
                .expect("serialize empty Session catalog snapshot"),
        )
    })
}

fn catalog_stream_until_shutdown(shutdown: watch::Receiver<Option<ServerShutdown>>) -> Response {
    let end = stream::unfold(shutdown, |mut shutdown| async move {
        shutdown.changed().await.ok()?;
        None::<(
            Result<Event, Infallible>,
            watch::Receiver<Option<ServerShutdown>>,
        )>
    });
    Sse::new(empty_catalog_snapshot().chain(end)).into_response()
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

/// Kills the server registered for `channel` outright, as a crash would, so
/// a test can watch its clients recover. Stopping what a test launched once it
/// is over is [`DetachedServers`]'s, not this.
fn crash_registered_server(state_dir: &std::path::Path, channel: &str) {
    let descriptor_path = test_runtime_root(state_dir, channel).join("runtime.json");
    let descriptor = read_runtime_descriptor(descriptor_path);
    let mut system = System::new_all();
    system.refresh_all();
    let process = system
        .process(Pid::from_u32(descriptor.pid))
        .expect("find detached test server");
    assert!(process.kill(), "crash detached test server");
}

fn describe_test_registration(state_dir: &std::path::Path, channel: &str) -> String {
    let descriptor_path = test_runtime_root(state_dir, channel).join("runtime.json");
    let Ok(contents) = std::fs::read(&descriptor_path) else {
        return format!("runtime registration missing at {descriptor_path:?}");
    };
    let Ok(descriptor) = serde_json::from_slice::<RuntimeDescriptor>(&contents) else {
        return format!("runtime registration unreadable at {descriptor_path:?}");
    };
    let mut system = System::new_all();
    system.refresh_all();
    let process_state = if system.process(Pid::from_u32(descriptor.pid)).is_some() {
        "running"
    } else {
        "exited"
    };
    format!(
        "runtime registration points to {process_state} pid {}, instance {}",
        descriptor.pid, descriptor.instance_id
    )
}

fn test_runtime_root(base_dir: &std::path::Path, channel: &str) -> std::path::PathBuf {
    if channel == "release" {
        base_dir.to_path_buf()
    } else {
        base_dir.join(channel)
    }
}

#[tokio::test]
async fn suru_config_dir_steers_a_real_server_and_config_problems_reach_the_log() {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let servers = DetachedServers::new();
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "transcript": { "defaultFoldPosture": "expanded" },
            "mysteryKnob": 1
        }"#,
    )
    .expect("write Config Document");
    let channel = "config-env-test";
    let mut process = server_cli(&servers, channel, "start");
    process.env("SURU_CONFIG_DIR", config_dir.path());
    let started = settle_server_cli(process, servers.state_dir(), channel, "start").await;
    assert!(
        started.status.success(),
        "server start failed despite the imperfect Config Document: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let mut client = ManagedClient::connect(servers.client_config(channel))
        .await
        .expect("connect to the environment-configured server");
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("connecting event arrives"),
        Some(ManagedEvent::Connecting)
    ));
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("connected event arrives"),
        Some(ManagedEvent::Connected(_))
    ));
    let event = timeout(PROGRESS_DEADLINE, client.next())
        .await
        .expect("settings snapshot arrives")
        .expect("managed client remains open");
    let ManagedEvent::SettingsSnapshot(snapshot) = event else {
        panic!("expected a settings snapshot event, got {event:?}");
    };
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        suru::protocol::FoldPosture::Expanded,
        "the override directory's pin reaches a connecting client"
    );
    assert!(
        snapshot
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.key.as_deref() == Some("mysteryKnob")),
        "the unknown key is reported in the snapshot: {:?}",
        snapshot.diagnostics
    );

    let log_dir = test_runtime_root(servers.state_dir(), channel).join("log");
    let log_contents = timeout(PROGRESS_DEADLINE, async {
        loop {
            let combined = std::fs::read_dir(&log_dir)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().contains("-server-"))
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .collect::<String>();
            if combined.contains("mysteryKnob") {
                return combined;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the server Log names the ignored key");
    assert!(
        log_contents.contains("configuration problem"),
        "the Log carries the configuration diagnostic: {log_contents}"
    );
}

#[tokio::test]
async fn a_syntax_broken_config_document_reaches_the_log_and_the_server_still_starts() {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let servers = DetachedServers::new();
    std::fs::write(config_dir.path().join("suru.jsonc"), r#"{ "transcript": "#)
        .expect("write broken Config Document");
    let channel = "config-broken-log-test";
    let mut process = server_cli(&servers, channel, "start");
    process.env("SURU_CONFIG_DIR", config_dir.path());
    let started = settle_server_cli(process, servers.state_dir(), channel, "start").await;
    assert!(
        started.status.success(),
        "a broken Config Document must never prevent startup: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let log_dir = test_runtime_root(servers.state_dir(), channel).join("log");
    let log_contents = timeout(PROGRESS_DEADLINE, async {
        loop {
            let combined = std::fs::read_dir(&log_dir)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().contains("-server-"))
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .collect::<String>();
            if combined.contains("configuration problem") {
                return combined;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the server Log reports the broken Config Document");
    assert!(
        log_contents.contains("ERROR") && log_contents.contains("not valid JSONC"),
        "the Log states the file was ignored and why: {log_contents}"
    );
}
