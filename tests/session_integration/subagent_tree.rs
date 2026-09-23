//! The per-tree subscription: the tree a top-level Session heads, read off
//! the Provider-neutral Subagent rows, as a snapshot and then live changes —
//! over the Server's own route and through the managed client.

use std::pin::Pin;

use crate::{
    provider_support::{ControlledProvider, ControlledProviderSession},
    server_support::PROGRESS_DEADLINE,
    support::{
        WorkingTurn, create_session, hosted_model, hosted_selection,
        receive_managed_client_initial_state, working_turn, working_turn_with_timings,
    },
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SubagentTreeEvent},
    protocol::{
        ActivityStatus, CreateSessionRequest, InitialPrompt, Outlook, PromptId, RuntimeDescriptor,
        SUBAGENT_TREE_SNAPSHOT_EVENT, SUBAGENT_TREE_UPDATED_EVENT, SessionError, SessionErrorCode,
        SessionId, SubagentTreeChange, SubagentTreeEntry, SubagentTreeSnapshot, SubagentTreeUpdate,
    },
    provider::{
        ProviderEvent, ProviderEventAttribution, ProviderSubagentId, ProviderSubagentStatus,
    },
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

type TreeUpdates = Pin<Box<dyn Stream<Item = SubagentTreeUpdate> + Send>>;

fn tree_url(descriptor: &RuntimeDescriptor, session_id: SessionId) -> String {
    format!(
        "{}/v1/sessions/{session_id}/subagent-tree",
        descriptor.base_url
    )
}

/// Opens the per-tree stream through `session_id` and decodes its leading
/// snapshot, handing back the changes that follow it.
async fn open_tree(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> (SubagentTreeSnapshot, TreeUpdates) {
    let response = reqwest::Client::new()
        .get(tree_url(descriptor, session_id))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Subagent tree stream")
        .error_for_status()
        .expect("the Subagent tree stream opens");
    let mut events = Box::pin(response.bytes_stream().eventsource());
    let event = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("the tree snapshot arrives")
        .expect("the tree stream stays open")
        .expect("the tree stream stays readable");
    assert_eq!(
        event.event, SUBAGENT_TREE_SNAPSHOT_EVENT,
        "the stream opens with its snapshot"
    );
    let snapshot = serde_json::from_str::<SubagentTreeSnapshot>(&event.data)
        .expect("decode the tree snapshot");
    assert_eq!(event.id, snapshot.revision.0.to_string());
    let updates = events.map(|event| {
        let event = event.expect("the tree stream stays readable");
        assert_eq!(event.event, SUBAGENT_TREE_UPDATED_EVENT);
        let update =
            serde_json::from_str::<SubagentTreeUpdate>(&event.data).expect("decode a tree update");
        assert_eq!(event.id, update.revision.0.to_string());
        update
    });
    (snapshot, Box::pin(updates))
}

/// The next change, checked to follow the revision before it without a gap.
async fn next_change(
    updates: &mut TreeUpdates,
    revision: &mut suru::protocol::SubagentTreeRevision,
) -> SubagentTreeChange {
    let update = timeout(PROGRESS_DEADLINE, updates.next())
        .await
        .expect("a tree change arrives")
        .expect("the tree stream stays open");
    assert!(
        update.revision.immediately_follows(*revision),
        "tree changes arrive in unbroken revision order"
    );
    *revision = update.revision;
    update.change
}

async fn spawn(
    provider: &ControlledProviderSession,
    spawner: Option<&str>,
    subagent: &str,
    name: &str,
    description: &str,
) {
    provider
        .emit_attributed_and_wait_until_observed(
            spawner.map_or(ProviderEventAttribution::OwningSession, |spawner| {
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(spawner))
            }),
            ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new(subagent),
                name: name.to_owned(),
                description: description.to_owned(),
            },
        )
        .await;
}

async fn settle(
    provider: &ControlledProviderSession,
    spawner: Option<&str>,
    subagent: &str,
    status: ProviderSubagentStatus,
) {
    provider
        .emit_attributed_and_wait_until_observed(
            spawner.map_or(ProviderEventAttribution::OwningSession, |spawner| {
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(spawner))
            }),
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(subagent),
                status,
            },
        )
        .await;
}

