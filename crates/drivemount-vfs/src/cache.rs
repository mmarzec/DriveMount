// SPDX-License-Identifier: GPL-3.0-or-later

//! The attribute, directory and negative caches of spec §5.6, as a [`RemoteFs`] wrapper.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::time::Instant;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FileKind, FsError,
    FsResult, FsStats, NameError, NameIndex, OpenMode, RemoteFs, RemoteHandle, RemotePath,
    Resolved, resolve_name,
};

/// The size limit of each of the three caches. A listing counts as one, plus one per entry it holds.
const CAPACITY: usize = 10_000;

/// The size limit of the names kept for finding a name in any case, counted the same way. A name
/// takes less room than an entry, and this lets a folder of 10,000 files be filled without listing
/// it again for each new name (NFR-PERF-07).
const NAMES_CAPACITY: usize = 2 * CAPACITY;

const NEGATIVE_TTL: Duration = Duration::from_secs(1);
const NOISE_NEGATIVE_TTL: Duration = Duration::from_secs(60);

/// Names Explorer keeps asking for, which rarely exist (spec §5.6).
const NOISE_NAMES: [&str; 10] = [
    "desktop.ini",
    "Desktop.ini",
    "thumbs.db",
    "Thumbs.db",
    "autorun.inf",
    "AutoRun.inf",
    "folder.jpg",
    "folder.gif",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// How many generation counters paths are shared out over; see [`Fill`]. Two paths on one counter
/// only cost the occasional answer that does not get cached.
const STRIPES: usize = 256;

/// A [`RemoteFs`] that remembers what the server said, and forgets it when this mount changes it.
///
/// [`stat`](RemoteFs::stat) and [`read_dir`](RemoteFs::read_dir) answer from memory while the answer
/// is fresh; everything else goes to the server. A listing also fills the attributes of its entries,
/// except symbolic links, whose entries describe the link and not its target.
///
/// A change takes effect in the caches when the call ends, however it ends. A call that failed or
/// was dropped by a deadline may still have reached the server, so it invalidates as well, and an
/// answer that was on its way while a change finished is not stored. Changes made by other clients
/// show up when the TTL runs out, and so do changes that reach a path through a symbolic link.
///
/// The negative TTLs are fixed by spec §5.6 and stay in force when `ttl` is zero.
pub struct CachedFs<F> {
    inner: F,
    ttl: Duration,
    state: Mutex<State>,
}

impl<F: RemoteFs> CachedFs<F> {
    /// Wraps `inner`, keeping attributes and listings for `ttl`. Zero turns those two caches off.
    #[must_use]
    pub fn new(inner: F, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            state: Mutex::new(State::new()),
        }
    }

    /// Nothing here panics while changing the caches in a way that matters: at worst one is missing
    /// an entry, which only costs a round trip.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Finds a Windows name in a directory, ignoring case where that is safe ([`resolve_name`]):
    /// the remote name it means, or `None` if it means none, or more than one.
    ///
    /// The names of a directory are kept for the profile's TTL, like its listing, but unlike the
    /// listing they stay up to date through this mount's own creates, removes and renames, as only
    /// the names matter here. Filling a folder therefore lists it once, not once per new file.
    ///
    /// # Errors
    ///
    /// The outer error is listing the directory failing; the inner one is `windows_name` having no
    /// remote form.
    pub async fn find_name(
        &self,
        dir: &RemotePath,
        windows_name: &str,
    ) -> FsResult<Result<Option<String>, NameError>> {
        let fill = {
            let mut state = self.state();
            let now = Instant::now();
            if let Some(index) = state.names.get(dir, now) {
                return Ok(found_name(index.resolve(windows_name)));
            }
            let listed = state
                .listings
                .get_with_expiry(dir, now)
                .map(|(entries, expires)| (Arc::clone(entries), expires));
            if let Some((entries, expires)) = listed {
                // The names are only as fresh as the listing they come from.
                let index = NameIndex::new(entries.iter().map(|entry| entry.name.as_str()));
                let result = found_name(index.resolve(windows_name));
                let weight = index.len() + 1;
                state.names.insert(dir, index, weight, expires, now);
                return Ok(result);
            }
            state.begin_fill(dir, now)
        };
        let entries = self.fetch_listing(dir, fill).await?;
        let names = entries.iter().map(|entry| entry.name.as_str());
        Ok(found_name(resolve_name(windows_name, names)))
    }

    /// Lists a directory on the server, keeping the listing, its entries' attributes and its names
    /// unless a change overtook the answer.
    async fn fetch_listing(&self, path: &RemotePath, fill: Fill) -> FsResult<Vec<DirEntry>> {
        let entries = self.inner.read_dir(path).await?;

        let keep = !self.ttl.is_zero();
        let to_store = (keep && entries.len() < CAPACITY)
            .then(|| (Arc::new(entries.clone()), attrs_of_entries(path, &entries)));
        let names = (keep && entries.len() < NAMES_CAPACITY)
            .then(|| NameIndex::new(entries.iter().map(|entry| entry.name.as_str())));
        let mut state = self.state();
        if state.is_current(&fill) {
            let now = Instant::now();
            let expires = fill.started + self.ttl;
            state.negatives.remove(path);
            if let Some((listing, attrs)) = to_store {
                state
                    .listings
                    .insert(path, listing, entries.len() + 1, expires, now);
                for (child, attr) in attrs {
                    state.negatives.remove(&child);
                    state.attrs.insert(&child, attr, 1, expires, now);
                }
            }
            if let Some(names) = names {
                let weight = names.len() + 1;
                state.names.insert(path, names, weight, expires, now);
            }
        }
        Ok(entries)
    }
}

