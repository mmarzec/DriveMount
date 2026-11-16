// SPDX-License-Identifier: GPL-3.0-or-later

//! `NfsFs` (M4-03): the contract suite and what is particular to NFS, against the knfsd test
//! server. All of it needs the S-03 servers:
//!
//! ```text
//! tests/docker/servers.ps1 up
//! cargo test -p drivemount-nfs --test nfsfs -- --ignored
//! ```
//!
//! Set `DRIVEMOUNT_TEST_HOST` to the WSL VM's IP (`wsl hostname -I`). The tests use the
//! `insecure` and `ro` exports, from any source port: binding a reserved one is M4-04's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use drivemount_nfs::{Dialer, FileHandle, Identity, NfsFs, NfsOptions};
use drivemount_vfs::{
    Disposition, FileKind, FsError, OpenMode, RemoteFs, RemotePath, async_trait, contract,
};
use nfs3_client::nfs3_types::mount::dirpath;
use nfs3_client::nfs3_types::nfs3::{
    GETATTR3args, LOOKUP3args, Nfs3Result, REMOVE3args, SYMLINK3args, diropargs3, filename3,
    nfs_fh3, nfspath3, sattr3, symlinkdata3,
};
use nfs3_client::nfs3_types::rpc::{auth_unix, opaque_auth};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::tokio::TokioIo;
use nfs3_client::{MountClient, Nfs3Client};
use tokio::net::TcpStream;

const NFS_PORT: u16 = 2049;
const MOUNT_PORT: u16 = 20048;
const INSECURE: &str = "/srv/nfs/insecure";
const READ_ONLY: &str = "/srv/nfs/ro";
const ME: Identity = Identity {
    uid: 1000,
    gid: 1000,
};

