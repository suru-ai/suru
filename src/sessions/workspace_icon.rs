//! Deriving a Workspace's Icon and Description from its presented name and,
//! where its main root is known, the opening of its README.
//!
//! The Errand this builds is the second of the pair [`super::title::Derivation`]
//! runs on one Session's creation task, asked only where that Session's
//! Workspace still lacks an Icon or a Description. One schema asks for both,
//! because both stand for the same thing — what the work is and what it is
//! for — and a reader tells Workspaces apart by both. Its prompt carries the
//! same presentation a reader already sees for that Workspace — the
//! Sidebar's own leaf-of-path name — and, for a Workspace whose main root
//! Suru has found, the opening of whichever README stands there: a project's
//! README is usually the shortest path to what the work is and what it is
//! for. A Bare Workspace, one whose main root is not yet known, and one with
//! no Repository at all send the name alone.
//!
//! Each half of the reply commits on its own and only into its own absence:
//! the Icon through [`SessionStore::commit_workspace_icon`], the Description
//! through [`SessionStore::commit_workspace_description`] — the same
//! fill-an-absence rule the Session's own derived Icon follows (see
//! [`super::title`]). Neither waits on the other, so a Workspace that already
//! carries an Icon still gains a Description from the same Errand, and a half
//! the reply gets wrong leaves the other to land alone. A race between two
//! Sessions created in the same Workspace before either Errand answers
//! resolves to whichever answers first, and the other's reply lands on
//! absences that are no longer there and is discarded. Failure records
//! nothing at all, so the next Session created in that Workspace tries again;
//! the Errand is asked for as long as either is still absent.
//!
//! A user may also choose a Workspace's Icon from the Icon Picker, through
//! [`SessionStore::set_workspace_icon`]. Unlike a derivation, a choice always
//! stands: it replaces whatever the Workspace already carried, and once
//! landed it stands against every later derivation the same way a chosen
//! Session Icon stands against one (see [`super::title::SessionStore::set_icon`]).
//! Both ways an Icon can land share the write, regroup, and publish this
//! module does once, in [`SessionStore::land_workspace_icon`]; only the guard
//! at the top differs. A Description is set the same way, through
//! [`super::workspace_description`].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    icon_catalog,
    protocol::{RepositoryLocation, SessionCatalogChange, Workspace, WorkspaceId},
    storage::StoredWorkspace,
};

use super::{SessionStore, SessionStoreState, workspace_description};

/// The most of a README this carries into an Errand's prompt. Short for the
/// same reason a first Prompt's opening is: naming an Icon should not cost a
/// full-context call, and a README states what a project is before it trails
/// into detail.
const MAX_README_CHARS: usize = 2_000;

/// What a Workspace Errand's reply carries, each half read on its own: a
/// half that is missing, or not a string, is no answer for that half, and
/// leaves the other to land alone.
#[derive(Debug, Default, Deserialize)]
struct WorkspaceReply {
    icon: Option<Value>,
    description: Option<Value>,
}

/// What a Workspace Errand's reply derived: an Icon the Icon Catalog
/// carries, a Description with something to say, either, both, or neither.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct DerivedWorkspace {
    pub(super) icon: Option<String>,
    pub(super) description: Option<String>,
}

/// The Prompt one Workspace Errand carries: the Workspace's presented name,
/// and — where its main root is known — the opening of its README.
pub(super) fn errand_prompt(workspace: &Workspace) -> String {
    let name = presented_name(&workspace.path);
    let ask = format!(
        "Answer with an Icon chosen from the offered names, standing for what \
         the Workspace is and what it is for, and a Description: one or two \
         plain sentences, under {} characters, saying what the Workspace is \
         for, so a reader choosing among Workspaces can tell it apart from the \
         rest.",
        crate::protocol::MAX_WORKSPACE_DESCRIPTION_CHARS
    );
    match main_root(workspace).and_then(readme_excerpt) {
        Some(readme) => format!(
            "Name an Icon for, and describe, the Workspace \"{name}\".\n\n\
             Its README begins:\n{readme}\n\n{ask}"
        ),
        None => format!("Name an Icon for, and describe, the Workspace \"{name}\".\n\n{ask}"),
    }
}

