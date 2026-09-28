//! Delegations an Agent sends a brokered Subagent after its spawn, through
//! the Broker's `send_to_subagent`. What one does is decided by how it is
//! delivered (ADR 0032), so the Subagent's own actor — the one place that
//! knows what its Provider is doing — delivers it: into a Turn a Delegation
//! began and its Provider still works, as a steer; and otherwise as the
//! opening of a resume, a new Turn in the Subagent's own Session with a row of
//! its own in the sending Agent's Transcript (ADR 0031). A Continuation still
//! open is no Delegation's to steer: like a Prompt, the Delegation settles it
//! first — interrupting whatever Provider work it owns — and begins the
//! resume once it has.

use std::ops::ControlFlow;

use tokio::sync::oneshot;

use super::{
    ActiveProviderTurn, ConnectedProviderSession, DelegatedTurnStart, ProviderCommand,
    ProviderConnector, ProviderOrchestrator, ProviderShutdown, SubagentRoutes,
    begin_delegated_turn, delegated_prompt, delegation_text, failure_message,
    release_watch_outcomes,
};
use crate::ansi::NormalizedText;
use crate::protocol::{ProviderId, SessionId, TurnId};
use crate::provider::{ProviderInput, ProviderPrompt, ProviderSession, ProviderSteerInput};
use crate::sessions::{
    BrokeredReadError, BrokeredResumeError, BrokeredSpawnCap, DeliveredDelegation, SessionStore,
};

/// How a Delegation sent to a brokered Subagent after its spawn was delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredDelivery {
    /// It began a new Turn in the Subagent's Session: a resume, standing as
    /// a row of its own in the sending Agent's Transcript.
    Resumed,
    /// It reached the Turn the Subagent was working in, adding no row.
    Steered,
}

/// Why a Delegation sent to a brokered Subagent was not delivered, for the
/// Broker to tell the Agent that sent it. A Delegation never delivered stands
/// nowhere.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredSendRefusal {
    /// The Subagent is none the sending Agent may reach through the Broker.
    Unreachable(BrokeredReadError),
    /// A resume would set more brokered Subagents working than the Broker's
    /// Settings allow. Nothing of it was begun, and nothing waits for room.
    Capped(BrokeredSpawnCap),
    /// Any other reason, already in words the sending Agent reads.
    Refused(String),
}

/// A Delegation on its way to a brokered Subagent's actor: the Session whose
/// Agent sent it, and what it asks, normalized and capped as a spawn's is.
pub(super) struct SentDelegation {
    caller: SessionId,
    text: NormalizedText,
}

/// A Delegation the actor holds until what its Subagent is doing settles —
/// a Continuation being interrupted — with the call waiting on how it was
/// delivered.
pub(super) struct PendingDelegation {
    pub(super) delegation: SentDelegation,
    pub(super) response: oneshot::Sender<Result<BrokeredDelivery, BrokeredSendRefusal>>,
}

impl PendingDelegation {
    fn answer(self, delivery: Result<BrokeredDelivery, BrokeredSendRefusal>) {
        let _ = self.response.send(delivery);
    }
}

/// What a Delegation still waiting when Suru stops is refused with.
const STOPPING: &str = "Suru is stopping, so the message was not delivered to the Subagent.";

