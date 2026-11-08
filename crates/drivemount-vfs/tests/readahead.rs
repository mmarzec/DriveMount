// SPDX-License-Identifier: GPL-3.0-or-later

//! `ReadAhead` over a `MemFs` that records every read request. The clock is paused, so a server delay
//! leaves requests overlapping in a repeatable way and takes no time.

// clippy.toml exempts only code inside #[test] functions, not the helpers.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, Capabilities, DeadlineFs, DirEntry, Disposition, FsError,
    FsResult, FsStats, MemFs, OpenMode, ReadAhead, RemoteFs, RemoteHandle, RemotePath, async_trait,
};
use proptest::prelude::*;
use tokio::time::{Instant, sleep, timeout};

const KIB: u32 = 1024;
const MIB: u32 = 1024 * KIB;
const CHUNK: u32 = 64 * KIB;
const MAX_AGE: Duration = Duration::from_secs(5);
const RTT: Duration = Duration::from_millis(10);

/// `MemFs` as a server with a window onto its read requests. Clones share the server, so a test can
/// change files behind the reader's back, the way another client would.
#[derive(Clone)]
struct Server {
    fs: Arc<MemFs>,
    requests: Arc<Mutex<Vec<(u64, u32)>>>,
    delay: Arc<Mutex<Duration>>,
    /// Answers are cut to this many bytes.
    short: Arc<Mutex<Option<usize>>>,
    /// Answers get this many bytes more than was asked for.
    extra: Arc<Mutex<usize>>,
    /// Requests covering this offset fail.
    fail_at: Arc<Mutex<Option<u64>>>,
    running: Arc<Mutex<Running>>,
    closes: Arc<AtomicUsize>,
}

#[derive(Default)]
struct Running {
    chunks: usize,
    bytes: u64,
    peak_chunks: usize,
    peak_bytes: u64,
}

/// A request in progress, however it ends.
struct InFlight<'a>(&'a Mutex<Running>, u32);

impl<'a> InFlight<'a> {
    fn new(running: &'a Mutex<Running>, len: u32) -> Self {
        let mut now = running.lock().unwrap();
        now.chunks += 1;
        now.bytes += u64::from(len);
        now.peak_chunks = now.peak_chunks.max(now.chunks);
        now.peak_bytes = now.peak_bytes.max(now.bytes);
        Self(running, len)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut now = self.0.lock().unwrap();
        now.chunks -= 1;
        now.bytes -= u64::from(self.1);
    }
}

