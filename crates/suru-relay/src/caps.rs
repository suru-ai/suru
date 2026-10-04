//! The caps a Relay's operator sets on each Account, so that no one Account
//! can exhaust the Relay: on how many Logins stand under it, and on how many
//! connections the Relay joins for it at once. They are also the whole of
//! what a phished login costs: a stranger's Server put under a user's Account
//! holds nothing there but a place against its cap of Logins (ADR-0048).
//!
//! Each cap is decided under the standing lock at the moment what it counts
//! comes to count — a Login as it is formed, a join as it is asked — so no
//! two decisions can each find the last place free. A Login holds its place
//! for as long as it is kept, its Account lapsed or not, and gives it back
//! the moment it is forgotten or removed; a join holds its place from just
//! before it is asked until it ends, however it ends.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use crate::admission::Verdicts;

/// How many Logins may stand under one Account, unless the Relay's
/// configuration says otherwise. One person's Servers — a laptop or two, a
/// workstation, the machines and virtual machines they work on, each Server
/// on a machine with a Login of its own — come to a dozen or so, and a
/// machine wiped and set up again leaves its old Login behind until the
/// operator removes it; 64 leaves room for years of that, while bounding
/// what one Account, or a stranger's Server a phished login put under it,
/// holds of the Relay.
pub const LOGINS_PER_ACCOUNT: NonZeroU32 = NonZeroU32::new(64).unwrap();

/// How many connections a Relay joins for one Account at once, unless its
/// configuration says otherwise. A Remote in view costs one, for as long as
/// anything on the Server viewing it keeps it in view, so an Account whose
/// Servers each keep every other in view holds one for each ordered pair of
/// them: sixteen Servers, 240. 256 leaves room for that, and for a join being
/// made again while the one before it winds down, while bounding what one
/// Account holds of the Relay to 512 of its connections.
pub const JOINED_CONNECTIONS_PER_ACCOUNT: NonZeroU32 = NonZeroU32::new(256).unwrap();

/// The connections joined for each Account, held to a cap.
pub(crate) struct Joined {
    limit: NonZeroU32,
    /// How many places each Account holds, by its id, where it holds any.
    held: Arc<Mutex<HashMap<i64, u32>>>,
}

/// One Account's place against its cap of joined connections, given back as
/// this drops.
pub(crate) struct Place {
    held: Arc<Mutex<HashMap<i64, u32>>>,
    account: i64,
}

impl Joined {
    pub(crate) fn new(limit: NonZeroU32) -> Self {
        Self {
            limit,
            held: Arc::default(),
        }
    }

    /// The most connections joined for one Account at once.
    pub(crate) fn limit(&self) -> NonZeroU32 {
        self.limit
    }

    /// A place for one more connection joined for the Account `account`,
    /// where it holds fewer than its cap, while the standing lock is held,
    /// as `_standing` shows.
    pub(crate) fn take(&self, _standing: &Verdicts, account: i64) -> Option<Place> {
        let mut held = lock(&self.held);
        let places = held.entry(account).or_default();
        if *places >= self.limit.get() {
            return None;
        }
        *places += 1;
        Some(Place {
            held: self.held.clone(),
            account,
        })
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut held = lock(&self.held);
        if let Some(places) = held.get_mut(&self.account) {
            *places -= 1;
            if *places == 0 {
                held.remove(&self.account);
            }
        }
    }
}

/// The places held, as they stand. Each step taken under its lock is a
/// lookup and a count, which leave it whole however they fail.
fn lock(held: &Mutex<HashMap<i64, u32>>) -> std::sync::MutexGuard<'_, HashMap<i64, u32>> {
    held.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_holds_no_more_places_than_its_cap_and_each_comes_back_as_it_goes() {
        let joined = Joined::new(NonZeroU32::new(2).unwrap());
        let standing = Verdicts::default();
        let first = joined.take(&standing, 1).expect("a place");
        let second = joined.take(&standing, 1).expect("a place");
        assert!(joined.take(&standing, 1).is_none(), "the cap is reached");
        let elsewhere = joined
            .take(&standing, 2)
            .expect("another Account's places are its own");

        drop(first);
        let again = joined.take(&standing, 1).expect("a place given back");
        drop((second, again, elsewhere));
        assert!(lock(&joined.held).is_empty(), "nothing is kept of no place");
    }
}
