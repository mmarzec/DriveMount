// SPDX-License-Identifier: GPL-3.0-or-later

//! The jump host (FR-SSH-11). A failure to reach the jump host itself runs everywhere. The rest needs
//! the S-03 servers and is ignored by default:
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p fsm-sftp --test jump -- --ignored
//! ```
//!
//! `sshd-target` publishes no port, and its name resolves only on the Docker network, so reaching it
//! at all shows the tunnel works. Its host key is RSA and the jump host's is Ed25519, so a mix-up of
//! the two would show.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::{ED, Link, Proxy, RSA, host_key};
use fsm_sftp::connector::{Auth, JumpHost, SftpConnector, SftpProfile, SftpSession};
use fsm_vfs::{
    ConnectError, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    Prompter, Remember, Secret, SecretKind, SecretRequest, async_trait,
};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::time;

const JUMP: &str = "localhost";
const JUMP_PORT: u16 = 2228;
const NO_FORWARD_PORT: u16 = 2229;
const TARGET: &str = "sshd-target";
const TARGET_PORT: u16 = 22;
const PASSPHRASE: &str = "test-passphrase";
/// So a broken test fails rather than hangs.
const TEST_LIMIT: Duration = Duration::from_secs(60);

type WhileAsked = Box<dyn Fn(&str) + Send + Sync>;

/// Answers dialogs from a script and remembers what it was asked. Host keys are trusted once unless
/// a decision is set for that host. A password dialog with no answer left counts as *Cancel*.
#[derive(Default)]
struct Dialogs {
    decisions: HashMap<&'static str, HostKeyDecision>,
    while_asked: Option<WhileAsked>,
    secrets: Mutex<VecDeque<(String, Remember)>>,
    keys_asked: Mutex<Vec<HostKeyRequest>>,
    secrets_asked: Mutex<Vec<SecretRequest>>,
}

