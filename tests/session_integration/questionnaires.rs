use crate::server_support::PROGRESS_DEADLINE;
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
        PROGRESS_DEADLINE,
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
            PROGRESS_DEADLINE,
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
        timeout(PROGRESS_DEADLINE, async {
            loop {
                let SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap() else { continue };
                assert!(!update.changes.iter().any(|c| matches!(c, SessionChange::TurnAdded { .. })));
                if update.changes.iter().any(|c| matches!(c, SessionChange::ActivityAdded { activity: Activity::Questionnaire { questionnaire, .. } } if questionnaire.id == question.id)) { break; }
            }
        }).await.unwrap();
    }
    timeout(PROGRESS_DEADLINE, async {
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
    timeout(PROGRESS_DEADLINE, async {
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

async fn streamed_outcome(
    feed: &mut suru::managed_client::SessionSubscription,
    expected: QuestionnaireOutcome,
) {
    use suru::{managed_client::SessionEvent, protocol::SessionChange};
    timeout(PROGRESS_DEADLINE, async {
        loop {
            match feed.next().await.unwrap().unwrap() {
                SessionEvent::Updated(update)
                    if update.changes.iter().any(|change| match change {
                        SessionChange::QuestionnaireAccepted { .. } => {
                            expected == QuestionnaireOutcome::Submitting
                        }
                        SessionChange::QuestionnaireSettled { outcome, .. } => *outcome == expected,
                        _ => false,
                    }) =>
                {
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("viewing Client receives authoritative Questionnaire state");
}

#[tokio::test]
async fn racing_clients_publish_acceptance_and_one_outcome_without_duplicate_provider_delivery() {
    use std::sync::Arc;
    for competing_decline in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let channel = "questionnaire-race";
        let mut live = working_turn(directory.path(), channel).await;
        live.provider_session.gate_questionnaire_deliveries();
        let mut clients = Vec::new();
        let mut feeds = Vec::new();
        for _ in 0..2 {
            let client = Arc::new(
                ManagedClient::connect(
                    ManagedClientConfig::new(directory.path(), channel).unwrap(),
                )
                .await
                .unwrap(),
            );
            let mut feed = client.subscribe_session(live.session_id).await.unwrap();
            feed.next().await.unwrap().unwrap();
            clients.push(client);
            feeds.push(feed);
        }
        let question = questionnaire();
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            });
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "question is live",
            |s| outcome(s, QuestionnaireOutcome::Pending),
        )
        .await;
        let answers = [
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![QuestionAnswer::Freeform {
                        text: "First Client's answer".into(),
                    }],
                },
            },
            if competing_decline {
                QuestionnaireSubmission::Decline
            } else {
                QuestionnaireSubmission::Answer {
                    answer: Answer {
                        questions: vec![QuestionAnswer::Freeform {
                            text: "Second Client's answer".into(),
                        }],
                    },
                }
            },
        ];
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut submissions = tokio::task::JoinSet::new();
        for (client, submission) in clients.iter().zip(&answers) {
            let client = client.clone();
            let submission = submission.clone();
            let barrier = barrier.clone();
            let session_id = live.session_id;
            let id = question.id;
            submissions.spawn(async move {
                barrier.wait().await;
                client
                    .submit_questionnaire(session_id, id, submission)
                    .await
            });
        }
        let delivery = timeout(
            PROGRESS_DEADLINE,
            live.provider_session.next_questionnaire_delivery(),
        )
        .await
        .unwrap();
        assert_eq!(delivery.id, question.id);
        assert!(answers.contains(&delivery.submission));
        // The winner is still waiting at the Provider. The losing request must
        // already be rejected; an actor blocked on delivery would time out here.
        assert!(
            timeout(PROGRESS_DEADLINE, submissions.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_err()
        );
        for feed in &mut feeds {
            streamed_outcome(feed, QuestionnaireOutcome::Submitting).await;
        }
        for client in &clients {
            let snapshot = client.read_session(live.session_id).await.unwrap();
            assert!(snapshot.activities.iter().any(|a| matches!(
                a,
                Activity::Questionnaire {
                    outcome: QuestionnaireOutcome::Submitting,
                    answer: None,
                    ..
                }
            )));
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
        }
        let (expected, stored) = match &delivery.submission {
            QuestionnaireSubmission::Answer { answer } => {
                (QuestionnaireOutcome::Answered, Some(answer.clone()))
            }
            QuestionnaireSubmission::Decline => (QuestionnaireOutcome::Declined, None),
        };
        delivery.succeed();
        timeout(PROGRESS_DEADLINE, submissions.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        for feed in &mut feeds {
            streamed_outcome(feed, expected).await;
        }
        for client in &clients {
            let snapshot = client.read_session(live.session_id).await.unwrap();
            assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { outcome, answer, .. } if *outcome == expected && *answer == stored)));
            assert!(
                client
                    .submit_questionnaire(live.session_id, question.id, answers[0].clone())
                    .await
                    .is_err()
            );
        }
        // Duplicate native arrivals cannot resurrect the already consumed ID.
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            });
        live.provider_session
            .emit_and_wait_until_observed(ProviderEvent::AgentMessageDelta {
                content: "Continued".into(),
            })
            .await;
        assert!(
            clients[0]
                .submit_questionnaire(
                    live.session_id,
                    question.id,
                    QuestionnaireSubmission::Decline
                )
                .await
                .is_err()
        );
        assert!(
            timeout(
                Duration::from_millis(20),
                live.provider_session.next_questionnaire_delivery()
            )
            .await
            .is_err()
        );
        drop(feeds);
        drop(clients);
        live.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn closing_and_reconnecting_clients_preserves_the_live_provider_request() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "questionnaire-reconnect";
    let mut live = working_turn(directory.path(), channel).await;
    let first =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
    let question = questionnaire();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: question.clone(),
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "question is live",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    assert!(outcome(
        &first.read_session(live.session_id).await.unwrap(),
        QuestionnaireOutcome::Pending
    ));
    drop(first);
    let second =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
    let snapshot = second.read_session(live.session_id).await.unwrap();
    assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { questionnaire: found, outcome: QuestionnaireOutcome::Pending, answer: None, .. } if found.id == question.id)));
    second
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
    drop(second);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn gated_delivery_does_not_block_withdrawal_or_interruption_and_cannot_overwrite_them() {
    use std::sync::Arc;
    for interrupt in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let channel = "questionnaire-delivery-lifecycle";
        let mut live = working_turn(directory.path(), channel).await;
        live.provider_session.gate_questionnaire_deliveries();
        let client = Arc::new(
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
                .await
                .unwrap(),
        );
        let question = questionnaire();
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            });
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "question is live",
            |s| outcome(s, QuestionnaireOutcome::Pending),
        )
        .await;
        let submitter = client.clone();
        let session_id = live.session_id;
        let submission = tokio::spawn(async move {
            submitter
                .submit_questionnaire(session_id, question.id, QuestionnaireSubmission::Decline)
                .await
        });
        let delivery = timeout(
            PROGRESS_DEADLINE,
            live.provider_session.next_questionnaire_delivery(),
        )
        .await
        .unwrap();
        let expected = if interrupt {
            let interrupter = client.clone();
            let interruption =
                tokio::spawn(async move { interrupter.interrupt_session(session_id).await });
            timeout(PROGRESS_DEADLINE, live.provider_session.next_interrupt())
                .await
                .unwrap()
                .succeed();
            live.provider_session.emit(ProviderEvent::TurnInterrupted);
            timeout(PROGRESS_DEADLINE, interruption)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            QuestionnaireOutcome::TurnEnded
        } else {
            live.provider_session
                .emit(ProviderEvent::QuestionnaireWithdrawn { id: question.id });
            QuestionnaireOutcome::Withdrawn
        };
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "inflight question is unavailable",
            |s| outcome(s, expected),
        )
        .await;
        delivery.succeed();
        assert!(
            timeout(PROGRESS_DEADLINE, submission)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(outcome(
            &client.read_session(session_id).await.unwrap(),
            expected
        ));
        assert!(
            client
                .submit_questionnaire(session_id, question.id, QuestionnaireSubmission::Decline)
                .await
                .is_err()
        );
        drop(client);
        live.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn nested_subagent_questionnaires_keep_ancestor_attention_and_answer_in_the_child_after_parent_settles()
 {
    use suru::protocol::{SessionChange, SessionId, SessionSnapshot, TurnStatus};
    use suru::provider::{ProviderEventAttribution, ProviderSubagentId};
    let directory = tempfile::tempdir().unwrap();
    let mut live = working_turn(directory.path(), "nested-questionnaires").await;
    let client = ManagedClient::connect(
        ManagedClientConfig::new(directory.path(), "nested-questionnaires").unwrap(),
    )
    .await
    .unwrap();
    async fn spawn(
        live: &crate::support::WorkingTurn,
        client: &ManagedClient,
        parent: SessionId,
        attribution: ProviderEventAttribution,
        name: &str,
    ) -> SessionId {
        live.provider_session
            .emit_attributed_and_wait_until_observed(
                attribution,
                ProviderEvent::SubagentStarted {
                    subagent_id: ProviderSubagentId::new(name),
                    name: name.into(),
                    description: name.into(),
                },
            )
            .await;
        let snapshot = client.read_session(parent).await.unwrap();
        snapshot
            .activities
            .iter()
            .find_map(|a| match a {
                Activity::Subagent {
                    name: found,
                    session_id,
                    ..
                } if found == name => Some(*session_id),
                _ => None,
            })
            .unwrap()
    }
    let child = spawn(
        &live,
        &client,
        live.session_id,
        ProviderEventAttribution::OwningSession,
        "child",
    )
    .await;
    let grandchild = spawn(
        &live,
        &client,
        child,
        ProviderEventAttribution::Subagent(ProviderSubagentId::new("child")),
        "grandchild",
    )
    .await;
    let first = questionnaire();
    let second = questionnaire();
    for (name, request) in [("child", &first), ("grandchild", &second)] {
        live.provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(name)),
                ProviderEvent::QuestionnaireRequested {
                    questionnaire: request.clone(),
                },
            )
            .await;
    }
    let mut feed = client.subscribe_session(live.session_id).await.unwrap();
    let suru::managed_client::SessionEvent::Snapshot(parent) = feed.next().await.unwrap().unwrap()
    else {
        panic!("snapshot")
    };
    assert_eq!(parent.subagent_questionnaire_count(), 2);
    assert_eq!(parent.pending_questionnaires_in_subagent(child), 2);
    assert!(
        !parent
            .activities
            .iter()
            .any(|a| matches!(a, Activity::Questionnaire { .. }))
    );
    let nested = client.read_session(child).await.unwrap();
    assert_eq!(nested.subagent_questionnaire_count(), 1);
    assert_eq!(nested.pending_questionnaires_in_subagent(grandchild), 1);
    let listed = client.list_sessions(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]
            .readable()
            .unwrap()
            .standing_inputs
            .pending_questionnaire_count(),
        2
    );

    live.provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let parent = client.read_session(live.session_id).await.unwrap();
    assert_eq!(parent.turns[0].status, TurnStatus::Completed);
    assert_eq!(parent.subagent_questionnaire_count(), 2);
    let answer = Answer {
        questions: vec![QuestionAnswer::Freeform {
            text: "Child answer".into(),
        }],
    };
    live.provider_session.gate_questionnaire_deliveries();
    let submission = QuestionnaireSubmission::Answer {
        answer: answer.clone(),
    };
    let (submitted, ()) = tokio::join!(
        client.submit_questionnaire(child, first.id, submission.clone()),
        async {
            let delivery = timeout(
                PROGRESS_DEADLINE,
                live.provider_session.next_questionnaire_delivery(),
            )
            .await
            .unwrap();
            assert_eq!(delivery.id, first.id);
            assert_eq!(delivery.submission, submission);
            // A settled parent still arbitrates its child's live request while
            // delivery is waiting, without blocking the actor on that delivery.
            assert!(
                timeout(
                    PROGRESS_DEADLINE,
                    client.submit_questionnaire(child, first.id, QuestionnaireSubmission::Decline)
                )
                .await
                .unwrap()
                .is_err()
            );
            assert!(outcome(
                &client.read_session(child).await.unwrap(),
                QuestionnaireOutcome::Submitting
            ));
            delivery.succeed();
        }
    );
    submitted.unwrap();
    live.provider_session.ungate_questionnaire_deliveries();
    let parent = client.read_session(live.session_id).await.unwrap();
    assert_eq!(parent.subagent_questionnaire_count(), 1);
    assert_eq!(
        parent.turns.len(),
        1,
        "answer does not create a Continuation"
    );
    assert!(
        !parent
            .activities
            .iter()
            .any(|a| matches!(a, Activity::Questionnaire { .. }))
    );
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let suru::managed_client::SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap()
                && update.changes.iter().any(|c| matches!(c, SessionChange::SubagentInterventionsChanged { subagent_interventions } if subagent_interventions.iter().map(|q| q.pending_questionnaires.len()).sum::<usize>() == 1)) { break; }
        }
    }).await.unwrap();
    client
        .submit_questionnaire(grandchild, second.id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    assert_eq!(
        live.provider_session.next_questionnaire_submission().await,
        (second.id, QuestionnaireSubmission::Decline)
    );
    assert_eq!(
        client
            .read_session(live.session_id)
            .await
            .unwrap()
            .subagent_questionnaire_count(),
        0
    );

    let withdrawn = questionnaire();
    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("grandchild")),
            ProviderEvent::QuestionnaireRequested {
                questionnaire: withdrawn.clone(),
            },
        )
        .await;
    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("grandchild")),
            ProviderEvent::QuestionnaireWithdrawn { id: withdrawn.id },
        )
        .await;
    assert_eq!(
        client
            .read_session(live.session_id)
            .await
            .unwrap()
            .subagent_questionnaire_count(),
        0
    );
    let interrupted = questionnaire();
    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("grandchild")),
            ProviderEvent::QuestionnaireRequested {
                questionnaire: interrupted.clone(),
            },
        )
        .await;
    let (interrupted_result, ()) = tokio::join!(client.interrupt_session(live.session_id), async {
        live.provider_session.next_subagents_stop().await.succeed();
    });
    interrupted_result.unwrap();
    let parent: SessionSnapshot = client.read_session(live.session_id).await.unwrap();
    assert_eq!(parent.subagent_questionnaire_count(), 0);
    assert!(
        client
            .submit_questionnaire(grandchild, interrupted.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    drop(feed);
    drop(client);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn definite_rejection_preserves_live_identity_for_explicit_retry_but_uncertainty_consumes_it()
{
    use std::sync::Arc;
    for definite in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let channel = "questionnaire-recovery";
        let mut live = working_turn(directory.path(), channel).await;
        live.provider_session.gate_questionnaire_deliveries();
        let client = Arc::new(
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
                .await
                .unwrap(),
        );
        let question = questionnaire();
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            });
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "live question",
            |s| outcome(s, QuestionnaireOutcome::Pending),
        )
        .await;
        let submitter = client.clone();
        let session_id = live.session_id;
        let id = question.id;
        let submission = tokio::spawn(async move {
            submitter
                .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
                .await
        });
        let delivery = timeout(
            PROGRESS_DEADLINE,
            live.provider_session.next_questionnaire_delivery(),
        )
        .await
        .unwrap();
        let listing = client.list_sessions(None).await.unwrap();
        let standing = &listing
            .iter()
            .find_map(|row| row.readable().filter(|s| s.session.id == session_id))
            .unwrap()
            .standing_inputs;
        assert!(standing.pending_questionnaires.is_empty());
        assert_eq!(standing.submitting_questionnaires, vec![id]);
        if definite {
            delivery.reject();
        } else {
            drop(delivery);
        }
        let error = timeout(PROGRESS_DEADLINE, submission)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains(if definite {
                "not delivered"
            } else {
                "uncertain"
            }),
            "{error}"
        );
        let expected = if definite {
            QuestionnaireOutcome::SubmissionRejected
        } else {
            QuestionnaireOutcome::DeliveryUncertain
        };
        assert!(outcome(
            &client.read_session(session_id).await.unwrap(),
            expected
        ));
        // A repeated native event cannot make a consumed delivery live again.
        live.provider_session
            .emit(ProviderEvent::QuestionnaireRequested {
                questionnaire: question,
            });
        if definite {
            let submitter = client.clone();
            let retry = tokio::spawn(async move {
                submitter
                    .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
                    .await
            });
            timeout(
                PROGRESS_DEADLINE,
                live.provider_session.next_questionnaire_delivery(),
            )
            .await
            .unwrap()
            .succeed();
            retry.await.unwrap().unwrap();
            assert!(outcome(
                &client.read_session(session_id).await.unwrap(),
                QuestionnaireOutcome::Declined
            ));
        }
        assert!(
            client
                .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
                .await
                .is_err()
        );
        assert!(
            timeout(
                Duration::from_millis(30),
                live.provider_session.next_questionnaire_delivery()
            )
            .await
            .is_err()
        );
        drop(client);
        live.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn lost_client_acknowledgement_reconciles_acceptance_without_duplicate_delivery() {
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let channel = "questionnaire-lost-ack";
    let mut live = working_turn(directory.path(), channel).await;
    live.provider_session.gate_questionnaire_deliveries();
    let client = Arc::new(
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap(),
    );
    let question = questionnaire();
    live.provider_session
        .emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: question.clone(),
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "live question",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    let submitter = client.clone();
    let session_id = live.session_id;
    let id = question.id;
    let submission = tokio::spawn(async move {
        submitter
            .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
            .await
    });
    let delivery = timeout(
        PROGRESS_DEADLINE,
        live.provider_session.next_questionnaire_delivery(),
    )
    .await
    .unwrap();
    // Drop the HTTP response future after the server won arbitration, before ack.
    submission.abort();
    assert!(submission.await.unwrap_err().is_cancelled());
    assert!(outcome(
        &client.read_session(session_id).await.unwrap(),
        QuestionnaireOutcome::Submitting
    ));
    assert!(
        client
            .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    delivery.succeed();
    read_session_until(
        &live.client,
        live.server.descriptor(),
        session_id,
        "server confirms delivery despite lost ack",
        |s| outcome(s, QuestionnaireOutcome::Declined),
    )
    .await;
    assert!(
        client
            .submit_questionnaire(session_id, id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    assert!(
        timeout(
            Duration::from_millis(30),
            live.provider_session.next_questionnaire_delivery()
        )
        .await
        .is_err()
    );
    drop(client);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_requires_a_genuinely_reissued_live_request_and_does_not_revive_consumed_identity()
{
    use crate::{provider_support::ControlledProvider, support::controlled_selection};
    use suru::{
        protocol::{
            AdmitPromptRequest, AgentId, AgentIdentity, CreateSessionRequest, InitialPrompt,
            PromptDelivery, PromptId,
        },
        server::{self, ServerConfig},
    };
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "questionnaire-restoration";
    let config = ServerConfig::new(directory.path(), channel).unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let client = std::sync::Arc::new(
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap(),
    );
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start".into(),
                skill_invocations: vec![],
            },
        })
        .await
        .unwrap();
    let session_id = created.session.id;
    let identity = AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-restore", "high", "fast"),
    };
    let mut native = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .unwrap()
        .succeed(identity.clone());
    native.next_turn().await.succeed();
    let pending = questionnaire();
    let consumed = questionnaire();
    for request in [&pending, &consumed] {
        native.emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: request.clone(),
        });
    }
    read_session_until(
        &reqwest::Client::new(),
        original.descriptor(),
        session_id,
        "both requests live",
        |s| {
            s.activities
                .iter()
                .filter(|a| matches!(a, Activity::Questionnaire { .. }))
                .count()
                == 2
        },
    )
    .await;
    client
        .submit_questionnaire(session_id, consumed.id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    native.next_questionnaire_submission().await;
    let uncertain = questionnaire();
    native.gate_questionnaire_deliveries();
    native.emit(ProviderEvent::QuestionnaireRequested {
        questionnaire: uncertain.clone(),
    });
    read_session_until(&reqwest::Client::new(), original.descriptor(), session_id, "third request live", |s| s.activities.iter().any(|a| matches!(a, Activity::Questionnaire { questionnaire, .. } if questionnaire.id == uncertain.id))).await;
    let submitter = client.clone();
    let uncertain_id = uncertain.id;
    let submission = tokio::spawn(async move {
        submitter
            .submit_questionnaire(session_id, uncertain_id, QuestionnaireSubmission::Decline)
            .await
    });
    let delivery = timeout(PROGRESS_DEADLINE, native.next_questionnaire_delivery())
        .await
        .unwrap();
    submission.abort();
    let _ = submission.await;
    drop(client);
    original.shutdown().await.unwrap();
    // Seed a terminal persisted Turn with abandoned request history. Native
    // restoration is tested on the next real conversation, independently of
    // the existing recovery limitation for persisted Active Turns.
    {
        use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
        let mut database =
            SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
                .unwrap();
        database
            .batch_execute("UPDATE turns SET payload = json_set(payload, '$.status', 'completed');")
            .unwrap();
    }
    let (runtime, mut provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, runtime).await.unwrap();
    let client =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
    let historical = client.read_session(session_id).await.unwrap();
    assert!(outcome(&historical, QuestionnaireOutcome::Unavailable));
    assert!(outcome(&historical, QuestionnaireOutcome::Declined));
    assert!(outcome(
        &historical,
        QuestionnaireOutcome::DeliveryUncertain
    ));
    drop(delivery);
    let listing = client.list_sessions(None).await.unwrap();
    let standing = &listing
        .iter()
        .find_map(|row| row.readable())
        .unwrap()
        .standing_inputs;
    assert!(
        standing.pending_questionnaires.is_empty() && standing.submitting_questionnaires.is_empty()
    );
    assert!(
        client
            .submit_questionnaire(session_id, pending.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Resume".into(),
                    skill_invocations: vec![],
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .unwrap();
    let mut native = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .unwrap()
        .succeed(identity);
    timeout(PROGRESS_DEADLINE, native.next_turn())
        .await
        .unwrap()
        .succeed();
    // Only this real event reinstates the previously unavailable request. The
    // consumed request is deliberately repeated too and must stay consumed.
    for request in [&pending, &consumed, &uncertain] {
        native.emit(ProviderEvent::QuestionnaireRequested {
            questionnaire: request.clone(),
        });
    }
    let restored = read_session_until(
        &reqwest::Client::new(),
        restarted.descriptor(),
        session_id,
        "Provider reissued live request",
        |s| outcome(s, QuestionnaireOutcome::Pending),
    )
    .await;
    assert_eq!(restored.activities.iter().filter(|a| matches!(a, Activity::Questionnaire { questionnaire, .. } if questionnaire.id == consumed.id)).count(), 1);
    client
        .submit_questionnaire(session_id, pending.id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    assert_eq!(
        native.next_questionnaire_submission().await,
        (pending.id, QuestionnaireSubmission::Decline)
    );
    assert!(
        client
            .submit_questionnaire(session_id, consumed.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    assert!(
        client
            .submit_questionnaire(session_id, uncertain.id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    drop(client);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn secret_questionnaire_answer_is_private_in_client_updates_and_persisted_history() {
    use suru::managed_client::SessionEvent;
    use suru::protocol::SessionChange;
    const SECRET: &str = "private-cross-platform-credential";
    let directory = tempfile::tempdir().unwrap();
    let channel = "questionnaire-secret";
    let mut live = working_turn(directory.path(), channel).await;
    let mut question = questionnaire();
    question.questions[0].secret = true;
    let client =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
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
        "secret Questionnaire is pending",
        |snapshot| outcome(snapshot, QuestionnaireOutcome::Pending),
    )
    .await;
    let mut observer = client.subscribe_session(live.session_id).await.unwrap();
    observer.next().await.unwrap().unwrap();
    let submission = QuestionnaireSubmission::Answer {
        answer: Answer {
            questions: vec![QuestionAnswer::Freeform {
                text: SECRET.into(),
            }],
        },
    };
    assert!(!format!("{submission:?}").contains(SECRET));
    client
        .submit_questionnaire(live.session_id, question.id, submission.clone())
        .await
        .unwrap();
    assert_eq!(
        live.provider_session.next_questionnaire_submission().await,
        (question.id, submission)
    );
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let SessionEvent::Updated(update) = observer.next().await.unwrap().unwrap() else {
                continue;
            };
            assert!(!serde_json::to_string(&update).unwrap().contains(SECRET));
            if update.changes.iter().any(|change| {
                matches!(
                    change,
                    SessionChange::QuestionnaireSettled {
                        outcome: QuestionnaireOutcome::Answered,
                        ..
                    }
                )
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let snapshot = client.read_session(live.session_id).await.unwrap();
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(SECRET));
    assert!(snapshot.activities.iter().any(|activity| matches!(activity, Activity::Questionnaire { answer: Some(answer), .. } if answer.questions == vec![QuestionAnswer::SecretAnswered])));
    drop(observer);
    drop(client);
    live.server.shutdown().await.unwrap();
    let server = suru::server::spawn_with_provider(
        suru::server::ServerConfig::new(directory.path(), channel).unwrap(),
        live.runtime,
    )
    .await
    .unwrap();
    let client =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
    let restored = client.read_session(live.session_id).await.unwrap();
    assert_eq!(restored.activities, snapshot.activities);
    assert!(!serde_json::to_string(&restored).unwrap().contains(SECRET));
    drop(client);
    server.shutdown().await.unwrap();
}
