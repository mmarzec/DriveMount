// SPDX-License-Identifier: GPL-3.0-or-later

//! Passwords and passphrases in Windows Credential Manager (NFR-SEC-01).
//!
//! Entries are `CRED_TYPE_GENERIC` and `CRED_PERSIST_LOCAL_MACHINE`, named
//! `DriveMount:<uuid>:<slot>`, with the value as UTF-16LE, the form Windows' own tools use.
//!
//! Credential Manager needs a logon session with a loaded profile. Without one, as in a network
//! logon or some services, calls fail with `ERROR_NO_SUCH_LOGON_SESSION`.

use std::io;
use std::ptr;
use std::slice;

pub use drivemount_vfs::SecretSlot;
use drivemount_vfs::{Secret, SecretStore};
use uuid::Uuid;
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};
use zeroize::Zeroize;

use crate::{os_error, wide};

/// The longest secret Credential Manager can hold, in UTF-16 code units.
pub const MAX_SECRET_UNITS: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize / 2;

/// A [`SecretStore`] over [`store`], [`load`], [`delete`] and [`delete_all`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CredentialStore;

impl SecretStore for CredentialStore {
    fn store(&self, profile: Uuid, slot: SecretSlot, value: &Secret<String>) -> io::Result<()> {
        store(profile, slot, value)
    }

    fn load(&self, profile: Uuid, slot: SecretSlot) -> io::Result<Option<Secret<String>>> {
        load(profile, slot)
    }

    fn delete(&self, profile: Uuid, slot: SecretSlot) -> io::Result<bool> {
        delete(profile, slot)
    }

    fn delete_all(&self, profile: Uuid) -> io::Result<()> {
        delete_all(profile)
    }
}

const fn slot_name(slot: SecretSlot) -> &'static str {
    match slot {
        SecretSlot::Password => "password",
        SecretSlot::Passphrase => "passphrase",
        SecretSlot::JumpPassword => "jump-password",
    }
}

/// How every entry's name starts.
const PREFIX: &str = "DriveMount:";

/// The entry's name in Credential Manager: `DriveMount:<uuid>:<slot>`.
#[must_use]
pub fn target(profile: Uuid, slot: SecretSlot) -> String {
    format!("{PREFIX}{profile}:{}", slot_name(slot))
}

/// Stores a secret, replacing any earlier one.
///
/// # Errors
/// `InvalidInput` if it is longer than [`MAX_SECRET_UNITS`], or the error from Windows. Neither
/// contains the secret.
pub fn store(profile: Uuid, slot: SecretSlot, value: &Secret<String>) -> io::Result<()> {
    write(&target(profile, slot), value)
}

/// Reads a secret. `None` if there is none.
///
/// # Errors
/// `InvalidData` if the entry is not UTF-16, as when something else wrote it, or the error from
/// Windows. Neither contains the secret.
pub fn load(profile: Uuid, slot: SecretSlot) -> io::Result<Option<Secret<String>>> {
    read(&target(profile, slot))
}

/// Deletes a secret. Returns whether there was one.
///
/// # Errors
/// The error from Windows.
pub fn delete(profile: Uuid, slot: SecretSlot) -> io::Result<bool> {
    let name = wide(target(profile, slot));
    // SAFETY: `name` is nul-terminated and outlives the call.
    match unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(os_error(&error)),
    }
}

/// Deletes all of a profile's secrets (FR-PROF-07). A failure does not stop the other slots.
///
/// # Errors
/// The first error from Windows.
pub fn delete_all(profile: Uuid) -> io::Result<()> {
    let mut result = Ok(());
    for slot in SecretSlot::ALL {
        if let Err(error) = delete(profile, slot) {
            result = result.and(Err(error));
        }
    }
    result
}

/// Deletes every entry of every profile, those no longer in the config included: the uninstaller's
/// *Remove my settings* (spec §9). Returns how many there were. A failure does not stop the others.
///
/// # Errors
/// The first error from Windows.
pub fn delete_everything() -> io::Result<usize> {
    delete_starting_with(PREFIX)
}

