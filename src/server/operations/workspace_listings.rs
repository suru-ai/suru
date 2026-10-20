//! The Workspace listing as this Server gives it (ADR 0052): answering a
//! request for it, as a Session catalog stream connects, and as that stream
//! pushes it. Every listing given is taken through one place, which gives it
//! a revision of its own only where it lists otherwise than the one given
//! before it, so the listings this Server gives during one run stand in one
//! order whoever is given them: a later revision is the newer, and two at
//! the same revision list the same. A Client hearing both an answer and a
//! push keeps whichever is the newer, in whatever order they reach it.

use std::sync::Arc;

use tokio::sync::watch;
use uuid::Uuid;

use crate::protocol::{Workspace, WorkspaceListing, WorkspaceListingRevision, WorkspacePaths};

/// The Workspace listing this Server gave last, and the run it gave it in.
#[derive(Clone)]
pub(crate) struct WorkspaceListings(Arc<watch::Sender<WorkspaceListing>>);

impl WorkspaceListings {
    /// Listings given during a run of their own, beginning with none.
    pub(crate) fn new() -> Self {
        Self(Arc::new(
            watch::channel(WorkspaceListing {
                revision: WorkspaceListingRevision {
                    run: Uuid::new_v4(),
                    changes: 0,
                },
                workspace_paths: WorkspacePaths::discover(),
                workspaces: Vec::new(),
            })
            .0,
        ))
    }

    /// Gives the Workspaces `read` lists, read now, as the listing: at the
    /// revision of the one given last where they list the same, and at the
    /// next otherwise. No two are read at once, so one read later is never
    /// given at an earlier revision.
    pub(crate) fn give(&self, read: impl FnOnce() -> Vec<Workspace>) -> WorkspaceListing {
        let mut given = None;
        self.0.send_if_modified(|listing| {
            let workspaces = read();
            let changed = workspaces != listing.workspaces;
            if changed {
                listing.workspaces = workspaces;
                listing.revision.changes += 1;
            }
            given = Some(listing.clone());
            changed
        });
        given.expect("a listing is given whenever one is read")
    }

    /// Announces every listing given at a revision of its own from now on.
    pub(crate) fn follow(&self) -> watch::Receiver<WorkspaceListing> {
        self.0.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn atlas() -> Workspace {
        Workspace::directory(PathBuf::from(if cfg!(windows) {
            r"C:\work\atlas"
        } else {
            "/work/atlas"
        }))
    }

    /// A listing given as it was given before keeps its revision, and one
    /// listing otherwise is given at the next, announced to whoever follows.
    #[test]
    fn a_listing_has_a_revision_of_its_own_only_where_it_lists_otherwise() {
        let listings = WorkspaceListings::new();
        let mut following = listings.follow();

        let first = listings.give(Vec::new);
        let again = listings.give(Vec::new);
        assert_eq!(first.revision, again.revision, "nothing listed otherwise");
        assert!(!following.has_changed().expect("the listings are given"));

        let moved = listings.give(|| vec![atlas()]);
        assert_eq!(moved.revision.run, first.revision.run);
        assert_eq!(moved.revision.changes, first.revision.changes + 1);
        assert!(moved.revision.is_no_older_than(&first.revision));
        assert!(!first.revision.is_no_older_than(&moved.revision));
        assert_eq!(*following.borrow_and_update(), moved);
    }

    /// Each run draws an identity of its own, so a count of one run's
    /// changes is never read against another's.
    #[test]
    fn each_run_gives_its_listings_a_run_of_their_own() {
        let one = WorkspaceListings::new().give(Vec::new);
        let another = WorkspaceListings::new().give(Vec::new);

        assert_ne!(one.revision.run, another.revision.run);
        assert!(!another.revision.is_no_older_than(&one.revision));
    }
}
