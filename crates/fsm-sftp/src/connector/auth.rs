// SPDX-License-Identifier: GPL-3.0-or-later

//! Logging in: password, key file with an optional certificate, SSH agent, *Ask each time*, and
//! keyboard-interactive where the server wants it (FR-SSH-01 to FR-SSH-07, spec Â§5.7).
//!
//! The server's list of methods decides what is tried, so a method it does not offer costs neither
//! a dialog nor a failed attempt. A profile's method is tried once: a refusal is E-AUTH, and only a
//! partial success leads to another factor. Each network step has the profile's timeout; time in a
//! dialog does not count.
//!
//! `russh` takes passwords and keyboard-interactive answers as plain `String`s, so its copies are
//! not zeroed.

use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fsm_vfs::{
    ConnectError, KbdIntPrompt, KbdIntRequest, LoginAnswer, LoginRequest, Prompter, Remember,
    Secret, SecretKind, SecretRequest,
};
use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse, Prompt};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::signature::Signer as _;
use russh::keys::ssh_encoding::Encode as _;
use russh::keys::ssh_key::private::KeypairData;
use russh::keys::ssh_key::public::KeyData;
use russh::keys::ssh_key::{self, Certificate};
use russh::keys::{self, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{AgentAuthError, MethodKind, MethodSet, Signer};
use tokio::task;
use tokio::time;
use tracing::{debug, info, warn};

use super::{Client, SftpConnector};

#[cfg(windows)]
const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
/// FR-SSH-02.
const MIN_RSA_BITS: u32 = 2048;
const PPK_HEADER: &str = "PuTTY-User-Key-File-";
const ENCRYPTED_PKCS8_HEADER: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";
const EC_PEM_HEADER: &str = "-----BEGIN EC PRIVATE KEY-----";
const PEM_ENCRYPTED_LINE: &str = "Proc-Type: 4,ENCRYPTED";
/// A server could otherwise keep a login going for ever with empty rounds.
const MAX_KBD_ROUNDS: usize = 16;
/// The same, with partial successes.
const MAX_FACTORS: usize = 4;

/// How to log in.
pub enum Auth {
    /// A password: the one from Credential Manager, or `None` to ask (FR-SSH-01). A
    /// keyboard-interactive password prompt is answered with it too (FR-SSH-07).
    Password(Option<Secret<String>>),
    /// A private key file, with `<path>-cert.pub` if that exists (FR-SSH-02, FR-SSH-06).
    KeyFile {
        /// The private key file.
        path: PathBuf,
        /// The passphrase from Credential Manager, if there is one. If the key needs one and this
        /// is `None`, the user is asked.
        passphrase: Option<Secret<String>>,
    },
    /// The keys in an SSH agent (FR-SSH-04).
    Agent(AgentSource),
    /// Ask the user for a password or a key file at each mount (FR-SSH-05).
    Ask,
}

/// Which SSH agent to use.
pub enum AgentSource {
    /// The Windows OpenSSH agent, then Pageant. Elsewhere, `SSH_AUTH_SOCK`.
    System,
    /// One agent on this named pipe or Unix socket.
    At(PathBuf),
}

/// What a login learned, for the next connection of the same mount, so a reconnect does not ask
/// again (FR-SSH-02). Dropped with the connector, on unmount (NFR-SEC-03). A refused login empties
/// it, so the next mount asks again.
#[derive(Default)]
pub(super) struct LoginMemory {
    password: Mutex<Option<Secret<String>>>,
    key: Mutex<Option<LoadedKey>>,
    to_remember: Mutex<Option<Secret<String>>>,
}

impl LoginMemory {
    fn forget(&self) {
        *lock(&self.password) = None;
        *lock(&self.key) = None;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `Secret` has no `Clone` on purpose. Each copy is zeroed when it is dropped.
fn copy(secret: &Secret<String>) -> Secret<String> {
    Secret::new(secret.expose_secret().clone())
}

#[derive(Clone)]
struct LoadedKey {
    path: PathBuf,
    key: Arc<PrivateKey>,
}

/// What a profile logs in with, once any dialog for it has been answered.
enum Credential<'a> {
    Password,
    Key(LoadedKey),
    Agent(&'a AgentSource),
}

/// Where a login stands after one method.
enum Step {
    Done,
    /// The method worked, and the server wants another one from this list.
    More(MethodSet),
    Refused,
}

impl From<AuthResult> for Step {
    fn from(result: AuthResult) -> Self {
        match result {
            AuthResult::Success => Self::Done,
            AuthResult::Failure {
                remaining_methods,
                partial_success: true,
            } => Self::More(remaining_methods),
            AuthResult::Failure { .. } => Self::Refused,
        }
    }
}

impl SftpConnector {
    /// Logs in, asking the user whatever the profile leaves open.
    pub(super) async fn log_in(
        &self,
        handle: &mut Handle<Client>,
        prompter: &dyn Prompter,
    ) -> Result<(), ConnectError> {
        let password = self.known_password();
        let mut login = Login {
            connector: self,
            handle: &mut *handle,
            prompter,
            password,
            typed: None,
            password_answered: false,
        };
        let accepted = login.run().await?;
        if accepted {
            login.keep_what_was_typed();
            return Ok(());
        }
        self.memory.forget();
        let p = &self.profile;
        warn!(user = %p.user, host = %p.host, "login refused");
        // A server that has had enough closes the connection, and so does a dying link.
        if handle.is_closed() {
            return Err(ConnectError::ConnectionLost);
        }
        Err(ConnectError::Auth {
            user: p.user.clone(),
            host: p.host.clone(),
        })
    }

    /// Takes the password or passphrase the user asked to remember, for the caller to store in
    /// Credential Manager (NFR-SEC-01). A password is here only once a server has accepted it, and
    /// a passphrase once it has opened the key. Which of the two it is follows from the profile.
    pub fn take_remembered(&self) -> Option<Secret<String>> {
        lock(&self.memory.to_remember).take()
    }

    fn known_password(&self) -> Option<Secret<String>> {
        if let Some(password) = &*lock(&self.memory.password) {
            return Some(copy(password));
        }
        match &self.profile.auth {
            Auth::Password(Some(password)) => Some(copy(password)),
            _ => None,
        }
    }

    async fn within<T>(
        &self,
        step: impl Future<Output = Result<T, russh::Error>>,
    ) -> Result<T, ConnectError> {
        time::timeout(self.profile.timeout, step)
            .await
            .map_err(|_| self.timed_out())?
            .map_err(|error| self.classify(error))
    }

    /// As [`within`](Self::within), for a login that needs a signature. `None` means there is no
    /// signature, because this key cannot sign or the agent declined. The connection is still fine,
    /// and another key can be tried.
    async fn signed(
        &self,
        step: impl Future<Output = Result<AuthResult, AgentAuthError>>,
    ) -> Result<Option<AuthResult>, ConnectError> {
        match time::timeout(self.profile.timeout, step).await {
            Err(_) => Err(self.timed_out()),
            Ok(Ok(result)) => Ok(Some(result)),
            Ok(Err(AgentAuthError::Send(_))) => Err(ConnectError::ConnectionLost),
            Ok(Err(AgentAuthError::Key(error))) => {
                warn!(%error, "could not sign the login");
                Ok(None)
            }
        }
    }
}

/// One login attempt.
struct Login<'a> {
    connector: &'a SftpConnector,
    handle: &'a mut Handle<Client>,
    prompter: &'a dyn Prompter,
    /// The password, once known.
    password: Option<Secret<String>>,
    /// Whether the password came from a dialog in this attempt, and what was ticked there.
    typed: Option<Remember>,
    /// A wrong password can come back as a new prompt. Only the user can answer that one.
    password_answered: bool,
}

impl<'a> Login<'a> {
    fn user(&self) -> &'a str {
        &self.connector.profile.user
    }

    fn host(&self) -> &'a str {
        &self.connector.profile.host
    }

    /// Whether the server let the user in.
    async fn run(&mut self) -> Result<bool, ConnectError> {
        let connector = self.connector;
        let none = self.handle.authenticate_none(self.user());
        let methods = match connector.within(none).await? {
            AuthResult::Success => return Ok(true),
            AuthResult::Failure {
                remaining_methods, ..
            } => remaining_methods,
        };
        debug!(?methods, "the server's login methods");

        let mut step = match self.credential().await? {
            Credential::Password => self.password_or_keyboard(&methods).await?,
            Credential::Key(key) if methods.contains(&MethodKind::PublicKey) => {
                self.key_file(&key).await?
            }
            Credential::Agent(source) if methods.contains(&MethodKind::PublicKey) => {
                self.agent(source).await?
            }
            Credential::Key(_) | Credential::Agent(_) => Step::Refused,
        };
        for _ in 0..MAX_FACTORS {
            match step {
                Step::Done => return Ok(true),
                Step::Refused => return Ok(false),
                Step::More(methods) => step = self.password_or_keyboard(&methods).await?,
            }
        }
        Ok(false)
    }

    fn keep_what_was_typed(&self) {
        let memory = &self.connector.memory;
        if let (Some(remember), Some(password)) = (self.typed, &self.password) {
            *lock(&memory.password) = Some(copy(password));
            if remember == Remember::Yes {
                *lock(&memory.to_remember) = Some(copy(password));
            }
        }
    }

    async fn credential(&mut self) -> Result<Credential<'a>, ConnectError> {
        let connector = self.connector;
        match &connector.profile.auth {
            Auth::Password(_) => Ok(Credential::Password),
            Auth::Agent(source) => Ok(Credential::Agent(source)),
            Auth::KeyFile { path, passphrase } => {
                let stored = passphrase.as_ref().map(copy);
                // NFR-SEC-01 has no place for a jump host's passphrase.
                let allow_remember = !connector.is_jump;
                self.open_key(path, stored, allow_remember)
                    .await
                    .map(Credential::Key)
            }
            Auth::Ask => {
                if let Some(key) = lock(&connector.memory.key).clone() {
                    return Ok(Credential::Key(key));
                }
                if self.password.is_some() {
                    return Ok(Credential::Password);
                }
                let request = LoginRequest {
                    user: self.user().to_owned(),
                    host: self.host().to_owned(),
                };
                match self.prompter.login(request).await {
                    None => Err(ConnectError::Cancelled),
                    Some(LoginAnswer::Password(password)) => {
                        self.password = Some(password);
                        self.typed = Some(Remember::No);
                        Ok(Credential::Password)
                    }
                    Some(LoginAnswer::KeyFile { path, passphrase }) => self
                        .open_key(&path, passphrase, false)
                        .await
                        .map(Credential::Key),
                }
            }
        }
    }

    /// The decrypted key: from memory, or from the file, asking for the passphrase if none is
    /// given. It is what stays in memory, so a reconnect needs no passphrase (FR-SSH-02).
    async fn open_key(
        &self,
        path: &Path,
        passphrase: Option<Secret<String>>,
        allow_remember: bool,
    ) -> Result<LoadedKey, ConnectError> {
        let memory = &self.connector.memory;
        if let Some(known) = &*lock(&memory.key)
            && known.path == path
        {
            return Ok(known.clone());
        }

        let key = match self.load_key(path, None).await {
            Err(KeyLoadError::Encrypted) => {
                let (passphrase, remember) = match passphrase {
                    Some(passphrase) => (passphrase, Remember::No),
                    None => self.ask_passphrase(path, allow_remember).await?,
                };
                let opened = self.load_key(path, Some(copy(&passphrase))).await;
                let key = opened.map_err(|error| self.key_error(path, error))?;
                if allow_remember && remember == Remember::Yes {
                    *lock(&memory.to_remember) = Some(passphrase);
                }
                key
            }
            opened => opened.map_err(|error| self.key_error(path, error))?,
        };
        let loaded = LoadedKey {
            path: path.to_owned(),
            key,
        };
        *lock(&memory.key) = Some(loaded.clone());
        Ok(loaded)
    }

    /// Reading a key is blocking, and opening one with a passphrase takes a noticeable moment.
    async fn load_key(
        &self,
        path: &Path,
        passphrase: Option<Secret<String>>,
    ) -> Result<Arc<PrivateKey>, KeyLoadError> {
        let path = path.to_owned();
        task::spawn_blocking(move || load_key(&path, passphrase.as_ref()))
            .await
            .unwrap_or_else(|error| {
                warn!(%error, "the task reading a key file ended");
                Err(KeyLoadError::Format)
            })
    }

    fn key_error(&self, path: &Path, error: KeyLoadError) -> ConnectError {
        match error {
            KeyLoadError::Format | KeyLoadError::Encrypted => {
                warn!(path = %path.display(), "unsupported key file");
                ConnectError::KeyFormat
            }
            // Spec FR-REC-06 treats a key that is gone as a failed login, which ends any retries.
            KeyLoadError::Unreadable | KeyLoadError::WrongPassphrase => {
                warn!(path = %path.display(), ?error, "could not open the key file");
                self.connector.memory.forget();
                ConnectError::Auth {
                    user: self.user().to_owned(),
                    host: self.host().to_owned(),
                }
            }
        }
    }

    async fn ask_passphrase(
        &self,
        path: &Path,
        allow_remember: bool,
    ) -> Result<(Secret<String>, Remember), ConnectError> {
        let key_name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let request = SecretRequest {
            kind: SecretKind::Passphrase { key_name },
            allow_remember,
        };
        self.prompter
            .secret(request)
            .await
            .ok_or(ConnectError::Cancelled)
    }

    async fn ask_password(&mut self) -> Result<(), ConnectError> {
        let allow_remember = matches!(self.connector.profile.auth, Auth::Password(_));
        let request = SecretRequest {
            kind: SecretKind::Password {
                user: self.user().to_owned(),
                host: self.host().to_owned(),
            },
            allow_remember,
        };
        let answer = self.prompter.secret(request).await;
        let (password, remember) = answer.ok_or(ConnectError::Cancelled)?;
        self.password = Some(password);
        self.typed = Some(if allow_remember {
            remember
        } else {
            Remember::No
        });
        Ok(())
    }

    /// The password method if the server offers it, else keyboard-interactive. A refused password
    /// is not tried again the other way: one wrong password should count as one failed login.
    async fn password_or_keyboard(&mut self, methods: &MethodSet) -> Result<Step, ConnectError> {
        if methods.contains(&MethodKind::Password) {
            if self.password.is_none() {
                self.ask_password().await?;
            }
            let Some(password) = &self.password else {
                return Ok(Step::Refused);
            };
            let attempt = self
                .handle
                .authenticate_password(self.user(), password.expose_secret().as_str());
            let result = self.connector.within(attempt).await?;
            return Ok(result.into());
        }
        if methods.contains(&MethodKind::KeyboardInteractive) {
            return self.keyboard_interactive().await;
        }
        debug!(?methods, "no login method left that this profile can use");
        Ok(Step::Refused)
    }

    /// FR-SSH-07.
    async fn keyboard_interactive(&mut self) -> Result<Step, ConnectError> {
        let connector = self.connector;
        let start = self
            .handle
            .authenticate_keyboard_interactive_start(self.user(), None);
        let mut reply = connector.within(start).await?;
        for _ in 0..MAX_KBD_ROUNDS {
            match reply {
                KeyboardInteractiveAuthResponse::Success => return Ok(Step::Done),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods,
                    partial_success: true,
                } => return Ok(Step::More(remaining_methods)),
                KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(Step::Refused),
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    let answers = self.answer_round(name, instructions, prompts).await?;
                    let responses = answers
                        .iter()
                        .map(|answer| answer.expose_secret().clone())
                        .collect();
                    let respond = self
                        .handle
                        .authenticate_keyboard_interactive_respond(responses);
                    reply = connector.within(respond).await?;
                }
            }
        }
        warn!("too many keyboard-interactive rounds");
        Ok(Step::Refused)
    }

    /// Answers one round: a password prompt from the password, and the rest from the user, in one
    /// dialog. No answer from that dialog is kept.
    async fn answer_round(
        &mut self,
        name: String,
        instructions: String,
        prompts: Vec<Prompt>,
    ) -> Result<Vec<Secret<String>>, ConnectError> {
        let mut answers: Vec<Option<Secret<String>>> = Vec::with_capacity(prompts.len());
        let mut for_the_user = Vec::new();
        for prompt in prompts {
            let answer = if is_password_prompt(&prompt.prompt) && !self.password_answered {
                self.password_for_prompt().await?
            } else {
                None
            };
            if answer.is_none() {
                for_the_user.push(KbdIntPrompt {
                    text: prompt.prompt,
                    echo: prompt.echo,
                });
            }
            answers.push(answer);
        }

        if !for_the_user.is_empty() {
            let asked = for_the_user.len();
            let request = KbdIntRequest {
                user: self.user().to_owned(),
                host: self.host().to_owned(),
                name,
                instructions,
                prompts: for_the_user,
            };
            let typed = self.prompter.keyboard_interactive(request).await;
            let typed = typed.ok_or(ConnectError::Cancelled)?;
            if typed.len() != asked {
                return Err(ConnectError::Other(format!(
                    "the dialog gave {} answers to {asked} questions",
                    typed.len()
                )));
            }
            let mut typed = typed.into_iter();
            for answer in &mut answers {
                if answer.is_none() {
                    *answer = typed.next();
                }
            }
        }
        Ok(answers
            .into_iter()
            .map(|answer| answer.unwrap_or_else(|| Secret::new(String::new())))
            .collect())
    }

    /// The answer to a password prompt, if this profile has one. A *Password* profile that has none
    /// yet asks through the password dialog, which can offer *Remember* (spec Â§7.4.1).
    async fn password_for_prompt(&mut self) -> Result<Option<Secret<String>>, ConnectError> {
        if self.password.is_none() && matches!(self.connector.profile.auth, Auth::Password(_)) {
            self.ask_password().await?;
        }
        let answer = self.password.as_ref().map(copy);
        self.password_answered = answer.is_some();
        Ok(answer)
    }

    /// The certificate first if there is one, as `ssh` does, and then the plain key.
    async fn key_file(&mut self, loaded: &LoadedKey) -> Result<Step, ConnectError> {
        let connector = self.connector;
        let hash = self
            .rsa_hash(&loaded.key.public_key().key_data().clone())
            .await?;
        let (path, key) = (loaded.path.clone(), Arc::clone(&loaded.key));
        let certificate = task::spawn_blocking(move || load_certificate(&path, &key))
            .await
            .unwrap_or(None);
        if let Some(certificate) = certificate {
            let mut signer = KeySigner(Arc::clone(&loaded.key));
            let attempt = self.handle.authenticate_certificate_with(
                self.user(),
                certificate,
                hash,
                &mut signer,
            );
            match connector.signed(attempt).await?.map(Step::from) {
                Some(Step::Refused) | None => debug!("certificate refused: trying the key"),
                Some(step) => return Ok(step),
            }
        }
        let key = PrivateKeyWithHashAlg::new(Arc::clone(&loaded.key), hash);
        let attempt = self.handle.authenticate_publickey(self.user(), key);
        Ok(connector.within(attempt).await?.into())
    }

    /// Every suitable key of every agent in turn (FR-SSH-04). An agent that is not running, is
    /// empty or declines to sign just leaves nothing to try.
    async fn agent(&mut self, source: &AgentSource) -> Result<Step, ConnectError> {
        for endpoint in endpoints(source) {
            let timeout = self.connector.profile.timeout;
            let opened = time::timeout(timeout, open_agent(&endpoint)).await;
            let mut agent = match opened {
                Ok(Ok(agent)) => agent,
                Ok(Err(error)) => {
                    debug!(?endpoint, %error, "no SSH agent here");
                    continue;
                }
                Err(_) => {
                    warn!(?endpoint, "the SSH agent did not respond");
                    continue;
                }
            };
            let identities = match time::timeout(timeout, agent.request_identities()).await {
                Ok(Ok(identities)) => identities,
                Ok(Err(error)) => {
                    warn!(?endpoint, %error, "the SSH agent failed to list its keys");
                    continue;
                }
                Err(_) => {
                    warn!(?endpoint, "the SSH agent did not list its keys");
                    continue;
                }
            };
            debug!(?endpoint, keys = identities.len(), "SSH agent found");
            for identity in identities {
                match self.agent_key(&mut agent, identity).await? {
                    Step::Refused => {}
                    step => return Ok(step),
                }
            }
        }
        Ok(Step::Refused)
    }

    async fn agent_key(
        &mut self,
        agent: &mut AgentConnection,
        identity: AgentIdentity,
    ) -> Result<Step, ConnectError> {
        let connector = self.connector;
        let public_key = identity.public_key().into_owned();
        if !key_is_allowed(public_key.key_data()) {
            debug!(algorithm = %public_key.algorithm(), "skipping an unsuitable agent key");
            return Ok(Step::Refused);
        }
        let hash = self.rsa_hash(public_key.key_data()).await?;
        let user = self.user();
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                let attempt = self
                    .handle
                    .authenticate_publickey_with(user, key, hash, agent);
                connector.signed(attempt).await?
            }
            AgentIdentity::Certificate { certificate, .. } => {
                let attempt =
                    self.handle
                        .authenticate_certificate_with(user, certificate, hash, agent);
                connector.signed(attempt).await?
            }
        };
        Ok(result.map_or(Step::Refused, Step::from))
    }

    /// `None` for other than RSA. For RSA, the best hash the server names, or SHA-512 if it names
    /// neither: never SHA-1 (FR-SSH-02, spec Â§8).
    async fn rsa_hash(&self, key: &KeyData) -> Result<Option<HashAlg>, ConnectError> {
        if key.rsa().is_none() {
            return Ok(None);
        }
        let connector = self.connector;
        let best = connector
            .within(self.handle.best_supported_rsa_hash())
            .await?;
        Ok(Some(best.flatten().unwrap_or(HashAlg::Sha512)))
    }
}

