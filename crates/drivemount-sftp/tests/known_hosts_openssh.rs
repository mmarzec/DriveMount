// SPDX-License-Identifier: GPL-3.0-or-later

//! `known_hosts` compared with OpenSSH itself: one file, read by both.
//!
//! The `ssh-keygen` tests run everywhere and fail if it is not on `PATH`, so they cannot go quiet.
//! The `ssh` tests also need the S-03 server and are ignored by default:
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p drivemount-sftp --test known_hosts_openssh -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{
    ED, ED_FP, ED2, ED2_FP, P256, P256_FP, P384, P384_FP, P521, P521_FP, RSA, RSA_FP, fixture_path,
    host_key,
};
use drivemount_sftp::known_hosts::{HostKeyStatus, KnownHosts, append_if_unknown};

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{program} must be on PATH for these tests: {error}"))
}

fn path_arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// What `ssh-keygen -F` finds: the lines of plain entries and of `@revoked` entries.
fn ssh_keygen_find(name: &str, file: &Path) -> (Vec<u32>, Vec<u32>) {
    let output = run("ssh-keygen", &["-F", name, "-f", path_arg(file)]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (mut plain, mut revoked) = (Vec::new(), Vec::new());
    for found in stdout
        .lines()
        .filter_map(|line| line.split_once("found: line "))
    {
        let mut words = found.1.split_whitespace();
        let line = words.next().unwrap().parse().unwrap();
        match words.next() {
            None => plain.push(line),
            Some("REVOKED") => revoked.push(line),
            Some(other) => assert_eq!(other, "CA", "unexpected ssh-keygen output: {stdout}"),
        }
    }
    (plain, revoked)
}

#[test]
fn fingerprints_equal_ssh_keygen_for_fresh_keys_of_every_type() {
    let dir = tempfile::tempdir().unwrap();
    for (name, args) in [
        ("ed25519", &["-t", "ed25519"][..]),
        ("rsa", &["-t", "rsa", "-b", "2048"]),
        ("p256", &["-t", "ecdsa", "-b", "256"]),
        ("p384", &["-t", "ecdsa", "-b", "384"]),
        ("p521", &["-t", "ecdsa", "-b", "521"]),
    ] {
        let private = dir.path().join(name);
        let mut all = vec!["-q", "-N", "", "-C", "", "-f", path_arg(&private)];
        all.extend_from_slice(args);
        let created = run("ssh-keygen", &all);
        assert!(created.status.success(), "{created:?}");

        let public = private.with_extension("pub");
        let printed = run(
            "ssh-keygen",
            &["-l", "-E", "sha256", "-f", path_arg(&public)],
        );
        let printed = String::from_utf8(printed.stdout).unwrap();
        let want = printed.split_whitespace().nth(1).unwrap();

        let key = fs::read_to_string(&public).unwrap();
        assert_eq!(host_key(&key).fingerprint(), want, "{name}");
    }
}

#[test]
fn the_recorded_fingerprints_equal_ssh_keygen() {
    let dir = tempfile::tempdir().unwrap();
    for (key, want) in [
        (ED, ED_FP),
        (ED2, ED2_FP),
        (RSA, RSA_FP),
        (P256, P256_FP),
        (P384, P384_FP),
        (P521, P521_FP),
    ] {
        let public = dir.path().join("key.pub");
        fs::write(&public, format!("{key}\n")).unwrap();
        let printed = run(
            "ssh-keygen",
            &["-l", "-E", "sha256", "-f", path_arg(&public)],
        );
        let printed = String::from_utf8(printed.stdout).unwrap();
        assert_eq!(printed.split_whitespace().nth(1).unwrap(), want);
    }
}

#[test]
fn the_fixture_reads_the_same_as_ssh_keygen() {
    let path = fixture_path();
    let file = KnownHosts::load(&path).unwrap();
    for (host, port) in [
        ("plain.example.test", 22),
        ("PLAIN.example.test", 22),
        ("upper.example.test", 22),
        ("multi.example.test", 22),
        ("port.example.test", 2200),
        ("port.example.test", 22),
        ("port.example.test", 2201),
        ("hashed.example.test", 22),
        ("hashed-port.example.test", 2200),
        ("hashed-port.example.test", 22),
        ("one.example.test", 22),
        ("two.example.test", 22),
        ("192.0.2.7", 22),
        ("a.wild.example.test", 22),
        ("no.wild.example.test", 22),
        ("wild.example.test", 22),
        ("host.example.test", 22),
        ("hot.example.test", 22),
        ("indented.example.test", 22),
        ("revoked.example.test", 22),
        ("hashed-revoked.example.test", 22),
        ("never.example.test", 22),
        ("x.ca.example.test", 22),
        ("marker.example.test", 22),
        ("commented.example.test", 22),
    ] {
        // Lines with a bad key are not in this table: ssh-keygen -F prints a line without reading
        // its key. The real ssh test below covers them. It also takes the name as given, while ssh
        // lowers it first.
        let lowered = host.to_ascii_lowercase();
        let name = if port == 22 {
            lowered
        } else {
            format!("[{lowered}]:{port}")
        };
        let (plain, revoked) = ssh_keygen_find(&name, &path);

        // ED2 is on no line, so the answer shows which plain lines this module found for the name.
        let want = plain.last().copied();
        let got = match file.check(host, port, &host_key(ED2)) {
            HostKeyStatus::Changed { line, .. } => Some(line),
            HostKeyStatus::Unknown => None,
            other => panic!("{name}: {other:?}"),
        };
        // A plain line also serves other ports, which ssh-keygen -F does not model. No host in this
        // table has one, so the two must agree.
        assert_eq!(got, want, "{name}: ssh-keygen found {plain:?}");

        // P521 is on the `@revoked *` line: every host must see that one as ssh-keygen does.
        assert!(revoked.contains(&19), "{name}: {revoked:?}");
        assert_eq!(
            file.check(host, port, &host_key(P521)),
            HostKeyStatus::Revoked { line: 19 },
            "{name}"
        );
    }
}

#[test]
fn ssh_keygen_finds_what_was_appended() {
    let dir = tempfile::tempdir().unwrap();
    let key = host_key(ED);
    for (name, start, hashed) in [
        ("plain", String::new(), false),
        ("existing", format!("old.test {ED2}"), false),
        (
            "hashed",
            fs::read_to_string(fixture_path())
                .unwrap()
                .lines()
                .nth(9)
                .unwrap()
                .to_owned(),
            true,
        ),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, &start).unwrap();
        append_if_unknown(&path, "New.Test", 22, &key).unwrap();
        append_if_unknown(&path, "port.test", 2200, &key).unwrap();

        let last = u32::try_from(fs::read_to_string(&path).unwrap().lines().count()).unwrap();
        assert_eq!(ssh_keygen_find("new.test", &path).0, [last - 1], "{name}");
        assert_eq!(
            ssh_keygen_find("[port.test]:2200", &path).0,
            [last],
            "{name}"
        );
        assert_eq!(ssh_keygen_find("other.test", &path).0, [0u32; 0], "{name}");

        let output = run("ssh-keygen", &["-F", "new.test", "-f", path_arg(&path)]);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(ED), "{stdout}");
        assert_eq!(stdout.contains("|1|"), hashed, "{stdout}");
    }
}

