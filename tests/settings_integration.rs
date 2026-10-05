//! Config Documents load at server startup, the effective-settings snapshot
//! reaches every connecting client, and typed mutations edit the document in
//! place, per the user configuration spec: a spawned server driven through the
//! managed client over the real protocol, against temp config directories only.

#[path = "support/deadlines.rs"]
mod deadlines;

use crate::deadlines::PROGRESS_DEADLINE;
use std::{net::IpAddr, path::Path};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        AgentSelection, AppearanceMode, AsideVisibility, AutoReclaim, AutoSettle,
        ClaudePermissionMode, CodexApprovalPolicy, CodexSandboxMode, CommandAutoExpand,
        CopilotPermissions, DerivationErrand, FoldPosture, GroupPosture, ModelId, ProviderId,
        ReasoningSummaryDetail, ReasoningVisibility, SessionContentWidth, SettingMutation,
        SettingsDiagnosticSeverity, SettingsSnapshot, SidebarScope, SidebarVisibility,
        ToolCallVisibility,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

async fn attach(state_dir: &Path, channel: &str) -> (ManagedClient, SettingsSnapshot) {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::Connecting)
    ));
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::Connected(_))
    ));
    let snapshot = next_snapshot(&mut client).await;
    (client, snapshot)
}

/// The next Settings snapshot, read past the Model Catalog and the Relays the
/// server pushes beside it: on every connect, and again as either changes.
async fn next_snapshot(client: &mut ManagedClient) -> SettingsSnapshot {
    loop {
        let event = timeout(PROGRESS_DEADLINE, client.next())
            .await
            .expect("settings snapshot arrives")
            .expect("managed client remains open");
        match event {
            ManagedEvent::SettingsSnapshot(snapshot) => return snapshot,
            ManagedEvent::ModelCatalog(_) | ManagedEvent::Relays(_) => continue,
            event => panic!("expected a settings snapshot event, got {event:?}"),
        }
    }
}

#[tokio::test]
async fn appearance_theme_round_trips_byte_preservingly_and_reaches_every_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // Keep the terminal looking delicious.\n",
        "  \"appearance\": { \"theme\":    \"catppuccin\" },\n",
        "  \"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-theme")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-theme").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-theme").await;

    assert_eq!(opening.settings.appearance.theme, "catppuccin");
    assert_eq!(
        opening.pinned,
        ["appearance.theme", "transcript.reasoningVisibility"]
    );
    assert_eq!(opening.diagnostics, []);

    let answered = editor
        .mutate_setting(SettingMutation::AppearanceTheme {
            value: Some("gruvbox".to_owned()),
        })
        .await
        .expect("pin another Theme");
    assert_eq!(answered.settings.appearance.theme, "gruvbox");
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, answered);
    }
    assert_eq!(
        config_document(config_dir.path()),
        original.replace("\"catppuccin\"", "\"gruvbox\""),
        "only the Theme pin's bytes change"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn appearance_mode_round_trips_byte_preservingly_and_reaches_every_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // Keep this Theme in its light variant.\n",
        "  \"appearance\": { \"mode\":    \"light\" },\n",
        "  \"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-mode")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-mode").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-mode").await;

    assert_eq!(opening.settings.appearance.mode, AppearanceMode::Light);
    assert_eq!(
        opening.pinned,
        ["appearance.mode", "transcript.reasoningVisibility"]
    );
    assert_eq!(opening.diagnostics, []);

    let answered = editor
        .mutate_setting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::Dark),
        })
        .await
        .expect("lock appearance to dark mode");
    assert_eq!(answered.settings.appearance.mode, AppearanceMode::Dark);
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, answered);
    }
    assert_eq!(
        config_document(config_dir.path()),
        original.replace("\"light\"", "\"dark\""),
        "only the Mode pin's bytes change"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn nerd_font_icons_round_trip_byte_preservingly_and_reach_every_client() {
    assert!(
        !suru::protocol::EffectiveSettings::default()
            .appearance
            .show_icons,
        "Nerd Font icons stay off until the reader asks for them"
    );

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // My terminal uses a Nerd Font.\n",
        "  \"appearance\": { \"showIcons\":    true },\n",
        "  \"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-icons")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-icons").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-icons").await;

    assert!(opening.settings.appearance.show_icons);
    assert_eq!(
        opening.pinned,
        ["appearance.showIcons", "transcript.reasoningVisibility"]
    );
    assert_eq!(opening.diagnostics, []);

    let answered = editor
        .mutate_setting(SettingMutation::AppearanceShowIcons { value: Some(false) })
        .await
        .expect("hide Nerd Font icons");
    assert!(!answered.settings.appearance.show_icons);
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, answered);
    }
    assert_eq!(
        config_document(config_dir.path()),
        original.replace("true", "false"),
        "only the Show icons pin's bytes change"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_invalid_appearance_mode_is_ignored_alone_and_names_all_three_accepted_values() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "appearance": { "mode": "sepia" },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-mode-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-mode-invalid").await.1;
    assert_eq!(snapshot.settings.appearance.mode, AppearanceMode::System);
    assert_eq!(
        snapshot.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown
    );
    assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.key.as_deref(), Some("appearance.mode"));
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not one of \"system\", \"dark\", or \"light\""
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_unknown_theme_name_is_a_valid_open_setting_pin() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "appearance": { "theme": "my-missing-theme" },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-open-theme")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (_, snapshot) = attach(state_dir.path(), "settings-open-theme").await;
    assert_eq!(snapshot.settings.appearance.theme, "my-missing-theme");
    assert_eq!(
        snapshot.pinned,
        ["appearance.theme", "transcript.reasoningVisibility"]
    );
    assert_eq!(
        snapshot.diagnostics,
        [],
        "runtime Theme discovery, not the schema, decides whether a name resolves"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_pinned_setting_reaches_every_connecting_client_and_the_rest_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Sessions should open with everything visible.
            "transcript": {
                "defaultFoldPosture": "expanded",
            },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-pin")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    for _ in 0..2 {
        let snapshot = attach(state_dir.path(), "settings-pin").await.1;
        assert_eq!(
            snapshot.settings.transcript.default_fold_posture,
            FoldPosture::Expanded
        );
        assert_eq!(
            snapshot.settings.provider.codex.reasoning_summary,
            ReasoningSummaryDetail::Auto,
            "an unpinned Setting keeps its built-in default"
        );
        assert_eq!(snapshot.pinned, ["transcript.defaultFoldPosture"]);
        assert_eq!(snapshot.diagnostics, []);
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn serving_settings_pin_from_the_config_document_with_dual_stack_defaults() {
    let defaults = suru::protocol::EffectiveSettings::default().serving;
    assert!(!defaults.enabled);
    assert_eq!(defaults.port, 7777);
    assert_eq!(
        defaults.bind_address,
        IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        "the default bind answers on every address an Invite could offer"
    );

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "serving": {
                "enabled": false,
                "port": 8443,
                "bindAddress": "0.0.0.0",
            },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-serving")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-serving").await;
    assert!(!opening.settings.serving.enabled);
    assert_eq!(opening.settings.serving.port, 8443);
    assert_eq!(
        opening.settings.serving.bind_address,
        IpAddr::from([0, 0, 0, 0])
    );
    assert_eq!(
        opening.pinned,
        ["serving.bindAddress", "serving.enabled", "serving.port"]
    );
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::ServingBindAddress {
            value: Some(IpAddr::from([127, 0, 0, 1])),
        })
        .await
        .expect("pin the localhost Serving address");
    assert_eq!(
        answered.settings.serving.bind_address,
        IpAddr::from([127, 0, 0, 1])
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// The listener is a way a Serving Server is reached by its user's choice,
/// as each Relay is: on unless they say otherwise, so turning Serving on
/// opens it as it always has, and pinned beside the other Serving Settings
/// by the same Config Document, which an edit of it changes no more of than
/// its own pin.
#[tokio::test]
async fn the_serving_listener_is_on_by_default_and_pins_beside_the_other_serving_settings() {
    assert!(
        suru::protocol::EffectiveSettings::default()
            .serving
            .listener,
        "a Serving Server listens at its own addresses unless its user says otherwise"
    );

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // This machine is reached through its Relays alone.\n",
        "  \"serving\": { \"listener\":    false, \"port\": 8443 },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-serving-listener")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-serving-listener").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-serving-listener").await;

    assert!(!opening.settings.serving.listener);
    assert_eq!(opening.settings.serving.port, 8443);
    assert_eq!(opening.pinned, ["serving.listener", "serving.port"]);
    assert_eq!(opening.diagnostics, []);

    let answered = editor
        .mutate_setting(SettingMutation::ServingListener { value: Some(true) })
        .await
        .expect("turn the Serving listener on");
    assert!(answered.settings.serving.listener);
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, answered);
    }
    assert_eq!(
        config_document(config_dir.path()),
        original.replace("false", "true"),
        "only the listener's pin changes"
    );

    let reset = editor
        .mutate_setting(SettingMutation::ServingListener { value: None })
        .await
        .expect("leave the Serving listener to its default");
    assert!(reset.settings.serving.listener);
    assert_eq!(reset.pinned, ["serving.port"]);

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn showing_reasoning_pins_from_a_document_and_resets_to_the_hidden_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I want to read the model think.
            "transcript": { "reasoningVisibility": "shown" },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-reasoning")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-reasoning").await;
    assert_eq!(
        opening.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown
    );
    assert_eq!(
        opening.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto,
        "what a Transcript draws is presentation and asks Codex for nothing different"
    );
    assert_eq!(opening.pinned, ["transcript.reasoningVisibility"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::TranscriptReasoningVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Hidden,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Tool Calls are drawn unless the user asks otherwise, and a Config Document
/// that hides them is read at startup like any other pin; a reset lets the
/// shown default resume and takes the pin back out of the document, leaving
/// the rest of it — Reasoning's own visibility among it — alone.
#[tokio::test]
async fn hiding_tool_calls_pins_from_a_document_and_resets_to_the_shown_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Only the answers, please.
            "transcript": { "toolCallVisibility": "hidden", "reasoningVisibility": "shown" },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-tool-calls")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    assert_eq!(
        suru::protocol::EffectiveSettings::default()
            .transcript
            .tool_call_visibility,
        ToolCallVisibility::Shown,
        "a Transcript draws Tool Calls unless the user says otherwise"
    );

    let (client, opening) = attach(state_dir.path(), "settings-tool-calls").await;
    assert_eq!(
        opening.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Hidden
    );
    assert_eq!(
        opening.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "each kind's visibility is its own Setting"
    );
    let mut pinned = opening.pinned.clone();
    pinned.sort();
    assert_eq!(
        pinned,
        [
            "transcript.reasoningVisibility",
            "transcript.toolCallVisibility"
        ]
    );
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::TranscriptToolCallVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Shown,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, ["transcript.reasoningVisibility"]);
    let document = config_document(config_dir.path());
    assert!(
        !document.contains("toolCallVisibility"),
        "the reset takes the pin back out: {document}"
    );
    assert!(
        document.contains("// Only the answers, please.")
            && document.contains("\"reasoningVisibility\": \"shown\""),
        "and leaves the rest of the document alone: {document}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Hiding Tool Calls from the settings panel is a typed mutation like any
/// other: it lands in the Config Document beside what the document already
/// held and reaches every client, and choosing the value the default already
/// holds still pins it, so a deliberate choice survives a later change of that
/// default.
#[tokio::test]
async fn tool_call_visibility_mutations_land_in_the_document_and_reach_every_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // Keep the Transcript's other choices exactly as written.\n",
        "  \"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-tool-call-mutations")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-tool-call-mutations").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-tool-call-mutations").await;
    assert_eq!(
        opening.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Shown
    );

    let hidden = editor
        .mutate_setting(SettingMutation::TranscriptToolCallVisibility {
            value: Some(ToolCallVisibility::Hidden),
        })
        .await
        .expect("hide Tool Calls");
    assert_eq!(
        hidden.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Hidden
    );
    assert_eq!(
        hidden.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "the rest of the Transcript's Settings stand"
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, hidden);
    }
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"toolCallVisibility\": \"hidden\""),
        "the pin lands in the Config Document: {document}"
    );
    assert!(
        document.contains("// Keep the Transcript's other choices exactly as written.")
            && document.contains("\"reasoningVisibility\": \"shown\""),
        "and leaves the rest of it alone: {document}"
    );

    let shown = editor
        .mutate_setting(SettingMutation::TranscriptToolCallVisibility {
            value: Some(ToolCallVisibility::Shown),
        })
        .await
        .expect("show Tool Calls again");
    assert_eq!(
        shown.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Shown
    );
    let mut pinned = shown.pinned.clone();
    pinned.sort();
    assert_eq!(
        pinned,
        [
            "transcript.reasoningVisibility",
            "transcript.toolCallVisibility"
        ],
        "a value equal to the default is still a pin"
    );
    assert!(
        config_document(config_dir.path()).contains("\"toolCallVisibility\": \"shown\""),
        "the explicit choice is what the document now says"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_invalid_tool_call_visibility_is_ignored_alone_and_names_both_accepted_values() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "transcript": { "toolCallVisibility": "invisible", "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-tool-calls-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-tool-calls-invalid")
        .await
        .1;
    assert_eq!(
        snapshot.settings.transcript.tool_call_visibility,
        ToolCallVisibility::Shown
    );
    assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(
        diagnostic.key.as_deref(),
        Some("transcript.toolCallVisibility")
    );
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not one of \"shown\" or \"hidden\""
    );

    server.shutdown().await.expect("shut down server");
}

