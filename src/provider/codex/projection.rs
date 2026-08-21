//! Projection of Codex's native notifications onto Suru's Provider events.
//!
//! [`NativeCorrelation`] is the running state this projection needs: which native Turn is active
//! and which Messages, commands, file changes, and Reasoning blocks are still open within it.
//! Notifications that belong to a Turn Suru is no longer tracking are dropped, notifications that
//! contradict the recorded state fail the Session, and everything else becomes the Provider events
//! a Session consumes.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{
    DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID,
    codex_error,
    process::ProcessGuard,
    reasoning::ReasoningSummarySplitter,
    wire::{
        NATIVE_REASONING_SECTION_SEPARATOR, NativeCommandStatus, NativeField, NativeFileChange,
        NativeFileChangeStatus, NativeNotification, NativeTurnFailureKind, NativeTurnOutcome,
    },
};
use crate::{
    protocol::{
        AgentSelection, FileChange, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionSelection, ModelOptionValue,
    },
    provider::{
        ProviderActivityId, ProviderCommandStatus, ProviderError, ProviderEvent,
        ProviderEventStream, ProviderFileChangeStatus,
    },
};

/// Everything the projection must remember between notifications for one Codex thread.
pub(super) struct NativeCorrelation {
    thread_id: String,
    turn_starting: bool,
    active_turn_id: Option<String>,
    active_selection: Option<AgentSelection>,
    active_agent_message: Option<ActiveNativeAgentMessage>,
    active_commands: HashMap<String, ActiveNativeCommand>,
    active_file_changes: HashMap<String, ActiveNativeFileChange>,
    active_reasoning: HashMap<String, ActiveNativeReasoning>,
}

struct ActiveNativeAgentMessage {
    item_id: String,
    streamed_text: String,
}

struct ActiveNativeCommand {
    streamed_output: String,
}

struct ActiveNativeFileChange {
    changes: Vec<FileChange>,
}

/// A Reasoning block Codex is still streaming. `streamed_summary` is the raw
/// summary text as Codex sent it — section breaks included — so the completed
/// item, which repeats the whole summary, can be reconciled against it, while
/// `splitter` holds the title block back from the content until it resolves.
struct ActiveNativeReasoning {
    streamed_summary: String,
    splitter: ReasoningSummarySplitter,
}

impl NativeCorrelation {
    pub(super) fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            turn_starting: false,
            active_turn_id: None,
            active_selection: None,
            active_agent_message: None,
            active_commands: HashMap::new(),
            active_file_changes: HashMap::new(),
            active_reasoning: HashMap::new(),
        }
    }

    /// The native Turn whose notifications are currently being projected.
    pub(super) fn active_turn_id(&self) -> Option<String> {
        self.active_turn_id.clone()
    }

    /// Whether a `turn/start` request has been issued but not yet answered.
    pub(super) fn is_turn_pending(&self) -> bool {
        self.turn_starting
    }

    /// Claims the single native Turn slot ahead of issuing a `turn/start` request.
    pub(super) fn begin_turn_start(&mut self) -> Result<(), ProviderError> {
        if self.turn_starting || self.active_turn_id.is_some() {
            return Err(codex_error(
                "Codex started a Turn while another native Turn was active",
            ));
        }
        self.turn_starting = true;
        Ok(())
    }

    /// Releases the claim taken by [`Self::begin_turn_start`], installing the Turn if it started.
    pub(super) fn finish_turn_start(
        &mut self,
        started: Result<String, ProviderError>,
        selection: AgentSelection,
    ) -> Result<(), ProviderError> {
        self.turn_starting = false;
        match started {
            Ok(turn_id) if self.active_turn_id.is_none() => {
                self.active_turn_id = Some(turn_id);
                self.active_selection = Some(selection);
                self.active_agent_message = None;
                self.active_commands.clear();
                self.active_file_changes.clear();
                self.active_reasoning.clear();
                Ok(())
            }
            Ok(_) => Err(codex_error(
                "Codex started a Turn while another native Turn was active",
            )),
            Err(error) => Err(error),
        }
    }

    fn is_active_turn(&self, thread_id: &str, turn_id: &str) -> bool {
        self.thread_id == thread_id && self.active_turn_id.as_deref() == Some(turn_id)
    }

    fn settle_turn(&mut self) {
        self.active_turn_id = None;
        self.active_selection = None;
        self.active_agent_message = None;
        self.active_commands.clear();
        self.active_file_changes.clear();
        self.active_reasoning.clear();
    }
}

