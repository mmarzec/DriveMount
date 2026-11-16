// SPDX-License-Identifier: GPL-3.0-or-later

//! The path → file-handle cache (M4-02).
//!
//! WinFsp asks by path and NFSv3 works by file handle, so a path is turned into a handle by a
//! chain of LOOKUP calls from the export's root. [`HandleCache`] remembers the handles it found, so
//! the next path in the same directory costs one LOOKUP, and a path seen before costs none.
//!
//! A handle names a file, not a path: it stays valid when the file is renamed, and turns
//! `NFS3ERR_STALE` when the file is removed. So this mount's own renames move cached paths
//! ([`HandleCache::renamed`]) and its removes drop them ([`HandleCache::forget`]). A handle that
//! comes back stale anyway (another client removed the file, or the server lost it) is dropped and
//! the path looked up again ([`HandleCache::with_handle`]). Other clients' renames are noticed when
//! the entry's TTL runs out, as for the attribute cache (spec §5.6).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use drivemount_vfs::{FsError, FsResult, RemotePath};
use tokio::time::Instant;

/// The default size limit, in paths: as many as the attribute cache keeps (spec §5.6).
pub const CAPACITY: usize = 10_000;

/// How many times one resolve starts over after meeting a stale handle. Each time drops at least
/// one cached entry, so this only runs out if other clients keep removing the directories.
const MAX_ATTEMPTS: usize = 4;

/// An NFSv3 file handle: up to 64 opaque bytes from the server (RFC 1813 `nfs_fh3`). Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileHandle(Arc<[u8]>);

impl FileHandle {
    /// A handle with these bytes.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Self {
        Self(Arc::from(bytes))
    }

    /// The handle's bytes, as the server sent them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for FileHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileHandle(")?;
        for byte in self.0.iter() {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

/// How an NFS call on a handle failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleError {
    /// `NFS3ERR_STALE` (or `NFS3ERR_BADHANDLE`): the handle no longer names a file.
    Stale,
    /// Anything else, already mapped. A LOOKUP of a missing name gives `NotFound`, and one in a
    /// file rather than a directory (`NFS3ERR_NOTDIR`) gives `PathNotFound`.
    Fs(FsError),
}

impl From<FsError> for HandleError {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

/// The LOOKUP call, which [`HandleCache`] makes as it walks a path. `NfsFs` sends it over the
/// connection pool; the tests answer it from a fake server.
pub trait Lookup: Sync {
    /// The handle of `name` in the directory `dir`. `name` is one component, never `.` or `..`.
    fn lookup(
        &self,
        dir: &FileHandle,
        name: &str,
    ) -> impl Future<Output = Result<FileHandle, HandleError>> + Send;
}

/// Paths and the file handles they led to, below an export's root.
///
/// Every method is safe to call while others are in flight. A resolve that was looking paths up
/// while this mount removed or renamed something does not store what it found, so it cannot bring
/// back a path that was just forgotten.
pub struct HandleCache {
    root: FileHandle,
    ttl: Duration,
    capacity: usize,
    state: Mutex<State>,
}

struct State {
    /// Keyed by the path's string, so a directory's subtree is one range: `"/a/"` up to `"/a0"`.
    /// The root is not stored; its handle is fixed.
    entries: BTreeMap<String, Entry>,
    /// Bumped by every forget and rename.
    generation: u64,
    /// For finding the least recently used entries.
    clock: u64,
}

struct Entry {
    handle: FileHandle,
    expires: Instant,
    used: u64,
}

/// Where a resolve starts: the deepest fresh cached ancestor.
struct Start {
    handle: FileHandle,
    /// The path the handle belongs to, as a byte length of the resolved path's string.
    len: usize,
    generation: u64,
}

impl HandleCache {
    /// A cache for the export whose root is `root`, keeping each path for `ttl`. With a zero TTL
    /// every path is looked up again each time.
    #[must_use]
    pub fn new(root: FileHandle, ttl: Duration) -> Self {
        Self::with_capacity(root, ttl, CAPACITY)
    }

    /// As [`new`](Self::new), keeping at most `capacity` paths besides the root.
    #[must_use]
    pub fn with_capacity(root: FileHandle, ttl: Duration, capacity: usize) -> Self {
        Self {
            root,
            ttl,
            capacity: capacity.max(1),
            state: Mutex::new(State {
                entries: BTreeMap::new(),
                generation: 0,
                clock: 0,
            }),
        }
    }

    /// The export's root handle, from MOUNT.
    #[must_use]
    pub fn root(&self) -> &FileHandle {
        &self.root
    }

    /// The handle cached for `path`, if it is still fresh. Makes no calls.
    #[must_use]
    pub fn cached(&self, path: &RemotePath) -> Option<FileHandle> {
        if path.is_root() {
            return Some(self.root.clone());
        }
        let now = Instant::now();
        let state = self.state();
        state
            .entries
            .get(path.as_str())
            .filter(|entry| entry.expires > now)
            .map(|entry| entry.handle.clone())
    }

