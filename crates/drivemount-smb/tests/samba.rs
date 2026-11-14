// SPDX-License-Identifier: GPL-3.0-or-later

//! The FR-SMB acceptance criteria of plan M3-02 against the Samba test server (spec §10.2). They map
//! drives from H: to Q: in this logon session for a moment, and remove them.
//!
//! `tests\docker\servers.ps1 up`, then
//! `cargo test -p drivemount-smb --test samba -- --ignored --test-threads=1`
//!
//! Windows reaches Samba at the WSL VM's address, not `localhost` (spike M3-01 §8). It is read from
//! WSL each run, or from `DRIVEMOUNT_SMB_HOST`. The outage test needs root in WSL, for iptables.
//!
//! Files made on the drives are left there: the agent's sandbox may refuse to delete on a mapped
//! letter (spike M3-01), so each test uses a new name.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Each test holds its turn for the whole test, awaits included.
#![allow(clippy::await_holding_lock)]

use std::env;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drivemount_smb::{DriveStatus, SmbAuth, SmbBackend, SmbTarget, other_drives};
use drivemount_vfs::{
    ConnectError, DriveLetter, Health, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MountBackend, MountError, Prompter, Remember, Secret, SecretRequest, async_trait,
};
use drivemount_win::drives::is_letter_free;
use drivemount_win::wnet;

const DISTRO: &str = "Ubuntu-22.04";
const USER1: (&str, &str) = ("smbuser1", "smbpass1");
const USER2: (&str, &str) = ("smbuser2", "smbpass2");

/// The tests share the server's sessions and the free letters, so they take turns.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wsl_root(command: &str) -> std::process::Output {
    Command::new("wsl.exe")
        .args(["-d", DISTRO, "-u", "root", "--", "bash", "-c", command])
        .output()
        .unwrap()
}

fn server() -> String {
    static SERVER: OnceLock<String> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            env::var("DRIVEMOUNT_SMB_HOST").unwrap_or_else(|_| {
                let output = wsl_root("hostname -I");
                let addresses = String::from_utf8_lossy(&output.stdout).into_owned();
                addresses
                    .split_whitespace()
                    .next()
                    .expect("no WSL address")
                    .to_owned()
            })
        })
        .clone()
}

/// Free letters from Q: down, the first `count` of them.
fn free_letters(count: usize) -> Vec<DriveLetter> {
    let free: Vec<_> = ('H'..='Q')
        .rev()
        .filter_map(DriveLetter::new)
        .filter(|&letter| is_letter_free(letter).unwrap())
        .take(count)
        .collect();
    assert_eq!(free.len(), count, "not enough free letters from H to Q");
    free
}

fn target(share: &str, (user, password): (&str, &str)) -> SmbTarget {
    SmbTarget {
        server: server(),
        share: share.to_owned(),
        domain: String::new(),
        user: user.to_owned(),
        auth: SmbAuth::Password(Some(Secret::new(password.to_owned()))),
    }
}

fn is_mapped(letter: DriveLetter) -> bool {
    let local = letter.to_string();
    wnet::connected()
        .unwrap()
        .iter()
        .any(|connection| connection.local.as_deref() == Some(local.as_str()))
}

fn unique_name(what: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{what}-{nanos}.txt")
}

/// Nobody answers.
struct NoAnswers;

#[async_trait]
impl Prompter for NoAnswers {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        None
    }
    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        None
    }
    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }
    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
}

/// FR-SMB-03: mapped through the API with the credentials in the call, never remembered by Windows;
/// FR-SMB-04: removed again.
#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn a_share_maps_temporarily_and_unmaps() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("a", USER1));

    backend.mount(letter, &NoAnswers).await.unwrap();
    assert!(is_mapped(letter));
    let local = letter.to_string();
    let remembered = wnet::remembered().unwrap();
    assert!(
        remembered
            .iter()
            .all(|connection| connection.local.as_deref() != Some(local.as_str())),
        "CONNECT_TEMPORARY: Windows must not remember it"
    );
    let hello = std::fs::read_to_string(format!(r"{letter}\hello.txt")).unwrap();
    assert_eq!(hello.trim(), "hello");
    assert!(backend.take_remembered().is_empty());

    backend.unmount().await.unwrap();
    assert!(!is_mapped(letter));
}

#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn a_wrong_password_is_e_auth_and_leaves_nothing() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("a", (USER2.0, "wrong")));

    let error = backend.mount(letter, &NoAnswers).await.unwrap_err();

    assert_eq!(
        error,
        MountError::Connect(ConnectError::Auth {
            user: USER2.0.to_owned(),
            host: server(),
        })
    );
    assert!(!is_mapped(letter));
}

#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn a_missing_share_is_e_smb_share_not_found() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("nosuchshare", USER1));

    let error = backend.mount(letter, &NoAnswers).await.unwrap_err();

    assert_eq!(
        error,
        MountError::Connect(ConnectError::ShareNotFound {
            share: "nosuchshare".to_owned(),
            server: server(),
        })
    );
}

