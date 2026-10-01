//! `read_session`: a Sidekick reading one Session on its own Server as
//! compact text — how it stands, what in it waits on the user, and what its
//! Transcript says — the user's Sessions, a Subagent's, another Sidekick's,
//! and one persisted before the Server last started with no Provider running
//! for it alike. Any other caller neither lists it nor may call it.
//!
//! Each test acts as the MCP client a Sidekick's harness is and asserts on
//! what `read_session` answers it, and on what the Session API says after.

use suru::provider::{ProviderActivityId, ProviderCommandStatus};

use super::*;

/// `read_session`'s answer to `arguments`, read from the structured content
/// the call carries.
async fn reads(client: &mut McpClient, arguments: Value) -> Value {
    let result = client.call_tool("read_session", arguments).await;
    assert_ne!(
        result["isError"],
        json!(true),
        "read_session answers: {result}"
    );
    result["structuredContent"].clone()
}

/// The transcript a reading holds.
fn transcript(reading: &Value) -> &str {
    reading["transcript"]
        .as_str()
        .expect("a reading holds a transcript")
}

/// Has `provider` settle its working Turn as completed, and waits until
/// `session_id`'s latest Turn has.
async fn complete_turn(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
) {
    provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(descriptor, session_id, TurnStatus::Completed).await;
}

/// Admits `text` to the idle `session_id` and has `provider` take up the Turn
/// it begins.
async fn next_turn(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &mut ControlledProviderSession,
    text: &str,
) {
    admit_prompt(descriptor, session_id, text).await;
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Turn reaches the Provider")
        .succeed();
}

/// Has `provider` think `text` aloud as a block of Reasoning.
async fn reason(provider: &ControlledProviderSession, text: &str) {
    let activity_id = ProviderActivityId::new("thinking");
    for event in [
        ProviderEvent::ReasoningStarted {
            activity_id: activity_id.clone(),
        },
        ProviderEvent::ReasoningTitleChanged {
            activity_id: activity_id.clone(),
            title: "Secret plans".to_owned(),
        },
        ProviderEvent::ReasoningDelta {
            activity_id: activity_id.clone(),
            content: text.to_owned(),
        },
        ProviderEvent::ReasoningCompleted { activity_id },
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }
}

/// Has `provider` run `command`, which writes `output` and exits with
/// `exit_status`.
async fn run_command(
    provider: &ControlledProviderSession,
    command: &str,
    output: &str,
    exit_status: i32,
) {
    let activity_id = ProviderActivityId::new(command);
    for event in [
        ProviderEvent::CommandStarted {
            activity_id: activity_id.clone(),
            command: command.to_owned(),
            cwd: None,
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: activity_id.clone(),
            content: output.to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id,
            status: ProviderCommandStatus::Completed,
            exit_status: Some(exit_status),
        },
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }
}

