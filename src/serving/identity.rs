//! A Server's identity key, which its Pairings pin and it proves itself to a
//! Relay by: where it is kept, and how it is got from there.
//!
//! The key is kept in the platform credential store where a release build,
//! or `SURU_IDENTITY_STORE`, selects that store and it answers, and otherwise
//! in an owner-only file in the Server's data directory (ADR-0050). A key
//! kept in the file — from before Suru kept keys in the store, or from a
//! time the store did not answer — is moved into the store, as it is, at the
//! first load the store takes it. Either way the data directory keeps a
//! marker, which holds nothing secret: where the key is kept — the store's
//! item, or the file — and the key's fingerprint. A key the marker says is
//! kept somewhere is never made anew, whatever stands in the way of getting
//! it: a new key would end every Pairing and Relay Login the Server has.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rcgen::{CertificateParams, KeyPair, PublicKeyData};
use serde::{Deserialize, Serialize};

use super::{
    SERVING_IDENTITY_NAME, fingerprint,
    identity_store::{
        BoundedStore, IdentityStore, IdentityStoreChoice, ItemId, NoIdentityStore, PLATFORM_STORE,
        Selection, StoreUnavailable, Stored,
    },
    write_private_json,
};
use crate::runtime::{protect_current_user_file, replace_private_file};

/// The owner-only file in a data directory its Server's identity key is kept
/// in, where it is not kept in the platform credential store.
const KEY_FILE: &str = "server-identity.pk8";
/// The file in a data directory that says where its Server's identity key is
/// kept.
const MARKER_FILE: &str = "server-identity.json";
/// How long a call to the platform credential store may take before it
/// counts as unavailable, where nothing says otherwise: long enough for a
/// store that answers at all, and short enough that nothing waits long on an
/// unlock nobody answers.
pub(crate) const IDENTITY_STORE_TIMEOUT: Duration = Duration::from_secs(5);

/// How a Server keeps its identity key: the platform credential store it
/// asks, which store the key is kept in, and how long it waits on the
/// platform credential store.
#[derive(Clone)]
pub(crate) struct IdentityKeeping {
    /// The platform credential store.
    pub(crate) store: Arc<dyn IdentityStore>,
    /// Which store the key is kept in, and what chose it.
    pub(crate) selection: Selection,
    /// The Server's channel, which the store's item is labelled with beside
    /// the Server's data directory, so its user can tell whose item it is.
    pub(crate) channel: String,
    /// How long each call to the store may take before it counts as
    /// unavailable.
    pub(crate) store_timeout: Duration,
}

impl IdentityKeeping {
    /// Keeping as a debug build does, in the file, with no platform
    /// credential store to ask.
    fn in_file() -> Self {
        Self {
            store: Arc::new(NoIdentityStore),
            selection: Selection::DebugBuild,
            channel: "debug".to_owned(),
            store_timeout: IDENTITY_STORE_TIMEOUT,
        }
    }
}

/// This Server's identity key: what its Pairings pin, and what it proves
/// itself to a Relay by. It is got from where its marker says it is kept —
/// or made and kept as its [`IdentityKeeping`] says, the first time — at its
/// first use, moved from the file into the store its keeping selects where
/// it is not kept there yet, and held from then on; a use that cannot get
/// it fails, and the next tries again. It is got off the async workers, so
/// a store slow to answer holds up only what needs the key, and one use at
/// a time gets it, the rest waiting for what that use gets. Its private key
/// never leaves the Serving module.
#[derive(Clone)]
pub(crate) struct IdentityKey(Arc<KeptKey>);

/// An identity key, as [`IdentityKey`] keeps it.
struct KeptKey {
    store: BoundedStore,
    selection: Selection,
    /// What the store's item is labelled.
    label: String,
    file: KeyFile,
    /// Where the marker is.
    marker: PathBuf,
    /// The key, once got.
    material: tokio::sync::OnceCell<IdentityMaterial>,
    /// The key as got off the async workers, held while it is got or made:
    /// a get begun while one for a use since given up on is still under way
    /// waits for it and takes what it got, rather than making a key beside
    /// it, or getting it, and saying where it is kept, again.
    got: StdMutex<Option<IdentityMaterial>>,
    /// The key's fingerprint, once known.
    fingerprint: OnceLock<String>,
}

impl IdentityKey {
    /// The identity key of the Server whose data directory is `data_dir`,
    /// kept as a debug build keeps it, in that directory's key file.
    pub(super) fn new(data_dir: &Path) -> Self {
        Self::kept_in(data_dir, IdentityKeeping::in_file())
    }

    /// The identity key of the Server whose data directory is `data_dir`,
    /// kept as `keeping` says.
    pub(super) fn kept_in(data_dir: &Path, keeping: IdentityKeeping) -> Self {
        Self(Arc::new(KeptKey {
            store: BoundedStore::new(keeping.store, keeping.store_timeout),
            selection: keeping.selection,
            label: format!(
                "Suru Server identity key ({} channel, {})",
                keeping.channel,
                data_dir.display()
            ),
            file: KeyFile {
                path: data_dir.join(KEY_FILE),
            },
            marker: data_dir.join(MARKER_FILE),
            material: tokio::sync::OnceCell::new(),
            got: StdMutex::default(),
            fingerprint: OnceLock::new(),
        }))
    }

    /// The key, as the DER SubjectPublicKeyInfo its Pairings pin.
    pub(crate) async fn public_key(&self) -> Result<Vec<u8>> {
        Ok(self.material().await?.public_key)
    }

    /// Signs `message` with the key, as the Server proves it to a Relay.
    pub(crate) async fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let identity = self.material().await?;
        let signing_key = KeyPair::try_from(identity.private_key.as_slice())
            .context("read Server identity key")?;
        rcgen::SigningKey::sign(&signing_key, message).context("sign with Server identity key")
    }

    /// The key's fingerprint, as a Peer's name uses it, where this Server
    /// has a key. It is known without asking the store the key is kept in —
    /// from the key once got, from what the marker records, or from the key
    /// file — so it is known while that store does not answer. A Server
    /// that has yet to make its key has none.
    pub(super) fn fingerprint(&self) -> Result<Option<String>> {
        let kept = &self.0;
        if let Some(known) = kept.fingerprint.get() {
            return Ok(Some(known.clone()));
        }
        let got = kept
            .material
            .get()
            .map(|material| fingerprint(&material.public_key));
        let known = match got {
            Some(known) => known,
            None => match Marker::read(&kept.marker)? {
                Some(marker) => marker.fingerprint().to_owned(),
                None => match kept.file.read()? {
                    Some(key) => fingerprint_of(&key)?,
                    None => return Ok(None),
                },
            },
        };
        Ok(Some(kept.fingerprint.get_or_init(|| known).clone()))
    }

    pub(super) async fn material(&self) -> Result<IdentityMaterial> {
        let got = self
            .0
            .material
            .get_or_try_init(|| async {
                let kept = Arc::clone(&self.0);
                // What it Logs goes where the use getting it Logs.
                let log = tracing::dispatcher::get_default(Clone::clone);
                tokio::task::spawn_blocking(move || {
                    tracing::dispatcher::with_default(&log, || kept.get())
                })
                .await
                .context("get Server identity key")?
            })
            .await?;
        Ok(got.clone())
    }
}