/// FR-SMB-06's cause, with the other connection named (spec §5.9).
#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn another_user_on_the_same_server_is_e_smb_1219_naming_the_first() {
    let _turn = one_at_a_time();
    let [first_letter, second_letter] = free_letters(2)[..] else {
        unreachable!()
    };
    let first = SmbBackend::new(target("a", USER1));
    first.mount(first_letter, &NoAnswers).await.unwrap();

    let second = SmbBackend::new(target("b", USER2));
    let error = second.mount(second_letter, &NoAnswers).await.unwrap_err();
    first.unmount().await.unwrap();

    let MountError::Connect(ConnectError::SmbCredentialConflict(conflict)) = error else {
        panic!("not E-SMB-1219: {error:?}");
    };
    assert_eq!(conflict.server, server());
    let other = conflict.other.expect("the first mapping was not found");
    assert_eq!(other.letter, Some(first_letter));
    assert!(other.user.ends_with(USER1.0), "{}", other.user);
    assert!(!is_mapped(second_letter));
}

/// FR-SMB-04: with a file open, a plain unmount refuses and leaves the drive; forced, it goes.
#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn open_files_refuse_an_unmount_until_it_is_forced() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("a", USER1));
    backend.mount(letter, &NoAnswers).await.unwrap();
    let mut open = File::create(format!(r"{letter}\{}", unique_name("open"))).unwrap();
    open.write_all(b"held open").unwrap();

    let refused = backend.unmount().await.unwrap_err();
    assert_eq!(refused, MountError::FilesOpen(letter));
    assert!(is_mapped(letter));

    backend.unmount_forced().await.unwrap();
    assert!(!is_mapped(letter));
    assert!(
        open.write_all(b" more").is_err(),
        "the handle should be gone"
    );
}

/// FR-SMB-07: a backend dropped while mapped, as when the app quits without unmounting, removes its
/// mapping, open files or not.
#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn a_backend_dropped_while_mapped_removes_its_drive() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("a", USER1));
    backend.mount(letter, &NoAnswers).await.unwrap();
    let _open = File::create(format!(r"{letter}\{}", unique_name("dropped"))).unwrap();

    drop(backend);

    assert!(!is_mapped(letter));
}

/// FR-SMB-05: health follows the server through an outage (packets dropped, as a cable pull), and
/// the mapping stays throughout. Needs root in WSL.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Samba test server and root in WSL"]
async fn health_is_lost_during_an_outage_and_back_after() {
    struct Outage;

    impl Outage {
        fn start() -> Self {
            let output = wsl_root("iptables -I DOCKER-USER -p tcp --dport 445 -j DROP");
            assert!(output.status.success(), "{output:?}");
            Self
        }
    }

    impl Drop for Outage {
        fn drop(&mut self) {
            let _ = wsl_root("iptables -D DOCKER-USER -p tcp --dport 445 -j DROP");
        }
    }

    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target("a", USER1));
    backend.mount(letter, &NoAnswers).await.unwrap();
    let mut health = backend.health();

    backend.check_connection().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(*health.borrow_and_update(), Health::Ok);

    let outage = Outage::start();
    let started = Instant::now();
    backend.check_connection().await;
    tokio::time::timeout(Duration::from_secs(10), health.changed())
        .await
        .expect("the check did not give up in time")
        .unwrap();
    assert!(matches!(*health.borrow(), Health::Lost(_)));
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert!(backend.reconnect(&NoAnswers).await.is_err());
    assert!(
        is_mapped(letter),
        "the mapping stays while the server is gone"
    );

    drop(outage);
    // Windows notices a returned server up to about 30 s late (spike M3-01 §6), and a check timed
    // out during the outage still has to return. Retry as the manager would.
    let back = Instant::now();
    loop {
        if backend.reconnect(&NoAnswers).await.is_ok() {
            break;
        }
        assert!(
            back.elapsed() < Duration::from_secs(150),
            "the drive did not come back"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert_eq!(*backend.health().borrow(), Health::Ok);
    backend.unmount_forced().await.unwrap();
}

/// FR-SMB-09: a drive mapped by `net use` is listed with its user; the letters the app holds are
/// not. The test credentials are on `net use`'s command line, which is fine for them alone.
#[tokio::test]
#[ignore = "needs the Samba test server"]
async fn a_net_use_drive_is_listed_as_another_drive() {
    struct NetUse(DriveLetter);

    impl Drop for NetUse {
        fn drop(&mut self) {
            let _ = Command::new("net")
                .args(["use", &self.0.to_string(), "/delete", "/y"])
                .output();
        }
    }

    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let unc = format!(r"\\{}\b", server());
    let output = Command::new("net")
        .args(["use", &letter.to_string(), &unc, USER1.1])
        .arg(format!("/user:{}", USER1.0))
        .arg("/persistent:no")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mapped = NetUse(letter);

    let listed = other_drives(&[]).unwrap();
    let drive = listed
        .iter()
        .find(|drive| drive.letter == letter)
        .expect("the net use drive is not listed");
    assert_eq!(drive.unc, unc);
    assert_eq!(drive.status, DriveStatus::Mapped);
    assert!(
        drive
            .user
            .as_deref()
            .is_some_and(|user| user.ends_with(USER1.0))
    );

    let held = other_drives(&[letter]).unwrap();
    assert!(held.iter().all(|drive| drive.letter != letter));

    drop(mapped);
    let gone = other_drives(&[]).unwrap();
    assert!(gone.iter().all(|drive| drive.letter != letter));
}