/// Deletes the generic entries whose names start with `prefix`.
fn delete_starting_with(prefix: &str) -> io::Result<usize> {
    let mut deleted = 0;
    let mut result = Ok(());
    for name in names_starting_with(prefix)? {
        // SAFETY: `name` is nul-terminated and outlives the call.
        match unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => deleted += 1,
            // Gone in the meantime.
            Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => {}
            Err(error) => result = result.and(Err(os_error(&error))),
        }
    }
    result.map(|()| deleted)
}

/// The names of the generic entries that start with `prefix`, each nul-terminated.
fn names_starting_with(prefix: &str) -> io::Result<Vec<Vec<u16>>> {
    let filter = wide(format!("{prefix}*"));
    let mut count = 0;
    let mut list = ptr::null_mut();
    // SAFETY: `filter` is nul-terminated and outlives the call, and both outputs can be written to.
    let listed =
        unsafe { CredEnumerateW(PCWSTR(filter.as_ptr()), None, &raw mut count, &raw mut list) };
    match listed {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(Vec::new()),
        Err(error) => return Err(os_error(&error)),
    }
    let list = Listed(list);
    let mut names = Vec::new();
    for index in 0..count as usize {
        // SAFETY: `CredEnumerateW` gave `count` pointers to valid credentials, freed only with `list`.
        let credential = unsafe { &**list.0.add(index) };
        if credential.Type != CRED_TYPE_GENERIC || credential.TargetName.is_null() {
            continue;
        }
        // SAFETY: Windows ends the name in a nul, and it lives as long as `list`.
        let name = unsafe { credential.TargetName.as_wide() };
        names.push(name.iter().copied().chain([0]).collect());
    }
    Ok(names)
}

/// What `CredEnumerateW` returned. Its secrets are not looked at; dropping it frees the memory.
struct Listed(*mut *mut CREDENTIALW);

impl Drop for Listed {
    fn drop(&mut self) {
        // SAFETY: `CredEnumerateW` allocated this, and it is freed only here.
        unsafe { CredFree(self.0.cast_const().cast()) };
    }
}

fn write(target: &str, value: &Secret<String>) -> io::Result<()> {
    let text = value.expose_secret();
    if text.encode_utf16().count() > MAX_SECRET_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Credential Manager holds at most {MAX_SECRET_UNITS} characters"),
        ));
    }
    let blob = encode(text);
    let bytes = blob.expose_secret();
    let mut name = wide(target);
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(name.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(bytes.len()).map_err(io::Error::other)?,
        // CredWriteW only reads through this; the struct is the same for reading and writing.
        CredentialBlob: bytes.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..CREDENTIALW::default()
    };
    // SAFETY: the name and blob the struct points to outlive the call, and the size is the blob's.
    unsafe { CredWriteW(&raw const credential, 0) }.map_err(|error| os_error(&error))
}

fn read(target: &str) -> io::Result<Option<Secret<String>>> {
    let name = wide(target);
    let mut raw = ptr::null_mut();
    // SAFETY: `name` is nul-terminated and outlives the call, and `raw` can be written to.
    let result = unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &raw mut raw) };
    match result {
        Ok(()) => decode(ReadCredential(raw).blob()).map(Some),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(None),
        Err(error) => Err(os_error(&error)),
    }
}

/// What `CredReadW` returned. Dropping it zeroes the secret and frees the memory.
struct ReadCredential(*mut CREDENTIALW);

impl ReadCredential {
    fn blob(&mut self) -> &mut [u8] {
        // SAFETY: `CredReadW` succeeded, so this points to a valid credential, and we hold the only
        // pointer to it.
        let credential = unsafe { &*self.0 };
        let size = credential.CredentialBlobSize as usize;
        if credential.CredentialBlob.is_null() || size == 0 {
            return &mut [];
        }
        // SAFETY: the blob is `size` bytes, lives as long as the credential, and is ours alone.
        unsafe { slice::from_raw_parts_mut(credential.CredentialBlob, size) }
    }
}

