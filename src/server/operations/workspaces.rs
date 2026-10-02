//! Naming a Workspace by a directory: the resolution the Workspace endpoint
//! performs for a Workspace no Session has been begun in yet, and a
//! Sidekick's `set_workspace_description` performs for a directory no listed
//! Workspace is presented at, so the two take the same Workspace for it — and
//! setting a Workspace's Description, which both perform through the one
//! operation here, so a Client's setting and a Sidekick's are judged alike.

use std::{fmt, path::Path};

use super::SessionOperations;
use crate::protocol::{Author, Workspace, WorkspaceDescription, WorkspaceId};
use crate::sessions::SetWorkspaceDescriptionError;

/// Why setting a Workspace's Description was refused.
#[derive(Debug)]
pub(crate) enum DescriptionRefusal {
    /// The store refused it, as it says.
    Store(SetWorkspaceDescriptionError),
    /// A Sidekick on a Peer asked to describe this Server's own Sidekick
    /// Workspace, which a Remote keeps to itself (ADR 0044).
    SidekickWorkspace,
}

impl fmt::Display for DescriptionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(refusal) => write!(formatter, "{refusal}"),
            Self::SidekickWorkspace => formatter.write_str(
                "The Workspace is the Sidekick Workspace, which a Sidekick on another machine may \
                 not describe.",
            ),
        }
    }
}

impl SessionOperations {
    /// The Workspace the directory `path` lies in, resolved as this server
    /// resolves any directory: read canonically, then through source
    /// control, so a directory within a Repository — one of its Worktrees
    /// included — names that Repository's Workspace. `None` where `path` is
    /// no absolute path of an existing directory, which names no Workspace.
    pub(crate) async fn workspace_at(&self, path: &Path) -> Option<Workspace> {
        if !path.is_absolute() || !path.is_dir() {
            return None;
        }
        let path = crate::paths::canonical(path).unwrap_or_else(|_| path.to_owned());
        Some(self.source_control.resolve(&path, None).await.workspace)
    }

    /// Sets the Description of the Workspace `workspace_id` to `text`, or
    /// clears it where `text` is blank, for `author` — the user where it
    /// names no one — answering the Description it carries now. A Workspace
    /// this server knows of no Session or row for is taken as the one its
    /// resolution of `presented_at` names, where it names `workspace_id`. A
    /// Sidekick on a Peer is refused this Server's own Sidekick Workspace.
    pub(crate) async fn describe_workspace(
        &self,
        workspace_id: &WorkspaceId,
        text: &str,
        presented_at: Option<&Path>,
        author: Option<&Author>,
    ) -> Result<Option<WorkspaceDescription>, DescriptionRefusal> {
        if matches!(author, Some(Author::PeerSidekick { .. }))
            && self.sidekick_workspace.is_named_by(workspace_id)
        {
            return Err(DescriptionRefusal::SidekickWorkspace);
        }
        let resolved = match presented_at {
            Some(path) if !self.sessions.knows_workspace(workspace_id) => {
                self.workspace_at(path).await
            }
            _ => None,
        };
        self.sessions
            .set_workspace_description(workspace_id, text, resolved.as_ref())
            .map_err(DescriptionRefusal::Store)
    }
}
