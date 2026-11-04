// SPDX-License-Identifier: GPL-3.0-or-later

//! `SftpConnector`: every error class and host key outcome.
//!
//! Failures that a local socket can produce run everywhere. The rest need the S-03 servers and are
//! ignored by default:
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p fsm-sftp --test connector -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{ED, ED_FP, ED2, ED2_FP, RSA, RSA_FP, host_key};
use fsm_sftp::connector::{Auth, SftpConnector, SftpProfile, SftpSession};
use fsm_vfs::{
    ConnectError, HostKeyDecision, HostKeyRequest, KbdIntRequest, Prompter, Remember, Secret,
    SecretRequest, async_trait,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time;

const HOST: &str = "localhost";
const PORT: u16 = 2222;
const NO_SFTP_PORT: u16 = 2223;
const TWO_KEYS_PORT: u16 = 2224;
const LEGACY_PORT: u16 = 2225;
const BROKEN_SFTP_PORT: u16 = 2226;
const HUNG_SFTP_PORT: u16 = 2227;
/// So a broken test fails rather than hangs.
const TEST_LIMIT: Duration = Duration::from_secs(60);

/// Answers host key questions and remembers them.
struct Answers {
    decision: HostKeyDecision,
    asked: Mutex<Vec<HostKeyRequest>>,
    while_asked: Box<dyn Fn() + Send + Sync>,
    thinking: Duration,
}

impl Answers {
    fn with(decision: HostKeyDecision) -> Self {
        Self {
            decision,
            asked: Mutex::new(Vec::new()),
            while_asked: Box::new(|| {}),
            thinking: Duration::ZERO,
        }
    }

    fn asked(&self) -> Vec<HostKeyRequest> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Prompter for Answers {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        panic!("no secret should be asked for")
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        panic!("no keyboard-interactive round should be asked for")
    }

    async fn host_key(&self, request: HostKeyRequest) -> HostKeyDecision {
        self.asked.lock().unwrap().push(request);
        (self.while_asked)();
        time::sleep(self.thinking).await;
        self.decision
    }
}

/// Never answers, like a dialog nobody has closed.
struct OpenDialog {
    asked: AtomicUsize,
}

#[async_trait]
impl Prompter for OpenDialog {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        None
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        self.asked.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

fn profile(host: &str, port: u16, known_hosts: &Path) -> SftpProfile {
    SftpProfile {
        host: host.to_owned(),
        port,
        user: "tester".to_owned(),
        auth: Auth::Password(Secret::new("tester".to_owned())),
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: Duration::from_secs(15),
        known_hosts: known_hosts.to_owned(),
    }
}

async fn connect_with(
    profile: SftpProfile,
    prompter: &dyn Prompter,
) -> Result<SftpSession, ConnectError> {
    let connector = SftpConnector::new(profile);
    time::timeout(TEST_LIMIT, connector.connect_session(prompter))
        .await
        .expect("connect_session must end by itself")
}

fn known_hosts_with(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
    let path = dir.path().join("known_hosts");
    fs::write(&path, contents).unwrap();
    path
}

fn empty_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    (dir, path)
}

fn expect_error(result: Result<SftpSession, ConnectError>) -> ConnectError {
    match result {
        Ok(session) => panic!("connected to {}", session.root()),
        Err(error) => error,
    }
}

fn server_key(file: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "../../tests/docker/keys", file]
        .iter()
        .collect();
    let line = fs::read_to_string(path).expect("run tests/docker/servers.ps1 up first");
    let mut words = line.split_whitespace();
    format!("{} {}", words.next().unwrap(), words.next().unwrap())
}

fn server_ed25519() -> String {
    server_key("ssh_host_ed25519_key.pub")
}

fn server_rsa() -> String {
    server_key("ssh_host_rsa_key.pub")
}

/// FR-SSH-09: whether real `ssh` passes the host key check using this file, without asking.
fn ssh_accepts(file: &Path) -> bool {
    let known_hosts = format!("UserKnownHostsFile={}", file.to_str().unwrap());
    let output = Command::new("ssh")
        .args(["-F", "none", "-p", "2222"])
        .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"])
        .args(["-o", "PreferredAuthentications=none"])
        .args(["-o", "GlobalKnownHostsFile=none", "-o", "CheckHostIP=no"])
        .args([
            "-o",
            "UpdateHostKeys=no",
            "-o",
            &known_hosts,
            "nobody@localhost",
        ])
        .output()
        .expect("ssh must be on PATH");
    // Offering no credentials, passing the check ends in this message.
    String::from_utf8_lossy(&output.stderr).contains("Permission denied")
}

