//! Copilot permission callbacks through the public Session seam.

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{
        LiveTurn, ScriptedCopilot, abort_arm, conversation_arms, hosting, permission_decision_arm,
        send_arm, session_where, settled_session,
    },
};
use serde_json::Value;
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, ApprovalOutcome, ApprovalPosture, ApprovalSubject,
        CopilotPermissions, CreateSessionRequest, Decision, InitialPrompt, PromptDelivery,
        PromptId, TurnStatus, UpdateApprovalPostureRequest,
    },
    provider::CopilotRuntime,
};
use tokio::time::timeout;

fn permission_response_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.permissions.handlePendingPermissionRequest"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"success":true}}}}'
{timeline}      ;;
"#,
    )
}

fn fixture(requests: &str, response_timeline: &str) -> ScriptedCopilot {
    let arms = conversation_arms().replace(
        &permission_decision_arm(),
        &permission_response_arm(response_timeline),
    );
    ScriptedCopilot::new(&format!("{}{}", arms, send_arm(requests)))
}

fn approval(
    snapshot: &suru::protocol::SessionSnapshot,
    request: &str,
) -> (suru::protocol::ApprovalId, ApprovalSubject) {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: ApprovalOutcome::Pending,
                ..
            } if approval.reason.as_deref() == Some(request) => {
                Some((approval.id, approval.subject.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("pending Approval for {request}"))
}

#[tokio::test]
async fn an_active_permission_handler_uses_the_changed_posture_on_its_next_request() {
    let timeline = r#"      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      event live permission.requested '{"requestId":"live-update","permissionRequest":{"kind":"read","path":"next.txt","intention":"Use changed posture"}}'
"#;
    let copilot = fixture(timeline, "      event idle session.idle '{}'\n");
    let live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-live-posture",
        "Wait",
    )
    .await;
    live.client
        .update_approval_posture(
            live.session_id,
            UpdateApprovalPostureRequest {
                posture: Some(ApprovalPosture::Copilot {
                    permissions: CopilotPermissions::AllowAll,
                }),
            },
        )
        .await
        .unwrap();
    copilot.release();
    let response = native_response(&copilot, "live-update").await;
    assert_eq!(response["result"]["kind"], "approve-once");
    live.shutdown().await;
}

#[tokio::test]
async fn the_next_turn_adopts_the_sessions_current_permission_posture() {
    let timeline = r#"      sends=$(( ${sends:-0} + 1 ))
      if [ "$sends" -eq 1 ]; then
        event first_idle session.idle '{}'
      else
        event next permission.requested '{"requestId":"next-turn","permissionRequest":{"kind":"read","path":"next.txt","intention":"Use the new posture"}}'
      fi
"#;
    let arms = conversation_arms().replace(
        &permission_decision_arm(),
        &permission_response_arm("      event second_idle session.idle '{}'\n"),
    );
    let copilot = ScriptedCopilot::new(&format!("{}{}", arms, send_arm(timeline)));
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (server, client) = hosting(&copilot, "copilot-next-turn-posture", state.path()).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "First".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap();
    settled_session(&client, created.session.id, 0).await;

    client
        .update_approval_posture(
            created.session.id,
            UpdateApprovalPostureRequest {
                posture: Some(ApprovalPosture::Copilot {
                    permissions: CopilotPermissions::AllowAll,
                }),
            },
        )
        .await
        .unwrap();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Second".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .unwrap();
    settled_session(&client, created.session.id, 1).await;

    let response = native_response(&copilot, "next-turn").await;
    assert_eq!(response["result"]["kind"], "approve-once");
    server.shutdown().await.unwrap();
}

async fn native_response(fixture: &ScriptedCopilot, request_id: &str) -> Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(request) = fixture.requests().into_iter().find(|request| {
                request["method"] == "session.permissions.handlePendingPermissionRequest"
                    && request["params"]["requestId"] == request_id
            }) {
                return request["params"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native permission response arrives")
}

#[tokio::test]
async fn ask_projects_a_typed_command_and_native_completion_waits_for_decision_history() {
    let copilot = fixture(
        r#"      event tool tool.execution_start '{"toolCallId":"t1","toolName":"bash","arguments":{"command":"cargo check"}}'
      event p permission.requested '{"requestId":"p1","permissionRequest":{"kind":"shell","toolCallId":"t1","fullCommandText":"cargo check","intention":"Check the project","commands":[{"identifier":"cargo","readOnly":true}]}}'
"#,
        r#"      event done permission.completed '{"requestId":"p1","result":{"kind":"approved"},"toolCallId":"t1"}'
      event idle session.idle '{}'
"#,
    );
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-approval-command",
        "Check",
    )
    .await;
    let pending = live
        .wait_for("Copilot Approval is pending", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let (id, subject) = approval(&pending, "Check the project");
    let ApprovalSubject::Command {
        command,
        cwd,
        actions,
    } = subject
    else {
        panic!("shell request was not a Command")
    };
    assert_eq!(command, "cargo check");
    assert!(cwd.is_some());
    assert_eq!(actions.len(), 1);
    assert!(
        pending.activities.iter().any(|activity| matches!(activity,
            Activity::Approval { approval, tool_activity_id: Some(_), .. } if approval.id == id
        )),
        "native toolCallId links the Approval to its Tool Activity"
    );

    live.client
        .submit_decision(live.session_id, id, Decision::Accept)
        .await
        .unwrap();
    let settled = live
        .wait_for("native completion follows durable Decision", |snapshot| {
            snapshot.turns[0].status == TurnStatus::Completed
        })
        .await;
    assert!(settled.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::Accept), .. }
            if approval.id == id)));
    let response = native_response(&copilot, "p1").await;
    assert_eq!(response["result"]["kind"], "approve-once");
    live.shutdown().await;
}

