//! The Sessions a Sidekick has a hand in: every Session it acts on — begins,
//! sends a Prompt, interrupts, sets aside, or brings back — is recorded
//! against the Sidekick's Session with the moment of its latest act, across a
//! Server stop, for as long as both Sessions exist. Reading a Session is no
//! act, and neither is an act refused.
//!
//! The tree a Client is given for the Sidekick's Session carries each of them,
//! with its own Subagents beneath it, the one acted on most recently first. A
//! Subsession's tree is its Sidekick's, so it is given through a Subsession
//! too; a Session the Sidekick only acted on heads its own tree, unchanged.
//! None of it makes a Subsession any less a top-level Session of its own: its
//! work keeps no Sidekick Working.
//!
//! Each test acts as the MCP client a Provider harness is, as the rest of the
//! Broker suite does, and asserts on what that client, the Session API and
//! the per-tree subscription observe.

use std::path::{Path, PathBuf};

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::Text};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SubagentTreeEvent},
    protocol::{
        Outlook, SessionListItem, SessionTimestamp, SubagentTreeRevision, SubagentTreeSession,
    },
};

use super::{
    sidekick::{latest_turn_settles, start_sidekick, started_session, working_session},
    *,
};
use crate::subagent_tree::{TreeUpdates, next_change};

/// What the Sidekick first asks of a Session it begins.
const ASKED: &str = "Fix the flaky login test in the auth suite.";

/// The answer a Tool gave the Sidekick, having checked it is no refusal.
async fn acted(client: &mut McpClient, tool: &str, arguments: Value) -> Value {
    let result = timeout(PROGRESS_DEADLINE, client.call_tool(tool, arguments))
        .await
        .unwrap_or_else(|_| panic!("{tool} answers in time"));
    assert_ne!(result["isError"], json!(true), "{tool} answers: {result}");
    result["structuredContent"].clone()
}

/// Has the Sidekick call `tool`, checking it is refused as the Tool's own
/// error.
async fn refused(client: &mut McpClient, tool: &str, arguments: Value) {
    let result = timeout(PROGRESS_DEADLINE, client.call_tool(tool, arguments))
        .await
        .unwrap_or_else(|_| panic!("{tool} answers in time"));
    assert_eq!(result["isError"], json!(true), "{tool} refuses: {result}");
}

/// The Session `begin_session`'s answer names.
fn begun_id(answer: &Value) -> SessionId {
    serde_json::from_value(answer["session_id"].clone())
        .unwrap_or_else(|_| panic!("begin_session names the Session it began: {answer}"))
}

/// `path` as the Server reads every directory: canonically.
fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).expect("read the directory")
}

/// The Sessions a tree lists beneath its Sidekick, in its order, as
/// (Session, whether the Sidekick began it).
fn listed(sessions: &[SubagentTreeSession]) -> Vec<(SessionId, bool)> {
    sessions
        .iter()
        .map(|entry| (entry.session_id, entry.subsession))
        .collect()
}

/// The entry the tree lists for `session_id`.
fn entry_for(sessions: &[SubagentTreeSession], session_id: SessionId) -> &SubagentTreeSession {
    sessions
        .iter()
        .find(|entry| entry.session_id == session_id)
        .unwrap_or_else(|| panic!("the tree lists {session_id}: {sessions:#?}"))
}

/// Follows a tree's changes until `session_id`'s entry joins it or is acted
/// on again: carried whole, with a latest act later than `after`.
async fn acted_on(
    updates: &mut TreeUpdates,
    revision: &mut SubagentTreeRevision,
    session_id: SessionId,
    after: Option<SessionTimestamp>,
) -> SubagentTreeSession {
    loop {
        if let SubagentTreeChange::SessionChanged { entry } = next_change(updates, revision).await
            && entry.session_id == session_id
            && after.is_none_or(|after| entry.acted_at > after)
        {
            return entry;
        }
    }
}

/// Deletes `session_id` as a Client does.
async fn delete(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    let response = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the deletion");
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "it is deleted");
}

/// The database of the stopped Server for `channel` beneath `state_dir`.
fn stored(state_dir: &Path, channel: &str) -> SqliteConnection {
    let config = ServerConfig::new(state_dir, channel).expect("configure server");
    let database = config.data_dir().join("suru.db");
    SqliteConnection::establish(database.to_str().expect("the database's path is UTF-8"))
        .expect("open the stopped Server's database")
}

