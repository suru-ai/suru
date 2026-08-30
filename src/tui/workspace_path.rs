//! The TUI's one reading of a path offered as a Workspace.

use std::path::{Path, PathBuf};

/// Why a path the reader offered cannot root a Workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspacePathRefusal {
    NoDirectoryThere,
    NotADirectory,
}

impl WorkspacePathRefusal {
    /// The refusal shown at whichever surface the reader offered the path.
    pub(super) const fn message(self) -> &'static str {
        match self {
            Self::NoDirectoryThere => "No directory there",
            Self::NotADirectory => "Not a directory",
        }
    }
}

/// Reads `named` the way every TUI surface reads a Workspace: into its
/// canonical spelling, and only when something there is a directory.
pub(super) fn read_workspace(named: &Path) -> Result<PathBuf, WorkspacePathRefusal> {
    let candidate =
        std::fs::canonicalize(named).map_err(|_| WorkspacePathRefusal::NoDirectoryThere)?;
    if !candidate.is_dir() {
        return Err(WorkspacePathRefusal::NotADirectory);
    }
    Ok(candidate)
}
