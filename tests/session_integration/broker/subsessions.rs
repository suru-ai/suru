//! A Sidekick beginning a Session on the user's behalf with `begin_session`:
//! a Subsession, begun exactly as the Landing begins a Session — its
//! directory resolved to its Workspace, a new Managed Worktree prepared first
//! where one is asked for, the Landing's Agent Selection where none is
//! chosen, and its Title derived from its first Prompt.
//!
//! A Subsession remembers the Sidekick's Session that began it, and every
//! Client is sent that with it; its first Message names the Sidekick; and the
//! Sidekick's Transcript gains a row of its own leading into it. Otherwise it
//! is an ordinary top-level Session: its work keeps no Sidekick Working, an
//! interrupt or a deletion of the Sidekick's Session leaves it alone, its
//! Usage and Cost are its own, and the user may prompt it. No Sidekick begins
//! a Session in the Sidekick Workspace, and a bare Repository's root takes
//! one only in a new Worktree.
//!
//! Each test acts as the MCP client a Provider harness is, as the rest of the
//! Broker suite does, and asserts on what that client and the Session API
//! observe.

use std::path::{Path, PathBuf};

use diesel::{Connection, RunQueryDsl, SqliteConnection};
use suru::protocol::{
    Author, CheckoutKind, CostTotal, ProviderUnavailability, ResolveWorkspaceRequest,
    ResolvedWorkspace, SessionChange, SessionListItem, SessionTimestamp, TurnId,
};

use super::{
    sidekick::{latest_turn_settles, sidekick_directory, start_sidekick, unoffered},
    *,
};
use crate::{repositories::git, support::refresh_catalog};

/// What the Sidekick first asks of the Subsession it begins.
const ASKED: &str = "Fix the flaky login test in the auth suite.";

/// What a Sidekick is told when it asks to begin a Session in the Sidekick
/// Workspace.
const SIDEKICK_WORKSPACE_BEGINNING: &str = "The directory is the Sidekick Workspace's, and no \
    Sidekick begins a Session there, since its Agent would be a Sidekick too.";

/// What anyone is told when they ask to begin a Session in a Repository's own
/// metadata, a bare Repository's root among them.
const REPOSITORY_METADATA: &str = "The directory is a Repository's own metadata rather than a \
    working copy, so no Session can work there; begin in one of its Worktrees, or ask for a new \
    one.";

/// The author a Sidekick's beginning names: its own Session, by the Title that
/// Session began with.
fn sidekick_author(sidekick: SessionId) -> Author {
    Author::Sidekick {
        session_id: sidekick,
        title: "Plan the work".to_owned(),
    }
}

/// `begin_session`'s answer to `arguments`, having checked it is no refusal.
async fn begun(client: &mut McpClient, arguments: Value) -> Value {
    let result = timeout(
        PROGRESS_DEADLINE,
        client.call_tool("begin_session", arguments),
    )
    .await
    .expect("begin_session answers in time");
    assert_ne!(
        result["isError"],
        json!(true),
        "begin_session answers: {result}"
    );
    result["structuredContent"].clone()
}

/// The id of the Session a `begin_session` answer names.
fn begun_id(answer: &Value) -> SessionId {
    serde_json::from_value(answer["session_id"].clone())
        .unwrap_or_else(|_| panic!("begin_session names the Session it began: {answer}"))
}

/// The words `begin_session` refused `arguments` with, having checked the
/// refusal is the Tool's own error rather than the transport's.
async fn refused(client: &mut McpClient, arguments: Value) -> String {
    let result = timeout(
        PROGRESS_DEADLINE,
        client.call_tool("begin_session", arguments),
    )
    .await
    .expect("begin_session answers in time");
    assert_eq!(
        result["isError"],
        json!(true),
        "begin_session refuses as the Tool's own error: {result}"
    );
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the refusal is given in words: {result}"))
        .to_owned()
}

/// `path` as the Server reads every directory: canonically.
fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).expect("read the directory")
}

/// The Server's listing, as every Client is sent it.
async fn listing(descriptor: &RuntimeDescriptor) -> Vec<SessionListItem> {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("the listing answers")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Session listing")
}

/// The Subsession rows `snapshot`'s Transcript holds, as (the Session each
/// leads into, its Title, what it was first asked, the Turn it stands in).
fn subsession_rows(snapshot: &SessionSnapshot) -> Vec<(SessionId, String, String, TurnId)> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subsession {
                session_id,
                title,
                prompt,
                turn_id,
                ..
            } => Some((*session_id, title.clone(), prompt.clone(), *turn_id)),
            _ => None,
        })
        .collect()
}

