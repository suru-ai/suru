//! A Manual stop is final for every Server launched before it: one still
//! waiting in the Channel's election when the winner is stopped — or one
//! launched before the stop that reaches the election only after it — ends
//! once elected rather than serving the Channel its user just stopped. A
//! Replacement hands the Channel to whichever Server waits for it, a Server
//! that no longer stands for its Channel stops without stopping it, and a
//! Server launched after the stop serves.
use super::*;
use crate::support::PROGRESS_DEADLINE;
use suru::{LastStop, server::RunningServer};

/// Timings for a Server that waits in the election for as long as a test
/// needs it to, and stops without lingering.
fn waiting_timings() -> ServerTimings {
    ServerTimings {
        election_handoff: PROGRESS_DEADLINE,
        shutdown_grace: Duration::from_millis(10),
        ..ServerTimings::default()
    }
}

fn config(state_dir: &tempfile::TempDir, channel: &str) -> ServerConfig {
    ServerConfig::new(state_dir.path(), channel).expect("configure server")
}

fn last_stop(config: &ServerConfig) -> LastStop {
    config
        .last_stop()
        .expect("read the channel's last manual stop")
}

/// Starts a Server in the election and waits until it is waiting there,
/// held by `winner`'s hold on the Channel.
async fn contend(config: ServerConfig) -> tokio::task::JoinHandle<anyhow::Result<RunningServer>> {
    let contending = tokio::spawn(server::spawn_with_timings(config, waiting_timings()));
    // Everything a starting Server does before it waits on the lock is done
    // without yielding, so once it has run at all it is waiting there.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !contending.is_finished(),
        "the contender is waiting out the winner's hold"
    );
    contending
}

async fn settle(
    contending: tokio::task::JoinHandle<anyhow::Result<RunningServer>>,
) -> anyhow::Result<RunningServer> {
    timeout(PROGRESS_DEADLINE, contending)
        .await
        .expect("the contender settles once the winner lets the channel go")
        .expect("the contending task does not panic")
}

fn assert_ended_by_the_stop(settled: anyhow::Result<RunningServer>) {
    let error = settled
        .err()
        .expect("a server launched before a manual stop is not elected after it");
    assert!(
        error
            .to_string()
            .contains("manual stop is final for every server launched before it"),
        "unexpected error: {error:#}"
    );
}

/// A client's `suru server stop` of the winner, while another Server waits
/// in the election, leaves no Server serving: the waiting one ends once it
/// takes the lock, having published nothing.
#[tokio::test]
async fn a_manual_stop_is_final_for_a_server_waiting_in_the_election() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "stop-while-waiting-test";
    let config = config(&state_dir, channel);
    let winner = server::spawn_with_timings(config.clone(), waiting_timings())
        .await
        .expect("spawn the election winner");
    let stopped = winner.descriptor().instance_id;
    let contending = contend(config.clone().launched_after(last_stop(&config))).await;

    let response = request_server_shutdown(winner.descriptor(), ShutdownReason::Manual).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    winner.shutdown().await.expect("the winner stops");

    assert_ended_by_the_stop(settle(contending).await);
    assert_eq!(last_stop(&config).stopped_instance(), Some(stopped));
    assert!(
        !config.descriptor_path().exists(),
        "no server is published after the manual stop"
    );
}

