//! A Sidekick's Clone on a Remote: `clone_workspace` with an `origin` naming
//! a Remote has the Sidekick's own Server carry the Clone there over the
//! Pairing, as it carries a Workspace's Description (ADR 0044), and the
//! Remote Clones as its own Sidekick's Clone is done — directly within one of
//! the Remote's own Workspace Roots and by every rule of its own — answering
//! the Workspace made as that Remote lists it, or the refusal in its own
//! words, each path spelled as that machine spells paths. What the Remote
//! Clones it Clones for a Sidekick on the Peer it came from, and its own
//! Sidekick Workspace takes no Clone from one. A Remote that does not
//! answer, or does not understand a Clone, refuses it, and nothing is kept to
//! Clone there later.
//!
//! Each test pairs two Servers as the rest of the Remotes suite does and
//! Clones a bare Repository made beside them, named by its path, which the
//! Remote's Git reads as it reads any Repository URL.

use std::pin::pin;

use suru::protocol::{AgentSelection, CloneWorkspaceRequest, WorkspaceId};

use super::acts::the_peer;
use super::*;
use crate::broker::sidekick::cloning::{
    Captured, bare_repository, contents, directory, git_says, readme, repository_id, spelled,
};
use crate::broker::sidekick::workspaces::next_workspace_errand;
use crate::broker::sidekick_acts::{acted, refused, refused_over_http};
use crate::workspace_registry::entries_on_disk;

/// How the Registry spells a Workspace a Sidekick Cloned.
const BY_CLONE: &str = "\"cloned\"";

/// Names `roots` the Workspace Roots of the Server `descriptor` describes, as
/// its user names them.
async fn name_roots(descriptor: &RuntimeDescriptor, roots: &[&Path]) {
    mutate_setting(
        descriptor,
        SettingMutation::WorkspaceRoots {
            value: Some(roots.iter().map(|root| spelled(root).to_owned()).collect()),
        },
    )
    .await;
}

/// The row list_workspaces gives the Repository Cloned to `clone` on the
/// Remote while it carries `description` and the Icon named `icon`.
fn row_there(clone: &Path, description: Value, icon: Value) -> Value {
    json!({
        "workspace_id": repository_id(clone),
        "origin": REMOTE,
        "name": clone
            .file_name()
            .expect("a clone's directory has a name")
            .to_string_lossy(),
        "path": clone,
        "description": description,
        "icon": icon,
    })
}

/// Stops both Servers, and answers how the Remote's Workspace Registry left
/// the entry `workspace_id` on disk, where it left one.
async fn entry_left_there(
    pair: Paired,
    workspace_id: &WorkspaceId,
) -> Option<(Option<PathBuf>, String)> {
    let Paired { own, remote, .. } = pair;
    own.shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
    let Serving {
        server,
        config,
        _directories,
        ..
    } = remote;
    server.shutdown().await.expect("shut down the Remote");
    entries_on_disk(&config).remove(workspace_id)
}

/// The rows list_workspaces gives of the Workspaces the Remote knows.
async fn listed_there(sidekick: &mut McpClient) -> Vec<Value> {
    acted(sidekick, "list_workspaces", json!({ "origin": REMOTE })).await["workspaces"]
        .as_array()
        .expect("the Remote lists its Workspaces")
        .clone()
}

/// Waits until list_workspaces gives `row` among the Workspaces the Remote
/// knows, asking again while the Remote does not answer.
async fn listed_until(sidekick: &mut McpClient, row: &Value) {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let listing = sidekick
                .call_tool("list_workspaces", json!({ "origin": REMOTE }))
                .await;
            if listing["isError"] != json!(true)
                && listing["structuredContent"]["workspaces"]
                    .as_array()
                    .is_some_and(|rows| rows.contains(row))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Remote lists {row}"));
}

