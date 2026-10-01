//! Origin-owned facts used to label Workspace paths on any Client platform.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The directory a Server puts the Worktrees it manages under. What it lays
/// out beneath that directory is Suru's own business rather than the reader's,
/// so a Worktree standing anywhere under it presents as its leaf name alone.
/// The Server that makes those Worktrees and the surfaces that present them
/// both answer for the same directory, so both name it from here.
pub const MANAGED_WORKTREE_DIRECTORY: &str = ".suru-worktrees";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStyle {
    Unix,
    Windows,
}

/// The owning Server's resolved home and path syntax. Paths stay strings here:
/// a Client's native `Path` parser cannot interpret another platform's roots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePaths {
    pub home: Option<String>,
    pub style: PathStyle,
}

impl WorkspacePaths {
    /// Resolve once on the owning machine, never against a Remote's paths on
    /// the Client. A home that cannot be resolved is unknown for abbreviation.
    pub fn discover() -> Self {
        Self::from_home(dirs::home_dir().as_deref())
    }

    /// Resolve an explicit home on this Server's filesystem. Keeping discovery
    /// outside this boundary also permits isolated home fixtures in tests.
    pub fn from_home(home: Option<&Path>) -> Self {
        Self {
            home: home
                .and_then(|home| crate::paths::canonical(home).ok())
                .and_then(|home| home.into_os_string().into_string().ok()),
            ..Self::default()
        }
    }

    pub fn label(&self, path: &Path) -> String {
        let spelled = self.spelling(&path.to_string_lossy());
        let separator = self.separator();
        if let Some(home) = self.home.as_ref().map(|home| self.spelling(home))
            && !home.is_empty()
            // Unresolved fallback paths do not establish containment.
            && !spelled.split(separator).any(|part| matches!(part, "." | ".."))
            && let Some(rest) = spelled.strip_prefix(home.trim_end_matches(separator))
            && (rest.is_empty() || rest.starts_with(separator))
        {
            return if rest.trim_matches(separator).is_empty() {
                "~".to_owned()
            } else {
                format!("~{rest}")
            };
        }
        spelled
    }

    /// A directory's name in its owning filesystem's syntax. Roots stand for
    /// themselves, using the same label as any other Workspace path.
    pub fn name(&self, path: &Path) -> String {
        let spelled = self.spelling(&path.to_string_lossy());
        let separator = self.separator();
        let trimmed = spelled.trim_end_matches(separator);
        let windows_root = self.style == PathStyle::Windows
            && ((trimmed.len() == 2 && trimmed.ends_with(':'))
                || trimmed
                    .strip_prefix(r"\\")
                    .is_some_and(|share| share.split(separator).count() == 2));
        if trimmed.is_empty() || windows_root {
            self.label(path)
        } else {
            trimmed
                .rsplit(separator)
                .next()
                .unwrap_or(trimmed)
                .to_owned()
        }
    }

    /// The name a Workspace goes by wherever Workspaces are listed: its
    /// presented root's [`Self::name`], said to be no checkout where Suru
    /// presents it by its Repository's metadata for want of a known main
    /// checkout.
    pub fn workspace_name(&self, workspace: &super::Workspace) -> String {
        let name = self.name(&workspace.path);
        if workspace.main_unknown() {
            format!("{name} (main checkout unknown)")
        } else {
            name
        }
    }

    /// Where a Worktree stands, said as shortly as it can be said without
    /// leaving the reader guessing which working copy they are looking at.
    ///
    /// A Worktree Suru made itself lives at any depth under a managed
    /// directory whose layout means nothing to the reader, so only its leaf
    /// name is shown. One standing inside `presented_root` — the path the
    /// Repository's Workspace is presented by — is said relative to that root,
    /// which the surface is already answering for. Anything else keeps its
    /// whole label, spelled in the owning Server's own syntax rather than the
    /// Client's.
    pub fn worktree_location(&self, root: &Path, presented_root: Option<&Path>) -> String {
        let separator = self.separator();
        let spelled = self.spelling(&root.to_string_lossy());
        let trimmed = spelled.trim_end_matches(separator);
        let mut ancestry = trimmed.split(separator).collect::<Vec<_>>();
        ancestry.pop();
        if ancestry.contains(&MANAGED_WORKTREE_DIRECTORY) {
            return self.name(root);
        }
        if let Some(presented_root) = presented_root {
            let presented_root = self.spelling(&presented_root.to_string_lossy());
            let presented_root = presented_root.trim_end_matches(separator);
            if !presented_root.is_empty()
                && let Some(rest) = trimmed.strip_prefix(presented_root)
                && let Some(rest) = rest.strip_prefix(separator)
                && !rest.is_empty()
            {
                return rest.to_owned();
            }
        }
        self.label(root)
    }

