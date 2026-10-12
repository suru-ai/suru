//! Cloning a Repository URL into a Workspace Root: the act a Sidekick's
//! `clone_workspace` performs (ADR 0051). A Clone goes directly within the
//! Workspace Root it names — as the Setting naming the Roots spells it, or
//! as the directory it names — or the only one configured, and nowhere else:
//! under the name it gives, which must be one plain directory name, or else
//! the one its Repository URL ends in. A Root is never made for it, so one
//! that cannot be read refuses it, and whatever already stands where it
//! would go refuses it too, named as the Workspace it is where it is one.
//!
//! The Clone itself is source control's ([`SourceControlService::clone_repository`]),
//! bounded by this Server's own timeout, and leaves nothing behind where it
//! fails or runs out of time. One that lands is read apart, within a bound
//! of its own, and entered in the Workspace Registry as Cloned — as read,
//! or as the Clone knew it where the reading does not answer in time, and
//! no scan of the Roots happening upon it first takes it for discovered —
//! has its Icon and Description derived at once, as the
//! first Session begun there would, and has the Roots discovered afresh,
//! waiting on that no longer than a listing would; the Log records what was
//! Cloned where, the URL's credential aside.
//!
//! A Clone is a Workspace act, so a Sidekick may ask it of a Remote: its own
//! Server carries it there through the Pairing as the act of a Sidekick on
//! this Peer, as it carries a Workspace's Description (ADR 0044), and the
//! Remote Clones it by this very operation — within its own Roots, by every
//! rule here — answering the Workspace made with the paths it is spelled in,
//! or its refusal in its own words. Only a Sidekick Clones, so a Remote
//! Clones for nothing but a Sidekick on a Peer, which it keeps out of its own
//! Sidekick Workspace as it does from every act there. Such a Clone has its
//! Icon and Description derived on the Remote, at the Agent its Landing
//! begins a Session with where none is named, as a Session that Sidekick
//! began there would be; and since a Clone takes as long as Git does, the
//! Remote is given as long to answer as one may take.
//!
//! [`SourceControlService::clone_repository`]: crate::source_control::SourceControlService::clone_repository

use std::{
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::http::Method;

use super::{
    SessionOperations,
    origins::{RemoteActRefusal, WORKSPACES_PATH},
};
use crate::{
    protocol::{
        ActId, Author, CloneWorkspaceRequest, ClonedWorkspace, SourceControlCapability, Workspace,
        WorkspaceId,
    },
    server::{
        reads_apart::{NotRead, ReadsApart},
        root_discovery::expand,
    },
    source_control::{CloneFailure, without_credentials},
};

/// The most reads a Clone makes apart — of Workspace Roots, and of what
/// stands where a Clone goes — that may be outstanding at once, each on a
/// thread of its own: a directory that never answers holds one of them
/// however often it is asked for again.
const ROOT_READS_AT_ONCE: usize = 8;

/// How long reading the Workspace a Clone has just landed may take before
/// the Clone answers with what it knew of it instead: a bound of its own,
/// past the Clone's deadline, since the clone has landed whatever the time.
const LANDED_READ_WITHIN: Duration = Duration::from_secs(5);

/// What a Clone takes that is this Server's own: the home a Workspace Root
/// beginning with `~` lies beneath, how long a Clone may run, and the reads
/// apart a Root is read by.
#[derive(Clone)]
pub(crate) struct WorkspaceCloning {
    home: Option<PathBuf>,
    timeout: Duration,
    reads: Arc<ReadsApart>,
}

impl WorkspaceCloning {
    pub(crate) fn new(home: Option<PathBuf>, timeout: Duration) -> Self {
        Self {
            home,
            timeout,
            reads: Arc::new(ReadsApart::new("suru-clone-root-read", ROOT_READS_AT_ONCE)),
        }
    }
}

/// Why a Clone was refused, in words a Sidekick can pass on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CloneRefusal {
    /// The user has named no Workspace Root.
    NoRoot,
    /// Several Roots are configured, and the Clone named none of them.
    RootUnnamed { roots: Vec<String> },
    /// The Clone named a Root that is none of those configured.
    UnknownRoot { named: String, roots: Vec<String> },
    /// The Root cannot be read, as `reason` says.
    UnreadableRoot { root: String, reason: String },
    /// The name given is not one plain directory name.
    NotPlainName { name: String },
    /// No name was given, and the Repository URL ends in none.
    NoNameInUrl { url: String },
    /// Something already stands at the destination: the Workspace named
    /// there, where it is one.
    Occupied {
        destination: PathBuf,
        workspace: Option<WorkspaceId>,
    },
    /// A Sidekick on a Peer asked for a Clone whose destination lies within
    /// this Server's own Sidekick Workspace, which a Remote keeps to itself
    /// (ADR 0044).
    SidekickWorkspace { destination: PathBuf },
    /// This Server's source control does not Clone, as `reason` says.
    Unsupported { reason: String },
    /// Git failed the Clone, in its own words, and nothing was left behind.
    Failed {
        url: String,
        destination: PathBuf,
        said: String,
    },
    /// Git failed the Clone, or ran out of time, and what it left behind
    /// could not be removed, as `said` says.
    LeftBehind {
        url: String,
        destination: PathBuf,
        said: String,
    },
    /// The Clone ran past the Server's bound on one, was stopped, and
    /// nothing was left behind: at `destination`, where it was known, or
    /// anywhere, where the Root it was to go in did not answer in time to
    /// tell.
    TimedOut {
        url: String,
        destination: Option<PathBuf>,
        timeout: Duration,
    },
    /// The clone was whole but still taking its name at `destination` when
    /// the Server's bound on a Clone passed: it may yet land there, and is
    /// then listed as discovery finds it.
    StillLanding {
        url: String,
        destination: PathBuf,
        timeout: Duration,
    },
}

