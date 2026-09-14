use super::*;
use diesel::{Connection, RunQueryDsl, sqlite::SqliteConnection};

#[derive(Clone)]
struct ReclaimLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for ReclaimLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reclaim_log() -> &'static std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static LOG: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = ReclaimLog(log.clone());
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install Session integration Log subscriber once");
        log
    })
}

async fn wait_for_log(expectation: &str, matches: impl Fn(&str) -> bool) -> String {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let log = String::from_utf8_lossy(&reclaim_log().lock().unwrap()).into_owned();
            if matches(&log) {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Log never contained {expectation}"))
}

const STARTUP_DELAY: Duration = Duration::from_millis(120);
const RECLAIM_INTERVAL: Duration = Duration::from_millis(35);

struct ReclaimLayout {
    _temp: tempfile::TempDir,
    root: PathBuf,
    main: PathBuf,
    managed: PathBuf,
    external: PathBuf,
    config: PathBuf,
}

impl ReclaimLayout {
    fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temp.path()).unwrap();
        let main = root.join("main");
        let managed = main.join(".suru-worktrees").join(name);
        let external = root.join("external");
        let config = root.join("config");
        std::fs::create_dir(&main).unwrap();
        std::fs::create_dir(&config).unwrap();
        git(&main, &["init", "-b", "main"]);
        std::fs::write(main.join("tracked"), "initial").unwrap();
        git(&main, &["add", "."]);
        commit(&main, "initial");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                &format!("suru/{name}"),
                managed.to_str().unwrap(),
            ],
        );
        Self {
            _temp: temp,
            root,
            main,
            managed,
            external,
            config,
        }
    }

    fn server_config(&self, channel: &str) -> ServerConfig {
        ServerConfig::new(self.root.join("state"), channel)
            .unwrap()
            .with_config_dir(&self.config)
    }

    fn pin(&self, value: &str) {
        std::fs::write(
            self.config.join("suru.jsonc"),
            format!(r#"{{ "worktree": {{ "autoReclaim": {value} }} }}"#),
        )
        .unwrap();
    }
}

fn timings() -> server::ServerTimings {
    server::ServerTimings::default()
        .with_worktree_reclaim_startup_delay(STARTUP_DELAY)
        .with_worktree_reclaim_interval(RECLAIM_INTERVAL)
}

async fn spawn(layout: &ReclaimLayout, channel: &str) -> server::RunningServer {
    let (runtime, _) = ControlledProvider::new();
    let server =
        server::spawn_with_provider_and_timings(layout.server_config(channel), runtime, timings())
            .await
            .unwrap();
    remember_repository(&server, &layout.main).await;
    server
}

async fn remember_repository(server: &server::RunningServer, root: &Path) -> ResolvedWorkspace {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/workspaces/resolve",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: None,
            path: root.to_owned(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn fail_preparation(
    server: &server::RunningServer,
    main: &Path,
    description: &str,
) -> PreparedCheckout {
    let result = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: main.to_owned(),
            },
            description: description.into(),
            provider: ProviderId::new("controlled"),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert!(result.error.is_some());
    result.preparation
}

fn age_preparation(config: &ServerConfig, preparation: &PreparedCheckout) -> PathBuf {
    let intent = config
        .data_dir()
        .join("checkout-preparations")
        .join(format!("{}.json", preparation.id.0));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent).unwrap()).unwrap();
    document["persisted_at"] = serde_json::json!(0);
    std::fs::write(&intent, serde_json::to_vec(&document).unwrap()).unwrap();
    intent
}

async fn wait_for_path(path: &Path, exists: bool) {
    timeout(PROGRESS_DEADLINE, async {
        while path.exists() != exists {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} existence never became {exists}", path.display()));
}

async fn wait_for_branch(root: &Path, branch: &str, exists: bool) {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let present = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["show-ref", "--verify", "--quiet", branch])
                .status()
                .unwrap()
                .success();
            if present == exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{branch} existence never became {exists}"));
}

fn set_session_updated_at(config: &ServerConfig, session_ids: &[SessionId], updated_at: u64) {
    let mut database = SqliteConnection::establish(
        config
            .data_dir()
            .join("suru.db")
            .to_str()
            .expect("database path is UTF-8"),
    )
    .expect("open the Session database");
    for session_id in session_ids {
        diesel::sql_query("UPDATE sessions SET updated_at = ? WHERE id = ?")
            .bind::<diesel::sql_types::BigInt, _>(i64::try_from(updated_at).unwrap())
            .bind::<diesel::sql_types::Text, _>(session_id.to_string())
            .execute(&mut database)
            .expect("backdate the Session");
    }
}

async fn listed_session(descriptor: &RuntimeDescriptor, session_id: SessionId) -> SessionListItem {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Vec<SessionListItem>>()
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("Session remains listed")
}

async fn wait_for_unavailable_reclaim_reason(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> String {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let listed = listed_session(descriptor, session_id).await;
            if let SessionListItem::Readable(summary) = listed
                && let Some(CheckoutSummary {
                    availability: SourceControlAvailability::Unavailable { reason },
                    ..
                }) = summary.checkout_state
                && reason.contains("Reclaim")
            {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("catalog reports the durable Reclaim reason")
}

async fn wait_for_reclaim_phase(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    phase: CheckoutReclaimPhase,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = support::read_session(descriptor, session_id).await;
            if snapshot
                .session
                .checkout
                .as_ref()
                .and_then(|checkout| checkout.reclaim.as_ref())
                .is_some_and(|reclaim| reclaim.phase == phase)
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Session persists the expected Reclaim phase")
}

struct DelayedAvailableObservation {
    git: GitSourceControl,
    delayed: AtomicBool,
    captured: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SourceControl for DelayedAvailableObservation {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }

    async fn observe(&self, checkout: &CheckoutAssociation) -> CheckoutSummary {
        let reading = self.git.observe(checkout).await;
        if reading.availability == SourceControlAvailability::Available
            && !self.delayed.swap(true, Ordering::SeqCst)
        {
            self.captured.notify_one();
            self.release.notified().await;
        }
        reading
    }

    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.git.inspect_removal(target).await
    }

    async fn removal_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git.removal_branch_outcome(target, inspection).await
    }

    async fn reclaim_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        branch_outcome: CheckoutBranchOutcome,
        preparations: &[PreparedCheckout],
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git
            .reclaim_checkout(target, inspection, branch_outcome, preparations)
            .await
    }
}