/// Makes the Landing begin its next Session with `selection`, as a Client's
/// Landing does when the user picks an Agent there.
async fn choose_landing_agent(descriptor: &RuntimeDescriptor, selection: &AgentSelection) {
    reqwest::Client::new()
        .put(format!(
            "{}/v1/landing-agent-selection",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(selection)
        .send()
        .await
        .expect("choose the Landing's Agent")
        .error_for_status()
        .expect("the Landing takes the Agent");
}

/// Starts the Provider `provider` was just asked to start for a Session on
/// `selection`, and its first Turn, answering that Turn's Provider session
/// and what the Turn was asked.
async fn run_first_turn(
    provider: &mut ControlledProvider,
    selection: AgentSelection,
) -> (ControlledProviderSession, String) {
    let mut session = next_start(provider).await.succeed(AgentIdentity {
        agent: AgentId::new(format!("{}-agent", selection.provider)),
        selection,
    });
    let turn = timeout(PROGRESS_DEADLINE, session.next_turn())
        .await
        .expect("the first Turn reaches the Provider");
    let asked = turn.prompt().to_owned();
    turn.succeed();
    (session, asked)
}

/// A Repository at `root` with one commit, as Git leaves a fresh clone's.
fn committed(root: &Path) {
    std::fs::create_dir_all(root).expect("create the Repository");
    git(root, &["init", "-b", "main"]);
    // Git for Windows turns on autocrlf system-wide, which would check files
    // out with CRLF.
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("README.md"), "The auth suite.\n").expect("write a file");
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
    );
}

/// The Workspace `path` resolves to, as the Landing asks the Server for it.
async fn resolve(descriptor: &RuntimeDescriptor, path: &Path) -> ResolvedWorkspace {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/resolve", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: None,
            path: path.to_owned(),
        })
        .send()
        .await
        .expect("resolve the Workspace")
        .error_for_status()
        .expect("the Workspace is resolved")
        .json::<ResolvedWorkspace>()
        .await
        .expect("decode the Workspace")
}

