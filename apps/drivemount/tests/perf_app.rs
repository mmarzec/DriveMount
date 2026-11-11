// SPDX-License-Identifier: GPL-3.0-or-later

//! The app's own budgets of spec §4.1, measured on the real `drivemount.exe` for `docs/perf/v1.md`:
//! start times (NFR-PERF-01, 02), memory (04), idle CPU (05), the UI thread (10) and Explorer opening
//! a drive (09). `APPDATA`, `LOCALAPPDATA` and `USERPROFILE` point at a scratch folder, so nothing of
//! the user's is read or changed, but a release build has the app's own single-instance name: the
//! user's own DriveMount must not be running.
//!
//! The drives are on the S-03 server (`tests\docker\servers.ps1 up`), as `keyonly` with the test key,
//! at the WSL VM's address (`wsl hostname -I`), and need WinFsp.
//!
//! ```text
//! DRIVEMOUNT_TEST_HOST=172.23.105.89 cargo test --release -p drivemount --test perf_app -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `PERF_IDLE_SECS` (default 600, the spec's 10 minutes) is how long idle CPU is measured, once with
//! the window hidden and once with it shown. `PERF_STARTS` (default 10) is how many starts are timed.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use drivemount_core::config::{Config, ConfigStore, LogLevel, Settings};
use drivemount_core::profile::{DriveChoice, Profile, Protocol, SftpAuth, SftpProfile};
use drivemount_vfs::DriveLetter;
use drivemount_win::drives::is_letter_free;
use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(60);
const WSL: &str = "Ubuntu-22.04";
const CONTAINER: &str = "drivemount-test-sshd-1";
const MOUNTS: usize = 5;
/// NFR-PERF-04's "after 60 s".
const SETTLING: Duration = Duration::from_secs(60);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn host() -> String {
    env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn number(name: &str, default: u64) -> u64 {
    env::var(name).map_or(default, |value| value.parse().unwrap())
}

/// Runs a PowerShell script with arguments, and returns what it printed.
fn powershell(script: &str, arguments: &[&str]) -> String {
    let folder = TempDir::new().unwrap();
    let file = folder.path().join("script.ps1");
    fs::write(&file, script).unwrap();
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&file)
        .args(arguments)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Runs a shell script in the S-03 server's container, as root. It goes in on standard input, as
/// `wsl.exe` would split it up again on the command line.
fn in_container(script: &str) -> String {
    let mut child = Command::new("wsl.exe")
        .args([
            "-d", WSL, "-u", "root", "--exec", "docker", "exec", "-i", CONTAINER, "sh",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{script}\n").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{script}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The process's working set in bytes and its CPU time so far in ms.
const USAGE: &str = r"
param([int]$ProcessId)
$p = Get-Process -Id $ProcessId
'{0} {1}' -f $p.WorkingSet64, $p.TotalProcessorTime.TotalMilliseconds
";

fn usage(process: u32) -> (u64, f64) {
    let out = powershell(USAGE, &[&process.to_string()]);
    let mut parts = out.split_whitespace();
    let bytes = parts.next().unwrap().parse().unwrap();
    let cpu = parts.next().unwrap().replace(',', ".").parse().unwrap();
    (bytes, cpu)
}

/// Presses the element of this name in a process's window, through UI Automation, if it is there.
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

/// Maximizes or restores a process's window, through UI Automation.
const WINDOW_STATE: &str = r"
param([int]$ProcessId, [string]$State)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$mine = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ProcessId)
$window = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $mine)
$window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).SetWindowVisualState(
    [System.Windows.Automation.WindowVisualState]::$State)
";

fn press_when_there(process: u32, label: &str) {
    let start = Instant::now();
    while powershell(PRESS, &[&process.to_string(), label]) != "pressed" {
        assert!(start.elapsed() < PATIENCE, "{label} was never there");
        thread::sleep(Duration::from_millis(250));
    }
}

/// Opens an Explorer window on This PC, then goes from there to a folder, as a double-click on a drive
/// does, and prints the ms until the folder's `perf-1.txt` is listed. The clock starts at the
/// navigation, so neither PowerShell's start nor a new window's is counted.
const EXPLORE: &str = r"
param([string]$Folder)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$A = [System.Windows.Automation.AutomationElement]
$named = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, 'perf-1.txt')
$shell = New-Object -ComObject Shell.Application
$before = @($shell.Windows() | ForEach-Object { $_.HWND })
$shell.Open('shell:MyComputerFolder')
$window = $null
$wait = [Diagnostics.Stopwatch]::StartNew()
while (-not $window -and $wait.Elapsed.TotalSeconds -lt 30) {
    Start-Sleep -Milliseconds 50
    $window = $shell.Windows() | Where-Object { $before -notcontains $_.HWND } | Select-Object -First 1
}
if (-not $window) { 'no window'; exit }
# Let This PC finish drawing.
Start-Sleep -Milliseconds 1500
$element = $A::FromHandle([IntPtr]$window.HWND)
$clock = [Diagnostics.Stopwatch]::StartNew()
$window.Navigate2($Folder)
while ($clock.Elapsed.TotalSeconds -lt 30) {
    if ($element.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $named)) {
        $ms = $clock.Elapsed.TotalMilliseconds
        $window.Quit()
        '{0:F0}' -f $ms
        exit
    }
    Start-Sleep -Milliseconds 5
}
$window.Quit()
'timeout'
";

