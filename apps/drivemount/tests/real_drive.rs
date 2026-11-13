// SPDX-License-Identifier: GPL-3.0-or-later

//! The manager with the real factory: a real SFTP login and a real drive, so all of M1-15's wiring
//! that a mock cannot show. These need the S-03 server (`tests\docker\servers.ps1 up`) and WinFsp,
//! and tell Explorer about their drives. Secrets go to a store in memory, not Credential Manager.
//!
//! `cargo test -p drivemount --test real_drive -- --ignored`
//!
//! The tests take turns by themselves (see [`ONE_AT_A_TIME`]).

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The `fsx`-style exerciser of `drivemount-winfsp`'s tests, which needs only `std` and `windows`.
#[path = "../../../crates/drivemount-winfsp/tests/support/exerciser.rs"]
mod exerciser;

use std::env;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use drivemount_app::backend::{SftpWinFspFactory, SystemDriveLetters};
use drivemount_core::config::{Config, ConfigStore};
use drivemount_core::manager::{ManagerParts, MountEvent, MountManager, MountState};
use drivemount_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use drivemount_vfs::{
    ConnectError, DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MemorySecretStore, MountError, Prompter, Remember, Secret, SecretRequest,
    SecretSlot, SecretStore, async_trait,
};
use drivemount_win::drives::is_letter_free;
use sha2::{Digest as _, Sha256};
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
    /// Last, so it is let go only once the manager and its drives have gone.
    _turn: MutexGuard<'static, ()>,
}

/// One test at a time, whatever `--test-threads` says. Some restart the `sshd` container or end its
/// connections, which breaks any test beside them; run in parallel, a restart during the 1 GiB copy
/// left a test process stuck in a drive's I/O, impossible to kill.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn host() -> String {
    env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn profile(name: &str, auth: SftpAuth) -> Profile {
    let mut sftp = SftpProfile::new(host(), "tester", auth);
    sftp.port = 2222;
    Profile::new(name, Protocol::Sftp(sftp))
}