/// Streams the Provider events projected from `notifications`, holding the process open meanwhile.
pub(super) fn provider_events(
    notifications: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    process: Arc<ProcessGuard>,
    correlation: Arc<StdMutex<NativeCorrelation>>,
) -> ProviderEventStream {
    Box::pin(stream::unfold(
        GuardedEventReceiver {
            receiver: notifications,
            _process: process,
            correlation,
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
}

struct GuardedEventReceiver {
    receiver: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    _process: Arc<ProcessGuard>,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: GuardedEventReceiver,
) -> Option<(Result<ProviderEvent, ProviderError>, GuardedEventReceiver)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let native = events.receiver.recv().await?;
        match native {
            Err(error) => return Some((Err(error), events)),
            Ok(native) => {
                let projected = {
                    let mut correlation = events
                        .correlation
                        .lock()
                        .expect("Codex native correlation lock is not poisoned");
                    project_native_notification(&mut correlation, native)
                };
                match projected {
                    Ok(projected) => events.pending.extend(projected.into_iter().map(Ok)),
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

fn project_native_notification(
    correlation: &mut NativeCorrelation,
    notification: NativeNotification,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    match notification {
        NativeNotification::AgentSelectionChanged {
            thread_id,
            model,
            effort,
            service_tier,
        } => project_agent_selection_changed(correlation, &thread_id, model, effort, service_tier),
        NativeNotification::AgentMessageStarted {
            thread_id,
            turn_id,
            item_id,
        } => project_agent_message_started(correlation, &thread_id, &turn_id, item_id),
        NativeNotification::AgentMessageDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => project_agent_message_delta(correlation, &thread_id, &turn_id, &item_id, delta),
        NativeNotification::AgentMessageCompleted {
            thread_id,
            turn_id,
            item_id,
            text,
        } => project_agent_message_completed(correlation, &thread_id, &turn_id, &item_id, &text),
        NativeNotification::CommandStarted {
            thread_id,
            turn_id,
            item_id,
            command,
            cwd,
            status,
        } => project_command_started(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            command,
            cwd,
            status,
        ),
        NativeNotification::CommandOutputDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => project_command_output_delta(correlation, &thread_id, &turn_id, item_id, delta),
        NativeNotification::CommandCompleted {
            thread_id,
            turn_id,
            item_id,
            aggregated_output,
            exit_status,
            status,
        } => project_command_completed(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            aggregated_output,
            exit_status,
            status,
        ),
        NativeNotification::FileChangeStarted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => {
            project_file_change_started(correlation, &thread_id, &turn_id, item_id, changes, status)
        }
        NativeNotification::FileChangeUpdated {
            thread_id,
            turn_id,
            item_id,
            changes,
        } => project_file_change_updated(correlation, &thread_id, &turn_id, item_id, changes),
        NativeNotification::FileChangeCompleted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => project_file_change_completed(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            changes,
            status,
        ),
        NativeNotification::ReasoningStarted {
            thread_id,
            turn_id,
            item_id,
        } => project_reasoning_started(correlation, &thread_id, &turn_id, item_id),
        NativeNotification::ReasoningDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => project_reasoning_delta(correlation, &thread_id, &turn_id, item_id, &delta),
        NativeNotification::ReasoningSectionBreak {
            thread_id,
            turn_id,
            item_id,
        } => project_reasoning_section_break(correlation, &thread_id, &turn_id, item_id),
        NativeNotification::ReasoningCompleted {
            thread_id,
            turn_id,
            item_id,
            summary,
        } => project_reasoning_completed(correlation, &thread_id, &turn_id, item_id, summary),
        NativeNotification::TurnCompleted {
            thread_id,
            turn_id,
            outcome,
        } => project_turn_completed(correlation, &thread_id, &turn_id, outcome),
    }
}

fn project_reasoning_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    if correlation.active_reasoning.contains_key(&item_id) {
        return Err(codex_error(
            "Codex reused an active Reasoning item identity",
        ));
    }
    correlation.active_reasoning.insert(
        item_id.clone(),
        ActiveNativeReasoning {
            streamed_summary: String::new(),
            splitter: ReasoningSummarySplitter::default(),
        },
    );
    Ok(vec![ProviderEvent::ReasoningStarted {
        activity_id: ProviderActivityId::new(item_id),
    }])
}

fn project_reasoning_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    delta: &str,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(reasoning) = correlation.active_reasoning.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    reasoning.streamed_summary.push_str(delta);
    let segment = reasoning.splitter.push(delta);
    Ok(reasoning_segment_events(&item_id, segment))
}

/// Projects the break between two Reasoning summary sections as the separator
/// that joins them. Codex announces the first section's break too, so a break
/// that arrives before any of the summary has streamed opens nothing and is
/// dropped: separating an empty section from the first real one would put a
/// blank line at the head of the block and leave the streamed summary no longer
/// a prefix of the one the completed item repeats.
fn project_reasoning_section_break(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    if correlation
        .active_reasoning
        .get(&item_id)
        .is_none_or(|reasoning| reasoning.streamed_summary.is_empty())
    {
        return Ok(Vec::new());
    }
    project_reasoning_delta(
        correlation,
        thread_id,
        turn_id,
        item_id,
        NATIVE_REASONING_SECTION_SEPARATOR,
    )
}

fn project_reasoning_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    summary: Vec<String>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    // Reasoning is the account of the work rather than the work, so nothing
    // about it fails a Turn: losing the Turn over a Reasoning summary would cost
    // the reader the answer it led to. A completion for a block Suru never saw
    // start has nowhere to land, and is dropped the way a stray delta is.
    let Some(reasoning) = correlation.active_reasoning.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    // Codex repeats the whole summary here, and only streams it when the block
    // was streaming to a client at all. Take whatever the stream had not
    // already carried; when the two disagree, the stream already showed the
    // reader a coherent block, so repeating the completed summary on top of it
    // would double the text rather than correct it.
    let completed_summary = summary.join(NATIVE_REASONING_SECTION_SEPARATOR);
    let remaining = completed_summary
        .strip_prefix(&reasoning.streamed_summary)
        .unwrap_or_default();
    let mut projected = Vec::new();
    if !remaining.is_empty() {
        projected.extend(reasoning_segment_events(
            &item_id,
            reasoning.splitter.push(remaining),
        ));
    }
    projected.extend(reasoning_segment_events(
        &item_id,
        reasoning.splitter.finish(),
    ));
    correlation.active_reasoning.remove(&item_id);
    projected.push(ProviderEvent::ReasoningCompleted {
        activity_id: ProviderActivityId::new(item_id),
    });
    Ok(projected)
}

/// Lowers one split step of a Reasoning summary onto the Provider events that
/// carry it, dropping the step that resolved nothing because the splitter is
/// still withholding the head.
fn reasoning_segment_events(
    item_id: &str,
    segment: super::reasoning::ReasoningSegment,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    if let Some(title) = segment.title {
        events.push(ProviderEvent::ReasoningTitleChanged {
            activity_id: ProviderActivityId::new(item_id.to_owned()),
            title,
        });
    }
    if !segment.content.is_empty() {
        events.push(ProviderEvent::ReasoningDelta {
            activity_id: ProviderActivityId::new(item_id.to_owned()),
            content: segment.content,
        });
    }
    events
}

fn project_agent_selection_changed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    model: String,
    effort: NativeField<Option<String>>,
    service_tier: NativeField<Option<String>>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if correlation.thread_id != thread_id || correlation.active_turn_id.is_none() {
        return Ok(Vec::new());
    }
    if model.is_empty() {
        return Err(codex_error(
            "Codex reported an empty effective Model for the active Turn",
        ));
    }
    let Some(requested) = correlation.active_selection.as_ref() else {
        return Err(codex_error(
            "Codex reported effective settings before accepting the active Turn",
        ));
    };
    let mut effective = requested.clone();
    effective.model = ModelId::new(model);
    apply_effective_select_option(
        &mut effective,
        REASONING_EFFORT_OPTION_ID,
        effort,
        NativeClearMapping::RemoveOption,
    )?;
    apply_effective_select_option(
        &mut effective,
        SERVICE_TIER_OPTION_ID,
        service_tier,
        NativeClearMapping::Select(DEFAULT_SERVICE_TIER_CHOICE_ID),
    )?;
    if effective == *requested {
        return Ok(Vec::new());
    }
    correlation.active_selection = Some(effective.clone());
    Ok(vec![ProviderEvent::AgentSelectionChanged {
        selection: effective,
    }])
}

fn project_agent_message_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    if correlation.active_agent_message.is_some() {
        return Err(codex_error(
            "Codex started a second Agent Message before completing the first",
        ));
    }
    correlation.active_agent_message = Some(ActiveNativeAgentMessage {
        item_id,
        streamed_text: String::new(),
    });
    Ok(vec![ProviderEvent::AgentMessageStarted])
}

fn project_agent_message_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    delta: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(message) = correlation.active_agent_message.as_mut() else {
        return Err(codex_error(
            "Codex sent Agent Message content before starting the Message",
        ));
    };
    if message.item_id != item_id {
        return Ok(Vec::new());
    }
    message.streamed_text.push_str(&delta);
    Ok(vec![ProviderEvent::AgentMessageDelta { content: delta }])
}

