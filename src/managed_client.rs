//! Client-side ownership of server discovery and event streaming.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use fs2::FileExt;
use futures_util::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    RuntimeConfig,
    protocol::{
        COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, Health, LifecycleState,
        RuntimeDescriptor, SNAPSHOT_EVENT,
    },
    runtime::protect_current_user_file,
};

const SERVER_LOG_FILE: &str = "server.log";
const SERVER_LOG_TAIL_BYTES: u64 = 8 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_RECOVERY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct ManagedClientConfig {
    runtime: RuntimeConfig,
    server_executable: PathBuf,
}

impl ManagedClientConfig {
    pub fn new(state_dir: impl AsRef<Path>, channel: impl Into<String>) -> Result<Self> {
        Ok(Self {
            runtime: RuntimeConfig::new(state_dir, channel)?,
            server_executable: std::env::current_exe()
                .context("find current Chidori executable")?,
        })
    }

    pub fn with_server_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.server_executable = executable.into();
        self
    }

    pub fn state_dir(&self) -> &Path {
        self.runtime.state_dir()
    }

    pub fn channel(&self) -> &str {
        self.runtime.channel()
    }

    fn descriptor_path(&self) -> PathBuf {
        self.runtime.descriptor_path()
    }

    fn create_private_runtime_dir(&self) -> Result<PathBuf> {
        self.runtime.create_private_runtime_dir()
    }

    fn lock_path(&self) -> PathBuf {
        self.runtime.lock_path()
    }

    fn log_path(&self) -> PathBuf {
        self.runtime.runtime_dir().join(SERVER_LOG_FILE)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedEvent {
    Connecting,
    Connected(Health),
    Snapshot(CounterSnapshot),
    CounterUpdated(CounterUpdate),
    Recovering { attempt: u32, retry_in: Duration },
    Fatal(String),
}

pub struct ManagedClient {
    events: mpsc::Receiver<ManagedEvent>,
    task: JoinHandle<()>,
}

impl ManagedClient {
    pub async fn connect(config: ManagedClientConfig) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let http = reqwest::Client::new();
        let connection = establish_connection(&config, &http, deadline).await?;
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(run_managed_client(config, http, connection, events_tx));

        Ok(Self {
            events: events_rx,
            task,
        })
    }

    pub async fn next(&mut self) -> Option<ManagedEvent> {
        self.events.recv().await
    }
}

struct ActiveConnection {
    descriptor: RuntimeDescriptor,
    health: Health,
    response: reqwest::Response,
}

