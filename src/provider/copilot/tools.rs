//! What one Copilot tool execution reads as in the Transcript.

use github_copilot_sdk::session_events::ToolExecutionStartData;

/// The most characters of any one argument an invocation carries. A Tool is handed whatever the
/// Model wrote for it — a whole file, for one that creates one — and an invocation stands in a
/// header row, so each argument keeps its opening and none of them crowds out the rest.
///
/// Only an invocation is bounded, never a command: a command Copilot reported is recorded whole and
/// as Copilot reported it, while an invocation is text Suru composed for work that reported no
/// command of its own, and what Suru composes it may compose to fit.
const MAX_TOOL_ARGUMENT_CHARS: usize = 96;

/// The most characters the whole of a Tool's arguments carry, bounding a Tool handed many
/// arguments the way [`MAX_TOOL_ARGUMENT_CHARS`] bounds one handed a long one.
const MAX_TOOL_INVOCATION_CHARS: usize = 256;

/// The command text one tool execution is recorded under: the command itself when Copilot ran one,
/// and otherwise the invocation that stands in for it.
pub(super) fn command_text(started: &ToolExecutionStartData) -> String {
    shell_command(started).unwrap_or_else(|| tool_invocation(started))
}

/// What a Tool that runs no command reads as: the Tool Copilot invoked and what it handed it, which
/// is everything Copilot says about work that has no command text of its own.
fn tool_invocation(started: &ToolExecutionStartData) -> String {
    let name = tool_name(started);
    match rendered_arguments(started) {
        Some(arguments) => format!("{name} {arguments}"),
        None => name,
    }
}

/// What to call the Tool. A Tool an MCP server hosts is named by that server and the name it goes
/// by there, because the name Copilot reaches it under is a wire name the server chose nothing of.
fn tool_name(started: &ToolExecutionStartData) -> String {
    let Some(server) = started
        .mcp_server_name
        .as_deref()
        .filter(|name| !name.is_empty())
    else {
        return started.tool_name.clone();
    };
    let hosted = started
        .mcp_tool_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(started.tool_name.as_str());
    format!("{server}/{hosted}")
}

/// The arguments a Tool was given, on the one line an Activity header is, or nothing when it was
/// given none worth reading.
fn rendered_arguments(started: &ToolExecutionStartData) -> Option<String> {
    let arguments = started.arguments.as_ref()?;
    if arguments.is_null() || matches!(arguments.as_object(), Some(fields) if fields.is_empty()) {
        return None;
    }
    // Serialized compactly, so every escape a value carries — newlines above all — stays an escape
    // rather than becoming a second line of a row that has room for one.
    let rendered = serde_json::to_string(&bounded_strings(arguments)).ok()?;
    Some(bounded_chars(&rendered, MAX_TOOL_INVOCATION_CHARS))
}

/// `value` with every string in it cut to what a header row can carry, so the argument a reader
/// recognizes the work by is not lost behind the one the Model wrote at length.
fn bounded_strings(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(bounded_chars(text, MAX_TOOL_ARGUMENT_CHARS))
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(bounded_strings).collect())
        }
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(field, value)| (field.clone(), bounded_strings(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// `text` cut to `limit` characters, marking that it was cut.
fn bounded_chars(text: &str, limit: usize) -> String {
    let mut characters = text.chars();
    let mut bounded = characters.by_ref().take(limit).collect::<String>();
    if characters.next().is_some() {
        bounded.push('\u{2026}');
    }
    bounded
}

/// The command a shell tool ran, or nothing when the execution is not one.
///
/// Copilot's shell driver rewrites what the Model asked for before spawning it — dropping the
/// redundant `cd` into the working directory the Session already runs in — and reports the rewrite
/// beside the arguments. That rewrite is the command as it actually ran, so it is the command a
/// reader should see; the arguments answer for a shape Copilot rewrote nothing in.
fn shell_command(started: &ToolExecutionStartData) -> Option<String> {
    let launched = started
        .shell_tool_info
        .as_ref()
        .and_then(|shell| shell.display_command.as_deref());
    let requested = started
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get("command"))
        .and_then(serde_json::Value::as_str);
    launched
        .or(requested)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use github_copilot_sdk::session_events::ToolExecutionStartShellToolInfo;

    use super::*;

    fn started(tool_name: &str, arguments: serde_json::Value) -> ToolExecutionStartData {
        ToolExecutionStartData {
            tool_call_id: "t1".to_owned(),
            tool_name: tool_name.to_owned(),
            arguments: Some(arguments),
            ..Default::default()
        }
    }

    #[test]
    fn a_tool_that_runs_no_command_reads_as_the_tool_and_what_it_was_given() {
        assert_eq!(
            command_text(&started(
                "view",
                serde_json::json!({ "path": "src/provider/copilot.rs" }),
            )),
            r#"view {"path":"src/provider/copilot.rs"}"#
        );
    }

    #[test]
    fn a_tool_handed_a_whole_file_still_names_every_argument_beside_it() {
        let text = command_text(&started(
            "create",
            serde_json::json!({ "path": "notes.md", "content": "line\n".repeat(4_096) }),
        ));

        assert!(
            text.contains(r#""path":"notes.md""#),
            "the argument a reader recognizes the work by survives the one that dwarfs it, got: {text}"
        );
        assert!(
            text.contains(r#""content":"line\nline\n"#),
            "the argument that dwarfs the rest is still shown, as much of it as fits: {text}"
        );
        assert!(
            text.chars().count() <= MAX_TOOL_INVOCATION_CHARS + "create ".len(),
            "a tool invocation stays a header row rather than a file, got {} characters",
            text.chars().count()
        );
    }

    #[test]
    fn a_tool_given_nothing_reads_as_the_tool_alone() {
        let mut started = started("list_agents", serde_json::Value::Null);
        started.arguments = None;

        assert_eq!(command_text(&started), "list_agents");
    }

    #[test]
    fn a_shell_tools_launcher_plumbing_is_off_the_command_a_reader_sees() {
        let mut started = started(
            "bash",
            serde_json::json!({ "command": "cd /work && cargo nextest run" }),
        );
        started.shell_tool_info = Some(ToolExecutionStartShellToolInfo {
            display_command: Some("cargo nextest run".to_owned()),
            has_write_file_redirection: false,
            possible_paths: Vec::new(),
        });

        assert_eq!(command_text(&started), "cargo nextest run");
    }

    #[test]
    fn a_shell_tool_in_a_shape_copilot_reported_no_command_in_keeps_what_it_did_report() {
        assert_eq!(
            command_text(&started(
                "local_shell",
                serde_json::json!({ "action": { "command": ["bash", "-lc", "ls"] } }),
            )),
            r#"local_shell {"action":{"command":["bash","-lc","ls"]}}"#,
            "a shape Suru cannot read a command out of keeps everything Copilot said about it"
        );
    }

    #[test]
    fn a_tool_an_mcp_server_hosts_is_named_by_the_server_and_the_name_it_goes_by_there() {
        let mut started = started("mcp__linear__list_issues", serde_json::json!({}));
        started.mcp_server_name = Some("linear".to_owned());
        started.mcp_tool_name = Some("list_issues".to_owned());

        assert_eq!(command_text(&started), "linear/list_issues");
    }

    #[test]
    fn a_shell_tool_records_the_command_it_was_asked_to_run() {
        assert_eq!(
            command_text(&started(
                "bash",
                serde_json::json!({ "command": "cargo nextest run", "description": "Run the tests" }),
            )),
            "cargo nextest run"
        );
    }
}
