// SPDX-License-Identifier: GPL-3.0-or-later

//! `SftpFs`: the contract suite and what is particular to SFTP, on a server with the OpenSSH
//! extensions and on one without (FR-SSH-14). All of it needs the S-03 servers:
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p fsm-sftp --test sftpfs -- --ignored
//! ```
//!
//! On Windows, set `FSM_TEST_HOST` to the WSL VM's IP (`wsl hostname -I`). The 1 GiB case takes many
//! minutes on ProFTPD through `localhost`, whose relay stalls single small writes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use common::{Link, Proxy};
use fsm_sftp::connector::{Auth, SftpConnector, SftpProfile, SftpSession};
use fsm_sftp::fs::{MAX_OPEN_FILES, SftpFs};
use fsm_vfs::{
    AttrChanges, AttrTarget, ConnectError, Connector, Disposition, FileKind, FsError, FsStats,
    HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest, OpenMode, Prompter,
    ReadAhead, Remember, RemoteFs, RemoteHandle, RemotePath, Secret, SecretRequest, async_trait,
    contract,
};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::time;

const OPENSSH: u16 = 2222;
const PROFTPD: u16 = 2230;
const REKEYING: u16 = 2231;
const WINDOWS_PATHS: u16 = 2232;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

struct TrustAll;

#[async_trait]
impl Prompter for TrustAll {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        panic!("no secret should be asked for")
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        panic!("no login should be asked for")
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        panic!("no keyboard-interactive round should be asked for")
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::TrustOnce
    }
}

struct Server {
    connector: SftpConnector,
    _known_hosts: tempfile::TempDir,
}

fn server_at(host: &str, port: u16, remote_path: &str, timeout: Duration) -> Server {
    let known_hosts = tempfile::tempdir().unwrap();
    let path: PathBuf = known_hosts.path().join("known_hosts");
    let connector = SftpConnector::new(SftpProfile {
        host: host.to_owned(),
        port,
        user: "tester".to_owned(),
        auth: Auth::Password(Some(Secret::new("tester".to_owned()))),
        remote_path: remote_path.to_owned(),
        keepalive_secs: 0,
        timeout,
        known_hosts: path,
        jump: None,
    });
    Server {
        connector,
        _known_hosts: known_hosts,
    }
}

fn server(port: u16, remote_path: &str) -> Server {
    let host = std::env::var("FSM_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned());
    server_at(&host, port, remote_path, Duration::from_secs(30))
}

impl Server {
    async fn session(&self) -> SftpSession {
        self.connector.connect_session(&TrustAll).await.unwrap()
    }

    async fn fs(&self) -> SftpFs {
        SftpFs::new(self.session().await)
    }
}

fn path(text: &str) -> RemotePath {
    RemotePath::new(text).unwrap()
}

/// A path no other run uses.
fn unique(name: &str) -> RemotePath {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path(&format!("/{name}-{}-{nanos}", std::process::id()))
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| (at ^ (at >> 8) ^ (at >> 16)) as u8)
        .collect()
}

const CREATE: OpenMode = OpenMode::read_write(Disposition::CreateNew);

async fn put(fs: &SftpFs, file: &RemotePath, data: &[u8]) {
    let handle = fs.open(file, CREATE).await.unwrap();
    fs.write(&handle, 0, Bytes::copy_from_slice(data))
        .await
        .unwrap();
    fs.close(handle).await.unwrap();
}

async fn get(fs: &SftpFs, file: &RemotePath) -> Vec<u8> {
    let size = fs.stat(file).await.unwrap().size;
    let handle = fs.open(file, OpenMode::read()).await.unwrap();
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let got = fs
            .read(&handle, out.len() as u64, (size - out.len() as u64) as u32)
            .await
            .unwrap();
        assert!(!got.is_empty(), "the file ended early");
        out.extend_from_slice(&got);
    }
    fs.close(handle).await.unwrap();
    out
}

// ---- The contract suite. ----