impl KeptKey {
    /// The key, from where it is kept, or made: work that waits on files and
    /// on the store, each call to which is bounded, so it is done off the
    /// async workers.
    fn get(&self) -> Result<IdentityMaterial> {
        let mut got = self
            .got
            .lock()
            .expect("Server identity lock is not poisoned");
        if let Some(material) = got.as_ref() {
            return Ok(material.clone());
        }
        let private_key = self.private_key()?;
        let signing_key =
            KeyPair::try_from(private_key.as_slice()).context("read Server identity key")?;
        let certificate = CertificateParams::new(vec![SERVING_IDENTITY_NAME.to_owned()])
            .context("describe Server identity certificate")?
            .self_signed(&signing_key)
            .context("mint Server identity certificate")?;
        let material = IdentityMaterial {
            public_key: signing_key.subject_public_key_info(),
            private_key,
            certificate: certificate.der().as_ref().to_vec(),
        };
        *got = Some(material.clone());
        Ok(material)
    }

    /// The PKCS#8 private key, got from where the marker says it is kept,
    /// and checked against the fingerprint it records; one the key file
    /// keeps is moved into the platform credential store where the
    /// selection says. With no marker, a key file the data directory
    /// already keeps is the key, and is marked as kept there; with neither,
    /// a key is made.
    fn private_key(&self) -> Result<Vec<u8>> {
        let Some(marker) = Marker::read(&self.marker)? else {
            return self.unmarked();
        };
        let key = match &marker {
            Marker::SystemStore { item, .. } => match self.store.get(item) {
                Ok(Stored::Found(key)) => key,
                Ok(Stored::NoSuchItem) => {
                    return Err(IdentityKeyUnavailable::item_gone(*item, &self.marker).into());
                }
                Err(unavailable) => {
                    return Err(IdentityKeyUnavailable::unanswered(&unavailable).into());
                }
            },
            Marker::File { .. } => self
                .file
                .read()?
                .ok_or_else(|| IdentityKeyUnavailable::file_gone(&self.file.path, &self.marker))?,
        };
        let place = marker.place();
        if fingerprint_of(&key)? != marker.fingerprint() {
            return Err(IdentityKeyUnavailable::not_its_key(
                &place,
                marker.fingerprint(),
                &self.marker,
            )
            .into());
        }
        match marker {
            Marker::SystemStore { .. } => {
                tracing::info!("Server identity key is kept in {place}, as its marker records");
                self.delete_file_left(&key, &marker);
            }
            Marker::File { fingerprint } => self.move_from_file(&key, fingerprint),
        }
        Ok(key)
    }

    /// The key where no marker says where it is kept: a key file the data
    /// directory already keeps, marked as kept there, and moved on from
    /// there as any key the file keeps is; or else a key made.
    fn unmarked(&self) -> Result<Vec<u8>> {
        let Some(key) = self.file.read()? else {
            return self.make();
        };
        let fingerprint = fingerprint_of(&key)?;
        Marker::File {
            fingerprint: fingerprint.clone(),
        }
        .write(&self.marker)?;
        self.move_from_file(&key, fingerprint);
        Ok(key)
    }

    /// Moves `key`, whose fingerprint is `fingerprint`, which the key file
    /// keeps and the marker says so, into the platform credential store,
    /// where the selection says a key is kept there and the store takes it.
    /// Where it is not moved, it stays in the file and is got from there,
    /// and the Log says why.
    fn move_from_file(&self, key: &[u8], fingerprint: String) {
        match self.selection.store() {
            IdentityStoreChoice::System => {
                if let Err(refused) = self.move_into_store(key, fingerprint) {
                    tracing::warn!(
                        "Server identity key is kept in an owner-only file in the data \
                         directory, as it could not be moved into {PLATFORM_STORE}: {refused:#}"
                    );
                }
            }
            IdentityStoreChoice::File => tracing::info!(
                "Server identity key is kept in an owner-only file in the data directory: {}",
                self.selection.why()
            ),
        }
    }

    /// Moves `key`, whose fingerprint is `fingerprint`, out of the key file
    /// into the platform credential store, as it is: kept there as a new
    /// item and read back, the marker made to name that item, and only once
    /// that marker is on the disk the file deleted. However far a move gets
    /// before it stops — the Server stopping, or the machine — the key
    /// still loads. Until the marker names the item, the file keeps the key
    /// and the marker says so, and the next move keeps the key as a new
    /// item again: one a move stopped before its marker leaves holds the
    /// key, labelled as this Server's, but no marker names it, as only a new
    /// item can be sure of being no item a marker names, in this data
    /// directory or a copy of it. Once the marker names the item, the key is
    /// got from there, and a file still left is deleted then.
    fn move_into_store(&self, key: &[u8], fingerprint: String) -> Result<()> {
        let item = self.keep_in_store(key)?;
        let marked = self.mark_kept_in_store(item, fingerprint)?;
        tracing::info!(
            "Server identity key is moved from an owner-only file in the data directory into \
             {PLATFORM_STORE}, as the item {item}: {}",
            self.selection.why()
        );
        let deleted = match marked {
            Marked::ForGood => self.file.delete(),
            Marked::InPlace => Err(anyhow!(
                "the marker naming its item may not be on the disk yet"
            )),
        };
        if let Err(left) = deleted {
            moved_file_left(&left);
        }
        Ok(())
    }

    /// Deletes the key file where it keeps `key`, which the platform
    /// credential store keeps as the item `marker` names: the file a move
    /// into the store stopped before deleting. A file keeping another key
    /// is no file a move left, and is left as it is. The key is got all the
    /// same, so what stands in the way is Logged, and the next load tries
    /// again.
    fn delete_file_left(&self, key: &[u8], marker: &Marker) {
        let deleted = match self.file.read() {
            Ok(None) => return,
            // A move can stop with the marker in place but not yet on the
            // disk, so it is written again, for good, before the file goes.
            Ok(Some(left)) if left == key => {
                marker.write(&self.marker).and_then(|()| self.file.delete())
            }
            Ok(Some(_)) => {
                tracing::warn!(
                    "{} keeps a key other than this Server's identity key, and is left as it is",
                    self.file.path.display()
                );
                return;
            }
            Err(unread) => Err(unread),
        };
        match deleted {
            Ok(()) => tracing::info!(
                "the owner-only file in the data directory the Server identity key was moved out \
                 of is deleted"
            ),
            Err(left) => moved_file_left(&left),
        }
    }

