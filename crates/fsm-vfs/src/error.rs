// SPDX-License-Identifier: GPL-3.0-or-later

//! The error model. [`FsError`] is what file operations return and maps to NTSTATUS (spec §5.8).
//! [`ConnectError`] and [`MountError`] map to the user-facing codes of spec §7.8, which each variant
//! names. Error text must never contain a secret (NFR-SEC-02).

use std::path::PathBuf;

use thiserror::Error;

use crate::DriveLetter;

/// The result of a file operation.
pub type FsResult<T> = Result<T, FsError>;

/// Why a file operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FsError {
    /// The file does not exist.
    #[error("not found")]
    NotFound,
    /// A parent directory of the path does not exist.
    #[error("path not found")]
    PathNotFound,
    /// The server refused access.
    #[error("permission denied")]
    PermissionDenied,
    /// The target already exists.
    #[error("already exists")]
    Exists,
    /// The directory is not empty.
    #[error("directory not empty")]
    NotEmpty,
    /// The server or backend cannot do this.
    #[error("not supported")]
    NotSupported,
    /// The volume is read-only.
    #[error("read-only volume")]
    ReadOnly,
    /// The operation deadline passed.
    #[error("timed out")]
    Timeout,
    /// The session is gone.
    #[error("connection lost")]
    ConnectionLost,
    /// The remote disk is full.
    #[error("no space left")]
    NoSpace,
    /// The session cannot hold another open file. Another open may work once one has closed.
    #[error("too many open files")]
    TooManyOpenFiles,
    /// Anything else, with details for the log.
    #[error("{0}")]
    Other(String),
}

/// The details of a refused changed host key (FR-SSH-10), for the dialog in spec §7.4.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedHostKey {
    /// The host name as the profile gives it.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// SHA256 fingerprint of the key in `known_hosts`, formatted like `ssh-keygen -l`.
    pub old_fingerprint: String,
    /// SHA256 fingerprint of the key the server presented.
    pub new_fingerprint: String,
    /// The `known_hosts` file holding the old key.
    pub known_hosts: PathBuf,
    /// The 1-based line of the old key in that file.
    pub line: u32,
}

/// The details of a refused revoked host key (FR-SSH-08).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedHostKey {
    /// The host name as the profile gives it.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// SHA256 fingerprint of the key the server presented.
    pub fingerprint: String,
    /// The `known_hosts` file holding the `@revoked` line.
    pub known_hosts: PathBuf,
    /// The 1-based line of the `@revoked` entry in that file.
    pub line: u32,
}

/// Why a connection could not be set up.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConnectError {
    /// The user cancelled a prompt. Not shown as an error: the mount returns to *Disconnected*.
    #[error("cancelled")]
    Cancelled,
    /// E-AUTH.
    #[error("authentication failed for {user}@{host}")]
    Auth {
        /// The user name tried.
        user: String,
        /// The host.
        host: String,
    },
    /// E-HOSTKEY-CHANGED. There is no way to override this.
    #[error("server key has changed: connection refused")]
    HostKeyChanged(Box<ChangedHostKey>),
    /// E-HOSTKEY-REVOKED. There is no way to override this.
    #[error("server key has been revoked: connection refused")]
    HostKeyRevoked(Box<RevokedHostKey>),
    /// E-HOSTKEY-REJECTED: the user did not trust an unknown key.
    #[error("server key not trusted")]
    HostKeyRejected,
    /// E-DNS.
    #[error("host {host} not found")]
    Dns {
        /// The host.
        host: String,
    },
    /// E-HOST-UNREACHABLE.
    #[error("cannot reach {host}:{port}")]
    HostUnreachable {
        /// The host.
        host: String,
        /// The TCP port.
        port: u16,
    },
    /// E-TIMEOUT.
    #[error("{host} did not respond in time")]
    Timeout {
        /// The host.
        host: String,
    },
    /// E-PATH-NOT-FOUND: the profile's remote path.
    #[error("remote path {path} not found")]
    PathNotFound {
        /// The path as the profile gives it, which may be unresolved, such as `~/data`.
        path: String,
    },
    /// E-PATH-NOT-DIR: the profile's remote path.
    #[error("remote path {path} is not a folder")]
    PathNotDir {
        /// The path as the profile gives it.
        path: String,
    },
    /// E-SFTP-UNAVAILABLE.
    #[error("the server does not provide SFTP")]
    SftpUnavailable,
    /// E-ALGORITHMS: the server offers only algorithms that spec §8 leaves out.
    #[error("no encryption method in common with the server")]
    NoCommonAlgorithm,
    /// E-KEY-FORMAT.
    #[error("unsupported key file format")]
    KeyFormat,
    /// E-CONN-LOST: the connection dropped during setup, or a probe found it gone.
    #[error("connection lost")]
    ConnectionLost,
    /// E-INTERNAL, with details for the log.
    #[error("{0}")]
    Other(String),
}

/// Why a mount or unmount failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MountError {
    /// The connection could not be set up.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// E-LETTER-TAKEN: the letter was taken by the time the mount ran.
    #[error("drive {0} is already in use")]
    LetterTaken(DriveLetter),
    /// E-WINFSP-MISSING.
    #[error("WinFsp 2.1 or later is required")]
    WinFspMissing,
    /// E-INTERNAL, with details for the log.
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_their_details() {
        let auth = ConnectError::Auth {
            user: "deploy".to_owned(),
            host: "10.0.4.21".to_owned(),
        };
        assert_eq!(
            auth.to_string(),
            "authentication failed for deploy@10.0.4.21"
        );

        let letter = DriveLetter::new('z').map(MountError::LetterTaken);
        assert_eq!(letter.unwrap().to_string(), "drive Z: is already in use");
    }

    #[test]
    fn connect_errors_pass_through_mount_errors() {
        let error = MountError::from(ConnectError::SftpUnavailable);
        assert_eq!(error, MountError::Connect(ConnectError::SftpUnavailable));
        assert_eq!(error.to_string(), "the server does not provide SFTP");
    }

    #[test]
    fn errors_stay_small() {
        // The host key details are boxed so the Result of every connect call stays small.
        assert!(size_of::<ConnectError>() <= 64);
        assert!(size_of::<MountError>() <= 64);
    }
}