/// FR-SSH-07.
fn is_password_prompt(text: &str) -> bool {
    text.to_lowercase().contains("password")
}

// ---- Key files. ----

#[derive(Debug, PartialEq, Eq)]
enum KeyLoadError {
    /// Missing, or not readable by this user.
    Unreadable,
    /// `.ppk`, `.pub`, a type left out by FR-SSH-02, or not a key at all.
    Format,
    /// It needs a passphrase, and none was given.
    Encrypted,
    WrongPassphrase,
}

/// Reads a private key (FR-SSH-02, FR-SSH-03).
fn load_key(
    path: &Path,
    passphrase: Option<&Secret<String>>,
) -> Result<Arc<PrivateKey>, KeyLoadError> {
    let text = fs::read_to_string(path).map_err(|error| match error.kind() {
        ErrorKind::InvalidData => KeyLoadError::Format,
        _ => {
            debug!(%error, "could not read the key file");
            KeyLoadError::Unreadable
        }
    })?;
    let text = Secret::new(text);
    // `russh` would read these, but FR-SSH-03 wants them converted.
    if text.expose_secret().trim_start().starts_with(PPK_HEADER) {
        return Err(KeyLoadError::Format);
    }
    // `russh` decrypts this kind and then reads it as RSA, so the right passphrase would look wrong.
    if text.expose_secret().contains(EC_PEM_HEADER)
        && text.expose_secret().contains(PEM_ENCRYPTED_LINE)
    {
        return Err(KeyLoadError::Format);
    }
    // Without a passphrase, `russh` reports this kind as a malformed key.
    if passphrase.is_none() && text.expose_secret().contains(ENCRYPTED_PKCS8_HEADER) {
        return Err(KeyLoadError::Encrypted);
    }
    let passphrase = passphrase.map(|secret| secret.expose_secret().as_str());
    let key = keys::decode_secret_key(text.expose_secret(), passphrase).map_err(|error| {
        debug!(%error, "could not decode the key file");
        match error {
            keys::Error::KeyIsEncrypted => KeyLoadError::Encrypted,
            // The right passphrase would look wrong. That error's type is not reachable from here.
            _ if format!("{error:?}").contains("UnsupportedAlgorithm") => KeyLoadError::Format,
            _ if passphrase.is_some() => KeyLoadError::WrongPassphrase,
            _ => KeyLoadError::Format,
        }
    })?;
    if key_is_allowed(key.public_key().key_data()) {
        Ok(Arc::new(key))
    } else {
        Err(KeyLoadError::Format)
    }
}

