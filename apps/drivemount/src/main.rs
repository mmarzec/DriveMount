// SPDX-License-Identifier: GPL-3.0-or-later

//! DriveMount: the GUI binary. It is all in the library's `app`.

#![windows_subsystem = "windows"]
#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use drivemount_app::app::{Setup, run};
    use drivemount_app::uninstall;

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // The installer's uninstall step (spec §9): no window, no single instance.
    if let Some(settings) = uninstall::Settings::from_arguments(&arguments) {
        return uninstall::run_for_this_user(settings);
    }
    // `--tray` from the Run value (FR-APP-03). Others, such as COM's `-Embedding` for a toast
    // clicked in Action Center, start it as usual.
    let start_in_tray = arguments.iter().any(|argument| argument == "--tray");
    let setup = Setup::for_this_user().map(|setup| Setup {
        start_in_tray,
        ..setup
    });
    match setup.and_then(run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // `run` has logged it. This is for a problem before there is a log, from a console.
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
