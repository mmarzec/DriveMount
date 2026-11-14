// SPDX-License-Identifier: GPL-3.0-or-later

//! From what the WNet calls answer to the codes of spec §7.8 (spec §5.9, spike M3-01 §3).

use drivemount_vfs::{
    ConnectError, DriveLetter, MountError, OtherSmbConnection, SmbCredentialConflict,
};
use drivemount_win::wnet::{NetUse, WNetError};

use crate::other::{account, parse_letter, server_of};

/// The port E-HOST-UNREACHABLE shows: SMB's, which `NETRESOURCEW` cannot change.
pub const SMB_PORT: u16 = 445;

pub const ERROR_BAD_NETPATH: u32 = 53;
pub const ERROR_BAD_NET_NAME: u32 = 67;
pub const ERROR_ALREADY_ASSIGNED: u32 = 85;
pub const ERROR_INVALID_PASSWORD: u32 = 86;
pub const ERROR_DEVICE_ALREADY_REMEMBERED: u32 = 1202;
pub const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;
pub const ERROR_LOGON_FAILURE: u32 = 1326;
pub const ERROR_NOT_CONNECTED: u32 = 2250;
pub const ERROR_OPEN_FILES: u32 = 2401;

/// What the mount is of, for the error texts.
pub struct Mapping<'a> {
    pub letter: DriveLetter,
    pub server: &'a str,
    pub share: &'a str,
    /// The user name as the error shows it.
    pub user: &'a str,
}

/// The code for a failed `WNetAddConnection2W`. 1219 is [`credential_conflict`]'s.
pub fn mount_error(error: &WNetError, mapping: &Mapping<'_>) -> MountError {
    match error.code {
        ERROR_LOGON_FAILURE | ERROR_INVALID_PASSWORD => ConnectError::Auth {
            user: mapping.user.to_owned(),
            host: mapping.server.to_owned(),
        }
        .into(),
        ERROR_BAD_NET_NAME => ConnectError::ShareNotFound {
            share: mapping.share.to_owned(),
            server: mapping.server.to_owned(),
        }
        .into(),
        // An unknown name too: SMB has no E-DNS (spike M3-01 §3).
        ERROR_BAD_NETPATH => ConnectError::HostUnreachable {
            host: mapping.server.to_owned(),
            port: SMB_PORT,
        }
        .into(),
        ERROR_ALREADY_ASSIGNED | ERROR_DEVICE_ALREADY_REMEMBERED => {
            MountError::LetterTaken(mapping.letter)
        }
        ERROR_SESSION_CREDENTIAL_CONFLICT => credential_conflict(mapping.server, &[]),
        _ => MountError::Other(format!("WNetAddConnection2W: {error}")),
    }
}

/// E-SMB-1219, naming the connection to `server` that holds other credentials if `uses` has one: a
/// lettered one first, as the hint can name its letter (spec §5.9).
pub fn credential_conflict(server: &str, uses: &[NetUse]) -> MountError {
    let to_server = |used: &&NetUse| {
        server_of(&used.remote).is_some_and(|other| other.eq_ignore_ascii_case(server))
    };
    let lettered = uses
        .iter()
        .filter(to_server)
        .find(|used| used.local.is_some());
    let other = lettered
        .or_else(|| uses.iter().find(to_server))
        .map(|used| OtherSmbConnection {
            letter: used.local.as_deref().and_then(parse_letter),
            user: account(used),
        });
    ConnectError::SmbCredentialConflict(Box::new(SmbCredentialConflict {
        server: server.to_owned(),
        other,
    }))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter(c: char) -> DriveLetter {
        DriveLetter::new(c).unwrap()
    }

    fn mapping() -> Mapping<'static> {
        Mapping {
            letter: letter('S'),
            server: "nas",
            share: "media",
            user: "marcin",
        }
    }

    fn failed(code: u32) -> MountError {
        let error = WNetError {
            code,
            text: "text".to_owned(),
        };
        mount_error(&error, &mapping())
    }

    fn net_use(local: Option<&str>, remote: &str, user: &str, domain: &str) -> NetUse {
        NetUse {
            local: local.map(str::to_owned),
            remote: remote.to_owned(),
            status: 0,
            user: user.to_owned(),
            domain: domain.to_owned(),
        }
    }

    #[test]
    fn the_spike_codes_map_as_the_spec_says() {
        let auth = MountError::Connect(ConnectError::Auth {
            user: "marcin".to_owned(),
            host: "nas".to_owned(),
        });
        assert_eq!(failed(1326), auth);
        assert_eq!(failed(86), auth);
        assert_eq!(
            failed(67),
            MountError::Connect(ConnectError::ShareNotFound {
                share: "media".to_owned(),
                server: "nas".to_owned(),
            })
        );
        assert_eq!(
            failed(53),
            MountError::Connect(ConnectError::HostUnreachable {
                host: "nas".to_owned(),
                port: 445,
            })
        );
        assert_eq!(failed(85), MountError::LetterTaken(letter('S')));
        assert_eq!(failed(1202), MountError::LetterTaken(letter('S')));
    }

    #[test]
    fn anything_else_is_internal_with_the_number() {
        for code in [1203, 1208, 5, 0xFFFF] {
            let MountError::Other(detail) = failed(code) else {
                panic!("{code} is not internal");
            };
            assert!(detail.contains(&code.to_string()), "{detail}");
            assert!(detail.contains("text"), "{detail}");
        }
    }

    #[test]
    fn a_conflict_names_the_lettered_connection() {
        let uses = [
            net_use(None, r"\\NAS\IPC$", "admin", ""),
            net_use(Some("W:"), r"\\other\photos", "marcin", ""),
            net_use(Some("X:"), r"\\Nas\music", "admin", "NAS"),
        ];
        let MountError::Connect(ConnectError::SmbCredentialConflict(conflict)) =
            credential_conflict("nas", &uses)
        else {
            panic!("not E-SMB-1219");
        };
        assert_eq!(conflict.server, "nas");
        assert_eq!(
            conflict.other,
            Some(OtherSmbConnection {
                letter: Some(letter('X')),
                user: r"NAS\admin".to_owned(),
            })
        );
    }

    #[test]
    fn a_conflict_without_a_letter_says_so() {
        let uses = [net_use(None, r"\\nas\scans", "admin", "")];
        let MountError::Connect(ConnectError::SmbCredentialConflict(conflict)) =
            credential_conflict("nas", &uses)
        else {
            panic!("not E-SMB-1219");
        };
        let other = conflict.other.unwrap();
        assert_eq!(other.letter, None);
        assert_eq!(other.user, "admin");
    }

    #[test]
    fn a_conflict_not_found_names_nobody() {
        let uses = [net_use(Some("W:"), r"\\nas2\photos", "admin", "")];
        for listed in [&uses[..], &[]] {
            let MountError::Connect(ConnectError::SmbCredentialConflict(conflict)) =
                credential_conflict("nas", listed)
            else {
                panic!("not E-SMB-1219");
            };
            assert_eq!(conflict.other, None);
        }
    }
}
