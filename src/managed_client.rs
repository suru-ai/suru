//! Client-side ownership of server discovery and event streaming.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::{
    RuntimeConfig,
    protocol::{
        AdmitPromptRequest, AgentSelection, CreateSessionRequest, Health, LifecycleState,
        ModelCatalog, Prompt, PromptId, RuntimeDescriptor, ServerShutdown, SessionCatalogSnapshot,
        SessionDeleted, SessionError, SessionId, SessionListItem, SessionSnapshot,
        SessionTitleChanged, SettingMutation, SettingsSnapshot, ShutdownReason, SkillCatalog,
        SkillCatalogRequest, Turn, TurnId, UpdateAgentSelectionRequest,
    },
};

mod event_stream;
mod launcher;
mod lifecycle;
mod recovery;
mod session_catalog_stream;
mod session_projection;
mod session_stream;

pub(crate) use session_projection::SessionProjection;
pub use session_stream::{SessionEvent, SessionStreamError, SessionSubscription};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_RECOVERY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct ManagedClientConfig {
    runtime: RuntimeConfig,
    server_executable: PathBuf,
    startup_timeout: Duration,
    stop_timeout: Duration,
    health_check_timeout: Duration,
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
}

impl ManagedClientConfig {
    pub fn new(state_base_dir: impl AsRef<Path>, channel: impl Into<String>) -> Result<Self> {
        Ok(Self {
            runtime: RuntimeConfig::new(state_base_dir, channel)?,
            server_executable: std::env::current_exe().context("find current Suru executable")?,
            startup_timeout: STARTUP_TIMEOUT,
            stop_timeout: STOP_TIMEOUT,
            health_check_timeout: HEALTH_CHECK_TIMEOUT,
            initial_recovery_backoff: INITIAL_RECOVERY_BACKOFF,
            max_recovery_backoff: MAX_RECOVERY_BACKOFF,
        })
    }

