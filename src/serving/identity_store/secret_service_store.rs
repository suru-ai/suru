//! The Secret Service a Linux desktop keeps its secrets in — GNOME Keyring,
//! KWallet, KeePassXC, whichever answers on the D-Bus session bus — as an
//! [`IdentityStore`]. Suru speaks the Secret Service's D-Bus API itself,
//! over zbus, a D-Bus client written in Rust, so it links no libdbus; and it
//! encrypts the secrets it sends and receives in a Diffie-Hellman session,
//! in Rust rather than OpenSSL.
//!
//! An item is a secret whose attributes name Suru and the item's id. A put
//! keeps it in the default collection; a get and a delete find it in any.
//! Nothing here ever brings up a prompt: Suru asks nothing of a collection
//! it finds locked, and where the Secret Service answers with a prompt all
//! the same — to unlock a collection that locked as Suru asked, say — Suru
//! dismisses it without showing it, and the call counts as unavailable. Nor
//! is an item ever said to be gone where a locked collection may keep it.

use std::{collections::HashMap, ffi::OsString, path::PathBuf, time::Duration};

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, anyhow};
use hkdf::Hkdf;
use num_bigint::BigUint;
use sha2::Sha256;
use zbus::{
    Connection,
    address::transport::{Transport, UnixSocket},
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type, Value},
};

use super::{IdentityStore, ItemId, StoreUnavailable, Stored};

#[cfg(test)]
mod fake;

/// The bus name the Secret Service answers at.
const SECRETS: &str = "org.freedesktop.secrets";
/// The Secret Service's own object.
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const LABEL_PROPERTY: &str = "org.freedesktop.Secret.Item.Label";
const ATTRIBUTES_PROPERTY: &str = "org.freedesktop.Secret.Item.Attributes";
/// The object path the Secret Service answers with where it names nothing:
/// no prompt, no collection.
const NOTHING: &str = "/";
/// The session Suru opens: Diffie-Hellman over RFC 2409's second Oakley
/// group, its shared secret through HKDF-SHA256 to an AES-128 key, which
/// encrypts each secret in CBC mode with PKCS#7 padding.
const ALGORITHM: &str = "dh-ietf1024-sha256-aes128-cbc-pkcs7";
/// The prime of RFC 2409's second Oakley group, whose generator is 2.
const PRIME: [u8; 128] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x37, 0xED, 0x6B, 0x0B, 0xFF, 0x5C, 0xB6, 0xF4, 0x06, 0xB7, 0xED,
    0xEE, 0x38, 0x6B, 0xFB, 0x5A, 0x89, 0x9F, 0xA5, 0xAE, 0x9F, 0x24, 0x11, 0x7C, 0x4B, 0x1F, 0xE6,
    0x49, 0x28, 0x66, 0x51, 0xEC, 0xE6, 0x53, 0x81, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];

/// The attribute every item Suru keeps in the Secret Service carries, as it
/// names Suru's Server identity keys.
const APPLICATION: (&str, &str) = ("application", "ai.suru.server-identity");
/// The attribute naming an item's [`ItemId`].
const ITEM_ATTRIBUTE: &str = "item";
/// What the Secret Service is told an item's secret is: a key's bytes.
const CONTENT_TYPE: &str = "application/octet-stream";
/// How long a call waits on the Secret Service, where nothing says
/// otherwise, before it is dropped, connection and all: long after whoever
/// made it has given up on it, so a bus that never answers anything holds
/// the thread the call was made on no longer than this.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The Secret Service on this user's D-Bus session bus. Each call connects
/// to the bus anew, on a runtime of its own on the thread it is called from
/// — never the Server's, which a call waiting on the Secret Service must
/// not hold up — so a call given up on holds nothing a later one needs. The
/// socket is connected without blocking, so where there is no session bus a
/// call fails at once, and where the bus never answers it gives its thread
/// back after its give-up time.
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
    At(zbus::Address),
}

impl SecretServiceStore {
    /// The Secret Service on this user's D-Bus session bus.
    pub(crate) fn new() -> Self {
        Self {
            bus: Bus::Session,
            give_up_after: GIVE_UP_AFTER,
        }
    }

