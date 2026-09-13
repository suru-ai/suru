use crate::support::{
    connected_application, enter_active_session, rendered_application_buffer,
    rendered_application_rows_at, text_position, type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde_json::json;
use std::path::PathBuf;
use suru::{
    protocol::{
        Activity, ActivityId, Approval, ApprovalId, ApprovalOutcome, ApprovalSubject,
        CommandAction, Decision, TranscriptItem,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

fn approval_activity(
    turn_id: suru::protocol::TurnId,
    subject: ApprovalSubject,
    reason: Option<&str>,
    outcome: ApprovalOutcome,
    decision: Option<Decision>,
) -> Activity {
    Activity::Approval {
        id: ActivityId::new(),
        turn_id,
        approval: Approval {
            id: ApprovalId::new(),
            subject,
            reason: reason.map(str::to_owned),
        },
        tool_activity_id: None,
        detail_truncated: false,
        outcome,
        decision,
    }
}

fn add_approval(snapshot: &mut suru::protocol::SessionSnapshot, activity: Activity) {
    let activity_id = activity.id();
    snapshot.activities.push(activity);
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id });
}

fn pending_application() -> (Application, ApprovalId, suru::protocol::SessionSnapshot) {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    type_terminal_text(&mut app, "kept draft");
    let activity = approval_activity(
        turn_id,
        ApprovalSubject::Network {
            host_or_url: "https://api.example.test/v1".into(),
        },
        Some("Download build metadata"),
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval { approval, .. } = &activity else {
        unreachable!()
    };
    let approval_id = approval.id;
    snapshot.pending_approvals = vec![approval_id];
    snapshot.pending_approvals_revision = snapshot.revision;
    add_approval(&mut snapshot, activity);
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(snapshot.clone()),
    ))
    .unwrap();
    (app, approval_id, snapshot)
}

fn invoke(app: &mut Application, command: SemanticCommandId) -> ApplicationTransition {
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        command,
    )))
    .unwrap()
}

fn key(app: &mut Application, code: KeyCode) -> ApplicationTransition {
    app.handle_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap()
}

