//! Discovery of the Workspaces directly within each Workspace Root (ADR
//! 0051): every child directory of a Root that is a Repository's main or
//! bare root is entered in the Workspace Registry, known by discovery under
//! that Root, so it is listed before any Session has begun there.
//!
//! Only a Root's immediate children are read, and of those only the ones
//! that are not hidden; nothing deeper is, so a nested or vendored
//! Repository stays out of the Registry. Each child is read through source
//! control discovery, which reads the Sidekick Workspace as a directory of
//! its own whatever it holds, and is kept only where it is the root its
//! Repository is presented by: a plain directory, a directory within a
//! Repository — as every child of a Root that is itself a Repository is —
//! and a linked Worktree of a Repository presented elsewhere are each left
//! alone, so the Root itself never becomes a Workspace and a linked Worktree
//! counts for its Repository's Workspace alone (ADR 0023). Discovery derives
//! no Icon and no Description: the first Session begun in a discovered
//! Workspace derives them as it would anywhere.
//!
//! It runs at start and when the Roots change, and whenever Workspaces are
//! listed, where a scan of the same Roots no older than the memo
//! [`super::ServerTimings::root_discovery_memo`] sets is reused instead.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt, stream};
use tokio::{sync::watch, time::Instant};

use crate::{
    protocol::{RepositoryLocation, ResolvedWorkspace, SettingsSnapshot, Workspace},
    sessions::SessionStore,
    source_control::{SourceControlService, is_directory, is_hidden},
};

/// How many of a Root's children are read from source control at once.
const CONCURRENT_READINGS: usize = 8;

/// Discovers the Repositories within this Server's Workspace Roots into its
/// Workspace Registry, one scan at a time.
#[derive(Clone)]
pub(crate) struct RootDiscovery {
    sessions: SessionStore,
    source_control: SourceControlService,
    settings: watch::Receiver<SettingsSnapshot>,
    /// The directory a Root beginning with `~` is read beneath.
    home: Option<PathBuf>,
    /// How long a listing reuses the last scan of the same Roots.
    memo: Duration,
    /// The last scan, held for as long as the next one runs, so scans never
    /// overlap and a listing waiting on one reuses what it found.
    last: Arc<tokio::sync::Mutex<Option<Scan>>>,
}

/// When a scan began, and of which Roots.
struct Scan {
    began: Instant,
    roots: Vec<String>,
}

impl RootDiscovery {
    pub(crate) fn new(
        sessions: SessionStore,
        source_control: SourceControlService,
        settings: watch::Receiver<SettingsSnapshot>,
        home: Option<PathBuf>,
        memo: Duration,
    ) -> Self {
        Self {
            sessions,
            source_control,
            settings,
            home,
            memo,
            last: Arc::default(),
        }
    }

    /// Discovers the Workspaces within the Roots, as a listing does: a scan
    /// of the Roots named now that began within the memo is reused rather
    /// than taken again, and one still running is waited on.
    pub(crate) async fn discover(&self) {
        let mut last = self.last.lock().await;
        let roots = self.roots();
        if last
            .as_ref()
            .is_some_and(|scan| scan.roots == roots && scan.began.elapsed() < self.memo)
        {
            return;
        }
        *last = Some(self.scan(roots).await);
    }

    /// Discovers the Workspaces within the Roots afresh, whatever was found
    /// before: at start, and once the Roots have changed.
    pub(crate) async fn rediscover(&self) {
        let mut last = self.last.lock().await;
        let roots = self.roots();
        *last = Some(self.scan(roots).await);
    }

    /// The Workspace Roots the Settings in force name.
    fn roots(&self) -> Vec<String> {
        self.settings.borrow().settings.workspace.roots.clone()
    }

