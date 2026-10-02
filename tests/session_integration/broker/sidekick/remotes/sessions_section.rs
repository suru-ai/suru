//! A Remote's Sessions in the Sidekick's own tree: a Session a Sidekick began
//! or acted on at a Remote is recorded against its Session here, since only
//! its own Server knows both ends, and the tree a Client is given for the
//! Sidekick's Session lists it, named by its Remote, as that Remote says of it
//! now — kept current while the tree is watched, keeping only what names it
//! once the Remote does not answer, and dropped once the Remote is found no
//! longer to hold it. One begun there stands in the Sidekick's Transcript as
//! a row naming that Remote. The record outlives a stop of either Server.

use suru::protocol::{
    ActivityStatus, SubagentTreeChange, SubagentTreeEntry, SubagentTreeRevision,
    SubagentTreeSession,
};

use super::*;
use crate::broker::sidekick_acts::acted;
use crate::subagent_tree::{TreeUpdates, next_change, open_tree};
use crate::support::open_catalog_stream_with_snapshot;

/// Follows a tree's changes until the entry of `session_id` on the Remote
/// joins it or moves so that `matches` holds of it, answering that entry.
async fn remote_entry(
    updates: &mut TreeUpdates,
    revision: &mut SubagentTreeRevision,
    session_id: SessionId,
    matches: impl Fn(&SubagentTreeSession) -> bool,
) -> SubagentTreeSession {
    loop {
        if let SubagentTreeChange::SessionChanged { entry } = next_change(updates, revision).await
            && entry.session_id == session_id
            && entry.origin.as_deref() == Some(REMOTE)
            && matches(&entry)
        {
            return entry;
        }
    }
}

/// Deletes `session_id` as a Client of the Server `descriptor` describes
/// does.
async fn delete(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    let response = reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send the deletion");
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "it is deleted");
}

#[tokio::test]
async fn a_session_acted_on_or_begun_on_a_remote_stands_in_the_sidekicks_tree_by_its_remote() {
    let remote = Serving::start("sidekick-remote-section").await;
    let mut own = OwnServer::start(
        "sidekick-remote-section",
        ServerTimings::default().with_remote_retry_interval(Duration::from_millis(50)),
    )
    .await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let mut remote = remote;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let directory = suru::paths::canonical(there.path()).expect("read the Remote's Workspace");
    let (sidekick_id, mut sidekick, _provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let (target, mut target_provider) = started_session(
        &remote.descriptor(),
        &mut remote.provider,
        there.path(),
        "Write the parser",
    )
    .await;
    complete_turn(&remote.descriptor(), target, &target_provider).await;
    let (tree, mut updates) = open_tree(&own.descriptor(), sidekick_id).await;
    let mut revision = tree.revision;
    assert!(tree.sessions.is_empty(), "nothing is acted on yet");

    let (_, ()) = tokio::join!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "origin": REMOTE, "prompt": "Cover the empty input." }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, target_provider.next_turn())
                .await
                .expect("the Prompt begins a Turn on the Remote")
                .succeed();
        },
    );
    let entry = remote_entry(&mut updates, &mut revision, target, |entry| {
        entry.status == Some(ActivityStatus::Active)
    })
    .await;
    assert_eq!(
        (
            entry.title.as_str(),
            &entry.workspace_path,
            entry.model.as_ref().map(|model| model.as_str()),
            entry.unanswered,
            entry.subsession,
        ),
        (
            "Write the parser",
            &directory,
            Some(default_selection(&claude_models()).model.as_str()),
            false,
            false,
        ),
        "a Session acted on at a Remote stands in the Sidekick's tree as that Remote says of it, \
         named by its Remote"
    );

    target_provider.emit(ProviderEvent::TurnCompleted);
    remote_entry(&mut updates, &mut revision, target, |entry| {
        entry.status == Some(ActivityStatus::Completed)
    })
    .await;

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "origin": REMOTE, "directory": directory, "prompt": "Tidy the docs." }),
    )
    .await;
    let begun: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let entry = remote_entry(&mut updates, &mut revision, begun, |_| true).await;
    assert_eq!(
        (entry.title.as_str(), entry.status, entry.subsession),
        ("Tidy the docs.", Some(ActivityStatus::Active), true),
        "a Session begun on a Remote stands there too, working, as the Sidekick's Subsession"
    );

    // A Remote that stops answering leaves its Sessions standing as it last
    // named them, with nothing of their work given as current, and gives them
    // back once it answers again.
    remote.route.set_online(false).await;
    let entry = remote_entry(&mut updates, &mut revision, target, |entry| {
        entry.unanswered
    })
    .await;
    assert_eq!(
        (
            entry.title.as_str(),
            &entry.workspace_path,
            entry.model,
            entry.status,
            entry.worked_ms,
            entry.working_since,
            entry.needs_intervention,
        ),
        (
            "Write the parser",
            &directory,
            None,
            None,
            None,
            None,
            false
        ),
        "what named it stays; nothing of its work stands as current"
    );
    remote.route.set_online(true).await;
    remote_entry(&mut updates, &mut revision, target, |entry| {
        !entry.unanswered && entry.title == "Write the parser"
    })
    .await;

    // Found deleted there, it leaves the tree and is forgotten here.
    delete(&remote.descriptor(), target).await;
    loop {
        if let SubagentTreeChange::SessionLeft { session_id, origin } =
            next_change(&mut updates, &mut revision).await
        {
            assert_eq!(
                (session_id, origin.as_deref()),
                (target, Some(REMOTE)),
                "the Session the Remote no longer holds leaves"
            );
            break;
        }
    }

    // The record outlives a stop: what the Remote still holds is listed
    // again once it is read, and what it was found not to hold is gone.
    drop(updates);
    own = own.restart().await;
    assert_eq!(
        own.stored_remote_acts(),
        [(REMOTE.to_owned(), begun.to_string())],
        "only the Session the Remote still holds is recorded"
    );
    let (tree, mut updates) = open_tree(&own.descriptor(), sidekick_id).await;
    let mut revision = tree.revision;
    let entry = remote_entry(&mut updates, &mut revision, begun, |_| true).await;
    assert_eq!(entry.title, "Tidy the docs.");

    drop(updates);
    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// The Sidekick's own Session, as the Server `descriptor` describes lists it.
