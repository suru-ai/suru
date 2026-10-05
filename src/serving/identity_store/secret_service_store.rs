//! The Secret Service a Linux desktop keeps its secrets in — GNOME Keyring,
//! KWallet, KeePassXC, whichever answers on the D-Bus session bus — as an
//! [`IdentityStore`]. It is spoken to by a D-Bus client written in Rust, so
//! Suru links no libdbus, over a session whose secrets are encrypted by Rust
//! code rather than OpenSSL.
//!
//! An item is a secret in the Secret Service whose attributes name Suru and
//! the item's id. A put keeps it in the default collection; a get and a
//! delete find it in any. Nothing here ever asks to unlock a collection or
//! an item: where what is asked of the Secret Service needs something
//! unlocked, it counts as unavailable. A collection that locks between Suru
//! finding it unlocked and putting an item in it, or deleting one from it,
//! can still bring up an unlock prompt, which the [`super::BoundedStore`]
//! every call goes through gives up on, and which the call itself gives up
//! on in time.

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, anyhow};
use secret_service::{EncryptionType, Item, SearchItemsResult, SecretService};

use super::{IdentityStore, ItemId, StoreUnavailable, Stored};

/// The attribute every item Suru keeps in the Secret Service carries, as it
/// names Suru's Server identity keys.
const APPLICATION: (&str, &str) = ("application", "ai.suru.server-identity");
/// The attribute naming an item's [`ItemId`].
const ITEM: &str = "item";
/// What the Secret Service is told an item's secret is: a key's bytes.
const CONTENT_TYPE: &str = "application/octet-stream";
/// How long a call waits on the Secret Service before it is dropped,
/// connection and all: long after whoever made it has given up on it, so a
/// prompt nobody answers, or a bus that never answers anything, holds the
/// thread the call was made on no longer than this.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The Secret Service on this user's D-Bus session bus. Each call connects
/// to it anew, on a runtime of its own on the thread it is called from —
/// never the Server's, which a call waiting on the Secret Service must not
/// hold up — so a call given up on holds nothing a later one needs, and
/// gives its thread back after [`GIVE_UP_AFTER`] at most. Where there is no
/// session bus to connect to, a call fails at once.
pub(crate) struct SecretServiceStore;

impl IdentityStore for SecretServiceStore {
    /// Replaces an item the default collection keeps as `item` already, so
    /// putting it again keeps the one item.
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        answered(put(item, label, bytes))
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        answered(get(item))
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        answered(delete(item))
    }
}

/// What `call` answers, run on a runtime of its own until it answers or
/// [`GIVE_UP_AFTER`] has passed.
fn answered<T>(
    call: impl Future<Output = Result<T, StoreUnavailable>>,
) -> Result<T, StoreUnavailable> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start a runtime to call it on")?
        // Timed on the runtime, which its timer needs.
        .block_on(async { tokio::time::timeout(GIVE_UP_AFTER, call).await })
        .unwrap_or_else(|_| Err(anyhow!("it did not answer within {GIVE_UP_AFTER:?}").into()))
}

async fn put(item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
    let service = connect().await?;
    let collection = match service.get_default_collection().await {
        Ok(collection) => collection,
        Err(secret_service::Error::NoResult) => {
            return Err(anyhow!("it has no default collection to keep the item in").into());
        }
        Err(error) => return Err(failed(error)),
    };
    if collection.is_locked().await.map_err(failed)? {
        return Err(anyhow!(
            "its default collection is locked, and Suru does not ask to unlock it"
        )
        .into());
    }
    let item = item.to_string();
    collection
        .create_item(label, attributes(&item), bytes, true, CONTENT_TYPE)
        .await
        .map_err(failed)?;
    Ok(())
}

async fn get(item: &ItemId) -> Result<Stored, StoreUnavailable> {
    let service = connect().await?;
    let found = matches(&service, item).await?;
    // A put replaces the item in the default collection it is put in, so
    // more than one is the same key, put again after the default moved.
    match found.first() {
        Some(found) => Ok(Stored::Found(found.get_secret().await.map_err(failed)?)),
        None => Ok(Stored::NoSuchItem),
    }
}

