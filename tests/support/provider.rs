use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::{StreamExt, stream};
use serde_json::Value;
use suru::protocol::{
    AgentIdentity, AgentSelection, ModelDescriptor, ModelId, ProviderId, ProviderUnavailability,
    SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus, SkillDescriptor, SkillId,
    SkillPromptDelivery, Workspace,
};
use suru::provider::{
    ProviderErrand, ProviderError, ProviderEvent, ProviderEventStream, ProviderFuture,
    ProviderPrompt, ProviderRuntime, ProviderSession, ProviderSessionConnection,
    ProviderSessionRequest, ProviderSteerInput, ProviderTurnInput,
};
use tokio::sync::{mpsc, oneshot};

pub struct ControlledProvider {
    starts: mpsc::UnboundedReceiver<StartRequest>,
    errands: mpsc::UnboundedReceiver<ErrandRequest>,
    errand_starts: mpsc::UnboundedReceiver<StartRequest>,
}

#[derive(Clone)]
pub struct ControlledProviderRuntime {
    provider: ProviderId,
    /// The catalog this Provider serves. Shared so a test can withdraw a Model
    /// the way a Provider dropping one from its own catalog does, which is the
    /// only way an Errand Selection resolved against a live catalog can be seen
    /// to be resolved afresh.
    models: Arc<Mutex<Vec<ModelDescriptor>>>,
    /// The Agent Selection this Provider declares for its own Errands — cheap
    /// and fast rather than capable — or nothing while it declares none and
    /// runs Errands at its default Model.
    errand_selection: Arc<Mutex<Option<AgentSelection>>>,
    /// The condition Model discovery reports instead of the catalog, standing
    /// in for a Provider the user has yet to install or sign in to. Shared so a
    /// test can fix it the way a user would and refresh.
    unavailable: Arc<Mutex<Option<ProviderUnavailability>>>,
    /// How many times Suru has asked this Provider for its Models. Every other
    /// assertion can only establish that a Provider is not selectable; this is
    /// what lets a test see that a Provider was never consulted at all.
    /// Session startup proves its own negative — a start request that never
    /// arrives on the double's channel is directly observable — so discovery is
    /// the one demand that needs counting.
    discoveries: Arc<AtomicUsize>,
    skill_discoveries: Arc<AtomicUsize>,
    skill_catalog: Arc<Mutex<Option<SkillCatalog>>>,
    skill_catalog_error: Arc<Mutex<Option<String>>>,
    starts: mpsc::UnboundedSender<StartRequest>,
    errands: mpsc::UnboundedSender<ErrandRequest>,
    /// Where a session-shaped Errand's startup goes. It is kept apart from
    /// `starts` so a test can tell the Provider-side session an Errand opens
    /// from the one the user's own first Turn opens, which arrive at the same
    /// moment and in no fixed order.
    errand_starts: mpsc::UnboundedSender<StartRequest>,
    /// Whether this double fulfils an Errand the way a harness with no one-shot
    /// mode does: by starting a Provider-side session, delivering the one
    /// Prompt, draining for the reply, and discarding the session (ADR 0011).
    session_shaped_errands: Arc<Mutex<bool>>,
}

pub struct StartRequest {
    request: ProviderSessionRequest,
    response: oneshot::Sender<Result<ProviderSessionConnection, ProviderError>>,
}

/// One Errand this Provider was asked to run, held until the test answers it.
/// Holding one without answering is how a test drives an Errand that times out,
/// because nothing else about a request that is never answered is observable.
pub struct ErrandRequest {
    errand: ProviderErrand,
    response: oneshot::Sender<Result<Value, ProviderError>>,
}

pub struct ControlledProviderSession {
    turns: mpsc::UnboundedReceiver<TurnStart>,
    steers: mpsc::UnboundedReceiver<TurnSteer>,
    interruptions: mpsc::UnboundedReceiver<TurnInterrupt>,
    events: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
}

pub struct PromptOperation {
    input: ProviderTurnInput,
    response: oneshot::Sender<Result<(), ProviderError>>,
}

pub type TurnStart = PromptOperation;

pub struct TurnSteer {
    input: ProviderSteerInput,
    response: oneshot::Sender<Result<(), ProviderError>>,
}

pub struct TurnInterrupt {
    response: oneshot::Sender<Result<(), ProviderError>>,
}