/// Image previews are drawn wherever the terminal can draw them unless the
/// user says otherwise, and turning them off is a pin like any other: it lands
/// in the Config Document beside what the document already held, reads back,
/// and a reset lets the built-in default resume.
#[tokio::test]
async fn image_previews_default_on_pin_off_through_the_config_document_and_reset() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // Keep the Transcript's other choices exactly as written.\n",
        "  \"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-image-previews")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-image-previews").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-image-previews").await;
    assert!(
        opening.settings.transcript.image_previews,
        "previews are drawn wherever the terminal can draw them unless the user says otherwise"
    );
    assert_eq!(opening.pinned, ["transcript.reasoningVisibility"]);

    let pinned = editor
        .mutate_setting(SettingMutation::TranscriptImagePreviews { value: Some(false) })
        .await
        .expect("turn image previews off");
    assert!(!pinned.settings.transcript.image_previews);
    assert_eq!(
        pinned.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "the rest of the Transcript's Settings stand"
    );
    let mut pinned_keys = pinned.pinned.clone();
    pinned_keys.sort();
    assert_eq!(
        pinned_keys,
        ["transcript.imagePreviews", "transcript.reasoningVisibility"]
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, pinned);
    }
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"imagePreviews\": false"),
        "the pin lands in the Config Document: {document}"
    );
    assert!(
        document.contains("// Keep the Transcript's other choices exactly as written.")
            && document.contains("\"reasoningVisibility\": \"shown\""),
        "and leaves the rest of it alone: {document}"
    );

    let reset = editor
        .mutate_setting(SettingMutation::TranscriptImagePreviews { value: None })
        .await
        .expect("reset image previews");
    assert!(
        reset.settings.transcript.image_previews,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(reset.pinned, ["transcript.reasoningVisibility"]);
    let document = config_document(config_dir.path());
    assert!(
        !document.contains("imagePreviews"),
        "the reset takes the pin back out: {document}"
    );
    assert!(
        document.contains("// Keep the Transcript's other choices exactly as written.")
            && document.contains("\"reasoningVisibility\": \"shown\""),
        "and leaves the rest of the document alone: {document}"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

/// Subsessions are listed like any other Session until the user says
/// otherwise, and hiding them is a pin like any other: it lands in the Config
/// Document beside what the document already held, reaches every Client, and a
/// reset lets the built-in default resume.
#[tokio::test]
async fn hiding_subsessions_defaults_off_pins_on_through_the_config_document_and_resets() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let original = concat!(
        "{\n",
        "  // Keep the Sidebar's other choices exactly as written.\n",
        "  \"sidebar\": { \"autoSettle\": \"off\" },\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), original).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-hide-subsessions")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-hide-subsessions").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-hide-subsessions").await;
    assert!(
        !opening.settings.sidekick.hide_subsessions,
        "Subsessions are listed like any other Session unless the user says otherwise"
    );
    assert_eq!(opening.pinned, ["sidebar.autoSettle"]);

    let pinned = editor
        .mutate_setting(SettingMutation::SidekickHideSubsessions { value: Some(true) })
        .await
        .expect("hide Subsessions");
    assert!(pinned.settings.sidekick.hide_subsessions);
    assert_eq!(
        pinned.settings.sidebar.auto_settle,
        AutoSettle::Off,
        "the Sidebar's own Settings stand"
    );
    let mut pinned_keys = pinned.pinned.clone();
    pinned_keys.sort();
    assert_eq!(
        pinned_keys,
        ["sidebar.autoSettle", "sidekick.hideSubsessions"]
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, pinned);
    }
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"hideSubsessions\": true"),
        "the pin lands in the Config Document: {document}"
    );
    assert!(
        document.contains("// Keep the Sidebar's other choices exactly as written.")
            && document.contains("\"autoSettle\": \"off\""),
        "and leaves the rest of it alone: {document}"
    );

    let reset = editor
        .mutate_setting(SettingMutation::SidekickHideSubsessions { value: None })
        .await
        .expect("reset hiding Subsessions");
    assert!(
        !reset.settings.sidekick.hide_subsessions,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(reset.pinned, ["sidebar.autoSettle"]);
    let document = config_document(config_dir.path());
    assert!(
        !document.contains("hideSubsessions"),
        "the reset takes the pin back out: {document}"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn command_auto_expansion_pins_a_millisecond_delay_and_resets_to_off() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "transcript": { "commandAutoExpand": 275 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-command-auto-expand")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-command-auto-expand").await;
    assert_eq!(
        opening.settings.transcript.command_auto_expand,
        CommandAutoExpand::AfterMillis(275)
    );
    assert_eq!(opening.pinned, ["transcript.commandAutoExpand"]);
    assert_eq!(opening.diagnostics, []);

    let reset = client
        .mutate_setting(SettingMutation::TranscriptCommandAutoExpand { value: None })
        .await
        .expect("reset Command auto-expansion");
    assert_eq!(
        reset.settings.transcript.command_auto_expand,
        CommandAutoExpand::Off
    );
    assert_eq!(reset.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Groups open collapsed unless the user says otherwise; a Config Document
/// pins any of the three postures, a mutation moves the pin, and a reset lets
/// the collapsed default resume.
#[tokio::test]
async fn transcript_groups_pin_from_a_document_move_by_mutation_and_reset_to_collapsed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "transcript": { "groups": "off" } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-transcript-groups")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    assert_eq!(
        suru::protocol::EffectiveSettings::default()
            .transcript
            .groups,
        GroupPosture::Collapsed
    );

    let (client, opening) = attach(state_dir.path(), "settings-transcript-groups").await;
    assert_eq!(opening.settings.transcript.groups, GroupPosture::Off);
    assert_eq!(opening.pinned, ["transcript.groups"]);
    assert_eq!(opening.diagnostics, []);

    let expanded = client
        .mutate_setting(SettingMutation::TranscriptGroups {
            value: Some(GroupPosture::Expanded),
        })
        .await
        .expect("open Groups expanded");
    assert_eq!(expanded.settings.transcript.groups, GroupPosture::Expanded);
    assert!(
        config_document(config_dir.path()).contains("\"groups\": \"expanded\""),
        "the mutation lands in the Config Document"
    );

    let reset = client
        .mutate_setting(SettingMutation::TranscriptGroups { value: None })
        .await
        .expect("reset Groups");
    assert_eq!(reset.settings.transcript.groups, GroupPosture::Collapsed);
    assert_eq!(reset.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_hidden_sidebar_pins_from_a_document_and_resets_to_the_shown_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I would rather have the columns.
            "sidebar": { "initialVisibility": "hidden" },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-sidebar")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-sidebar").await;
    assert_eq!(
        opening.settings.sidebar.initial_visibility,
        SidebarVisibility::Hidden
    );
    assert_eq!(opening.pinned, ["sidebar.initialVisibility"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::SidebarInitialVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.sidebar.initial_visibility,
        SidebarVisibility::Shown,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn sidebar_initial_width_loads_without_a_product_level_maximum_and_resets() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "sidebar": { "initialWidth": 1000000 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-sidebar-width")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-sidebar-width").await;
    assert_eq!(opening.settings.sidebar.initial_width, 1_000_000);
    assert_eq!(opening.pinned, ["sidebar.initialWidth"]);
    assert_eq!(opening.diagnostics, []);

    let minimum = client
        .mutate_setting(SettingMutation::SidebarInitialWidth { value: Some(24) })
        .await
        .expect("pin the minimum Sidebar width");
    assert_eq!(minimum.settings.sidebar.initial_width, 24);
    assert_eq!(minimum.pinned, ["sidebar.initialWidth"]);

    let reset = client
        .mutate_setting(SettingMutation::SidebarInitialWidth { value: None })
        .await
        .expect("reset Sidebar width at launch");
    assert_eq!(reset.settings.sidebar.initial_width, 32);
    assert_eq!(reset.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_sidebar_width_below_the_floor_is_ignored_individually_with_a_diagnostic() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "sidebar": { "initialWidth": 23 },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-sidebar-width-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (_, snapshot) = attach(state_dir.path(), "settings-sidebar-width-invalid").await;
    assert_eq!(snapshot.settings.sidebar.initial_width, 32);
    assert_eq!(
        snapshot.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "the unrelated valid pin still applies"
    );
    assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("the invalid width should produce one diagnostic");
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(diagnostic.key.as_deref(), Some("sidebar.initialWidth"));
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not an integer of at least 24"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_hidden_aside_pins_from_a_document_and_resets_to_the_shown_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I would rather have the columns.
            "aside": { "initialVisibility": "hidden" },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-aside")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-aside").await;
    assert_eq!(
        opening.settings.aside.initial_visibility,
        AsideVisibility::Hidden
    );
    assert_eq!(opening.pinned, ["aside.initialVisibility"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::AsideInitialVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.aside.initial_visibility,
        AsideVisibility::Shown,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn aside_initial_width_pins_from_a_document_and_resets_to_its_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "aside": { "initialWidth": 48 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-aside-width")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-aside-width").await;
    assert_eq!(opening.settings.aside.initial_width, 48);
    assert_eq!(
        opening.settings.sidebar.initial_width, 32,
        "the Aside's width is its own, not the Sidebar's"
    );
    assert_eq!(opening.pinned, ["aside.initialWidth"]);
    assert_eq!(opening.diagnostics, []);

    let minimum = client
        .mutate_setting(SettingMutation::AsideInitialWidth { value: Some(24) })
        .await
        .expect("pin the minimum Aside width");
    assert_eq!(minimum.settings.aside.initial_width, 24);
    assert_eq!(minimum.pinned, ["aside.initialWidth"]);

    let reset = client
        .mutate_setting(SettingMutation::AsideInitialWidth { value: None })
        .await
        .expect("reset Aside width at launch");
    assert_eq!(reset.settings.aside.initial_width, 32);
    assert_eq!(reset.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_aside_width_below_the_floor_is_ignored_individually_with_a_diagnostic() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "aside": { "initialWidth": 23 },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-aside-width-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (_, snapshot) = attach(state_dir.path(), "settings-aside-width-invalid").await;
    assert_eq!(snapshot.settings.aside.initial_width, 32);
    assert_eq!(
        snapshot.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "the unrelated valid pin still applies"
    );
    assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("the invalid width should produce one diagnostic");
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(diagnostic.key.as_deref(), Some("aside.initialWidth"));
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not an integer of at least 24"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_everywhere_sidebar_at_launch_pins_from_a_document_and_resets_to_every_workspace() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Every Origin is where I look for work.
            "sidebar": { "initialScope": "everywhere" },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-sidebar-scope")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-sidebar-scope").await;
    assert_eq!(
        opening.settings.sidebar.initial_scope,
        SidebarScope::Everywhere
    );
    assert_eq!(opening.pinned, ["sidebar.initialScope"]);
    assert_eq!(opening.diagnostics, []);

    let narrowed = client
        .mutate_setting(SettingMutation::SidebarInitialScope {
            value: Some(SidebarScope::CurrentWorkspace),
        })
        .await
        .expect("narrow the starting scope");
    assert_eq!(
        narrowed.settings.sidebar.initial_scope,
        SidebarScope::CurrentWorkspace
    );
    let everywhere = client
        .mutate_setting(SettingMutation::SidebarInitialScope {
            value: Some(SidebarScope::Everywhere),
        })
        .await
        .expect("pin Everywhere again");
    assert_eq!(
        everywhere.settings.sidebar.initial_scope,
        SidebarScope::Everywhere,
        "Everywhere survives the round trip through the Config Document"
    );
    assert!(
        config_document(config_dir.path()).contains(r#""initialScope": "everywhere""#),
        "the Setting keeps the schema's spelling in the document"
    );

    let answered = client
        .mutate_setting(SettingMutation::SidebarInitialScope { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.sidebar.initial_scope,
        SidebarScope::AllWorkspaces,
        "unpinning it lets the whole body of work resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_invalid_sidebar_scope_names_all_three_accepted_values() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "sidebar": { "initialScope": "nearby" } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-sidebar-scope-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-sidebar-scope-invalid")
        .await
        .1;
    assert_eq!(
        snapshot.settings.sidebar.initial_scope,
        SidebarScope::AllWorkspaces,
        "the rejected value leaves the built-in default in force"
    );
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.key.as_deref(), Some("sidebar.initialScope"));
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not one of \"all_workspaces\", \"current_workspace\", or \"everywhere\""
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_auto_settle_setting_pins_from_a_document_and_resets_to_its_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // A week is how long I leave a thing before it is done with me.
            "sidebar": { "autoSettle": 7 },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-auto-settle")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-auto-settle").await;
    assert_eq!(opening.settings.sidebar.auto_settle, AutoSettle::Idle(7));
    assert_eq!(opening.pinned, ["sidebar.autoSettle"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::SidebarAutoSettle {
            value: Some(AutoSettle::Off),
        })
        .await
        .expect("turn settling off");
    assert_eq!(
        answered.settings.sidebar.auto_settle,
        AutoSettle::Off,
        "one Setting holds both the threshold and the word that suspends it"
    );
    assert_eq!(answered.pinned, ["sidebar.autoSettle"]);

    let answered = client
        .mutate_setting(SettingMutation::SidebarAutoSettle { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.sidebar.auto_settle,
        AutoSettle::Idle(3),
        "unpinning it lets the built-in three days resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_auto_reclaim_setting_round_trips_both_scalar_forms_and_defaults_to_fourteen_days() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "worktree": { "autoReclaim": 7 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-auto-reclaim")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-auto-reclaim").await;
    assert_eq!(
        opening.settings.worktree.auto_reclaim,
        AutoReclaim::AfterDays(7)
    );
    assert_eq!(opening.pinned, ["worktree.autoReclaim"]);

    let off = client
        .mutate_setting(SettingMutation::WorktreeAutoReclaim {
            value: Some(AutoReclaim::Off),
        })
        .await
        .expect("turn Reclaim off");
    assert_eq!(off.settings.worktree.auto_reclaim, AutoReclaim::Off);

    let reset = client
        .mutate_setting(SettingMutation::WorktreeAutoReclaim { value: None })
        .await
        .expect("reset Reclaim");
    assert_eq!(
        reset.settings.worktree.auto_reclaim,
        AutoReclaim::AfterDays(14)
    );
    assert!(reset.pinned.is_empty());

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn auto_reclaim_rejects_values_below_one_with_the_usual_diagnostic() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "worktree": { "autoReclaim": 0 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-auto-reclaim-invalid")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-auto-reclaim-invalid")
        .await
        .1;
    assert_eq!(
        snapshot.settings.worktree.auto_reclaim,
        AutoReclaim::AfterDays(14)
    );
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic: {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.key.as_deref(), Some("worktree.autoReclaim"));
    assert_eq!(
        diagnostic.message,
        "ignored because its value is not one of \"off\" or a whole number of days, at least 1"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn invalid_auto_settle_pins_are_ignored_individually_with_diagnostics() {
    for (case, invalid) in [
        ("fraction", "1.5"),
        ("boolean", "false"),
        ("word", r#""never""#),
        ("below-minimum", "0"),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let config_dir = tempfile::tempdir().expect("create isolated config directory");
        std::fs::write(
            config_dir.path().join("suru.jsonc"),
            format!(
                r#"{{
                    "sidebar": {{ "autoSettle": {invalid} }},
                    "transcript": {{ "reasoningVisibility": "shown" }}
                }}"#
            ),
        )
        .expect("write Config Document");
        let channel = format!("settings-auto-settle-invalid-{case}");
        let server = server::spawn(
            ServerConfig::new(state_dir.path(), &channel)
                .expect("configure server")
                .with_config_dir(config_dir.path()),
        )
        .await
        .expect("spawn server");

        let snapshot = attach(state_dir.path(), &channel).await.1;
        assert_eq!(
            snapshot.settings.sidebar.auto_settle,
            AutoSettle::Idle(3),
            "{case} keeps the built-in fallback"
        );
        assert_eq!(
            snapshot.settings.transcript.reasoning_visibility,
            ReasoningVisibility::Shown,
            "{case} ignores only the invalid pin"
        );
        assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
        let [diagnostic] = snapshot.diagnostics.as_slice() else {
            panic!(
                "{case} should produce one diagnostic: {:?}",
                snapshot.diagnostics
            );
        };
        assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
        assert_eq!(diagnostic.key.as_deref(), Some("sidebar.autoSettle"));
        assert_eq!(
            diagnostic.message,
            "ignored because its value is not one of \"off\" or a whole number of days, at least 1"
        );

        server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn fill_session_content_width_loads_from_a_config_document() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "session": { "contentWidth": "fill" } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-content-fill")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-content-fill").await.1;
    assert_eq!(
        snapshot.settings.session.content_width,
        SessionContentWidth::Fill
    );
    assert_eq!(snapshot.pinned, ["session.contentWidth"]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn minimum_session_content_width_loads_from_a_config_document() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "session": { "contentWidth": 50 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-content-minimum")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-content-minimum").await.1;
    assert_eq!(
        snapshot.settings.session.content_width,
        SessionContentWidth::Maximum(50)
    );
    assert_eq!(snapshot.pinned, ["session.contentWidth"]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn session_content_width_has_no_product_level_maximum() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "session": { "contentWidth": 1000000 } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-content-unbounded")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-content-unbounded")
        .await
        .1;
    assert_eq!(
        snapshot.settings.session.content_width,
        SessionContentWidth::Maximum(1_000_000)
    );
    assert_eq!(snapshot.pinned, ["session.contentWidth"]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn invalid_session_content_width_pins_are_ignored_individually_with_diagnostics() {
    for (case, invalid) in [
        ("fraction", "49.5"),
        ("boolean", "true"),
        ("word", r#""wide""#),
        ("below-minimum", "49"),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let config_dir = tempfile::tempdir().expect("create isolated config directory");
        std::fs::write(
            config_dir.path().join("suru.jsonc"),
            format!(
                r#"{{
                    "session": {{ "contentWidth": {invalid} }},
                    "transcript": {{ "reasoningVisibility": "shown" }}
                }}"#
            ),
        )
        .expect("write Config Document");
        let channel = format!("settings-content-invalid-{case}");
        let server = server::spawn(
            ServerConfig::new(state_dir.path(), &channel)
                .expect("configure server")
                .with_config_dir(config_dir.path()),
        )
        .await
        .expect("spawn server");

        let snapshot = attach(state_dir.path(), &channel).await.1;
        assert_eq!(
            snapshot.settings.session.content_width,
            SessionContentWidth::Maximum(80),
            "{case} keeps the built-in fallback"
        );
        assert_eq!(
            snapshot.settings.transcript.reasoning_visibility,
            ReasoningVisibility::Shown,
            "{case} ignores only the invalid pin"
        );
        assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);
        let [diagnostic] = snapshot.diagnostics.as_slice() else {
            panic!(
                "{case} should produce one diagnostic: {:?}",
                snapshot.diagnostics
            );
        };
        assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
        assert_eq!(diagnostic.key.as_deref(), Some("session.contentWidth"));
        assert_eq!(
            diagnostic.message,
            "ignored because its value is not one of \"fill\" or an integer of at least 50"
        );

        server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn suru_json_alone_works_and_is_parsed_just_as_leniently() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.json"),
        r#"{
            /* Plain-JSON habits, JSONC leniency. */
            "provider": { "codex": { "reasoningSummary": "detailed", } },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-json")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-json").await.1;
    assert_eq!(
        snapshot.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Detailed
    );
    assert_eq!(snapshot.pinned, ["provider.codex.reasoningSummary"]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn suru_jsonc_wins_over_a_duplicate_suru_json_with_a_diagnostic() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "transcript": { "defaultFoldPosture": "expanded" } }"#,
    )
    .expect("write primary Config Document");
    std::fs::write(
        config_dir.path().join("suru.json"),
        r#"{
            "transcript": { "defaultFoldPosture": "folded" },
            "provider": { "codex": { "reasoningSummary": "detailed" } }
        }"#,
    )
    .expect("write duplicate Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-duplicate")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-duplicate").await.1;
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Expanded,
        "the winning suru.jsonc value applies"
    );
    assert_eq!(
        snapshot.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto,
        "nothing from the ignored suru.json is merged in"
    );
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(diagnostic.file, config_dir.path().join("suru.json"));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_syntax_broken_document_never_blocks_startup_and_defaults_apply_loudly() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "transcript": { "defaultFoldPosture" "#,
    )
    .expect("write broken Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-broken")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("a broken Config Document never prevents startup");

    let snapshot = attach(state_dir.path(), "settings-broken").await.1;
    assert_eq!(snapshot.settings, Default::default());
    assert_eq!(snapshot.pinned, [] as [String; 0]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Error);
    assert_eq!(diagnostic.file, config_dir.path().join("suru.jsonc"));
    assert_eq!(diagnostic.key, None);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn unknown_and_mistyped_keys_are_ignored_alone_while_the_rest_applies() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "transcript": {
                "defaultFoldPosture": "sideways",
                "unknownKnob": true
            },
            "provider": { "codex": { "reasoningSummary": "concise" } }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-perkey")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-perkey").await.1;
    assert_eq!(
        snapshot.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Concise,
        "the valid pin applies despite its neighbors"
    );
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Folded,
        "the mistyped Setting keeps its built-in default"
    );
    assert_eq!(snapshot.pinned, ["provider.codex.reasoningSummary"]);
    let keys: Vec<_> = snapshot
        .diagnostics
        .iter()
        .map(|diagnostic| {
            assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
            assert_eq!(diagnostic.file, config_dir.path().join("suru.jsonc"));
            diagnostic
                .key
                .as_deref()
                .expect("per-key diagnostics name their key")
        })
        .collect();
    assert_eq!(
        keys,
        ["transcript.defaultFoldPosture", "transcript.unknownKnob"]
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn without_a_config_root_every_setting_is_its_built_in_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-defaults").expect("configure server"),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-defaults").await.1;
    assert_eq!(snapshot.settings, Default::default());
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Folded
    );
    assert_eq!(
        snapshot.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto
    );
    assert_eq!(
        snapshot.settings.session.content_width,
        SessionContentWidth::Maximum(80),
        "the Session Content Column defaults to an 80-column maximum"
    );
    assert_eq!(snapshot.pinned, [] as [String; 0]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_flat_dotted_spelling_is_not_a_second_way_to_pin() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "transcript.defaultFoldPosture": "expanded" }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-flat")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-flat").await.1;
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Folded,
        "only the nested spelling the schema defines can pin a Setting"
    );
    assert_eq!(snapshot.pinned, [] as [String; 0]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(
        diagnostic.key.as_deref(),
        Some("transcript.defaultFoldPosture")
    );

    server.shutdown().await.expect("shut down server");
}

/// A hand-written Config Document: comments, tabs, irregular spacing around a
/// value, and a key order the author chose. Suru's edits must leave every byte
/// of it alone except the value they target.
const HAND_WRITTEN: &str = "{\n\t// Codex should say as much about its thinking as it likes.\n\t\"provider\": {\n\t\t\"codex\": { \"reasoningSummary\":    \"detailed\" }\n\t},\n\n\t// I read my Sessions folded.\n\t\"transcript\": { \"defaultFoldPosture\": \"folded\" },\n}\n";

const CONTENT_WIDTH_DOCUMENT: &str = concat!(
    "{\n",
    "\t// Keep this hand-written configuration exactly as authored.\n",
    "\t\"transcript\": { \"reasoningVisibility\": \"shown\" },\n",
    "\t\"session\": { \"contentWidth\": \"fill\" },\n",
    "}\n",
);

fn config_document(config_dir: &Path) -> String {
    std::fs::read_to_string(config_dir.join("suru.jsonc")).expect("read the Config Document")
}

#[tokio::test]
async fn session_content_width_mutations_preserve_bytes_broadcast_and_reset() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), CONTENT_WIDTH_DOCUMENT)
        .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-content-mutation")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-content-mutation").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-content-mutation").await;
    assert_eq!(
        opening.settings.session.content_width,
        SessionContentWidth::Fill
    );

    let maximum = editor
        .mutate_setting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Maximum(80)),
        })
        .await
        .expect("pin the built-in maximum explicitly");
    assert_eq!(
        maximum.settings.session.content_width,
        SessionContentWidth::Maximum(80)
    );
    assert_eq!(
        maximum.pinned,
        ["session.contentWidth", "transcript.reasoningVisibility"]
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, maximum);
    }
    assert_eq!(
        config_document(config_dir.path()),
        CONTENT_WIDTH_DOCUMENT.replace("\"fill\"", "80"),
        "the typed maximum changes only its target bytes"
    );

    let fill = editor
        .mutate_setting(SettingMutation::SessionContentWidth {
            value: Some(SessionContentWidth::Fill),
        })
        .await
        .expect("pin fill through a typed mutation");
    assert_eq!(
        fill.settings.session.content_width,
        SessionContentWidth::Fill
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, fill);
    }

    let reset = editor
        .mutate_setting(SettingMutation::SessionContentWidth { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        reset.settings.session.content_width,
        SessionContentWidth::Maximum(80)
    );
    assert_eq!(reset.pinned, ["transcript.reasoningVisibility"]);
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, reset);
    }
    assert!(
        config_document(config_dir.path())
            .contains("\t\"transcript\": { \"reasoningVisibility\": \"shown\" },"),
        "reset leaves the unrelated hand-written Setting bytes alone"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_set_reaches_the_document_the_mutating_client_and_a_second_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-set")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, opening) = attach(state_dir.path(), "settings-set").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-set").await;
    assert_eq!(
        opening.settings.transcript.default_fold_posture,
        FoldPosture::Folded
    );

    let answered = editor
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect("set the Setting");

    assert_eq!(
        answered.settings.transcript.default_fold_posture,
        FoldPosture::Expanded
    );
    assert_eq!(answered.pinned, ["transcript.defaultFoldPosture"]);
    for client in [&mut editor, &mut onlooker] {
        let pushed = next_snapshot(client).await;
        assert_eq!(
            pushed, answered,
            "every attached client sees the accepted mutation"
        );
    }
    assert_eq!(
        config_document(config_dir.path()),
        "{\n  \"transcript\": {\n    \"defaultFoldPosture\": \"expanded\"\n  }\n}\n"
    );

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_edit_leaves_a_hand_written_document_byte_for_byte_except_its_target() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), HAND_WRITTEN)
        .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-bytes")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-bytes").await;

    client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect("set the Setting");

    assert_eq!(
        config_document(config_dir.path()),
        HAND_WRITTEN.replace("\"folded\"", "\"expanded\""),
        "only the targeted value's bytes change"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_new_key_lands_in_the_documents_own_indent_style_leaving_the_rest_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let seeded = concat!(
        "{\n",
        "    // Codex should say as much about its thinking as it likes.\n",
        "    \"provider\": {\n",
        "        \"codex\": {\n",
        "            \"reasoningSummary\": \"detailed\"\n",
        "        }\n",
        "    }\n",
        "}\n",
    );
    std::fs::write(config_dir.path().join("suru.jsonc"), seeded).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-insert")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-insert").await;

    client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect("set the Setting");

    assert_eq!(
        config_document(config_dir.path()),
        concat!(
            "{\n",
            "    // Codex should say as much about its thinking as it likes.\n",
            "    \"provider\": {\n",
            "        \"codex\": {\n",
            "            \"reasoningSummary\": \"detailed\"\n",
            "        }\n",
            "    },\n",
            "    \"transcript\": {\n",
            "        \"defaultFoldPosture\": \"expanded\"\n",
            "    }\n",
            "}\n",
        ),
        "the inserted key follows the document's own indent style"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn pinning_the_built_in_default_still_writes_it_and_unset_takes_it_back_out() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-default-pin")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-default-pin").await;

    let pinned = client
        .mutate_setting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Auto),
        })
        .await
        .expect("pin the built-in default");

    assert_eq!(
        pinned.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto
    );
    assert_eq!(
        pinned.pinned,
        ["provider.codex.reasoningSummary"],
        "a deliberate choice is pinned even when it equals the default"
    );
    assert!(config_document(config_dir.path()).contains("\"reasoningSummary\": \"auto\""));

    let unpinned = client
        .mutate_setting(SettingMutation::ProviderCodexReasoningSummary { value: None })
        .await
        .expect("remove the pin");

    assert_eq!(
        unpinned.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto,
        "the built-in default resumes"
    );
    assert_eq!(unpinned.pinned, [] as [String; 0]);
    assert_eq!(
        config_document(config_dir.path()),
        "{}\n",
        "the pin leaves nothing behind"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_posture_defaults_pin_through_the_config_document_and_reset() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-codex-posture")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-codex-posture").await;
    assert_eq!(
        initial.settings.provider.codex.approval_policy,
        CodexApprovalPolicy::OnRequest
    );
    assert_eq!(
        initial.settings.provider.codex.sandbox_mode,
        CodexSandboxMode::WorkspaceWrite
    );

    let pinned = client
        .mutate_setting(SettingMutation::ProviderCodexApprovalPolicy {
            value: Some(CodexApprovalPolicy::Never),
        })
        .await
        .unwrap();
    assert_eq!(
        pinned.settings.provider.codex.approval_policy,
        CodexApprovalPolicy::Never
    );
    let pinned = client
        .mutate_setting(SettingMutation::ProviderCodexSandboxMode {
            value: Some(CodexSandboxMode::DangerFullAccess),
        })
        .await
        .unwrap();
    assert_eq!(
        pinned.settings.provider.codex.sandbox_mode,
        CodexSandboxMode::DangerFullAccess
    );
    assert!(
        pinned
            .pinned
            .contains(&"provider.codex.approvalPolicy".to_owned())
    );
    assert!(
        pinned
            .pinned
            .contains(&"provider.codex.sandboxMode".to_owned())
    );
    let document = config_document(config_dir.path());
    assert!(document.contains("\"approvalPolicy\": \"never\""));
    assert!(document.contains("\"sandboxMode\": \"danger-full-access\""));

    client
        .mutate_setting(SettingMutation::ProviderCodexApprovalPolicy { value: None })
        .await
        .unwrap();
    let reset = client
        .mutate_setting(SettingMutation::ProviderCodexSandboxMode { value: None })
        .await
        .unwrap();
    assert_eq!(
        reset.settings.provider.codex.approval_policy,
        CodexApprovalPolicy::OnRequest
    );
    assert_eq!(
        reset.settings.provider.codex.sandbox_mode,
        CodexSandboxMode::WorkspaceWrite
    );
    assert_eq!(reset.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn copilot_permissions_default_pins_through_the_config_document_and_resets() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-copilot-permissions")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-copilot-permissions").await;
    assert_eq!(
        initial.settings.provider.copilot.permissions,
        CopilotPermissions::Ask
    );
    let pinned = client
        .mutate_setting(SettingMutation::ProviderCopilotPermissions {
            value: Some(CopilotPermissions::AllowAll),
        })
        .await
        .unwrap();
    assert_eq!(
        pinned.settings.provider.copilot.permissions,
        CopilotPermissions::AllowAll
    );
    assert_eq!(pinned.pinned, ["provider.copilot.permissions"]);
    assert!(config_document(config_dir.path()).contains("\"permissions\": \"allowAll\""));
    let reset = client
        .mutate_setting(SettingMutation::ProviderCopilotPermissions { value: None })
        .await
        .unwrap();
    assert_eq!(
        reset.settings.provider.copilot.permissions,
        CopilotPermissions::Ask
    );
    assert_eq!(reset.pinned, [] as [String; 0]);
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn claude_permission_mode_defaults_pins_through_the_config_document_and_resets() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-claude-permission-mode")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-claude-permission-mode").await;
    assert_eq!(
        initial.settings.provider.claude.permission_mode,
        ClaudePermissionMode::Default
    );

    for (mode, wire) in [
        (ClaudePermissionMode::Default, "default"),
        (ClaudePermissionMode::AcceptEdits, "acceptEdits"),
        (ClaudePermissionMode::DontAsk, "dontAsk"),
        (ClaudePermissionMode::BypassPermissions, "bypassPermissions"),
        (ClaudePermissionMode::Auto, "auto"),
    ] {
        let pinned = client
            .mutate_setting(SettingMutation::ProviderClaudePermissionMode { value: Some(mode) })
            .await
            .unwrap();
        assert_eq!(pinned.settings.provider.claude.permission_mode, mode);
        assert!(
            pinned
                .pinned
                .contains(&"provider.claude.permissionMode".to_owned())
        );
        assert!(
            config_document(config_dir.path()).contains(&format!("\"permissionMode\": \"{wire}\""))
        );
    }

    let reset = client
        .mutate_setting(SettingMutation::ProviderClaudePermissionMode { value: None })
        .await
        .unwrap();
    assert_eq!(
        reset.settings.provider.claude.permission_mode,
        ClaudePermissionMode::Default
    );
    assert_eq!(reset.pinned, [] as [String; 0]);
    drop(client);
    server.shutdown().await.unwrap();
}

