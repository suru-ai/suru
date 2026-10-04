//! What Codex's Tool uses are in the Transcript: every item reporting a use of a Tool that no more
//! specific Activity records — an MCP tool call, a web search, an image view, an image generation,
//! a sleep — is a Tool Call, opened when the item starts and settled from the completed item.
//! Codex's items that are no Tool use stay recorded as nothing, but for a context compaction, which
//! is covered in `compactions`. The Broker's calls are covered in `broker`, and a Subagent's Tool
//! Calls in `subagents`.

use crate::support::{
    OpenedSession, ScriptedCodex, conversation_codex, opened_session, session_where,
    settled_session,
};
use suru::protocol::{Activity, ActivityStatus, MessageRole, SessionSnapshot, TurnStatus};

/// What a reader sees of one Tool Call: the server hosting its Tool, the Tool's name, its input,
/// its output, how it settled, and how many parts of its result were left out.
type Seen<'a> = (
    Option<&'a str>,
    &'a str,
    &'a str,
    &'a str,
    ActivityStatus,
    u32,
);

fn seen(activity: &Activity) -> Seen<'_> {
    let Activity::ToolCall {
        server,
        name,
        input,
        output,
        status,
        omitted_parts,
        ..
    } = activity
    else {
        panic!("every Activity here is a Tool Call, got {activity:?}");
    };
    (
        server.as_deref(),
        name.as_str(),
        input.as_str(),
        output.as_str(),
        *status,
        *omitted_parts,
    )
}

/// A Session whose one Turn a scripted Codex worked and settled, with the fixture held until the
/// server shuts down.
struct Worked {
    _codex: ScriptedCodex,
    opened: OpenedSession,
    settled: SessionSnapshot,
}

impl Worked {
    async fn shutdown(self) {
        self.opened
            .server
            .shutdown()
            .await
            .expect("shut down server");
    }
}

/// The Session a scripted Codex left once its one Turn, streaming `turn_events`, settled.
async fn worked_session(channel: &str, turn_events: &str) -> Worked {
    let codex = conversation_codex(turn_events);
    let opened = opened_session(&codex, channel, "Get to work").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    Worked {
        _codex: codex,
        opened,
        settled,
    }
}

/// Two MCP servers' tools, one answering with text alone and one with text beside an image, a
/// resource link, and audio, with progress reported on the first while it runs.
const MCP_TOOL_CALLS: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-issue","server":"github","tool":"create_issue","status":"inProgress","arguments":{"title":"Fix the seam","labels":["bug"]},"appContext":null,"mcpAppUi":null,"pluginId":null,"readOnlyHint":false,"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/mcpToolCall/progress","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"call-issue","message":"Creating the issue"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-issue","server":"github","tool":"create_issue","status":"completed","arguments":{"title":"Fix the seam","labels":["bug"]},"appContext":null,"mcpAppUi":null,"pluginId":null,"readOnlyHint":false,"result":{"content":[{"type":"text","text":"Created issue #7"}],"structuredContent":{"number":7},"_meta":null},"error":null,"durationMs":40}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-shot","server":"browser","tool":"screenshot","status":"inProgress","arguments":{},"appContext":null,"mcpAppUi":null,"pluginId":null,"readOnlyHint":null,"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-shot","server":"browser","tool":"screenshot","status":"completed","arguments":{},"appContext":null,"mcpAppUi":null,"pluginId":null,"readOnlyHint":null,"result":{"content":[{"type":"text","text":"Captured the page"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"},{"type":"resource_link","uri":"file:///shot.png","name":"shot.png"},{"type":"text","text":"Saved it beside the page"},{"type":"audio","data":"UklGRg==","mimeType":"audio/wav"}],"structuredContent":null,"_meta":null},"error":null,"durationMs":90}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"answer","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"answer","text":"Filed it."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn an_mcp_tool_call_is_named_by_its_server_and_its_result_text_is_its_output() {
    let worked = worked_session("codex-mcp-tool-calls", MCP_TOOL_CALLS).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [
            (
                Some("github"),
                "create_issue",
                r#"labels=["bug"] title=Fix the seam"#,
                "Created issue #7",
                ActivityStatus::Completed,
                0,
            ),
            (
                Some("browser"),
                "screenshot",
                "",
                "Captured the page\nSaved it beside the page",
                ActivityStatus::Completed,
                3,
            ),
        ],
        "each call is named by its server and its own name, reads its arguments as input and \
         its result's text as output, and counts the image, resource link and audio beside that \
         text as omitted; the progress Codex reported records nothing"
    );

    worked.shutdown().await;
}

