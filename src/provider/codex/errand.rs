//! Codex's Errands, run through `codex exec` — the harness's own one-shot mode.
//!
//! Codex offers, as flags on one non-interactive run, everything an Errand asks
//! for: a Prompt on stdin, a JSON Schema it constrains its final message to, and
//! `--ephemeral`, which leaves no rollout file behind for anything to resume.
//! That is why ADR 0011 prefers a native one-shot mode over the session-shaped
//! fallback — the guarantee is the harness's rather than a rule Suru has to keep
//! — and why nothing here goes near the app-server every Session is driven over.
//!
//! The run is given the least the harness can be left with: a read-only sandbox,
//! no approvals to escalate through, neither the user's Codex configuration nor
//! anyone's execpolicy rules loaded — which is where MCP servers, hooks, and
//! command permissions come from — and web search turned off, which is the one
//! Tool a read-only sandbox would not otherwise stop reaching the network. It
//! starts in the Session's own Workspace, so that Workspace's agent instructions
//! inform the answer and no repository check refuses the run.

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use serde::Deserialize;
use serde_json::Value;

use super::{
    REASONING_EFFORT_OPTION_ID, codex_error, codex_error_context,
    projection::has_billable_usage,
    wire::{ExecTokenBreakdown, NativeServiceTierOverride, lower_turn_options},
};
use crate::{
    pricing::{ModelsDevModel, PricingSource},
    protocol::{
        AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, ProviderId, ProviderUnavailability, Usage,
    },
    provider::{
        ErrandAnswer, ErrandConsumption, ErrandCost, ErrandFailed, ProviderErrand, ProviderError,
        harness::{HarnessSpec, run_harness_to_completion},
    },
};

/// The Codex subcommand that answers once and exits.
const EXEC_SUBCOMMAND: &str = "exec";

/// What every failure of a Codex Errand is called, so the Log names the same
/// thing whether Codex would not start, would not finish, or would not answer.
const ERRAND_FAILED: &str = "Codex Errand failed";

/// The Model named in Codex's Errand Selection: the small, fast, cheap one in
/// its catalog, next to anything a user converses with.
const ERRAND_MODEL: &str = "gpt-5.6-luna";

/// The reasoning effort named alongside it: the least that Model publishes,
/// because an Errand is not worth a thinking budget.
const ERRAND_REASONING_EFFORT: &str = "low";

/// The Codex config keys an Errand sets for the length of its own run. They are
/// passed on the command line rather than written anywhere, so nothing an Errand
/// decides for itself outlives it.
const REASONING_EFFORT_CONFIG_KEY: &str = "model_reasoning_effort";
const SERVICE_TIER_CONFIG_KEY: &str = "service_tier";
const WEB_SEARCH_CONFIG_KEY: &str = "web_search";

/// What `web_search` is set to, which is the whole point of setting it.
const WEB_SEARCH_DISABLED: &str = "disabled";

/// The Errand Selection Codex declares: the Model above, at the effort above.
///
/// Both are named outright rather than derived. Codex relays its efforts in its
/// own publication order under its own wire identifiers, so nothing about them
/// says which is the least, and the Model a user should converse with is a
/// different question from the one that should write six words. The declaration
/// is resolved against the live catalog every time an Errand runs, so a Model
/// Codex has withdrawn gives way to the one it defaults to: this going stale
/// costs a cheaper Errand rather than the Errand itself.
pub(super) fn declared_errand_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new(ERRAND_MODEL),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(ERRAND_REASONING_EFFORT),
            },
        }],
    }
}

