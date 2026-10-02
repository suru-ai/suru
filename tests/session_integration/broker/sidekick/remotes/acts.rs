//! A Sidekick acting on a Remote's Sessions and Workspaces: each acting Tool
//! takes the `origin` a row gave, and its own Server carries the act to that
//! Remote through the Pairing exactly as it carries a Client's (ADR 0044).
//!
//! What arrives on the Remote is a Sidekick's on the Peer it came from, named
//! by that Peer's name and by nothing the Sidekick's own Server said of the
//! Sidekick's Session, so there is nothing there to follow back. The Remote
//! refuses such an act on a Session of its own Sidekick Workspace, and a
//! beginning there, though it may be read. An act on a Remote that does not
//! answer is refused saying so, and nothing is kept to do there later. A
//! Client acts as the user, whichever Server it reaches a Session through, and
//! can name no one else as performing its act.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use suru::{
    protocol::{
        AUTHOR_HEADER, Author, Peer, QuestionnaireOutcome, SessionChange, SessionError,
        SessionErrorCode, SessionListItem,
    },
    provider::{ProviderEventAttribution, ProviderSubagentId},
};

use super::*;
use crate::attachments::{next_change, watch_session};
use crate::broker::sidekick::answering::{ask, stood, where_to_run};
use crate::broker::sidekick_acts::{SIDEKICK_WORKSPACE_REFUSAL, acted, refused};

/// The one Peer the Remote `remote` describes is paired with: the
/// Sidekick's own Server, as the Remote knows it.
async fn the_peer(remote: &RuntimeDescriptor) -> Peer {
    let peers: Vec<Peer> = reqwest::Client::new()
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
    peer.clone()
}

/// The Peer `author` names, whichever of its acts it was: the act the Peer
/// named stands beside it, and only the Peer can tell its acts apart by it.
pub(super) fn of_the_peer(author: Option<Author>) -> Option<Author> {
    author.map(|author| match author {
        Author::PeerSidekick {
            peer,
            fingerprint,
            act,
        } => {
            assert!(act.is_some(), "a Sidekick's act on a Remote names itself");
            Author::PeerSidekick {
                peer,
                fingerprint,
                act: None,
            }
        }
        sidekick @ Author::Sidekick { .. } => sidekick,
    })
}

/// Who the Remote `remote` says sent what a Sidekick on its one Peer sent: a
/// Sidekick on that Peer, by the Peer's name, whichever act it was (see
/// [`of_the_peer`]).
async fn by_the_peer(remote: &RuntimeDescriptor) -> Author {
    let peer = the_peer(remote).await;
    assert_ne!(
        peer.name, peer.fingerprint,
        "a Peer is known by the name it gave itself"
    );
    Author::PeerSidekick {
        peer: peer.name,
        fingerprint: peer.fingerprint,
        act: None,
    }
}

/// The Prompt a Session holds with `text`, where it holds one.
fn prompt_saying<'a>(
    snapshot: &'a SessionSnapshot,
    text: &str,
) -> Option<&'a suru::protocol::Prompt> {
    snapshot.prompts.iter().find(|prompt| prompt.text == text)
}

/// Who sent the user Message saying `text` in a Session, for each such
/// Message — a Sidekick on a Peer by that Peer, whichever act it was.
fn senders_of(snapshot: &SessionSnapshot, text: &str) -> Vec<Option<Author>> {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User && message.content == text)
        .map(|message| of_the_peer(message.author.clone()))
        .collect()
}

/// The words a Remote is refused an act with when it does not answer at all.
fn nothing_done(why: &str) -> String {
    format!("{why} Nothing was done there, and nothing is kept to do once it answers.")
}

/// A Client's admission of `text` to `session_id` at `url` — a Server's own
/// Session API, or a Remote's reached through its Peer — naming `author` as
/// the one who sent it, as no Client may.
async fn admits_naming(
    url: &str,
    token: &str,
    session_id: SessionId,
    text: &str,
    author: &Author,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/sessions/{session_id}/prompts"))
        .bearer_auth(token)
        .header(
            AUTHOR_HEADER,
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(author).expect("an author serializes")),
        )
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("send the admission")
}

