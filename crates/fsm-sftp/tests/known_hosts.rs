// SPDX-License-Identifier: GPL-3.0-or-later

//! `known_hosts` against a fixture file and in temporary files. Comparisons with OpenSSH itself are
//! in `known_hosts_openssh.rs`.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::sync::Barrier;
use std::thread;

use common::{
    ED, ED_FP, ED2, ED2_FP, P256, P256_FP, P384, P384_FP, P521, P521_FP, RSA, RSA_FP, fixture,
    fixture_path, host_key,
};
use fsm_sftp::known_hosts::{
    HostKey, HostKeyStatus, KnownHosts, KnownHostsError, append_if_unknown,
};
use proptest::prelude::*;

fn known_hosts() -> KnownHosts {
    KnownHosts::parse(&fixture())
}

fn changed(line: u32, old_fingerprint: &str) -> HostKeyStatus {
    HostKeyStatus::Changed {
        line,
        old_fingerprint: old_fingerprint.to_owned(),
    }
}

/// Checks a table of (host, port, key, expected status) against a parsed file.
fn assert_statuses(file: &KnownHosts, cases: &[(&str, u16, &str, HostKeyStatus)]) {
    for (host, port, key, want) in cases {
        assert_eq!(
            file.check(host, *port, &host_key(key)),
            *want,
            "{host}:{port} with {key}"
        );
    }
}

fn fixture_cases() -> Vec<(&'static str, u16, &'static str, HostKeyStatus)> {
    use HostKeyStatus::{Known, Revoked, Unknown};
    vec![
        // Plain names, in either case on either side.
        ("plain.example.test", 22, ED, Known),
        ("PLAIN.example.TEST", 22, ED, Known),
        ("upper.example.test", 22, ED, Known),
        ("plain.example.test", 22, ED2, changed(4, ED_FP)),
        // Several keys for one host: any of them is good, and the last offender is reported.
        ("multi.example.test", 22, ED, Known),
        ("multi.example.test", 22, RSA, Known),
        ("multi.example.test", 22, ED2, changed(7, RSA_FP)),
        // A key of another type is still a different key, as in ssh.
        ("plain.example.test", 22, RSA, changed(4, ED_FP)),
        ("plain.example.test", 22, P384, changed(4, ED_FP)),
        ("one.example.test", 22, P384, changed(12, P256_FP)),
        // [host]:port is that port only.
        ("port.example.test", 2200, ED, Known),
        ("port.example.test", 2200, ED2, changed(8, ED_FP)),
        ("port.example.test", 22, ED, Unknown),
        ("port.example.test", 2201, ED, Unknown),
        // A plain line also serves other ports, but only for its exact key.
        ("fallback.example.test", 2200, ED, Known),
        ("fallback.example.test", 2200, ED2, Unknown),
        // Hashed names.
        ("hashed.example.test", 22, ED, Known),
        ("HASHED.example.test", 22, ED, Known),
        ("hashed.example.test", 22, ED2, changed(10, ED_FP)),
        ("hashed.example.test", 2200, ED, Known),
        ("hashed.example.test", 2200, ED2, Unknown),
        ("hashed-port.example.test", 2200, ED, Known),
        ("hashed-port.example.test", 22, ED, Unknown),
        ("nothashed.example.test", 22, ED, Unknown),
        // Lists and patterns.
        ("one.example.test", 22, P256, Known),
        ("two.example.test", 22, P256, Known),
        ("192.0.2.7", 22, P256, Known),
        ("a.wild.example.test", 22, ED, Known),
        ("wild.example.test", 22, ED, Unknown),
        ("no.wild.example.test", 22, ED, Unknown),
        ("host.example.test", 22, ED, Known),
        ("hoot.example.test", 22, ED, Known),
        ("hot.example.test", 22, ED, Unknown),
        // Odd spacing.
        ("indented.example.test", 22, ED, Known),
        // @revoked beats a good line, and does not itself count as another key.
        ("revoked.example.test", 22, ED, Revoked { line: 16 }),
        ("revoked.example.test", 22, ED2, changed(15, ED_FP)),
        ("hashed-revoked.example.test", 22, ED, Revoked { line: 18 }),
        ("never.example.test", 22, P521, Revoked { line: 19 }),
        ("never.example.test", 2200, P521, Revoked { line: 19 }),
        ("plain.example.test", 22, P521, Revoked { line: 19 }),
        ("revoked.example.test", 2200, ED, Revoked { line: 16 }),
        // Nothing relevant.
        ("never.example.test", 22, ED, Unknown),
        ("never.example.test", 2200, ED, Unknown),
        // Lines that must have no effect.
        ("x.ca.example.test", 22, ED, Unknown),
        ("x.ca.example.test", 22, ED2, Unknown),
        ("marker.example.test", 22, ED, Unknown),
        ("commented.example.test", 22, ED, Unknown),
        ("#commented.example.test", 22, ED, Unknown),
        ("typemismatch.example.test", 22, ED, Unknown),
        ("typemismatch.example.test", 22, RSA, Unknown),
        ("badbase64.example.test", 22, ED, Unknown),
        ("nokey.example.test", 22, ED, Unknown),
        ("rsa1.example.test", 22, ED, Unknown),
    ]
}

