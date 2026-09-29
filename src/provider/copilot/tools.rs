//! What one Copilot tool execution is to a Transcript: which Activity records it, if any does, and
//! how that Activity presents it.
//!
//! Only a real shell execution is a Command, presented with Copilot's launcher plumbing off it. A
//! Tool whose execution another Activity records — a spawn its Subagent row, a send the Delegation
//! it delivers, a question its Questionnaire, a Broker call the Subagent row it affects — is
//! recorded as nothing more, and so is Copilot's plumbing. Every other execution is a Tool Call.

use github_copilot_sdk::session_events::ToolExecutionStartData;
use serde_json::Value;

use super::super::command_presentation::{PresentedCommand, present_command};
use super::super::tool_call_presentation::present_tool_input;
use crate::broker::{BROKER_SERVER_NAME, tool_affects_a_subagent_row};
use crate::provider::ProviderActivityId;

/// Copilot's shell tools, whose executions run the command they are handed. Copilot reports shell
/// details beside each such execution, and a Tool reported with them is a shell tool whatever its
/// name; the names answer for a CLI reporting none.
const SHELL_TOOLS: [&str; 3] = ["bash", "powershell", "local_shell"];

/// The tool that spawns a Subagent. Its execution is not work of its own: the Subagent row its
/// `subagent.started` opens is the delegation's representation, so the execution's Tool Call is
/// withheld rather than showing the reader the same delegation twice.
pub(super) const SPAWN_TOOL: &str = "task";

/// The tool an Agent sends a message to another agent's loop with — the main agent to a Subagent,
/// or one Subagent to a sibling. Its execution is not work of its own either: what it sends is a
/// Delegation, which stands in the Subagent that receives it where that Subagent received it —
/// as a steer, when Copilot delivers it into the working stretch, or opening the Turn it resumes
/// the settled Subagent into — and never in the sender's Transcript, whose only trace of it is a
/// resume's row, so the execution projects nothing, whatever it came to.
pub(super) const WRITE_AGENT_TOOL: &str = "write_agent";

/// The tool the agent asks the user through. Its user-input request is the Questionnaire (see
/// [`super::questionnaire`]), which records the use.
const QUESTIONNAIRE_TOOL: &str = "ask_user";

/// Copilot's plumbing: tools that only shape the CLI's own interface, whose executions are
/// recorded as nothing. `report_intent` tells that interface what the agent is doing, and
/// `task_complete` tells the CLI the agent considers its task done. Like the launcher plumbing
/// stripped from a shell tool's commands, they are Copilot's own, so the list lives here beside
/// that stripping.
const PLUMBING_TOOLS: [&str; 2] = ["report_intent", "task_complete"];

/// What one tool execution is to a Transcript, decided as it starts: which Activity records it, if
/// any does. A Tool Call is the fallback for every execution no more specific Activity records and
/// that is no plumbing, so a Tool this build has never heard of is a Tool Call.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum ToolDisposition {
    /// A shell execution, recorded as a Command running the command as a reader should see it.
    Command(PresentedCommand),
    /// The [`SPAWN_TOOL`], whose delegation is the Subagent row its `subagent.started` opens.
    Spawn,
    /// The [`WRITE_AGENT_TOOL`], whose send is a Delegation in the Subagent it reaches.
    WriteAgent,
    /// `ask_user`, whose use is its Questionnaire.
    Questionnaire,
    /// A Broker call whose effect is the Subagent row it spawns, sends to, or stops.
    BrokeredDelegation,
    /// Copilot's own plumbing, recorded as nothing.
    Plumbing,
    /// Every other tool execution: a Tool Call.
    ToolCall,
}

impl ToolDisposition {
    pub(super) fn of(started: &ToolExecutionStartData) -> Self {
        if let Some(server) = mcp_server(started) {
            return if server == BROKER_SERVER_NAME && tool_affects_a_subagent_row(tool(started)) {
                Self::BrokeredDelegation
            } else {
                Self::ToolCall
            };
        }
        match started.tool_name.as_str() {
            SPAWN_TOOL => Self::Spawn,
            WRITE_AGENT_TOOL => Self::WriteAgent,
            QUESTIONNAIRE_TOOL => Self::Questionnaire,
            name if PLUMBING_TOOLS.contains(&name) => Self::Plumbing,
            // `edit`, `create` and `apply_patch` are File Changes' to record (#450); until they
            // are, they fall through to the fallback like any other Tool.
            _ => shell_command(started).map_or(Self::ToolCall, |command| {
                Self::Command(present_command(command))
            }),
        }
    }
}

/// A Tool Call as it opens: the Tool's own name, the MCP server hosting it where one does, and its
/// arguments on the one line its row reads them on.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct PresentedToolCall {
    pub(super) name: String,
    pub(super) server: Option<String>,
    pub(super) input: String,
}

