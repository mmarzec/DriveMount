// SPDX-License-Identifier: GPL-3.0-or-later

//! M4-02: the path → file-handle cache against a fake NFS server.
//!
//! The fake keeps a tree of files and directories, each with a numbered handle that turns stale
//! when the file is removed, like knfsd's (spike M4-01 §3). Changes made straight on the fake are
//! another client's; the tests tell the cache about this mount's own.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drivemount_nfs::{FileHandle, HandleCache, HandleError, Lookup};
use drivemount_vfs::{FsError, RemotePath};
use tokio::sync::{Notify, Semaphore};

const TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

struct Node {
    kind: Kind,
    children: BTreeMap<String, u64>,
}

struct Tree {
    nodes: HashMap<u64, Node>,
    next_id: u64,
    /// Part of every handle. Bumping it makes every handle stale, the root's too, as when an
    /// export is replaced by another file system.
    epoch: u32,
}

/// A fake NFS server: LOOKUP and GETATTR by handle, and changes by path.
struct FakeServer {
    tree: Mutex<Tree>,
    lookups: AtomicUsize,
    /// LOOKUPs of these names fail with this error.
    refuse: Mutex<HashMap<String, FsError>>,
}

const ROOT: u64 = 1;

impl FakeServer {
    fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node {
                kind: Kind::Dir,
                children: BTreeMap::new(),
            },
        );
        Self {
            tree: Mutex::new(Tree {
                nodes,
                next_id: ROOT + 1,
                epoch: 0,
            }),
            lookups: AtomicUsize::new(0),
            refuse: Mutex::new(HashMap::new()),
        }
    }

    fn handle_of(id: u64, epoch: u32) -> FileHandle {
        let mut bytes = id.to_be_bytes().to_vec();
        bytes.extend_from_slice(&epoch.to_be_bytes());
        FileHandle::new(&bytes)
    }

    fn root(&self) -> FileHandle {
        Self::handle_of(ROOT, self.tree.lock().unwrap().epoch)
    }

    /// The node a handle names, or `Stale`.
    fn decode(tree: &Tree, handle: &FileHandle) -> Result<u64, HandleError> {
        let bytes = handle.as_bytes();
        let id = u64::from_be_bytes(bytes[..8].try_into().unwrap());
        let epoch = u32::from_be_bytes(bytes[8..].try_into().unwrap());
        if epoch != tree.epoch || !tree.nodes.contains_key(&id) {
            return Err(HandleError::Stale);
        }
        Ok(id)
    }

    /// GETATTR, reduced to the kind of file.
    fn getattr(&self, handle: &FileHandle) -> Result<Kind, HandleError> {
        let tree = self.tree.lock().unwrap();
        let id = Self::decode(&tree, handle)?;
        Ok(tree.nodes[&id].kind)
    }

    fn lookups(&self) -> usize {
        self.lookups.swap(0, Ordering::SeqCst)
    }

    /// The node at a path, walking from the root, for the tests' own checks.
    fn id_at(tree: &Tree, path: &str) -> Option<u64> {
        let mut id = ROOT;
        for name in path.split('/').filter(|name| !name.is_empty()) {
            id = *tree.nodes.get(&id)?.children.get(name)?;
        }
        Some(id)
    }

    /// The current handle of `path`, as a fresh LOOKUP chain would give it.
    fn handle_at(&self, path: &str) -> FileHandle {
        let tree = self.tree.lock().unwrap();
        Self::handle_of(Self::id_at(&tree, path).unwrap(), tree.epoch)
    }

    fn make(&self, path: &str, kind: Kind) -> FileHandle {
        let mut tree = self.tree.lock().unwrap();
        let (parent, name) = path.rsplit_once('/').unwrap();
        let parent_id = Self::id_at(&tree, parent).unwrap();
        let id = tree.next_id;
        tree.next_id += 1;
        tree.nodes.insert(
            id,
            Node {
                kind,
                children: BTreeMap::new(),
            },
        );
        let previous = tree
            .nodes
            .get_mut(&parent_id)
            .unwrap()
            .children
            .insert(name.to_owned(), id);
        assert!(previous.is_none(), "{path} exists");
        Self::handle_of(id, tree.epoch)
    }

    fn mkdir(&self, path: &str) -> FileHandle {
        self.make(path, Kind::Dir)
    }

    fn create(&self, path: &str) -> FileHandle {
        self.make(path, Kind::File)
    }

    /// Removes `path` and everything below it; their handles turn stale.
    fn remove(&self, path: &str) {
        let mut tree = self.tree.lock().unwrap();
        let (parent, name) = path.rsplit_once('/').unwrap();
        let parent_id = Self::id_at(&tree, parent).unwrap();
        let id = tree
            .nodes
            .get_mut(&parent_id)
            .unwrap()
            .children
            .remove(name)
            .unwrap();
        let mut gone = vec![id];
        while let Some(id) = gone.pop() {
            let node = tree.nodes.remove(&id).unwrap();
            gone.extend(node.children.into_values());
        }
    }

    /// Renames `from` to `to`, replacing what was there. Handles of `from` and below stay valid.
    fn rename(&self, from: &str, to: &str) {
        if Self::id_at(&self.tree.lock().unwrap(), to).is_some() {
            self.remove(to);
        }
        let mut tree = self.tree.lock().unwrap();
        let (from_parent, from_name) = from.rsplit_once('/').unwrap();
        let (to_parent, to_name) = to.rsplit_once('/').unwrap();
        let from_parent = Self::id_at(&tree, from_parent).unwrap();
        let id = tree
            .nodes
            .get_mut(&from_parent)
            .unwrap()
            .children
            .remove(from_name)
            .unwrap();
        let to_parent = Self::id_at(&tree, to_parent).unwrap();
        tree.nodes
            .get_mut(&to_parent)
            .unwrap()
            .children
            .insert(to_name.to_owned(), id);
    }

    /// Every handle turns stale, the root's too.
    fn new_epoch(&self) {
        self.tree.lock().unwrap().epoch += 1;
    }
}

