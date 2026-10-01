//! A Prompt sent while a requested Compaction runs. The Turn the request
//! began takes no steer (ADR 0041), so the Prompt is held, admitted and owed
//! the next Turn, with the Session Working throughout: it begins that Turn once
//! the Compaction completes, and is withdrawn — its text handed back to the
//! client that wrote it — if the Turn settles any other way. Suru holds it
//! itself, whatever the Provider would queue.

use super::{
    compaction_statuses, completed, failed, idle_session, request_compaction, session_where,
    turn_settled, user_messages,
};
use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    WorkingTurn, compact, controlled_selection, interrupt, read_session, working_turn,
};
use axum::http::StatusCode;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        ActivityStatus, AdmitPromptRequest, InitialPrompt, Prompt, PromptDelivery, PromptId,
        PromptStatus, RuntimeDescriptor, SessionChange, SessionId, SessionSnapshot, TurnStatus,
    },
    provider::ProviderEvent,
};
use tokio::time::timeout;

/// Sends `text` to the fixture's Session the way a client submits a Prompt by
/// default, to steer whatever Turn is running, answering the Prompt the
/// Session admitted.
async fn admit_steer(fixture: &WorkingTurn, text: &str) -> Prompt {
    admit_steer_to(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        text,
    )
    .await
}

/// [`admit_steer`] by the parts of a fixture, for a test holding the rest of
/// it elsewhere.
async fn admit_steer_to(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    text: &str,
) -> Prompt {
    let response = client
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("admit a Prompt");
    assert_eq!(response.status(), StatusCode::CREATED);
    response
        .json::<Prompt>()
        .await
        .expect("decode the admitted Prompt")
}

/// Whether `prompt` stands in `snapshot` held: admitted and still owed a Turn,
/// which every client draws as the user Message it will become.
fn held(snapshot: &SessionSnapshot, prompt: PromptId) -> bool {
    snapshot
        .prompts
        .iter()
        .any(|admitted| admitted.id == prompt && admitted.status == PromptStatus::Pending)
        && !snapshot
            .turns
            .iter()
            .any(|turn| turn.prompt_id == Some(prompt))
}

/// The status `prompt` stands at in `snapshot`.
fn prompt_status(snapshot: &SessionSnapshot, prompt: PromptId) -> PromptStatus {
    snapshot
        .prompts
        .iter()
        .find(|admitted| admitted.id == prompt)
        .expect("the Session keeps every Prompt it admitted")
        .status
}

/// A requested Compaction of the fixture's Session at work, with a Prompt sent
/// while it runs held behind it: the Turn the request began, and the Prompt.
async fn held_behind_a_requested_compaction(
    fixture: &mut WorkingTurn,
    text: &str,
) -> (SessionSnapshot, Prompt) {
    let requested = request_compaction(fixture).await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let prompt = admit_steer(fixture, text).await;
    let holding = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        held(&holding, prompt.id),
        "the Prompt is held, owed the next Turn: {:?}",
        holding.prompts
    );
    assert_eq!(holding.turns.len(), 2, "no Turn begins over the Compaction");
    assert_eq!(
        user_messages(&holding),
        user_messages(&requested),
        "nothing joins the Compaction's Turn"
    );
    assert_eq!(
        holding.session.working_since, requested.session.working_since,
        "the Session is Working as it was"
    );
    (requested, prompt)
}

