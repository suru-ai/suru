//! A secret Answer goes to the Agent that asked for it and nowhere else in
//! Suru (docs/questionnaire-design.md, ADR 0019). A Sidekick's call of
//! `answer_questionnaire` is a Tool Call in the Sidekick's own Transcript, so
//! what that Tool Call records of the call's arguments is withheld — every
//! Answer alike, since only the Questionnaire knows which are secret — and no
//! refusal, the Broker's or the Session API's, repeats what it was given.
//!
//! Each test drives the Sidekick's Provider double as a harness records the
//! call — its arguments as the Agent sent them, its output as the Broker
//! answered — and asserts on everything a reader of either Session can see.

use futures_util::StreamExt;
use suru::provider::{ProviderActivityId, ProviderToolCallStatus, ToolCallInput};

use super::*;

/// What the user gave the Sidekick to answer with, which no Transcript keeps.
const SECRET: &str = "tok-5ecret-1234";

/// A Questionnaire asking for a deploy token, which is secret, and a region,
/// which is not; both must be answered.
fn deploy_token() -> Questionnaire {
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![
            Question {
                id: "token".to_owned(),
                title: None,
                text: "Which deploy token?".to_owned(),
                choices: Vec::new(),
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: true,
                required: true,
            },
            Question {
                id: "region".to_owned(),
                title: None,
                text: "Which region?".to_owned(),
                choices: vec![offered("eu", "Europe"), offered("us", "America")],
                multiple: false,
                freeform: false,
                combine_freeform: false,
                secret: false,
                required: true,
            },
        ],
    }
}

/// Everything `snapshot` holds, as a Client is sent it.
fn everything(snapshot: &SessionSnapshot) -> String {
    serde_json::to_string(snapshot).expect("encode the Session")
}

/// What the Broker answered a call with, as a harness reads it out of the
/// call's result to record as its Tool Call's output.
fn answered_with(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the Broker answers in words: {result}"))
        .to_owned()
}

/// The input a harness's projection records a call of `answer_questionnaire`
/// with: whatever the Provider, it is made from the call's arguments and the
/// Broker's Tool they were given to, as every Provider's is.
fn recorded_input(arguments: &Value) -> ToolCallInput {
    ToolCallInput::of(Some("suru"), "answer_questionnaire", arguments)
}

/// Has the Sidekick's Provider record one call of `answer_questionnaire` as a
/// harness does — opened as `opened_with` says, its whole arguments given,
/// then the Broker's answer as its output — while `sidekick` makes the call.
/// Answers with the Broker's answer.
async fn recorded_call(
    sidekick: &mut McpClient,
    sidekick_provider: &ControlledProviderSession,
    native_id: &str,
    arguments: Value,
    opened_with_arguments: bool,
) -> Value {
    let activity_id = ProviderActivityId::new(native_id);
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::ToolCallStarted {
            activity_id: activity_id.clone(),
            name: "answer_questionnaire".to_owned(),
            server: Some("suru".to_owned()),
            input: opened_with_arguments.then(|| recorded_input(&arguments)),
        })
        .await;
    if !opened_with_arguments {
        // As Claude streams a tool use's input and knows it whole only once
        // the block closes.
        sidekick_provider
            .emit_and_wait_until_observed(ProviderEvent::ToolCallInputKnown {
                activity_id: activity_id.clone(),
                input: recorded_input(&arguments),
            })
            .await;
    }
    let result = sidekick.call_tool("answer_questionnaire", arguments).await;
    let is_error = result["isError"] == json!(true);
    for event in [
        ProviderEvent::ToolCallOutputDelta {
            activity_id: activity_id.clone(),
            content: answered_with(&result),
        },
        ProviderEvent::ToolCallCompleted {
            activity_id,
            status: if is_error {
                ProviderToolCallStatus::Failed
            } else {
                ProviderToolCallStatus::Completed
            },
            omitted_parts: 0,
        },
    ] {
        sidekick_provider.emit_and_wait_until_observed(event).await;
    }
    result
}

/// Every update `watched` streams until one carries a change `settles`
/// matches, each as a Client is sent it.
async fn streamed_until(
    watched: &mut (impl futures_util::Stream<Item = suru::protocol::SessionUpdate> + Unpin),
    settles: impl Fn(&SessionChange) -> bool,
) -> Vec<String> {
    timeout(PROGRESS_DEADLINE, async {
        let mut streamed = Vec::new();
        loop {
            let update = watched.next().await.expect("the Session stream stays open");
            let done = update.changes.iter().any(&settles);
            streamed.push(serde_json::to_string(&update).expect("encode the update"));
            if done {
                return streamed;
            }
        }
    })
    .await
    .expect("the Session's stream reaches what the test waits for")
}