/// The Subagent entry a tree lists under `name`.
fn named<'a>(subagents: &'a [SubagentTreeEntry], name: &str) -> &'a SubagentTreeEntry {
    subagents
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("the tree lists {name}"))
}

/// Each entry as (name, the name of its spawner or "top-level", spawn order),
/// in the order the tree lists them.
fn shape(snapshot: &SubagentTreeSnapshot) -> Vec<(String, String, u32)> {
    snapshot
        .subagents
        .iter()
        .map(|entry| {
            let parent = if entry.parent_session_id == snapshot.top_level.session_id {
                "top-level".to_owned()
            } else {
                snapshot
                    .subagents
                    .iter()
                    .find(|candidate| candidate.session_id == entry.parent_session_id)
                    .expect("every spawner is in the tree")
                    .name
                    .clone()
            };
            (entry.name.clone(), parent, entry.spawn_order)
        })
        .collect()
}

fn owned(shape: &[(&str, &str, u32)]) -> Vec<(String, String, u32)> {
    shape
        .iter()
        .map(|(name, parent, order)| ((*name).to_owned(), (*parent).to_owned(), *order))
        .collect()
}

/// A top-level Session with two Subagents of its own, the first of which
/// spawned two more, the first of those one more again.
async fn spawn_a_family(fixture: &WorkingTurn) {
    let provider = &fixture.provider_session;
    spawn(
        provider,
        None,
        "task-1",
        "Explore",
        "Map the provider seams",
    )
    .await;
    spawn(provider, None, "task-2", "Plan", "Weigh the seam options").await;
    spawn(
        provider,
        Some("task-1"),
        "task-3",
        "Review",
        "Check the mapped seams",
    )
    .await;
    spawn(provider, Some("task-1"), "task-4", "Test", "Run the suite").await;
    spawn(
        provider,
        Some("task-3"),
        "task-5",
        "Probe",
        "Probe one seam",
    )
    .await;
}

