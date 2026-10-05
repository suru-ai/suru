//! The caps a Relay's operator sets so that no one can exhaust the Relay.
//!
//! On each Account: how many Logins stand under it, and how many connections
//! the Relay joins for it at once. They are also the whole of what a phished
//! login costs: a stranger's Server put under a user's Account holds nothing
//! there but a place against its cap of Logins (ADR-0048). Each is decided
//! under the standing lock at the moment what it counts comes to count — a
//! Login as it is formed, a join as it is asked — so no two decisions can each
//! find the last place free. A Login holds its place for as long as it is
//! kept, its Account lapsed or not, and gives it back the moment it is
//! forgotten or removed; a join holds its place from just before it is asked
//! until it ends, however it ends.
//!
//! On connections, which anyone may open, Login or none: how many the Relay
//! holds at once, from everyone together ([`crate::listener`]); how many idle
//! connections each Server holds — those on which it does nothing, neither
//! waiting to be reached, nor joined or asking to be, nor logging in — a few
//! where it holds no Login that stands, and more where it does; and how many
//! logins are under way at once, each Server logging in on one connection at
//! a time. A Server's waiting connections are held to
//! [`crate::WAITING_CONNECTIONS_PER_SERVER`], the joins asked of it to
//! [`crate::JOINS_ASKED_PER_SERVER`], and those it asks for and carries to its
//! Account's cap of joined connections, so whatever a connection does, it is
//! held to a cap.

use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

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

/// How many connections a Relay holds at once, from every Server together,
/// unless its configuration says otherwise. Each costs the Relay a socket and
/// the buffers its WebSocket reads and writes through — idle, a little over a
/// hundred kibibytes; carrying a join, up to twice that — so 8,192 bounds what
/// they hold of it to a gibibyte or two, while leaving room for dozens of
/// Accounts each with every Server keeping every other in view: an Account at
/// its cap of joined connections holds 512.
pub const CONNECTIONS_AT_ONCE: NonZeroU32 = NonZeroU32::new(8192).unwrap();

/// How many logins may be under way at a Relay at once, from every Server
/// together, unless its configuration says otherwise. A login waits up to a
/// quarter of an hour for its user to finish it at the identity provider,
/// asking the provider how it stands every few seconds meanwhile, and GitHub
/// takes no more than fifty device logins an hour for each app; 128 is room
/// for a team logging in all at once, many times over.
pub const LOGINS_AT_ONCE: NonZeroU32 = NonZeroU32::new(128).unwrap();

/// How many idle connections a Server whose Login stands may hold at a Relay
/// at once, unless its configuration says otherwise. A Server keeps one —
/// the connection it hears at once on that its Login stops standing, unless
/// it waits on that one to be reached — and those it opens to ask for joins,
/// or take them up, ask the moment they are proven, within the idle grace,
/// so are never counted. 32 leaves room many times over — for a Server
/// connecting again while the Relay has yet to see its last connections go,
/// say — while bounding what an Account's Servers hold idle, at its cap of
/// Logins, to 2,048.
pub const IDLE_CONNECTIONS_PER_SERVER: NonZeroU32 = NonZeroU32::new(32).unwrap();

/// How many idle connections a Server holding no Login that stands may hold
/// at a Relay at once, unless its configuration says otherwise. Such a Server
/// connects to log in, to be forgotten, or to learn its Login needs renewing,
/// and asks what it came for at once, so it has one or two at a time.
pub const IDLE_CONNECTIONS_PER_SERVER_WITHOUT_LOGIN: NonZeroU32 = NonZeroU32::new(4).unwrap();

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

/// The idle connections each Server holds, held to one cap where its Login
/// stands and another where it holds none.
pub(crate) struct Idle {
    standing: NonZeroU32,
    without_login: NonZeroU32,
    /// How many places each Server holds, by its identity key, where it holds
    /// any.
    held: Arc<Mutex<HashMap<Vec<u8>, u32>>>,
}

/// A place for one idle connection of a Server's, given back as this drops.
pub(crate) struct IdlePlace {
    held: Arc<Mutex<HashMap<Vec<u8>, u32>>>,
    key: Vec<u8>,
}

impl Idle {
    pub(crate) fn new(standing: NonZeroU32, without_login: NonZeroU32) -> Self {
        Self {
            standing,
            without_login,
            held: Arc::default(),
        }
    }

    /// A place for one more idle connection of the Server whose identity key
    /// is `key`, where it holds fewer than its cap — the cap of a Server
    /// whose Login stands, where the connection stands on it, and of one
    /// holding none otherwise.
    pub(crate) fn take(&self, key: &[u8], stands: bool) -> Option<IdlePlace> {
        let limit = if stands {
            self.standing
        } else {
            self.without_login
        };
        let mut held = lock(&self.held);
        let places = held.entry(key.to_vec()).or_default();
        if *places >= limit.get() {
            return None;
        }
        *places += 1;
        Some(IdlePlace {
            held: self.held.clone(),
            key: key.to_vec(),
        })
    }

