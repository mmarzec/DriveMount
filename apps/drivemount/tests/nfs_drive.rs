// SPDX-License-Identifier: GPL-3.0-or-later

//! An NFS profile through the manager and the real factory, onto a real drive (M4-05): FR-NFS-01's
//! criterion (fixed ports with the portmapper stopped; files owned by the profile's UID and GID),
//! *Test connection*, and a read-only drive; and the `fsx`-style exerciser on an NFS drive (M4-06).
//! These need the knfsd test server and WinFsp, and tell Explorer about their drives.
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p drivemount --test nfs_drive -- --ignored --test-threads=1
//! ```
//!
//! Set `DRIVEMOUNT_TEST_HOST` to the WSL VM's IP (`wsl hostname -I`). The fixed-ports test stops
//! `rpcbind` in the `drivemount-test-nfs-1` container through `wsl docker exec`, and starts it again.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The `fsx`-style exerciser of `drivemount-winfsp`'s tests, which needs only `std` and `windows`.
#[path = "../../../crates/drivemount-winfsp/tests/support/exerciser.rs"]
mod exerciser;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use drivemount_app::backend::{DriveFactory, SystemDriveLetters};
use drivemount_core::config::{Config, ConfigStore};
use drivemount_core::manager::{ManagerParts, MountEvent, MountManager, MountState};
use drivemount_core::profile::{NfsProfile, Profile, Protocol};
use drivemount_vfs::{
    ConnectError, DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MemorySecretStore, Prompter, Remember, Secret, SecretRequest, SecretStore,
    async_trait,
};
use drivemount_win::drives::is_letter_free;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use uuid::Uuid;

const PATIENCE: Duration = Duration::from_secs(60);
const CONTAINER: &str = "drivemount-test-nfs-1";
/// `secure`: only a reserved source port may mount it.
const SECURE: &str = "/srv/nfs/rw";
const DENIED: &str = "/srv/nfs/denied";
/// The ports `tests/docker/nfs/entrypoint.sh` starts knfsd on.
const NFS_PORT: u16 = 2049;
const MOUNT_PORT: u16 = 20048;

/// AUTH_SYS asks nothing.
struct NoPrompts;

#[async_trait]
impl Prompter for NoPrompts {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        panic!("an NFS profile asked for a secret");
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        panic!("an NFS profile asked for a login");
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        panic!("an NFS profile asked for answers");
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        panic!("an NFS profile asked about a host key");
    }
}

/// One test at a time: one of them stops the portmapper the others would ask.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn host() -> String {
    env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn profile(name: &str, export: &str) -> Profile {
    Profile::new(name, Protocol::Nfs(NfsProfile::new(host(), export)))
}

