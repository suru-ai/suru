//! Remotes: a Sidekick reaching the Sessions and Workspaces of the user's
//! other machines through its own Server, never speaking to a Remote itself
//! (ADR 0044). `list_remotes` names each Remote and whether it answers now;
//! `list_sessions`, `read_session` and `list_workspaces` take an `origin` — a
//! Remote's name, or for a listing `everywhere` — and every row from a Remote
//! carries that name, while one from the Sidekick's own Server carries none.
//! A Remote that does not answer is named as not answering, and nothing it
//! said before is answered in its place. What a Sidekick does on a Remote is
//! covered in [`acts`], how the Sessions it has a hand in there stand in its
//! tree in [`sessions_section`], and the Reports it is owed of them in
//! [`reports`].
//!
//! Each test pairs two Servers in-process, as the Pairing suite does: the
//! Sidekick's own, and the Remote it redeemed an Invite from as
//! `workstation`, whose Serving listener it dials by way of a route the test
//! can close, or have take what it is sent and say nothing. Each test acts as
//! the MCP client a Sidekick's harness is and asserts on what the Tools answer
//! it.

use std::net::{IpAddr, Ipv4Addr};

use diesel::{Connection, QueryableByName, RunQueryDsl, SqliteConnection, sql_types::Text};

use reqwest::header::{ACCEPT, AUTHORIZATION, HOST};
use suru::{
    protocol::{
        IssueInviteRequest, IssuedInvite, PROTOCOL_VERSION, RedeemInviteRequest, Remote, Way,
    },
    provider::{ProviderActivityId, ProviderCommandStatus},
};

use super::*;
use crate::server_support::observed_tcp_proxy::ObservedTcpProxy;

mod acts;
mod kept_apart;
mod pairings;
mod readings;
mod references;
mod reports;
mod sessions_section;
mod unconfirmed;

/// The name the Sidekick's own Server knows its Remote by.
const REMOTE: &str = "workstation";

/// A Server Serving on this machine's loopback and hosting the Claude double:
/// a Remote to be, dialed by way of a route a test can take offline or have
/// take what it is sent and answer nothing.
struct Serving {
    server: RunningServer,
    /// Its one Provider, and the Models that Provider offers.
    provider: ControlledProvider,
    hosted: (ProviderId, Vec<ModelDescriptor>),
    route: ObservedTcpProxy,
    config: ServerConfig,
    /// How often the Remote's streams say they are still there.
    keep_alive: Duration,
    _directories: [tempfile::TempDir; 2],
}

impl Serving {
    /// A Server for `channel`, Serving on a port of its own that it keeps
    /// across a restart, so the route to it outlives one.
    async fn start(channel: &str) -> Self {
        Self::start_keeping_alive(channel, ServerTimings::default().sse_keepalive_interval).await
    }

    /// A Server for `channel` as [`Self::start`] makes one, whose streams say
    /// they are still there every `keep_alive`.
    async fn start_keeping_alive(channel: &str, keep_alive: Duration) -> Self {
        Self::start_hosting(
            channel,
            keep_alive,
            (ProviderId::new("claude"), claude_models()),
        )
        .await
    }

    /// A Server for `channel` as [`Self::start_keeping_alive`] makes one,
    /// hosting the Provider `hosted` names with the Models it gives.
    async fn start_hosting(
        channel: &str,
        keep_alive: Duration,
        hosted: (ProviderId, Vec<ModelDescriptor>),
    ) -> Self {
        let state = tempfile::tempdir().expect("create the Remote's state directory");
        let config_root = tempfile::tempdir().expect("create the Remote's config directory");
        let config = ServerConfig::new(state.path(), format!("{channel}-remote"))
            .expect("configure the Remote")
            .with_config_dir(config_root.path());
        let (server, provider) = Self::spawn(&config, PROTOCOL_VERSION, keep_alive, &hosted).await;
        for mutation in [
            SettingMutation::ServingPort { value: Some(0) },
            SettingMutation::ServingBindAddress {
                value: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            },
            SettingMutation::ServingEnabled { value: Some(true) },
        ] {
            mutate_setting(server.descriptor(), mutation).await;
        }
        let address = server.serving_address().expect("the Remote is Serving");
        mutate_setting(
            server.descriptor(),
            SettingMutation::ServingPort {
                value: Some(address.port()),
            },
        )
        .await;
        Self {
            server,
            provider,
            hosted,
            route: ObservedTcpProxy::start(address).await,
            config,
            keep_alive,
            _directories: [state, config_root],
        }
    }

    async fn spawn(
        config: &ServerConfig,
        protocol_version: u32,
        keep_alive: Duration,
        (provider, models): &(ProviderId, Vec<ModelDescriptor>),
    ) -> (RunningServer, ControlledProvider) {
        let (runtime, controlled) =
            ControlledProvider::with_provider(provider.clone(), models.clone());
        let server = server::spawn_with_provider_and_timings(
            config.clone(),
            runtime,
            ServerTimings {
                shutdown_grace: Duration::from_millis(5),
                pairing_protocol_version: protocol_version,
                sse_keepalive_interval: keep_alive,
                ..ServerTimings::default()
            },
        )
        .await
        .expect("spawn the Remote");
        (server, controlled)
    }

    fn descriptor(&self) -> RuntimeDescriptor {
        self.server.descriptor().clone()
    }

    /// The same Server stopped and started again speaking `protocol_version`
    /// to its Peers, Serving where it was.
    async fn restart_speaking(self, protocol_version: u32) -> Self {
        self.restart(protocol_version, |_| {}).await
    }

    /// The same Server stopped, its database `meanwhile` handed while it is
    /// stopped, and started again speaking `protocol_version` to its Peers,
    /// Serving where it was.
    async fn restart(self, protocol_version: u32, meanwhile: impl FnOnce(&Path)) -> Self {
        let Self {
            server,
            hosted,
            route,
            config,
            keep_alive,
            _directories,
            ..
        } = self;
        server.shutdown().await.expect("stop the Remote");
        meanwhile(&config.data_dir().join("suru.db"));
        let (server, provider) = Self::spawn(&config, protocol_version, keep_alive, &hosted).await;
        Self {
            server,
            provider,
            hosted,
            route,
            config,
            keep_alive,
            _directories,
        }
    }