#[test]
fn every_typed_subject_renders_all_available_detail_reason_and_tool_link() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let command_id = ActivityId::new();
    snapshot.activities.push(Activity::Command {
        id: command_id,
        turn_id,
        status: suru::protocol::ActivityStatus::Active,
        command: "cargo check".into(),
        cwd: Some(PathBuf::from("project")),
        output: String::new(),
        output_truncated: false,
        exit_status: None,
    });
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: command_id,
    });

    let mut command = approval_activity(
        turn_id,
        ApprovalSubject::Command {
            command: "cargo check --tests".into(),
            cwd: Some(PathBuf::from("project/client")),
            actions: vec![CommandAction::Search {
                command: "rg Approval src".into(),
                query: Some("Approval".into()),
                path: Some(PathBuf::from("src")),
            }],
        },
        Some("Validate the implementation"),
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval {
        tool_activity_id, ..
    } = &mut command
    else {
        unreachable!()
    };
    *tool_activity_id = Some(command_id);
    add_approval(&mut snapshot, command);
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::FileChange {
                paths: vec![
                    PathBuf::from("src/approval.rs"),
                    PathBuf::from("src/tui.rs"),
                ],
                grant_root: Some(PathBuf::from("src")),
            },
            Some("Apply the requested patch"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::Read {
                path: PathBuf::from("secrets/example.txt"),
            },
            Some("Inspect the requested file"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::Network {
                host_or_url: "https://api.example.test/v1".into(),
            },
            Some("Download build metadata"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::PermissionGrant {
                profile: json!({"sandbox": "workspace-write", "network": true}),
            },
            Some("Broaden this Turn's sandbox"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::OtherTool {
                name: "fetch_release".into(),
                input: json!({"repository": "suru", "tag": "next"}),
            },
            Some("Fetch the release manifest"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(snapshot),
    ))
    .unwrap();
    invoke(&mut app, SemanticCommandId::TranscriptFoldsToggle);
    let screen = rendered_application_rows_at(&app, 120, 80).join("\n");
    for expected in [
        "Command: cargo check --tests",
        "Directory: project/client",
        "Search: rg Approval src",
        "Query: Approval",
        "Tool row: Command",
        "Reason: Validate the implementation",
        "File Change: src/approval.rs",
        "Path: src/tui.rs",
        "Grant root: src",
        "Read: secrets/example.txt",
        "Network: https://api.example.test/v1",
        "Permission Grant",
        "workspace-write",
        "Other Tool: fetch_release",
        "repository",
        "Fetch the release manifest",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }
}

#[test]
fn decision_number_and_focus_keys_only_submit_from_an_explicitly_open_approval() {
    let decisions = [
        (KeyCode::Char('1'), Decision::Accept),
        (KeyCode::Char('2'), Decision::AcceptForSession),
        (KeyCode::Char('3'), Decision::Decline),
        (KeyCode::Char('4'), Decision::DeclineAndInterrupt),
    ];
    for (code, expected) in decisions {
        let (mut app, id, _) = pending_application();
        let arrived = rendered_application_rows_at(&app, 100, 30).join("\n");
        assert!(arrived.contains("kept draft"), "{arrived}");
        type_terminal_text(&mut app, "draft ");
        assert!(matches!(
            key(&mut app, code),
            ApplicationTransition::Continue
        ));
        let typed = rendered_application_rows_at(&app, 100, 30).join("\n");
        assert!(typed.contains(&format!(
            "draft {}",
            match code {
                KeyCode::Char(value) => value,
                _ => unreachable!(),
            }
        )));
        let open = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(
            app.command_for_terminal_input(open.clone()),
            Some(CommandId::InvokeSemantic(SemanticCommandId::ApprovalOpen))
        );
        app.handle_terminal_event(open).unwrap();
        assert_eq!(
            app.command_for_terminal_input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))),
            Some(CommandId::InvokeSemantic(match expected {
                Decision::Accept => SemanticCommandId::ApprovalAccept,
                Decision::AcceptForSession => SemanticCommandId::ApprovalAcceptForSession,
                Decision::Decline => SemanticCommandId::ApprovalDecline,
                Decision::DeclineAndInterrupt => SemanticCommandId::ApprovalDeclineAndInterrupt,
            }))
        );
        assert!(matches!(
            key(&mut app, code),
            ApplicationTransition::SubmitDecision { id: submitted, decision, .. }
                if submitted == id && decision == expected
        ));
    }

    let (mut app, id, _) = pending_application();
    invoke(&mut app, SemanticCommandId::ApprovalOpen);
    assert_eq!(
        app.command_for_terminal_input(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))),
        Some(CommandId::InvokeSemantic(
            SemanticCommandId::ApprovalChoiceNext,
        ))
    );
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    assert_eq!(
        app.command_for_terminal_input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))),
        Some(CommandId::InvokeSemantic(SemanticCommandId::ApprovalChoose,))
    );
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::SubmitDecision { id: submitted, decision: Decision::Decline, .. }
            if submitted == id
    ));
}

