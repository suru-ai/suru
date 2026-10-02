//! Native app-server question correlation, supported answers, and secret exclusion.
use crate::server_support::PROGRESS_DEADLINE;
use crate::support::ScriptedCodex;
use serde_json::{Value, json};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent, SessionSubscription},
    protocol::{
        Activity, Answer, ApprovalOutcome, ApprovalSubject, CreateSessionRequest, Decision,
        InitialPrompt, PromptId, QuestionAnswer, Questionnaire, QuestionnaireOutcome,
        QuestionnaireSubmission, SessionId, SessionSnapshot, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

const PREFIX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"config/read"'*) printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}' ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}' ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
"#;
fn question_request(id: Value, questions: Value) -> String {
    format!(
        "      printf '%s\\n' '{}'\n",
        json!({"id":id,"method":"item/tool/requestUserInput","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"question-item","questions":questions,"isBlocking":false,"autoResolutionMs":1}})
    )
}
fn script(request: &str, after_request: &str, responses: &str) -> String {
    format!("{PREFIX}{request}{after_request}\n      ;;\n{responses}  esac\ndone\n")
}
fn questions() -> Value {
    json!([
        {"id":"target","header":"Environment","question":"Which target?","isOther":true,"isSecret":false,"options":[{"label":"Local (Recommended)","description":"This machine"},{"label":"Remote","description":"Another machine"}]},
        {"id":"token","header":"Credential","question":"Access token?","isSecret":true,"options":null},
        {"id":"optional","header":"Optional","question":"Any note?","options":[]}
    ])
}
struct Live {
    state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    id: SessionId,
}
impl Live {
    async fn start(fixture: &ScriptedCodex) -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let server = server::spawn_with_provider(
            ServerConfig::new(state.path(), "codex-questionnaires").unwrap(),
            Arc::new(CodexRuntime::new(fixture.executable())),
        )
        .await
        .unwrap();
        let client = ManagedClient::connect(
            ManagedClientConfig::new(state.path(), "codex-questionnaires").unwrap(),
        )
        .await
        .unwrap();
        let created = client
            .create_session(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                agent_selection: None,
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Ask structured questions".into(),
                    skill_invocations: vec![],
                    attachments: Vec::new(),
                },
            })
            .await
            .unwrap();
        let id = created.session.id;
        let feed = client.subscribe_session(id).await.unwrap();
        Self {
            state,
            _workspace: workspace,
            server,
            client,
            feed,
            id,
        }
    }
    async fn until(&mut self, predicate: impl Fn(&SessionSnapshot) -> bool) -> SessionSnapshot {
        timeout(PROGRESS_DEADLINE, async {
            loop {
                let snapshot = self.client.read_session(self.id).await.unwrap();
                if predicate(&snapshot) {
                    return snapshot;
                }
                self.feed.next().await.unwrap().unwrap();
            }
        })
        .await
        .expect("expected native Questionnaire state reaches the Client")
    }
    async fn pending(&mut self) -> Questionnaire {
        self.until(|s| {
            s.activities.iter().any(|a| {
                matches!(
                    a,
                    Activity::Questionnaire {
                        outcome: QuestionnaireOutcome::Pending,
                        ..
                    }
                )
            })
        })
        .await
        .activities
        .into_iter()
        .find_map(|a| match a {
            Activity::Questionnaire {
                questionnaire,
                outcome: QuestionnaireOutcome::Pending,
                ..
            } => Some(questionnaire),
            _ => None,
        })
        .unwrap()
    }
    async fn shutdown(self) {
        drop(self.feed);
        drop(self.client);
        self.server.shutdown().await.unwrap();
    }
}
async fn response(fixture: &ScriptedCodex, id: Value) -> Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(response) = fixture
                .requests()
                .into_iter()
                .find(|r| r["id"] == id && r.get("result").is_some())
            {
                return response["result"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Codex receives the correlated response")
}

#[tokio::test]
async fn codex_batch_maps_single_choice_notes_native_other_and_omission_without_a_countdown() {
    let fixture = ScriptedCodex::new(&script(
        &question_request(json!("batch-17"), questions()),
        "",
        "",
    ));
    let mut live = Live::start(&fixture).await;
    let questionnaire = live.pending().await;
    let target = &questionnaire.questions[0];
    assert_eq!(target.id, "target");
    assert_eq!(target.title.as_deref(), Some("Environment"));
    assert_eq!(
        target.choices[0].description.as_deref(),
        Some("This machine")
    );
    assert!(target.choices[0].recommended);
    assert!(!target.multiple && target.combine_freeform && target.freeform && !target.required);
    assert_eq!(target.choices.last().unwrap().label, "None of the above");
    assert!(questionnaire.questions[1].secret);
    // Native arrays contain a single selection and optional note, never multiple choices.
    let invalid = Answer {
        questions: vec![
            QuestionAnswer::Selected {
                choices: vec!["0".into(), "1".into()],
            },
            QuestionAnswer::Omitted,
            QuestionAnswer::Omitted,
        ],
    };
    assert!(
        live.client
            .submit_questionnaire(
                live.id,
                questionnaire.id,
                QuestionnaireSubmission::Answer { answer: invalid }
            )
            .await
            .is_err()
    );
    let answer = Answer {
        questions: vec![
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["other".into()],
                text: "  Use a container  ".into(),
            },
            QuestionAnswer::Omitted,
            QuestionAnswer::Freeform {
                text: "  Keep output short  ".into(),
            },
        ],
    };
    live.client
        .submit_questionnaire(
            live.id,
            questionnaire.id,
            QuestionnaireSubmission::Answer { answer },
        )
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("batch-17")).await,
        json!({"answers":{"target":{"answers":["None of the above","user_note: Use a container"]},"token":{"answers":[]},"optional":{"answers":["user_note: Keep output short"]}}})
    );
    let snapshot = live.client.read_session(live.id).await.unwrap();
    assert!(
        snapshot
            .turns
            .iter()
            .any(|t| t.status == TurnStatus::Active)
    );
    assert!(
        !fixture
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn codex_decline_is_an_empty_native_answer_and_withdrawal_prevents_late_delivery() {
    let request = question_request(json!(42), questions());
    let next = question_request(json!("withdraw-me"), questions());
    let responses = format!(
        r#"    *'"id":42,"result"'*)
{next}      printf '%s\n' '{{"method":"serverRequest/resolved","params":{{"threadId":"other-thread","requestId":"withdraw-me"}}}}'
      printf '%s\n' '{{"method":"serverRequest/resolved","params":{{"threadId":"native-thread","requestId":"withdraw-me"}}}}'
      ;;
"#
    );
    let fixture = ScriptedCodex::new(&script(&request, "", &responses));
    let mut live = Live::start(&fixture).await;
    let questionnaire = live.pending().await;
    live.client
        .submit_questionnaire(live.id, questionnaire.id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    assert_eq!(response(&fixture, json!(42)).await, json!({"answers":{}}));
    let snapshot = live
        .until(|s| {
            s.activities.iter().any(|a| {
                matches!(
                    a,
                    Activity::Questionnaire {
                        outcome: QuestionnaireOutcome::Withdrawn,
                        ..
                    }
                )
            })
        })
        .await;
    let withdrawn = snapshot
        .activities
        .iter()
        .find_map(|a| match a {
            Activity::Questionnaire {
                questionnaire,
                outcome: QuestionnaireOutcome::Withdrawn,
                ..
            } => Some(questionnaire.id),
            _ => None,
        })
        .unwrap();
    assert!(
        live.client
            .submit_questionnaire(live.id, withdrawn, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    assert!(
        snapshot
            .turns
            .iter()
            .any(|turn| turn.status == TurnStatus::Active)
    );
    assert!(!fixture.requests().iter().any(|r| r["id"] == "withdraw-me"));
    assert!(
        !fixture
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn codex_secret_reaches_native_callback_but_not_streamed_errors_logs_or_restored_history() {
    const SECRET: &str = "suru-private-token-9f67af";
    let diagnostics = tempfile::tempdir().unwrap();
    let log_path = diagnostics.path().join("suru.log");
    let log = Arc::new(std::fs::File::create(&log_path).unwrap());
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(log)
            .finish(),
    )
    .unwrap();
    let (left, right) = SECRET.split_at(10);
    let content_events = [
        json!({"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"echo-message","text":""}}}),
        json!({"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"echo-command","command":"echo output","status":"inProgress"}}}),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"echo-message","delta":format!("token: {left}")}}),
        json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"echo-command","delta":format!("token: {left}")}}),
        json!({"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"echo-message","delta":format!("{right} done")}}),
        json!({"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"echo-command","delta":format!("{right} done")}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"echo-message","text":format!("token: {SECRET} done")}}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"echo-command","command":"echo output","status":"completed","aggregatedOutput":format!("token: {SECRET} done"),"exitCode":0}}}),
        json!({"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"echo-tool","server":"notes","tool":"find","status":"inProgress","arguments":{"text":SECRET},"result":null,"error":null}}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"echo-tool","server":"notes","tool":"find","status":"failed","arguments":{"text":SECRET},"result":{"content":[{"type":"text","text":format!("token: {SECRET} done")}]},"error":{"message":SECRET}}}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"echo-search","query":SECRET,"action":{"type":"findInPage","url":format!("https://example.test/{SECRET}"),"pattern":SECRET}}}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageGeneration","id":"echo-image","status":"completed","revisedPrompt":SECRET,"result":"","savedPath":format!("/tmp/{SECRET}.png")}}}),
        json!({"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"echo-path","changes":[{"path":format!("/tmp/{SECRET}.txt"),"kind":{"type":"add"},"diff":SECRET}],"status":"completed"}}}),
    ].iter().map(|event| format!("      printf '%s\\n' '{event}'\n")).collect::<String>();
    let echo_request = question_request(
        json!("echo-option"),
        json!([{
            "id":"echo", "header":SECRET, "question":SECRET,
            "options":[{"label":SECRET,"description":SECRET}]
        }]),
    );
    let permission_request = json!({
        "id": "secret-permission",
        "method": "item/permissions/requestApproval",
        "params": {
            "threadId": "native-thread",
            "turnId": "native-turn",
            "itemId": "secret-tool",
            "startedAtMs": 2,
            "cwd": "project",
            "reason": format!("read {SECRET}"),
            "permissions": {"fileSystem": {"read": [format!("cache/{SECRET}")]}}
        }
    });
    let responses = format!(
        r#"    *'"id":"secret-request","result"'*)
      printf '%s\n' "$line" >&2
      printf '%s\n' '{permission_request}'
{echo_request}      ;;
    *'"id":"echo-option","result"'*)
      printf '%s\n' "$line" >&2
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.005; done
{content_events}      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"native-turn","status":"failed","error":{{"message":"Native rejection echoed {SECRET}","additionalDetails":"{SECRET}"}},"items":[]}}}}}}'
      ;;
"#
    );
    let mut secret_questions = questions();
    secret_questions[2]["isSecret"] = json!(true);
    let fixture = ScriptedCodex::new(&script(
        &question_request(json!("secret-request"), secret_questions),
        "",
        &responses,
    ));
    let mut live = Live::start(&fixture).await;
    let questionnaire = live.pending().await;
    // Observe every post-submission update independently, including the terminal error.
    let mut observer = live.client.subscribe_session(live.id).await.unwrap();
    observer.next().await.unwrap().unwrap();
    live.client
        .submit_questionnaire(
            live.id,
            questionnaire.id,
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![
                        QuestionAnswer::Omitted,
                        QuestionAnswer::Freeform {
                            text: SECRET.into(),
                        },
                        QuestionAnswer::Freeform { text: left.into() },
                    ],
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("secret-request")).await["answers"]["token"],
        json!({"answers":[format!("user_note: {SECRET}")]})
    );
    let approval_snapshot = live
        .until(|snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Approval {
                        outcome: ApprovalOutcome::Pending,
                        ..
                    }
                )
            })
        })
        .await;
    let approval = approval_snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval {
                approval,
                outcome: ApprovalOutcome::Pending,
                ..
            } => Some(approval.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(approval.reason.as_deref(), Some("read [redacted]"));
    assert_eq!(
        approval.subject,
        ApprovalSubject::PermissionGrant {
            profile: json!({"fileSystem": {"read": ["cache/[redacted]"]}}),
        }
    );
    live.client
        .submit_decision(live.id, approval.id, Decision::Accept)
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("secret-permission")).await,
        json!({
            "permissions": {"fileSystem": {"read": [format!("cache/{SECRET}")]}},
            "scope": "turn",
        }),
        "redaction never mutates the Provider-owned grant payload"
    );
    let echo = live.pending().await;
    assert!(!serde_json::to_string(&echo).unwrap().contains(SECRET));
    assert_eq!(echo.questions[0].choices[0].label, "[redacted]");
    live.client
        .submit_questionnaire(
            live.id,
            echo.id,
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![QuestionAnswer::Selected {
                        choices: vec![echo.questions[0].choices[0].id.clone()],
                    }],
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("echo-option")).await,
        json!({"answers":{"echo":{"answers":[SECRET]}}})
    );
    fixture.release();
    let snapshot = live
        .until(|s| s.turns.iter().any(|t| t.status == TurnStatus::Failed))
        .await;
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains(SECRET) && !serialized.contains(left));
    assert!(serialized.contains("secret_answered") && serialized.contains("[redacted]"));
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.content == "token: [redacted] done")
    );
    assert!(snapshot.activities.iter().any(|activity| matches!(activity, Activity::Command { output, .. } if output == "token: [redacted] done")));
    assert!(snapshot.activities.iter().any(|activity| matches!(activity, Activity::ToolCall { input, output, .. } if input == "text=[redacted]" && output == "token: [redacted] done\n[redacted]")));
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = observer.next().await.unwrap().unwrap();
            let updated = match event {
                SessionEvent::Updated(update) => update,
                _ => continue,
            };
            let serialized = serde_json::to_string(&updated).unwrap();
            assert!(
                !serialized.contains(SECRET),
                "secret entered a Client update"
            );
            if updated.changes.iter().any(|change| {
                matches!(
                    change,
                    suru::protocol::SessionChange::TurnStatusChanged {
                        status: TurnStatus::Failed,
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
    drop(observer);
    let Live {
        state,
        _workspace,
        server,
        client,
        feed,
        id,
    } = live;
    drop(feed);
    drop(client);
    server.shutdown().await.unwrap();
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "codex-questionnaires").unwrap(),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .unwrap();
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "codex-questionnaires").unwrap(),
    )
    .await
    .unwrap();
    let restored = client.read_session(id).await.unwrap();
    assert!(!serde_json::to_string(&restored).unwrap().contains(SECRET));
    assert!(restored.activities.iter().any(|a| matches!(a, Activity::Questionnaire { answer: Some(answer), .. } if answer.questions.get(1) == Some(&QuestionAnswer::SecretAnswered))));
    drop(client);
    server.shutdown().await.unwrap();
    fn check_files(path: &std::path::Path, secret: &[u8]) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                check_files(&path, secret);
            } else {
                let bytes = std::fs::read(path).unwrap();
                assert!(
                    !bytes.windows(secret.len()).any(|part| part == secret),
                    "secret entered Suru persistence"
                );
            }
        }
    }
    check_files(state.path(), SECRET.as_bytes());
    let logs = std::fs::read_to_string(log_path).unwrap();
    assert!(
        !logs.contains(SECRET) && !logs.contains(left),
        "secret entered Suru diagnostics"
    );
    assert!(logs.contains("drained native stderr"));
}

