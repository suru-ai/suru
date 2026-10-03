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

/// How many joins a waiting Server may be told of and not yet have heard
/// before it is held not to be waiting.
const REACHES_QUEUED: usize = 16;

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

struct Asked<T> {
    /// The identity key of the Server the join was asked of, which alone may
    /// take it up.
    server_key: Vec<u8>,
    taker: oneshot::Sender<T>,
}

/// A connection a Server waits on. It stops waiting once this is dropped.
pub(crate) struct Waiting<T> {
    state: Arc<Mutex<State<T>>>,
    server_key: Vec<u8>,
    id: u64,
    /// The names of the joins asked of the Server, as they are asked.
    pub(crate) reaches: mpsc::Receiver<Vec<u8>>,
}

/// A join asked of a waiting Server. It is given up once this is dropped.
pub(crate) struct Asking<T> {
    state: Arc<Mutex<State<T>>>,
    name: Vec<u8>,
    /// The connection the Server takes the join up on, once it does.
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
    /// until what this answers is dropped.
    pub(crate) fn wait(&self, server_key: Vec<u8>) -> Waiting<T> {
        let (reach, reaches) = mpsc::channel(REACHES_QUEUED);
        let mut state = lock(&self.state);
        let id = state.next_waiting;
        state.next_waiting += 1;
        state
            .waiting
            .entry(server_key.clone())
            .or_default()
            .push(WaitingOn { id, reach });
        Waiting {
            state: self.state.clone(),
            server_key,
            id,
            reaches,
        }
    }

    /// Asks the Server whose identity key is `server_key` to take up a join,
    /// telling the connection it last began to wait on that has room to hear
    /// it: `None` where it waits on none that has.
    pub(crate) fn ask(&self, server_key: &[u8]) -> Option<Asking<T>> {
        let name = fresh_name();
        let (taker, taken_up) = oneshot::channel();
        let mut state = lock(&self.state);
        state
            .waiting
            .get(server_key)?
            .iter()
            .rev()
            .find(|waiting| waiting.reach.try_send(name.clone()).is_ok())?;
        state.asked.insert(
            name.clone(),
            Asked {
                server_key: server_key.to_vec(),
                taker,
            },
        );
        Some(Asking {
            state: self.state.clone(),
            name,
            taken_up,
        })
    }

    /// Takes up the join named `name` for the Server whose identity key is
    /// `server_key`: what hands its connection on, where that join was asked
    /// of that Server and is still asked. A join is taken up once.
    pub(crate) fn take_up(&self, name: &[u8], server_key: &[u8]) -> Option<oneshot::Sender<T>> {
        let mut state = lock(&self.state);
        if state.asked.get(name)?.server_key != server_key {
            return None;
        }
        state.asked.remove(name).map(|asked| asked.taker)
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
        assert!(joiner.ask(b"workstation").is_none());
        let mut first = joiner.wait(b"workstation".to_vec());
        let mut latest = joiner.wait(b"workstation".to_vec());

        let _asking = joiner.ask(b"workstation").expect("the Server waits");
        assert!(latest.reaches.recv().await.is_some());
        assert!(first.reaches.try_recv().is_err());

        drop(latest);
        let _asking = joiner.ask(b"workstation").expect("the Server waits still");
        assert!(first.reaches.recv().await.is_some());
        drop(first);
        assert!(joiner.ask(b"workstation").is_none());
    }

    #[tokio::test]
    async fn a_join_is_taken_up_once_by_the_server_it_was_asked_of_while_it_is_asked() {
        let joiner = Joiner::<&str>::new();
        let mut waiting = joiner.wait(b"workstation".to_vec());
        let mut asking = joiner.ask(b"workstation").unwrap();
        let name = waiting.reaches.recv().await.unwrap();
        assert!(joiner.take_up(&name, b"laptop").is_none());
        joiner
            .take_up(&name, b"workstation")
            .expect("the Server asked takes the join up")
            .send("connection")
            .unwrap();
        assert_eq!((&mut asking.taken_up).await, Ok("connection"));
        assert!(joiner.take_up(&name, b"workstation").is_none());

        let asking = joiner.ask(b"workstation").unwrap();
        let name = waiting.reaches.recv().await.unwrap();
        drop(asking);
        assert!(
            joiner.take_up(&name, b"workstation").is_none(),
            "a join given up is taken up by nobody"
        );
    }

    #[test]
    fn a_waiting_server_that_hears_nothing_is_held_not_to_be_waiting() {
        let joiner = Joiner::<()>::new();
        let _waiting = joiner.wait(b"workstation".to_vec());
        let asked = (0..REACHES_QUEUED)
            .map(|_| joiner.ask(b"workstation").expect("room to tell it"))
            .collect::<Vec<_>>();
        assert!(joiner.ask(b"workstation").is_none());
        drop(asked);
    }
}
