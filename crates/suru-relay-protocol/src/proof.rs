//! How a Server proves its identity key to a Relay: by signing the Relay's
//! fresh nonce with it, so the Login the key stands for is useless off the
//! machine that holds the key and no credential for the Relay is stored
//! anywhere (ADR-0048). The signature names the Relay it is for, by the
//! address the Server knows it at, so a Relay that hands another Relay's
//! challenge on to a Server gains nothing it can answer that other Relay
//! with.

use ring::signature::{
    ECDSA_P256_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ED25519, UnparsedPublicKey,
    VerificationAlgorithm,
};
use x509_parser::{
    oid_registry::{OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_SIG_ED25519},
    prelude::FromDer,
    x509::SubjectPublicKeyInfo,
};

/// How many bytes a Relay's challenge nonce has.
pub const NONCE_LEN: usize = 32;

/// What every proof begins with, so a signature the key made for anything
/// else proves nothing here.
const PROOF_CONTEXT: &[u8] = b"suru-relay key proof\0";

/// What a Server signs to prove `key`, its identity key, answering the
/// `nonce` of the Relay at `relay` — the Relay's canonical address (see
/// [`canonical_address`](crate::canonical_address)): the nonce bound to this
/// protocol, to that one Relay, and to the key itself.
pub fn proof_message(relay: &str, nonce: &[u8], key: &[u8]) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(PROOF_CONTEXT.len() + 12 + relay.len() + nonce.len() + key.len());
    message.extend_from_slice(PROOF_CONTEXT);
    for part in [relay.as_bytes(), nonce, key] {
        let length = u32::try_from(part.len()).expect("a proof's parts are small");
        message.extend_from_slice(&length.to_be_bytes());
        message.extend_from_slice(part);
    }
    message
}

/// Why a proof proves nothing.
#[derive(Debug, Eq, PartialEq)]
pub enum ProofError {
    /// The key is not a DER SubjectPublicKeyInfo of a kind a Relay checks:
    /// ECDSA on P-256 or P-384, or Ed25519.
    UnsupportedKey,
    /// The signature is not one `key` made over [`proof_message`].
    Wrong,
}

/// Whether `signature` is `key`'s, made over [`proof_message`] for the Relay
/// at `relay` and its `nonce`.
pub fn verify_proof(
    relay: &str,
    key: &[u8],
    nonce: &[u8],
    signature: &[u8],
) -> Result<(), ProofError> {
    let (algorithm, public_key) = verification(key)?;
    UnparsedPublicKey::new(algorithm, public_key)
        .verify(&proof_message(relay, nonce, key), signature)
        .map_err(|_| ProofError::Wrong)
}

/// Whether `key` is of a kind a Relay can check a proof by.
pub fn supports_key(key: &[u8]) -> bool {
    verification(key).is_ok()
}

/// The fingerprint of `key`: the lowercase hex BLAKE3 hash of its DER form,
/// as Suru names a Server's key wherever it shows one.
pub fn fingerprint(key: &[u8]) -> String {
    blake3::hash(key).to_hex().to_string()
}

/// How a proof by `key` is checked, and the public key's own bytes.
fn verification(key: &[u8]) -> Result<(&'static dyn VerificationAlgorithm, &[u8]), ProofError> {
    let Ok((rest, info)) = SubjectPublicKeyInfo::from_der(key) else {
        return Err(ProofError::UnsupportedKey);
    };
    if !rest.is_empty() {
        return Err(ProofError::UnsupportedKey);
    }
    let algorithm = &info.algorithm.algorithm;
    let verification: &'static dyn VerificationAlgorithm =
        if *algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY {
            let curve = info
                .algorithm
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.as_oid().ok());
            match curve {
                Some(curve) if curve == OID_EC_P256 => &ECDSA_P256_SHA256_ASN1,
                Some(curve) if curve == OID_NIST_EC_P384 => &ECDSA_P384_SHA384_ASN1,
                _ => return Err(ProofError::UnsupportedKey),
            }
        } else if *algorithm == OID_SIG_ED25519 {
            &ED25519
        } else {
            return Err(ProofError::UnsupportedKey);
        };
    let public_key = &key[key.len() - info.subject_public_key.data.len()..];
    Ok((verification, public_key))
}