#[test]
fn fingerprints_match_ssh_keygen() {
    // Each value is what `ssh-keygen -l -E sha256` printed for the key.
    for (key, want) in [
        (ED, ED_FP),
        (ED2, ED2_FP),
        (RSA, RSA_FP),
        (P256, P256_FP),
        (P384, P384_FP),
        (P521, P521_FP),
    ] {
        assert_eq!(host_key(key).fingerprint(), want);
    }
}

#[test]
fn key_types_are_read_from_the_key() {
    assert_eq!(host_key(ED).key_type(), "ssh-ed25519");
    assert_eq!(host_key(RSA).key_type(), "ssh-rsa");
    assert_eq!(host_key(P521).key_type(), "ecdsa-sha2-nistp521");
}

#[test]
fn the_fixture_gives_the_expected_answers() {
    assert_statuses(&known_hosts(), &fixture_cases());
}

#[test]
fn line_endings_and_a_byte_order_mark_change_nothing() {
    let lf = fixture();
    let text = String::from_utf8(lf.clone()).unwrap();
    let mut with_bom = b"\xEF\xBB\xBF".to_vec();
    with_bom.extend_from_slice(&lf);
    for contents in [
        text.replace('\n', "\r\n").into_bytes(),
        text.trim_end().as_bytes().to_vec(),
        with_bom,
    ] {
        assert_statuses(&KnownHosts::parse(&contents), &fixture_cases());
    }
}

#[test]
fn only_version_1_hashes_are_read() {
    let version_1 = fixture_line(10);
    assert!(version_1.starts_with("|1|"));
    let version_2 = version_1.replacen("|1|", "|2|", 1);

    let key = host_key(ED);
    let name = "hashed.example.test";
    let good = KnownHosts::parse(version_1.as_bytes());
    assert_eq!(good.check(name, 22, &key), HostKeyStatus::Known);
    let skipped = KnownHosts::parse(version_2.as_bytes());
    assert_eq!(skipped.check(name, 22, &key), HostKeyStatus::Unknown);
    assert!(!skipped.uses_hashing());
}

#[test]
fn a_comment_that_is_not_utf8_does_not_lose_the_key() {
    let mut contents = format!("a.example.test {ED} ").into_bytes();
    contents.extend_from_slice(&[0xFF, 0xFE, b'\n']);
    let file = KnownHosts::parse(&contents);
    assert_eq!(
        file.check("a.example.test", 22, &host_key(ED)),
        HostKeyStatus::Known
    );
}

#[test]
fn a_revoked_plain_line_beats_a_good_port_line() {
    // OpenSSH accepts this one, as it stops at the port line. Refusing is the safe difference.
    let file = KnownHosts::parse(
        format!("[a.test]:2200 {ED}\na.test {ED2}\n@revoked a.test {ED}\n").as_bytes(),
    );
    assert_eq!(
        file.check("a.test", 2200, &host_key(ED)),
        HostKeyStatus::Revoked { line: 3 }
    );
}

#[test]
fn a_port_line_beats_a_plain_line_with_the_key() {
    let file = KnownHosts::parse(format!("[a.test]:2200 {ED2}\na.test {ED}\n").as_bytes());
    assert_eq!(
        file.check("a.test", 2200, &host_key(ED)),
        changed(1, ED2_FP)
    );
}

#[test]
fn known_key_types_lists_what_is_on_file() {
    let file = known_hosts();
    let types = |host, port| file.known_key_types(host, port);
    assert_eq!(types("multi.example.test", 22), ["ssh-ed25519", "ssh-rsa"]);
    assert_eq!(types("hashed.example.test", 22), ["ssh-ed25519"]);
    assert_eq!(types("fallback.example.test", 2200), ["ssh-ed25519"]);
    assert_eq!(types("x.ca.example.test", 22), Vec::<String>::new());
    // The `@revoked *` line names nistp521 for every host, and must not make it preferred.
    assert_eq!(types("never.example.test", 22), Vec::<String>::new());

    let repeated = KnownHosts::parse(
        format!(
            "a.test {ED}
a.test {ED2}
"
        )
        .as_bytes(),
    );
    assert_eq!(repeated.known_key_types("a.test", 22), ["ssh-ed25519"]);

    let both =
        KnownHosts::parse(format!("a.test {RSA}\na.test {ED}\n[a.test]:2200 {P256}").as_bytes());
    assert_eq!(
        both.known_key_types("a.test", 2200),
        ["ecdsa-sha2-nistp256", "ssh-rsa", "ssh-ed25519"]
    );
}