// ---- A TCP proxy that can stop forwarding. ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Forward,
    /// Holds all data back, keeping TCP open: a pulled cable, as the client sees it.
    Stall,
    Cut,
}

struct Proxy {
    port: u16,
    link: watch::Sender<Link>,
}

impl Proxy {
    async fn to(server_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (link, state) = watch::channel(Link::Forward);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect((HOST, server_port)).await.unwrap();
                let (from_client, to_client) = client.into_split();
                let (from_server, to_server) = server.into_split();
                tokio::spawn(forward(from_client, to_server, state.clone()));
                tokio::spawn(forward(from_server, to_client, state.clone()));
            }
        });
        Self { port, link }
    }

    fn set(&self, state: Link) {
        self.link.send_replace(state);
    }
}

async fn forward(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    state: watch::Receiver<Link>,
) {
    let mut cut = state.clone();
    let copy = async move {
        let mut state = state;
        let mut buffer = vec![0; 64 * 1024];
        while let Ok(count @ 1..) = from.read(&mut buffer).await {
            if state.wait_for(|link| *link == Link::Forward).await.is_err()
                || to.write_all(&buffer[..count]).await.is_err()
            {
                return;
            }
        }
    };
    tokio::select! {
        () = copy => {}
        _ = cut.wait_for(|link| *link == Link::Cut) => {}
    }
}

// ---- Failures before SSH starts, or from a peer that is not an SSH server. No server needed. ----

/// Accepts connections and runs `peer` on each.
async fn local_peer<F, Fut>(peer: F) -> u16
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(peer(stream));
        }
    });
    port
}

async fn expect_local_error(port: u16) -> ConnectError {
    let (_dir, path) = empty_dir();
    let mut profile = profile("127.0.0.1", port, &path);
    profile.timeout = Duration::from_millis(500);
    expect_error(connect_with(profile, &Answers::with(HostKeyDecision::Reject)).await)
}

#[tokio::test]
async fn an_unknown_name_is_a_dns_error() {
    let (_dir, path) = empty_dir();
    let profile = profile("no-such-host.invalid", 22, &path);
    let error = expect_error(connect_with(profile, &Answers::with(HostKeyDecision::Reject)).await);
    assert_eq!(
        error,
        ConnectError::Dns {
            host: "no-such-host.invalid".to_owned()
        }
    );
}

#[tokio::test]
async fn a_refused_port_is_unreachable() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    assert_eq!(
        expect_local_error(port).await,
        ConnectError::HostUnreachable {
            host: "127.0.0.1".to_owned(),
            port
        }
    );
}

