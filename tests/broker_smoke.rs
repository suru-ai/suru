#![cfg(unix)]

//! The Broker end to end against the installed, signed-in CLIs (spec #407): a
//! real Claude Session's Agent spawns a real Codex Subagent through the Broker,
//! the Subagent's Turn settles, and its Subagent Report wakes the Claude Agent
//! into a Continuation where it answers.
//!
//! Every other Broker test drives Suru against Provider doubles and scripted
//! stand-ins; this one is the check that the real harnesses do what those
//! doubles were written to: that Claude loads the Broker from its MCP config,
//! finds its Tools, and calls them unasked; that Codex takes the Broker on its
//! thread and runs a brokered Subagent at the posture derived for it; and that
//! Claude answers a Report as a Turn of its own. It spends real Model calls, so
//! it runs only when asked twice — the ignore attribute and `SURU_BROKER_SMOKE=1`
//! — and skips when Suru finds either CLI not installed or not signed in:
//!
//! ```text
//! SURU_BROKER_SMOKE=1 cargo nextest run --test broker_smoke --run-ignored ignored-only --no-capture
//! ```
//!
//! `SURU_CLAUDE_PATH` and `SURU_CODEX_PATH` name other binaries, as they do for
//! Suru itself, and `SURU_LOG` writes the Server's own tracing to stderr. What
//! it asserts, and the versions it last passed against, are recorded in
//! `docs/validation/0421-broker-smoke.md`.

#[path = "support/mod.rs"]
#[allow(dead_code)]
mod server_support;

use std::{ffi::OsStr, path::Path, process::Command, sync::Arc};

use server_support::{PROGRESS_DEADLINE, receive_initial_state};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription, SubagentTreeEvent},
    protocol::{
        Activity, ActivityStatus, AgentSelection, ApprovalPosture, ClaudePermissionMode,
        CodexApprovalPolicy, CodexSandboxMode, CreateSessionRequest, Delegator, ExecutionDirectory,
        InitialPrompt, MessageRole, MessageStatus, ModelAvailability, ModelCatalog,
        ModelDescriptor, PromptId, ProviderCatalogStatus, ProviderId, ProviderUnavailability,
        SessionId, SessionSnapshot, TurnStatus,
    },
    provider::{ClaudeRuntime, CodexRuntime},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, Instant, timeout};

const GATE: &str = "SURU_BROKER_SMOKE";

/// The Server's channel, which names nothing outside the smoke's own state
/// directory.
const CHANNEL: &str = "broker-installed-smoke";

/// What the smoke asks of the Claude Agent. It names the Broker but none of
/// its Tools, which the Agent learns from the note Suru appends to its system
/// prompt. The Subagent sleeps before it answers so that its Report reaches a
/// Claude Agent whose Turn has settled — the Continuation this smoke is about —
/// rather than steering the Turn that spawned it.
const PROMPT: &str = "Using Suru's Broker, spawn one Subagent on the codex Provider, on that \
Provider's default Model, named echo. Tell it to run the shell command `sleep 8` and then reply \
with exactly the word PONG and nothing else. Do not do its task yourself, and do not wait for it \
or check on it: once it is spawned, end your turn straight away. Suru will bring you its Report \
as a new message; when it arrives, answer with the single word DONE followed by the Subagent's \
reply.";

/// The Subagent's name, as the Prompt gives it.
const SUBAGENT_NAME: &str = "echo";

/// The Claude Model the delegating Agent runs on: the cheapest the CLI offers,
/// which is capable enough to find a deferred Tool and follow the Prompt.
const PARENT_MODEL: &str = "haiku";

