//! `clone_workspace`: a Sidekick Cloning a Repository URL directly within one
//! of its own Server's Workspace Roots — the one it names, or the only one —
//! and nowhere else, answered once the Clone is done with the new Workspace
//! as `list_workspaces` rows it (ADR 0051). The Workspace is entered in the
//! Workspace Registry as Cloned, its Icon and Description are derived at
//! once as a first Session's would be, and the Log says what was Cloned
//! where. A Clone is refused, saying why, where no Root is there to take it,
//! where its Root cannot be read, where its name is no plain directory name,
//! where something stands at its destination already, where Git fails, and
//! where it outlasts the Server's bound on one; a Clone Git began and did
//! not finish leaves nothing behind. Only a Sidekick is offered the Tool.
//!
//! Each test acts as the MCP client a Sidekick's harness is, Clones a bare
//! Repository made beside the Roots — named by its path, rooted as the
//! platform roots paths, or by a file URL — and asserts on what the Tool
//! answers it and on what is left on disk.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use suru::{
    managed_client::ManagedEvent,
    protocol::{RepositoryId, WorkspaceId},
};

use super::{
    workspaces::{connected_client, next_workspace_errand},
    *,
};
use crate::{
    broker::sidekick_acts::{acted, refused},
    repositories::git,
    workspace_registry::entries_on_disk,
};

/// How the Registry spells a Workspace a Sidekick Cloned.
const BY_CLONE: &str = "\"cloned\"";

/// A path as a Workspace Root, a Repository URL or a refusal spells it.
fn spelled(path: &Path) -> &str {
    path.to_str().expect("the fixture's path is UTF-8")
}

/// A directory of the test's own, and its path as the Server reads it.
fn directory() -> (tempfile::TempDir, PathBuf) {
    let made = tempfile::tempdir().expect("create a directory");
    let path = suru::paths::canonical(made.path()).expect("read the directory canonically");
    (made, path)
}

/// Names `roots` the Workspace Roots, in a Config Document in `config_dir`.
fn name_roots(config_dir: &Path, roots: &[&str]) {
    std::fs::write(
        config_dir.join("suru.jsonc"),
        json!({ "workspace": { "roots": roots } }).to_string(),
    )
    .expect("name the Workspace Roots");
}

/// A bare Repository at `within`/`name`.git, as the server a Repository URL
/// names would hold it: one commit on `main` carrying a README, and a branch
/// `charts` besides. Answers its path.
fn bare_repository(within: &Path, name: &str) -> PathBuf {
    let work = within.join(format!("{name}-work"));
    std::fs::create_dir_all(&work).expect("create the working copy");
    git(&work, &["init", "-b", "main"]);
    std::fs::write(work.join("README.md"), readme(name)).expect("write the README");
    git(&work, &["add", "README.md"]);
    git(
        &work,
        &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
    );
    git(&work, &["branch", "charts"]);
    let bare = within.join(format!("{name}.git"));
    git(
        within,
        &["clone", "--bare", "--quiet", spelled(&work), spelled(&bare)],
    );
    bare
}

/// The README a Repository [`bare_repository`] makes as `name` carries.
fn readme(name: &str) -> String {
    format!("# {name}\n\nWhere the {name} is charted.\n")
}

/// `path` as a file URL, drive and all on Windows.
fn file_url(path: &Path) -> String {
    let spelled = spelled(path).replace('\\', "/");
    if spelled.starts_with('/') {
        format!("file://{spelled}")
    } else {
        format!("file:///{spelled}")
    }
}

