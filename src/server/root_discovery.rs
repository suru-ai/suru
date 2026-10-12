//! Discovery of the Workspaces directly within each Workspace Root (ADR
//! 0051): every child directory of a Root that is a Repository's main or
//! bare root is entered in the Workspace Registry, known by discovery under
//! that Root, so it is listed before any Session has begun there.
//!
//! Only a Root's immediate children are read, and of those only the ones
//! that are not hidden and that stand directly within the Root once read as
//! the Server reads paths: a link is kept where it names another directory
//! directly within the Root, and left alone where it names the Root itself
//! or anywhere else, since a Repository reached only through a link is not
//! directly within the Root. Nothing deeper is read, so a nested or vendored
//! Repository stays out of the Registry. Each child is read through source
//! control discovery, which reads the Sidekick Workspace as a directory of
//! its own whatever it holds, and is kept only where it is the root its
//! Repository is presented by: a plain directory, a directory within a
//! Repository — as every child of a Root that is itself a Repository is —
//! and a linked Worktree of a Repository presented elsewhere are each left
//! alone, so a linked Worktree counts for its Repository's Workspace alone
//! (ADR 0023). A Workspace presented at a Workspace Root — this one, or
//! another named within it — is never entered, so a Root is never a
//! Workspace, and an entry discovered under this Root that has since come to
//! be presented at one is dropped, unless a Session belongs to it. Roots are
//! told apart as directories (see [`super::root_directories`]); one that
//! cannot be resolved is told by its writing alone, so a Root on a mount
//! gone quiet holds back no other Root's discoveries. Discovery derives no
//! Icon and no Description: the first Session begun in a discovered
//! Workspace derives them as it would anywhere.
//!
//! Each Root is scanned on its own, on a task of its own, and everything a
//! scan reads from the filesystem or source control is read apart, on a
//! thread of its own (see [`super::reads_apart`]), so a Root on a mount gone
//! quiet holds up neither the others nor the Server. A directory whose read
//! has not ended is passed over by every later scan until it does, so it
//! holds one thread however often the Roots change, and a scan passing it
//! over goes on to finish. Nothing that asks for a scan waits on it for
//! longer than
//! [`super::ServerTimings::root_discovery_wait`], answering from the
//! Registry as it stands, and a slow scan shows in what is listed later.
//! Every Root is scanned afresh at start and whenever the Roots change,
//! superseding a scan of it still running, whose findings are then entered
//! nowhere. Whenever Workspaces are listed, each Root's scan still running
//! is joined, one that finished no longer ago than
//! [`super::ServerTimings::root_discovery_memo`] is reused, and the rest
//! are scanned again, so a Root that stalls keeps the others discovering
//! on their own.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{StreamExt, future::join_all, stream};
use tokio::sync::watch;

use super::{reads_apart::ReadsApart, root_directories};
use crate::{
    protocol::{RepositoryLocation, ResolvedWorkspace, Workspace},
    provider::wait_for_shutdown,
    sessions::SessionStore,
    source_control::{SourceControlService, is_directory, is_hidden},
};

/// How many of one Root's children are read from source control at once.
const CONCURRENT_READINGS: usize = 8;

/// One scan of one Workspace Root, as discovery runs it: injectable, so a
/// test can script how long one takes and what it enters.
#[async_trait]
pub(crate) trait RootScan: Send + Sync {
    /// Scans `root`, spelled as the Setting naming the Roots spells it,
    /// entering what it finds in the Registry through `commit` alone.
    async fn scan(&self, root: &str, commit: &Commit);
}

/// Where the Workspace Roots discovery follows are read from: the Settings
/// in force, read under the lock that decides each scan.
pub(crate) type Roots = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// A reading that only ever moves forward — the time since some fixed
/// moment — by which the memo is measured, so moving the system's clock
/// makes no scan seem younger than it is. A test moves it by hand.
#[derive(Clone)]
pub(crate) struct Monotonic(Arc<dyn Fn() -> Duration + Send + Sync>);

impl Monotonic {
    /// The time since now, as the platform's monotonic clock reads it.
    pub(crate) fn real() -> Self {
        let origin = std::time::Instant::now();
        Self(Arc::new(move || origin.elapsed()))
    }

    fn now(&self) -> Duration {
        (self.0)()
    }
}

/// Discovers the Repositories within this Server's Workspace Roots into its
/// Workspace Registry, each Root one scan at a time.
#[derive(Clone)]
pub(crate) struct RootDiscovery(Arc<Discovering>);