fn host() -> String {
    std::env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

async fn address(port: u16) -> SocketAddr {
    tokio::net::lookup_host((host(), port))
        .await
        .unwrap()
        .next()
        .unwrap()
}

struct Plain(SocketAddr);

#[async_trait]
impl Dialer for Plain {
    async fn dial(&self) -> io::Result<TcpStream> {
        let stream = TcpStream::connect(self.0).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

/// MOUNT, as `NfsConnector` will do it: the export's root handle.
async fn mount(export: &str) -> FileHandle {
    let stream = TcpStream::connect(address(MOUNT_PORT).await).await.unwrap();
    let mut client = MountClient::new(TokioIo::new(stream));
    let mounted = client
        .mnt(dirpath(Opaque::borrowed(export.as_bytes())))
        .await
        .unwrap();
    FileHandle::new(&mounted.fhandle.0)
}

fn options(export: &str, identity: Identity) -> NfsOptions {
    NfsOptions {
        identity,
        export: export.to_owned(),
        connections: NfsOptions::DEFAULT_CONNECTIONS,
        call_timeout: Duration::from_secs(15),
        handle_ttl: Duration::from_secs(60),
    }
}

async fn fs_as(export: &str, identity: Identity) -> NfsFs {
    let dialer = Box::new(Plain(address(NFS_PORT).await));
    NfsFs::new(dialer, mount(export).await, options(export, identity))
        .await
        .unwrap()
}

/// Another client of the same export, for what `RemoteFs` cannot do and for changes this mount
/// does not see coming.
struct Raw {
    client: Nfs3Client<TokioIo<TcpStream>>,
}

impl Raw {
    async fn new() -> Self {
        let stream = TcpStream::connect(address(NFS_PORT).await).await.unwrap();
        let credential = opaque_auth::auth_unix(&auth_unix {
            stamp: 0,
            machinename: Opaque::borrowed(b"drivemount-test"),
            uid: ME.uid,
            gid: ME.gid,
            gids: vec![ME.gid],
        });
        let io = TokioIo::new(stream);
        Self {
            client: Nfs3Client::new_with_auth(io, credential, opaque_auth::default()),
        }
    }

    async fn lookup(&mut self, dir: &nfs_fh3, name: &str) -> nfs_fh3 {
        let what = diropargs3 {
            dir: dir.clone(),
            name: filename3::from(name.as_bytes()),
        };
        match self.client.lookup(&LOOKUP3args { what }).await.unwrap() {
            Nfs3Result::Ok(found) => found.object,
            Nfs3Result::Err((stat, _)) => panic!("LOOKUP {name}: {stat}"),
        }
    }

    /// The handle of an export-relative path.
    async fn resolve(&mut self, root: &FileHandle, path: &str) -> nfs_fh3 {
        let mut handle = fh(root);
        for name in path.split('/').filter(|name| !name.is_empty()) {
            handle = self.lookup(&handle, name).await;
        }
        handle
    }

    async fn owner(&mut self, object: &nfs_fh3) -> (u32, u32) {
        let args = GETATTR3args {
            object: object.clone(),
        };
        let attr = self.client.getattr(&args).await.unwrap().unwrap();
        (attr.obj_attributes.uid, attr.obj_attributes.gid)
    }

    async fn symlink(&mut self, dir: &nfs_fh3, name: &str, target: &str) {
        let args = SYMLINK3args {
            where_: diropargs3 {
                dir: dir.clone(),
                name: filename3::from(name.as_bytes()),
            },
            symlink: symlinkdata3 {
                symlink_attributes: sattr3::default(),
                symlink_data: nfspath3::from(target.as_bytes()),
            },
        };
        self.client.symlink(&args).await.unwrap().unwrap();
    }

    async fn remove(&mut self, dir: &nfs_fh3, name: &str) {
        let object = diropargs3 {
            dir: dir.clone(),
            name: filename3::from(name.as_bytes()),
        };
        self.client
            .remove(&REMOVE3args { object })
            .await
            .unwrap()
            .unwrap();
    }
}

fn fh(handle: &FileHandle) -> nfs_fh3 {
    nfs_fh3 {
        data: Opaque::owned(handle.as_bytes().to_vec()),
    }
}

fn path(text: &str) -> RemotePath {
    RemotePath::new(text).unwrap()
}

/// A new directory of the test's own in the export, removed by hand if a test fails.
async fn scratch(fs: &NfsFs, name: &str) -> RemotePath {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = path(&format!("/{name}-{}-{nanos}", std::process::id()));
    fs.mkdir(&dir).await.unwrap();
    dir
}

async fn write_file(fs: &NfsFs, file: &RemotePath, data: &[u8]) {
    let handle = fs
        .open(file, OpenMode::write(Disposition::CreateNew))
        .await
        .unwrap();
    fs.write(&handle, 0, data.to_vec().into()).await.unwrap();
    fs.close(handle).await.unwrap();
}

async fn read_file(fs: &NfsFs, file: &RemotePath) -> Vec<u8> {
    let handle = fs.open(file, OpenMode::read()).await.unwrap();
    let data = fs.read(&handle, 0, 1 << 16).await.unwrap();
    fs.close(handle).await.unwrap();
    data.to_vec()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn the_contract_holds_on_knfsd() {
    let fs = fs_as(INSECURE, ME).await;
    contract::run_all(&fs, &RemotePath::root()).await;
    let capabilities = fs.capabilities();
    assert!(capabilities.posix_rename && capabilities.fsync && capabilities.statvfs);
    // knfsd offers 1 MiB (spike M4-01 §3), which is also the most asked for.
    assert_eq!(capabilities.max_read, 1024 * 1024);
    assert_eq!(capabilities.max_write, 1024 * 1024);
}

/// FR-NFS-01: files are created as the profile's UID and GID, whatever they are.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn new_files_belong_to_the_configured_user() {
    let root = mount(INSECURE).await;
    let other = Identity {
        uid: 1234,
        gid: 5678,
    };
    let fs = fs_as(INSECURE, other).await;
    let dir = scratch(&fs, "owner").await;
    let file = dir.join("file").unwrap();
    write_file(&fs, &file, b"mine").await;

    let mut raw = Raw::new().await;
    let file_fh = raw.resolve(&root, file.as_str()).await;
    let dir_fh = raw.resolve(&root, dir.as_str()).await;
    assert_eq!(raw.owner(&file_fh).await, (1234, 5678));
    assert_eq!(raw.owner(&dir_fh).await, (1234, 5678));

    let attr = fs.stat(&file).await.unwrap();
    assert_eq!(attr.perm, Some(0o644));
    assert_eq!(fs.stat(&dir).await.unwrap().perm, Some(0o755));

    fs.remove_file(&file).await.unwrap();
    fs.remove_dir(&dir).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn symbolic_links_at_the_end_of_a_path_are_followed_within_the_export() {
    let root = mount(INSECURE).await;
    let fs = fs_as(INSECURE, ME).await;
    let dir = scratch(&fs, "links").await;
    write_file(&fs, &dir.join("target").unwrap(), b"through a link").await;
    fs.mkdir(&dir.join("sub").unwrap()).await.unwrap();

    let mut raw = Raw::new().await;
    let dir_fh = raw.resolve(&root, dir.as_str()).await;
    let absolute = format!("{INSECURE}{}/target", dir.as_str());
    raw.symlink(&dir_fh, "relative", "target").await;
    raw.symlink(&dir_fh, "absolute", &absolute).await;
    raw.symlink(&dir_fh, "to-dir", "sub").await;
    raw.symlink(&dir_fh, "broken", "nothing-here").await;
    raw.symlink(&dir_fh, "outside", "/etc/passwd").await;
    raw.symlink(&dir_fh, "escaping", "../../../etc").await;

    for name in ["relative", "absolute"] {
        let link = dir.join(name).unwrap();
        assert_eq!(fs.lstat(&link).await.unwrap().kind, FileKind::Symlink);
        let attr = fs.stat(&link).await.unwrap();
        assert_eq!((attr.kind, attr.size), (FileKind::File, 14), "{name}");
        assert_eq!(read_file(&fs, &link).await, b"through a link", "{name}");
    }
    let to_dir = dir.join("to-dir").unwrap();
    assert!(fs.stat(&to_dir).await.unwrap().is_dir());
    assert!(fs.read_dir(&to_dir).await.unwrap().is_empty());
    for name in ["broken", "outside", "escaping"] {
        let link = dir.join(name).unwrap();
        assert_eq!(fs.stat(&link).await, Err(FsError::NotFound), "{name}");
        assert_eq!(fs.lstat(&link).await.unwrap().kind, FileKind::Symlink);
    }

    // A listing shows the links themselves, and removing one leaves its target.
    let listed = fs.read_dir(&dir).await.unwrap();
    let kind = |name: &str| {
        listed
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
            .attr
            .kind
    };
    assert_eq!(kind("relative"), FileKind::Symlink);
    assert_eq!(kind("target"), FileKind::File);
    for name in [
        "relative", "absolute", "to-dir", "broken", "outside", "escaping", "target",
    ] {
        fs.remove_file(&dir.join(name).unwrap()).await.unwrap();
    }
    fs.remove_dir(&dir.join("sub").unwrap()).await.unwrap();
    fs.remove_dir(&dir).await.unwrap();
}

/// M4-02's re-resolve, end to end: another client replaces a file this mount has a cached handle
/// for.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_file_replaced_by_another_client_is_found_again() {
    let root = mount(INSECURE).await;
    let fs = fs_as(INSECURE, ME).await;
    let dir = scratch(&fs, "stale").await;
    let file = dir.join("file").unwrap();
    write_file(&fs, &file, b"first").await;
    assert_eq!(fs.stat(&file).await.unwrap().size, 5);

    let mut raw = Raw::new().await;
    let dir_fh = raw.resolve(&root, dir.as_str()).await;
    raw.remove(&dir_fh, "file").await;
    // The same name, a new file: the cached handle is stale now.
    raw.symlink(&dir_fh, "file", "elsewhere").await;
    assert_eq!(fs.lstat(&file).await.unwrap().kind, FileKind::Symlink);

    raw.remove(&dir_fh, "file").await;
    assert_eq!(fs.lstat(&file).await, Err(FsError::NotFound));
    fs.remove_dir(&dir).await.unwrap();
}

/// An open file keeps its handle: removed under it by another client, it is gone.
#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn an_open_file_removed_by_another_client_is_not_found() {
    let root = mount(INSECURE).await;
    let fs = fs_as(INSECURE, ME).await;
    let dir = scratch(&fs, "gone").await;
    let file = dir.join("file").unwrap();
    write_file(&fs, &file, b"data").await;
    let handle = fs.open(&file, OpenMode::read()).await.unwrap();

    let mut raw = Raw::new().await;
    let dir_fh = raw.resolve(&root, dir.as_str()).await;
    raw.remove(&dir_fh, "file").await;
    assert_eq!(fs.fstat(&handle).await, Err(FsError::NotFound));
    assert_eq!(fs.read(&handle, 0, 4).await, Err(FsError::NotFound));
    fs.close(handle).await.unwrap();
    fs.remove_dir(&dir).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the S-03 servers: tests/docker/servers.ps1 up"]
async fn a_read_only_export_reads_and_refuses_changes() {
    let fs = fs_as(READ_ONLY, ME).await;
    let hello = path("/hello.txt");
    assert_eq!(read_file(&fs, &hello).await, b"hello\n");
    let names: Vec<String> = fs
        .read_dir(&RemotePath::root())
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["hello.txt"]);

    let write = OpenMode::read_write(Disposition::OpenExisting);
    assert_eq!(fs.open(&hello, write).await, Err(FsError::PermissionDenied));
    assert_eq!(fs.mkdir(&path("/new")).await, Err(FsError::ReadOnly));
    let create = OpenMode::write(Disposition::CreateNew);
    assert_eq!(fs.open(&path("/new"), create).await, Err(FsError::ReadOnly));
    assert_eq!(fs.remove_file(&hello).await, Err(FsError::ReadOnly));
    let stats = fs.statfs().await.unwrap();
    assert!(stats.total_bytes > 0);
}
