// SPDX-License-Identifier: GPL-3.0-or-later

//! `NfsConnector` (M4-04) against the knfsd test server: FR-NFS-03 (reserved source ports),
//! FR-NFS-04 (a copy across a server restart) and FR-NFS-05 (E-NFS-ACCESS). All of it needs the
//! S-03 servers, and a reserved source port: Windows needs no rights for one; on Linux (CI's `nfs`
//! job) run as root or lower `net.ipv4.ip_unprivileged_port_start` to 0.
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p drivemount-nfs --test connector -- --ignored --test-threads=1
//! ```
//!
//! On Windows, set `DRIVEMOUNT_TEST_HOST` to the WSL VM's IP (`wsl hostname -I`); on Linux the
//! default, `localhost`, is the server. The restart test stops and starts knfsd in the
//! `drivemount-test-nfs-1` container through `docker exec` (`wsl --exec docker exec` on Windows).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drivemount_nfs::{Identity, NfsConnector, NfsOptions, NfsProfile};
use drivemount_vfs::{
    Bytes, ConnectError, Connector, Disposition, HostKeyDecision, HostKeyRequest, KbdIntRequest,
    LoginAnswer, LoginRequest, OpenMode, Prompter, Remember, RemoteFs, RemotePath, Secret,
    SecretRequest, SessionFs, async_trait,
};
use tokio::time::sleep;

const SECURE: &str = "/srv/nfs/rw";
const DENIED: &str = "/srv/nfs/denied";
const CONTAINER: &str = "drivemount-test-nfs-1";

struct NoPrompts;

#[async_trait]
impl Prompter for NoPrompts {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        None
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

fn host() -> String {
    std::env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn profile(export: &str) -> NfsProfile {
    NfsProfile {
        host: host(),
        export: export.to_owned(),
        identity: Identity {
            uid: 1000,
            gid: 1000,
        },
        mount_port: None,
        nfs_port: None,
        reserved_port: true,
        timeout: Duration::from_secs(5),
        handle_ttl: Duration::from_secs(60),
    }
}

fn connector(profile: NfsProfile) -> NfsConnector {
    NfsConnector::new(profile, NfsOptions::DEFAULT_CONNECTIONS)
}

fn path(text: &str) -> RemotePath {
    RemotePath::new(text).unwrap()
}

fn unique(stem: &str) -> RemotePath {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path(&format!("/{stem}-{nanos}"))
}

async fn write_and_read_back(fs: &dyn RemoteFs) {
    let name = unique("connector");
    let handle = fs
        .open(&name, OpenMode::read_write(Disposition::CreateNew))
        .await
        .unwrap();
    fs.write(&handle, 0, Bytes::from_static(b"reserved"))
        .await
        .unwrap();
    fs.close(handle).await.unwrap();
    let handle = fs.open(&name, OpenMode::read()).await.unwrap();
    assert_eq!(
        fs.read(&handle, 0, 100).await.unwrap().as_ref(),
        b"reserved"
    );
    fs.close(handle).await.unwrap();
    fs.remove_file(&name).await.unwrap();
}

/// FR-NFS-03's acceptance criterion: the `secure` export mounts, and works.
#[tokio::test]
#[ignore = "needs the S-03 knfsd server"]
async fn a_secure_export_mounts_from_a_reserved_port() {
    let connector = connector(profile(SECURE));
    let fs = connector.connect(&NoPrompts).await.unwrap();
    write_and_read_back(fs.as_ref()).await;
    assert!(connector.probe().await.unwrap() < Duration::from_secs(5));
}

/// FR-NFS-01: no portmapper is asked when both ports are fixed.
#[tokio::test]
#[ignore = "needs the S-03 knfsd server"]
async fn fixed_ports_mount_too() {
    let mut profile = profile(SECURE);
    profile.mount_port = Some(20048);
    profile.nfs_port = Some(2049);
    let fs = connector(profile).connect(&NoPrompts).await.unwrap();
    write_and_read_back(fs.as_ref()).await;
}

fn access(export: &str) -> ConnectError {
    ConnectError::NfsAccess {
        export: export.to_owned(),
    }
}

/// FR-NFS-05's acceptance criterion: an export that does not list this PC.
#[tokio::test]
#[ignore = "needs the S-03 knfsd server"]
async fn an_export_that_does_not_list_this_pc_is_e_nfs_access() {
    let result = connector(profile(DENIED)).connect(&NoPrompts).await;
    assert_eq!(result.err(), Some(access(DENIED)));
}

/// knfsd answers a missing export as it does a refused one.
#[tokio::test]
#[ignore = "needs the S-03 knfsd server"]
async fn a_missing_export_is_e_nfs_access() {
    let result = connector(profile("/srv/nfs/nothing"))
        .connect(&NoPrompts)
        .await;
    assert_eq!(result.err(), Some(access("/srv/nfs/nothing")));
}

/// Without *Use reserved source port*, a `secure` export refuses MOUNT.
#[tokio::test]
#[ignore = "needs the S-03 knfsd server"]
async fn an_unreserved_port_on_a_secure_export_is_e_nfs_access() {
    let mut profile = profile(SECURE);
    profile.reserved_port = false;
    let result = connector(profile).connect(&NoPrompts).await;
    assert_eq!(result.err(), Some(access(SECURE)));
}

/// Runs a command in the knfsd container: Docker runs inside WSL on Windows, and beside the test on
/// Linux. `wsl --exec`, as plain `wsl` would hand the script to the VM's shell first.
fn in_container(script: &str) {
    let mut command = if cfg!(windows) {
        let mut wsl = Command::new("wsl");
        wsl.args(["--exec", "docker"]);
        wsl
    } else {
        Command::new("docker")
    };
    let status = command
        .args(["exec", CONTAINER, "sh", "-c", script])
        .status()
        .unwrap();
    assert!(status.success(), "{script}: {status}");
}

/// What the backend's reconnect loop does (spec §5.5), at a fixed second apart: connect again
/// whenever the session is lost. Gives how many reconnects there were, and whether each gave back
/// the file system that was there.
async fn reconnect_forever(
    connector: Arc<NfsConnector>,
    sessions: Arc<SessionFs>,
    first: Arc<dyn RemoteFs>,
) -> (usize, bool) {
    let mut reconnects = 0;
    let mut same = true;
    loop {
        let Some(generation) = sessions.up().await else {
            return (reconnects, same);
        };
        if sessions.lost(generation).await.is_none() {
            return (reconnects, same);
        }
        loop {
            match connector.connect(&NoPrompts).await {
                Ok(fs) => {
                    same &= Arc::ptr_eq(&fs, &first);
                    reconnects += 1;
                    sessions.replace(fs);
                    break;
                }
                Err(_) => sleep(Duration::from_secs(1)).await,
            }
        }
    }
}

fn pattern(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 11)) as u8)
        .collect()
}

