// SPDX-License-Identifier: GPL-3.0-or-later

//! The operation deadlines of spec §5.6, as a [`RemoteFs`] wrapper.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, FsError, FsResult, FsStats, OpenMode,
    RemoteFs, RemoteHandle, RemotePath,
};

/// A [`RemoteFs`] whose calls give up after `op_timeout` with [`FsError::Timeout`].
///
/// The deadline is per call. When it passes the inner call is dropped, which says nothing about
/// whether the server acted, so layers above must treat the call as possibly done; `CachedFs` does.
/// An `open` dropped this way can leave a handle open on the server until the session ends, because
/// nobody learns its identifier.
///
/// Every [`FsError::Timeout`], whether from the deadline or from the backend itself, marks the
/// session as suspect; see [`suspect`](Self::suspect).
pub struct DeadlineFs<F> {
    inner: F,
    op_timeout: Duration,
    suspect: watch::Sender<u64>,
}

impl<F: RemoteFs> DeadlineFs<F> {
    /// Wraps `inner`, limiting each of its calls to `op_timeout`.
    #[must_use]
    pub fn new(inner: F, op_timeout: Duration) -> Self {
        Self {
            inner,
            op_timeout,
            suspect: watch::Sender::new(0),
        }
    }

    /// The "suspect" signal: how many calls have timed out so far.
    ///
    /// Each rise says the connection may be dead and is worth a keep-alive probe. A count rather than
    /// a flag, so nobody has to clear it and a timeout after a probe still wakes the watcher. Timeouts
    /// close together may wake it only once.
    #[must_use]
    pub fn suspect(&self) -> watch::Receiver<u64> {
        self.suspect.subscribe()
    }

    async fn limit<T>(&self, call: impl Future<Output = FsResult<T>> + Send) -> FsResult<T> {
        let result = timeout(self.op_timeout, call)
            .await
            .unwrap_or(Err(FsError::Timeout));
        if matches!(result, Err(FsError::Timeout)) {
            // Unlike `send`, this also counts when nobody is watching yet.
            self.suspect.send_modify(|count| *count += 1);
        }
        result
    }
}

#[async_trait]
impl<F: RemoteFs> RemoteFs for DeadlineFs<F> {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.limit(self.inner.stat(path)).await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.limit(self.inner.lstat(path)).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.limit(self.inner.fstat(handle)).await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.limit(self.inner.read_dir(path)).await
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.limit(self.inner.open(path, mode)).await
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.limit(self.inner.read(handle, offset, len)).await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.limit(self.inner.write(handle, offset, data)).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.limit(self.inner.flush(handle)).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.limit(self.inner.close(handle)).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.limit(self.inner.set_attr(target, changes)).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.limit(self.inner.mkdir(path)).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.limit(self.inner.remove_file(path)).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.limit(self.inner.remove_dir(path)).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.limit(self.inner.rename(from, to, replace)).await
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        self.limit(self.inner.statfs()).await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}
