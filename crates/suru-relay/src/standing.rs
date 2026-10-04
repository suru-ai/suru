//! What stands on a Login at the Relay: each connection a Server holds there
//! on the strength of its Login — waiting to be reached, or kept open idle —
//! and each join the Relay carries between two. Each is held while it lasts,
//! so a Login that stops standing — its Account lapsing, its Server
//! forgetting it, or its operator removing it — has everything standing on it
//! cut at once, whatever the Server is doing.
//!
//! Everything is held, and cut, under the standing lock, after the Login it
//! stands on is read and as that Login changes, so nothing comes to stand on
//! a Login after it has been cut.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::admission::Verdicts;

/// Everything standing on a Login at the Relay.
pub(crate) struct Holdings {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    next: u64,
    held: HashMap<u64, Holding>,
}

struct Holding {
    /// The identity keys of the Servers whose Logins it stands on.
    keys: Vec<Vec<u8>>,
    cut: watch::Sender<bool>,
}

/// One thing standing on Logins, until this is dropped or the Logins are cut.
pub(crate) struct Held {
    state: Arc<Mutex<State>>,
    id: u64,
    cut: watch::Receiver<bool>,
}

impl Holdings {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
        }
    }

    /// Holds something that stands on the Logins tied to `keys`, while the
    /// standing lock is held, as `_standing` shows.
    pub(crate) fn hold(&self, _standing: &Verdicts, keys: Vec<Vec<u8>>) -> Held {
        let (cut, cut_off) = watch::channel(false);
        let mut state = lock(&self.state);
        let id = state.next;
        state.next += 1;
        state.held.insert(id, Holding { keys, cut });
        Held {
            state: self.state.clone(),
            id,
            cut: cut_off,
        }
    }

    /// Cuts everything standing on a Login tied to any of `keys`, while the
    /// standing lock is held, as `_standing` shows.
    pub(crate) fn cut(&self, _standing: &Verdicts, keys: &[Vec<u8>]) {
        lock(&self.state).held.retain(|_, holding| {
            let standing = !holding.keys.iter().any(|key| keys.contains(key));
            if !standing {
                holding.cut.send_replace(true);
            }
            standing
        });
    }
}

impl Held {
    /// Returns once what is held has been cut.
    pub(crate) async fn cut(&mut self) {
        // What cut it has said so before letting go.
        let _ = self.cut.wait_for(|cut| *cut).await;
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        lock(&self.state).held.remove(&self.id);
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .expect("the Relay's holdings lock is not poisoned")
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt as _;

    use super::*;

    #[test]
    fn cutting_a_login_cuts_everything_standing_on_it_and_nothing_else() {
        let holdings = Holdings::new();
        let standing = Verdicts::default();
        let mut laptop = holdings.hold(&standing, vec![b"laptop".to_vec()]);
        let mut join = holdings.hold(&standing, vec![b"laptop".to_vec(), b"workstation".to_vec()]);
        let mut workstation = holdings.hold(&standing, vec![b"workstation".to_vec()]);
        let mut tablet = holdings.hold(&standing, vec![b"tablet".to_vec()]);
        assert!(laptop.cut().now_or_never().is_none());

        holdings.cut(&standing, &[b"workstation".to_vec(), b"phone".to_vec()]);
        assert!(join.cut().now_or_never().is_some());
        assert!(workstation.cut().now_or_never().is_some());
        assert!(
            workstation.cut().now_or_never().is_some(),
            "what is cut stays cut"
        );
        assert!(laptop.cut().now_or_never().is_none());
        assert!(tablet.cut().now_or_never().is_none());

        drop(laptop);
        holdings.cut(&standing, &[b"laptop".to_vec()]);
        assert!(tablet.cut().now_or_never().is_none());
        assert_eq!(
            lock(&holdings.state).held.len(),
            1,
            "only the tablet's is held"
        );
    }
}