fn world(profiles: Vec<Profile>, typist: Typist) -> World {
    let turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
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
        _turn: turn,
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

// ---- Reconnecting (M2-01), against the real OpenSSH. These restart its container. ----

/// Runs `docker compose` with `args` in the S-03 project, as `servers.ps1` does.
fn compose(args: &[&str]) -> std::process::Output {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let output = std::process::Command::new("wsl.exe")
        .args(["-d", "Ubuntu-22.04", "-u", "root", "--cd"])
        .arg(&tests_dir)
        .args(["--", "docker", "compose"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "docker compose {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs a shell command as root inside the `sshd` container.
fn in_sshd(command: &str) {
    compose(&["exec", "-T", "sshd", "sh", "-c", command]);
}

/// A user of its own, so that changing its password touches no other test. Removed when dropped.
struct ServerUser(&'static str);

impl ServerUser {
    fn new(name: &'static str, password: &str) -> Self {
        in_sshd(&format!(
            "id {name} >/dev/null 2>&1 || useradd -m {name}; echo '{name}:{password}' | chpasswd"
        ));
        Self(name)
    }

    fn set_password(&self, password: &str) {
        in_sshd(&format!("echo '{}:{password}' | chpasswd", self.0));
    }

    /// Ends the user's SSH connections, as a server dropping them would.
    fn drop_connections(&self) {
        in_sshd(&format!("pkill -KILL -u {} || true", self.0));
    }
}

impl Drop for ServerUser {
    fn drop(&mut self) {
        let _ = std::process::Command::new("wsl.exe")
            .args(["-d", "Ubuntu-22.04", "-u", "root", "--", "docker", "exec"])
            .args(["drivemount-test-sshd-1", "sh", "-c"])
            .arg(format!("pkill -KILL -u {0}; userdel -r {0}", self.0))
            .output();
    }
}

/// Plan M2-01's first check, and FR-REC-02's and FR-REC-05's criteria: the server restarts in the
/// middle of a large read, the drive comes back by itself, and the copy's hash matches.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; restarts the sshd container"]
async fn a_server_restart_during_a_large_read_comes_back_by_itself_and_the_hash_matches() {
    const SIZE: u64 = 64 << 20;
    let saved = profile("M2-01 restart", SftpAuth::Password { remember: false });
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("tester"));
    let letter = world.mounted(id).await;

    let local = world.folder.path().join("large.bin");
    let wanted = random_file(&local, SIZE, 0x2001);
    let remote = on_drive(letter, &unique_name("large"));
    fs::copy(&local, &remote).unwrap();

    let read_so_far = Arc::new(AtomicUsize::new(0));
    let reader = std::thread::spawn({
        let (remote, read_so_far) = (remote.clone(), Arc::clone(&read_so_far));
        move || -> io::Result<Vec<u8>> {
            let mut file = fs::File::open(remote)?;
            let mut hash = Sha256::new();
            let mut block = vec![0u8; 1 << 20];
            loop {
                let read = file.read(&mut block)?;
                if read == 0 {
                    return Ok(hash.finalize().to_vec());
                }
                hash.update(&block[..read]);
                read_so_far.fetch_add(read, Ordering::SeqCst);
            }
        }
    });
    while read_so_far.load(Ordering::SeqCst) < (SIZE / 4) as usize {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let restart = Instant::now();
    compose(&["restart", "-t", "0", "sshd"]);
    let event = world.next_event().await;
    assert_eq!(event.state, MountState::Reconnecting, "{event:?}");
    assert_eq!(event.detail.unwrap().letter, letter, "the letter stays");
    let event = world.next_event().await;
    assert_eq!(event.state, MountState::Mounted, "{event:?}");
    println!("back after {:?}", restart.elapsed());

    let copied = tokio::task::spawn_blocking(move || reader.join().unwrap())
        .await
        .unwrap()
        .expect("the read goes on across the restart");
    assert_eq!(copied, wanted, "the copy differs");
    // Nothing was asked again: the password and the key trusted once are kept for the mount.
    assert_eq!(world.typist.asked.load(Ordering::SeqCst), 1);
    assert_eq!(sha256_of(&remote), wanted);

    fs::remove_file(&remote).unwrap();
    world.unmounted(id).await;
}

/// Plan M2-01's third check and FR-REC-06's criterion: a password changed on the server stops the
/// reconnect at the first try, with *Error* and the drive gone, and nothing tried again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; adds a user to the sshd container"]
async fn a_password_changed_on_the_server_is_an_auth_error_with_no_retries() {
    let user = ServerUser::new("m2rec", "first-password");
    let mut saved = profile("M2-01 password", SftpAuth::Password { remember: false });
    let Protocol::Sftp(sftp) = &mut saved.protocol;
    sftp.username = user.0.to_owned();
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("first-password"));
    let letter = world.mounted(id).await;

    user.set_password("second-password");
    user.drop_connections();

    let event = world.next_event().await;
    assert_eq!(event.state, MountState::Reconnecting, "{event:?}");
    let event = world.next_event().await;
    assert!(
        matches!(
            event.state,
            MountState::Error(MountError::Connect(ConnectError::Auth { .. }))
        ),
        "{event:?}"
    );
    assert!(is_letter_free(letter).unwrap(), "the drive has gone");
    // No retry loop: nothing more happens, and nothing was asked.
    let quiet = timeout(Duration::from_secs(5), world.events.recv()).await;
    assert!(quiet.is_err(), "{quiet:?}");
    assert_eq!(world.typist.asked.load(Ordering::SeqCst), 1);
}

/// FR-REC-07's criterion, all but the toast (M2-04): with `auto_reconnect` off, the server going away
/// removes the drive and the row shows E-CONN-LOST.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; adds a user to the sshd container"]
async fn without_auto_reconnect_a_dropped_connection_removes_the_drive() {
    let user = ServerUser::new("m2lost", "password");
    let mut saved = profile("M2-01 no reconnect", SftpAuth::Password { remember: false });
    saved.auto_reconnect = false;
    let Protocol::Sftp(sftp) = &mut saved.protocol;
    sftp.username = user.0.to_owned();
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("password"));
    let letter = world.mounted(id).await;

    user.drop_connections();
    let event = world.next_event().await;
    assert_eq!(
        event.state,
        MountState::Error(MountError::Connect(ConnectError::ConnectionLost)),
        "{event:?}"
    );
    assert!(is_letter_free(letter).unwrap(), "the drive has gone");
}

/// A file of pseudo-random bytes, never the same twice in a row, and its SHA-256.
fn random_file(path: &Path, size: u64, seed: u64) -> Vec<u8> {
    let mut file = io::BufWriter::new(fs::File::create(path).unwrap());
    let mut hash = Sha256::new();
    let mut state = seed | 1;
    let mut block = vec![0u8; 1 << 20];
    let mut left = size;
    while left > 0 {
        for chunk in block.as_chunks_mut::<8>().0 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *chunk = state.to_le_bytes();
        }
        let take = usize::try_from(left.min(block.len() as u64)).unwrap();
        file.write_all(&block[..take]).unwrap();
        hash.update(&block[..take]);
        left -= take as u64;
    }
    file.flush().unwrap();
    hash.finalize().to_vec()
}

fn sha256_of(path: &Path) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut block = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut block).unwrap();
        if read == 0 {
            return hash.finalize().to_vec();
        }
        hash.update(&block[..read]);
    }
}

