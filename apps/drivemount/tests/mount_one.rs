// SPDX-License-Identifier: GPL-3.0-or-later

//! `examples/mount_one.rs` from the outside, as a script uses it: M1-18's first criterion, what its
//! log holds (NFR-SEC-02, NFR-MAINT-04), and M1-19's auth methods, jump host and host keys (plan
//! §9.2). These need the S-03 server (`tests\docker\servers.ps1 up`) and WinFsp, and tell Explorer
//! about their drives. `the_windows_agent_mounts` also needs the `ssh-agent` service, which only an
//! administrator can turn on.
//!
//! `cargo test -p drivemount --test mount_one -- --ignored --test-threads=1`

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use drivemount_core::config::{Config, ConfigStore, LogLevel, Settings};
use drivemount_core::profile::{JumpAuth, JumpHost, Profile, Protocol, SftpAuth, SftpProfile};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha1::Sha1;
use tempfile::TempDir;

const PASSWORD: &str = "tester";
const PASSPHRASE: &str = "test-passphrase";
const SECRET_VARIABLE: &str = "DRIVEMOUNT_MOUNT_ONE_TEST_SECRET";
const ANSWERS_VARIABLE: &str = "DRIVEMOUNT_MOUNT_ONE_TEST_ANSWERS";
const TOTP_PASSWORD: &str = "totp-pass";
/// `JBSWY3DPEHPK3PXP`, decoded.
const TOTP_SECRET: &[u8] = b"Hello!\xde\xad\xbe\xef";

fn key(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/docker/keys")
        .join(name)
}

fn host() -> String {
    env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

/// The code `totpuser` and `keytotp` ask for now.
fn totp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<Sha1>::new_from_slice(TOTP_SECRET).unwrap();
    mac.update(&(seconds / 30).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 0x0f);
    let word: [u8; 4] = digest[offset..offset + 4].try_into().unwrap();
    format!(
        "{:06}",
        (u32::from_be_bytes(word) & 0x7fff_ffff) % 1_000_000
    )
}

