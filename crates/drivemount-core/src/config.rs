// SPDX-License-Identifier: GPL-3.0-or-later

//! `config.toml`: settings and profiles on disk (spec §6.1, NFR-REL-05).
//!
//! [`ConfigStore::open`] never fails. What it could not read leaves it read-only, with the reason in
//! [`read_only`](ConfigStore::read_only), and a read-only store never touches the disk. How much of
//! the file a fault costs depends on where it is:
//!
//! | Fault | Result |
//! |---|---|
//! | The file is not TOML, has no valid `schema_version`, or has the wrong shape | Read-only, with nothing loaded |
//! | A newer `schema_version` | Read-only, with what can be understood |
//! | A profile that does not fit the model (`port = 70000`, `protocol = "smb"`) | That profile is in [`Config::unreadable`], and is written back as it was |
//! | A profile that fits but breaks a rule (`keepalive_secs = 9999`) | Loaded, and listed by [`Config::invalid_profiles`] |
//! | A setting with a bad value (`theme = "purple"`) | Its default, listed by [`setting_problems`](ConfigStore::setting_problems) |
//!
//! Nothing here reads a key file: a key on a drive that is not plugged in is not a broken config.
//!
//! The file holds no secrets (NFR-SEC-02): the model has no field for one. They go through
//! [`SecretStore`].

mod file;
mod record;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use drivemount_vfs::Secret;
pub use drivemount_vfs::{SecretSlot, SecretStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::Table;
use uuid::Uuid;

use self::file::{Decoded, Kept, Schema};
use crate::profile::{FieldError, Profile, validate_profile};

/// The `schema_version` this build writes.
pub const SCHEMA_VERSION: u32 = 1;

/// The migrations, in order. Each takes the file from one version to the next, so there is one for
/// every version below [`SCHEMA_VERSION`] (spec §6.1). There is only one version so far.
const SCHEMA: Schema = Schema {
    current: SCHEMA_VERSION,
    steps: &[],
};

/// What the file holds, without what is only kept to be written back.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// The `[settings]` table.
    pub settings: Settings,
    /// The profiles the model can hold, in the file's order.
    pub profiles: Vec<Profile>,
    /// The profiles it cannot. Written back after the others, unless taken out of this list.
    pub unreadable: Vec<UnreadableProfile>,
}

impl Config {
    /// The profiles that break a rule of spec §7.3, with what is wrong. Only a key file's name is
    /// looked at, and not the file.
    #[must_use]
    pub fn invalid_profiles(&self) -> Vec<(Uuid, Vec<FieldError>)> {
        const A_PRIVATE_KEY: &[u8] = b"-----BEGIN OPENSSH PRIVATE KEY-----";

        let any_key_is_there = |_: &Path| Ok(Secret::new(A_PRIVATE_KEY.to_vec()));
        self.profiles
            .iter()
            .filter_map(|profile| {
                let errors = validate_profile(profile, &self.profiles, &any_key_is_there).errors;
                (!errors.is_empty()).then_some((profile.id, errors))
            })
            .collect()
    }
}

/// The `[settings]` table (FR-SET). The range of a value is checked when it is read from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settings {
    /// FR-SET-01.
    pub start_with_windows: bool,
    /// FR-SET-02.
    pub close_to_tray: bool,
    /// FR-SET-03.
    pub theme: Theme,
    /// FR-SET-04.
    pub letter_conflict: LetterConflict,
    /// FR-SET-05.
    pub notifications: bool,
    /// FR-SET-06.
    pub log_level: LogLevel,
    /// Not in the UI. 5–120.
    pub op_timeout_secs: u64,
    /// Not in the UI. 0 is the number of cores, and the most is 8.
    pub winfsp_threads: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            start_with_windows: false,
            close_to_tray: true,
            theme: Theme::System,
            letter_conflict: LetterConflict::Auto,
            notifications: true,
            log_level: LogLevel::Info,
            op_timeout_secs: 15,
            winfsp_threads: 0,
        }
    }
}