impl Lookup for FakeServer {
    async fn lookup(&self, dir: &FileHandle, name: &str) -> Result<FileHandle, HandleError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.refuse.lock().unwrap().get(name) {
            return Err(HandleError::Fs(error.clone()));
        }
        let tree = self.tree.lock().unwrap();
        let id = FakeServer::decode(&tree, dir)?;
        let node = &tree.nodes[&id];
        if node.kind != Kind::Dir {
            // NFS3ERR_NOTDIR
            return Err(HandleError::Fs(FsError::PathNotFound));
        }
        let child = node.children.get(name).ok_or(FsError::NotFound)?;
        Ok(FakeServer::handle_of(*child, tree.epoch))
    }
}

fn path(path: &str) -> RemotePath {
    RemotePath::new(path).unwrap()
}

/// A server with `/a/b/c/file`, `/a/other`, and a cache for it.
fn setup() -> (FakeServer, HandleCache) {
    let server = FakeServer::new();
    server.mkdir("/a");
    server.mkdir("/a/b");
    server.mkdir("/a/b/c");
    server.create("/a/b/c/file");
    server.create("/a/other");
    let cache = HandleCache::new(server.root(), TTL);
    (server, cache)
}

#[tokio::test]
async fn a_path_is_looked_up_once_per_component_and_then_comes_from_the_cache() {
    let (server, cache) = setup();
    let file = cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    assert_eq!(file, server.handle_at("/a/b/c/file"));
    assert_eq!(server.lookups(), 4);
    assert_eq!(cache.len(), 4, "every directory on the way is kept");

    let again = cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    assert_eq!(again, file);
    assert_eq!(server.lookups(), 0);
    assert_eq!(cache.cached(&path("/a/b")), Some(server.handle_at("/a/b")));
}

#[tokio::test]
async fn a_new_name_in_a_known_directory_costs_one_lookup() {
    let (server, cache) = setup();
    cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    server.lookups();
    server.create("/a/b/c/second");
    let second = cache
        .resolve(&server, &path("/a/b/c/second"))
        .await
        .unwrap();
    assert_eq!(second, server.handle_at("/a/b/c/second"));
    assert_eq!(server.lookups(), 1);
}

