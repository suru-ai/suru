//! How long a Sidekick is owed Reports: for each piece of work it set going —
//! a Prompt it sent, until the Turn that takes it settles; an Answer it gave,
//! once delivered, until the Turn it continued settles; and, through either,
//! the Subagents that Turn spawned, until the whole branch it set going has
//! settled. Work it never set going — a Turn the user begins, a Prompt of its
//! own withdrawn before any Turn took it, an Answer that never reached the
//! Agent — tells it nothing, whatever else it set going in the same Session.

use super::*;

/// Sends `session_id` `text` through `send_prompt` as `delivery` asks,
/// answering how the Session took it.
async fn send(client: &mut McpClient, session_id: SessionId, text: &str, delivery: &str) -> Value {
    acted(
        client,
        "send_prompt",
        json!({ "session_id": session_id, "prompt": text, "delivery": delivery }),
    )
    .await["admitted"]
        .clone()
}

/// Interrupts `session_id` as the user does, and has its Provider take the
/// interrupt and stop its Turn.
async fn user_interrupts(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &mut ControlledProviderSession,
) {
    let (stopped, ()) = tokio::join!(
        reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{session_id}/interrupt",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send(),
        async {
            timeout(PROGRESS_DEADLINE, provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Session's Provider")
                .succeed();
        },
    );
    stopped
        .expect("send the interrupt")
        .error_for_status()
        .expect("the interrupt is taken");
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    latest_turn_settles(descriptor, session_id, TurnStatus::Interrupted).await;
}

/// Has `provider` ask [`where_to_run`] in its working Turn, and waits until
/// `session_id` holds it waiting on an Answer.
async fn asks(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
) -> Questionnaire {
    let questionnaire = where_to_run();
    provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_until(
        descriptor,
        session_id,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Questionnaire {
                        questionnaire: asked,
                        outcome: suru::protocol::QuestionnaireOutcome::Pending,
                        ..
                    } if asked.id == questionnaire.id
                )
            })
        },
    )
    .await;
    questionnaire
}

/// The Sidekick answers `questionnaire` in `session_id` through `client`,
/// answering the words its Answer was refused with.
async fn answer_refused(
    client: &mut McpClient,
    session_id: SessionId,
    questionnaire: &Questionnaire,
) -> String {
    refused(
        client,
        "answer_questionnaire",
        json!({
            "session_id": session_id,
            "questionnaire_id": questionnaire.id,
            "answers": [{ "choices": ["staging"] }],
        }),
    )
    .await
}

/// Has `provider`'s Turn in `session_id` write `text` and complete.
async fn completes(provider: &ControlledProviderSession, text: &str) {
    write_agent_message(provider, text).await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
}

/// The first Report the Sidekick is given from here is of a Session it begins
/// now: proof nothing it was not owed was reported before it.
async fn next_report_is_of_a_session_begun_now(sidekick: &mut Sidekick, why: &str) {
    let descriptor = sidekick.descriptor.clone();
    let (begun, begun_provider) = sidekick.begin().await;
    fixes(&descriptor, begun, &begun_provider).await;
    let report = sidekick
        .steered("the Session the Sidekick began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(begun, ASKED, "completed", FIXED),
        "{why}"
    );
}

