//! `answer_questionnaire`: a Sidekick answering a Questionnaire that waits in
//! another Session on its own Server, so work blocked on a simple choice can
//! go on. It is the very act a Client performs when it submits an Answer,
//! through the same operation, so the Session's Turn continues exactly as it
//! would for the user's own, and every refusal is the one a Client meets.
//!
//! An Answer a Sidekick gave says so as typed data naming the Sidekick's
//! Session, in what the Session API answers, what it streams to every Client,
//! and what it keeps across a restart; a Client cannot claim that author for
//! an Answer of its own. No Sidekick answers a Questionnaire in a Session of
//! the Sidekick Workspace, and none decides an Approval (ADR 0043).
//!
//! Each test acts as the MCP client a Sidekick's harness is and asserts on
//! what the Tool answers it and on what the Session API and the Questionnaire's
//! Provider observe after.

mod outcomes;
mod secrets;

use suru::protocol::{
    Answer, Approval, ApprovalId, ApprovalSubject, Author, QuestionAnswer, Questionnaire,
    QuestionnaireId, QuestionnaireOutcome, QuestionnaireSubmission, SessionChange,
};
use suru::questionnaire::{Question, QuestionChoice};

use super::*;
use crate::attachments::{next_change, watch_session};
use crate::broker::sidekick_acts::{
    SIDEKICK_WORKSPACE_REFUSAL, acted, refused, refused_over_http, sidekick_author,
};

/// A choice a Question offers, by its id and label.
fn offered(id: &str, label: &str) -> QuestionChoice {
    QuestionChoice {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        recommended: false,
    }
}

/// A Questionnaire of two Questions: where the tests should run, one of two
/// machines with a note beside it if wanted, which must be answered; and
/// anything else to say, in free text, which may be left unanswered.
fn where_to_run() -> Questionnaire {
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![
            Question {
                id: "machine".to_owned(),
                title: Some("Machine".to_owned()),
                text: "Where should the tests run?".to_owned(),
                choices: vec![
                    offered("staging", "Staging"),
                    offered("local", "This machine"),
                ],
                multiple: false,
                freeform: true,
                combine_freeform: true,
                secret: false,
                required: true,
            },
            Question {
                id: "notes".to_owned(),
                title: None,
                text: "Anything else to keep in mind?".to_owned(),
                choices: Vec::new(),
                multiple: false,
                freeform: true,
                combine_freeform: false,
                secret: false,
                required: false,
            },
        ],
    }
}

/// Has `provider` ask `questionnaire` in its working Turn, and waits until
/// `session_id` holds it waiting on an Answer.
async fn ask(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
    questionnaire: &Questionnaire,
) {
    provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_session_until(
        &reqwest::Client::new(),
        descriptor,
        session_id,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            stood(snapshot, questionnaire.id)
                .is_some_and(|(outcome, ..)| outcome == QuestionnaireOutcome::Pending)
        },
    )
    .await;
}

/// How the Questionnaire `id` stands in `snapshot`: its outcome, the Answer
/// recorded for it, and who gave that Answer on the user's behalf.
fn stood(
    snapshot: &SessionSnapshot,
    id: QuestionnaireId,
) -> Option<(QuestionnaireOutcome, Option<Answer>, Option<Author>)> {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Questionnaire {
                questionnaire,
                outcome,
                answer,
                author,
                ..
            } if questionnaire.id == id => Some((*outcome, answer.clone(), author.clone())),
            _ => None,
        })
}

/// A Client's submission of `submission` to the Questionnaire `id`, as the
/// answering panel sends one.
async fn client_answers(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    id: QuestionnaireId,
    submission: &Value,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/questionnaires/{id}",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(submission)
        .send()
        .await
        .expect("send the submission")
}

/// The user's own Answer to [`where_to_run`] — this machine, and nothing more
/// to say — as the answering panel submits it.
fn users_own_answer() -> Value {
    json!({
        "kind": "answer",
        "answer": { "questions": [
            { "kind": "selected", "choices": ["local"] },
            { "kind": "omitted" },
        ] },
    })
}

