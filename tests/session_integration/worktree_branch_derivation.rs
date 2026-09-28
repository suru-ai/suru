//! A fresh Managed Worktree's branch renamed to the name its Session's Title
//! Errand derives, through the owning Server's protocol and real Git.
use crate::{
    provider_support::{ControlledProvider, ControlledProviderRuntime, ControlledProviderSession},
    repositories::git,
    server_support::{
        PROGRESS_DEADLINE, config_root_pinning, next_catalog_change_matching, next_derived_title,
        open_catalog_stream,
    },
    support::{hosted_model, hosted_selection},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::*,
    provider::ProviderEvent,
    server::{self, RunningServer, ServerConfig, ServerTimings},
    source_control::{BranchRename, CreatedBranch, GitSourceControl, SourceControl},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Duration, timeout},
};

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";
const ERRAND_MODEL: &str = "controlled-errand";

fn committed(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
    git(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}

fn read_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

/// Every local branch the Repository has, by short name.
fn branches(root: &Path) -> Vec<String> {
    let mut branches = read_git(
        root,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    branches.sort();
    branches
}

fn suru_base(root: &Path, branch: &str) -> Option<String> {
    read_git(
        root,
        &["config", "--local", &format!("branch.{branch}.suru-base")],
    )
}

fn branch_of(preparation: &PreparedCheckout) -> String {
    preparation
        .plan
        .branch()
        .expect("Git plans create a branch")
        .to_owned()
}

/// One Server over one real Repository, driven by a controlled Provider.
struct Fixture {
    _temporary: tempfile::TempDir,
    _config: Option<tempfile::TempDir>,
    root: PathBuf,
    state: PathBuf,
    channel: &'static str,
    main: PathBuf,
    server: RunningServer,
    client: ManagedClient,
    provider: ControlledProvider,
    renames: mpsc::UnboundedReceiver<Result<BranchRename, String>>,
    observations: Arc<HeldObservations>,
}

/// Git in every respect, reporting how each rename settled once the Server is
/// done with it — its recording included. A rename that is declined or
/// changes nothing is otherwise invisible by design, and this is what lets a
/// test say one was attempted, and how it went, without waiting out a
/// deadline.
struct WitnessedGit {
    git: GitSourceControl,
    renames: mpsc::UnboundedSender<Result<BranchRename, String>>,
    observations: Arc<HeldObservations>,
}

/// Holds the next reading of a linked Worktree once Git has answered it, so a
/// test decides when that reading reaches the Server rather than racing it.
#[derive(Default)]
struct HeldObservations {
    next: std::sync::Mutex<Option<HeldObservation>>,
}

struct HeldObservation {
    read: oneshot::Sender<CheckoutSummary>,
    release: oneshot::Receiver<()>,
}

impl HeldObservations {
    /// Holds the next reading of a linked Worktree, handing back where that
    /// reading arrives once Git has answered and what lets it go on.
    fn hold_next(&self) -> (oneshot::Receiver<CheckoutSummary>, oneshot::Sender<()>) {
        let (read, taken) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let held = HeldObservation {
            read,
            release: released,
        };
        assert!(
            self.next.lock().unwrap().replace(held).is_none(),
            "one reading is held at a time"
        );
        (taken, release)
    }
}

#[async_trait::async_trait]
impl SourceControl for WitnessedGit {
    async fn rename_branch(
        &self,
        created: &CreatedBranch,
        proposal: &str,
    ) -> Result<BranchRename, String> {
        self.git.rename_branch(created, proposal).await
    }
    async fn branch_rename_settled(
        &self,
        _created: &CreatedBranch,
        outcome: &Result<BranchRename, String>,
    ) {
        let _ = self.renames.send(outcome.clone());
    }
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        self.git.discover(directory).await
    }
    async fn checkpoint(
        &self,
        at: suru::source_control::PreparationCheckpoint,
        preparation: &PreparedCheckout,
    ) -> Result<(), String> {
        self.git.checkpoint(at, preparation).await
    }
    async fn plan_checkout(
        &self,
        id: PreparationId,
        source: &ResolvedWorkspace,
        name: &str,
        reserved: &[PathBuf],
    ) -> Result<PreparedCheckout, String> {
        self.git.plan_checkout(id, source, name, reserved).await
    }
    async fn prepare_checkout(&self, plan: &PreparedCheckout) -> Result<ResolvedWorkspace, String> {
        self.git.prepare_checkout(plan).await
    }
    async fn recover_checkout(
        &self,
        repository: &Repository,
        checkout: &CheckoutAssociation,
    ) -> Result<CheckoutRecovery, String> {
        self.git.recover_checkout(repository, checkout).await
    }
    async fn observe(&self, checkout: &CheckoutAssociation) -> CheckoutSummary {
        let reading = self.git.observe(checkout).await;
        let held = match checkout.kind {
            CheckoutKind::Linked => self.observations.next.lock().unwrap().take(),
            CheckoutKind::Main => None,
        };
        if let Some(held) = held {
            let _ = held.read.send(reading.clone());
            let _ = held.release.await;
        }
        reading
    }
    async fn list_checkouts(
        &self,
        repository: &Repository,
    ) -> Result<Vec<CheckoutAssociation>, String> {
        self.git.list_checkouts(repository).await
    }
    fn reuse_discovery(
        &self,
        directory: &Path,
        previous: &ResolvedWorkspace,
    ) -> Option<ResolvedWorkspace> {
        self.git.reuse_discovery(directory, previous)
    }
}

fn timings() -> ServerTimings {
    ServerTimings::default().with_checkout_observation_interval(Duration::from_millis(10))
}

fn controlled_provider() -> (Arc<ControlledProviderRuntime>, ControlledProvider) {
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![
            hosted_model(PROVIDER, MODEL),
            ModelDescriptor {
                is_default: false,
                ..hosted_model(PROVIDER, ERRAND_MODEL)
            },
        ],
    )
}

