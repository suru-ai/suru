//! A Server stands for its Channel only while its state directory does: one
//! whose directory is removed from under it stops, one waiting to be elected
//! in a directory removed meanwhile ends, and neither makes the directory
//! again — while a directory that merely cannot be read for now, or a
//! descriptor caught mid-replacement, leaves a running Server serving on.
use super::*;
use crate::support::PROGRESS_DEADLINE;
use suru::{
    protocol::{AgentId, AgentIdentity, AgentSelection, ModelId, ProviderId, RuntimeDescriptor},
    server::RunningServer,
};

/// How often the Servers here look to see that they still stand for their
/// Channel.
const CHECK_INTERVAL: Duration = Duration::from_millis(10);

/// Long enough for a Server looking every [`CHECK_INTERVAL`] to have looked
/// many times over, for a test that shows a look finds nothing.
const MANY_CHECKS: Duration = Duration::from_millis(250);

fn timings() -> ServerTimings {
    ServerTimings::default().with_state_dir_check_interval(CHECK_INTERVAL)
}

/// The state and data roots of an in-process Server, kept apart so the state
/// directory can be removed while the Server's storage, which Windows keeps
/// from being deleted while it is open, stays where it is.
struct Roots {
    state: tempfile::TempDir,
    data: tempfile::TempDir,
}

impl Roots {
    fn new() -> Self {
        Self {
            state: tempfile::tempdir().expect("create isolated state root"),
            data: tempfile::tempdir().expect("create isolated data root"),
        }
    }

    fn config(&self, channel: &str) -> ServerConfig {
        ServerConfig::new(self.state.path(), channel)
            .expect("configure server")
            .with_data_dir(self.data.path())
    }
}

async fn spawn(roots: &Roots, channel: &str) -> RunningServer {
    server::spawn_with_timings(roots.config(channel), timings())
        .await
        .expect("spawn server")
}

/// The Server's health, or `None` once nothing answers at its address.
async fn health(descriptor: &RuntimeDescriptor) -> Option<Health> {
    let response = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .timeout(PROGRESS_DEADLINE)
        .send()
        .await
        .ok()?;
    Some(
        response
            .error_for_status()
            .expect("an answering Server is healthy")
            .json::<Health>()
            .await
            .expect("decode health"),
    )
}

async fn assert_serving(descriptor: &RuntimeDescriptor, why: &str) {
    let health = health(descriptor).await;
    assert_eq!(
        health.map(|health| health.lifecycle),
        Some(LifecycleState::Ready),
        "{why}"
    );
}

/// Waits for the Server to stop answering at its address, as it does once a
/// stop has run its course.
async fn wait_until_stopped(descriptor: &RuntimeDescriptor, why: &str) {
    timeout(PROGRESS_DEADLINE, async {
        while health(descriptor).await.is_some() {
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{why}"));
}

/// A Server whose state directory is removed while it runs stops the way a
/// client's stop request stops it — taking down the Provider Session it was
/// running — and makes nothing in the directory's place.
#[tokio::test]
async fn a_server_whose_state_directory_is_removed_stops_and_shuts_its_providers_down() {
    let roots = Roots::new();
    let channel = "state-removed-test";
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = provider_support::ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(roots.config(channel), runtime, timings())
        .await
        .expect("spawn server");
    let descriptor = server.descriptor().clone();
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until the state directory goes".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds");
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("controlled-model"),
            options: Vec::new(),
        },
    });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();

    let state_dir = roots.config(channel).state_dir().to_owned();
    std::fs::remove_dir_all(&state_dir).expect("remove the Server's state directory");

    wait_until_stopped(
        &descriptor,
        "a Server whose state directory was removed stops",
    )
    .await;
    timeout(PROGRESS_DEADLINE, provider_session.next_shutdown())
        .await
        .expect("the stopping Server shuts its Provider Session down");
    server
        .shutdown()
        .await
        .expect("the Server stopped gracefully");
    assert!(
        !state_dir.exists(),
        "the stopped Server made its state directory again"
    );
}