/// The Broker is offered unless the user says otherwise, and turning it off is
/// a pin like any other: it lands in the Config Document, reads back, and a
/// reset lets the built-in default resume.
#[tokio::test]
async fn broker_enabled_defaults_on_pins_through_the_config_document_and_resets() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-broker-enabled")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-broker-enabled").await;
    assert!(
        initial.settings.broker.enabled,
        "every Provider Session is offered the Broker unless the user turns it off"
    );

    let pinned = client
        .mutate_setting(SettingMutation::BrokerEnabled { value: Some(false) })
        .await
        .unwrap();
    assert!(!pinned.settings.broker.enabled);
    assert_eq!(pinned.pinned, ["broker.enabled"]);
    assert!(config_document(config_dir.path()).contains("\"broker\": {"));
    assert!(config_document(config_dir.path()).contains("\"enabled\": false"));

    let reset = client
        .mutate_setting(SettingMutation::BrokerEnabled { value: None })
        .await
        .unwrap();
    assert!(reset.settings.broker.enabled);
    assert_eq!(reset.pinned, [] as [String; 0]);
    drop(client);
    server.shutdown().await.unwrap();
}

/// A brokered Subagent may stand three Sessions deep unless the user says
/// otherwise — the top-level Session counting as the first — and a deeper or
/// shallower cap is a pin like any other.
#[tokio::test]
async fn broker_max_depth_defaults_to_three_pins_through_the_config_document_and_resets() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-broker-max-depth")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-broker-max-depth").await;
    assert_eq!(
        initial.settings.broker.max_depth, 3,
        "a tree reaches three Sessions deep through the Broker unless the user says otherwise"
    );

    let pinned = client
        .mutate_setting(SettingMutation::BrokerMaxDepth { value: Some(2) })
        .await
        .unwrap();
    assert_eq!(pinned.settings.broker.max_depth, 2);
    assert_eq!(pinned.pinned, ["broker.maxDepth"]);
    assert!(config_document(config_dir.path()).contains("\"broker\": {"));
    assert!(config_document(config_dir.path()).contains("\"maxDepth\": 2"));

    let reset = client
        .mutate_setting(SettingMutation::BrokerMaxDepth { value: None })
        .await
        .unwrap();
    assert_eq!(reset.settings.broker.max_depth, 3);
    assert_eq!(reset.pinned, [] as [String; 0]);
    drop(client);
    server.shutdown().await.unwrap();
}

