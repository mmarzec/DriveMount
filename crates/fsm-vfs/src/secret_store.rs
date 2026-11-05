// SPDX-License-Identifier: GPL-3.0-or-later

//! Where remembered secrets live (NFR-SEC-01). `fsm-win` implements [`SecretStore`] over Credential
//! Manager. It is here, not in `fsm-core`, because `fsm-win` and `fsm-core` do not depend on each
//! other (spec §5.2).

use std::io;
#[cfg(feature = "testing")]
use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, PoisonError},
};

use uuid::Uuid;

use crate::Secret;

/// Which of a profile's secrets (NFR-SEC-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// The password for the server.
    Password,
    /// The passphrase of the key file.
    Passphrase,
    /// The password for the jump host.
    JumpPassword,
}

impl SecretSlot {
    /// Every slot a profile can have.
    pub const ALL: [Self; 3] = [Self::Password, Self::Passphrase, Self::JumpPassword];
}

/// Keeps secrets under a profile's id, so a rename does not touch them (FR-PROF-02).
///
/// No error may contain the secret.
pub trait SecretStore: Send + Sync {
    /// Stores a secret, replacing any earlier one. On an error, the earlier one is still there.
    ///
    /// # Errors
    /// `InvalidInput` if the store cannot hold one this long, or the store's own error.
    fn store(&self, profile: Uuid, slot: SecretSlot, value: &Secret<String>) -> io::Result<()>;

    /// Reads a secret. `None` if there is none.
    ///
    /// # Errors
    /// `InvalidData` if the entry cannot be understood, or the store's own error.
    fn load(&self, profile: Uuid, slot: SecretSlot) -> io::Result<Option<Secret<String>>>;

    /// Deletes a secret. Returns whether there was one.
    ///
    /// # Errors
    /// The store's own error.
    fn delete(&self, profile: Uuid, slot: SecretSlot) -> io::Result<bool>;

    /// Deletes all of a profile's secrets (FR-PROF-07). A failure does not stop the other slots.
    ///
    /// # Errors
    /// The first error.
    fn delete_all(&self, profile: Uuid) -> io::Result<()> {
        let mut result = Ok(());
        for slot in SecretSlot::ALL {
            if let Err(error) = self.delete(profile, slot) {
                result = result.and(Err(error));
            }
        }
        result
    }
}

/// A [`SecretStore`] in memory, for tests.
#[cfg(feature = "testing")]
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    secrets: Mutex<HashMap<(Uuid, SecretSlot), Secret<String>>>,
}

#[cfg(feature = "testing")]
impl MemorySecretStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many secrets it holds, of all profiles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.secrets().len()
    }

    /// Whether it holds no secrets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secrets().is_empty()
    }

    fn secrets(&self) -> MutexGuard<'_, HashMap<(Uuid, SecretSlot), Secret<String>>> {
        self.secrets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(feature = "testing")]
impl SecretStore for MemorySecretStore {
    fn store(&self, profile: Uuid, slot: SecretSlot, value: &Secret<String>) -> io::Result<()> {
        let copy = Secret::new(value.expose_secret().clone());
        self.secrets().insert((profile, slot), copy);
        Ok(())
    }

    fn load(&self, profile: Uuid, slot: SecretSlot) -> io::Result<Option<Secret<String>>> {
        let secrets = self.secrets();
        let stored = secrets.get(&(profile, slot));
        Ok(stored.map(|value| Secret::new(value.expose_secret().clone())))
    }

    fn delete(&self, profile: Uuid, slot: SecretSlot) -> io::Result<bool> {
        Ok(self.secrets().remove(&(profile, slot)).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(text: &str) -> Secret<String> {
        Secret::new(text.to_owned())
    }

    fn loaded(store: &dyn SecretStore, profile: Uuid, slot: SecretSlot) -> Option<String> {
        store
            .load(profile, slot)
            .unwrap()
            .map(|value| value.expose_secret().clone())
    }

    #[test]
    fn slots_and_profiles_are_separate_and_storing_again_replaces() {
        let store = MemorySecretStore::new();
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        store
            .store(one, SecretSlot::Password, &secret("a"))
            .unwrap();
        store
            .store(one, SecretSlot::Password, &secret("b"))
            .unwrap();
        store
            .store(one, SecretSlot::Passphrase, &secret("c"))
            .unwrap();
        store
            .store(two, SecretSlot::Password, &secret("d"))
            .unwrap();

        assert_eq!(
            loaded(&store, one, SecretSlot::Password).as_deref(),
            Some("b")
        );
        assert_eq!(
            loaded(&store, one, SecretSlot::Passphrase).as_deref(),
            Some("c")
        );
        assert_eq!(loaded(&store, one, SecretSlot::JumpPassword), None);
        assert_eq!(
            loaded(&store, two, SecretSlot::Password).as_deref(),
            Some("d")
        );
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn delete_says_whether_there_was_one() {
        let store = MemorySecretStore::new();
        let profile = Uuid::new_v4();
        assert!(!store.delete(profile, SecretSlot::Password).unwrap());
        store
            .store(profile, SecretSlot::Password, &secret("a"))
            .unwrap();
        assert!(store.delete(profile, SecretSlot::Password).unwrap());
        assert!(store.is_empty());
    }

    #[test]
    fn delete_all_only_touches_that_profile() {
        let store: &dyn SecretStore = &MemorySecretStore::new();
        let (one, other) = (Uuid::new_v4(), Uuid::new_v4());
        for slot in SecretSlot::ALL {
            store.store(one, slot, &secret("value")).unwrap();
        }
        store
            .store(other, SecretSlot::Password, &secret("kept"))
            .unwrap();

        store.delete_all(one).unwrap();

        for slot in SecretSlot::ALL {
            assert_eq!(loaded(store, one, slot), None, "{slot:?}");
        }
        assert_eq!(
            loaded(store, other, SecretSlot::Password).as_deref(),
            Some("kept")
        );
    }

    /// One slot failing must not leave the others behind.
    #[test]
    fn delete_all_goes_on_after_a_failure_and_reports_the_first() {
        struct Failing(MemorySecretStore);

        impl SecretStore for Failing {
            fn store(&self, p: Uuid, s: SecretSlot, v: &Secret<String>) -> io::Result<()> {
                self.0.store(p, s, v)
            }
            fn load(&self, p: Uuid, s: SecretSlot) -> io::Result<Option<Secret<String>>> {
                self.0.load(p, s)
            }
            fn delete(&self, p: Uuid, s: SecretSlot) -> io::Result<bool> {
                if s == SecretSlot::Password {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                self.0.delete(p, s)
            }
        }

        let store = Failing(MemorySecretStore::new());
        let profile = Uuid::new_v4();
        for slot in SecretSlot::ALL {
            store.store(profile, slot, &secret("value")).unwrap();
        }

        let error = store.delete_all(profile).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(store.0.len(), 1, "only the slot that failed is left");
    }

    #[test]
    fn debug_does_not_show_the_secrets() {
        let store = MemorySecretStore::new();
        store
            .store(Uuid::new_v4(), SecretSlot::Password, &secret("hunter2"))
            .unwrap();
        assert!(!format!("{store:?}").contains("hunter2"));
    }
}
