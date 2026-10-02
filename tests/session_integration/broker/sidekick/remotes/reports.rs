//! Sidekick Reports of a Remote's Sessions: a Sidekick is told of the work it
//! set going on a Remote as of the work it set going on its own Server. The
//! Remote owes a Peer's Sidekick nothing, so the Sidekick's own Server holds
//! what it is owed there and keeps the Remote in view — whatever any Client
//! is showing — while it is, and no longer: a Turn settling there that the
//! Sidekick began, prompted or answered is reported naming the Remote, with
//! its outcome and a bounded excerpt of its final Message read through the
//! Pairing, and each Questionnaire or Approval that work comes to owe there is
//! reported too, delivered exactly as a Report of the Sidekick's own Server's
//! Sessions is. A Remote that stops answering, or whose Pairing ends, while a
//! Report is owed from it tells the Sidekick so once, and nothing more is owed
//! there. Nothing is owed across a stop of the Sidekick's own Server, and no
//! Report stands in a Transcript on either Server.
//!
//! Each test pairs the Sidekick's own Server with its Remote in-process, as
//! the rest of this suite does, acts as the MCP client the Sidekick's harness
//! is, and reads what the Sidekick's Provider double is handed.

use reqwest::StatusCode;
use suru::protocol::{
    AgentId, Approval, ApprovalId, ApprovalSubject, MessageRole, SubagentTreeChange,
    SubagentTreeSnapshot,
};
use suru::provider::{ProviderEventAttribution, ProviderSubagentId, Report};

use super::*;
use crate::broker::sidekick::answering::{ask, where_to_run};
use crate::broker::sidekick_acts::{acted, refused};
use crate::server_support::broker::untimed_sidekick_report;
use crate::subagent_tree::{TreeUpdates, next_change, open_tree};

/// What the Sidekick asks of the Sessions it sets to work on the Remote.
const ASKED: &str = "Fix the flaky login test in the auth suite.";

/// What those Sessions' Agents answer with.
const FIXED: &str = "Fixed: the login test waited on a token that had already expired.";

/// How the Sidekick's own Server runs in these tests: looking often for
/// whether a Remote is still to be kept in view, and trying one that does not
/// answer again soon.
fn timings() -> ServerTimings {
    ServerTimings {
        sse_keepalive_interval: Duration::from_millis(50),
        ..ServerTimings::default()
    }
    .with_remote_retry_interval(Duration::from_millis(50))
    .with_remote_reach_timeout(Duration::from_secs(2))
}

/// Two Servers paired in-process, with a Sidekick on the own Server whose
/// first Turn is still working, the MCP client its Agent is, and its Provider
/// double's view of it; and a Workspace on the Remote to work in.
struct Owed {
    pair: Paired,
    own: RuntimeDescriptor,
    sidekick_id: SessionId,
    sidekick: McpClient,
    provider: ControlledProviderSession,
    there: tempfile::TempDir,
}

async fn owed(channel: &str, timings: ServerTimings) -> Owed {
    let mut pair = paired(channel, timings).await;
    let own = pair.own.descriptor().clone();
    let (sidekick_id, sidekick, provider) = start_sidekick(&own, &mut pair.claude).await;
    Owed {
        pair,
        own,
        sidekick_id,
        sidekick,
        provider,
        there: tempfile::tempdir().expect("create a Workspace on the Remote"),
    }
}

impl Owed {
    fn remote(&self) -> RuntimeDescriptor {
        self.pair.remote.descriptor()
    }

    /// Begins a Session on the Remote through `begin_session`, asking it
    /// [`ASKED`], and answers its Provider's start and first Turn there.
    async fn begin(&mut self) -> (SessionId, ControlledProviderSession) {
        let directory = suru::paths::canonical(self.there.path())
            .expect("read the Remote's Workspace canonically");
        let begun = acted(
            &mut self.sidekick,
            "begin_session",
            json!({ "origin": REMOTE, "directory": directory, "prompt": ASKED }),
        )
        .await;
        let session_id = serde_json::from_value(begun["session_id"].clone())
            .unwrap_or_else(|_| panic!("begin_session names the Session: {begun}"));
        let mut provider =
            next_start(&mut self.pair.remote.provider)
                .await
                .succeed(AgentIdentity {
                    agent: AgentId::new("claude-agent"),
                    selection: default_selection(&claude_models()),
                });
        timeout(PROGRESS_DEADLINE, provider.next_turn())
            .await
            .expect("the first Turn reaches the Remote's Provider")
            .succeed();
        (session_id, provider)
    }