struct ControlledSessionHandle {
    turns: mpsc::UnboundedSender<TurnStart>,
    steers: mpsc::UnboundedSender<TurnSteer>,
    interruptions: mpsc::UnboundedSender<TurnInterrupt>,
}

impl ControlledProvider {
    pub fn new() -> (Arc<ControlledProviderRuntime>, Self) {
        Self::with_provider(ProviderId::new("controlled"), Vec::new())
    }

    /// A controlled runtime hosted under a chosen Provider identity, serving
    /// the given Model catalog. Lets one server host several distinguishable
    /// doubles side by side.
    pub fn with_provider(
        provider: ProviderId,
        models: Vec<ModelDescriptor>,
    ) -> (Arc<ControlledProviderRuntime>, Self) {
        let (starts_tx, starts_rx) = mpsc::unbounded_channel();
        let (errands_tx, errands_rx) = mpsc::unbounded_channel();
        let (errand_starts_tx, errand_starts_rx) = mpsc::unbounded_channel();
        (
            Arc::new(ControlledProviderRuntime {
                provider,
                models: Arc::new(Mutex::new(models)),
                errand_selection: Arc::new(Mutex::new(None)),
                unavailable: Arc::new(Mutex::new(None)),
                discoveries: Arc::new(AtomicUsize::new(0)),
                skill_discoveries: Arc::new(AtomicUsize::new(0)),
                skill_catalog: Arc::new(Mutex::new(None)),
                skill_catalog_error: Arc::new(Mutex::new(None)),
                starts: starts_tx,
                errands: errands_tx,
                errand_starts: errand_starts_tx,
                session_shaped_errands: Arc::new(Mutex::new(false)),
            }),
            Self {
                starts: starts_rx,
                errands: errands_rx,
                errand_starts: errand_starts_rx,
            },
        )
    }

    pub async fn next_start(&mut self) -> StartRequest {
        self.starts
            .recv()
            .await
            .expect("Provider runtime remains connected")
    }

    /// The Session startup already asked for, without waiting for one. A test
    /// that must show a Provider was *never* asked to begin a Session reads the
    /// absence here rather than waiting out a timeout.
    pub fn try_next_start(&mut self) -> Option<StartRequest> {
        self.starts.try_recv().ok()
    }

    pub async fn next_errand(&mut self) -> ErrandRequest {
        self.errands
            .recv()
            .await
            .expect("Provider runtime remains connected")
    }

    /// The Errand already asked for, without waiting for one. A test that must
    /// show a Provider was *never* asked to run an Errand reads the absence
    /// here rather than waiting out the Errand's timeout.
    pub fn try_next_errand(&mut self) -> Option<ErrandRequest> {
        self.errands.try_recv().ok()
    }

    /// The Provider-side session a session-shaped Errand opened, for a double
    /// put in that mode by
    /// [`ControlledProviderRuntime::run_errands_through_a_session`].
    pub async fn next_errand_start(&mut self) -> StartRequest {
        self.errand_starts
            .recv()
            .await
            .expect("Provider runtime remains connected")
    }
}

impl ControlledProviderRuntime {
    pub fn offer_skills(&self, catalog: SkillCatalog) {
        *self
            .skill_catalog
            .lock()
            .expect("controlled Provider Skill Catalog lock is not poisoned") = Some(catalog);
    }

    pub fn fail_skill_discovery(&self, message: impl Into<String>) {
        *self
            .skill_catalog_error
            .lock()
            .expect("controlled Provider Skill error lock is not poisoned") = Some(message.into());
    }

    pub fn skill_discoveries(&self) -> usize {
        self.skill_discoveries.load(Ordering::SeqCst)
    }

    /// Makes Model discovery report `reason`, or — with `None` — serve the
    /// catalog again, as a user fixing the condition outside Suru would.
    pub fn set_unavailable(&self, reason: Option<ProviderUnavailability>) {
        *self
            .unavailable
            .lock()
            .expect("controlled Provider availability lock is not poisoned") = reason;
    }

    /// How many times Suru has asked this Provider for its Models.
    pub fn model_discoveries(&self) -> usize {
        self.discoveries.load(Ordering::SeqCst)
    }