impl fmt::Display for CloneRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoot => formatter.write_str(
                "No Workspace Root is configured on this server, so there is nowhere to Clone \
                 into; ask the user to name the directories their work lives in as Workspace \
                 Roots, in the workspace.roots Setting.",
            ),
            Self::RootUnnamed { roots } => write!(
                formatter,
                "Several Workspace Roots are configured on this server, so name the one to Clone \
                 into as `root`: {}.",
                listed(roots)
            ),
            Self::UnknownRoot { named, roots } => write!(
                formatter,
                "`{named}` is none of this server's Workspace Roots, so nothing was cloned; name \
                 one of those configured as `root`: {}.",
                listed(roots)
            ),
            Self::UnreadableRoot { root, reason } => write!(
                formatter,
                "The Workspace Root `{root}` cannot be read ({reason}), so nothing was cloned; \
                 Suru never makes a Workspace Root, so ask the user to make it or to name another."
            ),
            Self::NotPlainName { name } => write!(
                formatter,
                "`{name}` is not one plain directory name, so nothing was cloned; give `name` as \
                 the name of one directory to make within the Workspace Root, with no path in it."
            ),
            Self::NoNameInUrl { url } => write!(
                formatter,
                "No directory name can be told from the Repository URL `{url}`, so nothing was \
                 cloned; give one as `name`."
            ),
            Self::Occupied {
                destination,
                workspace: Some(workspace),
            } => write!(
                formatter,
                "`{}` already exists, so nothing was cloned there: it is the Workspace `{}`; give \
                 another `name` to Clone beside it.",
                destination.display(),
                workspace.0
            ),
            Self::Occupied {
                destination,
                workspace: None,
            } => write!(
                formatter,
                "`{}` already exists, so nothing was cloned there; give another `name` to Clone \
                 beside it.",
                destination.display()
            ),
            Self::SidekickWorkspace { destination } => write!(
                formatter,
                "`{}` lies within the Sidekick Workspace, which a Sidekick on another machine may \
                 not Clone into, so nothing was cloned.",
                destination.display()
            ),
            Self::Unsupported { reason } => write!(
                formatter,
                "This server's source control cannot Clone a Repository URL ({reason}), so \
                 nothing was cloned."
            ),
            Self::Failed {
                url,
                destination,
                said,
            } => write!(
                formatter,
                "Git could not Clone `{url}`, so nothing was left at `{}`: {said}",
                destination.display()
            ),
            Self::LeftBehind {
                url,
                destination,
                said,
            } => write!(
                formatter,
                "Git could not Clone `{url}`, and what it left at `{}` could not be removed: \
                 {said}",
                destination.display()
            ),
            Self::TimedOut {
                url,
                destination: Some(destination),
                timeout,
            } => write!(
                formatter,
                "The Clone of `{url}` did not finish within {}, so it was stopped and nothing \
                 was left at `{}`.",
                spelled(*timeout),
                destination.display()
            ),
            Self::StillLanding {
                url,
                destination,
                timeout,
            } => write!(
                formatter,
                "The Clone of `{url}` did not finish within {}: the whole clone was still taking \
                 its name, `{}`, so it may yet land there, and will then be listed among this \
                 server's Workspaces; if it cannot, nothing will be left of it.",
                spelled(*timeout),
                destination.display()
            ),
            Self::TimedOut {
                url,
                destination: None,
                timeout,
            } => write!(
                formatter,
                "The Clone of `{url}` did not finish within {}: the Workspace Root it was to go \
                 in did not answer in time, so nothing was cloned.",
                spelled(*timeout)
            ),
        }
    }
}

impl CloneRefusal {
    /// What the Log calls a refusal that came of running the Clone, for
    /// which the Log says nothing of the URL's credential or Git's words.
    fn outcome(&self) -> Option<&'static str> {
        match self {
            Self::Failed { .. } => Some("failed"),
            Self::LeftBehind { .. } => Some("failed, leaving what it made behind"),
            Self::TimedOut { .. } => Some("timed out"),
            Self::StillLanding { .. } => Some("was still landing when its time ran out"),
            Self::Occupied { .. } => Some("found its destination occupied"),
            Self::NoRoot
            | Self::RootUnnamed { .. }
            | Self::UnknownRoot { .. }
            | Self::UnreadableRoot { .. }
            | Self::NotPlainName { .. }
            | Self::NoNameInUrl { .. }
            | Self::SidekickWorkspace { .. }
            | Self::Unsupported { .. } => None,
        }
    }
}

