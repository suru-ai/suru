//! How a Questionnaire that no longer waits on an Answer refuses a Sidekick's,
//! whichever way it stopped waiting, and how a Sidekick and the user
//! answering the same Questionnaire leave exactly one Answer, by whoever gave
//! it.
//!
//! The first submission accepted wins (docs/questionnaire-design.md): one
//! arriving while another is on its way to the Agent is refused for that, and
//! one arriving after is refused as already answered. Each refusal is the
//! Tool's own error in a sentence the Sidekick can relay, and reaches no
//! Agent.

use super::*;

/// The Sidekick's Answer to [`where_to_run`]: staging, and nothing more.
fn sidekicks_answer(session_id: SessionId, id: QuestionnaireId) -> Value {
    json!({
        "session_id": session_id,
        "questionnaire_id": id,
        "answers": [{ "choices": ["staging"] }, {}],
    })
}

/// A Client's submission of `submission`, sent on a task of its own so a test
/// may act while it waits on the Agent.
fn client_answers_in_background(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    id: QuestionnaireId,
    submission: Value,
) -> tokio::task::JoinHandle<reqwest::Response> {
    let descriptor = descriptor.clone();
    tokio::spawn(async move { client_answers(&descriptor, session_id, id, &submission).await })
}

/// Waits until the Questionnaire `id` in `session_id` stands as `outcome`.
async fn stands(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    id: QuestionnaireId,
    outcome: QuestionnaireOutcome,
) {
    read_session_until(
        &reqwest::Client::new(),
        descriptor,
        session_id,
        "the Questionnaire stands as expected",
        |snapshot| stood(snapshot, id).is_some_and(|(stood, ..)| stood == outcome),
    )
    .await;
}

/// A Session at work on its first Turn that asks [`where_to_run`].
async fn asking(
    descriptor: &RuntimeDescriptor,
    claude: &mut ControlledProvider,
    workspace: &Path,
    title: &str,
) -> (SessionId, ControlledProviderSession, Questionnaire) {
    let (session_id, provider) = started_session(descriptor, claude, workspace, title).await;
    let questionnaire = where_to_run();
    ask(descriptor, session_id, &provider, &questionnaire).await;
    (session_id, provider, questionnaire)
}

/// Every way a Questionnaire stops waiting on an Answer, short of being
/// answered, refuses the Sidekick saying which, and hands the Agent nothing.
#[tokio::test]
async fn a_questionnaire_no_longer_waiting_refuses_a_sidekick_saying_why() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-closed",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    // Declined by the user.
    let (declined, mut declined_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Declined.").await;
    let decline = json!({ "kind": "decline" });
    let (submitted, _) = tokio::join!(
        client_answers(&descriptor, declined, asked.id, &decline),
        async {
            timeout(
                PROGRESS_DEADLINE,
                declined_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the decline reaches the Agent")
        },
    );
    assert!(submitted.status().is_success(), "{}", submitted.status());
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(declined, asked.id)
        )
        .await,
        "The Questionnaire has already been declined, so it takes no Answer.",
    );
    assert!(
        declined_provider
            .try_next_questionnaire_delivery()
            .is_none()
    );

    // Withdrawn by the Agent that asked.
    let (withdrawn, withdrawn_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Withdrawn.").await;
    withdrawn_provider.emit(ProviderEvent::QuestionnaireWithdrawn { id: asked.id });
    stands(
        &descriptor,
        withdrawn,
        asked.id,
        QuestionnaireOutcome::Withdrawn,
    )
    .await;
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(withdrawn, asked.id)
        )
        .await,
        "The Agent withdrew the Questionnaire, so it takes no Answer.",
    );

    // Its Turn ended while it waited.
    let (ended, mut ended_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Ended.").await;
    ended_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, ended, TurnStatus::Completed).await;
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(ended, asked.id)
        )
        .await,
        "The Turn that asked the Questionnaire has ended, so it takes no Answer.",
    );
    assert!(ended_provider.try_next_questionnaire_delivery().is_none());

    // The user's Answer is on its way to the Agent.
    let (submitting, mut submitting_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Submitting.").await;
    submitting_provider.gate_questionnaire_deliveries();
    let users = client_answers_in_background(&descriptor, submitting, asked.id, users_own_answer());
    let delivery = timeout(
        PROGRESS_DEADLINE,
        submitting_provider.next_questionnaire_delivery(),
    )
    .await
    .expect("the user's Answer reaches the Agent");
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(submitting, asked.id)
        )
        .await,
        "An Answer to the Questionnaire is already on its way to the Agent, so it takes no other.",
    );
    assert!(
        submitting_provider
            .try_next_questionnaire_delivery()
            .is_none()
    );
    delivery.succeed();
    let users = timeout(PROGRESS_DEADLINE, users)
        .await
        .expect("the user's submission is answered")
        .expect("the submission task runs");
    assert!(users.status().is_success(), "{}", users.status());

    // Whether the user's Answer reached the Agent is uncertain.
    let (uncertain, mut uncertain_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Uncertain.").await;
    uncertain_provider.gate_questionnaire_deliveries();
    let users = client_answers_in_background(&descriptor, uncertain, asked.id, users_own_answer());
    drop(
        timeout(
            PROGRESS_DEADLINE,
            uncertain_provider.next_questionnaire_delivery(),
        )
        .await
        .expect("the user's Answer reaches the Agent"),
    );
    timeout(PROGRESS_DEADLINE, users)
        .await
        .expect("the user's submission is answered")
        .expect("the submission task runs");
    stands(
        &descriptor,
        uncertain,
        asked.id,
        QuestionnaireOutcome::DeliveryUncertain,
    )
    .await;
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(uncertain, asked.id)
        )
        .await,
        "Whether an Answer already sent reached the Agent is uncertain, so the Questionnaire \
         takes no other.",
    );
    assert!(
        uncertain_provider
            .try_next_questionnaire_delivery()
            .is_none()
    );

    server.shutdown().await.expect("shut down server");
}

