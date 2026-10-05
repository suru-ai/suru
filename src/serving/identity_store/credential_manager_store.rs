//! Windows Credential Manager as an [`IdentityStore`]: each item a generic
//! credential in the credential set of the user the Server runs as, which
//! Windows keeps encrypted under that user's DPAPI keys.
//!
//! An item is the credential named [`TARGET_PREFIX`] and the item's id, so
//! the id alone finds it. Its label is the credential's user name, which
//! Credential Manager shows beneath its name, and its comment, which only
//! other programs read; a label longer than either holds is cut short in
//! the middle. The credential persists on this machine alone
//! (`CRED_PERSIST_LOCAL_MACHINE`), never carried to another by a roaming
//! profile: a Server's identity is its machine's.
//!
//! Credential Manager brings up no prompt for what Suru asks of it; the
//! [`super::BoundedStore`] every call goes through bounds each call all the
//! same. A logon session with no credential set to keep anything in — a
//! network logon, which an SSH login with a key may be — counts as
//! unavailable.

use std::borrow::Cow;
#[cfg(windows)]
use std::{
    ptr::{self, NonNull},
    slice,
};

use anyhow::anyhow;
#[cfg(windows)]
use windows_sys::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};
#[cfg(windows)]
use zeroize::Zeroize;

#[cfg(windows)]
use super::IdentityStore;
use super::{ItemId, StoreUnavailable, Stored};

