//! The login keychain, the platform credential store a Server keeps its
//! identity key in on macOS (ADR-0050).
//!
//! Each item is a generic password in the user's login keychain — the
//! file-based keychain, which needs no entitlements — kept under the service
//! [`SERVICE`] with the item's id as its account, and labelled as it is put,
//! so its user can find it in Keychain Access. Putting an item the keychain
//! already keeps replaces its bytes and its label.
//!
//! The keychain is left to ask its user what it would ask on behalf of any
//! program: to unlock it where it is locked, or to let a build it has yet to
//! trust use an item. With nobody it can ask — a Mac reached over SSH with
//! nobody logged in at its screen — it refuses at once, and with nobody
//! answering, the call is given up on at its timeout (see
//! [`super::BoundedStore`]), so a Server never waits on a prompt either way;
//! a user at the screen can unlock it, and the next use of the key finds it.
//! Barring the keychain from asking would leave that user nothing to answer,
//! and a build the keychain has yet to trust no way to be trusted — and it
//! can only be barred for the whole process, and the rest of the Security
//! framework the Server uses besides.

use anyhow::anyhow;
use core_foundation::data::CFData;
use security_framework::{
    base::Error,
    item::{
        ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions,
        ItemUpdateValue, Location, SearchResult, update_item,
    },
    os::macos::keychain::SecKeychain,
};

use super::{IdentityStore, ItemId, StoreUnavailable, Stored};

/// The service every item is kept under, which Keychain Access shows as
/// where the item is used.
const SERVICE: &str = "ai.suru.server-identity";

/// The login keychain, by the name Keychain Services finds it by among the
/// user's own keychains, whatever its file is called on this macOS.
const LOGIN_KEYCHAIN: &str = "login.keychain";

// Keychain Services' result codes, as `SecBase.h` names them.
const ERR_SEC_USER_CANCELED: i32 = -128;
const ERR_SEC_WRITE_PERMISSIONS: i32 = -61;
const ERR_SEC_NOT_AVAILABLE: i32 = -25291;
const ERR_SEC_READ_ONLY: i32 = -25292;
const ERR_SEC_AUTH_FAILED: i32 = -25293;
const ERR_SEC_NO_SUCH_KEYCHAIN: i32 = -25294;
const ERR_SEC_INVALID_KEYCHAIN: i32 = -25295;
const ERR_SEC_DUPLICATE_ITEM: i32 = -25299;
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_SEC_INTERACTION_REQUIRED: i32 = -25315;

/// The user's login keychain, as an [`super::IdentityStore`].
pub(crate) struct LoginKeychain;

impl LoginKeychain {
    /// The login keychain, which is found only once it is asked something.
    fn keychain() -> Result<SecKeychain, StoreUnavailable> {
        SecKeychain::open(LOGIN_KEYCHAIN).map_err(unavailable)
    }

    /// A search of `keychain` for the item `item`, and nothing else.
    fn search(keychain: SecKeychain, item: &ItemId) -> ItemSearchOptions {
        let mut search = ItemSearchOptions::new();
        search
            .keychains(&[keychain])
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .account(&item.to_string());
        search
    }
}

impl IdentityStore for LoginKeychain {
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        let keychain = Self::keychain()?;
        let mut adding = ItemAddOptions::new(ItemAddValue::Data {
            class: ItemClass::generic_password(),
            data: CFData::from_buffer(bytes),
        });
        adding
            .set_location(Location::FileKeychain(keychain.clone()))
            .set_service(SERVICE)
            .set_account_name(item.to_string())
            .set_label(label);
        match adding.add() {
            Err(error) if error.code() == ERR_SEC_DUPLICATE_ITEM => {
                let mut replacing = ItemUpdateOptions::new();
                replacing
                    .set_value(ItemUpdateValue::Data(CFData::from_buffer(bytes)))
                    .set_label(label);
                update_item(&Self::search(keychain, item), &replacing).map_err(unavailable)
            }
            added => added.map_err(unavailable),
        }
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        let mut search = Self::search(Self::keychain()?, item);
        stored(search.load_data(true).search())
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        deleted(Self::search(Self::keychain()?, item).delete())
    }
}

/// What a search for an item answered: the item's bytes, or that the
/// keychain keeps no such item.
fn stored(found: Result<Vec<SearchResult>, Error>) -> Result<Stored, StoreUnavailable> {
    match found {
        Ok(found) => match found.into_iter().next() {
            Some(SearchResult::Data(bytes)) => Ok(Stored::Found(bytes)),
            _ => Err(anyhow!("it answered with something other than the item's bytes").into()),
        },
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(Stored::NoSuchItem),
        Err(error) => Err(unavailable(error)),
    }
}

/// What deleting an item answered: an item the keychain did not keep is
/// deleted all the same.
fn deleted(deleted: Result<(), Error>) -> Result<(), StoreUnavailable> {
    match deleted {
        Err(error) if error.code() != ERR_SEC_ITEM_NOT_FOUND => Err(unavailable(error)),
        _ => Ok(()),
    }
}

