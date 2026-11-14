// SPDX-License-Identifier: GPL-3.0-or-later

//! A share mapped on a drive letter by Windows itself (spec §5.9).

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use drivemount_vfs::{
    ConnectError, DriveLetter, Health, MountBackend, MountError, Prompter, Remember, Secret,
    SecretKind, SecretRequest, SecretSlot, async_trait,
};
use drivemount_win::wnet::{self, Credentials, WNetError};
use tokio::sync::watch;
use tokio::task::spawn_blocking;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::errors::{
    ERROR_NOT_CONNECTED, ERROR_OPEN_FILES, ERROR_SESSION_CREDENTIAL_CONFLICT, Mapping,
    credential_conflict, mount_error,
};

/// How long the health check waits for `GetDiskFreeSpaceExW`, which blocks for 36–82 s during an
/// outage (spike M3-01 §6). No answer in time counts as a failure.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// How to log in to the server (FR-SMB-02).
pub enum SmbAuth {
    /// A password: the remembered one, or asked for, with *Remember* offered, when there is none.
    Password(Option<Secret<String>>),
    /// A password asked for at every mount, never stored.
    Ask,
    /// The signed-in Windows user (Kerberos or NTLM single sign-on), with no credentials in the call.
    /// Windows then reuses any session it already has to the server, whoever it belongs to (spike
    /// M3-01 §5).
    WindowsUser,
}

/// What to map.
pub struct SmbTarget {
    /// The server: a name or address, without `\\`.
    pub server: String,
    /// The share.
    pub share: String,
    /// The user's domain, or empty.
    pub domain: String,
    /// The user name. Unused with [`SmbAuth::WindowsUser`].
    pub user: String,
    /// How to log in.
    pub auth: SmbAuth,
}

impl SmbTarget {
    /// `\\server\share`, as Windows maps it. A sub-path is the UI's business: Windows cannot map a
    /// letter to a folder inside a share (spec §5.9).
    #[must_use]
    pub fn unc(&self) -> String {
        format!(r"\\{}\{}", self.server, self.share)
    }

    /// The user name as Windows is given it, and compares it for 1219: `DOMAIN\user`, or the user
    /// alone (spike M3-01 §4).
    #[must_use]
    pub fn account(&self) -> String {
        if self.domain.is_empty() {
            self.user.clone()
        } else {
            format!(r"{}\{}", self.domain, self.user)
        }
    }
}

/// An SMB drive: `WNetAddConnection2W` with `CONNECT_TEMPORARY` (FR-SMB-03), removed by
/// `WNetCancelConnection2W` (FR-SMB-04), and its health from `GetDiskFreeSpaceExW` on the network
/// events (FR-SMB-05). Windows' SMB client reconnects it, so [`reconnect`](MountBackend::reconnect)
/// only checks again.
///
/// The password goes once the mapping is made (NFR-SEC-03); one the user asked to remember waits for
/// [`take_remembered`](MountBackend::take_remembered). Dropping a backend whose drive is still
/// mapped removes it, open files or not, so no mapping outlives the app (FR-SMB-07).
pub struct SmbBackend {
    server: String,
    share: String,
    account: String,
    auth: Mutex<SmbAuth>,
    /// The letter while the drive is mapped.
    letter: Mutex<Option<DriveLetter>>,
    remembered: Mutex<Option<Secret<String>>>,
    health: Arc<watch::Sender<Health>>,
    check: Arc<Check>,
}

impl SmbBackend {
    /// A backend for one mount of `target`.
    #[must_use]
    pub fn new(target: SmbTarget) -> Self {
        let account = target.account();
        Self {
            server: target.server,
            share: target.share,
            account,
            auth: Mutex::new(target.auth),
            letter: Mutex::new(None),
            remembered: Mutex::new(None),
            health: Arc::new(watch::channel(Health::Ok).0),
            check: Arc::new(Check::default()),
        }
    }

    fn unc(&self) -> String {
        format!(r"\\{}\{}", self.server, self.share)
    }

    /// The user name the errors show: the signed-in user for single sign-on.
    fn shown_user(&self) -> String {
        if matches!(*lock(&self.auth), SmbAuth::WindowsUser) {
            std::env::var("USERNAME").unwrap_or_default()
        } else {
            self.account.clone()
        }
    }

