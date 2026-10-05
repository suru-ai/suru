//! Which Servers wait to be reached at the Relay, and the joins asked of them
//! that they have yet to take up. Neither outlasts the Relay: a restarted
//! Relay knows no Server to be waiting until it waits again.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{mpsc, oneshot};

/// How many bytes name a join.
const JOIN_NAME_LEN: usize = 32;

/// How many connections one Server waits on at once. One more gives way to
/// the oldest, which may be one its Server left behind on a network that
/// dropped it unannounced: a Relay hears nothing of a waiting connection
/// until it speaks on it.
pub const WAITING_CONNECTIONS_PER_SERVER: usize = 4;

/// How many joins may be asked of one waiting Server and not yet taken up;
/// past it, the Server is not waiting for more. No connection it waits on
/// is ever told of more than this many at once, either.
pub const JOINS_ASKED_PER_SERVER: usize = 16;

/// The waiting Servers and the joins asked of them. A join hands its taker's
/// connection, a `T`, to the Server that asked for it.
pub(crate) struct Joiner<T> {
    state: Arc<Mutex<State<T>>>,
}

struct State<T> {
    next_waiting: u64,
    /// The connections each Server waits on, by its identity key: the latest
    /// last.
    waiting: HashMap<Vec<u8>, Vec<WaitingOn>>,
    /// The joins asked and not yet taken up, by their names.
    asked: HashMap<Vec<u8>, Asked<T>>,
}

/// One connection a Server waits on, by its number, with what tells it of
/// each join asked of it.
struct WaitingOn {
    id: u64,
    reach: mpsc::Sender<Vec<u8>>,
}

/// A join asked and not yet taken up, bound to the two Logins it is between.
struct Asked<T> {
    /// The identity key of the Server the join was asked of, which alone may
    /// take it up.
    server_key: Vec<u8>,
    /// The identity key of the Server that asked for it.
    asker_key: Vec<u8>,
    /// The Account both Servers' Logins stood under as it was asked.
    account: i64,
    taker: oneshot::Sender<T>,
}

/// A join a Server has taken up: whom it is for, the Account both Logins
/// stood under as it was asked, and what hands the taker's connection on.
pub(crate) struct TakenUp<T> {
    pub(crate) asker_key: Vec<u8>,
    pub(crate) account: i64,
    pub(crate) taker: oneshot::Sender<T>,
}

/// Why a join could not be asked of a Server.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NotAsked {
    /// It waits on no connection that can be told of the join.
    NotWaiting,
    /// It has as many joins asked of it as it may.
    Busy,
}

/// A connection a Server waits on. It stops waiting once this is dropped.
pub(crate) struct Waiting<T> {
    state: Arc<Mutex<State<T>>>,
    server_key: Vec<u8>,
    id: u64,
    /// The names of the joins asked of the Server, as they are asked; it
    /// ends once the connection gives way to a later one.
    pub(crate) reaches: mpsc::Receiver<Vec<u8>>,
}

/// A join asked of a waiting Server. It is given up once this is dropped.
pub(crate) struct Asking<T> {
    state: Arc<Mutex<State<T>>>,
    name: Vec<u8>,
    /// The connection the Server takes the join up on, once it does; it
    /// ends untaken where the join is given up.
    pub(crate) taken_up: oneshot::Receiver<T>,
}

