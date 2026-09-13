//! Codex app-server Approval callbacks through the public Session seam.

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{ScriptedCodex, receive_initial_state},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, ApprovalOutcome, ApprovalSubject, CommandAction, CreateSessionRequest, Decision,
        InitialPrompt, PromptId, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const APPROVALS: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"command-item","command":"cargo check","cwd":"project","status":"inProgress"}}}'
      printf '%s\n' '{"id":"command-accept","method":"item/commandExecution/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"command-item","startedAtMs":1,"reason":"verify workspace","command":"cargo check --tests","cwd":"project/client","commandActions":[{"type":"read","command":"cat Cargo.toml","name":"Cargo.toml","path":"project/Cargo.toml"},{"type":"listFiles","command":"rg --files src","path":"src"},{"type":"search","command":"rg Approval src","query":"Approval","path":"src"},{"type":"unknown","command":"custom-tool"}]}}'
      printf '%s\n' '{"id":"command-decline","method":"item/commandExecution/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"command-item","approvalId":"second-callback","startedAtMs":2,"reason":"repeat callback","command":"cargo check"}}'
      printf '%s\n' '{"id":"network-decline","method":"item/commandExecution/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"command-item","approvalId":"network-callback","startedAtMs":2,"reason":"connect to registry","networkApprovalContext":{"host":"registry.example.test","protocol":"https"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"file-item","changes":[{"path":"old.rs","kind":{"type":"update","movePath":"new.rs"},"diff":"private"},{"path":"added.rs","kind":{"type":"add"},"diff":"private"}],"status":"inProgress"}}}'
      printf '%s\n' '{"id":"file-session","method":"item/fileChange/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"file-item","startedAtMs":3,"reason":"apply generated files","grantRoot":"generated"}}'
      printf '%s\n' '{"id":"permissions-accept","method":"item/permissions/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"command-item","startedAtMs":4,"cwd":"project","reason":"download metadata","permissions":{"network":{"enabled":true},"fileSystem":{"read":["cache"],"write":["generated"]}}}}'
      printf '%s\n' '{"id":"permissions-cancel","method":"item/permissions/requestApproval","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"command-item","startedAtMs":5,"cwd":"project","reason":"stop instead","permissions":{"network":{"enabled":true}}}}'
      printf '%s\n' '{"id":"elicitation","method":"mcpServer/elicitation/request","params":{"message":"unsupported"}}'
      printf '%s\n' '{"id":"dynamic-tool","method":"item/tool/call","params":{"tool":"unsupported"}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"id":4,"result":{}}'
      ;;
  esac
done
"#;

async fn snapshot_until(
    client: &ManagedClient,
    session_id: suru::protocol::SessionId,
    predicate: impl Fn(&suru::protocol::SessionSnapshot) -> bool,
) -> suru::protocol::SessionSnapshot {
    let mut feed = client.subscribe_session(session_id).await.unwrap();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client.read_session(session_id).await.unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            feed.next().await.unwrap().unwrap();
        }
    })
    .await
    .expect("Session reaches expected Approval state")
}

