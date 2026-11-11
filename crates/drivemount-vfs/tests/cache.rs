// SPDX-License-Identifier: GPL-3.0-or-later

//! `CachedFs` on top of `MemFs`: what it keeps and for how long, each invalidation rule of spec §5.6,
//! and NFR-REL-02. The clock is paused, so TTLs are exact and the tests take no time.

// clippy.toml exempts only code inside #[test] functions, not the helpers.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, CachedFs, Capabilities, DirEntry, Disposition, FsError,
    FsResult, FsStats, MemFs, OpenMode, RemoteFs, RemoteHandle, RemotePath, Resolved, async_trait,
    contract, resolve_name,
};
use proptest::prelude::*;
use tokio::sync::Notify;
use tokio::time::advance;

const TTL: Duration = Duration::from_secs(5);
const MS: Duration = Duration::from_millis(1);

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

/// `MemFs` as the server, with a window onto what reaches it. Clones share the server, so a test can
/// keep one and change files behind the cache's back, the way another client would.
#[derive(Clone, Default)]
struct Server {
    fs: Arc<MemFs>,
    calls: Arc<Mutex<HashMap<&'static str, usize>>>,
    hold: Arc<Mutex<Option<Hold>>>,
    /// Calls that change something do so, and then report a timeout.
    lose_replies: Arc<AtomicBool>,
    stat_error: Arc<Mutex<Option<FsError>>>,
}

/// Stops one kind of call after the server has acted on it, until the test says go on.
#[derive(Clone)]
struct Hold {
    op: &'static str,
    reached: Arc<Notify>,
    go_on: Arc<Notify>,
}

impl Server {
    fn calls(&self, op: &'static str) -> usize {
        self.calls.lock().unwrap().get(op).copied().unwrap_or(0)
    }

    fn hold(&self, op: &'static str) -> Hold {
        let hold = Hold {
            op,
            reached: Arc::new(Notify::new()),
            go_on: Arc::new(Notify::new()),
        };
        *self.hold.lock().unwrap() = Some(hold.clone());
        hold
    }

    /// Counts the call and applies any `Hold`.
    async fn after(&self, op: &'static str) {
        *self.calls.lock().unwrap().entry(op).or_default() += 1;
        let hold = self.hold.lock().unwrap().clone();
        if let Some(hold) = hold.filter(|hold| hold.op == op) {
            *self.hold.lock().unwrap() = None;
            hold.reached.notify_one();
            hold.go_on.notified().await;
        }
    }

    /// The result of a call that changes something, after `after`.
    fn reply<T>(&self, result: FsResult<T>) -> FsResult<T> {
        if self.lose_replies.load(Ordering::SeqCst) {
            Err(FsError::Timeout)
        } else {
            result
        }
    }
}

#[async_trait]
impl RemoteFs for Server {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        let result = self.fs.stat(path).await;
        self.after("stat").await;
        match self.stat_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => result,
        }
    }
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        let result = self.fs.lstat(path).await;
        self.after("lstat").await;
        result
    }
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.fs.fstat(handle).await
    }
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        let result = self.fs.read_dir(path).await;
        self.after("read_dir").await;
        result
    }
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        // Not `reply`: losing this reply would leak the handle, and the rule is covered by the others.
        let result = self.fs.open(path, mode).await;
        self.after("open").await;
        result
    }
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.fs.read(handle, offset, len).await
    }
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let result = self.fs.write(handle, offset, data).await;
        self.after("write").await;
        self.reply(result)
    }
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.fs.flush(handle).await
    }
    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.fs.close(handle).await
    }
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        let result = self.fs.set_attr(target, changes).await;
        self.after("set_attr").await;
        self.reply(result)
    }
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let result = self.fs.mkdir(path).await;
        self.after("mkdir").await;
        self.reply(result)
    }
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let result = self.fs.remove_file(path).await;
        self.after("remove_file").await;
        self.reply(result)
    }
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let result = self.fs.remove_dir(path).await;
        self.after("remove_dir").await;
        self.reply(result)
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        let result = self.fs.rename(from, to, replace).await;
        self.after("rename").await;
        self.reply(result)
    }
    async fn statfs(&self) -> FsResult<FsStats> {
        self.fs.statfs().await
    }
    fn capabilities(&self) -> Capabilities {
        self.fs.capabilities()
    }
}

fn cached(ttl: Duration) -> (Arc<CachedFs<Server>>, Server) {
    let server = Server::default();
    (Arc::new(CachedFs::new(server.clone(), ttl)), server)
}

fn path(path: &str) -> RemotePath {
    RemotePath::new(path).unwrap()
}

async fn create(fs: &impl RemoteFs, at: &str, contents: &[u8]) {
    let handle = fs
        .open(&path(at), OpenMode::write(Disposition::CreateOrTruncate))
        .await
        .unwrap();
    if !contents.is_empty() {
        fs.write(&handle, 0, Bytes::copy_from_slice(contents))
            .await
            .unwrap();
    }
    fs.close(handle).await.unwrap();
}

async fn mkdir(fs: &impl RemoteFs, at: &str) {
    fs.mkdir(&path(at)).await.unwrap();
}

async fn names(fs: &impl RemoteFs, dir: &str) -> Vec<String> {
    let mut names: Vec<_> = fs
        .read_dir(&path(dir))
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    names
}

async fn size(fs: &impl RemoteFs, at: &str) -> FsResult<u64> {
    fs.stat(&path(at)).await.map(|attr| attr.size)
}

/// Fills all three caches around `/d/f` (5 bytes) and `/d/missing`, leaving the server alone after.
/// Everything is then served from memory until something invalidates it.
async fn primed() -> (Arc<CachedFs<Server>>, Server) {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    create(&server, "/d/f", b"12345").await;
    size(&*fs, "/d").await.unwrap();
    size(&*fs, "/d/f").await.unwrap();
    assert_eq!(size(&*fs, "/d/missing").await, Err(FsError::NotFound));
    names(&*fs, "/d").await;
    names(&*fs, "/").await;
    (fs, server)
}

/// Whether asking for the attributes of `at` reaches the server.
async fn stat_reaches_server(fs: &impl RemoteFs, server: &Server, at: &str) -> bool {
    let before = server.calls("stat");
    let _ = fs.stat(&path(at)).await;
    server.calls("stat") != before
}

/// Whether listing `at` reaches the server.
async fn listing_reaches_server(fs: &impl RemoteFs, server: &Server, at: &str) -> bool {
    let before = server.calls("read_dir");
    fs.read_dir(&path(at)).await.unwrap();
    server.calls("read_dir") != before
}

