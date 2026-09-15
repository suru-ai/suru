//! Deriving a Workspace's Icon from its presented name and, where its main
//! root is known, the opening of its README.
//!
//! The Errand this builds is the second of the pair [`super::title::Derivation`]
//! runs on one Session's creation task, asked only where that Session's
//! Workspace still carries no Icon. Its prompt carries the same presentation a
//! reader already sees for that Workspace — the Sidebar's own leaf-of-path
//! name — and, for a Workspace whose main root Suru has found, the opening of
//! whichever README stands there: a project's README is usually the shortest
//! path to what the work is and what it is for, which is exactly what an Icon
//! stands for. A Bare Workspace, one whose main root is not yet known, and one
//! with no Repository at all send the name alone.
//!
//! The reply commits through [`SessionStore::commit_workspace_icon`], which
//! fills the Workspace's Icon only where it still has none — the same
//! fill-an-absence rule the Session's own derived Icon follows (see
//! [`super::title`]) — so a race between two Sessions created in the same
//! Workspace before either Errand answers resolves to whichever answers
//! first, and the other's reply lands on an absence that is no longer there
//! and is discarded. Failure records nothing at all, so the next Session
//! created in that Workspace tries again; success ends attempts for good.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    icon_catalog,
    protocol::{RepositoryLocation, SessionCatalogChange, Workspace, WorkspaceId},
};

use super::SessionStore;

/// The most of a README this carries into an Errand's prompt. Short for the
/// same reason a first Prompt's opening is: naming an Icon should not cost a
/// full-context call, and a README states what a project is before it trails
/// into detail.
const MAX_README_CHARS: usize = 2_000;

/// What Suru asks a Workspace Icon Errand to answer with. Suru's own
/// validation still looks the name up in the Icon Catalog regardless of
/// whether the harness could enforce the schema's enum.
#[derive(Debug, Deserialize)]
struct WorkspaceIconReply {
    icon: String,
}

/// The Prompt one Workspace Icon Errand carries: the Workspace's presented
/// name, and — where its main root is known — the opening of its README.
pub(super) fn errand_prompt(workspace: &Workspace) -> String {
    let name = presented_name(&workspace.path);
    match main_root(workspace).and_then(readme_excerpt) {
        Some(readme) => format!(
            "Name an Icon standing for the Workspace \"{name}\".\n\n\
             Its README begins:\n{readme}\n\n\
             Answer with an Icon chosen from the offered names, standing for \
             what the Workspace is and what it is for."
        ),
        None => format!(
            "Name an Icon standing for the Workspace \"{name}\".\n\n\
             Answer with an Icon chosen from the offered names, standing for \
             what the Workspace is and what it is for."
        ),
    }
}

/// The shape Suru asks a Workspace Icon Errand to answer in. A request rather
/// than a guarantee, exactly like the Title Errand's own schema — every reply
/// is validated here regardless of whether the harness could enforce it.
pub(super) fn reply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "icon": {
                "type": "string",
                "description": "the Icon Catalog name standing for the Workspace",
                "enum": icon_catalog::names(),
            },
        },
        "required": ["icon"],
        "additionalProperties": false,
    })
}

/// The Icon Catalog name a Workspace Icon Errand's reply names, or `None` for
/// a reply that names none the Catalog carries, or that does not deserialize
/// as this Errand's schema at all.
pub(super) fn derived_icon(answer: &Value) -> Option<String> {
    let reply: WorkspaceIconReply = serde_json::from_value(answer.clone()).ok()?;
    icon_catalog::glyph(&reply.icon)
        .is_some()
        .then_some(reply.icon)
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
            .workspace_icons
            .get(workspace_id)
            .cloned()
    }

    /// Fills a Workspace's Icon, but only where it still has none: the same
    /// fill-an-absence guard as a Session's own derived Icon
    /// ([`super::title::SessionStore::replace_derived_title`]), applied here
    /// to the one piece of state a Workspace owns for itself (see ADR 0027 for
    /// why nothing broader exists). Answers `true` where the Icon lands.
    ///
    /// Every Session presently grouped under this Workspace is regrouped with
    /// the Icon in hand — through [`super::SessionStore::regroup`], which
    /// reads it back out of the same durable record this just wrote — so an
    /// open Session's own header repaints through the ordinary
    /// `SessionChange::WorkspaceChanged` path rather than a bespoke one, and
    /// every listed Session catches up the same way it does for any other
    /// Workspace change. A single, separate
    /// [`SessionCatalogChange::WorkspaceIconChanged`] follows for the Sessions
    /// a listing may hold without any of them open.
    pub(crate) fn commit_workspace_icon(&self, workspace_id: &WorkspaceId, icon: String) -> bool {
        let grouped = {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if state.workspace_icons.contains_key(workspace_id) {
                return false;
            }
            state
                .workspace_icons
                .insert(workspace_id.clone(), icon.clone());
            state
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
                .collect::<Vec<_>>()
        };
        self.storage
            .save_workspace_icon(workspace_id.clone(), icon.clone());
        for (session_id, workspace, checkout) in grouped {
            if let Err(error) = self.regroup(session_id, workspace, checkout) {
                tracing::warn!(
                    %session_id,
                    %error,
                    "could not regroup a Session after its Workspace's Icon was derived"
                );
            }
        }
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .publish_catalog_change(SessionCatalogChange::WorkspaceIconChanged {
                workspace_id: workspace_id.clone(),
                icon: Some(icon),
            });
        true
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
        workspace.repository = Some(repository_with_root(root.path().to_owned()));

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
    fn a_reply_naming_an_unknown_icon_yields_none() {
        assert_eq!(derived_icon(&json!({ "icon": "not-a-catalog-name" })), None);
    }

    #[test]
    fn a_reply_naming_a_known_icon_yields_it() {
        assert_eq!(
            derived_icon(&json!({ "icon": "md-bug" })),
            Some("md-bug".to_owned())
        );
    }

    #[test]
    fn the_reply_schema_rejects_unasked_properties_and_enumerates_the_catalog() {
        let schema = reply_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["icon"]));
        assert_eq!(
            schema["properties"]["icon"]["enum"],
            json!(icon_catalog::names())
        );
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
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let created = store
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
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
}
