//! Sidekick Reports: a Sidekick is told of work it set going instead of
//! polling for it. Once it has begun a Session, sent one a Prompt, or answered
//! one's Questionnaire, Suru gives it an account of that Session when the
//! Turn it had a hand in settles — how it settled and a bounded excerpt of the
//! final Message — and whenever the Session comes to owe a Questionnaire or an
//! Approval meanwhile, naming which. The Report is the Sidekick's own Provider
//! input, delivered as a Subagent Report is: it steers a Turn still working,
//! wakes an idle Sidekick into a Continuation, and with no Provider process
//! waits for the head of its next Turn, never relaunching one. It stands in no
//! Transcript, and it is lost if Suru stops first.
//!
//! A Sidekick is never told of a Session it had no hand in, nor of one it
//! only read, listed, interrupted or set aside, nor of a Turn the user begins
//! in a Session it set to work: only of the work it set going there (see
//! `lifetime`).
//!
//! Each test acts as the MCP client a Sidekick's harness is and reads what the
//! Sidekick's Provider double is handed, which is what its Agent would read.

use reqwest::StatusCode;
use suru::protocol::{Approval, ApprovalId, ApprovalSubject, Questionnaire, QuestionnaireId};
use suru::provider::Report;
use suru::questionnaire::{Question, QuestionChoice};

use super::*;
use crate::broker::{
    reports::held_in,
    sidekick_acts::{acted, refused},
};
use crate::server_support::broker::untimed_sidekick_report;

/// What the Sidekick asks of the Sessions it sets to work.
const ASKED: &str = "Fix the flaky login test in the auth suite.";

/// What those Sessions' Agents answer with.
const FIXED: &str = "Fixed: the login test waited on a token that had already expired.";

mod lifetime;

/// A Server hosting Claude and Codex doubles, with a Sidekick on Claude whose
/// first Turn is still working, the MCP client its Agent is, the Broker
/// handoff its Provider start carried — from which a test may open another
/// client, for acts its Agent makes at once — and its Provider double's view
/// of it. The Sessions it sets to work run on Codex.
struct Sidekick {
    hosted: HostedProviders,
    descriptor: RuntimeDescriptor,
    id: SessionId,
    client: McpClient,
    handoff: BrokerHandoff,
    provider: ControlledProviderSession,
}

