//! The platform credential store a Server's identity key is kept in: an
//! [`IdentityStore`], which [`super::IdentityKey`] gets the key from and keeps
//! it in, and nothing else reads; each call to it bounded, so a store that
//! never answers counts as unavailable; and which store a new key is kept in.

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Condvar, Mutex, MutexGuard, atomic::AtomicBool},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

#[cfg(any(windows, test))]
mod credential_manager_store;
#[cfg(target_os = "linux")]
mod secret_service_store;

/// The platform credential store, as its user knows it.
#[cfg(target_os = "macos")]
pub(crate) const PLATFORM_STORE: &str = "the login keychain";
#[cfg(windows)]
pub(crate) const PLATFORM_STORE: &str = "Windows Credential Manager";
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const PLATFORM_STORE: &str = "the Secret Service keyring";

/// A place a Server's identity key is kept, item by item, each item bytes
/// under an [`ItemId`] the store gives no meaning to.
pub(crate) trait IdentityStore: Send + Sync {
    /// Keeps `bytes` as the item `item`, in place of whatever it was,
    /// labelled `label` wherever the store shows its user what it keeps.
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable>;

    /// What the store keeps as the item `item`.
    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable>;

    /// Keeps nothing as the item `item` from now on, whether or not it kept
    /// anything until now.
    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable>;
}

/// The id an [`IdentityStore`] keeps an item under: a random UUID made with
/// the item, naming nothing else, so no two data directories share an item
/// however alike they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct ItemId(Uuid);

impl ItemId {
    /// An id no item has yet.
    pub(crate) fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

/// What an [`IdentityStore`] keeps as an item, as it answers.
// Only the Secret Service and Credential Manager keep anything a Server
// asks for yet: elsewhere the platform's store is unavailable until Suru
// keeps keys there.
#[cfg_attr(not(any(test, target_os = "linux", windows)), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Found(Vec<u8>),
    NoSuchItem,
}

/// Why an [`IdentityStore`] could not answer: it is locked, not running, or
/// refusing this program, it took too long, or it failed as storage may.
#[derive(Debug)]
pub(crate) struct StoreUnavailable(anyhow::Error);

impl From<anyhow::Error> for StoreUnavailable {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for StoreUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// The platform credential store of the platform this Server runs on: the
/// Secret Service on Linux, Credential Manager on Windows. Suru keeps
/// nothing in any other platform's store yet, so there it counts as
/// unavailable, and a Server keeps a new key in its file instead.
pub(crate) fn platform_identity_store() -> Arc<dyn IdentityStore> {
    #[cfg(target_os = "linux")]
    return Arc::new(secret_service_store::SecretServiceStore::new());
    #[cfg(windows)]
    return Arc::new(credential_manager_store::CredentialManagerStore);
    #[cfg(not(any(target_os = "linux", windows)))]
    Arc::new(NoIdentityStore)
}

/// A platform credential store a Server does not use, every call to which
/// counts as unavailable.
pub(crate) struct NoIdentityStore;

impl NoIdentityStore {
    fn unavailable() -> StoreUnavailable {
        anyhow!("this Server keeps nothing in {PLATFORM_STORE}").into()
    }
}

impl IdentityStore for NoIdentityStore {
    fn put(&self, _item: &ItemId, _label: &str, _bytes: &[u8]) -> Result<(), StoreUnavailable> {
        Err(Self::unavailable())
    }

    fn get(&self, _item: &ItemId) -> Result<Stored, StoreUnavailable> {
        Err(Self::unavailable())
    }

    fn delete(&self, _item: &ItemId) -> Result<(), StoreUnavailable> {
        Err(Self::unavailable())
    }
}

/// The most calls a [`BoundedStore`] has under way at once, given up on or
/// not: enough that a call the store never answers leaves later ones free
/// to be answered, and few enough that a store answering nothing holds no
/// more threads than this.
const CALLS_AT_ONCE: usize = 4;

/// An identity store each call to which is given up on once it has taken
/// longer than its timeout, counting then as unavailable, so a keyring
/// asking for an unlock nobody answers holds a Server up no longer than
/// that.
///
/// The store's calls cannot be cut short, so each runs on a thread of its
/// own, which a call given up on leaves waiting for the store. A later call
/// starts all the same, on a thread of its own, and is answered however the
/// one given up on fares, so a call the store never answers stands in the
/// way of none after it. Only while [`CALLS_AT_ONCE`] calls have yet to
/// return does the next count as unavailable at once, starting nothing: a
/// store that never answers anything holds that many threads, however often
/// it is asked, and is asked again as they return.
pub(crate) struct BoundedStore {
    store: Arc<dyn IdentityStore>,
    timeout: Duration,
    /// How many calls are under way, given up on or not.
    under_way: Arc<AtomicUsize>,
}

impl BoundedStore {
    /// `store`, each call to which is given up on after `timeout`.
    pub(crate) fn new(store: Arc<dyn IdentityStore>, timeout: Duration) -> Self {
        Self {
            store,
            timeout,
            under_way: Arc::default(),
        }
    }