struct ObservationDuringRemoval {
    git: GitSourceControl,
    captured: tokio::sync::Notify,
    release: tokio::sync::Notify,
    delivered: AtomicBool,
    processed: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SourceControl for ObservationDuringRemoval {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }

    async fn observe(&self, checkout: &CheckoutAssociation) -> CheckoutSummary {
        let reading = self.git.observe(checkout).await;
        if checkout
            .reclaim
            .as_ref()
            .is_some_and(|reclaim| reclaim.phase == CheckoutReclaimPhase::Removing)
            && reading.availability == SourceControlAvailability::Available
            && !self.delivered.swap(true, Ordering::SeqCst)
        {
            self.captured.notify_one();
            self.release.notified().await;
        } else if checkout
            .reclaim
            .as_ref()
            .is_some_and(|reclaim| reclaim.phase == CheckoutReclaimPhase::Removed)
            && self.delivered.load(Ordering::SeqCst)
        {
            // Reaching the next tick proves the delayed result was handed to
            // the Session store before this fresh missing-path observation.
            self.processed.notify_one();
        }
        reading
    }

    async fn recover_checkout(
        &self,
        repository: &Repository,
        checkout: &CheckoutAssociation,
    ) -> Result<CheckoutRecovery, String> {
        self.git.recover_checkout(repository, checkout).await
    }

    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.git.inspect_removal(target).await
    }

    async fn removal_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git.removal_branch_outcome(target, inspection).await
    }

    async fn reclaim_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        branch_outcome: CheckoutBranchOutcome,
        preparations: &[PreparedCheckout],
    ) -> Result<CheckoutBranchOutcome, String> {
        self.captured.notified().await;
        self.git
            .reclaim_checkout(target, inspection, branch_outcome, preparations)
            .await
    }
}