fn profile(user: &str, auth: SftpAuth) -> Profile {
    let mut sftp = SftpProfile::new(host(), user, auth);
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

/// `examples/mount_one.rs`, built once for all the tests here.
///
/// Built with `--workspace`, which picks the same dependency features as `cargo test --workspace`, so
/// that after one the build finds everything fresh. `cargo run -p drivemount` picked fewer, rebuilt
/// the shared crates for them (one to two minutes, in the first test to ask), and left them for the
/// next `cargo test` to rebuild again.
fn example() -> &'static Path {
    static EXAMPLE: OnceLock<PathBuf> = OnceLock::new();
    EXAMPLE.get_or_init(|| {
        let output = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--workspace", "--example", "mount_one"])
            .arg("--message-format=json")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "building the example: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let messages = String::from_utf8(output.stdout).unwrap();
        let line = messages
            .lines()
            .find(|line| {
                line.contains(r#""kind":["example"]"#) && line.contains(r#""name":"mount_one""#)
            })
            .expect("cargo names the example it built");
        let (_, after) = line.split_once(r#""executable":""#).unwrap();
        let (path, _) = after.split_once('"').unwrap();
        PathBuf::from(path.replace(r"\\", r"\"))
    })
}

fn mount_one(folder: &TempDir, secret: Option<&str>, more: &[&str]) -> Output {
    mount_one_answering(folder, secret, &[], more)
}

/// With keyboard-interactive answers as well, which it uses in order.
fn mount_one_answering(
    folder: &TempDir,
    secret: Option<&str>,
    answers: &[&str],
    more: &[&str],
) -> Output {
    let mut command = Command::new(example());
    command
        .arg("--config")
        .arg(folder.path().join("config.toml"))
        .args(["--host-key", "once"])
        .args(more);
    if let Some(secret) = secret {
        command
            .args(["--secret-env", SECRET_VARIABLE])
            .env(SECRET_VARIABLE, secret);
    }
    if !answers.is_empty() {
        command
            .args(["--answers-env", ANSWERS_VARIABLE])
            .env(ANSWERS_VARIABLE, answers.join(","));
    }
    command.output().unwrap()
}

/// What `ssh.exe -o BatchMode=yes` makes of the server with only this `known_hosts`. It is not let
/// log in, so a host it trusts ends in "Permission denied", and one it does not before that.
fn real_ssh_with(known_hosts: &Path, folder: &TempDir) -> String {
    let empty = folder.path().join("empty");
    fs::write(&empty, "").unwrap();
    let output = Command::new(r"C:\Windows\System32\OpenSSH\ssh.exe")
        .arg("-F")
        .arg(&empty)
        .args(["-p", "2222", "-o", "BatchMode=yes"])
        .args(["-o", "StrictHostKeyChecking=yes"])
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-o")
        .arg(format!("GlobalKnownHostsFile={}", empty.display()))
        .args(["-o", "PreferredAuthentications=hostbased"])
        .arg(format!("tester@{}", host()))
        .arg("exit")
        .output()
        .unwrap();
    text(&output)
}

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn log_of(folder: &TempDir) -> String {
    fs::read_to_string(folder.path().join("logs").join("drivemount.log")).unwrap()
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

/// Takes the test key out of the agent again, however the test ends.
struct InTheAgent;

impl Drop for InTheAgent {
    fn drop(&mut self) {
        let _ = Command::new(SSH_ADD)
            .arg("-d")
            .arg(key("id_ed25519.pub"))
            .output();
    }
}

const SSH_ADD: &str = r"C:\Windows\System32\OpenSSH\ssh-add.exe";

/// FR-SSH-04 against the real Windows agent, which an administrator has to have started
/// (`Start-Service ssh-agent`). The key goes in through standard input, as `ssh-add` refuses a key
/// file anyone else can read, and the one in the repository is.
#[test]
#[ignore = "needs the S-03 server, WinFsp and the Windows ssh-agent service running"]
fn the_windows_agent_mounts() {
    let added = Command::new(SSH_ADD)
        .arg("-")
        .stdin(fs::File::open(key("id_ed25519")).unwrap())
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", text(&added));
    let _remove = InTheAgent;

    let folder = folder_with(profile("keyonly", SftpAuth::Agent));
    // No secret is given, so had it asked for one, the mount would have failed.
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

/// Plan §9.2: an unknown key takes the prompt path, and *Trust and save* writes a line that real
/// OpenSSH accepts with `BatchMode=yes`.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn an_unknown_host_key_is_refused_unless_the_script_says_and_a_saved_one_satisfies_ssh() {
    let folder = folder_with(profile("tester", SftpAuth::Password { remember: false }));
    let known_hosts = folder.path().join("known_hosts");

    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "reject"]);
    let said = text(&output);
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(said.contains("unknown host key"), "{said}");
    assert!(said.contains("server key not trusted"), "{said}");
    // The control: with nothing saved, ssh refuses the host.
    fs::write(&known_hosts, "").unwrap();
    let before = real_ssh_with(&known_hosts, &folder);
    assert!(before.contains("Host key verification failed"), "{before}");

    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "save"]);
    assert_it_all_worked(&output);
    let saved = fs::read_to_string(&known_hosts).unwrap();
    assert!(
        saved.contains(&format!("[{}]:2222", host())) || saved.contains("|1|"),
        "{saved}"
    );
    let after = real_ssh_with(&known_hosts, &folder);
    assert!(!after.contains("Host key verification failed"), "{after}");
    assert!(after.contains("Permission denied"), "{after}");

    // Saved, so there is no question the next time, whatever the answer would have been.
    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "reject"]);
    assert_it_all_worked(&output);
    assert!(
        !text(&output).contains("unknown host key"),
        "{}",
        text(&output)
    );
}

