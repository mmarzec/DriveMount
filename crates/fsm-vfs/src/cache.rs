// SPDX-License-Identifier: GPL-3.0-or-later

//! The attribute, directory and negative caches of spec §5.6, as a [`RemoteFs`] wrapper.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::time::Instant;

use crate::{
    Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FileKind, FsError,
    FsResult, FsStats, OpenMode, RemoteFs, RemoteHandle, RemotePath,
};

/// The size limit of each of the three caches. A listing counts as one, plus one per entry it holds.
const CAPACITY: usize = 10_000;

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

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.inner.fstat(handle).await
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
        let fill = match hit {
            Ok(entries) => return Ok(entries.as_ref().clone()),
            Err(fill) => fill,
        };

        let entries = self.inner.read_dir(path).await?;

        let to_store = (!self.ttl.is_zero() && entries.len() < CAPACITY)
            .then(|| (Arc::new(entries.clone()), attrs_of_entries(path, &entries)));
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
        }
        Ok(entries)
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let may_change = mode.disposition() != Disposition::OpenExisting;
        let _invalidate = OnDrop::new(|| {
            if may_change {
                self.state().name_changed(path);
            }
        });

        let handle = self.inner.open(path, mode).await?;

        let mut state = self.state();
        state.handles.insert(
            handle.id(),
            OpenFile {
                path: path.clone(),
                written: false,
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

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        let _invalidate = OnDrop::new(|| match target {
            AttrTarget::Path(path) => self.state().contents_changed(path),
            AttrTarget::Handle(handle) => self.state().handle_written(handle.id()),
        });
        self.inner.set_attr(target, changes).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let _invalidate = OnDrop::new(|| self.state().tree_changed(path));
        self.inner.mkdir(path).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let _invalidate = OnDrop::new(|| self.state().name_changed(path));
        self.inner.remove_file(path).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let _invalidate = OnDrop::new(|| self.state().tree_changed(path));
        self.inner.remove_dir(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        let _invalidate = OnDrop::new(|| {
            let mut state = self.state();
            state.tree_changed(from);
            state.tree_changed(to);
        });
        let result = self.inner.rename(from, to, replace).await;
        if result.is_ok() {
            self.state().handles_moved(from, to);
        }
        result
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
            attrs: Bounded::new(),
            negatives: Bounded::new(),
            listings: Bounded::new(),
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
        self.tree_generation += 1;
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

struct Slot<V> {
    value: V,
    expires: Instant,
    last_used: u64,
    weight: usize,
}

/// A map from paths to values that expire, holding at most [`CAPACITY`] in weight. When it is full,
/// the least recently used entries go first.
struct Bounded<V> {
    slots: HashMap<RemotePath, Slot<V>>,
    /// The same keys, oldest use first.
    by_use: BTreeMap<u64, RemotePath>,
    clock: u64,
    weight: usize,
}

impl<V> Bounded<V> {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            by_use: BTreeMap::new(),
            clock: 0,
            weight: 0,
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
        if expires <= now || weight > CAPACITY {
            return;
        }
        while self.weight + weight > CAPACITY {
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
        if let Some(slot) = self.slots.remove(key) {
            self.by_use.remove(&slot.last_used);
            self.weight -= slot.weight;
        }
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
        let mut map = Bounded::new();
        map.insert(&path("/a"), 1, 1, now + Duration::from_secs(1), now);
        assert_eq!(map.get(&path("/a"), now), Some(&1));
        assert_eq!(map.get(&path("/a"), now + Duration::from_secs(1)), None);
        assert_eq!(map.weight, 0);
        assert!(map.by_use.is_empty());
    }

    #[test]
    fn an_entry_already_expired_is_not_stored_and_replaces_the_old_one() {
        let now = Instant::now();
        let mut map = Bounded::new();
        map.insert(&path("/a"), 1, 1, far_future(now), now);
        map.insert(&path("/a"), 2, 1, now, now);
        assert_eq!(map.weight, 0);
        assert_eq!(map.get(&path("/a"), now), None);
    }

    #[test]
    fn the_least_recently_used_entry_goes_first() {
        let now = Instant::now();
        let mut map = Bounded::new();
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
        let mut map = Bounded::new();
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
        let mut map = Bounded::new();
        map.insert(&path("/kept"), 1, 1, far_future(now), now);
        map.insert(&path("/huge"), 2, CAPACITY + 1, far_future(now), now);
        assert_eq!(map.get(&path("/huge"), now), None);
        assert_eq!(map.get(&path("/kept"), now), Some(&1));
    }

    #[test]
    fn replacing_and_removing_keep_the_books_straight() {
        let now = Instant::now();
        let mut map = Bounded::new();
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
