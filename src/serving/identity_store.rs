//! The platform credential store a Server's identity key is kept in: an
//! [`IdentityStore`], which [`super::IdentityKey`] gets the key from and keeps
//! it in, and nothing else reads; each call to it bounded, so a store that
//! never answers counts as unavailable; and which store a new key is kept in.

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    // Only a key moving from one store to another is deleted, and none moves
    // yet.
    #[cfg_attr(not(test), allow(dead_code))]
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
// No store a Server asks keeps anything yet: the platform's is unavailable
// everywhere until Suru keeps keys there.
#[cfg_attr(not(test), allow(dead_code))]
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

/// The platform credential store of the platform this Server runs on. Suru
/// keeps nothing in any platform's store yet, so it counts as unavailable
/// everywhere, and a Server keeps a new key in its file instead.
pub(crate) fn platform_identity_store() -> Arc<dyn IdentityStore> {
    Arc::new(NoIdentityStore)
}

/// A platform credential store Suru does not use, every call to which
/// counts as unavailable.
pub(crate) struct NoIdentityStore;

impl NoIdentityStore {
    fn unavailable() -> StoreUnavailable {
        anyhow!("Suru keeps nothing in {PLATFORM_STORE} yet").into()
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

/// An identity store each call to which is given up on once it has taken
/// longer than its timeout, counting then as unavailable, so a keyring
/// asking for an unlock nobody answers holds a Server up no longer than
/// that.
///
/// The store's calls cannot be cut short, so each runs on a thread of its
/// own, which a call given up on leaves waiting for the store. While one
/// such call has yet to return, every later call counts as unavailable at
/// once rather than starting another: a store that never answers holds one
/// thread, however often it is asked.
pub(crate) struct BoundedStore {
    store: Arc<dyn IdentityStore>,
    timeout: Duration,
    /// Whether a call is under way, given up on or not.
    calling: Arc<AtomicBool>,
}

impl BoundedStore {
    /// `store`, each call to which is given up on after `timeout`.
    pub(crate) fn new(store: Arc<dyn IdentityStore>, timeout: Duration) -> Self {
        Self {
            store,
            timeout,
            calling: Arc::default(),
        }
    }

    /// What `call` answers of the store, where it answers within the
    /// timeout and no call given up on is still waiting for it.
    fn call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn IdentityStore) -> Result<T, StoreUnavailable> + Send + 'static,
    ) -> Result<T, StoreUnavailable> {
        if self.calling.swap(true, Ordering::AcqRel) {
            return Err(anyhow!("it has yet to answer an earlier call").into());
        }
        let (answer, answered) = mpsc::sync_channel(1);
        let store = Arc::clone(&self.store);
        let calling = CallUnderWay(Arc::clone(&self.calling));
        let started = thread::Builder::new()
            .name("suru-identity-store".to_owned())
            .spawn(move || {
                let answered = {
                    // Over before the answer is sent, so whoever reads the
                    // answer may call again at once.
                    let _calling = calling;
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
struct CallUnderWay(Arc<AtomicBool>);

impl Drop for CallUnderWay {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl IdentityStore for BoundedStore {
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        let (item, label, bytes) = (*item, label.to_owned(), bytes.to_vec());
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

/// Which store a Server keeps a new identity key in, by what chose it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// A release build, which keeps it in the platform credential store.
    ReleaseBuild,
    /// A debug build, which keeps it in the file: every build is a program
    /// the platform credential store has never seen, and would ask its user
    /// about.
    DebugBuild,
    /// `SURU_IDENTITY_STORE`, naming the store whatever the build.
    Chosen(IdentityStoreChoice),
}

impl Selection {
    /// The selection of a debug build, or a release one, where `chosen`, as
    /// `SURU_IDENTITY_STORE` names a store, does not override it.
    pub(crate) fn for_build(debug_build: bool, chosen: Option<IdentityStoreChoice>) -> Self {
        match chosen {
            Some(chosen) => Self::Chosen(chosen),
            None if debug_build => Self::DebugBuild,
            None => Self::ReleaseBuild,
        }
    }

    /// The store it selects.
    pub(crate) fn store(self) -> IdentityStoreChoice {
        match self {
            Self::ReleaseBuild => IdentityStoreChoice::System,
            Self::DebugBuild => IdentityStoreChoice::File,
            Self::Chosen(chosen) => chosen,
        }
    }

    /// Why a new key is kept in the store it selects, as the Log says.
    pub(crate) fn why(self) -> String {
        match self {
            Self::ReleaseBuild => "release builds keep it there".to_owned(),
            Self::DebugBuild => {
                "debug builds keep it there unless SURU_IDENTITY_STORE=system".to_owned()
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
    /// Held while the store answers nothing.
    held: Mutex<()>,
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
    pub(crate) fn hold(&self) -> MutexGuard<'_, ()> {
        self.held
            .lock()
            .expect("identity store hold is not poisoned")
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
        drop(self.hold());
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
        self.answering()?
            .insert(*item, (label.to_owned(), bytes.to_vec()));
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

    /// A store that does not answer within the timeout counts as
    /// unavailable, and so does every call made while that one has yet to
    /// return, without waiting on the store again; once it returns, the
    /// store is asked again.
    #[test]
    fn a_store_that_does_not_answer_in_time_is_given_up_on() {
        let fake = Arc::new(FakeIdentityStore::default());
        let store = BoundedStore::new(fake.clone(), Duration::from_millis(20));
        let item = ItemId::random();
        fake.put(&item, "label", b"kept").unwrap();

        let held = fake.hold();
        let error = store.get(&item).unwrap_err();
        assert!(
            error.to_string().starts_with("it did not answer within"),
            "{error}"
        );
        let error = store.get(&item).unwrap_err();
        assert_eq!(error.to_string(), "it has yet to answer an earlier call");

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

    /// Release builds keep a new key in the platform credential store and
    /// debug builds in the file, and `SURU_IDENTITY_STORE` overrides
    /// either.
    #[test]
    fn the_build_selects_the_store_and_suru_identity_store_overrides_it() {
        use IdentityStoreChoice::{File, System};

        assert_eq!(Selection::for_build(false, None).store(), System);
        assert_eq!(Selection::for_build(true, None).store(), File);
        for debug_build in [false, true] {
            for chosen in [System, File] {
                assert_eq!(
                    Selection::for_build(debug_build, Some(chosen)).store(),
                    chosen
                );
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
}
