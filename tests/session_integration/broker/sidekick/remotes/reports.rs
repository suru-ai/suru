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
use crate::broker::sidekick::answering::where_to_run;

mod delayed;
use crate::broker::sidekick_acts::{acted, refused};
use crate::server_support::broker::untimed_sidekick_report;
use crate::subagent_tree::{TreeUpdates, next_change, open_tree};

/// What the Sidekick asks of the Sessions it sets to work on the Remote.
const ASKED: &str = "Fix the flaky login test in the auth suite.";

/// What those Sessions' Agents answer with.
const FIXED: &str = "Fixed: the login test waited on a token that had already expired.";

/// How the Sidekick's own Server runs in these tests: looking often for
/// whether a Remote is still to be kept in view, trying one that does not
/// answer again soon, and reading what is owed there soon after it moves.
fn timings() -> ServerTimings {
    ServerTimings {
        sse_keepalive_interval: Duration::from_millis(50),
        ..ServerTimings::default()
    }
    .with_remote_retry_interval(Duration::from_millis(50))
    // Long enough that a Remote read under a loaded test run is never taken
    // for one not answering; nothing here waits it out.
    .with_remote_reach_timeout(Duration::from_secs(5))
    .with_remote_report_reads(Duration::from_millis(10), Duration::from_millis(100))
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
        let (session_id, _, provider) = self.begin_handing().await;
        (session_id, provider)
    }

    /// As [`Self::begin`], with the Broker handoff the begun Session's
    /// Provider start carried on the Remote, through which a test acts as
    /// its Agent there.
    async fn begin_handing(&mut self) -> (SessionId, BrokerHandoff, ControlledProviderSession) {
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
        let start = next_start(&mut self.pair.remote.provider).await;
        let handoff = start
            .broker()
            .cloned()
            .expect("a Provider start on the Remote carries its Broker handoff");
        let mut provider = start.succeed(AgentIdentity {
            agent: AgentId::new("claude-agent"),
            selection: default_selection(&claude_models()),
        });
        timeout(PROGRESS_DEADLINE, provider.next_turn())
            .await
            .expect("the first Turn reaches the Remote's Provider")
            .succeed();
        (session_id, handoff, provider)
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
        observed(&self.provider, ProviderEvent::TurnCompleted).await;
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

/// Has `provider` emit `event`, attributed to `attribution`, and waits —
/// no longer than the progress deadline — until its Provider actor has
/// taken it up.
async fn observed_as(
    provider: &ControlledProviderSession,
    attribution: ProviderEventAttribution,
    event: ProviderEvent,
) {
    let what = format!("{event:?}");
    timeout(
        PROGRESS_DEADLINE,
        provider.emit_attributed_and_wait_until_observed(attribution, event),
    )
    .await
    .unwrap_or_else(|_| panic!("the Provider actor takes up {what}"));
}

/// [`observed_as`] the Session owning the Provider.
async fn observed(provider: &ControlledProviderSession, event: ProviderEvent) {
    observed_as(provider, ProviderEventAttribution::OwningSession, event).await;
}

/// Has `provider` write `text` as one whole Agent Message, each event taken
/// up before the next is sent, within the progress deadline.
async fn writes(provider: &ControlledProviderSession, text: &str) {
    timeout(PROGRESS_DEADLINE, write_agent_message(provider, text))
        .await
        .unwrap_or_else(|_| panic!("the Provider actor takes up the Message {text:?}"));
}

/// Has `provider` ask `questionnaire` in the working Turn of the Remote's
/// `session_id`, and waits — no longer than the progress deadline — until
/// that Session holds it waiting on an Answer.
async fn ask(
    remote: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
    questionnaire: &suru::protocol::Questionnaire,
) {
    timeout(
        PROGRESS_DEADLINE,
        crate::broker::sidekick::answering::ask(remote, session_id, provider, questionnaire),
    )
    .await
    .expect("the Questionnaire comes to wait on an Answer");
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
    writes(provider, FIXED).await;
    observed(provider, ProviderEvent::TurnCompleted).await;
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

/// A Remote answers a Peer the outline of the tree a Session belongs to, read
/// in one moment and stamped with it: every Session of the tree, with its
/// Turns, Prompts — and the Turn that took each — rows into Subagents and
/// Interventions, each moment on the Remote's one clock, and nothing anyone
/// wrote in any of it.
#[tokio::test]
async fn a_remote_answers_the_outline_of_a_tree_read_in_one_moment_and_nothing_written_in_it() {
    let mut owed = owed("sidekick-remote-outline", timings()).await;
    let remote = owed.remote();
    let (session_id, handoff, provider) = owed.begin_handing().await;
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let selection = default_selection(&claude_models());
    let child = agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (child_provider, _) = run_child(&mut owed.pair.remote.provider, selection).await;
    ask(&remote, child, &child_provider, &where_to_run()).await;
    writes(&provider, FIXED).await;

    let outline: suru::protocol::SessionTreeOutline = reqwest::Client::new()
        .get(format!(
            "{}/v1/remotes/{REMOTE}/v1/sessions/{child}/outline",
            owed.own.base_url
        ))
        .bearer_auth(&owed.own.token)
        .send()
        .await
        .expect("ask the Remote for the outline")
        .error_for_status()
        .expect("the Remote answers it")
        .json()
        .await
        .expect("decode the outline");
    assert_eq!(
        outline
            .sessions
            .iter()
            .map(|snapshot| snapshot.session.id)
            .collect::<Vec<_>>(),
        [session_id, child],
        "the whole tree, its top-level Session first, whichever Session was asked after"
    );
    let head = &outline.sessions[0];
    let turn = &head.turns[0];
    assert_eq!(
        head.prompts[0].taken.map(|taken| taken.turn_id),
        Some(turn.id),
        "the Turn begun for the first Prompt took it"
    );
    let asked_at = outline.sessions[1]
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Questionnaire {
                asked_at,
                questionnaire,
                ..
            } => {
                assert!(
                    questionnaire.questions.is_empty(),
                    "nothing it asks: {activity:?}"
                );
                *asked_at
            }
            _ => None,
        })
        .expect("the Subagent's Questionnaire stands, stamped");
    assert!(
        turn.started_at < Some(asked_at) && asked_at < outline.read_at,
        "every moment on the Remote's one clock, before the moment it was read"
    );
    for snapshot in &outline.sessions {
        assert!(snapshot.transcript.is_empty());
        assert!(
            snapshot
                .messages
                .iter()
                .all(|message| message.content.is_empty()),
            "nothing written: {:?}",
            snapshot.messages
        );
        assert!(
            snapshot.prompts.iter().all(|prompt| prompt.text.is_empty()),
            "nothing asked: {:?}",
            snapshot.prompts
        );
    }
    assert_eq!(
        head.messages.len(),
        head.turns.len(),
        "the first Message of each Turn, saying who opened it"
    );

    owed.shutdown().await;
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

    observed(
        &begun_provider,
        ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        },
    )
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
    observed_as(
        &begun_provider,
        ProviderEventAttribution::OwningSession,
        ProviderEvent::SubagentStarted {
            subagent_id: explorer.clone(),
            name: "Explore".to_owned(),
            description: "Survey the flaky tests".to_owned(),
            delegation: None,
        },
    )
    .await;
    observed_as(
        &begun_provider,
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
async fn a_subagent_working_on_past_a_remote_turn_reports_until_the_branch_settles_and_no_longer() {
    let mut owed = owed("sidekick-remote-report-branch", timings()).await;
    let remote = owed.remote();
    let (begun, handoff, begun_provider) = owed.begin_handing().await;

    // The Turn the Sidekick began delegates there, and settles while its
    // Subagent works on.
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let selection = default_selection(&claude_models());
    let child = agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (child_provider, _) = run_child(&mut owed.pair.remote.provider, selection).await;
    fixes(&remote, begun, &begun_provider).await;
    assert_eq!(
        untimed_sidekick_report(
            &owed
                .steered("the Turn the Sidekick began is reported as it settles")
                .await
        ),
        settled_there(begun, ASKED, "completed", FIXED)
    );

    observed(
        &child_provider,
        ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        },
    )
    .await;
    assert_eq!(
        owed.steered("the Questionnaire of the Subagent the Turn left working is reported")
            .await,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"{ASKED}\" you set to work on \
             the Remote \"{REMOTE}\" asks a Questionnaire, which waits on an Answer. The \
             Session's session_id is {begun}, and the Subagent's is {child}, each at origin \
             \"{REMOTE}\": given the Subagent's, read_session gives its Questions, and \
             answer_questionnaire answers it."
        ),
        "the work the Sidekick set going goes on in the Subagent, so it is told"
    );

    // The branch settles: nothing more is owed there, and the Remote is let
    // go of.
    observed(&child_provider, ProviderEvent::TurnCompleted).await;
    latest_turn_settles(&remote, child, TurnStatus::Completed).await;
    owed.pair.remote.route.wait_for_connections(0).await;
    owed.steered_with_nothing("a branch settled whole tells nothing more")
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
    observed(&provider, ProviderEvent::TurnInterrupted).await;
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