    pub fn with_server_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.server_executable = executable.into();
        self
    }

    /// Bounds how long connecting waits for a server to become ready; injectable
    /// so tests can exercise the deadline without waiting out the default.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Bounds how long a manual stop waits for the target process and runtime
    /// registration to settle.
    pub fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = timeout;
        self
    }

    /// Bounds individual authenticated health probes used by status and
    /// shutdown settlement.
    pub fn with_health_check_timeout(mut self, timeout: Duration) -> Self {
        self.health_check_timeout = timeout;
        self
    }

    /// Overrides the crash-recovery retry schedule (initial wait, doubling up to
    /// the cap); injectable so tests can observe the schedule at millisecond scale.
    pub fn with_recovery_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_recovery_backoff = initial;
        self.max_recovery_backoff = max;
        self
    }

    pub fn with_data_dir(mut self, data_base_dir: impl AsRef<Path>) -> Self {
        self.runtime = self.runtime.with_data_dir(data_base_dir);
        self
    }

    /// Points a server this client launches at the Config Document root,
    /// mirroring how `SURU_CONFIG_DIR` points a real run at one.
    pub fn with_config_dir(mut self, config_dir: impl AsRef<Path>) -> Self {
        self.runtime = self.runtime.with_config_dir(config_dir);
        self
    }

    pub fn runtime(&self) -> &RuntimeConfig {
        &self.runtime
    }

    pub fn state_dir(&self) -> &Path {
        self.runtime.state_dir()
    }

    pub fn data_dir(&self) -> &Path {
        self.runtime.data_dir()
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
    /// The server's effective-settings view: pushed right after every connect
    /// and again whenever the server replaces it.
    SettingsSnapshot(SettingsSnapshot),
    /// A server-authoritative Skill Catalog changed after discovery or
    /// invalidation. Every attached client receives the same state transition.
    SkillCatalogUpdated(SkillCatalog),
    Recovering(RecoveryStatus),
    ServerShutdown(ServerShutdown),
    SessionDeleted(SessionDeleted),
    /// A Session's derived Title and Emoji landed. It arrives for every Session
    /// the server holds, open or not, because the picker lists Sessions this
    /// client has never opened.
    SessionTitleChanged(SessionTitleChanged),
    SessionCatalogReconciled(SessionCatalogSnapshot),
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
            Self::Missing => write!(formatter, "Suru server missing (no runtime registration)"),
            Self::Starting(health) => write!(
                formatter,
                "Suru server starting (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Ready(health) => write!(
                formatter,
                "Suru server ready (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Stopping(health) => write!(
                formatter,
                "Suru server stopping (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Failed(health) => write!(
                formatter,
                "Suru server failed (pid {}, instance {})",
                health.pid, health.instance_id
            ),
            Self::Stale(reason) => {
                write!(formatter, "Suru server registration stale: {reason}")
            }
            Self::Unreachable(reason) => write!(formatter, "Suru server unreachable: {reason}"),
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

    pub async fn update_agent_selection(
        &self,
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    ) -> Result<AgentSelection> {
        self.session_commands()
            .update_agent_selection(session_id, request)
            .await
    }

    pub async fn confirm_landing_agent_selection(
        &self,
        selection: AgentSelection,
    ) -> Result<AgentSelection> {
        self.session_commands()
            .confirm_landing_agent_selection(selection)
            .await
    }

    /// Changes one Setting in the server's Config Document. The answer is the
    /// effective-settings snapshot the edit leaves in force, which every
    /// attached client — this one included — also receives on its event stream.
    pub async fn mutate_setting(&self, mutation: SettingMutation) -> Result<SettingsSnapshot> {
        self.session_commands().mutate_setting(mutation).await
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

    pub async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        self.session_commands().delete_session(session_id).await
    }

    pub async fn list_sessions(&self, workspace: Option<&Path>) -> Result<Vec<SessionListItem>> {
        self.session_commands().list_sessions(workspace).await
    }

    pub async fn list_models(&self) -> Result<ModelCatalog> {
        self.session_commands().list_models().await
    }

    pub async fn refresh_models(&self) -> Result<ModelCatalog> {
        self.session_commands().refresh_models().await
    }

    pub async fn list_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        self.session_commands().list_skills(request).await
    }

    pub async fn refresh_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        self.session_commands().refresh_skills(request).await
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
    pub(crate) async fn list_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}/v1/skills", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send()
            .await
            .context("send Skill Catalog listing")?;
        decode_api_response(response, "Skill Catalog listing").await
    }

    pub(crate) async fn refresh_skills(
        &self,
        request: SkillCatalogRequest,
    ) -> Result<SkillCatalog> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(format!("{}/v1/skills/refresh", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send()
            .await
            .context("send Skill Catalog refresh")?;
        decode_api_response(response, "Skill Catalog refresh").await
    }

    pub(crate) async fn list_models(&self) -> Result<ModelCatalog> {
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

    pub(crate) async fn refresh_models(&self) -> Result<ModelCatalog> {
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

    pub(crate) async fn update_agent_selection(
        &self,
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    ) -> Result<AgentSelection> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/agent-selection"),
            &request,
            "Agent Selection update",
        )
        .await
    }

    pub(crate) async fn confirm_landing_agent_selection(
        &self,
        selection: AgentSelection,
    ) -> Result<AgentSelection> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .put(format!(
                "{}/v1/landing-agent-selection",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .json(&selection)
            .send()
            .await
            .context("send landing Agent Selection update command")?;
        decode_api_response(response, "Landing Agent Selection update").await
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

    pub(crate) async fn mutate_setting(
        &self,
        mutation: SettingMutation,
    ) -> Result<SettingsSnapshot> {
        self.post_session_command("/v1/settings", &mutation, "Setting mutation")
            .await
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

    pub(crate) async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Session deletion")?;
        decode_empty_api_response(response, "Session deletion").await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<SessionListItem>> {
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
    Err(decode_api_error(response, operation).await)
}

async fn decode_empty_api_response(response: reqwest::Response, operation: &str) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    Err(decode_api_error(response, operation).await)
}

async fn decode_api_error(response: reqwest::Response, operation: &str) -> anyhow::Error {
    let status = response.status();
    response.json::<SessionError>().await.map_or_else(
        |_| anyhow!("{operation} failed with HTTP {status}"),
        |error| anyhow!(error.message),
    )
}

pub async fn start_server(config: &ManagedClientConfig) -> Result<Health> {
    let deadline = tokio::time::Instant::now() + config.startup_timeout;
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
            bail!("cannot stop missing Suru server")
        }
        lifecycle::RegistrationInspection::Live(registration) => registration,
        lifecycle::RegistrationInspection::Stale(reason) => {
            bail!("cannot stop stale Suru server registration: {reason}")
        }
        lifecycle::RegistrationInspection::Unreachable(reason) => {
            bail!("cannot stop unreachable Suru server: {reason}")
        }
    };
    let deadline = tokio::time::Instant::now() + config.stop_timeout;
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
