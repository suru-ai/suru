//! Client-side ownership of server discovery and event streaming.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    RuntimeConfig,
    protocol::{
        CounterSnapshot, CounterUpdate, CreateSessionRequest, Health, LifecycleState,
        RuntimeDescriptor, ServerShutdown, SessionError, SessionId, SessionSnapshot,
        ShutdownReason,
    },
};

mod event_stream;
mod launcher;
mod lifecycle;
mod recovery;
mod session_stream;

pub use session_stream::{SessionEvent, SessionSubscription};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

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
    http: reqwest::Client,
    descriptor: watch::Receiver<RuntimeDescriptor>,
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
        recovery::connect(config).await
    }

    pub async fn next(&mut self) -> Option<ManagedEvent> {
        self.events.recv().await
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionSnapshot> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}/v1/sessions", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send()
            .await
            .context("send Session creation command")?;
        if response.status().is_success() {
            return response
                .json::<SessionSnapshot>()
                .await
                .context("decode created Session snapshot");
        }
        let status = response.status();
        let error = response.json::<SessionError>().await.ok();
        match error {
            Some(error) => bail!(error.message),
            None => bail!("Session creation failed with HTTP {status}"),
        }
    }

    pub async fn subscribe_session(&self, session_id: SessionId) -> Result<SessionSubscription> {
        let descriptor = self.descriptor.borrow().clone();
        SessionSubscription::open(&self.http, &descriptor, session_id).await
    }
}

pub async fn start_server(config: &ManagedClientConfig) -> Result<Health> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    launcher::ensure_server(config, deadline)
        .await
        .map(|registration| registration.health)
}

pub async fn server_status(config: &ManagedClientConfig) -> Result<ServerStatus> {
    let health = match lifecycle::inspect_registration(config).await? {
        lifecycle::RegistrationInspection::Missing => return Ok(ServerStatus::Missing),
        lifecycle::RegistrationInspection::Live(registration) => registration.health,
        lifecycle::RegistrationInspection::Stale(reason) => {
            return Ok(ServerStatus::Stale(reason));
        }
        lifecycle::RegistrationInspection::Unreachable(reason) => {
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
    let registration = match lifecycle::inspect_registration(config).await? {
        lifecycle::RegistrationInspection::Missing => {
            bail!("cannot stop missing Chidori server")
        }
        lifecycle::RegistrationInspection::Live(registration) => registration,
        lifecycle::RegistrationInspection::Stale(reason) => {
            bail!("cannot stop stale Chidori server registration: {reason}")
        }
        lifecycle::RegistrationInspection::Unreachable(reason) => {
            bail!("cannot stop unreachable Chidori server: {reason}")
        }
    };
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    lifecycle::shutdown_registered_instance(
        config,
        &registration,
        ShutdownReason::Manual,
        deadline,
    )
    .await?;
    Ok(registration.health)
}

impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}