/// How long each live stretch — the spawn, the Subagent's work, the Report's
/// Continuation — may take before the smoke calls it a failure. It is a
/// failure deadline for real CLIs calling real Models, not an expected wait:
/// every wait below returns the moment what it waits for arrives.
const LIVE_STEP_DEADLINE: Duration = Duration::from_secs(300);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "set SURU_BROKER_SMOKE=1 to use the installed, signed-in Claude and Codex binaries"]
async fn a_claude_agent_spawns_a_codex_subagent_through_the_broker_and_answers_its_report() {
    if std::env::var_os(GATE).as_deref() != Some(OsStr::new("1")) {
        eprintln!("skipping: set {GATE}=1 to opt in");
        return;
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
    // Claude's bypassPermissions derives Codex's never with danger-full-access for the Subagent
    // (ADR 0036), so nothing in the tree asks; and no Title or Workspace Icon is derived, which
    // would spend Model calls the smoke has no stake in.
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
        ServerConfig::new(state_dir.path(), CHANNEL)
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
        ManagedClientConfig::new(state_dir.path(), CHANNEL).expect("configure client"),
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
        return;
    }
    let parent_model = parent_model(&catalog);
    let codex_default = catalog_of(&catalog, "codex")
        .models
        .iter()
        .find(|model| model.is_default)
        .map_or_else(|| "none".to_owned(), |model| model.id.to_string());
    println!(
        "parent Model: claude/{}; Codex's default Model: {codex_default}",
        parent_model.id
    );

    // The Claude Session, at the posture the Config Document pinned.
    let prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("claude"),
                model: parent_model.id.clone(),
                options: Vec::new(),
            }),
            execution_directory: ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: PROMPT.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create the Claude Session");
    let parent_id = created.session.id;
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
    let mut parent = Watched::subscribe(&client, parent_id, "the Claude Session").await;

    // The Agent spawns the Subagent: a brokered row in the Turn the Prompt began. An Agent
    // whose Turn settles without one has given up, and is read as it stands then.
    let spawned = parent
        .read_until(
            &mut client,
            LIVE_STEP_DEADLINE,
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
    let (row_turn, child_id) = match the_subagent_row(&spawned) {
        Activity::Subagent {
            turn_id,
            name,
            session_id,
            brokered,
            ..
        } => {
            assert!(
                brokered,
                "the row is the Broker's, not Claude's own Agent tool's"
            );
            assert!(
                name.eq_ignore_ascii_case(SUBAGENT_NAME),
                "the row names the Subagent as the spawn did: {name}"
            );
            (*turn_id, *session_id)
        }
        _ => unreachable!("the_subagent_row answers a Subagent row"),
    };
    let prompt_turn = &spawned.turns[0];
    assert_eq!(prompt_turn.prompt_id, Some(prompt_id));
    assert_eq!(
        row_turn, prompt_turn.id,
        "the row stands in the Turn whose Agent spawned the Subagent"
    );
    println!(
        "{:>6.1}s: the Claude Agent spawned {child_id} through the Broker",
        began.elapsed().as_secs_f64()
    );

    // The child stands under the parent in the tree, and nowhere Sessions are listed.
    let mut tree = client.subscribe_subagent_tree(parent_id);
    let SubagentTreeEvent::Snapshot(tree_snapshot) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree opens")
        .expect("the Subagent tree stream stays open")
    else {
        panic!("the Subagent tree stream opens with its snapshot");
    };
    drop(tree);
    assert_eq!(tree_snapshot.top_level.session_id, parent_id);
    let entry = tree_snapshot
        .subagents
        .iter()
        .find(|entry| entry.session_id == child_id)
        .unwrap_or_else(|| panic!("the tree lists the Subagent: {tree_snapshot:?}"));
    assert_eq!(entry.parent_session_id, parent_id);
    assert!(entry.name.eq_ignore_ascii_case(SUBAGENT_NAME));
    let listed = client
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

    // The Codex Subagent's Session, opened by the Delegation.
    let mut child = Watched::subscribe(&client, child_id, "the Codex Subagent's Session").await;
    let delegated = child
        .read_until(
            &mut client,
            LIVE_STEP_DEADLINE,
            "the Delegation reaches the Codex Subagent",
            |snapshot| {
                snapshot
                    .messages
                    .iter()
                    .any(|message| matches!(message.role, MessageRole::Delegation(_)))
            },
        )
        .await;
    assert_eq!(delegated.session.parent, Some(parent_id));
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
        delegated.session.execution_directory, created.session.execution_directory,
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
            session_id: parent_id,
            name: None,
        }),
        "the Subagent's Transcript opens with the Delegation, from the Claude Session's Agent"
    );
    assert!(
        delegation.content.contains("PONG"),
        "the Delegation carries the task the Agent was asked to hand on: {}",
        delegation.content
    );

    // The Subagent's Turn settles, and its row with it.
    let child_settled = child
        .read_until(
            &mut client,
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
    let child_turn = &child_settled.turns[0];
    assert_eq!(
        child_turn.status,
        TurnStatus::Completed,
        "{}",
        describe(&child_settled)
    );
    let child_model = child_turn
        .agent
        .as_ref()
        .map(|identity| identity.selection.clone())
        .expect("the Codex Turn records the Agent that ran it");
    assert_eq!(child_model.provider, ProviderId::new("codex"));
    let child_reply = agent_text_in(&child_settled, child_turn.id);
    assert!(
        child_reply.contains("PONG"),
        "the Subagent did what it was delegated: {child_reply:?}"
    );
    println!(
        "{:>6.1}s: the Codex Subagent settled on {}: {child_reply:?}",
        began.elapsed().as_secs_f64(),
        child_model.model
    );

    // The Report wakes the settled Claude Agent into a Continuation, where it answers.
    let answered = parent
        .read_until(
            &mut client,
            LIVE_STEP_DEADLINE,
            "the Report wakes the Claude Agent into a Continuation that settles",
            |snapshot| {
                row_of(snapshot, child_id)
                    .is_some_and(|(status, _)| status != ActivityStatus::Active)
                    && snapshot.turns.len() > 1
                    && snapshot.turns.iter().all(|turn| turn.status.is_terminal())
                    && snapshot.working_since().is_none()
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
    println!(
        "{:>6.1}s: the Report woke the Claude Agent, which answered {answer:?}",
        began.elapsed().as_secs_f64()
    );

    // The tree reads the Subagent settled, on the Model its Provider confirmed.
    let mut tree = client.subscribe_subagent_tree(parent_id);
    let SubagentTreeEvent::Snapshot(settled_tree) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree opens")
        .expect("the Subagent tree stream stays open")
    else {
        panic!("the Subagent tree stream opens with its snapshot");
    };
    drop(tree);
    let entry = settled_tree
        .subagents
        .iter()
        .find(|entry| entry.session_id == child_id)
        .expect("the tree still lists the Subagent");
    assert_eq!(entry.status, ActivityStatus::Completed);
    assert_eq!(
        entry.model.as_ref(),
        Some(&child_model.model),
        "the tree shows the Model the Subagent's Provider confirmed"
    );
    assert_eq!(settled_tree.top_level.working_since, None);

    drop(parent);
    drop(child);
    drop(client);
    shut_down(server).await;
    println!(
        "{:>6.1}s: the Server shut down",
        began.elapsed().as_secs_f64()
    );
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

    /// Reads the Session until `wanted` holds, failing the smoke at `deadline`
    /// — or at once, should the Session reach a state `wanted` can no longer
    /// follow from — with the Session as it stood. The managed client's own
    /// events are read here too, so a smoke that has no use for them never
    /// leaves the client backed up behind them.
    async fn read_until(
        &mut self,
        client: &mut ManagedClient,
        deadline: Duration,
        awaited: &str,
        wanted: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        let what = self.what;
        let waited = timeout(deadline, async {
            loop {
                let snapshot = client
                    .read_session(self.session_id)
                    .await
                    .unwrap_or_else(|error| panic!("read {what}: {error}"));
                if wanted(&snapshot) {
                    return snapshot;
                }
                if let Some(stuck) = stuck(&snapshot) {
                    panic!(
                        "{awaited}, but {what} {stuck}: {}",
                        describe(&snapshot)
                    );
                }
                tokio::select! {
                    event = self.feed.next() => {
                        event
                            .unwrap_or_else(|| panic!("{what}'s stream stays open"))
                            .unwrap_or_else(|error| panic!("{what}'s stream stays readable: {error}"));
                    }
                    event = client.next() => {
                        event.expect("the managed client stays connected");
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
fn agent_text_in(snapshot: &SessionSnapshot, turn_id: suru::protocol::TurnId) -> String {
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