/// The user answers the Questionnaire `id` from a Client with
/// [`users_own_answer`], and `provider` takes the Answer.
async fn user_answers(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    id: QuestionnaireId,
    provider: &mut ControlledProviderSession,
) {
    let submission = users_own_answer();
    let (submitted, _) = tokio::join!(
        client_answers(descriptor, session_id, id, &submission),
        async {
            timeout(PROGRESS_DEADLINE, provider.next_questionnaire_submission())
                .await
                .expect("the user's Answer reaches the Agent")
        },
    );
    assert!(
        submitted.status().is_success(),
        "the user's Answer is taken: {}",
        submitted.status()
    );
}

/// The Questionnaires a reading of `session_id` gives as waiting on an Answer.
async fn waiting(client: &mut McpClient, session_id: SessionId) -> Value {
    let result = client
        .call_tool("read_session", json!({ "session_id": session_id }))
        .await;
    assert_ne!(
        result["isError"],
        json!(true),
        "read_session answers: {result}"
    );
    result["structuredContent"]["questionnaires"].clone()
}

#[tokio::test]
async fn a_sidekick_answers_the_questionnaire_read_session_gave_and_the_turn_goes_on() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-answers").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    let (_, mut watched) = watch_session(&descriptor, asking).await;

    let open = waiting(&mut sidekick, asking).await;
    assert_eq!(open[0]["id"], json!(questionnaire.id));
    let tools = listed_tools(&mut sidekick).await;
    assert!(
        tools.contains(&"answer_questionnaire".to_owned()),
        "a Sidekick is offered answer_questionnaire: {tools:?}"
    );

    let (answered, delivered) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": open[0]["id"],
                "answers": [{ "choices": ["staging"], "text": "after the backup" }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Questionnaire's Provider")
        },
    );
    assert_eq!(
        answered,
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answered": true,
        }),
        "the Tool answers once the Agent has the Answer"
    );
    let expected = Answer {
        questions: vec![
            QuestionAnswer::SelectedWithFreeform {
                choices: vec!["staging".to_owned()],
                text: "after the backup".to_owned(),
            },
            QuestionAnswer::Omitted,
        ],
    };
    assert_eq!(
        delivered,
        (
            questionnaire.id,
            QuestionnaireSubmission::Answer {
                answer: expected.clone()
            }
        ),
        "the Agent is handed the Answer as a Client's would be, one per Question"
    );

    let settled = next_change(&mut watched, "the Questionnaire is answered", |change| {
        matches!(
            change,
            SessionChange::QuestionnaireSettled {
                outcome: QuestionnaireOutcome::Answered,
                ..
            }
        )
    })
    .await;
    let SessionChange::QuestionnaireSettled { answer, author, .. } = settled else {
        unreachable!("the change was found as a settlement");
    };
    assert_eq!(
        (answer, author),
        (Some(expected.clone()), Some(sidekick_author(sidekick_id))),
        "every Client watching the Session is sent the Answer naming the Sidekick"
    );

    // The Turn that asked goes on as it would for the user's own Answer.
    write_agent_message(&asking_provider, "Running them on staging.").await;
    asking_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, asking, TurnStatus::Completed).await;
    let snapshot = read_session(&descriptor, asking).await;
    assert_eq!(
        snapshot.turns.len(),
        1,
        "the Answer began no Turn of its own"
    );
    assert!(
        snapshot.messages.iter().any(|message| {
            message.content == "Running them on staging." && message.turn_id == snapshot.turns[0].id
        }),
        "the Agent's work after the Answer stands in the Turn that asked"
    );
    assert_eq!(
        stood(&snapshot, questionnaire.id),
        Some((
            QuestionnaireOutcome::Answered,
            Some(expected.clone()),
            Some(sidekick_author(sidekick_id)),
        )),
        "a Client reading the Session afterwards is given the Answer naming the Sidekick"
    );
    assert_eq!(waiting(&mut sidekick, asking).await, json!([]));
    let reading = sidekick
        .call_tool(
            "read_session",
            json!({ "session_id": asking, "detail": "activities" }),
        )
        .await;
    let transcript = reading["structuredContent"]["transcript"]
        .as_str()
        .unwrap_or_else(|| panic!("the reading holds a transcript: {reading}"));
    assert!(
        transcript.contains(&format!(
            "questionnaire [answered by Sidekick \"Plan the work\" (Session {sidekick_id})]"
        )),
        "a read says the Answer was a Sidekick's: {transcript}"
    );

    drop((asking_provider, watched));
    server.shutdown().await.expect("stop the server");
    let (server, _claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-answers").await;
    let restored = read_session(&server.descriptor().clone(), asking).await;
    assert_eq!(
        stood(&restored, questionnaire.id),
        Some((
            QuestionnaireOutcome::Answered,
            Some(expected),
            Some(sidekick_author(sidekick_id)),
        )),
        "the Answer keeps its author across a restart"
    );

    server.shutdown().await.expect("shut down server");
}

