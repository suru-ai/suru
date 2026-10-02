//! What a Remote's tree did between two readings of it is replayed as it
//! happened, whatever has happened since: each Intervention a Subagent asked
//! is the Sidekick's — or not — by the Turn that had set that Subagent working
//! when it asked, though another Turn has resumed it by the time the tree is
//! read.
//!
//! Each test reads the Remote no more often than a long read interval, so the
//! tree's next reading comes only once everything a test does between two
//! readings is done.

use super::*;

/// How long the Sidekick's own Server waits between two readings of a tree
/// owed Reports: longer than everything a test does between two of them.
const READ_INTERVAL: Duration = Duration::from_millis(1_500);

fn delayed_timings() -> ServerTimings {
    timings().with_remote_report_reads(READ_INTERVAL, Duration::from_secs(600))
}

/// A Subagent asks a Questionnaire while the Sidekick's settled Turn's branch
/// works on, and settles; and before the tree is next read, a Continuation of
/// the Session — no Turn of the Sidekick's — resumes it. Read together, the
/// Questionnaire was asked in the Sidekick's branch, so it is reported, as
/// settled.
#[tokio::test]
async fn what_a_subagent_asked_for_the_sidekicks_turn_is_reported_though_another_turn_resumed_it() {
    let mut owed = owed("sidekick-remote-delayed-resumed-after", delayed_timings()).await;
    let remote = owed.remote();
    let (begun, handoff, mut provider) = owed.begin_handing().await;
    let selection = default_selection(&claude_models());
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let child = agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, mut child_provider) = run_there(&mut owed.pair.remote.provider, selection).await;
    fixes(&remote, begun, &provider).await;
    owed.steered("the Sidekick's Turn is reported as it settles, its Subagent working on")
        .await;

    // Before the tree is read again: the Subagent asks, settles, and a
    // Continuation of the Session resumes it.
    ask(&remote, child, &child_provider, &where_to_run()).await;
    writes(&child_provider, "Asked, and done.").await;
    observed(&child_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .expect("the Subagent's Report wakes the Session into a Continuation")
        .succeed();
    let resumed = agent.send_to_subagent(child, "Look once more.").await;
    assert_ne!(resumed["isError"], json!(true), "{resumed}");
    timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the resume reaches the Subagent's Provider")
        .succeed();

    assert_eq!(
        owed.steered("what the Subagent asked for the Sidekick's Turn is reported")
            .await,
        format!(
            "Sidekick Report from Suru: a Subagent of the Session \"{ASKED}\" you set to work on \
             the Remote \"{REMOTE}\" asked a Questionnaire, which no longer waits on anyone. The \
             Session's session_id is {begun}, and the Subagent's is {child}, each at origin \
             \"{REMOTE}\": given the Subagent's, read_session says how it was settled."
        )
    );

    owed.shutdown().await;
}

/// A Subagent a Turn of the user's set working asks a Questionnaire, and
/// settles; and before the tree is next read, a Turn the Sidekick's Prompt
/// began resumes it. Read together, the Questionnaire was asked in the
/// user's branch, so the Sidekick hears nothing of it — only of its own
/// Turn as it settles.
#[tokio::test]
async fn what_a_subagent_asked_for_the_users_turn_is_not_reported_though_the_sidekicks_resumed_it()
{
    let mut owed = owed("sidekick-remote-delayed-resumed-before", delayed_timings()).await;
    let remote = owed.remote();
    let (users, handoff, mut users_provider) =
        users_session_handing(&mut owed, "Run the auth suite.").await;
    let selection = default_selection(&claude_models());
    let mut agent = McpClient::handed(&handoff);
    agent.initialize().await;
    let child = agent
        .spawn_subagent(researcher("claude", selection.model.as_str(), json!({})))
        .await;
    let (_, mut child_provider) = run_there(&mut owed.pair.remote.provider, selection).await;
    // The user's Turn settles while its Subagent works on, and the
    // Sidekick's Prompt begins a Turn of its own.
    fixes(&remote, users, &users_provider).await;
    assert_eq!(owed.prompt(users).await, json!("new_turn"));
    timeout(PROGRESS_DEADLINE, users_provider.next_turn())
        .await
        .expect("the Sidekick's Prompt begins a Turn on the Remote")
        .succeed();

    // Before the tree is read again: the Subagent asks for the user's Turn,
    // settles, and the Sidekick's Turn resumes it.
    ask(&remote, child, &child_provider, &where_to_run()).await;
    writes(&child_provider, "Asked, and done.").await;
    observed(&child_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the Subagent's Report steers the Sidekick's working Turn")
        .succeed();
    let resumed = agent.send_to_subagent(child, "Look once more.").await;
    assert_ne!(resumed["isError"], json!(true), "{resumed}");
    timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the resume reaches the Subagent's Provider")
        .succeed();
    writes(&child_provider, "Looked.").await;
    observed(&child_provider, ProviderEvent::TurnCompleted).await;
    timeout(PROGRESS_DEADLINE, users_provider.next_steer())
        .await
        .expect("the resumed Subagent's Report steers the Sidekick's Turn")
        .succeed();
    fixes(&remote, users, &users_provider).await;

    assert_eq!(
        untimed_sidekick_report(
            &owed
                .steered("the Sidekick's own Turn is reported as it settles")
                .await
        ),
        settled_there(users, "Run the auth suite.", "completed", FIXED),
        "the first Report is of the Sidekick's own Turn, never of what the Subagent asked for \
         the user's"
    );
    owed.steered_with_nothing("and nothing more").await;

    owed.shutdown().await;
}
