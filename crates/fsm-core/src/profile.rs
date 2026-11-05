// SPDX-License-Identifier: GPL-3.0-or-later

//! What a saved connection is (spec §6.1), its rules (the `validate` submodule, spec §7.3) and
//! splitting pasted text (the `paste` submodule, FR-PROF-05).
//!
//! A profile holds no secrets. They are in Credential Manager, under the profile's [`id`](Profile::id)
//! (NFR-SEC-01), and `remember` only says whether one is meant to be there.

mod paste;
mod validate;

use std::path::PathBuf;

use fsm_vfs::DriveLetter;
use uuid::Uuid;

pub use self::paste::{PastedHost, parse_pasted_host};
pub use self::validate::{
    DriveWarning, Field, FieldError, JumpTarget, Problem, Validation, check_cache_secs,
    check_drive, check_group, check_host, check_keepalive_secs, check_key_file, check_name,
    check_port, check_remote_path, check_user, drive_warning, parse_cache_secs, parse_drive,
    parse_jump_host, parse_keepalive_secs, parse_port, read_key_head, validate_profile,
};

/// The SSH port.
pub const DEFAULT_PORT: u16 = 22;
/// The user's home directory (FR-SSH-13).
pub const DEFAULT_REMOTE_PATH: &str = "~";
/// Seconds attributes and listings stay cached.
pub const DEFAULT_CACHE_TTL_SECS: u64 = 5;
/// Seconds between keep-alives (FR-SSH-12).
pub const DEFAULT_KEEPALIVE_SECS: u64 = 15;

/// A saved connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Never changes, including on rename (FR-PROF-02).
    pub id: Uuid,
    /// Unique, ignoring case (FR-PROF-03).
    pub name: String,
    /// The header the row is listed under (FR-PROF-09). `None` when blank.
    pub group: Option<String>,
    /// What to connect to, and how.
    pub protocol: Protocol,
    /// The letter to mount on (FR-MNT-02).
    pub drive: DriveChoice,
    /// Mount a read-only volume (FR-MNT-07).
    pub read_only: bool,
    /// Mount when the app starts (FR-MNT-08).
    pub mount_on_start: bool,
    /// Reconnect by itself when the connection is lost (FR-REC).
    pub auto_reconnect: bool,
}

impl Profile {
    /// A new profile with a new id, on an automatic letter, reconnecting by itself.
    #[must_use]
    pub fn new(name: impl Into<String>, protocol: Protocol) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            protocol,
            drive: DriveChoice::Auto,
            read_only: false,
            mount_on_start: false,
            auto_reconnect: true,
        }
    }
}

/// The fields that depend on the protocol. SMB arrives in M3 and NFS in M4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// SFTP over SSH.
    Sftp(SftpProfile),
}

/// The SFTP fields of a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpProfile {
    /// A DNS name, or an IPv4 or IPv6 address without brackets.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The user to log in as.
    pub username: String,
    /// How to log in.
    pub auth: SftpAuth,
    /// `~`, `~/below/home`, `/absolute`, or `C:/data` on Windows OpenSSH (FR-SSH-13).
    pub remote_path: String,
    /// Seconds attributes and listings stay cached.
    pub cache_ttl_secs: u64,
    /// Seconds between keep-alives; 0 turns them off (FR-SSH-12).
    pub keepalive_secs: u64,
    /// The host to go through, if any (FR-SSH-11).
    pub jump: Option<JumpHost>,
}

impl SftpProfile {
    /// The given login, with the defaults for everything else.
    #[must_use]
    pub fn new(host: impl Into<String>, username: impl Into<String>, auth: SftpAuth) -> Self {
        Self {
            host: host.into(),
            port: DEFAULT_PORT,
            username: username.into(),
            auth,
            remote_path: DEFAULT_REMOTE_PATH.to_owned(),
            cache_ttl_secs: DEFAULT_CACHE_TTL_SECS,
            keepalive_secs: DEFAULT_KEEPALIVE_SECS,
            jump: None,
        }
    }
}

/// How to log in to the server (FR-SSH-01 to FR-SSH-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpAuth {
    /// A password.
    Password {
        /// Keep it in Credential Manager; otherwise it is asked for at every mount.
        remember: bool,
    },
    /// A private key file.
    KeyFile {
        /// The private key file.
        path: PathBuf,
        /// Keep its passphrase in Credential Manager.
        remember: bool,
    },
    /// The keys in an SSH agent.
    Agent,
    /// Nothing stored: a password or a key file is asked for at every mount.
    Ask,
}

/// A host to reach the server through (FR-SSH-11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHost {
    /// A DNS name, or an IPv4 or IPv6 address without brackets.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The user to log in as there; `None` is the profile's own user, as with `ssh -J`.
    pub username: Option<String>,
    /// How to log in there.
    pub auth: JumpAuth,
}

/// How to log in to a jump host. There is no *Ask each time* (spec §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpAuth {
    /// The keys in an SSH agent.
    Agent,
    /// A private key file. Its passphrase is never stored (NFR-SEC-01 has no target for it).
    KeyFile {
        /// The private key file.
        path: PathBuf,
    },
    /// A password, stored as `FsManager:<uuid>:jump-password` if the user asks for that.
    Password,
}

/// Which letter a profile mounts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveChoice {
    /// The highest free letter from Z downwards (FR-MNT-02).
    Auto,
    /// This letter: D to Z, by [`check_drive`].
    Letter(DriveLetter),
}