impl<T> Joiner<T> {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                next_waiting: 0,
                waiting: HashMap::new(),
                asked: HashMap::new(),
            })),
        }
    }

    /// Has the Server whose identity key is `server_key` wait to be reached,
    /// until what this answers is dropped — or until it waits on more
    /// connections than it may, when the oldest gives way.
    pub(crate) fn wait(&self, server_key: Vec<u8>) -> Waiting<T> {
        let (reach, reaches) = mpsc::channel(JOINS_ASKED_PER_SERVER);
        let mut state = lock(&self.state);
        let id = state.next_waiting;
        state.next_waiting += 1;
        let waiting = state.waiting.entry(server_key.clone()).or_default();
        waiting.push(WaitingOn { id, reach });
        if waiting.len() > WAITING_CONNECTIONS_PER_SERVER {
            waiting.remove(0);
        }
        Waiting {
            state: self.state.clone(),
            server_key,
            id,
            reaches,
        }
    }

    /// Asks the Server whose identity key is `server_key`, for the one whose
    /// key is `asker_key`, to take up a join between their Logins under
    /// `account`, telling the connection it last began to wait on.
    pub(crate) fn ask(
        &self,
        server_key: &[u8],
        asker_key: &[u8],
        account: i64,
    ) -> Result<Asking<T>, NotAsked> {
        let name = fresh_name();
        let (taker, taken_up) = oneshot::channel();
        let mut state = lock(&self.state);
        let asked_of_it = state
            .asked
            .values()
            .filter(|asked| asked.server_key == server_key)
            .count();
        if asked_of_it >= JOINS_ASKED_PER_SERVER {
            return Err(NotAsked::Busy);
        }
        state
            .waiting
            .get(server_key)
            .and_then(|waiting| {
                waiting
                    .iter()
                    .rev()
                    .find(|waiting| waiting.reach.try_send(name.clone()).is_ok())
            })
            .ok_or(NotAsked::NotWaiting)?;
        state.asked.insert(
            name.clone(),
            Asked {
                server_key: server_key.to_vec(),
                asker_key: asker_key.to_vec(),
                account,
                taker,
            },
        );
        Ok(Asking {
            state: self.state.clone(),
            name,
            taken_up,
        })
    }

    /// Takes up the join named `name` for the Server whose identity key is
    /// `server_key`, where that join was asked of that Server and is still
    /// asked. A join is taken up once.
    pub(crate) fn take_up(&self, name: &[u8], server_key: &[u8]) -> Option<TakenUp<T>> {
        let mut state = lock(&self.state);
        if state.asked.get(name)?.server_key != server_key {
            return None;
        }
        state.asked.remove(name).map(|asked| TakenUp {
            asker_key: asked.asker_key,
            account: asked.account,
            taker: asked.taker,
        })
    }

    /// Gives up every join asked of or by the Server whose identity key is
    /// `key`, whose Login has changed: no join is made on the strength of a
    /// Login as it stood before.
    pub(crate) fn give_up_joins_of(&self, key: &[u8]) {
        lock(&self.state)
            .asked
            .retain(|_, asked| asked.server_key != key && asked.asker_key != key);
    }

    /// Gives up every join asked under the Account `account`, which has
    /// lapsed: no join is made on the strength of Logins that no longer
    /// stand.
    pub(crate) fn give_up_joins_under(&self, account: i64) {
        lock(&self.state)
            .asked
            .retain(|_, asked| asked.account != account);
    }
}

impl<T> Drop for Waiting<T> {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        if let Some(waiting) = state.waiting.get_mut(&self.server_key) {
            waiting.retain(|waiting| waiting.id != self.id);
            if waiting.is_empty() {
                state.waiting.remove(&self.server_key);
            }
        }
    }
}

impl<T> Drop for Asking<T> {
    fn drop(&mut self) {
        lock(&self.state).asked.remove(&self.name);
    }
}

fn lock<T>(state: &Mutex<State<T>>) -> std::sync::MutexGuard<'_, State<T>> {
    state
        .lock()
        .expect("the Relay's joins lock is not poisoned")
}