    /// How many paths are cached, fresh or not, besides the root.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().entries.len()
    }

    /// Whether only the root is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The handle of `path`, looking up the components after its deepest cached ancestor. A stale
    /// cached directory on the way is dropped and the walk starts again above it.
    ///
    /// # Errors
    ///
    /// `NotFound` if the last component is missing, `PathNotFound` if an earlier one is, or runs
    /// through a file; the root's handle being stale ([`root_stale`]); and any other LOOKUP error
    /// as it came.
    pub async fn resolve<L: Lookup>(&self, server: &L, path: &RemotePath) -> FsResult<FileHandle> {
        for _ in 0..MAX_ATTEMPTS {
            let start = self.start(path);
            match self.walk(server, path, start).await {
                Walk::Done(handle) => return Ok(handle),
                Walk::Failed(error) => return Err(error),
                Walk::Stale => {}
            }
        }
        Err(FsError::NotFound)
    }

    /// Runs `op` with the handle of `path`. If the handle turns out stale, `path` is forgotten,
    /// looked up again and `op` run once more with the new handle, so only call this for an `op`
    /// that is safe to repeat after a stale handle (it did nothing then).
    ///
    /// # Errors
    ///
    /// Those of [`resolve`](Self::resolve) and `op`. A handle that is stale again after looking the
    /// path up afresh means the file went away in between: `NotFound`.
    pub async fn with_handle<L, T, Op, Fut>(
        &self,
        server: &L,
        path: &RemotePath,
        mut op: Op,
    ) -> FsResult<T>
    where
        L: Lookup,
        Op: FnMut(FileHandle) -> Fut,
        Fut: Future<Output = Result<T, HandleError>>,
    {
        let handle = self.resolve(server, path).await?;
        match op(handle).await {
            Ok(value) => return Ok(value),
            Err(HandleError::Fs(error)) => return Err(error),
            Err(HandleError::Stale) if path.is_root() => return Err(root_stale()),
            Err(HandleError::Stale) => {}
        }
        tracing::debug!(%path, "stale file handle, looking the path up again");
        self.forget(path);
        let handle = self.resolve(server, path).await?;
        match op(handle).await {
            Ok(value) => Ok(value),
            Err(HandleError::Fs(error)) => Err(error),
            Err(HandleError::Stale) => {
                self.forget(path);
                Err(FsError::NotFound)
            }
        }
    }

    /// Remembers that `path` has `handle`, as CREATE, MKDIR and READDIRPLUS tell. Does nothing for
    /// the root.
    pub fn insert(&self, path: &RemotePath, handle: FileHandle) {
        if path.is_root() {
            return;
        }
        let now = Instant::now();
        let mut state = self.state();
        state.put(path.as_str().to_owned(), handle, now + self.ttl);
        state.evict(self.capacity, now);
    }

    /// Forgets `path` and everything below it: after this mount removed it, or after it failed in
    /// a way that may mean it changed.
    pub fn forget(&self, path: &RemotePath) {
        let mut state = self.state();
        state.generation += 1;
        if path.is_root() {
            state.entries.clear();
        } else {
            state.remove_subtree(path.as_str());
        }
    }

