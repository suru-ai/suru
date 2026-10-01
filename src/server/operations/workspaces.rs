//! Naming a Workspace by a directory: the resolution the Workspace endpoint
//! performs for a Workspace no Session has been begun in yet, and a
//! Sidekick's `set_workspace_description` performs for a directory no listed
//! Workspace is presented at, so the two take the same Workspace for it.

use std::path::Path;

use super::SessionOperations;
use crate::protocol::Workspace;

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
}
