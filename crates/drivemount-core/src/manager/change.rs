// SPDX-License-Identifier: GPL-3.0-or-later

//! What Save and Delete do to the file and to the secrets (spec §6.2, FR-PROF-04, FR-PROF-07,
//! FR-PROF-08). Everything here blocks, so it runs off the async worker threads.
//!
//! Secrets are deleted before the file changes. If the file then cannot be written, the worst that
//! has happened is a question at the next mount, and trying again works. The other way round, a
//! profile could be gone from the file with its secrets left where nothing would ever find them.

use drivemount_vfs::{Secret, SecretSlot};
use tracing::warn;
use uuid::Uuid;

use super::{ProfileChangeError, Shared};
use crate::config::UnreadableProfile;
use crate::profile::{JumpAuth, Profile, Protocol, SftpAuth, read_key_head, validate_profile};

/// What FR-PROF-08 locks while a profile is in use. `cache_ttl_secs` and `keepalive_secs` are in the
/// protocol too: the drive took them when it started, so changing them could only mislead.
fn connection_changed(old: &Profile, new: &Profile) -> bool {
    old.protocol != new.protocol || old.drive != new.drive || old.read_only != new.read_only
}

/// The secrets this profile says it has. Nothing else may stay in the store (spec §6.2), and *Ask
/// each time* never has any (FR-SSH-05).
fn may_keep(profile: &Profile, slot: SecretSlot) -> bool {
    let Protocol::Sftp(sftp) = &profile.protocol;
    match slot {
        SecretSlot::Password => matches!(sftp.auth, SftpAuth::Password { remember: true }),
        SecretSlot::Passphrase => matches!(sftp.auth, SftpAuth::KeyFile { remember: true, .. }),
        SecretSlot::JumpPassword => jump_uses_a_password(profile),
    }
}

fn jump_uses_a_password(profile: &Profile) -> bool {
    let Protocol::Sftp(sftp) = &profile.protocol;
    matches!(
        sftp.jump.as_ref().map(|jump| &jump.auth),
        Some(JumpAuth::Password)
    )
}

/// The passphrase of another key file is of no use, and a wrong one is E-AUTH with no second prompt.
fn key_file_changed(old: &Profile, new: &Profile) -> bool {
    let Protocol::Sftp(old) = &old.protocol;
    let Protocol::Sftp(new) = &new.protocol;
    matches!(
        (&old.auth, &new.auth),
        (SftpAuth::KeyFile { path: from, .. }, SftpAuth::KeyFile { path: to, .. }) if from != to
    )
}

/// The Save command. `locked` is whether the profile is in use (FR-PROF-08).
pub(super) fn save(
    shared: &Shared,
    profile: &Profile,
    secrets: &[(SecretSlot, Secret<String>)],
    locked: bool,
) -> Result<(), ProfileChangeError> {
    let mut store = shared.store.blocking_lock();
    if let Some(reason) = store.read_only() {
        return Err(ProfileChangeError::ReadOnly(reason.clone()));
    }
    let mut config = store.config().clone();
    let position = config.profiles.iter().position(|p| p.id == profile.id);
    let old = position.map(|position| &config.profiles[position]);
    if locked && old.is_some_and(|old| connection_changed(old, profile)) {
        return Err(ProfileChangeError::ConnectionFieldsLocked);
    }
    let validation = validate_profile(profile, &config.profiles, &read_key_head);
    if !validation.errors.is_empty() {
        return Err(ProfileChangeError::Invalid(validation.errors));
    }

    for slot in SecretSlot::ALL {
        let outdated =
            slot == SecretSlot::Passphrase && old.is_some_and(|old| key_file_changed(old, profile));
        if outdated || !may_keep(profile, slot) {
            shared
                .secrets
                .delete(profile.id, slot)
                .map_err(ProfileChangeError::DeletingSecrets)?;
        }
    }

    match position {
        Some(position) => config.profiles[position] = profile.clone(),
        None => config.profiles.push(profile.clone()),
    }
    store.save(&config).map_err(ProfileChangeError::Save)?;

    for (slot, secret) in secrets {
        if may_keep(profile, *slot) {
            shared
                .secrets
                .store(profile.id, *slot, secret)
                .map_err(ProfileChangeError::StoringSecret)?;
        }
    }
    Ok(())
}