/// A secret the Agent then hands an MCP Tool spelled every way its arguments can spell it — as a
/// number, as a key, nested as both, and within a string — never reaches the Tool Call's input,
/// which renders the arguments afresh rather than repeating the text Codex sent.
#[tokio::test]
async fn codex_secret_never_reaches_a_tool_calls_input_however_its_arguments_spell_it() {
    const SECRET: &str = "90210417";
    let arguments = json!({
        "code": 90210417,
        SECRET: "as a key",
        "nested": {SECRET: 90210417},
        "note": format!("pin {SECRET}"),
    });
    let tool_call = |stage: &str, status: &str| json!({"method":format!("item/{stage}"),"params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"secret-tool","server":"vault","tool":"unlock","status":status,"arguments":arguments,"result":(status == "completed").then(|| json!({"content":[{"type":"text","text":"Unlocked."}]})),"error":null}}});
    let responses = format!(
        r#"    *'"id":"numeric-secret","result"'*)
      printf '%s\n' '{}'
      printf '%s\n' '{}'
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"native-turn","status":"completed","items":[]}}}}}}'
      ;;
"#,
        tool_call("started", "inProgress"),
        tool_call("completed", "completed"),
    );
    let fixture = ScriptedCodex::new(&script(
        &question_request(
            json!("numeric-secret"),
            json!([{"id":"code","header":"Code","question":"One-time code?","isSecret":true,"options":null}]),
        ),
        "",
        &responses,
    ));
    let mut live = Live::start(&fixture).await;
    let questionnaire = live.pending().await;
    live.client
        .submit_questionnaire(
            live.id,
            questionnaire.id,
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![QuestionAnswer::Freeform {
                        text: SECRET.into(),
                    }],
                },
            },
        )
        .await
        .unwrap();
    let snapshot = live
        .until(|s| s.turns.iter().any(|t| t.status == TurnStatus::Completed))
        .await;
    let inputs = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::ToolCall { input, output, .. } => Some((input.as_str(), output.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        inputs,
        [(
            r#"[redacted]=as a key code=[redacted] nested={"[redacted]":[redacted]} note=pin [redacted]"#,
            "Unlocked.",
        )],
        "every spelling of the secret in the rendered input is redacted"
    );
    live.shutdown().await;
}