impl Dialogs {
    fn deciding(decisions: &[(&'static str, HostKeyDecision)]) -> Self {
        Self {
            decisions: decisions.iter().copied().collect(),
            ..Self::default()
        }
    }

    fn typing(answers: &[(&str, Remember)]) -> Self {
        let dialogs = Self::default();
        let answers = answers.iter().map(|(text, r)| ((*text).to_owned(), *r));
        dialogs.secrets.lock().unwrap().extend(answers);
        dialogs
    }

    fn keys_asked(&self) -> Vec<HostKeyRequest> {
        self.keys_asked.lock().unwrap().clone()
    }

    fn hosts_asked(&self) -> Vec<String> {
        let asked = self.keys_asked();
        asked.into_iter().map(|request| request.host).collect()
    }

    fn secrets_asked(&self) -> Vec<SecretRequest> {
        self.secrets_asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Prompter for Dialogs {
    async fn secret(&self, request: SecretRequest) -> Option<(Secret<String>, Remember)> {
        self.secrets_asked.lock().unwrap().push(request);
        let (text, remember) = self.secrets.lock().unwrap().pop_front()?;
        Some((Secret::new(text), remember))
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        panic!("no login should be asked for")
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        panic!("no keyboard-interactive round should be asked for")
    }

    async fn host_key(&self, request: HostKeyRequest) -> HostKeyDecision {
        let host = request.host.clone();
        self.keys_asked.lock().unwrap().push(request);
        if let Some(while_asked) = &self.while_asked {
            while_asked(&host);
        }
        let decision = self.decisions.get(host.as_str()).copied();
        decision.unwrap_or(HostKeyDecision::TrustOnce)
    }
}

fn secret(text: &str) -> Secret<String> {
    Secret::new(text.to_owned())
}

fn password(text: &str) -> Auth {
    Auth::Password(Some(secret(text)))
}

fn jump_host(auth: Auth) -> JumpHost {
    JumpHost {
        host: JUMP.to_owned(),
        port: JUMP_PORT,
        user: "tester".to_owned(),
        auth,
    }
}

fn profile(known_hosts: &Path, jump: JumpHost) -> SftpProfile {
    SftpProfile {
        host: TARGET.to_owned(),
        port: TARGET_PORT,
        user: "tester".to_owned(),
        auth: password("tester"),
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: Duration::from_secs(15),
        known_hosts: known_hosts.to_owned(),
        jump: Some(jump),
    }
}

fn empty_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    (dir, path)
}

async fn connect(
    connector: &SftpConnector,
    dialogs: &Dialogs,
) -> Result<SftpSession, ConnectError> {
    time::timeout(TEST_LIMIT, connector.connect_session(dialogs))
        .await
        .expect("connect_session must end by itself")
}

async fn connect_profile(
    profile: SftpProfile,
    dialogs: &Dialogs,
) -> Result<SftpSession, ConnectError> {
    connect(&SftpConnector::new(profile), dialogs).await
}

fn expect_error(result: Result<SftpSession, ConnectError>) -> ConnectError {
    match result {
        Ok(session) => panic!("connected to {}", session.root()),
        Err(error) => error,
    }
}

/// `type base64` of a key the servers hold.
fn server_key(file: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "../../tests/docker/keys", file]
        .iter()
        .collect();
    let line = fs::read_to_string(path).expect("run tests/docker/servers.ps1 up first");
    let mut words = line.split_whitespace();
    format!("{} {}", words.next().unwrap(), words.next().unwrap())
}

fn jump_key() -> String {
    server_key("ssh_host_ed25519_key.pub")
}

fn target_key() -> String {
    server_key("ssh_host_rsa_key.pub")
}

fn jump_line(key: &str) -> String {
    format!("[{JUMP}]:{JUMP_PORT} {key}\n")
}

fn target_line(key: &str) -> String {
    format!("{TARGET} {key}\n")
}

fn key_path(file: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "../../tests/docker/keys", file]
        .iter()
        .collect()
}

fn auth_error(user: &str, host: &str) -> ConnectError {
    ConnectError::Auth {
        user: user.to_owned(),
        host: host.to_owned(),
    }
}

fn password_request(host: &str) -> SecretRequest {
    SecretRequest {
        kind: SecretKind::Password {
            user: "tester".to_owned(),
            host: host.to_owned(),
        },
        allow_remember: true,
    }
}

async fn closes_within(session: &SftpSession, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if session.is_closed() {
            return true;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ---- The jump host cannot be reached. No server needed. ----

#[tokio::test]
async fn a_jump_host_with_an_unknown_name_is_a_dns_error_about_it() {
    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(password("tester"));
    jump.host = "no-such-jump.invalid".to_owned();
    let error =
        expect_error(connect_profile(profile(&known_hosts, jump), &Dialogs::default()).await);
    assert_eq!(
        error,
        ConnectError::Dns {
            host: "no-such-jump.invalid".to_owned()
        }
    );
}

#[tokio::test]
async fn a_refused_jump_port_is_unreachable_and_about_the_jump_host() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(password("tester"));
    jump.host = "127.0.0.1".to_owned();
    jump.port = port;
    let error =
        expect_error(connect_profile(profile(&known_hosts, jump), &Dialogs::default()).await);
    assert_eq!(
        error,
        ConnectError::HostUnreachable {
            host: "127.0.0.1".to_owned(),
            port
        }
    );
}

/// Nothing but TCP from the jump host: E-TIMEOUT, within its own limit, and about the jump host.
#[tokio::test]
async fn a_silent_jump_host_times_out_and_is_named() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();

    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(password("tester"));
    jump.host = "127.0.0.1".to_owned();
    jump.port = port;
    let mut profile = profile(&known_hosts, jump);
    profile.timeout = Duration::from_millis(500);
    let start = Instant::now();
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert_eq!(
        error,
        ConnectError::Timeout {
            host: "127.0.0.1".to_owned()
        }
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    drop(listener);
}

// ---- The tunnel. ----

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_target_cannot_be_reached_without_the_jump_host() {
    let (_dir, known_hosts) = empty_dir();
    let mut profile = profile(&known_hosts, jump_host(password("tester")));
    profile.jump = None;
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert_eq!(
        error,
        ConnectError::Dns {
            host: TARGET.to_owned()
        }
    );
}

/// FR-SSH-11's acceptance test: the mount succeeds, and the bastion's key is in `known_hosts`.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn each_host_key_is_asked_for_and_saved_for_its_own_host() {
    let (_dir, known_hosts) = empty_dir();
    let dialogs = Dialogs::deciding(&[
        (JUMP, HostKeyDecision::TrustSave),
        (TARGET, HostKeyDecision::TrustSave),
    ]);
    let profile1 = profile(&known_hosts, jump_host(password("tester")));
    let session = connect_profile(profile1, &dialogs).await.unwrap();
    assert_eq!(session.root().as_str(), "/home/tester");
    assert!(!session.is_closed());

    assert_eq!(
        dialogs.keys_asked(),
        [
            HostKeyRequest {
                host: JUMP.to_owned(),
                port: JUMP_PORT,
                key_type: "ssh-ed25519".to_owned(),
                fingerprint: host_key(&jump_key()).fingerprint(),
            },
            HostKeyRequest {
                host: TARGET.to_owned(),
                port: TARGET_PORT,
                key_type: "ssh-rsa".to_owned(),
                fingerprint: host_key(&target_key()).fingerprint(),
            },
        ]
    );
    let saved = fs::read_to_string(&known_hosts).unwrap();
    assert!(saved.contains(jump_line(&jump_key()).trim_end()), "{saved}");
    assert!(
        saved.contains(target_line(&target_key()).trim_end()),
        "{saved}"
    );

    // Both keys are on file now, so nothing is asked.
    let silent = Dialogs::default();
    let again = profile(&known_hosts, jump_host(password("tester")));
    connect_profile(again, &silent).await.unwrap();
    assert_eq!(silent.keys_asked(), []);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn more_than_a_channel_window_of_data_passes_both_ways() {
    const SIZE: usize = 40 * 1024 * 1024;
    const CHUNK: usize = 128 * 1024;
    const IN_FLIGHT: usize = 32;

    let (_dir, known_hosts) = empty_dir();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let session = connect_profile(profile, &Dialogs::default()).await.unwrap();
    let sftp = session.sftp();
    let path = format!("{}/jump-transfer-{}", session.root(), std::process::id());
    let data: Vec<u8> = (0..SIZE).map(|at| (at ^ (at >> 11)) as u8).collect();

    let work = async {
        let flags = OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE | OpenFlags::READ;
        let file = sftp
            .open(path.as_str(), flags, FileAttributes::default())
            .await
            .unwrap();
        let handle = file.handle.as_str();

        stream::iter(data.chunks(CHUNK).enumerate())
            .map(|(index, chunk)| sftp.write(handle, (index * CHUNK) as u64, chunk.to_vec()))
            .buffer_unordered(IN_FLIGHT)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();

        let chunks: Vec<Vec<u8>> = stream::iter(0..SIZE.div_ceil(CHUNK))
            .map(|index| async move {
                let read = sftp.read(handle, (index * CHUNK) as u64, CHUNK as u32);
                read.await.map(|answer| answer.data)
            })
            .buffered(IN_FLIGHT)
            .try_collect()
            .await
            .unwrap();
        assert!(chunks.concat() == data, "the data must come back the same");

        sftp.close(handle).await.unwrap();
        sftp.remove(path.as_str()).await.unwrap();
    };
    time::timeout(TEST_LIMIT, work)
        .await
        .expect("the transfer must not stall");
}

// ---- Each host key is checked separately. ----

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn refusing_the_jump_hosts_key_never_reaches_the_target() {
    let (_dir, known_hosts) = empty_dir();
    let dialogs = Dialogs::deciding(&[(JUMP, HostKeyDecision::Reject)]);
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    assert_eq!(error, ConnectError::HostKeyRejected);
    assert_eq!(dialogs.hosts_asked(), [JUMP]);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn refusing_the_targets_key_keeps_only_the_jump_hosts() {
    let (_dir, known_hosts) = empty_dir();
    let dialogs = Dialogs::deciding(&[
        (JUMP, HostKeyDecision::TrustSave),
        (TARGET, HostKeyDecision::Reject),
    ]);
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    assert_eq!(error, ConnectError::HostKeyRejected);
    assert_eq!(dialogs.hosts_asked(), [JUMP, TARGET]);
    let saved = fs::read_to_string(&known_hosts).unwrap();
    assert!(saved.contains(&format!("[{JUMP}]:{JUMP_PORT}")), "{saved}");
    assert!(!saved.contains(TARGET), "{saved}");
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_changed_jump_key_is_refused_and_names_the_jump_host() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    fs::write(&known_hosts, jump_line(ED)).unwrap();
    let dialogs = Dialogs::default();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    let ConnectError::HostKeyChanged(details) = error else {
        panic!("{error:?}");
    };
    assert_eq!((details.host.as_str(), details.port), (JUMP, JUMP_PORT));
    assert_eq!(details.line, 1);
    assert_eq!(details.new_fingerprint, host_key(&jump_key()).fingerprint());
    assert_eq!(dialogs.keys_asked(), []);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_changed_target_key_is_refused_and_names_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    let lines = jump_line(&jump_key()) + &target_line(RSA);
    fs::write(&known_hosts, lines).unwrap();
    let dialogs = Dialogs::default();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    let ConnectError::HostKeyChanged(details) = error else {
        panic!("{error:?}");
    };
    assert_eq!((details.host.as_str(), details.port), (TARGET, TARGET_PORT));
    assert_eq!(details.line, 2);
    assert_eq!(dialogs.keys_asked(), []);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_jump_hosts_key_on_the_targets_line_does_not_pass() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    let lines = jump_line(&jump_key()) + &target_line(&jump_key());
    fs::write(&known_hosts, lines).unwrap();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert!(
        matches!(error, ConnectError::HostKeyChanged(_)),
        "{error:?}"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_revoked_jump_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    fs::write(&known_hosts, format!("@revoked {}", jump_line(&jump_key()))).unwrap();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    let ConnectError::HostKeyRevoked(details) = error else {
        panic!("{error:?}");
    };
    assert_eq!((details.host.as_str(), details.port), (JUMP, JUMP_PORT));
}

// ---- Each host has its own login. ----

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_refused_jump_login_names_the_jump_host_and_never_reaches_the_target() {
    let (_dir, known_hosts) = empty_dir();
    let dialogs = Dialogs::default();
    let profile = profile(&known_hosts, jump_host(password("wrong")));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    assert_eq!(error, auth_error("tester", JUMP));
    assert_eq!(dialogs.hosts_asked(), [JUMP]);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_refused_target_login_names_the_target() {
    let (_dir, known_hosts) = empty_dir();
    let mut profile = profile(&known_hosts, jump_host(password("tester")));
    profile.auth = password("wrong");
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert_eq!(error, auth_error("tester", TARGET));
}

/// Each of the two is offered *Remember* for its own place in Credential Manager, and a reconnect
/// of the same mount asks for neither.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_two_passwords_are_asked_for_and_remembered_apart() {
    for (jump_answer, target_answer) in
        [(Remember::Yes, Remember::No), (Remember::No, Remember::Yes)]
    {
        let (_dir, known_hosts) = empty_dir();
        let mut profile = profile(&known_hosts, jump_host(Auth::Password(None)));
        profile.auth = Auth::Password(None);
        let connector = SftpConnector::new(profile);
        let dialogs = Dialogs::typing(&[("tester", jump_answer), ("tester", target_answer)]);

        connect(&connector, &dialogs).await.unwrap();
        assert_eq!(
            dialogs.secrets_asked(),
            [password_request(JUMP), password_request(TARGET)]
        );
        let remembered = |taken: Option<Secret<String>>| taken.is_some();
        assert_eq!(
            remembered(connector.take_remembered_jump()),
            jump_answer == Remember::Yes
        );
        assert_eq!(
            remembered(connector.take_remembered()),
            target_answer == Remember::Yes
        );

        let silent = Dialogs::default();
        connect(&connector, &silent).await.unwrap();
        assert_eq!(silent.secrets_asked(), []);
    }
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn cancelling_the_jump_password_stops_there() {
    let (_dir, known_hosts) = empty_dir();
    let dialogs = Dialogs::default();
    let profile = profile(&known_hosts, jump_host(Auth::Password(None)));
    let error = expect_error(connect_profile(profile, &dialogs).await);
    assert_eq!(error, ConnectError::Cancelled);
    assert_eq!(dialogs.hosts_asked(), [JUMP]);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_jump_host_can_use_a_key_file_while_the_target_uses_a_password() {
    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(Auth::KeyFile {
        path: key_path("id_ed25519"),
        passphrase: None,
    });
    jump.user = "keyonly".to_owned();
    let dialogs = Dialogs::default();
    let session = connect_profile(profile(&known_hosts, jump), &dialogs)
        .await
        .unwrap();
    assert_eq!(session.root().as_str(), "/home/tester");
    assert_eq!(dialogs.secrets_asked(), []);
}

/// NFR-SEC-01 has a place for the jump host's password only.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_jump_keys_passphrase_is_not_offered_remember_and_is_kept_for_the_mount() {
    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(Auth::KeyFile {
        path: key_path("id_ed25519_pass"),
        passphrase: None,
    });
    jump.user = "keyonly".to_owned();
    let connector = SftpConnector::new(profile(&known_hosts, jump));
    // A dialog that says *Remember* anyway changes nothing.
    let dialogs = Dialogs::typing(&[(PASSPHRASE, Remember::Yes)]);

    connect(&connector, &dialogs).await.unwrap();
    assert_eq!(
        dialogs.secrets_asked(),
        [SecretRequest {
            kind: SecretKind::Passphrase {
                key_name: "id_ed25519_pass".to_owned()
            },
            allow_remember: false,
        }]
    );
    assert!(connector.take_remembered_jump().is_none());
    assert!(connector.take_remembered().is_none());

    let silent = Dialogs::default();
    connect(&connector, &silent).await.unwrap();
    assert_eq!(silent.secrets_asked(), []);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_targets_passphrase_can_still_be_remembered() {
    let (_dir, known_hosts) = empty_dir();
    let mut profile = profile(&known_hosts, jump_host(password("tester")));
    profile.user = "keyonly".to_owned();
    profile.auth = Auth::KeyFile {
        path: key_path("id_ed25519_pass"),
        passphrase: None,
    };
    let connector = SftpConnector::new(profile);
    let dialogs = Dialogs::typing(&[(PASSPHRASE, Remember::Yes)]);

    connect(&connector, &dialogs).await.unwrap();
    assert!(dialogs.secrets_asked()[0].allow_remember);
    let remembered = connector.take_remembered().expect("the passphrase");
    assert_eq!(remembered.expose_secret(), PASSPHRASE);
    assert!(connector.take_remembered_jump().is_none());
}

// ---- The jump host cannot open the channel. ----

async fn expect_target_unreachable(mut profile: SftpProfile, host: &str, port: u16) {
    profile.host = host.to_owned();
    profile.port = port;
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert_eq!(
        error,
        ConnectError::HostUnreachable {
            host: host.to_owned(),
            port
        }
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_jump_host_that_does_not_forward_leaves_the_target_unreachable() {
    let (_dir, known_hosts) = empty_dir();
    let mut jump = jump_host(password("tester"));
    jump.port = NO_FORWARD_PORT;
    expect_target_unreachable(profile(&known_hosts, jump), TARGET, TARGET_PORT).await;
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_refused_port_behind_the_jump_host_is_unreachable() {
    let (_dir, known_hosts) = empty_dir();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    expect_target_unreachable(profile, TARGET, 2).await;
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn an_unknown_name_behind_the_jump_host_is_unreachable() {
    let (_dir, known_hosts) = empty_dir();
    let profile = profile(&known_hosts, jump_host(password("tester")));
    expect_target_unreachable(profile, "no-such-target.invalid", TARGET_PORT).await;
}

/// TEST-NET-1 either drops packets or has no route, and the jump host's own limit is minutes.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_silent_address_behind_the_jump_host_is_unreachable_within_the_limit() {
    let (_dir, known_hosts) = empty_dir();
    let mut profile = profile(&known_hosts, jump_host(password("tester")));
    profile.timeout = Duration::from_secs(2);
    let start = Instant::now();
    expect_target_unreachable(profile, "192.0.2.1", TARGET_PORT).await;
    assert!(start.elapsed() < Duration::from_secs(10));
}

// ---- Losing the jump host. ----

fn profile_through(proxy: &Proxy, known_hosts: &Path) -> SftpProfile {
    let mut jump = jump_host(password("tester"));
    jump.host = "127.0.0.1".to_owned();
    jump.port = proxy.port;
    profile(known_hosts, jump)
}

async fn connection_ends_within(proxy: &Proxy, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !proxy.has_connections() {
            return true;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The connection to the jump host lasts as long as the session, and no longer.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn the_jump_connection_stays_for_the_session_and_ends_with_it() {
    let (_dir, known_hosts) = empty_dir();
    let proxy = Proxy::to(JUMP_PORT).await;
    let session = connect_profile(profile_through(&proxy, &known_hosts), &Dialogs::default())
        .await
        .unwrap();
    time::sleep(Duration::from_secs(1)).await;
    assert!(proxy.has_connections());
    session.sftp().realpath(".").await.unwrap();

    drop(session);
    assert!(connection_ends_within(&proxy, Duration::from_secs(5)).await);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_failed_target_login_does_not_leave_the_jump_connection_open() {
    let (_dir, known_hosts) = empty_dir();
    let proxy = Proxy::to(JUMP_PORT).await;
    let mut profile = profile_through(&proxy, &known_hosts);
    profile.auth = password("wrong");
    let error = expect_error(connect_profile(profile, &Dialogs::default()).await);
    assert_eq!(error, auth_error("tester", TARGET));
    assert!(connection_ends_within(&proxy, Duration::from_secs(5)).await);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn losing_the_jump_host_closes_the_session() {
    let (_dir, known_hosts) = empty_dir();
    let proxy = Proxy::to(JUMP_PORT).await;
    let session = connect_profile(profile_through(&proxy, &known_hosts), &Dialogs::default())
        .await
        .unwrap();
    assert!(!session.is_closed());

    proxy.set(Link::Cut);
    assert!(closes_within(&session, Duration::from_secs(5)).await);
    assert!(session.sftp().realpath(".").await.is_err());
}

/// One setting covers both connections, and neither can get a reply through a stalled link.
#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_stalled_jump_host_is_noticed_by_keep_alives() {
    let (_dir, known_hosts) = empty_dir();
    let proxy = Proxy::to(JUMP_PORT).await;
    let mut profile = profile_through(&proxy, &known_hosts);
    profile.keepalive_secs = 1;
    let session = connect_profile(profile, &Dialogs::default()).await.unwrap();

    // A healthy tunnel stays up past several intervals.
    assert!(!closes_within(&session, Duration::from_secs(3)).await);

    proxy.set(Link::Stall);
    assert!(closes_within(&session, Duration::from_secs(10)).await);
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn losing_the_jump_host_during_the_targets_handshake_is_a_lost_connection() {
    let (_dir, known_hosts) = empty_dir();
    let proxy = std::sync::Arc::new(Proxy::to(JUMP_PORT).await);
    let cutter = std::sync::Arc::clone(&proxy);
    let dialogs = Dialogs {
        while_asked: Some(Box::new(move |host| {
            if host == TARGET {
                cutter.set(Link::Cut);
            }
        })),
        ..Dialogs::default()
    };
    let profile = profile_through(&proxy, &known_hosts);
    let error = expect_error(connect_profile(profile, &dialogs).await);
    assert_eq!(error, ConnectError::ConnectionLost);
}
