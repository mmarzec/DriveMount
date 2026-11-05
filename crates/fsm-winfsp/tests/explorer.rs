// SPDX-License-Identifier: GPL-3.0-or-later

//! What a running Explorer shows, read from its own window through UI Automation (FR-MNT-05 and
//! FR-MNT-06, spike M0-1 §7). Asking the shell from another process would not do: a new process
//! enumerates the drives itself and sees them whatever Explorer knows.
//!
//! These need a signed-in desktop, so they are `#[ignore]`d, and they open a window on "This PC" for
//! a moment:
//!
//! ```text
//! cargo test -p fsm-winfsp --test explorer -- --ignored --test-threads=1
//! ```

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fsm_vfs::{
    ConnectError, Connector, DriveLetter, MountBackend, Prompter, RemoteFs, async_trait,
};
use fsm_winfsp::{VolumeConfig, WinFspBackend};
use support::{Probe, TrustOnce, free_letter, unique_slug};
use tokio::runtime::Runtime;

/// Well past what is usual here, to catch Explorer not being told at all without failing on a slow
/// moment: showing takes about 850 ms, and hiding about 1200 ms, which is over FR-MNT-06's 1 s. See
/// `docs/PROGRESS.md`. Each round prints what it measured. One look at the window costs about 100 ms.
const LIMIT_MS: u64 = 3000;

struct OnProbe(Arc<Probe>);

#[async_trait]
impl Connector for OnProbe {
    async fn connect(&self, _: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        Ok(Arc::clone(&self.0) as Arc<dyn RemoteFs>)
    }

    async fn probe(&self) -> Result<Duration, ConnectError> {
        Ok(Duration::ZERO)
    }
}

/// Opens a window on "This PC", and then for each line `show` or `hide` on its input, answers with how
/// many milliseconds it took for `$name` to appear or go, or `never`. Opening "This PC" directly would
/// reuse a window the user already has on it, so a new window starts elsewhere and goes there.
const WATCHER: &str = r#"
param([string]$Name, [string]$Elsewhere)
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$shell = New-Object -ComObject Shell.Application
$before = @($shell.Windows() | ForEach-Object { $_.HWND })
$shell.Open($Elsewhere)
$window = $null
for ($i = 0; $i -lt 100 -and -not $window; $i++) {
    Start-Sleep -Milliseconds 100
    $window = @($shell.Windows() | Where-Object { $before -notcontains $_.HWND }) | Select-Object -First 1
}
if (-not $window) { [Console]::Out.WriteLine('nowindow'); exit 1 }
$window.Navigate2('shell:MyComputerFolder')
Start-Sleep -Seconds 2
$item = New-Object Windows.Automation.PropertyCondition(
    [Windows.Automation.AutomationElement]::ControlTypeProperty,
    [Windows.Automation.ControlType]::ListItem)
function Shown {
    for ($try = 0; $try -lt 5; $try++) {
        try {
            $root = [Windows.Automation.AutomationElement]::FromHandle([IntPtr]$window.HWND)
            $names = @($root.FindAll([Windows.Automation.TreeScope]::Descendants, $item) | ForEach-Object { $_.Current.Name })
            return $names -contains $Name
        } catch { Start-Sleep -Milliseconds 20 }
    }
    return $false
}
try {
    # The first look loads what it needs, which would count against Explorer otherwise.
    $null = Shown
    $look = [Diagnostics.Stopwatch]::StartNew()
    $null = Shown
    [Console]::Error.WriteLine("one look at the window takes $($look.ElapsedMilliseconds) ms")
    [Console]::Out.WriteLine('ready')
    while ($null -ne ($line = [Console]::In.ReadLine())) {
        $wanted = $line -match 'show'
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $answer = 'never'
        while ($watch.ElapsedMilliseconds -lt 5000) {
            if ((Shown) -eq $wanted) { $answer = $watch.ElapsedMilliseconds; break }
            Start-Sleep -Milliseconds 20
        }
        [Console]::Out.WriteLine("$answer")
    }
} finally {
    $window.Quit()
}
"#;

