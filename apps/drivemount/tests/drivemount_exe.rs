// SPDX-License-Identifier: GPL-3.0-or-later

//! `drivemount.exe` itself: M1-18's second criterion. It starts, shows a saved profile, writes its log where
//! it should, and ends when it is told to close. Killed instead, it leaves no drive (M1-19). `APPDATA`, `LOCALAPPDATA` and `USERPROFILE` are pointed
//! at a temporary folder, so nothing of the user's is read or changed. It opens a real window, which is
//! read through UI Automation. No keys or clicks are sent.
//!
//! `cargo test -p drivemount --test drivemount_exe -- --ignored`

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use drivemount_core::config::{Config, ConfigStore, Settings};
use drivemount_core::profile::{DriveChoice, Profile, Protocol, SftpAuth, SftpProfile};
use drivemount_vfs::DriveLetter;
use drivemount_win::drives::is_letter_free;
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(30);
const NAME: &str = "Saved M1-18";
/// A debug build takes its single-instance name from here (FR-APP-01), so that these tests run beside
/// each other and beside the user's own app.
const INSTANCE_VARIABLE: &str = "DRIVEMOUNT_TEST_INSTANCE";

fn unique_instance() -> String {
    format!(r"Local\DriveMount.Test.{}", uuid::Uuid::new_v4())
}

/// Prints the names of what a process's window holds, once it has one.
const LIST_NAMES: &str = r"
param([int]$ProcessId)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$mine = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ProcessId)
$window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $mine)
if ($window) {
    $all = [System.Windows.Automation.Condition]::TrueCondition
    foreach ($element in $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $all)) {
        $element.Current.Name
    }
}
";

/// Presses the button of this name in a process's window, through UI Automation, if it is there.
const PRESS: &str = r"
param([int]$ProcessId, [string]$Label)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$mine = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ProcessId)
$window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $mine)
if ($window) {
    $named = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, $Label)
    $button = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $named)
    if ($button) {
        $button.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
        'pressed'
    }
}
";

fn saved_profile() -> Profile {
    let sftp = SftpProfile::new("example.com", "deploy", SftpAuth::Agent);
    Profile::new(NAME, Protocol::Sftp(sftp))
}

fn press(process: u32, script: &Path, label: &str) -> bool {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .arg(process.to_string())
        .arg(label)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).contains("pressed")
}

fn press_when_there(process: u32, script: &Path, label: &str) {
    let start = Instant::now();
    while !press(process, script, label) {
        assert!(start.elapsed() < PATIENCE, "{label} never came");
        thread::sleep(Duration::from_millis(250));
    }
}

/// `close_to_tray` off, so that the close button ends the app (FR-APP-02), as a test cannot reach
/// the tray's *Quit*.
fn closing_quits() -> Settings {
    Settings {
        close_to_tray: false,
        ..Settings::default()
    }
}

/// Asks it to close, as the close button would, and waits for it to end.
fn close_and_wait(app: &mut Running) -> Option<i32> {
    // Without /F this is WM_CLOSE.
    let asked = Command::new("taskkill.exe")
        .args(["/PID", &app.0.id().to_string()])
        .output()
        .unwrap();
    assert!(asked.status.success(), "{asked:?}");
    wait_for_the_end(app)
}

fn wait_for_the_end(app: &mut Running) -> Option<i32> {
    let start = Instant::now();
    loop {
        if let Some(status) = app.0.try_wait().unwrap() {
            return status.code();
        }
        assert!(start.elapsed() < PATIENCE, "it did not end");
        thread::sleep(Duration::from_millis(50));
    }
}

