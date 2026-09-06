//! Terminal lifecycle and the async run loop: the terminal session guard, the
//! `tokio::select!` loop that feeds the Application, and the tasks it spawns to
//! carry out the transitions the Application returns.

use std::{
    collections::{HashMap, HashSet},
    future::{Future, pending},
    net::{IpAddr, SocketAddr, SocketAddrV6},
    ops::ControlFlow,
    path::PathBuf,
    pin::Pin,
    time::{Duration, Instant},
};

use crate::{
    managed_client::{
        ManagedClient, ManagedEvent, RecoveryBackoff, SessionCatalogSubscription,
        SessionCommandClient, SessionEvent, SessionStreamError, SessionSubscription,
    },
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, CreateSessionRequest,
        ModelCatalog, Outlook, PromptId, ResolveWorkspaceRequest, SessionId, SessionListItem,
        SessionReference, SessionSnapshot, SettingMutation, SettingsSnapshot, SkillCatalog,
        SkillCatalogRequest, UpdateAgentSelectionRequest, Workspace,
    },
};
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent},
    execute,
    style::available_color_count,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use termina::Terminal as _;
use tokio::sync::mpsc::UnboundedSender;

use super::attachment::{AttachmentOperationId, AttachmentOutcome, SessionAttachment};
use super::commands::SemanticCommandId;
use super::shimmer;
use super::state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, EverywhereListRequest,
    ModelListRequest, SessionListRequest, SessionListSurface, WorkspaceResolutionSurface,
};
use crate::terminal::{
    DEFAULT_TERMINAL_PROBE_BUDGET, TerminalEvents, TerminalFacts, TerminalInput,
    request_terminal_colors,
};

const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(1);

pub async fn run(client: ManagedClient) -> Result<()> {
    let workspace =
        std::env::current_dir().map_err(|error| anyhow!("read current Workspace: {error}"))?;
    let mut session = TerminalSession::enter()?;
    let mut input = TerminalEvents::open()?;
    let terminal_probe = input
        .probe_colors(
            session.terminal.backend_mut(),
            DEFAULT_TERMINAL_PROBE_BUDGET,
        )
        .await?;
    let terminal_facts = TerminalFacts::new(terminal_probe, available_color_count() == u16::MAX);
    run_loop(
        &mut session.terminal,
        client,
        workspace,
        terminal_facts,
        input,
    )
    .await
}

/// How the run loop leaves the screen when an event ends the run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Exit {
    /// Stop immediately.
    Now,
    /// Render one last frame so the closing state reaches the screen first.
    AfterFinalFrame,
}

/// The Session-scoped work the run loop owns: the live event subscription and
/// the tasks establishing it, attaching a Session, and filling the pickers.
#[derive(Default)]
struct SessionTasks {
    subscription: Option<SessionSubscription>,
    subscribing: Option<(SessionReference, tokio::task::JoinHandle<()>)>,
    /// The Session attachment the reader is waiting on, correlated so that
    /// only their newest choice can land.
    attachment: SessionAttachment,
    /// One in-flight Session listing per surface and Origin: the picker and
    /// Sidebar list at once, while Everywhere lets the Sidebar ask several
    /// Servers concurrently. A fresh request supersedes only that exact
    /// surface-and-Origin conversation.
    listing_sessions:
        HashMap<(SessionListSurface, Outlook), (SessionListRequest, tokio::task::JoinHandle<()>)>,
    listing_models: Option<(ModelListRequest, tokio::task::JoinHandle<()>)>,
    listing_skills: Option<((Outlook, SkillCatalogRequest), tokio::task::JoinHandle<()>)>,
    catalog_origins: HashMap<Outlook, tokio::task::JoinHandle<()>>,
    resolving_workspaces: HashMap<WorkspaceResolutionSurface, (u64, tokio::task::JoinHandle<()>)>,
}

impl SessionTasks {
    /// Drops the live subscription, leaving any attempt to establish a new one
    /// running: the Session itself is still current.
    fn end_subscription(&mut self) {
        self.subscription = None;
    }

    /// Drops the live subscription along with any attempt to re-establish it,
    /// so nothing reconnects to a Session left behind.
    ///
    /// Attachment is left alone: this is also the housekeeping a client with
    /// no Session open does, and a reader attaching one from the Landing has
    /// no Session open yet.
    fn detach(&mut self) {
        self.end_subscription();
        self.abort_subscribing();
    }

    /// The reader left the Session they were on — for the Landing, another
    /// Workspace, or another Outlook. Nothing that was being loaded for them
    /// is still an answer to where they are, so the attachment goes with the
    /// subscription.
    fn leave_session(&mut self) {
        self.detach();
        self.attachment.abandon();
    }

    fn abort_subscribing(&mut self) {
        if let Some((_, task)) = self.subscribing.take() {
            task.abort();
        }
    }

    fn reset_skill_listing(&mut self) {
        if let Some((_, task)) = self.listing_skills.take() {
            task.abort();
        }
    }

    fn reconcile_catalog_origins(
        &mut self,
        client: &ManagedClient,
        wanted: HashSet<Outlook>,
        events: &UnboundedSender<OriginCatalogEvent>,
    ) {
        self.catalog_origins.retain(|outlook, task| {
            let keep = wanted.contains(outlook);
            if !keep {
                task.abort();
            }
            keep
        });
        for outlook in wanted {
            self.catalog_origins
                .entry(outlook.clone())
                .or_insert_with(|| {
                    spawn_origin_catalog_forwarder(
                        client
                            .session_commands_for(outlook.clone())
                            .subscribe_catalog(),
                        outlook,
                        events.clone(),
                    )
                });
        }
    }

    /// Replaces one Remote's recovering subscription so a reader's explicit
    /// retry does not wait for the current backoff delay.
    fn retry_catalog_origin(
        &mut self,
        client: &ManagedClient,
        outlook: Outlook,
        events: &UnboundedSender<OriginCatalogEvent>,
    ) {
        if let Some(task) = self.catalog_origins.remove(&outlook) {
            task.abort();
        }
        let task = spawn_origin_catalog_forwarder(
            client
                .session_commands_for(outlook.clone())
                .subscribe_catalog(),
            outlook.clone(),
            events.clone(),
        );
        self.catalog_origins.insert(outlook, task);
    }

    fn resolve_workspace(
        &mut self,
        commands: SessionCommandClient,
        outlook: Outlook,
        surface: WorkspaceResolutionSurface,
        request_id: u64,
        request: ResolveWorkspaceRequest,
        results: &UnboundedSender<WorkspaceResolutionResult>,
    ) {
        if surface != WorkspaceResolutionSurface::Outlook {
            self.cancel_workspace_resolution(WorkspaceResolutionSurface::Outlook);
        }
        let task = spawn_workspace_resolution(
            commands,
            outlook,
            surface,
            request_id,
            request,
            results.clone(),
        );
        if let Some((_, superseded)) = self
            .resolving_workspaces
            .insert(surface, (request_id, task))
        {
            superseded.abort();
        }
    }

    fn finish_workspace_resolution(
        &mut self,
        surface: WorkspaceResolutionSurface,
        request_id: u64,
    ) {
        if self
            .resolving_workspaces
            .get(&surface)
            .is_some_and(|(active, _)| *active == request_id)
        {
            self.resolving_workspaces.remove(&surface);
        }
    }

    fn cancel_workspace_resolution(&mut self, surface: WorkspaceResolutionSurface) {
        if let Some((_, task)) = self.resolving_workspaces.remove(&surface) {
            task.abort();
        }
    }

    fn reset_workspace_resolutions(&mut self) {
        for (_, (_, task)) in self.resolving_workspaces.drain() {
            task.abort();
        }
    }

    /// Takes over a subscription a spawned task established.
    fn adopt(&mut self, subscription: SessionSubscription) {
        self.subscription = Some(subscription);
    }

    /// Subscribes to `session_id`, abandoning any subscription attempt already
    /// in flight for an earlier Session.
    fn resubscribe(
        &mut self,
        commands: SessionCommandClient,
        reference: SessionReference,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        self.abort_subscribing();
        self.spawn_subscribe(commands, reference, connected);
    }

    /// Subscribes to `session_id` only when no attempt is already in flight, so
    /// stream recovery never restarts a connection that is still retrying.
    fn subscribe_if_idle(
        &mut self,
        commands: SessionCommandClient,
        reference: SessionReference,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        if self.subscribing.is_none() {
            self.spawn_subscribe(commands, reference, connected);
        }
    }

    fn spawn_subscribe(
        &mut self,
        commands: SessionCommandClient,
        reference: SessionReference,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        self.subscribing = Some((
            reference.clone(),
            spawn_session_subscription(commands, reference, connected.clone()),
        ));
    }

    /// Forgets the subscription attempt for `session_id` now that it connected.
    fn finish_subscribing(&mut self, reference: &SessionReference) {
        if self
            .subscribing
            .as_ref()
            .is_some_and(|(subscribing, _)| subscribing == reference)
        {
            self.subscribing = None;
        }
    }

    /// Attaches to `reference`, superseding whatever attachment was already in
    /// flight: the Session the reader just chose is the one they are waiting
    /// on. The live subscription is left alone until the target hydrates, so
    /// the Session on screen keeps its stream throughout.
    fn attach(
        &mut self,
        commands: SessionCommandClient,
        reference: SessionReference,
        results: &UnboundedSender<SessionPickerResult>,
    ) {
        let results = results.clone();
        self.attachment.begin(reference, |target, operation| {
            spawn_session_attachment(commands, target, operation, results)
        });
    }

    /// Whether a finished attachment is still the one the reader is waiting
    /// on, forgetting it when it is.
    fn settle_attachment(&mut self, operation: AttachmentOperationId) -> AttachmentOutcome {
        self.attachment.settle(operation)
    }

    fn finish_listing_sessions(&mut self, request: &SessionListRequest) {
        let key = (request.surface(), request.outlook().clone());
        if self
            .listing_sessions
            .get(&key)
            .is_some_and(|(active, _)| active == request)
        {
            self.listing_sessions.remove(&key);
        }
    }

    fn finish_listing_models(&mut self, request: &ModelListRequest) {
        finish_listing(&mut self.listing_models, request);
    }

    fn list_sessions(
        &mut self,
        commands: SessionCommandClient,
        request: SessionListRequest,
        results: &UnboundedSender<SessionPickerResult>,
    ) {
        let results = results.clone();
        let key = (request.surface(), request.outlook().clone());
        if let Some((_, superseded)) = self.listing_sessions.remove(&key) {
            superseded.abort();
        }
        let task = spawn_session_listing(commands, request.clone(), results);
        self.listing_sessions.insert(key, (request, task));
    }

    fn list_models(
        &mut self,
        commands: SessionCommandClient,
        request: ModelListRequest,
        results: &UnboundedSender<ModelPickerResult>,
    ) {
        let results = results.clone();
        replace_listing(&mut self.listing_models, request, |request| {
            spawn_model_listing(commands, request, results)
        });
    }

    fn list_skills_if_needed(
        &mut self,
        commands: SessionCommandClient,
        outlook: Outlook,
        request: SkillCatalogRequest,
        results: &UnboundedSender<SkillCatalogResult>,
    ) {
        let qualified_request = (outlook, request);
        if self
            .listing_skills
            .as_ref()
            .is_some_and(|(active, _)| active == &qualified_request)
        {
            return;
        }
        let results = results.clone();
        replace_listing(
            &mut self.listing_skills,
            qualified_request,
            |(outlook, request)| {
                spawn_skill_catalog_operation(
                    commands,
                    outlook,
                    request,
                    results,
                    SkillCatalogOperation::List,
                )
            },
        );
    }