/// With an `origin`, a Clone goes to the Remote and lands directly within
/// the Remote's own Workspace Root — not the Sidekick's own Server's — under
/// the name its URL ends in, an ordinary clone of the Repository's default
/// branch. It is answered as the Remote lists the new Workspace, carrying the
/// Remote's name, and is entered there as Cloned, by a Sidekick on the Peer
/// it came from, as the Remote's Log says.
#[tokio::test]
async fn a_clone_on_a_remote_lands_within_the_remotes_root_and_is_listed_from_the_remote() {
    const {
        assert!(
            PROTOCOL_VERSION >= 103,
            "a Clone is carried to a Remote over the Pairing"
        );
    }
    let log = Captured::default();
    let _log = log.install(tracing::Level::INFO);
    let mut pair = paired("sidekick-remote-clone", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_here, here) = directory();
    let (_there, there) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&own, &[&here]).await;
    name_roots(&remote, &[&there]).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    let atlas = there.join("atlas");
    let answered = acted(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin, "origin": REMOTE }),
    )
    .await;
    assert_eq!(
        answered,
        row_there(&atlas, Value::Null, Value::Null),
        "the Clone goes directly within the Remote's only Root, named as the URL ends"
    );
    assert!(
        listed_there(&mut sidekick).await.contains(&answered),
        "the answer is the row the Remote's listing gives the new Workspace"
    );
    assert_eq!(
        std::fs::read_to_string(atlas.join("README.md")).expect("the clone holds the README"),
        readme("atlas")
    );
    assert_eq!(
        git_says(&atlas, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "the Repository's default branch is checked out"
    );
    assert_eq!(contents(&there), ["atlas"], "nothing else is made there");
    assert!(
        contents(&here).is_empty(),
        "nothing is cloned within the Sidekick's own Server's Root"
    );

    let peer = the_peer(&remote).await;
    let logged = log.logged();
    assert!(
        logged.lines().any(|line| line.contains("Cloned")
            && line.contains(spelled(&atlas))
            && line.contains(&peer.name)),
        "the Remote's Log records the Clone as a Sidekick's on the Peer it came from: {logged}"
    );

    assert_eq!(
        entry_left_there(pair, &repository_id(&atlas)).await,
        Some((Some(atlas.clone()), BY_CLONE.to_owned())),
        "the Remote's Registry holds the new Workspace as Cloned"
    );
}

/// A Clone a Remote takes from a Peer's Sidekick has its Icon and
/// Description derived there at once, as one of its own Sidekick's has: the
/// Errand runs on the Remote, in the clone, at the Agent the Remote's Landing
/// begins a Session with where none is named — as a Session the Peer's
/// Sidekick began there would — and what it answers lands on the row the
/// Remote lists.
#[tokio::test]
async fn a_clone_on_a_remote_has_its_icon_and_description_derived_there() {
    let mut pair = paired("sidekick-remote-clone-derives", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_there, there) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&remote, &[&there]).await;
    let landing: AgentSelection = default_selection(&claude_models());
    reqwest::Client::new()
        .put(format!("{}/v1/landing-agent-selection", remote.base_url))
        .bearer_auth(&remote.token)
        .json(&landing)
        .send()
        .await
        .expect("choose the Remote Landing's Agent")
        .error_for_status()
        .expect("the Remote's Landing takes the Agent");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    let atlas = there.join("atlas");
    acted(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin, "origin": REMOTE }),
    )
    .await;
    let errand = next_workspace_errand(&mut pair.remote.provider).await;
    assert_eq!(
        errand.execution_directory(),
        atlas,
        "the Errand runs on the Remote, in the clone"
    );
    assert!(
        errand.prompt().contains("Where the atlas is charted."),
        "the Errand carries the clone's README: {}",
        errand.prompt()
    );
    errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the atlas is charted.",
    }));
    let derived = row_there(
        &atlas,
        json!({ "text": "Where the atlas is charted.", "set": false }),
        json!("dev-rust"),
    );
    listed_until(&mut sidekick, &derived).await;

    pair.shutdown().await;
}