struct Discovering {
    scan: Arc<dyn RootScan>,
    roots: Roots,
    /// How long after a Root's scan finished a listing reuses it.
    memo: Duration,
    /// The longest anything waits for a scan before answering without it.
    wait: Duration,
    /// What the memo is measured by.
    clock: Monotonic,
    /// Stops every scan still running once the Server stops.
    shutdown: watch::Receiver<bool>,
    /// Which scans are running and finished, which every decision to begin
    /// or join one, and every entry a scan makes, holds.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The generation the scan begun last took. One count serves every
    /// Root, so no scan of a Root since taken away and named again is ever
    /// mistaken for the current one.
    generation: u64,
    /// Each Root followed — those named when the Roots were last read —
    /// with its own scans.
    roots: HashMap<String, Scans>,
    /// The destinations a Clone is landing at, each with how many Clones
    /// hold it: no scan enters what it finds at one, since the Clone enters
    /// its own Workspace there, known by its Clone.
    landing: HashMap<PathBuf, usize>,
}

/// One Root's scans.
struct Scans {
    /// The generation of its scan begun last, and so the only one whose
    /// findings are entered.
    generation: u64,
    /// Whether that scan is still running.
    running: bool,
    /// When its scan finished last, by the monotonic reading.
    finished_at: Option<Duration>,
    /// The generation its scan begun last took. A scan stops once this
    /// moves past its own, or is gone with the Root.
    begun: watch::Sender<u64>,
    /// The generation of its scan that finished last. A scan superseded
    /// never finishes, so whatever waits on it waits on its successor.
    finished: watch::Sender<u64>,
}

impl Scans {
    fn new() -> Self {
        Self {
            generation: 0,
            running: false,
            finished_at: None,
            begun: watch::channel(0).0,
            finished: watch::channel(0).0,
        }
    }

    /// A wait for the scan running now, or one begun after it.
    fn awaited(&self) -> Awaited {
        Awaited {
            finished: self.finished.subscribe(),
            generation: self.generation,
        }
    }
}

/// The scans a caller may wait on, one for each Root: each the one whose
/// generation it names, or any of that Root begun after it.
#[derive(Debug)]
#[must_use]
pub(crate) struct Ticket(Vec<Awaited>);

#[derive(Debug)]
struct Awaited {
    finished: watch::Receiver<u64>,
    generation: u64,
}

/// What a scan enters its findings in the Registry through: each entry is
/// made only while the scan is still its Root's current one, which is
/// checked under the lock that begins every scan, so a scan superseded
/// enters nothing — however far it got before it noticed.
#[derive(Clone)]
pub(crate) struct Commit {
    discovery: RootDiscovery,
    root: String,
    /// Every Root followed as the scan began, its own among them.
    roots: Arc<[String]>,
    generation: u64,
}

impl Commit {
    /// Every Root followed as this scan began, its own among them: those in
    /// force for as long as it is current, since a change of them begins
    /// every scan afresh.
    pub(crate) fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Runs `enter`, for what this scan found at `directory`, while this
    /// scan is still its Root's current one and no Clone is landing at
    /// `directory`, answering whether it ran.
    pub(crate) fn while_current_at(&self, directory: &Path, enter: impl FnOnce()) -> bool {
        let state = self.discovery.0.state.lock().unwrap();
        let current = state
            .roots
            .get(&self.root)
            .is_some_and(|scans| scans.generation == self.generation)
            && !state.landing.contains_key(directory);
        if current {
            enter();
        }
        current
    }
}

/// A destination a Clone is landing at, held from before the clone takes
/// its name until the Clone has entered it in the Registry, so that no scan
/// happening upon it meanwhile enters it as discovered.
pub(crate) struct Landing {
    discovery: RootDiscovery,
    destination: PathBuf,
}

impl Drop for Landing {
    fn drop(&mut self) {
        let mut state = self
            .discovery
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = state.landing.get_mut(&self.destination) {
            *held -= 1;
            if *held == 0 {
                state.landing.remove(&self.destination);
            }
        }
    }
}

impl RootDiscovery {
    pub(crate) fn new(
        scan: Arc<dyn RootScan>,
        roots: Roots,
        memo: Duration,
        wait: Duration,
        clock: Monotonic,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self(Arc::new(Discovering {
            scan,
            roots,
            memo,
            wait,
            clock,
            shutdown,
            state: Mutex::default(),
        }))
    }

