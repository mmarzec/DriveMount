// SPDX-License-Identifier: GPL-3.0-or-later

//! Logging in with each method against the S-03 server (FR-SSH-01 to FR-SSH-07).
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p fsm-sftp --test auth -- --ignored
//! ```
//!
//! Agent tests talk to an in-process agent from `russh`, on a named pipe on Windows and a Unix socket
//! elsewhere, so they do not touch the user's own agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fsm_sftp::connector::{AgentSource, Auth, JumpHost, SftpConnector, SftpProfile, SftpSession};
use fsm_vfs::{
    ConnectError, HostKeyDecision, HostKeyRequest, KbdIntPrompt, KbdIntRequest, LoginAnswer,
    LoginRequest, Prompter, Remember, Secret, SecretKind, SecretRequest, async_trait,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::server;
use sha1::Sha1;
use tokio::time;

const HOST: &str = "localhost";
const PORT: u16 = 2222;
const TIMEOUT: Duration = Duration::from_secs(15);
/// So a broken test fails rather than hangs.
const TEST_LIMIT: Duration = Duration::from_secs(60);
const PASSPHRASE: &str = "test-passphrase";
const TOTP_PASSWORD: &str = "totp-pass";
/// `JBSWY3DPEHPK3PXP`, decoded.
const TOTP_SECRET: &[u8] = b"Hello!\xde\xad\xbe\xef";

fn key_path(file: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "../../tests/docker/keys", file]
        .iter()
        .collect()
}

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

fn secret(text: &str) -> Secret<String> {
    Secret::new(text.to_owned())
}

type KbdRound = fn(&KbdIntPrompt) -> String;

/// Answers dialogs from a script and remembers what it was asked. A dialog with no answer left
/// counts as *Cancel*.
#[derive(Default)]
struct Dialogs {
    secrets: Mutex<VecDeque<(String, Remember)>>,
    logins: Mutex<VecDeque<LoginAnswer>>,
    keyboard: Option<KbdRound>,
    secrets_asked: Mutex<Vec<SecretRequest>>,
    logins_asked: AtomicUsize,
    keyboard_asked: Mutex<Vec<Vec<String>>>,
    thinking: Duration,
}

impl Dialogs {
    fn nothing() -> Self {
        Self::default()
    }

    fn typing(text: &str, remember: Remember) -> Self {
        let dialogs = Self::default();
        dialogs
            .secrets
            .lock()
            .unwrap()
            .push_back((text.to_owned(), remember));
        dialogs
    }

    fn logging_in_with(answer: LoginAnswer) -> Self {
        let dialogs = Self::default();
        dialogs.logins.lock().unwrap().push_back(answer);
        dialogs
    }

    fn secrets_asked(&self) -> Vec<SecretRequest> {
        self.secrets_asked.lock().unwrap().clone()
    }

    fn keyboard_asked(&self) -> Vec<Vec<String>> {
        self.keyboard_asked.lock().unwrap().clone()
    }

    fn logins_asked(&self) -> usize {
        self.logins_asked.load(Ordering::SeqCst)
    }

    fn assert_silent(&self) {
        assert_eq!(self.secrets_asked(), [], "no secret prompt");
        assert_eq!(
            self.keyboard_asked(),
            Vec::<Vec<String>>::new(),
            "no question"
        );
        assert_eq!(self.logins_asked(), 0, "no login prompt");
    }
}

#[async_trait]
impl Prompter for Dialogs {
    async fn secret(&self, request: SecretRequest) -> Option<(Secret<String>, Remember)> {
        self.secrets_asked.lock().unwrap().push(request);
        time::sleep(self.thinking).await;
        let (text, remember) = self.secrets.lock().unwrap().pop_front()?;
        Some((Secret::new(text), remember))
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        self.logins_asked.fetch_add(1, Ordering::SeqCst);
        self.logins.lock().unwrap().pop_front()
    }