/// Every act the stopped Server for `channel` beneath `state_dir` holds a
/// record of, as (the Sidekick's Session, the Session it acted on).
fn stored_acts(state_dir: &Path, channel: &str) -> Vec<(String, String)> {
    #[derive(QueryableByName)]
    struct Act {
        #[diesel(sql_type = Text)]
        sidekick_session_id: String,
        #[diesel(sql_type = Text)]
        session_id: String,
    }
    diesel::sql_query("SELECT sidekick_session_id, session_id FROM sidekick_acts")
        .load::<Act>(&mut stored(state_dir, channel))
        .expect("read the recorded acts")
        .into_iter()
        .map(|act| (act.sidekick_session_id, act.session_id))
        .collect()
}

#[tokio::test]
async fn every_act_lists_the_session_by_its_latest_moment_and_reading_or_a_refusal_lists_nothing() {
    const {
        assert!(
            suru::protocol::PROTOCOL_VERSION >= 76,
            "the Sessions a Sidekick's tree carries change the wire"
        );
    }
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-sessions-acts", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (parser, parser_provider) = started_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        "Write the parser",
    )
    .await;
    parser_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, parser, TurnStatus::Completed).await;
    let (docs, mut docs_provider) =
        started_session(&descriptor, &mut hosted.claude, &workspace, "Tidy the docs").await;
    docs_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, docs, TurnStatus::Completed).await;

    // Reading and listing are no acts, and nor is an act refused.
    acted(&mut sidekick, "list_sessions", json!({})).await;
    acted(
        &mut sidekick,
        "read_session",
        json!({ "session_id": parser }),
    )
    .await;
    refused(
        &mut sidekick,
        "interrupt_session",
        json!({ "session_id": docs }),
    )
    .await;
    let (tree, mut updates) = open_tree(&descriptor, sidekick_id).await;
    assert!(
        tree.top_level.sidekick,
        "a Sidekick's Session heads a Sidekick's tree"
    );
    assert_eq!(tree.top_level.session_id, sidekick_id);
    assert!(
        tree.sessions.is_empty(),
        "nothing it only read or was refused stands in its tree: {:#?}",
        tree.sessions
    );
    let mut revision = tree.revision;

    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": parser }),
    )
    .await;
    let set_aside = acted_on(&mut updates, &mut revision, parser, None).await;
    assert!(!set_aside.subsession, "it only acted on the Session");

    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": docs }),
    )
    .await;
    let docs_set_aside = acted_on(&mut updates, &mut revision, docs, None).await;
    assert!(docs_set_aside.acted_at > set_aside.acted_at);

    acted(
        &mut sidekick,
        "unsettle_session",
        json!({ "session_id": parser }),
    )
    .await;
    let brought_back = acted_on(
        &mut updates,
        &mut revision,
        parser,
        Some(set_aside.acted_at),
    )
    .await;
    assert!(
        brought_back.acted_at > docs_set_aside.acted_at,
        "acting again moves the Session's latest act"
    );

    acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": docs, "prompt": "Cover the changelog too." }),
    )
    .await;
    let prompted = acted_on(
        &mut updates,
        &mut revision,
        docs,
        Some(docs_set_aside.acted_at),
    )
    .await;
    assert!(prompted.acted_at > brought_back.acted_at);
    timeout(PROGRESS_DEADLINE, docs_provider.next_turn())
        .await
        .expect("the Prompt begins a Turn")
        .succeed();

    let (interrupted, ()) = tokio::join!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": docs }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, docs_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Session's Provider")
                .succeed();
        },
    );
    assert_eq!(interrupted["outcome"], json!("stopped_work"));
    let stopped = acted_on(&mut updates, &mut revision, docs, Some(prompted.acted_at)).await;

    let subsession = begun_id(
        &acted(
            &mut sidekick,
            "begin_session",
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
    );
    let begun = acted_on(&mut updates, &mut revision, subsession, None).await;
    assert!(begun.subsession, "the Sidekick began it");
    assert!(begun.acted_at > stopped.acted_at);

    let (tree, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        listed(&tree.sessions),
        [(subsession, true), (docs, false), (parser, false)],
        "the tree lists each Session it acted on once, the latest acted on first"
    );
    assert_eq!(
        tree.sessions
            .iter()
            .map(|entry| entry.acted_at)
            .collect::<Vec<_>>(),
        [begun.acted_at, stopped.acted_at, brought_back.acted_at],
        "each by the moment of the Sidekick's latest act on it"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sessions_entry_says_where_and_on_what_it_works_how_its_work_stands_and_its_subagents() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-sessions-entry", None).await;
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
    acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": target, "prompt": "Cover the empty input as well." }),
    )
    .await;
    timeout(PROGRESS_DEADLINE, target_provider.next_turn())
        .await
        .expect("the Prompt begins a Turn")
        .succeed();

    let (tree, mut updates) = open_tree(&descriptor, sidekick_id).await;
    let mut revision = tree.revision;
    let entry = entry_for(&tree.sessions, target);
    assert_eq!(entry.title, "Write the parser");
    assert_eq!(
        entry.workspace_path,
        canonical(&workspace),
        "it names the Workspace the Session works in"
    );
    assert_eq!(
        entry.model,
        Some(default_selection(&claude_models()).model),
        "and the Model its Agent Selection names"
    );
    assert_eq!(
        (entry.status, entry.working_since.is_some()),
        (Some(ActivityStatus::Active), true),
        "it works, and says since when"
    );
    assert!(!entry.needs_intervention);

    // Its own Subagents stand beneath it, never beneath the Sidekick.
    target_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("explore"),
            name: "Explore".to_owned(),
            description: "Map the parser's callers".to_owned(),
            delegation: None,
        })
        .await;
    let spawned = loop {
        if let SubagentTreeChange::SubagentSpawned { entry } =
            next_change(&mut updates, &mut revision).await
        {
            break entry;
        }
    };
    assert_eq!(
        (spawned.parent_session_id, spawned.title.as_str()),
        (target, "Map the parser's callers"),
        "a Subagent the Session spawns stands beneath it"
    );

    target_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: ProviderSubagentId::new("explore"),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    target_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;
    let settled = loop {
        if let SubagentTreeChange::SessionChanged { entry } =
            next_change(&mut updates, &mut revision).await
            && entry.session_id == target
            && entry.status == Some(ActivityStatus::Completed)
        {
            break entry;
        }
    };
    assert_eq!(
        settled.working_since, None,
        "its settled work counts no longer"
    );
    assert!(
        settled.worked_ms.is_some(),
        "and stands at the time its Turn took"
    );
    assert_eq!(
        settled.acted_at,
        entry_for(&tree.sessions, target).acted_at,
        "work moves no act"
    );

    let (tree, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        listed(&tree.sessions),
        [(target, false)],
        "a settled Session stays in the tree"
    );
    assert_eq!(
        tree.subagents
            .iter()
            .map(|entry| (entry.parent_session_id, entry.status))
            .collect::<Vec<_>>(),
        [(target, ActivityStatus::Completed)],
        "with its settled Subagent beneath it"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subsessions_tree_is_its_sidekicks_and_a_session_only_acted_on_heads_its_own() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-sessions-trees", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let acted_only = working_session(&descriptor, &workspace, "Untangle the build").await;
    let _starting = next_start(&mut hosted.claude).await;
    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": acted_only }),
    )
    .await;
    let subsession = begun_id(
        &acted(
            &mut sidekick,
            "begin_session",
            json!({ "directory": workspace, "prompt": ASKED }),
        )
        .await,
    );
    sidekick_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;

    let (through_sidekick, _updates) = open_tree(&descriptor, sidekick_id).await;
    let (through_subsession, _updates) = open_tree(&descriptor, subsession).await;
    assert_eq!(
        (
            &through_subsession.top_level,
            &through_subsession.subagents,
            &through_subsession.sessions,
        ),
        (
            &through_sidekick.top_level,
            &through_sidekick.subagents,
            &through_sidekick.sessions,
        ),
        "a Subsession's tree is its Sidekick's"
    );
    assert_eq!(
        listed(&through_subsession.sessions),
        [(subsession, true), (acted_only, false)]
    );
    assert_eq!(
        through_sidekick.top_level.working_since, None,
        "the Sessions beneath it keep no Sidekick Working"
    );
    assert_eq!(
        entry_for(&through_sidekick.sessions, subsession).status,
        Some(ActivityStatus::Active),
        "while its Subsession works on"
    );

    let (own, _updates) = open_tree(&descriptor, acted_only).await;
    assert_eq!(
        (own.top_level.session_id, own.top_level.sidekick),
        (acted_only, false),
        "a Session only acted on heads its own tree, with no Sidekick above it"
    );
    assert!(own.sessions.is_empty() && own.subagents.is_empty());

    // The Subsession outlives its Sidekick's Session, heading a tree of its
    // own from then on. A reader who opened the Sidekick's Session and then
    // the Subsession goes on following the tree through the Sidekick's
    // Session, and is told it was deleted; one who came to the Subsession
    // first is answered with the Subsession's own tree, and told of no
    // deletion.
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "sidekick-sessions-trees")
            .expect("configure managed client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(20)),
    )
    .await
    .expect("connect managed client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    let mut through_sidekick = client
        .outlook(Outlook::Local)
        .subscribe_subagent_tree(sidekick_id);
    let mut through_subsession = client
        .outlook(Outlook::Local)
        .subscribe_subagent_tree(subsession);
    for (subscription, through) in [
        (&mut through_sidekick, "the Sidekick"),
        (&mut through_subsession, "the Subsession"),
    ] {
        let event = next_tree_event(subscription).await;
        assert!(
            matches!(&event, SubagentTreeEvent::Snapshot(tree) if tree.top_level.session_id == sidekick_id),
            "through {through}, the Sidekick's tree: {event:?}"
        );
    }
    delete(&descriptor, sidekick_id).await;
    assert_eq!(
        next_tree_event(&mut through_sidekick).await,
        SubagentTreeEvent::Deleted,
        "the tree followed through the Sidekick's Session is gone with it"
    );
    let own = loop {
        match next_tree_event(&mut through_subsession).await {
            SubagentTreeEvent::Snapshot(tree) => break tree,
            SubagentTreeEvent::Changed(change) => {
                assert_ne!(change, SubagentTreeChange::TreeDeleted)
            }
            event => panic!("the Subsession was not deleted: {event:?}"),
        }
    };
    assert_eq!(
        (own.top_level.session_id, own.top_level.sidekick),
        (subsession, false),
        "the tree followed through the Subsession is its own now"
    );
    assert!(own.sessions.is_empty());
    let (asked_again, _updates) = open_tree(&descriptor, subsession).await;
    assert_eq!(asked_again.top_level.session_id, subsession);

    hosted.server.shutdown().await.expect("shut down server");
}