/// FR-SSH-02: Ed25519, ECDSA, and RSA of at least 2048 bits.
fn key_is_allowed(key: &KeyData) -> bool {
    match key {
        KeyData::Ed25519(_) | KeyData::Ecdsa(_) => true,
        KeyData::Rsa(rsa) => rsa.key_size() >= MIN_RSA_BITS,
        _ => false,
    }
}

/// `<key>-cert.pub` next to the key (FR-SSH-06). A certificate that cannot be used is not worth
/// failing for: the plain key may still get in. It is read at each login, so a renewed one counts.
fn load_certificate(key_path: &Path, key: &PrivateKey) -> Option<Certificate> {
    let mut path = OsString::from(key_path);
    path.push("-cert.pub");
    let path = PathBuf::from(path);
    if !path.exists() {
        return None;
    }
    match keys::load_openssh_certificate(&path) {
        Ok(certificate) if certificate.public_key() == key.public_key().key_data() => {
            info!(path = %path.display(), "offering a certificate");
            Some(certificate)
        }
        Ok(_) => {
            warn!(path = %path.display(), "this certificate is for another key: not using it");
            None
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "could not read the certificate: not using it");
            None
        }
    }
}

/// Signs a certificate login with a key in memory. `russh`'s `authenticate_openssh_cert` would name
/// an RSA certificate with SHA-1 (`ssh-rsa-cert-v01`), which spec Â§8 leaves out and OpenSSH refuses.
struct KeySigner(Arc<PrivateKey>);

