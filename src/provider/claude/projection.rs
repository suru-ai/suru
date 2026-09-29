//! Projection of a Claude Session's conversation messages into attributed Provider events.
//!
//! The CLI streams a Turn as partial-message chunks — Anthropic streaming events riding in
//! `stream_event` envelopes — and ends each stretch of its loop with one `result` message. The
//! wire carries several conversations at once: the loop's own, and one for every subagent the
//! agent spawns through the Task tool, attributed to the spawning tool use's id as
//! `parent_tool_use_id`. Only the loop's own conversation streams; a subagent's arrives solely as
//! the full `assistant` snapshots that restate the loop's chunks after the fact, so each
//! conversation is presented from whichever account is all it gets. Both project the same way —
//! text blocks become the agent Message, thinking blocks become Reasoning Activity split at their
//! headings, the Bash tool's executions become Command Activity, its edit tools' uses File
//! Changes, and every other tool use no more specific Activity records a Tool Call, each settled
//! by the tool result the loop echoes back or, as failed, by a Decision declining it — and each
//! event leaves here attributed to the conversation that produced it, so orchestration lands a
//! subagent's work in the Subagent's own child Session rather than the parent's Transcript. A File
//! Change or Tool Call opens as its block opens, so an Approval gating the use links to its row
//! even when it arrives before the block closes.
//!
//! The task lifecycle the CLI reports beside the conversations is where Subagents begin and end:
//! `task_started` for an agent task opens the Subagent — known by its task id — in the
//! conversation whose tool use spawned it, `task_updated` revises what it is doing, and
//! `task_notification` settles its stretch of work. A background shell or monitor is instead a
//! Watch (ADR 0030): its start and its notification bracket the time its Session may read
//! Monitoring, and every Watch still live when the CLI process ends settles as lost. A shell the
//! agent waits on in the foreground of its Turn is never a Watch, though a long one reports a task
//! lifecycle too, unless the CLI later moves it to the background. A Watch belongs to the
//! conversation whose tool use launched it — a subagent's own, even after that subagent settles,
//! since its Watches outlive it and wake it. A settled agent the loop resumes through
//! SendMessage starts the same task again, naming the SendMessage tool use: that is a resume of the
//! Subagent rather than a new one, and since the resumed conversation still rides under the
//! original spawn's id, the resumed work lands in the Subagent's own Session, in the Turn the
//! resume begins there (ADR 0031). What the delegating tool handed the agent — the Agent tool's
//! `prompt`, SendMessage's `message` — is the Delegation that opens that Turn. The Resume State
//! records the spawn each agent's conversation rides under, so an agent spawned before a restart
//! resumes the same way once the conversation carries on; one resumed with no record claims the
//! first conversation nothing else has, provided it is the only such agent working.
//! A SendMessage to an agent still working **steers** it instead (ADR 0032): the CLI queues the
//! message for the agent's next tool round and says so only in the tool's result, and nothing on
//! the wire marks when the agent reads it. The steer is held pending against the agent, and
//! placed — after any sent before it — just before the first assistant message the agent begins
//! once its tool results next go back to it, as a Delegation from whichever conversation ran the SendMessage: the loop's own, or a sibling
//! subagent's. It adds nothing to the delegating Transcript. An agent that finishes before reading
//! it is restarted by the CLI with the steer as a fresh prompt — its task started again naming its
//! own spawn rather than the SendMessage — and that restart is a resume the steer opens, its row
//! described by the SendMessage as any resume's is. A steer the CLI refuses stands nowhere, nor
//! does one still pending when its agent is stopped, or is started again by anything but that
//! restart.
//! A `result` Settles the Turn as completed, interrupted, or failed — except where a steer's own
//! result is still to come. The CLI folds a message queued into a running loop into that loop at
//! its next tool round, and answers it with the loop's one result; a message still queued when
//! the loop ends begins a loop of its own, with a result of its own, and Suru keeps them all
//! inside the Turn the steer joined. Which of the two befell a steer, the CLI reports in the
//! lifecycle it gives the message under the uuid it was written with ([`super::turn_in_flight`]).
//! The result speaks only for the loop's own conversation: a subagent's streams live past it,
//! which is what lets a Subagent outlive the Turn (ADR 0015), and the stretch the loop later runs
//! to deliver its outcome ends with a result of its own. A fresh owning message after that
//! boundary explicitly begins a native Continuation, including when a background Bash command
//! woke the loop with no Subagent involved; its result and interrupt belong to that Continuation.
//! Every block kind this slice does not present is passed over rather than failed, because the
//! wire grows freely (ADR 0010).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::command_presentation::{PresentedCommand, present_command};
use super::super::tool_call_presentation::present_tool_input;
use super::{
    claude_error,
    session::ClaudeResumeState,
    thinking::{ThinkingEvent, ThinkingSplitter},
    transport::ConversationItem,
    turn_in_flight::TurnInFlight,
    wire::{
        AssistantMessageSnapshot, CommandLifecycle, CommandLifecycleState, ContentBlock,
        EchoedUserContent, EchoedUserMessage, ResultMessage, ResultUsage, StreamEventMessage,
        SystemMessage,
    },
};
use crate::protocol::{Cost, FileChange, Usage};
use crate::provider::{
    AttributedProviderEvent, ProviderActivityId, ProviderCommandStatus, ProviderError,
    ProviderEvent, ProviderEventAttribution, ProviderEventStream, ProviderFileChangeStatus,
    ProviderSubagentId, ProviderSubagentStatus, ProviderToolCallStatus, ProviderWatchId,
    ProviderWatchOutcome, ReportedTurnMetering,
};

/// The tool whose executions are Command Activity. Claude sends the command itself as the tool's
/// `command` input, so stripping applies only if recognizable launcher plumbing ever appears.
const COMMAND_TOOL: &str = "Bash";

/// The tools that change a file in place, whose uses are File Changes updating the file they name.
const EDIT_TOOL: &str = "Edit";
const MULTI_EDIT_TOOL: &str = "MultiEdit";
const NOTEBOOK_EDIT_TOOL: &str = "NotebookEdit";

/// The tool that writes a whole file, whose uses are File Changes adding the file they name or
/// updating it, as the file was absent or there when the tool use opened.
const WRITE_TOOL: &str = "Write";

/// The tool that spawns a subagent. Its tool-use id is what the CLI names as a task's
/// `tool_use_id` and what the subagent's every chunk rides under as `parent_tool_use_id`.
const TASK_TOOL: &str = "Task";
const AGENT_TOOL: &str = "Agent";

/// The tool that sends an agent more: a resume of a settled background agent, or a steer of one
/// still working. The CLI starts a resumed agent's task again naming this tool use as its
/// `tool_use_id`, so the resume opens in the conversation that ran it, like a spawn; a steer it
/// only queues, saying so in the tool's result. Either way the tool's input says what it asks.
const SEND_MESSAGE_TOOL: &str = "SendMessage";

/// The tool the agent asks the user through. Its `can_use_tool` request is the Questionnaire
/// (see [`super::questionnaire`]), which records the use.
const QUESTIONNAIRE_TOOL: &str = "AskUserQuestion";

/// Claude's plumbing: tools that only shape the CLI's own interface, whose uses are recorded as
/// nothing. `ToolSearch` loads a deferred tool's definition before the agent's first use of it
/// (docs/validation/0408-claude-http-mcp-long-calls.md). Like the launcher plumbing stripped from
/// a Bash tool's commands, it is Claude's own, so the list lives here beside [`COMMAND_TOOL`].
const PLUMBING_TOOLS: [&str; 1] = ["ToolSearch"];

/// How Claude names an MCP server's tools: `mcp__<server>__<tool>`.
const MCP_TOOL_PREFIX: &str = "mcp__";
const MCP_TOOL_SEPARATOR: &str = "__";

/// What one tool use is to a Transcript, decided by the tool's name alone: which Activity records
/// it, if any does. A Tool Call is the fallback for every use no more specific Activity records
/// and that is no plumbing, so a tool this build has never heard of is a Tool Call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolDisposition {
    /// The Bash tool, whose executions are Command Activity.
    Command,
    /// The Agent or Task tool, whose spawn is the Subagent row its task opens.
    Spawn,
    /// SendMessage, whose resume is the Subagent row its task opens, and whose steer is a
    /// Delegation in the Subagent's own Session.
    Resume,
    /// A Broker call whose effect is the Subagent row it spawns, sends to, or stops.
    BrokeredDelegation,
    /// AskUserQuestion, whose use is its Questionnaire.
    Questionnaire,
    /// A tool that edits files, whose use is a File Change of the kind the tool makes.
    FileChange(EditTool),
    /// Claude's own plumbing, recorded as nothing.
    Plumbing,
    /// Every other tool use: a Tool Call.
    ToolCall,
}

impl ToolDisposition {
    fn of(name: &str) -> Self {
        match name {
            COMMAND_TOOL => Self::Command,
            TASK_TOOL | AGENT_TOOL => Self::Spawn,
            SEND_MESSAGE_TOOL => Self::Resume,
            QUESTIONNAIRE_TOOL => Self::Questionnaire,
            EDIT_TOOL | MULTI_EDIT_TOOL => Self::FileChange(EditTool::Edit),
            NOTEBOOK_EDIT_TOOL => Self::FileChange(EditTool::NotebookEdit),
            WRITE_TOOL => Self::FileChange(EditTool::Write),
            name if PLUMBING_TOOLS.contains(&name) => Self::Plumbing,
            name => match mcp_tool(name) {
                Some((crate::broker::BROKER_SERVER_NAME, tool))
                    if crate::broker::tool_affects_a_subagent_row(tool) =>
                {
                    Self::BrokeredDelegation
                }
                _ => Self::ToolCall,
            },
        }
    }

    /// The native identity of the row recording a use, named by its tool-use id: the one scheme
    /// the projection opens rows under and an Approval gating the use links by. A use no row of
    /// its own records has none.
    fn row_activity_id(self, tool_use_id: &str) -> Option<ProviderActivityId> {
        let row = match self {
            Self::Command => "command",
            Self::FileChange(_) => "file_change",
            Self::ToolCall => "tool_call",
            Self::Spawn
            | Self::Resume
            | Self::BrokeredDelegation
            | Self::Questionnaire
            | Self::Plumbing => return None,
        };
        Some(ProviderActivityId::new(format!("{row}:{tool_use_id}")))
    }
}

/// The native identity of the row recording the use `tool_use_id` of the tool `tool_name`, which
/// an Approval gating that use links to — or `None` where no row records the use.
pub(super) fn gated_tool_activity_id(
    tool_name: &str,
    tool_use_id: &str,
) -> Option<ProviderActivityId> {
    ToolDisposition::of(tool_name).row_activity_id(tool_use_id)
}

/// Splits Claude's name for an MCP server's tool into the server and the tool's own name. A name
/// in any other shape is no MCP tool's.
fn mcp_tool(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(MCP_TOOL_PREFIX)?
        .split_once(MCP_TOOL_SEPARATOR)
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
}

/// What a SendMessage result says when the CLI queued the message for an agent still working
/// rather than resuming one — verified against 2.1.280: "Message queued for delivery to <task> at
/// its next tool round."
const QUEUED_FOR_DELIVERY: &str = "queued for delivery";

/// What one task the CLI reports is to Suru. Every task joins the roster an interrupt stops, but
/// only a Subagent has a conversation of its own, and only a Watch may wake the loop once it has
/// ended its Turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    /// A task running an agent, opened as a Subagent.
    Subagent,
    /// Background work whose settling the CLI delivers to the agent, waking its loop into a
    /// Continuation. Its Command Activity, where it has one, already stands in the Transcript.
    Watch,
    /// A shell the agent is waiting on inside its Turn. Its Command Activity is all it is, and
    /// its tool result, not its settling, is what the agent reads — unless the CLI moves it to the
    /// background, which makes it a Watch from then on.
    Foreground,
    /// Work that neither runs an agent Suru presents nor wakes this loop.
    Unwatched,
}

impl TaskKind {
    /// The one table classifying the CLI's tasks by `task_type` and `is_backgrounded` (ADR 0030).
    /// A background shell and a Monitor tool both run as `local_bash`, and a monitor over MCP or
    /// a WebSocket is a Watch too: each one's settling is delivered to the agent. A `local_bash`
    /// the CLI says is not backgrounded is a foreground Bash call, which is never a Watch —
    /// verified against Claude Code CLI 2.1.280: a short foreground Bash emits no task lifecycle
    /// at all; a long one emits `task_started` with `is_backgrounded: false` and its
    /// `task_notification` inside the Turn; a backgrounded one emits `is_backgrounded: true`. A
    /// start that does not say keeps reading as a Watch, as a monitor's start does.
    /// `local_workflow`, `remote_agent`, `in_process_teammate`, `dream`, plan-mode tasks, and any
    /// type this build has not heard of are none of Suru's to wait on, since the wire grows freely
    /// (ADR 0010) and a task wrongly read as a Watch would leave a Session Monitoring for nothing.
    fn of(task_type: Option<&str>, is_backgrounded: Option<bool>) -> Self {
        match (task_type, is_backgrounded) {
            (Some("local_agent"), _) => Self::Subagent,
            (Some("local_bash"), Some(false)) => Self::Foreground,
            (Some("local_bash" | "monitor_mcp" | "monitor_ws"), _) => Self::Watch,
            _ => Self::Unwatched,
        }
    }
}

/// A foreground task still running, kept with what its start said so that, moved to the
/// background, it becomes the Watch it would have been had it started there.
struct ForegroundTask {
    description: Option<String>,
    tool_use_id: Option<String>,
    owned_by_subagent: Option<bool>,
}

/// How a task's `task_notification` reports it finishing well; anything else — failed, stopped —
/// settles the Subagent as failed.
const TASK_COMPLETED_STATUS: &str = "completed";

/// How a task's `task_notification` reports it stopped rather than finishing or failing.
const TASK_STOPPED_STATUS: &str = "stopped";

