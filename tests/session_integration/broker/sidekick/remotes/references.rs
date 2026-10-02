//! Every Session a read names by its identity alone — the Session a
//! Subagent's works beneath, a Subagent's or a Subsession's row, the Sidekick
//! that began a Subsession — is one of the Server the read was made at, so the
//! read's own `origin` reaches it, whatever Session the same identity names
//! anywhere else. A Subsession a Remote's Sidekick began on a Server of that
//! Remote's own Pairings is named as this Server reaches it — itself, or a
//! Remote of its own by its own name, known by its key — and otherwise said
//! to be beyond the read, never by the Remote's name for it, which here may
//! name another Server.

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::BigInt};

use super::*;
use crate::broker::sidekick_acts::refused;

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

/// The number of the line in `transcript` that leads into the Subsession
/// `session_id`, to read it whole by, and that line.
fn subsession_line(transcript: &str, session_id: SessionId) -> (String, String) {
    let line = transcript
        .lines()
        .find(|line| line.contains(&format!("subsession [Session {session_id}")))
        .unwrap_or_else(|| panic!("a line leads into {session_id}: {transcript}"));
    let number = line
        .split_once(' ')
        .expect("a line begins with its number")
        .0;
    (number.to_owned(), line.to_owned())
}

/// What `sidekick` reads of the line leading into `subsession` in the
/// Session `session_id` on the Remote [`REMOTE`]: the line, and the line read
/// whole.
async fn read_subsession_row(
    sidekick: &mut McpClient,
    session_id: SessionId,
    subsession: SessionId,
) -> (String, String) {
    let reading = answered(
        sidekick,
        "read_session",
        json!({ "session_id": session_id, "origin": REMOTE, "detail": "activities" }),
    )
    .await;
    let (number, line) = subsession_line(
        reading["transcript"].as_str().expect("a transcript"),
        subsession,
    );
    let whole = answered(
        sidekick,
        "read_session",
        json!({ "session_id": session_id, "origin": REMOTE, "item": number }),
    )
    .await;
    (
        line,
        whole["transcript"]
            .as_str()
            .expect("the line read whole")
            .to_owned(),
    )
}

/// The Remote's Sidekick begins a Subsession on a third Server it knows as
/// `atlas`; this Server knows another Server by that very name, and the
/// Sidekick reading the Remote's is never sent there. Once this Server is
/// paired with the third Server too — by a name of its own — the reading
/// names the Subsession there by that name.
#[tokio::test]
async fn a_subsession_a_remotes_sidekick_began_beyond_it_is_named_as_this_server_reaches_it() {
    const ATLAS: &str = "atlas";
    let mut remote = Serving::start("sidekick-references-beyond").await;
    let third = Serving::start("sidekick-references-beyond-third").await;
    let namesake = Serving::start("sidekick-references-beyond-namesake").await;
    let there = remote.descriptor();
    pair(&there, &third, ATLAS).await;
    let (theirs, mut their_sidekick, _their_provider) =
        start_sidekick(&there, &mut remote.provider).await;
    let workspace = tempfile::tempdir().expect("create a Workspace on the third Server");
    let directory =
        suru::paths::canonical(workspace.path()).expect("read the Workspace canonically");
    let begun = answered(
        &mut their_sidekick,
        "begin_session",
        json!({ "origin": ATLAS, "directory": directory, "prompt": "Chart the atlas." }),
    )
    .await;
    let charting: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("a Subsession is begun");

    let (own, mut claude, _directories) =
        own_server("sidekick-references-beyond", ServerTimings::default(), None).await;
    pair(own.descriptor(), &remote, REMOTE).await;
    pair(own.descriptor(), &namesake, ATLAS).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(own.descriptor(), &mut claude).await;

    let (line, whole) = read_subsession_row(&mut sidekick, theirs, charting).await;
    assert!(
        line.contains(&format!(
            "subsession [Session {charting} on a server the Remote `{REMOTE}` reaches as \
             \"{ATLAS}\"]"
        )),
        "the line names the Server as the Remote reaches it: {line}"
    );
    assert!(
        whole.ends_with(&format!(
            "It works in its own Session, {charting}, on a server the Remote `{REMOTE}` \
             reaches as \"{ATLAS}\" through a Pairing of its own. \"{ATLAS}\" is \
             `{REMOTE}`'s name for that server, not an `origin` to pass, and that Session is \
             not reachable through this read."
        )),
        "and read whole, says it is beyond this read: {whole}"
    );
    for said in [&line, &whole] {
        assert!(
            !said.contains(&format!("on the Remote `{ATLAS}`"))
                && !said.contains(&format!("`origin` \"{ATLAS}\"")),
            "nothing sends the Sidekick to this Server's own `{ATLAS}`: {said}"
        );
    }
    let elsewhere = refused(
        &mut sidekick,
        "read_session",
        json!({ "session_id": charting, "origin": ATLAS }),
    )
    .await;
    assert!(
        elsewhere.starts_with(&format!("Suru holds no Session `{charting}` on the Remote")),
        "which is another Server, holding no such Session: {elsewhere}"
    );

    pair(own.descriptor(), &third, "studio").await;
    let (line, whole) = read_subsession_row(&mut sidekick, theirs, charting).await;
    assert!(
        line.contains(&format!(
            "subsession [Session {charting} on the Remote `studio`]"
        )) && whole.ends_with(&format!(
            "It works in its own Session, {charting}, on the Remote `studio`; read that \
                 Session there, with `origin` \"studio\", for it."
        )),
        "a Server this one is paired with is named by this Server's name for it, known by its \
         key: {line}\n{whole}"
    );
    let reached = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": charting, "origin": "studio" }),
    )
    .await;
    assert_eq!(
        (&reached["session_id"], &reached["begun_by"]),
        (
            &json!(charting),
            &json!(format!("a Sidekick on the Remote `{REMOTE}`"))
        ),
        "where it is read, begun by a Sidekick on the Remote that began it: {reached}"
    );

    own.shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
    for server in [remote, third, namesake] {
        server.shutdown().await;
    }
}

