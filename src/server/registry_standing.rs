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
//! unless a Session belongs to it: when the Roots change, and once at start,
//! so a Root taken away by hand while the Server was stopped is taken away
//! too — each time, what no Root then in force holds. One a Session belongs
//! to is kept, and is known by that Session from then on, so it stands as
//! any entry a Session made does. Roots are told apart as the directories
//! they name rather than by how the Setting spells them (see
//! [`super::root_directories`]), and an entry as the directory discovery
//! found it presented directly within — the one its Root named then, but
//! for a linked Worktree's Repository. A Root in force holds the entry where
//! it names the same directory as the Root the entry was discovered under,
//! or the one it was found within, so `~/projects` spelled out in full
//! takes nothing away; one that resolves elsewhere holds it not, so a Root
//! on a drive not mounted takes what was discovered under it once taken
//! away, though it cannot be resolved; and one that can be told apart
//! neither way — nothing there for it either, as for an alias on that same
//! drive — may be the same directory, so the entry is kept until that can be
//! told, at the next start or change of the Roots. What a Root in force
//! holds is known by discovery under that Root's spelling from then on, and
//! so is what a Root a change named anew may hold, as the Root respelled
//! may: taking that spelling away later takes it. A Root naming Suru's own
//! data directory is no Root in force, so what was discovered under it goes
//! too. Only Roots the Config Document names whole — before a change and
//! after it alike — settle anything, since those a document left that could
//! not be read whole are no word of the user's. The Roots are held still
//! while either decides (see [`RootsHeld`]), and the step at start settles
//! nothing where Settings were put in force anew since it read them.
//!
//! Every directory is read apart, on a thread of its own (see
//! [`super::reads_apart`]), so one on a mount gone quiet holds up neither
//! the rest nor the Server, and what each reading finds is taken as it
//! answers. One check reads at a time, on a task of its own, and every
//! listing asking meanwhile joins it rather than reading again; none waits
//! on it longer than [`super::ServerTimings::root_discovery_wait`] from
//! when it asked, and the check itself gives up on its readings once that
//! long has passed since it began. An entry whose reading has not answered
//! by then — or whose reading begun earlier is still outstanding — is
//! listed as unavailable and never dropped; one entered since the check
//! began is no part of it, and is listed as it is.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use tokio::sync::watch;

use super::{
    reads_apart::{NotRead, ReadsApart},
    root_directories::{self, Likeness, NamedDirectory},
};
use crate::{
    protocol::{SettingsSnapshot, WorkspaceId, WorkspaceUnavailability},
    sessions::{Rerooting, SessionStore, Sighting},
};

/// How many directories are read at once.
const CONCURRENT_READINGS: usize = 8;

/// The most readings outstanding at once: room for every reading at once
/// several times over, while directories that never answer hold no more
/// threads than this. A reading past it is not made, and its entry is
/// listed as unavailable until a later reading.
const READS_AT_ONCE: usize = 32;

/// The most resolutions of the Roots outstanding at once: a Root past it is
/// told by its writing alone.
const RESOLUTIONS_AT_ONCE: usize = 16;

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
    /// Suru's own data directory, read canonically, which no Root in force
    /// names.
    data_root: Option<PathBuf>,
    probe: Probe,
    /// The readings of where each entry is presented.
    reads: ReadsApart,
    /// The resolutions of the Roots, apart from `reads` so a Root at which
    /// an entry is presented is never left unresolved for that entry's
    /// reading, nor that entry unread for the Root's resolution.
    resolutions: ReadsApart,
    /// The longest anything waits on the readings.
    wait: Duration,
    /// How far the check begun last has come, which every listing asking
    /// while it reads joins.
    in_flight: Mutex<Option<watch::Receiver<Progress>>>,
    /// What holds the Roots still (see [`RootsHeld`]).
    roots_changing: tokio::sync::Mutex<()>,
}

impl RegistryStanding {
    pub(crate) fn new(
        sessions: SessionStore,
        home: Option<PathBuf>,
        data_root: Option<PathBuf>,
        wait: Duration,
    ) -> Self {
        Self::with_probe(sessions, home, data_root, wait, Arc::new(read_standing))
    }