impl Server {
    fn new(max_read: u32) -> Self {
        Self {
            fs: Arc::new(MemFs::with_capabilities(Capabilities {
                max_read,
                max_write: MIB,
                ..MemFs::new().capabilities()
            })),
            requests: Arc::default(),
            delay: Arc::default(),
            short: Arc::default(),
            extra: Arc::default(),
            fail_at: Arc::default(),
            running: Arc::default(),
            closes: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<(u64, u32)> {
        let mut requests = self.requests.lock().unwrap().clone();
        requests.sort_unstable();
        requests
    }

    fn take_requests(&self) -> Vec<(u64, u32)> {
        let requests = self.requests();
        self.requests.lock().unwrap().clear();
        requests
    }

    fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// A file of `size` bytes, each showing its own offset ([`byte_at`]).
    async fn file(&self, size: usize) -> RemoteHandle {
        let writer = self
            .fs
            .open(&path(), OpenMode::write(Disposition::CreateOrTruncate))
            .await
            .unwrap();
        let contents: Vec<u8> = (0..size).map(byte_at).collect();
        for (i, part) in contents.chunks(MIB as usize).enumerate() {
            let offset = i as u64 * u64::from(MIB);
            let data = Bytes::copy_from_slice(part);
            self.fs.write(&writer, offset, data).await.unwrap();
        }
        self.fs.close(writer).await.unwrap();
        self.fs.open(&path(), OpenMode::read()).await.unwrap()
    }

    /// A file of `size` bytes of zeros that takes no memory.
    async fn sparse_file(&self, size: u64) -> RemoteHandle {
        let handle = self
            .fs
            .open(&path(), OpenMode::read_write(Disposition::CreateOrTruncate))
            .await
            .unwrap();
        let changes = AttrChanges {
            size: Some(size),
            ..AttrChanges::default()
        };
        let target = AttrTarget::Handle(&handle);
        self.fs.set_attr(target, changes).await.unwrap();
        handle
    }

    async fn overwrite(&self, offset: u64, data: &'static [u8]) {
        let writer = self
            .fs
            .open(&path(), OpenMode::write(Disposition::OpenExisting))
            .await
            .unwrap();
        let data = Bytes::from_static(data);
        self.fs.write(&writer, offset, data).await.unwrap();
        self.fs.close(writer).await.unwrap();
    }
}

#[async_trait]
impl RemoteFs for Server {
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.requests.lock().unwrap().push((offset, len));
        let _in_flight = InFlight::new(&self.running, len);
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            sleep(delay).await;
        }
        let failed = self
            .fail_at
            .lock()
            .unwrap()
            .is_some_and(|at| (offset..offset + u64::from(len)).contains(&at));
        if failed {
            return Err(FsError::Other("bad sector".to_owned()));
        }
        let mut bytes = self.fs.read(handle, offset, len).await?;
        if let Some(short) = *self.short.lock().unwrap() {
            bytes.truncate(short);
        }
        let extra = *self.extra.lock().unwrap();
        if extra > 0 {
            let padded = [&bytes[..], &vec![0xEE; extra]].concat();
            bytes = Bytes::from(padded);
        }
        Ok(bytes)
    }
    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        self.fs.close(handle).await
    }
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.fs.stat(path).await
    }
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.fs.lstat(path).await
    }
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.fs.fstat(handle).await
    }
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.fs.read_dir(path).await
    }
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.fs.open(path, mode).await
    }
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.fs.write(handle, offset, data).await
    }
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.fs.flush(handle).await
    }
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.fs.set_attr(target, changes).await
    }
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.fs.mkdir(path).await
    }
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.fs.remove_file(path).await
    }
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.fs.remove_dir(path).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.fs.rename(from, to, replace).await
    }
    async fn statfs(&self) -> FsResult<FsStats> {
        self.fs.statfs().await
    }
    fn capabilities(&self) -> Capabilities {
        self.fs.capabilities()
    }
}

fn path() -> RemotePath {
    RemotePath::new("/f").unwrap()
}

/// Spreads over all byte values and does not repeat at any size a test uses, so a misplaced range
/// does not match by accident.
fn byte_at(offset: usize) -> u8 {
    (offset % 251) as u8 ^ (offset / 251 % 241) as u8
}

fn expected(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(byte_at).collect()
}

fn reader(server: &Server, handle: RemoteHandle) -> ReadAhead<Server> {
    ReadAhead::new(Arc::new(server.clone()), handle)
}

/// Lets the requests already started reach the server. Time moves on only once all tasks are idle.
async fn settle() {
    sleep(Duration::from_micros(1)).await;
}

/// `(offset, len)` for `count` requests of `CHUNK` bytes from `first_chunk` on.
fn chunks(first_chunk: u64, count: u64) -> Vec<(u64, u32)> {
    (first_chunk..first_chunk + count)
        .map(|i| (i * u64::from(CHUNK), CHUNK))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_sequential_read_issues_the_expected_chunk_pattern() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(64 * u64::from(MIB)).await);
    let mut offset = 0;
    let mut next = async |new: Vec<(u64, u32)>| {
        reader.read(offset, CHUNK).await.unwrap();
        settle().await;
        assert_eq!(server.take_requests(), new, "read at {offset}");
        offset += u64::from(CHUNK);
    };

    // The first read could be anything: just what it asked for.
    next(chunks(0, 1)).await;
    // The second proves it sequential: itself and a window of 128 KiB.
    next(chunks(1, 3)).await;
    // Same window, moved along by one read.
    next(chunks(4, 1)).await;
    // 128 KiB consumed, so the window doubles to 256 KiB: up to chunk 3 + 1 + 4 = 8.
    next(chunks(5, 3)).await;
    next(chunks(8, 1)).await;
    next(chunks(9, 1)).await;
    next(chunks(10, 1)).await;
    // 256 KiB consumed, so 512 KiB: up to chunk 7 + 1 + 8 = 16.
    next(chunks(11, 5)).await;
    next(chunks(16, 1)).await;
}