/// What the Tool Call a tool execution is recorded as reads as.
pub(super) fn presented_tool_call(started: &ToolExecutionStartData) -> PresentedToolCall {
    PresentedToolCall {
        name: tool(started).to_owned(),
        server: mcp_server(started).map(str::to_owned),
        input: present_tool_input(started.arguments.as_ref().unwrap_or(&Value::Null)),
    }
}

/// Names the Activity one tool execution projects onto, whichever kind records it — a Command, or
/// a Tool Call — so an Approval naming the tool call links to whichever row it became. Copilot
/// draws tool call and Reasoning identities from namespaces of their own, which the Provider seam
/// gives one identity space, so what tells them apart there is where they came from.
pub(super) fn tool_activity_id(tool_call_id: &str) -> ProviderActivityId {
    ProviderActivityId::new(format!("tool:{tool_call_id}"))
}

/// The MCP server hosting the Tool, when one does.
fn mcp_server(started: &ToolExecutionStartData) -> Option<&str> {
    started
        .mcp_server_name
        .as_deref()
        .filter(|name| !name.is_empty())
}

/// The Tool's own name: for a Tool an MCP server hosts, the name it goes by there, because the
/// name Copilot reaches it under is a wire name the server chose nothing of.
fn tool(started: &ToolExecutionStartData) -> &str {
    started
        .mcp_tool_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(started.tool_name.as_str())
}

