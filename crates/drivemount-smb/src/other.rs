// SPDX-License-Identifier: GPL-3.0-or-later

//! Network drives DriveMount did not make (FR-SMB-09, spec §5.9, spike M3-01 §11): read from what
//! Windows already knows, never from the drive or its server.

use drivemount_vfs::DriveLetter;
use drivemount_win::wnet::{self, NetConnection, NetUse, WNetError};
use tracing::warn;

/// The UNC prefix of DriveMount's own WinFsp drives (spec §5.8).
const OWN_PREFIX: &str = r"\\drivemount\";

/// A drive's state as Windows last saw it. It lags (spike M3-01 §11), and the app never checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveStatus {
    /// Connected, as far as Windows knows. Every non-SMB drive is shown so.
    Mapped,
    /// Windows is connecting to its server again (`ui2_status` 4 or 5).
    Reconnecting,
    /// Windows has lost its server (`ui2_status` 2 or 3).
    Disconnected,
    /// Remembered but not connected: Windows connects it when it is next opened.
    NotConnected,
}

/// A network drive mapped outside DriveMount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherDrive {
    /// Its letter.
    pub letter: DriveLetter,
    /// What it is mapped to, as `\\nas\photos`.
    pub unc: String,
    /// The network provider, as "Microsoft Windows Network", `P9NP` or `WinFsp.Np`.
    pub provider: String,
    /// Its state, as Windows reports it.
    pub status: DriveStatus,
    /// For an SMB drive, the user it was made as: `DOMAIN\user` when Windows gives a domain.
    pub user: Option<String>,
}

/// Every lettered network drive of this logon session but DriveMount's own: those on `held`, the
/// letters the mount manager holds (spec §5.5), and every `\\drivemount\…` drive. Sorted by letter.
/// Takes 2 ms or less and reaches no server, also during an outage, so it may run on the UI thread.
///
/// # Errors
/// Windows could not list the connected drives. A failure of the other two listings only loses
/// what they add, and is logged.
pub fn other_drives(held: &[DriveLetter]) -> Result<Vec<OtherDrive>, WNetError> {
    let connected = wnet::connected()?;
    let remembered = wnet::remembered().unwrap_or_else(|error| {
        warn!(%error, "could not list the remembered network drives");
        Vec::new()
    });
    let uses = wnet::net_uses().unwrap_or_else(|error| {
        warn!(%error, "could not list the SMB connections");
        Vec::new()
    });
    Ok(merge(&connected, &remembered, &uses, held))
}

/// The listings merged by letter: connected first, then remembered ones that are not connected,
/// with each SMB letter's status and user from `uses`.
pub(crate) fn merge(
    connected: &[NetConnection],
    remembered: &[NetConnection],
    uses: &[NetUse],
    held: &[DriveLetter],
) -> Vec<OtherDrive> {
    let mut drives: Vec<OtherDrive> = Vec::new();
    let listed = connected
        .iter()
        .map(|connection| (connection, true))
        .chain(remembered.iter().map(|connection| (connection, false)));
    for (connection, is_connected) in listed {
        let Some(letter) = connection.local.as_deref().and_then(parse_letter) else {
            continue;
        };
        if held.contains(&letter)
            || is_own(&connection.remote)
            || drives.iter().any(|drive| drive.letter == letter)
        {
            continue;
        }
        let used = uses
            .iter()
            .find(|used| used.local.as_deref().and_then(parse_letter) == Some(letter));
        let status = if is_connected {
            used.map_or(DriveStatus::Mapped, |used| status(used.status))
        } else {
            DriveStatus::NotConnected
        };
        drives.push(OtherDrive {
            letter,
            unc: connection.remote.clone(),
            provider: connection.provider.clone(),
            status,
            user: used.map(account),
        });
    }
    drives.sort_by_key(|drive| drive.letter);
    drives
}

/// `NetUseEnum`'s `ui2_status`. 1 (paused) is from LAN Manager days and shows as mapped.
const fn status(code: u32) -> DriveStatus {
    match code {
        2 | 3 => DriveStatus::Disconnected,
        4 | 5 => DriveStatus::Reconnecting,
        _ => DriveStatus::Mapped,
    }
}

/// `W:` as Windows writes it, in either case. Anything else, such as `LPT1`, is no drive.
pub(crate) fn parse_letter(local: &str) -> Option<DriveLetter> {
    let mut chars = local.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) => DriveLetter::new(letter),
        _ => None,
    }
}

fn is_own(unc: &str) -> bool {
    unc.get(..OWN_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(OWN_PREFIX))
}

/// `DOMAIN\user`, or the user alone when there is no domain.
pub(crate) fn account(used: &NetUse) -> String {
    if used.domain.is_empty() {
        used.user.clone()
    } else {
        format!(r"{}\{}", used.domain, used.user)
    }
}

