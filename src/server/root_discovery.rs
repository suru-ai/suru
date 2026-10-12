//! Discovery of the Workspaces directly within each Workspace Root (ADR
//! 0051): every child directory of a Root that is a Repository's main or
//! bare root is entered in the Workspace Registry, known by discovery under
//! that Root, so it is listed before any Session has begun there.
//!
//! Only a Root's immediate children are read, and of those only the ones
//! that are not hidden and that stand directly within the Root once read
//! as the Server reads paths: a link is kept where it names another
//! directory directly within the Root, and left alone where it names the
//! Root itself or anywhere else, since a Repository reached only through a
//! link is not directly within the Root. Nothing deeper is read, so a nested
//! or vendored Repository stays out of the Registry. Each child is read
//! through source control discovery, which reads the Sidekick Workspace as a
//! directory of its own whatever it holds, and is kept only where it is the
//! root its Repository is presented by: a plain directory, a directory
//! within a Repository — as every child of a Root that is itself a
//! Repository is — and a linked Worktree of a Repository presented elsewhere
//! are each left alone, so the Root itself never becomes a Workspace and a
//! linked Worktree counts for its Repository's Workspace alone (ADR 0023).
//! Discovery derives no Icon and no Description: the first Session begun in
//! a discovered Workspace derives them as it would anywhere.
//!
//! A scan runs on a task of its own, so nothing that asks for one is held
//! by it for longer than [`super::ServerTimings::root_discovery_wait`]:
//! whatever waits that long answers from the Registry as it stands, and a
//! slow scan shows in what is listed later. One runs at start and whenever
//! the Roots change, superseding a scan still running, and whenever
//! Workspaces are listed: a listing joins a scan of the same Roots already
//! running, and reuses one that finished no longer ago than
//! [`super::ServerTimings::root_discovery_memo`] instead of asking for
//! another.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{StreamExt, future::join_all, stream};
use tokio::sync::watch;

use crate::{
    clock::ServerClock,
    protocol::{
        RepositoryLocation, ResolvedWorkspace, SessionTimestamp, SettingsSnapshot, Workspace,
    },
    provider::wait_for_shutdown,
    sessions::SessionStore,
    source_control::{SourceControlService, is_directory, is_hidden},
};

/// How many of one Root's children are read from source control at once.
const CONCURRENT_READINGS: usize = 8;

/// One scan of the Workspace Roots, as discovery runs it: injectable, so a
/// test can script how long one takes.
#[async_trait]
pub(crate) trait RootScan: Send + Sync {
    /// Scans `roots`, each spelled as the Setting naming the Roots spells it.
    async fn scan(&self, roots: &[String]);
}

/// Discovers the Repositories within this Server's Workspace Roots into its
/// Workspace Registry, one scan at a time.
#[derive(Clone)]
pub(crate) struct RootDiscovery(Arc<Discovering>);

struct Discovering {
    scan: Arc<dyn RootScan>,
    settings: watch::Receiver<SettingsSnapshot>,
    /// How long after a scan finished a listing reuses it.
    memo: Duration,
    /// The longest anything waits for a scan before answering without it.
    wait: Duration,
    /// What the memo is measured by.
    clock: ServerClock,
    /// Stops a scan still running once the Server stops.
    shutdown: watch::Receiver<bool>,
    progress: Mutex<Progress>,
    /// The generation of the scan begun last, which a running scan watches
    /// so it stops once another supersedes it.
    begun: watch::Sender<u64>,
    /// The generation of the scan that finished last. A scan superseded
    /// never finishes, so whatever waits on it waits on its successor.
    finished: watch::Sender<u64>,
}

/// The scans begun and finished so far.
#[derive(Default)]
struct Progress {
    /// The generation of the scan begun last.
    generation: u64,
    /// The Roots the scan begun last reads, while it runs.
    running: Option<Vec<String>>,
    /// The scan that finished last: when, and of which Roots.
    finished: Option<(SessionTimestamp, Vec<String>)>,
}

/// The scan a caller may wait on: the one whose generation it names, or any
/// begun after it.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub(crate) struct Ticket(u64);

impl RootDiscovery {
    pub(crate) fn new(
        scan: Arc<dyn RootScan>,
        settings: watch::Receiver<SettingsSnapshot>,
        memo: Duration,
        wait: Duration,
        clock: ServerClock,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self(Arc::new(Discovering {
            scan,
            settings,
            memo,
            wait,
            clock,
            shutdown,
            progress: Mutex::default(),
            begun: watch::channel(0).0,
            finished: watch::channel(0).0,
        }))
    }

