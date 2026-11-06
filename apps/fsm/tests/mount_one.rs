// SPDX-License-Identifier: GPL-3.0-or-later

//! `examples/mount_one.rs` from the outside, as a script uses it: M1-18's first criterion, and what
//! its log holds (NFR-SEC-02, NFR-MAINT-04). These need the S-03 server (`tests\docker\servers.ps1 up`)
//! and WinFsp, and tell Explorer about their drives.
//!
//! `cargo test -p fsm --test mount_one -- --ignored --test-threads=1`

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fsm_core::config::{Config, ConfigStore, LogLevel, Settings};
use fsm_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use tempfile::TempDir;

const PASSWORD: &str = "tester";
const PASSPHRASE: &str = "test-passphrase";
const SECRET_VARIABLE: &str = "FSM_MOUNT_ONE_TEST_SECRET";

fn key(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/docker/keys")
        .join(name)
}

fn profile(user: &str, auth: SftpAuth) -> Profile {
    let host = env::var("FSM_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned());
    let mut sftp = SftpProfile::new(host, user, auth);
    sftp.port = 2222;
    Profile::new("Zażółć mount_one", Protocol::Sftp(sftp))
}

/// A folder holding a configuration of this one profile, logging everything it can.
fn folder_with(profile: Profile) -> TempDir {
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(folder.path().join("config.toml"));
    store
        .save(&Config {
            settings: Settings {
                log_level: LogLevel::Debug,
                ..Settings::default()
            },
            profiles: vec![profile],
            unreadable: Vec::new(),
        })
        .unwrap();
    folder
}

fn mount_one(folder: &TempDir, secret: Option<&str>, more: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "run",
            "--quiet",
            "-p",
            "fsm",
            "--example",
            "mount_one",
            "--",
        ])
        .arg("--config")
        .arg(folder.path().join("config.toml"))
        .args(["--host-key", "once"])
        .args(more);
    if let Some(secret) = secret {
        command
            .args(["--secret-env", SECRET_VARIABLE])
            .env(SECRET_VARIABLE, secret);
    }
    command.output().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn log_of(folder: &TempDir) -> String {
    fs::read_to_string(folder.path().join("logs").join("fsm.log")).unwrap()
}

fn assert_it_all_worked(output: &Output) {
    let said = text(output);
    assert!(output.status.success(), "{said}");
    assert!(
        said.contains("wrote, read back, found and removed"),
        "{said}"
    );
    assert!(said.contains("is free again"), "{said}");
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_password_mounts_checks_and_unmounts_in_a_span_of_its_own() {
    let folder = folder_with(profile("tester", SftpAuth::Password { remember: false }));

    let output = mount_one(&folder, Some(PASSWORD), &[]);

    assert_it_all_worked(&output);
    let log = log_of(&folder);
    // NFR-MAINT-04.
    assert!(
        log.contains("mount{profile_id=") && log.contains("name=Zażółć mount_one}"),
        "{log}"
    );
}

/// NFR-SEC-02, with a secret nothing else could explain: the password above is also a user name.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_key_with_a_passphrase_mounts_and_the_passphrase_stays_out_of_the_log() {
    let auth = SftpAuth::KeyFile {
        path: key("id_ed25519_pass"),
        remember: false,
    };
    let folder = folder_with(profile("keyonly", auth));

    let output = mount_one(&folder, Some(PASSPHRASE), &[]);

    assert_it_all_worked(&output);
    let log = log_of(&folder);
    assert!(!log.contains(PASSPHRASE), "NFR-SEC-02: {log}");
    assert!(!text(&output).contains(PASSPHRASE), "NFR-SEC-02");
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_key_with_no_passphrase_mounts() {
    let auth = SftpAuth::KeyFile {
        path: key("id_ed25519"),
        remember: false,
    };
    let folder = folder_with(profile("keyonly", auth));
    assert_it_all_worked(&mount_one(&folder, None, &[]));
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn ask_each_time_mounts_with_what_the_script_answers() {
    let folder = folder_with(profile("tester", SftpAuth::Ask));
    assert_it_all_worked(&mount_one(&folder, Some(PASSWORD), &[]));

    let key = key("id_ed25519");
    let key = key.to_str().unwrap();
    assert_it_all_worked(&mount_one(&folder, None, &["--login-key", key]));
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_read_only_profile_refuses_the_write() {
    let mut saved = profile("tester", SftpAuth::Password { remember: false });
    saved.read_only = true;
    let folder = folder_with(saved);

    let output = mount_one(&folder, Some(PASSWORD), &[]);

    let said = text(&output);
    assert!(output.status.success(), "{said}");
    assert!(said.contains("refused, as it should be"), "{said}");
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_wrong_password_is_exit_code_1_and_says_why() {
    let folder = folder_with(profile("tester", SftpAuth::Password { remember: false }));

    let output = mount_one(&folder, Some("not the password"), &[]);

    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("the mount failed"),
        "{}",
        text(&output)
    );
    assert!(!log_of(&folder).contains("not the password"), "NFR-SEC-02");
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn an_unknown_host_key_is_refused_unless_the_script_says() {
    let folder = folder_with(profile("tester", SftpAuth::Password { remember: false }));

    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "reject"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));

    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "save"]);
    assert_it_all_worked(&output);
    let saved = fs::read_to_string(folder.path().join("known_hosts")).unwrap();
    assert!(
        saved.contains("[localhost]:2222") || saved.contains("|1|"),
        "{saved}"
    );
}

/// Needs neither, as it stops at its arguments.
#[test]
fn wrong_arguments_are_exit_code_2() {
    let folder = TempDir::new().unwrap();
    let output = mount_one(&folder, None, &["--no-such-option", "x"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
}
