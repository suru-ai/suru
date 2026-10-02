//! A Remote's Sessions in the Sidekick's own tree: a Session a Sidekick began
//! or acted on at a Remote is recorded against its Session here, since only
//! its own Server knows both ends, and the tree a Client is given for the
//! Sidekick's Session lists it, named by its Remote, as that Remote says of it
//! now — kept current while the tree is watched, keeping only what names it
//! once the Remote does not answer, and dropped once the Remote is found no
//! longer to hold it. One begun there stands in the Sidekick's Transcript as
//! a row naming that Remote. The record outlives a stop of either Server.

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::Text};
use suru::protocol::{
    ActivityStatus, SubagentTreeChange, SubagentTreeRevision, SubagentTreeSession,
};

use super::*;
use crate::broker::sidekick_acts::acted;
use crate::subagent_tree::{TreeUpdates, next_change, open_tree};
use crate::support::open_catalog_stream_with_snapshot;

/// The Sidekick's own Server for `channel`, kept by its config so it can be
/// stopped and started again on the same data, paired with what it was.
struct OwnServer {
    server: RunningServer,
    claude: ControlledProvider,
    config: ServerConfig,
    timings: ServerTimings,
    _directories: [tempfile::TempDir; 2],
}

impl OwnServer {
    async fn start(channel: &str, timings: ServerTimings) -> Self {
        let state = tempfile::tempdir().expect("create the own Server's state directory");
        let config_root = tempfile::tempdir().expect("create the own Server's config directory");
        let config = ServerConfig::new(state.path(), format!("{channel}-own"))
            .expect("configure the Sidekick's own Server")
            .with_config_dir(config_root.path());
        let (server, claude) = Self::spawn(&config, &timings).await;
        Self {
            server,
            claude,
            config,
            timings,
            _directories: [state, config_root],
        }
    }

    async fn spawn(
        config: &ServerConfig,
        timings: &ServerTimings,
    ) -> (RunningServer, ControlledProvider) {
        let (runtime, claude) =
            ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
        let server = server::spawn_with_provider_and_timings(
            config.clone(),
            runtime,
            ServerTimings {
                shutdown_grace: Duration::from_millis(5),
                ..timings.clone()
            },
        )
        .await
        .expect("spawn the Sidekick's own Server");
        (server, claude)
    }

    fn descriptor(&self) -> RuntimeDescriptor {
        self.server.descriptor().clone()
    }

    /// The same Server stopped and started again on its own data.
    async fn restart(self) -> Self {
        let Self {
            server,
            config,
            timings,
            _directories,
            ..
        } = self;
        server
            .shutdown()
            .await
            .expect("stop the Sidekick's own Server");
        let (server, claude) = Self::spawn(&config, &timings).await;
        Self {
            server,
            claude,
            config,
            timings,
            _directories,
        }
    }

    /// Every act on a Remote's Session this stopped Server holds a record
    /// of, as (Origin, the Session acted on).
    fn stored_remote_acts(&self) -> Vec<(String, String)> {
        #[derive(QueryableByName)]
        struct Act {
            #[diesel(sql_type = Text)]
            origin: String,
            #[diesel(sql_type = Text)]
            session_id: String,
        }
        let database = self.config.data_dir().join("suru.db");
        let mut database =
            SqliteConnection::establish(database.to_str().expect("the database's path is UTF-8"))
                .expect("open the own Server's database");
        diesel::sql_query("SELECT origin, session_id FROM sidekick_acts WHERE origin <> ''")
            .load::<Act>(&mut database)
            .expect("read the recorded acts")
            .into_iter()
            .map(|act| (act.origin, act.session_id))
            .collect()
    }
}

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
        &mut remote.claude,
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
        (entry.title.as_str(), entry.status),
        ("Tidy the docs.", Some(ActivityStatus::Active)),
        "a Session begun on a Remote stands there too, working"
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
            let errand = remote.claude.next_errand().await;
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
            &mut remote.claude,
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
        &mut acted_on.remote.claude,
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
        &mut acted_on.remote.claude,
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
    let mut stored = acted_on.own.stored_remote_acts();
    stored.sort();
    assert!(
        stored.contains(&(REMOTE.to_owned(), spawner.to_string()))
            && !stored.contains(&(REMOTE.to_owned(), subagent.to_string())),
        "recorded by the Session heading it: {stored:?}"
    );

    acted_on.shutdown().await;
}
