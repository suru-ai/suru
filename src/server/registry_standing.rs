//! Keeping the Workspace Registry in step with the disk and the Workspace
//! Roots (ADR 0051). An entry stands while its directory can be read or a
//! Session belongs to it. Whenever Workspaces are listed, and once at start
//! once the Roots are discovered, the directory every entry is presented at
//! is read afresh, whatever way the entry came to be known:
//!
//! - one that can be read is listed as it is;
//! - one gone while the directory holding it can be read is dropped, its Icon
//!   and Description with it, so a folder the user deleted leaves no ghost —
//!   unless a Session belongs to it, which keeps it, listed as unavailable;
//! - one that cannot be read, or whose holder cannot be read — a drive not
//!   mounted, say — is kept and listed as unavailable, since nothing tells
//!   its absence from a deletion.
//!
//! An entry discovered under a Workspace Root is dropped with that Root,
//! unless a Session belongs to it: when the Roots change, and once at start
//! where the Config Document names the Roots whole, so a Root taken away by
//! hand while the Server was stopped is taken away too. One a Session
//! belongs to is kept, and is known by that Session from then on, so it
//! stands as any entry a Session made does. Roots are told apart as the
//! directories they name rather than by how the Setting spells them, so
//! `~/projects` spelled out in full takes nothing away; and a Config
//! Document that could not be read whole takes nothing away either, since
//! the Roots it leaves are no word of the user's.
//!
//! Every directory is read apart, on a thread of its own (see
//! [`super::reads_apart`]), so one on a mount gone quiet holds up neither
//! the rest nor the Server, and nothing waits on the readings longer than
//! [`super::ServerTimings::root_discovery_wait`]. An entry whose reading has
//! not answered by then — or whose reading begun earlier is still
//! outstanding — is listed as unavailable and never dropped.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt, stream};

use super::{
    reads_apart::{NotRead, ReadsApart},
    root_discovery,
};
use crate::{
    protocol::{WorkspaceId, WorkspaceUnavailability},
    sessions::SessionStore,
};

/// How many directories are read at once.
const CONCURRENT_READINGS: usize = 8;

/// The most readings outstanding at once: room for every reading at once
/// several times over, while directories that never answer hold no more
/// threads than this. A reading past it is not made, and its entry is
/// listed as unavailable until a later reading.
const READS_AT_ONCE: usize = 32;

/// What reading the directory an entry is presented at finds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reading {
    /// It can be read.
    Readable,
    /// It is gone, and the directory holding it can be read.
    Gone,
    /// It cannot be read, for the reason given.
    Unreadable(WorkspaceUnavailability),
}

/// How a directory is read for its standing: injectable, so a test can
/// script one that never answers.
pub(crate) type Probe = Arc<dyn Fn(&Path) -> Reading + Send + Sync>;

/// Brings the Workspace Registry in step with the disk and the Roots.
#[derive(Clone)]
pub(crate) struct RegistryStanding(Arc<Standing>);

struct Standing {
    sessions: SessionStore,
    /// The directory a Root beginning with `~` names a directory beneath.
    home: Option<PathBuf>,
    probe: Probe,
    reads: ReadsApart,
    /// The longest anything waits on the readings.
    wait: Duration,
    /// One check at a time, so two listings at once never find each other's
    /// reading of a directory outstanding and take it for one that does not
    /// answer.
    checking: tokio::sync::Mutex<()>,
    /// One change of the Roots taken in at a time, the one at start among
    /// them, so none decides on Roots another has changed since.
    roots_changing: tokio::sync::Mutex<()>,
}

impl RegistryStanding {
    pub(crate) fn new(sessions: SessionStore, home: Option<PathBuf>, wait: Duration) -> Self {
        Self::with_probe(sessions, home, wait, Arc::new(read_standing))
    }

    fn with_probe(
        sessions: SessionStore,
        home: Option<PathBuf>,
        wait: Duration,
        probe: Probe,
    ) -> Self {
        Self(Arc::new(Standing {
            sessions,
            home,
            probe,
            reads: ReadsApart::new("suru-workspace-standing-read", READS_AT_ONCE),
            wait,
            checking: tokio::sync::Mutex::new(()),
            roots_changing: tokio::sync::Mutex::new(()),
        }))
    }