impl ProviderOrchestrator {
    /// Delivers `message` to the brokered Subagent `subagent` for the Agent
    /// of `caller`, and answers how it was delivered once it has been: a
    /// steer once the Subagent's Provider has taken it into the Turn it works
    /// in, and a resume once the resume's Turn and row stand — whatever then
    /// becomes of the Turn, its Provider failing to start included, settles
    /// the Turn and its row rather than the answer, as for a spawn.
    ///
    /// A Subagent whose actor is gone — one this process restored after a
    /// restart, above all — has nothing working to steer, so its actor is
    /// started again to resume it; its Provider starts on the Subagent's own
    /// Resume State when the resume's Turn begins.
    ///
    /// Refused, in words the sending Agent reads, for a Subagent that is no
    /// brokered Subagent beneath `caller`, an empty message, a resume on a
    /// Provider the user has turned off or past the concurrency cap, and a
    /// steer its Provider did not take.
    pub(crate) async fn send_to_brokered_subagent(
        &self,
        caller: SessionId,
        subagent: SessionId,
        message: &str,
    ) -> Result<BrokeredDelivery, BrokeredSendRefusal> {
        self.sessions
            .reach_brokered_subagent(caller, subagent)
            .map_err(BrokeredSendRefusal::Unreachable)?;
        let text = delegation_text(message).ok_or_else(|| {
            BrokeredSendRefusal::Refused(
                "send_to_subagent's `message` is empty; say what the Subagent is to do.".to_owned(),
            )
        })?;
        let commands = match self.actor_commands(subagent) {
            Some(commands) => commands,
            None => {
                let runtime = self
                    .resolve_runtime(subagent)
                    .map_err(BrokeredSendRefusal::Refused)?;
                if !self.is_enabled(&runtime.provider_id()) {
                    return Err(BrokeredSendRefusal::Refused(turned_off(
                        &runtime.provider_id(),
                    )));
                }
                let execution_directory =
                    self.sessions.execution_directory(subagent).ok_or_else(|| {
                        BrokeredSendRefusal::Refused(
                            "The Subagent's Session no longer exists on this Suru server."
                                .to_owned(),
                        )
                    })?;
                self.get_or_spawn_actor_commands(subagent, execution_directory, runtime)
                    .map_err(|error| {
                        BrokeredSendRefusal::Refused(format!(
                            "Suru could not reach the Subagent's Provider: {error}."
                        ))
                    })?
            }
        };
        let (response, delivered) = oneshot::channel();
        commands
            .send(ProviderCommand::DeliverDelegation(PendingDelegation {
                delegation: SentDelegation { caller, text },
                response,
            }))
            .map_err(|_| BrokeredSendRefusal::Refused(STOPPING.to_owned()))?;
        delivered
            .await
            .unwrap_or_else(|_| Err(BrokeredSendRefusal::Refused(STOPPING.to_owned())))
    }
}

/// Why a resume on `provider` is refused while the user has it turned off:
/// Enablement governs what Suru begins next, and a resume begins a Turn.
fn turned_off(provider: &ProviderId) -> String {
    format!(
        "Provider `{provider}` is turned off in Suru's Settings, so the Subagent cannot be \
         resumed; ask the user to turn `provider.{provider}.enabled` back on."
    )
}

/// Opens the resume `pending` begins in the brokered Subagent's Session the
/// actor behind `connector` owns — its Turn, opened by the Delegation, and its
/// row in the sending Agent's Transcript — and answers the sending Agent's
/// call, with the Turn and the input its Provider is to begin it with; or
/// refuses the call, with nothing begun.
pub(super) fn open_resume(
    connector: &ProviderConnector<'_>,
    pending: PendingDelegation,
) -> Option<(TurnId, ProviderPrompt)> {
    if !connector
        .settings
        .borrow()
        .settings
        .provider_enabled(connector.provider_id)
    {
        pending.answer(Err(BrokeredSendRefusal::Refused(turned_off(
            connector.provider_id,
        ))));
        return None;
    }
    let PendingDelegation {
        delegation,
        response,
    } = pending;
    let delivered = delegation.text.content.clone();
    let resumed = connector.updates.apply(|| {
        connector.sessions.resume_brokered_subagent(
            delegation.caller,
            connector.session_id,
            delegation.text,
        )
    });
    let (answer, begun) = match resumed {
        None => (Err(BrokeredSendRefusal::Refused(STOPPING.to_owned())), None),
        Some(Ok(resumed)) => (
            Ok(BrokeredDelivery::Resumed),
            Some((
                resumed.turn_id,
                delegated_prompt(&resumed.delegator, &delivered),
            )),
        ),
        Some(Err(BrokeredResumeError::Unreachable(error))) => {
            (Err(BrokeredSendRefusal::Unreachable(error)), None)
        }
        Some(Err(BrokeredResumeError::Capped(cap))) => {
            (Err(BrokeredSendRefusal::Capped(cap)), None)
        }
        Some(Err(BrokeredResumeError::Storage(message))) => (
            Err(BrokeredSendRefusal::Refused(format!(
                "Suru could not resume the Subagent: {message}."
            ))),
            None,
        ),
    };
    let _ = response.send(answer);
    begun
}

