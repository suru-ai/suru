use crate::support::{LiveTurn, ScriptedCopilot, conversation_arms, send_arm};
use suru::{
    protocol::{Activity, Answer, QuestionAnswer, QuestionnaireOutcome, QuestionnaireSubmission},
    provider::CopilotRuntime,
};
use tokio::time::{Duration, timeout};

pub(super) const ASK: &str = r#"      reply '{"jsonrpc":"2.0","id":9001,"method":"userInput.request","params":{"sessionId":"'"$sid"'","question":"Where should I run?","choices":["Local","Remote"],"allowFreeform":true}}'
"#;
pub(super) const ANSWERED: &str = r#"    *'"id":9001'*)
      event done session.idle '{}'
      ;;
"#;

#[tokio::test]
async fn copilot_native_questionnaire_maps_selected_freeform_and_decline_results() {
    for (name, submission, native) in [
        (
            "copilot-question-selected",
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![QuestionAnswer::Selected {
                        choices: vec!["Remote".into()],
                    }],
                },
            },
            serde_json::json!({"answer":"Remote","wasFreeform":false}),
        ),
        (
            "copilot-question-freeform",
            QuestionnaireSubmission::Answer {
                answer: Answer {
                    questions: vec![QuestionAnswer::Freeform {
                        text: "Use staging".into(),
                    }],
                },
            },
            serde_json::json!({"answer":"Use staging","wasFreeform":true}),
        ),
        (
            "copilot-question-decline",
            QuestionnaireSubmission::Decline,
            serde_json::json!({"noResponse":true}),
        ),
    ] {
        let allow_freeform = name != "copilot-question-selected";
        let has_choices = name != "copilot-question-freeform";
        let mut ask = ASK.to_owned();
        if !allow_freeform {
            ask = ask.replace("\"allowFreeform\":true", "\"allowFreeform\":false");
        }
        if !has_choices {
            ask = ask.replace("\"choices\":[\"Local\",\"Remote\"],", "");
        }
        let fixture = ScriptedCopilot::new(&format!(
            "{}{}{}",
            conversation_arms(),
            send_arm(&ask),
            ANSWERED
        ));
        let mut live = LiveTurn::start(
            CopilotRuntime::new(fixture.executable()),
            name,
            "Choose a target",
        )
        .await;
        let snapshot = live
            .wait_for("native question arrives", |s| {
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
        let Activity::Questionnaire { questionnaire, .. } = snapshot
            .activities
            .iter()
            .find(|a| matches!(a, Activity::Questionnaire { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(
            questionnaire.questions[0].choices.len(),
            if has_choices { 2 } else { 0 }
        );
        assert_eq!(questionnaire.questions[0].freeform, allow_freeform);
        assert!(!questionnaire.questions[0].multiple);
        assert_eq!(
            fixture.wait_for_request("session.create").await["params"]["requestUserInput"],
            true
        );
        live.client
            .submit_questionnaire(live.session_id, questionnaire.id, submission)
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(response) = fixture.requests().into_iter().find(|r| r["id"] == 9001) {
                    break response;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(response["result"], native);
        live.wait_for("native response lets Turn settle", |s| {
            s.turns[0].status == suru::protocol::TurnStatus::Completed
        })
        .await;
        live.shutdown().await;
    }
}

#[tokio::test]
async fn interrupting_a_native_questionnaire_stops_work_and_disables_its_answer() {
    let fixture = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(ASK),
        crate::support::abort_arm("      event interrupted session.idle '{\"aborted\":true}'\n")
    ));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(fixture.executable()),
        "copilot-question-interrupt",
        "Ask before work",
    )
    .await;
    let snapshot = live
        .wait_for("pending Questionnaire", |s| {
            s.activities
                .iter()
                .any(|a| matches!(a, Activity::Questionnaire { .. }))
        })
        .await;
    let id = snapshot
        .activities
        .iter()
        .find_map(|a| match a {
            Activity::Questionnaire { questionnaire, .. } => Some(questionnaire.id),
            _ => None,
        })
        .unwrap();
    live.client
        .interrupt_session(live.session_id)
        .await
        .unwrap();
    let snapshot = live
        .wait_for("owning Turn interrupted", |s| {
            s.turns[0].status == suru::protocol::TurnStatus::Interrupted
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
            .submit_questionnaire(live.session_id, id, QuestionnaireSubmission::Decline)
            .await
            .is_err()
    );
    assert!(
        fixture
            .methods()
            .iter()
            .any(|method| method == "session.abort")
    );
    live.shutdown().await;
}