fn names_in_window(process: u32, script: &Path) -> String {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .arg(process.to_string())
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Ends it whatever the test finds, so that a failure leaves no window on the desktop.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
#[ignore = "opens a real window"]
fn it_starts_shows_the_saved_profile_logs_and_closes() {
    let home = TempDir::new().unwrap();
    let roaming = home.path().join("Roaming");
    let local = home.path().join("Local");
    let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![saved_profile()],
            settings: closing_quits(),
            ..Config::default()
        })
        .unwrap();
    let script = home.path().join("names.ps1");
    fs::write(&script, LIST_NAMES).unwrap();

    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_drivemount"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
            .env(INSTANCE_VARIABLE, unique_instance())
            .spawn()
            .unwrap(),
    );
    let id = app.0.id();

    let start = Instant::now();
    loop {
        let names = names_in_window(id, &script);
        if names.contains(NAME) {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "not shown: {names}");
        assert!(app.0.try_wait().unwrap().is_none(), "it ended by itself");
        thread::sleep(Duration::from_millis(250));
    }

    assert_eq!(close_and_wait(&mut app), Some(0));

    let log =
        fs::read_to_string(local.join("DriveMount").join("logs").join("drivemount.log")).unwrap();
    assert!(log.contains("starting"), "{log}");
    assert!(log.contains("profiles=1"), "{log}");
    assert!(log.contains("quitting"), "{log}");
    assert!(log.contains("stopped"), "{log}");
}

/// FR-APP-02 with the setting's default: the close button hides the window and the app goes on, in
/// the tray. The icon itself can only be seen by a person (spike M0-3), so the log says it is there.
#[test]
#[ignore = "opens a real window, and puts an icon in the tray"]
fn closing_hides_the_window_to_the_tray_and_the_app_goes_on() {
    let home = TempDir::new().unwrap();
    let roaming = home.path().join("Roaming");
    let local = home.path().join("Local");
    let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![saved_profile()],
            ..Config::default()
        })
        .unwrap();
    let script = home.path().join("names.ps1");
    fs::write(&script, LIST_NAMES).unwrap();
    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_drivemount"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
            .env(INSTANCE_VARIABLE, unique_instance())
            .spawn()
            .unwrap(),
    );
    let id = app.0.id();
    let start = Instant::now();
    while !names_in_window(id, &script).contains(NAME) {
        assert!(start.elapsed() < PATIENCE, "not shown");
        thread::sleep(Duration::from_millis(250));
    }

    // Without /F this is WM_CLOSE, as the close button sends.
    let asked = Command::new("taskkill.exe")
        .args(["/PID", &id.to_string()])
        .output()
        .unwrap();
    assert!(asked.status.success(), "{asked:?}");
    let start = Instant::now();
    while names_in_window(id, &script).contains(NAME) {
        assert!(start.elapsed() < PATIENCE, "the window is still up");
        thread::sleep(Duration::from_millis(250));
    }
    thread::sleep(Duration::from_secs(1));
    assert!(app.0.try_wait().unwrap().is_none(), "it ended");

    let log =
        fs::read_to_string(local.join("DriveMount").join("logs").join("drivemount.log")).unwrap();
    assert!(log.contains("the icon is in the tray"), "{log}");
    assert!(!log.contains("quitting"), "{log}");
}

/// The whole way round on the real event loop: a press goes to the engine, a question comes back from
/// a real login, the answer goes back to it, the row hears of the drive, and closing leaves no drive.
/// A profile for the S-03 server that logs in with a key file, so nothing is typed.
fn key_profile() -> Profile {
    let key = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/keys/id_ed25519");
    let auth = SftpAuth::KeyFile {
        path: key,
        remember: false,
    };
    let mut sftp = SftpProfile::new("localhost", "keyonly", auth);
    sftp.port = 2222;
    Profile::new(NAME, Protocol::Sftp(sftp))
}