fn explore(folder: &str) -> f64 {
    let out = powershell(EXPLORE, &[folder]);
    out.parse()
        .unwrap_or_else(|_| panic!("Explorer never listed {folder}: {out}"))
}

/// The S-03 server's host key, on file for the app's `known_hosts` so that no prompt comes up.
fn known_hosts_line() -> String {
    let key = in_container("cat /etc/ssh/ssh_host_ed25519_key.pub");
    let key: Vec<&str> = key.split_whitespace().take(2).collect();
    format!("[{}]:2222 {}\n", host(), key.join(" "))
}

fn free_letters(count: usize) -> Vec<DriveLetter> {
    let letters: Vec<DriveLetter> = ('D'..='Z')
        .rev()
        .filter_map(DriveLetter::new)
        .filter(|letter| is_letter_free(*letter).unwrap())
        .take(count)
        .collect();
    assert_eq!(letters.len(), count, "not enough free letters");
    letters
}

fn profile(number: usize, letter: DriveLetter, on_start: bool) -> Profile {
    let key = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/keys/id_ed25519");
    let auth = SftpAuth::KeyFile {
        path: key,
        remember: false,
    };
    let mut sftp = SftpProfile::new(host(), "keyonly", auth);
    sftp.port = 2222;
    let mut profile = Profile::new(format!("Perf {number}"), Protocol::Sftp(sftp));
    profile.drive = DriveChoice::Letter(letter);
    profile.mount_on_start = on_start;
    profile
}

/// A scratch home for the app: its config, its log and `known_hosts`.
struct Home {
    home: TempDir,
    roaming: PathBuf,
    local: PathBuf,
    letters: Vec<DriveLetter>,
    _turn: MutexGuard<'static, ()>,
}

impl Home {
    /// `mounts` profiles on the S-03 server, each on its own letter, mounted at start if `on_start`.
    fn new(mounts: usize, on_start: bool) -> Self {
        let turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let running = Command::new("tasklist.exe")
            .args(["/FI", "IMAGENAME eq drivemount.exe", "/NH"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&running.stdout).contains("drivemount.exe"),
            "DriveMount is running: a release build would hand over to it"
        );
        let home = TempDir::new().unwrap();
        let roaming = home.path().join("Roaming");
        let local = home.path().join("Local");
        let letters = free_letters(mounts);
        let profiles = letters
            .iter()
            .enumerate()
            .map(|(number, letter)| profile(number + 1, *letter, on_start))
            .collect();
        let settings = Settings {
            close_to_tray: false,
            notifications: false,
            log_level: LogLevel::Debug,
            ..Settings::default()
        };
        let mut store = ConfigStore::open(roaming.join("DriveMount").join("config.toml"));
        store
            .save(&Config {
                profiles,
                settings,
                ..Config::default()
            })
            .unwrap();
        if mounts > 0 {
            let ssh = home.path().join(".ssh");
            fs::create_dir_all(&ssh).unwrap();
            fs::write(ssh.join("known_hosts"), known_hosts_line()).unwrap();
        }
        Self {
            home,
            roaming,
            local,
            letters,
            _turn: turn,
        }
    }

