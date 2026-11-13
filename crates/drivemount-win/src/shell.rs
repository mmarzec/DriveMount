// SPDX-License-Identifier: GPL-3.0-or-later

//! Telling the shell about drives, opening them in Explorer, and the small things the dialogs ask of
//! Windows.

use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use drivemount_vfs::DriveLetter;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, RECT, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR,
    OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
use windows::Win32::UI::Shell::{
    SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNE_ID, SHCNF_PATHW, SHChangeNotify, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_VOLUME, DEV_BROADCAST_VOLUME,
    DEV_BROADCAST_VOLUME_FLAGS, FindWindowW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION,
    MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT,
    SW_SHOWNORMAL, SendMessageTimeoutW, WM_DEVICECHANGE,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::wide;

/// The longest Explorer may keep us waiting for one message. It usually takes far less.
const EXPLORER_WAIT_MS: u32 = 500;

/// Where Explorer keeps what it knows about each network drive, under `HKEY_CURRENT_USER`.
const MOUNT_POINTS: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2";

/// Tells the shell that a drive has appeared, so that a running Explorer shows it (FR-MNT-06).
///
/// Waits for Explorer for at most half a second, and never for anyone else, so this does not belong
/// on an async worker thread.
pub fn notify_drive_added(letter: DriveLetter) {
    notify(SHCNE_DRIVEADD, letter);
    tell_explorer(DBT_DEVICEARRIVAL, letter);
}

/// Tells the shell that a drive has gone, so that a running Explorer stops showing it (FR-MNT-06).
///
/// Waits for Explorer for at most half a second, and never for anyone else, so this does not belong
/// on an async worker thread.
pub fn notify_drive_removed(letter: DriveLetter) {
    notify(SHCNE_DRIVEREMOVED, letter);
    tell_explorer(DBT_DEVICEREMOVECOMPLETE, letter);
}

/// `SHCNF_FLUSH` must stay out of the flags: it waits for every listener, and a hung one would hang
/// us (FR-MNT-06).
fn notify(event: SHCNE_ID, letter: DriveLetter) {
    let root = wide(root_of(letter));
    // SAFETY: with `SHCNF_PATHW`, the first item is a nul-terminated path, read during the call. The
    // event takes no second item.
    unsafe { SHChangeNotify(event, SHCNF_PATHW, Some(root.as_ptr().cast()), None) };
}

/// What actually makes Explorer look again (spec §5.8): `WM_DEVICECHANGE`, sent to its desktop window
/// alone. A broadcast is what hung on a suspended program in spike M0-1, and one cannot be posted, as
/// the message points at its details. Without a desktop window, there is no Explorer to tell.
fn tell_explorer(event: u32, letter: DriveLetter) {
    // SAFETY: the class name is a static nul-terminated string.
    let Ok(desktop) = (unsafe { FindWindowW(w!("Progman"), PCWSTR::null()) }) else {
        return;
    };
    let volume = volume_message(letter);
    // SAFETY: `volume` outlives the call, which is limited in time and copes with a window that has
    // gone. The system copies the details across to Explorer, by the size given in them.
    unsafe {
        SendMessageTimeoutW(
            desktop,
            WM_DEVICECHANGE,
            WPARAM(event as usize),
            LPARAM((&raw const volume) as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            EXPLORER_WAIT_MS,
            None,
        )
    };
}

fn volume_message(letter: DriveLetter) -> DEV_BROADCAST_VOLUME {
    DEV_BROADCAST_VOLUME {
        dbcv_size: size_of::<DEV_BROADCAST_VOLUME>() as u32,
        dbcv_devicetype: DBT_DEVTYP_VOLUME.0,
        dbcv_reserved: 0,
        dbcv_unitmask: unit_mask(letter),
        dbcv_flags: DEV_BROADCAST_VOLUME_FLAGS(0),
    }
}

/// `A:` is bit 0.
fn unit_mask(letter: DriveLetter) -> u32 {
    1 << (letter.as_char() as u32 - 'A' as u32)
}

/// Sets the name Explorer shows for the network drive `\drivemount\<slug>`, as in "Production (Z:)", instead
/// of one made from its UNC path (FR-MNT-05). It must be there before Explorer hears of the drive.
///
/// # Errors
/// If the registry refuses.
pub fn set_drive_label(slug: &str, label: &str) -> io::Result<()> {
    let key_name = wide(mount_point_key(slug));
    let mut key = HKEY::default();
    // SAFETY: the name is nul-terminated, and `key` outlives the call.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &raw mut key,
            None,
        )
    })?;
    let value: Vec<u8> = wide(label).iter().flat_map(|u| u.to_le_bytes()).collect();
    // SAFETY: `key` was just opened to set values, and the data is a nul-terminated UTF-16 string
    // for the type given.
    let set =
        check(unsafe { RegSetValueExW(key, w!("_LabelFromReg"), None, REG_SZ, Some(&value)) });
    // SAFETY: `key` is open and is not used after this. Nothing can be done if closing fails.
    let _ = unsafe { RegCloseKey(key) };
    set
}

