//! Native stdio AskUserQuestion callbacks remain Questionnaires under Claude's permission mode.
use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    CLAUDE_MODELS, LiveTurn, ScriptedClaude, discovery_arms, flag_value, user_turn_arm,
};
use serde_json::{Value, json};
use suru::{
    protocol::{
        Activity, Answer, QuestionAnswer, Questionnaire, QuestionnaireOutcome,
        QuestionnaireSubmission, TurnStatus,
    },
    provider::ClaudeRuntime,
};
use tokio::time::timeout;

fn input() -> Value {
    json!({"questions":[
        {"question":"Which environment?","header":"Environment","options":[{"label":"Local (Recommended)","description":"On this machine"},{"label":"Remote","description":"A separate machine"}],"multiSelect":false},
        {"question":"Which checks?","header":"Checks","options":[{"label":"Unit","description":"Fast checks"},{"label":"Integration","description":"Across boundaries"}],"multiSelect":true}
    ], "metadata":{"source":"native"}})
}
fn ask(id: &str) -> String {
    format!(
        "      emit '{}'\n",
        json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"question-tool","input":input()}})
    )
}
async fn pending(live: &mut LiveTurn) -> Questionnaire {
    let snapshot = live
        .wait_for("Questionnaire reaches Clients", |s| {
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
        .await;
    snapshot
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
async fn native_response(fixture: &ScriptedClaude, id: &str) -> Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(value) = fixture
                .requests()
                .into_iter()
                .find(|r| r["type"] == "control_response" && r["response"]["request_id"] == id)
            {
                return value["response"]["response"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native correlated response arrives")
}
const COMPLETED: &str = r#"    *'"type":"control_response"'*)
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Thanks","terminal_reason":"completed","session_id":"prov-session"}'
      ;;
"#;

#[tokio::test]
async fn claude_batches_preserve_metadata_and_deliver_multiple_choices_with_custom_text() {
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&ask("batch-17")),
        COMPLETED
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-question-batch",
        "Ask for the environment and checks",
    )
    .await;
    let questionnaire = pending(&mut live).await;
    assert_eq!(questionnaire.questions.len(), 2);
    assert_eq!(
        questionnaire.questions[0].title.as_deref(),
        Some("Environment")
    );
    assert_eq!(
        questionnaire.questions[0].choices[0].description.as_deref(),
        Some("On this machine")
    );
    assert!(questionnaire.questions[0].choices[0].recommended);
    assert!(!questionnaire.questions[0].multiple && !questionnaire.questions[0].combine_freeform);
    assert!(questionnaire.questions[1].multiple && questionnaire.questions[1].combine_freeform);
    assert!(
        questionnaire
            .questions
            .iter()
            .all(|q| q.required && q.freeform)
    );
    let launch = fixture.launch_carrying("--session-id");
    assert!(!launch.carries("--dangerously-skip-permissions"));
    assert_eq!(launch.value("--permission-mode"), "default");
    assert_eq!(
        flag_value(&launch.arguments, "--permission-prompt-tool"),
        "stdio"
    );
    let invalid = Answer {
        questions: vec![
            QuestionAnswer::Selected {
                choices: vec!["Remote".into()],
            },
            QuestionAnswer::Omitted,
        ],
    };
    assert!(
        live.client
            .submit_questionnaire(
                live.session_id,
                questionnaire.id,
                QuestionnaireSubmission::Answer { answer: invalid }
            )
            .await
            .is_err()
    );
    let answer = Answer {
        questions: vec![
            QuestionAnswer::Selected {
                choices: vec!["Remote".into()],
            },
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["Unit".into(), "Integration".into()],
                text: "Lint".into(),
            },
        ],
    };
    live.client
        .submit_questionnaire(
            live.session_id,
            questionnaire.id,
            QuestionnaireSubmission::Answer {
                answer: answer.clone(),
            },
        )
        .await
        .unwrap();
    let native = native_response(&fixture, "batch-17").await;
    let mut expected = input();
    expected["answers"] =
        json!({"Which environment?":"Remote", "Which checks?":"Unit, Integration, Lint"});
    assert_eq!(native, json!({"behavior":"allow", "updatedInput":expected}));
    let snapshot = live
        .wait_for("Turn settles after Answer", |s| {
            s.turns[0].status == TurnStatus::Completed
        })
        .await;
    assert!(snapshot.activities.iter().any(|a| matches!(a, Activity::Questionnaire { answer: Some(stored), outcome: QuestionnaireOutcome::Answered, .. } if stored == &answer)));
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|m| m.role == suru::protocol::MessageRole::User)
            .count(),
        1
    );
    live.shutdown().await;
}