    /// A strict descendant's path relative to its root, in the owning
    /// Server's syntax. Equal or unrelated paths have no subdirectory label.
    pub fn subdirectory(&self, path: &Path, root: &Path) -> Option<String> {
        let separator = self.separator();
        let path = self.spelling(&path.to_string_lossy());
        let root = self.spelling(&root.to_string_lossy());
        if root.is_empty()
            || [&path, &root]
                .into_iter()
                .any(|path| path.split(separator).any(|part| matches!(part, "." | "..")))
        {
            return None;
        }
        let relative = path
            .trim_end_matches(separator)
            .strip_prefix(root.trim_end_matches(separator))?
            .strip_prefix(separator)?;
        (!relative.is_empty()).then(|| relative.to_owned())
    }

    fn separator(&self) -> char {
        match self.style {
            PathStyle::Unix => '/',
            PathStyle::Windows => '\\',
        }
    }

    fn spelling(&self, path: &str) -> String {
        match self.style {
            PathStyle::Unix => path.to_owned(),
            PathStyle::Windows => {
                let path = path.replace('/', "\\");
                if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
                    format!(r"\\{share}")
                } else {
                    path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
                }
            }
        }
    }
}

impl Default for WorkspacePaths {
    fn default() -> Self {
        Self {
            home: None,
            style: if cfg!(windows) {
                PathStyle::Windows
            } else {
                PathStyle::Unix
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PathStyle, WorkspacePaths};
    use std::path::Path;

    /// Both spellings are exercised on every platform: a Client labels a
    /// Remote's paths in that Server's syntax, never in its own.
    fn paths(style: PathStyle, home: &str) -> WorkspacePaths {
        WorkspacePaths {
            home: Some(home.to_owned()),
            style,
        }
    }

    #[test]
    fn subdirectories_use_the_owning_servers_syntax_and_require_containment() {
        for (style, root, path, expected) in [
            (PathStyle::Unix, "/repo/", "/repo/src/ui", Some("src/ui")),
            (PathStyle::Unix, "/", "/src", Some("src")),
            (PathStyle::Unix, "/repo", "/repo/", None),
            (PathStyle::Unix, "/repo", "/repo-other/src", None),
            (PathStyle::Unix, "/repo", "/repo/../src", None),
            (
                PathStyle::Windows,
                r"C:\repo",
                r"\\?\C:\repo\src\ui",
                Some(r"src\ui"),
            ),
            (PathStyle::Windows, r"C:\", r"C:\src", Some("src")),
            (
                PathStyle::Windows,
                r"C:\repo",
                "C:/repo/src/ui",
                Some(r"src\ui"),
            ),
            (
                PathStyle::Windows,
                r"\\host\share",
                r"\\?\UNC\host\share\src",
                Some("src"),
            ),
            (PathStyle::Windows, r"C:\repo", r"C:\repo", None),
            (PathStyle::Windows, r"C:\repo", r"D:\repo\src", None),
            (PathStyle::Windows, r"C:\repo", r"C:\repo-other\src", None),
        ] {
            assert_eq!(
                paths(style, "")
                    .subdirectory(Path::new(path), Path::new(root))
                    .as_deref(),
                expected,
                "{style:?}: {path} relative to {root}"
            );
        }
    }

    /// A managed Worktree presents as its leaf name at any depth under the
    /// managed directory, so both a Worktree standing directly beneath it and
    /// one standing further down are read the same way.
    #[test]
    fn managed_worktrees_present_as_their_leaf_name_in_either_syntax() {
        assert_eq!(
            paths(PathStyle::Unix, "/home/reader").worktree_location(
                Path::new("/home/reader/suru/.suru-worktrees/review-landing"),
                Some(Path::new("/home/reader/suru")),
            ),
            "review-landing"
        );
        assert_eq!(
            paths(PathStyle::Unix, "/home/reader").worktree_location(
                Path::new("/home/reader/suru/.suru-worktrees/nested/deeper/review-landing"),
                Some(Path::new("/home/reader/suru")),
            ),
            "review-landing"
        );
        assert_eq!(
            paths(PathStyle::Windows, r"C:\Users\reader").worktree_location(
                Path::new(r"C:\Users\reader\suru\.suru-worktrees\review-landing"),
                Some(Path::new(r"C:\Users\reader\suru")),
            ),
            "review-landing"
        );
        assert_eq!(
            paths(PathStyle::Windows, r"C:\Users\reader").worktree_location(
                Path::new(r"C:\Users\reader\suru\.suru-worktrees\nested\review-landing"),
                Some(Path::new(r"C:\Users\reader\suru")),
            ),
            "review-landing"
        );
    }

    #[test]
    fn worktrees_inside_the_presented_root_are_said_relative_to_it() {
        assert_eq!(
            paths(PathStyle::Unix, "/home/reader").worktree_location(
                Path::new("/home/reader/suru/trees/feature"),
                Some(Path::new("/home/reader/suru")),
            ),
            "trees/feature"
        );
        assert_eq!(
            paths(PathStyle::Windows, r"C:\Users\reader").worktree_location(
                Path::new(r"C:\Users\reader\suru\trees\feature"),
                Some(Path::new(r"C:\Users\reader\suru\")),
            ),
            r"trees\feature"
        );
    }

    #[test]
    fn the_presented_root_and_anything_outside_it_keep_their_whole_label() {
        let unix = paths(PathStyle::Unix, "/home/reader");
        let root = Path::new("/home/reader/suru");
        assert_eq!(unix.worktree_location(root, Some(root)), "~/suru");
        assert_eq!(
            unix.worktree_location(Path::new("/home/reader/elsewhere/feature"), Some(root)),
            "~/elsewhere/feature"
        );
        assert_eq!(
            unix.worktree_location(Path::new("/srv/feature"), Some(root)),
            "/srv/feature"
        );
        assert_eq!(
            unix.worktree_location(Path::new("/home/reader/suru-adjacent"), Some(root)),
            "~/suru-adjacent",
            "a sibling sharing the root's opening characters is not inside it"
        );
        let windows = paths(PathStyle::Windows, r"C:\Users\reader");
        let root = Path::new(r"C:\Users\reader\suru");
        assert_eq!(windows.worktree_location(root, Some(root)), r"~\suru");
        assert_eq!(
            windows.worktree_location(Path::new(r"D:\feature"), Some(root)),
            r"D:\feature"
        );
        assert_eq!(
            windows.worktree_location(Path::new(r"C:\Users\reader\suru\trees\feature"), None),
            r"~\suru\trees\feature",
            "a Workspace with no presented root has nothing to be relative to"
        );
    }

    #[test]
    fn a_workspace_goes_by_its_presented_roots_name_and_says_where_that_is_no_checkout() {
        use super::super::{
            Repository, RepositoryId, RepositoryLocation, SourceControlAvailability,
            SourceControlCapabilities, Workspace,
        };
        let unix = paths(PathStyle::Unix, "/home/reader");
        let mut workspace = Workspace::directory("/srv/atlas.git".into());
        assert_eq!(unix.workspace_name(&workspace), "atlas.git");
        workspace.repository = Some(Box::new(Repository {
            id: RepositoryId::from_metadata("git", &workspace.path),
            system: "git".to_owned(),
            metadata_directory: workspace.path.clone(),
            location: RepositoryLocation::UnknownMain,
            availability: SourceControlAvailability::Available,
            capabilities: SourceControlCapabilities::discovery_only(),
        }));
        assert_eq!(
            unix.workspace_name(&workspace),
            "atlas.git (main checkout unknown)"
        );
    }
}
