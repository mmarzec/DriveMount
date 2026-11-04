// SPDX-License-Identifier: GPL-3.0-or-later

//! What the mount manager drives.

use std::fmt;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::{MountError, Prompter};

/// A Windows drive letter, always stored in upper case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DriveLetter(char);

impl DriveLetter {
    /// Accepts `A`–`Z` in either case. Any other character gives `None`.
    #[must_use]
    pub const fn new(letter: char) -> Option<Self> {
        if letter.is_ascii_alphabetic() {
            Some(Self(letter.to_ascii_uppercase()))
        } else {
            None
        }
    }

    /// The upper-case letter.
    #[must_use]
    pub const fn as_char(self) -> char {
        self.0
    }
}

/// Shows the letter with its colon, as in `Z:`.
impl fmt::Display for DriveLetter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.0)
    }
}

/// The state of a mount's connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// The connection is working.
    Ok,
    /// The connection is gone, with the reason for the log.
    Lost(String),
}

/// A mount the manager can start and stop. There are two kinds: a WinFsp drive hosting a
/// [`RemoteFs`](crate::RemoteFs) (SFTP, NFS), and a native Windows mapping (SMB, M3).
#[async_trait]
pub trait MountBackend: Send + Sync + 'static {
    /// Connects and makes the drive appear on `letter`.
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError>;

    /// Flushes and removes the drive.
    async fn unmount(&self) -> Result<(), MountError>;

    /// Follows the connection's health.
    fn health(&self) -> watch::Receiver<Health>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_upper_case() {
        assert_eq!(DriveLetter::new('z').unwrap().as_char(), 'Z');
        assert_eq!(DriveLetter::new('A').unwrap().as_char(), 'A');
        assert_eq!(DriveLetter::new('q').unwrap().to_string(), "Q:");
        assert_eq!(DriveLetter::new('q'), DriveLetter::new('Q'));
    }

    #[test]
    fn other_characters_are_not_letters() {
        for c in ['1', ':', ' ', 'ą', 'Ż', '\0'] {
            assert_eq!(DriveLetter::new(c), None, "{c:?}");
        }
    }
}
