// SPDX-License-Identifier: GPL-3.0-or-later

//! `drivemount.exe --uninstall`: what the installer runs, as the user, just before it removes the
//! program (spec §9). It undoes what the app wrote for this user outside its own folders:
//!
//! - the toast keys of [`drivemount_win::toast::register`];
//! - the *Start with Windows* value, if it starts this exe (a portable copy's stays).
//!
//! Then, if asked to, *Remove my settings*: `%APPDATA%\DriveMount`, `%LOCALAPPDATA%\DriveMount` and
//! every `DriveMount:` entry in Credential Manager. `~\.ssh\known_hosts` stays: it is OpenSSH's file
//! too.
//!
//! The installer passes what it knows on the command line:
//!
//! ```text
//! --uninstall --remove-settings=[REMOVESETTINGS] --msi-ui=[UILevel] --bundle-ui=[DMBUNDLEUI]
//! ```
//!
//! `REMOVESETTINGS` is `1` or `0` from `msiexec`'s command line, or empty. Without it, the user is
//! asked if someone is there to answer: the MSI has its full or reduced UI, or the setup exe that ran
//! the MSI (silently, as it always does) shows its full UI. An uninstall nobody watches keeps them.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use drivemount_core::config::ConfigStore;
use drivemount_win::toast::AppId;
use drivemount_win::{credentials, shell, startup, toast};

use crate::app::Setup;

/// The argument that makes the exe do this instead of starting.
pub const ARGUMENT: &str = "--uninstall";

/// MSI's `UILevel` for its reduced UI; full is 5. Basic (3) and none (2) show no questions.
const MSI_REDUCED_UI: u32 = 4;

/// Burn's `WixBundleUILevel` for its full UI (`BOOTSTRAPPER_DISPLAY_FULL`).
const BUNDLE_FULL_UI: u32 = 4;

/// What to do about the user's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settings {
    /// Leave them for a later install.
    Keep,
    /// Delete them.
    Remove,
    /// Ask the user.
    Ask,
}

impl Settings {
    /// From the command line, without the program's name. `None` if it is not an uninstall.
    #[must_use]
    pub fn from_arguments(arguments: &[String]) -> Option<Self> {
        if !arguments.iter().any(|argument| argument == ARGUMENT) {
            return None;
        }
        let value = |name: &str| {
            let prefix = format!("--{name}=");
            arguments
                .iter()
                .find_map(|argument| argument.strip_prefix(&prefix))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        // UILevel can carry flags above the level itself (`INSTALLUILEVEL_*`, from 0x20).
        let level = |name: &str| value(name).and_then(|text| text.parse::<u32>().ok());
        let msi_ui = level("msi-ui").map(|level| level & 0x7);
        let bundle_ui = level("bundle-ui");

        Some(match value("remove-settings") {
            Some("1") => Self::Remove,
            Some("0") => Self::Keep,
            _ => match (bundle_ui, msi_ui) {
                (Some(bundle), _) if bundle == BUNDLE_FULL_UI => Self::Ask,
                (Some(_), _) => Self::Keep,
                (None, Some(msi)) if msi >= MSI_REDUCED_UI => Self::Ask,
                (None, Some(_)) => Self::Keep,
                // Run by hand.
                (None, None) => Self::Ask,
            },
        })
    }
}

/// What there is to undo.
#[derive(Debug, Clone)]
pub struct Places {
    /// Whose toast keys to remove.
    pub toasts: Option<AppId>,
    /// The Run value's name, and the exe it must start to be removed.
    pub run_value: Option<(String, PathBuf)>,
    /// Folders *Remove my settings* deletes.
    pub settings_folders: Vec<PathBuf>,
    /// Whether *Remove my settings* deletes the app's entries in Credential Manager.
    pub credentials: bool,
}

impl Places {
    /// Those of the user running this, for the exe running this.
    #[must_use]
    pub fn for_this_user() -> Self {
        let settings_folders = [
            ConfigStore::default_path().and_then(|config| Some(config.parent()?.to_path_buf())),
            env::var_os("LOCALAPPDATA")
                .filter(|folder| !folder.is_empty())
                .map(|folder| PathBuf::from(folder).join("DriveMount")),
        ];
        Self {
            toasts: Some(Setup::toasts_from()),
            run_value: env::current_exe()
                .ok()
                .map(|exe| (startup::RUN_VALUE.to_owned(), exe)),
            settings_folders: settings_folders.into_iter().flatten().collect(),
            credentials: true,
        }
    }
}

/// What was done, and the first thing that failed.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Whether the settings were removed.
    pub removed_settings: bool,
    /// Whether the Run value was removed.
    pub removed_run_value: bool,
    /// The first failure. The other steps still ran.
    pub error: Option<io::Error>,
}

