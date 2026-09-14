use super::*;

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
async fn an_unfinished_preparation_is_not_an_immediate_orphan() {
    let layout = ReclaimLayout::new("unrelated-orphan");
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        layout.server_config("reclaim-unfinished-preparation"),
        vec![runtime],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(FailOnceAt::new(PreparationCheckpoint::CheckoutCreated)),
        ),
    )
    .await
    .unwrap();
    remember_repository(&server, &layout.main).await;
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
            description: "unfinished preparation".into(),
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
    let unfinished = prepared.preparation.destination.path;
    assert!(unfinished.is_dir());

    tokio::time::sleep(STARTUP_DELAY + RECLAIM_INTERVAL * 3).await;
    assert!(
        unfinished.is_dir(),
        "the failed-preparation age rule belongs to #352"
    );
    server.shutdown().await.unwrap();
}
