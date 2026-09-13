//! Claude native permission callbacks through the public Session seam.

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{
        CLAUDE_MODELS, LiveTurn, ScriptedClaude, discovery_arms, settled_session, user_turn_arm,
    },
};
use serde_json::{Value, json};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, ApprovalOutcome, ApprovalPosture, ApprovalSubject,
        ClaudePermissionMode, Decision, InitialPrompt, PromptDelivery, PromptId,
        UpdateApprovalPostureRequest,
    },
    provider::ClaudeRuntime,
};
use tokio::time::timeout;

fn request(id: &str, tool: &str, input: Value, extra: Value) -> String {
    let mut request = json!({
        "subtype":"can_use_tool",
        "tool_name":tool,
        "tool_use_id":format!("tool-{id}"),
        "input":input,
    });
    request
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    format!(
        "      emit '{}'
",
        json!({"type":"control_request","request_id":id,"request":request})
    )
}

async fn native_response(fixture: &ScriptedClaude, id: &str) -> Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(value) = fixture.requests().into_iter().find(|request| {
                request["type"] == "control_response" && request["response"]["request_id"] == id
            }) {
                return value["response"]["response"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native Approval response arrives")
}

#[tokio::test]
async fn configured_permission_mode_is_fixed_on_the_user_session_launch() {
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm("      :\n"),
    ));
    let live = LiveTurn::start_configured(
        ClaudeRuntime::new(fixture.executable()),
        "claude-configured-permission-mode",
        "Wait",
        r#"{"provider":{"claude":{"permissionMode":"dontAsk"}}}"#,
    )
    .await;
    let launch = fixture.launch_carrying("--session-id");
    assert_eq!(launch.value("--permission-mode"), "dontAsk");
    assert_eq!(launch.value("--permission-prompt-tool"), "stdio");
    assert!(!launch.carries("--dangerously-skip-permissions"));
    assert_eq!(launch.value("--setting-sources"), "user,project");
    live.shutdown().await;
}

#[tokio::test]
async fn the_next_turn_launches_with_the_sessions_current_permission_mode() {
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(
            r#"      while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"prov-session"}'
"#,
        ),
    ));
    let live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-next-turn-posture",
        "First",
    )
    .await;
    fixture.release();
    settled_session(&live.client, live.session_id, 0).await;

    live.client
        .update_approval_posture(
            live.session_id,
            UpdateApprovalPostureRequest {
                posture: Some(ApprovalPosture::Claude {
                    permission_mode: ClaudePermissionMode::Auto,
                }),
            },
        )
        .await
        .unwrap();
    live.client
        .admit_prompt(
            live.session_id,
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
    settled_session(&live.client, live.session_id, 1).await;

    let sessions = fixture
        .exact_launches()
        .into_iter()
        .filter(|launch| launch.carries("--permission-prompt-tool"))
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].value("--permission-mode"), "default");
    assert_eq!(sessions[1].value("--permission-mode"), "auto");
    live.shutdown().await;
}

#[tokio::test]
async fn immediate_native_completion_waits_for_the_decision_to_be_recorded() {
    let request = request("immediate", "Bash", json!({"command":"false"}), json!({}));
    let completion = r#"    *'"type":"control_response"'*)
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Stopped","terminal_reason":"aborted_tools","session_id":"prov-session"}'
      ;;
"#;
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&request),
        completion,
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-immediate-approval-completion",
        "Ask then finish",
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
    let settled = live
        .wait_for("native completion follows durable Decision", |snapshot| {
            snapshot.turns[0].status == suru::protocol::TurnStatus::Interrupted
        })
        .await;
    assert!(settled.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::DeclineAndInterrupt), .. }
            if approval.id == id)));
    live.shutdown().await;
}

#[tokio::test]
async fn bounded_history_does_not_change_the_original_native_input() {
    let original = "native-secret-payload".repeat(5000);
    let native = request("large", "NovelTool", json!({"payload":original}), json!({}));
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&native),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-bounded-approval-history",
        "Ask with a large input",
    )
    .await;
    let snapshot = live
        .wait_for("bounded Approval arrives", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let (id, truncated) = snapshot
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
        .submit_decision(live.session_id, id, Decision::Accept)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "large").await["updatedInput"]["payload"]
            .as_str()
            .unwrap()
            .len(),
        "native-secret-payload".len() * 5000
    );
    live.shutdown().await;
}