    /// A new key, kept in the store the selection says — in the file where
    /// that is the platform credential store and it cannot take the key —
    /// and marked as kept there.
    fn make(&self) -> Result<Vec<u8>> {
        let key = KeyPair::generate()
            .context("generate Server identity")?
            .serialize_der();
        let fingerprint = fingerprint_of(&key)?;
        let why = self.selection.why();
        let refused = match self.selection.store() {
            IdentityStoreChoice::System => match self.keep_in_store(&key) {
                Ok(item) => {
                    self.mark_kept_in_store(item, fingerprint)?;
                    tracing::info!(
                        "Server identity key is made and kept in {PLATFORM_STORE}, as the item \
                         {item}: {why}"
                    );
                    return Ok(key);
                }
                Err(refused) => Some(refused),
            },
            IdentityStoreChoice::File => None,
        };
        self.file.write(&key)?;
        Marker::File { fingerprint }.write(&self.marker)?;
        match refused {
            Some(refused) => tracing::warn!(
                "Server identity key is made and kept in an owner-only file in the data \
                 directory, not in {PLATFORM_STORE}, which could not take it: {refused:#}"
            ),
            None => tracing::info!(
                "Server identity key is made and kept in an owner-only file in the data \
                 directory: {why}"
            ),
        }
        Ok(key)
    }

    /// Marks a key the platform credential store has just taken as the item
    /// `item` as kept there, and says how the marker stands. Where the
    /// marker cannot be written, nothing gets the key from the item, so it
    /// is taken out of the store again rather than left behind at each
    /// failure — unless a marker names it all the same, or may: the writing
    /// can fail after it put the marker in place, and a marker must never
    /// outlive the item it names. A Server stopping before the marker is
    /// written leaves the one item, which no marker names.
    fn mark_kept_in_store(&self, item: ItemId, fingerprint: String) -> Result<Marked> {
        let Err(error) = (Marker::SystemStore { item, fingerprint }).write(&self.marker) else {
            return Ok(Marked::ForGood);
        };
        match MarkingLeft::after(Marker::read(&self.marker), item) {
            MarkingLeft::Marked => {
                tracing::warn!(
                    "Server identity marker naming the item {item} is in place, though writing \
                     it failed: {error:#}"
                );
                Ok(Marked::InPlace)
            }
            MarkingLeft::Unmarked => {
                if let Err(left) = self.store.delete(&item) {
                    tracing::warn!(
                        "a Server identity key never used is left in {PLATFORM_STORE}, as the \
                         item {item}: {left:#}"
                    );
                }
                Err(error)
            }
            MarkingLeft::Unknown => Err(error),
        }
    }

    /// Keeps `key` in the platform credential store as a new item, and reads
    /// it back to be sure the store gives it up: the item, or why the store
    /// is no place for the key. An item that does not read back as the key
    /// is taken out again, as no marker names it.
    fn keep_in_store(&self, key: &[u8]) -> Result<ItemId, StoreUnavailable> {
        let item = ItemId::random();
        self.store.put(&item, &self.label, key)?;
        let refused = match self.store.get(&item) {
            Ok(Stored::Found(kept)) if kept == key => return Ok(item),
            Ok(Stored::Found(_)) => anyhow!("it gave back another key than it was given").into(),
            Ok(Stored::NoSuchItem) => anyhow!("it kept nothing of what it was given").into(),
            Err(unanswered) => unanswered,
        };
        if let Err(left) = self.store.delete(&item) {
            tracing::warn!(
                "an item the Server identity key was put in, which no marker names, is left in \
                 {PLATFORM_STORE}, as the item {item}: {left:#}"
            );
        }
        Err(refused)
    }
}

#[derive(Clone)]
pub(super) struct IdentityMaterial {
    pub(super) private_key: Vec<u8>,
    pub(super) certificate: Vec<u8>,
    pub(super) public_key: Vec<u8>,
}

/// The fingerprint of the PKCS#8 private key `key`, as a Peer's name uses
/// it.
fn fingerprint_of(key: &[u8]) -> Result<String> {
    let key = KeyPair::try_from(key).context("read Server identity key")?;
    Ok(fingerprint(&key.subject_public_key_info()))
}

/// What a data directory's marker says of its Server's identity key: where
/// it is kept, and its fingerprint, as a Peer's name uses it.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kept_in", rename_all = "snake_case", deny_unknown_fields)]
enum Marker {
    /// In the platform credential store, as the item `item`.
    SystemStore { item: ItemId, fingerprint: String },
    /// In the key file beside the marker.
    File { fingerprint: String },
}

impl Marker {
    fn fingerprint(&self) -> &str {
        match self {
            Self::SystemStore { fingerprint, .. } | Self::File { fingerprint } => fingerprint,
        }
    }

    /// Where it says the key is kept, as the Log and its user are told.
    fn place(&self) -> String {
        match self {
            Self::SystemStore { item, .. } => format!("{PLATFORM_STORE}, as the item {item}"),
            Self::File { .. } => "an owner-only file in the data directory".to_owned(),
        }
    }

