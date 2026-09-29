//! Claude native permission callbacks through the public Session seam.

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{
        CLAUDE_MODELS, LiveTurn, ScriptedClaude, discovery_arms, settled_session, user_turn_arm,
    },
};
use serde_json::{Value, json};
use std::path::PathBuf;
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, ApprovalId, ApprovalOutcome, ApprovalPosture,
        ApprovalSubject, ClaudePermissionMode, Decision, FileChange, InitialPrompt, PromptDelivery,
        PromptId, SessionSnapshot, TurnStatus, UpdateApprovalPostureRequest,
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
    let launch = fixture.wait_for_launch_carrying("--session-id").await;
    assert_eq!(launch.value("--permission-mode"), "dontAsk");
    assert_eq!(launch.value("--permission-prompt-tool"), "stdio");
    assert!(!launch.carries("--dangerously-skip-permissions"));
    assert_eq!(launch.value("--setting-sources"), "user,project");
    live.shutdown().await;
}

#[tokio::test]
async fn an_existing_session_applies_permission_mode_over_the_native_control_channel() {
    let posture_arm = r#"    *'"subtype":"set_permission_mode"'*)
      emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{}}}'
      ;;
"#;
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        posture_arm,
        user_turn_arm(
            r#"      (
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"prov-session"}'
      ) &
"#,
        ),
    ));
    let live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-next-turn-posture",
        "First",
    )
    .await;
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
    fixture.release();
    settled_session(&live.client, live.session_id, 0).await;
    live.client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Second".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
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
    assert_eq!(
        sessions.len(),
        1,
        "changing posture does not restart Claude"
    );
    assert_eq!(sessions[0].value("--permission-mode"), "default");
    assert!(fixture.requests().iter().any(|request| {
        request["type"] == "control_request"
            && request["request"]["subtype"] == "set_permission_mode"
            && request["request"]["mode"] == "auto"
    }));
    live.shutdown().await;
}

#[tokio::test]
async fn a_failed_live_mode_update_is_visible_and_retried_without_restarting_on_the_next_turn() {
    let posture_arm = r#"    *'"subtype":"set_permission_mode"'*)
      posture_updates=$(( ${posture_updates:-0} + 1 ))
      if [ "$posture_updates" -gt 1 ]; then
        emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{}}}'
      fi
      ;;
"#;
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        posture_arm,
        user_turn_arm(
            r#"      (
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{"type":"result","subtype":"success","is_error":false,"result":"Done","session_id":"prov-session"}'
      ) &
"#,
        ),
    ));
    let live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable())
            .with_control_request_timeout(tokio::time::Duration::from_millis(50)),
        "claude-live-posture-failure",
        "Wait",
    )
    .await;
    let requested = ApprovalPosture::Claude {
        permission_mode: ClaudePermissionMode::Auto,
    };
    let error = live
        .client
        .update_approval_posture(
            live.session_id,
            UpdateApprovalPostureRequest {
                posture: Some(requested),
            },
        )
        .await
        .expect_err("a native control timeout reaches the caller");
    assert!(error.to_string().contains("timed out"), "{error}");
    let snapshot = live.client.read_session(live.session_id).await.unwrap();
    let posture = snapshot.session.approval_posture.unwrap();
    assert_eq!(posture.value, requested);
    assert!(posture.pinned);
    assert_eq!(
        posture.application,
        suru::protocol::ApprovalPostureApplication::Failed
    );
    fixture.release();
    settled_session(&live.client, live.session_id, 0).await;
    live.client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Retry".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .unwrap();
    settled_session(&live.client, live.session_id, 1).await;
    assert_eq!(
        fixture
            .exact_launches()
            .into_iter()
            .filter(|launch| launch.carries("--permission-prompt-tool"))
            .count(),
        1,
        "retrying the native control keeps the existing Claude process"
    );
    assert_eq!(
        fixture
            .requests()
            .iter()
            .filter(|request| request["request"]["subtype"] == "set_permission_mode")
            .count(),
        2
    );
    live.shutdown().await;
}