#[tokio::test(start_paused = true)]
async fn the_window_stops_growing_at_4_mib() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(256 * MIB)).await);
    let mut furthest_ahead = Vec::new();
    for i in 0..1024 {
        let offset = i * u64::from(CHUNK);
        reader.read(offset, CHUNK).await.unwrap();
        settle().await;
        let requested_to = server
            .requests()
            .iter()
            .map(|(offset, len)| offset + u64::from(*len))
            .max()
            .unwrap();
        furthest_ahead.push(requested_to - (offset + u64::from(CHUNK)));
    }
    assert!(furthest_ahead.is_sorted());
    assert_eq!(*furthest_ahead.last().unwrap(), u64::from(4 * MIB));
}

#[tokio::test(start_paused = true)]
async fn requests_never_overlap_or_leave_gaps() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    for i in 0..300 {
        let offset = i * u64::from(40 * KIB);
        reader.read(offset, 40 * KIB).await.unwrap();
    }
    settle().await;
    let requests = server.requests();
    for pair in requests.windows(2) {
        assert_eq!(pair[0].0 + u64::from(pair[0].1), pair[1].0, "{pair:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_large_read_is_split_into_chunks() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file((2 * MIB) as usize).await);
    let bytes = reader.read(0, MIB).await.unwrap();
    assert_eq!(bytes, expected(0, MIB as usize));
    settle().await;
    assert_eq!(server.requests(), chunks(0, 16));
}

#[tokio::test(start_paused = true)]
async fn the_last_chunk_of_a_read_is_only_as_long_as_needed() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file((2 * MIB) as usize).await);
    reader.read(0, CHUNK + 1).await.unwrap();
    settle().await;
    assert_eq!(server.requests(), vec![(0, CHUNK), (u64::from(CHUNK), 1)]);
}

#[tokio::test(start_paused = true)]
async fn at_most_64_chunks_are_in_flight() {
    let server = Server::new(32 * KIB);
    server.set_delay(RTT);
    let reader = reader(&server, server.sparse_file(u64::from(32 * MIB)).await);
    let bytes = reader.read(0, 16 * MIB).await.unwrap();
    assert_eq!(bytes.len(), (16 * MIB) as usize);
    assert_eq!(server.running.lock().unwrap().peak_chunks, 64);
}

#[tokio::test(start_paused = true)]
async fn at_most_4_mib_is_in_flight() {
    let server = Server::new(256 * KIB);
    server.set_delay(RTT);
    let reader = reader(&server, server.sparse_file(u64::from(32 * MIB)).await);
    let bytes = reader.read(0, 16 * MIB).await.unwrap();
    assert_eq!(bytes.len(), (16 * MIB) as usize);
    let running = server.running.lock().unwrap();
    assert_eq!(running.peak_bytes, u64::from(4 * MIB));
    assert_eq!(running.peak_chunks, 16);
}

#[tokio::test(start_paused = true)]
async fn the_limits_hold_for_read_ahead_too() {
    let server = Server::new(32 * KIB);
    server.set_delay(RTT);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    for i in 0..1024 {
        reader
            .read(i * u64::from(32 * KIB), 32 * KIB)
            .await
            .unwrap();
    }
    let running = server.running.lock().unwrap();
    assert_eq!(running.peak_chunks, 64);
    assert!(running.peak_bytes <= u64::from(4 * MIB));
}

