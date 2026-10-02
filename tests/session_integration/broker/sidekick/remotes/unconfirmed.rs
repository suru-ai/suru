//! A Sidekick's act on a Remote whose answer never came back whole may have
//! been done all the same, so it is kept, durably, as not yet confirmed: the
//! Sidekick is told which Session to read — for a beginning, the Session it
//! is where it was begun, an identity chosen before the Remote was first
//! asked — and any read of that Remote finding the Session confirms it. A
//! beginning asked again by that Session is the very same request, so it
//! never begins a second Session, and never prepares a second Worktree.

use suru::protocol::{AgentId, SessionListItem};

use super::*;
use crate::broker::sidekick::answering::{ask, stood, where_to_run};
use crate::broker::sidekick_acts::{acted, refused};
use crate::broker::subsessions::committed;

/// Waits until `own` stores its act on the Remote's Session `session_id` as
/// `confirmed`, or not.
async fn stored_as(own: &OwnServer, session_id: SessionId, confirmed: bool) {
    let wanted = (REMOTE.to_owned(), session_id.to_string(), confirmed);
    let found = timeout(PROGRESS_DEADLINE, async {
        loop {
            if own.stored_remote_act_states().contains(&wanted) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "the act on {session_id} is stored as confirmed {confirmed}: {:?}",
        own.stored_remote_act_states()
    );
}

/// What a Sidekick is told of a beginning on the Remote whose answer was
/// lost: that it may have been begun as `session_id`, how to find out, and
/// how to ask again.
fn may_have_begun(session_id: SessionId) -> String {
    format!(
        "It may have been begun there all the same, as Session {session_id}: read it with \
         read_session, \"session_id\": \"{session_id}\" and \"origin\": \"{REMOTE}\", to find \
         out, or call begin_session again with the same arguments and \"session_id\": \
         \"{session_id}\", which begins it only where it was not."
    )
}

/// The Session a refusal of a beginning named.
fn named_in(refusal: &str) -> SessionId {
    let named = refusal
        .split("as Session ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_else(|| panic!("the refusal names the Session: {refusal}"));
    serde_json::from_value(json!(named)).expect("the named Session is an identity")
}

/// A Session a Client of the Remote begins at `directory`, whose first Turn
/// waits on its Provider being started — so it holds the Repository there
/// until that start is answered — answering the start to answer.
async fn holding_the_repository(
    remote: &mut Serving,
    directory: &Path,
) -> (SessionId, StartRequest) {
    let selection = default_selection(&claude_models());
    let created = create_session(
        &remote.descriptor(),
        &session_request(directory, selection, "Hold the Repository."),
    )
    .await;
    (created.session.id, next_start(&mut remote.provider).await)
}

/// Answers the start `start` waited on, freeing the Repository it held.
fn let_go(start: StartRequest) -> ControlledProviderSession {
    start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    })
}

/// An Answer the Remote took whose answer was lost is kept, not yet
/// confirmed, across a stop of this Server, and named to the Sidekick by the
/// Session to read; the first read of the Remote that finds the Session
/// confirms it.
#[tokio::test]
async fn an_answer_whose_answer_was_lost_is_kept_unconfirmed_until_a_read_finds_its_session() {
    let mut remote = Serving::start("sidekick-remote-unconfirmed-answer").await;
    let mut own = OwnServer::start(
        "sidekick-remote-unconfirmed-answer",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (asking, mut asking_provider) = started_session(
        &remote.descriptor(),
        &mut remote.provider,
        there.path(),
        "Run the tests.",
    )
    .await;
    let questionnaire = where_to_run();
    ask(
        &remote.descriptor(),
        asking,
        &asking_provider,
        &questionnaire,
    )
    .await;
    asking_provider.gate_questionnaire_deliveries();

    let answering = tokio::spawn(async move {
        let mut sidekick = sidekick;
        let refusal = refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        )
        .await;
        (sidekick, refusal)
    });
    let delivery = timeout(
        PROGRESS_DEADLINE,
        asking_provider.next_questionnaire_delivery(),
    )
    .await
    .expect("the Answer reaches the Remote's Agent");
    // Its answer is lost on the way back.
    remote.route.set_online(false).await;
    delivery.succeed();
    let (_sidekick, refusal) = answering.await.expect("the Tool answers");
    assert!(
        refusal.contains(&format!(
            "It may have been done there all the same: read the Session with read_session, \
             \"session_id\": \"{asking}\" and \"origin\": \"{REMOTE}\""
        )),
        "the Answer is never denied, and the Session to read is named: {refusal}"
    );
    stored_as(&own, asking, false).await;

    // Kept across a stop, and confirmed by the first read that finds it.
    own = own.restart().await;
    assert_eq!(
        own.stored_remote_act_states(),
        [(REMOTE.to_owned(), asking.to_string(), false)],
        "the act is kept, not yet confirmed, across a stop"
    );
    remote.route.set_online(true).await;
    let (_tree, updates) = crate::subagent_tree::open_tree(&own.descriptor(), sidekick_id).await;
    stored_as(&own, asking, true).await;
    assert_eq!(
        stood(
            &read_session(&remote.descriptor(), asking).await,
            questionnaire.id
        )
        .map(|(outcome, ..)| outcome),
        Some(suru::protocol::QuestionnaireOutcome::Answered),
        "it was answered there, once"
    );

    drop(updates);
    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// A beginning on the Remote whose answer is lost after the Remote began the
/// Session is named to the Sidekick by the Session it began — an identity
/// chosen before the Remote was asked — and kept, not yet confirmed. Asked
/// again by that Session, it is the same request, which finds the Session
/// already begun rather than beginning another.
#[tokio::test]
async fn a_beginning_whose_answer_was_lost_is_asked_again_as_the_same_beginning() {
    const ASKED: &str = "Fix the flaky login test.";
    let mut remote = Serving::start("sidekick-remote-unconfirmed-beginning").await;
    let mut own = OwnServer::start(
        "sidekick-remote-unconfirmed-beginning",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let home = tempfile::tempdir().expect("create a home for the Remote's Repository");
    let repository = suru::paths::canonical(home.path())
        .expect("read the Remote's home canonically")
        .join("auth");
    committed(&repository);
    let (_sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (holder, start) = holding_the_repository(&mut remote, &repository).await;

    let arguments = json!({ "origin": REMOTE, "directory": repository, "prompt": ASKED });
    let beginning = tokio::spawn({
        let arguments = arguments.clone();
        async move {
            let mut sidekick = sidekick;
            let refusal = refused(&mut sidekick, "begin_session", arguments).await;
            (sidekick, refusal)
        }
    });
    // Its creation is kept before it is asked for, and waits there on the
    // Repository; once it is let go, the Remote begins it and its answer is
    // lost on the way back.
    timeout(PROGRESS_DEADLINE, async {
        while own.stored_remote_act_states().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the beginning is kept before it is asked for");
    remote.route.lose_answers();
    let _holder_provider = let_go(start);
    let (mut sidekick, refusal) = beginning.await.expect("the Tool answers");
    let session_id = named_in(&refusal);
    assert!(
        refusal.ends_with(&format!(
            "{} It stands among the Sessions you have a hand in, not yet confirmed, until a \
             read of that Remote finds it — or finds it was not begun.",
            may_have_begun(session_id)
        )),
        "{refusal}"
    );
    assert_eq!(
        read_session(&remote.descriptor(), session_id)
            .await
            .prompts
            .first()
            .map(|prompt| prompt.text.as_str()),
        Some(ASKED),
        "the Remote began it, as the Session named"
    );
    stored_as(&own, session_id, false).await;

    // The Session begun holds the Repository until its own first Turn's
    // Provider is started, as every Session begun in one does.
    let _begun_provider = let_go(next_start(&mut remote.provider).await);
    remote.route.set_online(true).await;
    let mut again = arguments;
    again["session_id"] = json!(session_id);
    let begun = acted(&mut sidekick, "begin_session", again).await;
    assert_eq!(
        (&begun["session_id"], &begun["origin"]),
        (&json!(session_id), &json!(REMOTE)),
        "asked again, it answers with the Session the Remote began: {begun}"
    );
    let mut held = reqwest::Client::new()
        .get(format!("{}/v1/sessions", remote.descriptor().base_url))
        .bearer_auth(&remote.descriptor().token)
        .send()
        .await
        .expect("list the Remote's Sessions")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Remote's Sessions")
        .into_iter()
        .map(|listed| listed.id())
        .collect::<Vec<_>>();
    held.sort_by_key(|session| session.as_uuid());
    let mut expected = vec![holder, session_id];
    expected.sort_by_key(|session| session.as_uuid());
    assert_eq!(held, expected, "and no second Session was begun");
    stored_as(&own, session_id, true).await;

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// A beginning in a new Worktree whose preparation's answer is lost is asked
/// again by the Session it names as the same preparation, which the Remote
/// resumes: one Worktree is made, and the Session begins in it.
#[tokio::test]
async fn a_prepared_beginning_whose_answer_was_lost_resumes_the_same_preparation() {
    const ASKED: &str = "Tidy the auth module.";
    let mut remote = Serving::start("sidekick-remote-unconfirmed-prepared").await;
    let mut own = OwnServer::start(
        "sidekick-remote-unconfirmed-prepared",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let home = tempfile::tempdir().expect("create a home for the Remote's Repository");
    let repository = suru::paths::canonical(home.path())
        .expect("read the Remote's home canonically")
        .join("auth");
    committed(&repository);
    let (_sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (_holder, start) = holding_the_repository(&mut remote, &repository).await;

    let arguments = json!({
        "origin": REMOTE,
        "directory": repository,
        "prompt": ASKED,
        "new_worktree": true,
    });
    let beginning = tokio::spawn({
        let arguments = arguments.clone();
        async move {
            let mut sidekick = sidekick;
            let refusal = refused(&mut sidekick, "begin_session", arguments).await;
            (sidekick, refusal)
        }
    });
    timeout(PROGRESS_DEADLINE, async {
        while own.stored_remote_act_states().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the beginning is kept before it is asked for");
    remote.route.lose_answers();
    let _holder_provider = let_go(start);
    let (mut sidekick, refusal) = beginning.await.expect("the Tool answers");
    let session_id = named_in(&refusal);
    assert!(refusal.contains(&may_have_begun(session_id)), "{refusal}");

    remote.route.set_online(true).await;
    let mut again = arguments;
    again["session_id"] = json!(session_id);
    let begun = acted(&mut sidekick, "begin_session", again).await;
    let directory = std::path::PathBuf::from(
        begun["directory"]
            .as_str()
            .expect("the answer says where the Session works"),
    );
    assert_eq!(
        (&begun["session_id"], directory.parent()),
        (
            &json!(session_id),
            Some(repository.join(".suru-worktrees").as_path())
        ),
        "the Session named begins in the Worktree prepared for it: {begun}"
    );
    let worktrees = std::fs::read_dir(repository.join(".suru-worktrees"))
        .expect("read the Remote's Worktrees")
        .count();
    assert_eq!(worktrees, 1, "and no second Worktree was made");

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}
