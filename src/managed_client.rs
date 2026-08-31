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
        AdmitPromptRequest, AgentSelection, CreateSessionRequest, Health, InvitePreview,
        IssueInviteRequest, IssuedInvite, LifecycleState, ModelCatalog, Outlook, Peer,
        PreviewInviteRequest, Prompt, PromptId, RedeemInviteRequest, Remote, RemoteHealth,
        ResolveWorkspaceRequest, RuntimeDescriptor, ServerShutdown, SessionCatalogSnapshot,
        SessionCreated, SessionDeleted, SessionError, SessionId, SessionListItem,
        SessionSettlementChanged, SessionSnapshot, SessionSummary, SessionTitleChanged,
        SessionUsageChanged, SessionWorkingChanged, SettingMutation, SettingsSnapshot,
        SettleSessionRequest, ShutdownReason, SkillCatalog, SkillCatalogRequest,
        UpdateAgentSelectionRequest,
    },
};

mod event_stream;
mod launcher;
mod lifecycle;
mod recovery;
mod session_catalog_stream;
mod session_projection;
mod session_stream;

pub use session_catalog_stream::SessionCatalogSubscription;
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
    /// A Session joined the catalog. It carries an id and no more, so a
    /// surface listing Sessions answers it by asking for the listing the new
    /// row is drawn from.
    SessionCreated(SessionCreated),
    SessionDeleted(SessionDeleted),
    /// A Session's derived Title and Emoji landed. It arrives for every Session
    /// the server holds, open or not, because the picker lists Sessions this
    /// client has never opened.
    SessionTitleChanged(SessionTitleChanged),
    /// A Session was set aside as done for now, or brought back. It arrives on
    /// the same terms as a Title change, and for the same reason: every client
    /// lists the Session, and only some have it open.
    SessionSettlementChanged(SessionSettlementChanged),
    /// A Session's latest Turn began or settled, moving what a listing says
    /// live work has been running for. It arrives on the same terms as a Title
    /// change, and for the same reason: every client lists the Session, and
    /// only some have it open.
    SessionWorkingChanged(SessionWorkingChanged),
    /// A Session's total Usage moved, its own Turns and its Subagent subtree
    /// counted together. It arrives on the same terms as a Working change, so
    /// a client listing Sessions it has never opened can state what each of
    /// them has consumed.
    SessionUsageChanged(SessionUsageChanged),
    SessionCatalogReconciled(SessionCatalogSnapshot),
    Fatal(String),
}

impl ManagedEvent {
    /// Whether this event says anything a client draws the moment it lands. A
    /// Session another client made says an id and no more — the row it becomes
    /// comes with the listing a surface asks for in answer — so a client that
    /// draws on demand pays for the answer rather than for the announcement
    /// (ADR 0007). Everything else moves something on screen as it arrives.
    pub const fn is_drawn_on_arrival(&self) -> bool {
        // A total moving draws nothing yet either: no listing surface states
        // one, and the client with the Session open reads its total off the
        // Session's own stream.
        !matches!(self, Self::SessionCreated(_) | Self::SessionUsageChanged(_))
    }

    /// Whether this event reports the body of work moving: a Session made,
    /// retitled, deleted, set aside, brought back, worked on, or a whole
    /// catalog reconciled after a reconnection. A surface listing Sessions is
    /// only as truthful as the last such change it was told about, so it asks
    /// the server again for everything the change itself does not say — for a
    /// Turn starting, the last activity the commit moved and the order a
    /// listing keeps by it.
    ///
    /// A total moving is deliberately not one of them: unlike the others it
    /// carries the whole of what it moved and moves nothing a listing is
    /// ordered or drawn by, so there is nothing left to ask the server for.
    pub const fn moves_the_session_catalog(&self) -> bool {
        matches!(
            self,
            Self::SessionCreated(_)
                | Self::SessionDeleted(_)
                | Self::SessionTitleChanged(_)
                | Self::SessionSettlementChanged(_)
                | Self::SessionWorkingChanged(_)
                | Self::SessionCatalogReconciled(_)
        )
    }
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
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct SessionCommandClient {
    http: reqwest::Client,
    descriptor: watch::Receiver<RuntimeDescriptor>,
    outlook: Outlook,
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
}

/// Commands addressed to the one Server a Client's Outlook names. Remote
/// requests are encoded beneath the local Server's explicit proxy route; the
/// caller never needs to construct or understand that route.
#[derive(Clone)]
pub struct OutlookClient {
    commands: SessionCommandClient,
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

    pub fn outlook(&self, outlook: Outlook) -> OutlookClient {
        OutlookClient {
            commands: self.session_commands_for(outlook),
        }
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

    pub async fn issue_invite(&self, request: IssueInviteRequest) -> Result<IssuedInvite> {
        self.session_commands().issue_invite(request).await
    }

    pub async fn preview_invite(&self, invite: impl Into<String>) -> Result<InvitePreview> {
        self.session_commands().preview_invite(invite.into()).await
    }

    pub async fn redeem_invite(&self, request: RedeemInviteRequest) -> Result<Remote> {
        self.session_commands().redeem_invite(request).await
    }

    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        self.session_commands().list_peers().await
    }