#[tokio::test]
async fn a_prompt_sent_to_a_remote_stands_there_as_a_sidekicks_on_this_peer_by_its_name() {
    const ASKED: &str = "Pick the parser back up where it stopped.";
    let mut pair = paired("sidekick-remote-send", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (target, mut target_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Write the parser",
    )
    .await;
    complete_turn(&remote, target, &target_provider).await;
    let (_, mut watched) = watch_session(&remote, target).await;

    assert_eq!(
        acted(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "origin": REMOTE, "prompt": ASKED }),
        )
        .await,
        json!({ "session_id": target, "origin": REMOTE, "admitted": "new_turn" }),
        "the Remote takes the Prompt as a Client's, and says how, naming the Session by its \
         Remote as its row does"
    );
    let turn = timeout(PROGRESS_DEADLINE, target_provider.next_turn())
        .await
        .expect("the Prompt begins a Turn on the Remote");
    assert_eq!(turn.prompt(), ASKED, "the Remote's Agent is asked it");
    turn.succeed();

    let by_peer = by_the_peer(&remote).await;
    let streamed = next_change(&mut watched, "the Sidekick's Message", |change| {
        matches!(change, SessionChange::MessageAdded { message } if message.content == ASKED)
    })
    .await;
    let SessionChange::MessageAdded { message } = streamed else {
        unreachable!("the change was found as a Message");
    };
    assert_eq!(
        of_the_peer(message.author),
        Some(by_peer.clone()),
        "every Client of the Remote is sent the Message as a Sidekick's on this Peer, and \
         nothing of the Sidekick's own Session"
    );
    let snapshot = read_session(&remote, target).await;
    assert_eq!(
        prompt_saying(&snapshot, ASKED).map(|prompt| of_the_peer(prompt.author.clone())),
        Some(Some(by_peer.clone()))
    );
    assert_eq!(senders_of(&snapshot, ASKED), [Some(by_peer.clone())]);
    assert_eq!(
        senders_of(&snapshot, "Write the parser"),
        [None],
        "the Remote's user's own words stay theirs"
    );

    let reading = acted(
        &mut sidekick,
        "read_session",
        json!({ "session_id": target, "origin": REMOTE }),
    )
    .await;
    let Author::PeerSidekick { peer, .. } = &by_peer else {
        unreachable!("the Remote names a Sidekick on its Peer");
    };
    let transcript = reading["transcript"].as_str().expect("a transcript");
    assert!(
        transcript.contains(&format!("sent by a Sidekick on this server: {ASKED}"))
            && !transcript.contains(peer.as_str()),
        "a reading here names the Sidekick on the Peer the Remote stores — known by its key \
         to be this server — as this server, not by the Remote's name for it: {transcript}"
    );

    pair.remote = pair.remote.restart_speaking(PROTOCOL_VERSION).await;
    let restored = read_session(&pair.remote.descriptor(), target).await;
    assert_eq!(
        senders_of(&restored, ASKED),
        [Some(by_peer)],
        "and keeps it across a restart"
    );

    pair.shutdown().await;
}

/// Two Peers giving themselves one name — two Servers on one machine here —
/// are told apart by the second's fingerprint, so what a Sidekick on each
/// sends is attributed to that Peer alone, by a name and a key the Serving
/// user sees beside each other in the Peers they list.
#[tokio::test]
async fn two_peers_giving_one_name_are_told_apart_in_what_their_sidekicks_send() {
    const ASKED: &str = "Pick the parser back up.";
    let mut servers = paired("sidekick-remote-same-names", ServerTimings::default()).await;
    let (second, mut second_claude, _directories) = own_server(
        "sidekick-remote-same-names-second",
        ServerTimings::default(),
        None,
    )
    .await;
    pair(second.descriptor(), &servers.remote, REMOTE).await;
    let remote = servers.remote.descriptor();
    let peers: Vec<Peer> = reqwest::Client::new()
        .get(format!("{}/v1/pairing/peers", remote.base_url))
        .bearer_auth(&remote.token)
        .send()
        .await
        .expect("list the Remote's Peers")
        .json()
        .await
        .expect("decode the Remote's Peers");
    let [first_peer, second_peer] = peers.as_slice() else {
        panic!("the Remote has both Servers as Peers: {peers:?}");
    };
    assert_eq!(
        second_peer.name,
        format!("{} ({})", first_peer.name, &second_peer.fingerprint[..8]),
        "the second Peer giving the same name is told apart by its fingerprint"
    );

    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let target = working_session(&remote, there.path(), "Write the parser").await;
    let own = servers.own.descriptor().clone();
    let (_first, mut first, _first_provider) = start_sidekick(&own, &mut servers.claude).await;
    let (_second, mut second_sidekick, _second_provider) =
        start_sidekick(second.descriptor(), &mut second_claude).await;
    for sidekick in [&mut first, &mut second_sidekick] {
        acted(
            sidekick,
            "send_prompt",
            json!({ "session_id": target, "origin": REMOTE, "prompt": ASKED, "delivery": "queue" }),
        )
        .await;
    }
    let attributed = read_session(&remote, target)
        .await
        .prompts
        .into_iter()
        .filter(|prompt| prompt.text == ASKED)
        .map(|prompt| of_the_peer(prompt.author))
        .collect::<Vec<_>>();
    assert_eq!(
        attributed,
        [first_peer, second_peer].map(|peer| Some(Author::PeerSidekick {
            peer: peer.name.clone(),
            fingerprint: peer.fingerprint.clone(),
            act: None,
        })),
        "each Peer's Sidekick is named by its own Peer"
    );

    second
        .shutdown()
        .await
        .expect("shut down the second Server");
    servers.shutdown().await;
}

