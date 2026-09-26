use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{read_session_until, working_turn};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, Approval, ApprovalId, ApprovalOutcome, ApprovalSubject, Decision, Question,
        Questionnaire, QuestionnaireId, SessionChange,
    },
    provider::{ProviderActivityId, ProviderEvent, ProviderEventAttribution, ProviderSubagentId},
};
use tokio::time::timeout;

fn approval() -> Approval {
    Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Command {
            command: "cargo nextest run".into(),
            cwd: None,
            actions: Vec::new(),
        },
        reason: Some("Run the project tests".into()),
    }
}

#[tokio::test]
async fn provider_input_is_bounded_only_in_durable_approval_history() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-stored-detail-cap";
    let live = working_turn(directory.path(), channel).await;
    let empty_children = (0..100_000)
        .map(|index| (format!("empty-{index}"), serde_json::json!([])))
        .collect::<serde_json::Map<_, _>>();
    let approval = Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::OtherTool {
            name: "large_native_tool".into(),
            input: serde_json::json!({
                "empty_children": empty_children,
                "payload": "x".repeat(100_000),
            }),
        },
        reason: Some("The Provider keeps this native request whole".into()),
    };
    let original_chars = serde_json::to_string(&approval).unwrap().chars().count();
    live.provider_session
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        })
        .await;
    let snapshot = read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "bounded Approval history",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity, Activity::Approval { approval: stored, .. } if stored.id == approval.id)
            })
        },
    )
    .await;
    let (stored, detail_truncated) = snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval {
                approval: stored,
                detail_truncated,
                ..
            } if stored.id == approval.id => Some((stored, *detail_truncated)),
            _ => None,
        })
        .unwrap();
    let stored_chars = serde_json::to_string(stored).unwrap().chars().count();
    assert!(detail_truncated);
    assert!(
        stored_chars <= 65 * 1024,
        "stored {stored_chars} characters"
    );
    assert!(stored_chars < original_chars);
    assert_eq!(
        serde_json::to_string(&approval).unwrap().chars().count(),
        original_chars,
        "presentation storage must not mutate the Provider-owned request"
    );
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn serialized_escape_overhead_does_not_claim_untruncated_approval_content_was_cut() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-stored-detail-boundary";
    let live = working_turn(directory.path(), channel).await;
    let approval = Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Network {
            // Each character needs escaping on the JSON wire, but the
            // Provider supplied fewer content characters than the durable
            // detail budget permits.
            host_or_url: "\n\"".repeat(32_750),
        },
        reason: None,
    };
    assert!(serde_json::to_string(&approval).unwrap().chars().count() > 64 * 1024);
    live.provider_session
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        })
        .await;
    let snapshot = read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "near-boundary Approval history",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity, Activity::Approval { approval: stored, .. } if stored.id == approval.id)
            })
        },
    )
    .await;
    let Activity::Approval {
        approval: stored,
        detail_truncated,
        ..
    } = snapshot
        .activities
        .iter()
        .find(|activity| {
            matches!(activity, Activity::Approval { approval: stored, .. } if stored.id == approval.id)
        })
        .unwrap()
    else {
        unreachable!()
    };
    assert_eq!(stored, &approval);
    assert!(!detail_truncated);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_events_and_standing_follow_pending_submitting_and_settled_approval() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-live-state";
    let mut live = working_turn(directory.path(), channel).await;
    live.provider_session.gate_decision_deliveries();
    let client = std::sync::Arc::new(
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap(),
    );
    let mut feed = client.subscribe_session(live.session_id).await.unwrap();
    let SessionEvent::Snapshot(initial) = feed.next().await.unwrap().unwrap() else {
        panic!("subscription begins with a snapshot");
    };
    let approval = approval();
    live.provider_session
        .emit(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        });
    let added = timeout(PROGRESS_DEADLINE, async {
        loop {
            let SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap() else {
                continue;
            };
            if update.changes.iter().any(|change| {
                matches!(
                    change,
                    SessionChange::ActivityAdded {
                        activity: Activity::Approval { approval: stored, .. }
                    } if stored == &approval
                )
            }) {
                break update.revision;
            }
        }
    })
    .await
    .unwrap();
    let pending = client.read_session(live.session_id).await.unwrap();
    assert_eq!(pending.pending_approvals, vec![approval.id]);
    assert!(pending.submitting_approvals.is_empty());
    assert!(pending.pending_approvals_revision >= added);
    assert!(pending.pending_approvals_revision > initial.pending_approvals_revision);
    let listed = client.list_sessions(None).await.unwrap();
    let summary = listed
        .iter()
        .find_map(|row| {
            row.readable()
                .filter(|summary| summary.session.id == live.session_id)
        })
        .unwrap();
    assert_eq!(summary.standing_inputs.pending_approval_count(), 1);

    let submitter = client.clone();
    let session_id = live.session_id;
    let approval_id = approval.id;
    let submission = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, approval_id, Decision::Accept)
            .await
    });
    let delivery = timeout(
        PROGRESS_DEADLINE,
        live.provider_session.next_decision_delivery(),
    )
    .await
    .unwrap();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap() else {
                continue;
            };
            if update
                .changes
                .iter()
                .any(|change| matches!(change, SessionChange::DecisionAccepted { .. }))
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let submitting = client.read_session(live.session_id).await.unwrap();
    assert!(submitting.pending_approvals.is_empty());
    assert_eq!(submitting.submitting_approvals, vec![approval.id]);

    delivery.succeed();
    submission.await.unwrap().unwrap();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let SessionEvent::Updated(update) = feed.next().await.unwrap().unwrap() else {
                continue;
            };
            if update.changes.iter().any(|change| {
                matches!(
                    change,
                    SessionChange::ApprovalSettled {
                        outcome: ApprovalOutcome::Decided,
                        decision: Some(Decision::Accept),
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
    let settled = client.read_session(live.session_id).await.unwrap();
    assert!(settled.pending_approvals.is_empty());
    assert!(settled.submitting_approvals.is_empty());
    let listed = client.list_sessions(None).await.unwrap();
    let summary = listed
        .iter()
        .find_map(|row| {
            row.readable()
                .filter(|summary| summary.session.id == live.session_id)
        })
        .unwrap();
    assert_eq!(summary.standing_inputs.pending_approval_count(), 0);
    drop(feed);
    drop(client);
    live.server.shutdown().await.unwrap();
}

fn has_outcome(snapshot: &suru::protocol::SessionSnapshot, expected: ApprovalOutcome) -> bool {
    snapshot.activities.iter().any(
        |activity| matches!(activity, Activity::Approval { outcome, .. } if *outcome == expected),
    )
}

#[tokio::test]
async fn nested_subagent_approvals_roll_up_with_questions_survive_parent_settlement_and_clear_from_the_child()
 {
    use suru::protocol::{SessionId, TurnStatus};

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
                    delegation: None,
                },
            )
            .await;
        client
            .read_session(parent)
            .await
            .unwrap()
            .activities
            .iter()
            .find_map(|activity| match activity {
                Activity::Subagent {
                    name: found,
                    session_id,
                    ..
                } if found == name => Some(*session_id),
                _ => None,
            })
            .unwrap()
    }

    let directory = tempfile::tempdir().unwrap();
    let channel = "nested-approvals";
    let mut live = working_turn(directory.path(), channel).await;
    let client =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
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
    let question = Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "scope".into(),
            title: None,
            text: "Which scope?".into(),
            choices: Vec::new(),
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    };
    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("child")),
            ProviderEvent::QuestionnaireRequested {
                questionnaire: question.clone(),
            },
        )
        .await;
    let request = approval();
    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("grandchild")),
            ProviderEvent::ApprovalRequested {
                approval: request.clone(),
                tool_activity_id: None,
            },
        )
        .await;

    let root = read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "nested Approval and Questionnaire reach the root",
        |snapshot| {
            snapshot.subagent_questionnaire_count() == 1 && snapshot.subagent_approval_count() == 1
        },
    )
    .await;
    assert_eq!(root.pending_approvals_in_subagent(child), 1);
    let child_snapshot = client.read_session(child).await.unwrap();
    assert_eq!(child_snapshot.subagent_approval_count(), 1);
    assert_eq!(child_snapshot.pending_approvals_in_subagent(grandchild), 1);
    let listed = client.list_sessions(None).await.unwrap();
    let standing = &listed[0].readable().unwrap().standing_inputs;
    assert_eq!(standing.pending_questionnaire_count(), 1);
    assert_eq!(standing.pending_approval_count(), 1);

    live.provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled_parent = client.read_session(live.session_id).await.unwrap();
    assert_eq!(settled_parent.turns[0].status, TurnStatus::Completed);
    assert_eq!(settled_parent.subagent_approval_count(), 1);
    assert_eq!(
        client
            .read_session(grandchild)
            .await
            .unwrap()
            .pending_approvals,
        vec![request.id]
    );

    live.provider_session.gate_decision_deliveries();
    let submission = client.submit_decision(grandchild, request.id, Decision::Decline);
    let delivery = async {
        let delivery = timeout(
            PROGRESS_DEADLINE,
            live.provider_session.next_decision_delivery(),
        )
        .await
        .unwrap();
        assert_eq!(delivery.id, request.id);
        assert_eq!(delivery.decision, Decision::Decline);
        let root = read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "submitting descendant Approval reaches every ancestor",
            |snapshot| {
                snapshot.subagent_interventions.iter().any(|entry| {
                    entry.session_id == grandchild && entry.submitting_approvals == vec![request.id]
                })
            },
        )
        .await;
        assert_eq!(root.subagent_approval_count(), 0);
        delivery.succeed();
    };
    let (submitted, ()) = tokio::join!(submission, delivery);
    submitted.unwrap();
    let root = client.read_session(live.session_id).await.unwrap();
    assert_eq!(root.subagent_approval_count(), 0);
    assert_eq!(root.subagent_questionnaire_count(), 1);
    assert_eq!(
        client
            .read_session(child)
            .await
            .unwrap()
            .subagent_approval_count(),
        0
    );
    assert!(matches!(
        client
            .read_session(grandchild)
            .await
            .unwrap()
            .activities
            .iter()
            .find(|activity| matches!(activity, Activity::Approval { approval, .. } if approval.id == request.id)),
        Some(Activity::Approval {
            outcome: ApprovalOutcome::Decided,
            decision: Some(Decision::Decline),
            ..
        })
    ));
    let listed = client.list_sessions(None).await.unwrap();
    let standing = &listed[0].readable().unwrap().standing_inputs;
    assert_eq!(standing.pending_approval_count(), 0);
    assert_eq!(standing.pending_questionnaire_count(), 1);

    live.provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("child")),
            ProviderEvent::QuestionnaireWithdrawn { id: question.id },
        )
        .await;
    let root = client.read_session(live.session_id).await.unwrap();
    assert_eq!(root.subagent_questionnaire_count(), 0);
    let listed = client.list_sessions(None).await.unwrap();
    let standing = &listed[0].readable().unwrap().standing_inputs;
    assert_eq!(standing.pending_questionnaire_count(), 0);
    assert_eq!(standing.pending_approval_count(), 0);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_links_to_an_existing_tool_activity_through_provider_identity() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-tool-link";
    let live = working_turn(directory.path(), channel).await;
    let native_id = ProviderActivityId::new("native-command");
    live.provider_session.emit(ProviderEvent::CommandStarted {
        activity_id: native_id.clone(),
        command: "cargo check".into(),
        cwd: None,
    });
    live.provider_session
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: approval(),
            tool_activity_id: Some(native_id),
        })
        .await;
    let snapshot = read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Approval links to its gated Tool row",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Approval {
                        tool_activity_id: Some(_),
                        ..
                    }
                )
            })
        },
    )
    .await;
    let command_id = snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Command { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    assert!(snapshot.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval { tool_activity_id: Some(id), .. } if *id == command_id
    )));
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn each_decision_crosses_the_public_client_and_is_recorded_in_history() {
    for decision in [
        Decision::Accept,
        Decision::AcceptForSession,
        Decision::Decline,
        Decision::DeclineAndInterrupt,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let channel = format!("approval-decision-{decision:?}");
        let mut live = working_turn(directory.path(), &channel).await;
        let approval = approval();
        live.provider_session
            .emit(ProviderEvent::ApprovalRequested {
                approval: approval.clone(),
                tool_activity_id: None,
            });
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "Approval is pending",
            |snapshot| has_outcome(snapshot, ApprovalOutcome::Pending),
        )
        .await;

        let client =
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), &channel).unwrap())
                .await
                .unwrap();
        client
            .submit_decision(live.session_id, approval.id, decision)
            .await
            .unwrap();
        assert_eq!(
            timeout(PROGRESS_DEADLINE, live.provider_session.next_decision())
                .await
                .unwrap(),
            (approval.id, decision)
        );
        let snapshot = client.read_session(live.session_id).await.unwrap();
        assert!(snapshot.activities.iter().any(|activity| matches!(
            activity,
            Activity::Approval {
                approval: stored,
                outcome: ApprovalOutcome::Decided,
                decision: Some(stored_decision),
                ..
            } if stored == &approval && *stored_decision == decision
        )));
        drop(client);
        live.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn first_decision_wins_across_clients_without_duplicate_provider_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-race";
    let mut live = working_turn(directory.path(), channel).await;
    live.provider_session.gate_decision_deliveries();
    let clients = [
        std::sync::Arc::new(
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
                .await
                .unwrap(),
        ),
        std::sync::Arc::new(
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
                .await
                .unwrap(),
        ),
    ];
    let approval = approval();
    live.provider_session
        .emit(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "Approval is pending",
        |snapshot| has_outcome(snapshot, ApprovalOutcome::Pending),
    )
    .await;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut submissions = tokio::task::JoinSet::new();
    for (client, decision) in clients.iter().zip([Decision::Accept, Decision::Decline]) {
        let client = client.clone();
        let barrier = barrier.clone();
        let session_id = live.session_id;
        let approval_id = approval.id;
        submissions.spawn(async move {
            barrier.wait().await;
            (
                decision,
                client
                    .submit_decision(session_id, approval_id, decision)
                    .await,
            )
        });
    }
    let delivery = timeout(
        PROGRESS_DEADLINE,
        live.provider_session.next_decision_delivery(),
    )
    .await
    .unwrap();
    let (loser, rejected) = timeout(PROGRESS_DEADLINE, submissions.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(rejected.is_err());
    assert_ne!(loser, delivery.decision);
    assert!(
        clients[0]
            .submit_decision(live.session_id, approval.id, Decision::AcceptForSession)
            .await
            .is_err()
    );
    delivery.succeed();
    let (winner, accepted) = timeout(PROGRESS_DEADLINE, submissions.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    accepted.unwrap();
    assert_ne!(winner, loser);
    let snapshot = clients[0].read_session(live.session_id).await.unwrap();
    assert!(snapshot.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval {
            outcome: ApprovalOutcome::Decided,
            decision: Some(stored),
            ..
        } if *stored == winner
    )));
    drop(clients);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn definite_rejection_is_answerable_but_uncertain_delivery_is_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-delivery-outcomes";
    let mut live = working_turn(directory.path(), channel).await;
    live.provider_session.gate_decision_deliveries();
    let client = std::sync::Arc::new(
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap(),
    );

    let rejected = approval();
    live.provider_session
        .emit(ProviderEvent::ApprovalRequested {
            approval: rejected.clone(),
            tool_activity_id: None,
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "first Approval is pending",
        |snapshot| snapshot.pending_approvals == vec![rejected.id],
    )
    .await;
    let submitter = client.clone();
    let session_id = live.session_id;
    let rejected_id = rejected.id;
    let first = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, rejected_id, Decision::Decline)
            .await
    });
    live.provider_session
        .next_decision_delivery()
        .await
        .reject();
    assert!(first.await.unwrap().is_err());
    let retryable = client.read_session(live.session_id).await.unwrap();
    assert_eq!(retryable.pending_approvals, vec![rejected.id]);
    assert!(has_outcome(&retryable, ApprovalOutcome::SubmissionRejected));
    let submitter = client.clone();
    let retry = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, rejected_id, Decision::AcceptForSession)
            .await
    });
    live.provider_session
        .next_decision_delivery()
        .await
        .succeed();
    retry.await.unwrap().unwrap();

    let uncertain = approval();
    live.provider_session
        .emit(ProviderEvent::ApprovalRequested {
            approval: uncertain.clone(),
            tool_activity_id: None,
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "second Approval is pending",
        |snapshot| snapshot.pending_approvals == vec![uncertain.id],
    )
    .await;
    let submitter = client.clone();
    let uncertain_id = uncertain.id;
    let uncertain_submission = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, uncertain_id, Decision::Accept)
            .await
    });
    live.provider_session.next_decision_delivery().await.fail();
    assert!(uncertain_submission.await.unwrap().is_err());
    let terminal = client.read_session(live.session_id).await.unwrap();
    assert!(has_outcome(&terminal, ApprovalOutcome::DeliveryUncertain));
    assert!(terminal.pending_approvals.is_empty());
    assert!(
        client
            .submit_decision(live.session_id, uncertain.id, Decision::Decline)
            .await
            .is_err()
    );
    drop(client);
    live.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn withdrawal_and_interrupt_settle_submitting_and_pending_approvals_without_late_overwrite() {
    for interrupt in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let channel = if interrupt {
            "approval-interrupt-race"
        } else {
            "approval-withdraw-race"
        };
        let mut live = working_turn(directory.path(), channel).await;
        live.provider_session.gate_decision_deliveries();
        let client = std::sync::Arc::new(
            ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
                .await
                .unwrap(),
        );
        let submitting = approval();
        live.provider_session
            .emit(ProviderEvent::ApprovalRequested {
                approval: submitting.clone(),
                tool_activity_id: None,
            });
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "Approval is pending",
            |snapshot| snapshot.pending_approvals == vec![submitting.id],
        )
        .await;
        let submitter = client.clone();
        let session_id = live.session_id;
        let approval_id = submitting.id;
        let submission = tokio::spawn(async move {
            submitter
                .submit_decision(session_id, approval_id, Decision::Decline)
                .await
        });
        let delivery = timeout(
            PROGRESS_DEADLINE,
            live.provider_session.next_decision_delivery(),
        )
        .await
        .unwrap();

        let expected = if interrupt {
            let pending = approval();
            live.provider_session
                .emit(ProviderEvent::ApprovalRequested {
                    approval: pending.clone(),
                    tool_activity_id: None,
                });
            read_session_until(
                &live.client,
                live.server.descriptor(),
                live.session_id,
                "second Approval is pending",
                |snapshot| snapshot.pending_approvals == vec![pending.id],
            )
            .await;
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
            ApprovalOutcome::TurnEnded
        } else {
            live.provider_session
                .emit(ProviderEvent::ApprovalWithdrawn { id: submitting.id });
            ApprovalOutcome::Withdrawn
        };
        read_session_until(
            &live.client,
            live.server.descriptor(),
            live.session_id,
            "Approval settles independently of blocked delivery",
            |snapshot| has_outcome(snapshot, expected),
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
        let snapshot = client.read_session(session_id).await.unwrap();
        assert!(snapshot.pending_approvals.is_empty());
        assert!(snapshot.submitting_approvals.is_empty());
        assert!(
            snapshot
                .activities
                .iter()
                .filter(|activity| matches!(
                    activity,
                    Activity::Approval { outcome, .. } if *outcome == expected
                ))
                .count()
                >= if interrupt { 2 } else { 1 }
        );
        assert!(
            client
                .submit_decision(session_id, submitting.id, Decision::Accept)
                .await
                .is_err()
        );
        drop(client);
        live.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn restart_keeps_approval_history_but_disables_abandoned_provider_requests() {
    let directory = tempfile::tempdir().unwrap();
    let channel = "approval-restart";
    let config = suru::server::ServerConfig::new(directory.path(), channel).unwrap();
    let mut live = working_turn(directory.path(), channel).await;
    live.provider_session.gate_decision_deliveries();
    let client = std::sync::Arc::new(
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap(),
    );
    let decided = approval();
    live.provider_session
        .emit(ProviderEvent::ApprovalRequested {
            approval: decided.clone(),
            tool_activity_id: None,
        });
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "first Approval is pending",
        |snapshot| snapshot.pending_approvals == vec![decided.id],
    )
    .await;
    let submitter = client.clone();
    let session_id = live.session_id;
    let decided_id = decided.id;
    let decided_submission = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, decided_id, Decision::AcceptForSession)
            .await
    });
    live.provider_session
        .next_decision_delivery()
        .await
        .succeed();
    decided_submission.await.unwrap().unwrap();
    let pending = approval();
    let submitting = approval();
    for approval in [&pending, &submitting] {
        live.provider_session
            .emit(ProviderEvent::ApprovalRequested {
                approval: approval.clone(),
                tool_activity_id: None,
            });
    }
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "both Approvals are pending",
        |snapshot| snapshot.pending_approvals.len() == 2,
    )
    .await;
    let submitter = client.clone();
    let session_id = live.session_id;
    let submitting_id = submitting.id;
    let task = tokio::spawn(async move {
        submitter
            .submit_decision(session_id, submitting_id, Decision::Accept)
            .await
    });
    let delivery = timeout(
        PROGRESS_DEADLINE,
        live.provider_session.next_decision_delivery(),
    )
    .await
    .unwrap();
    read_session_until(
        &live.client,
        live.server.descriptor(),
        live.session_id,
        "one Approval is submitting",
        |snapshot| snapshot.submitting_approvals == vec![submitting.id],
    )
    .await;
    task.abort();
    let _ = task.await;
    drop(client);
    live.server.shutdown().await.unwrap();

    drop(delivery);
    let restarted = suru::server::spawn_with_provider(config, live.runtime)
        .await
        .unwrap();
    let client =
        ManagedClient::connect(ManagedClientConfig::new(directory.path(), channel).unwrap())
            .await
            .unwrap();
    let historical = client.read_session(session_id).await.unwrap();
    assert!(historical.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval {
            approval,
            outcome: ApprovalOutcome::Decided,
            decision: Some(Decision::AcceptForSession),
            ..
        } if approval.id == decided.id
    )));
    assert!(historical.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Unavailable, .. }
            if approval.id == pending.id
    )));
    assert!(historical.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::DeliveryUncertain, .. }
            if approval.id == submitting.id
    )));
    assert!(historical.pending_approvals.is_empty());
    assert!(historical.submitting_approvals.is_empty());
    assert!(
        client
            .submit_decision(session_id, pending.id, Decision::Decline)
            .await
            .is_err()
    );
    drop(client);
    restarted.shutdown().await.unwrap();
}
