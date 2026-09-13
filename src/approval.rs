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