struct Watcher {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    _script: tempfile::TempDir,
}

impl Watcher {
    fn start(name: &str) -> Self {
        let script = tempfile::tempdir().unwrap();
        let path = script.path().join("watch.ps1");
        // Windows PowerShell reads a script without a byte order mark in the machine's code page.
        let with_bom = format!("\u{feff}{WATCHER}");
        std::fs::write(&path, with_bom).unwrap();
        let mut child = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&path)
            .arg(name)
            .arg(script.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = Some(child.stdin.take().unwrap());
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut watcher = Self {
            child,
            input,
            output,
            _script: script,
        };
        assert_eq!(watcher.answer(), "ready");
        watcher
    }

    fn answer(&mut self) -> String {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        line.trim().to_owned()
    }

    /// Starts timing how long it takes for the name to appear or go.
    fn ask(&mut self, what: &str) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{what}").unwrap();
        input.flush().unwrap();
    }

    fn millis(&mut self) -> u64 {
        let answer = self.answer();
        answer
            .parse()
            .unwrap_or_else(|_| panic!("Explorer never got there: {answer}"))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // The end of its input makes the script close its window, which killing it would leave open.
        drop(self.input.take());
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn backend(label: &str) -> WinFspBackend<OnProbe> {
    WinFspBackend::new(
        OnProbe(Arc::default()),
        VolumeConfig {
            slug: unique_slug(),
            label: label.to_owned(),
            file_system_name: "FSM-SFTP".to_owned(),
            op_timeout: Duration::from_secs(30),
            cache_ttl: Duration::from_secs(5),
            threads: 0,
            read_only: false,
        },
    )
}

/// Mounts and unmounts, and gives how long Explorer took to show the drive once the mount had returned,
/// and to stop showing it once the unmount had. That is when the app says the drive is up or gone.
fn time_a_round(
    runtime: &Runtime,
    drive: &WinFspBackend<OnProbe>,
    letter: DriveLetter,
    name: &str,
) -> (u64, u64) {
    let mut watcher = Watcher::start(name);

    let start = Instant::now();
    runtime
        .block_on(drive.mount(letter, &TrustOnce::default()))
        .unwrap();
    println!("mount took {:?}", start.elapsed());
    watcher.ask("show");
    let shown = watcher.millis();

    let start = Instant::now();
    runtime.block_on(drive.unmount()).unwrap();
    println!("unmount took {:?}", start.elapsed());
    watcher.ask("hide");
    let hidden = watcher.millis();
    (shown, hidden)
}

#[test]
#[ignore = "needs a signed-in desktop; opens an Explorer window for a moment"]
fn a_running_explorer_shows_the_name_and_hides_it_within_a_second() {
    let runtime = Runtime::new().unwrap();
    let drive = backend("Production");
    let letter = free_letter();
    let name = format!("Production ({letter})");

    // Several rounds on one letter: Explorer must not be left thinking the drive is still there.
    for round in 0..3 {
        let (shown, hidden) = time_a_round(&runtime, &drive, letter, &name);
        println!("round {round}: shown after {shown} ms, hidden after {hidden} ms");
        assert!(shown <= LIMIT_MS, "shown after {shown} ms");
        assert!(hidden <= LIMIT_MS, "hidden after {hidden} ms");
    }
}

#[test]
#[ignore = "needs a signed-in desktop; opens an Explorer window for a moment"]
fn a_name_that_needs_unicode_is_shown_as_it_is() {
    let runtime = Runtime::new().unwrap();
    let drive = backend("Zażółć gęślą");
    let letter = free_letter();
    let name = format!("Zażółć gęślą ({letter})");
    let (shown, hidden) = time_a_round(&runtime, &drive, letter, &name);
    assert!(shown <= LIMIT_MS, "shown after {shown} ms");
    assert!(hidden <= LIMIT_MS, "hidden after {hidden} ms");
}
