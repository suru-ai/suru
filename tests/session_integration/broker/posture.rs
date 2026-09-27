//! A brokered Subagent's Approval Posture (ADR 0036): read from its spawner's,
//! verbatim on the spawner's Provider and through the fixed table between the
//! Providers' values on another, derived again whenever the spawner's changes
//! and never set on the Subagent itself.
//!
//! What a Subagent acts under is observed where its Provider receives it — the
//! start request and the posture updates its double records — and where a
//! reader finds it, on the Subagent's Session.

use std::path::Path;

use reqwest::StatusCode;
use serde_json::{Value, json};
use suru::protocol::{
    AgentId, AgentIdentity, ApprovalPosture, ApprovalPostureApplication, ClaudePermissionMode,
    CodexApprovalPolicy, CodexSandboxMode, RuntimeDescriptor, SessionApprovalPosture, SessionId,
    SettingMutation, UpdateApprovalPostureRequest,
};
use tokio::time::timeout;

use super::{
    DELEGATION, McpClient, claude_models, codex_selection, delegating, host_providers,
    mutate_setting, next_start, researcher,
};
use crate::{
    provider_support::{ControlledProvider, ControlledProviderSession},
    server_support::PROGRESS_DEADLINE,
    support::read_session,
};

fn claude(permission_mode: ClaudePermissionMode) -> ApprovalPosture {
    ApprovalPosture::Claude { permission_mode }
}

fn codex(approval_policy: CodexApprovalPolicy, sandbox_mode: CodexSandboxMode) -> ApprovalPosture {
    ApprovalPosture::Codex {
        approval_policy,
        sandbox_mode,
    }
}

/// Codex's level-4 value: nothing asks, everything is allowed.
fn codex_unrestricted() -> ApprovalPosture {
    codex(
        CodexApprovalPolicy::Never,
        CodexSandboxMode::DangerFullAccess,
    )
}

/// Codex's level-1 value: every edit or command that needs consent asks.
fn codex_asking() -> ApprovalPosture {
    codex(
        CodexApprovalPolicy::Untrusted,
        CodexSandboxMode::WorkspaceWrite,
    )
}

/// Codex's level-3 value: nothing asks, and nothing leaves the workspace.
fn codex_contained() -> ApprovalPosture {
    codex(CodexApprovalPolicy::Never, CodexSandboxMode::WorkspaceWrite)
}

/// A reading of `value` that has reached its Session's Provider.
fn applied(value: ApprovalPosture, pinned: bool) -> Option<SessionApprovalPosture> {
    Some(SessionApprovalPosture {
        value,
        pinned,
        application: ApprovalPostureApplication::Applied,
    })
}

/// What the Server answers a client setting `session_id`'s Approval Posture:
/// pinning `posture`, or with `None` resetting it to follow the Settings.
async fn set_posture(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    posture: Option<ApprovalPosture>,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/approval-posture",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&UpdateApprovalPostureRequest { posture })
        .send()
        .await
        .expect("reach the Server")
}

/// Pins `posture` on `session_id` as a client does, and answers with the
/// reading the Server gives back.
async fn pin(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    posture: ApprovalPosture,
) -> SessionApprovalPosture {
    let response = set_posture(descriptor, session_id, Some(posture)).await;
    assert_eq!(response.status(), StatusCode::OK, "the pin is accepted");
    response.json().await.expect("decode the Session's posture")
}

