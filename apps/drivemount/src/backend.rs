// SPDX-License-Identifier: GPL-3.0-or-later

//! From a saved profile to something the manager can mount: SFTP through WinFsp, and the system's
//! drive letters. Turning one crate's `SftpProfile` into the other's is the only part that does not
//! need Windows.

use std::path::Path;
use std::time::Duration;

use drivemount_core::profile::{JumpAuth, SftpAuth, SftpProfile as SavedSftp};
use drivemount_sftp::connector::{AgentSource, Auth, JumpHost, SftpProfile};
use drivemount_vfs::{Secret, SecretSlot};

/// The most dispatcher threads a drive gets (spec §6.1).
const MOST_THREADS: u32 = 8;

/// What a connection needs, from what a profile says. `secret` gives a stored or typed secret: a
/// profile only says whether it should have one, and a slot it has no use for is not asked for.
pub fn sftp_profile(
    saved: &SavedSftp,
    known_hosts: &Path,
    timeout: Duration,
    mut secret: impl FnMut(SecretSlot) -> Option<Secret<String>>,
) -> SftpProfile {
    let auth = match &saved.auth {
        SftpAuth::Password { .. } => Auth::Password(secret(SecretSlot::Password)),
        SftpAuth::KeyFile { path, .. } => Auth::KeyFile {
            path: path.clone(),
            passphrase: secret(SecretSlot::Passphrase),
        },
        SftpAuth::Agent => Auth::Agent(AgentSource::System),
        SftpAuth::Ask => Auth::Ask,
    };
    let jump = saved.jump.as_ref().map(|jump| JumpHost {
        host: jump.host.clone(),
        port: jump.port,
        // None is the profile's own user, as with `ssh -J`.
        user: jump
            .username
            .clone()
            .unwrap_or_else(|| saved.username.clone()),
        auth: match &jump.auth {
            JumpAuth::Agent => Auth::Agent(AgentSource::System),
            JumpAuth::KeyFile { path } => Auth::KeyFile {
                path: path.clone(),
                passphrase: None,
            },
            JumpAuth::Password => Auth::Password(secret(SecretSlot::JumpPassword)),
        },
    });
    SftpProfile {
        host: saved.host.clone(),
        port: saved.port,
        user: saved.username.clone(),
        auth,
        remote_path: saved.remote_path.clone(),
        keepalive_secs: saved.keepalive_secs,
        timeout,
        known_hosts: known_hosts.to_owned(),
        jump,
    }
}

/// Where what the connector remembers for the server goes: `take_remembered` says only that it is one
/// secret, and which one follows from the method.
#[must_use]
pub const fn own_slot(auth: &SftpAuth) -> Option<SecretSlot> {
    match auth {
        SftpAuth::Password { .. } => Some(SecretSlot::Password),
        SftpAuth::KeyFile { .. } => Some(SecretSlot::Passphrase),
        SftpAuth::Agent | SftpAuth::Ask => None,
    }
}

/// `winfsp_threads` as WinFsp takes it: in the file, 0 is the number of cores (spec §6.1), and to
/// WinFsp it would mean its own default.
#[must_use]
pub fn dispatcher_threads(setting: u32, cores: Option<u32>) -> u32 {
    let threads = if setting == 0 {
        cores.unwrap_or(MOST_THREADS)
    } else {
        setting
    };
    threads.min(MOST_THREADS)
}

#[cfg(windows)]
pub use self::windows::{SftpWinFspFactory, SystemDriveLetters, default_known_hosts};

#[cfg(windows)]
mod windows {
    use std::env;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use drivemount_core::config::Settings;
    use drivemount_core::manager::{BackendFactory, DriveLetters};
    use drivemount_core::profile::{Profile, Protocol};
    use drivemount_sftp::connector::SftpConnector;
    use drivemount_vfs::{
        ConnectError, Connector, DriveLetter, Health, MountBackend, MountError, Prompter, RemoteFs,
        Secret, SecretSlot, SecretStore, async_trait,
    };
    use drivemount_win::drives::is_letter_free;
    use drivemount_winfsp::{VolumeConfig, WinFspBackend};
    use tokio::sync::watch;
    use uuid::Uuid;