    pub async fn list_remotes(&self) -> Result<Vec<Remote>> {
        self.session_commands().list_remotes().await
    }

    pub async fn probe_remote(&self, name: &str) -> Result<RemoteHealth> {
        self.session_commands().probe_remote(name).await
    }

    pub async fn remove_peer(&self, peer_id: &str) -> Result<()> {
        self.session_commands().remove_peer(peer_id).await
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

    /// Stops what the Session is doing: its active Turn along with the
    /// Subagents it spawned, or — with no Turn active — its working Subagents
    /// alone. Interrupting a Subagent's own Session stops that one Subagent.
    pub async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.session_commands().interrupt_session(session_id).await
    }

    pub async fn read_session(&self, session_id: SessionId) -> Result<SessionSnapshot> {
        self.session_commands().read_session(session_id).await
    }

    pub async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        self.session_commands().delete_session(session_id).await
    }

    pub async fn settle_session(
        &self,
        session_id: SessionId,
        settled: bool,
    ) -> Result<SessionSummary> {
        self.session_commands()
            .settle_session(session_id, settled)
            .await
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
        self.session_commands_for(Outlook::Local)
    }

    pub(crate) fn session_commands_for(&self, outlook: Outlook) -> SessionCommandClient {
        SessionCommandClient {
            http: self.http.clone(),
            descriptor: self.descriptor.clone(),
            outlook,
            initial_recovery_backoff: self.initial_recovery_backoff,
            max_recovery_backoff: self.max_recovery_backoff,
        }
    }
}

impl OutlookClient {
    pub fn subscribe_catalog(&self) -> SessionCatalogSubscription {
        self.commands.subscribe_catalog()
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionSnapshot> {
        self.commands.create_session(request).await
    }

    pub async fn resolve_workspace(
        &self,
        request: ResolveWorkspaceRequest,
    ) -> Result<crate::protocol::Workspace> {
        self.commands.resolve_workspace(request).await
    }

    pub async fn admit_prompt(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<Prompt> {
        self.commands.admit_prompt(session_id, request).await
    }

    pub async fn update_agent_selection(
        &self,
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    ) -> Result<AgentSelection> {
        self.commands
            .update_agent_selection(session_id, request)
            .await
    }

    pub async fn confirm_landing_agent_selection(
        &self,
        selection: AgentSelection,
    ) -> Result<AgentSelection> {
        self.commands
            .confirm_landing_agent_selection(selection)
            .await
    }

    pub async fn subscribe_session(&self, session_id: SessionId) -> Result<SessionSubscription> {
        self.commands.subscribe_session(session_id).await
    }

    pub async fn promote_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.commands.promote_prompt(session_id, prompt_id).await
    }

    pub async fn cancel_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt> {
        self.commands.cancel_prompt(session_id, prompt_id).await
    }

    pub async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.commands.interrupt_session(session_id).await
    }

    pub async fn read_session(&self, session_id: SessionId) -> Result<SessionSnapshot> {
        self.commands.read_session(session_id).await
    }

    pub async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        self.commands.delete_session(session_id).await
    }

    pub async fn settle_session(
        &self,
        session_id: SessionId,
        settled: bool,
    ) -> Result<SessionSummary> {
        self.commands.settle_session(session_id, settled).await
    }

    pub async fn list_sessions(&self, workspace: Option<&Path>) -> Result<Vec<SessionListItem>> {
        self.commands.list_sessions(workspace).await
    }

    pub async fn list_models(&self) -> Result<ModelCatalog> {
        self.commands.list_models().await
    }

    pub async fn refresh_models(&self) -> Result<ModelCatalog> {
        self.commands.refresh_models().await
    }

    pub async fn list_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        self.commands.list_skills(request).await
    }

    pub async fn refresh_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        self.commands.refresh_skills(request).await
    }

    pub async fn attach_session(&self, session_id: SessionId) -> Result<SessionSubscription> {
        self.commands.attach_session(session_id).await
    }
}

impl SessionCommandClient {
    pub(crate) fn subscribe_catalog(&self) -> SessionCatalogSubscription {
        SessionCatalogSubscription::open_attached(
            self.http.clone(),
            self.descriptor.clone(),
            self.outlook.clone(),
            self.initial_recovery_backoff,
            self.max_recovery_backoff,
        )
    }

    pub(crate) async fn resolve_workspace(
        &self,
        request: ResolveWorkspaceRequest,
    ) -> Result<crate::protocol::Workspace> {
        self.post_session_command("/v1/workspaces/resolve", &request, "Workspace resolution")
            .await
    }