    fn with_probe(
        sessions: SessionStore,
        home: Option<PathBuf>,
        data_root: Option<PathBuf>,
        wait: Duration,
        probe: Probe,
    ) -> Self {
        Self(Arc::new(Standing {
            sessions,
            home,
            data_root,
            probe,
            reads: ReadsApart::new("suru-workspace-standing-read", READS_AT_ONCE),
            resolutions: ReadsApart::new("suru-workspace-root-resolution", RESOLUTIONS_AT_ONCE),
            wait,
            in_flight: Mutex::new(None),
            roots_changing: tokio::sync::Mutex::new(()),
        }))
    }

    /// Drops what the disk no longer holds, as the module says, and answers
    /// why each entry left that cannot be opened now cannot be, by its
    /// identity: as the check still reading finds, or one begun now, once
    /// it has read every entry or the bound on waiting has passed since
    /// this asked, whichever is sooner.
    pub(crate) async fn check(&self) -> HashMap<WorkspaceId, WorkspaceUnavailability> {
        let deadline = tokio::time::Instant::now() + self.0.wait;
        let mut progress = self.join_or_begin();
        let ended = tokio::time::timeout_at(deadline, progress.wait_for(|progress| progress.ended))
            .await
            .is_ok();
        if !ended {
            tracing::debug!("answered before every Workspace Registry entry was read");
        }
        progress.borrow().unavailable()
    }

    /// The check still reading, or one begun now where none is.
    fn join_or_begin(&self) -> watch::Receiver<Progress> {
        let mut in_flight = self.0.in_flight.lock().unwrap();
        if let Some(progress) = in_flight
            .as_ref()
            .filter(|progress| !progress.borrow().ended)
        {
            return progress.clone();
        }
        let (since, entries) = self.0.sessions.presented_entries();
        let (sender, progress) = watch::channel(Progress {
            unread: entries.iter().map(|(id, _)| id.clone()).collect(),
            ..Progress::default()
        });
        *in_flight = Some(progress.clone());
        tokio::spawn(self.clone().read(since, entries, sender));
        progress
    }

    /// Reads where each of `entries`, taken at `since`, is presented, taking
    /// what each reading finds as it answers, until every one has or the
    /// bound on waiting has passed.
    async fn read(
        self,
        since: Sighting,
        entries: Vec<(WorkspaceId, PathBuf)>,
        progress: watch::Sender<Progress>,
    ) {
        let mut presented = BTreeMap::<PathBuf, Vec<WorkspaceId>>::new();
        for (id, path) in entries {
            presented.entry(path).or_default().push(id);
        }
        let directories = presented.keys().cloned().collect::<Vec<_>>();
        let readings = stream::iter(directories.into_iter().map(|directory| {
            let probe = self.0.probe.clone();
            let reading = directory.clone();
            let reads = &self.0.reads;
            async move {
                let read = reads
                    .read(&directory, move || async move { probe(&reading) })
                    .await;
                (directory, read)
            }
        }))
        .buffer_unordered(CONCURRENT_READINGS)
        .for_each(|(directory, read)| {
            for id in &presented[&directory] {
                let why = self.standing(id, &directory, since, &read);
                progress.send_modify(|progress| {
                    progress.unread.remove(id);
                    if let Some(why) = why {
                        progress.unavailable.insert(id.clone(), why);
                    }
                });
            }
            std::future::ready(())
        });
        if tokio::time::timeout(self.0.wait, readings).await.is_err() {
            tracing::debug!("gave up on reading every Workspace Registry entry");
        }
        progress.send_modify(|progress| {
            let unread = std::mem::take(&mut progress.unread);
            progress.unavailable.extend(
                unread
                    .into_iter()
                    .map(|id| (id, WorkspaceUnavailability::Unanswered)),
            );
            progress.ended = true;
        });
    }

