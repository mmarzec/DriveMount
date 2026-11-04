// SPDX-License-Identifier: GPL-3.0-or-later

//! The remote file system and how sessions are created. The contract tests of M1-03 hold backends to
//! what these doc comments promise.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, ConnectError, DirEntry, FsResult, FsStats,
    OpenMode, Prompter, RemoteHandle, RemotePath,
};

/// Async file system operations on one remote session. Implemented by `fsm-sftp` and `fsm-nfs`, and
/// wrapped by the caching and deadline layers.
///
/// Calls can overlap, including several on one handle. Names and paths are the server's own, before
/// name mapping.
#[async_trait]
pub trait RemoteFs: Send + Sync + 'static {
    /// The attributes of `path`, following symbolic links. A broken link gives `NotFound`.
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr>;

    /// The attributes of `path` itself, without following a symbolic link.
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr>;

    /// The attributes of an open file.
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr>;

    /// All entries of a directory, in no particular order and without `.` and `..`. Symbolic links
    /// are not followed.
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>>;

    /// Opens a file. New files get the server's default permissions (spec §5.6).
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle>;

    /// Reads up to `len` bytes at `offset`. Fewer bytes mean only that the rest must be asked for
    /// again; an empty result means end of file.
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes>;

    /// Writes all of `data` at `offset`, or fails. Returns once the server has acknowledged it.
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()>;

    /// Asks the server to move the file's data to stable storage, where it can
    /// ([`Capabilities::fsync`]).
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()>;

    /// Closes a file. The handle is gone even if this fails.
    async fn close(&self, handle: RemoteHandle) -> FsResult<()>;

    /// Changes attributes and returns the result.
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr>;

    /// Creates a directory with the server's default permissions. `Exists` if the name is taken.
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr>;

    /// Removes a file or symbolic link.
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()>;

    /// Removes an empty directory. `NotEmpty` otherwise.
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()>;

    /// Renames a file or directory. If the target exists, `replace` decides between replacing it
    /// and failing with `Exists`.
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()>;

    /// The space on the file system.
    async fn statfs(&self) -> FsResult<FsStats>;

    /// What this session can do. Fixed for the life of the session.
    fn capabilities(&self) -> Capabilities;
}

/// Creates a [`RemoteFs`] session for one profile, and re-creates it on reconnect.
#[async_trait]
pub trait Connector: Send + Sync + 'static {
    /// Connects, authenticates and opens a session, asking the user through `prompter` if needed.
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError>;

    /// Sends a keep-alive on the current connection and returns its round-trip time.
    async fn probe(&self) -> Result<Duration, ConnectError>;
}
