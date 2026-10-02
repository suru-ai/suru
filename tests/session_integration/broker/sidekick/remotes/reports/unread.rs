//! A read of a Remote that fails while the Remote's catalog answers is no
//! outage: what is owed there is kept, and nothing says the Remote stopped
//! answering. An answer running past what this Server reads of one is said
//! so in plain words, once — the Turn it settled told without its final
//! Message, or the tree it outlines told to be past following — and
//! whatever else is owed there is told as it comes.
//!
//! Each test reads its Remote within a small budget, which a listing of it
//! and every word its catalog says fit within, and what a test has its
//! Sessions hold does not.

use super::*;
use crate::broker::admit_prompt;

/// The most this Server reads of any one answer from its Remote here.
const BUDGET: usize = 16 * 1024;

fn budgeted_timings() -> ServerTimings {
    timings().with_remote_reach_budget(BUDGET)
}

/// The Report of a Turn of the Remote's `session_id`, titled `title`, that
/// settled as `settled`, the Session holding too much to read its final
/// Message, as the Sidekick reads it untimed.
fn settled_past_budget_there(session_id: SessionId, title: &str, settled: &str) -> String {
    format!(
        "Sidekick Report from Suru: the Session \"{title}\" you set to work on the Remote \
         \"{REMOTE}\" has settled its Turn, which {settled}. Its session_id is {session_id} and \
         its origin \"{REMOTE}\", which read_session takes.\n\nIt holds more than Suru reads \
         of a Remote at once, so its Agent's final Message is not given here."
    )
}

/// The Report that the Remote's `session_id`, titled `title`, holds more
/// than this Server reads of it to follow its work.
fn past_following_there(session_id: SessionId, title: &str) -> String {
    format!(
        "Sidekick Report from Suru: the Session \"{title}\" you set to work on the Remote \
         \"{REMOTE}\" has grown, with all beneath it, past what Suru reads of a Remote at once, \
         so its work cannot be followed for Reports while it stays so. Its session_id is \
         {session_id} and its origin \"{REMOTE}\": read it with read_session to learn how its \
         work goes."
    )
}

/// A Turn the Sidekick began settles with a final Message longer than the
/// budget: reading it fails while the catalog answers, so the settling is
/// told without it — and nothing says the Remote stopped answering, the next
/// Session's settling there told as it comes.
#[tokio::test]
async fn a_final_message_past_the_budget_is_left_out_of_the_report_and_no_outage() {
    let mut owed = owed(
        "sidekick-remote-report-message-past-budget",
        budgeted_timings(),
    )
    .await;
    let remote = owed.remote();
    let (long, long_provider) = owed.begin().await;
    let (short, short_provider) = owed.begin().await;

    writes(&long_provider, &"Every detail. ".repeat(4 * 1024)).await;
    observed(&long_provider, ProviderEvent::TurnCompleted).await;
    latest_turn_settles(&remote, long, TurnStatus::Completed).await;
    let report = owed
        .steered("the settling is told though its final Message runs past the budget")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_past_budget_there(long, ASKED, "completed")
    );

    fixes(&remote, short, &short_provider).await;
    let report = owed
        .steered("what else is owed there is told as it comes")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_there(short, ASKED, "completed", FIXED)
    );
    owed.steered_with_nothing("nothing says the Remote stopped answering")
        .await;

    owed.shutdown().await;
}

/// A tree the Sidekick began grows past the budget — its user queues more
/// Prompts in it than an outline of it holds within the budget — while the
/// catalog answers: the Sidekick is told once, in plain words, that its work
/// there cannot be followed and that read_session reads it; nothing says the
/// Remote stopped answering, and another tree's settling is told as it comes.
#[tokio::test]
async fn a_tree_past_the_budget_is_told_once_to_be_past_following_and_no_outage() {
    let mut owed = owed(
        "sidekick-remote-report-outline-past-budget",
        budgeted_timings(),
    )
    .await;
    let remote = owed.remote();
    let (crowded, crowded_provider) = owed.begin().await;
    let (short, short_provider) = owed.begin().await;

    for _ in 0..200 {
        admit_prompt(&remote, crowded, "And then the next one, please.").await;
    }
    // Its Turn settling has its tree read again; the next Turn the queued
    // Prompts begin is left waiting on its Provider.
    observed(&crowded_provider, ProviderEvent::TurnCompleted).await;
    assert_eq!(
        owed.steered("a tree past the budget is said to be past following")
            .await,
        past_following_there(crowded, ASKED)
    );

    fixes(&remote, short, &short_provider).await;
    let report = owed
        .steered("another tree owed Reports there is told as it comes")
        .await;
    assert_eq!(
        untimed_sidekick_report(&report),
        settled_there(short, ASKED, "completed", FIXED)
    );
    owed.steered_with_nothing(
        "the tree past following is told so once, and the Remote never said to stop answering",
    )
    .await;

    owed.shutdown().await;
}
