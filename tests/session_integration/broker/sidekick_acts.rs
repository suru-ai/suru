//! A Sidekick acting on the Sessions on its own Server: `send_prompt`,
//! `interrupt_session`, `settle_session` and `unsettle_session`, each the very
//! act a Client performs through the Session API, so what it does and every
//! refusal it meets are a Client's (ADR 0043).
//!
//! A Prompt a Sidekick sent says so as typed data naming the Sidekick's
//! Session, on the Prompt and on the Message it becomes, in what the Session
//! API answers, what it streams to every Client, and what it keeps across a
//! restart. No Sidekick acts on a Session of the Sidekick Workspace, its own
//! included, though it may read one.
//!
//! Each test acts as the MCP client a Provider harness is, as the rest of the
//! Broker suite does, and asserts on what that client and the Session API
//! observe.

use suru::protocol::{Author, PromptStatus, SessionChange, SessionError, SettleSessionRequest};

use super::{
    sidekick::{
        latest_turn_settles, list_sessions, start_sidekick, started_session, titles, unoffered,
        working_session,
    },
    *,
};
use crate::attachments::{next_change, watch_session};

/// The Tools through which a Sidekick acts on a Session.
const ACTING_TOOLS: [&str; 4] = [
    "send_prompt",
    "interrupt_session",
    "settle_session",
    "unsettle_session",
];

/// What every act a Sidekick sends to a Session of the Sidekick Workspace is
/// refused with.
pub(super) const SIDEKICK_WORKSPACE_REFUSAL: &str = "The Session is one of the Sidekick Workspace's, and no \
    Sidekick acts on a Session there, its own included, though it may read one.";

/// The answer a Tool gave the Sidekick, read from the structured content the
/// call carries, having checked it is no refusal.
pub(super) async fn acted(client: &mut McpClient, tool: &str, arguments: Value) -> Value {
    let result = client.call_tool(tool, arguments).await;
    assert_ne!(result["isError"], json!(true), "{tool} answers: {result}");
    result["structuredContent"].clone()
}

/// The words a Tool refused the Sidekick with, having checked the refusal is
/// the Tool's own error rather than the transport's.
pub(super) async fn refused(client: &mut McpClient, tool: &str, arguments: Value) -> String {
    let result = client.call_tool(tool, arguments).await;
    assert_eq!(
        result["isError"],
        json!(true),
        "{tool} refuses as the Tool's own error: {result}"
    );
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal is given in words: {result}"))
        .to_owned()
}

/// The author a Sidekick's act names: its own Session, by the Title that
/// Session began with.
pub(super) fn sidekick_author(sidekick: SessionId) -> Author {
    Author::Sidekick {
        session_id: sidekick,
        title: "Plan the work".to_owned(),
    }
}

/// The words the Session API refuses a Client's request with.
pub(super) async fn refused_over_http(response: reqwest::Response) -> String {
    assert!(
        response.status().is_client_error(),
        "the Session API refuses the request: {}",
        response.status()
    );
    response
        .json::<SessionError>()
        .await
        .expect("decode the Session error")
        .message
}

/// A Client's admission of `text` to `session_id`, as the composer sends one.
async fn client_admits(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    text: &str,
) -> reqwest::Response {
    reqwest::Client::new()
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
        .expect("send the admission")
}

/// A Client's interrupt of `session_id`.
async fn client_interrupts(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the interrupt")
}

/// A Client's settling of `session_id`, or bringing it back.
async fn client_settles(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    settled: bool,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/settlement",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&SettleSessionRequest { settled })
        .send()
        .await
        .expect("send the settlement")
}

/// The user Messages a Session's Transcript holds with `content`.
fn messages_saying<'a>(
    snapshot: &'a SessionSnapshot,
    content: &str,
) -> Vec<&'a suru::protocol::Message> {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User && message.content == content)
        .collect()
}

/// The Prompt a Session holds with `text`.
fn prompt_saying<'a>(snapshot: &'a SessionSnapshot, text: &str) -> &'a suru::protocol::Prompt {
    snapshot
        .prompts
        .iter()
        .find(|prompt| prompt.text == text)
        .unwrap_or_else(|| panic!("the Session holds the Prompt {text:?}"))
}