/// A secret Question's Answer reaches the Agent whole and is kept only as
/// answered, as a Client's is, and an optional Question may be left out of
/// nothing but its own Answer.
#[tokio::test]
async fn a_sidekicks_secret_answer_reaches_the_agent_and_is_kept_only_as_answered() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answers-secret",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Deploy it.").await;
    let questionnaire = Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "token".to_owned(),
            title: None,
            text: "Which deploy token?".to_owned(),
            choices: Vec::new(),
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: true,
            required: true,
        }],
    };
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;

    let (_, delivered) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "text": "tok-123" }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Questionnaire's Provider")
        },
    );
    assert_eq!(
        delivered.1,
        QuestionnaireSubmission::Answer {
            answer: Answer {
                questions: vec![QuestionAnswer::Freeform {
                    text: "tok-123".to_owned()
                }],
            },
        },
        "the Agent is handed the secret itself"
    );
    let snapshot = read_session(&descriptor, asking).await;
    assert_eq!(
        stood(&snapshot, questionnaire.id),
        Some((
            QuestionnaireOutcome::Answered,
            Some(Answer {
                questions: vec![QuestionAnswer::SecretAnswered],
            }),
            Some(sidekick_author(sidekick_id)),
        )),
        "Suru keeps only that the secret was answered, and by whom"
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .expect("encode the Session")
            .contains("tok-123"),
        "the secret stands nowhere in what a Client reads"
    );

    server.shutdown().await.expect("shut down server");
}

