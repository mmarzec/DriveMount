// SPDX-License-Identifier: GPL-3.0-or-later

//! *Start with Windows* (FR-SET-01, FR-APP-03): the value
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\DriveMount = "<exe>" --tray`.

use std::io;
use std::path::Path;

use crate::registry;

/// Where Windows looks for what to start at sign-in, for this user.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The app's value under the Run key.
pub const RUN_VALUE: &str = "DriveMount";

/// What the Run value holds for `exe`: the path in quotes, then `--tray`, so that it starts without
/// showing its window.
#[must_use]
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" --tray", exe.display())
}

/// Whether a Run value holding `command` starts `exe`, as [`command_for`] writes it. Windows paths
/// ignore case.
#[must_use]
pub fn starts(command: &str, exe: &Path) -> bool {
    command.eq_ignore_ascii_case(&command_for(exe))
}

/// Makes Windows start `exe --tray` at sign-in, under the value `name`.
///
/// # Errors
/// If the registry refuses.
pub fn enable(name: &str, exe: &Path) -> io::Result<()> {
    registry::set_string(RUN_KEY, Some(name), &command_for(exe))
}

/// Takes the value `name` away. Having nothing to take away is fine.
///
/// # Errors
/// If the registry refuses.
pub fn disable(name: &str) -> io::Result<()> {
    registry::delete_value(RUN_KEY, name)
}

/// What the value `name` holds, if it is there.
///
/// # Errors
/// If the registry refuses.
pub fn command(name: &str) -> io::Result<Option<String>> {
    registry::get_string(RUN_KEY, name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_command_quotes_the_path_and_starts_in_the_tray() {
        assert_eq!(
            command_for(Path::new(r"C:\Program Files\DriveMount\drivemount.exe")),
            r#""C:\Program Files\DriveMount\drivemount.exe" --tray"#
        );
    }

    #[test]
    fn a_command_starts_the_exe_it_names_whatever_the_case() {
        let exe = Path::new(r"C:\Program Files\DriveMount\drivemount.exe");
        assert!(starts(
            r#""C:\Program Files\DriveMount\drivemount.exe" --tray"#,
            exe
        ));
        assert!(starts(
            r#""c:\program files\drivemount\DRIVEMOUNT.EXE" --tray"#,
            exe
        ));
        assert!(!starts(
            r#""D:\Tools\DriveMount\drivemount.exe" --tray"#,
            exe
        ));
        assert!(!starts(
            r#""C:\Program Files\DriveMount\drivemount.exe""#,
            exe
        ));
    }

    /// Touches this user's Run key for a moment, under a name of its own, which Windows would only
    /// act on at a sign-in in between.
    #[test]
    fn enabling_writes_the_value_and_disabling_removes_it() {
        let name = format!("DriveMount.Test.{}", std::process::id());
        let exe = Path::new(r"C:\nowhere\drivemount.exe");
        assert_eq!(command(&name).unwrap(), None);

        enable(&name, exe).unwrap();
        assert_eq!(command(&name).unwrap(), Some(command_for(exe)));
        // Again, with the exe moved: the value follows.
        let moved = Path::new(r"D:\portable\drivemount.exe");
        enable(&name, moved).unwrap();
        assert_eq!(command(&name).unwrap(), Some(command_for(moved)));

        disable(&name).unwrap();
        assert_eq!(command(&name).unwrap(), None);
        disable(&name).unwrap();
    }
}