async fn sidekick(state_dir: &Path, channel: &str) -> Sidekick {
    let mut hosted = host_providers(state_dir, channel, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let directory = sidekick_directory(&descriptor).await;
    let (id, handoff, provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &directory,
        default_selection(&claude_models()),
    )
    .await;
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;
    Sidekick {
        hosted,
        descriptor,
        id,
        client,
        handoff,
        provider,
    }
}

impl Sidekick {
    /// Begins a Session on Codex through `begin_session`, asking it
    /// [`ASKED`], and answers its Provider's start and first Turn.
    async fn begin(&mut self) -> (SessionId, ControlledProviderSession) {
        let (session_id, _, provider) = self.begin_handing().await;
        (session_id, provider)
    }

    /// As [`Self::begin`], with the Broker handoff the begun Session's
    /// Provider start carried, through which a test acts as its Agent.
    async fn begin_handing(&mut self) -> (SessionId, BrokerHandoff, ControlledProviderSession) {
        let selection = default_selection(&codex_models());
        let answer = acted(
            &mut self.client,
            "begin_session",
            json!({
                "directory": self.hosted.workspace.path(),
                "prompt": ASKED,
                "agent_selection": {
                    "provider": "codex",
                    "model": selection.model,
                    "options": {},
                },
            }),
        )
        .await;
        let session_id = serde_json::from_value(answer["session_id"].clone())
            .unwrap_or_else(|_| panic!("begin_session names the Session: {answer}"));
        let (handoff, provider) = self.run_first_turn(selection).await;
        (session_id, handoff, provider)
    }

    /// A Session the user began on Codex with `text`, whose first Turn is
    /// running, the Broker handoff its Provider start carried, and its
    /// Provider double's view of it.
    async fn users_session(
        &mut self,
        text: &str,
    ) -> (SessionId, BrokerHandoff, ControlledProviderSession) {
        let selection = default_selection(&codex_models());
        let created = create_session(
            &self.descriptor,
            &session_request(self.hosted.workspace.path(), selection.clone(), text),
        )
        .await;
        let start = next_start(&mut self.hosted.codex).await;
        let handoff = start
            .broker()
            .cloned()
            .expect("a Provider start carries the Broker handoff");
        let mut provider = start.succeed(AgentIdentity {
            agent: AgentId::new("codex-agent"),
            selection,
        });
        timeout(PROGRESS_DEADLINE, provider.next_turn())
            .await
            .expect("the first Turn reaches the Provider")
            .succeed();
        (created.session.id, handoff, provider)
    }

    /// Starts the Codex Provider just asked to start for a Session on
    /// `selection`, and takes up its first Turn.
    async fn run_first_turn(
        &mut self,
        selection: AgentSelection,
    ) -> (BrokerHandoff, ControlledProviderSession) {
        run_on_codex(&mut self.hosted.codex, selection).await
    }

    /// Sends `session_id` [`ASKED`] through `send_prompt`, answering how the
    /// Session took it.
    async fn prompt(&mut self, session_id: SessionId) -> Value {
        acted(
            &mut self.client,
            "send_prompt",
            json!({ "session_id": session_id, "prompt": ASKED }),
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
        latest_turn_settles(&self.descriptor, self.id, TurnStatus::Completed).await;
    }

    /// Asserts the Sidekick's Provider has been handed nothing — no steer and
    /// no Turn — since it was last read.
    fn handed_nothing(&mut self, why: &str) {
        assert!(self.provider.try_next_steer().is_none(), "{why}");
        assert!(self.provider.try_next_turn().is_none(), "{why}");
    }
}

/// Starts the Codex Provider `codex` was just asked to start for a Session on
/// `selection` — a Session's own, or a brokered Subagent's — and takes up its
/// first Turn, answering the Broker handoff its start carried and the
/// Provider double's view of it.
async fn run_on_codex(
    codex: &mut ControlledProvider,
    selection: AgentSelection,
) -> (BrokerHandoff, ControlledProviderSession) {
    let start = next_start(codex).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a Provider start carries the Broker handoff");
    let mut provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection,
    });
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    (handoff, provider)
}

/// The one Report `reports` holds, as the text its Agent reads.
fn the_report(reports: &[Report]) -> String {
    assert_eq!(reports.len(), 1, "one Report is delivered: {reports:?}");
    reports[0].to_string()
}

/// The Report of a Turn of `session_id`, titled `title`, that settled as
/// `settled` with `final_message`, as the Sidekick reads it untimed.
fn settled_report(
    session_id: SessionId,
    title: &str,
    settled: &str,
    final_message: &str,
) -> String {
    format!(
        "Sidekick Report from Suru: the Session \"{title}\" you set to work has settled its \
         Turn, which {settled}. Its session_id is {session_id}, which read_session \
         takes.\n\nIts Agent's final Message:\n\n{final_message}"
    )
}

/// Has `provider` write [`FIXED`] and settle its Turn as completed, and waits
/// until `session_id` says so.
async fn fixes(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
) {
    write_agent_message(provider, FIXED).await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(descriptor, session_id, TurnStatus::Completed).await;
}

/// Everything `snapshot`'s Transcript holds.
fn transcript_len(snapshot: &SessionSnapshot) -> usize {
    snapshot.transcript.len()
}