/// FR-SSH-10: a key other than the saved one is refused with no question, and the file is kept.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_changed_host_key_is_refused_and_not_asked_about() {
    let folder = folder_with(profile("tester", SftpAuth::Password { remember: false }));
    let known_hosts = folder.path().join("known_hosts");
    // A real Ed25519 key, but not the server's.
    let other = fs::read_to_string(key("id_ed25519.pub")).unwrap();
    let mut parts = other.split_whitespace();
    let line = format!(
        "[{}]:2222 {} {}\n",
        host(),
        parts.next().unwrap(),
        parts.next().unwrap()
    );
    fs::write(&known_hosts, &line).unwrap();

    let output = mount_one(&folder, Some(PASSWORD), &["--host-key", "save"]);

    let said = text(&output);
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(said.contains("server key has changed"), "{said}");
    assert!(!said.contains("unknown host key"), "{said}");
    assert_eq!(fs::read_to_string(&known_hosts).unwrap(), line);
}

/// FR-SSH-05: the password answers the password question, and the script gives the code.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn keyboard_interactive_with_a_one_time_code_mounts() {
    let folder = folder_with(profile("totpuser", SftpAuth::Password { remember: false }));
    let code = totp();
    let output = mount_one_answering(&folder, Some(TOTP_PASSWORD), &[&code], &[]);
    assert_it_all_worked(&output);
}

#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn keyboard_interactive_then_a_password_mounts() {
    let folder = folder_with(profile("kbdpass", SftpAuth::Password { remember: false }));
    assert_it_all_worked(&mount_one(&folder, Some(TOTP_PASSWORD), &[]));
}

/// A key, then keyboard-interactive questions that nothing stored can answer.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_key_then_a_password_and_a_code_mounts() {
    let auth = SftpAuth::KeyFile {
        path: key("id_ed25519"),
        remember: false,
    };
    let folder = folder_with(profile("keytotp", auth));
    let code = totp();
    let output = mount_one_answering(&folder, None, &[TOTP_PASSWORD, &code], &[]);
    assert_it_all_worked(&output);
}

/// FR-SSH-04: an OpenSSH certificate beside its key.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_certificate_mounts() {
    let auth = SftpAuth::KeyFile {
        path: key("id_cert"),
        remember: false,
    };
    let folder = folder_with(profile("certuser", auth));
    assert_it_all_worked(&mount_one(&folder, None, &[]));
}

/// FR-SSH-02: RSA in the old PEM form, with a passphrase.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn an_rsa_pem_key_with_a_passphrase_mounts() {
    let auth = SftpAuth::KeyFile {
        path: key("id_rsa_pem_pass"),
        remember: false,
    };
    let folder = folder_with(profile("keyonly", auth));
    assert_it_all_worked(&mount_one(&folder, Some(PASSPHRASE), &[]));
}

/// FR-SSH-11: `sshd-target` resolves only on the Docker network, so any drive at all is the tunnel.
#[test]
#[ignore = "needs the S-03 server and WinFsp"]
fn a_profile_behind_a_jump_host_mounts() {
    let auth = SftpAuth::Password { remember: false };
    let mut sftp = SftpProfile::new("sshd-target", "tester", auth);
    sftp.jump = Some(JumpHost {
        host: host(),
        port: 2228,
        username: None,
        auth: JumpAuth::Password,
    });
    let folder = folder_with(Profile::new("Behind a jump host", Protocol::Sftp(sftp)));

    let output = mount_one(&folder, Some(PASSWORD), &[]);

    let said = text(&output);
    assert_it_all_worked(&output);
    // Both keys were met: the jump host's and the target's.
    assert_eq!(said.matches("unknown host key").count(), 2, "{said}");
}

/// Needs neither, as it stops at its arguments.
#[test]
fn wrong_arguments_are_exit_code_2() {
    let folder = TempDir::new().unwrap();
    let output = mount_one(&folder, None, &["--no-such-option", "x"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
}
