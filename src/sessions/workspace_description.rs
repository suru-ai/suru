//! A Workspace's Description: a sentence or two saying what it is for, kept
//! beside its Icon (see the **Description** glossary entry).
//!
//! A Description lands one of two ways. A derivation fills it, from the same
//! Errand that derives the Workspace's Icon (see [`super::workspace_icon`]),
//! through [`SessionStore::commit_workspace_description`] — and, like a
//! derived Icon, only ever into an absence. The user or a Sidekick sets it,
//! through [`SessionStore::set_workspace_description`], which replaces
//! whatever stood there and marks it set, so it stands against every later
//! derivation; setting blank text clears it instead, leaving an absence the
//! next Session created in the Workspace may derive into again.
//!
//! Either way a Description is kept as [`one_line_description`] keeps it, so
//! every surface draws it as it draws a Title, and only then measured against
//! [`MAX_WORKSPACE_DESCRIPTION_CHARS`] — the same rule a Client counts a
//! reader's writing by. A derived one running longer is cut and marked where
//! it is cut, because a Model that will not stop talking is not the user's
//! fault; a set one running longer is refused instead, because whoever set
//! it can say it again more briefly.
//!
//! [`SessionStore::set_workspace_description`] needs no request to call, so
//! the Workspace endpoint and a Sidekick's `set_workspace_description` set
//! exactly the same one.
//!
//! Either way the Workspace's row also keeps the path it was presented by
//! when its Description landed, so a Workspace described before any Session
//! works in it, or after the last one is deleted, is still known by where it
//! is.

use std::{fmt, path::Path};

use crate::protocol::{
    MAX_WORKSPACE_DESCRIPTION_CHARS, SessionCatalogChange, Workspace, WorkspaceDescription,
    WorkspaceId, description_too_long, one_line_description,
};

use super::SessionStore;

/// Why [`SessionStore::set_workspace_description`] refused a Description. Its
/// display is a sentence the one who asked can act on, whether a Client
/// shows it or a Sidekick relays it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SetWorkspaceDescriptionError {
    /// This server groups no Session under the named Workspace, holds
    /// nothing of it, and was shown nowhere that resolves to it — there is
    /// nothing here to describe.
    WorkspaceNotFound(WorkspaceId),
    /// The Description runs longer than [`MAX_WORKSPACE_DESCRIPTION_CHARS`],
    /// counted as [`one_line_description`] keeps it.
    TooLong { chars: usize },
}

impl fmt::Display for SetWorkspaceDescriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceNotFound(workspace_id) => write!(
                formatter,
                "`{}` is not a Workspace this server knows",
                workspace_id.0
            ),
            Self::TooLong { chars } => formatter.write_str(&description_too_long(*chars)),
        }
    }
}

impl std::error::Error for SetWorkspaceDescriptionError {}

/// A Model-authored Description as Suru stores it: kept on one line and cut
/// to [`MAX_WORKSPACE_DESCRIPTION_CHARS`], marked with an ellipsis where it
/// was cut. A Description with nothing left to say is no Description.
pub(super) fn derived(raw: &str) -> Option<String> {
    let line = one_line_description(raw);
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= MAX_WORKSPACE_DESCRIPTION_CHARS {
        return Some(line);
    }
    let mut cut = line
        .chars()
        .take(MAX_WORKSPACE_DESCRIPTION_CHARS - 1)
        .collect::<String>();
    cut.push('\u{2026}');
    Some(cut)
}