/// TEST-NET-1 either drops packets or has no route; both must end within the limit, as *cannot
/// reach*, well before Windows' own 21 s.
#[tokio::test]
async fn an_address_that_never_answers_is_unreachable_in_time() {
    let (_dir, path) = empty_dir();
    let mut profile = profile("192.0.2.1", 22, &path);
    profile.timeout = Duration::from_secs(1);
    let start = Instant::now();
    let error = expect_error(connect_with(profile, &Answers::with(HostKeyDecision::Reject)).await);
    assert_eq!(
        error,
        ConnectError::HostUnreachable {
            host: "192.0.2.1".to_owned(),
            port: 22
        }
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_silent_peer_times_out() {
    let port = local_peer(|mut stream| async move {
        let mut buffer = [0; 256];
        while matches!(stream.read(&mut buffer).await, Ok(1..)) {}
    })
    .await;
    assert_eq!(
        expect_local_error(port).await,
        ConnectError::Timeout {
            host: "127.0.0.1".to_owned()
        }
    );
}

#[tokio::test]
async fn silence_after_the_ssh_banner_times_out() {
    let port = local_peer(|mut stream| async move {
        stream.write_all(b"SSH-2.0-silent\r\n").await.unwrap();
        let mut buffer = [0; 256];
        while matches!(stream.read(&mut buffer).await, Ok(1..)) {}
    })
    .await;
    assert_eq!(
        expect_local_error(port).await,
        ConnectError::Timeout {
            host: "127.0.0.1".to_owned()
        }
    );
}

#[tokio::test]
async fn a_peer_that_closes_at_once_is_a_lost_connection() {
    let port = local_peer(|stream| async move { drop(stream) }).await;
    assert_eq!(expect_local_error(port).await, ConnectError::ConnectionLost);
}

/// Closing with our banner unread is a reset. Reading it first is a clean close, which `russh`
/// reports differently.
#[tokio::test]
async fn a_peer_that_reads_and_then_closes_is_a_lost_connection() {
    let port = local_peer(|mut stream| async move {
        let mut buffer = [0; 256];
        let _ = stream.read(&mut buffer).await;
        stream.shutdown().await.unwrap();
        while matches!(stream.read(&mut buffer).await, Ok(1..)) {}
    })
    .await;
    assert_eq!(expect_local_error(port).await, ConnectError::ConnectionLost);
}

/// `localhost` also resolves to `::1`, where nothing listens. Getting as far as this peer, whatever
/// the order of the addresses, means no failed address ended the attempt.
#[tokio::test]
async fn a_dead_address_does_not_stop_the_next_one_being_tried() {
    let port = local_peer(|stream| async move { drop(stream) }).await;
    let (_dir, path) = empty_dir();
    let profile = profile(HOST, port, &path);
    let error = expect_error(connect_with(profile, &Answers::with(HostKeyDecision::Reject)).await);
    assert_eq!(error, ConnectError::ConnectionLost);
}

/// A web server answers the banner with an error page and closes, so this looks like the above.
#[tokio::test]
async fn a_peer_that_answers_something_else_and_closes_is_a_lost_connection() {
    let port = local_peer(|mut stream| async move {
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .unwrap();
    })
    .await;
    assert_eq!(expect_local_error(port).await, ConnectError::ConnectionLost);
}

#[tokio::test]
async fn a_peer_that_keeps_talking_something_else_does_not_provide_sftp() {
    let port = local_peer(|mut stream| async move {
        let line = format!("{}\r\n", "x".repeat(100));
        while stream.write_all(line.as_bytes()).await.is_ok() {}
    })
    .await;
    assert_eq!(
        expect_local_error(port).await,
        ConnectError::SftpUnavailable
    );
}

/// Treat every `known_hosts` problem as not trusted, before anything reaches the network.
#[tokio::test]
async fn an_unreadable_known_hosts_refuses_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let profile = profile("no-such-host.invalid", 22, dir.path());
    let error = expect_error(connect_with(profile, &Answers::with(HostKeyDecision::Reject)).await);
    assert!(matches!(error, ConnectError::Other(_)), "{error:?}");
}

// ---- Host key outcomes against the server. ----

/// FR-SSH-09, the whole acceptance criterion.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn trust_and_save_is_asked_once_and_real_ssh_agrees() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustSave);
    connect_with(profile(HOST, PORT, &path), &answers)
        .await
        .unwrap();

    let key = host_key(&server_ed25519());
    assert_eq!(
        answers.asked(),
        [HostKeyRequest {
            host: HOST.to_owned(),
            port: PORT,
            key_type: "ssh-ed25519".to_owned(),
            fingerprint: key.fingerprint(),
        }]
    );
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .starts_with("[localhost]:2222 ssh-ed25519 ")
    );

    let again = Answers::with(HostKeyDecision::Reject);
    connect_with(profile(HOST, PORT, &path), &again)
        .await
        .unwrap();
    assert!(again.asked().is_empty());
    assert!(ssh_accepts(&path));
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn trust_once_connects_and_saves_nothing() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    connect_with(profile(HOST, PORT, &path), &answers)
        .await
        .unwrap();
    assert_eq!(answers.asked().len(), 1);
    assert!(!path.exists());

    connect_with(profile(HOST, PORT, &path), &answers)
        .await
        .unwrap();
    assert_eq!(answers.asked().len(), 2, "asked again next time");
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn cancel_refuses_and_saves_nothing() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::Reject);
    let error = expect_error(connect_with(profile(HOST, PORT, &path), &answers).await);
    assert_eq!(error, ConnectError::HostKeyRejected);
    assert!(!path.exists());
}

