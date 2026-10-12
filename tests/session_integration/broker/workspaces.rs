//! `list_workspaces` for every Agent: an Agent working in any Session is
//! offered a listing of its own Server's Workspaces beside the Subagent Tools,
//! so that it knows where the user's other work lives (ADR 0051). It reads the
//! very rows a Sidekick's listing gives — each Workspace's identity, name,
//! presented root's path, Description and Icon, the Sidekick Workspace among
//! them — and is told of no Remote: an `origin`, `everywhere` among them, is
//! refused saying the Tool answers this server's Workspaces alone. No other
//! Workspace Tool is offered it. A Subagent of any Agent, brokered or native,
//! is offered the same, a Sidekick's included.
//!
//! Each test acts as the MCP client a Provider harness is, as the rest of the
//! Broker suite does, and asserts on what the Broker answers it.

use suru::protocol::RepositoryId;

use super::*;
use crate::broker::{
    sidekick::{
        ORDINARY_TOOLS, SIDEKICK_TOOLS, listed_tools, sidekick_directory, start_sidekick,
        unoffered,
        workspaces::{connected_client, description, quiet_session, row},
    },
    sidekick_acts::{acted, refused},
};
use crate::{repositories::git, workspace_discovery::AS_LONG_AS_IT_TAKES};

/// What an Agent other than a Sidekick is told when it asks for any Server's
/// Workspaces but its own.
const THIS_SERVER_ALONE: &str = "list_workspaces answers you with this Suru server's Workspaces \
                                 alone, so it takes no `origin` from you; call it again without \
                                 `origin`.";

/// `list_workspaces`' rows as `client` is answered them.
async fn listed_workspaces(client: &mut McpClient) -> Vec<Value> {
    acted(client, "list_workspaces", json!({})).await["workspaces"]
        .as_array()
        .expect("a listing lists Workspaces")
        .clone()
}

/// Checks `client`, an Agent's MCP client, is answered its own Server's
/// Workspaces and refused any other Server's.
async fn assert_lists_this_servers_workspaces_alone(client: &mut McpClient, agent: &str) {
    let rows = listed_workspaces(client).await;
    assert!(
        !rows.is_empty() && rows.iter().all(|row| row.get("origin").is_none()),
        "{agent} is answered this server's Workspaces: {rows:?}"
    );
    for origin in ["everywhere", "workstation"] {
        assert_eq!(
            refused(client, "list_workspaces", json!({ "origin": origin })).await,
            THIS_SERVER_ALONE,
            "{agent} is told of no Remote, so an `origin` of {origin:?} is refused"
        );
    }
}