/// The Broker's own Tools are known by the server and name Codex reports a call under, before
/// anything Codex sent is redacted: a Session that answered its own secret Questions with the
/// Broker's server name and a Broker Tool's name still has its later `answer_questionnaire` call
/// recorded with every Answer withheld, rather than mistaken for another server's Tool whose
/// arguments stand as given — which would put a secret it was never told of in its Transcript.
#[tokio::test]
async fn codex_a_broker_call_is_known_by_its_own_name_whatever_secret_spells_that_name() {
    const UNTOLD: &str = "tok-untold-5ecret";
    let arguments = json!({
        "session_id": "0198b27e-3a01-7c4c-a83b-a83a4787453f",
        "questionnaire_id": "0198b27e-4b02-7c4c-a83b-a83a4787453f",
        "answers": [{ "text": UNTOLD }],
    });
    let tool_call = |stage: &str, status: &str| json!({"method":format!("item/{stage}"),"params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"answer-call","server":"suru","tool":"answer_questionnaire","status":status,"arguments":arguments,"result":(status == "completed").then(|| json!({"content":[{"type":"text","text":"answered"}]})),"error":null}}});
    let responses = format!(
        r#"    *'"id":"names-as-secrets","result"'*)
      printf '%s\n' '{}'
      printf '%s\n' '{}'
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"native-turn","status":"completed","items":[]}}}}}}'
      ;;
