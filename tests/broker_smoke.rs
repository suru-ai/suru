#![cfg(unix)]

//! The Broker end to end against the installed, signed-in CLIs (spec #407): a
//! real Claude Session's Agent spawns a real Codex Subagent through the Broker,
//! and hears back from it — through its Subagent Report, waking the Claude
//! Agent into a Continuation, and through a wait on it held open for longer
//! than Claude's idle window for a silent call.
//!
//! Every other Broker test drives Suru against Provider doubles and scripted
//! stand-ins; these are the check that the real harnesses do what those
//! doubles were written to: that Claude loads the Broker from its MCP config,
//! finds its Tools, and calls them unasked, at whichever MCP revision it
//! negotiates with the Broker; that Codex takes the Broker on its thread and
//! runs a brokered Subagent at the posture derived for it; that Claude answers
//! a Report as a Turn of its own; and that a long Broker call survives Claude's
//! timers. They spend real Model calls, so they run only when asked twice —
//! the ignore attribute and `SURU_BROKER_SMOKE=1` — and skip when Suru finds
//! either CLI not installed or not signed in:
//!
//! ```text
//! SURU_BROKER_SMOKE=1 cargo nextest run --test broker_smoke --run-ignored ignored-only --no-capture
//! ```
//!
//! `SURU_CLAUDE_PATH` and `SURU_CODEX_PATH` name other binaries, as they do for
//! Suru itself, and `SURU_LOG` writes the Server's own tracing to stderr. What
//! they assert, and the versions they last passed against, are recorded in
//! `docs/validation/0421-broker-smoke.md`.

#[path = "support/mod.rs"]
#[allow(dead_code)]
mod server_support;

use std::{ffi::OsStr, fmt::Display, path::Path, process::Command, sync::Arc};

use server_support::{PROGRESS_DEADLINE, receive_initial_state};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription, SubagentTreeEvent},
    protocol::{
        Activity, ActivityStatus, AgentSelection, ApprovalPosture, ClaudePermissionMode,
        CodexApprovalPolicy, CodexSandboxMode, CreateSessionRequest, Delegator, ExecutionDirectory,
        InitialPrompt, MessageRole, MessageStatus, ModelAvailability, ModelCatalog,
        ModelDescriptor, ModelId, PromptId, ProviderCatalogStatus, ProviderId,
        ProviderUnavailability, SessionId, SessionRevision, SessionSnapshot, TurnId, TurnStatus,
    },
    provider::{ClaudeRuntime, CodexRuntime},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, Instant, timeout};

const GATE: &str = "SURU_BROKER_SMOKE";

/// What the Report smoke asks of the Claude Agent. It names the Broker but
/// none of its Tools, which the Agent learns from the note Suru appends to its
/// system prompt. The Subagent sleeps before it answers so that its Report
/// reaches a Claude Agent whose Turn has settled — the Continuation that smoke
/// is about — rather than steering the Turn that spawned it.
const REPORT_PROMPT: &str = "Using Suru's Broker, spawn one Subagent on the codex Provider, on \
that Provider's default Model, named echo. Tell it to run the shell command `sleep 8` and then \
reply with exactly the word PONG and nothing else. Do not do its task yourself, and do not wait \
for it or check on it: once it is spawned, end your turn straight away. Suru will bring you its \
Report as a new message; when it arrives, answer with the single word DONE followed by the \
Subagent's reply.";

/// How long the wait smoke's Subagent sleeps: past the 300 s after which
/// Claude aborts an HTTP MCP call that has sent nothing
/// (`docs/validation/0408-claude-http-mcp-long-calls.md`, run 4), so the
/// Agent's wait on it has to outlast Claude's idle window.
const SUBAGENT_SLEEP_SECONDS: u64 = 330;

/// The `timeout_seconds` the wait smoke's Agent waits with: longer than the
/// Subagent sleeps, so the wait answers because the Subagent settled.
const WAIT_TIMEOUT_SECONDS: u64 = 400;

/// What the wait smoke asks of the Claude Agent: to hold one Broker call open
/// for as long as the Subagent sleeps, and to say so plainly should the call
/// fail, so a call Claude cut short is told apart from one that answered.
fn wait_prompt() -> String {
    format!(
        "Using Suru's Broker, spawn one Subagent on the codex Provider, on that Provider's \
         default Model, named echo. Tell it to run the shell command `sleep \
         {SUBAGENT_SLEEP_SECONDS}`, which takes over five minutes, to wait for that command to \
         finish, and only then to reply with exactly the word PONG and nothing else. Do not do \
         its task yourself. Once it is spawned, call wait_subagents on it with timeout_seconds \
         {WAIT_TIMEOUT_SECONDS} and wait for that call to answer, calling nothing else \
         meanwhile. When it answers with the Subagent settled, reply with the single word DONE \
         followed by the Subagent's reply, and end your turn. If the call fails or times out \
         instead, reply with WAIT FAILED followed by the error it gave, and end your turn \
         without calling it again."
    )
}