    /// Why the entry `id` presented at `path` cannot be opened, as `read`,
    /// begun after `since`, found it, dropping it where it is gone and
    /// neither seen since nor a Session's; `None` where it can be opened, or
    /// no longer stands as read.
    fn standing(
        &self,
        id: &WorkspaceId,
        path: &Path,
        since: Sighting,
        read: &Result<Reading, NotRead>,
    ) -> Option<WorkspaceUnavailability> {
        match read {
            Ok(Reading::Readable) => None,
            Ok(Reading::Gone) => self
                .0
                .sessions
                .forget_gone(id, path, since)
                .then_some(WorkspaceUnavailability::Gone),
            Ok(Reading::Unreadable(why)) => Some(*why),
            Err(NotRead::Outstanding) => Some(WorkspaceUnavailability::Unanswered),
            // No thread could be made to read it on, or too many readings
            // that never answered hold theirs: it has not answered either.
            Err(not_read) => {
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

/// Where the step at start reads the Workspace Roots in force, and learns
/// whether Settings have been put in force anew since.
pub(crate) trait RootsInForce {
    /// The Roots in force, where the Config Document names them whole, and
    /// `None` where it does not.
    fn read(&mut self) -> Option<Vec<String>>;

    /// Whether Settings have been put in force anew since they were read.
    fn moved(&self) -> bool;
}

impl RootsInForce for watch::Receiver<SettingsSnapshot> {
    fn read(&mut self) -> Option<Vec<String>> {
        crate::settings::workspace_roots_named_whole(&self.borrow_and_update())
            .map(<[String]>::to_vec)
    }

    fn moved(&self) -> bool {
        self.has_changed().unwrap_or(true)
    }
}

/// The Workspace Roots held still: no change of them is put in force, nor
/// anything taken away on their account, but by whoever holds this.
pub(crate) struct RootsHeld<'a> {
    _still: tokio::sync::MutexGuard<'a, ()>,
}

impl RegistryStanding {
    /// Holds the Roots still, once whoever held them last lets go. A change
    /// of the Settings holds them from before it reads the Roots in force
    /// until it has put its own in force and taken away what they no longer
    /// hold, so the step at start never decides on Roots a change puts in
    /// force meanwhile.
    pub(crate) async fn hold_roots(&self) -> RootsHeld<'_> {
        RootsHeld {
            _still: self.0.roots_changing.lock().await,
        }
    }

    /// Drops what was discovered under a Workspace Root that no Root in
    /// `after` holds, and knows what another spelling holds by that
    /// spelling, as the module says: once the Roots have changed from
    /// `before` to `after`, while `_held` holds them still.
    pub(crate) async fn roots_changed(
        &self,
        _held: &RootsHeld<'_>,
        before: &[String],
        after: &[String],
    ) {
        self.reroot(after, Some(before), || true).await;
    }

    /// Drops what was discovered under a Workspace Root that no Root in
    /// force holds, and knows what another spelling holds by that spelling,
    /// as the module says: once at start, holding the Roots still
    /// throughout. The Roots are read from `in_force` as this decides;
    /// nothing is settled where the Config Document did not name them whole,
    /// nor where Settings were put in force anew before this could settle
    /// it, since what it read no longer says what the user named.
    pub(crate) async fn roots_at_start(&self, mut in_force: impl RootsInForce) {
        let _held = self.hold_roots().await;
        let Some(roots) = in_force.read() else {
            tracing::warn!(
                "the Workspace Roots were not read whole from the Config Document, so nothing \
                 discovered under a Root is dropped at start"
            );
            return;
        };
        let still = || {
            let moved = in_force.moved();
            if moved {
                tracing::info!(
                    "Settings were put in force while the Workspace Roots were read at start, so \
                     nothing discovered under a Root is dropped then"
                );
            }
            !moved
        };
        self.reroot(&roots, None, still).await;
    }

    /// Settles what was discovered under a Root that `roots`, the Roots in
    /// force, no longer spell — so long as `still` says, as it comes to
    /// that, that they are what the user named: each entry no Root in force
    /// holds is dropped, and each one does is known by discovery under that
    /// Root from then on (see [`rerooting`]). `before` holds the Roots a
    /// change took them from, so a Root it named anew may be one it took
    /// away, respelled. A Root naming Suru's own data directory is none of
    /// the Roots in force.
    async fn reroot(&self, roots: &[String], before: Option<&[String]>, still: impl Fn() -> bool) {
        let discovered = self.0.sessions.discovered_entries();
        if discovered.is_empty() {
            return;
        }
        let recorded = discovered
            .iter()
            .map(|entry| &entry.root)
            .filter(|root| !roots.contains(root))
            .collect::<BTreeSet<_>>();
        let named = root_directories::resolve(
            &self.0.resolutions,
            self.0.home.as_deref(),
            self.0.wait,
            roots.iter().chain(recorded),
        )
        .await;
        let in_force = roots
            .iter()
            .filter(|root| {
                !self
                    .0
                    .data_root
                    .as_deref()
                    .is_some_and(|data_root| named[*root].names(data_root))
            })
            .map(|root| HeldBy {
                spelled: root,
                named: &named[root],
                named_anew: before.is_some_and(|before| !before.contains(root)),
            })
            .collect::<Vec<_>>();
        let rerooted = discovered
            .into_iter()
            .filter(|entry| !in_force.iter().any(|root| *root.spelled == entry.root))
            .filter_map(|entry| {
                let holder = entry.path.as_deref().and_then(Path::parent);
                let rerooting = rerooting(&named[&entry.root], holder, &in_force)?;
                Some((entry.id, (entry.root, rerooting)))
            })
            .collect::<HashMap<_, _>>();
        if !rerooted.is_empty() && still() {
            self.0.sessions.reroot_discovered(rerooted);
        }
    }
}

/// A Root in force, as [`rerooting`] weighs whether it holds an entry.
struct HeldBy<'a> {
    spelled: &'a String,
    named: &'a NamedDirectory,
    /// Whether the change that put it in force named it anew.
    named_anew: bool,
}

/// What becomes of an entry discovered under the Root `discovered` names, a
/// spelling no Root `in_force` has, and found directly within `holder` — the
/// directory that Root named then, as discovery read it. A Root in force
/// surely holds it where it is the same directory, or names `holder`, and
/// the entry is known by discovery under that Root from then on. One that
/// resolves elsewhere is told apart from it: it was found where that Root
/// does not lead. One that can be told apart neither way — nothing there for
/// either, as on a drive not mounted, or one beneath a home not known — may
/// be the same directory, so the entry is kept as it is and told again at
/// the next start or change of the Roots; but where the change named that
/// Root anew, as the Root respelled may be, the entry is known by it from
/// then on. Where no Root in force holds the entry, nor may, it goes with
/// the Root it was discovered under. `None` where it is kept as it is.
fn rerooting(
    discovered: &NamedDirectory,
    holder: Option<&Path>,
    in_force: &[HeldBy<'_>],
) -> Option<Rerooting> {
    let mut may_hold = false;
    let mut respelled = None;
    for root in in_force {
        let likeness = discovered.likeness(root.named);
        if likeness == Likeness::Same || holder.is_some_and(|holder| root.named.names(holder)) {
            return Some(Rerooting::Respelled(root.spelled.clone()));
        }
        // A Root that resolves is told apart by where the entry was found,
        // where that is known.
        if likeness == Likeness::Untold && (holder.is_none() || !root.named.resolves()) {
            may_hold = true;
            if root.named_anew && respelled.is_none() {
                respelled = Some(Rerooting::Respelled(root.spelled.clone()));
            }
        }
    }
    respelled.or((!may_hold).then_some(Rerooting::Unrooted))
}

/// How far one check has come, as every listing joining it reads it.
#[derive(Default)]
struct Progress {
    /// The entries it reads whose reading has not yet been taken.
    unread: HashSet<WorkspaceId>,
    /// Why each entry read so far cannot be opened.
    unavailable: HashMap<WorkspaceId, WorkspaceUnavailability>,
    /// Whether it has ended, every entry it reads read or given up on.
    ended: bool,
}

impl Progress {
    /// Why each entry cannot be opened as far as the check has come: one
    /// whose reading has not been taken has not answered in time.
    fn unavailable(&self) -> HashMap<WorkspaceId, WorkspaceUnavailability> {
        let mut unavailable = self.unavailable.clone();
        unavailable.extend(
            self.unread
                .iter()
                .map(|id| (id.clone(), WorkspaceUnavailability::Unanswered)),
        );
        unavailable
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

    /// A Registry holding an entry presented at each of `paths`, with the
    /// storage it writes to, which lasts as long as it does.
    struct Registry {
        sessions: SessionStore,
        _writer: StorageWriter,
        _data_dir: tempfile::TempDir,
    }

    impl Registry {
        async fn holding(paths: &[&Path]) -> Self {
            Self::of(paths.iter().map(|path| entry(path)).collect()).await
        }

        async fn of(entries: HashMap<WorkspaceId, StoredWorkspace>) -> Self {
            let data_dir = tempfile::tempdir().expect("create a data directory");
            let repository = StorageRepository::open(data_dir.path())
                .await
                .expect("open the Server's storage");
            let (writer, storage) = StorageWriter::spawn(repository);
            Self {
                sessions: SessionStore::new(Default::default(), storage, Vec::new(), entries),
                _writer: writer,
                _data_dir: data_dir,
            }
        }
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
        let stalled = absolute(&["mnt", "quiet", "atlas"]);
        let readable = absolute(&["work", "ledger"]);
        let registry = Registry::holding(&[&stalled, &readable]).await;
        let sessions = &registry.sessions;
        let gate = Arc::new(Gate::default());
        let held = OpensOnDrop(gate.clone());
        let readings = Arc::new(AtomicUsize::new(0));
        let standing = RegistryStanding::with_probe(
            sessions.clone(),
            None,
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
                listed(sessions),
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
            listed(sessions),
            [readable],
            "once its directory answers that it is gone, the entry follows the disk"
        );
    }

    /// A reading that found a directory gone drops nothing seen there since
    /// it began: here the Repository is moved away and read gone, then
    /// moved back and discovered again before that reading is taken. A
    /// reading begun after it was last seen still drops it.
    #[tokio::test]
    async fn a_reading_that_found_a_directory_gone_drops_nothing_seen_there_since_it_began() {
        let atlas = absolute(&["work", "atlas"]);
        let registry = Registry::holding(&[&atlas]).await;
        let gate = Arc::new(Gate::default());
        let held = OpensOnDrop(gate.clone());
        let readings = Arc::new(AtomicUsize::new(0));
        let standing = RegistryStanding::with_probe(
            registry.sessions.clone(),
            None,
            None,
            SURELY,
            Arc::new({
                let (gate, readings) = (gate.clone(), readings.clone());
                move |_: &Path| {
                    readings.fetch_add(1, Ordering::SeqCst);
                    gate.wait();
                    Reading::Gone
                }
            }),
        );

        let listing = tokio::spawn({
            let standing = standing.clone();
            async move { standing.check().await }
        });
        tokio::time::timeout(SURELY, async {
            while readings.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the directory is read");
        registry.sessions.enter_discovered(
            &crate::protocol::Workspace::directory(atlas.clone()),
            &atlas,
            "~/work",
        );
        drop(held);

        assert_eq!(
            tokio::time::timeout(SURELY, listing)
                .await
                .expect("the listing answers once the reading does")
                .expect("the listing runs to its end"),
            HashMap::new(),
            "the entry seen since the reading began is listed as it was seen"
        );
        assert_eq!(
            listed(&registry.sessions),
            std::slice::from_ref(&atlas),
            "and kept"
        );

        tokio::time::timeout(SURELY, standing.check())
            .await
            .expect("a later listing answers");
        assert_eq!(
            listed(&registry.sessions),
            Vec::<PathBuf>::new(),
            "a reading begun after it was last seen drops it"
        );
    }

    /// Listings asking while a check reads join it rather than reading
    /// again, so each directory is read once however many ask at once.
    #[tokio::test]
    async fn listings_asking_at_once_join_one_check_and_read_each_directory_once() {
        let atlas = absolute(&["work", "atlas"]);
        let registry = Registry::holding(&[&atlas]).await;
        let gate = Arc::new(Gate::default());
        let _held = OpensOnDrop(gate.clone());
        let readings = Arc::new(AtomicUsize::new(0));
        let standing = RegistryStanding::with_probe(
            registry.sessions.clone(),
            None,
            None,
            SURELY,
            Arc::new({
                let (gate, readings) = (gate.clone(), readings.clone());
                move |_: &Path| {
                    readings.fetch_add(1, Ordering::SeqCst);
                    gate.wait();
                    Reading::Readable
                }
            }),
        );

        let listings = futures_util::future::join_all((0..5).map(|_| standing.check()));
        let answer = async {
            while readings.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            gate.open();
        };
        let (answered, ()) = tokio::time::timeout(SURELY, async { tokio::join!(listings, answer) })
            .await
            .expect("every listing answers once the directory does");

        assert!(
            answered.iter().all(HashMap::is_empty),
            "each listing is answered what the one reading found: {answered:?}"
        );
        assert_eq!(
            readings.load(Ordering::SeqCst),
            1,
            "the directory is read once for them all"
        );
    }

    /// Lets the readings that wait on it through one at a time, in the order
    /// they began, and every one once dropped.
    #[derive(Default)]
    struct Turnstile {
        let_through: Mutex<usize>,
        moved: Condvar,
    }

    impl Turnstile {
        /// Waits until the `nth` reading, counting from one, is let through.
        fn pass(&self, nth: usize) {
            let mut let_through = self.let_through.lock().unwrap();
            while *let_through < nth {
                let_through = self.moved.wait(let_through).unwrap();
            }
        }

        fn let_one_through(&self) {
            *self.let_through.lock().unwrap() += 1;
            self.moved.notify_all();
        }
    }

    struct LetsAllThroughOnDrop(Arc<Turnstile>);

    impl Drop for LetsAllThroughOnDrop {
        fn drop(&mut self) {
            *self.0.let_through.lock().unwrap() = usize::MAX;
            self.0.moved.notify_all();
        }
    }

    /// However many listings ask at once, none waits on the readings longer
    /// than its bound from when it asked, queueing included: here the one
    /// reading the directory gets answers just inside the bound, and any
    /// reading after it would not answer at all.
    #[tokio::test(start_paused = true)]
    async fn no_listing_waits_longer_than_its_bound_however_many_ask_at_once() {
        let atlas = absolute(&["work", "atlas"]);
        let registry = Registry::holding(&[&atlas]).await;
        let turnstile = Arc::new(Turnstile::default());
        let _held = LetsAllThroughOnDrop(turnstile.clone());
        let readings = Arc::new(AtomicUsize::new(0));
        let wait = Duration::from_secs(1);
        let standing = RegistryStanding::with_probe(
            registry.sessions.clone(),
            None,
            None,
            wait,
            Arc::new({
                let (turnstile, readings) = (turnstile.clone(), readings.clone());
                move |_: &Path| {
                    turnstile.pass(readings.fetch_add(1, Ordering::SeqCst) + 1);
                    Reading::Readable
                }
            }),
        );

        let listings = tokio::spawn({
            let standing = standing.clone();
            async move {
                futures_util::future::join_all((0..10).map(|_| async {
                    let asked = tokio::time::Instant::now();
                    standing.check().await;
                    asked.elapsed()
                }))
                .await
            }
        });
        tokio::time::sleep(wait * 9 / 10).await;
        turnstile.let_one_through();

        for waited in listings.await.expect("the listings run to their end") {
            assert!(waited <= wait, "a listing waited {waited:?}, past {wait:?}");
        }
    }

    /// The Roots in force as a test scripts them: what reading them
    /// answers, worked out as they are read, and whether Settings have been
    /// put in force anew since.
    struct ScriptedRoots<Read> {
        read: Option<Read>,
        moved: Arc<std::sync::atomic::AtomicBool>,
    }

    impl<Read: FnOnce() -> Option<Vec<String>>> RootsInForce for ScriptedRoots<Read> {
        fn read(&mut self) -> Option<Vec<String>> {
            (self.read.take().expect("the Roots are read once"))()
        }

        fn moved(&self) -> bool {
            self.moved.load(Ordering::SeqCst)
        }
    }

    /// A Registry holding one entry, at `b/atlas`, discovered under the Root
    /// `~/b`, which no Session belongs to.
    async fn discovered_under_b() -> (Registry, PathBuf) {
        let atlas = absolute(&["b", "atlas"]);
        let registry = Registry::of(
            [(
                WorkspaceId::directory(&atlas),
                StoredWorkspace {
                    path: Some(atlas.clone()),
                    presentation: Some(WorkspacePresentation::Directory),
                    ..StoredWorkspace::known_by(WorkspaceProvenance::Discovered {
                        root: "~/b".to_owned(),
                    })
                },
            )]
            .into_iter()
            .collect(),
        )
        .await;
        (registry, atlas)
    }

    /// Settings put in force while the step at start decides, by whatever
    /// way, leave it taking nothing away: what it read no longer says what
    /// the user named. Here a change puts `~/b` back in force just after
    /// the step read Roots without it.
    #[tokio::test]
    async fn settings_put_in_force_while_the_step_at_start_decides_leave_it_taking_nothing() {
        let (registry, atlas) = discovered_under_b().await;
        let standing = RegistryStanding::new(registry.sessions.clone(), None, None, SURELY);
        let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));

        standing
            .roots_at_start(ScriptedRoots {
                read: Some({
                    let moved = moved.clone();
                    move || {
                        moved.store(true, Ordering::SeqCst);
                        Some(Vec::new())
                    }
                }),
                moved,
            })
            .await;

        assert_eq!(
            listed(&registry.sessions),
            [atlas],
            "nothing is taken away on Roots no longer in force"
        );

        standing
            .roots_at_start(ScriptedRoots {
                read: Some(|| Some(Vec::new())),
                moved: Arc::default(),
            })
            .await;
        assert_eq!(
            listed(&registry.sessions),
            Vec::<PathBuf>::new(),
            "while Roots still in force as the step decides take away what they no longer hold"
        );
    }

    /// A change of the Roots cannot put Roots in force while the step at
    /// start decides on the ones it read. Here a change putting back a Root
    /// the Config Document had left out asks to, just as that step reads
    /// the Roots without it, and is held off until the step has taken away
    /// what it decided to: never does the step decide on Roots the change
    /// had already put in force.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_change_of_the_roots_waits_for_the_step_at_start_to_decide() {
        let (registry, _) = discovered_under_b().await;
        let standing = RegistryStanding::new(registry.sessions.clone(), None, None, SURELY);
        let change = Arc::new(Mutex::new(None::<std::thread::JoinHandle<Vec<PathBuf>>>));
        let runtime = tokio::runtime::Handle::current();

        standing
            .roots_at_start(ScriptedRoots {
                read: Some({
                    let (standing, sessions, change) =
                        (standing.clone(), registry.sessions.clone(), change.clone());
                    move || {
                        let putting_back = std::thread::spawn(move || {
                            runtime.block_on(async {
                                let held = standing.hold_roots().await;
                                // What the Registry held as the change put `~/b`
                                // back in force.
                                let listed_then = listed(&sessions);
                                standing
                                    .roots_changed(&held, &[], &["~/b".to_owned()])
                                    .await;
                                listed_then
                            })
                        });
                        let deadline = std::time::Instant::now() + Duration::from_millis(50);
                        while !putting_back.is_finished() && std::time::Instant::now() < deadline {
                            std::thread::yield_now();
                        }
                        *change.lock().unwrap() = Some(putting_back);
                        Some(Vec::new())
                    }
                }),
                moved: Arc::default(),
            })
            .await;

        let putting_back = change.lock().unwrap().take().expect("the change began");
        assert_eq!(
            tokio::task::spawn_blocking(move || putting_back.join())
                .await
                .expect("the change is waited on")
                .expect("the change runs to its end"),
            Vec::<PathBuf>::new(),
            "the step at start had taken away what it decided to before the change put its \
             Roots in force"
        );
    }

    /// The Root each entry discovered under one is known by, by the path it
    /// is presented at.
    fn discovered_under(sessions: &SessionStore) -> Vec<(Option<PathBuf>, String)> {
        let mut discovered = sessions
            .discovered_entries()
            .into_iter()
            .map(|entry| (entry.path, entry.root))
            .collect::<Vec<_>>();
        discovered.sort();
        discovered
    }

    /// A Root taken away keeps what was discovered under it only where a
    /// Root still named may hold it: one that names nothing there, as on a
    /// drive not mounted, may be it, while one that resolves elsewhere than
    /// where it was found is not. Here `~/b` names a home this Server does
    /// not know, so it is resolved by nothing.
    #[tokio::test]
    async fn a_root_taken_away_keeps_what_it_held_only_where_a_root_still_named_may_hold_it() {
        let (registry, atlas) = discovered_under_b().await;
        let standing = RegistryStanding::new(registry.sessions.clone(), None, None, SURELY);
        let elsewhere = tempfile::tempdir().expect("create a directory elsewhere");
        let elsewhere = elsewhere.path().display().to_string();
        let unplugged = absolute(&["nowhere", "kept"]).display().to_string();

        let held = standing.hold_roots().await;
        standing
            .roots_changed(
                &held,
                &["~/b".to_owned(), unplugged.clone(), elsewhere.clone()],
                &[unplugged.clone(), elsewhere.clone()],
            )
            .await;
        assert_eq!(
            discovered_under(&registry.sessions),
            [(Some(atlas.clone()), "~/b".to_owned())],
            "a Root still named that names nothing there may be the one taken away, so what that \
             held is kept as it is"
        );

        standing
            .roots_changed(
                &held,
                &[unplugged, elsewhere.clone()],
                std::slice::from_ref(&elsewhere),
            )
            .await;
        assert_eq!(
            listed(&registry.sessions),
            Vec::<PathBuf>::new(),
            "while one resolving elsewhere than where it was found holds it not"
        );
    }

    /// What a Root in force holds of what was discovered under a spelling no
    /// longer named is known by discovery under that Root from then on: one
    /// naming where it was found, or one named anew in the very change that
    /// took the other away and that cannot be told from it, as the Root
    /// respelled may be. One that only may hold it, named before as well,
    /// leaves it known as it was.
    #[tokio::test]
    async fn what_a_root_in_force_holds_is_known_by_its_spelling_from_then_on() {
        let spell = |components: &[&str]| absolute(components).display().to_string();
        let (b, unplugged) = (spell(&["b"]), spell(&["nowhere", "kept"]));
        for (before, after, known_by, why) in [
            (
                vec!["~/b".to_owned()],
                vec![b.clone()],
                b.clone(),
                "a Root naming where it was found holds it",
            ),
            (
                vec!["~/b".to_owned()],
                vec![spell(&["nowhere", "b"])],
                spell(&["nowhere", "b"]),
                "a Root named anew that cannot be told from the one taken away may be it respelled",
            ),
            (
                vec!["~/b".to_owned(), unplugged.clone()],
                vec![unplugged.clone()],
                "~/b".to_owned(),
                "a Root named before as well that only may hold it leaves it as it was",
            ),
        ] {
            let (registry, atlas) = discovered_under_b().await;
            let standing = RegistryStanding::new(registry.sessions.clone(), None, None, SURELY);
            let held = standing.hold_roots().await;

            standing.roots_changed(&held, &before, &after).await;

            assert_eq!(
                discovered_under(&registry.sessions),
                [(Some(atlas), known_by)],
                "{why}"
            );
        }
    }

    /// A Root naming Suru's own data directory is none of the Roots in
    /// force, so what was discovered under it is dropped, though it is
    /// still named.
    #[tokio::test]
    async fn a_root_naming_the_data_directory_keeps_nothing_discovered_under_it() {
        let data = tempfile::tempdir().expect("create a data directory");
        let data = crate::paths::canonical(data.path()).expect("read it canonically");
        let root = data.display().to_string();
        let atlas = data.join("atlas");
        let registry = Registry::of(
            [(
                WorkspaceId::directory(&atlas),
                StoredWorkspace {
                    path: Some(atlas.clone()),
                    presentation: Some(WorkspacePresentation::Directory),
                    ..StoredWorkspace::known_by(WorkspaceProvenance::Discovered {
                        root: root.clone(),
                    })
                },
            )]
            .into_iter()
            .collect(),
        )
        .await;
        let standing =
            RegistryStanding::new(registry.sessions.clone(), None, Some(data.clone()), SURELY);

        standing
            .roots_at_start(ScriptedRoots {
                read: Some(move || Some(vec![root])),
                moved: Arc::default(),
            })
            .await;

        assert_eq!(listed(&registry.sessions), Vec::<PathBuf>::new());
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
