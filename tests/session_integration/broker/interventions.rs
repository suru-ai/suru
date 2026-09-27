//! Interventions a brokered Subagent raises. Suru runs a brokered Subagent on
//! a Provider actor of its own (ADR 0035), so an Approval or Questionnaire its
//! Provider raises belongs to the Subagent's own Session and crosses the
//! client as a native Subagent's does: listed on that Session, marking that
//! Session's entry in the Subagent tree and its top-level Session's Standing.
//! The Decision or Answer the client submits there reaches the Subagent's own
//! Provider — never the one its parent runs on — and the client-facing
//! protocol is the one a native Subagent's Interventions use.

use suru::protocol::{
    Answer, Approval, ApprovalId, ApprovalOutcome, ApprovalSubject, Decision, Question,
    QuestionAnswer, Questionnaire, QuestionnaireId, QuestionnaireOutcome, QuestionnaireSubmission,
    SessionListItem, SessionStandingInputs, SubagentTreeSnapshot,
};

use super::*;

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

fn questionnaire() -> Questionnaire {
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "scope".into(),
            title: None,
            text: "Which seams should the survey cover?".into(),
            choices: Vec::new(),
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    }
}

/// The facts `session_id`'s Sidebar row derives its Standing from, as the
/// Server's listing gives them.
async fn standing(descriptor: &RuntimeDescriptor, session_id: SessionId) -> SessionStandingInputs {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("the listing answers")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Session listing")
        .iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == session_id)
                .map(|summary| summary.standing_inputs.clone())
        })
        .expect("the Session is listed and readable")
}

/// Each entry's own-Intervention flag in `tree`, top-level Session first.
fn intervention_flags(tree: &SubagentTreeSnapshot) -> Vec<(SessionId, bool)> {
    std::iter::once((tree.top_level.session_id, tree.top_level.needs_intervention))
        .chain(
            tree.subagents
                .iter()
                .map(|entry| (entry.session_id, entry.needs_intervention)),
        )
        .collect()
}

/// The next change to the tree that flags or clears an entry's
/// Intervention.
async fn next_intervention_change(
    updates: &mut crate::subagent_tree::TreeUpdates,
    revision: &mut suru::protocol::SubagentTreeRevision,
) -> SubagentTreeChange {
    changes_until(updates, revision, |change| {
        matches!(change, SubagentTreeChange::NeedsInterventionChanged { .. })
    })
    .await
    .pop()
    .expect("the change looked for is the last one read")
}

/// The client's Decision on `approval`, submitted to the Session that owns
/// it, answering with the raw response.
async fn submit_decision(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    approval: ApprovalId,
    decision: Decision,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/approvals/{approval}/decision",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&decision)
        .send()
        .await
        .expect("submit the Decision")
}

/// The client's Answer to `questionnaire`, submitted to the Session that owns
/// it, answering with the raw response.
async fn submit_questionnaire(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    questionnaire: QuestionnaireId,
    submission: &QuestionnaireSubmission,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/questionnaires/{questionnaire}",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(submission)
        .send()
        .await
        .expect("submit the Answer")
}

/// The Approval Activity `snapshot` holds for `approval`, if any.
fn find_approval<'a>(snapshot: &'a SessionSnapshot, approval: &Approval) -> Option<&'a Activity> {
    snapshot.activities.iter().find(|activity| match activity {
        Activity::Approval { approval: held, .. } => held.id == approval.id,
        _ => false,
    })
}

fn approval_activity<'a>(snapshot: &'a SessionSnapshot, approval: &Approval) -> &'a Activity {
    find_approval(snapshot, approval)
        .unwrap_or_else(|| panic!("the Transcript holds the Approval: {snapshot:?}"))
}

/// The Questionnaire Activity `snapshot` holds for `questionnaire`, if any.
fn find_questionnaire<'a>(
    snapshot: &'a SessionSnapshot,
    questionnaire: &Questionnaire,
) -> Option<&'a Activity> {
    snapshot.activities.iter().find(|activity| match activity {
        Activity::Questionnaire {
            questionnaire: held,
            ..
        } => held.id == questionnaire.id,
        _ => false,
    })
}