/// A Server launched before a Manual stop is held to it however late it
/// reaches the election — a slow launch, there after the stop is over — and
/// whether its launcher told it the stop it followed or it read the stop
/// itself as its launch was asked for. A Server launched after the stop
/// serves.
#[tokio::test]
async fn a_server_launched_before_a_manual_stop_ends_however_late_it_is_elected() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "late-after-stop-test";
    let config = config(&state_dir, channel);
    let before = last_stop(&config);
    assert_eq!(before.stopped_instance(), None);
    let winner = server::spawn(config.clone())
        .await
        .expect("spawn the election winner");
    let stopped = winner.descriptor().instance_id;
    let read_itself = contend(config.clone()).await;
    // A signal is answered as a client's stop request is.
    winner.shutdown().await.expect("the winner stops");

    assert_ended_by_the_stop(settle(read_itself).await);
    assert_ended_by_the_stop(
        server::spawn_with_timings(config.clone().launched_after(before), waiting_timings()).await,
    );

    let after = last_stop(&config);
    assert_eq!(after.stopped_instance(), Some(stopped));
    let successor = server::spawn(config.clone().launched_after(after))
        .await
        .expect("a server launched after the stop is elected");
    assert_eq!(
        read_runtime_descriptor(config.descriptor_path()).instance_id,
        successor.descriptor().instance_id
    );
    successor.shutdown().await.expect("shut down the successor");
    let fresh = server::spawn(config.clone())
        .await
        .expect("a server reading the stops itself is elected after them");
    fresh.shutdown().await.expect("shut down the fresh server");
}

/// A launch asked for before a Manual stop is held to it even where nothing
/// polls it until the stop is over — one slow to be scheduled, or to look at
/// its Providers, before it reaches the election — since it follows the stop
/// recorded when it was asked for, not when it gets round to starting.
#[tokio::test]
async fn a_launch_asked_for_before_a_manual_stop_is_held_to_it_however_late_it_runs() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "delayed-launch-test";
    let config = config(&state_dir, channel);
    let winner = server::spawn(config.clone())
        .await
        .expect("spawn the election winner");
    let delayed = server::spawn_with_timings(config.clone(), waiting_timings());

    winner.shutdown().await.expect("the winner stops");
    assert!(last_stop(&config).stopped_instance().is_some());
    assert_ended_by_the_stop(
        timeout(PROGRESS_DEADLINE, delayed)
            .await
            .expect("the delayed launch settles"),
    );

    let successor = server::spawn(config.clone())
        .await
        .expect("a launch asked for after the stop is elected");
    successor.shutdown().await.expect("shut down the successor");
}

/// A Manual stop that cannot be recorded — here a directory stands where
/// the record goes, so nothing can be put in its place — is reported to the
/// client that asked for it as a failed stop, since a Server launched before
/// it may yet serve; the Server stops all the same, as it was asked to.
#[tokio::test]
async fn a_manual_stop_that_cannot_be_recorded_is_reported_as_failed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "unrecorded-stop-test";
    let config = config(&state_dir, channel);
    let winner = server::spawn_with_timings(config.clone(), waiting_timings())
        .await
        .expect("spawn the server");
    let descriptor = winner.descriptor().clone();
    std::fs::create_dir(state_dir.path().join(channel).join("last-stop.json"))
        .expect("stand a directory where the stop is recorded");

    let error = stop_server(
        &ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure stop client")
            .with_health_check_timeout(Duration::from_millis(50))
            .with_stop_timeout(PROGRESS_DEADLINE),
    )
    .await
    .expect_err("a stop that could not be recorded is not reported as a success");
    assert!(
        error
            .to_string()
            .contains("manual stop could not be recorded"),
        "unexpected error: {error:#}"
    );

    winner
        .shutdown()
        .await
        .expect("the server stops all the same");
    assert!(
        reqwest::Client::new()
            .get(format!("{}/health", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .is_err(),
        "the server stopped"
    );
}

/// Every client asking for a Manual stop at once hears the one outcome of
/// recording it — here that it could not be — not only the one whose
/// request was accepted first: a request that finds the stop already
/// accepted waits for its record rather than reporting a success the record
/// may yet fail. Run on several threads, as a real Server's requests are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_concurrent_manual_stop_hears_that_it_could_not_be_recorded() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "concurrent-unrecorded-stop-test";
    let config = config(&state_dir, channel);
    let server = server::spawn_with_timings(
        config.clone(),
        ServerTimings {
            // Long enough that every request below reaches the Server before
            // it stops taking them.
            shutdown_grace: Duration::from_millis(500),
            ..ServerTimings::default()
        },
    )
    .await
    .expect("spawn the server");
    let descriptor = server.descriptor().clone();
    std::fs::create_dir(state_dir.path().join(channel).join("last-stop.json"))
        .expect("stand a directory where the stop is recorded");

    let responses = futures_util::future::join_all(
        (0..8).map(|_| request_server_shutdown(&descriptor, ShutdownReason::Manual)),
    )
    .await;
    for response in responses {
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "a concurrent stop was reported as a success"
        );
        let reason = response.text().await.expect("read why the stop failed");
        assert!(
            reason.contains("manual stop could not be recorded"),
            "unexpected reason: {reason}"
        );
    }
    server
        .shutdown()
        .await
        .expect("the server stops all the same");
}