    /// Discovers the Workspaces within the Roots, as a listing does: joins a
    /// scan of the Roots named now that is still running, reuses one that
    /// finished within the memo, and otherwise begins one — then waits for
    /// it, though never past the bound on waiting.
    pub(crate) async fn discover(&self) {
        if let Some(ticket) = self.for_listing() {
            self.settled(ticket).await;
        }
    }

    /// Begins discovering the Workspaces within the Roots afresh, whatever
    /// was found before and superseding a scan still running: at start, and
    /// once the Roots have changed. Answers the scan to wait on, which this
    /// does not.
    pub(crate) fn rediscover(&self) -> Ticket {
        let roots = self.roots();
        self.begin(&mut self.0.progress.lock().unwrap(), roots)
    }

    /// Waits for the scan `ticket` names to finish, or for one begun after
    /// it to, but never past the bound on waiting: a scan still running
    /// then goes on, and what it finds shows in what is listed later.
    pub(crate) async fn settled(&self, Ticket(generation): Ticket) {
        let mut finished = self.0.finished.subscribe();
        let waited = tokio::time::timeout(
            self.0.wait,
            finished.wait_for(|finished| *finished >= generation),
        )
        .await;
        if waited.is_err() {
            tracing::debug!("answered before a scan of the Workspace Roots finished");
        }
    }

    /// The Workspace Roots the Settings in force name.
    fn roots(&self) -> Vec<String> {
        self.0.settings.borrow().settings.workspace.roots.clone()
    }

    /// The scan a listing waits on, if any.
    fn for_listing(&self) -> Option<Ticket> {
        let roots = self.roots();
        let mut progress = self.0.progress.lock().unwrap();
        match &progress.running {
            Some(running) if *running == roots => return Some(Ticket(progress.generation)),
            Some(_) => {}
            None => {
                let now = self.0.clock.now().0;
                let memo = u64::try_from(self.0.memo.as_millis()).unwrap_or(u64::MAX);
                if progress.finished.as_ref().is_some_and(|(at, scanned)| {
                    *scanned == roots && now.saturating_sub(at.0) < memo
                }) {
                    return None;
                }
            }
        }
        Some(self.begin(&mut progress, roots))
    }

    /// Begins a scan of `roots` on a task of its own, superseding any still
    /// running, which stops.
    fn begin(&self, progress: &mut Progress, roots: Vec<String>) -> Ticket {
        progress.generation += 1;
        let generation = progress.generation;
        progress.running = Some(roots.clone());
        self.0.begun.send_replace(generation);
        let discovery = self.clone();
        tokio::spawn(async move {
            let mut superseded = discovery.0.begun.subscribe();
            let mut shutdown = discovery.0.shutdown.clone();
            let whole = tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => false,
                _ = superseded.wait_for(|begun| *begun != generation) => false,
                () = discovery.0.scan.scan(&roots) => true,
            };
            if whole {
                discovery.finish(generation, roots);
            }
        });
        Ticket(generation)
    }

    /// Records the scan `generation` names as finished, unless another has
    /// been begun since, which it is then no reading of the Roots beside.
    fn finish(&self, generation: u64, roots: Vec<String>) {
        let mut progress = self.0.progress.lock().unwrap();
        if progress.generation != generation {
            return;
        }
        progress.running = None;
        progress.finished = Some((self.0.clock.now(), roots));
        self.0.finished.send_replace(generation);
    }
}

/// The scan the Server runs: every Repository root directly within each
/// Root, entered in the Registry. The Roots are read at once, so one that
/// is slow to answer holds back none of the others.
pub(crate) struct RegistryScan {
    sessions: SessionStore,
    source_control: SourceControlService,
    /// The directory a Root beginning with `~` is read beneath.
    home: Option<PathBuf>,
}

