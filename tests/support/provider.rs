use std::sync::Arc;

use chidori::protocol::AgentIdentity;
use chidori::provider::{
    ProviderError, ProviderEvent, ProviderEventStream, ProviderFuture, ProviderRuntime,
    ProviderSession, ProviderSessionConnection, ProviderSessionRequest, ProviderTurnInput,
};
use futures_util::stream;
use tokio::sync::{mpsc, oneshot};

pub struct ControlledProvider {
    starts: mpsc::UnboundedReceiver<StartRequest>,
}

#[derive(Clone)]
pub struct ControlledProviderRuntime {
    starts: mpsc::UnboundedSender<StartRequest>,
}

pub struct StartRequest {
    request: ProviderSessionRequest,
    response: oneshot::Sender<Result<ProviderSessionConnection, ProviderError>>,
}

pub struct ControlledProviderSession {
    turns: mpsc::UnboundedReceiver<TurnStart>,
    events: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
}

pub struct TurnStart {
    input: ProviderTurnInput,
    response: oneshot::Sender<Result<(), ProviderError>>,
}

struct ControlledSessionHandle {
    turns: mpsc::UnboundedSender<TurnStart>,
}

impl ControlledProvider {
    pub fn new() -> (Arc<ControlledProviderRuntime>, Self) {
        let (starts_tx, starts_rx) = mpsc::unbounded_channel();
        (
            Arc::new(ControlledProviderRuntime { starts: starts_tx }),
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

impl StartRequest {
    pub fn workspace(&self) -> &std::path::Path {
        &self.request.workspace
    }

    pub fn succeed(self, identity: AgentIdentity) -> ControlledProviderSession {
        let (turns_tx, turns_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let events: ProviderEventStream = Box::pin(stream::unfold(events_rx, |mut events| async {
            events.recv().await.map(|event| (event, events))
        }));
        self.response
            .send(Ok(ProviderSessionConnection::new(
                identity,
                Arc::new(ControlledSessionHandle { turns: turns_tx }),
                events,
            )))
            .unwrap_or_else(|_| panic!("Provider startup response remains connected"));
        ControlledProviderSession {
            turns: turns_rx,
            events: events_tx,
        }
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
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

    pub fn emit(&self, event: ProviderEvent) {
        self.events
            .send(Ok(event))
            .expect("Provider event stream remains connected");
    }
}

impl TurnStart {
    pub fn prompt(&self) -> &str {
        &self.input.prompt
    }

    pub fn succeed(self) {
        self.response
            .send(Ok(()))
            .unwrap_or_else(|_| panic!("Provider Turn response remains connected"));
    }

    pub fn fail(self, message: impl Into<String>) {
        self.response
            .send(Err(ProviderError::new(message)))
            .unwrap_or_else(|_| panic!("Provider Turn response remains connected"));
    }
}

impl ProviderRuntime for ControlledProviderRuntime {
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
}

impl ProviderSession for ControlledSessionHandle {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()> {
        let turns = self.turns.clone();
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            turns
                .send(TurnStart {
                    input,
                    response: response_tx,
                })
                .map_err(|_| ProviderError::new("test Provider Session disconnected"))?;
            response_rx
                .await
                .map_err(|_| ProviderError::new("test Provider Turn was abandoned"))?
        })
    }
}
