// SPDX-License-Identifier: GPL-3.0-or-later

//! Which drive letters are free (FR-MNT-03).

use std::io;

use drivemount_vfs::DriveLetter;
use windows::Win32::Foundation::{ERROR_NOT_CONNECTED, MAX_PATH, WIN32_ERROR};
use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::core::{PCWSTR, PWSTR};

use crate::wide;

/// Whether a letter is free: not in `GetLogicalDrives()` and not a network connection, even a
/// remembered one that is not connected now, which `GetLogicalDrives()` may not list (spike M0-1).
///
/// This is the definition alone. Never using A, B and C is the allocator's rule.
///
/// # Errors
/// The error from `GetLogicalDrives`.
pub fn is_letter_free(letter: DriveLetter) -> io::Result<bool> {
    // SAFETY: takes no arguments and has no preconditions.
    let present = unsafe { GetLogicalDrives() };
    if present == 0 {
        // There is always a system drive, so this can only be the call failing.
        return Err(io::Error::last_os_error());
    }
    Ok(!in_mask(present, letter) && !is_network_taken(connection_status(letter)))
}

/// Whether `GetLogicalDrives`' mask has this letter: bit 0 is A.
const fn in_mask(mask: u32, letter: DriveLetter) -> bool {
    (mask >> (letter.as_char() as u32 - 'A' as u32)) & 1 == 1
}

/// FR-MNT-03: anything but `ERROR_NOT_CONNECTED` counts as taken. That includes a mapping that is
/// remembered but down (`ERROR_CONNECTION_UNAVAIL`), one whose name did not fit
/// (`ERROR_MORE_DATA`), and a network layer that cannot say (`ERROR_NO_NETWORK`).
fn is_network_taken(status: WIN32_ERROR) -> bool {
    status != ERROR_NOT_CONNECTED
}

fn connection_status(letter: DriveLetter) -> WIN32_ERROR {
    let device = wide(letter.to_string());
    let mut remote = [0u16; MAX_PATH as usize];
    let mut length = MAX_PATH;
    // SAFETY: `device` is nul-terminated, and `length` is the size of `remote` in characters. All of
    // them outlive the call.
    unsafe {
        WNetGetConnectionW(
            PCWSTR(device.as_ptr()),
            Some(PWSTR(remote.as_mut_ptr())),
            &raw mut length,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use windows::Win32::Foundation::{
        ERROR_BAD_DEVICE, ERROR_CONNECTION_UNAVAIL, ERROR_MORE_DATA, ERROR_NO_NETWORK, NO_ERROR,
    };

    use super::*;
    use crate::test_support::{SubstDrive, free_letter, letter, lock_letters};

    #[test]
    fn the_mask_counts_from_a() {
        let mask = 0b101 | (1 << 25);
        assert!(in_mask(mask, letter('A')));
        assert!(!in_mask(mask, letter('B')));
        assert!(in_mask(mask, letter('C')));
        assert!(!in_mask(mask, letter('D')));
        assert!(!in_mask(mask, letter('Y')));
        assert!(in_mask(mask, letter('Z')));
    }

    #[test]
    fn only_not_connected_leaves_a_letter_free() {
        assert!(!is_network_taken(ERROR_NOT_CONNECTED));
        for status in [
            NO_ERROR,
            ERROR_CONNECTION_UNAVAIL,
            ERROR_MORE_DATA,
            ERROR_NO_NETWORK,
            ERROR_BAD_DEVICE,
        ] {
            assert!(is_network_taken(status), "{status:?}");
        }
    }

    #[test]
    fn the_system_drive_is_taken() {
        let drive = std::env::var("SystemDrive").unwrap();
        let system = DriveLetter::new(drive.chars().next().unwrap()).unwrap();
        assert!(!is_letter_free(system).unwrap());
    }

    #[test]
    fn a_subst_drive_is_taken_until_it_goes() {
        let _letters = lock_letters();
        let target = tempfile::tempdir().unwrap();
        let drive = SubstDrive::new(target.path());
        assert!(!is_letter_free(drive.letter()).unwrap());

        let freed = drive.letter();
        drop(drive);
        assert!(is_letter_free(freed).unwrap());
        assert_eq!(connection_status(freed), ERROR_NOT_CONNECTED);
    }

    /// The acceptance test of FR-MNT-03: a mapping remembered in `HKCU\Network`, down, on a letter
    /// `GetLogicalDrives` does not list. **Writes `HKCU\Network\<letter>` for a moment**; if killed
    /// here, `net use <letter>: /delete` removes it.
    #[test]
    #[ignore = "writes this user's remembered network drives for a moment"]
    fn a_remembered_mapping_that_is_down_is_taken() {
        struct Remembered(String);

        impl Drop for Remembered {
            fn drop(&mut self) {
                let _ = reg(&["delete", &self.0, "/f"]);
            }
        }

        fn reg(arguments: &[&str]) -> bool {
            Command::new("reg")
                .args(arguments)
                .output()
                .unwrap()
                .status
                .success()
        }

        let _letters = lock_letters();
        let free = free_letter();
        let key = format!(r"HKCU\Network\{}", free.as_char());
        assert!(!reg(&["query", &key]), "{key} is someone's mapping");

        let remembered = Remembered(key);
        for (name, kind, data) in [
            ("RemotePath", "REG_SZ", r"\\drivemount-test.invalid\share"),
            ("UserName", "REG_SZ", ""),
            ("ProviderName", "REG_SZ", "Microsoft Windows Network"),
            ("ProviderType", "REG_DWORD", "131072"),
            ("ConnectionType", "REG_DWORD", "1"),
            ("DeferFlags", "REG_DWORD", "4"),
        ] {
            let key = &remembered.0;
            assert!(reg(&["add", key, "/v", name, "/t", kind, "/d", data, "/f"]));
        }

        // SAFETY: as in `is_letter_free`.
        let present = unsafe { GetLogicalDrives() };
        assert!(
            !in_mask(present, free),
            "the test needs the letter unlisted"
        );
        assert_eq!(connection_status(free), ERROR_CONNECTION_UNAVAIL);
        assert!(!is_letter_free(free).unwrap());

        drop(remembered);
        assert!(is_letter_free(free).unwrap());
    }
}
