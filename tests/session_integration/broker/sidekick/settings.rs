//! `list_settings`, `describe_setting` and `set_setting`: a Sidekick reading
//! and changing the Settings of its own Server (#479).
//!
//! A Sidekick lists every Setting by its key and the value in force,
//! describes one by its own words and what it accepts, and sets one by a
//! value it accepts, or removes its pin. A change goes through the very
//! operation the settings panel's does: the Config Document is edited in
//! place, the rest of it left as its author wrote it, and the change takes
//! effect and reaches every attached Client, outliving a restart — whole, one
//! change at a time, and whether or not the Sidekick waits for its answer.
//! What a Sidekick may not touch is bounded where the Tools are served (ADR
//! 0043), by what each Setting declares of a Sidekick: the Settings governing
//! Serving and Pairing are never listed, described or set, nor their values
//! told — not even in why Serving could not follow a change — and those
//! bounding what Agents may do or doing what cannot be undone are read but
//! never changed. Any other caller neither lists the Tools nor may call them.
//!
//! Each test acts as the MCP client a Sidekick's harness is and asserts on
//! what the Tools answer it, on the Config Document, and on what a Client of
//! the Session API observes.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{AppearanceMode, ClaudePermissionMode, FoldPosture, SettingsSnapshot},
    settings::{SCHEMA, SettingDescriptor, SidekickAccess},
};

use super::*;
use crate::broker::sidekick_acts::{acted, refused};

/// The Tools through which a Sidekick reads and changes Settings.
const SETTINGS_TOOLS: [&str; 3] = ["list_settings", "describe_setting", "set_setting"];

/// A Config Document as a user writes one by hand: comments, tabs, a trailing
/// comma, Settings in no order of Suru's, and a Serving port of their own.
const HAND_WRITTEN: &str = "{\n\t// Codex should say as much about its thinking as it likes.\n\t\"provider\": {\n\t\t\"codex\": { \"reasoningSummary\":    \"detailed\" }\n\t},\n\n\t// I read my Sessions folded.\n\t\"transcript\": { \"defaultFoldPosture\": \"folded\" },\n\t\"serving\": { \"port\": 9443 },\n}\n";

/// The Config Document under `config_dir`, or `None` where there is none.
fn config_document(config_dir: &Path) -> Option<String> {
    std::fs::read_to_string(config_dir.join("suru.jsonc")).ok()
}

fn write_config_document(config_dir: &Path, text: &str) {
    std::fs::write(config_dir.join("suru.jsonc"), text).expect("write the Config Document");
}

/// A Client attached to the Server of `channel`, past what it is told on
/// attaching, answering with the Settings it was told were in force.
async fn attached_client(state_dir: &Path, channel: &str) -> (ManagedClient, SettingsSnapshot) {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure the Client"),
    )
    .await
    .expect("connect the Client");
    let opening = next_settings(&mut client).await;
    (client, opening)
}

/// The next Settings `client` is told are in force, read past everything
/// else it is told.
async fn next_settings(client: &mut ManagedClient) -> SettingsSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            match client.next().await {
                Some(ManagedEvent::SettingsSnapshot(snapshot)) => return snapshot,
                Some(_) => {}
                None => panic!("the Client stays attached"),
            }
        }
    })
    .await
    .expect("the Client is told the Settings in force")
}

/// `list_settings`' rows for `arguments`, in its order.
async fn list_settings(client: &mut McpClient, arguments: Value) -> Vec<Value> {
    acted(client, "list_settings", arguments).await["settings"]
        .as_array()
        .expect("a listing lists Settings")
        .clone()
}

/// The keys of `rows`, in their order.
fn keys(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["key"].as_str().expect("every row has its key"))
        .collect()
}

/// The value the row for `key` gives.
fn listed_value<'a>(rows: &'a [Value], key: &str) -> &'a Value {
    &rows
        .iter()
        .find(|row| row["key"] == json!(key))
        .unwrap_or_else(|| panic!("{key} is listed: {rows:?}"))["value"]
}

async fn describe(client: &mut McpClient, key: &str) -> Value {
    acted(client, "describe_setting", json!({ "key": key })).await
}

async fn set(client: &mut McpClient, arguments: Value) -> Value {
    acted(client, "set_setting", arguments).await
}

/// Every Setting whose declared Sidekick access `matches` says.
fn declaring(matches: impl Fn(SidekickAccess) -> bool) -> Vec<&'static SettingDescriptor> {
    SCHEMA
        .iter()
        .filter(|descriptor| matches(descriptor.sidekick))
        .collect()
}

/// The next Settings `client` is told are in force that `holds` says of.
async fn settings_until(
    client: &mut ManagedClient,
    holds: impl Fn(&SettingsSnapshot) -> bool,
) -> SettingsSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let told = next_settings(client).await;
            if holds(&told) {
                return told;
            }
        }
    })
    .await
    .expect("the Client is told of the Settings it waits for")
}