    /// What `call` answers of the store, where it answers within the
    /// timeout and fewer than [`CALLS_AT_ONCE`] calls are under way.
    fn call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn IdentityStore) -> Result<T, StoreUnavailable> + Send + 'static,
    ) -> Result<T, StoreUnavailable> {
        let started =
            self.under_way
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |under_way| {
                    (under_way < CALLS_AT_ONCE).then_some(under_way + 1)
                });
        if started.is_err() {
            return Err(anyhow!("it has yet to answer {CALLS_AT_ONCE} calls made before").into());
        }
        let under_way = CallUnderWay(Arc::clone(&self.under_way));
        let (answer, answered) = mpsc::sync_channel(1);
        let store = Arc::clone(&self.store);
        let started = thread::Builder::new()
            .name("suru-identity-store".to_owned())
            .spawn(move || {
                let answered = {
                    // Over before the answer is sent, so whoever reads the
                    // answer finds it over.
                    let _under_way = under_way;
                    call(store.as_ref())
                };
                let _ = answer.send(answered);
            });
        if let Err(error) = started {
            return Err(anyhow::Error::from(error)
                .context("could not start a call to it")
                .into());
        }
        match answered.recv_timeout(self.timeout) {
            Ok(answered) => answered,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(anyhow!("it did not answer within {:?}", self.timeout).into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("it failed without answering").into())
            }
        }
    }
}

/// A call to a [`BoundedStore`] under way, which is over once this is
/// dropped — however the call ends, or where its thread never started.
struct CallUnderWay(Arc<AtomicUsize>);

impl Drop for CallUnderWay {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl IdentityStore for BoundedStore {
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        let (item, label) = (*item, label.to_owned());
        // Wiped once the call is over, given up on or not.
        let bytes = Zeroizing::new(bytes.to_vec());
        self.call(move |store| store.put(&item, &label, &bytes))
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        let item = *item;
        self.call(move |store| store.get(&item))
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        let item = *item;
        self.call(move |store| store.delete(&item))
    }
}

/// A store a Server may keep its identity key in, as `SURU_IDENTITY_STORE`
/// names it: `system`, the platform credential store, or `file`, an
/// owner-only file in the Server's data directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityStoreChoice {
    System,
    File,
}

impl std::str::FromStr for IdentityStoreChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "system" => Ok(Self::System),
            "file" => Ok(Self::File),
            _ => Err(anyhow!(
                "the identity store is `system` or `file`, not {value:?}"
            )),
        }
    }
}

impl std::fmt::Display for IdentityStoreChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::System => "system",
            Self::File => "file",
        })
    }
}

/// Which store a Server keeps its identity key in, by what chose it: the
/// store a new key is made into, and whether a key kept in the file is moved
/// into the platform credential store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// A release build, which keeps it in the platform credential store.
    ReleaseBuild,
    /// A debug build, which keeps it in the file: every build is a program
    /// the platform credential store has never seen, and would ask its user
    /// about.
    DebugBuild,
    /// A release build Cargo runs — by `cargo run`, `cargo test` or `cargo
    /// nextest run` — which keeps it in the file as a debug build does: it
    /// too is a program the platform credential store has never seen, and a
    /// test's Server keeps nothing in the store of the machine it runs on.
    CargoRun,
    /// `SURU_IDENTITY_STORE`, naming the store whatever the build.
    Chosen(IdentityStoreChoice),
}

impl Selection {
    /// The selection of this build as it runs, where `chosen`, as
    /// `SURU_IDENTITY_STORE` names a store, does not override it. Cargo sets
    /// `CARGO_MANIFEST_DIR` for whatever it runs, which those programs pass
    /// on to theirs, and a Suru installed and run as it ships never has it.
    pub(crate) fn for_this_build(chosen: Option<IdentityStoreChoice>) -> Self {
        Self::for_build(
            cfg!(debug_assertions),
            std::env::var_os("CARGO_MANIFEST_DIR").is_some(),
            chosen,
        )
    }