#[tokio::test]
async fn native_approvals_preserve_subjects_callback_identity_decisions_and_interrupt_order() {
    let fixture = ScriptedCodex::new(APPROVALS);
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("suru.jsonc"),
        r#"{"provider":{"codex":{"approvalPolicy":"untrusted","sandboxMode":"read-only"}}}"#,
    )
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "codex-native-approvals")
            .unwrap()
            .with_config_dir(config.path()),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "codex-native-approvals").unwrap(),
    )
    .await
    .unwrap();
    receive_initial_state(&mut client).await;
    let session_id = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Exercise native Approvals".into(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap()
        .session
        .id;

    let snapshot = snapshot_until(&client, session_id, |snapshot| {
        snapshot.pending_approvals.len() == 6
    })
    .await;
    assert_eq!(snapshot.turns[0].status, TurnStatus::Active);
    let start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "thread/start")
        .unwrap();
    assert_eq!(start["params"]["approvalPolicy"], "untrusted");
    assert_eq!(start["params"]["sandbox"], "read-only");

    let approvals = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval {
                approval,
                tool_activity_id,
                ..
            } => Some((
                approval.reason.clone().unwrap(),
                (approval.clone(), *tool_activity_id),
            )),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let (command, command_link) = &approvals["verify workspace"];
    assert!(command_link.is_some());
    assert_eq!(
        command.subject,
        ApprovalSubject::Command {
            command: "cargo check --tests".into(),
            cwd: Some("project/client".into()),
            actions: vec![
                CommandAction::Read {
                    command: "cat Cargo.toml".into(),
                    name: "Cargo.toml".into(),
                    path: "project/Cargo.toml".into(),
                },
                CommandAction::ListFiles {
                    command: "rg --files src".into(),
                    path: Some("src".into()),
                },
                CommandAction::Search {
                    command: "rg Approval src".into(),
                    query: Some("Approval".into()),
                    path: Some("src".into()),
                },
                CommandAction::Unknown {
                    command: "custom-tool".into()
                },
            ],
        }
    );
    assert_eq!(
        approvals["apply generated files"].0.subject,
        ApprovalSubject::FileChange {
            paths: vec!["old.rs".into(), "new.rs".into(), "added.rs".into()],
            grant_root: Some("generated".into()),
        }
    );
    assert_eq!(
        approvals["connect to registry"].0.subject,
        ApprovalSubject::Network {
            host_or_url: "https://registry.example.test".into(),
        }
    );
    assert_eq!(
        approvals["download metadata"].0.subject,
        ApprovalSubject::PermissionGrant {
            profile: json!({
                "network": {"enabled": true},
                "fileSystem": {"read": ["cache"], "write": ["generated"]},
            }),
        }
    );
    for callback in ["elicitation", "dynamic-tool"] {
        let response = fixture
            .requests()
            .into_iter()
            .find(|message| message["id"] == callback)
            .expect("unsupported callback receives an error");
        assert_eq!(response["error"]["code"], -32000);
    }

    for (reason, decision) in [
        ("verify workspace", Decision::Accept),
        ("repeat callback", Decision::Decline),
        ("connect to registry", Decision::DeclineAndInterrupt),
        ("apply generated files", Decision::AcceptForSession),
        ("download metadata", Decision::Accept),
    ] {
        client
            .submit_decision(session_id, approvals[reason].0.id, decision)
            .await
            .unwrap();
    }
    client
        .submit_decision(
            session_id,
            approvals["stop instead"].0.id,
            Decision::DeclineAndInterrupt,
        )
        .await
        .unwrap();

    let settled = snapshot_until(&client, session_id, |snapshot| {
        snapshot.turns[0].status == TurnStatus::Interrupted
    })
    .await;
    for (_, (approval, _)) in approvals {
        assert!(settled.activities.iter().any(|activity| matches!(
            activity,
            Activity::Approval { approval: found, outcome: ApprovalOutcome::Decided, .. }
                if found.id == approval.id
        )));
    }
    let responses = fixture.requests();
    let result = |id: &str| -> Value {
        responses
            .iter()
            .find(|message| message["id"] == id && message.get("result").is_some())
            .unwrap_or_else(|| panic!("response for {id}: {responses:?}"))["result"]
            .clone()
    };
    assert_eq!(result("command-accept"), json!({"decision":"accept"}));
    assert_eq!(result("command-decline"), json!({"decision":"decline"}));
    assert_eq!(result("network-decline"), json!({"decision":"cancel"}));
    assert_eq!(
        result("file-session"),
        json!({"decision":"acceptForSession"})
    );
    assert_eq!(
        result("permissions-accept"),
        json!({
            "permissions": {
                "network": {"enabled": true},
                "fileSystem": {"read": ["cache"], "write": ["generated"]},
            },
            "scope": "turn",
        })
    );
    assert_eq!(
        result("permissions-cancel"),
        json!({"permissions": {}, "scope": "turn"})
    );
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| method.as_str() == "turn/interrupt")
            .count(),
        1
    );

    server.shutdown().await.unwrap();
}