async fn listed_summary(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> suru::protocol::SessionSummary {
    let listed: Vec<suru::protocol::SessionListItem> = reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list the Sessions")
        .json()
        .await
        .expect("decode the Sessions");
    listed
        .into_iter()
        .find_map(|item| match item {
            suru::protocol::SessionListItem::Readable(summary)
                if summary.session.id == session_id =>
            {
                Some(*summary)
            }
            _ => None,
        })
        .expect("the Server lists the Session")
}

/// A Session a Sidekick begins on a Remote names only the Peer it came from
/// there, so the Sidekick's own Session names it among the Sessions it began
/// on Remotes, in the summary every Client of its own Server receives and in
/// the change its catalog says — and keeps naming it across a stop. A Session
/// it only acted on there is no Subsession of its, and is not named.
#[tokio::test]
async fn a_sidekicks_session_names_the_sessions_it_began_on_remotes_for_every_client() {
    let remote = Serving::start("sidekick-remote-subsessions").await;
    let mut own = OwnServer::start("sidekick-remote-subsessions", ServerTimings::default()).await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let directory = suru::paths::canonical(there.path()).expect("read the Remote's Workspace");
    let (sidekick_id, mut sidekick, _provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    let acted_on = working_session(&remote.descriptor(), there.path(), "Write the parser").await;
    let (_, mut catalog) = open_catalog_stream_with_snapshot(&own.descriptor()).await;

    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": acted_on, "origin": REMOTE }),
    )
    .await;
    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "origin": REMOTE, "directory": directory, "prompt": "Tidy the docs." }),
    )
    .await;
    let begun: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let named = vec![suru::protocol::RemoteSession {
        origin: REMOTE.to_owned(),
        session_id: begun,
    }];
    let changed = timeout(PROGRESS_DEADLINE, async {
        loop {
            let update = futures_util::StreamExt::next(&mut catalog)
                .await
                .expect("the catalog stream stays open");
            if let suru::protocol::SessionCatalogChange::RemoteSubsessionsChanged {
                session_id,
                remote_subsessions,
            } = update.change
            {
                break (session_id, remote_subsessions);
            }
        }
    })
    .await
    .expect("the catalog says what the Sidekick began");
    assert_eq!(
        changed,
        (sidekick_id, named.clone()),
        "every Client hears the Sidekick's Session began a Session there"
    );
    assert_eq!(
        listed_summary(&own.descriptor(), sidekick_id)
            .await
            .remote_subsessions,
        named,
        "and its summary names that one alone"
    );

    drop(catalog);
    own = own.restart().await;
    assert_eq!(
        listed_summary(&own.descriptor(), sidekick_id)
            .await
            .remote_subsessions,
        named,
        "across a stop"
    );

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// The rows of `snapshot`'s Transcript leading into a Subsession, each by
/// the Session it leads into, the Remote that Session lives on, the Title
/// it names and what it was first asked.
fn subsession_rows(snapshot: &SessionSnapshot) -> Vec<(SessionId, Option<String>, String, String)> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subsession {
                session_id,
                origin,
                title,
                prompt,
                ..
            } => Some((*session_id, origin.clone(), title.clone(), prompt.clone())),
            _ => None,
        })
        .collect()
}