/// Runs one Errand as a single `codex exec` invocation and answers with the JSON
/// Codex wrote. The reply is handed back unvalidated: the schema constrains
/// Codex rather than binding it, so whoever asked for the Errand still checks
/// the shape it came back in.
///
/// Beside it is what the run consumed, read from the events Codex printed, and
/// its Cost estimated from `pricing` as a Codex Turn's is. A run that failed
/// after its Turn completed keeps both: the Model was paid for either way.
pub(super) async fn run(
    executable: OsString,
    pricing: Option<Arc<PricingSource>>,
    errand: ProviderErrand,
) -> Result<ErrandAnswer, ErrandFailed> {
    let model = errand.selection.model.clone();
    let files = ErrandFiles::new(&errand.schema)?;
    let spec = HarnessSpec {
        executable,
        args: arguments(&errand, &files)?,
        name: super::CODEX_ONE_SHOT_NAME.to_owned(),
        cwd: Some(errand.execution_directory),
        env: Vec::new(),
        install_fix: super::codex_fix(ProviderUnavailability::NotInstalled),
    };
    let finished = run_harness_to_completion(&spec, errand.prompt)
        .await
        .map_err(|error| codex_error_context(ERRAND_FAILED, error))?;
    let events = ExecEvents::read(&finished.stdout);
    let consumption = consumption(events.usage, pricing.as_deref(), model).await;
    if !finished.status.success() {
        // Codex says why in its event stream, or on its stderr where it failed
        // before it had one, and the Log is the only place an Errand failure
        // is ever reported — so what it said is carried through rather than
        // reduced to an exit status nobody can act on.
        let said = events
            .failure
            .or_else(|| Some(finished.stderr.trim().to_owned()).filter(|said| !said.is_empty()))
            .unwrap_or_else(|| format!("Codex exited with {}", finished.status));
        return Err(ErrandFailed::measured(
            codex_error(format!("{ERRAND_FAILED}: {said}")),
            consumption,
        ));
    }
    match files.answer() {
        Ok(reply) => Ok(ErrandAnswer { reply, consumption }),
        Err(error) => Err(ErrandFailed::measured(error, consumption)),
    }
}

/// What the run consumed, and its Cost where the rate table prices `model`.
/// Codex states tokens and never dollars, so the Cost is always Estimated.
async fn consumption(
    usage: Option<Usage>,
    pricing: Option<&PricingSource>,
    model: ModelId,
) -> ErrandConsumption {
    let cost = match (&usage, pricing) {
        (Some(usage), Some(pricing)) => pricing
            .estimate(
                &ModelsDevModel::new(super::MODELS_DEV_PROVIDER, model),
                usage,
            )
            .await
            .map(ErrandCost::from),
        _ => None,
    };
    ErrandConsumption { usage, cost }
}

/// One line of what `codex exec --json` prints, as far as an Errand cares:
/// the Turn's completion with its usage, and the reasons a run failed. Every
/// other event is passed over.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ExecEvent {
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: ExecTokenBreakdown },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: ExecError },
    #[serde(rename = "error")]
    Error(ExecError),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ExecError {
    message: String,
}

/// What a run's event stream said about it.
#[derive(Default)]
struct ExecEvents {
    /// The last completed Turn's usage. Absent where Codex printed none that
    /// could be read, or one that measured nothing: Codex states a zeroed
    /// usage when it never received a reading, and that is a figure nobody
    /// measured rather than an Errand that consumed nothing.
    usage: Option<Usage>,
    /// Why the run failed: the Turn's own failure, else the last error the
    /// stream reported.
    failure: Option<String>,
}

impl ExecEvents {
    /// Reads `stdout` a line at a time. A line that is not an event Codex
    /// documents — not JSON, or an event not shaped as its kind says — is a
    /// measurement lost rather than a failed Errand, so it is passed over.
    fn read(stdout: &str) -> Self {
        let mut events = Self::default();
        let mut turn_failure = None;
        let mut stream_error = None;
        for event in stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<ExecEvent>(line).ok())
        {
            match event {
                ExecEvent::TurnCompleted { usage } => {
                    events.usage = Some(usage.into_usage()).filter(has_billable_usage);
                }
                ExecEvent::TurnFailed { error } => turn_failure = Some(error.message),
                ExecEvent::Error(error) => stream_error = Some(error.message),
                ExecEvent::Other => {}
            }
        }
        events.failure = turn_failure.or(stream_error);
        events
    }
}

/// The two files one `codex exec` run is pointed at: the schema going in, and
/// the final message coming back. They live in a directory of their own that is
/// removed with this value, so an Errand leaves nothing behind on any path out —
/// including the deadline that drops the run mid-flight.
struct ErrandFiles {
    directory: tempfile::TempDir,
}