/// Every refusal is the Tool's own error in a sentence the Sidekick can
/// relay, and where a Client could send the same Answer it is refused in the
/// same words.
#[tokio::test]
async fn an_answer_a_questionnaire_cannot_take_is_refused_saying_why() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-refusals",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    let answering = |answers: Value| {
        json!({
            "session_id": asking,
            "questionnaire_id": questionnaire.id,
            "answers": answers,
        })
    };

    for (answers, says) in [
        (
            json!([{ "choices": ["staging"] }]),
            "The Questionnaire asks 2 Questions and takes one Answer for each, in their order, \
             but 1 was given.",
        ),
        (
            json!([{ "choices": ["remote"] }, {}]),
            "Question 1 (\"machine\") does not offer the choice given; its choices are \
             \"staging\" and \"local\".",
        ),
        (
            json!([{ "choices": ["staging", "local"] }, {}]),
            "Question 1 (\"machine\") takes one choice, but 2 were given.",
        ),
        (
            json!([{}, {}]),
            "Question 1 (\"machine\") is required, so it cannot be left unanswered.",
        ),
        (
            json!([{ "choices": ["staging"] }, { "choices": ["staging"] }]),
            "Question 2 (\"notes\") offers no choices, but was given 1.",
        ),
    ] {
        assert_eq!(
            refused(
                &mut sidekick,
                "answer_questionnaire",
                answering(answers.clone())
            )
            .await,
            says,
            "{answers}"
        );
    }
    assert_eq!(
        refused_over_http(
            client_answers(
                &descriptor,
                asking,
                questionnaire.id,
                &json!({
                    "kind": "answer",
                    "answer": { "questions": [
                        { "kind": "selected", "choices": ["remote"] },
                        { "kind": "omitted" },
                    ] },
                }),
            )
            .await
        )
        .await,
        "Question 1 (\"machine\") does not offer the choice given; its choices are \"staging\" \
         and \"local\".",
        "a Client's Answer is refused in the words a Sidekick's is"
    );
    assert!(
        asking_provider.try_next_questionnaire_delivery().is_none(),
        "no refused Answer reaches the Agent"
    );

    let unknown = QuestionnaireId::new();
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": unknown,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        )
        .await,
        "The Session holds no Questionnaire with that id, so there is nothing to answer.",
    );
    let missing = SessionId::new();
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": missing,
                "questionnaire_id": questionnaire.id,
                "answers": [],
            }),
        )
        .await,
        "The Session does not exist on this Suru server.",
    );
    let response =
        client_answers(&descriptor, missing, questionnaire.id, &users_own_answer()).await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a Client is refused as every act on a missing Session refuses it"
    );
    let refusal = response
        .json::<suru::protocol::SessionError>()
        .await
        .expect("decode the Session error");
    assert_eq!(
        (refusal.code, refusal.message.as_str()),
        (
            suru::protocol::SessionErrorCode::SessionNotFound,
            "The Session does not exist on this Suru server."
        )
    );

    // Answered once, by the user, it takes no other Answer from anyone.
    user_answers(&descriptor, asking, questionnaire.id, &mut asking_provider).await;
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            answering(json!([{ "choices": ["staging"] }, {}])),
        )
        .await,
        "The Questionnaire has already been answered, so it takes no other Answer.",
    );
    let snapshot = read_session(&descriptor, asking).await;
    assert_eq!(
        stood(&snapshot, questionnaire.id).map(|(outcome, _, author)| (outcome, author)),
        Some((QuestionnaireOutcome::Answered, None)),
        "the user's Answer stands, naming no one"
    );

    for (arguments, says) in [
        (
            json!({ "session_id": asking, "answers": [] }),
            "answer_questionnaire needs `questionnaire_id`, the id read_session gives the \
             Questionnaire.",
        ),
        (
            json!({ "session_id": asking, "questionnaire_id": "the first one", "answers": [] }),
            "answer_questionnaire's `questionnaire_id` must be a Questionnaire's id as \
             read_session gives it, and what was given is not one.",
        ),
        (
            json!({ "session_id": asking, "questionnaire_id": questionnaire.id }),
            "answer_questionnaire needs `answers`, one Answer for each of the Questionnaire's \
             Questions, in their order.",
        ),
        (
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choice": "staging" }],
            }),
            "Answer 1 in `answers` names something other than `choices` and `text`, which are \
             all an Answer takes.",
        ),
        (
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [],
                "decision": "accept",
            }),
            "answer_questionnaire takes only `session_id`, `questionnaire_id` and `answers`, \
             and was given an argument besides them.",
        ),
    ] {
        assert_eq!(
            refused(&mut sidekick, "answer_questionnaire", arguments.clone()).await,
            says,
            "{arguments}"
        );
    }

    server.shutdown().await.expect("shut down server");
}

/// An Answer the Agent's Provider refuses leaves the Questionnaire waiting,
/// and the Sidekick is told it may answer again.
#[tokio::test]
async fn an_answer_the_provider_refuses_leaves_the_questionnaire_open_saying_so() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-rejected",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    asking_provider.gate_questionnaire_deliveries();

    let (refusal, ()) = tokio::join!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Answer reaches the Questionnaire's Provider")
            .reject();
        },
    );
    assert_eq!(
        refusal,
        "The Answer was not delivered: the Agent's Provider refused it, and the Questionnaire \
         still waits on one, so it may be answered again."
    );
    assert_eq!(
        waiting(&mut sidekick, asking).await[0]["id"],
        json!(questionnaire.id),
        "the Questionnaire is still read as waiting"
    );

    server.shutdown().await.expect("shut down server");
}

/// No Sidekick answers a Questionnaire in a Session of the Sidekick
/// Workspace, its own included; the user answers one as any other.
#[tokio::test]
async fn no_sidekick_answers_a_questionnaire_in_a_session_of_the_sidekick_workspace() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-workspace",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (own, mut sidekick, own_provider) = start_sidekick(&descriptor, &mut claude).await;
    let (other, _other_client, mut other_provider) = start_sidekick(&descriptor, &mut claude).await;
    let asked_of_own = where_to_run();
    let asked_of_other = where_to_run();
    ask(&descriptor, own, &own_provider, &asked_of_own).await;
    ask(&descriptor, other, &other_provider, &asked_of_other).await;

    for (target, questionnaire) in [(own, &asked_of_own), (other, &asked_of_other)] {
        assert_eq!(
            refused(
                &mut sidekick,
                "answer_questionnaire",
                json!({
                    "session_id": target,
                    "questionnaire_id": questionnaire.id,
                    "answers": [{ "choices": ["staging"] }, {}],
                }),
            )
            .await,
            SIDEKICK_WORKSPACE_REFUSAL,
        );
        assert_eq!(
            stood(&read_session(&descriptor, target).await, questionnaire.id)
                .map(|(outcome, ..)| outcome),
            Some(QuestionnaireOutcome::Pending),
            "the Questionnaire still waits on the user"
        );
    }
    assert!(
        other_provider.try_next_questionnaire_delivery().is_none(),
        "the other Sidekick's Agent is handed no Answer"
    );

    // The user answers a Sidekick's Questionnaire as any other.
    user_answers(&descriptor, other, asked_of_other.id, &mut other_provider).await;

    server.shutdown().await.expect("shut down server");
}