    /// The selection of a debug build, or a release one, run by Cargo or
    /// not, where `chosen`, as `SURU_IDENTITY_STORE` names a store, does not
    /// override it.
    pub(crate) fn for_build(
        debug_build: bool,
        run_by_cargo: bool,
        chosen: Option<IdentityStoreChoice>,
    ) -> Self {
        match chosen {
            Some(chosen) => Self::Chosen(chosen),
            None if debug_build => Self::DebugBuild,
            None if run_by_cargo => Self::CargoRun,
            None => Self::ReleaseBuild,
        }
    }

    /// The store it selects.
    pub(crate) fn store(self) -> IdentityStoreChoice {
        match self {
            Self::ReleaseBuild => IdentityStoreChoice::System,
            Self::DebugBuild | Self::CargoRun => IdentityStoreChoice::File,
            Self::Chosen(chosen) => chosen,
        }
    }

    /// Why a key is kept in the store it selects, as the Log says.
    pub(crate) fn why(self) -> String {
        match self {
            Self::ReleaseBuild => "release builds keep it there".to_owned(),
            Self::DebugBuild => {
                "debug builds keep it there unless SURU_IDENTITY_STORE=system".to_owned()
            }
            Self::CargoRun => {
                "builds Cargo runs keep it there unless SURU_IDENTITY_STORE=system".to_owned()
            }
            Self::Chosen(chosen) => format!("SURU_IDENTITY_STORE={chosen} keeps it there"),
        }
    }
}

/// Each item a [`FakeIdentityStore`] keeps, by its label and bytes.
#[cfg(test)]
type FakeItems = HashMap<ItemId, (String, Vec<u8>)>;

/// An identity store held in memory for tests, which can be made to answer
/// as a store that is unavailable would, or not to answer at all.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeIdentityStore {
    items: Mutex<FakeItems>,
    unavailable: AtomicBool,
    /// Whether the store answers nothing, and what is told as that ends.
    held: (Mutex<bool>, Condvar),
    /// How many calls the store has been asked, answered or not.
    calls: AtomicUsize,
    /// What the next call waits on, where it is to wait.
    stalled: Mutex<Option<mpsc::Receiver<()>>>,
    /// How many more calls the store answers before it locks, where it is
    /// to lock.
    locking: Mutex<Option<usize>>,
    /// Whether the store keeps something other than what it is put.
    mangling: AtomicBool,
}

/// A call to a [`FakeIdentityStore`] left unanswered, until this is dropped.
#[cfg(test)]
pub(crate) struct Stalled {
    _released: mpsc::Sender<()>,
}

/// A [`FakeIdentityStore`] answering nothing, until this is dropped.
#[cfg(test)]
pub(crate) struct Held<'store>(&'store FakeIdentityStore);

#[cfg(test)]
impl Drop for Held<'_> {
    fn drop(&mut self) {
        let (held, ended) = &self.0.held;
        *held.lock().expect("identity store hold is not poisoned") = false;
        ended.notify_all();
    }
}

#[cfg(test)]
impl FakeIdentityStore {
    /// Makes the store answer as it is, or as an unavailable store would.
    pub(crate) fn set_available(&self, available: bool) {
        self.unavailable.store(!available, Ordering::SeqCst);
    }

