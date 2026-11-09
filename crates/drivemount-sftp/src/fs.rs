// SPDX-License-Identifier: GPL-3.0-or-later

//! [`SftpFs`]: [`RemoteFs`] over one SFTP channel (spec §5.6, §5.7, FR-SSH-14).
//!
//! Paths are relative to the profile's remote path, so `/` here is the folder the mount shows.
//!
//! # What SFTP v3 does not say
//!
//! The protocol has one code for most failures, and OpenSSH sends no detail with it. Where the trait
//! promises a particular error, a failed call looks at the server again to find out which it was.
//! Those checks are separate requests, so another client can change the answer between the two.
//!
//! # Open handles
//!
//! `russh-sftp` decodes file handles as lossy UTF-8, but they are opaque bytes. OpenSSH's are 4-byte
//! indexes that reuse the lowest free slots, so the first 128 survive and the 129th open at once
//! comes back garbled and useless. So a session holds at most [`MAX_OPEN_FILES`] files open, and a
//! handle that arrives garbled anyway, from a server with other handles or after an abandoned open
//! used up a slot, is refused.
//!
//! # Status codes
//!
//! `russh-sftp` knows only the nine codes of SFTP v3, and any other in a reply ends the session. So
//! requests that ProFTPD answers with a later code are not sent when the answer is known already.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use drivemount_vfs::{
    Access, Attr, AttrChanges, AttrTarget, Capabilities, ConnectError, Connector, DirEntry,
    Disposition, FileKind, FsError, FsResult, FsStats, OpenMode, Prompter, RemoteFs, RemoteHandle,
    RemotePath, apply_read_only,
};
use futures::future::join_all;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, Status, StatusCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};
use tracing::{debug, warn};

use crate::connector::{SftpConnector, SftpSession};

/// The largest request, whatever the server allows (spec §5.6).
pub(crate) const MAX_CHUNK: u32 = 4 * 1024 * 1024;
/// What SFTP v3 implementations must accept, for a server that does not say (spec §5.6).
const DEFAULT_CHUNK: u32 = 32 * 1024;
/// Room for a request's own header within the server's packet limit. OpenSSH leaves the same.
const PACKET_OVERHEAD: u64 = 1024;
const MAX_CHUNKS_IN_FLIGHT: usize = 64;
const MAX_BYTES_IN_FLIGHT: usize = MAX_CHUNK as usize;

/// Files a session can hold open. With [`MAX_OPEN_LISTINGS`], this is the 128 handles that survive
/// `russh-sftp`'s decoding (see the module's docs).
pub const MAX_OPEN_FILES: usize = 112;
/// Directories being listed at once. They are not held open, so further listings wait their turn
/// rather than fail.
const MAX_OPEN_LISTINGS: usize = 16;

const POSIX_RENAME: &str = "posix-rename@openssh.com";
const FSYNC: &str = "fsync@openssh.com";
const STATVFS: &str = "statvfs@openssh.com";

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const PERMISSION_BITS: u32 = 0o7777;

/// [`RemoteFs`] over one SFTP session.
pub struct SftpFs {
    session: Arc<SftpSession>,
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_id: AtomicU64,
    /// Names the target while a rename without `posix-rename` moves it out of the way.
    next_aside: AtomicU64,
    file_slots: Arc<Semaphore>,
    listing_slots: Semaphore,
    capabilities: Capabilities,
}

struct OpenFile {
    handle: String,
    access: Access,
    in_flight: InFlight,
    _slot: OwnedSemaphorePermit,
}

/// The limits on what one handle has in flight (spec §5.6).
struct InFlight {
    chunks: Semaphore,
    bytes: Semaphore,
}

impl InFlight {
    fn new() -> Self {
        Self {
            chunks: Semaphore::new(MAX_CHUNKS_IN_FLIGHT),
            bytes: Semaphore::new(MAX_BYTES_IN_FLIGHT),
        }
    }

    async fn enter(&self, len: u32) -> FsResult<(SemaphorePermit<'_>, SemaphorePermit<'_>)> {
        let closed = |_| FsError::Other("in-flight limit closed".to_owned());
        let chunk = self.chunks.acquire().await.map_err(closed)?;
        let bytes = self.bytes.acquire_many(len).await.map_err(closed)?;
        Ok((chunk, bytes))
    }
}

/// One request's part of a longer read or write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chunk {
    /// Where it goes in the file.
    offset: u64,
    /// Where it goes in the caller's data.
    start: usize,
    len: u32,
}

fn split(offset: u64, total: usize, chunk_size: u32) -> Vec<Chunk> {
    let chunk_size = chunk_size as usize;
    (0..total)
        .step_by(chunk_size)
        .map(|start| Chunk {
            offset: offset.saturating_add(start as u64),
            start,
            len: (total - start).min(chunk_size) as u32,
        })
        .collect()
}

