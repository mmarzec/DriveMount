// SPDX-License-Identifier: GPL-3.0-or-later

//! `fsm.exe` itself: M1-18's second criterion. It starts, shows a saved profile, writes its log where
//! it should, and ends when it is told to close. `APPDATA`, `LOCALAPPDATA` and `USERPROFILE` are pointed
//! at a temporary folder, so nothing of the user's is read or changed. It opens a real window, which is
//! read through UI Automation. No keys or clicks are sent.
//!
//! `cargo test -p fsm --test fsm_exe -- --ignored`

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use fsm_core::config::{Config, ConfigStore};
use fsm_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(30);
const NAME: &str = "Saved M1-18";

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

/// Asks it to close, as the close button would, and waits for it to end.
fn close_and_wait(app: &mut Running) -> Option<i32> {
    // Without /F this is WM_CLOSE.
    let asked = Command::new("taskkill.exe")
        .args(["/PID", &app.0.id().to_string()])
        .output()
        .unwrap();
    assert!(asked.status.success(), "{asked:?}");
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
    let mut store = ConfigStore::open(roaming.join("FsManager").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![saved_profile()],
            ..Config::default()
        })
        .unwrap();
    let script = home.path().join("names.ps1");
    fs::write(&script, LIST_NAMES).unwrap();

    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_fsm"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
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

    let log = fs::read_to_string(local.join("FsManager").join("logs").join("fsm.log")).unwrap();
    assert!(log.contains("starting"), "{log}");
    assert!(log.contains("profiles=1"), "{log}");
    assert!(log.contains("quitting"), "{log}");
    assert!(log.contains("stopped"), "{log}");
}

/// The whole way round on the real event loop: a press goes to the engine, a question comes back from
/// a real login, the answer goes back to it, the row hears of the drive, and closing leaves no drive.
#[test]
#[ignore = "opens a real window, and needs the S-03 server and WinFsp"]
fn a_mount_from_the_window_asks_about_the_key_and_closing_takes_the_drive_away() {
    let home = TempDir::new().unwrap();
    let roaming = home.path().join("Roaming");
    let local = home.path().join("Local");
    let key = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/keys/id_ed25519");
    let auth = SftpAuth::KeyFile {
        path: key,
        remember: false,
    };
    let mut sftp = SftpProfile::new("localhost", "keyonly", auth);
    sftp.port = 2222;
    let mut store = ConfigStore::open(roaming.join("FsManager").join("config.toml"));
    store
        .save(&Config {
            profiles: vec![Profile::new(NAME, Protocol::Sftp(sftp))],
            ..Config::default()
        })
        .unwrap();
    let names = home.path().join("names.ps1");
    fs::write(&names, LIST_NAMES).unwrap();
    let script = home.path().join("press.ps1");
    fs::write(&script, PRESS).unwrap();

    let mut app = Running(
        Command::new(env!("CARGO_BIN_EXE_fsm"))
            .env("APPDATA", &roaming)
            .env("LOCALAPPDATA", &local)
            .env("USERPROFILE", home.path())
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

    assert_eq!(close_and_wait(&mut app), Some(0));

    let known_hosts = home.path().join(".ssh").join("known_hosts");
    assert!(fs::read_to_string(known_hosts).unwrap().contains("ssh-"));
    let log = fs::read_to_string(local.join("FsManager").join("logs").join("fsm.log")).unwrap();
    assert!(log.contains("state=Mounted"), "{log}");
    let after_mounted = &log[log.find("state=Mounted").unwrap()..];
    assert!(after_mounted.contains("state=Disconnected"), "{log}");
    assert!(log.contains("stopped"), "{log}");
}