/// What the wait smoke's Agent says when its wait did not answer.
const WAIT_FAILED: &str = "WAIT FAILED";

/// The Subagent's name, as both Prompts give it.
const SUBAGENT_NAME: &str = "echo";

/// The Claude Model the delegating Agent runs on: the cheapest the CLI offers,
/// which is capable enough to find a deferred Tool and follow the Prompt.
const PARENT_MODEL: &str = "haiku";

/// How long each live stretch of the Report smoke — the spawn, the
/// Subagent's work, the Report's Continuation — may take before the smoke
/// calls it a failure. It is a failure deadline for real CLIs calling real
/// Models, not an expected wait: every wait returns the moment what it waits
/// for arrives.
const LIVE_STEP_DEADLINE: Duration = Duration::from_secs(300);

/// How long the whole of the wait smoke may take, from its Session's creation:
/// its Subagent's sleep, and the minutes either side of it that real CLIs
/// take to start, spawn, and answer.
const WAIT_SMOKE_DEADLINE: Duration = Duration::from_secs(600);

/// How long a Session may stand at rest — nothing in it Working, every Turn
/// settled — without changing before a smoke waiting on it for something else
/// calls that a state nothing will now move it from. A Session passes through
/// rest on its way to a Continuation, between the commit that settles its
/// Subagent's row and the one that opens the Continuation the Report wakes, so
/// rest alone is no failure; but that step takes a moment, not this long.
const REST_GRACE: Duration = PROGRESS_DEADLINE;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "set SURU_BROKER_SMOKE=1 to use the installed, signed-in Claude and Codex binaries"]
async fn a_claude_agent_spawns_a_codex_subagent_through_the_broker_and_answers_its_report() {
    let Some(mut live) = Live::start("broker-report-smoke").await else {
        return;
    };
    let (created, prompt_id) = live.create_claude_session(REPORT_PROMPT).await;
    let parent_id = created.session.id;
    let mut parent = Watched::subscribe(&live.client, parent_id, "the Claude Session").await;

    let (spawned, child_id) =
        brokered_spawn(&mut live, &mut parent, prompt_id, LIVE_STEP_DEADLINE).await;
    let prompt_turn = spawned.turns[0].clone();
    live.say(format!(
        "the Claude Agent spawned {child_id} through the Broker"
    ));
    stands_under_its_parent(&live, parent_id, child_id).await;
    let mut child = delegated(&mut live, &created, child_id, LIVE_STEP_DEADLINE).await;

    // The Subagent's Turn settles, and its row with it.
    let child_settled = child
        .read_until(
            &mut live.client,
            LIVE_STEP_DEADLINE,
            "the Codex Subagent's Turn settles",
            |snapshot| {
                snapshot
                    .turns
                    .first()
                    .is_some_and(|turn| turn.status.is_terminal())
            },
        )
        .await;
    let (child_model, child_reply) = settled_with_pong(&child_settled);
    live.say(format!(
        "the Codex Subagent settled on {}: {child_reply:?}",
        child_model.model
    ));

    // The Report wakes the settled Claude Agent into a Continuation, where it answers.
    let answered = parent
        .read_until(
            &mut live.client,
            LIVE_STEP_DEADLINE,
            "the Report wakes the Claude Agent into a Continuation that settles",
            |snapshot| {
                row_of(snapshot, child_id)
                    .is_some_and(|(status, _)| status != ActivityStatus::Active)
                    && snapshot.turns.len() > 1
                    && at_rest(snapshot)
            },
        )
        .await;
    let (row_status, row_duration) = row_of(&answered, child_id).expect("the row still stands");
    assert_eq!(
        row_status,
        ActivityStatus::Completed,
        "the row settles as the Subagent's Turn did"
    );
    assert!(
        row_duration.is_some(),
        "a settled row says how long the Subagent worked"
    );
    assert_eq!(
        the_subagent_row(&answered).turn_id(),
        prompt_turn.id,
        "the row stays in the Turn that spawned the Subagent"
    );
    assert_eq!(
        answered.turns.len(),
        2,
        "the Prompt's Turn and the one Continuation the Report began: {}",
        describe(&answered)
    );
    let [prompted, continuation] = [&answered.turns[0], &answered.turns[1]];
    assert_eq!(prompted.status, TurnStatus::Completed);
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation, which no Prompt began"
    );
    assert_eq!(continuation.status, TurnStatus::Completed);
    let (Some(prompt_settled), Some(continuation_began)) =
        (prompted.settled_at, continuation.started_at)
    else {
        panic!("both Turns say when they worked: {}", describe(&answered));
    };
    assert!(
        prompt_settled <= continuation_began,
        "the Report woke an Agent whose Turn had settled, rather than steering it: {}",
        describe(&answered)
    );
    assert!(
        answered
            .messages
            .iter()
            .filter(|message| message.turn_id == continuation.id)
            .all(|message| message.role == MessageRole::Agent),
        "the Report stands nowhere in the Transcript: {}",
        describe(&answered)
    );
    let answer = agent_text_in(&answered, continuation.id);
    assert!(
        answer.contains("DONE") && answer.contains("PONG"),
        "the Claude Agent answers the Report with the Subagent's reply: {answer:?}"
    );
    assert!(
        !answered
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Command { .. })),
        "the Broker's calls, and the ToolSearch loading them, add no Command row: {}",
        describe(&answered)
    );
    live.say(format!(
        "the Report woke the Claude Agent, which answered {answer:?}"
    ));

    settled_in_the_tree(&live, parent_id, child_id, &child_model.model).await;
    assert_eq!(
        subagent_tree(&live, parent_id)
            .await
            .top_level
            .working_since,
        None,
        "nothing in the tree is Working any more"
    );
    drop(parent);
    drop(child);
    live.finish().await;
}