/// A Questionnaire whose Provider request a restart ended is history: it
/// takes no Answer from a Sidekick, as it takes none from a Client.
#[tokio::test]
async fn a_questionnaire_a_restart_left_unavailable_refuses_a_sidekick() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-unavailable",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, _sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (asked_in, asked_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    drop((sidekick_provider, asked_provider));
    server.shutdown().await.expect("stop the server");

    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-unavailable",
    )
    .await;
    let descriptor = server.descriptor().clone();
    admit_prompt(&descriptor, sidekick_id, "Answer it.").await;
    let relaunch = next_start(&mut claude).await;
    let handoff = relaunch
        .broker()
        .cloned()
        .expect("the relaunched Sidekick is handed the Broker");
    let mut relaunched = relaunch.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the Sidekick's Turn reaches its Provider")
        .succeed();
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;

    assert_eq!(
        stood(&read_session(&descriptor, asked_in).await, asked.id).map(|(outcome, ..)| outcome),
        Some(QuestionnaireOutcome::Unavailable),
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(asked_in, asked.id)
        )
        .await,
        "The Questionnaire is no longer live with its Provider, so it takes no Answer.",
    );

    server.shutdown().await.expect("shut down server");
}

/// A Sidekick and the user answering the same Questionnaire at once leave one
/// Answer, recorded as given by whichever was accepted first; the other is
/// refused, saying an Answer is already on its way, and once that Answer has
/// landed, saying the Questionnaire was answered. It goes both ways.
#[tokio::test]
async fn a_sidekick_and_the_user_answering_at_once_leave_one_answer_by_whoever_won() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-answer-race").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let on_its_way =
        "An Answer to the Questionnaire is already on its way to the Agent, so it takes no other.";
    let answered_already =
        "The Questionnaire has already been answered, so it takes no other Answer.";

    // The Sidekick's is accepted first.
    let (first, mut first_provider, asked) = asking(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Sidekick first.",
    )
    .await;
    first_provider.gate_questionnaire_deliveries();
    let (answered, users) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(first, asked.id)
        ),
        async {
            let delivery = timeout(
                PROGRESS_DEADLINE,
                first_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Sidekick's Answer reaches the Agent");
            let users = refused_over_http(
                client_answers(&descriptor, first, asked.id, &users_own_answer()).await,
            )
            .await;
            delivery.succeed();
            users
        },
    );
    assert_eq!(answered["answered"], json!(true));
    assert_eq!(
        users, on_its_way,
        "the user's Answer loses to the one on its way"
    );
    assert_eq!(
        refused_over_http(client_answers(&descriptor, first, asked.id, &users_own_answer()).await)
            .await,
        answered_already,
    );
    assert!(first_provider.try_next_questionnaire_delivery().is_none());
    assert_eq!(
        stood(&read_session(&descriptor, first).await, asked.id),
        Some((
            QuestionnaireOutcome::Answered,
            Some(Answer {
                questions: vec![
                    QuestionAnswer::Selected {
                        choices: vec!["staging".to_owned()]
                    },
                    QuestionAnswer::Omitted,
                ],
            }),
            Some(sidekick_author(sidekick_id)),
        )),
        "the Sidekick's Answer stands, naming the Sidekick"
    );

    // The user's is accepted first.
    let (second, mut second_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "User first.").await;
    second_provider.gate_questionnaire_deliveries();
    let users = client_answers_in_background(&descriptor, second, asked.id, users_own_answer());
    let delivery = timeout(
        PROGRESS_DEADLINE,
        second_provider.next_questionnaire_delivery(),
    )
    .await
    .expect("the user's Answer reaches the Agent");
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(second, asked.id)
        )
        .await,
        on_its_way,
        "the Sidekick's Answer loses to the one on its way"
    );
    delivery.succeed();
    let users = timeout(PROGRESS_DEADLINE, users)
        .await
        .expect("the user's submission is answered")
        .expect("the submission task runs");
    assert!(users.status().is_success(), "{}", users.status());
    assert_eq!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(second, asked.id)
        )
        .await,
        answered_already,
    );
    assert!(second_provider.try_next_questionnaire_delivery().is_none());
    assert_eq!(
        stood(&read_session(&descriptor, second).await, asked.id)
            .map(|(outcome, _, author)| (outcome, author)),
        Some((QuestionnaireOutcome::Answered, None)),
        "the user's Answer stands, naming no one"
    );

    server.shutdown().await.expect("shut down server");
}