impl Signer for KeySigner {
    type Error = AgentAuthError;

    fn auth_sign(
        &mut self,
        _: &AgentIdentity,
        hash: Option<HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        let signed = append_signature(&self.0, hash, &mut to_sign)
            .map(|()| to_sign)
            .map_err(AgentAuthError::Key);
        async move { signed }
    }
}

/// What `russh` expects back from a signer: the data, followed by the signature as an SSH string.
fn append_signature(
    key: &PrivateKey,
    hash: Option<HashAlg>,
    data: &mut Vec<u8>,
) -> Result<(), keys::Error> {
    let signature: ssh_key::Signature = match key.key_data() {
        KeypairData::Rsa(rsa) => (rsa, hash).try_sign(data)?,
        other => other.try_sign(data)?,
    };
    signature.encode_vec()?.encode(data)?;
    Ok(())
}

// ---- Agents. ----

type AgentConnection = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    /// A named pipe on Windows, or a Unix socket.
    Path(PathBuf),
    #[cfg(windows)]
    Pageant,
}

/// In the order to try them.
fn endpoints(source: &AgentSource) -> Vec<Endpoint> {
    match source {
        AgentSource::At(path) => vec![Endpoint::Path(path.clone())],
        #[cfg(windows)]
        AgentSource::System => vec![
            Endpoint::Path(PathBuf::from(OPENSSH_AGENT_PIPE)),
            Endpoint::Pageant,
        ],
        #[cfg(not(windows))]
        AgentSource::System => std::env::var_os("SSH_AUTH_SOCK")
            .map(|path| Endpoint::Path(PathBuf::from(path)))
            .into_iter()
            .collect(),
    }
}

