//! Claude's Errands, run through the Claude Code CLI's own print mode.
//!
//! Print mode is the CLI's native one-shot, and so the path ADR 0011 prefers: one Prompt on stdin,
//! one result object on stdout, and a process that ends. It takes the schema natively — the CLI
//! enforces it and hands back the answer already parsed — so Claude never falls back to starting a
//! Provider-side session and discarding it, and never leaves a Suru Session behind either way.
//! Nothing here goes near the stream-json transport every Session is driven over; the run is the
//! provider-neutral one-shot the harness machinery offers, so abandoning the wait — at the Errand's
//! deadline or on shutdown — takes the process tree down with it.
//!
//! An Errand is not a Turn, and the launch says so in every flag it carries. It disables the
//! built-in Tools outright and starts no MCP servers, so a call made to name a Session cannot read
//! a file, run a command, or change the Workspace; and it persists no conversation, so nothing it
//! touches can be resumed — by Suru, by the user's own CLI, or by anything else. It also leaves the
//! user's personal and project settings unloaded: an Errand is Suru's own work, not an ordinary
//! user Session (ADR 0013). What it deliberately does *not* carry is permission bypass: with no
//! Tools there is nothing to permit, and an Errand that skips permissions would be a call the user
//! never asked for running with more authority than the work they did ask for.
//!
//! What the launch does take from the Session is its Workspace, as the working directory, so a
//! Workspace still scopes whatever repository content the toolless Prompt itself carries, without
//! giving Suru's own call the hooks, MCP servers, plugins, and overrides configured for user work.

use std::ffi::OsString;

use serde_json::Value;

use super::{claude_error, claude_error_context, wire::ResultMessage};
use crate::provider::{
    ProviderErrand, ProviderError,
    harness::{HarnessSpec, run_harness_to_completion},
};

/// What every failure of a Claude Errand is called, so the Log names the same thing whether the CLI
/// would not start, would not finish, or would not answer in the shape it was given.
const ERRAND_FAILED: &str = "Claude Errand failed";

/// What puts the CLI in the print mode an Errand runs through, ahead of the schema and the Agent
/// Selection the Errand itself names.
///
/// Every flag here is a way of carrying less than a Turn does: no built-in Tools, no MCP servers to
/// serve more, and no conversation written to disk for anything to resume. `--print` with
/// `--output-format json` is what makes the whole exchange one object on stdout rather than a
/// stream to project.
///
const CLAUDE_PRINT_MODE_ARGS: [&str; 9] = [
    "--print",
    "--output-format",
    "json",
    "--tools",
    "",
    "--strict-mcp-config",
    "--no-session-persistence",
    "--setting-sources",
    "",
];

/// Runs one Errand as a single print-mode invocation and answers with the JSON the CLI shaped to
/// the Errand's schema. The reply is handed back unvalidated: the schema constrains the CLI rather
/// than binding it, so whoever asked for the Errand still checks the shape it came back in.
pub(super) async fn run_claude_errand(
    executable: OsString,
    errand: ProviderErrand,
) -> Result<Value, ProviderError> {
    let spec = HarnessSpec {
        executable,
        args: arguments(&errand)?,
        name: super::CLAUDE_ONE_SHOT_NAME.to_owned(),
        cwd: Some(errand.execution_directory),
    };
    let finished = run_harness_to_completion(&spec, errand.prompt)
        .await
        .map_err(|error| claude_error_context(ERRAND_FAILED, error))?;
    // What print mode printed is read first whatever its exit status, because it reports a refusal
    // the same way it reports an answer — one result object on stdout — and exits non-zero
    // alongside it. The object carries the CLI's own account of what went wrong; the status carries
    // a number nobody can act on.
    if !finished.stdout.trim().is_empty() {
        return errand_answer(&finished.stdout);
    }
    // Nothing printed at all, so the run ended before the CLI could say anything in the one place
    // it says things. Its stderr is then the only account there is, and the Log is the only place
    // an Errand failure is ever reported — so it is carried through rather than dropped.
    let said = finished.stderr.trim();
    Err(claude_error(if said.is_empty() {
        format!(
            "{ERRAND_FAILED}: the Claude Code CLI printed nothing and exited with {}",
            finished.status
        )
    } else {
        format!("{ERRAND_FAILED}: {said}")
    }))
}