    fn start(&self, arguments: &[&str]) -> Running {
        Running(
            Command::new(env!("CARGO_BIN_EXE_drivemount"))
                .args(arguments)
                .env("APPDATA", &self.roaming)
                .env("LOCALAPPDATA", &self.local)
                .env("USERPROFILE", self.home.path())
                .spawn()
                .unwrap(),
        )
    }

    fn log_file(&self) -> PathBuf {
        self.local
            .join("DriveMount")
            .join("logs")
            .join("drivemount.log")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.log_file()).unwrap_or_default()
    }

    fn forget_log(&self) {
        let _ = fs::remove_file(self.log_file());
    }

    fn wait_for_log(&self, text: &str) -> String {
        let start = Instant::now();
        loop {
            let log = self.log();
            if let Some(at) = log.find(text) {
                let end = log[at..].find('\n').map_or(log.len(), |n| at + n);
                let begin = log[..at].rfind('\n').map_or(0, |n| n + 1);
                return log[begin..end].to_owned();
            }
            assert!(start.elapsed() < PATIENCE, "never logged {text}: {log}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_until_mounted(&self) {
        let start = Instant::now();
        while self
            .letters
            .iter()
            .any(|letter| is_letter_free(*letter).unwrap())
        {
            assert!(
                start.elapsed() < PATIENCE,
                "not all drives came up: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_until_unmounted(&self) {
        let start = Instant::now();
        while !self
            .letters
            .iter()
            .all(|letter| is_letter_free(*letter).unwrap())
        {
            assert!(start.elapsed() < PATIENCE, "a drive stayed: {}", self.log());
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Kills the app if a test fails midway.
struct Running(Child);

impl Running {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    /// Asks it to close, as the window's close button does, and waits. With drives up it asks first
    /// (FR-APP-04), and is told to unmount and quit.
    fn close(&mut self) {
        let asked = Command::new("taskkill.exe")
            .args(["/PID", &self.id().to_string()])
            .output()
            .unwrap();
        assert!(asked.status.success(), "{asked:?}");
        let start = Instant::now();
        while self.0.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < PATIENCE, "it did not end");
            if start.elapsed() > Duration::from_secs(1) {
                let _ = powershell(PRESS, &[&self.id().to_string(), "Unmount and quit"]);
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The number after `field=` in a log line.
fn field(line: &str, field: &str) -> f64 {
    let at = line
        .find(&format!("{field}="))
        .unwrap_or_else(|| panic!("no {field} in {line}"));
    let rest = &line[at + field.len() + 1..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().unwrap()
}

fn summary(name: &str, values: &mut [f64]) {
    let first = values[0];
    values.sort_by(f64::total_cmp);
    let median = values[values.len() / 2];
    println!(
        "PERF | {name} | first {first:.0} ms | min {:.0} ms | median {median:.0} ms | max {:.0} ms | n {}",
        values.first().unwrap(),
        values.last().unwrap(),
        values.len()
    );
}

/// NFR-PERF-01 and 02: from the process's creation to the first frame, and with `--tray` to the icon,
/// as the app logs them, with no profiles and with five. Not truly cold: the exe and its DLLs are in
/// the file cache after the first start.
#[test]
#[ignore = "opens real windows and puts icons in the tray, and needs the S-03 server's host key"]
fn nfr_perf_01_02_start_times() {
    let starts = number("PERF_STARTS", 10);
    for (profiles, arguments, text, name) in [
        (
            0,
            &[][..],
            "the first frame is drawn",
            "NFR-PERF-01 start to first frame, no profiles",
        ),
        (
            MOUNTS,
            &[][..],
            "the first frame is drawn",
            "NFR-PERF-01 start to first frame, 5 profiles",
        ),
        (
            0,
            &["--tray"][..],
            "the icon is in the tray",
            "NFR-PERF-02 --tray to icon, no profiles",
        ),
        (
            MOUNTS,
            &["--tray"][..],
            "the icon is in the tray",
            "NFR-PERF-02 --tray to icon, 5 profiles",
        ),
    ] {
        let home = Home::new(profiles, false);
        let mut times = Vec::new();
        for _ in 0..starts {
            home.forget_log();
            let mut app = home.start(arguments);
            let line = home.wait_for_log(text);
            times.push(field(&line, "since_start_ms"));
            app.kill();
            // Explorer takes a moment to drop a killed process's icon.
            thread::sleep(Duration::from_millis(500));
        }
        println!("PERF | {name} | runs in order {times:.0?}");
        summary(name, &mut times);
    }
}

fn working_set_after_settling(app: &Running, what: &str) -> u64 {
    thread::sleep(SETTLING);
    let (bytes, _) = usage(app.id());
    #[allow(clippy::cast_precision_loss)]
    let mib = bytes as f64 / 1024.0 / 1024.0;
    println!("PERF | NFR-PERF-04 working set after 60 s, {what} | {mib:.1} MiB");
    bytes
}

/// CPU over `PERF_IDLE_SECS`, as a share of one logical CPU and of the whole machine.
fn idle_cpu(app: &Running, what: &str) {
    let seconds = number("PERF_IDLE_SECS", 600);
    let (_, before) = usage(app.id());
    let start = Instant::now();
    thread::sleep(Duration::from_secs(seconds));
    let (bytes, after) = usage(app.id());
    let wall = start.elapsed().as_secs_f64() * 1000.0;
    let cpus = thread::available_parallelism().map_or(1, std::num::NonZero::get);
    #[allow(clippy::cast_precision_loss)]
    let machine = (after - before) / wall / cpus as f64 * 100.0;
    let one = (after - before) / wall * 100.0;
    #[allow(clippy::cast_precision_loss)]
    let mib = bytes as f64 / 1024.0 / 1024.0;
    println!(
        "PERF | NFR-PERF-05 idle CPU over {seconds} s, {what} | {:.0} ms of CPU | {one:.4}% of one CPU | {machine:.5}% of {cpus} | working set after {mib:.1} MiB",
        after - before
    );
}

/// NFR-PERF-04 and 05: the working set after 60 s with no drives, and with five; then CPU while
/// idle with five, the window hidden and then shown.
#[test]
#[ignore = "opens a real window, and needs the S-03 server and WinFsp; takes over 20 minutes"]
fn nfr_perf_04_05_memory_and_idle_cpu() {
    let none = Home::new(0, false);
    let mut app = none.start(&[]);
    none.wait_for_log("the first frame is drawn");
    let shown = working_set_after_settling(&app, "no drives, window shown");
    app.kill();
    let mut app = none.start(&["--tray"]);
    none.wait_for_log("the icon is in the tray");
    let hidden = working_set_after_settling(&app, "no drives, in the tray");
    app.kill();
    drop(none);

    let five = Home::new(MOUNTS, true);
    let mut app = five.start(&["--tray"]);
    five.wait_until_mounted();
    let with_five = working_set_after_settling(&app, "5 drives, in the tray");
    #[allow(clippy::cast_precision_loss)]
    let each = (with_five.saturating_sub(hidden)) as f64 / MOUNTS as f64 / 1024.0 / 1024.0;
    println!("PERF | NFR-PERF-04 each idle drive | {each:.2} MiB");
    let _ = shown;

    idle_cpu(&app, "5 drives, in the tray");
    // A second start brings up the first one's window (FR-APP-01).
    let mut second = five.start(&[]);
    let _ = second.0.wait();
    five.wait_for_log("so showing the window");
    thread::sleep(Duration::from_secs(5));
    idle_cpu(&app, "5 drives, window shown");
    let busy = five.log().matches("busy for longer than a frame").count();
    println!("PERF | NFR-PERF-05 UI stretches over a frame while idle | {busy}");
    app.close();
    five.wait_until_unmounted();
}

/// NFR-PERF-10: the window is used as a person would (five drives mounted and unmounted, the new-profile
/// opened and closed, the settings page and back, the window maximized and restored) and the app's log says how long the UI thread was
/// kept busy at a time.
#[test]
#[ignore = "opens a real window, and needs the S-03 server and WinFsp"]
fn nfr_perf_10_ui_thread() {
    let home = Home::new(MOUNTS, false);
    let mut app = home.start(&[]);
    let id = app.id();
    let first = home.wait_for_log("the first frame is drawn");
    for round in 0..2 {
        press_when_there(id, "Mount all");
        home.wait_until_mounted();
        press_when_there(id, "Add connection");
        thread::sleep(Duration::from_millis(500));
        press_when_there(id, "Cancel");
        press_when_there(id, "Settings");
        thread::sleep(Duration::from_millis(500));
        press_when_there(id, "Back");
        for state in ["Maximized", "Normal"] {
            powershell(WINDOW_STATE, &[&id.to_string(), state]);
            thread::sleep(Duration::from_millis(500));
        }
        press_when_there(id, "Unmount all");
        home.wait_until_unmounted();
        println!("round {round} done");
    }
    app.close();
    let log = home.log();
    let longest = home.wait_for_log("longest busy stretch");
    println!("PERF | NFR-PERF-10 {first}");
    println!("PERF | NFR-PERF-10 {longest}");
    for line in log
        .lines()
        .filter(|line| line.contains("busy for longer than a frame"))
    {
        println!("PERF | NFR-PERF-10 over a frame | {line}");
    }
}

/// NFR-PERF-09: Explorer going to the root of a drive just mounted, until it lists a file there; then
/// again (from the cache), and a local folder with as many entries for what Explorer itself costs.
#[test]
#[ignore = "opens Explorer windows, and needs the S-03 server and WinFsp"]
fn nfr_perf_09_explorer_opens_the_drive() {
    // Something to list: a home folder of 100 files.
    in_container(
        "for i in $(seq 1 100); do echo $i > /home/keyonly/perf-$i.txt; done; chown keyonly /home/keyonly/perf-*",
    );
    let local = TempDir::new().unwrap();
    for i in 1..=100 {
        fs::write(local.path().join(format!("perf-{i}.txt")), i.to_string()).unwrap();
    }
    let rounds = number("PERF_STARTS", 5);
    let mut first = Vec::new();
    let mut again = Vec::new();
    let mut baseline = Vec::new();
    for _ in 0..rounds {
        let home = Home::new(1, true);
        let mut app = home.start(&[]);
        home.wait_until_mounted();
        let root = format!(r"{}\", home.letters[0]);
        first.push(explore(&root));
        again.push(explore(&root));
        baseline.push(explore(&local.path().display().to_string()));
        println!(
            "round: fresh {:.0} ms, again {:.0} ms, local {:.0} ms",
            first.last().unwrap(),
            again.last().unwrap(),
            baseline.last().unwrap()
        );
        app.close();
        home.wait_until_unmounted();
    }
    in_container("rm -f /home/keyonly/perf-*");
    summary("NFR-PERF-09 Explorer, root of a fresh mount", &mut first);
    summary("NFR-PERF-09 Explorer, the same root again", &mut again);
    summary(
        "NFR-PERF-09 Explorer, a local folder of 100 files",
        &mut baseline,
    );
}