/// Six brokered Subagents may work at once beneath a top-level Session unless
/// the user says otherwise, and a tighter or looser cap is a pin like any
/// other.
#[tokio::test]
async fn broker_max_concurrent_subagents_defaults_to_six_pins_through_the_config_document_and_resets()
 {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-broker-concurrency")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, initial) = attach(state_dir.path(), "settings-broker-concurrency").await;
    assert_eq!(
        initial.settings.broker.max_concurrent_subagents, 6,
        "six brokered Subagents may work at once unless the user says otherwise"
    );

    let pinned = client
        .mutate_setting(SettingMutation::BrokerMaxConcurrentSubagents { value: Some(12) })
        .await
        .unwrap();
    assert_eq!(pinned.settings.broker.max_concurrent_subagents, 12);
    assert_eq!(pinned.pinned, ["broker.maxConcurrentSubagents"]);
    assert!(config_document(config_dir.path()).contains("\"broker\": {"));
    assert!(config_document(config_dir.path()).contains("\"maxConcurrentSubagents\": 12"));

    let reset = client
        .mutate_setting(SettingMutation::BrokerMaxConcurrentSubagents { value: None })
        .await
        .unwrap();
    assert_eq!(reset.settings.broker.max_concurrent_subagents, 6);
    assert_eq!(reset.pinned, [] as [String; 0]);
    drop(client);
    server.shutdown().await.unwrap();
}