    fn refresh_skills(
        &mut self,
        commands: SessionCommandClient,
        outlook: Outlook,
        request: SkillCatalogRequest,
        results: &UnboundedSender<SkillCatalogResult>,
    ) {
        let results = results.clone();
        replace_listing(
            &mut self.listing_skills,
            (outlook, request),
            |(outlook, request)| {
                spawn_skill_catalog_operation(
                    commands,
                    outlook,
                    request,
                    results,
                    SkillCatalogOperation::Refresh,
                )
            },
        );
    }
}

/// The channels the run loop's spawned tasks report their results back on.
struct TaskChannels {
    submissions: UnboundedSender<SubmissionResult>,
    subscriptions: UnboundedSender<ConnectedSessionSubscription>,
    pickers: UnboundedSender<SessionPickerResult>,
    models: UnboundedSender<ModelPickerResult>,
    skills: UnboundedSender<SkillCatalogResult>,
    pairing: UnboundedSender<PairingResult>,
    workspaces: UnboundedSender<WorkspaceResolutionResult>,
    origin_catalog: UnboundedSender<OriginCatalogEvent>,
}

/// The run loop's mutable world: the Application it feeds, the client it sends
/// Session commands through, the Session work it owns, and the channels its
/// spawned tasks report back on.
struct RunLoop {
    client: ManagedClient,
    application: Application,
    tasks: SessionTasks,
    channels: TaskChannels,
    reconnect_grace: Option<Pin<Box<tokio::time::Sleep>>>,
    /// The optimistic shell's one-shot quiet-period wakeup, paired with its
    /// absolute deadline so a superseding route can replace it exactly once.
    opening_loading_delay: Option<(Instant, Pin<Box<tokio::time::Sleep>>)>,
    /// Armed only while something on screen animates, so an idle TUI schedules
    /// zero wakeups (ADR 0009). Re-armed on every fire.
    spinner_tick: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Set by anything that changes what is on screen, so an event the user
    /// cannot see costs no frame.
    needs_redraw: bool,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<termina::PlatformTerminal>>,
    client: ManagedClient,
    workspace: PathBuf,
    terminal_facts: TerminalFacts,
    mut input: TerminalEvents,
) -> Result<()> {
    let config_root = client.config_dir().map(PathBuf::from);
    let (submissions, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscriptions, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pickers, mut picker_rx) = tokio::sync::mpsc::unbounded_channel();
    let (models, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
    let (skills, mut skill_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pairing, mut pairing_rx) = tokio::sync::mpsc::unbounded_channel();
    let (workspaces, mut workspace_rx) = tokio::sync::mpsc::unbounded_channel();
    let (origin_catalog, mut origin_catalog_rx) = tokio::sync::mpsc::unbounded_channel();
    let application = Application::new(workspace, terminal_facts);
    let application = match config_root {
        Some(config_root) => application.with_config_root(config_root),
        None => application,
    };
    let mut run = RunLoop {
        client,
        application,
        tasks: SessionTasks::default(),
        channels: TaskChannels {
            submissions,
            subscriptions,
            pickers,
            models,
            skills,
            pairing,
            workspaces,
            origin_catalog,
        },
        reconnect_grace: None,
        opening_loading_delay: None,
        spinner_tick: None,
        needs_redraw: true,
    };
    let mut clipboard = native_clipboard();
    loop {
        run.sync_skill_catalog();
        if run.needs_redraw && run.application.first_frame_ready() {
            terminal.draw(|frame| run.application.render(frame))?;
            run.needs_redraw = false;
        }
        // Rendering records which animation is actually visible, including a
        // Working Indicator that may have scrolled out of the viewport.
        run.sync_opening_loading_delay();
        run.sync_spinner_tick();
        // Every arm reports through ControlFlow so the two events that can end
        // the run -- a Provider shutdown and the exit command -- leave by the
        // same path as the input stream closing.
        let step = tokio::select! {
            managed_event = run.client.next() => run.receive_managed_event(managed_event)?,
            () = wait_for_reconnect_grace(&mut run.reconnect_grace) => {
                run.expire_reconnect_grace()?
            }
            () = wait_for_opening_loading_delay(&mut run.opening_loading_delay) => {
                run.reveal_opening_loading()
            }
            () = wait_for_spinner_tick(&mut run.spinner_tick) => run.advance_spinner(),
            session_event = next_session_event(&mut run.tasks.subscription) => {
                run.receive_session_event(session_event)?
            }
            connected = subscription_rx.recv() => run.receive_subscription(connected)?,
            submission = submission_rx.recv() => run.receive_submission(submission)?,
            model = model_rx.recv() => run.receive_model_listing(model)?,
            skill = skill_rx.recv() => run.receive_skill_listing(skill)?,
            picker = picker_rx.recv() => run.receive_session_picker(picker)?,
            pairing = pairing_rx.recv() => run.receive_pairing_result(pairing)?,
            workspace = workspace_rx.recv() => run.receive_workspace_result(workspace)?,
            catalog = origin_catalog_rx.recv() => run.receive_origin_catalog(catalog)?,
            input_event = input.next() => match input_event {
                Some(Ok(event)) => run.handle_terminal_input(event, terminal.backend_mut(), &mut clipboard)?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            },
        };
        if let ControlFlow::Break(exit) = step {
            return leave_run_loop(terminal, &run.application, exit);
        }

        // Coalesce input that is already pending into this frame so a burst of
        // events (wheel scrolling, key auto-repeat) costs one redraw instead of
        // one per event. Bounded so a continuous flood cannot starve rendering.
        //
        // The stream must be polled with the run loop's own task context: a
        // detached poll (`now_or_never`) would hand the stream a no-op waker,
        // and a Pending poll would then leave nothing to wake this task when
        // the next event arrives, deadlocking all input.
        for _ in 0..128 {
            let pending_input = std::future::poll_fn(|context| {
                std::task::Poll::Ready(match input.poll_next_unpin(context) {
                    std::task::Poll::Ready(event) => Some(event),
                    std::task::Poll::Pending => None,
                })
            })
            .await;
            let Some(pending_input) = pending_input else {
                break;
            };
            let step = match pending_input {
                Some(Ok(event)) => {
                    run.handle_terminal_input(event, terminal.backend_mut(), &mut clipboard)?
                }
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            };
            if let ControlFlow::Break(exit) = step {
                return leave_run_loop(terminal, &run.application, exit);
            }
        }
    }
}

fn leave_run_loop(
    terminal: &mut Terminal<CrosstermBackend<termina::PlatformTerminal>>,
    application: &Application,
    exit: Exit,
) -> Result<()> {
    if exit == Exit::AfterFinalFrame {
        terminal.draw(|frame| application.render(frame))?;
    }
    Ok(())
}

impl RunLoop {
    fn handle_terminal_input(
        &mut self,
        input: TerminalInput,
        output: &mut impl std::io::Write,
        clipboard: &mut impl NativeClipboardSink,
    ) -> Result<ControlFlow<Exit>> {
        match input {
            TerminalInput::Event(event) => {
                self.handle_input_event(event, &mut TerminalOutput(output), clipboard)
            }
            TerminalInput::Colors(update) => {
                let mut facts = self.application.terminal_facts;
                facts.merge_probe(update);
                if facts != self.application.terminal_facts {
                    self.application.set_terminal_facts(facts);
                    self.needs_redraw = true;
                }
                Ok(ControlFlow::Continue(()))
            }
            TerminalInput::Reprobe => {
                request_terminal_colors(output)?;
                Ok(ControlFlow::Continue(()))
            }
        }
    }

    fn sync_skill_catalog(&mut self) {
        let Some(request) = self.application.skill_catalog_request() else {
            return;
        };
        if self.application.has_skill_catalog_for(&request) {
            return;
        }
        self.tasks.list_skills_if_needed(
            self.outlook_commands(),
            self.application.outlook().clone(),
            request,
            &self.channels.skills,
        );
    }

    fn handle_input_event(
        &mut self,
        event: InputEvent,
        output: &mut impl TerminalSink,
        clipboard: &mut impl NativeClipboardSink,
    ) -> Result<ControlFlow<Exit>> {
        if matches!(event, InputEvent::Resize(..)) {
            self.needs_redraw = true;
        }
        if self.application.note_interaction(&event) {
            self.needs_redraw = true;
        }
        let Some(command) = self.application.command_for_terminal_input(event) else {
            return Ok(ControlFlow::Continue(()));
        };
        self.needs_redraw = true;
        let transition = self
            .application
            .handle_event(ApplicationEvent::Command(command))?;
        if let ApplicationTransition::CopyToClipboard(text) = transition {
            copy_to_clipboard(output, clipboard, &text);
            return Ok(ControlFlow::Continue(()));
        }
        Ok(self.dispatch_transition(transition))
    }

    /// Carries out the transition a terminal command produced, spawning
    /// whatever Session command it asked for.
    fn dispatch_transition(&mut self, transition: ApplicationTransition) -> ControlFlow<Exit> {
        match transition {
            ApplicationTransition::Continue => {}
            ApplicationTransition::Exit => return ControlFlow::Break(Exit::Now),
            ApplicationTransition::SessionEnded => self.tasks.end_subscription(),
            ApplicationTransition::DetachSession => self.tasks.leave_session(),
            ApplicationTransition::CreateSession(request) => {
                let outlook = self.application.outlook().clone();
                spawn_session_creation(
                    self.client.session_commands_for(outlook.clone()),
                    outlook,
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::AdmitPrompt { session, request } => {
                let commands = self.client.session_commands_for(session.origin.clone());
                spawn_prompt_admission(
                    commands,
                    session,
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::PromotePrompt { session, prompt_id } => self.spawn_operation(
                session.clone(),
                SessionOperation::PromotePrompt {
                    session_id: session.session_id,
                    prompt_id,
                },
            ),
            ApplicationTransition::CancelPrompt { session, prompt_id } => self.spawn_operation(
                session.clone(),
                SessionOperation::CancelPrompt {
                    session_id: session.session_id,
                    prompt_id,
                },
            ),
            ApplicationTransition::SubmitQuestionnaire {
                session,
                id,
                submission,
            } => {
                let session_id = session.session_id;
                self.spawn_operation(
                    session,
                    SessionOperation::SubmitQuestionnaire {
                        session_id,
                        id,
                        submission,
                    },
                );
            }
            ApplicationTransition::InterruptSession { session } => {
                let session_id = session.session_id;
                self.spawn_operation(session, SessionOperation::InterruptSession { session_id });
            }
            ApplicationTransition::DeleteSession(session) => {
                let session_id = session.session_id;
                self.spawn_operation(session, SessionOperation::DeleteSession { session_id });
            }
            ApplicationTransition::SettleSession { session, settled } => {
                let session_id = session.session_id;
                self.spawn_operation(
                    session,
                    SessionOperation::SettleSession {
                        session_id,
                        settled,
                    },
                );
            }
            ApplicationTransition::SubscribeSession(_) => {
                unreachable!("terminal input cannot end a Session subscription")
            }
            ApplicationTransition::ViewSession(reference) => {
                spawn_session_view(
                    self.client.session_commands_for(reference.origin.clone()),
                    reference,
                );
            }
            ApplicationTransition::ViewAndAttachSession(reference) => {
                self.view_and_attach(reference);
            }
            ApplicationTransition::AttachSession(reference) => {
                let commands = self.client.session_commands_for(reference.origin.clone());
                self.tasks
                    .attach(commands, reference, &self.channels.pickers);
            }
            ApplicationTransition::ListSessions(request) => self.list_sessions(request),
            ApplicationTransition::ReconcileCatalogOrigins {
                catalog_origins,
                requests,
            } => self.reconcile_catalog_origins(catalog_origins, requests),
            ApplicationTransition::RetryCatalogOrigin(request) => {
                let outlook = request.outlook().clone();
                self.tasks.retry_catalog_origin(
                    &self.client,
                    outlook,
                    &self.channels.origin_catalog,
                );
                self.list_sessions(request);
            }
            ApplicationTransition::ListModels(request) => {
                let commands = self.client.session_commands_for(request.outlook().clone());
                self.tasks
                    .list_models(commands, request, &self.channels.models);
            }
            ApplicationTransition::RefreshSkills(request) => {
                let outlook = self.application.outlook().clone();
                self.tasks.refresh_skills(
                    self.client.session_commands_for(outlook.clone()),
                    outlook,
                    request,
                    &self.channels.skills,
                );
            }
            ApplicationTransition::ConfirmLandingAgentSelection(selection) => {
                let outlook = self.application.outlook().clone();
                spawn_landing_agent_selection_confirmation(
                    self.client.session_commands_for(outlook.clone()),
                    outlook,
                    selection,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::UpdateAgentSelection { session, request } => {
                let commands = self.client.session_commands_for(session.origin.clone());
                spawn_agent_selection_update(
                    commands,
                    session,
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::MutateSetting(mutation) => {
                spawn_setting_mutation(
                    self.client.session_commands(),
                    mutation,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::BeginServing { enable, port } => {
                spawn_serving_preparation(
                    self.client.session_commands(),
                    enable,
                    port,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::IssueInvite(request) => {
                spawn_invite_issuance(
                    self.client.session_commands(),
                    request,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::RemovePeer(peer_id) => {
                spawn_peer_removal(
                    self.client.session_commands(),
                    peer_id,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::BeginConnecting => {
                spawn_remote_listing(
                    self.client.session_commands(),
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::ListEverywhereRemotes(request_id) => {
                spawn_everywhere_remote_listing(
                    self.client.session_commands(),
                    request_id,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::PreviewInvite(invite) => {
                spawn_invite_preview(
                    self.client.session_commands(),
                    invite,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::RedeemInvite(request) => {
                spawn_invite_redemption(
                    self.client.session_commands(),
                    request,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::TurnOutlook {
                outlook,
                catalog_origins,
            } => {
                self.prepare_outlook_turn(catalog_origins);
                self.dispatch_pending_listing_after_turn();
                if matches!(outlook, Outlook::Remote(_)) {
                    self.tasks.resolve_workspace(
                        self.client.session_commands_for(outlook.clone()),
                        outlook,
                        WorkspaceResolutionSurface::Outlook,
                        self.application
                            .pending_workspace_resolution(WorkspaceResolutionSurface::Outlook)
                            .expect("turning toward a remote begins Workspace resolution"),
                        ResolveWorkspaceRequest {
                            base: None,
                            path: PathBuf::from("."),
                        },
                        &self.channels.workspaces,
                    );
                }
            }
            ApplicationTransition::TurnOutlookAndViewAndAttach {
                session,
                catalog_origins,
            } => {
                self.prepare_outlook_turn(catalog_origins);
                self.dispatch_pending_listing_after_turn();
                self.view_and_attach(session);
            }
            ApplicationTransition::ResolveWorkspace {
                outlook,
                surface,
                request_id,
                request,
            } => {
                self.tasks.resolve_workspace(
                    self.client.session_commands_for(outlook.clone()),
                    outlook,
                    surface,
                    request_id,
                    request,
                    &self.channels.workspaces,
                );
            }
            ApplicationTransition::CancelWorkspaceResolution(surface) => {
                self.tasks.cancel_workspace_resolution(surface);
            }
            ApplicationTransition::CopyToClipboard(_) => {
                unreachable!("clipboard output is handled before task dispatch")
            }
        }
        ControlFlow::Continue(())
    }

    fn spawn_operation(&self, session: SessionReference, operation: SessionOperation) {
        spawn_session_operation(
            self.client.session_commands_for(session.origin.clone()),
            session,
            operation,
            self.channels.submissions.clone(),
        );
    }

    fn prepare_outlook_turn(&mut self, catalog_origins: HashSet<Outlook>) {
        self.tasks.leave_session();
        self.tasks.reset_skill_listing();
        self.tasks.reconcile_catalog_origins(
            &self.client,
            catalog_origins,
            &self.channels.origin_catalog,
        );
        self.tasks.reset_workspace_resolutions();
    }

    fn dispatch_pending_listing_after_turn(&mut self) {
        match self.application.take_session_listing_transition() {
            ApplicationTransition::ListSessions(request) => self.list_sessions(request),
            ApplicationTransition::ReconcileCatalogOrigins {
                catalog_origins,
                requests,
            } => self.reconcile_catalog_origins(catalog_origins, requests),
            ApplicationTransition::ListEverywhereRemotes(request) => {
                spawn_everywhere_remote_listing(
                    self.client.session_commands(),
                    request,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::Continue => {}
            other => unreachable!(
                "a Sidebar pending after an Outlook turn only lists Sessions: {other:?}"
            ),
        }
    }

    fn list_sessions(&mut self, request: SessionListRequest) {
        let commands = self.client.session_commands_for(request.outlook().clone());
        self.tasks
            .list_sessions(commands, request, &self.channels.pickers);
    }

    fn view_and_attach(&mut self, reference: SessionReference) {
        let commands = self.client.session_commands_for(reference.origin.clone());
        spawn_session_view(commands.clone(), reference.clone());
        self.tasks
            .attach(commands, reference, &self.channels.pickers);
    }

    fn reconcile_catalog_origins(
        &mut self,
        catalog_origins: HashSet<Outlook>,
        requests: Vec<SessionListRequest>,
    ) {
        self.tasks.reconcile_catalog_origins(
            &self.client,
            catalog_origins,
            &self.channels.origin_catalog,
        );
        for request in requests {
            self.list_sessions(request);
        }
    }

    fn outlook_commands(&self) -> SessionCommandClient {
        self.client
            .session_commands_for(self.application.outlook().clone())
    }

    fn receive_managed_event(&mut self, event: Option<ManagedEvent>) -> Result<ControlFlow<Exit>> {
        let event = event.ok_or_else(|| anyhow!("managed client stopped unexpectedly"))?;
        if event.is_drawn_on_arrival() {
            self.needs_redraw = true;
        }
        if matches!(&event, ManagedEvent::Connecting) {
            self.tasks.reset_skill_listing();
        }
        let was_recovering = self.application.is_recovering();
        let transition = self
            .application
            .handle_event(ApplicationEvent::Managed(event))?;
        if self.application.is_recovering() {
            if !was_recovering {
                self.reconnect_grace = Some(Box::pin(tokio::time::sleep(RECONNECT_GRACE_PERIOD)));
            }
        } else {
            self.reconnect_grace = None;
        }
        match transition {
            ApplicationTransition::Continue => {}
            ApplicationTransition::SessionEnded => self.tasks.end_subscription(),
            // The shutdown state is worth one last frame before the screen goes.
            ApplicationTransition::Exit => return Ok(ControlFlow::Break(Exit::AfterFinalFrame)),
            // The one command a managed event issues: the effective-settings
            // snapshot decides whether the Sidebar opens, and a Sidebar that
            // opens wants the Sessions it lists.
            ApplicationTransition::ListSessions(request) => self.list_sessions(request),
            ApplicationTransition::ReconcileCatalogOrigins {
                catalog_origins,
                requests,
            } => self.reconcile_catalog_origins(catalog_origins, requests),
            ApplicationTransition::RetryCatalogOrigin(_) => {
                unreachable!("managed events cannot explicitly retry a Remote")
            }
            ApplicationTransition::ListEverywhereRemotes(request_id) => {
                spawn_everywhere_remote_listing(
                    self.client.session_commands(),
                    request_id,
                    self.channels.pairing.clone(),
                );
            }
            ApplicationTransition::CreateSession(_)
            | ApplicationTransition::DetachSession
            | ApplicationTransition::DeleteSession(_)
            | ApplicationTransition::SettleSession { .. }
            | ApplicationTransition::AdmitPrompt { .. }
            | ApplicationTransition::PromotePrompt { .. }
            | ApplicationTransition::CancelPrompt { .. }
            | ApplicationTransition::SubmitQuestionnaire { .. }
            | ApplicationTransition::InterruptSession { .. }
            | ApplicationTransition::SubscribeSession(_)
            | ApplicationTransition::ViewSession(_)
            | ApplicationTransition::ViewAndAttachSession(_)
            | ApplicationTransition::AttachSession(_)
            | ApplicationTransition::ListModels(_)
            | ApplicationTransition::RefreshSkills(_)
            | ApplicationTransition::ConfirmLandingAgentSelection(_)
            | ApplicationTransition::UpdateAgentSelection { .. }
            | ApplicationTransition::MutateSetting(_)
            | ApplicationTransition::BeginServing { .. }
            | ApplicationTransition::IssueInvite(_)
            | ApplicationTransition::CopyToClipboard(_)
            | ApplicationTransition::RemovePeer(_)
            | ApplicationTransition::BeginConnecting
            | ApplicationTransition::PreviewInvite(_)
            | ApplicationTransition::RedeemInvite(_)
            | ApplicationTransition::TurnOutlook { .. }
            | ApplicationTransition::TurnOutlookAndViewAndAttach { .. }
            | ApplicationTransition::ResolveWorkspace { .. }
            | ApplicationTransition::CancelWorkspaceResolution(_) => {
                unreachable!("managed events issue no other Session command");
            }
        }
        // The transition carries at most one Session command, and the Sidebar
        // can ask to catch up on an event that already produced another one.
        let pending = self.application.take_session_listing_transition();
        if pending != ApplicationTransition::Continue {
            let _ = self.dispatch_transition(pending);
        }
        if self.application.session_id().is_none() {
            self.tasks.detach();
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Keeps exactly one wakeup at the optimistic route's loading threshold.
    /// Ordinary events do not move the deadline, while success, cancellation,
    /// and superseding navigation respectively drop or replace it.
    fn sync_opening_loading_delay(&mut self) {
        let wanted = self.application.opening_loading_deadline();
        let armed = self
            .opening_loading_delay
            .as_ref()
            .map(|(deadline, _)| *deadline);
        if armed == wanted {
            return;
        }
        self.opening_loading_delay = wanted.map(|deadline| {
            (
                deadline,
                Box::pin(tokio::time::sleep_until(deadline.into())),
            )
        });
    }

    fn reveal_opening_loading(&mut self) -> ControlFlow<Exit> {
        self.opening_loading_delay = None;
        self.needs_redraw = true;
        let transition = self
            .application
            .handle_event(ApplicationEvent::OpeningLoadingDelayElapsed)
            .expect("the opening loading delay is presentation-only");
        debug_assert_eq!(transition, ApplicationTransition::Continue);
        ControlFlow::Continue(())
    }

    /// Arms the presentation tick while anything on screen animates and drops it
    /// the moment nothing does, keeping the run loop idle-by-default. Called
    /// once per loop iteration, so every event that starts or settles work
    /// re-decides the tick before the frame it changed draws.
    fn sync_spinner_tick(&mut self) {
        if self.application.wants_spinner() {
            if self.spinner_tick.is_none() {
                self.spinner_tick = Some(Box::pin(tokio::time::sleep(shimmer::TICK_PERIOD)));
            }
        } else {
            self.spinner_tick = None;
        }
    }

    fn advance_spinner(&mut self) -> ControlFlow<Exit> {
        self.needs_redraw = true;
        let transition = self
            .application
            .handle_event(ApplicationEvent::SpinnerTick)
            .expect("a presentation tick is infallible");
        debug_assert_eq!(transition, ApplicationTransition::Continue);
        self.spinner_tick = Some(Box::pin(tokio::time::sleep(shimmer::TICK_PERIOD)));
        ControlFlow::Continue(())
    }

    fn expire_reconnect_grace(&mut self) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        self.application
            .handle_event(ApplicationEvent::ReconnectGraceElapsed)?;
        self.reconnect_grace = None;
        Ok(ControlFlow::Continue(()))
    }

    fn receive_session_event(
        &mut self,
        event: Option<std::result::Result<SessionEvent, SessionStreamError>>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        match event {
            Some(Ok(event)) => {
                let transition = self
                    .application
                    .handle_event(ApplicationEvent::Session(event))?;
                match transition {
                    ApplicationTransition::Continue => {}
                    ApplicationTransition::ViewSession(_) => {
                        let _ = self.dispatch_transition(transition);
                    }
                    other => unreachable!(
                        "an open Session event only continues or reports Viewed: {other:?}"
                    ),
                }
            }
            Some(Err(error)) => {
                if let Some(event) =
                    remote_failure_from_session_error(self.application.outlook().clone(), &error)
                {
                    return self.receive_origin_catalog(Some(event));
                }
                if !error.is_recoverable() {
                    return Err(error.into());
                }
                self.recover_session_subscription()?;
            }
            None => self.recover_session_subscription()?,
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Re-establishes the Session subscription after the stream dropped,
    /// leaving any attempt already in flight to finish rather than restarting.
    fn recover_session_subscription(&mut self) -> Result<()> {
        self.tasks.end_subscription();
        let transition = self
            .application
            .handle_event(ApplicationEvent::SessionSubscriptionEnded)?;
        if let ApplicationTransition::SubscribeSession(session_id) = transition {
            self.tasks.subscribe_if_idle(
                self.client.session_commands_for(session_id.origin.clone()),
                session_id,
                &self.channels.subscriptions,
            );
        }
        Ok(())
    }

    /// Adopts a subscription a spawned task established. Nothing on screen
    /// changes, so this is the one event that does not ask for a redraw.
    fn receive_subscription(
        &mut self,
        connected: Option<ConnectedSessionSubscription>,
    ) -> Result<ControlFlow<Exit>> {
        let connected = connected
            .ok_or_else(|| anyhow!("Session subscription task channel stopped unexpectedly"))?;
        self.tasks.finish_subscribing(&connected.reference);
        if self.application.session_reference().as_ref() == Some(&connected.reference) {
            self.tasks.adopt(connected.subscription);
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_submission(
        &mut self,
        submission: Option<SubmissionResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let submission = submission
            .ok_or_else(|| anyhow!("Prompt admission task channel stopped unexpectedly"))?;
        match submission {
            SubmissionResult::SessionCreated { outlook, snapshot } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let session_id = snapshot.session.id;
                self.application
                    .handle_event(ApplicationEvent::SessionCreated(*snapshot))?;
                self.tasks.resubscribe(
                    self.client.session_commands_for(outlook.clone()),
                    SessionReference::new(outlook, session_id),
                    &self.channels.subscriptions,
                );
            }
            SubmissionResult::PromptAdmitted { session, prompt_id } => {
                self.application
                    .handle_event(ApplicationEvent::PromptAdmissionSucceeded {
                        session,
                        prompt_id,
                    })?;
            }
            SubmissionResult::PromptDeliveryFailed {
                outlook,
                session,
                prompt_id,
                error,
            } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let event = match session {
                    Some(session) => ApplicationEvent::PromptAdmissionFailed {
                        session,
                        prompt_id,
                        error,
                    },
                    None => ApplicationEvent::SessionCreationFailed { prompt_id, error },
                };
                self.application.handle_event(event)?;
            }
            SubmissionResult::QuestionnaireReconciled {
                session,
                id,
                snapshot,
                error,
            } => {
                self.application.handle_event(
                    ApplicationEvent::QuestionnaireSubmissionReconciled {
                        session,
                        id,
                        snapshot,
                        error,
                    },
                )?;
            }
            SubmissionResult::OperationSucceeded(session) => {
                if self.application.outlook() != &session.origin {
                    return Ok(ControlFlow::Continue(()));
                }
            }
            SubmissionResult::SessionSettled(session) => {
                if self.application.session_reference().as_ref() == Some(&session) {
                    let transition = self.application.handle_event(ApplicationEvent::Command(
                        CommandId::InvokeSemantic(SemanticCommandId::SessionNew),
                    ))?;
                    return Ok(self.dispatch_transition(transition));
                }
            }
            SubmissionResult::OperationFailed { session, error } => {
                if self.application.outlook() != &session.origin {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::SessionOperationFailed(error))?;
            }
            SubmissionResult::SessionDeletionFailed { session, error } => {
                if self.application.outlook() != &session.origin {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::SessionDeletionFailed {
                        reference: session,
                        error,
                    })?;
            }
            SubmissionResult::LandingAgentSelectionConfirmed { outlook, selection } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let transition = self
                    .application
                    .handle_event(ApplicationEvent::LandingAgentSelectionConfirmed(selection))?;
                self.flush_landing_agent_selection(transition);
            }
            SubmissionResult::LandingAgentSelectionConfirmationFailed { outlook, error } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let transition = self.application.handle_event(
                    ApplicationEvent::LandingAgentSelectionConfirmationFailed(error),
                )?;
                self.flush_landing_agent_selection(transition);
            }
            SubmissionResult::AgentSelectionUpdated {
                session,
                operation_id,
                selection,
            } => {
                if self.application.session_reference().as_ref() != Some(&session) {
                    return Ok(ControlFlow::Continue(()));
                }
                let transition =
                    self.application
                        .handle_event(ApplicationEvent::AgentSelectionUpdated {
                            operation_id,
                            selection,
                        })?;
                self.flush_agent_selection(transition);
            }
            SubmissionResult::AgentSelectionUpdateFailed {
                session,
                operation_id,
                error,
            } => {
                if self.application.session_reference().as_ref() != Some(&session) {
                    return Ok(ControlFlow::Continue(()));
                }
                let transition = self.application.handle_event(
                    ApplicationEvent::AgentSelectionUpdateFailed {
                        operation_id,
                        error,
                    },
                )?;
                self.flush_agent_selection(transition);
            }
            SubmissionResult::SettingMutated(snapshot) => {
                self.application
                    .handle_event(ApplicationEvent::SettingMutated(*snapshot))?;
            }
            SubmissionResult::SettingMutationFailed(error) => {
                self.application
                    .handle_event(ApplicationEvent::SettingMutationFailed(error))?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Dispatches the follow-up request when settling one Agent Selection
    /// operation released a coalesced newer selection.
    fn flush_agent_selection(&self, transition: ApplicationTransition) {
        if let ApplicationTransition::UpdateAgentSelection { session, request } = transition {
            spawn_agent_selection_update(
                self.client.session_commands_for(session.origin.clone()),
                session,
                request,
                self.channels.submissions.clone(),
            );
        }
    }

    fn flush_landing_agent_selection(&self, transition: ApplicationTransition) {
        if let ApplicationTransition::ConfirmLandingAgentSelection(selection) = transition {
            let outlook = self.application.outlook().clone();
            spawn_landing_agent_selection_confirmation(
                self.client.session_commands_for(outlook.clone()),
                outlook,
                selection,
                self.channels.submissions.clone(),
            );
        }
    }

    fn receive_model_listing(
        &mut self,
        result: Option<ModelPickerResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let result =
            result.ok_or_else(|| anyhow!("Model picker task channel stopped unexpectedly"))?;
        let request = match &result {
            ModelPickerResult::Listed { request, .. }
            | ModelPickerResult::Refreshed { request, .. }
            | ModelPickerResult::Failed { request, .. } => request,
        };
        if request.outlook() != self.application.outlook() {
            self.tasks.finish_listing_models(request);
            return Ok(ControlFlow::Continue(()));
        }
        match result {
            ModelPickerResult::Listed { request, catalog } => {
                self.application
                    .handle_event(ApplicationEvent::ModelsListed { request, catalog })?;
            }
            ModelPickerResult::Refreshed { request, catalog } => {
                self.tasks.finish_listing_models(&request);
                self.application
                    .handle_event(ApplicationEvent::ModelsRefreshed { request, catalog })?;
            }
            ModelPickerResult::Failed { request, error } => {
                self.tasks.finish_listing_models(&request);
                self.application
                    .handle_event(ApplicationEvent::ModelListingFailed { request, error })?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_skill_listing(
        &mut self,
        result: Option<SkillCatalogResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let result =
            result.ok_or_else(|| anyhow!("Skill Catalog task channel stopped unexpectedly"))?;
        match result {
            SkillCatalogResult::Listed {
                outlook,
                request,
                catalog,
            } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::SkillsListed { request, catalog })?;
            }
            SkillCatalogResult::Failed {
                outlook,
                request,
                error,
            } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::SkillListingFailed { request, error })?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_session_picker(
        &mut self,
        result: Option<SessionPickerResult>,
    ) -> Result<ControlFlow<Exit>> {
        let result =
            result.ok_or_else(|| anyhow!("Session picker task channel stopped unexpectedly"))?;
        // An attachment the reader moved on from is dropped whole, before
        // anything reads it: its snapshot never becomes the open Session, its
        // subscription goes with it rather than replacing the one still on
        // screen, and its failure is never drawn at them.
        if let SessionPickerResult::Attached { operation, .. }
        | SessionPickerResult::AttachmentFailed { operation, .. } = &result
            && !self.tasks.settle_attachment(*operation).is_current()
        {
            return Ok(ControlFlow::Continue(()));
        }
        // A Sidebar catching up with a change it had already taken in place is
        // answered with the listing it is already drawing, and a straggler
        // lands nowhere at all. Neither is worth a frame to an idle TUI
        // (ADR 0007); everything else the picker tasks report is.
        self.needs_redraw |= match &result {
            SessionPickerResult::Listed { request, sessions } => {
                self.application.listing_moves_the_frame(request, sessions)
            }
            SessionPickerResult::ListingFailed { request, .. } => {
                self.application.awaits_listing(request)
            }
            SessionPickerResult::Attached { .. } | SessionPickerResult::AttachmentFailed { .. } => {
                true
            }
        };
        match result {
            SessionPickerResult::Listed { request, sessions } => {
                self.tasks.finish_listing_sessions(&request);
                self.application
                    .handle_event(ApplicationEvent::SessionsListed { request, sessions })?;
            }
            SessionPickerResult::ListingFailed { request, error } => {
                self.tasks.finish_listing_sessions(&request);
                self.application
                    .handle_event(ApplicationEvent::SessionListingFailed { request, error })?;
            }
            SessionPickerResult::Attached {
                reference,
                snapshot,
                subscription,
                ..
            } => {
                let transition =
                    self.application
                        .handle_event(ApplicationEvent::OriginSessionAttached {
                            reference: reference.clone(),
                            snapshot: *snapshot,
                        })?;
                if self.application.session_reference().as_ref() == Some(&reference) {
                    self.tasks.adopt(subscription);
                    self.tasks.abort_subscribing();
                }
                return Ok(self.dispatch_transition(transition));
            }
            SessionPickerResult::AttachmentFailed {
                reference, error, ..
            } => {
                let transition = self.application.handle_event(
                    ApplicationEvent::OriginSessionAttachmentFailed { reference, error },
                )?;
                return Ok(self.dispatch_transition(transition));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_pairing_result(
        &mut self,
        result: Option<PairingResult>,
    ) -> Result<ControlFlow<Exit>> {
        let result = result.ok_or_else(|| anyhow!("Pairing task channel stopped unexpectedly"))?;
        self.needs_redraw |= match &result {
            PairingResult::EverywhereRemotesListed { request, .. }
            | PairingResult::EverywhereRemoteListingFailed { request, .. } => {
                self.application.accepts_everywhere_remotes(*request)
            }
            _ => true,
        };
        let event = match result {
            PairingResult::Prepared {
                settings,
                candidates,
            } => ApplicationEvent::ServingPrepared {
                settings: settings.map(|settings| *settings),
                candidates,
            },
            PairingResult::PreparationFailed(error) => {
                ApplicationEvent::ServingPreparationFailed(error)
            }
            PairingResult::InviteIssued { invite, peers } => {
                ApplicationEvent::InviteIssued { invite, peers }
            }
            PairingResult::PeerRemoved(peer_id) => ApplicationEvent::PeerRemoved(peer_id),
            PairingResult::RemotesListed(remotes) => ApplicationEvent::RemotesListed(remotes),
            PairingResult::RemoteListingFailed(error) => {
                ApplicationEvent::RemoteListingFailed(error)
            }
            PairingResult::EverywhereRemotesListed { request, remotes } => {
                ApplicationEvent::EverywhereRemotesListed { request, remotes }
            }
            PairingResult::EverywhereRemoteListingFailed { request, error } => {
                ApplicationEvent::EverywhereRemoteListingFailed { request, error }
            }
            PairingResult::InvitePreviewed { invite, preview } => {
                ApplicationEvent::InvitePreviewed { invite, preview }
            }
            PairingResult::InvitePreviewFailed { invite, error } => {
                ApplicationEvent::InvitePreviewFailed { invite, error }
            }
            PairingResult::RemoteRedeemed(remote) => ApplicationEvent::RemoteRedeemed(remote),
            PairingResult::InviteRedemptionFailed(error) => {
                ApplicationEvent::InviteRedemptionFailed(error)
            }
            PairingResult::RemoteProbed { name, result } => {
                ApplicationEvent::RemoteProbed { name, result }
            }
            PairingResult::OperationFailed(error) => {
                ApplicationEvent::ServingOperationFailed(error)
            }
        };
        let transition = self.application.handle_event(event)?;
        Ok(self.dispatch_transition(transition))
    }

    fn receive_workspace_result(
        &mut self,
        result: Option<WorkspaceResolutionResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let result = result
            .ok_or_else(|| anyhow!("Workspace resolution task channel stopped unexpectedly"))?;
        self.tasks
            .finish_workspace_resolution(result.surface, result.request_id);
        let transition = self
            .application
            .handle_event(ApplicationEvent::WorkspaceResolved {
                outlook: result.outlook,
                surface: result.surface,
                request_id: result.request_id,
                result: result.result,
            })?;
        Ok(self.dispatch_transition(transition))
    }

    fn receive_origin_catalog(
        &mut self,
        event: Option<OriginCatalogEvent>,
    ) -> Result<ControlFlow<Exit>> {
        let event = event.ok_or_else(|| anyhow!("Origin catalog task channel stopped"))?;
        if !self
            .application
            .accepts_catalog_event(&event.outlook, &event.event)
        {
            return Ok(ControlFlow::Continue(()));
        }
        self.needs_redraw |= event.event.is_drawn_on_arrival();
        let was_recovering = self.application.is_recovering();
        let transition = self
            .application
            .handle_event(ApplicationEvent::OriginCatalog {
                outlook: event.outlook,
                event: event.event,
            })?;
        if self.application.is_recovering() {
            if !was_recovering {
                self.reconnect_grace = Some(Box::pin(tokio::time::sleep(RECONNECT_GRACE_PERIOD)));
            }
        } else {
            self.reconnect_grace = None;
        }
        Ok(self.dispatch_transition(transition))
    }
}

struct OriginCatalogEvent {
    outlook: Outlook,
    event: ManagedEvent,
}

fn remote_failure_from_session_error(
    outlook: Outlook,
    error: &SessionStreamError,
) -> Option<OriginCatalogEvent> {
    error.remote_status().map(|status| OriginCatalogEvent {
        outlook,
        event: ManagedEvent::RemoteFailed {
            status,
            message: error.to_string(),
        },
    })
}

fn spawn_origin_catalog_forwarder(
    mut subscription: SessionCatalogSubscription,
    outlook: Outlook,
    events: UnboundedSender<OriginCatalogEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = subscription.next().await {
            if events
                .send(OriginCatalogEvent {
                    outlook: outlook.clone(),
                    event,
                })
                .is_err()
            {
                return;
            }
        }
    })
}

struct WorkspaceResolutionResult {
    outlook: Outlook,
    surface: WorkspaceResolutionSurface,
    request_id: u64,
    result: std::result::Result<Workspace, String>,
}

fn spawn_workspace_resolution(
    commands: SessionCommandClient,
    outlook: Outlook,
    surface: WorkspaceResolutionSurface,
    request_id: u64,
    request: ResolveWorkspaceRequest,
    results: UnboundedSender<WorkspaceResolutionResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = commands
            .resolve_workspace(request)
            .await
            .map_err(|error| error.to_string());
        let _ = results.send(WorkspaceResolutionResult {
            outlook,
            surface,
            request_id,
            result,
        });
    })
}

enum PairingResult {
    Prepared {
        settings: Option<Box<SettingsSnapshot>>,
        candidates: Vec<SocketAddr>,
    },
    PreparationFailed(String),
    InviteIssued {
        invite: crate::protocol::IssuedInvite,
        peers: Vec<crate::protocol::Peer>,
    },
    PeerRemoved(String),
    RemotesListed(Vec<crate::protocol::Remote>),
    RemoteListingFailed(String),
    EverywhereRemotesListed {
        request: EverywhereListRequest,
        remotes: Vec<crate::protocol::Remote>,
    },
    EverywhereRemoteListingFailed {
        request: EverywhereListRequest,
        error: String,
    },
    InvitePreviewed {
        invite: String,
        preview: crate::protocol::InvitePreview,
    },
    InvitePreviewFailed {
        invite: String,
        error: String,
    },
    RemoteRedeemed(crate::protocol::Remote),
    InviteRedemptionFailed(String),
    RemoteProbed {
        name: String,
        result: Result<crate::protocol::RemoteHealth, String>,
    },
    OperationFailed(String),
}

fn spawn_invite_redemption(
    commands: SessionCommandClient,
    request: crate::protocol::RedeemInviteRequest,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = commands
            .redeem_invite(request)
            .await
            .map(PairingResult::RemoteRedeemed)
            .unwrap_or_else(|error| PairingResult::InviteRedemptionFailed(error.to_string()));
        let _ = results.send(result);
    });
}

fn spawn_invite_preview(
    commands: SessionCommandClient,
    invite: String,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = match commands.preview_invite(invite.clone()).await {
            Ok(preview) => PairingResult::InvitePreviewed { invite, preview },
            Err(error) => PairingResult::InvitePreviewFailed {
                invite,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

fn spawn_remote_listing(commands: SessionCommandClient, results: UnboundedSender<PairingResult>) {
    tokio::spawn(async move {
        let remotes = match commands.list_remotes().await {
            Ok(remotes) => remotes,
            Err(error) => {
                let _ = results.send(PairingResult::RemoteListingFailed(error.to_string()));
                return;
            }
        };
        if results
            .send(PairingResult::RemotesListed(remotes.clone()))
            .is_err()
        {
            return;
        }
        let mut probes = futures_util::stream::iter(remotes)
            .map(|remote| {
                let commands = commands.clone();
                async move {
                    let result = commands
                        .probe_remote(&remote.name)
                        .await
                        .map_err(|error| error.to_string());
                    (remote.name, result)
                }
            })
            .buffer_unordered(8);
        while let Some((name, result)) = probes.next().await {
            if results
                .send(PairingResult::RemoteProbed { name, result })
                .is_err()
            {
                return;
            }
        }
    });
}

fn spawn_everywhere_remote_listing(
    commands: SessionCommandClient,
    request: EverywhereListRequest,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = match commands.list_remotes().await {
            Ok(remotes) => PairingResult::EverywhereRemotesListed { request, remotes },
            Err(error) => PairingResult::EverywhereRemoteListingFailed {
                request,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

fn spawn_serving_preparation(
    commands: SessionCommandClient,
    enable: bool,
    port: u16,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = async {
            let settings = if enable {
                Some(Box::new(
                    commands
                        .mutate_setting(SettingMutation::ServingEnabled { value: Some(true) })
                        .await?,
                ))
            } else {
                None
            };
            let mut candidates = if_addrs::get_if_addrs()?
                .into_iter()
                .filter(if_addrs::Interface::is_oper_up)
                .filter_map(|interface| invite_candidate(&interface, port))
                .collect::<Vec<_>>();
            candidates.sort_unstable();
            candidates.dedup();
            Ok::<_, anyhow::Error>(PairingResult::Prepared {
                settings,
                candidates,
            })
        }
        .await
        .unwrap_or_else(|error| PairingResult::PreparationFailed(error.to_string()));
        let _ = results.send(result);
    });
}

fn invite_candidate(interface: &if_addrs::Interface, port: u16) -> Option<SocketAddr> {
    let address = interface.ip();
    let is_candidate = match address {
        IpAddr::V4(address) => {
            !address.is_loopback() && !address.is_unspecified() && !address.is_multicast()
        }
        IpAddr::V6(address) => {
            !address.is_loopback() && !address.is_unspecified() && !address.is_multicast()
        }
    };
    if !is_candidate {
        return None;
    }
    Some(match address {
        IpAddr::V6(address) if address.is_unicast_link_local() => {
            SocketAddr::V6(SocketAddrV6::new(address, port, 0, interface.index?))
        }
        address => SocketAddr::new(address, port),
    })
}

fn spawn_invite_issuance(
    commands: SessionCommandClient,
    request: crate::protocol::IssueInviteRequest,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = async {
            let invite = commands.issue_invite(request).await?;
            let peers = commands.list_peers().await?;
            Ok::<_, anyhow::Error>(PairingResult::InviteIssued { invite, peers })
        }
        .await
        .unwrap_or_else(|error| PairingResult::OperationFailed(error.to_string()));
        let _ = results.send(result);
    });
}

fn spawn_peer_removal(
    commands: SessionCommandClient,
    peer_id: String,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = commands
            .remove_peer(&peer_id)
            .await
            .map(|()| PairingResult::PeerRemoved(peer_id))
            .unwrap_or_else(|error| PairingResult::OperationFailed(error.to_string()));
        let _ = results.send(result);
    });
}

fn spawn_session_creation(
    commands: SessionCommandClient,
    outlook: Outlook,
    request: CreateSessionRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    spawn_prompt_delivery(outlook.clone(), None, prompt_id, results, async move {
        let created = commands.create_session(request).await?;
        Ok(SubmissionResult::SessionCreated {
            outlook,
            snapshot: Box::new(created),
        })
    });
}

fn spawn_prompt_admission(
    commands: SessionCommandClient,
    session: SessionReference,
    request: AdmitPromptRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    let outlook = session.origin.clone();
    spawn_prompt_delivery(
        outlook,
        Some(session.clone()),
        prompt_id,
        results,
        async move {
            let prompt = commands.admit_prompt(session.session_id, request).await?;
            Ok(SubmissionResult::PromptAdmitted {
                session,
                prompt_id: prompt.id,
            })
        },
    );
}

/// Delivers a Prompt, reporting any transport failure against `prompt_id` so
/// the composer can restore the text the user submitted.
fn spawn_prompt_delivery(
    outlook: Outlook,
    session: Option<SessionReference>,
    prompt_id: PromptId,
    results: UnboundedSender<SubmissionResult>,
    deliver: impl Future<Output = Result<SubmissionResult>> + Send + 'static,
) {
    tokio::spawn(async move {
        let result = deliver
            .await
            .unwrap_or_else(|error| SubmissionResult::PromptDeliveryFailed {
                outlook,
                session,
                prompt_id,
                error: error.to_string(),
            });
        let _ = results.send(result);
    });
}

enum SubmissionResult {
    SessionCreated {
        outlook: Outlook,
        snapshot: Box<SessionSnapshot>,
    },
    PromptAdmitted {
        session: SessionReference,
        prompt_id: PromptId,
    },
    PromptDeliveryFailed {
        outlook: Outlook,
        session: Option<SessionReference>,
        prompt_id: PromptId,
        error: String,
    },
    QuestionnaireReconciled {
        id: crate::protocol::QuestionnaireId,
        session: SessionReference,
        snapshot: Option<SessionSnapshot>,
        error: Option<String>,
    },
    OperationSucceeded(SessionReference),
    SessionSettled(SessionReference),
    OperationFailed {
        session: SessionReference,
        error: String,
    },
    SessionDeletionFailed {
        session: SessionReference,
        error: String,
    },
    LandingAgentSelectionConfirmed {
        outlook: Outlook,
        selection: AgentSelection,
    },
    LandingAgentSelectionConfirmationFailed {
        outlook: Outlook,
        error: String,
    },
    AgentSelectionUpdated {
        session: SessionReference,
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    },
    AgentSelectionUpdateFailed {
        session: SessionReference,
        operation_id: AgentSelectionOperationId,
        error: String,
    },
    SettingMutated(Box<SettingsSnapshot>),
    SettingMutationFailed(String),
}

enum ModelPickerResult {
    Listed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Refreshed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Failed {
        request: ModelListRequest,
        error: String,
    },
}

enum SkillCatalogResult {
    Listed {
        outlook: Outlook,
        request: SkillCatalogRequest,
        catalog: SkillCatalog,
    },
    Failed {
        outlook: Outlook,
        request: SkillCatalogRequest,
        error: String,
    },
}

#[derive(Clone, Copy)]
enum SkillCatalogOperation {
    List,
    Refresh,
}

fn spawn_skill_catalog_operation(
    commands: SessionCommandClient,
    outlook: Outlook,
    request: SkillCatalogRequest,
    results: UnboundedSender<SkillCatalogResult>,
    operation: SkillCatalogOperation,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let response = match operation {
            SkillCatalogOperation::List => commands.list_skills(request.clone()).await,
            SkillCatalogOperation::Refresh => commands.refresh_skills(request.clone()).await,
        };
        let result = match response {
            Ok(catalog) => SkillCatalogResult::Listed {
                outlook,
                request,
                catalog,
            },
            Err(error) => SkillCatalogResult::Failed {
                outlook,
                request,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    })
}

fn spawn_model_listing(
    commands: SessionCommandClient,
    request: ModelListRequest,
    results: UnboundedSender<ModelPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listing_error = match commands.list_models().await {
            Ok(catalog) => {
                if results
                    .send(ModelPickerResult::Listed {
                        request: request.clone(),
                        catalog,
                    })
                    .is_err()
                {
                    return;
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        let result = match commands.refresh_models().await {
            Ok(catalog) => ModelPickerResult::Refreshed { request, catalog },
            Err(error) => ModelPickerResult::Failed {
                request,
                error: listing_error.map_or_else(
                    || error.to_string(),
                    |listing| format!("{listing}; refresh failed: {error}"),
                ),
            },
        };
        let _ = results.send(result);
    })
}

fn spawn_landing_agent_selection_confirmation(
    commands: SessionCommandClient,
    outlook: Outlook,
    selection: AgentSelection,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands.confirm_landing_agent_selection(selection).await {
            Ok(selection) => {
                SubmissionResult::LandingAgentSelectionConfirmed { outlook, selection }
            }
            Err(error) => SubmissionResult::LandingAgentSelectionConfirmationFailed {
                outlook,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

fn spawn_agent_selection_update(
    commands: SessionCommandClient,
    session: SessionReference,
    request: UpdateAgentSelectionRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let operation_id = request.operation_id;
        let result = match commands
            .update_agent_selection(session.session_id, request)
            .await
        {
            Ok(selection) => SubmissionResult::AgentSelectionUpdated {
                session,
                operation_id,
                selection,
            },
            Err(error) => SubmissionResult::AgentSelectionUpdateFailed {
                session,
                operation_id,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

enum SessionPickerResult {
    Listed {
        request: SessionListRequest,
        sessions: Vec<SessionListItem>,
    },
    ListingFailed {
        request: SessionListRequest,
        error: String,
    },
    Attached {
        reference: SessionReference,
        operation: AttachmentOperationId,
        snapshot: Box<SessionSnapshot>,
        subscription: SessionSubscription,
    },
    AttachmentFailed {
        reference: SessionReference,
        operation: AttachmentOperationId,
        error: String,
    },
}

fn replace_listing<Request: Clone>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    request: Request,
    spawn: impl FnOnce(Request) -> tokio::task::JoinHandle<()>,
) {
    if let Some((_, task)) = active.take() {
        task.abort();
    }
    let task = spawn(request.clone());
    *active = Some((request, task));
}

fn finish_listing<Request: PartialEq>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    completed: &Request,
) {
    if active
        .as_ref()
        .is_some_and(|(request, _)| request == completed)
    {
        *active = None;
    }
}

fn spawn_session_listing(
    commands: SessionCommandClient,
    request: SessionListRequest,
    results: UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match commands
            .list_sessions(request.scope.workspace_filter())
            .await
        {
            Ok(sessions) => SessionPickerResult::Listed { request, sessions },
            Err(error) => SessionPickerResult::ListingFailed {
                request,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    })
}

/// Attaches `reference`, reporting the `operation` that asked for it on both
/// the answer and the refusal so the run loop can tell the navigation the
/// reader is still waiting on from the one they left.
fn spawn_session_attachment(
    commands: SessionCommandClient,
    reference: SessionReference,
    operation: AttachmentOperationId,
    results: UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let session_id = reference.session_id;
        let result = async {
            let mut subscription = commands.attach_session(session_id).await?;
            let event = subscription
                .next()
                .await
                .ok_or_else(|| anyhow!("target Session subscription ended before hydration"))??;
            let SessionEvent::Snapshot(snapshot) = event else {
                return Err(anyhow!("target Session updated before hydration"));
            };
            Ok::<_, anyhow::Error>(SessionPickerResult::Attached {
                reference: reference.clone(),
                operation,
                snapshot,
                subscription,
            })
        }
        .await
        .unwrap_or_else(|error| SessionPickerResult::AttachmentFailed {
            reference,
            operation,
            error: error.to_string(),
        });
        let _ = results.send(result);
    })
}

enum SessionOperation {
    SubmitQuestionnaire {
        session_id: SessionId,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    },
    DeleteSession {
        session_id: SessionId,
    },
    SettleSession {
        session_id: SessionId,
        settled: bool,
    },
    PromotePrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    CancelPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    InterruptSession {
        session_id: SessionId,
    },
}

impl SessionOperation {
    async fn run(
        self,
        commands: SessionCommandClient,
        session: SessionReference,
    ) -> SubmissionResult {
        match self {
            Self::DeleteSession { session_id } => match commands.delete_session(session_id).await {
                Ok(()) => SubmissionResult::OperationSucceeded(session),
                Err(error) => SubmissionResult::SessionDeletionFailed {
                    session,
                    error: error.to_string(),
                },
            },
            // The catalog carries the updated summary to every client. Only
            // the client that settled the Session should start a new one.
            Self::SettleSession {
                session_id,
                settled,
            } => match commands.settle_session(session_id, settled).await {
                Ok(_) if settled => SubmissionResult::SessionSettled(session),
                result => operation_result(session, result.map(|_| ())),
            },
            Self::PromotePrompt {
                session_id,
                prompt_id,
            } => operation_result(
                session,
                commands
                    .promote_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::CancelPrompt {
                session_id,
                prompt_id,
            } => operation_result(
                session,
                commands
                    .cancel_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::SubmitQuestionnaire {
                session_id,
                id,
                submission,
            } => {
                let delivery = commands
                    .submit_questionnaire(session_id, id, submission)
                    .await;
                // A failed acknowledgement says nothing about Provider delivery. Read
                // server arbitration before permitting an explicit retry; never resend.
                let snapshot = commands.read_session(session_id).await.ok();
                let outcome = snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .activities
                        .iter()
                        .rev()
                        .find_map(|activity| match activity {
                            crate::protocol::Activity::Questionnaire {
                                questionnaire,
                                outcome,
                                ..
                            } if questionnaire.id == id => Some(*outcome),
                            _ => None,
                        })
                });
                let error = match outcome {
                    Some(
                        crate::protocol::QuestionnaireOutcome::Answered
                        | crate::protocol::QuestionnaireOutcome::Declined,
                    ) => None,
                    Some(crate::protocol::QuestionnaireOutcome::DeliveryUncertain) => Some(
                        "Provider delivery is uncertain. This Answer will not be resent.".into(),
                    ),
                    Some(crate::protocol::QuestionnaireOutcome::SubmissionRejected) => {
                        Some("Answer was not delivered. Review your draft and retry.".into())
                    }
                    _ if snapshot.is_none() => Some(
                        "Submission status is unconfirmed. Reconnect to check before retrying."
                            .into(),
                    ),
                    _ => delivery.err().map(|error| error.to_string()),
                };
                SubmissionResult::QuestionnaireReconciled {
                    session,
                    id,
                    snapshot,
                    error,
                }
            }
            Self::InterruptSession { session_id } => {
                operation_result(session, commands.interrupt_session(session_id).await)
            }
        }
    }
}

fn operation_result(session: SessionReference, result: anyhow::Result<()>) -> SubmissionResult {
    match result {
        Ok(()) => SubmissionResult::OperationSucceeded(session),
        Err(error) => SubmissionResult::OperationFailed {
            session,
            error: error.to_string(),
        },
    }
}

/// Sends one Setting's typed edit to the server, which owns the Config
/// Document, and brings back the effective settings the edit left in force.
fn spawn_setting_mutation(
    commands: SessionCommandClient,
    mutation: SettingMutation,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands.mutate_setting(mutation).await {
            Ok(snapshot) => SubmissionResult::SettingMutated(Box::new(snapshot)),
            Err(error) => SubmissionResult::SettingMutationFailed(error.to_string()),
        };
        let _ = results.send(result);
    });
}

fn spawn_session_operation(
    commands: SessionCommandClient,
    session: SessionReference,
    operation: SessionOperation,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = operation.run(commands, session).await;
        let _ = results.send(result);
    });
}

/// Reports a root Session open without coupling navigation to the request's
/// answer. The catalog stream carries the authoritative Viewed moment back to
/// every client, including this one; a transient reporting failure must not
/// cancel an attachment that can still succeed.
fn spawn_session_view(commands: SessionCommandClient, session: SessionReference) {
    tokio::spawn(async move {
        if let Err(error) = commands
            .view_session(
                session.session_id,
                crate::protocol::ViewSessionRequest {
                    operation_id: crate::protocol::ViewSessionOperationId::new(),
                },
            )
            .await
        {
            tracing::warn!(
                session_id = %session.session_id,
                origin = ?session.origin,
                "could not report Session Viewed: {error}"
            );
        }
    });
}

struct ConnectedSessionSubscription {
    reference: SessionReference,
    subscription: SessionSubscription,
}

fn spawn_session_subscription(
    commands: SessionCommandClient,
    reference: SessionReference,
    connected: UnboundedSender<ConnectedSessionSubscription>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let session_id = reference.session_id;
        let (initial_backoff, max_backoff) = commands.recovery_backoff();
        let mut backoff = RecoveryBackoff::new(initial_backoff, max_backoff);
        loop {
            if connected.is_closed() {
                return;
            }
            match commands.subscribe_session(session_id).await {
                Ok(subscription) => {
                    let _ = connected.send(ConnectedSessionSubscription {
                        reference,
                        subscription,
                    });
                    return;
                }
                Err(error)
                    if error
                        .downcast_ref::<SessionStreamError>()
                        .is_some_and(|error| error.remote_status().is_some()) =>
                {
                    return;
                }
                Err(_) => {
                    let retry_in = backoff.next();
                    tokio::time::sleep(retry_in).await;
                }
            }
        }
    })
}

async fn next_session_event(
    subscription: &mut Option<SessionSubscription>,
) -> Option<std::result::Result<SessionEvent, SessionStreamError>> {
    match subscription {
        Some(subscription) => subscription.next().await,
        None => pending().await,
    }
}

async fn wait_for_reconnect_grace(grace: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match grace {
        Some(grace) => grace.as_mut().await,
        None => pending().await,
    }
}

async fn wait_for_opening_loading_delay(
    delay: &mut Option<(Instant, Pin<Box<tokio::time::Sleep>>)>,
) {
    match delay {
        Some((_, delay)) => delay.as_mut().await,
        None => pending().await,
    }
}

async fn wait_for_spinner_tick(tick: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match tick {
        Some(tick) => tick.as_mut().await,
        None => pending().await,
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<termina::PlatformTerminal>>,
}

/// Enables button and wheel reporting (1000), SGR encoding (1006), and
/// motion while a button is held (1002).
/// Deliberately excludes any-motion tracking (1003), which crossterm's
/// `EnableMouseCapture` turns on: motion tracking floods the input stream with
/// pointer-move events nothing in the TUI consumes.
struct EnableMouseButtonReporting;

impl crossterm::Command for EnableMouseButtonReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!("\x1b[?1000h", "\x1b[?1006h", "\x1b[?1002h"))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

struct DisableMouseButtonReporting;

impl crossterm::Command for DisableMouseButtonReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!("\x1b[?1002l", "\x1b[?1006l", "\x1b[?1000l"))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Asks for modified-key disambiguation, the first level of the kitty keyboard
/// protocol. This is emitted as VT on every platform because the owned input
/// stream parses the terminal's replies itself.
struct PushModifiedKeyReporting;

impl crossterm::Command for PushModifiedKeyReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[>1u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )
        .execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

struct PopModifiedKeyReporting;

impl crossterm::Command for PopModifiedKeyReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[<1u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::PopKeyboardEnhancementFlags.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Subscribes to terminal dark/light notifications (DEC private mode 2031).
/// A supporting terminal reports changes as `CSI ? 997 ; 1|2 n`; terminals
/// that do not know the mode ignore it.
struct EnableTerminalThemeUpdates;

impl crossterm::Command for EnableTerminalThemeUpdates {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?2031h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

struct DisableTerminalThemeUpdates;

impl crossterm::Command for DisableTerminalThemeUpdates {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?2031l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Terminal features are progressive: a terminal that does not implement one
/// ignores its escape sequence. That is not a reason to refuse to draw the TUI,
/// while a terminal that has gone away still is.
fn ignore_unsupported(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        result => result,
    }
}

/// The seam every terminal-mode change is written through. Production routes
/// each command through crossterm's `execute!`; tests substitute a sink that
/// records the ANSI rendering so the order can be asserted on every platform.
trait TerminalSink {
    fn apply(&mut self, command: impl crossterm::Command) -> std::io::Result<()>;
}

/// Native copies are best effort: no failure may reach the Application or end
/// the run. Tests substitute a recording sink without opening a display.
trait NativeClipboardSink {
    fn copy(&mut self, text: &str);
}

/// The native writer is injectable so failure reporting can be exercised
/// through the sink without ever accessing a real Clipboard.
struct NativeClipboard<W> {
    write: W,
    failure_reported: bool,
}

impl<W: FnMut(&str) -> Result<(), arboard::Error>> NativeClipboard<W> {
    fn new(write: W) -> Self {
        Self {
            write,
            failure_reported: false,
        }
    }
}

impl<W: FnMut(&str) -> Result<(), arboard::Error>> NativeClipboardSink for NativeClipboard<W> {
    fn copy(&mut self, text: &str) {
        if let Err(error) = (self.write)(text) {
            if self.failure_reported {
                tracing::debug!(%error, "could not write native Clipboard");
            } else {
                self.failure_reported = true;
                tracing::warn!(%error, "could not write native Clipboard");
            }
        }
    }
}

fn native_clipboard() -> impl NativeClipboardSink {
    // Keep the handle for the whole run: on Linux it owns the copied text.
    // Failed creation leaves it absent, so the next copy retries immediately.
    let mut clipboard = None;
    NativeClipboard::new(move |text: &str| {
        let clipboard = match &mut clipboard {
            Some(clipboard) => clipboard,
            slot @ None => slot.insert(arboard::Clipboard::new()?),
        };
        clipboard.set_text(text)
    })
}

/// OSC 52 asks the terminal to place bytes on its clipboard. The Invite stays
/// drawn after this command, so a terminal that ignores OSC 52 still leaves
/// the reader able to select the same string directly from the screen.
struct CopyToClipboard<'a>(&'a str);

impl crossterm::Command for CopyToClipboard<'_> {
    fn write_ansi(&self, output: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(output, "\x1b]52;c;{}\x07", STANDARD.encode(self.0))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

fn copy_to_clipboard(
    output: &mut impl TerminalSink,
    native: &mut impl NativeClipboardSink,
    text: &str,
) {
    native.copy(text);
    if let Err(error) = ignore_unsupported(output.apply(CopyToClipboard(text))) {
        tracing::warn!(%error, "could not write Clipboard through OSC 52");
    }
}

struct TerminalOutput<W: std::io::Write>(W);

impl<W: std::io::Write> TerminalSink for TerminalOutput<W> {
    fn apply(&mut self, command: impl crossterm::Command) -> std::io::Result<()> {
        execute!(self.0, command)
    }
}

fn enable_terminal_features(output: &mut impl TerminalSink) -> std::io::Result<()> {
    ignore_unsupported(output.apply(EnableBracketedPaste))?;
    // Mouse reporting is the one feature with no graceful degradation: a TUI
    // that cannot read clicks or the wheel is worth refusing to start.
    output.apply(EnableMouseButtonReporting)?;
    ignore_unsupported(output.apply(PushModifiedKeyReporting))?;
    ignore_unsupported(output.apply(EnableTerminalThemeUpdates))
}

/// Every restore is attempted even after one of them fails: leaving the terminal
/// in mouse capture is worse than a restore whose error nobody could act on. The
/// first failure is the one reported.
fn disable_terminal_features(output: &mut impl TerminalSink) -> std::io::Result<()> {
    let theme_updates = ignore_unsupported(output.apply(DisableTerminalThemeUpdates));
    let modified_keys = ignore_unsupported(output.apply(PopModifiedKeyReporting));
    let mouse = output.apply(DisableMouseButtonReporting);
    let paste = ignore_unsupported(output.apply(DisableBracketedPaste));
    theme_updates.and(modified_keys).and(mouse).and(paste)
}

/// Gives the terminal back the screen it was showing and the cursor it was
/// showing it with. Both are attempted for the same reason the features above
/// are: a terminal left on the alternate screen is worse than a failed restore.
fn leave_terminal_screen(output: &mut impl TerminalSink) -> std::io::Result<()> {
    let screen = output.apply(LeaveAlternateScreen);
    let cursor = output.apply(Show);
    screen.and(cursor)
}

/// Takes the screen and then the input features, undoing whatever already
/// succeeded when a later step fails so a refusal to start never strands the
/// terminal half-configured.
fn enter_terminal_display(output: &mut impl TerminalSink) -> std::io::Result<()> {
    if let Err(error) = output
        .apply(EnterAlternateScreen)
        .and_then(|()| output.apply(Hide))
    {
        let _ = leave_terminal_screen(output);
        return Err(error);
    }
    if let Err(error) = enable_terminal_features(output) {
        let _ = disable_terminal_features(output);
        let _ = leave_terminal_screen(output);
        return Err(error);
    }
    Ok(())
}

/// Restores in the exact reverse of [`enter_terminal_display`], so a terminal
/// that ignored one of the features on the way in sees the matching restore.
fn leave_terminal_display(output: &mut impl TerminalSink) -> std::io::Result<()> {
    let features = disable_terminal_features(output);
    let screen = leave_terminal_screen(output);
    features.and(screen)
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        let mut platform = termina::PlatformTerminal::new()?;
        platform.enter_raw_mode()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(platform))?;
        if let Err(error) = enter_terminal_display(&mut TerminalOutput(terminal.backend_mut())) {
            return Err(error.into());
        }
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Err(error) = leave_terminal_display(&mut TerminalOutput(self.terminal.backend_mut()))
        {
            tracing::warn!("could not restore the terminal: {error}");
        }
        // The Termina writer restores its captured platform mode on drop.
    }
}

#[cfg(test)]
mod tests {
    use crossterm::Command;

    use super::{
        Application, ApplicationEvent, DisableMouseButtonReporting, EnableMouseButtonReporting,
        NativeClipboard, NativeClipboardSink, PopModifiedKeyReporting, PushModifiedKeyReporting,
        TerminalSink, copy_to_clipboard, enter_terminal_display, ignore_unsupported,
        leave_terminal_display, remote_failure_from_session_error,
    };
    use crate::{
        managed_client::{ManagedEvent, SessionStreamError},
        protocol::{EffectiveSettings, Outlook, RemoteStatus, SettingsSnapshot},
    };

    #[test]
    fn the_first_frame_waits_until_the_initial_settings_snapshot_is_applied() {
        let mut application = Application::default();
        assert!(!application.first_frame_ready());

        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
                SettingsSnapshot {
                    settings: EffectiveSettings::default(),
                    pinned: Vec::new(),
                    diagnostics: Vec::new(),
                },
            )))
            .expect("apply the initial settings snapshot");

        assert!(application.first_frame_ready());
    }

    #[test]
    fn an_ignored_background_lifecycle_event_does_not_request_a_frame() {
        let application = Application::default();
        let event = ManagedEvent::RemoteRecovered;

        assert!(!application.accepts_catalog_event(&Outlook::Remote("studio".to_owned()), &event,));
        assert!(application.accepts_catalog_event(&Outlook::Local, &event));
    }

    #[test]
    fn a_terminal_remote_session_failure_uses_the_outlook_ejection_path() {
        let outlook = Outlook::Remote("studio".to_owned());
        let event = remote_failure_from_session_error(
            outlook.clone(),
            &SessionStreamError::remote(RemoteStatus::Revoked, "Remote revoked this Pairing"),
        )
        .expect("terminal Remote Session errors become Outlook failures");

        assert_eq!(event.outlook, outlook);
        assert!(matches!(
            event.event,
            ManagedEvent::RemoteFailed {
                status: RemoteStatus::Revoked,
                ref message,
            } if message == "Remote revoked this Pairing"
        ));
    }

    /// Records what an ANSI terminal would receive. Going through `write_ansi`
    /// rather than `execute!` keeps the recording independent of the console the
    /// test process happens to be attached to, so the sequence is the same on
    /// Windows, macOS, and Linux.
    struct AnsiTranscript(String);

    impl TerminalSink for AnsiTranscript {
        fn apply(&mut self, command: impl Command) -> std::io::Result<()> {
            command
                .write_ansi(&mut self.0)
                .map_err(std::io::Error::other)
        }
    }

    impl AnsiTranscript {
        fn record(sequence: impl Fn(&mut Self) -> std::io::Result<()>) -> String {
            let mut transcript = Self(String::new());
            sequence(&mut transcript).expect("an ANSI transcript never fails to record");
            transcript.0
        }

        /// Positions the escape sequences within the transcript, in the order
        /// they were asked for, so a caller can assert that order is ascending.
        fn positions(transcript: &str, sequences: &[&str]) -> Vec<usize> {
            sequences
                .iter()
                .map(|sequence| {
                    transcript.find(sequence).unwrap_or_else(|| {
                        panic!("{sequence:?} is missing from the transcript {transcript:?}")
                    })
                })
                .collect()
        }
    }

    #[test]
    fn copying_an_invite_writes_the_same_text_to_both_clipboards() {
        let mut native = RecordingClipboard::default();
        let mut terminal = AnsiTranscript(String::new());
        copy_to_clipboard(&mut terminal, &mut native, "suru-v1-example");

        assert_eq!(terminal.0, "\x1b]52;c;c3VydS12MS1leGFtcGxl\x07");
        assert_eq!(native.0, ["suru-v1-example"]);
    }

    #[test]
    fn copying_a_text_selection_preserves_unicode_and_newlines_in_both_clipboards() {
        let mut native = RecordingClipboard::default();
        let mut terminal = AnsiTranscript(String::new());
        copy_to_clipboard(&mut terminal, &mut native, "Hello\n界🙂");

        assert_eq!(native.0, ["Hello\n界🙂"]);
        assert_eq!(terminal.0, "\x1b]52;c;SGVsbG8K55WM8J+Zgg==\x07");
    }

    #[derive(Default)]
    struct RecordingClipboard(Vec<String>);

    impl NativeClipboardSink for RecordingClipboard {
        fn copy(&mut self, text: &str) {
            self.0.push(text.to_owned());
        }
    }

    struct FailedTerminal(std::io::ErrorKind);

    impl TerminalSink for FailedTerminal {
        fn apply(&mut self, _: impl Command) -> std::io::Result<()> {
            Err(self.0.into())
        }
    }

    fn record_log(action: impl FnOnce()) -> String {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clipboard.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::sync::Arc::new(std::fs::File::create(&path).unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, action);
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn an_osc_52_io_failure_is_logged_without_failing_the_copy() {
        let log = record_log(|| {
            let mut terminal = FailedTerminal(std::io::ErrorKind::BrokenPipe);
            let mut native = RecordingClipboard::default();
            copy_to_clipboard(&mut terminal, &mut native, "private copied text");
            assert_eq!(native.0, ["private copied text"]);
        });
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains("OSC 52"), "{log}");
        assert!(!log.contains("private copied text"), "{log}");
    }

    #[test]
    fn unsupported_osc_52_still_copies_natively_without_a_warning() {
        let log = record_log(|| {
            let mut terminal = FailedTerminal(std::io::ErrorKind::Unsupported);
            let mut native = RecordingClipboard::default();
            copy_to_clipboard(&mut terminal, &mut native, "suru-v1-example");
            assert_eq!(native.0, ["suru-v1-example"]);
        });
        assert!(log.is_empty(), "{log}");
    }

    #[test]
    fn native_failures_warn_once_per_run_and_never_suppress_osc_52() {
        let mut terminal = AnsiTranscript(String::new());
        let mut offered = Vec::new();
        let log = record_log(|| {
            let mut native = NativeClipboard::new(|text: &str| {
                offered.push(text.to_owned());
                if text == "ok" {
                    Ok(())
                } else {
                    Err(arboard::Error::ClipboardNotSupported)
                }
            });
            for text in ["one", "two", "ok", "three"] {
                copy_to_clipboard(&mut terminal, &mut native, text);
            }
        });

        assert_eq!(offered, ["one", "two", "ok", "three"]);
        assert_eq!(
            terminal.0,
            "\x1b]52;c;b25l\x07\x1b]52;c;dHdv\x07\x1b]52;c;b2s=\x07\x1b]52;c;dGhyZWU=\x07"
        );
        let lines: Vec<_> = log.lines().collect();
        assert_eq!(lines.len(), 3, "{log}");
        assert!(lines[0].contains("WARN"), "{log}");
        assert!(lines[1].contains("DEBUG"), "{log}");
        assert!(lines[2].contains("DEBUG"), "{log}");
        assert!(
            lines.iter().all(|line| line.contains("native Clipboard")),
            "{log}"
        );
    }

    /// The screen has to be taken before the input features are turned on: a
    /// terminal that starts reporting mouse and paste input while the shell is
    /// still on screen delivers that input to whatever is running there.
    #[test]
    fn entering_the_display_takes_the_screen_before_arming_the_input_features() {
        let transcript = AnsiTranscript::record(enter_terminal_display);
        let positions = AnsiTranscript::positions(
            &transcript,
            &[
                "\x1b[?1049h", // alternate screen
                "\x1b[?25l",   // cursor hidden
                "\x1b[?2004h", // bracketed paste
                "\x1b[?1000h", // mouse button reporting
                "\x1b[?1006h", // SGR mouse encoding
                "\x1b[?1002h", // button-motion reporting
                "\x1b[>1u",    // modified key reporting
                "\x1b[?2031h", // terminal Theme updates
            ],
        );
        assert!(
            positions.is_sorted(),
            "the display was entered out of order: {transcript:?}"
        );
    }

    /// The exact reverse of entering. Restoring the features only after the
    /// screen is given back would leave them armed over the shell for as long as
    /// the restore takes, which is the same window entering avoids.
    #[test]
    fn leaving_the_display_restores_in_the_reverse_of_the_order_it_took() {
        let transcript = AnsiTranscript::record(leave_terminal_display);
        let positions = AnsiTranscript::positions(
            &transcript,
            &[
                "\x1b[?2031l", // terminal Theme updates
                "\x1b[<1u",    // modified key reporting
                "\x1b[?1002l", // button-motion reporting
                "\x1b[?1006l", // SGR mouse encoding
                "\x1b[?1000l", // mouse button reporting
                "\x1b[?2004l", // bracketed paste
                "\x1b[?1049l", // alternate screen
                "\x1b[?25h",   // cursor shown
            ],
        );
        assert!(
            positions.is_sorted(),
            "the display was restored out of order: {transcript:?}"
        );
    }

    /// Anything the TUI turns on has to be turned off again, or it outlives the
    /// process in the terminal that hosted it.
    #[test]
    fn every_feature_entering_the_display_turns_on_is_turned_off_again() {
        let entered = AnsiTranscript::record(enter_terminal_display);
        let left = AnsiTranscript::record(leave_terminal_display);
        assert!(
            !entered.contains("\x1b[?1003h"),
            "any-motion tracking stays off"
        );
        for (enabled, disabled) in [
            ("\x1b[?1049h", "\x1b[?1049l"),
            ("\x1b[?25l", "\x1b[?25h"),
            ("\x1b[?2004h", "\x1b[?2004l"),
            ("\x1b[?1000h", "\x1b[?1000l"),
            ("\x1b[?1006h", "\x1b[?1006l"),
            ("\x1b[?1002h", "\x1b[?1002l"),
            ("\x1b[>1u", "\x1b[<1u"),
            ("\x1b[?2031h", "\x1b[?2031l"),
        ] {
            assert!(
                entered.contains(enabled),
                "{enabled:?} is missing from {entered:?}"
            );
            assert!(
                left.contains(disabled),
                "{enabled:?} was turned on but {disabled:?} never turned it off: {left:?}"
            );
        }
    }
    /// Pins the input feature sequences every supported platform receives.
    #[test]
    fn terminal_input_capabilities_enable_mouse_and_modified_key_reporting() {
        let mut enabled = String::new();
        EnableMouseButtonReporting
            .write_ansi(&mut enabled)
            .expect("format mouse reporting command");
        PushModifiedKeyReporting
            .write_ansi(&mut enabled)
            .expect("format modified key reporting command");
        assert!(
            enabled.contains("\x1b[?1000h"),
            "mouse capture was not enabled"
        );
        assert!(
            enabled.contains("\x1b[>1u"),
            "modified key reporting was not enabled"
        );

        let mut disabled = String::new();
        PopModifiedKeyReporting
            .write_ansi(&mut disabled)
            .expect("format modified key restoration command");
        DisableMouseButtonReporting
            .write_ansi(&mut disabled)
            .expect("format mouse reporting restoration command");
        assert!(
            disabled.contains("\x1b[?1000l"),
            "mouse capture was not disabled"
        );
        assert!(
            disabled.contains("\x1b[<1u"),
            "modified key reporting was not restored"
        );
    }

    #[test]
    fn a_feature_the_terminal_cannot_support_is_not_fatal() {
        let unsupported =
            std::io::Error::new(std::io::ErrorKind::Unsupported, "no keyboard enhancement");
        ignore_unsupported(Err(unsupported)).expect("an unsupported feature is not an error");
    }

    #[test]
    fn a_failure_other_than_an_unsupported_feature_still_propagates() {
        let broken = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the terminal went away");
        let error = ignore_unsupported(Err(broken)).expect_err("a broken terminal is fatal");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// The owned Windows reader consumes VT bytes, so input features must ask
    /// Windows Terminal for the same ANSI reports used on Unix.
    #[cfg(windows)]
    #[test]
    fn input_features_use_virtual_terminal_sequences_on_windows() {
        assert!(
            EnableMouseButtonReporting.is_ansi_code_supported(),
            "mouse reporting did not use the VT input stream"
        );
        assert!(
            DisableMouseButtonReporting.is_ansi_code_supported(),
            "mouse reporting restoration did not use the VT input stream"
        );
        assert!(
            PushModifiedKeyReporting.is_ansi_code_supported(),
            "modified-key reporting did not use the VT input stream"
        );
        assert!(
            PopModifiedKeyReporting.is_ansi_code_supported(),
            "modified-key restoration did not use the VT input stream"
        );
    }
}
