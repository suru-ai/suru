use crate::support::{read_session_until, working_turn};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, Answer, Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireId,
        QuestionnaireOutcome, QuestionnaireSubmission,
    },
    provider::ProviderEvent,
};
use tokio::time::{Duration, timeout};

fn questionnaire() -> Questionnaire {
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "target".into(),
            title: None,
            text: "Which target?".into(),
            choices: vec![QuestionChoice {
                id: "local".into(),
                label: "Local".into(),
                description: None,
                recommended: true,
            }],
            multiple: false,
            freeform: true,
            secret: false,
            required: true,
        }],
    }
}
fn outcome(snapshot: &suru::protocol::SessionSnapshot, expected: QuestionnaireOutcome) -> bool {
    snapshot
        .activities
        .iter()
        .any(|a| matches!(a, Activity::Questionnaire { outcome, .. } if *outcome == expected))
}

#[tokio::test]
async fn questionnaire_answer_crosses_the_public_client_and_is_readable_in_durable_history() {
    let directory = tempfile::tempdir().unwrap();
    let mut live = working_turn(directory.path(), "questionnaire-answer").await;
    let question = questionnaire();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: question.clone(),
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Questionnaire is pending",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    let client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "questionnaire-answer").unwrap(),
    )
    .await
    .unwrap();
    let answer = Answer {
        questions: vec![QuestionAnswer::Freeform {
            text: "Use the staging machine".into(),
        }],
    };
    client
        .submit_questionnaire(
            live.session_id,
            question.id,
            QuestionnaireSubmission::Answer {
                answer: answer.clone(),
            },
        )
        .await
        .unwrap();
    let received = timeout(
        Duration::from_secs(1),
        live.provider_session.next_questionnaire_submission(),
    )
    .await
    .unwrap();
    assert_eq!(
        received,
        (
            question.id,
            QuestionnaireSubmission::Answer {
                answer: answer.clone()
            }
        )
    );
    let snapshot = client.read_session(live.session_id).await.unwrap();
    assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { answer: Some(stored), outcome: QuestionnaireOutcome::Answered, .. } if stored == &answer)));
    assert!(
        client
            .submit_questionnaire(
                live.session_id,
                question.id,
                QuestionnaireSubmission::Decline
            )
            .await
            .is_err()
    );
    drop(client);
    live.server.shutdown().await.unwrap();
    let server = suru::server::spawn_with_provider(
        suru::server::ServerConfig::new(directory.path(), "questionnaire-answer").unwrap(),
        live.runtime,
    )
    .await
    .unwrap();
    let client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "questionnaire-answer").unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .read_session(live.session_id)
            .await
            .unwrap()
            .activities,
        snapshot.activities
    );
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn decline_sends_no_answer_without_ending_the_turn_and_withdrawal_disables_submission() {
    let directory = tempfile::tempdir().unwrap();
    let mut live = working_turn(directory.path(), "questionnaire-decline").await;
    let question = questionnaire();
    let client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "questionnaire-decline").unwrap(),
    )
    .await
    .unwrap();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: question.clone(),
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Questionnaire is pending",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    client
        .submit_questionnaire(
            live.session_id,
            question.id,
            QuestionnaireSubmission::Decline,
        )
        .await
        .unwrap();
    assert_eq!(
        live.provider_session.next_questionnaire_submission().await,
        (question.id, QuestionnaireSubmission::Decline)
    );
    let snapshot = client.read_session(live.session_id).await.unwrap();
    assert!(
        snapshot
            .turns
            .iter()
            .any(|t| t.status == suru::protocol::TurnStatus::Active)
    );
    let next = questionnaire();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: next.clone(),
        });
    live.provider_session
        .emit(ProviderEvent::QuestionnaireWithdrawn { id: next.id });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Questionnaire was withdrawn",
        |s| outcome(s, QuestionnaireOutcome::Withdrawn),
    )
    .await;
    assert!(
        client
            .submit_questionnaire(live.session_id, next.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    let last = questionnaire();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: last.clone(),
        });
    live.provider_session.emit(ProviderEvent::TurnCompleted);
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Turn ended",
        |s| outcome(s, QuestionnaireOutcome::TurnEnded),
    )
    .await;
    assert!(
        client
            .submit_questionnaire(live.session_id, last.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    drop(client);
    live.server.shutdown().await.unwrap();
}
