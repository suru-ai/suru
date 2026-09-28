//! The caps the Broker's Settings put on what it spawns: how many Sessions
//! deep a tree may stand, its top-level Session counting as the first
//! (`broker.maxDepth`), and how many brokered Subagents may work at once
//! anywhere beneath that top-level Session (`broker.maxConcurrentSubagents`).
//! A spawn past either is refused in words naming the cap and its Setting, and
//! never queued: nothing of it is created, and nothing of it starts once room
//! is made. Native Subagents are not counted, because Suru cannot refuse them.

use super::*;

/// What `spawn_subagent` tells an Agent refused for the concurrency cap,
/// while `working` brokered Subagents work beneath its top-level Session.
fn too_many_working(cap: u32, working: u32) -> String {
    format!(
        "Suru's Broker lets at most {cap} brokered Subagents work at once beneath a top-level \
         Session (`broker.maxConcurrentSubagents`), and {working} are working now, so nothing was \
         spawned. Call wait_subagents to wait for one to settle, or ask the user to raise the \
         Setting."
    )
}

/// What `spawn_subagent` tells an Agent refused for the depth cap, whose
/// Subagent would have stood `depth` Sessions deep.
fn too_deep(cap: u32, depth: u32) -> String {
    format!(
        "Suru's Broker lets Subagents stand at most {cap} Sessions deep, counting the top-level \
         Session as the first (`broker.maxDepth`), and one spawned here would stand {depth} deep, \
         so nothing was spawned. Do this work yourself, or ask the user to raise the Setting."
    )
}

/// A brokered Subagent's own Provider, started on `provider`'s double, with
/// its first Turn taken up, and the MCP client its Agent is — holding the
/// Broker token its own start carried — for a Subagent that delegates in turn.
async fn handed_child(
    provider: &mut ControlledProvider,
    selection: AgentSelection,
) -> (ControlledProviderSession, McpClient) {
    let start = next_start(provider).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut child = start.succeed(AgentIdentity {
        agent: AgentId::new(format!("{}-agent", selection.provider)),
        selection,
    });
    timeout(PROGRESS_DEADLINE, child.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    let mut client = McpClient::handed(&handoff);
    client.initialize().await;
    (child, client)
}

/// `spawn_subagent`'s arguments for a Subagent named Scout on Claude's Haiku.
fn scout() -> Value {
    json!({
        "provider": "claude",
        "model": "haiku",
        "name": "Scout",
        "description": "Chase one seam",
        "prompt": DELEGATION,
    })
}

/// The Agent Selection a Claude Subagent on Haiku runs under.
fn haiku() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("haiku"),
        options: Vec::new(),
    }
}

/// The brokered Subagents `session_id`'s Transcript holds rows for.
async fn brokered_rows(descriptor: &RuntimeDescriptor, session_id: SessionId) -> Vec<SessionId> {
    read_session(descriptor, session_id)
        .await
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subagent {
                session_id,
                brokered: true,
                ..
            } => Some(*session_id),
            _ => None,
        })
        .collect()
}

/// A Config Document pinning `document`, in a directory held as long as the
/// test holds it.
fn config_pinning(document: &str) -> tempfile::TempDir {
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), document).expect("write Config Document");
    config_dir
}

