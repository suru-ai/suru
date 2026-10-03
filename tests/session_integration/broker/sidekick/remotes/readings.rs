//! What a Server answers a reading of one of its Sessions with, as a Peer's
//! Server asks it for its own Sidekick (ADR 0049): the Server holding the
//! Session takes the slice asked for — the window of Turns, the cap on what
//! they say, the point to read on from, or one entry whole — so only that
//! crosses the Pairing, and refuses an answer that would run past what its
//! asker reads of one.

use super::*;

/// What the Server `descriptor` describes answers a `GET` of `path_and_query`
/// on its own Session API with: its status, and its body as JSON.
async fn asked(descriptor: &RuntimeDescriptor, path_and_query: &str) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .get(format!("{}{path_and_query}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .unwrap_or_else(|error| panic!("ask for {path_and_query}: {error}"));
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// The words of a reading's transcript, which names no other Server.
fn words(reading: &Value) -> String {
    let spans = reading["transcript"]
        .as_array()
        .unwrap_or_else(|| panic!("a reading holds a transcript: {reading}"));
    spans
        .iter()
        .map(|span| {
            span["text"]
                .as_str()
                .unwrap_or_else(|| panic!("this transcript is words alone: {span}"))
        })
        .collect()
}

#[tokio::test]
async fn a_server_answers_a_reading_of_its_session_with_only_the_slice_asked_for() {
    const {
        assert!(
            suru::protocol::PROTOCOL_VERSION >= 85,
            "a reading taken where its Session lives changes what Servers say to each other"
        );
    }
    let mut remote = Serving::start("sidekick-remote-readings").await;
    let descriptor = remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (session_id, provider) = started_session(
        &descriptor,
        &mut remote.provider,
        there.path(),
        "Look at the ledger.",
    )
    .await;
    write_agent_message(&provider, "The ledger has a flaky test.").await;
    complete_turn(&descriptor, session_id, &provider).await;
    let reading = |query: &str| format!("/v1/sessions/{session_id}/reading?{query}");

    let (status, answer) = asked(
        &descriptor,
        &reading("turns=1&max_chars=2000&detail=messages"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["summary"]["id"], json!(session_id));
    assert_eq!(answer["summary"]["title"], json!("Look at the ledger."));
    let read = &answer["reading"]["Ok"];
    assert!(
        words(read)
            .ends_with("\n1.1 user: Look at the ledger.\n1.2 agent: The ledger has a flaky test."),
        "{read}"
    );
    assert_eq!(
        (&read["before"], &read["earlier"]),
        (&Value::Null, &Value::Null),
        "the whole Session fits"
    );

    let (_, answer) = asked(&descriptor, &reading("turns=1&max_chars=4&detail=messages")).await;
    let read = &answer["reading"]["Ok"];
    assert!(words(read).ends_with("1.2 agent: […]est."), "{read}");
    assert_eq!(
        read["before"],
        json!("1.2.24"),
        "the point to read on from is the one the Server holding the Session gives"
    );
    let (_, answer) = asked(
        &descriptor,
        &reading("turns=1&max_chars=2000&detail=messages&before=1.2.24"),
    )
    .await;
    assert!(
        words(&answer["reading"]["Ok"])
            .ends_with("1.1 user: Look at the ledger.\n1.2 agent: The ledger has a flaky t"),
        "and reading on from it there picks up where the last read left off: {answer}"
    );

    let (_, answer) = asked(&descriptor, &reading("item=1.1")).await;
    assert_eq!(
        words(&answer["reading"]["Ok"]),
        "1.1 user: Look at the ledger."
    );
    let (status, answer) = asked(&descriptor, &reading("item=9.1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        answer["reading"]["Err"],
        json!({ "no_such_turn": { "turn": 9, "turns": 1 } }),
        "a read naming what the Session does not hold is answered with what it does hold"
    );

    let (status, answer) = asked(&descriptor, &reading("item=1.1&turns=2")).await;
    assert_eq!(
        (status, &answer["code"]),
        (StatusCode::BAD_REQUEST, &json!("invalid_command")),
        "an entry read whole takes no window beside it"
    );
    let (status, answer) = asked(&descriptor, &reading("item=1.1&within=64")).await;
    assert_eq!(
        (status, &answer["code"]),
        (StatusCode::PAYLOAD_TOO_LARGE, &json!("reading_too_large")),
        "an answer past what its asker reads of one is refused, not sent: {answer}"
    );
    let (status, answer) = asked(
        &descriptor,
        &format!("/v1/sessions/{}/reading?item=1.1", SessionId::new()),
    )
    .await;
    assert_eq!(
        (status, &answer["code"]),
        (StatusCode::NOT_FOUND, &json!("session_not_found"))
    );
    let unauthenticated = reqwest::Client::new()
        .get(format!("{}{}", descriptor.base_url, reading("item=1.1")))
        .send()
        .await
        .expect("ask without the token");
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    remote.shutdown().await;
}