#[tokio::test]
async fn a_client_names_no_author_whichever_server_it_reaches_a_session_through() {
    const FORGED: &str = "Delete the release branch.";
    let pair = paired("sidekick-remote-forged", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let target = working_session(&remote, there.path(), "Write the parser").await;
    let claimed = Author::PeerSidekick {
        peer: "a machine of its choosing".to_owned(),
        fingerprint: "0".repeat(64),
        act: None,
    };

    for (through, url, token) in [
        (
            "the Remote's own Session API",
            remote.base_url.clone(),
            &remote.token,
        ),
        (
            "its own Server, turned toward the Remote",
            format!("{}/v1/remotes/{REMOTE}", own.base_url),
            &own.token,
        ),
    ] {
        let answered = admits_naming(&url, token, target, FORGED, &claimed).await;
        assert_eq!(
            answered.status(),
            StatusCode::BAD_REQUEST,
            "a Client naming an author through {through} is refused"
        );
        let error: SessionError = answered.json().await.expect("decode the refusal");
        assert_eq!(error.code, SessionErrorCode::InvalidCommand);
        assert!(
            error.message.starts_with("A Client acts as the user"),
            "{through}: {}",
            error.message
        );
    }
    assert_eq!(
        prompt_saying(&read_session(&remote, target).await, FORGED),
        None,
        "and nothing it sent was admitted"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn each_act_on_a_remotes_session_is_done_there_as_a_clients_is() {
    let mut pair = paired("sidekick-remote-acts", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (asking, mut asking_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Run the tests.",
    )
    .await;
    let by_peer = by_the_peer(&remote).await;

    // An Answer.
    let questionnaire = where_to_run();
    ask(&remote, asking, &asking_provider, &questionnaire).await;
    let (answered, _) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Remote's Agent")
        },
    );
    assert_eq!(
        answered,
        json!({
            "session_id": asking,
            "origin": REMOTE,
            "questionnaire_id": questionnaire.id,
            "answered": true,
        })
    );
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        &remote,
        asking,
        "the Questionnaire is answered",
        |snapshot| {
            stood(snapshot, questionnaire.id)
                .is_some_and(|(outcome, ..)| outcome == QuestionnaireOutcome::Answered)
        },
    )
    .await;
    assert_eq!(
        stood(&snapshot, questionnaire.id).and_then(|(_, _, author)| of_the_peer(author)),
        Some(by_peer.clone()),
        "the Answer stands as a Sidekick's on this Peer"
    );

    // An interrupt.
    let (interrupted, ()) = tokio::join!(
        acted(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": asking, "origin": REMOTE }),
        ),
        async {
            timeout(PROGRESS_DEADLINE, asking_provider.next_interrupt())
                .await
                .expect("the interrupt reaches the Remote's Provider")
                .succeed();
        },
    );
    assert_eq!(
        interrupted,
        json!({ "session_id": asking, "origin": REMOTE, "outcome": "stopped_work" }),
        "the interrupt stops the Remote's work as a Client's does"
    );

    // Setting it aside, and bringing it back.
    for (tool, settled) in [("settle_session", true), ("unsettle_session", false)] {
        assert_eq!(
            acted(
                &mut sidekick,
                tool,
                json!({ "session_id": asking, "origin": REMOTE })
            )
            .await,
            json!({ "session_id": asking, "origin": REMOTE, "settled": settled })
        );
        assert_eq!(
            self::settled(&remote, asking).await,
            settled,
            "{tool} sets it so on the Remote"
        );
    }

    // A Description of the Workspace it works in, named as the Remote lists
    // it.
    let workspaces = acted(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": REMOTE }),
    )
    .await;
    let path = workspaces["workspaces"][0]["path"].clone();
    assert_eq!(
        acted(
            &mut sidekick,
            "set_workspace_description",
            json!({ "workspace": path, "origin": REMOTE, "text": "Where the tests\nrun." }),
        )
        .await,
        json!({
            "workspace_id": workspaces["workspaces"][0]["workspace_id"],
            "origin": REMOTE,
            "path": path,
            "description": { "text": "Where the tests run.", "set": true },
        }),
        "it answers naming the Workspace as its row does, by its Remote too"
    );
    assert_eq!(
        acted(
            &mut sidekick,
            "list_workspaces",
            json!({ "origin": REMOTE })
        )
        .await["workspaces"][0]["description"],
        json!({ "text": "Where the tests run.", "set": true }),
        "the Remote holds it as a Description its user set"
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "set_workspace_description",
            json!({ "workspace": "/nowhere/at/all", "origin": REMOTE, "text": "Nothing." }),
        )
        .await,
        format!(
            "The Remote `{REMOTE}` knows no Workspace `/nowhere/at/all`, and resolving it there \
             found none: \"No directory there\". Name one by the workspace_id or the path \
             list_workspaces gives it with \"origin\": \"{REMOTE}\", or by the absolute path \
             of a directory in it, written as that server writes paths."
        ),
        "a directory the Remote finds nothing at is refused in the Remote's own words"
    );

    pair.shutdown().await;
}