/// After the Session's Turns have settled, asks it for one more and answers
/// the Prompt the Provider was next asked to begin a Turn with, which shows
/// whether any Prompt held before reached it.
async fn next_turn_begun_after(fixture: &mut WorkingTurn) -> String {
    let follow_up = admit_steer(fixture, "Start over on the lexer").await;
    let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("an idle Session begins a Turn for a Prompt");
    let prompt = next.prompt().to_owned();
    next.succeed();
    let begun = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        begun
            .turns
            .iter()
            .any(|turn| turn.prompt_id == Some(follow_up.id)),
        "the follow-up begins a Turn: {:?}",
        begun.turns
    );
    prompt
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_begins_the_next_turn_working_throughout() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-held-prompt-test";
    let mut fixture = idle_session(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    let (requested, prompt) =
        held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;
    let working_since = requested.session.working_since;
    assert_eq!(
        working_since, requested.turns[1].started_at,
        "Working from the moment the request began its Turn"
    );

    // Every reading from here until the held Prompt's Turn begins.
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the Session");
    let SessionEvent::Snapshot(subscribed) = timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("the Session's snapshot arrives")
        .expect("the Session stream stays open")
        .expect("the snapshot is valid")
    else {
        panic!("a subscription opens on a snapshot");
    };
    assert!(held(&subscribed, prompt.id));

    fixture
        .provider_session
        .emit(completed(Some(182_000), Some(31_000)));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let next = timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the held Prompt begins the next Turn once the Compaction completes");
    assert_eq!(next.prompt(), "Now the lexer");
    assert!(
        fixture.provider_session.try_next_steer().is_none(),
        "the held Prompt was never delivered as a steer"
    );

    let mut working = Vec::new();
    loop {
        let update = crate::support::next_session_update(&mut subscription).await;
        working.extend(update.changes.iter().filter_map(|change| match change {
            SessionChange::SessionWorkingChanged { working_since } => Some(*working_since),
            _ => None,
        }));
        if update.changes.iter().any(|change| {
            matches!(change, SessionChange::TurnAdded { turn } if turn.prompt_id == Some(prompt.id))
        }) {
            break;
        }
    }
    assert_eq!(
        working,
        Vec::new(),
        "Working never changes from the Compaction's Turn to the held Prompt's"
    );
    let begun = read_session(fixture.server.descriptor(), session_id).await;
    assert_eq!(begun.turns[1].status, TurnStatus::Completed);
    assert!(
        !begun
            .messages
            .iter()
            .any(|message| message.turn_id == begun.turns[1].id),
        "nothing joined the Compaction's Turn"
    );
    assert_eq!(begun.turns[2].prompt_id, Some(prompt.id));
    assert_eq!(begun.turns[2].status, TurnStatus::Active);
    assert_eq!(prompt_status(&begun, prompt.id), PromptStatus::Delivered);
    assert_eq!(begun.session.working_since, working_since);
    next.succeed();
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_that_fails_is_withdrawn() {
    for (described, outcome) in [
        (
            "the Compaction fails",
            vec![
                failed("Conversation too long to summarise"),
                ProviderEvent::TurnCompleted,
            ],
        ),
        (
            "the Provider ends the Turn mid-Compaction",
            vec![ProviderEvent::TurnCompleted],
        ),
        (
            "the Provider fails the Turn",
            vec![ProviderEvent::TurnFailed {
                message: "the CLI exited".to_owned(),
            }],
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let mut fixture = idle_session(state_dir.path(), "compaction-held-fails-test").await;
        let session_id = fixture.session_id;
        let (_, prompt) = held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;

        for event in outcome {
            fixture.provider_session.emit(event);
        }
        let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
            turn_settled(snapshot, 1)
        })
        .await;
        assert_eq!(settled.turns[1].status, TurnStatus::Failed, "{described}");
        assert_eq!(
            prompt_status(&settled, prompt.id),
            PromptStatus::Cancelled,
            "when {described}, the held Prompt is withdrawn as the Turn settles"
        );
        assert_eq!(
            settled.session.working_since, None,
            "when {described}, nothing is left Working"
        );
        assert_eq!(settled.turns.len(), 2, "{described}: no Turn begins for it");
        assert_eq!(
            next_turn_begun_after(&mut fixture).await,
            "Start over on the lexer",
            "when {described}, the withdrawn Prompt never reaches the Provider"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_the_provider_refuses_is_withdrawn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-held-refused-test").await;
    let session_id = fixture.session_id;
    let response = compact(
        &fixture.client,
        fixture.server.descriptor(),
        session_id,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let request = timeout(
        PROGRESS_DEADLINE,
        fixture.provider_session.next_compaction(),
    )
    .await
    .expect("the request reaches the Provider");
    let prompt = admit_steer(&fixture, "Now the lexer").await;
    request.fail("the CLI is not reading its input");

    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    assert_eq!(prompt_status(&settled, prompt.id), PromptStatus::Cancelled);
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_requested_compaction_that_is_interrupted_is_withdrawn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = idle_session(state_dir.path(), "compaction-held-interrupted-test").await;
    let session_id = fixture.session_id;
    let (_, prompt) = held_behind_a_requested_compaction(&mut fixture, "Now the lexer").await;

    let (response, ()) = tokio::join!(
        interrupt(&fixture.client, fixture.server.descriptor(), session_id),
        async {
            timeout(PROGRESS_DEADLINE, fixture.provider_session.next_interrupt())
                .await
                .expect("the interrupt reaches the Provider")
                .succeed();
        }
    );
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "the interrupt stops the Compaction, not the Prompt held behind it"
    );
    let interrupting = read_session(fixture.server.descriptor(), session_id).await;
    assert!(
        held(&interrupting, prompt.id),
        "the Prompt stays held until the Compaction settles: {:?}",
        interrupting.prompts
    );
    for event in [
        failed("Request was aborted."),
        ProviderEvent::TurnInterrupted,
    ] {
        fixture.provider_session.emit(event);
    }
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 1)
    })
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Interrupted);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Interrupted]);
    assert_eq!(
        prompt_status(&settled, prompt.id),
        PromptStatus::Cancelled,
        "the held Prompt is withdrawn as the Turn settles"
    );
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer",
        "the withdrawn Prompt never reaches the Provider"
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_held_behind_a_request_a_native_turn_overtook_is_withdrawn_leaving_that_turn_be() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "compaction-held-overtaken-test").await;
    let session_id = fixture.session_id;
    let descriptor = fixture.server.descriptor().clone();
    // A Watch outlives the Turn, leaving the Session idle but Monitoring.
    for event in [
        ProviderEvent::WatchStarted {
            watch_id: suru::provider::ProviderWatchId::new("monitor-1"),
            description: "Watch the build".to_owned(),
        },
        ProviderEvent::TurnCompleted,
    ] {
        fixture
            .provider_session
            .emit_and_wait_until_observed(event)
            .await;
    }
    session_where(
        &fixture,
        session_id,
        "the Session is Monitoring",
        |snapshot| turn_settled(snapshot, 0) && snapshot.session.monitoring_since.is_some(),
    )
    .await;

    // Hold the actor inside a Watch stop, so that what follows waits for it
    // in a fixed order: the request's command, then the held Prompt's, then
    // the Provider's own native turn, which the actor reads first.
    let client = fixture.client.clone();
    let (stopped, prompt) = tokio::join!(interrupt(&client, &descriptor, session_id), async {
        let stop = timeout(
            PROGRESS_DEADLINE,
            fixture.provider_session.next_watches_stop(),
        )
        .await
        .expect("the interrupt holds the actor in a Watch stop");
        let response = compact(&client, &descriptor, session_id, None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let prompt = admit_steer_to(&client, &descriptor, session_id, "Now the lexer").await;
        fixture
            .provider_session
            .emit(ProviderEvent::ContinuationStarted {
                selection: controlled_selection("gpt-subagent", "high", "fast"),
            });
        stop.succeed();
        prompt
    });
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);

    let woken = session_where(
        &fixture,
        session_id,
        "the native turn stands in a Continuation",
        |snapshot| snapshot.turns.len() == 3,
    )
    .await;
    assert_eq!(woken.turns[1].status, TurnStatus::Failed);
    assert_eq!(
        prompt_status(&woken, prompt.id),
        PromptStatus::Cancelled,
        "the Prompt held for a Compaction that never ran is withdrawn with it"
    );
    // The actor reads the Provider's output only between the commands queued
    // ahead of it — and not at all while it waits on a stop it asked for.
    timeout(
        PROGRESS_DEADLINE,
        fixture
            .provider_session
            .emit_and_wait_until_observed(ProviderEvent::TurnCompleted),
    )
    .await
    .expect("the actor reads the native turn's end rather than stopping that turn");
    let settled = session_where(
        &fixture,
        session_id,
        "the Continuation settles",
        |snapshot| turn_settled(snapshot, 2),
    )
    .await;
    assert_eq!(
        settled.turns[2].status,
        TurnStatus::Completed,
        "the Agent's own turn runs to its own boundary"
    );
    assert!(
        fixture.provider_session.try_next_interrupt().is_none(),
        "nothing withdrawn stops the Agent's own turn"
    );
    assert_eq!(
        settled.turns.len(),
        3,
        "no Turn begins for the withdrawn Prompt"
    );
    assert_eq!(
        next_turn_begun_after(&mut fixture).await,
        "Start over on the lexer"
    );
    fixture.server.shutdown().await.expect("shut down server");
}