    /// The password to map with, asking for one if need be. `None` for single sign-on.
    async fn password(
        &self,
        prompter: &dyn Prompter,
    ) -> Result<Option<Secret<String>>, MountError> {
        let allow_remember = {
            let mut auth = lock(&self.auth);
            match &mut *auth {
                SmbAuth::WindowsUser => return Ok(None),
                SmbAuth::Password(stored) => match stored.take() {
                    Some(password) => return Ok(Some(password)),
                    None => true,
                },
                SmbAuth::Ask => false,
            }
        };
        let request = SecretRequest {
            kind: SecretKind::Password {
                user: self.account.clone(),
                host: self.server.clone(),
            },
            allow_remember,
        };
        let (password, remember) = prompter
            .secret(request)
            .await
            .ok_or(MountError::Connect(ConnectError::Cancelled))?;
        if allow_remember && remember == Remember::Yes {
            // Handed over only once the mapping worked, so a wrong one is never stored.
            *lock(&self.remembered) = Some(Secret::new(password.expose_secret().clone()));
        }
        Ok(Some(password))
    }

    async fn remove(&self, force: bool) -> Result<(), MountError> {
        let Some(letter) = *lock(&self.letter) else {
            return Ok(());
        };
        let removed = spawn_blocking(move || wnet::cancel_connection(letter, force))
            .await
            .map_err(|error| MountError::Other(error.to_string()))?;
        match removed {
            Ok(()) => {}
            // Someone removed it already, which is what was wanted.
            Err(error) if error.code == ERROR_NOT_CONNECTED => {
                info!(%letter, "the SMB drive was already gone");
            }
            Err(error) if error.code == ERROR_OPEN_FILES && !force => {
                return Err(MountError::FilesOpen(letter));
            }
            Err(error) => {
                return Err(MountError::Other(format!(
                    "WNetCancelConnection2W: {error}"
                )));
            }
        }
        *lock(&self.letter) = None;
        Ok(())
    }

    /// Checks the drive and says what it found as health, once the check is over.
    fn check_in_background(&self) {
        let Some(letter) = *lock(&self.letter) else {
            return;
        };
        let check = Arc::clone(&self.check);
        let health = Arc::clone(&self.health);
        tokio::spawn(async move {
            if let Err(reason) = check.run(letter).await {
                health.send_if_modified(|health| {
                    let was_ok = *health == Health::Ok;
                    if was_ok {
                        warn!(%letter, %reason, "the SMB drive does not answer");
                        *health = Health::Lost(reason);
                    }
                    was_ok
                });
            }
        });
    }
}

impl SmbBackend {
    /// Maps the share on `letter`, or with none, with `password` or as the signed-in user.
    async fn map(
        &self,
        letter: Option<DriveLetter>,
        password: Option<Secret<String>>,
    ) -> Result<(), MountError> {
        let user = password.as_ref().map(|_| self.account.clone());
        let unc = self.unc();
        let mapped = spawn_blocking(move || -> Result<(), WNetError> {
            let provider = smb_provider()?;
            let credentials = match (&user, &password) {
                (Some(user), Some(password)) => Credentials::Explicit {
                    user,
                    password: password.expose_secret(),
                },
                _ => Credentials::Current,
            };
            wnet::add_connection(letter, &unc, &provider, credentials)
        })
        .await
        .map_err(|error| MountError::Other(error.to_string()))?;

        let Err(error) = mapped else {
            return Ok(());
        };
        let user = self.shown_user();
        let mapping = Mapping {
            letter,
            server: &self.server,
            share: &self.share,
            user: &user,
        };
        debug!(?letter, unc = %self.unc(), %error, "mapping failed");
        if error.code == ERROR_SESSION_CREDENTIAL_CONFLICT {
            // Who holds the other session, for the message (spec §5.9). Reaches no server.
            let uses = spawn_blocking(wnet::net_uses).await;
            let uses = uses.ok().and_then(Result::ok).unwrap_or_default();
            return Err(credential_conflict(&self.server, &uses));
        }
        Err(mount_error(&error, &mapping))
    }