#[tokio::test]
async fn list_settings_lists_every_setting_but_servings_by_key_and_value_in_force() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    write_config_document(
        config_dir.path(),
        "{ \"appearance\": { \"mode\": \"dark\" }, \"serving\": { \"port\": 9443 } }\n",
    );
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-list-settings",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    let rows = list_settings(&mut sidekick, json!({})).await;
    let readable = SCHEMA
        .iter()
        .filter(|setting| setting.sidekick != SidekickAccess::Hidden)
        .map(|setting| setting.key)
        .collect::<Vec<_>>();
    assert_eq!(
        keys(&rows),
        readable,
        "every Setting the schema declares is listed, a new one with it, but Serving's"
    );
    assert!(
        rows.iter()
            .all(|row| row.as_object().map(|row| row.len()) == Some(2)),
        "each row is a key and a value, no more: {rows:?}"
    );
    assert_eq!(listed_value(&rows, "appearance.mode"), &json!("dark"));
    assert_eq!(
        listed_value(&rows, "appearance.theme"),
        &json!("system"),
        "a Setting nothing pins lists its built-in default"
    );
    assert_eq!(listed_value(&rows, "broker.maxDepth"), &json!(3));
    assert_eq!(listed_value(&rows, "provider.claude.enabled"), &json!(true));
    let listed = serde_json::to_string(&rows).expect("rows serialize");
    assert!(
        !listed.contains("serving") && !listed.contains("9443"),
        "nothing of Serving is told, not even the port the user pinned: {listed}"
    );

    assert_eq!(
        list_settings(&mut sidekick, json!({ "group": "appearance" })).await,
        vec![
            json!({ "key": "appearance.theme", "value": "system" }),
            json!({ "key": "appearance.mode", "value": "dark" }),
            json!({ "key": "appearance.landingPage", "value": "Minimal" }),
            json!({ "key": "appearance.showIcons", "value": false }),
        ],
        "a group lists its own Settings alone"
    );
    assert_eq!(
        keys(&list_settings(&mut sidekick, json!({ "group": "experimental" })).await),
        [
            "broker.enabled",
            "broker.maxDepth",
            "broker.maxConcurrentSubagents"
        ],
        "and the Serving Settings in the same group are left out of it"
    );
    let refusal = refused(
        &mut sidekick,
        "list_settings",
        json!({ "group": "serving" }),
    )
    .await;
    assert_eq!(
        refusal,
        "list_settings' `group` must be one of `appearance`, `general`, `transcript`, \
         `providers`, `source_control`, `experimental`; `serving` is none of them."
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn describe_setting_says_what_each_kind_of_setting_holds_and_accepts() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    write_config_document(
        config_dir.path(),
        concat!(
            "{\n",
            "  \"appearance\": { \"mode\": \"dark\" },\n",
            "  \"derivation\": { \"errand\": { \"provider\": \"claude\", \"model\": \"claude-opus-4-5\", \"options\": [] } },\n",
            "  \"sidebar\": { \"initialWidth\": 40 }\n",
            "}\n",
        ),
    );
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-describe-settings",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    assert_eq!(
        describe(&mut sidekick, "appearance.showIcons").await,
        json!({
            "key": "appearance.showIcons",
            "label": "Show icons",
            "description": "Show Nerd Font icons. Requires a Nerd Font in your terminal.",
            "group": "appearance",
            "scope": "client",
            "value": false,
            "default": false,
            "pinned": false,
            "accepts": "one of false or true",
            "settable": true,
        }),
        "a toggle the user left at its default"
    );
    assert_eq!(
        describe(&mut sidekick, "appearance.mode").await,
        json!({
            "key": "appearance.mode",
            "label": "Mode",
            "description": "Whether Themes follow the terminal or use a dark or light variant",
            "group": "appearance",
            "scope": "client",
            "value": "dark",
            "default": "system",
            "pinned": true,
            "accepts": "one of \"system\", \"dark\", or \"light\"",
            "settable": true,
        }),
        "a choice the user pinned"
    );
    let width = describe(&mut sidekick, "sidebar.initialWidth").await;
    assert_eq!(
        (
            &width["value"],
            &width["default"],
            &width["pinned"],
            &width["accepts"]
        ),
        (
            &json!(40),
            &json!(32),
            &json!(true),
            &json!("an integer of at least 24")
        ),
        "a number: {width}"
    );
    let theme = describe(&mut sidekick, "appearance.theme").await;
    assert_eq!(
        (&theme["value"], &theme["pinned"], &theme["accepts"]),
        (
            &json!("system"),
            &json!(false),
            &json!("one of \"system\" or a Theme name")
        ),
        "text, which names what else it takes: {theme}"
    );
    let errand = describe(&mut sidekick, "derivation.errand").await;
    assert_eq!(
        (
            &errand["value"],
            &errand["default"],
            &errand["scope"],
            &errand["accepts"]
        ),
        (
            &json!({ "provider": "claude", "model": "claude-opus-4-5", "options": [] }),
            &json!("session"),
            &json!("server"),
            &json!("one of \"session\", \"off\", or an Agent Selection")
        ),
        "a Server Setting holding an Agent Selection, as the Config Document spells it: {errand}"
    );
    let reclaim = describe(&mut sidekick, "worktree.autoReclaim").await;
    assert_eq!(
        (
            &reclaim["group"],
            &reclaim["scope"],
            &reclaim["value"],
            &reclaim["settable"]
        ),
        (
            &json!("source_control"),
            &json!("server"),
            &json!(14),
            &json!(false)
        ),
        "a Setting whose effect cannot be undone is read, and said to be no Sidekick's to \
         change: {reclaim}"
    );
    let posture = describe(&mut sidekick, "provider.claude.permissionMode").await;
    assert_eq!(
        (&posture["value"], &posture["settable"]),
        (&json!("default"), &json!(false)),
        "a Setting an Approval Posture is made of is read, and said to be no Sidekick's to \
         change: {posture}"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn set_setting_edits_the_config_document_in_place_as_the_panel_does_and_reaches_every_client()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    write_config_document(config_dir.path(), HAND_WRITTEN);
    let channel = "sidekick-set-client-setting";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let (mut client, opening) = attached_client(state_dir.path(), channel).await;
    let (mut onlooker, _) = attached_client(state_dir.path(), channel).await;
    assert_eq!(
        opening.settings.transcript.default_fold_posture,
        FoldPosture::Folded
    );

    assert_eq!(
        set(
            &mut sidekick,
            json!({ "key": "transcript.defaultFoldPosture", "value": "expanded" })
        )
        .await,
        json!({ "key": "transcript.defaultFoldPosture", "value": "expanded", "pinned": true })
    );
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some(HAND_WRITTEN.replace("\"folded\"", "\"expanded\"").as_str()),
        "only the value set changes, every other byte left as the user wrote it"
    );
    for attached in [&mut client, &mut onlooker] {
        let told = next_settings(attached).await;
        assert_eq!(
            told.settings.transcript.default_fold_posture,
            FoldPosture::Expanded,
            "every attached Client is told of the change as it is of the panel's"
        );
    }

    // A Setting the document never pinned lands where the panel's change of
    // it lands, byte for byte.
    write_config_document(config_dir.path(), HAND_WRITTEN);
    set(
        &mut sidekick,
        json!({ "key": "appearance.mode", "value": "light" }),
    )
    .await;
    let sidekicks = config_document(config_dir.path()).expect("the document is still there");
    for attached in [&mut client, &mut onlooker] {
        let told = next_settings(attached).await;
        assert_eq!(told.settings.appearance.mode, AppearanceMode::Light);
        assert!(
            told.pinned.contains(&"appearance.mode".to_owned()),
            "{:?}",
            told.pinned
        );
    }
    write_config_document(config_dir.path(), HAND_WRITTEN);
    client
        .mutate_setting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::Light),
        })
        .await
        .expect("the panel sets the Setting");
    assert_eq!(
        config_document(config_dir.path()),
        Some(sidekicks),
        "a Sidekick's change and the panel's leave the same document"
    );
    next_settings(&mut client).await;
    next_settings(&mut onlooker).await;

    // Removing the pin leaves what the panel's removal of it leaves, too.
    let pinned = config_document(config_dir.path()).expect("the document is still there");
    assert_eq!(
        set(&mut sidekick, json!({ "key": "appearance.mode" })).await,
        json!({ "key": "appearance.mode", "value": "system", "pinned": false }),
        "no value removes the pin, and the built-in default is in force again"
    );
    let sidekicks = config_document(config_dir.path()).expect("the document is still there");
    assert!(
        !sidekicks.contains("appearance"),
        "the pin is taken back out of the document: {sidekicks}"
    );
    for attached in [&mut client, &mut onlooker] {
        let told = next_settings(attached).await;
        assert_eq!(told.settings.appearance.mode, AppearanceMode::System);
        assert!(!told.pinned.contains(&"appearance.mode".to_owned()));
    }
    write_config_document(config_dir.path(), &pinned);
    client
        .mutate_setting(SettingMutation::AppearanceMode { value: None })
        .await
        .expect("the panel removes the pin");
    assert_eq!(config_document(config_dir.path()), Some(sidekicks));
    next_settings(&mut client).await;
    next_settings(&mut onlooker).await;

    assert_eq!(
        set(
            &mut sidekick,
            json!({ "key": "transcript.defaultFoldPosture", "value": null })
        )
        .await,
        json!({ "key": "transcript.defaultFoldPosture", "value": "folded", "pinned": false }),
        "and so does a null one"
    );
    let left = config_document(config_dir.path()).expect("the document is still there");
    assert!(
        left.contains("// Codex should say as much about its thinking as it likes.")
            && left.contains("\"codex\": { \"reasoningSummary\":    \"detailed\" }")
            && left.contains("\"serving\": { \"port\": 9443 }")
            && !left.contains("defaultFoldPosture"),
        "removing a pin leaves the rest of the document alone: {left}"
    );
    for attached in [&mut client, &mut onlooker] {
        assert_eq!(
            next_settings(attached)
                .await
                .settings
                .transcript
                .default_fold_posture,
            FoldPosture::Folded
        );
    }

    drop(client);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_server_setting_set_by_a_sidekick_takes_effect_and_outlives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "sidekick-set-server-setting";
    let mut hosted = host_providers(state_dir.path(), channel, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (mut client, _) = attached_client(state_dir.path(), channel).await;
    let codex = |listing: &Value| {
        listing["providers"]
            .as_array()
            .and_then(|providers| providers.iter().find(|provider| provider["id"] == "codex"))
            .map(|codex| codex["enabled"].clone())
    };
    assert_eq!(codex(&sidekick.list_providers().await), Some(json!(true)));

    assert_eq!(
        set(
            &mut sidekick,
            json!({ "key": "provider.codex.enabled", "value": false })
        )
        .await,
        json!({ "key": "provider.codex.enabled", "value": false, "pinned": true })
    );
    assert_eq!(
        codex(&sidekick.list_providers().await),
        Some(json!(false)),
        "the Server stops offering Codex at once, as it does for the panel's change"
    );
    assert!(
        !next_settings(&mut client)
            .await
            .settings
            .provider
            .codex
            .enabled,
        "and an attached Client is told so"
    );
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some("{\n  \"provider\": {\n    \"codex\": {\n      \"enabled\": false\n    }\n  }\n}\n")
    );

    drop(client);
    drop(sidekick);
    hosted.server.shutdown().await.expect("stop the server");
    let mut hosted = host_providers(state_dir.path(), channel, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let described = describe(&mut sidekick, "provider.codex.enabled").await;
    assert_eq!(
        (&described["value"], &described["pinned"]),
        (&json!(false), &json!(true)),
        "the change outlives a restart: {described}"
    );
    assert_eq!(codex(&sidekick.list_providers().await), Some(json!(false)));

    set(&mut sidekick, json!({ "key": "provider.codex.enabled" })).await;
    assert_eq!(codex(&sidekick.list_providers().await), Some(json!(true)));

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_rejected_value_is_answered_with_what_to_type_instead_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-reject-setting",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    assert_eq!(
        refused(
            &mut sidekick,
            "set_setting",
            json!({ "key": "appearance.mode", "value": "sepia" })
        )
        .await,
        "`appearance.mode` takes one of \"system\", \"dark\", or \"light\", not \"sepia\"; \
         nothing was changed."
    );
    for (key, value, accepts) in [
        (
            "appearance.showIcons",
            json!("true"),
            "one of false or true",
        ),
        (
            "sidebar.initialWidth",
            json!(12),
            "an integer of at least 24",
        ),
        (
            "broker.maxDepth",
            json!(0),
            "an integer from 1 to 4294967295",
        ),
        (
            "derivation.errand",
            json!("sometimes"),
            "one of \"session\", \"off\", or an Agent Selection",
        ),
        (
            "session.contentWidth",
            json!({ "columns": 120 }),
            "one of \"fill\" or an integer of at least 50",
        ),
    ] {
        let refusal = refused(
            &mut sidekick,
            "set_setting",
            json!({ "key": key, "value": value }),
        )
        .await;
        assert!(
            refusal.contains(&format!("`{key}` takes {accepts}, not ")),
            "{key} is refused saying what it takes: {refusal}"
        );
    }
    assert_eq!(
        config_document(config_dir.path()),
        None,
        "no refused value reaches a Config Document"
    );
    assert_eq!(
        describe(&mut sidekick, "appearance.mode").await["value"],
        json!("system")
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_unknown_key_is_answered_with_the_nearest_keys_or_how_to_list_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-unknown-setting",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    assert_eq!(
        refused(
            &mut sidekick,
            "describe_setting",
            json!({ "key": "apperance.mode" })
        )
        .await,
        "Suru has no Setting `apperance.mode`, so nothing was described; did you mean \
         `appearance.mode`? list_settings lists every key."
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "set_setting",
            json!({ "key": "theme", "value": "dracula" })
        )
        .await,
        "Suru has no Setting `theme`, so nothing was changed; did you mean `appearance.theme`? \
         list_settings lists every key."
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "set_setting",
            json!({ "key": "colour", "value": "blue" })
        )
        .await,
        "Suru has no Setting `colour`, so nothing was changed; list_settings lists every key, \
         and takes a `group` to narrow them."
    );
    let near_serving = refused(
        &mut sidekick,
        "describe_setting",
        json!({ "key": "servng.port" }),
    )
    .await;
    assert!(
        !near_serving.contains("serving."),
        "a Setting governing Serving is never offered as the nearest: {near_serving}"
    );
    assert_eq!(config_document(config_dir.path()), None);

    server.shutdown().await.expect("shut down server");
}