/// The Workspace Roots as a refusal names them: each as the Setting spells
/// it, quoted, in the Setting's order.
fn listed(roots: &[String]) -> String {
    roots
        .iter()
        .map(|root| format!("`{root}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The Sidekick a Clone is done for, as the Log names it: one of this
/// Server's by its Session, or one on a Peer by that Peer's name and key.
struct Cloner<'a>(&'a Author);

impl fmt::Display for Cloner<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Author::Sidekick { session_id, .. } => write!(formatter, "{session_id}"),
            Author::PeerSidekick {
                peer, fingerprint, ..
            } => write!(formatter, "a Sidekick on the Peer `{peer}` ({fingerprint})"),
        }
    }
}

/// A timeout in the plainest whole unit that spells it.
fn spelled(timeout: Duration) -> String {
    let millis = timeout.as_millis();
    let (amount, unit) = if millis > 0 && millis.is_multiple_of(60_000) {
        (millis / 60_000, "minute")
    } else if millis > 0 && millis.is_multiple_of(1_000) {
        (millis / 1_000, "second")
    } else {
        (millis, "millisecond")
    };
    let plural = if amount == 1 { "" } else { "s" };
    format!("{amount} {unit}{plural}")
}

/// The Workspace Root among `roots` a Clone goes in: the one `named` names,
/// as the Setting spells it or as the directory it names — however either
/// spells that directory, so long as it can be read — or the only one where
/// it names none.
fn chosen_root<'r>(
    roots: &'r [String],
    named: Option<&str>,
    home: Option<&Path>,
) -> Result<&'r str, CloneRefusal> {
    match (roots, named) {
        ([], _) => Err(CloneRefusal::NoRoot),
        ([only], None) => Ok(only),
        (_, None) => Err(CloneRefusal::RootUnnamed {
            roots: roots.to_vec(),
        }),
        (_, Some(named)) => {
            let directory = expand(named, home);
            let read = || {
                directory
                    .as_ref()
                    .and_then(|directory| crate::paths::canonical(directory).ok())
            };
            let spelled_alike = roots.iter().find(|root| {
                root.as_str() == named
                    || directory
                        .as_ref()
                        .is_some_and(|directory| expand(root, home).as_ref() == Some(directory))
            });
            spelled_alike
                .or_else(|| {
                    let read = read()?;
                    roots.iter().find(|root| {
                        expand(root, home)
                            .and_then(|root| crate::paths::canonical(root).ok())
                            .is_some_and(|root| root == read)
                    })
                })
                .map(String::as_str)
                .ok_or_else(|| CloneRefusal::UnknownRoot {
                    named: named.to_owned(),
                    roots: roots.to_vec(),
                })
        }
    }
}

/// The directory the Workspace Root `root` names, as this Server reads it,
/// where it can be read.
fn readable_root(root: &str, home: Option<&Path>) -> Result<PathBuf, CloneRefusal> {
    let unreadable = |reason: String| CloneRefusal::UnreadableRoot {
        root: root.to_owned(),
        reason,
    };
    let directory = expand(root, home).ok_or_else(|| {
        unreadable("it begins with ~ but names no home this server knows".to_owned())
    })?;
    std::fs::read_dir(&directory).map_err(|error| unreadable(error.to_string()))?;
    crate::paths::canonical(&directory).map_err(|error| unreadable(error.to_string()))
}

/// The characters no Windows file name may hold: every separator any
/// platform reads, a drive's colon, and the wildcards and redirections the
/// Windows shell reserves.
const UNPORTABLE: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// `name`, where it is one plain directory name, and the same one directory
/// on every platform Suru runs on: something besides space, naming one
/// directory within whatever directory it is joined to — never `.` or `..`
/// — and holding no control character nor any of the [`UNPORTABLE`] ones,
/// ending in neither a dot nor a space, which Windows takes away, and naming
/// no device Windows reserves. A name refused on one platform is refused on
/// all, so that a Clone's name never means one directory here and another,
/// or none, there — and never a directory outside its Root.
fn plain_name(name: &str) -> Result<&str, CloneRefusal> {
    let mut components = Path::new(name).components();
    let plain = !name.trim().is_empty()
        && !name.contains(UNPORTABLE)
        && !name.chars().any(char::is_control)
        && !name.ends_with(['.', ' '])
        && !names_a_device(name)
        && matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(only)), None) if only == name
        );
    if plain {
        Ok(name)
    } else {
        Err(CloneRefusal::NotPlainName {
            name: name.to_owned(),
        })
    }
}

