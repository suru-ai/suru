//! Config Documents load at server startup, the effective-settings snapshot
//! reaches every connecting client, and typed mutations edit the document in
//! place, per the user configuration spec: a spawned server driven through the
//! managed client over the real protocol, against temp config directories only.

use std::path::Path;

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        FoldPosture, ReasoningSummaryDetail, ReasoningVisibility, SettingMutation,
        SettingsDiagnosticSeverity, SettingsSnapshot,
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
async fn hiding_reasoning_pins_from_a_document_and_resets_to_the_default() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // I do not want to read the model think.
            "transcript": { "reasoningVisibility": "hidden" },
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
        ReasoningVisibility::Hidden
    );
    assert_eq!(
        opening.settings.provider.codex.reasoning_summary,
        ReasoningSummaryDetail::Auto,
        "hiding Reasoning is presentation and asks Codex for no less of it"
    );
    assert_eq!(opening.pinned, ["transcript.reasoningVisibility"]);
    assert_eq!(opening.diagnostics, []);

    let answered = client
        .mutate_setting(SettingMutation::TranscriptReasoningVisibility { value: None })
        .await
        .expect("reset the Setting");
    assert_eq!(
        answered.settings.transcript.reasoning_visibility,
        ReasoningVisibility::Shown,
        "unpinning it lets the built-in default resume"
    );
    assert_eq!(answered.pinned, [] as [String; 0]);

    drop(client);
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

fn config_document(config_dir: &Path) -> String {
    std::fs::read_to_string(config_dir.join("suru.jsonc")).expect("read the Config Document")
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