/// FR-NFS-04's acceptance criterion: a copy that runs across a 10 s server restart completes, and
/// the file matches. The open file is not opened again: the same file system comes back, and sends
/// again what the restarted server may have lost.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 knfsd server, and stops it for 10 s"]
async fn a_copy_runs_across_a_server_restart() {
    const CHUNK: usize = 256 * 1024;
    const CHUNKS: usize = 160; // 40 MiB: more than one COMMIT's worth.
    // What the connector and the file system did, with --nocapture.
    let _log = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .finish(),
    );

    let connector = Arc::new(connector(profile(SECURE)));
    let first = connector.connect(&NoPrompts).await.unwrap();
    let sessions = Arc::new(SessionFs::new(Arc::clone(&first)));
    let reconnecting = tokio::spawn(reconnect_forever(
        Arc::clone(&connector),
        Arc::clone(&sessions),
        Arc::clone(&first),
    ));

    let name = unique("restart");
    let handle = sessions
        .open(&name, OpenMode::write(Disposition::CreateNew))
        .await
        .unwrap();
    let restart = tokio::task::spawn_blocking(|| {
        std::thread::sleep(Duration::from_secs(1));
        in_container("rpc.nfsd 0 && sleep 10 && rpc.nfsd --no-nfs-version 4 --no-udp 8");
    });
    let started = Instant::now();
    for index in 0..CHUNKS {
        let offset = index * CHUNK;
        let data = Bytes::from(pattern(offset, CHUNK));
        sessions.write(&handle, offset as u64, data).await.unwrap();
        sleep(Duration::from_millis(50)).await;
    }
    sessions.close(handle).await.unwrap();
    restart.await.unwrap();
    let took = started.elapsed();
    assert!(
        took > Duration::from_secs(10),
        "the copy ended before the restart: {took:?}"
    );

    let handle = sessions.open(&name, OpenMode::read()).await.unwrap();
    let mut offset = 0;
    while offset < CHUNKS * CHUNK {
        let read = sessions
            .read(&handle, offset as u64, CHUNK as u32)
            .await
            .unwrap();
        assert!(!read.is_empty(), "short file at {offset}");
        assert_eq!(read.as_ref(), pattern(offset, read.len()), "at {offset}");
        offset += read.len();
    }
    sessions.close(handle).await.unwrap();
    sessions.remove_file(&name).await.unwrap();

    sessions.shut_down();
    let (reconnects, same) = reconnecting.await.unwrap();
    assert!(reconnects >= 1, "the restart was not noticed");
    assert!(same, "a reconnect gave another file system");
}