/// What every credential Suru keeps a Server identity key as is named
/// first, before the item's id.
const TARGET_PREFIX: &str = "ai.suru.server-identity/";
/// The most bytes a credential's secret holds:
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE`.
const BLOB_LIMIT: usize = 5 * 512;
/// The most UTF-16 code units a credential's user name holds:
/// `CRED_MAX_USERNAME_LENGTH`.
const USER_NAME_LIMIT: usize = 513;
/// The most UTF-16 code units a credential's comment holds:
/// `CRED_MAX_STRING_LENGTH`.
const COMMENT_LIMIT: usize = 256;
/// What marks where a label too long for its field was cut short.
const CUT: char = '…';
/// Credential Manager keeps no credential by the name asked about.
const ERROR_NOT_FOUND: u32 = 1168;
/// The logon session has no credential set to keep credentials in.
const ERROR_NO_SUCH_LOGON_SESSION: u32 = 1312;

// The limits and errors above are Windows's own.
#[cfg(windows)]
const _: () = {
    use windows_sys::Win32::{Foundation, Security::Credentials};
    assert!(BLOB_LIMIT == Credentials::CRED_MAX_CREDENTIAL_BLOB_SIZE as usize);
    assert!(USER_NAME_LIMIT == Credentials::CRED_MAX_USERNAME_LENGTH as usize);
    assert!(COMMENT_LIMIT == Credentials::CRED_MAX_STRING_LENGTH as usize);
    assert!(ERROR_NOT_FOUND == Foundation::ERROR_NOT_FOUND);
    assert!(ERROR_NO_SUCH_LOGON_SESSION == Foundation::ERROR_NO_SUCH_LOGON_SESSION);
};

/// Windows Credential Manager, keeping each item in the credential set of
/// the user the Server runs as.
#[cfg(windows)]
pub(crate) struct CredentialManagerStore;

#[cfg(windows)]
impl IdentityStore for CredentialManagerStore {
    /// Replaces a credential kept as `item` already, as Credential Manager
    /// does a credential written by a name it keeps, so putting it again
    /// keeps the one credential.
    fn put(&self, item: &ItemId, label: &str, bytes: &[u8]) -> Result<(), StoreUnavailable> {
        let size = blob_size(bytes)?;
        let mut target = wide(&target_name(item));
        let mut user_name = wide(&fitted(label, USER_NAME_LIMIT));
        let mut comment = wide(&fitted(label, COMMENT_LIMIT));
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            CredentialBlobSize: size,
            // Only ever read: CredWriteW writes nothing it is given.
            CredentialBlob: bytes.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user_name.as_mut_ptr(),
            ..CREDENTIALW::default()
        };
        // SAFETY: each string `credential` points to is NUL-terminated, and
        // its secret is `size` bytes long, all alive until the call returns,
        // which only reads them.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(Failure::last().into());
        }
        Ok(())
    }

    fn get(&self, item: &ItemId) -> Result<Stored, StoreUnavailable> {
        match ReadCredential::read(item) {
            Ok(credential) => Ok(Stored::Found(credential.secret().to_vec())),
            Err(failure) => failed_get(failure),
        }
    }

    fn delete(&self, item: &ItemId) -> Result<(), StoreUnavailable> {
        let target = wide(&target_name(item));
        // SAFETY: `target` is NUL-terminated, and alive until the call
        // returns.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            return failed_delete(Failure::last());
        }
        Ok(())
    }
}

/// A credential Credential Manager read out, in a buffer it made for Suru,
/// which is freed, its secret wiped first, once this is dropped.
#[cfg(windows)]
struct ReadCredential(NonNull<CREDENTIALW>);

#[cfg(windows)]
impl ReadCredential {
    /// The credential kept as `item`.
    fn read(item: &ItemId) -> Result<Self, Failure> {
        let target = wide(&target_name(item));
        let mut credential = ptr::null_mut();
        // SAFETY: `target` is NUL-terminated, and alive until the call
        // returns; `credential` is where it answers the buffer it makes.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
            return Err(Failure::last());
        }
        NonNull::new(credential).map(Self).ok_or_else(|| Failure {
            code: 0,
            worded: "it read out no credential, though it said it had".to_owned(),
        })
    }

    /// The credential's secret, as Credential Manager read it out.
    fn secret(&self) -> &[u8] {
        // SAFETY: the buffer is alive until this is dropped, and its
        // secret, where it has one, is the size it says, within it.
        unsafe {
            let credential = self.0.as_ref();
            if credential.CredentialBlob.is_null() {
                return &[];
            }
            slice::from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            )
        }
    }
}

#[cfg(windows)]
impl Drop for ReadCredential {
    fn drop(&mut self) {
        // SAFETY: the buffer is this one's alone, and freed here alone; its
        // secret, where it has one, is the size it says, within it.
        unsafe {
            let credential = self.0.as_mut();
            if !credential.CredentialBlob.is_null() {
                slice::from_raw_parts_mut(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .zeroize();
            }
            CredFree(self.0.as_ptr().cast());
        }
    }
}

/// `text`, NUL-terminated in UTF-16, as Windows takes it.
#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// The name of the credential the item `item` is kept as.
fn target_name(item: &ItemId) -> String {
    format!("{TARGET_PREFIX}{item}")
}

/// `label`, cut short in the middle where it is longer than `limit` UTF-16
/// code units, keeping its start, which names the channel, and its end,
/// which names the data directory's last folders.
fn fitted(label: &str, limit: usize) -> Cow<'_, str> {
    if label.encode_utf16().count() <= limit {
        return Cow::Borrowed(label);
    }
    let room = limit - CUT.len_utf16();
    let (mut start, mut start_units) = (0, 0);
    for (at, character) in label.char_indices() {
        if start_units + character.len_utf16() > room / 2 {
            break;
        }
        start_units += character.len_utf16();
        start = at + character.len_utf8();
    }
    let (mut end, mut end_units) = (label.len(), 0);
    for (at, character) in label.char_indices().rev() {
        if start_units + end_units + character.len_utf16() > room {
            break;
        }
        end_units += character.len_utf16();
        end = at;
    }
    Cow::Owned(format!("{}{CUT}{}", &label[..start], &label[end..]))
}

/// The size of the secret `bytes`, where a credential holds that much.
fn blob_size(bytes: &[u8]) -> Result<u32, StoreUnavailable> {
    match u32::try_from(bytes.len()) {
        Ok(size) if bytes.len() <= BLOB_LIMIT => Ok(size),
        _ => Err(anyhow!(
            "the item is {} bytes, more than the {BLOB_LIMIT} a credential holds",
            bytes.len()
        )
        .into()),
    }
}

/// A Win32 error a call to Credential Manager failed with, as Windows words
/// it.
#[derive(Debug)]
struct Failure {
    code: u32,
    worded: String,
}

#[cfg(windows)]
impl Failure {
    /// What the call last made on this thread failed with.
    fn last() -> Self {
        let error = std::io::Error::last_os_error();
        Self {
            code: error.raw_os_error().unwrap_or_default().cast_unsigned(),
            worded: error.to_string(),
        }
    }
}

/// Why Credential Manager is unavailable, as it failed.
impl From<Failure> for StoreUnavailable {
    fn from(failure: Failure) -> Self {
        let Failure { code, worded } = failure;
        match code {
            ERROR_NO_SUCH_LOGON_SESSION => anyhow!(
                "this logon session has no credential set to keep anything in, as a network \
                 logon, which an SSH login with a key may be, has none ({worded})"
            ),
            _ => anyhow!("{worded}"),
        }
        .into()
    }
}

/// What a get answers that failed with `failure`: no such item, where
/// Credential Manager keeps no credential by the item's name.
fn failed_get(failure: Failure) -> Result<Stored, StoreUnavailable> {
    match failure.code {
        ERROR_NOT_FOUND => Ok(Stored::NoSuchItem),
        _ => Err(failure.into()),
    }
}

/// What a delete answers that failed with `failure`: done, where
/// Credential Manager keeps no credential by the item's name to delete.
fn failed_delete(failure: Failure) -> Result<(), StoreUnavailable> {
    match failure.code {
        ERROR_NOT_FOUND => Ok(()),
        _ => Err(failure.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure Windows words as it does, by its code.
    fn failure(code: u32, worded: &str) -> Failure {
        Failure {
            code,
            worded: worded.to_owned(),
        }
    }

    /// Each item is the credential named for Suru's Server identity keys
    /// and the item's id, so the id alone finds it.
    #[test]
    fn an_item_is_the_credential_named_for_suru_and_its_id() {
        let item = ItemId::random();
        assert_eq!(
            target_name(&item),
            format!("ai.suru.server-identity/{item}")
        );
        assert_ne!(target_name(&item), target_name(&ItemId::random()));
    }

    /// A label that fits is kept as it is.
    #[test]
    fn a_label_that_fits_is_kept_whole() {
        let label = r"Suru Server identity key (stable channel, C:\Users\ana\AppData\Roaming\suru)";
        assert_eq!(fitted(label, COMMENT_LIMIT), label);
        let exact = "x".repeat(COMMENT_LIMIT);
        assert_eq!(fitted(&exact, COMMENT_LIMIT), exact);
    }

    /// A label too long for its field keeps its start, naming the channel,
    /// and its end, naming the data directory's last folders, with what is
    /// cut from between them marked.
    #[test]
    fn a_label_too_long_is_cut_short_in_the_middle() {
        let directory = format!(r"C:\{}suru", "deep\\".repeat(120));
        let label = format!("Suru Server identity key (stable channel, {directory})");
        for limit in [COMMENT_LIMIT, USER_NAME_LIMIT] {
            let fitted = fitted(&label, limit);
            assert_eq!(fitted.encode_utf16().count(), limit, "{fitted}");
            assert!(
                fitted.starts_with("Suru Server identity key (stable channel, C:\\deep\\"),
                "{fitted}"
            );
            assert!(fitted.ends_with("\\deep\\suru)"), "{fitted}");
            assert_eq!(fitted.matches('…').count(), 1, "{fitted}");
        }
    }

    /// Cutting a label short never splits a character Windows writes as
    /// two UTF-16 code units, and keeps it within its field.
    #[test]
    fn a_label_is_cut_short_between_characters() {
        let label = "🔑".repeat(COMMENT_LIMIT);
        let fitted = fitted(&label, COMMENT_LIMIT);
        assert!(fitted.encode_utf16().count() <= COMMENT_LIMIT, "{fitted}");
        assert!(
            fitted.starts_with('🔑') && fitted.ends_with('🔑'),
            "{fitted}"
        );
        assert_eq!(fitted.matches('…').count(), 1, "{fitted}");
    }

    /// A key as long as a credential holds is kept; a longer one is not,
    /// saying why.
    #[test]
    fn a_secret_longer_than_a_credential_holds_is_not_kept() {
        assert_eq!(blob_size(&[7; 138]).unwrap(), 138);
        assert_eq!(blob_size(&[7; BLOB_LIMIT]).unwrap(), 2560);
        let error = blob_size(&[7; BLOB_LIMIT + 1]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the item is 2561 bytes, more than the 2560 a credential holds"
        );
    }

    /// No credential by the item's name is no such item to a get, and
    /// nothing to do to a delete.
    #[test]
    fn a_credential_not_found_is_no_such_item() {
        let not_found = || failure(ERROR_NOT_FOUND, "Element not found. (os error 1168)");
        assert_eq!(failed_get(not_found()).unwrap(), Stored::NoSuchItem);
        failed_delete(not_found()).unwrap();
    }

    /// A logon session with no credential set — a network logon's — counts
    /// as unavailable, saying so, whatever was asked.
    #[test]
    fn a_logon_session_with_no_credential_set_counts_as_unavailable() {
        let no_session = || {
            failure(
                ERROR_NO_SUCH_LOGON_SESSION,
                "A specified logon session does not exist. It may already have been terminated. \
                 (os error 1312)",
            )
        };
        let said = "this logon session has no credential set to keep anything in, as a network \
                    logon, which an SSH login with a key may be, has none (A specified logon \
                    session does not exist. It may already have been terminated. (os error \
                    1312))";
        assert_eq!(failed_get(no_session()).unwrap_err().to_string(), said);
        assert_eq!(failed_delete(no_session()).unwrap_err().to_string(), said);
        assert_eq!(StoreUnavailable::from(no_session()).to_string(), said);
    }

    /// Any other failure counts as unavailable, as Windows words it, its
    /// code and all; to a put, no credential found is one of them.
    #[test]
    fn any_other_failure_counts_as_unavailable_as_windows_words_it() {
        let denied = || failure(5, "Access is denied. (os error 5)");
        assert_eq!(
            failed_get(denied()).unwrap_err().to_string(),
            "Access is denied. (os error 5)"
        );
        assert_eq!(
            failed_delete(denied()).unwrap_err().to_string(),
            "Access is denied. (os error 5)"
        );
        assert_eq!(
            StoreUnavailable::from(failure(
                ERROR_NOT_FOUND,
                "Element not found. (os error 1168)"
            ))
            .to_string(),
            "Element not found. (os error 1168)"
        );
    }

    /// The user name, comment and persistence of `credential`.
    #[cfg(windows)]
    fn shown(credential: &ReadCredential) -> (String, String, u32) {
        /// The NUL-terminated UTF-16 string at `text`, alive while this
        /// reads it, or none where `text` is null.
        fn read(text: *const u16) -> String {
            if text.is_null() {
                return String::new();
            }
            // SAFETY: `text` is NUL-terminated, within the credential's
            // buffer, which is alive while this reads it.
            unsafe {
                let length = (0..).take_while(|&at| *text.add(at) != 0).count();
                String::from_utf16_lossy(slice::from_raw_parts(text, length))
            }
        }
        // SAFETY: the buffer is alive until `credential` is dropped.
        let credential = unsafe { credential.0.as_ref() };
        (
            read(credential.UserName),
            read(credential.Comment),
            credential.Persist,
        )
    }

    /// Takes the credential out of this user's Credential Manager when
    /// dropped, however the test holding it ends.
    #[cfg(windows)]
    struct Throwaway(ItemId);

    #[cfg(windows)]
    impl Drop for Throwaway {
        fn drop(&mut self) {
            let _ = CredentialManagerStore.delete(&self.0);
        }
    }

    /// Puts, gets and deletes a throwaway credential in this user's
    /// Credential Manager, labelled where its user sees it. Run it by hand on
    /// Windows, logged in with a password rather than over SSH with a key,
    /// with `cargo nextest run --run-ignored only credential_manager_store`.
    #[cfg(windows)]
    #[test]
    #[ignore = "touches this user's Credential Manager"]
    fn credential_manager_keeps_gives_up_and_deletes_a_throwaway_credential() {
        let store = CredentialManagerStore;
        let item = Throwaway(ItemId::random());
        // Longer than a user name or a comment holds.
        let label = format!(
            "Suru smoke test credential, safe to delete ({})",
            "padding ".repeat(80)
        );

        assert_eq!(store.get(&item.0).unwrap(), Stored::NoSuchItem);
        store.put(&item.0, &label, b"first").unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(b"first".to_vec())
        );
        let credential = ReadCredential::read(&item.0).unwrap();
        assert_eq!(
            shown(&credential),
            (
                fitted(&label, USER_NAME_LIMIT).into_owned(),
                fitted(&label, COMMENT_LIMIT).into_owned(),
                CRED_PERSIST_LOCAL_MACHINE,
            )
        );
        drop(credential);

        store.put(&item.0, &label, &[7; BLOB_LIMIT]).unwrap();
        assert_eq!(
            store.get(&item.0).unwrap(),
            Stored::Found(vec![7; BLOB_LIMIT])
        );
        store.delete(&item.0).unwrap();
        assert_eq!(store.get(&item.0).unwrap(), Stored::NoSuchItem);
        store.delete(&item.0).unwrap();
    }
}