/// Begins the Turn a Delegation opened in the brokered Subagent's Session the
/// actor behind `connector` owns (see [`begin_delegated_turn`]), and makes it
/// the actor's active Turn once its Provider takes it. Breaks when Suru is
/// stopping, as the actor does with it.
pub(super) async fn run_delegated_turn(
    connector: &ProviderConnector<'_>,
    provider: &mut Option<ConnectedProviderSession>,
    subagents: &mut SubagentRoutes,
    shutdown: &mut ProviderShutdown,
    active: &mut Option<ActiveProviderTurn>,
    turn_id: TurnId,
    input: ProviderPrompt,
) -> ControlFlow<()> {
    match begin_delegated_turn(connector, provider, subagents, shutdown, turn_id, input).await {
        DelegatedTurnStart::Began => {
            *active = Some(ActiveProviderTurn::new_delegated(turn_id));
            subagents.late_settle_owes_continuation = false;
            // A Watch that woke the Agent heads the Turn it next works in,
            // whatever began it.
            release_watch_outcomes(
                connector.sessions,
                connector.updates,
                subagents,
                connector.session_id,
                turn_id,
            );
            ControlFlow::Continue(())
        }
        DelegatedTurnStart::Settled => ControlFlow::Continue(()),
        DelegatedTurnStart::Stopping => ControlFlow::Break(()),
    }
}

/// Steers `turn_id` — a Turn a Delegation began in the brokered Subagent's
/// Session `session_id`, which its Provider still works — with the Delegation
/// `pending` carries, and answers the sending Agent's call. Taken by the
/// Provider, the Delegation stands in that Turn as a Message from the Agent
/// that sent it, after everything the Subagent did before receiving it
/// (ADR 0032), and no row is added anywhere. Refused by the Provider — its
/// Turn most likely ended as the Delegation arrived, which Suru hears of only
/// afterwards — it was never delivered and stands nowhere; the sending Agent
/// is told so, and sent back to resume the Subagent once it has settled.
/// Breaks when Suru is stopping.
pub(super) async fn steer_with_delegation(
    sessions: &SessionStore,
    updates: &super::ProviderUpdateGate,
    provider_session: &dyn ProviderSession,
    shutdown: &mut ProviderShutdown,
    session_id: SessionId,
    turn_id: TurnId,
    pending: PendingDelegation,
) -> ControlFlow<()> {
    let Some(delegator) = sessions.delegating_agent(pending.delegation.caller) else {
        pending.answer(Err(BrokeredSendRefusal::Unreachable(
            BrokeredReadError::CallerNotFound,
        )));
        return ControlFlow::Continue(());
    };
    let input = delegated_prompt(&delegator, &pending.delegation.text.content);
    let steered = tokio::select! {
        biased;
        _ = shutdown.wait() => {
            pending.answer(Err(BrokeredSendRefusal::Refused(STOPPING.to_owned())));
            return ControlFlow::Break(());
        }
        steered = provider_session.steer_turn(ProviderSteerInput {
            input: ProviderInput::from_prompt(input),
        }) => steered,
    };
    let PendingDelegation {
        delegation,
        response,
    } = pending;
    let answer = match steered {
        Ok(()) => {
            if let Some(Err(error)) = updates.apply(|| {
                sessions.deliver_delegation(
                    session_id,
                    turn_id,
                    DeliveredDelegation {
                        delegating_session: delegation.caller,
                        text: delegation.text,
                    },
                )
            }) {
                tracing::warn!(%session_id, "a steering Delegation could not be recorded: {error:#}");
            }
            Ok(BrokeredDelivery::Steered)
        }
        Err(error) => Err(BrokeredSendRefusal::Refused(format!(
            "{}. The Subagent's Provider did not take the message into the work it was doing, so \
             it was not delivered; that work most likely ended as the message arrived. Once \
             read_subagent says the Subagent has settled, call send_to_subagent again to resume \
             it.",
            failure_message("Steering the Subagent failed", &error)
        ))),
    };
    let _ = response.send(answer);
    ControlFlow::Continue(())
}

/// Refuses `pending` because the Turn it would steer is being stopped: the
/// stop reaches the Provider first, so the Delegation could only arrive at an
/// end the Subagent is not told of.
pub(super) fn refuse_while_stopping(pending: PendingDelegation) {
    pending.answer(Err(BrokeredSendRefusal::Refused(
        "The Subagent's work is being stopped, so the message was not delivered. Once \
         read_subagent says it has settled, call send_to_subagent again to resume it."
            .to_owned(),
    )));
}

/// Refuses `pending`, which waited for its Subagent's Continuation to settle,
/// because the Subagent was stopped meanwhile: the stop ends what it was
/// doing, and a resume behind it would begin again what the stop ended.
pub(super) fn refuse_withdrawn(pending: PendingDelegation) {
    pending.answer(Err(BrokeredSendRefusal::Refused(
        "The Subagent was stopped before the message reached it, so it was not delivered. Call \
         send_to_subagent again to resume it once it has settled."
            .to_owned(),
    )));
}