/// A Remote judges a Clone by its own Workspace Roots and its own rules, and
/// refuses one in its own words, its paths spelled as it spells them: where
/// it has no Root, though the Sidekick's own Server has; where `root` names
/// none of its own; where something stands at the destination already,
/// naming the Workspace there; and where its Root lies within its own
/// Sidekick Workspace, which a Peer's Sidekick may not Clone into as it may
/// not act there otherwise. Nor does it Clone for a Client, since a Clone is
/// a Sidekick's act. Nothing a refusal answers is cloned anywhere.
#[tokio::test]
async fn a_remote_refuses_a_clone_by_its_own_roots_and_rules_in_its_own_words() {
    let mut pair = paired("sidekick-remote-clone-refused", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_here, here) = directory();
    let (_there, there) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&own, &[&here]).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let clone = json!({ "repository_url": origin, "origin": REMOTE });

    assert_eq!(
        refused(&mut sidekick, "clone_workspace", clone.clone()).await,
        format!(
            "The Remote `{REMOTE}` refused it: No Workspace Root is configured on this server, so \
             there is nowhere to Clone into; ask the user to name the directories their work \
             lives in as Workspace Roots, in the workspace.roots Setting."
        ),
        "a Remote with no Root refuses, whatever Roots the Sidekick's own Server has"
    );

    name_roots(&remote, &[&there]).await;
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "origin": REMOTE, "root": here }),
        )
        .await,
        format!(
            "The Remote `{REMOTE}` refused it: `{}` is none of this server's Workspace Roots, so \
             nothing was cloned; name one of those configured as `root`: `{}`.",
            spelled(&here),
            spelled(&there)
        ),
        "a Root of the Sidekick's own Server is none of the Remote's"
    );
    assert!(
        contents(&here).is_empty() && contents(&there).is_empty(),
        "nothing refused is cloned anywhere"
    );

    let atlas = there.join("atlas");
    acted(&mut sidekick, "clone_workspace", clone.clone()).await;
    assert_eq!(
        refused(&mut sidekick, "clone_workspace", clone.clone()).await,
        format!(
            "The Remote `{REMOTE}` refused it: `{}` already exists, so nothing was cloned there: \
             it is the Workspace `{}`; give another `name` to Clone beside it.",
            spelled(&atlas),
            repository_id(&atlas).0
        ),
        "an existing destination is refused naming the Workspace there, as the Remote spells it"
    );

    let theirs = sidekick_directory(&remote).await;
    name_roots(&remote, &[&theirs]).await;
    assert_eq!(
        refused(&mut sidekick, "clone_workspace", clone.clone()).await,
        format!(
            "The Remote `{REMOTE}` refused it: `{}` lies within the Sidekick Workspace, which a \
             Sidekick on another machine may not Clone into, so nothing was cloned.",
            spelled(&theirs.join("atlas"))
        ),
        "the Remote's own Sidekick Workspace takes no Clone from a Peer's Sidekick"
    );
    assert!(
        !theirs.join("atlas").exists(),
        "nothing is cloned into the Sidekick Workspace"
    );

    name_roots(&remote, &[&there]).await;
    let by_a_client = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/clone", remote.base_url))
        .bearer_auth(&remote.token)
        .json(&CloneWorkspaceRequest {
            repository_url: spelled(&origin).to_owned(),
            root: None,
            name: Some("charts".to_owned()),
            branch: None,
        })
        .send()
        .await
        .expect("ask the Remote's Session API for a Clone");
    assert_eq!(
        refused_over_http(by_a_client).await,
        "A Clone is a Sidekick's act, so a Client's request for one is refused; a Sidekick on \
         this machine or on a Peer Clones through clone_workspace."
    );
    assert_eq!(
        contents(&there),
        ["atlas"],
        "nothing a Client asked is cloned"
    );

    pair.shutdown().await;
}

/// `name` read in any case, as a filesystem that does not tell case apart
/// reads it.
fn in_any_case(name: &str) -> String {
    name.to_uppercase().to_lowercase()
}

/// A Remote whose data directory is named a Workspace Root before it has
/// made its Sidekick Workspace takes no Clone where that Workspace would be
/// made, in whatever case its name is spelled, nor spelled with the Kelvin
/// sign a filesystem that composes characters reads as `K`: a filesystem
/// that does not tell case apart takes any of them for that very directory,
/// so the Sidekick Workspace would be made of what a Peer's Sidekick filled
/// it with. Nothing is cloned there.
#[tokio::test]
async fn a_remote_takes_no_clone_where_its_sidekick_workspace_would_be_made_however_spelled() {
    let mut pair = paired(
        "sidekick-remote-clone-sidekick-place",
        ServerTimings::default(),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let data = suru::paths::canonical(pair.remote.config.data_dir())
        .expect("read the Remote's data directory canonically");
    assert!(
        contents(&data)
            .iter()
            .all(|entry| in_any_case(entry) != "sidekick"),
        "the Remote has made no Sidekick Workspace yet"
    );
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&remote, &[&data]).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    for name in ["SIDEKICK", "Sidekick", "sidekic\u{212A}", "sidekick"] {
        assert_eq!(
            refused(
                &mut sidekick,
                "clone_workspace",
                json!({ "repository_url": origin, "origin": REMOTE, "name": name }),
            )
            .await,
            format!(
                "The Remote `{REMOTE}` refused it: `{}` is where the Sidekick Workspace is kept, \
                 in whatever case its name is spelled, and no Clone may take its place, so \
                 nothing was cloned; give another `name`.",
                spelled(&data.join(name))
            ),
            "{name:?} names the Sidekick Workspace's place"
        );
    }
    let left = contents(&data);
    assert!(
        left.iter().all(|entry| in_any_case(entry) != "sidekick"),
        "nothing is cloned where the Sidekick Workspace would be made: {left:?}"
    );

    pair.shutdown().await;
}