/// A Remote unpaired and at once paired again — the same Server, by the same
/// name and key — is another Pairing: what was owed through the one that
/// ended is told lost once, and what the Sidekick sets going through the new
/// one is owed and told, however soon after the old one's watch retires.
#[tokio::test]
async fn a_remote_paired_again_at_once_owes_what_is_set_going_through_the_new_pairing() {
    let mut owed = owed("sidekick-remote-report-repaired", timings()).await;
    let remote = owed.remote();
    let (before, before_provider) = owed.begin().await;

    reqwest::Client::new()
        .delete(format!("{}/v1/pairing/remotes/{REMOTE}", owed.own.base_url))
        .bearer_auth(&owed.own.token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
    pair(&owed.own, &owed.pair.remote, REMOTE).await;
    let (after, after_provider) = owed.begin().await;
    assert_eq!(
        owed.steered("the Pairing that ended is reported, though another stands by its name")
            .await,
        lost_there(false, &[(before, ASKED)])
    );

    // What was owed through the ended Pairing ended with it; what the
    // Sidekick set going through the new one is told as it settles.
    fixes(&remote, before, &before_provider).await;
    fixes(&remote, after, &after_provider).await;
    let report = owed
        .steered("the Session begun through the new Pairing is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_there(after, ASKED, "completed", FIXED)
    );
    owed.steered_with_nothing("nothing of the ended Pairing's Session is told")
        .await;

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

/// Starts the Claude double's Provider on the Remote for a brokered Subagent
/// just spawned there on `selection`, and takes up its first Turn, answering
/// the Broker handoff its start carried and the double's view of it.
async fn run_there(
    remote: &mut ControlledProvider,
    selection: AgentSelection,
) -> (BrokerHandoff, ControlledProviderSession) {
    let start = next_start(remote).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a Provider start on the Remote carries its Broker handoff");
    let mut provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection,
    });
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    (handoff, provider)
}

