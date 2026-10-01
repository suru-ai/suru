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
//!
//! It is known by the directory it is, read as every Workspace's directory is
//! read: canonically, through a symlink and in the case the filesystem keeps,
//! so however a Session names it, it is the same Workspace. And it is a
//! directory outside source control even where the data root lies within a
//! Repository: source control reads it as a directory Workspace of its own
//! wherever it resolves a directory (see
//! [`SourceControlService::with_sidekick_workspace`]), so a Session begun
//! there, or regrouped after a restart, is grouped under it and no Repository.
//!
//! [`SourceControlService::with_sidekick_workspace`]: crate::source_control::SourceControlService::with_sidekick_workspace

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::protocol::{Session, Workspace, WorkspaceId};

/// The directory beneath the data root that the Sidekick Workspace is.
const DIRECTORY: &str = "sidekick";

/// Where a Server's Sidekick Workspace is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SidekickWorkspace {
    /// Where the data root says it is: the `sidekick` entry beneath it, which
    /// may itself be a symlink, or be kept in another case, until it is read.
    root: PathBuf,
}

impl SidekickWorkspace {
    /// The Sidekick Workspace beside the data in `data_dir`, which must
    /// already exist. Nothing is made here.
    pub(crate) fn beside(data_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            root: crate::paths::canonical(data_dir)?.join(DIRECTORY),
        })
    }

    /// The Sidekick Workspace's directory as every Workspace's directory is
    /// read — canonically, the whole of it, so a symlink names what it points
    /// at and a name the filesystem keeps in another case reads as it keeps it
    /// — or, while there is nothing there yet, where it would be made.
    pub(crate) fn directory(&self) -> PathBuf {
        crate::paths::canonical(&self.root).unwrap_or_else(|_| self.root.clone())
    }

    /// The Sidekick Workspace's directory, read as [`Self::directory`] reads
    /// it, made first if it is not there yet — with the data root's own
    /// permissions, readable by the Server's user alone — and left as it
    /// stands, with whatever the user keeps in it, if it is.
    pub(crate) fn ensure(&self) -> anyhow::Result<PathBuf> {
        if !self.root.is_dir() {
            std::fs::create_dir_all(&self.root)?;
            crate::runtime::protect_current_user_directory(&self.root)?;
            tracing::info!(directory = %self.root.display(), "made the Sidekick Workspace");
        }
        Ok(crate::paths::canonical(&self.root)?)
    }

    /// Whether `directory`, however it is spelled, is the Sidekick
    /// Workspace's.
    pub(crate) fn is_directory(&self, directory: &Path) -> bool {
        crate::paths::canonical(directory).unwrap_or_else(|_| directory.to_owned())
            == self.directory()
    }

    /// Whether `workspace` is the Sidekick Workspace.
    pub(crate) fn holds(&self, workspace: &Workspace) -> bool {
        workspace.id == WorkspaceId::directory(&self.directory())
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
        assert!(sidekick.is_directory(&expected.join("..").join("sidekick")));
        assert!(!sidekick.is_directory(data.path()));
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
        let root = sidekick.ensure().expect("make it");
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

    #[cfg(unix)]
    #[test]
    fn a_sidekick_entry_that_is_a_symlink_is_the_directory_it_names() {
        let data = tempfile::tempdir().expect("create a data root");
        let elsewhere = tempfile::tempdir().expect("create the named directory");
        std::os::unix::fs::symlink(elsewhere.path(), data.path().join("sidekick"))
            .expect("point the Sidekick Workspace elsewhere");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let named = crate::paths::canonical(elsewhere.path()).expect("read the named directory");

        assert_eq!(sidekick.ensure().expect("find it"), named);
        assert!(sidekick.holds(&Workspace::directory(named.clone())));
        assert!(sidekick.is_directory(&data.path().join("sidekick")));
        assert!(sidekick.is_directory(&named));
    }
}
