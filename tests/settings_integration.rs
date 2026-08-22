//! Config Documents load at server startup and the effective-settings
//! snapshot reaches every connecting client, per the user configuration spec:
//! a spawned server driven through the managed client over the real protocol,
//! against temp config directories only.

use std::path::Path;

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{FoldPosture, ReasoningSummaryDetail, SettingsDiagnosticSeverity, SettingsSnapshot},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

async fn connect_and_receive_snapshot(state_dir: &Path, channel: &str) -> SettingsSnapshot {
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
        let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-pin").await;
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-json").await;
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-duplicate").await;
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-broken").await;
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-perkey").await;
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-defaults").await;
    assert_eq!(snapshot.settings, Default::default());
    assert_eq!(
        snapshot.settings.transcript.default_fold_posture,
        FoldPosture::Folded
    );
    assert_eq!(
        snapshot.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto
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

    let snapshot = connect_and_receive_snapshot(state_dir.path(), "settings-flat").await;
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