/// A Remote paired back with this Server — as a Peer of its own, by a name
/// of its own — whose Sidekick begins a Subsession here: a reading of that
/// Sidekick's Session names the Subsession as this server's, to be read with
/// no `origin`, not by the Remote's name for this Server nor at the read's.
#[tokio::test]
async fn a_subsession_a_remotes_sidekick_began_on_this_server_is_named_as_this_servers() {
    let mut own = Serving::start("sidekick-references-back-here").await;
    let mut remote = Serving::start("sidekick-references-back-there").await;
    let here = own.descriptor();
    let there = remote.descriptor();
    pair(&here, &remote, REMOTE).await;
    pair(&there, &own, "laptop").await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&here, &mut own.provider).await;
    let (theirs, mut their_sidekick, _their_provider) =
        start_sidekick(&there, &mut remote.provider).await;
    let workspace = tempfile::tempdir().expect("create a Workspace on this Server");
    let directory =
        suru::paths::canonical(workspace.path()).expect("read the Workspace canonically");
    let begun = answered(
        &mut their_sidekick,
        "begin_session",
        json!({ "origin": "laptop", "directory": directory, "prompt": "Chart the atlas." }),
    )
    .await;
    let charting: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("a Subsession is begun");

    let (line, whole) = read_subsession_row(&mut sidekick, theirs, charting).await;
    assert!(
        line.contains(&format!("subsession [Session {charting} on this server]"))
            && whole.ends_with(&format!(
                "It works in its own Session, {charting}, on this server; read that Session, \
                 with no `origin`, for it."
            )),
        "the Subsession is named as this server's: {line}\n{whole}"
    );
    assert!(
        !line.contains("laptop") && !whole.contains("laptop"),
        "never by the Remote's name for this Server: {whole}"
    );
    let reached = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": charting }),
    )
    .await;
    assert!(
        reached["begun_by"]
            .as_str()
            .is_some_and(|begun_by| begun_by.starts_with("a Sidekick on the Peer ")),
        "read here, it is the Subsession a Sidekick on this Server's Peer began: {reached}"
    );
    let at_the_reads_origin = refused(
        &mut sidekick,
        "read_session",
        json!({ "session_id": charting, "origin": REMOTE }),
    )
    .await;
    assert!(
        at_the_reads_origin.starts_with(&format!(
            "Suru holds no Session `{charting}` on the Remote `{REMOTE}`"
        )),
        "and the read's own origin does not reach it: {at_the_reads_origin}"
    );

    own.shutdown().await;
    remote.shutdown().await;
}