impl Outcome {
    fn note(&mut self, result: io::Result<()>) {
        if let Err(error) = result
            && self.error.is_none()
        {
            self.error = Some(error);
        }
    }
}

/// Undoes what [`Places`] names. `ask` is called only for [`Settings::Ask`], and says whether to
/// remove them.
pub fn run(settings: Settings, places: &Places, ask: impl FnOnce() -> bool) -> Outcome {
    let mut outcome = Outcome::default();
    if let Some(app) = &places.toasts {
        outcome.note(toast::unregister(app));
    }
    if let Some((name, exe)) = &places.run_value {
        match startup::command(name) {
            Ok(Some(command)) if startup::starts(&command, exe) => {
                let removed = startup::disable(name);
                outcome.removed_run_value = removed.is_ok();
                outcome.note(removed);
            }
            Ok(_) => {}
            Err(error) => outcome.note(Err(error)),
        }
    }

    let remove = match settings {
        Settings::Keep => false,
        Settings::Remove => true,
        Settings::Ask => ask(),
    };
    if remove {
        for folder in &places.settings_folders {
            outcome.note(remove_folder(folder));
        }
        if places.credentials {
            outcome.note(credentials::delete_everything().map(|_| ()));
        }
        outcome.removed_settings = true;
    }
    outcome
}