#[tokio::test]
async fn a_sidekick_is_offered_the_acting_tools_and_any_other_agent_is_not() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-acting-tools", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
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

    let listed = sidekick.request("tools/list", json!({})).await;
    for tool in ACTING_TOOLS {
        let described = listed["tools"]
            .as_array()
            .expect("tools/list lists Tools")
            .iter()
            .find(|described| described["name"] == json!(tool))
            .unwrap_or_else(|| panic!("a Sidekick is offered {tool}: {listed}"));
        assert_eq!(
            described["annotations"]["readOnlyHint"],
            json!(false),
            "{tool} changes what Suru holds: {described}"
        );
        assert_eq!(
            described["inputSchema"]["required"][0],
            json!("session_id"),
            "{tool} names the Session it acts on: {described}"
        );

        let refused = unoffered(&mut ordinary, tool, json!({ "session_id": ordinary_id })).await;
        assert_eq!(
            refused["message"],
            json!(format!("The Broker offers no Tool named `{tool}`")),
            "any other caller is answered as though {tool} did not exist: {refused}"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn send_prompt_begins_a_turn_in_an_idle_session_whose_message_names_the_sidekick() {
    const ASKED: &str = "Pick the parser back up where it stopped.";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-send-idle", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, mut target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    target_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;
    let (_, mut watched) = watch_session(&descriptor, target).await;

    assert_eq!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": ASKED }),
        )
        .await,
        json!({ "session_id": target, "admitted": "new_turn" }),
        "an idle Session takes the Prompt as a Turn of its own"
    );
    let turn = timeout(PROGRESS_DEADLINE, target_provider.next_turn())
        .await
        .expect("the Prompt begins a Turn with the Session's Provider");
    assert_eq!(
        turn.prompt(),
        ASKED,
        "the Agent is asked what the Sidekick sent"
    );
    turn.succeed();

    let streamed = next_change(&mut watched, "the Sidekick's Message", |change| {
        matches!(change, SessionChange::MessageAdded { message } if message.content == ASKED)
    })
    .await;
    let SessionChange::MessageAdded { message } = streamed else {
        unreachable!("the change was found as a Message");
    };
    assert_eq!(
        message.author,
        Some(sidekick_author(sidekick_id)),
        "every Client watching the Session is sent the Message naming the Sidekick"
    );

    let snapshot = read_session(&descriptor, target).await;
    assert_eq!(
        prompt_saying(&snapshot, ASKED).author,
        Some(sidekick_author(sidekick_id)),
        "the Prompt names the Sidekick that sent it"
    );
    let sent = messages_saying(&snapshot, ASKED);
    assert_eq!(sent.len(), 1, "{:?}", snapshot.messages);
    assert_eq!(sent[0].author, Some(sidekick_author(sidekick_id)));
    assert_eq!(
        messages_saying(&snapshot, "Write the parser")[0].author,
        None,
        "the user's own Message names no one"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn send_prompt_steers_a_working_turn_unless_asked_to_queue() {
    const STEERED: &str = "Cover the empty input as well.";
    const QUEUED: &str = "Then write the changelog entry.";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-send-working", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, mut target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;

    assert_eq!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": STEERED }),
        )
        .await,
        json!({ "session_id": target, "admitted": "steer" }),
        "a Prompt steers the working Turn unless asked otherwise"
    );
    let steer = timeout(PROGRESS_DEADLINE, target_provider.next_steer())
        .await
        .expect("the Prompt is steered into the working Turn");
    assert_eq!(steer.prompt(), STEERED);
    steer.succeed();
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        target,
        "the steer stands in the working Turn",
        |snapshot| !messages_saying(snapshot, STEERED).is_empty(),
    )
    .await;
    let steered = messages_saying(&snapshot, STEERED)[0];
    assert_eq!(steered.turn_id, snapshot.turns[0].id, "it joins the Turn");
    assert_eq!(steered.author, Some(sidekick_author(sidekick_id)));

    assert_eq!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": QUEUED, "delivery": "queue" }),
        )
        .await,
        json!({ "session_id": target, "admitted": "queued" }),
    );
    let snapshot = read_session(&descriptor, target).await;
    let queued = prompt_saying(&snapshot, QUEUED);
    assert_eq!(
        (queued.delivery, queued.status),
        (PromptDelivery::Queue, PromptStatus::Pending),
        "a queued Prompt waits behind the working Turn"
    );
    assert_eq!(queued.author, Some(sidekick_author(sidekick_id)));
    assert!(
        target_provider.try_next_steer().is_none(),
        "and is not steered in"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupt_session_says_whether_it_stopped_work_or_withdrew_a_prompt() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-interrupt", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let (working, mut working_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    let (answer, ()) = tokio::join!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": working }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, working_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Session's Provider")
                .succeed();
        },
    );
    assert_eq!(
        answer,
        json!({ "session_id": working, "outcome": "stopped_work" }),
        "interrupting a working Turn stops its work"
    );

    let (idle, idle_provider) =
        started_session(&descriptor, &mut hosted.claude, &workspace, "Tidy the docs").await;
    idle_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, idle, TurnStatus::Completed).await;
    let refusal = refused(
        &mut sidekick,
        "interrupt_session",
        json!({ "session_id": idle }),
    )
    .await;
    assert_eq!(
        refusal,
        "The Session has no active Turn, no working Subagent, and no live Watch, so there is \
         nothing to interrupt.",
    );
    assert_eq!(
        refused_over_http(client_interrupts(&descriptor, idle).await).await,
        refusal,
        "a Sidekick is refused in the words a Client is"
    );

    // A Session whose Provider is still starting is Working only because its
    // first Prompt waits to begin a Turn, so interrupting it withdraws that
    // Prompt (ADR 0024).
    let waiting = working_session(&descriptor, &workspace, "Untangle the build").await;
    let _starting = next_start(&mut hosted.claude).await;
    assert_eq!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": waiting }),
        )
        .await,
        json!({
            "session_id": waiting,
            "outcome": "withdrew_prompt",
            "prompt": "Untangle the build",
        }),
        "interrupting a Session owed a Turn withdraws the Prompt, saying which"
    );
    let snapshot = read_session(&descriptor, waiting).await;
    assert_eq!(
        prompt_saying(&snapshot, "Untangle the build").status,
        PromptStatus::Cancelled
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn settle_session_sets_a_session_aside_and_unsettle_session_brings_it_back() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-settle", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    target_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;

    assert_eq!(
        acted(
            &mut sidekick,
            "settle_session",
            json!({ "session_id": target }),
        )
        .await,
        json!({ "session_id": target, "settled": true }),
    );
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "liveness": "settled" })).await),
        ["Write the parser"],
        "the Session is listed as set aside"
    );
    assert_eq!(
        acted(
            &mut sidekick,
            "settle_session",
            json!({ "session_id": target }),
        )
        .await,
        json!({ "session_id": target, "settled": true }),
        "settling a settled Session leaves it so"
    );

    assert_eq!(
        acted(
            &mut sidekick,
            "unsettle_session",
            json!({ "session_id": target }),
        )
        .await,
        json!({ "session_id": target, "settled": false }),
    );
    assert!(
        titles(&list_sessions(&mut sidekick, json!({})).await).contains(&"Write the parser"),
        "the Session is listed among the active ones again"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// No Sidekick acts on a Session of the Sidekick Workspace — its own, or
/// another Sidekick's — so none sets another to work; it may read them all
/// the same, and the user may act on them as on any other.
#[tokio::test]
async fn no_act_reaches_a_session_of_the_sidekick_workspace_though_reading_one_does() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-refusals", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (own, mut sidekick, _own_provider) = start_sidekick(&descriptor, &mut hosted.claude).await;
    let (other, _other_client, mut other_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let before = read_session(&descriptor, other).await;

    for target in [own, other] {
        for (tool, arguments) in [
            (
                "send_prompt",
                json!({ "session_id": target, "prompt": "Take over from me." }),
            ),
            (
                "send_prompt",
                json!({ "session_id": target, "prompt": "Later.", "delivery": "queue" }),
            ),
            ("interrupt_session", json!({ "session_id": target })),
            ("settle_session", json!({ "session_id": target })),
            ("unsettle_session", json!({ "session_id": target })),
        ] {
            assert_eq!(
                refused(&mut sidekick, tool, arguments).await,
                SIDEKICK_WORKSPACE_REFUSAL,
                "{tool} is refused for a Session of the Sidekick Workspace"
            );
        }
    }
    assert!(
        timeout(Duration::from_millis(50), other_provider.next_interrupt())
            .await
            .is_err(),
        "the other Sidekick's Provider is never told to stop"
    );
    let after = read_session(&descriptor, other).await;
    assert_eq!(
        (after.prompts, after.messages, after.turns),
        (before.prompts, before.messages, before.turns),
        "nothing reached the other Sidekick's Session"
    );

    let listed = list_sessions(&mut sidekick, json!({ "liveness": "all" })).await;
    let listed_ids = listed["sessions"]
        .as_array()
        .expect("a listing lists Sessions")
        .iter()
        .map(|row| row["session_id"].clone())
        .collect::<Vec<_>>();
    assert!(
        listed_ids.contains(&json!(own)) && listed_ids.contains(&json!(other)),
        "reading the Sidekick Workspace's Sessions is no act: {listed}"
    );
    let settled = client_settles(&descriptor, other, true).await;
    assert!(
        settled.status().is_success(),
        "the user settles a Sidekick's Session as any other: {}",
        settled.status()
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subagents_session_refuses_a_sidekicks_prompt_as_it_refuses_a_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-subagent-prompt", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
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
    let subagent = parent
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around.",
        }))
        .await;
    next_start(&mut hosted.codex).await;

    let refusal = refused(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": subagent, "prompt": "Look elsewhere." }),
    )
    .await;
    assert_eq!(
        refusal,
        "A Subagent's Session refuses Prompts; it is sent Delegations by the Agent that \
         delegated to it."
    );
    assert_eq!(
        refused_over_http(client_admits(&descriptor, subagent, "Look elsewhere.").await).await,
        refusal,
        "a Sidekick is refused in the words a Client is"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// Every refusal is the Tool's own error in a sentence the Sidekick can
/// relay — never the transport's — and an act's refusal is the very one a
/// Client's request meets.
#[tokio::test]
async fn every_refusal_is_a_tool_error_in_words_the_sidekick_can_relay() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-act-refusals", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, _target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    let missing = SessionId::new();

    let not_found = "The Session does not exist on this Suru server.";
    for (tool, arguments, client) in [
        (
            "send_prompt",
            json!({ "session_id": missing, "prompt": "Go on." }),
            client_admits(&descriptor, missing, "Go on.").await,
        ),
        (
            "interrupt_session",
            json!({ "session_id": missing }),
            client_interrupts(&descriptor, missing).await,
        ),
        (
            "settle_session",
            json!({ "session_id": missing }),
            client_settles(&descriptor, missing, true).await,
        ),
        (
            "unsettle_session",
            json!({ "session_id": missing }),
            client_settles(&descriptor, missing, false).await,
        ),
    ] {
        let refusal = refused(&mut sidekick, tool, arguments).await;
        assert_eq!(refusal, not_found, "{tool}");
        assert_eq!(
            refused_over_http(client).await,
            refusal,
            "{tool} is refused in the words a Client is"
        );
    }

    assert_eq!(
        refused(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": "   " }),
        )
        .await,
        "A Prompt must contain text other than whitespace.",
    );
    assert_eq!(
        refused_over_http(client_admits(&descriptor, target, "   ").await).await,
        "A Prompt must contain text other than whitespace.",
    );
    for tool in ACTING_TOOLS {
        assert_eq!(
            refused(&mut sidekick, tool, json!({})).await,
            format!("{tool} needs `session_id`, the id of a Session as list_sessions gives it."),
        );
        assert_eq!(
            refused(
                &mut sidekick,
                tool,
                json!({ "session_id": "the parser one" })
            )
            .await,
            format!(
                "{tool}'s `session_id` must be the id of a Session as list_sessions gives it; \
                 \"the parser one\" is not one."
            ),
        );
    }
    assert_eq!(
        refused(
            &mut sidekick,
            "settle_session",
            json!({ "session_id": target, "settled": true }),
        )
        .await,
        "settle_session takes no argument `settled`; it takes `session_id`, `origin`.",
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// The author is part of what Suru keeps of a Prompt and its Message, so a
/// Session read after a restart still says which words were the Sidekick's.
#[tokio::test]
async fn a_sidekicks_prompt_and_its_message_keep_their_author_across_a_restart() {
    const ASKED: &str = "Pick the parser back up where it stopped.";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-author-kept").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (target, mut target_provider) = started_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Write the parser",
    )
    .await;
    target_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;
    acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": target, "prompt": ASKED }),
    )
    .await;
    timeout(PROGRESS_DEADLINE, target_provider.next_turn())
        .await
        .expect("the Prompt begins a Turn")
        .succeed();
    target_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;
    drop(target_provider);
    server.shutdown().await.expect("stop the server");

    let (server, _claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-author-kept").await;
    let descriptor = server.descriptor().clone();
    let snapshot = read_session(&descriptor, target).await;
    assert_eq!(
        prompt_saying(&snapshot, ASKED).author,
        Some(sidekick_author(sidekick_id))
    );
    assert_eq!(
        messages_saying(&snapshot, ASKED)[0].author,
        Some(sidekick_author(sidekick_id))
    );

    server.shutdown().await.expect("shut down server");
}

/// The Prompt `session_id` holds with `text`, promoted to steer the working
/// Turn as a Client's queue does it.
async fn client_promotes(descriptor: &RuntimeDescriptor, session_id: SessionId, text: &str) {
    let prompt_id = prompt_saying(&read_session(descriptor, session_id).await, text).id;
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/prompts/{prompt_id}/promote",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the promotion")
        .error_for_status()
        .expect("the queued Prompt is promoted");
}