#[tokio::test]
async fn an_unsettled_session_idle_past_the_threshold_is_reclaimed() {
    let layout = ReclaimLayout::new("idle-unsettled");
    layout.pin("1");
    let config = layout.server_config("reclaim-idle-unsettled");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(config.clone(), runtime, timings())
        .await
        .unwrap();
    let (session, _connection) = open(&server, &mut provider, &layout.managed).await;
    assert_eq!(
        listed_session(server.descriptor(), session.session.id)
            .await
            .settled_at(),
        None
    );
    server.shutdown().await.unwrap();
    set_session_updated_at(&config, &[session.session.id], 0);

    let stale = Arc::new(DelayedAvailableObservation {
        git: GitSourceControl::default(),
        delayed: AtomicBool::new(false),
        captured: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (runtime, _provider) = ControlledProvider::new();
    let first_restart = server::spawn_with_source_control(
        config.clone(),
        vec![runtime],
        timings().with_checkout_observation_interval(Duration::from_millis(5)),
        stale.clone(),
    )
    .await
    .unwrap();
    let (_ids, _catalog) =
        support::open_catalog_stream_with_snapshot(first_restart.descriptor()).await;
    timeout(PROGRESS_DEADLINE, stale.captured.notified())
        .await
        .expect("capture an Available observation before Reclaim");
    first_restart.workspace_discovery_settled().await;
    wait_for_path(&layout.managed, false).await;
    stale.release.notify_one();
    let retained = listed_session(first_restart.descriptor(), session.session.id).await;
    assert_eq!(
        retained.settled_at(),
        None,
        "Reclaim does not settle a Session"
    );
    assert_eq!(
        retained.updated_at(),
        SessionTimestamp(0),
        "Reclaim does not count as Session activity"
    );
    let reason =
        wait_for_unavailable_reclaim_reason(first_restart.descriptor(), session.session.id).await;
    assert!(reason.contains("1 day"), "threshold is named: {reason}");
    first_restart.shutdown().await.unwrap();

    let (runtime, mut provider) = ControlledProvider::new();
    let recovered = server::spawn_with_provider_and_timings(config, runtime, timings())
        .await
        .unwrap();
    let (_ids, _catalog) = support::open_catalog_stream_with_snapshot(recovered.descriptor()).await;
    assert!(
        wait_for_unavailable_reclaim_reason(recovered.descriptor(), session.session.id)
            .await
            .contains("1 day"),
        "the Reclaim cause survives restart and fresh observation"
    );
    assert_success(
        admit(
            recovered.descriptor(),
            session.session.id,
            prompt("Resume from the retained branch tip"),
        )
        .await,
    )
    .await;
    let _resumed = restarted(&mut provider, &layout.managed).await;
    assert_eq!(
        read_git(&layout.managed, &["branch", "--show-current"]),
        "suru/idle-unsettled"
    );
    let history = support::read_session_until(
        &reqwest::Client::new(),
        recovered.descriptor(),
        session.session.id,
        "recovered Turn completes",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(history.prompts[0].text, "Original history");
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn latest_session_activity_and_a_live_numeric_threshold_govern_shared_idle_reclaim() {
    let layout = ReclaimLayout::new("latest-session");
    layout.pin("2");
    let config = layout.server_config("reclaim-latest-session");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(config.clone(), runtime, timings())
        .await
        .unwrap();
    let (old, _old_connection) = open(&server, &mut provider, &layout.managed).await;
    let (recent, _recent_connection) = open(&server, &mut provider, &layout.managed).await;
    server.shutdown().await.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    set_session_updated_at(&config, &[old.session.id], 0);
    set_session_updated_at(
        &config,
        &[recent.session.id],
        now.saturating_sub(36 * 60 * 60 * 1_000),
    );

    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider_and_timings(config, runtime, timings())
        .await
        .unwrap();
    restarted.workspace_discovery_settled().await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(
        layout.managed.is_dir(),
        "the latest sibling is inside the two-day threshold"
    );
    reqwest::Client::new()
        .post(format!("{}/v1/settings", restarted.descriptor().base_url))
        .bearer_auth(&restarted.descriptor().token)
        .json(&SettingMutation::WorktreeAutoReclaim {
            value: Some(AutoReclaim::AfterDays(1)),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_for_path(&layout.managed, false).await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_observation_started_during_idle_reclaim_cannot_replace_detached_recovery() {
    let layout = ReclaimLayout::new("idle-merged");
    layout.pin("1");
    let base = read_git(&layout.main, &["rev-parse", "main"]);
    git(
        &layout.main,
        &["config", "branch.suru/idle-merged.suru-base", &base],
    );
    git(
        &layout.main,
        &["config", "branch.suru/idle-merged.suru-base-branch", "main"],
    );
    std::fs::write(layout.managed.join("tracked"), "merged managed work").unwrap();
    git(&layout.managed, &["add", "."]);
    commit(&layout.managed, "managed work merged into base");
    let merged_commit = read_git(&layout.managed, &["rev-parse", "HEAD"]);
    git(&layout.main, &["merge", "--ff-only", "suru/idle-merged"]);
    let config = layout.server_config("reclaim-idle-merged");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(config.clone(), runtime, timings())
        .await
        .unwrap();
    let (session, _connection) = open(&server, &mut provider, &layout.managed).await;
    server.shutdown().await.unwrap();
    set_session_updated_at(&config, &[session.session.id], 0);

    let source_control = Arc::new(ObservationDuringRemoval {
        git: GitSourceControl::default(),
        captured: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        delivered: AtomicBool::new(false),
        processed: tokio::sync::Notify::new(),
    });
    let (runtime, mut provider) = ControlledProvider::new();
    let running = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings().with_checkout_observation_interval(Duration::from_millis(5)),
        source_control.clone(),
    )
    .await
    .unwrap();
    let (_ids, _catalog) = support::open_catalog_stream_with_snapshot(running.descriptor()).await;
    wait_for_path(&layout.managed, false).await;
    wait_for_branch(&layout.main, "refs/heads/suru/idle-merged", false).await;
    wait_for_reclaim_phase(
        running.descriptor(),
        session.session.id,
        CheckoutReclaimPhase::Removed,
    )
    .await;
    source_control.release.notify_one();
    timeout(PROGRESS_DEADLINE, source_control.processed.notified())
        .await
        .expect("the stale Available reading is recorded before another observation tick");
    let retained = support::read_session(running.descriptor(), session.session.id).await;
    let reclaim = retained
        .session
        .checkout
        .as_ref()
        .and_then(|checkout| checkout.reclaim.as_ref())
        .expect("the completed Reclaim cause survives the stale Available reading");
    assert_eq!(reclaim.phase, CheckoutReclaimPhase::Removed);
    assert!(reclaim.reason.contains("1 day"));
    assert!(matches!(
        retained
            .session
            .checkout
            .and_then(|checkout| checkout.recovery_revision),
        Some(CheckoutRevision::Detached { ref commit }) if commit == &merged_commit
    ));
    assert_success(
        admit(
            running.descriptor(),
            session.session.id,
            prompt("Recover the merged Reclaimed worktree"),
        )
        .await,
    )
    .await;
    let _resumed = restarted(&mut provider, &layout.managed).await;
    assert_eq!(read_git(&layout.managed, &["branch", "--show-current"]), "");
    assert_eq!(
        read_git(&layout.managed, &["rev-parse", "HEAD"]),
        merged_commit
    );
    let history = support::read_session_until(
        &reqwest::Client::new(),
        running.descriptor(),
        session.session.id,
        "detached recovery Turn completes",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(history.prompts[0].text, "Original history");
    running.shutdown().await.unwrap();
}

struct PauseAfterCandidateObservation {
    git: GitSourceControl,
    paused: AtomicBool,
    inspected: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SourceControl for PauseAfterCandidateObservation {
    async fn reclaim_candidate_observed(&self, _target: &CheckoutRemovalTarget) {
        if !self.paused.swap(true, Ordering::SeqCst) {
            self.inspected.notify_one();
            self.resume.notified().await;
        }
    }

    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }

    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.git.inspect_removal(target).await
    }

    async fn removal_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git.removal_branch_outcome(target, inspection).await
    }

    async fn reclaim_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        branch_outcome: CheckoutBranchOutcome,
        preparations: &[PreparedCheckout],
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git
            .reclaim_checkout(target, inspection, branch_outcome, preparations)
            .await
    }
}

#[tokio::test]
async fn candidate_admission_leaving_a_surviving_subagent_is_rechecked_before_removal() {
    let layout = ReclaimLayout::new("working-race");
    layout.pin("1");
    let config = layout.server_config("reclaim-working-race");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(config.clone(), runtime, timings())
        .await
        .unwrap();
    let (session, _connection) = open(&server, &mut provider, &layout.managed).await;
    server.shutdown().await.unwrap();
    set_session_updated_at(&config, &[session.session.id], 0);

    let adapter = Arc::new(PauseAfterCandidateObservation {
        git: GitSourceControl::default(),
        paused: AtomicBool::new(false),
        inspected: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let (runtime, mut provider) = ControlledProvider::new();
    let running =
        server::spawn_with_source_control(config, vec![runtime], timings(), adapter.clone())
            .await
            .unwrap();
    timeout(PROGRESS_DEADLINE, adapter.inspected.notified())
        .await
        .expect("Reclaim observes the idle candidate");
    assert_success(
        admit(
            running.descriptor(),
            session.session.id,
            prompt("Become Working before guarded removal"),
        )
        .await,
    )
    .await;
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("admitted Prompt starts its Provider");
    let mut connection = start.succeed(identity());
    timeout(PROGRESS_DEADLINE, connection.next_turn())
        .await
        .expect("admitted Prompt reaches the Provider")
        .succeed();
    let child = suru::provider::ProviderSubagentId::new("reclaim-race-child");
    connection
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: child,
            name: "Protect checkout".into(),
            description: "Survive the parent Turn".into(),
        })
        .await;
    connection
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    support::read_session_until(
        &reqwest::Client::new(),
        running.descriptor(),
        session.session.id,
        "parent Turn settles while its Subagent survives",
        |snapshot| {
            snapshot
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
                && snapshot.session.working_since.is_some()
        },
    )
    .await;
    assert!(
        listed_session(running.descriptor(), session.session.id)
            .await
            .working_since()
            .is_some(),
        "the surviving Subagent keeps its parent Working before guarded removal"
    );
    adapter.resume.notify_one();
    tokio::time::sleep(RECLAIM_INTERVAL * 3).await;
    assert!(
        layout.managed.is_dir(),
        "the last-moment Working check protects the checkout"
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_discovers_and_reclaims_a_prepared_worktree_after_its_last_session_is_deleted() {
    let layout = ReclaimLayout::new("unrelated-startup-orphan");
    layout.pin(r#""off""#);
    let config = layout.server_config("reclaim-prepared-after-restart");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(config.clone(), runtime, timings())
        .await
        .unwrap();

    // This surviving Session is the only durable fact needed for startup
    // discovery to remember the Repository after restart.
    let (_main, _main_connection) = open(&server, &mut provider, &layout.main).await;
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: layout.main.clone(),
            },
            description: "Reclaim after restart".into(),
            provider: ProviderId::new("controlled"),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert_eq!(prepared.error, None);
    let CheckoutPreparationPlan::Git { branch, .. } = &prepared.preparation.plan;
    let branch = format!("refs/heads/{branch}");
    let managed = prepared.preparation.destination.path.clone();
    let session = support::create_session(
        server.descriptor(),
        &CreateSessionRequest {
            preparation_id: Some(prepared.preparation.id),
            agent_selection: None,
            execution_directory: prepared.preparation.destination,
            prompt: prompt("Finish and delete this managed Session"),
        },
    )
    .await;
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .unwrap();
    let mut managed_connection = start.succeed_with_resume(identity(), Some(resume()));
    timeout(PROGRESS_DEADLINE, managed_connection.next_turn())
        .await
        .unwrap()
        .succeed();
    managed_connection
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let deleted = reqwest::Client::new()
        .delete(format!(
            "{}/v1/sessions/{}",
            server.descriptor().base_url,
            session.session.id
        ))
        .bearer_auth(&server.descriptor().token)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(managed.is_dir(), "off keeps the orphan until restart");
    server.shutdown().await.unwrap();

    layout.pin("14");
    let (runtime, _provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider_and_timings(config, runtime, timings())
        .await
        .unwrap();
    restarted.workspace_discovery_settled().await;
    // No resolve call or catalog subscription follows restart: discovery from
    // the surviving Session must be enough to feed the dedicated startup pass.
    wait_for_path(&managed, false).await;
    wait_for_branch(&layout.main, &branch, false).await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_reclaims_an_orphan_without_an_attached_client_and_retains_an_unproven_branch() {
    let layout = ReclaimLayout::new("startup-orphan");
    let branch = "refs/heads/suru/startup-orphan";
    let server = spawn(&layout, "reclaim-startup-orphan").await;

    wait_for_path(&layout.managed, false).await;
    assert!(!read_git(&layout.main, &["rev-parse", branch]).is_empty());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn off_disables_startup_and_interval_reclaim_then_a_live_threshold_enables_the_next_pass() {
    let layout = ReclaimLayout::new("live-setting");
    layout.pin(r#""off""#);
    let server = spawn(&layout, "reclaim-live-setting").await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(layout.managed.is_dir(), "off disables every pass");

    reqwest::Client::new()
        .post(format!("{}/v1/settings", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .json(&SettingMutation::WorktreeAutoReclaim {
            value: Some(AutoReclaim::AfterDays(3)),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_for_path(&layout.managed, false).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn turning_reclaim_off_live_stops_the_pending_startup_and_interval_passes() {
    let layout = ReclaimLayout::new("live-off");
    let server = spawn(&layout, "reclaim-live-off").await;
    reqwest::Client::new()
        .post(format!("{}/v1/settings", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .json(&SettingMutation::WorktreeAutoReclaim {
            value: Some(AutoReclaim::Off),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(layout.managed.is_dir());
    server.shutdown().await.unwrap();
}

async fn excluded(name: &str, arrange: impl FnOnce(&ReclaimLayout)) {
    let layout = ReclaimLayout::new(name);
    arrange(&layout);
    let server = spawn(&layout, &format!("reclaim-excluded-{name}")).await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(layout.managed.is_dir(), "{name} must exclude Reclaim");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tracked_change_excludes_reclaim() {
    excluded("tracked", |layout| {
        std::fs::write(layout.managed.join("tracked"), "changed").unwrap();
    })
    .await;
}

#[tokio::test]
async fn an_untracked_file_excludes_reclaim() {
    excluded("untracked", |layout| {
        std::fs::write(layout.managed.join("personal"), "keep").unwrap();
    })
    .await;
}

#[tokio::test]
async fn an_initialized_submodule_excludes_reclaim() {
    let layout = ReclaimLayout::new("submodule");
    let module = layout.root.join("module");
    std::fs::create_dir(&module).unwrap();
    git(&module, &["init", "-b", "main"]);
    std::fs::write(module.join("contents"), "module").unwrap();
    git(&module, &["add", "."]);
    commit(&module, "module");
    git(
        &layout.managed,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            module.to_str().unwrap(),
            "module",
        ],
    );
    commit(&layout.managed, "add module");
    let server = spawn(&layout, "reclaim-excluded-submodule").await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(layout.managed.join("module/contents").is_file());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_external_git_lock_excludes_reclaim_and_release_allows_a_later_pass() {
    let layout = ReclaimLayout::new("external-lock");
    git(
        &layout.main,
        &[
            "worktree",
            "lock",
            "--reason",
            "reader pin",
            layout.managed.to_str().unwrap(),
        ],
    );
    let server = spawn(&layout, "reclaim-external-lock").await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(layout.managed.is_dir());
    git(
        &layout.main,
        &["worktree", "unlock", layout.managed.to_str().unwrap()],
    );
    wait_for_path(&layout.managed, false).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ignored_files_do_not_exclude_nonforce_reclaim() {
    let layout = ReclaimLayout::new("ignored");
    std::fs::write(layout.main.join(".git/info/exclude"), "generated\n").unwrap();
    std::fs::write(layout.managed.join("generated"), "reproducible").unwrap();
    let server = spawn(&layout, "reclaim-ignored").await;
    wait_for_path(&layout.managed, false).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_exact_suru_recovery_lock_does_not_exclude_nonforce_reclaim() {
    let layout = ReclaimLayout::new("recovery-lock");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_provider_and_timings(
        layout.server_config("reclaim-recovery-lock"),
        runtime,
        timings(),
    )
    .await
    .unwrap();
    let resolved = remember_repository(&server, &layout.main).await;
    let checkout = resolved
        .checkouts
        .iter()
        .find(|checkout| checkout.association.root == layout.managed)
        .unwrap();
    let reason = format!("suru-recovery:{}", checkout.association.id.0);
    git(
        &layout.main,
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            layout.managed.to_str().unwrap(),
        ],
    );
    wait_for_path(&layout.managed, false).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_suru_prefixed_but_nonmatching_lock_remains_external() {
    excluded("false-owned-lock", |layout| {
        git(
            &layout.main,
            &[
                "worktree",
                "lock",
                "--reason",
                "suru-recovery:not-this-checkout",
                layout.managed.to_str().unwrap(),
            ],
        );
    })
    .await;
}

#[tokio::test]
async fn main_and_external_linked_worktrees_are_never_candidates() {
    let layout = ReclaimLayout::new("managed-candidate");
    git(
        &layout.main,
        &[
            "worktree",
            "add",
            "-b",
            "external-topic",
            layout.external.to_str().unwrap(),
        ],
    );
    let server = spawn(&layout, "reclaim-boundaries").await;
    wait_for_path(&layout.managed, false).await;
    assert!(layout.main.join("tracked").is_file());
    assert!(layout.external.join("tracked").is_file());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_fully_merged_recorded_branch_is_deleted() {
    let layout = ReclaimLayout::new("merged");
    let base = read_git(&layout.main, &["rev-parse", "main"]);
    git(
        &layout.main,
        &["config", "branch.suru/merged.suru-base", &base],
    );
    git(
        &layout.main,
        &["config", "branch.suru/merged.suru-base-branch", "main"],
    );
    let server = spawn(&layout, "reclaim-merged-branch").await;
    wait_for_path(&layout.managed, false).await;
    wait_for_branch(&layout.main, "refs/heads/suru/merged", false).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unmerged_recorded_branch_is_retained() {
    let layout = ReclaimLayout::new("unmerged");
    let base = read_git(&layout.main, &["rev-parse", "main"]);
    git(
        &layout.main,
        &["config", "branch.suru/unmerged.suru-base", &base],
    );
    git(
        &layout.main,
        &["config", "branch.suru/unmerged.suru-base-branch", "main"],
    );
    std::fs::write(layout.managed.join("tracked"), "committed divergence").unwrap();
    git(&layout.managed, &["add", "."]);
    commit(&layout.managed, "unmerged work");
    let server = spawn(&layout, "reclaim-unmerged-branch").await;
    wait_for_path(&layout.managed, false).await;
    assert!(!read_git(&layout.main, &["rev-parse", "refs/heads/suru/unmerged"]).is_empty());
    server.shutdown().await.unwrap();
}

struct FailFirstReclaim {
    git: GitSourceControl,
    failed: AtomicBool,
}

struct FailFirstPreparationRetirement {
    git: GitSourceControl,
    failed: AtomicBool,
    reached_retry: tokio::sync::Notify,
    release_retry: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl SourceControl for FailFirstPreparationRetirement {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }

    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.git.inspect_removal(target).await
    }

    async fn removal_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git.removal_branch_outcome(target, inspection).await
    }

    async fn reclaim_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        branch_outcome: CheckoutBranchOutcome,
        preparations: &[PreparedCheckout],
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git
            .reclaim_checkout(target, inspection, branch_outcome, preparations)
            .await
    }

    async fn retire_preparation(
        &self,
        preparation: &PreparedCheckout,
        retire_branch: bool,
    ) -> Result<CheckoutBranchOutcome, String> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            return Err("simulated ownership-ref retirement failure".into());
        }
        self.reached_retry.notify_one();
        self.release_retry.acquire().await.unwrap().forget();
        self.git
            .retire_preparation(preparation, retire_branch)
            .await
    }
}

#[async_trait::async_trait]
impl SourceControl for FailFirstReclaim {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }

    async fn inspect_removal(
        &self,
        target: &CheckoutRemovalTarget,
    ) -> Result<CheckoutRemovalInspection, String> {
        self.git.inspect_removal(target).await
    }

    async fn removal_branch_outcome(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
    ) -> Result<CheckoutBranchOutcome, String> {
        self.git.removal_branch_outcome(target, inspection).await
    }

    async fn reclaim_checkout(
        &self,
        target: &CheckoutRemovalTarget,
        inspection: &CheckoutRemovalInspection,
        branch_outcome: CheckoutBranchOutcome,
        preparations: &[PreparedCheckout],
    ) -> Result<CheckoutBranchOutcome, String> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            return Err("simulated transient Git refusal".into());
        }
        self.git
            .reclaim_checkout(target, inspection, branch_outcome, preparations)
            .await
    }
}

#[tokio::test]
async fn a_failed_removal_is_retried_on_a_later_interval() {
    let layout = ReclaimLayout::new("retry");
    let log = reclaim_log();
    log.lock().unwrap().clear();
    let (runtime, _) = ControlledProvider::new();
    let adapter = Arc::new(FailFirstReclaim {
        git: GitSourceControl::default(),
        failed: AtomicBool::new(false),
    });
    let server = server::spawn_with_source_control(
        layout.server_config("reclaim-retry"),
        vec![runtime],
        timings(),
        adapter.clone(),
    )
    .await
    .unwrap();
    remember_repository(&server, &layout.main).await;

    wait_for_path(&layout.managed, false).await;
    assert!(adapter.failed.load(Ordering::SeqCst));
    let path = layout.managed.display().to_string();
    let rendered = wait_for_log("this Worktree's failure and success", |log| {
        log.lines().any(|line| {
            line.contains(&path) && line.contains("Managed Worktree Reclaim failed; will retry")
        }) && log
            .lines()
            .any(|line| line.contains(&path) && line.contains("Managed Worktree Reclaimed"))
    })
    .await;
    let lines = rendered
        .lines()
        .filter(|line| line.contains(&path))
        .collect::<Vec<_>>();
    assert!(
        lines.iter().any(|line| {
            line.contains("Managed Worktree Reclaim failed; will retry")
                && line.contains("simulated transient Git refusal")
                && line.contains("rule=\"orphaned\"")
        }),
        "failure Log names its path, rule, and retry: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| {
            line.contains("Managed Worktree Reclaimed")
                && line.contains("rule=\"orphaned\"")
                && line.contains("branch_outcome=\"retained\"")
        }),
        "success Log names its path, rule, and actual branch outcome: {lines:?}"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_post_removal_metadata_retirement_retries_from_the_persisted_intent() {
    let layout = ReclaimLayout::new("failed-retirement-retry");
    let config = layout.server_config("reclaim-failed-retirement-retry");
    let (runtime, _) = ControlledProvider::new();
    let preparing = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default().with_preparation_observer(FailOnceAt::new(
                PreparationCheckpoint::SessionPersisted,
            )),
        ),
    )
    .await
    .unwrap();
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            preparing.descriptor().base_url
        ))
        .bearer_auth(&preparing.descriptor().token)
        .json(&PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: layout.main.clone(),
            },
            description: "retry metadata retirement".into(),
            provider: ProviderId::new("controlled"),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert_eq!(prepared.error, None);
    let withheld = prompt("Retry this withheld Prompt");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", preparing.descriptor().base_url))
        .bearer_auth(&preparing.descriptor().token)
        .json(&CreateSessionRequest {
            preparation_id: Some(prepared.preparation.id),
            agent_selection: None,
            execution_directory: prepared.preparation.destination.clone(),
            prompt: withheld.clone(),
        })
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(response.text().await.unwrap().contains("SessionPersisted"));
    let preparation = prepared.preparation;
    let intent = age_preparation(&config, &preparation);
    let ownership = format!("refs/suru/preparations/{}", preparation.id.0.simple());
    preparing.shutdown().await.unwrap();

    let adapter = Arc::new(FailFirstPreparationRetirement {
        git: GitSourceControl::default(),
        failed: AtomicBool::new(false),
        reached_retry: Default::default(),
        release_retry: tokio::sync::Semaphore::new(0),
    });
    let server =
        server::spawn_with_source_control(config, vec![runtime], timings(), adapter.clone())
            .await
            .unwrap();
    server.workspace_discovery_settled().await;
    timeout(PROGRESS_DEADLINE, adapter.reached_retry.notified())
        .await
        .expect("a later pass reaches metadata retirement again");
    assert!(!preparation.destination.path.exists());
    assert!(intent.is_file());
    assert!(!read_git(&layout.main, &["rev-parse", &ownership]).is_empty());
    let removed = support::read_session(server.descriptor(), preparation.intended_session).await;
    let reclaim = removed
        .session
        .checkout
        .as_ref()
        .and_then(|checkout| checkout.reclaim.as_ref())
        .expect("the public Session records its durable Reclaim reason");
    assert_eq!(reclaim.phase, CheckoutReclaimPhase::Removed);
    assert!(reclaim.reason.contains("preparation failed 14 days ago"));
    assert_eq!(removed.prompts[0].status, PromptStatus::Pending);
    adapter.release_retry.add_permits(1);
    wait_for_path(&intent, false).await;
    wait_for_branch(&layout.main, &ownership, false).await;
    let retired = support::read_session(server.descriptor(), preparation.intended_session).await;
    assert_eq!(retired.prompts[0].id, withheld.id);
    assert_eq!(retired.prompts[0].status, PromptStatus::Cancelled);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unfinished_preparation_is_not_an_immediate_orphan() {
    let layout = ReclaimLayout::new("unrelated-orphan");
    let config = layout.server_config("reclaim-unfinished-preparation");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::CheckoutCreated)),
        ),
    )
    .await
    .unwrap();
    remember_repository(&server, &layout.main).await;
    let request = PrepareCheckoutRequest {
        id: Default::default(),
        source: ExecutionDirectory {
            path: layout.main.clone(),
        },
        description: "unfinished preparation".into(),
        provider: ProviderId::new("controlled"),
    };
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert!(prepared.error.is_some());
    assert!(prepared.preparation.persisted_at.is_some());
    let unfinished = prepared.preparation.destination.path.clone();
    assert!(unfinished.is_dir());

    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 3).await;
    assert!(
        unfinished.is_dir(),
        "a young failed preparation is preserved"
    );
    server.shutdown().await.unwrap();

    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 2).await;
    assert!(unfinished.is_dir());
    let retried = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            restarted.descriptor().base_url
        ))
        .bearer_auth(&restarted.descriptor().token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert_eq!(retried.error, None);
    assert_eq!(retried.preparation.id, prepared.preparation.id);
    assert_eq!(retried.preparation.destination.path, unfinished);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_reclaims_a_legacy_failed_preparation_without_a_catalogued_repository() {
    let layout = ReclaimLayout::new("failed-preparation-discovery");
    let config = layout.server_config("reclaim-failed-preparation-discovery");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::CheckoutCreated)),
        ),
    )
    .await
    .unwrap();
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: layout.main.clone(),
            },
            description: "old failed preparation".into(),
            provider: ProviderId::new("controlled"),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert!(prepared.error.is_some());
    let destination = prepared.preparation.destination.path.clone();
    let CheckoutPreparationPlan::Git { branch, .. } = &prepared.preparation.plan;
    let branch = format!("refs/heads/{branch}");
    let ownership = format!(
        "refs/suru/preparations/{}",
        prepared.preparation.id.0.simple()
    );
    let intent = config
        .data_dir()
        .join("checkout-preparations")
        .join(format!("{}.json", prepared.preparation.id.0));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent).unwrap()).unwrap();
    document.as_object_mut().unwrap().remove("persisted_at");
    std::fs::write(&intent, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(destination.is_dir());
    assert!(!read_git(&layout.main, &["rev-parse", &ownership]).is_empty());
    server.shutdown().await.unwrap();

    // No Workspace resolve or catalog subscription follows restart. The
    // intent is the only durable Repository discovery fact left to the pass.
    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    wait_for_path(&destination, false).await;
    wait_for_branch(&layout.main, &branch, false).await;
    wait_for_branch(&layout.main, &ownership, false).await;
    wait_for_path(&intent, false).await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_reclaims_an_old_branch_only_preparation_and_its_merged_branch() {
    let layout = ReclaimLayout::new("failed-branch-only");
    let config = layout.server_config("reclaim-failed-branch-only");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::BranchCreated)),
        ),
    )
    .await
    .unwrap();
    let preparation = fail_preparation(&server, &layout.main, "branch-only failure").await;
    let intent = age_preparation(&config, &preparation);
    let CheckoutPreparationPlan::Git { branch, .. } = &preparation.plan;
    let branch = format!("refs/heads/{branch}");
    let ownership = format!("refs/suru/preparations/{}", preparation.id.0.simple());
    assert!(!preparation.destination.path.exists());
    assert!(!read_git(&layout.main, &["rev-parse", &branch]).is_empty());
    assert!(!read_git(&layout.main, &["rev-parse", &ownership]).is_empty());
    server.shutdown().await.unwrap();

    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    wait_for_path(&intent, false).await;
    wait_for_branch(&layout.main, &branch, false).await;
    wait_for_branch(&layout.main, &ownership, false).await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn branch_only_reclaim_retires_metadata_but_preserves_unmerged_commits() {
    let layout = ReclaimLayout::new("failed-branch-only-unmerged");
    let config = layout.server_config("reclaim-failed-branch-only-unmerged");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::BranchCreated)),
        ),
    )
    .await
    .unwrap();
    let preparation = fail_preparation(&server, &layout.main, "unmerged branch-only failure").await;
    let intent = age_preparation(&config, &preparation);
    let CheckoutPreparationPlan::Git { branch, .. } = &preparation.plan;
    let branch_ref = format!("refs/heads/{branch}");
    let ownership = format!("refs/suru/preparations/{}", preparation.id.0.simple());
    let authoring = layout.root.join("author-unmerged");
    git(
        &layout.main,
        &["worktree", "add", authoring.to_str().unwrap(), branch],
    );
    std::fs::write(authoring.join("tracked"), "user commit").unwrap();
    git(&authoring, &["add", "."]);
    commit(&authoring, "unmerged user work");
    let unmerged_tip = read_git(&authoring, &["rev-parse", "HEAD"]);
    git(
        &layout.main,
        &["worktree", "remove", authoring.to_str().unwrap()],
    );
    server.shutdown().await.unwrap();

    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    wait_for_path(&intent, false).await;
    wait_for_branch(&layout.main, &ownership, false).await;
    assert_eq!(
        read_git(&layout.main, &["rev-parse", &branch_ref]),
        unmerged_tip
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn reclaim_preserves_an_interrupted_session_shell_and_cancels_and_logs_its_prompt() {
    let layout = ReclaimLayout::new("failed-session-shell");
    let config = layout.server_config("reclaim-failed-session-shell");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default().with_preparation_observer(FailOnceAt::new(
                PreparationCheckpoint::SessionPersisted,
            )),
        ),
    )
    .await
    .unwrap();
    let request = PrepareCheckoutRequest {
        id: Default::default(),
        source: ExecutionDirectory {
            path: layout.main.clone(),
        },
        description: "failed Session shell".into(),
        provider: ProviderId::new("controlled"),
    };
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert_eq!(prepared.error, None);
    let withheld = prompt("Keep this exact first line\nDo not quote this second line");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .json(&CreateSessionRequest {
            preparation_id: Some(prepared.preparation.id),
            agent_selection: None,
            execution_directory: prepared.preparation.destination.clone(),
            prompt: withheld.clone(),
        })
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(response.text().await.unwrap().contains("SessionPersisted"));
    assert!(provider.try_next_start().is_none());
    let intent = config
        .data_dir()
        .join("checkout-preparations")
        .join(format!("{}.json", prepared.preparation.id.0));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&intent).unwrap()).unwrap();
    document["persisted_at"] = serde_json::json!(0);
    std::fs::write(&intent, serde_json::to_vec(&document).unwrap()).unwrap();
    server.shutdown().await.unwrap();
    reclaim_log().lock().unwrap().clear();

    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    wait_for_path(&prepared.preparation.destination.path, false).await;
    wait_for_path(&intent, false).await;
    let restored = support::read_session(
        restarted.descriptor(),
        prepared.preparation.intended_session,
    )
    .await;
    assert_eq!(restored.prompts.len(), 1);
    assert_eq!(restored.prompts[0].id, withheld.id);
    assert_eq!(restored.prompts[0].text, withheld.text);
    assert_eq!(restored.prompts[0].status, PromptStatus::Cancelled);
    assert!(restored.turns.is_empty());
    assert!(provider.try_next_start().is_none());
    let log = wait_for_log("the withheld Prompt's first line", |log| {
        log.contains("Failed Worktree preparation intent retired")
            && log.contains("Keep this exact first line")
    })
    .await;
    assert!(!log.contains("Do not quote this second line"));
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn off_preserves_an_old_failed_preparation() {
    let layout = ReclaimLayout::new("failed-preparation-off");
    layout.pin(r#""off""#);
    let config = layout.server_config("reclaim-failed-preparation-off");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::CheckoutCreated)),
        ),
    )
    .await
    .unwrap();
    let preparation = fail_preparation(&server, &layout.main, "off keeps this intent").await;
    let intent = age_preparation(&config, &preparation);
    let CheckoutPreparationPlan::Git { branch, .. } = &preparation.plan;
    let ownership = format!("refs/suru/preparations/{}", preparation.id.0.simple());

    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 3).await;
    assert!(preparation.destination.path.is_dir());
    assert!(intent.is_file());
    assert!(
        !read_git(
            &layout.main,
            &["rev-parse", &format!("refs/heads/{branch}")]
        )
        .is_empty()
    );
    assert!(!read_git(&layout.main, &["rev-parse", &ownership]).is_empty());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_external_lock_preserves_an_old_failed_preparation_but_its_exact_suru_lock_does_not() {
    let layout = ReclaimLayout::new("failed-preparation-locks");
    let config = layout.server_config("reclaim-failed-preparation-locks");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::CheckoutCreated)),
        ),
    )
    .await
    .unwrap();
    let preparation = fail_preparation(&server, &layout.main, "locked failed intent").await;
    let intent = age_preparation(&config, &preparation);
    git(
        &layout.main,
        &[
            "worktree",
            "lock",
            "--reason",
            "reader pin",
            preparation.destination.path.to_str().unwrap(),
        ],
    );
    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 3).await;
    assert!(preparation.destination.path.is_dir());
    assert!(intent.is_file());
    server.shutdown().await.unwrap();

    let pointer = std::fs::read_to_string(preparation.destination.path.join(".git")).unwrap();
    let metadata = PathBuf::from(
        pointer
            .trim_end_matches(['\r', '\n'])
            .strip_prefix("gitdir: ")
            .unwrap(),
    );
    let own_reason = std::fs::read_to_string(metadata.join("suru-preparation")).unwrap();
    git(
        &layout.main,
        &[
            "worktree",
            "unlock",
            preparation.destination.path.to_str().unwrap(),
        ],
    );
    git(
        &layout.main,
        &[
            "worktree",
            "lock",
            "--reason",
            &own_reason,
            preparation.destination.path.to_str().unwrap(),
        ],
    );
    let restarted = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    restarted.workspace_discovery_settled().await;
    wait_for_path(&preparation.destination.path, false).await;
    wait_for_path(&intent, false).await;
    restarted.shutdown().await.unwrap();
}