/// Every Setting the schema withholds from a Sidekick — those governing
/// Serving and Pairing, a Setting added there later among them — is refused
/// by all three Tools, its value never told, and the Config Document pinning
/// it is left as it was.
#[tokio::test]
async fn no_setting_governing_serving_or_pairing_is_listed_described_or_set() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    write_config_document(config_dir.path(), HAND_WRITTEN);
    let channel = "sidekick-serving-settings";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let withheld = declaring(|access| access == SidekickAccess::Hidden)
        .into_iter()
        .map(|descriptor| descriptor.key)
        .collect::<Vec<_>>();
    for key in ["serving.enabled", "serving.port", "serving.bindAddress"] {
        assert!(withheld.contains(&key), "{key} is withheld: {withheld:?}");
    }

    let listed = list_settings(&mut sidekick, json!({})).await;
    for key in withheld
        .iter()
        .copied()
        .chain(["pairing.inviteLifetime", "serving.tls"])
    {
        assert!(!keys(&listed).contains(&key), "{key} is not listed");
        let attempts = [
            ("describe_setting", json!({ "key": key })),
            ("set_setting", json!({ "key": key, "value": true })),
            ("set_setting", json!({ "key": key, "value": 7777 })),
            ("set_setting", json!({ "key": key, "value": "0.0.0.0" })),
            ("set_setting", json!({ "key": key })),
        ];
        for (tool, arguments) in attempts {
            let refusal = refused(&mut sidekick, tool, arguments.clone()).await;
            assert!(
                refusal.contains("govern Serving and Pairing, which no Sidekick reads or changes")
                    && refusal.contains("the user may change them in Suru's settings panel")
                    && !refusal.contains("9443"),
                "{tool} refuses {arguments} saying why, and telling nothing of it: {refusal}"
            );
        }
    }
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some(HAND_WRITTEN),
        "the Config Document is left as the user wrote it"
    );
    let (_client, in_force) = attached_client(state_dir.path(), channel).await;
    assert!(
        !in_force.settings.serving.enabled && in_force.settings.serving.port == 9443,
        "and nothing about Serving moved"
    );

    server.shutdown().await.expect("shut down server");
}