#[tokio::test]
async fn child_approval_is_attributed_to_its_tool_and_survives_the_parent_result() {
    let input = json!({"command":"cargo check"});
    let child_message = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"child-bash","name":"Bash","input":input}]} ,"parent_tool_use_id":"spawn-child","session_id":"prov-session"});
    let request = json!({"type":"control_request","request_id":"child-approval","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"child-bash","agent_id":"native-agent","input":input}});
    let timeline = format!(
        r#"      emit '{{"type":"system","subtype":"task_started","task_id":"task-child","tool_use_id":"spawn-child","task_type":"local_agent","description":"Run child checks"}}'
      emit '{child_message}'
      emit '{request}'
      (
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{{"type":"result","subtype":"success","is_error":false,"result":"Delegated","session_id":"prov-session"}}'
      ) &
"#
    );
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline),
        crate::support::stop_task_arm(),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-child-approval",
        "Delegate a command",
    )
    .await;
    fixture.release();
    let parent = live
        .wait_for("child Approval survives parent completion", |snapshot| {
            snapshot.turns[0].status == suru::protocol::TurnStatus::Completed
                && snapshot.subagent_approval_count() == 1
        })
        .await;
    assert!(
        !parent
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Approval { .. }))
    );
    let child_id = parent.subagent_interventions[0].session_id;
    let child = live.client.read_session(child_id).await.unwrap();
    let approval = child
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: ApprovalOutcome::Pending,
                ..
            } => Some(approval.id),
            _ => None,
        })
        .unwrap();
    live.client
        .submit_decision(child_id, approval, Decision::Accept)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "child-approval").await["updatedInput"]["command"],
        "cargo check"
    );
    assert_eq!(
        live.client
            .read_session(live.session_id)
            .await
            .unwrap()
            .subagent_approval_count(),
        0
    );
    live.shutdown().await;
}

#[tokio::test]
async fn immediate_native_child_completion_waits_for_its_decision_to_be_recorded() {
    let input = json!({"command":"cargo check"});
    let child_message = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"child-immediate-bash","name":"Bash","input":input}]} ,"parent_tool_use_id":"spawn-immediate-child","session_id":"prov-session"});
    let request = json!({"type":"control_request","request_id":"child-immediate-approval","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"child-immediate-bash","agent_id":"native-agent","input":input}});
    let timeline = format!(
        r#"      emit '{{"type":"system","subtype":"task_started","task_id":"task-immediate-child","tool_use_id":"spawn-immediate-child","task_type":"local_agent","description":"Run immediate child checks"}}'
      emit '{child_message}'
      emit '{request}'
"#
    );
    let completion = r#"    *'"type":"control_response"'*)
      emit '{"type":"system","subtype":"task_notification","task_id":"task-immediate-child","status":"completed","summary":"Done","session_id":"prov-session"}'
      ;;
"#;
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline),
        completion,
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-immediate-child-approval-completion",
        "Delegate a command",
    )
    .await;
    let parent = live
        .wait_for("child Approval is pending", |snapshot| {
            snapshot.subagent_approval_count() == 1
        })
        .await;
    let child_id = parent.subagent_interventions[0].session_id;
    let child = live.client.read_session(child_id).await.unwrap();
    let approval_id = child.pending_approvals[0];
    let mut child_feed = live.client.subscribe_session(child_id).await.unwrap();

    live.client
        .submit_decision(child_id, approval_id, Decision::Accept)
        .await
        .unwrap();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = live.client.read_session(child_id).await.unwrap();
            if snapshot.activities.iter().any(|activity| matches!(activity,
                Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::Accept), .. }
                    if approval.id == approval_id))
            {
                return;
            }
            child_feed.next().await.unwrap().unwrap();
        }
    })
    .await
    .expect("child completion follows the durable Decision");
    let parent = live
        .wait_for("native child completion settles its row", |snapshot| {
            snapshot.activities.iter().any(|activity| matches!(activity,
                Activity::Subagent { status: suru::protocol::ActivityStatus::Completed, session_id, .. }
                    if *session_id == child_id))
        })
        .await;
    assert!(parent.activities.iter().any(|activity| matches!(activity,
        Activity::Subagent { status: suru::protocol::ActivityStatus::Completed, session_id, .. }
            if *session_id == child_id)));
    let child = live.client.read_session(child_id).await.unwrap();
    assert!(child.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Decided, decision: Some(Decision::Accept), .. }
            if approval.id == approval_id)));
    drop(child_feed);
    live.shutdown().await;
}