/// A directory on a Remote that no Session works in, and that the Remote
/// holds nothing of, is no Workspace it lists; it is described all the same,
/// as a Client turned toward that Remote describes it — resolved by the
/// Remote itself, in its own path syntax — and is then a Workspace the Remote
/// knows. The Remote's own Sidekick Workspace, reached so, still refuses.
#[tokio::test]
async fn a_directory_a_remote_lists_no_workspace_at_is_described_as_its_client_describes_it() {
    let mut pair = paired("sidekick-remote-unlisted", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let unlisted = tempfile::tempdir().expect("create a directory on the Remote");
    let directory = suru::paths::canonical(unlisted.path())
        .expect("read the directory canonically")
        .to_string_lossy()
        .into_owned();
    let listed = |listing: &Value| {
        listing["workspaces"]
            .as_array()
            .expect("the Remote lists its Workspaces")
            .iter()
            .find(|row| row["path"] == json!(directory))
            .cloned()
    };
    assert_eq!(
        listed(
            &acted(
                &mut sidekick,
                "list_workspaces",
                json!({ "origin": REMOTE })
            )
            .await
        ),
        None,
        "the Remote lists no Workspace there"
    );

    let described = acted(
        &mut sidekick,
        "set_workspace_description",
        json!({ "workspace": directory, "origin": REMOTE, "text": "Scratch space." }),
    )
    .await;
    let row = listed(
        &acted(
            &mut sidekick,
            "list_workspaces",
            json!({ "origin": REMOTE }),
        )
        .await,
    )
    .expect("the Remote lists the Workspace once it is described");
    assert_eq!(
        described,
        json!({
            "workspace_id": row["workspace_id"],
            "origin": REMOTE,
            "path": directory,
            "description": { "text": "Scratch space.", "set": true },
        }),
        "it answers naming the Workspace as the Remote's row now does"
    );
    assert_eq!(
        row["description"],
        json!({ "text": "Scratch space.", "set": true }),
        "the Remote holds it as a Description its user set"
    );

    // Its own Sidekick Workspace, in which no Session works yet, is no
    // Workspace it lists either, and refuses.
    let their_directory = sidekick_directory(&remote).await;
    assert_eq!(
        refused(
            &mut sidekick,
            "set_workspace_description",
            json!({ "workspace": their_directory, "origin": REMOTE, "text": "Theirs." }),
        )
        .await,
        format!(
            "The Remote `{REMOTE}` refused it: The Workspace is the Sidekick Workspace, which a \
             Sidekick on another machine may not describe."
        )
    );

    pair.shutdown().await;
}

/// A Session's identity is unique only within its Origin, so a Remote's
/// Session and one of this Server's may share one. Each act reaches the one
/// its `origin` names, and answers naming that one as its row does — by its
/// Remote too, where it lives on one, and by its id alone here — so the
/// answer never leaves the Sidekick to guess which it acted on.
#[tokio::test]
async fn each_act_names_the_session_it_was_done_to_by_its_origin_where_two_share_an_id() {
    const SENT: &str = "Then cover the empty input.";
    let mut pair = paired("sidekick-remote-shared-id", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let here = tempfile::tempdir().expect("create a Workspace on the Sidekick's own Server");
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let shared = SessionId::new();
    for (descriptor, workspace) in [(&own, here.path()), (&remote, there.path())] {
        let mut request = session_request(
            workspace,
            default_selection(&claude_models()),
            "Write the parser",
        );
        request.session_id = Some(shared);
        assert_eq!(
            create_session(descriptor, &request).await.session.id,
            shared,
            "each Server holds a Session by the one id"
        );
    }

    let sent = acted(
        &mut sidekick,
        "send_prompt",
        json!({ "session_id": shared, "origin": REMOTE, "prompt": SENT }),
    )
    .await;
    assert_eq!(
        sent,
        json!({ "session_id": shared, "origin": REMOTE, "admitted": sent["admitted"] }),
        "the Remote's Session is named by its Remote"
    );
    assert!(prompt_saying(&read_session(&remote, shared).await, SENT).is_some());
    assert_eq!(
        prompt_saying(&read_session(&own, shared).await, SENT),
        None,
        "the Prompt went to the Remote's Session alone"
    );

    assert_eq!(
        acted(
            &mut sidekick,
            "settle_session",
            json!({ "session_id": shared, "origin": REMOTE }),
        )
        .await,
        json!({ "session_id": shared, "origin": REMOTE, "settled": true }),
        "the Remote's is named by its Remote"
    );
    assert_eq!(
        (settled(&remote, shared).await, settled(&own, shared).await),
        (true, false)
    );
    assert_eq!(
        acted(
            &mut sidekick,
            "settle_session",
            json!({ "session_id": shared }),
        )
        .await,
        json!({ "session_id": shared, "settled": true }),
        "and this Server's by its id alone, as its row is"
    );
    assert!(settled(&own, shared).await);
    assert_eq!(
        acted(
            &mut sidekick,
            "unsettle_session",
            json!({ "session_id": shared, "origin": REMOTE }),
        )
        .await,
        json!({ "session_id": shared, "origin": REMOTE, "settled": false })
    );
    assert_eq!(
        (settled(&remote, shared).await, settled(&own, shared).await),
        (false, true),
        "each act reached the Session its origin named, and no other"
    );

    pair.shutdown().await;
}

/// Whether the Server `descriptor` describes lists `session_id` as settled.
async fn settled(descriptor: &RuntimeDescriptor, session_id: SessionId) -> bool {
    list_sessions_at(descriptor)
        .await
        .into_iter()
        .find_map(|item| match item {
            SessionListItem::Readable(summary) if summary.session.id == session_id => {
                Some(summary.settled_at.is_some())
            }
            _ => None,
        })
        .expect("the Server lists the Session")
}

/// Every top-level Session the Server `descriptor` describes lists.
async fn list_sessions_at(descriptor: &RuntimeDescriptor) -> Vec<SessionListItem> {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list the Sessions")
        .json()
        .await
        .expect("decode the Sessions")
}

#[tokio::test]
async fn a_session_begun_on_a_remote_is_a_sidekicks_on_this_peer_heading_its_own_tree() {
    const ASKED: &str = "Fix the flaky login test in the auth suite.";
    let mut pair = paired("sidekick-remote-begin", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let directory =
        suru::paths::canonical(there.path()).expect("read the Remote's Workspace canonically");

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "origin": REMOTE, "directory": directory, "prompt": ASKED }),
    )
    .await;
    let selection = default_selection(&claude_models());
    let session_id: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    assert_eq!(
        begun,
        json!({
            "session_id": session_id,
            "origin": REMOTE,
            "directory": directory,
            "provider": selection.provider,
            "model": selection.model,
        }),
        "it begins where it was asked, with the Agent the Remote's own Landing would, and \
         is named by its Remote as every act on it names it"
    );
    let start = next_start(&mut pair.remote.provider).await;
    assert_eq!(
        start.execution_directory(),
        directory,
        "the Remote's Provider starts it"
    );

    let by_peer = by_the_peer(&remote).await;
    let snapshot = read_session(&remote, session_id).await;
    assert_eq!(
        of_the_peer(snapshot.session.begun_by.clone()),
        Some(by_peer.clone()),
        "the Remote remembers a Sidekick on this Peer began it"
    );
    assert_eq!(
        of_the_peer(snapshot.prompts[0].author.clone()),
        Some(by_peer.clone())
    );
    let summary = list_sessions_at(&remote)
        .await
        .into_iter()
        .find_map(|item| match item {
            SessionListItem::Readable(summary) if summary.session.id == session_id => Some(summary),
            _ => None,
        })
        .expect("the Remote lists it as a top-level Session");
    assert_eq!(
        of_the_peer(summary.session.begun_by),
        Some(by_peer),
        "and says so in the summary every Client of it receives"
    );

    let (tree, _) = crate::subagent_tree::open_tree(&remote, session_id).await;
    assert_eq!(
        (tree.top_level.session_id, tree.top_level.sidekick),
        (session_id, false),
        "it heads its own tree on the Remote, its Sidekick being elsewhere"
    );
    assert!(tree.sessions.is_empty());

    pair.shutdown().await;
}

