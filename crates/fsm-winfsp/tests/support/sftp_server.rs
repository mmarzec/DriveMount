// SPDX-License-Identifier: GPL-3.0-or-later

//! An SFTP server inside the test process: `russh` and the server side of `russh-sftp`, over any
//! [`RemoteFs`] (spec §10.3). Between a drive and a `MemFs` it puts the real protocol, the real client
//! and a real socket, while tests still see the server's files directly.
//!
//! It behaves like OpenSSH where `SftpFs` can tell: the same extensions, and "already exists" and "not
//! empty" as the catch-all failure. Any password is accepted.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fsm_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, DirEntry, Disposition, FileKind, FsError, OpenMode,
    RemoteFs, RemoteHandle, RemotePath,
};
use russh::keys::PrivateKey;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, Disconnect};
use russh_sftp::extensions::{
    FSYNC, FsyncExtension, LIMITS, LimitsExtension, STATVFS, Statvfs, StatvfsExtension,
};
use russh_sftp::protocol::{
    Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status,
    StatusCode, Version,
};
use russh_sftp::server::{Config as SftpConfig, Handler, StatusReply, run_with_config};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const POSIX_RENAME: &str = "posix-rename@openssh.com";
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const WRITE_BIT: u32 = 0o200;
/// Entries in one `READDIR` answer, as OpenSSH sends about this many.
const NAMES_PER_ANSWER: usize = 100;
/// What the server says it can do, which `SftpFs` keeps to.
const MAX_IO: u64 = 256 * 1024;
const MAX_PACKET: u32 = 512 * 1024;
const MAX_OPEN_HANDLES: u64 = 1024;

/// A server listening on a port of this machine, until it is dropped.
pub struct SftpServer {
    pub port: u16,
    accepting: JoinHandle<()>,
    sessions: Arc<Mutex<Vec<server::Handle>>>,
}

impl SftpServer {
    /// Starts serving `fs` on the current runtime.
    pub async fn start(fs: Arc<dyn RemoteFs>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(server::Config {
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32]))],
            nodelay: true,
            ..server::Config::default()
        });
        let sessions: Arc<Mutex<Vec<server::Handle>>> = Arc::default();
        let running = Arc::clone(&sessions);
        let accepting = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = SshSession {
                    fs: Arc::clone(&fs),
                    channels: HashMap::new(),
                };
                let Ok(session) = server::run_stream(Arc::clone(&config), stream, handler).await
                else {
                    continue;
                };
                running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(session.handle());
                tokio::spawn(session);
            }
        });
        Self {
            port,
            accepting,
            sessions,
        }
    }

    /// Ends every connection, as a server going down would. New ones are still accepted.
    pub async fn drop_connections(&self) {
        let sessions: Vec<server::Handle> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        for session in sessions {
            // One that has already ended has nothing left to end.
            let _ = session
                .disconnect(Disconnect::ByApplication, String::new(), String::new())
                .await;
        }
    }
}

impl Drop for SftpServer {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

/// One SSH connection.
struct SshSession {
    fs: Arc<dyn RemoteFs>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(open) = self.channels.remove(&channel).filter(|_| name == "sftp") else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let handler = SftpSession {
            fs: Arc::clone(&self.fs),
            open: HashMap::new(),
            next_handle: 0,
        };
        let config = SftpConfig {
            max_client_packet_len: MAX_PACKET,
        };
        run_with_config(open.into_stream(), handler, config).await;
        Ok(())
    }
}

/// What a handle of this session stands for.
enum Open {
    File(RemoteHandle),
    Directory(VecDeque<DirEntry>),
}

/// One SFTP channel. Requests are handled one at a time, in order.
struct SftpSession {
    fs: Arc<dyn RemoteFs>,
    open: HashMap<String, Open>,
    next_handle: u64,
}

impl SftpSession {
    fn keep(&mut self, open: Open) -> String {
        self.next_handle += 1;
        let handle = self.next_handle.to_string();
        self.open.insert(handle.clone(), open);
        handle
    }

    fn file(&self, handle: &str) -> Result<&RemoteHandle, StatusReply> {
        match self.open.get(handle) {
            Some(Open::File(file)) => Ok(file),
            _ => Err(StatusReply::new(StatusCode::Failure).with_message("no such handle")),
        }
    }
}