/// The directories a Repository's managed container holds: one for each
/// Managed Worktree Suru made there.
fn managed_worktrees(repository: &Path) -> Vec<PathBuf> {
    let mut worktrees = std::fs::read_dir(repository.join(".suru-worktrees"))
        .map(|entries| {
            entries
                .map(|entry| entry.expect("read the managed container").path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    worktrees.sort();
    worktrees
}

/// Runs `sql` against the database of the stopped Server for `channel`
/// beneath `state_dir`, as a fault a crash could leave behind.
fn alter_stored(state_dir: &Path, config_dir: &Path, channel: &str, sql: &str) {
    let config = ServerConfig::new(state_dir, channel)
        .expect("configure server")
        .with_config_dir(config_dir);
    let database = config.data_dir().join("suru.db");
    let mut connection =
        SqliteConnection::establish(database.to_str().expect("the database's path is UTF-8"))
            .expect("open the stopped Server's database");
    diesel::sql_query(sql)
        .execute(&mut connection)
        .expect("alter the stored Sessions");
}

#[tokio::test]
async fn a_sidekick_is_offered_begin_session_and_any_other_agent_is_not() {
    const {
        assert!(
            suru::protocol::PROTOCOL_VERSION >= 74,
            "a Subsession's beginning author and its row change the wire, a Remote's included"
        );
    }
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-offered", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_ordinary, ordinary_handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&ordinary_handoff);
    ordinary.initialize().await;

    let listed = sidekick.request("tools/list", json!({})).await;
    let described = listed["tools"]
        .as_array()
        .expect("tools/list lists Tools")
        .iter()
        .find(|described| described["name"] == json!("begin_session"))
        .unwrap_or_else(|| panic!("a Sidekick is offered begin_session: {listed}"));
    assert_eq!(
        described["annotations"]["readOnlyHint"],
        json!(false),
        "begin_session changes what Suru holds: {described}"
    );
    assert_eq!(
        described["inputSchema"]["required"],
        json!(["directory", "prompt"])
    );

    let refused = unoffered(
        &mut ordinary,
        "begin_session",
        json!({ "directory": workspace, "prompt": ASKED }),
    )
    .await;
    assert_eq!(
        refused["message"],
        json!("The Broker offers no Tool named `begin_session`"),
        "any other caller is answered as though begin_session did not exist: {refused}"
    );
    assert_eq!(
        listing(&descriptor).await.len(),
        2,
        "nothing was begun for the caller refused"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sidekick_begins_a_subsession_as_the_landing_would_and_its_transcript_leads_into_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-begun", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let landing = default_selection(&codex_models());
    choose_landing_agent(&descriptor, &landing).await;
    let (_, mut catalog) = crate::attachments::watch_session(&descriptor, sidekick_id).await;

    let answer = begun(
        &mut sidekick,
        json!({ "directory": workspace, "prompt": ASKED }),
    )
    .await;
    let subsession = begun_id(&answer);
    assert_eq!(
        answer,
        json!({
            "session_id": subsession,
            "directory": canonical(&workspace),
            "provider": "codex",
            "model": "gpt-5.5",
        }),
        "a Session begun with no Agent chosen begins with the Landing's"
    );
    let (_subsession_provider, delivered) =
        run_first_turn(&mut hosted.codex, landing.clone()).await;
    assert_eq!(
        delivered, ASKED,
        "its Agent is asked what the Sidekick sent"
    );

    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        subsession,
        "the first Prompt stands as the Subsession's first Message",
        |snapshot| !snapshot.messages.is_empty(),
    )
    .await;
    assert_eq!(snapshot.session.parent, None, "a Subsession is top-level");
    assert_eq!(
        snapshot.session.begun_by,
        Some(sidekick_author(sidekick_id)),
        "it remembers the Sidekick's Session that began it"
    );
    assert_eq!(
        snapshot.session.workspace.path,
        canonical(&workspace),
        "it works in the Workspace its directory resolves to"
    );
    assert_eq!(
        snapshot.title, ASKED,
        "its Title begins as its first Prompt"
    );
    assert_eq!(
        snapshot.prompts[0].author,
        Some(sidekick_author(sidekick_id))
    );
    assert_eq!(
        (
            snapshot.messages[0].role.clone(),
            snapshot.messages[0].content.as_str(),
            snapshot.messages[0].author.clone(),
        ),
        (MessageRole::User, ASKED, Some(sidekick_author(sidekick_id))),
        "its first Message is the Sidekick's, sent on the user's behalf"
    );

    let listed = listing(&descriptor).await;
    let summary = listed
        .iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == subsession)
        })
        .unwrap_or_else(|| panic!("the Subsession is listed like any Session: {listed:?}"));
    assert_eq!(
        summary.session.begun_by,
        Some(sidekick_author(sidekick_id)),
        "every Client is sent the Sidekick that began it with the Session"
    );
    let landed = create_session(
        &descriptor,
        &session_request(&workspace, landing.clone(), "Begun from the Landing"),
    )
    .await;
    assert_eq!(
        summary
            .session
            .approval_posture
            .map(|posture| (posture.value, posture.pinned)),
        landed
            .session
            .approval_posture
            .map(|posture| (posture.value, posture.pinned)),
        "it acts under its Provider's ordinary Approval Posture, as a Session the Landing begins"
    );
    assert!(
        summary.session.approval_posture.is_some(),
        "which it has, being on a Provider"
    );

    let sidekick_snapshot = read_session(&descriptor, sidekick_id).await;
    assert_eq!(
        subsession_rows(&sidekick_snapshot),
        [(
            subsession,
            ASKED.to_owned(),
            ASKED.to_owned(),
            sidekick_snapshot.turns[0].id
        )],
        "the Sidekick's working Turn gains a row naming the Subsession and what it was asked"
    );
    crate::attachments::next_change(&mut catalog, "the Sidekick's row", |change| {
        matches!(
            change,
            SessionChange::ActivityAdded {
                activity: Activity::Subsession { session_id, .. }
            } if *session_id == subsession
        )
    })
    .await;

    // The Title is derived as the Landing's Session's is, on the Subsession's
    // own Provider, and the Sidekick's row names it by the Title it takes.
    let errand = timeout(PROGRESS_DEADLINE, async {
        loop {
            let errand = hosted.codex.next_errand().await;
            if errand.prompt().contains(ASKED) && errand.schema()["properties"]["title"].is_object()
            {
                break errand;
            }
        }
    })
    .await
    .expect("the Subsession's Title is derived through an Errand");
    errand.succeed(json!({ "title": "Fix the flaky login test", "icon": "md-bug" }));
    let sidekick_snapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        sidekick_id,
        "the Sidekick's row takes the Subsession's derived Title",
        |snapshot| {
            subsession_rows(snapshot)
                .iter()
                .any(|(_, title, _, _)| title == "Fix the flaky login test")
        },
    )
    .await;
    assert_eq!(
        subsession_rows(&sidekick_snapshot)[0].2,
        ASKED,
        "and still says what it was first asked"
    );
    assert_eq!(
        read_session(&descriptor, subsession).await.title,
        "Fix the flaky login test"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_agent_runs_the_subsession_and_leaves_the_landings_own_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-agent", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let landing = default_selection(&codex_models());
    choose_landing_agent(&descriptor, &landing).await;

    let chosen = begun(
        &mut sidekick,
        json!({
            "directory": workspace,
            "prompt": ASKED,
            "agent_selection": { "provider": "claude", "model": "haiku" },
        }),
    )
    .await;
    assert_eq!(
        (chosen["provider"].clone(), chosen["model"].clone()),
        (json!("claude"), json!("haiku")),
        "the Agent the Sidekick chose runs the Session: {chosen}"
    );
    let snapshot = read_session(&descriptor, begun_id(&chosen)).await;
    assert_eq!(
        snapshot
            .session
            .agent_selection
            .as_ref()
            .map(|selection| selection.model.as_str()),
        Some("haiku")
    );

    let defaulted = begun(
        &mut sidekick,
        json!({ "directory": workspace, "prompt": "Then tidy the changelog." }),
    )
    .await;
    assert_eq!(
        (defaulted["provider"].clone(), defaulted["model"].clone()),
        (json!("codex"), json!("gpt-5.5")),
        "the Landing still begins with the user's own choice, not the Sidekick's: {defaulted}"
    );

    let refusal = refused(
        &mut sidekick,
        json!({
            "directory": workspace,
            "prompt": ASKED,
            "agent_selection": { "provider": "claude", "model": "sonnet" },
        }),
    )
    .await;
    assert_eq!(
        refusal, "Provider `claude` offers no Model `sonnet`; choose one of `opus`, `haiku`.",
        "an Agent that cannot be chosen is refused as a spawn's is"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subsession_works_apart_from_the_sidekick_that_began_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-apart", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, mut sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let subsession = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
    );
    let (mut subsession_provider, _) =
        run_first_turn(&mut hosted.claude, default_selection(&claude_models())).await;

    // Each reports what it consumed, and each keeps its own.
    let usd = |usd: f64| suru::protocol::Cost::from_usd(usd).expect("a representable Cost");
    let measured = |output: u64| Usage {
        fresh_input_tokens: Some(1_000),
        output_tokens: Some(output),
        ..Usage::default()
    };
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(100),
            cost: Some(MeteredCost::reported(usd(0.20))),
        })
        .await;
    subsession_provider
        .emit_and_wait_until_observed(ProviderEvent::Usage {
            usage: measured(500),
            cost: Some(MeteredCost::reported(usd(0.05))),
        })
        .await;

    // The Sidekick's Turn settles while the Subsession still works.
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    let sidekick_snapshot = read_session(&descriptor, sidekick_id).await;
    let subsession_snapshot = read_session(&descriptor, subsession).await;
    assert_eq!(
        sidekick_snapshot.session.working_since, None,
        "a Subsession's working keeps no Sidekick Working"
    );
    assert!(
        subsession_snapshot.session.working_since.is_some(),
        "while the Subsession works on"
    );
    let settled = |usd_amount: f64| {
        Some(CostTotal {
            cost: usd(usd_amount),
            is_partial: false,
        })
    };
    assert_eq!(
        (sidekick_snapshot.total_cost, sidekick_snapshot.own_cost),
        (settled(0.20), settled(0.20)),
        "nothing the Subsession costs is rolled up beneath the Sidekick"
    );
    assert_eq!(
        sidekick_snapshot
            .total_usage()
            .and_then(|total| total.output_tokens),
        Some(100),
        "nor anything it consumes"
    );
    assert_eq!(
        (
            subsession_snapshot.total_cost.map(|total| total.cost),
            subsession_snapshot.own_cost.map(|total| total.cost),
        ),
        (Some(usd(0.05)), Some(usd(0.05))),
        "the Subsession keeps its own, partial only for the Turn it still works in"
    );

    // Interrupting the Sidekick's Session stops the Sidekick alone.
    admit_prompt(&descriptor, sidekick_id, "Check on it again.").await;
    timeout(PROGRESS_DEADLINE, sidekick_provider.next_turn())
        .await
        .expect("the Sidekick works again")
        .succeed();
    let (interrupted, ()) = tokio::join!(
        reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{sidekick_id}/interrupt",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send(),
        async {
            timeout(PROGRESS_DEADLINE, sidekick_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Sidekick's Provider")
                .succeed();
        },
    );
    interrupted
        .expect("send the interrupt")
        .error_for_status()
        .expect("the Sidekick is interrupted");
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Interrupted).await;
    assert!(
        subsession_provider.try_next_interrupt().is_none(),
        "the interrupt never reaches the Subsession's Provider"
    );
    assert_eq!(
        read_session(&descriptor, subsession).await.turns[0].status,
        TurnStatus::Active,
        "and the Subsession works on"
    );

    // The user may prompt the Subsession as any Session of theirs.
    admit_prompt(&descriptor, subsession, "Also cover the logout test.").await;

    // Deleting the Sidekick's Session leaves the Subsession, still working.
    let deleted = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{sidekick_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the deletion");
    assert_eq!(
        deleted.status(),
        StatusCode::NO_CONTENT,
        "the Sidekick's Session is no longer Working, whatever its Subsession does"
    );
    let snapshot = read_session(&descriptor, subsession).await;
    assert_eq!(snapshot.turns[0].status, TurnStatus::Active);
    assert_eq!(
        snapshot.session.begun_by,
        Some(sidekick_author(sidekick_id)),
        "it still remembers which Sidekick began it"
    );
    assert!(
        listing(&descriptor)
            .await
            .iter()
            .any(|item| item.id() == subsession),
        "and is still listed"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_new_worktree_is_prepared_for_a_subsession_and_a_bare_root_takes_one_only_so() {
    let temporary = tempfile::tempdir().expect("create a home for the Repositories");
    let root = canonical(temporary.path());
    let main = root.join("auth");
    committed(&main);
    let bare = root.join("auth.git");
    git(
        &root,
        &[
            "clone",
            "--bare",
            main.to_str().expect("the Repository's path is UTF-8"),
            bare.to_str().expect("the bare Repository's path is UTF-8"),
        ],
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-worktree", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let answer = begun(
        &mut sidekick,
        json!({ "directory": main, "prompt": ASKED, "new_worktree": true }),
    )
    .await;
    let subsession = begun_id(&answer);
    let directory = PathBuf::from(
        answer["directory"]
            .as_str()
            .expect("the answer says where the Subsession works"),
    );
    assert_eq!(
        directory.parent(),
        Some(main.join(".suru-worktrees").as_path()),
        "a new Worktree is made in the Repository's managed container: {answer}"
    );
    let snapshot = read_session(&descriptor, subsession).await;
    assert_eq!(snapshot.session.execution_directory.path, directory);
    let checkout = snapshot
        .session
        .checkout
        .as_ref()
        .expect("the Subsession works in a Worktree");
    assert_eq!(checkout.kind, CheckoutKind::Linked);
    assert_eq!(checkout.root, directory);
    assert_eq!(
        snapshot.session.workspace.id,
        resolve(&descriptor, &main).await.workspace.id,
        "it belongs to the Workspace of the Repository it was begun from"
    );
    assert_eq!(
        snapshot.session.begun_by,
        Some(sidekick_author(sidekick_id))
    );
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&directory)
        .args(["branch", "--show-current"])
        .output()
        .expect("read the Worktree's branch");
    assert!(
        String::from_utf8_lossy(&branch.stdout).starts_with("suru/"),
        "on a branch Suru named: {}",
        String::from_utf8_lossy(&branch.stdout)
    );

    assert_eq!(
        refused(&mut sidekick, json!({ "directory": bare, "prompt": ASKED })).await,
        REPOSITORY_METADATA,
        "a bare Repository's root is no place a Session can work"
    );
    let answer = begun(
        &mut sidekick,
        json!({ "directory": bare, "prompt": "Bump the auth dependency.", "new_worktree": true }),
    )
    .await;
    assert_eq!(
        PathBuf::from(answer["directory"].as_str().expect("the answer says where")).parent(),
        Some(bare.join(".suru-worktrees").as_path()),
        "but a new Worktree of it is: {answer}"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn no_sidekick_begins_a_session_in_the_sidekick_workspace_or_where_none_can_work() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-refusals", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let sidekick_workspace = sidekick_directory(&descriptor).await;

    for new_worktree in [false, true] {
        assert_eq!(
            refused(
                &mut sidekick,
                json!({
                    "directory": sidekick_workspace,
                    "prompt": "Take over from me.",
                    "new_worktree": new_worktree,
                }),
            )
            .await,
            SIDEKICK_WORKSPACE_BEGINNING,
            "no Sidekick begins another, in a new Worktree or not"
        );
    }
    #[cfg(unix)]
    {
        let linked = workspace.join("sidekick-link");
        std::os::unix::fs::symlink(&sidekick_workspace, &linked)
            .expect("link to the Sidekick Workspace");
        for new_worktree in [false, true] {
            assert_eq!(
                refused(
                    &mut sidekick,
                    json!({
                        "directory": linked,
                        "prompt": "Take over from me.",
                        "new_worktree": new_worktree,
                    }),
                )
                .await,
                SIDEKICK_WORKSPACE_BEGINNING,
                "however the Sidekick Workspace is reached"
            );
        }
    }
    let gone = workspace.join("gone");
    assert_eq!(
        refused(&mut sidekick, json!({ "directory": gone, "prompt": ASKED }),).await,
        format!(
            "There is no directory {} on this Suru server for a Session to work in.",
            gone.display()
        )
    );
    assert_eq!(
        refused(
            &mut sidekick,
            json!({ "directory": "auth/suite", "prompt": ASKED }),
        )
        .await,
        format!(
            "begin_session's `directory` must be an absolute path; {} is not one.",
            Path::new("auth/suite").display()
        )
    );
    assert_eq!(
        refused(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": " \n " }),
        )
        .await,
        "begin_session's `prompt` is empty; say what the Session's Agent is to do."
    );
    assert_eq!(
        refused(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": ASKED, "new_worktree": true }),
        )
        .await,
        "No new Worktree was made, so no Session was begun: A new Worktree can only be made in a \
         Repository, and this directory is not in one.",
        "a new Worktree needs a Repository to make it in"
    );
    assert_eq!(
        refused(
            &mut sidekick,
            json!({
                "directory": workspace,
                "prompt": ASKED,
                "agent_selection": { "provider": "elsewhere", "model": "any" },
            }),
        )
        .await,
        "Suru hosts no Provider `elsewhere`; the Providers it hosts are `claude`, `codex`, \
         `copilot`. Call list_providers to see which may be chosen."
    );

    let listed = listing(&descriptor).await;
    assert_eq!(
        listed.iter().map(SessionListItem::id).collect::<Vec<_>>(),
        [sidekick_id],
        "nothing refused was begun"
    );
    assert!(
        subsession_rows(&read_session(&descriptor, sidekick_id).await).is_empty(),
        "and the Sidekick's Transcript gained no row for any of it"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subsession_and_the_row_leading_into_it_survive_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "subsession-kept").await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let subsession = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace.path(), "prompt": ASKED }),
        )
        .await,
    );
    let (subsession_provider, _) =
        run_first_turn(&mut claude, default_selection(&claude_models())).await;
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        subsession,
        "the first Message stands",
        |snapshot| !snapshot.messages.is_empty(),
    )
    .await;
    drop((sidekick_provider, subsession_provider));
    server.shutdown().await.expect("stop the server");

    let (server, _claude) =
        host_claude(state_dir.path(), config_dir.path(), "subsession-kept").await;
    let descriptor = server.descriptor().clone();
    let snapshot = read_session(&descriptor, subsession).await;
    assert_eq!(
        snapshot.session.begun_by,
        Some(sidekick_author(sidekick_id))
    );
    assert_eq!(
        snapshot.prompts[0].author,
        Some(sidekick_author(sidekick_id))
    );
    assert_eq!(
        snapshot.messages[0].author,
        Some(sidekick_author(sidekick_id))
    );
    assert!(
        listing(&descriptor).await.iter().any(|item| {
            item.readable().is_some_and(|summary| {
                summary.session.id == subsession
                    && summary.session.begun_by == Some(sidekick_author(sidekick_id))
            })
        }),
        "the listing still names the Sidekick that began it"
    );
    assert_eq!(
        subsession_rows(&read_session(&descriptor, sidekick_id).await)
            .into_iter()
            .map(|(session_id, _, prompt, _)| (session_id, prompt))
            .collect::<Vec<_>>(),
        [(subsession, ASKED.to_owned())],
        "and the Sidekick's Transcript still leads into it"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_landing_agent_that_cannot_be_used_is_refused_before_anything_is_begun() {
    let temporary = tempfile::tempdir().expect("create a home for the Repository");
    let main = canonical(temporary.path()).join("auth");
    committed(&main);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let mut hosted = host_providers(
        state_dir.path(),
        "subsession-landing-agent",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    choose_landing_agent(&descriptor, &default_selection(&codex_models())).await;
    // The user signs out of the Landing's Provider after choosing it; the
    // Landing remembers the choice, which the user can fix outside Suru.
    hosted.runtimes[1].set_unavailable(Some(ProviderUnavailability::NotSignedIn));
    refresh_catalog(&descriptor).await;

    for (directory, new_worktree) in [(&workspace, false), (&main, true)] {
        let refusal = refused(
            &mut sidekick,
            json!({ "directory": directory, "prompt": ASKED, "new_worktree": new_worktree }),
        )
        .await;
        assert!(
            refusal.starts_with(
                "The user's Landing would begin the Session with Provider `codex`, which cannot \
                 be used now:"
            ) && refusal.ends_with(
                "Choose another Agent with `agent_selection`; list_providers says which may be \
                 chosen."
            ),
            "a defaulted Agent is held to what a chosen one is: {refusal}"
        );
    }
    assert!(
        hosted.codex.try_next_start().is_none(),
        "no Session was begun to fail on it"
    );
    assert!(
        managed_worktrees(&main).is_empty(),
        "and no Worktree was made for one"
    );

    for setting in [
        SettingMutation::ProviderClaudeEnabled { value: Some(false) },
        SettingMutation::ProviderCodexEnabled { value: Some(false) },
    ] {
        mutate_setting(&descriptor, setting).await;
    }
    assert_eq!(
        refused(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
        "No Provider Suru hosts can begin a Session now: each is turned off or cannot be used. \
         Ask the user to turn one on, or call list_providers to see why.",
        "with every Provider off or unusable, the Landing has no Agent to begin with"
    );
    assert_eq!(
        refused(
            &mut sidekick,
            json!({
                "directory": workspace,
                "prompt": ASKED,
                "agent_selection": { "provider": "claude", "model": "opus" },
            }),
        )
        .await,
        "Provider `claude` is turned off in Suru; ask the user to turn it back on, or choose \
         another Provider."
    );
    assert_eq!(
        listing(&descriptor)
            .await
            .iter()
            .map(SessionListItem::id)
            .collect::<Vec<_>>(),
        [sidekick_id],
        "nothing refused was begun"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// Whether a Provider that may be chosen will start is no question beginning
/// a Session asks, the Landing's or a Sidekick's: the Subsession is begun,
/// and its first Turn fails saying why.
#[tokio::test]
async fn a_provider_that_fails_at_launch_still_begins_the_subsession_as_the_landings_would() {
    const UNAVAILABLE: &str = "the codex CLI is not signed in";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-launch-fails", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    choose_landing_agent(&descriptor, &default_selection(&codex_models())).await;

    let subsession = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
    );
    next_start(&mut hosted.codex)
        .await
        .fail_unavailable(ProviderUnavailability::NotSignedIn, UNAVAILABLE);
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        subsession,
        "the Subsession's first Turn fails",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
        },
    )
    .await;
    assert!(
        snapshot.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { text, .. } if text.contains(UNAVAILABLE)
        )),
        "saying why: {:?}",
        snapshot.activities
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_worktree_kept_after_a_failed_beginning_is_where_its_sidekicks_retry_begins() {
    let temporary = tempfile::tempdir().expect("create a home for the Repository");
    let main = canonical(temporary.path()).join("auth");
    committed(&main);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-worktree-retry", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_first_id, mut first, _first_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_second_id, mut second, _second_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let asked = json!({ "directory": main, "prompt": ASKED, "new_worktree": true });

    hosted.runtimes[0].fail_skill_discovery("the Skill catalog is offline");
    let refusal = refused(&mut first, asked.clone()).await;
    let (why, kept_as) = refusal
        .split_once("\"preparation\": \"")
        .unwrap_or_else(|| panic!("the refusal names the kept preparation: {refusal}"));
    assert!(
        why.contains("Destination Skills could not refresh")
            && why.ends_with(
                "The new Worktree is kept: call begin_session again with the same `directory` \
                 and "
            ),
        "the Sidekick is told its Worktree waits for a retry: {refusal}"
    );
    let named = kept_as
        .split_once('"')
        .map(|(named, _)| named.to_owned())
        .expect("the preparation is quoted");
    let kept = managed_worktrees(&main);
    assert_eq!(kept.len(), 1, "the Worktree made is kept: {kept:?}");
    hosted.runtimes[0].clear_skill_discovery_failure();
    let retry = json!({
        "directory": main,
        "prompt": ASKED,
        "new_worktree": true,
        "preparation": named,
    });

    // Each Subsession holds its Repository's checkout lease until its first
    // Turn begins, as the Landing's Session does, so each is started in turn.
    let claude = default_selection(&claude_models());
    let elsewhere = begun(&mut second, retry.clone()).await;
    run_first_turn(&mut hosted.claude, claude.clone()).await;
    assert_ne!(
        PathBuf::from(
            elsewhere["directory"]
                .as_str()
                .expect("the answer says where")
        ),
        kept[0],
        "the name means nothing to another Sidekick: {elsewhere}"
    );
    let retried = begun(&mut first, retry).await;
    run_first_turn(&mut hosted.claude, claude.clone()).await;
    assert_eq!(
        PathBuf::from(
            retried["directory"]
                .as_str()
                .expect("the answer says where")
        ),
        kept[0],
        "the Sidekick's retry begins in the Worktree its failed beginning kept: {retried}"
    );
    assert_eq!(
        managed_worktrees(&main).len(),
        2,
        "and makes none of its own"
    );

    let again = begun(&mut first, asked).await;
    run_first_turn(&mut hosted.claude, claude).await;
    assert_ne!(
        begun_id(&again),
        begun_id(&retried),
        "asking the same afresh begins another Session"
    );
    assert_eq!(
        managed_worktrees(&main).len(),
        3,
        "in a Worktree of its own"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Subsession and its row are written separately, so a Server that stops
/// between the two can leave the Subsession without its row; one whose
/// Title changed while its Sidekick's Session was not read leaves the row
/// naming the Title before. Reading the Sidekick's Session again puts both
/// right.
#[tokio::test]
async fn a_lost_or_stale_row_is_put_right_when_the_sidekicks_session_is_read_again() {
    const CHANNEL: &str = "subsession-rows-repaired";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let lost = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace.path(), "prompt": ASKED }),
        )
        .await,
    );
    // The lost row's Subsession is titled before its row goes, so the row
    // put back names the Title it has now rather than the one it began with.
    let errand = timeout(PROGRESS_DEADLINE, async {
        loop {
            let errand = claude.next_errand().await;
            if errand.prompt().contains(ASKED) && errand.schema()["properties"]["title"].is_object()
            {
                break errand;
            }
        }
    })
    .await
    .expect("the Subsession's Title is derived through an Errand");
    errand.succeed(json!({ "title": "Flaky login test", "icon": "md-bug" }));
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        lost,
        "the Subsession takes its derived Title",
        |snapshot| snapshot.title == "Flaky login test",
    )
    .await;
    let renamed = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace.path(), "prompt": "Then tidy the changelog." }),
        )
        .await,
    );
    drop(sidekick_provider);
    server.shutdown().await.expect("stop the server");

    alter_stored(
        state_dir.path(),
        config_dir.path(),
        CHANNEL,
        &format!(
            "DELETE FROM activities WHERE session_id = '{sidekick_id}' \
             AND payload LIKE '%\"subsession\"%' AND payload LIKE '%{lost}%'"
        ),
    );
    alter_stored(
        state_dir.path(),
        config_dir.path(),
        CHANNEL,
        &format!("UPDATE sessions SET title = 'Renamed while away' WHERE id = '{renamed}'"),
    );

    let (server, _claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let mut rows = subsession_rows(&read_session(&descriptor, sidekick_id).await)
        .into_iter()
        .map(|(session_id, title, prompt, _)| (session_id, title, prompt))
        .collect::<Vec<_>>();
    rows.sort_by_key(|(session_id, ..)| *session_id == lost);
    assert_eq!(
        rows,
        [
            (
                renamed,
                "Renamed while away".to_owned(),
                "Then tidy the changelog.".to_owned()
            ),
            (lost, "Flaky login test".to_owned(), ASKED.to_owned()),
        ],
        "the lost row is back, naming its Subsession as it is now, and the stale one follows \
         the Title its Subsession took meanwhile"
    );

    server.shutdown().await.expect("shut down server");
}