    async fn keyboard_interactive(&self, request: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        let texts = request.prompts.iter().map(|p| p.text.clone()).collect();
        self.keyboard_asked.lock().unwrap().push(texts);
        let answer = self.keyboard?;
        Some(
            request
                .prompts
                .iter()
                .map(|prompt| Secret::new(answer(prompt)))
                .collect(),
        )
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::TrustOnce
    }
}

/// A connector with an empty `known_hosts`, which the [`TempDir`](tempfile::TempDir) keeps alive.
fn connector(user: &str, auth: Auth) -> (tempfile::TempDir, SftpConnector) {
    let dir = tempfile::tempdir().unwrap();
    let profile = SftpProfile {
        host: HOST.to_owned(),
        port: PORT,
        user: user.to_owned(),
        auth,
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: TIMEOUT,
        known_hosts: dir.path().join("known_hosts"),
        jump: None,
    };
    (dir, SftpConnector::new(profile))
}

async fn connect(
    connector: &SftpConnector,
    dialogs: &Dialogs,
) -> Result<SftpSession, ConnectError> {
    time::timeout(TEST_LIMIT, connector.connect_session(dialogs))
        .await
        .expect("connect_session must end by itself")
}

/// Logging in reached the SFTP setup, which checks the home folder.
async fn assert_logs_in(connector: &SftpConnector, dialogs: &Dialogs, user: &str) {
    let session = connect(connector, dialogs).await.unwrap();
    assert_eq!(session.root().as_str(), format!("/home/{user}"));
}

async fn error(connector: &SftpConnector, dialogs: &Dialogs) -> ConnectError {
    match connect(connector, dialogs).await {
        Ok(session) => panic!("logged in to {}", session.root()),
        Err(error) => error,
    }
}

fn auth_error(user: &str) -> ConnectError {
    ConnectError::Auth {
        user: user.to_owned(),
        host: HOST.to_owned(),
    }
}

fn key_file(file: &str) -> Auth {
    Auth::KeyFile {
        path: key_path(file),
        passphrase: None,
    }
}

fn password_prompt(user: &str, allow_remember: bool) -> SecretRequest {
    SecretRequest {
        kind: SecretKind::Password {
            user: user.to_owned(),
            host: HOST.to_owned(),
        },
        allow_remember,
    }
}

fn passphrase_prompt(key_name: &str, allow_remember: bool) -> SecretRequest {
    SecretRequest {
        kind: SecretKind::Passphrase {
            key_name: key_name.to_owned(),
        },
        allow_remember,
    }
}

fn assert_remembered(connector: &SftpConnector, expected: &str) {
    let remembered = connector.take_remembered().expect("something to remember");
    assert_eq!(remembered.expose_secret(), expected);
    assert!(connector.take_remembered().is_none(), "taken once");
}

#[test]
fn connecting_can_run_on_another_task() {
    fn assert_send<T: Send>(_: &T) {}
    let (_dir, connector) = connector("tester", Auth::Ask);
    let dialogs = Dialogs::nothing();
    assert_send(&connector.connect_session(&dialogs));
}

