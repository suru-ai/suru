//! Fixtures shared by more than one area of the session protocol tests.

use crate::provider_support::{
    ControlledProvider, ControlledProviderRuntime, ControlledProviderSession,
};
use crate::server_support::PROGRESS_DEADLINE;
use suru::{
    managed_client::{ManagedClient, SessionEvent},
    protocol::{
        Activity, AgentId, AgentIdentity, AgentSelection, CreateSessionRequest, InitialPrompt,
        ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionSelection, ModelOptionValue, PromptId, ProviderId,
        RuntimeDescriptor, SessionId, SessionRevision, SessionSnapshot, SessionUpdate,
    },
    server::{self, RunningServer, ServerConfig, ServerTimings},
};
use tokio::time::timeout;

/// One Provider's default Model, named after the Provider that serves it, which
/// is all any multi-Provider test needs of a catalog.
pub fn hosted_model(provider: &str, model: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(model),
        display_name: model.to_owned(),
        description: String::new(),
        is_default: true,
        availability: ModelAvailability::Available,
        options: Vec::new(),
    }
}

pub fn hosted_selection(provider: &str, model: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new(provider),
        model: ModelId::new(model),
        options: Vec::new(),
    }
}

pub async fn create_session(
    descriptor: &RuntimeDescriptor,
    request: &CreateSessionRequest,
) -> SessionSnapshot {
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session")
}

pub async fn read_session(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> SessionSnapshot {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session")
        .error_for_status()
        .expect("Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session")
}

pub async fn list_catalog(descriptor: &RuntimeDescriptor) -> ModelCatalog {
    reqwest::Client::new()
        .get(format!("{}/v1/models", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Models")
        .error_for_status()
        .expect("Model listing succeeds")
        .json::<ModelCatalog>()
        .await
        .expect("decode Model catalog")
}

pub async fn refresh_catalog(descriptor: &RuntimeDescriptor) -> ModelCatalog {
    reqwest::Client::new()
        .post(format!("{}/v1/models/refresh", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("refresh Model catalog")
        .error_for_status()
        .expect("Model refresh succeeds")
        .json::<ModelCatalog>()
        .await
        .expect("decode refreshed Model catalog")
}

pub fn controlled_selection(model: &str, effort: &str, speed: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new(model),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(speed),
                },
            },
        ],
    }
}

pub async fn next_session_update(
    subscription: &mut suru::managed_client::SessionSubscription,
) -> SessionUpdate {
    let SessionEvent::Updated(update) = timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("Session update is valid")
    else {
        panic!("expected a Session update");
    };
    update
}

pub async fn read_session_at_least_revision(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    revision: SessionRevision,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("read Session while awaiting revision")
                .error_for_status()
                .expect("Session remains readable")
                .json::<SessionSnapshot>()
                .await
                .expect("decode Session while awaiting revision");
            if snapshot.revision >= revision {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Session reaches expected revision")
}

/// A Session whose first Turn is running against the controlled Provider —
/// the state every Subagent test starts from, because only a working Turn's
/// Agent can spawn one.
pub struct WorkingTurn {
    _workspace: tempfile::TempDir,
    pub server: RunningServer,
    pub provider_session: ControlledProviderSession,
    /// The hosted runtime double itself, so a test can flip what the Provider
    /// declares — the per-Subagent stop capability today — mid-scenario.
    pub runtime: std::sync::Arc<ControlledProviderRuntime>,
    pub session_id: SessionId,
    pub client: reqwest::Client,
}

pub async fn working_turn(state_dir: &std::path::Path, channel: &str) -> WorkingTurn {
    working_turn_with_timings(state_dir, channel, ServerTimings::default()).await
}

/// [`working_turn`] on a server running at the given timings, for a test that
/// must see a production-scale interval — a stream's keepalive — pass at a
/// millisecond scale.
pub async fn working_turn_with_timings(
    state_dir: &std::path::Path,
    channel: &str,
    timings: ServerTimings,
) -> WorkingTurn {
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir, channel).expect("configure server"),
        runtime.clone(),
        timings,
    )
    .await
    .expect("spawn server");
    let client = reqwest::Client::new();
    let descriptor = server.descriptor().clone();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate the mapping".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-subagent", "high", "fast"),
    });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    WorkingTurn {
        _workspace: workspace,
        server,
        provider_session,
        runtime,
        session_id: created.session.id,
        client,
    }
}