"#,
        tool_call("started", "inProgress"),
        tool_call("completed", "completed"),
    );
    let fixture = ScriptedCodex::new(&script(
        &question_request(
            json!("names-as-secrets"),
            json!([
                {"id":"server","header":"Server","question":"Which server?","isSecret":true,"options":null},
                {"id":"tool","header":"Tool","question":"Which tool?","isSecret":true,"options":null}
            ]),
        ),
        "",
        &responses,
    ));
    let mut live = Live::start(&fixture).await;
    let questionnaire = live.pending().await;
    live.client
        .submit_questionnaire(
            live.id,
            questionnaire.id,
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![
                        QuestionAnswer::Freeform {
                            text: "suru".into(),
                        },
                        QuestionAnswer::Freeform {
                            text: "answer_questionnaire".into(),
                        },
                    ],
                },
            },
        )
        .await
        .unwrap();
    let snapshot = live
        .until(|s| s.turns.iter().any(|t| t.status == TurnStatus::Completed))
        .await;
    let inputs = snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::ToolCall { input, .. } => Some(input.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        inputs,
        [
            "answers=1 Answer withheld questionnaire_id=0198b27e-4b02-7c4c-a83b-a83a4787453f \
          session_id=0198b27e-3a01-7c4c-a83b-a83a4787453f"
        ],
        "the call is the Broker's, and its Answers are withheld"
    );
    assert!(
        !serde_json::to_string(&snapshot).unwrap().contains(UNTOLD),
        "the secret the call carried stands nowhere in the Session"
    );
    live.shutdown().await;
}