fn project_agent_message_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    text: &str,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(message) = correlation.active_agent_message.as_ref() else {
        return Err(codex_error(
            "Codex completed an Agent Message before starting it",
        ));
    };
    if message.item_id != item_id {
        return Ok(Vec::new());
    }
    let Some(remaining) = text.strip_prefix(&message.streamed_text) else {
        return Err(codex_error(
            "Codex completed an Agent Message with content that did not match its stream",
        ));
    };
    let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
    if !remaining.is_empty() {
        projected.push(ProviderEvent::AgentMessageDelta {
            content: remaining.to_owned(),
        });
    }
    projected.push(ProviderEvent::AgentMessageCompleted);
    correlation.active_agent_message = None;
    Ok(projected)
}

fn project_command_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    command: String,
    cwd: Option<PathBuf>,
    status: NativeCommandStatus,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    if !matches!(status, NativeCommandStatus::InProgress) {
        return Err(codex_error(
            "Codex started a command outside its active state",
        ));
    }
    if correlation.active_commands.contains_key(&item_id) {
        return Err(codex_error("Codex reused an active command item identity"));
    }
    correlation.active_commands.insert(
        item_id.clone(),
        ActiveNativeCommand {
            streamed_output: String::new(),
        },
    );
    Ok(vec![ProviderEvent::CommandStarted {
        activity_id: ProviderActivityId::new(item_id),
        command,
        cwd,
    }])
}

