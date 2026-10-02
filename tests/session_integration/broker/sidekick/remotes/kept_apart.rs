//! What stays with each Server across a Pairing: a Server's Settings and the
//! Memories its Sidekicks keep are its own. Every Settings Tool and every
//! Memory Tool refuses an `origin`, saying so in its own words and doing
//! nothing anywhere; what each Server's Sidekick sets or keeps is reached by
//! neither Sidekick on the other; and a Peer reaches neither through the
//! Pairing, a Remote's Broker answering no Peer and its Settings changed by
//! its own user alone.

use super::*;
use crate::broker::sidekick_acts::{acted, refused};

/// Why every Settings Tool refuses an `origin`.
const SETTINGS_ALONE: &str =
    "Settings are this server's alone, and a Remote's are its own user's, so nothing was done.";

/// Why every Memory Tool refuses an `origin`.
const MEMORIES_ALONE: &str =
    "Memories are this server's alone, kept for its own Sidekicks, so nothing was done.";

/// The value the Sidekick `client` is given for the Setting `key`.
async fn setting(client: &mut McpClient, key: &str) -> Value {
    acted(client, "describe_setting", json!({ "key": key })).await["value"].clone()
}

/// The titles of the Memories a search by `client` for `query` finds.
async fn found(client: &mut McpClient, query: &str) -> Vec<String> {
    let answer = acted(client, "search_memory", json!({ "query": query })).await;
    let mut titles = answer["memories"]
        .as_array()
        .unwrap_or_else(|| panic!("a search answers rows: {answer}"))
        .iter()
        .map(|row| {
            row["title"]
                .as_str()
                .expect("every row has a title")
                .to_owned()
        })
        .collect::<Vec<_>>();
    titles.sort_unstable();
    titles
}

#[tokio::test]
async fn settings_and_memories_stay_with_their_own_server_across_a_pairing() {
    const MODE: &str = "appearance.mode";
    let mut pair = paired("sidekick-remote-kept-apart", ServerTimings::default()).await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let (_ours, mut ours, _our_provider) = start_sidekick(&own, &mut pair.claude).await;
    let (_theirs, mut theirs, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;
    let kept_here = acted(
        &mut ours,
        "store_memory",
        json!({ "title": "Kept here", "body": "The user reviews on Fridays." }),
    )
    .await;
    let kept_there = acted(
        &mut theirs,
        "store_memory",
        json!({ "title": "Kept there", "body": "The user reviews on Mondays." }),
    )
    .await;
    let only_there = acted(
        &mut theirs,
        "store_memory",
        json!({ "title": "Only there", "body": "The user deploys on Tuesdays." }),
    )
    .await;
    let mode_here = setting(&mut ours, MODE).await;

    // Every Settings Tool and every Memory Tool refuses an `origin`, naming
    // a Remote or every Server at once, and does nothing anywhere.
    for origin in [REMOTE, "everywhere"] {
        for (tool, arguments, why) in [
            ("list_settings", json!({}), SETTINGS_ALONE),
            ("describe_setting", json!({ "key": MODE }), SETTINGS_ALONE),
            (
                "set_setting",
                json!({ "key": MODE, "value": "dark" }),
                SETTINGS_ALONE,
            ),
            (
                "store_memory",
                json!({ "title": "Sent across", "body": "Reviews on Sundays." }),
                MEMORIES_ALONE,
            ),
            (
                "search_memory",
                json!({ "query": "reviews" }),
                MEMORIES_ALONE,
            ),
            (
                "recall_memory",
                json!({ "memory_id": kept_there["memory_id"] }),
                MEMORIES_ALONE,
            ),
            (
                "update_memory",
                json!({ "memory_id": kept_there["memory_id"], "body": "Reviews daily." }),
                MEMORIES_ALONE,
            ),
            (
                "forget_memory",
                json!({ "memory_id": kept_there["memory_id"] }),
                MEMORIES_ALONE,
            ),
        ] {
            let mut arguments = arguments;
            arguments["origin"] = json!(origin);
            assert_eq!(
                refused(&mut ours, tool, arguments).await,
                format!("{tool} takes no `origin`: {why}"),
                "{tool} refuses the origin {origin}"
            );
        }
    }
    assert_eq!(setting(&mut ours, MODE).await, mode_here);
    assert_eq!(setting(&mut theirs, MODE).await, mode_here);
    assert_eq!(found(&mut ours, "reviews").await, ["Kept here"]);
    assert_eq!(
        found(&mut theirs, "user").await,
        ["Kept there", "Only there"],
        "nothing was stored, changed or forgotten on either Server"
    );

    // What each Server's Sidekick keeps or sets is its own Server's alone:
    // a Memory's identity names it only on the Server keeping it.
    assert_eq!(
        kept_here["memory_id"], kept_there["memory_id"],
        "each Server numbers its own Memories"
    );
    assert_eq!(
        acted(
            &mut ours,
            "recall_memory",
            json!({ "memory_id": kept_there["memory_id"] }),
        )
        .await["title"],
        json!("Kept here"),
        "so the Remote's Memory's identity names this Server's own here"
    );
    let unknown = refused(
        &mut ours,
        "recall_memory",
        json!({ "memory_id": only_there["memory_id"] }),
    )
    .await;
    assert!(
        unknown.starts_with("Suru keeps no Memory"),
        "and one only the Remote keeps is no Memory here: {unknown}"
    );
    assert_eq!(found(&mut ours, "deploys").await, Vec::<String>::new());
    acted(
        &mut theirs,
        "set_setting",
        json!({ "key": MODE, "value": "dark" }),
    )
    .await;
    assert_eq!(setting(&mut theirs, MODE).await, json!("dark"));
    assert_eq!(
        setting(&mut ours, MODE).await,
        mode_here,
        "a Setting the Remote's Sidekick changed is the Remote's alone"
    );

    // Nor does a Peer reach them through the Pairing: the Remote's Broker
    // answers no Peer, and its Settings are its own user's to change.
    let through_the_pairing = |path: &str| {
        reqwest::Client::new()
            .post(format!("{}/v1/remotes/{REMOTE}{path}", own.base_url))
            .bearer_auth(&own.token)
    };
    let broker = through_the_pairing("/broker/mcp")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "search_memory", "arguments": { "query": "reviews" } },
        }))
        .send()
        .await
        .expect("ask the Remote's Broker through the Pairing");
    assert_eq!(
        broker.status(),
        StatusCode::NOT_FOUND,
        "the Remote's Broker is no Peer's to reach"
    );
    let changed = through_the_pairing("/v1/settings")
        .json(&SettingMutation::AppearanceMode {
            value: Some(suru::protocol::AppearanceMode::Light),
        })
        .send()
        .await
        .expect("ask to change the Remote's Settings through the Pairing");
    assert_eq!(
        changed.status(),
        StatusCode::FORBIDDEN,
        "nor are the Remote's Settings a Peer's to change"
    );
    assert_eq!(setting(&mut theirs, MODE).await, json!("dark"));
    assert_eq!(
        found(&mut theirs, "user").await,
        ["Kept there", "Only there"]
    );

    pair.shutdown().await;
}