fn path(path: &str) -> Result<RemotePath, StatusReply> {
    RemotePath::new(path).map_err(|_| StatusCode::NoSuchFile.into())
}

fn refusal(error: FsError) -> StatusReply {
    match error {
        FsError::NotFound | FsError::PathNotFound => StatusCode::NoSuchFile.into(),
        FsError::PermissionDenied => StatusCode::PermissionDenied.into(),
        FsError::NotSupported => StatusCode::OpUnsupported.into(),
        FsError::NoSpace => StatusReply::new(StatusCode::Failure).with_message("no space left"),
        other => StatusReply::new(StatusCode::Failure).with_message(other.to_string()),
    }
}

fn seconds(time: Option<SystemTime>) -> Option<u32> {
    let since = time?.duration_since(UNIX_EPOCH).ok()?;
    u32::try_from(since.as_secs()).ok()
}

fn wire_attrs(attr: &Attr) -> FileAttributes {
    let kind = match attr.kind {
        FileKind::Dir => S_IFDIR,
        FileKind::Symlink => S_IFLNK,
        _ => S_IFREG,
    };
    FileAttributes {
        size: Some(attr.size),
        permissions: Some(kind | attr.perm.unwrap_or(0o644)),
        mtime: seconds(attr.mtime),
        atime: seconds(attr.atime),
        ..FileAttributes::default()
    }
}

/// Only what `AttrChanges` can carry: the owner's write bit stands for the read-only attribute.
fn changes(attrs: &FileAttributes) -> AttrChanges {
    let time = |seconds: Option<u32>| {
        seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(u64::from(seconds)))
    };
    AttrChanges {
        size: attrs.size,
        read_only: attrs
            .permissions
            .map(|permissions| permissions & WRITE_BIT == 0),
        mtime: time(attrs.mtime),
        atime: time(attrs.atime),
    }
}

fn open_mode(flags: OpenFlags) -> OpenMode {
    let create = flags.contains(OpenFlags::CREATE);
    let truncate = flags.contains(OpenFlags::TRUNCATE);
    let disposition = if create && flags.contains(OpenFlags::EXCLUDE) {
        Disposition::CreateNew
    } else if create && truncate {
        Disposition::CreateOrTruncate
    } else if create {
        Disposition::OpenOrCreate
    } else if truncate {
        Disposition::TruncateExisting
    } else {
        Disposition::OpenExisting
    };
    match (
        flags.contains(OpenFlags::READ),
        flags.contains(OpenFlags::WRITE),
    ) {
        (true, true) => OpenMode::read_write(disposition),
        (false, true) => OpenMode::write(disposition),
        _ => OpenMode::read(),
    }
}

fn done(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_owned(),
        language_tag: "en-US".to_owned(),
    }
}

/// Reads an SSH string, as the extended requests hold them.
fn take_string(data: &mut &[u8]) -> Option<String> {
    let (len, rest) = data.split_first_chunk::<4>()?;
    let len = u32::from_be_bytes(*len) as usize;
    let (text, rest) = (rest.get(..len)?, rest.get(len..)?);
    *data = rest;
    String::from_utf8(text.to_vec()).ok()
}

