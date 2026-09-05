//! Live callback identity and Client arbitration, separate from durable history.
use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::oneshot,
    task::{AbortHandle, JoinSet},
};

use super::{ProviderSession, ProviderUpdateGate};
use crate::{
    protocol::{
        Activity, ActivityId, Answer, Questionnaire, QuestionnaireId, QuestionnaireOutcome,
        QuestionnaireSubmission, SessionChange, SessionId, TurnId,
    },
    sessions::SessionStore,
};

#[derive(Default)]
pub(super) struct LiveQuestionnaires {
    registered: HashMap<QuestionnaireId, (ActivityId, Questionnaire)>,
}

impl LiveQuestionnaires {
    pub(super) fn register(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
        questionnaire: Questionnaire,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !questionnaire.questions.is_empty(),
            "Provider sent a Questionnaire without Questions"
        );
        // A duplicate native event cannot revive a consumed request identity.
        if sessions.snapshot(session_id).is_some_and(|snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity, Activity::Questionnaire { questionnaire: previous, outcome, .. }
                    if previous.id == questionnaire.id && *outcome != QuestionnaireOutcome::Unavailable)
            })
        }) {
            return Ok(());
        }
        let activity_id = ActivityId::new();
        sessions.publish_agent_output(
            session_id,
            SessionChange::ActivityAdded {
                activity: Activity::Questionnaire {
                    id: activity_id,
                    turn_id,
                    questionnaire: questionnaire.clone(),
                    outcome: QuestionnaireOutcome::Pending,
                    answer: None,
                },
            },
        )?;
        self.registered
            .insert(questionnaire.id, (activity_id, questionnaire));
        Ok(())
    }

    pub(super) fn withdraw(&mut self, id: QuestionnaireId) {
        self.registered.remove(&id);
    }

    fn reserve(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        id: QuestionnaireId,
        submission: &QuestionnaireSubmission,
    ) -> Result<AcceptedSubmission, String> {
        let (activity_id, questionnaire) = self
            .registered
            .get(&id)
            .ok_or("Questionnaire is unavailable")?;
        let (outcome, answer) = match submission {
            QuestionnaireSubmission::Answer { answer } => {
                questionnaire.validate(answer)?;
                (
                    QuestionnaireOutcome::Answered,
                    Some(questionnaire.history_answer(answer)),
                )
            }
            QuestionnaireSubmission::Decline => (QuestionnaireOutcome::Declined, None),
        };
        let accepted = AcceptedSubmission {
            session_id,
            activity_id: *activity_id,
            outcome,
            answer,
        };
        sessions
            .publish_agent_output(
                session_id,
                SessionChange::QuestionnaireAccepted {
                    activity_id: *activity_id,
                },
            )
            .map_err(|_| "Questionnaire is unavailable".to_owned())?;
        // History arbitrates consumption; retain the live callback identity so a
        // definite Provider rejection can permit an explicit retry.
        Ok(accepted)
    }
}

struct AcceptedSubmission {
    session_id: SessionId,
    activity_id: ActivityId,
    outcome: QuestionnaireOutcome,
    answer: Option<Answer>,
}

/// Actor-owned tasks let delivery wait without blocking withdrawal or interruption.
/// Dropping the actor cancels delivery; closing the submitting Client does not.
#[derive(Default)]
pub(super) struct QuestionnaireDeliveries {
    tasks: JoinSet<()>,
    active: HashMap<(SessionId, ActivityId), AbortHandle>,
}

impl QuestionnaireDeliveries {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn submit(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        session_id: SessionId,
        live: &mut LiveQuestionnaires,
        provider: Arc<dyn ProviderSession>,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
        response: oneshot::Sender<Result<(), String>>,
    ) {
        let accepted = match updates.apply(|| live.reserve(sessions, session_id, id, &submission)) {
            Some(Ok(accepted)) => accepted,
            result => {
                let _ = response.send(Err(result
                    .and_then(Result::err)
                    .unwrap_or_else(|| "Questionnaire is unavailable".into())));
                return;
            }
        };
        let key = (session_id, accepted.activity_id);
        let sessions = sessions.clone();
        let updates = updates.clone();
        let task = self.tasks.spawn(async move {
            let delivery = provider.submit_questionnaire(id, submission).await;
            let delivered = delivery.is_ok();
            let outcome = if delivered {
                accepted.outcome
            } else if delivery.as_ref().is_err_and(|error| error.is_questionnaire_rejected()) {
                QuestionnaireOutcome::SubmissionRejected
            } else {
                QuestionnaireOutcome::DeliveryUncertain
            };
            // Session projection is the final compare-and-set: a concurrent withdrawal
            // or Turn settlement must never be overwritten by this late completion.
            let settled = updates.apply(|| {
                sessions.publish_agent_output(
                    accepted.session_id,
                    SessionChange::QuestionnaireSettled {
                        activity_id: accepted.activity_id,
                        outcome,
                        answer: if delivered { accepted.answer } else { None },
                    },
                )
            });
            let result = if !delivered {
                Err(if outcome == QuestionnaireOutcome::SubmissionRejected {
                    "Answer was not delivered. Review your draft and retry.".into()
                } else {
                    "Provider delivery is uncertain. This Answer will not be resent.".into()
                })
            } else if matches!(settled, Some(Ok(_))) {
                Ok(())
            } else {
                Err("Questionnaire delivery could not be confirmed".into())
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
            let submitting = sessions.snapshot(*session_id).is_some_and(|snapshot| {
                snapshot.activities.iter().any(|activity| {
                    matches!(activity, Activity::Questionnaire { id, outcome: QuestionnaireOutcome::Submitting, .. }
                        if id == activity_id)
                })
            });
            if !submitting {
                task.abort();
            }
            submitting
        });
    }
}