/// The command a shell tool ran, or nothing when the execution is no shell tool's or carries no
/// command Suru can read.
///
/// Copilot's shell driver rewrites what the Model asked for before spawning it — dropping the
/// redundant `cd` into the working directory the Session already runs in — and reports the rewrite
/// beside the arguments. That rewrite is the command as it actually ran, so it is the command a
/// reader should see; the arguments answer for a shape Copilot rewrote nothing in.
fn shell_command(started: &ToolExecutionStartData) -> Option<String> {
    if started.shell_tool_info.is_none() && !SHELL_TOOLS.contains(&started.tool_name.as_str()) {
        return None;
    }
    let launched = started
        .shell_tool_info
        .as_ref()
        .and_then(|shell| shell.display_command.as_deref());
    let requested = started
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get("command"))
        .and_then(Value::as_str);
    launched
        .or(requested)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use github_copilot_sdk::session_events::ToolExecutionStartShellToolInfo;
    use serde_json::json;

    use super::*;

    fn started(tool_name: &str, arguments: Value) -> ToolExecutionStartData {
        ToolExecutionStartData {
            tool_call_id: "t1".to_owned(),
            tool_name: tool_name.to_owned(),
            arguments: Some(arguments),
            ..Default::default()
        }
    }

    fn hosted(server: &str, tool: &str, arguments: Value) -> ToolExecutionStartData {
        ToolExecutionStartData {
            mcp_server_name: Some(server.to_owned()),
            mcp_tool_name: Some(tool.to_owned()),
            ..started(&format!("{server}-{tool}"), arguments)
        }
    }

    fn shell_info(display_command: Option<&str>) -> Option<ToolExecutionStartShellToolInfo> {
        Some(ToolExecutionStartShellToolInfo {
            display_command: display_command.map(str::to_owned),
            has_write_file_redirection: false,
            possible_paths: Vec::new(),
        })
    }

    #[test]
    fn a_shell_tool_records_the_command_it_was_asked_to_run() {
        assert_eq!(
            ToolDisposition::of(&started(
                "bash",
                json!({ "command": "cargo nextest run", "description": "Run the tests" }),
            )),
            ToolDisposition::Command(PresentedCommand {
                command: "cargo nextest run".to_owned(),
                cwd: None,
            })
        );
    }

    #[test]
    fn a_shell_tools_launcher_plumbing_is_off_the_command_a_reader_sees() {
        let mut started = started(
            "bash",
            json!({ "command": "cd /work && cargo nextest run" }),
        );
        started.shell_tool_info = shell_info(Some("cargo nextest run"));

        assert_eq!(
            ToolDisposition::of(&started),
            ToolDisposition::Command(PresentedCommand {
                command: "cargo nextest run".to_owned(),
                cwd: None,
            })
        );
    }

    #[test]
    fn a_shell_tools_change_into_another_directory_becomes_the_directory_it_runs_in() {
        let elsewhere = if cfg!(windows) {
            "C:/elsewhere"
        } else {
            "/elsewhere"
        };
        let mut started = started(
            "bash",
            json!({ "command": format!("cd {elsewhere} && ls") }),
        );
        started.shell_tool_info = shell_info(Some(&format!("cd {elsewhere} && ls")));

        assert_eq!(
            ToolDisposition::of(&started),
            ToolDisposition::Command(PresentedCommand {
                command: "ls".to_owned(),
                cwd: Some(elsewhere.into()),
            })
        );
    }

    #[test]
    fn a_tool_copilot_reports_shell_details_for_is_a_shell_tool_whatever_its_name() {
        let mut started = started("run_in_terminal", json!({ "command": "ls" }));
        started.shell_tool_info = shell_info(None);

        assert_eq!(
            ToolDisposition::of(&started),
            ToolDisposition::Command(PresentedCommand {
                command: "ls".to_owned(),
                cwd: None,
            })
        );
    }

    #[test]
    fn a_tool_handed_a_command_argument_that_is_no_shell_tool_is_a_tool_call() {
        assert_eq!(
            ToolDisposition::of(&started(
                "str_replace_editor",
                json!({ "command": "view", "path": "src/lib.rs" }),
            )),
            ToolDisposition::ToolCall
        );
        assert_eq!(
            ToolDisposition::of(&hosted("docker", "exec", json!({ "command": "ls" }))),
            ToolDisposition::ToolCall,
            "an MCP server's Tool is never Copilot's shell"
        );
    }

    #[test]
    fn a_shell_tool_in_a_shape_copilot_reported_no_command_in_is_a_tool_call_keeping_it_all() {
        let started = started(
            "local_shell",
            json!({ "action": { "command": ["bash", "-lc", "ls"] } }),
        );

        assert_eq!(ToolDisposition::of(&started), ToolDisposition::ToolCall);
        assert_eq!(
            presented_tool_call(&started),
            PresentedToolCall {
                name: "local_shell".to_owned(),
                server: None,
                input: r#"action={"command":["bash","-lc","ls"]}"#.to_owned(),
            },
            "a shape Suru cannot read a command out of keeps everything Copilot said about it"
        );
    }

    #[test]
    fn a_tool_that_runs_no_command_is_a_tool_call_named_as_copilot_names_it() {
        let started = started("view", json!({ "path": "src/provider/copilot.rs" }));

        assert_eq!(ToolDisposition::of(&started), ToolDisposition::ToolCall);
        assert_eq!(
            presented_tool_call(&started),
            PresentedToolCall {
                name: "view".to_owned(),
                server: None,
                input: "path=src/provider/copilot.rs".to_owned(),
            }
        );
    }

    #[test]
    fn a_tool_given_nothing_shows_no_input() {
        let mut started = started("list_agents", Value::Null);
        started.arguments = None;

        assert_eq!(ToolDisposition::of(&started), ToolDisposition::ToolCall);
        assert_eq!(presented_tool_call(&started).input, "");
    }

    #[test]
    fn a_tool_an_mcp_server_hosts_is_named_by_the_server_and_the_name_it_goes_by_there() {
        let started = hosted("linear", "list_issues", json!({ "team": "core" }));

        assert_eq!(ToolDisposition::of(&started), ToolDisposition::ToolCall);
        assert_eq!(
            presented_tool_call(&started),
            PresentedToolCall {
                name: "list_issues".to_owned(),
                server: Some("linear".to_owned()),
                input: "team=core".to_owned(),
            }
        );
    }

    #[test]
    fn tools_another_activity_records_and_copilots_plumbing_are_no_tool_calls() {
        for (tool, disposition) in [
            ("task", ToolDisposition::Spawn),
            ("write_agent", ToolDisposition::WriteAgent),
            ("ask_user", ToolDisposition::Questionnaire),
            ("report_intent", ToolDisposition::Plumbing),
            ("task_complete", ToolDisposition::Plumbing),
            ("read_agent", ToolDisposition::ToolCall),
            ("list_agents", ToolDisposition::ToolCall),
        ] {
            assert_eq!(
                ToolDisposition::of(&started(tool, json!({}))),
                disposition,
                "{tool}"
            );
        }
    }

    #[test]
    fn only_the_broker_calls_that_affect_a_subagent_row_are_absorbed_by_it() {
        for (tool, disposition) in [
            ("spawn_subagent", ToolDisposition::BrokeredDelegation),
            ("send_to_subagent", ToolDisposition::BrokeredDelegation),
            ("stop_subagent", ToolDisposition::BrokeredDelegation),
            ("list_providers", ToolDisposition::ToolCall),
            ("read_subagent", ToolDisposition::ToolCall),
            ("wait_subagents", ToolDisposition::ToolCall),
        ] {
            assert_eq!(
                ToolDisposition::of(&hosted(BROKER_SERVER_NAME, tool, json!({}))),
                disposition,
                "{tool}"
            );
        }
        assert_eq!(
            ToolDisposition::of(&hosted("elsewhere", "spawn_subagent", json!({}))),
            ToolDisposition::ToolCall,
            "a Tool of the same name on another server is that server's own"
        );
    }
}