#[cfg(test)]
mod tests {
    use rcgen::{KeyPair, PublicKeyData, SigningKey};

    use super::*;

    const NONCE: [u8; NONCE_LEN] = [7; NONCE_LEN];
    const RELAY: &str = "https://relay.example.com";

    fn proof(key: &KeyPair, nonce: &[u8]) -> Vec<u8> {
        proof_for(RELAY, key, nonce)
    }

    fn proof_for(relay: &str, key: &KeyPair, nonce: &[u8]) -> Vec<u8> {
        key.sign(&proof_message(relay, nonce, &key.subject_public_key_info()))
            .expect("sign a proof")
    }

    #[test]
    fn a_signature_over_the_nonce_by_the_named_key_proves_it_for_every_kind_of_key_checked() {
        for algorithm in [
            &rcgen::PKCS_ECDSA_P256_SHA256,
            &rcgen::PKCS_ECDSA_P384_SHA384,
            &rcgen::PKCS_ED25519,
        ] {
            let key = KeyPair::generate_for(algorithm).expect("generate a key");
            let public_key = key.subject_public_key_info();
            assert!(supports_key(&public_key));
            assert_eq!(
                verify_proof(RELAY, &public_key, &NONCE, &proof(&key, &NONCE)),
                Ok(())
            );
        }
    }

    #[test]
    fn a_suru_identity_key_is_one_a_relay_checks() {
        let key = KeyPair::generate().expect("generate an identity key as a Server does");
        assert!(supports_key(&key.subject_public_key_info()));
    }

    #[test]
    fn a_signature_by_another_key_or_over_another_nonce_proves_nothing() {
        let key = KeyPair::generate().unwrap();
        let other = KeyPair::generate().unwrap();
        let public_key = key.subject_public_key_info();
        assert_eq!(
            verify_proof(RELAY, &public_key, &NONCE, &proof(&other, &NONCE)),
            Err(ProofError::Wrong)
        );
        assert_eq!(
            verify_proof(RELAY, &public_key, &[8; NONCE_LEN], &proof(&key, &NONCE)),
            Err(ProofError::Wrong)
        );
        let bare_nonce = key.sign(&NONCE).unwrap();
        assert_eq!(
            verify_proof(RELAY, &public_key, &NONCE, &bare_nonce),
            Err(ProofError::Wrong),
            "a signature over the bare nonce, made for some other purpose, proves nothing"
        );
        assert_eq!(
            verify_proof(RELAY, &public_key, &NONCE, b"not a signature"),
            Err(ProofError::Wrong)
        );
    }

    #[test]
    fn a_proof_made_for_one_relay_proves_nothing_at_another() {
        let key = KeyPair::generate().unwrap();
        let public_key = key.subject_public_key_info();
        let elsewhere = proof_for("https://other-relay.example.com", &key, &NONCE);
        assert_eq!(
            verify_proof(RELAY, &public_key, &NONCE, &elsewhere),
            Err(ProofError::Wrong)
        );
        assert_eq!(
            verify_proof(
                "https://other-relay.example.com",
                &public_key,
                &NONCE,
                &elsewhere
            ),
            Ok(())
        );
    }

    #[test]
    fn a_key_that_is_no_public_key_a_relay_checks_is_unsupported() {
        assert!(!supports_key(b"not a key"));
        assert!(!supports_key(&[]));
        let mut trailing = KeyPair::generate().unwrap().subject_public_key_info();
        trailing.push(0);
        assert!(!supports_key(&trailing));
        assert_eq!(
            verify_proof(RELAY, b"not a key", &NONCE, b"signature"),
            Err(ProofError::UnsupportedKey)
        );
    }

    #[test]
    fn a_fingerprint_is_the_hex_blake3_of_the_key() {
        assert_eq!(
            fingerprint(b"key"),
            blake3::hash(b"key").to_hex().to_string()
        );
        assert_eq!(fingerprint(b"key").len(), 64);
    }
}