/// Includes a rekey this side asks for: `russh` does so after 1 GiB.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_contract_holds_on_openssh() {
    let server = server(OPENSSH, "~");
    let session = server.session().await;
    let before = session.key_exchanges();
    let capabilities = {
        let fs = SftpFs::new(session);
        contract::run_all(&fs, &RemotePath::root()).await;
        fs.capabilities()
    };
    assert!(capabilities.posix_rename && capabilities.fsync && capabilities.statvfs);
    assert_eq!(capabilities.max_read, 261_120);
    assert_eq!(capabilities.max_write, 261_120);

    // The session is gone, but the count is the connector's.
    let session = server.session().await;
    assert!(
        session.key_exchanges() >= before + 2,
        "2 GiB should have caused a rekey: {before} then {}",
        session.key_exchanges()
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_contract_holds_on_a_server_without_the_extensions() {
    let server = server(PROFTPD, "~");
    let session = server.session().await;
    for extension in [
        "limits@openssh.com",
        "posix-rename@openssh.com",
        "fsync@openssh.com",
        "statvfs@openssh.com",
    ] {
        assert!(!session.has_extension(extension), "{extension}");
    }
    let fs = SftpFs::new(session);
    let capabilities = fs.capabilities();
    assert!(!capabilities.posix_rename && !capabilities.fsync && !capabilities.statvfs);
    assert_eq!(capabilities.max_read, 32 * 1024);
    assert_eq!(capabilities.max_write, 32 * 1024);
    assert_eq!(fs.statfs().await, Ok(FsStats::PLACEHOLDER));

    contract::run_all(&fs, &RemotePath::root()).await;
}

/// `/` as the mount's root, where nothing is added to a path.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_contract_holds_with_the_servers_root_as_the_mounts() {
    let server = server(OPENSSH, "/");
    let fs = server.fs().await;
    contract::run_quick(&fs, &path("/home/tester")).await;
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn paths_are_relative_to_the_remote_path() {
    let at_home = server(OPENSSH, "~").fs().await;
    let at_root = server(OPENSSH, "/").fs().await;
    let file = unique("rooted");
    put(&at_home, &file, b"data").await;

    let from_root = path(&format!("/home/tester{}", file.as_str()));
    assert_eq!(get(&at_root, &from_root).await, b"data");
    assert!(at_home.stat(&RemotePath::root()).await.unwrap().is_dir());
    let listing = at_home.read_dir(&RemotePath::root()).await.unwrap();
    let name = file.file_name().unwrap();
    assert!(listing.iter().any(|entry| entry.name == name));

    at_home.remove_file(&file).await.unwrap();
}

// ---- Connector. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_connector_gives_a_working_file_system_and_probes_it() {
    let server = server(OPENSSH, "~");
    assert_eq!(
        server.connector.probe().await,
        Err(ConnectError::ConnectionLost),
        "nothing to probe yet"
    );

    let fs: Arc<dyn RemoteFs> = server.connector.connect(&TrustAll).await.unwrap();
    assert!(fs.capabilities().posix_rename);
    assert!(fs.stat(&RemotePath::root()).await.unwrap().is_dir());
    let round_trip = server.connector.probe().await.unwrap();
    assert!(round_trip < Duration::from_secs(5), "{round_trip:?}");

    drop(fs);
    assert_eq!(
        server.connector.probe().await,
        Err(ConnectError::ConnectionLost),
        "the session went with its file system"
    );
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_probe_notices_a_silent_link_and_a_cut_one() {
    let proxy = Proxy::to(OPENSSH).await;
    let server = server_at("127.0.0.1", proxy.port, "~", Duration::from_secs(2));
    let fs = server.connector.connect(&TrustAll).await.unwrap();
    server.connector.probe().await.unwrap();

    proxy.set(Link::Stall);
    assert!(matches!(
        server.connector.probe().await,
        Err(ConnectError::Timeout { .. })
    ));

    proxy.set(Link::Cut);
    let lost = time::timeout(Duration::from_secs(10), async {
        loop {
            if server.connector.probe().await == Err(ConnectError::ConnectionLost) {
                return;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
    });
    lost.await.expect("a cut link must read as lost");
    drop(fs);
}

/// `send_ping` also returns when the session dies with the reply still due, as if it had come.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_probe_in_flight_when_the_connection_ends_says_so() {
    let proxy = Proxy::to(OPENSSH).await;
    let server = server_at("127.0.0.1", proxy.port, "~", Duration::from_secs(15));
    let _fs = server.connector.connect(&TrustAll).await.unwrap();

    proxy.set(Link::Stall);
    let (probe, ()) = tokio::join!(server.connector.probe(), async {
        time::sleep(Duration::from_millis(200)).await;
        proxy.set(Link::Cut);
    });
    assert_eq!(probe, Err(ConnectError::ConnectionLost));
}

// ---- Failures during use. ----

/// The operation deadline (spec §10.2): the backend's own limit reads as a timeout.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_silent_server_gives_a_timeout() {
    let proxy = Proxy::to(OPENSSH).await;
    let server = server_at("127.0.0.1", proxy.port, "~", Duration::from_secs(1));
    let fs = server.fs().await;
    proxy.set(Link::Stall);

    let start = std::time::Instant::now();
    assert_eq!(fs.stat(&RemotePath::root()).await, Err(FsError::Timeout));
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(3),
        "{took:?}"
    );
}