/// A Broker cap is typed rather than chosen from a list, so like the side
/// columns' widths it has no product-level maximum: a Config Document may ask
/// for as many as the count the Broker keeps can hold, and as few as one — a
/// tree of the top-level Session alone, or one brokered Subagent at a time.
#[tokio::test]
async fn broker_caps_load_without_a_product_level_maximum_and_pin_down_to_one() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{"broker": {"maxConcurrentSubagents": 4294967295, "maxDepth": 1000000}}"#,
    )
    .unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-broker-caps-unbounded")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, opening) = attach(state_dir.path(), "settings-broker-caps-unbounded").await;
    assert_eq!(opening.settings.broker.max_depth, 1_000_000);
    assert_eq!(
        opening.settings.broker.max_concurrent_subagents,
        u32::MAX,
        "the largest count the Broker keeps is a cap like any other"
    );
    assert_eq!(
        opening.pinned,
        ["broker.maxConcurrentSubagents", "broker.maxDepth"]
    );
    assert_eq!(opening.diagnostics, []);

    let depth = client
        .mutate_setting(SettingMutation::BrokerMaxDepth { value: Some(1) })
        .await
        .unwrap();
    assert_eq!(depth.settings.broker.max_depth, 1);
    let concurrency = client
        .mutate_setting(SettingMutation::BrokerMaxConcurrentSubagents { value: Some(1) })
        .await
        .unwrap();
    assert_eq!(concurrency.settings.broker.max_concurrent_subagents, 1);
    assert_eq!(
        concurrency.pinned,
        ["broker.maxConcurrentSubagents", "broker.maxDepth"]
    );
    assert_eq!(
        config_document(config_dir.path()),
        r#"{"broker": {"maxConcurrentSubagents": 1, "maxDepth": 1}}"#,
        "each pin replaces only its own count"
    );
    drop(client);
    server.shutdown().await.unwrap();
}