#[tokio::test]
async fn native_permission_families_keep_their_typed_detail_and_all_decisions() {
    let requests = r#"      event p1 permission.requested '{"requestId":"write","permissionRequest":{"kind":"write","toolCallId":"tw","fileName":"src/main.rs","diff":"secret diff","intention":"Edit file"}}'
      event p2 permission.requested '{"requestId":"read","permissionRequest":{"kind":"read","toolCallId":"tr","path":"Cargo.toml","intention":"Read manifest"}}'
      event p3 permission.requested '{"requestId":"url","permissionRequest":{"kind":"url","toolCallId":"tu","url":"https://user:pass@[2001:db8::1]:8443/path","intention":"Fetch docs"}}'
      event p4 permission.requested '{"requestId":"mcp","permissionRequest":{"kind":"mcp","toolCallId":"tm","serverName":"docs","toolName":"search","args":{"query":"rust"},"intention":"Search docs"}}'
      event p5 permission.requested '{"requestId":"custom","permissionRequest":{"kind":"custom-tool","toolCallId":"tc","toolName":"deploy","input":{"region":"test"},"intention":"Run custom"}}'
      event p6 permission.requested '{"requestId":"future","permissionRequest":{"kind":"future-kind","toolCallId":"tf","payload":{"whole":true},"intention":"Use future"}}'
"#;
    let copilot = fixture(requests, "");
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-approval-families",
        "Use tools",
    )
    .await;
    let pending = live
        .wait_for("all native permission kinds arrive", |snapshot| {
            snapshot.pending_approvals.len() == 6
        })
        .await;
    assert!(
        matches!(approval(&pending, "Edit file").1, ApprovalSubject::FileChange { paths, .. } if paths == [std::path::PathBuf::from("src/main.rs")])
    );
    assert!(
        matches!(approval(&pending, "Read manifest").1, ApprovalSubject::Read { path } if path == *"Cargo.toml")
    );
    assert!(
        matches!(approval(&pending, "Fetch docs").1, ApprovalSubject::Network { host_or_url } if host_or_url.contains("2001:db8"))
    );
    assert!(
        matches!(approval(&pending, "Search docs").1, ApprovalSubject::OtherTool { name, input } if name == "MCP docs/search" && input["query"] == "rust")
    );
    assert!(
        matches!(approval(&pending, "Run custom").1, ApprovalSubject::OtherTool { name, input } if name == "deploy" && input["region"] == "test")
    );
    assert!(
        matches!(approval(&pending, "Use future").1, ApprovalSubject::OtherTool { name, input } if name == "future-kind" && input["payload"]["whole"] == true)
    );

    for (request, decision) in [
        ("Edit file", Decision::AcceptForSession),
        ("Read manifest", Decision::Decline),
        ("Fetch docs", Decision::AcceptForSession),
        ("Search docs", Decision::Accept),
        ("Run custom", Decision::AcceptForSession),
        ("Use future", Decision::Decline),
    ] {
        let id = approval(&pending, request).0;
        live.client
            .submit_decision(live.session_id, id, decision)
            .await
            .unwrap();
    }
    assert_eq!(
        native_response(&copilot, "write").await["result"],
        serde_json::json!({"kind":"approve-for-session","approval":{"kind":"write"}})
    );
    assert_eq!(
        native_response(&copilot, "read").await["result"]["kind"],
        "reject"
    );
    assert_eq!(
        native_response(&copilot, "url").await["result"]["domain"],
        "[2001:db8::1]"
    );
    assert_eq!(
        native_response(&copilot, "mcp").await["result"]["kind"],
        "approve-once"
    );
    assert_eq!(
        native_response(&copilot, "custom").await["result"]["approval"],
        serde_json::json!({"kind":"custom-tool","toolName":"deploy"})
    );
    live.shutdown().await;
}

