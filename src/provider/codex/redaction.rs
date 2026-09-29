//! Secret-aware redaction at the typed native content boundary. Stream tails
//! stay private until they can no longer become a submitted secret.
use super::wire::{
    NativeField, NativeMcpContent, NativeNotification as Event, NativeToolUse,
    NativeTurnFailureKind, NativeTurnOutcome, NativeWebSearchAction,
};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Eq, Hash, PartialEq)]
enum Channel {
    Message,
    Command,
    Reasoning(usize),
}
#[derive(Clone, Eq, Hash, PartialEq)]
struct Stream {
    thread: String,
    turn: String,
    item: String,
    channel: Channel,
}
impl Stream {
    fn new(thread: &str, turn: &str, item: &str, channel: Channel) -> Self {
        Self {
            thread: thread.into(),
            turn: turn.into(),
            item: item.into(),
            channel,
        }
    }
    fn delta(self, delta: String) -> Event {
        let Self {
            thread: thread_id,
            turn: turn_id,
            item: item_id,
            channel,
        } = self;
        match channel {
            Channel::Message => Event::AgentMessageDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            },
            Channel::Command => Event::CommandOutputDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            },
            Channel::Reasoning(summary_index) => Event::ReasoningDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
                summary_index,
            },
        }
    }
}
#[derive(Default)]
pub(super) struct SecretRedactor {
    secrets: Vec<String>,
    tails: HashMap<Stream, String>,
}
impl SecretRedactor {
    pub(super) fn remember(&mut self, values: impl IntoIterator<Item = String>) {
        self.secrets
            .extend(values.into_iter().filter(|value| !value.is_empty()));
        self.secrets
            .sort_by_key(|value| std::cmp::Reverse(value.len()));
        self.secrets.dedup();
    }
    pub(super) fn text(&self, text: &str) -> String {
        let mut remaining = text;
        let mut output = String::new();
        while !remaining.is_empty() {
            if let Some(secret) = self
                .secrets
                .iter()
                .find(|secret| remaining.starts_with(secret.as_str()))
            {
                output.push_str("[redacted]");
                remaining = &remaining[secret.len()..];
            } else {
                let next = remaining
                    .chars()
                    .next()
                    .expect("nonempty content remainder");
                output.push(next);
                remaining = &remaining[next.len_utf8()..];
            }
        }
        output
    }
    fn path(&self, path: &mut PathBuf) {
        if let Some(text) = path.to_str() {
            *path = self.text(text).into();
        }
    }
    fn json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.text(text),
            serde_json::Value::Array(values) => {
                values.iter_mut().for_each(|value| self.json(value));
            }
            serde_json::Value::Object(values) => {
                values.values_mut().for_each(|value| self.json(value));
            }
            _ => {}
        }
    }
    fn optional(&self, text: &mut Option<String>) {
        if let Some(text) = text {
            *text = self.text(text);
        }
    }
    fn tool_use(&self, tool: &mut NativeToolUse) {
        match tool {
            NativeToolUse::Mcp(call) => {
                call.server = self.text(&call.server);
                call.tool = self.text(&call.tool);
                // Only the strings: the projection redacts the input again once rendered, where
                // a number or a key may spell a secret too.
                self.json(&mut call.arguments);
                for block in call
                    .result
                    .iter_mut()
                    .flat_map(|result| result.content.iter_mut())
                {
                    if let NativeMcpContent::Text(text) = block {
                        *text = self.text(text);
                    }
                }
                if let Some(error) = &mut call.error {
                    error.message = self.text(&error.message);
                }
            }
            NativeToolUse::WebSearch(search) => {
                self.optional(&mut search.query);
                match &mut search.action {
                    Some(NativeWebSearchAction::Search { query, queries }) => {
                        self.optional(query);
                        for query in queries.iter_mut().flatten() {
                            *query = self.text(query);
                        }
                    }
                    Some(NativeWebSearchAction::OpenPage { url }) => self.optional(url),
                    Some(NativeWebSearchAction::FindInPage { url, pattern }) => {
                        self.optional(url);
                        self.optional(pattern);
                    }
                    Some(NativeWebSearchAction::Other) | None => {}
                }
            }
            NativeToolUse::ImageView(view) => view.path = self.text(&view.path),
            NativeToolUse::ImageGeneration(generation) => {
                self.optional(&mut generation.revised_prompt);
                self.optional(&mut generation.saved_path);
            }
            NativeToolUse::Sleep(_) => {}
        }
    }
    fn delta(&mut self, key: Stream, delta: &mut String) {
        let mut input = self.tails.remove(&key).unwrap_or_default();
        input.push_str(delta);
        let mut remaining = input.as_str();
        let mut output = String::new();
        while !remaining.is_empty() {
            // A shorter secret may prefix a longer one. Wait for enough input
            // to make the same longest-match decision as completed text.
            if self
                .secrets
                .iter()
                .any(|secret| secret.len() > remaining.len() && secret.starts_with(remaining))
            {
                self.tails.insert(key, remaining.to_owned());
                break;
            } else if let Some(secret) = self
                .secrets
                .iter()
                .find(|secret| remaining.starts_with(secret.as_str()))
            {
                output.push_str("[redacted]");
                remaining = &remaining[secret.len()..];
            } else {
                let next = remaining.chars().next().expect("nonempty stream remainder");
                output.push(next);
                remaining = &remaining[next.len_utf8()..];
            }
        }
        *delta = output;
    }
    fn finish(&mut self, predicate: impl Fn(&Stream) -> bool) -> Vec<Event> {
        let keys: Vec<_> = self
            .tails
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect();
        keys.into_iter()
            .map(|key| {
                let tail = self
                    .tails
                    .remove(&key)
                    .expect("selected stream remains buffered");
                // A shorter secret can prefix a longer one still buffered here.
                // At this boundary whole-text redaction makes the final choice.
                key.delta(self.text(&tail))
            })
            .collect()
    }
    pub(super) fn notification(&mut self, mut event: Event) -> Vec<Event> {
        let mut prefix = Vec::new();
        match &mut event {
            Event::AgentMessageDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            } => self.delta(
                Stream::new(thread_id, turn_id, item_id, Channel::Message),
                delta,
            ),
            Event::CommandOutputDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            } => self.delta(
                Stream::new(thread_id, turn_id, item_id, Channel::Command),
                delta,
            ),
            Event::ReasoningDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
                summary_index,
            } => self.delta(
                Stream::new(
                    thread_id,
                    turn_id,
                    item_id,
                    Channel::Reasoning(*summary_index),
                ),
                delta,
            ),
            Event::AgentMessageCompleted {
                thread_id,
                turn_id,
                item_id,
                text,
            } => {
                *text = self.text(text);
                self.tails
                    .remove(&Stream::new(thread_id, turn_id, item_id, Channel::Message));
            }
            Event::CommandCompleted {
                thread_id,
                turn_id,
                item_id,
                aggregated_output,
                ..
            } => {
                let key = Stream::new(thread_id, turn_id, item_id, Channel::Command);
                if let Some(output) = aggregated_output {
                    *output = self.text(output);
                    self.tails.remove(&key);
                } else {
                    prefix.extend(self.finish(|stream| stream == &key));
                }
            }
            Event::ReasoningCompleted {
                thread_id,
                turn_id,
                item_id,
                summary,
            } => {
                summary.iter_mut().for_each(|text| *text = self.text(text));
                self.tails.retain(|stream, _| {
                    stream.thread != *thread_id
                        || stream.turn != *turn_id
                        || stream.item != *item_id
                });
            }
            Event::ReasoningSectionBreak {
                thread_id,
                turn_id,
                item_id,
                summary_index,
            } => {
                prefix.extend(self.finish(|stream| {
                    stream.thread == *thread_id
                        && stream.turn == *turn_id
                        && stream.item == *item_id
                        && stream.channel != Channel::Reasoning(*summary_index)
                }));
            }
            Event::CommandStarted { command, cwd, .. } => {
                *command = self.text(command);
                if let Some(cwd) = cwd {
                    self.path(cwd);
                }
            }
            Event::CommandApprovalRequested { params, .. } => {
                params.reason = params.reason.as_ref().map(|text| self.text(text));
                params.command = params.command.as_ref().map(|text| self.text(text));
                if let Some(cwd) = &mut params.cwd {
                    self.path(cwd);
                }
                if let Some(network) = &mut params.network_approval_context {
                    network.host = self.text(&network.host);
                    network.protocol = self.text(&network.protocol);
                }
                if let Some(actions) = &mut params.command_actions {
                    actions.iter_mut().for_each(|action| self.json(action));
                }
            }
            Event::FileChangeApprovalRequested { params, .. } => {
                params.reason = params.reason.as_ref().map(|text| self.text(text));
                if let Some(root) = &mut params.grant_root {
                    self.path(root);
                }
            }
            Event::PermissionsApprovalRequested { params, .. } => {
                params.reason = params.reason.as_ref().map(|text| self.text(text));
                self.json(&mut params.permissions);
            }
            Event::FileChangeStarted { changes, .. }
            | Event::FileChangeUpdated { changes, .. }
            | Event::FileChangeCompleted { changes, .. } => {
                for change in changes {
                    change.redact_paths(|path| self.path(path));
                }
            }
            Event::TurnCompleted {
                thread_id,
                turn_id,
                outcome,
                final_agent_message,
            } => {
                if let Some(message) = final_agent_message {
                    message.text = self.text(&message.text);
                    self.tails.remove(&Stream::new(
                        thread_id,
                        turn_id,
                        &message.item_id,
                        Channel::Message,
                    ));
                }
                if let NativeTurnOutcome::Failed { message, kind } = outcome {
                    *message =
                        super::concise_remote_message(&self.text(message), "Codex Turn failed");
                    if let NativeTurnFailureKind::BadRequest {
                        additional_details: Some(details),
                    } = kind
                    {
                        *details = self.text(details);
                    }
                }
                prefix.extend(
                    self.finish(|stream| stream.thread == *thread_id && stream.turn == *turn_id),
                );
            }
            Event::ToolUseStarted { tool, .. } | Event::ToolUseCompleted { tool, .. } => {
                self.tool_use(tool);
            }
            Event::UserMessage { text, .. } => *text = self.text(text),
            Event::CollabCallStarted { prompt, .. } | Event::CollabCallCompleted { prompt, .. } => {
                if let Some(prompt) = prompt {
                    *prompt = self.text(prompt);
                }
            }
            Event::SubagentActivity { agent_path, .. } => *agent_path = self.text(agent_path),
            Event::AgentSelectionChanged {
                model,
                effort,
                service_tier,
                ..
            } => {
                *model = self.text(model);
                for field in [effort, service_tier] {
                    if let NativeField::Present(Some(value)) = field {
                        *value = self.text(value);
                    }
                }
            }
            Event::QuestionnaireRequested { .. }
            | Event::QuestionnaireResolved { .. }
            | Event::SkillsChanged
            | Event::AgentMessageStarted { .. }
            | Event::ReasoningStarted { .. }
            | Event::TokenUsage { .. }
            | Event::TurnStarted { .. } => {}
        }
        prefix.push(event);
        prefix
    }
}