/// The Sidekick's Prompts outlive the queue they waited in: one the user
/// promoted steers the working Turn, one left queued begins the next, and the
/// Message each becomes names the Sidekick — to a Client watching all along,
/// and to one that attaches only afterwards.
#[tokio::test]
async fn a_queued_sidekick_prompt_promoted_or_delivered_becomes_a_message_naming_the_sidekick() {
    const PROMOTED: &str = "Cover the empty input as well.";
    const QUEUED: &str = "Then write the changelog entry.";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-queued-delivered", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, mut target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    for text in [PROMOTED, QUEUED] {
        assert_eq!(
            acted(
                &mut sidekick,
                "send_prompt",
                json!({ "session_id": target, "prompt": text, "delivery": "queue" }),
            )
            .await,
            json!({ "session_id": target, "admitted": "queued" }),
        );
    }
    client_promotes(&descriptor, target, PROMOTED).await;
    assert_eq!(
        prompt_saying(&read_session(&descriptor, target).await, PROMOTED).author,
        Some(sidekick_author(sidekick_id)),
        "promoting a Prompt leaves whose it is alone"
    );

    target_provider.emit(ProviderEvent::TurnCompleted);
    let next = timeout(PROGRESS_DEADLINE, target_provider.next_turn())
        .await
        .expect("the queued Prompt begins the next Turn");
    assert_eq!(next.prompt(), QUEUED);
    next.succeed();
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        target,
        "both Prompts stand as Messages",
        |snapshot| {
            !messages_saying(snapshot, PROMOTED).is_empty()
                && !messages_saying(snapshot, QUEUED).is_empty()
        },
    )
    .await;
    let promoted = messages_saying(&snapshot, PROMOTED)[0];
    assert_eq!(
        promoted.turn_id, snapshot.turns[0].id,
        "the promoted Prompt steered the Turn it waited behind"
    );
    assert_eq!(promoted.author, Some(sidekick_author(sidekick_id)));
    let queued = messages_saying(&snapshot, QUEUED)[0];
    assert_eq!(
        queued.turn_id, snapshot.turns[1].id,
        "the queued one began the next"
    );
    assert_eq!(queued.author, Some(sidekick_author(sidekick_id)));

    let (attached, _) = watch_session(&descriptor, target).await;
    for text in [PROMOTED, QUEUED] {
        assert_eq!(
            messages_saying(&attached, text)[0].author,
            Some(sidekick_author(sidekick_id)),
            "a Client attaching afterwards is sent the Message naming the Sidekick"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Prompt a Sidekick sent that is still waiting keeps its author through a
/// restart, and an interrupt that withdraws one — a Session Working only to
/// deliver it — says so, the withdrawn Prompt still naming the Sidekick.
#[tokio::test]
async fn a_waiting_sidekick_prompt_keeps_its_author_across_a_restart_and_is_withdrawn_by_an_interrupt()
 {
    const QUEUED: &str = "Then write the changelog entry.";
    const WITHDRAWN: &str = "Pick the parser back up where it stopped.";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-author-waits").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (target, target_provider) = started_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        "Write the parser",
    )
    .await;
    acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": target, "prompt": QUEUED, "delivery": "queue" }),
    )
    .await;
    drop((sidekick_provider, target_provider));
    server.shutdown().await.expect("stop the server");

    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-author-waits").await;
    let descriptor = server.descriptor().clone();
    let restored = read_session(&descriptor, target).await;
    let waiting = prompt_saying(&restored, QUEUED);
    assert_eq!(
        (waiting.status, waiting.author.clone()),
        (PromptStatus::Pending, Some(sidekick_author(sidekick_id))),
        "a Prompt still waiting is restored naming the Sidekick that sent it"
    );

    // The Sidekick's Agent starts again for the next thing asked of it, and
    // the target's Provider, asked to start for the Sidekick's Prompt, is
    // held starting: the target is Working only to deliver that Prompt.
    admit_prompt(&descriptor, sidekick_id, "Carry on.").await;
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
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": WITHDRAWN }),
        )
        .await,
        json!({ "session_id": target, "admitted": "new_turn" }),
    );
    let _starting = next_start(&mut claude).await;

    assert_eq!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": target }),
        )
        .await,
        json!({
            "session_id": target,
            "outcome": "withdrew_prompt",
            "prompt": WITHDRAWN,
        }),
    );
    let withdrawn = read_session(&descriptor, target).await;
    let withdrawn = prompt_saying(&withdrawn, WITHDRAWN);
    assert_eq!(
        (withdrawn.status, withdrawn.author.clone()),
        (PromptStatus::Cancelled, Some(sidekick_author(sidekick_id))),
        "the withdrawn Prompt still says whose it was"
    );

    server.shutdown().await.expect("shut down server");
}