/// A Session on the Remote whose Turn delegated two levels deep there: the
/// grandchild has settled while the child works on, and the Turn that
/// spawned the child has settled too.
struct DelegatedThere {
    /// The Agent of the Session at the top, as the MCP client it is.
    agent: McpClient,
    provider: ControlledProviderSession,
    grandchild: SessionId,
    grandchild_provider: ControlledProviderSession,
    child_provider: ControlledProviderSession,
}

/// Has the Agent of the Remote's Session `handoff` was handed to — its Turn
/// working on `provider` — delegate two levels deep, and lets everything but
/// the child settle.
async fn delegated_two_deep_there(
    owed: &mut Owed,
    session_id: SessionId,
    handoff: &BrokerHandoff,
    provider: ControlledProviderSession,
) -> DelegatedThere {
    let remote = owed.remote();
    let selection = default_selection(&claude_models());
    let mut agent = McpClient::handed(handoff);
    agent.initialize().await;
    agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (child_handoff, mut child_provider) =
        run_there(&mut owed.pair.remote.provider, selection.clone()).await;
    let mut child = McpClient::handed(&child_handoff);
    child.initialize().await;
    let grandchild = child
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, grandchild_provider) = run_there(&mut owed.pair.remote.provider, selection).await;
    // The grandchild settles, its Report steering the child at work.
    writes(&grandchild_provider, "The seams are mapped.").await;
    observed(&grandchild_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, child_provider.next_steer())
        .await
        .expect("the grandchild's Report steers the child's working Turn")
        .succeed();
    // The Turn that spawned the child settles while the child works on.
    fixes(&remote, session_id, &provider).await;
    DelegatedThere {
        agent,
        provider,
        grandchild,
        grandchild_provider,
        child_provider,
    }
}