/// What the CLI is invoked with to run `errand`: the flags that put it in print mode and keep the
/// run toolless and traceless, the schema it is to shape its answer to, and the Errand Selection.
/// The Prompt is not among them — it arrives on stdin, where no length or shape of it can be
/// mistaken for an argument.
fn arguments(errand: &ProviderErrand) -> Result<Vec<OsString>, ProviderError> {
    let mut arguments = CLAUDE_PRINT_MODE_ARGS
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    arguments.push("--json-schema".into());
    arguments.push(errand.schema.to_string().into());
    arguments.extend(super::selection_args(&errand.selection)?);
    Ok(arguments)
}

/// The answer inside what print mode printed, or the failure the CLI printed in its place.
///
/// A result the CLI could not shape to the schema is a failure rather than a partial answer: the
/// caller asked for one shape, and the prose the CLI would otherwise fall back to is not it.
fn errand_answer(printed: &str) -> Result<Value, ProviderError> {
    let printed = printed.trim();
    if printed.is_empty() {
        return Err(claude_error(
            "the Claude Code CLI printed no answer to an Errand",
        ));
    }
    let result: ResultMessage = serde_json::from_str(printed).map_err(|error| {
        claude_error(format!(
            "the Claude Code CLI printed an invalid Errand result: {error}"
        ))
    })?;
    if result.is_error || result.subtype != "success" {
        // Already bounded and collapsed by the result itself, so it is not passed through
        // `claude_error` a second time.
        return Err(ProviderError::new(super::result_failure_message(
            "Errand", &result,
        )));
    }
    result.structured_output.ok_or_else(|| {
        claude_error("the Claude Code CLI answered an Errand outside the schema it was given")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// What the launch must never carry: an Errand carries no Tools, so there is nothing for it to
    /// be permitted to do, and a call the user never asked for is the last one that should run with
    /// more authority than the work they did.
    #[test]
    fn print_mode_neither_carries_tools_nor_skips_permissions() {
        assert!(
            !CLAUDE_PRINT_MODE_ARGS.contains(&"--dangerously-skip-permissions"),
            "an Errand asks for no permission bypass: {CLAUDE_PRINT_MODE_ARGS:?}"
        );
        let tools = CLAUDE_PRINT_MODE_ARGS
            .iter()
            .position(|argument| *argument == "--tools")
            .expect("print mode says what Tools it carries");
        assert_eq!(
            CLAUDE_PRINT_MODE_ARGS.get(tools + 1),
            Some(&""),
            "the empty Tool list is what disables the built-in set"
        );
        assert!(
            CLAUDE_PRINT_MODE_ARGS.contains(&"--no-session-persistence"),
            "an Errand leaves nothing resumable behind: {CLAUDE_PRINT_MODE_ARGS:?}"
        );
    }

    /// Errands are Suru-owned, toolless work and therefore load no personal or project settings.
    #[test]
    fn print_mode_isolated_from_personal_and_project_settings() {
        let setting_sources = CLAUDE_PRINT_MODE_ARGS
            .iter()
            .position(|argument| *argument == "--setting-sources")
            .expect("print mode says which setting sources it loads");
        assert!(
            CLAUDE_PRINT_MODE_ARGS.get(setting_sources + 1) == Some(&""),
            "an Errand loads no personal or project settings: {CLAUDE_PRINT_MODE_ARGS:?}"
        );
    }

    #[test]
    fn a_schema_shaped_answer_is_the_errands_answer() {
        let printed = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "{\"title\":\"Fix the flicker\"}",
            "structured_output": { "title": "Fix the flicker" },
        })
        .to_string();
        assert_eq!(
            errand_answer(&printed).expect("a schema-shaped answer is an answer"),
            json!({ "title": "Fix the flicker" })
        );
    }

    #[test]
    fn an_answer_the_cli_could_not_shape_is_no_answer() {
        let printed = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "Here is a title for you!",
        })
        .to_string();
        let error = errand_answer(&printed).expect_err("prose is not the shape Suru asked for");
        assert!(error.to_string().contains("outside the schema"), "{error}");
    }

    /// The CLI reports a Model it cannot serve as an errored result rather than as a failed exit,
    /// so what it says about the failure is inside the object it printed.
    #[test]
    fn an_errored_result_reports_what_the_cli_said_about_it() {
        let printed = json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "result": "There's an issue with the selected model (haiku).",
        })
        .to_string();
        let error = errand_answer(&printed).expect_err("an errored result is a failed Errand");
        assert!(
            error.to_string().contains("Claude Errand failed"),
            "{error}"
        );
        assert!(error.to_string().contains("haiku"), "{error}");
    }

    #[test]
    fn a_cli_that_printed_nothing_at_all_says_so() {
        let error = errand_answer("  \n ").expect_err("silence is not an answer");
        assert!(error.to_string().contains("printed no answer"), "{error}");
    }
}