    /// The marker at `path`, where there is one. One there that cannot be
    /// read fails, and is never taken for none.
    fn read(path: &Path) -> Result<Option<Self>> {
        match fs::read(path) {
            Ok(marker) => {
                protect_current_user_file(path)?;
                serde_json::from_slice(&marker)
                    .map(Some)
                    .with_context(|| format!("read Server identity marker {path:?}"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read Server identity marker {path:?}"))
            }
        }
    }

    /// Stores the marker at `path`, replacing what was there whole or not at
    /// all, owner-only.
    fn write(&self, path: &Path) -> Result<()> {
        write_private_json(path, self)
            .with_context(|| format!("write Server identity marker {path:?}"))
    }
}

/// How the marker naming a new item stands, once marking it is done.
#[derive(Debug, PartialEq, Eq)]
enum Marked {
    /// Written, and on the disk.
    ForGood,
    /// In place, though writing it failed after it put it there, so it may
    /// not be on the disk yet.
    InPlace,
}

/// What writing the marker naming a new key's item left in place, where the
/// writing failed: it puts the marker in place whole or not at all, but can
/// fail after it has.
#[derive(Debug, PartialEq, Eq)]
enum MarkingLeft {
    /// A marker naming the item.
    Marked,
    /// No marker naming the item.
    Unmarked,
    /// A marker that cannot be read, which may name it.
    Unknown,
}

impl MarkingLeft {
    /// What is in place, as the marker reads afterwards — `read` — for the
    /// item `item`.
    fn after(read: Result<Option<Marker>>, item: ItemId) -> Self {
        match read {
            Ok(Some(Marker::SystemStore { item: named, .. })) if named == item => Self::Marked,
            Ok(_) => Self::Unmarked,
            Err(_) => Self::Unknown,
        }
    }
}

/// Logs that the key file the key was moved out of is left, as `why` says,
/// for the next load that gets the key to delete.
fn moved_file_left(why: &anyhow::Error) {
    tracing::warn!(
        "the owner-only file in the data directory the Server identity key was moved out of is \
         left, and is deleted at its next load: {why:#}"
    );
}

/// The owner-only file in a data directory an identity key is kept in,
/// where its marker says so.
struct KeyFile {
    path: PathBuf,
}

impl KeyFile {
    /// The key the file keeps, where there is one.
    fn read(&self) -> Result<Option<Vec<u8>>> {
        match fs::read(&self.path) {
            Ok(key) => {
                protect_current_user_file(&self.path)?;
                Ok(Some(key))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read Server identity {:?}", self.path))
            }
        }
    }

    /// Keeps `key` in the file, replacing what was there whole or not at all.
    fn write(&self, key: &[u8]) -> Result<()> {
        replace_private_file(&self.path, key)
            .with_context(|| format!("publish Server identity {:?}", self.path))
    }

    /// Deletes the file, where there is one.
    fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("delete Server identity {:?}", self.path))
            }
        }
    }
}

/// Why this Server's identity key could not be got from where its marker
/// says it is kept, worded for its user, who can put it right. Whatever
/// stood in the way, no key was made in its place, and the next use of the
/// key tries again.
#[derive(Debug)]
pub(crate) struct IdentityKeyUnavailable(String);

/// What a user can do about a platform credential store that does not
/// answer.
#[cfg(target_os = "macos")]
const UNANSWERED_HINT: &str = "Unlock the login keychain, by logging in at this Mac or with \
                               `security unlock-keychain`, and try again.";
#[cfg(target_os = "linux")]
const UNANSWERED_HINT: &str = "Unlock the keyring, as logging in at this machine's desktop does, \
                               and try again.";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const UNANSWERED_HINT: &str = "Try again once it answers.";

/// What a platform credential store that answers it keeps no item a marker
/// names has told, and what a user can do before giving the key up for
/// lost. The Secret Service cannot tell an item it keeps nowhere from one
/// a keyring database it has closed keeps, which it shows nobody.
#[cfg(target_os = "linux")]
const ITEM_GONE: &str = "which it does not find";
#[cfg(target_os = "linux")]
const ITEM_GONE_HINT: &str = "A keyring database or collection holding it may be closed or \
                              locked: open or unlock it, and try again. Otherwise restore the item";
#[cfg(not(target_os = "linux"))]
const ITEM_GONE: &str = "which it no longer keeps";
#[cfg(not(target_os = "linux"))]
const ITEM_GONE_HINT: &str = "Restore the item";

impl IdentityKeyUnavailable {
    /// The platform credential store did not answer, as `unanswered` says.
    fn unanswered(unanswered: &StoreUnavailable) -> Self {
        Self(format!(
            "this Server's identity key is kept in the platform credential store, \
             {PLATFORM_STORE}, which did not answer ({unanswered:#}); no new key was made in its \
             place. {UNANSWERED_HINT}"
        ))
    }

    /// The platform credential store keeps nothing as the item `item`, which
    /// the marker at `marker` names.
    fn item_gone(item: ItemId, marker: &Path) -> Self {
        Self(format!(
            "this Server's identity key is kept in the platform credential store, \
             {PLATFORM_STORE}, as the item {item}, {ITEM_GONE}; no new key was made in its place. \
             {ITEM_GONE_HINT}, or remove {} to give this Server a new identity, ending its \
             Pairings and Relay Logins.",
            marker.display()
        ))
    }

    /// The key file at `file`, which the marker at `marker` names, is gone.
    fn file_gone(file: &Path, marker: &Path) -> Self {
        Self(format!(
            "this Server's identity key is kept in {}, which is gone; no new key was made in its \
             place. Restore the file, or remove {} to give this Server a new identity, ending \
             its Pairings and Relay Logins.",
            file.display(),
            marker.display()
        ))
    }

    /// The key kept in `place` is not the one the marker at `marker`
    /// records, by its `fingerprint`.
    fn not_its_key(place: &str, fingerprint: &str, marker: &Path) -> Self {
        Self(format!(
            "the key kept in {place} is not this Server's identity key, {fingerprint}, as {} \
             records it; no new key was made in its place.",
            marker.display()
        ))
    }
}

impl std::fmt::Display for IdentityKeyUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IdentityKeyUnavailable {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::serving::FakeIdentityStore;

    /// The DER SubjectPublicKeyInfo of the PKCS#8 key `private_key`.
    fn public_key_of(private_key: &[u8]) -> Vec<u8> {
        KeyPair::try_from(private_key)
            .unwrap()
            .subject_public_key_info()
    }

    /// Keeping in `store`, as a release build keeps a key.
    fn kept_in(store: &Arc<FakeIdentityStore>) -> IdentityKeeping {
        IdentityKeeping {
            store: store.clone(),
            selection: Selection::ReleaseBuild,
            channel: "test".to_owned(),
            store_timeout: Duration::from_secs(10),
        }
    }