    /// Declares the Agent Selection this Provider runs its Errands under, as a
    /// real runtime declares the Model it knows to be cheap and the least
    /// effort it publishes.
    pub fn declare_errand_selection(&self, selection: AgentSelection) {
        *self
            .errand_selection
            .lock()
            .expect("controlled Provider Errand Selection lock is not poisoned") = Some(selection);
    }

    /// Drops `model` from the catalog this Provider serves, the way a Provider
    /// withdrawing a Model from its own catalog does. The next discovery is
    /// what makes the withdrawal visible to Suru.
    pub fn withdraw_model(&self, model: &ModelId) {
        self.models
            .lock()
            .expect("controlled Provider catalog lock is not poisoned")
            .retain(|candidate| &candidate.id != model);
    }

    /// Makes this double stand in for a Provider whose harness has no one-shot
    /// mode: an Errand is fulfilled by starting a Provider-side session,
    /// delivering the one Prompt, draining for the reply, and discarding the
    /// session. Which path ran is the runtime's own business, so nothing Suru
    /// asks of it changes.
    pub fn run_errands_through_a_session(&self) {
        *self
            .session_shaped_errands
            .lock()
            .expect("controlled Provider Errand mode lock is not poisoned") = true;
    }
}

impl ErrandRequest {
    pub fn prompt(&self) -> &str {
        &self.errand.prompt
    }

    pub fn schema(&self) -> &Value {
        &self.errand.schema
    }

    pub fn selection(&self) -> &AgentSelection {
        &self.errand.selection
    }

    pub fn workspace(&self) -> &std::path::Path {
        &self.errand.workspace
    }

    pub fn succeed(self, reply: Value) {
        self.response
            .send(Ok(reply))
            .unwrap_or_else(|_| panic!("Provider Errand response remains connected"));
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
            .unwrap_or_else(|_| panic!("Provider Errand response remains connected"));
    }
}

impl StartRequest {
    pub fn workspace(&self) -> &std::path::Path {
        &self.request.workspace
    }

    pub fn resume_state(&self) -> Option<&suru::provider::ProviderResumeState> {
        self.request.resume_state.as_ref()
    }

    pub fn succeed(self, identity: AgentIdentity) -> ControlledProviderSession {
        let (turns_tx, turns_rx) = mpsc::unbounded_channel();
        let (steers_tx, steers_rx) = mpsc::unbounded_channel();
        let (interruptions_tx, interruptions_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let events: ProviderEventStream = Box::pin(stream::unfold(events_rx, |mut events| async {
            events.recv().await.map(|event| (event, events))
        }));
        self.response
            .send(Ok(ProviderSessionConnection::new(
                identity,
                None,
                Arc::new(ControlledSessionHandle {
                    turns: turns_tx,
                    steers: steers_tx,
                    interruptions: interruptions_tx,
                }),
                events,
            )))
            .unwrap_or_else(|_| panic!("Provider startup response remains connected"));
        ControlledProviderSession {
            turns: turns_rx,
            steers: steers_rx,
            interruptions: interruptions_rx,
            events: events_tx,
        }
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
            .unwrap_or_else(|_| panic!("Provider startup response remains connected"));
    }

    /// Fails the startup the way a Provider whose CLI the user has yet to
    /// install or sign in to does: with the condition typed on the failure.
    pub fn fail_unavailable(self, reason: ProviderUnavailability, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::unavailable(reason, message)))
            .unwrap_or_else(|_| panic!("Provider startup response remains connected"));
    }
}

impl ControlledProviderSession {
    pub async fn next_turn(&mut self) -> TurnStart {
        self.turns
            .recv()
            .await
            .expect("Provider Session remains connected")
    }

    pub async fn next_steer(&mut self) -> TurnSteer {
        self.steers
            .recv()
            .await
            .expect("Provider Session remains connected")
    }

    pub async fn next_interrupt(&mut self) -> TurnInterrupt {
        self.interruptions
            .recv()
            .await
            .expect("Provider Session remains connected")
    }

    pub fn emit(&self, event: ProviderEvent) {
        self.events
            .send(Ok(event))
            .expect("Provider event stream remains connected");
    }
}

impl PromptOperation {
    pub fn prompt(&self) -> &str {
        &self.input.prompt.text
    }

    pub fn skill_invocations(&self) -> &[suru::provider::ProviderSkillInvocation] {
        &self.input.prompt.skill_invocations
    }

