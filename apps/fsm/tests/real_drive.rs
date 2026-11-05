// SPDX-License-Identifier: GPL-3.0-or-later

//! The manager with the real factory: a real SFTP login and a real drive, so all of M1-15's wiring
//! that a mock cannot show. These need the S-03 server (`tests\docker\servers.ps1 up`) and WinFsp,
//! and tell Explorer about their drives. Secrets go to a store in memory, not Credential Manager.
//!
//! `cargo test -p fsm --test real_drive -- --ignored --test-threads=1`

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fsm_app::backend::{SftpWinFspFactory, SystemDriveLetters};
use fsm_core::config::{Config, ConfigStore};
use fsm_core::manager::{ManagerParts, MountEvent, MountManager, MountState};
use fsm_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use fsm_vfs::{
    ConnectError, DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MemorySecretStore, MountError, Prompter, Remember, Secret, SecretRequest,
    SecretSlot, SecretStore, async_trait,
};
use fsm_win::drives::is_letter_free;
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tokio::time::timeout;
use uuid::Uuid;

const PATIENCE: Duration = Duration::from_secs(60);

/// Trusts the server once, and types a password if it is asked, ticking *Remember*.
struct Typist {
    password: &'static str,
    asked: AtomicUsize,
    /// Makes the key dialog stay open, as a user might leave it.
    hang_on_host_key: bool,
    at_the_dialog: Notify,
}

impl Typist {
    fn new(password: &'static str) -> Self {
        Self {
            password,
            asked: AtomicUsize::new(0),
            hang_on_host_key: false,
            at_the_dialog: Notify::new(),
        }
    }
}

#[async_trait]
impl Prompter for Typist {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some((Secret::new(self.password.to_owned()), Remember::Yes))
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        None
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        if self.hang_on_host_key {
            self.at_the_dialog.notify_one();
            std::future::pending::<()>().await;
        }
        HostKeyDecision::TrustOnce
    }
}

struct World {
    folder: TempDir,
    typist: Arc<Typist>,
    secrets: Arc<MemorySecretStore>,
    manager: MountManager,
    events: broadcast::Receiver<MountEvent>,
}

fn host() -> String {
    env::var("FSM_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn profile(name: &str, auth: SftpAuth) -> Profile {
    let mut sftp = SftpProfile::new(host(), "tester", auth);
    sftp.port = 2222;
    Profile::new(name, Protocol::Sftp(sftp))
}

fn world(profiles: Vec<Profile>, typist: Typist) -> World {
    let folder = TempDir::new().unwrap();
    let path = folder.path().join("config.toml");
    let mut store = ConfigStore::open(path);
    let config = Config {
        profiles,
        ..Config::default()
    };
    store.save(&config).unwrap();

    let typist = Arc::new(typist);
    let secrets = Arc::new(MemorySecretStore::new());
    let known_hosts = folder.path().join("known_hosts");
    let manager = MountManager::new(ManagerParts {
        store,
        factory: Arc::new(SftpWinFspFactory::new(
            Arc::clone(&secrets) as Arc<dyn SecretStore>,
            known_hosts,
        )),
        secrets: Arc::clone(&secrets) as Arc<dyn SecretStore>,
        letters: Arc::new(SystemDriveLetters),
        prompter: Arc::clone(&typist) as Arc<dyn Prompter>,
    });
    let events = manager.subscribe();
    World {
        folder,
        typist,
        secrets,
        manager,
        events,
    }
}

impl World {
    async fn next_event(&mut self) -> MountEvent {
        timeout(PATIENCE, self.events.recv())
            .await
            .expect("no event came")
            .expect("the event channel broke")
    }

    async fn mounted(&mut self, id: Uuid) -> DriveLetter {
        self.manager.mount(id).await.unwrap();
        assert_eq!(self.next_event().await.state, MountState::Connecting);
        let mounted = self.next_event().await;
        assert_eq!(mounted.state, MountState::Mounted, "{mounted:?}");
        mounted.detail.unwrap().letter
    }

    async fn unmounted(&mut self, id: Uuid) {
        self.manager.unmount(id).await;
        assert_eq!(self.next_event().await.state, MountState::Unmounting);
        assert_eq!(self.next_event().await.state, MountState::Disconnected);
    }

    fn in_the_file(&self) -> Config {
        let path = self.folder.path().join("config.toml");
        ConfigStore::open(path).config().clone()
    }
}

fn on_drive(letter: DriveLetter, name: &str) -> PathBuf {
    Path::new(&format!(r"{letter}\")).join(name)
}

fn unique_name(what: &str) -> String {
    format!("m1-15-{what}-{}.txt", Uuid::new_v4().simple())
}

/// The whole of what a *Password* profile with *Remember* ticked in the prompt goes through.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn a_remembered_password_mounts_writes_and_asks_only_once() {
    let saved = profile("Zażółć M1-15 #1", SftpAuth::Password { remember: false });
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("tester"));

    let letter = world.mounted(id).await;
    assert!(
        !is_letter_free(letter).unwrap(),
        "the drive is really there"
    );
    let file = on_drive(letter, &unique_name("write"));
    fs::write(&file, "through a real drive").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "through a real drive");

    assert_eq!(world.typist.asked.load(Ordering::SeqCst), 1);
    let stored = world.secrets.load(id, SecretSlot::Password).unwrap();
    assert_eq!(stored.unwrap().expose_secret(), "tester");
    let Protocol::Sftp(sftp) = &world.in_the_file().profiles[0].protocol.clone();
    assert_eq!(sftp.auth, SftpAuth::Password { remember: true });

    world.unmounted(id).await;
    assert!(is_letter_free(letter).unwrap(), "the drive is really gone");

    // The stored one is used, with no question, and it can clean up after the test.
    let letter = world.mounted(id).await;
    assert_eq!(world.typist.asked.load(Ordering::SeqCst), 1);
    fs::remove_file(on_drive(
        letter,
        file.file_name().unwrap().to_str().unwrap(),
    ))
    .unwrap();
    world.unmounted(id).await;

    world.manager.delete(id).await.unwrap();
    assert!(world.secrets.is_empty(), "FR-PROF-07");
    assert!(world.in_the_file().profiles.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn a_read_only_profile_gets_a_read_only_drive() {
    let mut saved = profile("Read only M1-15", SftpAuth::Password { remember: false });
    saved.read_only = true;
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("tester"));

    let letter = world.mounted(id).await;

    let refused = fs::write(on_drive(letter, &unique_name("refused")), "no").unwrap_err();
    const ERROR_WRITE_PROTECT: i32 = 19;
    assert_eq!(
        refused.raw_os_error(),
        Some(ERROR_WRITE_PROTECT),
        "{refused}"
    );
    world.unmounted(id).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn a_wrong_password_is_e_auth_with_nothing_stored_or_left_behind() {
    let saved = profile("Wrong M1-15", SftpAuth::Password { remember: false });
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("not the password"));

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_event().await.state, MountState::Connecting);
    let failed = world.next_event().await.state;
    assert!(
        matches!(
            failed,
            MountState::Error(MountError::Connect(ConnectError::Auth { .. }))
        ),
        "{failed:?}"
    );
    assert!(world.secrets.is_empty());
}

