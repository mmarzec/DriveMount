// SPDX-License-Identifier: GPL-3.0-or-later

//! A drive's file system across reconnects (FR-REC-04, FR-REC-05): [`SessionFs`] holds the session in
//! use, and swaps in the next one without the layers above noticing.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::watch;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FsError, FsResult, FsStats,
    OpenMode, RemoteFs, RemoteHandle, RemotePath,
};

/// The session a call goes to, with the number that tells it from the ones before.
#[derive(Clone)]
struct Session {
    generation: u64,
    fs: Arc<dyn RemoteFs>,
}

/// Where the connection is.
#[derive(Clone)]
enum State {
    /// Calls go to this session.
    Up(Session),
    /// The session of this generation is gone, for this reason. Calls wait for the next.
    Down { generation: u64, reason: String },
    /// No session will come: calls fail at once.
    Closed,
}

/// One open file, whichever session it is open on.
struct OpenFile {
    /// Where it is now, following renames through this file system.
    path: Mutex<RemotePath>,
    mode: OpenMode,
    /// The session it is open on, and its handle there.
    remote: Mutex<(u64, u64)>,
    /// Lets one call at a time open it again on a new session.
    reopening: tokio::sync::Mutex<()>,
}

impl OpenFile {
    fn remote(&self) -> (u64, u64) {
        *lock(&self.remote)
    }
}