/// A Session a Sidekick begins on a Remote stands in its Transcript as a
/// Subsession row naming that Remote — never a second time — and, while the
/// Remote is kept in view, the row follows the Title the Remote derives for
/// it, across a stop.
#[tokio::test]
async fn a_session_begun_on_a_remote_stands_as_a_row_naming_its_remote_that_follows_its_title() {
    const ASKED: &str = "Tidy the docs.";
    let mut remote = Serving::start("sidekick-remote-subsession-row").await;
    let mut own =
        OwnServer::start("sidekick-remote-subsession-row", ServerTimings::default()).await;
    pair(&own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let directory = suru::paths::canonical(there.path()).expect("read the Remote's Workspace");
    let (sidekick_id, mut sidekick, _provider) =
        start_sidekick(&own.descriptor(), &mut own.claude).await;
    // Watching the Sidekick's tree keeps the Remote in view.
    let (_, updates) = open_tree(&own.descriptor(), sidekick_id).await;

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "origin": REMOTE, "directory": directory, "prompt": ASKED }),
    )
    .await;
    let begun: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let row = |title: &str| {
        vec![(
            begun,
            Some(REMOTE.to_owned()),
            title.to_owned(),
            ASKED.to_owned(),
        )]
    };
    let client = reqwest::Client::new();
    let snapshot = read_session_until(
        &client,
        &own.descriptor(),
        sidekick_id,
        "the Sidekick's Transcript leads into what it began",
        |snapshot| !subsession_rows(snapshot).is_empty(),
    )
    .await;
    assert_eq!(
        subsession_rows(&snapshot),
        row(ASKED),
        "one row names the Session begun, its Remote and what it was first asked"
    );

    let errand = timeout(PROGRESS_DEADLINE, async {
        loop {
            let errand = remote.provider.next_errand().await;
            if errand.prompt().contains(ASKED) && errand.schema()["properties"]["title"].is_object()
            {
                break errand;
            }
        }
    })
    .await
    .expect("the Remote derives the Session's Title through an Errand");
    errand.succeed(json!({ "title": "Docs tidy-up", "icon": "md-bug" }));
    read_session_until(
        &client,
        &own.descriptor(),
        sidekick_id,
        "the row follows the Title the Remote derived",
        |snapshot| subsession_rows(snapshot) == row("Docs tidy-up"),
    )
    .await;

    drop(updates);
    own = own.restart().await;
    assert_eq!(
        subsession_rows(&read_session(&own.descriptor(), sidekick_id).await),
        row("Docs tidy-up"),
        "the row and the Title it took outlive a stop"
    );

    own.server.shutdown().await.expect("stop the own Server");
    remote.shutdown().await;
}

/// A Sidekick's own Server paired with a Remote, the Sidekick's Session
/// started on it, and a Session on the Remote the Sidekick sent a Prompt to,
/// whose first Turn settled.
struct ActedOn {
    remote: Serving,
    own: OwnServer,
    sidekick_id: SessionId,
    sidekick: McpClient,
    _sidekick_provider: ControlledProviderSession,
    target: SessionId,
    _target_provider: ControlledProviderSession,
    _there: tempfile::TempDir,
}