/// Whether `name` is one Windows reserves for a device — `CON`, `PRN`,
/// `AUX`, `NUL`, `CONIN$`, `CONOUT$`, and `COM` or `LPT` with one digit, a
/// superscript one among them — in any case, and whatever follows its first
/// dot or the spaces before it.
fn names_a_device(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let mut characters = stem.chars();
    let port = characters.by_ref().take(3).collect::<String>();
    matches!(port.as_str(), "COM" | "LPT")
        && matches!(
            (characters.next(), characters.next()),
            (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
        )
}

/// The path within its Repository URL a Repository is named by, apart from
/// where that Repository is and who may reach it: what follows the host of
/// a URL spelled with a scheme — `https://`, `ssh://`, `file://` and the
/// rest — and of Git's scp-like `user@host:path`, what follows the host's
/// colon; any other address, a local path, is its own. Nothing of a URL's
/// user, password or host is ever part of it.
fn repository_path(url: &str) -> &str {
    if let Some(scheme) = url.find("://").filter(|&scheme| {
        scheme > 0
            && url[..scheme]
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
    }) {
        let located = &url[scheme + "://".len()..];
        let path = located
            .find(['/', '\\'])
            .map_or("", |start| &located[start..]);
        return path.split(['?', '#']).next().unwrap_or(path);
    }
    match url.find(':') {
        // A drive's colon begins a local path, as one after a separator ends
        // no host.
        Some(1) if url.starts_with(|first: char| first.is_ascii_alphabetic()) => url,
        Some(colon) if !url[..colon].contains(['/', '\\']) => &url[colon + 1..],
        _ => url,
    }
}

/// The directory name a Clone of the Repository `url` names takes where it
/// is given none, as Git names one: the last segment of the path it names
/// its Repository by (see [`repository_path`]), without `.git` — never
/// anything of its user, password or host.
fn named_by(url: &str) -> Result<&str, CloneRefusal> {
    let path = repository_path(url);
    let trimmed = path.trim_end_matches(['/', '\\']);
    let trimmed = trimmed
        .strip_suffix("/.git")
        .or_else(|| trimmed.strip_suffix("\\.git"))
        .unwrap_or(trimmed);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    plain_name(last.strip_suffix(".git").unwrap_or(last)).map_err(|_| CloneRefusal::NoNameInUrl {
        url: url.to_owned(),
    })
}

/// Where a Clone named `name` goes — directly within the Root `named`
/// names among `roots`, or the only one — and whether something stands
/// there already, the Root read by `read` apart (see [`ReadsApart`]), so a
/// Root on a mount gone quiet holds up none of the Server's own threads,
/// and holds one of its own however often it is Cloned into again. `None`
/// where the reading has not answered by `deadline`, or one begun earlier
/// still has not.
async fn placed(
    reads: &ReadsApart,
    deadline: tokio::time::Instant,
    roots: Vec<String>,
    named: Option<String>,
    name: String,
    home: Option<PathBuf>,
    read: impl FnOnce(&str, Option<&Path>) -> Result<PathBuf, CloneRefusal> + Send + 'static,
) -> Option<Result<(PathBuf, bool), CloneRefusal>> {
    // No Root, or several and none named, is refused without reading one.
    let asked = match (&named, roots.as_slice()) {
        (Some(named), _) => named.clone(),
        (None, [only]) => only.clone(),
        (None, _) => {
            return Some(chosen_root(&roots, None, home.as_deref()).map(|_| unreachable!()));
        }
    };
    let placing = reads.read(Path::new(&asked), move || async move {
        let root = chosen_root(&roots, named.as_deref(), home.as_deref())?;
        let destination = read(root, home.as_deref())?.join(name);
        let occupied = std::fs::symlink_metadata(&destination).is_ok();
        Ok((destination, occupied))
    });
    match tokio::time::timeout_at(deadline, placing).await {
        Err(_) | Ok(Err(NotRead::Outstanding)) => None,
        Ok(Err(not_read)) => Some(Err(CloneRefusal::UnreadableRoot {
            root: asked,
            reason: not_read.to_string(),
        })),
        Ok(Ok(placed)) => Some(placed),
    }
}

/// What `read` reads of `path` apart (see [`ReadsApart`]), where it answers
/// within `bound`; `None` where it does not, or where a read of `path` begun
/// earlier has still not ended.
async fn read_within<T, Reading>(
    reads: &ReadsApart,
    path: &Path,
    bound: Duration,
    read: impl FnOnce() -> Reading + Send + 'static,
) -> Option<T>
where
    T: Send + 'static,
    Reading: std::future::Future<Output = T>,
{
    tokio::time::timeout(bound, reads.read(path, read))
        .await
        .ok()?
        .ok()
}

/// The Workspace a Clone landed at `destination`: as `read` reads it there
/// apart, where that reading answers within `bound` and finds a Repository
/// presented there, and otherwise `known`, what the Clone knew of it — its
/// identity and where and how it is presented — so a mount gone quiet
/// holds up neither the Clone's answer nor any of the Server's threads.
async fn landed<Reading>(
    reads: &ReadsApart,
    destination: &Path,
    known: Workspace,
    bound: Duration,
    read: impl FnOnce() -> Reading + Send + 'static,
) -> Workspace
where
    Reading: std::future::Future<Output = crate::protocol::ResolvedWorkspace>,
{
    match read_within(reads, destination, bound, read).await {
        Some(reading)
            if reading.workspace.repository.is_some() && reading.workspace.path == destination =>
        {
            reading.workspace
        }
        _ => {
            tracing::debug!(
                destination = %destination.display(),
                "a Clone's Workspace could not be read in time, so it is entered as the Clone knew it"
            );
            known
        }
    }
}

impl SessionOperations {
    /// Clones the Repository `request` names directly within the Workspace
    /// Root it names, or the only one configured, for the Sidekick `author`
    /// names — this Server's own, or one on a Peer — and answers the
    /// Workspace made there as this Server's Workspace listing gives it —
    /// once its Registry entry is written, the Errand deriving its Icon and
    /// Description is begun, and discovery of the Roots is asked for again.
    /// Refuses, saying why and having made nothing, where no Root is there to
    /// take the Clone, where its Root cannot be read, where its name is no
    /// plain directory name, where something stands at its destination
    /// already, where a Sidekick on a Peer would Clone into this Server's own
    /// Sidekick Workspace, and where Git fails it or it outlasts this
    /// Server's bound on one.
    pub(crate) async fn clone_workspace(
        &self,
        request: &CloneWorkspaceRequest,
        author: &Author,
    ) -> Result<Workspace, CloneRefusal> {
        // One bound for the whole of it: reading the Root, cloning, naming
        // the clone, and removing what a Clone that failed made.
        let deadline = tokio::time::Instant::now() + self.cloning.timeout;
        let url = request.repository_url.as_str();
        let roots = self.settings.borrow().settings.workspace.roots.clone();
        let name = match &request.name {
            Some(name) => plain_name(name)?,
            None => named_by(url)?,
        }
        .to_owned();
        let Some(placed) = placed(
            &self.cloning.reads,
            deadline,
            roots,
            request.root.clone(),
            name,
            self.cloning.home.clone(),
            readable_root,
        )
        .await
        else {
            return Err(CloneRefusal::TimedOut {
                url: url.to_owned(),
                destination: None,
                timeout: self.cloning.timeout,
            });
        };
        let (destination, occupied) = placed?;
        if matches!(author, Author::PeerSidekick { .. })
            && self.sidekick_workspace.contains(&destination)
        {
            return Err(CloneRefusal::SidekickWorkspace { destination });
        }
        if occupied {
            return Err(self.occupied(destination, deadline).await);
        }
        if let SourceControlCapability::Unsupported { reason } =
            self.source_control.clone_capability()
        {
            return Err(CloneRefusal::Unsupported { reason });
        }
        // No scan enters the clone as discovered between its taking its name
        // and the Clone entering it as Cloned.
        let landing = self.root_discovery.landing(&destination);
        let cloned = self
            .source_control
            .clone_repository(
                url,
                &destination,
                request.branch.as_deref(),
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await;
        let known = match cloned {
            Ok(known) => known,
            Err(failure) => {
                let refusal = match failure {
                    CloneFailure::Occupied => self.occupied(destination.clone(), deadline).await,
                    CloneFailure::Failed(said) => CloneRefusal::Failed {
                        url: url.to_owned(),
                        destination: destination.clone(),
                        said,
                    },
                    CloneFailure::LeftBehind(said) => CloneRefusal::LeftBehind {
                        url: url.to_owned(),
                        destination: destination.clone(),
                        said,
                    },
                    CloneFailure::TimedOut => CloneRefusal::TimedOut {
                        url: url.to_owned(),
                        destination: Some(destination.clone()),
                        timeout: self.cloning.timeout,
                    },
                    CloneFailure::StillLanding => CloneRefusal::StillLanding {
                        url: url.to_owned(),
                        destination: destination.clone(),
                        timeout: self.cloning.timeout,
                    },
                };
                if let Some(outcome) = refusal.outcome() {
                    tracing::info!(
                        repository_url = %without_credentials(url),
                        destination = %destination.display(),
                        sidekick = %Cloner(author),
                        "a Clone of a Repository URL into a Workspace Root {outcome}"
                    );
                }
                return Err(refusal);
            }
        };
        let source_control = self.source_control.clone();
        let reading = destination.clone();
        let workspace = landed(
            &self.cloning.reads,
            &destination,
            known,
            LANDED_READ_WITHIN,
            move || async move { source_control.read_unremembered(&reading).await },
        )
        .await;
        let workspace = self.sessions.enter_cloned(&workspace, &destination);
        drop(landing);
        // A Sidekick on a Peer has no Session here: its Clone's Errand runs
        // at the Agent a Session it began here naming none would run at.
        let provider = match author.sidekick_session() {
            Some(sidekick) => self.sessions.provider(sidekick),
            None => self.landing_selection().map(|selection| selection.provider),
        };
        self.derivation.derive_cloned_workspace(
            Cloner(author),
            provider,
            &workspace,
            destination.clone(),
        );
        self.rediscover_after_clone().await;
        tracing::info!(
            repository_url = %without_credentials(url),
            destination = %destination.display(),
            workspace_id = %workspace.id.0,
            sidekick = %Cloner(author),
            "a Sidekick Cloned a Repository URL into a Workspace Root"
        );
        Ok(self
            .registered_workspace_listing()
            .workspaces
            .into_iter()
            .find(|listed| listed.id == workspace.id)
            .unwrap_or(workspace))
    }

    /// Clones the Repository `request` names on the Remote `name` for
    /// `author`, carried there through the Pairing as the act of a Sidekick
    /// on this Peer, and answers the Workspace the Remote made with the paths
    /// it is spelled in. The Remote Clones it by [`Self::clone_workspace`],
    /// as its own Sidekick's Clone is done, and refuses it in its own words.
    /// A Clone takes as long as Git does, so the Remote is given as long to
    /// answer as one may take there — the bound on a Clone and on reading
    /// what landed, which no Setting changes, so the Remote's are this
    /// Server's own — and then as long as any act is given.
    pub(crate) async fn clone_remote_workspace(
        &self,
        name: &str,
        request: &CloneWorkspaceRequest,
        author: Author,
    ) -> Result<ClonedWorkspace, RemoteActRefusal> {
        let remotes = self
            .remotes
            .answering_within(self.cloning.timeout + LANDED_READ_WITHIN + self.remotes.timeout());
        remotes
            .act(
                name,
                Method::POST,
                &format!("{WORKSPACES_PATH}/clone"),
                Some(request),
                &author,
                ActId::new(),
            )
            .await?
            .read()
    }

    /// The refusal of a Clone to `destination`, where something already
    /// stands: naming the Workspace there where it is one — one this
    /// Server's Registry presents there, or one source control finds rooted
    /// there that it has not yet been told of: a Repository presented there,
    /// or a Worktree of one presented elsewhere. Source control reads it
    /// apart and no later than `deadline`, past which what stands there goes
    /// unnamed.
    async fn occupied(&self, destination: PathBuf, deadline: tokio::time::Instant) -> CloneRefusal {
        let registered = self
            .sessions
            .listed_workspaces()
            .into_iter()
            .find(|workspace| workspace.path == destination)
            .map(|workspace| workspace.id);
        let workspace = match registered {
            Some(workspace) => Some(workspace),
            None => {
                let source_control = self.source_control.clone();
                let reading = destination.clone();
                read_within(
                    &self.cloning.reads,
                    &destination,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                    move || async move { source_control.workspace_rooted_at(&reading).await },
                )
                .await
                .flatten()
            }
        };
        CloneRefusal::Occupied {
            destination,
            workspace,
        }
    }

    /// Has every Workspace Root discovered afresh once a Clone has landed in
    /// one and entered its own Workspace in the Registry — the one place a
    /// Clone asks for discovery (ADR 0051) — superseding a scan still running
    /// that read the Root before the clone took its name, and waits for it
    /// no longer than a listing waits on one. What the scan finds there
    /// leaves the Clone's own entry known by its Clone, as an entry already
    /// made always is.
    async fn rediscover_after_clone(&self) {
        let scans = self.root_discovery.rediscover();
        self.root_discovery.settled(scans).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path rooted as this platform roots one.
    fn rooted(path: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{}", path.replace('/', r"\"))
        } else {
            format!("/{path}")
        }
    }

    #[test]
    fn a_clone_is_named_as_its_repository_url_ends_without_git() {
        for (url, name) in [
            ("https://github.com/org/atlas.git", "atlas"),
            ("https://github.com/org/atlas", "atlas"),
            ("https://github.com/org/atlas.git/", "atlas"),
            ("https://example.com/org/atlas/.git", "atlas"),
            ("git@github.com:org/atlas.git", "atlas"),
            ("git@github.com:atlas.git", "atlas"),
            ("ssh://git@example.com:2222/srv/atlas.git", "atlas"),
            ("file:///C:/src/atlas.git", "atlas"),
            (r"C:\src\atlas.git", "atlas"),
            (r"C:\src\atlas\", "atlas"),
            ("/srv/git/atlas.git", "atlas"),
            ("../atlas.git", "atlas"),
            ("atlas.dotted.git", "atlas.dotted"),
            ("https://github.com/org/atlas.git?ref=main#readme", "atlas"),
            ("git+ssh://example.com/srv/atlas.git", "atlas"),
        ] {
            assert_eq!(named_by(url), Ok(name), "{url}");
        }
        for url in [
            "",
            ".git",
            "/",
            r"\",
            "https://example.com",
            "https://example.com/",
        ] {
            assert_eq!(
                named_by(url),
                Err(CloneRefusal::NoNameInUrl {
                    url: url.to_owned()
                }),
                "{url:?}"
            );
        }
    }

    /// A name is told from the path a Repository URL names its Repository
    /// by, and never from who may reach it: a URL carrying a credential
    /// gives the same name as one without, and one whose path says nothing
    /// gives none — never the credential or the host — so a name the Log
    /// may show carries nothing of either.
    #[test]
    fn a_clone_is_never_named_by_a_urls_credential_or_host() {
        for (url, name) in [
            (
                "https://octocat:SECRET@example.invalid/org/atlas.git",
                Some("atlas"),
            ),
            ("https://SECRET@example.invalid/atlas", Some("atlas")),
            (
                "ssh://git:SECRET@example.invalid:2222/srv/atlas.git/",
                Some("atlas"),
            ),
            (
                "http://user:SECRET@example.invalid:8080/atlas.git",
                Some("atlas"),
            ),
            ("https://octocat:SECRET@example.invalid", None),
            ("https://octocat:SECRET@example.invalid/", None),
            ("https://SECRET@example.invalid?SECRET", None),
            ("ssh://SECRET@example.invalid:2222", None),
            ("octocat:SECRET@example.invalid:atlas.git", None),
            ("SECRET@example.invalid:", None),
        ] {
            let named = named_by(url);
            assert_eq!(
                named,
                match name {
                    Some(name) => Ok(name),
                    None => Err(CloneRefusal::NoNameInUrl {
                        url: url.to_owned(),
                    }),
                },
                "{url}"
            );
            if let Ok(name) = named {
                assert!(
                    !name.contains("SECRET") && !name.contains("example"),
                    "{url}"
                );
            }
        }
    }

    #[test]
    fn a_name_is_one_plain_directory_name_on_every_platform() {
        for name in [
            "atlas",
            "atlas-2",
            "atlas.git",
            ".atlas",
            "the atlas",
            "console",
            "com10",
            "lpt",
            "nul-atlas",
            "CON-tools",
            "auxiliary.txt",
        ] {
            assert_eq!(plain_name(name), Ok(name));
        }
        for name in [
            "",
            "  ",
            ".",
            "..",
            "../atlas",
            "nested/atlas",
            r"nested\atlas",
            "atlas/",
            "C:atlas",
            "atlas\0",
            "atlas\ttab",
            "atlas?",
            "atlas*",
            "a<b",
            "a>b",
            "a|b",
            "\"atlas\"",
            "atlas.",
            "atlas ",
            "NUL",
            "nul",
            "CON.txt",
            "con .txt",
            "Aux.tar.gz",
            "PRN",
            "COM1",
            "com0",
            "LPT9.log",
            "COM\u{b9}",
            "lpt\u{b3}",
            "CONIN$",
            "conout$.txt",
            &rooted("atlas"),
        ] {
            assert_eq!(
                plain_name(name),
                Err(CloneRefusal::NotPlainName {
                    name: name.to_owned()
                }),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_root_is_implied_alone_named_among_several_and_refused_naming_none() {
        let home = PathBuf::from(rooted("home/ada"));
        let projects = rooted("work/projects");
        let roots = vec![projects.clone(), "~/src".to_owned()];
        let one = vec![projects.clone()];

        assert_eq!(chosen_root(&[], None, None), Err(CloneRefusal::NoRoot));
        assert_eq!(
            chosen_root(&[], Some(&projects), None),
            Err(CloneRefusal::NoRoot)
        );
        assert_eq!(chosen_root(&one, None, None), Ok(projects.as_str()));
        assert_eq!(
            chosen_root(&roots, None, Some(&home)),
            Err(CloneRefusal::RootUnnamed {
                roots: roots.clone()
            })
        );
        assert_eq!(
            chosen_root(&roots, Some("~/src"), Some(&home)),
            Ok("~/src"),
            "as the Setting spells it"
        );
        let beneath_home = home.join("src");
        assert_eq!(
            chosen_root(&roots, beneath_home.to_str(), Some(&home)),
            Ok("~/src"),
            "or as the directory it names"
        );
        let trailing = format!("{projects}{}", std::path::MAIN_SEPARATOR);
        assert_eq!(
            chosen_root(&roots, Some(&trailing), Some(&home)),
            Ok(projects.as_str())
        );
        for named in [rooted("work"), "projects".to_owned(), "~".to_owned()] {
            assert_eq!(
                chosen_root(&roots, Some(&named), Some(&home)),
                Err(CloneRefusal::UnknownRoot {
                    named: named.clone(),
                    roots: roots.clone(),
                }),
                "{named}"
            );
        }
        assert_eq!(
            chosen_root(&one, Some(&rooted("elsewhere")), None),
            Err(CloneRefusal::UnknownRoot {
                named: rooted("elsewhere"),
                roots: one.clone(),
            }),
            "a Root named is held to the Roots configured, though one alone would be implied"
        );
        assert_eq!(
            chosen_root(&roots, Some("~/src"), None),
            Ok("~/src"),
            "a Root is named by its spelling where no home is known"
        );
    }

    /// A Root is named by the directory it names however either spells it,
    /// where that directory can be read: the path a listing gives, read
    /// canonically, names a Root the Setting spells another way.
    #[test]
    fn a_root_is_named_by_the_directory_it_names_however_spelled() {
        let temporary = tempfile::tempdir().unwrap();
        let projects = temporary.path().join("projects");
        std::fs::create_dir(&projects).unwrap();
        let roundabout = temporary
            .path()
            .join("projects")
            .join("..")
            .join("projects");
        let roots = vec![roundabout.to_str().unwrap().to_owned(), "~/src".to_owned()];
        let read = crate::paths::canonical(&projects).unwrap();
        assert_eq!(
            chosen_root(&roots, read.to_str(), None),
            Ok(roots[0].as_str())
        );
        let missing = temporary.path().join("missing");
        assert_eq!(
            chosen_root(&roots, missing.to_str(), None),
            Err(CloneRefusal::UnknownRoot {
                named: missing.to_str().unwrap().to_owned(),
                roots: roots.clone(),
            })
        );
    }

    #[test]
    fn a_root_beginning_with_a_tilde_lies_beneath_the_home_or_cannot_be_read() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("src")).unwrap();
        assert_eq!(
            readable_root("~/src", Some(home.path())),
            Ok(crate::paths::canonical(home.path().join("src")).unwrap())
        );
        for (root, home) in [("~/src", None), ("~ada/src", Some(home.path()))] {
            assert_eq!(
                readable_root(root, home),
                Err(CloneRefusal::UnreadableRoot {
                    root: root.to_owned(),
                    reason: "it begins with ~ but names no home this server knows".to_owned(),
                })
            );
        }
        let missing = home.path().join("missing");
        let Err(CloneRefusal::UnreadableRoot { root, .. }) =
            readable_root(missing.to_str().unwrap(), None)
        else {
            panic!("a Root that is not there cannot be read");
        };
        assert_eq!(root, missing.to_str().unwrap());
        assert!(!missing.exists(), "and is never made");
    }

    /// A Root that does not answer — on a mount gone quiet, say — holds a
    /// Clone no longer than its deadline: placing it answers then, saying
    /// it timed out, and the refusal says the Root did not answer in time.
    /// While its read is still outstanding, another Clone into that Root
    /// answers so at once rather than reading it again, and another Root is
    /// read as ever.
    #[tokio::test]
    async fn a_root_that_does_not_answer_holds_a_clone_no_longer_than_its_deadline() {
        let reads = ReadsApart::new("suru-clone-root-read", 2);
        let (release, released) = std::sync::mpsc::channel::<()>();
        let quiet = move |root: &str, _home: Option<&Path>| {
            let _ = released.recv();
            Err(CloneRefusal::UnreadableRoot {
                root: root.to_owned(),
                reason: "released".to_owned(),
            })
        };
        let placing = placed(
            &reads,
            tokio::time::Instant::now() + Duration::from_millis(50),
            vec![rooted("unplugged")],
            None,
            "atlas".to_owned(),
            None,
            quiet,
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(30), placing)
                .await
                .expect("placing a Clone answers by its deadline")
                .is_none(),
            "and says it ran out of time"
        );
        assert_eq!(
            CloneRefusal::TimedOut {
                url: "https://example.com/atlas.git".to_owned(),
                destination: None,
                timeout: Duration::from_millis(50),
            }
            .to_string(),
            "The Clone of `https://example.com/atlas.git` did not finish within 50 milliseconds: \
             the Workspace Root it was to go in did not answer in time, so nothing was cloned."
        );

        let again = placed(
            &reads,
            tokio::time::Instant::now() + Duration::from_secs(60 * 60),
            vec![rooted("unplugged")],
            None,
            "charts".to_owned(),
            None,
            |_: &str, _: Option<&Path>| {
                panic!("a Root whose read is outstanding is not read again")
            },
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(30), again)
                .await
                .expect("a Clone into a Root still unread answers at once")
                .is_none()
        );

        let answering = placed(
            &reads,
            tokio::time::Instant::now() + Duration::from_secs(30),
            vec![rooted("projects")],
            None,
            "atlas".to_owned(),
            None,
            |root: &str, _home: Option<&Path>| Ok(PathBuf::from(root)),
        );
        assert_eq!(
            answering.await,
            Some(Ok((PathBuf::from(rooted("projects")).join("atlas"), false))),
            "a Root that answers places the Clone directly within it"
        );
        drop(release);
    }

    /// The Workspace a Clone just landed is read apart within a bound of its
    /// own: one read in time is entered as source control reads it, and one
    /// held up — on a mount gone quiet, say — or reading as no Repository
    /// there, is entered as the Clone knew it, its identity and where and how
    /// it is presented, so the Clone answers rather than hanging.
    #[tokio::test]
    async fn a_landed_clone_is_entered_as_read_in_time_or_as_the_clone_knew_it() {
        use crate::protocol::{
            Repository, RepositoryId, RepositoryLocation, ResolvedWorkspace,
            SourceControlAvailability, SourceControlCapabilities,
        };
        let reads = ReadsApart::new("suru-clone-root-read", 4);
        let destination = PathBuf::from(rooted("projects/atlas"));
        let repository = |capabilities| Repository {
            id: RepositoryId::from_metadata("git", &destination.join(".git")),
            system: "git".to_owned(),
            metadata_directory: destination.join(".git"),
            location: RepositoryLocation::Main {
                root: destination.clone(),
            },
            availability: SourceControlAvailability::Available,
            capabilities,
        };
        let known = crate::source_control::repository_workspace(&repository(
            SourceControlCapabilities::discovery_only(),
        ));
        let mut capable = SourceControlCapabilities::discovery_only();
        capable.create_checkout = SourceControlCapability::Available;
        let read = crate::source_control::repository_workspace(&repository(capable));

        let answered = read.clone();
        let reading = ResolvedWorkspace {
            workspace: answered,
            ..ResolvedWorkspace::directory(destination.clone())
        };
        assert_eq!(
            landed(
                &reads,
                &destination,
                known.clone(),
                Duration::from_secs(30),
                move || async move { reading },
            )
            .await,
            read,
            "a reading in time is entered as read"
        );

        let elsewhere = ResolvedWorkspace::directory(destination.clone());
        assert_eq!(
            landed(
                &reads,
                &destination,
                known.clone(),
                Duration::from_secs(30),
                move || async move { elsewhere },
            )
            .await,
            known,
            "a reading finding no Repository there leaves what the Clone knew"
        );

        let (release, released) = std::sync::mpsc::channel::<()>();
        let quiet = move || {
            let _ = released.recv();
            async move { ResolvedWorkspace::directory(PathBuf::new()) }
        };
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(30),
                landed(
                    &reads,
                    &destination,
                    known.clone(),
                    Duration::from_millis(50),
                    quiet,
                ),
            )
            .await
            .expect("a reading held up holds the Clone no longer than its bound"),
            known
        );
        drop(release);
    }

    #[test]
    fn a_timeout_is_spelled_in_its_plainest_whole_unit() {
        for (timeout, spelled_as) in [
            (Duration::from_secs(600), "10 minutes"),
            (Duration::from_secs(60), "1 minute"),
            (Duration::from_secs(90), "90 seconds"),
            (Duration::from_secs(1), "1 second"),
            (Duration::from_millis(500), "500 milliseconds"),
            (Duration::from_millis(1), "1 millisecond"),
            (Duration::ZERO, "0 milliseconds"),
        ] {
            assert_eq!(spelled(timeout), spelled_as);
        }
    }
}
