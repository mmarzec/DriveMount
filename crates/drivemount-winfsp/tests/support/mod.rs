// SPDX-License-Identifier: GPL-3.0-or-later

//! A mounted drive over a `MemFs`, with a layer between them that counts calls, makes them slow or
//! makes them fail, and adds what `MemFs` lacks: symbolic links.
//!
//! Setting `DRIVEMOUNT_TEST_SERVER=sftp` puts a real SFTP session in between (spec §10.3): `SftpFs`, SSH and
//! a socket, down to a server in this process over that same layer. Tests that count remote calls
//! see what reaches the server, which can differ from what the drive asks for.

// Each test binary uses its own part of this.
#![allow(dead_code)]

pub mod exerciser;
pub mod proxy;
pub mod sftp_server;

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use drivemount_sftp::connector::{Auth, SftpConnector, SftpProfile};
use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, Capabilities, Connector, DirEntry, Disposition,
    DriveLetter, FileKind, FsError, FsResult, FsStats, HostKeyDecision, HostKeyRequest,
    KbdIntRequest, LoginAnswer, LoginRequest, MemFs, MountError, OpenMode, Prompter, Remember,
    RemoteFs, RemoteHandle, RemotePath, Secret, SecretRequest, async_trait,
};
use drivemount_win::drives::is_letter_free;
use drivemount_winfsp::{Volume, VolumeConfig};
#[allow(unused_imports)] // Each test binary uses its own part of this.
pub use exerciser::MappedFile;
use sftp_server::SftpServer;
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
    Write,
    Flush,
    SetAttr,
    Mkdir,
    RemoveFile,
    RemoveDir,
    Rename,
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
    /// Remote handles open now, and whether a file was removed while any was.
    open_handles: AtomicU64,
    watching_removals: AtomicBool,
    removed_while_open: AtomicBool,
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

    /// From now on, notes any file removed while a remote handle is open.
    pub fn require_closed_before_remove(&self) {
        self.watching_removals.store(true, Ordering::SeqCst);
    }

    pub fn removed_while_open(&self) -> bool {
        self.removed_while_open.load(Ordering::SeqCst)
    }

    /// Remote handles open now.
    pub fn open_handles(&self) -> u64 {
        self.open_handles.load(Ordering::SeqCst)
    }

    /// Stops failing this call.
    pub fn heal(&self, op: Op) {
        self.failures.lock().unwrap().remove(&op);
    }

    /// Creates a file straight on the `MemFs`, as another client of the server would.
    pub fn inner_put(&self, path: &str, data: &[u8]) {
        block_on_here(async {
            let mode = OpenMode::write(Disposition::CreateOrTruncate);
            let handle = self.inner.open(&remote(path), mode).await.unwrap();
            let max = self.inner.capabilities().max_write as usize;
            for (index, chunk) in data.chunks(max).enumerate() {
                let offset = (index * max) as u64;
                let chunk = Bytes::copy_from_slice(chunk);
                self.inner.write(&handle, offset, chunk).await.unwrap();
            }
            self.inner.close(handle).await.unwrap();
        });
    }

    /// What another client of the server would read.
    pub fn inner_get(&self, path: &str) -> Vec<u8> {
        block_on_here(async {
            let handle = self
                .inner
                .open(&remote(path), OpenMode::read())
                .await
                .unwrap();
            let mut data = Vec::new();
            loop {
                let offset = data.len() as u64;
                let bytes = self.inner.read(&handle, offset, 1 << 20).await.unwrap();
                if bytes.is_empty() {
                    break;
                }
                data.extend_from_slice(&bytes);
            }
            self.inner.close(handle).await.unwrap();
            data
        })
    }

    /// Whether the server has this path.
    pub fn has(&self, path: &str) -> bool {
        block_on_here(self.inner.stat(&remote(path))).is_ok()
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
        if self.links.lock().unwrap().contains_key(path.as_str()) {
            return Ok(link_attr());
        }
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
                    attr: link_attr(),
                });
            }
        }
        Ok(entries)
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.enter(Op::Open).await?;
        let handle = self.inner.open(&self.follow(path), mode).await?;
        self.open_handles.fetch_add(1, Ordering::SeqCst);
        Ok(handle)
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.enter(Op::Read).await?;
        self.inner.read(handle, offset, len).await
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.enter(Op::Write).await?;
        self.inner.write(handle, offset, data).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.enter(Op::Flush).await?;
        self.inner.flush(handle).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.enter(Op::Close).await?;
        self.open_handles.fetch_sub(1, Ordering::SeqCst);
        self.inner.close(handle).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.enter(Op::SetAttr).await?;
        self.inner.set_attr(target, changes).await
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.enter(Op::Mkdir).await?;
        self.inner.mkdir(path).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.enter(Op::RemoveFile).await?;
        if self.links.lock().unwrap().remove(path.as_str()).is_some() {
            return Ok(());
        }
        if self.watching_removals.load(Ordering::SeqCst)
            && self.open_handles.load(Ordering::SeqCst) > 0
        {
            self.removed_while_open.store(true, Ordering::SeqCst);
        }
        self.inner.remove_file(path).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.enter(Op::RemoveDir).await?;
        self.inner.remove_dir(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.enter(Op::Rename).await?;
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

/// What a listing and `lstat` say about a link.
fn link_attr() -> Attr {
    Attr {
        kind: FileKind::Symlink,
        size: 7,
        perm: Some(0o777),
        mtime: Some(SystemTime::UNIX_EPOCH),
        atime: Some(SystemTime::UNIX_EPOCH),
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

/// A drive that goes away with this value.
pub struct Drive {
    pub probe: Arc<Probe>,
    pub backend: Volume,
    pub letter: DriveLetter,
    /// With `DRIVEMOUNT_TEST_SERVER=sftp`, what stands between the drive and the probe.
    pub sftp: Option<Sftp>,
    runtime: Runtime,
    _turn: DriveTurn,
}

/// A real SFTP session, and the server at the other end of it.
pub struct Sftp {
    pub server: SftpServer,
    pub connector: SftpConnector,
    _known_hosts: tempfile::TempDir,
}

impl Sftp {
    /// A server for `probe` on the current runtime, and a connector to it that has never met it.
    pub async fn start(probe: Arc<Probe>, op_timeout: Duration) -> Self {
        let server = SftpServer::start(probe).await;
        let known_hosts = tempfile::tempdir().unwrap();
        let connector = SftpConnector::new(SftpProfile {
            host: "127.0.0.1".to_owned(),
            port: server.port,
            user: "tester".to_owned(),
            auth: Auth::Password(Some(Secret::from("anything".to_owned()))),
            remote_path: "/".to_owned(),
            keepalive_secs: 0,
            timeout: op_timeout,
            known_hosts: known_hosts.path().join("known_hosts"),
            jump: None,
        });
        Self {
            server,
            connector,
            _known_hosts: known_hosts,
        }
    }
}

/// Trusts any host key, and asks nothing else: a password comes with the connector.
#[derive(Default)]
pub struct TrustOnce {
    pub host_keys_asked: AtomicU64,
}

#[async_trait]
impl Prompter for TrustOnce {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        panic!("no secret should be asked for")
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        panic!("no login should be asked for")
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        panic!("no keyboard-interactive round should be asked for")
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        self.host_keys_asked.fetch_add(1, Ordering::SeqCst);
        HostKeyDecision::TrustOnce
    }
}

/// Whether this run puts a real SFTP session under its drives.
pub fn over_sftp() -> bool {
    std::env::var("DRIVEMOUNT_TEST_SERVER").is_ok_and(|server| server == "sftp")
}

/// For a test whose fault cannot reach the drive through a real session, so that a run over SFTP does
/// not fail for that: an error kind that does not cross the wire, or a hung request, which a server
/// that answers in order (this one, and OpenSSH's) never gets past. Returns whether to skip.
pub fn not_over_sftp(reason: &str) -> bool {
    let skip = over_sftp();
    if skip {
        println!("skipped over SFTP: {reason}");
    }
    skip
}

/// The same time, as far as the transport can tell: SFTP v3 times are whole seconds.
pub fn same_time(left: SystemTime, right: SystemTime) -> bool {
    let seconds = |time: SystemTime| {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    if over_sftp() {
        seconds(left) == seconds(right)
    } else {
        left == right
    }
}

/// A slug no other drive of this machine has right now.
pub fn unique_slug() -> String {
    format!(
        "t{}-{}",
        std::process::id(),
        NEXT_SLUG.fetch_add(1, Ordering::Relaxed)
    )
}

/// The ways a test's drive differs from the usual one.
pub struct Options {
    pub label: String,
    pub op_timeout: Duration,
    pub cache_ttl: Duration,
    pub read_only: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            label: "DriveMount Test".to_owned(),
            op_timeout: Duration::from_secs(30),
            cache_ttl: Duration::from_secs(5),
            read_only: false,
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
        let (fs, sftp): (Arc<dyn RemoteFs>, Option<Sftp>) = if over_sftp() {
            runtime.block_on(async {
                let sftp = Sftp::start(Arc::clone(&probe), options.op_timeout).await;
                let fs = sftp.connector.connect(&TrustOnce::default()).await.unwrap();
                (fs, Some(sftp))
            })
        } else {
            (Arc::clone(&probe) as Arc<dyn RemoteFs>, None)
        };
        let config = VolumeConfig {
            slug: unique_slug(),
            label: options.label,
            file_system_name: "DM-SFTP".to_owned(),
            op_timeout: options.op_timeout,
            cache_ttl: options.cache_ttl,
            threads: 0,
            read_only: options.read_only,
        };
        let backend = Volume::new(fs, config, runtime.handle().clone());
        let letter = start_on_a_free_letter(&backend);
        Self {
            probe,
            backend,
            letter,
            sftp,
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

    /// What another client of the server would read.
    pub fn get(&self, path: &str) -> Vec<u8> {
        self.server(async {
            let fs = &self.probe.inner;
            let handle = fs.open(&remote(path), OpenMode::read()).await.unwrap();
            let mut data = Vec::new();
            loop {
                let offset = data.len() as u64;
                let bytes = fs.read(&handle, offset, 1 << 20).await.unwrap();
                if bytes.is_empty() {
                    break;
                }
                data.extend_from_slice(&bytes);
            }
            fs.close(handle).await.unwrap();
            data
        })
    }

    /// What the server says about a path, or `None` if it is not there.
    pub fn attr(&self, path: &str) -> Option<Attr> {
        self.server(self.probe.inner.stat(&remote(path))).ok()
    }

    /// Creates a file with these contents that the server shows as read-only.
    pub fn put_read_only(&self, path: &str, data: &[u8]) {
        self.put(path, data);
        let changes = AttrChanges {
            read_only: Some(true),
            ..AttrChanges::default()
        };
        self.server(
            self.probe
                .inner
                .set_attr(AttrTarget::Path(&remote(path)), changes),
        )
        .unwrap();
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
pub fn start_on_a_free_letter(backend: &Volume) -> DriveLetter {
    try_start_on_a_free_letter(backend).unwrap()
}

/// Tries to start a drive on whatever letter is free, one test at a time.
pub fn try_start_on_a_free_letter(backend: &Volume) -> Result<DriveLetter, MountError> {
    let _picking = PICKING_A_LETTER
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let letter = free_letter();
    backend.start(letter).map(|()| letter)
}

pub fn free_letter() -> DriveLetter {
    ('G'..='Y')
        .rev()
        .filter_map(DriveLetter::new)
        .find(|letter| is_letter_free(*letter).unwrap())
        .expect("no free drive letter for the test")
}

/// `MemFs` never waits for anything, so its calls can finish right here without a runtime.
fn block_on_here<T>(call: impl Future<Output = T>) -> T {
    futures::executor::block_on(call)
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