fn questionnaire_activity<'a>(
    snapshot: &'a SessionSnapshot,
    questionnaire: &Questionnaire,
) -> &'a Activity {
    find_questionnaire(snapshot, questionnaire)
        .unwrap_or_else(|| panic!("the Transcript holds the Questionnaire: {snapshot:?}"))
}

#[tokio::test]
async fn a_brokered_subagents_approval_crosses_the_client_and_its_decision_reaches_the_subagents_own_provider()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-approval", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    let (tree, mut updates) = open_tree(&descriptor, delegating.caller).await;
    let mut revision = tree.revision;
    assert_eq!(
        intervention_flags(&tree),
        [(delegating.caller, false), (child_id, false)]
    );

    let request = approval();
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: request.clone(),
            tool_activity_id: None,
        })
        .await;

    let child = read_until(
        &descriptor,
        child_id,
        "the Approval is listed on the Subagent's own Session",
        |snapshot| snapshot.pending_approvals == [request.id],
    )
    .await;
    let Activity::Approval {
        turn_id, outcome, ..
    } = approval_activity(&child, &request)
    else {
        unreachable!()
    };
    assert_eq!(
        (*turn_id, *outcome),
        (child.turns[0].id, ApprovalOutcome::Pending),
        "in the Turn its Delegation began, awaiting a Decision"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert!(
        caller.pending_approvals.is_empty()
            && !caller
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Approval { .. })),
        "the parent's own Transcript holds none of it"
    );
    assert_eq!(
        caller.pending_approvals_in_subagent(child_id),
        1,
        "the parent reads it through the Subagent it spawned"
    );

    assert_eq!(
        next_intervention_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: child_id,
            needs_intervention: true,
        },
        "the Approval marks the Subagent's entry in the tree"
    );
    let (flagged, _flagged_updates) = open_tree(&descriptor, child_id).await;
    assert_eq!(
        intervention_flags(&flagged),
        [(delegating.caller, false), (child_id, true)],
        "and only that entry"
    );
    let raised = standing(&descriptor, delegating.caller).await;
    assert_eq!(
        raised.pending_approval_count(),
        1,
        "the top-level Session's Standing rolls it up"
    );
    assert_eq!(
        raised
            .subagent_interventions
            .iter()
            .map(|entry| (
                entry.session_id,
                entry.via_session_id,
                entry.pending_approvals.clone()
            ))
            .collect::<Vec<_>>(),
        [(child_id, child_id, vec![request.id])],
        "naming the Subagent whose Session owns it"
    );

    let (response, delivered) = tokio::join!(
        submit_decision(&descriptor, child_id, request.id, Decision::Accept),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_decision())
                .await
                .expect("the Decision reaches the Subagent's own Provider")
        },
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(delivered, (request.id, Decision::Accept));
    assert!(
        delegating.caller_provider.try_next_decision().is_none(),
        "the parent's Provider is handed no Decision on an Approval it never raised"
    );

    let decided = read_until(
        &descriptor,
        child_id,
        "the Approval settles as decided",
        |snapshot| {
            matches!(
                approval_activity(snapshot, &request),
                Activity::Approval {
                    outcome: ApprovalOutcome::Decided,
                    ..
                }
            )
        },
    )
    .await;
    let Activity::Approval { decision, .. } = approval_activity(&decided, &request) else {
        unreachable!()
    };
    assert_eq!(*decision, Some(Decision::Accept));
    assert!(decided.pending_approvals.is_empty());
    assert_eq!(
        decided.turns[0].status,
        TurnStatus::Active,
        "an accepted Approval leaves the Subagent working"
    );
    assert_eq!(
        next_intervention_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: child_id,
            needs_intervention: false,
        },
        "the entry's mark clears once the Decision is made"
    );
    assert_eq!(
        standing(&descriptor, delegating.caller)
            .await
            .pending_approval_count(),
        0,
        "and so does the Standing's"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagents_questionnaire_crosses_the_client_and_its_answer_reaches_the_subagents_own_provider()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-questionnaire", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    let (tree, mut updates) = open_tree(&descriptor, delegating.caller).await;
    let mut revision = tree.revision;

    let asked = questionnaire();
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: asked.clone(),
        })
        .await;

    let child = read_until(
        &descriptor,
        child_id,
        "the Questionnaire is listed on the Subagent's own Session",
        |snapshot| find_questionnaire(snapshot, &asked).is_some(),
    )
    .await;
    let Activity::Questionnaire {
        turn_id, outcome, ..
    } = questionnaire_activity(&child, &asked)
    else {
        unreachable!()
    };
    assert_eq!(
        (*turn_id, *outcome),
        (child.turns[0].id, QuestionnaireOutcome::Pending),
        "in the Turn its Delegation began, awaiting an Answer"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert!(
        !caller
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Questionnaire { .. })),
        "the parent's own Transcript holds none of it"
    );
    assert_eq!(caller.pending_questionnaires_in_subagent(child_id), 1);

    assert_eq!(
        next_intervention_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: child_id,
            needs_intervention: true,
        },
        "the Questionnaire marks the Subagent's entry in the tree"
    );
    let raised = standing(&descriptor, delegating.caller).await;
    assert_eq!(
        raised.pending_questionnaire_count(),
        1,
        "the top-level Session's Standing rolls it up"
    );
    assert!(raised.pending_questionnaires.is_empty());
    assert_eq!(
        raised
            .subagent_interventions
            .iter()
            .map(|entry| (entry.session_id, entry.pending_questionnaires.clone()))
            .collect::<Vec<_>>(),
        [(child_id, vec![asked.id])]
    );

    let answer = QuestionnaireSubmission::Answer {
        answer: Answer {
            questions: vec![QuestionAnswer::Freeform {
                text: "Only the Provider seams".into(),
            }],
        },
    };
    let (response, delivered) = tokio::join!(
        submit_questionnaire(&descriptor, child_id, asked.id, &answer),
        async {
            timeout(
                PROGRESS_DEADLINE,
                child_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Subagent's own Provider")
        },
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(delivered, (asked.id, answer));
    assert!(
        delegating
            .caller_provider
            .try_next_questionnaire_delivery()
            .is_none(),
        "the parent's Provider is handed no Answer to a Questionnaire it never raised"
    );

    read_until(
        &descriptor,
        child_id,
        "the Questionnaire settles as answered",
        |snapshot| {
            matches!(
                questionnaire_activity(snapshot, &asked),
                Activity::Questionnaire {
                    outcome: QuestionnaireOutcome::Answered,
                    answer: Some(_),
                    ..
                }
            )
        },
    )
    .await;
    assert_eq!(
        next_intervention_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: child_id,
            needs_intervention: false,
        },
        "the entry's mark clears once it is answered"
    );
    assert_eq!(
        standing(&descriptor, delegating.caller)
            .await
            .pending_questionnaire_count(),
        0,
        "and so does the Standing's"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn declining_and_interrupting_a_brokered_subagents_approval_ends_its_turn_through_its_own_provider_alone()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-decline-interrupt", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = spawn_working_child(&mut delegating).await;
    let request = approval();
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: request.clone(),
            tool_activity_id: None,
        })
        .await;
    read_until(
        &descriptor,
        child_id,
        "the Approval awaits a Decision",
        |snapshot| snapshot.pending_approvals == [request.id],
    )
    .await;

    let (response, delivered) = tokio::join!(
        submit_decision(
            &descriptor,
            child_id,
            request.id,
            Decision::DeclineAndInterrupt
        ),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_decision())
                .await
                .expect("the Decision reaches the Subagent's own Provider")
        },
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(delivered, (request.id, Decision::DeclineAndInterrupt));
    assert!(
        delegating.caller_provider.try_next_decision().is_none()
            && delegating.caller_provider.try_next_interrupt().is_none()
            && delegating
                .caller_provider
                .try_next_subagent_stop()
                .is_none(),
        "the parent's Provider is asked nothing of a Turn it does not run"
    );

    // The Subagent's Provider refuses the request and ends its own Turn, as
    // the Decision asks, at its own boundary.
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the Subagent's row settles with its Turn",
        |snapshot| row_status(snapshot, child_id).0 != ActivityStatus::Active,
    )
    .await;
    assert_eq!(
        row_status(&settled, child_id).0,
        ActivityStatus::Interrupted
    );
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Active,
        "the parent's Turn works on"
    );
    let child = read_session(&descriptor, child_id).await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);
    assert!(matches!(
        approval_activity(&child, &request),
        Activity::Approval {
            outcome: ApprovalOutcome::Decided,
            decision: Some(Decision::DeclineAndInterrupt),
            ..
        }
    ));

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