#[tokio::test(start_paused = true)]
async fn a_chunk_is_never_above_4_mib_whatever_the_server_allows() {
    let server = Server::new(64 * MIB);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    reader.read(0, 10 * MIB).await.unwrap();
    settle().await;
    assert_eq!(
        server.requests(),
        vec![
            (0, 4 * MIB),
            (u64::from(4 * MIB), 4 * MIB),
            (u64::from(8 * MIB), 2 * MIB),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn fetching_ahead_saves_round_trips() {
    let server = Server::new(CHUNK);
    server.set_delay(RTT);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    let start = Instant::now();
    for i in 0..512 {
        reader.read(i * u64::from(CHUNK), CHUNK).await.unwrap();
    }
    // 512 round trips one at a time. Once the window is open the reader waits for few.
    assert!(start.elapsed() < 100 * RTT, "{:?}", start.elapsed());
}

#[tokio::test(start_paused = true)]
async fn small_reads_do_not_become_small_requests() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    for i in 0..256 {
        reader.read(i * u64::from(4 * KIB), 4 * KIB).await.unwrap();
    }
    settle().await;
    let requests = server.requests();
    // Only the first read, made before access looked sequential, and the next one are that small.
    assert!(
        requests.iter().filter(|(_, len)| *len < CHUNK).count() <= 2,
        "{requests:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_window_smaller_than_a_chunk_still_fetches_ahead() {
    let server = Server::new(MIB);
    let reader = reader(&server, server.sparse_file(u64::from(16 * MIB)).await);
    reader.read(0, 4 * KIB).await.unwrap();
    reader.read(u64::from(4 * KIB), 4 * KIB).await.unwrap();
    settle().await;
    let requested_to = server
        .requests()
        .iter()
        .map(|(offset, len)| offset + u64::from(*len))
        .max()
        .unwrap();
    assert_eq!(requested_to, u64::from(8 * KIB + 128 * KIB));
}

#[tokio::test(start_paused = true)]
async fn a_window_smaller_than_a_chunk_keeps_being_filled() {
    let server = Server::new(MIB);
    let reader = reader(&server, server.sparse_file(u64::from(16 * MIB)).await);
    for i in 0..64 {
        let offset = i * u64::from(4 * KIB);
        reader.read(offset, 4 * KIB).await.unwrap();
        settle().await;
        let requested_to = server
            .requests()
            .iter()
            .map(|(offset, len)| offset + u64::from(*len))
            .max()
            .unwrap();
        if i > 0 {
            let ahead = requested_to - (offset + u64::from(4 * KIB));
            assert!(ahead >= u64::from(64 * KIB), "{ahead} ahead at {offset}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_window_grows_with_what_is_consumed_not_with_the_number_of_reads() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    for i in 0..30 {
        reader.read(i * u64::from(KIB), KIB).await.unwrap();
    }
    settle().await;
    let requested_to = server
        .requests()
        .iter()
        .map(|(offset, len)| offset + u64::from(*len))
        .max()
        .unwrap();
    // 30 KiB read: still the first window, not 4 MiB.
    assert!(
        requested_to <= u64::from(30 * KIB + 128 * KIB),
        "{requested_to}"
    );
}

#[tokio::test(start_paused = true)]
async fn random_reads_fetch_only_what_they_ask_for() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    for offset in [5 * MIB, MIB, 9 * MIB, 3 * MIB] {
        reader.read(u64::from(offset), 4 * KIB).await.unwrap();
        settle().await;
        assert_eq!(server.take_requests(), vec![(u64::from(offset), 4 * KIB)]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_jump_stops_read_ahead_and_starts_over() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file((8 * MIB) as usize).await);
    for i in 0..8 {
        reader.read(i * u64::from(CHUNK), CHUNK).await.unwrap();
    }
    settle().await;
    server.take_requests();

    let bytes = reader.read(u64::from(4 * MIB), CHUNK).await.unwrap();
    assert_eq!(bytes, expected((4 * MIB) as usize, CHUNK as usize));
    settle().await;
    assert_eq!(server.take_requests(), vec![(u64::from(4 * MIB), CHUNK)]);

    // Sequential again: back to the first window, not the one from before the jump.
    let next = u64::from(4 * MIB + CHUNK);
    reader.read(next, CHUNK).await.unwrap();
    settle().await;
    assert_eq!(server.take_requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_jump_back_returns_the_right_data() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    let bytes = reader.read(100, 1000).await.unwrap();
    assert_eq!(bytes, expected(100, 1000));
}

#[tokio::test(start_paused = true)]
async fn abandoned_requests_stop() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    reader.read(0, CHUNK).await.unwrap();
    server.set_delay(Duration::from_secs(3600));
    let sequential = timeout(RTT, reader.read(u64::from(CHUNK), 1));
    sequential.await.unwrap_err();
    settle().await;
    assert!(server.running.lock().unwrap().chunks > 0);

    reader.discard().await;
    settle().await;
    assert_eq!(server.running.lock().unwrap().chunks, 0);
}

#[tokio::test(start_paused = true)]
async fn the_file_end_gives_a_short_read_then_an_empty_one() {
    let server = Server::new(CHUNK);
    let size = 100_000;
    let reader = reader(&server, server.file(size).await);
    assert_eq!(reader.read(0, 60_000).await.unwrap(), expected(0, 60_000));
    assert_eq!(
        reader.read(60_000, 60_000).await.unwrap(),
        expected(60_000, 40_000)
    );
    assert!(reader.read(100_000, 60_000).await.unwrap().is_empty());
    assert!(reader.read(100_000, 60_000).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn nothing_is_fetched_ahead_after_the_file_end() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(1000).await);
    reader.read(0, 1000).await.unwrap();
    assert!(reader.read(1000, 1000).await.unwrap().is_empty());
    settle().await;
    server.take_requests();

    assert!(reader.read(1000, 1000).await.unwrap().is_empty());
    settle().await;
    assert_eq!(server.take_requests(), vec![(1000, 1000)]);
}

#[tokio::test(start_paused = true)]
async fn a_file_that_grows_can_be_followed() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(1000).await);
    reader.read(0, 1000).await.unwrap();
    assert!(reader.read(1000, 10).await.unwrap().is_empty());

    server.overwrite(1000, b"more").await;
    assert_eq!(reader.read(1000, 10).await.unwrap(), &b"more"[..]);
}

#[tokio::test(start_paused = true)]
async fn short_answers_are_asked_for_again_without_gaps() {
    let server = Server::new(CHUNK);
    *server.short.lock().unwrap() = Some(1000);
    let size = MIB as usize;
    let reader = reader(&server, server.file(size).await);
    let mut offset = 0;
    for len in [5000, CHUNK, 3 * CHUNK, 7] {
        let bytes = reader.read(offset as u64, len).await.unwrap();
        assert_eq!(bytes, expected(offset, len as usize), "at {offset}");
        offset += len as usize;
    }
}

#[tokio::test(start_paused = true)]
async fn answers_longer_than_asked_for_are_cut() {
    let server = Server::new(CHUNK);
    *server.extra.lock().unwrap() = 100;
    let reader = reader(&server, server.file(MIB as usize).await);
    let mut offset = 0;
    for _ in 0..8 {
        let bytes = reader.read(offset as u64, CHUNK).await.unwrap();
        assert_eq!(bytes, expected(offset, CHUNK as usize), "at {offset}");
        offset += CHUNK as usize;
    }
}

#[tokio::test(start_paused = true)]
async fn asking_for_nothing_asks_the_server_nothing() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(1000).await);
    assert!(reader.read(0, 0).await.unwrap().is_empty());
    settle().await;
    assert!(server.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn offsets_at_the_very_end_of_the_range_are_safe() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(1000).await);
    assert!(reader.read(u64::MAX, 100).await.unwrap().is_empty());
    assert!(reader.read(u64::MAX - 10, 100).await.unwrap().is_empty());
    assert!(reader.read(u64::MAX - 10, 100).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_request_fails_the_read_that_needs_it() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    *server.fail_at.lock().unwrap() = Some(u64::from(CHUNK) + 5);
    let result = reader.read(0, 2 * CHUNK).await;
    assert_eq!(result, Err(FsError::Other("bad sector".to_owned())));

    // The error is not kept: the same read goes back to the server.
    *server.fail_at.lock().unwrap() = None;
    let bytes = reader.read(0, 2 * CHUNK).await.unwrap();
    assert_eq!(bytes, expected(0, 2 * CHUNK as usize));
}

#[tokio::test(start_paused = true)]
async fn a_failed_request_harms_only_the_reads_that_reach_it() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    *server.fail_at.lock().unwrap() = Some(u64::from(3 * CHUNK));

    let chunk = CHUNK as usize;
    assert_eq!(reader.read(0, CHUNK).await.unwrap(), expected(0, chunk));
    // Fetches ahead over the bad spot, but returns what it asked for.
    let second = reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    assert_eq!(second, expected(chunk, chunk));
    settle().await;
    let third = reader.read(u64::from(2 * CHUNK), CHUNK).await.unwrap();
    assert_eq!(third, expected(2 * chunk, chunk));

    assert!(reader.read(u64::from(3 * CHUNK), CHUNK).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_read_that_failed_can_be_tried_again_at_the_same_place() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    *server.fail_at.lock().unwrap() = Some(u64::from(CHUNK));
    assert!(reader.read(u64::from(CHUNK), CHUNK).await.is_err());

    *server.fail_at.lock().unwrap() = None;
    let bytes = reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    assert_eq!(bytes, expected(CHUNK as usize, CHUNK as usize));
}

#[tokio::test(start_paused = true)]
async fn old_data_is_not_returned() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    settle().await;

    server.overwrite(u64::from(2 * CHUNK), b"new").await;
    sleep(MAX_AGE).await;
    let bytes = reader.read(u64::from(2 * CHUNK), 3).await.unwrap();
    assert_eq!(bytes, &b"new"[..]);
}

#[tokio::test(start_paused = true)]
async fn data_fetched_ahead_is_used_while_fresh() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    settle().await;

    server.overwrite(u64::from(2 * CHUNK), b"new").await;
    sleep(MAX_AGE - RTT).await;
    let bytes = reader.read(u64::from(2 * CHUNK), 3).await.unwrap();
    assert_eq!(bytes, expected(2 * CHUNK as usize, 3));
}