/// FR-SET-03.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow Windows.
    System,
    /// Light.
    Light,
    /// Dark.
    Dark,
}

/// What to do when a profile's letter is in use (FR-SET-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LetterConflict {
    /// Use another free letter.
    Auto,
    /// Don't mount.
    Fail,
}

/// FR-SET-06.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Error.
    Error,
    /// Warning.
    Warn,
    /// Info.
    Info,
    /// Debug.
    Debug,
}

/// A setting whose value was not used, so its default was. Saving replaces the value in the file.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{key}: {detail}")]
pub struct SettingProblem {
    /// The key in `[settings]`.
    pub key: &'static str,
    /// What is wrong with it.
    pub detail: String,
}

/// A `[[profiles]]` entry the model cannot hold. Kept as it is, so saving does not lose it.
#[derive(Debug, Clone, PartialEq)]
pub struct UnreadableProfile {
    /// Its `name`, if it has one, to tell the user which it is.
    pub name: Option<String>,
    /// Its `id`, if it has a valid one, to delete its secrets along with it.
    pub id: Option<Uuid>,
    /// What is wrong with it.
    pub problem: String,
    raw: Table,
}

/// Why a store will not write.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReadOnlyReason {
    /// The file is not a configuration file this version could ever have written.
    #[error("The configuration file is damaged: {detail}")]
    Damaged {
        /// What is wrong with it.
        detail: String,
    },
    /// The file could not be read, so what is in it is not known.
    #[error("The configuration file could not be read: {detail}")]
    CannotRead {
        /// The error from the system.
        detail: String,
    },
    /// Spec §6.1's text.
    #[error("This configuration was written by a newer DriveMount")]
    Newer {
        /// Its `schema_version`.
        found: i64,
    },
    /// A migration failed. The file is as it was.
    #[error("The configuration file could not be updated from version {from}: {detail}")]
    MigrationFailed {
        /// The version the failing migration starts from.
        from: u32,
        /// What is wrong.
        detail: String,
    },
}

/// Why a save did not happen. The file on disk is as it was in all cases, though the backups may
/// have been written.
#[derive(Debug, Error)]
pub enum SaveError {
    /// The store is read-only.
    #[error(transparent)]
    ReadOnly(ReadOnlyReason),
    /// A key file's path is not Unicode, so TOML cannot hold it.
    #[error("The key file path of '{profile}' cannot be saved")]
    PathNotText {
        /// The profile's name.
        profile: String,
    },
    /// The file could not be put together.
    #[error("The configuration could not be written: {0}")]
    Encode(#[from] toml::ser::Error),
    /// The file would not read back, as with a number too large for TOML, which the writer lets by.
    #[error("The configuration could not be written: {detail}")]
    WouldNotLoad {
        /// What reading it says.
        detail: String,
    },
    /// The disk said no.
    #[error("The configuration could not be saved: {0}")]
    Io(#[from] io::Error),
}

/// `config.toml`, and everything in it that the model has no place for.
///
/// There is one per process. Changes made to the file while it is open are overwritten by the next
/// save, unless they broke it: then the save is refused, and the store turns read-only.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    schema: Schema,
    config: Config,
    kept: Kept,
    setting_problems: Vec<SettingProblem>,
    read_only: Option<ReadOnlyReason>,
}

impl ConfigStore {
    /// `%APPDATA%\DriveMount\config.toml`. `None` if `APPDATA` is not set.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        path_under(env::var_os("APPDATA"))
    }