/// Why the login keychain could not answer, by the code it failed with, and
/// as it says itself.
fn unavailable(error: Error) -> StoreUnavailable {
    let why = match error.code() {
        ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_INTERACTION_REQUIRED => {
            "it is locked, or would ask whether Suru may use the item, and nobody can be asked here"
        }
        ERR_SEC_USER_CANCELED => {
            "asking to unlock it, or whether Suru may use the item, was cancelled"
        }
        ERR_SEC_AUTH_FAILED => "it refused Suru the item",
        ERR_SEC_NO_SUCH_KEYCHAIN | ERR_SEC_INVALID_KEYCHAIN => {
            "this user has no login keychain Suru can open"
        }
        ERR_SEC_NOT_AVAILABLE => "no keychain is available",
        ERR_SEC_READ_ONLY | ERR_SEC_WRITE_PERMISSIONS => "it cannot be written to",
        _ => "it failed",
    };
    let said = error
        .message()
        .unwrap_or_else(|| "it gave no reason".to_owned());
    anyhow!("{why}: {said} (OSStatus {})", error.code()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes of the item a search finds are the item; a search finding
    /// none answers that the keychain keeps no such item.
    #[test]
    fn a_search_finds_the_item_or_no_such_item() {
        assert_eq!(
            stored(Ok(vec![SearchResult::Data(b"key".to_vec())])).unwrap(),
            Stored::Found(b"key".to_vec())
        );
        assert_eq!(
            stored(Err(Error::from_code(ERR_SEC_ITEM_NOT_FOUND))).unwrap(),
            Stored::NoSuchItem
        );
        let error = stored(Ok(Vec::new())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "it answered with something other than the item's bytes"
        );
    }

    /// Deleting an item the keychain does not keep is no failure; deleting
    /// one from a keychain that cannot answer is.
    #[test]
    fn deleting_an_item_the_keychain_does_not_keep_succeeds() {
        deleted(Ok(())).unwrap();
        deleted(Err(Error::from_code(ERR_SEC_ITEM_NOT_FOUND))).unwrap();
        let error = deleted(Err(Error::from_code(ERR_SEC_INTERACTION_NOT_ALLOWED))).unwrap_err();
        assert!(error.to_string().starts_with("it is locked"), "{error}");
    }

    /// A keychain that is locked, or that would ask whether Suru may use the
    /// item, with nobody to ask — as over SSH with nobody logged in at the
    /// Mac's screen — counts as unavailable, and says so.
    #[test]
    fn a_locked_keychain_with_nobody_to_ask_counts_as_unavailable() {
        for code in [
            ERR_SEC_INTERACTION_NOT_ALLOWED,
            ERR_SEC_INTERACTION_REQUIRED,
        ] {
            let error = unavailable(Error::from_code(code)).to_string();
            assert!(
                error.starts_with(
                    "it is locked, or would ask whether Suru may use the item, and nobody can \
                     be asked here: "
                ),
                "{error}"
            );
            assert!(error.ends_with(&format!(" (OSStatus {code})")), "{error}");
        }
    }

    /// Each way the keychain can turn Suru away is told apart, with what the
    /// keychain itself says of it.
    #[test]
    fn each_refusal_says_why() {
        for (code, why) in [
            (
                ERR_SEC_USER_CANCELED,
                "asking to unlock it, or whether Suru may use the item, was cancelled",
            ),
            (ERR_SEC_AUTH_FAILED, "it refused Suru the item"),
            (
                ERR_SEC_NO_SUCH_KEYCHAIN,
                "this user has no login keychain Suru can open",
            ),
            (
                ERR_SEC_INVALID_KEYCHAIN,
                "this user has no login keychain Suru can open",
            ),
            (ERR_SEC_NOT_AVAILABLE, "no keychain is available"),
            (ERR_SEC_READ_ONLY, "it cannot be written to"),
            (ERR_SEC_WRITE_PERMISSIONS, "it cannot be written to"),
            (-36, "it failed"),
        ] {
            let error = unavailable(Error::from_code(code)).to_string();
            let message = Error::from_code(code)
                .message()
                .expect("the keychain describes its codes");
            assert_eq!(error, format!("{why}: {message} (OSStatus {code})"));
        }
    }

    /// What the login keychain labels the item `item`, where it keeps it.
    fn label_of(item: &ItemId) -> Option<String> {
        let mut search = LoginKeychain::search(LoginKeychain::keychain().unwrap(), item);
        match search.load_attributes(true).search() {
            Ok(found) => found
                .first()
                .and_then(SearchResult::simplify_dict)
                .and_then(|attributes| attributes.get("labl").cloned()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => None,
            Err(error) => panic!("the login keychain answers: {error}"),
        }
    }

    /// An item in the login keychain, deleted from it once this is dropped,
    /// however the test holding it ends.
    struct Throwaway(ItemId);

    impl Drop for Throwaway {
        fn drop(&mut self) {
            if let Err(error) = LoginKeychain.delete(&self.0) {
                eprintln!("the throwaway item {} is left behind: {error}", self.0);
            }
        }
    }

    /// The store a Server on macOS is given keeps a throwaway item in this
    /// user's real login keychain, labelled as it is put, gives it back,
    /// replaces it, and deletes it. It touches the real keychain, so it runs
    /// only by hand, on a Mac whose login keychain is unlocked:
    /// `cargo nextest run --lib --run-ignored only login_keychain`.
    #[test]
    #[ignore = "touches this user's real login keychain"]
    fn the_login_keychain_keeps_gives_back_replaces_and_deletes_an_item() {
        let store = crate::serving::platform_identity_store();
        let item = Throwaway(ItemId::random());
        let label = format!("Suru Server identity key (smoke test, {})", item.0);

        store.put(&item.0, &label, b"first").unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(b"first".to_vec())
        );
        assert_eq!(label_of(&item.0), Some(label.clone()));

        let relabelled = format!("{label}, replaced");
        store.put(&item.0, &relabelled, b"second").unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(b"second".to_vec())
        );
        assert_eq!(label_of(&item.0), Some(relabelled));

        store.delete(&item.0).unwrap();
        assert_eq!(store.get(&item.0).unwrap(), Stored::NoSuchItem);
        assert_eq!(label_of(&item.0), None);
        store.delete(&item.0).unwrap();
    }
}