/// An MCP call Codex could not make, reported with an error, and one whose tool answered with an
/// error result, which Codex reports as failed with the result standing.
const FAILED_MCP_TOOL_CALLS: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-down","server":"linear","tool":"list_issues","status":"inProgress","arguments":{"team":"core"},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-down","server":"linear","tool":"list_issues","status":"failed","arguments":{"team":"core"},"result":null,"error":{"message":"tool call error: connection refused"},"durationMs":3}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-refused","server":"github","tool":"create_issue","status":"inProgress","arguments":{"title":""},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-refused","server":"github","tool":"create_issue","status":"failed","arguments":{"title":""},"result":{"content":[{"type":"text","text":"A title is required"}],"structuredContent":null},"error":null,"durationMs":5}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_failed_mcp_tool_call_settles_failed_with_its_error_as_output() {
    let worked = worked_session("codex-failed-mcp-tool-calls", FAILED_MCP_TOOL_CALLS).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [
            (
                Some("linear"),
                "list_issues",
                "team=core",
                "tool call error: connection refused",
                ActivityStatus::Failed,
                0,
            ),
            (
                Some("github"),
                "create_issue",
                "title=",
                "A title is required",
                ActivityStatus::Failed,
                0,
            ),
        ],
        "a call Codex reports failed is Failed, with the error it reports — or the error result \
         the tool answered with — as its output"
    );

    worked.shutdown().await;
}

/// A search, a page opened, and a pattern found in a page, each started before Codex knows what it
/// searches, as Codex reports a web search.
const WEB_SEARCHES: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-search","query":"","action":null,"results":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-search","query":"rust async traits","action":{"type":"search","query":"rust async traits","queries":null},"results":null}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-open","query":"","action":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-open","query":"https://docs.rs/tokio","action":{"type":"openPage","url":"https://docs.rs/tokio"}}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-find","query":"","action":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-find","query":"spawn in https://docs.rs/tokio","action":{"type":"findInPage","url":"https://docs.rs/tokio","pattern":"spawn"}}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_web_search_reads_its_query_or_the_page_and_pattern_it_looked_in() {
    let worked = worked_session("codex-web-searches", WEB_SEARCHES).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [
            (
                None,
                "web_search",
                "query=rust async traits",
                "",
                ActivityStatus::Completed,
                0,
            ),
            (
                None,
                "web_search",
                "url=https://docs.rs/tokio",
                "",
                ActivityStatus::Completed,
                0,
            ),
            (
                None,
                "web_search",
                "pattern=spawn url=https://docs.rs/tokio",
                "",
                ActivityStatus::Completed,
                0,
            ),
        ],
        "a web search is named as Codex names the Tool, with the query it searched or the page \
         and pattern it looked in as input, and no output"
    );

    worked.shutdown().await;
}

/// An image viewed, an image generated and saved, one whose generation failed, and a sleep.
const IMAGES_AND_SLEEPS: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageView","id":"view-chart","path":"/work/chart.png"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageView","id":"view-chart","path":"/work/chart.png"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageGeneration","id":"gen-logo","status":"in_progress","revisedPrompt":null,"result":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageGeneration","id":"gen-logo","status":"completed","revisedPrompt":"A flat fox logo","result":"iVBORw0KGgo=","savedPath":"/home/me/.codex/generated_images/gen-logo.png"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageGeneration","id":"gen-banner","status":"in_progress","revisedPrompt":null,"result":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageGeneration","id":"gen-banner","status":"failed","revisedPrompt":"A wide banner","result":"","failure":{"type":"usageLimitExceeded","limitId":"images","resetsAt":null}}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"sleep","id":"nap","durationMs":1500}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"sleep","id":"nap","durationMs":1500}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn image_views_image_generations_and_sleeps_are_tool_calls() {
    let worked = worked_session("codex-images-and-sleeps", IMAGES_AND_SLEEPS).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [
            (
                None,
                "view_image",
                "path=/work/chart.png",
                "",
                ActivityStatus::Completed,
                0,
            ),
            (
                None,
                "image_gen.imagegen",
                "prompt=A flat fox logo",
                "/home/me/.codex/generated_images/gen-logo.png",
                ActivityStatus::Completed,
                0,
            ),
            (
                None,
                "image_gen.imagegen",
                "prompt=A wide banner",
                "usageLimitExceeded",
                ActivityStatus::Failed,
                0,
            ),
            (
                None,
                "clock.sleep",
                "duration_ms=1500",
                "",
                ActivityStatus::Completed,
                0,
            ),
        ],
        "an image view reads the path it viewed, an image generation the prompt it drew and the \
         path it saved the image at — or, failed, why it failed — and a sleep how long it slept"
    );

    worked.shutdown().await;
}

