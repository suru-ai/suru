//! What one Codex Tool use is to a Transcript, for the items that report nothing but a Tool's
//! use: an MCP tool call, a web search, an image view, an image generation, and a sleep.
//!
//! Codex reports a shell run, an edit, and a collab delegation as items of kinds their own
//! Activities read, so every use reaching here is a Tool Call — but a Broker call that spawns,
//! sends to, or stops a Subagent, which the Subagent row it affects records, so that the call
//! itself is recorded as nothing more.
//!
//! An MCP server's Tool is named by that server and the name it goes by there. Codex's own Tools
//! are named as Codex names them to the Model: `web_search` and `view_image`, and the namespaced
//! `image_gen.imagegen` and `clock.sleep` spelled as Codex's own descriptions of them spell them.
//!
//! Each input reads as the arguments the Tool was called with would, on the one line a
//! [`ToolCallInput`] renders them on — an MCP call's own arguments, and for Codex's own
//! Tools what the item reports of theirs: a search's query, or the page it opened or the pattern
//! it looked for there; the path an image was viewed at; the prompt an image was drawn from; how
//! long a sleep lasted. The output is the text a result carried — an MCP result's text, with the
//! error of a failed call below it, or the path a generated image was saved at, or the reason a
//! generation failed with its message below it — and a Tool whose result the item does not carry
//! has none.

use serde_json::{Map, Value};

use super::wire::{
    NativeImageGenerationFailure, NativeImageGenerationStatus, NativeMcpContent,
    NativeToolCallStatus, NativeToolUse, NativeWebSearch, NativeWebSearchAction,
};
use crate::{
    broker::{BROKER_SERVER_NAME, tool_is_recorded_by_its_row},
    provider::{ProviderToolCallStatus, ToolCallInput},
};

/// Codex's web search Tool, which searches the web and opens and looks through the pages it finds.
const WEB_SEARCH_TOOL: &str = "web_search";

/// Codex's Tool for showing the Model an image from the filesystem.
const VIEW_IMAGE_TOOL: &str = "view_image";

/// Codex's image generation Tool: `imagegen` in the `image_gen` namespace.
const IMAGE_GENERATION_TOOL: &str = "image_gen.imagegen";

/// Codex's interruptible sleep: `sleep` in the `clock` namespace.
const SLEEP_TOOL: &str = "clock.sleep";

/// A Tool Call as far as one item of its use says: the Tool's name, the MCP server hosting it
/// where one does, and its input, where the item already carries it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct PresentedToolCall {
    pub(super) name: String,
    pub(super) server: Option<String>,
    pub(super) input: Option<ToolCallInput>,
}

/// How a Tool Call settles, read from the completed item of its use.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ToolCallOutcome {
    pub(super) output: String,
    pub(super) status: ProviderToolCallStatus,
    /// How many parts of the result were no text and so are left out of `output`.
    pub(super) omitted_parts: u32,
}

impl NativeToolUse {
    /// The item this use is reported as, whose identity names its Tool Call.
    pub(super) fn item_id(&self) -> &str {
        match self {
            Self::Mcp(call) => &call.id,
            Self::WebSearch(search) => &search.id,
            Self::ImageView(view) => &view.id,
            Self::ImageGeneration(generation) => &generation.id,
            Self::Sleep(sleep) => &sleep.id,
        }
    }

    /// Whether another Activity records this use, so that it is no Tool Call: a Broker call whose
    /// effect is the row it opens or settles: a Subagent it spawns, sends to, or stops, or a Session
    /// it begins.
    pub(super) fn is_recorded_elsewhere(&self) -> bool {
        matches!(
            self,
            Self::Mcp(call)
                if call.server == BROKER_SERVER_NAME && tool_is_recorded_by_its_row(&call.tool)
        )
    }

    /// The Tool Call this use is recorded as, as far as this item of it says.
    pub(super) fn tool_call(&self) -> PresentedToolCall {
        let (name, server, arguments) = match self {
            Self::Mcp(call) => (
                call.tool.clone(),
                Some(call.server.clone()),
                Some(call.arguments.clone()),
            ),
            Self::WebSearch(search) => (WEB_SEARCH_TOOL.to_owned(), None, web_search_input(search)),
            Self::ImageView(view) => (
                VIEW_IMAGE_TOOL.to_owned(),
                None,
                Some(arguments([("path", Some(Value::from(view.path.as_str())))])),
            ),
            Self::ImageGeneration(generation) => (
                IMAGE_GENERATION_TOOL.to_owned(),
                None,
                generation
                    .revised_prompt
                    .as_deref()
                    .map(|prompt| arguments([("prompt", Some(Value::from(prompt)))])),
            ),
            Self::Sleep(sleep) => (
                SLEEP_TOOL.to_owned(),
                None,
                sleep
                    .duration_ms
                    .map(|duration| arguments([("duration_ms", Some(Value::from(duration)))])),
            ),
        };
        PresentedToolCall {
            input: arguments
                .map(|arguments| ToolCallInput::of(server.as_deref(), &name, &arguments)),
            name,
            server,
        }
    }