    /// An Invite to this Server, by way of its route.
    async fn invite(&self) -> String {
        let invite: IssuedInvite = posted(
            &self.descriptor(),
            "/v1/pairing/invites",
            &IssueInviteRequest {
                ways: vec![Way::Direct(self.route.address)],
            },
        )
        .await;
        invite.invite
    }

    async fn shutdown(self) {
        self.server.shutdown().await.expect("shut down the Remote");
    }
}

/// Redeems an Invite to `remote` at the Server `own` describes, naming the
/// Remote `name`, and answers how the Server answered.
async fn redeem(own: &RuntimeDescriptor, remote: &Serving, name: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/pairing/remotes", own.base_url))
        .bearer_auth(&own.token)
        .json(&RedeemInviteRequest {
            invite: remote.invite().await,
            name: Some(name.to_owned()),
            ways: Vec::new(),
        })
        .send()
        .await
        .expect("redeem the Invite")
}

/// Pairs the Server `own` describes with `remote`, naming it `name`.
async fn pair(own: &RuntimeDescriptor, remote: &Serving, name: &str) {
    let redeemed: Remote = redeem(own, remote, name)
        .await
        .error_for_status()
        .expect("the Pairing forms")
        .json()
        .await
        .expect("decode the Remote");
    assert_eq!(redeemed.name, name);
}

/// The Sidekick's own Server for `channel`, running by `timings`, hosting the
/// Claude double, and holding from the first what the database `seed` holds,
/// where a test gives one.
async fn own_server(
    channel: &str,
    timings: ServerTimings,
    seed: Option<&Path>,
) -> (RunningServer, ControlledProvider, [tempfile::TempDir; 2]) {
    let state = tempfile::tempdir().expect("create the own Server's state directory");
    let config_root = tempfile::tempdir().expect("create the own Server's config directory");
    let config = ServerConfig::new(state.path(), format!("{channel}-own"))
        .expect("configure the Sidekick's own Server")
        .with_config_dir(config_root.path());
    if let Some(seed) = seed {
        std::fs::create_dir_all(config.data_dir()).expect("create the own Server's data root");
        std::fs::copy(seed, config.data_dir().join("suru.db")).expect("seed the own Server");
    }
    let (runtime, claude) =
        ControlledProvider::with_provider(ProviderId::new("claude"), claude_models());
    let own = server::spawn_with_provider_and_timings(
        config,
        runtime,
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..timings
        },
    )
    .await
    .expect("spawn the Sidekick's own Server");
    (own, claude, [state, config_root])
}

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

    /// Every act on a Remote's Session this Server holds a record of, as
    /// (Origin, the Session acted on).
    fn stored_remote_acts(&self) -> Vec<(String, String)> {
        self.stored_remote_act_states()
            .into_iter()
            .map(|(origin, session_id, _)| (origin, session_id))
            .collect()
    }

    /// Every act on a Remote's Session this Server holds a record of, as
    /// (Origin, the Session acted on, whether the act is confirmed).
    fn stored_remote_act_states(&self) -> Vec<(String, String, bool)> {
        #[derive(QueryableByName)]
        struct Act {
            #[diesel(sql_type = Text)]
            origin: String,
            #[diesel(sql_type = Text)]
            session_id: String,
            #[diesel(sql_type = diesel::sql_types::Bool)]
            confirmed: bool,
        }
        let database = self.config.data_dir().join("suru.db");
        let mut database =
            SqliteConnection::establish(database.to_str().expect("the database's path is UTF-8"))
                .expect("open the own Server's database");
        // The running Server may be writing as it is read.
        diesel::sql_query("PRAGMA busy_timeout = 5000")
            .execute(&mut database)
            .expect("wait out the Server's writes");
        diesel::sql_query(
            "SELECT origin, session_id, confirmed FROM sidekick_acts WHERE origin <> ''",
        )
        .load::<Act>(&mut database)
        .expect("read the recorded acts")
        .into_iter()
        .map(|act| (act.origin, act.session_id, act.confirmed))
        .collect()
    }
}

/// Two Servers paired in-process, each hosting the Claude double: the
/// Sidekick's own, and the Remote it knows as [`REMOTE`].
struct Paired {
    own: RunningServer,
    claude: ControlledProvider,
    remote: Serving,
    _directories: [tempfile::TempDir; 2],
}

impl Paired {
    async fn shutdown(self) {
        self.own
            .shutdown()
            .await
            .expect("shut down the Sidekick's own Server");
        self.remote.shutdown().await;
    }
}