/// Runs `request` for every chunk at once, within the limits. Requests start in order, and those
/// not started when one ends in anything but `carry_on` are skipped. Everything in flight is waited
/// for, so nothing is still happening on the server when this returns.
async fn pipelined<T, F, Fut>(
    in_flight: &InFlight,
    chunks: Vec<Chunk>,
    request: F,
    carry_on: impl Fn(&Chunk, &T) -> bool,
) -> Vec<FsResult<T>>
where
    T: Default,
    F: Fn(Chunk) -> Fut,
    Fut: Future<Output = FsResult<T>>,
{
    let stop = AtomicBool::new(false);
    let (stop, request, carry_on) = (&stop, &request, &carry_on);
    join_all(chunks.into_iter().map(|chunk| async move {
        let _entered = in_flight.enter(chunk.len).await?;
        if stop.load(Ordering::Relaxed) {
            return Ok(T::default());
        }
        let result = request(chunk).await;
        if !matches!(&result, Ok(value) if carry_on(&chunk, value)) {
            stop.store(true, Ordering::Relaxed);
        }
        result
    }))
    .await
}

/// Puts the chunks of a read back together, up to the first that fell short. A failure after some
/// data only makes the read short: the caller asks again and meets it then.
fn assemble(results: Vec<FsResult<Vec<u8>>>, chunk_size: u32) -> FsResult<Bytes> {
    let mut results = results.into_iter();
    let Some(first) = results.next() else {
        return Ok(Bytes::new());
    };
    let mut data = first?;
    if data.len() == chunk_size as usize {
        for result in results {
            let Ok(more) = result else { break };
            let complete = more.len() == chunk_size as usize;
            data.extend_from_slice(&more);
            if !complete {
                break;
            }
        }
    }
    Ok(Bytes::from(data))
}

/// The largest request for one direction. Neither limit is certain to be there.
pub(crate) fn chunk_size(direction_limit: Option<u64>, packet_limit: Option<u64>) -> u32 {
    let room = packet_limit.map_or(u64::MAX, |limit| limit.saturating_sub(PACKET_OVERHEAD));
    let wanted = direction_limit.unwrap_or(u64::from(DEFAULT_CHUNK));
    wanted.min(room).clamp(1, u64::from(MAX_CHUNK)) as u32
}

/// Spike M0-2: a dead session gives varying strings, so everything that is neither a status nor a
/// timeout means the connection is gone. `Limited` is the exception: it comes from this side, and
/// says nothing about the connection.
fn map_error(error: SftpError) -> FsError {
    match error {
        SftpError::Status(status) => map_status(&status),
        SftpError::Timeout => FsError::Timeout,
        SftpError::Limited(reason) => FsError::Other(format!("SFTP: {reason}")),
        error => {
            debug!(%error, "SFTP request failed");
            FsError::ConnectionLost
        }
    }
}

fn map_status(status: &Status) -> FsError {
    match status.status_code {
        StatusCode::NoSuchFile => FsError::NotFound,
        StatusCode::PermissionDenied => FsError::PermissionDenied,
        StatusCode::OpUnsupported => FsError::NotSupported,
        // Only a client should produce these two, but they mean what they say.
        StatusCode::NoConnection | StatusCode::ConnectionLost => FsError::ConnectionLost,
        code => {
            let message = status.error_message.to_ascii_lowercase();
            if message.contains("no space") || message.contains("quota") {
                FsError::NoSpace
            } else if message.contains("not a directory") {
                // ProFTPD: OpenSSH says "no such file" for a path through a file.
                FsError::PathNotFound
            } else {
                FsError::Other(format!("SFTP: {code}: {}", status.error_message))
            }
        }
    }
}

/// Whether the server gave its one catch-all code, which needs a closer look.
fn is_failure(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code == StatusCode::Failure)
}

/// Whether the server refused in a way that a name already taken could explain: not only with the
/// catch-all code, as ProFTPD denies an exclusive create on a directory.
fn could_be_taken(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code != StatusCode::NoSuchFile)
}

fn is_eof(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if status.status_code == StatusCode::Eof)
}

fn file_kind(mode: u32) -> FileKind {
    match mode & S_IFMT {
        S_IFREG => FileKind::File,
        S_IFDIR => FileKind::Dir,
        S_IFLNK => FileKind::Symlink,
        _ => FileKind::Other,
    }
}