/// The Subagent's Session the Session `session_id` on the Server
/// `descriptor` describes spawned first, once its row stands.
async fn first_subagent(descriptor: &RuntimeDescriptor, session_id: SessionId) -> SessionId {
    let snapshot = read_session_until(
        &reqwest::Client::new(),
        descriptor,
        session_id,
        "the Subagent's row stands",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the row leads into the Subagent's Session")
}

/// Every act a Sidekick on a Peer sends to a Session of the Remote's own
/// Sidekick Workspace — a Subagent's beneath one among them — or that would
/// begin a Session there, in a new Worktree or not, or describe that
/// Workspace, is refused by the Remote itself; reading there is not.
#[tokio::test]
async fn a_remotes_sidekick_workspace_refuses_a_peers_sidekick_though_it_reads_there() {
    let mut pair = paired("sidekick-remote-their-sidekick", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let their_directory = sidekick_directory(&remote).await;
    let (theirs, their_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        &their_directory,
        "Plan their week",
    )
    .await;
    let questionnaire = where_to_run();
    ask(&remote, theirs, &their_provider, &questionnaire).await;
    their_provider
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::OwningSession,
            ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new("their-explorer"),
                name: "Explore".to_owned(),
                description: "Survey their week".to_owned(),
                delegation: None,
            },
        )
        .await;
    let their_subagent = first_subagent(&remote, theirs).await;
    let refusal = format!("The Remote `{REMOTE}` refused it: {SIDEKICK_WORKSPACE_REFUSAL}");

    for (tool, arguments) in [
        (
            "send_prompt",
            json!({ "session_id": theirs, "origin": REMOTE, "prompt": "Change their Settings." }),
        ),
        (
            "interrupt_session",
            json!({ "session_id": theirs, "origin": REMOTE }),
        ),
        (
            "interrupt_session",
            json!({ "session_id": their_subagent, "origin": REMOTE }),
        ),
        (
            "settle_session",
            json!({ "session_id": theirs, "origin": REMOTE }),
        ),
        (
            "unsettle_session",
            json!({ "session_id": theirs, "origin": REMOTE }),
        ),
        (
            "answer_questionnaire",
            json!({
                "session_id": theirs,
                "origin": REMOTE,
                "questionnaire_id": questionnaire.id,
                "answers": [{ "choices": ["staging"] }, {}],
            }),
        ),
    ] {
        assert_eq!(
            refused(&mut sidekick, tool, arguments.clone()).await,
            refusal,
            "{tool} is refused on the Remote's Sidekick Workspace, by the Remote: {arguments}"
        );
    }
    let beginning_refusal = format!(
        "The Remote `{REMOTE}` refused it: The directory is the Sidekick Workspace's, and no \
         Sidekick begins a Session there, since its Agent would be a Sidekick too."
    );
    for new_worktree in [false, true] {
        assert_eq!(
            refused(
                &mut sidekick,
                "begin_session",
                json!({
                    "origin": REMOTE,
                    "directory": their_directory,
                    "prompt": "Be a Sidekick for me.",
                    "new_worktree": new_worktree,
                }),
            )
            .await,
            beginning_refusal,
            "and so is a beginning there, in a new Worktree or not ({new_worktree})"
        );
    }
    let workspaces = acted(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": REMOTE }),
    )
    .await;
    let their_workspace = workspaces["workspaces"]
        .as_array()
        .expect("the Remote lists its Workspaces")
        .iter()
        .find(|row| {
            row["path"]
                .as_str()
                .is_some_and(|path| std::path::Path::new(path) == their_directory)
        })
        .unwrap_or_else(|| panic!("the Remote lists its Sidekick Workspace: {workspaces}"))
        .clone();
    assert_eq!(
        refused(
            &mut sidekick,
            "set_workspace_description",
            json!({
                "workspace": their_workspace["workspace_id"],
                "origin": REMOTE,
                "text": "Wherever you like.",
            }),
        )
        .await,
        format!(
            "The Remote `{REMOTE}` refused it: The Workspace is the Sidekick Workspace, which a \
             Sidekick on another machine may not describe."
        ),
        "nor may it describe the Remote's Sidekick Workspace"
    );

    let snapshot = read_session(&remote, theirs).await;
    assert_eq!(snapshot.prompts.len(), 1, "nothing was admitted there");
    assert_eq!(
        stood(&snapshot, questionnaire.id).map(|(outcome, ..)| outcome),
        Some(QuestionnaireOutcome::Pending),
        "nor answered"
    );
    assert!(!settled(&remote, theirs).await, "nor set aside");
    assert!(
        acted(
            &mut sidekick,
            "list_workspaces",
            json!({ "origin": REMOTE })
        )
        .await["workspaces"]
            .as_array()
            .expect("the Remote lists its Workspaces")
            .iter()
            .all(|row| row["workspace_id"] != their_workspace["workspace_id"]
                || row["description"].is_null()),
        "nor described"
    );

    for read in [theirs, their_subagent] {
        let reading = acted(
            &mut sidekick,
            "read_session",
            json!({ "session_id": read, "origin": REMOTE }),
        )
        .await;
        assert_eq!(
            reading["session_id"],
            json!(read),
            "a Session there may still be read"
        );
    }

    pair.shutdown().await;
}