/// A Clone on a Remote that cannot be reached is refused saying so; on one
/// speaking a version of the protocol without the Clone, saying it does not
/// understand one. Either way nothing was cloned there, and nothing is kept
/// to Clone once the Remote answers.
#[tokio::test]
async fn a_clone_on_a_remote_that_does_not_answer_or_understand_it_is_refused_and_not_kept() {
    let mut pair = paired(
        "sidekick-remote-clone-unanswered",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_millis(300)),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_there, there) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&remote, &[&there]).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let clone = json!({ "repository_url": origin, "origin": REMOTE });

    pair.remote.route.set_online(false).await;
    assert_eq!(
        refused(&mut sidekick, "clone_workspace", clone.clone()).await,
        format!(
            "{} Nothing was done there, and nothing is kept to do once it answers.",
            unreachable()
        ),
        "an Unreachable Remote is refused, saying so"
    );
    pair.remote.route.set_online(true).await;
    assert!(
        contents(&there).is_empty(),
        "nothing refused was kept to Clone once the Remote answers"
    );

    pair.remote = pair.remote.restart_speaking(PROTOCOL_VERSION - 1).await;
    assert_eq!(
        refused(&mut sidekick, "clone_workspace", clone).await,
        format!(
            "The Remote `{REMOTE}` does not understand a Clone: it runs a version of Suru that \
             speaks another protocol than this server's, so nothing can be asked of it until one \
             of them is updated. Nothing was cloned there, and nothing is kept to do once it \
             answers."
        ),
        "a Remote on a version of the protocol without the Clone says it does not understand one"
    );
    assert!(contents(&there).is_empty(), "nothing was cloned there");

    pair.shutdown().await;
}

/// A Clone takes as long as Git does, so a Remote Cloning is waited on past
/// the time any other act is given to be answered, for as long as a Clone
/// may take. One whose answer is lost once the Remote took it may have
/// landed all the same: the Sidekick is told so, and to list the Remote's
/// Workspaces rather than ask again; and the Remote sees it through, entering
/// it as Cloned.
#[tokio::test]
async fn a_clone_on_a_remote_is_waited_on_as_a_clone_and_one_whose_answer_is_lost_may_have_landed()
{
    const REACH: Duration = Duration::from_millis(500);
    let mut pair = paired(
        "sidekick-remote-clone-waited",
        ServerTimings::default().with_remote_reach_timeout(REACH),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_there, there) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(&remote, &[&there]).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    // The Remote has cloned, and is held reading what it cloned.
    let atlas = there.join("atlas");
    let (reached, release) = pair.remote.git.hold_discovery_of(&atlas);
    {
        let mut cloning = pin!(sidekick.call_tool(
            "clone_workspace",
            json!({ "repository_url": origin, "origin": REMOTE }),
        ));
        tokio::select! {
            reached = reached => reached.expect("the Remote reads its clone"),
            answered = &mut cloning => panic!("the Clone was answered before it landed: {answered}"),
        }
        assert!(
            timeout(REACH * 2, &mut cloning).await.is_err(),
            "a Clone is waited on past the time any act is given"
        );
        release.send(()).expect("let the Remote read its clone");
        let answered = cloning.await;
        assert_ne!(answered["isError"], json!(true), "{answered}");
        assert_eq!(
            answered["structuredContent"],
            row_there(&atlas, Value::Null, Value::Null)
        );
    }

    // Its answer is lost on the way back, once the Remote has cloned.
    let charts = there.join("charts");
    let (reached, release) = pair.remote.git.hold_discovery_of(&charts);
    let cloning = tokio::spawn(async move {
        let refusal = refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "origin": REMOTE, "name": "charts" }),
        )
        .await;
        (sidekick, refusal)
    });
    timeout(PROGRESS_DEADLINE, reached)
        .await
        .expect("the Remote clones")
        .expect("the Remote reads its clone");
    pair.remote.route.set_online(false).await;
    release.send(()).expect("let the Remote read its clone");
    let (mut sidekick, refusal) = cloning.await.expect("the Tool answers");
    assert_eq!(
        refusal,
        format!(
            "The Remote `{REMOTE}` stopped answering once it had been asked. The Clone may have \
             been done there all the same: list that Remote's Workspaces with list_workspaces \
             and \"origin\": \"{REMOTE}\" to find out before asking again; nothing is kept to do \
             later."
        ),
        "a Clone the Remote took is never denied"
    );
    pair.remote.route.set_online(true).await;
    listed_until(&mut sidekick, &row_there(&charts, Value::Null, Value::Null)).await;

    assert_eq!(
        entry_left_there(pair, &repository_id(&charts)).await,
        Some((Some(charts.clone()), BY_CLONE.to_owned())),
        "the Remote saw the Clone through though no one heard its answer"
    );
}