/// Has `delegated`'s Agent resume its grandchild from the Turn its Session
/// works in now, and the grandchild take the resume up.
async fn resumes_grandchild_there(delegated: &mut DelegatedThere) {
    let resumed = delegated
        .agent
        .send_to_subagent(delegated.grandchild, "Check the seams once more.")
        .await;
    assert_ne!(
        resumed["isError"],
        json!(true),
        "the resume is taken: {resumed}"
    );
    timeout(PROGRESS_DEADLINE, delegated.grandchild_provider.next_turn())
        .await
        .expect("the resume reaches the grandchild's Provider")
        .succeed();
}

/// A Turn the Sidekick began, then steered, then answered is one Turn of its
/// work: each Intervention it asks is told once, and its settling once.
#[tokio::test]
async fn a_remote_turn_the_sidekick_began_steered_and_answered_is_told_of_once() {
    let mut owed = owed("sidekick-remote-report-merged", timings()).await;
    let remote = owed.remote();
    let (begun, mut begun_provider) = owed.begin().await;
    assert_eq!(owed.prompt(begun).await, json!("steer"));
    timeout(PROGRESS_DEADLINE, begun_provider.next_steer())
        .await
        .expect("the Sidekick's steer reaches the Remote's working Turn")
        .succeed();

    let questionnaire = where_to_run();
    ask(&remote, begun, &begun_provider, &questionnaire).await;
    assert!(
        owed.steered("the Questionnaire is reported")
            .await
            .contains("asks a Questionnaire"),
    );
    owed.steered_with_nothing("the Turn the Sidekick began and steered is one Turn to hear of")
        .await;
    let (answered, _) = tokio::join!(
        acted(
            &mut owed.sidekick,
            "answer_questionnaire",
            json!({
                "session_id": begun,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["local"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                begun_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Remote's Agent")
        },
    );
    assert_eq!(answered["answered"], json!(true));

    fixes(&remote, begun, &begun_provider).await;
    assert_eq!(
        untimed_sidekick_report(&owed.steered("the Turn's settling is reported").await),
        settled_there(begun, ASKED, "completed", FIXED)
    );
    owed.steered_with_nothing("its settling is told once, however it was set to work")
        .await;

    owed.shutdown().await;
}

/// A steer the Remote's Agent never took — only recorded in the Turn it was
/// meant for as that Turn settled — tells its Sidekick nothing; and a Prompt
/// another Sidekick on the same Peer sent in the very same words is that
/// Sidekick's alone.
#[tokio::test]
async fn a_steer_the_remotes_agent_never_took_is_no_ones_and_the_same_words_are_their_senders() {
    let mut owed = owed("sidekick-remote-report-untaken", timings()).await;
    let remote = owed.remote();
    let (_other_id, mut other, mut other_provider) =
        start_sidekick(&owed.own, &mut owed.pair.claude).await;
    let (users, mut users_provider) = owed.users_session("Run the auth suite.").await;

    assert_eq!(owed.prompt(users).await, json!("steer"));
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt is offered to the Remote's working Turn")
        .fail("The Turn no longer takes steers.");
    fixes(&remote, users, &users_provider).await;
    owed.pair.remote.route.wait_for_connections(0).await;
    owed.steered_with_nothing("a steer the Agent never took set nothing going")
        .await;

    // The other Sidekick on this Peer sends the very same words, which begin
    // a Turn of their own.
    assert_eq!(
        acted(
            &mut other,
            "send_prompt",
            json!({ "session_id": users, "origin": REMOTE, "prompt": ASKED }),
        )
        .await["admitted"],
        json!("new_turn")
    );
    timeout(PROGRESS_DEADLINE, users_provider.next_turn())
        .await
        .expect("the other Sidekick's Prompt begins a Turn")
        .succeed();
    fixes(&remote, users, &users_provider).await;
    let steer = timeout(PROGRESS_DEADLINE, other_provider.next_steer())
        .await
        .expect("the Turn the other Sidekick's Prompt began is reported to it");
    assert_eq!(
        untimed_sidekick_report(&the_report(steer.reports())),
        settled_there(users, "Run the auth suite.", "completed", FIXED)
    );
    steer.succeed();
    owed.steered_with_nothing("the same words sent by another are that other's alone")
        .await;

    owed.shutdown().await;
}

/// A grandchild that a Turn of the Remote's user resumes works for that
/// Turn, so what it asks is no Report of the Sidekick's whose Turn first set
/// it working; and one a Turn of the Sidekick's resumes is the Sidekick's,
/// whoever first set it working.
#[tokio::test]
async fn a_remote_subagent_belongs_to_the_turn_that_last_set_it_working() {
    let mut owed = owed("sidekick-remote-report-resumed", timings()).await;
    let remote = owed.remote();
    let (begun, handoff, provider) = owed.begin_handing().await;
    let mut delegated = delegated_two_deep_there(&mut owed, begun, &handoff, provider).await;
    owed.steered("the Sidekick's Turn is reported as it settles, its branch working on")
        .await;

    // A Turn of the user's own there resumes the grandchild, which asks.
    admit_prompt(&remote, begun, "Ask the grandchild again.").await;
    timeout(PROGRESS_DEADLINE, delegated.provider.next_turn())
        .await
        .expect("the user's Prompt begins a Turn on the Remote")
        .succeed();
    resumes_grandchild_there(&mut delegated).await;
    ask(
        &remote,
        delegated.grandchild,
        &delegated.grandchild_provider,
        &where_to_run(),
    )
    .await;
    owed.steered_with_nothing("the grandchild works for the user's Turn now")
        .await;

    // A Turn of the Sidekick's resumes it again, and it asks an Approval.
    writes(&delegated.grandchild_provider, "Checked.").await;
    observed(&delegated.grandchild_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, delegated.provider.next_steer())
        .await
        .expect("the grandchild's Report steers the user's Turn")
        .succeed();
    fixes(&remote, begun, &delegated.provider).await;
    assert_eq!(owed.prompt(begun).await, json!("new_turn"));
    timeout(PROGRESS_DEADLINE, delegated.provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn on the Remote")
        .succeed();
    resumes_grandchild_there(&mut delegated).await;
    observed(
        &delegated.grandchild_provider,
        ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        },
    )
    .await;
    assert_eq!(
        owed.steered("the grandchild's Approval for the Sidekick's Turn is reported")
            .await,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"{ASKED}\" you set to work on \
             the Remote \"{REMOTE}\" asks an Approval, which waits on the user's Decision. The \
             Session's session_id is {begun}, and the Subagent's is {}, each at origin \
             \"{REMOTE}\": given the Subagent's, read_session says what it asks.",
            delegated.grandchild
        ),
        "the first Report after the Sidekick's own branch settled is of the grandchild its own \
         Turn resumed, never of what it asked for the user's"
    );
    drop(delegated.child_provider);

    owed.shutdown().await;
}

/// A branch the Sidekick's Turn set going whose rows have all settled while
/// a grandchild works on still works: what the grandchild asks is reported,
/// though no tree of that Remote is followed to say it works.
#[tokio::test]
async fn a_remote_branch_works_on_while_a_grandchild_does_though_no_tree_is_followed() {
    let mut owed = owed(
        "sidekick-remote-report-grandchild",
        timings().with_remote_watch_limits(suru::server::RemoteWatchLimits {
            trees_per_remote: 0,
            ..suru::server::RemoteWatchLimits::default()
        }),
    )
    .await;
    let remote = owed.remote();
    let (begun, handoff, mut provider) = owed.begin_handing().await;
    let selection = default_selection(&claude_models());
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (child_handoff, child_provider) =
        run_there(&mut owed.pair.remote.provider, selection.clone()).await;
    let mut child = McpClient::handed(&child_handoff);
    child.initialize().await;
    let grandchild = child
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, grandchild_provider) = run_there(&mut owed.pair.remote.provider, selection).await;
    // The child's own Turn settles — its row in the Session settling with it
    // — while the grandchild it spawned works on; and then the Turn that
    // spawned the child settles too.
    writes(&child_provider, "Handed on.").await;
    observed(&child_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, provider.next_steer())
        .await
        .expect("the child's Report steers the Turn that spawned it")
        .succeed();
    fixes(&remote, begun, &provider).await;
    owed.steered("the Sidekick's Turn is reported as it settles, its branch working on")
        .await;

    ask(&remote, grandchild, &grandchild_provider, &where_to_run()).await;
    assert!(
        owed.steered("the grandchild's Questionnaire, in the branch working on, is reported")
            .await
            .contains(&format!(
                "a Subagent of the Session \"{ASKED}\" you set to work on the Remote \"{REMOTE}\" \
                 asks a Questionnaire"
            ))
    );

    owed.shutdown().await;
}

/// A Session the Remote's own user began, whose first Turn works, with the
/// Broker handoff its Provider start carried — through which a test acts as
/// its Agent there — and its Provider double's view of it.
async fn users_session_handing(
    owed: &mut Owed,
    text: &str,
) -> (SessionId, BrokerHandoff, ControlledProviderSession) {
    let remote = owed.remote();
    let selection = default_selection(&claude_models());
    let created = create_session(
        &remote,
        &session_request(owed.there.path(), selection.clone(), text),
    )
    .await;
    let start = next_start(&mut owed.pair.remote.provider).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a Provider start on the Remote carries its Broker handoff");
    let mut provider = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection,
    });
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the first Turn reaches the Remote's Provider")
        .succeed();
    (created.session.id, handoff, provider)
}