#[tokio::test]
async fn an_act_on_a_remote_that_does_not_answer_is_refused_and_nothing_is_kept_for_later() {
    const UNSENT: &str = "Pick the parser back up.";
    let mut pair = paired(
        "sidekick-remote-unanswered-acts",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_millis(300)),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let target = working_session(&remote, there.path(), "Write the parser").await;
    let send = json!({ "session_id": target, "origin": REMOTE, "prompt": UNSENT });

    assert_eq!(
        refused(
            &mut sidekick,
            "send_prompt",
            json!({ "session_id": target, "origin": "elsewhere", "prompt": UNSENT }),
        )
        .await,
        "Suru is paired with no Remote named `elsewhere`; list_remotes names the Remotes it is \
         paired with, and leaving `origin` out reaches this server.",
        "an Origin naming no Remote is refused"
    );

    pair.remote.route.set_online(false).await;
    assert_eq!(
        refused(&mut sidekick, "send_prompt", send.clone()).await,
        nothing_done(&unreachable()),
        "an Unreachable Remote is refused, saying so"
    );
    for tool in ["interrupt_session", "settle_session", "unsettle_session"] {
        assert_eq!(
            refused(
                &mut sidekick,
                tool,
                json!({ "session_id": target, "origin": REMOTE })
            )
            .await,
            nothing_done(&unreachable()),
            "{tool}"
        );
    }
    pair.remote.route.set_online(true).await;
    // Answering again, the Remote is asked nothing it was not asked before.
    let snapshot = read_session(&remote, target).await;
    assert_eq!(
        prompt_saying(&snapshot, UNSENT),
        None,
        "nothing refused was kept to send once the Remote answers"
    );
    assert!(!settled(&remote, target).await);

    pair.remote.route.swallow_connections().await;
    assert_eq!(
        refused(&mut sidekick, "send_prompt", send.clone()).await,
        format!(
            "The Remote `{REMOTE}` is not answering: it said nothing within 300 milliseconds. \
             It may have been done there all the same: read the Session with read_session, \
             \"session_id\": \"{target}\" and \"origin\": \"{REMOTE}\", to find out before \
             asking again. It stands among the Sessions you have a hand in, not yet confirmed, \
             until a read of that Remote finds it; nothing is kept to do later."
        ),
        "a Remote that takes the act and says nothing may have done it"
    );

    pair.remote.route.set_online(true).await;
    pair.remote = pair.remote.restart_speaking(PROTOCOL_VERSION + 1).await;
    assert_eq!(
        refused(&mut sidekick, "send_prompt", send).await,
        nothing_done(&format!(
            "The Remote `{REMOTE}` runs a version of Suru that speaks another protocol than this \
             server's, so nothing can be asked of it until one of them is updated."
        )),
        "a Remote speaking another protocol is answered as not answering"
    );

    pair.shutdown().await;
}