/// A Remote that begins to stop while a Clone a Peer's Sidekick asked of it
/// is still running Git stops that Clone, though it runs apart from the
/// Peer's request: Git ends, and what it had begun is removed before the
/// Remote lets go. The Sidekick is told the Remote stopped it — or, where
/// the Remote's stop cut the answer off on its way back, that it may have
/// been done — and is never answered for from what the Remote last said.
/// Git is held here by a Repository URL naming a server that takes the
/// connection and never answers.
#[tokio::test]
async fn a_remote_stopping_mid_clone_stops_git_and_removes_what_it_began_before_it_lets_go() {
    use tokio::io::AsyncReadExt as _;

    let mut pair = paired("sidekick-remote-clone-stopping", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_there, there) = directory();
    name_roots(&remote, &[&there]).await;
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen for Git");
    let url = format!(
        "git://127.0.0.1:{}/atlas.git",
        silent.local_addr().expect("a listening address").port()
    );
    let (connected, git_connected) = oneshot::channel();
    let (let_go, git_let_go) = oneshot::channel();
    tokio::spawn(async move {
        let (mut connection, _) = silent.accept().await.expect("Git connects");
        let _ = connected.send(());
        let mut said = [0; 1024];
        while connection.read(&mut said).await.is_ok_and(|read| read > 0) {}
        let _ = let_go.send(());
    });
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;

    let cloning = {
        let url = url.clone();
        tokio::spawn(async move {
            let refusal = refused(
                &mut sidekick,
                "clone_workspace",
                json!({ "repository_url": url, "origin": REMOTE }),
            )
            .await;
            (sidekick, refusal)
        })
    };
    timeout(PROGRESS_DEADLINE, git_connected)
        .await
        .expect("the Remote's Git asks for the Repository")
        .expect("Git connects");
    assert!(
        contents(&there).iter().all(|entry| entry.starts_with('.')) && !contents(&there).is_empty(),
        "Git is making the clone out of sight: {:?}",
        contents(&there)
    );

    let Serving {
        server,
        _directories,
        ..
    } = pair.remote;
    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("the Remote stops")
        .expect("shut down the Remote");
    assert_eq!(
        contents(&there),
        Vec::<String>::new(),
        "what the Clone began is removed before the Remote lets go"
    );
    timeout(PROGRESS_DEADLINE, git_let_go)
        .await
        .expect("the Remote's Git is stopped")
        .expect("Git lets go of its connection");

    let (_sidekick, refusal) = cloning.await.expect("the Tool answers");
    let stopped = format!(
        "The Remote `{REMOTE}` refused it: This server began to stop before the Clone of `{url}` \
         was done, so it was stopped and what it had begun is removed; a clone already whole may \
         land all the same, and is then listed among this server's Workspaces once it is back."
    );
    let cut_off = format!(
        "The Remote `{REMOTE}` stopped answering once it had been asked. The Clone may have been \
         done there all the same: list that Remote's Workspaces with list_workspaces and \
         \"origin\": \"{REMOTE}\" to find out before asking again; nothing is kept to do later."
    );
    assert!(
        refusal == stopped || refusal == cut_off,
        "the Sidekick is told the Remote stopped the Clone, or that its answer was cut off: \
         {refusal}"
    );

    pair.own
        .shutdown()
        .await
        .expect("shut down the Sidekick's own Server");
}