#[tokio::test]
async fn the_root_needs_no_lookup() {
    let (server, cache) = setup();
    assert_eq!(
        cache.resolve(&server, &RemotePath::root()).await.unwrap(),
        server.root()
    );
    assert_eq!(cache.root(), &server.root());
    assert_eq!(server.lookups(), 0);
    assert!(cache.is_empty());
}

#[tokio::test]
async fn a_missing_name_is_not_found_and_a_missing_or_file_parent_is_path_not_found() {
    let (server, cache) = setup();
    assert_eq!(
        cache.resolve(&server, &path("/a/b/missing")).await,
        Err(FsError::NotFound)
    );
    assert_eq!(
        cache.resolve(&server, &path("/a/missing/file")).await,
        Err(FsError::PathNotFound)
    );
    assert_eq!(
        cache.resolve(&server, &path("/a/other/file")).await,
        Err(FsError::PathNotFound)
    );
    // What was found on the way is kept; a failed walk stores nothing, which costs only a lookup.
    assert!(cache.cached(&path("/a/b/missing")).is_none());
}

#[tokio::test]
async fn other_lookup_errors_come_through_as_they_are() {
    let (server, cache) = setup();
    server
        .refuse
        .lock()
        .unwrap()
        .insert("b".to_owned(), FsError::PermissionDenied);
    assert_eq!(
        cache.resolve(&server, &path("/a/b/c")).await,
        Err(FsError::PermissionDenied)
    );
}

#[tokio::test]
async fn forgetting_a_directory_forgets_what_is_below_it_and_nothing_else() {
    let (server, cache) = setup();
    server.create("/ab");
    cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    cache.resolve(&server, &path("/a/other")).await.unwrap();
    cache.resolve(&server, &path("/ab")).await.unwrap();
    server.lookups();

    server.remove("/a/b");
    cache.forget(&path("/a/b"));
    for gone in ["/a/b", "/a/b/c", "/a/b/c/file"] {
        assert!(cache.cached(&path(gone)).is_none(), "{gone}");
    }
    for kept in ["/a", "/a/other", "/ab"] {
        assert!(cache.cached(&path(kept)).is_some(), "{kept}");
    }
    assert_eq!(
        cache.resolve(&server, &path("/a/b/c/file")).await,
        Err(FsError::PathNotFound)
    );
    assert_eq!(server.lookups(), 1, "only `b` in the cached `/a`");
}

#[tokio::test]
async fn a_rename_moves_the_cached_subtree_with_its_handles() {
    let (server, cache) = setup();
    let file = cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    server.lookups();

    server.rename("/a/b", "/a/moved");
    cache.renamed(&path("/a/b"), &path("/a/moved"));
    assert!(cache.cached(&path("/a/b")).is_none());
    assert!(cache.cached(&path("/a/b/c/file")).is_none());
    let moved = cache
        .resolve(&server, &path("/a/moved/c/file"))
        .await
        .unwrap();
    assert_eq!(moved, file, "a rename keeps the handle");
    assert_eq!(server.lookups(), 0);
    assert_eq!(server.getattr(&moved), Ok(Kind::File));
}

#[tokio::test]
async fn a_rename_onto_a_path_forgets_what_was_there() {
    let (server, cache) = setup();
    server.mkdir("/x");
    server.create("/x/inside");
    cache.resolve(&server, &path("/x/inside")).await.unwrap();
    let file = cache.resolve(&server, &path("/a/other")).await.unwrap();

    server.remove("/x");
    server.rename("/a/other", "/x");
    cache.renamed(&path("/a/other"), &path("/x"));
    assert_eq!(cache.cached(&path("/x")), Some(file));
    assert!(cache.cached(&path("/x/inside")).is_none());
    assert!(cache.cached(&path("/a/other")).is_none());
}