    use super::{dispatcher_threads, own_slot, sftp_profile};

    /// `%USERPROFILE%\.ssh\known_hosts` (spec §6.3). `None` if `USERPROFILE` is not set.
    #[must_use]
    pub fn default_known_hosts() -> Option<PathBuf> {
        let home = env::var_os("USERPROFILE").filter(|home| !home.is_empty())?;
        Some(PathBuf::from(home).join(".ssh").join("known_hosts"))
    }

    /// FR-MNT-03's definition of a free letter.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SystemDriveLetters;

    impl DriveLetters for SystemDriveLetters {
        fn is_free(&self, letter: DriveLetter) -> io::Result<bool> {
            is_letter_free(letter)
        }
    }

    /// Makes SFTP drives. NFS and SMB join it at M3 and M4.
    pub struct SftpWinFspFactory {
        secrets: Arc<dyn SecretStore>,
        known_hosts: PathBuf,
    }

    impl SftpWinFspFactory {
        /// A factory that reads what is remembered from `secrets`, and checks host keys against
        /// `known_hosts`.
        #[must_use]
        pub fn new(secrets: Arc<dyn SecretStore>, known_hosts: PathBuf) -> Self {
            Self {
                secrets,
                known_hosts,
            }
        }

        /// A secret that cannot be read costs a question, not the mount.
        fn stored(&self, profile: Uuid, slot: SecretSlot) -> Option<Secret<String>> {
            self.secrets
                .load(profile, slot)
                .inspect_err(|error| {
                    tracing::warn!(%profile, ?slot, %error, "could not read a remembered secret");
                })
                .ok()
                .flatten()
        }
    }

    #[async_trait]
    impl BackendFactory for SftpWinFspFactory {
        fn create(
            &self,
            profile: &Profile,
            settings: &Settings,
        ) -> Result<Box<dyn MountBackend>, MountError> {
            let Protocol::Sftp(sftp) = &profile.protocol;
            let timeout = Duration::from_secs(settings.op_timeout_secs);
            let connection = sftp_profile(sftp, &self.known_hosts, timeout, |slot| {
                self.stored(profile.id, slot)
            });
            // One connector for the whole mount: what it remembers is what makes a reconnect ask
            // nothing, and it goes when the backend does (NFR-SEC-03).
            let connector = Arc::new(SftpConnector::new(connection));
            let cores = thread::available_parallelism()
                .ok()
                .and_then(|cores| u32::try_from(cores.get()).ok());
            let config = VolumeConfig {
                slug: profile.slug(),
                label: profile.name.clone(),
                file_system_name: "DM-SFTP".to_owned(),
                op_timeout: timeout,
                cache_ttl: Duration::from_secs(sftp.cache_ttl_secs),
                threads: dispatcher_threads(settings.winfsp_threads, cores),
                read_only: profile.read_only,
            };
            Ok(Box::new(SftpDrive {
                drive: WinFspBackend::new(SharedConnector(Arc::clone(&connector)), config),
                connector,
                own_slot: own_slot(&sftp.auth),
            }))
        }

        async fn test(
            &self,
            profile: &Profile,
            settings: &Settings,
            typed: Vec<(SecretSlot, Secret<String>)>,
            prompter: &dyn Prompter,
        ) -> Result<Duration, ConnectError> {
            let Protocol::Sftp(sftp) = &profile.protocol;
            let timeout = Duration::from_secs(settings.op_timeout_secs);
            let mut typed = typed;
            let connection = sftp_profile(sftp, &self.known_hosts, timeout, |slot| {
                let from_the_dialog = typed.iter().position(|(typed, _)| *typed == slot);
                match from_the_dialog {
                    Some(position) => Some(typed.swap_remove(position).1),
                    None => self.stored(profile.id, slot),
                }
            });
            // Connecting checks that the remote path is there and is a folder (FR-PROF-06).
            let connector = SftpConnector::new(connection);
            let _fs = connector.connect(prompter).await?;
            connector.probe().await
        }
    }