/// FR-SSH-10: refused without a question, naming both keys and the line.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_changed_key_is_refused_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let path = known_hosts_with(&dir, &format!("other.test {ED}\n[localhost]:2222 {ED2}\n"));
    let before = fs::read(&path).unwrap();
    let answers = Answers::with(HostKeyDecision::TrustSave);
    let error = expect_error(connect_with(profile(HOST, PORT, &path), &answers).await);

    let ConnectError::HostKeyChanged(details) = error else {
        panic!("{error:?}")
    };
    assert_eq!(details.host, HOST);
    assert_eq!(details.port, PORT);
    assert_eq!(details.line, 2);
    assert_eq!(details.old_fingerprint, ED2_FP);
    assert_eq!(
        details.new_fingerprint,
        host_key(&server_ed25519()).fingerprint()
    );
    assert_eq!(details.known_hosts, path);
    assert!(answers.asked().is_empty());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_revoked_key_is_refused_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let server = server_ed25519();
    let path = known_hosts_with(
        &dir,
        &format!("[localhost]:2222 {server}\n@revoked [localhost]:2222 {server}\n"),
    );
    let answers = Answers::with(HostKeyDecision::TrustSave);
    let error = expect_error(connect_with(profile(HOST, PORT, &path), &answers).await);

    let ConnectError::HostKeyRevoked(details) = error else {
        panic!("{error:?}")
    };
    assert_eq!(details.line, 2);
    assert_eq!(details.fingerprint, host_key(&server).fingerprint());
    assert_eq!(details.known_hosts, path);
    assert!(answers.asked().is_empty());
}

/// Spec §6.3: another program saved a different key while the dialog was open.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_key_saved_elsewhere_during_the_dialog_refuses_under_either_answer() {
    for decision in [HostKeyDecision::TrustSave, HostKeyDecision::TrustOnce] {
        let (_dir, path) = empty_dir();
        let mut answers = Answers::with(decision);
        let file = path.clone();
        let line = format!("[localhost]:2222 {ED}\n");
        answers.while_asked = Box::new(move || fs::write(&file, &line).unwrap());
        let error = expect_error(connect_with(profile(HOST, PORT, &path), &answers).await);

        let ConnectError::HostKeyChanged(details) = error else {
            panic!("{decision:?}: {error:?}")
        };
        assert_eq!(details.old_fingerprint, ED_FP);
        assert_eq!(details.line, 1);
    }
}

/// `append_if_unknown` failing must not count as trusted.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_key_that_cannot_be_saved_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    // The folder for the file is a file.
    let blocker = dir.path().join("not-a-folder");
    fs::write(&blocker, "").unwrap();
    let path = blocker.join("known_hosts");
    // Reading finds nothing there, or fails; saving must fail either way.
    let answers = Answers::with(HostKeyDecision::TrustSave);
    let error = expect_error(connect_with(profile(HOST, PORT, &path), &answers).await);
    assert!(matches!(error, ConnectError::Other(_)), "{error:?}");
}

/// The reason for asking the server for the key types on file first: without that, this server
/// would answer with its Ed25519 key, which would count as changed, with no way out.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_key_type_on_file_is_asked_for_first() {
    let dir = tempfile::tempdir().unwrap();
    let server = server_rsa();
    let path = known_hosts_with(&dir, &format!("[localhost]:2224 {server}\n"));
    let answers = Answers::with(HostKeyDecision::Reject);
    connect_with(profile(HOST, TWO_KEYS_PORT, &path), &answers)
        .await
        .unwrap();
    assert!(answers.asked().is_empty());
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_different_key_of_the_type_on_file_is_changed() {
    let dir = tempfile::tempdir().unwrap();
    let path = known_hosts_with(&dir, &format!("[localhost]:2224 {RSA}\n"));
    let answers = Answers::with(HostKeyDecision::TrustSave);
    let error = expect_error(connect_with(profile(HOST, TWO_KEYS_PORT, &path), &answers).await);
    let ConnectError::HostKeyChanged(details) = error else {
        panic!("{error:?}")
    };
    assert_eq!(details.old_fingerprint, RSA_FP);
    assert_eq!(
        details.new_fingerprint,
        host_key(&server_rsa()).fingerprint()
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn with_nothing_on_file_ed25519_is_preferred() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    connect_with(profile(HOST, TWO_KEYS_PORT, &path), &answers)
        .await
        .unwrap();
    assert_eq!(answers.asked()[0].key_type, "ssh-ed25519");
}

// ---- The dialog and the clock. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn time_in_the_dialog_does_not_count_against_the_timeout() {
    let (_dir, path) = empty_dir();
    let mut profile = profile(HOST, PORT, &path);
    profile.timeout = Duration::from_secs(5);
    let mut answers = Answers::with(HostKeyDecision::TrustOnce);
    answers.thinking = Duration::from_secs(7);
    connect_with(profile, &answers).await.unwrap();
}