#[test]
fn a_file_hashed_by_ssh_keygen_is_read_and_followed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let key = host_key(ED);
    append_if_unknown(&path, "first.test", 22, &key).unwrap();
    append_if_unknown(&path, "second.test", 2200, &key).unwrap();

    let hashed = run("ssh-keygen", &["-q", "-H", "-f", path_arg(&path)]);
    assert!(hashed.status.success(), "{hashed:?}");

    let file = KnownHosts::load(&path).unwrap();
    assert!(file.uses_hashing());
    assert_eq!(file.check("first.test", 22, &key), HostKeyStatus::Known);
    assert_eq!(file.check("second.test", 2200, &key), HostKeyStatus::Known);

    append_if_unknown(&path, "third.test", 22, &key).unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains("third.test"));
    assert_eq!(ssh_keygen_find("third.test", &path).0, [3]);
}

// ---- Real ssh against the S-03 server. ----

const SERVER_PORT: u16 = 2222;
const SERVER_PORT_ARG: &str = "2222";

fn server_key() -> String {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "../../tests/docker/keys/ssh_host_ed25519_key.pub",
    ]
    .iter()
    .collect();
    let line = fs::read_to_string(path).expect("run tests/docker/servers.ps1 up first");
    let mut words = line.split_whitespace();
    format!("{} {}", words.next().unwrap(), words.next().unwrap())
}

/// What ssh decided about the server's key, from its message. Offering no credentials means that
/// passing the host-key check ends in "Permission denied", with nothing sent to log in.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Unknown,
    Changed(u32),
    Revoked,
}