/// A [`RemoteFs`] over whichever session is current, so that a drive outlives its connection.
///
/// - **While the connection is down**, calls wait for the next session. The deadline layer above
///   decides how long ([`DeadlineFs`](crate::DeadlineFs)), so no call waits longer than the
///   operation timeout (FR-REC-04). Once [shut down](Self::shut_down), they fail at once.
/// - **A call that finds the connection gone** ([`FsError::ConnectionLost`]) marks the session
///   [lost](Self::mark_lost), as a transport error is one of FR-REC-01's triggers. A call that is safe
///   to repeat then waits and goes again on the next session: reads, writes at an offset, lookups,
///   attribute changes and opens that create nothing new. Creating, removing and renaming are not
///   repeated, as the server may have acted, and give `ConnectionLost`.
/// - **Handles are this layer's own.** Each knows the session it was opened on, and is opened again
///   by path on the first use after a reconnect (FR-REC-05). That reopening never creates or
///   truncates: the file has to be there, and keeps what it holds. Paths follow renames made through
///   this layer. A handle from a session that has gone is closed by going with it.
///
/// [`capabilities`](RemoteFs::capabilities) are those of the current session, or the last one.
pub struct SessionFs {
    state: watch::Sender<State>,
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_id: AtomicU64,
    capabilities: Mutex<Capabilities>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here panics while holding a lock in a way that leaves the data half changed.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionFs {
    /// Starts with `fs` as the first session, generation 1.
    #[must_use]
    pub fn new(fs: Arc<dyn RemoteFs>) -> Self {
        let capabilities = fs.capabilities();
        Self {
            state: watch::Sender::new(State::Up(Session { generation: 1, fs })),
            files: Mutex::default(),
            next_id: AtomicU64::new(1),
            capabilities: Mutex::new(capabilities),
        }
    }

    /// Makes `fs` the session calls go to, whatever the state, and wakes the calls waiting for one.
    /// Returns its generation. Does nothing once [shut down](Self::shut_down), and returns `None`.
    pub fn replace(&self, fs: Arc<dyn RemoteFs>) -> Option<u64> {
        let capabilities = fs.capabilities();
        let mut generation = None;
        self.state.send_if_modified(|state| {
            let last = match state {
                State::Up(session) => session.generation,
                State::Down { generation, .. } => *generation,
                State::Closed => return false,
            };
            generation = Some(last + 1);
            *state = State::Up(Session {
                generation: last + 1,
                fs,
            });
            true
        });
        if generation.is_some() {
            *lock(&self.capabilities) = capabilities;
        }
        generation
    }

    /// Says the session of `generation` is gone, if it is still the current one. Calls wait for the
    /// next from now on. Returns whether this changed anything.
    pub fn mark_lost(&self, generation: u64, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        self.state.send_if_modified(|state| match state {
            State::Up(session) if session.generation == generation => {
                *state = State::Down { generation, reason };
                true
            }
            _ => false,
        })
    }

    /// No session will come any more: calls waiting for one, and all later calls, fail with
    /// [`FsError::ConnectionLost`]. The session in use goes, and with it its handles.
    pub fn shut_down(&self) {
        self.state.send_replace(State::Closed);
    }

    /// The generation of the session calls go to now, or `None` while there is none.
    #[must_use]
    pub fn current(&self) -> Option<u64> {
        match &*self.state.borrow() {
            State::Up(session) => Some(session.generation),
            State::Down { .. } | State::Closed => None,
        }
    }

    /// Waits for a session to be current, and gives its generation. `None` once shut down.
    pub async fn up(&self) -> Option<u64> {
        let mut state = self.state.subscribe();
        let ready = state
            .wait_for(|state| !matches!(state, State::Down { .. }))
            .await
            .ok()?;
        match &*ready {
            State::Up(session) => Some(session.generation),
            State::Down { .. } | State::Closed => None,
        }
    }

    /// Waits until the session of `generation` is no longer the current one, and gives the reason it
    /// was [lost](Self::mark_lost). `None` if it went for another reason: replaced or shut down.
    pub async fn lost(&self, generation: u64) -> Option<String> {
        let mut state = self.state.subscribe();
        let gone = state
            .wait_for(|state| !matches!(state, State::Up(s) if s.generation == generation))
            .await
            .ok()?;
        match &*gone {
            State::Down {
                generation: lost,
                reason,
            } if *lost == generation => Some(reason.clone()),
            _ => None,
        }
    }

    /// The current session, waiting for one while the connection is down.
    async fn session(&self) -> FsResult<Session> {
        let mut state = self.state.subscribe();
        let ready = state
            .wait_for(|state| !matches!(state, State::Down { .. }))
            .await
            .map_err(|_| FsError::ConnectionLost)?;
        match &*ready {
            State::Up(session) => Ok(session.clone()),
            State::Down { .. } | State::Closed => Err(FsError::ConnectionLost),
        }
    }

    /// Runs `op` on the current session. A `ConnectionLost` marks that session lost, and is tried
    /// again on the next one if `repeatable`.
    async fn call<T, F, Fut>(&self, repeatable: bool, mut op: F) -> FsResult<T>
    where
        F: FnMut(Session) -> Fut + Send,
        Fut: Future<Output = FsResult<T>> + Send,
    {
        loop {
            let session = self.session().await?;
            let generation = session.generation;
            match op(session).await {
                Err(FsError::ConnectionLost) => {
                    self.mark_lost(generation, "a call found the connection gone");
                    if !repeatable {
                        return Err(FsError::ConnectionLost);
                    }
                }
                result => return result,
            }
        }
    }

    fn file(&self, handle: &RemoteHandle) -> FsResult<Arc<OpenFile>> {
        lock(&self.files)
            .get(&handle.id())
            .cloned()
            .ok_or_else(|| FsError::Other("unknown file handle".to_owned()))
    }

    /// The file's handle on `session`, opening it there first if it was opened on an earlier one.
    async fn remote(&self, file: &OpenFile, session: &Session) -> FsResult<RemoteHandle> {
        let (generation, id) = file.remote();
        if generation == session.generation {
            return Ok(RemoteHandle::new(id));
        }
        let _turn = file.reopening.lock().await;
        let (generation, id) = file.remote();
        if generation == session.generation {
            return Ok(RemoteHandle::new(id));
        }
        // The handle of the old session went with it, so there is nothing to close.
        let path = lock(&file.path).clone();
        let reopened = session.fs.open(&path, reopen_mode(file.mode)).await?;
        *lock(&file.remote) = (session.generation, reopened.id());
        Ok(reopened)
    }

    /// Open files follow a rename of themselves, or of a directory above them.
    fn moved(&self, from: &RemotePath, to: &RemotePath) {
        for file in lock(&self.files).values() {
            let mut path = lock(&file.path);
            if let Some(moved) = moved_path(&path, from, to) {
                *path = moved;
            }
        }
    }
}

/// Opening a file again after a reconnect finds it where it is, and leaves it as it is.
fn reopen_mode(mode: OpenMode) -> OpenMode {
    match mode.access() {
        crate::Access::Read => OpenMode::read(),
        crate::Access::Write => OpenMode::write(Disposition::OpenExisting),
        crate::Access::ReadWrite => OpenMode::read_write(Disposition::OpenExisting),
    }
}

/// Whether an open can go again after the connection dropped: only one that would not make a second
/// file, or fail because the first attempt made one.
const fn repeatable_open(mode: OpenMode) -> bool {
    !matches!(mode.disposition(), Disposition::CreateNew)
}

/// Where `path` is after `from` moved to `to`, if it was within `from`.
fn moved_path(path: &RemotePath, from: &RemotePath, to: &RemotePath) -> Option<RemotePath> {
    let rest = path.as_str().strip_prefix(from.as_str())?;
    if rest.is_empty() {
        return Some(to.clone());
    }
    let rest = rest.strip_prefix('/')?;
    to.join(rest).ok()
}

#[async_trait]
impl RemoteFs for SessionFs {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.call(true, |s| async move { s.fs.stat(path).await })
            .await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.call(true, |s| async move { s.fs.lstat(path).await })
            .await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        let file = self.file(handle)?;
        let file = &file;
        self.call(true, |s| async move {
            let remote = self.remote(file, &s).await?;
            s.fs.fstat(&remote).await
        })
        .await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.call(true, |s| async move { s.fs.read_dir(path).await })
            .await
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let (generation, remote) = self
            .call(repeatable_open(mode), |s| async move {
                let remote = s.fs.open(path, mode).await?;
                Ok((s.generation, remote))
            })
            .await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let file = OpenFile {
            path: Mutex::new(path.clone()),
            mode,
            remote: Mutex::new((generation, remote.id())),
            reopening: tokio::sync::Mutex::new(()),
        };
        lock(&self.files).insert(id, Arc::new(file));
        Ok(RemoteHandle::new(id))
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        let file = self.file(handle)?;
        let file = &file;
        self.call(true, |s| async move {
            let remote = self.remote(file, &s).await?;
            s.fs.read(&remote, offset, len).await
        })
        .await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let file = self.file(handle)?;
        let (file, data) = (&file, &data);
        // Writing the same bytes to the same place again leaves the file as one write would.
        self.call(true, |s| async move {
            let remote = self.remote(file, &s).await?;
            s.fs.write(&remote, offset, data.clone()).await
        })
        .await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        let file = self.file(handle)?;
        let file = &file;
        self.call(true, |s| async move {
            let remote = self.remote(file, &s).await?;
            s.fs.flush(&remote).await
        })
        .await
    }

