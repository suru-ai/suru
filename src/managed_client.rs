//! Client-side ownership of server discovery and event streaming.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    RuntimeConfig,
    protocol::{
        AdmitPromptRequest, CreateSessionRequest, Health, LifecycleState, ModelCatalog, Prompt,
        PromptId, RuntimeDescriptor, ServerShutdown, SessionError, SessionId, SessionSnapshot,
        SessionSummary, ShutdownReason, Turn, TurnId,
    },
};

mod event_stream;
mod launcher;
mod lifecycle;
mod recovery;
mod session_projection;
mod session_stream;

pub(crate) use session_projection::SessionProjection;
pub use session_stream::{SessionEvent, SessionStreamError, SessionSubscription};

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

#[derive(Clone)]
pub(crate) struct SessionCommandClient {
    http: reqwest::Client,
    descriptor: watch::Receiver<RuntimeDescriptor>,
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
        self.session_commands().create_session(request).await
    }

    pub async fn admit_prompt(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<Prompt> {
        self.session_commands()
            .admit_prompt(session_id, request)
            .await
    }

    pub async fn subscribe_session(&self, session_id: SessionId) -> Result<SessionSubscription> {
        self.session_commands().subscribe_session(session_id).await
    }

    pub async fn promote_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.session_commands()
            .promote_prompt(session_id, prompt_id)
            .await
    }

    pub async fn cancel_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.session_commands()
            .cancel_prompt(session_id, prompt_id)
            .await
    }

    pub async fn interrupt_turn(&self, session_id: SessionId, turn_id: TurnId) -> Result<Turn> {
        self.session_commands()
            .interrupt_turn(session_id, turn_id)
            .await
    }

    pub async fn read_session(&self, session_id: SessionId) -> Result<SessionSnapshot> {
        self.session_commands().read_session(session_id).await
    }

    pub async fn list_sessions(&self, workspace: Option<&Path>) -> Result<Vec<SessionSummary>> {
        self.session_commands().list_sessions(workspace).await
    }

    pub async fn list_models(&self) -> Result<ModelCatalog> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(format!("{}/v1/models", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Model listing")?;
        decode_api_response(response, "Model listing").await
    }

    pub async fn refresh_models(&self) -> Result<ModelCatalog> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}/v1/models/refresh", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Model catalog refresh")?;
        decode_api_response(response, "Model catalog refresh").await
    }

    pub async fn attach_session(&self, session_id: SessionId) -> Result<SessionSubscription> {
        self.session_commands().attach_session(session_id).await
    }

    pub(crate) fn session_commands(&self) -> SessionCommandClient {
        SessionCommandClient {
            http: self.http.clone(),
            descriptor: self.descriptor.clone(),
        }
    }
}

impl SessionCommandClient {
    pub(crate) async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionSnapshot> {
        self.post_session_command("/v1/sessions", &request, "Session creation")
            .await
    }

    pub(crate) async fn admit_prompt(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<Prompt> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/prompts"),
            &request,
            "Prompt admission",
        )
        .await
    }

    pub(crate) async fn promote_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.post_session_command_without_body(
            &format!("/v1/sessions/{session_id}/prompts/{prompt_id}/promote"),
            "Prompt promotion",
        )
        .await
    }

    pub(crate) async fn cancel_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.post_session_command_without_body(
            &format!("/v1/sessions/{session_id}/prompts/{prompt_id}/cancel"),
            "Prompt cancellation",
        )
        .await
    }

    pub(crate) async fn interrupt_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Turn> {
        self.post_session_command_without_body(
            &format!("/v1/sessions/{session_id}/turns/{turn_id}/interrupt"),
            "Turn interruption",
        )
        .await
    }

    async fn post_session_command<RequestBody, ResponseBody>(
        &self,
        path: &str,
        request: &RequestBody,
        operation: &str,
    ) -> Result<ResponseBody>
    where
        RequestBody: Serialize + ?Sized,
        ResponseBody: DeserializeOwned,
    {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}{path}", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(request)
            .send()
            .await
            .with_context(|| format!("send {operation} command"))?;
        decode_api_response(response, operation).await
    }

    async fn post_session_command_without_body<ResponseBody>(
        &self,
        path: &str,
        operation: &str,
    ) -> Result<ResponseBody>
    where
        ResponseBody: DeserializeOwned,
    {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}{path}", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .with_context(|| format!("send {operation} command"))?;
        decode_api_response(response, operation).await
    }

    pub(crate) async fn subscribe_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSubscription> {
        let descriptor = self.descriptor.borrow().clone();
        SessionSubscription::open(&self.http, &descriptor, session_id).await
    }

    pub(crate) async fn read_session(&self, session_id: SessionId) -> Result<SessionSnapshot> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Session read")?;
        decode_api_response(response, "Session read").await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<SessionSummary>> {
        let descriptor = self.descriptor.borrow().clone();
        let request = self
            .http
            .get(format!("{}/v1/sessions", descriptor.base_url))
            .bearer_auth(&descriptor.token);
        let request = match workspace {
            Some(workspace) => request.query(&[("workspace", workspace)]),
            None => request,
        };
        let response = request.send().await.context("send Session listing")?;
        decode_api_response(response, "Session listing").await
    }

    pub(crate) async fn attach_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSubscription> {
        SessionSubscription::open_attached(&self.http, self.descriptor.clone(), session_id).await
    }
}

async fn decode_api_response<T>(response: reqwest::Response, operation: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    if response.status().is_success() {
        return response
            .json::<T>()
            .await
            .with_context(|| format!("decode {operation} response"));
    }
    let status = response.status();
    let error = response.json::<SessionError>().await.ok();
    match error {
        Some(error) => bail!(error.message),
        None => bail!("{operation} failed with HTTP {status}"),
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
