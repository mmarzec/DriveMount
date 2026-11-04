// SPDX-License-Identifier: GPL-3.0-or-later

//! Telling the shell about drives, and opening them in Explorer.

use std::io;
use std::path::PathBuf;

use fsm_vfs::DriveLetter;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::UI::Shell::{
    SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNE_ID, SHCNF_PATHW, SHChangeNotify, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use crate::wide;

/// Tells the shell that a drive has appeared. Returns at once, whoever is listening.
///
/// Spike M0-1: this alone may not make a running Explorer show the drive. M1-12c settles what does
/// (FR-MNT-06).
pub fn notify_drive_added(letter: DriveLetter) {
    notify(SHCNE_DRIVEADD, letter);
}

/// Tells the shell that a drive has gone. Returns at once, whoever is listening.
pub fn notify_drive_removed(letter: DriveLetter) {
    notify(SHCNE_DRIVEREMOVED, letter);
}

/// `SHCNF_FLUSH` must stay out of the flags: it waits for every listener, and a hung one would hang
/// us (FR-MNT-06).
fn notify(event: SHCNE_ID, letter: DriveLetter) {
    let root = wide(root_of(letter));
    // SAFETY: with `SHCNF_PATHW`, the first item is a nul-terminated path, read during the call. The
    // event takes no second item.
    unsafe { SHChangeNotify(event, SHCNF_PATHW, Some(root.as_ptr().cast()), None) };
}

/// Opens the drive's root in a new Explorer window (FR-MNT-12).
///
/// # Errors
/// If Explorer cannot be started. Explorer's own complaints, such as a drive that is not there,
/// appear in its window, not here.
pub fn open_in_explorer(letter: DriveLetter) -> io::Result<()> {
    let explorer = wide(explorer_path()?);
    let root = wide(root_of(letter));
    // SAFETY: all strings are nul-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(explorer.as_ptr()),
            PCWSTR(root.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success. The rest are SE_ERR_ codes, not all of them Win32 errors.
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW could not start Explorer: code {code}"
        )))
    }
}

/// The drive's root, as in `Z:\`.
fn root_of(letter: DriveLetter) -> String {
    format!("{letter}\\")
}

/// Explorer by its full path, as `ShellExecuteW` looks in the current directory first.
fn explorer_path() -> io::Result<PathBuf> {
    // SAFETY: with no buffer, this only returns the size needed.
    let size = unsafe { GetSystemWindowsDirectoryW(None) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: the binding passes the slice's pointer and length.
    let written = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) };
    if written == 0 || written >= size {
        return Err(io::Error::last_os_error());
    }
    let directory = String::from_utf16(&buffer[..written as usize]).map_err(io::Error::other)?;
    Ok(PathBuf::from(directory).join("explorer.exe"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_support::{SubstDrive, letter, lock_letters};

    #[test]
    fn a_root_has_its_backslash() {
        assert_eq!(root_of(letter('z')), r"Z:\");
    }

    #[test]
    fn explorer_is_found_by_its_full_path() {
        let path = explorer_path().unwrap();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.is_file(), "{}", path.display());
        assert!(path.ends_with("explorer.exe"));
    }

    /// Only that the calls return. Whether anything hears them is not checked here: see
    /// `docs/PROGRESS.md`, M1-11.
    #[test]
    fn notifying_returns_with_no_such_drive() {
        let start = Instant::now();
        notify_drive_added(letter('Q'));
        notify_drive_removed(letter('Q'));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Opens a real Explorer window on a `subst` drive, finds it through the shell, and closes it.
    #[test]
    #[ignore = "needs a signed-in desktop; opens an Explorer window for a moment"]
    fn explorer_opens_at_the_root_of_the_drive() {
        let _letters = lock_letters();
        let target = tempfile::tempdir().unwrap();
        let drive = SubstDrive::new(target.path());
        open_in_explorer(drive.letter()).unwrap();

        let drive_letter = drive.letter().as_char();
        let script = format!(
            "$deadline = (Get-Date).AddSeconds(15)\n\
             while ((Get-Date) -lt $deadline) {{\n\
               $found = @((New-Object -ComObject Shell.Application).Windows() |\n\
                 Where-Object {{ $_.LocationURL -eq 'file:///{drive_letter}:/' }})\n\
               if ($found.Count -gt 0) {{ $found | ForEach-Object {{ $_.Quit() }}; exit 0 }}\n\
               Start-Sleep -Milliseconds 200\n\
             }}\n\
             exit 1"
        );
        let status = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .unwrap();
        assert!(status.success(), "no Explorer window at {drive_letter}:\\");
    }
}