fn project_command_output_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    delta: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(command) = correlation.active_commands.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    command.streamed_output.push_str(&delta);
    Ok(vec![ProviderEvent::CommandOutputDelta {
        activity_id: ProviderActivityId::new(item_id),
        content: delta,
    }])
}

fn project_command_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    aggregated_output: Option<String>,
    exit_status: Option<i32>,
    status: NativeCommandStatus,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(command) = correlation.active_commands.get(&item_id) else {
        return Err(codex_error(
            "Codex completed a command before starting the Activity",
        ));
    };
    let remaining = match aggregated_output {
        Some(output) => output
            .strip_prefix(&command.streamed_output)
            .ok_or_else(|| {
                codex_error("Codex completed a command with output that did not match its stream")
            })?
            .to_owned(),
        None => String::new(),
    };
    let status = match status {
        NativeCommandStatus::Completed => ProviderCommandStatus::Completed,
        NativeCommandStatus::Failed | NativeCommandStatus::Declined => {
            ProviderCommandStatus::Failed
        }
        NativeCommandStatus::InProgress => {
            return Err(codex_error(
                "Codex completed a command while it was still active",
            ));
        }
    };
    correlation.active_commands.remove(&item_id);
    let activity_id = ProviderActivityId::new(item_id);
    let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
    if !remaining.is_empty() {
        projected.push(ProviderEvent::CommandOutputDelta {
            activity_id: activity_id.clone(),
            content: remaining,
        });
    }
    projected.push(ProviderEvent::CommandCompleted {
        activity_id,
        status,
        exit_status,
    });
    Ok(projected)
}

