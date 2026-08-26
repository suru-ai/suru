//! Config Documents load at server startup, the effective-settings snapshot
//! reaches every connecting client, and typed mutations edit the document in
//! place, per the user configuration spec: a spawned server driven through the
//! managed client over the real protocol, against temp config directories only.

use std::path::Path;

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        AgentSelection, FoldPosture, ModelId, ProviderId, ReasoningSummaryDetail,
        ReasoningVisibility, SessionContentWidth, SettingMutation, SettingsDiagnosticSeverity,
        SettingsSnapshot, SidebarVisibility, TitleErrand,
    },
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

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

async fn next_snapshot(client: &mut ManagedClient) -> SettingsSnapshot {
    let event = timeout(Duration::from_secs(1), client.next())
        .await
        .expect("settings snapshot arrives")
        .expect("managed client remains open");
    let ManagedEvent::SettingsSnapshot(snapshot) = event else {
        panic!("expected a settings snapshot event, got {event:?}");
    };
    snapshot
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

#[tokio::test]
async fn a_hidden_sidebar_pins_from_a_document_and_resets_to_the_shown_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I would rather have the columns.
            "sidebar": { "launchVisibility": "hidden" },
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
        opening.settings.sidebar.launch_visibility,
        SidebarVisibility::Hidden
    );
    assert_eq!(opening.pinned, ["sidebar.launchVisibility"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::SidebarLaunchVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.sidebar.launch_visibility,
        SidebarVisibility::Shown,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
    server.shutdown().await.expect("shut down server");
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

/// Title derivation is the first Setting whose values the schema cannot
/// enumerate, so these hold the line on a value richer than a word: pinned from
/// a document, pinned by an edit, and diagnosed by naming the values that do
/// have words alongside a description of the one that does not.
#[tokio::test]
async fn a_pinned_title_errand_selection_round_trips_through_a_config_document() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Every Title written by the cheap Model, whatever the Session uses.
            "session": {
                "title": {
                    "errand": {
                        "provider": "codex",
                        "model": "gpt-5-mini",
                        "options": [],
                    },
                },
            },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-title-pin")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let (client, opening) = attach(state_dir.path(), "settings-title-pin").await;
    assert_eq!(
        opening.settings.session.title.errand,
        TitleErrand::Pinned(pinned_title_selection())
    );
    assert_eq!(opening.pinned, ["session.title.errand"]);
    assert_eq!(opening.diagnostics, []);

    let off = client
        .mutate_setting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::Off),
        })
        .await
        .expect("turn Title derivation off");
    assert_eq!(off.settings.session.title.errand, TitleErrand::Off);
    let document = config_document(config_dir.path());
    assert!(
        document.contains("\"errand\": \"off\""),
        "a named value is pinned as the one word a reader would type: {document:?}"
    );

    let repinned = client
        .mutate_setting(SettingMutation::SessionTitleErrand {
            value: Some(TitleErrand::Pinned(pinned_title_selection())),
        })
        .await
        .expect("pin an Agent Selection again");
    assert_eq!(
        repinned.settings.session.title.errand,
        TitleErrand::Pinned(pinned_title_selection()),
        "an Agent Selection survives the round trip through the document"
    );

    let reset = client
        .mutate_setting(SettingMutation::SessionTitleErrand { value: None })
        .await
        .expect("remove the pin");
    assert_eq!(
        reset.settings.session.title.errand,
        TitleErrand::FollowSession,
        "the built-in default follows the Session's own Provider"
    );
    assert_eq!(reset.pinned, [] as [String; 0]);
    let emptied = config_document(config_dir.path());
    assert!(
        !emptied.contains("session") && !emptied.contains("title"),
        "the pin takes the objects that existed only to hold it with it: {emptied:?}"
    );
    assert!(
        emptied.contains("// Every Title written by the cheap Model"),
        "the author's comment about the removed pin is theirs to delete: {emptied:?}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_mistyped_title_errand_is_ignored_alone_and_says_what_it_accepts() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            "session": { "title": { "errand": "sometimes" } },
            "transcript": { "reasoningVisibility": "shown" }
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "settings-title-mistyped")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
    )
    .await
    .expect("spawn server");

    let snapshot = attach(state_dir.path(), "settings-title-mistyped").await.1;
    assert_eq!(
        snapshot.settings.session.title.errand,
        TitleErrand::FollowSession,
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
    assert_eq!(diagnostic.key.as_deref(), Some("session.title.errand"));
    assert!(
        diagnostic
            .message
            .contains("one of \"session\", \"off\", or an Agent Selection"),
        "a Setting the schema cannot enumerate names what it can and describes the rest: {:?}",
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