/// A Server waiting out another's hold on the Channel's lock, whose state
/// directory is removed before the hold ends, takes a lock on a file nothing
/// else can open. It ends once it holds it, rather than opening its storage
/// and publishing itself in a directory made again.
#[tokio::test]
async fn an_election_loser_whose_state_directory_is_removed_ends_without_making_it_again() {
    let roots = Roots::new();
    let channel = "state-removed-election-test";
    let config = roots.config(channel);
    let lock = hold_channel_lock(roots.state.path(), channel);
    let starting = tokio::spawn(server::spawn_with_timings(
        config.clone(),
        ServerTimings {
            election_handoff: PROGRESS_DEADLINE,
            ..timings()
        },
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !starting.is_finished(),
        "the contender is waiting out the hold"
    );

    std::fs::remove_dir_all(roots.state.path()).expect("remove the state root");
    drop(lock);

    let error = timeout(PROGRESS_DEADLINE, starting)
        .await
        .expect("the contender ends once the hold does")
        .expect("the starting task does not panic")
        .err()
        .expect("a contender whose state directory was removed is not elected");
    assert!(
        error
            .to_string()
            .contains("removed while this server waited to be elected"),
        "unexpected error: {error:#}"
    );
    assert!(
        !roots.state.path().exists(),
        "the contender made its state directory again"
    );
    let data_dir = std::fs::read_dir(config.data_dir())
        .expect("list the contender's data directory")
        .map(|entry| entry.expect("read data entry").file_name())
        .collect::<Vec<_>>();
    assert!(
        data_dir.is_empty(),
        "the contender opened its storage: {data_dir:?}"
    );
}

/// A Server launched into directories its launcher made, which are gone by
/// the time it starts, ends rather than making them again.
#[tokio::test]
async fn a_launched_server_whose_state_directory_is_gone_makes_nothing() {
    let roots = Roots::new();
    let channel = "launched-into-nothing-test";
    let config = roots.config(channel);
    config
        .create_private_runtime_dir()
        .expect("make the directories, as a launcher does");
    std::fs::remove_dir_all(config.state_dir()).expect("remove the state directory");

    let error = server::spawn_with_timings(config.clone().launched_into_existing_dirs(), timings())
        .await
        .err()
        .expect("a launched Server does not start without its state directory");
    assert!(
        error
            .to_string()
            .contains("a launched server uses the directories its launcher made"),
        "unexpected error: {error:#}"
    );
    assert!(
        !config.state_dir().exists(),
        "the launched Server made its state directory again"
    );
}

/// A descriptor caught mid-replacement — moved aside, or half written — says
/// nothing about who stands for the Channel, and a Server finding it so
/// serves on. One read whole that names another instance means another
/// Server was elected, and this one stops.
#[tokio::test]
async fn a_server_serves_on_through_a_descriptor_mid_replacement_but_not_one_naming_another() {
    let roots = Roots::new();
    let channel = "descriptor-replacement-test";
    let server = spawn(&roots, channel).await;
    let descriptor = server.descriptor().clone();
    let descriptor_path = roots.config(channel).descriptor_path();

    std::fs::remove_file(&descriptor_path).expect("move the descriptor aside");
    tokio::time::sleep(MANY_CHECKS).await;
    assert_serving(&descriptor, "a missing descriptor is no finding").await;

    let mut another = descriptor.clone();
    another.instance_id = uuid::Uuid::new_v4();
    let encoded = serde_json::to_vec(&another).expect("encode another's descriptor");
    std::fs::write(&descriptor_path, &encoded[..encoded.len() / 2])
        .expect("write half a descriptor");
    tokio::time::sleep(MANY_CHECKS).await;
    assert_serving(&descriptor, "a half-written descriptor is no finding").await;

    write_runtime_descriptor(&descriptor_path, &another);
    wait_until_stopped(
        &descriptor,
        "a Server whose descriptor names another instance stops",
    )
    .await;
    server
        .shutdown()
        .await
        .expect("the Server stopped gracefully");
    assert_eq!(
        read_runtime_descriptor(&descriptor_path).instance_id,
        another.instance_id,
        "the stopped Server leaves the other instance's descriptor in place"
    );
}

/// A state directory that cannot be read for now — its permissions changed,
/// as a mount gone away also leaves it unanswering — says nothing about the
/// Server's standing, so the Server serves on; once it answers again, its
/// removal still stops the Server.
#[cfg(unix)]
#[tokio::test]
async fn a_server_serves_on_while_its_state_directory_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives the directory back its permissions however the test ends, so
    /// it can be removed.
    struct Restore<'a>(&'a std::path::Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    let roots = Roots::new();
    let channel = "state-unreadable-test";
    let server = spawn(&roots, channel).await;
    let descriptor = server.descriptor().clone();
    let state_dir = roots.config(channel).state_dir().to_owned();

    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o000))
        .expect("make the state directory unreadable");
    let restore = Restore(&state_dir);
    tokio::time::sleep(MANY_CHECKS).await;
    assert_serving(&descriptor, "an unreadable state directory is no finding").await;
    drop(restore);

    std::fs::remove_dir_all(&state_dir).expect("remove the state directory");
    wait_until_stopped(
        &descriptor,
        "the Server still stops once its state directory is removed",
    )
    .await;
    server
        .shutdown()
        .await
        .expect("the Server stopped gracefully");
}

/// A state directory moved elsewhere — as moving it to the Trash does —
/// leaves the Server's lock with its name and its descriptor unanswered where
/// it was, which is no finding: the Server serves on, until a Server elected
/// in a directory made afresh where the old one stood publishes itself there.
/// Windows does not move a directory holding an open file.
#[cfg(unix)]
#[tokio::test]
async fn a_server_whose_state_directory_is_moved_serves_on_until_another_is_elected_there() {
    let roots = Roots::new();
    let channel = "state-moved-test";
    let server = spawn(&roots, channel).await;
    let descriptor = server.descriptor().clone();
    let state_dir = roots.config(channel).state_dir().to_owned();

    std::fs::rename(&state_dir, roots.state.path().join("moved-away"))
        .expect("move the state directory away");
    tokio::time::sleep(MANY_CHECKS).await;
    assert_serving(&descriptor, "a moved state directory is no finding").await;

    // The successor keeps storage of its own, so the two never share it.
    let successor_data = tempfile::tempdir().expect("create the successor's data root");
    let successor = server::spawn_with_timings(
        ServerConfig::new(roots.state.path(), channel)
            .expect("configure the successor")
            .with_data_dir(successor_data.path()),
        timings(),
    )
    .await
    .expect("a successor is elected in a state directory made afresh");
    wait_until_stopped(
        &descriptor,
        "the moved-away Server stops once a successor publishes itself",
    )
    .await;
    server
        .shutdown()
        .await
        .expect("the moved-away Server stopped gracefully");
    assert_serving(
        successor.descriptor(),
        "the successor serves on, its descriptor its own",
    )
    .await;
    successor.shutdown().await.expect("shut down the successor");
}