#[tokio::test(start_paused = true)]
async fn an_old_error_is_not_returned() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    *server.fail_at.lock().unwrap() = Some(u64::from(2 * CHUNK));
    reader.read(0, CHUNK).await.unwrap();
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    settle().await;

    *server.fail_at.lock().unwrap() = None;
    sleep(MAX_AGE).await;
    let bytes = reader.read(u64::from(2 * CHUNK), CHUNK).await.unwrap();
    assert_eq!(bytes, expected(2 * CHUNK as usize, CHUNK as usize));
}

#[tokio::test(start_paused = true)]
async fn a_reader_too_slow_for_its_window_gets_a_smaller_one() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(256 * MIB)).await);
    for i in 0..256 {
        reader.read(i * u64::from(CHUNK), CHUNK).await.unwrap();
    }
    settle().await;
    server.take_requests();

    sleep(MAX_AGE).await;
    reader.read(256 * u64::from(CHUNK), CHUNK).await.unwrap();
    settle().await;
    // The read itself and 128 KiB.
    assert_eq!(server.take_requests(), chunks(256, 3));
}

#[tokio::test(start_paused = true)]
async fn discard_forgets_what_was_fetched() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    settle().await;

    server.overwrite(u64::from(2 * CHUNK), b"new").await;
    reader.discard().await;
    let bytes = reader.read(u64::from(2 * CHUNK), 3).await.unwrap();
    assert_eq!(bytes, &b"new"[..]);
}

