//! Cloning a Repository URL into a Workspace Root: the act a Sidekick's
//! `clone_workspace` performs (ADR 0051). A Clone goes directly within the
//! Workspace Root it names — as the Setting naming the Roots spells it, or
//! as the directory it names — or the only one configured, and nowhere else:
//! under the name it gives, which must be one plain directory name, or else
//! the one its Repository URL ends in — never a hidden one, beginning with a
//! dot, which discovery passes over. A Root is never made for it, so one
//! that cannot be read refuses it, and whatever already stands where it
//! would go refuses it too, named as the Workspace it is where it is one.
//! Nor does any Clone take the place the Sidekick Workspace is kept in,
//! which stays outside source control, however the case of its name is
//! spelled — a filesystem that does not tell case apart takes any spelling
//! for that very directory, made or yet to be — nor the place a symlink its
//! user relocated it by leads to before anything is there; judged as the
//! Root is read, within the Clone's deadline. Nor does a Clone go in a Root
//! naming Suru's own data directory, where the Sidekick Workspace is kept,
//! however the Root is spelled: told apart as every Root is told from a
//! directory, it is passed over as no Root a Clone may go in, and refused
//! where named or where it is the only one — and so is a Root read as that
//! directory only once the Clone reads it.
//!
//! The Clone itself is source control's ([`SourceControlService::clone_repository`]),
//! bounded by this Server's own timeout, and leaves nothing behind where it
//! fails or runs out of time. One whose clone is whole is settled on a task
//! of its own once it lands, whether or not whoever asked for it is still
//! waiting — as where the bound passed while the clone was still taking its
//! name — and counted among the Clones run apart, so a stopping Server waits
//! on it: it is read apart, within a bound of its own, and entered in the
//! Workspace Registry as Cloned — as read, or as the Clone knew it where the
//! reading does not answer in time, and no scan of the Roots happening upon
//! it first takes it for discovered — has its Icon and Description derived
//! at once, as the first Session begun there would, and has the Roots
//! discovered afresh, waiting on that no longer than a listing would; the
//! Log records what was Cloned where, the URL's credential aside.
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
//! Remote is given as long to answer as one may take. The Remote sees such
//! a Clone through on a task of its own, whatever becomes of the Peer's
//! request, until it begins to stop: that stops the Clone as an abandoned
//! one is stopped, and the stop waits a while for it to let go of what it
//! began, and for a clone already whole to land and be settled, before the
//! Remote lets go.
//!
//! [`SourceControlService::clone_repository`]: crate::source_control::SourceControlService::clone_repository

use std::{
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::http::Method;
use tokio::sync::{oneshot, watch};

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
        root_directories,
        root_discovery::{self, expand},
    },
    sidekick::SidekickWorkspace,
    source_control::{CloneFailure, CloneLanding, without_credentials},
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

/// What a Clone takes that is this Server's own: how its Workspace Roots
/// are read, how long a Clone may run, and the Clones run on tasks of their
/// own.
#[derive(Clone)]
pub(crate) struct WorkspaceCloning {
    roots: CloneRoots,
    timeout: Duration,
    detached: DetachedClones,
}

impl WorkspaceCloning {
    pub(crate) fn new(roots: CloneRoots, timeout: Duration, detached: DetachedClones) -> Self {
        Self {
            roots,
            timeout,
            detached,
        }
    }
}

/// How the Workspace Roots a Clone may go in are read: the home a Root
/// beginning with `~` lies beneath; Suru's own data directory, read
/// canonically, where the Sidekick Workspace is kept and which is never a
/// Root a Clone may go in; the reads apart every Root is read by, so one on
/// a mount gone quiet holds up none of the Server's own threads; and the
/// longest the Roots are waited on to be told apart.
#[derive(Clone)]
pub(crate) struct CloneRoots {
    home: Option<PathBuf>,
    data_root: Option<PathBuf>,
    reads: Arc<ReadsApart>,
    wait: Duration,
}

impl CloneRoots {
    pub(crate) fn new(home: Option<PathBuf>, data_root: Option<PathBuf>, wait: Duration) -> Self {
        Self {
            home,
            data_root,
            reads: Arc::new(ReadsApart::new("suru-clone-root-read", ROOT_READS_AT_ONCE)),
            wait,
        }
    }

