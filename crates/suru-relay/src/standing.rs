//! What stands on a Login at the Relay: each connection a Server holds there
//! on the strength of its Login — waiting to be reached, or kept open idle —
//! and each join the Relay carries between two. Each is held while it lasts,
//! with the Account its Logins stood under as it was held, so a Login that
//! stops standing — its Account lapsing, its Server forgetting it, or its
//! operator removing it — or that comes to stand under another Account has
//! everything standing on it cut at once, whatever the Server is doing; and an
//! Account that lapses has everything standing on its Logins cut, whether or
//! not the Relay could read which Logins those are.
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
    /// The Account those Logins stood under as it was held, by its id.
    account: i64,
    cut: watch::Sender<Option<Cut>>,
}

/// Why what stands on a Login is cut.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Cut {
    /// The Login stands no longer, so its Server must log in again.
    Refused,
    /// The Login stands under another Account than what was held stood
    /// under, so its Server may connect again at once and stand under that.
    Moved,
}

/// What a cut cut, until each has let go: a connection closed, a join's
/// line handed to the connection log.
pub(crate) struct Released(Vec<watch::Sender<Option<Cut>>>);

impl Released {
    /// Returns once everything cut has let go.
    pub(crate) async fn all(self) {
        for cut in &self.0 {
            cut.closed().await;
        }
    }
}

/// One thing standing on Logins, until this is dropped or the Logins are cut.
pub(crate) struct Held {
    state: Arc<Mutex<State>>,
    id: u64,
    cut: watch::Receiver<Option<Cut>>,
}

impl Holdings {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
        }
    }

    /// Holds something that stands on the Logins tied to `keys`, which stand
    /// under the Account `account`, while the standing lock is held, as
    /// `_standing` shows.
    pub(crate) fn hold(&self, _standing: &Verdicts, keys: Vec<Vec<u8>>, account: i64) -> Held {
        let (cut, cut_off) = watch::channel(None);
        let mut state = lock(&self.state);
        let id = state.next;
        state.next += 1;
        state.held.insert(id, Holding { keys, account, cut });
        Held {
            state: self.state.clone(),
            id,
            cut: cut_off,
        }
    }

    /// Cuts, for `why`, everything standing on a Login tied to any of `keys`,
    /// while the standing lock is held, as `_standing` shows: answers what it
    /// cut, to be waited on to let go.
    pub(crate) fn cut(&self, _standing: &Verdicts, keys: &[Vec<u8>], why: Cut) -> Released {
        self.cut_where(
            |holding| holding.keys.iter().any(|key| keys.contains(key)),
            why,
        )
    }

    /// Cuts, for `why`, everything standing on Logins that stood under the
    /// Account `account` as it was held, while the standing lock is held, as
    /// `_standing` shows: answers what it cut, to be waited on to let go.
    pub(crate) fn cut_account(&self, _standing: &Verdicts, account: i64, why: Cut) -> Released {
        self.cut_where(|holding| holding.account == account, why)
    }

    /// Cuts, for `why`, everything held that `cuts` picks out.
    fn cut_where(&self, mut cuts: impl FnMut(&Holding) -> bool, why: Cut) -> Released {
        let mut state = lock(&self.state);
        let cut = state
            .held
            .extract_if(|_, holding| cuts(holding))
            .map(|(_, holding)| {
                holding.cut.send_replace(Some(why));
                holding.cut
            })
            .collect();
        Released(cut)
    }
}

impl Held {
    /// Returns once what is held has been cut, saying why.
    pub(crate) async fn cut(&mut self) -> Cut {
        // What cut it has said why before letting go.
        let cut = self.cut.wait_for(Option::is_some).await;
        cut.ok().and_then(|cut| *cut).unwrap_or(Cut::Refused)
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
        let mut laptop = holdings.hold(&standing, vec![b"laptop".to_vec()], 1);
        let mut join = holdings.hold(
            &standing,
            vec![b"laptop".to_vec(), b"workstation".to_vec()],
            1,
        );
        let mut workstation = holdings.hold(&standing, vec![b"workstation".to_vec()], 1);
        let mut tablet = holdings.hold(&standing, vec![b"tablet".to_vec()], 2);
        assert!(laptop.cut().now_or_never().is_none());

        let released = holdings.cut(
            &standing,
            &[b"workstation".to_vec(), b"phone".to_vec()],
            Cut::Moved,
        );
        assert_eq!(join.cut().now_or_never(), Some(Cut::Moved));
        assert_eq!(workstation.cut().now_or_never(), Some(Cut::Moved));
        assert_eq!(
            workstation.cut().now_or_never(),
            Some(Cut::Moved),
            "what is cut stays cut"
        );
        assert!(laptop.cut().now_or_never().is_none());
        assert!(tablet.cut().now_or_never().is_none());

        let mut releasing = Box::pin(released.all());
        assert!(
            (&mut releasing).now_or_never().is_none(),
            "what is cut has yet to let go"
        );
        drop((join, workstation));
        assert!(
            releasing.now_or_never().is_some(),
            "what is cut has let go once each is dropped"
        );

        drop(laptop);
        holdings.cut(&standing, &[b"laptop".to_vec()], Cut::Refused);
        assert!(tablet.cut().now_or_never().is_none());
        assert_eq!(
            lock(&holdings.state).held.len(),
            1,
            "only the tablet's is held"
        );
    }

    #[test]
    fn cutting_an_account_cuts_everything_standing_on_its_logins_and_nothing_else() {
        let holdings = Holdings::new();
        let standing = Verdicts::default();
        let mut laptop = holdings.hold(&standing, vec![b"laptop".to_vec()], 1);
        let mut join = holdings.hold(
            &standing,
            vec![b"laptop".to_vec(), b"workstation".to_vec()],
            1,
        );
        let mut stranger = holdings.hold(&standing, vec![b"stranger".to_vec()], 2);

        holdings.cut_account(&standing, 1, Cut::Refused);
        assert_eq!(laptop.cut().now_or_never(), Some(Cut::Refused));
        assert_eq!(join.cut().now_or_never(), Some(Cut::Refused));
        assert!(stranger.cut().now_or_never().is_none());
    }
}