impl SessionStore {
    /// Fills a Workspace's Description with a derived one, but only where it
    /// still has none — the fill-an-absence rule its derived Icon follows
    /// (see [`Self::commit_workspace_icon`]). Answers `true` where the
    /// Description lands.
    pub(crate) fn commit_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        text: String,
    ) -> bool {
        self.land_workspace_description(
            workspace_id,
            Some(WorkspaceDescription { text, set: false }),
            None,
        )
    }

    /// Sets a Workspace's Description to `text`, kept as
    /// [`one_line_description`] keeps it, replacing whatever it already
    /// carried — derived, set before, or absent — and marking it set, so it
    /// stands against every later derivation. Text with nothing left once
    /// kept on one line clears the Description instead, making it derivable
    /// again. Refuses text longer than [`MAX_WORKSPACE_DESCRIPTION_CHARS`] as
    /// kept, and a Workspace this server cannot tell is its own: one it has
    /// grouped a Session under or holds a row for is, and so is one the
    /// server's own resolution of where it is presented — `resolved`, where
    /// the caller resolved one — names. Answers with the Description the
    /// Workspace now carries.
    pub(crate) fn set_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        text: &str,
        resolved: Option<&Workspace>,
    ) -> Result<Option<WorkspaceDescription>, SetWorkspaceDescriptionError> {
        let line = one_line_description(text);
        let chars = line.chars().count();
        if chars > MAX_WORKSPACE_DESCRIPTION_CHARS {
            return Err(SetWorkspaceDescriptionError::TooLong { chars });
        }
        let resolved = resolved.filter(|resolved| &resolved.id == workspace_id);
        if resolved.is_none() && !self.knows_workspace(workspace_id) {
            return Err(SetWorkspaceDescriptionError::WorkspaceNotFound(
                workspace_id.clone(),
            ));
        }
        let description = (!line.is_empty()).then_some(WorkspaceDescription {
            text: line,
            set: true,
        });
        self.land_workspace_description(
            workspace_id,
            description.clone(),
            resolved.map(|resolved| resolved.path.as_path()),
        );
        Ok(description)
    }

    /// Whether this server can tell a Workspace is its own without resolving
    /// anything: it groups a Session under it, or holds a row for it.
    pub(crate) fn knows_workspace(&self, workspace_id: &WorkspaceId) -> bool {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .knows_workspace(workspace_id)
    }

    /// Writes a Workspace's Description into the in-memory record, persists
    /// it, and publishes a [`SessionCatalogChange::WorkspaceDescriptionChanged`],
    /// then regroups every Session held under the Workspace, as
    /// [`Self::commit_workspace_icon`] does for an Icon. A derived
    /// Description — one not `set` — fills an absence only, and answers
    /// `false`, doing nothing else, where the Workspace already carries one;
    /// a set Description, or none, always lands.
    ///
    /// The durable write is queued, and the change published, under the
    /// store's own lock, together with the write they follow: the writer and
    /// every Client hear of landings in the order this store decided them, so
    /// a derivation that landed just before a user set a Description can
    /// never be announced after it, and a clearing and the derivation that
    /// fills the absence it left are never stored the other way round. The
    /// regrouping that follows reads the record as it then stands, so it is
    /// never stale either.
    ///
    /// The row keeps where the Workspace is presented — at `presented_at`,
    /// where the caller resolved it there — as
    /// [`Self::record_presented_path`] keeps it.
    fn land_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        description: Option<WorkspaceDescription>,
        presented_at: Option<&Path>,
    ) -> bool {
        self.land_workspace_description_meanwhile(workspace_id, description, presented_at, || {})
    }

    /// [`Self::land_workspace_description`], running `meanwhile` at the one
    /// point another landing could come between this one and the Sessions it
    /// regroups — which is how a test lands a second Description at exactly
    /// that point, rather than hoping a scheduler does.
    fn land_workspace_description_meanwhile(
        &self,
        workspace_id: &WorkspaceId,
        description: Option<WorkspaceDescription>,
        presented_at: Option<&Path>,
        meanwhile: impl FnOnce(),
    ) -> bool {
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let presented = state.presented_path(workspace_id, presented_at);
            let stored = state.workspaces.entry(workspace_id.clone()).or_default();
            match &description {
                Some(derived @ WorkspaceDescription { set: false, .. }) => {
                    if stored.description.is_some() {
                        return false;
                    }
                    stored.description = description.clone();
                    self.storage
                        .save_workspace_description(workspace_id.clone(), derived.clone());
                }
                _ => {
                    stored.description = description.clone();
                    self.storage
                        .replace_workspace_description(workspace_id.clone(), description.clone());
                }
            }
            self.record_presented_path(stored, workspace_id, presented);
            state.publish_catalog_change(SessionCatalogChange::WorkspaceDescriptionChanged {
                workspace_id: workspace_id.clone(),
                description,
            });
        }
        meanwhile();
        self.regroup_workspace(workspace_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::{CreateSessionRequest, InitialPrompt, PromptId, SessionCatalogChange},
        storage::{StorageRepository, StorageWriter},
    };

    /// A store holding one Session, that Session, and its Workspace — and
    /// the writer behind the store, which runs for as long as it is held.
    async fn store_with_a_session(
        data_dir: &std::path::Path,
        execution_directory: &std::path::Path,
    ) -> (
        StorageWriter,
        SessionStore,
        crate::protocol::SessionId,
        WorkspaceId,
    ) {
        let repository = StorageRepository::open(data_dir)
            .await
            .expect("open Session repository");
        let (writer, storage) = StorageWriter::spawn(repository);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let crate::sessions::StoreOutcome::Created(snapshot) = store
            .create(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session")
        else {
            panic!("a fresh Prompt creates a Session");
        };
        let workspace_id = snapshot.session.workspace.id.clone();
        (writer, store, snapshot.session.id, workspace_id)
    }

    /// The Descriptions the catalog announced, in the order it announced them.
    fn announced(
        updates: &mut tokio::sync::broadcast::Receiver<crate::protocol::SessionCatalogUpdate>,
    ) -> Vec<Option<WorkspaceDescription>> {
        let mut announced = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let SessionCatalogChange::WorkspaceDescriptionChanged { description, .. } =
                update.change
            {
                announced.push(description);
            }
        }
        announced
    }

    /// A derivation lands its Description, and before it has told anyone a
    /// user sets one: the set Description is the last word every Client
    /// hears, never the derived one it replaced.
    #[tokio::test]
    async fn a_description_set_while_a_derived_one_lands_is_the_last_one_announced() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let (_writer, store, session_id, workspace_id) =
            store_with_a_session(data_dir.path(), execution_directory.path()).await;
        let mut updates = store.subscribe_catalog(Default::default()).updates;

        assert!(store.land_workspace_description_meanwhile(
            &workspace_id,
            Some(WorkspaceDescription {
                text: "Derived.".to_owned(),
                set: false,
            }),
            None,
            || {
                store
                    .set_workspace_description(&workspace_id, "Set by hand.", None)
                    .expect("set the Description meanwhile");
            },
        ));

        let set = Some(WorkspaceDescription {
            text: "Set by hand.".to_owned(),
            set: true,
        });
        assert_eq!(
            announced(&mut updates).last(),
            Some(&set),
            "the set Description is announced last"
        );
        assert_eq!(
            store
                .subscribe(session_id)
                .expect("the Session remains held")
                .snapshot
                .session
                .workspace
                .description,
            set,
            "and is what the Session sharing the Workspace carries"
        );
    }

    #[test]
    fn a_derived_description_is_kept_on_one_line() {
        assert_eq!(
            derived("  Where the release\n notes are\tdrafted.  "),
            Some("Where the release notes are drafted.".to_owned())
        );
    }

    #[test]
    fn a_derived_description_of_nothing_is_no_description() {
        assert_eq!(derived(" \n\t "), None);
    }

    #[test]
    fn an_over_long_derived_description_is_cut_and_marked() {
        let cut = derived(&"word ".repeat(100)).expect("a long Description still yields one");
        assert_eq!(cut.chars().count(), MAX_WORKSPACE_DESCRIPTION_CHARS);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn a_derived_description_that_exactly_fills_the_cap_is_left_whole() {
        let exact = "w".repeat(MAX_WORKSPACE_DESCRIPTION_CHARS);
        assert_eq!(derived(&exact), Some(exact));
    }

    #[test]
    fn a_refusal_says_what_to_do_in_a_sentence() {
        assert_eq!(
            SetWorkspaceDescriptionError::TooLong { chars: 412 }.to_string(),
            "A Description runs to at most 300 characters, and this one runs to 412; say it in \
             a sentence or two"
        );
        assert_eq!(
            SetWorkspaceDescriptionError::WorkspaceNotFound(WorkspaceId("elsewhere".to_owned()))
                .to_string(),
            "`elsewhere` is not a Workspace this server knows"
        );
    }
}
