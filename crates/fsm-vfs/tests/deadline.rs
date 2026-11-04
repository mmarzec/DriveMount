// SPDX-License-Identifier: GPL-3.0-or-later

//! `DeadlineFs` over a slow `MemFs`. The clock is paused, so deadlines are exact and take no time.

// clippy.toml exempts only code inside #[test] functions, not the helpers.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fsm_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, CachedFs, Capabilities, DeadlineFs, DirEntry,
    Disposition, FsError, FsResult, FsStats, MemFs, OpenMode, RemoteFs, RemoteHandle, RemotePath,
    async_trait, contract,
};
use tokio::time::{Instant, sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(15);
const MS: Duration = Duration::from_millis(1);

/// `MemFs` as a server that acts at once and answers after `delay`, or with `error`. Clones share
/// the server.
#[derive(Clone, Default)]
struct Slow {
    fs: Arc<MemFs>,
    delay: Arc<Mutex<Duration>>,
    error: Arc<Mutex<Option<FsError>>>,
    waiting: Arc<AtomicUsize>,
}

struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Slow {
    fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    async fn reply<T>(&self, result: FsResult<T>) -> FsResult<T> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiting);
        let delay = *self.delay.lock().unwrap();
        sleep(delay).await;
        match self.error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => result,
        }
    }
}

#[async_trait]
impl RemoteFs for Slow {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.reply(self.fs.stat(path).await).await
    }
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.reply(self.fs.lstat(path).await).await
    }
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.reply(self.fs.fstat(handle).await).await
    }
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.reply(self.fs.read_dir(path).await).await
    }
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.reply(self.fs.open(path, mode).await).await
    }
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.reply(self.fs.read(handle, offset, len).await).await
    }
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.reply(self.fs.write(handle, offset, data).await).await
    }
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.reply(self.fs.flush(handle).await).await
    }
    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.reply(self.fs.close(handle).await).await
    }
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.reply(self.fs.set_attr(target, changes).await).await
    }
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.reply(self.fs.mkdir(path).await).await
    }
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.reply(self.fs.remove_file(path).await).await
    }
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.reply(self.fs.remove_dir(path).await).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.reply(self.fs.rename(from, to, replace).await).await
    }
    async fn statfs(&self) -> FsResult<FsStats> {
        self.reply(self.fs.statfs().await).await
    }
    fn capabilities(&self) -> Capabilities {
        self.fs.capabilities()
    }
}

fn limited() -> (DeadlineFs<Slow>, Slow) {
    let slow = Slow::default();
    (DeadlineFs::new(slow.clone(), DEADLINE), slow)
}

fn path(path: &str) -> RemotePath {
    RemotePath::new(path).unwrap()
}

async fn open_new(fs: &impl RemoteFs, at: &str) -> RemoteHandle {
    fs.open(
        &path(at),
        OpenMode::read_write(Disposition::CreateOrTruncate),
    )
    .await
    .unwrap()
}

/// Runs one call and checks that it timed out exactly at the deadline.
macro_rules! assert_timed_out {
    ($name:literal, $call:expr) => {{
        let start = Instant::now();
        let result = $call.await.map(|_| ());
        assert_eq!(result, Err(FsError::Timeout), "{}", $name);
        assert_eq!(start.elapsed(), DEADLINE, "{}", $name);
    }};
}

#[tokio::test]
async fn memfs_with_a_deadline_passes_the_contract() {
    let fs = DeadlineFs::new(MemFs::new(), DEADLINE);
    contract::run_all(&fs, &RemotePath::root()).await;
    assert_eq!(*fs.suspect().borrow(), 0);
}

#[tokio::test(start_paused = true)]
async fn every_call_times_out_at_the_deadline_and_signals_suspect() {
    let (fs, slow) = limited();
    let handle = open_new(&fs, "/f").await;
    let to_close = open_new(&fs, "/g").await;
    let (file, dir, other) = (path("/f"), path("/d"), path("/other"));
    let suspect = fs.suspect();
    slow.set_delay(DEADLINE + MS);

    assert_timed_out!("stat", fs.stat(&file));
    assert_timed_out!("lstat", fs.lstat(&file));
    assert_timed_out!("fstat", fs.fstat(&handle));
    assert_timed_out!("read_dir", fs.read_dir(&RemotePath::root()));
    assert_timed_out!("open", fs.open(&file, OpenMode::read()));
    assert_timed_out!("read", fs.read(&handle, 0, 1));
    assert_timed_out!("write", fs.write(&handle, 0, Bytes::from_static(b"x")));
    assert_timed_out!("flush", fs.flush(&handle));
    assert_timed_out!("close", fs.close(to_close));
    assert_timed_out!(
        "set_attr",
        fs.set_attr(AttrTarget::Path(&file), AttrChanges::default())
    );
    assert_timed_out!("mkdir", fs.mkdir(&dir));
    assert_timed_out!("rename", fs.rename(&file, &other, false));
    assert_timed_out!("remove_file", fs.remove_file(&other));
    assert_timed_out!("remove_dir", fs.remove_dir(&dir));
    assert_timed_out!("statfs", fs.statfs());

    assert_eq!(*suspect.borrow(), 15);
}