/// A Client's submission carries no author: the Session API takes none from
/// a Client, so the user's Answer can never be passed off as a Sidekick's,
/// nor a Sidekick's claimed by a Client.
#[tokio::test]
async fn a_client_cannot_name_an_author_for_its_answer() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-forged",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, _sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asking, mut asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&descriptor, asking, &asking_provider, &questionnaire).await;
    let answer = users_own_answer()["answer"].clone();
    let author = serde_json::to_value(sidekick_author(sidekick_id)).expect("encode an author");

    for forged in [
        json!({ "kind": "answer", "answer": answer, "author": author }),
        json!({ "kind": "answer", "answer": { "questions": answer["questions"], "author": author } }),
    ] {
        let response = client_answers(&descriptor, asking, questionnaire.id, &forged).await;
        assert!(
            response.status().is_client_error(),
            "a submission naming an author is refused: {forged} → {}",
            response.status()
        );
    }
    assert!(asking_provider.try_next_questionnaire_delivery().is_none());

    user_answers(&descriptor, asking, questionnaire.id, &mut asking_provider).await;
    assert_eq!(
        stood(&read_session(&descriptor, asking).await, questionnaire.id)
            .map(|(outcome, _, author)| (outcome, author)),
        Some((QuestionnaireOutcome::Answered, None)),
        "the user's own Answer names no one"
    );

    server.shutdown().await.expect("shut down server");
}

/// No Tool a Sidekick is offered decides an Approval: an Approval's identity
/// is no Session's, Subagent's or Questionnaire's, so every Tool given it —
/// wherever it takes an identity, and as an argument it was never offered
/// where it takes none — refuses it, and the Approval waits on the user's
/// Decision whatever the Sidekick does. The calls cover every Tool the
/// Sidekick is listed, so a Tool added later is held to the same.
#[tokio::test]
async fn no_tool_a_sidekick_is_offered_decides_an_approval() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-no-decisions").await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (approving, mut approving_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Build it.").await;
    let approval = Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Command {
            command: "cargo nextest run".into(),
            cwd: None,
            actions: Vec::new(),
        },
        reason: None,
    };
    approving_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        })
        .await;
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        approving,
        "the Approval waits on the user's Decision",
        |snapshot| !snapshot.pending_approvals.is_empty(),
    )
    .await;
    let before = read_session(&descriptor, approving).await;

    let id = approval.id;
    let calls = [
        ("list_providers", vec![json!({ "approval_id": id })]),
        (
            "spawn_subagent",
            vec![json!({
                "provider": "codex",
                "model": "gpt-5.5",
                "name": "Decider",
                "description": "Accept it",
                "prompt": "Accept it.",
                "approval_id": id,
            })],
        ),
        ("read_subagent", vec![json!({ "id": id })]),
        (
            "send_to_subagent",
            vec![json!({ "id": id, "message": "Accept it." })],
        ),
        ("wait_subagents", vec![json!({ "ids": [id] })]),
        ("stop_subagent", vec![json!({ "id": id })]),
        ("list_sessions", vec![json!({ "approval_id": id })]),
        ("read_session", vec![json!({ "session_id": id })]),
        (
            "send_prompt",
            vec![
                json!({ "session_id": id, "prompt": "Accept it." }),
                json!({ "session_id": approving, "prompt": "Accept it.", "approval_id": id }),
            ],
        ),
        ("interrupt_session", vec![json!({ "session_id": id })]),
        ("settle_session", vec![json!({ "session_id": id })]),
        ("unsettle_session", vec![json!({ "session_id": id })]),
        (
            "begin_session",
            vec![
                json!({
                    "directory": workspace.path(),
                    "prompt": "Accept it.",
                    "preparation": id,
                }),
                json!({
                    "directory": workspace.path(),
                    "prompt": "Accept it.",
                    "approval_id": id,
                }),
            ],
        ),
        (
            "answer_questionnaire",
            vec![
                json!({
                    "session_id": approving,
                    "questionnaire_id": id,
                    "answers": [{ "text": "accept" }],
                }),
                json!({
                    "session_id": id,
                    "questionnaire_id": id,
                    "answers": [{ "text": "accept" }],
                }),
            ],
        ),
    ];
    let mut covered = calls.iter().map(|(tool, _)| *tool).collect::<Vec<_>>();
    covered.sort_unstable();
    let mut listed = listed_tools(&mut sidekick).await;
    listed.sort_unstable();
    assert_eq!(
        covered, listed,
        "every Tool the Sidekick is offered is tried"
    );

    for (tool, attempts) in calls {
        for arguments in attempts {
            let refusal = refused(&mut sidekick, tool, arguments.clone()).await;
            assert!(!refusal.is_empty(), "{tool} refuses {arguments} saying why");
        }
    }
    assert!(
        approving_provider.try_next_decision().is_none(),
        "no Decision reaches the Agent"
    );
    let after = read_session(&descriptor, approving).await;
    assert_eq!(
        after.pending_approvals,
        vec![approval.id],
        "the Approval still waits on the user's Decision"
    );
    assert_eq!(
        (after.activities, after.messages, after.prompts),
        (before.activities, before.messages, before.prompts),
        "nothing reached the Session the Approval waits in"
    );

    server.shutdown().await.expect("shut down server");
}