    /// The Secret Service on the bus at `address`.
    #[cfg(test)]
    fn at(address: &str) -> Self {
        Self {
            bus: Bus::At(address.parse().expect("a D-Bus address")),
            give_up_after: GIVE_UP_AFTER,
        }
    }

    /// The store, each call to which is dropped after `give_up_after`.
    #[cfg(test)]
    fn with_give_up_after(mut self, give_up_after: Duration) -> Self {
        self.give_up_after = give_up_after;
        self
    }

    /// What `call` answers of the Secret Service on the bus, made on a
    /// runtime of its own until it answers or the give-up time has passed.
    /// Either way the runtime is shut down without waiting on anything the
    /// call left under way, so the thread is given back then.
    fn answered<T>(
        &self,
        call: impl AsyncFnOnce(&Connection) -> Result<T, StoreUnavailable>,
    ) -> Result<T, StoreUnavailable> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime to call it on")?;
        // Timed on the runtime, which its timer needs.
        let answered = runtime.block_on(async {
            tokio::time::timeout(self.give_up_after, async {
                let bus = connect(&self.bus).await?;
                call(&bus).await
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
        self.answered(async |bus| put(bus, item, label, bytes).await)
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        self.answered(async |bus| get(bus, item).await)
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        self.answered(async |bus| delete(bus, item).await)
    }
}

/// A connection to the D-Bus bus `bus` names. Its socket is connected
/// without blocking, as zbus would not, so a call given up on drops it
/// however far it got.
async fn connect(bus: &Bus) -> Result<Connection, StoreUnavailable> {
    let address = match bus {
        Bus::Session => {
            zbus::Address::session().context("could not tell where the D-Bus session bus is")?
        }
        #[cfg(test)]
        Bus::At(address) => address.clone(),
    };
    let unconnected = || format!("could not connect to the D-Bus session bus at {address}");
    let socket = tokio::net::UnixStream::connect(socket_of(&address)?)
        .await
        .with_context(unconnected)?;
    Ok(zbus::connection::Builder::unix_stream(socket)
        .build()
        .await
        .with_context(unconnected)?)
}

/// The Unix socket the bus at `address` listens on, an abstract one written
/// with a leading NUL, as tokio takes it.
fn socket_of(address: &zbus::Address) -> Result<PathBuf, StoreUnavailable> {
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
    Err(
        anyhow!("Suru reaches a D-Bus session bus only over a Unix socket, not at {address}")
            .into(),
    )
}

async fn put(
    bus: &Connection,
    item: &ItemId,
    label: &str,
    bytes: &[u8],
) -> Result<(), StoreUnavailable> {
    let Some(collection) = default_collection(bus).await? else {
        return Err(anyhow!("it has no default collection to keep the item in").into());
    };
    if locked(bus, &collection).await? {
        return Err(anyhow!(
            "its default collection is locked, and Suru does not ask to unlock it"
        )
        .into());
    }
    let session = Session::open(bus).await?;
    let item = item.to_string();
    let properties = HashMap::from([
        (LABEL_PROPERTY, Value::from(label)),
        (ATTRIBUTES_PROPERTY, Value::from(attributes(&item))),
    ]);
    let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = call(
        bus,
        &collection,
        COLLECTION,
        "CreateItem",
        &(properties, session.encrypt(bytes)?, true),
    )
    .await?;
    refuse_prompt(bus, &prompt).await
}

async fn get(bus: &Connection, item: &ItemId) -> Result<Stored, StoreUnavailable> {
    let Some(found) = search(bus, item).await?.readable()? else {
        return Ok(Stored::NoSuchItem);
    };
    let session = Session::open(bus).await?;
    let secret: Secret = call(bus, &found, ITEM_INTERFACE, "GetSecret", &(&session.path,)).await?;
    Ok(Stored::Found(session.decrypt(secret)?))
}

async fn delete(bus: &Connection, item: &ItemId) -> Result<(), StoreUnavailable> {
    for found in search(bus, item).await?.deletable()? {
        let prompt: OwnedObjectPath = call(bus, &found, ITEM_INTERFACE, "Delete", &()).await?;
        refuse_prompt(bus, &prompt).await?;
    }
    Ok(())
}

/// What the method `method` of `interface`, on the Secret Service's object
/// at `path`, answers given `body`.
async fn call<R>(
    bus: &Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &(impl serde::Serialize + DynamicType),
) -> Result<R, StoreUnavailable>
where
    R: serde::de::DeserializeOwned + Type,
{
    let reply = bus
        .call_method(Some(SECRETS), path, Some(interface), method, body)
        .await
        .map_err(refused)?;
    Ok(reply
        .body()
        .deserialize()
        .with_context(|| format!("it answered {method} with what Suru cannot read"))?)
}

/// The value of the property `property` of `interface`, on the Secret
/// Service's object at `path`.
async fn property<T>(
    bus: &Connection,
    path: &str,
    interface: &str,
    property: &str,
) -> Result<T, StoreUnavailable>
where
    T: TryFrom<OwnedValue, Error: std::error::Error + Send + Sync + 'static>,
{
    let value: OwnedValue = call(bus, path, PROPERTIES, "Get", &(interface, property)).await?;
    Ok(T::try_from(value).with_context(|| format!("it gave {property} as what it cannot be"))?)
}

/// Whether the collection at `collection` is locked.
async fn locked(bus: &Connection, collection: &str) -> Result<bool, StoreUnavailable> {
    property(bus, collection, COLLECTION, "Locked").await
}

/// The collection the Secret Service keeps new items in, where it has one.
async fn default_collection(bus: &Connection) -> Result<Option<String>, StoreUnavailable> {
    let collection: OwnedObjectPath =
        call(bus, SERVICE_PATH, SERVICE, "ReadAlias", &("default",)).await?;
    Ok((collection.as_str() != NOTHING).then(|| collection.as_str().to_owned()))
}

/// Whether anything may keep an item from a search's sight: a collection
/// that is locked, which a Secret Service may not search, or there being no
/// default collection, as where the one items are kept in is closed.
async fn may_hide_items(bus: &Connection) -> Result<bool, StoreUnavailable> {
    if default_collection(bus).await?.is_none() {
        return Ok(true);
    }
    let collections: Vec<OwnedObjectPath> =
        property(bus, SERVICE_PATH, SERVICE, "Collections").await?;
    for collection in collections {
        if locked(bus, collection.as_str()).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a search for `item` finds, and whether anything may have kept one
/// from its sight, before it or after: a collection that locks or unlocks
/// as Suru searches is seen one way or the other.
async fn search(bus: &Connection, item: &ItemId) -> Result<Search<String>, StoreUnavailable> {
    let hidden_before = may_hide_items(bus).await?;
    let item = item.to_string();
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = call(
        bus,
        SERVICE_PATH,
        SERVICE,
        "SearchItems",
        &(attributes(&item),),
    )
    .await?;
    let hidden_after = may_hide_items(bus).await?;
    let paths = |found: Vec<OwnedObjectPath>| {
        found
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect()
    };
    Ok(Search {
        unlocked: paths(unlocked),
        locked: paths(locked),
        maybe_hidden: hidden_before || hidden_after,
    })
}

/// Where the Secret Service answered with a prompt rather than doing what
/// it was asked, dismisses the prompt — which shows only once Suru asks it
/// to, which it never does — and counts the call as unavailable.
async fn refuse_prompt(bus: &Connection, prompt: &OwnedObjectPath) -> Result<(), StoreUnavailable> {
    if prompt.as_str() == NOTHING {
        return Ok(());
    }
    // What was asked is left undone whether or not the prompt is dismissed,
    // as it is with the connection where not before.
    let _ = call::<()>(bus, prompt.as_str(), PROMPT, "Dismiss", &()).await;
    Err(anyhow!(
        "it would do what was asked only after a prompt, which Suru does not bring up; a \
         collection of it may be locked"
    )
    .into())
}

/// The attributes the item `item` is kept under, as its id is written.
fn attributes(item: &str) -> HashMap<&str, &str> {
    HashMap::from([APPLICATION, (ITEM_ATTRIBUTE, item)])
}

/// What a search for an item found: the items it found unlocked, the ones
/// it found locked, and whether anything may have kept one from its sight.
struct Search<T> {
    unlocked: Vec<T>,
    locked: Vec<T>,
    maybe_hidden: bool,
}

impl<T> Search<T> {
    /// The item to read, where the search found one unlocked; or none,
    /// where it can be sure the Secret Service keeps none. Telling a Server
    /// its item is gone while a locked collection may keep it would have
    /// its user give its identity up for lost.
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

/// Why the Secret Service, or the bus it is on, refused a call, in words a
/// user can act on where Suru knows them.
fn refused(error: zbus::Error) -> StoreUnavailable {
    match &error {
        zbus::Error::MethodError(name, description, _) => {
            refusal(name.as_str(), description.as_deref())
        }
        _ => anyhow!("{error}"),
    }
    .into()
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

/// A secret as the Secret Service takes and gives it: the session it is
/// encrypted in, the IV it is encrypted with, it encrypted, and its content
/// type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// A session with the Secret Service, the secrets in which are encrypted
/// under a key only Suru and the Secret Service know.
struct Session {
    path: OwnedObjectPath,
    key: [u8; 16],
}

impl Session {
    /// A new session with the Secret Service.
    async fn open(bus: &Connection) -> Result<Self, StoreUnavailable> {
        let ours = KeyPair::generate()?;
        let (theirs, path): (OwnedValue, OwnedObjectPath) = call(
            bus,
            SERVICE_PATH,
            SERVICE,
            "OpenSession",
            &(ALGORITHM, Value::from(ours.public())),
        )
        .await?;
        let theirs = Vec::<u8>::try_from(theirs)
            .context("it answered a session with no public key of its own")?;
        Ok(Self {
            path,
            key: ours.shared_key(&theirs)?,
        })
    }

    /// `bytes`, encrypted in the session.
    fn encrypt(&self, bytes: &[u8]) -> Result<Secret, StoreUnavailable> {
        let mut iv = [0; 16];
        getrandom::fill(&mut iv).context("could not make an IV to encrypt it with")?;
        let encrypted = cbc::Encryptor::<aes::Aes128>::new(&self.key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(bytes);
        Ok((
            self.path.clone(),
            iv.to_vec(),
            encrypted,
            CONTENT_TYPE.to_owned(),
        ))
    }

    /// The bytes `secret` encrypts in the session.
    fn decrypt(&self, secret: Secret) -> Result<Vec<u8>, StoreUnavailable> {
        let (_, iv, encrypted, _) = secret;
        let iv = <[u8; 16]>::try_from(iv)
            .map_err(|_| anyhow!("it gave a secret with no IV it can be decrypted with"))?;
        Ok(
            cbc::Decryptor::<aes::Aes128>::new(&self.key.into(), &iv.into())
                .decrypt_padded_vec_mut::<Pkcs7>(&encrypted)
                .map_err(|_| anyhow!("it gave a secret that does not decrypt in its session"))?,
        )
    }
}

/// One side's Diffie-Hellman key pair, for one session.
struct KeyPair {
    private: BigUint,
    public: BigUint,
}

impl KeyPair {
    /// A new key pair.
    fn generate() -> Result<Self, StoreUnavailable> {
        let mut private = [0; 128];
        getrandom::fill(&mut private).context("could not make a key to open a session with")?;
        let private = BigUint::from_bytes_be(&private);
        let public = BigUint::from(2_u8).modpow(&private, &BigUint::from_bytes_be(&PRIME));
        Ok(Self { private, public })
    }

    /// The public key, as the other side is given it.
    fn public(&self) -> Vec<u8> {
        self.public.to_bytes_be()
    }

    /// The AES-128 key the session's secrets are encrypted under, given the
    /// other side's public key `theirs`.
    fn shared_key(&self, theirs: &[u8]) -> Result<[u8; 16], StoreUnavailable> {
        let prime = BigUint::from_bytes_be(&PRIME);
        let theirs = BigUint::from_bytes_be(theirs);
        if theirs <= BigUint::from(1_u8) || theirs >= &prime - 1_u8 {
            return Err(anyhow!("it answered a session with a public key unfit for one").into());
        }
        let shared = theirs.modpow(&self.private, &prime).to_bytes_be();
        // The shared secret as wide as the prime, as both sides take it.
        let mut padded = [0; PRIME.len()];
        padded[PRIME.len() - shared.len()..].copy_from_slice(&shared);
        let mut key = [0; 16];
        Hkdf::<Sha256>::new(None, &padded)
            .expand(&[], &mut key)
            .expect("HKDF-SHA256 makes keys of 16 bytes");
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{fake::FakeSecrets, *};

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
            "Suru reaches a D-Bus session bus only over a Unix socket, not at \
             tcp:host=localhost,port=4000"
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

    /// D-Bus errors say why the Secret Service is unavailable, in words a
    /// user can act on where Suru knows them.
    #[test]
    fn a_refusal_says_why_the_secret_service_is_unavailable() {
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
        assert_eq!(
            refusal("org.freedesktop.DBus.Error.Failed", None).to_string(),
            "org.freedesktop.DBus.Error.Failed"
        );
    }

    /// Two sides of a session come to one key, which each decrypts what
    /// the other encrypts with; a public key unfit for the group is refused.
    #[test]
    fn both_sides_of_a_session_come_to_one_key() {
        let (ours, theirs) = (KeyPair::generate().unwrap(), KeyPair::generate().unwrap());
        let key = ours.shared_key(&theirs.public()).unwrap();
        assert_eq!(theirs.shared_key(&ours.public()).unwrap(), key);

        let path = OwnedObjectPath::try_from("/org/freedesktop/secrets/session/1").unwrap();
        let session = Session {
            path: path.clone(),
            key,
        };
        let secret = session.encrypt(b"the key").unwrap();
        assert_eq!(secret.0, path);
        assert_eq!(secret.3, CONTENT_TYPE);
        assert_ne!(secret.2, b"the key");
        assert_eq!(session.decrypt(secret).unwrap(), b"the key");

        let prime = BigUint::from_bytes_be(&PRIME);
        for unfit in [
            BigUint::from(0_u8),
            BigUint::from(1_u8),
            &prime - 1_u8,
            prime.clone(),
        ] {
            let error = ours.shared_key(&unfit.to_bytes_be()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "it answered a session with a public key unfit for one"
            );
        }
    }

    /// An item put is kept, labelled and named as Suru's, encrypted on its
    /// way; got back as it was put; and gone once deleted, deleting it
    /// again being no error.
    #[tokio::test]
    async fn an_item_put_is_got_back_and_deleted() {
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();

        assert_eq!(get(&fake.bus, &item).await.unwrap(), Stored::NoSuchItem);
        put(&fake.bus, &item, "Suru Server identity key", b"the key")
            .await
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
            get(&fake.bus, &item).await.unwrap(),
            Stored::Found(b"the key".to_vec())
        );
        assert_eq!(
            get(&fake.bus, &ItemId::random()).await.unwrap(),
            Stored::NoSuchItem
        );

        delete(&fake.bus, &item).await.unwrap();
        assert!(fake.state().items().is_empty());
        assert_eq!(get(&fake.bus, &item).await.unwrap(), Stored::NoSuchItem);
        delete(&fake.bus, &item).await.unwrap();
    }

    /// Putting an item again replaces it, keeping the one item.
    #[tokio::test]
    async fn an_item_put_again_is_replaced() {
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();
        put(&fake.bus, &item, "label", b"first").await.unwrap();
        put(&fake.bus, &item, "label", b"second").await.unwrap();
        assert_eq!(fake.state().items().len(), 1);
        assert_eq!(
            get(&fake.bus, &item).await.unwrap(),
            Stored::Found(b"second".to_vec())
        );
    }

    /// Nothing is asked of a default collection that is locked, so no
    /// unlock prompt comes up; nor is anything put where there is no
    /// default collection.
    #[tokio::test]
    async fn nothing_is_put_where_the_default_collection_is_locked_or_missing() {
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();
        fake.state().lock_all(true);
        let error = put(&fake.bus, &item, "label", b"key").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "its default collection is locked, and Suru does not ask to unlock it"
        );
        assert_eq!(fake.state().prompts_shown, 0);

        fake.state().lock_all(false);
        fake.state().default = None;
        let error = put(&fake.bus, &item, "label", b"key").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "it has no default collection to keep the item in"
        );
        assert!(fake.state().items().is_empty());
    }

    /// A prompt the Secret Service answers a put or a delete with, as for a
    /// collection that locked as it was asked, is dismissed without ever
    /// being shown, and the call counts as unavailable.
    #[tokio::test]
    async fn a_prompt_answering_a_put_or_a_delete_is_dismissed_unshown() {
        let refused = "it would do what was asked only after a prompt, which Suru does not bring \
                       up; a collection of it may be locked";
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();
        fake.state().prompts_on_create = true;
        let error = put(&fake.bus, &item, "label", b"key").await.unwrap_err();
        assert_eq!(error.to_string(), refused);
        assert!(fake.state().items().is_empty());
        assert_eq!(fake.state().prompts_shown, 0);
        assert_eq!(fake.state().prompts_dismissed, 1);

        fake.state().prompts_on_create = false;
        put(&fake.bus, &item, "label", b"key").await.unwrap();
        fake.state().prompts_on_delete = true;
        let error = delete(&fake.bus, &item).await.unwrap_err();
        assert_eq!(error.to_string(), refused);
        assert_eq!(fake.state().items().len(), 1);
        assert_eq!(fake.state().prompts_shown, 0);
        assert_eq!(fake.state().prompts_dismissed, 2);
    }

    /// An item kept in a locked collection is neither read nor deleted,
    /// and never taken for no item, whether a search answers it as locked
    /// or leaves it out.
    #[tokio::test]
    async fn an_item_a_locked_collection_keeps_is_never_taken_for_none() {
        for hides_locked_items in [false, true] {
            let fake = FakeSecrets::new().await;
            let item = ItemId::random();
            put(&fake.bus, &item, "label", b"key").await.unwrap();
            fake.state().hides_locked_items = hides_locked_items;
            fake.state().lock_all(true);

            assert!(get(&fake.bus, &item).await.is_err());
            assert!(delete(&fake.bus, &item).await.is_err());
            assert_eq!(fake.state().items().len(), 1);
            assert_eq!(fake.state().prompts_shown, 0);

            fake.state().lock_all(false);
            assert_eq!(
                get(&fake.bus, &item).await.unwrap(),
                Stored::Found(b"key".to_vec())
            );
        }
    }

    /// No item is found while another collection is locked, which may
    /// keep it, or while there is no default collection: that counts as
    /// unavailable, not as no item.
    #[tokio::test]
    async fn no_item_found_while_one_may_be_hidden_counts_as_unavailable() {
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();
        let other = fake.add_collection("other").await;
        fake.state().hides_locked_items = true;
        fake.state().lock(&other, true);
        assert!(get(&fake.bus, &item).await.is_err());
        assert!(delete(&fake.bus, &item).await.is_err());

        fake.state().lock(&other, false);
        fake.state().default = None;
        assert!(get(&fake.bus, &item).await.is_err());
    }

    /// A collection that hides its items while locked, and locks or
    /// unlocks as Suru searches, never has an item it keeps taken for none.
    #[tokio::test]
    async fn a_collection_locking_or_unlocking_as_searched_never_loses_an_item() {
        let fake = FakeSecrets::new().await;
        let item = ItemId::random();
        put(&fake.bus, &item, "label", b"key").await.unwrap();
        fake.state().hides_locked_items = true;

        fake.state().lock_all(true);
        fake.state().unlocks_as_searched = true;
        assert!(get(&fake.bus, &item).await.is_err());
        fake.state().unlocks_as_searched = false;
        assert_eq!(
            get(&fake.bus, &item).await.unwrap(),
            Stored::Found(b"key".to_vec())
        );

        fake.state().locks_as_searched = true;
        assert!(get(&fake.bus, &item).await.is_err());
        assert_eq!(fake.state().prompts_shown, 0);
    }

    /// A call with no bus to connect to fails at once, saying where the bus
    /// was looked for.
    #[test]
    fn a_call_with_no_bus_to_connect_to_fails_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", directory.path().join("bus").display());
        let store = SecretServiceStore::at(&address).with_give_up_after(Duration::from_secs(60));

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
        let store = SecretServiceStore::at(&address).with_give_up_after(Duration::from_secs(60));

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
        let store = SecretServiceStore::at(&format!("unix:path={}", path.display()))
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
    /// --run-ignored only secret_service_store`.
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
