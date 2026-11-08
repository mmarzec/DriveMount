// SPDX-License-Identifier: GPL-3.0-or-later

//! The per-handle reader of spec §5.6: sequential detection, read-ahead and chunked parallel reads.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::sync::Mutex;
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::Instant;

use crate::{FsError, FsResult, RemoteFs, RemoteHandle};

const INITIAL_WINDOW: u64 = 128 * 1024;
const MAX_WINDOW: u64 = 4 * 1024 * 1024;
const MAX_CHUNKS_IN_FLIGHT: usize = 64;
const MAX_BYTES_IN_FLIGHT: u64 = 4 * 1024 * 1024;

/// How long an answer is kept for, counted from when it was asked for: the default of
/// `cache_ttl_secs`. Without a limit, a reader that pauses would later get bytes or an error from
/// any time in the past.
const MAX_AGE: Duration = Duration::from_secs(5);

/// Reads one open file, fetching ahead of the caller when it reads sequentially.
///
/// The first read and any read that does not start where the last one ended fetch just what was
/// asked for. Each read that does continue fetches `window` bytes beyond itself. The window starts at
/// 128 KiB and doubles up to 4 MiB each time the caller has consumed a window's worth, so what is
/// fetched stays in proportion to what is used.
///
/// Requests are at most [`max_read`](crate::Capabilities::max_read) and at most 4 MiB, with up to 64
/// of them and 4 MiB in flight. They run as tasks, so fetching carries on between reads, and nothing
/// runs once the window is full.
///
/// `fs` should apply deadlines: a read waits for as long as the requests it needs do. Requests that
/// fail do no harm unless a read reaches them. Must be used inside a Tokio runtime.
pub struct ReadAhead<F: ?Sized> {
    fs: Arc<F>,
    handle: Arc<RemoteHandle>,
    chunk_size: u32,
    state: Mutex<State>,
}

impl<F: RemoteFs + ?Sized> ReadAhead<F> {
    /// A reader for `handle`, which it keeps until [`close`](Self::close).
    #[must_use]
    pub fn new(fs: Arc<F>, handle: RemoteHandle) -> Self {
        let max_read = fs.capabilities().max_read;
        Self {
            fs,
            handle: Arc::new(handle),
            chunk_size: u32::try_from(MAX_BYTES_IN_FLIGHT)
                .map_or(max_read, |max| max_read.min(max)),
            state: Mutex::new(State {
                position: None,
                window: 0,
                consumed: 0,
                chunks: VecDeque::new(),
                tasks: JoinSet::new(),
            }),
        }
    }

    /// The handle, for calls other than reads. After any change to the file, call
    /// [`discard`](Self::discard).
    #[must_use]
    pub fn handle(&self) -> &RemoteHandle {
        &self.handle
    }

    /// Reads `len` bytes at `offset`. Fewer bytes mean the file ends there, unlike
    /// [`RemoteFs::read`]. If any of the range cannot be read, the whole call fails.
    ///
    /// Overlapping calls take turns. A call may be dropped at any time; the next read then simply
    /// does not count as sequential.
    pub async fn read(&self, offset: u64, len: u32) -> FsResult<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let mut guard = self.state.lock().await;
        let state = &mut *guard;
        state.begin_read(offset);

