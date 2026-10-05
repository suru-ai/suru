//! The Secret Service a Linux desktop keeps its secrets in — GNOME Keyring,
//! KWallet, KeePassXC, whichever answers on the D-Bus session bus — as an
//! [`IdentityStore`]. It is spoken to through the `secret-service` crate,
//! over zbus, a D-Bus client written in Rust, so Suru links no libdbus, in a
//! session whose secrets are encrypted by Rust code rather than OpenSSL.
//!
//! An item is a secret whose attributes name Suru and the item's id. A put
//! keeps it in the default collection; a get and a delete find it in any.
//! Suru never asks to unlock anything: what is asked of a locked collection
//! or item counts as unavailable. A prompt the Secret Service brings up on
//! its own — KeePassXC asks before it keeps a new item — is waited on as
//! long as the call is, and no longer: the [`super::BoundedStore`] every
//! call goes through gives up on it, and the call itself is dropped in time,
//! so a prompt nobody answers holds up no Server. Nor is an item ever said
//! to be gone where a locked collection may keep it.

use std::{collections::HashMap, ffi::OsString, path::PathBuf, time::Duration};

use anyhow::{Context, anyhow};
use secret_service::{EncryptionType, Item, SecretService};
use zbus::{
    Connection,
    address::transport::{Transport, UnixSocket},
};

use super::{IdentityStore, ItemId, StoreUnavailable, Stored};

#[cfg(test)]
mod fake;

/// The attribute every item Suru keeps in the Secret Service carries, as it
/// names Suru's Server identity keys.
const APPLICATION: (&str, &str) = ("application", "ai.suru.server-identity");
/// The attribute naming an item's [`ItemId`].
const ITEM: &str = "item";
/// What the Secret Service is told an item's secret is: a key's bytes.
const CONTENT_TYPE: &str = "application/octet-stream";
/// How long a call waits on the Secret Service, where nothing says
/// otherwise, before it is dropped, connection and all: long after whoever
/// made it has given up on it, so a prompt nobody answers, or a bus that
/// never answers anything, holds the thread the call was made on no longer
/// than this.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The Secret Service on this user's D-Bus session bus. Each call connects
/// to the bus anew, on a runtime of its own on the thread it is called from
/// — never the Server's, which a call waiting on the Secret Service must
/// not hold up — so a call given up on holds nothing a later one needs. The
/// socket is connected without blocking, so where there is no session bus a
/// call fails at once, and where nothing answers it gives its thread back
/// after its give-up time.
pub(crate) struct SecretServiceStore {
    bus: Bus,
    give_up_after: Duration,
}

/// The D-Bus bus the Secret Service is asked on.
enum Bus {
    /// The user's session bus: where `DBUS_SESSION_BUS_ADDRESS` says, or
    /// else `$XDG_RUNTIME_DIR/bus`.
    Session,
    /// The bus at an address, in tests.
    #[cfg(test)]
    At(String),
    /// A peer that answers as the Secret Service itself, with no bus
    /// between, at a Unix socket, in tests: in plain sessions, as the
    /// crate's own tests cover encrypting them.
    #[cfg(test)]
    Peer(PathBuf),
}

impl SecretServiceStore {
    /// The Secret Service on this user's D-Bus session bus.
    pub(crate) fn new() -> Self {
        Self {
            bus: Bus::Session,
            give_up_after: GIVE_UP_AFTER,
        }
    }

    /// The store, asking the Secret Service on `bus`.
    #[cfg(test)]
    fn on(bus: Bus) -> Self {
        Self {
            bus,
            give_up_after: GIVE_UP_AFTER,
        }
    }

    /// The store, each call to which is dropped after `give_up_after`.
    #[cfg(test)]
    fn with_give_up_after(mut self, give_up_after: Duration) -> Self {
        self.give_up_after = give_up_after;
        self
    }

    /// How the session's secrets are sent: encrypted, but to the tests'
    /// peer.
    fn encryption(&self) -> EncryptionType {
        #[cfg(test)]
        if let Bus::Peer(_) = self.bus {
            return EncryptionType::Plain;
        }
        EncryptionType::Dh
    }