    pub fn selection(&self) -> &suru::protocol::AgentSelection {
        &self.input.selection
    }

    pub fn succeed(self) {
        self.response
            .send(Ok(()))
            .unwrap_or_else(|_| panic!("Provider Prompt operation response remains connected"));
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
            .unwrap_or_else(|_| panic!("Provider Prompt operation response remains connected"));
    }

    pub fn reject_selection(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::selection_rejected(message)))
            .unwrap_or_else(|_| panic!("Provider Prompt operation response remains connected"));
    }
}

impl TurnSteer {
    pub fn prompt(&self) -> &str {
        &self.input.prompt.text
    }

    pub fn skill_invocations(&self) -> &[suru::provider::ProviderSkillInvocation] {
        &self.input.prompt.skill_invocations
    }

    pub fn succeed(self) {
        self.response
            .send(Ok(()))
            .unwrap_or_else(|_| panic!("Provider steer operation response remains connected"));
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
            .unwrap_or_else(|_| panic!("Provider steer operation response remains connected"));
    }
}

impl TurnInterrupt {
    pub fn succeed(self) {
        self.response
            .send(Ok(()))
            .unwrap_or_else(|_| panic!("Provider interruption response remains connected"));
    }
}

impl ProviderRuntime for ControlledProviderRuntime {
    fn provider_id(&self) -> ProviderId {
        self.provider.clone()
    }

    // The wire identifier doubles as the display name so tests keep asserting
    // one vocabulary per double.
    fn display_name(&self) -> &str {
        self.provider.as_str()
    }

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        self.discoveries.fetch_add(1, Ordering::SeqCst);
        let models = self
            .models
            .lock()
            .expect("controlled Provider catalog lock is not poisoned")
            .clone();
        let unavailable = *self
            .unavailable
            .lock()
            .expect("controlled Provider availability lock is not poisoned");
        let provider = self.provider.clone();
        Box::pin(async move {
            match unavailable {
                Some(reason) => Err(ProviderError::unavailable(
                    reason,
                    format!("the {provider} CLI is {}", reason.label()),
                )),
                None => Ok(models),
            }
        })
    }

    fn skill_catalog(&self, workspace: &std::path::Path) -> ProviderFuture<'_, SkillCatalog> {
        self.skill_discoveries.fetch_add(1, Ordering::SeqCst);
        let catalog = self
            .skill_catalog
            .lock()
            .expect("controlled Provider Skill Catalog lock is not poisoned")
            .clone();
        let error = self
            .skill_catalog_error
            .lock()
            .expect("controlled Provider Skill error lock is not poisoned")
            .clone();
        let provider = self.provider.clone();
        let workspace = workspace.to_owned();
        Box::pin(async move {
            if let Some(error) = error {
                return Err(ProviderError::new(error));
            }
            Ok(catalog.unwrap_or_else(|| fixture_skill_catalog(provider, workspace)))
        })
    }

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        dispatch_start(self.starts.clone(), request)
    }

    fn errand_selection(&self) -> Option<AgentSelection> {
        self.errand_selection
            .lock()
            .expect("controlled Provider Errand Selection lock is not poisoned")
            .clone()
    }

    fn run_errand(&self, errand: ProviderErrand) -> ProviderFuture<'_, Value> {
        if *self
            .session_shaped_errands
            .lock()
            .expect("controlled Provider Errand mode lock is not poisoned")
        {
            let starts = self.errand_starts.clone();
            return Box::pin(async move { run_errand_through_a_session(starts, errand).await });
        }
        let errands = self.errands.clone();
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            errands
                .send(ErrandRequest {
                    errand,
                    response: response_tx,
                })
                .map_err(|_| ProviderError::new("test Provider controller disconnected"))?;
            response_rx
                .await
                .map_err(|_| ProviderError::new("test Provider Errand was abandoned"))?
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