fn to_attr(attrs: &FileAttributes) -> Attr {
    let time = |seconds: Option<u32>| {
        seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(u64::from(seconds)))
    };
    Attr {
        // SFTP v3 lets a server leave this out. A file is the kind that does the least harm.
        kind: attrs.permissions.map_or(FileKind::File, file_kind),
        size: attrs.size.unwrap_or(0),
        perm: attrs.permissions.map(|mode| mode & PERMISSION_BITS),
        mtime: time(attrs.mtime),
        atime: time(attrs.atime),
    }
}

/// SFTP v3 times are whole seconds in 32 bits, so anything further out is held at the edge.
fn to_seconds(time: SystemTime) -> u32 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u32::try_from(since.as_secs()).unwrap_or(u32::MAX)
    })
}

fn open_flags(mode: OpenMode) -> OpenFlags {
    let access = match mode.access() {
        Access::Read => OpenFlags::READ,
        Access::Write => OpenFlags::WRITE,
        Access::ReadWrite => OpenFlags::READ | OpenFlags::WRITE,
    };
    let disposition = match mode.disposition() {
        Disposition::OpenExisting => OpenFlags::empty(),
        Disposition::CreateNew => OpenFlags::CREATE | OpenFlags::EXCLUDE,
        Disposition::OpenOrCreate => OpenFlags::CREATE,
        Disposition::TruncateExisting => OpenFlags::TRUNCATE,
        Disposition::CreateOrTruncate => OpenFlags::CREATE | OpenFlags::TRUNCATE,
    };
    access | disposition
}

/// A garbled handle cannot be used or even closed (see the module's docs).
fn usable_handle(handle: String) -> FsResult<String> {
    if handle.contains(char::REPLACEMENT_CHARACTER) {
        warn!("the server sent a file handle that cannot be represented");
        return Err(FsError::Other(
            "the server's file handle cannot be used".to_owned(),
        ));
    }
    Ok(handle)
}

fn sftp_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

enum Target {
    Path(String),
    File(Arc<OpenFile>),
}

impl SftpFs {
    /// A file system over `session`, whose `/` is the session's [`root`](SftpSession::root).
    #[must_use]
    pub fn new(session: SftpSession) -> Self {
        let limits = session.limits();
        let capabilities = Capabilities {
            posix_rename: session.has_extension(POSIX_RENAME),
            fsync: session.has_extension(FSYNC),
            statvfs: session.has_extension(STATVFS),
            max_read: chunk_size(limits.read_len, limits.packet_len),
            max_write: chunk_size(limits.write_len, limits.packet_len),
        };
        Self {
            session: Arc::new(session),
            files: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_aside: AtomicU64::new(1),
            file_slots: Arc::new(Semaphore::new(MAX_OPEN_FILES)),
            listing_slots: Semaphore::new(MAX_OPEN_LISTINGS),
            capabilities,
        }
    }

    pub(crate) fn session(&self) -> &Arc<SftpSession> {
        &self.session
    }

    /// The path on the server.
    fn full(&self, path: &RemotePath) -> String {
        let root = self.session.root();
        if root.is_root() {
            path.as_str().to_owned()
        } else if path.is_root() {
            root.as_str().to_owned()
        } else {
            format!("{}{}", root.as_str(), path.as_str())
        }
    }

    fn file(&self, handle: &RemoteHandle) -> FsResult<Arc<OpenFile>> {
        let files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        files
            .get(&handle.id())
            .cloned()
            .ok_or_else(|| FsError::Other("unknown file handle".to_owned()))
    }

    async fn lstat_full(&self, path: &str) -> FsResult<Attr> {
        let answer = self.session.sftp().lstat(path).await;
        answer.map(|a| to_attr(&a.attrs)).map_err(map_error)
    }

    async fn stat_full(&self, path: &str) -> FsResult<Attr> {
        let answer = self.session.sftp().stat(path).await;
        answer.map(|a| to_attr(&a.attrs)).map_err(map_error)
    }

    async fn exists(&self, path: &str) -> bool {
        self.lstat_full(path).await.is_ok()
    }

    async fn read_chunk(&self, file: &OpenFile, chunk: Chunk) -> FsResult<Vec<u8>> {
        let sftp = self.session.sftp();
        match sftp
            .read(file.handle.as_str(), chunk.offset, chunk.len)
            .await
        {
            Ok(mut answer) => {
                answer.data.truncate(chunk.len as usize);
                Ok(answer.data)
            }
            Err(error) if is_eof(&error) => Ok(Vec::new()),
            Err(error) => Err(map_error(error)),
        }
    }

    async fn write_chunk(&self, file: &OpenFile, offset: u64, data: Vec<u8>) -> FsResult<()> {
        let sftp = self.session.sftp();
        match sftp.write(file.handle.as_str(), offset, data).await {
            Ok(_) => Ok(()),
            Err(error) if is_failure(&error) && self.disk_is_full().await => Err(FsError::NoSpace),
            Err(error) => Err(map_error(error)),
        }
    }