impl ActedOn {
    async fn start(channel: &str, remote: Serving, timings: ServerTimings) -> Self {
        let mut remote = remote;
        let mut own = OwnServer::start(channel, timings).await;
        pair(&own.descriptor(), &remote, REMOTE).await;
        let there = tempfile::tempdir().expect("create a Workspace on the Remote");
        let (sidekick_id, sidekick, sidekick_provider) =
            start_sidekick(&own.descriptor(), &mut own.claude).await;
        let (target, target_provider) = started_session(
            &remote.descriptor(),
            &mut remote.provider,
            there.path(),
            "Write the parser",
        )
        .await;
        complete_turn(&remote.descriptor(), target, &target_provider).await;
        let mut acted_on = Self {
            remote,
            own,
            sidekick_id,
            sidekick,
            _sidekick_provider: sidekick_provider,
            target,
            _target_provider: target_provider,
            _there: there,
        };
        acted(
            &mut acted_on.sidekick,
            "settle_session",
            json!({ "session_id": target, "origin": REMOTE }),
        )
        .await;
        acted_on
    }

    async fn shutdown(self) {
        self.own
            .server
            .shutdown()
            .await
            .expect("stop the own Server");
        self.remote.shutdown().await;
    }
}

/// Follows a tree's changes until a Subagent beneath a Session on the Remote
/// joins it or moves so that `matches` holds of it, answering its entry.
async fn remote_subagent(
    updates: &mut TreeUpdates,
    revision: &mut SubagentTreeRevision,
    matches: impl Fn(&SubagentTreeEntry) -> bool,
) -> SubagentTreeEntry {
    loop {
        if let SubagentTreeChange::SubagentSpawned { entry } = next_change(updates, revision).await
            && entry.origin.as_deref() == Some(REMOTE)
            && matches(&entry)
        {
            return entry;
        }
    }
}

/// A Session acted on at a Remote stands, while its tree is watched, with
/// the Subagents beneath it there as that Remote's own tree says of them,
/// each named by that Remote; and its time is read from its Turns there as
/// a Session's of this Server is, once settled as much as while working.
#[tokio::test]
async fn a_remote_sessions_subagents_stand_beneath_it_as_its_remote_says_of_them() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-subagents",
        Serving::start("sidekick-remote-subagents").await,
        ServerTimings::default(),
    )
    .await;
    let remote = acted_on.remote.descriptor();
    let there = tempfile::tempdir().expect("create another Workspace on the Remote");
    let (spawner, spawner_provider) = started_session(
        &remote,
        &mut acted_on.remote.provider,
        there.path(),
        "Survey the tests",
    )
    .await;
    acted(
        &mut acted_on.sidekick,
        "settle_session",
        json!({ "session_id": spawner, "origin": REMOTE }),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    remote_entry(&mut updates, &mut revision, spawner, |entry| {
        entry.status == Some(ActivityStatus::Active)
    })
    .await;

    spawner_provider
        .emit_attributed_and_wait_until_observed(
            suru::provider::ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentStarted {
                subagent_id: suru::provider::ProviderSubagentId::new("explorer"),
                name: "Explore".to_owned(),
                description: "Survey the flaky tests".to_owned(),
                delegation: None,
            },
        )
        .await;
    let spawned = remote_subagent(&mut updates, &mut revision, |entry| {
        entry.status == ActivityStatus::Active
    })
    .await;
    assert_eq!(
        (
            spawned.parent_session_id,
            spawned.name.as_str(),
            spawned.title.as_str()
        ),
        (spawner, "Explore", "Survey the flaky tests"),
        "the Subagent stands beneath the Session that spawned it there, named by its Remote"
    );

    spawner_provider
        .emit_attributed_and_wait_until_observed(
            suru::provider::ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentCompleted {
                subagent_id: suru::provider::ProviderSubagentId::new("explorer"),
                status: suru::provider::ProviderSubagentStatus::Completed,
            },
        )
        .await;
    remote_subagent(&mut updates, &mut revision, |entry| {
        entry.session_id == spawned.session_id && entry.status == ActivityStatus::Completed
    })
    .await;
    // A Turn's time is only known once it outlasts a moment.
    tokio::time::sleep(Duration::from_millis(5)).await;
    complete_turn(&remote, spawner, &spawner_provider).await;
    let settled = remote_entry(&mut updates, &mut revision, spawner, |entry| {
        entry.status == Some(ActivityStatus::Completed)
    })
    .await;
    assert!(
        settled.worked_ms.is_some_and(|worked| worked > 0),
        "settled, it says how long it worked, as a Session of this Server's does: {settled:?}"
    );
    let (reopened, _) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    assert!(
        reopened.subagents.iter().any(|entry| {
            entry.session_id == spawned.session_id && entry.origin.as_deref() == Some(REMOTE)
        }),
        "a reader opening the tree now is given it: {:?}",
        reopened.subagents
    );

    drop(updates);
    acted_on.shutdown().await;
}