    /// Discovers the Workspaces within the Roots, as a listing does: for
    /// each Root named now, joins its scan still running, reuses one that
    /// finished within the memo, and otherwise begins one — then waits for
    /// them, though never past the bound on waiting.
    pub(crate) async fn discover(&self) {
        let ticket = {
            let mut state = self.0.state.lock().unwrap();
            let roots = self.follow(&mut state);
            let followed = Arc::<[String]>::from(roots.as_slice());
            let now = self.0.clock.now();
            let mut awaited = Vec::new();
            for root in roots {
                let scans = state.roots.entry(root.clone()).or_insert_with(Scans::new);
                if scans.running {
                    awaited.push(scans.awaited());
                    continue;
                }
                if scans
                    .finished_at
                    .is_some_and(|at| now.saturating_sub(at) < self.0.memo)
                {
                    continue;
                }
                awaited.push(self.begin(&mut state, root, &followed));
            }
            Ticket(awaited)
        };
        self.settled(ticket).await;
    }

    /// Begins discovering every Root afresh, whatever was found before and
    /// superseding each scan still running: at start, and once the Roots
    /// have changed. Answers the scans to wait on, which this does not.
    pub(crate) fn rediscover(&self) -> Ticket {
        let mut state = self.0.state.lock().unwrap();
        let roots = self.follow(&mut state);
        let followed = Arc::<[String]>::from(roots.as_slice());
        Ticket(
            roots
                .into_iter()
                .map(|root| self.begin(&mut state, root, &followed))
                .collect(),
        )
    }

    /// Holds `destination`, where a Clone is about to land, from every scan
    /// until the answer is dropped.
    pub(crate) fn landing(&self, destination: &Path) -> Landing {
        *self
            .0
            .state
            .lock()
            .unwrap()
            .landing
            .entry(destination.to_owned())
            .or_default() += 1;
        Landing {
            discovery: self.clone(),
            destination: destination.to_owned(),
        }
    }

    /// Waits for the scans `ticket` names to finish, or for ones begun after
    /// them to, but never past the bound on waiting: a scan still running
    /// then goes on, and what it finds shows in what is listed later.
    pub(crate) async fn settled(&self, Ticket(awaited): Ticket) {
        let scans = join_all(awaited.into_iter().map(|mut awaited| async move {
            // A Root taken away meanwhile is waited on no longer.
            let _ = awaited
                .finished
                .wait_for(|finished| *finished >= awaited.generation)
                .await;
        }));
        if tokio::time::timeout(self.0.wait, scans).await.is_err() {
            tracing::debug!("answered before a scan of the Workspace Roots finished");
        }
    }

    /// Reads the Roots named now, under the lock `state` is held by, so no
    /// decision is made on Roots older than one made before it, and follows
    /// them alone: a Root no longer named is let go, and its scan still
    /// running stops, entering nothing.
    fn follow(&self, state: &mut State) -> Vec<String> {
        let roots = (self.0.roots)();
        state.roots.retain(|root, _| roots.contains(root));
        roots
    }

    /// Begins a scan of `root`, one of the Roots followed, on a task of its
    /// own, superseding any of it still running.
    fn begin(&self, state: &mut State, root: String, followed: &Arc<[String]>) -> Awaited {
        state.generation += 1;
        let generation = state.generation;
        let scans = state.roots.entry(root.clone()).or_insert_with(Scans::new);
        scans.generation = generation;
        scans.running = true;
        scans.begun.send_replace(generation);
        let mut begun = scans.begun.subscribe();
        let commit = Commit {
            discovery: self.clone(),
            root,
            roots: followed.clone(),
            generation,
        };
        let scan = self.0.scan.clone();
        let mut shutdown = self.0.shutdown.clone();
        tokio::spawn(async move {
            let whole = tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => false,
                _ = begun.wait_for(|begun| *begun != generation) => false,
                () = scan.scan(&commit.root, &commit) => true,
            };
            if whole {
                commit.discovery.finish(&commit.root, generation);
            }
        });
        scans.awaited()
    }

    /// Records the scan of `root` `generation` names as finished, unless
    /// another has been begun since or the Root let go.
    fn finish(&self, root: &str, generation: u64) {
        let now = self.0.clock.now();
        let mut state = self.0.state.lock().unwrap();
        if let Some(scans) = state.roots.get_mut(root)
            && scans.generation == generation
        {
            scans.running = false;
            scans.finished_at = Some(now);
            scans.finished.send_replace(generation);
        }
    }
}