#[test]
fn decision_retry_waits_for_authoritative_reconciliation_and_uncertainty_stays_terminal() {
    let (mut app, id, mut stale_pending) = pending_application();
    let session = suru::protocol::SessionReference::new(
        suru::protocol::Outlook::Local,
        stale_pending.session.id,
    );
    invoke(&mut app, SemanticCommandId::ApprovalOpen);
    assert!(matches!(
        key(&mut app, KeyCode::Char('1')),
        ApplicationTransition::SubmitDecision { .. }
    ));

    stale_pending.revision.0 += 1;
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(stale_pending.clone()),
    ))
    .unwrap();
    assert!(matches!(
        key(&mut app, KeyCode::Char('2')),
        ApplicationTransition::Continue
    ));

    stale_pending.revision.0 += 1;
    app.handle_event(ApplicationEvent::ApprovalSubmissionReconciled {
        id,
        session: session.clone(),
        snapshot: Some(stale_pending.clone()),
        error: Some("request failed before arbitration".into()),
    })
    .unwrap();
    assert!(matches!(
        key(&mut app, KeyCode::Char('2')),
        ApplicationTransition::SubmitDecision {
            id: submitted,
            decision: Decision::AcceptForSession,
            ..
        } if submitted == id
    ));

    let Activity::Approval { outcome, .. } = stale_pending
        .activities
        .iter_mut()
        .find(
            |activity| matches!(activity, Activity::Approval { approval, .. } if approval.id == id),
        )
        .unwrap()
    else {
        unreachable!()
    };
    *outcome = ApprovalOutcome::SubmissionRejected;
    stale_pending.revision.0 += 1;
    app.handle_event(ApplicationEvent::ApprovalSubmissionReconciled {
        id,
        session: session.clone(),
        snapshot: Some(stale_pending.clone()),
        error: Some("Provider refused delivery".into()),
    })
    .unwrap();
    assert!(matches!(
        key(&mut app, KeyCode::Char('3')),
        ApplicationTransition::SubmitDecision {
            id: submitted,
            decision: Decision::Decline,
            ..
        } if submitted == id
    ));

    let Activity::Approval { outcome, .. } = stale_pending
        .activities
        .iter_mut()
        .find(
            |activity| matches!(activity, Activity::Approval { approval, .. } if approval.id == id),
        )
        .unwrap()
    else {
        unreachable!()
    };
    *outcome = ApprovalOutcome::DeliveryUncertain;
    stale_pending.pending_approvals.clear();
    stale_pending.submitting_approvals.clear();
    stale_pending.revision.0 += 1;
    app.handle_event(ApplicationEvent::ApprovalSubmissionReconciled {
        id,
        session,
        snapshot: Some(stale_pending),
        error: Some("delivery uncertain".into()),
    })
    .unwrap();
    assert!(matches!(
        invoke(&mut app, SemanticCommandId::ApprovalOpen),
        ApplicationTransition::Continue
    ));
    assert!(matches!(
        invoke(&mut app, SemanticCommandId::ApprovalDecline),
        ApplicationTransition::Continue
    ));
}

#[test]
fn approval_history_distinguishes_every_lifecycle_outcome_and_records_full_decisions() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let cases = [
        (ApprovalOutcome::Pending, None),
        (ApprovalOutcome::Submitting, None),
        (ApprovalOutcome::SubmissionRejected, None),
        (ApprovalOutcome::DeliveryUncertain, None),
        (ApprovalOutcome::Decided, Some(Decision::Accept)),
        (ApprovalOutcome::Decided, Some(Decision::AcceptForSession)),
        (ApprovalOutcome::Decided, Some(Decision::Decline)),
        (
            ApprovalOutcome::Decided,
            Some(Decision::DeclineAndInterrupt),
        ),
        (ApprovalOutcome::Withdrawn, None),
        (ApprovalOutcome::Unavailable, None),
    ];
    for (index, (outcome, decision)) in cases.into_iter().enumerate() {
        add_approval(
            &mut snapshot,
            approval_activity(
                turn_id,
                ApprovalSubject::Network {
                    host_or_url: format!("status-{index}.example.test"),
                },
                None,
                outcome,
                decision,
            ),
        );
    }
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(snapshot),
    ))
    .unwrap();
    let screen = rendered_application_rows_at(&app, 120, 70).join("\n");
    for expected in [
        "Pending",
        "Submitting",
        "Decision not delivered; retry",
        "Delivery uncertain",
        "will not be resent",
        "Accepted once",
        "Accepted for Session",
        "Declined",
        "Declined and interrupted",
        "Withdrawn; Provider no longer",
        "requests this action",
        "Unavailable; previous Provider",
        "request is no longer live",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }
}