/// Whatever string the session dies with, it is one error, in flight and afterwards.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_cut_connection_is_connection_lost() {
    let proxy = Proxy::to(OPENSSH).await;
    let server = server_at("127.0.0.1", proxy.port, "~", Duration::from_secs(15));
    let fs = Arc::new(server.fs().await);
    let file = unique("cut");
    put(&fs, &file, &pattern(8 * MIB)).await;

    let handle = fs.open(&file, OpenMode::read()).await.unwrap();
    proxy.set(Link::Stall);
    let reading = {
        let fs = Arc::clone(&fs);
        tokio::spawn(async move { fs.read(&handle, 0, 8 * MIB as u32).await })
    };
    time::sleep(Duration::from_millis(100)).await;
    proxy.set(Link::Cut);

    let result = time::timeout(Duration::from_secs(10), reading).await;
    assert_eq!(
        result.expect("must end at once").unwrap(),
        Err(FsError::ConnectionLost)
    );
    assert_eq!(
        fs.stat(&RemotePath::root()).await,
        Err(FsError::ConnectionLost)
    );

    // Tidy up over a connection of its own.
    server_for_cleanup().await.remove_file(&file).await.unwrap();
}

async fn server_for_cleanup() -> SftpFs {
    server(OPENSSH, "~").fs().await
}

// ---- Chunks. ----

async fn longer_than_a_chunk(port: u16) {
    let server = server(port, "~");
    let fs = server.fs().await;
    let chunk = fs.capabilities().max_write as usize;
    let file = unique("chunks");
    let data = pattern(20 * chunk + 123);

    // One write and one read, each many times the largest request.
    put(&fs, &file, &data).await;
    let handle = fs.open(&file, OpenMode::read()).await.unwrap();
    let got = fs
        .read(&handle, 0, data.len() as u32)
        .await
        .expect("a long read");
    assert!(got == data, "a long read must return the data in order");

    // Over the end, from a place that is not a chunk boundary.
    let offset = data.len() - chunk - 7;
    let got = fs.read(&handle, offset as u64, 5 * chunk as u32).await;
    assert!(
        got.unwrap() == data[offset..],
        "a long read over the end must stop there"
    );
    assert!(
        fs.read(&handle, data.len() as u64, 5 * chunk as u32)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fs.read(&handle, 0, 0).await.unwrap().is_empty());
    fs.close(handle).await.unwrap();
    fs.remove_file(&file).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn reads_and_writes_longer_than_a_chunk_work_on_openssh() {
    longer_than_a_chunk(OPENSSH).await;
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn reads_and_writes_longer_than_a_chunk_work_on_proftpd() {
    longer_than_a_chunk(PROFTPD).await;
}

/// The way the WinFsp layer reads a file: `SftpFs` under `ReadAhead`.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn read_ahead_works_over_sftp() {
    let server = server(OPENSSH, "~");
    let fs = Arc::new(server.fs().await);
    let file = unique("ahead");
    let data = pattern(24 * MIB);
    put(&fs, &file, &data).await;

    let handle = fs.open(&file, OpenMode::read()).await.unwrap();
    let reader = ReadAhead::new(Arc::clone(&fs), handle);
    let mut out = Vec::with_capacity(data.len());
    loop {
        let got = reader.read(out.len() as u64, MIB as u32).await.unwrap();
        if got.is_empty() {
            break;
        }
        out.extend_from_slice(&got);
    }
    reader.close().await.unwrap();
    assert!(out == data, "the data must come back the same");
    fs.remove_file(&file).await.unwrap();
}

// ---- A rekey during a transfer. ----

/// The server rekeys after every MiB, with requests going both ways all the time.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_transfer_survives_rekeying() {
    let server = server(REKEYING, "~");
    let session = server.session().await;
    let before = session.key_exchanges();
    let fs = SftpFs::new(session);
    let file = unique("rekey");
    let data = pattern(32 * MIB);

    put(&fs, &file, &data).await;
    let got = get(&fs, &file).await;
    assert!(got == data, "the data must come back the same");
    fs.remove_file(&file).await.unwrap();
    drop(fs);

    let after = server.session().await.key_exchanges();
    // 64 MiB at one per MiB, less whatever batching does; plus the new connection's own.
    assert!(after >= before + 20, "{before} then {after}");
}

