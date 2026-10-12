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

/// How many symlinks one after another a relocation of the Sidekick
/// Workspace is followed through before it is taken to lead nowhere, as a
/// platform gives up on a path leading through as many.
const RELOCATION_HOPS: usize = 40;

/// The Icon the Sidekick Workspace is named beside wherever it has none of
/// its own: a standing default rather than a stored one, so it is never
/// derived and a user's own choice still replaces it.
pub(crate) const ICON: &str = "md-robot";

/// Where a Server's Sidekick Workspace is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SidekickWorkspace {
    /// The data root it is beneath, which it is only ever made within.
    data_root: PathBuf,
    /// Where the data root says it is: the `sidekick` entry beneath it, which
    /// may itself be a symlink, or be kept in another case, until it is read.
    root: PathBuf,
}

impl SidekickWorkspace {
    /// The Sidekick Workspace beside the data in `data_dir`, which must
    /// already exist. Nothing is made here.
    pub(crate) fn beside(data_dir: &Path) -> io::Result<Self> {
        let data_root = crate::paths::canonical(data_dir)?;
        Ok(Self {
            root: data_root.join(DIRECTORY),
            data_root,
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
    /// stands, with whatever the user keeps in it, if it is. It is made only
    /// within the data root, never the data root with it.
    pub(crate) fn ensure(&self) -> anyhow::Result<PathBuf> {
        if !self.root.is_dir() {
            crate::paths::create_dir_beneath(&self.data_root, DIRECTORY)?;
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

    /// Whether `destination` — a directory whose parent is read
    /// canonically, though it need not be there itself — is where the
    /// Sidekick Workspace is kept, or would be made: its very directory, read
    /// canonically where something stands there; its entry directly beneath
    /// the data root; or, where the user relocated it by a symlink to a place
    /// nothing is at yet, that place (see [`Self::relocated_to`]). A
    /// filesystem that does not tell case apart — Windows' and macOS's, as
    /// they come — takes another case of a name for the same one, and one
    /// that composes characters may take the Kelvin sign for `K`, which
    /// leaves only case between it and `k`; so a name is read in any case on
    /// every platform, and nothing named so is taken for any other
    /// directory. Reads the filesystem, so a caller that must not wait on it
    /// reads it apart.
    pub(crate) fn is_kept_at(&self, destination: &Path) -> bool {
        is_named_as(destination, &self.root)
            || crate::paths::canonical(destination).is_ok_and(|read| read == self.directory())
            || self
                .relocated_to()
                .is_some_and(|place| is_named_as(destination, &place))
    }

    /// Whether `path`, read canonically, lies within the Sidekick
    /// Workspace's directory, or within the place it was relocated to where
    /// nothing is there yet. Reads the filesystem, as [`Self::is_kept_at`]
    /// does.
    pub(crate) fn encloses(&self, path: &Path) -> bool {
        path.starts_with(self.directory())
            || self
                .relocated_to()
                .is_some_and(|place| path.starts_with(place))
    }

    /// Where the Sidekick Workspace would appear, where its entry beneath
    /// the data root is a symlink — the user's relocation of it — leading to
    /// nothing there yet: the place it leads to, its parent read
    /// canonically and its last part as the link spells it, followed
    /// through each further symlink standing there; or, where a place's
    /// parent cannot be read at all, the place as the link spells it, which
    /// only that very spelling can make. `None` where the entry is no
    /// symlink, or leads somewhere there, which [`Self::directory`] reads.
    /// Each link is read as a link, never through, so one leading nowhere is
    /// read too.
    fn relocated_to(&self) -> Option<PathBuf> {
        let mut entry = self.root.clone();
        for _ in 0..RELOCATION_HOPS {
            if !std::fs::symlink_metadata(&entry)
                .ok()?
                .file_type()
                .is_symlink()
            {
                return None;
            }
            // A relative link leads from the directory it stands in.
            let leads_to = entry.parent()?.join(std::fs::read_link(&entry).ok()?);
            if crate::paths::canonical(&leads_to).is_ok() {
                return None;
            }
            let (Some(parent), Some(name)) = (leads_to.parent(), leads_to.file_name()) else {
                return Some(leads_to);
            };
            let Ok(parent) = crate::paths::canonical(parent) else {
                return Some(leads_to);
            };
            let place = parent.join(name);
            if !std::fs::symlink_metadata(&place).is_ok_and(|read| read.file_type().is_symlink()) {
                return Some(place);
            }
            entry = place;
        }
        None
    }

    /// Whether `workspace` is the Sidekick Workspace.
    pub(crate) fn holds(&self, workspace: &Workspace) -> bool {
        self.is_named_by(&workspace.id)
    }

    /// Whether the Workspace `id` names is the Sidekick Workspace.
    pub(crate) fn is_named_by(&self, id: &WorkspaceId) -> bool {
        *id == WorkspaceId::directory(&self.directory())
    }

    /// Whether `session`'s Agent is a Sidekick: a top-level Session of the
    /// Sidekick Workspace. A Subagent's Session is never one, wherever it
    /// works, so work a Sidekick delegates is no Sidekick of its own.
    pub(crate) fn is_sidekicks(&self, session: &Session) -> bool {
        !session.is_subagent() && self.holds(&session.workspace)
    }
}

/// Whether `destination` names the very entry `place` does: within the same
/// parent, by a name alike in any case.
fn is_named_as(destination: &Path, place: &Path) -> bool {
    destination.parent() == place.parent()
        && match (destination.file_name(), place.file_name()) {
            (Some(named), Some(placed)) => match (named.to_str(), placed.to_str()) {
                (Some(named), Some(placed)) => in_any_case(named) == in_any_case(placed),
                _ => named == placed,
            },
            _ => false,
        }
}

/// `name` as it reads in any case: upper-cased, then lower-cased, so a
/// letter that upper-cases to an ASCII one — the long s, the dotless i — or
/// lower-cases to one — the Kelvin sign — reads as that letter does.
fn in_any_case(name: &str) -> String {
    name.to_uppercase().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the Sidekick Workspace is kept, or would be made, is told by
    /// its name in any case — the Kelvin sign and the long s among the
    /// letters that read as ASCII ones — whether or not it has been made,
    /// and only directly beneath the data root; once it is made, its very
    /// directory is told however it is reached, and what lies within it is
    /// enclosed by it.
    #[test]
    fn the_sidekick_workspaces_place_is_told_by_its_name_in_any_case() {
        let data = tempfile::tempdir().expect("create a data root");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let data = crate::paths::canonical(data.path()).expect("read the data root");
        for name in [
            "sidekick",
            "SIDEKICK",
            "SideKick",
            "sidekic\u{212A}",
            "\u{17F}idekick",
            "s\u{131}dekick",
        ] {
            assert!(sidekick.is_kept_at(&data.join(name)), "{name:?}");
        }
        for elsewhere in [
            data.join("sidekicks"),
            data.join("other").join("sidekick"),
            data.join("sidekick").join("atlas"),
        ] {
            assert!(!sidekick.is_kept_at(&elsewhere), "{}", elsewhere.display());
        }

        let made = sidekick.ensure().expect("make it");
        std::fs::create_dir(data.join("other")).expect("create a waypoint");
        assert!(
            sidekick.is_kept_at(&data.join("other").join("..").join("sidekick")),
            "its very directory is told however it is reached"
        );
        assert!(sidekick.encloses(&made.join("atlas")));
        assert!(!sidekick.encloses(&data.join("atlas")));
    }

    #[test]
    fn the_sidekick_workspaces_icon_is_one_the_icon_catalog_carries() {
        assert!(crate::icon_catalog::glyph(ICON).is_some());
    }

    /// A Sidekick Workspace relocated by a symlink to a place not there yet
    /// is kept at that place — in any case, followed through further
    /// symlinks, however relative each is — so nothing made there becomes
    /// it, and what lies within it once it is there is enclosed by it; where
    /// even the place's parent cannot be read, only the very spelling the
    /// link gives is taken for it. Windows is left out: making a symlink
    /// there takes a privilege a test cannot count on, though links are read
    /// there alike.
    #[cfg(unix)]
    #[test]
    fn a_sidekick_workspace_relocated_to_nothing_yet_is_kept_where_its_symlink_leads() {
        use std::os::unix::fs::symlink;

        let data = tempfile::tempdir().expect("create a data root");
        let sidekick = SidekickWorkspace::beside(data.path()).expect("read the data root");
        let entry = crate::paths::canonical(data.path())
            .expect("read the data root")
            .join("sidekick");
        let elsewhere = tempfile::tempdir().expect("create a directory elsewhere");
        let projects = crate::paths::canonical(elsewhere.path()).expect("read the directory");
        let relocate = |to: &Path| {
            let _ = std::fs::remove_file(&entry);
            symlink(to, &entry).expect("relocate the Sidekick Workspace");
        };

        relocate(&projects.join("helper"));
        for name in ["helper", "HELPER"] {
            assert!(sidekick.is_kept_at(&projects.join(name)), "{name:?}");
        }
        assert!(!sidekick.is_kept_at(&projects.join("atlas")));
        assert!(!sidekick.encloses(&projects.join("atlas")));

        std::fs::create_dir(projects.join("archive")).expect("create a directory");
        symlink("archive/kept", projects.join("link")).expect("link onward");
        relocate(&projects.join("link"));
        assert!(
            sidekick.is_kept_at(&projects.join("archive").join("kept")),
            "followed through a further, relative, symlink"
        );

        relocate(Path::new("relocated"));
        assert!(
            sidekick.is_kept_at(&entry.with_file_name("relocated")),
            "a relative link leads from the data root"
        );

        let unreadable = projects.join("gone").join("helper");
        relocate(&unreadable);
        assert!(
            sidekick.is_kept_at(&unreadable),
            "where the place cannot be read, the very spelling the link gives"
        );
        assert!(!sidekick.is_kept_at(&projects.join("helper")));

        relocate(&projects.join("helper"));
        std::fs::create_dir(projects.join("helper")).expect("make the place");
        assert!(sidekick.is_kept_at(&projects.join("helper")));
        assert!(sidekick.encloses(&projects.join("helper").join("atlas")));
    }

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