    /// A Session the Remote's own user began asking `text`, whose first Turn
    /// is running, and its Provider double's view of it.
    async fn users_session(&mut self, text: &str) -> (SessionId, ControlledProviderSession) {
        let remote = self.remote();
        started_session(
            &remote,
            &mut self.pair.remote.provider,
            self.there.path(),
            text,
        )
        .await
    }

    /// Sends the Remote's `session_id` [`ASKED`] through `send_prompt`,
    /// answering how the Session took it.
    async fn prompt(&mut self, session_id: SessionId) -> Value {
        acted(
            &mut self.sidekick,
            "send_prompt",
            json!({ "session_id": session_id, "origin": REMOTE, "prompt": ASKED }),
        )
        .await["admitted"]
            .clone()
    }

    /// The text of the one Report the next steer of the Sidekick's working
    /// Turn delivers, having checked the steer carries nothing else.
    async fn steered(&mut self, what: &str) -> String {
        let steer = timeout(PROGRESS_DEADLINE, self.provider.next_steer())
            .await
            .unwrap_or_else(|_| panic!("{what}"));
        let report = the_report(steer.reports());
        steer.succeed();
        report
    }

    /// Settles the Sidekick's working Turn, as its Agent ending its turn
    /// would, and waits until its Session says so.
    async fn idles(&mut self) {
        self.provider
            .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
            .await;
        latest_turn_settles(&self.own, self.sidekick_id, TurnStatus::Completed).await;
    }

    /// Asserts the Sidekick's Provider has been handed nothing — no steer and
    /// no Turn — since it was last read.
    fn handed_nothing(&mut self, why: &str) {
        assert!(self.provider.try_next_steer().is_none(), "{why}");
        assert!(self.provider.try_next_turn().is_none(), "{why}");
    }

    /// Asserts the Sidekick's Provider is steered with nothing for a while
    /// longer than its own Server takes to hear of the Remote, since nothing
    /// more is owed.
    async fn steered_with_nothing(&mut self, why: &str) {
        assert!(
            timeout(Duration::from_millis(300), self.provider.next_steer())
                .await
                .is_err(),
            "{why}"
        );
        self.handed_nothing(why);
    }

    async fn shutdown(self) {
        self.pair.shutdown().await;
    }
}

/// The one Report `reports` holds, as the text its Agent reads.
fn the_report(reports: &[Report]) -> String {
    assert_eq!(reports.len(), 1, "one Report is delivered: {reports:?}");
    reports[0].to_string()
}

/// The Report of a Turn of the Remote's `session_id`, titled `title`, that
/// settled as `settled` with `final_message`, as the Sidekick reads it
/// untimed.
fn settled_there(session_id: SessionId, title: &str, settled: &str, final_message: &str) -> String {
    format!(
        "Sidekick Report from Suru: the Session \"{title}\" you set to work on the Remote \
         \"{REMOTE}\" has settled its Turn, which {settled}. Its session_id is {session_id} and \
         its origin \"{REMOTE}\", which read_session takes.\n\nIts Agent's final \
         Message:\n\n{final_message}"
    )
}

/// The Report that the Remote stopped answering, or that its Pairing ended,
/// while the Sidekick was owed Reports of `sessions` there, each with its
/// Title.
fn lost_there(stopped_answering: bool, sessions: &[(SessionId, &str)]) -> String {
    let named = sessions
        .iter()
        .map(|(session_id, title)| format!("\"{title}\" (session_id {session_id})"))
        .collect::<Vec<_>>()
        .join(", ");
    if stopped_answering {
        format!(
            "Sidekick Report from Suru: the Remote \"{REMOTE}\" stopped answering while you were \
             owed Reports of the Sessions you set to work there, so none will come of them: \
             {named}. read_session with origin \"{REMOTE}\" reads them once it answers again, \
             which list_remotes tells."
        )
    } else {
        format!(
            "Sidekick Report from Suru: the Pairing with the Remote \"{REMOTE}\" ended while you \
             were owed Reports of the Sessions you set to work there, so none will come of them: \
             {named}. Nothing of them can be read unless it is paired again."
        )
    }
}