/// FR-MNT-11: *Cancel* drops the future, which must not wait for the dialog.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn dropping_the_call_during_the_dialog_ends_it() {
    let (_dir, path) = empty_dir();
    let dialog = OpenDialog {
        asked: AtomicUsize::new(0),
    };
    let connector = SftpConnector::new(profile(HOST, PORT, &path));
    let cancelled = time::timeout(Duration::from_secs(3), connector.connect_session(&dialog)).await;
    assert!(cancelled.is_err(), "the dialog never answers");
    assert_eq!(dialog.asked.load(Ordering::SeqCst), 1);
    assert!(!path.exists());
}

/// A server gives up on a login after a while (`LoginGraceTime`), which a long dialog can reach.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_connection_lost_during_the_dialog_is_reported() {
    let (_dir, path) = empty_dir();
    let proxy = Arc::new(Proxy::to(PORT).await);
    let mut answers = Answers::with(HostKeyDecision::TrustOnce);
    let cut = Arc::clone(&proxy);
    answers.while_asked = Box::new(move || cut.set(Link::Cut));
    let error = expect_error(connect_with(profile("127.0.0.1", proxy.port, &path), &answers).await);
    assert_eq!(error, ConnectError::ConnectionLost);
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn silence_after_the_dialog_times_out() {
    let (_dir, path) = empty_dir();
    let proxy = Arc::new(Proxy::to(PORT).await);
    let mut profile = profile("127.0.0.1", proxy.port, &path);
    profile.timeout = Duration::from_secs(2);
    let mut answers = Answers::with(HostKeyDecision::TrustOnce);
    let stall = Arc::clone(&proxy);
    answers.while_asked = Box::new(move || stall.set(Link::Stall));
    let error = expect_error(connect_with(profile, &answers).await);
    assert_eq!(
        error,
        ConnectError::Timeout {
            host: "127.0.0.1".to_owned()
        }
    );
}

// ---- The other error classes against the server. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_wrong_password_is_an_auth_error() {
    let (_dir, path) = empty_dir();
    let mut profile = profile(HOST, PORT, &path);
    profile.auth = Auth::Password(Secret::new("wrong".to_owned()));
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    let error = expect_error(connect_with(profile, &answers).await);
    assert_eq!(
        error,
        ConnectError::Auth {
            user: "tester".to_owned(),
            host: HOST.to_owned()
        }
    );
}