/// Every item Codex reports that is no use of a Tool Suru records nothing of: a review entered and
/// left, a hook's prompt, a plan, and a call to a dynamic Tool, which Suru never offers. A context
/// compaction is no Tool use either, but a Compaction of its own (see `compactions`).
const ITEMS_THAT_ARE_NO_TOOL_USE: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"enteredReviewMode","id":"review-in","review":"current changes"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"enteredReviewMode","id":"review-in","review":"current changes"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"exitedReviewMode","id":"review-out","review":"Looks good."}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"hookPrompt","id":"hook","fragments":[{"text":"Remember the style guide.","hookRunId":"run-1"}]}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"plan","id":"plan","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"plan","id":"plan","text":"1. Map the seam"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"dynamicToolCall","id":"dynamic","namespace":null,"tool":"lookup","arguments":{"key":"a"},"status":"inProgress","contentItems":null,"success":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"dynamicToolCall","id":"dynamic","namespace":null,"tool":"lookup","arguments":{"key":"a"},"status":"completed","contentItems":[{"type":"inputText","text":"found"}],"success":true,"durationMs":2}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"answer","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"answer","text":"Reviewed."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn items_that_are_no_tool_use_are_still_recorded_as_nothing() {
    let worked = worked_session("codex-items-no-tool-use", ITEMS_THAT_ARE_NO_TOOL_USE).await;

    assert!(
        worked.settled.activities.is_empty(),
        "a review, a hook prompt, a plan and a dynamic Tool call record nothing: {:?}",
        worked.settled.activities
    );
    assert_eq!(
        worked
            .settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Agent)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Reviewed."],
        "and the Turn around them is recorded as ever"
    );

    worked.shutdown().await;
}

/// An MCP call that has started and holds, with progress on it, until the test releases the
/// fixture, which then completes it and the Turn.
const HELD_MCP_TOOL_CALL: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-deploy","server":"fly","tool":"deploy","status":"inProgress","arguments":{"app":"suru-docs"},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/mcpToolCall/progress","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"call-deploy","message":"Building the image"}}'
      wait_for "$CODEX_FIXTURE_RELEASE"
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-deploy","server":"fly","tool":"deploy","status":"completed","arguments":{"app":"suru-docs"},"result":{"content":[{"type":"text","text":"Deployed v12"}],"structuredContent":null},"error":null,"durationMs":1200}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_tool_call_stands_active_from_the_moment_its_item_starts() {
    let codex = conversation_codex(HELD_MCP_TOOL_CALL);
    let opened = opened_session(&codex, "codex-held-tool-call", "Deploy the docs").await;

    let running = session_where(
        &opened.client,
        opened.session_id,
        "the started call opens its Tool Call",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;
    assert_eq!(
        running.activities.iter().map(seen).collect::<Vec<_>>(),
        [(
            Some("fly"),
            "deploy",
            "app=suru-docs",
            "",
            ActivityStatus::Active,
            0,
        )],
        "the call is Active, with its input, while Codex runs it, and its progress adds nothing"
    );

    codex.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.activities.iter().map(seen).collect::<Vec<_>>(),
        [(
            Some("fly"),
            "deploy",
            "app=suru-docs",
            "Deployed v12",
            ActivityStatus::Completed,
            0,
        )],
        "and the same row settles from the completed item"
    );

    opened.server.shutdown().await.expect("shut down server");
}

/// A web search Codex starts and never completes before the Turn does.
const DANGLING_WEB_SEARCH: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"webSearch","id":"ws-lost","query":"","action":null}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"sleep","id":"nap-lost","durationMs":60000}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_tool_call_codex_never_completed_settles_failed_when_the_turn_ends() {
    let worked = worked_session("codex-dangling-tool-call", DANGLING_WEB_SEARCH).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [
            (None, "web_search", "", "", ActivityStatus::Failed, 0),
            (
                None,
                "clock.sleep",
                "duration_ms=60000",
                "",
                ActivityStatus::Failed,
                0,
            ),
        ],
        "a Tool Call with no completed item settles as failed rather than staying active, keeping \
         what input it had"
    );

    worked.shutdown().await;
}

/// An image view Codex reports only as completed.
const COMPLETED_ONLY_IMAGE_VIEW: &str = r#"      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"imageView","id":"view-late","path":"/work/diagram.png"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

#[tokio::test]
async fn a_tool_use_codex_reports_only_once_it_completed_is_still_recorded() {
    let worked = worked_session("codex-completed-only-tool-call", COMPLETED_ONLY_IMAGE_VIEW).await;

    assert_eq!(
        worked
            .settled
            .activities
            .iter()
            .map(seen)
            .collect::<Vec<_>>(),
        [(
            None,
            "view_image",
            "path=/work/diagram.png",
            "",
            ActivityStatus::Completed,
            0,
        )],
        "a completed item Suru never saw start opens and settles its Tool Call at once"
    );

    worked.shutdown().await;
}
