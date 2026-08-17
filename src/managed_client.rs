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
        BUILD_IDENTITY, COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, Health,
        LifecycleState, PROTOCOL_VERSION, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT, SNAPSHOT_EVENT,
        ServerShutdown, ShutdownReason,
    },
    runtime::protect_current_user_file,
};

const SERVER_LOG_FILE: &str = "server.log";
const SERVER_LOG_TAIL_BYTES: u64 = 8 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
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
    Recovering(RecoveryStatus),
    ServerShutdown(ServerShutdown),
    Fatal(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryStatus {
    pub attempt: u32,
    pub retry_in: Duration,
}

pub struct ManagedClient {
    events: mpsc::Receiver<ManagedEvent>,
    task: JoinHandle<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerStatus {
    Missing,
    Starting(Health),
    Ready(Health),
    Stopping(Health),
    Failed(Health),
    Stale(String),
    Unreachable(String),
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                formatter,
                "Chidori server missing (no runtime registration)"
            ),
            Self::Starting(health) => write!(
                formatter,
                "Chidori server starting (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Ready(health) => write!(
                formatter,
                "Chidori server ready (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Stopping(health) => write!(
                formatter,
                "Chidori server stopping (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Failed(health) => write!(
                formatter,
                "Chidori server failed (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Stale(reason) => {
                write!(formatter, "Chidori server registration stale: {reason}")
            }
            Self::Unreachable(reason) => write!(formatter, "Chidori server unreachable: {reason}"),
        }
    }
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

pub async fn server_status(config: &ManagedClientConfig) -> Result<ServerStatus> {
    let health = match inspect_registration(config).await? {
        RegistrationInspection::Missing => return Ok(ServerStatus::Missing),
        RegistrationInspection::Live { health, .. } => health,
        RegistrationInspection::Stale(reason) => return Ok(ServerStatus::Stale(reason)),
        RegistrationInspection::Unreachable(reason) => {
            return Ok(ServerStatus::Unreachable(reason));
        }
    };
    Ok(match health.lifecycle {
        LifecycleState::Starting => ServerStatus::Starting(health),
        LifecycleState::Ready => ServerStatus::Ready(health),
        LifecycleState::Stopping => ServerStatus::Stopping(health),
        LifecycleState::Failed => ServerStatus::Failed(health),
    })
}

pub async fn stop_server(config: &ManagedClientConfig) -> Result<Health> {
    let (descriptor, health) = match inspect_registration(config).await? {
        RegistrationInspection::Missing => bail!("cannot stop missing Chidori server"),
        RegistrationInspection::Live { descriptor, health } => (descriptor, health),
        RegistrationInspection::Stale(reason) => {
            bail!("cannot stop stale Chidori server registration: {reason}")
        }
        RegistrationInspection::Unreachable(reason) => {
            bail!("cannot stop unreachable Chidori server: {reason}")
        }
    };
    let request = ServerShutdown {
        instance_id: health.instance_id,
        reason: ShutdownReason::Manual,
    };
    let response = tokio::time::timeout(
        STATUS_TIMEOUT,
        reqwest::Client::new()
            .post(format!("{}/v1/server/stop", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send(),
    )
    .await
    .context("manual stop request timed out")?
    .context("send manual stop request")?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        bail!("registered Chidori server changed before it could be stopped");
    }
    response
        .error_for_status()
        .context("server rejected manual stop request")?;

    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    let mut target_stopped = false;
    loop {
        if !target_stopped {
            target_stopped = matches!(
                tokio::time::timeout_at(deadline, inspect_health(&descriptor)).await,
                Ok(Err(HealthInspectionError::Unreachable(_)))
            );
        }
        let registration_released = match read_descriptor(&config.descriptor_path()) {
            Ok(current) => current.instance_id != request.instance_id,
            Err(_) => !config.descriptor_path().exists(),
        };
        if target_stopped && registration_released {
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!("Chidori server did not stop within 5s")
        }
        tokio::time::sleep_until((now + Duration::from_millis(25)).min(deadline)).await;
    }
    Ok(health)
}

enum RegistrationInspection {
    Missing,
    Live {
        descriptor: RuntimeDescriptor,
        health: Health,
    },
    Stale(String),
    Unreachable(String),
}

async fn inspect_registration(config: &ManagedClientConfig) -> Result<RegistrationInspection> {
    let descriptor_path = config.descriptor_path();
    if !descriptor_path
        .try_exists()
        .context("inspect runtime descriptor")?
    {
        return Ok(RegistrationInspection::Missing);
    }
    let descriptor = match read_descriptor(&descriptor_path) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            if !descriptor_path
                .try_exists()
                .context("reinspect runtime descriptor")?
            {
                return Ok(RegistrationInspection::Missing);
            }
            return Ok(RegistrationInspection::Stale(format!("{error:#}")));
        }
    };
    if let Err(error) = validate_loopback_url(&descriptor.base_url) {
        return Ok(RegistrationInspection::Stale(format!("{error:#}")));
    }
    match tokio::time::timeout(STATUS_TIMEOUT, inspect_health(&descriptor)).await {
        Ok(Ok(health)) => Ok(RegistrationInspection::Live { descriptor, health }),
        Ok(Err(HealthInspectionError::Stale(reason))) => Ok(RegistrationInspection::Stale(reason)),
        Ok(Err(HealthInspectionError::Unreachable(reason))) => {
            Ok(RegistrationInspection::Unreachable(reason))
        }
        Err(_) => Ok(RegistrationInspection::Unreachable(
            "authenticated health check timed out".to_owned(),
        )),
    }
}