#[tokio::test]
async fn a_stale_handle_is_looked_up_again_and_the_call_repeated() {
    let (server, cache) = setup();
    let old = cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    // Another client replaces the file: same path, new handle.
    server.remove("/a/b/c/file");
    let new = server.create("/a/b/c/file");
    server.lookups();

    let mut seen = Vec::new();
    let kind = cache
        .with_handle(&server, &path("/a/b/c/file"), |handle| {
            seen.push(handle.clone());
            let result = server.getattr(&handle);
            async move { result }
        })
        .await;
    assert_eq!(kind, Ok(Kind::File));
    assert_eq!(seen, [old, new.clone()]);
    assert_eq!(
        server.lookups(),
        1,
        "only the file; its directory is still good"
    );
    assert_eq!(cache.cached(&path("/a/b/c/file")), Some(new));
}

#[tokio::test]
async fn a_stale_directory_on_the_way_is_dropped_and_the_walk_starts_above_it() {
    let (server, cache) = setup();
    cache.resolve(&server, &path("/a/b/c")).await.unwrap();
    // Another client replaces `/a/b` with a new tree.
    server.remove("/a/b");
    server.mkdir("/a/b");
    server.mkdir("/a/b/c");
    let file = server.create("/a/b/c/file");
    server.lookups();

    let resolved = cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    assert_eq!(resolved, file);
    // `file` in the stale `c`, then `c` in the stale cached `b`, then `b`, `c`, `file` from the
    // cached `/a`: each stale directory is dropped as it is met.
    assert_eq!(server.lookups(), 5);
    assert_eq!(cache.cached(&path("/a/b")), Some(server.handle_at("/a/b")));
}

#[tokio::test]
async fn a_file_removed_by_another_client_is_not_found() {
    let (server, cache) = setup();
    cache.resolve(&server, &path("/a/b/c/file")).await.unwrap();
    server.remove("/a/b/c/file");
    let result = cache
        .with_handle(&server, &path("/a/b/c/file"), |handle| {
            let result = server.getattr(&handle);
            async move { result }
        })
        .await;
    assert_eq!(result, Err(FsError::NotFound));
    assert!(cache.cached(&path("/a/b/c/file")).is_none());
}

#[tokio::test]
async fn a_handle_stale_again_after_a_fresh_lookup_is_not_found() {
    let (server, cache) = setup();
    let calls = AtomicUsize::new(0);
    let result: Result<(), FsError> = cache
        .with_handle(&server, &path("/a/other"), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(HandleError::Stale) }
        })
        .await;
    assert_eq!(result, Err(FsError::NotFound));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "tried once more, not forever"
    );
}

#[tokio::test]
async fn errors_from_the_call_are_not_retried() {
    let (server, cache) = setup();
    let calls = AtomicUsize::new(0);
    let result: Result<(), FsError> = cache
        .with_handle(&server, &path("/a/other"), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(HandleError::Fs(FsError::NoSpace)) }
        })
        .await;
    assert_eq!(result, Err(FsError::NoSpace));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_stale_root_is_an_error_naming_it() {
    let (server, cache) = setup();
    cache.resolve(&server, &path("/a/b")).await.unwrap();
    server.new_epoch();
    let error = cache.resolve(&server, &path("/a/other")).await.unwrap_err();
    assert_eq!(error, drivemount_nfs::root_stale());
    let error = cache
        .with_handle(&server, &RemotePath::root(), |handle| {
            let result = server.getattr(&handle);
            async move { result }
        })
        .await
        .unwrap_err();
    assert_eq!(error, drivemount_nfs::root_stale());
}