    /// The most idle connections a Server may hold at once, where its Login
    /// stands, as `stands` says, and where it holds none.
    pub(crate) fn limit(&self, stands: bool) -> NonZeroU32 {
        if stands {
            self.standing
        } else {
            self.without_login
        }
    }
}

impl Drop for IdlePlace {
    fn drop(&mut self) {
        let mut held = lock(&self.held);
        if let Some(places) = held.get_mut(&self.key) {
            *places -= 1;
            if *places == 0 {
                held.remove(&self.key);
            }
        }
    }
}

/// The logins under way at the Relay, held to a cap, each Server's on one
/// connection at a time.
pub(crate) struct Logins {
    places: Arc<Semaphore>,
    state: Arc<Mutex<LoginsUnderWay>>,
}

#[derive(Default)]
struct LoginsUnderWay {
    next: u64,
    /// The login each Server has under way, by its identity key.
    by_server: HashMap<Vec<u8>, UnderWay>,
}

/// A Server's login under way: its number, its place against the cap, and
/// what tells it that it has been given up for a later one.
struct UnderWay {
    number: u64,
    place: OwnedSemaphorePermit,
    give_up: oneshot::Sender<()>,
}

/// A login under way, holding its place against the cap until it drops.
pub(crate) struct LoginPlace {
    state: Arc<Mutex<LoginsUnderWay>>,
    key: Vec<u8>,
    number: u64,
    given_up: oneshot::Receiver<()>,
}

impl Logins {
    pub(crate) fn new(cap: NonZeroU32) -> Self {
        Self {
            places: Arc::new(Semaphore::new(cap.get() as usize)),
            state: Arc::default(),
        }
    }

    /// A place for a login the Server whose identity key is `key` begins:
    /// the place of the login it already has under way, where it has one,
    /// which is given up — the Server gave it up as it began this one — and
    /// otherwise one of those free against the cap, where one is.
    pub(crate) fn begin(&self, key: &[u8]) -> Option<LoginPlace> {
        let mut state = lock(&self.state);
        let place = match state.by_server.remove(key) {
            Some(earlier) => {
                let _ = earlier.give_up.send(());
                earlier.place
            }
            None => self.places.clone().try_acquire_owned().ok()?,
        };
        let number = state.next;
        state.next += 1;
        let (give_up, given_up) = oneshot::channel();
        state.by_server.insert(
            key.to_vec(),
            UnderWay {
                number,
                place,
                give_up,
            },
        );
        Some(LoginPlace {
            state: self.state.clone(),
            key: key.to_vec(),
            number,
            given_up,
        })
    }
}

impl LoginPlace {
    /// Returns once the login is given up for a later one its Server began.
    pub(crate) async fn given_up(&mut self) {
        let _ = (&mut self.given_up).await;
    }
}

impl Drop for LoginPlace {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        if state
            .by_server
            .get(&self.key)
            .is_some_and(|under_way| under_way.number == self.number)
        {
            state.by_server.remove(&self.key);
        }
    }
}

/// What a cap holds, as it stands. Each step taken under its lock is a lookup
/// and a count, which leave it whole however they fail.
fn lock<T>(held: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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

    #[test]
    fn a_server_holds_no_more_idle_connections_than_its_cap_and_fewer_without_a_login() {
        let idle = Idle::new(NonZeroU32::new(3).unwrap(), NonZeroU32::new(1).unwrap());
        let first = idle.take(b"laptop", false).expect("a place");
        assert!(
            idle.take(b"laptop", false).is_none(),
            "the cap without a Login"
        );
        let second = idle
            .take(b"laptop", true)
            .expect("a connection standing on a Login is held to the larger cap");
        let third = idle.take(b"laptop", true).expect("a place");
        assert!(idle.take(b"laptop", true).is_none(), "the cap with a Login");
        let elsewhere = idle
            .take(b"tablet", false)
            .expect("another Server's places are its own");
        assert!(idle.take(b"tablet", false).is_none());
        drop((first, second, third, elsewhere));
        assert!(lock(&idle.held).is_empty(), "nothing is kept of no place");
    }

    #[tokio::test]
    async fn a_server_logs_in_once_at_a_time_its_later_login_taking_the_place_of_the_earlier() {
        let logins = Logins::new(NonZeroU32::new(2).unwrap());
        let mut earlier = logins.begin(b"laptop").expect("a place");
        let tablet = logins.begin(b"tablet").expect("a place");
        assert!(logins.begin(b"phone").is_none(), "the cap is reached");

        let mut later = logins.begin(b"laptop").expect("the earlier login's place");
        earlier.given_up().await;
        drop(earlier);
        assert!(
            logins.begin(b"phone").is_none(),
            "an earlier login given up and gone leaves its place with the later"
        );
        assert!(
            futures_util::FutureExt::now_or_never(later.given_up()).is_none(),
            "the later login is under way still"
        );
        drop(later);
        let phone = logins.begin(b"phone").expect("a place given back");
        drop((tablet, phone));
        assert!(lock(&logins.state).by_server.is_empty());
        assert_eq!(logins.places.available_permits(), 2);
    }
}