/// The server part of `\\server\share\…`, or `None` for anything else.
pub(crate) fn server_of(unc: &str) -> Option<&str> {
    let rest = unc.strip_prefix(r"\\")?;
    let server = rest.split('\\').next()?;
    (!server.is_empty()).then_some(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMB: &str = "Microsoft Windows Network";

    fn letter(c: char) -> DriveLetter {
        DriveLetter::new(c).unwrap()
    }

    fn connection(local: Option<&str>, remote: &str, provider: &str) -> NetConnection {
        NetConnection {
            local: local.map(str::to_owned),
            remote: remote.to_owned(),
            provider: provider.to_owned(),
        }
    }

    fn net_use(local: Option<&str>, remote: &str, status: u32, user: &str, domain: &str) -> NetUse {
        NetUse {
            local: local.map(str::to_owned),
            remote: remote.to_owned(),
            status,
            user: user.to_owned(),
            domain: domain.to_owned(),
        }
    }

    /// The owner's machine during spike M3-01 §11: `W:` and `X:` connected, `Z:` persistent with its
    /// server off, `Y:` WSL's Plan 9, `S:` our own SMB drive, `V:` our own WinFsp drive, and `T:`
    /// remembered but not connected. Letters as Windows writes them, which may be lower case.
    fn spike_machine() -> (Vec<NetConnection>, Vec<NetConnection>, Vec<NetUse>) {
        let connected = vec![
            connection(Some("W:"), r"\\nas\photos", SMB),
            connection(Some("x:"), r"\\nas\music", SMB),
            connection(Some("Y:"), r"\\wsl$\Ubuntu", "Plan 9 Network Provider"),
            connection(Some("Z:"), r"\\oldnas\backup", SMB),
            connection(Some("S:"), r"\\172.23.105.89\a", SMB),
            connection(Some("V:"), r"\\drivemount\nas-backup", "WinFsp.Np"),
            connection(None, r"\\nas\scans", SMB),
        ];
        let remembered = vec![
            connection(Some("Z:"), r"\\oldnas\backup", SMB),
            connection(Some("T:"), r"\\nas\archive", SMB),
        ];
        let uses = vec![
            net_use(Some("W:"), r"\\nas\photos", 0, "marcin", ""),
            net_use(Some("X:"), r"\\nas\music", 5, "marcin", ""),
            net_use(Some("Z:"), r"\\oldnas\backup", 2, "admin", "OLDNAS"),
            net_use(Some("S:"), r"\\172.23.105.89\a", 0, "smbuser1", ""),
            net_use(None, r"\\nas\IPC$", 0, "marcin", ""),
        ];
        (connected, remembered, uses)
    }

    #[test]
    fn the_listings_merge_by_letter() {
        let (connected, remembered, uses) = spike_machine();
        let drives = merge(&connected, &remembered, &uses, &[letter('S')]);
        let summary: Vec<_> = drives
            .iter()
            .map(|drive| (drive.letter.as_char(), drive.status, drive.user.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                ('T', DriveStatus::NotConnected, None),
                ('W', DriveStatus::Mapped, Some("marcin")),
                ('X', DriveStatus::Reconnecting, Some("marcin")),
                ('Y', DriveStatus::Mapped, None),
                ('Z', DriveStatus::Disconnected, Some(r"OLDNAS\admin")),
            ]
        );
        let wsl = &drives[3];
        assert_eq!(wsl.unc, r"\\wsl$\Ubuntu");
        assert_eq!(wsl.provider, "Plan 9 Network Provider");
    }

    #[test]
    fn the_apps_own_drives_are_left_out() {
        let (connected, remembered, uses) = spike_machine();
        let drives = merge(&connected, &remembered, &uses, &[letter('S'), letter('W')]);
        let letters: Vec<_> = drives.iter().map(|drive| drive.letter.as_char()).collect();
        assert_eq!(letters, ['T', 'X', 'Y', 'Z']);

        // A WinFsp drive of ours mid-mount, before the manager holds its letter, in any case.
        let ours = [connection(Some("Q:"), r"\\DriveMount\Box", "WinFsp.Np")];
        assert!(merge(&ours, &[], &[], &[]).is_empty());
    }

    #[test]
    fn another_winfsp_programs_drive_is_shown() {
        let theirs = [connection(Some("R:"), r"\\sshfs\me@host", "WinFsp.Np")];
        let drives = merge(&theirs, &[], &[], &[]);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].status, DriveStatus::Mapped);
    }

    #[test]
    fn nothing_listed_is_nothing_shown() {
        assert!(merge(&[], &[], &[], &[]).is_empty());
    }

    #[test]
    fn every_status_code_has_a_state() {
        assert_eq!(status(0), DriveStatus::Mapped);
        assert_eq!(status(1), DriveStatus::Mapped);
        assert_eq!(status(2), DriveStatus::Disconnected);
        assert_eq!(status(3), DriveStatus::Disconnected);
        assert_eq!(status(4), DriveStatus::Reconnecting);
        assert_eq!(status(5), DriveStatus::Reconnecting);
        assert_eq!(status(99), DriveStatus::Mapped);
    }

    #[test]
    fn only_a_letter_and_colon_is_a_drive() {
        assert_eq!(parse_letter("W:"), Some(letter('W')));
        assert_eq!(parse_letter("w:"), Some(letter('W')));
        for other in ["", "W", "W:\\", "LPT1:", "1:", "WW:"] {
            assert_eq!(parse_letter(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_server_is_the_first_part() {
        assert_eq!(server_of(r"\\nas\photos"), Some("nas"));
        assert_eq!(server_of(r"\\10.0.0.5\IPC$"), Some("10.0.0.5"));
        assert_eq!(server_of(r"\\nas"), Some("nas"));
        assert_eq!(server_of(r"\\\share"), None);
        assert_eq!(server_of("nas"), None);
    }

    /// Against what this machine has: run with `net use` mappings made by hand to see them listed.
    #[test]
    fn this_machine_lists() {
        let drives = other_drives(&[]).unwrap();
        for drive in &drives {
            assert!(!is_own(&drive.unc), "{drive:?}");
        }
    }
}