impl Fixture {
    async fn start(channel: &'static str) -> Self {
        Self::start_with(channel, None, timings()).await
    }

    async fn start_with(
        channel: &'static str,
        errand: Option<DerivationErrand>,
        timings: ServerTimings,
    ) -> Self {
        Self::start_with_git(channel, errand, timings, |_| GitSourceControl::default()).await
    }

    /// A Fixture whose Git is built knowing the Server's data directory, for
    /// an observer that reaches into what the Server stores.
    async fn start_with_git(
        channel: &'static str,
        errand: Option<DerivationErrand>,
        timings: ServerTimings,
        git: impl FnOnce(&Path) -> GitSourceControl,
    ) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temporary.path()).unwrap();
        let main = root.join("repo");
        committed(&main);
        let config = errand.as_ref().map(config_root_pinning);
        let state = root.join("state");
        let data = ServerConfig::new(&state, channel)
            .expect("configure server")
            .data_dir()
            .to_owned();
        let (server, client, provider, renames, observations) =
            spawn(&state, channel, config.as_ref(), timings, git(&data)).await;
        Self {
            _temporary: temporary,
            _config: config,
            root,
            state,
            channel,
            main,
            server,
            client,
            provider,
            renames,
            observations,
        }
    }

    /// Another Repository on this Server, and so another Workspace, whose
    /// first Session's derivation ends in a Workspace Icon Errand of its own.
    fn repository(&self, name: &str) -> PathBuf {
        let repository = self.root.join(name);
        committed(&repository);
        repository
    }

    async fn prepare(&self, text: &str) -> PreparedCheckout {
        self.prepare_in(&self.main, text).await
    }

    async fn prepare_in(&self, source: &Path, text: &str) -> PreparedCheckout {
        let result = self
            .client
            .prepare_checkout(PrepareCheckoutRequest {
                id: Default::default(),
                source: ExecutionDirectory {
                    path: source.to_owned(),
                },
                prompt: PreparationPrompt {
                    text: text.to_owned(),
                    skill_invocations: vec![],
                },
                provider: ProviderId::new(PROVIDER),
            })
            .await
            .expect("prepare a Managed Worktree");
        assert_eq!(result.error, None);
        result.preparation
    }

    async fn create(&self, preparation: Option<&PreparedCheckout>, path: &Path) -> SessionSnapshot {
        self.client
            .create_session(CreateSessionRequest {
                preparation_id: preparation.map(|preparation| preparation.id),
                agent_selection: Some(hosted_selection(PROVIDER, MODEL)),
                execution_directory: ExecutionDirectory {
                    path: path.to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Please fix the reasoning flicker in $review".to_owned(),
                    skill_invocations: vec![],
                    attachments: Vec::new(),
                },
            })
            .await
            .expect("create the Session")
    }

    /// A Managed Worktree prepared from `source` and admitted, its Title
    /// Errand in hand.
    async fn admitted(
        &mut self,
        source: &Path,
        text: &str,
    ) -> (PreparedCheckout, SessionSnapshot, ErrandReply) {
        let preparation = self.prepare_in(source, text).await;
        let created = self
            .create(Some(&preparation), &preparation.destination.path)
            .await;
        let errand = self.next_errand().await;
        (preparation, created, errand)
    }

    async fn next_errand(&mut self) -> ErrandReply {
        let errand = timeout(PROGRESS_DEADLINE, self.provider.next_errand())
            .await
            .expect("an Errand reaches the Provider");
        ErrandReply {
            prompt: errand.prompt().to_owned(),
            schema: errand.schema().clone(),
            selection: errand.selection().clone(),
            errand: Some(errand),
        }
    }

    /// Runs the Session's first Turn to completion: every point at which its
    /// execution lease on the Repository could still be held. The Provider
    /// Session stays open for as long as the caller keeps it.
    async fn work_the_first_turn(&mut self) -> ControlledProviderSession {
        let mut session = timeout(PROGRESS_DEADLINE, self.provider.next_start())
            .await
            .expect("the first Turn starts a Provider Session")
            .succeed(AgentIdentity {
                agent: AgentId::new("controlled-agent"),
                selection: hosted_selection(PROVIDER, MODEL),
            });
        timeout(PROGRESS_DEADLINE, session.next_turn())
            .await
            .expect("the first Turn reaches the Provider")
            .succeed();
        session
            .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
            .await;
        session
    }

    /// Answers the Workspace Icon Errand, which only a Workspace's first
    /// Session asks for, once its Title Errand is settled. By then the
    /// derivation has decided whether to rename anything: a rename it will not
    /// attempt never reaches source control after this.
    async fn workspace_errand(&mut self) {
        let errand = self.next_errand().await;
        assert!(errand.prompt.contains("Workspace"), "{}", errand.prompt);
        errand.succeed(json!({ "icon": "dev-rust" }));
    }

    /// How the one rename this derivation attempted settled.
    async fn rename_attempted(&mut self) -> Result<BranchRename, String> {
        timeout(PROGRESS_DEADLINE, self.renames.recv())
            .await
            .expect("the rename reaches source control")
            .expect("the Server is still running")
    }

    /// Removes a Worktree outside Suru, then prompts a Session working in it
    /// and waits for that Prompt to reach its Provider in the Worktree Suru
    /// recovered in its place.
    async fn remove_and_prompt(
        &mut self,
        session_id: SessionId,
        destination: &Path,
    ) -> ControlledProviderSession {
        git(
            &self.main,
            &[
                "worktree",
                "remove",
                "--force",
                destination.to_str().unwrap(),
            ],
        );
        assert!(!destination.exists());
        self.client
            .admit_prompt(
                session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: "Carry on".to_owned(),
                        skill_invocations: vec![],
                        attachments: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit a Prompt to the Session");
        let start = timeout(PROGRESS_DEADLINE, self.provider.next_start())
            .await
            .expect("the recreated Worktree reconnects its Provider");
        assert_eq!(start.execution_directory(), destination);
        let mut session = start.succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: hosted_selection(PROVIDER, MODEL),
        });
        timeout(PROGRESS_DEADLINE, session.next_turn())
            .await
            .expect("the Prompt reaches the Provider")
            .succeed();
        session
    }

    fn no_rename_attempted(&mut self) {
        assert!(
            self.renames.try_recv().is_err(),
            "no rename reached source control"
        );
    }
}