    /// Drops what the disk no longer holds, as the module says, and answers
    /// why each entry left that cannot be opened now cannot be, by its
    /// identity.
    pub(crate) async fn check(&self) -> HashMap<WorkspaceId, WorkspaceUnavailability> {
        let _one_at_a_time = self.0.checking.lock().await;
        let entries = self.0.sessions.presented_entries();
        let directories = entries
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut read = HashMap::new();
        let readings = stream::iter(directories.into_iter().map(|directory| {
            let probe = self.0.probe.clone();
            let reading = directory.clone();
            async move {
                let read = self
                    .0
                    .reads
                    .read(&directory, move || async move { probe(&reading) })
                    .await;
                (directory, read)
            }
        }))
        .buffer_unordered(CONCURRENT_READINGS)
        .for_each(|(directory, reading)| {
            read.insert(directory, reading);
            std::future::ready(())
        });
        if tokio::time::timeout(self.0.wait, readings).await.is_err() {
            tracing::debug!("answered before every Workspace Registry entry was read");
        }
        entries
            .into_iter()
            .filter_map(|(id, path)| {
                let why = self.standing(&id, &path, read.get(&path))?;
                Some((id, why))
            })
            .collect()
    }

    /// Why the entry `id` presented at `path` cannot be opened, as `read`
    /// found it, dropping it where it is gone and no Session belongs to it;
    /// `None` where it can be opened, or no longer stands there.
    fn standing(
        &self,
        id: &WorkspaceId,
        path: &Path,
        read: Option<&Result<Reading, NotRead>>,
    ) -> Option<WorkspaceUnavailability> {
        match read {
            Some(Ok(Reading::Readable)) => None,
            Some(Ok(Reading::Gone)) => self
                .0
                .sessions
                .forget_gone(id, path)
                .then_some(WorkspaceUnavailability::Gone),
            Some(Ok(Reading::Unreadable(why))) => Some(*why),
            None | Some(Err(NotRead::Outstanding)) => Some(WorkspaceUnavailability::Unanswered),
            // No thread could be made to read it on, or too many readings
            // that never answered hold theirs: it has not answered either.
            Some(Err(not_read)) => {
                tracing::debug!(
                    workspace = %id.0,
                    path = %path.display(),
                    %not_read,
                    "a Workspace Registry entry's directory was not read"
                );
                Some(WorkspaceUnavailability::Unanswered)
            }
        }
    }
}

impl RegistryStanding {
    /// Drops what was discovered under a Workspace Root `before` named and
    /// `after` names no longer, as the module says: once the Roots have
    /// changed from `before` to `after`.
    pub(crate) async fn roots_changed(&self, before: &[String], after: &[String]) {
        let _one_at_a_time = self.0.roots_changing.lock().await;
        self.forget_removed(before, after).await;
    }

    /// Drops what was discovered under a Workspace Root that the Roots
    /// `named` reads no longer name, as the module says: once at start.
    /// `named` reads the Roots in force as this decides, and nothing where
    /// the Config Document did not name them whole.
    pub(crate) async fn roots_at_start(&self, named: impl FnOnce() -> Option<Vec<String>>) {
        let _one_at_a_time = self.0.roots_changing.lock().await;
        let Some(roots) = named() else {
            tracing::warn!(
                "the Workspace Roots were not read whole from the Config Document, so nothing \
                 discovered under a Root is dropped at start"
            );
            return;
        };
        self.forget_removed(&self.0.sessions.discovered_roots(), &roots)
            .await;
    }

    async fn forget_removed(&self, before: &[String], after: &[String]) {
        let recorded = self.0.sessions.discovered_roots();
        if recorded.is_empty() {
            return;
        }
        let named = self
            .resolve(before.iter().chain(after).chain(&recorded))
            .await;
        let same = |left: &String, right: &String| named[left].same(&named[right]);
        let removed = before
            .iter()
            .filter(|root| !after.iter().any(|kept| same(root, kept)))
            .collect::<Vec<_>>();
        let unrooted = recorded
            .iter()
            .filter(|root| {
                removed.iter().any(|gone| same(root, gone))
                    && !after.iter().any(|kept| same(root, kept))
            })
            .cloned()
            .collect::<HashSet<_>>();
        if !unrooted.is_empty() {
            self.0
                .sessions
                .forget_unrooted(|root| unrooted.contains(root));
        }
    }