    /// Moves `from` and everything below it to `to`, after this mount renamed it. Whatever was at
    /// `to` is forgotten. The handles stay as they were: a rename does not change them.
    pub fn renamed(&self, from: &RemotePath, to: &RemotePath) {
        let mut state = self.state();
        state.generation += 1;
        if from == to {
            return;
        }
        if from.is_root() || to.is_root() {
            // The contract forbids both; forget everything rather than guess.
            state.entries.clear();
            return;
        }
        let moved = state.remove_subtree(from.as_str());
        state.remove_subtree(to.as_str());
        let from_len = from.as_str().len();
        for (path, entry) in moved {
            let mut new_path = to.as_str().to_owned();
            new_path.push_str(&path[from_len..]);
            state.entries.insert(new_path, entry);
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Nothing here panics halfway through a change that matters: at worst an entry is missing,
        // which costs a LOOKUP.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The deepest ancestor of `path` (or `path` itself) with a fresh entry, else the root.
    fn start(&self, path: &RemotePath) -> Start {
        let now = Instant::now();
        let mut state = self.state();
        let generation = state.generation;
        state.clock += 1;
        let clock = state.clock;
        let mut prefix = path.as_str();
        while prefix.len() > 1 {
            if let Some(entry) = state.entries.get_mut(prefix)
                && entry.expires > now
            {
                entry.used = clock;
                return Start {
                    handle: entry.handle.clone(),
                    len: prefix.len(),
                    generation,
                };
            }
            prefix = parent_str(prefix);
        }
        Start {
            handle: self.root.clone(),
            len: 1,
            generation,
        }
    }

    async fn walk<L: Lookup>(&self, server: &L, path: &RemotePath, start: Start) -> Walk {
        let full = path.as_str();
        let rest = &full[start.len..];
        let names: Vec<&str> = rest.split('/').filter(|name| !name.is_empty()).collect();
        let mut handle = start.handle;
        let mut dir_len = start.len;
        let mut found = Vec::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            let dir = &full[..dir_len];
            match server.lookup(&handle, name).await {
                Ok(child) => {
                    // From the root, the first component's path starts right after its `/`.
                    dir_len = if dir_len == 1 {
                        1 + name.len()
                    } else {
                        dir_len + 1 + name.len()
                    };
                    found.push((full[..dir_len].to_owned(), child.clone()));
                    handle = child;
                }
                Err(HandleError::Stale) => {
                    if dir == "/" {
                        return Walk::Failed(root_stale());
                    }
                    tracing::debug!(dir, "stale directory handle, starting the lookup again");
                    let mut state = self.state();
                    state.generation += 1;
                    state.remove_subtree(dir);
                    return Walk::Stale;
                }
                Err(HandleError::Fs(FsError::NotFound)) if index + 1 < names.len() => {
                    return Walk::Failed(FsError::PathNotFound);
                }
                Err(HandleError::Fs(error)) => return Walk::Failed(error),
            }
        }
        if !found.is_empty() {
            let now = Instant::now();
            let mut state = self.state();
            if state.generation == start.generation {
                let expires = now + self.ttl;
                for (found_path, found_handle) in found {
                    state.put(found_path, found_handle, expires);
                }
                state.evict(self.capacity, now);
            }
        }
        Walk::Done(handle)
    }
}

enum Walk {
    Done(FileHandle),
    Failed(FsError),
    /// A cached directory handle was stale and has been dropped: start again.
    Stale,
}

impl State {
    fn put(&mut self, path: String, handle: FileHandle, expires: Instant) {
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(
            path,
            Entry {
                handle,
                expires,
                used,
            },
        );
    }

    /// Removes `path` and the paths below it, and returns them.
    fn remove_subtree(&mut self, path: &str) -> Vec<(String, Entry)> {
        let mut removed = Vec::new();
        if let Some(entry) = self.entries.remove(path) {
            removed.push((path.to_owned(), entry));
        }
        // Every path below `path` starts with `path/`; `0` is the character after `/`.
        let low = format!("{path}/");
        let high = format!("{path}0");
        let below: Vec<String> = self
            .entries
            .range::<str, _>((
                Bound::Included(low.as_str()),
                Bound::Excluded(high.as_str()),
            ))
            .map(|(key, _)| key.clone())
            .collect();
        for key in below {
            if let Some(entry) = self.entries.remove(&key) {
                removed.push((key, entry));
            }
        }
        removed
    }

    /// Over `capacity`, drops expired entries and then the least recently used, down to 90%, so
    /// the sort is paid once per many inserts.
    fn evict(&mut self, capacity: usize, now: Instant) {
        if self.entries.len() <= capacity {
            return;
        }
        self.entries.retain(|_, entry| entry.expires > now);
        let target = capacity - capacity / 10;
        if self.entries.len() <= target {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.used, key.clone()))
            .collect();
        by_age.sort_unstable();
        let excess = self.entries.len() - target;
        for (_, key) in by_age.into_iter().take(excess) {
            self.entries.remove(&key);
        }
    }
}

/// The parent of a normalised path's string; the root's parent is the root.
fn parent_str(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(slash) => &path[..slash],
    }
}

/// The export's root handle is stale: the export was removed or replaced on the server. Nothing
/// below it can be reached without a new MOUNT.
#[must_use]
pub fn root_stale() -> FsError {
    FsError::Other("the export's root file handle is stale; mount it again".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_a_path_string() {
        assert_eq!(parent_str("/a/b/c"), "/a/b");
        assert_eq!(parent_str("/a"), "/");
        assert_eq!(parent_str("/"), "/");
    }

    #[test]
    fn a_subtree_is_its_path_and_the_paths_below_it_only() {
        let mut state = State {
            entries: BTreeMap::new(),
            generation: 0,
            clock: 0,
        };
        let expires = Instant::now() + Duration::from_secs(60);
        for path in ["/a", "/a/b", "/a/b/c", "/a b", "/a-b", "/a0", "/ab", "/b"] {
            state.put(path.to_owned(), FileHandle::new(path.as_bytes()), expires);
        }
        let mut removed: Vec<String> = state
            .remove_subtree("/a")
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        removed.sort();
        assert_eq!(removed, ["/a", "/a/b", "/a/b/c"]);
        let left: Vec<&str> = state.entries.keys().map(String::as_str).collect();
        assert_eq!(left, ["/a b", "/a-b", "/a0", "/ab", "/b"]);
    }

    #[test]
    fn a_handle_prints_as_hex() {
        assert_eq!(
            format!("{:?}", FileHandle::new(&[0x01, 0xab])),
            "FileHandle(01ab)"
        );
    }
}