// -- What is kept, and for how long ------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn attributes_are_served_from_memory_until_the_profile_ttl_runs_out() {
    for ttl in [1, 5, 60].map(Duration::from_secs) {
        let (fs, server) = cached(ttl);
        create(&server, "/f", b"12").await;
        assert_eq!(size(&*fs, "/f").await, Ok(2));

        create(&server, "/f", b"1234").await;
        let calls = server.calls("stat");
        advance(ttl - MS).await;
        assert_eq!(size(&*fs, "/f").await, Ok(2), "still fresh");
        assert_eq!(server.calls("stat"), calls);

        advance(MS).await;
        assert_eq!(size(&*fs, "/f").await, Ok(4), "expired at {ttl:?}");
        assert_eq!(server.calls("stat"), calls + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn listings_are_served_from_memory_until_the_profile_ttl_runs_out() {
    for ttl in [1, 5, 60].map(Duration::from_secs) {
        let (fs, server) = cached(ttl);
        create(&server, "/a", b"").await;
        assert_eq!(names(&*fs, "/").await, ["a"]);

        create(&server, "/b", b"").await;
        let calls = server.calls("read_dir");
        advance(ttl - MS).await;
        assert_eq!(names(&*fs, "/").await, ["a"], "still fresh");
        assert_eq!(server.calls("read_dir"), calls);

        advance(MS).await;
        assert_eq!(names(&*fs, "/").await, ["a", "b"], "expired at {ttl:?}");
        assert_eq!(server.calls("read_dir"), calls + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_zero_ttl_turns_attributes_and_listings_off() {
    let (fs, server) = cached(Duration::ZERO);
    create(&server, "/f", b"").await;
    for _ in 0..2 {
        assert!(listing_reaches_server(&*fs, &server, "/").await);
        assert!(stat_reaches_server(&*fs, &server, "/f").await);
    }
}

#[tokio::test(start_paused = true)]
async fn freshness_counts_from_when_the_question_was_asked() {
    let (fs, server) = cached(TTL);
    create(&server, "/f", b"").await;
    let hold = server.hold("stat");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/f")).await })
    };
    hold.reached.notified().await;
    advance(TTL - MS).await;
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap();

    assert!(!stat_reaches_server(&*fs, &server, "/f").await);
    advance(MS).await;
    assert!(stat_reaches_server(&*fs, &server, "/f").await);
}

#[tokio::test(start_paused = true)]
async fn the_age_of_a_listing_counts_from_when_the_question_was_asked() {
    let (fs, server) = cached(TTL);
    create(&server, "/f", b"").await;
    let hold = server.hold("read_dir");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.read_dir(&RemotePath::root()).await })
    };
    hold.reached.notified().await;
    advance(TTL - MS).await;
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap();

    assert!(!listing_reaches_server(&*fs, &server, "/").await);
    assert!(!stat_reaches_server(&*fs, &server, "/f").await);
    advance(MS).await;
    assert!(listing_reaches_server(&*fs, &server, "/").await);
}

#[tokio::test(start_paused = true)]
async fn the_age_of_a_negative_lookup_counts_from_when_the_question_was_asked() {
    let (fs, server) = cached(TTL);
    let hold = server.hold("stat");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/f")).await })
    };
    hold.reached.notified().await;
    advance(Duration::from_secs(1) - MS).await;
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap_err();

    assert!(!stat_reaches_server(&*fs, &server, "/f").await);
    advance(MS).await;
    assert!(stat_reaches_server(&*fs, &server, "/f").await);
}

/// Two lookups overlap, and the file was there for the one whose answer arrives last. Once that
/// answer expires the name must be asked about again, not reported missing from the other answer.
#[tokio::test(start_paused = true)]
async fn a_late_positive_answer_ends_a_negative_lookup() {
    let (fs, server) = cached(TTL);
    create(&server, "/desktop.ini", b"").await;
    let hold = server.hold("stat");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/desktop.ini")).await })
    };
    hold.reached.notified().await;
    server.remove_file(&path("/desktop.ini")).await.unwrap();
    size(&*fs, "/desktop.ini").await.unwrap_err();
    create(&server, "/desktop.ini", b"").await;
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap();

    advance(TTL).await;
    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(0));
}

/// The reverse order: an older "missing" must not hide a newer answer that found the file.
#[tokio::test(start_paused = true)]
async fn a_late_negative_answer_does_not_hide_a_positive_one() {
    let (fs, server) = cached(TTL);
    let hold = server.hold("stat");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/f")).await })
    };
    hold.reached.notified().await;
    create(&server, "/f", b"12").await;
    assert_eq!(size(&*fs, "/f").await, Ok(2));
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap_err();

    assert_eq!(size(&*fs, "/f").await, Ok(2));
}

#[tokio::test(start_paused = true)]
async fn an_answer_slower_than_the_ttl_is_returned_but_not_kept() {
    let (fs, server) = cached(TTL);
    create(&server, "/f", b"").await;
    let hold = server.hold("stat");
    let asking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/f")).await })
    };
    hold.reached.notified().await;
    advance(TTL).await;
    hold.go_on.notify_one();
    asking.await.unwrap().unwrap();

    assert!(stat_reaches_server(&*fs, &server, "/f").await);
}

#[tokio::test(start_paused = true)]
async fn a_listing_fills_the_attributes_of_its_entries_with_its_own_ttl() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    create(&server, "/d/f", b"123").await;
    mkdir(&server, "/d/sub").await;
    names(&*fs, "/d").await;

    assert_eq!(size(&*fs, "/d/f").await, Ok(3));
    assert!(fs.stat(&path("/d/sub")).await.unwrap().is_dir());
    assert_eq!(server.calls("stat"), 0);

    advance(TTL).await;
    assert!(stat_reaches_server(&*fs, &server, "/d/f").await);
}

#[tokio::test(start_paused = true)]
async fn lstat_always_reaches_the_server() {
    let (fs, server) = cached(TTL);
    create(&server, "/f", b"").await;
    size(&*fs, "/f").await.unwrap();
    fs.lstat(&path("/f")).await.unwrap();
    fs.lstat(&path("/f")).await.unwrap();
    assert_eq!(server.calls("lstat"), 2);
}

// -- Negative lookups --------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn ordinary_names_stay_missing_for_one_second() {
    let (fs, server) = cached(TTL);
    assert_eq!(size(&*fs, "/f").await, Err(FsError::NotFound));
    assert_eq!(server.calls("stat"), 1);

    create(&server, "/f", b"").await;
    advance(Duration::from_secs(1) - MS).await;
    assert_eq!(size(&*fs, "/f").await, Err(FsError::NotFound));
    assert_eq!(server.calls("stat"), 1);

    advance(MS).await;
    assert_eq!(size(&*fs, "/f").await, Ok(0));
    assert_eq!(server.calls("stat"), 2);
}

