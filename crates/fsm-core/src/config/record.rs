// SPDX-License-Identifier: GPL-3.0-or-later

//! A `[[profiles]]` entry as spec §6.1 has it. The file is flat and the model is enums, so the model
//! knows nothing of serde and these types know nothing of its rules.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Table;
use uuid::Uuid;

use super::SaveError;
use crate::profile::{
    DEFAULT_CACHE_TTL_SECS, DEFAULT_KEEPALIVE_SECS, DEFAULT_PORT, DEFAULT_REMOTE_PATH, DriveChoice,
    JumpAuth, JumpHost, Profile, Protocol, SftpAuth, SftpProfile, parse_drive,
};

const AUTO: &str = "auto";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ProfileRecord {
    id: Uuid,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    protocol: ProtocolRecord,
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    username: String,
    auth: AuthRecord,
    /// Kept for an auth method that has no use for it, as the model has nowhere to hold it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_path: Option<String>,
    /// The same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remember: Option<bool>,
    #[serde(default = "default_remote_path")]
    remote_path: String,
    #[serde(default = "default_drive")]
    drive_letter: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    mount_on_start: bool,
    #[serde(default = "default_true")]
    auto_reconnect: bool,
    #[serde(default = "default_cache")]
    cache_ttl_secs: u64,
    #[serde(default = "default_keepalive")]
    keepalive_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jump: Option<JumpRecord>,
    #[serde(flatten)]
    extra: Table,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ProtocolRecord {
    Sftp,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AuthRecord {
    Password,
    Key,
    Agent,
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JumpRecord {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    auth: JumpAuthRecord,
    /// As on [`ProfileRecord::key_path`]. Spec §6.1 shows it blank with `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_path: Option<String>,
    #[serde(flatten)]
    extra: Table,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JumpAuthRecord {
    Agent,
    Key,
    Password,
}

const fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_remote_path() -> String {
    DEFAULT_REMOTE_PATH.to_owned()
}

fn default_drive() -> String {
    AUTO.to_owned()
}

const fn default_true() -> bool {
    true
}

const fn default_cache() -> u64 {
    DEFAULT_CACHE_TTL_SECS
}

const fn default_keepalive() -> u64 {
    DEFAULT_KEEPALIVE_SECS
}

/// Puts the values in `extra` back as they are in the table it was read from. `flatten` buffers them
/// on the way, and a date does not survive that: it comes out as a table.
fn restore_exact_values(extra: &mut Table, raw: &Table) {
    for (key, value) in extra.iter_mut() {
        if let Some(exact) = raw.get(key) {
            value.clone_from(exact);
        }
    }
}

impl ProfileRecord {
    /// Reads an entry of `[[profiles]]`.
    ///
    /// # Errors
    ///
    /// When it does not have the keys and types of spec §6.1.
    pub fn read(raw: &Table) -> Result<Self, toml::de::Error> {
        let mut record: Self = toml::Value::Table(raw.clone()).try_into()?;
        restore_exact_values(&mut record.extra, raw);
        if let (Some(jump), Some(raw_jump)) = (
            &mut record.jump,
            raw.get("jump").and_then(toml::Value::as_table),
        ) {
            restore_exact_values(&mut jump.extra, raw_jump);
        }
        Ok(record)
    }

    /// The model's view of it. No key file is looked at, and the rules of spec §7.3 are not applied,
    /// except that a drive that is not Auto or D–Z has no place in the model's text form.
    ///
    /// # Errors
    ///
    /// What to say about the profile.
    pub fn to_profile(&self) -> Result<Profile, String> {
        let drive = parse_drive(&self.drive_letter).map_err(|problem| problem.to_string())?;
        let key_path = || PathBuf::from(self.key_path.clone().unwrap_or_default());
        let remember = self.remember.unwrap_or(false);
        let auth = match self.auth {
            AuthRecord::Password => SftpAuth::Password { remember },
            AuthRecord::Key => SftpAuth::KeyFile {
                path: key_path(),
                remember,
            },
            AuthRecord::Agent => SftpAuth::Agent,
            AuthRecord::Ask => SftpAuth::Ask,
        };
        let protocol = match self.protocol {
            ProtocolRecord::Sftp => Protocol::Sftp(SftpProfile {
                host: self.host.clone(),
                port: self.port,
                username: self.username.clone(),
                auth,
                remote_path: self.remote_path.clone(),
                cache_ttl_secs: self.cache_ttl_secs,
                keepalive_secs: self.keepalive_secs,
                jump: self.jump.as_ref().map(JumpRecord::to_jump),
            }),
        };
        Ok(Profile {
            id: self.id,
            name: self.name.clone(),
            group: self.group.clone().filter(|group| !group.trim().is_empty()),
            protocol,
            drive,
            read_only: self.read_only,
            mount_on_start: self.mount_on_start,
            auto_reconnect: self.auto_reconnect,
        })
    }

    /// The record for a profile. `previous`, its record on file, gives what the model does not hold.
    ///
    /// # Errors
    ///
    /// [`SaveError::PathNotText`].
    pub fn from_profile(profile: &Profile, previous: Option<&Self>) -> Result<Self, SaveError> {
        let Protocol::Sftp(sftp) = &profile.protocol;
        let path_text = |path: &Path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| SaveError::PathNotText {
                    profile: profile.name.clone(),
                })
        };

        let mut key_path = previous.and_then(|record| record.key_path.clone());
        let mut remember = previous.and_then(|record| record.remember);
        let auth = match &sftp.auth {
            SftpAuth::Password { remember: chosen } => {
                remember = Some(*chosen);
                AuthRecord::Password
            }
            SftpAuth::KeyFile {
                path,
                remember: chosen,
            } => {
                key_path = Some(path_text(path)?);
                remember = Some(*chosen);
                AuthRecord::Key
            }
            SftpAuth::Agent => AuthRecord::Agent,
            SftpAuth::Ask => AuthRecord::Ask,
        };

        let previous_jump = previous.and_then(|record| record.jump.as_ref());
        let jump = match &sftp.jump {
            Some(jump) => Some(JumpRecord::from_jump(jump, previous_jump, &path_text)?),
            None => None,
        };

        Ok(Self {
            id: profile.id,
            name: profile.name.clone(),
            group: profile.group.clone(),
            protocol: ProtocolRecord::Sftp,
            host: sftp.host.clone(),
            port: sftp.port,
            username: sftp.username.clone(),
            auth,
            key_path,
            remember,
            remote_path: sftp.remote_path.clone(),
            drive_letter: match profile.drive {
                DriveChoice::Auto => AUTO.to_owned(),
                DriveChoice::Letter(letter) => letter.as_char().to_string(),
            },
            read_only: profile.read_only,
            mount_on_start: profile.mount_on_start,
            auto_reconnect: profile.auto_reconnect,
            cache_ttl_secs: sftp.cache_ttl_secs,
            keepalive_secs: sftp.keepalive_secs,
            jump,
            extra: previous
                .map(|record| record.extra.clone())
                .unwrap_or_default(),
        })
    }
}

impl JumpRecord {
    fn to_jump(&self) -> JumpHost {
        JumpHost {
            host: self.host.clone(),
            port: self.port,
            // No user is the profile's own, and spec §6.1 shows blank fields as "".
            username: self.username.clone().filter(|user| !user.is_empty()),
            auth: match self.auth {
                JumpAuthRecord::Agent => JumpAuth::Agent,
                JumpAuthRecord::Key => JumpAuth::KeyFile {
                    path: PathBuf::from(self.key_path.clone().unwrap_or_default()),
                },
                JumpAuthRecord::Password => JumpAuth::Password,
            },
        }
    }

    fn from_jump(
        jump: &JumpHost,
        previous: Option<&Self>,
        path_text: &dyn Fn(&Path) -> Result<String, SaveError>,
    ) -> Result<Self, SaveError> {
        let mut key_path = previous.and_then(|record| record.key_path.clone());
        let auth = match &jump.auth {
            JumpAuth::Agent => JumpAuthRecord::Agent,
            JumpAuth::KeyFile { path } => {
                key_path = Some(path_text(path)?);
                JumpAuthRecord::Key
            }
            JumpAuth::Password => JumpAuthRecord::Password,
        };
        Ok(Self {
            host: jump.host.clone(),
            port: jump.port,
            username: jump.username.clone(),
            auth,
            key_path,
            extra: previous
                .map(|record| record.extra.clone())
                .unwrap_or_default(),
        })
    }
}