/// The next event a managed client's tree subscription delivers.
async fn next_tree_event(
    subscription: &mut suru::managed_client::SubagentTreeSubscription,
) -> SubagentTreeEvent {
    timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("a tree event arrives")
        .expect("the tree subscription stays open")
}

#[tokio::test]
async fn an_act_on_a_subagents_session_names_it_and_lists_the_session_heading_its_tree() {
    const CHANNEL: &str = "sidekick-sessions-subagent";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), CHANNEL, None).await;
    let descriptor = delegating.descriptor.clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut delegating.hosted.claude).await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    let (answer, ()) = tokio::join!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": child_id }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, child_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Subagent's Provider")
                .succeed();
        },
    );
    assert_eq!(answer["outcome"], json!("stopped_work"));

    let (tree, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        listed(&tree.sessions),
        [(delegating.caller, false)],
        "the Session heading the Subagent's tree stands beneath the Sidekick"
    );
    assert!(
        tree.subagents
            .iter()
            .any(|entry| entry.session_id == child_id
                && entry.parent_session_id == delegating.caller),
        "with the Subagent it acted on beneath it: {:#?}",
        tree.subagents
    );

    drop((sidekick, sidekick_provider, child_provider));
    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("stop the server");
    assert_eq!(
        stored_acts(state_dir.path(), CHANNEL),
        [(sidekick_id.to_string(), child_id.to_string())],
        "the record names the Session the Sidekick acted on, the Subagent's own"
    );
}