// ---- Handles. ----

/// Why `MAX_OPEN_FILES` exists. If this fails after an upgrade, `russh-sftp` has stopped garbling
/// handles: the limit can go, and with it this test.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn russh_sftp_garbles_the_129th_openssh_handle() {
    let server = server(OPENSSH, "~");
    let session = server.session().await;
    let sftp = session.sftp();
    let mut handles = Vec::new();
    for _ in 0..129 {
        let open = sftp.open("/etc/passwd", OpenFlags::READ, FileAttributes::default());
        handles.push(open.await.unwrap().handle);
    }
    for handle in &handles[..128] {
        assert!(!handle.contains(char::REPLACEMENT_CHARACTER));
        sftp.read(handle.as_str(), 0, 1).await.unwrap();
    }
    assert!(handles[128].contains(char::REPLACEMENT_CHARACTER));
    assert!(sftp.read(handles[128].as_str(), 0, 1).await.is_err());
}

/// Why `SftpFs::rename` looks first on such a server. If this fails after an upgrade, `russh-sftp`
/// has learned to keep going: the check can go, and with it this test.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn russh_sftp_loses_the_session_over_proftpds_rename_refusal() {
    let server = server(PROFTPD, "~");
    let fs = server.fs().await;
    let (from, to) = (unique("raw-from"), unique("raw-to"));
    put(&fs, &from, b"").await;
    put(&fs, &to, b"").await;
    drop(fs);

    let session = server.session().await;
    let sftp = session.sftp();
    let home = session.root().as_str();
    let (raw_from, raw_to) = (
        format!("{home}{}", from.as_str()),
        format!("{home}{}", to.as_str()),
    );
    let refusal = sftp.rename(raw_from.as_str(), raw_to.as_str()).await;
    println!("the refusal: {refusal:?}");
    assert!(refusal.is_err());
    let after = sftp.stat(raw_from.as_str()).await;
    println!("the next request: {after:?}");
    assert!(after.is_err(), "the channel is gone");
    drop(session);

    let fs = server.fs().await;
    fs.remove_file(&from).await.unwrap();
    fs.remove_file(&to).await.unwrap();
}