#[tokio::test(start_paused = true)]
async fn noise_names_stay_missing_for_sixty_seconds_in_any_directory() {
    for name in NOISE_NAMES {
        let (fs, server) = cached(TTL);
        mkdir(&server, "/d").await;
        let at = format!("/d/{name}");

        assert_eq!(size(&*fs, &at).await, Err(FsError::NotFound), "{name}");
        assert_eq!(server.calls("stat"), 1, "{name} is asked for first");

        create(&server, &at, b"").await;
        advance(Duration::from_secs(60) - MS).await;
        assert_eq!(size(&*fs, &at).await, Err(FsError::NotFound), "{name}");
        assert_eq!(server.calls("stat"), 1, "{name}");

        advance(MS).await;
        assert_eq!(size(&*fs, &at).await, Ok(0), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_real_file_with_a_noise_name_works() {
    let (fs, server) = cached(TTL);
    create(&server, "/desktop.ini", b"123").await;
    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(3));
    assert_eq!(names(&*fs, "/").await, ["desktop.ini"]);
}

#[tokio::test(start_paused = true)]
async fn a_noise_name_differing_in_case_is_an_ordinary_name() {
    let (fs, server) = cached(TTL);
    size(&*fs, "/DESKTOP.INI").await.unwrap_err();
    advance(Duration::from_secs(1)).await;
    assert!(stat_reaches_server(&*fs, &server, "/DESKTOP.INI").await);
}

#[tokio::test(start_paused = true)]
async fn negative_lookups_do_not_depend_on_the_profile_ttl() {
    let (fs, server) = cached(Duration::ZERO);
    size(&*fs, "/f").await.unwrap_err();
    assert!(!stat_reaches_server(&*fs, &server, "/f").await);
    advance(Duration::from_secs(1)).await;
    assert!(stat_reaches_server(&*fs, &server, "/f").await);
}

#[tokio::test(start_paused = true)]
async fn the_kind_of_not_found_is_remembered() {
    let (fs, server) = cached(TTL);
    for error in [FsError::NotFound, FsError::PathNotFound] {
        *server.stat_error.lock().unwrap() = Some(error.clone());
        let at = format!("/{error}");
        assert_eq!(size(&*fs, &at).await, Err(error.clone()));
        *server.stat_error.lock().unwrap() = None;
        let calls = server.calls("stat");
        assert_eq!(size(&*fs, &at).await, Err(error));
        assert_eq!(server.calls("stat"), calls);
    }
}

#[tokio::test(start_paused = true)]
async fn other_errors_are_not_remembered() {
    let (fs, server) = cached(TTL);
    create(&server, "/f", b"").await;
    for error in [
        FsError::PermissionDenied,
        FsError::Timeout,
        FsError::ConnectionLost,
    ] {
        *server.stat_error.lock().unwrap() = Some(error.clone());
        assert_eq!(size(&*fs, "/f").await, Err(error));
    }
    *server.stat_error.lock().unwrap() = None;
    assert_eq!(size(&*fs, "/f").await, Ok(0));
}

#[tokio::test(start_paused = true)]
async fn a_listing_showing_a_name_ends_its_negative_lookup() {
    let (fs, server) = cached(TTL);
    size(&*fs, "/desktop.ini").await.unwrap_err();
    create(&server, "/desktop.ini", b"12").await;
    names(&*fs, "/").await;
    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(2));

    // The attributes from the listing run out long before the 60 s would have.
    advance(TTL).await;
    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(2));
}

#[tokio::test(start_paused = true)]
async fn listing_a_directory_ends_its_negative_lookup() {
    let (fs, server) = cached(TTL);
    size(&*fs, "/$RECYCLE.BIN").await.unwrap_err();
    mkdir(&server, "/$RECYCLE.BIN").await;
    names(&*fs, "/$RECYCLE.BIN").await;
    assert_eq!(size(&*fs, "/$RECYCLE.BIN").await, Ok(0));
}

#[tokio::test(start_paused = true)]
async fn opening_a_file_ends_its_negative_lookup() {
    let (fs, server) = cached(TTL);
    size(&*fs, "/desktop.ini").await.unwrap_err();
    create(&server, "/desktop.ini", b"12").await;
    let handle = fs
        .open(&path("/desktop.ini"), OpenMode::read())
        .await
        .unwrap();
    fs.close(handle).await.unwrap();
    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(2));
}

// -- Invalidation by local changes (spec §5.6) -------------------------------------------------

const CREATING: [Disposition; 4] = [
    Disposition::CreateNew,
    Disposition::OpenOrCreate,
    Disposition::TruncateExisting,
    Disposition::CreateOrTruncate,
];