/// The Tool Calls `snapshot` records, as `(input, output)`.
fn tool_calls(snapshot: &SessionSnapshot) -> Vec<(String, String)> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::ToolCall { input, output, .. } => Some((input.clone(), output.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_secret_answer_stands_nowhere_in_the_sidekicks_own_transcript() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-secret-kept").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (_reader_id, mut reader, _reader_provider) = start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Deploy it.").await;
    let questionnaire = deploy_token();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    let (_, mut watched_sidekick) = watch_session(&descriptor, sidekick_id).await;
    let (_, mut watched_asking) = watch_session(&descriptor, asking).await;

    // A malformed call first, opened with its arguments as Codex and
    // Copilot open one, its secret where `choices` belongs.
    let malformed = recorded_call(
        &mut sidekick,
        &sidekick_provider,
        "call-malformed",
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answers": [{ "choices": SECRET }, { "choices": ["eu"] }],
        }),
        true,
    )
    .await;
    assert_eq!(malformed["isError"], json!(true), "{malformed}");
    // Then one carrying its secret as the name of an argument the Tool does
    // not take.
    let misnamed = recorded_call(
        &mut sidekick,
        &sidekick_provider,
        "call-misnamed",
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answers": [{ "text": "tok-1" }, { "choices": ["eu"] }],
            SECRET: true,
        }),
        true,
    )
    .await;
    assert_eq!(misnamed["isError"], json!(true), "{misnamed}");
    // Then the call that answers, its arguments known only later, as Claude
    // knows them.
    let (answered, delivered) = tokio::join!(
        recorded_call(
            &mut sidekick,
            &sidekick_provider,
            "call-answer",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "text": SECRET }, { "choices": ["eu"] }],
            }),
            false,
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Agent that asked")
        },
    );
    assert_ne!(answered["isError"], json!(true), "{answered}");
    assert_eq!(
        delivered.1,
        QuestionnaireSubmission::Answer {
            answer: Answer {
                questions: vec![
                    QuestionAnswer::Freeform {
                        text: SECRET.to_owned()
                    },
                    QuestionAnswer::Selected {
                        choices: vec!["eu".to_owned()]
                    },
                ],
            },
        },
        "the Agent that asked is handed the secret itself"
    );

    let streamed = streamed_until(&mut watched_sidekick, |change| {
        matches!(
            change,
            SessionChange::ToolCallStatusChanged {
                status: ActivityStatus::Completed,
                ..
            }
        )
    })
    .await;
    assert!(
        streamed.iter().all(|update| !update.contains(SECRET)),
        "no Client watching the Sidekick is sent the secret: {streamed:#?}"
    );
    let streamed = streamed_until(&mut watched_asking, |change| {
        matches!(
            change,
            SessionChange::QuestionnaireSettled {
                outcome: QuestionnaireOutcome::Answered,
                ..
            }
        )
    })
    .await;
    assert!(
        streamed.iter().all(|update| !update.contains(SECRET)),
        "no Client watching the Session that asked is sent the secret: {streamed:#?}"
    );

    let snapshot = read_session(&descriptor, sidekick_id).await;
    let withheld = format!(
        "answers=2 Answers withheld questionnaire_id={} session_id={asking}",
        questionnaire.id
    );
    assert_eq!(
        tool_calls(&snapshot),
        [
            (withheld.clone(), answered_with(&malformed)),
            (withheld.clone(), answered_with(&misnamed)),
            (withheld.clone(), answered_with(&answered)),
        ],
        "each call records which Questionnaire it answered and how many Answers it gave, and \
         the Broker's own answer"
    );
    assert!(
        !everything(&snapshot).contains(SECRET),
        "the Sidekick's Session holds the secret nowhere"
    );
    let asked = read_session(&descriptor, asking).await;
    assert_eq!(
        stood(&asked, questionnaire.id).map(|(outcome, answer, _)| (outcome, answer)),
        Some((
            QuestionnaireOutcome::Answered,
            Some(Answer {
                questions: vec![
                    QuestionAnswer::SecretAnswered,
                    QuestionAnswer::Selected {
                        choices: vec!["eu".to_owned()]
                    },
                ],
            }),
        )),
        "the Session that asked keeps only that its secret was answered"
    );
    assert!(!everything(&asked).contains(SECRET));

    for (session_id, items) in [
        (sidekick_id, &["1.2", "1.3", "1.4"][..]),
        (asking, &["1.2"][..]),
    ] {
        let reading = reader
            .call_tool(
                "read_session",
                json!({ "session_id": session_id, "turns": 1, "detail": "activities" }),
            )
            .await;
        assert_ne!(reading["isError"], json!(true), "{reading}");
        assert!(!reading.to_string().contains(SECRET), "{reading}");
        for item in items {
            let whole = reader
                .call_tool(
                    "read_session",
                    json!({ "session_id": session_id, "item": item }),
                )
                .await;
            assert_ne!(whole["isError"], json!(true), "{whole}");
            assert!(!whole.to_string().contains(SECRET), "{item}: {whole}");
        }
    }
    let reading = reader
        .call_tool(
            "read_session",
            json!({ "session_id": sidekick_id, "turns": 1, "detail": "activities" }),
        )
        .await;
    assert!(
        reading["structuredContent"]["transcript"]
            .as_str()
            .is_some_and(|transcript| transcript.contains(&withheld)),
        "another Sidekick reads that the call answered, and how: {reading}"
    );

    drop((
        sidekick_provider,
        asking_provider,
        watched_sidekick,
        watched_asking,
    ));
    server.shutdown().await.expect("stop the server");
    let (server, _claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-secret-kept").await;
    let descriptor = server.descriptor().clone();
    let restored = read_session(&descriptor, sidekick_id).await;
    assert_eq!(
        tool_calls(&restored)
            .into_iter()
            .map(|(input, _)| input)
            .collect::<Vec<_>>(),
        [withheld.clone(), withheld.clone(), withheld],
        "restored, the calls read as they did"
    );
    assert!(
        !everything(&restored).contains(SECRET),
        "nor does the Sidekick's Session hold the secret once restored"
    );
    let restored = read_session(&descriptor, asking).await;
    assert_eq!(
        stood(&restored, questionnaire.id).map(|(_, answer, _)| answer),
        Some(Some(Answer {
            questions: vec![
                QuestionAnswer::SecretAnswered,
                QuestionAnswer::Selected {
                    choices: vec!["eu".to_owned()]
                },
            ],
        })),
    );
    assert!(
        !everything(&restored).contains(SECRET),
        "nor the Session that asked"
    );

    server.shutdown().await.expect("shut down server");
}