#[tokio::test]
async fn codex_owning_turn_completion_and_explicit_interrupt_end_native_answerability() {
    for interrupt in [false, true] {
        let after_request = if interrupt {
            ""
        } else {
            r#"      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.005; done
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'"#
        };
        let responses = r#"    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":5,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      ;;
"#;
        let fixture = ScriptedCodex::new(&script(
            &question_request(json!("end-request"), questions()),
            after_request,
            responses,
        ));
        let mut live = Live::start(&fixture).await;
        let question = live.pending().await;
        if interrupt {
            live.client.interrupt_session(live.id).await.unwrap();
        } else {
            fixture.release();
        }
        let snapshot = live
            .until(|s| s.turns.iter().any(|turn| turn.status != TurnStatus::Active))
            .await;
        assert!(snapshot.activities.iter().any(|a| matches!(
            a,
            Activity::Questionnaire {
                outcome: QuestionnaireOutcome::TurnEnded,
                ..
            }
        )));
        assert!(
            live.client
                .submit_questionnaire(live.id, question.id, QuestionnaireSubmission::Decline)
                .await
                .is_err()
        );
        assert!(!fixture.requests().iter().any(|r| r["id"] == "end-request"));
        assert_eq!(
            fixture
                .methods()
                .iter()
                .any(|method| method == "turn/interrupt"),
            interrupt
        );
        live.shutdown().await;
    }
}