/// What the Server `descriptor` describes answers a `POST` of `body` to
/// `path` with.
async fn posted<T: serde::de::DeserializeOwned>(
    descriptor: &RuntimeDescriptor,
    path: &str,
    body: &impl serde::Serialize,
) -> T {
    reqwest::Client::new()
        .post(format!("{}{path}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(body)
        .send()
        .await
        .unwrap_or_else(|error| panic!("post {path}: {error}"))
        .error_for_status()
        .unwrap_or_else(|error| panic!("{path} is answered: {error}"))
        .json()
        .await
        .unwrap_or_else(|error| panic!("decode what {path} answered: {error}"))
}

/// Two Servers for `channel`, the Sidekick's own running by `timings`, its
/// Remote Serving on this machine's loopback and paired as [`REMOTE`].
async fn paired(channel: &str, timings: ServerTimings) -> Paired {
    let remote = Serving::start(channel).await;
    let (own, claude, directories) = own_server(channel, timings, None).await;
    pair(own.descriptor(), &remote, REMOTE).await;
    Paired {
        own,
        claude,
        remote,
        _directories: directories,
    }
}

/// A Relay in-process whose identity provider the test scripts, reached at a
/// route of its own whose address it is known by.
struct TestRelay {
    running: suru_relay::RunningRelay,
    provider: std::sync::Arc<suru_relay::ScriptedProvider>,
    route: ObservedTcpProxy,
    _directory: tempfile::TempDir,
}

impl TestRelay {
    async fn start() -> Self {
        let directory = tempfile::tempdir().expect("create the Relay's directory");
        let provider = std::sync::Arc::new(suru_relay::ScriptedProvider::new());
        // The route comes first, since the Relay is known by the address
        // Servers reach it at, and is pointed at the Relay once it runs.
        let route = ObservedTcpProxy::start((Ipv4Addr::LOCALHOST, 9).into()).await;
        let running = suru_relay::start(
            suru_relay::RelayConfig::new(
                (Ipv4Addr::LOCALHOST, 0).into(),
                directory.path().join("relay.db"),
                format!("http://{}", route.address),
            )
            .with_connection_log(std::io::sink()),
            provider.clone(),
        )
        .await
        .expect("start the Relay");
        route.retarget(running.address());
        Self {
            running,
            provider,
            route,
            _directory: directory,
        }
    }

    /// Where a Server reaches the Relay.
    fn address(&self) -> String {
        format!("http://{}", self.route.address)
    }

    /// The Server `server` describes's Relay route beneath `/v1/relays` for
    /// this Relay and `rest`, its address one segment, slashes and all.
    fn route_of(&self, server: &RuntimeDescriptor, rest: &[&str]) -> reqwest::Url {
        let mut url = reqwest::Url::parse(&server.base_url).expect("a Server's address");
        url.path_segments_mut()
            .expect("a Server's address takes a path")
            .extend(["v1", "relays", &self.address()])
            .extend(rest);
        url
    }

    /// Adds the Relay to the Server `server` describes and logs it in there
    /// as the one identity every Server of the test is logged in as.
    async fn log_in(&self, server: &RuntimeDescriptor) {
        let http = reqwest::Client::new();
        let _: suru::protocol::Relay = posted(
            server,
            "/v1/relays",
            &suru::protocol::AddRelayRequest {
                address: self.address(),
            },
        )
        .await;
        let login: suru::protocol::RelayLogin = http
            .post(self.route_of(server, &["login"]))
            .bearer_auth(&server.token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("begin a login at the Relay")
            .json()
            .await
            .expect("decode the login");
        assert!(self.provider.approve(
            &login.user_code,
            suru_relay::Identity {
                subject: "583231".to_owned(),
                username: "octocat".to_owned(),
            },
        ));
        let logged_in = timeout(PROGRESS_DEADLINE, async {
            loop {
                let relays: Vec<suru::protocol::Relay> = http
                    .get(format!("{}/v1/relays", server.base_url))
                    .bearer_auth(&server.token)
                    .send()
                    .await
                    .expect("list the Server's Relays")
                    .json()
                    .await
                    .expect("decode the Server's Relays");
                if relays
                    .iter()
                    .any(|relay| relay.state == suru::protocol::RelayState::LoggedIn)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        logged_in.expect("the Server is logged in at the Relay");
    }

    /// Has the Server `server` describes Serve through the Relay.
    async fn serve_through(&self, server: &RuntimeDescriptor) {
        reqwest::Client::new()
            .put(self.route_of(server, &["serve-through"]))
            .bearer_auth(&server.token)
            .json(&suru::protocol::RelayServeThroughRequest {
                serve_through: true,
            })
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("Serve through the Relay");
    }

    /// Waits until `server` waits at the Relay to be reached, as a Server
    /// logged in there under the same Account finds by being joined to it:
    /// Serving through a Relay is chosen at once, and waited on as the Server
    /// next connects there.
    async fn waits(&self, server: &Serving) {
        let key = std::fs::read(server.config.data_dir().join("server-identity.pk8"))
            .expect("read the Server's identity key");
        let key = rcgen::KeyPair::try_from(key.as_slice()).expect("decode the identity key");
        let voice = crate::server_support::relay_voice::RelayVoice {
            at: self.route.address,
            known_as: self.address(),
        };
        let asker = rcgen::KeyPair::generate().expect("make a key to ask with");
        voice
            .log_in(&self.provider, &asker, "583231", "octocat")
            .await;
        voice
            .joined(&asker, &rcgen::PublicKeyData::subject_public_key_info(&key))
            .await;
    }

    async fn shutdown(self) {
        self.running.shutdown().await.expect("stop the Relay");
    }
}

/// Two Servers for `channel` as [`paired`] pairs them, but by an Invite
/// offering only a Relay the Remote Serves through, at which both are logged
/// in under one Account.
async fn paired_through_relay(channel: &str, timings: ServerTimings) -> (Paired, TestRelay) {
    let remote = Serving::start(channel).await;
    let (own, claude, directories) = own_server(channel, timings, None).await;
    let relay = TestRelay::start().await;
    relay.log_in(&remote.descriptor()).await;
    relay.log_in(own.descriptor()).await;
    relay.serve_through(&remote.descriptor()).await;
    relay.waits(&remote).await;
    let invite: IssuedInvite = posted(
        &remote.descriptor(),
        "/v1/pairing/invites",
        &IssueInviteRequest {
            ways: vec![Way::Relay(relay.address())],
        },
    )
    .await;
    let redeemed: Remote = posted(
        own.descriptor(),
        "/v1/pairing/remotes",
        &RedeemInviteRequest {
            invite: invite.invite,
            name: Some(REMOTE.to_owned()),
            ways: Vec::new(),
        },
    )
    .await;
    assert_eq!(redeemed.ways, vec![Way::Relay(relay.address())]);
    (
        Paired {
            own,
            claude,
            remote,
            _directories: directories,
        },
        relay,
    )
}

/// What `tool` answers `arguments` with, read from the structured content the
/// call carries.
async fn answered(client: &mut McpClient, tool: &str, arguments: Value) -> Value {
    let result = client.call_tool(tool, arguments).await;
    assert_ne!(result["isError"], json!(true), "{tool} answers: {result}");
    result["structuredContent"].clone()
}

/// The `origin` each row of `listing`'s `rows` carries, in its order: `None`
/// for one that carries none.
fn origins<'a>(listing: &'a Value, rows: &str) -> Vec<Option<&'a str>> {
    listing[rows]
        .as_array()
        .unwrap_or_else(|| panic!("a listing lists {rows}: {listing}"))
        .iter()
        .map(|row| {
            assert!(
                row.get("origin").is_none_or(Value::is_string),
                "an origin is a name: {row}"
            );
            row.get("origin").and_then(Value::as_str)
        })
        .collect()
}

/// `reading` without the `origin` a read through the Pairing names its
/// Session's Remote by, to set beside the same read made on that Remote.
fn without_origin(mut reading: Value) -> Value {
    reading
        .as_object_mut()
        .expect("a reading is an object")
        .remove("origin");
    reading
}

/// Has `provider` settle its working Turn as completed, and waits until
/// `session_id`'s latest Turn on the Server `descriptor` describes has.
async fn complete_turn(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
) {
    provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(descriptor, session_id, TurnStatus::Completed).await;
}

/// What the Remote is said to be when it cannot be reached at all.
fn unreachable() -> String {
    format!(
        "The Remote `{REMOTE}` is not answering: Suru could not reach it at any address it was \
         paired at."
    )
}

/// The moment a row's `last_active` spells.
fn last_active(spelled: &Value) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(
        spelled
            .as_str()
            .expect("a row says when it was last active"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("a row's last activity is an RFC 3339 moment")
}

/// The rows of a listing of Sessions, as their Titles and origins, having
/// checked they stand most recently active first by the `last_active` each
/// row reports. Each Server stamps its own Sessions by its own clock, so two
/// Servers' rows interleave by those stamps, never by the order a test made
/// them in.
fn by_recency(listing: &Value) -> Vec<(String, Option<String>)> {
    let rows = listing["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("a listing lists Sessions: {listing}"));
    for pair in rows.windows(2) {
        assert!(
            last_active(&pair[0]["last_active"]) >= last_active(&pair[1]["last_active"]),
            "rows stand most recently active first: {listing}"
        );
    }
    titles(listing)
        .into_iter()
        .map(str::to_owned)
        .zip(
            origins(listing, "sessions")
                .into_iter()
                .map(|origin| origin.map(str::to_owned)),
        )
        .collect()
}

/// `rows`, in an order that says nothing of when each was last active.
fn sorted(mut rows: Vec<(String, Option<String>)>) -> Vec<(String, Option<String>)> {
    rows.sort();
    rows
}

/// The `session_id` of every row of a listing of Sessions, in its order.
fn ids(listing: &Value) -> Vec<Value> {
    listing["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("a listing lists Sessions: {listing}"))
        .iter()
        .map(|row| row["session_id"].clone())
        .collect()
}

#[tokio::test]
async fn list_remotes_names_each_remote_and_whether_it_answers_now() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (unpaired, mut unpaired_claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-remotes-unpaired",
    )
    .await;
    let (_alone, mut alone, _alone_provider) =
        start_sidekick(unpaired.descriptor(), &mut unpaired_claude).await;
    assert_eq!(
        answered(&mut alone, "list_remotes", json!({})).await,
        json!({ "remotes": [] }),
        "a Server paired with no Remote names none"
    );
    unpaired.shutdown().await.expect("shut down server");

    let mut pair = paired("sidekick-remotes-answering", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    assert_eq!(
        answered(&mut sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": true }] })
    );

    pair.remote.route.set_online(false).await;
    assert_eq!(
        answered(&mut sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": false, "reason": unreachable() }] }),
        "a Remote that cannot be reached is asked, and named as not answering, saying why"
    );

    pair.remote.route.set_online(true).await;
    assert_eq!(
        answered(&mut sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": true }] }),
        "and answering again the moment it does"
    );
    assert!(
        sidekick
            .refusal("list_remotes", json!({ "origin": REMOTE }))
            .await
            .contains("list_remotes takes no arguments"),
        "list_remotes takes nothing"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_listing_is_this_servers_by_default_a_remotes_by_its_origin_and_every_servers_everywhere()
{
    let (clock, hand) = ServerClock::manual();
    let mut pair = paired(
        "sidekick-remotes-listing",
        ServerTimings::default().with_clock(clock),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let here = tempfile::tempdir().expect("create a Workspace on the own Server");
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");

    let (charted, charted_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Chart the atlas",
    )
    .await;
    complete_turn(&remote, charted, &charted_provider).await;
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    working_session(&own, here.path(), "Print the atlas").await;
    let bound = working_session(&remote, there.path(), "Bind the ledger").await;
    working_session(&own, here.path(), "Audit the ledger").await;

    let mine = list_sessions(&mut sidekick, json!({})).await;
    assert_eq!(
        titles(&mine),
        ["Audit the ledger", "Print the atlas", "Plan the work"],
        "a listing is the Sidekick's own Server's unless it names another: {mine}"
    );
    assert_eq!(
        origins(&mine, "sessions"),
        [None, None, None],
        "and its rows carry no Origin"
    );
    assert!(mine.get("unanswered").is_none(), "{mine}");

    let theirs = list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await;
    assert_eq!(titles(&theirs), ["Bind the ledger", "Chart the atlas"]);
    assert_eq!(
        origins(&theirs, "sessions"),
        [Some(REMOTE), Some(REMOTE)],
        "a Remote's rows carry its name: {theirs}"
    );
    assert_eq!(theirs["sessions"][0]["session_id"], json!(bound));
    assert_eq!(
        theirs["sessions"][0]["workspace"],
        json!(suru::paths::canonical(there.path()).expect("read the Remote's Workspace")),
        "a Remote's row names the Workspace its Session works in there"
    );
    assert_eq!(
        (
            &theirs["sessions"][0]["standing"],
            &theirs["sessions"][1]["standing"]
        ),
        (&json!("working"), &json!("done")),
        "each row stands as its Remote's own listing says"
    );

    let everywhere = list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await;
    let mut merged = by_recency(&everywhere);
    let mut each = by_recency(&mine);
    each.extend(by_recency(&theirs));
    merged.sort();
    each.sort();
    assert_eq!(
        merged, each,
        "Everywhere is one listing of every Server's rows, most recently active first by the \
         moment each Server stamped: {everywhere}"
    );
    assert!(everywhere.get("unanswered").is_none(), "{everywhere}");

    // Every filter, the limit and the count left out range over the rows of
    // every Server together.
    let atlas = list_sessions(
        &mut sidekick,
        json!({ "origin": "everywhere", "title": "ATLAS" }),
    )
    .await;
    assert_eq!(
        sorted(by_recency(&atlas)),
        [
            ("Chart the atlas".to_owned(), Some(REMOTE.to_owned())),
            ("Print the atlas".to_owned(), None),
        ]
    );
    let their_workspace = theirs["sessions"][0]["workspace"].clone();
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "origin": "everywhere", "workspace": their_workspace })
            )
            .await
        ),
        ["Bind the ledger", "Chart the atlas"]
    );
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "origin": "everywhere", "standing": "done" })
            )
            .await
        ),
        ["Chart the atlas"]
    );
    let limited = list_sessions(&mut sidekick, json!({ "origin": "everywhere", "limit": 2 })).await;
    assert_eq!(titles(&limited), titles(&everywhere)[..2]);
    assert_eq!(limited["omitted"], json!(3));
    let rows = everywhere["sessions"].as_array().expect("rows");
    for boundary in rows {
        let boundary = &boundary["last_active"];
        let at = last_active(boundary);
        let after = list_sessions(
            &mut sidekick,
            json!({ "origin": "everywhere", "active_after": boundary }),
        )
        .await;
        let before = list_sessions(
            &mut sidekick,
            json!({ "origin": "everywhere", "active_before": boundary }),
        )
        .await;
        let split = |keep: &dyn Fn(time::OffsetDateTime) -> bool| {
            rows.iter()
                .filter(|row| keep(last_active(&row["last_active"])))
                .map(|row| row["session_id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&after),
            split(&|active| active >= at),
            "active_after keeps every Server's rows last active at or after {boundary}"
        );
        assert_eq!(
            ids(&before),
            split(&|active| active < at),
            "and active_before every Server's rows last active before it"
        );
    }

    // Auto-settle reads every row against the Sidekick's own Server's
    // Setting and clock, as the user's Everywhere does.
    hand.advance(Duration::from_secs(4 * 24 * 60 * 60));
    let settled = list_sessions(
        &mut sidekick,
        json!({ "origin": "everywhere", "liveness": "settled" }),
    )
    .await;
    assert_eq!(titles(&settled), ["Chart the atlas"], "{settled}");
    assert_eq!(settled["sessions"][0]["settled"], json!(true));
    assert_eq!(settled["sessions"][0]["origin"], json!(REMOTE));
    assert_eq!(
        sorted(by_recency(
            &list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await
        )),
        [
            ("Audit the ledger".to_owned(), None),
            ("Bind the ledger".to_owned(), Some(REMOTE.to_owned())),
            ("Plan the work".to_owned(), None),
            ("Print the atlas".to_owned(), None),
        ]
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn an_everywhere_listing_names_a_remote_that_does_not_answer_and_nothing_it_said_before() {
    let mut pair = paired("sidekick-remotes-unanswered", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let here = tempfile::tempdir().expect("create a Workspace on the own Server");
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    working_session(&remote, there.path(), "Bind the ledger").await;
    working_session(&own, here.path(), "Audit the ledger").await;
    let answering = [
        ("Audit the ledger".to_owned(), None),
        ("Bind the ledger".to_owned(), Some(REMOTE.to_owned())),
        ("Plan the work".to_owned(), None),
    ];
    assert_eq!(
        sorted(by_recency(
            &list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await
        )),
        answering
    );
    let workspaces = answered(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": "everywhere" }),
    )
    .await;
    assert!(
        origins(&workspaces, "workspaces").contains(&Some(REMOTE)),
        "{workspaces}"
    );

    pair.remote.route.set_online(false).await;
    let unanswered = json!([{ "origin": REMOTE, "reason": unreachable() }]);
    let everywhere = list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await;
    assert_eq!(
        titles(&everywhere),
        ["Audit the ledger", "Plan the work"],
        "nothing the Remote listed before is listed in its place: {everywhere}"
    );
    assert_eq!(origins(&everywhere, "sessions"), [None, None]);
    assert_eq!(everywhere["omitted"], json!(0));
    assert_eq!(
        everywhere["unanswered"], unanswered,
        "the Remote is named as not answering, saying why"
    );
    let workspaces = answered(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": "everywhere" }),
    )
    .await;
    assert!(
        origins(&workspaces, "workspaces")
            .iter()
            .all(Option::is_none),
        "{workspaces}"
    );
    assert_eq!(workspaces["unanswered"], unanswered);
    assert!(
        list_sessions(&mut sidekick, json!({}))
            .await
            .get("unanswered")
            .is_none(),
        "a listing of the Sidekick's own Server asks no Remote"
    );

    assert_eq!(
        sidekick
            .refusal("list_sessions", json!({ "origin": REMOTE }))
            .await,
        format!("{} Its Sessions were not listed.", unreachable()),
        "a listing of the Remote alone is refused, saying why"
    );
    assert_eq!(
        sidekick
            .refusal("list_workspaces", json!({ "origin": REMOTE }))
            .await,
        format!("{} Its Workspaces were not listed.", unreachable())
    );

    pair.remote.route.set_online(true).await;
    let everywhere = list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await;
    assert_eq!(
        sorted(by_recency(&everywhere)),
        answering,
        "the Remote's rows return the moment it answers: {everywhere}"
    );
    assert!(everywhere.get("unanswered").is_none(), "{everywhere}");

    pair.shutdown().await;
}

#[tokio::test]
async fn a_remote_that_says_nothing_is_named_as_not_answering_once_the_reach_timeout_passes() {
    let mut pair = paired(
        "sidekick-remotes-silent",
        ServerTimings::default().with_remote_reach_timeout(Duration::from_millis(300)),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let bound = working_session(&remote, there.path(), "Bind the ledger").await;

    pair.remote.route.swallow_connections().await;
    let silent =
        format!("The Remote `{REMOTE}` is not answering: it said nothing within 300 milliseconds.");
    assert_eq!(
        answered(&mut sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": false, "reason": silent }] })
    );
    let everywhere = list_sessions(&mut sidekick, json!({ "origin": "everywhere" })).await;
    assert_eq!(titles(&everywhere), ["Plan the work"], "{everywhere}");
    assert_eq!(
        everywhere["unanswered"],
        json!([{ "origin": REMOTE, "reason": silent }])
    );
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": bound, "origin": REMOTE })
            )
            .await,
        format!("{silent} Session `{bound}` was not read.")
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_remotes_session_reads_through_the_pairing_as_the_same_read_reads_on_that_remote() {
    let mut pair = paired("sidekick-remotes-reading", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    // A Sidekick of the Remote's own, reading there as any read on the Remote
    // reads.
    let (_theirs, mut on_the_remote, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;

    let (session_id, mut provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Look at the ledger.",
    )
    .await;
    write_agent_message(&provider, "The ledger has a flaky test.").await;
    complete_turn(&remote, session_id, &provider).await;
    admit_prompt(&remote, session_id, "Fix it.").await;
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Turn reaches the Provider")
        .succeed();
    write_agent_message(&provider, "Let me run the tests first.").await;
    let command = ProviderActivityId::new("cargo test");
    for event in [
        ProviderEvent::CommandStarted {
            activity_id: command.clone(),
            command: "cargo test -p ledger".to_owned(),
            cwd: None,
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: command.clone(),
            content: "test sync ... FAILED".to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: command,
            status: ProviderCommandStatus::Completed,
            exit_status: Some(101),
        },
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }
    write_agent_message(&provider, "Fixed the race in sync; the tests pass.").await;
    complete_turn(&remote, session_id, &provider).await;

    for asked in [
        json!({}),
        json!({ "turns": 2 }),
        json!({ "before": "2" }),
        json!({ "detail": "activities" }),
        json!({ "max_chars": 12 }),
        json!({ "item": "2.3" }),
    ] {
        let mut arguments = asked.clone();
        arguments["session_id"] = json!(session_id);
        let read_there = answered(&mut on_the_remote, "read_session", arguments.clone()).await;
        arguments["origin"] = json!(REMOTE);
        let read_through = answered(&mut sidekick, "read_session", arguments).await;
        assert!(read_there.get("origin").is_none(), "{read_there}");
        assert_eq!(read_through["origin"], json!(REMOTE), "{read_through}");
        assert_eq!(
            without_origin(read_through),
            read_there,
            "{asked} reads through the Pairing as it reads on the Remote"
        );
    }
    let read = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": session_id, "origin": REMOTE, "item": "2.3" }),
    )
    .await;
    assert_eq!(
        read["transcript"],
        json!(
            "2.3 command [completed, exit 101]: cargo test -p ledger\noutput:\ntest sync ... FAILED"
        ),
        "an entry is read whole, its output included"
    );
    let read = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": session_id, "origin": REMOTE }),
    )
    .await;
    assert_eq!(
        (&read["status"], &read["standing"]),
        (&json!("idle"), &json!("done"))
    );

    // An open Questionnaire reads whole, its identity among it.
    let (asking, asking_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Run the tests.",
    )
    .await;
    let questionnaire = suru::protocol::Questionnaire {
        id: suru::protocol::QuestionnaireId::new(),
        questions: vec![suru::questionnaire::Question {
            id: "machine".to_owned(),
            title: Some("Machine".to_owned()),
            text: "Where should the tests run?".to_owned(),
            choices: vec![suru::questionnaire::QuestionChoice {
                id: "staging".to_owned(),
                label: "Staging".to_owned(),
                description: None,
                recommended: true,
            }],
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    };
    asking_provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: questionnaire.clone(),
        })
        .await;
    read_session_until(
        &reqwest::Client::new(),
        &remote,
        asking,
        "the Questionnaire waits on an Answer",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Questionnaire { .. }))
        },
    )
    .await;
    let read_there = answered(
        &mut on_the_remote,
        "read_session",
        json!({ "session_id": asking }),
    )
    .await;
    let read_through = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": asking, "origin": REMOTE }),
    )
    .await;
    assert_eq!(read_through["standing"], json!("needs_intervention"));
    assert_eq!(
        read_through["questionnaires"][0]["id"],
        json!(questionnaire.id),
        "{read_through}"
    );
    assert_eq!(without_origin(read_through), read_there);

    // Reading is no Viewed report, on a Remote as anywhere.
    let done = list_sessions(
        &mut sidekick,
        json!({ "origin": REMOTE, "standing": "done" }),
    )
    .await;
    assert_eq!(done["sessions"][0]["session_id"], json!(session_id));

    pair.shutdown().await;
}

/// What the Remote's own Session API says of `session_id`: its snapshot, and
/// its row in the Remote's listing.
async fn as_the_remote_holds_it(
    remote: &RuntimeDescriptor,
    session_id: SessionId,
) -> (Value, Value) {
    let http = reqwest::Client::new();
    let snapshot = http
        .get(format!("{}/v1/sessions/{session_id}", remote.base_url))
        .bearer_auth(&remote.token)
        .send()
        .await
        .expect("read the Session on the Remote")
        .json::<Value>()
        .await
        .expect("decode the Session");
    let listing = http
        .get(format!("{}/v1/sessions", remote.base_url))
        .bearer_auth(&remote.token)
        .send()
        .await
        .expect("list the Remote's Sessions")
        .json::<Vec<Value>>()
        .await
        .expect("decode the Remote's listing");
    let row = listing
        .into_iter()
        .find(|row| row["summary"]["id"] == json!(session_id))
        .expect("the Remote lists the Session");
    (snapshot, row)
}

/// A Session on a Remote whose Approval awaits the user reads as it reads
/// there — the Approval named as the user's to decide — and reading it, at
/// every detail, leaves it on the Remote as it was: no Viewed report, no
/// revision, nothing its listing says moved.
#[tokio::test]
async fn reading_a_remotes_session_names_its_approval_and_changes_nothing_there() {
    let mut pair = paired("sidekick-remotes-unchanged", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (_theirs, mut on_the_remote, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;
    let (approving, approving_provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Build it.",
    )
    .await;
    approving_provider
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: suru::protocol::Approval {
                id: suru::protocol::ApprovalId::new(),
                subject: suru::protocol::ApprovalSubject::Command {
                    command: "cargo nextest run".into(),
                    cwd: None,
                    actions: Vec::new(),
                },
                reason: Some("The tests need the network.".to_owned()),
            },
            tool_activity_id: None,
        })
        .await;
    read_session_until(
        &reqwest::Client::new(),
        &remote,
        approving,
        "the Approval waits on the user's Decision",
        |snapshot| !snapshot.pending_approvals.is_empty(),
    )
    .await;
    let before = as_the_remote_holds_it(&remote, approving).await;

    for asked in [
        json!({}),
        json!({ "detail": "activities" }),
        json!({ "item": "1.2" }),
    ] {
        let mut arguments = asked.clone();
        arguments["session_id"] = json!(approving);
        let read_there = answered(&mut on_the_remote, "read_session", arguments.clone()).await;
        arguments["origin"] = json!(REMOTE);
        let read_through = answered(&mut sidekick, "read_session", arguments).await;
        assert_eq!(
            without_origin(read_through),
            read_there,
            "{asked} reads through the Pairing as it reads on the Remote"
        );
    }
    let read = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": approving, "origin": REMOTE }),
    )
    .await;
    assert_eq!(read["standing"], json!("needs_intervention"));
    assert_eq!(
        read["approvals"],
        json!([
            "Approval 1.2 awaits the user's Decision on whether the Agent may run `cargo nextest \
             run`. It gives as its reason \"The tests need the network.\". Only the user can \
             decide it, so tell them it is waiting on them."
        ])
    );
    let needs_the_user = list_sessions(
        &mut sidekick,
        json!({ "origin": REMOTE, "standing": "needs_intervention" }),
    )
    .await;
    assert_eq!(ids(&needs_the_user), [json!(approving)]);

    assert_eq!(
        as_the_remote_holds_it(&remote, approving).await,
        before,
        "reading the Session through the Pairing changed nothing on the Remote"
    );

    pair.shutdown().await;
}

/// A Subagent's Session is listed nowhere, on a Remote as anywhere, so how it
/// stands is read from its own snapshot — and reads as the same read made on
/// that Remote, as it works and once it is done.
#[tokio::test]
async fn a_subagents_session_on_a_remote_reads_through_the_pairing_as_it_reads_there() {
    let mut pair = paired("sidekick-remotes-subagent", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (theirs, mut on_the_remote, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;
    let subagent = on_the_remote
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "opus",
            "name": "Scout",
            "description": "Look around",
            "prompt": "Look around the ledger.",
        }))
        .await;
    let (subagent_provider, _) = run_child(
        &mut pair.remote.provider,
        default_selection(&claude_models()),
    )
    .await;
    write_agent_message(&subagent_provider, "Nothing amiss.").await;

    let read_there = answered(
        &mut on_the_remote,
        "read_session",
        json!({ "session_id": subagent }),
    )
    .await;
    let read_through = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": subagent, "origin": REMOTE }),
    )
    .await;
    assert_eq!(
        (&read_through["parent"], &read_through["standing"]),
        (&json!(theirs), &json!("working")),
        "{read_through}"
    );
    assert_eq!(without_origin(read_through), read_there);

    complete_turn(&remote, subagent, &subagent_provider).await;
    let read_there = answered(
        &mut on_the_remote,
        "read_session",
        json!({ "session_id": subagent }),
    )
    .await;
    let read_through = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": subagent, "origin": REMOTE }),
    )
    .await;
    assert_eq!(read_through["standing"], json!("done"), "{read_through}");
    assert_eq!(without_origin(read_through), read_there);

    pair.shutdown().await;
}

#[tokio::test]
async fn reading_at_an_origin_that_is_unknown_or_does_not_answer_is_refused_in_words_to_relay() {
    let mut pair = paired("sidekick-remotes-refusals", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let bound = working_session(&remote, there.path(), "Bind the ledger").await;

    let unknown = "Suru is paired with no Remote named `laptop`; list_remotes names the Remotes \
                   it is paired with, and leaving `origin` out reaches this server.";
    for (tool, arguments) in [
        (
            "read_session",
            json!({ "session_id": bound, "origin": "laptop" }),
        ),
        ("list_sessions", json!({ "origin": "laptop" })),
        ("list_workspaces", json!({ "origin": "laptop" })),
    ] {
        assert_eq!(
            sidekick.refusal(tool, arguments.clone()).await,
            unknown,
            "{tool} {arguments}"
        );
    }
    assert!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": bound, "origin": "everywhere" })
            )
            .await
            .contains("`everywhere` names no one server"),
        "a Session is read on the one Server it lives on"
    );
    assert!(
        sidekick
            .refusal("read_session", json!({ "session_id": bound, "origin": 7 }))
            .await
            .starts_with("read_session's `origin` must be a string")
    );
    assert_eq!(
        sidekick
            .refusal("read_session", json!({ "session_id": bound }))
            .await,
        format!(
            "Suru holds no Session `{bound}` on this server; pass a session_id, and the origin \
             beside it, as list_sessions or a Subagent row gives them."
        ),
        "a Session's identity names it only at its Origin"
    );
    let nowhere = SessionId::new();
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": nowhere, "origin": REMOTE })
            )
            .await,
        format!(
            "Suru holds no Session `{nowhere}` on the Remote `{REMOTE}`; pass a session_id, and \
             the origin beside it, as list_sessions or a Subagent row gives them."
        )
    );
    assert!(
        sidekick
            .refusal("list_providers", json!({ "origin": REMOTE }))
            .await
            .contains("list_providers takes no arguments"),
        "nothing but a Session or a Workspace is reached on a Remote"
    );

    pair.remote.route.set_online(false).await;
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": bound, "origin": REMOTE })
            )
            .await,
        format!("{} Session `{bound}` was not read.", unreachable()),
        "a Session whose Origin does not answer is refused, saying why"
    );

    pair.shutdown().await;
}