fn project_file_change_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
    status: NativeFileChangeStatus,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    if !matches!(status, NativeFileChangeStatus::InProgress) {
        return Err(codex_error(
            "Codex started file changes outside their active state",
        ));
    }
    if correlation.active_commands.contains_key(&item_id)
        || correlation.active_file_changes.contains_key(&item_id)
    {
        return Err(codex_error(
            "Codex reused an active file-change item identity",
        ));
    }
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    correlation.active_file_changes.insert(
        item_id.clone(),
        ActiveNativeFileChange {
            changes: changes.clone(),
        },
    );
    Ok(vec![ProviderEvent::FileChangeStarted {
        activity_id: ProviderActivityId::new(item_id),
        changes,
    }])
}

fn project_file_change_updated(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(file_change) = correlation.active_file_changes.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    file_change.changes.clone_from(&changes);
    Ok(vec![ProviderEvent::FileChangeUpdated {
        activity_id: ProviderActivityId::new(item_id),
        changes,
    }])
}

fn project_file_change_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
    status: NativeFileChangeStatus,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let Some(file_change) = correlation.active_file_changes.get(&item_id) else {
        return Err(codex_error(
            "Codex completed file changes before starting the Activity",
        ));
    };
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    let changes_changed = file_change.changes != changes;
    let status = match status {
        NativeFileChangeStatus::Completed => ProviderFileChangeStatus::Completed,
        NativeFileChangeStatus::Failed | NativeFileChangeStatus::Declined => {
            ProviderFileChangeStatus::Failed
        }
        NativeFileChangeStatus::InProgress => {
            return Err(codex_error(
                "Codex completed file changes while they were still active",
            ));
        }
    };
    correlation.active_file_changes.remove(&item_id);
    let activity_id = ProviderActivityId::new(item_id);
    let mut projected = Vec::with_capacity(if changes_changed { 2 } else { 1 });
    if changes_changed {
        projected.push(ProviderEvent::FileChangeUpdated {
            activity_id: activity_id.clone(),
            changes,
        });
    }
    projected.push(ProviderEvent::FileChangeCompleted {
        activity_id,
        status,
    });
    Ok(projected)
}

fn project_turn_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    outcome: NativeTurnOutcome,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let selection_rejected = match (&outcome, &correlation.active_selection) {
        (NativeTurnOutcome::Failed { message, kind }, Some(selection)) => {
            is_native_selection_rejection(message, kind, selection)
        }
        _ => false,
    };
    correlation.settle_turn();
    Ok(vec![match outcome {
        NativeTurnOutcome::Completed => ProviderEvent::TurnCompleted,
        NativeTurnOutcome::Interrupted => ProviderEvent::TurnInterrupted,
        NativeTurnOutcome::Failed { message, .. } if selection_rejected => {
            ProviderEvent::AgentSelectionRejected { message }
        }
        NativeTurnOutcome::Failed { message, .. } => ProviderEvent::TurnFailed { message },
    }])
}

/// Decides whether a failed native Turn was Codex refusing the Agent Selection it was given.
fn is_native_selection_rejection(
    message: &str,
    kind: &NativeTurnFailureKind,
    selection: &AgentSelection,
) -> bool {
    match kind {
        NativeTurnFailureKind::BadRequest { additional_details } => additional_details
            .as_deref()
            .and_then(|details| serde_json::from_str::<Value>(details).ok())
            .is_some_and(|details| json_identifies_agent_selection_parameter(&details)),
        NativeTurnFailureKind::Other => {
            let message = message.to_ascii_lowercase();
            let selected_model = selection.model.as_str().to_ascii_lowercase();
            let rejected = [
                "unavailable",
                "unsupported",
                "not available",
                "not found",
                "does not exist",
                "unknown",
                "invalid",
                "access",
                "denied",
                "retired",
            ]
            .iter()
            .any(|reason| message.contains(reason));
            let identifies_model = message.contains("model") && message.contains(&selected_model);
            let identifies_option = selection.options.iter().any(|option| {
                let option_id = option.id.as_str().to_ascii_lowercase();
                let option_label = option_id.replace('_', " ");
                message.contains(&option_id) || message.contains(&option_label)
            });
            rejected && (identifies_model || identifies_option)
        }
    }
}