/// The shape Suru asks a Workspace Errand to answer in. A request rather than
/// a guarantee, exactly like the Title Errand's own schema — every reply is
/// validated here regardless of whether the harness could enforce it.
pub(super) fn reply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "icon": {
                "type": "string",
                "description": "the Icon Catalog name standing for the Workspace",
                "enum": icon_catalog::names(),
            },
            "description": {
                "type": "string",
                "description": "one or two plain sentences saying what the Workspace is for",
            },
        },
        "required": ["icon", "description"],
        "additionalProperties": false,
    })
}

/// What a Workspace Errand's reply derived. An Icon counts only where the
/// Icon Catalog carries it, and a Description only where it says something
/// once its whitespace is collapsed — see
/// [`workspace_description::derived`] for how a long one is cut.
pub(super) fn derived_reply(answer: &Value) -> DerivedWorkspace {
    let reply: WorkspaceReply = serde_json::from_value(answer.clone()).unwrap_or_default();
    let icon = reply
        .icon
        .as_ref()
        .and_then(Value::as_str)
        .filter(|icon| icon_catalog::glyph(icon).is_some())
        .map(str::to_owned);
    let description = reply
        .description
        .as_ref()
        .and_then(Value::as_str)
        .and_then(workspace_description::derived);
    DerivedWorkspace { icon, description }
}

/// The Workspace as a reader already sees it named — the Sidebar's own
/// leaf-of-path presentation (see `crate::tui::sidebar::workspace_name`,
/// which this mirrors rather than calls: the server runs without the TUI, and
/// a six-line function is cheaper to keep in step than a dependency the other
/// way around).
fn presented_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(
            || path.as_os_str().to_string_lossy(),
            |name| name.to_string_lossy(),
        )
        .into_owned()
}

/// The Workspace's main root, where its Repository is known to have one. A
/// Bare Workspace, an `UnknownMain` one, and one with no Repository at all
/// have nowhere a README could stand that Suru would trust as the project's
/// own.
fn main_root(workspace: &Workspace) -> Option<&Path> {
    match &workspace.repository.as_ref()?.location {
        RepositoryLocation::Main { root } => Some(root.as_path()),
        RepositoryLocation::Bare { .. } | RepositoryLocation::UnknownMain => None,
    }
}

/// The opening of the first README `root` holds, or `None` where the
/// directory holds none, or cannot be read at all.
fn readme_excerpt(root: &Path) -> Option<String> {
    let path = find_readme(root)?;
    let contents = std::fs::read_to_string(path).ok()?;
    Some(contents.chars().take(MAX_README_CHARS).collect())
}

/// The first README a directory holds, by a case-insensitive `README` stem
/// with any extension or none: a `.md` one first, then the rest in file-name
/// order, which is the only order two READMEs of equal standing could be
/// picked between deterministically.
fn find_readme(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut candidates = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.eq_ignore_ascii_case("readme"))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    candidates.sort_by_key(|path| {
        !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    });
    candidates.into_iter().next()
}

/// How a Workspace's Icon lands: filling an absence only, the way a
/// derivation's does, or replacing whatever stood there, the way a user's own
/// choice does. The two calls into [`SessionStore::land_workspace_icon`]
/// differ only in this — everything else about landing an Icon is shared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IconLanding {
    /// [`SessionStore::commit_workspace_icon`]'s rule: a Workspace that
    /// already carries an Icon keeps it.
    FillAbsence,
    /// [`SessionStore::set_workspace_icon`]'s rule: a user's choice always
    /// stands, whatever the Workspace carried before.
    Replace,
}

/// Why [`SessionStore::set_workspace_icon`] refused a user's chosen Icon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetWorkspaceIconError {
    /// This server groups no Session under the named Workspace and holds no
    /// durable row for it either — there is nothing here to set an Icon on.
    WorkspaceNotFound,
    /// The named Icon does not resolve in the Icon Catalog.
    UnknownIcon,
}