/// Has `provider` write [`FIXED`] and settle its Turn as completed, and
/// waits until the Remote's `session_id` says so.
async fn fixes(
    remote: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
) {
    write_agent_message(provider, FIXED).await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(remote, session_id, TurnStatus::Completed).await;
}

/// Follows the Sidekick's tree, opened as `tree`, until it lists every one of
/// `sessions` on the Remote as that Remote says of them.
async fn listed(tree: SubagentTreeSnapshot, updates: &mut TreeUpdates, sessions: &[SessionId]) {
    let mut revision = tree.revision;
    let mut listed = tree
        .sessions
        .iter()
        .filter(|entry| entry.origin.as_deref() == Some(REMOTE) && !entry.unanswered)
        .map(|entry| entry.session_id)
        .collect::<Vec<_>>();
    while !sessions
        .iter()
        .all(|session_id| listed.contains(session_id))
    {
        if let SubagentTreeChange::SessionChanged { entry } =
            next_change(updates, &mut revision).await
            && entry.origin.as_deref() == Some(REMOTE)
            && !entry.unanswered
        {
            listed.push(entry.session_id);
        }
    }
}

/// An Approval to run the test suite.
fn run_the_suite() -> Approval {
    Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Command {
            command: "cargo nextest run".into(),
            cwd: None,
            actions: Vec::new(),
        },
        reason: None,
    }
}

/// The roles of the Messages `snapshot`'s Transcript holds, and how many
/// items it holds besides.
fn transcript_of(snapshot: &SessionSnapshot) -> (Vec<MessageRole>, usize) {
    let roles = snapshot
        .messages
        .iter()
        .map(|message| message.role.clone())
        .collect::<Vec<_>>();
    let besides = snapshot.transcript.len() - roles.len();
    (roles, besides)
}

#[tokio::test]
async fn a_remote_turn_a_sidekick_began_steers_it_with_a_report_naming_the_remote_kept_in_view_for_it()
 {
    let mut owed = owed("sidekick-remote-report-steer", timings()).await;
    let remote = owed.remote();
    let (begun, begun_provider) = owed.begin().await;
    let before = read_session(&owed.own, owed.sidekick_id).await;
    // No Client watches anything here: what holds the Remote's route open
    // is the Report owed from it alone.
    owed.pair
        .remote
        .route
        .wait_for_connections_at_least(1)
        .await;

    fixes(&remote, begun, &begun_provider).await;
    let report = owed
        .steered("the Report of the Turn the Sidekick began steers its working Turn")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_there(begun, ASKED, "completed", FIXED),
        "the Report names the Session and its Remote, how its Turn settled, and its final \
         Message as read through the Pairing"
    );
    owed.handed_nothing("the steer is the whole delivery");

    owed.pair.remote.route.wait_for_connections(0).await;
    let after = read_session(&owed.own, owed.sidekick_id).await;
    assert_eq!(
        (after.turns.len(), after.transcript.len()),
        (before.turns.len(), before.transcript.len()),
        "the Report stands nowhere in the Sidekick's Transcript"
    );
    assert_eq!(
        transcript_of(&read_session(&remote, begun).await),
        (vec![MessageRole::User, MessageRole::Agent], 0),
        "nor in the reported Session's on the Remote, which holds its first Prompt and its \
         Agent's Message"
    );

    owed.shutdown().await;
}