    pub(crate) async fn list_skills(&self, request: SkillCatalogRequest) -> Result<SkillCatalog> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/skills",
            )?)
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
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/skills/refresh",
            )?)
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
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/models",
            )?)
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
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/models/refresh",
            )?)
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
            .put(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/landing-agent-selection",
            )?)
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

    pub(crate) async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.post_session_command_without_response(
            &format!("/v1/sessions/{session_id}/interrupt"),
            "Session interruption",
        )
        .await
    }

    /// Posts a body-less command whose success answers with no body either.
    async fn post_session_command_without_response(
        &self,
        path: &str,
        operation: &str,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(&descriptor.base_url, &self.outlook, path)?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .with_context(|| format!("send {operation} command"))?;
        decode_empty_api_response(response, operation).await
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
            .post(server_url(&descriptor.base_url, &self.outlook, path)?)
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
            .post(server_url(&descriptor.base_url, &self.outlook, path)?)
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

    pub(crate) async fn issue_invite(&self, request: IssueInviteRequest) -> Result<IssuedInvite> {
        self.post_session_command("/v1/pairing/invites", &request, "Invite issuance")
            .await
    }

    pub(crate) async fn preview_invite(&self, invite: String) -> Result<InvitePreview> {
        self.post_session_command(
            "/v1/pairing/invites/preview",
            &PreviewInviteRequest { invite },
            "Invite preview",
        )
        .await
    }

    pub(crate) async fn redeem_invite(&self, request: RedeemInviteRequest) -> Result<Remote> {
        self.post_session_command("/v1/pairing/remotes", &request, "Invite redemption")
            .await
    }

    pub(crate) async fn list_peers(&self) -> Result<Vec<Peer>> {
        self.get_pairing_resource("/v1/pairing/peers", "Peer listing")
            .await
    }

    pub(crate) async fn list_remotes(&self) -> Result<Vec<Remote>> {
        self.get_pairing_resource("/v1/pairing/remotes", "Remote listing")
            .await
    }

    pub(crate) async fn probe_remote(&self, name: &str) -> Result<RemoteHealth> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(remote_probe_url(&descriptor.base_url, name)?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Remote probe command")?;
        decode_api_response(response, "Remote probe").await
    }

    pub(crate) async fn remove_peer(&self, peer_id: &str) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .delete(format!(
                "{}/v1/pairing/peers/{peer_id}",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Peer removal")?;
        decode_empty_api_response(response, "Peer removal").await
    }

    async fn get_pairing_resource<ResponseBody>(
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
            .get(format!("{}{path}", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .with_context(|| format!("send {operation}"))?;
        decode_api_response(response, operation).await
    }

    pub(crate) async fn subscribe_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSubscription> {
        let descriptor = self.descriptor.borrow().clone();
        SessionSubscription::open(&self.http, &descriptor, &self.outlook, session_id).await
    }

    pub(crate) async fn read_session(&self, session_id: SessionId) -> Result<SessionSnapshot> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}"),
            )?)
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
            .delete(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}"),
            )?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Session deletion")?;
        decode_empty_api_response(response, "Session deletion").await
    }

    /// Sets a Session aside as done for now, or brings it back. Which of the
    /// two is stated rather than toggled, so a client acting on a listing that
    /// has moved on cannot flip a Session it meant to leave alone.
    pub(crate) async fn settle_session(
        &self,
        session_id: SessionId,
        settled: bool,
    ) -> Result<SessionSummary> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/settlement"),
            &SettleSessionRequest { settled },
            "Session settlement",
        )
        .await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<SessionListItem>> {
        let descriptor = self.descriptor.borrow().clone();
        let request = self
            .http
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/sessions",
            )?)
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
        SessionSubscription::open_attached(
            &self.http,
            self.descriptor.clone(),
            self.outlook.clone(),
            session_id,
        )
        .await
    }
}

pub(super) fn server_url(base_url: &str, outlook: &Outlook, path: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("parse server base URL")?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| anyhow!("server base URL cannot contain path segments"))?;
    segments.clear();
    if let Outlook::Remote(name) = outlook {
        segments.extend(["v1", "remotes", name]);
    }
    segments.extend(path.trim_start_matches('/').split('/'));
    drop(segments);
    Ok(url)
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
        |error| anyhow!(error),
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

fn remote_probe_url(base_url: &str, name: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("parse server base URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("server base URL cannot contain path segments"))?
        .extend(["v1", "pairing", "remotes", name, "health"]);
    Ok(url)
}

impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::remote_probe_url;

    #[test]
    fn remote_probe_url_encodes_a_freely_editable_name_as_one_path_segment() {
        let url = remote_probe_url("http://127.0.0.1:7777", "lab/east?#")
            .expect("build Remote probe URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/pairing/remotes/lab%2Feast%3F%23/health"
        );
    }
}
