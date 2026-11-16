// SPDX-License-Identifier: GPL-3.0-or-later

//! The FR-SMB acceptance criteria that need no outage, against this machine's own Windows server
//! (plan M3-05, spike M3-01 §8): the Windows half of the SMB tests, which CI runs. They map drives
//! from H: to Q: in this logon session for a moment, and remove them.
//!
//! From an elevated PowerShell, once: `scripts\smb-test-share.ps1` (a share and two local users;
//! `-Remove` takes them away again). Then, from any shell:
//! `cargo test -p drivemount-smb --test local_share -- --ignored --test-threads=1`
//!
//! The settings come from the environment (`DRIVEMOUNT_SMB_LOCAL_SHARE`, `…_USER1`, `…_PASS1`,
//! `…_USER2`, `…_PASS2`), or else from the `target\smb-test-share.env` the script writes.
//!
//! Windows counts `127.0.0.1`, `localhost` and the computer's name as three servers (spike M3-01
//! §4). The explicit logins use `127.0.0.1`; `localhost` is the second name; the computer's name is
//! for *Current Windows user* alone, as null credentials would reuse any session still open to the
//! same name (§5).

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Each test holds its turn for the whole test, awaits included.
#![allow(clippy::await_holding_lock)]

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use drivemount_smb::{SmbAuth, SmbBackend, SmbTarget};
use drivemount_vfs::{
    ConnectError, DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MountBackend, MountError, Prompter, Remember, Secret, SecretRequest, async_trait,
};
use drivemount_win::drives::is_letter_free;
use drivemount_win::wnet;

const ADDRESS: &str = "127.0.0.1";
const SECOND_NAME: &str = "localhost";

/// The tests share the server's sessions and the free letters, so they take turns.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What `scripts\smb-test-share.ps1` made.
struct Setup {
    share: String,
    users: [(String, String); 2],
}

fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\..\target\smb-test-share.env");
        let from_file: HashMap<String, String> = std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
            .collect();
        let get = |key: &str| {
            let key = format!("DRIVEMOUNT_SMB_LOCAL_{key}");
            env::var(&key)
                .ok()
                .or_else(|| from_file.get(&key).cloned())
                .unwrap_or_else(|| {
                    panic!(
                        "{key} is not set and not in {}: run scripts\\smb-test-share.ps1 first",
                        file.display()
                    )
                })
        };
        Setup {
            share: get("SHARE"),
            users: [(get("USER1"), get("PASS1")), (get("USER2"), get("PASS2"))],
        }
    })
}

fn computer_name() -> String {
    env::var("COMPUTERNAME").expect("COMPUTERNAME is not set")
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

fn target(server: &str, share: &str, (user, password): &(String, String)) -> SmbTarget {
    SmbTarget {
        server: server.to_owned(),
        share: share.to_owned(),
        domain: String::new(),
        user: user.clone(),
        auth: SmbAuth::Password(Some(Secret::new(password.clone()))),
    }
}

fn user(number: usize) -> &'static (String, String) {
    &setup().users[number - 1]
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

/// FR-SMB-02 (*Password*), FR-SMB-03: mapped with the credentials in the call, readable and
/// writable, never remembered by Windows, and removed again.
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn a_share_maps_temporarily_and_unmaps() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));

    backend.mount(letter, &NoAnswers).await.unwrap();
    assert!(is_mapped(letter));
    let local = letter.to_string();
    assert!(
        wnet::remembered()
            .unwrap()
            .iter()
            .all(|connection| connection.local.as_deref() != Some(local.as_str())),
        "CONNECT_TEMPORARY: Windows must not remember it"
    );
    let hello = std::fs::read_to_string(format!(r"{letter}\hello.txt")).unwrap();
    assert_eq!(hello.trim(), "hello");
    let written = format!(r"{letter}\{}", unique_name("written"));
    std::fs::write(&written, b"written over SMB").unwrap();
    assert_eq!(std::fs::read(&written).unwrap(), b"written over SMB");
    std::fs::remove_file(&written).unwrap();
    assert!(backend.take_remembered().is_empty());

    backend.unmount().await.unwrap();
    assert!(!is_mapped(letter));
}

/// FR-SMB-02 (*Current Windows user*): no credentials in the call, the signed-in user's session.
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn the_windows_user_maps_with_no_credentials() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let server = computer_name();
    let backend = SmbBackend::new(SmbTarget {
        server: server.clone(),
        share: setup().share.clone(),
        domain: String::new(),
        user: String::new(),
        auth: SmbAuth::WindowsUser,
    });

    backend.mount(letter, &NoAnswers).await.unwrap();

    let local = letter.to_string();
    let used = wnet::net_uses().unwrap();
    let mine = used
        .iter()
        .find(|used| used.local.as_deref() == Some(local.as_str()))
        .expect("the mapping is not in NetUseEnum");
    // The signed-in account, which NetUseEnum may name differently from USERNAME (a Microsoft
    // account shows as `MicrosoftAccount\<email>`). What matters is that no test user's session was
    // picked up.
    assert!(
        setup()
            .users
            .iter()
            .all(|(test_user, _)| !mine.user.eq_ignore_ascii_case(test_user)),
        "mapped as the test user {}, not the signed-in user",
        mine.user
    );
    let hello = std::fs::read_to_string(format!(r"{letter}\hello.txt")).unwrap();
    assert_eq!(hello.trim(), "hello");
    backend.unmount().await.unwrap();
    assert!(!is_mapped(letter));
}