/// Spec §8.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_server_with_only_sha1_host_key_signatures_is_refused_clearly() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    let error = expect_error(connect_with(profile(HOST, LEGACY_PORT, &path), &answers).await);
    assert_eq!(error, ConnectError::NoCommonAlgorithm);
    assert!(answers.asked().is_empty());
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_refused_subsystem_is_sftp_unavailable_at_once() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    let start = Instant::now();
    let error = expect_error(connect_with(profile(HOST, NO_SFTP_PORT, &path), &answers).await);
    assert_eq!(error, ConnectError::SftpUnavailable);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_subsystem_that_exits_at_once_is_sftp_unavailable() {
    let (_dir, path) = empty_dir();
    let answers = Answers::with(HostKeyDecision::TrustOnce);
    let start = Instant::now();
    let error = expect_error(connect_with(profile(HOST, BROKEN_SFTP_PORT, &path), &answers).await);
    assert_eq!(error, ConnectError::SftpUnavailable);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_subsystem_that_never_answers_times_out() {
    let (_dir, path) = empty_dir();
    let mut profile = profile(HOST, HUNG_SFTP_PORT, &path);
    profile.timeout = Duration::from_secs(2);
    let error =
        expect_error(connect_with(profile, &Answers::with(HostKeyDecision::TrustOnce)).await);
    assert_eq!(
        error,
        ConnectError::Timeout {
            host: HOST.to_owned()
        }
    );
}

async fn connect_to_path(remote_path: &str) -> Result<SftpSession, ConnectError> {
    let (_dir, path) = empty_dir();
    let mut profile = profile(HOST, PORT, &path);
    profile.remote_path = remote_path.to_owned();
    connect_with(profile, &Answers::with(HostKeyDecision::TrustOnce)).await
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn remote_paths_are_resolved_and_checked() {
    for (wanted, root) in [
        ("~", "/home/tester"),
        ("~/.ssh", "/home/tester/.ssh"),
        ("/tmp", "/tmp"),
        ("/", "/"),
        ("/usr/../tmp", "/tmp"),
    ] {
        let session = connect_to_path(wanted).await.unwrap();
        assert_eq!(session.root().as_str(), root, "{wanted}");
    }
    for wanted in [
        "/no/such",
        "~/no-such",
        "relative",
        "",
        // Exists, but this user may not look.
        "/root/.ssh",
    ] {
        assert_eq!(
            expect_error(connect_to_path(wanted).await),
            ConnectError::PathNotFound {
                path: wanted.to_owned()
            }
        );
    }
    for wanted in ["/etc/passwd", "~/.bashrc"] {
        assert_eq!(
            expect_error(connect_to_path(wanted).await),
            ConnectError::PathNotDir {
                path: wanted.to_owned()
            }
        );
    }
}

/// FR-SSH-14, the part that is here. A server without these is ProFTPD's, in M1-10.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_session_works_and_knows_the_servers_limits_and_extensions() {
    let session = connect_to_path("~").await.unwrap();
    let limits = session.limits();
    assert!(limits.read_len.is_some_and(|len| len >= 32 * 1024));
    assert!(limits.write_len.is_some_and(|len| len >= 32 * 1024));
    assert!(session.has_extension("limits@openssh.com"));
    assert!(session.has_extension("posix-rename@openssh.com"));
    assert!(!session.has_extension("no-such@example.test"));

    let name = session.sftp().realpath(".").await.unwrap();
    assert_eq!(name.files[0].filename, "/home/tester");
    assert!(!session.is_closed());
}

// ---- Keep-alive (FR-SSH-12). ----

async fn keepalive_session(keepalive_secs: u64) -> (Proxy, SftpSession, tempfile::TempDir) {
    let (dir, path) = empty_dir();
    let proxy = Proxy::to(PORT).await;
    let mut profile = profile("127.0.0.1", proxy.port, &path);
    profile.keepalive_secs = keepalive_secs;
    let session = connect_with(profile, &Answers::with(HostKeyDecision::TrustOnce))
        .await
        .unwrap();
    (proxy, session, dir)
}

async fn closed_within(session: &SftpSession, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if session.is_closed() {
            return Some(start.elapsed());
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// One keep-alive per second: three go unanswered, and the next due one ends it, so 4 s.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn unanswered_keep_alives_close_the_connection() {
    let (proxy, session, _dir) = keepalive_session(1).await;
    proxy.set(Link::Stall);
    let closed = closed_within(&session, Duration::from_secs(10))
        .await
        .expect("must notice a dead link");
    assert!(
        closed >= Duration::from_secs(3) && closed <= Duration::from_secs(6),
        "{closed:?}"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn answered_keep_alives_keep_an_idle_connection_open() {
    let (_proxy, session, _dir) = keepalive_session(1).await;
    assert_eq!(closed_within(&session, Duration::from_secs(6)).await, None);
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn with_keep_alives_off_a_dead_link_goes_unnoticed() {
    let (proxy, session, _dir) = keepalive_session(0).await;
    proxy.set(Link::Stall);
    assert_eq!(closed_within(&session, Duration::from_secs(6)).await, None);
}