/// An act a Remote took whose answer was lost on its way back may have been
/// done: the Sidekick is told so, never that nothing was done, and which
/// Session to read; it is asked of no other address the Remote was paired
/// at, so it is not done twice; and it is kept, not yet confirmed, until a
/// read of the Remote finds the Session.
#[tokio::test]
async fn an_act_whose_answer_is_lost_once_the_remote_took_it_is_neither_denied_nor_asked_again() {
    let mut remote = Serving::start("sidekick-remote-lost-answer").await;
    let second_route = ObservedTcpProxy::start(
        remote
            .server
            .serving_address()
            .expect("the Remote is Serving"),
    )
    .await;
    let (own, mut claude, _directories) = own_server(
        "sidekick-remote-lost-answer",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
        None,
    )
    .await;
    // Paired at two addresses, the first in the Invite asked first.
    let invite: IssuedInvite = posted(
        &remote.descriptor(),
        "/v1/pairing/invites",
        &IssueInviteRequest {
            addresses: vec![remote.route.address, second_route.address],
        },
    )
    .await;
    reqwest::Client::new()
        .post(format!("{}/v1/pairing/remotes", own.descriptor().base_url))
        .bearer_auth(&own.descriptor().token)
        .json(&RedeemInviteRequest {
            invite: invite.invite,
            name: Some(REMOTE.to_owned()),
            addresses: Vec::new(),
        })
        .send()
        .await
        .expect("redeem the Invite")
        .error_for_status()
        .expect("the Pairing forms");
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (target, mut target_provider) = started_session(
        &remote.descriptor(),
        &mut remote.provider,
        there.path(),
        "Write the parser",
    )
    .await;
    let (sidekick_id, sidekick, _provider) = start_sidekick(own.descriptor(), &mut claude).await;

    let interrupting = tokio::spawn(async move {
        let mut sidekick = sidekick;
        let refusal = refused(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": target, "origin": REMOTE }),
        )
        .await;
        (sidekick, refusal)
    });
    let interrupt = timeout(PROGRESS_DEADLINE, target_provider.next_interrupt())
        .await
        .expect("the interrupt reaches the Remote's Agent");
    // Its answer is lost on the way back.
    remote.route.set_online(false).await;
    interrupt.succeed();
    let (_sidekick, refusal) = interrupting.await.expect("the Tool answers");
    assert_eq!(
        refusal,
        format!(
            "The Remote `{REMOTE}` stopped answering once it had been asked. It may have been \
             done there all the same: read the Session with read_session, \"session_id\": \
             \"{target}\" and \"origin\": \"{REMOTE}\", to find out before asking again. It \
             stands among the Sessions you have a hand in, not yet confirmed, until a read of \
             that Remote finds it; nothing is kept to do later."
        ),
        "an act the Remote took is never denied, and the Session it names is the one to read"
    );
    assert_eq!(
        second_route.opened_connections(),
        0,
        "and is asked of no other address it was paired at"
    );
    assert!(
        timeout(Duration::from_millis(200), target_provider.next_interrupt())
            .await
            .is_err(),
        "so it was done once"
    );
    // Kept, not yet confirmed, the act is confirmed by the first read of the
    // Remote that finds the Session — here through the other address, the
    // first being offline still.
    let (tree, mut updates) = crate::subagent_tree::open_tree(own.descriptor(), sidekick_id).await;
    let mut revision = tree.revision;
    let confirmed = |entry: &suru::protocol::SubagentTreeSession| {
        entry.session_id == target && entry.origin.as_deref() == Some(REMOTE) && !entry.unconfirmed
    };
    if !tree.sessions.iter().any(confirmed) {
        loop {
            if let suru::protocol::SubagentTreeChange::SessionChanged { entry } =
                crate::subagent_tree::next_change(&mut updates, &mut revision).await
                && confirmed(&entry)
            {
                break;
            }
        }
    }

    own.shutdown().await.expect("shut down the own Server");
    remote.shutdown().await;
}

/// A Remote hosting other Providers and Models than this Server begins a
/// Session with what it offers: its own Landing's Agent where none is named,
/// and an Agent named from its own catalog. One only this Server hosts is
/// refused, naming what the Remote hosts — never this Server's Providers, nor
/// list_providers, which says only what this Server offers.
#[tokio::test]
async fn a_remote_hosting_other_agents_begins_with_what_it_offers_and_says_what_that_is() {
    let mut remote = Serving::start_hosting(
        "sidekick-remote-catalog",
        ServerTimings::default().sse_keepalive_interval,
        (ProviderId::new("codex"), codex_models()),
    )
    .await;
    let (own, mut claude, _directories) =
        own_server("sidekick-remote-catalog", ServerTimings::default(), None).await;
    pair(own.descriptor(), &remote, REMOTE).await;
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let directory =
        suru::paths::canonical(there.path()).expect("read the Remote's Workspace canonically");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(own.descriptor(), &mut claude).await;
    let codex = default_selection(&codex_models());

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "origin": REMOTE, "directory": directory, "prompt": "Tidy the docs." }),
    )
    .await;
    assert_eq!(
        (&begun["provider"], &begun["model"]),
        (&json!(codex.provider), &json!(codex.model)),
        "it begins with the Agent the Remote's own Landing would: {begun}"
    );
    next_start(&mut remote.provider).await;
    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({
            "origin": REMOTE,
            "directory": directory,
            "prompt": "Tidy the changelog.",
            "agent_selection": {
                "provider": "codex",
                "model": codex.model,
                "options": { "reasoning_effort": "low" },
            },
        }),
    )
    .await;
    assert_eq!(begun["model"], json!(codex.model));
    next_start(&mut remote.provider).await;
    let session_id: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let selection = read_session(&remote.descriptor(), session_id)
        .await
        .session
        .agent_selection
        .expect("it runs the Agent named");
    assert_eq!(
        serde_json::to_value(&selection.options).expect("encode the Model Options"),
        json!([{ "id": "reasoning_effort", "value": { "type": "select", "choice": "low" } }]),
        "with the Agent named from the Remote's catalog"
    );

    let claude_default = default_selection(&claude_models());
    assert_eq!(
        refused(
            &mut sidekick,
            "begin_session",
            json!({
                "origin": REMOTE,
                "directory": directory,
                "prompt": "Tidy the docs.",
                "agent_selection": {
                    "provider": claude_default.provider,
                    "model": claude_default.model,
                },
            }),
        )
        .await,
        format!(
            "The Remote `{REMOTE}` hosts no Provider `claude`; the Providers it hosts are \
             `codex`."
        ),
        "an Agent only this Server hosts is refused by what the Remote hosts"
    );

    own.shutdown().await.expect("shut down the own Server");
    remote.shutdown().await;
}