    /// Reads the file. If there is none, this is a first run: the defaults, with nothing written
    /// until the first [`save`](Self::save).
    #[must_use]
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self::open_with(path.into(), SCHEMA)
    }

    fn open_with(path: PathBuf, schema: Schema) -> Self {
        let mut store = Self {
            path,
            schema,
            config: Config::default(),
            kept: Kept::default(),
            setting_problems: Vec::new(),
            read_only: None,
        };
        let decoded = match fs::read(&store.path) {
            Ok(bytes) => file::decode(&schema, &bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => return store,
            Err(error) => Err(ReadOnlyReason::CannotRead {
                detail: error.to_string(),
            }),
        };
        match decoded {
            Ok(decoded) => {
                store.read_only = decoded.newer();
                store.config = decoded.config;
                store.kept = decoded.kept;
                store.setting_problems = decoded.setting_problems;
            }
            Err(reason) => store.read_only = Some(reason),
        }
        store
    }

    /// The file this store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last good file: `config.toml.bak`, next to it.
    #[must_use]
    pub fn backup_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_owned();
        name.push(".bak");
        self.path.with_file_name(name)
    }

    /// What was last read or saved.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Why [`save`](Self::save) will refuse, which the app has to show (NFR-REL-05).
    #[must_use]
    pub const fn read_only(&self) -> Option<&ReadOnlyReason> {
        self.read_only.as_ref()
    }

    /// The settings in the file that were not used.
    #[must_use]
    pub fn setting_problems(&self) -> &[SettingProblem] {
        &self.setting_problems
    }

    /// Writes `config`, with the previous file as `config.toml.bak`.
    ///
    /// A file that was migrated on opening is first kept as `config.v<N>.bak`. Nothing is written if
    /// the file would not change, so the backup stays one version behind. `config` is not checked
    /// against spec §7.3: that is for the Save command, which knows about the dialog.
    ///
    /// # Errors
    ///
    /// See [`SaveError`]. A file on disk that has become unreadable turns the store read-only.
    pub fn save(&mut self, config: &Config) -> Result<(), SaveError> {
        if let Some(reason) = &self.read_only {
            return Err(SaveError::ReadOnly(reason.clone()));
        }
        let (text, records) = file::encode(self.schema.current, config, &self.kept)?;
        // A save must not be what breaks the file.
        file::decode(&self.schema, text.as_bytes()).map_err(|reason| SaveError::WouldNotLoad {
            detail: reason.to_string(),
        })?;
        let current = match fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let unchanged = current.as_deref() == Some(text.as_bytes());
        if !unchanged {
            if let Some(bytes) = current {
                let on_disk = self.good_file_on_disk(&bytes)?;
                if on_disk.version < self.schema.current {
                    let from = on_disk.version;
                    write_atomic(&self.versioned_backup_path(from), &bytes)?;
                }
                write_atomic(&self.backup_path(), &bytes)?;
            }
            write_atomic(&self.path, text.as_bytes())?;
        }
        self.config = config.clone();
        self.kept.records = records;
        self.setting_problems.clear();
        Ok(())
    }

    /// A broken file is never overwritten, and never replaces the backup.
    fn good_file_on_disk(&mut self, bytes: &[u8]) -> Result<Decoded, SaveError> {
        let reason = match file::decode(&self.schema, bytes) {
            Ok(decoded) => match decoded.newer() {
                None => return Ok(decoded),
                Some(reason) => reason,
            },
            Err(reason) => reason,
        };
        self.read_only = Some(reason.clone());
        Err(SaveError::ReadOnly(reason))
    }

    fn versioned_backup_path(&self, version: u32) -> PathBuf {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        self.path.with_file_name(format!("{stem}.v{version}.bak"))
    }
}

fn path_under(appdata: Option<OsString>) -> Option<PathBuf> {
    let appdata = appdata.filter(|path| !path.is_empty())?;
    Some(
        PathBuf::from(appdata)
            .join("DriveMount")
            .join("config.toml"),
    )
}

/// A crash leaves either the old file or the new one, whole, and at worst a stray temp file next to
/// it. `persist` is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows, and `rename` elsewhere.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let folder = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(folder)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".drivemount-config-")
        .suffix(".tmp")
        .tempfile_in(folder)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