async fn open_agent(endpoint: &Endpoint) -> Result<AgentConnection, keys::Error> {
    match endpoint {
        #[cfg(windows)]
        Endpoint::Path(path) => Ok(AgentClient::connect_named_pipe(path).await?.dynamic()),
        #[cfg(not(windows))]
        Endpoint::Path(path) => Ok(AgentClient::connect_uds(path).await?.dynamic()),
        #[cfg(windows)]
        Endpoint::Pageant => Ok(AgentClient::connect_pageant().await?.dynamic()),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const PASSPHRASE: &str = "a passphrase";
    const DSA_PUBLIC: &str = "AAAAB3NzaC1kc3MAAACBAKU1YHBED+baHBtkP+jVDGkyzvsul7WnEmYEP1GchXO70nC/zgfUvJkrX0meLP+epFIgTL3yHaffyudAuMkAeWAcdtEUgt0JoVbVa5GSS5LyQFRLFJwwbJdSDcrcwKLSe2/NL2XU4DVSbbxFsu8IDxG1++jyKSNwdGrEHK3yeGgTAAAAFQDizsTrqJgE5cfzeRp67UiAWA/YeQAAAIEAgmedDO6DG10xSau2wMaGU0yF0VEkTMG2x2ySzrjwtQwVOf1m2cen5HcRi2J4Dnr6rQhYvQlgB4Io6UArbZrvLo12A0xn95IEAmo9rC4kGNDTk6eF9Wt5IQcoReNdEn32vjWxhJJ2hYTCDdw7WR6osETiL95sMsiS/nkLxoLbckYAAACACJW6uyLiPzmw4FlmVVzMS2E6Pd+dU6HAPBX10FXiMy6H7PLyjlfkT9BFY2dhul/HHOChbWx6657LKXvPFoWmJ+rPoP1knn/0LzRBOqKOGelZ4N3G94RU9WUU5/DTSH8ToXu3PnAGw6kwn+0yI/jTOZg42M6jjEQ/eUZI7MyWunI=";

    fn keygen(dir: &Path, name: &str, passphrase: &str, options: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let status = Command::new("ssh-keygen")
            .args(["-q", "-C", "fsm-unit-test", "-N", passphrase, "-f"])
            .arg(&path)
            .args(options)
            .status()
            .expect("ssh-keygen must be on PATH");
        assert!(status.success());
        path
    }

    fn secret(text: &str) -> Secret<String> {
        Secret::new(text.to_owned())
    }

    fn load_error(path: &Path, passphrase: Option<&str>) -> KeyLoadError {
        let passphrase = passphrase.map(secret);
        load_key(path, passphrase.as_ref()).expect_err("must fail")
    }

    #[test]
    fn password_prompts_are_recognised_in_any_case() {
        for text in [
            "Password:",
            "password: ",
            "(deploy@host) PASSWORD for deploy:",
        ] {
            assert!(is_password_prompt(text), "{text}");
        }
        for text in ["Verification code:", "Passphrase:", "PIN", ""] {
            assert!(!is_password_prompt(text), "{text}");
        }
    }

    #[test]
    fn plain_keys_of_each_type_load() {
        let dir = tempfile::tempdir().unwrap();
        for (name, options) in [
            ("ed25519", &["-t", "ed25519"][..]),
            ("ecdsa", &["-t", "ecdsa", "-b", "384"][..]),
            ("rsa", &["-t", "rsa", "-b", "2048"][..]),
            ("rsa_pem", &["-t", "rsa", "-b", "2048", "-m", "PEM"][..]),
            ("ecdsa_pem", &["-t", "ecdsa", "-m", "PEM"][..]),
            ("rsa_pkcs8", &["-t", "rsa", "-b", "2048", "-m", "PKCS8"][..]),
        ] {
            let path = keygen(dir.path(), name, "", options);
            assert!(load_key(&path, None).is_ok(), "{name}");
        }
    }

    #[test]
    fn encrypted_keys_ask_open_and_notice_a_wrong_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        for (name, options) in [
            ("ed25519", &["-t", "ed25519"][..]),
            ("ecdsa", &["-t", "ecdsa"][..]),
            ("rsa_pem", &["-t", "rsa", "-b", "2048", "-m", "PEM"][..]),
        ] {
            let path = keygen(dir.path(), name, PASSPHRASE, options);
            assert_eq!(load_error(&path, None), KeyLoadError::Encrypted, "{name}");
            assert_eq!(
                load_error(&path, Some("wrong")),
                KeyLoadError::WrongPassphrase,
                "{name}"
            );
            assert!(load_key(&path, Some(&secret(PASSPHRASE))).is_ok(), "{name}");
        }
    }

    /// `ssh-keygen -m PKCS8` as built with LibreSSL, as on Windows: PBKDF2 with HMAC-SHA1, which
    /// `russh` cannot read. Built with OpenSSL it uses SHA-256 and works: see `tests/auth.rs`.
    #[test]
    fn pkcs8_with_a_sha1_key_derivation_is_a_format_problem_whatever_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, include_str!("../../tests/fixtures/pkcs8_hmac_sha1")).unwrap();
        assert_eq!(load_error(&path, None), KeyLoadError::Encrypted);
        assert_eq!(load_error(&path, Some("wrong")), KeyLoadError::Format);
        assert_eq!(load_error(&path, Some(PASSPHRASE)), KeyLoadError::Format);
    }

    #[test]
    fn an_encrypted_ec_pem_key_is_a_format_problem_whatever_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let options = ["-t", "ecdsa", "-m", "PEM"];
        let path = keygen(dir.path(), "ecdsa_pem", PASSPHRASE, &options);
        assert_eq!(load_error(&path, None), KeyLoadError::Format);
        assert_eq!(load_error(&path, Some(PASSPHRASE)), KeyLoadError::Format);
    }

    #[test]
    fn a_small_rsa_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = keygen(dir.path(), "small", "", &["-t", "rsa", "-b", "1024"]);
        assert_eq!(load_error(&path, None), KeyLoadError::Format);
    }

    #[test]
    fn ppk_files_are_refused_whatever_their_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, "PuTTY-User-Key-File-3: ssh-ed25519\n").unwrap();
        assert_eq!(load_error(&path, None), KeyLoadError::Format);
        assert_eq!(load_error(&path, Some("x")), KeyLoadError::Format);
    }

    #[test]
    fn dsa_keys_are_not_allowed() {
        let key = keys::parse_public_key_base64(DSA_PUBLIC).unwrap();
        assert!(!key_is_allowed(key.key_data()));
    }

    #[test]
    fn forgetting_clears_what_a_login_learned() {
        let dir = tempfile::tempdir().unwrap();
        let path = keygen(dir.path(), "key", "", &["-t", "ed25519"]);
        let memory = LoginMemory::default();
        *lock(&memory.password) = Some(secret("a password"));
        *lock(&memory.key) = Some(LoadedKey {
            key: load_key(&path, None).unwrap(),
            path,
        });
        memory.forget();
        assert!(lock(&memory.password).is_none());
        assert!(lock(&memory.key).is_none());
    }

    #[test]
    fn what_is_not_a_private_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = keygen(dir.path(), "key", "", &["-t", "ed25519"]);
        assert_eq!(
            load_error(&key.with_extension("pub"), None),
            KeyLoadError::Format
        );

        let text = dir.path().join("text");
        fs::write(&text, "not a key\n").unwrap();
        assert_eq!(load_error(&text, None), KeyLoadError::Format);
    }

    #[test]
    fn a_missing_file_is_not_a_format_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone");
        assert_eq!(load_error(&path, None), KeyLoadError::Unreadable);
    }

    #[test]
    fn a_binary_file_is_a_format_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary");
        fs::write(&path, [0xff, 0xfe, 0x00, 0x80]).unwrap();
        assert_eq!(load_error(&path, None), KeyLoadError::Format);
    }

    #[test]
    fn the_certificate_must_belong_to_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let ca = keygen(dir.path(), "ca", "", &["-t", "ed25519"]);
        let key_path = keygen(dir.path(), "key", "", &["-t", "ed25519"]);
        let other_path = keygen(dir.path(), "other", "", &["-t", "ed25519"]);
        let status = Command::new("ssh-keygen")
            .args(["-q", "-s"])
            .arg(&ca)
            .args(["-I", "test", "-n", "someone"])
            .arg(dir.path().join("key.pub"))
            .status()
            .unwrap();
        assert!(status.success());

        let key = load_key(&key_path, None).unwrap();
        assert!(load_certificate(&key_path, &key).is_some());

        let other = load_key(&other_path, None).unwrap();
        assert!(
            load_certificate(&other_path, &other).is_none(),
            "none there"
        );
        fs::copy(
            dir.path().join("key-cert.pub"),
            dir.path().join("other-cert.pub"),
        )
        .unwrap();
        assert!(load_certificate(&other_path, &other).is_none(), "wrong key");

        fs::write(dir.path().join("key-cert.pub"), "not a certificate").unwrap();
        assert!(load_certificate(&key_path, &key).is_none(), "garbage");
    }

    #[test]
    fn an_explicit_agent_is_the_only_one_tried() {
        let path = PathBuf::from("somewhere");
        assert_eq!(
            endpoints(&AgentSource::At(path.clone())),
            [Endpoint::Path(path)]
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_system_agents_are_openssh_then_pageant() {
        assert_eq!(
            endpoints(&AgentSource::System),
            [
                Endpoint::Path(PathBuf::from(r"\\.\pipe\openssh-ssh-agent")),
                Endpoint::Pageant
            ]
        );
    }
}
