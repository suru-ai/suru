//! Where a Server's identity key is kept: an [`IdentityStore`], which
//! [`super::IdentityKey`] gets the key from and keeps it in, and nothing else
//! reads.

#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::runtime::{protect_current_user_file, replace_private_file};

/// The owner-only file in a data directory that is its [`FileIdentityStore`].
const IDENTITY_FILE: &str = "server-identity.pk8";

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

/// The id an [`IdentityStore`] keeps an item under: which item it is, and
/// nothing more.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ItemId(String);

impl From<&str> for ItemId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

/// What an [`IdentityStore`] keeps as an item, as it answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Found(Vec<u8>),
    NoSuchItem,
}

/// Why an [`IdentityStore`] could not answer: it is locked, not running, or
/// refusing this program, or it failed as storage may.
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

/// The identity store that is a data directory's owner-only identity file.
/// It keeps one item, whatever id it is asked by: a data directory has one
/// identity, and the file's place says whose it is, so no label is kept.
pub(crate) struct FileIdentityStore {
    path: PathBuf,
}

impl FileIdentityStore {
    pub(crate) fn in_data_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(IDENTITY_FILE),
        }
    }
}

impl IdentityStore for FileIdentityStore {
    fn put(&self, _item: &ItemId, _label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        replace_private_file(&self.path, bytes)
            .with_context(|| format!("publish Server identity {:?}", self.path))?;
        Ok(())
    }

    fn get(&self, _item: &ItemId) -> Result<Stored, StoreUnavailable> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                protect_current_user_file(&self.path)?;
                Ok(Stored::Found(bytes))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Stored::NoSuchItem),
            Err(error) => Err(error)
                .with_context(|| format!("read Server identity {:?}", self.path))
                .map_err(StoreUnavailable),
        }
    }

    fn delete(&self, _item: &ItemId) -> Result<(), StoreUnavailable> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("delete Server identity {:?}", self.path))
                .map_err(StoreUnavailable),
        }
    }
}

/// An identity store held in memory for tests, which can be made to answer
/// as a store that is unavailable would.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeIdentityStore {
    items: Mutex<HashMap<ItemId, Vec<u8>>>,
    unavailable: AtomicBool,
}

#[cfg(test)]
impl FakeIdentityStore {
    /// Makes the store answer as it is, or as an unavailable store would.
    pub(crate) fn set_available(&self, available: bool) {
        self.unavailable.store(!available, Ordering::SeqCst);
    }

    /// The items the store keeps, while it is available.
    fn items(&self) -> Result<MutexGuard<'_, HashMap<ItemId, Vec<u8>>>, StoreUnavailable> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("the identity store is unavailable").into());
        }
        Ok(self
            .items
            .lock()
            .expect("identity store lock is not poisoned"))
    }
}

#[cfg(test)]
impl IdentityStore for FakeIdentityStore {
    fn put(&self, item: &ItemId, _label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        self.items()?.insert(item.clone(), bytes.to_vec());
        Ok(())
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        Ok(match self.items()?.get(item) {
            Some(bytes) => Stored::Found(bytes.clone()),
            None => Stored::NoSuchItem,
        })
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        self.items()?.remove(item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity file a data directory has kept since before identity
    /// stores: its key is the item the file store finds, whatever it is
    /// asked by.
    #[test]
    fn the_file_store_finds_the_key_a_data_directory_already_keeps() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("server-identity.pk8"), b"kept key").unwrap();
        let store = FileIdentityStore::in_data_dir(directory.path());
        assert_eq!(
            store.get(&ItemId::from("any")).unwrap(),
            Stored::Found(b"kept key".to_vec())
        );
    }

    #[test]
    fn the_file_store_keeps_an_item_in_the_owner_only_identity_file_until_it_is_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let store = FileIdentityStore::in_data_dir(directory.path());
        let item = ItemId::from("server-identity");
        assert_eq!(store.get(&item).unwrap(), Stored::NoSuchItem);

        store.put(&item, "label", b"first").unwrap();
        store.put(&item, "label", b"second").unwrap();
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"second".to_vec()));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"second",
            "kept as the bytes alone"
        );
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            1,
            "nothing is left beside it"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        store.delete(&item).unwrap();
        assert!(!path.exists());
        assert_eq!(store.get(&item).unwrap(), Stored::NoSuchItem);
        store
            .delete(&item)
            .expect("deleting what is not kept is no failure");
    }

    #[test]
    fn the_fake_store_keeps_items_apart_and_answers_nothing_while_unavailable() {
        let store = FakeIdentityStore::default();
        let (first, second) = (ItemId::from("first"), ItemId::from("second"));
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
}