pub fn fixture_skill_catalog(provider: ProviderId, workspace: std::path::PathBuf) -> SkillCatalog {
    let skills = [
        ("safe-review-id", "review"),
        ("safe-explain-id", "explain"),
        ("safe-steer-review-id", "review"),
        ("safe-retry-id", "retry"),
        ("safe-review-after-interrupt-id", "review"),
        ("safe-smaller-interface-id", "smaller-interface"),
    ]
    .into_iter()
    .map(|(id, name)| SkillDescriptor {
        id: SkillId::new(id),
        name: name.to_owned(),
        description: format!("Test Skill {name}"),
        scope: Some("Workspace".to_owned()),
    })
    .collect();
    SkillCatalog {
        provider,
        workspace: Workspace { path: workspace },
        skills,
        capabilities: SkillCatalogCapabilities {
            max_distinct_invocations: None,
            supported_deliveries: vec![
                SkillPromptDelivery::Initial,
                SkillPromptDelivery::Queue,
                SkillPromptDelivery::Steer,
            ],
        },
        status: SkillCatalogStatus::Fresh { warning: None },
    }
}

fn dispatch_start(
    starts: mpsc::UnboundedSender<StartRequest>,
    request: ProviderSessionRequest,
) -> ProviderFuture<'static, ProviderSessionConnection> {
    Box::pin(async move {
        let (response_tx, response_rx) = oneshot::channel();
        starts
            .send(StartRequest {
                request,
                response: response_tx,
            })
            .map_err(|_| ProviderError::new("test Provider controller disconnected"))?;
        response_rx
            .await
            .map_err(|_| ProviderError::new("test Provider startup was abandoned"))?
    })
}

/// The escape hatch ADR 0011 keeps for a harness with no one-shot mode: start a
/// Provider-side session, deliver the one Prompt, drain it for the reply, and
/// shut it down. No Suru Session is involved on any of it — the request names
/// none, and nothing here reaches the Session store.
async fn run_errand_through_a_session(
    starts: mpsc::UnboundedSender<StartRequest>,
    errand: ProviderErrand,
) -> Result<Value, ProviderError> {
    let connection = dispatch_start(
        starts,
        ProviderSessionRequest {
            workspace: errand.workspace,
            resume_state: None,
        },
    )
    .await?;
    let (_identity, _resume_state, session, mut events) = connection.into_parts();
    session
        .start_turn(ProviderTurnInput {
            prompt: ProviderPrompt::plain(errand.prompt),
            selection: errand.selection,
        })
        .await?;
    let mut reply = String::new();
    while let Some(event) = events.next().await {
        match event? {
            ProviderEvent::AgentMessageDelta { content } => reply.push_str(&content),
            ProviderEvent::TurnCompleted => break,
            _ => {}
        }
    }
    session.shutdown().await?;
    serde_json::from_str(&reply)
        .map_err(|error| ProviderError::new(format!("Errand reply is not JSON: {error}")))
}

fn dispatch_prompt_operation(
    operations: mpsc::UnboundedSender<PromptOperation>,
    input: ProviderTurnInput,
    abandoned_message: &'static str,
) -> ProviderFuture<'static, ()> {
    Box::pin(async move {
        let (response_tx, response_rx) = oneshot::channel();
        operations
            .send(PromptOperation {
                input,
                response: response_tx,
            })
            .map_err(|_| ProviderError::new("test Provider Session disconnected"))?;
        response_rx
            .await
            .map_err(|_| ProviderError::new(abandoned_message))?
    })
}

fn dispatch_steer_operation(
    operations: mpsc::UnboundedSender<TurnSteer>,
    input: ProviderSteerInput,
) -> ProviderFuture<'static, ()> {
    Box::pin(async move {
        let (response_tx, response_rx) = oneshot::channel();
        operations
            .send(TurnSteer {
                input,
                response: response_tx,
            })
            .map_err(|_| ProviderError::new("test Provider Session disconnected"))?;
        response_rx
            .await
            .map_err(|_| ProviderError::new("test Provider steering was abandoned"))?
    })
}

impl ProviderSession for ControlledSessionHandle {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        dispatch_prompt_operation(
            self.turns.clone(),
            input,
            "test Provider Turn was abandoned",
        )
    }

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()> {
        dispatch_steer_operation(self.steers.clone(), input)
    }

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()> {
        let interruptions = self.interruptions.clone();
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            interruptions
                .send(TurnInterrupt {
                    response: response_tx,
                })
                .map_err(|_| ProviderError::new("test Provider Session disconnected"))?;
            response_rx
                .await
                .map_err(|_| ProviderError::new("test Provider interruption was abandoned"))?
        })
    }

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