#[tokio::test]
async fn a_child_approval_outlives_parent_idle_and_child_completion_cannot_erase_its_decision() {
    let copilot = fixture(
        r#"      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      agent_event s agent-1 subagent.started '{"toolCallId":"spawn","agentName":"worker","agentDisplayName":"Worker","agentDescription":"Do child work"}'
      agent_event p agent-1 permission.requested '{"requestId":"child","permissionRequest":{"kind":"read","toolCallId":"child-tool","path":"child.txt","intention":"Child read"}}'
      event idle session.idle '{}'
"#,
        r#"      agent_event done agent-1 permission.completed '{"requestId":"child","result":{"kind":"approved"},"toolCallId":"child-tool"}'
      agent_event settled agent-1 subagent.completed '{"toolCallId":"spawn","agentName":"worker","agentDisplayName":"Worker","durationMs":1}'
"#,
    );
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-child-approval",
        "Delegate",
    )
    .await;
    copilot.release();
    let parent = live
        .wait_for("parent settles while child remains", |snapshot| {
            snapshot.turns[0].status == TurnStatus::Completed
                && snapshot
                    .activities
                    .iter()
                    .any(|activity| matches!(activity, Activity::Subagent { .. }))
        })
        .await;
    let child_id = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("child Session exists");
    let mut child_feed = live.client.subscribe_session(child_id).await.unwrap();
    let child = session_where(
        &live.client,
        &mut child_feed,
        child_id,
        "child Approval remains pending after parent idle",
        |snapshot| snapshot.pending_approvals.len() == 1,
    )
    .await;
    let id = child.pending_approvals[0];
    live.client
        .submit_decision(child_id, id, Decision::Accept)
        .await
        .unwrap();
    let child = session_where(
        &live.client,
        &mut child_feed,
        child_id,
        "child completion follows durable Decision",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert!(child.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::Accept), .. }
            if approval.id == id)));
    live.shutdown().await;
}

#[tokio::test]
async fn allow_all_auto_approves_only_when_managed_policy_does_not_require_another_path() {
    let requests = r#"      event auto permission.requested '{"requestId":"auto","permissionRequest":{"kind":"read","path":"auto.txt","intention":"Automatic"}}'
      event managed permission.requested '{"requestId":"managed","permissionRequest":{"kind":"read","path":"managed.txt","intention":"Managed user","managedSettingsEnabled":true}}'
      event both permission.requested '{"requestId":"both","permissionRequest":{"kind":"read","path":"both.txt","intention":"Managed settings win","managedSettingsEnabled":true,"managedApprovalRequired":true}}'
      event external permission.requested '{"requestId":"external","permissionRequest":{"kind":"read","path":"external.txt","intention":"Managed authority","managedApprovalRequired":true}}'
"#;
    let copilot = fixture(requests, "");
    let mut live = LiveTurn::start_configured(
        CopilotRuntime::new(copilot.executable()),
        "copilot-allow-all-managed",
        "Use managed tools",
        r#"{"provider":{"copilot":{"permissions":"allowAll"}}}"#,
    )
    .await;
    let pending = live
        .wait_for("managed settings requests reach the user", |snapshot| {
            snapshot.pending_approvals.len() == 2
        })
        .await;
    let auto = native_response(&copilot, "auto").await;
    assert_eq!(auto["result"]["kind"], "approve-once");
    assert!(
        copilot.requests().iter().all(|request| {
            request["method"] != "session.permissions.handlePendingPermissionRequest"
                || request["params"]["requestId"] != "external"
        }),
        "managedApprovalRequired is left for the managed authority"
    );
    for reason in ["Managed user", "Managed settings win"] {
        let id = approval(&pending, reason).0;
        live.client
            .submit_decision(live.session_id, id, Decision::Decline)
            .await
            .unwrap();
    }
    live.shutdown().await;
}

