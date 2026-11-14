// SPDX-License-Identifier: GPL-3.0-or-later

//! The one place where errors become the codes and texts of spec §7.8 (plan §11). The texts are in
//! `ui/texts.slint`, as `@tr` is only there. M2-04's toasts should take theirs from here.

use drivemount_core::config::{ReadOnlyReason, SettingProblem};
use drivemount_vfs::{ConnectError, DriveLetter, MountError, SmbCredentialConflict};
use slint::SharedString;

use super::Texts;

/// An error as the user sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownError {
    /// The code of spec §7.8, for the log. It is not shown (NFR-UX-03).
    pub code: &'static str,
    /// What happened.
    pub text: SharedString,
    /// What to do, which may be nothing.
    pub hint: SharedString,
}

impl ShownError {
    fn new(code: &'static str, text: SharedString, hint: Option<SharedString>) -> Self {
        Self {
            code,
            text,
            hint: hint.unwrap_or_default(),
        }
    }

    /// One line for a row: what happened, then what to do.
    #[must_use]
    pub fn line(&self) -> SharedString {
        if self.hint.is_empty() {
            self.text.clone()
        } else {
            format!("{} {}", self.text, self.hint).into()
        }
    }
}

/// What spec §7.8 has for this error.
#[must_use]
pub fn shown_error(texts: &Texts<'_>, error: &MountError) -> ShownError {
    shown_error_among(texts, error, &[])
}

/// As [`shown_error`], knowing the letters DriveMount's own drives are on: E-SMB-1219 tells the user
/// to disconnect a letter in Explorer only when it is not one of them (spec §7.8).
#[must_use]
pub fn shown_error_among(texts: &Texts<'_>, error: &MountError, own: &[DriveLetter]) -> ShownError {
    match error {
        MountError::Connect(error) => shown_connect_error_among(texts, error, own),
        MountError::LetterTaken(letter) => ShownError::new(
            "E-LETTER-TAKEN",
            texts.invoke_letter_taken(letter.to_string().into()),
            Some(texts.invoke_letter_taken_hint()),
        ),
        MountError::NoFreeLetter => {
            ShownError::new("E-NO-FREE-LETTER", texts.invoke_no_free_letter(), None)
        }
        // The banner has the Download button.
        MountError::WinFspMissing => {
            ShownError::new("E-WINFSP-MISSING", texts.invoke_winfsp_missing(), None)
        }
        // The manager keeps the drive mounted on this; it is no row's error (FR-SMB-04).
        MountError::FilesOpen(_) | MountError::Other(_) => internal(texts),
    }
}

fn internal(texts: &Texts<'_>) -> ShownError {
    ShownError::new(
        "E-INTERNAL",
        texts.invoke_internal(),
        Some(texts.invoke_internal_hint()),
    )
}

/// What spec §7.8 has for this error, as Test connection reports it too.
#[must_use]
pub fn shown_connect_error(texts: &Texts<'_>, error: &ConnectError) -> ShownError {
    shown_connect_error_among(texts, error, &[])
}