/// The Broker call Claude holds open longest is a wait on a Subagent. This
/// smoke holds one open for longer than Claude aborts a silent HTTP MCP call,
/// at whichever MCP revision Claude negotiated with the Broker, while the
/// Broker reports progress on it: the call answers once the Subagent settles,
/// and the Agent acts on the answer in the very Turn that waited.
///
/// The Subagent's Report reaches the Agent as the wait answers, steering the
/// Turn still in its Broker call, and the CLI folds it into that loop's next
/// request and answers both with one `result`. That `result` must Settle the
/// Turn that waited, so the smoke waits past the answer for the Session to come
/// to rest (`docs/validation/0407-claude-folded-steer.md`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "set SURU_BROKER_SMOKE=1 to use the installed, signed-in Claude and Codex binaries"]
async fn a_claude_agents_wait_on_a_codex_subagent_outlasts_claudes_idle_window() {
    let Some(mut live) = Live::start("broker-wait-smoke").await else {
        return;
    };
    let deadline = Instant::now() + WAIT_SMOKE_DEADLINE;
    let (created, prompt_id) = live.create_claude_session(&wait_prompt()).await;
    let parent_id = created.session.id;
    let mut parent = Watched::subscribe(&live.client, parent_id, "the Claude Session").await;

    let (spawned, child_id) =
        brokered_spawn(&mut live, &mut parent, prompt_id, left_before(deadline)).await;
    let spawned_at = Instant::now();
    let waiting_turn = spawned.turns[0].id;
    live.say(format!(
        "the Claude Agent spawned {child_id} through the Broker"
    ));
    let delegated_child = delegated(&mut live, &created, child_id, left_before(deadline)).await;
    drop(delegated_child);

    // The Turn that spawned the Subagent waits on it, and answers once the wait does — or says
    // the wait failed, which ends the smoke there. It fails, or is interrupted, never.
    let answered = parent
        .read_until_unless(
            &mut live.client,
            left_before(deadline),
            "the Claude Agent's wait on its Subagent answers, and the Agent acts on it",
            |snapshot| {
                let answer = agent_text_in(snapshot, waiting_turn);
                answer.contains("DONE") && answer.contains("PONG")
            },
            |snapshot| {
                snapshot
                    .messages
                    .iter()
                    .find(|message| {
                        message.role == MessageRole::Agent && message.content.contains(WAIT_FAILED)
                    })
                    .map(|message| format!("said its wait failed: {:?}", message.content))
            },
        )
        .await;
    let waited = spawned_at.elapsed();
    let answered_at = Instant::now();
    let answer = agent_text_in(&answered, waiting_turn);
    live.say(format!(
        "the Claude Agent answered {answer:?} in the Turn that waited, {:.1}s after the spawn",
        waited.as_secs_f64()
    ));
    assert!(
        waited >= Duration::from_secs(SUBAGENT_SLEEP_SECONDS),
        "the answer came once the Subagent had slept, not before: {}",
        describe(&answered)
    );
    let (row_status, _) = row_of(&answered, child_id).expect("the row still stands");
    assert_eq!(
        row_status,
        ActivityStatus::Completed,
        "the Subagent the Agent waited on settled Completed: {}",
        describe(&answered)
    );
    let waiting = answered
        .turns
        .iter()
        .find(|turn| turn.id == waiting_turn)
        .expect("the Turn that waited still stands");
    assert!(
        matches!(waiting.status, TurnStatus::Active | TurnStatus::Completed),
        "the Turn that waited is neither failed nor interrupted: {}",
        describe(&answered)
    );

    // The Report steered the Turn that waited, and the one result the CLI answered the Prompt and
    // the folded Report with Settles it: the Session comes to rest, rather than reading Working
    // for as long as the smoke would let it.
    let rested = parent
        .read_until(
            &mut live.client,
            left_before(deadline),
            "the Turn that waited settles once the Agent has answered, leaving nothing Working",
            at_rest,
        )
        .await;
    let settled_turn = rested
        .turns
        .iter()
        .find(|turn| turn.id == waiting_turn)
        .expect("the Turn that waited still stands");
    assert_eq!(
        settled_turn.status,
        TurnStatus::Completed,
        "the Turn that waited settles Completed: {}",
        describe(&rested)
    );
    assert_eq!(
        rested.turns.len(),
        1,
        "the Report steered the Turn that waited rather than waking a Continuation: {}",
        describe(&rested)
    );
    live.say(format!(
        "the Turn that waited settled {:?}, {:.1}s after the answer",
        settled_turn.status,
        answered_at.elapsed().as_secs_f64()
    ));

    // The Subagent slept as long as it was told to, so the call it answered was that long.
    let child_settled = live
        .client
        .read_session(child_id)
        .await
        .expect("read the Codex Subagent's Session");
    let (child_model, child_reply) = settled_with_pong(&child_settled);
    let child_turn = &child_settled.turns[0];
    let (Some(began), Some(settled)) = (child_turn.started_at, child_turn.settled_at) else {
        panic!(
            "the Subagent's Turn says when it worked: {}",
            describe(&child_settled)
        );
    };
    assert!(
        settled.0 - began.0 >= SUBAGENT_SLEEP_SECONDS * 1_000,
        "the Subagent slept out its {SUBAGENT_SLEEP_SECONDS} s before it answered: {}",
        describe(&child_settled)
    );
    live.say(format!(
        "the Codex Subagent worked {:.1}s on {} and answered {child_reply:?}",
        (settled.0 - began.0) as f64 / 1_000.0,
        child_model.model
    ));

    settled_in_the_tree(&live, parent_id, child_id, &child_model.model).await;
    drop(parent);
    live.finish().await;
}