enum HealthInspectionError {
    Stale(String),
    Unreachable(String),
}

impl HealthInspectionError {
    fn reason(self) -> String {
        match self {
            Self::Stale(reason) | Self::Unreachable(reason) => reason,
        }
    }
}

async fn inspect_health(
    descriptor: &RuntimeDescriptor,
) -> std::result::Result<Health, HealthInspectionError> {
    let response = reqwest::Client::new()
        .get(format!("{}/health", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .map_err(|error| HealthInspectionError::Unreachable(error.to_string()))?;
    if !response.status().is_success() {
        return Err(HealthInspectionError::Stale(format!(
            "authenticated health request returned {}",
            response.status()
        )));
    }
    let health = response.json::<Health>().await.map_err(|error| {
        HealthInspectionError::Stale(format!("invalid health response: {error}"))
    })?;
    validate_identity(descriptor, &health)
        .map_err(|error| HealthInspectionError::Stale(error.to_string()))?;
    Ok(health)
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
                LifecycleState::Ready if health.build_identity != BUILD_IDENTITY => {
                    replace_server(config, &descriptor, &health, deadline).await?;
                    anyhow::anyhow!("registered Chidori server is being replaced")
                }
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

async fn replace_server(
    config: &ManagedClientConfig,
    descriptor: &RuntimeDescriptor,
    health: &Health,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let request = ServerShutdown {
        instance_id: health.instance_id,
        reason: ShutdownReason::Replacement,
    };
    let response = tokio::time::timeout_at(
        deadline,
        reqwest::Client::new()
            .post(format!("{}/v1/server/stop", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send(),
    )
    .await
    .context("replacement stop request timed out")?
    .context("send replacement stop request")?;
    if response.status() != reqwest::StatusCode::CONFLICT {
        response
            .error_for_status()
            .context("server rejected replacement stop request")?;
    }

    let mut target_stopped = false;
    loop {
        if !target_stopped {
            target_stopped = matches!(
                tokio::time::timeout_at(deadline, inspect_health(descriptor)).await,
                Ok(Err(HealthInspectionError::Unreachable(_)))
            );
        }
        let registration_released = match read_descriptor(&config.descriptor_path()) {
            Ok(current) => current.instance_id != request.instance_id,
            Err(_) => !config.descriptor_path().exists(),
        };
        if target_stopped && registration_released && channel_lock_is_released(config)? {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!("mismatched Chidori server did not release the channel before startup timed out")
        }
        tokio::time::sleep_until((now + Duration::from_millis(25)).min(deadline)).await;
    }
}

fn channel_lock_is_released(config: &ManagedClientConfig) -> Result<bool> {
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.lock_path())
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).context("inspect server election lock"),
    };
    Ok(lock.try_lock_exclusive().is_ok())
}

async fn probe(config: &ManagedClientConfig) -> Result<(RuntimeDescriptor, Health)> {
    let descriptor = read_descriptor(&config.descriptor_path())?;
    validate_loopback_url(&descriptor.base_url)?;
    let health = inspect_health(&descriptor)
        .await
        .map_err(|error| anyhow!(error.reason()))?;
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

    let mut follows_replacement = false;
    loop {
        if events
            .send(ManagedEvent::Connected(connection.health))
            .await
            .is_err()
        {
            return;
        }
        let mut replaced_instance_id = None;
        match stream_events(
            connection.response,
            &events,
            connection.descriptor.instance_id,
        )
        .await
        {
            Ok(StreamOutcome::Disconnected) => {}
            Ok(StreamOutcome::ManualShutdown) => return,
            Ok(StreamOutcome::Replacement { instance_id }) => {
                follows_replacement = true;
                replaced_instance_id = Some(instance_id);
            }
            Ok(StreamOutcome::ReceiverClosed) => return,
            Err(error) => {
                let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                return;
            }
        }

        if follows_replacement {
            if events
                .send(ManagedEvent::Recovering(RecoveryStatus {
                    attempt: 1,
                    retry_in: Duration::ZERO,
                }))
                .await
                .is_err()
            {
                return;
            }
            let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
            match establish_replacement_connection(&config, &http, replaced_instance_id, deadline)
                .await
            {
                Ok(replacement) => {
                    connection = replacement;
                    continue;
                }
                Err(error) => {
                    let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                    return;
                }
            }
        }

        let mut attempt = 1;
        let mut retry_in = Duration::ZERO;
        loop {
            if events
                .send(ManagedEvent::Recovering(RecoveryStatus {
                    attempt,
                    retry_in,
                }))
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
    ManualShutdown,
    Replacement { instance_id: uuid::Uuid },
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
        if let ManagedEvent::ServerShutdown(shutdown) = &managed_event
            && shutdown.reason == ShutdownReason::Replacement
        {
            return Ok(StreamOutcome::Replacement {
                instance_id: shutdown.instance_id,
            });
        }
        let manual_shutdown = matches!(managed_event, ManagedEvent::ServerShutdown(_));
        if events.send(managed_event).await.is_err() {
            return Ok(StreamOutcome::ReceiverClosed);
        }
        if manual_shutdown {
            return Ok(StreamOutcome::ManualShutdown);
        }
    }
    Ok(StreamOutcome::Disconnected)
}

enum ReplacementProbe {
    Pending,
    Ready(Box<ActiveConnection>),
    Incompatible(u32),
}

async fn establish_replacement_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    replaced_instance_id: Option<uuid::Uuid>,
    deadline: tokio::time::Instant,
) -> Result<ActiveConnection> {
    loop {
        match probe_replacement_connection(config, http, replaced_instance_id, deadline).await {
            ReplacementProbe::Ready(connection) => return Ok(*connection),
            ReplacementProbe::Incompatible(protocol_version) => {
                bail!(
                    "replacement server protocol version {protocol_version} is incompatible with client protocol version {PROTOCOL_VERSION}"
                )
            }
            ReplacementProbe::Pending => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!("replacement Chidori server did not become ready within 15s")
        }
        tokio::time::sleep_until((now + Duration::from_millis(50)).min(deadline)).await;
    }
}

async fn probe_replacement_connection(
    config: &ManagedClientConfig,
    http: &reqwest::Client,
    replaced_instance_id: Option<uuid::Uuid>,
    deadline: tokio::time::Instant,
) -> ReplacementProbe {
    let Ok(descriptor) = read_descriptor(&config.descriptor_path()) else {
        return ReplacementProbe::Pending;
    };
    if replaced_instance_id == Some(descriptor.instance_id)
        || validate_loopback_url(&descriptor.base_url).is_err()
    {
        return ReplacementProbe::Pending;
    }
    let Ok(Ok(health)) = tokio::time::timeout_at(deadline, inspect_health(&descriptor)).await
    else {
        return ReplacementProbe::Pending;
    };
    if replaced_instance_id == Some(health.instance_id) || health.lifecycle != LifecycleState::Ready
    {
        return ReplacementProbe::Pending;
    }
    if health.protocol_version != PROTOCOL_VERSION {
        return ReplacementProbe::Incompatible(health.protocol_version);
    }
    let Ok(Ok(response)) = tokio::time::timeout_at(
        deadline,
        http.get(format!("{}/v1/events", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send(),
    )
    .await
    else {
        return ReplacementProbe::Pending;
    };
    let Ok(response) = response.error_for_status() else {
        return ReplacementProbe::Pending;
    };
    ReplacementProbe::Ready(Box::new(ActiveConnection {
        descriptor,
        health,
        response,
    }))
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
        SERVER_SHUTDOWN_EVENT => {
            let shutdown: ServerShutdown =
                serde_json::from_str(&event.data).context("decode server shutdown intent")?;
            if shutdown.instance_id != expected_instance_id {
                bail!("shutdown intent came from an unexpected server instance");
            }
            Ok(ManagedEvent::ServerShutdown(shutdown))
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