    /// The directory each Root among `roots` names, by how it is spelled:
    /// as written, with `~` read beneath the home, and as the filesystem
    /// resolves it — read apart, and waited on no longer than the bound on
    /// waiting, so a Root on a mount gone quiet is told apart by the other
    /// two alone.
    async fn resolve<'a>(
        &self,
        roots: impl Iterator<Item = &'a String>,
    ) -> HashMap<String, NamedDirectory> {
        let mut named = roots
            .map(|root| {
                let written = root_discovery::expand(root, self.0.home.as_deref())
                    .map(|directory| directory.components().collect::<PathBuf>());
                (
                    root.clone(),
                    NamedDirectory {
                        spelled: root.clone(),
                        written,
                        resolved: None,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let written = named
            .iter()
            .filter_map(|(root, directory)| Some((root.clone(), directory.written.clone()?)))
            .collect::<Vec<_>>();
        let mut resolved = HashMap::new();
        let resolutions = stream::iter(written.into_iter().map(|(root, directory)| async move {
            let read = directory.clone();
            let resolution = self
                .0
                .reads
                .read(
                    &directory,
                    move || async move { crate::paths::canonical(&read) },
                )
                .await;
            (root, resolution)
        }))
        .buffer_unordered(CONCURRENT_READINGS)
        .for_each(|(root, resolution)| {
            if let Ok(Ok(directory)) = resolution {
                resolved.insert(root, directory);
            }
            std::future::ready(())
        });
        if tokio::time::timeout(self.0.wait, resolutions)
            .await
            .is_err()
        {
            tracing::debug!("told Workspace Roots apart before every one was resolved");
        }
        for (root, directory) in resolved {
            if let Some(named) = named.get_mut(&root) {
                named.resolved = Some(directory);
            }
        }
        named
    }
}

/// The directory a Workspace Root names, as far as it could be told.
struct NamedDirectory {
    /// The Root as the Setting spells it.
    spelled: String,
    /// The path it names, `~` read beneath the home and its separators made
    /// plain; `None` for one beneath a home this Server does not know.
    written: Option<PathBuf>,
    /// That path as the filesystem resolves it, links and all; `None` where
    /// it could not be resolved in time.
    resolved: Option<PathBuf>,
}

impl NamedDirectory {
    /// Whether `other` names the same directory: spelled alike, written
    /// alike, or resolved alike.
    fn same(&self, other: &Self) -> bool {
        self.spelled == other.spelled
            || self.written.is_some() && self.written == other.written
            || self.resolved.is_some() && self.resolved == other.resolved
    }
}

/// Reads `directory` for its standing: readable where it is a directory
/// whose entries can be listed, and otherwise gone — nothing there, or
/// something other than a directory — only where the directory holding it
/// can be read too, so an absence nothing tells from a deletion keeps it.
fn read_standing(directory: &Path) -> Reading {
    let gone = match std::fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {
            if std::fs::read_dir(directory).is_ok() {
                return Reading::Readable;
            }
            false
        }
        Ok(_) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    };
    let holder_readable = directory
        .parent()
        .is_some_and(|holder| std::fs::read_dir(holder).is_ok());
    match (holder_readable, gone) {
        (true, true) => Reading::Gone,
        (true, false) => Reading::Unreadable(WorkspaceUnavailability::Unreadable),
        (false, _) => Reading::Unreadable(WorkspaceUnavailability::HolderUnreadable),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::storage::{
        StorageRepository, StorageWriter, StoredWorkspace, WorkspacePresentation,
        WorkspaceProvenance,
    };

    /// How long a test waits for something it knows will happen.
    const SURELY: Duration = Duration::from_secs(10);

    /// Holds every thread that waits on it until it is opened, and opens
    /// once dropped, so no reader is left held when a test ends.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    struct OpensOnDrop(Arc<Gate>);

    impl Drop for OpensOnDrop {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// A path rooted as this platform roots absolute paths.
    fn absolute(components: &[&str]) -> PathBuf {
        components.iter().fold(
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            |path, component| path.join(component),
        )
    }

    /// An entry presented at `path` as a plain directory, known by its
    /// Description.
    fn entry(path: &Path) -> (WorkspaceId, StoredWorkspace) {
        (
            WorkspaceId::directory(path),
            StoredWorkspace {
                path: Some(path.to_owned()),
                presentation: Some(WorkspacePresentation::Directory),
                ..StoredWorkspace::known_by(WorkspaceProvenance::Description)
            },
        )
    }

    /// The paths the Registry lists.
    fn listed(sessions: &SessionStore) -> Vec<PathBuf> {
        let mut listed = sessions
            .listed_workspaces()
            .into_iter()
            .map(|workspace| workspace.path)
            .collect::<Vec<_>>();
        listed.sort();
        listed
    }

    /// An entry whose directory does not answer is listed as unavailable,
    /// at once and however often it is asked after while its reading is
    /// outstanding — read once meanwhile — and is never dropped; once its
    /// directory answers that it is gone, it follows the disk. An entry
    /// beside it is read and listed on its own.
    #[tokio::test]
    async fn an_entry_whose_directory_does_not_answer_is_unavailable_and_never_dropped() {
        let data_dir = tempfile::tempdir().expect("create a data directory");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open the Server's storage");
        let (_writer, storage) = StorageWriter::spawn(repository);
        let stalled = absolute(&["mnt", "quiet", "atlas"]);
        let readable = absolute(&["work", "ledger"]);
        let sessions = SessionStore::new(
            Default::default(),
            storage,
            Vec::new(),
            [entry(&stalled), entry(&readable)].into_iter().collect(),
        );
        let gate = Arc::new(Gate::default());
        let held = OpensOnDrop(gate.clone());
        let readings = Arc::new(AtomicUsize::new(0));
        let standing = RegistryStanding::with_probe(
            sessions.clone(),
            None,
            Duration::from_millis(20),
            Arc::new({
                let (gate, readings, stalled) = (gate.clone(), readings.clone(), stalled.clone());
                move |directory: &Path| {
                    if directory == stalled {
                        readings.fetch_add(1, Ordering::SeqCst);
                        gate.wait();
                        Reading::Gone
                    } else {
                        Reading::Readable
                    }
                }
            }),
        );

        for _ in 0..3 {
            let unavailable = tokio::time::timeout(SURELY, standing.check())
                .await
                .expect("a check answers within its bound");
            assert_eq!(
                unavailable,
                [(
                    WorkspaceId::directory(&stalled),
                    WorkspaceUnavailability::Unanswered
                )]
                .into_iter()
                .collect(),
                "only the entry whose directory does not answer is unavailable"
            );
            assert_eq!(
                listed(&sessions),
                [stalled.clone(), readable.clone()],
                "and it is never dropped"
            );
        }
        assert_eq!(
            readings.load(Ordering::SeqCst),
            1,
            "a directory that does not answer is read once while that read is outstanding"
        );

        drop(held);
        tokio::time::timeout(SURELY, async {
            while !standing.check().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the directory answers in time once it is let go");
        assert_eq!(
            listed(&sessions),
            [readable],
            "once its directory answers that it is gone, the entry follows the disk"
        );
    }

    #[test]
    fn a_directory_is_readable_gone_or_unreadable_as_it_and_its_holder_stand() {
        let home = tempfile::tempdir().expect("create a home");
        let atlas = home.path().join("atlas");
        std::fs::create_dir(&atlas).expect("create a directory");
        assert_eq!(read_standing(&atlas), Reading::Readable);

        let ledger = home.path().join("ledger");
        assert_eq!(
            read_standing(&ledger),
            Reading::Gone,
            "nothing stands where the holder can be read"
        );
        std::fs::write(&ledger, "not a directory").expect("write a file");
        assert_eq!(
            read_standing(&ledger),
            Reading::Gone,
            "a file stands where the directory was"
        );

        assert_eq!(
            read_standing(&home.path().join("unplugged").join("atlas")),
            Reading::Unreadable(WorkspaceUnavailability::HolderUnreadable),
            "nothing tells a holder not there from a drive not mounted"
        );
    }
}