    /// The Roots among `roots` a Clone may go in — every one but those
    /// naming Suru's own data directory — as a Sidekick is told them.
    pub(crate) async fn open_to_clones(&self, roots: Vec<String>) -> Vec<String> {
        self.told(roots, self.wait).await.0
    }

    /// `roots`, each in the Setting's order, told apart as those a Clone may
    /// go in and those naming Suru's own data directory, as every Root is
    /// told from a directory (see [`root_directories`]): written as it, or
    /// resolving to it where reading the Root answers within `wait`.
    async fn told(&self, roots: Vec<String>, wait: Duration) -> (Vec<String>, Vec<String>) {
        let Some(data_root) = &self.data_root else {
            return (roots, Vec::new());
        };
        let named =
            root_directories::resolve(&self.reads, self.home.as_deref(), wait, roots.iter()).await;
        roots
            .into_iter()
            .partition(|root| !named[root].names(data_root))
    }
}

/// The work of Clones run on tasks of its own, counted while it runs, so a
/// stopping Server can wait until each piece has let go of what it began:
/// the Clones Sidekicks on Peers ask of this Server, seen through however
/// their Peer fares until this Server begins to stop, which stops every one;
/// and the settling of every Clone that lands, which goes on however whoever
/// asked for it fares, and which a stop cuts short only of discovering the
/// Roots afresh.
#[derive(Clone)]
pub(crate) struct DetachedClones {
    /// Whether this Server has begun to stop.
    stopping: watch::Receiver<bool>,
    running: Arc<watch::Sender<usize>>,
}

impl DetachedClones {
    /// Clones run apart until `stopping` says this Server has begun to stop,
    /// and settled apart.
    pub(crate) fn new(stopping: watch::Receiver<bool>) -> Self {
        Self {
            stopping,
            running: Arc::new(watch::channel(0).0),
        }
    }

    /// Waits until none is running: what a stopping Server waits on, once
    /// its stop has stopped every Clone a Peer asked for, before it waits on
    /// what they left to see through (see
    /// [`SourceControlService::settle_clones`]).
    ///
    /// [`SourceControlService::settle_clones`]: crate::source_control::SourceControlService::settle_clones
    pub(crate) async fn stopped(&self) {
        let _ = self
            .running
            .subscribe()
            .wait_for(|running| *running == 0)
            .await;
    }

    /// One more running, until the answer is dropped.
    fn begin(&self) -> RunningClone {
        self.running.send_modify(|running| *running += 1);
        RunningClone(self.running.clone())
    }
}

/// One Clone, or one Clone's settling, running apart.
struct RunningClone(Arc<watch::Sender<usize>>);