impl Handler for SftpSession {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        for (name, revision) in [
            (POSIX_RENAME, "1"),
            (FSYNC, "1"),
            (STATVFS, "2"),
            (LIMITS, "1"),
        ] {
            version
                .extensions
                .insert(name.to_owned(), revision.to_owned());
        }
        Ok(version)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let file = self
            .fs
            .open(&path(&filename)?, open_mode(pflags))
            .await
            .map_err(refusal)?;
        let handle = self.keep(Open::File(file));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.open.remove(&handle) {
            Some(Open::File(file)) => self.fs.close(file).await.map_err(refusal)?,
            Some(Open::Directory(_)) => {}
            None => return Err(StatusCode::Failure.into()),
        }
        Ok(done(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let data = self
            .fs
            .read(self.file(&handle)?, offset, len)
            .await
            .map_err(refusal)?;
        if data.is_empty() {
            return Err(StatusCode::Eof.into());
        }
        Ok(Data {
            id,
            data: data.to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.fs
            .write(self.file(&handle)?, offset, Bytes::from(data))
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn lstat(&mut self, id: u32, path_name: String) -> Result<Attrs, Self::Error> {
        let attr = self.fs.lstat(&path(&path_name)?).await.map_err(refusal)?;
        Ok(Attrs {
            id,
            attrs: wire_attrs(&attr),
        })
    }

    async fn stat(&mut self, id: u32, path_name: String) -> Result<Attrs, Self::Error> {
        let attr = self.fs.stat(&path(&path_name)?).await.map_err(refusal)?;
        Ok(Attrs {
            id,
            attrs: wire_attrs(&attr),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let attr = self.fs.fstat(self.file(&handle)?).await.map_err(refusal)?;
        Ok(Attrs {
            id,
            attrs: wire_attrs(&attr),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path_name: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let target = path(&path_name)?;
        self.fs
            .set_attr(AttrTarget::Path(&target), changes(&attrs))
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.fs
            .set_attr(AttrTarget::Handle(self.file(&handle)?), changes(&attrs))
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn opendir(&mut self, id: u32, path_name: String) -> Result<Handle, Self::Error> {
        let entries = self
            .fs
            .read_dir(&path(&path_name)?)
            .await
            .map_err(refusal)?;
        let handle = self.keep(Open::Directory(entries.into()));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(Open::Directory(entries)) = self.open.get_mut(&handle) else {
            return Err(StatusCode::Failure.into());
        };
        if entries.is_empty() {
            return Err(StatusCode::Eof.into());
        }
        let count = entries.len().min(NAMES_PER_ANSWER);
        let files = entries
            .drain(..count)
            .map(|entry| File::new(entry.name, wire_attrs(&entry.attr)))
            .collect();
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.fs
            .remove_file(&path(&filename)?)
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path_name: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.fs.mkdir(&path(&path_name)?).await.map_err(refusal)?;
        Ok(done(id))
    }

    async fn rmdir(&mut self, id: u32, path_name: String) -> Result<Status, Self::Error> {
        self.fs
            .remove_dir(&path(&path_name)?)
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn realpath(&mut self, id: u32, path_name: String) -> Result<Name, Self::Error> {
        let real = if path_name == "." {
            "/".to_owned()
        } else {
            path_name
        };
        Ok(Name {
            id,
            files: vec![File::dummy(real)],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.fs
            .rename(&path(&oldpath)?, &path(&newpath)?, false)
            .await
            .map_err(refusal)?;
        Ok(done(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        let malformed = || StatusReply::from(StatusCode::BadMessage);
        let reply = |bytes: Result<Bytes, _>| {
            let data: Bytes = bytes.map_err(|_| StatusReply::from(StatusCode::Failure))?;
            Ok(Packet::ExtendedReply(ExtendedReply {
                id,
                data: data.to_vec(),
            }))
        };
        match request.as_str() {
            LIMITS => reply(russh_sftp::ser::to_bytes(&LimitsExtension {
                max_packet_len: u64::from(MAX_PACKET),
                max_read_len: MAX_IO,
                max_write_len: MAX_IO,
                max_open_handles: MAX_OPEN_HANDLES,
            })),
            FSYNC => {
                let request: FsyncExtension =
                    russh_sftp::de::from_bytes(&mut data.into()).map_err(|_| malformed())?;
                self.fs
                    .flush(self.file(&request.handle)?)
                    .await
                    .map_err(refusal)?;
                Ok(Packet::Status(done(id)))
            }
            STATVFS => {
                let _: StatvfsExtension =
                    russh_sftp::de::from_bytes(&mut data.into()).map_err(|_| malformed())?;
                let stats = self.fs.statfs().await.map_err(refusal)?;
                reply(russh_sftp::ser::to_bytes(&Statvfs {
                    block_size: 1,
                    fragment_size: 1,
                    blocks: stats.total_bytes,
                    blocks_free: stats.free_bytes,
                    blocks_avail: stats.free_bytes,
                    inodes: 0,
                    inodes_free: 0,
                    inodes_avail: 0,
                    fs_id: 0,
                    flags: 0,
                    name_max: 255,
                }))
            }
            POSIX_RENAME => {
                let mut rest = data.as_slice();
                let from = take_string(&mut rest).ok_or_else(malformed)?;
                let to = take_string(&mut rest).ok_or_else(malformed)?;
                self.fs
                    .rename(&path(&from)?, &path(&to)?, true)
                    .await
                    .map_err(refusal)?;
                Ok(Packet::Status(done(id)))
            }
            _ => Err(StatusCode::OpUnsupported.into()),
        }
    }
}