/// NFR-REL-04 for `drivemount.exe` itself (plan §9.2): killed with a drive up, it leaves none.
#[test]
#[ignore = "opens a real window, and needs the S-03 server and WinFsp"]
fn killing_it_with_a_drive_up_leaves_no_drive() {
    let home = TempDir::new().unwrap();
    let roaming = home.path().join("Roaming");
    let local = home.path().join("Local");
    let letter = ('D'..='Z')
        .rev()
        .filter_map(DriveLetter::new)
        .find(|letter| is_letter_free(*letter).unwrap())
        .expect("no free letter");
    let mut profile = key_profile();
    profile.drive = DriveChoice::Letter(letter);
    let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![profile],
            ..Config::default()
        })
        .unwrap();
    let script = home.path().join("press.ps1");
    fs::write(&script, PRESS).unwrap();

    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_drivemount"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
            .env(INSTANCE_VARIABLE, unique_instance())
            .spawn()
            .unwrap(),
    );
    let id = app.0.id();
    press_when_there(id, &script, &format!("Mount {NAME}"));
    press_when_there(id, &script, "Trust and save");
    let start = Instant::now();
    while is_letter_free(letter).unwrap() {
        assert!(start.elapsed() < PATIENCE, "{letter} never came up");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        fs::read_dir(format!(r"{letter}\")).is_ok(),
        "{letter} works"
    );

    let killed = Command::new("taskkill.exe")
        .args(["/F", "/PID", &id.to_string()])
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
    app.0.wait().unwrap();

    let start = Instant::now();
    while !is_letter_free(letter).unwrap() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{letter} is still there after the kill"
        );
        thread::sleep(Duration::from_millis(50));
    }
    println!("{letter} went {:?} after the kill", start.elapsed());
}

#[test]
#[ignore = "opens a real window, and needs the S-03 server and WinFsp"]
fn a_mount_from_the_window_asks_about_the_key_and_closing_takes_the_drive_away() {
    let home = TempDir::new().unwrap();
    let roaming = home.path().join("Roaming");
    let local = home.path().join("Local");
    let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![key_profile()],
            settings: closing_quits(),
            ..Config::default()
        })
        .unwrap();
    let names = home.path().join("names.ps1");
    fs::write(&names, LIST_NAMES).unwrap();
    let script = home.path().join("press.ps1");
    fs::write(&script, PRESS).unwrap();

    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_drivemount"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
            .env(INSTANCE_VARIABLE, unique_instance())
            .spawn()
            .unwrap(),
    );
    let id = app.0.id();

    press_when_there(id, &script, &format!("Mount {NAME}"));
    press_when_there(id, &script, "Trust and save");
    let unmount = format!("Unmount {NAME}");
    let start = Instant::now();
    while !names_in_window(id, &names).contains(&unmount) {
        assert!(start.elapsed() < PATIENCE, "the row never said it was up");
        thread::sleep(Duration::from_millis(250));
    }

    // With a drive up, closing asks first (FR-APP-04).
    let asked = Command::new("taskkill.exe")
        .args(["/PID", &id.to_string()])
        .output()
        .unwrap();
    assert!(asked.status.success(), "{asked:?}");
    press_when_there(id, &script, "Unmount and quit");
    assert_eq!(wait_for_the_end(&mut app), Some(0));

    let known_hosts = home.path().join(".ssh").join("known_hosts");
    assert!(fs::read_to_string(known_hosts).unwrap().contains("ssh-"));
    let log =
        fs::read_to_string(local.join("DriveMount").join("logs").join("drivemount.log")).unwrap();
    assert!(log.contains("state=Mounted"), "{log}");
    let after_mounted = &log[log.find("state=Mounted").unwrap()..];
    assert!(after_mounted.contains("state=Disconnected"), "{log}");
    assert!(log.contains("stopped"), "{log}");
}

/// The app's own files under a scratch home, with the saved profile, and a single-instance name of
/// its own.
struct Home {
    home: TempDir,
    roaming: PathBuf,
    local: PathBuf,
    names: PathBuf,
    instance: String,
}

impl Home {
    fn new(settings: Settings) -> Self {
        let home = TempDir::new().unwrap();
        let roaming = home.path().join("Roaming");
        let local = home.path().join("Local");
        let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
        store
            .save(&Config {
                profiles: vec![saved_profile()],
                settings,
                ..Config::default()
            })
            .unwrap();
        let names = home.path().join("names.ps1");
        fs::write(&names, LIST_NAMES).unwrap();
        Self {
            home,
            roaming,
            local,
            names,
            instance: unique_instance(),
        }
    }

    fn start(&self, arguments: &[&str]) -> Running {
        Running(
            Command::new(env!("CARGO_BIN_EXE_drivemount"))
                .args(arguments)
                .env("APPDATA", &self.roaming)
                .env("LOCALAPPDATA", &self.local)
                .env("USERPROFILE", self.home.path())
                .env(INSTANCE_VARIABLE, &self.instance)
                .spawn()
                .unwrap(),
        )
    }