/// A Sidekick's attempt that came to nothing — refused before it reached the
/// Agent, or refused by the Agent's Provider — leaves no trace of the
/// Sidekick on the Answer the user then gives.
#[tokio::test]
async fn the_users_answer_after_a_sidekicks_failed_attempt_names_no_one() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-answer-then-user",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    let (refused_first, mut refused_provider, asked) =
        asking(&descriptor, &mut claude, workspace.path(), "Refused first.").await;
    refused(
        &mut sidekick,
        "answer_questionnaire",
        json!({
            "session_id": refused_first,
            "questionnaire_id": asked.id,
            "answers": [{ "choices": ["remote"] }, {}],
        }),
    )
    .await;
    user_answers(&descriptor, refused_first, asked.id, &mut refused_provider).await;
    assert_eq!(
        stood(&read_session(&descriptor, refused_first).await, asked.id)
            .map(|(outcome, _, author)| (outcome, author)),
        Some((QuestionnaireOutcome::Answered, None)),
    );

    let (rejected_first, mut rejected_provider, asked) = asking(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Rejected first.",
    )
    .await;
    rejected_provider.gate_questionnaire_deliveries();
    let (refusal, ()) = tokio::join!(
        refused(
            &mut sidekick,
            "answer_questionnaire",
            sidekicks_answer(rejected_first, asked.id)
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                rejected_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Sidekick's Answer reaches the Agent")
            .reject();
        },
    );
    assert!(
        refusal.starts_with("The Answer was not delivered"),
        "{refusal}"
    );
    rejected_provider.ungate_questionnaire_deliveries();
    user_answers(
        &descriptor,
        rejected_first,
        asked.id,
        &mut rejected_provider,
    )
    .await;
    assert_eq!(
        stood(&read_session(&descriptor, rejected_first).await, asked.id),
        Some((
            QuestionnaireOutcome::Answered,
            Some(Answer {
                questions: vec![
                    QuestionAnswer::Selected {
                        choices: vec!["local".to_owned()]
                    },
                    QuestionAnswer::Omitted,
                ],
            }),
            None,
        )),
        "the user's Answer stands, naming no one"
    );

    server.shutdown().await.expect("shut down server");
}