async fn establish_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    deadline: tokio::time::Instant,
) -> Result<ActiveConnection> {
    let (descriptor, health) = ensure_server(config, deadline).await?;
    let response = match tokio::time::timeout_at(
        deadline,
        http.get(format!("{}/v1/events", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send(),
    )
    .await
    {
        Ok(Ok(response)) => response.error_for_status().map_err(|error| {
            startup_error(
                config,
                &format!("server rejected the initial event stream: {error}"),
            )
        })?,
        Ok(Err(error)) => {
            return Err(startup_error(
                config,
                &format!("could not open the initial event stream: {error}"),
            ));
        }
        Err(_) => {
            return Err(startup_error(
                config,
                "initial event stream did not open within 15s",
            ));
        }
    };
    Ok(ActiveConnection {
        descriptor,
        health,
        response,
    })
}

pub async fn start_server(config: &ManagedClientConfig) -> Result<Health> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    ensure_server(config, deadline)
        .await
        .map(|(_, health)| health)
}

async fn ensure_server(
    config: &ManagedClientConfig,
    deadline: tokio::time::Instant,
) -> Result<(RuntimeDescriptor, Health)> {
    let mut spawned = None;
    loop {
        let probe_result = match tokio::time::timeout_at(deadline, probe(config)).await {
            Ok(result) => result,
            Err(_) => {
                return Err(startup_error(
                    config,
                    "detached Chidori server did not become ready within 15s",
                ));
            }
        };
        let error = match probe_result {
            Ok((descriptor, health)) => match health.lifecycle {
                LifecycleState::Ready => return Ok((descriptor, health)),
                LifecycleState::Starting => {
                    anyhow::anyhow!("registered Chidori server is still starting")
                }
                LifecycleState::Stopping => {
                    anyhow::anyhow!("registered Chidori server is stopping")
                }
                LifecycleState::Failed => {
                    return Err(startup_error(
                        config,
                        "registered Chidori server reported failed startup",
                    ));
                }
            },
            Err(error) => {
                if spawned.is_none() {
                    spawned = Some(spawn_detached(config).map_err(|spawn_error| {
                        startup_error(
                            config,
                            &format!(
                                "could not launch the detached Chidori server: {spawn_error:#}"
                            ),
                        )
                    })?);
                }
                error
            }
        };
        if let Some(process) = spawned.as_mut()
            && let Some(status) = process
                .try_wait()
                .context("inspect detached server process")?
            && !another_server_owns_channel(config)
        {
            return Err(startup_error(
                config,
                &format!(
                    "detached Chidori server exited before becoming ready ({})",
                    describe_exit(status)
                ),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(startup_error(
                config,
                &format!("detached Chidori server did not become ready within 15s: {error:#}"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn probe(config: &ManagedClientConfig) -> Result<(RuntimeDescriptor, Health)> {
    let descriptor = read_descriptor(&config.descriptor_path())?;
    validate_loopback_url(&descriptor.base_url)?;
    let health = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .context("request server health")?
        .error_for_status()
        .context("server rejected health request")?
        .json::<Health>()
        .await
        .context("decode server health")?;
    validate_identity(&descriptor, &health)?;
    Ok((descriptor, health))
}

fn spawn_detached(config: &ManagedClientConfig) -> Result<Child> {
    let runtime_dir = config.create_private_runtime_dir()?;

    let log_path = runtime_dir.join(SERVER_LOG_FILE);
    let mut log_options = OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600);
    }
    let stdout = log_options
        .open(&log_path)
        .with_context(|| format!("open server log {log_path:?}"))?;
    protect_current_user_file(&log_path)?;
    let stderr = stdout.try_clone().context("clone server log handle")?;

    let mut command = Command::new(&config.server_executable);
    command
        .arg("__server")
        .arg("--state-dir")
        .arg(config.state_dir())
        .arg("--channel")
        .arg(config.channel())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_detached_process(&mut command);
    command.spawn().context("spawn detached Chidori server")
}

fn another_server_owns_channel(config: &ManagedClientConfig) -> bool {
    let Ok(lock) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.lock_path())
    else {
        return false;
    };
    lock.try_lock_exclusive().is_err()
}

fn describe_exit(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by a signal".to_owned(),
        |code| format!("exit code {code}"),
    )
}

fn startup_error(config: &ManagedClientConfig, message: &str) -> anyhow::Error {
    let log_path = config.log_path();
    match read_log_tail(&log_path) {
        Ok(tail) if !tail.is_empty() => anyhow!(
            "{message}. Inspect {log_path:?} and retry.\nRecent server log ({log_path:?}):\n{tail}"
        ),
        _ => anyhow!("{message}. Inspect {log_path:?} and retry."),
    }
}

fn read_log_tail(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open server log {path:?}"))?;
    let length = file.metadata().context("read server log metadata")?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(SERVER_LOG_TAIL_BYTES),
    ))
    .context("seek to recent server log")?;
    let mut tail = Vec::with_capacity(SERVER_LOG_TAIL_BYTES as usize);
    file.read_to_end(&mut tail)
        .context("read recent server log")?;
    Ok(String::from_utf8_lossy(&tail).trim().to_owned())
}

#[cfg(unix)]
fn configure_detached_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run_managed_client(
    config: ManagedClientConfig,
    http: reqwest::Client,
    mut connection: ActiveConnection,
    events: mpsc::Sender<ManagedEvent>,
) {
    if events.send(ManagedEvent::Connecting).await.is_err() {
        return;
    }

    loop {
        if events
            .send(ManagedEvent::Connected(connection.health))
            .await
            .is_err()
        {
            return;
        }
        match stream_events(
            connection.response,
            &events,
            connection.descriptor.instance_id,
        )
        .await
        {
            Ok(StreamOutcome::Disconnected) => {}
            Ok(StreamOutcome::ReceiverClosed) => return,
            Err(error) => {
                let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                return;
            }
        }

        let mut attempt = 1;
        let mut retry_in = Duration::ZERO;
        loop {
            if events
                .send(ManagedEvent::Recovering { attempt, retry_in })
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(retry_in).await;

            let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
            match establish_connection(&config, &http, deadline).await {
                Ok(recovered) => {
                    connection = recovered;
                    break;
                }
                Err(_) => {
                    attempt = attempt.saturating_add(1);
                    retry_in = next_recovery_backoff(retry_in);
                }
            }
        }
    }
}

fn next_recovery_backoff(previous: Duration) -> Duration {
    if previous.is_zero() {
        INITIAL_RECOVERY_BACKOFF
    } else {
        previous.saturating_mul(2).min(MAX_RECOVERY_BACKOFF)
    }
}

enum StreamOutcome {
    Disconnected,
    ReceiverClosed,
}

async fn stream_events(
    response: reqwest::Response,
    events: &mpsc::Sender<ManagedEvent>,
    expected_instance_id: uuid::Uuid,
) -> Result<StreamOutcome> {
    let mut stream = response.bytes_stream().eventsource();
    let mut last_revision = None;
    let mut saw_snapshot = false;
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(event) => event,
            Err(EventStreamError::Transport(_)) => return Ok(StreamOutcome::Disconnected),
            Err(error) => bail!("server event stream failed: {error}"),
        };
        let managed_event = match decode_event(
            event,
            expected_instance_id,
            &mut saw_snapshot,
            &mut last_revision,
        ) {
            Ok(event) => event,
            Err(error) => {
                return Err(error);
            }
        };
        if events.send(managed_event).await.is_err() {
            return Ok(StreamOutcome::ReceiverClosed);
        }
    }
    Ok(StreamOutcome::Disconnected)
}

fn decode_event(
    event: Event,
    expected_instance_id: uuid::Uuid,
    saw_snapshot: &mut bool,
    last_revision: &mut Option<u64>,
) -> Result<ManagedEvent> {
    let event_revision = event
        .id
        .parse::<u64>()
        .context("server event has an invalid revision ID")?;
    match event.event.as_str() {
        SNAPSHOT_EVENT => {
            if *saw_snapshot {
                bail!("server sent more than one snapshot");
            }
            let snapshot: CounterSnapshot =
                serde_json::from_str(&event.data).context("decode counter snapshot")?;
            if snapshot.instance_id != expected_instance_id {
                bail!("counter snapshot came from an unexpected server instance");
            }
            if snapshot.revision != event_revision {
                bail!("counter snapshot revision does not match its SSE ID");
            }
            *saw_snapshot = true;
            *last_revision = Some(snapshot.revision);
            Ok(ManagedEvent::Snapshot(snapshot))
        }
        COUNTER_UPDATED_EVENT => {
            if !*saw_snapshot {
                bail!("server sent a counter update before its snapshot");
            }
            let update: CounterUpdate =
                serde_json::from_str(&event.data).context("decode counter update")?;
            if update.revision != event_revision {
                bail!("counter update revision does not match its SSE ID");
            }
            if last_revision.is_some_and(|previous| update.revision <= previous) {
                bail!("counter update revision is not monotonic");
            }
            *last_revision = Some(update.revision);
            Ok(ManagedEvent::CounterUpdated(update))
        }
        name => bail!("server sent unknown event type '{name}'"),
    }
}

fn read_descriptor(path: &Path) -> Result<RuntimeDescriptor> {
    let file = File::open(path)
        .with_context(|| format!("no running Chidori server was found at {path:?}"))?;
    serde_json::from_reader(file).context("decode runtime descriptor")
}

fn validate_loopback_url(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url).context("runtime descriptor has an invalid URL")?;
    if url.scheme() != "http" || url.host_str() != Some("127.0.0.1") || url.port().is_none() {
        bail!("runtime descriptor URL is not an HTTP IPv4 loopback address");
    }
    Ok(())
}

fn validate_identity(descriptor: &RuntimeDescriptor, health: &Health) -> Result<()> {
    if health.instance_id != descriptor.instance_id
        || health.pid != descriptor.pid
        || health.protocol_version != descriptor.protocol_version
        || health.build_identity != descriptor.build_identity
    {
        bail!("registered server identity does not match its runtime descriptor");
    }
    Ok(())
}
