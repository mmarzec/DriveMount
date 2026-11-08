// SPDX-License-Identifier: GPL-3.0-or-later

//! WinFsp's own test suite against a mounted drive, in its external mode (spec §10.3).
//!
//! `winfsp-tests` is not part of the WinFsp installer, so these are `#[ignore]`d. To run them:
//!
//! ```text
//! powershell -File scripts/get-winfsp-tests.ps1
//! cargo test -p drivemount-winfsp --test winfsp_tests -- --ignored --test-threads=1
//! ```
//!
//! `WINFSP_TESTS` can name the program, instead of the place that script puts it. What is left out,
//! and why, is in `tests/winfsp-tests-exclusions.txt`.
//!
//! The suite stops at its first failure and leaves files behind, and its README warns that it can hang.
//! So each test runs by itself, in a folder of its own on the drive, and is killed if it takes too
//! long. All of them run, and the report names every failure.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drivemount_vfs::{Connector, RemoteFs};
use drivemount_winfsp::{Volume, VolumeConfig};
use support::{Probe, Sftp, TrustOnce, eventually, free_letter, unique_slug};
use tokio::runtime::Runtime;

/// The longest any one test may take. The longest that passes here takes a few seconds.
const TEST_LIMIT: Duration = Duration::from_secs(120);

fn program() -> PathBuf {
    if let Some(path) = std::env::var_os("WINFSP_TESTS") {
        return PathBuf::from(path);
    }
    workspace().join(r"target\winfsp-tests\winfsp-tests-x64.exe")
}

/// The suite needs WinFsp's DLL, which is not on the search path of programs in general.
fn path_with_winfsp() -> OsString {
    let output = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\WOW6432Node\WinFsp",
            "/v",
            "InstallDir",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|line| line.contains("InstallDir"))
        .expect("WinFsp is not installed");
    let install_dir = line.split("REG_SZ").nth(1).unwrap().trim();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths =
        std::iter::once(Path::new(install_dir).join("bin")).chain(std::env::split_paths(&path));
    std::env::join_paths(paths).unwrap()
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_owned()
}

/// `name  # reason`, one to a line, or `sftp: name  # reason` for one that only applies over SFTP. A
/// line without a reason is an error, as is a name the suite does not have, so that this list cannot
/// go stale without notice.
fn exclusions(sftp: bool) -> BTreeMap<String, String> {
    let text = fs::read_to_string(workspace().join(r"tests\winfsp-tests-exclusions.txt")).unwrap();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (name, reason) = line
                .split_once('#')
                .unwrap_or_else(|| panic!("no reason given: {line}"));
            (name.trim().to_owned(), reason.trim().to_owned())
        })
        .filter_map(|(name, reason)| match name.strip_prefix("sftp:") {
            Some(name) => sftp.then(|| (name.trim().to_owned(), reason)),
            None => Some((name, reason)),
        })
        .collect()
}

/// The suite's own name for every test, optional and long-running ones included.
fn all_tests() -> Vec<String> {
    let output = Command::new(program())
        .env("PATH", path_with_winfsp())
        .args(["--external", "--resilient", "--list", "+*"])
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "cannot run {}: {error}. See the top of this file",
                program().display()
            )
        });
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

enum Outcome {
    Passed,
    Failed(String),
    TimedOut,
}

/// Runs one test in `directory`.
fn run_one(name: &str, directory: &Path, share: &str) -> Outcome {
    fs::create_dir(directory).unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(program())
        .env("PATH", path_with_winfsp())
        .args([
            "--external",
            "--resilient",
            "--case-insensitive-cmp",
            &format!("--share-prefix={share}"),
        ])
        .arg(format!("+{name}"))
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(log.reopen().unwrap())
        .stderr(log.reopen().unwrap())
        .spawn()
        .unwrap();

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let text = fs::read(log.path()).unwrap();
            let text = String::from_utf8_lossy(&text).trim().to_owned();
            return if status.success() {
                Outcome::Passed
            } else {
                Outcome::Failed(text)
            };
        }
        if start.elapsed() > TEST_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Runs everything not excluded on a drive over `fs`, and fails with a list of what failed.
fn run_suite(fs: Arc<dyn RemoteFs>, runtime: &Runtime, what: &str, sftp: bool) {
    let excluded = exclusions(sftp);
    let tests = all_tests();
    assert!(!tests.is_empty(), "the suite lists no tests");
    for name in excluded.keys() {
        assert!(
            tests.contains(name),
            "excluded but not in the suite: {name}"
        );
    }

    let config = VolumeConfig {
        slug: unique_slug(),
        label: "winfsp-tests".to_owned(),
        file_system_name: "DM-SFTP".to_owned(),
        op_timeout: Duration::from_secs(30),
        cache_ttl: Duration::from_secs(5),
        threads: 0,
        read_only: false,
    };
    let share = format!(r"\drivemount\{}", config.slug);
    let volume = Volume::new(fs, config, runtime.handle().clone());
    let letter = free_letter();
    volume.start(letter).unwrap();
    let root = PathBuf::from(format!(r"{letter}\"));
    assert!(eventually(|| root.exists()));

    let mut passed = 0;
    let mut failures = Vec::new();
    let to_run: Vec<&String> = tests
        .iter()
        .filter(|name| !excluded.contains_key(*name))
        .collect();
    for (index, name) in to_run.iter().enumerate() {
        let directory = root.join(format!("t{index}"));
        let start = Instant::now();
        let outcome = run_one(name, &directory, &share);
        let took = start.elapsed();
        match outcome {
            Outcome::Passed => {
                passed += 1;
                println!("{what}: {name} ok, {took:?}");
            }
            Outcome::Failed(text) => {
                println!("{what}: {name} FAILED, {took:?}\n{text}");
                failures.push(format!("{name}: {text}"));
            }
            Outcome::TimedOut => {
                println!("{what}: {name} TIMED OUT");
                failures.push(format!("{name}: timed out"));
            }
        }
    }
    volume.stop();
    println!(
        "{what}: {passed} of {} passed, {} of the suite's {} excluded",
        to_run.len(),
        excluded.len(),
        tests.len()
    );
    assert!(
        failures.is_empty(),
        "{what}: {} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
#[ignore = "needs winfsp-tests: see the top of this file"]
fn winfsp_tests_pass_on_a_drive_over_memfs() {
    let runtime = Runtime::new().unwrap();
    run_suite(Arc::new(Probe::default()), &runtime, "MemFs", false);
}

#[test]
#[ignore = "needs winfsp-tests: see the top of this file"]
fn winfsp_tests_pass_on_a_drive_over_sftp() {
    let runtime = Runtime::new().unwrap();
    let probe = Arc::new(Probe::default());
    let (sftp, fs) = runtime.block_on(async {
        let sftp = Sftp::start(probe, Duration::from_secs(30)).await;
        let fs = sftp.connector.connect(&TrustOnce::default()).await.unwrap();
        (sftp, fs)
    });
    run_suite(fs, &runtime, "SFTP", true);
    drop(sftp);
}