#[tokio::test]
async fn claude_decline_is_a_correlated_refusal_and_does_not_interrupt_the_turn() {
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&ask("decline-42"))
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-question-decline",
        "Ask for input",
    )
    .await;
    let questionnaire = pending(&mut live).await;
    live.client
        .submit_questionnaire(
            live.session_id,
            questionnaire.id,
            QuestionnaireSubmission::Decline,
        )
        .await
        .unwrap();
    let native = native_response(&fixture, "decline-42").await;
    assert_eq!(native["behavior"], "deny");
    assert_eq!(native["interrupt"], false);
    assert!(native["updatedInput"].is_null());
    let snapshot = live.client.read_session(live.session_id).await.unwrap();
    assert_eq!(snapshot.turns[0].status, TurnStatus::Active);
    assert!(
        !fixture
            .control_subtypes()
            .iter()
            .any(|subtype| subtype == "interrupt")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn claude_cancellation_withdraws_the_questionnaire_without_a_native_response() {
    let timeline = format!(
        "{}      (\n        while [ ! -e \"$CLAUDE_FIXTURE_RELEASE\" ]; do sleep 0.01; done\n        emit '{{\"type\":\"control_cancel_request\",\"request_id\":\"withdraw-9\"}}'\n      ) &\n",
        ask("withdraw-9")
    );
    let fixture = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&timeline)
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-question-withdraw",
        "Ask for input",
    )
    .await;
    let questionnaire = pending(&mut live).await;
    fixture.release();
    live.wait_for("withdrawal disables the pending request", |s| {
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
    assert!(
        live.client
            .submit_questionnaire(
                live.session_id,
                questionnaire.id,
                QuestionnaireSubmission::Decline
            )
            .await
            .is_err()
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r["type"] == "control_response")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn interrupting_claude_while_a_questionnaire_is_pending_stops_the_turn_without_answering() {
    let interrupted = "      emit '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"terminal_reason\":\"aborted_tools\",\"session_id\":\"prov-session\"}'\n";
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&ask("interrupt-51")),
        crate::support::interrupt_arm(interrupted)
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-question-interrupt",
        "Ask for input",
    )
    .await;
    let questionnaire = pending(&mut live).await;
    live.client
        .interrupt_session(live.session_id)
        .await
        .unwrap();
    let snapshot = live
        .wait_for("owning Turn interrupted", |s| {
            s.turns[0].status == TurnStatus::Interrupted
        })
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
            .submit_questionnaire(
                live.session_id,
                questionnaire.id,
                QuestionnaireSubmission::Decline
            )
            .await
            .is_err()
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r["type"] == "control_response")
    );
    live.shutdown().await;
}

#[tokio::test]
async fn native_child_questionnaire_is_correlated_to_its_tool_call_and_survives_parent_result() {
    let question_input = input();
    let child_message = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"child-question-tool","name":"AskUserQuestion","input":question_input}]},"parent_tool_use_id":"spawn-child","session_id":"prov-session"});
    let request = json!({"type":"control_request","request_id":"child-question","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"child-question-tool","agent_id":"native-agent-42","input":question_input}});
    let native = format!(
        r#"      emit '{{"type":"system","subtype":"task_started","task_id":"task-child","tool_use_id":"spawn-child","task_type":"local_agent","description":"Choose child settings"}}'
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
        user_turn_arm(&native),
        crate::support::stop_task_arm()
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-child-question",
        "Ask in a Subagent",
    )
    .await;
    fixture.release();
    let parent = live
        .wait_for("child question survives owning result", |s| {
            s.turns[0].status == TurnStatus::Completed && s.subagent_questionnaire_count() == 1
        })
        .await;
    assert!(
        !parent
            .activities
            .iter()
            .any(|a| matches!(a, Activity::Questionnaire { .. }))
    );
    let child_id = parent.subagent_interventions[0].session_id;
    let child = live.client.read_session(child_id).await.unwrap();
    let question = child
        .activities
        .iter()
        .find_map(|a| match a {
            Activity::Questionnaire { questionnaire, .. } => Some(questionnaire),
            _ => None,
        })
        .unwrap();
    let answer = suru::protocol::Answer {
        questions: vec![
            QuestionAnswer::Freeform {
                text: "Child environment".into(),
            },
            QuestionAnswer::Selected {
                choices: vec!["Unit".into()],
            },
        ],
    };
    live.client
        .submit_questionnaire(
            child_id,
            question.id,
            QuestionnaireSubmission::Answer { answer },
        )
        .await
        .unwrap();
    let response = native_response(&fixture, "child-question").await;
    assert_eq!(
        response["updatedInput"]["answers"]["Which environment?"],
        "Child environment"
    );
    assert_eq!(
        live.client
            .read_session(live.session_id)
            .await
            .unwrap()
            .subagent_questionnaire_count(),
        0
    );
    assert_eq!(live.client.list_sessions(None).await.unwrap().len(), 1);
    live.shutdown().await;
}

#[tokio::test]
async fn an_unidentified_native_child_question_is_declined_without_entering_parent_history() {
    let request = json!({"type":"control_request","request_id":"unknown-child","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"unobserved-tool","agent_id":"unknown-native-agent","input":input()}});
    let fixture = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!(
            r#"      (
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{request}'
      ) &
"#
        )),
        COMPLETED
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(fixture.executable()),
        "claude-unidentified-question",
        "Use a Subagent",
    )
    .await;
    fixture.release();
    let response = native_response(&fixture, "unknown-child").await;
    assert_eq!(response["behavior"], "deny");
    assert_eq!(response["interrupt"], false);
    let settled = live
        .wait_for("parent completes normally", |s| {
            s.turns[0].status == TurnStatus::Completed
        })
        .await;
    assert!(
        !settled
            .activities
            .iter()
            .any(|a| matches!(a, Activity::Questionnaire { .. }))
    );
    live.shutdown().await;
}