#[tokio::test]
async fn the_snapshot_lists_the_tree_depth_first_in_spawn_order() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-snapshot-test").await;
    spawn_a_family(&fixture).await;

    let (snapshot, _updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;

    assert_eq!(snapshot.top_level.session_id, fixture.session_id);
    assert_eq!(
        snapshot.top_level.title, "Delegate the mapping",
        "the top-level entry carries its Session's Title"
    );
    assert_eq!(
        shape(&snapshot),
        owned(&[
            ("Explore", "top-level", 0),
            ("Review", "Explore", 0),
            ("Probe", "Review", 0),
            ("Test", "Explore", 1),
            ("Plan", "top-level", 1),
        ]),
        "each Subagent follows the Session that spawned it, siblings in the order they spawned"
    );
    let explore = named(&snapshot.subagents, "Explore");
    assert_eq!(explore.title, "Map the provider seams");
    assert_eq!(explore.status, ActivityStatus::Active);
    assert_eq!(
        explore.duration_ms, None,
        "a working Subagent has no duration"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subscription_through_a_nested_child_resolves_to_the_top_level_tree() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-nested-test").await;
    spawn_a_family(&fixture).await;
    let (through_top_level, _top_level_updates) =
        open_tree(fixture.server.descriptor(), fixture.session_id).await;
    let deepest = named(&through_top_level.subagents, "Probe").session_id;
    let middle = named(&through_top_level.subagents, "Test").session_id;

    for session_id in [deepest, middle] {
        let (through_child, _updates) = open_tree(fixture.server.descriptor(), session_id).await;
        assert_eq!(
            through_child.top_level.session_id, fixture.session_id,
            "the snapshot names the top-level Session, whichever Session it was asked through"
        );
        assert_eq!(through_child.top_level, through_top_level.top_level);
        assert_eq!(
            through_child.subagents, through_top_level.subagents,
            "every Session in the tree answers with the same tree"
        );
    }

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn spawns_settles_and_updates_arrive_as_changes_and_settled_entries_keep_their_places() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-changes-test").await;
    let provider = &fixture.provider_session;
    let (snapshot, mut updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        snapshot.subagents.is_empty(),
        "a Session with no Subagents is a tree of its own entry alone"
    );
    let mut revision = snapshot.revision;

    spawn(
        provider,
        None,
        "task-1",
        "Explore",
        "Map the provider seams",
    )
    .await;
    let SubagentTreeChange::SubagentSpawned { entry: explore } =
        next_change(&mut updates, &mut revision).await
    else {
        panic!("a spawn arrives as a spawned entry");
    };
    assert_eq!(explore.parent_session_id, fixture.session_id);
    assert_eq!(explore.spawn_order, 0);
    assert_eq!(explore.name, "Explore");
    assert_eq!(explore.title, "Map the provider seams");
    assert_eq!(explore.status, ActivityStatus::Active);
    assert_eq!(explore.duration_ms, None);

    spawn(
        provider,
        Some("task-1"),
        "task-2",
        "Review",
        "Check the seams",
    )
    .await;
    let SubagentTreeChange::SubagentSpawned { entry: review } =
        next_change(&mut updates, &mut revision).await
    else {
        panic!("a nested spawn arrives as a spawned entry");
    };
    assert_eq!(
        review.parent_session_id, explore.session_id,
        "a spawn attributed to a Subagent lands beneath that Subagent"
    );
    assert_eq!(review.spawn_order, 0);

    spawn(provider, None, "task-3", "Plan", "Weigh the options").await;
    let SubagentTreeChange::SubagentSpawned { entry: plan } =
        next_change(&mut updates, &mut revision).await
    else {
        panic!("a second spawn arrives as a spawned entry");
    };
    assert_eq!(plan.parent_session_id, fixture.session_id);
    assert_eq!(
        plan.spawn_order, 1,
        "a later sibling spawns after the first"
    );

    provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentUpdated {
            subagent_id: ProviderSubagentId::new("task-1"),
            description: "Reading the orchestration actor".to_owned(),
        })
        .await;
    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        SubagentTreeChange::SubagentRetitled {
            session_id: explore.session_id,
            name: "Explore".to_owned(),
            title: "Reading the orchestration actor".to_owned(),
        }
    );

    settle(
        provider,
        Some("task-1"),
        "task-2",
        ProviderSubagentStatus::Completed,
    )
    .await;
    let SubagentTreeChange::SubagentSettled {
        session_id,
        status,
        duration_ms,
    } = next_change(&mut updates, &mut revision).await
    else {
        panic!("a nested settle arrives as a settled entry");
    };
    assert_eq!(session_id, review.session_id);
    assert_eq!(status, ActivityStatus::Completed);
    assert!(
        duration_ms.is_some(),
        "a settle the Provider reported carries how long the Subagent worked"
    );

    settle(provider, None, "task-1", ProviderSubagentStatus::Failed).await;
    let SubagentTreeChange::SubagentSettled {
        session_id,
        status,
        duration_ms,
    } = next_change(&mut updates, &mut revision).await
    else {
        panic!("a failed settle arrives as a settled entry");
    };
    assert_eq!(session_id, explore.session_id);
    assert_eq!(status, ActivityStatus::Failed);
    assert!(duration_ms.is_some());

    let (settled, _updates) = open_tree(fixture.server.descriptor(), plan.session_id).await;
    assert_eq!(
        shape(&settled),
        owned(&[
            ("Explore", "top-level", 0),
            ("Review", "Explore", 0),
            ("Plan", "top-level", 1),
        ]),
        "settled Subagents keep their places"
    );
    assert_eq!(
        named(&settled.subagents, "Explore").status,
        ActivityStatus::Failed
    );
    assert_eq!(
        named(&settled.subagents, "Review").status,
        ActivityStatus::Completed
    );
    assert_eq!(
        named(&settled.subagents, "Plan").status,
        ActivityStatus::Active
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_derived_title_retitles_the_top_level_entry() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::with_provider(
        suru::protocol::ProviderId::new("controlled"),
        vec![hosted_model("controlled", "controlled-default")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "subagent-tree-title-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let created = create_session(
        server.descriptor(),
        &CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(hosted_selection("controlled", "controlled-default")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "the aside needs a tree".to_owned(),
                skill_invocations: Vec::new(),
            },
        },
    )
    .await;
    let (snapshot, mut updates) = open_tree(server.descriptor(), created.session.id).await;
    assert_eq!(snapshot.top_level.title, "the aside needs a tree");
    let mut revision = snapshot.revision;

    timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Title Errand reaches the Provider")
        .succeed(json!({ "title": "Build the Aside tree", "icon": "md-bug" }));

    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        SubagentTreeChange::TopLevelRetitled {
            title: "Build the Aside tree".to_owned(),
        }
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_tree_is_not_found_through_a_session_the_server_does_not_hold() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-missing-test").await;
    let descriptor = fixture.server.descriptor();

    let response = reqwest::Client::new()
        .get(tree_url(descriptor, SessionId::new()))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("ask for an unknown tree");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the refusal")
            .code,
        SessionErrorCode::SessionNotFound
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_tree_stream_keeps_alive_at_the_servers_interval() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn_with_timings(
        state_dir.path(),
        "subagent-tree-keepalive-test",
        ServerTimings {
            sse_keepalive_interval: Duration::from_millis(5),
            ..ServerTimings::default()
        },
    )
    .await;
    let descriptor = fixture.server.descriptor();
    let response = reqwest::Client::new()
        .get(tree_url(descriptor, fixture.session_id))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open the Subagent tree stream")
        .error_for_status()
        .expect("the Subagent tree stream opens");
    let mut body = response.bytes_stream();
    let mut received = Vec::new();
    timeout(PROGRESS_DEADLINE, async {
        while !String::from_utf8_lossy(&received).contains(": keep-alive") {
            let chunk = body
                .next()
                .await
                .expect("the tree stream stays open")
                .expect("the tree stream stays readable");
            received.extend_from_slice(&chunk);
        }
    })
    .await
    .expect("a keepalive follows the snapshot");

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

async fn next_tree_event(
    subscription: &mut suru::managed_client::SubagentTreeSubscription,
) -> SubagentTreeEvent {
    timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("a tree event arrives")
        .expect("the tree subscription stays open")
}

#[tokio::test]
async fn the_managed_client_surfaces_the_tree_and_its_changes() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-tree-managed-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let provider = &fixture.provider_session;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(20)),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    spawn(
        provider,
        None,
        "task-1",
        "Explore",
        "Map the provider seams",
    )
    .await;
    spawn(
        provider,
        Some("task-1"),
        "task-2",
        "Review",
        "Check the seams",
    )
    .await;
    let (known, _updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;
    let review = named(&known.subagents, "Review").clone();

    let mut subscription = client
        .outlook(Outlook::Local)
        .subscribe_subagent_tree(review.session_id);
    let SubagentTreeEvent::Snapshot(snapshot) = next_tree_event(&mut subscription).await else {
        panic!("the subscription opens with the tree");
    };
    assert_eq!(snapshot.top_level.session_id, fixture.session_id);
    assert_eq!(
        shape(&snapshot),
        owned(&[("Explore", "top-level", 0), ("Review", "Explore", 0)])
    );

    settle(
        provider,
        Some("task-1"),
        "task-2",
        ProviderSubagentStatus::Completed,
    )
    .await;
    let SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSettled {
        session_id,
        status,
        duration_ms,
    }) = next_tree_event(&mut subscription).await
    else {
        panic!("a settle reaches the managed client as a change");
    };
    assert_eq!(session_id, review.session_id);
    assert_eq!(status, ActivityStatus::Completed);
    assert!(duration_ms.is_some());

    spawn(provider, None, "task-3", "Plan", "Weigh the options").await;
    let SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSpawned { entry }) =
        next_tree_event(&mut subscription).await
    else {
        panic!("a spawn reaches the managed client as a change");
    };
    assert_eq!(entry.name, "Plan");
    assert_eq!(entry.parent_session_id, fixture.session_id);
    assert_eq!(entry.spawn_order, 1);

    let mut missing = client.subscribe_subagent_tree(SessionId::new());
    assert!(
        matches!(
            next_tree_event(&mut missing).await,
            SubagentTreeEvent::Failed(_)
        ),
        "a tree the Server does not hold fails rather than retrying forever"
    );
    assert!(
        timeout(PROGRESS_DEADLINE, missing.next())
            .await
            .expect("the failed subscription ends")
            .is_none()
    );

    drop(subscription);
    drop(client);
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}