fn json_identifies_agent_selection_parameter(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("param")
                .and_then(Value::as_str)
                .is_some_and(|parameter| {
                    matches!(
                        parameter,
                        "model"
                            | "effort"
                            | "reasoningEffort"
                            | "reasoning_effort"
                            | "serviceTier"
                            | "service_tier"
                    )
                })
                || fields
                    .values()
                    .any(json_identifies_agent_selection_parameter)
        }
        Value::Array(values) => values.iter().any(json_identifies_agent_selection_parameter),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn apply_effective_select_option(
    selection: &mut AgentSelection,
    option_id: &str,
    field: NativeField<Option<String>>,
    clear: NativeClearMapping,
) -> Result<(), ProviderError> {
    let NativeField::Present(value) = field else {
        return Ok(());
    };
    let choice = match (value, clear) {
        (Some(choice), _) => choice,
        (None, NativeClearMapping::Select(choice)) => choice.to_owned(),
        (None, NativeClearMapping::RemoveOption) => {
            selection
                .options
                .retain(|option| option.id.as_str() != option_id);
            return Ok(());
        }
    };
    if choice.is_empty() {
        return Err(codex_error(format!(
            "Codex reported an empty effective value for Model Option `{option_id}`"
        )));
    }
    let value = ModelOptionValue::Select {
        choice: ModelOptionChoiceId::new(choice),
    };
    if let Some(option) = selection
        .options
        .iter_mut()
        .find(|option| option.id.as_str() == option_id)
    {
        option.value = value;
    } else {
        selection.options.push(ModelOptionSelection {
            id: ModelOptionId::new(option_id),
            value,
        });
    }
    Ok(())
}

/// How a cleared native Model Option maps back onto a Suru Agent Selection.
enum NativeClearMapping {
    RemoveOption,
    Select(&'static str),
}

#[cfg(test)]
mod tests {
    use crate::protocol::{
        AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, ProviderId,
    };

    use super::{
        NativeCorrelation, NativeNotification, NativeTurnFailureKind, ProviderActivityId,
        ProviderEvent, is_native_selection_rejection, project_native_notification,
    };

    const THREAD: &str = "thread-fixture";
    const TURN: &str = "turn-fixture";
    const ITEM: &str = "item-fixture";

    fn reasoning_turn() -> NativeCorrelation {
        let mut correlation = NativeCorrelation::new(THREAD.to_owned());
        correlation.begin_turn_start().expect("claim the Turn slot");
        correlation
            .finish_turn_start(
                Ok(TURN.to_owned()),
                AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-fixture"),
                    options: Vec::new(),
                },
            )
            .expect("install the native Turn");
        correlation
    }

    fn project(
        correlation: &mut NativeCorrelation,
        notification: NativeNotification,
    ) -> Vec<ProviderEvent> {
        project_native_notification(correlation, notification).expect("project the notification")
    }