/// A Server hosting the installed Claude and Codex, and the client a smoke
/// drives it through.
struct Live {
    began: Instant,
    server: server::RunningServer,
    client: ManagedClient,
    parent_model: ModelId,
    workspace: tempfile::TempDir,
    _state_dir: tempfile::TempDir,
    _config_dir: tempfile::TempDir,
}

impl Live {
    /// The Server a smoke runs against, or `None` — having said why — when
    /// the smoke is not to run here: not opted into, or either CLI not
    /// installed or not signed in.
    async fn start(channel: &str) -> Option<Self> {
        if std::env::var_os(GATE).as_deref() != Some(OsStr::new("1")) {
            eprintln!("skipping: set {GATE}=1 to opt in");
            return None;
        }
        trace_the_server_when_asked();
        let began = Instant::now();
        println!(
            "claude --version: {}",
            cli_version("SURU_CLAUDE_PATH", "claude")
        );
        println!(
            "codex --version: {}",
            cli_version("SURU_CODEX_PATH", "codex")
        );

        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let config_dir = tempfile::tempdir().expect("create isolated config directory");
        // Claude's bypassPermissions derives Codex's never with danger-full-access for the
        // Subagent (ADR 0036), so nothing in the tree asks; and no Title or Workspace Icon is
        // derived, which would spend Model calls the smoke has no stake in.
        std::fs::write(
            config_dir.path().join("suru.jsonc"),
            r#"{
  "provider": { "claude": { "permissionMode": "bypassPermissions" } },
  "derivation": { "errand": "off" }
}
"#,
        )
        .expect("write the Config Document");
        let workspace = scratch_repository();
        let server = server::spawn_with_providers(
            ServerConfig::new(state_dir.path(), channel)
                .expect("configure server")
                .with_config_dir(config_dir.path()),
            vec![
                Arc::new(ClaudeRuntime::from_environment()),
                Arc::new(CodexRuntime::from_environment()),
            ],
        )
        .await
        .expect("launch a Server hosting the installed Claude and Codex");
        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
        )
        .await
        .expect("connect client");
        receive_initial_state(&mut client).await;

        let catalog = client
            .refresh_models()
            .await
            .expect("ask both Providers what they offer");
        if let Some(reason) = unusable(&catalog) {
            eprintln!("skipping: {reason}");
            drop(client);
            shut_down(server).await;
            return None;
        }
        let parent_model = parent_model(&catalog).id.clone();
        let codex_default = catalog_of(&catalog, "codex")
            .models
            .iter()
            .find(|model| model.is_default)
            .map_or_else(|| "none".to_owned(), |model| model.id.to_string());
        println!("parent Model: claude/{parent_model}; Codex's default Model: {codex_default}");
        Some(Self {
            began,
            server,
            client,
            parent_model,
            workspace,
            _state_dir: state_dir,
            _config_dir: config_dir,
        })
    }

    /// Prints `what` against how long the smoke has run, so a run's log is its
    /// timeline.
    fn say(&self, what: impl Display) {
        println!("{:>6.1}s: {what}", self.began.elapsed().as_secs_f64());
    }

    /// A Claude Session in the scratch repository, begun by `prompt`, at the
    /// posture the Config Document pinned.
    async fn create_claude_session(&self, prompt: &str) -> (SessionSnapshot, PromptId) {
        let prompt_id = PromptId::new();
        let created = self
            .client
            .create_session(CreateSessionRequest {
                preparation_id: None,
                agent_selection: Some(AgentSelection {
                    provider: ProviderId::new("claude"),
                    model: self.parent_model.clone(),
                    options: Vec::new(),
                }),
                execution_directory: ExecutionDirectory {
                    path: self.workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: prompt.to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create the Claude Session");
        assert_eq!(
            created
                .session
                .approval_posture
                .as_ref()
                .map(|posture| &posture.value),
            Some(&ApprovalPosture::Claude {
                permission_mode: ClaudePermissionMode::BypassPermissions,
            }),
            "the Claude Session follows the bypassPermissions Setting"
        );
        (created, prompt_id)
    }

    async fn finish(self) {
        let began = self.began;
        drop(self.client);
        shut_down(self.server).await;
        println!(
            "{:>6.1}s: the Server shut down",
            began.elapsed().as_secs_f64()
        );
    }
}

/// How long is left before `deadline`.
fn left_before(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Waits for the Claude Agent to spawn its Subagent through the Broker:
/// exactly one row, brokered and named as the Prompt named it, in the Turn the
/// Prompt began. Answers with the Session as it then stood and the Subagent's
/// Session. An Agent whose Turn settles without one has given up, and is read
/// as it stands then.
async fn brokered_spawn(
    live: &mut Live,
    parent: &mut Watched,
    prompt_id: PromptId,
    within: Duration,
) -> (SessionSnapshot, SessionId) {
    let spawned = parent
        .read_until(
            &mut live.client,
            within,
            "the Claude Agent spawns a Subagent through the Broker",
            |snapshot| {
                subagent_rows(snapshot).next().is_some()
                    || snapshot
                        .turns
                        .first()
                        .is_some_and(|turn| turn.status.is_terminal())
            },
        )
        .await;
    let Activity::Subagent {
        turn_id,
        name,
        session_id,
        brokered,
        ..
    } = the_subagent_row(&spawned)
    else {
        unreachable!("the_subagent_row answers a Subagent row");
    };
    assert!(
        brokered,
        "the row is the Broker's, not Claude's own Agent tool's"
    );
    assert!(
        named_as_prompted(name),
        "the row names the Subagent as the spawn did: {name}"
    );
    let prompt_turn = &spawned.turns[0];
    assert_eq!(prompt_turn.prompt_id, Some(prompt_id));
    assert_eq!(
        *turn_id, prompt_turn.id,
        "the row stands in the Turn whose Agent spawned the Subagent"
    );
    let child_id = *session_id;
    (spawned, child_id)
}

/// Whether the Agent named its Subagent as the Prompt asked: the name there,
/// in whatever case, and whatever the Agent wrapped around it.
fn named_as_prompted(name: &str) -> bool {
    name.to_lowercase().contains(SUBAGENT_NAME)
}

/// The Subagent stands under its parent in the tree, and nowhere Sessions are
/// listed.
async fn stands_under_its_parent(live: &Live, parent_id: SessionId, child_id: SessionId) {
    let tree = subagent_tree(live, parent_id).await;
    assert_eq!(tree.top_level.session_id, parent_id);
    let entry = tree
        .subagents
        .iter()
        .find(|entry| entry.session_id == child_id)
        .unwrap_or_else(|| panic!("the tree lists the Subagent: {tree:?}"));
    assert_eq!(entry.parent_session_id, parent_id);
    assert!(named_as_prompted(&entry.name), "{}", entry.name);
    let listed = live
        .client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .iter()
        .map(|item| item.id())
        .collect::<Vec<_>>();
    assert!(listed.contains(&parent_id), "the Claude Session is listed");
    assert!(
        !listed.contains(&child_id),
        "a Subagent's Session is reached through its parent's tree, never listed"
    );
}

/// Waits for the Delegation to reach the Codex Subagent, and checks the
/// Session it opened: under the Claude Session, on Codex, in the parent's
/// Execution Directory, at the posture derived for it, opening with the
/// Delegation from the parent's Agent. Answers with the Subagent's Session to
/// wait on.
async fn delegated(
    live: &mut Live,
    parent: &SessionSnapshot,
    child_id: SessionId,
    within: Duration,
) -> Watched {
    let mut child =
        Watched::subscribe(&live.client, child_id, "the Codex Subagent's Session").await;
    let delegated = child
        .read_until(
            &mut live.client,
            within,
            "the Delegation reaches the Codex Subagent",
            |snapshot| {
                snapshot
                    .messages
                    .iter()
                    .any(|message| matches!(message.role, MessageRole::Delegation(_)))
            },
        )
        .await;
    assert_eq!(delegated.session.parent, Some(parent.session.id));
    assert_eq!(
        delegated
            .session
            .agent_selection
            .as_ref()
            .map(|selection| selection.provider.clone()),
        Some(ProviderId::new("codex")),
        "the Subagent runs on the Provider the spawn chose"
    );
    assert_eq!(
        delegated.session.execution_directory, parent.session.execution_directory,
        "the Subagent works in its parent's Execution Directory"
    );
    assert_eq!(
        delegated
            .session
            .approval_posture
            .as_ref()
            .map(|posture| &posture.value),
        Some(&ApprovalPosture::Codex {
            approval_policy: CodexApprovalPolicy::Never,
            sandbox_mode: CodexSandboxMode::DangerFullAccess,
        }),
        "the Subagent takes Codex's value at bypassPermissions' level (ADR 0036)"
    );
    let delegation = &delegated.messages[0];
    assert_eq!(
        delegation.role,
        MessageRole::Delegation(Delegator {
            session_id: parent.session.id,
            name: None,
        }),
        "the Subagent's Transcript opens with the Delegation, from the Claude Session's Agent"
    );
    assert!(
        delegation.content.contains("PONG"),
        "the Delegation carries the task the Agent was asked to hand on: {}",
        delegation.content
    );
    child
}

/// The Codex Subagent's first Turn, settled Completed with PONG in its reply:
/// the Agent Selection that ran it, and the reply.
fn settled_with_pong(child: &SessionSnapshot) -> (AgentSelection, String) {
    let turn = child
        .turns
        .first()
        .expect("the Subagent's Session holds its Turn");
    assert_eq!(turn.status, TurnStatus::Completed, "{}", describe(child));
    let selection = turn
        .agent
        .as_ref()
        .map(|identity| identity.selection.clone())
        .expect("the Codex Turn records the Agent that ran it");
    assert_eq!(selection.provider, ProviderId::new("codex"));
    let reply = agent_text_in(child, turn.id);
    assert!(
        reply.contains("PONG"),
        "the Subagent did what it was delegated: {reply:?}"
    );
    (selection, reply)
}

/// The tree, read afresh, shows the Subagent settled on the Model its
/// Provider confirmed.
async fn settled_in_the_tree(
    live: &Live,
    parent_id: SessionId,
    child_id: SessionId,
    model: &ModelId,
) {
    let tree = subagent_tree(live, parent_id).await;
    let entry = tree
        .subagents
        .iter()
        .find(|entry| entry.session_id == child_id)
        .expect("the tree still lists the Subagent");
    assert_eq!(entry.status, ActivityStatus::Completed);
    assert_eq!(
        entry.model.as_ref(),
        Some(model),
        "the tree shows the Model the Subagent's Provider confirmed"
    );
}

/// The tree `parent_id` heads, as its stream's opening snapshot gives it.
async fn subagent_tree(live: &Live, parent_id: SessionId) -> suru::protocol::SubagentTreeSnapshot {
    let mut tree = live.client.subscribe_subagent_tree(parent_id);
    let SubagentTreeEvent::Snapshot(snapshot) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree opens")
        .expect("the Subagent tree stream stays open")
    else {
        panic!("the Subagent tree stream opens with its snapshot");
    };
    snapshot
}

/// One Session the smoke waits on, woken by each update its stream carries.
struct Watched {
    session_id: SessionId,
    what: &'static str,
    feed: SessionSubscription,
}

impl Watched {
    async fn subscribe(client: &ManagedClient, session_id: SessionId, what: &'static str) -> Self {
        let feed = client
            .subscribe_session(session_id)
            .await
            .unwrap_or_else(|error| panic!("subscribe to {what}: {error}"));
        Self {
            session_id,
            what,
            feed,
        }
    }

    /// [`Self::read_until_unless`], with nothing about this wait in
    /// particular that dooms it.
    async fn read_until(
        &mut self,
        client: &mut ManagedClient,
        deadline: Duration,
        awaited: &str,
        wanted: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        self.read_until_unless(client, deadline, awaited, wanted, |_| None)
            .await
    }

    /// Reads the Session until `wanted` holds, failing the smoke at `deadline`
    /// with the Session as it stood — or sooner, once it has plainly stopped
    /// going anywhere: at once when it is [`stuck`] or `doomed` says why this
    /// wait cannot end well, and after [`REST_GRACE`] when it has stood at
    /// rest, unchanged, all that while. The managed client's own events are
    /// read here too, so a smoke that has no use for them never leaves the
    /// client backed up behind them.
    async fn read_until_unless(
        &mut self,
        client: &mut ManagedClient,
        deadline: Duration,
        awaited: &str,
        wanted: impl Fn(&SessionSnapshot) -> bool,
        doomed: impl Fn(&SessionSnapshot) -> Option<String>,
    ) -> SessionSnapshot {
        let what = self.what;
        let waited = timeout(deadline, async {
            let mut resting: Option<(SessionRevision, Instant)> = None;
            loop {
                let snapshot = client
                    .read_session(self.session_id)
                    .await
                    .unwrap_or_else(|error| panic!("read {what}: {error}"));
                if wanted(&snapshot) {
                    return snapshot;
                }
                if let Some(why) = stuck(&snapshot).or_else(|| doomed(&snapshot)) {
                    panic!("{awaited}, but {what} {why}: {}", describe(&snapshot));
                }
                resting = at_rest(&snapshot).then(|| match resting {
                    Some((revision, until)) if revision == snapshot.revision => (revision, until),
                    _ => (snapshot.revision, Instant::now() + REST_GRACE),
                });
                let rest_ends = resting.map_or_else(Instant::now, |(_, until)| until);
                tokio::select! {
                    event = self.feed.next() => {
                        event
                            .unwrap_or_else(|| panic!("{what}'s stream stays open"))
                            .unwrap_or_else(|error| panic!("{what}'s stream stays readable: {error}"));
                    }
                    event = client.next() => {
                        event.expect("the managed client stays connected");
                    }
                    () = tokio::time::sleep_until(rest_ends), if resting.is_some() => {
                        panic!(
                            "{awaited}, but {what} has stood at rest — nothing Working, every \
                             Turn settled — unchanged for {REST_GRACE:?}: {}",
                            describe(&snapshot)
                        );
                    }
                }
            }
        })
        .await;
        match waited {
            Ok(snapshot) => snapshot,
            Err(_) => {
                let snapshot = client
                    .read_session(self.session_id)
                    .await
                    .unwrap_or_else(|error| panic!("read {what}: {error}"));
                panic!(
                    "{awaited} within {deadline:?}, but {what} stands as: {}",
                    describe(&snapshot)
                );
            }
        }
    }
}

/// Whether nothing in the Session is Working and every Turn it holds has
/// settled: all it will do unless something wakes it.
fn at_rest(snapshot: &SessionSnapshot) -> bool {
    snapshot.working_since().is_none()
        && !snapshot.turns.is_empty()
        && snapshot.turns.iter().all(|turn| turn.status.is_terminal())
}

/// What makes a Session unable to go on as the smoke needs: a Turn that failed
/// or was interrupted, or an Intervention nothing in the smoke will answer —
/// which, at the posture the smoke pins, nothing should raise.
fn stuck(snapshot: &SessionSnapshot) -> Option<String> {
    if let Some(turn) = snapshot
        .turns
        .iter()
        .find(|turn| matches!(turn.status, TurnStatus::Failed | TurnStatus::Interrupted))
    {
        return Some(format!("settled a Turn {:?}", turn.status));
    }
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Approval { approval, .. } => Some(format!("asked an Approval: {approval:?}")),
            Activity::Questionnaire { questionnaire, .. } => {
                Some(format!("asked a Questionnaire: {questionnaire:?}"))
            }
            _ => None,
        })
}

fn subagent_rows(snapshot: &SessionSnapshot) -> impl Iterator<Item = &Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
}

