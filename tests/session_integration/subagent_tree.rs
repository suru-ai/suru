//! The per-tree subscription: the tree a top-level Session heads, read off
//! the Provider-neutral Subagent rows, as a snapshot and then live changes —
//! over the Server's own route and through the managed client.

use std::pin::Pin;

use crate::{
    provider_support::{ControlledProvider, ControlledProviderSession},
    server_support::PROGRESS_DEADLINE,
    support::{
        WorkingTurn, create_session, hosted_model, hosted_selection, read_session,
        read_session_until, receive_managed_client_initial_state, working_turn,
        working_turn_with_timings,
    },
};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SubagentTreeEvent},
    protocol::{
        ActivityStatus, AdmitPromptRequest, Approval, ApprovalId, ApprovalSubject,
        CreateSessionRequest, Decision, InitialPrompt, Outlook, PromptDelivery, PromptId, Question,
        Questionnaire, QuestionnaireId, RuntimeDescriptor, SUBAGENT_TREE_SNAPSHOT_EVENT,
        SUBAGENT_TREE_UPDATED_EVENT, SessionError, SessionErrorCode, SessionId, SubagentTreeChange,
        SubagentTreeEntry, SubagentTreeSnapshot, SubagentTreeUpdate,
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
                delegation: None,
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

async fn resume(
    provider: &ControlledProviderSession,
    delegator: Option<&str>,
    subagent: &str,
    description: &str,
) {
    provider
        .emit_attributed_and_wait_until_observed(
            delegator.map_or(ProviderEventAttribution::OwningSession, |delegator| {
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(delegator))
            }),
            ProviderEvent::SubagentResumed {
                subagent_id: ProviderSubagentId::new(subagent),
                description: description.to_owned(),
                delegation: None,
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

/// Settles every Subagent [`spawn_a_family`] spawned, deepest first, then the
/// Turn that spawned them, and waits until nothing in the tree is Working — the
/// state a top-level Session must be in to be deleted, and the one a restart
/// finds nothing to repair in.
async fn settle_the_family(fixture: &WorkingTurn) {
    let provider = &fixture.provider_session;
    settle(
        provider,
        Some("task-3"),
        "task-5",
        ProviderSubagentStatus::Completed,
    )
    .await;
    settle(
        provider,
        Some("task-1"),
        "task-3",
        ProviderSubagentStatus::Completed,
    )
    .await;
    settle(
        provider,
        Some("task-1"),
        "task-4",
        ProviderSubagentStatus::Failed,
    )
    .await;
    settle(provider, None, "task-1", ProviderSubagentStatus::Completed).await;
    settle(
        provider,
        None,
        "task-2",
        ProviderSubagentStatus::Interrupted,
    )
    .await;
    provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the whole tree stops Working",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
}

#[tokio::test]
async fn deleting_the_top_level_session_invalidates_its_tree() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-tree-deletion-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    spawn_a_family(&fixture).await;
    settle_the_family(&fixture).await;
    let descriptor = fixture.server.descriptor();
    let (tree, _updates) = open_tree(descriptor, fixture.session_id).await;
    let deepest = named(&tree.subagents, "Probe").session_id;
    let (through_child, mut updates) = open_tree(descriptor, deepest).await;
    let mut revision = through_child.revision;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(20)),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client.subscribe_subagent_tree(deepest);
    assert!(matches!(
        next_tree_event(&mut subscription).await,
        SubagentTreeEvent::Snapshot(_)
    ));

    fixture
        .client
        .delete(format!(
            "{}/v1/sessions/{}",
            descriptor.base_url, fixture.session_id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("delete the top-level Session")
        .error_for_status()
        .expect("an idle top-level Session is deleted");

    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        SubagentTreeChange::TreeDeleted,
        "a subscriber is told the tree is gone, whichever Session it subscribed through"
    );
    assert!(
        timeout(PROGRESS_DEADLINE, updates.next())
            .await
            .expect("the stream ends after the deletion")
            .is_none(),
        "the deletion is the stream's last word"
    );
    assert_eq!(
        next_tree_event(&mut subscription).await,
        SubagentTreeEvent::Deleted,
        "the managed client surfaces the deletion as the subscription's end"
    );
    assert!(
        timeout(PROGRESS_DEADLINE, subscription.next())
            .await
            .expect("the deleted subscription ends")
            .is_none(),
        "rather than reconnecting to a tree that is no longer there"
    );
    let reopened = reqwest::Client::new()
        .get(tree_url(descriptor, deepest))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("ask for the deleted tree again");
    assert_eq!(
        reopened.status(),
        reqwest::StatusCode::NOT_FOUND,
        "no Session in a deleted tree answers for it"
    );

    drop(subscription);
    drop(client);
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restart_restores_the_tree_and_its_entries() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-tree-restart-test";
    let config = ServerConfig::new(state_dir.path(), channel).expect("configure server");
    let fixture = working_turn(state_dir.path(), channel).await;
    spawn_a_family(&fixture).await;
    settle_the_family(&fixture).await;
    let (before, before_updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;
    let deepest = named(&before.subagents, "Probe").session_id;
    drop(before_updates);
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("respawn server");
    // Asked through the deepest Subagent first, before anything else has
    // read the tree back from storage.
    let (after, _updates) = open_tree(restarted.descriptor(), deepest).await;

    assert_eq!(after.top_level, before.top_level);
    assert_eq!(
        after.subagents, before.subagents,
        "every entry comes back where it stood, as it last said it"
    );
    assert_eq!(
        shape(&after),
        owned(&[
            ("Explore", "top-level", 0),
            ("Review", "Explore", 0),
            ("Probe", "Review", 0),
            ("Test", "Explore", 1),
            ("Plan", "top-level", 1),
        ])
    );
    assert_eq!(
        named(&after.subagents, "Test").status,
        ActivityStatus::Failed
    );
    assert_eq!(
        named(&after.subagents, "Plan").status,
        ActivityStatus::Interrupted
    );
    assert!(
        after
            .subagents
            .iter()
            .all(|entry| entry.duration_ms.is_some()),
        "settled durations are restored with their entries"
    );

    restarted
        .shutdown()
        .await
        .expect("shut down restarted server");
}

/// Every change up to and including the first that `wanted` picks out, each
/// checked to follow the one before it without a gap.
async fn changes_until(
    updates: &mut TreeUpdates,
    revision: &mut suru::protocol::SubagentTreeRevision,
    wanted: impl Fn(&SubagentTreeChange) -> bool,
) -> Vec<SubagentTreeChange> {
    let mut changes = Vec::new();
    loop {
        let change = next_change(updates, revision).await;
        let found = wanted(&change);
        changes.push(change);
        if found {
            return changes;
        }
    }
}

fn is_top_level_working_change(change: &SubagentTreeChange) -> bool {
    matches!(change, SubagentTreeChange::TopLevelWorkingChanged { .. })
}

#[tokio::test]
async fn entries_say_when_their_work_began_and_the_top_level_since_when_it_works() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-timing-test").await;
    spawn_a_family(&fixture).await;
    let descriptor = fixture.server.descriptor();

    let (tree, _updates) = open_tree(descriptor, fixture.session_id).await;

    let working_since = tree
        .top_level
        .working_since
        .expect("a top-level Session in the middle of its Turn is Working");
    assert_eq!(
        Some(working_since),
        read_session(descriptor, fixture.session_id)
            .await
            .working_since(),
        "the top-level entry's Working reads what its Sidebar row reads"
    );
    for entry in &tree.subagents {
        let started_at = entry
            .started_at
            .unwrap_or_else(|| panic!("{} says when its work began", entry.name));
        assert_eq!(
            Some(started_at),
            read_session(descriptor, entry.session_id).await.turns[0].started_at,
            "{} began its work when its own Session's Turn began, at its spawn",
            entry.name
        );
        assert!(started_at >= working_since);
        assert!(!entry.needs_intervention);
    }
    let started = |name| named(&tree.subagents, name).started_at;
    assert!(
        started("Explore") <= started("Review") && started("Review") <= started("Probe"),
        "a Subagent begins no earlier than the Subagent that spawned it"
    );
    assert!(!tree.top_level.needs_intervention);

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_top_level_working_changes_arrive_as_its_work_stops_and_starts_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut fixture = working_turn(state_dir.path(), "subagent-tree-working-test").await;
    spawn(
        &fixture.provider_session,
        None,
        "task-1",
        "Explore",
        "Map the provider seams",
    )
    .await;
    let descriptor = fixture.server.descriptor().clone();
    let (tree, mut updates) = open_tree(&descriptor, fixture.session_id).await;
    let mut revision = tree.revision;
    assert!(tree.top_level.working_since.is_some());

    settle(
        &fixture.provider_session,
        None,
        "task-1",
        ProviderSubagentStatus::Completed,
    )
    .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let stopped = changes_until(&mut updates, &mut revision, is_top_level_working_change).await;
    assert_eq!(
        stopped.last(),
        Some(&SubagentTreeChange::TopLevelWorkingChanged {
            working_since: None
        }),
        "the tree says when its top-level Session stops Working"
    );
    assert!(
        !stopped[..stopped.len() - 1]
            .iter()
            .any(is_top_level_working_change),
        "and only when it stops: {stopped:?}"
    );

    fixture
        .client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, fixture.session_id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Map the rest".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit another Prompt")
        .error_for_status()
        .expect("the idle Session admits it");
    let resumed = changes_until(&mut updates, &mut revision, is_top_level_working_change).await;
    let Some(SubagentTreeChange::TopLevelWorkingChanged {
        working_since: Some(resumed_since),
    }) = resumed.last()
    else {
        panic!("a Prompt admitted sets the top-level Session Working again: {resumed:?}");
    };
    assert_eq!(
        Some(*resumed_since),
        read_session(&descriptor, fixture.session_id)
            .await
            .working_since()
    );
    timeout(PROGRESS_DEADLINE, fixture.provider_session.next_turn())
        .await
        .expect("the next Turn reaches the Provider")
        .succeed();

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

fn approval() -> Approval {
    Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Command {
            command: "cargo nextest run".into(),
            cwd: None,
            actions: Vec::new(),
        },
        reason: Some("Run the project tests".into()),
    }
}

fn questionnaire() -> Questionnaire {
    Questionnaire {
        id: QuestionnaireId::new(),
        questions: vec![Question {
            id: "scope".into(),
            title: None,
            text: "Which scope?".into(),
            choices: Vec::new(),
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        }],
    }
}

/// Each entry's own-Intervention flag, top-level first, as (name, flag).
fn interventions(snapshot: &SubagentTreeSnapshot) -> Vec<(String, bool)> {
    std::iter::once((
        "top-level".to_owned(),
        snapshot.top_level.needs_intervention,
    ))
    .chain(
        snapshot
            .subagents
            .iter()
            .map(|entry| (entry.name.clone(), entry.needs_intervention)),
    )
    .collect()
}

fn flags(expected: &[(&str, bool)]) -> Vec<(String, bool)> {
    expected
        .iter()
        .map(|(name, flag)| ((*name).to_owned(), *flag))
        .collect()
}

async fn next_managed_change(
    subscription: &mut suru::managed_client::SubagentTreeSubscription,
) -> SubagentTreeChange {
    loop {
        match next_tree_event(subscription).await {
            SubagentTreeEvent::Changed(change) => return change,
            SubagentTreeEvent::Snapshot(_) => {}
            other => panic!("the subscription stays live: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_intervention_flags_only_the_session_that_owns_it_until_it_is_answered() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "subagent-tree-intervention-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let provider = &fixture.provider_session;
    spawn(provider, None, "task-1", "Explore", "Map the seams").await;
    spawn(
        provider,
        Some("task-1"),
        "task-2",
        "Review",
        "Check the seams",
    )
    .await;
    let descriptor = fixture.server.descriptor();
    let (tree, mut updates) = open_tree(descriptor, fixture.session_id).await;
    let mut revision = tree.revision;
    let review = named(&tree.subagents, "Review").session_id;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure managed client")
            .with_recovery_backoff(Duration::from_millis(5), Duration::from_millis(20)),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client.subscribe_subagent_tree(fixture.session_id);
    let SubagentTreeEvent::Snapshot(managed) = next_tree_event(&mut subscription).await else {
        panic!("the subscription opens with the tree");
    };
    assert_eq!(
        managed.top_level.working_since, tree.top_level.working_since,
        "the managed client carries the top-level Working reading"
    );
    assert_eq!(
        managed.subagents, tree.subagents,
        "and each entry's start and Intervention flag"
    );

    let request = approval();
    provider
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(ProviderSubagentId::new("task-2")),
            ProviderEvent::ApprovalRequested {
                approval: request.clone(),
                tool_activity_id: None,
            },
        )
        .await;
    let raised = SubagentTreeChange::NeedsInterventionChanged {
        session_id: review,
        needs_intervention: true,
    };
    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        raised,
        "an Approval raised in the nested Subagent's Session flags that Session's entry"
    );
    assert_eq!(next_managed_change(&mut subscription).await, raised);
    let (flagged, _flagged_updates) = open_tree(descriptor, review).await;
    assert_eq!(
        interventions(&flagged),
        flags(&[("top-level", false), ("Explore", false), ("Review", true)]),
        "and neither its spawner nor the top-level Session repeats it"
    );

    client
        .submit_decision(review, request.id, Decision::Accept)
        .await
        .expect("answer the Approval");
    let answered = SubagentTreeChange::NeedsInterventionChanged {
        session_id: review,
        needs_intervention: false,
    };
    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        answered,
        "the flag clears once the Approval is answered"
    );
    assert_eq!(next_managed_change(&mut subscription).await, answered);

    let question = questionnaire();
    provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireRequested {
            questionnaire: question.clone(),
        })
        .await;
    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: fixture.session_id,
            needs_intervention: true,
        },
        "a Questionnaire in the top-level Session's own Transcript flags the top-level entry"
    );
    let (asked, _asked_updates) = open_tree(descriptor, review).await;
    assert_eq!(
        interventions(&asked),
        flags(&[("top-level", true), ("Explore", false), ("Review", false)]),
        "and only that entry"
    );
    provider
        .emit_and_wait_until_observed(ProviderEvent::QuestionnaireWithdrawn { id: question.id })
        .await;
    assert_eq!(
        next_change(&mut updates, &mut revision).await,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: fixture.session_id,
            needs_intervention: false,
        }
    );

    drop(subscription);
    drop(client);
    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn two_subscribers_to_one_tree_hear_the_same_change() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-two-subscribers-test").await;
    let provider = &fixture.provider_session;
    spawn(
        provider,
        None,
        "task-1",
        "Explore",
        "Map the provider seams",
    )
    .await;
    let (through_top, mut top_updates) =
        open_tree(fixture.server.descriptor(), fixture.session_id).await;
    let explore = named(&through_top.subagents, "Explore").session_id;
    // A second reader, as another Client with the Subagent open would be.
    let (through_child, mut child_updates) = open_tree(fixture.server.descriptor(), explore).await;
    assert_eq!(through_child.revision, through_top.revision);

    settle(provider, None, "task-1", ProviderSubagentStatus::Completed).await;
    let mut top_revision = through_top.revision;
    let mut child_revision = through_child.revision;
    let heard_at_top = next_change(&mut top_updates, &mut top_revision).await;
    let heard_at_child = next_change(&mut child_updates, &mut child_revision).await;
    assert!(
        matches!(
            &heard_at_top,
            SubagentTreeChange::SubagentSettled { session_id, status: ActivityStatus::Completed, .. }
                if *session_id == explore
        ),
        "{heard_at_top:?}"
    );
    assert_eq!(
        heard_at_child, heard_at_top,
        "every subscriber to the tree hears the same change"
    );
    assert_eq!(child_revision, top_revision, "under the same revision");

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_resumed_subagent_is_one_entry_standing_where_it_first_spawned() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "subagent-tree-resume-test").await;
    let provider = &fixture.provider_session;
    spawn(provider, None, "task-1", "Review", "Check the seams").await;
    spawn(
        provider,
        None,
        "task-2",
        "Explore",
        "Map the provider seams",
    )
    .await;
    settle(provider, None, "task-2", ProviderSubagentStatus::Completed).await;
    let (before, mut updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;
    let mut revision = before.revision;
    let explore = named(&before.subagents, "Explore").clone();

    // The reviewer, which spawned before the explorer, resumes it; then the
    // explorer settles and the top-level Session resumes it again. Each
    // resume adds a row leading into the explorer's one Session.
    resume(provider, Some("task-1"), "task-2", "Map the tests too").await;
    settle(provider, None, "task-2", ProviderSubagentStatus::Completed).await;
    resume(provider, None, "task-2", "Map the fixtures too").await;
    let reviewer = read_session(
        fixture.server.descriptor(),
        named(&before.subagents, "Review").session_id,
    )
    .await;
    assert!(
        reviewer.activities.iter().any(|activity| matches!(
            activity,
            suru::protocol::Activity::Subagent { session_id, .. } if *session_id == explore.session_id
        )),
        "the reviewer's Transcript holds the row of the resume it sent"
    );
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the top-level Session holds the second resume's row",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .filter(|activity| {
                    matches!(
                        activity,
                        suru::protocol::Activity::Subagent { session_id, .. }
                            if *session_id == explore.session_id
                    )
                })
                .count()
                == 2
        },
    )
    .await;
    spawn(provider, None, "task-3", "Plan", "Weigh the options").await;

    let SubagentTreeChange::SubagentSpawned { entry } =
        next_change(&mut updates, &mut revision).await
    else {
        panic!("the resumes move nothing in the tree, so the next change is the new spawn");
    };
    assert_eq!(entry.name, "Plan");
    assert_eq!(
        entry.spawn_order, 2,
        "the resume rows take no place among the top-level Session's spawns"
    );

    let (after, _updates) = open_tree(fixture.server.descriptor(), fixture.session_id).await;
    assert_eq!(
        shape(&after),
        owned(&[
            ("Review", "top-level", 0),
            ("Explore", "top-level", 1),
            ("Plan", "top-level", 2),
        ]),
        "the explorer is listed once, where it first spawned, and not again under the reviewer"
    );
    assert_eq!(
        named(&after.subagents, "Explore"),
        &explore,
        "the entry still reads as the spawn left it"
    );

    drop(fixture.provider_session);
    fixture.server.shutdown().await.expect("shut down server");
}