#[tokio::test(start_paused = true)]
async fn an_answer_before_the_deadline_is_returned() {
    let (fs, slow) = limited();
    slow.set_delay(DEADLINE - MS);
    let start = Instant::now();
    fs.mkdir(&path("/d")).await.unwrap();
    assert_eq!(start.elapsed(), DEADLINE - MS);
    assert_eq!(*fs.suspect().borrow(), 0);
}

#[tokio::test(start_paused = true)]
async fn other_errors_pass_through_without_suspicion() {
    let (fs, slow) = limited();
    assert_eq!(fs.stat(&path("/none")).await, Err(FsError::NotFound));
    *slow.error.lock().unwrap() = Some(FsError::ConnectionLost);
    assert_eq!(fs.statfs().await, Err(FsError::ConnectionLost));
    assert_eq!(*fs.suspect().borrow(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_timeout_from_the_backend_signals_suspect_too() {
    let (fs, slow) = limited();
    *slow.error.lock().unwrap() = Some(FsError::Timeout);
    assert_eq!(fs.statfs().await, Err(FsError::Timeout));
    assert_eq!(*fs.suspect().borrow(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_deadline_is_per_call() {
    let (fs, slow) = limited();
    slow.set_delay(DEADLINE - MS);
    for _ in 0..3 {
        fs.statfs().await.unwrap();
    }
    slow.set_delay(DEADLINE + MS);
    assert_timed_out!("after answers", fs.statfs());
    slow.set_delay(Duration::ZERO);
    fs.statfs().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn overlapping_calls_time_out_independently() {
    let (fs, slow) = limited();
    slow.set_delay(DEADLINE + MS);
    let first = fs.statfs();
    let second = async {
        sleep(Duration::from_secs(5)).await;
        let start = Instant::now();
        let result = fs.statfs().await;
        (result, start.elapsed())
    };
    let (first, (second, waited)) = tokio::join!(first, second);
    assert_eq!(first, Err(FsError::Timeout));
    assert_eq!(second, Err(FsError::Timeout));
    assert_eq!(waited, DEADLINE);
    assert_eq!(*fs.suspect().borrow(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_inner_call_is_dropped_at_the_deadline() {
    let (fs, slow) = limited();
    slow.set_delay(Duration::from_secs(3600));
    assert_timed_out!("statfs", fs.statfs());
    assert_eq!(slow.waiting.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn the_signal_wakes_a_watcher_on_each_timeout() {
    let (fs, slow) = limited();
    let mut suspect = fs.suspect();
    slow.set_delay(DEADLINE + MS);

    assert!(!suspect.has_changed().unwrap());
    fs.statfs().await.unwrap_err();
    timeout(MS, suspect.changed()).await.unwrap().unwrap();
    assert_eq!(*suspect.borrow_and_update(), 1);
    assert!(!suspect.has_changed().unwrap());

    fs.statfs().await.unwrap_err();
    timeout(MS, suspect.changed()).await.unwrap().unwrap();
    assert_eq!(*suspect.borrow_and_update(), 2);
}

#[tokio::test(start_paused = true)]
async fn timeouts_count_with_nobody_watching() {
    let (fs, slow) = limited();
    slow.set_delay(DEADLINE + MS);
    fs.statfs().await.unwrap_err();
    assert_eq!(*fs.suspect().borrow(), 1);
}

#[tokio::test(start_paused = true)]
async fn capabilities_come_from_the_inner_session() {
    let (fs, slow) = limited();
    assert_eq!(fs.capabilities(), slow.capabilities());
}

/// The order M1-12a uses. The server acted on a call whose answer never came, so the cache must not
/// keep showing the old state.
#[tokio::test(start_paused = true)]
async fn a_change_that_timed_out_shows_in_the_next_listing() {
    let slow = Slow::default();
    let fs = CachedFs::new(
        DeadlineFs::new(slow.clone(), DEADLINE),
        Duration::from_secs(60),
    );
    assert!(fs.read_dir(&RemotePath::root()).await.unwrap().is_empty());

    slow.set_delay(DEADLINE + MS);
    assert_timed_out!("mkdir", fs.mkdir(&path("/d")));

    slow.set_delay(Duration::ZERO);
    let listing = fs.read_dir(&RemotePath::root()).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "d");
}