    /// What `call` answers of a session with the Secret Service, made on a
    /// runtime of its own until it answers or the give-up time has passed.
    /// Either way the runtime is shut down without waiting on anything the
    /// call left under way, so the thread is given back then.
    fn answered<T>(
        &self,
        call: impl AsyncFnOnce(&SecretService<'static>) -> Result<T, StoreUnavailable>,
    ) -> Result<T, StoreUnavailable> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime to call it on")?;
        // Timed on the runtime, which its timer needs.
        let answered = runtime.block_on(async {
            tokio::time::timeout(self.give_up_after, async {
                let bus = connect(&self.bus).await?;
                let service = SecretService::connect_with_existing(self.encryption(), bus)
                    .await
                    .map_err(unopened)?;
                call(&service).await
            })
            .await
        });
        runtime.shutdown_background();
        answered.unwrap_or_else(|_| {
            Err(anyhow!("it did not answer within {:?}", self.give_up_after).into())
        })
    }
}

impl IdentityStore for SecretServiceStore {
    /// Replaces an item the default collection keeps as `item` already, so
    /// putting it again keeps the one item.
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        self.answered(async |service| put(service, item, label, bytes).await)
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        self.answered(async |service| get(service, item).await)
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        self.answered(async |service| delete(service, item).await)
    }
}

/// A connection to the D-Bus bus `bus` names.
async fn connect(bus: &Bus) -> Result<Connection, StoreUnavailable> {
    let address = match bus {
        Bus::Session => zbus::Address::session()
            .context("could not tell where the D-Bus session bus is")?
            .to_string(),
        #[cfg(test)]
        Bus::At(address) => address.clone(),
        #[cfg(test)]
        Bus::Peer(socket) => {
            let socket = tokio::net::UnixStream::connect(socket)
                .await
                .context("could not connect to the Secret Service")?;
            return Ok(zbus::connection::Builder::unix_stream(socket)
                .p2p()
                .build()
                .await
                .context("could not connect to the Secret Service")?);
        }
    };
    connect_at(&address).await.map_err(|error| {
        anyhow!("{error:#}")
            .context(format!(
                "could not connect to the D-Bus session bus at {address}"
            ))
            .into()
    })
}

/// A connection to the D-Bus bus at `address`. Its socket is connected
/// without blocking, as zbus would not, so a call given up on drops it
/// however far it got.
async fn connect_at(address: &str) -> anyhow::Result<Connection> {
    let address: zbus::Address = address.parse()?;
    let socket = tokio::net::UnixStream::connect(socket_of(&address)?).await?;
    Ok(zbus::connection::Builder::unix_stream(socket)
        .build()
        .await?)
}

/// The Unix socket the bus at `address` listens on, an abstract one written
/// with a leading NUL, as tokio takes it.
fn socket_of(address: &zbus::Address) -> anyhow::Result<PathBuf> {
    if let Transport::Unix(unix) = address.transport() {
        match unix.path() {
            UnixSocket::File(path) => return Ok(path.clone()),
            UnixSocket::Abstract(name) => {
                let mut socket = OsString::from("\0");
                socket.push(name);
                return Ok(socket.into());
            }
            _ => {}
        }
    }
    Err(anyhow!(
        "Suru reaches a D-Bus session bus only over a Unix socket"
    ))
}

async fn put(
    service: &SecretService<'_>,
    item: &ItemId,
    label: &str,
    bytes: &[u8],
) -> Result<(), StoreUnavailable> {
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

async fn get(service: &SecretService<'_>, item: &ItemId) -> Result<Stored, StoreUnavailable> {
    let Some(found) = search(service, item).await?.readable()? else {
        return Ok(Stored::NoSuchItem);
    };
    Ok(Stored::Found(found.get_secret().await.map_err(failed)?))
}

async fn delete(service: &SecretService<'_>, item: &ItemId) -> Result<(), StoreUnavailable> {
    for found in search(service, item).await?.deletable()? {
        found.delete().await.map_err(failed)?;
    }
    Ok(())
}

/// What a search for `item` finds, and whether anything may have kept one
/// from its sight, before it or after: a collection that locks or unlocks
/// as Suru searches is seen one way or the other.
async fn search<'service>(
    service: &'service SecretService<'_>,
    item: &ItemId,
) -> Result<Search<Item<'service>>, StoreUnavailable> {
    let hidden_before = may_hide_items(service).await?;
    let item = item.to_string();
    let found = service
        .search_items(attributes(&item))
        .await
        .map_err(failed)?;
    let hidden_after = may_hide_items(service).await?;
    Ok(Search {
        unlocked: found.unlocked,
        locked: found.locked,
        maybe_hidden: hidden_before || hidden_after,
    })
}