async fn posture_of(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> Option<SessionApprovalPosture> {
    read_session(descriptor, session_id)
        .await
        .session
        .approval_posture
}

/// The brokered Subagent's own Provider, started on `provider`'s double
/// having checked the posture its start request carries, with its first
/// Turn — the Delegation — taken up under that same posture.
async fn start_child(
    provider: &mut ControlledProvider,
    agent: &str,
    selection: suru::protocol::AgentSelection,
    posture: ApprovalPosture,
    described: &str,
) -> (ControlledProviderSession, suru::provider::BrokerHandoff) {
    let start = next_start(provider).await;
    assert_eq!(start.approval_posture(), Some(&posture), "{described}");
    let handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut child = start.succeed(AgentIdentity {
        agent: AgentId::new(agent),
        selection,
    });
    let turn = timeout(PROGRESS_DEADLINE, child.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider");
    assert_eq!(
        turn.approval_posture(),
        Some(&posture),
        "its first Turn runs under the same posture"
    );
    turn.succeed();
    (child, handoff)
}

/// The next posture `provider` is asked to act under, once it is running.
async fn next_posture(provider: &mut ControlledProviderSession) -> ApprovalPosture {
    timeout(PROGRESS_DEADLINE, provider.next_posture_update())
        .await
        .expect("the Subagent's Provider is told its new posture")
        .posture
}

/// `spawn_subagent`'s arguments for a Claude Subagent on Haiku.
fn claude_scout() -> Value {
    json!({
        "provider": "claude",
        "model": "haiku",
        "name": "Scout",
        "description": "Chase the Claude seam",
        "prompt": DELEGATION,
    })
}

fn haiku() -> suru::protocol::AgentSelection {
    claude_models()
        .into_iter()
        .find(|model| model.id.as_str() == "haiku")
        .expect("the Claude catalog carries Haiku")
        .default_agent_selection()
}

#[tokio::test]
async fn a_codex_subagent_of_a_claude_session_pinned_to_bypass_permissions_starts_never_asking_with_full_access_and_follows_the_pin_down_to_asking()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-posture-other-provider", None).await;
    let descriptor = delegating.descriptor.clone();
    pin(
        &descriptor,
        delegating.caller,
        claude(ClaudePermissionMode::BypassPermissions),
    )
    .await;

    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) = start_child(
        &mut delegating.hosted.codex,
        "codex-agent",
        codex_selection("high"),
        codex_unrestricted(),
        "the Subagent starts under Codex's value at bypassPermissions' level rather than \
         Codex's own Setting",
    )
    .await;
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_unrestricted(), true),
        "its Session reads that value, carrying its spawner's pin"
    );

    pin(
        &descriptor,
        delegating.caller,
        claude(ClaudePermissionMode::Default),
    )
    .await;
    assert_eq!(
        next_posture(&mut child_provider).await,
        codex_asking(),
        "moving the spawner down to asking moves the Subagent to Codex's asking value"
    );
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_asking(), true),
        "and its Session reads it, applied by the time the spawner's change is answered"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_on_its_spawners_provider_inherits_its_posture_verbatim_where_one_on_another_reads_the_twin()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-posture-same-provider", None).await;
    let descriptor = delegating.descriptor.clone();
    // Claude's auto has no twin elsewhere; the table reads it at level 2.
    let auto = claude(ClaudePermissionMode::Auto);
    pin(&descriptor, delegating.caller, auto).await;

    let claude_id = delegating.client.spawn_subagent(claude_scout()).await;
    start_child(
        &mut delegating.hosted.claude,
        "claude-agent",
        haiku(),
        auto,
        "a Claude Subagent of a Claude Session starts under auto itself, not acceptEdits",
    )
    .await;
    assert_eq!(
        posture_of(&descriptor, claude_id).await,
        applied(auto, true),
        "its Session reads its spawner's posture verbatim, the pin included"
    );

    let codex_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let on_request = codex(
        CodexApprovalPolicy::OnRequest,
        CodexSandboxMode::WorkspaceWrite,
    );
    start_child(
        &mut delegating.hosted.codex,
        "codex-agent",
        codex_selection("high"),
        on_request,
        "while a Codex sibling starts under Codex's value at auto's level",
    )
    .await;
    assert_eq!(
        posture_of(&descriptor, codex_id).await,
        applied(on_request, true)
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagents_posture_is_refused_as_inherited_when_set_on_the_subagent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-posture-refused", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    start_child(
        &mut delegating.hosted.codex,
        "codex-agent",
        codex_selection("high"),
        codex_asking(),
        "a Claude Session following its default Setting spawns a Codex Subagent that asks",
    )
    .await;

    for (posture, described) in [
        (
            Some(codex_unrestricted()),
            "pinning a value on the Subagent",
        ),
        (None, "resetting the Subagent to follow the Settings"),
    ] {
        let response = set_posture(&descriptor, child_id, posture).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{described}");
        let refusal = response.json::<Value>().await.expect("decode the refusal");
        assert!(
            refusal["message"]
                .as_str()
                .is_some_and(|message| message.contains("inherited from its parent")),
            "{described} is refused as inherited: {refusal}"
        );
    }
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_asking(), false),
        "and the Subagent still reads what it derived from its spawner"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_settings_change_the_spawner_follows_is_derived_again_for_its_brokered_subagent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let mut delegating = delegating(
        state_dir.path(),
        "broker-posture-settings",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) = start_child(
        &mut delegating.hosted.codex,
        "codex-agent",
        codex_selection("high"),
        codex_asking(),
        "Claude's default Setting reads as Codex's asking value, not Codex's own default",
    )
    .await;

    mutate_setting(
        &descriptor,
        SettingMutation::ProviderClaudePermissionMode {
            value: Some(ClaudePermissionMode::DontAsk),
        },
    )
    .await;
    assert_eq!(
        next_posture(&mut child_provider).await,
        codex_contained(),
        "the Claude Setting the spawner follows moves the Codex Subagent with it"
    );
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_contained(), false),
        "unpinned, as its spawner is"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