#[tokio::test]
async fn codex_child_questions_before_and_after_parent_settlement_are_answered_in_the_child() {
    let before = question_request(json!("child-before"), questions())
        .replace("native-thread", "child-thread")
        .replace("native-turn", "child-turn")
        .replace("question-item", "child-before-item");
    let after = question_request(json!("child-after"), questions())
        .replace("native-thread", "child-thread")
        .replace("native-turn", "child-turn")
        .replace("question-item", "child-after-item");
    let responses = format!(
        r#"    *'"method":"thread/resume"'*)
      printf '%s\n' '{{"id":5,"result":{{"thread":{{"id":"child-thread","parentThreadId":"native-thread"}},"model":"gpt-fixture"}}}}'
{before}      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"native-turn","status":"completed","items":[]}}}}}}'
{after}      ;;
    *'"id":"child-after","result"'*)
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.005; done
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"child-thread","turn":{{"id":"child-turn","status":"completed","items":[]}}}}}}'
      printf '%s\n' '{{"method":"item/completed","params":{{"threadId":"native-thread","turnId":"native-turn","item":{{"type":"subAgentActivity","id":"child-ended","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/scout"}}}}}}'
      ;;
"#
    );
    let spawn = r#"      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"subAgentActivity","id":"child-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'"#;
    let program = script("", spawn, &responses).replace(
        "*'\"method\":\"thread/start\"'*|*'\"method\":\"thread/resume\"'*)",
        "*'\"method\":\"thread/start\"'*)",
    );
    let fixture = ScriptedCodex::new(&program);
    let mut live = Live::start(&fixture).await;
    let parent = live
        .until(|snapshot| {
            snapshot
                .turns
                .iter()
                .any(|turn| turn.status == TurnStatus::Completed)
                && snapshot.subagent_questionnaire_count() == 2
        })
        .await;
    assert!(
        !parent
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Questionnaire { .. }))
    );
    let child = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap();
    let mut child_feed = live.client.subscribe_session(child).await.unwrap();
    child_feed.next().await.unwrap().unwrap();
    let child_snapshot = live.client.read_session(child).await.unwrap();
    let pending: Vec<_> = child_snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Questionnaire {
                questionnaire,
                outcome: QuestionnaireOutcome::Pending,
                ..
            } => Some(questionnaire.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(pending.len(), 2);
    assert!(
        live.client
            .submit_questionnaire(live.id, pending[0].id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    live.client
        .submit_questionnaire(
            child,
            pending[0].id,
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![
                        QuestionAnswer::Selected {
                            choices: vec!["1".into()],
                        },
                        QuestionAnswer::Omitted,
                        QuestionAnswer::Omitted,
                    ],
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("child-before")).await["answers"]["target"],
        json!({"answers":["Remote"]})
    );
    live.client
        .submit_questionnaire(child, pending[1].id, QuestionnaireSubmission::Decline)
        .await
        .unwrap();
    assert_eq!(
        response(&fixture, json!("child-after")).await,
        json!({"answers":{}})
    );
    let child_snapshot = live.client.read_session(child).await.unwrap();
    assert!(child_snapshot.activities.iter().any(|activity| matches!(
        activity,
        Activity::Questionnaire {
            outcome: QuestionnaireOutcome::Answered,
            ..
        }
    )));
    assert!(child_snapshot.activities.iter().any(|activity| matches!(
        activity,
        Activity::Questionnaire {
            outcome: QuestionnaireOutcome::Declined,
            ..
        }
    )));
    let parent = live.client.read_session(live.id).await.unwrap();
    assert_eq!(parent.subagent_questionnaire_count(), 0);
    assert_eq!(parent.turns.len(), 1);
    assert_eq!(parent.turns[0].status, TurnStatus::Completed);
    fixture.release();
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if live
                .client
                .read_session(child)
                .await
                .unwrap()
                .turns
                .iter()
                .any(|turn| turn.status == TurnStatus::Completed)
            {
                break;
            }
            child_feed.next().await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        live.client
            .submit_questionnaire(child, pending[0].id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    drop(child_feed);
    live.shutdown().await;
}