#[tokio::test]
async fn immediate_native_completion_waits_for_the_decision_to_be_recorded() {
    let request = request("immediate", "Bash", json!({"command":"false"}), json!({}));
    let completion = r#"    *'"type":"control_response"'*)
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"],"terminal_reason":"aborted_tools","session_id":"prov-session"}'
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
        json!({"command":"cd /srv/app && cargo test; echo done"}),
        json!({}),
    )
    .replace("tool-bash-repeat", "tool-bash");
    let timeline = [
        request("bash", "Bash", json!({"command":"cd /srv/app && cargo check","description":"verify"}), json!({"decision_reason":"Run checks"})),
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
        ApprovalSubject::Command { command, cwd: Some(cwd), actions } if command == "cargo check" && cwd == std::path::Path::new("/srv/app") && actions.is_empty()) && approval.reason.as_deref() == Some("Run checks")),
        "a leading change of directory is where the approved command runs, not part of it");
    assert!(approvals.iter().any(|approval| matches!(&approval.subject,
        ApprovalSubject::Command { command, .. } if command == "cd /srv/app && cargo test; echo done")),
        "an approved command part of which may run outside the directory it changes into is presented whole");
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
                    command == "cd /srv/app && cargo test; echo done"
                }
                (ApprovalSubject::FileChange { paths, .. }, "edit") => paths[0] == *"src/main.rs",
                (ApprovalSubject::FileChange { paths, .. }, "write") => paths[0] == *"notes.txt",
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
        json!({"behavior":"allow","updatedInput":{"command":"cd /srv/app && cargo check","description":"verify"}}),
        "the command Claude runs is its own, however the Approval presented it"
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

/// One stream-json message as a fixture `emit` line.
fn emit(message: &Value) -> String {
    let line = message.to_string();
    assert!(
        !line.contains('\''),
        "an emitted line is single-quoted in the fixture, got {line}"
    );
    format!("      emit '{line}'\n")
}

/// A chunk of the loop's own streaming conversation.
fn chunk(event: Value) -> String {
    emit(&json!({
        "type": "stream_event",
        "event": event,
        "parent_tool_use_id": null,
        "session_id": "prov-session",
    }))
}

/// The chunks that open the `tool_use` block `index`, using the tool `name` under the tool-use id
/// `id`, and stream its `input` — everything of the block short of its close.
fn opened_tool_use(index: usize, id: &str, name: &str, input: &Value) -> String {
    [
        chunk(json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
        })),
        chunk(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()},
        })),
    ]
    .concat()
}

fn closed_block(index: usize) -> String {
    chunk(json!({"type": "content_block_stop", "index": index}))
}

/// Claude asking, under the control request `request_id`, whether its use `tool_use_id` of the
/// tool `name` may run with `input`.
fn can_use_tool(request_id: &str, name: &str, tool_use_id: &str, input: &Value) -> String {
    emit(&json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "can_use_tool",
            "tool_name": name,
            "tool_use_id": tool_use_id,
            "input": input,
        },
    }))
}

/// The tool results the loop echoes back, each `(tool-use id, content, is_error)`.
fn tool_results(results: &[(&str, &str, bool)]) -> String {
    let content = results
        .iter()
        .map(|(id, content, is_error)| {
            json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error})
        })
        .collect::<Vec<_>>();
    emit(&json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "parent_tool_use_id": null,
        "session_id": "prov-session",
    }))
}

/// One streamed assistant message saying `text`.
fn said(text: &str) -> String {
    [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        chunk(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
        chunk(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})),
        closed_block(0),
        chunk(json!({"type": "message_stop"})),
    ]
    .concat()
}

fn turn_result(text: &str) -> String {
    emit(&json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": text,
        "session_id": "prov-session",
    }))
}

/// An arm playing `timeline` once Suru answers the control request `request_id` — an identity
/// only the fixture's requests carry, so only Suru's answer to it names it.
fn answered_arm(request_id: &str, timeline: &str) -> String {
    format!("    *'\"request_id\":\"{request_id}\"'*)\n{timeline}      ;;\n")
}

/// The Approval `id` as the Transcript holds it.
fn approval_of(snapshot: &SessionSnapshot, id: ApprovalId) -> &Activity {
    snapshot
        .activities
        .iter()
        .find(
            |activity| matches!(activity, Activity::Approval { approval, .. } if approval.id == id),
        )
        .unwrap_or_else(|| {
            panic!(
                "the Approval stands in the Transcript: {:?}",
                snapshot.activities
            )
        })
}