/// The one Subagent Activity a snapshot holds, failing the test when the
/// Transcript carries none or more than one.
pub fn the_subagent_row(snapshot: &SessionSnapshot) -> &Activity {
    let mut rows = snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }));
    let row = rows.next().expect("the Transcript holds a Subagent row");
    assert!(
        rows.next().is_none(),
        "the Transcript holds exactly one Subagent row"
    );
    row
}

/// Asks the server to interrupt `session_id`, answering with the raw response
/// so refusals stay assertable.
pub async fn interrupt(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
    client
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
}

/// Asks the server to compact `session_id`'s context now, with `instructions`
/// for the summary where given, answering with the raw response so refusals
/// stay assertable.
pub async fn compact(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    instructions: Option<&str>,
) -> reqwest::Response {
    client
        .post(format!(
            "{}/v1/sessions/{session_id}/compact",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&suru::protocol::CompactSessionRequest {
            instructions: instructions.map(ToOwned::to_owned),
        })
        .send()
        .await
        .expect("send Session compaction request")
}

/// Polls the Session until its snapshot satisfies `predicate`, so a test can
/// wait on the state it means rather than on revision arithmetic.
pub async fn read_session_until(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    described: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("read Session while awaiting state")
                .error_for_status()
                .expect("Session remains readable")
                .json::<SessionSnapshot>()
                .await
                .expect("decode Session while awaiting state");
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Session reaches expected state: {described}"))
}

/// Opens the Session catalog SSE stream and decodes its leading snapshot, so a
/// test can read what the catalog says it holds before any change arrives.
pub async fn open_catalog_stream_with_snapshot(
    descriptor: &RuntimeDescriptor,
) -> (
    Vec<SessionId>,
    impl futures_util::Stream<Item = suru::protocol::SessionCatalogUpdate> + Unpin + use<>,
) {
    use eventsource_stream::Eventsource;
    use futures_util::StreamExt;

    let response = reqwest::Client::new()
        .get(format!("{}/v1/session-events", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Session catalog stream")
        .error_for_status()
        .expect("the catalog stream authenticates");
    let mut events = Box::pin(response.bytes_stream().eventsource());
    let snapshot = loop {
        let event = timeout(PROGRESS_DEADLINE, events.next())
            .await
            .expect("the catalog snapshot arrives")
            .expect("the catalog stream stays open")
            .expect("the catalog stream stays readable");
        if event.event == suru::protocol::SESSION_CATALOG_SNAPSHOT_EVENT {
            break serde_json::from_str::<suru::protocol::SessionCatalogSnapshot>(&event.data)
                .expect("decode the catalog snapshot");
        }
    };
    let updates = events.filter_map(|event| async move {
        let event = event.expect("the catalog stream stays open");
        (event.event == suru::protocol::SESSION_CATALOG_UPDATED_EVENT).then(|| {
            serde_json::from_str::<suru::protocol::SessionCatalogUpdate>(&event.data)
                .expect("decode a catalog update")
        })
    });
    (snapshot.session_ids, Box::pin(updates))
}

/// Checks whether `haystack` (typically `git worktree list --porcelain`
/// output) mentions `path`, comparing with path separators normalized on
/// both sides. On Windows, Git always spells absolute paths with forward
/// slashes in its own output, even when the path passed on the command line
/// used backslashes — a plain substring check against a `Path`'s native
/// rendering silently never matches there. Normalizing both sides keeps the
/// comparison meaningful on every platform instead of hardcoding one slash
/// flavour.
pub fn git_output_mentions_path(haystack: &str, path: &std::path::Path) -> bool {
    let needle = path.to_str().unwrap().replace('\\', "/");
    haystack.replace('\\', "/").contains(&needle)
}

pub async fn receive_managed_client_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::Connecting)
    ));
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::Connected(_))
    ));
    assert!(matches!(
        client.next().await,
        Some(suru::managed_client::ManagedEvent::SettingsSnapshot(_))
    ));
    crate::server_support::receive_model_catalog(client).await;
}

