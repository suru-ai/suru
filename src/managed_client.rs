//! Client-side ownership of server discovery and event streaming.

use std::{
    fs::{File, OpenOptions},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use eventsource_stream::{Event, Eventsource};
use futures_util::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    RuntimeConfig,
    protocol::{
        COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, Health, LifecycleState,
        RuntimeDescriptor, SNAPSHOT_EVENT,
    },
};

pub type ManagedClientConfig = RuntimeConfig;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedEvent {
    Connected(Health),
    Snapshot(CounterSnapshot),
    CounterUpdated(CounterUpdate),
    Fatal(String),
}

pub struct ManagedClient {
    events: mpsc::Receiver<ManagedEvent>,
    task: JoinHandle<()>,
}

impl ManagedClient {
    pub async fn connect(config: ManagedClientConfig) -> Result<Self> {
        let (descriptor, health) = probe(&config).await?;

        let http = reqwest::Client::new();
        let response = http
            .get(format!("{}/v1/events", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("open server event stream")?
            .error_for_status()
            .context("server rejected event stream")?;
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(stream_events(
            response,
            events_tx,
            health,
            descriptor.instance_id,
        ));

        Ok(Self {
            events: events_rx,
            task,
        })
    }

    pub async fn next(&mut self) -> Option<ManagedEvent> {
        self.events.recv().await
    }
}

pub async fn start_server(config: &ManagedClientConfig) -> Result<Health> {
    if let Ok((_, health)) = probe(config).await {
        return Ok(health);
    }

    spawn_detached(config)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let error = match probe(config).await {
            Ok((_, health)) => return Ok(health),
            Err(error) => error,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(error).context("detached Chidori server did not become ready within 15s");
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

fn spawn_detached(config: &ManagedClientConfig) -> Result<()> {
    let runtime_dir = config.create_private_runtime_dir()?;

    let log_path = runtime_dir.join("server.log");
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
    let stderr = stdout.try_clone().context("clone server log handle")?;

    let executable = std::env::current_exe().context("find current Chidori executable")?;
    let mut command = Command::new(executable);
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
    command.spawn().context("spawn detached Chidori server")?;
    Ok(())
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

async fn stream_events(
    response: reqwest::Response,
    events: mpsc::Sender<ManagedEvent>,
    health: Health,
    expected_instance_id: uuid::Uuid,
) {
    if events.send(ManagedEvent::Connected(health)).await.is_err() {
        return;
    }

    let mut stream = response.bytes_stream().eventsource();
    let mut last_revision = None;
    let mut saw_snapshot = false;
    while let Some(next) = stream.next().await {
        let event = match next {
            Ok(event) => event,
            Err(error) => {
                let _ = events
                    .send(ManagedEvent::Fatal(format!(
                        "server event stream failed: {error}"
                    )))
                    .await;
                return;
            }
        };
        let managed_event = match decode_event(
            event,
            expected_instance_id,
            &mut saw_snapshot,
            &mut last_revision,
        ) {
            Ok(event) => event,
            Err(error) => {
                let _ = events.send(ManagedEvent::Fatal(error.to_string())).await;
                return;
            }
        };
        if events.send(managed_event).await.is_err() {
            return;
        }
    }
    let _ = events
        .send(ManagedEvent::Fatal(
            "server event stream closed unexpectedly".to_owned(),
        ))
        .await;
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
    if health.lifecycle != LifecycleState::Ready {
        bail!("registered server is not ready");
    }
    if health.instance_id != descriptor.instance_id
        || health.pid != descriptor.pid
        || health.protocol_version != descriptor.protocol_version
        || health.build_identity != descriptor.build_identity
    {
        bail!("registered server identity does not match its runtime descriptor");
    }
    Ok(())
}