/// A Setting bounding what Agents may do — those an Approval Posture is made
/// of — or doing what no later change undoes — automatic Worktree Reclaim — is
/// listed and described, so a Sidekick can tell the user what it says, but
/// never changed, its pin neither set nor removed, the refusal saying why and
/// that the user may change it.
#[tokio::test]
async fn a_setting_a_sidekick_may_only_read_is_listed_and_described_but_never_changed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let pinned = "{ \"worktree\": { \"autoReclaim\": \"off\" } }\n";
    write_config_document(config_dir.path(), pinned);
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-read-only-settings",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (sidekick_id, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let posture_before = read_session(&descriptor, sidekick_id)
        .await
        .session
        .approval_posture;
    let read_only = declaring(|access| matches!(access, SidekickAccess::ReadOnly { .. }));
    let keys_read_only = read_only
        .iter()
        .map(|descriptor| descriptor.key)
        .collect::<Vec<_>>();
    for key in ["provider.claude.permissionMode", "worktree.autoReclaim"] {
        assert!(keys_read_only.contains(&key), "{keys_read_only:?}");
    }

    let listed = list_settings(&mut sidekick, json!({})).await;
    for setting in read_only {
        let key = setting.key;
        let SidekickAccess::ReadOnly { why } = setting.sidekick else {
            unreachable!("only read-only Settings are tried");
        };
        assert!(keys(&listed).contains(&key), "{key} is listed");
        assert_eq!(describe(&mut sidekick, key).await["settable"], json!(false));
        let mut attempts = setting
            .values
            .named()
            .iter()
            .map(|choice| {
                let value =
                    serde_json::to_value(choice.mutation()).expect("a pin serializes")["value"]
                        .clone();
                json!({ "key": key, "value": value })
            })
            .collect::<Vec<_>>();
        attempts.push(json!({ "key": key, "value": 7 }));
        attempts.push(json!({ "key": key, "value": null }));
        attempts.push(json!({ "key": key }));
        for arguments in attempts {
            let refusal = refused(&mut sidekick, "set_setting", arguments.clone()).await;
            assert_eq!(
                refusal,
                format!(
                    "`{key}` is a Setting a Sidekick may read but not change, since {why}, so it \
                     was left as it is; tell the user, who may change it in Suru's settings \
                     panel."
                ),
                "{arguments}"
            );
        }
    }
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some(pinned),
        "no pin was set or taken away"
    );
    assert_eq!(
        describe(&mut sidekick, "worktree.autoReclaim").await["value"],
        json!("off"),
        "automatic Reclaim stays as the user left it"
    );
    assert_eq!(
        read_session(&descriptor, sidekick_id)
            .await
            .session
            .approval_posture,
        posture_before,
        "and the Sidekick's own posture is as it was"
    );

    server.shutdown().await.expect("shut down server");
}