/// The row the Approval `id` links to as the use it gates.
fn gated_row(snapshot: &SessionSnapshot, id: ApprovalId) -> &Activity {
    let Activity::Approval {
        tool_activity_id, ..
    } = approval_of(snapshot, id)
    else {
        unreachable!("approval_of finds only Approvals");
    };
    let linked = tool_activity_id.unwrap_or_else(|| {
        panic!(
            "the Approval links to the row it gates: {:?}",
            snapshot.activities
        )
    });
    snapshot
        .activities
        .iter()
        .find(|activity| activity.id() == linked)
        .expect("the linked row stands in the Transcript")
}

/// Each pending Approval's subject beside the row it links to.
fn gated_rows(snapshot: &SessionSnapshot) -> Vec<(ApprovalSubject, Activity)> {
    snapshot
        .pending_approvals
        .iter()
        .map(|id| {
            let Activity::Approval { approval, .. } = approval_of(snapshot, *id) else {
                unreachable!("approval_of finds only Approvals");
            };
            (approval.subject.clone(), gated_row(snapshot, *id).clone())
        })
        .collect()
}

fn update(path: &str) -> Vec<FileChange> {
    vec![FileChange::Update {
        path: PathBuf::from(path),
        moved_to: None,
    }]
}

#[tokio::test]
async fn every_gated_tool_use_links_its_approval_to_the_row_recording_it() {
    let uses = [
        ("toolu_bash", "Bash", json!({"command": "cargo check"})),
        (
            "toolu_edit",
            "Edit",
            json!({"file_path": "src/main.rs", "old_string": "a", "new_string": "b"}),
        ),
        (
            "toolu_multi_edit",
            "MultiEdit",
            json!({"file_path": "src/lib.rs", "edits": [{"old_string": "a", "new_string": "b"}]}),
        ),
        (
            "toolu_notebook_edit",
            "NotebookEdit",
            json!({"notebook_path": "analysis.ipynb", "cell_id": "cell-1", "new_source": "print(1)"}),
        ),
        (
            "toolu_write",
            "Write",
            json!({"file_path": "notes.txt", "content": "hello"}),
        ),
        ("toolu_read", "Read", json!({"file_path": "Cargo.toml"})),
        (
            "toolu_fetch",
            "WebFetch",
            json!({"url": "https://example.test/data", "prompt": "summarize"}),
        ),
        (
            "toolu_mcp",
            "mcp__linear__create_issue",
            json!({"title": "Link the Approval"}),
        ),
    ];
    // Each block closes before its use asks, as a use whose input the CLI has whole does.
    let timeline = std::iter::once(chunk(
        json!({"type": "message_start", "message": {"role": "assistant"}}),
    ))
    .chain(uses.iter().enumerate().map(|(index, (id, name, input))| {
        [
            opened_tool_use(index, id, name, input),
            closed_block(index),
            can_use_tool(id, name, id, input),
        ]
        .concat()
    }))
    .chain(std::iter::once(chunk(json!({"type": "message_stop"}))))
    .collect::<String>();
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-gated-rows",
        "Use every Tool",
    )
    .await;

    let snapshot = live
        .wait_for("every gated use asks for an Approval", |snapshot| {
            snapshot.pending_approvals.len() == uses.len()
        })
        .await;
    let gated = gated_rows(&snapshot);
    let links = |what: &str, pairs: fn(&(ApprovalSubject, Activity)) -> bool| {
        assert!(gated.iter().any(pairs), "{what}, got {gated:#?}");
    };
    links("a Bash Approval links to its Command", |pair| {
        matches!(pair, (ApprovalSubject::Command { command, .. }, Activity::Command { command: ran, .. })
            if command == "cargo check" && ran == "cargo check")
    });
    links("an Edit Approval links to its File Change", |pair| {
        matches!(pair, (ApprovalSubject::FileChange { paths, .. }, Activity::FileChange { changes, .. })
            if paths == &[PathBuf::from("src/main.rs")] && changes == &update("src/main.rs"))
    });
    links(
        "a MultiEdit Approval asks about the file it edits and links to its File Change",
        |pair| {
            matches!(pair, (ApprovalSubject::FileChange { paths, .. }, Activity::FileChange { changes, .. })
            if paths == &[PathBuf::from("src/lib.rs")] && changes == &update("src/lib.rs"))
        },
    );
    links("a NotebookEdit Approval links to its File Change", |pair| {
        matches!(pair, (ApprovalSubject::FileChange { paths, .. }, Activity::FileChange { changes, .. })
            if paths == &[PathBuf::from("analysis.ipynb")] && changes == &update("analysis.ipynb"))
    });
    links("a Write Approval links to its File Change", |pair| {
        matches!(pair, (ApprovalSubject::FileChange { paths, .. }, Activity::FileChange { changes, .. })
            if paths == &[PathBuf::from("notes.txt")]
                && changes == &[FileChange::Add { path: PathBuf::from("notes.txt") }])
    });
    links("a Read Approval links to its Tool Call", |pair| {
        matches!(pair, (ApprovalSubject::Read { path }, Activity::ToolCall { name, server: None, status: ActivityStatus::Active, .. })
            if path == &PathBuf::from("Cargo.toml") && name == "Read")
    });
    links("a WebFetch Approval links to its Tool Call", |pair| {
        matches!(pair, (ApprovalSubject::Network { host_or_url }, Activity::ToolCall { name, server: None, .. })
            if host_or_url == "https://example.test/data" && name == "WebFetch")
    });
    links("an MCP Tool's Approval links to its Tool Call", |pair| {
        matches!(pair, (ApprovalSubject::OtherTool { name, .. }, Activity::ToolCall { name: tool, server: Some(server), .. })
            if name == "mcp__linear__create_issue" && tool == "create_issue" && server == "linear")
    });
    let rows = gated
        .iter()
        .map(|(_, row)| row.id())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        rows.len(),
        uses.len(),
        "each Approval links to a row of its own"
    );
    live.shutdown().await;
}