impl RegistryScan {
    pub(crate) fn new(
        sessions: SessionStore,
        source_control: SourceControlService,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            sessions,
            source_control,
            home,
        }
    }

    /// Enters every Repository root directly within `root` in the Registry,
    /// answering how many there were. A Root that cannot be read is
    /// skipped, saying why in the Log, and never made.
    async fn scan_root(&self, root: &str) -> usize {
        let Some(directory) = expand(root, self.home.as_deref()) else {
            tracing::warn!(
                root,
                "a Workspace Root beginning with ~ names a home this Server does not know, \
                 so nothing within it was discovered"
            );
            return 0;
        };
        let children =
            match tokio::task::spawn_blocking(move || child_directories(&directory)).await {
                Ok(Ok(children)) => children,
                Ok(Err(error)) => {
                    tracing::warn!(
                        root,
                        %error,
                        "a Workspace Root could not be read, so nothing within it was discovered"
                    );
                    return 0;
                }
                Err(error) => {
                    tracing::error!(root, %error, "reading a Workspace Root did not finish");
                    return 0;
                }
            };
        let mut readings = stream::iter(children.into_iter().map(|child| async move {
            let reading = self.source_control.read_unremembered(&child).await;
            (child, reading)
        }))
        .buffer_unordered(CONCURRENT_READINGS);
        let mut discovered = 0;
        while let Some((child, reading)) = readings.next().await {
            if let Some(workspace) = rooted_at(&child, reading) {
                self.sessions.enter_discovered(&workspace, &child, root);
                discovered += 1;
            }
        }
        discovered
    }
}