#[tokio::test(start_paused = true)]
async fn another_clients_rename_is_seen_when_the_ttl_runs_out() {
    let (server, cache) = setup();
    let old = cache.resolve(&server, &path("/a/other")).await.unwrap();
    server.rename("/a/other", "/a/elsewhere");
    let new = server.create("/a/other");

    tokio::time::advance(TTL - Duration::from_secs(1)).await;
    assert_eq!(
        cache.resolve(&server, &path("/a/other")).await.unwrap(),
        old
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    server.lookups();
    assert_eq!(
        cache.resolve(&server, &path("/a/other")).await.unwrap(),
        new
    );
    assert_eq!(server.lookups(), 2, "`/a` expired too");
}

#[tokio::test]
async fn a_zero_ttl_looks_everything_up_each_time() {
    let server = FakeServer::new();
    server.mkdir("/a");
    server.create("/a/f");
    let cache = HandleCache::new(server.root(), Duration::ZERO);
    for _ in 0..3 {
        cache.resolve(&server, &path("/a/f")).await.unwrap();
        assert_eq!(server.lookups(), 2);
    }
}

#[tokio::test]
async fn the_cache_keeps_to_its_capacity_and_the_paths_used_last() {
    let server = FakeServer::new();
    server.mkdir("/d");
    let cache = HandleCache::with_capacity(server.root(), TTL, 50);
    for i in 0..200 {
        server.create(&format!("/d/f{i}"));
    }
    for i in 0..200 {
        let name = format!("/d/f{i}");
        assert_eq!(
            cache.resolve(&server, &path(&name)).await.unwrap(),
            server.handle_at(&name)
        );
        // `/d` is used by every walk, so it stays.
        assert!(cache.len() <= 50);
    }
    assert!(cache.cached(&path("/d")).is_some());
    assert!(cache.cached(&path("/d/f199")).is_some());
    assert!(cache.cached(&path("/d/f0")).is_none());
}

#[tokio::test]
async fn insert_records_what_create_returned() {
    let (server, cache) = setup();
    cache.resolve(&server, &path("/a/b/c")).await.unwrap();
    let made = server.create("/a/b/c/new");
    cache.insert(&path("/a/b/c/new"), made.clone());
    server.lookups();
    assert_eq!(
        cache.resolve(&server, &path("/a/b/c/new")).await.unwrap(),
        made
    );
    assert_eq!(server.lookups(), 0);
}

/// A server whose LOOKUPs wait until let through, to hold a resolve in flight.
struct Gated {
    server: Arc<FakeServer>,
    started: Notify,
    gate: Semaphore,
}

impl Lookup for Gated {
    async fn lookup(&self, dir: &FileHandle, name: &str) -> Result<FileHandle, HandleError> {
        self.started.notify_one();
        self.gate.acquire().await.unwrap().forget();
        self.server.lookup(dir, name).await
    }
}

#[tokio::test]
async fn a_resolve_in_flight_during_a_forget_does_not_store_what_it_found() {
    let (server, _) = setup();
    let server = Arc::new(server);
    let cache = Arc::new(HandleCache::new(server.root(), TTL));
    let gated = Arc::new(Gated {
        server: Arc::clone(&server),
        started: Notify::new(),
        gate: Semaphore::new(0),
    });

    let task = tokio::spawn({
        let cache = Arc::clone(&cache);
        let gated = Arc::clone(&gated);
        async move { cache.resolve(&*gated, &path("/a/b")).await }
    });
    gated.started.notified().await;
    // This mount removes `/a/b` while the lookup is on its way.
    cache.forget(&path("/a/b"));
    gated.gate.add_permits(2);
    task.await.unwrap().unwrap();

    assert!(cache.cached(&path("/a/b")).is_none());
    assert!(cache.cached(&path("/a")).is_none());
}

#[tokio::test]
async fn concurrent_resolves_agree() {
    let (server, cache) = setup();
    let server = Arc::new(server);
    let cache = Arc::new(cache);
    let mut tasks = Vec::new();
    for i in 0..16 {
        let server = Arc::clone(&server);
        let cache = Arc::clone(&cache);
        tasks.push(tokio::spawn(async move {
            let name = if i % 2 == 0 {
                "/a/b/c/file"
            } else {
                "/a/other"
            };
            cache.resolve(&*server, &path(name)).await.unwrap()
        }));
    }
    for (i, task) in tasks.into_iter().enumerate() {
        let name = if i % 2 == 0 {
            "/a/b/c/file"
        } else {
            "/a/other"
        };
        assert_eq!(task.await.unwrap(), server.handle_at(name));
    }
}