#[tokio::test]
async fn a_listed_session_is_drawn_from_what_was_stored_of_it_without_reading_its_transcript() {
    const CHANNEL: &str = "sidekick-sessions-unread";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let workspace = workspace.path().to_owned();
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (target, target_provider) =
        started_session(&descriptor, &mut claude, &workspace, "Write the parser").await;
    target_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("explore"),
            name: "Explore".to_owned(),
            description: "Map the parser's callers".to_owned(),
            delegation: None,
        })
        .await;
    target_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: ProviderSubagentId::new("explore"),
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    target_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, target, TurnStatus::Completed).await;
    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": target }),
    )
    .await;
    sidekick_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    let (before, _updates) = open_tree(&descriptor, sidekick_id).await;
    drop((sidekick, sidekick_provider, target_provider));
    server.shutdown().await.expect("stop the server");

    // A Transcript that can no longer be read: reading it would make the
    // Session unreadable, which is how reading it would show.
    {
        use diesel::connection::SimpleConnection;
        stored(state_dir.path(), CHANNEL)
            .batch_execute(&format!(
                "UPDATE messages SET payload = '{{' WHERE session_id = '{target}';"
            ))
            .expect("damage the stored Transcript");
    }

    let (server, _claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (after, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        (after.sessions.clone(), after.subagents.clone()),
        (before.sessions.clone(), before.subagents.clone()),
        "the Session it acted on stands as it did, with its Subagent beneath it"
    );
    let listing = reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the listing");
    assert!(
        listing
            .iter()
            .any(|item| matches!(item, SessionListItem::Readable(summary) if summary.session.id == target)),
        "and its Transcript was never read to draw it: {listing:#?}"
    );
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_sessions_a_sidekick_acted_on_survive_a_restart_until_either_is_deleted() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let workspace = workspace.path().to_owned();
    const CHANNEL: &str = "sidekick-sessions-kept";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (parser, parser_provider) =
        started_session(&descriptor, &mut claude, &workspace, "Write the parser").await;
    parser_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, parser, TurnStatus::Completed).await;
    let (docs, docs_provider) =
        started_session(&descriptor, &mut claude, &workspace, "Tidy the docs").await;
    docs_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, docs, TurnStatus::Completed).await;
    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": parser }),
    )
    .await;
    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": docs }),
    )
    .await;
    sidekick_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    let (before, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(listed(&before.sessions), [(docs, false), (parser, false)]);
    drop((sidekick, parser_provider, docs_provider, sidekick_provider));
    server.shutdown().await.expect("stop the server");

    let (server, _claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (after, mut updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        after.sessions, before.sessions,
        "every Session the Sidekick acted on stands as it did, by the moment of its latest act"
    );
    assert!(after.top_level.sidekick);

    // Deleting a Session it acted on takes it out of the tree, and the record
    // with it.
    let mut revision = after.revision;
    delete(&descriptor, docs).await;
    loop {
        if next_change(&mut updates, &mut revision).await
            == (SubagentTreeChange::SessionLeft { session_id: docs })
        {
            break;
        }
    }
    let (left, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(listed(&left.sessions), [(parser, false)]);
    server.shutdown().await.expect("stop the server");
    assert_eq!(
        stored_acts(state_dir.path(), CHANNEL),
        [(sidekick_id.to_string(), parser.to_string())],
        "the deleted Session's record went with it"
    );

    // Deleting the Sidekick's Session takes its records with it, and leaves
    // the Session it acted on heading its own tree as it always did.
    let (server, _claude) = host_claude(state_dir.path(), config_dir.path(), CHANNEL).await;
    let descriptor = server.descriptor().clone();
    let (kept, _updates) = open_tree(&descriptor, sidekick_id).await;
    assert_eq!(
        listed(&kept.sessions),
        [(parser, false)],
        "a deletion is kept across a stop"
    );
    delete(&descriptor, sidekick_id).await;
    let (own, _updates) = open_tree(&descriptor, parser).await;
    assert_eq!(own.top_level.session_id, parser);
    server.shutdown().await.expect("stop the server");
    assert!(
        stored_acts(state_dir.path(), CHANNEL).is_empty(),
        "the Sidekick's records went with its Session"
    );
}