    /// *Test connection* (FR-PROF-06): connects to the share without a drive letter, checks that
    /// `sub_path` (`films\2024`, or empty) is a folder in it, and disconnects. Returns how long
    /// connecting took. A password typed at a prompt is never remembered.
    ///
    /// # Errors
    /// Why it could not connect, or [`ConnectError::PathNotFound`] / [`ConnectError::PathNotDir`]
    /// for the sub-path.
    pub async fn test(
        &self,
        sub_path: &str,
        prompter: &dyn Prompter,
    ) -> Result<Duration, ConnectError> {
        let as_connect = |error: MountError| match error {
            MountError::Connect(error) => error,
            other => ConnectError::Other(other.to_string()),
        };
        let password = self.password(prompter).await.map_err(as_connect)?;
        *lock(&self.remembered) = None;
        let started = std::time::Instant::now();
        self.map(None, password).await.map_err(as_connect)?;
        let took = started.elapsed();

        let unc = self.unc();
        let folder = if sub_path.is_empty() {
            unc.clone()
        } else {
            format!(r"{unc}\{sub_path}")
        };
        let found = spawn_blocking({
            let folder = folder.clone();
            move || std::fs::metadata(folder)
        })
        .await;
        let removed = spawn_blocking({
            let unc = unc.clone();
            move || wnet::cancel_connection_to(&unc, false)
        })
        .await;
        if let Ok(Err(error)) = removed {
            debug!(%unc, %error, "the test connection could not be removed");
        }
        let shown = || sub_path.to_owned();
        match found {
            Ok(Ok(metadata)) if metadata.is_dir() => Ok(took),
            Ok(Ok(_)) => Err(ConnectError::PathNotDir { path: shown() }),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(ConnectError::PathNotFound { path: shown() })
            }
            Ok(Err(error)) => Err(ConnectError::Other(format!("{folder}: {error}"))),
            Err(error) => Err(ConnectError::Other(error.to_string())),
        }
    }
}

#[async_trait]
impl MountBackend for SmbBackend {
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError> {
        let password = self.password(prompter).await?;
        if let Err(error) = self.map(Some(letter), password).await {
            *lock(&self.remembered) = None;
            return Err(error);
        }
        *lock(&self.letter) = Some(letter);
        self.health.send_replace(Health::Ok);
        info!(%letter, unc = %self.unc(), "SMB drive mapped");
        Ok(())
    }

    async fn unmount(&self) -> Result<(), MountError> {
        self.remove(false).await
    }

    async fn unmount_forced(&self) -> Result<(), MountError> {
        self.remove(true).await
    }

    /// Windows reconnects by itself (FR-SMB-05): this only checks whether it has.
    async fn reconnect(&self, _prompter: &dyn Prompter) -> Result<(), MountError> {
        let Some(letter) = *lock(&self.letter) else {
            return Err(MountError::Other("the SMB drive is not mapped".to_owned()));
        };
        match self.check.run(letter).await {
            Ok(()) => {
                self.health.send_replace(Health::Ok);
                Ok(())
            }
            Err(reason) => {
                debug!(%letter, %reason, "the SMB drive is not back yet");
                Err(ConnectError::ConnectionLost.into())
            }
        }
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.health.subscribe()
    }

    async fn check_connection(&self) {
        self.check_in_background();
    }

    fn reconnects_itself(&self) -> bool {
        true
    }

    fn take_remembered(&self) -> Vec<(SecretSlot, Secret<String>)> {
        lock(&self.remembered)
            .take()
            .map(|password| (SecretSlot::Password, password))
            .into_iter()
            .collect()
    }
}