/// A brokered Subagent `spawner` spawns on Codex, at work on its first Turn
/// and asking [`where_to_run`]: its Session, its Provider double's view of it,
/// and what it asks.
async fn asking_subagent(
    descriptor: &RuntimeDescriptor,
    codex: &mut ControlledProvider,
    spawner: &mut McpClient,
) -> (SessionId, ControlledProviderSession, Questionnaire) {
    let subagent = spawner
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around.",
        }))
        .await;
    let mut provider = next_start(codex).await.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: default_selection(&codex_models()),
    });
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Subagent's Turn reaches its Provider")
        .succeed();
    let questionnaire = where_to_run();
    ask(descriptor, subagent, &provider, &questionnaire).await;
    (subagent, provider, questionnaire)
}

/// A Subagent's Questionnaire is answered in the Subagent's own Session, as a
/// Client answers it there — unless the Subagent works beneath a Sidekick, in
/// the Sidekick Workspace, where no Sidekick acts.
#[tokio::test]
async fn a_subagents_questionnaire_is_answered_in_its_own_session_unless_a_sidekick_spawned_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-answers-subagent", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_parent, parent_handoff, _parent_provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;
    let mut parent = McpClient::handed(&parent_handoff);
    parent.initialize().await;

    let (subagent, mut subagent_provider, questionnaire) =
        asking_subagent(&descriptor, &mut hosted.codex, &mut parent).await;
    let (answered, delivered) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": subagent,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                subagent_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Subagent's Provider")
        },
    );
    assert_eq!(answered["answered"], json!(true));
    assert_eq!(delivered.0, questionnaire.id);
    assert_eq!(
        stood(&read_session(&descriptor, subagent).await, questionnaire.id)
            .map(|(outcome, _, author)| (outcome, author)),
        Some((
            QuestionnaireOutcome::Answered,
            Some(sidekick_author(sidekick_id))
        )),
        "the Subagent's Questionnaire stands answered by the Sidekick"
    );

    let (beneath, mut beneath_provider, asked) =
        asking_subagent(&descriptor, &mut hosted.codex, &mut sidekick).await;
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": beneath,
                "questionnaire_id": asked.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        )
        .await,
        SIDEKICK_WORKSPACE_REFUSAL,
        "a Subagent working in the Sidekick Workspace is one of its Sessions"
    );
    assert!(beneath_provider.try_next_questionnaire_delivery().is_none());

    hosted.server.shutdown().await.expect("shut down server");
}