/// At a Windows server a wrong password is 86 or 1326; both are E-AUTH (spike M3-01 §3).
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn a_wrong_password_is_e_auth_and_leaves_nothing() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let (name, _) = user(2);
    let wrong = (name.clone(), "not-the-password".to_owned());
    let backend = SmbBackend::new(target(ADDRESS, &setup().share, &wrong));

    let error = backend.mount(letter, &NoAnswers).await.unwrap_err();

    assert_eq!(
        error,
        MountError::Connect(ConnectError::Auth {
            user: name.clone(),
            host: ADDRESS.to_owned(),
        })
    );
    assert!(!is_mapped(letter));
}

#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn a_missing_share_is_e_smb_share_not_found() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target(ADDRESS, "drivemount-no-such-share", user(1)));

    let error = backend.mount(letter, &NoAnswers).await.unwrap_err();

    assert_eq!(
        error,
        MountError::Connect(ConnectError::ShareNotFound {
            share: "drivemount-no-such-share".to_owned(),
            server: ADDRESS.to_owned(),
        })
    );
    assert!(!is_mapped(letter));
}

/// FR-PROF-06 for SMB: *Test connection* maps without a letter, finds the sub-path or says it is
/// missing, and leaves no connection behind.
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn test_connection_finds_the_sub_path_or_says_it_is_missing() {
    let _turn = one_at_a_time();
    // A stored password is used once and dropped, so each test gets a backend of its own, as in the
    // app.
    let test = |sub_path: &'static str| async move {
        let backend = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));
        backend.test(sub_path, &NoAnswers).await
    };
    let unc = format!(r"\\{ADDRESS}\{}", setup().share).to_lowercase();
    let left_behind = || {
        wnet::connected()
            .unwrap()
            .iter()
            .any(|connection| connection.remote.to_lowercase() == unc)
    };

    test("").await.unwrap();
    test("sub").await.unwrap();
    assert_eq!(
        test("missing").await.unwrap_err(),
        ConnectError::PathNotFound {
            path: "missing".to_owned()
        }
    );
    assert_eq!(
        test("hello.txt").await.unwrap_err(),
        ConnectError::PathNotDir {
            path: "hello.txt".to_owned()
        }
    );
    assert!(!left_behind(), "the test connection is still there");
}

/// FR-SMB-06's cause at a Windows server, with the other connection named (spec §5.9).
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn another_user_at_the_same_address_is_e_smb_1219_naming_the_first() {
    let _turn = one_at_a_time();
    let [first_letter, second_letter] = free_letters(2)[..] else {
        unreachable!()
    };
    let first = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));
    first.mount(first_letter, &NoAnswers).await.unwrap();

    let second = SmbBackend::new(target(ADDRESS, &setup().share, user(2)));
    let error = second.mount(second_letter, &NoAnswers).await.unwrap_err();
    first.unmount().await.unwrap();

    let MountError::Connect(ConnectError::SmbCredentialConflict(conflict)) = error else {
        panic!("not E-SMB-1219: {error:?}");
    };
    assert_eq!(conflict.server, ADDRESS);
    let other = conflict.other.expect("the first mapping was not found");
    assert_eq!(other.letter, Some(first_letter));
    assert!(
        other
            .user
            .to_lowercase()
            .ends_with(&user(1).0.to_lowercase()),
        "{}",
        other.user
    );
    assert!(!is_mapped(second_letter));
}

/// E-SMB-1219's hint: another name for the server takes the second user beside the first.
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn another_name_for_the_server_takes_the_second_user() {
    let _turn = one_at_a_time();
    let [first_letter, second_letter] = free_letters(2)[..] else {
        unreachable!()
    };
    let first = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));
    first.mount(first_letter, &NoAnswers).await.unwrap();
    let second = SmbBackend::new(target(SECOND_NAME, &setup().share, user(2)));

    let mapped = second.mount(second_letter, &NoAnswers).await;
    let both = is_mapped(first_letter) && is_mapped(second_letter);
    let _ = second.unmount().await;
    first.unmount().await.unwrap();

    mapped.unwrap();
    assert!(both);
}

/// FR-SMB-04: with a file open, a plain unmount refuses and leaves the drive; forced, it goes.
#[tokio::test]
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn open_files_refuse_an_unmount_until_it_is_forced() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));
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
#[ignore = "needs scripts\\smb-test-share.ps1"]
async fn a_backend_dropped_while_mapped_removes_its_drive() {
    let _turn = one_at_a_time();
    let [letter] = free_letters(1)[..] else {
        unreachable!()
    };
    let backend = SmbBackend::new(target(ADDRESS, &setup().share, user(1)));
    backend.mount(letter, &NoAnswers).await.unwrap();
    let _open = File::create(format!(r"{letter}\{}", unique_name("dropped"))).unwrap();

    drop(backend);

    assert!(!is_mapped(letter));
}