#[async_trait]
impl RootScan for RegistryScan {
    async fn scan(&self, roots: &[String]) {
        let began = std::time::Instant::now();
        let discovered = join_all(roots.iter().map(|root| self.scan_root(root)))
            .await
            .into_iter()
            .sum::<usize>();
        tracing::debug!(
            roots = roots.len(),
            discovered,
            elapsed_ms = began.elapsed().as_millis(),
            "Workspace Roots discovered"
        );
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
/// Server reads paths: every child that is a directory standing directly
/// within the Root once read so — a link counting as the directory it names,
/// where that is another directly within the Root, and as nothing where it
/// names the Root itself or anywhere else — but those that are hidden and
/// those whose names are not Unicode, which no Workspace can be presented
/// by.
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
        .filter(|child| child.parent() == Some(root.as_path()))
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
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll},
    };

    use tokio::sync::{Semaphore, mpsc};

    use super::*;
    use crate::clock::ManualClock;

    /// A path rooted as this platform roots absolute paths.
    fn absolute(components: &[&str]) -> PathBuf {
        components.iter().fold(
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            |path, component| path.join(component),
        )
    }

    /// A scan that says when it begins and which Roots it reads, and runs
    /// until a test lets it finish, one permit at a time.
    struct ScriptedScan {
        begun: mpsc::UnboundedSender<Vec<String>>,
        finish: Semaphore,
    }

    #[async_trait]
    impl RootScan for ScriptedScan {
        async fn scan(&self, roots: &[String]) {
            let _ = self.begun.send(roots.to_vec());
            self.finish
                .acquire()
                .await
                .expect("the scan's permits stay open")
                .forget();
        }
    }

    /// Discovery whose scans are scripted, reading Roots `settings` names,
    /// reusing a finished scan for `memo` by a clock the test moves, and
    /// waiting on a scan no longer than `wait`.
    struct Scripted {
        discovery: RootDiscovery,
        scan: Arc<ScriptedScan>,
        begun: mpsc::UnboundedReceiver<Vec<String>>,
        settings: watch::Sender<SettingsSnapshot>,
        hand: ManualClock,
        _shutdown: watch::Sender<bool>,
    }

    fn scripted(memo: Duration, wait: Duration) -> Scripted {
        let (begun_tx, begun) = mpsc::unbounded_channel();
        let scan = Arc::new(ScriptedScan {
            begun: begun_tx,
            finish: Semaphore::new(0),
        });
        let mut snapshot = SettingsSnapshot::default();
        snapshot.settings.workspace.roots = vec!["~/projects".to_owned()];
        let (settings, settings_rx) = watch::channel(snapshot);
        let (clock, hand) = ServerClock::manual();
        let (shutdown, shutdown_rx) = watch::channel(false);
        Scripted {
            discovery: RootDiscovery::new(
                scan.clone(),
                settings_rx,
                memo,
                wait,
                clock,
                shutdown_rx,
            ),
            scan,
            begun,
            settings,
            hand,
            _shutdown: shutdown,
        }
    }

    /// Polls `future` once, answering whether it finished.
    async fn finishes_at_once(future: impl Future<Output = ()>) -> bool {
        let mut future = pin!(future);
        std::future::poll_fn(|context: &mut Context<'_>| {
            Poll::Ready(future.as_mut().poll(context).is_ready())
        })
        .await
    }

    /// How long a test waits for something it knows will happen.
    const SURELY: Duration = Duration::from_secs(10);

    /// A bound on waiting for a scan that no test reaches, so only a scan
    /// finishing ends a wait.
    const AS_LONG_AS_IT_TAKES: Duration = Duration::from_secs(60 * 60);

    #[tokio::test]
    async fn a_listing_during_a_slow_scan_joins_it_and_the_memo_runs_from_its_end() {
        let memo = Duration::from_secs(3);
        let mut scripted = scripted(memo, AS_LONG_AS_IT_TAKES);
        let first = tokio::spawn({
            let discovery = scripted.discovery.clone();
            async move { discovery.discover().await }
        });
        assert_eq!(
            scripted.begun.recv().await,
            Some(vec!["~/projects".to_owned()]),
            "the first listing begins a scan"
        );
        scripted.hand.advance(Duration::from_secs(4));
        let second = tokio::spawn({
            let discovery = scripted.discovery.clone();
            async move { discovery.discover().await }
        });
        // The second listing has made its choice once it is waiting.
        tokio::task::yield_now().await;
        assert!(!second.is_finished() && !first.is_finished());

        scripted.scan.finish.add_permits(1);
        tokio::time::timeout(SURELY, first)
            .await
            .expect("the first listing answers once the scan finishes")
            .expect("the first listing runs to its end");
        tokio::time::timeout(SURELY, second)
            .await
            .expect("the second listing answers with the scan it joined")
            .expect("the second listing runs to its end");
        assert!(
            scripted.begun.try_recv().is_err(),
            "a listing during a scan longer than the memo joins it rather than scanning again"
        );

        scripted.hand.advance(memo - Duration::from_millis(1));
        assert!(
            finishes_at_once(scripted.discovery.discover()).await,
            "a scan is reused for the memo from when it finished, though it began longer ago"
        );
        assert!(scripted.begun.try_recv().is_err());

        scripted.hand.advance(Duration::from_millis(1));
        let third = tokio::spawn({
            let discovery = scripted.discovery.clone();
            async move { discovery.discover().await }
        });
        assert!(
            tokio::time::timeout(SURELY, scripted.begun.recv())
                .await
                .is_ok(),
            "once the memo has run out, a listing scans again"
        );
        scripted.scan.finish.add_permits(1);
        third.await.expect("the third listing runs to its end");
    }

    #[tokio::test]
    async fn a_listing_waits_for_a_scan_no_longer_than_its_bound() {
        let mut scripted = scripted(Duration::ZERO, Duration::from_millis(20));
        tokio::time::timeout(SURELY, scripted.discovery.discover())
            .await
            .expect("a listing answers once its bound runs out, the scan still running");
        assert!(
            scripted.begun.try_recv().is_ok(),
            "the listing began a scan"
        );

        let ticket = scripted.discovery.rediscover();
        tokio::time::timeout(SURELY, scripted.discovery.settled(ticket))
            .await
            .expect("a fresh scan is waited on no longer than the bound either");
    }

    #[tokio::test]
    async fn a_fresh_scan_supersedes_one_still_running() {
        let mut scripted = scripted(Duration::ZERO, AS_LONG_AS_IT_TAKES);
        let listing = tokio::spawn({
            let discovery = scripted.discovery.clone();
            async move { discovery.discover().await }
        });
        assert_eq!(
            scripted.begun.recv().await,
            Some(vec!["~/projects".to_owned()])
        );

        scripted.settings.send_modify(|snapshot| {
            snapshot.settings.workspace.roots = vec!["~/notes".to_owned()];
        });
        let ticket = scripted.discovery.rediscover();
        assert_eq!(
            scripted.begun.recv().await,
            Some(vec!["~/notes".to_owned()]),
            "the changed Roots are scanned at once, the scan of the old ones still running"
        );
        scripted.scan.finish.add_permits(1);
        tokio::time::timeout(SURELY, scripted.discovery.settled(ticket))
            .await
            .expect("the scan of the changed Roots finishes on the one permit");
        tokio::time::timeout(SURELY, listing)
            .await
            .expect("a listing waiting on the superseded scan answers with its successor")
            .expect("the listing runs to its end");
        assert!(
            scripted.begun.try_recv().is_err(),
            "the superseded scan stopped, leaving the permit to its successor"
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

        let mut children = child_directories(&root).expect("read the Root");
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
            child_directories(&root).expect("read the Root"),
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