/// What Git prints, trimmed, for `args` run in `directory`.
fn git_says(directory: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The identity of the Repository whose main root is `clone`.
fn repository_id(clone: &Path) -> WorkspaceId {
    RepositoryId::from_metadata("git", &clone.join(".git")).workspace_id()
}

/// The row `list_workspaces` gives the Repository Cloned to `clone` while it
/// carries `description` and the Icon named `icon`.
fn cloned_row(clone: &Path, description: Value, icon: Value) -> Value {
    json!({
        "workspace_id": repository_id(clone),
        "name": clone
            .file_name()
            .expect("a clone's directory has a name")
            .to_string_lossy(),
        "path": clone,
        "description": description,
        "icon": icon,
    })
}

/// `list_workspaces`' rows, in its order.
async fn list_workspaces(client: &mut McpClient) -> Vec<Value> {
    acted(client, "list_workspaces", json!({})).await["workspaces"]
        .as_array()
        .expect("a listing lists Workspaces")
        .clone()
}

/// The names of what stands directly within `directory`, sorted.
fn contents(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .expect("read the directory")
        .map(|entry| {
            entry
                .expect("read an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// Everything logged on this test's thread — where the Server it hosts runs
/// — at info and above, kept as it is written.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Captures what is logged on this thread for as long as the answer is
    /// held.
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .finish(),
        )
    }

    fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// With one Workspace Root configured, a Sidekick Clones a Repository URL —
/// a bare Repository's path — into it under the URL's last segment without
/// `.git`, and is answered once the Clone is done with the new Workspace as
/// list_workspaces rows it: an ordinary clone of the Repository's default
/// branch, neither shallow nor anything but whole, entered in the Registry as
/// Cloned and recorded in the Log with its URL and where it went.
#[tokio::test]
async fn a_sidekick_clones_a_repository_url_into_the_only_root_and_is_answered_its_row() {
    const CHANNEL: &str = "sidekick-clone-into-the-root";
    let log = Captured::default();
    let _log = log.install();
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let atlas = projects.join("atlas");
    let answered = acted(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin }),
    )
    .await;
    assert_eq!(
        answered,
        cloned_row(&atlas, Value::Null, Value::Null),
        "the Clone goes directly within the only Root, named as the URL ends without .git"
    );
    assert!(
        list_workspaces(&mut sidekick).await.contains(&answered),
        "the answer is the row list_workspaces gives the new Workspace"
    );
    assert_eq!(
        std::fs::read_to_string(atlas.join("README.md")).expect("the clone holds the README"),
        readme("atlas")
    );
    assert_eq!(
        git_says(&atlas, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "the Repository's default branch is checked out"
    );
    assert_eq!(
        git_says(&atlas, &["rev-parse", "--is-shallow-repository"]),
        "false",
        "the clone is Git's ordinary one"
    );
    assert_eq!(
        contents(&projects),
        ["atlas"],
        "nothing else is made in the Root"
    );

    let logged = log.logged();
    assert!(
        logged.lines().any(|line| line.contains(spelled(&origin))
            && line.contains(spelled(&atlas))
            && line.contains("Cloned")),
        "the Log records the Clone with its URL and destination: {logged}"
    );

    hosted.server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(
        &ServerConfig::new(state_dir.path(), CHANNEL).expect("configure the stopped Server"),
    );
    assert_eq!(
        entries.get(&repository_id(&atlas)),
        Some(&(Some(atlas.clone()), BY_CLONE.to_owned())),
        "the Registry holds the new Workspace as Cloned: {entries:?}"
    );
}

/// A Cloned Workspace has its Icon and Description derived at once, as a
/// first Session begun there would have them: the Errand asks the Sidekick's
/// own Provider, in the clone, for what the Workspace named by its directory
/// and its README is, and what it answers lands on the row list_workspaces
/// gives.
#[tokio::test]
async fn a_cloned_workspace_has_its_icon_and_description_derived_at_once() {
    const CHANNEL: &str = "sidekick-clone-derives";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let mut user = connected_client(state_dir.path(), CHANNEL).await;

    let atlas = projects.join("atlas");
    acted(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin }),
    )
    .await;
    let errand = next_workspace_errand(&mut hosted.claude).await;
    assert_eq!(
        errand.execution_directory(),
        atlas,
        "the Errand runs in the clone"
    );
    assert!(
        errand.prompt().contains("atlas")
            && errand.prompt().contains("Where the atlas is charted."),
        "the Errand names the Workspace and carries its README: {}",
        errand.prompt()
    );
    errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the atlas is charted.",
    }));
    let cloned = repository_id(&atlas);
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(ManagedEvent::WorkspaceIconChanged(changed)) = user.next().await
                && changed.workspace_id == cloned
            {
                return;
            }
        }
    })
    .await
    .expect("the derived Icon lands");

    let rows = list_workspaces(&mut sidekick).await;
    assert!(
        rows.contains(&cloned_row(
            &atlas,
            json!({ "text": "Where the atlas is charted.", "set": false }),
            json!("dev-rust"),
        )),
        "the Workspace wears what was derived for it: {rows:?}"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// With several Workspace Roots configured, a Clone names the one it goes in
/// — as the Setting spells it, or as the directory it names — and one naming
/// none of them, or naming none at all, is refused naming every Root
/// configured. A Clone on a Remote is not yet understood, and is refused.
/// Nothing a refusal answers is cloned anywhere.
#[tokio::test]
async fn with_several_roots_a_clone_names_its_root_and_is_refused_one_that_is_none_of_them() {
    const CHANNEL: &str = "sidekick-clone-several-roots";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_archive, archive) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "charts");
    name_roots(config_dir.path(), &[spelled(&projects), spelled(&archive)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let url = file_url(&origin);
    let configured = format!("`{}`, `{}`", spelled(&projects), spelled(&archive));

    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url }),
        )
        .await,
        format!(
            "Several Workspace Roots are configured on this server, so name the one to Clone \
             into as `root`: {configured}."
        )
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url, "root": elsewhere }),
        )
        .await,
        format!(
            "`{}` is none of this server's Workspace Roots, so nothing was cloned; name one of \
             those configured as `root`: {configured}.",
            spelled(&elsewhere)
        )
    );
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url, "root": projects, "origin": "workstation" }),
        )
        .await,
        "clone_workspace does not yet understand a Clone on a Remote, so nothing was cloned on \
         `workstation`; leave `origin` out to Clone within this server's Workspace Roots."
    );
    assert!(
        contents(&projects).is_empty() && contents(&archive).is_empty(),
        "nothing refused is cloned anywhere"
    );

    let charts = archive.join("charts");
    assert_eq!(
        acted(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url, "root": archive }),
        )
        .await,
        cloned_row(&charts, Value::Null, Value::Null),
        "a Root named as the Setting spells it takes the Clone, named as its file URL ends"
    );
    let through = format!("{}{}", spelled(&projects), std::path::MAIN_SEPARATOR);
    assert_eq!(
        acted(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url, "root": through }),
        )
        .await,
        cloned_row(&projects.join("charts"), Value::Null, Value::Null),
        "and so does one named by another spelling of the directory it names"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// No Clone is made where the user has named no Workspace Root, nor in a