#[tokio::test]
async fn the_seventh_working_brokered_subagent_beneath_a_top_level_session_is_refused_naming_the_cap()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-limits-concurrency", None).await;
    let descriptor = delegating.descriptor.clone();

    // A native Subagent works beneath the top-level Session throughout: Suru
    // cannot refuse its Provider's own spawns, so it counts for nothing.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the seams".to_owned(),
            delegation: Some("Map every seam.".to_owned()),
        })
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the native Subagent's row opens",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Subagent {
                        brokered: false,
                        ..
                    }
                )
            })
        },
    )
    .await;

    // Six brokered Subagents working anywhere beneath the top-level Session:
    // one it spawned, one that Subagent spawned in its turn, and four more of
    // its own.
    let researcher_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (_researcher, mut researcher_client) =
        handed_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    researcher_client.spawn_subagent(scout()).await;
    let (_scout, _) = handed_child(&mut delegating.hosted.claude, haiku()).await;
    let mut working = Vec::new();
    for _ in 0..4 {
        working.push(spawn_working_child(&mut delegating).await);
    }

    assert_eq!(
        delegating
            .client
            .refusal("spawn_subagent", researcher("codex", "gpt-5.5", json!({})))
            .await,
        too_many_working(6, 6),
        "a seventh is refused naming the cap and the Setting that pins it"
    );
    assert_eq!(
        researcher_client.refusal("spawn_subagent", scout()).await,
        too_many_working(6, 6),
        "and so is one from a Subagent beneath it: the cap counts the whole tree"
    );

    assert_eq!(
        brokered_rows(&descriptor, delegating.caller).await.len(),
        5,
        "nothing of a refused spawn stands in the caller's Transcript"
    );
    assert_eq!(
        brokered_rows(&descriptor, researcher_id).await.len(),
        1,
        "nor in the Subagent's"
    );
    assert!(
        delegating.hosted.codex.try_next_start().is_none()
            && delegating.hosted.claude.try_next_start().is_none(),
        "and no Provider was asked to start one"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_settled_brokered_subagent_frees_its_slot_and_a_refused_spawn_is_never_queued() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_pinning(r#"{"broker": {"maxConcurrentSubagents": 2}}"#);
    let mut delegating = delegating(
        state_dir.path(),
        "broker-limits-settled",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = delegating.descriptor.clone();

    let (first_id, first) = spawn_working_child(&mut delegating).await;
    let (_second_id, _second) = spawn_working_child(&mut delegating).await;
    assert_eq!(
        delegating
            .client
            .refusal("spawn_subagent", researcher("codex", "gpt-5.5", json!({})))
            .await,
        too_many_working(2, 2),
        "the cap the Config Document pins is the one a spawn is refused at"
    );

    first
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the first Subagent's row settles with its Turn",
        |snapshot| row_status(snapshot, first_id).0 == ActivityStatus::Completed,
    )
    .await;
    assert_eq!(
        brokered_rows(&descriptor, delegating.caller).await.len(),
        2,
        "the refused spawn was not queued into the slot the settled Subagent freed"
    );
    assert!(
        delegating.hosted.codex.try_next_start().is_none(),
        "and no Provider was asked to start it"
    );

    let (third_id, _third) = spawn_working_child(&mut delegating).await;
    let read = delegating.client.read_subagent(third_id).await;
    assert_eq!(
        read["status"],
        json!("working"),
        "a settled Subagent frees its slot for the next spawn"
    );
    assert_eq!(brokered_rows(&descriptor, delegating.caller).await.len(), 3);
    assert_eq!(
        delegating
            .client
            .refusal("spawn_subagent", researcher("codex", "gpt-5.5", json!({})))
            .await,
        too_many_working(2, 2),
        "and the cap stands again once the two working fill it"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_spawn_that_would_stand_four_sessions_deep_is_refused_naming_the_cap() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-limits-depth", None).await;
    let descriptor = delegating.descriptor.clone();

    // The top-level Session is the first Session deep, the Subagent it
    // spawns the second, and that Subagent's own the third.
    delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (_researcher, mut researcher_client) =
        handed_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    let scout_id = researcher_client.spawn_subagent(scout()).await;
    let (_scout, mut scout_client) = handed_child(&mut delegating.hosted.claude, haiku()).await;

    assert_eq!(
        scout_client.refusal("spawn_subagent", scout()).await,
        too_deep(3, 4),
        "a Subagent three Sessions deep is refused a spawn of its own"
    );
    assert!(
        brokered_rows(&descriptor, scout_id).await.is_empty(),
        "nothing of the refused spawn stands in its Transcript"
    );
    assert!(
        delegating.hosted.codex.try_next_start().is_none()
            && delegating.hosted.claude.try_next_start().is_none(),
        "and no Provider was asked to start one"
    );
    assert_eq!(
        scout_client.list_providers().await["providers"][0]["id"],
        json!("claude"),
        "the Broker is still offered to it: only spawning deeper is refused"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_depth_limit_pinned_shallower_refuses_a_subagents_spawn_one_level_down() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = config_pinning(r#"{"broker": {"maxDepth": 2}}"#);
    let mut delegating = delegating(
        state_dir.path(),
        "broker-limits-pinned-depth",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = delegating.descriptor.clone();

    let researcher_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (_researcher, mut researcher_client) =
        handed_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    assert_eq!(
        researcher_client.refusal("spawn_subagent", scout()).await,
        too_deep(2, 3),
        "the depth the Config Document pins is the one a spawn is refused at"
    );
    assert!(brokered_rows(&descriptor, researcher_id).await.is_empty());

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
