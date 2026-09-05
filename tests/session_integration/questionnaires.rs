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
            combine_freeform: false,
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

#[tokio::test]
async fn session_batch_validation_preserves_each_question_and_only_accepts_supported_combined_answers()
 {
    let directory = tempfile::tempdir().unwrap();
    let mut live = working_turn(directory.path(), "questionnaire-batch").await;
    let mut batch = questionnaire();
    let mut second = batch.questions[0].clone();
    second.id = "checks".into();
    second.text = "Which checks?".into();
    second.multiple = true;
    second.combine_freeform = true;
    second.choices = vec![
        QuestionChoice {
            id: "unit".into(),
            label: "Unit".into(),
            description: None,
            recommended: false,
        },
        QuestionChoice {
            id: "integration".into(),
            label: "Integration".into(),
            description: None,
            recommended: false,
        },
    ];
    batch.questions.push(second);
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: batch.clone(),
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "batch is pending",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    let client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "questionnaire-batch").unwrap(),
    )
    .await
    .unwrap();
    let invalid = Answer {
        questions: vec![
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["local".into()],
                text: "unsupported combination".into(),
            },
            QuestionAnswer::Selected {
                choices: vec!["unit".into()],
            },
        ],
    };
    assert!(
        client
            .submit_questionnaire(
                live.session_id,
                batch.id,
                QuestionnaireSubmission::Answer { answer: invalid }
            )
            .await
            .is_err()
    );
    let answer = Answer {
        questions: vec![
            QuestionAnswer::Freeform {
                text: "Remote environment".into(),
            },
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["unit".into(), "integration".into()],
                text: "Lint".into(),
            },
        ],
    };
    client
        .submit_questionnaire(
            live.session_id,
            batch.id,
            QuestionnaireSubmission::Answer {
                answer: answer.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        timeout(
            Duration::from_secs(1),
            live.provider_session.next_questionnaire_submission()
        )
        .await
        .unwrap(),
        (
            batch.id,
            QuestionnaireSubmission::Answer {
                answer: answer.clone()
            }
        )
    );
    let snapshot = client.read_session(live.session_id).await.unwrap();
    assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { questionnaire, answer: Some(stored), .. } if questionnaire == &batch && stored == &answer)));
    drop(client);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_questionnaires_keep_ids_order_and_catalog_attention_through_client_updates() {
    use suru::managed_client::{ManagedEvent, SessionEvent};
    use suru::protocol::{SessionChange, TurnStatus};
    let directory = tempfile::tempdir().unwrap();
    let mut live = working_turn(directory.path(), "questionnaire-concurrent").await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "questionnaire-concurrent").unwrap(),
    )
    .await
    .unwrap();
    let mut feed = client.subscribe_session(live.session_id).await.unwrap();
    assert!(matches!(
        feed.next().await.unwrap().unwrap(),
        SessionEvent::Snapshot(_)
    ));
    let first = questionnaire();
    let second = questionnaire();
    for question in [&first, &second] {
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            });
        timeout(Duration::from_secs(2), async {
            loop {
                let SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap() else { continue };
                assert!(!update.changes.iter().any(|c| matches!(c, SessionChange::TurnAdded { .. })));
                if update.changes.iter().any(|c| matches!(c, SessionChange::ActivityAdded { activity: Activity::Questionnaire { questionnaire, .. } } if questionnaire.id == question.id)) { break; }
            }
        }).await.unwrap();
    }
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(ManagedEvent::SessionStandingInputsChanged(changed)) = client.next().await
                && changed.inputs.pending_questionnaires == vec![first.id, second.id]
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let listed = client.list_sessions(None).await.unwrap();
    let summary = listed
        .iter()
        .find_map(|row| row.readable().filter(|s| s.session.id == live.session_id))
        .unwrap();
    assert_eq!(
        summary.standing_inputs.pending_questionnaires,
        vec![first.id, second.id]
    );

    client
        .submit_questionnaire(live.session_id, second.id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    assert_eq!(
        live.provider_session.next_questionnaire_submission().await,
        (second.id, QuestionnaireSubmission::Decline)
    );
    let snapshot = client.read_session(live.session_id).await.unwrap();
    assert_eq!(
        snapshot
            .turns
            .iter()
            .filter(|t| t.status == TurnStatus::Active)
            .count(),
        1
    );
    assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { questionnaire, outcome: QuestionnaireOutcome::Pending, .. } if questionnaire.id == first.id)));
    live.provider_session
        .emit(ProviderEvent::QuestionnaireWithdrawn { id: first.id });
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(ManagedEvent::SessionStandingInputsChanged(changed)) = client.next().await
                && changed.inputs.pending_questionnaires.is_empty()
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        client
            .submit_questionnaire(live.session_id, first.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    drop(feed);
    drop(client);
    live.server.shutdown().await.unwrap();
}