    /// How the Tool Call settles, when this is the completed item of its use.
    pub(super) fn outcome(&self) -> ToolCallOutcome {
        match self {
            Self::Mcp(call) => {
                let content = call
                    .result
                    .as_ref()
                    .map_or(&[][..], |result| result.content.as_slice());
                let mut output = content
                    .iter()
                    .filter_map(|block| match block {
                        NativeMcpContent::Text(text) => Some(text.as_str()),
                        NativeMcpContent::Omitted => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let omitted = content
                    .iter()
                    .filter(|block| matches!(block, NativeMcpContent::Omitted))
                    .count();
                if let Some(error) = &call.error {
                    push_error(&mut output, &error.message);
                }
                ToolCallOutcome {
                    output,
                    status: if call.status == NativeToolCallStatus::Failed || call.error.is_some() {
                        ProviderToolCallStatus::Failed
                    } else {
                        ProviderToolCallStatus::Completed
                    },
                    omitted_parts: u32::try_from(omitted).unwrap_or(u32::MAX),
                }
            }
            Self::ImageGeneration(generation) => {
                if generation.status == NativeImageGenerationStatus::Failed {
                    ToolCallOutcome {
                        output: generation
                            .failure
                            .as_ref()
                            .map(image_generation_failure)
                            .unwrap_or_default(),
                        status: ProviderToolCallStatus::Failed,
                        omitted_parts: 0,
                    }
                } else {
                    ToolCallOutcome {
                        output: generation.saved_path.clone().unwrap_or_default(),
                        status: ProviderToolCallStatus::Completed,
                        omitted_parts: 0,
                    }
                }
            }
            Self::WebSearch(_) | Self::ImageView(_) | Self::Sleep(_) => ToolCallOutcome {
                output: String::new(),
                status: ProviderToolCallStatus::Completed,
                omitted_parts: 0,
            },
        }
    }
}

/// A web search's input: the query it searched — or, where it gave several and no one query,
/// them all — or the page it opened, or the page and the pattern it looked for there. Codex's
/// own reading of the search stands in for an action this build does not know, and a search not
/// yet named has no input.
fn web_search_input(search: &NativeWebSearch) -> Option<Value> {
    let read = search
        .query
        .as_deref()
        .filter(|query| !query.is_empty())
        .map(Value::from);
    let input = match &search.action {
        Some(NativeWebSearchAction::Search { query, queries }) => {
            match (query.as_deref().filter(|query| !query.is_empty()), queries) {
                (None, Some(queries)) if !queries.is_empty() => {
                    arguments([("queries", Some(Value::from(queries.clone())))])
                }
                (query, _) => arguments([("query", query.map(Value::from).or(read))]),
            }
        }
        Some(NativeWebSearchAction::OpenPage { url }) => {
            arguments([("url", url.as_deref().map(Value::from))])
        }
        Some(NativeWebSearchAction::FindInPage { url, pattern }) => arguments([
            ("url", url.as_deref().map(Value::from)),
            ("pattern", pattern.as_deref().map(Value::from)),
        ]),
        Some(NativeWebSearchAction::Other) | None => arguments([("query", read)]),
    };
    input
        .as_object()
        .is_some_and(|arguments| !arguments.is_empty())
        .then_some(input)
}

/// The arguments a Tool called with `pairs` was given, leaving out each it was not.
fn arguments<const N: usize>(pairs: [(&str, Option<Value>); N]) -> Value {
    Value::Object(
        pairs
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_owned(), value?)))
            .collect::<Map<_, _>>(),
    )
}

/// What a failed image generation's output reads: the reason Codex gave, as Codex spells it, with
/// any message that came with it below.
fn image_generation_failure(failure: &NativeImageGenerationFailure) -> String {
    let mut output = failure.reason.clone();
    if let Some(message) = &failure.message {
        push_error(&mut output, message);
    }
    output
}

