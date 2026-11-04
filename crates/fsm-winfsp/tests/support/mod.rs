// SPDX-License-Identifier: GPL-3.0-or-later

//! A mounted drive over a `MemFs`, with a layer between them that counts calls, makes them slow or
//! makes them fail, and adds what `MemFs` lacks: symbolic links.

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use fsm_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, Capabilities, DirEntry, Disposition, DriveLetter,
    FileKind, FsError, FsResult, FsStats, MemFs, MountError, OpenMode, RemoteFs, RemoteHandle,
    RemotePath, async_trait,
};
use fsm_win::drives::is_letter_free;
use fsm_winfsp::{VolumeConfig, WinFspBackend};
use tokio::runtime::Runtime;
use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
};
use windows::core::PCWSTR;

/// A remote call the tests can count, slow down or fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Stat,
    Fstat,
    ReadDir,
    Open,
    Read,
    Close,
    Statfs,
}

/// `MemFs`, plus counters, delays, failures and symbolic links.
#[derive(Default)]
pub struct Probe {
    pub inner: MemFs,
    counts: Mutex<HashMap<Op, u64>>,
    /// Listings per directory. Other programs may look at a new drive's root at any time, so a count
    /// over all directories would not say what a test did.
    listings: Mutex<HashMap<String, u64>>,
    delays: Mutex<HashMap<Op, Duration>>,
    failures: Mutex<HashMap<Op, FsError>>,
    /// Link path → target. Created by `add_link`; the target need not exist.
    links: Mutex<HashMap<String, String>>,
}

impl Probe {
    pub fn count(&self, op: Op) -> u64 {
        self.counts.lock().unwrap().get(&op).copied().unwrap_or(0)
    }