#[test]
fn long_approval_detail_has_a_reversible_fold_and_a_distinct_stored_truncation_marker() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let long_tail = "final available input";
    let foldable = approval_activity(
        turn_id,
        ApprovalSubject::Command {
            command: format!("printf '{}{}'", "detail line\n".repeat(30), long_tail),
            cwd: None,
            actions: Vec::new(),
        },
        Some("Review every argument\nbefore choosing"),
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval {
        approval: foldable_approval,
        ..
    } = &foldable
    else {
        unreachable!()
    };
    let foldable_id = foldable_approval.id;
    snapshot.pending_approvals = vec![foldable_id];
    snapshot.pending_approvals_revision = snapshot.revision;
    add_approval(&mut snapshot, foldable);
    let mut truncated = approval_activity(
        turn_id,
        ApprovalSubject::OtherTool {
            name: "capped_tool".into(),
            input: json!({"payload": "stored prefix"}),
        },
        None,
        ApprovalOutcome::Unavailable,
        None,
    );
    let Activity::Approval {
        detail_truncated, ..
    } = &mut truncated
    else {
        unreachable!()
    };
    *detail_truncated = true;
    add_approval(&mut snapshot, truncated);
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(snapshot),
    ))
    .unwrap();

    let folded = rendered_application_rows_at(&app, 100, 60).join("\n");
    assert!(
        folded.contains("+"),
        "Fold marker must count hidden detail:\n{folded}"
    );
    assert!(
        folded.contains("+33"),
        "Fold counts every logical Command and reason Line:\n{folded}"
    );
    assert!(
        !folded.contains(long_tail),
        "Fold hides available stored detail"
    );
    invoke(&mut app, SemanticCommandId::TranscriptFoldsToggle);
    let expanded = rendered_application_rows_at(&app, 100, 60).join("\n");
    assert!(
        expanded.contains(long_tail),
        "expanded Fold reveals stored detail:\n{expanded}"
    );
    assert!(
        expanded.contains("[Approval detail truncated]"),
        "typed Truncation is distinct from the Fold marker:\n{expanded}"
    );

    invoke(&mut app, SemanticCommandId::ApprovalOpen);
    let panel = rendered_application_rows_at(&app, 80, 24).join("\n");
    for choice in [
        "1. Accept once",
        "2. Accept for Session",
        "3. Decline",
        "4. Decline and Interrupt",
        "1–4 decide",
    ] {
        assert!(
            panel.contains(choice),
            "long detail hid {choice:?}:\n{panel}"
        );
    }
}

#[test]
fn folded_approval_summary_is_bounded_and_its_fold_affordance_is_not_copied() {
    let workspace = workspace_dir();
    let mut app = connected_application(workspace.path());
    let (_, mut snapshot, turn_id) = enter_active_session(&mut app, workspace.path());
    let hidden_tail = "HIDDEN-COMMAND-TAIL";
    let command = format!("echo {}{hidden_tail}\nsecond line", "x".repeat(2_000));
    add_approval(
        &mut snapshot,
        approval_activity(
            turn_id,
            ApprovalSubject::Command {
                command,
                cwd: None,
                actions: Vec::new(),
            },
            Some("Explain this request"),
            ApprovalOutcome::Pending,
            None,
        ),
    );
    app.handle_event(ApplicationEvent::Session(
        suru::managed_client::SessionEvent::snapshot(snapshot),
    ))
    .unwrap();

    let buffer = rendered_application_buffer(&app, 100, 24);
    let start = text_position(&buffer, "Approval · Command");
    let marker = text_position(&buffer, "+3 lines");
    assert!(
        !crate::support::buffer_rows(&buffer)
            .join("\n")
            .contains(hidden_tail),
        "folded summary exposed the long Command tail"
    );
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: start.0,
        row: start.1,
        modifiers: KeyModifiers::NONE,
    }))
    .unwrap();
    app.handle_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: marker.0 + 7,
        row: marker.1,
        modifiers: KeyModifiers::NONE,
    }))
    .unwrap();
    let copied = app
        .handle_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: marker.0 + 7,
            row: marker.1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
    let ApplicationTransition::CopyToClipboard(copied) = copied else {
        panic!("folded Approval selection did not copy: {copied:?}")
    };
    assert!(
        copied.text.contains("Approval · Command"),
        "{}",
        copied.text
    );
    assert!(!copied.text.contains("+3 lines"), "{}", copied.text);
}