#[test]
fn a_missing_file_is_empty_and_a_directory_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = KnownHosts::load(&dir.path().join("none")).unwrap();
    assert_eq!(
        missing.check("a.test", 22, &host_key(ED)),
        HostKeyStatus::Unknown
    );
    assert!(matches!(
        KnownHosts::load(dir.path()),
        Err(KnownHostsError::Io { .. })
    ));
    let loaded = KnownHosts::load(&fixture_path()).unwrap();
    assert_statuses(&loaded, &fixture_cases());
}

#[test]
fn appending_creates_the_directory_and_writes_openssh_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new").join(".ssh").join("known_hosts");
    let key = host_key(ED);

    assert_eq!(
        append_if_unknown(&path, "Server.Test", 22, &key).unwrap(),
        HostKeyStatus::Unknown
    );
    assert_eq!(
        append_if_unknown(&path, "ported.test", 2200, &key).unwrap(),
        HostKeyStatus::Unknown
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("server.test {ED}\n[ported.test]:2200 {ED}\n")
    );

    let file = KnownHosts::load(&path).unwrap();
    assert!(!file.uses_hashing());
    assert_eq!(file.check("server.test", 22, &key), HostKeyStatus::Known);
    assert_eq!(file.check("ported.test", 2200, &key), HostKeyStatus::Known);
    assert_eq!(file.check("ported.test", 22, &key), HostKeyStatus::Unknown);
}

#[test]
fn appending_writes_only_when_still_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let start = format!("a.test {ED}\n@revoked b.test {ED}\nc.test {ED}\n");
    fs::write(&path, &start).unwrap();

    for (host, port, key, want) in [
        ("a.test", 22, ED, HostKeyStatus::Known),
        ("a.test", 22, ED2, changed(1, ED_FP)),
        ("b.test", 22, ED, HostKeyStatus::Revoked { line: 2 }),
        // Known through the plain line, so no line for the port is needed.
        ("c.test", 2200, ED, HostKeyStatus::Known),
    ] {
        assert_eq!(
            append_if_unknown(&path, host, port, &host_key(key)).unwrap(),
            want,
            "{host}:{port}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), start);
    }
}

#[test]
fn appending_follows_a_hashed_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    let hashed_line = fixture_line(10);
    fs::write(&path, format!("plain.test {ED}\n{hashed_line}\n")).unwrap();
    let key = host_key(ED2);

    append_if_unknown(&path, "New.Test", 22, &key).unwrap();
    append_if_unknown(&path, "other.test", 2200, &key).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(
        lines[2].starts_with("|1|") && lines[2].ends_with(ED2),
        "{}",
        lines[2]
    );
    assert!(lines[3].starts_with("|1|"), "{}", lines[3]);
    assert!(!text.to_lowercase().contains("new.test"));
    assert!(!text.contains("other.test"));
    assert_ne!(salt_of(lines[2]), salt_of(lines[3]));

    let file = KnownHosts::parse(text.as_bytes());
    assert_eq!(file.check("new.test", 22, &key), HostKeyStatus::Known);
    assert_eq!(file.check("other.test", 2200, &key), HostKeyStatus::Known);
    assert_eq!(file.check("other.test", 22, &key), HostKeyStatus::Unknown);
    assert_eq!(
        file.check("new.test", 22, &host_key(ED)),
        changed(3, ED2_FP)
    );
}

fn fixture_line(line: usize) -> String {
    let contents = String::from_utf8(fixture()).unwrap();
    contents.lines().nth(line - 1).unwrap().to_owned()
}

fn salt_of(line: &str) -> &str {
    line.split('|').nth(2).unwrap()
}

#[test]
fn a_cert_authority_hashed_line_counts_as_the_files_style() {
    let hashed = fixture_line(10);
    let name = hashed.split(' ').next().unwrap();
    let file = KnownHosts::parse(format!("@cert-authority {name} {ED}\n").as_bytes());
    assert!(file.uses_hashing());
}