/// Whether anything may keep an item from a search's sight: a collection
/// that is locked, which a Secret Service may not search, or there being no
/// default collection, as where the one items are kept in is closed.
async fn may_hide_items(service: &SecretService<'_>) -> Result<bool, StoreUnavailable> {
    match service.get_default_collection().await {
        Ok(_) => {}
        Err(secret_service::Error::NoResult) => return Ok(true),
        Err(error) => return Err(failed(error)),
    }
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

/// What a search for an item found: the items it found unlocked, the ones
/// it found locked, and whether anything may have kept one from its sight.
struct Search<T> {
    unlocked: Vec<T>,
    locked: Vec<T>,
    maybe_hidden: bool,
}

impl<T> Search<T> {
    /// The item to read, where the search found one unlocked — a put
    /// replaces the item in the default collection it is put in, so more
    /// than one is the same key, put again after the default moved — or
    /// none, where it can be sure the Secret Service keeps none. Telling a
    /// Server its item is gone while a locked collection may keep it would
    /// have its user give its identity up for lost.
    fn readable(mut self) -> Result<Option<T>, StoreUnavailable> {
        if !self.unlocked.is_empty() {
            return Ok(Some(self.unlocked.swap_remove(0)));
        }
        self.whole()?;
        Ok(None)
    }

    /// Every item to delete, where the search can be sure it found every
    /// one, and each unlocked.
    fn deletable(self) -> Result<Vec<T>, StoreUnavailable> {
        self.whole()?;
        Ok(self.unlocked)
    }

    /// Whether the search found every item there is, each unlocked.
    fn whole(&self) -> Result<(), StoreUnavailable> {
        if !self.locked.is_empty() {
            return Err(
                anyhow!("it keeps the item locked, and Suru does not ask to unlock it").into(),
            );
        }
        if self.maybe_hidden {
            return Err(anyhow!(
                "it may keep the item where a search cannot see it, as a collection of it was \
                 locked, or it had no default collection, while Suru searched"
            )
            .into());
        }
        Ok(())
    }
}

/// The Secret Service's failure, as why it is unavailable.
fn failed(error: secret_service::Error) -> StoreUnavailable {
    cause(error).into()
}

/// The failure to open a session with the Secret Service — there being no
/// Secret Service on the bus, above all — as why it is unavailable.
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
        secret_service::Error::Zbus(zbus::Error::MethodError(name, description, _)) => {
            refusal(name.as_str(), description.as_deref())
        }
        secret_service::Error::ZbusFdo(
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_),
        ) => refusal("org.freedesktop.DBus.Error.ServiceUnknown", None),
        // Its own words already carry what failed within it, which its
        // sources would only say again.
        error => anyhow!("{error}"),
    }
}

