//! The Sidekick: the Agent of a top-level Session in the Sidekick Workspace,
//! which the Broker offers more Tools than any other Agent (ADR 0042).
//!
//! The Sidekick Workspace is a `sidekick` directory beside the Server's own
//! data, made the first time a Client asks for it. Being a Session of it is the
//! whole of what makes a Session's Agent a Sidekick, however the Session was
//! begun: the Server reads it off the Session's stored Workspace when it mints
//! the Session's Broker token, and nothing about the Session says so. A
//! Sidekick lists, and may call, `list_sessions`; any other caller — a
//! Sidekick's own brokered Subagent included — neither lists it nor may call
//! it.
//!
//! Each test acts as the MCP client a Provider harness is, as the rest of the
//! Broker suite does, and asserts on what that client and the Session API
//! observe.

use std::path::PathBuf;

use suru::{protocol::ResolvedWorkspace, server::ServerClock};

use super::*;

/// The Tools every Agent is offered, in the Broker's order.
const ORDINARY_TOOLS: [&str; 6] = [
    "list_providers",
    "spawn_subagent",
    "read_subagent",
    "send_to_subagent",
    "wait_subagents",
    "stop_subagent",
];

/// The Tools a Sidekick is offered: the ordinary ones, then its own.
const SIDEKICK_TOOLS: [&str; 7] = [
    "list_providers",
    "spawn_subagent",
    "read_subagent",
    "send_to_subagent",
    "wait_subagents",
    "stop_subagent",
    "list_sessions",
];

/// Asks the Server for its Sidekick Workspace, as `/sidekick` does.
async fn sidekick_workspace(descriptor: &RuntimeDescriptor) -> ResolvedWorkspace {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/sidekick", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("ask for the Sidekick Workspace")
        .error_for_status()
        .expect("the Sidekick Workspace is resolved")
        .json::<ResolvedWorkspace>()
        .await
        .expect("decode the Sidekick Workspace")
}

/// The directory a Session in the Sidekick Workspace works in.
async fn sidekick_directory(descriptor: &RuntimeDescriptor) -> PathBuf {
    sidekick_workspace(descriptor)
        .await
        .execution_directory
        .expect("the Sidekick Workspace is somewhere a Session can work")
        .path
}

/// The names `tools/list` answers `client` with, in the order listed.
async fn listed_tools(client: &mut McpClient) -> Vec<String> {
    client.request("tools/list", json!({})).await["tools"]
        .as_array()
        .expect("tools/list lists Tools")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("every Tool is named")
                .to_owned()
        })
        .collect()
}

/// The JSON-RPC error a call of `tool` is answered with, for a call the
/// Broker refuses before any Tool runs.
async fn unoffered(client: &mut McpClient, tool: &str, arguments: Value) -> Value {
    client.call_tool_error(tool, arguments).await
}

/// `list_sessions`' answer to `arguments`, read from the structured content
/// the call carries.
async fn list_sessions(client: &mut McpClient, arguments: Value) -> Value {
    let result = client.call_tool("list_sessions", arguments).await;
    assert_ne!(
        result["isError"],
        json!(true),
        "list_sessions answers: {result}"
    );
    result["structuredContent"].clone()
}

/// The Titles of the rows a listing answered with, in its order.
fn titles(listing: &Value) -> Vec<&str> {
    listing["sessions"]
        .as_array()
        .expect("a listing lists Sessions")
        .iter()
        .map(|row| row["title"].as_str().expect("every row has its Title"))
        .collect()
}

/// A Session in `workspace` whose Provider start is never answered: it stands
/// Working, waiting on its Provider, for as long as the test runs.
async fn working_session(
    descriptor: &RuntimeDescriptor,
    workspace: &Path,
    title: &str,
) -> SessionId {
    create_session(
        descriptor,
        &session_request(workspace, default_selection(&claude_models()), title),
    )
    .await
    .session
    .id
}

