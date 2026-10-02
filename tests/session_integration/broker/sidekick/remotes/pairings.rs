//! What a Sidekick is told of a Remote whose Pairing does not stand as it
//! did: one that refuses this Server's key, one restarted speaking another
//! protocol, one unpaired while an Everywhere read was out, one whose answer
//! runs past what this Server reads of a Remote, and the one name no Remote
//! may take.

use suru::protocol::{Peer, SessionError, SessionErrorCode};

use super::*;

/// What a Remote that ended the Pairing on its side is said to have done.
fn revoked() -> String {
    format!(
        "The Remote `{REMOTE}` refused this Suru server's key: its user has ended the Pairing on \
         their side, and reaching it again takes a new Invite."
    )
}

/// What a Remote speaking another protocol is said to do.
fn incompatible() -> String {
    format!(
        "The Remote `{REMOTE}` runs a version of Suru that speaks another protocol than this \
         server's, so nothing can be asked of it until one of them is updated."
    )
}

/// What a Sidekick observes of a Remote that does not answer for `why`:
/// `list_remotes` says it does not, Everywhere names it with no rows of its
/// own, and a read of `session_id` on it is refused.
async fn not_answering(sidekick: &mut McpClient, session_id: SessionId, why: &str) {
    assert_eq!(
        answered(sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": false, "reason": why }] })
    );
    let everywhere = list_sessions(sidekick, json!({ "origin": "everywhere" })).await;
    assert!(
        origins(&everywhere, "sessions").iter().all(Option::is_none),
        "none of the Remote's rows is listed: {everywhere}"
    );
    assert_eq!(
        everywhere["unanswered"],
        json!([{ "origin": REMOTE, "reason": why }])
    );
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": session_id, "origin": REMOTE })
            )
            .await,
        format!("{why} Session `{session_id}` was not read.")
    );
}

#[tokio::test]
async fn a_remote_that_ended_the_pairing_on_its_side_is_named_as_refusing_this_servers_key() {
    let mut pair = paired("sidekick-remotes-revoked", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let bound = working_session(&remote, there.path(), "Bind the ledger").await;
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await),
        ["Bind the ledger"]
    );

    let http = reqwest::Client::new();
    let peers: Vec<Peer> = http
        .get(format!("{}/v1/pairing/peers", remote.base_url))
        .bearer_auth(&remote.token)
        .send()
        .await
        .expect("list the Remote's Peers")
        .json()
        .await
        .expect("decode the Remote's Peers");
    let [peer] = peers.as_slice() else {
        panic!("the Remote has the Sidekick's own Server as its one Peer: {peers:?}");
    };
    http.delete(format!("{}/v1/pairing/peers/{}", remote.base_url, peer.id))
        .bearer_auth(&remote.token)
        .send()
        .await
        .expect("remove the Peer")
        .error_for_status()
        .expect("the Remote ends the Pairing on its side");

    not_answering(&mut sidekick, bound, &revoked()).await;

    pair.shutdown().await;
}

#[tokio::test]
async fn a_remote_speaking_another_protocol_is_named_as_such_and_nothing_of_it_is_read() {
    let mut pair = paired("sidekick-remotes-incompatible", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let bound = working_session(&remote, there.path(), "Bind the ledger").await;
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await),
        ["Bind the ledger"]
    );

    pair.remote = pair.remote.restart_speaking(PROTOCOL_VERSION + 1).await;

    not_answering(&mut sidekick, bound, &incompatible()).await;

    pair.shutdown().await;
}

