// SPDX-License-Identifier: GPL-3.0-or-later

//! Keyboard-interactive rounds that OpenSSH cannot be made to send, from an in-process `russh`
//! server that follows a script. It accepts after the last round and offers no SFTP, so a login
//! that worked ends in some error other than E-AUTH. Needs `ssh-keygen`, but no S-03 server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fsm_sftp::connector::{Auth, SftpConnector, SftpProfile};
use fsm_vfs::{
    ConnectError, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    Prompter, Remember, Secret, SecretRequest, async_trait,
};
use russh::server::{self, Auth as ServerAuth, Response};
use russh::{MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio::time;

const PASSWORD: &str = "the password";
const TYPED: &str = "typed";
const TEST_LIMIT: Duration = Duration::from_secs(30);

type Round = &'static [&'static str];
type Received = Arc<Mutex<Vec<Vec<String>>>>;

/// What a scripted server does once its rounds run out.
#[derive(Clone, Copy)]
enum Then {
    Accept,
    /// Sends empty rounds for ever.
    KeepAsking,
}

struct Script {
    rounds: VecDeque<Round>,
    then: Then,
    received: Received,
}

impl server::Handler for Script {
    type Error = russh::Error;

    async fn auth_none(&mut self, _: &str) -> Result<ServerAuth, Self::Error> {
        Ok(ServerAuth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..])),
            partial_success: false,
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<ServerAuth, Self::Error> {
        if let Some(response) = response {
            let answers = response
                .map(|answer| String::from_utf8_lossy(&answer).into_owned())
                .collect();
            self.received.lock().unwrap().push(answers);
        }
        let round = match (self.rounds.pop_front(), self.then) {
            (Some(round), _) => round,
            (None, Then::KeepAsking) => &[],
            (None, Then::Accept) => return Ok(ServerAuth::Accept),
        };
        let prompts: Vec<_> = round
            .iter()
            .map(|text| (Cow::Borrowed(*text), false))
            .collect();
        Ok(ServerAuth::Partial {
            name: Cow::Borrowed(""),
            instructions: Cow::Borrowed(""),
            prompts: Cow::Owned(prompts),
        })
    }
}