/// A Replacement records no stop: the Server waiting in the election when
/// another build replaces the winner is elected and serves.
#[tokio::test]
async fn a_replacement_hands_the_channel_to_a_server_waiting_in_the_election() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "replacement-handoff-test";
    let config = config(&state_dir, channel);
    let winner = server::spawn_with_timings(config.clone(), waiting_timings())
        .await
        .expect("spawn the election winner");
    let contending = contend(config.clone().launched_after(last_stop(&config))).await;

    let response = request_server_shutdown(winner.descriptor(), ShutdownReason::Replacement).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    winner.shutdown().await.expect("the winner stops");

    let successor = settle(contending)
        .await
        .expect("the waiting server is elected by a replacement");
    assert_eq!(last_stop(&config), LastStop::default());
    assert_eq!(
        read_runtime_descriptor(config.descriptor_path()).instance_id,
        successor.descriptor().instance_id
    );
    successor.shutdown().await.expect("shut down the successor");
}

/// A Server that stops because it no longer stands for its Channel — its
/// descriptor names another instance — tells its clients the stop was
/// Manual but records no stop for the Channel, so the Server waiting in the
/// election there is elected.
#[tokio::test]
async fn a_server_that_no_longer_stands_for_its_channel_records_no_stop() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "superseded-handoff-test";
    let config = config(&state_dir, channel);
    let winner = server::spawn_with_timings(
        config.clone(),
        waiting_timings().with_state_dir_check_interval(Duration::from_millis(10)),
    )
    .await
    .expect("spawn the election winner");
    let contending = contend(config.clone().launched_after(last_stop(&config))).await;

    let mut another = winner.descriptor().clone();
    another.instance_id = uuid::Uuid::new_v4();
    write_runtime_descriptor(config.descriptor_path(), &another);

    let successor = settle(contending)
        .await
        .expect("the waiting server is elected once the superseded winner stands down");
    assert_eq!(last_stop(&config), LastStop::default());
    winner.shutdown().await.expect("join the superseded winner");
    successor.shutdown().await.expect("shut down the successor");
}

/// A launch's command line carries the stop it followed as `none` or the
/// stopped instance's id, and reads back the same.
#[test]
fn a_last_stop_reads_back_as_it_is_written() {
    let none: LastStop = "none".parse().expect("parse no stop");
    assert_eq!(none, LastStop::default());
    assert_eq!(none.to_string(), "none");
    let instance_id = uuid::Uuid::new_v4();
    let stopped: LastStop = instance_id.to_string().parse().expect("parse a stop");
    assert_eq!(stopped.stopped_instance(), Some(instance_id));
    assert_eq!(stopped.to_string(), instance_id.to_string());
    assert!("not-a-stop".parse::<LastStop>().is_err());

    assert_eq!(none.superseded_by(stopped), Some(instance_id));
    assert_eq!(stopped.superseded_by(stopped), None);
    assert_eq!(
        stopped.superseded_by(none),
        None,
        "a record gone missing ends no launch"
    );
}