/// A terminal client reading one Session as a person at it would: a TUI
/// following the Session through a subscription of its own, whose Prompts go
/// to the Server like any client's. Whatever it hands its reader back — a
/// withdrawn Prompt in its composer — is what that reader would send next.
pub struct Reader {
    application: suru::tui::Application,
    subscription: suru::managed_client::SessionSubscription,
    revision: SessionRevision,
    _client: ManagedClient,
    _launched_in: tempfile::TempDir,
}

impl Reader {
    /// Opens `session_id` on the Server running under `state_dir` and
    /// `channel`, as it stands now.
    pub async fn open(state_dir: &std::path::Path, channel: &str, session_id: SessionId) -> Self {
        let mut client = ManagedClient::connect(
            suru::managed_client::ManagedClientConfig::new(state_dir, channel)
                .expect("configure managed client"),
        )
        .await
        .expect("connect managed client");
        receive_managed_client_initial_state(&mut client).await;
        let mut subscription = client
            .subscribe_session(session_id)
            .await
            .expect("subscribe to the Session");
        let SessionEvent::Snapshot(snapshot) = timeout(PROGRESS_DEADLINE, subscription.next())
            .await
            .expect("the Session's snapshot arrives")
            .expect("the Session stream stays open")
            .expect("the snapshot is valid")
        else {
            panic!("a subscription opens on a snapshot");
        };
        let launched_in = tempfile::tempdir().expect("create a launch directory");
        let mut application = suru::tui::Application::new(launched_in.path(), Default::default());
        let revision = snapshot.revision;
        application
            .handle_event(suru::tui::ApplicationEvent::SessionAttached(*snapshot))
            .expect("open the Session");
        Self {
            application,
            subscription,
            revision,
            _client: client,
            _launched_in: launched_in,
        }
    }

    /// Hands the reader every update the Session has made up to `revision`.
    pub async fn catch_up(&mut self, revision: SessionRevision) {
        while self.revision < revision {
            let update = next_session_update(&mut self.subscription).await;
            self.revision = update.revision;
            self.application
                .handle_event(suru::tui::ApplicationEvent::Session(SessionEvent::Updated(
                    update,
                )))
                .expect("follow the Session");
        }
    }

    /// Writes `text` into the composer and sends it, answering the admission
    /// the reader asked for.
    pub fn write_and_send(&mut self, text: &str) -> suru::protocol::AdmitPromptRequest {
        self.application
            .handle_event(suru::tui::ApplicationEvent::Command(
                suru::tui::CommandId::InsertText(text.to_owned()),
            ))
            .expect("write a Prompt");
        self.send().expect("a written Prompt is sent")
    }

    /// Sends whatever the composer holds, answering the admission the reader
    /// asked for, if it asked for one.
    pub fn send(&mut self) -> Option<suru::protocol::AdmitPromptRequest> {
        match self
            .application
            .handle_event(suru::tui::ApplicationEvent::Command(
                suru::tui::CommandId::SubmitSteer,
            ))
            .expect("send the composer")
        {
            suru::tui::ApplicationTransition::AdmitPrompt { request, .. } => Some(request),
            _ => None,
        }
    }

    /// Asks the Server to admit `request` as the reader's client would, and
    /// tells the reader the Server answered, answering how.
    pub async fn admit(
        &mut self,
        descriptor: &RuntimeDescriptor,
        session_id: SessionId,
        request: &suru::protocol::AdmitPromptRequest,
    ) -> reqwest::StatusCode {
        let status = reqwest::Client::new()
            .post(format!(
                "{}/v1/sessions/{session_id}/prompts",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .json(request)
            .send()
            .await
            .expect("admit the Prompt")
            .status();
        assert!(status.is_success(), "the Server admits it: {status}");
        self.application
            .handle_event(suru::tui::ApplicationEvent::PromptAdmissionSucceeded {
                session: suru::protocol::SessionReference::new(
                    suru::protocol::Outlook::Local,
                    session_id,
                ),
                prompt_id: request.prompt.id,
            })
            .expect("hear the Server's answer");
        status
    }
}