fn host_key(dir: &Path) -> russh::keys::PrivateKey {
    let path = dir.join("host_key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&path)
        .status()
        .expect("ssh-keygen must be on PATH");
    assert!(status.success());
    russh::keys::load_secret_key(&path, None).unwrap()
}

/// Starts the server for one connection, and returns its port and what it will have received.
async fn serve(dir: &Path, rounds: &[Round], then: Then) -> (u16, Received) {
    let config = Arc::new(server::Config {
        keys: vec![host_key(dir)],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Received::default();
    let script = Script {
        rounds: rounds.iter().copied().collect(),
        then,
        received: Arc::clone(&received),
    };
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if let Ok(session) = server::run_stream(config, stream, script).await {
            let _ = session.await;
        }
    });
    (port, received)
}

/// Types the same answer to everything, and remembers what it was asked.
struct Typing {
    answers_per_question: usize,
    asked: Mutex<Vec<Vec<String>>>,
}

impl Typing {
    fn new() -> Self {
        Self {
            answers_per_question: 1,
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Prompter for Typing {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        panic!("the password is known")
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        panic!("not an Ask each time profile")
    }

    async fn keyboard_interactive(&self, request: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        let texts: Vec<_> = request.prompts.iter().map(|p| p.text.clone()).collect();
        let answers = texts.len() * self.answers_per_question;
        self.asked.lock().unwrap().push(texts);
        Some(
            (0..answers)
                .map(|_| Secret::new(TYPED.to_owned()))
                .collect(),
        )
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::TrustOnce
    }
}

/// Connects with a stored password. Connecting never fully succeeds, as there is no SFTP here.
async fn log_in(dir: &Path, port: u16, typing: &Typing) -> ConnectError {
    let connector = SftpConnector::new(SftpProfile {
        host: "127.0.0.1".to_owned(),
        port,
        user: "someone".to_owned(),
        auth: Auth::Password(Some(Secret::new(PASSWORD.to_owned()))),
        remote_path: "~".to_owned(),
        keepalive_secs: 0,
        timeout: Duration::from_secs(5),
        known_hosts: dir.join("known_hosts"),
        jump: None,
    });
    let attempt = time::timeout(TEST_LIMIT, connector.connect_session(typing)).await;
    match attempt.expect("connect_session must end by itself") {
        Ok(_) => panic!("this server has no SFTP"),
        Err(error) => error,
    }
}

fn is_auth_error(error: &ConnectError) -> bool {
    matches!(error, ConnectError::Auth { .. })
}

/// Runs a script that ends in acceptance, and returns what the server received and the user saw.
async fn run(rounds: &[Round]) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let dir = tempfile::tempdir().unwrap();
    let (port, received) = serve(dir.path(), rounds, Then::Accept).await;
    let typing = Typing::new();
    let error = log_in(dir.path(), port, &typing).await;
    assert!(!is_auth_error(&error), "{error}");
    let received = received.lock().unwrap().clone();
    let asked = typing.asked.lock().unwrap().clone();
    (received, asked)
}

fn lists(expected: &[&[&str]]) -> Vec<Vec<String>> {
    expected
        .iter()
        .map(|list| list.iter().map(|text| (*text).to_owned()).collect())
        .collect()
}

#[tokio::test]
async fn a_question_before_the_password_prompt_is_not_given_the_password() {
    let (received, asked) = run(&[&["Verification code:"], &["Password:"]]).await;
    assert_eq!(received, lists(&[&[TYPED], &[PASSWORD]]));
    assert_eq!(asked, lists(&[&["Verification code:"]]));
}

#[tokio::test]
async fn a_second_password_prompt_goes_to_the_user() {
    let (received, asked) = run(&[&["Password:"], &["Password:"]]).await;
    assert_eq!(received, lists(&[&[PASSWORD], &[TYPED]]));
    assert_eq!(asked, lists(&[&["Password:"]]));
}

#[tokio::test]
async fn a_change_of_password_is_not_answered_with_the_old_one_twice() {
    let (received, asked) = run(&[&["Password:", "New password:"]]).await;
    assert_eq!(received, lists(&[&[PASSWORD, TYPED]]));
    assert_eq!(asked, lists(&[&["New password:"]]));
}

#[tokio::test]
async fn answers_keep_their_places_in_a_mixed_round() {
    let (received, asked) = run(&[&["Code:", "Password:", "PIN:"]]).await;
    assert_eq!(received, lists(&[&[TYPED, PASSWORD, TYPED]]));
    assert_eq!(asked, lists(&[&["Code:", "PIN:"]]));
}

#[tokio::test]
async fn a_round_with_no_questions_shows_no_dialog() {
    let (received, asked) = run(&[&[], &["Password:"], &[]]).await;
    assert_eq!(received, lists(&[&[], &[PASSWORD], &[]]));
    assert_eq!(asked, Vec::<Vec<String>>::new());
}

#[tokio::test]
async fn a_server_that_never_stops_asking_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (port, received) = serve(dir.path(), &[], Then::KeepAsking).await;
    let error = log_in(dir.path(), port, &Typing::new()).await;
    assert!(is_auth_error(&error), "{error}");
    assert!(received.lock().unwrap().len() <= 16);
}

#[tokio::test]
async fn a_dialog_with_the_wrong_number_of_answers_is_not_sent_on() {
    let dir = tempfile::tempdir().unwrap();
    let (port, received) = serve(dir.path(), &[&["Code:"]], Then::Accept).await;
    let mut typing = Typing::new();
    typing.answers_per_question = 2;
    let error = log_in(dir.path(), port, &typing).await;
    assert!(matches!(error, ConnectError::Other(_)), "{error}");
    assert_eq!(received.lock().unwrap().len(), 0);
}