    /// Lets the factory's drive and its `take_remembered` share one connector.
    struct SharedConnector(Arc<SftpConnector>);

    #[async_trait]
    impl Connector for SharedConnector {
        async fn connect(
            &self,
            prompter: &dyn Prompter,
        ) -> Result<Arc<dyn RemoteFs>, ConnectError> {
            self.0.connect(prompter).await
        }

        async fn probe(&self) -> Result<Duration, ConnectError> {
            self.0.probe().await
        }

        async fn session_ended(&self) {
            self.0.session_ended().await;
        }
    }

    /// A WinFsp drive, and the connector that knows what the user asked to remember.
    struct SftpDrive {
        drive: WinFspBackend<SharedConnector>,
        connector: Arc<SftpConnector>,
        own_slot: Option<SecretSlot>,
    }

    #[async_trait]
    impl MountBackend for SftpDrive {
        async fn mount(
            &self,
            letter: DriveLetter,
            prompter: &dyn Prompter,
        ) -> Result<(), MountError> {
            self.drive.mount(letter, prompter).await
        }

        async fn unmount(&self) -> Result<(), MountError> {
            self.drive.unmount().await
        }

        async fn reconnect(&self, prompter: &dyn Prompter) -> Result<(), MountError> {
            self.drive.reconnect(prompter).await
        }

        fn health(&self) -> watch::Receiver<Health> {
            self.drive.health()
        }