#[tokio::test]
async fn an_idle_sidekick_is_woken_by_a_remote_turn_its_prompt_began_and_never_by_the_users_next() {
    let mut owed = owed("sidekick-remote-report-wake", timings()).await;
    let remote = owed.remote();
    let (prompted, mut prompted_provider) = owed.users_session("Run the auth suite.").await;
    // The user's own first Turn settles before the Sidekick has any hand in
    // the Session, and is no Sidekick's to hear of.
    fixes(&remote, prompted, &prompted_provider).await;
    owed.idles().await;

    assert_eq!(owed.prompt(prompted).await, json!("new_turn"));
    let turn = timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn on the Remote");
    assert_eq!(turn.prompt(), ASKED);
    turn.succeed();
    fixes(&remote, prompted, &prompted_provider).await;

    let woken = timeout(PROGRESS_DEADLINE, owed.provider.next_turn())
        .await
        .expect("the Report wakes the idle Sidekick into a Continuation");
    assert!(!woken.has_prompt(), "the Report is the whole of its input");
    assert_eq!(
        untimed_sidekick_report(&the_report(woken.reports())),
        settled_there(prompted, "Run the auth suite.", "completed", FIXED)
    );
    woken.succeed();
    owed.pair.remote.route.wait_for_connections(0).await;

    // The user begins the next Turn there; the Sidekick's work has settled.
    admit_prompt(&remote, prompted, "Now the signup test.").await;
    timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the user's Prompt begins a Turn on the Remote")
        .succeed();
    fixes(&remote, prompted, &prompted_provider).await;
    owed.steered_with_nothing("the user's later Turn on the Remote is never reported")
        .await;

    owed.shutdown().await;
}

#[tokio::test]
async fn a_remote_turn_a_sidekick_answered_waits_for_the_head_of_its_next_turn_with_no_provider() {
    let mut owed = owed("sidekick-remote-report-held", timings()).await;
    let remote = owed.remote();
    let (asking, mut asking_provider) = owed.users_session("Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&remote, asking, &asking_provider, &questionnaire).await;
    owed.idles().await;

    let (answered, _) = tokio::join!(
        acted(
            &mut owed.sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["local"] }, { "text": "Quickly." }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Remote's Agent")
        },
    );
    assert_eq!(answered["answered"], json!(true));

    // The Sidekick's Provider process ends while the Session it answered
    // works on.
    let Owed {
        mut pair,
        own,
        sidekick_id,
        sidekick: mut client,
        provider,
        ..
    } = owed;
    drop(provider);
    timeout(PROGRESS_DEADLINE, async {
        while client.initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Sidekick's Provider connection is gone");

    fixes(&remote, asking, &asking_provider).await;
    // Told of the Turn, its own Server owes nothing more there and lets the
    // Remote go: the Report waits for the Sidekick's next Turn.
    pair.remote.route.wait_for_connections(0).await;
    assert!(
        pair.claude.try_next_start().is_none(),
        "no Provider is started to deliver it"
    );
    admit_prompt(&own, sidekick_id, "How did the tests go?").await;
    let mut relaunched = next_start(&mut pair.claude).await.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the Prompt's Turn reaches the Provider it relaunched");
    assert_eq!(
        untimed_sidekick_report(&the_report(turn.reports())),
        settled_there(asking, "Run the tests.", "completed", FIXED),
        "the Turn the Sidekick's Answer went on in is reported at the head of its next Turn"
    );
    assert_eq!(turn.prompt(), "How did the tests go?");
    turn.succeed();

    pair.shutdown().await;
}

#[tokio::test]
async fn each_intervention_a_remote_turn_the_sidekick_began_comes_to_owe_is_reported_once() {
    let mut owed = owed("sidekick-remote-report-interventions", timings()).await;
    let remote = owed.remote();
    let (begun, begun_provider) = owed.begin().await;

    ask(&remote, begun, &begun_provider, &where_to_run()).await;
    assert_eq!(
        owed.steered("the Questionnaire the Remote's Session asks is reported")
            .await,
        format!(
            "Sidekick Report from Suru: the Session \"{ASKED}\" you set to work on the Remote \
             \"{REMOTE}\" asks a Questionnaire, which waits on an Answer. Its session_id is \
             {begun} and its origin \"{REMOTE}\": read_session gives its Questions, and \
             answer_questionnaire answers it."
        )
    );

    begun_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        })
        .await;
    assert_eq!(
        owed.steered("the Approval the Remote's Session asks is reported")
            .await,
        format!(
            "Sidekick Report from Suru: the Session \"{ASKED}\" you set to work on the Remote \
             \"{REMOTE}\" asks an Approval, which waits on the user's Decision. Its session_id is \
             {begun} and its origin \"{REMOTE}\": read_session says what it asks."
        )
    );

    // A Subagent the Turn spawned there asks one of its own.
    let explorer = ProviderSubagentId::new("explorer");
    begun_provider
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentStarted {
                subagent_id: explorer.clone(),
                name: "Explore".to_owned(),
                description: "Survey the flaky tests".to_owned(),
                delegation: None,
            },
        )
        .await;
    begun_provider
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(explorer),
            ProviderEvent::QuestionnaireRequested {
                questionnaire: where_to_run(),
            },
        )
        .await;
    let asked = owed
        .steered("the Questionnaire a Subagent of the Turn asks there is reported")
        .await;
    let subagent = read_session(&remote, begun)
        .await
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the Subagent's row stands on the Remote");
    assert_eq!(
        asked,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"{ASKED}\" you set to work on \
             the Remote \"{REMOTE}\" asks a Questionnaire, which waits on an Answer. The \
             Session's session_id is {begun}, and the Subagent's is {subagent}, each at origin \
             \"{REMOTE}\": given the Subagent's, read_session gives its Questions, and \
             answer_questionnaire answers it."
        )
    );
    owed.steered_with_nothing("each Intervention is reported once, however often it is read")
        .await;

    owed.shutdown().await;
}