/// FR-MNT-11's criterion, with the mount at a real dialog.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn cancel_at_a_real_dialog_is_back_within_a_second() {
    let saved = profile("Cancel M1-15", SftpAuth::Password { remember: false });
    let id = saved.id;
    let typist = Typist {
        hang_on_host_key: true,
        ..Typist::new("tester")
    };
    let mut world = world(vec![saved], typist);

    world.manager.mount(id).await.unwrap();
    assert_eq!(world.next_event().await.state, MountState::Connecting);
    timeout(PATIENCE, world.typist.at_the_dialog.notified())
        .await
        .unwrap();

    let start = Instant::now();
    world.manager.cancel(id);

    assert_eq!(world.next_event().await.state, MountState::Disconnected);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

/// FR-PROF-06's criterion, without a drive.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server"]
async fn test_connection_reports_a_good_login_a_bad_one_and_a_missing_path() {
    let saved = profile("Test M1-15", SftpAuth::Password { remember: true });
    let world = world(Vec::new(), Typist::new("unused"));
    let typed = |password: &str| vec![(SecretSlot::Password, Secret::new(password.to_owned()))];

    let round_trip = world.manager.test(&saved, typed("tester")).await.unwrap();
    assert!(round_trip < PATIENCE);
    assert_eq!(
        world.typist.asked.load(Ordering::SeqCst),
        0,
        "typed in the dialog"
    );

    let wrong = world.manager.test(&saved, typed("wrong")).await;
    assert!(matches!(wrong, Err(ConnectError::Auth { .. })), "{wrong:?}");

    let mut missing = saved.clone();
    let Protocol::Sftp(sftp) = &mut missing.protocol;
    sftp.remote_path = "/no/such/folder".to_owned();
    let not_there = world.manager.test(&missing, typed("tester")).await;
    assert!(
        matches!(not_there, Err(ConnectError::PathNotFound { .. })),
        "{not_there:?}"
    );

    assert!(world.secrets.is_empty(), "a test stores nothing");
    assert!(world.in_the_file().profiles.is_empty());
}

/// Their volume prefixes must differ, or the second mount would fail.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn two_names_that_look_alike_mount_together_on_their_own_letters() {
    let (one, two) = (
        profile("Look alike", SftpAuth::Password { remember: false }),
        profile("look-alike", SftpAuth::Password { remember: false }),
    );
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two], Typist::new("tester"));

    let first = world.mounted(one_id).await;
    let second = world.mounted(two_id).await;

    assert_ne!(first, second);
    assert!(fs::read_dir(format!(r"{first}\")).is_ok());
    assert!(fs::read_dir(format!(r"{second}\")).is_ok());
    world.unmounted(one_id).await;
    assert!(
        fs::read_dir(format!(r"{second}\")).is_ok(),
        "the other stays"
    );
    world.unmounted(two_id).await;
}
