//! A Remote's Sessions in the Sidekick's own tree: a Session a Sidekick began
//! or acted on at a Remote is recorded against its Session here, since only
//! its own Server knows both ends, and the tree a Client is given for the
//! Sidekick's Session lists it, named by its Remote, as that Remote says of it
//! now — kept current while the tree is watched, with nothing of what the
//! Remote said before once it does not answer, and dropped once the Remote
//! is found no longer to hold it. The record outlives a stop of either
//! Server.

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::Text};
use suru::protocol::{
    ActivityStatus, SubagentTreeChange, SubagentTreeRevision, SubagentTreeSession,
};

use super::*;
use crate::broker::sidekick_acts::acted;
use crate::subagent_tree::{TreeUpdates, next_change, open_tree};

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

    // A Remote that stops answering leaves its Sessions standing with nothing
    // it said before, and gives them back once it answers again.
    remote.route.set_online(false).await;
    let entry = remote_entry(&mut updates, &mut revision, target, |entry| {
        entry.unanswered
    })
    .await;
    assert_eq!(
        (
            entry.title.as_str(),
            entry.workspace_path.as_os_str().is_empty(),
            entry.model,
            entry.status,
        ),
        ("", true, None, None),
        "nothing stale stands beside it"
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