    /// Spec §5.8. Without `statvfs` a full disk is an ordinary failure, unless the text says so.
    async fn disk_is_full(&self) -> bool {
        self.capabilities.statvfs && self.statfs().await.is_ok_and(|s| s.free_bytes == 0)
    }

    async fn plain_rename(&self, from: &str, to: &str) -> FsResult<()> {
        let answer = self.session.sftp().rename(from, to).await;
        answer.map(|_| ()).map_err(map_error)
    }

    async fn posix_rename(&self, from: &str, to: &str) -> FsResult<()> {
        let mut data = Vec::with_capacity(from.len() + to.len() + 8);
        sftp_string(&mut data, from);
        sftp_string(&mut data, to);
        let answer = self.session.sftp().extended(POSIX_RENAME, data).await;
        match answer.map_err(map_error)? {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            Packet::Status(status) => Err(map_status(&status)),
            _ => Err(FsError::ConnectionLost),
        }
    }

    /// A rename that replaces, on a server without `posix-rename`. It cannot be atomic, but it must
    /// still leave both paths alone if it fails. So the checks come first, and the target only moves
    /// aside until the source has taken its place.
    async fn replace_by_hand(&self, from: &str, to: &str) -> FsResult<()> {
        let source = self.lstat_full(from).await?;
        let target = match self.lstat_full(to).await {
            Ok(target) => target,
            Err(FsError::NotFound) => return self.plain_rename(from, to).await,
            Err(error) => return Err(error),
        };
        if source.is_dir() != target.is_dir() {
            return Err(FsError::Exists);
        }
        if target.is_dir() && !self.read_dir_full(to).await?.is_empty() {
            return Err(FsError::NotEmpty);
        }

        debug!(
            from,
            to, "non-atomic rename: the server has no posix-rename"
        );
        let number = self.next_aside.fetch_add(1, Ordering::Relaxed);
        let aside = format!("{to}.drivemount-replaced-{}-{number}", std::process::id());
        self.plain_rename(to, &aside).await?;
        if let Err(error) = self.plain_rename(from, to).await {
            if let Err(put_back) = self.plain_rename(&aside, to).await {
                warn!(to, aside, %put_back, "could not put a rename's target back");
            }
            return Err(error);
        }
        let sftp = self.session.sftp();
        let removed = if target.is_dir() {
            sftp.rmdir(aside.as_str()).await
        } else {
            sftp.remove(aside.as_str()).await
        };
        // The rename has happened, so this does not fail it.
        if let Err(error) = removed {
            warn!(aside, %error, "could not remove a replaced file");
        }
        Ok(())
    }

    async fn read_dir_full(&self, path: &str) -> FsResult<Vec<DirEntry>> {
        let _slot = self
            .listing_slots
            .acquire()
            .await
            .map_err(|_| FsError::Other("listing limit closed".to_owned()))?;
        let sftp = self.session.sftp();
        let handle = sftp.opendir(path).await.map_err(map_error)?;
        let handle = usable_handle(handle.handle)?;

        let mut entries = Vec::new();
        let result = loop {
            match sftp.readdir(handle.as_str()).await {
                Ok(name) => {
                    let names = name.files.into_iter();
                    let names = names.filter(|file| file.filename != "." && file.filename != "..");
                    entries.extend(names.map(|file| DirEntry {
                        attr: to_attr(&file.attrs),
                        name: file.filename,
                    }));
                }
                Err(error) if is_eof(&error) => break Ok(()),
                Err(error) => break Err(map_error(error)),
            }
        };
        // Nothing depends on this once the entries are here.
        if let Err(error) = sftp.close(handle).await {
            debug!(path, %error, "closing a listing failed");
        }
        result.map(|()| entries)
    }

    fn target(&self, target: AttrTarget<'_>) -> FsResult<Target> {
        Ok(match target {
            AttrTarget::Path(path) => Target::Path(self.full(path)),
            AttrTarget::Handle(handle) => Target::File(self.file(handle)?),
        })
    }

    async fn stat_target(&self, target: &Target) -> FsResult<Attr> {
        match target {
            Target::Path(path) => self.stat_full(path).await,
            Target::File(file) => {
                let answer = self.session.sftp().fstat(file.handle.as_str()).await;
                answer.map(|a| to_attr(&a.attrs)).map_err(map_error)
            }
        }
    }
}