        fn take_remembered(&self) -> Vec<(SecretSlot, Secret<String>)> {
            let own = self
                .own_slot
                .and_then(|slot| Some((slot, self.connector.take_remembered()?)));
            let jump = self.connector.take_remembered_jump();
            let jump = jump.map(|secret| (SecretSlot::JumpPassword, secret));
            own.into_iter().chain(jump).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use drivemount_core::profile::JumpHost as SavedJump;

    use super::*;

    fn saved(auth: SftpAuth) -> SavedSftp {
        let mut saved = SavedSftp::new("example.com", "deploy", auth);
        saved.port = 2222;
        saved.remote_path = "/var/www".to_owned();
        saved.keepalive_secs = 30;
        saved
    }

    fn converted(saved: &SavedSftp, stored: &[SecretSlot]) -> (SftpProfile, Vec<SecretSlot>) {
        let mut asked = Vec::new();
        let profile = sftp_profile(
            saved,
            Path::new("known_hosts"),
            Duration::from_secs(15),
            |slot| {
                asked.push(slot);
                stored
                    .contains(&slot)
                    .then(|| Secret::new(format!("{slot:?}")))
            },
        );
        (profile, asked)
    }

    fn secret_of(secret: Option<&Secret<String>>) -> Option<&str> {
        secret.map(|secret| secret.expose_secret().as_str())
    }

    #[test]
    fn the_fields_carry_over() {
        let (profile, _) = converted(&saved(SftpAuth::Agent), &[]);
        assert_eq!(profile.host, "example.com");
        assert_eq!(profile.port, 2222);
        assert_eq!(profile.user, "deploy");
        assert_eq!(profile.remote_path, "/var/www");
        assert_eq!(profile.keepalive_secs, 30);
        assert_eq!(profile.timeout, Duration::from_secs(15));
        assert_eq!(profile.known_hosts, Path::new("known_hosts"));
        assert!(profile.jump.is_none());
    }

    #[test]
    fn a_stored_password_is_handed_over_and_a_missing_one_means_asking() {
        let password = SftpAuth::Password { remember: true };
        let (with, asked) = converted(&saved(password.clone()), &[SecretSlot::Password]);
        let Auth::Password(given) = with.auth else {
            panic!("not a password");
        };
        assert_eq!(secret_of(given.as_ref()), Some("Password"));
        assert_eq!(asked, [SecretSlot::Password]);

        let (without, _) = converted(&saved(password), &[]);
        assert!(matches!(without.auth, Auth::Password(None)));
    }

    #[test]
    fn a_stored_passphrase_goes_with_the_key_file() {
        let path = PathBuf::from("id_ed25519");
        let auth = SftpAuth::KeyFile {
            path: path.clone(),
            remember: true,
        };
        let (profile, asked) = converted(&saved(auth), &[SecretSlot::Passphrase]);
        let Auth::KeyFile {
            path: given_path,
            passphrase,
        } = profile.auth
        else {
            panic!("not a key file");
        };
        assert_eq!(given_path, path);
        assert_eq!(secret_of(passphrase.as_ref()), Some("Passphrase"));
        assert_eq!(asked, [SecretSlot::Passphrase]);
    }

    /// FR-SSH-05: nothing is even looked for.
    #[test]
    fn agent_and_ask_look_for_no_secret() {
        let (agent, asked) = converted(&saved(SftpAuth::Agent), &SecretSlot::ALL);
        assert!(matches!(agent.auth, Auth::Agent(AgentSource::System)));
        assert!(asked.is_empty());

        let (ask, asked) = converted(&saved(SftpAuth::Ask), &SecretSlot::ALL);
        assert!(matches!(ask.auth, Auth::Ask));
        assert!(asked.is_empty());
    }

    fn with_jump(username: Option<&str>, auth: JumpAuth) -> SavedSftp {
        let mut saved = saved(SftpAuth::Agent);
        saved.jump = Some(SavedJump {
            host: "bastion".to_owned(),
            port: 2228,
            username: username.map(str::to_owned),
            auth,
        });
        saved
    }

    #[test]
    fn a_jump_host_without_a_user_takes_the_profiles() {
        let (profile, _) = converted(&with_jump(None, JumpAuth::Agent), &[]);
        let jump = profile.jump.unwrap();
        assert_eq!(jump.host, "bastion");
        assert_eq!(jump.port, 2228);
        assert_eq!(jump.user, "deploy");

        let (profile, _) = converted(&with_jump(Some("hop"), JumpAuth::Agent), &[]);
        assert_eq!(profile.jump.unwrap().user, "hop");
    }

    #[test]
    fn a_jump_password_comes_from_its_own_slot() {
        let stored = [SecretSlot::JumpPassword];
        let (profile, asked) = converted(&with_jump(None, JumpAuth::Password), &stored);
        let Auth::Password(given) = profile.jump.unwrap().auth else {
            panic!("not a password");
        };
        assert_eq!(secret_of(given.as_ref()), Some("JumpPassword"));
        assert_eq!(asked, [SecretSlot::JumpPassword]);
    }

    /// NFR-SEC-01 has no target for it.
    #[test]
    fn a_jump_key_has_no_stored_passphrase() {
        let path = PathBuf::from("bastion_key");
        let auth = JumpAuth::KeyFile { path: path.clone() };
        let (profile, asked) = converted(&with_jump(None, auth), &SecretSlot::ALL);
        let Auth::KeyFile {
            path: given_path,
            passphrase,
        } = profile.jump.unwrap().auth
        else {
            panic!("not a key file");
        };
        assert_eq!(given_path, path);
        assert!(passphrase.is_none());
        assert!(asked.is_empty());
    }

    #[test]
    fn what_is_remembered_goes_by_the_method() {
        let password = SftpAuth::Password { remember: false };
        assert_eq!(own_slot(&password), Some(SecretSlot::Password));
        let key = SftpAuth::KeyFile {
            path: PathBuf::from("key"),
            remember: false,
        };
        assert_eq!(own_slot(&key), Some(SecretSlot::Passphrase));
        assert_eq!(own_slot(&SftpAuth::Agent), None);
        assert_eq!(own_slot(&SftpAuth::Ask), None);
    }

    #[test]
    fn threads_are_the_setting_or_the_cores_and_never_over_eight() {
        assert_eq!(dispatcher_threads(0, Some(4)), 4);
        assert_eq!(dispatcher_threads(0, Some(16)), 8);
        assert_eq!(dispatcher_threads(0, None), 8);
        assert_eq!(dispatcher_threads(2, Some(16)), 2);
        assert_eq!(dispatcher_threads(32, Some(16)), 8);
    }
}
