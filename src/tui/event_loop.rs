//! Terminal lifecycle and the async run loop: the terminal session guard, the
//! `tokio::select!` loop that feeds the Application, and the tasks it spawns to
//! carry out the transitions the Application returns.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
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
        SubagentTreeEvent,
    },
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, CreateSessionRequest,
        ModelCatalog, Outlook, PromptId, ResolveWorkspaceRequest, SessionId, SessionListItem,
        SessionReference, SessionSnapshot, SettingMutation, SettingsSnapshot, SkillCatalog,
        SkillCatalogRequest, UpdateAgentSelectionRequest, UpdateApprovalPostureRequest,
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
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
};
use termina::Terminal as _;
use tokio::sync::mpsc::UnboundedSender;

use super::ClipboardContent;
use super::commands::SemanticCommandId;
use super::session_attach::{AttachOperationId, AttachOutcome, SessionAttach};
use super::shimmer;
use super::state::{
    Application, ApplicationEvent, ApplicationTransition, CommandId, EverywhereListRequest,
    ModelListRequest, SessionListRequest, SessionListSurface, WorkspaceResolutionSurface,
};
use crate::terminal::{
    CellSize, GraphicsReply, TerminalEvents, TerminalFacts, TerminalInput, request_terminal_colors,
};

const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(1);

mod clipboard_thread;
pub(super) mod frame_backend;

/// termina's own writer is 128 bytes on Windows and 4 KiB on Unix, so a frame
/// of a few KiB reached the console as dozens of writes and the terminal could
/// repaint between any two of them. A frame-sized buffer, flushed once per
/// frame, complements DEC 2026 rather than replacing it: it is what keeps the
/// bracket and its contents in one write on terminals that honour the mode,
/// and what keeps the diff contiguous on those that do not.
const FRAME_BUFFER_CAPACITY: usize = 64 * 1024;

/// The production terminal: crossterm over a frame-sized buffer over the
/// platform terminal, each draw reaching it as one whole frame.
type TuiTerminal = Terminal<
    frame_backend::FrameBackend<CrosstermBackend<std::io::BufWriter<termina::PlatformTerminal>>>,
>;