/// A Subagent of a Sidekick works in the Sidekick Workspace, so it is one of
/// that Workspace's Sessions as surely as the Sidekick's own, and no Sidekick
/// acts on it — the one that spawned it included.
#[tokio::test]
async fn no_act_reaches_a_subagent_working_beneath_a_sidekick() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-subagent-refusals", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_spawner, mut spawner, _spawner_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_other, mut other, _other_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let subagent = spawner
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around the Sidekick Workspace.",
        }))
        .await;
    let start = next_start(&mut hosted.codex).await;
    let mut subagent_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: default_selection(&codex_models()),
    });
    timeout(PROGRESS_DEADLINE, subagent_provider.next_turn())
        .await
        .expect("the Subagent's Turn reaches its Provider")
        .succeed();
    let before = read_session(&descriptor, subagent).await;

    for client in [&mut spawner, &mut other] {
        for (tool, arguments) in [
            (
                "send_prompt",
                json!({ "session_id": subagent, "prompt": "Look elsewhere." }),
            ),
            ("interrupt_session", json!({ "session_id": subagent })),
            ("settle_session", json!({ "session_id": subagent })),
            ("unsettle_session", json!({ "session_id": subagent })),
        ] {
            assert_eq!(
                refused(client, tool, arguments).await,
                SIDEKICK_WORKSPACE_REFUSAL,
                "{tool} is refused for a Subagent working in the Sidekick Workspace"
            );
        }
    }
    assert!(
        timeout(
            Duration::from_millis(50),
            subagent_provider.next_interrupt()
        )
        .await
        .is_err(),
        "the Subagent's Provider is never told to stop"
    );
    let after = read_session(&descriptor, subagent).await;
    assert_eq!(
        (after.prompts, after.messages, after.turns),
        (before.prompts, before.messages, before.turns),
        "nothing reached the Subagent's Session"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// The author is the Server's to set, from the Broker caller it knows: a
/// Client naming one in the Session API's own requests is refused as any
/// request carrying what the API does not take is, and nothing is admitted.
#[tokio::test]
async fn a_client_cannot_name_an_author_through_the_session_api() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-author-not-client", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, _sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (target, _target_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    let author = json!({ "kind": "sidekick", "session_id": sidekick_id, "title": "Plan the work" });
    let prompt = |author: Option<&Value>| {
        let mut prompt = json!({
            "id": PromptId::new(),
            "text": "Claim to be the Sidekick.",
            "skill_invocations": [],
        });
        if let Some(author) = author {
            prompt["author"] = author.clone();
        }
        prompt
    };

    for body in [
        json!({ "prompt": prompt(Some(&author)), "delivery": "queue" }),
        json!({ "prompt": prompt(None), "delivery": "queue", "author": author }),
    ] {
        let response = reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{target}/prompts",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .json(&body)
            .send()
            .await
            .expect("send the admission");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        let refused = response
            .json::<SessionError>()
            .await
            .expect("decode the refusal");
        assert_eq!(
            refused.code,
            suru::protocol::SessionErrorCode::InvalidCommand
        );
    }
    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&json!({
            "agent_selection": default_selection(&claude_models()),
            "execution_directory": { "path": workspace },
            "prompt": prompt(Some(&author)),
        }))
        .send()
        .await
        .expect("send the creation");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let snapshot = read_session(&descriptor, target).await;
    assert!(
        snapshot
            .prompts
            .iter()
            .all(|prompt| prompt.text != "Claim to be the Sidekick."),
        "nothing a Client claimed was admitted: {:?}",
        snapshot.prompts
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// The Error the `turns`th Turn of `target` failed with, once it has.
async fn failed_with(descriptor: &RuntimeDescriptor, target: SessionId, turns: usize) -> String {
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        descriptor,
        target,
        "the Turn fails",
        |snapshot| {
            snapshot.turns.len() == turns && snapshot.turns[turns - 1].status == TurnStatus::Failed
        },
    )
    .await;
    let turn = snapshot.turns[turns - 1].id;
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Error { turn_id, text, .. } if *turn_id == turn => Some(text.clone()),
            _ => None,
        })
        .expect("the failed Turn says why")
}