/// A Session its Remote holds but cannot read is listed as unreadable there,
/// and a read of it is refused saying so, as the same read on that Remote
/// would be.
#[tokio::test]
async fn a_session_its_remote_cannot_read_is_refused_saying_so() {
    let mut pair = paired("sidekick-remotes-unreadable", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let broken = working_session(&pair.remote.descriptor(), there.path(), "Bind the ledger").await;
    pair.remote = pair
        .remote
        .restart(PROTOCOL_VERSION, |database| {
            use diesel::{Connection, RunQueryDsl, SqliteConnection};
            let mut database = SqliteConnection::establish(database.to_str().unwrap())
                .expect("open the Remote's database");
            diesel::sql_query(format!(
                "UPDATE sessions SET workspace = '{{' WHERE id = '{broken}'"
            ))
            .execute(&mut database)
            .expect("spoil what the Remote stored of the Session");
        })
        .await;

    let theirs = list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await;
    assert_eq!(theirs["sessions"][0]["session_id"], json!(broken));
    assert_eq!(theirs["sessions"][0]["unreadable"], json!(true), "{theirs}");
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": broken, "origin": REMOTE })
            )
            .await,
        format!(
            "Suru holds Session `{broken}` on the Remote `{REMOTE}` but could not read what it \
             stored of it, so there is nothing to read."
        )
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_remotes_workspaces_are_listed_as_it_lists_them_each_carrying_its_name() {
    const {
        assert!(
            suru::protocol::PROTOCOL_VERSION >= 77,
            "a Remote's Workspaces are asked of it over the Pairing"
        );
    }
    let mut pair = paired("sidekick-remotes-workspaces", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let here = tempfile::tempdir().expect("create a Workspace on the own Server");
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let unworked = tempfile::tempdir().expect("create a directory no Session works in");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (_theirs, mut on_the_remote, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;
    working_session(&own, here.path(), "Audit the ledger").await;
    working_session(&remote, there.path(), "Bind the ledger").await;
    // A Workspace the Remote knows though no Session works there.
    answered(
        &mut on_the_remote,
        "set_workspace_description",
        json!({ "workspace": unworked.path(), "text": "Where the bindery keeps its patterns." }),
    )
    .await;

    let mine = answered(&mut sidekick, "list_workspaces", json!({})).await;
    assert!(
        origins(&mine, "workspaces").iter().all(Option::is_none),
        "{mine}"
    );
    let theirs = answered(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": REMOTE }),
    )
    .await;
    assert!(
        origins(&theirs, "workspaces")
            .iter()
            .all(|origin| *origin == Some(REMOTE)),
        "{theirs}"
    );
    let listed_there = answered(&mut on_the_remote, "list_workspaces", json!({})).await;
    assert_eq!(
        theirs["workspaces"]
            .as_array()
            .expect("rows")
            .iter()
            .cloned()
            .map(without_origin)
            .collect::<Vec<_>>(),
        listed_there["workspaces"].as_array().expect("rows").clone(),
        "a Remote's Workspaces are listed as it lists them: {theirs}"
    );
    let unworked_path =
        suru::paths::canonical(unworked.path()).expect("read the unworked directory");
    assert!(
        theirs["workspaces"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| {
                row["path"] == json!(unworked_path)
                    && row["description"]
                        == json!({ "text": "Where the bindery keeps its patterns.", "set": true })
            }),
        "among them one the Remote knows though no Session works there: {theirs}"
    );

    let everywhere = answered(
        &mut sidekick,
        "list_workspaces",
        json!({ "origin": "everywhere" }),
    )
    .await;
    let mut both = mine["workspaces"].as_array().expect("rows").clone();
    both.extend(
        theirs["workspaces"]
            .as_array()
            .expect("rows")
            .iter()
            .cloned(),
    );
    assert_eq!(
        everywhere,
        json!({ "workspaces": both }),
        "Everywhere lists this server's Workspaces, then each Remote's"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn only_a_sidekick_is_offered_list_remotes() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create a Workspace");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-remotes-offered",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut claude).await;
    let (_ordinary, handoff, _provider) = start_session(
        &descriptor,
        &mut claude,
        workspace.path(),
        default_selection(&claude_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&handoff);
    ordinary.initialize().await;

    assert!(
        listed_tools(&mut sidekick)
            .await
            .contains(&"list_remotes".to_owned())
    );
    assert!(
        !listed_tools(&mut ordinary)
            .await
            .contains(&"list_remotes".to_owned())
    );
    let refused = unoffered(&mut ordinary, "list_remotes", json!({})).await;
    assert_eq!(
        refused["message"],
        json!("The Broker offers no Tool named `list_remotes`"),
        "{refused}"
    );

    server.shutdown().await.expect("shut down server");
}

/// The Broker a Sidekick reaches a Remote through is its own machine's, on
/// that machine's loopback alone: it answers no request addressed to any
/// other host, and the Pairing carries no request to a Remote's.
#[tokio::test]
async fn the_broker_a_sidekick_reaches_remotes_through_stays_on_its_own_machines_loopback() {
    let mut pair = paired("sidekick-remotes-loopback", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let directory = sidekick_directory(&own).await;
    let (_sidekick, handoff, _provider) = start_session(
        &own,
        &mut pair.claude,
        &directory,
        default_selection(&claude_models()),
    )
    .await;
    let endpoint = reqwest::Url::parse(handoff.endpoint().as_str()).expect("the endpoint is a URL");
    assert!(
        endpoint
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback()),
        "the Sidekick is handed its own machine's loopback endpoint: {endpoint}"
    );
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;
    assert_eq!(
        answered(&mut sidekick, "list_remotes", json!({})).await,
        json!({ "remotes": [{ "name": REMOTE, "answers": true }] })
    );

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "elsewhere", "version": "0" },
        },
    });
    let http = reqwest::Client::new();
    let addressed_elsewhere = http
        .post(endpoint.as_str())
        .header(AUTHORIZATION, handoff.token().bearer())
        .header(HOST, format!("{REMOTE}.lan:7777"))
        .header(ACCEPT, "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .expect("reach the Broker");
    assert_eq!(
        addressed_elsewhere.status(),
        StatusCode::FORBIDDEN,
        "a request addressed to another host is refused, even with a Sidekick's token"
    );

    let through_the_pairing = format!("{}/v1/remotes/{REMOTE}/broker", own.base_url);
    let with_the_sidekicks_token = http
        .post(&through_the_pairing)
        .header(AUTHORIZATION, handoff.token().bearer())
        .header(ACCEPT, "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .expect("attempt the Remote's Broker");
    assert_eq!(
        with_the_sidekicks_token.status(),
        StatusCode::UNAUTHORIZED,
        "a Sidekick's token reaches nothing of a Remote's"
    );
    let with_the_api_token = http
        .post(&through_the_pairing)
        .bearer_auth(&own.token)
        .header(ACCEPT, "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .expect("attempt the Remote's Broker");
    assert_eq!(
        with_the_api_token.status(),
        StatusCode::NOT_FOUND,
        "and the Pairing carries nothing to a Remote's Broker"
    );

    pair.shutdown().await;
}