/// One Setting governs everything Suru serves an Agent, so a Sidekick asked
/// to turn the Broker off may — narrowing what every Agent is offered widens
/// nothing — and is told it has switched its own Tools off, for the user, who
/// alone can turn them back on.
#[tokio::test]
async fn a_sidekick_turning_the_broker_off_switches_its_own_tools_off_and_is_told_so() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "sidekick-broker-off";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let (client, _) = attached_client(state_dir.path(), channel).await;

    let answer = set(
        &mut sidekick,
        json!({ "key": "broker.enabled", "value": false }),
    )
    .await;
    assert_eq!(
        (&answer["value"], &answer["pinned"]),
        (&json!(false), &json!(true))
    );
    assert!(
        answer["note"].as_str().is_some_and(|note| note.contains(
            "The Broker is off now, so Suru offers you and every other Agent none of its Tools"
        ) && note.contains("tell the user")),
        "{answer}"
    );
    assert_eq!(
        sidekick.call_tool_status("list_settings", json!({})).await,
        StatusCode::NOT_FOUND,
        "the Sidekick's next call is refused, as any Agent's is with the Broker off"
    );
    assert_eq!(
        sidekick.initialize_status().await,
        StatusCode::NOT_FOUND,
        "and the Broker answers it nothing more"
    );

    client
        .mutate_setting(SettingMutation::BrokerEnabled { value: None })
        .await
        .expect("the user turns the Broker back on in the panel");
    sidekick.initialize().await;
    assert!(
        set(
            &mut sidekick,
            json!({ "key": "appearance.mode", "value": "dark" })
        )
        .await
        .get("note")
        .is_none(),
        "with the Broker on, the Sidekick's Tools answer again, and a change says nothing more"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn only_a_sidekick_is_offered_the_settings_tools() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let mut hosted = host_providers(
        state_dir.path(),
        "sidekick-settings-tools-by-caller",
        Some(config_dir.path()),
    )
    .await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_ordinary, handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&handoff);
    ordinary.initialize().await;

    let sidekicks = listed_tools(&mut sidekick).await;
    let ordinarys = listed_tools(&mut ordinary).await;
    for tool in SETTINGS_TOOLS {
        assert!(
            sidekicks.contains(&tool.to_owned()),
            "{tool}: {sidekicks:?}"
        );
        assert!(
            !ordinarys.contains(&tool.to_owned()),
            "{tool}: {ordinarys:?}"
        );
    }
    for (tool, arguments) in [
        ("list_settings", json!({})),
        ("describe_setting", json!({ "key": "appearance.mode" })),
        (
            "set_setting",
            json!({ "key": "appearance.mode", "value": "dark" }),
        ),
    ] {
        let refusal = unoffered(&mut ordinary, tool, arguments).await;
        assert_eq!(
            refusal["message"],
            json!(format!("The Broker offers no Tool named `{tool}`")),
            "any other caller is answered as though the Tool did not exist: {refusal}"
        );
    }
    assert_eq!(config_document(config_dir.path()), None);

    hosted.server.shutdown().await.expect("shut down server");
}

