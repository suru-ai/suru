//! A Server's identity key, which its Pairings pin and it proves itself to a
//! Relay by: where it is kept, and how it is got from there.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, PublicKeyData};

use super::{
    SERVING_IDENTITY_NAME,
    identity_store::{FileIdentityStore, IdentityStore, ItemId, Stored},
};

/// The item a Server's identity key is kept as in its identity store.
const IDENTITY_ITEM: &str = "server-identity";

/// This Server's identity key: what its Pairings pin, and what it proves
/// itself to a Relay by. It is got from the Server's identity store — or
/// made and kept there, the first time — at its first use, and held from
/// then on. Its private key never leaves this module.
#[derive(Clone)]
pub(crate) struct IdentityKey {
    data_dir: PathBuf,
    store: Arc<dyn IdentityStore>,
    material: Arc<StdMutex<Option<IdentityMaterial>>>,
}

impl IdentityKey {
    /// The identity key of the Server whose data directory is `data_dir`,
    /// kept in that directory's identity file.
    pub(super) fn new(data_dir: &Path) -> Self {
        Self::kept_in(data_dir, Arc::new(FileIdentityStore::in_data_dir(data_dir)))
    }

    /// The identity key of the Server whose data directory is `data_dir`,
    /// kept in `store`.
    pub(super) fn kept_in(data_dir: &Path, store: Arc<dyn IdentityStore>) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            store,
            material: Arc::default(),
        }
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

    pub(super) fn material(&self) -> Result<IdentityMaterial> {
        let mut identity = self
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

    /// The PKCS#8 private key the store keeps, made and kept there if it
    /// keeps none. One the store cannot say it keeps or not is never made.
    fn private_key(&self) -> Result<Vec<u8>> {
        let item = ItemId::from(IDENTITY_ITEM);
        if let Stored::Found(private_key) = self.store.get(&item)? {
            return Ok(private_key);
        }
        let private_key = KeyPair::generate()
            .context("generate Server identity")?
            .serialize_der();
        let label = format!("Suru Server identity key for {}", self.data_dir.display());
        self.store.put(&item, &label, &private_key)?;
        Ok(private_key)
    }
}

#[derive(Clone)]
pub(super) struct IdentityMaterial {
    pub(super) private_key: Vec<u8>,
    pub(super) certificate: Vec<u8>,
    pub(super) public_key: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::serving::FakeIdentityStore;

    /// The DER SubjectPublicKeyInfo of the PKCS#8 key `private_key`.
    fn public_key_of(private_key: &[u8]) -> Vec<u8> {
        KeyPair::try_from(private_key)
            .unwrap()
            .subject_public_key_info()
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

    /// A data directory with no identity yet has one made at its first use,
    /// in its owner-only identity file, and the same one at every use after.
    #[test]
    fn a_first_identity_key_is_made_in_the_owner_only_identity_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server-identity.pk8");

        let public_key = IdentityKey::new(directory.path()).public_key().unwrap();
        assert_eq!(public_key_of(&fs::read(&path).unwrap()), public_key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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

    /// An identity key is made at its first use into the identity store it
    /// is kept in, not the data directory, and got from there again by the
    /// next Server over that store.
    #[test]
    fn an_identity_key_is_made_into_its_store_and_got_from_it() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());

        let public_key = IdentityKey::kept_in(directory.path(), store.clone())
            .public_key()
            .unwrap();
        assert_eq!(
            IdentityKey::kept_in(directory.path(), store.clone())
                .public_key()
                .unwrap(),
            public_key
        );
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            0,
            "nothing is written to the data directory"
        );
    }

    /// While its store cannot answer, each use of an identity key fails and
    /// none is made in its place; the next use once it answers gets the key
    /// it kept all along.
    #[test]
    fn an_identity_key_whose_store_cannot_answer_is_got_once_it_does() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(FakeIdentityStore::default());
        let public_key = IdentityKey::kept_in(directory.path(), store.clone())
            .public_key()
            .unwrap();

        store.set_available(false);
        let identity = IdentityKey::kept_in(directory.path(), store.clone());
        assert!(identity.public_key().is_err());
        assert!(identity.sign(b"message").is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

        store.set_available(true);
        assert_eq!(identity.public_key().unwrap(), public_key);
    }
}