/// The one Subagent row a snapshot holds, failing the smoke when the Agent
/// spawned any other.
fn the_subagent_row(snapshot: &SessionSnapshot) -> &Activity {
    let rows = subagent_rows(snapshot).collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        1,
        "the Agent spawned exactly one Subagent: {}",
        describe(snapshot)
    );
    rows[0]
}

/// How the row leading to `child` stands, and how long it says the Subagent
/// worked.
fn row_of(snapshot: &SessionSnapshot, child: SessionId) -> Option<(ActivityStatus, Option<u64>)> {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent {
                session_id,
                status,
                duration_ms,
                ..
            } if *session_id == child => Some((*status, *duration_ms)),
            _ => None,
        })
}

/// Everything the Agent wrote in one Turn, its Messages joined.
fn agent_text_in(snapshot: &SessionSnapshot, turn_id: TurnId) -> String {
    snapshot
        .messages
        .iter()
        .filter(|message| {
            message.turn_id == turn_id
                && message.role == MessageRole::Agent
                && message.status == MessageStatus::Completed
        })
        .map(|message| message.content.trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A Session as a reader of a failed live run needs it: each Turn, and what
/// stands in it, briefly.
fn describe(snapshot: &SessionSnapshot) -> String {
    fn brief(text: &str) -> String {
        const LIMIT: usize = 240;
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        match one_line.char_indices().nth(LIMIT) {
            Some((cut, _)) => format!("{}…", &one_line[..cut]),
            None => one_line,
        }
    }
    let mut lines = vec![format!(
        "Session {} ({:?}, working since {:?})",
        snapshot.session.id, snapshot.session.status, snapshot.session.working_since
    )];
    for (index, turn) in snapshot.turns.iter().enumerate() {
        lines.push(format!(
            "  Turn {index} {:?}{} started {:?} settled {:?}",
            turn.status,
            if turn.prompt_id.is_some() {
                " (Prompt)"
            } else {
                ""
            },
            turn.started_at,
            turn.settled_at
        ));
        for message in snapshot.messages.iter().filter(|m| m.turn_id == turn.id) {
            lines.push(format!(
                "    {:?} {:?}: {}",
                message.role,
                message.status,
                brief(&message.content)
            ));
        }
        for activity in snapshot
            .activities
            .iter()
            .filter(|activity| activity.turn_id() == turn.id)
        {
            lines.push(format!("    {}", brief(&format!("{activity:?}"))));
        }
    }
    lines.join("\n")
}

/// Why the smoke cannot run here, when Suru finds either CLI not installed or
/// not signed in. Anything else wrong with a Provider is the smoke's to report
/// as a failure.
fn unusable(catalog: &ModelCatalog) -> Option<String> {
    ["claude", "codex"].into_iter().find_map(|provider| {
        match &catalog_of(catalog, provider).status {
            ProviderCatalogStatus::Unavailable {
                reason:
                    reason
                    @ (ProviderUnavailability::NotInstalled | ProviderUnavailability::NotSignedIn),
                message,
            } => Some(format!("{provider} is {}: {message}", reason.label())),
            ProviderCatalogStatus::Unavailable { message, .. }
            | ProviderCatalogStatus::Failed { message } => {
                panic!("{provider} cannot be used: {message}")
            }
            _ => None,
        }
    })
}

fn catalog_of<'a>(
    catalog: &'a ModelCatalog,
    provider: &str,
) -> &'a suru::protocol::ProviderModelCatalog {
    catalog
        .providers
        .iter()
        .find(|entry| entry.provider == ProviderId::new(provider))
        .unwrap_or_else(|| panic!("the Server hosts {provider}"))
}

