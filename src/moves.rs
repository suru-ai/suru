//! Counting the moves of something whose followers read it afresh whenever
//! it moves rather than being told what moved: each of the things the
//! Workspace listing is read from announces its moves this way, and the
//! listing a Server pushes follows them all.

use std::sync::Arc;

use tokio::sync::watch;

/// Counts each move of what it is kept beside, and announces each to
/// whoever follows it. Clones count the same moves.
#[derive(Clone, Debug)]
pub(crate) struct Moves(Arc<watch::Sender<u64>>);

impl Default for Moves {
    fn default() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }
}

impl Moves {
    /// Counts one move, and announces it. Whoever keeps what moved counts
    /// the move while it still holds that thing as the move left it, so a
    /// reading taken while the count stands still saw no move half made.
    pub(crate) fn moved(&self) {
        self.0.send_modify(|moves| *moves += 1);
    }

    /// Announces every move from now on.
    pub(crate) fn follow(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_move_is_counted_and_announced_to_every_follower_of_any_clone() {
        let moves = Moves::default();
        let mut follower = moves.follow();
        let clone = moves.clone();

        clone.moved();
        clone.moved();

        assert!(follower.has_changed().expect("the moves are still counted"));
        assert_eq!(*follower.borrow_and_update(), 2);
    }
}