/// Past as many trees of one Remote's Sessions as this Server follows at
/// once, a Session acted on there stands without the Subagents beneath it,
/// saying not all of them are shown, rather than having one more followed.
#[tokio::test]
async fn past_the_trees_followed_of_a_remote_a_session_says_its_subagents_are_not_all_shown() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-trees-bounded",
        Serving::start("sidekick-remote-trees-bounded").await,
        ServerTimings::default().with_remote_watch_limits(suru::server::RemoteWatchLimits {
            trees_per_remote: 1,
            ..suru::server::RemoteWatchLimits::default()
        }),
    )
    .await;
    let remote = acted_on.remote.descriptor();
    let there = tempfile::tempdir().expect("create another Workspace on the Remote");
    let (another, _another_provider) = started_session(
        &remote,
        &mut acted_on.remote.provider,
        there.path(),
        "Survey the tests",
    )
    .await;
    acted(
        &mut acted_on.sidekick,
        "settle_session",
        json!({ "session_id": another, "origin": REMOTE }),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    let mut entries = tree
        .sessions
        .into_iter()
        .map(|entry| (entry.session_id, entry))
        .collect::<std::collections::HashMap<_, _>>();
    let unshown = |entries: &std::collections::HashMap<SessionId, SubagentTreeSession>| {
        entries
            .values()
            .filter(|entry| entry.subagents_unshown)
            .map(|entry| entry.session_id)
            .collect::<Vec<_>>()
    };
    let following_one = timeout(PROGRESS_DEADLINE, async {
        while !(entries.len() == 2 && unshown(&entries).len() == 1) {
            if let SubagentTreeChange::SessionChanged { entry } =
                next_change(&mut updates, &mut revision).await
            {
                entries.insert(entry.session_id, entry);
            }
        }
    })
    .await;
    assert!(
        following_one.is_ok(),
        "one of the two is followed, and the other says its Subagents are not all shown: \
         {entries:?}"
    );
    let mut both = [acted_on.target, another];
    both.sort_by_key(|session_id| session_id.as_uuid());
    assert_eq!(
        unshown(&entries),
        [both[1]],
        "the one past the room left, in a steady order"
    );

    drop(updates);
    acted_on.shutdown().await;
}

/// A Session the Remote's own Sidekick began heads no tree of its own there:
/// the Remote answers for it with its Sidekick's. Acted on from here, it
/// still stands with its own Subagents beneath it, read from its own branch
/// of that tree.
#[tokio::test]
async fn a_remote_session_its_own_sidekick_began_stands_with_its_own_subagents() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-their-subsession",
        Serving::start("sidekick-remote-their-subsession").await,
        ServerTimings::default(),
    )
    .await;
    let remote = acted_on.remote.descriptor();
    let there = tempfile::tempdir().expect("create another Workspace on the Remote");
    let directory = suru::paths::canonical(there.path()).expect("read the Remote's Workspace");
    let (_their_sidekick, mut theirs, _their_provider) =
        start_sidekick(&remote, &mut acted_on.remote.provider).await;
    let begun = acted(
        &mut theirs,
        "begin_session",
        json!({ "directory": directory, "prompt": "Survey the tests" }),
    )
    .await;
    let begun: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let mut begun_provider =
        next_start(&mut acted_on.remote.provider)
            .await
            .succeed(AgentIdentity {
                agent: suru::protocol::AgentId::new("claude-agent"),
                selection: default_selection(&claude_models()),
            });
    timeout(PROGRESS_DEADLINE, begun_provider.next_turn())
        .await
        .expect("its first Turn reaches the Remote's Provider")
        .succeed();
    acted(
        &mut acted_on.sidekick,
        "settle_session",
        json!({ "session_id": begun, "origin": REMOTE }),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    remote_entry(&mut updates, &mut revision, begun, |_| true).await;

    begun_provider
        .emit_attributed_and_wait_until_observed(
            suru::provider::ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentStarted {
                subagent_id: suru::provider::ProviderSubagentId::new("explorer"),
                name: "Explore".to_owned(),
                description: "Survey the flaky tests".to_owned(),
                delegation: None,
            },
        )
        .await;
    let spawned = remote_subagent(&mut updates, &mut revision, |_| true).await;
    assert_eq!(
        (spawned.parent_session_id, spawned.title.as_str()),
        (begun, "Survey the flaky tests"),
        "its Subagent stands beneath it, though its Remote answers for it with its \
         Sidekick's tree"
    );

    drop(updates);
    acted_on.shutdown().await;
}

