// SPDX-License-Identifier: GPL-3.0-or-later

//! What the mount manager drives.

use std::fmt;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::{MountError, Prompter, Secret, SecretSlot};

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

    /// Flushes and removes the drive. A backend may refuse while files are open on it, with
    /// [`MountError::FilesOpen`] and the drive left as it was, for the user to be asked first
    /// (FR-SMB-04); [`unmount_forced`](Self::unmount_forced) is what comes after a yes.
    async fn unmount(&self) -> Result<(), MountError>;

    /// Removes the drive even with files open on it, which lose their handles: after the user said
    /// yes to FR-SMB-04's question, and wherever there is nobody to ask (Quit, sign-out, deleting
    /// the profile). Never [`MountError::FilesOpen`]. The default is [`unmount`](Self::unmount), for
    /// a backend that never refuses.
    async fn unmount_forced(&self) -> Result<(), MountError> {
        self.unmount().await
    }

    /// Connects again once [`health`](Self::health) has said the connection is lost, keeping the
    /// drive on its letter (FR-REC-02). Files open on the drive carry on over the new connection
    /// (FR-REC-05), and `health` is `Ok` again. On failure the drive stays as it was, for another
    /// try or an unmount. Dropping the future gives up this try.
    async fn reconnect(&self, prompter: &dyn Prompter) -> Result<(), MountError>;

    /// Follows the connection's health. It goes to `Lost` once, and stays there until a
    /// [`reconnect`](Self::reconnect) works.
    fn health(&self) -> watch::Receiver<Health>;

    /// Asks the connection whether it still answers, as after Windows resumed or the network changed
    /// (FR-REC-03). The answer comes as [`health`](Self::health): `Lost` if it did not. This returns
    /// without waiting for it. The default does nothing, for a backend whose connection Windows looks
    /// after (SMB).
    async fn check_connection(&self) {}

    /// Whether the system reconnects this drive by itself, as Windows' SMB client does (FR-SMB-05).
    /// Then the manager never unmounts it for a lost connection: it waits in *Reconnecting*, whatever
    /// the profile's auto-reconnect flag says, and [`reconnect`](Self::reconnect) only checks whether
    /// the connection is back. The default is `false`.
    fn reconnects_itself(&self) -> bool {
        false
    }

    /// Takes what the user asked to remember during [`mount`](Self::mount), for the manager to store
    /// (NFR-SEC-01). Each secret is handed over once. A backend that logs in some other way has none.
    fn take_remembered(&self) -> Vec<(SecretSlot, Secret<String>)> {
        Vec::new()
    }
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