    /// Holds every call to the store, answering none, until what this
    /// answers is dropped: as a keyring waiting on an unlock nobody answers
    /// does.
    pub(crate) fn hold(&self) -> Held<'_> {
        *self
            .held
            .0
            .lock()
            .expect("identity store hold is not poisoned") = true;
        Held(self)
    }

    /// Leaves the next call to the store unanswered until what this answers
    /// is dropped, the calls after it answered as ever: as an unlock prompt
    /// nobody answers holds up the call that brought it up, and no other.
    pub(crate) fn stall_next(&self) -> Stalled {
        let (released, release) = mpsc::channel();
        *self
            .stalled
            .lock()
            .expect("identity store stall is not poisoned") = Some(release);
        Stalled {
            _released: released,
        }
    }

    /// Answers the next `calls` calls as it is, and every one after as an
    /// unavailable store would until made available again: as a keyring
    /// that locks partway through what it is asked does.
    pub(crate) fn lock_after(&self, calls: usize) {
        *self
            .locking
            .lock()
            .expect("identity store lock is not poisoned") = Some(calls);
    }

    /// Makes the store keep something other than what it is put from now
    /// on, as a store that mangles what it keeps does.
    pub(crate) fn mangle(&self) {
        self.mangling.store(true, Ordering::SeqCst);
    }

    /// How many calls the store has been asked, answered or not.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every item the store keeps, available or not.
    pub(crate) fn contents(&self) -> HashMap<ItemId, Vec<u8>> {
        self.items
            .lock()
            .expect("identity store lock is not poisoned")
            .iter()
            .map(|(item, (_, bytes))| (*item, bytes.clone()))
            .collect()
    }

    /// What the item `item` is labelled, where the store keeps it.
    pub(crate) fn label(&self, item: &ItemId) -> Option<String> {
        self.items
            .lock()
            .expect("identity store lock is not poisoned")
            .get(item)
            .map(|(label, _)| label.clone())
    }

    /// The items the store keeps, once it answers, while it is available.
    fn answering(&self) -> Result<MutexGuard<'_, FakeItems>, StoreUnavailable> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let stalled = self
            .stalled
            .lock()
            .expect("identity store stall is not poisoned")
            .take();
        if let Some(stalled) = stalled {
            let _ = stalled.recv();
        }
        let (held, ended) = &self.held;
        drop(
            ended
                .wait_while(
                    held.lock().expect("identity store hold is not poisoned"),
                    |held| *held,
                )
                .expect("identity store hold is not poisoned"),
        );
        {
            let mut locking = self
                .locking
                .lock()
                .expect("identity store lock is not poisoned");
            match locking.as_mut() {
                Some(0) => {
                    self.set_available(false);
                    *locking = None;
                }
                Some(left) => *left -= 1,
                None => {}
            }
        }
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(anyhow!("the identity store is unavailable").into());
        }
        Ok(self
            .items
            .lock()
            .expect("identity store lock is not poisoned"))
    }
}

#[cfg(test)]
impl IdentityStore for FakeIdentityStore {
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        let mut items = self.answering()?;
        let kept = if self.mangling.load(Ordering::SeqCst) {
            bytes.iter().map(|byte| !byte).collect()
        } else {
            bytes.to_vec()
        };
        items.insert(*item, (label.to_owned(), kept));
        Ok(())
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        Ok(match self.answering()?.get(item) {
            Some((_, bytes)) => Stored::Found(bytes.clone()),
            None => Stored::NoSuchItem,
        })
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        self.answering()?.remove(item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_store_keeps_items_apart_and_answers_nothing_while_unavailable() {
        let store = FakeIdentityStore::default();
        let (first, second) = (ItemId::random(), ItemId::random());
        store.put(&first, "label", b"first").unwrap();
        assert_eq!(store.get(&second).unwrap(), Stored::NoSuchItem);

        store.set_available(false);
        assert!(store.get(&first).is_err());
        assert!(store.put(&second, "label", b"second").is_err());
        assert!(store.delete(&first).is_err());

        store.set_available(true);
        assert_eq!(store.get(&first).unwrap(), Stored::Found(b"first".to_vec()));
        assert_eq!(store.get(&second).unwrap(), Stored::NoSuchItem);
        store.delete(&first).unwrap();
        assert_eq!(store.get(&first).unwrap(), Stored::NoSuchItem);
    }

    /// The fake store can lock partway through what it is asked, and can
    /// keep something other than what it is put.
    #[test]
    fn the_fake_store_locks_after_so_many_calls_and_mangles_what_it_is_put() {
        let store = FakeIdentityStore::default();
        let item = ItemId::random();
        store.lock_after(1);
        store.put(&item, "label", b"kept").unwrap();
        assert!(store.get(&item).is_err());
        assert!(store.get(&item).is_err());
        store.set_available(true);
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"kept".to_vec()));