/// Root that cannot be read — one that is not there, or that is no
/// directory — and a Root that is not there is not made for it.
#[tokio::test]
async fn a_clone_is_refused_where_no_root_is_configured_or_its_root_cannot_be_read() {
    const CHANNEL: &str = "sidekick-clone-no-root";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let user = connected_client(state_dir.path(), CHANNEL).await;

    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin }),
        )
        .await,
        "No Workspace Root is configured on this server, so there is nowhere to Clone into; ask \
         the user to name the directories their work lives in as Workspace Roots, in the \
         workspace.roots Setting."
    );

    let unplugged = elsewhere.join("unplugged");
    user.mutate_setting(SettingMutation::WorkspaceRoots {
        value: Some(vec![spelled(&unplugged).to_owned()]),
    })
    .await
    .expect("the user names a Root that is not there");
    let refusal = refused(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin }),
    )
    .await;
    assert!(
        refusal.starts_with(&format!(
            "The Workspace Root `{}` cannot be read (",
            spelled(&unplugged)
        )) && refusal.ends_with(
            "), so nothing was cloned; Suru never makes a Workspace Root, so ask the user to \
             make it or to name another."
        ),
        "{refusal}"
    );
    assert!(!unplugged.exists(), "the Root is not made for the Clone");

    let file = elsewhere.join("notes.txt");
    std::fs::write(&file, "Not a directory.").expect("write a file");
    user.mutate_setting(SettingMutation::WorkspaceRoots {
        value: Some(vec![spelled(&file).to_owned()]),
    })
    .await
    .expect("the user names a file as a Root");
    let refusal = refused(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin }),
    )
    .await;
    assert!(
        refusal.starts_with(&format!(
            "The Workspace Root `{}` cannot be read (",
            spelled(&file)
        )),
        "{refusal}"
    );

    drop(user);
    hosted.server.shutdown().await.expect("shut down server");
}