#[tokio::test(start_paused = true)]
async fn discard_also_drops_answers_still_on_their_way() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(MIB as usize).await);
    reader.read(0, CHUNK).await.unwrap();
    server.set_delay(RTT);
    reader.read(u64::from(CHUNK), CHUNK).await.unwrap();
    server.set_delay(Duration::ZERO);

    reader.discard().await;
    server.overwrite(u64::from(2 * CHUNK), b"new").await;
    settle().await;
    let bytes = reader.read(u64::from(2 * CHUNK), 3).await.unwrap();
    assert_eq!(bytes, &b"new"[..]);
}

#[tokio::test(start_paused = true)]
async fn a_read_dropped_part_way_leaves_the_reader_correct() {
    let server = Server::new(CHUNK);
    server.set_delay(RTT);
    let reader = reader(&server, server.file(8 * MIB as usize).await);
    let len = 6 * MIB;

    // After one round trip the first 4 MiB are in, and the rest cannot be yet.
    let dropped = timeout(RTT * 3 / 2, reader.read(0, len)).await;
    assert!(dropped.is_err());
    let bytes = reader.read(0, len).await.unwrap();
    assert!(bytes == expected(0, len as usize));
    let more = reader.read(u64::from(len), CHUNK).await.unwrap();
    assert!(more == expected(len as usize, CHUNK as usize));
}

