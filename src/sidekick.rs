//! The Sidekick Workspace: the one Workspace on a Server that Suru itself
//! owns, a `sidekick` directory beside the Server's own data (ADR 0042).
//!
//! It lives under the data root, so each Channel has its own as surely as it
//! has its own Sessions, and it is made the first time a Client asks for it,
//! with the data root's own permissions. Otherwise it is a Workspace like any
//! other: the user may keep files of their own there, and a Session begun in
//! it by any route is one of its Sessions. Being a top-level Session of it is
//! the whole of what makes a Session's Agent a Sidekick, so the Server reads
//! that off the Workspace a Session already stores — nothing is stamped on a
//! Session to say so, and the request that begins one is the same as for any
//! other Session.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::protocol::{Session, Workspace, WorkspaceId};

/// The directory beneath the data root that the Sidekick Workspace is.
const DIRECTORY: &str = "sidekick";

/// Where a Server's Sidekick Workspace is, and the Workspace identity its
/// Sessions are grouped under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SidekickWorkspace {
    root: PathBuf,
    id: WorkspaceId,
}

impl SidekickWorkspace {
    /// The Sidekick Workspace beside the data in `data_dir`, which must
    /// already exist: it is read canonically, as every Workspace's directory
    /// is, so its identity is the one a Session working there is grouped
    /// under however the data root was spelled. Nothing is made here.
    pub(crate) fn beside(data_dir: &Path) -> io::Result<Self> {
        let root = crate::paths::canonical(data_dir)?.join(DIRECTORY);
        Ok(Self {
            id: WorkspaceId::directory(&root),
            root,
        })
    }

    /// The Sidekick Workspace's directory, made first if it is not there yet
    /// — with the data root's own permissions, readable by the Server's user
    /// alone — and left as it stands, with whatever the user keeps in it, if
    /// it is.
    pub(crate) fn ensure(&self) -> anyhow::Result<&Path> {
        if !self.root.is_dir() {
            std::fs::create_dir_all(&self.root)?;
            crate::runtime::protect_current_user_directory(&self.root)?;
            tracing::info!(directory = %self.root.display(), "made the Sidekick Workspace");
        }
        Ok(&self.root)
    }

    /// Whether `workspace` is the Sidekick Workspace.
    pub(crate) fn holds(&self, workspace: &Workspace) -> bool {
        workspace.id == self.id
    }

    /// Whether `session`'s Agent is a Sidekick: a top-level Session of the
    /// Sidekick Workspace. A Subagent's Session is never one, wherever it
    /// works, so work a Sidekick delegates is no Sidekick of its own.
    pub(crate) fn is_sidekicks(&self, session: &Session) -> bool {
        !session.is_subagent() && self.holds(&session.workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidekick_workspace_is_a_directory_beneath_the_data_root_made_on_first_use() {
        let data = tempfile::tempdir().expect("create a data root");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let expected = crate::paths::canonical(data.path())
            .expect("read the data root")
            .join("sidekick");
        assert!(!expected.exists(), "nothing is made until it is asked for");

        assert_eq!(sidekick.ensure().expect("make it"), expected);
        assert!(expected.is_dir());
        std::fs::write(expected.join("AGENTS.md"), "Be brief.").expect("keep a file there");
        assert_eq!(sidekick.ensure().expect("find it again"), expected);
        assert!(
            expected.join("AGENTS.md").is_file(),
            "a second ask leaves what is kept there alone"
        );
        assert!(sidekick.holds(&Workspace::directory(expected)));
    }

    #[test]
    fn only_a_top_level_session_of_the_sidekick_workspace_is_a_sidekicks() {
        let data = tempfile::tempdir().expect("create a data root");
        let elsewhere = tempfile::tempdir().expect("create another Workspace");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let root = sidekick.ensure().expect("make it").to_owned();
        let session = |workspace: &Path, parent| Session {
            parent,
            ..Session::for_tests(Workspace::directory(workspace.to_owned()))
        };

        assert!(sidekick.is_sidekicks(&session(&root, None)));
        assert!(
            !sidekick.is_sidekicks(&session(&root, Some(crate::protocol::SessionId::new()))),
            "a Subagent working in the Sidekick Workspace is no Sidekick"
        );
        assert!(!sidekick.is_sidekicks(&session(
            &crate::paths::canonical(elsewhere.path()).expect("read the other Workspace"),
            None
        )));
        assert!(
            !sidekick.is_sidekicks(&session(&root.join("notes"), None)),
            "a directory within it is a Workspace of its own"
        );
    }
}