impl ErrandFiles {
    fn new(schema: &Value) -> Result<Self, ProviderError> {
        let directory = tempfile::tempdir()
            .map_err(|error| codex_error(format!("Codex Errand needs a directory: {error}")))?;
        let files = Self { directory };
        std::fs::write(files.schema_path(), schema.to_string()).map_err(|error| {
            codex_error(format!(
                "Codex Errand could not be given its schema: {error}"
            ))
        })?;
        Ok(files)
    }

    fn schema_path(&self) -> PathBuf {
        self.directory.path().join("schema.json")
    }

    fn answer_path(&self) -> PathBuf {
        self.directory.path().join("answer.json")
    }

    /// What Codex wrote as its final message, decoded. A run that ended well
    /// but wrote nothing is as much a failure as one that ended badly: there is
    /// no answer either way.
    fn answer(&self) -> Result<Value, ProviderError> {
        let answer = std::fs::read_to_string(self.answer_path()).unwrap_or_default();
        if answer.trim().is_empty() {
            return Err(codex_error("Codex ran the Errand but answered nothing"));
        }
        serde_json::from_str(&answer).map_err(|error| {
            codex_error(format!(
                "Codex answered an Errand with invalid JSON: {error}"
            ))
        })
    }
}

/// What Codex is invoked with to run `errand`: its one-shot subcommand, the
/// flags that keep the run traceless and unprivileged, the Errand Selection
/// lowered onto Codex's own config keys, and the two files it is pointed at.
/// The Prompt is not among them — it arrives on stdin, where no length or shape
/// of it can be mistaken for an argument.
fn arguments(errand: &ProviderErrand, files: &ErrandFiles) -> Result<Vec<OsString>, ProviderError> {
    let options = lower_turn_options(&errand.selection)?;
    let mut arguments: Vec<OsString> = vec![
        EXEC_SUBCOMMAND.into(),
        // Nothing resumable is left behind: no rollout file, and no session
        // file in Codex's own state directory.
        "--ephemeral".into(),
        // A Workspace is not always a repository, and a Codex that refused to
        // run outside one would decide which Sessions can be titled.
        "--skip-git-repo-check".into(),
        // An Errand carries no Tools, and the user's own Codex configuration is
        // where its MCP servers and hooks are declared — so none of it is
        // loaded. Authentication is not configuration and is unaffected. This
        // is what Claude's availability probe does for the same reason, and its
        // cost is the same: a user whose Codex is configured to reach a Model
        // some other way runs no Errand, and keeps the Title their Prompt gave.
        "--ignore-user-config".into(),
        // The other half of that: execpolicy rules, which a Workspace may carry
        // as readily as the user can, decide what a command may do without
        // asking. An Errand runs no commands and so needs none of them.
        "--ignore-rules".into(),
        // The least permission the harness offers.
        "--sandbox".into(),
        "read-only".into(),
        // A read-only sandbox stops a Tool changing the Workspace but not one
        // reaching the network, and an Errand needs neither.
        "--config".into(),
        format!("{WEB_SEARCH_CONFIG_KEY}=\"{WEB_SEARCH_DISABLED}\"").into(),
        "--model".into(),
        errand.selection.model.as_str().into(),
    ];
    if let Some(effort) = options.effort {
        arguments.push("--config".into());
        arguments.push(format!("{REASONING_EFFORT_CONFIG_KEY}=\"{effort}\"").into());
    }
    if let NativeServiceTierOverride::Value(service_tier) = options.service_tier {
        arguments.push("--config".into());
        arguments.push(format!("{SERVICE_TIER_CONFIG_KEY}=\"{service_tier}\"").into());
    }
    arguments.extend([
        // Events on stdout as JSONL, so the run says what it consumed. The
        // answer is still the file below, which Codex writes only once its
        // final message is whole.
        "--json".into(),
        "--output-schema".into(),
        files.schema_path().into_os_string(),
        "--output-last-message".into(),
        files.answer_path().into_os_string(),
        // The Prompt follows on stdin.
        "-".into(),
    ]);
    Ok(arguments)
}