#[tokio::test]
async fn a_remote_session_only_read_interrupted_or_set_aside_tells_the_sidekick_nothing() {
    let mut owed = owed("sidekick-remote-report-untouched", timings()).await;
    let remote = owed.remote();
    let (untouched, mut provider) = owed.users_session("Run the auth suite.").await;

    acted(
        &mut owed.sidekick,
        "read_session",
        json!({ "session_id": untouched, "origin": REMOTE }),
    )
    .await;
    let (interrupted, _) = tokio::join!(
        acted(
            &mut owed.sidekick,
            "interrupt_session",
            json!({ "session_id": untouched, "origin": REMOTE }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Remote's Agent")
                .succeed();
        },
    );
    assert_eq!(
        interrupted["outcome"],
        json!("stopped_work"),
        "{interrupted}"
    );
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    latest_turn_settles(&remote, untouched, TurnStatus::Interrupted).await;
    acted(
        &mut owed.sidekick,
        "settle_session",
        json!({ "session_id": untouched, "origin": REMOTE }),
    )
    .await;
    acted(
        &mut owed.sidekick,
        "unsettle_session",
        json!({ "session_id": untouched, "origin": REMOTE }),
    )
    .await;

    admit_prompt(&remote, untouched, "Run it again.").await;
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the user's Prompt begins a Turn on the Remote")
        .succeed();
    fixes(&remote, untouched, &provider).await;
    owed.steered_with_nothing("nothing the Sidekick did there set work going")
        .await;
    owed.pair.remote.route.wait_for_connections(0).await;

    owed.shutdown().await;
}

#[tokio::test]
async fn a_remote_that_stops_answering_while_a_report_is_owed_is_reported_once_and_owes_no_more() {
    let mut owed = owed("sidekick-remote-report-unreachable", timings()).await;
    let remote = owed.remote();
    let (begun, begun_provider) = owed.begin().await;
    let (prompted, mut prompted_provider) = owed.users_session("Run the auth suite.").await;
    assert_eq!(owed.prompt(prompted).await, json!("steer"));
    timeout(PROGRESS_DEADLINE, prompted_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt steers the Remote's working Turn")
        .succeed();
    // A Client watches the Sidekick's tree, so the Remote is tried again for
    // as long as it does not answer; and it lists both Sessions once their
    // Remote has been read.
    let (tree, mut updates) = open_tree(&owed.own, owed.sidekick_id).await;
    listed(tree, &mut updates, &[begun, prompted]).await;

    owed.pair.remote.route.set_online(false).await;
    let lost = owed
        .steered("the Remote's not answering is reported while a Report is owed from it")
        .await;
    assert_eq!(
        lost,
        lost_there(true, &[(begun, ASKED), (prompted, "Run the auth suite.")]),
        "one Report names every Session it was owed Reports of there"
    );
    // It is tried again, and does not answer, over and over.
    let opened = owed.pair.remote.route.opened_connections();
    owed.pair
        .remote
        .route
        .wait_for_opened_connections(opened + 3)
        .await;
    owed.handed_nothing("however often it fails again, the Sidekick is told once");

    // What it was owed ended there: the work settling once the Remote
    // answers again tells it nothing.
    owed.pair.remote.route.set_online(true).await;
    fixes(&remote, begun, &begun_provider).await;
    fixes(&remote, prompted, &prompted_provider).await;
    owed.steered_with_nothing("nothing is owed of the Remote past the Report that it stopped")
        .await;

    // Nor is a Remote that stops answering when nothing is owed from it
    // ever reported, however a Client watches it.
    owed.pair.remote.route.set_online(false).await;
    let opened = owed.pair.remote.route.opened_connections();
    owed.pair
        .remote
        .route
        .wait_for_opened_connections(opened + 2)
        .await;
    owed.handed_nothing("a Remote owed nothing from is no Report's to tell");

    owed.shutdown().await;
}

#[tokio::test]
async fn a_remote_whose_pairing_ends_while_a_report_is_owed_is_reported_once() {
    let mut owed = owed("sidekick-remote-report-unpaired", timings()).await;
    let (begun, _begun_provider) = owed.begin().await;

    reqwest::Client::new()
        .delete(format!("{}/v1/pairing/remotes/{REMOTE}", owed.own.base_url))
        .bearer_auth(&owed.own.token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
    assert_eq!(
        owed.steered("the Pairing ending is reported while a Report is owed of the Remote")
            .await,
        lost_there(false, &[(begun, ASKED)])
    );
    owed.pair.remote.route.wait_for_connections(0).await;
    owed.steered_with_nothing("and only once").await;

    owed.shutdown().await;
}

#[tokio::test]
async fn an_answer_whose_answer_was_lost_owes_a_report_once_a_read_finds_the_turn_it_went_on_in() {
    let mut owed = owed("sidekick-remote-report-unconfirmed", timings()).await;
    let remote = owed.remote();
    let (asking, mut asking_provider) = owed.users_session("Run the tests.").await;
    let questionnaire = where_to_run();
    ask(&remote, asking, &asking_provider, &questionnaire).await;
    asking_provider.gate_questionnaire_deliveries();

    // The Answer reaches the Remote's Agent, and its answer is lost on the
    // way back.
    let route = &mut owed.pair.remote.route;
    let (refusal, ()) = tokio::join!(
        refused(
            &mut owed.sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
        async {
            let delivery = timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Answer reaches the Remote's Agent");
            route.set_online(false).await;
            delivery.succeed();
        },
    );
    assert!(
        refusal.contains("It may have been done there all the same"),
        "{refusal}"
    );
    // The Turn it went on in settles there before this Server can read
    // anything of it.
    fixes(&remote, asking, &asking_provider).await;
    owed.handed_nothing("an act whose outcome is unknown owes nothing yet");

    owed.pair.remote.route.set_online(true).await;
    let report = owed
        .steered("a read finding the Answer delivered reports the Turn it went on in, settled")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_there(asking, "Run the tests.", "completed", FIXED)
    );
    owed.pair.remote.route.wait_for_connections(0).await;
    owed.steered_with_nothing("the Turn read settled is reported once")
        .await;

    owed.shutdown().await;
}

#[tokio::test]
async fn nothing_owed_of_a_remote_outlives_a_stop_of_the_sidekicks_own_server() {
    let mut remote = Serving::start("sidekick-remote-report-restart").await;
    let mut own = OwnServer::start("sidekick-remote-report-restart", timings()).await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (prompted, mut prompted_provider) = started_session(
        &remote.descriptor(),
        &mut remote.provider,
        there.path(),
        "Run the auth suite.",
    )
    .await;
    fixes(&remote.descriptor(), prompted, &prompted_provider).await;
    acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": prompted, "origin": REMOTE, "prompt": ASKED }),
    )
    .await;
    timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn on the Remote")
        .succeed();

    drop(sidekick);
    let mut own = own.restart().await;
    fixes(&remote.descriptor(), prompted, &prompted_provider).await;
    remote.route.wait_for_connections(0).await;

    admit_prompt(&own.descriptor(), sidekick_id, "How did the auth suite go?").await;
    let mut relaunched = next_start(&mut own.claude).await.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the Prompt's Turn reaches the relaunched Provider");
    assert!(
        turn.reports().is_empty(),
        "nothing owed before the stop is reported after it: {:?}",
        turn.reports()
    );
    turn.succeed();

    own.server
        .shutdown()
        .await
        .expect("shut down the own Server");
    remote.shutdown().await;
}
