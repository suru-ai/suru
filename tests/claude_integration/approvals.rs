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