impl Drop for ReadCredential {
    fn drop(&mut self) {
        self.blob().zeroize();
        // SAFETY: `CredReadW` allocated this, and it is freed only here.
        unsafe { CredFree(self.0.cast_const().cast()) };
    }
}

/// The secret as UTF-16LE. The capacity is enough from the start, so no copy is left behind in a
/// buffer that grew: UTF-16 never takes more than twice the bytes of UTF-8.
fn encode(text: &str) -> Secret<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Secret::new(bytes)
}

/// The reverse of [`encode`]. The capacity is enough from the start, as a UTF-16 unit never takes
/// more than three bytes of UTF-8.
fn decode(blob: &[u8]) -> io::Result<Secret<String>> {
    let not_text = || io::Error::new(io::ErrorKind::InvalidData, "the entry is not UTF-16 text");

    let (pairs, odd_byte) = blob.as_chunks::<2>();
    if !odd_byte.is_empty() {
        return Err(not_text());
    }
    let units = pairs.iter().map(|pair| u16::from_le_bytes(*pair));
    let mut text = String::with_capacity(blob.len() / 2 * 3);
    for unit in char::decode_utf16(units) {
        let Ok(c) = unit else {
            text.zeroize();
            return Err(not_text());
        };
        text.push(c);
    }
    Ok(Secret::new(text))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Security::Credentials::{CRED_PERSIST, CRED_TYPE};

    use super::*;

    /// A random profile, whose entries are removed at the end of the test.
    struct Profile(Uuid);

    impl Profile {
        fn new() -> Self {
            Self(Uuid::new_v4())
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = delete_all(self.0);
        }
    }

    fn secret(text: &str) -> Secret<String> {
        Secret::new(text.to_owned())
    }

    fn loaded(profile: &Profile, slot: SecretSlot) -> Option<String> {
        load(profile.0, slot)
            .unwrap()
            .map(|value| value.expose_secret().clone())
    }

    /// The entry as Windows has it, without going through [`decode`].
    fn raw_entry(target: &str) -> (CRED_TYPE, CRED_PERSIST, String, Vec<u8>) {
        let name = wide(target);
        let mut raw = ptr::null_mut();
        // SAFETY: as in `read`.
        unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &raw mut raw) }.unwrap();
        let mut entry = ReadCredential(raw);
        let blob = entry.blob().to_vec();
        // SAFETY: as in `ReadCredential::blob`.
        let credential = unsafe { &*entry.0 };
        // SAFETY: Windows ends the name in a nul.
        let name = unsafe { credential.TargetName.to_string() }.unwrap();
        (credential.Type, credential.Persist, name, blob)
    }

    #[test]
    fn targets_are_named_as_nfr_sec_01_says() {
        let id = Uuid::parse_str("67E55044-10B1-426F-9247-BB680E5FE0C8").unwrap();
        let of = |slot| target(id, slot);
        assert_eq!(
            of(SecretSlot::Password),
            "DriveMount:67e55044-10b1-426f-9247-bb680e5fe0c8:password"
        );
        assert_eq!(
            of(SecretSlot::Passphrase),
            "DriveMount:67e55044-10b1-426f-9247-bb680e5fe0c8:passphrase"
        );
        assert_eq!(
            of(SecretSlot::JumpPassword),
            "DriveMount:67e55044-10b1-426f-9247-bb680e5fe0c8:jump-password"
        );
    }

    #[test]
    fn a_secret_comes_back_as_stored() {
        let profile = Profile::new();
        for value in ["plain", "zażółć gęślą jaźń", "key 🔑 pair", "", " spaces "] {
            store(profile.0, SecretSlot::Password, &secret(value)).unwrap();
            assert_eq!(
                loaded(&profile, SecretSlot::Password).as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    fn the_entry_is_generic_local_and_utf16() {
        let profile = Profile::new();
        store(profile.0, SecretSlot::Passphrase, &secret("aż")).unwrap();

        let name = target(profile.0, SecretSlot::Passphrase);
        let (kind, persist, stored_name, blob) = raw_entry(&name);
        assert_eq!(kind, CRED_TYPE_GENERIC);
        assert_eq!(persist, CRED_PERSIST_LOCAL_MACHINE);
        assert_eq!(stored_name, name);
        assert_eq!(blob, [b'a', 0, 0x7C, 0x01]);
    }

    #[test]
    fn storing_again_replaces() {
        let profile = Profile::new();
        store(profile.0, SecretSlot::Password, &secret("first and longer")).unwrap();
        store(profile.0, SecretSlot::Password, &secret("second")).unwrap();
        assert_eq!(
            loaded(&profile, SecretSlot::Password).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn slots_and_profiles_are_separate() {
        let (one, two) = (Profile::new(), Profile::new());
        store(one.0, SecretSlot::Password, &secret("a")).unwrap();
        store(one.0, SecretSlot::JumpPassword, &secret("b")).unwrap();
        store(two.0, SecretSlot::Password, &secret("c")).unwrap();

        assert_eq!(loaded(&one, SecretSlot::Password).as_deref(), Some("a"));
        assert_eq!(loaded(&one, SecretSlot::JumpPassword).as_deref(), Some("b"));
        assert_eq!(loaded(&one, SecretSlot::Passphrase), None);
        assert_eq!(loaded(&two, SecretSlot::Password).as_deref(), Some("c"));
    }

    #[test]
    fn a_missing_secret_is_none() {
        let profile = Profile::new();
        assert_eq!(loaded(&profile, SecretSlot::Password), None);
    }

    #[test]
    fn delete_says_whether_there_was_one() {
        let profile = Profile::new();
        assert!(!delete(profile.0, SecretSlot::Password).unwrap());

        store(profile.0, SecretSlot::Password, &secret("value")).unwrap();
        assert!(delete(profile.0, SecretSlot::Password).unwrap());
        assert_eq!(loaded(&profile, SecretSlot::Password), None);
        assert!(!delete(profile.0, SecretSlot::Password).unwrap());
    }

    #[test]
    fn delete_all_leaves_nothing_and_only_touches_that_profile() {
        let (one, other) = (Profile::new(), Profile::new());
        for slot in SecretSlot::ALL {
            store(one.0, slot, &secret("value")).unwrap();
        }
        store(other.0, SecretSlot::Password, &secret("kept")).unwrap();

        delete_all(one.0).unwrap();

        for slot in SecretSlot::ALL {
            assert_eq!(loaded(&one, slot), None, "{slot:?}");
        }
        assert_eq!(
            loaded(&other, SecretSlot::Password).as_deref(),
            Some("kept")
        );
    }

    /// `delete_everything`, under a prefix of the test's own, so the user's own entries stay.
    #[test]
    fn deleting_by_prefix_takes_every_entry_under_it_and_nothing_else() {
        let prefix = format!("DriveMount.Test.{}:", Uuid::new_v4());
        let outside = Profile::new();
        store(outside.0, SecretSlot::Password, &secret("kept")).unwrap();
        for name in [
            "one:password",
            "two:passphrase",
            "gone-from-config:jump-password",
        ] {
            write(&format!("{prefix}{name}"), &secret("value")).unwrap();
        }

        assert_eq!(delete_starting_with(&prefix).unwrap(), 3);

        assert!(names_starting_with(&prefix).unwrap().is_empty());
        assert_eq!(
            loaded(&outside, SecretSlot::Password).as_deref(),
            Some("kept")
        );
        assert_eq!(delete_starting_with(&prefix).unwrap(), 0);
    }

    #[test]
    fn every_entry_is_under_the_prefix_delete_everything_uses() {
        for slot in SecretSlot::ALL {
            assert!(target(Uuid::new_v4(), slot).starts_with(PREFIX));
        }
    }

    #[test]
    fn delete_all_with_nothing_stored_is_fine() {
        delete_all(Uuid::new_v4()).unwrap();
    }

    #[test]
    fn the_longest_secret_fits_and_a_longer_one_changes_nothing() {
        let profile = Profile::new();
        let longest = "x".repeat(MAX_SECRET_UNITS);
        store(profile.0, SecretSlot::Password, &secret(&longest)).unwrap();
        assert_eq!(
            loaded(&profile, SecretSlot::Password),
            Some(longest.clone())
        );

        let longer = secret(&"y".repeat(MAX_SECRET_UNITS + 1));
        let error = store(profile.0, SecretSlot::Password, &longer).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!error.to_string().contains("yyy"));
        assert_eq!(loaded(&profile, SecretSlot::Password), Some(longest));
    }

    #[test]
    fn the_limit_counts_utf16_units_not_characters() {
        let profile = Profile::new();
        let pairs = "🔑".repeat(MAX_SECRET_UNITS / 2);
        store(profile.0, SecretSlot::Password, &secret(&pairs)).unwrap();
        assert_eq!(loaded(&profile, SecretSlot::Password), Some(pairs.clone()));

        let longer = secret(&(pairs + "x"));
        assert_eq!(
            store(profile.0, SecretSlot::Password, &longer)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn neither_direction_outgrows_its_first_buffer() {
        for text in ["", "abc", "żółć", "🔑🔑", "€uro"] {
            let bytes = encode(text);
            assert_eq!(bytes.expose_secret().capacity(), text.len() * 2, "{text}");
            let back = decode(bytes.expose_secret()).unwrap();
            assert_eq!(back.expose_secret(), text);
            assert_eq!(
                back.expose_secret().capacity(),
                bytes.expose_secret().len() / 2 * 3,
                "{text}"
            );
        }
    }

    #[test]
    fn an_entry_that_is_not_utf16_is_invalid_data_without_its_contents() {
        for blob in [&b"abc"[..], &[0x00, 0xD8, b'a', 0], &[0x00, 0xD8]] {
            let error = decode(blob).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!format!("{error:?}").contains("abc"));
        }
    }

    #[test]
    fn the_store_goes_through_to_credential_manager() {
        let profile = Profile::new();
        let credentials: &dyn SecretStore = &CredentialStore;

        credentials
            .store(profile.0, SecretSlot::Passphrase, &secret("value"))
            .unwrap();
        assert_eq!(
            loaded(&profile, SecretSlot::Passphrase).as_deref(),
            Some("value"),
            "under the name NFR-SEC-01 gives"
        );
        let back = credentials
            .load(profile.0, SecretSlot::Passphrase)
            .unwrap()
            .unwrap();
        assert_eq!(back.expose_secret(), "value");

        assert!(
            credentials
                .delete(profile.0, SecretSlot::Passphrase)
                .unwrap()
        );
        assert!(
            !credentials
                .delete(profile.0, SecretSlot::Passphrase)
                .unwrap()
        );

        store(profile.0, SecretSlot::Password, &secret("a")).unwrap();
        store(profile.0, SecretSlot::JumpPassword, &secret("b")).unwrap();
        credentials.delete_all(profile.0).unwrap();
        assert_eq!(loaded(&profile, SecretSlot::Password), None);
        assert_eq!(loaded(&profile, SecretSlot::JumpPassword), None);
    }

    #[test]
    fn an_entry_written_by_something_else_reads_as_invalid_data() {
        let profile = Profile::new();
        let mut name = wide(target(profile.0, SecretSlot::Password));
        let mut blob = *b"abc";
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: 3,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..CREDENTIALW::default()
        };
        // SAFETY: as in `write`.
        unsafe { CredWriteW(&raw const credential, 0) }.unwrap();

        let error = load(profile.0, SecretSlot::Password).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