/// A Sidekick's Session on Claude, begun in the Sidekick Workspace by the
/// ordinary create-Session request, its first Turn running, and the MCP
/// client its Agent is.
async fn start_sidekick(
    descriptor: &RuntimeDescriptor,
    claude: &mut ControlledProvider,
) -> (SessionId, McpClient, ControlledProviderSession) {
    let directory = sidekick_directory(descriptor).await;
    let (session_id, handoff, provider) = start_session(
        descriptor,
        claude,
        &directory,
        default_selection(&claude_models()),
    )
    .await;
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;
    (session_id, client, provider)
}

#[tokio::test]
async fn the_sidekick_workspace_is_made_on_first_use_beside_each_channels_own_data() {
    assert_eq!(
        suru::protocol::PROTOCOL_VERSION,
        71,
        "the Sidekick Workspace route changes the wire, a Remote's included"
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let (release_runtime, _release) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let (development_runtime, _development) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let release = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "release")
            .expect("configure the release server")
            .with_data_dir(data_dir.path()),
        release_runtime,
    )
    .await
    .expect("spawn the release server");
    let development = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "sidekick-development")
            .expect("configure the development server")
            .with_data_dir(data_dir.path()),
        development_runtime,
    )
    .await
    .expect("spawn the development server");
    let release_data = suru::paths::canonical(data_dir.path()).expect("read the data root");
    let development_data = release_data.join("sidekick-development");
    assert!(
        !release_data.join("sidekick").exists() && !development_data.join("sidekick").exists(),
        "nothing is made until a Sidekick is asked for"
    );

    let resolved = sidekick_workspace(release.descriptor()).await;
    let expected = release_data.join("sidekick");
    assert!(expected.is_dir(), "asking makes the Sidekick Workspace");
    assert_eq!(
        resolved.execution_directory.map(|directory| directory.path),
        Some(expected.clone()),
        "a Session there works in the directory itself"
    );
    assert_eq!(resolved.workspace.path, expected);
    assert_eq!(
        resolved.execution_status,
        suru::protocol::ExecutionDirectoryStatus::Available
    );
    assert!(
        resolved.workspace.repository.is_none(),
        "it is a directory outside source control"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&expected)
            .expect("read the Sidekick Workspace's permissions")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "it is made with the data root's own permissions"
        );
    }
    std::fs::write(expected.join("AGENTS.md"), "Be brief.").expect("keep a file of the user's");
    assert_eq!(
        sidekick_workspace(release.descriptor())
            .await
            .execution_directory
            .map(|directory| directory.path),
        Some(expected.clone()),
        "asking again answers with the same directory"
    );
    assert!(
        expected.join("AGENTS.md").is_file(),
        "and leaves what the user keeps there alone"
    );

    assert_eq!(
        sidekick_directory(development.descriptor()).await,
        development_data.join("sidekick"),
        "each Channel keeps a Sidekick Workspace of its own, beside its own data"
    );

    release
        .shutdown()
        .await
        .expect("shut down the release server");
    development
        .shutdown()
        .await
        .expect("shut down the development server");
}