#[tokio::test(start_paused = true)]
async fn close_stops_fetching_and_closes_the_handle() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    reader.read(0, CHUNK).await.unwrap();
    server.set_delay(Duration::from_secs(3600));
    timeout(RTT, reader.read(u64::from(CHUNK), 1))
        .await
        .unwrap_err();
    settle().await;
    assert!(server.running.lock().unwrap().chunks > 0);

    let start = Instant::now();
    reader.close().await.unwrap();
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(server.running.lock().unwrap().chunks, 0);
    assert_eq!(server.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_reader_stops_fetching() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.sparse_file(u64::from(64 * MIB)).await);
    reader.read(0, CHUNK).await.unwrap();
    server.set_delay(Duration::from_secs(3600));
    timeout(RTT, reader.read(u64::from(CHUNK), 1))
        .await
        .unwrap_err();
    settle().await;
    assert!(server.running.lock().unwrap().chunks > 0);

    drop(reader);
    settle().await;
    assert_eq!(server.running.lock().unwrap().chunks, 0);
}

#[tokio::test(start_paused = true)]
async fn the_handle_stays_usable_for_other_calls() {
    let server = Server::new(CHUNK);
    let reader = reader(&server, server.file(1000).await);
    let attr = server.fstat(reader.handle()).await.unwrap();
    assert_eq!(attr.size, 1000);
}

#[tokio::test(start_paused = true)]
async fn a_deadline_below_the_reader_gives_timeout() {
    let server = Server::new(CHUNK);
    let handle = server.file(1000).await;
    server.set_delay(Duration::from_secs(3600));
    let deadline = Duration::from_secs(15);
    let fs = Arc::new(DeadlineFs::new(server.clone(), deadline));
    let suspect = fs.suspect();
    let reader = ReadAhead::new(fs, handle);

    let start = Instant::now();
    assert_eq!(reader.read(0, 1000).await, Err(FsError::Timeout));
    assert_eq!(start.elapsed(), deadline);
    assert_eq!(*suspect.borrow(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_works_behind_dyn_with_overlapping_reads() {
    let server = Server::new(CHUNK);
    let size = 2 * MIB as usize;
    let handle = server.file(size).await;
    let fs: Arc<dyn RemoteFs> = Arc::new(server);
    let reader = Arc::new(ReadAhead::new(fs, handle));

    let mut reads = tokio::task::JoinSet::new();
    for i in 0..32 {
        let reader = Arc::clone(&reader);
        reads.spawn(async move {
            let offset = i * CHUNK as usize;
            let bytes = reader.read(offset as u64, CHUNK).await.unwrap();
            assert_eq!(bytes, expected(offset, CHUNK as usize));
        });
    }
    while let Some(read) = reads.join_next().await {
        read.unwrap();
    }
    Arc::into_inner(reader).unwrap().close().await.unwrap();
}

#[derive(Clone, Debug)]
enum Step {
    Next(u32),
    Jump(u64, u32),
    Discard,
    Wait(u64),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0..300 * KIB).prop_map(Step::Next),
        2 => (0..3 * u64::from(MIB), 0..300 * KIB).prop_map(|(at, len)| Step::Jump(at, len)),
        1 => Just(Step::Discard),
        1 => (0..10_000u64).prop_map(Step::Wait),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever the access pattern and server limits, each read returns exactly what the file holds.
    #[test]
    fn reads_match_the_file(
        steps in proptest::collection::vec(step(), 1..60),
        max_read in prop_oneof![Just(1000), Just(32 * KIB), Just(261_120), Just(8 * MIB)],
        short in proptest::option::of(1..100_000usize),
        delayed in any::<bool>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let size = 2 * MIB as usize;
            let server = Server::new(max_read);
            let reader = reader(&server, server.file(size).await);
            *server.short.lock().unwrap() = short;
            if delayed {
                server.set_delay(RTT);
            }
            let mut position = 0u64;
            for step in steps {
                let (offset, len) = match step {
                    Step::Next(len) => (position, len),
                    Step::Jump(offset, len) => (offset, len),
                    Step::Discard => {
                        reader.discard().await;
                        continue;
                    }
                    Step::Wait(ms) => {
                        sleep(Duration::from_millis(ms)).await;
                        continue;
                    }
                };
                let bytes = reader.read(offset, len).await.unwrap();
                let start = (offset as usize).min(size);
                let end = (start + len as usize).min(size);
                assert_eq!(bytes, expected(start, end - start), "{offset} {len}");
                position = offset + bytes.len() as u64;
            }
            let running = server.running.lock().unwrap();
            assert!(running.peak_chunks <= 64);
            assert!(running.peak_bytes <= u64::from(4 * MIB));
        });
    }
}