async fn spawn(
    state: &Path,
    channel: &str,
    config: Option<&tempfile::TempDir>,
    timings: ServerTimings,
    git: GitSourceControl,
) -> (
    RunningServer,
    ManagedClient,
    ControlledProvider,
    mpsc::UnboundedReceiver<Result<BranchRename, String>>,
    Arc<HeldObservations>,
) {
    let (runtime, provider) = controlled_provider();
    let mut server_config = ServerConfig::new(state, channel).expect("configure server");
    if let Some(config) = config {
        server_config = server_config.with_config_dir(config.path());
    }
    let (renames, witnessed) = mpsc::unbounded_channel();
    let observations = Arc::new(HeldObservations::default());
    let server = server::spawn_with_source_control(
        server_config,
        vec![runtime],
        timings,
        Arc::new(WitnessedGit {
            git,
            renames,
            observations: observations.clone(),
        }),
    )
    .await
    .expect("spawn server");
    let mut client =
        ManagedClient::connect(ManagedClientConfig::new(state, channel).expect("configure client"))
            .await
            .expect("connect client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    (server, client, provider, witnessed, observations)
}

/// An Errand the test has taken from the Provider, with what it asked kept
/// readable after it is answered.
struct ErrandReply {
    prompt: String,
    schema: Value,
    selection: AgentSelection,
    errand: Option<crate::provider_support::ErrandRequest>,
}

impl ErrandReply {
    fn asks_for_a_branch(&self) -> bool {
        self.schema["properties"].get("branch").is_some()
    }
    fn succeed(mut self, reply: Value) {
        self.errand.take().unwrap().succeed(reply);
    }
    fn fail(mut self, message: &str) {
        self.errand.take().unwrap().fail(message);
    }
}

async fn branch_reading(
    catalog: &mut (impl futures_util::Stream<Item = SessionCatalogUpdate> + Unpin),
    checkout: &CheckoutId,
    branch: &str,
) {
    next_catalog_change_matching(catalog, |change| {
        matches!(
            change,
            SessionCatalogChange::CheckoutStateChanged {
                checkout_id,
                checkout_state: Some(CheckoutSummary {
                    revision: Some(CheckoutRevision::Branch { name, .. }),
                    ..
                }),
            } if checkout_id == checkout && name == branch
        )
    })
    .await;
}

#[tokio::test]
async fn a_derived_branch_renames_the_fresh_worktree_its_session_was_admitted_from() {
    let mut fixture = Fixture::start("derived-branch-rename").await;
    let preparation = fixture.prepare("Please fix the reasoning flicker").await;
    let created_branch = branch_of(&preparation);
    assert_eq!(created_branch, "suru/fix-reasoning-flicker");
    let destination = preparation.destination.path.clone();
    let base = suru_base(&fixture.main, &created_branch).expect("Reclaim's base is recorded");
    let descriptor = fixture.server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;

    let created = fixture.create(Some(&preparation), &destination).await;
    let session_id = created.session.id;
    let checkout = created
        .session
        .checkout
        .clone()
        .expect("the Session works in its Managed Worktree");
    let errand = fixture.next_errand().await;
    assert!(
        errand.asks_for_a_branch(),
        "a fresh Worktree's Errand asks for its branch: {}",
        errand.schema
    );
    assert_eq!(
        errand.schema["required"],
        json!(["title", "icon", "branch"]),
        "strict-mode schemas require every property they name"
    );
    assert!(
        errand.prompt.contains("branch name") && errand.prompt.contains("review"),
        "the Model reads the Prompt as the Title Errand presents it: {}",
        errand.prompt
    );
    errand.succeed(json!({
        "title": "Fix reasoning group flicker",
        "icon": "md-bug",
        "branch": "suru/Reasoning group flicker",
    }));
    assert_eq!(
        next_derived_title(&mut fixture.client).await,
        SessionTitleChanged {
            session_id,
            title: "Fix reasoning group flicker".to_owned(),
            icon: Some("md-bug".to_owned()),
        }
    );

    // The rename now waits on the first Turn, which holds the Repository
    // until its Prompt is delivered; the Workspace Errand waits on neither.
    fixture.workspace_errand().await;
    fixture.no_rename_attempted();
    assert_eq!(
        read_git(&destination, &["symbolic-ref", "--short", "HEAD"]).as_deref(),
        Some(created_branch.as_str())
    );

    // The Title has landed and the rename is due, yet the first Turn starts
    // regardless: neither the Errand nor the rename stands in front of it.
    fixture.work_the_first_turn().await;
    let renamed = "suru/reasoning-group-flicker";
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: renamed.to_owned()
        })
    );
    branch_reading(&mut catalog, &checkout.id, renamed).await;

    assert!(
        branches(&fixture.main).contains(&renamed.to_owned())
            && !branches(&fixture.main).contains(&created_branch),
        "{:?}",
        branches(&fixture.main)
    );
    assert_eq!(
        read_git(&destination, &["symbolic-ref", "--short", "HEAD"]).as_deref(),
        Some(renamed)
    );
    assert_eq!(
        suru_base(&fixture.main, renamed),
        Some(base),
        "Reclaim's recorded base moves with the branch"
    );
    assert_eq!(suru_base(&fixture.main, &created_branch), None);
    assert!(
        read_git(
            &fixture.main,
            &["reflog", "exists", &format!("refs/heads/{renamed}")]
        )
        .is_some(),
        "the branch keeps its reflog"
    );
    assert!(
        destination.ends_with(".suru-worktrees/fix-reasoning-flicker") && destination.is_dir(),
        "the Worktree's location never moves: {}",
        destination.display()
    );
    let listed = read_git(&fixture.main, &["worktree", "list", "--porcelain"]).unwrap();
    assert!(
        crate::support::git_output_mentions_path(&listed, &destination),
        "{listed}"
    );
    let summary = fixture
        .client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| match item {
            SessionListItem::Readable(summary) if summary.session.id == session_id => Some(summary),
            _ => None,
        })
        .expect("the Session is listed");
    assert!(
        matches!(
            summary.checkout_state.as_ref().and_then(|state| state.revision.as_ref()),
            Some(CheckoutRevision::Branch { name, .. }) if name == renamed
        ),
        "the Session's Checkout State reports the new branch: {:?}",
        summary.checkout_state
    );
    assert_eq!(summary.session.execution_directory.path, destination);

    drop(catalog);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_session_in_an_existing_worktree_is_asked_for_no_branch_and_renames_nothing() {
    let mut fixture = Fixture::start("derived-branch-existing").await;
    let existing = fixture.root.join("existing");
    git(
        &fixture.main,
        &[
            "worktree",
            "add",
            "-b",
            "suru/existing",
            existing.to_str().unwrap(),
        ],
    );
    let before = branches(&fixture.main);

    fixture.create(None, &existing).await;
    let errand = fixture.next_errand().await;
    assert!(!errand.asks_for_a_branch(), "{}", errand.schema);
    assert_eq!(errand.schema["required"], json!(["title", "icon"]));
    assert!(!errand.prompt.contains("branch"), "{}", errand.prompt);
    // A Model volunteering one anyway is not heard.
    errand.succeed(json!({
        "title": "Fix reasoning group flicker",
        "icon": "md-bug",
        "branch": "something else entirely",
    }));
    fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;
    fixture.no_rename_attempted();

    assert_eq!(branches(&fixture.main), before);
    assert_eq!(
        read_git(&existing, &["symbolic-ref", "--short", "HEAD"]).as_deref(),
        Some("suru/existing")
    );

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_worktree_no_longer_on_its_created_branch_is_not_renamed() {
    let mut fixture = Fixture::start("derived-branch-head-moved").await;
    for (repository, move_head) in [
        ("switched", &["checkout", "-b", "elsewhere"][..]),
        ("detached", &["checkout", "--detach"][..]),
    ] {
        let source = fixture.repository(repository);
        let (preparation, _, errand) = fixture.admitted(&source, "Ship the picker").await;
        let created_branch = branch_of(&preparation);
        git(&preparation.destination.path, move_head);
        errand.succeed(json!({
            "title": "Something better",
            "icon": "md-bug",
            "branch": "renamed anyway",
        }));
        fixture.work_the_first_turn().await;
        fixture.workspace_errand().await;
        assert!(
            fixture.rename_attempted().await.is_err(),
            "{move_head:?}: a Worktree off its created branch is declined"
        );

        let branches = branches(&source);
        assert!(
            branches.contains(&created_branch)
                && !branches
                    .iter()
                    .any(|branch| branch.starts_with("suru/renamed")),
            "{move_head:?}: {branches:?}"
        );
    }

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_taken_derived_name_is_numbered_and_the_current_name_is_left_alone() {
    let mut fixture = Fixture::start("derived-branch-numbering").await;
    // Git would refuse the bare name and `-2` beside these on some platform,
    // and `-3` beside the last on every one.
    for taken in [
        "suru/reasoning-flicker",
        "suru/Reasoning-Flicker-2",
        "suru/reasoning-flicker-3/old",
    ] {
        git(&fixture.main, &["branch", taken]);
    }
    let main = fixture.main.clone();
    let (preparation, _, errand) = fixture.admitted(&main, "Fix the parser").await;
    errand.succeed(json!({
        "title": "Fix reasoning flicker",
        "icon": "md-bug",
        "branch": "reasoning flicker",
    }));
    fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: "suru/reasoning-flicker-4".to_owned()
        })
    );
    assert_eq!(
        read_git(
            &preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .as_deref(),
        Some("suru/reasoning-flicker-4")
    );
    assert!(!branches(&main).contains(&branch_of(&preparation)));

    // Proposing the name a Worktree already has, whether it was the first
    // name tried or the first free one, changes nothing.
    for (repository, taken, created) in [
        ("first", None, "suru/ship-picker"),
        ("numbered", Some("suru/ship-picker"), "suru/ship-picker-2"),
    ] {
        let source = fixture.repository(repository);
        if let Some(taken) = taken {
            git(&source, &["branch", taken]);
        }
        let (preparation, _, errand) = fixture.admitted(&source, "Ship the picker").await;
        assert_eq!(branch_of(&preparation), created);
        let reflog = || {
            read_git(
                &source,
                &[
                    "reflog",
                    "show",
                    "--format=%gs",
                    &format!("refs/heads/{created}"),
                ],
            )
        };
        let before = reflog();
        errand.succeed(
            json!({ "title": "Ship the picker", "icon": "md-bug", "branch": "ship picker" }),
        );
        fixture.work_the_first_turn().await;
        fixture.workspace_errand().await;
        assert_eq!(
            fixture.rename_attempted().await,
            Ok(BranchRename::Unchanged)
        );
        assert_eq!(
            read_git(
                &preparation.destination.path,
                &["symbolic-ref", "--short", "HEAD"]
            )
            .as_deref(),
            Some(created)
        );
        assert_eq!(reflog(), before, "{repository}: the branch was not touched");
    }

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn derivation_turned_off_leaves_the_first_name_standing() {
    let mut fixture =
        Fixture::start_with("derived-branch-off", Some(DerivationErrand::Off), timings()).await;
    let preparation = fixture.prepare("Fix the parser").await;
    fixture
        .create(Some(&preparation), &preparation.destination.path)
        .await;
    fixture.work_the_first_turn().await;

    assert!(
        fixture.provider.try_next_errand().is_none(),
        "derivation turned off asks for no Errand"
    );
    assert_eq!(
        read_git(
            &preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .as_deref(),
        Some("suru/fix-parser")
    );

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_pinned_selection_derives_the_branch() {
    let pinned = AgentSelection {
        provider: ProviderId::new(PROVIDER),
        model: ModelId::new(ERRAND_MODEL),
        options: Vec::new(),
    };
    let mut fixture = Fixture::start_with(
        "derived-branch-pinned",
        Some(DerivationErrand::Pinned(pinned.clone())),
        timings(),
    )
    .await;
    let main = fixture.main.clone();
    let (preparation, _, errand) = fixture.admitted(&main, "Fix the parser").await;
    assert_eq!(errand.selection, pinned);
    assert!(errand.asks_for_a_branch());
    errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: "suru/repair-config-parser".to_owned()
        })
    );

    assert_eq!(
        read_git(
            &preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .as_deref(),
        Some("suru/repair-config-parser")
    );

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_without_a_usable_branch_leaves_the_first_name_standing() {
    let mut fixture = Fixture::start("derived-branch-unusable").await;
    let replies: [(&str, Option<Value>); 5] = [
        ("failed", None),
        // Out of schema: no Title, and so nothing at all.
        ("unschematic", Some(json!({ "branch": "renamed anyway" }))),
        // A good Title beside a branch Suru cannot use.
        (
            "empty",
            Some(json!({ "title": "Explain the seam", "icon": "md-bug", "branch": "" })),
        ),
        (
            "filler",
            Some(
                json!({ "title": "Explain the seam", "icon": "md-bug", "branch": "please do it" }),
            ),
        ),
        (
            "mistyped",
            Some(json!({ "title": "Explain the seam", "icon": "md-bug", "branch": 42 })),
        ),
    ];
    for (repository, reply) in replies {
        let source = fixture.repository(repository);
        let (_, created, errand) = fixture.admitted(&source, "Fix the parser").await;
        let titled = reply
            .as_ref()
            .is_some_and(|reply| reply.get("title").is_some());
        match reply {
            Some(reply) => errand.succeed(reply),
            None => errand.fail("the Provider is signed out"),
        }
        if titled {
            assert_eq!(
                next_derived_title(&mut fixture.client).await,
                SessionTitleChanged {
                    session_id: created.session.id,
                    title: "Explain the seam".to_owned(),
                    icon: Some("md-bug".to_owned()),
                },
                "{repository}: an unusable branch costs the reply nothing else"
            );
        }
        fixture.work_the_first_turn().await;
        fixture.workspace_errand().await;
        fixture.no_rename_attempted();
        assert_eq!(
            branches(&source),
            ["main", "suru/fix-parser"],
            "{repository}"
        );
    }

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_errand_that_never_answers_leaves_the_first_name_standing() {
    let mut fixture = Fixture::start_with(
        "derived-branch-timeout",
        None,
        timings().with_errand_timeout(Duration::from_millis(20)),
    )
    .await;
    let main = fixture.main.clone();
    // Held rather than answered: a wedged Provider takes the request and says
    // nothing.
    let (preparation, _, _wedged) = fixture.admitted(&main, "Fix the parser").await;
    fixture.work_the_first_turn().await;
    let _workspace_errand = fixture.next_errand().await;
    fixture.no_rename_attempted();

    assert_eq!(branches(&main), ["main", "suru/fix-parser"]);
    assert_eq!(
        read_git(
            &preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .as_deref(),
        Some("suru/fix-parser")
    );

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_restart_after_admission_attempts_no_rename() {
    let mut fixture = Fixture::start("derived-branch-restart").await;
    let main = fixture.main.clone();
    let (preparation, _, outstanding) = fixture.admitted(&main, "Fix the parser").await;
    assert!(outstanding.asks_for_a_branch());
    fixture.work_the_first_turn().await;
    // The Server goes away with the Errand still outstanding.
    let Fixture {
        _temporary,
        _config,
        state,
        channel,
        server,
        client,
        provider,
        ..
    } = fixture;
    drop(client);
    server.shutdown().await.expect("shut down server");
    drop((outstanding, provider));

    let (server, client, mut provider, mut renames, _) = spawn(
        &state,
        channel,
        None,
        timings(),
        GitSourceControl::default(),
    )
    .await;
    // A new Session's Errand is the first to reach the Provider after the
    // restart: had the admitted Session's derivation been attempted again,
    // its own Errand, asking for a branch, would have come first.
    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(hosted_selection(PROVIDER, MODEL)),
            execution_directory: ExecutionDirectory { path: main.clone() },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the seam".to_owned(),
                skill_invocations: vec![],
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create a Session after the restart");
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the new Session's Errand reaches the Provider");
    assert!(errand.prompt().contains("Explain the seam"));
    assert!(errand.schema()["properties"].get("branch").is_none());
    errand.fail("done");
    let workspace_errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace Icon Errand follows");
    assert!(workspace_errand.prompt().contains("Workspace"));
    assert!(
        renames.try_recv().is_err(),
        "no rename reached source control"
    );
    assert_eq!(
        read_git(
            &preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        )
        .as_deref(),
        Some("suru/fix-parser")
    );

    server.shutdown().await.expect("shut down server");
}

/// Checkout observation that never ticks again after startup, so whatever a
/// test sees of a rename was recorded when the rename happened.
fn without_observation() -> ServerTimings {
    ServerTimings::default().with_checkout_observation_interval(Duration::from_secs(60 * 60))
}

fn head(root: &Path) -> Option<String> {
    read_git(root, &["symbolic-ref", "--short", "HEAD"])
}

fn recovery_branch(snapshot: &SessionSnapshot) -> Option<&str> {
    match snapshot
        .session
        .checkout
        .as_ref()
        .and_then(|checkout| checkout.recovery_revision.as_ref())
    {
        Some(CheckoutRevision::Branch { name, .. }) => Some(name),
        _ => None,
    }
}

/// Waits for a Session's recovery facts to name `branch`, which with
/// observation stood down only the rename's own recording can bring about.
async fn recovers_on(fixture: &Fixture, session_id: SessionId, branch: &str) -> SessionSnapshot {
    crate::support::read_session_until(
        &reqwest::Client::new(),
        fixture.server.descriptor(),
        session_id,
        &format!("recovery facts name {branch}"),
        |snapshot| recovery_branch(snapshot) == Some(branch),
    )
    .await
}

#[tokio::test]
async fn a_branch_with_an_upstream_keeps_its_name() {
    let mut fixture = Fixture::start("derived-branch-upstream").await;
    let remote = fixture.root.join("remote.git");
    git(
        &fixture.root,
        &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
    );
    git(
        &fixture.main,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let main = fixture.main.clone();
    let (preparation, _, errand) = fixture.admitted(&main, "Fix the parser").await;
    let created = branch_of(&preparation);
    // The Agent pushes its branch before the Errand answers.
    git(
        &preparation.destination.path,
        &["push", "--set-upstream", "origin", &created],
    );
    errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;

    assert!(
        fixture
            .rename_attempted()
            .await
            .is_err_and(|reason| reason.contains("upstream")),
        "a branch with an upstream is declined"
    );
    assert_eq!(
        head(&preparation.destination.path).as_deref(),
        Some(created.as_str())
    );
    assert_eq!(branches(&main), ["main", created.as_str()]);
    assert_eq!(
        read_git(&main, &["config", &format!("branch.{created}.remote")]).as_deref(),
        Some("origin")
    );

    fixture.server.shutdown().await.expect("shut down server");
}

/// Admits the preparation exactly as the Server does, then puts its intent
/// back where the Server just deleted it from — as a deletion that failed
/// after admission leaves it.
struct RetainIntentAfterAdmission {
    data: PathBuf,
}

#[async_trait::async_trait]
impl suru::source_control::PreparationObserver for RetainIntentAfterAdmission {
    async fn checkpoint(
        &self,
        at: suru::source_control::PreparationCheckpoint,
        preparation: &PreparedCheckout,
    ) -> Result<(), String> {
        if at == suru::source_control::PreparationCheckpoint::Admitted {
            let intents = self.data.join("checkout-preparations");
            std::fs::create_dir_all(&intents).unwrap();
            std::fs::write(
                intents.join(format!("{}.json", preparation.id.0)),
                serde_json::to_vec(preparation).unwrap(),
            )
            .unwrap();
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_retained_preparation_intent_naming_the_branch_prevents_the_rename() {
    let mut fixture =
        Fixture::start_with_git("derived-branch-retained-intent", None, timings(), |data| {
            GitSourceControl::default().with_preparation_observer(Arc::new(
                RetainIntentAfterAdmission {
                    data: data.to_owned(),
                },
            ))
        })
        .await;
    let main = fixture.main.clone();
    let (preparation, _, errand) = fixture.admitted(&main, "Fix the parser").await;
    errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;

    assert!(
        fixture
            .rename_attempted()
            .await
            .is_err_and(|reason| reason.contains("intent")),
        "a retained intent naming the branch keeps it"
    );
    assert_eq!(
        head(&preparation.destination.path).as_deref(),
        Some("suru/fix-parser")
    );
    assert!(branches(&main).contains(&"suru/fix-parser".to_owned()));

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_worktree_removed_right_after_its_rename_recovers_on_the_new_branch() {
    let mut fixture =
        Fixture::start_with("derived-branch-recovery", None, without_observation()).await;
    let main = fixture.main.clone();
    let (preparation, created, errand) = fixture.admitted(&main, "Fix the parser").await;
    let destination = preparation.destination.path.clone();
    errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    let _first = fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;
    let renamed = "suru/repair-config-parser";
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: renamed.to_owned()
        })
    );
    recovers_on(&fixture, created.session.id, renamed).await;

    // Gone outside Suru, with no observation to have noticed either change.
    let _recovered = fixture
        .remove_and_prompt(created.session.id, &destination)
        .await;
    assert_eq!(head(&destination).as_deref(), Some(renamed));
    assert!(!branches(&main).contains(&"suru/fix-parser".to_owned()));
    assert!(
        suru_base(&main, renamed).is_some(),
        "Reclaim still finds the branch's recorded base"
    );

    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_observation_read_before_the_rename_cannot_undo_its_recording() {
    let mut fixture = Fixture::start("derived-branch-stale-observation").await;
    let descriptor = fixture.server.descriptor().clone();
    let catalog = open_catalog_stream(&descriptor).await;
    let (stale, release_stale) = fixture.observations.hold_next();
    let main = fixture.main.clone();
    let (preparation, created, errand) = fixture.admitted(&main, "Fix the parser").await;
    let destination = preparation.destination.path.clone();
    let stale = timeout(PROGRESS_DEADLINE, stale)
        .await
        .expect("observation reads the fresh Worktree")
        .expect("the reading is handed over");
    assert!(
        matches!(
            &stale.revision,
            Some(CheckoutRevision::Branch { name, .. }) if name == "suru/fix-parser"
        ),
        "{:?}",
        stale.revision
    );

    // The rename is read and recorded while that reading is still on its way.
    errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    let _first = fixture.work_the_first_turn().await;
    fixture.workspace_errand().await;
    let renamed = "suru/repair-config-parser";
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: renamed.to_owned()
        })
    );
    recovers_on(&fixture, created.session.id, renamed).await;

    // Observation goes on to its next reading of the Worktree only once the
    // held one has arrived, and that next one is held for good, so nothing
    // after the stale reading can put right what it did.
    let (next, _held_for_good) = fixture.observations.hold_next();
    release_stale.send(()).unwrap();
    timeout(PROGRESS_DEADLINE, next)
        .await
        .expect("observation goes on to its next reading")
        .expect("the reading is handed over");
    let summary = fixture
        .client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| match item {
            SessionListItem::Readable(summary) if summary.session.id == created.session.id => {
                Some(summary)
            }
            _ => None,
        })
        .expect("the Session is listed");
    let branch_of = |revision: Option<&CheckoutRevision>| match revision {
        Some(CheckoutRevision::Branch { name, .. }) => Some(name.clone()),
        _ => None,
    };
    assert_eq!(
        branch_of(
            summary
                .session
                .checkout
                .as_ref()
                .and_then(|checkout| checkout.recovery_revision.as_ref())
        )
        .as_deref(),
        Some(renamed),
        "the recovery facts still name the new branch"
    );
    assert_eq!(
        branch_of(
            summary
                .checkout_state
                .as_ref()
                .and_then(|state| state.revision.as_ref())
        )
        .as_deref(),
        Some(renamed),
        "the Checkout State never steps back to the old branch"
    );

    let _recovered = fixture
        .remove_and_prompt(created.session.id, &destination)
        .await;
    assert_eq!(head(&destination).as_deref(), Some(renamed));

    drop(catalog);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn every_session_sharing_the_worktree_reports_the_new_branch() {
    let mut fixture =
        Fixture::start_with("derived-branch-shared", None, without_observation()).await;
    let descriptor = fixture.server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;
    let main = fixture.main.clone();
    let (preparation, first, first_errand) = fixture.admitted(&main, "Fix the parser").await;
    let destination = preparation.destination.path.clone();
    let _first_turn = fixture.work_the_first_turn().await;

    // A second Session in the same Managed Worktree, which was not prepared
    // for it and so is asked for no branch.
    let second = fixture.create(None, &destination).await;
    let second_errand = fixture.next_errand().await;
    assert!(!second_errand.asks_for_a_branch());
    second_errand.fail("the Provider is signed out");
    fixture.workspace_errand().await;
    let _second_turn = fixture.work_the_first_turn().await;
    let checkout = first.session.checkout.clone().expect("a Managed Worktree");
    assert_eq!(
        second.session.checkout.as_ref().map(|c| &c.id),
        Some(&checkout.id)
    );

    first_errand.succeed(json!({
        "title": "Repair the config parser",
        "icon": "md-bug",
        "branch": "repair config parser",
    }));
    fixture.workspace_errand().await;
    let renamed = "suru/repair-config-parser";
    assert_eq!(
        fixture.rename_attempted().await,
        Ok(BranchRename::Renamed {
            branch: renamed.to_owned()
        })
    );
    branch_reading(&mut catalog, &checkout.id, renamed).await;
    for session in [&first, &second] {
        recovers_on(&fixture, session.session.id, renamed).await;
    }
    let listed = fixture
        .client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .filter_map(|item| match item {
            SessionListItem::Readable(summary) => Some(summary),
            SessionListItem::Unreadable(_) => None,
        })
        .collect::<Vec<_>>();
    for session in [&first, &second] {
        let summary = listed
            .iter()
            .find(|summary| summary.session.id == session.session.id)
            .expect("the Session is listed");
        assert!(
            matches!(
                summary.checkout_state.as_ref().and_then(|state| state.revision.as_ref()),
                Some(CheckoutRevision::Branch { name, .. }) if name == renamed
            ),
            "{:?}",
            summary.checkout_state
        );
    }

    drop(catalog);
    fixture.server.shutdown().await.expect("shut down server");
}