fn ssh_verdict_with(host: &str, file: &Path, extra: &[&str]) -> Verdict {
    let known_hosts = format!("UserKnownHostsFile={}", path_arg(file));
    let target = format!("nobody@{host}");
    let mut args = vec![
        "-F",
        "none",
        "-p",
        SERVER_PORT_ARG,
        "-o",
        "BatchMode=yes",
        "-o",
        "PreferredAuthentications=none",
        "-o",
        "GlobalKnownHostsFile=none",
        "-o",
        "CheckHostIP=no",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "HostKeyAlgorithms=ssh-ed25519",
        "-o",
        &known_hosts,
    ];
    args.extend_from_slice(extra);
    args.push(&target);
    let output = run("ssh", &args);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if stderr.contains("Permission denied") {
        return Verdict::Accepted;
    }
    if stderr.contains("REVOKED HOST KEY") {
        return Verdict::Revoked;
    }
    for line in stderr.lines() {
        if line.starts_with("Offending") {
            let number = line.trim_end().rsplit(':').next().unwrap();
            return Verdict::Changed(number.parse().unwrap());
        }
    }
    assert!(
        stderr.contains("host key is known"),
        "cannot read ssh's answer; is the server up? {stderr}"
    );
    Verdict::Unknown
}

fn ssh_verdict(host: &str, file: &Path) -> Verdict {
    ssh_verdict_with(host, file, &["-o", "StrictHostKeyChecking=yes"])
}

fn verdict_of(status: &HostKeyStatus) -> Verdict {
    match status {
        HostKeyStatus::Known => Verdict::Accepted,
        HostKeyStatus::Unknown => Verdict::Unknown,
        HostKeyStatus::Changed { line, .. } => Verdict::Changed(*line),
        HostKeyStatus::Revoked { .. } => Verdict::Revoked,
    }
}

#[test]
#[ignore = "needs the S-03 server: tests/docker/servers.ps1 up"]
fn ssh_and_this_module_agree_on_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let server = server_key();
    let key = host_key(&server);
    let h = "[localhost]:2222";

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("exact", format!("{h} {server}\n").into_bytes()),
        ("empty", Vec::new()),
        ("other key", format!("# note\n{h} {ED}\n").into_bytes()),
        ("other type", format!("{h} {RSA}\n").into_bytes()),
        ("other ecdsa", format!("{h} {P384}\n").into_bytes()),
        ("two others", format!("{h} {ED}\n{h} {RSA}\n").into_bytes()),
        (
            "found among others",
            format!("{h} {ED}\n{h} {RSA}\n{h} {server}\n").into_bytes(),
        ),
        (
            "revoked",
            format!("{h} {server}\n@revoked {h} {server}\n").into_bytes(),
        ),
        (
            "revoked by star",
            format!("@revoked * {server}\n").into_bytes(),
        ),
        (
            "revoked alone",
            format!("@revoked {h} {server}\n").into_bytes(),
        ),
        (
            "revoked other host",
            format!("{h} {server}\n@revoked other.test {server}\n").into_bytes(),
        ),
        (
            "revoked other key",
            format!("{h} {server}\n@revoked {h} {ED}\n").into_bytes(),
        ),
        (
            "only another key revoked",
            format!("@revoked {h} {ED}\n").into_bytes(),
        ),
        (
            "certificate authority",
            format!("@cert-authority {h} {server}\n").into_bytes(),
        ),
        (
            "other certificate authority",
            format!("@cert-authority {h} {ED}\n").into_bytes(),
        ),
        (
            "pattern star",
            format!("[local*]:2222 {server}\n").into_bytes(),
        ),
        (
            "pattern question",
            format!("[localhos?]:2222 {server}\n").into_bytes(),
        ),
        ("bare star", format!("* {server}\n").into_bytes()),
        (
            "negated",
            format!("![localhost]:2222,[local*]:2222 {server}\n").into_bytes(),
        ),
        (
            "negated after",
            format!("[local*]:2222,![localhost]:2222 {server}\n").into_bytes(),
        ),
        ("negated star", format!("!*,{h} {server}\n").into_bytes()),
        (
            "negated other",
            format!("!other.test {server}\n").into_bytes(),
        ),
        ("list", format!("a.test,{h},b.test {server}\n").into_bytes()),
        (
            "upper case",
            format!("[LOCALHOST]:2222 {server}\n").into_bytes(),
        ),
        ("plain name", format!("localhost {server}\n").into_bytes()),
        (
            "plain name upper case",
            format!("LOCALHOST {server}\n").into_bytes(),
        ),
        (
            "plain name other key",
            format!("localhost {ED}\n").into_bytes(),
        ),
        (
            "plain name other host",
            format!("127.0.0.1 {server}\n").into_bytes(),
        ),
        (
            "other port",
            format!("[localhost]:2223 {server}\n").into_bytes(),
        ),
        (
            "port beats plain",
            format!("{h} {ED}\nlocalhost {server}\n").into_bytes(),
        ),
        (
            "unknown marker",
            format!("@other {h} {server}\n").into_bytes(),
        ),
        ("commented out", format!("#{h} {server}\n").into_bytes()),
        (
            "type mismatch",
            format!("{h} ssh-rsa {}\n", server.split(' ').nth(1).unwrap()).into_bytes(),
        ),
        (
            "bad base64",
            format!(
                "{h} ssh-ed25519 !!!!
"
            )
            .into_bytes(),
        ),
        (
            "no key",
            format!(
                "{h} ssh-ed25519
{h}
"
            )
            .into_bytes(),
        ),
        (
            "old rsa1 line",
            format!(
                "{h} 2048 35 12345678901234567890
"
            )
            .into_bytes(),
        ),
        ("tab", format!("{h}\t{server}\n").into_bytes()),
        ("indented", format!("   {h} {server}\n").into_bytes()),
        (
            "crlf",
            format!("{h} {server}\r\n\r\nrubbish\r\n").into_bytes(),
        ),
        ("no final newline", format!("{h} {server}").into_bytes()),
    ];
    let mut cases = cases;
    if cfg!(windows) {
        // Only the Windows build of OpenSSH skips the mark.
        cases.push((
            "byte order mark",
            format!("\u{FEFF}{h} {server}\n").into_bytes(),
        ));
    }

    for (name, contents) in &cases {
        fs::write(&path, contents).unwrap();
        let ours = KnownHosts::load(&path)
            .unwrap()
            .check("localhost", SERVER_PORT, &key);
        assert_eq!(verdict_of(&ours), ssh_verdict("localhost", &path), "{name}");
    }
}