/// Removes what [`set_drive_label`], and Explorer itself, keep for this drive. Having nothing to remove
/// is fine.
///
/// # Errors
/// If the registry refuses.
pub fn forget_drive_label(slug: &str) -> io::Result<()> {
    let key_name = wide(mount_point_key(slug));
    // SAFETY: the name is nul-terminated.
    let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key_name.as_ptr())) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(result)
}

/// Explorer spells `\\drivemount\<slug>` with `#` for each backslash.
fn mount_point_key(slug: &str) -> String {
    format!(r"{MOUNT_POINTS}\##drivemount#{slug}")
}

fn check(result: WIN32_ERROR) -> io::Result<()> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result.0.cast_signed()))
    }
}

/// Opens a web page in the user's browser, as the *Download WinFsp* button does (FR-APP-06). Only
/// `https://` is taken, as anything else could start a program.
///
/// # Errors
/// If it is not `https://`, or nothing could open it.
pub fn open_web_page(url: &str) -> io::Result<()> {
    if !url.starts_with("https://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only an https:// page can be opened",
        ));
    }
    let page = wide(url);
    // SAFETY: all strings are nul-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(page.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success, as for Explorer below.
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW could not open the page: code {code}"
        )))
    }
}

/// Whether any of this rectangle, in physical pixels, is on a monitor. A remembered window position
/// may not be, once a monitor has gone.
#[must_use]
pub fn is_on_a_monitor(x: i32, y: i32, width: u32, height: u32) -> bool {
    let rect = RECT {
        left: x,
        top: y,
        right: x.saturating_add_unsigned(width),
        bottom: y.saturating_add_unsigned(height),
    };
    // SAFETY: `rect` outlives the call, which only reads it.
    let monitor = unsafe { MonitorFromRect(&raw const rect, MONITOR_DEFAULTTONULL) };
    !monitor.is_invalid()
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

/// Opens the folder a file is in, with the file selected: *Open file location* (spec §7.4.3).
///
/// # Errors
/// If Explorer cannot be started.
pub fn show_in_explorer(file: &Path) -> io::Result<()> {
    let explorer = wide(explorer_path()?);
    let arguments = wide(select_argument(file));
    // SAFETY: all strings are nul-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(explorer.as_ptr()),
            PCWSTR(arguments.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success, as in `open_in_explorer`.
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW could not start Explorer: code {code}"
        )))
    }
}

/// Opens a folder in Explorer, as *Open logs folder* does (FR-SET-06). The folder is made if it is
/// not there yet, as the log's is until the first line.
///
/// # Errors
/// If the folder cannot be made, or Explorer cannot be started.
pub fn open_folder(folder: &Path) -> io::Result<()> {
    std::fs::create_dir_all(folder)?;
    let explorer = wide(explorer_path()?);
    let folder = wide(folder);
    // SAFETY: all strings are nul-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(explorer.as_ptr()),
            PCWSTR(folder.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success, as in `open_in_explorer`.
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW could not start Explorer: code {code}"
        )))
    }
}

/// Opens an HTML file in the user's browser, as the *Third-party licences* button does
/// (NFR-LIC-03). Nothing else is taken, as opening another kind of file could start a program.
///
/// # Errors
/// If it is not an `.html` file that is there, or nothing could open it.
pub fn open_html_file(file: &Path) -> io::Result<()> {
    let is_html = file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("html"));
    if !is_html {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only an .html file can be opened",
        ));
    }
    if !file.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not there", file.display()),
        ));
    }
    let path = wide(file);
    // SAFETY: all strings are nul-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecuteW could not open the file: code {code}"
        )))
    }
}

/// What Explorer takes to select a file.
fn select_argument(file: &Path) -> String {
    format!("/select,\"{}\"", file.display())
}