/// With its state and data in one directory, as they are by default on
/// macOS, a Server whose directory is removed makes nothing there while it
/// has yet to notice: a request that would make the Sidekick Workspace
/// fails rather than making the data root again, and the Server, stopping,
/// leaves neither the directory nor its storage behind. Windows does not
/// remove a directory holding the Server's open storage.
#[cfg(unix)]
#[tokio::test]
async fn a_server_sharing_one_root_makes_nothing_there_before_it_notices_the_root_is_gone() {
    let root = tempfile::tempdir().expect("create isolated root");
    let channel = "shared-root-removed-test";
    let config = ServerConfig::new(root.path(), channel).expect("configure server");
    // Long enough that it does not look again within the test, so what it
    // does in the meantime is what is tested.
    let server = server::spawn_with_timings(
        config.clone(),
        ServerTimings::default().with_state_dir_check_interval(Duration::from_secs(60 * 60)),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    assert_eq!(config.state_dir(), config.data_dir());

    std::fs::remove_dir_all(config.state_dir()).expect("remove the shared directory");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/sidekick", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("request the Sidekick Workspace");
    assert!(
        !response.status().is_success(),
        "the Sidekick Workspace was made without its data root: {}",
        response.status()
    );
    assert!(
        !config.state_dir().exists(),
        "the request made the data root again"
    );

    server
        .shutdown()
        .await
        .expect("the Server stopped gracefully");
    assert!(
        !config.state_dir().exists(),
        "the stopped Server left its directory behind"
    );
}

/// A look at the Server's standing stuck on a descriptor that never answers
/// — here a FIFO held open for writing but never written to, as a directory
/// on a mount gone away may keep a read waiting — does not keep the stopped
/// Server's election held: a successor is elected at once.
#[cfg(unix)]
#[tokio::test]
async fn a_look_stuck_on_its_descriptor_does_not_keep_a_stopped_server_elected() {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

    /// Opens the FIFO for writing and lets it go, so a look still waiting to
    /// open it reads to its end and its thread finishes however the test
    /// ends, even before the writer below is had.
    struct Unblock(std::path::PathBuf);
    impl Drop for Unblock {
        fn drop(&mut self) {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.0);
        }
    }

    let roots = Roots::new();
    let channel = "stuck-look-test";
    let server = spawn(&roots, channel).await;
    let descriptor_path = roots.config(channel).descriptor_path();

    std::fs::remove_file(&descriptor_path).expect("move the descriptor aside");
    let fifo = std::ffi::CString::new(descriptor_path.as_os_str().as_bytes())
        .expect("the descriptor path has no NUL");
    assert_eq!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
        0,
        "make a FIFO where the descriptor was"
    );
    // A second name, by which the FIFO is still reached once a successor
    // publishes its own descriptor over the first.
    let kept = roots.state.path().join("kept-fifo");
    std::fs::hard_link(&descriptor_path, &kept).expect("keep the FIFO reachable");
    let _unblock = Unblock(kept.clone());
    // Opening a FIFO to write without blocking fails until something has it
    // open to read, so the first such open that succeeds proves a look is
    // reading the FIFO. Held open and never written to, it keeps that look
    // waiting through the stop and the election that follow; dropped as the
    // test ends, it lets the look read to the end.
    let _writer = timeout(PROGRESS_DEADLINE, async {
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&kept)
            {
                Ok(writer) => return writer,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    tokio::time::sleep(CHECK_INTERVAL).await;
                }
                Err(error) => panic!("open the FIFO to write: {error}"),
            }
        }
    })
    .await
    .expect("a look reads the FIFO");
    // The Server's own descriptor goes back over the FIFO's first name, so
    // its stop reads a descriptor rather than waiting on the FIFO too; the
    // look stays stuck on the FIFO it opened.
    let restored = roots.state.path().join("restored.json");
    write_runtime_descriptor(&restored, server.descriptor());
    std::fs::rename(&restored, &descriptor_path).expect("restore the descriptor");

    server.shutdown().await.expect("shut down the Server");
    let successor = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_timings(
            roots.config(channel),
            ServerTimings {
                election_handoff: Duration::from_millis(100),
                ..timings()
            },
        ),
    )
    .await
    .expect("the successor starts")
    .expect("the successor is elected while the stuck look holds the old lock open");
    successor.shutdown().await.expect("shut down the successor");
}