#[tokio::test]
async fn work_queued_behind_a_turn_the_sidekick_set_going_is_reported_turn_by_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-queued").await;
    let (begun, mut begun_provider) = sidekick.begin().await;
    assert_eq!(
        send(
            &mut sidekick.client,
            begun,
            "Then the signup test.",
            "queue"
        )
        .await,
        json!("queued")
    );

    completes(&begun_provider, FIXED).await;
    let first = sidekick
        .steered("the Turn the Sidekick began is reported though its next Prompt waits")
        .await;
    assert_eq!(
        untimed_sidekick_report(&first),
        settled_report(begun, ASKED, "completed", FIXED),
        "a Prompt waiting behind it hides nothing of the Turn already settled"
    );

    let turn = timeout(PROGRESS_DEADLINE, begun_provider.next_turn())
        .await
        .expect("the queued Prompt begins the next Turn");
    assert_eq!(turn.prompt(), "Then the signup test.");
    turn.succeed();
    completes(&begun_provider, "The signup test passes too.").await;
    let second = sidekick
        .steered("the Turn the queued Prompt began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&second),
        settled_report(begun, ASKED, "completed", "The signup test passes too.")
    );
    sidekick.handed_nothing("each Turn is reported once");

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_prompt_withdrawn_before_delivery_tells_nothing_of_the_users_turn_it_waited_behind() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-withdrawn").await;
    let descriptor = sidekick.descriptor.clone();
    let (users, _, users_provider) = sidekick.users_session("Run the auth suite.").await;
    assert_eq!(
        send(&mut sidekick.client, users, ASKED, "queue").await,
        json!("queued")
    );
    let prompt = read_session(&descriptor, users)
        .await
        .prompts
        .iter()
        .find(|prompt| prompt.text == ASKED)
        .expect("the Sidekick's Prompt waits in the Session")
        .id;

    // The user withdraws it before any Turn takes it.
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{users}/prompts/{prompt}/cancel",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the cancellation")
        .error_for_status()
        .expect("the Prompt is withdrawn");
    asks(&descriptor, users, &users_provider).await;
    fixes(&descriptor, users, &users_provider).await;
    sidekick.handed_nothing("the user's own Turn is no Sidekick's to hear of");

    next_report_is_of_a_session_begun_now(
        &mut sidekick,
        "neither the Questionnaire nor the settling of the Turn the withdrawn Prompt waited \
         behind was reported",
    )
    .await;

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn an_answer_still_on_its_way_when_its_turn_ends_tells_the_sidekick_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-answer-ended").await;
    let descriptor = sidekick.descriptor.clone();
    let (users, _, mut users_provider) = sidekick.users_session("Run the auth suite.").await;
    let questionnaire = asks(&descriptor, users, &users_provider).await;

    // The Answer is held on its way to the Agent while the user stops the
    // Turn that asked, and never arrives.
    users_provider.gate_questionnaire_deliveries();
    let (refusal, ()) = tokio::join!(
        answer_refused(&mut sidekick.client, users, &questionnaire),
        async {
            let delivery = timeout(
                PROGRESS_DEADLINE,
                users_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Answer reaches the Session's Provider");
            user_interrupts(&descriptor, users, &mut users_provider).await;
            delivery.reject();
        },
    );
    assert!(
        !refusal.is_empty(),
        "the Sidekick is told its Answer did not take"
    );
    sidekick.handed_nothing("an Answer that never arrived gave the Sidekick no hand in the Turn");

    next_report_is_of_a_session_begun_now(
        &mut sidekick,
        "the Turn the undelivered Answer was for was never reported",
    )
    .await;

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_refused_answer_leaves_the_report_owed_for_a_prompt_sent_meanwhile() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-answer-refused").await;
    let descriptor = sidekick.descriptor.clone();
    let (users, _, mut users_provider) = sidekick.users_session("Run the auth suite.").await;
    let questionnaire = asks(&descriptor, users, &users_provider).await;
    let mut alongside = McpClient::handed(&sidekick.handoff);
    alongside.initialize().await;

    // While its Answer is on its way, the Sidekick queues a Prompt; then the
    // Agent's Provider refuses the Answer.
    users_provider.gate_questionnaire_deliveries();
    let (refusal, ()) = tokio::join!(
        answer_refused(&mut sidekick.client, users, &questionnaire),
        async {
            let delivery = timeout(
                PROGRESS_DEADLINE,
                users_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Answer reaches the Session's Provider");
            assert_eq!(
                send(&mut alongside, users, "Then the signup test.", "queue").await,
                json!("queued")
            );
            delivery.reject();
        },
    );
    assert!(
        refusal.contains("The Answer was not delivered"),
        "the Sidekick is told its Answer did not take: {refusal}"
    );

    // The user's Turn settles, and the queued Prompt begins the next.
    completes(&users_provider, FIXED).await;
    let turn = timeout(PROGRESS_DEADLINE, users_provider.next_turn())
        .await
        .expect("the queued Prompt begins the next Turn");
    assert_eq!(turn.prompt(), "Then the signup test.");
    turn.succeed();
    completes(&users_provider, "The signup test passes too.").await;

    let report = sidekick
        .steered("the Turn the Sidekick's queued Prompt began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(
            users,
            "Run the auth suite.",
            "completed",
            "The signup test passes too."
        ),
        "the refused Answer took back nothing the Prompt was owed, and the user's Turn it was \
         for was never reported"
    );
    sidekick.handed_nothing("nothing else is reported");

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn an_intervention_asked_as_soon_as_a_sidekicks_prompt_arrives_is_reported() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-at-once").await;
    let (users, _, mut users_provider) = sidekick.users_session("Run the auth suite.").await;
    assert_eq!(
        send(&mut sidekick.client, users, ASKED, "steer").await,
        json!("steer")
    );
    // The Agent asks the moment the Sidekick's Prompt reaches its Turn.
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt steers the working Turn")
        .succeed();
    users_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        })
        .await;

    let report = sidekick
        .steered("the Questionnaire asked at once is reported")
        .await;
    assert_eq!(
        report,
        format!(
            "Sidekick Report from Suru: the Session \"Run the auth suite.\" you set to work asks \
             a Questionnaire, which waits on an Answer. Its session_id is {users}: read_session \
             gives its Questions, and answer_questionnaire answers it."
        )
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_subagent_working_on_after_the_turn_that_spawned_it_settled_still_reports_its_intervention()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-branch").await;
    let descriptor = sidekick.descriptor.clone();
    let (users, handoff, mut users_provider) = sidekick.users_session("Run the auth suite.").await;
    assert_eq!(
        send(&mut sidekick.client, users, ASKED, "steer").await,
        json!("steer")
    );
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt steers the working Turn")
        .succeed();

    // The Turn the Sidekick steered delegates, and settles while its
    // Subagent works on.
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let child = agent
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) = run_child(&mut sidekick.hosted.codex, codex_selection("high")).await;
    completes(&users_provider, "The Researcher is on it.").await;
    let settled = sidekick
        .steered("the Turn the Sidekick steered is reported as it settles")
        .await;
    assert_eq!(
        untimed_sidekick_report(&settled),
        settled_report(
            users,
            "Run the auth suite.",
            "completed",
            "The Researcher is on it."
        )
    );
    assert!(
        read_session(&descriptor, child).await.turns[0].status == TurnStatus::Active,
        "the Subagent works on"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        })
        .await;
    let asked = sidekick
        .steered("the Questionnaire of the Subagent the steered Turn spawned is reported")
        .await;
    assert_eq!(
        asked,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"Run the auth suite.\" you \
             set to work asks a Questionnaire, which waits on an Answer. The Session's \
             session_id is {users}, and the Subagent's is {child}: given the Subagent's, \
             read_session gives its Questions, and answer_questionnaire answers it."
        ),
        "the work the Sidekick set going goes on in the Subagent, so it is told"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_subsession_reports_the_turns_the_sidekick_set_going_and_not_the_users_later_ones() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-lifetime-subsession").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, mut begun_provider) = sidekick.begin().await;
    fixes(&descriptor, begun, &begun_provider).await;
    let first = sidekick
        .steered("the Subsession's first Turn is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&first),
        settled_report(begun, ASKED, "completed", FIXED)
    );

    // The user takes the Subsession on: a Turn of theirs, and its
    // Questionnaire, are theirs alone.
    admit_prompt(&descriptor, begun, "Now the signup test.").await;
    timeout(PROGRESS_DEADLINE, begun_provider.next_turn())
        .await
        .expect("the user's Prompt begins a Turn")
        .succeed();
    asks(&descriptor, begun, &begun_provider).await;
    completes(&begun_provider, "The signup test passes too.").await;
    latest_turn_settles(&descriptor, begun, TurnStatus::Completed).await;
    sidekick.handed_nothing("the user's later Turn in the Subsession is not reported");

    // The Sidekick's own next Prompt is.
    assert_eq!(
        send(&mut sidekick.client, begun, "And the logout test.", "steer").await,
        json!("new_turn")
    );
    timeout(PROGRESS_DEADLINE, begun_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn")
        .succeed();
    completes(&begun_provider, "All three pass.").await;
    let next = sidekick
        .steered("the Turn the Sidekick's next Prompt began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&next),
        settled_report(begun, ASKED, "completed", "All three pass."),
        "the first Report after the user's Turn is of the Sidekick's own"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