/// The Claude Model the delegating Agent runs on: [`PARENT_MODEL`] where the
/// CLI offers it, and its default otherwise.
fn parent_model(catalog: &ModelCatalog) -> &ModelDescriptor {
    let models = &catalog_of(catalog, "claude").models;
    let available = |model: &&ModelDescriptor| model.availability == ModelAvailability::Available;
    models
        .iter()
        .filter(available)
        .find(|model| model.id.as_str() == PARENT_MODEL)
        .or_else(|| {
            models
                .iter()
                .filter(available)
                .find(|model| model.is_default)
        })
        .expect("Claude offers a Model to run the delegating Agent on")
}

/// What `--version` says of the CLI the Server will launch, which the Server
/// finds as the runtimes do: at `variable` when it is set, else by `name` on
/// the PATH.
fn cli_version(variable: &str, name: &str) -> String {
    let executable = std::env::var_os(variable).unwrap_or_else(|| name.into());
    match Command::new(&executable).arg("--version").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        Ok(output) => format!(
            "{} --version failed: {}",
            Path::new(&executable).display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("{} cannot run: {error}", Path::new(&executable).display()),
    }
}

/// The Execution Directory both Agents work in: a Git repository with one
/// commit and nothing in it worth touching.
fn scratch_repository() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("create the Execution Directory");
    std::fs::write(
        workspace.path().join("README.md"),
        "# Broker smoke\n\nA scratch repository the smoke's Agents work in.\n",
    )
    .expect("write the scratch repository's README");
    for args in [
        &["init", "--quiet", "-b", "main"][..],
        &["add", "README.md"],
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            "Scratch repository",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace.path())
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Suru Smoke")
            .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
            .env("GIT_COMMITTER_NAME", "Suru Smoke")
            .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    workspace
}

/// Writes the Server's own tracing to stderr when `SURU_LOG` names a filter,
/// as the detached Server writes it to its Log file.
fn trace_the_server_when_asked() {
    if let Ok(filter) = std::env::var("SURU_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
}

async fn shut_down(server: server::RunningServer) {
    timeout(Duration::from_secs(60), server.shutdown())
        .await
        .expect("the live Server's shutdown remains bounded")
        .expect("shut down the live Server");
}