/// Writes the Config Document the next Server opens with.
fn write_settings(config_dir: &Path, document: &str) {
    std::fs::write(config_dir.join("suru.jsonc"), document).expect("write Config Document");
}

#[tokio::test]
async fn a_restored_brokered_subagent_derives_its_posture_again_from_its_spawners() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "broker-posture-restored";
    write_settings(
        config_dir.path(),
        r#"{"provider":{"claude":{"permissionMode":"bypassPermissions"}}}"#,
    );
    let (caller_id, child_id) = {
        let mut delegating = delegating(state_dir.path(), channel, Some(config_dir.path())).await;
        let child_id = delegating
            .client
            .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
            .await;
        let (child_provider, _) = start_child(
            &mut delegating.hosted.codex,
            "codex-agent",
            codex_selection("high"),
            codex_unrestricted(),
            "a spawner following a bypassPermissions Setting spawns a Subagent that never asks",
        )
        .await;
        delegating
            .hosted
            .server
            .shutdown()
            .await
            .expect("shut down server");
        drop(child_provider);
        (delegating.caller, child_id)
    };

    write_settings(
        config_dir.path(),
        r#"{"provider":{"claude":{"permissionMode":"dontAsk"}}}"#,
    );
    let restarted = host_providers(state_dir.path(), channel, Some(config_dir.path())).await;
    let descriptor = restarted.server.descriptor().clone();
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_contained(), false),
        "the restored Subagent reads what its spawner's posture now derives, applied since no \
         Provider runs for it"
    );
    assert_eq!(
        posture_of(&descriptor, caller_id).await,
        applied(claude(ClaudePermissionMode::DontAsk), false),
        "the spawner having caught up with the Settings the Server restarted with"
    );

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_brokered_subagent_of_a_brokered_subagent_derives_from_the_posture_its_spawner_derived() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-posture-nested", None).await;
    let descriptor = delegating.descriptor.clone();
    pin(
        &descriptor,
        delegating.caller,
        claude(ClaudePermissionMode::DontAsk),
    )
    .await;
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, child_handoff) = start_child(
        &mut delegating.hosted.codex,
        "codex-agent",
        codex_selection("high"),
        codex_contained(),
        "dontAsk reads as Codex's never within the workspace",
    )
    .await;

    let mut child_client = McpClient::handed(&child_handoff);
    child_client.initialize().await;
    let grandchild_id = child_client.spawn_subagent(claude_scout()).await;
    let (mut grandchild_provider, _) = start_child(
        &mut delegating.hosted.claude,
        "claude-agent",
        haiku(),
        claude(ClaudePermissionMode::DontAsk),
        "the Codex Subagent's Claude Subagent reads Claude's value at the Codex one's level",
    )
    .await;

    pin(
        &descriptor,
        delegating.caller,
        claude(ClaudePermissionMode::BypassPermissions),
    )
    .await;
    assert_eq!(
        next_posture(&mut child_provider).await,
        codex_unrestricted()
    );
    assert_eq!(
        next_posture(&mut grandchild_provider).await,
        claude(ClaudePermissionMode::BypassPermissions),
        "a change at the top reaches every level, each derived from the one above it"
    );
    assert_eq!(
        posture_of(&descriptor, grandchild_id).await,
        applied(claude(ClaudePermissionMode::BypassPermissions), true)
    );
    assert_eq!(
        posture_of(&descriptor, child_id).await,
        applied(codex_unrestricted(), true)
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