    pub fn listings_of(&self, path: &str) -> u64 {
        self.listings
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    pub fn delay(&self, op: Op, delay: Duration) {
        self.delays.lock().unwrap().insert(op, delay);
    }

    pub fn fail(&self, op: Op, error: FsError) {
        self.failures.lock().unwrap().insert(op, error);
    }

    pub fn add_link(&self, link: &str, target: &str) {
        self.links
            .lock()
            .unwrap()
            .insert(link.to_owned(), target.to_owned());
    }

    /// Runs what every call has in common: count, then wait, then maybe fail.
    async fn enter(&self, op: Op) -> FsResult<()> {
        *self.counts.lock().unwrap().entry(op).or_insert(0) += 1;
        let delay = self.delays.lock().unwrap().get(&op).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let failure = self.failures.lock().unwrap().get(&op).cloned();
        failure.map_or(Ok(()), Err)
    }

    /// Follows links in a path, as a server does.
    fn follow(&self, path: &RemotePath) -> RemotePath {
        let links = self.links.lock().unwrap();
        let mut current = path.as_str().to_owned();
        for (link, target) in links.iter() {
            if current == *link {
                current.clone_from(target);
            } else if let Some(rest) = current.strip_prefix(&format!("{link}/")) {
                current = format!("{target}/{rest}");
            }
        }
        RemotePath::new(&current).unwrap()
    }
}

#[async_trait]
impl RemoteFs for Probe {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.enter(Op::Stat).await?;
        self.inner.stat(&self.follow(path)).await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.inner.lstat(path).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.enter(Op::Fstat).await?;
        self.inner.fstat(handle).await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        *self
            .listings
            .lock()
            .unwrap()
            .entry(path.as_str().to_owned())
            .or_insert(0) += 1;
        self.enter(Op::ReadDir).await?;
        let followed = self.follow(path);
        let mut entries = self.inner.read_dir(&followed).await?;
        let links: Vec<String> = self.links.lock().unwrap().keys().cloned().collect();
        for link in links {
            let link = RemotePath::new(&link).unwrap();
            if link.parent().as_ref() == Some(&followed) {
                entries.push(DirEntry {
                    name: link.file_name().unwrap().to_owned(),
                    attr: Attr {
                        kind: FileKind::Symlink,
                        size: 7,
                        perm: Some(0o777),
                        mtime: Some(SystemTime::UNIX_EPOCH),
                        atime: Some(SystemTime::UNIX_EPOCH),
                    },
                });
            }
        }
        Ok(entries)
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.enter(Op::Open).await?;
        self.inner.open(&self.follow(path), mode).await
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.enter(Op::Read).await?;
        self.inner.read(handle, offset, len).await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.inner.write(handle, offset, data).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.inner.flush(handle).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.enter(Op::Close).await?;
        self.inner.close(handle).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.inner.set_attr(target, changes).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.inner.mkdir(path).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.inner.remove_file(path).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.inner.remove_dir(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.inner.rename(from, to, replace).await
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        self.enter(Op::Statfs).await?;
        self.inner.statfs().await
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

/// Tests run side by side, and a letter is only known to be free until someone mounts on it.
static PICKING_A_LETTER: Mutex<()> = Mutex::new(());
static NEXT_SLUG: AtomicU64 = AtomicU64::new(0);

/// How many drives may be up at once: enough for tests to overlap, and few enough to leave letters
/// over on any machine, and not to fill the Explorer window of whoever is running them.
const MAX_DRIVES: usize = 4;
static DRIVES_UP: Mutex<usize> = Mutex::new(0);
static DRIVE_WENT_AWAY: Condvar = Condvar::new();

/// A turn to have a drive up, given back on drop.
struct DriveTurn;

impl DriveTurn {
    fn wait() -> Self {
        let mut up = DRIVES_UP.lock().unwrap_or_else(PoisonError::into_inner);
        while *up >= MAX_DRIVES {
            up = DRIVE_WENT_AWAY
                .wait(up)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *up += 1;
        Self
    }
}

impl Drop for DriveTurn {
    fn drop(&mut self) {
        *DRIVES_UP.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        DRIVE_WENT_AWAY.notify_one();
    }
}

/// A read-only drive that goes away with this value.
pub struct Drive {
    pub probe: Arc<Probe>,
    pub backend: WinFspBackend,
    pub letter: DriveLetter,
    runtime: Runtime,
    _turn: DriveTurn,
}

/// The ways a test's drive differs from the usual one.
pub struct Options {
    pub label: String,
    pub op_timeout: Duration,
    pub cache_ttl: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            label: "FSM Test".to_owned(),
            op_timeout: Duration::from_secs(30),
            cache_ttl: Duration::from_secs(5),
        }
    }
}

impl Drive {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    pub fn with_options(options: Options) -> Self {
        let turn = DriveTurn::wait();
        let runtime = Runtime::new().unwrap();
        let probe = Arc::new(Probe::default());
        let fs: Arc<dyn RemoteFs> = Arc::clone(&probe) as Arc<dyn RemoteFs>;
        let config = VolumeConfig {
            slug: format!(
                "t{}-{}",
                std::process::id(),
                NEXT_SLUG.fetch_add(1, Ordering::Relaxed)
            ),
            label: options.label,
            file_system_name: "FSM-SFTP".to_owned(),
            op_timeout: options.op_timeout,
            cache_ttl: options.cache_ttl,
            threads: 0,
        };
        let backend = WinFspBackend::new(fs, config, runtime.handle().clone());
        let letter = start_on_a_free_letter(&backend);
        Self {
            probe,
            backend,
            letter,
            runtime,
            _turn: turn,
        }
    }

    /// A path on the drive, from one that starts at its root, such as `dir\file.txt`.
    pub fn path(&self, relative: &str) -> PathBuf {
        PathBuf::from(format!(r"{}\{relative}", self.letter))
    }

    /// Runs a call straight on the `MemFs`, as another client of the server would.
    pub fn server<T>(&self, call: impl Future<Output = T>) -> T {
        self.runtime.block_on(call)
    }

    pub fn mkdir(&self, path: &str) {
        self.server(self.probe.inner.mkdir(&remote(path))).unwrap();
    }