async fn delete(item: &ItemId) -> Result<(), StoreUnavailable> {
    let service = connect().await?;
    for found in matches(&service, item).await? {
        found.delete().await.map_err(failed)?;
    }
    Ok(())
}

/// A session with the Secret Service, whose secrets are encrypted between
/// it and Suru.
async fn connect() -> Result<SecretService<'static>, StoreUnavailable> {
    SecretService::connect(EncryptionType::Dh)
        .await
        .map_err(unopened)
}

/// Every item the Secret Service keeps as `item`, in any collection, where
/// it can tell them all without anything being unlocked.
async fn matches<'service>(
    service: &'service SecretService<'_>,
    item: &ItemId,
) -> Result<Vec<Item<'service>>, StoreUnavailable> {
    let item = item.to_string();
    let found = service
        .search_items(attributes(&item))
        .await
        .map_err(failed)?;
    let a_collection_locked = found.unlocked.is_empty()
        && found.locked.is_empty()
        && a_collection_is_locked(service).await?;
    every_match(found, a_collection_locked)
}

/// Whether any collection the Secret Service keeps is locked.
async fn a_collection_is_locked(service: &SecretService<'_>) -> Result<bool, StoreUnavailable> {
    for collection in service.get_all_collections().await.map_err(failed)? {
        if collection.is_locked().await.map_err(failed)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The attributes the item `item` is kept under, as its id is written.
fn attributes(item: &str) -> HashMap<&str, &str> {
    HashMap::from([APPLICATION, (ITEM, item)])
}

/// Every item a search found, where none it found is locked and, where it
/// found none, no collection is (`a_collection_locked`) that may keep one
/// the search could not find: a locked item gives up nothing, and is not
/// deleted, without an unlock prompt, and a Secret Service may search no
/// collection while it is locked. Telling a Server its item is gone while
/// a locked collection may keep it would have its user give it up for
/// lost.
fn every_match<T>(
    found: SearchItemsResult<T>,
    a_collection_locked: bool,
) -> Result<Vec<T>, StoreUnavailable> {
    if !found.locked.is_empty() {
        return Err(anyhow!("it keeps the item locked, and Suru does not ask to unlock it").into());
    }
    if found.unlocked.is_empty() && a_collection_locked {
        return Err(anyhow!(
            "a collection of it is locked, which may keep the item, and Suru does not ask to \
             unlock it"
        )
        .into());
    }
    Ok(found.unlocked)
}

/// The Secret Service's failure, as why it is unavailable.
fn failed(error: secret_service::Error) -> StoreUnavailable {
    cause(error).into()
}

/// The failure to open a session with the Secret Service — there being no
/// session bus, above all, or no Secret Service on it — as why it is
/// unavailable.
fn unopened(error: secret_service::Error) -> StoreUnavailable {
    cause(error)
        .context("could not open a session with it on the D-Bus session bus")
        .into()
}

/// What the Secret Service failed at, in words a user can act on where Suru
/// knows them.
fn cause(error: secret_service::Error) -> anyhow::Error {
    match error {
        secret_service::Error::Unavailable => {
            anyhow!("no D-Bus session bus, or no Secret Service on it, was found")
        }
        secret_service::Error::Locked => {
            anyhow!("what was asked of it is locked, and Suru does not ask to unlock it")
        }
        secret_service::Error::Prompt => anyhow!("it brought up a prompt, which was dismissed"),
        // Its own words already carry what failed within it, which its
        // sources would only say again.
        error => anyhow!("{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each item is kept under attributes naming Suru's Server identity
    /// keys and the item's id, so no other item, Suru's or not, answers a
    /// search for it.
    #[test]
    fn an_item_is_kept_under_suru_and_its_id() {
        let item = ItemId::random().to_string();
        assert_eq!(
            attributes(&item),
            HashMap::from([
                ("application", "ai.suru.server-identity"),
                ("item", item.as_str()),
            ])
        );
    }

    /// A search's unlocked items are what it found, where none is locked.
    #[test]
    fn a_search_finding_only_unlocked_items_answers_them() {
        let found = SearchItemsResult {
            unlocked: vec!["first", "second"],
            locked: vec![],
        };
        assert_eq!(every_match(found, false).unwrap(), ["first", "second"]);
    }

    /// A search finding nothing, while no collection is locked, answers
    /// that there is no such item.
    #[test]
    fn a_search_finding_nothing_while_nothing_is_locked_answers_no_items() {
        let found = SearchItemsResult::<&str> {
            unlocked: vec![],
            locked: vec![],
        };
        assert!(every_match(found, false).unwrap().is_empty());
    }

    /// A locked item counts as unavailable, whatever else the search found,
    /// rather than be unlocked or left out.
    #[test]
    fn a_search_finding_a_locked_item_counts_as_unavailable() {
        for unlocked in [vec![], vec!["unlocked"]] {
            let found = SearchItemsResult {
                unlocked,
                locked: vec!["locked"],
            };
            let error = every_match(found, false).unwrap_err();
            assert_eq!(
                error.to_string(),
                "it keeps the item locked, and Suru does not ask to unlock it"
            );
        }
    }

    /// A search finding nothing while a collection is locked counts as
    /// unavailable, as the locked collection may keep the item; one finding
    /// an unlocked item answers it whatever is locked.
    #[test]
    fn a_search_finding_nothing_while_a_collection_is_locked_counts_as_unavailable() {
        let found = SearchItemsResult::<&str> {
            unlocked: vec![],
            locked: vec![],
        };
        let error = every_match(found, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a collection of it is locked, which may keep the item, and Suru does not ask to \
             unlock it"
        );

        let found = SearchItemsResult {
            unlocked: vec!["unlocked"],
            locked: vec![],
        };
        assert_eq!(every_match(found, true).unwrap(), ["unlocked"]);
    }

    /// The Secret Service's failures say why it is unavailable, in words a
    /// user can act on where Suru knows them.
    #[test]
    fn a_failure_says_why_the_secret_service_is_unavailable() {
        assert_eq!(
            failed(secret_service::Error::Unavailable).to_string(),
            "no D-Bus session bus, or no Secret Service on it, was found"
        );
        assert_eq!(
            failed(secret_service::Error::Locked).to_string(),
            "what was asked of it is locked, and Suru does not ask to unlock it"
        );
        assert_eq!(
            failed(secret_service::Error::Prompt).to_string(),
            "it brought up a prompt, which was dismissed"
        );
        assert_eq!(
            format!("{:#}", failed(secret_service::Error::NoResult)),
            "SS error: result not returned from SS API"
        );
    }

    /// A session that could not be opened says so, before why.
    #[test]
    fn a_session_not_opened_says_so() {
        assert_eq!(
            format!("{:#}", unopened(secret_service::Error::Unavailable)),
            "could not open a session with it on the D-Bus session bus: no D-Bus session bus, or \
             no Secret Service on it, was found"
        );
    }

    /// Takes the item out of this machine's Secret Service when dropped,
    /// however the test holding it ends.
    struct Throwaway(ItemId);

    impl Drop for Throwaway {
        fn drop(&mut self) {
            let _ = SecretServiceStore.delete(&self.0);
        }
    }

    /// Puts, gets and deletes a throwaway item in this machine's Secret
    /// Service. Run it by hand on a Linux desktop whose default collection
    /// is unlocked: `cargo nextest run --run-ignored only
    /// secret_service_store`.
    #[test]
    #[ignore = "touches this machine's Secret Service"]
    fn the_secret_service_keeps_gives_up_and_deletes_a_throwaway_item() {
        let store = SecretServiceStore;
        let item = Throwaway(ItemId::random());
        let label = "Suru smoke test item (safe to delete)";

        assert_eq!(store.get(&item.0).unwrap(), Stored::NoSuchItem);
        store.put(&item.0, label, b"first").unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(b"first".to_vec())
        );
        store.put(&item.0, label, b"second").unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(b"second".to_vec())
        );
        store.delete(&item.0).unwrap();
        assert_eq!(store.get(&item.0).unwrap(), Stored::NoSuchItem);
        store.delete(&item.0).unwrap();
    }
}