    fn started() -> NativeNotification {
        NativeNotification::ReasoningStarted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
        }
    }

    fn delta(delta: &str) -> NativeNotification {
        NativeNotification::ReasoningDelta {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            delta: delta.to_owned(),
        }
    }

    fn section_break() -> NativeNotification {
        NativeNotification::ReasoningSectionBreak {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
        }
    }

    fn completed(summary: &[&str]) -> NativeNotification {
        NativeNotification::ReasoningCompleted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            summary: summary
                .iter()
                .map(|section| (*section).to_owned())
                .collect(),
        }
    }

    fn activity_id() -> ProviderActivityId {
        ProviderActivityId::new(ITEM)
    }

    #[test]
    fn a_streamed_reasoning_summary_becomes_a_titled_block_and_its_body() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(&mut correlation, started()),
            vec![ProviderEvent::ReasoningStarted {
                activity_id: activity_id(),
            }]
        );
        assert_eq!(
            project(&mut correlation, delta("**Inspecting the")),
            Vec::new()
        );
        assert_eq!(
            project(&mut correlation, delta(" seam**\n\nReading it.")),
            vec![
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: activity_id(),
                    title: "Inspecting the seam".to_owned(),
                },
                ProviderEvent::ReasoningDelta {
                    activity_id: activity_id(),
                    content: "Reading it.".to_owned(),
                },
            ]
        );
        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Inspecting the seam**\n\nReading it."]),
            ),
            vec![ProviderEvent::ReasoningCompleted {
                activity_id: activity_id(),
            }]
        );
    }

    #[test]
    fn a_section_break_joins_the_sections_the_completed_summary_reports() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta("**Seam**\n\nReading it."));
        assert_eq!(
            project(&mut correlation, section_break()),
            vec![ProviderEvent::ReasoningDelta {
                activity_id: activity_id(),
                content: "\n\n".to_owned(),
            }]
        );
        project(&mut correlation, delta("Now the store."));

        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Seam**\n\nReading it.", "Now the store."]),
            ),
            vec![ProviderEvent::ReasoningCompleted {
                activity_id: activity_id(),
            }]
        );
    }

    #[test]
    fn the_break_codex_announces_before_the_first_section_separates_nothing() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        assert_eq!(project(&mut correlation, section_break()), Vec::new());

        // An untitled section shows the separator a dropped break would have
        // left: its content would open on a blank line the Provider never sent.
        assert_eq!(
            project(&mut correlation, delta("Reading it.")),
            vec![ProviderEvent::ReasoningDelta {
                activity_id: activity_id(),
                content: "Reading it.".to_owned(),
            }]
        );
        // Dropping the break also left the stream a prefix of the summary the
        // completed item repeats, so the block reconciles and adds nothing.
        assert_eq!(
            project(&mut correlation, completed(&["Reading it."])),
            vec![ProviderEvent::ReasoningCompleted {
                activity_id: activity_id(),
            }]
        );
    }

    #[test]
    fn a_completion_for_reasoning_suru_never_saw_start_is_dropped_rather_than_failing_the_turn() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(&mut correlation, completed(&["**Seam**\n\nReading it."])),
            Vec::new()
        );
    }

    #[test]
    fn a_summary_codex_never_streamed_arrives_whole_with_the_completed_item() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());

        assert_eq!(
            project(&mut correlation, completed(&["**Seam**\n\nReading it."])),
            vec![
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: activity_id(),
                    title: "Seam".to_owned(),
                },
                ProviderEvent::ReasoningDelta {
                    activity_id: activity_id(),
                    content: "Reading it.".to_owned(),
                },
                ProviderEvent::ReasoningCompleted {
                    activity_id: activity_id(),
                },
            ]
        );
    }

    #[test]
    fn a_completed_summary_that_contradicts_its_stream_still_settles_the_block() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta("**Seam**\n\nReading it."));

        assert_eq!(
            project(&mut correlation, completed(&["Something else entirely."])),
            vec![ProviderEvent::ReasoningCompleted {
                activity_id: activity_id(),
            }]
        );
    }

    #[test]
    fn reasoning_from_a_turn_suru_no_longer_tracks_is_dropped() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::ReasoningStarted {
                    thread_id: THREAD.to_owned(),
                    turn_id: "turn-elsewhere".to_owned(),
                    item_id: ITEM.to_owned(),
                },
            ),
            Vec::new()
        );
    }

    #[test]
    fn generic_failures_do_not_treat_incidental_choice_text_as_selection_rejection() {
        let selection = AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-fixture"),
            options: vec![ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low"),
                },
            }],
        };

        assert!(!is_native_selection_rejection(
            "Access denied because credits are low",
            &NativeTurnFailureKind::Other,
            &selection,
        ));
    }
}