/// For `main`: [`run`] for this user, asking with a message box. Fails if any step did; the installer
/// goes on either way.
#[must_use]
pub fn run_for_this_user(settings: Settings) -> ExitCode {
    let outcome = run(settings, &Places::for_this_user(), || {
        shell::ask_yes_no(
            "Uninstall DriveMount",
            "Also remove your DriveMount settings?\n\n\
             Your connections, their saved passwords and the logs will be deleted. \
             Choose No to keep them for a later install.",
        )
    });
    match outcome.error {
        None => ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Deletes `folder` and all in it. One that is not there is fine.
fn remove_folder(folder: &Path) -> io::Result<()> {
    match fs::remove_dir_all(folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn settings(line: &str) -> Option<Settings> {
        let arguments: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        Settings::from_arguments(&arguments)
    }

    #[test]
    fn without_the_argument_it_is_not_an_uninstall() {
        assert_eq!(settings(""), None);
        assert_eq!(settings("--tray"), None);
        assert_eq!(settings("--remove-settings=1"), None);
    }

    #[test]
    fn remove_settings_on_msiexec_s_line_wins() {
        for ui in [
            "--msi-ui=2",
            "--msi-ui=5",
            "--bundle-ui=4",
            "--bundle-ui=2",
            "",
        ] {
            assert_eq!(
                settings(&format!("--uninstall --remove-settings=1 {ui}")),
                Some(Settings::Remove),
                "{ui}"
            );
            assert_eq!(
                settings(&format!("--uninstall --remove-settings=0 {ui}")),
                Some(Settings::Keep),
                "{ui}"
            );
        }
    }

    #[test]
    fn the_msi_asks_only_with_a_ui_someone_answers() {
        let with = |level: &str| {
            settings(&format!(
                "--uninstall --remove-settings= --msi-ui={level} --bundle-ui="
            ))
        };
        assert_eq!(with("5"), Some(Settings::Ask));
        assert_eq!(with("4"), Some(Settings::Ask));
        assert_eq!(with("3"), Some(Settings::Keep));
        assert_eq!(with("2"), Some(Settings::Keep));
        // A flag above the level, such as INSTALLUILEVEL_ENDDIALOG (0x80).
        assert_eq!(with("133"), Some(Settings::Ask));
        assert_eq!(with("130"), Some(Settings::Keep));
    }

    #[test]
    fn under_the_setup_exe_its_ui_decides() {
        // Burn runs the MSI with no UI of its own.
        let with = |level: &str| settings(&format!("--uninstall --msi-ui=2 --bundle-ui={level}"));
        assert_eq!(with("4"), Some(Settings::Ask));
        assert_eq!(with("3"), Some(Settings::Keep));
        assert_eq!(with("2"), Some(Settings::Keep));
        assert_eq!(with("1"), Some(Settings::Keep));
    }

    #[test]
    fn run_by_hand_it_asks() {
        assert_eq!(settings("--uninstall"), Some(Settings::Ask));
    }

    /// A settings folder and a log folder, with the files the app keeps.
    fn folders() -> (tempfile::TempDir, Places) {
        let root = tempfile::tempdir().unwrap();
        let roaming = root.path().join("Roaming").join("DriveMount");
        let local = root.path().join("Local").join("DriveMount");
        fs::create_dir_all(&roaming).unwrap();
        fs::create_dir_all(local.join("logs")).unwrap();
        fs::write(roaming.join("config.toml"), "schema_version = 1\n").unwrap();
        fs::write(local.join("logs").join("last-crash-noticed"), "").unwrap();
        let places = Places {
            toasts: None,
            run_value: None,
            settings_folders: vec![roaming, local],
            credentials: false,
        };
        (root, places)
    }

    #[test]
    fn keep_leaves_the_folders_and_does_not_ask() {
        let (_root, places) = folders();
        let outcome = run(Settings::Keep, &places, || panic!("asked"));
        assert!(!outcome.removed_settings);
        assert!(outcome.error.is_none());
        assert!(places.settings_folders.iter().all(|folder| folder.is_dir()));
    }

    #[test]
    fn remove_deletes_the_folders_without_asking() {
        let (_root, places) = folders();
        let outcome = run(Settings::Remove, &places, || panic!("asked"));
        assert!(outcome.removed_settings);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(
            places
                .settings_folders
                .iter()
                .all(|folder| !folder.exists())
        );
    }

    #[test]
    fn ask_goes_by_the_answer() {
        for answer in [false, true] {
            let (_root, places) = folders();
            let asked = Cell::new(0);
            let outcome = run(Settings::Ask, &places, || {
                asked.set(asked.get() + 1);
                answer
            });
            assert_eq!(asked.get(), 1);
            assert_eq!(outcome.removed_settings, answer);
            assert_eq!(
                places
                    .settings_folders
                    .iter()
                    .all(|folder| !folder.exists()),
                answer
            );
        }
    }

    #[test]
    fn folders_that_are_not_there_are_fine() {
        let (root, mut places) = folders();
        places.settings_folders = vec![root.path().join("never")];
        let outcome = run(Settings::Remove, &places, || false);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
    }

    /// Touches this user's Run key for a moment, under a name of its own, as `startup`'s tests do.
    #[test]
    fn the_run_value_is_removed_for_this_exe_and_kept_for_another() {
        let name = format!("DriveMount.Test.Uninstall.{}", std::process::id());
        let installed = PathBuf::from(r"C:\nowhere\DriveMount\drivemount.exe");
        let portable = Path::new(r"D:\nowhere\portable\drivemount.exe");
        let places = Places {
            toasts: None,
            run_value: Some((name.clone(), installed.clone())),
            settings_folders: Vec::new(),
            credentials: false,
        };

        startup::enable(&name, portable).unwrap();
        let outcome = run(Settings::Keep, &places, || false);
        let kept = startup::command(&name).unwrap();
        startup::disable(&name).unwrap();
        assert!(!outcome.removed_run_value);
        assert_eq!(kept, Some(startup::command_for(portable)));

        startup::enable(&name, &installed).unwrap();
        let outcome = run(Settings::Keep, &places, || false);
        let left = startup::command(&name).unwrap();
        startup::disable(&name).unwrap();
        assert!(outcome.removed_run_value, "{:?}", outcome.error);
        assert_eq!(left, None);

        // Nothing there is fine.
        let outcome = run(Settings::Keep, &places, || false);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
    }
}