/// OpenSSH warns about this key but still treats it as unknown; both refuse it without a prompt.
#[test]
#[ignore = "needs the S-03 server: tests/docker/servers.ps1 up"]
fn a_key_revoked_under_the_plain_name_is_refused_by_both() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let server = server_key();
    fs::write(&path, format!("@revoked localhost {server}\n")).unwrap();

    assert_eq!(ssh_verdict("localhost", &path), Verdict::Revoked);
    assert_eq!(
        KnownHosts::load(&path)
            .unwrap()
            .check("localhost", SERVER_PORT, &host_key(&server)),
        HostKeyStatus::Revoked { line: 1 }
    );
}

/// FR-SSH-09: after *Trust and save*, `ssh.exe` accepts the server without asking.
#[test]
#[ignore = "needs the S-03 server: tests/docker/servers.ps1 up"]
fn ssh_accepts_what_was_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("known_hosts");
    let key = host_key(&server_key());

    assert_eq!(
        append_if_unknown(&path, "localhost", SERVER_PORT, &key).unwrap(),
        HostKeyStatus::Unknown
    );
    assert_eq!(ssh_verdict("localhost", &path), Verdict::Accepted);
}

/// FR-SSH-08/09 the other way: what `ssh` saved is known here, and hashing follows its style.
#[test]
#[ignore = "needs the S-03 server: tests/docker/servers.ps1 up"]
fn entries_ssh_wrote_are_known_and_set_the_style() {
    let dir = tempfile::tempdir().unwrap();
    let key = host_key(&server_key());
    for (name, hash) in [("plain", "no"), ("hashed", "yes")] {
        let path = dir.path().join(name);
        let verdict = ssh_verdict_with(
            "localhost",
            &path,
            &[
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                &format!("HashKnownHosts={hash}"),
            ],
        );
        assert_eq!(verdict, Verdict::Accepted, "{name}");

        let file = KnownHosts::load(&path).unwrap();
        assert_eq!(file.uses_hashing(), hash == "yes", "{name}");
        assert_eq!(
            file.check("localhost", SERVER_PORT, &key),
            HostKeyStatus::Known,
            "{name}"
        );

        // Save the same server under its address, in a file ssh started, and let ssh read it back.
        append_if_unknown(&path, "127.0.0.1", SERVER_PORT, &key).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.contains("127.0.0.1"), hash == "no", "{name}: {text}");
        assert_eq!(ssh_verdict("127.0.0.1", &path), Verdict::Accepted, "{name}");
    }
}