/// A Questionnaire of one Question, offering two machines to run on.
fn where_to_run() -> Questionnaire {
    let offered = |id: &str, label: &str| QuestionChoice {
        id: id.to_owned(),
        label: label.to_owned(),
        description: None,
        recommended: false,
    };
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "machine".to_owned(),
            title: None,
            text: "Where should the tests run?".to_owned(),
            choices: vec![
                offered("staging", "Staging"),
                offered("local", "This machine"),
            ],
            multiple: false,
            freeform: false,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
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

#[tokio::test]
async fn a_working_sidekick_is_steered_with_the_report_of_a_session_it_began_settling() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-steer").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, begun_provider) = sidekick.begin().await;
    let before = read_session(&descriptor, sidekick.id).await;
    let begun_before = read_session(&descriptor, begun).await;

    fixes(&descriptor, begun, &begun_provider).await;

    let report = sidekick
        .steered("the Report steers the Sidekick's working Turn")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(begun, ASKED, "completed", FIXED),
        "the Report names the Session, how its Turn settled, and its final Message"
    );
    sidekick.handed_nothing("the steer is the whole delivery: no Turn begins for it");
    let after = read_session(&descriptor, sidekick.id).await;
    assert_eq!(
        (after.turns.len(), transcript_len(&after)),
        (before.turns.len(), transcript_len(&before)),
        "the Report stands nowhere in the Sidekick's Transcript"
    );
    let begun_after = read_session(&descriptor, begun).await;
    assert_eq!(
        transcript_len(&begun_after),
        transcript_len(&begun_before) + 1,
        "nor in the reported Session's, which gained only its Agent's Message"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn an_idle_sidekick_is_woken_into_a_continuation_with_the_report() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-wake").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, begun_provider) = sidekick.begin().await;
    sidekick.idles().await;

    fixes(&descriptor, begun, &begun_provider).await;

    let woken = timeout(PROGRESS_DEADLINE, sidekick.provider.next_turn())
        .await
        .expect("the Report wakes the idle Sidekick's Provider into a Turn");
    assert!(
        !woken.has_prompt(),
        "no Prompt begins it: the Report is the whole of its input"
    );
    assert_eq!(
        untimed_sidekick_report(&the_report(woken.reports())),
        settled_report(begun, ASKED, "completed", FIXED)
    );
    woken.succeed();
    let continuing = read_until(
        &descriptor,
        sidekick.id,
        "the Sidekick's Session shows the Continuation the Report woke",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    let continuation = &continuing.turns[1];
    assert_eq!(continuation.status, TurnStatus::Active);
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation, which no Prompt began"
    );
    assert_eq!(
        held_in(&continuing, continuation.id),
        0,
        "the Report stands nowhere in the Sidekick's Transcript"
    );

    write_agent_message(&sidekick.provider, "The login test is fixed.").await;
    sidekick
        .provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        sidekick.id,
        "the Continuation settles like any Turn",
        |snapshot| snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(
        settled
            .messages
            .iter()
            .filter(|message| message.turn_id == settled.turns[1].id)
            .map(|message| (message.role.clone(), message.content.as_str()))
            .collect::<Vec<_>>(),
        [(MessageRole::Agent, "The login test is fixed.")],
        "the Continuation holds what the Sidekick's Agent did, and nothing for the Report"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_with_no_provider_process_takes_the_report_at_the_head_of_its_next_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-held").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, begun_provider) = sidekick.begin().await;
    sidekick.idles().await;
    // The Sidekick's Provider process ends while the Session it began works on.
    let Sidekick {
        mut hosted,
        id: sidekick_id,
        mut client,
        provider,
        ..
    } = sidekick;
    drop(provider);
    timeout(PROGRESS_DEADLINE, async {
        while client.initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Sidekick's Provider connection is gone");

    fixes(&descriptor, begun, &begun_provider).await;
    let waiting = read_session(&descriptor, sidekick_id).await;
    assert_eq!(
        waiting.turns.len(),
        1,
        "no Continuation begins without a Provider to take the Report"
    );
    assert!(
        hosted.claude.try_next_start().is_none(),
        "and no Provider is started to deliver it"
    );

    admit_prompt(&descriptor, sidekick_id, "How did the login test go?").await;
    let mut relaunched = next_start(&mut hosted.claude).await.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the Prompt's Turn reaches the Provider it relaunched");
    assert_eq!(
        untimed_sidekick_report(&the_report(turn.reports())),
        settled_report(begun, ASKED, "completed", FIXED),
        "the held Report stands at the head of the next Turn's input"
    );
    assert_eq!(turn.prompt(), "How did the login test go?");
    turn.succeed();
    let prompted = read_session(&descriptor, sidekick_id).await;
    assert_eq!(
        held_in(&prompted, prompted.turns[1].id),
        1,
        "the Turn holds the user's Message and nothing for the Report"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_turn_a_sidekicks_prompt_began_is_reported_once_and_the_users_next_turn_never() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-prompted").await;
    let descriptor = sidekick.descriptor.clone();
    let (prompted, _, mut prompted_provider) = sidekick.users_session("Run the auth suite.").await;
    // The user's own first Turn settles before the Sidekick has any hand in
    // the Session, and is no Sidekick's to hear of.
    fixes(&descriptor, prompted, &prompted_provider).await;

    assert_eq!(sidekick.prompt(prompted).await, json!("new_turn"));
    let turn = timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn");
    assert_eq!(turn.prompt(), ASKED);
    turn.succeed();
    fixes(&descriptor, prompted, &prompted_provider).await;
    let report = sidekick
        .steered("the Turn the Sidekick's Prompt began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(prompted, "Run the auth suite.", "completed", FIXED)
    );

    // The user begins the next Turn; the Sidekick's work there has settled.
    admit_prompt(&descriptor, prompted, "Now the signup test.").await;
    timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the user's Prompt begins a Turn")
        .succeed();
    write_agent_message(&prompted_provider, "The signup test passes too.").await;
    prompted_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, prompted, TurnStatus::Completed).await;

    // The next Report the Sidekick is given is of the next Turn it prompts.
    assert_eq!(sidekick.prompt(prompted).await, json!("new_turn"));
    timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the Sidekick's second Prompt begins a Turn")
        .succeed();
    write_agent_message(&prompted_provider, "Both suites pass.").await;
    prompted_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let report = sidekick
        .steered("the Turn the Sidekick's second Prompt began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(
            prompted,
            "Run the auth suite.",
            "completed",
            "Both suites pass."
        ),
        "the user's Turn between them was never reported"
    );
    sidekick.handed_nothing("each settled Turn is reported once");

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_report_says_a_turn_failed_or_was_interrupted_and_where_to_read_a_message_it_cut() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-outcomes").await;
    let descriptor = sidekick.descriptor.clone();
    let (prompted, _, mut prompted_provider) = sidekick.users_session("Run the auth suite.").await;

    // The Sidekick steers the user's working Turn, which then fails having
    // written more than a Report carries.
    assert_eq!(sidekick.prompt(prompted).await, json!("steer"));
    timeout(PROGRESS_DEADLINE, prompted_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt steers the working Turn")
        .succeed();
    let long = format!("{}{}", "a".repeat(1_990), "b".repeat(100));
    write_agent_message(&prompted_provider, &long).await;
    prompted_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnFailed {
            message: "The model stopped responding.".to_owned(),
        })
        .await;
    latest_turn_settles(&descriptor, prompted, TurnStatus::Failed).await;
    let failed = sidekick
        .steered("the Turn the Sidekick steered is reported failed")
        .await;
    let failure = read_session(&descriptor, prompted)
        .await
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("the Turn's Transcript says why it failed");
    let (head, cut) = untimed_sidekick_report(&failed)
        .split_once("\n\n[Cut at 2000 characters: read_session with item \"")
        .map(|(head, cut)| (head.to_owned(), cut.to_owned()))
        .unwrap_or_else(|| panic!("the excerpt is cut, saying where the rest is: {failed}"));
    assert_eq!(
        head,
        format!(
            "Sidekick Report from Suru: the Session \"Run the auth suite.\" you set to work has \
             settled its Turn, which failed. Its session_id is {prompted}, which read_session \
             takes.\n\nIt failed with: {failure}\n\nIts Agent's final Message:\n\n{}",
            &long[..2_000]
        ),
        "a failed Turn says what it failed with, then as much of its final Message as a \
         Report carries"
    );
    let item = cut
        .strip_suffix("\" gives the whole Message.]")
        .unwrap_or_else(|| panic!("the cut names an item: {cut}"));
    let whole = sidekick
        .client
        .call_tool(
            "read_session",
            json!({ "session_id": prompted, "item": item }),
        )
        .await;
    assert!(
        whole["structuredContent"]["transcript"]
            .as_str()
            .is_some_and(|transcript| transcript.contains(&long)),
        "the item the Report names reads the whole Message: {whole}"
    );

    // The Sidekick prompts again, and the user stops that Turn.
    assert_eq!(sidekick.prompt(prompted).await, json!("new_turn"));
    timeout(PROGRESS_DEADLINE, prompted_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn")
        .succeed();
    let (stopped, ()) = tokio::join!(
        reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{prompted}/interrupt",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send(),
        async {
            timeout(PROGRESS_DEADLINE, prompted_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Session's Provider")
                .succeed();
        },
    );
    stopped
        .expect("send the interrupt")
        .error_for_status()
        .expect("the interrupt is taken");
    prompted_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    let interrupted = sidekick
        .steered("the Turn the user stopped is reported interrupted")
        .await;
    assert_eq!(
        untimed_sidekick_report(&interrupted),
        format!(
            "Sidekick Report from Suru: the Session \"Run the auth suite.\" you set to work has \
             settled its Turn, which was interrupted. Its session_id is {prompted}, which \
             read_session takes.\n\nIts Agent wrote no final Message in it."
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
async fn each_questionnaire_and_approval_a_session_it_set_to_work_comes_to_owe_is_reported_once() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-interventions").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, mut begun_provider) = sidekick.begin().await;

    let questionnaire = where_to_run();
    begun_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    let asked = sidekick
        .steered("the Questionnaire the Session asks is reported")
        .await;
    assert_eq!(
        asked,
        format!(
            "Sidekick Report from Suru: the Session \"{ASKED}\" you set to work asks a \
             Questionnaire, which waits on an Answer. Its session_id is {begun}: read_session \
             gives its Questions, and answer_questionnaire answers it."
        ),
        "the Report names the Intervention and the Tools that read and answer it"
    );

    // The Sidekick answers it as the Report says, and the Turn goes on.
    let (answered, _) = tokio::join!(
        acted(
            &mut sidekick.client,
            "answer_questionnaire",
            json!({
                "session_id": begun,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                begun_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Session's Provider")
        },
    );
    assert_eq!(answered["answered"], json!(true));

    begun_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        })
        .await;
    let approval = sidekick
        .steered("the Approval the Session asks is reported")
        .await;
    assert_eq!(
        approval,
        format!(
            "Sidekick Report from Suru: the Session \"{ASKED}\" you set to work asks an \
             Approval, which waits on the user's Decision. Its session_id is {begun}: \
             read_session says what it asks."
        )
    );

    fixes(&descriptor, begun, &begun_provider).await;
    let settled = sidekick
        .steered("the Turn settling is reported after its Interventions")
        .await;
    assert_eq!(
        untimed_sidekick_report(&settled),
        settled_report(begun, ASKED, "completed", FIXED)
    );
    sidekick.handed_nothing("each Intervention and the settled Turn are reported once");

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_that_answers_a_questionnaire_is_told_when_the_turn_it_answered_settles() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-answered").await;
    let descriptor = sidekick.descriptor.clone();
    let (asking, _, mut asking_provider) = sidekick.users_session("Run the auth suite.").await;
    // The Session asks before the Sidekick has any hand in it.
    let questionnaire = where_to_run();
    asking_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_until(
        &descriptor,
        asking,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Questionnaire {
                        outcome: suru::protocol::QuestionnaireOutcome::Pending,
                        ..
                    }
                )
            })
        },
    )
    .await;

    tokio::join!(
        acted(
            &mut sidekick.client,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["local"] }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Session's Provider")
        },
    );
    fixes(&descriptor, asking, &asking_provider).await;

    let report = sidekick
        .steered("the Turn the Sidekick answered is reported as it settles")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(asking, "Run the auth suite.", "completed", FIXED),
        "the first Report the Sidekick is given is of the Turn it answered, not of the \
         Questionnaire asked before it had a hand in the Session"
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
async fn a_questionnaire_a_subagent_asks_beneath_a_session_it_set_to_work_names_the_subagent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-subagent").await;
    let (prompted, handoff, mut prompted_provider) =
        sidekick.users_session("Run the auth suite.").await;
    assert_eq!(sidekick.prompt(prompted).await, json!("steer"));
    timeout(PROGRESS_DEADLINE, prompted_provider.next_steer())
        .await
        .expect("the Sidekick's Prompt steers the working Turn")
        .succeed();

    // The Session's Agent delegates to a brokered Subagent, which asks.
    let mut delegating = McpClient::handed(&handoff);
    delegating.initialize().await;
    let child = delegating
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) = run_child(&mut sidekick.hosted.codex, codex_selection("high")).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        })
        .await;

    let report = sidekick
        .steered("the Subagent's Questionnaire is reported")
        .await;
    assert_eq!(
        report,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"Run the auth suite.\" you \
             set to work asks a Questionnaire, which waits on an Answer. The Session's \
             session_id is {prompted}, and the Subagent's is {child}: given the Subagent's, \
             read_session gives its Questions, and answer_questionnaire answers it."
        ),
        "the work the Sidekick set going waits on it, so it is told, and where to answer"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_that_answers_a_subagents_questionnaire_is_told_of_that_subagents_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-subagent-answered").await;
    let descriptor = sidekick.descriptor.clone();
    let (delegating, handoff, _delegating_provider) =
        sidekick.users_session("Run the auth suite.").await;
    // The user's Session delegates to a brokered Subagent, which asks before
    // the Sidekick has any hand in either.
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let child = agent
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut sidekick.hosted.codex, codex_selection("high")).await;
    let questionnaire = where_to_run();
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;

    tokio::join!(
        acted(
            &mut sidekick.client,
            "answer_questionnaire",
            json!({
                "session_id": child,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                child_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Subagent's Provider")
        },
    );
    write_agent_message(&child_provider, FIXED).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, child, TurnStatus::Completed).await;

    let report = sidekick
        .steered("the Turn of the Subagent the Sidekick answered is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"Run the auth suite.\" you set \
             to work has settled its Turn, which completed. The Session's session_id is \
             {delegating}, and the Subagent's is {child}, which read_session \
             takes.\n\nIts Agent's final Message:\n\n{FIXED}"
        ),
        "the Report names the Session the Sidekick's tree lists and the Subagent it answered, \
         whose Turn it was, and no Report told of the Questionnaire asked before its Answer"
    );
    sidekick.handed_nothing("the Subagent's Turn is reported once");

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_is_told_nothing_of_a_session_it_only_read_listed_interrupted_set_aside_or_never_touched()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-untouched").await;
    let descriptor = sidekick.descriptor.clone();
    let (read, _, mut read_provider) = sidekick.users_session("Run the migrations.").await;
    let (untouched, _, mut untouched_provider) =
        sidekick.users_session("Update the changelog.").await;

    acted(&mut sidekick.client, "list_sessions", json!({})).await;
    acted(
        &mut sidekick.client,
        "read_session",
        json!({ "session_id": read }),
    )
    .await;
    acted(
        &mut sidekick.client,
        "settle_session",
        json!({ "session_id": read }),
    )
    .await;
    acted(
        &mut sidekick.client,
        "unsettle_session",
        json!({ "session_id": read }),
    )
    .await;
    // Questionnaires asked and Turns settled where it had no hand.
    read_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        })
        .await;
    untouched_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: run_the_suite(),
            tool_activity_id: None,
        })
        .await;
    let (interrupted, ()) = tokio::join!(
        acted(
            &mut sidekick.client,
            "interrupt_session",
            json!({ "session_id": read }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, read_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Session's Provider")
                .succeed();
        },
    );
    assert_eq!(interrupted["outcome"], json!("stopped_work"));
    read_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    latest_turn_settles(&descriptor, read, TurnStatus::Interrupted).await;
    fixes(&descriptor, untouched, &untouched_provider).await;
    sidekick.handed_nothing("nothing it only read, listed, interrupted or set aside is reported");

    // The first Report it is given is of the first Session it set to work.
    assert_eq!(sidekick.prompt(untouched).await, json!("new_turn"));
    timeout(PROGRESS_DEADLINE, untouched_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn")
        .succeed();
    write_agent_message(&untouched_provider, "The changelog is updated.").await;
    untouched_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let report = sidekick
        .steered("the Turn the Sidekick prompted is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(
            untouched,
            "Update the changelog.",
            "completed",
            "The changelog is updated."
        ),
        "the Sidekick hears first of the work it set going, and of nothing before it"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn reports_owed_or_held_are_lost_when_the_server_stops() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "sidekick-report-restart";
    let mut sidekick = sidekick(state_dir.path(), channel).await;
    let descriptor = sidekick.descriptor.clone();
    let (working, working_provider) = sidekick.begin().await;
    let (settling, settling_provider) = sidekick.begin().await;
    sidekick.idles().await;
    let Sidekick {
        hosted,
        id: sidekick_id,
        mut client,
        provider,
        ..
    } = sidekick;
    drop(provider);
    timeout(PROGRESS_DEADLINE, async {
        while client.initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Sidekick's Provider connection is gone");
    // One Report is held for the Sidekick's next Turn, and another is owed
    // by a Session still working, when the Server stops.
    fixes(&descriptor, settling, &settling_provider).await;
    hosted.server.shutdown().await.expect("shut down server");
    drop((working_provider, settling_provider));

    let mut restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    latest_turn_settles(&descriptor, working, TurnStatus::Failed).await;
    assert!(
        restarted.claude.try_next_start().is_none(),
        "no Report wakes the Sidekick after the restart"
    );
    admit_prompt(&descriptor, sidekick_id, "Where were we?").await;
    let mut relaunched = next_start(&mut restarted.claude)
        .await
        .succeed(AgentIdentity {
            agent: AgentId::new("claude-agent"),
            selection: default_selection(&claude_models()),
        });
    let turn = timeout(PROGRESS_DEADLINE, relaunched.next_turn())
        .await
        .expect("the Prompt's Turn reaches the Sidekick's Provider");
    assert!(
        turn.reports().is_empty(),
        "what was held or owed before the stop was lost with it: {:?}",
        turn.reports()
    );
    assert_eq!(turn.prompt(), "Where were we?");
    turn.succeed();

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_whose_answer_was_not_delivered_is_owed_nothing_of_the_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-refused-answer").await;
    let descriptor = sidekick.descriptor.clone();
    let (asking, _, mut asking_provider) = sidekick.users_session("Run the auth suite.").await;
    let questionnaire = where_to_run();
    asking_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_until(
        &descriptor,
        asking,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Questionnaire {
                        outcome: suru::protocol::QuestionnaireOutcome::Pending,
                        ..
                    }
                )
            })
        },
    )
    .await;

    // The Session's Provider refuses the Sidekick's Answer.
    asking_provider.gate_questionnaire_deliveries();
    let (refusal, ()) = tokio::join!(
        refused(
            &mut sidekick.client,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["local"] }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_delivery(),
            )
            .await
            .expect("the Answer reaches the Session's Provider")
            .reject();
        },
    );
    assert!(
        refusal.contains("The Answer was not delivered"),
        "the Sidekick is told its Answer did not take: {refusal}"
    );
    fixes(&descriptor, asking, &asking_provider).await;

    // The first Report it is given is of the next Session it sets to work.
    let (begun, begun_provider) = sidekick.begin().await;
    fixes(&descriptor, begun, &begun_provider).await;
    let report = sidekick
        .steered("the Session the Sidekick began is reported")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_report(begun, ASKED, "completed", FIXED),
        "an Answer that did not take gave the Sidekick no hand in the Turn it was for"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_whose_session_is_deleted_is_told_nothing_more_and_nothing_starts_to_tell_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut sidekick = sidekick(state_dir.path(), "sidekick-report-deleted").await;
    let descriptor = sidekick.descriptor.clone();
    let (begun, mut begun_provider) = sidekick.begin().await;
    sidekick.idles().await;

    let deleted = reqwest::Client::new()
        .delete(format!(
            "{}/v1/sessions/{}",
            descriptor.base_url, sidekick.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the deletion");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    begun_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: where_to_run(),
        })
        .await;
    fixes(&descriptor, begun, &begun_provider).await;

    assert!(
        sidekick.hosted.claude.try_next_start().is_none(),
        "no Provider is started for a Sidekick that is gone"
    );
    sidekick.handed_nothing("a deleted Sidekick's Provider is handed nothing");
    assert!(
        begun_provider.try_next_steer().is_none() && begun_provider.try_next_turn().is_none(),
        "and the Session it began works on as the user's own"
    );

    sidekick
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