/// The D-Bus error `name`, with its `description`, in words a user can act
/// on where Suru knows them.
fn refusal(name: &str, description: Option<&str>) -> anyhow::Error {
    match (name, description) {
        (
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner",
            _,
        ) => anyhow!("no Secret Service is running on the D-Bus session bus"),
        ("org.freedesktop.Secret.Error.IsLocked", _) => {
            anyhow!("what was asked of it is locked, and Suru does not ask to unlock it")
        }
        (name, Some(description)) => anyhow!("{name}: {description}"),
        (name, None) => anyhow!("{name}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        fake::{Answer, FakeSecrets},
        *,
    };

    /// The store, asking `fake`.
    fn asking(fake: &FakeSecrets) -> SecretServiceStore {
        SecretServiceStore::on(Bus::Peer(fake.socket().to_owned()))
    }

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

    /// A bus is reached at its Unix socket, an abstract one as tokio takes
    /// it; a bus reached any other way is not reached.
    #[test]
    fn a_bus_is_reached_at_its_unix_socket() {
        let socket = |address: &str| socket_of(&address.parse().unwrap());
        assert_eq!(
            socket("unix:path=/run/user/1000/bus").unwrap(),
            PathBuf::from("/run/user/1000/bus")
        );
        assert_eq!(
            socket("unix:abstract=/tmp/dbus-suru").unwrap(),
            PathBuf::from("\0/tmp/dbus-suru")
        );
        let error = socket("tcp:host=localhost,port=4000").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Suru reaches a D-Bus session bus only over a Unix socket"
        );
    }

    /// A search's unlocked items are read and deleted where it can be sure
    /// it found them all; an item it found is read whatever else it found.
    #[test]
    fn a_search_answers_what_it_found_where_nothing_may_be_hidden() {
        let search =
            |unlocked: Vec<&'static str>, locked: Vec<&'static str>, maybe_hidden| Search {
                unlocked,
                locked,
                maybe_hidden,
            };
        assert_eq!(
            search(vec!["first", "second"], vec![], false)
                .readable()
                .unwrap(),
            Some("first")
        );
        assert_eq!(
            search(vec!["first", "second"], vec![], false)
                .deletable()
                .unwrap(),
            ["first", "second"]
        );
        assert_eq!(search(vec![], vec![], false).readable().unwrap(), None);
        assert!(
            search(vec![], vec![], false)
                .deletable()
                .unwrap()
                .is_empty()
        );
        for (locked, maybe_hidden) in [(vec![], true), (vec!["locked"], false)] {
            assert_eq!(
                search(vec!["unlocked"], locked, maybe_hidden)
                    .readable()
                    .unwrap(),
                Some("unlocked")
            );
        }
    }

    /// A locked item, or one a locked collection may hide, is never taken
    /// for no item at all, nor left behind by a delete said to succeed.
    #[test]
    fn a_search_that_may_have_missed_an_item_counts_as_unavailable() {
        let locked = "it keeps the item locked, and Suru does not ask to unlock it";
        let hidden = "it may keep the item where a search cannot see it, as a collection of it \
                      was locked, or it had no default collection, while Suru searched";
        let search =
            |unlocked: Vec<&'static str>, locked: Vec<&'static str>, maybe_hidden| Search {
                unlocked,
                locked,
                maybe_hidden,
            };
        let error = search(vec![], vec!["locked"], false)
            .readable()
            .unwrap_err();
        assert_eq!(error.to_string(), locked);
        let error = search(vec![], vec![], true).readable().unwrap_err();
        assert_eq!(error.to_string(), hidden);
        let error = search(vec!["unlocked"], vec!["locked"], false)
            .deletable()
            .unwrap_err();
        assert_eq!(error.to_string(), locked);
        let error = search(vec!["unlocked"], vec![], true)
            .deletable()
            .unwrap_err();
        assert_eq!(error.to_string(), hidden);
    }

    /// The Secret Service's failures say why it is unavailable, in words a
    /// user can act on where Suru knows them.
    #[test]
    fn a_failure_says_why_the_secret_service_is_unavailable() {
        let said = |error| failed(error).to_string();
        assert_eq!(
            said(secret_service::Error::Unavailable),
            "no D-Bus session bus, or no Secret Service on it, was found"
        );
        assert_eq!(
            said(secret_service::Error::Locked),
            "what was asked of it is locked, and Suru does not ask to unlock it"
        );
        assert_eq!(
            said(secret_service::Error::Prompt),
            "it brought up a prompt, which was dismissed"
        );
        assert_eq!(
            said(secret_service::Error::ZbusFdo(
                zbus::fdo::Error::ServiceUnknown("not activatable".to_owned())
            )),
            "no Secret Service is running on the D-Bus session bus"
        );
        assert_eq!(
            said(secret_service::Error::NoResult),
            "SS error: result not returned from SS API"
        );
        assert_eq!(
            format!("{:#}", unopened(secret_service::Error::Unavailable)),
            "could not open a session with it on the D-Bus session bus: no D-Bus session bus, or \
             no Secret Service on it, was found"
        );
        for name in [
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
        ] {
            assert_eq!(
                refusal(name, Some("The name is not activatable")).to_string(),
                "no Secret Service is running on the D-Bus session bus"
            );
        }
        assert_eq!(
            refusal("org.freedesktop.Secret.Error.IsLocked", None).to_string(),
            "what was asked of it is locked, and Suru does not ask to unlock it"
        );
        assert_eq!(
            refusal("org.freedesktop.DBus.Error.Failed", Some("it broke")).to_string(),
            "org.freedesktop.DBus.Error.Failed: it broke"
        );
    }

    /// An item put is kept, labelled and named as Suru's; got back as it
    /// was put; and gone once deleted, deleting it again being no error.
    #[test]
    fn an_item_put_is_got_back_and_deleted() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();

        assert_eq!(store.get(&item).unwrap(), Stored::NoSuchItem);
        store
            .put(&item, "Suru Server identity key", b"the key")
            .unwrap();
        let kept = fake.state().items();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].collection,
            "/org/freedesktop/secrets/collection/login"
        );
        assert_eq!(kept[0].label, "Suru Server identity key");
        assert_eq!(
            kept[0].attributes,
            HashMap::from([
                (
                    "application".to_owned(),
                    "ai.suru.server-identity".to_owned()
                ),
                ("item".to_owned(), item.to_string()),
            ])
        );
        assert_eq!(kept[0].secret, b"the key");
        assert_eq!(
            store.get(&item).unwrap(),
            Stored::Found(b"the key".to_vec())
        );
        assert_eq!(store.get(&ItemId::random()).unwrap(), Stored::NoSuchItem);

        store.delete(&item).unwrap();
        assert!(fake.state().items().is_empty());
        assert_eq!(store.get(&item).unwrap(), Stored::NoSuchItem);
        store.delete(&item).unwrap();
        assert_eq!(fake.state().prompts_shown, 0);
    }

    /// Putting an item again replaces it, keeping the one item.
    #[test]
    fn an_item_put_again_is_replaced() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        store.put(&item, "label", b"first").unwrap();
        store.put(&item, "label", b"second").unwrap();
        assert_eq!(fake.state().items().len(), 1);
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"second".to_vec()));
    }

    /// Nothing is asked of a default collection that is locked, so no
    /// unlock prompt comes up; nor is anything put where there is no
    /// default collection.
    #[test]
    fn nothing_is_put_where_the_default_collection_is_locked_or_missing() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        fake.state().lock_all(true);
        let error = store.put(&item, "label", b"key").unwrap_err();
        assert_eq!(
            error.to_string(),
            "its default collection is locked, and Suru does not ask to unlock it"
        );
        assert_eq!(fake.state().prompts_shown, 0);

        fake.state().lock_all(false);
        fake.state().default = None;
        let error = store.put(&item, "label", b"key").unwrap_err();
        assert_eq!(
            error.to_string(),
            "it has no default collection to keep the item in"
        );
        assert!(fake.state().items().is_empty());
    }

    /// A prompt the Secret Service brings up before it keeps or deletes an
    /// item, as KeePassXC does, is shown, and the item kept or deleted once
    /// its user goes ahead.
    #[test]
    fn a_prompt_before_a_put_or_a_delete_is_shown_and_gone_ahead_with() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        fake.state().prompts_on_create = true;
        fake.state().prompts_on_delete = true;

        store.put(&item, "label", b"key").unwrap();
        assert_eq!(fake.state().prompts_shown, 1);
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"key".to_vec()));
        store.delete(&item).unwrap();
        assert_eq!(fake.state().prompts_shown, 2);
        assert!(fake.state().items().is_empty());
    }

    /// A prompt its user dismisses counts as the store being unavailable,
    /// and leaves things as they were.
    #[test]
    fn a_prompt_dismissed_counts_as_unavailable() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        fake.state().prompts_on_create = true;
        fake.state().prompt_answer = Answer::Dismissed;
        let error = store.put(&item, "label", b"key").unwrap_err();
        assert_eq!(
            error.to_string(),
            "it brought up a prompt, which was dismissed"
        );
        assert!(fake.state().items().is_empty());
    }

    /// A prompt nobody answers holds a call no longer than its give-up
    /// time, giving back the thread it was made on; a later call is
    /// answered as ever.
    #[test]
    fn a_prompt_nobody_answers_is_given_up_on_in_time() {
        let fake = FakeSecrets::new();
        let store = asking(&fake).with_give_up_after(Duration::from_millis(500));
        let item = ItemId::random();
        fake.state().prompts_on_create = true;
        fake.state().prompt_answer = Answer::Never;

        let error = store.put(&item, "label", b"key").unwrap_err();
        assert_eq!(error.to_string(), "it did not answer within 500ms");
        assert_eq!(fake.state().prompts_shown, 1);
        assert!(fake.state().items().is_empty());

        fake.state().prompts_on_create = false;
        store.put(&item, "label", b"key").unwrap();
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"key".to_vec()));
    }

    /// An item kept in a locked collection is neither read nor deleted,
    /// with no unlock prompt, and never taken for no item, whether a search
    /// answers it as locked or leaves it out.
    #[test]
    fn an_item_a_locked_collection_keeps_is_never_taken_for_none() {
        for hides_locked_items in [false, true] {
            let fake = FakeSecrets::new();
            let store = asking(&fake);
            let item = ItemId::random();
            store.put(&item, "label", b"key").unwrap();
            fake.state().hides_locked_items = hides_locked_items;
            fake.state().lock_all(true);

            assert!(store.get(&item).is_err());
            assert!(store.delete(&item).is_err());
            assert_eq!(fake.state().items().len(), 1);
            assert_eq!(fake.state().prompts_shown, 0);

            fake.state().lock_all(false);
            assert_eq!(store.get(&item).unwrap(), Stored::Found(b"key".to_vec()));
        }
    }

    /// No item found while another collection is locked, which may keep
    /// it, or while there is no default collection, counts as unavailable,
    /// not as no item.
    #[test]
    fn no_item_found_while_one_may_be_hidden_counts_as_unavailable() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        let other = fake.state().add_collection("other");
        fake.state().hides_locked_items = true;
        fake.state().lock(&other, true);
        assert!(store.get(&item).is_err());
        assert!(store.delete(&item).is_err());

        fake.state().lock(&other, false);
        fake.state().default = None;
        assert!(store.get(&item).is_err());
    }

    /// A collection that hides its items while locked, and locks or
    /// unlocks as Suru searches, never has an item it keeps taken for none.
    #[test]
    fn a_collection_locking_or_unlocking_as_searched_never_loses_an_item() {
        let fake = FakeSecrets::new();
        let store = asking(&fake);
        let item = ItemId::random();
        store.put(&item, "label", b"key").unwrap();
        fake.state().hides_locked_items = true;

        fake.state().lock_all(true);
        fake.state().unlocks_as_searched = true;
        assert!(store.get(&item).is_err());
        fake.state().unlocks_as_searched = false;
        assert_eq!(store.get(&item).unwrap(), Stored::Found(b"key".to_vec()));

        fake.state().locks_as_searched = true;
        assert!(store.get(&item).is_err());
    }

    /// A call with no bus to connect to fails at once, saying where the bus
    /// was looked for.
    #[test]
    fn a_call_with_no_bus_to_connect_to_fails_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", directory.path().join("bus").display());
        let store = SecretServiceStore::on(Bus::At(address.clone()))
            .with_give_up_after(Duration::from_secs(60));

        let started = Instant::now();
        let error = store.get(&ItemId::random()).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("could not connect to the D-Bus session bus at {address}")
        );
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    /// A call to a bus whose backlog is full, whose socket a blocking
    /// connect would wait on, fails at once rather than wait.
    #[test]
    fn a_call_to_a_bus_whose_backlog_is_full_fails_at_once() {
        use socket2::{Domain, SockAddr, Socket, Type};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus");
        let socket = SockAddr::unix(&path).unwrap();
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&socket).unwrap();
        listener.listen(0).unwrap();
        let mut waiting = Vec::new();
        loop {
            let connecting = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
            connecting.set_nonblocking(true).unwrap();
            if connecting.connect(&socket).is_err() {
                break;
            }
            waiting.push(connecting);
            assert!(waiting.len() < 1024, "the backlog fills");
        }
        let address = format!("unix:path={}", path.display());
        let store = SecretServiceStore::on(Bus::At(address.clone()))
            .with_give_up_after(Duration::from_secs(60));

        let started = Instant::now();
        let error = store.get(&ItemId::random()).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("could not connect to the D-Bus session bus at {address}")
        );
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    /// A call to a bus that never answers is dropped after the store's
    /// give-up time, giving back the thread it was made on.
    #[test]
    fn a_call_a_bus_never_answers_gives_its_thread_back_in_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus");
        // Takes every connection into its backlog, and never says a word.
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let store = SecretServiceStore::on(Bus::At(format!("unix:path={}", path.display())))
            .with_give_up_after(Duration::from_millis(20));

        for _ in 0..3 {
            let error = store.get(&ItemId::random()).unwrap_err();
            assert_eq!(error.to_string(), "it did not answer within 20ms");
        }
    }

    /// Takes the item out of this machine's Secret Service when dropped,
    /// however the test holding it ends.
    struct Throwaway(ItemId);

    impl Drop for Throwaway {
        fn drop(&mut self) {
            let _ = SecretServiceStore::new().delete(&self.0);
        }
    }

    /// Puts, gets and deletes a throwaway item in this machine's Secret
    /// Service, on the user's session bus. Run it by hand on a Linux
    /// desktop with a Secret Service running and every collection of it
    /// unlocked — a locked one may hide an item, so Suru answers whether it
    /// keeps one as unavailable while any is — with `cargo nextest run
    /// --run-ignored only secret_service_store`. A Secret Service that asks
    /// before it keeps or deletes an item, as KeePassXC does, wants each
    /// prompt answered within the store's give-up time.
    #[test]
    #[ignore = "touches this machine's Secret Service"]
    fn the_secret_service_keeps_gives_up_and_deletes_a_throwaway_item() {
        let store = SecretServiceStore::new();
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