/// A Remote that falls silent mid-stream — saying nothing at all, not even
/// the keep-alive its stream sends — is held as not answering once the
/// silence limit passes, and its Sessions stand as such until it answers
/// again.
#[tokio::test]
async fn a_remote_falling_silent_mid_stream_is_held_as_not_answering_until_it_answers_again() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-silent-stream",
        Serving::start_keeping_alive("sidekick-remote-silent-stream", Duration::from_millis(50))
            .await,
        ServerTimings::default()
            .with_remote_retry_interval(Duration::from_millis(50))
            .with_remote_silence_limit(Duration::from_millis(400))
            .with_remote_reach_timeout(Duration::from_millis(300)),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    let target = acted_on.target;
    remote_entry(&mut updates, &mut revision, target, |entry| {
        !entry.unanswered
    })
    .await;

    acted_on.remote.route.stall().await;
    remote_entry(&mut updates, &mut revision, target, |entry| {
        entry.unanswered
    })
    .await;

    acted_on.remote.route.set_online(true).await;
    remote_entry(&mut updates, &mut revision, target, |entry| {
        !entry.unanswered
    })
    .await;

    drop(updates);
    acted_on.shutdown().await;
}

/// A Remote whose Pairing ends while a tree listing its Sessions is watched
/// takes them out of the tree at once, as a Remote whose Pairing has ended
/// takes its rows with it.
#[tokio::test]
async fn a_remote_unpaired_while_watched_takes_its_sessions_out_of_the_tree() {
    let acted_on = ActedOn::start(
        "sidekick-remote-unpaired-watched",
        Serving::start("sidekick-remote-unpaired-watched").await,
        ServerTimings::default(),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    let target = acted_on.target;
    remote_entry(&mut updates, &mut revision, target, |_| true).await;

    reqwest::Client::new()
        .delete(format!(
            "{}/v1/pairing/remotes/{REMOTE}",
            acted_on.own.descriptor().base_url
        ))
        .bearer_auth(&acted_on.own.descriptor().token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
    loop {
        if let SubagentTreeChange::SessionLeft { session_id, origin } =
            next_change(&mut updates, &mut revision).await
        {
            assert_eq!((session_id, origin.as_deref()), (target, Some(REMOTE)));
            break;
        }
    }

    drop(updates);
    acted_on.shutdown().await;
}

/// A Remote whose Pairing is removed while its listing is being read again
/// — the Remote saying nothing, so the read would wait out the reach
/// timeout — is let go of at once: nothing of the read outlives the Pairing,
/// and its Sessions leave the tree as soon as it ends.
#[tokio::test]
async fn a_remote_unpaired_while_its_listing_is_read_again_is_let_go_of_at_once() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-unpaired-mid-read",
        Serving::start("sidekick-remote-unpaired-mid-read").await,
        ServerTimings::default()
            .with_remote_reach_timeout(Duration::from_secs(120))
            .with_remote_silence_limit(Duration::from_secs(120)),
    )
    .await;
    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    let target = acted_on.target;
    remote_entry(&mut updates, &mut revision, target, |_| true).await;

    // The Remote falls silent, and a reader opening the tree again has its
    // listing read again, which waits on it.
    acted_on.remote.route.stall().await;
    let (_again, _again_updates) =
        open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    reqwest::Client::new()
        .delete(format!(
            "{}/v1/pairing/remotes/{REMOTE}",
            acted_on.own.descriptor().base_url
        ))
        .bearer_auth(&acted_on.own.descriptor().token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
    let left = timeout(Duration::from_secs(5), async {
        loop {
            if let SubagentTreeChange::SessionLeft { session_id, origin } =
                next_change(&mut updates, &mut revision).await
            {
                break (session_id, origin);
            }
        }
    })
    .await
    .expect("the Remote is let go of without waiting out the read");
    assert_eq!((left.0, left.1.as_deref()), (target, Some(REMOTE)));

    acted_on.remote.route.set_online(true).await;
    drop(updates);
    acted_on.shutdown().await;
}

/// A Remote's Session a Sidekick acted on is dropped once a read of it finds
/// it gone, or a listing of the Remote asked for after the act no longer
/// holds it — and only then: while the Remote's listing is not read, a
/// Session deleted there stays recorded.
#[tokio::test]
async fn a_remote_session_found_gone_by_a_read_or_a_listing_is_dropped() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-found-gone",
        Serving::start("sidekick-remote-found-gone").await,
        ServerTimings::default(),
    )
    .await;
    let target = acted_on.target;
    let remote = acted_on.remote.descriptor();
    let there = tempfile::tempdir().expect("create another Workspace on the Remote");
    let (listed, listed_provider) = started_session(
        &remote,
        &mut acted_on.remote.provider,
        there.path(),
        "Tidy the docs",
    )
    .await;
    complete_turn(&remote, listed, &listed_provider).await;
    acted(
        &mut acted_on.sidekick,
        "settle_session",
        json!({ "session_id": listed, "origin": REMOTE }),
    )
    .await;
    delete(&remote, target).await;
    delete(&remote, listed).await;

    acted_on.own = acted_on.own.restart().await;
    let mut stored = acted_on.own.stored_remote_acts();
    stored.sort();
    let mut both = vec![
        (REMOTE.to_owned(), target.to_string()),
        (REMOTE.to_owned(), listed.to_string()),
    ];
    both.sort();
    assert_eq!(stored, both, "nothing has read the Remote since");

    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&acted_on.own.descriptor(), &mut acted_on.own.claude).await;
    assert!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": target, "origin": REMOTE })
            )
            .await
            .contains("Suru holds no Session"),
        "a read finds it gone"
    );
    acted(&mut sidekick, "list_sessions", json!({ "origin": REMOTE })).await;
    acted_on.own = acted_on.own.restart().await;
    assert_eq!(
        acted_on.own.stored_remote_acts(),
        Vec::<(String, String)>::new(),
        "and so does a listing, and both are dropped"
    );

    acted_on.shutdown().await;
}