    /// Creates a file with these contents, or replaces the contents of one.
    pub fn put(&self, path: &str, data: &[u8]) {
        self.server(async {
            let mode = OpenMode::write(Disposition::CreateOrTruncate);
            let handle = self.probe.inner.open(&remote(path), mode).await.unwrap();
            let max = self.probe.inner.capabilities().max_write as usize;
            for (index, chunk) in data.chunks(max).enumerate() {
                let offset = (index * max) as u64;
                let chunk = Bytes::copy_from_slice(chunk);
                self.probe
                    .inner
                    .write(&handle, offset, chunk)
                    .await
                    .unwrap();
            }
            self.probe.inner.close(handle).await.unwrap();
        });
    }
}

impl Drop for Drive {
    fn drop(&mut self) {
        self.backend.stop();
    }
}

/// Starts a drive on whatever letter is free, one test at a time.
pub fn start_on_a_free_letter(backend: &WinFspBackend) -> DriveLetter {
    try_start_on_a_free_letter(backend).unwrap()
}

/// Tries to start a drive on whatever letter is free, one test at a time.
pub fn try_start_on_a_free_letter(backend: &WinFspBackend) -> Result<DriveLetter, MountError> {
    let _picking = PICKING_A_LETTER
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let letter = free_letter();
    backend.start(letter).map(|()| letter)
}

fn free_letter() -> DriveLetter {
    ('G'..='Y')
        .rev()
        .filter_map(DriveLetter::new)
        .find(|letter| is_letter_free(*letter).unwrap())
        .expect("no free drive letter for the test")
}

pub fn remote(path: &str) -> RemotePath {
    RemotePath::new(path).unwrap()
}

/// Bytes that differ from one offset to the next and from one seed to the next.
pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3] ^ seed ^ (i >> 16) as u8)
        .collect()
}

/// Waits for something that happens a moment after what the test did, and says if it ever did.
pub fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

pub fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// What `FindFirstFileW` and `FindNextFileW` say about one entry.
#[derive(Debug, Clone)]
pub struct Found {
    pub name: String,
    pub attributes: u32,
    pub size: u64,
}

/// What `FindFirstFileW` alone finds. Going on to `FindNextFileW` makes the driver read the directory,
/// even after a search for one exact name.
pub fn find_first(pattern: &Path) -> std::io::Result<Found> {
    let pattern = wide(pattern);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated, and both outlive the call.
    let search = unsafe { FindFirstFileW(PCWSTR(pattern.as_ptr()), &raw mut data) }
        .map_err(|error| std::io::Error::from_raw_os_error(error.code().0 & 0xFFFF))?;
    // SAFETY: the search handle is open, and is not used after this.
    unsafe { FindClose(search) }.unwrap();
    Ok(found_entry(&data))
}

/// Everything a search finds, or the Win32 error of `FindFirstFileW`.
pub fn find(pattern: &Path) -> std::io::Result<Vec<Found>> {
    let pattern = wide(pattern);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated, and both outlive the call.
    let search = unsafe { FindFirstFileW(PCWSTR(pattern.as_ptr()), &raw mut data) }
        .map_err(|error| std::io::Error::from_raw_os_error(error.code().0 & 0xFFFF))?;
    let mut found = vec![found_entry(&data)];
    loop {
        // SAFETY: the search handle is open, and `data` outlives the call.
        match unsafe { FindNextFileW(search, &raw mut data) } {
            Ok(()) => found.push(found_entry(&data)),
            Err(error) => {
                assert_eq!(error.code(), ERROR_NO_MORE_FILES.to_hresult());
                break;
            }
        }
    }
    // SAFETY: the search handle is open, and is not used after this.
    unsafe { FindClose(search) }.unwrap();
    Ok(found)
}

fn found_entry(data: &WIN32_FIND_DATAW) -> Found {
    let len = data
        .cFileName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(data.cFileName.len());
    Found {
        name: String::from_utf16(&data.cFileName[..len]).unwrap(),
        attributes: data.dwFileAttributes,
        size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
    }
}

/// A `HANDLE` for APIs the standard library has no wrapper for.
pub fn raw_handle(file: &std::fs::File) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    HANDLE(file.as_raw_handle())
}