fn listed(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// Plan §9.2, NFR-REL-01 and NFR-REL-02 on a real drive over real SFTP: 1 GiB and 10,000 small files
/// in and out through Win32 calls (`CopyFileExW`, `CreateFileW`, `FindFirstFileW`), hashes compared.
/// Set `DRIVEMOUNT_TEST_HOST` to the WSL address (`wsl hostname -I`), as the localhost relay is slow.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; takes minutes and 2 GiB of local disk"]
async fn a_gibibyte_and_ten_thousand_small_files_go_in_and_out_unchanged() {
    const SMALL_FILES: u64 = 10_000;
    let saved = profile("M1-19 copy", SftpAuth::Password { remember: false });
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("tester"));
    let local = TempDir::new().unwrap();
    let letter = world.mounted(id).await;
    let remote = on_drive(letter, &format!("m1-19-{}", Uuid::new_v4().simple()));

    tokio::task::block_in_place(|| {
        fs::create_dir(&remote).unwrap();

        // 1 GiB.
        let original = local.path().join("big-in");
        let expected = random_file(&original, 1 << 30, 19);
        let start = Instant::now();
        fs::copy(&original, remote.join("big")).unwrap();
        let copied_in = start.elapsed();
        assert!(listed(&remote).contains(&"big".to_owned()), "NFR-REL-02");
        let back = local.path().join("big-out");
        let start = Instant::now();
        fs::copy(remote.join("big"), &back).unwrap();
        let copied_out = start.elapsed();
        assert!(sha256_of(&back) == expected, "the GiB came back different");
        println!("1 GiB in {copied_in:?}, out {copied_out:?}, hashes match");
        fs::remove_file(&original).unwrap();
        fs::remove_file(&back).unwrap();
        fs::remove_file(remote.join("big")).unwrap();

        // 10,000 small files, from empty to a little over a page.
        let small = local.path().join("small");
        fs::create_dir(&small).unwrap();
        let start = Instant::now();
        let mut expected = Vec::new();
        for index in 0..SMALL_FILES {
            let name = format!("f{index:05}");
            let path = small.join(&name);
            expected.push(random_file(&path, index % 4100, index));
            fs::copy(&path, remote.join(&name)).unwrap();
        }
        let copied_in = start.elapsed();
        let mut names = listed(&remote);
        names.sort();
        assert_eq!(names.len() as u64, SMALL_FILES, "NFR-REL-02");
        let start = Instant::now();
        for (index, name) in names.iter().enumerate() {
            let back = small.join(format!("{name}.back"));
            fs::copy(remote.join(name), &back).unwrap();
            assert!(
                sha256_of(&back) == expected[index],
                "{name} came back different"
            );
        }
        let copied_out = start.elapsed();
        println!("{SMALL_FILES} files in {copied_in:?}, out {copied_out:?}, hashes match");
        fs::remove_dir_all(&remote).unwrap();
    });

    world.unmounted(id).await;
}

/// The SHA-256 of a file on the `sshd` container's disk, as another client would read it.
fn sha256_on_the_server(path: &str) -> Vec<u8> {
    let output = compose(&["exec", "-T", "sshd", "sha256sum", path]);
    let text = String::from_utf8_lossy(&output.stdout);
    let hex = text.split_whitespace().next().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
        .collect()
}

/// M2-09, NFR-REL-01: the `fsx`-style exerciser on a real drive over OpenSSH, with the server's file
/// hashed in the container every 1,024 operations and at the end, and read once more through a new
/// session. `FSX_OPS` (default 10,000; M2-09 runs 100,000) and `FSX_SEED` as in `drivemount-winfsp`'s
/// `tests/fsx.rs`. Set `DRIVEMOUNT_TEST_HOST` to the WSL address (`wsl hostname -I`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; 100,000 operations take tens of minutes"]
async fn fsx_over_openssh_loses_nothing() {
    let ops = exerciser::from_env("FSX_OPS", exerciser::DEFAULT_OPS);
    let seed = exerciser::from_env("FSX_SEED", exerciser::DEFAULT_SEED);
    let saved = profile("M2-09 fsx", SftpAuth::Password { remember: false });
    let id = saved.id;
    let mut world = world(vec![saved], Typist::new("tester"));
    let folder = format!("m2-09-fsx-{}", Uuid::new_v4().simple());
    let on_server = format!("/home/tester/{folder}/fsx");

    let letter = world.mounted(id).await;
    let file = on_drive(letter, &folder).join("fsx");
    let start = Instant::now();
    let copy = tokio::task::block_in_place(|| {
        fs::create_dir(file.parent().unwrap()).unwrap();
        exerciser::run(&file, ops, seed, &mut |copy, log| {
            let expected: Vec<u8> = Sha256::digest(copy).to_vec();
            if sha256_on_the_server(&on_server) != expected {
                exerciser::compare("the server's file", b"(another hash)", copy, 0, log);
            }
        })
    });
    println!(
        "{ops} operations with seed {seed:#x} in {:?}, the file {} bytes at the end",
        start.elapsed(),
        copy.len()
    );
    world.unmounted(id).await;

    let letter = world.mounted(id).await;
    let file = on_drive(letter, &folder).join("fsx");
    tokio::task::block_in_place(|| {
        assert!(
            fs::read(&file).unwrap() == copy,
            "a new session reads another file"
        );
        fs::remove_dir_all(file.parent().unwrap()).unwrap();
    });
    world.unmounted(id).await;
}
