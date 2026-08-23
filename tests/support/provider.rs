use std::sync::{Arc, Mutex};

use futures_util::stream;
use suru::protocol::{AgentIdentity, ModelDescriptor, ProviderId, ProviderUnavailability};
use suru::provider::{
    ProviderError, ProviderEvent, ProviderEventStream, ProviderFuture, ProviderRuntime,
    ProviderSession, ProviderSessionConnection, ProviderSessionRequest, ProviderSteerInput,
    ProviderTurnInput,
};
use tokio::sync::{mpsc, oneshot};

pub struct ControlledProvider {
    starts: mpsc::UnboundedReceiver<StartRequest>,
}

#[derive(Clone)]
pub struct ControlledProviderRuntime {
    provider: ProviderId,
    models: Vec<ModelDescriptor>,
    /// The condition Model discovery reports instead of the catalog, standing
    /// in for a Provider the user has yet to install or sign in to. Shared so a
    /// test can fix it the way a user would and refresh.
    unavailable: Arc<Mutex<Option<ProviderUnavailability>>>,
    starts: mpsc::UnboundedSender<StartRequest>,
}

pub struct StartRequest {
    request: ProviderSessionRequest,
    response: oneshot::Sender<Result<ProviderSessionConnection, ProviderError>>,
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
        (
            Arc::new(ControlledProviderRuntime {
                provider,
                models,
                unavailable: Arc::new(Mutex::new(None)),
                starts: starts_tx,
            }),
            Self { starts: starts_rx },
        )
    }

    pub async fn next_start(&mut self) -> StartRequest {
        self.starts
            .recv()
            .await
            .expect("Provider runtime remains connected")
    }
}

impl ControlledProviderRuntime {
    /// Makes Model discovery report `reason`, or — with `None` — serve the
    /// catalog again, as a user fixing the condition outside Suru would.
    pub fn set_unavailable(&self, reason: Option<ProviderUnavailability>) {
        *self
            .unavailable
            .lock()
            .expect("controlled Provider availability lock is not poisoned") = reason;
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
        &self.input.prompt
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
        &self.input.prompt
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

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>> {
        let models = self.models.clone();
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

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection> {
        let starts = self.starts.clone();
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

    fn shutdown(&self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
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