/// A Questionnaire on a Remote is answered by the identity the Remote's own
/// reading gave it, and the Answer stands there as a Sidekick's on this
/// Peer: so every Client watching the Session is told as it lands, and so it
/// is kept across a stop of the Remote.
#[tokio::test]
async fn an_answer_named_as_a_remotes_reading_names_it_stands_there_as_the_peers_sidekicks() {
    let mut pair = paired("sidekick-remote-answer", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (asking, mut asking_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Run the tests.",
    )
    .await;
    let by_peer = by_the_peer(&remote).await;
    let questionnaire = where_to_run();
    ask(&remote, asking, &asking_provider, &questionnaire).await;

    let reading = acted(
        &mut sidekick,
        "read_session",
        json!({ "session_id": asking, "origin": REMOTE }),
    )
    .await;
    let named = reading["questionnaires"][0]["id"].clone();
    assert_eq!(
        named,
        json!(questionnaire.id),
        "the Remote's reading names the Questionnaire waiting there: {reading}"
    );
    let (_, mut watched) = watch_session(&remote, asking).await;
    let (answered, _) = tokio::join!(
        acted(
            &mut sidekick,
            "answer_questionnaire",
            json!({
                "session_id": asking,
                "origin": REMOTE,
                "questionnaire_id": named,
                "answers": [{ "choices": ["local"] }, { "text": "Quickly." }],
            }),
        ),
        async {
            timeout(
                PROGRESS_DEADLINE,
                asking_provider.next_questionnaire_submission(),
            )
            .await
            .expect("the Answer reaches the Remote's Agent")
        },
    );
    assert_eq!(answered["answered"], json!(true));
    let settled = next_change(&mut watched, "the Questionnaire settling", |change| {
        matches!(
            change,
            SessionChange::QuestionnaireSettled {
                outcome: QuestionnaireOutcome::Answered,
                ..
            }
        )
    })
    .await;
    let SessionChange::QuestionnaireSettled { author, .. } = settled else {
        unreachable!("the change was found as a settling");
    };
    assert_eq!(
        of_the_peer(author),
        Some(by_peer.clone()),
        "every Client of the Remote is told the Answer is a Sidekick's on this Peer"
    );

    drop(watched);
    drop(asking_provider);
    pair.remote = pair.remote.restart_speaking(PROTOCOL_VERSION).await;
    let snapshot = read_session(&pair.remote.descriptor(), asking).await;
    assert_eq!(
        stood(&snapshot, questionnaire.id)
            .map(|(outcome, _, author)| (outcome, of_the_peer(author))),
        Some((QuestionnaireOutcome::Answered, Some(by_peer))),
        "and the Remote keeps it so across a stop"
    );

    pair.shutdown().await;
}

/// A beginning on a Remote in a new Worktree has the Remote make one of its
/// own Repository, as its own Landing would, and begins the Session there.
#[tokio::test]
async fn a_session_begun_on_a_remote_in_a_new_worktree_works_in_one_made_there() {
    let mut pair = paired("sidekick-remote-worktree", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let temporary = tempfile::tempdir().expect("create a home for the Remote's Repository");
    let main = suru::paths::canonical(temporary.path())
        .expect("read the Remote's home canonically")
        .join("auth");
    crate::broker::subsessions::committed(&main);
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({
            "origin": REMOTE,
            "directory": main,
            "prompt": "Fix the flaky login test.",
            "new_worktree": true,
        }),
    )
    .await;
    let directory = std::path::PathBuf::from(
        begun["directory"]
            .as_str()
            .expect("the answer says where the Session works"),
    );
    assert_eq!(
        (directory.parent(), &begun["origin"]),
        (Some(main.join(".suru-worktrees").as_path()), &json!(REMOTE)),
        "a new Worktree is made in the Remote's Repository: {begun}"
    );
    let start = next_start(&mut pair.remote.provider).await;
    assert_eq!(start.execution_directory(), directory, "and it works there");
    let session_id: SessionId =
        serde_json::from_value(begun["session_id"].clone()).expect("the Session begun is named");
    let snapshot = read_session(&remote, session_id).await;
    assert_eq!(
        snapshot
            .session
            .checkout
            .as_ref()
            .map(|checkout| checkout.kind),
        Some(suru::protocol::CheckoutKind::Linked)
    );
    assert_eq!(
        of_the_peer(snapshot.session.begun_by.clone()),
        Some(by_the_peer(&remote).await)
    );

    pair.shutdown().await;
}

/// An act the Remote took stays one whose outcome is unknown however the
/// Pairing changes while its answer is awaited: the Remote removed here
/// before the answer comes back is no reason to say nothing was done.
#[tokio::test]
async fn an_act_the_remote_took_is_not_denied_for_a_pairing_removed_before_its_answer() {
    let mut pair = paired(
        "sidekick-remote-unpaired-midway",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_secs(2)),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (target, mut target_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Write the parser",
    )
    .await;
    let (_sidekick_id, sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    let interrupting = tokio::spawn(async move {
        let mut sidekick = sidekick;
        let refusal = refused(
            &mut sidekick,
            "interrupt_session",
            json!({ "session_id": target, "origin": REMOTE }),
        )
        .await;
        (sidekick, refusal)
    });
    let interrupt = timeout(PROGRESS_DEADLINE, target_provider.next_interrupt())
        .await
        .expect("the interrupt reaches the Remote's Agent");
    // The Remote is removed here while its answer is awaited, and the
    // answer is lost on the way back.
    reqwest::Client::new()
        .delete(format!("{}/v1/pairing/remotes/{REMOTE}", own.base_url))
        .bearer_auth(&own.token)
        .send()
        .await
        .expect("remove the Remote")
        .error_for_status()
        .expect("the Remote is removed");
    pair.remote.route.set_online(false).await;
    interrupt.succeed();
    let (_sidekick, refusal) = interrupting.await.expect("the Tool answers");
    assert!(
        refusal.contains("It may have been done there all the same"),
        "an act the Remote took is never denied for the Pairing ending meanwhile: {refusal}"
    );

    pair.shutdown().await;
}