/// The scan the Server runs: every Repository root directly within a Root,
/// entered in the Registry. The Root is listed, each child read through source control, and the other Roots
/// resolved, apart (see [`ReadsApart`]), so a filesystem that stops
/// answering holds a thread of discovery's own rather than one the Server
/// runs anything else on, and no more of them at once than [`READS_AT_ONCE`].
pub(crate) struct RegistryScan {
    sessions: SessionStore,
    source_control: SourceControlService,
    /// The directory a Root beginning with `~` is read beneath.
    home: Option<PathBuf>,
    /// The longest a scan waits on the other Roots being resolved.
    wait: Duration,
    reads: ReadsApart,
    /// The other Roots' resolutions, apart from `reads` so none is passed
    /// over for a read of the same Root its own scan has outstanding.
    resolutions: ReadsApart,
}

/// The most reads of Roots and of the directories within them outstanding
/// at once, across every Root: room for several Roots scanned together,
/// each reading [`CONCURRENT_READINGS`] directories at a time, while
/// directories that never answer hold no more threads than this. A read
/// past it is passed over until a later scan.
const READS_AT_ONCE: usize = 64;

/// The most resolutions of the other Roots outstanding at once, across
/// every scan: a Root past it is told by its writing alone.
const RESOLUTIONS_AT_ONCE: usize = 16;

impl RegistryScan {
    pub(crate) fn new(
        sessions: SessionStore,
        source_control: SourceControlService,
        home: Option<PathBuf>,
        wait: Duration,
    ) -> Self {
        Self {
            sessions,
            source_control,
            home,
            wait,
            reads: ReadsApart::new("suru-root-discovery-read", READS_AT_ONCE),
            resolutions: ReadsApart::new("suru-root-discovery-resolution", RESOLUTIONS_AT_ONCE),
        }
    }
}

#[async_trait]
impl RootScan for RegistryScan {
    /// Enters what is directly within `root` in the Registry, as the module
    /// says. A Root that cannot be read is skipped, saying why in the Log,
    /// and never made.
    async fn scan(&self, root: &str, commit: &Commit) {
        let began = std::time::Instant::now();
        let Some(directory) = expand(root, self.home.as_deref()) else {
            tracing::warn!(
                root,
                "a Workspace Root beginning with ~ names a home this Server does not know, \
                 so nothing within it was discovered"
            );
            return;
        };
        let listed = directory.clone();
        let (resolved, children) = match self
            .reads
            .read(
                &directory,
                move || async move { child_directories(&listed) },
            )
            .await
        {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                tracing::warn!(
                    root,
                    %error,
                    "a Workspace Root could not be read, so nothing within it was discovered"
                );
                return;
            }
            Err(not_read) => {
                tracing::debug!(
                    root,
                    %not_read,
                    "a Workspace Root was passed over, so nothing within it was discovered"
                );
                return;
            }
        };
        let others = root_directories::resolve(
            &self.resolutions,
            self.home.as_deref(),
            self.wait,
            commit.roots().iter().filter(|other| *other != root),
        )
        .await;
        let at_a_root = |presented: &Path| {
            presented == resolved || others.values().any(|other| other.names(presented))
        };
        let mut readings = stream::iter(children.into_iter().map(|child| async move {
            let source_control = self.source_control.clone();
            let read = child.clone();
            let reading = self
                .reads
                .read(&child, move || async move {
                    source_control.read_unremembered(&read).await
                })
                .await;
            (child, reading)
        }))
        .buffer_unordered(CONCURRENT_READINGS);
        let mut discovered = 0usize;
        while let Some((child, reading)) = readings.next().await {
            let reading = match reading {
                Ok(reading) => reading,
                Err(not_read) => {
                    tracing::debug!(
                        root,
                        directory = %child.display(),
                        %not_read,
                        "a directory within a Workspace Root was passed over"
                    );
                    continue;
                }
            };
            // What the reading teaches, of the Registry and of the
            // Repositories this Server remembers alike, is taken only while
            // this scan is current, so a reading begun before the directory
            // changed and ended after a newer scan read it teaches nothing;
            // nor while a Clone is landing there, which enters its own.
            let mut entered = false;
            commit.while_current_at(&child, || {
                if let Some(read) = reading.workspace.repository.as_deref() {
                    self.source_control.learn_main_checkout(read);
                }
                match rooted_at(&child, reading) {
                    Some(workspace) if at_a_root(&workspace.path) => {
                        self.sessions.forget_discovered(&workspace.id, root);
                    }
                    Some(workspace) => {
                        self.sessions.enter_discovered(&workspace, &child, root);
                        entered = true;
                    }
                    None => {}
                }
            });
            if entered {
                discovered += 1;
            }
        }
        tracing::debug!(
            root,
            discovered,
            elapsed_ms = began.elapsed().as_millis(),
            "Workspace Root discovered"
        );
    }
}

