//! Secret-aware redaction at the typed native content boundary. Stream tails
//! stay private until they can no longer become a submitted secret.
use super::wire::{
    NativeField, NativeNotification as Event, NativeTurnFailureKind, NativeTurnOutcome,
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
            Event::CollabCallCompleted { prompt, .. } => {
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
            | Event::TokenUsage { .. } => {}
        }
        prefix.push(event);
        prefix
    }
}