/// What to send for `changes`, given the attributes as they are if that was needed. SFTP v3 can only
/// set both times together, and the read-only flag changes one bit of what is there now.
fn attributes_to_send(changes: AttrChanges, current: Option<&Attr>) -> FsResult<FileAttributes> {
    let mut attrs = FileAttributes {
        size: changes.size,
        ..FileAttributes::default()
    };
    if let Some(read_only) = changes.read_only {
        let perm = current.and_then(|current| current.perm);
        let perm = perm.ok_or(FsError::NotSupported)?;
        attrs.permissions = Some(apply_read_only(perm, read_only));
    }
    if changes.mtime.is_some() || changes.atime.is_some() {
        let other = changes.mtime.or(changes.atime);
        let mtime = changes.mtime.or(current.and_then(|c| c.mtime)).or(other);
        let atime = changes.atime.or(current.and_then(|c| c.atime)).or(other);
        attrs.mtime = mtime.map(to_seconds);
        attrs.atime = atime.map(to_seconds);
    }
    Ok(attrs)
}

#[async_trait]
impl RemoteFs for SftpFs {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.stat_full(&self.full(path)).await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.lstat_full(&self.full(path)).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.stat_target(&Target::File(self.file(handle)?)).await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.read_dir_full(&self.full(path)).await
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let slot = Arc::clone(&self.file_slots)
            .try_acquire_owned()
            .map_err(|_| FsError::TooManyOpenFiles)?;
        let full = self.full(path);
        let opened = self
            .session
            .sftp()
            .open(full.as_str(), open_flags(mode), FileAttributes::default())
            .await;
        let handle = match opened {
            Ok(answer) => usable_handle(answer.handle)?,
            Err(error)
                if could_be_taken(&error)
                    && mode.disposition() == Disposition::CreateNew
                    && self.exists(&full).await =>
            {
                return Err(FsError::Exists);
            }
            Err(error) => return Err(map_error(error)),
        };

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let file = Arc::new(OpenFile {
            handle,
            access: mode.access(),
            in_flight: InFlight::new(),
            _slot: slot,
        });
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        files.insert(id, file);
        Ok(RemoteHandle::new(id))
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        let file = self.file(handle)?;
        if file.access == Access::Write {
            return Err(FsError::PermissionDenied);
        }
        let chunk_size = self.capabilities.max_read;
        let chunks = split(offset, len as usize, chunk_size);
        let file = file.as_ref();
        let results = pipelined(
            &file.in_flight,
            chunks,
            |chunk| self.read_chunk(file, chunk),
            |chunk, data: &Vec<u8>| data.len() == chunk.len as usize,
        )
        .await;
        assemble(results, chunk_size)
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let file = self.file(handle)?;
        if file.access == Access::Read {
            return Err(FsError::PermissionDenied);
        }
        let chunks = split(offset, data.len(), self.capabilities.max_write);
        let file = file.as_ref();
        let data = &data;
        let results = pipelined(
            &file.in_flight,
            chunks,
            |chunk| {
                let part = data.slice(chunk.start..chunk.start + chunk.len as usize);
                self.write_chunk(file, chunk.offset, Vec::from(part))
            },
            |_, ()| true,
        )
        .await;
        results.into_iter().collect()
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        let file = self.file(handle)?;
        // A handle opened for reading has nothing to flush, and not every server need allow it.
        if !self.capabilities.fsync || file.access == Access::Read {
            return Ok(());
        }
        let answer = self.session.sftp().fsync(file.handle.as_str()).await;
        answer.map(|_| ()).map_err(map_error)
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        let file = {
            let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            files.remove(&handle.id())
        };
        let file = file.ok_or_else(|| FsError::Other("unknown file handle".to_owned()))?;
        let answer = self.session.sftp().close(file.handle.as_str()).await;
        answer.map(|_| ()).map_err(map_error)
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        let target = self.target(target)?;
        if changes == AttrChanges::default() {
            return self.stat_target(&target).await;
        }
        let needs_current =
            changes.read_only.is_some() || changes.mtime.is_some() != changes.atime.is_some();
        let current = if needs_current {
            Some(self.stat_target(&target).await?)
        } else {
            None
        };
        let attrs = attributes_to_send(changes, current.as_ref())?;
        let sftp = self.session.sftp();
        let answer = match &target {
            Target::Path(path) => sftp.setstat(path.as_str(), attrs).await,
            Target::File(file) => sftp.fsetstat(file.handle.as_str(), attrs).await,
        };
        answer.map_err(map_error)?;
        self.stat_target(&target).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let full = self.full(path);
        let sftp = self.session.sftp();
        match sftp.mkdir(full.as_str(), FileAttributes::default()).await {
            Ok(_) => self.stat_full(&full).await,
            Err(error) if could_be_taken(&error) && self.exists(&full).await => {
                Err(FsError::Exists)
            }
            Err(error) => Err(map_error(error)),
        }
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let answer = self.session.sftp().remove(self.full(path)).await;
        answer.map(|_| ()).map_err(map_error)
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let full = self.full(path);
        match self.session.sftp().rmdir(full.as_str()).await {
            Ok(_) => Ok(()),
            Err(error) if is_failure(&error) => match self.read_dir_full(&full).await {
                Ok(entries) if !entries.is_empty() => Err(FsError::NotEmpty),
                _ => Err(map_error(error)),
            },
            Err(error) => Err(map_error(error)),
        }
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        let (from, to) = (self.full(from), self.full(to));
        // Servers differ on this one, and a rename by hand would remove the only copy.
        if from == to {
            self.lstat_full(&from).await?;
            return if replace {
                Ok(())
            } else {
                Err(FsError::Exists)
            };
        }
        if replace {
            return if self.capabilities.posix_rename {
                self.posix_rename(&from, &to).await
            } else {
                self.replace_by_hand(&from, &to).await
            };
        }
        // ProFTPD refuses a taken name with a status outside SFTP v3, and `russh-sftp` drops the whole
        // session over that (see the module's docs). OpenSSH, which has `posix-rename`, does not.
        if !self.capabilities.posix_rename {
            self.lstat_full(&from).await?;
            if self.exists(&to).await {
                return Err(FsError::Exists);
            }
        }
        match self.session.sftp().rename(from.as_str(), to.as_str()).await {
            Ok(_) => Ok(()),
            Err(error) if could_be_taken(&error) && self.exists(&to).await => Err(FsError::Exists),
            Err(error) => Err(map_error(error)),
        }
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        if !self.capabilities.statvfs {
            return Ok(FsStats::PLACEHOLDER);
        }
        let root = self.session.root().as_str();
        let stats = self.session.sftp().statvfs(root).await.map_err(map_error)?;
        // Both sizes are in the spec, and a server that fills in only one is not unheard of.
        let unit = if stats.fragment_size > 0 {
            stats.fragment_size
        } else {
            stats.block_size
        };
        Ok(FsStats {
            total_bytes: stats.blocks.saturating_mul(unit),
            free_bytes: stats.blocks_avail.saturating_mul(unit),
        })
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
}