/// A Session of the Remote's user whose working Turn delegated to two
/// Subagents there: the Sidekick answers the first's Questionnaire, and that
/// Subagent's Turn the Answer went on in settles while the other keeps the
/// Session working — so nothing the Remote says of the Session itself moves,
/// and only the tree it heads changes. Answers the Report the Sidekick is
/// steered with.
async fn answered_subagent_settles_beside_a_sibling(
    owed: &mut Owed,
) -> (SessionId, SessionId, String) {
    let remote = owed.remote();
    let (users, handoff, mut users_provider) =
        users_session_handing(owed, "Run the auth suite.").await;
    let selection = default_selection(&claude_models());
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let answered = agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, mut answered_provider) =
        run_there(&mut owed.pair.remote.provider, selection.clone()).await;
    agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, _sibling_provider) = run_there(&mut owed.pair.remote.provider, selection).await;

    let questionnaire = where_to_run();
    ask(&remote, answered, &answered_provider, &questionnaire).await;
    let (reply, _) = tokio::join!(
        acted(
            &mut owed.sidekick,
            "answer_questionnaire",
            json!({
                "session_id": answered,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["local"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                answered_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Subagent's Agent")
        },
    );
    assert_eq!(reply["answered"], json!(true));
    // A Questionnaire it asks while it works is reported: what this Server
    // read of the tree has the Turn the Answer went on in working.
    let second = where_to_run();
    ask(&remote, answered, &answered_provider, &second).await;
    assert!(
        owed.steered("the answered Subagent's next Questionnaire is reported")
            .await
            .contains("asks a Questionnaire")
    );
    let (reply, _) = tokio::join!(
        acted(
            &mut owed.sidekick,
            "answer_questionnaire",
            json!({
                "session_id": answered,
                "origin": REMOTE,
                "questionnaire_id": second.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                answered_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the second Answer reaches the Subagent's Agent")
        },
    );
    assert_eq!(reply["answered"], json!(true));
    // Each Answer had the Remote read again; the Subagent settles once those
    // readings have landed, so only its tree says it did.
    tokio::time::sleep(Duration::from_millis(200)).await;

    writes(&answered_provider, FIXED).await;
    observed(&answered_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the answered Subagent's Report steers the user's working Turn")
        .succeed();
    let report = owed
        .steered("the Turn the Sidekick's Answer went on in is reported as it settles")
        .await;
    drop(_sibling_provider);
    (users, answered, report)
}

/// The Report of the answered Subagent's Turn settling, untimed.
fn settled_beneath(users: SessionId, answered: SessionId) -> String {
    format!(
        "Sidekick Report from Suru: a Subagent of the Session \"Run the auth suite.\" you set to \
         work on the Remote \"{REMOTE}\" has settled its Turn, which completed. The Session's \
         session_id is {users}, and the Subagent's is {answered}, each at origin \"{REMOTE}\", \
         which read_session takes.\n\nIts Agent's final Message:\n\n{FIXED}"
    )
}

/// Review item 7: a tree owed Reports that no stream of its own is followed
/// for — here there is room for none — is read all the same each poll
/// interval, so what only its tree says, an answered Subagent settling
/// while its sibling keeps the Session working, is reported.
#[tokio::test]
async fn a_tree_owed_reports_with_no_stream_of_its_own_is_polled() {
    let mut owed = owed(
        "sidekick-remote-report-polled",
        timings().with_remote_watch_limits(suru::server::RemoteWatchLimits {
            trees_per_remote: 0,
            ..suru::server::RemoteWatchLimits::default()
        }),
    )
    .await;
    let (users, answered, report) = answered_subagent_settles_beside_a_sibling(&mut owed).await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_beneath(users, answered)
    );

    owed.shutdown().await;
}

/// Review item 7: a tree owed Reports takes the stream of one only a Client
/// watches, where there is no room for both, so what only its tree says is
/// heard of as it moves rather than at the next poll, which here never
/// comes in time.
#[tokio::test]
async fn a_tree_owed_reports_takes_the_stream_of_one_only_a_client_watches() {
    let mut owed = owed(
        "sidekick-remote-report-preempts",
        timings()
            .with_remote_watch_limits(suru::server::RemoteWatchLimits {
                trees_per_remote: 1,
                ..suru::server::RemoteWatchLimits::default()
            })
            .with_remote_report_reads(Duration::from_millis(10), Duration::from_secs(600)),
    )
    .await;
    // A Client watches the Sidekick's tree, which lists another Session the
    // Sidekick only set aside there, whose own tree takes the one stream.
    let (set_aside, _set_aside_provider) = owed.users_session("Tidy the listing.").await;
    let (tree, mut updates) = open_tree(&owed.own, owed.sidekick_id).await;
    acted(
        &mut owed.sidekick,
        "settle_session",
        json!({ "session_id": set_aside, "origin": REMOTE }),
    )
    .await;
    listed(tree, &mut updates, &[set_aside]).await;

    let (users, answered, report) = answered_subagent_settles_beside_a_sibling(&mut owed).await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_beneath(users, answered)
    );

    drop(updates);
    owed.shutdown().await;
}

/// Review item 10: a Remote's trees owed Reports are read no more often
/// than the read interval allows, however soon the Remote says something
/// more moved in them.
#[tokio::test]
async fn a_remote_tree_owed_reports_is_read_no_more_often_than_the_read_interval() {
    const READ_INTERVAL: Duration = Duration::from_millis(400);
    let mut owed = owed(
        "sidekick-remote-report-paced",
        timings().with_remote_report_reads(READ_INTERVAL, Duration::from_secs(600)),
    )
    .await;
    let remote = owed.remote();
    let (begun, begun_provider) = owed.begin().await;

    ask(&remote, begun, &begun_provider, &where_to_run()).await;
    owed.steered("the first Questionnaire is reported").await;
    let first = tokio::time::Instant::now();
    observed(
        &begun_provider,
        ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        },
    )
    .await;
    owed.steered("the Approval asked at once after it is reported")
        .await;
    assert!(
        first.elapsed() >= READ_INTERVAL - Duration::from_millis(100),
        "read no sooner than the interval allows after the reading before: {:?}",
        first.elapsed()
    );

    owed.shutdown().await;
}