/// However a call carrying a secret falls short, the Sidekick is told where
/// and what was expected, never what it sent; and a Client's submission is
/// refused the same way.
#[tokio::test]
async fn no_refusal_repeats_what_it_was_given() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-secret-refused",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Deploy it.").await;
    let questionnaire = deploy_token();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    let answering = |answers: Value| {
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answers": answers,
        })
    };

    for (arguments, says) in [
        (
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "text": "tok-1" }, { "choices": ["eu"] }],
                SECRET: true,
            }),
            "answer_questionnaire takes only `session_id`, `origin`, `questionnaire_id` and \
             `answers`, and was given an argument besides them.",
        ),
        (
            json!({
                "session_id": SECRET,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "text": "tok-1" }, { "choices": ["eu"] }],
            }),
            "answer_questionnaire's `session_id` must be the id of a Session as list_sessions \
             gives it, and what was given is not one.",
        ),
        (
            json!({
                "session_id": asking,
                "questionnaire_id": SECRET,
                "answers": [{ "text": SECRET }, { "choices": ["eu"] }],
            }),
            "answer_questionnaire's `questionnaire_id` must be a Questionnaire's id as \
             read_session gives it, and what was given is not one.",
        ),
        (
            answering(json!(SECRET)),
            "answer_questionnaire's `answers` must be a list, one Answer for each of the \
             Questionnaire's Questions, in their order, and it was given a string.",
        ),
        (
            answering(json!([SECRET, { "choices": ["eu"] }])),
            "Answer 1 in `answers` must be an object giving `choices`, `text`, or both, and it \
             was given a string.",
        ),
        (
            answering(json!([{ SECRET: true }, { "choices": ["eu"] }])),
            "Answer 1 in `answers` names something other than `choices` and `text`, which are \
             all an Answer takes.",
        ),
        (
            answering(json!([{ "choices": SECRET }, { "choices": ["eu"] }])),
            "Answer 1's `choices` must be a list of the ids of the Question's choices, as \
             read_session gives them, and it was given a string.",
        ),
        (
            answering(json!([{ "choices": [SECRET, 7] }, { "choices": ["eu"] }])),
            "Answer 1's `choices` must be a list of the ids of the Question's choices, as \
             read_session gives them, and it was given a list holding something other than \
             strings.",
        ),
        (
            answering(json!([{ "text": { "token": SECRET } }, { "choices": ["eu"] }])),
            "Answer 1's `text` must be a string, and it was given an object.",
        ),
        (
            answering(json!([{ "choices": [SECRET] }, { "choices": ["eu"] }])),
            "Question 1 (\"token\") offers no choices, but was given 1.",
        ),
        (
            answering(json!([{ "text": "tok-1" }, { "choices": [SECRET] }])),
            "Question 2 (\"region\") does not offer the choice given; its choices are \"eu\" and \
             \"us\".",
        ),
        (
            answering(json!([{ "text": "tok-1" }, { "choices": ["eu", SECRET] }])),
            "Question 2 (\"region\") takes one choice, but 2 were given.",
        ),
        (
            answering(json!([{ "text": "tok-1" }, { "text": SECRET }])),
            "Question 2 (\"region\") takes no free text.",
        ),
    ] {
        let refusal = refused(&mut sidekick, "answer_questionnaire", arguments.clone()).await;
        assert_eq!(refusal, says, "{arguments}");
        assert!(!refusal.contains(SECRET), "{refusal}");
    }

    for (submission, status, says) in [
        (
            json!({ "kind": "answer", "answer": { "questions": [
                { "kind": "selected", "choices": [SECRET] },
                { "kind": "selected", "choices": ["eu"] },
            ] } }),
            reqwest::StatusCode::CONFLICT,
            "Question 1 (\"token\") offers no choices, but was given 1.",
        ),
        (
            json!({ "kind": "answer", "answer": { "questions": [
                { "kind": "freeform", "text": "tok-1" },
                { "kind": "freeform", "text": SECRET },
            ] } }),
            reqwest::StatusCode::CONFLICT,
            "Question 2 (\"region\") takes no free text.",
        ),
        (
            json!({ "kind": "answer", "answer": { "questions": [
                { "kind": "selected", "choices": SECRET },
                { "kind": "selected", "choices": ["eu"] },
            ] } }),
            reqwest::StatusCode::BAD_REQUEST,
            "Questionnaire submission command is not valid JSON",
        ),
    ] {
        let response = client_answers(&descriptor, asking, questionnaire.id, &submission).await;
        assert_eq!(response.status(), status, "{submission}");
        let body = response.text().await.expect("read the refusal");
        assert!(!body.contains(SECRET), "{body}");
        let refusal = serde_json::from_str::<suru::protocol::SessionError>(&body)
            .expect("decode the Session error");
        assert_eq!(refusal.message, says, "{submission}");
    }

    assert!(
        asking_provider.try_next_questionnaire_delivery().is_none(),
        "nothing refused reaches the Agent"
    );
    assert_eq!(
        stood(&read_session(&descriptor, asking).await, questionnaire.id)
            .map(|(outcome, ..)| outcome),
        Some(QuestionnaireOutcome::Pending),
        "the Questionnaire still waits on an Answer"
    );

    server.shutdown().await.expect("shut down server");
}