#[tokio::test]
async fn a_sidekick_lists_and_may_call_list_sessions_while_any_other_session_neither_lists_nor_calls_it()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-tools-by-caller", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();

    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_ordinary, ordinary_handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&ordinary_handoff);
    ordinary.initialize().await;

    assert_eq!(
        listed_tools(&mut sidekick).await,
        SIDEKICK_TOOLS,
        "a Sidekick is offered every ordinary Tool and then its own"
    );
    assert_eq!(
        listed_tools(&mut ordinary).await,
        ORDINARY_TOOLS,
        "a Session elsewhere is offered the ordinary Tools alone"
    );

    let listing = list_sessions(&mut sidekick, json!({})).await;
    assert!(
        listing["sessions"].is_array() && listing["omitted"].is_number(),
        "a Sidekick's call is answered: {listing}"
    );
    let refused = unoffered(&mut ordinary, "list_sessions", json!({})).await;
    assert_eq!(
        refused["message"],
        json!("The Broker offers no Tool named `list_sessions`"),
        "any other caller is answered as though the Tool did not exist: {refused}"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sidekicks_brokered_subagent_is_offered_the_ordinary_tools_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-subagent-tools", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    sidekick
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around the Sidekick Workspace.",
        }))
        .await;
    let start = next_start(&mut hosted.codex).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut subagent = McpClient::handed(&handoff);
    subagent.initialize().await;

    assert_eq!(
        listed_tools(&mut subagent).await,
        ORDINARY_TOOLS,
        "work a Sidekick delegates is offered nothing that reaches across Suru"
    );
    let refused = unoffered(&mut subagent, "list_sessions", json!({})).await;
    assert_eq!(
        refused["message"],
        json!("The Broker offers no Tool named `list_sessions`"),
        "{refused}"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A native Subagent rides its parent's Provider connection, and so its token.
/// Where its calls name it — as each Codex thread names itself — a call is its
/// own Session's, and that Session is no top-level Session of the Sidekick
/// Workspace, so it is refused the Sidekick's Tools as any other Agent is.
#[tokio::test]
async fn a_call_a_sidekicks_native_subagent_makes_in_its_own_name_is_refused_the_sidekicks_tools() {
    const ROOT_THREAD: &str = "019a0e25-60f7-sidekick-thread";
    const NATIVE_THREAD: &str = "019a0e25-8213-native-thread";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-native-subagent", None).await;
    let descriptor = hosted.server.descriptor().clone();
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
    list_sessions(&mut sidekick, json!({})).await;

    super::attribution::spawn_native(&descriptor, sidekick_id, &provider, NATIVE_THREAD).await;
    let mut native = McpClient::handed(&handoff).with_call_meta(json!({
        "threadId": NATIVE_THREAD,
        "sessionId": ROOT_THREAD,
    }));
    native.initialize().await;
    let refused = unoffered(&mut native, "list_sessions", json!({})).await;
    assert_eq!(
        refused["message"],
        json!("The Broker offers no Tool named `list_sessions`"),
        "{refused}"
    );
    native.list_providers().await;
    list_sessions(&mut sidekick, json!({})).await;

    hosted.server.shutdown().await.expect("shut down server");
}

/// The Server derives a Sidekick from the Session's stored Workspace each time
/// it mints a token, so a Session restored after a stop is a Sidekick's still,
/// though nothing about it was written down to say so.
#[tokio::test]
async fn a_restored_session_of_the_sidekick_workspace_is_a_sidekicks_still() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-restored").await;
    let descriptor = server.descriptor().clone();
    let (session_id, _sidekick, provider) = start_sidekick(&descriptor, &mut claude).await;
    provider.emit(ProviderEvent::TurnCompleted);
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        session_id,
        "the Sidekick's first Turn completes",
        |snapshot| {
            snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
        },
    )
    .await;
    drop(provider);
    server.shutdown().await.expect("stop the server");

    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-restored").await;
    let descriptor = server.descriptor().clone();
    admit_prompt(&descriptor, session_id, "What is going on?").await;
    let relaunch = next_start(&mut claude).await;
    let handoff = relaunch
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker");
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;
    assert_eq!(listed_tools(&mut sidekick).await, SIDEKICK_TOOLS);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn with_the_broker_off_a_sidekick_is_offered_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        "{ \"broker\": { \"enabled\": false } }\n",
    )
    .expect("write Config Document");
    let (server, mut claude) =
        host_claude(state_dir.path(), config_dir.path(), "sidekick-broker-off").await;
    let descriptor = server.descriptor().clone();

    let directory = sidekick_directory(&descriptor).await;
    create_session(
        &descriptor,
        &session_request(
            &directory,
            default_selection(&claude_models()),
            "What is going on?",
        ),
    )
    .await;
    let start = next_start(&mut claude).await;
    assert_eq!(
        start.broker(),
        None,
        "the Broker's `enabled` Setting turns the Sidekick's Tools off with the rest"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn list_sessions_lists_active_top_level_sessions_most_recent_first_and_says_how_many_it_left_out()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-listing-defaults", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let subagent = sidekick
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-5.5",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around.",
        }))
        .await;
    let mut created = Vec::new();
    for index in 1..=21 {
        let title = format!("Work {index:02}");
        working_session(&descriptor, &workspace, &title).await;
        created.push(title);
    }

    let listing = list_sessions(&mut sidekick, json!({})).await;
    let expected = created
        .iter()
        .rev()
        .take(20)
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        titles(&listing),
        expected,
        "the most recently active Sessions come first, twenty of them"
    );
    assert_eq!(
        listing["omitted"],
        json!(2),
        "the oldest Session and the Sidekick's own are left out, and the listing says how many"
    );
    let rows = listing["sessions"].as_array().expect("rows");
    assert!(
        rows.iter().all(|row| row["session_id"] != json!(subagent)),
        "a Subagent's Session is never listed: {listing}"
    );

    let row = &rows[0];
    let mut fields = row
        .as_object()
        .expect("a row is an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "last_active",
            "session_id",
            "settled",
            "standing",
            "title",
            "workspace"
        ],
        "a row is compact: {row}"
    );
    let canonical_workspace = suru::paths::canonical(&workspace).expect("read the Workspace");
    assert_eq!(row["workspace"], json!(canonical_workspace));
    assert_eq!(row["standing"], json!("working"));
    assert_eq!(row["settled"], json!(false));
    assert!(
        row["last_active"]
            .as_str()
            .is_some_and(|moment| moment.ends_with('Z') && moment.contains('T')),
        "last activity is an RFC 3339 moment in UTC: {row}"
    );

    let everything = list_sessions(&mut sidekick, json!({ "limit": 50 })).await;
    assert_eq!(everything["omitted"], json!(0));
    assert_eq!(everything["sessions"].as_array().map(Vec::len), Some(22));
    assert_eq!(
        everything["sessions"][21]["session_id"],
        json!(sidekick_id),
        "the Sidekick's own Session is a top-level Session like any other"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn list_sessions_narrows_by_workspace_title_liveness_standing_and_last_activity() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-listing-filters",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let ledger = tempfile::tempdir().expect("create the ledger Workspace");
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;

    // A Session whose Turn completed and no Client has Viewed reads Done.
    let (finished, _handoff, finished_provider) = start_session(
        &descriptor,
        &mut claude,
        atlas.path(),
        default_selection(&claude_models()),
    )
    .await;
    finished_provider.emit(ProviderEvent::TurnCompleted);
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        finished,
        "the atlas Turn completes",
        |snapshot| {
            snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
        },
    )
    .await;
    let shelved = working_session(&descriptor, ledger.path(), "Shelved ledger work").await;
    working_session(&descriptor, ledger.path(), "Reconcile the LEDGER").await;
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{shelved}/settlement",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&json!({ "settled": true }))
        .send()
        .await
        .expect("settle a Session")
        .error_for_status()
        .expect("the Session is settled");

    let atlas_path = suru::paths::canonical(atlas.path()).expect("read the atlas Workspace");
    let in_atlas = list_sessions(&mut sidekick, json!({ "workspace": atlas_path })).await;
    assert_eq!(titles(&in_atlas), ["Plan the work"], "{in_atlas}");
    assert_eq!(in_atlas["sessions"][0]["standing"], json!("done"));
    assert_eq!(in_atlas["omitted"], json!(0));

    let ledger_path = suru::paths::canonical(ledger.path()).expect("read the ledger Workspace");
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "workspace": ledger_path })).await),
        ["Reconcile the LEDGER"],
        "a Session set aside is left out of the default, active listing"
    );
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "title": "ledger" })).await),
        ["Reconcile the LEDGER"],
        "a Title matches by its words, whatever their case"
    );
    let settled = list_sessions(&mut sidekick, json!({ "liveness": "settled" })).await;
    assert_eq!(titles(&settled), ["Shelved ledger work"]);
    assert_eq!(settled["sessions"][0]["settled"], json!(true));
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "liveness": "all", "workspace": ledger_path })
            )
            .await
        ),
        ["Reconcile the LEDGER", "Shelved ledger work"]
    );
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "standing": "done" })).await),
        ["Plan the work"]
    );

    let all = list_sessions(&mut sidekick, json!({ "liveness": "all" })).await;
    let rows = all["sessions"].as_array().expect("rows");
    assert_eq!(rows.len(), 4, "{all}");
    let boundary = rows[1]["last_active"].clone();
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "liveness": "all", "active_after": boundary })
            )
            .await
        ),
        titles(&all)[..2],
        "active_after keeps what was last active at or after the moment it names"
    );
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "liveness": "all", "active_before": boundary })
            )
            .await
        ),
        titles(&all)[2..],
        "and active_before what was last active before it"
    );
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "liveness": "all", "active_after": "2000-01-01", "active_before": "2000-01-02" })
            )
            .await
        ),
        Vec::<&str>::new(),
        "a day may stand for the moment it begins"
    );

    let limited = list_sessions(&mut sidekick, json!({ "liveness": "all", "limit": 1 })).await;
    assert_eq!(titles(&limited), titles(&all)[..1]);
    assert_eq!(limited["omitted"], json!(3));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn list_sessions_settles_a_session_left_alone_as_the_sidebar_does() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (clock, hand) = ServerClock::manual();
    let (runtime, mut claude) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let server = server::spawn_with_providers_and_timings(
        ServerConfig::new(state_dir.path(), "sidekick-listing-auto-settle")
            .expect("configure server"),
        vec![runtime],
        ServerTimings::default().with_clock(clock),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (idle, _handoff, idle_provider) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;
    idle_provider.emit(ProviderEvent::TurnCompleted);
    read_session_until(
        &reqwest::Client::new(),
        &descriptor,
        idle,
        "the idle Session's Turn completes",
        |snapshot| {
            snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
        },
    )
    .await;
    working_session(&descriptor, workspace.path(), "Still working").await;
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({ "liveness": "settled" })).await),
        Vec::<&str>::new()
    );

    // Past `sidebar.autoSettle`'s three days, the Session nothing has moved
    // settles on its own; the Sessions still Working do not, however long.
    hand.advance(Duration::from_secs(4 * 24 * 60 * 60));
    let settled = list_sessions(&mut sidekick, json!({ "liveness": "settled" })).await;
    assert_eq!(titles(&settled), ["Plan the work"], "{settled}");
    assert_eq!(settled["sessions"][0]["settled"], json!(true));
    assert_eq!(
        titles(&list_sessions(&mut sidekick, json!({})).await),
        ["Still working", "Plan the work"],
        "the active listing holds what is still Working, the Sidekick's own Session among it"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn list_sessions_refuses_what_it_does_not_take_in_words_the_sidekick_can_act_on() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-listing-refusals",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    for (arguments, says) in [
        (json!({ "origin": "studio" }), "takes no argument `origin`"),
        (
            json!({ "liveness": "everything" }),
            "`active`, `settled` or `all`",
        ),
        (json!({ "standing": "busy" }), "`needs_intervention`"),
        (json!({ "active_after": "yesterday" }), "RFC 3339"),
        (json!({ "limit": "many" }), "`limit` must be a whole number"),
        (json!({ "title": 7 }), "`title` must be a string"),
    ] {
        let refusal = sidekick.refusal("list_sessions", arguments.clone()).await;
        assert!(
            refusal.contains(says),
            "{arguments} is refused saying {says:?}: {refusal}"
        );
    }

    server.shutdown().await.expect("shut down server");
}