#[async_trait]
impl Connector for SftpConnector {
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        let fs = SftpFs::new(self.connect_session(prompter).await?);
        self.remember_session(fs.session());
        Ok(Arc::new(fs))
    }

    async fn probe(&self) -> Result<Duration, ConnectError> {
        self.probe_session().await
    }

    async fn session_ended(&self) {
        SftpConnector::session_ended(self).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    const KIB: u32 = 1024;

    #[test]
    fn chunks_cover_the_data_exactly() {
        assert_eq!(split(10, 0, 4), []);
        assert_eq!(
            split(10, 9, 4),
            [
                Chunk {
                    offset: 10,
                    start: 0,
                    len: 4
                },
                Chunk {
                    offset: 14,
                    start: 4,
                    len: 4
                },
                Chunk {
                    offset: 18,
                    start: 8,
                    len: 1
                },
            ]
        );
        assert_eq!(split(0, 8, 4).len(), 2);
    }

    #[test]
    fn chunk_sizes_follow_the_servers_limits() {
        assert_eq!(chunk_size(None, None), 32 * KIB);
        assert_eq!(chunk_size(Some(261_120), Some(262_144)), 261_120);
        assert_eq!(chunk_size(Some(64 * 1024 * 1024), None), MAX_CHUNK);
        // A server that names only a packet size, or one that leaves no room for headers.
        assert_eq!(chunk_size(None, Some(16_384)), 16_384 - 1024);
        assert_eq!(chunk_size(Some(100_000), Some(100_000)), 100_000 - 1024);
        assert_eq!(chunk_size(None, Some(100)), 1);
    }

    fn status(status_code: StatusCode, message: &str) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code,
            error_message: message.to_owned(),
            language_tag: String::new(),
        })
    }

    #[test]
    fn statuses_map_to_errors() {
        let cases = [
            (StatusCode::NoSuchFile, FsError::NotFound),
            (StatusCode::PermissionDenied, FsError::PermissionDenied),
            (StatusCode::OpUnsupported, FsError::NotSupported),
            (StatusCode::NoConnection, FsError::ConnectionLost),
            (StatusCode::ConnectionLost, FsError::ConnectionLost),
        ];
        for (code, expected) in cases {
            assert_eq!(map_error(status(code, "")), expected);
        }
        assert_eq!(
            map_error(status(StatusCode::Failure, "Failure")),
            FsError::Other("SFTP: Failure: Failure".to_owned())
        );
        assert_eq!(
            map_error(status(StatusCode::Failure, "No space left on device")),
            FsError::NoSpace
        );
        assert_eq!(
            map_error(status(StatusCode::Failure, "Disk quota exceeded")),
            FsError::NoSpace
        );
        assert_eq!(
            map_error(status(StatusCode::Failure, "Not a directory")),
            FsError::PathNotFound
        );
    }

    #[test]
    fn what_is_neither_a_status_nor_a_timeout_is_a_lost_connection() {
        assert_eq!(map_error(SftpError::Timeout), FsError::Timeout);
        for error in [
            SftpError::UnexpectedBehavior("sender dropped".to_owned()),
            SftpError::UnexpectedBehavior("session closed".to_owned()),
            SftpError::UnexpectedPacket,
            SftpError::IO("reset".to_owned()),
        ] {
            assert_eq!(map_error(error), FsError::ConnectionLost);
        }
    }

    #[test]
    fn a_limit_on_this_side_does_not_end_the_connection() {
        let error = map_error(SftpError::Limited("handle limit reached".to_owned()));
        assert_eq!(
            error,
            FsError::Other("SFTP: handle limit reached".to_owned())
        );
    }

    #[test]
    fn modes_give_kinds() {
        assert_eq!(file_kind(0o100_644), FileKind::File);
        assert_eq!(file_kind(0o040_755), FileKind::Dir);
        assert_eq!(file_kind(0o120_777), FileKind::Symlink);
        assert_eq!(file_kind(0o140_755), FileKind::Other);
        assert_eq!(file_kind(0o020_620), FileKind::Other);
    }

    #[test]
    fn attributes_convert_and_may_be_missing() {
        let attr = to_attr(&FileAttributes {
            size: Some(7),
            permissions: Some(0o104_755),
            mtime: Some(5),
            atime: Some(6),
            ..FileAttributes::default()
        });
        assert_eq!(attr.kind, FileKind::File);
        assert_eq!(attr.size, 7);
        assert_eq!(attr.perm, Some(0o4755));
        assert_eq!(attr.mtime, Some(UNIX_EPOCH + Duration::from_secs(5)));
        assert_eq!(attr.atime, Some(UNIX_EPOCH + Duration::from_secs(6)));

        let bare = to_attr(&FileAttributes::default());
        assert_eq!(bare.kind, FileKind::File);
        assert_eq!((bare.size, bare.perm, bare.mtime), (0, None, None));
    }

    #[test]
    fn times_are_held_within_32_bits() {
        assert_eq!(to_seconds(UNIX_EPOCH + Duration::from_secs(9)), 9);
        assert_eq!(to_seconds(UNIX_EPOCH - Duration::from_secs(9)), 0);
        let far = UNIX_EPOCH + Duration::from_secs(1 << 33);
        assert_eq!(to_seconds(far), u32::MAX);
    }

    #[test]
    fn open_modes_become_flags() {
        let flags = |mode| open_flags(mode).bits();
        let (read, write) = (OpenFlags::READ.bits(), OpenFlags::WRITE.bits());
        let (create, trunc) = (OpenFlags::CREATE.bits(), OpenFlags::TRUNCATE.bits());
        let exclude = OpenFlags::EXCLUDE.bits();

        assert_eq!(flags(OpenMode::read()), read);
        assert_eq!(flags(OpenMode::write(Disposition::OpenExisting)), write);
        assert_eq!(
            flags(OpenMode::read_write(Disposition::OpenExisting)),
            read | write
        );
        assert_eq!(
            flags(OpenMode::write(Disposition::CreateNew)),
            write | create | exclude
        );
        assert_eq!(
            flags(OpenMode::write(Disposition::OpenOrCreate)),
            write | create
        );
        assert_eq!(
            flags(OpenMode::write(Disposition::TruncateExisting)),
            write | trunc
        );
        assert_eq!(
            flags(OpenMode::write(Disposition::CreateOrTruncate)),
            write | create | trunc
        );
    }

    #[test]
    fn a_garbled_handle_is_refused() {
        assert_eq!(
            usable_handle("\0\0\0\x7f".to_owned()),
            Ok("\0\0\0\x7f".to_owned())
        );
        assert!(usable_handle(String::from_utf8_lossy(&[0, 0, 0, 0x80]).into_owned()).is_err());
    }

    #[test]
    fn strings_are_length_prefixed() {
        let mut out = Vec::new();
        sftp_string(&mut out, "ab");
        assert_eq!(out, [0, 0, 0, 2, b'a', b'b']);
    }

    fn seconds(value: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(value)
    }

    #[test]
    fn one_time_alone_keeps_the_other() {
        let current = Attr {
            kind: FileKind::File,
            size: 0,
            perm: Some(0o644),
            mtime: Some(seconds(10)),
            atime: Some(seconds(20)),
        };
        let attrs = attributes_to_send(
            AttrChanges {
                mtime: Some(seconds(30)),
                ..AttrChanges::default()
            },
            Some(&current),
        )
        .unwrap();
        assert_eq!((attrs.mtime, attrs.atime), (Some(30), Some(20)));
        assert_eq!(attrs.permissions, None);
        assert_eq!(attrs.size, None);

        // A server that does not report the other time gets the same one twice.
        let unknown = Attr {
            mtime: None,
            atime: None,
            ..current
        };
        let attrs = attributes_to_send(
            AttrChanges {
                atime: Some(seconds(40)),
                ..AttrChanges::default()
            },
            Some(&unknown),
        )
        .unwrap();
        assert_eq!((attrs.mtime, attrs.atime), (Some(40), Some(40)));
    }

    #[test]
    fn the_read_only_flag_needs_known_permissions() {
        let current = Attr {
            kind: FileKind::File,
            size: 0,
            perm: None,
            mtime: None,
            atime: None,
        };
        let changes = AttrChanges {
            read_only: Some(true),
            ..AttrChanges::default()
        };
        assert_eq!(
            attributes_to_send(changes, Some(&current)).unwrap_err(),
            FsError::NotSupported
        );
        let known = Attr {
            perm: Some(0o640),
            ..current
        };
        let attrs = attributes_to_send(changes, Some(&known)).unwrap();
        assert_eq!(attrs.permissions, Some(0o440));
    }

    /// Reads `count` chunks of `len` bytes through the limits, returning the most in flight at once.
    async fn most_in_flight(count: usize, len: u32) -> usize {
        let limits = InFlight::new();
        let now = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let results = pipelined(
            &limits,
            split(0, count * len as usize, len),
            |_| async {
                let running = now.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(running, Ordering::SeqCst);
                tokio::task::yield_now().await;
                now.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            },
            |_, ()| true,
        )
        .await;
        assert_eq!(results.len(), count);
        assert!(results.iter().all(Result::is_ok));
        most.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn small_chunks_are_limited_by_their_number() {
        assert_eq!(most_in_flight(200, KIB).await, MAX_CHUNKS_IN_FLIGHT);
    }

    #[tokio::test]
    async fn large_chunks_are_limited_by_their_bytes() {
        assert_eq!(most_in_flight(40, 256 * KIB).await, 16);
        assert_eq!(most_in_flight(3, MAX_CHUNK).await, 1);
    }

    #[tokio::test]
    async fn chunks_are_all_sent_at_once_when_they_fit() {
        assert_eq!(most_in_flight(16, 256 * KIB).await, 16);
    }

    #[tokio::test]
    async fn after_a_failure_nothing_more_is_sent_and_what_was_sent_is_waited_for() {
        let limits = InFlight::new();
        let sent = AtomicUsize::new(0);
        let finished = AtomicUsize::new(0);
        // 40 × 256 KiB: 16 go out at once, the 3rd fails, and the others wait in line.
        let results = pipelined(
            &limits,
            split(0, 40 * 256 * KIB as usize, 256 * KIB),
            |chunk| {
                sent.fetch_add(1, Ordering::SeqCst);
                let (finished, fails) = (&finished, chunk.start == 2 * 256 * KIB as usize);
                async move {
                    tokio::task::yield_now().await;
                    finished.fetch_add(1, Ordering::SeqCst);
                    if fails { Err(FsError::NoSpace) } else { Ok(()) }
                }
            },
            |_, ()| true,
        )
        .await;
        let sent = sent.load(Ordering::SeqCst);
        assert!((16..40).contains(&sent), "{sent}");
        assert_eq!(finished.load(Ordering::SeqCst), sent);
        let result: FsResult<()> = results.into_iter().collect();
        assert_eq!(result, Err(FsError::NoSpace));
    }

    #[test]
    fn a_read_ends_at_the_first_short_chunk() {
        let full = || Ok(vec![1; 4]);
        let assembled = assemble(vec![full(), Ok(vec![2; 3]), full()], 4).unwrap();
        assert_eq!(assembled.as_ref(), [1, 1, 1, 1, 2, 2, 2]);

        assert_eq!(assemble(vec![full(), full()], 4).unwrap().len(), 8);
        assert_eq!(assemble(vec![Ok(vec![]), full()], 4).unwrap().len(), 0);
        assert_eq!(assemble(vec![], 4).unwrap().len(), 0);
    }

    #[test]
    fn a_failure_after_data_only_makes_a_read_short() {
        let full = || Ok(vec![1; 4]);
        let failed = || Err(FsError::ConnectionLost);
        assert_eq!(assemble(vec![full(), failed()], 4).unwrap().len(), 4);
        assert_eq!(
            assemble(vec![failed(), full()], 4).unwrap_err(),
            FsError::ConnectionLost
        );
    }
}