#[tokio::test]
async fn native_cancellation_withdraws_the_correlated_approval() {
    let timeline = format!(
        "{}      (\n        while [ ! -e \"$CLAUDE_FIXTURE_RELEASE\" ]; do sleep 0.01; done\n        emit '{{\"type\":\"control_cancel_request\",\"request_id\":\"withdraw-approval\"}}'\n      ) &\n",
        request(
            "withdraw-approval",
            "Read",
            json!({"file_path":"private.txt"}),
            json!({})
        )
    );
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-withdraw-approval",
        "Ask then cancel",
    )
    .await;
    let pending = live
        .wait_for("Approval is pending", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let id = pending.pending_approvals[0];
    fixture.release();
    live.wait_for("Approval is withdrawn", |snapshot| snapshot.activities.iter().any(|activity| matches!(activity,
        Activity::Approval { approval, outcome: ApprovalOutcome::Withdrawn, .. } if approval.id == id))).await;
    assert!(
        live.client
            .submit_decision(live.session_id, id, Decision::Decline)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|request| request["type"] == "control_response")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn claude_maps_every_tool_family_and_delivers_all_four_decisions() {
    let repeated_tool = request(
        "bash-repeat",
        "Bash",
        json!({"command":"cargo test"}),
        json!({}),
    )
    .replace("tool-bash-repeat", "tool-bash");
    let timeline = [
        request("bash", "Bash", json!({"command":"cargo check","description":"verify"}), json!({"decision_reason":"Run checks"})),
        repeated_tool,
        request("edit", "Edit", json!({"file_path":"src/main.rs","old_string":"a","new_string":"b"}), json!({"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Edit","ruleContent":"src/**"}],"behavior":"allow","destination":"localSettings"}]})),
        request("write", "Write", json!({"file_path":"notes.txt","content":"hello"}), json!({})),
        request("read", "Read", json!({"file_path":"Cargo.toml"}), json!({})),
        request("network", "WebFetch", json!({"url":"https://example.test/data","prompt":"summarize"}), json!({})),
        request("other", "mcp__linear__create_issue", json!({"title":"Keep the exact native input","metadata":{"private":true}}), json!({"permission_suggestions":[]})),
    ].concat();
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-native-approvals",
        "Exercise native approvals",
    )
    .await;
    let snapshot = live
        .wait_for("all native Approvals reach the Client", |snapshot| {
            snapshot.pending_approvals.len() == 7
        })
        .await;
    let approvals = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: ApprovalOutcome::Pending,
                ..
            } => Some(approval.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::Command { command, cwd: Some(_), actions } if command == "cargo check" && actions.is_empty()) && approval.reason.as_deref() == Some("Run checks")));
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::FileChange { paths, grant_root: None } if paths == &[std::path::PathBuf::from("src/main.rs")])));
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::FileChange { paths, .. } if paths == &[std::path::PathBuf::from("notes.txt")])));
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::Read { path } if path == &std::path::PathBuf::from("Cargo.toml"))));
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::Network { host_or_url } if host_or_url == "https://example.test/data")));
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::OtherTool { name, input } if name == "mcp__linear__create_issue" && input["metadata"]["private"] == true)));

    let approval = |kind: &str| {
        approvals
            .iter()
            .find(|approval| match (&approval.subject, kind) {
                (ApprovalSubject::Command { command, .. }, "bash") => command == "cargo check",
                (ApprovalSubject::Command { command, .. }, "bash-repeat") => {
                    command == "cargo test"
                }
                (ApprovalSubject::FileChange { paths, .. }, "edit") => {
                    paths[0] == std::path::PathBuf::from("src/main.rs")
                }
                (ApprovalSubject::FileChange { paths, .. }, "write") => {
                    paths[0] == std::path::PathBuf::from("notes.txt")
                }
                (ApprovalSubject::Read { .. }, "read") => true,
                (ApprovalSubject::Network { .. }, "network") => true,
                (ApprovalSubject::OtherTool { .. }, "other") => true,
                _ => false,
            })
            .unwrap()
            .id
    };

    live.client
        .submit_decision(live.session_id, approval("bash"), Decision::Accept)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "bash").await,
        json!({"behavior":"allow","updatedInput":{"command":"cargo check","description":"verify"}})
    );

    live.client
        .submit_decision(live.session_id, approval("bash-repeat"), Decision::Decline)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "bash-repeat").await["behavior"],
        "deny"
    );

    live.client
        .submit_decision(
            live.session_id,
            approval("edit"),
            Decision::AcceptForSession,
        )
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "edit").await["updatedPermissions"],
        json!([{
            "type":"addRules","rules":[{"toolName":"Edit","ruleContent":"src/**"}],"behavior":"allow","destination":"session"
        }])
    );

    live.client
        .submit_decision(
            live.session_id,
            approval("other"),
            Decision::AcceptForSession,
        )
        .await
        .unwrap();
    let other = native_response(&fixture, "other").await;
    assert_eq!(other["updatedInput"]["metadata"]["private"], true);
    assert_eq!(
        other["updatedPermissions"],
        json!([{
            "type":"addRules","rules":[{"toolName":"mcp__linear__create_issue"}],"behavior":"allow","destination":"session"
        }])
    );

    live.client
        .submit_decision(live.session_id, approval("read"), Decision::Decline)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "read").await,
        json!({"behavior":"deny","message":"User declined the tool request","interrupt":false})
    );

    live.client
        .submit_decision(
            live.session_id,
            approval("network"),
            Decision::DeclineAndInterrupt,
        )
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "network").await,
        json!({"behavior":"deny","message":"User declined the tool request","interrupt":true})
    );

    live.client
        .submit_decision(live.session_id, approval("write"), Decision::Accept)
        .await
        .unwrap();
    assert_eq!(
        native_response(&fixture, "write").await["updatedInput"]["content"],
        "hello"
    );
    live.shutdown().await;
}
