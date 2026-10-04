//! A Sidekick's act on a Remote whose answer never came back whole may have
//! been done all the same, so it is kept, durably, as not yet confirmed: the
//! Sidekick is told which Session to read — for a beginning, the Session it
//! is where it was begun, an identity chosen before the Remote was first
//! asked — and any read of that Remote finding the Session confirms it. A
//! beginning asked again by that Session is the very same request, so it
//! never begins a second Session, and never prepares a second Worktree.

use std::path::PathBuf;

use suru::protocol::{AgentId, SessionListItem};
use tokio::sync::oneshot;

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

/// A Repository on the Remote, committed, and a directory of it a beginning
/// names and nothing else does: so the Remote's first reading of that
/// directory is the beginning's own, made as its request reaches the Remote.
fn named_only_by_the_beginning(home: &Path) -> (PathBuf, PathBuf) {
    let repository = suru::paths::canonical(home)
        .expect("read the Remote's home canonically")
        .join("auth");
    committed(&repository);
    let directory = repository.join("login");
    std::fs::create_dir_all(&directory).expect("create the directory the beginning names");
    (repository, directory)
}

/// Waits until the request asking the Remote for a beginning has reached it,
/// held there as the Remote first reads its directory, `reached` says, and
/// the beginning is kept; then has every answer lost on the way back from the
/// Remote before letting the request go on, by `release`, so the Remote does
/// what it was asked and nothing of it is heard.
async fn lose_the_answer_to(
    remote: &mut Serving,
    own: &OwnServer,
    reached: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
) {
    timeout(PROGRESS_DEADLINE, reached)
        .await
        .expect("the beginning reaches the Remote")
        .expect("the Remote reads the directory it names");
    timeout(PROGRESS_DEADLINE, async {
        while own.stored_remote_act_states().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the beginning is kept, as it was before it was asked for");
    remote.route.lose_answers();
    release
        .send(())
        .expect("the Remote goes on with the beginning");
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
    let (_repository, directory) = named_only_by_the_beginning(home.path());
    let (_sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (reached, release) = remote.git.hold_discovery_of(&directory);

    let arguments = json!({ "origin": REMOTE, "directory": directory, "prompt": ASKED });
    let beginning = tokio::spawn({
        let arguments = arguments.clone();
        async move {
            let mut sidekick = sidekick;
            let refusal = refused(&mut sidekick, "begin_session", arguments).await;
            (sidekick, refusal)
        }
    });
    // Its creation reaches the Remote, which begins it once let go, and its
    // answer is lost on the way back.
    lose_the_answer_to(&mut remote, &own, reached, release).await;
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
    let held = reqwest::Client::new()
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
    assert_eq!(held, [session_id], "and no second Session was begun");
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
    let (repository, directory) = named_only_by_the_beginning(home.path());
    let (_sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (reached, release) = remote.git.hold_discovery_of(&directory);

    let arguments = json!({
        "origin": REMOTE,
        "directory": directory,
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
    // Its preparation reaches the Remote, which makes the Worktree once let
    // go, and its answer is lost on the way back.
    lose_the_answer_to(&mut remote, &own, reached, release).await;
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

/// Has the Sidekick `sidekick` answer `questionnaire`, waiting in the Remote's
/// Session `asking`, and the Answer reach the Remote's Agent, whose answer is
/// lost on the way back as the route to the Remote goes offline: answers the
/// Sidekick and its refusal.
async fn answer_lost(
    remote: &mut Serving,
    sidekick: McpClient,
    asking: SessionId,
    asking_provider: &mut ControlledProviderSession,
    questionnaire: &suru::protocol::Questionnaire,
) -> (McpClient, String) {
    asking_provider.gate_questionnaire_deliveries();
    let id = questionnaire.id;
    let answering = tokio::spawn(async move {
        let mut sidekick = sidekick;
        let refusal = refused(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": id,
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
    remote.route.set_online(false).await;
    delivery.succeed();
    asking_provider.ungate_questionnaire_deliveries();
    answering.await.expect("the Tool answers")
}

/// Second review, item 2: acts whose answers never came back, in two
/// Sessions heading trees of their own on one Remote, are each judged by a
/// reading of their own tree alone. A reading of one tree shows nothing of
/// the other, so it never finds the other's act was never done: each is
/// confirmed once its own tree shows it.
#[tokio::test]
async fn an_unknown_act_is_judged_only_by_a_reading_of_its_own_tree() {
    let mut remote = Serving::start("sidekick-remote-unconfirmed-trees").await;
    let mut own = OwnServer::start(
        "sidekick-remote-unconfirmed-trees",
        ServerTimings::default()
            .with_remote_reach_timeout(Duration::from_secs(2))
            .with_remote_retry_interval(Duration::from_millis(50)),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick_id, sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let mut asked = Vec::new();
    for title in ["Run the tests.", "Run the linters."] {
        let (asking, asking_provider) = started_session(
            &remote.descriptor(),
            &mut remote.provider,
            there.path(),
            title,
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
        asked.push((asking, asking_provider, questionnaire));
    }

    let mut sidekick = sidekick;
    for (asking, asking_provider, questionnaire) in &mut asked {
        let (answered, refusal) = answer_lost(
            &mut remote,
            sidekick,
            *asking,
            asking_provider,
            questionnaire,
        )
        .await;
        sidekick = answered;
        assert!(
            refusal.contains("It may have been done there all the same"),
            "{refusal}"
        );
        stored_as(&own, *asking, false).await;
        remote.route.set_online(true).await;
    }

    for (asking, ..) in &asked {
        stored_as(&own, *asking, true).await;
    }
    drop(sidekick);

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// An act whose answer never came back is confirmed by the Sidekick's own
/// read of its Session, as the refusal promised — the read taking only the
/// slice it asked for, and an outline of the Session's tree, which holds
/// nothing anyone wrote, judging the act — though nothing else keeps the
/// Remote in view meanwhile.
#[tokio::test]
async fn a_read_of_its_session_alone_confirms_an_act_whose_answer_was_lost() {
    let mut remote = Serving::start("sidekick-remote-unconfirmed-read").await;
    let hour = Duration::from_secs(60 * 60);
    let mut own = OwnServer::start(
        "sidekick-remote-unconfirmed-read",
        ServerTimings::default()
            .with_remote_reach_timeout(Duration::from_secs(2))
            .with_remote_retry_interval(hour)
            .with_remote_report_reads(hour, hour),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick_id, sidekick, _sidekick_provider) =
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
    let (mut sidekick, refusal) = answer_lost(
        &mut remote,
        sidekick,
        asking,
        &mut asking_provider,
        &questionnaire,
    )
    .await;
    assert!(
        refusal.contains("read the Session with read_session"),
        "{refusal}"
    );
    stored_as(&own, asking, false).await;
    remote.route.set_online(true).await;

    let read = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": asking, "origin": REMOTE, "detail": "activities" }),
    )
    .await;
    assert!(
        read["transcript"]
            .as_str()
            .is_some_and(|transcript| transcript.contains("answered by a Sidekick on this server")),
        "the read shows the Answer given there: {read}"
    );
    stored_as(&own, asking, true).await;

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}