#[tokio::test]
async fn an_approval_arriving_before_its_block_closes_links_to_the_row_the_block_opened() {
    let fetch = json!({"url": "https://example.test/data", "prompt": "summarize"});
    let write = json!({"file_path": "notes.txt", "content": "hello"});
    let message_start = chunk(json!({"type": "message_start", "message": {"role": "assistant"}}));
    let message_stop = chunk(json!({"type": "message_stop"}));
    // Each use asks while its block is still open; answering it is what lets the block close.
    let timeline = [
        message_start.clone(),
        opened_tool_use(0, "toolu_fetch", "WebFetch", &fetch),
        can_use_tool("fetch", "WebFetch", "toolu_fetch", &fetch),
    ]
    .concat();
    let fetched = [
        closed_block(0),
        message_stop.clone(),
        tool_results(&[("toolu_fetch", "Fetched the data.", false)]),
        message_start,
        opened_tool_use(0, "toolu_write", "Write", &write),
        can_use_tool("write", "Write", "toolu_write", &write),
    ]
    .concat();
    let written = [
        closed_block(0),
        message_stop,
        tool_results(&[(
            "toolu_write",
            "File created successfully at: notes.txt",
            false,
        )]),
        turn_result("Saved."),
    ]
    .concat();
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        answered_arm("fetch", &fetched),
        answered_arm("write", &written),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-approval-before-close",
        "Fetch and save",
    )
    .await;

    let asked = live
        .wait_for("the fetch asks while its block is open", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let fetch_approval = asked.pending_approvals[0];
    let Activity::ToolCall {
        id: fetch_row,
        status,
        name,
        input,
        ..
    } = gated_row(&asked, fetch_approval)
    else {
        panic!(
            "the fetch's Approval links to its Tool Call: {:?}",
            asked.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(name, "WebFetch");
    assert_eq!(input, "", "the row stands before its block has closed");
    let fetch_row = *fetch_row;
    live.client
        .submit_decision(live.session_id, fetch_approval, Decision::Accept)
        .await
        .unwrap();

    let asked = live
        .wait_for("the write asks while its block is open", |snapshot| {
            snapshot.pending_approvals.len() == 1 && snapshot.pending_approvals[0] != fetch_approval
        })
        .await;
    let write_approval = asked.pending_approvals[0];
    let Activity::FileChange {
        id: write_row,
        status,
        changes,
        ..
    } = gated_row(&asked, write_approval)
    else {
        panic!(
            "the write's Approval links to its File Change: {:?}",
            asked.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(
        *changes,
        [FileChange::Add {
            path: PathBuf::from("notes.txt")
        }],
        "the Approval's input, the first whole one seen, opens the File Change with its change"
    );
    let write_row = *write_row;
    live.client
        .submit_decision(live.session_id, write_approval, Decision::Accept)
        .await
        .unwrap();

    let settled = live
        .wait_for("the Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(gated_row(&settled, fetch_approval).id(), fetch_row);
    let Activity::ToolCall {
        status,
        input,
        output,
        ..
    } = gated_row(&settled, fetch_approval)
    else {
        unreachable!("the link does not move");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert!(
        input.contains("https://example.test/data"),
        "the input fills in once the block closes, got {input:?}"
    );
    assert_eq!(output, "Fetched the data.");
    assert_eq!(gated_row(&settled, write_approval).id(), write_row);
    assert!(
        matches!(gated_row(&settled, write_approval), Activity::FileChange { status: ActivityStatus::Completed, changes, .. }
            if changes == &[FileChange::Add { path: PathBuf::from("notes.txt") }]),
        "the File Change still records the write once its block closes: {:?}",
        settled.activities
    );
    assert_eq!(
        settled
            .activities
            .iter()
            .filter(|activity| matches!(
                activity,
                Activity::ToolCall { .. } | Activity::FileChange { .. }
            ))
            .count(),
        2,
        "each use is one row: {:?}",
        settled.activities
    );
    live.shutdown().await;
}

#[tokio::test]
async fn an_approval_of_an_edit_naming_no_file_links_to_the_tool_call_it_is() {
    let edit = json!({"old_string": "teh", "new_string": "the"});
    let missing_path = "<tool_use_error>InputValidationError: The required parameter `file_path` \
                        is missing</tool_use_error>";
    // The edit asks while its block is still open; answering it is what lets the block close.
    let timeline = [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        opened_tool_use(0, "toolu_edit", "Edit", &edit),
        can_use_tool("edit", "Edit", "toolu_edit", &edit),
    ]
    .concat();
    let answered = [
        closed_block(0),
        chunk(json!({"type": "message_stop"})),
        tool_results(&[("toolu_edit", missing_path, true)]),
        turn_result("Could not edit."),
    ]
    .concat();
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        answered_arm("edit", &answered),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-pathless-edit-approval",
        "Fix the typo",
    )
    .await;

    let asked = live
        .wait_for("the edit asks while its block is open", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let approval = asked.pending_approvals[0];
    let Activity::ToolCall {
        id: row,
        status,
        name,
        input,
        ..
    } = gated_row(&asked, approval)
    else {
        panic!(
            "an Approval of an edit naming no file links to its Tool Call: {:?}",
            asked.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Active);
    assert_eq!(name, "Edit");
    assert_eq!(
        input, "new_string=the old_string=teh",
        "the Approval's input, the first whole one seen, fills the row in as it opens"
    );
    let row = *row;
    live.client
        .submit_decision(live.session_id, approval, Decision::Accept)
        .await
        .unwrap();

    let settled = live
        .wait_for("the Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        gated_row(&settled, approval).id(),
        row,
        "the link does not move"
    );
    let rows = settled
        .activities
        .iter()
        .filter(|activity| !matches!(activity, Activity::Approval { .. }))
        .collect::<Vec<_>>();
    assert!(
        matches!(rows[..], [
            Activity::ToolCall { status: ActivityStatus::Failed, name, input, output, .. },
        ] if name == "Edit" && input == "new_string=the old_string=teh" && output == missing_path),
        "the edit is one failed Tool Call showing its input and error, and no File Change: \
         {rows:#?}"
    );
    live.shutdown().await;
}

/// The rows the Approvals of `snapshot` link to, in the order the Approvals were asked.
fn rows_linked_in_order(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval { approval, .. } => Some(gated_row(snapshot, approval.id)),
            _ => None,
        })
        .collect()
}

/// Asserts that every row of `snapshot` besides its Approvals is one an Approval links to, in the
/// same order: each use one row, and no row — an empty File Change among them — left over.
fn assert_each_use_is_the_row_its_approval_links_to(snapshot: &SessionSnapshot) {
    let recorded = snapshot
        .activities
        .iter()
        .filter(|activity| !matches!(activity, Activity::Approval { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        recorded
            .iter()
            .map(|activity| activity.id())
            .collect::<Vec<_>>(),
        rows_linked_in_order(snapshot)
            .iter()
            .map(|activity| activity.id())
            .collect::<Vec<_>>(),
        "each use is one row, the one its Approval links to: {recorded:#?}"
    );
}

#[tokio::test]
async fn the_first_whole_input_seen_of_an_edit_decides_its_row_whatever_copy_follows() {
    // A PreToolUse hook may rewrite the input an Approval carries, so it need not agree with the
    // input the block streams: here each edit's two copies disagree about naming a file.
    let naming =
        |path: &str, word: &str| json!({"file_path": path, "old_string": word, "new_string": "b"});
    let unnamed = |word: &str| json!({"old_string": word, "new_string": "b"});
    let timeline = [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        // The Approval names a file before a close that names none.
        opened_tool_use(0, "toolu_one", "Edit", &unnamed("one")),
        can_use_tool("gate-one", "Edit", "toolu_one", &naming("one.rs", "one")),
        closed_block(0),
        // The Approval names no file before a close that names one.
        opened_tool_use(1, "toolu_two", "Edit", &naming("two.rs", "two")),
        can_use_tool("gate-two", "Edit", "toolu_two", &unnamed("two")),
        closed_block(1),
        // A close naming no file before an Approval that names one.
        opened_tool_use(2, "toolu_three", "Edit", &unnamed("three")),
        closed_block(2),
        can_use_tool(
            "gate-three",
            "Edit",
            "toolu_three",
            &naming("three.rs", "three"),
        ),
        // A close naming a file before an Approval that names none.
        opened_tool_use(3, "toolu_four", "Edit", &naming("four.rs", "four")),
        closed_block(3),
        can_use_tool("gate-four", "Edit", "toolu_four", &unnamed("four")),
        chunk(json!({"type": "message_stop"})),
    ]
    .concat();
    let edited = [
        tool_results(&[
            ("toolu_one", "Edited one.", false),
            ("toolu_two", "Edited two.", false),
            ("toolu_three", "Edited three.", false),
            ("toolu_four", "Edited four.", false),
        ]),
        turn_result("Edited."),
    ]
    .concat();
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        answered_arm("gate-four", &edited),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-disagreeing-edit-inputs",
        "Edit four files",
    )
    .await;

    let asked = live
        .wait_for("every edit asks", |snapshot| {
            snapshot.pending_approvals.len() == 4
        })
        .await;
    let rows = rows_linked_in_order(&asked);
    assert!(
        matches!(rows[..], [
            Activity::FileChange { status: ActivityStatus::Active, changes: one, .. },
            Activity::ToolCall { status: ActivityStatus::Active, name: two_tool, input: two, .. },
            Activity::ToolCall { status: ActivityStatus::Active, name: three_tool, input: three, .. },
            Activity::FileChange { status: ActivityStatus::Active, changes: four, .. },
        ] if one == &update("one.rs")
            && two_tool == "Edit" && two == "new_string=b old_string=two"
            && three_tool == "Edit" && three == "new_string=b old_string=three"
            && four == &update("four.rs")),
        "each edit is the row its first whole input decided, filled in from that input, and its \
         Approval links to it: {rows:#?}"
    );
    let linked = rows.iter().map(|row| row.id()).collect::<Vec<_>>();
    assert_each_use_is_the_row_its_approval_links_to(&asked);
    let approvals = asked
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval { approval, .. } => Some(approval.id),
            _ => None,
        })
        .collect::<Vec<_>>();
    for approval in approvals {
        live.client
            .submit_decision(live.session_id, approval, Decision::Accept)
            .await
            .unwrap();
    }

    let settled = live
        .wait_for("the Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let rows = rows_linked_in_order(&settled);
    assert_eq!(
        rows.iter().map(|row| row.id()).collect::<Vec<_>>(),
        linked,
        "no link moves"
    );
    assert!(
        matches!(rows[..], [
            Activity::FileChange { status: ActivityStatus::Completed, changes: one, .. },
            Activity::ToolCall { status: ActivityStatus::Completed, input: two, output: two_output, .. },
            Activity::ToolCall { status: ActivityStatus::Completed, input: three, output: three_output, .. },
            Activity::FileChange { status: ActivityStatus::Completed, changes: four, .. },
        ] if one == &update("one.rs")
            && two == "new_string=b old_string=two" && two_output == "Edited two."
            && three == "new_string=b old_string=three" && three_output == "Edited three."
            && four == &update("four.rs")),
        "each row settles from its result, neither refilled nor reclassified by the copy that \
         followed: {rows:#?}"
    );
    assert_each_use_is_the_row_its_approval_links_to(&settled);
    live.shutdown().await;
}

#[tokio::test]
async fn a_declined_use_settles_its_row_as_failed_and_the_turn_carries_on() {
    let read = json!({"file_path": "private.txt"});
    let edit = json!({"file_path": "src/main.rs", "old_string": "a", "new_string": "b"});
    let timeline = [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        opened_tool_use(0, "toolu_read", "Read", &read),
        closed_block(0),
        can_use_tool("read", "Read", "toolu_read", &read),
        opened_tool_use(1, "toolu_edit", "Edit", &edit),
        closed_block(1),
        can_use_tool("edit", "Edit", "toolu_edit", &edit),
        chunk(json!({"type": "message_stop"})),
    ]
    .concat();
    // Claude tells the loop each use was refused, as it does for every denied use, and carries on
    // — but only once released, so the rows must settle on the Decisions alone.
    let declined = format!(
        "      (\n        while [ ! -e \"$CLAUDE_FIXTURE_RELEASE\" ]; do sleep 0.01; done\n{}{}{}      ) &\n",
        tool_results(&[
            ("toolu_read", "User declined the tool request", true),
            ("toolu_edit", "User declined the tool request", true),
        ]),
        said("I was not allowed to read it."),
        turn_result("I was not allowed to read it."),
    );
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        answered_arm("edit", &declined),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-declined-use",
        "Read and edit",
    )
    .await;

    let asked = live
        .wait_for("both uses ask", |snapshot| {
            snapshot.pending_approvals.len() == 2
        })
        .await;
    let (read_approval, edit_approval) = match gated_row(&asked, asked.pending_approvals[0]) {
        Activity::ToolCall { .. } => (asked.pending_approvals[0], asked.pending_approvals[1]),
        _ => (asked.pending_approvals[1], asked.pending_approvals[0]),
    };
    assert!(
        matches!(gated_row(&asked, read_approval), Activity::ToolCall { name, status: ActivityStatus::Active, .. } if name == "Read")
    );
    assert!(matches!(
        gated_row(&asked, edit_approval),
        Activity::FileChange {
            status: ActivityStatus::Active,
            ..
        }
    ));
    live.client
        .submit_decision(live.session_id, read_approval, Decision::Decline)
        .await
        .unwrap();
    live.client
        .submit_decision(live.session_id, edit_approval, Decision::Decline)
        .await
        .unwrap();

    let declined = live
        .wait_for("the declined uses settle as failed", |snapshot| {
            [read_approval, edit_approval].iter().all(|id| {
                matches!(
                    gated_row(snapshot, *id),
                    Activity::ToolCall {
                        status: ActivityStatus::Failed,
                        ..
                    } | Activity::FileChange {
                        status: ActivityStatus::Failed,
                        ..
                    }
                )
            })
        })
        .await;
    assert_eq!(
        declined.turns[0].status,
        TurnStatus::Active,
        "the rows settle on the Decisions, before Claude says anything more"
    );
    assert!(matches!(
        approval_of(&declined, read_approval),
        Activity::Approval {
            outcome: ApprovalOutcome::Decided,
            decision: Some(Decision::Decline),
            ..
        }
    ));
    fixture.release();

    let settled = live
        .wait_for("the Turn carries on and settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert!(
        settled
            .messages
            .iter()
            .any(|message| message.content == "I was not allowed to read it."),
        "the loop carries on past the refusals: {:?}",
        settled.messages
    );
    let Activity::ToolCall { status, output, .. } = gated_row(&settled, read_approval) else {
        unreachable!("the link does not move");
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        output, "User declined the tool request",
        "the refusal is the output, once, however Claude echoes it"
    );
    assert!(matches!(
        gated_row(&settled, edit_approval),
        Activity::FileChange {
            status: ActivityStatus::Failed,
            ..
        }
    ));
    live.shutdown().await;
}

#[tokio::test]
async fn a_use_declined_before_its_block_closes_settles_failed_showing_what_it_would_have_done() {
    let fetch = json!({"url": "https://example.test/data", "prompt": "summarize"});
    let write = json!({"file_path": "notes.txt", "content": "hello"});
    let hold = |gate: &str, timeline: String| {
        format!(
            "      (\n        while [ ! -e \"$CLAUDE_FIXTURE_RELEASE{gate}\" ]; do sleep 0.01; done\n{timeline}      ) &\n"
        )
    };
    // Each use asks while its block is open, and its block closes only once the test lets it — so
    // whatever the rows show of their input before then came from the Approval alone.
    let timeline = [
        chunk(json!({"type": "message_start", "message": {"role": "assistant"}})),
        opened_tool_use(0, "toolu_fetch", "WebFetch", &fetch),
        can_use_tool("fetch", "WebFetch", "toolu_fetch", &fetch),
    ]
    .concat();
    let fetch_declined = hold(
        "",
        [
            closed_block(0),
            opened_tool_use(1, "toolu_write", "Write", &write),
            can_use_tool("write", "Write", "toolu_write", &write),
        ]
        .concat(),
    );
    let write_declined = hold(
        "-write",
        [
            closed_block(1),
            chunk(json!({"type": "message_stop"})),
            tool_results(&[
                ("toolu_fetch", "User declined the tool request", true),
                ("toolu_write", "User declined the tool request", true),
            ]),
            emit(&json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "terminal_reason": "aborted_tools",
                "session_id": "prov-session",
            })),
        ]
        .concat(),
    );
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        answered_arm("fetch", &fetch_declined),
        answered_arm("write", &write_declined),
        user_turn_arm(&timeline),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-declined-before-close",
        "Fetch and save",
    )
    .await;

    let asked = live
        .wait_for("the fetch asks while its block is open", |snapshot| {
            snapshot.pending_approvals.len() == 1
        })
        .await;
    let fetch_approval = asked.pending_approvals[0];
    live.client
        .submit_decision(live.session_id, fetch_approval, Decision::Decline)
        .await
        .unwrap();
    let declined = live
        .wait_for("the declined fetch settles as failed", |snapshot| {
            matches!(
                gated_row(snapshot, fetch_approval),
                Activity::ToolCall {
                    status: ActivityStatus::Failed,
                    ..
                }
            )
        })
        .await;
    let Activity::ToolCall { input, output, .. } = gated_row(&declined, fetch_approval) else {
        unreachable!("the wait found a Tool Call");
    };
    assert_eq!(
        input, "prompt=summarize url=https://example.test/data",
        "the refused use still says what it would have done, though its block never closed"
    );
    assert_eq!(output, "User declined the tool request");
    fixture.release();

    let asked = live
        .wait_for("the write asks while its block is open", |snapshot| {
            snapshot.pending_approvals.len() == 1 && snapshot.pending_approvals[0] != fetch_approval
        })
        .await;
    let write_approval = asked.pending_approvals[0];
    live.client
        .submit_decision(
            live.session_id,
            write_approval,
            Decision::DeclineAndInterrupt,
        )
        .await
        .unwrap();
    let declined = live
        .wait_for(
            "the write declined with an interrupt settles as failed",
            |snapshot| {
                matches!(
                    gated_row(snapshot, write_approval),
                    Activity::FileChange {
                        status: ActivityStatus::Failed,
                        ..
                    }
                )
            },
        )
        .await;
    assert_eq!(
        declined.turns[0].status,
        TurnStatus::Active,
        "the row settles on the Decision, before Claude ends the Turn"
    );
    assert!(
        matches!(gated_row(&declined, write_approval), Activity::FileChange { changes, .. }
            if changes == &[FileChange::Add { path: PathBuf::from("notes.txt") }]),
        "the refused write still names the file it would have added: {:?}",
        declined.activities
    );
    fixture.release_gate("write");

    let settled = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;
    assert_eq!(settled.turns[0].status, TurnStatus::Interrupted);
    let rows = settled
        .activities
        .iter()
        .filter(|activity| {
            matches!(
                activity,
                Activity::ToolCall { .. } | Activity::FileChange { .. }
            )
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(rows[..], [
            Activity::ToolCall { status: ActivityStatus::Failed, input, output, .. },
            Activity::FileChange { status: ActivityStatus::Failed, changes, .. },
        ] if input == "prompt=summarize url=https://example.test/data"
            && output == "User declined the tool request"
            && changes == &[FileChange::Add { path: PathBuf::from("notes.txt") }]),
        "each refused use is one failed row, unchanged by the close and the echo that follow: {rows:#?}"
    );
    live.shutdown().await;
}