/// The Delete command, once the profile is no longer in use (FR-PROF-07).
pub(super) fn delete(shared: &Shared, id: Uuid) -> Result<(), ProfileChangeError> {
    let mut store = shared.store.blocking_lock();
    if let Some(reason) = store.read_only() {
        return Err(ProfileChangeError::ReadOnly(reason.clone()));
    }
    shared
        .secrets
        .delete_all(id)
        .map_err(ProfileChangeError::DeletingSecrets)?;
    let mut config = store.config().clone();
    config.profiles.retain(|profile| profile.id != id);
    store.save(&config).map_err(ProfileChangeError::Save)
}

/// Takes one unreadable entry out of the file, with its secrets if the secrets are only its own: a
/// second use of an id must not cost the profile that has it its secrets.
pub(super) fn delete_unreadable(
    shared: &Shared,
    entry: &UnreadableProfile,
) -> Result<(), ProfileChangeError> {
    let mut store = shared.store.blocking_lock();
    if let Some(reason) = store.read_only() {
        return Err(ProfileChangeError::ReadOnly(reason.clone()));
    }
    let mut config = store.config().clone();
    let position = config
        .unreadable
        .iter()
        .position(|listed| listed == entry)
        .ok_or(ProfileChangeError::UnknownProfile)?;
    config.unreadable.remove(position);
    if let Some(id) = entry.id
        && !config.profiles.iter().any(|profile| profile.id == id)
        && !config.unreadable.iter().any(|other| other.id == Some(id))
    {
        shared
            .secrets
            .delete_all(id)
            .map_err(ProfileChangeError::DeletingSecrets)?;
    }
    store.save(&config).map_err(ProfileChangeError::Save)
}

/// Stores what the user asked the backend to remember, and marks the profile, so the dialog shows
/// *Remember* ticked and the next Save keeps the secret. A failure costs a question at the next mount
/// and no more, so it goes to the log: the drive is up by now.
pub(super) fn keep_remembered(
    shared: &Shared,
    id: Uuid,
    remembered: &[(SecretSlot, Secret<String>)],
) {
    let mut store = shared.store.blocking_lock();
    let mut config = store.config().clone();
    let Some(profile) = config.profiles.iter_mut().find(|p| p.id == id) else {
        return;
    };
    for (slot, secret) in remembered {
        if !has_use_for(profile, *slot) {
            warn!(%id, ?slot, "a secret this profile has no use for was not stored");
            continue;
        }
        if let Err(error) = shared.secrets.store(id, *slot, secret) {
            warn!(%id, ?slot, %error, "could not remember a secret");
            continue;
        }
        tick_remember(profile, *slot);
    }
    if *store.config() != config
        && store.read_only().is_none()
        && let Err(error) = store.save(&config)
    {
        warn!(%id, %error, "could not mark the profile as remembering its secret");
    }
}

/// Whether a backend can have had a reason to remember this: `remember` may still be off, as the user
/// has only now ticked it in the prompt.
fn has_use_for(profile: &Profile, slot: SecretSlot) -> bool {
    let Protocol::Sftp(sftp) = &profile.protocol;
    match slot {
        SecretSlot::Password => matches!(sftp.auth, SftpAuth::Password { .. }),
        SecretSlot::Passphrase => matches!(sftp.auth, SftpAuth::KeyFile { .. }),
        SecretSlot::JumpPassword => jump_uses_a_password(profile),
    }
}

/// A jump host has no flag: whether its password is stored is all there is.
const fn tick_remember(profile: &mut Profile, slot: SecretSlot) {
    let Protocol::Sftp(sftp) = &mut profile.protocol;
    if let (SecretSlot::Password, SftpAuth::Password { remember })
    | (SecretSlot::Passphrase, SftpAuth::KeyFile { remember, .. }) = (slot, &mut sftp.auth)
    {
        *remember = true;
    }
}