/// A Broker cap counts from one up to what the count it is kept in holds, so a
/// Config Document asking for none, for less than none, for part of one, for
/// something that is not a number, or for more than that count can hold is
/// ignored — each cap alone, with a diagnostic naming the bound — while the
/// rest of the document still applies.
#[tokio::test]
async fn a_broker_cap_outside_its_bound_is_ignored_alone_naming_the_bound() {
    for (case, invalid) in [
        ("zero", "0"),
        ("negative", "-1"),
        ("fraction", "2.5"),
        ("word", r#""three""#),
        ("past-u32", "4294967296"),
        ("absurd", "100000000000000000000"),
    ] {
        let state_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_dir.path().join("suru.jsonc"),
            format!(
                r#"{{"broker": {{"enabled": false, "maxConcurrentSubagents": {invalid}, "maxDepth": {invalid}}}}}"#
            ),
        )
        .unwrap();
        let channel = format!("settings-broker-caps-invalid-{case}");
        let server = server::spawn(
            ServerConfig::new(state_dir.path(), &channel)
                .unwrap()
                .with_config_dir(config_dir.path()),
        )
        .await
        .unwrap();

        let snapshot = attach(state_dir.path(), &channel).await.1;
        assert!(
            !snapshot.settings.broker.enabled,
            "{case} ignores only the invalid caps"
        );
        assert_eq!(snapshot.pinned, ["broker.enabled"], "{case}");
        assert_eq!(
            snapshot.settings.broker.max_depth, 3,
            "{case} keeps the built-in depth cap"
        );
        assert_eq!(
            snapshot.settings.broker.max_concurrent_subagents, 6,
            "{case} keeps the built-in concurrency cap"
        );
        let [concurrency, depth] = snapshot.diagnostics.as_slice() else {
            panic!(
                "{case} should diagnose each cap once: {:?}",
                snapshot.diagnostics
            );
        };
        for (diagnostic, key) in [
            (concurrency, "broker.maxConcurrentSubagents"),
            (depth, "broker.maxDepth"),
        ] {
            assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
            assert_eq!(diagnostic.key.as_deref(), Some(key), "{case}");
            assert_eq!(
                diagnostic.message,
                "ignored because its value is not an integer from 1 to 4294967295",
                "{case} names the bound {key} takes"
            );
        }

        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn an_unset_keeps_the_comments_that_outlive_the_pin_it_removes() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), HAND_WRITTEN)
        .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-unset-comments")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-unset-comments").await;

    let snapshot = client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture { value: None })
        .await
        .expect("remove the pin");

    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Folded
    );
    assert_eq!(snapshot.pinned, ["provider.codex.reasoningSummary"]);
    let document = config_document(config_dir.path());
    assert!(
        document.contains("// I read my Sessions folded."),
        "a comment about the removed pin is the author's to delete, not Suru's: {document:?}"
    );
    assert!(
        document.contains("\"reasoningSummary\":    \"detailed\""),
        "the untouched Setting keeps its spacing: {document:?}"
    );
    assert!(
        !document.contains("defaultFoldPosture"),
        "the pin itself is gone: {document:?}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_first_set_creates_the_config_document_under_the_config_root() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_home = tempfile::tempdir().expect("create isolated config home");
    let config_dir = config_home.path().join("suru");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-create")
            .expect("configure server")
            .with_config_dir(&config_dir),
    )
    .await
    .expect("spawn server");
    let (client, opening) = attach(state_dir.path(), "settings-create").await;
    assert_eq!(opening.pinned, [] as [String; 0]);
    assert!(!config_dir.exists(), "nothing scaffolds the file up front");

    client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect("set the Setting");

    assert_eq!(
        config_document(&config_dir),
        "{\n  \"transcript\": {\n    \"defaultFoldPosture\": \"expanded\"\n  }\n}\n"
    );
    assert!(
        !config_dir.join("suru.json").exists(),
        "Suru creates the primary Config Document, never the fallback name"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_document_that_does_not_parse_is_refused_rather_than_rewritten() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let broken = r#"{ "transcript": { "defaultFoldPosture" "#;
    std::fs::write(config_dir.path().join("suru.jsonc"), broken).expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-refuse")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-refuse").await;

    let refusal = client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect_err("a document Suru cannot read is a document Suru cannot edit");

    assert!(
        refusal.to_string().contains("suru.jsonc"),
        "the refusal names the file standing in the way: {refusal}"
    );
    assert_eq!(
        config_document(config_dir.path()),
        broken,
        "Suru never rewrites a document to fix it"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn without_a_config_root_there_is_nowhere_to_pin_a_setting() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-rootless").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-rootless").await;

    let refusal = client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .await
        .expect_err("without a config root a mutation has nowhere to land");

    assert!(
        refusal.to_string().contains("config"),
        "the refusal says what is missing: {refusal}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_unauthenticated_mutation_never_reaches_the_config_document() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-unauthenticated")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let refused = reqwest::Client::new()
        .post(format!("{}/v1/settings", descriptor.base_url))
        .json(&SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Expanded),
        })
        .send()
        .await
        .expect("request a mutation without authentication");

    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(
        !config_dir.path().join("suru.jsonc").exists(),
        "an unauthenticated command writes nothing"
    );

    server.shutdown().await.expect("shut down server");
}

