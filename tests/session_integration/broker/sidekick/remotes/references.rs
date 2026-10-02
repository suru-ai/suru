//! Every Session a read names — the Session a Subagent's works beneath, a
//! Subagent's or a Subsession's row, the Sidekick that began a Subsession —
//! is one of the Server the read was made at, so the read's own `origin`
//! reaches it, whatever Session the same identity names anywhere else.

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::BigInt};

use super::*;

/// A database holding every Session `config`'s Server stored — once it holds
/// each of `sessions` — with each Title marked as this copy's.
async fn copied_sessions(config: &ServerConfig, sessions: &[SessionId], copy: &Path) {
    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }
    let ids = sessions
        .iter()
        .map(|session_id| format!("'{session_id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut database =
        SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
            .expect("open the Remote's database");
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let stored = diesel::sql_query(format!(
                "SELECT COUNT(*) AS count FROM sessions WHERE id IN ({ids})"
            ))
            .get_result::<Count>(&mut database)
            .expect("count the stored Sessions")
            .count;
            if usize::try_from(stored).ok() == Some(sessions.len()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the Remote stores every Session");
    diesel::sql_query(format!("VACUUM INTO '{}'", copy.display()))
        .execute(&mut database)
        .expect("copy the Remote's database");
    let mut copied =
        SqliteConnection::establish(copy.to_str().unwrap()).expect("open the copied database");
    diesel::sql_query("UPDATE sessions SET title = 'Here: ' || title")
        .execute(&mut copied)
        .expect("mark the copies' Titles");
}

/// The Title a reading names its Session by.
fn title(reading: &Value) -> &str {
    reading["title"]
        .as_str()
        .expect("a reading names its Title")
}

#[tokio::test]
async fn every_session_a_remote_read_names_is_reached_at_that_reads_origin() {
    let mut remote = Serving::start("sidekick-remotes-references").await;
    let there = remote.descriptor();
    let workspace = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (theirs, mut their_sidekick, _their_provider) =
        start_sidekick(&there, &mut remote.provider).await;
    let scout = their_sidekick
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "opus",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around the ledger.",
        }))
        .await;
    let (scout_provider, _) =
        run_child(&mut remote.provider, default_selection(&claude_models())).await;
    write_agent_message(&scout_provider, "Nothing amiss.").await;
    let begun = answered(
        &mut their_sidekick,
        "begin_session",
        json!({ "directory": workspace.path(), "prompt": "Chart the atlas." }),
    )
    .await;
    let charting: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("a Subsession is begun");

    // The Sidekick's own Server holds a Session of its own by each of those
    // identities.
    let seed = tempfile::tempdir().expect("create a directory for the copy");
    let seed = seed.path().join("suru.db");
    copied_sessions(&remote.config, &[theirs, scout, charting], &seed).await;
    let (own, mut claude, _directories) = own_server(
        "sidekick-remotes-references",
        ServerTimings::default(),
        Some(&seed),
    )
    .await;
    pair(own.descriptor(), &remote, REMOTE).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(own.descriptor(), &mut claude).await;
    for session_id in [theirs, scout, charting] {
        let here = answered(
            &mut sidekick,
            "read_session",
            json!({ "session_id": session_id }),
        )
        .await;
        assert!(
            title(&here).starts_with("Here: "),
            "{session_id} names a Session of the Sidekick's own Server here: {here}"
        );
    }

    let read_at = |session_id: SessionId| json!({ "session_id": session_id, "origin": REMOTE, "detail": "activities" });
    let scouting = answered(&mut sidekick, "read_session", read_at(scout)).await;
    assert!(!title(&scouting).starts_with("Here: "), "{scouting}");
    assert_eq!(scouting["parent"], json!(theirs));
    let parent = answered(&mut sidekick, "read_session", read_at(theirs)).await;
    assert!(
        !title(&parent).starts_with("Here: "),
        "the parent a Remote's Subagent names is reached at the read's origin: {parent}"
    );
    let transcript = parent["transcript"].as_str().expect("a transcript");
    assert!(
        transcript.contains(&format!("Session {scout}]"))
            && transcript.contains(&format!("subsession [Session {charting}]")),
        "the Remote Sidekick's rows name the Subagent and Subsession it set going: {transcript}"
    );
    let subsession = answered(&mut sidekick, "read_session", read_at(charting)).await;
    assert!(
        !title(&subsession).starts_with("Here: "),
        "a Subsession a Remote's row names is reached at the read's origin: {subsession}"
    );
    assert!(
        subsession["begun_by"]
            .as_str()
            .is_some_and(|begun_by| begun_by.contains(&format!("(Session {theirs})"))),
        "and the Sidekick that began it is the Remote's: {subsession}"
    );

    own.shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
    remote.shutdown().await;
}
