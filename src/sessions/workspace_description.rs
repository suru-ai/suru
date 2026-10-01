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
//! Either way a Description is kept on one line, its whitespace collapsed, so
//! every surface draws it as it draws a Title. A derived one is cut to
//! [`MAX_WORKSPACE_DESCRIPTION_CHARS`] and marked where it is cut, because a Model that
//! will not stop talking is not the user's fault; a set one running longer is
//! refused instead, because whoever set it can say it again more briefly.
//!
//! [`SessionStore::set_workspace_description`] needs no request to call, so
//! the Workspace endpoint and anything else that sets a Description — a
//! Sidekick's Tool, once it has one — set exactly the same one.

use std::fmt;

use crate::protocol::{
    MAX_WORKSPACE_DESCRIPTION_CHARS, SessionCatalogChange, WorkspaceDescription, WorkspaceId,
};

use super::SessionStore;

/// Why [`SessionStore::set_workspace_description`] refused a Description. Its
/// display is a sentence the one who asked can act on, whether a Client
/// shows it or a Sidekick relays it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SetWorkspaceDescriptionError {
    /// This server groups no Session under the named Workspace and holds no
    /// durable row for it either — there is nothing here to describe.
    WorkspaceNotFound(WorkspaceId),
    /// The Description runs longer than [`MAX_WORKSPACE_DESCRIPTION_CHARS`], counted
    /// once its whitespace is collapsed.
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
            Self::TooLong { chars } => write!(
                formatter,
                "A Description runs to at most {MAX_WORKSPACE_DESCRIPTION_CHARS} characters, and this one \
                 runs to {chars}; say it in a sentence or two"
            ),
        }
    }
}

impl std::error::Error for SetWorkspaceDescriptionError {}

/// A Description's text kept on one line: its whitespace, line breaks
/// included, collapsed to single spaces and trimmed from both ends.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A Model-authored Description as Suru stores it: kept on one line and cut
/// to [`MAX_WORKSPACE_DESCRIPTION_CHARS`], marked with an ellipsis where it was cut. A
/// Description with nothing left to say is no Description.
pub(super) fn derived(raw: &str) -> Option<String> {
    let line = one_line(raw);
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
        )
    }

    /// Sets a Workspace's Description to `text`, kept on one line, replacing
    /// whatever it already carried — derived, set before, or absent — and
    /// marking it set, so it stands against every later derivation. Text
    /// left blank once its whitespace is collapsed clears the Description
    /// instead, making it derivable again. Refuses a Workspace this server
    /// does not know at all, and text longer than [`MAX_WORKSPACE_DESCRIPTION_CHARS`].
    /// Answers with the Description the Workspace now carries.
    pub(crate) fn set_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        text: &str,
    ) -> Result<Option<WorkspaceDescription>, SetWorkspaceDescriptionError> {
        let line = one_line(text);
        let chars = line.chars().count();
        if chars > MAX_WORKSPACE_DESCRIPTION_CHARS {
            return Err(SetWorkspaceDescriptionError::TooLong { chars });
        }
        let known = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .knows_workspace(workspace_id);
        if !known {
            return Err(SetWorkspaceDescriptionError::WorkspaceNotFound(
                workspace_id.clone(),
            ));
        }
        let description = (!line.is_empty()).then_some(WorkspaceDescription {
            text: line,
            set: true,
        });
        self.land_workspace_description(workspace_id, description.clone());
        Ok(description)
    }

    /// Writes a Workspace's Description into the in-memory record and
    /// persists it, then regroups every Session held under the Workspace and
    /// publishes a [`SessionCatalogChange::WorkspaceDescriptionChanged`], as
    /// [`Self::commit_workspace_icon`] does for an Icon. A derived
    /// Description — one not `set` — fills an absence only, and answers
    /// `false`, doing nothing else, where the Workspace already carries one;
    /// a set Description, or none, always lands.
    ///
    /// The durable write is queued under the store's own lock, so the writer
    /// sees landings in the order this store decided them: a clearing and the
    /// derivation that fills the absence it left are never stored the other
    /// way round.
    fn land_workspace_description(
        &self,
        workspace_id: &WorkspaceId,
        description: Option<WorkspaceDescription>,
    ) -> bool {
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
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
        }
        self.regroup_workspace(workspace_id);
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .publish_catalog_change(SessionCatalogChange::WorkspaceDescriptionChanged {
                workspace_id: workspace_id.clone(),
                description,
            });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