/// The Log at its most verbose holds no Answer a Sidekick gave, refused or
/// taken: the Broker's MCP transport, which at its own verbose levels logs
/// every call it receives whole, is held to its warnings and errors whatever
/// `SURU_LOG` asks, while Suru's own lines are written as verbosely as asked.
#[tokio::test]
async fn the_log_at_its_most_verbose_holds_no_answer_a_sidekick_gave() {
    const LOGGED: &str = "tok-l0gged-5ecret";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let channel = "sidekick-secret-logged";
    let config = ServerConfig::new(state_dir.path(), channel).expect("configure the Log");
    let log = suru::logging::init_with_filter_directives(
        &config,
        suru::logging::Role::Server,
        Some("trace,rmcp=trace,rmcp::service=trace".to_owned()),
    )
    .expect("initialize the Log at its most verbose");
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Deploy it.").await;
    let questionnaire = deploy_token();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;

    refused(
        &mut sidekick,
        "answer_questionnaire",
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answers": [{ "choices": LOGGED }, { "choices": ["eu"] }],
        }),
    )
    .await;
    let (answered, _) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "text": LOGGED }, { "choices": ["eu"] }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Agent that asked")
        },
    );
    assert_eq!(answered["answered"], json!(true));
    drop(asking_provider);
    server.shutdown().await.expect("shut down server");
    drop(log);

    let logged = std::fs::read_dir(config.state_dir().join("log"))
        .expect("read the Log directory")
        .map(|entry| std::fs::read_to_string(entry.expect("a Log file").path()).expect("a Log"))
        .collect::<String>();
    assert!(
        logged.contains("Broker Tool called"),
        "Suru's own lines are written as verbosely as asked: {logged}"
    );
    assert!(
        !logged.contains(LOGGED),
        "no Answer reaches the Log, however verbose"
    );
}