/// As [`shown_connect_error`], knowing the letters of DriveMount's own drives.
#[must_use]
pub fn shown_connect_error_among(
    texts: &Texts<'_>,
    error: &ConnectError,
    own: &[DriveLetter],
) -> ShownError {
    match error {
        ConnectError::Auth { user, host } => ShownError::new(
            "E-AUTH",
            texts.invoke_auth(user.into(), host.into()),
            Some(texts.invoke_auth_hint()),
        ),
        // The dialog of spec 7.4.3 has what to do.
        ConnectError::HostKeyChanged(_) => {
            ShownError::new("E-HOSTKEY-CHANGED", texts.invoke_host_key_changed(), None)
        }
        ConnectError::HostKeyRevoked(details) => ShownError::new(
            "E-HOSTKEY-REVOKED",
            texts.invoke_host_key_revoked(),
            Some(
                texts.invoke_host_key_revoked_hint(i32::try_from(details.line).unwrap_or(i32::MAX)),
            ),
        ),
        ConnectError::HostKeyRejected => ShownError::new(
            "E-HOSTKEY-REJECTED",
            texts.invoke_host_key_rejected(),
            Some(texts.invoke_host_key_rejected_hint()),
        ),
        ConnectError::Dns { host } => ShownError::new(
            "E-DNS",
            texts.invoke_dns(host.into()),
            Some(texts.invoke_dns_hint()),
        ),
        ConnectError::HostUnreachable { host, port } => ShownError::new(
            "E-HOST-UNREACHABLE",
            texts.invoke_unreachable(host.into(), i32::from(*port)),
            Some(texts.invoke_unreachable_hint()),
        ),
        ConnectError::Timeout { host } => {
            ShownError::new("E-TIMEOUT", texts.invoke_timeout(host.into()), None)
        }
        ConnectError::PathNotFound { path } => ShownError::new(
            "E-PATH-NOT-FOUND",
            texts.invoke_path_not_found(path.into()),
            Some(texts.invoke_path_not_found_hint()),
        ),
        ConnectError::PathNotDir { path } => ShownError::new(
            "E-PATH-NOT-DIR",
            texts.invoke_path_not_dir(path.into()),
            None,
        ),
        ConnectError::SftpUnavailable => ShownError::new(
            "E-SFTP-UNAVAILABLE",
            texts.invoke_sftp_unavailable(),
            Some(texts.invoke_sftp_unavailable_hint()),
        ),
        ConnectError::NoCommonAlgorithm => ShownError::new(
            "E-ALGORITHMS",
            texts.invoke_algorithms(),
            Some(texts.invoke_algorithms_hint()),
        ),
        ConnectError::KeyFormat => ShownError::new(
            "E-KEY-FORMAT",
            texts.invoke_key_format(),
            Some(texts.invoke_key_format_hint()),
        ),
        ConnectError::ConnectionLost => {
            ShownError::new("E-CONN-LOST", texts.invoke_connection_lost(), None)
        }
        ConnectError::SmbCredentialConflict(conflict) => smb_conflict(texts, conflict, own),
        ConnectError::ShareNotFound { share, server } => ShownError::new(
            "E-SMB-SHARE-NOT-FOUND",
            texts.invoke_share_not_found(share.into(), server.into()),
            None,
        ),
        // Cancelling goes to Disconnected, and has no row of its own in the catalogue.
        ConnectError::Cancelled | ConnectError::Other(_) => internal(texts),
    }
}

/// E-SMB-1219, naming the other connection when the app found it (FR-SMB-06, spec §5.9).
fn smb_conflict(
    texts: &Texts<'_>,
    conflict: &SmbCredentialConflict,
    own: &[DriveLetter],
) -> ShownError {
    let server = SharedString::from(conflict.server.as_str());
    let text = match &conflict.other {
        None => texts.invoke_smb_conflict(server),
        Some(other) => match other.letter {
            Some(letter) => texts.invoke_smb_conflict_on(
                server,
                other.user.as_str().into(),
                letter.to_string().into(),
            ),
            None => texts.invoke_smb_conflict_no_letter(server, other.user.as_str().into()),
        },
    };
    let elsewhere = conflict
        .other
        .as_ref()
        .and_then(|other| other.letter)
        .filter(|letter| !own.contains(letter));
    let hint = match elsewhere {
        Some(letter) => texts.invoke_smb_conflict_letter_hint(letter.to_string().into()),
        None => texts.invoke_smb_conflict_hint(),
    };
    ShownError::new("E-SMB-1219", text, Some(hint))
}

/// The banner for a file that cannot be written (NFR-REL-05). Of a damaged file's detail only the
/// first line is used, as `toml` goes on to quote the file.
#[must_use]
pub fn read_only_text(texts: &Texts<'_>, reason: &ReadOnlyReason) -> SharedString {
    let first_line = |detail: &str| SharedString::from(detail.lines().next().unwrap_or_default());
    match reason {
        ReadOnlyReason::Damaged { detail } => texts.invoke_read_only_damaged(first_line(detail)),
        ReadOnlyReason::CannotRead { detail } => {
            texts.invoke_read_only_cannot_read(first_line(detail))
        }
        ReadOnlyReason::Newer { .. } => texts.invoke_read_only_newer(),
        ReadOnlyReason::MigrationFailed { from, detail } => texts.invoke_read_only_migration(
            i32::try_from(*from).unwrap_or(i32::MAX),
            first_line(detail),
        ),
    }
}

/// The banner for settings that were not used. Empty if there are none.
#[must_use]
pub fn setting_problems_text(texts: &Texts<'_>, problems: &[SettingProblem]) -> SharedString {
    if problems.is_empty() {
        return SharedString::new();
    }
    let keys: Vec<&str> = problems.iter().map(|problem| problem.key).collect();
    texts.invoke_setting_problems(keys.join(", ").into())
}