pub async fn run(client: ManagedClient) -> Result<()> {
    let workspace =
        std::env::current_dir().map_err(|error| anyhow!("read current Workspace: {error}"))?;
    let mut session = TerminalSession::enter()?;
    let mut input = TerminalEvents::open()?;
    // A window that reports its pixels spares the probe asking for the cell
    // size; one that cannot say is asked instead.
    let cell_size = session
        .terminal
        .backend_mut()
        .window_size()
        .ok()
        .and_then(CellSize::from_window);
    let terminal_facts = TerminalFacts::unprobed(available_color_count() == u16::MAX)
        .with_hyperlinks(TerminalFacts::hyperlinks_from_environment())
        .with_multiplexer(TerminalFacts::multiplexed_from_environment())
        .with_terminal_program(std::env::var("TERM_PROGRAM").ok().as_deref())
        .with_cell_size(cell_size);
    input.request_probe(session.terminal.backend_mut(), &terminal_facts)?;
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
    /// The Session attach the reader is waiting on, correlated so that
    /// only their newest choice can land.
    session_attach: SessionAttach,
    /// One in-flight Session listing per surface and Origin: the picker and
    /// Sidebar list at once, while Everywhere lets the Sidebar ask several
    /// Servers concurrently. A fresh request supersedes only that exact
    /// surface-and-Origin conversation.
    listing_sessions:
        HashMap<(SessionListSurface, Outlook), (SessionListRequest, tokio::task::JoinHandle<()>)>,
    listing_models: Option<(ModelListRequest, tokio::task::JoinHandle<()>)>,
    listing_skills: Option<((Outlook, SkillCatalogRequest), tokio::task::JoinHandle<()>)>,
    catalog_origins: HashMap<Outlook, tokio::task::JoinHandle<()>>,
    /// The per-tree subscription the Aside follows, named by the Session it
    /// was asked through.
    subagent_tree: Option<(SessionReference, tokio::task::JoinHandle<()>)>,
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
    /// A Session attach is left alone: this is also the housekeeping a client
    /// with no Session open does, and a reader attaching one from the Landing
    /// has no Session open yet.
    fn detach(&mut self) {
        self.end_subscription();
        self.abort_subscribing();
    }

    /// The reader left the Session they were on — for the Landing, another
    /// Workspace, or another Outlook. Nothing that was being loaded for them
    /// is still an answer to where they are, so the Session attach goes with
    /// the subscription.
    fn leave_session(&mut self) {
        self.detach();
        self.session_attach.abandon();
    }

    fn abort_subscribing(&mut self) {
        if let Some((_, task)) = self.subscribing.take() {
            task.abort();
        }
    }

    /// Keeps exactly the per-tree subscription the Aside wants: the one
    /// asked through `wanted`, or none. A subscription asked through the same
    /// Session is kept, whichever Session of its tree the reader moves to.
    fn follow_subagent_tree(
        &mut self,
        client: &ManagedClient,
        wanted: Option<SessionReference>,
        events: &UnboundedSender<SubagentTreeDelivery>,
    ) {
        if self.subagent_tree.as_ref().map(|(through, _)| through) == wanted.as_ref() {
            return;
        }
        if let Some((_, task)) = self.subagent_tree.take() {
            task.abort();
        }
        let Some(through) = wanted else {
            return;
        };
        let mut subscription = client
            .session_commands_for(through.origin.clone())
            .subscribe_subagent_tree(through.session_id);
        let events = events.clone();
        let forwarded = through.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = subscription.next().await {
                if events
                    .send(SubagentTreeDelivery {
                        through: forwarded.clone(),
                        event,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        self.subagent_tree = Some((through, task));
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
        if !matches!(
            surface,
            WorkspaceResolutionSurface::Outlook | WorkspaceResolutionSurface::WorktreeList
        ) {
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

    /// Attaches to `reference`, superseding whatever attach was already in
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
        self.session_attach.begin(reference, |target, operation| {
            spawn_session_attach(commands, target, operation, results)
        });
    }

    /// Whether a finished attach is still the one the reader is waiting
    /// on, forgetting it when it is.
    fn settle_attach(&mut self, operation: AttachOperationId) -> AttachOutcome {
        self.session_attach.settle(operation)
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
    subagent_trees: UnboundedSender<SubagentTreeDelivery>,
}

/// The run loop's mutable world: the Application it feeds, the client it sends
/// Session commands through, the Session work it owns, and the channels its
/// spawned tasks report back on.
struct RunLoop {
    client: ManagedClient,
    application: Application,
    tasks: SessionTasks,
    channels: TaskChannels,
    /// One grace period per Origin presently recovering, in the order they
    /// were armed — which is the order they come due, every grace being the
    /// same length. A Remote's drives its banner; this machine's own Server's
    /// drives the whole-frame modal.
    reconnect_grace: Vec<(Outlook, Pin<Box<tokio::time::Sleep>>)>,
    /// The optimistic shell's one-shot quiet-period wakeup, paired with its
    /// absolute deadline so a superseding route can replace it exactly once.
    opening_loading_delay: Option<(Instant, Pin<Box<tokio::time::Sleep>>)>,
    /// The Aside's one-shot wakeup at the end of a tree's quiet period, so a
    /// tree still arriving is redrawn saying Loading.
    tree_loading_delay: Option<(Instant, Pin<Box<tokio::time::Sleep>>)>,
    /// Armed only while something on screen animates, so an idle TUI schedules
    /// zero wakeups (ADR 0009). Re-armed on every fire.
    spinner_tick: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Set by anything that changes what is on screen, so an event the user
    /// cannot see costs no frame.
    needs_redraw: bool,
}

async fn run_loop(
    terminal: &mut TuiTerminal,
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
    let (subagent_trees, mut subagent_tree_rx) = tokio::sync::mpsc::unbounded_channel();
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
            subagent_trees,
        },
        reconnect_grace: Vec::new(),
        opening_loading_delay: None,
        tree_loading_delay: None,
        spinner_tick: None,
        needs_redraw: true,
    };
    let mut clipboard = clipboard_thread::ClipboardThread::new(native_clipboard);
    let mut delivery = ClipboardDelivery::default();
    loop {
        run.sync_skill_catalog();
        run.sync_subagent_tree();
        if run.needs_redraw && run.application.first_frame_ready() {
            draw_frame(
                terminal,
                run.application.terminal_facts.hyperlinks,
                |frame| run.application.render(frame),
            )?;
            run.needs_redraw = false;
        }
        // Rendering records which animation is actually visible, including a
        // Working Indicator that may have scrolled out of the viewport.
        run.sync_opening_loading_delay();
        run.sync_tree_loading_delay();
        run.sync_spinner_tick();
        // Every arm reports through ControlFlow so the two events that can end
        // the run -- a Provider shutdown and the exit command -- leave by the
        // same path as the input stream closing.
        let step = tokio::select! {
            text = delivery.fallback() => {
                copy_to_terminal(&mut TerminalOutput(terminal.backend_mut()), &text);
                ControlFlow::Continue(())
            }
            managed_event = run.client.next() => run.receive_managed_event(managed_event)?,
            outlook = wait_for_reconnect_grace(&mut run.reconnect_grace) => {
                run.expire_reconnect_grace(outlook)?
            }
            () = wait_for_opening_loading_delay(&mut run.opening_loading_delay) => {
                run.reveal_opening_loading()
            }
            () = wait_for_opening_loading_delay(&mut run.tree_loading_delay) => {
                // The frame reads Loading off the presentation clock, so the
                // wakeup only has to draw it.
                run.tree_loading_delay = None;
                run.needs_redraw = true;
                ControlFlow::Continue(())
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
            tree = subagent_tree_rx.recv() => run.receive_subagent_tree(tree)?,
            input_event = input.next() => match input_event {
                Some(Ok(event)) => run.handle_terminal_input(event, terminal.backend_mut(), &mut clipboard, &mut delivery)?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            },
        };
        if let ControlFlow::Break(exit) = step {
            clipboard.shutdown();
            delivery.finish(&mut TerminalOutput(terminal.backend_mut()));
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
                Some(Ok(event)) => run.handle_terminal_input(
                    event,
                    terminal.backend_mut(),
                    &mut clipboard,
                    &mut delivery,
                )?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            };
            if let ControlFlow::Break(exit) = step {
                clipboard.shutdown();
                delivery.finish(&mut TerminalOutput(terminal.backend_mut()));
                return leave_run_loop(terminal, &run.application, exit);
            }
        }
    }
}

fn leave_run_loop(terminal: &mut TuiTerminal, application: &Application, exit: Exit) -> Result<()> {
    if exit == Exit::AfterFinalFrame {
        draw_frame(terminal, application.terminal_facts.hyperlinks, |frame| {
            application.render(frame)
        })?;
    }
    Ok(())
}

/// Draws one frame and ends it, so the frame reaches the terminal whole even
/// when the draw fails part way. Why the frame is bracketed, and why a failed
/// one is still ended, is on [`frame_backend::FrameBackend`]. The failure that
/// ended the frame is the one reported.
fn draw_frame<B: Backend + std::io::Write>(
    terminal: &mut Terminal<frame_backend::FrameBackend<B>>,
    hyperlinks: bool,
    render: impl FnOnce(&mut Frame),
) -> std::io::Result<()> {
    frame_backend::begin_frame(hyperlinks);
    let drawn = terminal.draw(render).map(|_| ());
    let ended = terminal.backend_mut().end_frame();
    drawn.and(ended)
}

impl RunLoop {
    fn handle_terminal_input(
        &mut self,
        input: TerminalInput,
        output: &mut impl std::io::Write,
        clipboard: &mut impl NativeClipboardSink,
        delivery: &mut ClipboardDelivery,
    ) -> Result<ControlFlow<Exit>> {
        match input {
            TerminalInput::Event(event) => {
                self.handle_input_event(event, &mut TerminalOutput(output), clipboard, delivery)
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
            TerminalInput::Graphics(reply) => {
                if let GraphicsReply::Version(version) = &reply {
                    tracing::debug!("the terminal names itself {version:?}");
                }
                let mut facts = self.application.terminal_facts;
                facts.merge_graphics(&reply);
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

    fn sync_subagent_tree(&mut self) {
        let wanted = self.application.subagent_tree_request();
        self.tasks
            .follow_subagent_tree(&self.client, wanted, &self.channels.subagent_trees);
    }

    fn receive_subagent_tree(
        &mut self,
        delivery: Option<SubagentTreeDelivery>,
    ) -> Result<ControlFlow<Exit>> {
        let delivery = delivery.ok_or_else(|| anyhow!("Subagent tree task channel stopped"))?;
        self.needs_redraw = true;
        let transition = self
            .application
            .handle_event(ApplicationEvent::SubagentTree {
                through: delivery.through,
                event: delivery.event,
            })?;
        Ok(self.dispatch_transition(transition))
    }

    fn handle_input_event(
        &mut self,
        event: InputEvent,
        output: &mut impl TerminalSink,
        clipboard: &mut impl NativeClipboardSink,
        delivery: &mut ClipboardDelivery,
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
            delivery.copy(output, clipboard, &text);
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
            ApplicationTransition::PreviewCheckoutRemoval { request_id, target } => {
                let outlook = self.application.outlook().clone();
                let commands = self.client.session_commands_for(outlook.clone());
                let results = self.channels.submissions.clone();
                tokio::spawn(async move {
                    let result = commands
                        .preview_checkout_removal(*target)
                        .await
                        .map(|preview| crate::protocol::RemoveCheckoutResult {
                            preview,
                            removed: false,
                            error: None,
                        })
                        .map_err(|e| e.to_string());
                    let _ = results.send(SubmissionResult::CheckoutRemoval {
                        outlook,
                        request_id,
                        result,
                    });
                });
            }
            ApplicationTransition::RemoveCheckout {
                request_id,
                request,
            } => {
                let outlook = self.application.outlook().clone();
                let commands = self.client.session_commands_for(outlook.clone());
                let results = self.channels.submissions.clone();
                tokio::spawn(async move {
                    let result = commands
                        .remove_checkout(*request)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = results.send(SubmissionResult::CheckoutRemoval {
                        outlook,
                        request_id,
                        result,
                    });
                });
            }
            ApplicationTransition::PrepareCheckout {
                attempt_id,
                prompt_id,
                request,
            } => {
                let outlook = self.application.outlook().clone();
                let commands = self.client.session_commands_for(outlook.clone());
                let results = self.channels.submissions.clone();
                tokio::spawn(async move {
                    let result = match commands.prepare_checkout(request).await {
                        Ok(result) => SubmissionResult::CheckoutPrepared {
                            outlook,
                            attempt_id,
                            prompt_id,
                            result,
                        },
                        Err(error) => SubmissionResult::CheckoutPreparationFailed {
                            outlook,
                            attempt_id,
                            prompt_id,
                            error: error.to_string(),
                        },
                    };
                    let _ = results.send(result);
                });
            }
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
            ApplicationTransition::SubmitDecision {
                session,
                id,
                decision,
            } => {
                let session_id = session.session_id;
                self.spawn_operation(
                    session,
                    SessionOperation::SubmitDecision {
                        session_id,
                        id,
                        decision,
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
            ApplicationTransition::SetSessionIcon { session, icon } => {
                let session_id = session.session_id;
                self.spawn_operation(
                    session,
                    SessionOperation::SetSessionIcon { session_id, icon },
                );
            }
            ApplicationTransition::SetWorkspaceIcon {
                origin,
                workspace_id,
                icon,
            } => {
                spawn_workspace_icon_set(
                    self.client.session_commands_for(origin.clone()),
                    origin,
                    workspace_id,
                    icon,
                    self.channels.submissions.clone(),
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
            ApplicationTransition::UpdateApprovalPosture { session, request } => {
                spawn_approval_posture_update(
                    self.client.session_commands_for(session.origin.clone()),
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
            ApplicationTransition::RemoveRemote(name) => {
                spawn_remote_removal(
                    self.client.session_commands(),
                    name,
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
                self.tasks.resolve_workspace(
                    self.client.session_commands_for(outlook.clone()),
                    outlook,
                    WorkspaceResolutionSurface::Outlook,
                    self.application
                        .pending_workspace_resolution(WorkspaceResolutionSurface::Outlook)
                        .expect("turning Outlook begins Workspace resolution"),
                    self.application.current_workspace_request(),
                    &self.channels.workspaces,
                );
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
            ApplicationTransition::OpenHyperlink(target) => {
                if let Err(error) = super::hyperlink::open(&target) {
                    tracing::warn!("could not open hyperlink: {error}");
                }
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
        // A TUI connecting is what asks the local server to discover the
        // Models it has not yet heard from this process; the answer lands as
        // Model Catalog events, so nothing waits on the request itself.
        if matches!(&event, ManagedEvent::Connected(_)) {
            let commands = self.client.session_commands();
            tokio::spawn(async move {
                if let Err(error) = commands.warm_models().await {
                    tracing::warn!("could not warm the Model Catalog on connect: {error:#}");
                }
            });
        }
        let transition = self
            .application
            .handle_event(ApplicationEvent::Managed(event))?;
        self.sync_reconnect_grace();
        match transition {
            ApplicationTransition::Continue => {}
            ApplicationTransition::SessionEnded => self.tasks.end_subscription(),
            // The shutdown state is worth one last frame before the screen goes.
            ApplicationTransition::Exit => return Ok(ControlFlow::Break(Exit::AfterFinalFrame)),
            // Connecting resolves the exact launch context on its owning
            // Server before subsequent Workspace filters use its identity.
            transition @ ApplicationTransition::ResolveWorkspace { .. } => {
                let _ = self.dispatch_transition(transition);
            }
            // Effective settings decide whether the Sidebar opens, and an
            // open Sidebar asks for the Sessions it lists.
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
            ApplicationTransition::PreviewCheckoutRemoval { .. }
            | ApplicationTransition::RemoveCheckout { .. }
            | ApplicationTransition::PrepareCheckout { .. }
            | ApplicationTransition::CreateSession(_)
            | ApplicationTransition::DetachSession
            | ApplicationTransition::DeleteSession(_)
            | ApplicationTransition::SettleSession { .. }
            | ApplicationTransition::SetSessionIcon { .. }
            | ApplicationTransition::SetWorkspaceIcon { .. }
            | ApplicationTransition::AdmitPrompt { .. }
            | ApplicationTransition::PromotePrompt { .. }
            | ApplicationTransition::CancelPrompt { .. }
            | ApplicationTransition::SubmitQuestionnaire { .. }
            | ApplicationTransition::SubmitDecision { .. }
            | ApplicationTransition::InterruptSession { .. }
            | ApplicationTransition::SubscribeSession(_)
            | ApplicationTransition::ViewSession(_)
            | ApplicationTransition::ViewAndAttachSession(_)
            | ApplicationTransition::AttachSession(_)
            | ApplicationTransition::ListModels(_)
            | ApplicationTransition::RefreshSkills(_)
            | ApplicationTransition::ConfirmLandingAgentSelection(_)
            | ApplicationTransition::UpdateAgentSelection { .. }
            | ApplicationTransition::UpdateApprovalPosture { .. }
            | ApplicationTransition::MutateSetting(_)
            | ApplicationTransition::BeginServing { .. }
            | ApplicationTransition::IssueInvite(_)
            | ApplicationTransition::CopyToClipboard(_)
            | ApplicationTransition::OpenHyperlink(_)
            | ApplicationTransition::RemovePeer(_)
            | ApplicationTransition::RemoveRemote(_)
            | ApplicationTransition::BeginConnecting
            | ApplicationTransition::PreviewInvite(_)
            | ApplicationTransition::RedeemInvite(_)
            | ApplicationTransition::TurnOutlook { .. }
            | ApplicationTransition::TurnOutlookAndViewAndAttach { .. }
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

    /// Keeps exactly one wakeup at the end of the Aside's quiet period for a
    /// tree still arriving, and none once it has passed or the tree is in.
    fn sync_tree_loading_delay(&mut self) {
        let wanted = self.application.subagent_tree_loading_deadline();
        let armed = self
            .tree_loading_delay
            .as_ref()
            .map(|(deadline, _)| *deadline);
        if armed == wanted {
            return;
        }
        self.tree_loading_delay = wanted.map(|deadline| {
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

    fn expire_reconnect_grace(&mut self, outlook: Outlook) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        self.reconnect_grace.retain(|(armed, _)| armed != &outlook);
        self.application
            .handle_event(ApplicationEvent::ReconnectGraceElapsed(outlook))?;
        Ok(ControlFlow::Continue(()))
    }

    fn sync_reconnect_grace(&mut self) {
        sync_reconnect_grace(
            &mut self.reconnect_grace,
            &self.application.origins_awaiting_grace(),
            RECONNECT_GRACE_PERIOD,
        );
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
            SubmissionResult::CheckoutRemoval {
                outlook,
                request_id,
                result,
            } => {
                if self.application.outlook() == &outlook {
                    self.application
                        .handle_event(ApplicationEvent::CheckoutRemoval { request_id, result })?;
                }
            }
            SubmissionResult::CheckoutPrepared {
                outlook,
                attempt_id,
                prompt_id,
                result,
            } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let transition =
                    self.application
                        .handle_event(ApplicationEvent::CheckoutPrepared {
                            attempt_id,
                            prompt_id,
                            result,
                        })?;
                return Ok(self.dispatch_transition(transition));
            }
            SubmissionResult::CheckoutPreparationFailed {
                outlook,
                attempt_id,
                prompt_id,
                error,
            } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::CheckoutPreparationFailed {
                        attempt_id,
                        prompt_id,
                        error,
                    })?;
            }
            SubmissionResult::SessionCreated { outlook, snapshot } => {
                if self.application.outlook() != &outlook {
                    return Ok(ControlFlow::Continue(()));
                }
                let session_id = snapshot.session.id;
                let transition = self
                    .application
                    .handle_event(ApplicationEvent::SessionCreated(*snapshot))?;
                // A reader who left this creation for another Session is
                // watching that one. Taking its stream away for a Session they
                // are not in would be the late answer pulling them back by
                // another route.
                if self
                    .application
                    .session_reference()
                    .map(|session| session.session_id)
                    == Some(session_id)
                {
                    self.tasks.resubscribe(
                        self.client.session_commands_for(outlook.clone()),
                        SessionReference::new(outlook, session_id),
                        &self.channels.subscriptions,
                    );
                }
                return Ok(self.dispatch_transition(transition));
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
                code,
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
                    None => ApplicationEvent::SessionCreationFailed {
                        prompt_id,
                        code,
                        error,
                    },
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
            SubmissionResult::ApprovalReconciled {
                session,
                id,
                snapshot,
                error,
            } => {
                self.application
                    .handle_event(ApplicationEvent::ApprovalSubmissionReconciled {
                        session,
                        id,
                        snapshot,
                        error,
                    })?;
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
            SubmissionResult::WorkspaceIconSetFailed { origin, error } => {
                if self.application.outlook() != &origin {
                    return Ok(ControlFlow::Continue(()));
                }
                self.application
                    .handle_event(ApplicationEvent::SessionOperationFailed(error))?;
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
        // An attach the reader moved on from is dropped whole, before
        // anything reads it: its snapshot never becomes the open Session, its
        // subscription goes with it rather than replacing the one still on
        // screen, and its failure is never drawn at them.
        if let SessionPickerResult::Attached { operation, .. }
        | SessionPickerResult::AttachFailed { operation, .. } = &result
            && !self.tasks.settle_attach(*operation).is_current()
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
            SessionPickerResult::Attached { .. } | SessionPickerResult::AttachFailed { .. } => true,
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
            SessionPickerResult::AttachFailed {
                reference, error, ..
            } => {
                let transition =
                    self.application
                        .handle_event(ApplicationEvent::OriginSessionAttachFailed {
                            reference,
                            error,
                        })?;
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
            PairingResult::RemoteRemoved { name, result } => {
                ApplicationEvent::RemoteRemoved { name, result }
            }
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
        let transition = self
            .application
            .handle_event(ApplicationEvent::OriginCatalog {
                outlook: event.outlook,
                event: event.event,
            })?;
        self.sync_reconnect_grace();
        Ok(self.dispatch_transition(transition))
    }
}

/// One event from the per-tree subscription, named by the Session it was
/// asked through so a late event from one the Aside has moved on from can be
/// told apart.
struct SubagentTreeDelivery {
    through: SessionReference,
    event: SubagentTreeEvent,
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
    result: std::result::Result<crate::protocol::ResolvedWorkspace, String>,
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
    RemoteRemoved {
        name: String,
        result: Result<crate::protocol::RemoteRemoval, String>,
    },
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

/// Ends the Pairing with one Remote. The local Server forgets it either way;
/// the answer says whether the Remote itself acknowledged in time.
fn spawn_remote_removal(
    commands: SessionCommandClient,
    name: String,
    results: UnboundedSender<PairingResult>,
) {
    tokio::spawn(async move {
        let result = commands
            .remove_remote(&name)
            .await
            .map_err(|error| error.to_string());
        let _ = results.send(PairingResult::RemoteRemoved { name, result });
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
        let result = deliver.await.unwrap_or_else(|error| {
            let code = error
                .downcast_ref::<crate::protocol::SessionError>()
                .map(|error| error.code);
            SubmissionResult::PromptDeliveryFailed {
                outlook,
                session,
                prompt_id,
                code,
                error: error.to_string(),
            }
        });
        let _ = results.send(result);
    });
}

enum SubmissionResult {
    CheckoutRemoval {
        outlook: Outlook,
        request_id: uuid::Uuid,
        result: Result<crate::protocol::RemoveCheckoutResult, String>,
    },
    CheckoutPrepared {
        outlook: Outlook,
        attempt_id: uuid::Uuid,
        prompt_id: PromptId,
        result: crate::protocol::PrepareCheckoutResult,
    },
    CheckoutPreparationFailed {
        outlook: Outlook,
        attempt_id: uuid::Uuid,
        prompt_id: PromptId,
        error: String,
    },
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
        code: Option<crate::protocol::SessionErrorCode>,
        error: String,
    },
    QuestionnaireReconciled {
        id: crate::protocol::QuestionnaireId,
        session: SessionReference,
        snapshot: Option<SessionSnapshot>,
        error: Option<String>,
    },
    ApprovalReconciled {
        id: crate::protocol::ApprovalId,
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
    /// A user's chosen Workspace Icon was refused or could not be sent. A
    /// success carries no result of its own: it reaches every client,
    /// including this one, through the ordinary catalog stream instead.
    WorkspaceIconSetFailed {
        origin: Outlook,
        error: String,
    },
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

fn spawn_approval_posture_update(
    commands: SessionCommandClient,
    session: SessionReference,
    request: UpdateApprovalPostureRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands
            .update_approval_posture(session.session_id, request)
            .await
        {
            Ok(_) => SubmissionResult::OperationSucceeded(session),
            Err(error) => SubmissionResult::OperationFailed {
                session,
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
        operation: AttachOperationId,
        snapshot: Box<SessionSnapshot>,
        subscription: SessionSubscription,
    },
    AttachFailed {
        reference: SessionReference,
        operation: AttachOperationId,
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
            .list_workspace_sessions(request.scope.workspace_filter())
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
fn spawn_session_attach(
    commands: SessionCommandClient,
    reference: SessionReference,
    operation: AttachOperationId,
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
        .unwrap_or_else(|error| SessionPickerResult::AttachFailed {
            reference,
            operation,
            error: error.to_string(),
        });
        let _ = results.send(result);
    })
}

enum SessionOperation {
    SubmitDecision {
        session_id: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    },
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
    SetSessionIcon {
        session_id: SessionId,
        icon: String,
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
            Self::SubmitDecision {
                session_id,
                id,
                decision,
            } => {
                let delivery = commands.submit_decision(session_id, id, decision).await;
                // The request can fail after server arbitration. Read the
                // authoritative lifecycle before enabling a retry so a stale
                // local Pending snapshot never resends a Decision.
                let snapshot = commands.read_session(session_id).await.ok();
                let outcome = snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .activities
                        .iter()
                        .rev()
                        .find_map(|activity| match activity {
                            crate::protocol::Activity::Approval {
                                approval, outcome, ..
                            } if approval.id == id => Some(*outcome),
                            _ => None,
                        })
                });
                let error = match outcome {
                    Some(crate::protocol::ApprovalOutcome::Decided) => {
                        delivery.err().map(|error| error.to_string())
                    }
                    Some(crate::protocol::ApprovalOutcome::DeliveryUncertain) => Some(
                        "Provider delivery is uncertain. This Decision will not be resent.".into(),
                    ),
                    Some(crate::protocol::ApprovalOutcome::SubmissionRejected) => {
                        Some("Decision was not delivered. Review and retry.".into())
                    }
                    Some(crate::protocol::ApprovalOutcome::Withdrawn) => {
                        Some("Approval was already resolved and is no longer available.".into())
                    }
                    _ if snapshot.is_none() => Some(
                        "Decision status is unconfirmed. Reconnect to check before retrying."
                            .into(),
                    ),
                    _ => delivery.err().map(|error| error.to_string()),
                };
                SubmissionResult::ApprovalReconciled {
                    session,
                    id,
                    snapshot,
                    error,
                }
            }
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
            // The catalog carries the resulting `TitleChanged` change to
            // every client, this one included, so nothing further is done
            // with the answered summary beyond reporting failure.
            Self::SetSessionIcon { session_id, icon } => operation_result(
                session,
                commands
                    .set_session_icon(session_id, &icon)
                    .await
                    .map(|_| ()),
            ),
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

/// Sends a user's chosen Workspace Icon to its own Origin. The catalog
/// carries the resulting `WorkspaceIconChanged` change to every client, this
/// one included, through the ordinary catalog stream, so nothing further is
/// done with a success beyond that; a failure is reported the same way a
/// Session operation's is, gated on the same Outlook a Session operation's
/// failure gates on.
fn spawn_workspace_icon_set(
    commands: SessionCommandClient,
    origin: Outlook,
    workspace_id: crate::protocol::WorkspaceId,
    icon: String,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        if let Err(error) = commands.set_workspace_icon(&workspace_id, &icon).await {
            let _ = results.send(SubmissionResult::WorkspaceIconSetFailed {
                origin,
                error: error.to_string(),
            });
        }
    });
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
/// cancel an attach that can still succeed.
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

/// Arms a grace period for every Origin still owed one, and gives up the one
/// held for any Origin no longer owed it — because it answered again, or
/// because its grace has already been served. Each loss is held back from the
/// frame for a grace of its own, once.
///
/// Held apart from the run loop so the whole arming-and-expiry path can be
/// driven at millisecond scale by a test that owns nothing else.
fn sync_reconnect_grace(
    armed: &mut Vec<(Outlook, Pin<Box<tokio::time::Sleep>>)>,
    awaiting: &BTreeSet<Outlook>,
    grace: Duration,
) {
    armed.retain(|(outlook, _)| awaiting.contains(outlook));
    for outlook in awaiting {
        if armed.iter().any(|(armed, _)| armed == outlook) {
            continue;
        }
        armed.push((outlook.clone(), Box::pin(tokio::time::sleep(grace))));
    }
}

/// The next grace period to come due. Every grace is the same length, so the
/// one armed first is always the one that fires first and waiting on the front
/// of the queue waits on all of them.
async fn wait_for_reconnect_grace(
    grace: &mut [(Outlook, Pin<Box<tokio::time::Sleep>>)],
) -> Outlook {
    match grace.first_mut() {
        Some((outlook, grace)) => {
            grace.as_mut().await;
            outlook.clone()
        }
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
    terminal: TuiTerminal,
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
    /// Report Clipboard delivery before attempting the optional primary selection.
    /// A queued sink owns the callback with the corresponding content.
    fn copy(&mut self, content: &ClipboardContent, completed: impl FnOnce(bool) + Send + 'static);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeClipboardTarget {
    Clipboard,
    #[cfg(target_os = "linux")]
    Primary,
}

/// The native writer is injectable so failure reporting can be exercised
/// through the sink without ever accessing a real Clipboard.
struct NativeClipboard<W> {
    write: W,
    failure_reported: bool,
}

impl<W: FnMut(NativeClipboardTarget, &ClipboardContent) -> Result<(), arboard::Error>>
    NativeClipboard<W>
{
    fn new(write: W) -> Self {
        Self {
            write,
            failure_reported: false,
        }
    }
}

impl<W: FnMut(NativeClipboardTarget, &ClipboardContent) -> Result<(), arboard::Error>>
    NativeClipboardSink for NativeClipboard<W>
{
    fn copy(&mut self, content: &ClipboardContent, completed: impl FnOnce(bool) + Send + 'static) {
        let result = (self.write)(NativeClipboardTarget::Clipboard, content);
        let delivered = result.is_ok();
        if let Err(error) = result {
            if self.failure_reported {
                tracing::debug!(%error, "could not write native Clipboard");
            } else {
                self.failure_reported = true;
                tracing::warn!(%error, "could not write native Clipboard");
            }
        }
        completed(delivered);
        // Clipboard delivery decides success; an unavailable primary selection
        // must not consume the run's first native Clipboard failure warning.
        #[cfg(target_os = "linux")]
        if let Err(error) =
            (self.write)(NativeClipboardTarget::Primary, &content.text.clone().into())
        {
            tracing::debug!(%error, "could not write native primary selection");
        }
    }
}

fn native_clipboard() -> impl NativeClipboardSink {
    // Keep the handle for the whole run: on Linux it owns the copied text.
    // Failed creation leaves it absent, so the next copy retries immediately.
    let mut clipboard = None;
    NativeClipboard::new(move |target, content: &ClipboardContent| {
        let clipboard = match &mut clipboard {
            Some(clipboard) => clipboard,
            slot @ None => slot.insert(arboard::Clipboard::new()?),
        };
        match target {
            NativeClipboardTarget::Clipboard => match &content.html {
                Some(html) => clipboard.set_html(html.as_str(), Some(content.text.as_str())),
                None => clipboard.set_text(content.text.as_str()),
            },
            #[cfg(target_os = "linux")]
            NativeClipboardTarget::Primary => {
                use arboard::{LinuxClipboardKind, SetExtLinux};

                clipboard
                    .set()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text(content.text.as_str())
            }
        }
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

/// Only the current copy may request terminal fallback. Replacing this receiver
/// cancels older callbacks without retaining an unbounded queue of completions.
struct ClipboardFallback {
    result: tokio::sync::oneshot::Receiver<bool>,
    text: String,
    deadline: tokio::time::Instant,
}

struct ClipboardDelivery {
    pending: Option<ClipboardFallback>,
    timeout: Duration,
}

impl Default for ClipboardDelivery {
    fn default() -> Self {
        Self {
            pending: None,
            timeout: Duration::from_millis(250),
        }
    }
}

impl ClipboardDelivery {
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn copy(
        &mut self,
        output: &mut impl TerminalSink,
        native: &mut impl NativeClipboardSink,
        content: &ClipboardContent,
    ) {
        self.pending = None;
        let (completed, result) = tokio::sync::oneshot::channel();
        let deadline = tokio::time::Instant::now() + self.timeout;
        native.copy(content, move |delivered| {
            let _ = completed.send(delivered);
        });
        if content.html.is_some() {
            self.pending = Some(ClipboardFallback {
                result,
                text: content.text.clone(),
                deadline,
            });
        } else {
            copy_to_terminal(output, &content.text);
        }
    }

    /// Called after the worker's bounded shutdown: do not lose a copy merely
    /// because exit was the next input event, and never wait a second deadline.
    fn finish(&mut self, output: &mut impl TerminalSink) {
        if let Some(mut copy) = self.pending.take()
            && !matches!(copy.result.try_recv(), Ok(true))
        {
            copy_to_terminal(output, &copy.text);
        }
    }

    /// Cancellation safe: select can poll this alongside input, then replace the
    /// pending copy before polling it again. No detached task writes to stdout.
    async fn fallback(&mut self) -> String {
        loop {
            let Some(copy) = &mut self.pending else {
                return pending().await;
            };
            let delivered = tokio::select! {
                biased;
                result = &mut copy.result => result.unwrap_or(false),
                () = tokio::time::sleep_until(copy.deadline) => {
                    tracing::debug!("native Clipboard delivery exceeded its fallback timeout");
                    false
                }
            };
            let copy = self.pending.take().unwrap();
            if !delivered {
                return copy.text;
            }
        }
    }
}

fn copy_to_terminal(output: &mut impl TerminalSink, text: &str) {
    if let Err(error) = ignore_unsupported(output.apply(CopyToClipboard(text))) {
        tracing::warn!(%error, "could not write Clipboard through OSC 52");
    }
}

#[cfg(test)]
fn copy_to_clipboard(
    output: &mut impl TerminalSink,
    native: &mut impl NativeClipboardSink,
    text: &str,
) {
    ClipboardDelivery::default().copy(output, native, &text.into());
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
        let mut terminal = Terminal::new(frame_backend::FrameBackend::new(CrosstermBackend::new(
            std::io::BufWriter::with_capacity(FRAME_BUFFER_CAPACITY, platform),
        )))?;
        if let Err(error) = enter_terminal_display(&mut TerminalOutput(terminal.backend_mut())) {
            return Err(error.into());
        }
        // Entry always re-asserts the cursor: it wrote Hide past the memory, and
        // the terminal may have been handed back with any cursor at all.
        terminal.backend_mut().forget_cursor();
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
    use crossterm::{
        Command,
        cursor::{Hide, MoveTo, Show},
        style::Print,
        terminal::{Clear, ClearType},
    };
    use ratatui::{
        Terminal,
        backend::{Backend, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
    };

    use super::{
        Application, ApplicationEvent, ClipboardContent, DisableMouseButtonReporting,
        EnableMouseButtonReporting, FRAME_BUFFER_CAPACITY, NativeClipboard, NativeClipboardSink,
        PopModifiedKeyReporting, PushModifiedKeyReporting, TerminalSink, copy_to_clipboard,
        draw_frame, enter_terminal_display,
        frame_backend::{FrameBackend, ansi, finish_frame, register_hyperlink},
        ignore_unsupported, leave_terminal_display, remote_failure_from_session_error,
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
            self.0.push_str(&ansi(&command)?);
            Ok(())
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

    /// A ratatui backend that draws a frame as ANSI bytes through a sink, for
    /// the same cross-platform reason [`AnsiTranscript`] exists. The cursor
    /// methods flush the sink the way `CrosstermBackend` does (it writes them
    /// with `execute!`), so a write count taken through this backend is
    /// faithful to the production one.
    struct AnsiTranscriptBackend<W = Vec<u8>> {
        sink: W,
        refuses_to_draw: bool,
    }

    impl AnsiTranscriptBackend {
        fn new() -> Self {
            Self::over(Vec::new())
        }

        /// Stands in for a terminal that goes away mid-frame.
        fn refusing_to_draw() -> Self {
            Self {
                refuses_to_draw: true,
                ..Self::new()
            }
        }

        fn transcript(&self) -> &str {
            std::str::from_utf8(&self.sink).expect("ANSI transcripts are UTF-8")
        }
    }

    impl<W: std::io::Write> AnsiTranscriptBackend<W> {
        const SIZE: Size = Size {
            width: 20,
            height: 5,
        };

        fn over(sink: W) -> Self {
            Self {
                sink,
                refuses_to_draw: false,
            }
        }

        fn queue(&mut self, command: impl Command) -> std::io::Result<()> {
            self.sink.write_all(ansi(&command)?.as_bytes())
        }

        fn execute(&mut self, command: impl Command) -> std::io::Result<()> {
            self.queue(command)?;
            self.sink.flush()
        }
    }

    impl<W: std::io::Write> Backend for AnsiTranscriptBackend<W> {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            if self.refuses_to_draw {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            for (column, row, cell) in content {
                self.queue(MoveTo(column, row))?;
                self.queue(Print(cell.symbol().to_owned()))?;
            }
            Ok(())
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.execute(Hide)
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.execute(Show)
        }

        fn get_cursor_position(&mut self) -> std::io::Result<Position> {
            Ok(Position::new(0, 0))
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
            let position = position.into();
            self.execute(MoveTo(position.x, position.y))
        }

        fn clear(&mut self) -> std::io::Result<()> {
            self.execute(Clear(ClearType::All))
        }

        fn size(&self) -> std::io::Result<Size> {
            Ok(Self::SIZE)
        }

        fn window_size(&mut self) -> std::io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: Self::SIZE,
                pixels: Size {
                    width: 0,
                    height: 0,
                },
            })
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.sink.flush()
        }
    }

    impl<W: std::io::Write> std::io::Write for AnsiTranscriptBackend<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sink.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.sink.flush()
        }
    }

    /// Counts the writes a console would receive and keeps nothing else.
    #[derive(Default)]
    struct CountingWriter {
        writes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl CountingWriter {
        fn counter(&self) -> std::rc::Rc<std::cell::Cell<usize>> {
            std::rc::Rc::clone(&self.writes)
        }
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.set(self.writes.get() + 1);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A frame with several changed runs and a caret, the shape a spinner
    /// frame has.
    fn a_busy_frame(frame: &mut ratatui::Frame, text: &'static str) {
        frame.render_widget(text, frame.area());
        frame.set_cursor_position(Position::new(3, 2));
    }

    /// termina's own writer is 128 bytes on Windows, so a several-KiB frame
    /// became dozens of WriteFile calls and the terminal could repaint between
    /// any two of them. With a frame-sized buffer and one flush per frame the
    /// whole frame -- bracket, diff, cursor -- is one write; without the
    /// buffer the same frame is several, so it is the buffering that
    /// collapses it.
    #[test]
    fn a_frame_reaches_the_terminal_as_one_write() {
        let console = CountingWriter::default();
        let writes = console.counter();
        let buffered = std::io::BufWriter::with_capacity(FRAME_BUFFER_CAPACITY, console);
        let mut terminal = Terminal::new(FrameBackend::new(AnsiTranscriptBackend::over(buffered)))
            .expect("a counting Terminal always starts");

        draw_frame(&mut terminal, false, |frame| {
            a_busy_frame(frame, "suru\nis\nhere")
        })
        .expect("a counting console never fails");
        assert_eq!(writes.get(), 1, "a busy frame took more than one write");

        draw_frame(&mut terminal, false, |frame| {
            a_busy_frame(frame, "suru\nis\nhere")
        })
        .expect("a counting console never fails");
        assert_eq!(writes.get(), 2, "an idle frame took more than one write");

        let console = CountingWriter::default();
        let writes = console.counter();
        let mut unbuffered = Terminal::new(FrameBackend::new(AnsiTranscriptBackend::over(console)))
            .expect("a counting Terminal always starts");
        draw_frame(&mut unbuffered, false, |frame| {
            a_busy_frame(frame, "suru\nis\nhere")
        })
        .expect("a counting console never fails");
        assert!(
            writes.get() > 1,
            "an unbuffered frame collapsed to one write on its own, so the buffer proves nothing"
        );
    }

    /// The frame -- cell writes and the cursor trip they end with -- has to
    /// reach the terminal inside one synchronized update, or the cursor is
    /// visibly parked on the animation cell until the frame moves it back.
    #[test]
    fn a_frame_is_drawn_inside_one_synchronized_update() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());

        draw_frame(&mut terminal, false, |frame| {
            frame.render_widget("suru", frame.area());
            frame.set_cursor_position(Position::new(3, 2));
        })
        .expect("an ANSI transcript never fails to record");

        let transcript = terminal.backend().inner().transcript();
        assert!(
            transcript.starts_with("\x1b[?2026h"),
            "the frame did not open a synchronized update: {transcript:?}"
        );
        assert!(
            transcript.ends_with("\x1b[?2026l"),
            "the frame did not close its synchronized update: {transcript:?}"
        );
        let positions = AnsiTranscript::positions(
            transcript,
            &[
                "\x1b[?2026h", // synchronized update opened
                "\x1b[1;1H",   // the frame's first painted cell
                "\x1b[?25h",   // cursor shown
                "\x1b[3;4H",   // cursor parked at the composer caret
                "\x1b[?2026l", // synchronized update closed
            ],
        );
        assert!(
            positions.is_sorted(),
            "the frame was drawn out of order: {transcript:?}"
        );
    }

    /// A frame that fails part way through still has to close its synchronized
    /// update: a terminal left inside mode 2026 shows nothing further until its
    /// own timeout expires, including the sequences that restore the display.
    /// What reached the terminal before the failure -- the Hide ahead of the
    /// diff -- is exactly what the memory keeps, so the next frame recovers.
    #[test]
    fn a_frame_that_fails_to_draw_still_closes_its_synchronized_update() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::refusing_to_draw());

        let failure = draw_frame(&mut terminal, false, |frame| {
            frame.render_widget("suru", frame.area());
            frame.set_cursor_position(Position::new(3, 2));
        })
        .expect_err("a terminal that refuses to draw fails the frame");

        assert_eq!(
            failure.kind(),
            std::io::ErrorKind::BrokenPipe,
            "the failure the draw reported is the one that survives: {failure}"
        );
        let transcript = terminal.backend().inner().transcript();
        assert!(
            transcript.starts_with("\x1b[?2026h"),
            "the frame did not open a synchronized update: {transcript:?}"
        );
        assert_eq!(
            transcript, "\x1b[?2026h\x1b[?25l\x1b[?2026l",
            "the failed frame left the terminal inside a synchronized update: {transcript:?}"
        );
    }

    type RecordingTerminal = Terminal<FrameBackend<AnsiTranscriptBackend>>;

    fn recording_terminal(backend: AnsiTranscriptBackend) -> RecordingTerminal {
        Terminal::new(FrameBackend::new(backend)).expect("a recording Terminal always starts")
    }

    /// Draws one frame and returns only what that frame wrote, so a test can
    /// look at a later frame without the first frame's full paint in the way.
    fn draw_recorded_frame(
        terminal: &mut RecordingTerminal,
        caret: Option<Position>,
        text: &'static str,
    ) -> String {
        let before = terminal.backend().inner().transcript().len();
        draw_frame(terminal, false, |frame| {
            frame.render_widget(text, frame.area());
            if let Some(caret) = caret {
                frame.set_cursor_position(caret);
            }
        })
        .expect("an ANSI transcript never fails to record");
        terminal.backend().inner().transcript()[before..].to_owned()
    }

    /// Terminals restart the blink phase whenever the cursor is shown or moved,
    /// and the run loop redraws every 32 ms while anything animates. A frame
    /// that changes nothing has to say nothing about the cursor, or the
    /// composer caret never gets to its dark phase.
    #[test]
    fn a_redraw_with_the_same_content_and_caret_writes_nothing_but_the_synchronized_update() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let caret = Some(Position::new(3, 2));

        draw_recorded_frame(&mut terminal, caret, "suru");
        let second = draw_recorded_frame(&mut terminal, caret, "suru");

        assert_eq!(
            second, "\x1b[?2026h\x1b[?2026l",
            "an unchanged frame re-emitted cursor traffic"
        );
    }

    fn draw_hyperlink_frame(
        terminal: &mut RecordingTerminal,
        enabled: bool,
        text: &'static str,
        target: Option<&str>,
    ) -> String {
        let before = terminal.backend().inner().transcript().len();
        draw_frame(terminal, enabled, |frame| {
            frame.render_widget(text, frame.area());
            if let Some(target) = target {
                let width =
                    u16::try_from(unicode_width::UnicodeWidthStr::width(text)).unwrap_or(u16::MAX);
                register_hyperlink(frame.buffer_mut(), 0, 0, width, target);
            }
            finish_frame(frame.buffer_mut(), true);
        })
        .expect("an ANSI transcript never fails to record");
        terminal.backend().inner().transcript()[before..].to_owned()
    }

    #[test]
    fn hyperlink_cells_emit_osc8_and_idle_frames_reuse_the_terminal_state() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let first = draw_hyperlink_frame(
            &mut terminal,
            true,
            "link",
            Some("https://example.test/one"),
        );
        assert!(first.contains("\x1b]8;;https://example.test/one\x1b\\"));
        assert!(first.contains("\x1b]8;;\x1b\\"));

        let idle = draw_hyperlink_frame(
            &mut terminal,
            true,
            "link",
            Some("https://example.test/one"),
        );
        assert_eq!(idle, "\x1b[?2026h\x1b[?2026l");
    }

    #[test]
    fn changing_or_removing_only_a_target_repaints_its_visible_cell() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        draw_hyperlink_frame(&mut terminal, true, "界", Some("https://example.test/one"));

        let changed =
            draw_hyperlink_frame(&mut terminal, true, "界", Some("https://example.test/two"));
        assert!(changed.contains("\x1b]8;;https://example.test/two\x1b\\"));
        assert!(changed.contains('界'));
        assert!(
            !changed.contains("\x1b[1;2H"),
            "wide trailing cell was repainted"
        );

        let removed = draw_hyperlink_frame(&mut terminal, true, "界", None);
        assert!(removed.contains('界'));
        assert!(!removed.contains("\x1b]8;;https://"));
    }

    #[test]
    fn removing_a_link_cannot_repaint_over_a_new_wide_glyph() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        draw_hyperlink_frame(&mut terminal, true, "ab", Some("https://example.test/one"));

        let replaced = draw_hyperlink_frame(&mut terminal, true, "界", None);
        assert!(replaced.contains('界'));
        assert!(!replaced.contains("\x1b[1;2H"));
        assert!(!replaced.contains('b'));
    }

    #[test]
    fn an_occluding_wide_glyph_also_clears_the_link_without_overlap() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        draw_hyperlink_frame(&mut terminal, true, "ab", Some("https://example.test/one"));
        let before = terminal.backend().inner().transcript().len();
        draw_frame(&mut terminal, true, |frame| {
            frame.render_widget("界", frame.area());
            finish_frame(frame.buffer_mut(), false);
        })
        .unwrap();
        let replaced = &terminal.backend().inner().transcript()[before..];
        assert!(replaced.contains('界'));
        assert!(!replaced.contains("\x1b[1;2H"));
        assert!(!replaced.contains('b'));
    }

    #[test]
    fn ordinary_cells_keep_ratatuis_row_major_draw_order() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let before = terminal.backend().inner().transcript().len();
        draw_frame(&mut terminal, false, |frame| {
            for (x, y, symbol) in [(0, 0, "a"), (1, 0, "b"), (0, 1, "c"), (1, 1, "d")] {
                frame.buffer_mut()[(x, y)].set_symbol(symbol);
            }
            finish_frame(frame.buffer_mut(), true);
        })
        .unwrap();
        let frame = &terminal.backend().inner().transcript()[before..];
        let positions =
            AnsiTranscript::positions(frame, &["\x1b[1;1H", "\x1b[1;2H", "\x1b[2;1H", "\x1b[2;2H"]);
        assert!(
            positions.is_sorted(),
            "cell diff became column-major: {frame:?}"
        );
    }

    #[test]
    fn unsupported_terminals_emit_no_hyperlink_sequences() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let frame = draw_hyperlink_frame(
            &mut terminal,
            false,
            "link",
            Some("https://example.test/one"),
        );
        assert!(!frame.contains("\x1b]8;;"));
    }

    /// Hide and Show are transitions, not per-frame decorations: once the first
    /// paint has settled, three frames that toggle the caret off and on again
    /// cost one Hide and one Show.
    #[test]
    fn cursor_visibility_is_only_written_when_it_changes() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let caret = Some(Position::new(3, 2));
        // The first paint hides an unknown cursor across its repaint; the
        // transitions are counted from a settled screen.
        draw_recorded_frame(&mut terminal, caret, "suru");

        let mut frames = String::new();
        frames += &draw_recorded_frame(&mut terminal, caret, "suru");
        frames += &draw_recorded_frame(&mut terminal, None, "suru");
        frames += &draw_recorded_frame(&mut terminal, caret, "suru");

        assert_eq!(
            frames.matches("\x1b[?25l").count(),
            1,
            "Hide was written other than on the one transition: {frames:?}"
        );
        assert_eq!(
            frames.matches("\x1b[?25h").count(),
            1,
            "Show was written other than on the one transition: {frames:?}"
        );
    }

    /// Display entry writes its own Hide past the memory, and a resumed
    /// terminal may have been handed back with any cursor, so the first frame
    /// after forgetting re-asserts both visibility and position.
    #[test]
    fn forgetting_the_cursor_makes_the_next_frame_re_assert_it() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let caret = Some(Position::new(3, 2));

        draw_recorded_frame(&mut terminal, caret, "suru");
        terminal.backend_mut().forget_cursor();
        let resumed = draw_recorded_frame(&mut terminal, caret, "suru");

        assert!(
            resumed.contains("\x1b[?25h"),
            "the first frame after forgetting did not show the cursor: {resumed:?}"
        );
        assert!(
            resumed.contains("\x1b[3;4H"),
            "the first frame after forgetting did not park the caret: {resumed:?}"
        );
    }

    /// Typing moves the caret without changing the visible cells: only the
    /// move is written, since the cursor is already shown.
    #[test]
    fn a_moved_caret_over_unchanged_content_writes_only_the_move() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());

        draw_recorded_frame(&mut terminal, Some(Position::new(3, 2)), "suru");
        let moved = draw_recorded_frame(&mut terminal, Some(Position::new(4, 2)), "suru");

        assert!(
            moved.contains("\x1b[3;5H"),
            "the caret was not moved to its new cell: {moved:?}"
        );
        assert!(
            !moved.contains("\x1b[?25h"),
            "an already shown cursor was shown again: {moved:?}"
        );
    }

    /// On a terminal that ignores mode 2026 the diff is presented as it is
    /// written, so a shown cursor is dragged across every repainted cell. The
    /// cursor is hidden ahead of the diff and shown again once, after it, at
    /// the caret -- and no more than once each.
    #[test]
    fn a_changed_diff_hides_the_cursor_across_the_repaint_and_shows_it_once_at_the_caret() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());
        let caret = Some(Position::new(3, 2));

        draw_recorded_frame(&mut terminal, caret, "suru");
        let repainted = draw_recorded_frame(&mut terminal, caret, "sura");

        let positions = AnsiTranscript::positions(
            &repainted,
            &[
                "\x1b[?2026h", // synchronized update opened
                "\x1b[?25l",   // cursor hidden ahead of the diff
                "\x1b[1;4H",   // the changed cell repainted
                "\x1b[?25h",   // cursor shown again after the diff
                "\x1b[3;4H",   // cursor parked at the composer caret
                "\x1b[?2026l", // synchronized update closed
            ],
        );
        assert!(
            positions.is_sorted(),
            "the repaint was not bracketed by Hide and Show: {repainted:?}"
        );
        assert_eq!(
            repainted.matches("\x1b[?25l").count(),
            1,
            "the cursor was hidden more than once in one frame: {repainted:?}"
        );
        assert_eq!(
            repainted.matches("\x1b[?25h").count(),
            1,
            "the cursor was shown more than once in one frame: {repainted:?}"
        );
    }

    /// A surface without a caret hides the cursor once; later repaints find
    /// it already hidden and have nothing to say about it.
    #[test]
    fn repaints_without_a_caret_hide_the_cursor_once_and_never_show_it() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());

        let mut frames = draw_recorded_frame(&mut terminal, None, "suru");
        frames += &draw_recorded_frame(&mut terminal, None, "sura");

        assert_eq!(
            frames.matches("\x1b[?25l").count(),
            1,
            "a hidden cursor was hidden again: {frames:?}"
        );
        assert!(
            !frames.contains("\x1b[?25h"),
            "a caret-less surface showed the cursor: {frames:?}"
        );
    }

    /// Before the first frame nothing is known about the cursor, and a cursor
    /// that might be shown is treated as shown: the first repaint is hidden
    /// too.
    #[test]
    fn the_first_frame_hides_an_unknown_cursor_ahead_of_its_repaint() {
        let mut terminal = recording_terminal(AnsiTranscriptBackend::new());

        let first = draw_recorded_frame(&mut terminal, Some(Position::new(3, 2)), "suru");

        let positions = AnsiTranscript::positions(
            &first,
            &[
                "\x1b[?25l", // cursor hidden ahead of the diff
                "\x1b[1;1H", // the frame's first painted cell
                "\x1b[?25h", // cursor shown again after the diff
            ],
        );
        assert!(
            positions.is_sorted(),
            "the first repaint was not hidden: {first:?}"
        );
    }

    /// Synchronized output is per frame. Entering or leaving the display with
    /// mode 2026 still on would hold every later repaint hostage.
    #[test]
    fn synchronized_output_is_never_part_of_entering_or_leaving_the_display() {
        for transcript in [
            AnsiTranscript::record(enter_terminal_display),
            AnsiTranscript::record(leave_terminal_display),
        ] {
            assert!(
                !transcript.contains("?2026"),
                "synchronized output leaked into the display lifecycle: {transcript:?}"
            );
        }
    }

    #[test]
    fn a_stalled_native_write_leaves_copy_responsive_and_only_keeps_the_latest_waiting_text() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;

        let (started, writing) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Arc::new(Mutex::new(resume));
        let (written, copies) = mpsc::channel();
        let mut native = super::clipboard_thread::ClipboardThread::new(move || {
            let started = started.clone();
            let resume = resume.clone();
            let written = written.clone();
            NativeClipboard::new(move |target, content: &ClipboardContent| {
                let text = content.text.as_str();
                if target == super::NativeClipboardTarget::Clipboard {
                    if text == "first" {
                        started.send(()).unwrap();
                        let _ = resume.lock().unwrap().recv();
                    }
                    written.send(text.to_owned()).unwrap();
                }
                Ok(())
            })
        })
        .with_exit_timeout(Duration::from_millis(100));
        let mut terminal = AnsiTranscript(String::new());
        copy_to_clipboard(&mut terminal, &mut native, "first");
        writing.recv_timeout(Duration::from_secs(1)).unwrap();
        copy_to_clipboard(&mut terminal, &mut native, "second");
        copy_to_clipboard(&mut terminal, &mut native, "latest");
        assert_eq!(
            terminal.0,
            "\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07\x1b]52;c;bGF0ZXN0\x07"
        );
        assert!(copies.try_recv().is_err());

        release.send(()).unwrap();
        assert!(native.shutdown());
        assert_eq!(copies.try_iter().collect::<Vec<_>>(), ["first", "latest"]);
    }

    #[tokio::test]
    async fn rich_native_success_keeps_html_and_does_not_write_terminal_text() {
        let content = crate::tui::ClipboardContent {
            text: "**chosen**".into(),
            html: Some("<p><strong>chosen</strong></p>\n".into()),
        };
        let mut offered = Vec::new();
        let mut terminal = AnsiTranscript(String::new());
        let mut delivery = super::ClipboardDelivery::default();
        let mut native = NativeClipboard::new(|target, content: &crate::tui::ClipboardContent| {
            offered.push((target, content.clone()));
            Ok(())
        });
        delivery.copy(&mut terminal, &mut native, &content);
        assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
        assert!(terminal.0.is_empty());
        assert_eq!(
            offered[0],
            (super::NativeClipboardTarget::Clipboard, content)
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            offered[1],
            (super::NativeClipboardTarget::Primary, "**chosen**".into())
        );
    }

    #[tokio::test]
    async fn rich_failure_falls_back_to_markdown_and_terminal_failure_is_log_only() {
        let content = ClipboardContent {
            text: "**chosen**".into(),
            html: Some("<strong>chosen</strong>".into()),
        };
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::Unsupported,
        ] {
            let mut native = NativeClipboard::new(|_, _: &ClipboardContent| {
                Err(arboard::Error::ClipboardNotSupported)
            });
            let mut delivery = super::ClipboardDelivery::default()
                .with_timeout(std::time::Duration::from_millis(5));
            let mut terminal = AnsiTranscript(String::new());
            delivery.copy(&mut terminal, &mut native, &content);
            assert!(terminal.0.is_empty());
            let text = delivery.fallback().await;
            super::copy_to_terminal(&mut terminal, &text);
            assert_eq!(terminal.0, "\x1b]52;c;KipjaG9zZW4qKg==\x07");
            let log = record_log(|| super::copy_to_terminal(&mut FailedTerminal(kind), &text));
            assert_eq!(
                log.contains("WARN"),
                kind != std::io::ErrorKind::Unsupported
            );
            assert!(!log.contains("chosen"));
        }
    }

    #[tokio::test]
    async fn superseded_rich_completions_cannot_fall_back_and_pending_formats_stay_paired() {
        use std::sync::{Arc, Mutex, mpsc};
        for latest_is_rich in [true, false] {
            let (entered, writing) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let resume = Arc::new(Mutex::new(resume));
            let (offered, copies) = mpsc::channel();
            let mut native = super::clipboard_thread::ClipboardThread::new(move || {
                let entered = entered.clone();
                let resume = resume.clone();
                let offered = offered.clone();
                NativeClipboard::new(move |target, content: &ClipboardContent| {
                    if target != super::NativeClipboardTarget::Clipboard {
                        return Ok(());
                    }
                    offered.send(content.clone()).unwrap();
                    if content.text == "first" {
                        entered.send(()).unwrap();
                        let _ = resume.lock().unwrap().recv();
                        Err(arboard::Error::ClipboardNotSupported)
                    } else {
                        Ok(())
                    }
                })
            })
            .with_exit_timeout(std::time::Duration::from_millis(100));
            let first = ClipboardContent {
                text: "first".into(),
                html: Some("<p>first</p>".into()),
            };
            let replaced = ClipboardContent {
                text: "replaced".into(),
                html: Some("<h1>replaced</h1>".into()),
            };
            let latest = ClipboardContent {
                text: "latest".into(),
                html: latest_is_rich.then(|| "<em>latest</em>".into()),
            };
            let mut delivery = super::ClipboardDelivery::default()
                .with_timeout(std::time::Duration::from_millis(5));
            let mut terminal = AnsiTranscript(String::new());
            delivery.copy(&mut terminal, &mut native, &first);
            writing
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            delivery.copy(&mut terminal, &mut native, &replaced);
            delivery.copy(&mut terminal, &mut native, &latest);
            release.send(()).unwrap();
            assert!(native.shutdown());
            assert_eq!(copies.try_iter().collect::<Vec<_>>(), [first, latest]);
            assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
            assert_eq!(
                terminal.0,
                if latest_is_rich {
                    ""
                } else {
                    "\x1b]52;c;bGF0ZXN0\x07"
                }
            );
        }
    }

    #[tokio::test]
    async fn stalled_rich_native_access_has_bounded_fallback_and_shutdown() {
        use std::sync::{Arc, Mutex, mpsc};
        for stall_creation in [true, false] {
            let (entered, stalled) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let resume = Arc::new(Mutex::new(resume));
            let (done, finished) = mpsc::channel();
            let mut native = super::clipboard_thread::ClipboardThread::new(move || {
                if stall_creation {
                    entered.send(()).unwrap();
                    let _ = resume.lock().unwrap().recv();
                }
                let entered = entered.clone();
                let resume = resume.clone();
                let done = done.clone();
                NativeClipboard::new(move |target, _: &ClipboardContent| {
                    if target == super::NativeClipboardTarget::Clipboard {
                        if !stall_creation {
                            entered.send(()).unwrap();
                            let _ = resume.lock().unwrap().recv();
                        }
                        done.send(()).unwrap();
                        return Err(arboard::Error::ClipboardNotSupported);
                    }
                    Ok(())
                })
            })
            .with_exit_timeout(std::time::Duration::from_millis(5));
            let mut delivery = super::ClipboardDelivery::default()
                .with_timeout(std::time::Duration::from_millis(5));
            let mut terminal = AnsiTranscript(String::new());
            let content = ClipboardContent {
                text: "**chosen**".into(),
                html: Some("<strong>chosen</strong>".into()),
            };
            delivery.copy(&mut terminal, &mut native, &content);
            stalled
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let text = tokio::time::timeout(std::time::Duration::from_secs(1), delivery.fallback())
                .await
                .unwrap();
            super::copy_to_terminal(&mut terminal, &text);
            assert_eq!(terminal.0, "\x1b]52;c;KipjaG9zZW4qKg==\x07");
            let began = std::time::Instant::now();
            assert!(!native.shutdown());
            assert!(began.elapsed() < std::time::Duration::from_millis(120));
            release.send(()).unwrap();
            finished
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rich_success_is_reported_before_a_stalled_or_unsupported_primary_selection() {
        use std::sync::{Arc, Mutex, mpsc};
        let (entered, stalled) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Arc::new(Mutex::new(resume));
        let mut native = super::clipboard_thread::ClipboardThread::new(move || {
            let entered = entered.clone();
            let resume = resume.clone();
            NativeClipboard::new(move |target, content: &ClipboardContent| {
                if target == super::NativeClipboardTarget::Primary {
                    assert_eq!(content, &ClipboardContent::from("chosen"));
                    entered.send(()).unwrap();
                    let _ = resume.lock().unwrap().recv();
                    return Err(arboard::Error::ClipboardNotSupported);
                }
                Ok(())
            })
        })
        .with_exit_timeout(std::time::Duration::from_millis(100));
        let mut delivery =
            super::ClipboardDelivery::default().with_timeout(std::time::Duration::ZERO);
        let mut terminal = AnsiTranscript(String::new());
        delivery.copy(
            &mut terminal,
            &mut native,
            &ClipboardContent {
                text: "chosen".into(),
                html: Some("<p>chosen</p>".into()),
            },
        );
        stalled
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
        release.send(()).unwrap();
        assert!(native.shutdown());
        assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
        assert!(terminal.0.is_empty());
    }

    #[tokio::test]
    async fn exiting_after_rich_copy_preserves_success_or_flushes_the_latest_fallback() {
        for delivered in [true, false] {
            let mut native = super::clipboard_thread::ClipboardThread::new(move || {
                NativeClipboard::new(move |_, _: &ClipboardContent| {
                    if delivered {
                        Ok(())
                    } else {
                        Err(arboard::Error::ClipboardNotSupported)
                    }
                })
            })
            .with_exit_timeout(std::time::Duration::from_millis(100));
            let mut delivery = super::ClipboardDelivery::default();
            let mut terminal = AnsiTranscript(String::new());
            delivery.copy(
                &mut terminal,
                &mut native,
                &ClipboardContent {
                    text: "chosen".into(),
                    html: Some("<p>chosen</p>".into()),
                },
            );
            assert!(native.shutdown());
            delivery.finish(&mut terminal);
            assert_eq!(
                terminal.0,
                if delivered {
                    ""
                } else {
                    "\x1b]52;c;Y2hvc2Vu\x07"
                }
            );
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

    #[tokio::test]
    async fn rendered_markdown_selection_reaches_clipboard_destinations_in_both_copy_modes() {
        use crate::protocol::{
            Message, MessageId, MessageRole, MessageStatus, ModelAvailability, Session, SessionId,
            SessionRevision, SessionSnapshot, SessionStatus, SidebarVisibility, TextSelectionCopy,
            TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
        };
        use crate::tui::{ApplicationTransition, CommandId, SemanticCommandId};
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        for mode in [TextSelectionCopy::Manual, TextSelectionCopy::Release] {
            let mut application = Application::new(&workspace, Default::default());
            let mut settings = EffectiveSettings::default();
            settings.text_selection.copy = mode;
            settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
            application
                .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
                    SettingsSnapshot {
                        settings,
                        pinned: vec![],
                        diagnostics: vec![],
                    },
                )))
                .unwrap();
            let message_id = MessageId::new();
            let turn_id = TurnId::new();
            application
                .handle_event(ApplicationEvent::SessionAttached(SessionSnapshot {
                    title: String::new(),
                    icon: None,
                    session: Session {
                        checkout: None,
                        context_fill: None,
                        id: SessionId::new(),
                        execution_directory: crate::protocol::ExecutionDirectory {
                            path: workspace.clone(),
                        },
                        workspace: Workspace::directory(workspace.clone()),
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        approval_posture: None,
                        status: SessionStatus::Idle,
                        working_since: None,
                        monitoring_since: None,
                        parent: None,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: vec![],
                    turns: vec![Turn {
                        id: turn_id,
                        prompt_id: None,
                        agent: None,
                        status: TurnStatus::Completed,
                        started_at: None,
                        settled_at: None,
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
                    }],
                    messages: vec![Message {
                        id: message_id,
                        turn_id,
                        role: MessageRole::Agent,
                        status: MessageStatus::Completed,
                        content: "**chosen words**".into(),
                        skill_invocations: vec![],
                        attachments: Vec::new(),
                        truncated: false,
                    }],
                    activities: vec![],
                    transcript: vec![TranscriptItem::Message { message_id }],
                    subagent_interventions: vec![],
                    pending_approvals: Vec::new(),
                    submitting_approvals: Vec::new(),
                    pending_approvals_revision: crate::protocol::SessionRevision(0),
                    watches: Vec::new(),
                    subagent_usage: None,
                    total_cost: None,
                }))
                .unwrap();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|frame| application.render(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let start = (0..24)
                .find_map(|y| {
                    let row = (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                    row.find("chosen words").map(|x| (x as u16, y))
                })
                .expect("the Agent Message is painted");
            let mut released = ApplicationTransition::Continue;
            for (kind, column) in [
                (MouseEventKind::Down(MouseButton::Left), start.0),
                (MouseEventKind::Drag(MouseButton::Left), start.0 + 5),
                (MouseEventKind::Up(MouseButton::Left), start.0 + 5),
            ] {
                released = application
                    .handle_terminal_event(Event::Mouse(MouseEvent {
                        kind,
                        column,
                        row: start.1,
                        modifiers: KeyModifiers::NONE,
                    }))
                    .unwrap();
            }
            let transition = if mode == TextSelectionCopy::Manual {
                assert_eq!(released, ApplicationTransition::Continue);
                application
                    .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                        SemanticCommandId::TextSelectionCopy,
                    )))
                    .unwrap()
            } else {
                released
            };
            let ApplicationTransition::CopyToClipboard(text) = transition else {
                panic!("expected a copy")
            };
            let mut offered = Vec::new();
            let mut ansi = AnsiTranscript(String::new());
            {
                let mut native = NativeClipboard::new(|target, content: &ClipboardContent| {
                    offered.push((target, content.clone()));
                    Ok(())
                });
                let mut delivery = super::ClipboardDelivery::default();
                delivery.copy(&mut ansi, &mut native, &text);
                assert!(futures_util::FutureExt::now_or_never(delivery.fallback()).is_none());
            }
            assert_eq!(
                offered[0],
                (
                    super::NativeClipboardTarget::Clipboard,
                    ClipboardContent {
                        text: "**chosen**".into(),
                        html: Some("<p><strong>chosen</strong></p>\n".into())
                    }
                )
            );
            #[cfg(target_os = "linux")]
            assert_eq!(
                offered[1],
                (super::NativeClipboardTarget::Primary, "**chosen**".into())
            );
            assert!(ansi.0.is_empty());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn text_selection_and_invite_copies_also_fill_the_native_primary_selection() {
        use super::NativeClipboardTarget::{Clipboard, Primary};

        let mut offered = Vec::new();
        let mut terminal = AnsiTranscript(String::new());
        {
            let mut native = NativeClipboard::new(|target, content: &ClipboardContent| {
                let text = content.text.as_str();
                offered.push((target, text.to_owned()));
                Ok(())
            });
            copy_to_clipboard(&mut terminal, &mut native, "Hello\n界🙂");
            copy_to_clipboard(&mut terminal, &mut native, "suru-v1-example");
        }

        assert_eq!(
            offered,
            [
                (Clipboard, "Hello\n界🙂".to_owned()),
                (Primary, "Hello\n界🙂".to_owned()),
                (Clipboard, "suru-v1-example".to_owned()),
                (Primary, "suru-v1-example".to_owned()),
            ]
        );
        assert_eq!(
            terminal.0,
            "\x1b]52;c;SGVsbG8K55WM8J+Zgg==\x07\x1b]52;c;c3VydS12MS1leGFtcGxl\x07"
        );
    }

    #[derive(Default)]
    struct RecordingClipboard(Vec<String>);

    impl NativeClipboardSink for RecordingClipboard {
        fn copy(
            &mut self,
            content: &ClipboardContent,
            completed: impl FnOnce(bool) + Send + 'static,
        ) {
            self.0.push(content.text.clone());
            completed(true);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_primary_selection_logs_at_debug_without_counting_as_a_clipboard_failure() {
        use super::NativeClipboardTarget::{Clipboard, Primary};

        let mut terminal = AnsiTranscript(String::new());
        let mut copied = Vec::new();
        let log = record_log(|| {
            let mut native = NativeClipboard::new(|target, content: &ClipboardContent| {
                match (target, content.text.as_str()) {
                    (Clipboard, "failed") => Err(arboard::Error::ClipboardNotSupported),
                    (Clipboard, _) => {
                        copied.push(content.text.clone());
                        Ok(())
                    }
                    (Primary, "failed") => Ok(()),
                    (Primary, _) => Err(arboard::Error::ClipboardNotSupported),
                }
            });
            for text in ["one", "two", "failed"] {
                copy_to_clipboard(&mut terminal, &mut native, text);
            }
        });

        assert_eq!(copied, ["one", "two"]);
        assert_eq!(
            terminal.0,
            "\x1b]52;c;b25l\x07\x1b]52;c;dHdv\x07\x1b]52;c;ZmFpbGVk\x07"
        );
        let lines: Vec<_> = log.lines().collect();
        assert_eq!(lines.len(), 3, "{log}");
        for line in &lines[..2] {
            assert!(line.contains("DEBUG"), "{log}");
            assert!(line.contains("primary selection"), "{log}");
        }
        assert!(lines[2].contains("WARN"), "{log}");
        assert!(lines[2].contains("native Clipboard"), "{log}");
    }

    #[test]
    fn native_failures_warn_once_per_run_and_never_suppress_osc_52() {
        let mut terminal = AnsiTranscript(String::new());
        let mut offered = Vec::new();
        let log = record_log(|| {
            let mut native = NativeClipboard::new(|target, content: &ClipboardContent| {
                let text = content.text.as_str();
                if target != super::NativeClipboardTarget::Clipboard {
                    return Ok(());
                }
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

#[cfg(test)]
mod reconnect_grace_tests {
    use std::{collections::BTreeSet, time::Duration};

    use crate::{
        managed_client::ManagedEvent,
        protocol::Outlook,
        tui::state::{Application, ApplicationEvent},
    };

    use super::{sync_reconnect_grace, wait_for_reconnect_grace};

    /// One loss is served one grace. A recovery goes on retrying long after
    /// its grace has been spent, and every attempt reaches the run loop as an
    /// event: if each of those armed another wakeup, an idle Client would be
    /// woken forever and a loss taken off the frame would be put back on by
    /// the next one to come due.
    #[tokio::test]
    async fn a_grace_is_served_once_however_long_the_recovery_goes_on() {
        let grace = Duration::from_millis(5);
        let mut application = Application::default();
        let mut armed = Vec::new();
        let recovering = |attempt| {
            ApplicationEvent::Managed(ManagedEvent::Recovering(
                crate::managed_client::RecoveryStatus {
                    attempt,
                    retry_in: Duration::from_millis(1),
                },
            ))
        };

        application
            .handle_event(recovering(1))
            .expect("lose the local Server");
        sync_reconnect_grace(&mut armed, &application.origins_awaiting_grace(), grace);
        assert_eq!(armed.len(), 1, "a fresh loss is owed its grace");

        assert_eq!(wait_for_reconnect_grace(&mut armed).await, Outlook::Local);
        drop(armed.remove(0));
        application
            .handle_event(ApplicationEvent::ReconnectGraceElapsed(Outlook::Local))
            .expect("serve the grace");

        // Every further attempt of the same recovery asks for nothing.
        for attempt in 2..5 {
            application
                .handle_event(recovering(attempt))
                .expect("go on retrying");
            sync_reconnect_grace(&mut armed, &application.origins_awaiting_grace(), grace);
            assert!(
                armed.is_empty(),
                "attempt {attempt} re-armed a grace already served"
            );
        }

        // Answering and losing it again is a new loss, owed a grace of its own.
        application
            .handle_event(ApplicationEvent::Managed(ManagedEvent::RemoteRecovered))
            .expect("answer again");
        application
            .handle_event(recovering(1))
            .expect("lose the local Server afresh");
        sync_reconnect_grace(&mut armed, &application.origins_awaiting_grace(), grace);
        assert_eq!(armed.len(), 1, "a fresh loss is owed a grace of its own");
    }

    /// The graces armed follow what is recovering: one apiece for a loss newly
    /// arrived, and none at all for an Origin that has answered again. Driven
    /// end to end at millisecond scale so no test waits a production grace out.
    #[tokio::test]
    async fn a_grace_is_armed_per_loss_and_given_up_when_the_origin_answers() {
        let grace = Duration::from_millis(5);
        let studio = Outlook::Remote("studio".to_owned());
        let laptop = Outlook::Remote("laptop".to_owned());
        let mut armed = Vec::new();

        sync_reconnect_grace(&mut armed, &BTreeSet::from([studio.clone()]), grace);
        assert_eq!(armed.len(), 1);

        // A second loss takes a grace of its own; the first keeps the one it
        // was already serving rather than starting over.
        sync_reconnect_grace(
            &mut armed,
            &BTreeSet::from([studio.clone(), laptop.clone()]),
            grace,
        );
        assert_eq!(
            armed.iter().map(|(outlook, _)| outlook).collect::<Vec<_>>(),
            vec![&studio, &laptop]
        );

        // The first Origin answers again, so its grace is given up and the
        // one still recovering is what comes due.
        sync_reconnect_grace(&mut armed, &BTreeSet::from([laptop.clone()]), grace);
        assert_eq!(armed.len(), 1);
        assert_eq!(wait_for_reconnect_grace(&mut armed).await, laptop);

        // Everything answers: nothing is armed, so an idle Client schedules no
        // wakeup at all (ADR 0009).
        sync_reconnect_grace(&mut armed, &BTreeSet::new(), grace);
        assert!(armed.is_empty());
    }

    /// Each Origin's loss is held back from the frame for a grace of its own,
    /// and they come due in the order they were armed — so waiting on the
    /// front of the queue waits on all of them. The graces here are
    /// millisecond-scale so the test drives the real timing path without
    /// waiting a production grace out.
    #[tokio::test]
    async fn each_origins_grace_comes_due_in_the_order_it_was_armed() {
        let studio = Outlook::Remote("studio".to_owned());
        let laptop = Outlook::Remote("laptop".to_owned());
        let mut armed = vec![
            (
                studio.clone(),
                Box::pin(tokio::time::sleep(Duration::from_millis(5))),
            ),
            (
                laptop.clone(),
                Box::pin(tokio::time::sleep(Duration::from_millis(25))),
            ),
        ];

        assert_eq!(wait_for_reconnect_grace(&mut armed).await, studio);
        drop(armed.remove(0));
        assert_eq!(wait_for_reconnect_grace(&mut armed).await, laptop);
        drop(armed.remove(0));

        // Nothing armed is nothing to wake for: an idle Client schedules no
        // wakeup at all (ADR 0009).
        assert!(
            tokio::time::timeout(
                Duration::from_millis(15),
                wait_for_reconnect_grace(&mut armed),
            )
            .await
            .is_err()
        );
    }
}