/// Provider Enablement is the first boolean Setting, so these hold the line on
/// a JSON boolean being first-class: pinned, reset, and diagnosed as `true` or
/// `false` rather than as quoted strings.
#[tokio::test]
async fn a_provider_pinned_off_round_trips_and_the_rest_default_to_enabled() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I only ever work with Codex.
            "provider": {
                "copilot": { "enabled": false },
            },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-provider-off")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-provider-off").await.1;
    assert!(!snapshot.settings.provider.copilot.enabled);
    assert!(
        snapshot.settings.provider.codex.enabled && snapshot.settings.provider.claude.enabled,
        "a Provider with no pin is enabled, so a fresh install works without finding the Setting"
    );
    assert_eq!(snapshot.pinned, ["provider.copilot.enabled"]);
    assert_eq!(snapshot.diagnostics, []);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn turning_a_provider_off_and_resetting_it_leaves_the_rest_of_the_document_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(config_dir.path().join("suru.jsonc"), HAND_WRITTEN)
        .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-provider-toggle")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-provider-toggle").await;

    let disabled = client
        .mutate_setting(SettingMutation::ProviderClaudeEnabled { value: Some(false) })
        .await
        .expect("turn the Provider off");
    assert!(!disabled.settings.provider.claude.enabled);
    assert!(
        disabled
            .pinned
            .contains(&"provider.claude.enabled".to_owned())
    );
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"enabled\": false"),
        "the pin is a real JSON boolean rather than a quoted string: {document:?}"
    );
    assert!(
        document.contains("\"reasoningSummary\":    \"detailed\""),
        "the untouched Setting keeps its spacing: {document:?}"
    );
    assert!(
        document.contains("// I read my Sessions folded."),
        "the author's comments survive: {document:?}"
    );

    let reset = client
        .mutate_setting(SettingMutation::ProviderClaudeEnabled { value: None })
        .await
        .expect("undo the choice");
    assert!(
        reset.settings.provider.claude.enabled,
        "removing the pin lets the built-in default resume"
    );
    assert_eq!(
        config_document(config_dir.path()),
        HAND_WRITTEN,
        "the round trip leaves the document byte for byte as its author wrote it"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn pinning_a_provider_on_explicitly_still_writes_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-provider-on")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-provider-on").await;

    let pinned = client
        .mutate_setting(SettingMutation::ProviderCodexEnabled { value: Some(true) })
        .await
        .expect("pin the built-in default");

    assert!(pinned.settings.provider.codex.enabled);
    assert_eq!(
        pinned.pinned,
        ["provider.codex.enabled"],
        "a deliberate choice is pinned even when it equals the default"
    );
    assert_eq!(
        config_document(config_dir.path()),
        "{\n  \"provider\": {\n    \"codex\": {\n      \"enabled\": true\n    }\n  }\n}\n"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_mistyped_enablement_is_diagnosed_with_unquoted_booleans_and_ignored_alone() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "provider": {
                "codex": { "enabled": "no" },
                "copilot": { "enabled": false },
                "gemini": { "enabled": false }
            }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-provider-mistyped")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-provider-mistyped")
        .await
        .1;
    assert!(
        snapshot.settings.provider.codex.enabled,
        "the mistyped Setting keeps its built-in default"
    );
    assert!(
        !snapshot.settings.provider.copilot.enabled,
        "one mistake does not void the rest of the document"
    );
    assert_eq!(snapshot.pinned, ["provider.copilot.enabled"]);

    let diagnostics: Vec<_> = snapshot
        .diagnostics
        .iter()
        .map(|diagnostic| {
            assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
            (
                diagnostic.key.as_deref().expect("a per-key diagnostic"),
                diagnostic.message.as_str(),
            )
        })
        .collect();
    let [(codex_key, codex_message), (gemini_key, _)] = diagnostics.as_slice() else {
        panic!("expected two diagnostics, got {diagnostics:?}");
    };
    assert_eq!(*codex_key, "provider.codex.enabled");
    assert!(
        codex_message.contains("one of true or false"),
        "the message tells the reader what to actually type, got {codex_message:?}"
    );
    assert!(
        !codex_message.contains("\"true\""),
        "a boolean is not spelled as a quoted string, got {codex_message:?}"
    );
    assert_eq!(
        *gemini_key, "provider.gemini",
        "a Provider that does not exist is visible rather than silent"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn turning_a_provider_off_in_one_client_reaches_every_other() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-provider-broadcast")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (mut editor, _) = attach(state_dir.path(), "settings-provider-broadcast").await;
    let (mut onlooker, _) = attach(state_dir.path(), "settings-provider-broadcast").await;

    let answered = editor
        .mutate_setting(SettingMutation::ProviderCopilotEnabled { value: Some(false) })
        .await
        .expect("turn the Provider off");

    for client in [&mut editor, &mut onlooker] {
        let pushed = next_snapshot(client).await;
        assert_eq!(
            pushed, answered,
            "every view agrees on which Providers exist"
        );
        assert!(!pushed.settings.provider.copilot.enabled);
    }

    drop(editor);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

/// The Agent Selection a reader pins for Title derivation, which is the value
/// no fixed choice list could have named.
fn pinned_title_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-5-mini"),
        options: Vec::new(),
    }
}