/// `read_session`'s answer to `arguments`, having checked it is no refusal.
async fn reads(client: &mut McpClient, arguments: Value) -> Value {
    let result = timeout(
        PROGRESS_DEADLINE,
        client.call_tool("read_session", arguments),
    )
    .await
    .expect("read_session answers in time");
    assert_ne!(
        result["isError"],
        json!(true),
        "read_session answers: {result}"
    );
    result["structuredContent"].clone()
}

/// The number a reading's transcript gives the line of the Subsession row
/// leading into `subsession`, such as "1.3".
fn subsession_item(transcript: &str, subsession: SessionId) -> String {
    transcript
        .lines()
        .find_map(|line| {
            line.split_once(&format!(" subsession [Session {subsession}]:"))
                .map(|(number, _)| number.to_owned())
        })
        .unwrap_or_else(|| panic!("the transcript leads into {subsession}: {transcript}"))
}

/// What the listing says of `session_id`: how it stands, and when it last
/// moved.
async fn listed_as(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> (Option<suru::protocol::SessionStanding>, SessionTimestamp) {
    listing(descriptor)
        .await
        .into_iter()
        .find(|item| item.id() == session_id)
        .map(|item| (item.standing(), item.updated_at()))
        .unwrap_or_else(|| panic!("{session_id} is listed"))
}

/// A later Sidekick reading this one's Session can follow what it began, and
/// reading the Subsession itself says which Sidekick began it.
#[tokio::test]
async fn a_later_sidekick_reading_this_ones_session_follows_the_subsession_it_began() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subsession-read", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let subsession = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
    );
    run_first_turn(&mut hosted.claude, default_selection(&claude_models())).await;
    let (_later_id, mut later, _later_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let reading = reads(
        &mut later,
        json!({ "session_id": sidekick_id, "detail": "activities" }),
    )
    .await;
    let transcript = reading["transcript"].as_str().expect("a transcript");
    let item = subsession_item(transcript, subsession);
    assert!(
        transcript.contains(&format!(
            "{item} subsession [Session {subsession}]: \"{ASKED}\", first asked: {ASKED}"
        )),
        "the row names the Subsession's Session, its Title and what it was first asked: \
         {transcript}"
    );
    assert_eq!(
        reading["begun_by"],
        json!(null),
        "a Sidekick's own Session was begun by the user"
    );
    let whole = reads(
        &mut later,
        json!({ "session_id": sidekick_id, "item": item }),
    )
    .await;
    assert!(
        whole["transcript"]
            .as_str()
            .is_some_and(|whole| whole.ends_with(&format!(
                "It works in its own Session, {subsession}; read that Session for it."
            ))),
        "read whole, it says where to follow it: {whole}"
    );

    let followed = reads(&mut later, json!({ "session_id": subsession })).await;
    assert_eq!(
        followed["begun_by"],
        json!(format!(
            "Sidekick \"Plan the work\" (Session {sidekick_id})"
        )),
        "reading the Subsession says which Sidekick began it, as a Sidekick's Message names one"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// Reading a Sidekick's Session through `read_session` loads it as the
/// Session API does, which puts a row a stop lost right — and that is all a
/// read changes: no Turn is added, the Session's Standing and last movement
/// stand as they were, and a read finding nothing to put right changes
/// nothing at all.
#[tokio::test]
async fn reading_a_sidekicks_session_back_puts_a_lost_row_right_and_changes_nothing_else() {
    const CHANNEL: &str = "subsession-read-repairs";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let subsession = begun_id(
        &begun(
            &mut sidekick,
            json!({ "directory": workspace.path(), "prompt": ASKED }),
        )
        .await,
    );
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    let before = read_session(&descriptor, sidekick_id).await;
    let listed_before = listed_as(&descriptor, sidekick_id).await;
    drop(sidekick_provider);
    server.shutdown().await.expect("stop the server");
    alter_stored(
        state_dir.path(),
        config_dir.path(),
        CHANNEL,
        &format!(
            "DELETE FROM activities WHERE session_id = '{sidekick_id}' \
             AND payload LIKE '%\"subsession\"%'"
        ),
    );

    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    assert_eq!(
        listed_as(&descriptor, sidekick_id).await,
        listed_before,
        "the listing reads the Sidekick's Session as it was, before anything loads it"
    );
    let (_later_id, mut later, _later_provider) = start_sidekick(&descriptor, &mut claude).await;
    let reading = reads(
        &mut later,
        json!({ "session_id": sidekick_id, "detail": "activities" }),
    )
    .await;
    subsession_item(
        reading["transcript"].as_str().expect("a transcript"),
        subsession,
    );

    let after = read_session(&descriptor, sidekick_id).await;
    assert_eq!(
        subsession_rows(&after)
            .into_iter()
            .map(|(session_id, _, prompt, turn_id)| (session_id, prompt, turn_id))
            .collect::<Vec<_>>(),
        [(subsession, ASKED.to_owned(), before.turns[0].id)],
        "the lost row is back, in the Sidekick's latest Turn, where it was lost from"
    );
    assert_eq!(
        after.turns.len(),
        before.turns.len(),
        "putting it right adds no Turn"
    );
    assert_eq!(
        listed_as(&descriptor, sidekick_id).await,
        listed_before,
        "nor moves how the Sidekick's Session stands or when it last moved"
    );

    reads(
        &mut later,
        json!({ "session_id": sidekick_id, "detail": "activities" }),
    )
    .await;
    assert_eq!(
        read_session(&descriptor, sidekick_id).await.revision,
        after.revision,
        "a read finding nothing to put right changes nothing"
    );

    server.shutdown().await.expect("shut down server");
}