    /// Never waits for a session: a handle whose session has gone is closed already.
    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        let Some(file) = lock(&self.files).remove(&handle.id()) else {
            return Err(FsError::Other("unknown file handle".to_owned()));
        };
        let (generation, id) = file.remote();
        let session = match &*self.state.borrow() {
            State::Up(session) if session.generation == generation => session.clone(),
            _ => return Ok(()),
        };
        match session.fs.close(RemoteHandle::new(id)).await {
            Err(FsError::ConnectionLost) => {
                self.mark_lost(generation, "a call found the connection gone");
                Ok(())
            }
            result => result,
        }
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        match target {
            AttrTarget::Path(path) => {
                self.call(true, |s| async move {
                    s.fs.set_attr(AttrTarget::Path(path), changes).await
                })
                .await
            }
            AttrTarget::Handle(handle) => {
                let file = self.file(handle)?;
                let file = &file;
                self.call(true, |s| async move {
                    let remote = self.remote(file, &s).await?;
                    s.fs.set_attr(AttrTarget::Handle(&remote), changes).await
                })
                .await
            }
        }
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.call(false, |s| async move { s.fs.mkdir(path).await })
            .await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.call(false, |s| async move { s.fs.remove_file(path).await })
            .await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.call(false, |s| async move { s.fs.remove_dir(path).await })
            .await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.call(
            false,
            |s| async move { s.fs.rename(from, to, replace).await },
        )
        .await?;
        self.moved(from, to);
        Ok(())
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        self.call(true, |s| async move { s.fs.statfs().await })
            .await
    }

    fn capabilities(&self) -> Capabilities {
        *lock(&self.capabilities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RemotePath {
        RemotePath::new(text).unwrap()
    }

    #[test]
    fn a_path_follows_a_rename_of_itself_or_of_a_directory_above_it() {
        let moved = |p: &str| moved_path(&path(p), &path("/a/b"), &path("/x/y"));
        assert_eq!(moved("/a/b"), Some(path("/x/y")));
        assert_eq!(moved("/a/b/c/d"), Some(path("/x/y/c/d")));
        assert_eq!(moved("/a/bc"), None);
        assert_eq!(moved("/a"), None);
    }

    #[test]
    fn reopening_never_creates_or_truncates() {
        for disposition in [
            Disposition::OpenExisting,
            Disposition::CreateNew,
            Disposition::OpenOrCreate,
            Disposition::TruncateExisting,
            Disposition::CreateOrTruncate,
        ] {
            for mode in [
                OpenMode::write(disposition),
                OpenMode::read_write(disposition),
            ] {
                let again = reopen_mode(mode);
                assert_eq!(again.disposition(), Disposition::OpenExisting);
                assert_eq!(again.access(), mode.access());
            }
        }
        assert_eq!(reopen_mode(OpenMode::read()), OpenMode::read());
    }

    #[test]
    fn only_an_exclusive_create_is_not_repeated() {
        assert!(!repeatable_open(OpenMode::write(Disposition::CreateNew)));
        assert!(repeatable_open(OpenMode::write(Disposition::OpenOrCreate)));
        assert!(repeatable_open(OpenMode::write(
            Disposition::CreateOrTruncate
        )));
        assert!(repeatable_open(OpenMode::read()));
    }
}