    fn logs(&self) -> PathBuf {
        self.local.join("DriveMount").join("logs")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.logs().join("drivemount.log")).unwrap_or_default()
    }

    fn wait_for_log(&self, text: &str) {
        let start = Instant::now();
        while !self.log().contains(text) {
            assert!(
                start.elapsed() < PATIENCE,
                "never logged {text}: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn shows(&self, process: u32, text: &str) -> bool {
        names_in_window(process, &self.names).contains(text)
    }

    fn wait_until_shown(&self, process: u32, text: &str) {
        let start = Instant::now();
        while !self.shows(process, text) {
            assert!(start.elapsed() < PATIENCE, "{text} was not shown");
            thread::sleep(Duration::from_millis(250));
        }
    }
}

/// FR-APP-01's acceptance: a second start leaves one process, with its window up. The first's window
/// is hidden in the tray when the second starts, and comes back.
#[test]
#[ignore = "opens a real window, and puts an icon in the tray"]
fn a_second_start_brings_up_the_first_and_ends() {
    let home = Home::new(Settings::default());
    let first = home.start(&[]);
    let id = first.0.id();
    home.wait_until_shown(id, NAME);
    // Without /F this is WM_CLOSE, which hides it to the tray.
    let asked = Command::new("taskkill.exe")
        .args(["/PID", &id.to_string()])
        .output()
        .unwrap();
    assert!(asked.status.success(), "{asked:?}");
    let start = Instant::now();
    while home.shows(id, NAME) {
        assert!(start.elapsed() < PATIENCE, "the window is still up");
        thread::sleep(Duration::from_millis(250));
    }

    let mut second = home.start(&[]);
    assert_eq!(wait_for_the_end(&mut second), Some(0));
    home.wait_until_shown(id, NAME);
    home.wait_for_log("the app was started again, so showing the window");
    let mut first = first;
    assert!(first.0.try_wait().unwrap().is_none(), "the first ended");
    assert_eq!(
        home.log().matches(" starting").count(),
        1,
        "the second wrote to the log: {}",
        home.log()
    );
}

/// FR-APP-03: `--tray` starts with no window once the icon is in. Another start brings it up.
#[test]
#[ignore = "puts an icon in the tray, and opens a real window"]
fn with_tray_it_starts_with_no_window() {
    let home = Home::new(Settings::default());
    let app = home.start(&["--tray"]);
    let id = app.0.id();
    home.wait_for_log("the icon is in the tray");
    thread::sleep(Duration::from_secs(2));
    assert!(!home.shows(id, NAME), "the window is up");
    let log = home.log();
    assert!(
        log.contains("started in the tray") || log.contains("so the window stays hidden"),
        "{log}"
    );

    let mut second = home.start(&[]);
    assert_eq!(wait_for_the_end(&mut second), Some(0));
    home.wait_until_shown(id, NAME);
}

/// FR-APP-07's notice: a crash report the last run left is told about at the next start, once. That a
/// panic writes one is `drivemount_app::crash`'s own test.
#[test]
#[ignore = "opens a real window"]
fn a_crash_report_from_the_last_run_is_told_about_once() {
    let home = Home::new(closing_quits());
    fs::create_dir_all(home.logs()).unwrap();
    let report = "crash-2026-11-09T171153.042Z.txt";
    fs::write(home.logs().join(report), "DriveMount stopped").unwrap();

    let mut app = home.start(&[]);
    let id = app.0.id();
    home.wait_until_shown(id, report);
    assert!(home.shows(id, "Open logs folder"));
    assert_eq!(close_and_wait(&mut app), Some(0));
    assert!(
        home.log().contains("the last run crashed"),
        "{}",
        home.log()
    );

    let mut again = home.start(&[]);
    let id = again.0.id();
    home.wait_until_shown(id, NAME);
    assert!(!home.shows(id, report), "told about twice");
    assert_eq!(close_and_wait(&mut again), Some(0));
}