/// A Remote whose Pairing ends while an Everywhere read waits on another
/// takes its rows with it, though it answered before it was unpaired: what
/// it said is no longer this Server's to give.
#[tokio::test]
async fn a_remote_unpaired_while_everywhere_is_asked_takes_what_it_answered_with_it() {
    let (own, mut claude, _directories) = own_server(
        "sidekick-remotes-unpaired-meanwhile",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
        None,
    )
    .await;
    let own = {
        let descriptor = own.descriptor().clone();
        (own, descriptor)
    };
    let workstation = Serving::start("sidekick-remotes-unpaired-workstation").await;
    let mut laptop = Serving::start("sidekick-remotes-unpaired-laptop").await;
    pair(&own.1, &workstation, REMOTE).await;
    pair(&own.1, &laptop, "laptop").await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remotes");
    working_session(&workstation.descriptor(), there.path(), "Bind the ledger").await;
    working_session(&laptop.descriptor(), there.path(), "Press the atlas").await;
    let (_sidekick, sidekick, _provider) = start_sidekick(&own.1, &mut claude).await;
    let silent = "The Remote `laptop` is not answering: it said nothing within 2 seconds.";

    laptop.route.swallow_connections().await;
    let asked = laptop.route.opened_connections();
    let listing = tokio::spawn(async move {
        let mut sidekick = sidekick;
        let listing = list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await;
        (sidekick, listing)
    });
    // The read is out once the silent Remote has been dialed; the one that
    // answers is unpaired before the silent one's time runs out.
    laptop.route.wait_for_opened_connections(asked + 1).await;
    unpair(&own.1, REMOTE).await;
    let (mut sidekick, everywhere) = listing.await.expect("the listing answers");
    assert_eq!(
        sorted(by_recency(&everywhere)),
        [("Plan the work".to_owned(), None)],
        "nothing of the unpaired Remote is listed: {everywhere}"
    );
    assert_eq!(
        everywhere["unanswered"],
        json!([{ "origin": "laptop", "reason": silent }]),
        "and it is not named, being paired no longer"
    );

    // So too of whether each Remote answers.
    pair(&own.1, &workstation, REMOTE).await;
    let asked = laptop.route.opened_connections();
    let remotes = tokio::spawn(async move {
        let remotes = answered(&mut sidekick, "list_remotes", json!({})).await;
        (sidekick, remotes)
    });
    laptop.route.wait_for_opened_connections(asked + 1).await;
    unpair(&own.1, REMOTE).await;
    let (_sidekick, remotes) = remotes.await.expect("list_remotes answers");
    assert_eq!(
        remotes,
        json!({ "remotes": [{ "name": "laptop", "answers": false, "reason": silent }] })
    );

    own.0
        .shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
    workstation.shutdown().await;
    laptop.shutdown().await;
}

/// Removes the Remote `name` at the Server `own` describes.
async fn unpair(own: &RuntimeDescriptor, name: &str) {
    reqwest::Client::new()
        .delete(format!("{}/v1/pairing/remotes/{name}", own.base_url))
        .bearer_auth(&own.token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
}

/// `everywhere` names every Server at once wherever a Sidekick names an
/// Origin, so no Remote may be named so, in any case: its own name passed
/// back would widen a listing to every Server.
#[tokio::test]
async fn no_remote_may_be_named_everywhere_which_names_every_server() {
    let remote = Serving::start("sidekick-remotes-reserved").await;
    let (own, mut claude, _directories) =
        own_server("sidekick-remotes-reserved", ServerTimings::default(), None).await;
    let descriptor = own.descriptor().clone();
    for name in ["everywhere", "Everywhere", "EVERYWHERE"] {
        let refused = redeem(&descriptor, &remote, name).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{name}");
        let error: SessionError = refused.json().await.expect("decode the refusal");
        assert_eq!(error.code, SessionErrorCode::InvalidRemoteName, "{name}");
        assert_eq!(
            error.message,
            "`everywhere` names every Server at once, so no Remote may be named so; choose \
             another name"
        );
    }
    pair(&descriptor, &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let bound = working_session(&remote.descriptor(), there.path(), "Bind the ledger").await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    let everywhere = list_sessions(&mut sidekick, json!({ "origin": "Everywhere" })).await;
    assert!(
        origins(&everywhere, "sessions").contains(&Some(REMOTE)),
        "`everywhere` in any case ranges over every Server: {everywhere}"
    );
    assert!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": bound, "origin": "EVERYWHERE" })
            )
            .await
            .contains("`everywhere` names no one server"),
        "and names no one Server a Session lives on"
    );

    own.shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
    remote.shutdown().await;
}

/// A Remote is read no further than this Server's budget for one answer, so
/// one answering with more than that — faulty, or worse — cannot exhaust its
/// memory: what it said is not read, and the Sidekick is told why.
#[tokio::test]
async fn a_remote_answering_past_the_byte_budget_is_refused_rather_than_read() {
    let mut pair = paired(
        "sidekick-remotes-budget",
        ServerTimings::default().with_remote_reach_budget(16 * 1024),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (long, provider) = started_session(
        &remote,
        &mut pair.remote.claude,
        there.path(),
        "Explain everything.",
    )
    .await;
    write_agent_message(&provider, &"Every detail. ".repeat(4 * 1024)).await;

    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await),
        ["Explain everything."],
        "an answer within the budget is read"
    );
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": long, "origin": REMOTE })
            )
            .await,
        format!(
            "The Remote `{REMOTE}` answered with more than the 16 KiB this server reads of one \
             answer from a Remote, so nothing it said was read. Session `{long}` was not read."
        )
    );

    pair.shutdown().await;
}
