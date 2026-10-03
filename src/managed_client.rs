//! Client-side ownership of server discovery and event streaming.

use std::{
    ffi::OsString,
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
        AddRelayRequest, AdmitPromptRequest, AgentSelection, AttachmentDescriptor, AttachmentId,
        CheckoutStateChanged, CreateSessionRequest, Health, InterruptOutcome, InvitePreview,
        IssueInviteRequest, IssuedInvite, LifecycleState, ModelCatalog, Outlook, Peer,
        PreviewInviteRequest, Prompt, PromptId, RedeemInviteRequest, Relay, RelayLogin,
        RelayRemoval, Remote, RemoteHealth, RemoteRemoval, ResolveWorkspaceRequest,
        RuntimeDescriptor, SESSION_ERROR_CODE_HEADER, ServerShutdown, SessionApprovalPosture,
        SessionCatalogSnapshot, SessionCreated, SessionDeleted, SessionError, SessionErrorCode,
        SessionId, SessionListItem, SessionMonitoringChanged, SessionRemoteSubsessionsChanged,
        SessionSettlementChanged, SessionSnapshot, SessionStandingInputsChanged, SessionSummary,
        SessionTitleChanged, SessionUsageChanged, SessionWorkingChanged, SetSessionIconRequest,
        SetWorkspaceDescriptionRequest, SetWorkspaceIconRequest, SettingMutation, SettingsSnapshot,
        SettleSessionRequest, ShutdownReason, SkillCatalog, SkillCatalogRequest,
        UpdateAgentSelectionRequest, UpdateApprovalPostureRequest, ViewSessionRequest,
        WorkspaceDescriptionChanged, WorkspaceIconChanged, WorkspaceId,
    },
};

mod event_stream;
mod launcher;
mod lifecycle;
mod recovery;
mod remote_connection;
mod session_catalog_stream;
mod session_projection;
mod session_stream;
mod subagent_tree_stream;

pub use launcher::StoppedDuringLaunch;
#[doc(hidden)]
pub use launcher::launch_detached;
pub(crate) use recovery::RecoveryBackoff;
pub use session_catalog_stream::SessionCatalogSubscription;
pub(crate) use session_projection::SessionProjection;
pub(crate) use session_stream::SESSION_EVENT_CAPACITY;
pub use session_stream::{SessionEvent, SessionStreamError, SessionSubscription};
pub use subagent_tree_stream::{SubagentTreeEvent, SubagentTreeSubscription};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_RECOVERY_BACKOFF: Duration = Duration::from_millis(50);
const MAX_RECOVERY_BACKOFF: Duration = Duration::from_secs(5);
const ATTACHMENT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct ManagedClientConfig {
    runtime: RuntimeConfig,
    server_executable: PathBuf,
    server_environment: Vec<(OsString, OsString)>,
    startup_timeout: Duration,
    stop_timeout: Duration,
    health_check_timeout: Duration,
    initial_readiness_interval: Duration,
    max_readiness_interval: Duration,
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
    attachment_fetch_timeout: Duration,
    /// How long a server this client launches waits for the channel's
    /// election lock, where a test holds one in its election.
    election_handoff: Option<Duration>,
}

impl ManagedClientConfig {
    pub fn new(state_base_dir: impl AsRef<Path>, channel: impl Into<String>) -> Result<Self> {
        Ok(Self {
            runtime: RuntimeConfig::new(state_base_dir, channel)?,
            server_executable: std::env::current_exe().context("find current Suru executable")?,
            server_environment: Vec::new(),
            startup_timeout: STARTUP_TIMEOUT,
            stop_timeout: STOP_TIMEOUT,
            health_check_timeout: HEALTH_CHECK_TIMEOUT,
            initial_readiness_interval: Duration::from_millis(5),
            max_readiness_interval: Duration::from_millis(50),
            initial_recovery_backoff: INITIAL_RECOVERY_BACKOFF,
            max_recovery_backoff: MAX_RECOVERY_BACKOFF,
            attachment_fetch_timeout: ATTACHMENT_FETCH_TIMEOUT,
            election_handoff: None,
        })
    }