/// The remote name a lookup found, if it found exactly one.
fn found_name(resolved: Result<Resolved<'_>, NameError>) -> Result<Option<String>, NameError> {
    Ok(match resolved? {
        Resolved::Exact(name) | Resolved::CaseInsensitive(name) => Some(name.to_owned()),
        Resolved::Ambiguous | Resolved::NotFound => None,
    })
}

#[async_trait]
impl<F: RemoteFs> RemoteFs for CachedFs<F> {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        let fill = {
            let mut state = self.state();
            let now = Instant::now();
            if let Some(attr) = state.attrs.get(path, now) {
                return Ok(*attr);
            }
            if let Some(error) = state.negatives.get(path, now) {
                return Err(error.clone());
            }
            state.begin_fill(path, now)
        };

        let result = self.inner.stat(path).await;

        let mut state = self.state();
        if state.is_current(&fill) {
            let now = Instant::now();
            match &result {
                Ok(attr) => {
                    state.negatives.remove(path);
                    state
                        .attrs
                        .insert(path, *attr, 1, fill.started + self.ttl, now);
                }
                Err(error @ (FsError::NotFound | FsError::PathNotFound)) => {
                    let expires = fill.started + negative_ttl(path);
                    state.negatives.insert(path, error.clone(), 1, expires, now);
                }
                Err(_) => {}
            }
        }
        result
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.inner.lstat(path).await
    }

    /// The answer also stands for the path the handle was opened on, as a `stat` would, so the next
    /// lookup of a file being written does not ask again.
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        let fill = self.state().begin_handle_fill(handle.id());
        let result = self.inner.fstat(handle).await;
        if let (Some((path, fill)), Ok(attr)) = (fill, &result) {
            let mut state = self.state();
            if state.is_current(&fill) {
                state.attr_learned(&path, *attr, &fill, self.ttl);
            }
        }
        result
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        let hit = {
            let mut state = self.state();
            let now = Instant::now();
            state
                .listings
                .get(path, now)
                .cloned()
                .ok_or_else(|| state.begin_fill(path, now))
        };
        // Copied outside the lock: a listing can be long, and every other call needs that lock.
        match hit {
            Ok(entries) => Ok(entries.as_ref().clone()),
            Err(fill) => self.fetch_listing(path, fill).await,
        }
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let may_change = mode.disposition() != Disposition::OpenExisting;
        let opened = AtomicBool::new(false);
        let _invalidate = OnDrop::new(|| {
            if may_change {
                let mut state = self.state();
                state.name_changed(path);
                // Whatever the disposition, the name exists now.
                state.names_learned(path, opened.load(Ordering::Relaxed).then_some(Exists::Yes));
            }
        });

        let handle = self.inner.open(path, mode).await?;
        opened.store(true, Ordering::Relaxed);

        let mut state = self.state();
        state.handles.insert(
            handle.id(),
            OpenFile {
                path: path.clone(),
                written: false,
                detached: false,
            },
        );
        // It exists, whatever an earlier lookup found.
        state.negatives.remove(path);
        drop(state);
        Ok(handle)
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.inner.read(handle, offset, len).await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let _invalidate = OnDrop::new(|| self.state().handle_written(handle.id()));
        self.inner.write(handle, offset, data).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        let _invalidate = OnDrop::new(|| self.state().handle_flushed(handle.id()));
        self.inner.flush(handle).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        let id = handle.id();
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            state.handle_flushed(id);
            state.handles.remove(&id);
        });
        self.inner.close(handle).await
    }

    /// The answer is the file after the change, and is kept as a `stat`'s would be, unless anything
    /// else changed the path's counter while the call was out.
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        let fill = {
            let state = self.state();
            match target {
                AttrTarget::Path(path) => {
                    Some((path.clone(), state.begin_fill(path, Instant::now())))
                }
                AttrTarget::Handle(handle) => state.begin_handle_fill(handle.id()),
            }
        };
        let answer = OnceLock::new();
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            let current = fill
                .as_ref()
                .is_some_and(|(_, fill)| state.is_current(fill));
            match target {
                AttrTarget::Path(path) => state.contents_changed(path),
                AttrTarget::Handle(handle) => state.handle_written(handle.id()),
            }
            if current && let (Some((path, fill)), Some(attr)) = (&fill, answer.get()) {
                state.attr_learned(path, *attr, fill, self.ttl);
            }
        });
        let attr = self.inner.set_attr(target, changes).await?;
        let _ = answer.set(attr);
        Ok(attr)
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let done = AtomicBool::new(false);
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            state.tree_changed(path);
            state.names_learned(path, done.load(Ordering::Relaxed).then_some(Exists::Yes));
        });
        let attr = self.inner.mkdir(path).await?;
        done.store(true, Ordering::Relaxed);
        Ok(attr)
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let done = AtomicBool::new(false);
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            state.name_changed(path);
            state.handles_detached(path);
            state.names_learned(path, done.load(Ordering::Relaxed).then_some(Exists::No));
        });
        self.inner.remove_file(path).await?;
        done.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let done = AtomicBool::new(false);
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            state.tree_changed(path);
            state.handles_detached(path);
            state.names_learned(path, done.load(Ordering::Relaxed).then_some(Exists::No));
        });
        self.inner.remove_dir(path).await?;
        done.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        let done = AtomicBool::new(false);
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            let done = done.load(Ordering::Relaxed);
            state.tree_changed(from);
            state.tree_changed(to);
            // What was at `to` lost its name; after an unknown outcome, so may what was at `from`.
            state.handles_detached(to);
            if done {
                state.handles_moved(from, to);
            } else {
                state.handles_detached(from);
            }
            state.names_learned(from, done.then_some(Exists::No));
            state.names_learned(to, done.then_some(Exists::Yes));
        });
        self.inner.rename(from, to, replace).await?;
        done.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        self.inner.statfs().await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// How long `path` stays known to be missing.