/// Title, Icon, and branch derivation is the first Setting whose values the
/// schema cannot enumerate, so these hold the line on a value richer than a
/// word: pinned from a document, pinned by an edit, and diagnosed by naming
/// the values that do have words alongside a description of the one that does
/// not.
#[tokio::test]
async fn a_pinned_derivation_errand_selection_round_trips_through_a_config_document() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Every Title and Icon written by the cheap Model, whatever the Session uses.
            "derivation": {
                "errand": {
                    "provider": "codex",
                    "model": "gpt-5-mini",
                    "options": [],
                },
            },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-derivation-pin")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-derivation-pin").await;
    assert_eq!(
        opening.settings.derivation.errand,
        DerivationErrand::Pinned(pinned_title_selection())
    );
    assert_eq!(opening.pinned, ["derivation.errand"]);
    assert_eq!(opening.diagnostics, []);

    let off = client
        .mutate_setting(SettingMutation::DerivationErrand {
            value: Some(DerivationErrand::Off),
        })
        .await
        .expect("turn Title, Icon, and branch derivation off");
    assert_eq!(off.settings.derivation.errand, DerivationErrand::Off);
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"errand\": \"off\""),
        "a named value is pinned as the one word a reader would type: {document:?}"
    );

    let repinned = client
        .mutate_setting(SettingMutation::DerivationErrand {
            value: Some(DerivationErrand::Pinned(pinned_title_selection())),
        })
        .await
        .expect("pin an Agent Selection again");
    assert_eq!(
        repinned.settings.derivation.errand,
        DerivationErrand::Pinned(pinned_title_selection()),
        "an Agent Selection survives the round trip through the document"
    );

    let reset = client
        .mutate_setting(SettingMutation::DerivationErrand { value: None })
        .await
        .expect("remove the pin");
    assert_eq!(
        reset.settings.derivation.errand,
        DerivationErrand::FollowSession,
        "the built-in default follows the Session's own Provider"
    );
    assert_eq!(reset.pinned, [] as [String; 0]);
    let emptied = config_document(config_dir.path());
    assert!(
        !emptied.contains("derivation"),
        "the pin takes the object that existed only to hold it with it: {emptied:?}"
    );
    assert!(
        emptied.contains("// Every Title and Icon written by the cheap Model"),
        "the author's comment about the removed pin is theirs to delete: {emptied:?}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_mistyped_derivation_errand_is_ignored_alone_and_says_what_it_accepts() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "derivation": { "errand": "sometimes" },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-derivation-mistyped")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-derivation-mistyped")
        .await
        .1;
    assert_eq!(
        snapshot.settings.derivation.errand,
        DerivationErrand::FollowSession,
        "the mistyped Setting keeps its built-in default"
    );
    assert_eq!(
        snapshot.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "one mistake does not void the rest of the document"
    );
    assert_eq!(snapshot.pinned, ["transcript.reasoningVisibility"]);

    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(diagnostic.file, config_dir.path().join("suru.jsonc"));
    assert_eq!(diagnostic.key.as_deref(), Some("derivation.errand"));
    assert!(
        diagnostic
            .message
            .contains("one of \"session\", \"off\", or an Agent Selection"),
        "a Setting the schema cannot enumerate names what it can and describes the rest: {:?}",
        diagnostic.message
    );

    server.shutdown().await.expect("shut down server");
}

/// The retired `session` › `title` › `errand` key names no Setting any more:
/// renaming it to `derivation.errand` (so it plainly covers the Icon it now
/// also governs) leaves the old path recognized only as far as `session`
/// itself, a group other Settings still live under.
#[tokio::test]
async fn the_retired_title_derivation_key_is_no_longer_a_known_setting() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{ "session": { "title": { "errand": "off" } } }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-derivation-retired-key")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-derivation-retired-key")
        .await
        .1;
    assert_eq!(
        snapshot.settings.derivation.errand,
        DerivationErrand::FollowSession,
        "the retired key pins nothing, so the built-in default stands"
    );
    assert_eq!(snapshot.pinned, [] as [String; 0]);
    let [diagnostic] = snapshot.diagnostics.as_slice() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics);
    };
    assert_eq!(diagnostic.severity, SettingsDiagnosticSeverity::Warning);
    assert_eq!(diagnostic.key.as_deref(), Some("session.title"));
    assert!(
        diagnostic.message.contains("not a known Setting"),
        "the retired path is diagnosed exactly like any other unknown key: {:?}",
        diagnostic.message
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_reset_on_a_fresh_install_leaves_the_config_root_empty() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-reset-fresh")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");
    let (client, _) = attach(state_dir.path(), "settings-reset-fresh").await;

    let snapshot = client
        .mutate_setting(SettingMutation::TranscriptDefaultFoldPosture { value: None })
        .await
        .expect("remove a pin that was never written");

    assert_eq!(snapshot.settings, Default::default());
    assert_eq!(snapshot.pinned, [] as [String; 0]);
    assert!(
        !config_dir.path().join("suru.jsonc").exists(),
        "an edit with nothing to remove writes no document"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn text_selection_copy_pins_resets_and_reaches_every_client() {
    use suru::protocol::TextSelectionCopy;
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{"textSelection": {"copy": "manual"}}"#,
    )
    .unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "selection-copy")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (mut editor, opening) = attach(state_dir.path(), "selection-copy").await;
    let (mut onlooker, second) = attach(state_dir.path(), "selection-copy").await;
    assert_eq!(opening, second);
    assert_eq!(
        opening.settings.text_selection.copy,
        TextSelectionCopy::Manual
    );
    assert_eq!(opening.pinned, ["textSelection.copy"]);
    assert!(opening.diagnostics.is_empty());
    let pinned = editor
        .mutate_setting(SettingMutation::TextSelectionCopy {
            value: Some(TextSelectionCopy::Release),
        })
        .await
        .unwrap();
    assert_eq!(
        pinned.settings.text_selection.copy,
        TextSelectionCopy::Release
    );
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, pinned);
    }
    let reset = editor
        .mutate_setting(SettingMutation::TextSelectionCopy { value: None })
        .await
        .unwrap();
    #[cfg(windows)]
    assert_eq!(
        reset.settings.text_selection.copy,
        TextSelectionCopy::Manual
    );
    #[cfg(not(windows))]
    assert_eq!(
        reset.settings.text_selection.copy,
        TextSelectionCopy::Release
    );
    assert!(reset.pinned.is_empty());
    for client in [&mut editor, &mut onlooker] {
        assert_eq!(next_snapshot(client).await, reset);
    }
    let (late, snapshot) = attach(state_dir.path(), "selection-copy").await;
    assert_eq!(snapshot, reset);
    drop((editor, onlooker, late));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn text_selection_copy_rejects_unknown_values_naming_both_choices() {
    let state_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{"textSelection":{"copy":"never"}}"#,
    )
    .unwrap();
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "selection-invalid")
            .unwrap()
            .with_config_dir(config_dir.path()),
    )
    .await
    .unwrap();
    let (client, snapshot) = attach(state_dir.path(), "selection-invalid").await;
    assert!(snapshot.pinned.is_empty());
    assert_eq!(snapshot.settings, Default::default());
    assert_eq!(snapshot.diagnostics.len(), 1);
    let diagnostic = &snapshot.diagnostics[0];
    assert_eq!(diagnostic.key.as_deref(), Some("textSelection.copy"));
    assert!(
        diagnostic
            .message
            .contains("one of \"release\" or \"manual\""),
        "{diagnostic:?}"
    );
    drop(client);
    server.shutdown().await.unwrap();
}