#[tokio::test]
async fn decline_and_interrupt_delivers_rejection_before_suru_interrupts_the_turn() {
    let arms =
        conversation_arms().replace(&permission_decision_arm(), &permission_response_arm(""));
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        arms,
        send_arm(
            r#"      event p permission.requested '{"requestId":"stop","permissionRequest":{"kind":"shell","toolCallId":"t-stop","fullCommandText":"danger","intention":"Stop this"}}'
"#
        ),
        abort_arm(
            r#"      event idle session.idle '{"aborted":true}'
"#
        ),
    ));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-decline-interrupt",
        "Try then stop",
    )
    .await;
    let pending = live
        .wait_for("Approval is pending", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let id = pending.pending_approvals[0];
    live.client
        .submit_decision(live.session_id, id, Decision::DeclineAndInterrupt)
        .await
        .unwrap();
    let interrupted = live
        .wait_for("Turn is interrupted", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    assert!(interrupted.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::DeclineAndInterrupt), .. }
            if approval.id == id)));
    let methods = copilot
        .requests()
        .into_iter()
        .filter_map(|request| request["method"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let rejected = methods
        .iter()
        .position(|method| method == "session.permissions.handlePendingPermissionRequest")
        .unwrap();
    let aborted = methods
        .iter()
        .position(|method| method == "session.abort")
        .unwrap();
    assert!(
        rejected < aborted,
        "native rejection precedes Suru's interrupt"
    );
    assert_eq!(
        native_response(&copilot, "stop").await["result"]["kind"],
        "reject"
    );
    live.shutdown().await;
}

#[tokio::test]
async fn bounded_history_does_not_change_the_native_session_approval() {
    let identifier = "native-secret-command".repeat(5_000);
    let request = format!(
        "      event p permission.requested '{}'\n",
        serde_json::json!({
            "requestId":"large",
            "permissionRequest":{
                "kind":"shell",
                "toolCallId":"large-tool",
                "fullCommandText":identifier,
                "intention":"Large command",
                "commands":[{"identifier":identifier,"readOnly":false}]
            }
        })
    );
    let copilot = fixture(&request, "");
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-bounded-native-approval",
        "Run large command",
    )
    .await;
    let pending = live
        .wait_for("bounded Approval arrives", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let (id, truncated) = pending
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval {
                approval,
                detail_truncated,
                ..
            } => Some((approval.id, *detail_truncated)),
            _ => None,
        })
        .unwrap();
    assert!(truncated);
    live.client
        .submit_decision(live.session_id, id, Decision::AcceptForSession)
        .await
        .unwrap();
    assert_eq!(
        native_response(&copilot, "large").await["result"]["approval"]["commandIdentifiers"][0],
        identifier
    );
    live.shutdown().await;
}

#[tokio::test]
async fn repeated_tool_identity_keeps_callbacks_distinct_and_native_completion_withdraws_one() {
    let copilot = fixture(
        r#"      event first permission.requested '{"requestId":"first","permissionRequest":{"kind":"read","toolCallId":"shared","path":"one","intention":"First"}}'
      event second permission.requested '{"requestId":"second","permissionRequest":{"kind":"read","toolCallId":"shared","path":"two","intention":"Second"}}'
      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      event done permission.completed '{"requestId":"first","result":{"kind":"cancelled"},"toolCallId":"shared"}'
"#,
        "",
    );
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-callback-identity",
        "Request twice",
    )
    .await;
    let pending = live
        .wait_for("both callbacks remain distinct", |snapshot| {
            snapshot.pending_approvals.len() == 2
        })
        .await;
    let first = approval(&pending, "First").0;
    let second = approval(&pending, "Second").0;
    assert_ne!(first, second);
    copilot.release();
    let withdrawn = live.wait_for("only completed callback is withdrawn", |snapshot| {
        snapshot.pending_approvals == [second]
            && snapshot.activities.iter().any(|activity| matches!(activity,
                Activity::Approval { approval, outcome: ApprovalOutcome::Withdrawn, .. } if approval.id == first))
    }).await;
    assert_eq!(withdrawn.pending_approvals, [second]);
    live.shutdown().await;
}

#[tokio::test]
async fn an_already_resolved_native_callback_withdraws_instead_of_becoming_retryable() {
    let false_arm = r#"    *'"method":"session.permissions.handlePendingPermissionRequest"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"success":false}}'
      ;;
"#;
    let arms = conversation_arms().replace(&permission_decision_arm(), false_arm);
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}",
        arms,
        send_arm(
            r#"      event p permission.requested '{"requestId":"stale","permissionRequest":{"kind":"read","path":"gone","intention":"Stale"}}'
"#
        ),
    ));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-stale-approval",
        "Ask",
    )
    .await;
    let pending = live
        .wait_for("Approval is pending", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let id = pending.pending_approvals[0];
    let error = live
        .client
        .submit_decision(live.session_id, id, Decision::Accept)
        .await
        .expect_err("already-resolved native request is refused");
    assert!(
        error
            .to_string()
            .contains("Approval was already resolved and is no longer available"),
        "the public error reports the definite native outcome: {error}"
    );
    live.wait_for("stale native callback is unavailable", |snapshot| {
        snapshot.pending_approvals.is_empty()
            && snapshot.activities.iter().any(|activity| matches!(activity,
                Activity::Approval { approval, outcome: ApprovalOutcome::Withdrawn, .. } if approval.id == id))
    }).await;
    live.shutdown().await;
}
