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

/// The most this Server reads of any one answer from its Remote below.
const BUDGET: usize = 16 * 1024;

/// A Turn the user begins with `prompt` in `session_id` on the Remote
/// `descriptor` describes, its Agent answering `answer` and settling.
async fn turn_answered(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &mut ControlledProviderSession,
    prompt: &str,
    answer: &str,
) {
    admit_prompt(descriptor, session_id, prompt).await;
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Turn reaches the Provider")
        .succeed();
    write_agent_message(provider, answer).await;
    complete_turn(descriptor, session_id, provider).await;
}

/// What the Agent says in Turn `turn` below: a few KiB, so the Session as a
/// whole runs far past the budget while any one Turn stays within it.
fn worked_through(turn: usize) -> String {
    format!("Turn {turn} done. {}", "Every detail. ".repeat(400))
}

/// A Remote's Session many times larger than what this Server reads of one
/// answer from it reads from here all the same, as it reads on the Remote:
/// the Remote takes the slice each read asks for, so only that crosses the
/// Pairing. Reading on from each `before` reads every Turn, and an entry is
/// read whole wherever it fits. A read whose answer would not fit is refused
/// saying how to ask for less — or, for one entry larger than that on its
/// own, that it is read on the Remote.
#[tokio::test]
async fn a_remotes_session_past_the_byte_budget_reads_in_slices_the_remote_takes() {
    let mut pair = paired(
        "sidekick-remote-readings-past-budget",
        ServerTimings::default().with_remote_reach_budget(BUDGET),
    )
    .await;
    let own = pair.own.descriptor().clone();
    let remote = pair.remote.descriptor();
    let there = tempfile::tempdir().expect("create a Workspace on the Remote");
    let (_sidekick, mut sidekick, _provider) = start_sidekick(&own, &mut pair.claude).await;
    let (_theirs, mut on_the_remote, _their_provider) =
        start_sidekick(&remote, &mut pair.remote.provider).await;
    let (long, mut provider) = started_session(
        &remote,
        &mut pair.remote.provider,
        there.path(),
        "Explain everything.",
    )
    .await;
    write_agent_message(&provider, &worked_through(1)).await;
    complete_turn(&remote, long, &provider).await;
    const TURNS: usize = 6;
    for turn in 2..=TURNS {
        turn_answered(
            &remote,
            long,
            &mut provider,
            &format!("Now step {turn}."),
            &worked_through(turn),
        )
        .await;
    }
    let (whole, _) = as_the_remote_holds_it(&remote, long).await;
    assert!(
        whole.to_string().len() > 2 * BUDGET,
        "the Session runs far past the budget"
    );

    assert!(
        titles(&list_sessions(&mut sidekick, json!({ "origin": REMOTE })).await)
            .contains(&"Explain everything."),
        "a listing within the budget is read"
    );
    for asked in [
        json!({}),
        json!({ "turns": 2, "detail": "activities" }),
        json!({ "max_chars": 12, "before": "4" }),
        json!({ "item": "3.2" }),
    ] {
        let mut arguments = asked.clone();
        arguments["session_id"] = json!(long);
        let read_there = answered(&mut on_the_remote, "read_session", arguments.clone()).await;
        arguments["origin"] = json!(REMOTE);
        let read_through = answered(&mut sidekick, "read_session", arguments).await;
        assert_eq!(
            without_origin(read_through),
            read_there,
            "{asked} reads through the Pairing as it reads on the Remote"
        );
    }

    // Read on from each point given until the Session's start, a Turn at a
    // time: every Turn is read, and nothing twice.
    let mut headings = Vec::new();
    let mut before = Value::Null;
    loop {
        let mut arguments = json!({
            "session_id": long,
            "origin": REMOTE,
            "max_chars": 8_000,
        });
        if !before.is_null() {
            arguments["before"] = before.clone();
        }
        let read = answered(&mut sidekick, "read_session", arguments).await;
        let transcript = read["transcript"]
            .as_str()
            .expect("a transcript")
            .to_owned();
        headings.extend(
            transcript
                .lines()
                .filter(|line| line.starts_with("[Turn "))
                .map(|line| {
                    line.split(" · ")
                        .next()
                        .expect("a Turn's number")
                        .to_owned()
                }),
        );
        before = read["before"].clone();
        if before.is_null() {
            break;
        }
    }
    headings.reverse();
    assert_eq!(
        headings,
        (1..=TURNS)
            .map(|turn| format!("[Turn {turn} of {TURNS}"))
            .collect::<Vec<_>>(),
        "reading on from each before reads every Turn once"
    );

    let too_much = sidekick
        .refusal(
            "read_session",
            json!({ "session_id": long, "origin": REMOTE, "turns": TURNS, "max_chars": 1_000_000 }),
        )
        .await;
    assert_eq!(
        too_much,
        format!(
            "What this read asks of Session `{long}` on the Remote `{REMOTE}` would run past the \
             16 KiB this server reads of one answer from a Remote, so it was not read. Ask for \
             less at once — fewer \"turns\" or a smaller \"max_chars\" — and read on from the \
             \"before\" each answer gives."
        )
    );

    turn_answered(
        &remote,
        long,
        &mut provider,
        "And the appendix?",
        &"Appendix. ".repeat(2 * BUDGET / 10),
    )
    .await;
    let read = answered(
        &mut sidekick,
        "read_session",
        json!({ "session_id": long, "origin": REMOTE }),
    )
    .await;
    assert!(
        read["transcript"]
            .as_str()
            .is_some_and(|transcript| transcript.ends_with("Appendix. Appendix. ")),
        "the default read shows the end of an entry past the budget: {read}"
    );
    assert_eq!(
        sidekick
            .refusal(
                "read_session",
                json!({ "session_id": long, "origin": REMOTE, "item": "7.2" }),
            )
            .await,
        format!(
            "Entry 7.2 of Session `{long}` on the Remote `{REMOTE}` runs, on its own, past the \
             16 KiB this server reads of one answer from a Remote, so it cannot be read whole \
             from here. It can be read on `{REMOTE}` itself, where the user can open it, as \
             they can from a Client turned toward that Remote."
        )
    );

    pair.shutdown().await;
}
