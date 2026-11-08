// SPDX-License-Identifier: GPL-3.0-or-later

//! DriveMount: the GUI binary. It is all in the library's `app`.

#![windows_subsystem = "windows"]
#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use drivemount_app::app::{Setup, run};

    match Setup::for_this_user().and_then(run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // `run` has logged it. This is for a problem before there is a log, from a console.
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