/// An Agent working elsewhere than the Sidekick Workspace is offered
/// `list_workspaces`, described to it as this server's listing, and reads the
/// listing a Sidekick reads: the Workspace a Description and Icon were set
/// for, its own, and the Sidekick Workspace, each in the same row. It is
/// refused an `origin` and any argument besides, and is still offered none of
/// a Sidekick's own Tools.
#[tokio::test]
async fn an_agent_reads_its_own_servers_workspaces_and_is_refused_any_other_servers() {
    const CHANNEL: &str = "agent-workspace-listing";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let sidekick_workspace = read_session(&descriptor, sidekick_id)
        .await
        .session
        .workspace;
    let workspace = hosted.workspace.path().to_owned();
    let (agent_id, handoff, _agent_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let agent_workspace = read_session(&descriptor, agent_id).await.session.workspace;
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;

    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let notes_workspace = quiet_session(&descriptor, notes.path(), "Keep the notes")
        .await
        .session
        .workspace;
    let user = connected_client(state_dir.path(), CHANNEL).await;
    user.set_workspace_icon(&notes_workspace.id, "dev-rust")
        .await
        .expect("the user chooses the notes Workspace's Icon");
    user.set_workspace_description(&notes_workspace.id, None, "Where the user keeps notes.")
        .await
        .expect("the user describes the notes Workspace");

    let listed = agent.request("tools/list", json!({})).await;
    let listing = listed["tools"]
        .as_array()
        .expect("tools/list lists Tools")
        .iter()
        .find(|tool| tool["name"] == json!("list_workspaces"))
        .unwrap_or_else(|| panic!("an Agent is offered list_workspaces: {listed}"))
        .clone();
    assert_eq!(listing["annotations"]["readOnlyHint"], json!(true));
    assert_eq!(
        listing["inputSchema"],
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        "an Agent's list_workspaces takes nothing: {listing}"
    );
    let described = listing["description"]
        .as_str()
        .expect("list_workspaces is described");
    assert!(
        !described.contains("Remote") && !described.contains("origin"),
        "an Agent is told of no Remote: {described}"
    );

    let rows = listed_workspaces(&mut agent).await;
    for (expected, which) in [
        (
            row(
                &notes_workspace,
                description("Where the user keeps notes.", true),
                json!("dev-rust"),
            ),
            "the Workspace the user described, with its Description and Icon",
        ),
        (
            row(&sidekick_workspace, Value::Null, json!("md-robot")),
            "the Sidekick Workspace, as any other row",
        ),
    ] {
        assert!(rows.contains(&expected), "{which} is listed: {rows:?}");
    }
    assert!(
        rows.iter()
            .any(|row| row["workspace_id"] == json!(agent_workspace.id)
                && row["path"] == json!(agent_workspace.path)),
        "the Agent's own Workspace is listed: {rows:?}"
    );
    assert_eq!(
        rows,
        listed_workspaces(&mut sidekick).await,
        "an Agent reads the very rows a Sidekick's listing of this server gives"
    );

    assert_lists_this_servers_workspaces_alone(&mut agent, "an Agent").await;
    assert_eq!(
        refused(&mut agent, "list_workspaces", json!({ "limit": 5 })).await,
        "list_workspaces takes no arguments; call it again without limit."
    );

    for tool in SIDEKICK_TOOLS
        .into_iter()
        .filter(|tool| !ORDINARY_TOOLS.contains(tool))
    {
        let refused = unoffered(&mut agent, tool, json!({})).await;
        assert_eq!(
            refused["message"],
            json!(format!("The Broker offers no Tool named `{tool}`")),
            "an Agent is still offered none of a Sidekick's own Tools: {refused}"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Subagent is offered the ordinary Tools whatever its parent is: a brokered
/// Subagent of an Agent working elsewhere lists `list_workspaces` with its own
/// token and reads this server's Workspaces alone, and a call a Sidekick's
/// native Subagent makes in its own name, under the Sidekick's token, is
/// answered as any other Agent's is — refused an `origin` the Sidekick itself
/// may give.
///
/// Codex names the calling thread on each call but not on `tools/list`, so a
/// native Subagent sharing a Sidekick's token is listed the Sidekick's Tools
/// and schemas, `list_workspaces`' `origin` among them
/// (`docs/validation/0408-subagent-mcp-attribution.md`). One that gives an
/// `origin` as that schema allows is told to call again without it, and that
/// next call is answered.
#[tokio::test]
async fn a_subagent_of_any_agent_brokered_or_native_reads_this_servers_workspaces_alone() {
    const ROOT_THREAD: &str = "019a0e25-60f7-sidekick-thread";
    const NATIVE_THREAD: &str = "019a0e25-8213-native-thread";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "subagent-workspace-listing", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();

    let (_agent, handoff, _agent_provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &workspace,
        default_selection(&claude_models()),
    )
    .await;
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    agent
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look next door",
            "prompt": "Find where the API this Repository calls lives.",
        }))
        .await;
    let brokered_start = next_start(&mut hosted.codex).await;
    let mut brokered = McpClient::handed(
        brokered_start
            .broker()
            .expect("a brokered Subagent is handed the Broker too"),
    );
    brokered.initialize().await;
    assert_eq!(listed_tools(&mut brokered).await, ORDINARY_TOOLS);
    assert_lists_this_servers_workspaces_alone(&mut brokered, "a brokered Subagent").await;

    let directory = sidekick_directory(&descriptor).await;
    let (sidekick_id, handoff, provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &directory,
        default_selection(&codex_models()),
    )
    .await;
    let mut sidekick = McpClient::handed(&handoff).with_call_meta(json!({
        "threadId": ROOT_THREAD,
        "sessionId": ROOT_THREAD,
    }));
    sidekick.initialize().await;
    super::attribution::spawn_native(&descriptor, sidekick_id, &provider, NATIVE_THREAD).await;
    let mut native = McpClient::handed(&handoff).with_call_meta(json!({
        "threadId": NATIVE_THREAD,
        "sessionId": ROOT_THREAD,
    }));
    native.initialize().await;
    let listed = native.request("tools/list", json!({})).await;
    let listing = listed["tools"]
        .as_array()
        .expect("tools/list lists Tools")
        .iter()
        .find(|tool| tool["name"] == json!("list_workspaces"))
        .unwrap_or_else(|| panic!("a native Subagent is offered list_workspaces: {listed}"));
    assert!(
        listing["inputSchema"]["properties"]["origin"].is_object(),
        "a list names no thread, so it is the token's Session's — the Sidekick's: {listing}"
    );
    assert_eq!(
        refused(
            &mut native,
            "list_workspaces",
            json!({ "origin": "everywhere" })
        )
        .await,
        THIS_SERVER_ALONE,
        "a native Subagent following that schema is told to call again without `origin`"
    );
    let rows = listed_workspaces(&mut native).await;
    assert!(
        !rows.is_empty() && rows.iter().all(|row| row.get("origin").is_none()),
        "and the call it makes next is answered this server's Workspaces: {rows:?}"
    );
    assert_lists_this_servers_workspaces_alone(&mut native, "a Sidekick's native Subagent").await;
    acted(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": "everywhere" }),
    )
    .await;

    drop(brokered_start);
    hosted.server.shutdown().await.expect("shut down server");
}

/// An Agent's `list_workspaces` lists each Repository discovered directly
/// within a Workspace Root as any other row, before any Session has begun
/// there — and one cloned by hand into a Root after the Server started at
/// the very next call, without a restart.
#[tokio::test]
async fn an_agent_lists_the_repositories_discovered_within_the_workspace_roots() {
    const CHANNEL: &str = "agent-discovered-workspace-listing";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let projects = tempfile::tempdir().expect("create the Workspace Root");
    let projects = suru::paths::canonical(projects.path()).expect("read the Root canonically");
    let atlas = projects.join("atlas");
    std::fs::create_dir(&atlas).expect("create the atlas Repository");
    git(&atlas, &["init", "-b", "main"]);
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        json!({ "workspace": { "roots": [projects] } }).to_string(),
    )
    .expect("name the Workspace Root");
    let mut hosted = host_providers_with_timings(
        state_dir.path(),
        CHANNEL,
        Some(config_dir.path()),
        ServerTimings::default()
            .with_root_discovery_memo(Duration::ZERO)
            .with_root_discovery_wait(AS_LONG_AS_IT_TAKES),
    )
    .await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_agent, handoff, _agent_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let discovered = |repository: &Path| {
        json!({
            "workspace_id": RepositoryId::from_metadata("git", &repository.join(".git"))
                .workspace_id(),
            "name": repository.file_name().expect("a Repository's directory has a name")
                .to_string_lossy(),
            "path": repository,
            "description": null,
            "icon": null,
        })
    };

    let rows = listed_workspaces(&mut agent).await;
    assert!(
        rows.contains(&discovered(&atlas)),
        "the Repository within the Root is listed though no Session began there: {rows:?}"
    );

    let copy = projects.join("atlas-copy");
    git(
        &projects,
        &[
            "clone",
            "--quiet",
            atlas.to_str().expect("the fixture's path is UTF-8"),
            copy.to_str().expect("the fixture's path is UTF-8"),
        ],
    );
    let rows = listed_workspaces(&mut agent).await;
    assert!(
        rows.contains(&discovered(&atlas)) && rows.contains(&discovered(&copy)),
        "the clone made by hand since is listed at the next call: {rows:?}"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A row says why its Workspace cannot be opened, as `"unavailable"`, where
/// its directory cannot be read now — here gone, though a Session belongs to
/// it — and leaves it out for every other, in the very rows an Agent and a
/// Sidekick read alike; each is told so in the Tool's description.
#[tokio::test]
async fn a_row_says_why_its_workspace_cannot_be_opened_for_an_agent_as_for_a_sidekick() {
    const CHANNEL: &str = "agent-unavailable-workspace-listing";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let workspace = hosted.workspace.path().to_owned();
    let (agent_id, handoff, _agent_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let agent_workspace = read_session(&descriptor, agent_id).await.session.workspace;
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let notes_workspace = quiet_session(&descriptor, notes.path(), "Keep the notes")
        .await
        .session
        .workspace;

    notes
        .close()
        .expect("delete the notes Workspace's directory");

    let rows = listed_workspaces(&mut agent).await;
    let mut unavailable = row(&notes_workspace, Value::Null, Value::Null);
    unavailable["unavailable"] = json!("its directory is gone");
    assert!(
        rows.contains(&unavailable),
        "the row of a Workspace whose directory is gone says why it cannot be opened: {rows:?}"
    );
    let own = rows
        .iter()
        .find(|row| row["workspace_id"] == json!(agent_workspace.id))
        .unwrap_or_else(|| panic!("the Agent's own Workspace is listed: {rows:?}"));
    assert!(
        own.get("unavailable").is_none(),
        "a Workspace whose directory can be read says nothing of it: {own}"
    );
    assert_eq!(
        rows,
        listed_workspaces(&mut sidekick).await,
        "a Sidekick reads the very rows an Agent does"
    );
    for (client, caller) in [(&mut agent, "an Agent"), (&mut sidekick, "a Sidekick")] {
        let listed = client.request("tools/list", json!({})).await;
        let described = listed["tools"]
            .as_array()
            .expect("tools/list lists Tools")
            .iter()
            .find(|tool| tool["name"] == json!("list_workspaces"))
            .and_then(|tool| tool["description"].as_str())
            .unwrap_or_else(|| panic!("{caller} is offered list_workspaces: {listed}"))
            .to_owned();
        assert!(
            described.contains("\"unavailable\""),
            "{caller} is told a row may say it cannot be opened: {described}"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}
