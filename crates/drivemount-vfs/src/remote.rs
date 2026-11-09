// SPDX-License-Identifier: GPL-3.0-or-later

//! The remote file system and how sessions are created. The contract tests in `contract` (feature
//! `testing`) hold backends to what these doc comments promise.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, ConnectError, DirEntry, FsResult, FsStats,
    OpenMode, Prompter, RemoteHandle, RemotePath,
};

/// Async file system operations on one remote session. Implemented by `drivemount-sftp` and `drivemount-nfs`, and
/// wrapped by the caching and deadline layers.
///
/// Calls can overlap, including several on one handle. Names and paths are the server's own, before
/// name mapping.
///
/// A path whose parent is missing, or runs through a file, gives `PathNotFound` where the backend can
/// tell and `NotFound` where it cannot: SFTP has only one "no such file" status.
#[async_trait]
pub trait RemoteFs: Send + Sync + 'static {
    /// The attributes of `path`, following symbolic links. A broken link gives `NotFound`.
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr>;

    /// The attributes of `path` itself, without following a symbolic link.
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr>;

    /// The attributes of an open file.
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr>;

    /// All entries of a directory, in no particular order and without `.` and `..`. Symbolic links
    /// are not followed. Fails if `path` is not a directory.
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>>;

    /// Opens a file. New files get the server's default permissions (spec §5.6). Asking to write a
    /// read-only file gives `PermissionDenied` and leaves it unchanged.
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle>;

    /// Reads up to `len` bytes at `offset`. Fewer bytes mean only that the rest must be asked for
    /// again; an empty result means end of file. Fails on a write-only handle.
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes>;

    /// Writes all of `data` at `offset`, or fails. Returns once the server has acknowledged it. A gap
    /// left by writing past the end of the file reads as zeros. Fails on a read-only handle.
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()>;

    /// Asks the server to move the file's data to stable storage, where it can
    /// ([`Capabilities::fsync`]).
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()>;

    /// Closes a file. The handle is gone even if this fails.
    async fn close(&self, handle: RemoteHandle) -> FsResult<()>;

    /// Changes attributes and returns the result. Extending a file adds zeros, and data cut off by
    /// shrinking it does not come back.
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr>;

    /// Creates a directory with the server's default permissions. `Exists` if the name is taken.
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr>;

    /// Removes a file or symbolic link. Fails on a directory, leaving it in place.
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()>;

    /// Removes an empty directory. `NotEmpty` if it has entries; fails on a file, leaving it in place.
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()>;

    /// Renames a file or directory. If the target exists, `replace` decides between replacing it
    /// and failing with `Exists`.
    ///
    /// A failed rename leaves both paths as they were, whether the source is missing or the target
    /// cannot be replaced: a non-empty directory cannot be, and a file and a directory cannot replace
    /// each other. A directory cannot move into itself. Renaming a path onto itself with `replace`
    /// does nothing and keeps the file.
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()>;

    /// The space on the file system.
    async fn statfs(&self) -> FsResult<FsStats>;

    /// What this session can do. Fixed for the life of the session.
    fn capabilities(&self) -> Capabilities;
}

/// A shared file system is a file system, so the wrappers can sit on top of `Arc<dyn RemoteFs>`, which
/// is what [`Connector::connect`] hands out, and a layer can be shared with whoever needs to reach it
/// directly.
#[async_trait]
impl<F: RemoteFs + ?Sized> RemoteFs for Arc<F> {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        (**self).stat(path).await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        (**self).lstat(path).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        (**self).fstat(handle).await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        (**self).read_dir(path).await
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        (**self).open(path, mode).await
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        (**self).read(handle, offset, len).await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        (**self).write(handle, offset, data).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        (**self).flush(handle).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        (**self).close(handle).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        (**self).set_attr(target, changes).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        (**self).mkdir(path).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        (**self).remove_file(path).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        (**self).remove_dir(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        (**self).rename(from, to, replace).await
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        (**self).statfs().await
    }

    fn capabilities(&self) -> Capabilities {
        (**self).capabilities()
    }
}

/// Creates a [`RemoteFs`] session for one profile, and re-creates it on reconnect.
#[async_trait]
pub trait Connector: Send + Sync + 'static {
    /// Connects, authenticates and opens a session, asking the user through `prompter` if needed.
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError>;

    /// Sends a keep-alive on the current connection and returns its round-trip time.
    async fn probe(&self) -> Result<Duration, ConnectError>;

    /// Resolves once the session handed out by the last [`connect`](Self::connect) has ended by
    /// itself: the server closed it, keep-alives went unanswered (FR-SSH-12) or the transport failed.
    /// At once if there is no such session. The default never resolves, for a backend that cannot
    /// tell: probes and failed calls still find out.
    async fn session_ended(&self) {
        std::future::pending::<()>().await;
    }
}