/// Adds a failed call's error below the result text `output` holds, unless the result already
/// ends by saying it.
fn push_error(output: &mut String, error: &str) {
    if error.trim().is_empty() || output.trim_end().ends_with(error.trim_end()) {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(error);
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::wire::{NativeToolUse, tool_use_item as tool_use};
    use super::{PresentedToolCall, ToolCallInput, ToolCallOutcome};
    use crate::provider::ProviderToolCallStatus;

    fn mcp(server: &str, tool: &str, status: &str, result: Value, error: Value) -> NativeToolUse {
        tool_use(json!({
            "type": "mcpToolCall",
            "id": "call",
            "server": server,
            "tool": tool,
            "status": status,
            "arguments": {"session_id": "child"},
            "result": result,
            "error": error,
            "durationMs": null,
        }))
    }

    fn presented(name: &str, server: Option<&str>, input: Option<&str>) -> PresentedToolCall {
        PresentedToolCall {
            name: name.to_owned(),
            server: server.map(str::to_owned),
            input: input.map(ToolCallInput::rendered),
        }
    }

    #[test]
    fn only_the_brokers_calls_that_stand_as_a_row_of_their_own_are_recorded_elsewhere() {
        for tool in [
            "spawn_subagent",
            "send_to_subagent",
            "stop_subagent",
            "begin_session",
        ] {
            assert!(
                mcp("suru", tool, "inProgress", Value::Null, Value::Null).is_recorded_elsewhere(),
                "{tool} is its row's to record"
            );
        }
        for tool in [
            "list_providers",
            "read_subagent",
            "wait_subagents",
            "list_sessions",
            "send_prompt",
        ] {
            let call = mcp("suru", tool, "inProgress", Value::Null, Value::Null);
            assert!(!call.is_recorded_elsewhere(), "{tool} is a Tool Call");
            assert_eq!(
                call.tool_call(),
                presented(tool, Some("suru"), Some("session_id=child"))
            );
        }
        assert!(
            !mcp(
                "elsewhere",
                "spawn_subagent",
                "inProgress",
                Value::Null,
                Value::Null
            )
            .is_recorded_elsewhere(),
            "another server's Tool of the same name is a Tool Call"
        );
    }

    #[test]
    fn an_mcp_result_reads_its_text_and_counts_every_other_block_as_omitted() {
        let call = mcp(
            "browser",
            "screenshot",
            "completed",
            json!({"content": [
                {"type": "text", "text": "Captured"},
                {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "not output"}},
                {"type": "text", "text": "Saved"},
                {"type": "futureBlock"},
                {"no": "type"},
            ]}),
            Value::Null,
        );
        assert_eq!(
            call.outcome(),
            ToolCallOutcome {
                output: "Captured\nSaved".to_owned(),
                status: ProviderToolCallStatus::Completed,
                omitted_parts: 4,
            }
        );
    }

    #[test]
    fn a_failed_mcp_call_is_failed_with_its_error_below_any_result() {
        let error_only = mcp(
            "linear",
            "list_issues",
            "failed",
            Value::Null,
            json!({"message": "connection refused"}),
        );
        assert_eq!(
            error_only.outcome(),
            ToolCallOutcome {
                output: "connection refused".to_owned(),
                status: ProviderToolCallStatus::Failed,
                omitted_parts: 0,
            }
        );
        let both = mcp(
            "linear",
            "list_issues",
            "failed",
            json!({"content": [{"type": "text", "text": "partial"}]}),
            json!({"message": "timed out"}),
        );
        assert_eq!(both.outcome().output, "partial\ntimed out");
        let restated = mcp(
            "linear",
            "list_issues",
            "failed",
            json!({"content": [{"type": "text", "text": "Error: timed out\n"}]}),
            json!({"message": "timed out"}),
        );
        assert_eq!(
            restated.outcome().output,
            "Error: timed out\n",
            "an error the result already ends by saying is not repeated"
        );
        let error_result = mcp(
            "github",
            "create_issue",
            "failed",
            json!({"content": [{"type": "text", "text": "A title is required"}]}),
            Value::Null,
        );
        assert_eq!(
            error_result.outcome(),
            ToolCallOutcome {
                output: "A title is required".to_owned(),
                status: ProviderToolCallStatus::Failed,
                omitted_parts: 0,
            },
            "a result the Tool marked as an error fails the call with the result standing"
        );
        let unknown = mcp(
            "github",
            "create_issue",
            "futureStatus",
            Value::Null,
            json!({"message": "refused"}),
        );
        assert_eq!(
            unknown.outcome().status,
            ProviderToolCallStatus::Failed,
            "an error fails a call whatever status it is reported under"
        );
    }

    fn web_search(query: &str, action: Value) -> NativeToolUse {
        tool_use(json!({"type": "webSearch", "id": "ws", "query": query, "action": action}))
    }

    #[test]
    fn a_web_search_reads_what_it_searched_once_codex_says() {
        let input = |search: NativeToolUse| search.tool_call().input;
        assert_eq!(
            input(web_search("", Value::Null)),
            None,
            "a search started before Codex knows its query has no input yet"
        );
        assert_eq!(
            input(web_search(
                "rust",
                json!({"type": "search", "query": "rust", "queries": null})
            ))
            .as_ref()
            .map(ToolCallInput::as_str),
            Some("query=rust")
        );
        assert_eq!(
            input(web_search(
                "rust ...",
                json!({"type": "search", "query": null, "queries": ["rust", "tokio"]})
            ))
            .as_ref()
            .map(ToolCallInput::as_str),
            Some(r#"queries=["rust","tokio"]"#),
            "several queries with no one query are read in full"
        );
        assert_eq!(
            input(web_search(
                "https://docs.rs",
                json!({"type": "openPage", "url": "https://docs.rs"})
            ))
            .as_ref()
            .map(ToolCallInput::as_str),
            Some("url=https://docs.rs")
        );
        assert_eq!(
            input(web_search(
                "spawn in https://docs.rs",
                json!({"type": "findInPage", "url": "https://docs.rs", "pattern": "spawn"})
            ))
            .as_ref()
            .map(ToolCallInput::as_str),
            Some("pattern=spawn url=https://docs.rs")
        );
        assert_eq!(
            input(web_search(
                "what Codex read",
                json!({"type": "futureAction"})
            ))
            .as_ref()
            .map(ToolCallInput::as_str),
            Some("query=what Codex read"),
            "Codex's own reading stands in for an action this build does not know"
        );
        assert_eq!(
            web_search("rust", Value::Null).tool_call(),
            presented("web_search", None, Some("query=rust"))
        );
        assert_eq!(
            web_search("rust", Value::Null).outcome(),
            ToolCallOutcome {
                output: String::new(),
                status: ProviderToolCallStatus::Completed,
                omitted_parts: 0,
            }
        );
    }

    #[test]
    fn codexs_own_tools_are_named_as_codex_names_them() {
        let view = tool_use(json!({"type": "imageView", "id": "view", "path": "chart.png"}));
        assert_eq!(
            view.tool_call(),
            presented("view_image", None, Some("path=chart.png"))
        );
        assert_eq!(view.outcome().output, "");

        let sleep = tool_use(json!({"type": "sleep", "id": "nap", "durationMs": 1500}));
        assert_eq!(
            sleep.tool_call(),
            presented("clock.sleep", None, Some("duration_ms=1500"))
        );
        assert_eq!(sleep.outcome().status, ProviderToolCallStatus::Completed);

        let started = tool_use(json!({
            "type": "imageGeneration", "id": "gen", "status": "in_progress",
            "revisedPrompt": null, "result": "",
        }));
        assert_eq!(
            started.tool_call(),
            presented("image_gen.imagegen", None, None),
            "a generation started before its prompt is settled has no input yet"
        );
        let drawn = tool_use(json!({
            "type": "imageGeneration", "id": "gen", "status": "completed",
            "revisedPrompt": "A fox", "result": "iVBORw0KGgo=", "savedPath": "fox.png",
        }));
        assert_eq!(
            drawn.tool_call().input.as_ref().map(ToolCallInput::as_str),
            Some("prompt=A fox")
        );
        assert_eq!(
            drawn.outcome(),
            ToolCallOutcome {
                output: "fox.png".to_owned(),
                status: ProviderToolCallStatus::Completed,
                omitted_parts: 0,
            }
        );
        let failed = tool_use(json!({
            "type": "imageGeneration", "id": "gen", "status": "failed",
            "revisedPrompt": "A fox", "result": "",
            "failure": {"type": "usageLimitExceeded", "limitId": "images", "resetsAt": null},
        }));
        assert_eq!(
            failed.outcome(),
            ToolCallOutcome {
                output: "usageLimitExceeded".to_owned(),
                status: ProviderToolCallStatus::Failed,
                omitted_parts: 0,
            }
        );
    }

    #[test]
    fn a_failed_image_generation_says_why_it_failed() {
        let failed = |failure: Value| {
            tool_use(json!({
                "type": "imageGeneration", "id": "gen", "status": "failed",
                "revisedPrompt": "A fox", "result": "", "failure": failure,
            }))
            .outcome()
        };
        assert_eq!(
            failed(json!({"type": "contentPolicy", "message": "The prompt was refused."})).output,
            "contentPolicy\nThe prompt was refused.",
            "a failure this build has not heard of is read by its reason, its message below"
        );
        assert_eq!(
            failed(json!({"type": "usageLimitExceeded", "message": ""})).output,
            "usageLimitExceeded",
            "an empty message adds nothing"
        );
        assert_eq!(
            failed(Value::Null),
            ToolCallOutcome {
                output: String::new(),
                status: ProviderToolCallStatus::Failed,
                omitted_parts: 0,
            },
            "a failure Codex gives no reason for is still failed"
        );
        let completed = tool_use(json!({
            "type": "imageGeneration", "id": "gen", "status": "completed",
            "revisedPrompt": "A fox", "result": "iVBORw0KGgo=", "savedPath": "fox.png",
            "failure": null,
        }));
        assert_eq!(completed.outcome().output, "fox.png");
    }
}