/// The directory the Workspace Root `root` names: beneath `home` where it
/// begins with `~`, and as it is spelled where it is absolute. `None` for
/// one beginning with `~` where no home is known, or naming another user's.
/// A Clone reads the Root it goes in by this too.
pub(super) fn expand(root: &str, home: Option<&Path>) -> Option<PathBuf> {
    match Path::new(root).strip_prefix("~") {
        Ok(beneath_home) => Some(home?.join(beneath_home)),
        Err(_) if root.starts_with('~') => None,
        Err(_) => Some(PathBuf::from(root)),
    }
}

/// The directory `root` leads to, read canonically, and the directories
/// directly within it that discovery reads, each as the Server reads paths:
/// every child that is a directory standing directly within the Root once
/// read so — a link counting as the directory it names, where that is
/// another directly within the Root, and as nothing where it names the Root
/// itself or anywhere else — but those that are hidden and those whose names
/// are not Unicode, which no Workspace can be presented by.
fn child_directories(root: &Path) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    let root = crate::paths::canonical(root)?;
    let children = std::fs::read_dir(&root)?
        .flatten()
        .filter(is_directory)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !is_hidden(name, entry))
        })
        .filter_map(|entry| crate::paths::canonical(entry.path()).ok())
        .filter(|child| child.parent() == Some(root.as_path()))
        .collect();
    Ok((root, children))
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tokio::sync::{Semaphore, mpsc};

    use super::*;

    /// A path rooted as this platform roots absolute paths.
    fn absolute(components: &[&str]) -> PathBuf {
        components.iter().fold(
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            |path, component| path.join(component),
        )
    }

    /// How long a test waits for something it knows will happen.
    const SURELY: Duration = Duration::from_secs(10);

    /// A bound on waiting for a scan that no test reaches, so only a scan
    /// finishing ends a wait.
    const AS_LONG_AS_IT_TAKES: Duration = Duration::from_secs(60 * 60);

    /// A scan that says when it begins, which Root it reads and how it
    /// commits, runs until a test lets a scan of that Root finish, and then
    /// enters its Root by name.
    #[derive(Default)]
    struct ScriptedScan {
        begun: Mutex<Option<mpsc::UnboundedSender<(String, Commit)>>>,
        finish: Mutex<HashMap<String, Arc<Semaphore>>>,
        entered: Mutex<Vec<String>>,
    }

    impl ScriptedScan {
        fn gate(&self, root: &str) -> Arc<Semaphore> {
            self.finish
                .lock()
                .unwrap()
                .entry(root.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(0)))
                .clone()
        }

        /// Lets one more scan of `root` finish.
        fn let_finish(&self, root: &str) {
            self.gate(root).add_permits(1);
        }

        fn entered(&self) -> Vec<String> {
            self.entered.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RootScan for ScriptedScan {
        async fn scan(&self, root: &str, commit: &Commit) {
            if let Some(begun) = &*self.begun.lock().unwrap() {
                let _ = begun.send((root.to_owned(), commit.clone()));
            }
            self.gate(root)
                .acquire()
                .await
                .expect("the scan's permits stay open")
                .forget();
            commit.while_current_at(Path::new(root), || {
                self.entered.lock().unwrap().push(root.to_owned());
            });
        }
    }

    /// Discovery whose scans are scripted, following the Roots `roots`
    /// holds, reusing a finished scan for `memo` by a monotonic reading the
    /// test moves, and waiting on scans no longer than `wait`.
    struct Scripted {
        discovery: RootDiscovery,
        scan: Arc<ScriptedScan>,
        begun: mpsc::UnboundedReceiver<(String, Commit)>,
        roots: Arc<Mutex<Vec<String>>>,
        hand: Arc<AtomicU64>,
        _shutdown: watch::Sender<bool>,
    }

    impl Scripted {
        /// Moves the monotonic reading on by `by`.
        fn advance(&self, by: Duration) {
            let by = u64::try_from(by.as_millis()).expect("a test moves little time");
            self.hand.fetch_add(by, Ordering::SeqCst);
        }

        /// Names `roots` as the Roots in force.
        fn name(&self, roots: &[&str]) {
            *self.roots.lock().unwrap() = roots.iter().map(|root| (*root).to_owned()).collect();
        }

        /// The Root the next scan begun reads, once one has begun.
        async fn next_begun(&mut self) -> String {
            tokio::time::timeout(SURELY, self.begun.recv())
                .await
                .expect("a scan begins")
                .expect("the scan's channel stays open")
                .0
        }

        /// The Roots of every scan begun so far and not yet taken.
        fn begun_since(&mut self) -> Vec<String> {
            std::iter::from_fn(|| self.begun.try_recv().ok().map(|(root, _)| root)).collect()
        }

        /// A listing's discovery on a task of its own.
        fn list(&self) -> tokio::task::JoinHandle<()> {
            let discovery = self.discovery.clone();
            tokio::spawn(async move { discovery.discover().await })
        }
    }

    fn scripted(roots: &[&str], memo: Duration, wait: Duration) -> Scripted {
        scripted_reading(roots, memo, wait, |named| named.lock().unwrap().clone())
    }

    /// [`scripted`], the Roots read by `read` from what the test names.
    fn scripted_reading(
        roots: &[&str],
        memo: Duration,
        wait: Duration,
        read: impl Fn(&Arc<Mutex<Vec<String>>>) -> Vec<String> + Send + Sync + 'static,
    ) -> Scripted {
        let (begun_tx, begun) = mpsc::unbounded_channel();
        let scan = Arc::new(ScriptedScan::default());
        *scan.begun.lock().unwrap() = Some(begun_tx);
        let named = Arc::new(Mutex::new(
            roots
                .iter()
                .map(|root| (*root).to_owned())
                .collect::<Vec<_>>(),
        ));
        let hand = Arc::new(AtomicU64::new(0));
        let reading = hand.clone();
        let clock = Monotonic(Arc::new(move || {
            Duration::from_millis(reading.load(Ordering::SeqCst))
        }));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let followed = named.clone();
        Scripted {
            discovery: RootDiscovery::new(
                scan.clone(),
                Arc::new(move || read(&followed)),
                memo,
                wait,
                clock,
                shutdown_rx,
            ),
            scan,
            begun,
            roots: named,
            hand,
            _shutdown: shutdown,
        }
    }

    #[tokio::test]
    async fn a_listing_during_a_slow_scan_joins_it_and_the_memo_runs_from_its_end() {
        let memo = Duration::from_secs(3);
        let mut scripted = scripted(&["~/projects"], memo, AS_LONG_AS_IT_TAKES);
        let first = scripted.list();
        assert_eq!(scripted.next_begun().await, "~/projects");
        scripted.advance(Duration::from_secs(4));
        let second = scripted.list();
        // The second listing has made its choice once it is waiting.
        tokio::task::yield_now().await;
        assert!(!second.is_finished() && !first.is_finished());

        scripted.scan.let_finish("~/projects");
        for listing in [first, second] {
            tokio::time::timeout(SURELY, listing)
                .await
                .expect("each listing answers once the scan finishes")
                .expect("the listing runs to its end");
        }
        assert_eq!(
            scripted.begun_since(),
            Vec::<String>::new(),
            "a listing during a scan longer than the memo joins it rather than scanning again"
        );

        scripted.advance(memo - Duration::from_millis(1));
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a listing within the memo answers at once");
        assert_eq!(
            scripted.begun_since(),
            Vec::<String>::new(),
            "a scan is reused for the memo from when it finished, though it began longer ago"
        );

        scripted.advance(Duration::from_millis(1));
        let third = scripted.list();
        assert_eq!(
            scripted.next_begun().await,
            "~/projects",
            "once the memo has run out, a listing scans again"
        );
        scripted.scan.let_finish("~/projects");
        third.await.expect("the third listing runs to its end");
    }

    #[tokio::test]
    async fn a_listing_waits_for_a_scan_no_longer_than_its_bound() {
        let mut scripted = scripted(&["~/projects"], Duration::ZERO, Duration::from_millis(20));
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a listing answers once its bound runs out, the scan still running");
        assert_eq!(scripted.next_begun().await, "~/projects");

        let ticket = scripted.discovery.rediscover();
        tokio::time::timeout(SURELY, scripted.discovery.settled(ticket))
            .await
            .expect("a fresh scan is waited on no longer than the bound either");
    }

    #[tokio::test]
    async fn a_fresh_scan_supersedes_one_still_running() {
        let mut scripted = scripted(&["~/projects"], Duration::ZERO, AS_LONG_AS_IT_TAKES);
        let listing = scripted.list();
        assert_eq!(scripted.next_begun().await, "~/projects");

        scripted.name(&["~/notes"]);
        let ticket = scripted.discovery.rediscover();
        assert_eq!(
            scripted.next_begun().await,
            "~/notes",
            "the changed Roots are scanned at once, the scan of the old ones still running"
        );
        scripted.scan.let_finish("~/notes");
        tokio::time::timeout(SURELY, scripted.discovery.settled(ticket))
            .await
            .expect("the scan of the changed Roots finishes");
        tokio::time::timeout(SURELY, listing)
            .await
            .expect("a listing waiting on a Root taken away waits no longer")
            .expect("the listing runs to its end");
        scripted.scan.let_finish("~/projects");
        assert_eq!(
            scripted.scan.entered(),
            ["~/notes"],
            "only the Root in force is entered"
        );
    }

    #[tokio::test]
    async fn a_scan_superseded_enters_nothing_however_far_it_got() {
        let mut scripted = scripted(&["~/projects"], Duration::ZERO, AS_LONG_AS_IT_TAKES);
        let _ = scripted.discovery.rediscover();
        let (_, superseded) = tokio::time::timeout(SURELY, scripted.begun.recv())
            .await
            .expect("the first scan begins")
            .expect("the scan's channel stays open");

        let _ = scripted.discovery.rediscover();
        let (_, current) = tokio::time::timeout(SURELY, scripted.begun.recv())
            .await
            .expect("the second scan begins")
            .expect("the scan's channel stays open");

        assert!(
            !superseded.while_current_at(Path::new("found"), || panic!(
                "a superseded scan enters nothing"
            )),
            "a scan that reaches its entry after another has begun is refused it"
        );
        assert!(
            current.while_current_at(Path::new("found"), || {}),
            "the scan begun last enters what it finds"
        );
    }

    /// A scan happening upon a Clone landing — named, but not yet entered by
    /// its Clone — enters nothing there, so the Clone's entry is known by
    /// its Clone; it enters what it finds anywhere else, and there too once
    /// every Clone landing there has let go.
    #[tokio::test]
    async fn a_scan_enters_nothing_where_a_clone_is_landing() {
        let mut scripted = scripted(&["~/projects"], Duration::ZERO, AS_LONG_AS_IT_TAKES);
        let _ = scripted.discovery.rediscover();
        let (_, scan) = tokio::time::timeout(SURELY, scripted.begun.recv())
            .await
            .expect("the scan begins")
            .expect("the scan's channel stays open");
        let atlas = absolute(&["home", "ada", "projects", "atlas"]);
        let maps = absolute(&["home", "ada", "projects", "maps"]);

        let landing = scripted.discovery.landing(&atlas);
        let racing = scripted.discovery.landing(&atlas);
        assert!(
            !scan.while_current_at(&atlas, || panic!("no scan enters a Clone landing")),
            "a destination a Clone is landing at is the Clone's to enter"
        );
        assert!(
            scan.while_current_at(&maps, || {}),
            "and nowhere else is held from the scan"
        );
        drop(landing);
        assert!(
            !scan.while_current_at(&atlas, || panic!("no scan enters a Clone landing")),
            "it is held while any Clone landing there holds it"
        );
        drop(racing);
        assert!(
            scan.while_current_at(&atlas, || {}),
            "and the scan enters what it finds there once the Clones let go"
        );
    }

    #[tokio::test]
    async fn a_root_whose_scan_stalls_leaves_the_others_discovering_on_their_own() {
        let memo = Duration::from_secs(3);
        let mut scripted = scripted(
            &["~/stalled", "~/projects"],
            memo,
            Duration::from_millis(20),
        );
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a listing answers within its bound");
        let mut begun = vec![scripted.next_begun().await, scripted.next_begun().await];
        begun.sort();
        assert_eq!(begun, ["~/projects", "~/stalled"], "each Root is scanned");
        scripted.scan.let_finish("~/projects");
        let mut entered = Vec::new();
        while entered.is_empty() {
            tokio::task::yield_now().await;
            entered = scripted.scan.entered();
        }
        assert_eq!(entered, ["~/projects"]);

        scripted.advance(memo);
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a listing answers within its bound");
        assert_eq!(
            scripted.next_begun().await,
            "~/projects",
            "the healthy Root is scanned again once its memo runs out"
        );
        assert_eq!(
            scripted.begun_since(),
            Vec::<String>::new(),
            "while the stalled Root's scan is joined rather than begun again"
        );
    }

    /// A listing deciding which scans to begin reads the Roots under the
    /// same lock a change of the Roots begins its scans under, so whichever
    /// decides last decides on the Roots in force. Here the Roots change
    /// while a listing reads them — the change naming others and beginning
    /// to scan them, which it may finish before the listing goes on — and
    /// the Root it took away is neither scanned on nor entered.
    #[tokio::test]
    async fn a_listing_never_brings_back_roots_a_change_took_away() {
        let changed = Arc::new(AtomicBool::new(false));
        let discovery = Arc::new(std::sync::OnceLock::<RootDiscovery>::new());
        let (tickets, mut ticket) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Handle::current();
        let scripted = scripted_reading(&["~/old"], AS_LONG_AS_IT_TAKES, AS_LONG_AS_IT_TAKES, {
            let changed = changed.clone();
            let discovery = discovery.clone();
            move |named| {
                let read = named.lock().unwrap().clone();
                if !changed.swap(true, Ordering::SeqCst) {
                    let named = named.clone();
                    let discovery = discovery.get().expect("discovery is made").clone();
                    let runtime = runtime.clone();
                    let tickets = tickets.clone();
                    let change = std::thread::spawn(move || {
                        let _runtime = runtime.enter();
                        *named.lock().unwrap() = vec!["~/new".to_owned()];
                        let _ = tickets.send(discovery.rediscover());
                    });
                    let deadline = std::time::Instant::now() + Duration::from_millis(50);
                    while !change.is_finished() && std::time::Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                }
                read
            }
        });
        discovery
            .set(scripted.discovery.clone())
            .unwrap_or_else(|_| unreachable!("discovery is made once"));
        let listing = scripted.list();
        let ticket = tokio::time::timeout(SURELY, ticket.recv())
            .await
            .expect("the change begins its scans")
            .expect("the change answers its scans");
        scripted.scan.let_finish("~/new");
        scripted.scan.let_finish("~/old");
        tokio::time::timeout(SURELY, scripted.discovery.settled(ticket))
            .await
            .expect("the scan of the Roots in force finishes");
        tokio::time::timeout(SURELY, listing)
            .await
            .expect("the listing waits on no Root taken away")
            .expect("the listing runs to its end");
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a later listing reuses the scan of the Roots in force");
        assert_eq!(
            scripted.scan.entered(),
            ["~/new"],
            "the Root the change took away is neither scanned on nor entered"
        );
    }

    #[test]
    fn a_roots_children_are_the_directories_directly_within_it_but_the_hidden_ones() {
        let root = tempfile::tempdir().expect("create a Root");
        let root = crate::paths::canonical(root.path()).expect("read the Root canonically");
        for directory in ["atlas", "ledger", ".tooling", "notes/deeper"] {
            std::fs::create_dir_all(root.join(directory)).expect("create a child directory");
        }
        std::fs::write(root.join("README.md"), "Projects").expect("write a file");

        let (_, mut children) = child_directories(&root).expect("read the Root");
        children.sort();

        assert_eq!(
            children,
            [root.join("atlas"), root.join("ledger"), root.join("notes")],
            "each directory directly within the Root, but neither a hidden directory, a file, \
             nor anything deeper"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_counts_only_where_it_names_another_directory_directly_within_the_root() {
        let root = tempfile::tempdir().expect("create a Root");
        let root = crate::paths::canonical(root.path()).expect("read the Root canonically");
        std::fs::create_dir_all(root.join("atlas").join("deeper")).expect("create a child");
        let elsewhere = tempfile::tempdir().expect("create a directory elsewhere");
        let link = |target: &Path, name: &str| {
            std::os::unix::fs::symlink(target, root.join(name)).expect("link into the Root");
        };
        link(Path::new("."), "self");
        link(elsewhere.path(), "elsewhere");
        link(&root.join("atlas").join("deeper"), "deeper");
        link(&root.join("atlas"), "alias");

        assert_eq!(
            child_directories(&root).expect("read the Root").1,
            [root.join("atlas"), root.join("atlas")],
            "a link to another directory directly within the Root reads as that directory; one \
             to the Root itself, to anywhere else, or to anything deeper is left alone"
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