/// Each kind of Setting a value is typed for — a number, free text, an Agent
/// Selection, and one taking a word or a number — is pinned by the value a
/// Sidekick types and read back as typed, and its pin removed again, each
/// change written to the Config Document and in force at once.
#[tokio::test]
async fn set_setting_pins_and_unpins_numbers_text_and_agent_selections() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "sidekick-set-setting-kinds";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let (mut client, _) = attached_client(state_dir.path(), channel).await;
    let selection = json!({ "provider": "claude", "model": "claude-opus-4-5", "options": [] });

    for (key, value, default) in [
        ("sidebar.initialWidth", json!(40), json!(32)),
        ("appearance.theme", json!("tokyonight"), json!("system")),
        ("derivation.errand", selection.clone(), json!("session")),
        ("session.contentWidth", json!(120), json!(80)),
        ("session.contentWidth", json!("fill"), json!(80)),
    ] {
        assert_eq!(
            set(&mut sidekick, json!({ "key": key, "value": value })).await,
            json!({ "key": key, "value": value, "pinned": true }),
            "{key} pins {value}"
        );
        let described = describe(&mut sidekick, key).await;
        assert_eq!(
            (&described["value"], &described["pinned"]),
            (&value, &json!(true)),
            "{key} reads back as typed: {described}"
        );
        let document = config_document(config_dir.path()).expect("the pin is written");
        let name = key.rsplit('.').next().expect("a key names its Setting");
        assert!(document.contains(name), "{key}: {document}");
        let pinned = key.to_owned();
        settings_until(&mut client, |told| told.pinned.contains(&pinned)).await;

        assert_eq!(
            set(&mut sidekick, json!({ "key": key })).await,
            json!({ "key": key, "value": default, "pinned": false }),
            "{key}'s pin is removed and its default in force again"
        );
        settings_until(&mut client, |told| !told.pinned.contains(&pinned)).await;
    }
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some("{}\n"),
        "every pin removed leaves nothing behind"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Config Document that does not parse is never rewritten: a change is