async fn the_limit_on_open_files_is_clean(port: u16) {
    let server = server(port, "~");
    let fs = server.fs().await;
    let file = unique("handles");
    put(&fs, &file, b"data").await;

    let mut handles: Vec<RemoteHandle> = Vec::new();
    for _ in 0..MAX_OPEN_FILES {
        handles.push(fs.open(&file, OpenMode::read()).await.unwrap());
    }
    let refused = fs.open(&file, OpenMode::read()).await.unwrap_err();
    assert!(matches!(refused, FsError::Other(_)), "{refused:?}");

    // Listings and other requests carry on at the limit, and several listings fit beside it.
    let root = RemotePath::root();
    let (a, b, c, d) = tokio::join!(
        fs.read_dir(&root),
        fs.read_dir(&root),
        fs.read_dir(&root),
        fs.read_dir(&root),
    );
    for listing in [a, b, c, d] {
        assert!(!listing.unwrap().is_empty());
    }
    for handle in &handles {
        assert_eq!(fs.read(handle, 0, 4).await.unwrap().as_ref(), b"data");
    }

    // Closing one makes room for one.
    fs.close(handles.pop().unwrap()).await.unwrap();
    handles.push(fs.open(&file, OpenMode::read()).await.unwrap());
    for handle in handles {
        fs.close(handle).await.unwrap();
    }
    fs.remove_file(&file).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_limit_on_open_files_is_clean_on_openssh() {
    the_limit_on_open_files_is_clean(OPENSSH).await;
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_limit_on_open_files_is_clean_on_proftpd() {
    the_limit_on_open_files_is_clean(PROFTPD).await;
}

/// More than fit in the listing slots at once, while files are open too.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn listings_beyond_their_limit_wait_their_turn() {
    let server = server(OPENSSH, "~");
    let fs = server.fs().await;
    let root = RemotePath::root();
    let listings = (0..64).map(|_| fs.read_dir(&root));
    for listing in futures::future::join_all(listings).await {
        listing.unwrap();
    }
}

// ---- Rename without posix-rename. ----

/// Removing the target first would lose it here: the target's folder allows that, but the source
/// cannot leave its own.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_failed_rename_by_hand_gives_the_target_back() {
    let server = server(PROFTPD, "~");
    let fs = server.fs().await;
    let base = unique("aside");
    fs.mkdir(&base).await.unwrap();
    let locked = base.join("locked").unwrap();
    fs.mkdir(&locked).await.unwrap();
    let from = locked.join("from").unwrap();
    let to = base.join("to").unwrap();
    put(&fs, &from, b"source").await;
    put(&fs, &to, b"target").await;
    let read_only = AttrChanges {
        read_only: Some(true),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&locked), read_only)
        .await
        .unwrap();

    let result = fs.rename(&from, &to, true).await;

    let writable = AttrChanges {
        read_only: Some(false),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&locked), writable)
        .await
        .unwrap();
    assert!(result.is_err(), "the source's folder is read-only");
    assert_eq!(get(&fs, &to).await, b"target");
    assert_eq!(get(&fs, &from).await, b"source");
    let names: Vec<_> = fs
        .read_dir(&base)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names.len(), 2, "nothing left over: {names:?}");

    fs.remove_file(&from).await.unwrap();
    fs.remove_file(&to).await.unwrap();
    fs.remove_dir(&locked).await.unwrap();
    fs.remove_dir(&base).await.unwrap();
}

// ---- What only a prepared server can show. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn symbolic_links_follow_the_trait() {
    let server = server(OPENSSH, "/srv/fixture");
    let fs = server.fs().await;

    let link = path("/link-to-file");
    assert_eq!(fs.stat(&link).await.unwrap().kind, FileKind::File);
    assert_eq!(fs.stat(&link).await.unwrap().size, 7);
    assert_eq!(fs.lstat(&link).await.unwrap().kind, FileKind::Symlink);
    assert_eq!(get(&fs, &link).await, b"target\n");

    let dir_link = path("/link-to-dir");
    assert_eq!(fs.stat(&dir_link).await.unwrap().kind, FileKind::Dir);
    assert_eq!(fs.lstat(&dir_link).await.unwrap().kind, FileKind::Symlink);
    assert!(fs.read_dir(&dir_link).await.unwrap().is_empty());

    let broken = path("/broken-link");
    assert_eq!(fs.stat(&broken).await, Err(FsError::NotFound));
    assert_eq!(fs.lstat(&broken).await.unwrap().kind, FileKind::Symlink);
    assert_eq!(
        fs.open(&broken, OpenMode::read()).await,
        Err(FsError::NotFound)
    );

    let listing = fs.read_dir(&RemotePath::root()).await.unwrap();
    let kind = |name: &str| {
        let entry = listing.iter().find(|entry| entry.name == name);
        entry.unwrap().attr.kind
    };
    assert_eq!(kind("file"), FileKind::File);
    assert_eq!(kind("dir"), FileKind::Dir);
    for name in ["link-to-file", "link-to-dir", "broken-link"] {
        assert_eq!(kind(name), FileKind::Symlink, "{name}");
    }
}

/// `caf\xE9`: `russh-sftp` turns its last byte into U+FFFD, and the true name is lost.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_name_that_is_not_utf8_is_listed_but_cannot_be_reached() {
    let server = server(OPENSSH, "/srv/fixture");
    let fs = server.fs().await;
    let listing = fs.read_dir(&RemotePath::root()).await.unwrap();
    let name = "caf\u{FFFD}";
    assert!(listing.iter().any(|entry| entry.name == name));

    let file = RemotePath::root().join(name).unwrap();
    assert_eq!(fs.stat(&file).await, Err(FsError::NotFound));
    // The session is fine.
    assert!(fs.stat(&path("/file")).await.is_ok());
}