impl Drop for SmbBackend {
    fn drop(&mut self) {
        let letter = self
            .letter
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(letter) = letter {
            // A local call of about 15 ms: it never waits for the server (spike M3-01 §2).
            match wnet::cancel_connection(letter, true) {
                Ok(()) => warn!(%letter, "removed an SMB drive left mapped"),
                Err(error) if error.code == ERROR_NOT_CONNECTED => {}
                Err(error) => warn!(%letter, %error, "could not remove an SMB drive left mapped"),
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The SMB provider's name, asked for once (spec §5.9).
fn smb_provider() -> Result<String, WNetError> {
    static PROVIDER: OnceLock<String> = OnceLock::new();
    if let Some(provider) = PROVIDER.get() {
        return Ok(provider.clone());
    }
    let provider = wnet::smb_provider()?;
    Ok(PROVIDER.get_or_init(|| provider).clone())
}

/// One health check per drive at a time (spec §5.9). A check that timed out keeps its thread until
/// Windows answers, and a check asked for meanwhile waits for that answer rather than starting
/// another.
#[derive(Default)]
struct Check {
    running: Mutex<Option<Answer>>,
}

/// A check's result once it has one: `Err` with the reason for the log.
type Answer = watch::Receiver<Option<Result<(), String>>>;

impl Check {
    async fn run(&self, letter: DriveLetter) -> Result<(), String> {
        let mut answer = self.start(letter)?;
        let answered = timeout(CHECK_TIMEOUT, answer.wait_for(Option::is_some)).await;
        match answered {
            Ok(Ok(answer)) => answer
                .clone()
                .unwrap_or_else(|| Err("no answer".to_owned())),
            Ok(Err(_)) => Err("the check ended without an answer".to_owned()),
            Err(_) => Err(format!(
                "no answer from {letter}\\ in {} s",
                CHECK_TIMEOUT.as_secs()
            )),
        }
    }

    /// The check under way, or a new one.
    fn start(&self, letter: DriveLetter) -> Result<Answer, String> {
        let mut running = lock(&self.running);
        if let Some(answer) = running.as_ref().filter(|answer| answer.borrow().is_none()) {
            return Ok(answer.clone());
        }
        let (sender, answer) = watch::channel(None);
        let root = format!("{letter}\\");
        thread::Builder::new()
            .name(format!("smb-check-{}", letter.as_char()))
            .spawn(move || {
                let found = wnet::free_space(&root)
                    .map(drop)
                    .map_err(|error| format!("{root}: {error}"));
                sender.send_replace(Some(found));
            })
            .map_err(|error| format!("could not start the check: {error}"))?;
        *running = Some(answer.clone());
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use drivemount_vfs::{
        HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    };

    use super::*;

    struct Answers(Option<(&'static str, Remember)>);

    #[async_trait]
    impl Prompter for Answers {
        async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
            self.0
                .map(|(password, remember)| (Secret::new(password.to_owned()), remember))
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

    fn target(domain: &str, auth: SmbAuth) -> SmbTarget {
        SmbTarget {
            server: "nas".to_owned(),
            share: "media".to_owned(),
            domain: domain.to_owned(),
            user: "marcin".to_owned(),
            auth,
        }
    }

    #[test]
    fn the_unc_and_account_are_as_windows_takes_them() {
        let plain = target("", SmbAuth::Ask);
        assert_eq!(plain.unc(), r"\\nas\media");
        assert_eq!(plain.account(), "marcin");
        assert_eq!(target("HOME", SmbAuth::Ask).account(), r"HOME\marcin");
    }

    #[tokio::test]
    async fn a_stored_password_is_used_without_asking() {
        let stored = Secret::new("stored".to_owned());
        let backend = SmbBackend::new(target("", SmbAuth::Password(Some(stored))));
        let password = backend.password(&Answers(None)).await.unwrap().unwrap();
        assert_eq!(password.expose_secret(), "stored");
        assert!(backend.take_remembered().is_empty());
    }

    #[tokio::test]
    async fn a_missing_password_is_asked_for_and_remembered_on_request() {
        let backend = SmbBackend::new(target("", SmbAuth::Password(None)));
        let answers = Answers(Some(("typed", Remember::Yes)));
        let password = backend.password(&answers).await.unwrap().unwrap();
        assert_eq!(password.expose_secret(), "typed");
        let remembered = backend.take_remembered();
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].0, SecretSlot::Password);
        assert_eq!(remembered[0].1.expose_secret(), "typed");
        assert!(backend.take_remembered().is_empty(), "handed over once");
    }

    #[tokio::test]
    async fn ask_each_time_never_remembers() {
        let backend = SmbBackend::new(target("", SmbAuth::Ask));
        let answers = Answers(Some(("typed", Remember::Yes)));
        backend.password(&answers).await.unwrap().unwrap();
        assert!(backend.take_remembered().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_prompt_cancels_the_mount() {
        let backend = SmbBackend::new(target("", SmbAuth::Ask));
        let error = backend.password(&Answers(None)).await.unwrap_err();
        assert_eq!(error, MountError::Connect(ConnectError::Cancelled));
    }

    #[tokio::test]
    async fn the_windows_user_needs_no_password() {
        let backend = SmbBackend::new(target("", SmbAuth::WindowsUser));
        assert!(backend.password(&Answers(None)).await.unwrap().is_none());
        assert!(backend.reconnects_itself());
    }

    #[tokio::test]
    async fn unmounting_what_was_never_mounted_does_nothing() {
        let backend = SmbBackend::new(target("", SmbAuth::WindowsUser));
        backend.unmount().await.unwrap();
        backend.unmount_forced().await.unwrap();
    }

    #[tokio::test]
    async fn a_host_that_does_not_resolve_is_unreachable_on_445() {
        let mut unreachable = target("", SmbAuth::WindowsUser);
        unreachable.server = "drivemount-test.invalid".to_owned();
        let backend = SmbBackend::new(unreachable);
        let letter = ('H'..='Q')
            .rev()
            .filter_map(DriveLetter::new)
            .find(|&letter| drivemount_win::drives::is_letter_free(letter).unwrap())
            .unwrap();
        let error = backend.mount(letter, &Answers(None)).await.unwrap_err();
        assert_eq!(
            error,
            MountError::Connect(ConnectError::HostUnreachable {
                host: "drivemount-test.invalid".to_owned(),
                port: 445,
            })
        );
    }
}