        let end = offset.saturating_add(u64::from(len));
        let target = end.saturating_add(state.window);
        let mut remaining = len as usize;
        let mut parts = Vec::new();
        while remaining > 0 {
            state.collect_finished();
            self.start_chunks(state, target);
            let Some(chunk) = state.chunks.front_mut() else {
                // The range runs past the largest possible offset.
                break;
            };
            match &mut chunk.progress {
                Progress::Planned | Progress::Running(_) => {
                    match state.tasks.join_next_with_id().await {
                        Some(finished) => state.record(finished),
                        None => {
                            // Not reachable: start_chunks leaves at least one task running here.
                            state.reset();
                            return Err(FsError::Other("read-ahead has no request".to_owned()));
                        }
                    }
                }
                Progress::Done(Err(error)) => {
                    let error = error.clone();
                    state.reset();
                    return Err(error);
                }
                Progress::Done(Ok(bytes)) if bytes.is_empty() => {
                    // End of file. Not worth fetching ahead until the file proves to have grown.
                    state.reset();
                    break;
                }
                Progress::Done(Ok(bytes)) => {
                    let part = bytes.split_to(bytes.len().min(remaining));
                    remaining -= part.len();
                    let taken = part.len() as u64;
                    let answer_used_up = bytes.is_empty();
                    parts.push(part);
                    chunk.offset += taken;
                    chunk.len -= taken as u32;
                    state.position = Some(chunk.offset);
                    state.consumed += taken;
                    if answer_used_up {
                        if chunk.len == 0 {
                            state.chunks.pop_front();
                        } else {
                            // A short answer: the rest must be asked for again.
                            chunk.progress = Progress::Planned;
                            chunk.asked = Instant::now();
                        }
                    }
                }
            }
        }
        Ok(join(parts))
    }

    /// Forgets everything fetched so far. Call this once a change to the file has finished, however
    /// it finished, so that nothing fetched before it can be returned after it.
    pub async fn discard(&self) {
        self.state.lock().await.reset();
    }

    /// Stops all fetching and closes the handle. The handle is gone even if this fails.
    pub async fn close(self) -> FsResult<()> {
        let Self {
            fs, handle, state, ..
        } = self;
        state.into_inner().tasks.shutdown().await;
        // Every other reference was held by a task, and none is left.
        match Arc::into_inner(handle) {
            Some(handle) => fs.close(handle).await,
            None => Err(FsError::Other("read-ahead handle still in use".to_owned())),
        }
    }

    /// Starts as many requests as the limits allow, oldest first, planning new ones up to `target`.
    /// New ones wait until a good size is due, or a caller making small reads would send one small
    /// request per read. The window is at least twice that size, so the range a read needs is always
    /// due.
    fn start_chunks(&self, state: &mut State, target: u64) {
        let State {
            position,
            window,
            chunks,
            tasks,
            ..
        } = state;
        let mut in_flight = chunks
            .iter()
            .filter(|chunk| matches!(chunk.progress, Progress::Running(_)))
            .fold((0, 0), |(count, bytes), chunk| {
                (count + 1, bytes + u64::from(chunk.len))
            });
        let has_room = |(count, bytes): (usize, u64), len: u32| {
            count < MAX_CHUNKS_IN_FLIGHT && bytes + u64::from(len) <= MAX_BYTES_IN_FLIGHT
        };

        for chunk in chunks.iter_mut() {
            if matches!(chunk.progress, Progress::Planned) {
                if !has_room(in_flight, chunk.len) {
                    return;
                }
                chunk.progress = Progress::Running(self.spawn(tasks, chunk.offset, chunk.len));
                in_flight = (in_flight.0 + 1, in_flight.1 + u64::from(chunk.len));
            }
        }

        let good_size = u64::from(self.chunk_size).min(*window / 2);
        let mut next = chunks
            .back()
            .map(|chunk| chunk.offset + u64::from(chunk.len))
            .or(*position)
            .unwrap_or(target);
        while next < target && target - next >= good_size {
            let len = u64::from(self.chunk_size).min(target - next);
            let len = u32::try_from(len).unwrap_or(self.chunk_size);
            if !has_room(in_flight, len) {
                return;
            }
            chunks.push_back(Chunk {
                offset: next,
                len,
                asked: Instant::now(),
                progress: Progress::Running(self.spawn(tasks, next, len)),
            });
            in_flight = (in_flight.0 + 1, in_flight.1 + u64::from(len));
            next += u64::from(len);
        }
    }

    fn spawn(&self, tasks: &mut JoinSet<FsResult<Bytes>>, offset: u64, len: u32) -> task::Id {
        let fs = Arc::clone(&self.fs);
        let handle = Arc::clone(&self.handle);
        tasks
            .spawn(async move { fs.read(&handle, offset, len).await })
            .id()
    }
}

/// What the reader knows. `chunks` covers one unbroken range starting at `position`.
struct State {
    /// Where a sequential read would start.
    position: Option<u64>,
    /// How far beyond each read to fetch; 0 while access does not look sequential.
    window: u64,
    /// Bytes returned since the window last changed.
    consumed: u64,
    chunks: VecDeque<Chunk>,
    tasks: JoinSet<FsResult<Bytes>>,
}

struct Chunk {
    offset: u64,
    len: u32,
    asked: Instant,
    progress: Progress,
}

enum Progress {
    /// To be requested as soon as the limits allow.
    Planned,
    Running(task::Id),
    Done(FsResult<Bytes>),
}

impl State {
    fn begin_read(&mut self, offset: u64) {
        if self.position != Some(offset) {
            self.reset();
            self.position = Some(offset);
        } else if self.is_stale() {
            // Fetched faster than it is used: a smaller window suits this reader.
            self.reset();
            self.position = Some(offset);
            self.window = INITIAL_WINDOW;
        } else if self.window == 0 {
            self.window = INITIAL_WINDOW;
            self.consumed = 0;
        } else if self.consumed >= self.window {
            self.window = (self.window * 2).min(MAX_WINDOW);
            self.consumed = 0;
        }
    }

    fn is_stale(&self) -> bool {
        self.chunks
            .front()
            .is_some_and(|chunk| chunk.asked.elapsed() >= MAX_AGE)
    }

    /// Back to how a new reader starts. Answers to requests already sent are ignored.
    fn reset(&mut self) {
        self.tasks.abort_all();
        self.chunks.clear();
        self.position = None;
        self.window = 0;
        self.consumed = 0;
    }

    fn collect_finished(&mut self) {
        while let Some(finished) = self.tasks.try_join_next_with_id() {
            self.record(finished);
        }
    }

    fn record(&mut self, finished: Result<(task::Id, FsResult<Bytes>), JoinError>) {
        let (id, mut result) = match finished {
            Ok(finished) => finished,
            Err(error) => (error.id(), Err(FsError::Other(error.to_string()))),
        };
        let chunk = self
            .chunks
            .iter_mut()
            .find(|chunk| matches!(chunk.progress, Progress::Running(running) if running == id));
        if let Some(chunk) = chunk {
            if let Ok(bytes) = &mut result {
                bytes.truncate(chunk.len as usize);
            }
            chunk.progress = Progress::Done(result);
        }
    }
}

/// One piece is returned as it is, which is the usual case and copies nothing.
fn join(mut parts: Vec<Bytes>) -> Bytes {
    if parts.len() <= 1 {
        return parts.pop().unwrap_or_default();
    }
    let mut joined = BytesMut::with_capacity(parts.iter().map(Bytes::len).sum());
    for part in parts {
        joined.extend_from_slice(&part);
    }
    joined.freeze()
}