#[tokio::test]
async fn read_session_reads_the_latest_turns_ask_and_answer_by_default_and_reads_back_on_request() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-defaults",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let workspace = tempfile::tempdir().expect("create the Workspace read");
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    let (session_id, mut provider) = started_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Look at the ledger.",
    )
    .await;
    write_agent_message(&provider, "The ledger has a flaky test.").await;
    complete_turn(&descriptor, session_id, &provider).await;
    next_turn(&descriptor, session_id, &mut provider, "Fix it.").await;
    reason(&provider, "Nobody but the user's screen sees this.").await;
    write_agent_message(&provider, "Let me run the tests first.").await;
    run_command(
        &provider,
        "cargo test -p ledger",
        "test sync ... FAILED",
        101,
    )
    .await;
    write_agent_message(&provider, "Fixed the race in sync; the tests pass.").await;
    complete_turn(&descriptor, session_id, &provider).await;

    let reading = reads(&mut sidekick, json!({ "session_id": session_id })).await;
    assert_eq!(reading["session_id"], json!(session_id));
    assert_eq!(
        reading["workspace"],
        json!(suru::paths::canonical(workspace.path()).expect("read the Workspace"))
    );
    assert_eq!(reading["parent"], Value::Null);
    assert_eq!(reading["status"], json!("idle"));
    assert_eq!(reading["standing"], json!("done"));
    assert_eq!(reading["questionnaires"], json!([]));
    assert_eq!(reading["approvals"], json!([]));
    let text = transcript(&reading);
    assert!(
        text.starts_with("[Turn 2 of 2 · completed at ")
            && text.contains(" · 1 agent Message and 1 Activity not shown]\n"),
        "{text}"
    );
    assert!(
        text.ends_with("\n2.1 user: Fix it.\n2.4 agent: Fixed the race in sync; the tests pass."),
        "the latest Turn's user Message and final agent Message: {text}"
    );
    assert!(text.chars().count() <= 2_000);
    assert_eq!(reading["before"], json!("2"));
    assert_eq!(
        reading["earlier"],
        json!("Left out before this: Turn 1. Call again with before \"2\" to read on.")
    );

    let both = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "turns": 2 }),
    )
    .await;
    assert!(
        transcript(&both).contains(
            "\n1.1 user: Look at the ledger.\n1.2 agent: The ledger has a flaky test.\n[Turn 2 of 2"
        ),
        "{both}"
    );
    assert_eq!(
        (&both["before"], &both["earlier"]),
        (&Value::Null, &Value::Null)
    );
    let earlier = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "before": "2" }),
    )
    .await;
    assert!(
        transcript(&earlier).starts_with("[Turn 1 of 2"),
        "{earlier}"
    );
    assert!(!transcript(&earlier).contains("Turn 2"), "{earlier}");

    let activities = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "detail": "activities" }),
    )
    .await;
    assert!(
        transcript(&activities).ends_with(
            "\n2.1 user: Fix it.\n2.2 agent: Let me run the tests first.\n2.3 command [completed, \
             exit 101]: cargo test -p ledger\n2.4 agent: Fixed the race in sync; the tests pass."
        ),
        "{activities}"
    );
    assert!(!transcript(&activities).contains("FAILED"), "no output");
    let command = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "item": "2.3" }),
    )
    .await;
    assert_eq!(
        transcript(&command),
        "2.3 command [completed, exit 101]: cargo test -p ledger\noutput:\ntest sync ... FAILED"
    );
    for read in [&reading, &both, &earlier, &activities, &command] {
        assert!(
            !transcript(read).contains("Secret plans") && !transcript(read).contains("Nobody"),
            "Reasoning never appears: {read}"
        );
    }

    // A third Turn's answer runs past the cap: the read keeps its end, says
    // where it was cut, and reading on from there gives the rest.
    next_turn(
        &descriptor,
        session_id,
        &mut provider,
        "Explain everything.",
    )
    .await;
    let long = (0..300)
        .map(|line| format!("Line {line} of the explanation 🦀."))
        .collect::<Vec<_>>()
        .join("\n");
    write_agent_message(&provider, &long).await;
    complete_turn(&descriptor, session_id, &provider).await;
    let cut = reads(&mut sidekick, json!({ "session_id": session_id })).await;
    assert!(transcript(&cut).chars().count() <= 2_000);
    let (_, kept) = transcript(&cut)
        .split_once("\n3.2 agent: […]")
        .expect("the final Message lost its start to the cap");
    assert!(long.ends_with(kept));
    let before = cut["before"]
        .as_str()
        .expect("the cut is stated")
        .to_owned();
    assert_eq!(
        before,
        format!("3.2.{}", long.chars().count() - kept.chars().count())
    );
    assert!(
        cut["earlier"]
            .as_str()
            .is_some_and(|earlier| earlier.contains(&format!("before \"{before}\""))),
        "{cut}"
    );
    let rest = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "before": before, "max_chars": 100_000 }),
    )
    .await;
    let (_, head) = transcript(&rest)
        .split_once("\n3.2 agent: ")
        .expect("the rest of the Message");
    assert_eq!(format!("{head}{kept}"), long);
    let whole = reads(
        &mut sidekick,
        json!({ "session_id": session_id, "item": "3.2" }),
    )
    .await;
    assert_eq!(transcript(&whole), format!("3.2 agent: {long}"));

    assert_eq!(
        list_sessions(&mut sidekick, json!({ "standing": "done" })).await["sessions"][0]["session_id"],
        json!(session_id),
        "reading a Session is no Viewed report, so its Standing reads as it did"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn read_session_gives_an_open_questionnaire_whole_and_a_pending_approval_as_awaiting_the_user()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-interventions",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let workspace = tempfile::tempdir().expect("create the Workspace read");
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    let (asking, asking_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Run the tests.").await;
    let questionnaire = suru::protocol::Questionnaire {
        id: suru::protocol::QuestionnaireId::new(),
        questions: vec![suru::questionnaire::Question {
            id: "machine".to_owned(),
            title: Some("Machine".to_owned()),
            text: "Where should the tests run?".to_owned(),
            choices: vec![
                suru::questionnaire::QuestionChoice {
                    id: "staging".to_owned(),
                    label: "Staging".to_owned(),
                    description: Some("The shared machine".to_owned()),
                    recommended: true,
                },
                suru::questionnaire::QuestionChoice {
                    id: "local".to_owned(),
                    label: "This machine".to_owned(),
                    description: None,
                    recommended: false,
                },
            ],
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    };
    asking_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        asking,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Questionnaire { .. }))
        },
    )
    .await;

    let reading = reads(&mut sidekick, json!({ "session_id": asking })).await;
    assert_eq!(reading["status"], json!("active"));
    assert_eq!(reading["standing"], json!("needs_intervention"));
    assert_eq!(
        reading["questionnaires"],
        json!([{
            "id": questionnaire.id,
            "item": "1.2",
            "questions": [{
                "id": "machine",
                "title": "Machine",
                "text": "Where should the tests run?",
                "choices": [
                    {
                        "id": "staging",
                        "label": "Staging",
                        "description": "The shared machine",
                        "recommended": true,
                    },
                    {
                        "id": "local",
                        "label": "This machine",
                        "description": null,
                        "recommended": false,
                    },
                ],
                "multiple": false,
                "freeform": true,
                "combine_freeform": false,
                "secret": false,
                "required": true,
            }],
        }]),
        "the open Questionnaire comes whole, with its identity for an Answer to name"
    );
    assert_eq!(reading["approvals"], json!([]));

    let (approving, approving_provider) =
        started_session(&descriptor, &mut claude, workspace.path(), "Build it.").await;
    approving_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: suru::protocol::Approval {
                id: suru::protocol::ApprovalId::new(),
                subject: suru::protocol::ApprovalSubject::Command {
                    command: "cargo nextest run".into(),
                    cwd: None,
                    actions: Vec::new(),
                },
                reason: Some("The tests need the network.".to_owned()),
            },
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

    let reading = reads(&mut sidekick, json!({ "session_id": approving })).await;
    assert_eq!(reading["standing"], json!("needs_intervention"));
    assert_eq!(reading["questionnaires"], json!([]));
    assert_eq!(
        reading["approvals"],
        json!([
            "Approval 1.2 awaits the user's Decision on whether the Agent may run `cargo nextest \
             run`, saying: The tests need the network.. Only the user can decide it, so tell them \
             it is waiting on them."
        ])
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_reads_a_subagents_session_and_another_sidekicks() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-read-anyone", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (other_sidekick_id, mut other_sidekick, _other_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let subagent_id = sidekick
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around the ledger.",
        }))
        .await;
    let start = next_start(&mut hosted.codex).await;
    let mut subagent_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: default_selection(&codex_models()),
    });
    timeout(PROGRESS_DEADLINE, subagent_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    write_agent_message(&subagent_provider, "Nothing amiss.").await;
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        subagent_id,
        "the Subagent's Message is recorded",
        |snapshot| {
            snapshot
                .messages
                .iter()
                .any(|message| message.content == "Nothing amiss.")
        },
    )
    .await;

    let reading = reads(&mut sidekick, json!({ "session_id": subagent_id })).await;
    assert_eq!(reading["parent"], json!(sidekick_id));
    assert_eq!(reading["status"], json!("active"));
    assert!(
        transcript(&reading)
            .ends_with("\n1.1 delegation: Look around the ledger.\n1.2 agent: Nothing amiss."),
        "a Subagent's Session reads its Delegation and what it wrote: {reading}"
    );

    let reading = reads(&mut sidekick, json!({ "session_id": other_sidekick_id })).await;
    assert_eq!(reading["parent"], Value::Null);
    assert_eq!(reading["standing"], json!("working"));
    assert!(
        transcript(&reading).ends_with("\n1.1 user: Plan the work"),
        "another Sidekick's Session is read like any other: {reading}"
    );
    let own = reads(
        &mut other_sidekick,
        json!({ "session_id": other_sidekick_id }),
    )
    .await;
    assert_eq!(
        transcript(&own),
        transcript(&reading),
        "and a Sidekick may read its own"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_session_persisted_before_a_restart_reads_with_no_provider_running_for_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create the Workspace read");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-restored",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (session_id, provider) = started_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Look at the ledger.",
    )
    .await;
    write_agent_message(&provider, "The ledger has a flaky test.").await;
    complete_turn(&descriptor, session_id, &provider).await;
    drop(provider);
    server.shutdown().await.expect("stop the server");

    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-restored",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    let reading = reads(&mut sidekick, json!({ "session_id": session_id })).await;
    assert_eq!(reading["status"], json!("idle"));
    assert!(
        transcript(&reading)
            .ends_with("\n1.1 user: Look at the ledger.\n1.2 agent: The ledger has a flaky test."),
        "{reading}"
    );
    assert!(
        claude.try_next_start().is_none(),
        "reading it started no Provider for it"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn read_session_is_offered_to_a_sidekick_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-read-by-caller", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (ordinary_id, ordinary_handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&ordinary_handoff);
    ordinary.initialize().await;
    sidekick
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around.",
        }))
        .await;
    // Held, so the Subagent's Provider start stays pending and its token live.
    let subagent_start = next_start(&mut hosted.codex).await;
    let subagent_handoff = subagent_start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut subagent = McpClient::handed(&subagent_handoff);
    subagent.initialize().await;

    assert!(
        listed_tools(&mut sidekick)
            .await
            .contains(&"read_session".to_owned())
    );
    reads(&mut sidekick, json!({ "session_id": ordinary_id })).await;
    for (caller, client) in [
        ("another Session", &mut ordinary),
        ("a Sidekick's Subagent", &mut subagent),
    ] {
        assert!(
            !listed_tools(client)
                .await
                .contains(&"read_session".to_owned()),
            "{caller} is not offered read_session"
        );
        let refused = unoffered(client, "read_session", json!({ "session_id": ordinary_id })).await;
        assert_eq!(
            refused["message"],
            json!("The Broker offers no Tool named `read_session`"),
            "{caller} is answered as though the Tool did not exist: {refused}"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn read_session_refuses_what_the_session_does_not_hold_in_words_the_sidekick_can_act_on() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-refusals",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let unknown = SessionId::new();

    for (arguments, says) in [
        (
            json!({ "session_id": unknown }),
            format!("Suru holds no Session `{unknown}`"),
        ),
        (json!({}), "needs `session_id`".to_owned()),
        (
            json!({ "session_id": sidekick_id, "origin": "studio" }),
            "takes no argument `origin`".to_owned(),
        ),
        (
            json!({ "session_id": sidekick_id, "before": "9" }),
            "names Turn 9, but the Session has 1 Turn".to_owned(),
        ),
        (
            json!({ "session_id": sidekick_id, "item": "1.9" }),
            "names 1.9, but Turn 1 holds 1 entry".to_owned(),
        ),
        (
            json!({ "session_id": sidekick_id, "max_chars": 10 }),
            "at least 500".to_owned(),
        ),
        (
            json!({ "session_id": sidekick_id, "detail": "everything" }),
            "`messages` or `activities`".to_owned(),
        ),
        (
            json!({ "session_id": sidekick_id, "item": "1.1", "before": "1" }),
            "takes no `before` beside it".to_owned(),
        ),
    ] {
        // Each refusal is the Tool's own answer, `isError` and a sentence to
        // relay, never a JSON-RPC error the transport raises.
        let refusal = sidekick.refusal("read_session", arguments.clone()).await;
        assert!(
            refusal.contains(&says),
            "{arguments} is refused saying {says:?}: {refusal}"
        );
    }

    server.shutdown().await.expect("shut down server");
}