/// Through `wsl --exec`: plain `wsl` hands its arguments to the VM's shell, which would expand
/// `$(…)` there rather than in the container.
fn in_the_container(script: &str) -> String {
    let output = Command::new("wsl")
        .args(["--exec", "docker", "exec", CONTAINER, "sh", "-c", script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct World {
    folder: TempDir,
    secrets: Arc<MemorySecretStore>,
    manager: MountManager,
    events: broadcast::Receiver<MountEvent>,
    _turn: MutexGuard<'static, ()>,
}

fn world(profiles: Vec<Profile>) -> World {
    let turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(folder.path().join("config.toml"));
    store
        .save(&Config {
            profiles,
            ..Config::default()
        })
        .unwrap();
    let secrets = Arc::new(MemorySecretStore::new());
    let manager = MountManager::new(ManagerParts {
        store,
        factory: Arc::new(DriveFactory::new(
            Arc::clone(&secrets) as Arc<dyn SecretStore>,
            folder.path().join("known_hosts"),
        )),
        secrets: Arc::clone(&secrets) as Arc<dyn SecretStore>,
        letters: Arc::new(SystemDriveLetters),
        prompter: Arc::new(NoPrompts),
    });
    let events = manager.subscribe();
    World {
        folder,
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
}

fn on_drive(letter: DriveLetter, name: &str) -> PathBuf {
    Path::new(&format!(r"{letter}\")).join(name)
}

fn unique_name(what: &str) -> String {
    format!("m4-05-{what}-{}", Uuid::new_v4().simple())
}

/// Starts the portmapper again, however the test ends. `-w` brings back what was registered. The
/// container has `pidof`, but no `pkill` or `pgrep`.
struct PortmapperStopped;

impl PortmapperStopped {
    fn new() -> Self {
        in_the_container("kill $(pidof rpcbind); sleep 1; ! pidof rpcbind");
        Self
    }
}

impl Drop for PortmapperStopped {
    /// No panic here: it may run while the test is already panicking.
    fn drop(&mut self) {
        let started = Command::new("wsl")
            .args(["--exec", "docker", "exec", CONTAINER, "sh", "-c"])
            .arg("pidof rpcbind || rpcbind -w")
            .status();
        if !started.as_ref().is_ok_and(|status| status.success()) {
            eprintln!("could not start rpcbind again in {CONTAINER}: {started:?}");
        }
    }
}

/// FR-NFS-01's criterion: with the portmapper stopped, fixed ports mount, and what the drive creates
/// is owned on the server by the profile's UID and GID. From a reserved port (FR-NFS-03), as the
/// export is `secure`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the knfsd test server and WinFsp"]
async fn fixed_ports_mount_without_the_portmapper_and_files_belong_to_the_profiles_ids() {
    let mut saved = profile("NFS fixed M4-05", SECURE);
    let Protocol::Nfs(nfs) = &mut saved.protocol else {
        unreachable!();
    };
    nfs.uid = 1234;
    nfs.gid = 5678;
    nfs.nfs_port = Some(NFS_PORT);
    nfs.mount_port = Some(MOUNT_PORT);
    let id = saved.id;
    let mut world = world(vec![saved.clone()]);

    let stopped = PortmapperStopped::new();
    let without_ports = {
        let mut asking = saved.clone();
        asking.id = Uuid::new_v4();
        let Protocol::Nfs(nfs) = &mut asking.protocol else {
            unreachable!();
        };
        (nfs.nfs_port, nfs.mount_port) = (None, None);
        world.manager.test(&asking, Vec::new()).await
    };
    assert!(
        matches!(without_ports, Err(ConnectError::HostUnreachable { .. })),
        "the portmapper is really off: {without_ports:?}"
    );

    let letter = world.mounted(id).await;
    assert!(
        !is_letter_free(letter).unwrap(),
        "the drive is really there"
    );
    let (file, folder) = (unique_name("file"), unique_name("folder"));
    fs::write(on_drive(letter, &file), "made through the drive").unwrap();
    fs::create_dir(on_drive(letter, &folder)).unwrap();
    assert_eq!(
        fs::read_to_string(on_drive(letter, &file)).unwrap(),
        "made through the drive"
    );
    drop(stopped);

    let owners = in_the_container(&format!(
        "stat -c '%u %g' {SECURE}/{file} {SECURE}/{folder}"
    ));
    assert_eq!(owners, "1234 5678\n1234 5678");

    fs::remove_file(on_drive(letter, &file)).unwrap();
    fs::remove_dir(on_drive(letter, &folder)).unwrap();
    world.unmounted(id).await;
    assert!(is_letter_free(letter).unwrap(), "the drive is really gone");
    assert!(world.secrets.is_empty(), "AUTH_SYS has nothing to remember");
    let _ = &world.folder;
}

/// FR-PROF-06 for NFS: a good export answers, a refused one is E-NFS-ACCESS (FR-NFS-05), and a
/// server with nothing on the NFS port is E-HOST-UNREACHABLE with that port.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the knfsd test server"]
async fn test_connection_reports_a_good_export_a_refused_one_and_a_wrong_port() {
    let world = world(Vec::new());

    let round_trip = world
        .manager
        .test(&profile("Good", SECURE), Vec::new())
        .await
        .unwrap();
    assert!(round_trip < PATIENCE);

    let refused = world
        .manager
        .test(&profile("Refused", DENIED), Vec::new())
        .await;
    assert_eq!(
        refused,
        Err(ConnectError::NfsAccess {
            export: DENIED.to_owned()
        })
    );

    let mut wrong_port = profile("Wrong port", SECURE);
    let Protocol::Nfs(nfs) = &mut wrong_port.protocol else {
        unreachable!();
    };
    (nfs.nfs_port, nfs.mount_port) = (Some(2050), Some(MOUNT_PORT));
    let unreachable = world.manager.test(&wrong_port, Vec::new()).await;
    assert!(
        matches!(
            &unreachable,
            Err(ConnectError::HostUnreachable { port: 2050, .. })
        ),
        "{unreachable:?}"
    );
}

/// FR-MNT-07 holds for NFS: the drive refuses a write before the server sees it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the knfsd test server and WinFsp"]
async fn a_read_only_nfs_profile_gets_a_read_only_drive() {
    const ERROR_WRITE_PROTECT: i32 = 19;
    let mut saved = profile("NFS read-only M4-05", SECURE);
    saved.read_only = true;
    let id = saved.id;
    let mut world = world(vec![saved]);

    let letter = world.mounted(id).await;
    let refused = fs::write(on_drive(letter, &unique_name("refused")), "no").unwrap_err();
    assert_eq!(
        refused.raw_os_error(),
        Some(ERROR_WRITE_PROTECT),
        "{refused}"
    );
    world.unmounted(id).await;
}

/// The SHA-256 of a file on knfsd's disk, as another client would read it.
fn sha256_on_the_server(path: &str) -> Vec<u8> {
    let line = in_the_container(&format!("sha256sum {path}"));
    let hex = line.split_whitespace().next().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
        .collect()
}

/// M4-06, NFR-REL-01 for NFS: the `fsx`-style exerciser on a real drive over knfsd, from a reserved
/// port on the `secure` export, with the server's file hashed in the container every 1,024
/// operations and at the end, and read once more through a new session. `FSX_OPS` (default 10,000)
/// and `FSX_SEED` as in `drivemount-winfsp`'s `tests/fsx.rs`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the knfsd test server and WinFsp; 100,000 operations take minutes"]
async fn fsx_over_knfsd_loses_nothing() {
    let ops = exerciser::from_env("FSX_OPS", exerciser::DEFAULT_OPS);
    let seed = exerciser::from_env("FSX_SEED", exerciser::DEFAULT_SEED);
    let saved = profile("NFS fsx M4-06", SECURE);
    let id = saved.id;
    let mut world = world(vec![saved]);
    let folder = unique_name("fsx");
    let on_server = format!("{SECURE}/{folder}/fsx");

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
