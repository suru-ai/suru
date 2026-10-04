//! The one reading of a directory Suru stores, compares, and hands to tools.

use std::{
    io,
    path::{Path, PathBuf},
};

/// The canonical reading of `path`, resolved through symlinks and relative
/// waypoints so that two ways of naming one directory cannot become two
/// Workspaces, two Repositories, or two Execution Directories.
///
/// This is deliberately not `std::fs::canonicalize`. On Windows that answers
/// an extended-length path — `\\?\C:\…` — and the verbatim prefix costs far
/// more than the display noise it resembles. Git accepts such a path to read
/// at, so Repository discovery succeeds against one, but refuses it as a
/// creation target: `git init` and `git worktree add` both fail with
/// `Invalid argument` on a path Suru had just discovered a Repository
/// through, which is enough to break Worktree preparation outright. A
/// [`WorkspaceId`](crate::protocol::WorkspaceId) is also hashed from the
/// path, so `\\?\C:\src\suru` and `C:\src\suru` would be two identities for
/// one directory, and every association keyed by one would be invisible to
/// the other.
///
/// The `dunce` crate answers the ordinary Win32 spelling wherever it can
/// express the same path, and keeps the verbatim prefix only where it is
/// genuinely required — beyond `MAX_PATH`, a reserved device name, some UNC
/// shapes — because there the prefix is the path rather than an embellishment
/// on it. On every other platform it is `std::fs::canonicalize` itself, so
/// Linux and macOS read exactly as before.
///
/// This does not make the display-side stripping in
/// [`WorkspacePaths`](crate::protocol::WorkspacePaths) redundant: a Remote
/// Windows Server may still send a genuinely verbatim path, and a Client
/// spells what it is given rather than what it can resolve locally.
pub fn canonical(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// Makes the directory `name` directly beneath `root`, which must already
/// exist, and answers its path; a directory already there is left as it is.
///
/// This is how a Server makes anything inside its state or data directory.
/// Those roots are made once, before a Server starts, and never again: a
/// root removed while the Server runs means its state is gone, and the
/// Server stops for it (see `server::election`) rather than bringing it back.
/// Recursive creation would make the root again in the moments before the
/// Server notices, and leave it behind once the Server has stopped; making
/// only the one directory beneath fails instead, as making it in a parent
/// that is not there always does.
pub(crate) fn create_dir_beneath(root: &Path, name: &str) -> io::Result<PathBuf> {
    let dir = root.join(name);
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(dir),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(dir),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_canonical_reading_resolves_the_waypoints_a_spelling_took() {
        let directory = tempfile::tempdir().expect("create a directory to read");
        std::fs::create_dir(directory.path().join("sub")).expect("create the waypoint");
        assert_eq!(
            super::canonical(directory.path().join("sub").join(".."))
                .expect("read the waypointed spelling"),
            super::canonical(directory.path()).expect("read the direct spelling"),
        );
    }

    /// An ordinary Windows directory reads as an ordinary Windows path. Were
    /// the verbatim prefix to come back, Git would refuse to create anything
    /// at the reading and one Workspace would answer to two identities.
    #[cfg(windows)]
    #[test]
    fn a_canonical_reading_carries_no_verbatim_prefix() {
        let directory = tempfile::tempdir().expect("create a directory to read");
        let canonical = super::canonical(directory.path()).expect("read the directory");
        assert!(
            !canonical.to_string_lossy().starts_with(r"\\?\"),
            "canonical reading kept a verbatim prefix: {}",
            canonical.display()
        );
    }

    /// A directory is made beneath a root that exists, and one already there
    /// is kept; beneath a root that is gone, nothing is made, the root least
    /// of all.
    #[test]
    fn a_directory_is_made_beneath_its_root_only_while_the_root_exists() {
        let root = tempfile::tempdir().expect("create the root");
        let made = super::create_dir_beneath(root.path(), "beneath").expect("make beneath");
        assert!(made.is_dir());
        super::create_dir_beneath(root.path(), "beneath").expect("keep what is there");

        let removed = root.path().join("removed");
        let error = super::create_dir_beneath(&removed, "beneath")
            .expect_err("nothing is made beneath a root that is gone");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!removed.exists(), "the root was made again");
    }
}