fn fresh_name() -> Vec<u8> {
    let mut name = vec![0; JOIN_NAME_LEN];
    SystemRandom::new()
        .fill(&mut name)
        .expect("the operating system supplies randomness");
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_join_reaches_the_connection_a_server_last_began_to_wait_on() {
        let joiner = Joiner::<&str>::new();
        assert_eq!(
            joiner.ask(b"workstation", b"laptop", 1).err(),
            Some(NotAsked::NotWaiting)
        );
        let mut first = joiner.wait(b"workstation".to_vec());
        let mut latest = joiner.wait(b"workstation".to_vec());

        let _asking = joiner.ask(b"workstation", b"laptop", 1).unwrap();
        assert!(latest.reaches.recv().await.is_some());
        assert!(first.reaches.try_recv().is_err());

        drop(latest);
        let _asking = joiner.ask(b"workstation", b"laptop", 1).unwrap();
        assert!(first.reaches.recv().await.is_some());
        drop(first);
        assert_eq!(
            joiner.ask(b"workstation", b"laptop", 1).err(),
            Some(NotAsked::NotWaiting)
        );
    }

    #[tokio::test]
    async fn a_server_waiting_on_one_connection_too_many_lets_the_oldest_go() {
        let joiner = Joiner::<()>::new();
        let mut waiting = (0..=WAITING_CONNECTIONS_PER_SERVER)
            .map(|_| joiner.wait(b"workstation".to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(
            waiting[0].reaches.recv().await,
            None,
            "the oldest stops waiting"
        );
        let _asking = joiner.ask(b"workstation", b"laptop", 1).unwrap();
        assert!(waiting.last_mut().unwrap().reaches.recv().await.is_some());
    }

    #[tokio::test]
    async fn a_join_is_taken_up_once_by_the_server_it_was_asked_of_while_it_is_asked() {
        let joiner = Joiner::<&str>::new();
        let mut waiting = joiner.wait(b"workstation".to_vec());
        let mut asking = joiner.ask(b"workstation", b"laptop", 7).unwrap();
        let name = waiting.reaches.recv().await.unwrap();
        assert!(joiner.take_up(&name, b"laptop").is_none());
        let taken_up = joiner
            .take_up(&name, b"workstation")
            .expect("the Server asked takes the join up");
        assert_eq!(
            (taken_up.asker_key.as_slice(), taken_up.account),
            (&b"laptop"[..], 7)
        );
        taken_up.taker.send("connection").unwrap();
        assert_eq!((&mut asking.taken_up).await, Ok("connection"));
        assert!(joiner.take_up(&name, b"workstation").is_none());

        let asking = joiner.ask(b"workstation", b"laptop", 7).unwrap();
        let name = waiting.reaches.recv().await.unwrap();
        drop(asking);
        assert!(
            joiner.take_up(&name, b"workstation").is_none(),
            "a join given up is taken up by nobody"
        );
    }

    #[tokio::test]
    async fn the_joins_of_a_server_whose_login_changed_are_given_up() {
        let joiner = Joiner::<()>::new();
        let mut waiting = joiner.wait(b"workstation".to_vec());
        let mut by_laptop = joiner.ask(b"workstation", b"laptop", 1).unwrap();
        let mut by_tablet = joiner.ask(b"workstation", b"tablet", 1).unwrap();
        let (laptops, tablets) = (
            waiting.reaches.recv().await.unwrap(),
            waiting.reaches.recv().await.unwrap(),
        );

        joiner.give_up_joins_of(b"laptop");
        assert!((&mut by_laptop.taken_up).await.is_err());
        assert!(joiner.take_up(&laptops, b"workstation").is_none());
        joiner.give_up_joins_of(b"workstation");
        assert!((&mut by_tablet.taken_up).await.is_err());
        assert!(joiner.take_up(&tablets, b"workstation").is_none());
    }

    #[tokio::test]
    async fn the_joins_asked_under_an_account_that_lapsed_are_given_up() {
        let joiner = Joiner::<()>::new();
        let mut waiting = joiner.wait(b"workstation".to_vec());
        let mut lapsed = joiner.ask(b"workstation", b"laptop", 1).unwrap();
        let mut other = joiner.ask(b"workstation", b"tablet", 2).unwrap();
        let (_, others) = (
            waiting.reaches.recv().await.unwrap(),
            waiting.reaches.recv().await.unwrap(),
        );

        joiner.give_up_joins_under(1);
        assert!((&mut lapsed.taken_up).await.is_err());
        assert!(
            futures_util::FutureExt::now_or_never(&mut other.taken_up).is_none(),
            "a join under another Account is asked still"
        );
        assert!(joiner.take_up(&others, b"workstation").is_some());
    }

    #[test]
    fn a_waiting_server_has_a_bounded_number_of_joins_asked_of_it() {
        let joiner = Joiner::<()>::new();
        let mut waiting = joiner.wait(b"workstation".to_vec());
        let asked = (0..JOINS_ASKED_PER_SERVER)
            .map(|_| joiner.ask(b"workstation", b"laptop", 1).expect("room"))
            .collect::<Vec<_>>();
        while waiting.reaches.try_recv().is_ok() {}
        assert_eq!(
            joiner.ask(b"workstation", b"laptop", 1).err(),
            Some(NotAsked::Busy)
        );
        drop(asked);
        assert!(joiner.ask(b"workstation", b"laptop", 1).is_ok());
    }
}