/// Whether a Provider can run is no question admission asks, a Client's or a
/// Sidekick's: a Prompt to a Session whose Provider cannot start is admitted
/// to begin a Turn, and that Turn is recorded as failed, saying why.
#[tokio::test]
async fn send_prompt_to_a_session_whose_provider_is_unavailable_is_admitted_as_a_clients_is() {
    const UNAVAILABLE: &str = "the codex CLI is not signed in";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-unavailable", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let target = create_session(
        &descriptor,
        &session_request(&workspace, default_selection(&codex_models()), "Write it"),
    )
    .await
    .session
    .id;
    hosted.runtimes[1].set_unavailable(Some(ProviderUnavailability::NotSignedIn));
    next_start(&mut hosted.codex)
        .await
        .fail_unavailable(ProviderUnavailability::NotSignedIn, UNAVAILABLE);

    failed_with(&descriptor, target, 1).await;

    assert_eq!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "prompt": "Try again." }),
        )
        .await,
        json!({ "session_id": target, "admitted": "new_turn" }),
        "the Sidekick's Prompt is admitted to begin a Turn"
    );
    next_start(&mut hosted.codex)
        .await
        .fail_unavailable(ProviderUnavailability::NotSignedIn, UNAVAILABLE);
    let sidekicks = failed_with(&descriptor, target, 2).await;

    let response = client_admits(&descriptor, target, "Try once more.").await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a Client's Prompt is admitted to begin a Turn just the same"
    );
    next_start(&mut hosted.codex)
        .await
        .fail_unavailable(ProviderUnavailability::NotSignedIn, UNAVAILABLE);
    let clients = failed_with(&descriptor, target, 3).await;

    assert_eq!(
        sidekicks, clients,
        "and each Turn fails, saying why, as the other does"
    );
    assert!(sidekicks.contains(UNAVAILABLE), "{sidekicks}");
    let snapshot = read_session(&descriptor, target).await;
    assert_eq!(snapshot.turns.len(), 3, "one failed Turn for each Prompt");

    hosted.server.shutdown().await.expect("shut down server");
}
