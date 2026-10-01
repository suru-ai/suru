//! The note every harness handed the Broker appends to its Agent's instructions, saying the
//! Broker is there and when to prefer it: Claude's `--append-system-prompt`, Codex's developer
//! instructions on `thread/start`, and Copilot's system message in append mode. It is written once
//! here and lowered by each harness onto its own seam, beside the Broker's endpoint and only where
//! that endpoint is handed, so an Errand — which carries no Tools — is told of nothing.
//!
//! The note names the Broker's Tools by the names the harness gives an MCP server's Tools, since
//! that is what the Agent calls — and, for a harness that defers MCP Tools behind a search of its
//! own, what it selects them by (docs/validation/0408-claude-http-mcp-long-calls.md). It offers
//! the Broker beside the Provider's own way of spawning Subagents, never in its place: Suru
//! re-routes no native spawn. It tells the Agent a Subagent Report wakes it, so an Agent with
//! nothing left to do ends its Turn rather than holding it open on `wait_subagents`, which would
//! show its Session as working on its own Turn rather than waiting for Subagents.
//!
//! The note is written for what the Agent is to the Broker (ADR 0042). Every Agent's names the
//! Tools every Agent is offered. A Sidekick's goes on to say what a Sidekick is, to name the Tools
//! that are its alone — read off the same registry the Broker lists and dispatches by, so a Tool
//! added there is named here without more — and to say what it may not do, so it does not attempt
//! what will be refused (ADR 0043).

use super::{BrokerRole, tools::BrokerTool};

/// The note for an Agent that is `role` to the Broker, naming each of the Broker's Tools as
/// `tool_name` spells the Tool the Broker serves under the given name.
pub(crate) fn instruction_note(role: BrokerRole, tool_name: impl Fn(&str) -> String) -> String {
    let note = agent_note(&tool_name);
    match role {
        BrokerRole::Agent => note,
        BrokerRole::Sidekick => format!("{note} {}", sidekick_note(&tool_name)),
    }
}

/// What every Agent handed the Broker is told: the Tools through which it reaches any Provider.
fn agent_note(tool_name: &impl Fn(&str) -> String) -> String {
    let tools = BrokerTool::offered_to(BrokerRole::Agent)
        .map(|tool| tool_name(tool.name()))
        .collect::<Vec<_>>()
        .join(", ");
    let list_providers = tool_name(BrokerTool::ListProviders.name());
    let wait_subagents = tool_name(BrokerTool::WaitSubagents.name());
    format!(
        "Suru, the app hosting this session, offers you Tools of its own through its Broker: \
         {tools}. With them you can spawn Subagents on any Provider Suru hosts, on the Model you \
         choose, then check on them, send them more work and stop them. Prefer them when the user \
         names another Provider or Model, or a task suits another one better; call \
         {list_providers} first to learn what may be chosen. A Subagent's settling reaches you as \
         a new message that wakes you if your turn has ended, so once you are only waiting on \
         Subagents, end your turn rather than calling {wait_subagents}. Your own tools for \
         spawning Subagents remain available, and Suru never re-routes them."
    )
}

/// What a Sidekick is told besides: what it is, the Tools that are its alone, and the acts no
/// Tool offers it.
fn sidekick_note(tool_name: &impl Fn(&str) -> String) -> String {
    let tools = BrokerTool::offered_to(BrokerRole::Sidekick)
        .filter(|tool| tool.is_sidekicks())
        .map(|tool| tool_name(tool.name()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are a Sidekick: an Agent that works across Suru itself rather than within one body \
         of work, so Suru offers you Tools of its own for that as well: {tools}. Their \
         descriptions say what each does. You cannot delete a Session, decide an Approval, change \
         an Approval Posture, or read or change the Settings that govern Serving and Pairing, and \
         you may not act on any Session of the Sidekick Workspace, your own included, though you \
         may read them. The Subagents you spawn are offered none of these Tools, so do such work \
         yourself rather than delegating it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_named(tool: &str) -> String {
        format!("mcp__suru__{tool}")
    }

    #[test]
    fn the_note_names_every_tool_the_broker_serves_as_the_harness_names_it() {
        let note = instruction_note(BrokerRole::Agent, claude_named);
        for tool in BrokerTool::offered_to(BrokerRole::Agent) {
            assert!(
                note.contains(&claude_named(tool.name())),
                "the note names {}: {note}",
                tool.name()
            );
        }
        assert!(
            note.contains("call mcp__suru__list_providers first"),
            "the note sends the Agent to the Tool that says what may be chosen: {note}"
        );
        assert!(
            note.contains("send them more work and stop them"),
            "and says a Subagent may be sent more work and stopped, as well as spawned: {note}"
        );
        assert!(
            note.contains("end your turn rather than calling mcp__suru__wait_subagents"),
            "and says a settling Subagent wakes the Agent, so it need not wait on one: {note}"
        );
    }

    /// The note is appended to instructions the Agent reads on every request, so it stays short,
    /// and it is one line, as a launch argument or a system message carries it.
    #[test]
    fn the_note_is_one_short_paragraph() {
        let note = instruction_note(BrokerRole::Agent, claude_named);
        let words = note.split_whitespace().count();
        assert!(words <= 120, "the note runs to {words} words: {note}");
        assert!(!note.contains('\n'), "the note is one line: {note:?}");
    }

    #[test]
    fn an_agent_is_told_nothing_of_the_sidekicks_tools() {
        let note = instruction_note(BrokerRole::Agent, claude_named);
        for tool in BrokerTool::ALL
            .into_iter()
            .filter(|tool| tool.is_sidekicks())
        {
            assert!(
                !note.contains(tool.name()),
                "an Agent other than a Sidekick is not told of {}: {note}",
                tool.name()
            );
        }
        assert!(!note.contains("Sidekick"), "{note}");
    }

    #[test]
    fn a_sidekick_is_told_its_own_tools_and_what_it_may_not_do() {
        let note = instruction_note(BrokerRole::Sidekick, claude_named);
        assert!(
            note.starts_with(&instruction_note(BrokerRole::Agent, claude_named)),
            "a Sidekick is told of every Tool any Agent is: {note}"
        );
        for tool in BrokerTool::offered_to(BrokerRole::Sidekick) {
            assert!(
                note.contains(&claude_named(tool.name())),
                "the note names {}: {note}",
                tool.name()
            );
        }
        for exclusion in [
            "cannot delete a Session",
            "decide an Approval",
            "change an Approval Posture",
            "the Settings that govern Serving and Pairing",
            "may not act on any Session of the Sidekick Workspace, your own included",
            "The Subagents you spawn are offered none of these Tools",
        ] {
            assert!(
                note.contains(exclusion),
                "the note says what a Sidekick may not do — {exclusion:?}: {note}"
            );
        }
        assert!(!note.contains('\n'), "the note is one line: {note:?}");
    }
}