#[tokio::test(start_paused = true)]
async fn create_ends_a_negative_lookup_and_changes_the_listing() {
    for disposition in [
        Disposition::CreateNew,
        Disposition::OpenOrCreate,
        Disposition::CreateOrTruncate,
    ] {
        let (fs, _server) = primed().await;
        let handle = fs
            .open(&path("/d/missing"), OpenMode::write(disposition))
            .await
            .unwrap();
        assert_eq!(size(&*fs, "/d/missing").await, Ok(0), "{disposition:?}");
        assert_eq!(names(&*fs, "/d").await, ["f", "missing"], "{disposition:?}");
        fs.close(handle).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn create_changes_the_attributes_of_the_directory() {
    let (fs, server) = primed().await;
    create(&*fs, "/d/new", b"").await;
    assert!(stat_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn opening_with_truncation_invalidates_the_old_size_and_the_listing() {
    for disposition in [Disposition::TruncateExisting, Disposition::CreateOrTruncate] {
        let (fs, _server) = primed().await;
        let handle = fs
            .open(&path("/d/f"), OpenMode::write(disposition))
            .await
            .unwrap();
        assert_eq!(size(&*fs, "/d/f").await, Ok(0), "{disposition:?}");
        let listed = fs.read_dir(&path("/d")).await.unwrap();
        assert_eq!(listed[0].attr.size, 0, "{disposition:?}");
        fs.close(handle).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn every_creating_disposition_invalidates_even_when_it_fails() {
    // CreateNew on an existing file, and TruncateExisting on a missing one, change nothing, but it
    // is the server that knows. Nothing here may be left stale whichever way they go.
    for disposition in CREATING {
        for at in ["/d/f", "/d/missing"] {
            let (fs, server) = primed().await;
            if let Ok(handle) = fs.open(&path(at), OpenMode::write(disposition)).await {
                fs.close(handle).await.unwrap();
            }
            assert!(
                stat_reaches_server(&*fs, &server, at).await,
                "{disposition:?} {at}"
            );
            assert!(
                listing_reaches_server(&*fs, &server, "/d").await,
                "{disposition:?} {at}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn browsing_keeps_the_caches() {
    let (fs, server) = primed().await;
    let handle = fs.open(&path("/d/f"), OpenMode::read()).await.unwrap();
    fs.read(&handle, 0, 5).await.unwrap();
    fs.fstat(&handle).await.unwrap();
    fs.flush(&handle).await.unwrap();
    fs.close(handle).await.unwrap();
    // Opening for writing changes nothing by itself either: only a write does.
    let handle = fs
        .open(
            &path("/d/f"),
            OpenMode::read_write(Disposition::OpenExisting),
        )
        .await
        .unwrap();
    fs.flush(&handle).await.unwrap();
    fs.close(handle).await.unwrap();

    assert!(!stat_reaches_server(&*fs, &server, "/d/f").await);
    assert!(!stat_reaches_server(&*fs, &server, "/d").await);
    assert!(!listing_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn write_invalidates_the_file_and_the_listing_of_its_directory() {
    let (fs, _server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.write(&handle, 5, Bytes::from_static(b"678"))
        .await
        .unwrap();

    assert_eq!(size(&*fs, "/d/f").await, Ok(8));
    assert_eq!(fs.read_dir(&path("/d")).await.unwrap()[0].attr.size, 8);
    fs.close(handle).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn write_keeps_what_it_does_not_change() {
    let (fs, server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.write(&handle, 0, Bytes::from_static(b"x"))
        .await
        .unwrap();
    fs.close(handle).await.unwrap();

    assert!(!stat_reaches_server(&*fs, &server, "/d").await);
    assert!(!listing_reaches_server(&*fs, &server, "/").await);
    assert_eq!(size(&*fs, "/d/missing").await, Err(FsError::NotFound));
}

/// Servers can update the times as late as flush or close, so these drop what a stat in between
/// stored.
#[tokio::test(start_paused = true)]
async fn flush_and_close_after_a_write_invalidate_again() {
    let (fs, server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.write(&handle, 0, Bytes::from_static(b"x"))
        .await
        .unwrap();

    size(&*fs, "/d/f").await.unwrap();
    names(&*fs, "/d").await;
    fs.flush(&handle).await.unwrap();
    assert!(stat_reaches_server(&*fs, &server, "/d/f").await);
    assert!(listing_reaches_server(&*fs, &server, "/d").await);

    fs.close(handle).await.unwrap();
    assert!(stat_reaches_server(&*fs, &server, "/d/f").await);
    assert!(listing_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn set_attr_by_path_invalidates_the_file_and_the_listing() {
    let (fs, _server) = primed().await;
    let changes = AttrChanges {
        size: Some(2),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&path("/d/f")), changes)
        .await
        .unwrap();
    assert_eq!(size(&*fs, "/d/f").await, Ok(2));
    assert_eq!(fs.read_dir(&path("/d")).await.unwrap()[0].attr.size, 2);
}

#[tokio::test(start_paused = true)]
async fn set_attr_by_handle_invalidates_the_file_and_the_listing() {
    let (fs, _server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    let changes = AttrChanges {
        size: Some(2),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Handle(&handle), changes)
        .await
        .unwrap();
    assert_eq!(size(&*fs, "/d/f").await, Ok(2));
    assert_eq!(fs.read_dir(&path("/d")).await.unwrap()[0].attr.size, 2);
    fs.close(handle).await.unwrap();
}

/// What `set_attr` answers is the file after the change, so the next lookup need not ask.
#[tokio::test(start_paused = true)]
async fn set_attr_keeps_its_answer() {
    let (fs, server) = primed().await;
    let changes = AttrChanges {
        size: Some(2),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&path("/d/f")), changes)
        .await
        .unwrap();
    assert!(!stat_reaches_server(&*fs, &server, "/d/f").await);
    assert_eq!(size(&*fs, "/d/f").await, Ok(2));

    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    let changes = AttrChanges {
        size: Some(1),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Handle(&handle), changes)
        .await
        .unwrap();
    assert!(!stat_reaches_server(&*fs, &server, "/d/f").await);
    assert_eq!(size(&*fs, "/d/f").await, Ok(1));
    fs.close(handle).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn fstat_answers_for_the_path_of_its_handle() {
    let (fs, server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.write(&handle, 5, Bytes::from_static(b"678"))
        .await
        .unwrap();
    assert_eq!(fs.fstat(&handle).await.unwrap().size, 8);
    assert!(!stat_reaches_server(&*fs, &server, "/d/f").await);
    assert_eq!(size(&*fs, "/d/f").await, Ok(8));

    // Closing a written file drops it again, as servers can change the times then.
    fs.close(handle).await.unwrap();
    assert!(stat_reaches_server(&*fs, &server, "/d/f").await);
}

/// An open file outlives its name: after the name is removed or replaced, the handle's answers are
/// about a file the path no longer names.
#[tokio::test(start_paused = true)]
async fn a_handle_whose_name_went_does_not_answer_for_the_path() {
    for replace in [false, true] {
        let (fs, _server) = primed().await;
        let handle = fs.open(&path("/d/f"), OpenMode::read()).await.unwrap();
        if replace {
            create(&*fs, "/d/other", b"12").await;
            fs.rename(&path("/d/other"), &path("/d/f"), true)
                .await
                .unwrap();
        } else {
            fs.remove_file(&path("/d/f")).await.unwrap();
            create(&*fs, "/d/f", b"12").await;
        }
        assert_eq!(
            fs.fstat(&handle).await.unwrap().size,
            5,
            "replace {replace}"
        );
        assert_eq!(size(&*fs, "/d/f").await, Ok(2), "replace {replace}");
        fs.close(handle).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_set_attr_overtaken_by_a_write_is_not_kept() {
    let (fs, server) = primed().await;
    let writer = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    let hold = server.hold("set_attr");
    let setting = {
        let fs = fs.clone();
        tokio::spawn(async move {
            let changes = AttrChanges {
                size: Some(2),
                ..AttrChanges::default()
            };
            fs.set_attr(AttrTarget::Path(&path("/d/f")), changes)
                .await
                .unwrap()
        })
    };
    hold.reached.notified().await;
    fs.write(&writer, 0, Bytes::from_static(b"1234567"))
        .await
        .unwrap();
    hold.go_on.notify_one();
    assert_eq!(setting.await.unwrap().size, 2);
    assert_eq!(size(&*fs, "/d/f").await, Ok(7));
    fs.close(writer).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_read_only_attribute_shows_at_once() {
    let (fs, _server) = primed().await;
    let changes = AttrChanges {
        read_only: Some(true),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&path("/d/f")), changes)
        .await
        .unwrap();
    assert!(fs.stat(&path("/d/f")).await.unwrap().is_read_only());
}

#[tokio::test(start_paused = true)]
async fn mkdir_ends_a_negative_lookup_and_changes_the_listing() {
    let (fs, server) = primed().await;
    mkdir(&*fs, "/d/missing").await;
    assert!(fs.stat(&path("/d/missing")).await.unwrap().is_dir());
    assert_eq!(names(&*fs, "/d").await, ["f", "missing"]);
    assert!(stat_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn mkdir_ends_negative_lookups_below_the_new_directory() {
    let (fs, _server) = cached(TTL);
    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::PathNotFound));
    mkdir(&*fs, "/d").await;
    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::NotFound));
}

#[tokio::test(start_paused = true)]
async fn remove_file_invalidates_the_file_and_the_listing() {
    let (fs, server) = primed().await;
    fs.remove_file(&path("/d/f")).await.unwrap();
    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::NotFound));
    assert!(names(&*fs, "/d").await.is_empty());
    assert!(stat_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn remove_dir_invalidates_the_directory_and_what_was_known_below_it() {
    let (fs, server) = primed().await;
    server.remove_file(&path("/d/f")).await.unwrap();
    fs.remove_dir(&path("/d")).await.unwrap();

    assert_eq!(size(&*fs, "/d").await, Err(FsError::NotFound));
    assert!(
        fs.stat(&path("/d/f")).await.is_err(),
        "the child's attributes"
    );
    assert!(fs.read_dir(&path("/d")).await.is_err(), "its listing");
    assert!(names(&*fs, "/").await.is_empty(), "the parent's listing");
}

#[tokio::test(start_paused = true)]
async fn rename_invalidates_the_source_and_the_destination() {
    let (fs, server) = primed().await;
    fs.rename(&path("/d/f"), &path("/d/missing"), false)
        .await
        .unwrap();

    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::NotFound));
    assert_eq!(size(&*fs, "/d/missing").await, Ok(5), "was known missing");
    assert_eq!(names(&*fs, "/d").await, ["missing"]);
    assert!(stat_reaches_server(&*fs, &server, "/d").await);
}

#[tokio::test(start_paused = true)]
async fn rename_replacing_a_file_invalidates_the_destination() {
    let (fs, server) = primed().await;
    create(&server, "/d/other", b"1").await;
    assert_eq!(size(&*fs, "/d/other").await, Ok(1));

    fs.rename(&path("/d/f"), &path("/d/other"), true)
        .await
        .unwrap();

    assert_eq!(size(&*fs, "/d/other").await, Ok(5));
    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::NotFound));
}

#[tokio::test(start_paused = true)]
async fn rename_across_directories_changes_both_listings() {
    let (fs, server) = primed().await;
    mkdir(&server, "/e").await;
    assert!(names(&*fs, "/e").await.is_empty());
    size(&*fs, "/e").await.unwrap();

    fs.rename(&path("/d/f"), &path("/e/f"), false)
        .await
        .unwrap();

    assert!(names(&*fs, "/d").await.is_empty());
    assert_eq!(names(&*fs, "/e").await, ["f"]);
    assert!(stat_reaches_server(&*fs, &server, "/d").await);
    assert!(stat_reaches_server(&*fs, &server, "/e").await);
}

#[tokio::test(start_paused = true)]
async fn renaming_a_directory_invalidates_everything_below_both_paths() {
    let (fs, server) = primed().await;
    mkdir(&server, "/e").await;
    create(&server, "/d/only_in_d", b"").await;
    size(&*fs, "/e/only_in_d").await.unwrap_err();
    size(&*fs, "/d/only_in_d").await.unwrap();
    names(&*fs, "/e").await;
    server.remove_dir(&path("/e")).await.unwrap();

    fs.rename(&path("/d"), &path("/e"), false).await.unwrap();

    assert_eq!(size(&*fs, "/e/f").await, Ok(5));
    assert_eq!(size(&*fs, "/e/only_in_d").await, Ok(0), "was known missing");
    assert_eq!(names(&*fs, "/e").await, ["f", "only_in_d",]);
    assert!(fs.stat(&path("/d/f")).await.is_err(), "the old child");
    assert!(fs.read_dir(&path("/d")).await.is_err(), "the old listing");
    assert_eq!(names(&*fs, "/").await, ["e"]);
}

#[tokio::test(start_paused = true)]
async fn renames_keep_a_sibling_with_a_longer_name() {
    let (fs, server) = primed().await;
    create(&server, "/d2", b"12").await;
    size(&*fs, "/d2").await.unwrap();
    fs.rename(&path("/d"), &path("/e"), false).await.unwrap();
    assert!(!stat_reaches_server(&*fs, &server, "/d2").await);
}

#[tokio::test(start_paused = true)]
async fn a_handle_writes_to_where_its_file_was_renamed() {
    let (fs, _server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.rename(&path("/d/f"), &path("/d/g"), false)
        .await
        .unwrap();
    assert_eq!(size(&*fs, "/d/g").await, Ok(5));
    names(&*fs, "/d").await;

    fs.write(&handle, 5, Bytes::from_static(b"6"))
        .await
        .unwrap();

    assert_eq!(size(&*fs, "/d/g").await, Ok(6));
    assert_eq!(fs.read_dir(&path("/d")).await.unwrap()[0].attr.size, 6);
    fs.close(handle).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_handle_follows_a_rename_of_its_directory() {
    let (fs, _server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    fs.rename(&path("/d"), &path("/e"), false).await.unwrap();
    assert_eq!(size(&*fs, "/e/f").await, Ok(5));

    fs.write(&handle, 5, Bytes::from_static(b"6"))
        .await
        .unwrap();

    assert_eq!(size(&*fs, "/e/f").await, Ok(6));
    fs.close(handle).await.unwrap();
}

// -- Changes whose outcome is unknown or that overlap with a lookup ----------------------------

/// A timeout says nothing about whether the server acted, and here it did.
#[tokio::test(start_paused = true)]
async fn calls_that_fail_invalidate_too() {
    let (fs, server) = primed().await;
    let handle = fs
        .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
        .await
        .unwrap();
    server.lose_replies.store(true, Ordering::SeqCst);

    fs.write(&handle, 5, Bytes::from_static(b"6"))
        .await
        .unwrap_err();
    assert_eq!(size(&*fs, "/d/f").await, Ok(6), "write");
    assert_eq!(
        fs.read_dir(&path("/d")).await.unwrap()[0].attr.size,
        6,
        "write"
    );

    let shrink = AttrChanges {
        size: Some(2),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Handle(&handle), shrink)
        .await
        .unwrap_err();
    assert_eq!(size(&*fs, "/d/f").await, Ok(2), "set_attr by handle");

    let shrink = AttrChanges {
        size: Some(1),
        ..AttrChanges::default()
    };
    fs.set_attr(AttrTarget::Path(&path("/d/f")), shrink)
        .await
        .unwrap_err();
    assert_eq!(size(&*fs, "/d/f").await, Ok(1), "set_attr by path");

    fs.mkdir(&path("/d/missing")).await.unwrap_err();
    assert_eq!(size(&*fs, "/d/missing").await, Ok(0), "mkdir");
    assert_eq!(names(&*fs, "/d").await, ["f", "missing"], "mkdir");

    fs.rename(&path("/d/f"), &path("/d/g"), false)
        .await
        .unwrap_err();
    assert_eq!(names(&*fs, "/d").await, ["g", "missing"], "rename");
    assert_eq!(size(&*fs, "/d/g").await, Ok(1), "rename");
    assert!(fs.stat(&path("/d/f")).await.is_err(), "rename");

    fs.remove_file(&path("/d/g")).await.unwrap_err();
    assert_eq!(names(&*fs, "/d").await, ["missing"], "remove_file");
    assert!(fs.stat(&path("/d/g")).await.is_err(), "remove_file");

    fs.remove_dir(&path("/d/missing")).await.unwrap_err();
    assert!(names(&*fs, "/d").await.is_empty(), "remove_dir");
    assert!(fs.stat(&path("/d/missing")).await.is_err(), "remove_dir");

    server.lose_replies.store(false, Ordering::SeqCst);
    fs.close(handle).await.unwrap();
}

/// A layer above can give up on a call and drop it, and the server may have acted by then.
#[tokio::test(start_paused = true)]
async fn calls_that_are_dropped_invalidate_too() {
    let (fs, server) = primed().await;
    let hold = server.hold("remove_file");
    let removing = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.remove_file(&path("/d/f")).await })
    };
    hold.reached.notified().await;
    removing.abort();
    assert!(removing.await.unwrap_err().is_cancelled());

    assert_eq!(size(&*fs, "/d/f").await, Err(FsError::NotFound));
    assert!(names(&*fs, "/d").await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_dropped_write_invalidates_too() {
    let (fs, server) = primed().await;
    let hold = server.hold("write");
    let writer = fs.clone();
    let writing = tokio::spawn(async move {
        let handle = writer
            .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
            .await
            .unwrap();
        writer.write(&handle, 5, Bytes::from_static(b"6")).await
    });
    hold.reached.notified().await;
    writing.abort();
    assert!(writing.await.unwrap_err().is_cancelled());

    assert_eq!(size(&*fs, "/d/f").await, Ok(6));
}

/// The server answers a lookup, the same path then changes, and only then does the answer arrive.
async fn lookup_overtaken_by(
    op: &'static str,
    change: impl AsyncFnOnce(&CachedFs<Server>),
    expect: impl AsyncFnOnce(&CachedFs<Server>),
) {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    create(&server, "/d/f", b"12345").await;
    let hold = server.hold(op);
    let looking = {
        let fs = fs.clone();
        tokio::spawn(async move {
            let _ = fs.stat(&path("/d/f")).await;
            let _ = fs.read_dir(&path("/d")).await;
        })
    };
    hold.reached.notified().await;
    change(&fs).await;
    hold.go_on.notify_one();
    looking.await.unwrap();
    expect(&fs).await;
}

#[tokio::test(start_paused = true)]
async fn a_stat_overtaken_by_a_write_is_not_kept() {
    lookup_overtaken_by(
        "stat",
        async |fs| {
            let handle = fs
                .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
                .await
                .unwrap();
            fs.write(&handle, 5, Bytes::from_static(b"678"))
                .await
                .unwrap();
            fs.close(handle).await.unwrap();
        },
        async |fs| assert_eq!(size(fs, "/d/f").await, Ok(8)),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_listing_overtaken_by_a_create_is_not_kept() {
    lookup_overtaken_by(
        "read_dir",
        async |fs| create(fs, "/d/new", b"").await,
        async |fs| assert_eq!(names(fs, "/d").await, ["f", "new"]),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_listing_overtaken_by_a_write_leaves_no_old_attributes() {
    lookup_overtaken_by(
        "read_dir",
        async |fs| {
            let handle = fs
                .open(&path("/d/f"), OpenMode::write(Disposition::OpenExisting))
                .await
                .unwrap();
            fs.write(&handle, 5, Bytes::from_static(b"678"))
                .await
                .unwrap();
            fs.close(handle).await.unwrap();
        },
        async |fs| assert_eq!(size(fs, "/d/f").await, Ok(8)),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_stat_overtaken_by_a_rename_of_its_directory_is_not_kept() {
    lookup_overtaken_by(
        "stat",
        async |fs| fs.rename(&path("/d"), &path("/e"), false).await.unwrap(),
        async |fs| assert!(fs.stat(&path("/d/f")).await.is_err()),
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_negative_answer_overtaken_by_a_create_is_not_kept() {
    let (fs, server) = cached(TTL);
    let hold = server.hold("stat");
    let looking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.stat(&path("/desktop.ini")).await })
    };
    hold.reached.notified().await;
    create(&*fs, "/desktop.ini", b"12").await;
    hold.go_on.notify_one();
    looking.await.unwrap().unwrap_err();

    assert_eq!(size(&*fs, "/desktop.ini").await, Ok(2));
}

// -- Size limits -------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn attributes_are_bounded_and_the_oldest_go_first() {
    let (fs, server) = cached(TTL);
    create(&server, "/keep", b"").await;
    for i in 0..10_000 {
        create(&server, &format!("/{i}"), b"").await;
    }
    size(&*fs, "/keep").await.unwrap();
    for i in 0..9_999 {
        size(&*fs, &format!("/{i}")).await.unwrap();
    }
    // Full. Using an entry protects it, so the next to go is /0.
    size(&*fs, "/keep").await.unwrap();
    size(&*fs, "/9999").await.unwrap();

    assert!(!stat_reaches_server(&*fs, &server, "/keep").await);
    assert!(!stat_reaches_server(&*fs, &server, "/1").await);
    assert!(stat_reaches_server(&*fs, &server, "/0").await);
}

#[tokio::test(start_paused = true)]
async fn negative_lookups_are_bounded_and_do_not_push_out_attributes() {
    let (fs, server) = cached(TTL);
    create(&server, "/real", b"").await;
    size(&*fs, "/real").await.unwrap();
    for i in 0..=10_000 {
        size(&*fs, &format!("/{i}")).await.unwrap_err();
    }
    assert!(!stat_reaches_server(&*fs, &server, "/real").await);
    assert!(!stat_reaches_server(&*fs, &server, "/10000").await);
    assert!(stat_reaches_server(&*fs, &server, "/0").await);
}

#[tokio::test(start_paused = true)]
async fn listings_are_bounded_by_the_entries_they_hold() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/small").await;
    mkdir(&server, "/big").await;
    create(&server, "/small/f", b"").await;
    // With the listing's own weight of one, this exactly fills the cache.
    for i in 0..9_999 {
        create(&server, &format!("/big/{i}"), b"").await;
    }
    names(&*fs, "/small").await;
    names(&*fs, "/big").await;
    assert!(!listing_reaches_server(&*fs, &server, "/big").await);
    assert!(
        listing_reaches_server(&*fs, &server, "/small").await,
        "pushed out"
    );
}

#[tokio::test(start_paused = true)]
async fn a_listing_too_big_for_the_cache_still_works_and_pushes_nothing_out() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/small").await;
    mkdir(&server, "/huge").await;
    for i in 0..10_000 {
        create(&server, &format!("/huge/{i}"), b"").await;
    }
    names(&*fs, "/small").await;

    assert_eq!(names(&*fs, "/huge").await.len(), 10_000);

    assert!(listing_reaches_server(&*fs, &server, "/huge").await);
    assert!(stat_reaches_server(&*fs, &server, "/huge/0").await);
    assert!(!listing_reaches_server(&*fs, &server, "/small").await);
}

// -- Finding a name in any case ----------------------------------------------------------------

async fn find(fs: &CachedFs<Server>, dir: &str, name: &str) -> Option<String> {
    fs.find_name(&path(dir), name).await.unwrap().unwrap()
}

#[tokio::test(start_paused = true)]
async fn find_name_ignores_case_only_when_one_name_matches() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    create(&server, "/d/Report.txt", b"").await;
    create(&server, "/d/notes", b"").await;
    create(&server, "/d/NOTES", b"").await;

    assert_eq!(
        find(&fs, "/d", "REPORT.TXT").await.as_deref(),
        Some("Report.txt")
    );
    assert_eq!(find(&fs, "/d", "NOTES").await.as_deref(), Some("NOTES"));
    assert_eq!(find(&fs, "/d", "Notes").await, None, "ambiguous");
    assert_eq!(find(&fs, "/d", "other").await, None);
    assert_eq!(
        fs.find_name(&path("/d"), "..").await.unwrap(),
        Err(drivemount_vfs::NameError::DotName)
    );
    assert_eq!(server.calls("read_dir"), 1);
}

/// NFR-PERF-07: each new file used to cost a listing of its whole folder (M1-19's finding).
#[tokio::test(start_paused = true)]
async fn filling_a_folder_lists_it_once() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    for i in 0..100 {
        let name = format!("File{i}");
        assert_eq!(find(&fs, "/d", &name.to_lowercase()).await, None);
        create(&*fs, &format!("/d/{name}"), b"data").await;
        assert_eq!(
            find(&fs, "/d", &name.to_uppercase()).await,
            Some(name),
            "the new name is found at once, in any case"
        );
    }
    assert_eq!(server.calls("read_dir"), 1);

    fs.remove_file(&path("/d/File7")).await.unwrap();
    assert_eq!(find(&fs, "/d", "file7").await, None);
    fs.rename(&path("/d/File8"), &path("/d/Moved"), false)
        .await
        .unwrap();
    assert_eq!(find(&fs, "/d", "file8").await, None);
    assert_eq!(find(&fs, "/d", "MOVED").await.as_deref(), Some("Moved"));
    fs.mkdir(&path("/d/Sub")).await.unwrap();
    assert_eq!(find(&fs, "/d", "sub").await.as_deref(), Some("Sub"));
    fs.remove_dir(&path("/d/Sub")).await.unwrap();
    assert_eq!(find(&fs, "/d", "sub").await, None);
    assert_eq!(server.calls("read_dir"), 1);
}

#[tokio::test(start_paused = true)]
async fn names_last_as_long_as_the_listing_they_came_from() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    names(&*fs, "/d").await;
    advance(TTL - MS).await;
    create(&server, "/d/behind-the-back", b"").await;

    assert_eq!(find(&fs, "/d", "BEHIND-THE-BACK").await, None);
    advance(MS).await;
    assert_eq!(
        find(&fs, "/d", "BEHIND-THE-BACK").await.as_deref(),
        Some("behind-the-back")
    );
    assert_eq!(server.calls("read_dir"), 2);
}

#[tokio::test(start_paused = true)]
async fn a_change_with_an_unknown_outcome_drops_the_names() {
    let (fs, server) = primed().await;
    assert_eq!(find(&fs, "/d", "F").await.as_deref(), Some("f"));
    server.lose_replies.store(true, Ordering::SeqCst);

    fs.remove_file(&path("/d/f")).await.unwrap_err();
    let listed = server.calls("read_dir");
    assert_eq!(find(&fs, "/d", "F").await, None, "it was removed after all");
    assert_eq!(server.calls("read_dir"), listed + 1);

    fs.mkdir(&path("/d/new")).await.unwrap_err();
    assert_eq!(find(&fs, "/d", "NEW").await.as_deref(), Some("new"));

    fs.rename(&path("/d/new"), &path("/d/renamed"), false)
        .await
        .unwrap_err();
    assert_eq!(find(&fs, "/d", "NEW").await, None);
    assert_eq!(find(&fs, "/d", "RENAMED").await.as_deref(), Some("renamed"));
    server.lose_replies.store(false, Ordering::SeqCst);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_create_drops_the_names() {
    let (fs, server) = primed().await;
    assert_eq!(find(&fs, "/d", "G").await, None);
    let hold = server.hold("open");
    let creating = {
        let fs = fs.clone();
        tokio::spawn(async move {
            fs.open(&path("/d/g"), OpenMode::write(Disposition::CreateNew))
                .await
        })
    };
    hold.reached.notified().await;
    creating.abort();
    assert!(creating.await.unwrap_err().is_cancelled());

    assert_eq!(find(&fs, "/d", "G").await.as_deref(), Some("g"));
}

#[tokio::test(start_paused = true)]
async fn names_overtaken_by_a_create_are_not_kept() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/d").await;
    let hold = server.hold("read_dir");
    let looking = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.find_name(&path("/d"), "NEW").await })
    };
    hold.reached.notified().await;
    create(&*fs, "/d/new", b"").await;
    hold.go_on.notify_one();
    assert_eq!(
        looking.await.unwrap(),
        Ok(Ok(None)),
        "asked before the create"
    );

    assert_eq!(find(&fs, "/d", "NEW").await.as_deref(), Some("new"));
}

#[tokio::test(start_paused = true)]
async fn a_zero_ttl_keeps_no_names() {
    let (fs, server) = cached(Duration::ZERO);
    mkdir(&server, "/d").await;
    create(&server, "/d/f", b"").await;
    assert_eq!(find(&fs, "/d", "F").await.as_deref(), Some("f"));
    assert_eq!(find(&fs, "/d", "F").await.as_deref(), Some("f"));
    assert_eq!(server.calls("read_dir"), 2);
}

#[tokio::test(start_paused = true)]
async fn the_names_of_a_folder_too_big_to_list_from_memory_are_kept() {
    let (fs, server) = cached(TTL);
    mkdir(&server, "/huge").await;
    for i in 0..10_000 {
        create(&server, &format!("/huge/{i}"), b"").await;
    }
    assert_eq!(find(&fs, "/huge", "9999").await.as_deref(), Some("9999"));
    create(&*fs, "/huge/New", b"").await;
    assert_eq!(find(&fs, "/huge", "NEW").await.as_deref(), Some("New"));
    assert_eq!(server.calls("read_dir"), 1);
}

// -- The requirement and the contract ----------------------------------------------------------

/// NFR-REL-02: what happens through the drive shows in the very next listing. The first listing of
/// each step fills the cache; no time passes, so only invalidation can make the next one right.
#[tokio::test(start_paused = true)]
async fn nfr_rel_02_changes_show_in_the_very_next_listing() {
    let fs = CachedFs::new(MemFs::new(), Duration::from_secs(60));
    mkdir(&fs, "/d").await;
    assert!(names(&fs, "/d").await.is_empty());

    create(&fs, "/d/a", b"").await;
    assert_eq!(names(&fs, "/d").await, ["a"], "created");

    fs.rename(&path("/d/a"), &path("/d/b"), false)
        .await
        .unwrap();
    assert_eq!(names(&fs, "/d").await, ["b"], "renamed");

    fs.remove_file(&path("/d/b")).await.unwrap();
    assert!(names(&fs, "/d").await.is_empty(), "deleted");

    mkdir(&fs, "/d/sub").await;
    assert_eq!(names(&fs, "/d").await, ["sub"], "directory created");

    fs.rename(&path("/d/sub"), &path("/d/moved"), false)
        .await
        .unwrap();
    assert_eq!(names(&fs, "/d").await, ["moved"], "directory renamed");

    fs.remove_dir(&path("/d/moved")).await.unwrap();
    assert!(names(&fs, "/d").await.is_empty(), "directory deleted");
}

#[tokio::test]
async fn passes_the_contract_with_a_long_ttl() {
    let fs = CachedFs::new(MemFs::new(), Duration::from_secs(60));
    contract::run_quick(&fs, &RemotePath::root()).await;
}

#[tokio::test]
async fn passes_the_contract_with_a_zero_ttl() {
    contract::run_quick(
        &CachedFs::new(MemFs::new(), Duration::ZERO),
        &RemotePath::root(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passes_the_contract_on_worker_threads_behind_dyn() {
    let fs: Arc<dyn RemoteFs> = Arc::new(CachedFs::new(MemFs::new(), TTL));
    tokio::spawn(async move { contract::run_quick(&*fs, &RemotePath::root()).await })
        .await
        .unwrap();
}

// -- Any sequence of local changes -------------------------------------------------------------

/// Upper-case names too, so that finding a name has cases to tell apart.
const MODEL_PATHS: [&str; 9] = [
    "/a", "/b", "/A", "/a/a", "/a/b", "/a/B", "/b/a", "/b/b", "/a/a/a",
];

#[derive(Debug, Clone)]
enum Op {
    Create(usize),
    Truncate(usize),
    Mkdir(usize),
    RemoveFile(usize),
    RemoveDir(usize),
    Rename(usize, usize, bool),
    Open(usize),
    Write(usize),
    SetSize(usize),
    SetSizeByPath(usize),
    Fstat(usize),
    Flush(usize),
    Close(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let at = 0..MODEL_PATHS.len();
    let handle = 0..4_usize;
    prop_oneof![
        at.clone().prop_map(Op::Create),
        at.clone().prop_map(Op::Truncate),
        at.clone().prop_map(Op::Mkdir),
        at.clone().prop_map(Op::RemoveFile),
        at.clone().prop_map(Op::RemoveDir),
        (at.clone(), at.clone(), any::<bool>()).prop_map(|(a, b, r)| Op::Rename(a, b, r)),
        at.clone().prop_map(Op::Open),
        handle.clone().prop_map(Op::Write),
        handle.clone().prop_map(Op::SetSize),
        at.clone().prop_map(Op::SetSizeByPath),
        handle.clone().prop_map(Op::Fstat),
        handle.clone().prop_map(Op::Flush),
        handle.prop_map(Op::Close),
    ]
}

async fn apply(fs: &impl RemoteFs, op: Op, handles: &mut Vec<RemoteHandle>) {
    let at = |i: usize| path(MODEL_PATHS[i]);
    let open_write = OpenMode::write(Disposition::OpenExisting);
    let resize = AttrChanges {
        size: Some(3),
        ..AttrChanges::default()
    };
    // Plenty of these fail, on purpose: they must leave the caches right as well.
    match op {
        Op::Create(i) => {
            if let Ok(handle) = fs
                .open(&at(i), OpenMode::write(Disposition::OpenOrCreate))
                .await
            {
                handles.push(handle);
            }
        }
        Op::Truncate(i) => {
            let mode = OpenMode::write(Disposition::TruncateExisting);
            if let Ok(handle) = fs.open(&at(i), mode).await {
                handles.push(handle);
            }
        }
        Op::Mkdir(i) => drop(fs.mkdir(&at(i)).await),
        Op::RemoveFile(i) => drop(fs.remove_file(&at(i)).await),
        Op::RemoveDir(i) => drop(fs.remove_dir(&at(i)).await),
        Op::Rename(a, b, replace) => drop(fs.rename(&at(a), &at(b), replace).await),
        Op::Open(i) => {
            if let Ok(handle) = fs.open(&at(i), open_write).await {
                handles.push(handle);
            }
        }
        Op::Write(i) => {
            if let Some(handle) = handles.get(i) {
                drop(fs.write(handle, 0, Bytes::from_static(b"1234567")).await);
            }
        }
        Op::SetSize(i) => {
            if let Some(handle) = handles.get(i) {
                drop(fs.set_attr(AttrTarget::Handle(handle), resize).await);
            }
        }
        Op::SetSizeByPath(i) => drop(fs.set_attr(AttrTarget::Path(&at(i)), resize).await),
        Op::Fstat(i) => {
            if let Some(handle) = handles.get(i) {
                drop(fs.fstat(handle).await);
            }
        }
        Op::Flush(i) => {
            if let Some(handle) = handles.get(i) {
                drop(fs.flush(handle).await);
            }
        }
        Op::Close(i) => {
            if i < handles.len() {
                drop(fs.close(handles.remove(i)).await);
            }
        }
    }
}

fn sorted(listing: FsResult<Vec<DirEntry>>) -> FsResult<Vec<DirEntry>> {
    listing.map(|mut entries| {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// With a TTL too long to run out, the cache must still say just what the server says after every
    /// step of any sequence. Comparing after every step also fills the caches, ready to go stale.
    #[test]
    fn the_cache_never_differs_from_the_server_after_local_changes(
        ops in proptest::collection::vec(op(), 1..40)
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = Server::default();
            let fs = CachedFs::new(server.clone(), Duration::from_secs(60));
            let mut handles = Vec::new();
            for op in ops {
                let step = format!("{op:?}");
                apply(&fs, op, &mut handles).await;
                for at in MODEL_PATHS.iter().chain(&["/"]).map(|at| path(at)) {
                    assert_eq!(fs.stat(&at).await, server.fs.stat(&at).await, "stat {at} after {step}");
                    // Before the listing, which would fill the names afresh.
                    for name in ["a", "A", "b", "B"] {
                        let expected = server.fs.read_dir(&at).await.map(|entries| {
                            let names = entries.iter().map(|entry| entry.name.as_str());
                            match resolve_name(name, names).unwrap() {
                                Resolved::Exact(found) | Resolved::CaseInsensitive(found) => {
                                    Some(found.to_owned())
                                }
                                Resolved::Ambiguous | Resolved::NotFound => None,
                            }
                        });
                        assert_eq!(
                            fs.find_name(&at, name).await.map(Result::unwrap),
                            expected,
                            "finding {name} in {at} after {step}"
                        );
                    }
                    assert_eq!(
                        sorted(fs.read_dir(&at).await),
                        sorted(server.fs.read_dir(&at).await),
                        "listing {at} after {step}"
                    );
                }
            }
        });
    }
}