/// An act on a Subagent's Session on a Remote stands beneath the Sidekick by
/// the Session heading the Subagent there, as one on its own Server's does,
/// and stays when the Remote's listing of its top-level Sessions is next
/// read.
#[tokio::test]
async fn an_act_on_a_remotes_subagent_stands_by_the_session_heading_it_and_stays() {
    let mut acted_on = ActedOn::start(
        "sidekick-remote-subagent-act",
        Serving::start("sidekick-remote-subagent-act").await,
        ServerTimings::default(),
    )
    .await;
    let remote = acted_on.remote.descriptor();
    let there = tempfile::tempdir().expect("create another Workspace on the Remote");
    let (spawner, spawner_provider) = started_session(
        &remote,
        &mut acted_on.remote.provider,
        there.path(),
        "Survey the tests",
    )
    .await;
    spawner_provider
        .emit_attributed_and_wait_until_observed(
            suru::provider::ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentStarted {
                subagent_id: suru::provider::ProviderSubagentId::new("explorer"),
                name: "Explore".to_owned(),
                description: "Survey the flaky tests".to_owned(),
                delegation: None,
            },
        )
        .await;
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &remote,
        spawner,
        "the Subagent's row stands",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let subagent = snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the row names the Subagent's Session");
    acted(
        &mut acted_on.sidekick,
        "settle_session",
        json!({ "session_id": subagent, "origin": REMOTE }),
    )
    .await;

    let (tree, mut updates) = open_tree(&acted_on.own.descriptor(), acted_on.sidekick_id).await;
    let mut revision = tree.revision;
    let entry = remote_entry(&mut updates, &mut revision, spawner, |_| true).await;
    assert_eq!(
        entry.title, "Survey the tests",
        "the Session heading the Subagent stands beneath the Sidekick"
    );
    drop(updates);
    // What is stored lands a moment after the tree says it.
    let by_its_head = |stored: &[(String, String)]| {
        stored.contains(&(REMOTE.to_owned(), spawner.to_string()))
            && !stored.contains(&(REMOTE.to_owned(), subagent.to_string()))
    };
    let stored = timeout(PROGRESS_DEADLINE, async {
        loop {
            let stored = acted_on.own.stored_remote_acts();
            if by_its_head(&stored) {
                break stored;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        stored.is_ok(),
        "recorded by the Session heading it: {:?}",
        acted_on.own.stored_remote_acts()
    );

    acted_on.shutdown().await;
}