/// refused, saying where the document is for the user to mend, and every
/// byte of it is left as it was, while the Settings are still read.
#[tokio::test]
async fn a_config_document_that_does_not_parse_is_refused_and_left_as_it_is() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let broken = "{ \"appearance\": { \"mode\": \"dark\" \n";
    write_config_document(config_dir.path(), broken);
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-broken-config-document",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    for arguments in [
        json!({ "key": "appearance.mode", "value": "light" }),
        json!({ "key": "appearance.mode" }),
    ] {
        let refusal = refused(&mut sidekick, "set_setting", arguments.clone()).await;
        assert!(
            refusal.starts_with("Nothing was changed: the user's Config Document at `")
                && refusal.contains("suru.jsonc")
                && refusal.contains("cannot be edited in place")
                && refusal.contains("tell the user, who can mend it by hand"),
            "{arguments} is refused saying where the document is: {refusal}"
        );
    }
    assert_eq!(
        config_document(config_dir.path()).as_deref(),
        Some(broken),
        "the document is left exactly as it was"
    );
    assert_eq!(
        describe(&mut sidekick, "appearance.mode").await["value"],
        json!("system"),
        "and the Settings are read all the same, at their defaults"
    );

    server.shutdown().await.expect("shut down server");
}

/// Settings stay with the Sidekick's own Server: every Settings Tool refuses
/// an `origin`, whatever Remote it names, saying so.
#[tokio::test]
async fn every_settings_tool_refuses_an_origin() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (server, mut claude) = host_claude(
        state_dir.path(),
        config_dir.path(),
        "sidekick-settings-origin",
    )
    .await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;

    for (tool, arguments) in [
        ("list_settings", json!({ "origin": "workstation" })),
        (
            "describe_setting",
            json!({ "key": "appearance.mode", "origin": "workstation" }),
        ),
        (
            "set_setting",
            json!({ "key": "appearance.mode", "value": "dark", "origin": "everywhere" }),
        ),
    ] {
        assert_eq!(
            refused(&mut sidekick, tool, arguments).await,
            format!(
                "{tool} takes no `origin`: Settings are this server's alone, and a Remote's are \
                 its own user's, so nothing was done."
            )
        );
    }
    assert_eq!(config_document(config_dir.path()), None);

    server.shutdown().await.expect("shut down server");
}