pub(super) fn provider_events(
    messages: mpsc::UnboundedReceiver<Result<ConversationItem, ProviderError>>,
    projection: ClaudeProjection,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    approvals: Arc<super::approval::ClaudeApprovals>,
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
) -> ProviderEventStream {
    Box::pin(stream::unfold(
        EventReceiver {
            context,
            reports,
            questionnaires,
            approvals,
            messages,
            projection,
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
}

struct EventReceiver {
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    approvals: Arc<super::approval::ClaudeApprovals>,
    messages: mpsc::UnboundedReceiver<Result<ConversationItem, ProviderError>>,
    projection: ClaudeProjection,
    pending: VecDeque<Result<AttributedProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: EventReceiver,
) -> Option<(
    Result<AttributedProviderEvent, ProviderError>,
    EventReceiver,
)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let message = tokio::select! {
            message = events.messages.recv() => message?,
            Some(report) = events.reports.recv() => {
                if let Some(report) = events.context.route_report(report) {
                    return Some((Ok(report), events));
                }
                continue;
            },
        };
        match message {
            Err(error) => {
                events.questionnaires.clear();
                events.approvals.clear();
                return Some((Err(error), events));
            }
            Ok(ConversationItem::ProcessEnded) => {
                let settled = events.projection.project_process_ended();
                events.pending.extend(settled.into_iter().map(Ok));
            }
            Ok(ConversationItem::TasksStopped(tasks)) => {
                let settled = events.projection.project_watches_stopped(&tasks);
                events.pending.extend(settled.into_iter().map(Ok));
            }
            Ok(ConversationItem::ToolUseDeclined {
                tool_use_id,
                message,
            }) => {
                let settled = events
                    .projection
                    .project_declined_tool_use(&tool_use_id, &message);
                events.pending.extend(settled.into_iter().map(Ok));
            }
            Ok(ConversationItem::Message(message)) => {
                events.context.observe(&message);
                let attribution = events.projection.intervention_attribution(&message);
                match events
                    .questionnaires
                    .receive(&message, attribution.clone())
                    .await
                {
                    Ok(Some(projected)) => {
                        events
                            .context
                            .observe_output(&projected, events.projection.turn.is_running());
                        events.pending.extend(projected.into_iter().map(Ok));
                        continue;
                    }
                    Err(error) => {
                        events.pending.push_back(Err(error));
                        continue;
                    }
                    Ok(None) => {}
                }
                match events
                    .approvals
                    .receive(
                        &message,
                        attribution,
                        &events.projection.execution_directory,
                    )
                    .await
                {
                    Ok(Some(projected)) => {
                        events
                            .context
                            .observe_output(&projected, events.projection.turn.is_running());
                        events.pending.extend(projected.into_iter().map(Ok));
                        continue;
                    }
                    Err(error) => {
                        events.pending.push_back(Err(error));
                        continue;
                    }
                    Ok(None) => {}
                }
                let prompt_running = events.projection.turn.is_running();
                match events.projection.project(message) {
                    Ok(projected) => {
                        events.context.observe_output(&projected, prompt_running);
                        for event in &projected {
                            match &event.event {
                                ProviderEvent::TurnCompleted
                                | ProviderEvent::TurnInterrupted
                                | ProviderEvent::TurnFailed { .. } => {
                                    events.questionnaires.settle(&event.attribution);
                                    events.approvals.settle(&event.attribution);
                                    if event.attribution == ProviderEventAttribution::OwningSession
                                    {
                                        events.context.request();
                                        events
                                            .projection
                                            .intervention_tools
                                            .retain(|_, owner| owner.is_some());
                                    }
                                }
                                ProviderEvent::SubagentCompleted { subagent_id, .. } => {
                                    events.questionnaires.settle(
                                        &ProviderEventAttribution::Subagent(subagent_id.clone()),
                                    );
                                    events.approvals.settle(&ProviderEventAttribution::Subagent(
                                        subagent_id.clone(),
                                    ));
                                }
                                _ => {}
                            }
                        }
                        events.pending.extend(projected.into_iter().map(Ok));
                    }
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

/// Which conversation on the wire produced a message: the loop's own, or the subagent's whose
/// spawning tool use `parent_tool_use_id` names.
type ConversationKey = Option<String>;

/// The loop's own conversation, whose events land in the owning Session.
const OWNING_CONVERSATION: ConversationKey = None;

/// A `tool_use` block between its start and stop: the input streams in `input_json_delta`
/// increments beside whatever the start already carried.
struct OpenToolUse {
    id: String,
    name: String,
    streamed_input: String,
    opening_input: Option<Value>,
}

/// The thinking block a conversation has open, and the Reasoning block its split is currently
/// filling.
struct OpenThinking {
    index: u64,
    activity: ProviderActivityId,
    splitter: ThinkingSplitter,
}

/// What the projection remembers about one conversation between its chunks: the streaming text
/// block that is its agent Message currently open, the thinking block feeding its Reasoning
/// Activity, and the tool-use blocks still streaming their input, keyed by the block index each
/// numbers within its own conversation.
#[derive(Default)]
struct ConversationInFlight {
    open_text_block: Option<u64>,
    open_thinking: Option<OpenThinking>,
    open_tools: BTreeMap<u64, OpenToolUse>,
    /// Whether any chunk has streamed for this conversation. A conversation that streams is
    /// presented from its chunks, so its full-message snapshots restate what already projected
    /// and are passed over; one that never streams is presented from the snapshots alone.
    streamed: bool,
}

/// A command running until a tool result settles it, remembering the conversation that ran it —
/// which is where its output and settle land.
struct RunningCommand {
    owner: ConversationKey,
    activity: ProviderActivityId,
    /// The command as its Activity reads it, which describes a background task it starts that
    /// the CLI gives no description of its own.
    command: String,
}

/// A File Change running until a tool result settles it, remembering the conversation whose tool
/// use made it — which is where its settle lands.
struct RunningFileChange {
    owner: ConversationKey,
    activity: ProviderActivityId,
}

/// A Tool Call awaiting the tool result that settles it, remembering the conversation that ran it
/// — which is where its input, output and settle land.
struct RunningToolCall {
    owner: ConversationKey,
    activity: ProviderActivityId,
}

/// One of the tools whose uses are File Changes, by what it does to the file it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditTool {
    /// Edit or MultiEdit, changing the file named by `file_path` in place.
    Edit,
    /// NotebookEdit, changing the notebook named by `notebook_path` in place.
    NotebookEdit,
    /// Write, replacing or creating the whole file named by `file_path`.
    Write,
}

impl EditTool {
    /// The change a use makes, read from its completed input: an Update of the file it names, or
    /// for a Write naming a file absent right now — as its tool use closes, before it runs — an
    /// Add. The CLI shares the Server's filesystem, and a relative path names a file where it
    /// works. The path is recorded as Claude named it, and input naming none changes nothing.
    fn change(self, input: &Value, execution_directory: &Path) -> Option<FileChange> {
        let field = match self {
            Self::Edit | Self::Write => "file_path",
            Self::NotebookEdit => "notebook_path",
        };
        let path = PathBuf::from(input.get(field)?.as_str().filter(|path| !path.is_empty())?);
        Some(match self {
            Self::Write if !execution_directory.join(&path).exists() => FileChange::Add { path },
            Self::Edit | Self::NotebookEdit | Self::Write => FileChange::Update {
                path,
                moved_to: None,
            },
        })
    }
}

/// A tool use that delegates to an agent, remembered from its block until the task it delegates
/// starts: the conversation that ran it, whose Turn the Delegation's row stands in, and which kind
/// of Delegation it is. Its input is read once the block closes, since it streams.
struct DelegationTool {
    delegator: ConversationKey,
    kind: DelegationKind,
}

enum DelegationKind {
    /// The Agent or Task tool, spawning a new agent. The task's start describes it, and the tool's
    /// `prompt` is the Delegation's text.
    Spawn { prompt: Option<String> },
    /// SendMessage, resuming an agent that settled or steering one still working. Its input
    /// describes the resume — the loop's `summary` of the message, or else the message's own first
    /// line — its `message` is the Delegation's text, and `to` names the agent it is sent to.
    Resume {
        description: Option<String>,
        message: Option<String>,
        to: Option<String>,
    },
}

impl DelegationKind {
    /// Reads what the tool's completed input says of the Delegation. Input in no shape this reads
    /// leaves the Delegation to be described by the task's start instead.
    fn read_input(&mut self, input: &Value) {
        match self {
            Self::Spawn { prompt } => *prompt = input_text(input, "prompt"),
            Self::Resume {
                description,
                message,
                to,
            } => {
                *description = send_message_description(input);
                *message = input_text(input, "message");
                *to = input_text(input, "to");
            }
        }
    }

    /// The Delegation's text, as the tool's input gave it.
    fn text(&mut self) -> Option<String> {
        match self {
            Self::Spawn { prompt } => prompt.take(),
            Self::Resume { message, .. } => message.take(),
        }
    }
}

/// One agent task the CLI has run as a Subagent. Its task id is the Subagent's identity, and it
/// is remembered for as long as the wire lasts, because a settled agent may be resumed.
struct AgentTask {
    /// The `parent_tool_use_id` its conversation rides under: the spawning tool use's id, which a
    /// resume does not change. `None` where no chunk can be attributed to it.
    conversation: Option<String>,
    /// While the agent works, the description the row of its current stretch reads, kept so
    /// updates repeating it unchanged publish nothing; `None` once that stretch has settled.
    working: Option<String>,
}

/// A Delegation the CLI queued for an agent still working — a steer (ADR 0032) — held until the
/// agent reads it. The stream never says when that is: the CLI delivers queued messages at the
/// agent's next tool round, so the steer stands just before the first assistant message the agent
/// begins after that round. If the agent finishes first, the CLI restarts it with the message as
/// a fresh prompt, and the steer is instead the Delegation that opens the resume.
struct PendingSteer {
    /// The conversation that ran the SendMessage: the loop's own, or a sibling subagent's.
    delegator: ConversationKey,
    /// The Delegation's text: the SendMessage's `message`.
    text: String,
    /// What a resume row the steer turns into reads: the SendMessage's `summary`, or its message's
    /// first line.
    description: Option<String>,
    /// Whether the agent has had a tool round since the steer was queued — its tool results
    /// echoed back into its conversation — and so has read it. Until then the message it is
    /// writing is one it began without the steer, however late its snapshot arrives.
    read: bool,
}

/// What the projection remembers between conversation messages, across every conversation the
/// wire carries at once.
pub(super) struct ClaudeProjection {
    intervention_tools: BTreeMap<String, ConversationKey>,
    conversations: BTreeMap<ConversationKey, ConversationInFlight>,
    /// The commands whose tool results are still to be echoed back, by tool-use id — the CLI's
    /// ids are unique across conversations, so one table serves them all.
    running_commands: BTreeMap<String, RunningCommand>,
    /// The File Changes whose tool results are still to be echoed back, by tool-use id.
    running_file_changes: BTreeMap<String, RunningFileChange>,
    /// The Tool Calls whose tool results are still to be echoed back, by tool-use id, on the same
    /// terms as the commands.
    running_tool_calls: BTreeMap<String, RunningToolCall>,
    /// The delegating tool uses that have streamed, by tool-use id. A `task_started` naming one of
    /// them is a Delegation out of the conversation that ran it — which is how a subagent's own
    /// spawns recurse one level down, and how a sibling's resume lands in the sibling's Turn.
    delegation_tools: BTreeMap<String, DelegationTool>,
    /// Every agent task this wire has started, working or settled, by task id — the identity the
    /// rest of the lifecycle and a resume name it by.
    agent_tasks: BTreeMap<String, AgentTask>,
    /// The Watches running in the current CLI process, by task id, against the conversation
    /// whose agent a Watch's settling wakes. They settle on their own notification, or all at
    /// once as lost when the process ends.
    watches: BTreeMap<String, ConversationKey>,
    /// The foreground tasks running in the current CLI process, by task id. Their notifications
    /// settle nothing a Session shows, and a move to the background promotes one to a Watch.
    foreground_tasks: BTreeMap<String, ForegroundTask>,
    /// The subagent conversations, from the `parent_tool_use_id` each rides under to the task id
    /// of the agent it belongs to — whose Subagent its events are, in whichever stretch.
    conversation_agents: BTreeMap<String, String>,
    /// The agents working in a resume whose conversation this wire does not know, by task id —
    /// agents spawned before a restart the Resume State never recorded — until one claims the
    /// conversation its chunks turn out to ride under.
    unplaced_resumes: BTreeSet<String>,
    /// The steers queued for each agent and not yet read, by task id, in the order they were sent.
    /// A stopped agent never reads its steers, so they are discarded, as are those of an agent
    /// that settled and was then started again by anything but the CLI's restart for them.
    pending_steers: BTreeMap<String, VecDeque<PendingSteer>>,
    /// What the Session must remember to continue after a restart, kept current as agents spawn so
    /// orchestration can store each revision.
    resume: ClaudeResumeState,
    /// Latest assistant-snapshot Model evidence by conversation, since the stretch it began in.
    /// A snapshot can race the task lifecycle, so evidence waits here until the stretch's row
    /// exists, and a settle clears it so a resume reports only its own.
    subagent_models: BTreeMap<String, crate::protocol::ModelId>,
    /// The latest cumulative Cost results reported, by the reporting lifetime whose running total
    /// each is. A later report in the same lifetime is accepted only if it has not gone back.
    latest_reported_costs: HashMap<String, Cost>,
    seen_results: HashSet<String>,
    reasoning_blocks: u64,
    turn_metering: Option<ReportedTurnMetering>,
    /// What the Session reads back out of the conversation: whether the Turn it started is still
    /// running, and the background work it must stop before interrupting.
    turn: Arc<TurnInFlight>,
    /// The directory the CLI launched in, which its shell returns to after every command — so the
    /// directory every command runs in unless it changes directory itself.
    execution_directory: PathBuf,
}

impl ClaudeProjection {
    /// can_use_tool identifies the tool call, while its preceding native
    /// assistant block names the spawning tool use through parent_tool_use_id,
    /// which resolves to the Subagent the conversation belongs to. agent_id
    /// alone is not interchangeable with that spawning tool identity.
    fn intervention_attribution(&self, message: &Value) -> Option<ProviderEventAttribution> {
        let request = &message["request"];
        if let Some(tool) = request["tool_use_id"].as_str()
            && let Some(owner) = self.intervention_tools.get(tool)
        {
            return Some(self.attribution(owner));
        }
        request["agent_id"]
            .is_null()
            .then_some(ProviderEventAttribution::OwningSession)
    }

    /// A projection for a Session whose conversation `resume` restores: every agent it records
    /// spawning is a settled Subagent a resume may name, riding under the conversation recorded
    /// for it. Its CLI works in `execution_directory`.
    pub(super) fn new(
        turn: Arc<TurnInFlight>,
        resume: ClaudeResumeState,
        execution_directory: PathBuf,
    ) -> Self {
        let agent_tasks = resume
            .agents
            .iter()
            .map(|(task_id, conversation)| {
                (
                    task_id.clone(),
                    AgentTask {
                        conversation: Some(conversation.clone()),
                        working: None,
                    },
                )
            })
            .collect();
        let conversation_agents = resume
            .agents
            .iter()
            .map(|(task_id, conversation)| (conversation.clone(), task_id.clone()))
            .collect();
        Self {
            intervention_tools: BTreeMap::new(),
            conversations: BTreeMap::new(),
            running_commands: BTreeMap::new(),
            running_file_changes: BTreeMap::new(),
            running_tool_calls: BTreeMap::new(),
            delegation_tools: BTreeMap::new(),
            agent_tasks,
            watches: BTreeMap::new(),
            foreground_tasks: BTreeMap::new(),
            conversation_agents,
            unplaced_resumes: BTreeSet::new(),
            pending_steers: BTreeMap::new(),
            resume,
            subagent_models: BTreeMap::new(),
            latest_reported_costs: HashMap::new(),
            seen_results: HashSet::new(),
            reasoning_blocks: 0,
            turn_metering: None,
            turn,
            execution_directory,
        }
    }

    /// The attribution one conversation's events land under: the owning Session for the loop's
    /// own, and for a subagent's the Subagent its agent task is. A conversation whose task has not
    /// started yet names no Subagent orchestration holds, so what it carries lands nowhere.
    fn attribution(&self, owner: &ConversationKey) -> ProviderEventAttribution {
        match owner {
            None => ProviderEventAttribution::OwningSession,
            Some(conversation) => ProviderEventAttribution::Subagent(ProviderSubagentId::new(
                self.conversation_agents
                    .get(conversation)
                    .unwrap_or(conversation)
                    .clone(),
            )),
        }
    }

    fn attributed(&self, owner: &ConversationKey, event: ProviderEvent) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: self.attribution(owner),
            event,
        }
    }

    fn project(&mut self, message: Value) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.project_stream_event(message),
            Some("assistant") => Ok(self.project_assistant_snapshot(message)),
            Some("user") => Ok(self.project_tool_results(message)),
            Some("result") => self.project_result(message),
            Some("system") => Ok(self.project_task_lifecycle(message)),
            Some("command_lifecycle") => Ok(self.project_command_lifecycle(message)),
            // Everything else the CLI says about itself — nothing this projection presents.
            _ => Ok(Vec::new()),
        }
    }

    /// What the CLI reports of a user message written into the Turn — its Prompt, or a steer —
    /// under the uuid it was written with: when a loop takes it up, which is what tells the result
    /// that answers the Turn from one ending a loop the message never joined. A message a loop
    /// never took up and never will, once the result ending the Turn's last loop was left waiting
    /// on it, leaves nothing else to Settle the Turn, so it Settles here. One `cancelled` there was
    /// swept by the interrupt the user asked for — Suru withdraws no message of its own, and with
    /// no loop running that interrupt aborts none, so no aborted result will follow — and the Turn
    /// Settles as interrupted. One the CLI discarded or refused leaves the Turn as the completed
    /// loop it last ran.
    fn project_command_lifecycle(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(lifecycle) = serde_json::from_value::<CommandLifecycle>(message) else {
            return Vec::new();
        };
        let settled = match lifecycle.state {
            CommandLifecycleState::Queued | CommandLifecycleState::Other => {
                self.turn.message_queued();
                return Vec::new();
            }
            CommandLifecycleState::Started => {
                self.turn.message_started(&lifecycle.command_uuid);
                return Vec::new();
            }
            CommandLifecycleState::Cancelled => ProviderEvent::TurnInterrupted,
            CommandLifecycleState::Completed
            | CommandLifecycleState::Discarded
            | CommandLifecycleState::Refused => ProviderEvent::TurnCompleted,
        };
        if !self.turn.message_ended(&lifecycle.command_uuid) {
            return Vec::new();
        }
        self.turn_metering = None;
        vec![settled.into()]
    }

    /// The task lifecycle the CLI reports beside the conversations. Every task joins the roster
    /// of background work an interrupt stops before it stops the loop — kept from the tasks' own
    /// start and settle rather than from the roster snapshot the CLI also sends, because that
    /// snapshot covers only work already in the background, and a subagent still running in the
    /// foreground of the Turn is exactly what an interrupt alone would leave behind. A task
    /// running an agent is more: a Subagent, opened in the conversation that spawned it, resumed
    /// from whichever conversation sent it more, and revised and settled under its task id.
    fn project_task_lifecycle(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<SystemMessage>(message) else {
            return Vec::new();
        };
        match message.subtype.as_str() {
            "task_started" => self.project_task_started(message),
            // A progress tick's description is the subagent's latest tool activity ("Running
            // cargo test"), not what it was asked to do, so it revises nothing.
            "task_progress" => Vec::new(),
            "task_updated" => {
                let (description, backgrounded) = message.patch.map_or((None, None), |patch| {
                    (patch.description, patch.is_backgrounded)
                });
                let mut projected = match (&message.task_id, backgrounded) {
                    (Some(task_id), Some(true)) => self.project_task_backgrounded(task_id),
                    _ => Vec::new(),
                };
                projected.extend(self.project_task_description(message.task_id, description));
                projected
            }
            // However a task ends — finished, failed, or stopped — the CLI notifies, so the
            // notification alone is enough to settle it.
            "task_notification" => self.project_task_settled(message),
            _ => Vec::new(),
        }
    }

    fn project_task_started(&mut self, message: SystemMessage) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = message.task_id else {
            return Vec::new();
        };
        self.turn.task_started(task_id.clone());
        match TaskKind::of(message.task_type.as_deref(), message.is_backgrounded) {
            TaskKind::Subagent => {}
            // The agent reads a foreground task's outcome from its tool result inside the Turn,
            // so there is nothing to wait on past the Turn and no Watch to start.
            TaskKind::Foreground => {
                self.foreground_tasks.insert(
                    task_id,
                    ForegroundTask {
                        description: message.description,
                        tool_use_id: message.tool_use_id,
                        owned_by_subagent: message.owned_by_subagent,
                    },
                );
                return Vec::new();
            }
            TaskKind::Watch => {
                self.foreground_tasks.remove(&task_id);
                return self.project_watch_started(
                    task_id,
                    message.description,
                    message.tool_use_id.as_deref(),
                    message.owned_by_subagent,
                );
            }
            TaskKind::Unwatched => return Vec::new(),
        }
        // The roster is the running process's, so an agent task joins it on every start — even
        // one whose row is still working, which is what an agent the previous process was running
        // looks like when the process that replaced it resumes the same task.
        self.turn.subagent_task_started(task_id.clone());
        if self
            .agent_tasks
            .get(&task_id)
            .is_some_and(|task| task.working.is_some())
        {
            return Vec::new();
        }
        // Every Delegation starts its task from the tool use that sent it — a SendMessage resume
        // names the SendMessage — so a settled agent started again naming no tool use at all was
        // sent nothing: its own Watch settled, and the CLI woke it to hear how.
        if message.tool_use_id.is_none()
            && let Some(task) = self.agent_tasks.get_mut(&task_id)
        {
            task.working = Some(message.description.unwrap_or_default());
            let conversation = task.conversation.clone();
            if conversation.is_none() {
                self.unplaced_resumes.insert(task_id.clone());
            }
            // The agent was sent nothing, so any steer still waiting for it was never delivered.
            self.pending_steers.remove(&task_id);
            let subagent_id = ProviderSubagentId::new(task_id);
            let model = conversation
                .and_then(|conversation| self.subagent_models.get(&conversation))
                .cloned()
                .map(|model| {
                    ProviderEvent::SubagentModelChanged {
                        subagent_id: subagent_id.clone(),
                        model,
                    }
                    .into()
                });
            return std::iter::once(ProviderEvent::SubagentWoken { subagent_id }.into())
                .chain(model)
                .collect();
        }
        // The tool use the start names is the Delegation, and the conversation that ran it is the
        // delegating one. A start naming no tool this projection saw delegates from the loop's own
        // conversation, which always has a Turn to land in (ADR 0015). The one exception is the
        // CLI restarting a settled agent for a steer it finished too soon to read: that start
        // names the agent's own spawn, and the steer is the Delegation, sent by whichever Agent
        // sent it (ADR 0032). Any other start of a settled agent means its pending steers were
        // never delivered, so they are discarded.
        let delegation = match self.late_steer(&task_id, message.tool_use_id.as_deref()) {
            Some(steer) => Some(DelegationTool {
                delegator: steer.delegator,
                kind: DelegationKind::Resume {
                    description: steer.description,
                    message: Some(steer.text),
                    to: None,
                },
            }),
            None => {
                self.pending_steers.remove(&task_id);
                message
                    .tool_use_id
                    .as_ref()
                    .and_then(|tool| self.delegation_tools.remove(tool))
            }
        };
        let (delegator, mut kind) = delegation.map_or(
            (OWNING_CONVERSATION, DelegationKind::Spawn { prompt: None }),
            |tool| (tool.delegator, tool.kind),
        );
        // What the agent was handed is the delegating tool's own input, or where that never
        // streamed, the text the start carries.
        let delegation = kind.text().or(message.prompt);
        let subagent_id = ProviderSubagentId::new(task_id.clone());
        let name = message
            .subagent_type
            .unwrap_or_else(|| TASK_TOOL.to_owned());
        let mut recorded = false;
        let (conversation, event) = match (self.agent_tasks.get_mut(&task_id), kind) {
            // A task this wire started before, or one the Resume State recorded spawning before a
            // restart, is a settled agent resumed: the same Subagent, continuing the same
            // conversation. The start repeats the spawn's description, so the resume reads what the
            // SendMessage asked instead.
            (Some(task), kind) => {
                let description = resume_description(kind)
                    .or(message.description)
                    .unwrap_or_default();
                task.working = Some(description.clone());
                if task.conversation.is_none() {
                    self.unplaced_resumes.insert(task_id.clone());
                }
                (
                    task.conversation.clone(),
                    ProviderEvent::SubagentResumed {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
            // A resume of an agent this wire never saw start, and no Resume State recorded, is
            // still a resume: orchestration finds the Session the agent's identity was stored
            // with, or records it as a new Subagent where none was. Its conversation rides under a
            // spawn this wire never saw either, so it is claimed from the chunks once they come.
            (None, kind @ DelegationKind::Resume { .. }) => {
                let description = resume_description(kind)
                    .or(message.description)
                    .unwrap_or_default();
                self.agent_tasks.insert(
                    task_id.clone(),
                    AgentTask {
                        conversation: None,
                        working: Some(description.clone()),
                    },
                );
                self.unplaced_resumes.insert(task_id.clone());
                (
                    None,
                    ProviderEvent::SubagentResumed {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
            (None, DelegationKind::Spawn { .. }) => {
                // A spawn's tool use is the identity the subagent's every chunk rides under, and
                // the Resume State records it so a resume after a restart still routes them. A
                // start that names no tool use leaves nothing to attribute chunks by, though the
                // Subagent's row and settle still reach the Transcript.
                let conversation = message.tool_use_id;
                if let Some(conversation) = &conversation {
                    self.conversation_agents
                        .insert(conversation.clone(), task_id.clone());
                    self.resume
                        .agents
                        .insert(task_id.clone(), conversation.clone());
                    recorded = true;
                }
                let description = message.description.unwrap_or_default();
                self.agent_tasks.insert(
                    task_id.clone(),
                    AgentTask {
                        conversation: conversation.clone(),
                        working: Some(description.clone()),
                    },
                );
                (
                    conversation,
                    ProviderEvent::SubagentStarted {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
        };
        // The start alone is attributed to the delegating conversation, because it is what decides
        // which Session the row stands in. The rest of the lifecycle addresses the row by the
        // Subagent's own identity and rides the owning conversation, so a nested Subagent's settle
        // still lands after its spawner's own — order the wire does not promise.
        let mut projected = vec![self.attributed(&delegator, event)];
        if let Some(model) = conversation
            .and_then(|conversation| self.subagent_models.get(&conversation))
            .cloned()
        {
            projected.push(
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(task_id),
                    model,
                }
                .into(),
            );
        }
        if recorded {
            projected.push(self.resume_state_changed());
        }
        projected
    }

    /// The steer a settled agent's start is Claude's restart for, if it is one: a start naming the
    /// agent's own spawn — where a resume through SendMessage names the SendMessage — while a steer
    /// is still waiting for it. The restart hands the agent the first steer it never read as a
    /// fresh prompt; any queued after it stay pending, read like any other steer at the restarted
    /// stretch's next tool round.
    fn late_steer(&mut self, task_id: &str, tool_use_id: Option<&str>) -> Option<PendingSteer> {
        let spawn = self.agent_tasks.get(task_id)?.conversation.as_deref()?;
        if tool_use_id != Some(spawn) {
            return None;
        }
        let queued = self.pending_steers.get_mut(task_id)?;
        let steer = queued.pop_front();
        if queued.is_empty() {
            self.pending_steers.remove(task_id);
        }
        steer
    }

    /// A tool round in a subagent conversation: its tool results echoed back on their way into
    /// the agent's next model call, which is where the CLI hands the agent every steer queued for
    /// it so far.
    fn steers_read(&mut self, conversation: &str) {
        let Some(steers) = self
            .conversation_agents
            .get(conversation)
            .and_then(|task_id| self.pending_steers.get_mut(task_id))
        else {
            return;
        };
        for steer in steers {
            steer.read = true;
        }
    }

    /// The steers a working agent read at its last tool round, placed as its conversation carries
    /// the first assistant message it wrote having read them — just before it, each attributed to
    /// the conversation that sent it, in the order they were sent. Steers queued since that round
    /// wait for the next one.
    fn deliver_read_steers(&mut self, conversation: &str) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = self
            .conversation_agents
            .get(conversation)
            .filter(|task_id| {
                self.agent_tasks
                    .get(*task_id)
                    .is_some_and(|task| task.working.is_some())
            })
            .cloned()
        else {
            return Vec::new();
        };
        let Some(steers) = self.pending_steers.get_mut(&task_id) else {
            return Vec::new();
        };
        let read = steers.iter().take_while(|steer| steer.read).count();
        let delivered = steers.drain(..read).collect::<Vec<_>>();
        if steers.is_empty() {
            self.pending_steers.remove(&task_id);
        }
        delivered
            .into_iter()
            .map(|steer| {
                self.attributed(
                    &steer.delegator,
                    ProviderEvent::SubagentSteered {
                        subagent_id: ProviderSubagentId::new(task_id.clone()),
                        delegation: steer.text,
                    },
                )
            })
            .collect()
    }

    /// What a SendMessage's tool result says of the message it sent. One the CLI queued for an
    /// agent still working is a steer, held pending against the agent the result names until the
    /// agent reads it; one it refused (`success: false`) was never delivered, and delegates
    /// nothing. Any other result — a resume, whose task's start carries the Delegation — leaves
    /// the tool use to that start.
    fn receive_send_message_result(&mut self, tool_use_id: &str, content: &Value, is_error: bool) {
        let result = serde_json::from_str::<Value>(&tool_result_text(content)).ok();
        let success = result
            .as_ref()
            .and_then(|result| result.get("success"))
            .and_then(Value::as_bool);
        let queued = success == Some(true)
            && result
                .as_ref()
                .and_then(|result| result.get("message"))
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains(QUEUED_FOR_DELIVERY));
        if !queued {
            if is_error || success == Some(false) {
                self.delegation_tools.remove(tool_use_id);
            }
            return;
        }
        let Some(DelegationTool {
            delegator,
            kind:
                DelegationKind::Resume {
                    description,
                    message: Some(text),
                    to,
                },
        }) = self.delegation_tools.remove(tool_use_id)
        else {
            return;
        };
        // The result pins the agent it queued the message for by its task id, which `to` — an
        // agent's name, where it was given one — need not be.
        let Some(target) = result
            .as_ref()
            .and_then(|result| result.pointer("/pin/id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(to)
        else {
            return;
        };
        self.pending_steers
            .entry(target)
            .or_default()
            .push_back(PendingSteer {
                delegator,
                text,
                description,
                read: false,
            });
    }

    /// A background task that may wake the loop once its Turn has ended: a Watch, described by
    /// what the task's start says it does, or else by the command that started it. A start
    /// repeating a Watch already live announces nothing new. The owner is kept with the Watch, so
    /// its settle — or its stop, or its loss with the process — lands where its start did.
    fn project_watch_started(
        &mut self,
        task_id: String,
        description: Option<String>,
        tool_use_id: Option<&str>,
        owned_by_subagent: Option<bool>,
    ) -> Vec<AttributedProviderEvent> {
        if self.watches.contains_key(&task_id) {
            return Vec::new();
        }
        let description = description
            .filter(|description| !description.trim().is_empty())
            .or_else(|| {
                tool_use_id
                    .and_then(|tool| self.running_commands.get(tool))
                    .map(|command| command.command.clone())
            })
            .unwrap_or_else(|| task_id.clone());
        let owner = self.watch_owner(&task_id, tool_use_id, owned_by_subagent);
        let event = ProviderEvent::WatchStarted {
            watch_id: ProviderWatchId::new(task_id.clone()),
            description,
        };
        let started = self.attributed(&owner, event);
        self.watches.insert(task_id, owner);
        vec![started]
    }

    /// A foreground task the CLI moved to the background, so the agent stopped waiting on it and
    /// its settling will wake the loop: a Watch from here on, described and owned as its start
    /// would have made it. A task not running in the foreground has nothing to promote. The move
    /// has not been captured live, but the 2.1.280 CLI's own schema says a later move to the
    /// background arrives as `task_updated` with `patch.is_backgrounded`, and it arms a foreground
    /// shell to be backgrounded on its own once it runs long enough.
    fn project_task_backgrounded(&mut self, task_id: &str) -> Vec<AttributedProviderEvent> {
        let Some(task) = self.foreground_tasks.remove(task_id) else {
            return Vec::new();
        };
        self.project_watch_started(
            task_id.to_owned(),
            task.description,
            task.tool_use_id.as_deref(),
            task.owned_by_subagent,
        )
    }

    /// The conversation whose agent left a Watch running, and so the one its settling wakes: the
    /// conversation that ran the tool use launching it, the way a spawn's delegating conversation
    /// is found (ADR 0030). A background Subagent's Watch stays its own after the Subagent settles,
    /// because the conversation keeps naming the agent it belongs to.
    ///
    /// The loop's own conversation owns whatever cannot be placed: a start naming no tool this
    /// wire saw, or one run by a conversation no Subagent claims yet — which would otherwise land
    /// nowhere, and leave its Session reading idle while the Watch runs. `owned_by_subagent` only
    /// confirms the reading: where the CLI says a subagent owns a Watch this projection cannot
    /// place under one, the disagreement is logged and the loop keeps it.
    fn watch_owner(
        &self,
        task_id: &str,
        tool_use_id: Option<&str>,
        owned_by_subagent: Option<bool>,
    ) -> ConversationKey {
        let owner = tool_use_id
            .and_then(|tool| self.intervention_tools.get(tool))
            .cloned()
            .flatten()
            .filter(|conversation| self.conversation_agents.contains_key(conversation));
        if owner.is_none() && owned_by_subagent == Some(true) {
            tracing::debug!(
                task_id,
                tool_use_id,
                "a Watch the CLI says a subagent owns names no subagent conversation; the loop's own conversation keeps it"
            );
        }
        owner
    }

    /// A Watch's own notification that it settled, which the CLI delivers to the agent whose
    /// loop it wakes; the notification's summary is how the Watch settled in the CLI's words. A
    /// stopped Watch wakes nothing: the agent stopped it itself mid-Turn, or Suru stopped it on
    /// the way to interrupting the loop, and neither begins a Continuation. Any status this build
    /// does not know reads as failed, as it does for a Subagent.
    fn project_watch_settled(
        &mut self,
        task_id: &str,
        message: &SystemMessage,
    ) -> Option<AttributedProviderEvent> {
        let owner = self.watches.remove(task_id)?;
        let outcome = match message.status.as_deref() {
            None | Some(TASK_COMPLETED_STATUS) => ProviderWatchOutcome::Completed,
            Some(TASK_STOPPED_STATUS) => ProviderWatchOutcome::Stopped,
            Some(_) => ProviderWatchOutcome::Failed,
        };
        let event = ProviderEvent::WatchSettled {
            watch_id: ProviderWatchId::new(task_id),
            outcome,
            summary: message
                .summary
                .clone()
                .filter(|summary| !summary.trim().is_empty()),
            woke_agent: outcome != ProviderWatchOutcome::Stopped,
        };
        Some(self.attributed(&owner, event))
    }

    /// Tasks the Session stopped and the CLI acknowledged stopping, which are off its roster
    /// whether or not their notifications follow. Each one still a live Watch settles as stopped
    /// here, waking nothing, so the Session stops Monitoring on the acknowledgement alone; a
    /// notification arriving afterwards finds the Watch already settled and repeats nothing.
    fn project_watches_stopped(&mut self, tasks: &[String]) -> Vec<AttributedProviderEvent> {
        tasks
            .iter()
            .filter_map(|task_id| {
                // A stopped agent reads none of the steers still waiting for it.
                self.pending_steers.remove(task_id);
                // A stopped foreground task was never a Watch, so its stop settles nothing.
                self.foreground_tasks.remove(task_id);
                let owner = self.watches.remove(task_id)?;
                Some(self.attributed(
                    &owner,
                    ProviderEvent::WatchSettled {
                        watch_id: ProviderWatchId::new(task_id.clone()),
                        outcome: ProviderWatchOutcome::Stopped,
                        summary: None,
                        woke_agent: false,
                    },
                ))
            })
            .collect()
    }

    /// The CLI process this projection was reading has ended — stopped by the Session, or replaced
    /// by one spawned under another Agent Selection — and everything it wrote before it did has
    /// projected already. Every task on its roster died with it, so each live Watch settles as
    /// lost and wakes nothing, and the roster starts empty again for whatever the next process
    /// reports: output the old process left queued can no longer put its tasks back.
    fn project_process_ended(&mut self) -> Vec<AttributedProviderEvent> {
        self.turn.tasks_died_with_process();
        // A foreground task that died with the process was never a Watch, so none is lost.
        self.foreground_tasks.clear();
        // Nor is any steer the process still held for its agents ever delivered.
        self.pending_steers.clear();
        std::mem::take(&mut self.watches)
            .into_iter()
            .map(|(task_id, owner)| {
                self.attributed(
                    &owner,
                    ProviderEvent::WatchSettled {
                        watch_id: ProviderWatchId::new(task_id),
                        outcome: ProviderWatchOutcome::Lost,
                        summary: None,
                        woke_agent: false,
                    },
                )
            })
            .collect()
    }

    /// The Resume State as it stands now, for orchestration to store in place of what the
    /// Session's startup reported.
    fn resume_state_changed(&self) -> AttributedProviderEvent {
        ProviderEvent::ResumeStateChanged {
            resume_state: self.resume.to_provider(),
        }
        .into()
    }

    /// Claims a subagent conversation nothing on this wire has attributed for the one agent resumed
    /// without a conversation of its own — an agent spawned before a restart that the Resume State
    /// never recorded, whose conversation rides under a spawn this wire never saw. Only a lone such
    /// agent can claim it: with two working, nothing on the wire tells whose a chunk is, so neither
    /// claims and their chunks land nowhere, as an unknown conversation's always have. A claimed
    /// conversation is recorded like a spawned one, so the next restart need not claim it again.
    fn claim_conversation(&mut self, owner: &ConversationKey) -> Option<AttributedProviderEvent> {
        let conversation = owner.as_ref()?;
        if self.unplaced_resumes.len() != 1
            || self.conversation_agents.contains_key(conversation)
            || self.delegation_tools.contains_key(conversation)
        {
            return None;
        }
        let task_id = self.unplaced_resumes.pop_first()?;
        self.conversation_agents
            .insert(conversation.clone(), task_id.clone());
        if let Some(task) = self.agent_tasks.get_mut(&task_id) {
            task.conversation = Some(conversation.clone());
        }
        self.resume.agents.insert(task_id, conversation.clone());
        Some(self.resume_state_changed())
    }

    /// A revised description for a running Subagent. Tasks that are not Subagents, tasks never
    /// started, and updates repeating the description unchanged all publish nothing.
    fn project_task_description(
        &mut self,
        task_id: Option<String>,
        description: Option<String>,
    ) -> Vec<AttributedProviderEvent> {
        let Some((task_id, description)) = task_id.zip(description) else {
            return Vec::new();
        };
        let Some(working) = self
            .agent_tasks
            .get_mut(&task_id)
            .and_then(|task| task.working.as_mut())
        else {
            return Vec::new();
        };
        if *working == description {
            return Vec::new();
        }
        working.clone_from(&description);
        vec![
            ProviderEvent::SubagentUpdated {
                subagent_id: ProviderSubagentId::new(task_id),
                description,
            }
            .into(),
        ]
    }

    fn project_task_settled(&mut self, message: SystemMessage) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = message.task_id.clone() else {
            return Vec::new();
        };
        self.turn.task_settled(&task_id);
        // A stopped agent reads none of the steers still waiting for it. One that settled on its
        // own keeps them, since the CLI may yet restart it to read the first.
        if message.status.as_deref() == Some(TASK_STOPPED_STATUS) {
            self.pending_steers.remove(&task_id);
        }
        // A foreground task's notification comes inside the Turn that waited on it, whose tool
        // result already told the agent how it went: it wakes nothing and settles no Watch.
        if self.foreground_tasks.remove(&task_id).is_some() {
            return Vec::new();
        }
        if let Some(settled) = self.project_watch_settled(&task_id, &message) {
            return vec![settled];
        }
        let Some(task) = self
            .agent_tasks
            .get_mut(&task_id)
            .filter(|task| task.working.is_some())
        else {
            return Vec::new();
        };
        task.working = None;
        self.unplaced_resumes.remove(&task_id);
        // The settle is the last of this stretch's events: whatever its streams leave open, the
        // settle closes in the child Session, and nothing more of the conversation's lands until a
        // resume begins the next stretch.
        if let Some(conversation) = task.conversation.clone() {
            self.conversations.remove(&Some(conversation.clone()));
            self.intervention_tools
                .retain(|_, owner| owner.as_deref() != Some(conversation.as_str()));
            self.running_commands
                .retain(|_, command| command.owner.as_deref() != Some(conversation.as_str()));
            self.running_file_changes.retain(|_, file_change| {
                file_change.owner.as_deref() != Some(conversation.as_str())
            });
            self.running_tool_calls
                .retain(|_, tool_call| tool_call.owner.as_deref() != Some(conversation.as_str()));
            self.subagent_models.remove(&conversation);
        }
        let status = if message
            .status
            .as_deref()
            .is_none_or(|status| status == TASK_COMPLETED_STATUS)
        {
            ProviderSubagentStatus::Completed
        } else {
            ProviderSubagentStatus::Failed
        };
        vec![
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(task_id),
                status,
            }
            .into(),
        ]
    }

    fn project_stream_event(
        &mut self,
        message: Value,
    ) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        let message: StreamEventMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed stream event: {error}"
            ))
        })?;
        let owner: ConversationKey = message.parent_tool_use_id;
        let claimed = self.claim_conversation(&owner);
        let event = message.event;
        // The conversation steps out of the table while its chunk projects, so the projection's
        // shared state — the commands and spawns other conversations feed too — stays reachable.
        let mut conversation = self.conversations.remove(&owner).unwrap_or_default();
        conversation.streamed = true;
        let mut projected = Vec::new();
        match event.kind.as_str() {
            // Only a new owning message can begin a native loop; child messages and trailing
            // block stops cannot revive a settled Turn.
            "message_start" if owner.is_none() => {
                if let Some(selection) = self.turn.begin_continuation() {
                    projected.push(ProviderEvent::ContinuationStarted { selection });
                }
            }
            "content_block_start" => {
                if let Some(block) = event.content_block {
                    // Blocks stream strictly one at a time within a conversation, so a start
                    // while its text or thinking block is open means a stop was lost; settle
                    // what is open rather than interleaving two.
                    if conversation.open_text_block.take().is_some() {
                        projected.push(ProviderEvent::AgentMessageCompleted);
                    }
                    self.settle_open_thinking(&mut conversation, &mut projected);
                    match block.kind.as_str() {
                        "text" => {
                            conversation.open_text_block = event.index;
                            projected.push(ProviderEvent::AgentMessageStarted);
                            if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                                projected.push(ProviderEvent::AgentMessageDelta { content: text });
                            }
                        }
                        "thinking" => {
                            if let Some(index) = event.index {
                                self.open_thinking(
                                    &mut conversation,
                                    index,
                                    block.thinking,
                                    &mut projected,
                                );
                            }
                        }
                        "tool_use" => self.open_tool_use(
                            &owner,
                            &mut conversation,
                            event.index,
                            block,
                            &mut projected,
                        ),
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                if let Some((index, delta)) = event.index.zip(event.delta) {
                    if conversation.open_text_block == Some(index) && delta.kind == "text_delta" {
                        projected.push(ProviderEvent::AgentMessageDelta {
                            content: delta.text.unwrap_or_default(),
                        });
                    } else if let Some(thinking) = conversation
                        .open_thinking
                        .as_mut()
                        .filter(|open| open.index == index && delta.kind == "thinking_delta")
                    {
                        let split = thinking.splitter.push(&delta.thinking.unwrap_or_default());
                        Self::project_thinking_split(
                            &mut self.reasoning_blocks,
                            thinking,
                            split,
                            &mut projected,
                        );
                    } else if delta.kind == "input_json_delta"
                        && let Some(tool) = conversation.open_tools.get_mut(&index)
                    {
                        tool.streamed_input
                            .push_str(&delta.partial_json.unwrap_or_default());
                    }
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.index {
                    if conversation.open_text_block == Some(index) {
                        conversation.open_text_block = None;
                        projected.push(ProviderEvent::AgentMessageCompleted);
                    } else if conversation
                        .open_thinking
                        .as_ref()
                        .is_some_and(|open| open.index == index)
                    {
                        self.settle_open_thinking(&mut conversation, &mut projected);
                    } else {
                        self.close_tool_use(&owner, &mut conversation, index, &mut projected);
                    }
                }
            }
            // Message boundaries carry nothing the Transcript presents.
            _ => {}
        }
        self.conversations.insert(owner.clone(), conversation);
        let attribution = self.attribution(&owner);
        Ok(claimed
            .into_iter()
            .chain(projected.into_iter().map(|event| AttributedProviderEvent {
                attribution: attribution.clone(),
                event,
            }))
            .collect())
    }

    /// A full-message snapshot of an assistant message. A conversation that streamed is already
    /// in the Transcript chunk by chunk, so its snapshots are passed over. A subagent's
    /// conversation never streams — verified against 2.1.280, which attributes no `stream_event`
    /// to a parent tool use — so its snapshots are all the wire carries of it, and each block
    /// projects as a settled whole: text as the agent Message, thinking as Reasoning split at its
    /// headings, and tool uses through the same open/close pair the streaming path takes, which
    /// is what records a Task block as a spawn and a Bash block as a Command awaiting its result.
    fn project_assistant_snapshot(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<AssistantMessageSnapshot>(message) else {
            return Vec::new();
        };
        let owner: ConversationKey = message.parent_tool_use_id;
        let claimed = self.claim_conversation(&owner);
        let observed_model = owner.as_ref().and_then(|conversation| {
            message
                .message
                .model
                .filter(|model| !model.is_empty())
                .map(|model| (conversation.clone(), crate::protocol::ModelId::new(model)))
        });
        if let Some((conversation, model)) = observed_model.as_ref() {
            self.subagent_models
                .insert(conversation.clone(), model.clone());
        }
        let steers = owner.as_deref().map_or_else(Vec::new, |conversation| {
            self.deliver_read_steers(conversation)
        });
        let held = self.conversations.remove(&owner);
        let known = held.is_some();
        let mut conversation = held.unwrap_or_default();
        let mut projected = Vec::new();
        if !conversation.streamed {
            for (index, block) in message.message.content.into_iter().enumerate() {
                let index = index as u64;
                match block.kind.as_str() {
                    "text" => {
                        if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                            projected.push(ProviderEvent::AgentMessageStarted);
                            projected.push(ProviderEvent::AgentMessageDelta { content: text });
                            projected.push(ProviderEvent::AgentMessageCompleted);
                        }
                    }
                    "thinking" => {
                        if block
                            .thinking
                            .as_deref()
                            .is_some_and(|text| !text.is_empty())
                        {
                            self.open_thinking(
                                &mut conversation,
                                index,
                                block.thinking,
                                &mut projected,
                            );
                            self.settle_open_thinking(&mut conversation, &mut projected);
                        }
                    }
                    "tool_use" => {
                        self.open_tool_use(
                            &owner,
                            &mut conversation,
                            Some(index),
                            block,
                            &mut projected,
                        );
                        self.close_tool_use(&owner, &mut conversation, index, &mut projected);
                    }
                    _ => {}
                }
            }
        }
        // A snapshot's blocks arrive settled, so they leave nothing open behind: a conversation
        // the table did not already hold has nothing to remember, and reinserting one would
        // recreate entries for subagents whose settle already cleared them.
        if known {
            self.conversations.insert(owner.clone(), conversation);
        }
        // Evidence reaches the row of the stretch the agent is working in; while none is, it waits
        // for the start that opens one.
        let model_evidence = observed_model.and_then(|(conversation, model)| {
            let task_id = self.conversation_agents.get(&conversation)?;
            self.agent_tasks.get(task_id)?.working.as_ref()?;
            Some(
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(task_id.clone()),
                    model,
                }
                .into(),
            )
        });
        let mut attributed_events = claimed
            .into_iter()
            .chain(model_evidence)
            .chain(steers)
            .collect::<Vec<_>>();
        let attribution = self.attribution(&owner);
        attributed_events.extend(projected.into_iter().map(|event| AttributedProviderEvent {
            attribution: attribution.clone(),
            event,
        }));
        attributed_events
    }

    /// Starts tracking a `tool_use` block whose input is about to stream. An Agent or Task tool use
    /// is remembered as a spawn and a SendMessage tool use as a resume, so the task the CLI starts
    /// for either opens its row in the conversation that ran the tool. An edit opens its File
    /// Change now, the change it makes following once its input has streamed, and a tool use no
    /// more specific Activity records opens its Tool Call now. The CLI starts a use — asking its
    /// `can_use_tool` Approval — from the full `assistant` message it writes for the block before
    /// it streams the block's close, so the Approval may overtake the close (verified against
    /// 2.1.283), and the row must stand from the block's opening for the Approval to link to it.
    fn open_tool_use(
        &mut self,
        owner: &ConversationKey,
        conversation: &mut ConversationInFlight,
        index: Option<u64>,
        block: ContentBlock,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let (Some(index), Some(id), Some(name)) = (index, block.id, block.name) else {
            return;
        };
        self.intervention_tools.insert(id.clone(), owner.clone());
        let disposition = ToolDisposition::of(&name);
        let row = disposition.row_activity_id(&id);
        let kind = match (disposition, row) {
            (ToolDisposition::Spawn, _) => Some(DelegationKind::Spawn { prompt: None }),
            (ToolDisposition::Resume, _) => Some(DelegationKind::Resume {
                description: None,
                message: None,
                to: None,
            }),
            (ToolDisposition::FileChange(_), Some(activity))
                if !self.running_file_changes.contains_key(&id) =>
            {
                projected.push(ProviderEvent::FileChangeStarted {
                    activity_id: activity.clone(),
                    changes: Vec::new(),
                });
                self.running_file_changes.insert(
                    id.clone(),
                    RunningFileChange {
                        owner: owner.clone(),
                        activity,
                    },
                );
                None
            }
            (ToolDisposition::ToolCall, Some(activity))
                if !self.running_tool_calls.contains_key(&id) =>
            {
                let (server, tool) = mcp_tool(&name)
                    .map_or((None, name.as_str()), |(server, tool)| {
                        (Some(server.to_owned()), tool)
                    });
                projected.push(ProviderEvent::ToolCallStarted {
                    activity_id: activity.clone(),
                    name: tool.to_owned(),
                    server,
                    input: None,
                });
                self.running_tool_calls.insert(
                    id.clone(),
                    RunningToolCall {
                        owner: owner.clone(),
                        activity,
                    },
                );
                None
            }
            _ => None,
        };
        if let Some(kind) = kind {
            self.delegation_tools.insert(
                id.clone(),
                DelegationTool {
                    delegator: owner.clone(),
                    kind,
                },
            );
        }
        conversation.open_tools.insert(
            index,
            OpenToolUse {
                id,
                name,
                streamed_input: String::new(),
                opening_input: block.input,
            },
        );
    }

    /// Closes a `tool_use` block: a completed Bash tool use becomes a running Command Activity in
    /// the conversation that ran it, recording the command as a reader should see it — any
    /// leading change of directory lifted out as where it runs, and the execution directory
    /// where it changes none — a File Change its opening started is given the change it makes, a
    /// Tool Call its opening started is given its input, and a completed delegating tool use
    /// leaves what it asks for the spawn or resume it starts. Any other tool, a row already
    /// settled — its use declined before the close — and input in no shape this projection reads,
    /// are passed over.
    fn close_tool_use(
        &mut self,
        owner: &ConversationKey,
        conversation: &mut ConversationInFlight,
        index: u64,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let Some(tool) = conversation.open_tools.remove(&index) else {
            return;
        };
        let tool_call = self
            .running_tool_calls
            .get(&tool.id)
            .map(|tool_call| tool_call.activity.clone());
        let disposition = ToolDisposition::of(&tool.name);
        let command = disposition == ToolDisposition::Command;
        let file_change = match disposition {
            ToolDisposition::FileChange(edit) => self
                .running_file_changes
                .get(&tool.id)
                .map(|file_change| (edit, file_change.activity.clone())),
            _ => None,
        };
        let delegation = self.delegation_tools.get_mut(&tool.id);
        if !command && delegation.is_none() && file_change.is_none() && tool_call.is_none() {
            return;
        }
        let streamed = serde_json::from_str::<Value>(&tool.streamed_input).ok();
        let input = streamed.or(tool.opening_input).unwrap_or(Value::Null);
        if let Some(activity_id) = tool_call {
            projected.push(ProviderEvent::ToolCallInputKnown {
                activity_id,
                input: present_tool_input(&input),
            });
            return;
        }
        if let Some(delegation) = delegation {
            delegation.kind.read_input(&input);
            return;
        }
        if let Some((edit, activity_id)) = file_change {
            if let Some(change) = edit.change(&input, &self.execution_directory) {
                projected.push(ProviderEvent::FileChangeUpdated {
                    activity_id,
                    changes: vec![change],
                });
            }
            return;
        }
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return;
        };
        let Some(activity_id) = disposition.row_activity_id(&tool.id) else {
            return;
        };
        let PresentedCommand { command, cwd } = present_command(command.to_owned());
        projected.push(ProviderEvent::CommandStarted {
            activity_id: activity_id.clone(),
            command: command.clone(),
            cwd: Some(cwd.unwrap_or_else(|| self.execution_directory.clone())),
        });
        self.running_commands.insert(
            tool.id,
            RunningCommand {
                owner: owner.clone(),
                activity: activity_id,
                command,
            },
        );
    }

    /// The tool results a `user` message echoes back, settling the commands, File Changes and Tool
    /// Calls they report on in whichever conversation ran each: the result's text is a command's
    /// or Tool Call's output, a result reporting an error settles its use as failed, and a Tool
    /// Call counts the parts of its result that are not text as omitted. A user message in any
    /// other shape is not the
    /// projection's to present.
    fn project_tool_results(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<EchoedUserMessage>(message) else {
            return Vec::new();
        };
        let EchoedUserContent::Blocks(blocks) = message.message.content else {
            return Vec::new();
        };
        if let Some(conversation) = message.parent_tool_use_id.as_deref()
            && blocks.iter().any(|block| block.kind == "tool_result")
        {
            self.steers_read(conversation);
        }
        let mut projected = Vec::new();
        for block in blocks {
            if block.kind != "tool_result" {
                continue;
            }
            let Some(tool) = block.tool_use_id.as_deref() else {
                continue;
            };
            if self
                .delegation_tools
                .get(tool)
                .is_some_and(|delegation| matches!(delegation.kind, DelegationKind::Resume { .. }))
            {
                self.receive_send_message_result(tool, &block.content, block.is_error);
                continue;
            }
            self.settle_tool_use(tool, &block.content, block.is_error, &mut projected);
        }
        projected
    }

    /// Settles as failed the row of a use whose Approval the user declined: the tool never runs,
    /// and what Claude was told of the refusal — `message` — is what the use returned. The CLI
    /// also echoes the refusal back as the use's tool result, an error carrying that same message
    /// — verified against 2.1.283 — so whichever of the two arrives second finds nothing left to
    /// settle, and the row settles on the Decision even if the echo never comes.
    fn project_declined_tool_use(
        &mut self,
        tool_use_id: &str,
        message: &str,
    ) -> Vec<AttributedProviderEvent> {
        let mut projected = Vec::new();
        self.settle_tool_use(
            tool_use_id,
            &Value::String(message.to_owned()),
            true,
            &mut projected,
        );
        projected
    }

    /// Settles the row recording the use `tool_use_id` — a File Change, a Tool Call or a Command —
    /// in whichever conversation ran it, from what the use returned: `content`'s text is a Tool
    /// Call's or command's output, a Tool Call counts the parts of it that are not text as
    /// omitted, and `is_error` settles the use as failed. The row is forgotten, so nothing settles
    /// it twice; a use no running row records settles nothing.
    fn settle_tool_use(
        &mut self,
        tool_use_id: &str,
        content: &Value,
        is_error: bool,
        projected: &mut Vec<AttributedProviderEvent>,
    ) {
        if let Some(file_change) = self.running_file_changes.remove(tool_use_id) {
            projected.push(self.attributed(
                &file_change.owner,
                ProviderEvent::FileChangeCompleted {
                    activity_id: file_change.activity,
                    status: if is_error {
                        ProviderFileChangeStatus::Failed
                    } else {
                        ProviderFileChangeStatus::Completed
                    },
                },
            ));
            return;
        }
        if let Some(tool_call) = self.running_tool_calls.remove(tool_use_id) {
            let output = tool_result_text(content);
            if !output.is_empty() {
                projected.push(self.attributed(
                    &tool_call.owner,
                    ProviderEvent::ToolCallOutputDelta {
                        activity_id: tool_call.activity.clone(),
                        content: output,
                    },
                ));
            }
            projected.push(self.attributed(
                &tool_call.owner,
                ProviderEvent::ToolCallCompleted {
                    activity_id: tool_call.activity,
                    status: if is_error {
                        ProviderToolCallStatus::Failed
                    } else {
                        ProviderToolCallStatus::Completed
                    },
                    omitted_parts: omitted_result_parts(content),
                },
            ));
            return;
        }
        let Some(command) = self.running_commands.remove(tool_use_id) else {
            return;
        };
        let output = tool_result_text(content);
        if !output.is_empty() {
            projected.push(self.attributed(
                &command.owner,
                ProviderEvent::CommandOutputDelta {
                    activity_id: command.activity.clone(),
                    content: output,
                },
            ));
        }
        projected.push(self.attributed(
            &command.owner,
            ProviderEvent::CommandCompleted {
                activity_id: command.activity,
                status: if is_error {
                    ProviderCommandStatus::Failed
                } else {
                    ProviderCommandStatus::Completed
                },
                exit_status: None,
            },
        ));
    }

    /// Opens a conversation's thinking block and the first Reasoning block of its split.
    fn open_thinking(
        &mut self,
        conversation: &mut ConversationInFlight,
        index: u64,
        opening: Option<String>,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let activity = next_reasoning_activity(&mut self.reasoning_blocks);
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: activity.clone(),
        });
        let mut thinking = OpenThinking {
            index,
            activity,
            splitter: ThinkingSplitter::default(),
        };
        if let Some(opening) = opening.filter(|opening| !opening.is_empty()) {
            let split = thinking.splitter.push(&opening);
            Self::project_thinking_split(
                &mut self.reasoning_blocks,
                &mut thinking,
                split,
                projected,
            );
        }
        conversation.open_thinking = Some(thinking);
    }

    /// Settles a conversation's open thinking block, releasing whatever its split still withholds.
    fn settle_open_thinking(
        &mut self,
        conversation: &mut ConversationInFlight,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let Some(mut thinking) = conversation.open_thinking.take() else {
            return;
        };
        let split = thinking.splitter.finish();
        Self::project_thinking_split(&mut self.reasoning_blocks, &mut thinking, split, projected);
        projected.push(ProviderEvent::ReasoningCompleted {
            activity_id: thinking.activity,
        });
    }

    /// Lowers a thinking split's resolutions onto the Reasoning block it is filling: a heading
    /// break settles the block and starts the next one under the heading's title.
    fn project_thinking_split(
        reasoning_blocks: &mut u64,
        thinking: &mut OpenThinking,
        split: Vec<ThinkingEvent>,
        projected: &mut Vec<ProviderEvent>,
    ) {
        for event in split {
            match event {
                ThinkingEvent::Title(title) => {
                    projected.push(ProviderEvent::ReasoningTitleChanged {
                        activity_id: thinking.activity.clone(),
                        title,
                    })
                }
                ThinkingEvent::Content(content) => projected.push(ProviderEvent::ReasoningDelta {
                    activity_id: thinking.activity.clone(),
                    content,
                }),
                ThinkingEvent::Break { title } => {
                    projected.push(ProviderEvent::ReasoningCompleted {
                        activity_id: thinking.activity.clone(),
                    });
                    thinking.activity = next_reasoning_activity(reasoning_blocks);
                    projected.push(ProviderEvent::ReasoningStarted {
                        activity_id: thinking.activity.clone(),
                    });
                    projected.push(ProviderEvent::ReasoningTitleChanged {
                        activity_id: thinking.activity.clone(),
                        title,
                    });
                }
            }
        }
    }

    /// The reporting lifetime a result's cumulative Cost belongs to: the conversation the CLI keeps
    /// the running total for. The CLI saves that total into the conversation when a process exits
    /// and a process resuming it carries on from there, so every process a Session spawns under
    /// one conversation — across Selection changes and Suru restarts alike — continues one total,
    /// and reporting each process separately would count what came before it again. A `/clear`
    /// moves the process onto a new conversation whose total starts from zero, which is a new
    /// lifetime. A result naming no conversation belongs to the one the Session is filed under.
    fn reporting_lifetime(&self, result: &ResultMessage) -> String {
        let conversation = result
            .session_id
            .as_deref()
            .filter(|conversation| !conversation.is_empty())
            .unwrap_or(&self.resume.session_id);
        format!("claude:{conversation}")
    }

    fn project_result(
        &mut self,
        message: Value,
    ) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        let result: ResultMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed result message: {error}"
            ))
        })?;
        if result
            .uuid
            .as_ref()
            .is_some_and(|uuid| !self.seen_results.insert(uuid.clone()))
        {
            return Ok(Vec::new());
        }
        let mut projected = Vec::new();
        // A result is a boundary of the loop's own conversation alone: its subagents stream on
        // past it (ADR 0015). A result while the loop's blocks are still streaming is the CLI
        // failing mid-stream; what did stream stays in the Transcript, settled.
        let mut owning = self
            .conversations
            .remove(&OWNING_CONVERSATION)
            .unwrap_or_default();
        self.settle_open_thinking(&mut owning, &mut projected);
        if owning.open_text_block.take().is_some() {
            projected.push(ProviderEvent::AgentMessageCompleted);
        }
        // A command of the loop's own whose tool result never came back has no outcome to match,
        // so it settles as failed rather than holding the Turn open. A subagent's commands are
        // owed nothing by this result and keep running.
        let unanswered = self
            .running_commands
            .iter()
            .filter(|(_, command)| command.owner.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in unanswered {
            let command = self
                .running_commands
                .remove(&id)
                .expect("an unanswered command was just listed from the table");
            projected.push(ProviderEvent::CommandCompleted {
                activity_id: command.activity,
                status: ProviderCommandStatus::Failed,
                exit_status: None,
            });
        }
        // So does an edit of the loop's own.
        let unanswered = self
            .running_file_changes
            .iter()
            .filter(|(_, file_change)| file_change.owner.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in unanswered {
            let file_change = self
                .running_file_changes
                .remove(&id)
                .expect("an unanswered File Change was just listed from the table");
            projected.push(ProviderEvent::FileChangeCompleted {
                activity_id: file_change.activity,
                status: ProviderFileChangeStatus::Failed,
            });
        }
        // A Tool Call of the loop's own settles the same way, and a subagent's keeps running.
        let unanswered = self
            .running_tool_calls
            .iter()
            .filter(|(_, tool_call)| tool_call.owner.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in unanswered {
            let tool_call = self
                .running_tool_calls
                .remove(&id)
                .expect("an unanswered Tool Call was just listed from the table");
            projected.push(ProviderEvent::ToolCallCompleted {
                activity_id: tool_call.activity,
                status: ProviderToolCallStatus::Failed,
                omitted_parts: 0,
            });
        }
        // The result ends a stretch, not the wire's account of the loop: that the loop's own
        // conversation streams is what keeps its snapshots passed over, so the flag outlives the
        // blocks the settle just closed.
        self.conversations.insert(
            OWNING_CONVERSATION,
            ConversationInFlight {
                streamed: owning.streamed,
                ..ConversationInFlight::default()
            },
        );
        let reporting_lifetime = self.reporting_lifetime(&result);
        let reported_cost = result
            .total_cost_usd
            .and_then(Cost::from_usd)
            .filter(|cost| {
                self.latest_reported_costs
                    .get(&reporting_lifetime)
                    .is_none_or(|latest| cost.nano_usd() >= latest.nano_usd())
            });
        if let Some(cost) = reported_cost {
            self.latest_reported_costs
                .insert(reporting_lifetime.clone(), cost);
        }
        if result.usage.is_some() || reported_cost.is_some() || self.turn_metering.is_some() {
            let usage = result
                .usage
                .as_ref()
                .map_or_else(Usage::default, result_usage);
            if let Some(metering) = self.turn_metering.as_mut() {
                metering.add_usage_with_cumulative_cost(usage, reported_cost);
            } else {
                self.turn_metering = Some(ReportedTurnMetering::new(usage, reported_cost));
            }
            let metering = self
                .turn_metering
                .as_ref()
                .expect("Claude Turn metering was just initialized");
            projected.push(metering.subtree_event(&reporting_lifetime));
        }
        let turn_settled;
        if was_interrupted(&result) {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnInterrupted);
            turn_settled = true;
        } else if result.subtype == "success" && !result.is_error {
            // A steered Turn may be answered stretch by stretch: a steer the loop took up at a
            // tool round is answered by the loop's own result, but one still queued when the loop
            // ended begins a loop of its own, and only that loop's result Settles the Turn that
            // holds them all. A result no Turn waited on at all ends a stretch the loop ran on its
            // own — waking to deliver a Subagent's outcome — and completing it is what settles the
            // Continuation that output began, or nothing where none is open.
            if self.turn.result_settles_turn() || !self.turn.is_running() {
                projected.push(ProviderEvent::TurnCompleted);
                turn_settled = true;
            } else {
                turn_settled = false;
            }
        } else {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnFailed {
                message: super::result_failure_message("Turn", &result),
            });
            turn_settled = true;
        }
        if turn_settled {
            self.turn_metering = None;
        }
        Ok(projected.into_iter().map(Into::into).collect())
    }
}