#[test]
fn appending_never_touches_existing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let key = host_key(ED);
    for (name, start, separator) in [
        ("no-newline", format!("old.test {ED2}").into_bytes(), "\n"),
        ("crlf", format!("old.test {ED2}\r\n").into_bytes(), ""),
        ("bom", format!("\u{FEFF}old.test {ED2}\n").into_bytes(), ""),
        ("comment", b"# \xFF\xFE not utf-8".to_vec(), "\n"),
        ("empty", Vec::new(), ""),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, &start).unwrap();
        append_if_unknown(&path, "new.test", 22, &key).unwrap();

        let mut want = start;
        want.extend_from_slice(format!("{separator}new.test {ED}\n").as_bytes());
        assert_eq!(fs::read(&path).unwrap(), want, "{name}");
        let file = KnownHosts::load(&path).unwrap();
        assert_eq!(
            file.check("new.test", 22, &key),
            HostKeyStatus::Known,
            "{name}"
        );
    }
}

#[test]
fn a_host_name_cannot_add_lines_or_patterns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    for host in [
        "a.test\nevil.test ssh-ed25519 AAAA",
        "a.test b",
        "*",
        "a.test,evil.test",
        "",
    ] {
        assert!(
            matches!(
                append_if_unknown(&path, host, 22, &host_key(ED)),
                Err(KnownHostsError::InvalidHost(_))
            ),
            "{host:?}"
        );
        assert!(!path.exists(), "{host:?}");
    }
}

#[test]
fn an_unreadable_file_is_an_error_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let error = append_if_unknown(dir.path(), "a.test", 22, &host_key(ED)).unwrap_err();
    assert!(matches!(error, KnownHostsError::Io { .. }), "{error}");
}

/// Runs `count` appends at once, released together so they overlap.
fn append_concurrently(
    path: &std::path::Path,
    count: usize,
    host: impl Fn(usize) -> String + Sync,
    key: &HostKey,
) {
    let barrier = Barrier::new(count);
    thread::scope(|scope| {
        for index in 0..count {
            let (barrier, host) = (&barrier, &host);
            scope.spawn(move || {
                barrier.wait();
                append_if_unknown(path, &host(index), 22, key).unwrap();
            });
        }
    });
}

#[test]
fn concurrent_appends_all_land_on_their_own_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    // No final newline, so a racing writer could also get the separator wrong.
    fs::write(&path, format!("old.test {ED}")).unwrap();
    let key = host_key(ED2);

    append_concurrently(&path, 16, |i| format!("h{i}.test"), &key);

    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 17, "{text}");
    assert!(text.ends_with('\n'));
    let file = KnownHosts::parse(text.as_bytes());
    assert_eq!(
        file.check("old.test", 22, &host_key(ED)),
        HostKeyStatus::Known
    );
    for i in 0..16 {
        assert_eq!(
            file.check(&format!("h{i}.test"), 22, &key),
            HostKeyStatus::Known
        );
    }
}

#[test]
fn concurrent_appends_of_one_host_write_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");

    append_concurrently(&path, 16, |_| "same.test".to_owned(), &host_key(ED));

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("same.test {ED}\n")
    );
}

proptest! {
    #[test]
    fn parsing_any_bytes_does_not_panic(contents in proptest::collection::vec(any::<u8>(), 0..512)) {
        let file = KnownHosts::parse(&contents);
        let _ = file.check("a.test", 2200, &host_key(ED));
        let _ = file.known_key_types("a.test", 2200);
    }

    #[test]
    fn parsing_mangled_lines_does_not_panic(
        lines in proptest::collection::vec("[@|1a.*?!,\\[\\]: \t#=+/-]{0,40}", 0..8),
    ) {
        let contents = lines.iter().map(|line| format!("{line} {ED}\n")).collect::<String>();
        let file = KnownHosts::parse(contents.as_bytes());
        let _ = file.check("a.test", 22, &host_key(ED));
    }

    #[test]
    fn what_is_appended_is_known_only_where_it_was_saved(
        host in "[a-zA-Z0-9][a-zA-Z0-9.:-]{0,30}",
        port in 1u16..,
        hashed in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let start = if hashed { fixture_line(10) } else { String::new() };
        fs::write(&path, format!("{start}\n")).unwrap();

        let key = host_key(ED2);
        prop_assert_eq!(
            append_if_unknown(&path, &host, port, &key).unwrap(),
            HostKeyStatus::Unknown
        );

        let file = KnownHosts::load(&path).unwrap();
        prop_assert_eq!(file.uses_hashing(), hashed);
        prop_assert_eq!(file.check(&host, port, &key), HostKeyStatus::Known);
        let other_name = format!("x{host}");
        prop_assert_eq!(file.check(&other_name, port, &key), HostKeyStatus::Unknown);
        let is_changed = matches!(
            file.check(&host, port, &host_key(ED)),
            HostKeyStatus::Changed { .. }
        );
        prop_assert!(is_changed);
    }
}