    pub fn with_server_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.server_executable = executable.into();
        self
    }

    /// Sets `key` to `value` in the environment of every server this client
    /// launches, over the environment it would otherwise inherit from this
    /// process. A launched server outlives its client and runs whatever
    /// Providers it finds there, so this is how a test keeps a server it
    /// causes to launch away from the Providers installed on its machine —
    /// something no other part of the configuration reaches.
    pub fn with_server_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.server_environment.push((key.into(), value.into()));
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

    /// Overrides startup polling: double the initial interval up to the cap.
    /// Internal timing injection, independent of connection-loss recovery.
    /// Clamp to at least one millisecond so zero cannot create a busy loop.
    pub fn with_readiness_polling(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_readiness_interval = initial.max(Duration::from_millis(1));
        self.max_readiness_interval = max.max(self.initial_readiness_interval);
        self
    }

    /// Overrides the crash-recovery retry schedule (initial wait, doubling up to
    /// the cap); injectable so tests can observe the schedule at millisecond scale.
    pub fn with_recovery_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_recovery_backoff = initial;
        self.max_recovery_backoff = max;
        self
    }

    /// Bounds each request that moves or checks an Attachment's bytes — its
    /// upload, its fetch, and the check that it is still stored — from
    /// sending it to reading the last of its body, so a Server that takes the
    /// request and then stalls fails it rather than leaving it pending for
    /// good; injectable so tests can meet the deadline in milliseconds.
    pub fn with_attachment_fetch_timeout(mut self, timeout: Duration) -> Self {
        self.attachment_fetch_timeout = timeout;
        self
    }

    /// Bounds how long a server this client launches waits for the
    /// channel's election lock to come free before conceding the channel to
    /// whoever holds it; injectable so a test can hold a launched server in
    /// its election for as long as it needs to, which the default second
    /// does not allow.
    pub fn with_election_handoff(mut self, handoff: Duration) -> Self {
        self.election_handoff = Some(handoff);
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
    /// The server's Model Catalog: pushed right after the Settings snapshot on
    /// every connect, and again whenever a Provider's discovery settles or its
    /// Enablement turns, so a client names Models without asking for them.
    ModelCatalog(ModelCatalog),
    /// A server-authoritative Skill Catalog changed after discovery or
    /// invalidation. Every attached client receives the same state transition.
    SkillCatalogUpdated(SkillCatalog),
    Recovering(RecoveryStatus),
    /// A Remote catalog stream resumed after a transient link failure.
    RemoteRecovered,
    /// A Remote rejected further use of its Pairing. Unlike a transient drop,
    /// this ends the current Outlook and must not be retried automatically.
    RemoteFailed {
        status: crate::protocol::RemoteStatus,
        message: String,
    },
    ServerShutdown(ServerShutdown),
    /// A Session joined the catalog. It carries an id and no more, so a
    /// surface listing Sessions answers it by asking for the listing the new
    /// row is drawn from.
    SessionCreated(SessionCreated),
    SessionCatalogInvalidated {
        session_id: SessionId,
    },
    CheckoutStateChanged(CheckoutStateChanged),
    SessionDeleted(SessionDeleted),
    /// A Session's derived Title and Icon landed. It arrives for every Session
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
    /// A Session began or stopped Monitoring. It arrives on the same terms as
    /// a Working change, and for the same reason; unlike one it moves no
    /// Session's last activity, since a Watch starting or settling is no work
    /// of the Agent's.
    SessionMonitoringChanged(SessionMonitoringChanged),
    /// A Session's latest Turn Settled, replacing the facts from which every
    /// client derives its Sidebar Standing.
    SessionStandingInputsChanged(SessionStandingInputsChanged),
    /// A Session's total Usage moved, its own Turns and its Subagent subtree
    /// counted together. It arrives on the same terms as a Working change, so
    /// a client listing Sessions it has never opened can state what each of
    /// them has consumed.
    SessionUsageChanged(SessionUsageChanged),
    /// A Workspace's Icon landed, or — once issue #360 lands a way to choose
    /// one — changed. It names no Session, unlike every other catalog event
    /// here: a Workspace may hold no Session this client currently lists, and
    /// the Landing draws its current Workspace's Icon whether or not it ever
    /// has.
    WorkspaceIconChanged(WorkspaceIconChanged),
    /// A Workspace's Description was derived, set, or cleared. Like a
    /// Workspace's Icon it names no Session, and like it, it moves the
    /// catalog: it belongs to the Origin whose stream carried it, and only
    /// that Origin's rows may take it.
    WorkspaceDescriptionChanged(WorkspaceDescriptionChanged),
    /// The Sessions a Sidekick's Session began on Remotes changed. It arrives
    /// on the same terms as a Title change, and for the same reason: a
    /// client hiding Subsessions reads it wherever it lists that Session.
    SessionRemoteSubsessionsChanged(SessionRemoteSubsessionsChanged),
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
    /// retitled, deleted, set aside, brought back, worked on, left Monitoring,
    /// or a whole catalog reconciled after a reconnection. A surface listing Sessions is
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
            Self::SessionCatalogInvalidated { .. }
                | Self::CheckoutStateChanged(_)
                | Self::SessionCreated(_)
                | Self::SessionDeleted(_)
                | Self::SessionTitleChanged(_)
                | Self::SessionSettlementChanged(_)
                | Self::SessionWorkingChanged(_)
                | Self::SessionMonitoringChanged(_)
                | Self::SessionStandingInputsChanged(_)
                | Self::WorkspaceIconChanged(_)
                | Self::WorkspaceDescriptionChanged(_)
                | Self::SessionRemoteSubsessionsChanged(_)
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
    attachment_fetch_timeout: Duration,
    config_dir: Option<PathBuf>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct SessionCommandClient {
    http: reqwest::Client,
    descriptor: watch::Receiver<RuntimeDescriptor>,
    outlook: Outlook,
    initial_recovery_backoff: Duration,
    max_recovery_backoff: Duration,
    attachment_fetch_timeout: Duration,
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

    /// A client of no Server at all, for a test of what a client does with
    /// events it is handed rather than with requests it sends: it reports no
    /// events, and every request it sends fails.
    #[cfg(test)]
    pub(crate) fn offline() -> Self {
        let (_, events) = mpsc::channel(1);
        let (_, descriptor) = watch::channel(RuntimeDescriptor::new(
            "http://127.0.0.1:9".to_owned(),
            "offline-token".to_owned(),
            crate::protocol::ServerIdentity {
                instance_id: uuid::Uuid::new_v4(),
                pid: std::process::id(),
                protocol_version: crate::protocol::PROTOCOL_VERSION,
                build_identity: "offline".to_owned(),
            },
        ));
        Self {
            events,
            http: reqwest::Client::new(),
            descriptor,
            initial_recovery_backoff: Duration::from_millis(1),
            max_recovery_backoff: Duration::from_millis(1),
            attachment_fetch_timeout: ATTACHMENT_FETCH_TIMEOUT,
            config_dir: None,
            task: tokio::spawn(std::future::ready(())),
        }
    }

    pub(crate) fn config_dir(&self) -> Option<&Path> {
        self.config_dir.as_deref()
    }

    pub fn outlook(&self, outlook: Outlook) -> OutlookClient {
        OutlookClient {
            commands: self.session_commands_for(outlook),
        }
    }

    pub async fn preview_checkout_removal(
        &self,
        target: crate::protocol::CheckoutRemovalTarget,
    ) -> Result<crate::protocol::CheckoutRemovalPreview> {
        self.session_commands()
            .preview_checkout_removal(target)
            .await
    }
    pub async fn remove_checkout(
        &self,
        request: crate::protocol::RemoveCheckoutRequest,
    ) -> Result<crate::protocol::RemoveCheckoutResult> {
        self.session_commands().remove_checkout(request).await
    }
    pub async fn prepare_checkout(
        &self,
        request: crate::protocol::PrepareCheckoutRequest,
    ) -> Result<crate::protocol::PrepareCheckoutResult> {
        self.session_commands().prepare_checkout(request).await
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionSnapshot> {
        self.session_commands().create_session(request).await
    }

    /// The Sidekick Workspace of this Client's own Server, made there first if
    /// it is not there yet: a Session begun in it is a Sidekick's.
    pub async fn sidekick_workspace(&self) -> Result<crate::protocol::ResolvedWorkspace> {
        self.session_commands().sidekick_workspace().await
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

    pub async fn upload_attachment(&self, png: Vec<u8>) -> Result<AttachmentDescriptor> {
        self.session_commands().upload_attachment(png).await
    }

    pub async fn fetch_attachment(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(String, Vec<u8>)> {
        self.session_commands()
            .fetch_attachment(attachment_id)
            .await
    }

    pub async fn attachment_exists(&self, attachment_id: &AttachmentId) -> Result<bool> {
        self.session_commands()
            .attachment_exists(attachment_id)
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

    pub async fn update_approval_posture(
        &self,
        session_id: SessionId,
        request: UpdateApprovalPostureRequest,
    ) -> Result<SessionApprovalPosture> {
        self.session_commands()
            .update_approval_posture(session_id, request)
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

    pub async fn remove_remote(&self, name: &str) -> Result<RemoteRemoval> {
        self.session_commands().remove_remote(name).await
    }

    /// The Relays the Client's own Server holds entries for, whichever way
    /// the Outlook is turned.
    pub async fn list_relays(&self) -> Result<Vec<Relay>> {
        self.session_commands().list_relays().await
    }

    pub async fn add_relay(&self, address: impl Into<String>) -> Result<Relay> {
        self.session_commands().add_relay(address.into()).await
    }

    /// Has the Client's own Server begin a login at the Relay at `address`:
    /// where its user goes to log in, and what they enter there.
    pub async fn begin_relay_login(&self, address: &str) -> Result<RelayLogin> {
        self.session_commands().begin_relay_login(address).await
    }

    /// Waits for the latest login begun at the Relay at `address` to end,
    /// answering how it did.
    pub async fn follow_relay_login(&self, address: &str) -> Result<RelayLogin> {
        self.session_commands().follow_relay_login(address).await
    }

    pub async fn remove_relay(&self, address: &str) -> Result<RelayRemoval> {
        self.session_commands().remove_relay(address).await
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
    pub async fn submit_questionnaire(
        &self,
        session_id: SessionId,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> Result<()> {
        self.session_commands()
            .submit_questionnaire(session_id, id, submission)
            .await
    }

    pub async fn submit_decision(
        &self,
        session_id: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> Result<()> {
        self.session_commands()
            .submit_decision(session_id, id, decision)
            .await
    }

    pub async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.session_commands().interrupt_session(session_id).await
    }

    /// Asks the Session's Provider to compact its context now, which begins
    /// a Turn of its own holding that Compaction (ADR 0041). A refusal carries
    /// its typed [`crate::protocol::SessionError`].
    pub async fn compact_session(
        &self,
        session_id: SessionId,
        request: crate::protocol::CompactSessionRequest,
    ) -> Result<()> {
        self.session_commands()
            .compact_session(session_id, request)
            .await
    }

    /// Asks the Session's Provider what occupies its context now. A refusal
    /// carries its typed [`crate::protocol::SessionError`].
    pub async fn context_breakdown(
        &self,
        session_id: SessionId,
    ) -> Result<crate::protocol::ContextBreakdown> {
        self.session_commands().context_breakdown(session_id).await
    }

    /// Interrupts the Session and reports what the interrupt did: stopped work,
    /// or the undelivered Prompt it withdrew (ADR 0024).
    pub async fn interrupt_session_reporting_outcome(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptOutcome> {
        self.session_commands()
            .interrupt_session_reporting_outcome(session_id)
            .await
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

    /// Sets a Session's Icon to a user's choice from the Icon Catalog by
    /// name, refused where the Catalog does not carry it.
    pub async fn set_session_icon(
        &self,
        session_id: SessionId,
        icon: &str,
    ) -> Result<SessionSummary> {
        self.session_commands()
            .set_session_icon(session_id, icon)
            .await
    }

    /// Sets a Workspace's Icon to a user's own choice from the Icon Catalog by
    /// name, refused where the Catalog does not carry it or where this Client's
    /// own local server does not know the Workspace. A Remote Workspace's own
    /// choice reaches it through [`Self::outlook`] instead, the way every
    /// other Remote-addressed command does.
    pub async fn set_workspace_icon(&self, workspace_id: &WorkspaceId, icon: &str) -> Result<()> {
        self.session_commands()
            .set_workspace_icon(workspace_id, icon)
            .await
    }

    /// Sets a Workspace's Description to the user's own text, or clears it
    /// where the text is blank, refused where this Client's own local server
    /// cannot tell the Workspace is its own or the text runs longer than a
    /// Description may. `path`, where the Workspace is presented, is what
    /// lets the server take as its own a Workspace no Session has been begun
    /// in yet. A Remote Workspace's own reaches it through [`Self::outlook`]
    /// instead, the way its chosen Icon does.
    pub async fn set_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        path: Option<&std::path::Path>,
        description: &str,
    ) -> Result<()> {
        self.session_commands()
            .set_workspace_description(workspace_id, path, description)
            .await
    }

    pub async fn view_session(
        &self,
        session_id: SessionId,
        request: ViewSessionRequest,
    ) -> Result<SessionSummary> {
        self.session_commands()
            .view_session(session_id, request)
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

    /// Asks the server to discover the Models of every Provider it has not
    /// yet heard from this process. The answer arrives as Model Catalog
    /// events, not here.
    pub async fn warm_models(&self) -> Result<()> {
        self.session_commands().warm_models().await
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

    /// Subscribes to the tree `session_id` belongs to on the local Server.
    pub fn subscribe_subagent_tree(&self, session_id: SessionId) -> SubagentTreeSubscription {
        self.session_commands().subscribe_subagent_tree(session_id)
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
            attachment_fetch_timeout: self.attachment_fetch_timeout,
        }
    }
}

impl OutlookClient {
    pub fn subscribe_catalog(&self) -> SessionCatalogSubscription {
        self.commands.subscribe_catalog()
    }

    /// Subscribes to the tree `session_id` belongs to, headed by its
    /// top-level Session, on the Server this Outlook names.
    pub fn subscribe_subagent_tree(&self, session_id: SessionId) -> SubagentTreeSubscription {
        self.commands.subscribe_subagent_tree(session_id)
    }

    pub async fn preview_checkout_removal(
        &self,
        target: crate::protocol::CheckoutRemovalTarget,
    ) -> Result<crate::protocol::CheckoutRemovalPreview> {
        self.commands.preview_checkout_removal(target).await
    }
    pub async fn remove_checkout(
        &self,
        request: crate::protocol::RemoveCheckoutRequest,
    ) -> Result<crate::protocol::RemoveCheckoutResult> {
        self.commands.remove_checkout(request).await
    }
    pub async fn prepare_checkout(
        &self,
        request: crate::protocol::PrepareCheckoutRequest,
    ) -> Result<crate::protocol::PrepareCheckoutResult> {
        self.commands.prepare_checkout(request).await
    }

    pub async fn create_session(&self, request: CreateSessionRequest) -> Result<SessionSnapshot> {
        self.commands.create_session(request).await
    }

    pub async fn resolve_workspace(
        &self,
        request: ResolveWorkspaceRequest,
    ) -> Result<crate::protocol::ResolvedWorkspace> {
        self.commands.resolve_workspace(request).await
    }

    /// The Sidekick Workspace of the Server this Client's own Outlook names,
    /// made there first if it is not there yet.
    pub async fn sidekick_workspace(&self) -> Result<crate::protocol::ResolvedWorkspace> {
        self.commands.sidekick_workspace().await
    }

    pub async fn admit_prompt(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
    ) -> Result<Prompt> {
        self.commands.admit_prompt(session_id, request).await
    }

    pub async fn upload_attachment(&self, png: Vec<u8>) -> Result<AttachmentDescriptor> {
        self.commands.upload_attachment(png).await
    }

    pub async fn fetch_attachment(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(String, Vec<u8>)> {
        self.commands.fetch_attachment(attachment_id).await
    }

    pub async fn attachment_exists(&self, attachment_id: &AttachmentId) -> Result<bool> {
        self.commands.attachment_exists(attachment_id).await
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

    pub async fn update_approval_posture(
        &self,
        session_id: SessionId,
        request: UpdateApprovalPostureRequest,
    ) -> Result<SessionApprovalPosture> {
        self.commands
            .update_approval_posture(session_id, request)
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

    pub async fn submit_decision(
        &self,
        session_id: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> Result<()> {
        self.commands
            .submit_decision(session_id, id, decision)
            .await
    }

    pub async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.commands.interrupt_session(session_id).await
    }

    /// Asks the Session's Provider to compact its context now, which begins
    /// a Turn of its own holding that Compaction (ADR 0041).
    pub async fn compact_session(
        &self,
        session_id: SessionId,
        request: crate::protocol::CompactSessionRequest,
    ) -> Result<()> {
        self.commands.compact_session(session_id, request).await
    }

    /// Asks the Session's Provider what occupies its context now.
    pub async fn context_breakdown(
        &self,
        session_id: SessionId,
    ) -> Result<crate::protocol::ContextBreakdown> {
        self.commands.context_breakdown(session_id).await
    }

    /// Interrupts the Session and reports what the interrupt did: stopped work,
    /// or the undelivered Prompt it withdrew (ADR 0024).
    pub async fn interrupt_session_reporting_outcome(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptOutcome> {
        self.commands
            .interrupt_session_reporting_outcome(session_id)
            .await
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

    /// Sets a Session's Icon to a user's choice from the Icon Catalog by
    /// name, refused where the Catalog does not carry it.
    pub async fn set_session_icon(
        &self,
        session_id: SessionId,
        icon: &str,
    ) -> Result<SessionSummary> {
        self.commands.set_session_icon(session_id, icon).await
    }

    /// Sets a Workspace's Icon to a user's own choice from the Icon Catalog by
    /// name, refused where the Catalog does not carry it or where this
    /// Outlook's own server does not know the Workspace.
    pub async fn set_workspace_icon(&self, workspace_id: &WorkspaceId, icon: &str) -> Result<()> {
        self.commands.set_workspace_icon(workspace_id, icon).await
    }

    /// Sets a Workspace's Description to the user's own text, or clears it
    /// where the text is blank, on this Outlook's own server.
    pub async fn set_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        path: Option<&std::path::Path>,
        description: &str,
    ) -> Result<()> {
        self.commands
            .set_workspace_description(workspace_id, path, description)
            .await
    }

    pub async fn view_session(
        &self,
        session_id: SessionId,
        request: ViewSessionRequest,
    ) -> Result<SessionSummary> {
        self.commands.view_session(session_id, request).await
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
    pub(crate) fn recovery_backoff(&self) -> (Duration, Duration) {
        (self.initial_recovery_backoff, self.max_recovery_backoff)
    }

    pub(crate) fn subscribe_catalog(&self) -> SessionCatalogSubscription {
        SessionCatalogSubscription::open_attached(
            self.http.clone(),
            self.descriptor.clone(),
            self.outlook.clone(),
            self.initial_recovery_backoff,
            self.max_recovery_backoff,
        )
    }

    pub(crate) fn subscribe_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> SubagentTreeSubscription {
        SubagentTreeSubscription::open(
            self.http.clone(),
            self.descriptor.clone(),
            self.outlook.clone(),
            session_id,
            self.initial_recovery_backoff,
            self.max_recovery_backoff,
        )
    }

    pub(crate) async fn resolve_workspace(
        &self,
        request: ResolveWorkspaceRequest,
    ) -> Result<crate::protocol::ResolvedWorkspace> {
        self.post_session_command("/v1/workspaces/resolve", &request, "Workspace resolution")
            .await
    }

    /// The Sidekick Workspace of the Server this Outlook names, which that
    /// Server makes the first time it is asked for.
    pub(crate) async fn sidekick_workspace(&self) -> Result<crate::protocol::ResolvedWorkspace> {
        self.post_session_command_without_body("/v1/workspaces/sidekick", "Sidekick Workspace")
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

    pub(crate) async fn warm_models(&self) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        self.http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/models/warm",
            )?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Model catalog warm-up")?
            .error_for_status()
            .context("Model catalog warm-up")?;
        Ok(())
    }
    pub(crate) async fn preview_checkout_removal(
        &self,
        target: crate::protocol::CheckoutRemovalTarget,
    ) -> Result<crate::protocol::CheckoutRemovalPreview> {
        self.post_session_command(
            "/v1/checkouts/removal-preview",
            &target,
            "Worktree removal preview",
        )
        .await
    }
    pub(crate) async fn remove_checkout(
        &self,
        request: crate::protocol::RemoveCheckoutRequest,
    ) -> Result<crate::protocol::RemoveCheckoutResult> {
        self.post_session_command("/v1/checkouts/remove", &request, "Worktree removal")
            .await
    }
    pub(crate) async fn prepare_checkout(
        &self,
        request: crate::protocol::PrepareCheckoutRequest,
    ) -> Result<crate::protocol::PrepareCheckoutResult> {
        self.post_session_command("/v1/checkouts/prepare", &request, "Worktree preparation")
            .await
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

    /// Runs one Attachment request, `action` naming it, within the client's
    /// Attachment deadline. The deadline covers the whole request — its
    /// headers and all of its body — so a Server that takes it and then
    /// stalls fails it rather than leaving it pending for good.
    async fn within_attachment_deadline<T>(
        &self,
        action: &str,
        request: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let deadline = self.attachment_fetch_timeout;
        tokio::time::timeout(deadline, request)
            .await
            .unwrap_or_else(|_| Err(anyhow!("{action} timed out after {deadline:?}")))
    }

    /// Uploads an image's PNG bytes as an Attachment on the Server this
    /// client's Outlook names, answering with what it was stored as. A
    /// refusal is the Server's own `SessionError`, whose message a client can
    /// show as it stands.
    pub(crate) async fn upload_attachment(&self, png: Vec<u8>) -> Result<AttachmentDescriptor> {
        self.within_attachment_deadline("Attachment upload", async {
            let descriptor = self.descriptor.borrow().clone();
            let response = self
                .http
                .post(server_url(
                    &descriptor.base_url,
                    &self.outlook,
                    "/v1/attachments",
                )?)
                .bearer_auth(&descriptor.token)
                .header(reqwest::header::CONTENT_TYPE, "image/png")
                .body(png)
                .send()
                .await
                .context("send Attachment upload")?;
            decode_api_response(response, "Attachment upload").await
        })
        .await
    }

    /// Fetches a stored Attachment's bytes, with the type they were stored as.
    /// A body past the 5 MiB an Attachment may be is refused as it arrives,
    /// rather than read to its end: nothing the Server admitted is that large,
    /// so nothing that large is taken for an Attachment.
    pub(crate) async fn fetch_attachment(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(String, Vec<u8>)> {
        self.within_attachment_deadline("Attachment fetch", self.read_attachment(attachment_id))
            .await
    }

    async fn read_attachment(&self, attachment_id: &AttachmentId) -> Result<(String, Vec<u8>)> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/attachments/{attachment_id}"),
            )?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Attachment fetch")?;
        if !response.status().is_success() {
            return Err(decode_api_error(response, "Attachment fetch").await);
        }
        let mime_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("Attachment fetch answered without a content type"))?
            .to_owned();
        let limit = crate::attachments::MAX_ATTACHMENT_BYTES;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            bail!("the fetched Attachment is larger than an Attachment may be");
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read the fetched Attachment")?
        {
            if bytes.len() + chunk.len() > limit {
                bail!("the fetched Attachment is larger than an Attachment may be");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((mime_type, bytes))
    }

    /// Whether the Server stores an Attachment under `attachment_id`, asked
    /// with a `HEAD` of its fetch route so none of its bytes are sent. Only a
    /// Not Found whose code header says `attachment_not_found` means it is
    /// not stored there: any other refusal, a Remote this Server no longer
    /// knows among them, is an error, so the Attachment is not taken for gone.
    pub(crate) async fn attachment_exists(&self, attachment_id: &AttachmentId) -> Result<bool> {
        let response = self
            .within_attachment_deadline("Attachment check", async {
                let descriptor = self.descriptor.borrow().clone();
                self.http
                    .head(server_url(
                        &descriptor.base_url,
                        &self.outlook,
                        &format!("/v1/attachments/{attachment_id}"),
                    )?)
                    .bearer_auth(&descriptor.token)
                    .send()
                    .await
                    .context("send Attachment check")
            })
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(true);
        }
        let code = response
            .headers()
            .get(SESSION_ERROR_CODE_HEADER)
            .and_then(|code| code.to_str().ok())
            .unwrap_or("none");
        if status == reqwest::StatusCode::NOT_FOUND
            && code == SessionErrorCode::AttachmentNotFound.wire_name()
        {
            return Ok(false);
        }
        Err(anyhow!(
            "Attachment check failed with HTTP {status} (error code {code})"
        ))
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

    pub(crate) async fn update_approval_posture(
        &self,
        session_id: SessionId,
        request: UpdateApprovalPostureRequest,
    ) -> Result<SessionApprovalPosture> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/approval-posture"),
            &request,
            "Approval Posture update",
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

    pub(crate) async fn submit_questionnaire(
        &self,
        session_id: SessionId,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}/questionnaires/{id}"),
            )?)
            .bearer_auth(&descriptor.token)
            .json(&submission)
            .send()
            .await
            .context("send Questionnaire submission")?;
        decode_empty_api_response(response, "Questionnaire submission").await
    }

    pub(crate) async fn submit_decision(
        &self,
        session_id: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}/approvals/{id}/decision"),
            )?)
            .bearer_auth(&descriptor.token)
            .json(&decision)
            .send()
            .await
            .context("send Approval Decision")?;
        decode_empty_api_response(response, "Approval Decision").await
    }

    pub(crate) async fn interrupt_session(&self, session_id: SessionId) -> Result<()> {
        self.interrupt_session_reporting_outcome(session_id)
            .await
            .map(|_| ())
    }

    pub(crate) async fn compact_session(
        &self,
        session_id: SessionId,
        request: crate::protocol::CompactSessionRequest,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}/compact"),
            )?)
            .bearer_auth(&descriptor.token)
            .json(&request)
            .send()
            .await
            .context("send Session compaction request")?;
        decode_empty_api_response(response, "Session compaction").await
    }

    /// Interrupts the Session and reports what the interrupt did, so a client
    /// that withdrew an undelivered Prompt can return its text to the composer
    /// it was submitted from (ADR 0024). Stopping work answers with no body at
    /// all, which is [`InterruptOutcome::StoppedWork`].
    pub(crate) async fn interrupt_session_reporting_outcome(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptOutcome> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}/interrupt"),
            )?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Session interruption command")?;
        if !response.status().is_success() {
            return Err(decode_api_error(response, "Session interruption").await);
        }
        let body = response
            .bytes()
            .await
            .context("read Session interruption outcome")?;
        if body.is_empty() {
            return Ok(InterruptOutcome::StoppedWork);
        }
        serde_json::from_slice(&body).context("decode Session interruption outcome")
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

    pub(crate) async fn remove_remote(&self, name: &str) -> Result<RemoteRemoval> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .delete(remote_removal_url(&descriptor.base_url, name)?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Remote removal")?;
        decode_api_response(response, "Remote removal").await
    }

    pub(crate) async fn list_relays(&self) -> Result<Vec<Relay>> {
        self.get_pairing_resource("/v1/relays", "Relay listing")
            .await
    }

    pub(crate) async fn add_relay(&self, address: String) -> Result<Relay> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(relay_url(&descriptor.base_url, &[])?)
            .bearer_auth(&descriptor.token)
            .json(&AddRelayRequest { address })
            .send()
            .await
            .context("send Relay addition")?;
        decode_api_response(response, "Relay addition").await
    }

    pub(crate) async fn begin_relay_login(&self, address: &str) -> Result<RelayLogin> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(relay_url(&descriptor.base_url, &[address, "login"])?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Relay login")?;
        decode_api_response(response, "Relay login").await
    }

    pub(crate) async fn follow_relay_login(&self, address: &str) -> Result<RelayLogin> {
        use eventsource_stream::Eventsource;
        use futures_util::StreamExt;

        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(relay_url(&descriptor.base_url, &[address, "login"])?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("follow Relay login")?;
        if !response.status().is_success() {
            return Err(decode_api_error(response, "Relay login").await);
        }
        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event.context("read Relay login progress")?;
            if event.event != crate::protocol::RELAY_LOGIN_EVENT {
                continue;
            }
            let login = serde_json::from_str::<RelayLogin>(&event.data)
                .context("decode Relay login progress")?;
            if login.outcome.is_settled() {
                return Ok(login);
            }
        }
        bail!("the Relay login was given up before it ended")
    }

    pub(crate) async fn remove_relay(&self, address: &str) -> Result<RelayRemoval> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .delete(relay_url(&descriptor.base_url, &[address])?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Relay removal")?;
        decode_api_response(response, "Relay removal").await
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

    pub(crate) async fn context_breakdown(
        &self,
        session_id: SessionId,
    ) -> Result<crate::protocol::ContextBreakdown> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                &format!("/v1/sessions/{session_id}/context"),
            )?)
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .context("send Context Breakdown request")?;
        decode_api_response(response, "Context Breakdown").await
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

    /// Sets a Session's Icon to a user's own choice from the Icon Catalog by
    /// name. Routed like every other Session command: against this Client's
    /// own Outlook, which is the Session's Origin wherever a caller reached
    /// this through [`ManagedClient::session_commands_for`].
    pub(crate) async fn set_session_icon(
        &self,
        session_id: SessionId,
        icon: &str,
    ) -> Result<SessionSummary> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/icon"),
            &SetSessionIconRequest {
                icon: icon.to_owned(),
            },
            "Session Icon",
        )
        .await
    }

    /// Sets a Workspace's Icon to a user's own choice from the Icon Catalog by
    /// name. Routed like every other Session-adjacent command: against this
    /// Client's own Outlook, which is the Workspace's Origin wherever a
    /// caller reached this through [`ManagedClient::session_commands_for`].
    /// The Workspace's identity travels in the request body rather than a URL
    /// path segment, since a `WorkspaceId`'s inner string may itself contain
    /// path characters.
    pub(crate) async fn set_workspace_icon(
        &self,
        workspace_id: &WorkspaceId,
        icon: &str,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/workspaces/icon",
            )?)
            .bearer_auth(&descriptor.token)
            .json(&SetWorkspaceIconRequest {
                workspace_id: workspace_id.clone(),
                icon: icon.to_owned(),
            })
            .send()
            .await
            .context("send Workspace Icon command")?;
        decode_empty_api_response(response, "Workspace Icon").await
    }

    /// Sets a Workspace's Description, or clears it where the text is blank.
    /// Routed exactly like [`Self::set_workspace_icon`]: against this
    /// Client's own Outlook, which is the Workspace's Origin wherever a
    /// caller reached this through [`ManagedClient::session_commands_for`].
    pub(crate) async fn set_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        path: Option<&std::path::Path>,
        description: &str,
    ) -> Result<()> {
        let descriptor = self.descriptor.borrow().clone();
        let response = self
            .http
            .post(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/workspaces/description",
            )?)
            .bearer_auth(&descriptor.token)
            .json(&SetWorkspaceDescriptionRequest {
                workspace_id: workspace_id.clone(),
                path: path.map(std::path::Path::to_path_buf),
                description: description.to_owned(),
            })
            .send()
            .await
            .context("send Workspace Description command")?;
        decode_empty_api_response(response, "Workspace Description").await
    }

    /// Reports that this Client has the Session open in its main view.
    pub(crate) async fn view_session(
        &self,
        session_id: SessionId,
        request: ViewSessionRequest,
    ) -> Result<SessionSummary> {
        self.post_session_command(
            &format!("/v1/sessions/{session_id}/viewed"),
            &request,
            "Session viewed",
        )
        .await
    }

    pub(crate) async fn list_workspace_sessions(
        &self,
        workspace: Option<&crate::protocol::WorkspaceId>,
    ) -> Result<Vec<SessionListItem>> {
        let descriptor = self.descriptor.borrow().clone();
        let mut request = self
            .http
            .get(server_url(
                &descriptor.base_url,
                &self.outlook,
                "/v1/sessions",
            )?)
            .bearer_auth(&descriptor.token);
        if let Some(workspace) = workspace {
            request = request.query(&[("workspace_id", &workspace.0)]);
        }
        decode_api_response(
            request.send().await.context("send Session listing")?,
            "Session listing",
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
            self.initial_recovery_backoff,
            self.max_recovery_backoff,
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
    let baseline = launcher::StopBaseline::read(config);
    let deadline = tokio::time::Instant::now() + config.startup_timeout;
    launcher::ensure_server(config, deadline, baseline)
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

/// The local Server's Relay route beneath `/v1/relays` for `segments`, each
/// one encoded whole — a Relay's address among them, slashes and all.
fn relay_url(base_url: &str, segments: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("parse server base URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("server base URL cannot contain path segments"))?
        .extend(["v1", "relays"])
        .extend(segments);
    Ok(url)
}

fn remote_removal_url(base_url: &str, name: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url).context("parse server base URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("server base URL cannot contain path segments"))?
        .extend(["v1", "pairing", "remotes", name]);
    Ok(url)
}

impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionCommandClient, remote_probe_url, remote_removal_url, server_url};
    use crate::protocol::Outlook;

    mod attachment_fetch {
        use std::time::Duration;

        use axum::{
            Router,
            body::Body,
            extract::Path,
            http::{StatusCode, header::CONTENT_TYPE},
            response::{IntoResponse, Response},
            routing::{get, head, post},
        };
        use futures_util::StreamExt;
        use tokio::sync::watch;

        use super::SessionCommandClient;
        use crate::{
            attachments::MAX_ATTACHMENT_BYTES,
            managed_client::ATTACHMENT_FETCH_TIMEOUT,
            protocol::{
                AttachmentId, Outlook, PROTOCOL_VERSION, RuntimeDescriptor, ServerIdentity,
            },
        };

        /// Answers `/v1/attachments/{id}` with a PNG's worth of bytes of a
        /// length the id names: `exact` is the cap, `over` one byte past it
        /// and saying so, and `streamed-over` one byte past it without saying
        /// so up front.
        async fn answer(Path(id): Path<String>) -> Response {
            let png = [(CONTENT_TYPE, "image/png")];
            match id.as_str() {
                "exact" => (png, vec![0_u8; MAX_ATTACHMENT_BYTES]).into_response(),
                "over" => (png, vec![0_u8; MAX_ATTACHMENT_BYTES + 1]).into_response(),
                _ => {
                    let chunks = (0..=MAX_ATTACHMENT_BYTES / 65_536)
                        .map(|_| Ok::<_, std::io::Error>(vec![0_u8; 65_536]));
                    (png, Body::from_stream(futures_util::stream::iter(chunks))).into_response()
                }
            }
        }

        async fn client() -> (SessionCommandClient, tokio::task::JoinHandle<()>) {
            fixture_client(Router::new().route("/v1/attachments/{id}", get(answer))).await
        }

        /// A client of a fixture Server answering with `app`.
        pub(super) async fn fixture_client(
            app: Router,
        ) -> (SessionCommandClient, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind the Attachment fixture");
            let address = listener.local_addr().expect("read the fixture address");
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("serve the Attachment fixture");
            });
            let (_, descriptor) = watch::channel(RuntimeDescriptor::new(
                format!("http://{address}"),
                "attachment-fixture-token".to_owned(),
                ServerIdentity {
                    instance_id: uuid::Uuid::new_v4(),
                    pid: std::process::id(),
                    protocol_version: PROTOCOL_VERSION,
                    build_identity: "attachment-fixture".to_owned(),
                },
            ));
            let client = SessionCommandClient {
                http: reqwest::Client::new(),
                descriptor,
                outlook: Outlook::Local,
                initial_recovery_backoff: Duration::from_millis(1),
                max_recovery_backoff: Duration::from_millis(1),
                attachment_fetch_timeout: ATTACHMENT_FETCH_TIMEOUT,
            };
            (client, server)
        }

        /// The deadline a stalled request is failed at.
        const DEADLINE: Duration = Duration::from_millis(50);

        /// A client of a fixture Server answering with `app`, whose Attachment
        /// requests are failed at `deadline`.
        async fn client_within(
            app: Router,
            deadline: Duration,
        ) -> (SessionCommandClient, tokio::task::JoinHandle<()>) {
            let (mut client, server) = fixture_client(app).await;
            client.attachment_fetch_timeout = deadline;
            (client, server)
        }

        /// Awaits `request`, which must end by itself: a guard long past any
        /// deadline a test sets fails the test rather than letting it hang.
        async fn ends_by_itself<T>(request: impl Future<Output = T>) -> T {
            tokio::time::timeout(Duration::from_secs(5), request)
                .await
                .expect("the request ends at its own deadline")
        }

        /// Answers `/v1/attachments/{id}` and stalls where the id says:
        /// `headers` before sending any, and `body` after its first chunk.
        async fn stall(Path(id): Path<String>) -> Response {
            if id == "headers" {
                std::future::pending::<()>().await;
            }
            let first =
                futures_util::stream::once(async { Ok::<_, std::io::Error>(vec![0_u8; 1024]) });
            let body = Body::from_stream(first.chain(futures_util::stream::pending()));
            ([(CONTENT_TYPE, "image/png")], body).into_response()
        }

        /// Answers `/v1/attachments/{id}` with its body in three chunks a few
        /// milliseconds apart.
        async fn trickle() -> Response {
            let chunks = futures_util::stream::iter(0..3).then(|_| async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok::<_, std::io::Error>(vec![0_u8; 1024])
            });
            ([(CONTENT_TYPE, "image/png")], Body::from_stream(chunks)).into_response()
        }

        #[tokio::test]
        async fn a_fetch_the_server_stalls_fails_at_its_deadline() {
            let (client, server) = client_within(
                Router::new().route("/v1/attachments/{id}", get(stall)),
                DEADLINE,
            )
            .await;

            for id in ["headers", "body"] {
                let refused = ends_by_itself(client.fetch_attachment(&AttachmentId::new(id)))
                    .await
                    .expect_err("a stalled fetch fails");
                assert!(
                    refused
                        .to_string()
                        .contains("Attachment fetch timed out after 50ms"),
                    "{id}: {refused:#}"
                );
            }
            server.abort();
        }

        #[tokio::test]
        async fn a_fetch_finished_within_its_deadline_is_taken() {
            let (client, server) = client_within(
                Router::new().route("/v1/attachments/{id}", get(trickle)),
                Duration::from_millis(500),
            )
            .await;

            let (mime_type, bytes) = client
                .fetch_attachment(&AttachmentId::new("trickled"))
                .await
                .expect("a body read to its end in time is an Attachment");
            assert_eq!(mime_type, "image/png");
            assert_eq!(bytes.len(), 3 * 1024);
            server.abort();
        }

        #[tokio::test]
        async fn an_upload_or_a_check_the_server_stalls_fails_at_its_deadline() {
            async fn stalled() -> StatusCode {
                std::future::pending().await
            }
            let (client, server) = client_within(
                Router::new()
                    .route("/v1/attachments", post(stalled))
                    .route("/v1/attachments/{id}", head(stalled)),
                DEADLINE,
            )
            .await;

            let upload = ends_by_itself(client.upload_attachment(vec![0_u8; 16]))
                .await
                .expect_err("a stalled upload fails");
            assert!(
                upload
                    .to_string()
                    .contains("Attachment upload timed out after 50ms"),
                "{upload:#}"
            );
            let check = ends_by_itself(client.attachment_exists(&AttachmentId::new("stored")))
                .await
                .expect_err("a stalled check fails");
            assert!(
                check
                    .to_string()
                    .contains("Attachment check timed out after 50ms"),
                "{check:#}"
            );
            server.abort();
        }

        #[tokio::test]
        async fn a_fetch_over_the_attachment_cap_is_refused_and_one_at_it_is_taken() {
            let (client, server) = client().await;

            let (mime_type, bytes) = client
                .fetch_attachment(&AttachmentId::new("exact"))
                .await
                .expect("a body at the cap is an Attachment");
            assert_eq!(mime_type, "image/png");
            assert_eq!(bytes.len(), MAX_ATTACHMENT_BYTES);

            for id in ["over", "streamed-over"] {
                let refused = client
                    .fetch_attachment(&AttachmentId::new(id))
                    .await
                    .expect_err("a body past the cap is refused");
                assert!(
                    refused
                        .to_string()
                        .contains("larger than an Attachment may be"),
                    "{id}: {refused:#}"
                );
            }
            server.abort();
        }
    }

    mod attachment_check {
        use axum::{
            Router,
            extract::Path,
            http::StatusCode,
            response::{IntoResponse, Response},
            routing::head,
        };

        use super::attachment_fetch::fixture_client;
        use crate::protocol::{AttachmentId, SESSION_ERROR_CODE_HEADER};

        /// Answers a `HEAD` of `/v1/attachments/{id}` as the id names: stored,
        /// not stored, a Remote the proxy no longer knows, a Not Found that
        /// does not say why, and a failure.
        async fn answer(Path(id): Path<String>) -> Response {
            match id.as_str() {
                "stored" => StatusCode::OK.into_response(),
                "swept" => (
                    StatusCode::NOT_FOUND,
                    [(SESSION_ERROR_CODE_HEADER, "attachment_not_found")],
                )
                    .into_response(),
                "unpaired" => (
                    StatusCode::NOT_FOUND,
                    [(SESSION_ERROR_CODE_HEADER, "remote_not_found")],
                )
                    .into_response(),
                "silent" => StatusCode::NOT_FOUND.into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }

        #[tokio::test]
        async fn only_a_not_found_naming_the_attachment_reads_as_not_stored() {
            let (client, server) =
                fixture_client(Router::new().route("/v1/attachments/{id}", head(answer))).await;
            let exists = |id: &str| {
                let client = client.clone();
                let id = AttachmentId::new(id);
                async move { client.attachment_exists(&id).await }
            };

            assert!(exists("stored").await.expect("a stored Attachment answers"));
            assert!(!exists("swept").await.expect("a missing Attachment answers"));
            for id in ["unpaired", "silent", "broken"] {
                let error = exists(id)
                    .await
                    .expect_err("a refusal not about the Attachment is no answer");
                assert!(
                    error.to_string().contains("Attachment check failed"),
                    "{id}: {error:#}"
                );
            }
            server.abort();
        }
    }

    #[test]
    fn remote_probe_url_encodes_a_freely_editable_name_as_one_path_segment() {
        let url = remote_probe_url("http://127.0.0.1:7777", "lab/east?#")
            .expect("build Remote probe URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/pairing/remotes/lab%2Feast%3F%23/health"
        );
    }

    #[test]
    fn remote_removal_url_encodes_a_freely_editable_name_as_one_path_segment() {
        let url = remote_removal_url("http://127.0.0.1:7777", "lab/east?#")
            .expect("build Remote removal URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/pairing/remotes/lab%2Feast%3F%23"
        );
    }

    #[test]
    fn approval_decision_uses_the_remote_session_command_route() {
        let url = server_url(
            "http://127.0.0.1:7777",
            &Outlook::Remote("workstation".into()),
            "/v1/sessions/session-id/approvals/approval-id/decision",
        )
        .expect("build remote Decision URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/remotes/workstation/v1/sessions/session-id/approvals/approval-id/decision"
        );
    }

    /// A chosen Icon routes to a remote Session's own Origin exactly like
    /// every other Session command: `session_commands_for(reference.origin)`
    /// carries this Outlook into every request it sends, `set_session_icon`
    /// included, and `server_url` is where that carrying becomes the request
    /// a remote Server actually receives.
    #[test]
    fn set_session_icon_uses_the_remote_session_command_route() {
        let url = server_url(
            "http://127.0.0.1:7777",
            &Outlook::Remote("workstation".into()),
            "/v1/sessions/session-id/icon",
        )
        .expect("build remote Icon URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/remotes/workstation/v1/sessions/session-id/icon"
        );
    }

    /// A chosen Workspace Icon routes to that Workspace's own Origin exactly
    /// like a chosen Session Icon does: `session_commands_for(reference.origin)`
    /// carries this Outlook into every request it sends, `set_workspace_icon`
    /// included, and `server_url` is where that carrying becomes the request a
    /// remote Server actually receives. The Workspace's identity is not part of
    /// this path at all — it travels in the body instead, so this only proves
    /// the route reaches the right Server.
    #[test]
    fn set_workspace_icon_uses_the_remote_session_command_route() {
        let url = server_url(
            "http://127.0.0.1:7777",
            &Outlook::Remote("workstation".into()),
            "/v1/workspaces/icon",
        )
        .expect("build remote Workspace Icon URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/remotes/workstation/v1/workspaces/icon"
        );
    }

    /// `/sidekick` turned toward a Remote asks that Remote for its own Sidekick
    /// Workspace, through the same route every other Session command takes.
    #[test]
    fn the_sidekick_workspace_is_asked_of_the_outlooks_own_server() {
        let url = server_url(
            "http://127.0.0.1:7777",
            &Outlook::Remote("workstation".into()),
            "/v1/workspaces/sidekick",
        )
        .expect("build remote Sidekick Workspace URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/remotes/workstation/v1/workspaces/sidekick"
        );
    }

    /// A Workspace's Description routes to that Workspace's own Origin the
    /// way its chosen Icon does, so a Remote's Workspace is described on the
    /// Remote, which a Peer may ask it of.
    #[test]
    fn set_workspace_description_uses_the_remote_session_command_route() {
        let url = server_url(
            "http://127.0.0.1:7777",
            &Outlook::Remote("workstation".into()),
            "/v1/workspaces/description",
        )
        .expect("build remote Workspace Description URL");

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:7777/v1/remotes/workstation/v1/workspaces/description"
        );
    }
}