fn negative_ttl(path: &RemotePath) -> Duration {
    if path
        .file_name()
        .is_some_and(|name| NOISE_NAMES.contains(&name))
    {
        NOISE_NEGATIVE_TTL
    } else {
        NEGATIVE_TTL
    }
}

/// What a listing says about `stat` of its entries. A symbolic link's entry says nothing about that.
fn attrs_of_entries(dir: &RemotePath, entries: &[DirEntry]) -> Vec<(RemotePath, Attr)> {
    entries
        .iter()
        .filter(|entry| entry.attr.kind != FileKind::Symlink)
        .filter_map(|entry| Some((dir.join(&entry.name).ok()?, entry.attr)))
        .collect()
}

/// Whether `path` is `dir` or lies below it.
fn is_within(path: &RemotePath, dir: &RemotePath) -> bool {
    dir.is_root()
        || path
            .as_str()
            .strip_prefix(dir.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Runs a closure when dropped, which covers a future being dropped in the middle of a call.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    fn new(action: F) -> Self {
        Self(Some(action))
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}

struct OpenFile {
    /// Where the file is now, following local renames. Only as good as those renames' outcome: after
    /// one that failed without saying whether it happened, this is the old path until the TTL runs out.
    path: RemotePath,
    /// Whether anything was changed through this handle. Servers can update the times as late as
    /// close, so a written file is invalidated again then. Handles that changed nothing must not
    /// invalidate, or browsing would empty the caches.
    written: bool,
    /// Whether the path may no longer name this file, after a local remove, a rename over it, or a
    /// rename whose outcome is unknown. The file a handle has open outlives its name, so `fstat`
    /// answers for it are then not answers for the path.
    detached: bool,
}

/// A note taken before asking the server, to tell afterwards whether the answer can be stored.
///
/// The answer may describe the time before a local change that finished while it was on its way, and
/// storing it would hide that change until the TTL ran out. So each change advances the counter of
/// the paths it affects, and an answer is stored only if the counter of its path has not moved.
struct Fill {
    /// Freshness counts from here, so nothing is ever served older than its TTL.
    started: Instant,
    stripe: usize,
    stripe_generation: u64,
    tree_generation: u64,
}

struct State {
    attrs: Bounded<Attr>,
    negatives: Bounded<FsError>,
    listings: Bounded<Arc<Vec<DirEntry>>>,
    /// The names of directories, for [`CachedFs::find_name`]. Unlike a listing, a directory's names
    /// are updated in place by changes through this mount whose outcome is known.
    names: Bounded<NameIndex>,
    handles: HashMap<u64, OpenFile>,
    hasher: RandomState,
    /// Advanced by changes to one path. Any change to an entry also advances its directory's counter,
    /// which covers what a listing says about its entries.
    stripes: [u64; STRIPES],
    /// Advanced by changes to whole subtrees, which would be too many paths to go through.
    tree_generation: u64,
}

impl State {
    fn new() -> Self {
        Self {
            attrs: Bounded::new(CAPACITY),
            negatives: Bounded::new(CAPACITY),
            listings: Bounded::new(CAPACITY),
            names: Bounded::new(NAMES_CAPACITY),
            handles: HashMap::new(),
            hasher: RandomState::new(),
            stripes: [0; STRIPES],
            tree_generation: 0,
        }
    }

    fn stripe(&self, path: &RemotePath) -> usize {
        // Truncating on 32-bit targets does no harm to a hash.
        #[allow(clippy::cast_possible_truncation)]
        let hash = self.hasher.hash_one(path) as usize;
        hash % STRIPES
    }

    fn begin_fill(&self, path: &RemotePath, now: Instant) -> Fill {
        let stripe = self.stripe(path);
        Fill {
            started: now,
            stripe,
            stripe_generation: self.stripes[stripe],
            tree_generation: self.tree_generation,
        }
    }

    fn is_current(&self, fill: &Fill) -> bool {
        self.stripes[fill.stripe] == fill.stripe_generation
            && self.tree_generation == fill.tree_generation
    }

    /// A [`Fill`] for the path an open handle stands for, if this cache saw it opened.
    fn begin_handle_fill(&self, id: u64) -> Option<(RemotePath, Fill)> {
        let file = self.handles.get(&id).filter(|file| !file.detached)?;
        let path = file.path.clone();
        let fill = self.begin_fill(&path, Instant::now());
        Some((path, fill))
    }

    /// Keeps `attr` as the answer for `path`, from a fill that is still current.
    fn attr_learned(&mut self, path: &RemotePath, attr: Attr, fill: &Fill, ttl: Duration) {
        self.negatives.remove(path);
        self.attrs
            .insert(path, attr, 1, fill.started + ttl, Instant::now());
    }

    fn advance(&mut self, path: &RemotePath) {
        let stripe = self.stripe(path);
        self.stripes[stripe] += 1;
    }

    /// What `path` holds changed, as after a write. Its parent's listing shows the new size and time.
    fn contents_changed(&mut self, path: &RemotePath) {
        self.attrs.remove(path);
        self.advance(path);
        if let Some(parent) = path.parent() {
            self.listings.remove(&parent);
            self.advance(&parent);
        }
    }

    /// `path` appeared or went away. That also changes its parent's own time.
    fn name_changed(&mut self, path: &RemotePath) {
        self.contents_changed(path);
        self.negatives.remove(path);
        if let Some(parent) = path.parent() {
            self.attrs.remove(&parent);
        }
    }

    /// `path` appeared or went away with anything below it. Entries below it have to go too: after a
    /// directory is renamed, the old children are gone and the new ones may have been known missing.
    fn tree_changed(&mut self, path: &RemotePath) {
        self.name_changed(path);
        self.attrs.remove_within(path);
        self.negatives.remove_within(path);
        self.listings.remove_within(path);
        self.names.remove_within(path);
        self.tree_generation += 1;
    }

    /// Brings the names of `path`'s directory up to date after a change to `path`: it now exists, or
    /// it does not, or nobody knows, because the call failed or was dropped and the server may or may
    /// not have acted. Not knowing drops the names, to be listed again.
    fn names_learned(&mut self, path: &RemotePath, exists: Option<Exists>) {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let Some(exists) = exists else {
            self.names.remove(&dir);
            return;
        };
        self.names.update(&dir, Instant::now(), |names| {
            match exists {
                Exists::Yes => names.insert(name),
                Exists::No => names.remove(name),
            }
            names.len() + 1
        });
    }

    fn handle_written(&mut self, id: u64) {
        if let Some(file) = self.handles.get_mut(&id) {
            file.written = true;
            let path = file.path.clone();
            self.contents_changed(&path);
        }
    }

    fn handle_flushed(&mut self, id: u64) {
        if self.handles.get(&id).is_some_and(|file| file.written) {
            self.handle_written(id);
        }
    }

    fn handles_detached(&mut self, path: &RemotePath) {
        for file in self.handles.values_mut() {
            if is_within(&file.path, path) {
                file.detached = true;
            }
        }
    }

    fn handles_moved(&mut self, from: &RemotePath, to: &RemotePath) {
        for file in self.handles.values_mut() {
            if let Some(moved) = file
                .path
                .as_str()
                .strip_prefix(from.as_str())
                .filter(|_| is_within(&file.path, from))
                .and_then(|rest| to.join(rest.trim_start_matches('/')).ok())
            {
                file.path = moved;
            }
        }
    }
}

/// What a change that succeeded says about its path.
#[derive(Clone, Copy)]
enum Exists {
    Yes,
    No,
}

struct Slot<V> {
    value: V,
    expires: Instant,
    last_used: u64,
    weight: usize,
}

/// A map from paths to values that expire, holding at most `capacity` in weight. When it is full,
/// the least recently used entries go first.
struct Bounded<V> {
    slots: HashMap<RemotePath, Slot<V>>,
    /// The same keys, oldest use first.
    by_use: BTreeMap<u64, RemotePath>,
    clock: u64,
    weight: usize,
    capacity: usize,
}

impl<V> Bounded<V> {
    fn new(capacity: usize) -> Self {
        Self {
            slots: HashMap::new(),
            by_use: BTreeMap::new(),
            clock: 0,
            weight: 0,
            capacity,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn get(&mut self, key: &RemotePath, now: Instant) -> Option<&V> {
        if self.slots.get(key)?.expires <= now {
            self.remove(key);
            return None;
        }
        let used = self.tick();
        let slot = self.slots.get_mut(key)?;
        let previous = std::mem::replace(&mut slot.last_used, used);
        if let Some(owned_key) = self.by_use.remove(&previous) {
            self.by_use.insert(used, owned_key);
        }
        Some(&slot.value)
    }

    /// [`get`](Self::get), with when the entry expires.
    fn get_with_expiry(&mut self, key: &RemotePath, now: Instant) -> Option<(&V, Instant)> {
        self.get(key, now)?;
        self.slots.get(key).map(|slot| (&slot.value, slot.expires))
    }

    /// Changes a live entry in place, keeping when it expires. `change` returns the entry's new
    /// weight; one that no longer fits goes.
    fn update(&mut self, key: &RemotePath, now: Instant, change: impl FnOnce(&mut V) -> usize) {
        let Some(mut slot) = self.take(key) else {
            return;
        };
        let weight = change(&mut slot.value);
        self.insert(key, slot.value, weight, slot.expires, now);
    }

    /// Replaces any entry for `key`. An entry that would be expired already, or could never fit, is
    /// not stored, and the old one still goes.
    fn insert(
        &mut self,
        key: &RemotePath,
        value: V,
        weight: usize,
        expires: Instant,
        now: Instant,
    ) {
        self.remove(key);
        if expires <= now || weight > self.capacity {
            return;
        }
        while self.weight + weight > self.capacity {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            if let Some(slot) = self.slots.remove(&oldest) {
                self.weight -= slot.weight;
            }
        }
        let last_used = self.tick();
        self.by_use.insert(last_used, key.clone());
        self.weight += weight;
        self.slots.insert(
            key.clone(),
            Slot {
                value,
                expires,
                last_used,
                weight,
            },
        );
    }

    fn remove(&mut self, key: &RemotePath) {
        self.take(key);
    }

    fn take(&mut self, key: &RemotePath) -> Option<Slot<V>> {
        let slot = self.slots.remove(key)?;
        self.by_use.remove(&slot.last_used);
        self.weight -= slot.weight;
        Some(slot)
    }

    /// Removes `dir` and everything below it. Goes through every entry, which is at most a fraction
    /// of a millisecond at [`CAPACITY`]: little next to the round trip of the change causing it.
    fn remove_within(&mut self, dir: &RemotePath) {
        let Self {
            slots,
            by_use,
            weight,
            ..
        } = self;
        slots.retain(|key, slot| {
            let keep = !is_within(key, dir);
            if !keep {
                by_use.remove(&slot.last_used);
                *weight -= slot.weight;
            }
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> RemotePath {
        RemotePath::new(path).unwrap()
    }

    fn attr(kind: FileKind) -> Attr {
        Attr {
            kind,
            size: 7,
            perm: None,
            mtime: None,
            atime: None,
        }
    }

    fn numbered(i: usize) -> RemotePath {
        path(&format!("/{i}"))
    }

    #[test]
    fn is_within_matches_whole_components() {
        assert!(is_within(&path("/a"), &path("/a")));
        assert!(is_within(&path("/a/b/c"), &path("/a")));
        assert!(is_within(&path("/a"), &RemotePath::root()));
        assert!(is_within(&RemotePath::root(), &RemotePath::root()));
        assert!(!is_within(&path("/ab"), &path("/a")));
        assert!(!is_within(&path("/a"), &path("/a/b")));
        assert!(!is_within(&RemotePath::root(), &path("/a")));
    }

    #[test]
    fn a_symlink_entry_says_nothing_about_stat() {
        let entries = [
            DirEntry {
                name: "file".to_owned(),
                attr: attr(FileKind::File),
            },
            DirEntry {
                name: "link".to_owned(),
                attr: attr(FileKind::Symlink),
            },
            DirEntry {
                name: "dir".to_owned(),
                attr: attr(FileKind::Dir),
            },
        ];
        let names: Vec<_> = attrs_of_entries(&path("/d"), &entries)
            .into_iter()
            .map(|(child, _)| child)
            .collect();
        assert_eq!(names, [path("/d/file"), path("/d/dir")]);
    }

    #[test]
    fn only_the_listed_names_are_noise() {
        for name in NOISE_NAMES {
            let noise = RemotePath::root().join(name).unwrap();
            assert_eq!(negative_ttl(&noise), NOISE_NEGATIVE_TTL, "{name}");
        }
        for name in ["/DESKTOP.INI", "/desktop.ini/x", "/notes.txt", "/"] {
            assert_eq!(negative_ttl(&path(name)), NEGATIVE_TTL, "{name}");
        }
    }

    #[test]
    fn handles_follow_renames_of_themselves_and_their_directories() {
        let mut state = State::new();
        for (id, at) in [(1, "/a"), (2, "/a/b/c"), (3, "/ab"), (4, "/z")] {
            let file = OpenFile {
                path: path(at),
                written: false,
                detached: false,
            };
            state.handles.insert(id, file);
        }
        state.handles_moved(&path("/a"), &path("/x/y"));
        assert_eq!(state.handles[&1].path, path("/x/y"));
        assert_eq!(state.handles[&2].path, path("/x/y/b/c"));
        assert_eq!(state.handles[&3].path, path("/ab"));
        assert_eq!(state.handles[&4].path, path("/z"));
    }

    fn far_future(now: Instant) -> Instant {
        now + Duration::from_secs(3600)
    }

    #[test]
    fn expired_entries_are_dropped_when_looked_up() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        map.insert(&path("/a"), 1, 1, now + Duration::from_secs(1), now);
        assert_eq!(map.get(&path("/a"), now), Some(&1));
        assert_eq!(map.get(&path("/a"), now + Duration::from_secs(1)), None);
        assert_eq!(map.weight, 0);
        assert!(map.by_use.is_empty());
    }

    #[test]
    fn an_entry_already_expired_is_not_stored_and_replaces_the_old_one() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        map.insert(&path("/a"), 1, 1, far_future(now), now);
        map.insert(&path("/a"), 2, 1, now, now);
        assert_eq!(map.weight, 0);
        assert_eq!(map.get(&path("/a"), now), None);
    }

    #[test]
    fn the_least_recently_used_entry_goes_first() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        for i in 0..CAPACITY {
            map.insert(&numbered(i), i, 1, far_future(now), now);
        }
        assert_eq!(map.get(&numbered(0), now), Some(&0));

        map.insert(&numbered(CAPACITY), CAPACITY, 1, far_future(now), now);

        assert_eq!(map.weight, CAPACITY);
        assert_eq!(map.slots.len(), CAPACITY);
        assert_eq!(map.by_use.len(), CAPACITY);
        assert_eq!(map.get(&numbered(0), now), Some(&0));
        assert_eq!(map.get(&numbered(1), now), None);
        assert_eq!(map.get(&numbered(CAPACITY), now), Some(&CAPACITY));
    }

    #[test]
    fn heavy_entries_push_out_as_many_as_needed() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        for i in 0..CAPACITY {
            map.insert(&numbered(i), i, 1, far_future(now), now);
        }
        map.insert(&path("/heavy"), 0, 100, far_future(now), now);
        assert_eq!(map.weight, CAPACITY);
        assert_eq!(map.slots.len(), CAPACITY - 99);
        assert_eq!(map.get(&numbered(99), now), None);
        assert_eq!(map.get(&numbered(100), now), Some(&100));
    }

    #[test]
    fn an_entry_that_can_never_fit_is_not_stored() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        map.insert(&path("/kept"), 1, 1, far_future(now), now);
        map.insert(&path("/huge"), 2, CAPACITY + 1, far_future(now), now);
        assert_eq!(map.get(&path("/huge"), now), None);
        assert_eq!(map.get(&path("/kept"), now), Some(&1));
    }

    #[test]
    fn replacing_and_removing_keep_the_books_straight() {
        let now = Instant::now();
        let mut map = Bounded::new(CAPACITY);
        map.insert(&path("/a"), 1, 5, far_future(now), now);
        map.insert(&path("/a"), 2, 3, far_future(now), now);
        map.insert(&path("/a/b"), 3, 2, far_future(now), now);
        map.insert(&path("/ab"), 4, 1, far_future(now), now);
        assert_eq!(map.weight, 6);

        map.remove_within(&path("/a"));

        assert_eq!(map.weight, 1);
        assert_eq!(map.by_use.len(), 1);
        assert_eq!(map.get(&path("/ab"), now), Some(&4));

        map.remove(&path("/ab"));
        assert_eq!(map.weight, 0);
        assert!(map.by_use.is_empty());
    }

    #[test]
    fn a_fill_is_stale_after_a_change_to_its_path_its_entries_or_any_tree() {
        let now = Instant::now();
        let dir = path("/d");
        let mut state = State::new();

        let fill = state.begin_fill(&dir, now);
        assert!(state.is_current(&fill));
        state.contents_changed(&dir);
        assert!(!state.is_current(&fill));

        let fill = state.begin_fill(&dir, now);
        state.contents_changed(&path("/d/entry"));
        assert!(!state.is_current(&fill));

        let fill = state.begin_fill(&dir, now);
        state.tree_changed(&path("/somewhere/else"));
        assert!(!state.is_current(&fill));
    }

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn closing_forgets_the_handle_even_if_the_close_fails() {
        let fs = CachedFs::new(crate::MemFs::new(), Duration::from_secs(5));
        let mode = OpenMode::write(Disposition::CreateNew);
        let handle = fs.open(&path("/f"), mode).await.unwrap();
        assert_eq!(fs.state().handles.len(), 1);

        fs.close(handle).await.unwrap();
        assert!(fs.state().handles.is_empty());

        let mode = OpenMode::write(Disposition::OpenExisting);
        let handle = fs.open(&path("/f"), mode).await.unwrap();
        let stale_copy = RemoteHandle::new(handle.id());
        fs.close(stale_copy).await.unwrap();
        fs.close(handle).await.unwrap_err();
        assert!(fs.state().handles.is_empty());
    }
}