// ---- Password (FR-SSH-01). ----

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_stored_password_logs_in_without_asking() {
    let (_dir, connector) = connector("tester", Auth::Password(Some(secret("tester"))));
    let dialogs = Dialogs::nothing();
    assert_logs_in(&connector, &dialogs, "tester").await;
    dialogs.assert_silent();
    assert!(connector.take_remembered().is_none());
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_missing_password_is_asked_for_once_and_not_on_reconnect() {
    let (_dir, connector) = connector("tester", Auth::Password(None));
    let dialogs = Dialogs::typing("tester", Remember::No);
    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_eq!(dialogs.secrets_asked(), [password_prompt("tester", true)]);
    assert!(
        connector.take_remembered().is_none(),
        "Remember was not ticked"
    );

    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_eq!(
        dialogs.secrets_asked().len(),
        1,
        "the reconnect asks nothing"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_remembered_password_is_handed_over_after_the_login() {
    let (_dir, connector) = connector("tester", Auth::Password(None));
    let dialogs = Dialogs::typing("tester", Remember::Yes);
    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_remembered(&connector, "tester");
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_wrong_typed_password_is_kept_nowhere_and_the_next_mount_asks_again() {
    let (_dir, connector) = connector("tester", Auth::Password(None));
    let dialogs = Dialogs::typing("wrong", Remember::Yes);
    assert_eq!(error(&connector, &dialogs).await, auth_error("tester"));
    assert!(connector.take_remembered().is_none());

    dialogs
        .secrets
        .lock()
        .unwrap()
        .push_back(("tester".to_owned(), Remember::No));
    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_eq!(dialogs.secrets_asked().len(), 2);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn cancelling_the_password_prompt_is_not_an_error() {
    let (_dir, connector) = connector("tester", Auth::Password(None));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::Cancelled);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn no_password_is_asked_for_when_the_server_takes_none() {
    let (_dir, connector) = connector("keyonly", Auth::Password(None));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn time_in_a_dialog_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let connector = SftpConnector::new(SftpProfile {
        host: HOST.to_owned(),
        port: PORT,
        user: "tester".to_owned(),
        auth: Auth::Password(None),
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: Duration::from_secs(2),
        known_hosts: dir.path().join("known_hosts"),
        jump: None,
    });
    let mut dialogs = Dialogs::typing("tester", Remember::No);
    dialogs.thinking = Duration::from_secs(4);
    assert_logs_in(&connector, &dialogs, "tester").await;
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn dropping_the_call_during_a_prompt_ends_it() {
    struct Open;

    #[async_trait]
    impl Prompter for Open {
        async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
            std::future::pending().await
        }

        async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
            None
        }

        async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
            None
        }

        async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
            HostKeyDecision::TrustOnce
        }
    }

    let (_dir, connector) = connector("tester", Auth::Password(None));
    let open = Open;
    let call = time::timeout(Duration::from_secs(2), connector.connect_session(&open));
    assert!(call.await.is_err(), "the dialog was still open");
}

// ---- Key files (FR-SSH-02, FR-SSH-03). ----

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn each_key_format_logs_in_without_asking() {
    // OpenSSH 9.2 takes no SHA-1 `ssh-rsa` signatures, so the RSA logins also show rsa-sha2.
    for file in ["id_ed25519", "id_ecdsa", "id_rsa", "id_rsa_pem"] {
        let (_dir, connector) = connector("keyonly", key_file(file));
        let dialogs = Dialogs::nothing();
        assert_logs_in(&connector, &dialogs, "keyonly").await;
        dialogs.assert_silent();
    }
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn each_encrypted_key_format_asks_once_and_not_on_reconnect() {
    for file in ["id_ed25519_pass", "id_rsa_pem_pass", "id_rsa_pkcs8_pass"] {
        let (_dir, connector) = connector("keyonly", key_file(file));
        let dialogs = Dialogs::typing(PASSPHRASE, Remember::No);
        assert_logs_in(&connector, &dialogs, "keyonly").await;
        assert_eq!(dialogs.secrets_asked(), [passphrase_prompt(file, true)]);
        assert!(connector.take_remembered().is_none());

        assert_logs_in(&connector, &dialogs, "keyonly").await;
        assert_eq!(
            dialogs.secrets_asked().len(),
            1,
            "{file}: the reconnect asks nothing"
        );
    }
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_stored_passphrase_opens_the_key_without_asking() {
    let auth = Auth::KeyFile {
        path: key_path("id_ed25519_pass"),
        passphrase: Some(secret(PASSPHRASE)),
    };
    let (_dir, connector) = connector("keyonly", auth);
    let dialogs = Dialogs::nothing();
    assert_logs_in(&connector, &dialogs, "keyonly").await;
    dialogs.assert_silent();
    assert!(
        connector.take_remembered().is_none(),
        "it is stored already"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_remembered_passphrase_is_handed_over() {
    let (_dir, connector) = connector("keyonly", key_file("id_ed25519_pass"));
    let dialogs = Dialogs::typing(PASSPHRASE, Remember::Yes);
    assert_logs_in(&connector, &dialogs, "keyonly").await;
    assert_remembered(&connector, PASSPHRASE);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_wrong_passphrase_is_an_auth_error_and_not_remembered() {
    let (_dir, connector) = connector("keyonly", key_file("id_ed25519_pass"));
    let dialogs = Dialogs::typing("wrong", Remember::Yes);
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
    assert!(connector.take_remembered().is_none());
    assert_eq!(dialogs.secrets_asked().len(), 1, "one prompt, no retry");
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn cancelling_the_passphrase_prompt_is_not_an_error() {
    let (_dir, connector) = connector("keyonly", key_file("id_ed25519_pass"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::Cancelled);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_key_the_server_does_not_know_is_an_auth_error() {
    let (_dir, connector) = connector("keyonly", key_file("id_cert"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_refused_key_does_not_fall_back_to_a_password_prompt() {
    let (_dir, connector) = connector("tester", key_file("id_cert"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, auth_error("tester"));
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_ppk_file_is_a_format_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.ppk");
    fs::write(&path, "PuTTY-User-Key-File-3: ssh-ed25519\n").unwrap();
    let auth = Auth::KeyFile {
        path,
        passphrase: None,
    };
    let (_dir, connector) = connector("keyonly", auth);
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::KeyFormat);
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_public_key_file_is_a_format_error() {
    let (_dir, connector) = connector("keyonly", key_file("id_ed25519.pub"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::KeyFormat);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_key_file_that_has_gone_is_an_auth_error() {
    let (_dir, connector) = connector("keyonly", key_file("no-such-key"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_key_is_not_tried_on_a_user_who_takes_no_keys_and_nothing_is_asked() {
    let (_dir, connector) = connector("totpuser", key_file("id_ed25519"));
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, auth_error("totpuser"));
    dialogs.assert_silent();
}

// ---- Certificates (FR-SSH-06). ----

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_certificate_next_to_the_key_is_offered() {
    // The RSA one would be refused if it were named or signed with SHA-1.
    for file in ["id_cert", "id_cert_rsa"] {
        let (_dir, connector) = connector("certuser", key_file(file));
        let dialogs = Dialogs::nothing();
        assert_logs_in(&connector, &dialogs, "certuser").await;
        dialogs.assert_silent();
    }
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_certificate_the_server_refuses_leaves_the_plain_key() {
    // Its principal is not this user, while the key itself is in `authorized_keys`.
    let (_dir, connector) = connector("keyonly", key_file("id_wrongcert"));
    assert_logs_in(&connector, &Dialogs::nothing(), "keyonly").await;
}

/// A private copy of a key in a folder of its own, with or without something as its certificate.
fn key_copy(dir: &Path, key: &str, certificate: Option<&[u8]>) -> PathBuf {
    let path = dir.join(key);
    fs::copy(key_path(key), &path).unwrap();
    if let Some(certificate) = certificate {
        fs::write(dir.join(format!("{key}-cert.pub")), certificate).unwrap();
    }
    path
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn without_its_certificate_the_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::KeyFile {
        path: key_copy(dir.path(), "id_cert", None),
        passphrase: None,
    };
    let (_known_hosts, connector) = connector("certuser", auth);
    assert_eq!(
        error(&connector, &Dialogs::nothing()).await,
        auth_error("certuser")
    );
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_certificate_that_cannot_be_used_leaves_the_plain_key() {
    let dir = tempfile::tempdir().unwrap();
    let other_key = fs::read(key_path("id_cert-cert.pub")).unwrap();
    for certificate in [&b"not a certificate"[..], &other_key] {
        let auth = Auth::KeyFile {
            path: key_copy(dir.path(), "id_ed25519", Some(certificate)),
            passphrase: None,
        };
        let (_known_hosts, connector) = connector("keyonly", auth);
        assert_logs_in(&connector, &Dialogs::nothing(), "keyonly").await;
    }
}

// ---- Agents (FR-SSH-04). ----

static AGENTS: AtomicUsize = AtomicUsize::new(0);

/// Starts an in-process agent holding these keys.
async fn agent_with(dir: &Path, keys: &[&str]) -> PathBuf {
    let number = AGENTS.fetch_add(1, Ordering::SeqCst);
    let path = agent_path(dir, number);
    serve_agent(&path);

    let mut client = open_agent(&path).await;
    for key in keys {
        let key = russh::keys::load_secret_key(key_path(key), None).unwrap();
        client.add_identity(&key, &[]).await.unwrap();
    }
    path
}

#[cfg(windows)]
fn agent_path(_: &Path, number: usize) -> PathBuf {
    let process = std::process::id();
    PathBuf::from(format!(r"\\.\pipe\fsm-test-agent-{process}-{number}"))
}

#[cfg(not(windows))]
fn agent_path(dir: &Path, number: usize) -> PathBuf {
    dir.join(format!("agent-{number}"))
}

#[cfg(windows)]
fn serve_agent(path: &Path) {
    use tokio::net::windows::named_pipe::ServerOptions;

    let path = path.to_owned();
    let first = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&path)
        .unwrap();
    let connections = futures::stream::unfold(first, move |pipe| {
        let path = path.clone();
        async move {
            pipe.connect().await.ok()?;
            let next = ServerOptions::new().create(&path).ok()?;
            Some((Ok(pipe), next))
        }
    });
    tokio::spawn(server::serve(Box::pin(connections), ()));
}

#[cfg(windows)]
async fn open_agent(path: &Path) -> AgentClient<tokio::net::windows::named_pipe::NamedPipeClient> {
    AgentClient::connect_named_pipe(path).await.unwrap()
}

#[cfg(not(windows))]
fn serve_agent(path: &Path) {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let connections = futures::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(stream, _)| stream);
        Some((accepted, listener))
    });
    tokio::spawn(server::serve(Box::pin(connections), ()));
}

#[cfg(not(windows))]
async fn open_agent(path: &Path) -> AgentClient<tokio::net::UnixStream> {
    AgentClient::connect_uds(path).await.unwrap()
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_key_in_the_agent_logs_in_without_asking() {
    for file in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_with(dir.path(), &[file]).await;
        let (_known_hosts, connector) = connector("keyonly", Auth::Agent(AgentSource::At(agent)));
        let dialogs = Dialogs::nothing();
        assert_logs_in(&connector, &dialogs, "keyonly").await;
        dialogs.assert_silent();
    }
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn the_agent_is_asked_for_each_key_until_one_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent_with(dir.path(), &["id_cert", "id_cert_rsa", "id_ed25519"]).await;
    let (_known_hosts, connector) = connector("keyonly", Auth::Agent(AgentSource::At(agent)));
    assert_logs_in(&connector, &Dialogs::nothing(), "keyonly").await;
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn an_agent_with_no_suitable_key_is_an_auth_error() {
    let dir = tempfile::tempdir().unwrap();
    for keys in [&[][..], &["id_cert"][..]] {
        let agent = agent_with(dir.path(), keys).await;
        let (_known_hosts, connector) = connector("keyonly", Auth::Agent(AgentSource::At(agent)));
        let dialogs = Dialogs::nothing();
        assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
        dialogs.assert_silent();
    }
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_small_rsa_key_in_the_agent_is_not_used() {
    // The server would take it (`keys/id_rsa_small` is in `authorized_keys`), but FR-SSH-02 would not.
    let dir = tempfile::tempdir().unwrap();
    let agent = agent_with(dir.path(), &["id_rsa_small"]).await;
    let (_known_hosts, connector) = connector("keyonly", Auth::Agent(AgentSource::At(agent)));
    assert_eq!(
        error(&connector, &Dialogs::nothing()).await,
        auth_error("keyonly")
    );
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn no_agent_running_is_an_auth_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = agent_path(dir.path(), usize::MAX);
    let (_known_hosts, connector) = connector("keyonly", Auth::Agent(AgentSource::At(path)));
    assert_eq!(
        error(&connector, &Dialogs::nothing()).await,
        auth_error("keyonly")
    );
}

/// FR-SSH-11: the target itself is reached with a password. See `tests/jump.rs` for the rest.
fn through_a_jump_host_with(agent: PathBuf) -> (tempfile::TempDir, SftpConnector) {
    let dir = tempfile::tempdir().unwrap();
    let profile = SftpProfile {
        host: "sshd-target".to_owned(),
        port: 22,
        user: "tester".to_owned(),
        auth: Auth::Password(Some(secret("tester"))),
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: TIMEOUT,
        known_hosts: dir.path().join("known_hosts"),
        jump: Some(JumpHost {
            host: HOST.to_owned(),
            port: 2228,
            user: "keyonly".to_owned(),
            auth: Auth::Agent(AgentSource::At(agent)),
        }),
    };
    (dir, SftpConnector::new(profile))
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn a_jump_host_can_log_in_with_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent_with(dir.path(), &["id_ed25519"]).await;
    let (_known_hosts, connector) = through_a_jump_host_with(agent);
    let dialogs = Dialogs::nothing();
    assert_logs_in(&connector, &dialogs, "tester").await;
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 servers"]
async fn no_agent_for_the_jump_host_is_an_auth_error_about_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = agent_path(dir.path(), usize::MAX - 1);
    let (_known_hosts, connector) = through_a_jump_host_with(path);
    assert_eq!(
        error(&connector, &Dialogs::nothing()).await,
        auth_error("keyonly")
    );
}

/// The real OpenSSH agent, which holds a certificate as well. Windows' agent runs as a service and
/// cannot be started by a test, so this runs elsewhere; `russh` uses the same protocol on the pipe.
#[cfg(unix)]
mod real_agent {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Child, Command};

    use super::*;

    struct RealAgent(Child);

    impl Drop for RealAgent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    async fn start(dir: &Path, key: &str) -> (RealAgent, PathBuf) {
        let socket = dir.join("agent.sock");
        let agent = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .spawn()
            .expect("ssh-agent must be on PATH");
        let agent = RealAgent(agent);
        while !socket.exists() {
            time::sleep(Duration::from_millis(10)).await;
        }

        // `ssh-add` wants a private file, and finds the certificate next to the key.
        let copy = dir.join(key);
        fs::copy(key_path(key), &copy).unwrap();
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o600)).unwrap();
        let certificate = format!("{key}-cert.pub");
        if key_path(&certificate).exists() {
            fs::copy(key_path(&certificate), dir.join(&certificate)).unwrap();
        }
        let status = Command::new("ssh-add")
            .arg(&copy)
            .env("SSH_AUTH_SOCK", &socket)
            .status()
            .expect("ssh-add must be on PATH");
        assert!(status.success());
        (agent, socket)
    }

    #[tokio::test]
    #[ignore = "needs the S-03 server"]
    async fn a_key_in_real_ssh_agent_logs_in() {
        for key in ["id_ed25519", "id_rsa"] {
            let dir = tempfile::tempdir().unwrap();
            let (_agent, socket) = start(dir.path(), key).await;
            let auth = Auth::Agent(AgentSource::At(socket));
            let (_known_hosts, connector) = connector("keyonly", auth);
            assert_logs_in(&connector, &Dialogs::nothing(), "keyonly").await;
        }
    }

    #[tokio::test]
    #[ignore = "needs the S-03 server"]
    async fn a_certificate_in_real_ssh_agent_logs_in() {
        for key in ["id_cert", "id_cert_rsa"] {
            let dir = tempfile::tempdir().unwrap();
            let (_agent, socket) = start(dir.path(), key).await;
            let auth = Auth::Agent(AgentSource::At(socket));
            let (_known_hosts, connector) = connector("certuser", auth);
            assert_logs_in(&connector, &Dialogs::nothing(), "certuser").await;
        }
    }
}

// ---- Keyboard-interactive (FR-SSH-07). ----

fn answer_totp_only(prompt: &KbdIntPrompt) -> String {
    assert!(prompt.text.contains("Verification code"), "{}", prompt.text);
    totp()
}

fn answer_both(prompt: &KbdIntPrompt) -> String {
    if prompt.text.contains("Password") {
        TOTP_PASSWORD.to_owned()
    } else {
        totp()
    }
}

fn answer_nonsense(_: &KbdIntPrompt) -> String {
    "000000".to_owned()
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_stored_password_answers_the_password_prompt_and_only_the_code_is_asked() {
    let auth = Auth::Password(Some(secret(TOTP_PASSWORD)));
    let (_dir, connector) = connector("totpuser", auth);
    let mut dialogs = Dialogs::nothing();
    dialogs.keyboard = Some(answer_totp_only);
    assert_logs_in(&connector, &dialogs, "totpuser").await;

    let asked = dialogs.keyboard_asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].len(), 1, "{asked:?}");
    assert_eq!(dialogs.secrets_asked(), []);
    assert!(connector.take_remembered().is_none());
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn with_no_stored_password_the_password_dialog_comes_first_and_can_remember() {
    let (_dir, connector) = connector("totpuser", Auth::Password(None));
    let mut dialogs = Dialogs::typing(TOTP_PASSWORD, Remember::Yes);
    dialogs.keyboard = Some(answer_totp_only);
    assert_logs_in(&connector, &dialogs, "totpuser").await;

    assert_eq!(dialogs.secrets_asked(), [password_prompt("totpuser", true)]);
    assert_eq!(dialogs.keyboard_asked().len(), 1);
    assert_remembered(&connector, TOTP_PASSWORD);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_wrong_code_is_an_auth_error_and_nothing_is_remembered() {
    let (_dir, connector) = connector("totpuser", Auth::Password(None));
    let mut dialogs = Dialogs::typing(TOTP_PASSWORD, Remember::Yes);
    dialogs.keyboard = Some(answer_nonsense);
    assert_eq!(error(&connector, &dialogs).await, auth_error("totpuser"));
    assert!(connector.take_remembered().is_none());
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn cancelling_a_server_question_is_not_an_error() {
    let auth = Auth::Password(Some(secret(TOTP_PASSWORD)));
    let (_dir, connector) = connector("totpuser", auth);
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::Cancelled);
    assert_eq!(dialogs.keyboard_asked().len(), 1);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_key_can_be_the_first_factor_and_the_server_asks_the_rest() {
    let (_dir, connector) = connector("keytotp", key_file("id_ed25519"));
    let mut dialogs = Dialogs::nothing();
    dialogs.keyboard = Some(answer_both);
    assert_logs_in(&connector, &dialogs, "keytotp").await;
    assert!(!dialogs.keyboard_asked().is_empty());
    assert_eq!(dialogs.secrets_asked(), []);
    assert!(
        connector.take_remembered().is_none(),
        "answers are never stored"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn a_password_can_follow_keyboard_interactive() {
    let auth = Auth::Password(Some(secret(TOTP_PASSWORD)));
    let (_dir, connector) = connector("kbdpass", auth);
    let dialogs = Dialogs::nothing();
    assert_logs_in(&connector, &dialogs, "kbdpass").await;
    dialogs.assert_silent();
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn an_agent_key_can_be_the_first_factor_too() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent_with(dir.path(), &["id_ed25519"]).await;
    let (_known_hosts, connector) = connector("keytotp", Auth::Agent(AgentSource::At(agent)));
    let mut dialogs = Dialogs::nothing();
    dialogs.keyboard = Some(answer_both);
    assert_logs_in(&connector, &dialogs, "keytotp").await;
}

// ---- Ask each time (FR-SSH-05). ----

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn ask_with_a_password_stores_nothing_and_reconnects_without_asking() {
    let (_dir, connector) = connector("tester", Auth::Ask);
    let dialogs = Dialogs::logging_in_with(LoginAnswer::Password(secret("tester")));
    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_eq!(dialogs.logins_asked(), 1);
    assert!(connector.take_remembered().is_none());

    assert_logs_in(&connector, &dialogs, "tester").await;
    assert_eq!(dialogs.logins_asked(), 1, "the reconnect asks nothing");
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn ask_with_a_key_and_its_passphrase_asks_nothing_more() {
    let (_dir, connector) = connector("keyonly", Auth::Ask);
    let dialogs = Dialogs::logging_in_with(LoginAnswer::KeyFile {
        path: key_path("id_ed25519_pass"),
        passphrase: Some(secret(PASSPHRASE)),
    });
    assert_logs_in(&connector, &dialogs, "keyonly").await;
    assert_eq!(dialogs.secrets_asked(), []);
    assert!(connector.take_remembered().is_none());

    assert_logs_in(&connector, &dialogs, "keyonly").await;
    assert_eq!(dialogs.logins_asked(), 1, "the reconnect asks nothing");
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn ask_with_a_key_and_no_passphrase_asks_without_offering_to_remember() {
    let (_dir, connector) = connector("keyonly", Auth::Ask);
    let dialogs = Dialogs::logging_in_with(LoginAnswer::KeyFile {
        path: key_path("id_ed25519_pass"),
        passphrase: None,
    });
    // Even a prompter that says Yes against the request must not get it stored.
    dialogs
        .secrets
        .lock()
        .unwrap()
        .push_back((PASSPHRASE.to_owned(), Remember::Yes));
    assert_logs_in(&connector, &dialogs, "keyonly").await;
    assert_eq!(
        dialogs.secrets_asked(),
        [passphrase_prompt("id_ed25519_pass", false)]
    );
    assert!(connector.take_remembered().is_none());
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn ask_with_a_password_answers_the_password_prompt_of_a_two_step_server() {
    let (_dir, connector) = connector("totpuser", Auth::Ask);
    let mut dialogs = Dialogs::logging_in_with(LoginAnswer::Password(secret(TOTP_PASSWORD)));
    dialogs.keyboard = Some(answer_totp_only);
    assert_logs_in(&connector, &dialogs, "totpuser").await;
    assert_eq!(dialogs.secrets_asked(), []);
    assert!(connector.take_remembered().is_none());
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn after_a_refused_key_the_next_ask_starts_again() {
    let (_dir, connector) = connector("keyonly", Auth::Ask);
    let unknown = || LoginAnswer::KeyFile {
        path: key_path("id_cert"),
        passphrase: None,
    };
    let dialogs = Dialogs::logging_in_with(unknown());
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));

    dialogs.logins.lock().unwrap().push_back(unknown());
    assert_eq!(error(&connector, &dialogs).await, auth_error("keyonly"));
    assert_eq!(dialogs.logins_asked(), 2);
}

#[tokio::test]
#[ignore = "needs the S-03 server"]
async fn cancelling_the_ask_prompt_is_not_an_error() {
    let (_dir, connector) = connector("tester", Auth::Ask);
    let dialogs = Dialogs::nothing();
    assert_eq!(error(&connector, &dialogs).await, ConnectError::Cancelled);
}