    /// What the marker in `directory` says.
    fn marker(directory: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(directory.join("server-identity.json")).unwrap()).unwrap()
    }

    /// The item the marker in `directory` names.
    fn marked_item(directory: &Path) -> ItemId {
        serde_json::from_value(marker(directory)["item"].clone()).unwrap()
    }

    /// The failure of an identity key `error` is, as its user is told it.
    fn told(error: &anyhow::Error) -> String {
        error
            .downcast_ref::<IdentityKeyUnavailable>()
            .unwrap_or_else(|| panic!("worded for the Server's user: {error:#}"))
            .to_string()
    }

    /// What `action` answers, and what it Logs.
    async fn record_log<T>(action: impl Future<Output = T>) -> (T, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(Arc::new(fs::File::create(&path).unwrap()))
            .finish();
        let answered = {
            let _log = tracing::subscriber::set_default(subscriber);
            action.await
        };
        (answered, fs::read_to_string(path).unwrap())
    }

    #[cfg(unix)]
    fn assert_owner_only(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{path:?}"
        );
    }

    /// The key an existing data directory keeps in its identity file is the
    /// Server's identity key, and what the Server signs to prove itself to a
    /// Relay verifies under it.
    #[tokio::test]
    async fn a_data_directorys_key_file_is_its_identity_key_and_signs_as_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let kept = KeyPair::generate().unwrap().serialize_der();
        fs::write(&path, &kept).unwrap();

        let identity = IdentityKey::new(directory.path());
        let public_key = identity.public_key().await.unwrap();
        assert_eq!(public_key, public_key_of(&kept));
        let nonce = [7; suru_relay_protocol::NONCE_LEN];
        let proof = identity
            .sign(&suru_relay_protocol::proof_message(
                "wss://relay.example",
                &nonce,
                &public_key,
            ))
            .await
            .unwrap();
        assert_eq!(
            suru_relay_protocol::verify_proof("wss://relay.example", &public_key, &nonce, &proof),
            Ok(())
        );
        assert_eq!(fs::read(&path).unwrap(), kept, "the file is left as it was");
    }

    /// A data directory with the key `key` in its key file, and no marker,
    /// as one kept before markers were.
    fn key_file_with_no_marker(key: &[u8]) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("server-identity.pk8"), key).unwrap();
        directory
    }

    /// Asserts that `key` has been moved out of the key file in `directory`
    /// into `store`, as it is: the file is gone, the store keeps the key as
    /// the item the marker names, labelled as a new key's item is, and the
    /// marker records the key's fingerprint. The item.
    fn assert_moved_into(store: &FakeIdentityStore, directory: &Path, key: &[u8]) -> ItemId {
        assert!(
            !directory.join("server-identity.pk8").exists(),
            "no key is kept in the data directory"
        );
        let item = marked_item(directory);
        assert_eq!(
            marker(directory),
            json!({
                "kept_in": "system_store",
                "item": item.to_string(),
                "fingerprint": fingerprint(&public_key_of(key)),
            })
        );
        #[cfg(unix)]
        assert_owner_only(&directory.join("server-identity.json"));
        assert_eq!(store.contents()[&item], key, "the key is moved as it is");
        assert_eq!(
            store.label(&item).unwrap(),
            format!(
                "Suru Server identity key (test channel, {})",
                directory.display()
            )
        );
        item
    }

    /// Asserts that `key` is kept in the key file in `directory` still,
    /// as it was, and marked as kept there.
    fn assert_kept_in_file(directory: &Path, key: &[u8]) {
        assert_eq!(
            fs::read(directory.join("server-identity.pk8")).unwrap(),
            key,
            "the file is left as it was"
        );
        assert_eq!(
            marker(directory),
            json!({ "kept_in": "file", "fingerprint": fingerprint(&public_key_of(key)) })
        );
    }

    /// `key` written out as hex, as a Log might carry it.
    fn hex(key: &[u8]) -> String {
        key.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A key file a data directory kept before it had a marker is moved
    /// into the platform credential store where it answers, as it is: the
    /// same key comes out, the store keeps it as the item the marker names,
    /// and the file is gone. One Log line says the key was moved, where to
    /// and why, and nothing of the key itself; a fresh identity key over
    /// the directory gets the same key from the store.
    #[tokio::test]
    async fn a_key_file_with_no_marker_is_moved_into_the_store() {
        let kept = KeyPair::generate().unwrap().serialize_der();
        let directory = key_file_with_no_marker(&kept);
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));

        let (public_key, log) = record_log(identity.public_key()).await;
        assert_eq!(public_key.unwrap(), public_key_of(&kept));
        let item = assert_moved_into(&store, directory.path(), &kept);
        assert_eq!(store.contents().len(), 1);

        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(
            lines[0].contains("moved")
                && lines[0].contains("owner-only file")
                && lines[0].contains(PLATFORM_STORE)
                && lines[0].contains(&item.to_string())
                && lines[0].contains("release builds keep it there"),
            "{log}"
        );
        assert!(!log.contains(&hex(&kept)), "{log}");

        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key_of(&kept)
        );
    }

    /// A key kept in the file while the platform credential store did not
    /// answer is moved into the store, as it is, at the first load once the
    /// store answers.
    #[tokio::test]
    async fn a_key_kept_in_the_file_is_moved_into_the_store_once_it_answers() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        store.set_available(false);
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let kept = fs::read(directory.path().join("server-identity.pk8")).unwrap();
        assert_kept_in_file(directory.path(), &kept);

        store.set_available(true);
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key
        );
        assert_moved_into(&store, directory.path(), &kept);
    }

    /// While the platform credential store does not answer, a key file
    /// keeps the key, whether a marker says so yet or not: it is used, and
    /// nothing is deleted. Each load Logs one line saying the key is kept
    /// in the file, and why it was not moved.
    #[tokio::test]
    async fn a_key_file_is_kept_and_nothing_deleted_while_the_store_does_not_answer() {
        let kept = KeyPair::generate().unwrap().serialize_der();
        let directory = key_file_with_no_marker(&kept);
        let store = Arc::new(FakeIdentityStore::default());
        store.set_available(false);

        for _ in 0..2 {
            let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
            let (public_key, log) = record_log(identity.public_key()).await;
            assert_eq!(public_key.unwrap(), public_key_of(&kept));
            assert_kept_in_file(directory.path(), &kept);
            assert!(store.contents().is_empty());
            let lines = log.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 1, "{log}");
            assert!(
                lines[0].contains("owner-only file")
                    && lines[0].contains(PLATFORM_STORE)
                    && lines[0].contains("the identity store is unavailable"),
                "{log}"
            );
        }
    }

    /// A move the platform credential store stops answering partway
    /// through — after it took the key, before it gave it back — leaves the
    /// key in the file, from which that load gets it, and the next load
    /// the store answers finishes the move with the same key.
    #[tokio::test]
    async fn a_move_interrupted_after_the_store_took_the_key_is_finished_at_the_next_load() {
        let kept = KeyPair::generate().unwrap().serialize_der();
        let directory = key_file_with_no_marker(&kept);
        let store = Arc::new(FakeIdentityStore::default());

        store.lock_after(1);
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key_of(&kept)
        );
        assert_eq!(
            store.contents().into_values().collect::<Vec<_>>(),
            std::slice::from_ref(&kept),
            "the store took the key before it locked"
        );
        assert_kept_in_file(directory.path(), &kept);

        store.set_available(true);
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key_of(&kept)
        );
        assert_moved_into(&store, directory.path(), &kept);
    }

    /// A key file left beside a marker naming the item its key was moved
    /// into — as a move stopped before its last step leaves it — is deleted
    /// at the next load that gets the key from the store, and the Log says
    /// so.
    #[tokio::test]
    async fn a_key_file_a_finished_move_left_is_deleted_at_the_next_load() {
        let kept = KeyPair::generate().unwrap().serialize_der();
        let directory = key_file_with_no_marker(&kept);
        let store = Arc::new(FakeIdentityStore::default());
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let marked = fs::read(directory.path().join("server-identity.json")).unwrap();
        let path = directory.path().join("server-identity.pk8");
        fs::write(&path, &kept).unwrap();

        store.set_available(false);
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap_err();
        assert_eq!(
            fs::read(&path).unwrap(),
            kept,
            "nothing is deleted while the store does not answer"
        );

        store.set_available(true);
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        let (public_key, log) = record_log(identity.public_key()).await;
        assert_eq!(public_key.unwrap(), public_key_of(&kept));
        assert!(!path.exists(), "the file left is deleted");
        assert_eq!(
            fs::read(directory.path().join("server-identity.json")).unwrap(),
            marked
        );
        assert_eq!(store.contents().len(), 1);
        assert!(
            log.lines()
                .any(|line| line.contains("owner-only file") && line.contains("is deleted")),
            "{log}"
        );
        assert!(!log.contains(&hex(&kept)), "{log}");
    }

    /// A key file beside a marker naming an item that keeps another key is
    /// no file a move left, and is left as it is.
    #[tokio::test]
    async fn a_key_file_keeping_another_key_than_the_stores_is_left_as_it_is() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let path = directory.path().join("server-identity.pk8");
        let other = KeyPair::generate().unwrap().serialize_der();
        fs::write(&path, &other).unwrap();

        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key
        );
        assert_eq!(fs::read(&path).unwrap(), other);
    }

    /// A key the platform credential store gives back other than it was
    /// put is not moved: the key file is left as it is and the key is got
    /// from it, the store keeps nothing of it, and the Log says why.
    #[tokio::test]
    async fn a_key_the_store_gives_back_otherwise_is_left_in_its_file() {
        let kept = KeyPair::generate().unwrap().serialize_der();
        let directory = key_file_with_no_marker(&kept);
        let store = Arc::new(FakeIdentityStore::default());
        store.mangle();

        for _ in 0..2 {
            let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
            let (public_key, log) = record_log(identity.public_key()).await;
            assert_eq!(public_key.unwrap(), public_key_of(&kept));
            assert_kept_in_file(directory.path(), &kept);
            assert!(store.contents().is_empty(), "nothing is left in the store");
            assert!(
                log.lines().any(|line| line.contains("owner-only file")
                    && line.contains("gave back another key")),
                "{log}"
            );
        }
    }

    /// A debug build, and `SURU_IDENTITY_STORE=file`, leave a key file
    /// where it is, marked as kept there, though the platform credential
    /// store would take it, and the Log says why.
    #[tokio::test]
    async fn a_key_file_is_not_moved_where_the_file_is_selected() {
        for (selection, why) in [
            (Selection::DebugBuild, "debug builds keep it there"),
            (
                Selection::Chosen(IdentityStoreChoice::File),
                "SURU_IDENTITY_STORE=file keeps it there",
            ),
        ] {
            let kept = KeyPair::generate().unwrap().serialize_der();
            let directory = key_file_with_no_marker(&kept);
            let store = Arc::new(FakeIdentityStore::default());
            let keeping = IdentityKeeping {
                selection,
                ..kept_in(&store)
            };

            for _ in 0..2 {
                let identity = IdentityKey::kept_in(directory.path(), keeping.clone());
                let (public_key, log) = record_log(identity.public_key()).await;
                assert_eq!(public_key.unwrap(), public_key_of(&kept));
                assert_kept_in_file(directory.path(), &kept);
                assert!(store.contents().is_empty());
                let lines = log.lines().collect::<Vec<_>>();
                assert_eq!(lines.len(), 1, "{log}");
                assert!(
                    lines[0].contains("owner-only file") && lines[0].contains(why),
                    "{log}"
                );
            }
        }
    }

    /// With no platform credential store to ask, a data directory with no
    /// identity yet has one made at its first use, in its owner-only key
    /// file and marked as kept there, and the same one at every use after.
    #[tokio::test]
    async fn a_first_identity_key_is_made_in_the_owner_only_identity_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");

        let public_key = IdentityKey::new(directory.path())
            .public_key()
            .await
            .unwrap();
        assert_eq!(public_key_of(&fs::read(&path).unwrap()), public_key);
        assert_eq!(marker(directory.path())["kept_in"], "file");
        #[cfg(unix)]
        assert_owner_only(&path);
        assert_eq!(
            IdentityKey::new(directory.path())
                .public_key()
                .await
                .unwrap(),
            public_key
        );
    }

    /// A key file that cannot be read fails each use of the identity key,
    /// saying so, and no key is made over it.
    #[tokio::test]
    async fn an_unreadable_key_file_fails_the_identity_key_and_is_never_made_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        fs::create_dir(&path).unwrap();

        let error = IdentityKey::new(directory.path())
            .public_key()
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("read Server identity"),
            "{error:#}"
        );
        assert!(path.is_dir(), "nothing is written in its place");
    }

    /// A first identity key is made into the platform credential store where
    /// it answers: the store keeps it as an item, and the data directory no
    /// key, only an owner-only marker naming the item beside the key's
    /// fingerprint, by which a fresh identity key over the directory gets
    /// the same key. The item is labelled with the Server's channel and data
    /// directory, and one Log line says where the key is kept and why, and
    /// nothing of the key itself.
    #[tokio::test]
    async fn a_first_identity_key_is_made_into_the_store_and_marked_as_kept_there() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));

        let (public_key, log) = record_log(async {
            let public_key = identity.public_key().await.unwrap();
            identity.sign(b"message").await.unwrap();
            public_key
        })
        .await;
        assert!(
            !directory.path().join("server-identity.pk8").exists(),
            "no key is kept in the data directory"
        );
        let item = marked_item(directory.path());
        assert_eq!(
            marker(directory.path()),
            json!({
                "kept_in": "system_store",
                "item": item.to_string(),
                "fingerprint": fingerprint(&public_key),
            })
        );
        #[cfg(unix)]
        assert_owner_only(&directory.path().join("server-identity.json"));
        let kept = store.contents();
        assert_eq!(kept.len(), 1);
        assert_eq!(public_key_of(&kept[&item]), public_key);
        assert_eq!(
            store.label(&item).unwrap(),
            format!(
                "Suru Server identity key (test channel, {})",
                directory.path().display()
            )
        );

        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(lines[0].contains(PLATFORM_STORE), "{log}");
        assert!(lines[0].contains("release builds keep it there"), "{log}");
        assert!(!log.contains(&hex(&kept[&item])), "{log}");

        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key
        );
    }

    /// Where the platform credential store cannot take a first identity
    /// key, the key is kept in the owner-only file instead and marked as
    /// kept there, and one Log line says so and why, however often the key
    /// is used.
    #[tokio::test]
    async fn a_first_identity_key_is_kept_in_the_file_where_the_store_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let store = Arc::new(FakeIdentityStore::default());
        store.set_available(false);
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));

        let (public_key, log) = record_log(async {
            let public_key = identity.public_key().await.unwrap();
            identity.sign(b"message").await.unwrap();
            identity.public_key().await.unwrap();
            public_key
        })
        .await;
        assert_eq!(public_key_of(&fs::read(&path).unwrap()), public_key);
        #[cfg(unix)]
        assert_owner_only(&path);
        assert_eq!(
            marker(directory.path()),
            json!({ "kept_in": "file", "fingerprint": fingerprint(&public_key) })
        );
        assert!(store.contents().is_empty());

        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(
            lines[0].contains("owner-only file")
                && lines[0].contains(PLATFORM_STORE)
                && lines[0].contains("the identity store is unavailable"),
            "{log}"
        );

        store.set_available(true);
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key
        );
    }

    /// A debug build keeps a first identity key in the owner-only file
    /// though the platform credential store would take it, and its Log line
    /// says why.
    #[tokio::test]
    async fn a_debug_build_keeps_a_first_identity_key_in_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(
            directory.path(),
            IdentityKeeping {
                selection: Selection::DebugBuild,
                ..kept_in(&store)
            },
        );

        let (public_key, log) = record_log(async { identity.public_key().await.unwrap() }).await;
        assert_eq!(
            public_key_of(&fs::read(directory.path().join("server-identity.pk8")).unwrap()),
            public_key
        );
        assert_eq!(marker(directory.path())["kept_in"], "file");
        assert!(store.contents().is_empty());
        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{log}");
        assert!(lines[0].contains("debug builds keep it there"), "{log}");
    }

    /// While the platform credential store does not answer, a key its
    /// marker says the store keeps cannot be used, its user is told why,
    /// and nothing is made or written in its place; once the store answers,
    /// the next use gets the key it kept all along.
    #[tokio::test]
    async fn a_key_the_store_keeps_is_never_made_anew_while_the_store_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let marked = fs::read(&marker_path).unwrap();

        store.set_available(false);
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        let told = told(&identity.public_key().await.unwrap_err());
        assert!(
            told.contains("platform credential store")
                && told.contains(PLATFORM_STORE)
                && told.contains("did not answer")
                && told.contains("no new key was made"),
            "{told}"
        );
        #[cfg(target_os = "macos")]
        assert!(told.contains("security unlock-keychain"), "{told}");
        assert!(identity.sign(b"message").await.is_err());
        assert!(
            !directory.path().join("server-identity.pk8").exists(),
            "no key is written to the file"
        );
        assert_eq!(store.contents().len(), 1, "no key is made into the store");
        assert_eq!(fs::read(&marker_path).unwrap(), marked);

        store.set_available(true);
        assert_eq!(identity.public_key().await.unwrap(), public_key);
    }

    /// A key whose item the platform credential store no longer keeps
    /// cannot be used, and nothing is made in its place, at that use or any
    /// after.
    #[tokio::test]
    async fn a_key_the_store_has_lost_is_never_made_anew() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        let store = Arc::new(FakeIdentityStore::default());
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let marked = fs::read(&marker_path).unwrap();
        let item = marked_item(directory.path());
        store.delete(&item).unwrap();

        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        for _ in 0..2 {
            let told = told(&identity.public_key().await.unwrap_err());
            assert!(
                told.contains(&item.to_string())
                    && told.contains(ITEM_GONE)
                    && told.contains("no new key was made"),
                "{told}"
            );
        }
        assert!(store.contents().is_empty());
        assert!(!directory.path().join("server-identity.pk8").exists());
        assert_eq!(fs::read(&marker_path).unwrap(), marked);
    }

    /// A key its store keeps that is not the one its marker records is
    /// never used, and none is made in its place.
    #[tokio::test]
    async fn a_key_other_than_the_one_marked_is_never_used() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let item = marked_item(directory.path());
        let other = KeyPair::generate().unwrap().serialize_der();
        store.put(&item, "label", &other).unwrap();

        let told = told(
            &IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap_err(),
        );
        assert!(
            told.contains(&item.to_string()) && told.contains("is not this Server's identity key"),
            "{told}"
        );
        assert_eq!(store.contents()[&item], other);
        assert!(!directory.path().join("server-identity.pk8").exists());
    }

    /// A key file marked as keeping the key that keeps another, or is gone,
    /// is never used, and no key is made in its place.
    #[tokio::test]
    async fn a_key_file_other_than_the_one_marked_or_gone_is_never_used() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        IdentityKey::new(directory.path())
            .public_key()
            .await
            .unwrap();
        let other = KeyPair::generate().unwrap().serialize_der();
        fs::write(&path, &other).unwrap();

        let mismatched = told(
            &IdentityKey::new(directory.path())
                .public_key()
                .await
                .unwrap_err(),
        );
        assert!(
            mismatched.contains("an owner-only file")
                && mismatched.contains("is not this Server's identity key"),
            "{mismatched}"
        );
        assert_eq!(fs::read(&path).unwrap(), other);

        fs::remove_file(&path).unwrap();
        let gone = told(
            &IdentityKey::new(directory.path())
                .public_key()
                .await
                .unwrap_err(),
        );
        assert!(
            gone.contains("which is gone") && gone.contains("no new key was made"),
            "{gone}"
        );
        assert!(!path.exists());
    }

    /// A marker that cannot be read stands for a key kept somewhere all the
    /// same, so none is made in its place.
    #[tokio::test]
    async fn an_unreadable_marker_fails_the_identity_key_and_is_never_made_over() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        fs::write(&marker_path, b"not a marker").unwrap();
        let store = Arc::new(FakeIdentityStore::default());

        let error = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("read Server identity marker"),
            "{error:#}"
        );
        assert_eq!(fs::read(&marker_path).unwrap(), b"not a marker");
        assert!(!directory.path().join("server-identity.pk8").exists());
        assert!(store.contents().is_empty());
    }

    /// While the platform credential store holds up a call to get the key,
    /// whatever else the Server runs goes on, though it runs on one thread;
    /// the key comes once the store answers.
    #[tokio::test(flavor = "current_thread")]
    async fn a_store_holding_up_the_key_holds_up_nothing_else() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let identity = IdentityKey::kept_in(
            directory.path(),
            IdentityKeeping {
                store_timeout: Duration::from_secs(2),
                ..kept_in(&store)
            },
        );

        let held = store.hold();
        let getting = tokio::spawn({
            let identity = identity.clone();
            async move { identity.public_key().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!getting.is_finished(), "the store holds the key up still");
        drop(held);
        assert_eq!(getting.await.unwrap().unwrap(), public_key);
    }

    /// A key being made for a use given up on is the key the next use gets:
    /// none is made beside it, nor got again, so the Log says once where
    /// it is kept.
    #[tokio::test(flavor = "current_thread")]
    async fn a_key_made_for_a_use_given_up_on_is_the_key_the_next_use_gets() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        let getting = || {
            let identity = identity.clone();
            tokio::spawn(async move { identity.public_key().await })
        };

        let (public_key, log) = record_log(async {
            let held = store.hold();
            let given_up = getting();
            while store.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            given_up.abort();
            assert!(given_up.await.unwrap_err().is_cancelled());
            let next = getting();
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
            next.await.unwrap().unwrap()
        })
        .await;
        assert_eq!(store.contents().len(), 1, "one key is made");
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("Server identity key"))
                .count(),
            1,
            "{log}"
        );
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            public_key,
            "the key kept is the key the use got"
        );
    }

    /// A call to get the key that the platform credential store never
    /// answers fails that use, and the next use gets the key all the same:
    /// the store answering again is all it takes, whatever became of that
    /// call.
    #[tokio::test]
    async fn a_key_is_got_at_the_next_use_though_the_store_never_answers_one_call() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let identity = IdentityKey::kept_in(
            directory.path(),
            IdentityKeeping {
                store_timeout: Duration::from_millis(200),
                ..kept_in(&store)
            },
        );

        let stalled = store.stall_next();
        let told = told(&identity.public_key().await.unwrap_err());
        assert!(told.contains("did not answer within 200ms"), "{told}");
        assert_eq!(identity.public_key().await.unwrap(), public_key);
        drop(stalled);
    }

    /// A first key the platform credential store took, whose marker cannot
    /// be written, is not left in the store: that use fails, and the next
    /// one able to write its marker keeps the one key it makes.
    #[tokio::test]
    async fn a_first_key_whose_marker_cannot_be_written_is_not_left_in_the_store() {
        let directory = tempfile::tempdir().unwrap();
        // Nothing can be written in a data directory that is not there.
        let data_dir = directory.path().join("data");
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(&data_dir, kept_in(&store));

        let error = identity.public_key().await.unwrap_err();
        assert!(
            format!("{error:#}").starts_with("write Server identity marker"),
            "{error:#}"
        );
        assert!(store.contents().is_empty(), "the key is taken out again");

        fs::create_dir(&data_dir).unwrap();
        let public_key = identity.public_key().await.unwrap();
        let kept = store.contents();
        assert_eq!(kept.len(), 1);
        assert_eq!(public_key_of(&kept[&marked_item(&data_dir)]), public_key);
    }

    /// After writing the marker naming a new key's item fails, the item is
    /// taken out of the store only where no marker names it: a marker put
    /// in place before the writing failed — its last step, making it
    /// owner-only, failing, say — names it still, and one that cannot be
    /// read may.
    #[test]
    fn a_new_item_is_taken_out_only_where_no_marker_names_it() {
        let item = ItemId::random();
        let marking = |kept_in| MarkingLeft::after(Ok(Some(kept_in)), item);
        assert_eq!(
            marking(Marker::SystemStore {
                item,
                fingerprint: "fingerprint".to_owned(),
            }),
            MarkingLeft::Marked
        );
        assert_eq!(
            marking(Marker::SystemStore {
                item: ItemId::random(),
                fingerprint: "fingerprint".to_owned(),
            }),
            MarkingLeft::Unmarked
        );
        assert_eq!(
            marking(Marker::File {
                fingerprint: "fingerprint".to_owned(),
            }),
            MarkingLeft::Unmarked
        );
        assert_eq!(MarkingLeft::after(Ok(None), item), MarkingLeft::Unmarked);
        assert_eq!(
            MarkingLeft::after(Err(anyhow!("unreadable")), item),
            MarkingLeft::Unknown
        );
    }

    /// Two data directories with one platform credential store between them
    /// have identities of their own, each kept as an item of its own.
    #[tokio::test]
    async fn data_directories_sharing_a_store_have_identities_of_their_own() {
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Arc::new(FakeIdentityStore::default());

        let first_key = IdentityKey::kept_in(first.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        let second_key = IdentityKey::kept_in(second.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();
        assert_ne!(first_key, second_key);
        assert_ne!(marked_item(first.path()), marked_item(second.path()));
        assert_eq!(store.contents().len(), 2);
        assert_eq!(
            IdentityKey::kept_in(first.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            first_key
        );
    }

    /// A platform credential store that never answers is given up on once
    /// the store timeout has passed, and counts as unavailable: a key it
    /// keeps cannot be used, and a first key is kept in the file instead.
    #[tokio::test]
    async fn a_store_that_never_answers_counts_as_unavailable() {
        let (kept, first) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Arc::new(FakeIdentityStore::default());
        let impatient = IdentityKeeping {
            store_timeout: Duration::from_millis(20),
            ..kept_in(&store)
        };
        let kept_key = IdentityKey::kept_in(kept.path(), kept_in(&store))
            .public_key()
            .await
            .unwrap();

        let held = store.hold();
        let told = told(
            &IdentityKey::kept_in(kept.path(), impatient.clone())
                .public_key()
                .await
                .unwrap_err(),
        );
        assert!(told.contains("did not answer within 20ms"), "{told}");
        let public_key = IdentityKey::kept_in(first.path(), impatient)
            .public_key()
            .await
            .unwrap();
        assert_eq!(
            public_key_of(&fs::read(first.path().join("server-identity.pk8")).unwrap()),
            public_key
        );
        assert_eq!(marker(first.path())["kept_in"], "file");
        drop(held);

        assert_eq!(
            IdentityKey::kept_in(kept.path(), kept_in(&store))
                .public_key()
                .await
                .unwrap(),
            kept_key
        );
    }
}
