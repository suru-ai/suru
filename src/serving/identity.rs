//! A Server's identity key, which its Pairings pin and it proves itself to a
//! Relay by: where it is kept, and how it is got from there.
//!
//! The key is kept in the platform credential store where a release build,
//! or `SURU_IDENTITY_STORE`, selects that store and it answers as the Server
//! first needs a key, and otherwise in an owner-only file in the Server's
//! data directory (ADR-0050). Either way the data directory keeps a
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
/// asks, which store a new key is kept in, and how long it waits on the
/// platform credential store.
#[derive(Clone)]
pub(crate) struct IdentityKeeping {
    /// The platform credential store.
    pub(crate) store: Arc<dyn IdentityStore>,
    /// Which store a new key is kept in, and what chose it.
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
/// first use, and held from then on; a use that cannot get it fails, and
/// the next tries again. Its private key never leaves the Serving module.
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
    material: StdMutex<Option<IdentityMaterial>>,
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
            material: StdMutex::default(),
            fingerprint: OnceLock::new(),
        }))
    }

    /// The key, as the DER SubjectPublicKeyInfo its Pairings pin.
    pub(crate) fn public_key(&self) -> Result<Vec<u8>> {
        Ok(self.material()?.public_key)
    }

    /// Signs `message` with the key, as the Server proves it to a Relay.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let identity = self.material()?;
        let signing_key = KeyPair::try_from(identity.private_key.as_slice())
            .context("read Server identity key")?;
        rcgen::SigningKey::sign(&signing_key, message).context("sign with Server identity key")
    }

    /// The key's fingerprint, as a Peer's name uses it. Where the key has
    /// not been got, it is what the marker records, so it is known without
    /// the key: while the store the key is kept in does not answer, say.
    pub(super) fn fingerprint(&self) -> Result<String> {
        if let Some(known) = self.0.fingerprint.get() {
            return Ok(known.clone());
        }
        let known = match Marker::read(&self.0.marker)? {
            Some(marker) => marker.fingerprint().to_owned(),
            None => fingerprint(&self.material()?.public_key),
        };
        Ok(self.0.fingerprint.get_or_init(|| known).clone())
    }

    pub(super) fn material(&self) -> Result<IdentityMaterial> {
        let mut identity = self
            .0
            .material
            .lock()
            .expect("Server identity lock is not poisoned");
        if let Some(identity) = identity.as_ref() {
            return Ok(identity.clone());
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
        *identity = Some(material.clone());
        Ok(material)
    }

    /// The PKCS#8 private key, got from where the marker says it is kept,
    /// and checked against the fingerprint it records. With no marker, a
    /// key file the data directory already keeps is the key, and is marked
    /// as kept there; with neither, a key is made.
    fn private_key(&self) -> Result<Vec<u8>> {
        let kept = &self.0;
        match Marker::read(&kept.marker)? {
            Some(Marker::SystemStore { item, fingerprint }) => {
                let key = match kept.store.get(&item) {
                    Ok(Stored::Found(key)) => key,
                    Ok(Stored::NoSuchItem) => {
                        return Err(IdentityKeyUnavailable::item_gone(item, &kept.marker).into());
                    }
                    Err(unavailable) => {
                        return Err(IdentityKeyUnavailable::unanswered(&unavailable).into());
                    }
                };
                if fingerprint_of(&key)? != fingerprint {
                    return Err(IdentityKeyUnavailable::not_its_key(
                        &format!("the item {item} {PLATFORM_STORE} keeps"),
                        &fingerprint,
                        &kept.marker,
                    )
                    .into());
                }
                tracing::info!(
                    "Server identity key is kept in {PLATFORM_STORE}, as the item {item}, as its \
                     marker records"
                );
                Ok(key)
            }
            Some(Marker::File { fingerprint }) => {
                let Some(key) = kept.file.read()? else {
                    return Err(
                        IdentityKeyUnavailable::file_gone(&kept.file.path, &kept.marker).into(),
                    );
                };
                if fingerprint_of(&key)? != fingerprint {
                    return Err(IdentityKeyUnavailable::not_its_key(
                        &kept.file.path.display().to_string(),
                        &fingerprint,
                        &kept.marker,
                    )
                    .into());
                }
                tracing::info!(
                    "Server identity key is kept in an owner-only file in the data directory, as \
                     its marker records"
                );
                Ok(key)
            }
            None => match kept.file.read()? {
                Some(key) => {
                    Marker::File {
                        fingerprint: fingerprint_of(&key)?,
                    }
                    .write(&kept.marker)?;
                    tracing::info!(
                        "Server identity key is kept in the owner-only file in the data \
                         directory it was found in"
                    );
                    Ok(key)
                }
                None => self.make(),
            },
        }
    }

    /// A new key, kept in the store the selection says — in the file where
    /// that is the platform credential store and it cannot take the key —
    /// and marked as kept there.
    fn make(&self) -> Result<Vec<u8>> {
        let kept = &self.0;
        let key = KeyPair::generate()
            .context("generate Server identity")?
            .serialize_der();
        let fingerprint = fingerprint_of(&key)?;
        let why = kept.selection.why();
        let refused = match kept.selection.store() {
            IdentityStoreChoice::System => match self.keep_in_store(&key) {
                Ok(item) => {
                    Marker::SystemStore { item, fingerprint }.write(&kept.marker)?;
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
        kept.file.write(&key)?;
        Marker::File { fingerprint }.write(&kept.marker)?;
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

    /// Keeps `key` in the platform credential store as a new item, and reads
    /// it back to be sure the store gives it up: the item, or why the store
    /// is no place for the key.
    fn keep_in_store(&self, key: &[u8]) -> Result<ItemId, StoreUnavailable> {
        let item = ItemId::random();
        self.0.store.put(&item, &self.0.label, key)?;
        match self.0.store.get(&item)? {
            Stored::Found(kept) if kept == key => Ok(item),
            Stored::Found(_) => Err(anyhow!("it gave back another key than it was given").into()),
            Stored::NoSuchItem => Err(anyhow!("it kept nothing of what it was given").into()),
        }
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
#[cfg(not(target_os = "macos"))]
const UNANSWERED_HINT: &str = "Try again once it answers.";

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
             {PLATFORM_STORE}, as the item {item}, which it no longer keeps; no new key was made \
             in its place. Restore the item, or remove {} to give this Server a new identity, \
             ending its Pairings and Relay Logins.",
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

    /// The key `kept` names is not the one the marker at `marker` records,
    /// by its `fingerprint`.
    fn not_its_key(kept: &str, fingerprint: &str, marker: &Path) -> Self {
        Self(format!(
            "{kept} is not this Server's identity key, {fingerprint}, as {} records it; no new \
             key was made in its place.",
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
    fn record_log<T>(action: impl FnOnce() -> T) -> (T, String) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(Arc::new(fs::File::create(&path).unwrap()))
            .finish();
        let answered = tracing::subscriber::with_default(subscriber, action);
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
    #[test]
    fn a_data_directorys_key_file_is_its_identity_key_and_signs_as_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let kept = KeyPair::generate().unwrap().serialize_der();
        fs::write(&path, &kept).unwrap();

        let identity = IdentityKey::new(directory.path());
        let public_key = identity.public_key().unwrap();
        assert_eq!(public_key, public_key_of(&kept));
        let nonce = [7; suru_relay_protocol::NONCE_LEN];
        let proof = identity
            .sign(&suru_relay_protocol::proof_message(
                "wss://relay.example",
                &nonce,
                &public_key,
            ))
            .unwrap();
        assert_eq!(
            suru_relay_protocol::verify_proof("wss://relay.example", &public_key, &nonce, &proof),
            Ok(())
        );
        assert_eq!(fs::read(&path).unwrap(), kept, "the file is left as it was");
    }

    /// A key file a data directory kept before it had a marker stays its
    /// identity key, kept where it is and marked as kept there, whatever
    /// store a new key would go into.
    #[test]
    fn a_key_file_with_no_marker_is_kept_where_it_is_and_marked_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let kept = KeyPair::generate().unwrap().serialize_der();
        fs::write(&path, &kept).unwrap();
        let store = Arc::new(FakeIdentityStore::default());

        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .unwrap();
        assert_eq!(public_key, public_key_of(&kept));
        assert_eq!(
            marker(directory.path()),
            json!({ "kept_in": "file", "fingerprint": fingerprint(&public_key) })
        );
        assert_eq!(fs::read(&path).unwrap(), kept, "the file is left as it was");
        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .unwrap(),
            public_key
        );
    }

    /// With no platform credential store to ask, a data directory with no
    /// identity yet has one made at its first use, in its owner-only key
    /// file and marked as kept there, and the same one at every use after.
    #[test]
    fn a_first_identity_key_is_made_in_the_owner_only_identity_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");

        let public_key = IdentityKey::new(directory.path()).public_key().unwrap();
        assert_eq!(public_key_of(&fs::read(&path).unwrap()), public_key);
        assert_eq!(marker(directory.path())["kept_in"], "file");
        #[cfg(unix)]
        assert_owner_only(&path);
        assert_eq!(
            IdentityKey::new(directory.path()).public_key().unwrap(),
            public_key
        );
    }

    /// A key file that cannot be read fails each use of the identity key,
    /// saying so, and no key is made over it.
    #[test]
    fn an_unreadable_key_file_fails_the_identity_key_and_is_never_made_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        fs::create_dir(&path).unwrap();

        let error = IdentityKey::new(directory.path()).public_key().unwrap_err();
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
    #[test]
    fn a_first_identity_key_is_made_into_the_store_and_marked_as_kept_there() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));

        let (public_key, log) = record_log(|| {
            let public_key = identity.public_key().unwrap();
            identity.sign(b"message").unwrap();
            public_key
        });
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
        let hex = kept[&item]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(!log.contains(&hex), "{log}");

        assert_eq!(
            IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .unwrap(),
            public_key
        );
    }

    /// Where the platform credential store cannot take a first identity
    /// key, the key is kept in the owner-only file instead and marked as
    /// kept there, and one Log line says so and why, however often the key
    /// is used.
    #[test]
    fn a_first_identity_key_is_kept_in_the_file_where_the_store_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");
        let store = Arc::new(FakeIdentityStore::default());
        store.set_available(false);
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));

        let (public_key, log) = record_log(|| {
            let public_key = identity.public_key().unwrap();
            identity.sign(b"message").unwrap();
            identity.public_key().unwrap();
            public_key
        });
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
                .unwrap(),
            public_key
        );
    }

    /// A debug build keeps a first identity key in the owner-only file
    /// though the platform credential store would take it, and its Log line
    /// says why.
    #[test]
    fn a_debug_build_keeps_a_first_identity_key_in_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let identity = IdentityKey::kept_in(
            directory.path(),
            IdentityKeeping {
                selection: Selection::DebugBuild,
                ..kept_in(&store)
            },
        );

        let (public_key, log) = record_log(|| identity.public_key().unwrap());
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
    #[test]
    fn a_key_the_store_keeps_is_never_made_anew_while_the_store_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .unwrap();
        let marked = fs::read(&marker_path).unwrap();

        store.set_available(false);
        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        let told = told(&identity.public_key().unwrap_err());
        assert!(
            told.contains("platform credential store")
                && told.contains(PLATFORM_STORE)
                && told.contains("did not answer")
                && told.contains("no new key was made"),
            "{told}"
        );
        #[cfg(target_os = "macos")]
        assert!(told.contains("security unlock-keychain"), "{told}");
        assert!(identity.sign(b"message").is_err());
        assert!(
            !directory.path().join("server-identity.pk8").exists(),
            "no key is written to the file"
        );
        assert_eq!(store.contents().len(), 1, "no key is made into the store");
        assert_eq!(fs::read(&marker_path).unwrap(), marked);

        store.set_available(true);
        assert_eq!(identity.public_key().unwrap(), public_key);
    }

    /// A key whose item the platform credential store no longer keeps
    /// cannot be used, and nothing is made in its place, at that use or any
    /// after.
    #[test]
    fn a_key_the_store_has_lost_is_never_made_anew() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        let store = Arc::new(FakeIdentityStore::default());
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .unwrap();
        let marked = fs::read(&marker_path).unwrap();
        let item = marked_item(directory.path());
        store.delete(&item).unwrap();

        let identity = IdentityKey::kept_in(directory.path(), kept_in(&store));
        for _ in 0..2 {
            let told = told(&identity.public_key().unwrap_err());
            assert!(
                told.contains(&item.to_string())
                    && told.contains("no longer keeps")
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
    #[test]
    fn a_key_other_than_the_one_marked_is_never_used() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .unwrap();
        let item = marked_item(directory.path());
        let other = KeyPair::generate().unwrap().serialize_der();
        store.put(&item, "label", &other).unwrap();

        let told = told(
            &IdentityKey::kept_in(directory.path(), kept_in(&store))
                .public_key()
                .unwrap_err(),
        );
        assert!(
            told.contains(&item.to_string()) && told.contains("is not this Server's identity key"),
            "{told}"
        );
        assert_eq!(store.contents()[&item], other);
        assert!(!directory.path().join("server-identity.pk8").exists());
    }

    /// A marker that cannot be read stands for a key kept somewhere all the
    /// same, so none is made in its place.
    #[test]
    fn an_unreadable_marker_fails_the_identity_key_and_is_never_made_over() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("server-identity.json");
        fs::write(&marker_path, b"not a marker").unwrap();
        let store = Arc::new(FakeIdentityStore::default());

        let error = IdentityKey::kept_in(directory.path(), kept_in(&store))
            .public_key()
            .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("read Server identity marker"),
            "{error:#}"
        );
        assert_eq!(fs::read(&marker_path).unwrap(), b"not a marker");
        assert!(!directory.path().join("server-identity.pk8").exists());
        assert!(store.contents().is_empty());
    }

    /// Two data directories with one platform credential store between them
    /// have identities of their own, each kept as an item of its own.
    #[test]
    fn data_directories_sharing_a_store_have_identities_of_their_own() {
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Arc::new(FakeIdentityStore::default());

        let first_key = IdentityKey::kept_in(first.path(), kept_in(&store))
            .public_key()
            .unwrap();
        let second_key = IdentityKey::kept_in(second.path(), kept_in(&store))
            .public_key()
            .unwrap();
        assert_ne!(first_key, second_key);
        assert_ne!(marked_item(first.path()), marked_item(second.path()));
        assert_eq!(store.contents().len(), 2);
        assert_eq!(
            IdentityKey::kept_in(first.path(), kept_in(&store))
                .public_key()
                .unwrap(),
            first_key
        );
    }

    /// A platform credential store that never answers is given up on once
    /// the store timeout has passed, and counts as unavailable: a key it
    /// keeps cannot be used, and a first key is kept in the file instead.
    #[test]
    fn a_store_that_never_answers_counts_as_unavailable() {
        let (kept, first) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = Arc::new(FakeIdentityStore::default());
        let impatient = IdentityKeeping {
            store_timeout: Duration::from_millis(20),
            ..kept_in(&store)
        };
        let kept_key = IdentityKey::kept_in(kept.path(), kept_in(&store))
            .public_key()
            .unwrap();

        let held = store.hold();
        let told = told(
            &IdentityKey::kept_in(kept.path(), impatient.clone())
                .public_key()
                .unwrap_err(),
        );
        assert!(told.contains("did not answer within 20ms"), "{told}");
        let public_key = IdentityKey::kept_in(first.path(), impatient)
            .public_key()
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
                .unwrap(),
            kept_key
        );
    }
}
