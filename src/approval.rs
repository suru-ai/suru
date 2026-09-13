//! Provider-neutral consent requests and the Decisions that settle them.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::ApprovalId;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub id: ApprovalId,
    pub subject: ApprovalSubject,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalSubject {
    Command {
        command: String,
        cwd: Option<PathBuf>,
        actions: Vec<CommandAction>,
    },
    FileChange {
        paths: Vec<PathBuf>,
        grant_root: Option<PathBuf>,
    },
    Read {
        path: PathBuf,
    },
    Network {
        host_or_url: String,
    },
    PermissionGrant {
        profile: serde_json::Value,
    },
    OtherTool {
        name: String,
        input: serde_json::Value,
    },
}

/// One best-effort semantic action parsed from a command line by its Provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAction {
    Read {
        command: String,
        name: String,
        path: PathBuf,
    },
    ListFiles {
        command: String,
        path: Option<PathBuf>,
    },
    Search {
        command: String,
        query: Option<String>,
        path: Option<PathBuf>,
    },
    Unknown {
        command: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accept,
    AcceptForSession,
    Decline,
    DeclineAndInterrupt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Pending,
    /// A Client won arbitration; Provider delivery is still in progress.
    Submitting,
    /// Provider rejected delivery without consuming the live request; explicit retry is safe.
    SubmissionRejected,
    Decided,
    Withdrawn,
    TurnEnded,
    Unavailable,
    DeliveryUncertain,
}

impl ApprovalOutcome {
    pub const fn is_answerable(self) -> bool {
        matches!(self, Self::Pending | Self::SubmissionRejected)
    }

    pub const fn is_live(self) -> bool {
        self.is_answerable() || matches!(self, Self::Submitting)
    }
}

impl Approval {
    /// Keeps the durable, user-visible copy of an Approval bounded without
    /// changing the Provider-owned request used to deliver a Decision. The
    /// typed subject remains intact; strings, collections, and JSON children
    /// are retained in order until the shared character budget is exhausted.
    pub(crate) fn into_bounded_history(mut self, max_chars: usize) -> (Self, bool) {
        let original = self.clone();
        let mut budget = CharacterBudget(max_chars);
        self.subject.retain_within(&mut budget);
        if let Some(reason) = &mut self.reason {
            budget.retain_string(reason);
        }
        let truncated = self != original;
        (self, truncated)
    }
}

struct CharacterBudget(usize);

impl CharacterBudget {
    fn charge(&mut self, chars: usize) {
        self.0 = self.0.saturating_sub(chars);
    }

    fn retain_string(&mut self, value: &mut String) {
        self.charge(2);
        let keep = value.chars().count().min(self.0);
        if keep < value.chars().count() {
            value.truncate(
                value
                    .char_indices()
                    .nth(keep)
                    .map_or(value.len(), |(at, _)| at),
            );
        }
        self.0 = self.0.saturating_sub(keep);
    }

    fn retain_path(&mut self, value: &mut PathBuf) {
        let mut rendered = value.to_string_lossy().into_owned();
        if rendered.chars().count().saturating_add(2) <= self.0 {
            self.charge(rendered.chars().count().saturating_add(2));
            return;
        }
        self.retain_string(&mut rendered);
        *value = PathBuf::from(rendered);
    }

    fn retain_json(&mut self, value: &mut serde_json::Value) {
        use serde_json::Value;
        match value {
            Value::Null => self.charge(4),
            Value::Bool(value) => self.charge(if *value { 4 } else { 5 }),
            Value::Number(value) => self.charge(value.to_string().chars().count()),
            Value::String(value) => self.retain_string(value),
            Value::Array(values) => {
                self.charge(2);
                let mut retained = 0;
                for value in values.iter_mut() {
                    if self.0 == 0 {
                        break;
                    }
                    if retained > 0 {
                        self.charge(1);
                    }
                    self.retain_json(value);
                    retained += 1;
                }
                values.truncate(retained);
            }
            Value::Object(values) => {
                self.charge(2);
                let source = std::mem::take(values);
                for (key, mut value) in source {
                    if self.0 == 0 {
                        break;
                    }
                    let entry_cost = key.chars().count().saturating_add(3);
                    if entry_cost > self.0 {
                        self.0 = 0;
                        break;
                    }
                    self.charge(entry_cost + usize::from(!values.is_empty()));
                    self.retain_json(&mut value);
                    values.insert(key, value);
                }
            }
        }
    }
}

impl ApprovalSubject {
    fn retain_within(&mut self, budget: &mut CharacterBudget) {
        match self {
            Self::Command {
                command,
                cwd,
                actions,
            } => {
                budget.retain_string(command);
                if let Some(cwd) = cwd {
                    budget.retain_path(cwd);
                }
                let mut retained = 0;
                for action in actions.iter_mut() {
                    if budget.0 < 64 {
                        break;
                    }
                    // Variant and field names remain even when every value is
                    // empty, so each retained action pays a conservative
                    // structural cost before its content.
                    budget.charge(64);
                    action.retain_within(budget);
                    retained += 1;
                }
                actions.truncate(retained);
            }
            Self::FileChange { paths, grant_root } => {
                let mut retained = 0;
                for path in paths.iter_mut() {
                    if budget.0 < 3 {
                        break;
                    }
                    budget.charge(1);
                    budget.retain_path(path);
                    retained += 1;
                }
                paths.truncate(retained);
                if let Some(root) = grant_root {
                    budget.retain_path(root);
                }
            }
            Self::Read { path } => budget.retain_path(path),
            Self::Network { host_or_url } => budget.retain_string(host_or_url),
            Self::PermissionGrant { profile } => budget.retain_json(profile),
            Self::OtherTool { name, input } => {
                budget.retain_string(name);
                budget.retain_json(input);
            }
        }
    }
}

impl CommandAction {
    fn retain_within(&mut self, budget: &mut CharacterBudget) {
        match self {
            Self::Read {
                command,
                name,
                path,
            } => {
                budget.retain_string(command);
                budget.retain_string(name);
                budget.retain_path(path);
            }
            Self::ListFiles { command, path } => {
                budget.retain_string(command);
                if let Some(path) = path {
                    budget.retain_path(path);
                }
            }
            Self::Search {
                command,
                query,
                path,
            } => {
                budget.retain_string(command);
                if let Some(query) = query {
                    budget.retain_string(query);
                }
                if let Some(path) = path {
                    budget.retain_path(path);
                }
            }
            Self::Unknown { command } => budget.retain_string(command),
        }
    }
}