impl SessionStore {
    /// The Icon a Workspace carries, or `None` for one with no Icon yet — read
    /// off the same in-memory record [`Self::commit_workspace_icon`] writes.
    /// Production code reads a Session's own `workspace.icon` instead, which
    /// [`Self::create_in_with_identity`] and [`Self::regroup`] already keep in
    /// step with this same record; this accessor exists for tests that want
    /// the Workspace's own reading without going through a Session.
    #[cfg(test)]
    pub(crate) fn workspace_icon(&self, workspace_id: &WorkspaceId) -> Option<String> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .workspaces
            .get(workspace_id)
            .and_then(|stored| stored.icon.clone())
    }

    /// Fills a Workspace's Icon, but only where it still has none: the same
    /// fill-an-absence guard as a Session's own derived Icon
    /// ([`super::title::SessionStore::replace_derived_title`]), applied here
    /// to state a Workspace owns for itself — nothing broader exists, since
    /// ADR 0027 keeps no registry of Repositories. Answers `true` where the
    /// Icon lands.
    pub(crate) fn commit_workspace_icon(&self, workspace_id: &WorkspaceId, icon: String) -> bool {
        self.land_workspace_icon(workspace_id, icon, IconLanding::FillAbsence)
    }

    /// Sets a Workspace's Icon to a user's own choice from the Icon Catalog,
    /// replacing whatever it already carried — derived earlier, chosen
    /// before, or absent — because a user's choice always stands rather than
    /// only ever filling an absence the way derivation does. Refuses a name
    /// the Icon Catalog does not carry, and refuses a Workspace this server
    /// does not know at all: one with no Session presently grouped under it
    /// and no durable row of its own, which is the only way this server could
    /// ever have heard of it.
    pub(crate) fn set_workspace_icon(
        &self,
        workspace_id: &WorkspaceId,
        icon: &str,
    ) -> Result<(), SetWorkspaceIconError> {
        if icon_catalog::glyph(icon).is_none() {
            return Err(SetWorkspaceIconError::UnknownIcon);
        }
        let known = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .knows_workspace(workspace_id);
        if !known {
            return Err(SetWorkspaceIconError::WorkspaceNotFound);
        }
        self.land_workspace_icon(workspace_id, icon.to_owned(), IconLanding::Replace);
        Ok(())
    }

    /// Writes a Workspace's Icon into the in-memory cache under `landing`'s
    /// rule, persists it, publishes a single, separate
    /// [`SessionCatalogChange::WorkspaceIconChanged`] for the Sessions a
    /// listing may hold without any of them open, and then regroups every
    /// Session presently held under that Workspace (see
    /// [`Self::regroup_workspace`]). The write is queued and the change
    /// published under the same lock as the write they follow, so a derived
    /// Icon landing just before a chosen one can never be announced after it
    /// (see [`Self::commit_workspace_description`] for the same rule). Answers
    /// whether the write landed: [`IconLanding::FillAbsence`] answers
    /// `false`, and does nothing else at all, where the Workspace already
    /// carried one.
    fn land_workspace_icon(
        &self,
        workspace_id: &WorkspaceId,
        icon: String,
        landing: IconLanding,
    ) -> bool {
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let presented = state.presented_path(workspace_id, None);
            let stored = state.workspaces.entry(workspace_id.clone()).or_default();
            if landing == IconLanding::FillAbsence && stored.icon.is_some() {
                return false;
            }
            stored.icon = Some(icon.clone());
            match landing {
                IconLanding::FillAbsence => self
                    .storage
                    .save_workspace_icon(workspace_id.clone(), icon.clone()),
                IconLanding::Replace => self
                    .storage
                    .replace_workspace_icon(workspace_id.clone(), icon.clone()),
            }
            self.record_presented_path(stored, workspace_id, presented);
            state.publish_catalog_change(SessionCatalogChange::WorkspaceIconChanged {
                workspace_id: workspace_id.clone(),
                icon: Some(icon),
            });
        }
        self.regroup_workspace(workspace_id);
        true
    }

    /// Regroups every Session presently held under a Workspace whose Icon or
    /// Description just landed — through [`super::SessionStore::regroup`],
    /// which reads both back out of the same record the landing just wrote —
    /// so an open Session's own header repaints through the ordinary
    /// `SessionChange::WorkspaceChanged` path rather than a bespoke one.
    pub(super) fn regroup_workspace(&self, workspace_id: &WorkspaceId) {
        let grouped = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .values()
            .filter(|record| &record.snapshot.session.workspace.id == workspace_id)
            .map(|record| {
                (
                    record.snapshot.session.id,
                    record.snapshot.session.workspace.clone(),
                    record.snapshot.session.checkout.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (session_id, workspace, checkout) in grouped {
            if let Err(error) = self.regroup(session_id, workspace, checkout) {
                tracing::warn!(
                    %session_id,
                    %error,
                    "could not regroup a Session after its Workspace's Icon or Description changed"
                );
            }
        }
    }

    /// Keeps `presented` as where the Workspace whose row `stored` is was
    /// presented when its Icon or Description just landed, writing it behind
    /// that landing's own write where it moved — which is what lets the
    /// Workspace be named once no Session works in it.
    pub(super) fn record_presented_path(
        &self,
        stored: &mut StoredWorkspace,
        workspace_id: &WorkspaceId,
        presented: Option<PathBuf>,
    ) {
        if let Some(path) = presented
            && stored.path.as_ref() != Some(&path)
        {
            stored.path = Some(path.clone());
            self.storage
                .record_workspace_path(workspace_id.clone(), path);
        }
    }
}

impl SessionStoreState {
    /// Whether this server has heard of a Workspace at all: it holds a
    /// durable row for it, or groups a Session under it now, one Suru could
    /// not read included. A Workspace it knows neither way is not one
    /// anything may be set on.
    pub(super) fn knows_workspace(&self, workspace_id: &WorkspaceId) -> bool {
        self.workspaces.contains_key(workspace_id) || self.grouped(workspace_id).is_some()
    }

    /// Where a Workspace is presented now: at `presented_at`, where the
    /// caller resolved it there, or else as a Session working in it presents
    /// it, or else as its row last recorded — or nowhere this server can say.
    pub(super) fn presented_path(
        &self,
        workspace_id: &WorkspaceId,
        presented_at: Option<&Path>,
    ) -> Option<PathBuf> {
        presented_at
            .map(Path::to_path_buf)
            .or_else(|| {
                self.grouped(workspace_id)
                    .map(|workspace| workspace.path.clone())
            })
            .or_else(|| {
                self.workspaces
                    .get(workspace_id)
                    .and_then(|stored| stored.path.clone())
            })
    }

    /// A copy of a Workspace that a Session grouped under it carries, one
    /// Suru could not read included.
    fn grouped(&self, workspace_id: &WorkspaceId) -> Option<&Workspace> {
        self.sessions
            .values()
            .map(|record| &record.snapshot.session.workspace)
            .chain(
                self.unreadable_sessions
                    .values()
                    .filter_map(|unreadable| unreadable.summary.workspace.as_ref()),
            )
            .find(|workspace| &workspace.id == workspace_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        Repository, RepositoryId, SourceControlAvailability, SourceControlCapabilities,
    };

    fn repository_with_root(root: PathBuf) -> Repository {
        Repository {
            id: RepositoryId::from_metadata("git", &root),
            system: "git".to_owned(),
            metadata_directory: root.join(".git"),
            location: RepositoryLocation::Main { root },
            availability: SourceControlAvailability::Available,
            capabilities: SourceControlCapabilities::discovery_only(),
        }
    }

    #[test]
    fn a_markdown_readme_is_preferred_over_an_extensionless_and_a_text_one() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        std::fs::write(root.path().join("readme.txt"), "text readme").unwrap();
        std::fs::write(root.path().join("README"), "bare readme").unwrap();
        std::fs::write(root.path().join("README.md"), "# Markdown readme").unwrap();

        assert_eq!(
            find_readme(root.path()),
            Some(root.path().join("README.md")),
            "a .md README is preferred over any other spelling"
        );
    }

    #[test]
    fn without_a_markdown_readme_the_rest_are_picked_by_file_name() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        std::fs::write(root.path().join("readme.txt"), "text readme").unwrap();
        std::fs::write(root.path().join("README"), "bare readme").unwrap();

        assert_eq!(
            find_readme(root.path()),
            Some(root.path().join("README")),
            "'README' sorts before 'readme.txt' by file name"
        );
    }

    #[test]
    fn a_directory_with_no_readme_yields_none() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        std::fs::write(root.path().join("notes.md"), "not a README").unwrap();

        assert_eq!(find_readme(root.path()), None);
    }

    #[test]
    fn an_unreadable_directory_yields_no_readme_rather_than_failing() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        let missing = root.path().join("does-not-exist");

        assert_eq!(find_readme(&missing), None);
        assert_eq!(readme_excerpt(&missing), None);
    }

    #[test]
    fn a_readme_excerpt_is_capped_at_a_char_boundary() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        // A multi-byte character sits right at the cap, which is what proves
        // the cut is by character and never mid-codepoint.
        let contents = format!("{}\u{2603}{}", "a".repeat(MAX_README_CHARS - 1), "trailer");
        std::fs::write(root.path().join("README.md"), &contents).unwrap();

        let excerpt = readme_excerpt(root.path()).expect("a README stands there");
        assert_eq!(excerpt.chars().count(), MAX_README_CHARS);
        assert!(excerpt.ends_with('\u{2603}'));
    }

    #[test]
    fn the_prompt_carries_the_readme_for_a_workspace_with_a_known_main_root() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        std::fs::write(root.path().join("README.md"), "What this project does").unwrap();
        let mut workspace = Workspace::directory(root.path().to_owned());
        workspace.repository = Some(Box::new(repository_with_root(root.path().to_owned())));

        let prompt = errand_prompt(&workspace);
        assert!(prompt.contains("What this project does"));
    }

    #[test]
    fn the_prompt_carries_only_the_name_for_a_workspace_without_a_known_main_root() {
        let root = tempfile::tempdir().expect("create a per-platform temp root");
        let workspace = Workspace::directory(root.path().join("my-project"));

        let prompt = errand_prompt(&workspace);
        assert!(prompt.contains("my-project"));
        assert!(!prompt.contains("README"));
    }

    #[test]
    fn a_reply_naming_an_unknown_icon_yields_its_description_alone() {
        assert_eq!(
            derived_reply(&json!({
                "icon": "not-a-catalog-name",
                "description": "Where the release notes are drafted.",
            })),
            DerivedWorkspace {
                icon: None,
                description: Some("Where the release notes are drafted.".to_owned()),
            }
        );
    }

    #[test]
    fn a_reply_naming_a_known_icon_and_a_blank_description_yields_the_icon_alone() {
        assert_eq!(
            derived_reply(&json!({ "icon": "md-bug", "description": " \n " })),
            DerivedWorkspace {
                icon: Some("md-bug".to_owned()),
                description: None,
            }
        );
    }

    #[test]
    fn a_reply_missing_a_half_yields_the_other() {
        assert_eq!(
            derived_reply(&json!({ "icon": "md-bug" })),
            DerivedWorkspace {
                icon: Some("md-bug".to_owned()),
                description: None,
            }
        );
        assert_eq!(
            derived_reply(&json!({ "description": "Drafts.", "icon": 7 })),
            DerivedWorkspace {
                icon: None,
                description: Some("Drafts.".to_owned()),
            }
        );
    }

    #[test]
    fn a_reply_that_is_not_an_object_yields_nothing() {
        assert_eq!(derived_reply(&json!("md-bug")), DerivedWorkspace::default());
    }

    #[test]
    fn the_reply_schema_asks_for_an_icon_and_a_description_and_nothing_else() {
        let schema = reply_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["icon", "description"]));
        assert_eq!(
            schema["properties"]["icon"]["enum"],
            json!(icon_catalog::names())
        );
        assert_eq!(schema["properties"]["description"]["type"], json!("string"));
    }

    /// The commit guard itself: a Workspace's Icon fills an absence once and
    /// stands against every later attempt, exactly like a Session's own
    /// derived Icon (see `super::title`'s own guard test).
    #[tokio::test]
    async fn a_workspace_icon_fills_an_absence_but_never_overwrites_one() {
        use crate::{
            protocol::{CreateSessionRequest, InitialPrompt, PromptId},
            storage::{StorageRepository, StorageWriter},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let created = store
            .create(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session");
        let crate::sessions::StoreOutcome::Created(snapshot) = created else {
            panic!("a fresh Prompt creates a Session");
        };
        let workspace_id = snapshot.session.workspace.id.clone();

        assert_eq!(store.workspace_icon(&workspace_id), None);
        assert!(store.commit_workspace_icon(&workspace_id, "md-bug".to_owned()));
        assert_eq!(
            store.workspace_icon(&workspace_id),
            Some("md-bug".to_owned())
        );

        assert!(
            !store.commit_workspace_icon(&workspace_id, "dev-rust".to_owned()),
            "a second commit is refused once the Workspace already carries an Icon"
        );
        assert_eq!(
            store.workspace_icon(&workspace_id),
            Some("md-bug".to_owned()),
            "the first Icon stands"
        );

        let regrouped = store
            .subscribe(snapshot.session.id)
            .expect("the Session remains held")
            .snapshot;
        assert_eq!(
            regrouped.session.workspace.icon.as_deref(),
            Some("md-bug"),
            "the Session sharing this Workspace was regrouped with the committed Icon"
        );
    }

    /// A user's own choice replaces whatever a Workspace already carried —
    /// the opposite rule from [`Self::commit_workspace_icon`]'s own guard
    /// test above — and is refused for a Catalog name the Icon Catalog does
    /// not carry, or a Workspace this store has never heard of at all.
    #[tokio::test]
    async fn a_chosen_workspace_icon_replaces_whatever_stood_there_and_refuses_the_unknown() {
        use crate::{
            protocol::{CreateSessionRequest, InitialPrompt, PromptId},
            storage::{StorageRepository, StorageWriter},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let created = store
            .create(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session");
        let crate::sessions::StoreOutcome::Created(snapshot) = created else {
            panic!("a fresh Prompt creates a Session");
        };
        let workspace_id = snapshot.session.workspace.id.clone();

        assert_eq!(
            store.set_workspace_icon(&workspace_id, "not-a-catalog-name"),
            Err(SetWorkspaceIconError::UnknownIcon)
        );
        assert_eq!(
            store.set_workspace_icon(
                &WorkspaceId("no-session-or-row-names-this-one".to_owned()),
                "md-bug"
            ),
            Err(SetWorkspaceIconError::WorkspaceNotFound)
        );
        assert_eq!(store.workspace_icon(&workspace_id), None);

        assert_eq!(store.set_workspace_icon(&workspace_id, "md-bug"), Ok(()));
        assert_eq!(
            store.workspace_icon(&workspace_id),
            Some("md-bug".to_owned())
        );

        assert_eq!(
            store.set_workspace_icon(&workspace_id, "dev-rust"),
            Ok(()),
            "a later choice replaces the one before it, unlike a derivation's own guard"
        );
        assert_eq!(
            store.workspace_icon(&workspace_id),
            Some("dev-rust".to_owned())
        );

        let regrouped = store
            .subscribe(snapshot.session.id)
            .expect("the Session remains held")
            .snapshot;
        assert_eq!(
            regrouped.session.workspace.icon.as_deref(),
            Some("dev-rust"),
            "the Session sharing this Workspace was regrouped with the chosen Icon"
        );
    }
}