    /// One scan of `roots`, entering every Repository root directly within
    /// each in the Registry. A Root that cannot be read is skipped, saying
    /// why in the Log, and never made.
    async fn scan(&self, roots: Vec<String>) -> Scan {
        let began = Instant::now();
        let mut discovered = 0usize;
        for root in &roots {
            let Some(directory) = expand(root, self.home.as_deref()) else {
                tracing::warn!(
                    root,
                    "a Workspace Root beginning with ~ names a home this Server does not \
                     know, so nothing within it was discovered"
                );
                continue;
            };
            let children = match tokio::task::spawn_blocking(move || child_directories(&directory))
                .await
            {
                Ok(Ok(children)) => children,
                Ok(Err(error)) => {
                    tracing::warn!(
                        root,
                        %error,
                        "a Workspace Root could not be read, so nothing within it was discovered"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::error!(root, %error, "reading a Workspace Root did not finish");
                    continue;
                }
            };
            let mut readings = stream::iter(children.into_iter().map(|child| async move {
                let reading = self.source_control.read_unremembered(&child).await;
                (child, reading)
            }))
            .buffer_unordered(CONCURRENT_READINGS);
            while let Some((child, reading)) = readings.next().await {
                if let Some(workspace) = rooted_at(&child, reading) {
                    self.sessions.enter_discovered(&workspace, &child, root);
                    discovered += 1;
                }
            }
        }
        tracing::debug!(
            roots = roots.len(),
            discovered,
            elapsed_ms = began.elapsed().as_millis(),
            "Workspace Roots discovered"
        );
        Scan { began, roots }
    }
}

/// The directory the Workspace Root `root` names: beneath `home` where it
/// begins with `~`, and as it is spelled where it is absolute. `None` for
/// one beginning with `~` where no home is known, or naming another user's.
fn expand(root: &str, home: Option<&Path>) -> Option<PathBuf> {
    match Path::new(root).strip_prefix("~") {
        Ok(beneath_home) => Some(home?.join(beneath_home)),
        Err(_) if root.starts_with('~') => None,
        Err(_) => Some(PathBuf::from(root)),
    }
}

/// The directories directly within `root` that discovery reads, each as the
/// Server reads paths: every child that is a directory, a symlink counting
/// as what it names, but those that are hidden and those whose names are not
/// Unicode, which no Workspace can be presented by.
fn child_directories(root: &Path) -> io::Result<Vec<PathBuf>> {
    let root = crate::paths::canonical(root)?;
    Ok(std::fs::read_dir(&root)?
        .flatten()
        .filter(is_directory)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !is_hidden(name, entry))
        })
        .filter_map(|entry| crate::paths::canonical(entry.path()).ok())
        .collect())
}

/// The Workspace `reading` found, where `directory` is the root its
/// Repository is presented by — main or bare — and nothing otherwise.
fn rooted_at(directory: &Path, reading: ResolvedWorkspace) -> Option<Workspace> {
    let repository = reading.workspace.repository.as_deref()?;
    match &repository.location {
        RepositoryLocation::Main { root } | RepositoryLocation::Bare { root }
            if root == directory =>
        {
            Some(reading.workspace)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path rooted as this platform roots absolute paths.
    fn absolute(components: &[&str]) -> PathBuf {
        components.iter().fold(
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            |path, component| path.join(component),
        )
    }

    #[test]
    fn a_roots_children_are_its_directories_but_the_hidden_ones() {
        let root = tempfile::tempdir().expect("create a Root");
        let root = crate::paths::canonical(root.path()).expect("read the Root canonically");
        for directory in ["atlas", "ledger", ".tooling", "notes/deeper"] {
            std::fs::create_dir_all(root.join(directory)).expect("create a child directory");
        }
        std::fs::write(root.join("README.md"), "Projects").expect("write a file");
        let elsewhere = tempfile::tempdir().expect("create a directory elsewhere");
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path(), root.join("linked"))
            .expect("link a directory elsewhere into the Root");

        let mut children = child_directories(&root).expect("read the Root");
        children.sort();

        let mut expected = vec![root.join("atlas"), root.join("ledger"), root.join("notes")];
        #[cfg(unix)]
        expected.push(crate::paths::canonical(elsewhere.path()).expect("read it canonically"));
        expected.sort();
        assert_eq!(
            children, expected,
            "each directory directly within the Root, a link read as what it names, but neither \
             a hidden directory, a file, nor anything deeper"
        );
    }

    #[test]
    fn a_root_that_is_not_there_cannot_be_read() {
        let home = tempfile::tempdir().expect("create a home");
        let missing = home.path().join("unplugged");
        assert!(child_directories(&missing).is_err());
        assert!(!missing.exists(), "reading a Root makes nothing");
    }

    #[test]
    fn a_root_beginning_with_a_tilde_is_read_beneath_the_home() {
        let home = absolute(&["home", "user"]);
        assert_eq!(expand("~", Some(&home)), Some(home.clone()));
        assert_eq!(
            expand(&format!("~{}src", std::path::MAIN_SEPARATOR), Some(&home)),
            Some(home.join("src"))
        );
        assert_eq!(expand("~/src", None), None, "no home is known");
        assert_eq!(
            expand("~someone/src", Some(&home)),
            None,
            "another user's home is not this Server's to read"
        );
        let projects = absolute(&["work", "projects"]);
        assert_eq!(
            expand(projects.to_str().expect("UTF-8"), Some(&home)),
            Some(projects.clone())
        );
    }
}