/// A `name` is one plain directory name made within the Root, never a path:
/// one that climbs out of the Root, reaches into a directory within it, is
/// a path of its own, or names nothing is refused, and nothing is made. One
/// that is plain names the Clone, and a `branch` checks that branch out.
#[tokio::test]
async fn a_name_must_be_one_plain_directory_name_and_a_branch_is_checked_out() {
    const CHANNEL: &str = "sidekick-clone-names";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let before = contents(&elsewhere);

    for name in [
        "../escaped",
        "nested/atlas",
        "nested\\atlas",
        "..",
        ".",
        "",
        "   ",
        spelled(&elsewhere.join("atlas-copy")),
    ] {
        assert_eq!(
            refused(
                &mut sidekick,
                "clone_workspace",
                json!({ "repository_url": origin, "name": name }),
            )
            .await,
            format!(
                "`{name}` is not one plain directory name, so nothing was cloned; give `name` as \
                 the name of one directory to make within the Workspace Root, with no path in it."
            ),
            "{name:?}"
        );
    }
    assert!(
        contents(&projects).is_empty(),
        "nothing is made in the Root"
    );
    assert_eq!(contents(&elsewhere), before, "nor anywhere else");

    let charts = projects.join("charts");
    assert_eq!(
        acted(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "name": "charts", "branch": "charts" }),
        )
        .await,
        cloned_row(&charts, Value::Null, Value::Null),
        "a plain name names the Clone"
    );
    assert_eq!(
        git_says(&charts, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "charts",
        "the branch named is checked out"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// Whatever already stands where a Clone would go refuses it and is left as
/// it was: a Workspace there is named by its workspace_id — one the Server
/// knows, and a Repository it has not yet been told of — and a plain
/// directory is only said to be there.
#[tokio::test]
async fn an_existing_destination_is_refused_naming_the_workspace_there() {
    const CHANNEL: &str = "sidekick-clone-occupied";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_elsewhere, elsewhere) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let occupied = |destination: &Path, workspace: Option<&WorkspaceId>| match workspace {
        Some(workspace) => format!(
            "`{}` already exists, so nothing was cloned there: it is the Workspace `{}`; give \
             another `name` to Clone beside it.",
            spelled(destination),
            workspace.0
        ),
        None => format!(
            "`{}` already exists, so nothing was cloned there; give another `name` to Clone \
             beside it.",
            spelled(destination)
        ),
    };

    let atlas = projects.join("atlas");
    acted(
        &mut sidekick,
        "clone_workspace",
        json!({ "repository_url": origin }),
    )
    .await;
    std::fs::write(atlas.join("NOTES.md"), "Mine.").expect("work in the clone");
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin }),
        )
        .await,
        occupied(&atlas, Some(&repository_id(&atlas))),
        "a Workspace the Server knows is named"
    );
    assert_eq!(
        std::fs::read_to_string(atlas.join("NOTES.md")).expect("the work stands"),
        "Mine."
    );

    let maps = projects.join("maps");
    std::fs::create_dir(&maps).expect("create a Repository by hand");
    git(&maps, &["init", "-b", "main"]);
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "name": "maps" }),
        )
        .await,
        occupied(&maps, Some(&repository_id(&maps))),
        "so is a Repository the Server was never told of"
    );

    let notes = projects.join("notes");
    std::fs::create_dir(&notes).expect("create a plain directory");
    std::fs::write(notes.join("todo.txt"), "Chart the atlas.").expect("write a note");
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "name": "notes" }),
        )
        .await,
        occupied(&notes, None),
        "a plain directory is no Workspace to name"
    );
    assert_eq!(contents(&notes), ["todo.txt"], "and is left as it was");

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Clone Git cannot make is refused in Git's own words — a Repository URL
/// naming nothing, or a branch the Repository does not have — and leaves
/// nothing at its destination.
#[tokio::test]
async fn a_clone_git_cannot_make_is_refused_in_gits_own_words_and_leaves_nothing() {
    const CHANNEL: &str = "sidekick-clone-git-fails";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let (_elsewhere, elsewhere) = directory();
    let (_scratch, scratch) = directory();
    let origin = bare_repository(&elsewhere, "atlas");
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    // What Git says making the very same Clone itself.
    let git_refuses = |url: &str, branch: Option<&str>| {
        let mut command = std::process::Command::new("git");
        command.arg("-C").arg(&scratch).args(["clone", "--quiet"]);
        if let Some(branch) = branch {
            command.arg(format!("--branch={branch}"));
        }
        let output = command
            .args(["--", url, "attempt"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run Git");
        assert!(!output.status.success(), "Git refuses {url}");
        String::from_utf8_lossy(&output.stderr).trim().to_owned()
    };

    let missing = elsewhere.join("missing.git");
    let said = git_refuses(spelled(&missing), None);
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": missing }),
        )
        .await,
        format!(
            "Git could not Clone `{}`, so nothing was left at `{}`: {said}",
            spelled(&missing),
            spelled(&projects.join("missing"))
        )
    );

    let said = git_refuses(spelled(&origin), Some("nowhere"));
    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": origin, "branch": "nowhere" }),
        )
        .await,
        format!(
            "Git could not Clone `{}`, so nothing was left at `{}`: {said}",
            spelled(&origin),
            spelled(&projects.join("atlas"))
        )
    );
    assert!(
        contents(&projects).is_empty(),
        "no failed Clone leaves anything"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Clone still running when the Server's bound on one passes is stopped
/// and refused saying so, and what Git had begun making is removed: here, a
/// Repository URL naming a server that takes the connection and never
/// answers.
#[tokio::test]
async fn a_clone_outlasting_the_servers_bound_is_refused_and_leaves_no_directory() {
    const CHANNEL: &str = "sidekick-clone-timed-out";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    name_roots(config_dir.path(), &[spelled(&projects)]);
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen for Git");
    let url = format!(
        "git://127.0.0.1:{}/atlas.git",
        silent.local_addr().expect("a listening address").port()
    );
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((connection, _)) = silent.accept().await {
            held.push(connection);
        }
    });
    let mut hosted = host_providers_with_timings(
        state_dir.path(),
        CHANNEL,
        Some(config_dir.path()),
        ServerTimings::default()
            .with_broker_wait_second(WAIT_SECOND)
            .with_broker_wait_progress_interval(WAIT_PROGRESS_EVERY)
            .with_workspace_clone_timeout(Duration::from_millis(500)),
    )
    .await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    assert_eq!(
        refused(
            &mut sidekick,
            "clone_workspace",
            json!({ "repository_url": url }),
        )
        .await,
        format!(
            "The Clone of `{url}` did not finish within 500 milliseconds, so it was stopped and \
             nothing was left at `{}`.",
            spelled(&projects.join("atlas"))
        )
    );
    assert!(
        contents(&projects).is_empty(),
        "nothing of the Clone is left in the Root"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// Only a Sidekick is offered `clone_workspace`, and its note names the Tool
/// and the Workspace Roots that bound it as the user named them; any other
/// Agent is neither offered nor told of it, and is answered as though it did
/// not exist.
#[tokio::test]
async fn only_a_sidekick_is_offered_clone_workspace_and_told_the_roots_that_bound_it() {
    const CHANNEL: &str = "sidekick-clone-offered";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let (_projects, projects) = directory();
    let roots = [spelled(&projects), "~/src"];
    name_roots(config_dir.path(), &roots);
    let mut hosted = host_providers(state_dir.path(), CHANNEL, Some(config_dir.path())).await;
    let descriptor = hosted.server.descriptor().clone();
    let directory = sidekick_directory(&descriptor).await;
    let (_sidekick_id, sidekick_handoff, _sidekick_provider) = start_session(
        &descriptor,
        &mut hosted.claude,
        &directory,
        default_selection(&claude_models()),
    )
    .await;
    let mut sidekick = McpClient::handed(&sidekick_handoff);
    sidekick.initialize().await;
    let workspace = hosted.workspace.path().to_owned();
    let (_ordinary_id, ordinary_handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&ordinary_handoff);
    ordinary.initialize().await;

    let listed = sidekick.request("tools/list", json!({})).await;
    let cloning = listed["tools"]
        .as_array()
        .expect("tools/list lists Tools")
        .iter()
        .find(|described| described["name"] == json!("clone_workspace"))
        .unwrap_or_else(|| panic!("a Sidekick is offered clone_workspace: {listed}"))
        .clone();
    assert_eq!(
        cloning["annotations"]["readOnlyHint"],
        json!(false),
        "a Clone writes to disk: {cloning}"
    );
    assert_eq!(
        cloning["inputSchema"]["required"],
        json!(["repository_url"]),
        "{cloning}"
    );
    let note = sidekick_handoff.instruction_note(|tool| format!("mcp__suru__{tool}"));
    assert!(
        note.contains("mcp__suru__clone_workspace")
            && note.contains(&serde_json::to_string(&roots).expect("the Roots as JSON")),
        "a Sidekick is told of the Tool and of the Roots that bound it: {note}"
    );

    assert!(
        !listed_tools(&mut ordinary)
            .await
            .iter()
            .any(|listed| listed == "clone_workspace"),
        "no other Agent is offered clone_workspace"
    );
    assert!(
        !ordinary_handoff
            .instruction_note(|tool| format!("mcp__suru__{tool}"))
            .contains("clone_workspace"),
        "nor told of it"
    );
    assert_eq!(
        unoffered(
            &mut ordinary,
            "clone_workspace",
            json!({ "repository_url": "https://example.invalid/atlas.git" }),
        )
        .await["message"],
        json!("The Broker offers no Tool named `clone_workspace`")
    );
    assert!(contents(&projects).is_empty(), "and nothing is cloned");

    hosted.server.shutdown().await.expect("shut down server");
}