        store.mangle();
        store.put(&item, "label", b"kept").unwrap();
        let Stored::Found(kept) = store.get(&item).unwrap() else {
            panic!("the store keeps the item");
        };
        assert_ne!(kept, b"kept");
    }

    /// A bounded store answers as the store it bounds does, while that
    /// answers in time.
    #[test]
    fn a_bounded_store_answers_as_its_store_does() {
        let fake = Arc::new(FakeIdentityStore::default());
        let store = BoundedStore::new(fake.clone(), Duration::from_secs(10));
        let item = ItemId::random();

        store.put(&item, "label", b"kept").unwrap();
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"kept".to_vec()));
        store.delete(&item).unwrap();
        assert_eq!(store.get(&item).unwrap(), Stored::NoSuchItem);

        fake.set_available(false);
        assert!(store.get(&item).is_err());
    }

    /// A call the store does not answer within the timeout counts as
    /// unavailable, and stands in the way of none after it: a later call is
    /// answered though that one never returns.
    #[test]
    fn a_call_the_store_never_answers_is_given_up_on_and_stands_in_the_way_of_none() {
        let fake = Arc::new(FakeIdentityStore::default());
        let store = BoundedStore::new(fake.clone(), Duration::from_millis(200));
        let item = ItemId::random();
        fake.put(&item, "label", b"kept").unwrap();

        let stalled = fake.stall_next();
        let error = store.get(&item).unwrap_err();
        assert_eq!(error.to_string(), "it did not answer within 200ms");
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"kept".to_vec()));
        drop(stalled);
    }

    /// While as many calls as a bounded store makes at once have yet to
    /// return, the next counts as unavailable at once, starting nothing; as
    /// they return, the store is asked again.
    #[test]
    fn a_store_answering_nothing_holds_no_more_than_so_many_calls() {
        let fake = Arc::new(FakeIdentityStore::default());
        let store = BoundedStore::new(fake.clone(), Duration::from_millis(20));
        let item = ItemId::random();
        fake.put(&item, "label", b"kept").unwrap();
        let asked = fake.calls();

        let held = fake.hold();
        for _ in 0..CALLS_AT_ONCE {
            let error = store.get(&item).unwrap_err();
            assert_eq!(error.to_string(), "it did not answer within 20ms");
        }
        // Each call given up on reaches the store on its own thread, in its
        // own time.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fake.calls() - asked < CALLS_AT_ONCE {
            assert!(
                std::time::Instant::now() < deadline,
                "the calls given up on reach the store"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let error = store.get(&item).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("it has yet to answer {CALLS_AT_ONCE} calls made before")
        );
        assert_eq!(fake.calls() - asked, CALLS_AT_ONCE, "nothing more is asked");

        drop(held);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let answered = loop {
            match store.get(&item) {
                Ok(answered) => break answered,
                Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("the store is never asked again: {error}"),
            }
        };
        assert_eq!(answered, Stored::Found(b"kept".to_vec()));
    }

    /// Release builds keep a new key in the platform credential store, and
    /// debug builds and builds Cargo runs, whatever their profile, in the
    /// file; `SURU_IDENTITY_STORE` overrides any of them.
    #[test]
    fn the_build_selects_the_store_and_suru_identity_store_overrides_it() {
        use IdentityStoreChoice::{File, System};

        assert_eq!(Selection::for_build(false, false, None).store(), System);
        assert_eq!(Selection::for_build(true, false, None).store(), File);
        assert_eq!(Selection::for_build(false, true, None).store(), File);
        assert_eq!(Selection::for_build(true, true, None).store(), File);
        assert_eq!(
            Selection::for_build(false, true, None).why(),
            "builds Cargo runs keep it there unless SURU_IDENTITY_STORE=system"
        );
        for debug_build in [false, true] {
            for run_by_cargo in [false, true] {
                for chosen in [System, File] {
                    assert_eq!(
                        Selection::for_build(debug_build, run_by_cargo, Some(chosen)).store(),
                        chosen
                    );
                }
            }
        }

        assert_eq!("system".parse::<IdentityStoreChoice>().unwrap(), System);
        assert_eq!("file".parse::<IdentityStoreChoice>().unwrap(), File);
        for chosen in [System, File] {
            assert_eq!(
                chosen.to_string().parse::<IdentityStoreChoice>().unwrap(),
                chosen
            );
        }
        let error = "keychain".parse::<IdentityStoreChoice>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "the identity store is `system` or `file`, not \"keychain\""
        );
    }

    /// This build, run by Cargo as every test is, keeps a new key in the
    /// file whatever its profile — a debug build, or a release one Cargo
    /// runs — so no test's Server keeps one in the store of the machine it
    /// runs on unless `SURU_IDENTITY_STORE` chooses it.
    #[test]
    fn a_build_cargo_runs_selects_the_file_whatever_its_profile() {
        let selected = Selection::for_this_build(None);
        let expected = if cfg!(debug_assertions) {
            Selection::DebugBuild
        } else {
            Selection::CargoRun
        };
        assert_eq!(selected, expected);
        assert_eq!(selected.store(), IdentityStoreChoice::File);
    }
}