/// What a resume Delegation asks, as its row reads it. Only a SendMessage carries a description of
/// its own; a start naming anything else is described by the task itself.
fn resume_description(kind: DelegationKind) -> Option<String> {
    match kind {
        DelegationKind::Resume { description, .. } => description,
        DelegationKind::Spawn { .. } => None,
    }
}

/// One text field of a delegating tool's input, where it holds text with something to read. A
/// field in any other shape — SendMessage's structured protocol messages among them — holds no
/// Delegation text.
fn input_text(input: &Value, field: &str) -> Option<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

/// How a SendMessage's input describes the resume it starts: the `summary` the loop gave of its
/// message, or where it gave none, the message's own first line. A message in some shape other
/// than text describes nothing.
fn send_message_description(input: &Value) -> Option<String> {
    let text = |field: &str| input.get(field).and_then(Value::as_str);
    text("summary")
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .or_else(|| {
            text("message")?
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map(str::to_owned)
}

/// Reads an integer token count without letting a malformed negative,
/// fractional, non-finite, or unrepresentable Provider number become a
/// fabricated zero.
/// A result's usage in Suru's disjoint parts. The CLI counts thinking inside its output tokens, so
/// the thinking it breaks out is taken back out of the output; a breakdown it does not give leaves
/// the thinking unknown and the output as stated.
fn result_usage(usage: &ResultUsage) -> Usage {
    let output = reported_token_count(usage.output_tokens);
    let reasoning = usage
        .output_tokens_details
        .as_ref()
        .and_then(|details| details.thinking_tokens)
        .map(|thinking| reported_token_count(Some(thinking)));
    let (output_tokens, reasoning_tokens) = match reasoning {
        None => (output, None),
        Some(reasoning) => (
            output
                .zip(reasoning)
                .and_then(|(output, reasoning)| output.checked_sub(reasoning)),
            reasoning,
        ),
    };
    Usage {
        fresh_input_tokens: reported_token_count(usage.input_tokens),
        cache_read_tokens: reported_token_count(usage.cache_read_input_tokens),
        cache_write_tokens: reported_token_count(usage.cache_creation_input_tokens),
        output_tokens,
        reasoning_tokens,
        native_meter: None,
        model_context_window: None,
    }
}

fn reported_token_count(value: Option<f64>) -> Option<u64> {
    value
        .filter(|value| {
            value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64
        })
        .map(|value| value as u64)
}

/// Whether a result is a Turn the user stopped. The CLI reports an interrupted loop as an errored
/// `error_during_execution` whose only error is a CLI-internal diagnostic, so the abort is legible
/// only in `terminal_reason` — verified against 2.1.280, whose interrupted result reads
/// `aborted_streaming` or `aborted_tools`. Which of the two abort reasons the CLI gives says only
/// where its loop was when the interrupt landed: streaming an answer, or waiting on a tool it had
/// already called.
fn was_interrupted(result: &ResultMessage) -> bool {
    matches!(
        result.terminal_reason.as_deref(),
        Some("aborted_streaming" | "aborted_tools")
    )
}

/// The next Reasoning block's identity. Blocks are numbered across the Session — one namespace
/// for every conversation, since each block lands in a Session of its own anyway — apart from the
/// tool-use ids commands are named by.
fn next_reasoning_activity(reasoning_blocks: &mut u64) -> ProviderActivityId {
    let activity = ProviderActivityId::new(format!("reasoning:{reasoning_blocks}"));
    *reasoning_blocks += 1;
    activity
}

/// How many parts of a tool result are not text — images, documents, resources — and so are left
/// out of its output. A bare string is text entire.
fn omitted_result_parts(content: &Value) -> u32 {
    let Value::Array(blocks) = content else {
        return 0;
    };
    let omitted = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
        .count();
    u32::try_from(omitted).unwrap_or(u32::MAX)
}

/// The text of a tool result, in either shape the wire carries one: a bare string, or a list of
/// blocks whose text entries are the output.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::{
        ClaudeProjection, ClaudeResumeState, Cost, TurnInFlight, gated_tool_activity_id,
        send_message_description,
    };
    use crate::provider::{
        AttributedProviderEvent, ProviderActivityId, ProviderEvent, ProviderEventAttribution,
        ProviderSubagentId, ProviderWatchId, ProviderWatchOutcome,
    };

    /// A projection whose CLI works in `/work`, restoring `resume`.
    fn projection_resuming(resume: ClaudeResumeState) -> ClaudeProjection {
        ClaudeProjection::new(TurnInFlight::new(), resume, PathBuf::from("/work"))
    }

    /// The loop's own conversation running SendMessage `tool` to the agent `task`, and the CLI
    /// starting the agent's task again for it.
    fn send_message_resuming(tool: &str, task: &str) -> [Value; 3] {
        [
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": tool, "name": "SendMessage", "input": {}},
                },
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "stream_event",
                "event": {"type": "content_block_stop", "index": 0},
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": task,
                "tool_use_id": tool,
                "description": "The spawn's description",
                "task_type": "local_agent",
                "subagent_type": "general-purpose",
            }),
        ]
    }

    fn said_under(conversation: &str, text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": conversation,
        })
    }

    fn project(
        projection: &mut ClaudeProjection,
        messages: &[Value],
    ) -> Vec<AttributedProviderEvent> {
        messages
            .iter()
            .flat_map(|message| {
                projection
                    .project(message.clone())
                    .expect("the message projects")
            })
            .collect()
    }

    /// Where the agent Message `text` lands: under the Subagent the attribution names.
    fn attribution_of(events: &[AttributedProviderEvent], text: &str) -> ProviderEventAttribution {
        events
            .iter()
            .find(|event| {
                matches!(&event.event, ProviderEvent::AgentMessageDelta { content } if content == text)
            })
            .unwrap_or_else(|| panic!("{text} projects as an agent Message: {events:?}"))
            .attribution
            .clone()
    }

    fn subagent(task: &str) -> ProviderEventAttribution {
        ProviderEventAttribution::Subagent(ProviderSubagentId::new(task))
    }

    fn task_started(task: &str, task_type: &str, description: Option<&str>) -> Value {
        json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task,
            "task_type": task_type,
            "description": description,
        })
    }

    fn task_notification(task: &str, status: &str, summary: Option<&str>) -> Value {
        json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task,
            "status": status,
            "summary": summary,
        })
    }

    fn fresh_projection() -> ClaudeProjection {
        projection_resuming(ClaudeResumeState::default())
    }

    fn owning(event: ProviderEvent) -> AttributedProviderEvent {
        event.into()
    }

    #[test]
    fn a_background_shell_or_monitor_starts_a_watch_and_nothing_else_does() {
        for (task_type, watched) in [
            ("local_bash", true),
            ("monitor_mcp", true),
            ("monitor_ws", true),
            ("local_workflow", false),
            ("remote_agent", false),
            ("in_process_teammate", false),
            ("dream", false),
            ("a_task_type_this_build_has_never_heard_of", false),
        ] {
            let mut projection = fresh_projection();
            let events = project(
                &mut projection,
                &[task_started("task-1", task_type, Some("Background work"))],
            );
            let expected = if watched {
                vec![owning(ProviderEvent::WatchStarted {
                    watch_id: ProviderWatchId::new("task-1"),
                    description: "Background work".to_owned(),
                })]
            } else {
                Vec::new()
            };
            assert_eq!(events, expected, "a `{task_type}` task");
            assert_eq!(
                projection.turn.live_tasks(),
                ["task-1"],
                "a `{task_type}` task still joins the roster an interrupt stops"
            );
        }
    }

    #[test]
    fn a_watch_the_cli_gives_no_description_is_described_by_the_command_that_started_it() {
        let mut projection = fresh_projection();
        let events = project(
            &mut projection,
            &[
                json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Bash",
                            "input": {"command": "cargo test", "run_in_background": true},
                        },
                    },
                    "parent_tool_use_id": null,
                }),
                json!({
                    "type": "stream_event",
                    "event": {"type": "content_block_stop", "index": 0},
                    "parent_tool_use_id": null,
                }),
                json!({
                    "type": "system",
                    "subtype": "task_started",
                    "task_id": "task-1",
                    "tool_use_id": "toolu_1",
                    "task_type": "local_bash",
                }),
                task_started("task-2", "local_bash", None),
            ],
        );
        let described = events
            .iter()
            .filter_map(|event| match &event.event {
                ProviderEvent::WatchStarted { description, .. } => Some(description.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            ["cargo test", "task-2"],
            "a Watch with nothing else to go on is named by its task"
        );
    }

    #[test]
    fn a_watchs_notification_settles_it_with_the_outcome_and_summary_the_cli_gives() {
        for (status, outcome, woke_agent) in [
            ("completed", ProviderWatchOutcome::Completed, true),
            ("failed", ProviderWatchOutcome::Failed, true),
            ("stopped", ProviderWatchOutcome::Stopped, false),
            ("killed", ProviderWatchOutcome::Failed, true),
        ] {
            let mut projection = fresh_projection();
            let events = project(
                &mut projection,
                &[
                    task_started("task-1", "local_bash", Some("cargo test")),
                    task_notification("task-1", status, Some("cargo test finished")),
                    task_notification("task-1", status, Some("a repeated notification")),
                ],
            );
            assert_eq!(
                events[1..],
                [owning(ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("task-1"),
                    outcome,
                    summary: Some("cargo test finished".to_owned()),
                    woke_agent,
                })],
                "a `{status}` notification settles the Watch once, and only a stop wakes nothing"
            );
            assert!(projection.turn.live_tasks().is_empty());
        }
    }

    #[test]
    fn the_watches_a_process_was_running_settle_as_lost_when_it_ends() {
        let mut projection = fresh_projection();
        project(
            &mut projection,
            &[
                task_started("task-1", "local_bash", Some("cargo test")),
                task_started("task-2", "monitor_mcp", Some("Watch the queue")),
                task_notification("task-2", "completed", None),
            ],
        );

        let lost = projection.project_process_ended();

        assert_eq!(
            lost,
            [owning(ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new("task-1"),
                outcome: ProviderWatchOutcome::Lost,
                summary: None,
                woke_agent: false,
            })],
            "only the Watch still live is lost, and its loss wakes nothing"
        );
        assert!(
            projection.turn.live_tasks().is_empty(),
            "the roster starts empty again for the next process"
        );
        assert!(
            projection.project_process_ended().is_empty(),
            "a Watch is lost once"
        );
    }

    /// A `local_bash` start saying whether the CLI backgrounded it, named by its tool use as a
    /// live CLI's is.
    fn shell_started(task: &str, is_backgrounded: bool) -> Value {
        json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task,
            "tool_use_id": "toolu_1",
            "task_type": "local_bash",
            "description": "sleep 12 && echo hello-slow",
            "is_backgrounded": is_backgrounded,
        })
    }

    fn slow_shell_started_as_a_watch(task: &str) -> AttributedProviderEvent {
        owning(ProviderEvent::WatchStarted {
            watch_id: ProviderWatchId::new(task),
            description: "sleep 12 && echo hello-slow".to_owned(),
        })
    }

    #[test]
    fn a_foreground_shell_joins_the_roster_but_starts_and_settles_no_watch() {
        let mut projection = fresh_projection();
        let started = project(&mut projection, &[shell_started("task-1", false)]);
        assert!(started.is_empty(), "{started:?}");
        assert_eq!(
            projection.turn.live_tasks(),
            ["task-1"],
            "an interrupt still stops a foreground shell"
        );

        let settled = project(
            &mut projection,
            &[task_notification(
                "task-1",
                "completed",
                Some("sleep 12 && echo hello-slow"),
            )],
        );

        assert!(
            settled.is_empty(),
            "a foreground shell's notification wakes nothing: {settled:?}"
        );
        assert!(projection.turn.live_tasks().is_empty());
    }

    #[test]
    fn a_shell_the_cli_backgrounds_or_does_not_say_about_starts_a_watch() {
        let unsaid = json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "task-1",
            "tool_use_id": "toolu_1",
            "task_type": "local_bash",
            "description": "sleep 12 && echo hello-slow",
        });
        for start in [shell_started("task-1", true), unsaid] {
            let mut projection = fresh_projection();
            let events = project(&mut projection, std::slice::from_ref(&start));
            assert_eq!(events, [slow_shell_started_as_a_watch("task-1")], "{start}");
        }
    }

    #[test]
    fn a_foreground_shell_moved_to_the_background_becomes_a_watch_its_notification_settles() {
        let mut projection = fresh_projection();
        let events = project(
            &mut projection,
            &[
                shell_started("task-1", false),
                json!({
                    "type": "system",
                    "subtype": "task_updated",
                    "task_id": "task-1",
                    "patch": {"is_backgrounded": true},
                }),
                task_notification("task-1", "completed", Some("Background command completed")),
            ],
        );

        assert_eq!(
            events,
            [
                slow_shell_started_as_a_watch("task-1"),
                owning(ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("task-1"),
                    outcome: ProviderWatchOutcome::Completed,
                    summary: Some("Background command completed".to_owned()),
                    woke_agent: true,
                }),
            ],
            "the move to the background starts the Watch the start would have"
        );
    }

    #[test]
    fn a_foreground_shell_running_when_the_process_ends_is_no_lost_watch() {
        let mut projection = fresh_projection();
        project(&mut projection, &[shell_started("task-1", false)]);

        assert!(projection.project_process_ended().is_empty());
        assert!(projection.turn.live_tasks().is_empty());
        let moved = project(
            &mut projection,
            &[json!({
                "type": "system",
                "subtype": "task_updated",
                "task_id": "task-1",
                "patch": {"is_backgrounded": true},
            })],
        );
        assert!(
            moved.is_empty(),
            "a shell that died with its process has nothing left to promote: {moved:?}"
        );
    }

    /// A background agent spawned from the loop, whose conversation then runs a Bash tool use
    /// that the CLI backgrounds as task `task-shell`, telling Suru a subagent owns it.
    fn subagent_backgrounds_a_shell() -> Vec<Value> {
        vec![
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": "agent_1", "name": "Agent", "input": {}},
                },
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "stream_event",
                "event": {"type": "content_block_stop", "index": 0},
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-task",
                "tool_use_id": "agent_1",
                "task_type": "local_agent",
                "description": "Run the suite",
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_sub",
                        "name": "Bash",
                        "input": {"command": "cargo test", "run_in_background": true},
                    }],
                },
                "parent_tool_use_id": "agent_1",
            }),
            json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "task-shell",
                "tool_use_id": "toolu_sub",
                "task_type": "local_bash",
                "description": "cargo test",
                "owned_by_subagent": true,
            }),
        ]
    }

    fn watch_events(events: &[AttributedProviderEvent]) -> Vec<AttributedProviderEvent> {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.event,
                    ProviderEvent::WatchStarted { .. } | ProviderEvent::WatchSettled { .. }
                )
            })
            .cloned()
            .collect()
    }

    #[test]
    fn a_watch_a_subagent_launched_is_the_subagents_and_stays_so_after_the_subagent_settles() {
        let mut projection = fresh_projection();
        let started = project(&mut projection, &subagent_backgrounds_a_shell());
        assert_eq!(
            watch_events(&started),
            [AttributedProviderEvent {
                attribution: subagent("agent-task"),
                event: ProviderEvent::WatchStarted {
                    watch_id: ProviderWatchId::new("task-shell"),
                    description: "cargo test".to_owned(),
                },
            }],
            "the conversation that ran the launching tool use owns the Watch"
        );

        let settled = project(
            &mut projection,
            &[
                task_notification("agent-task", "completed", Some("Started the suite")),
                task_notification("task-shell", "completed", Some("cargo test passed")),
            ],
        );
        assert_eq!(
            watch_events(&settled),
            [AttributedProviderEvent {
                attribution: subagent("agent-task"),
                event: ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("task-shell"),
                    outcome: ProviderWatchOutcome::Completed,
                    summary: Some("cargo test passed".to_owned()),
                    woke_agent: true,
                },
            }],
            "a background Subagent's Watch outlives it and settles where it started"
        );
    }

    #[test]
    fn a_subagents_watch_lost_with_the_process_is_lost_under_the_subagent() {
        let mut projection = fresh_projection();
        project(&mut projection, &subagent_backgrounds_a_shell());

        assert_eq!(
            projection.project_process_ended(),
            [AttributedProviderEvent {
                attribution: subagent("agent-task"),
                event: ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("task-shell"),
                    outcome: ProviderWatchOutcome::Lost,
                    summary: None,
                    woke_agent: false,
                },
            }]
        );
    }

    #[test]
    fn a_watch_said_to_be_a_subagents_that_no_subagent_launched_stays_with_the_loop() {
        let mut projection = fresh_projection();
        let events = project(
            &mut projection,
            &[json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "task-shell",
                "tool_use_id": "toolu_unseen",
                "task_type": "local_bash",
                "description": "cargo test",
                "owned_by_subagent": true,
            })],
        );
        assert_eq!(
            events,
            [owning(ProviderEvent::WatchStarted {
                watch_id: ProviderWatchId::new("task-shell"),
                description: "cargo test".to_owned(),
            })],
            "a Watch no conversation can be found for keeps the owning Session Monitoring"
        );
    }

    #[test]
    fn a_spawn_records_the_conversation_its_agent_rides_under_in_the_resume_state() {
        let mut projection = projection_resuming(ClaudeResumeState {
            session_id: "provider-session".to_owned(),
            ..Default::default()
        });

        let events = project(
            &mut projection,
            &[json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "a2046dbbe8ecd4a5c",
                "tool_use_id": "agent_1",
                "description": "Say hello",
                "task_type": "local_agent",
            })],
        );

        let revised = events
            .iter()
            .find_map(|event| match &event.event {
                ProviderEvent::ResumeStateChanged { resume_state } => Some(resume_state.payload()),
                _ => None,
            })
            .expect("the spawn revises the Resume State");
        assert_eq!(
            *revised,
            json!({
                "session_id": "provider-session",
                "agents": {"a2046dbbe8ecd4a5c": "agent_1"},
            })
        );
    }

    #[test]
    fn an_agent_the_resume_state_recorded_resumes_with_its_conversation_routed() {
        let mut projection = projection_resuming(ClaudeResumeState {
            session_id: "provider-session".to_owned(),
            agents: [("a2046dbbe8ecd4a5c".to_owned(), "agent_1".to_owned())].into(),
        });

        let mut messages = send_message_resuming("send_1", "a2046dbbe8ecd4a5c").to_vec();
        messages.push(said_under("agent_1", "GOODBYE"));
        let events = project(&mut projection, &messages);

        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                ProviderEvent::SubagentResumed { subagent_id, .. }
                    if subagent_id.as_str() == "a2046dbbe8ecd4a5c"
            )),
            "the start resumes the recorded agent: {events:?}"
        );
        assert_eq!(
            attribution_of(&events, "GOODBYE"),
            subagent("a2046dbbe8ecd4a5c")
        );
    }

    #[test]
    fn a_settled_agent_started_again_naming_no_tool_use_is_woken_rather_than_resumed() {
        let mut projection = projection_resuming(ClaudeResumeState {
            session_id: "provider-session".to_owned(),
            agents: [("agent-task".to_owned(), "agent_1".to_owned())].into(),
        });

        let mut events = project(
            &mut projection,
            &[json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "agent-task",
                "description": "Run the suite",
                "task_type": "local_agent",
                "subagent_type": "general-purpose",
            })],
        );
        assert_eq!(
            projection.turn.subagent_task("agent-task").as_deref(),
            Some("agent-task"),
            "the woken agent is back on the roster, so stopping its Subagent still finds it"
        );
        events.extend(project(
            &mut projection,
            &[
                said_under("agent_1", "The suite passed."),
                task_notification("agent-task", "completed", Some("Reported the suite")),
            ],
        ));

        let lifecycle = events
            .iter()
            .filter(|event| {
                !matches!(
                    event.event,
                    ProviderEvent::AgentMessageStarted
                        | ProviderEvent::AgentMessageDelta { .. }
                        | ProviderEvent::AgentMessageCompleted
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            [
                owning(ProviderEvent::SubagentWoken {
                    subagent_id: ProviderSubagentId::new("agent-task"),
                }),
                owning(ProviderEvent::SubagentCompleted {
                    subagent_id: ProviderSubagentId::new("agent-task"),
                    status: crate::provider::ProviderSubagentStatus::Completed,
                }),
            ],
            "nothing delegated the stretch, so it is a wake and no resume, and it settles as \
             any stretch does"
        );
        assert_eq!(
            attribution_of(&events, "The suite passed."),
            subagent("agent-task"),
            "the woken agent's conversation rides under its spawn as before"
        );
    }

    #[test]
    fn a_lone_agent_resumed_with_no_record_claims_the_conversation_nothing_else_has() {
        let mut projection = projection_resuming(ClaudeResumeState::default());

        let mut messages = send_message_resuming("send_1", "a0ld").to_vec();
        messages.push(said_under("agent_before", "Picking up."));
        let events = project(&mut projection, &messages);

        assert_eq!(attribution_of(&events, "Picking up."), subagent("a0ld"));
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                ProviderEvent::ResumeStateChanged { resume_state }
                    if resume_state.payload()["agents"]["a0ld"] == "agent_before"
            )),
            "the claim is recorded so a later restart need not claim it again: {events:?}"
        );
    }

    #[test]
    fn two_agents_resumed_with_no_record_claim_no_conversation() {
        let mut projection = projection_resuming(ClaudeResumeState::default());

        let mut messages = send_message_resuming("send_1", "a0ld").to_vec();
        messages.extend(send_message_resuming("send_2", "b0ld"));
        messages.push(said_under("agent_before", "Whose is this?"));
        let events = project(&mut projection, &messages);

        assert!(
            ![subagent("a0ld"), subagent("b0ld")]
                .contains(&attribution_of(&events, "Whose is this?")),
            "nothing tells whose the conversation is, so neither agent claims it: {events:?}"
        );
    }

    /// The loop's own conversation spawning the background agent `task` through the Agent tool
    /// `tool`, as snapshots, and the CLI starting its task.
    fn agent_spawned(tool: &str, task: &str, conversation: Option<&str>) -> [Value; 2] {
        [
            json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": tool,
                    "name": "Agent",
                    "input": {"description": "Run the sleeps", "prompt": "Sleep three times."},
                }]},
                "parent_tool_use_id": conversation,
            }),
            json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": task,
                "tool_use_id": tool,
                "description": "Run the sleeps",
                "task_type": "local_agent",
                "subagent_type": "general-purpose",
            }),
        ]
    }

    /// The conversation `conversation` — the loop's own for `None` — running SendMessage `tool` to
    /// the agent `task` with `message`, and the CLI answering it with `result`.
    fn send_message_answered(
        conversation: Option<&str>,
        tool: &str,
        task: &str,
        message: &str,
        result: Value,
    ) -> [Value; 2] {
        [
            json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": tool,
                    "name": "SendMessage",
                    "input": {"to": task, "message": message, "summary": format!("steer {message}")},
                }]},
                "parent_tool_use_id": conversation,
            }),
            json!({
                "type": "user",
                "message": {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool,
                    "content": [{"type": "text", "text": result.to_string()}],
                }]},
                "parent_tool_use_id": conversation,
            }),
        ]
    }

    /// The same SendMessage, which the CLI queued because `task` is still working, as the live
    /// 2.1.280 CLI answers it.
    fn send_message_queued(
        conversation: Option<&str>,
        tool: &str,
        task: &str,
        message: &str,
    ) -> [Value; 2] {
        send_message_answered(
            conversation,
            tool,
            task,
            message,
            json!({
                "success": true,
                "message": format!("Message queued for delivery to {task} at its next tool round."),
                "pin": {"id": task, "name": task, "ref": "5283b6"},
            }),
        )
    }

    /// A tool round in the subagent conversation `conversation`: the result of its tool use
    /// `tool` echoed back into it.
    fn tool_round(conversation: &str, tool: &str) -> Value {
        json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": tool,
                "content": "slept",
                "is_error": false,
            }]},
            "parent_tool_use_id": conversation,
        })
    }

    /// One block of the assistant message `id` in the subagent conversation `conversation`.
    fn block_of(conversation: &str, id: &str, text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"id": id, "role": "assistant", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": conversation,
        })
    }

    /// Where each event stands in `events`, told apart as the steers it carries — each with the
    /// attribution naming its sender, the agent it reached, and what it said — and the agent
    /// Messages around them.
    fn steers_and_messages(events: &[AttributedProviderEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match &event.event {
                ProviderEvent::SubagentSteered {
                    subagent_id,
                    delegation,
                } => Some(format!(
                    "steer {delegation:?} to {} from {:?}",
                    subagent_id.as_str(),
                    event.attribution
                )),
                ProviderEvent::AgentMessageDelta { content } => Some(format!("said {content:?}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn steers_queued_before_one_tool_round_stand_in_order_before_the_next_assistant_message() {
        let mut projection = fresh_projection();
        let mut messages = agent_spawned("agent_1", "agent-task", None).to_vec();
        messages.push(block_of("agent_1", "msg_1", "Sleeping."));
        messages.extend(send_message_queued(None, "send_1", "agent-task", "First"));
        messages.extend(send_message_queued(None, "send_2", "agent-task", "Second"));
        let queued = project(&mut projection, &messages);
        assert_eq!(
            steers_and_messages(&queued),
            [r#"said "Sleeping.""#],
            "nothing stands for a steer while it is only queued"
        );

        let events = project(
            &mut projection,
            &[
                block_of("agent_1", "msg_1", "Still the same message."),
                tool_round("agent_1", "toolu_sleep"),
                block_of("agent_1", "msg_2", "Read both."),
            ],
        );

        assert_eq!(
            steers_and_messages(&events),
            [
                r#"said "Still the same message.""#.to_owned(),
                r#"steer "First" to agent-task from OwningSession"#.to_owned(),
                r#"steer "Second" to agent-task from OwningSession"#.to_owned(),
                r#"said "Read both.""#.to_owned(),
            ],
            "the message the agent was writing when they were queued is not where it read them; \
             both stand, in the order they were sent, before the first message after its tool round"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event.event,
                ProviderEvent::SubagentResumed { .. } | ProviderEvent::SubagentUpdated { .. }
            )),
            "a steer begins no Turn and revises no row: {events:?}"
        );
        assert!(
            steers_and_messages(&project(
                &mut projection,
                &[block_of("agent_1", "msg_3", "Done.")]
            ))
            .iter()
            .all(|event| !event.starts_with("steer")),
            "a delivered steer stands once"
        );
    }

    #[test]
    fn a_steer_sent_by_a_sibling_is_attributed_to_the_sibling() {
        let mut projection = fresh_projection();
        let mut messages = agent_spawned("agent_writer", "writer-task", None).to_vec();
        messages.extend(agent_spawned("agent_reviewer", "reviewer-task", None));
        messages.extend(send_message_queued(
            Some("agent_reviewer"),
            "send_review",
            "writer-task",
            "Tighten the second paragraph.",
        ));
        messages.push(tool_round("agent_writer", "toolu_draft"));
        messages.push(block_of("agent_writer", "msg_w", "Tightening."));
        let events = project(&mut projection, &messages);

        assert_eq!(
            steers_and_messages(&events),
            [
                r#"steer "Tighten the second paragraph." to writer-task from Subagent(ProviderSubagentId("reviewer-task"))"#,
                r#"said "Tightening.""#,
            ]
        );
    }

    #[test]
    fn a_refused_send_message_creates_nothing() {
        let mut projection = fresh_projection();
        let mut messages = agent_spawned("agent_1", "agent-task", None).to_vec();
        messages.extend(send_message_answered(
            None,
            "send_1",
            "slowpoke",
            "Hurry up.",
            json!({
                "success": false,
                "message": "No agent named 'slowpoke' is reachable.",
            }),
        ));
        messages.push(block_of("agent_1", "msg_1", "Sleeping."));
        messages.push(task_notification("agent-task", "completed", None));
        let events = project(&mut projection, &messages);

        assert_eq!(steers_and_messages(&events), [r#"said "Sleeping.""#]);
        assert_eq!(
            projection.delegation_tools.len(),
            0,
            "the refused SendMessage delegates nothing a later start could take up"
        );
    }

    #[test]
    fn a_steer_pending_when_its_subagent_is_stopped_never_appears() {
        for stop in ["acknowledged", "notified"] {
            let mut projection = fresh_projection();
            let mut messages = agent_spawned("agent_1", "agent-task", None).to_vec();
            messages.extend(send_message_queued(
                None,
                "send_1",
                "agent-task",
                "Never read.",
            ));
            messages.push(tool_round("agent_1", "toolu_sleep"));
            let mut events = project(&mut projection, &messages);
            if stop == "acknowledged" {
                events.extend(projection.project_watches_stopped(&["agent-task".to_owned()]));
            } else {
                events.extend(project(
                    &mut projection,
                    &[task_notification("agent-task", "stopped", None)],
                ));
            }
            events.extend(project(
                &mut projection,
                &[
                    block_of("agent_1", "msg_trailing", "Trailing output."),
                    task_notification("agent-task", "stopped", None),
                    // The CLI starting the agent again naming its spawn is no restart for a steer
                    // it no longer holds.
                    json!({
                        "type": "system",
                        "subtype": "task_started",
                        "task_id": "agent-task",
                        "tool_use_id": "agent_1",
                        "description": "Run the sleeps",
                        "task_type": "local_agent",
                    }),
                    block_of("agent_1", "msg_after", "Back again."),
                ],
            ));

            assert!(
                events.iter().all(|event| !matches!(
                    &event.event,
                    ProviderEvent::SubagentSteered { .. }
                        | ProviderEvent::SubagentResumed {
                            delegation: Some(_),
                            ..
                        }
                )),
                "a steer its stopped ({stop}) agent never read stands nowhere: {events:?}"
            );
        }
    }

    #[test]
    fn a_steer_its_agent_settled_without_reading_is_discarded_when_another_resume_starts_it() {
        let mut projection = fresh_projection();
        let mut messages = agent_spawned("agent_1", "agent-task", None).to_vec();
        messages.extend(send_message_queued(
            None,
            "send_1",
            "agent-task",
            "Too late.",
        ));
        messages.push(block_of("agent_1", "msg_1", "Finished."));
        messages.push(task_notification("agent-task", "completed", None));
        messages.extend(send_message_resuming("send_2", "agent-task"));
        messages.push(tool_round("agent_1", "toolu_sleep"));
        messages.push(block_of("agent_1", "msg_2", "Resumed."));
        let events = project(&mut projection, &messages);

        assert!(
            events
                .iter()
                .all(|event| !matches!(event.event, ProviderEvent::SubagentSteered { .. })),
            "the steer was never delivered, so the resume's stretch never shows it: {events:?}"
        );
    }

    #[test]
    fn a_settled_agent_restarted_naming_its_spawn_is_resumed_by_the_steer_it_never_read() {
        let mut projection = fresh_projection();
        let mut messages = agent_spawned("agent_1", "agent-task", None).to_vec();
        messages.extend(agent_spawned("agent_sibling", "sibling-task", None));
        messages.extend(send_message_queued(
            Some("agent_sibling"),
            "send_1",
            "agent-task",
            "Say PINEAPPLE.\nThen stop.",
        ));
        messages.push(block_of("agent_1", "msg_1", "Finished without reading it."));
        messages.push(task_notification("agent-task", "completed", None));
        project(&mut projection, &messages);

        let events = project(
            &mut projection,
            &[
                json!({
                    "type": "system",
                    "subtype": "task_started",
                    "task_id": "agent-task",
                    "tool_use_id": "agent_1",
                    "description": "Run the sleeps",
                    "task_type": "local_agent",
                    "subagent_type": "general-purpose",
                    "prompt": "Say PINEAPPLE.\nThen stop.",
                }),
                block_of("agent_1", "msg_2", "PINEAPPLE"),
            ],
        );

        assert_eq!(
            events[0],
            AttributedProviderEvent {
                attribution: subagent("sibling-task"),
                event: ProviderEvent::SubagentResumed {
                    subagent_id: ProviderSubagentId::new("agent-task"),
                    name: "general-purpose".to_owned(),
                    description: "steer Say PINEAPPLE.\nThen stop.".to_owned(),
                    delegation: Some("Say PINEAPPLE.\nThen stop.".to_owned()),
                },
            },
            "the restart is a resume the steer opens, delegated by the sibling that sent it and \
             described by its SendMessage"
        );
        assert_eq!(
            steers_and_messages(&events),
            [r#"said "PINEAPPLE""#],
            "the steer opens the restarted stretch, and stands nowhere else"
        );
        assert_eq!(attribution_of(&events, "PINEAPPLE"), subagent("agent-task"));
    }

    #[test]
    fn a_send_message_is_described_by_its_summary() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": "Now say GOODBYE.\nNothing else.",
                "summary": "  Say goodbye ",
            })),
            Some("Say goodbye".to_owned())
        );
    }

    #[test]
    fn a_send_message_without_a_summary_is_described_by_its_messages_first_line() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": "\n  Tighten the second paragraph.\nThen stop.",
                "summary": "",
            })),
            Some("Tighten the second paragraph.".to_owned())
        );
    }

    #[test]
    fn a_send_message_carrying_no_text_describes_nothing() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": {"type": "shutdown_request"},
            })),
            None
        );
        assert_eq!(send_message_description(&serde_json::Value::Null), None);
    }

    /// A terminal result carrying `total_cost_usd` for the conversation `session_id`.
    fn result_costing(uuid: &str, session_id: &str, total_cost_usd: f64) -> Value {
        json!({
            "type": "result",
            "uuid": uuid,
            "subtype": "success",
            "is_error": false,
            "terminal_reason": "completed",
            "session_id": session_id,
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "total_cost_usd": total_cost_usd,
        })
    }

    /// The Cost the one Usage event among `events` reports, with the lifetime it is cumulative in.
    fn reported_cost(events: &[AttributedProviderEvent]) -> Option<(Cost, String)> {
        let [usage] = events
            .iter()
            .filter_map(|event| match &event.event {
                ProviderEvent::Usage { cost, .. } => Some(cost),
                _ => None,
            })
            .collect::<Vec<_>>()[..]
        else {
            panic!("a result reports its metering once: {events:?}");
        };
        usage.as_ref().map(|cost| {
            let crate::protocol::CostCoverage::SessionSubtree { reporting_lifetime } =
                cost.coverage()
            else {
                panic!("a Claude Cost is cumulative for its conversation: {cost:?}");
            };
            (cost.cost(), reporting_lifetime.clone())
        })
    }

    fn usd(usd: f64) -> Cost {
        Cost::from_usd(usd).expect("a fixture Cost is a valid USD figure")
    }

    #[test]
    fn a_resumed_conversation_continues_the_reporting_lifetime_it_was_filed_under() {
        // The CLI carries a conversation's running total into every process that resumes it, so a
        // Session restored after a restart reports into the lifetime it reported into before —
        // where a lifetime of its own would count the earlier spend a second time.
        let resume = ClaudeResumeState {
            session_id: "conversation-1".to_owned(),
            agents: Default::default(),
        };
        let mut before = projection_resuming(resume.clone());
        let mut after = projection_resuming(resume);

        let first = project(
            &mut before,
            &[result_costing("result-1", "conversation-1", 0.25)],
        );
        let resumed = project(
            &mut after,
            &[result_costing("result-2", "conversation-1", 0.40)],
        );

        assert_eq!(
            reported_cost(&first),
            Some((usd(0.25), "claude:conversation-1".to_owned()))
        );
        assert_eq!(
            reported_cost(&resumed),
            Some((usd(0.40), "claude:conversation-1".to_owned()))
        );
    }

    #[test]
    fn a_cleared_conversation_reports_its_fresh_total_in_a_lifetime_of_its_own() {
        let mut projection = fresh_projection();
        let before = project(
            &mut projection,
            &[result_costing("result-1", "conversation-1", 0.25)],
        );
        // `/clear` moves the process onto a new conversation whose running total starts again.
        let cleared = project(
            &mut projection,
            &[result_costing("result-2", "conversation-2", 0.0)],
        );
        let regressed = project(
            &mut projection,
            &[result_costing("result-3", "conversation-1", 0.10)],
        );

        assert_eq!(
            reported_cost(&before),
            Some((usd(0.25), "claude:conversation-1".to_owned()))
        );
        assert_eq!(
            reported_cost(&cleared),
            Some((usd(0.0), "claude:conversation-2".to_owned())),
            "a new conversation's total is no regression of the old one's"
        );
        assert_eq!(
            reported_cost(&regressed),
            None,
            "a total that went back within one conversation is still not believed"
        );
    }

    #[test]
    fn a_results_thinking_is_taken_out_of_its_output_as_reasoning() {
        let usage_of = |usage: Value| {
            let mut projection = fresh_projection();
            let events = project(
                &mut projection,
                &[json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "usage": usage,
                })],
            );
            events
                .into_iter()
                .find_map(|event| match event.event {
                    ProviderEvent::Usage { usage, .. } => Some(usage),
                    _ => None,
                })
                .expect("a result with usage reports it")
        };

        let broken_out = usage_of(json!({
            "input_tokens": 10,
            "output_tokens": 46,
            "output_tokens_details": {"thinking_tokens": 39},
        }));
        assert_eq!(
            (broken_out.output_tokens, broken_out.reasoning_tokens),
            (Some(7), Some(39))
        );

        let not_broken_out = usage_of(json!({"input_tokens": 10, "output_tokens": 46}));
        assert_eq!(
            (
                not_broken_out.output_tokens,
                not_broken_out.reasoning_tokens
            ),
            (Some(46), None),
            "output stands as stated when the CLI says nothing of its thinking"
        );

        let unreadable = usage_of(json!({
            "output_tokens": 5,
            "output_tokens_details": {"thinking_tokens": 39},
        }));
        assert_eq!(
            (unreadable.output_tokens, unreadable.reasoning_tokens),
            (None, Some(39)),
            "more thinking than output leaves the output unknown rather than wrapped"
        );
    }

    /// A tool use the loop streams, its input given whole as the block opens.
    fn streamed_tool_use(index: u64, tool: &str, name: &str, input: Value) -> [Value; 2] {
        [
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "tool_use", "id": tool, "name": name, "input": input},
                },
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "stream_event",
                "event": {"type": "content_block_stop", "index": index},
                "parent_tool_use_id": null,
            }),
        ]
    }

    /// A tool use a subagent's conversation restates whole.
    fn restated_tool_use(conversation: &str, tool: &str, name: &str, input: Value) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "tool_use", "id": tool, "name": name, "input": input,
            }]},
            "parent_tool_use_id": conversation,
        })
    }

    /// The result the CLI echoes back for `tool`, in the conversation that ran it.
    fn tool_result(conversation: Option<&str>, tool: &str) -> Value {
        json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": tool, "content": "done", "is_error": false,
            }]},
            "parent_tool_use_id": conversation,
        })
    }

    /// An Approval gating a use links to it by the identity `gated_tool_activity_id` names, so for
    /// every kind of row that identity must be the one the use's row opens under, and a use no row
    /// records must name none.
    #[test]
    fn an_approval_links_by_the_identity_its_uses_row_opens_under() {
        let uses = [
            ("Bash", json!({"command": "ls"}), Some("command")),
            ("Edit", json!({"file_path": "a.rs"}), Some("file_change")),
            (
                "MultiEdit",
                json!({"file_path": "b.rs"}),
                Some("file_change"),
            ),
            (
                "NotebookEdit",
                json!({"notebook_path": "c.ipynb"}),
                Some("file_change"),
            ),
            ("Write", json!({"file_path": "d.txt"}), Some("file_change")),
            ("Read", json!({"file_path": "e.txt"}), Some("tool_call")),
            (
                "mcp__linear__create_issue",
                json!({"title": "Link"}),
                Some("tool_call"),
            ),
            ("NovelTool", json!({}), Some("tool_call")),
            ("Agent", json!({"prompt": "Look."}), None),
            ("Task", json!({"prompt": "Look."}), None),
            (
                "SendMessage",
                json!({"to": "agent", "message": "More."}),
                None,
            ),
            ("AskUserQuestion", json!({"questions": []}), None),
            ("ToolSearch", json!({"query": "select:Read"}), None),
            (
                "mcp__suru__spawn_subagent",
                json!({"prompt": "Look."}),
                None,
            ),
        ];
        for (name, input, row) in uses {
            let opened = project(
                &mut fresh_projection(),
                &streamed_tool_use(0, "toolu_use", name, input),
            )
            .into_iter()
            .filter_map(|event| match event.event {
                ProviderEvent::CommandStarted { activity_id, .. }
                | ProviderEvent::FileChangeStarted { activity_id, .. }
                | ProviderEvent::ToolCallStarted { activity_id, .. } => Some(activity_id),
                _ => None,
            })
            .collect::<Vec<_>>();
            let expected = row
                .map(|row| ProviderActivityId::new(format!("{row}:toolu_use")))
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(opened, expected, "the row {name} opens");
            assert_eq!(
                gated_tool_activity_id(name, "toolu_use")
                    .into_iter()
                    .collect::<Vec<_>>(),
                expected,
                "the row an Approval of {name} links to"
            );
        }
    }

    /// The Broker's Tools reach Claude as MCP tool uses named `mcp__suru__*`, and 2.1.283 defers
    /// them behind a native `ToolSearch` the Agent calls before its first use of each
    /// (docs/validation/0408-claude-http-mcp-long-calls.md). Neither a Broker call affecting a
    /// Subagent nor the search is work a Transcript presents — the Broker's own rows are the
    /// Broker's to add, and the search is the CLI's plumbing — whether the loop calls them or a
    /// native Subagent does: a stream carrying them projects exactly what the same stream without
    /// them does.
    #[test]
    fn a_broker_call_affecting_a_subagent_and_the_tool_search_loading_it_project_nothing() {
        let search = json!({"query": "select:mcp__suru__send_to_subagent", "max_results": 1});
        let spawn = json!({
            "provider": "codex", "model": "gpt-5.5", "name": "Scout",
            "description": "Map the crates", "prompt": "Map the crates.",
        });
        let loop_bash = streamed_tool_use(2, "toolu_bash", "Bash", json!({"command": "ls"}));
        let [spawned, started] = agent_spawned("toolu_agent", "agent-task", None);
        let with_broker = [
            streamed_tool_use(0, "toolu_search", "ToolSearch", search.clone()).to_vec(),
            vec![tool_result(None, "toolu_search")],
            streamed_tool_use(
                1,
                "toolu_send",
                "mcp__suru__send_to_subagent",
                json!({"session_id": "child", "prompt": "More."}),
            )
            .to_vec(),
            vec![tool_result(None, "toolu_send")],
            loop_bash.to_vec(),
            vec![
                tool_result(None, "toolu_bash"),
                spawned.clone(),
                started.clone(),
            ],
            vec![
                restated_tool_use("toolu_agent", "toolu_sub_search", "ToolSearch", search),
                tool_result(Some("toolu_agent"), "toolu_sub_search"),
                restated_tool_use(
                    "toolu_agent",
                    "toolu_sub_spawn",
                    "mcp__suru__spawn_subagent",
                    spawn,
                ),
                tool_result(Some("toolu_agent"), "toolu_sub_spawn"),
                restated_tool_use(
                    "toolu_agent",
                    "toolu_sub_bash",
                    "Bash",
                    json!({"command": "cargo check"}),
                ),
                tool_result(Some("toolu_agent"), "toolu_sub_bash"),
            ],
        ]
        .concat();
        let without_broker = [
            loop_bash.to_vec(),
            vec![tool_result(None, "toolu_bash"), spawned, started],
            vec![
                restated_tool_use(
                    "toolu_agent",
                    "toolu_sub_bash",
                    "Bash",
                    json!({"command": "cargo check"}),
                ),
                tool_result(Some("toolu_agent"), "toolu_sub_bash"),
            ],
        ]
        .concat();

        let projected = project(&mut fresh_projection(), &with_broker);
        assert_eq!(
            projected,
            project(&mut fresh_projection(), &without_broker),
            "the Broker's calls and the searches loading them project nothing"
        );
        let commands = projected
            .iter()
            .filter_map(|event| match &event.event {
                ProviderEvent::CommandStarted { command, .. } => Some(command.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            ["ls", "cargo check"],
            "the stream around them still projects, a Subagent's work included"
        );
    }

    /// What the CLI reports of the user message written under `uuid`.
    fn lifecycle(uuid: &str, state: &str) -> Value {
        json!({
            "type": "command_lifecycle",
            "command_uuid": uuid,
            "state": state,
            "uuid": format!("frame-{uuid}-{state}"),
            "session_id": "conversation-1",
        })
    }

    /// The owning conversation's next model request opening.
    fn owning_message_start() -> Value {
        json!({
            "type": "stream_event",
            "event": {"type": "message_start"},
            "parent_tool_use_id": null,
        })
    }

    /// Only the Turn boundaries among `events`: where a loop began a Continuation, and where a
    /// Turn Settled.
    fn boundaries(events: &[AttributedProviderEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|event| match event.event {
                ProviderEvent::ContinuationStarted { .. } => Some("continuation started"),
                ProviderEvent::TurnCompleted => Some("turn completed"),
                ProviderEvent::TurnInterrupted => Some("turn interrupted"),
                ProviderEvent::TurnFailed { .. } => Some("turn failed"),
                _ => None,
            })
            .collect()
    }

    /// A projection whose Turn's Prompt a loop has taken up, as the CLI reports it: the Prompt's
    /// uuid, and the projection.
    fn prompted_projection() -> (ClaudeProjection, String) {
        let mut projection = fresh_projection();
        let prompt = projection.turn.begin_turn(selection());
        let taken_up = project(
            &mut projection,
            &[
                lifecycle(&prompt, "queued"),
                lifecycle(&prompt, "started"),
                owning_message_start(),
            ],
        );
        assert!(boundaries(&taken_up).is_empty());
        (projection, prompt)
    }

    fn selection() -> crate::protocol::AgentSelection {
        crate::protocol::AgentSelection {
            provider: crate::protocol::ProviderId::new("claude"),
            model: crate::protocol::ModelId::new("default"),
            options: Vec::new(),
        }
    }

    #[test]
    fn a_steer_folded_in_at_a_tool_round_settles_the_turn_on_the_loops_one_result() {
        // Claude 2.1.283, a steer written while a Bash call ran
        // (docs/validation/0407-claude-folded-steer.md, case A).
        let (mut projection, prompt) = prompted_projection();
        let steer = projection
            .turn
            .accept_steer()
            .expect("the running Turn takes the steer");
        let events = project(
            &mut projection,
            &[
                lifecycle(&steer, "queued"),
                tool_result(None, "toolu_sleep"),
                lifecycle(&steer, "started"),
                owning_message_start(),
                lifecycle(&steer, "completed"),
                result_costing("result-1", "conversation-1", 0.02),
                lifecycle(&prompt, "completed"),
            ],
        );
        assert_eq!(
            boundaries(&events),
            ["turn completed"],
            "the one result answers the Prompt and the steer the loop took up"
        );
        assert!(!projection.turn.is_running());
    }

    #[test]
    fn a_steer_queued_past_its_loops_end_is_answered_in_the_same_turn_by_the_loop_it_begins() {
        // Claude 2.1.283, a steer written while the loop's last request streamed its answer
        // (docs/validation/0407-claude-folded-steer.md, case D).
        let (mut projection, prompt) = prompted_projection();
        let steer = projection
            .turn
            .accept_steer()
            .expect("the running Turn takes the steer");
        let first = project(
            &mut projection,
            &[
                lifecycle(&steer, "queued"),
                result_costing("result-1", "conversation-1", 0.02),
                lifecycle(&prompt, "completed"),
            ],
        );
        assert!(
            boundaries(&first).is_empty(),
            "the loop the steer never joined ends without ending the Turn: {first:?}"
        );
        let second = project(
            &mut projection,
            &[
                lifecycle(&steer, "started"),
                owning_message_start(),
                result_costing("result-2", "conversation-1", 0.03),
                lifecycle(&steer, "completed"),
            ],
        );
        assert_eq!(
            boundaries(&second),
            ["turn completed"],
            "the steer's own loop runs in the Turn it joined and Settles it"
        );
    }

    #[test]
    fn a_steer_the_cli_drops_after_the_turns_last_loop_ended_settles_the_turn_as_completed() {
        for dropped in ["discarded", "refused"] {
            let (mut projection, _prompt) = prompted_projection();
            let steer = projection
                .turn
                .accept_steer()
                .expect("the running Turn takes the steer");
            let ended = project(
                &mut projection,
                &[
                    lifecycle(&steer, "queued"),
                    result_costing("result-1", "conversation-1", 0.02),
                ],
            );
            assert!(boundaries(&ended).is_empty());
            let settled = project(&mut projection, &[lifecycle(&steer, dropped)]);
            assert_eq!(
                boundaries(&settled),
                ["turn completed"],
                "no loop will ever answer a {dropped} steer, so its end is the Turn's"
            );
            assert!(!projection.turn.is_running());
        }
    }

    #[test]
    fn an_interrupt_between_the_turns_loops_settles_it_interrupted_as_its_steer_is_cancelled() {
        // The loop has ended with its result, and the steer it never took up has yet to start a
        // loop of its own when the user interrupts: `cancel_queued` sweeps the steer, and with no
        // loop running there is no aborted result to follow.
        let (mut projection, prompt) = prompted_projection();
        let steer = projection
            .turn
            .accept_steer()
            .expect("the running Turn takes the steer");
        let ended = project(
            &mut projection,
            &[
                lifecycle(&steer, "queued"),
                result_costing("result-1", "conversation-1", 0.02),
                lifecycle(&prompt, "completed"),
            ],
        );
        assert!(boundaries(&ended).is_empty());
        let interrupted = project(&mut projection, &[lifecycle(&steer, "cancelled")]);
        assert_eq!(
            boundaries(&interrupted),
            ["turn interrupted"],
            "the user stopped the Turn, and the steer's cancellation is where it ends"
        );
        assert!(!projection.turn.is_running());
    }

    #[test]
    fn a_cancellation_after_an_aborted_result_settles_nothing_more() {
        let (mut projection, prompt) = prompted_projection();
        let steer = projection
            .turn
            .accept_steer()
            .expect("the running Turn takes the steer");
        let aborted = project(
            &mut projection,
            &[
                lifecycle(&steer, "queued"),
                lifecycle(&steer, "cancelled"),
                json!({
                    "type": "result",
                    "subtype": "error_during_execution",
                    "is_error": true,
                    "terminal_reason": "aborted_tools",
                    "errors": [],
                }),
                lifecycle(&prompt, "cancelled"),
            ],
        );
        assert_eq!(
            boundaries(&aborted),
            ["turn interrupted"],
            "the aborted result alone settles the Turn the interrupt stopped (case F)"
        );
    }

    #[test]
    fn without_a_lifecycle_a_loop_begun_after_a_steered_result_is_a_continuation() {
        // A CLI that reports no message's fate: nothing tells a steer folded into the loop from
        // one still queued, so the result Settles the Turn rather than wait on one that may never
        // come.
        let mut projection = fresh_projection();
        projection.turn.begin_turn(selection());
        projection
            .turn
            .accept_steer()
            .expect("the running Turn takes the steer");
        let events = project(
            &mut projection,
            &[
                owning_message_start(),
                result_costing("result-1", "conversation-1", 0.02),
                owning_message_start(),
                result_costing("result-2", "conversation-1", 0.03),
            ],
        );
        assert_eq!(
            boundaries(&events),
            ["turn completed", "continuation started", "turn completed"]
        );
    }
}