/// Asks a yes-or-no question in a message box of its own, with *No* as the default, and blocks until
/// it is answered. For a program with no window, such as the uninstaller's step. `false` for *No*, and
/// if the box could not be shown.
#[must_use]
pub fn ask_yes_no(title: &str, question: &str) -> bool {
    let (title, question) = (wide(title), wide(question));
    // SAFETY: both strings are nul-terminated and outlive the call.
    let answer = unsafe {
        MessageBoxW(
            None,
            PCWSTR(question.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    answer == IDYES
}

/// Whether Caps Lock is on, for the hint in a password prompt (spec §7.4.1).
#[must_use]
pub fn caps_lock_is_on() -> bool {
    // SAFETY: takes a key code and returns a number. The low bit is whether the key is toggled on.
    let state = unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) };
    state & 1 != 0
}

/// The longest path the file dialog gives back, in UTF-16 units, as `\?\` paths can be this long.
const MOST_PATH_UNITS: usize = 32_768;

/// Asks the user to choose a file, starting in `start_in`. `None` if they cancelled.
///
/// This blocks until they answer, so it belongs on a thread of its own, which may be any.
///
/// # Errors
/// If the dialog could not be shown.
pub fn pick_file(title: &str, start_in: Option<&Path>) -> io::Result<Option<PathBuf>> {
    let title = wide(title);
    let start_in = start_in.map(wide);
    let mut chosen = vec![0u16; MOST_PATH_UNITS];
    let mut request = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: PWSTR(chosen.as_mut_ptr()),
        nMaxFile: MOST_PATH_UNITS as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrInitialDir: start_in
            .as_ref()
            .map_or_else(PCWSTR::null, |folder| PCWSTR(folder.as_ptr())),
        // Without the last one, the dialog would change the whole process's current directory.
        Flags: OFN_FILEMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
        ..OPENFILENAMEW::default()
    };
    // SAFETY: every pointer in `request` is to a nul-terminated string or to `chosen`, whose length is
    // given, and all of them outlive the call.
    let answered = unsafe { GetOpenFileNameW(&raw mut request) }.as_bool();
    if answered {
        let end = chosen
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(chosen.len());
        return Ok(Some(std::ffi::OsString::from_wide(&chosen[..end]).into()));
    }
    // SAFETY: takes nothing and returns a number.
    let code = unsafe { CommDlgExtendedError() };
    if code.0 == 0 {
        Ok(None)
    } else {
        Err(io::Error::other(format!(
            "the file dialog could not be shown: code {:#x}",
            code.0
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
    fn explorer_is_told_which_file_to_select_even_with_spaces_in_its_name() {
        let file = Path::new(r"C:\Users\a b\.ssh\known_hosts");
        assert_eq!(
            select_argument(file),
            r#"/select,"C:\Users\a b\.ssh\known_hosts""#
        );
    }

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

    /// Only that the calls return. That Explorer hears them is shown in `drivemount-winfsp`'s
    /// `tests/explorer.rs`, with a real drive and a real window.
    #[test]
    fn notifying_returns_with_no_such_drive() {
        let start = Instant::now();
        notify_drive_added(letter('Q'));
        notify_drive_removed(letter('Q'));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn only_a_secure_web_page_is_opened() {
        for url in [
            "http://winfsp.dev/",
            r"C:\Windows
otepad.exe",
            "file:///C:/",
            "",
        ] {
            let refused = open_web_page(url).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{url}");
        }
    }

    #[test]
    fn a_window_far_from_every_monitor_is_not_on_one() {
        assert!(!is_on_a_monitor(-1_000_000, -1_000_000, 640, 420));
        assert!(!is_on_a_monitor(i32::MAX - 10, i32::MAX - 10, 640, 420));
    }

    /// Windows puts the primary monitor's corner at 0, 0.
    #[test]
    #[ignore = "needs a signed-in desktop, which has a monitor"]
    fn a_window_at_the_corner_of_the_primary_monitor_is_on_it() {
        assert!(is_on_a_monitor(0, 0, 640, 420));
    }

    #[test]
    fn the_message_names_the_one_letter() {
        assert_eq!(unit_mask(letter('A')), 1);
        assert_eq!(unit_mask(letter('C')), 1 << 2);
        assert_eq!(unit_mask(letter('Z')), 1 << 25);

        let message = volume_message(letter('Z'));
        assert_eq!(message.dbcv_devicetype, DBT_DEVTYP_VOLUME.0);
        assert_eq!(message.dbcv_unitmask, 1 << 25);
        // Windows copies this many bytes across to Explorer.
        assert_eq!(message.dbcv_size, 20);
    }

    #[test]
    fn a_slug_has_its_own_key() {
        assert_eq!(
            mount_point_key("web"),
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##drivemount#web"
        );
    }

    #[test]
    fn a_label_is_stored_and_forgotten() {
        use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};

        let slug = format!("drivemount-win-test-{}", std::process::id());
        let read = || {
            let name = wide(mount_point_key(&slug));
            let mut buffer = [0u16; 64];
            let mut size = size_of_val(&buffer) as u32;
            // SAFETY: the strings are nul-terminated, and the buffer and its size outlive the call.
            let result = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(name.as_ptr()),
                    w!("_LabelFromReg"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buffer.as_mut_ptr().cast()),
                    Some(&raw mut size),
                )
            };
            (result == ERROR_SUCCESS).then(|| {
                let end = buffer.iter().position(|unit| *unit == 0).unwrap();
                String::from_utf16(&buffer[..end]).unwrap()
            })
        };

        set_drive_label(&slug, "Zażółć 😀").unwrap();
        assert_eq!(read().as_deref(), Some("Zażółć 😀"));
        set_drive_label(&slug, "Renamed").unwrap();
        assert_eq!(read().as_deref(), Some("Renamed"));
        forget_drive_label(&slug).unwrap();
        assert_eq!(read(), None);
        forget_drive_label(&slug).unwrap();
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