/// Spec §5.8: a failed write after `statvfs` shows nothing free.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_full_disk_is_no_space() {
    let server = server(OPENSSH, "/srv/small");
    let fs = server.fs().await;
    // A run that failed before tidying up must not fail every later run for that reason.
    let root = RemotePath::root();
    for entry in fs.read_dir(&root).await.unwrap() {
        let left = root.join(&entry.name).unwrap();
        fs.remove_file(&left).await.unwrap();
    }
    let file = unique("full");
    let handle = fs.open(&file, CREATE).await.unwrap();

    let result = fs.write(&handle, 0, Bytes::from(pattern(3 * MIB))).await;
    let free_when_full = fs.statfs().await.unwrap().free_bytes;

    fs.close(handle).await.unwrap();
    fs.remove_file(&file).await.unwrap();
    assert_eq!(result, Err(FsError::NoSpace));
    assert_eq!(free_when_full, 0);
    assert!(fs.statfs().await.unwrap().free_bytes > 0);
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn statfs_reports_the_servers_numbers() {
    let server = server(OPENSSH, "/srv/small");
    let stats = server.fs().await.statfs().await.unwrap();
    assert_eq!(stats.total_bytes, MIB as u64);
    assert_ne!(stats, FsStats::PLACEHOLDER);
}

// ---- Handles know their access, on any server. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn access_is_checked_before_anything_is_sent() {
    let server = server(PROFTPD, "~");
    let fs = server.fs().await;
    let file = unique("access");
    put(&fs, &file, b"data").await;

    let reader = fs.open(&file, OpenMode::read()).await.unwrap();
    let writer = fs
        .open(&file, OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    assert_eq!(
        fs.write(&reader, 0, Bytes::from_static(b"x")).await,
        Err(FsError::PermissionDenied)
    );
    assert_eq!(fs.read(&writer, 0, 1).await, Err(FsError::PermissionDenied));
    fs.close(reader).await.unwrap();
    fs.close(writer).await.unwrap();
    fs.remove_file(&file).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_handle_of_another_file_system_is_refused() {
    let server = server(OPENSSH, "~");
    let fs = server.fs().await;
    let foreign = RemoteHandle::new(u64::MAX);
    assert!(matches!(fs.fstat(&foreign).await, Err(FsError::Other(_))));
    assert!(matches!(
        fs.read(&foreign, 0, 1).await,
        Err(FsError::Other(_))
    ));
    assert!(matches!(fs.close(foreign).await, Err(FsError::Other(_))));
}

// ---- Windows-style paths (FR-SSH-13), on a Linux server that shows them that way. ----

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn drive_letter_paths_work_where_the_server_uses_them() {
    for (wanted, root) in [
        ("~", "/C:/Users/tester"),
        ("C:/data", "/C:/data"),
        ("C:\\data", "/C:/data"),
        ("/C:/data", "/C:/data"),
        ("C:\\Users\\tester", "/C:/Users/tester"),
    ] {
        let session = server(WINDOWS_PATHS, wanted).session().await;
        assert_eq!(session.root().as_str(), root, "{wanted}");
    }

    let session = server(WINDOWS_PATHS, "C:\\no\\such");
    let error = session.connector.connect_session(&TrustAll).await;
    assert!(matches!(error, Err(ConnectError::PathNotFound { .. })));

    let fs = server(WINDOWS_PATHS, "C:\\data").fs().await;
    let file = unique("windows");
    put(&fs, &file, b"data").await;
    assert_eq!(get(&fs, &file).await, b"data");
    fs.remove_file(&file).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn drive_letter_paths_are_not_found_on_other_servers() {
    for wanted in ["C:/data", "C:\\data"] {
        let session = server(OPENSSH, wanted);
        let error = session.connector.connect_session(&TrustAll).await;
        assert!(
            matches!(&error, Err(ConnectError::PathNotFound { path }) if path == wanted),
            "{wanted}"
        );
    }
}