/// A Serving listener that cannot start fails to follow every change of the
/// Settings, whatever the change, and why names the address and port it could
/// not take — the values of Settings no Sidekick may read (ADR 0043). So a
/// Sidekick whose change lands is told it is in force and that Serving could
/// not follow, and nothing of where, in its answer or anything a harness
/// records of it; the user's own panel is told the rest.
#[tokio::test]
async fn a_serving_listener_that_cannot_start_tells_a_sidekick_nothing_of_where() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let occupied = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("occupy a loopback port");
    let port = occupied
        .local_addr()
        .expect("read the occupied port")
        .port()
        .to_string();
    write_config_document(
        config_dir.path(),
        &format!(
            "{{ \"serving\": {{ \"enabled\": true, \"bindAddress\": \"127.0.0.1\", \"port\": \
             {port} }} }}\n"
        ),
    );
    let channel = "sidekick-serving-cannot-start";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&descriptor, &mut claude).await;
    let (mut client, _) = attached_client(state_dir.path(), channel).await;

    let result = sidekick
        .call_tool(
            "set_setting",
            json!({ "key": "appearance.mode", "value": "dark" }),
        )
        .await;
    let told = result.to_string();
    assert!(
        !told.contains("127.0.0.1") && !told.contains(&port),
        "nothing a harness reads or records of the call tells where Serving could not start: \
         {told}"
    );
    assert_ne!(
        result["isError"],
        json!(true),
        "the change landed: {result}"
    );
    let answer = &result["structuredContent"];
    assert_eq!(
        (&answer["value"], &answer["pinned"]),
        (&json!("dark"), &json!(true))
    );
    assert!(
        answer["note"].as_str().is_some_and(|note| {
            note.contains("The change is saved and in force, but Suru could not start Serving")
        }),
        "{answer}"
    );
    assert_eq!(
        settings_until(&mut client, |told| told.settings.appearance.mode
            == AppearanceMode::Dark)
        .await
        .settings
        .appearance
        .mode,
        AppearanceMode::Dark,
        "the change is in force and every Client told of it"
    );

    let error = client
        .mutate_setting(SettingMutation::AppearanceMode {
            value: Some(AppearanceMode::Light),
        })
        .await
        .expect_err("the panel is told Serving could not follow");
    assert!(
        format!("{error:#}").contains(&format!("127.0.0.1:{port}")),
        "the user's own panel is told where: {error:#}"
    );

    drop(occupied);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A change of the Settings runs whole and one at a time, on the Server's own:
/// a Sidekick's change asked for while the user's is still being put in force
/// waits until that one is done, writing nothing meanwhile, and lands after it
/// though the Sidekick's harness hangs up before its answer — so the settings
/// in force are always those the Config Document last said.
#[tokio::test]
async fn a_sidekicks_change_waits_for_one_under_way_and_lands_though_it_stops_waiting() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "sidekick-settings-one-at-a-time";
    let (server, mut claude) = host_claude(state_dir.path(), config_dir.path(), channel).await;
    let descriptor = server.descriptor().clone();
    let directory = sidekick_directory(&descriptor).await;
    let (_sidekick, handoff, mut provider) = start_session(
        &descriptor,
        &mut claude,
        &directory,
        default_selection(&claude_models()),
    )
    .await;
    provider.gate_posture_updates();
    let (editor, _) = attached_client(state_dir.path(), channel).await;
    let (mut onlooker, _) = attached_client(state_dir.path(), channel).await;

    // The user's change of Claude's permission mode reaches the Sidekick's own
    // Session, which follows it, and that Session's Provider holds it there:
    // the user's change is under way until the Provider answers.
    let users = tokio::spawn(async move {
        let changed = editor
            .mutate_setting(SettingMutation::ProviderClaudePermissionMode {
                value: Some(ClaudePermissionMode::AcceptEdits),
            })
            .await
            .map(|snapshot| snapshot.settings.provider.claude.permission_mode);
        (editor, changed)
    });
    let held = timeout(PROGRESS_DEADLINE, provider.next_posture_update())
        .await
        .expect("the user's change reaches the Sidekick's Session");

    let mut hanging_up = McpClient::handed(&handoff);
    hanging_up.initialize().await;
    let sidekicks = tokio::spawn(async move {
        hanging_up
            .call_tool(
                "set_setting",
                json!({ "key": "appearance.mode", "value": "dark" }),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !sidekicks.is_finished(),
        "the Sidekick's change waits while the user's is under way"
    );
    assert!(
        config_document(config_dir.path()).is_some_and(|document| !document.contains("appearance")),
        "and writes nothing meanwhile"
    );
    sidekicks.abort();
    assert!(
        sidekicks.await.is_err_and(|error| error.is_cancelled()),
        "the Sidekick's harness hangs up before it is answered"
    );

    held.next_turn();
    let (_editor, changed) = users.await.expect("the user's change answers");
    assert_eq!(
        changed.expect("the user's change lands"),
        ClaudePermissionMode::AcceptEdits
    );
    let in_force = settings_until(&mut onlooker, |told| {
        told.settings.appearance.mode == AppearanceMode::Dark
    })
    .await;
    assert_eq!(
        in_force.settings.provider.claude.permission_mode,
        ClaudePermissionMode::AcceptEdits,
        "the Sidekick's change lands after the user's, keeping it"
    );
    let document = config_document(config_dir.path()).expect("both changes are written");
    assert!(
        document.contains("\"permissionMode\": \"acceptEdits\"")
            && document.contains("\"mode\": \"dark\""),
        "and what is in force is what the document says: {document}"
    );
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;
    assert_eq!(
        describe(&mut sidekick, "appearance.mode").await["value"],
        json!("dark")
    );

    drop(onlooker);
    server.shutdown().await.expect("shut down server");
}
