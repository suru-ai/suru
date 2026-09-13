//! Live Approval identity and Client arbitration, separate from durable history.
use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::oneshot,
    task::{AbortHandle, JoinSet},
};

use super::{ProviderSession, ProviderUpdateGate};
use crate::{
    protocol::{
        Activity, ActivityId, Approval, ApprovalId, ApprovalOutcome, Decision, SessionChange,
        SessionId, TurnId,
    },
    sessions::SessionStore,
};

/// Approval detail is structured but can contain arbitrary Provider input.
/// Bound the durable copy at the same generous size as command output while
/// the Provider keeps its native request whole for Decision translation.
const MAX_STORED_APPROVAL_DETAIL_CHARS: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct LiveApprovals {
    registered: HashMap<ApprovalId, ActivityId>,
}

impl LiveApprovals {
    pub(super) fn register(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
        approval: Approval,
        tool_activity_id: Option<ActivityId>,
    ) -> anyhow::Result<()> {
        if sessions.snapshot(session_id).is_some_and(|snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity, Activity::Approval { approval: previous, outcome, .. }
                    if previous.id == approval.id && *outcome != ApprovalOutcome::Unavailable)
            })
        }) {
            return Ok(());
        }
        let approval_id = approval.id;
        let (approval, detail_truncated) =
            approval.into_bounded_history(MAX_STORED_APPROVAL_DETAIL_CHARS);
        let activity_id = ActivityId::new();
        sessions.publish_agent_output(
            session_id,
            SessionChange::ActivityAdded {
                activity: Activity::Approval {
                    id: activity_id,
                    turn_id,
                    approval: approval.clone(),
                    tool_activity_id,
                    detail_truncated,
                    outcome: ApprovalOutcome::Pending,
                    decision: None,
                    follow_up_error: None,
                },
            },
        )?;
        self.registered.insert(approval_id, activity_id);
        Ok(())
    }

    pub(super) fn withdraw(&mut self, id: ApprovalId) {
        self.registered.remove(&id);
    }

    fn reserve(
        &self,
        sessions: &SessionStore,
        session_id: SessionId,
        id: ApprovalId,
        decision: Decision,
    ) -> Result<AcceptedDecision, String> {
        let activity_id = self
            .registered
            .get(&id)
            .copied()
            .ok_or("Approval is unavailable")?;
        sessions
            .publish_agent_output(session_id, SessionChange::DecisionAccepted { activity_id })
            .map_err(|_| "Approval is unavailable".to_owned())?;
        Ok(AcceptedDecision {
            session_id,
            activity_id,
            decision,
        })
    }
}

struct AcceptedDecision {
    session_id: SessionId,
    activity_id: ActivityId,
    decision: Decision,
}

/// Actor-owned delivery survives the submitting Client disconnecting. Dropping
/// the actor cancels it, and restart recovery records the uncertainty.
#[derive(Default)]
pub(super) struct DecisionDeliveries {
    tasks: JoinSet<()>,
    active: HashMap<(SessionId, ActivityId), AbortHandle>,
}

impl DecisionDeliveries {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn submit(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        session_id: SessionId,
        live: &mut LiveApprovals,
        provider: Arc<dyn ProviderSession>,
        id: ApprovalId,
        decision: Decision,
        response: oneshot::Sender<Result<(), String>>,
    ) {
        let accepted = match updates.apply(|| live.reserve(sessions, session_id, id, decision)) {
            Some(Ok(accepted)) => accepted,
            result => {
                let _ = response.send(Err(result
                    .and_then(Result::err)
                    .unwrap_or_else(|| "Approval is unavailable".into())));
                return;
            }
        };
        let key = (session_id, accepted.activity_id);
        let sessions = sessions.clone();
        let updates = updates.clone();
        let task = self.tasks.spawn(async move {
            let delivery = provider.submit_decision(id, decision).await;
            let delivered = delivery.is_ok();
            let outcome = if delivered {
                ApprovalOutcome::Decided
            } else if delivery
                .as_ref()
                .is_err_and(|error| error.is_decision_withdrawn())
            {
                ApprovalOutcome::Withdrawn
            } else if delivery
                .as_ref()
                .is_err_and(|error| error.is_decision_rejected())
            {
                ApprovalOutcome::SubmissionRejected
            } else {
                ApprovalOutcome::DeliveryUncertain
            };
            let settled = updates.apply(|| {
                sessions.publish_agent_output(
                    accepted.session_id,
                    SessionChange::ApprovalSettled {
                        activity_id: accepted.activity_id,
                        outcome,
                        decision: delivered.then_some(accepted.decision),
                    },
                )
            });
            let follow_up_error = match delivery {
                Ok(delivery) if matches!(settled, Some(Ok(_))) => {
                    delivery.finish().await.err().map(|error| {
                        super::failure_message("Provider post-Decision action failed", &error)
                    })
                }
                // Dropping a delivered receipt releases any Provider stream
                // barrier when core could not durably record the Decision.
                Ok(delivery) => {
                    drop(delivery);
                    None
                }
                Err(_) => None,
            };
            if let Some(error) = &follow_up_error {
                let _ = updates.apply(|| {
                    sessions.publish_agent_output(
                        accepted.session_id,
                        SessionChange::ApprovalFollowUpFailed {
                            activity_id: accepted.activity_id,
                            error: error.clone(),
                        },
                    )
                });
            }
            let result = if !matches!(settled, Some(Ok(_))) {
                Err("Decision delivery could not be confirmed".into())
            } else if let Some(error) = follow_up_error {
                Err(error)
            } else if delivered {
                Ok(())
            } else if outcome == ApprovalOutcome::SubmissionRejected {
                Err("Decision was not delivered. Review and retry.".into())
            } else {
                Err("Provider delivery is uncertain. This Decision will not be resent.".into())
            };
            let _ = response.send(result);
        });
        self.active.insert(key, task);
    }

    pub(super) fn reconcile(&mut self, sessions: &SessionStore) {
        while self.tasks.try_join_next().is_some() {}
        self.active.retain(|(session_id, activity_id), task| {
            if task.is_finished() {
                return false;
            }
            let keep_running = sessions.snapshot(*session_id).is_some_and(|snapshot| {
                snapshot.activities.iter().any(|activity| {
                    matches!(activity, Activity::Approval { id, outcome: ApprovalOutcome::Submitting | ApprovalOutcome::Decided, .. }
                        if id == activity_id)
                })
            });
            if !keep_running {
                task.abort();
            }
            keep_running
        });
    }
}