impl Drop for RunningClone {
    fn drop(&mut self) {
        self.0.send_modify(|running| *running -= 1);
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
    /// The Root the Clone would go in, `root` as the Setting or the Clone
    /// spells it, names Suru's own data directory, where the Sidekick
    /// Workspace is kept, which is never a Root a Clone may go in.
    DataRoot { root: String },
    /// The name given is not one plain directory name.
    NotPlainName { name: String },
    /// The name given — or, where `url` is named, the one the Repository
    /// URL ends in — begins with a dot, as a hidden directory's does, which
    /// discovery passes over and a Clone makes its own out of sight under.
    HiddenName { name: String, url: Option<String> },
    /// No name was given, and the Repository URL ends in none.
    NoNameInUrl { url: String },
    /// Something already stands at the destination: the Workspace named
    /// there, where it is one.
    Occupied {
        destination: PathBuf,
        workspace: Option<WorkspaceId>,
    },
    /// The destination is where the Sidekick Workspace is kept, or would be
    /// made, however the case of its name is spelled: a directory outside
    /// source control, which no Clone may take.
    SidekickWorkspacePlace { destination: PathBuf },
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
    /// then entered as Cloned and settled as any Clone that lands.
    StillLanding {
        url: String,
        destination: PathBuf,
        timeout: Duration,
    },
    /// This Server began to stop before the Clone was done, and stopped it.
    Stopped { url: String },
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
            Self::DataRoot { root } => write!(
                formatter,
                "`{root}` names Suru's own data directory, where the Sidekick Workspace is kept, \
                 which is not a Root a Clone may go in, so nothing was cloned; Clone into another \
                 of this server's Workspace Roots, or ask the user to name a directory of their \
                 own in the workspace.roots Setting."
            ),
            Self::NotPlainName { name } => write!(
                formatter,
                "`{name}` is not one plain directory name, so nothing was cloned; give `name` as \
                 the name of one directory to make within the Workspace Root, with no path in it."
            ),
            Self::HiddenName { name, url: None } => write!(
                formatter,
                "`{name}` begins with a dot, and a Workspace's name may not be hidden, so nothing \
                 was cloned; give `name` as one that does not begin with a dot."
            ),
            Self::HiddenName {
                name,
                url: Some(url),
            } => write!(
                formatter,
                "The Repository URL `{url}` ends in `{name}`, which begins with a dot, and a \
                 Workspace's name may not be hidden, so nothing was cloned; give one that does \
                 not begin with a dot as `name`."
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
            Self::SidekickWorkspacePlace { destination } => write!(
                formatter,
                "`{}` is where the Sidekick Workspace is kept, in whatever case its name is \
                 spelled, and no Clone may take its place, so nothing was cloned; give another \
                 `name`.",
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
                 server's Workspaces as Cloned, its Icon and Description derived; if it cannot, \
                 nothing will be left of it.",
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
            Self::Stopped { url } => write!(
                formatter,
                "This server began to stop before the Clone of `{url}` was done, so it was \
                 stopped and what it had begun is removed; a clone already whole may land all \
                 the same, and is then listed among this server's Workspaces once it is back."
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
            Self::Stopped { .. } => Some("was stopped as the server stopped"),
            Self::Occupied { .. } => Some("found its destination occupied"),
            Self::NoRoot
            | Self::RootUnnamed { .. }
            | Self::UnknownRoot { .. }
            | Self::UnreadableRoot { .. }
            | Self::DataRoot { .. }
            | Self::NotPlainName { .. }
            | Self::HiddenName { .. }
            | Self::NoNameInUrl { .. }
            | Self::SidekickWorkspacePlace { .. }
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

/// Refuses a Clone to `destination` where the Sidekick Workspace is kept, or
/// would be made, whoever asks for it, since it is to stay outside source
/// control; and, for a Sidekick on a Peer, `from_a_peer`, anywhere within
/// it, which a Remote keeps to itself. Reads the filesystem, so it is run
/// apart, within the Clone's deadline, as the Root is read.
fn kept_from_sidekick_workspace(
    sidekick_workspace: &SidekickWorkspace,
    destination: &Path,
    from_a_peer: bool,
) -> Result<(), CloneRefusal> {
    if sidekick_workspace.is_kept_at(destination) {
        return Err(CloneRefusal::SidekickWorkspacePlace {
            destination: destination.to_owned(),
        });
    }
    if from_a_peer && sidekick_workspace.encloses(destination) {
        return Err(CloneRefusal::SidekickWorkspace {
            destination: destination.to_owned(),
        });
    }
    Ok(())
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

/// Logs `refusal` of the Clone of `url` to `destination` for `author`,
/// where it came of running the Clone, saying nothing of the URL's
/// credential or Git's words.
fn logged(refusal: &CloneRefusal, url: &str, destination: &Path, author: &Author) {
    if let Some(outcome) = refusal.outcome() {
        tracing::info!(
            repository_url = %without_credentials(url),
            destination = %destination.display(),
            sidekick = %Cloner(author),
            "a Clone of a Repository URL into a Workspace Root {outcome}"
        );
    }
}

/// A Clone whose clone is whole and naming at `destination`: what settling
/// it once it lands needs of it, and its place among the Clones run apart,
/// held from before it ran until it is settled.
struct Landed {
    url: String,
    destination: PathBuf,
    author: Author,
    counted: RunningClone,
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

/// `name`, where it may name the Workspace a Clone makes: one plain
/// directory name (see [`plain_name`]) that does not begin with a dot. A
/// hidden directory is one discovery passes over, so a Workspace there would
/// never be listed as discovered; and a Clone makes its own out of sight
/// under such a name, which no Workspace it makes may take.
fn workspace_name(name: &str) -> Result<&str, CloneRefusal> {
    let name = plain_name(name)?;
    if name.starts_with('.') {
        return Err(CloneRefusal::HiddenName {
            name: name.to_owned(),
            url: None,
        });
    }
    Ok(name)
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
/// anything of its user, password or host — where that may name a
/// Workspace (see [`workspace_name`]).
fn named_by(url: &str) -> Result<&str, CloneRefusal> {
    let path = repository_path(url);
    let trimmed = path.trim_end_matches(['/', '\\']);
    let trimmed = trimmed
        .strip_suffix("/.git")
        .or_else(|| trimmed.strip_suffix("\\.git"))
        .unwrap_or(trimmed);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    workspace_name(last.strip_suffix(".git").unwrap_or(last)).map_err(|refused| match refused {
        CloneRefusal::HiddenName { name, .. } => CloneRefusal::HiddenName {
            name,
            url: Some(url.to_owned()),
        },
        _ => CloneRefusal::NoNameInUrl {
            url: url.to_owned(),
        },
    })
}

/// Where a Clone is asked to go: the Workspace Roots configured that a
/// Clone may go in, those naming Suru's own data directory, which none may,
/// the one it names where it names one, the directory it makes within it,
/// the home a Root beginning with `~` lies beneath, and the data directory,
/// read canonically.
struct Placement {
    roots: Vec<String>,
    data_roots: Vec<String>,
    named: Option<String>,
    name: String,
    home: Option<PathBuf>,
    data_root: Option<PathBuf>,
}

/// Where the Clone `placement` asks for goes — directly within the Root it
/// names, or the only one, never one naming Suru's own data directory,
/// however it is spelled or comes to be read — and whether something stands
/// there already,
/// the Root read by `read` apart (see [`ReadsApart`]), so a Root on a mount
/// gone quiet holds up none of the Server's own threads, and holds one of
/// its own however often it is Cloned into again; the destination is judged
/// by `judged` in the same read apart, so what it reads of the filesystem
/// is bound by the same deadline. `None` where the reading has not answered
/// by `deadline`, or one begun earlier still has not.
async fn placed(
    reads: &ReadsApart,
    deadline: tokio::time::Instant,
    placement: Placement,
    read: impl FnOnce(&str, Option<&Path>) -> Result<PathBuf, CloneRefusal> + Send + 'static,
    judged: impl FnOnce(&Path) -> Result<(), CloneRefusal> + Send + 'static,
) -> Option<Result<(PathBuf, bool), CloneRefusal>> {
    let Placement {
        roots,
        data_roots,
        named,
        name,
        home,
        data_root,
    } = placement;
    // No Root, or several and none named, is refused without reading one.
    let asked = match (&named, roots.as_slice(), data_roots.as_slice()) {
        (Some(named), _, _) => named.clone(),
        (None, [only], _) => only.clone(),
        (None, [], [data_root, ..]) => {
            return Some(Err(CloneRefusal::DataRoot {
                root: data_root.clone(),
            }));
        }
        (None, _, _) => {
            return Some(chosen_root(&roots, None, home.as_deref()).map(|_| unreachable!()));
        }
    };
    let placing = reads.read(Path::new(&asked), move || async move {
        if let Some(named) = named.as_deref()
            && chosen_root(&data_roots, Some(named), home.as_deref()).is_ok()
        {
            return Err(CloneRefusal::DataRoot {
                root: named.to_owned(),
            });
        }
        let root = chosen_root(&roots, named.as_deref(), home.as_deref())?;
        let directory = read(root, home.as_deref())?;
        // A Root not told from the data directory in time is told now.
        if data_root.as_ref() == Some(&directory) {
            return Err(CloneRefusal::DataRoot {
                root: root.to_owned(),
            });
        }
        let destination = directory.join(name);
        judged(&destination)?;
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
    /// Workspace made there as this Server's Workspace listing gives it,
    /// once the Clone is settled (see [`Self::settle_clone`]). Refuses,
    /// saying why and having made nothing, where no Root is there to take
    /// the Clone, where its Root cannot be read, where its name is no plain
    /// directory name or a hidden one, where it would take the Sidekick
    /// Workspace's place — or, for a Sidekick on a Peer, go anywhere within
    /// it — where something stands at its destination already, and where Git
    /// fails it or it outlasts this Server's bound on one. One whose clone is
    /// whole but still taking its name when that bound passes is refused
    /// then, saying it may yet land, and is settled all the same once it
    /// does.
    pub(crate) async fn clone_workspace(
        &self,
        request: &CloneWorkspaceRequest,
        author: &Author,
    ) -> Result<Workspace, CloneRefusal> {
        // One bound for the whole of it: reading the Root, cloning, naming
        // the clone, and removing what a Clone that failed made.
        let deadline = tokio::time::Instant::now() + self.cloning.timeout;
        let url = request.repository_url.as_str();
        let configured = self.settings.borrow().settings.workspace.roots.clone();
        let name = match &request.name {
            Some(name) => workspace_name(name)?,
            None => named_by(url)?,
        }
        .to_owned();
        let sidekick_workspace = self.sidekick_workspace.clone();
        let from_a_peer = matches!(author, Author::PeerSidekick { .. });
        let telling = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(self.cloning.roots.wait);
        let (roots, data_roots) = self.cloning.roots.told(configured, telling).await;
        let Some(placed) = placed(
            &self.cloning.roots.reads,
            deadline,
            Placement {
                roots,
                data_roots,
                named: request.root.clone(),
                name,
                home: self.cloning.roots.home.clone(),
                data_root: self.cloning.roots.data_root.clone(),
            },
            readable_root,
            move |destination| {
                kept_from_sidekick_workspace(&sidekick_workspace, destination, from_a_peer)
            },
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
        if occupied {
            return Err(self.occupied(destination, deadline).await);
        }
        if let SourceControlCapability::Unsupported { reason } =
            self.source_control.clone_capability()
        {
            return Err(CloneRefusal::Unsupported { reason });
        }
        // No scan enters the clone as discovered between its taking its name
        // and its settling entering it as Cloned; and the Clone is counted
        // among those run apart from before it runs, so a stopping Server
        // never finds nothing under way between the clone's landing and its
        // settling.
        let held = self.root_discovery.landing(&destination);
        let counted = self.cloning.detached.begin();
        let cloned = self
            .source_control
            .clone_repository(
                url,
                &destination,
                request.branch.as_deref(),
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await;
        let landing = match cloned {
            Ok(landing) => landing,
            Err(failure) => {
                return Err(self
                    .clone_failed(failure, url, &destination, author, deadline)
                    .await);
            }
        };
        let (landed, landed_in_time) = oneshot::channel();
        let settling = self.settle_clone(
            landing,
            landed,
            held,
            Landed {
                url: url.to_owned(),
                destination: destination.clone(),
                author: author.clone(),
                counted,
            },
        );
        // Landed, or not to land: the settling answers which. Still landing
        // at the deadline: refused now, and settled all the same.
        if tokio::time::timeout_at(deadline, landed_in_time)
            .await
            .is_err()
        {
            let refusal = CloneRefusal::StillLanding {
                url: url.to_owned(),
                destination: destination.clone(),
                timeout: self.cloning.timeout,
            };
            logged(&refusal, url, &destination, author);
            return Err(refusal);
        }
        let settled = match settling.await {
            Ok(settled) => settled,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only a runtime going away cancels it.
            Err(_) => {
                return Err(CloneRefusal::Stopped {
                    url: url.to_owned(),
                });
            }
        };
        match settled {
            Ok(workspace) => Ok(self
                .registered_workspaces()
                .into_iter()
                .find(|listed| listed.id == workspace.id)
                .unwrap_or(workspace)),
            Err(failure) => Err(self
                .clone_failed(failure, url, &destination, author, deadline)
                .await),
        }
    }

    /// Settles the Clone whose whole clone `landing` is naming at its
    /// destination, as `clone` describes it, on a task of its own, so it is
    /// settled whenever the clone lands, whether or not whoever asked for
    /// the Clone is still waiting: counted, as `clone` is, among the Clones
    /// run apart, so a stopping Server waits on it a while before it lets go. Once the clone
    /// has landed — said on `landed` — its Workspace is read apart, within a
    /// bound of its own, and entered in the Workspace Registry as Cloned —
    /// as read, or as the Clone knew it where the reading does not answer in
    /// time — and `held`, which kept every scan of the Roots from taking it
    /// for discovered meanwhile, is let go; the Errand deriving its Icon and
    /// Description is begun, as the first Session begun there would begin
    /// it; the Roots are discovered afresh, unless this Server has begun to
    /// stop; and the Log records what was Cloned where, the URL's credential
    /// aside. Answers the Workspace entered, or why the clone could not land.
    fn settle_clone(
        &self,
        landing: CloneLanding,
        landed: oneshot::Sender<()>,
        held: root_discovery::Landing,
        clone: Landed,
    ) -> tokio::task::JoinHandle<Result<Workspace, CloneFailure>> {
        let operations = self.clone();
        tokio::spawn(async move {
            let Landed {
                url,
                destination,
                author,
                counted: _counted,
            } = clone;
            let known = match landing.await {
                Ok(known) => known,
                Err(failure) => {
                    if landed.is_closed() {
                        tracing::info!(
                            repository_url = %without_credentials(&url),
                            destination = %destination.display(),
                            sidekick = %Cloner(&author),
                            "a Clone of a Repository URL into a Workspace Root that was still \
                             landing when its time ran out could not land"
                        );
                    }
                    return Err(failure);
                }
            };
            let _ = landed.send(());
            let source_control = operations.source_control.clone();
            let reading = destination.clone();
            let workspace = self::landed(
                &operations.cloning.roots.reads,
                &destination,
                known,
                LANDED_READ_WITHIN,
                move || async move { source_control.read_unremembered(&reading).await },
            )
            .await;
            let workspace = operations.sessions.enter_cloned(&workspace, &destination);
            drop(held);
            // A Sidekick on a Peer has no Session here: its Clone's Errand
            // runs at the Agent a Session it began here naming none would
            // run at.
            let provider = match author.sidekick_session() {
                Some(sidekick) => operations.sessions.provider(sidekick),
                None => operations
                    .landing_selection()
                    .map(|selection| selection.provider),
            };
            operations.derivation.derive_cloned_workspace(
                Cloner(&author),
                provider,
                &workspace,
                destination.clone(),
            );
            let mut stopping = operations.cloning.detached.stopping.clone();
            tokio::select! {
                biased;
                () = crate::provider::wait_for_shutdown(&mut stopping) => {}
                () = operations.rediscover_after_clone() => {}
            }
            tracing::info!(
                repository_url = %without_credentials(&url),
                destination = %destination.display(),
                workspace_id = %workspace.id.0,
                sidekick = %Cloner(&author),
                "a Sidekick Cloned a Repository URL into a Workspace Root"
            );
            Ok(workspace)
        })
    }

    /// The refusal of the Clone of `url` to `destination` for `author` that
    /// failed as `failure`, logged where it came of running the Clone. What
    /// stands at the destination, where something does, is read no later
    /// than `deadline`.
    async fn clone_failed(
        &self,
        failure: CloneFailure,
        url: &str,
        destination: &Path,
        author: &Author,
        deadline: tokio::time::Instant,
    ) -> CloneRefusal {
        let refusal = match failure {
            CloneFailure::Occupied => self.occupied(destination.to_owned(), deadline).await,
            CloneFailure::Failed(said) => CloneRefusal::Failed {
                url: url.to_owned(),
                destination: destination.to_owned(),
                said,
            },
            CloneFailure::LeftBehind(said) => CloneRefusal::LeftBehind {
                url: url.to_owned(),
                destination: destination.to_owned(),
                said,
            },
            CloneFailure::TimedOut => CloneRefusal::TimedOut {
                url: url.to_owned(),
                destination: Some(destination.to_owned()),
                timeout: self.cloning.timeout,
            },
        };
        logged(&refusal, url, destination, author);
        refusal
    }

    /// Clones as [`Self::clone_workspace`] does, for the Sidekick on a Peer
    /// `author` names, on a task of its own: the Clone is seen through
    /// however the Peer fares — one that stops listening loses only the
    /// answer — until this Server begins to stop. That stops it as an
    /// abandoned Clone is stopped: Git ends, and what it began is removed on
    /// its own, among what every Clone leaves to see through, which the
    /// stopping Server waits on; a clone already whole may land all the
    /// same, and is settled as every Clone that lands is. A Clone asked for
    /// once the stop has begun is stopped before it begins.
    pub(crate) async fn clone_workspace_detached(
        &self,
        request: CloneWorkspaceRequest,
        author: Author,
    ) -> Result<Workspace, CloneRefusal> {
        let url = request.repository_url.clone();
        let running = self.cloning.detached.begin();
        let mut stopping = self.cloning.detached.stopping.clone();
        let operations = self.clone();
        let cloning = tokio::spawn(async move {
            // Let go only once the Clone is dropped, and with it whatever it
            // left to be removed is counted among what Clones see through.
            let _running = running;
            tokio::select! {
                biased;
                () = crate::provider::wait_for_shutdown(&mut stopping) => {
                    tracing::info!(
                        repository_url = %without_credentials(&request.repository_url),
                        sidekick = %Cloner(&author),
                        "a Clone of a Repository URL into a Workspace Root was stopped as the \
                         server stopped"
                    );
                    Err(CloneRefusal::Stopped {
                        url: request.repository_url.clone(),
                    })
                }
                cloned = operations.clone_workspace(&request, &author) => cloned,
            }
        });
        match cloning.await {
            Ok(answer) => answer,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only a runtime going away cancels it.
            Err(_) => Err(CloneRefusal::Stopped { url }),
        }
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
                    &self.cloning.roots.reads,
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
    /// no longer than the bound on waiting for discovery. What the scan
    /// finds there leaves the Clone's own entry known by its Clone, as an
    /// entry already made always is.
    async fn rediscover_after_clone(&self) {
        let scans = self.root_discovery.rediscover();
        self.root_discovery.settled(scans).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Clone named `name` asked to go within the only Workspace Root,
    /// `root`, rooted as this platform roots paths.
    fn placement(root: &str, name: &str) -> Placement {
        Placement {
            roots: vec![rooted(root)],
            data_roots: Vec::new(),
            named: None,
            name: name.to_owned(),
            home: None,
            data_root: None,
        }
    }

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

    /// A Workspace a Clone makes may not be hidden, whether it is named so
    /// or its Repository URL ends so: discovery passes over a hidden
    /// directory, and a Clone makes its own out of sight under such a name
    /// — one spelled as a Clone's staging is spelled among them — which no
    /// Workspace may take. Every other plain name is one a Workspace may
    /// have.
    #[test]
    fn a_workspace_a_clone_makes_may_not_be_hidden() {
        for name in [".atlas", ".atlas.suru-clone-deadbeef", ".git", "..atlas"] {
            assert_eq!(
                workspace_name(name),
                Err(CloneRefusal::HiddenName {
                    name: name.to_owned(),
                    url: None,
                }),
                "{name:?}"
            );
        }
        for name in ["atlas", "atlas.suru-clone-deadbeef", "atlas.git", "a.b"] {
            assert_eq!(workspace_name(name), Ok(name));
        }
        assert_eq!(
            workspace_name(".."),
            Err(CloneRefusal::NotPlainName {
                name: "..".to_owned()
            }),
            "a name that is no plain directory name is refused as that first"
        );

        for (url, name) in [
            ("https://github.com/org/.atlas.git", ".atlas"),
            (
                "https://github.com/org/.atlas.suru-clone-deadbeef",
                ".atlas.suru-clone-deadbeef",
            ),
            (
                "git@github.com:org/.atlas.suru-clone-deadbeef.git",
                ".atlas.suru-clone-deadbeef",
            ),
            ("/srv/git/.hidden/", ".hidden"),
        ] {
            assert_eq!(
                named_by(url),
                Err(CloneRefusal::HiddenName {
                    name: name.to_owned(),
                    url: Some(url.to_owned()),
                }),
                "{url}"
            );
        }
        assert_eq!(
            CloneRefusal::HiddenName {
                name: ".atlas.suru-clone-deadbeef".to_owned(),
                url: None,
            }
            .to_string(),
            "`.atlas.suru-clone-deadbeef` begins with a dot, and a Workspace's name may not be \
             hidden, so nothing was cloned; give `name` as one that does not begin with a dot."
        );
        assert_eq!(
            CloneRefusal::HiddenName {
                name: ".atlas".to_owned(),
                url: Some("https://github.com/org/.atlas.git".to_owned()),
            }
            .to_string(),
            "The Repository URL `https://github.com/org/.atlas.git` ends in `.atlas`, which \
             begins with a dot, and a Workspace's name may not be hidden, so nothing was cloned; \
             give one that does not begin with a dot as `name`."
        );
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
            placement("unplugged", "atlas"),
            quiet,
            |_: &Path| Ok(()),
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
            placement("unplugged", "charts"),
            |_: &str, _: Option<&Path>| {
                panic!("a Root whose read is outstanding is not read again")
            },
            |_: &Path| panic!("a Root whose read is outstanding is not read again"),
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
            placement("projects", "atlas"),
            |root: &str, _home: Option<&Path>| Ok(PathBuf::from(root)),
            |_: &Path| Ok(()),
        );
        assert_eq!(
            answering.await,
            Some(Ok((PathBuf::from(rooted("projects")).join("atlas"), false))),
            "a Root that answers places the Clone directly within it"
        );
        drop(release);
    }

    /// What judging a destination reads of the filesystem — where the
    /// Sidekick Workspace is, say, behind a symlink into a mount gone quiet —
    /// is read apart with the Root, so it holds a Clone no longer than its
    /// deadline and holds up none of the Server's own threads; and what it
    /// refuses is the Clone's answer.
    #[tokio::test]
    async fn judging_a_destination_holds_a_clone_no_longer_than_its_deadline() {
        let reads = ReadsApart::new("suru-clone-root-read", 2);
        let (release, released) = std::sync::mpsc::channel::<()>();
        let placing = placed(
            &reads,
            tokio::time::Instant::now() + Duration::from_millis(50),
            placement("projects", "atlas"),
            |root: &str, _home: Option<&Path>| Ok(PathBuf::from(root)),
            move |_: &Path| {
                let _ = released.recv();
                Ok(())
            },
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(30), placing)
                .await
                .expect("placing a Clone answers by its deadline")
                .is_none(),
            "and says it ran out of time"
        );
        drop(release);

        let archive = PathBuf::from(rooted("archive"));
        assert_eq!(
            placed(
                &reads,
                tokio::time::Instant::now() + Duration::from_secs(30),
                placement("archive", "sidekick"),
                |root: &str, _home: Option<&Path>| Ok(PathBuf::from(root)),
                |destination: &Path| {
                    Err(CloneRefusal::SidekickWorkspacePlace {
                        destination: destination.to_owned(),
                    })
                },
            )
            .await,
            Some(Err(CloneRefusal::SidekickWorkspacePlace {
                destination: archive.join("sidekick"),
            }))
        );
    }

    /// No Clone goes in a Root naming Suru's own data directory, where the
    /// Sidekick Workspace is kept: one told apart as naming it is refused
    /// when named, and where it is the only Root configured, and passed over
    /// otherwise, so the only other Root takes a Clone naming none; and one
    /// not told apart in time is refused once it is read as that directory.
    #[tokio::test]
    async fn no_clone_goes_in_a_root_naming_suru_s_own_data_directory() {
        let reads = ReadsApart::new("suru-clone-root-read", 4);
        let (data, projects) = (rooted("suru/data"), rooted("projects"));
        let data_root = PathBuf::from(&data);
        let deadline = || tokio::time::Instant::now() + Duration::from_secs(30);
        let placing =
            |roots: &[&String], data_roots: &[&String], named: Option<&String>| Placement {
                roots: roots.iter().map(|root| (*root).clone()).collect(),
                data_roots: data_roots.iter().map(|root| (*root).clone()).collect(),
                named: named.cloned(),
                name: "atlas".to_owned(),
                home: None,
                data_root: Some(data_root.clone()),
            };
        let read = |root: &str, _home: Option<&Path>| Ok(PathBuf::from(root));
        let refused = |root: &String| Some(Err(CloneRefusal::DataRoot { root: root.clone() }));

        assert_eq!(
            placed(
                &reads,
                deadline(),
                placing(&[], &[&data], None),
                read,
                |_: &Path| Ok(())
            )
            .await,
            refused(&data),
            "the only Root configured takes no Clone"
        );
        assert_eq!(
            placed(
                &reads,
                deadline(),
                placing(&[&projects], &[&data], Some(&data)),
                read,
                |_: &Path| Ok(())
            )
            .await,
            refused(&data),
            "nor one naming it"
        );
        assert_eq!(
            placed(
                &reads,
                deadline(),
                placing(&[&projects], &[&data], None),
                read,
                |_: &Path| Ok(())
            )
            .await,
            Some(Ok((PathBuf::from(&projects).join("atlas"), false))),
            "and the only other Root takes a Clone naming none"
        );
        let quiet = rooted("link-to-data");
        assert_eq!(
            placed(
                &reads,
                deadline(),
                placing(&[&quiet], &[], None),
                move |_: &str, _: Option<&Path>| Ok(PathBuf::from(rooted("suru/data"))),
                |_: &Path| Ok(())
            )
            .await,
            refused(&quiet),
            "a Root read as that directory only once the Clone reads it is refused then"
        );
        assert_eq!(
            CloneRefusal::DataRoot {
                root: "~/.local/share/suru".to_owned(),
            }
            .to_string(),
            "`~/.local/share/suru` names Suru's own data directory, where the Sidekick Workspace \
             is kept, which is not a Root a Clone may go in, so nothing was cloned; Clone into \
             another of this server's Workspace Roots, or ask the user to name a directory of \
             their own in the workspace.roots Setting."
        );
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
