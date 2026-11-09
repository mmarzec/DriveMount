// SPDX-License-Identifier: GPL-3.0-or-later

//! The WinFsp callbacks (spec §5.8).

use std::ffi::c_void;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, CachedFs, DeadlineFs, DirEntry, Disposition, FileKind,
    FsError, FsResult, FsStats, OpenMode, ReadAhead, RemoteFs, RemoteHandle, RemotePath, Resolved,
    SessionFs, remote_to_windows, resolve_name, windows_to_remote,
};
use futures::{StreamExt, TryStreamExt, stream};
use tokio::runtime::Handle;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::time::timeout;
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_IO_TIMEOUT, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_NETWORK_UNREACHABLE, STATUS_NOT_A_DIRECTORY, STATUS_NOT_SUPPORTED,
    STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext,
    ModificationDescriptor, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use crate::status::fail;
use crate::{attrs, paths, security};

/// The deadline layer, which sits below the caches so that a call that timed out still invalidates
/// them (spec §5.6), and above the session in use, so that waiting for a reconnect has the same
/// limit as any call (FR-REC-04).
pub(crate) type Deadline = DeadlineFs<Arc<SessionFs>>;
/// The file system the callbacks use.
pub(crate) type Stack = CachedFs<Arc<Deadline>>;

/// How long `statfs` answers are kept (spec §5.8).
const VOLUME_INFO_TTL: Duration = Duration::from_secs(30);

/// How long a failed read may wait to clean up after itself, on top of its operation timeout.
const DISCARD_LIMIT: Duration = Duration::from_millis(50);

/// How many symbolic links of one listing are followed at a time.
const LINKS_AT_A_TIME: usize = 16;

/// The longest name WinFsp can be given, in UTF-16 units: `MaxComponentLength`, and `DirInfo`'s size.
const MAX_NAME: usize = 255;

/// `create_options` asks for a directory.
const FILE_DIRECTORY_FILE: u32 = 0x1;

/// How much of one `Write` goes out at a time: the limits reads have (spec §5.6).
const MAX_CHUNKS_IN_FLIGHT: usize = 64;
const MAX_BYTES_IN_FLIGHT: usize = 4 * 1024 * 1024;

/// How many files left open are flushed and closed at a time when a drive stops.
const FILES_CLOSED_AT_A_TIME: usize = 16;

/// Set in `file_attributes` of `SetBasicInfo` to leave them as they are.
const ATTRIBUTES_UNCHANGED: u32 = u32::MAX;

fn status(status: NTSTATUS) -> FspError {
    status.into()
}

/// One open file or directory, per Windows handle.
///
/// The remote handles are opened on first use (spec §5.8): plenty of opens only look at attributes,
/// and a server allows only so many open files. Reading and writing have one each, so that a handle
/// Windows opened for both still works on a file the server lets this user only read or only write.
pub(crate) struct OpenFile {
    /// Where the file is now, following renames through the drive.
    path: Mutex<RemotePath>,
    is_dir: bool,
    /// The handle for reads and its read-ahead state. Reads take turns anyway.
    reader: tokio::sync::Mutex<Option<ReadAhead<Stack>>>,
    /// Something changed this file, so what `reader` fetched before now must not be returned. Taken
    /// by the next read, so that a change never waits for a read of the same file.
    stale: AtomicBool,
    /// The handle for writes. Overlapping writes can share it.
    writer: RwLock<Option<RemoteHandle>>,
    /// The listing as of the start of an enumeration, so that later pages neither list the directory
    /// again nor see it change.
    listing: DirBuffer,
}

impl OpenFile {
    fn path(&self) -> RemotePath {
        self.path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Where a Windows path for something new leads.
struct Place {
    /// The parent as it is on the server, with the name as Windows spelled it.
    spelled: RemotePath,
    /// What the name already means, which may differ from `spelled` by case.
    existing: Option<Found>,
}

/// Where a Windows path led.
struct Found {
    path: RemotePath,
    attr: Attr,
}

/// The file system WinFsp calls.
pub(crate) struct Adapter {
    pub(crate) fs: Arc<Stack>,
    pub(crate) deadline: Arc<Deadline>,
    pub(crate) sessions: Arc<SessionFs>,
    pub(crate) op_timeout: Duration,
    pub(crate) read_only: bool,
    pub(crate) runtime: Handle,
    pub(crate) label: String,
    pub(crate) descriptor: Vec<u8>,
    pub(crate) volume: Mutex<Option<(Instant, FsStats)>>,
    /// Every open file, so that a change through one handle reaches the others. A handful usually,
    /// so going through all of them is cheap next to the remote call behind each change. Shared with
    /// the volume, as WinFsp does not call `Close` for what is still open when a drive stops.
    pub(crate) open_files: Arc<Mutex<Vec<Arc<OpenFile>>>>,
}

/// Closes a file's remote handles. Both go even if closing one fails.
async fn release(fs: &Stack, file: &OpenFile) -> FsResult<()> {
    let reader = file.reader.lock().await.take();
    let writer = file.writer.write().await.take();
    let reader = match reader {
        Some(reader) => reader.close().await,
        None => Ok(()),
    };
    let writer = match writer {
        Some(writer) => fs.close(writer).await,
        None => Ok(()),
    };
    reader.and(writer)
}

/// Flushes and closes what programs still had open when the drive stopped (FR-MNT-10). What fails
/// goes to the log: the drive has gone by now, and nobody is left to tell.
pub(crate) async fn close_left_open(fs: &Stack, files: Vec<Arc<OpenFile>>) {
    stream::iter(files)
        .for_each_concurrent(FILES_CLOSED_AT_A_TIME, |file| async move {
            let path = file.path();
            if let Some(writer) = file.writer.read().await.as_ref()
                && let Err(error) = fs.flush(writer).await
            {
                warn!(%error, path = %path.as_str(), "flushing failed as the drive stopped");
            }
            if let Err(error) = release(fs, &file).await {
                warn!(%error, path = %path.as_str(), "closing failed as the drive stopped");
            }
        })
        .await;
}

impl Adapter {
    /// Runs `work` to its end on this dispatcher thread, for at most the operation timeout in all,
    /// however many remote calls it makes (NFR-REL-03).
    ///
    /// Running out of time while there is no connection is the network being unreachable, not a
    /// server slow to answer (FR-REC-04), and no reason to probe the connection.
    fn run<T>(&self, work: impl Future<Output = winfsp::Result<T>>) -> winfsp::Result<T> {
        let result = self.runtime.block_on(async {
            timeout(self.op_timeout, work).await.unwrap_or_else(|_| {
                if self.sessions.current().is_some() {
                    self.deadline.mark_suspect();
                }
                Err(status(STATUS_IO_TIMEOUT))
            })
        });
        match result {
            Err(error)
                if error.to_ntstatus() == STATUS_IO_TIMEOUT.0
                    && self.sessions.current().is_none() =>
            {
                Err(status(STATUS_NETWORK_UNREACHABLE))
            }
            result => result,
        }
    }

    /// What every change gets on a read-only drive. WinFsp's read-only flag only tells programs that
    /// the volume is read-only: its driver still passes their changes down, so refusing them is up to
    /// us (FR-MNT-07). Opening a file with write access does work, because a program asking for
    /// `MAXIMUM_ALLOWED` must not be turned away.
    fn check_writable(&self) -> winfsp::Result<()> {
        if self.read_only {
            Err(status(STATUS_MEDIA_WRITE_PROTECTED))
        } else {
            Ok(())
        }
    }

    fn open_files(&self) -> MutexGuard<'_, Vec<Arc<OpenFile>>> {
        self.open_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the context of a newly opened file.
    fn track(&self, path: RemotePath, is_dir: bool, writer: Option<RemoteHandle>) -> Arc<OpenFile> {
        let file = Arc::new(OpenFile {
            path: Mutex::new(path),
            is_dir,
            reader: tokio::sync::Mutex::new(None),
            stale: AtomicBool::new(false),
            writer: RwLock::new(writer),
            listing: DirBuffer::new(),
        });
        self.open_files().push(Arc::clone(&file));
        file
    }

    /// Tells every reader of `path` that what it fetched so far is out of date (spec §5.6). Called
    /// however the change ended: a call that failed may still have reached the server.
    fn contents_changed(&self, path: &RemotePath) {
        for file in self.open_files().iter() {
            if *file.path.lock().unwrap_or_else(PoisonError::into_inner) == *path {
                file.stale.store(true, Ordering::Release);
            }
        }
    }

    /// Open files follow a rename, whether of the file itself or of a directory above it.
    fn moved(&self, from: &RemotePath, to: &RemotePath) {
        for file in self.open_files().iter() {
            let mut path = file.path.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(moved) = moved_path(&path, from, to) {
                *path = moved;
            }
        }
    }

    /// The handle for writes, opened if this is the first.
    async fn writer<'a>(&self, file: &'a OpenFile) -> FsResult<RwLockReadGuard<'a, RemoteHandle>> {
        if let Ok(writer) = RwLockReadGuard::try_map(file.writer.read().await, Option::as_ref) {
            return Ok(writer);
        }
        let mut slot = file.writer.write().await;
        if slot.is_none() {
            let mode = OpenMode::write(Disposition::OpenExisting);
            *slot = Some(self.fs.open(&file.path(), mode).await?);
        }
        RwLockWriteGuard::try_downgrade_map(slot, Option::as_ref)
            .map_err(|_| FsError::Other("the file's handle went away".to_owned()))
    }

    /// Writes all of `data`, in pieces the server takes, side by side (spec §5.6).
    async fn write_all(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let chunk = self.fs.capabilities().max_write.max(1) as usize;
        let at_a_time = (MAX_BYTES_IN_FLIGHT / chunk).clamp(1, MAX_CHUNKS_IN_FLIGHT);
        let starts = (0..data.len()).step_by(chunk);
        stream::iter(starts)
            .map(|start| {
                let end = data.len().min(start + chunk);
                self.fs
                    .write(handle, offset + start as u64, data.slice(start..end))
            })
            .buffer_unordered(at_a_time)
            .try_collect()
            .await
    }

    /// The attributes of an open file. One with a remote handle has an answer that does not depend
    /// on its path, and that is never stale: a size up to a TTL old is how readers run into the zeros
    /// of NEO's bug class (spec §5.6).
    async fn attr_of(&self, file: &OpenFile) -> FsResult<Attr> {
        if let Some(writer) = file.writer.read().await.as_ref() {
            return self.fs.fstat(writer).await;
        }
        if let Some(reader) = file.reader.lock().await.as_ref() {
            return self.fs.fstat(reader.handle()).await;
        }
        self.fs.stat(&file.path()).await
    }

    /// Changes an open file's attributes through its handle if it has one, as that also works on a
    /// file that became read-only since. No handle is opened just for this.
    async fn set_attr_of(&self, file: &OpenFile, changes: AttrChanges) -> FsResult<Attr> {
        if let Some(writer) = file.writer.read().await.as_ref() {
            return self.fs.set_attr(AttrTarget::Handle(writer), changes).await;
        }
        let path = file.path();
        self.fs.set_attr(AttrTarget::Path(&path), changes).await
    }

    /// Whether `path` is a link: `stat` shows a link as its target, and a link to a directory is
    /// removed as a file, however full that directory is.
    async fn is_link(&self, path: &RemotePath) -> FsResult<bool> {
        Ok(self.fs.lstat(path).await?.kind == FileKind::Symlink)
    }

    /// Where a new name goes, in a parent that has to exist. The driver has already seen to it that
    /// the parent is a directory.
    async fn place(&self, components: &[String]) -> winfsp::Result<Place> {
        let (name, parents) = components
            .split_last()
            .ok_or_else(|| status(STATUS_OBJECT_NAME_INVALID))?;
        let parent = self.find(parents).await.map_err(|error| {
            if error.to_ntstatus() == STATUS_OBJECT_NAME_NOT_FOUND.0 {
                status(STATUS_OBJECT_PATH_NOT_FOUND)
            } else {
                error
            }
        })?;

        let invalid = || status(STATUS_OBJECT_NAME_INVALID);
        let remote_name = windows_to_remote(name).map_err(|_| invalid())?;
        let spelled = parent.path.join(&remote_name).map_err(|_| invalid())?;
        let existing = match self.find(components).await {
            Ok(found) => Some(found),
            Err(error) if error.to_ntstatus() == STATUS_OBJECT_NAME_NOT_FOUND.0 => None,
            Err(error) => return Err(error),
        };
        Ok(Place { spelled, existing })
    }

    /// Finds a path: first with just the spelling asked for, then going down the listings for an
    /// entry that differs by case, or that Windows shows differently (spec §5.6). Any symbolic link
    /// is followed.
    async fn find(&self, components: &[String]) -> winfsp::Result<Found> {
        let exact = paths::exact_path(components)?;
        match self.fs.stat(&exact).await {
            Ok(attr) => return Ok(Found { path: exact, attr }),
            Err(FsError::NotFound | FsError::PathNotFound) => {}
            Err(error) => return Err(fail(error)),
        }

        let mut current = RemotePath::root();
        for (index, component) in components.iter().enumerate() {
            let last = index + 1 == components.len();
            let (next, entry) = self.find_entry(&current, component, last).await?;
            if !last {
                self.require_directory(&next, &entry).await?;
            }
            current = next;
        }
        let attr = self.fs.stat(&current).await.map_err(fail)?;
        Ok(Found {
            path: current,
            attr,
        })
    }

    /// The entry of a directory that a Windows name means.
    async fn find_entry(
        &self,
        directory: &RemotePath,
        name: &str,
        last: bool,
    ) -> winfsp::Result<(RemotePath, DirEntry)> {
        let missing = || {
            status(if last {
                STATUS_OBJECT_NAME_NOT_FOUND
            } else {
                STATUS_OBJECT_PATH_NOT_FOUND
            })
        };
        let entries = self
            .fs
            .read_dir(directory)
            .await
            .map_err(|error| match error {
                FsError::NotFound | FsError::PathNotFound => status(STATUS_OBJECT_PATH_NOT_FOUND),
                error => fail(error),
            })?;
        let names = entries.iter().map(|entry| entry.name.as_str());
        let resolved = resolve_name(name, names).map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        let remote_name = match resolved {
            Resolved::Exact(name) | Resolved::CaseInsensitive(name) => name,
            Resolved::Ambiguous | Resolved::NotFound => return Err(missing()),
        };
        let path = directory
            .join(remote_name)
            .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        let entry = entries
            .iter()
            .find(|entry| entry.name == remote_name)
            .cloned()
            .ok_or_else(missing)?;
        Ok((path, entry))
    }

    /// Refuses to go on through anything but a directory, or a link to one.
    async fn require_directory(&self, path: &RemotePath, entry: &DirEntry) -> winfsp::Result<()> {
        let not_found = || status(STATUS_OBJECT_PATH_NOT_FOUND);
        match entry.attr.kind {
            FileKind::Dir => Ok(()),
            FileKind::File | FileKind::Other => Err(not_found()),
            FileKind::Symlink => match self.fs.stat(path).await {
                Ok(target) if target.is_dir() => Ok(()),
                Ok(_) | Err(FsError::NotFound | FsError::PathNotFound) => Err(not_found()),
                Err(error) => Err(fail(error)),
            },
        }
    }

    /// What a search for one name in a directory finds, from the listing.
    ///
    /// WinFsp's driver has upper-cased the name by now, so the case that was asked for is gone, and
    /// a first try with this spelling would be wasted. For the same reason, several entries matching
    /// is no reason to find nothing, as it is for an open: `notes` and `Notes` would both read as
    /// missing. Windows would show either, so this always shows the same one.
    async fn find_listed(&self, directory: &RemotePath, name: &str) -> winfsp::Result<DirInfo> {
        let entries = self.fs.read_dir(directory).await.map_err(fail)?;
        let invalid = |_| status(STATUS_OBJECT_NAME_INVALID);
        let names = entries.iter().map(|entry| entry.name.as_str());
        let entry = match resolve_name(name, names).map_err(invalid)? {
            Resolved::Exact(found) | Resolved::CaseInsensitive(found) => {
                entries.iter().find(|entry| entry.name == found)
            }
            Resolved::Ambiguous => entries
                .iter()
                .filter(|entry| {
                    let one = [entry.name.as_str()];
                    !matches!(resolve_name(name, one), Ok(Resolved::NotFound) | Err(_))
                })
                .min_by_key(|entry| entry.name.as_str()),
            Resolved::NotFound => None,
        };
        let entry = entry.ok_or_else(|| status(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let info = if entry.attr.kind == FileKind::Symlink {
            self.follow(directory, entry).await?
        } else {
            entry_info(directory, entry, &entry.attr)
        };
        info.ok_or_else(|| status(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    /// A directory's entries as Windows shows them, each link as its target (spec §5.6).
    async fn listing(&self, directory: &RemotePath) -> winfsp::Result<Vec<DirInfo>> {
        let entries = self.fs.read_dir(directory).await.map_err(fail)?;
        let mut shown = Vec::with_capacity(entries.len() + 2);
        if !directory.is_root() {
            // `..` shows the directory's own details too: its parent's would cost a round trip for
            // something nobody reads.
            let own = self.fs.stat(directory).await.map_err(fail)?;
            let info = attrs::file_info(&own, directory);
            shown.extend(dir_info(".", &info));
            shown.extend(dir_info("..", &info));
        }

        let (links, others): (Vec<_>, Vec<_>) = entries
            .iter()
            .partition(|entry| entry.attr.kind == FileKind::Symlink);
        for entry in others {
            shown.extend(entry_info(directory, entry, &entry.attr));
        }
        let followed: Vec<_> = stream::iter(links)
            .map(|link| self.follow(directory, link))
            .buffer_unordered(LINKS_AT_A_TIME)
            .try_collect()
            .await?;
        shown.extend(followed.into_iter().flatten());
        Ok(shown)
    }

    /// A link as its target. A link that cannot be followed is still listed, so that it can be seen
    /// and removed: only a dead connection fails the listing.
    async fn follow(
        &self,
        directory: &RemotePath,
        link: &DirEntry,
    ) -> winfsp::Result<Option<DirInfo>> {
        let Ok(path) = directory.join(&link.name) else {
            warn!(name = %link.name, "skipping a listed name that is not a path component");
            return Ok(None);
        };
        match self.fs.stat(&path).await {
            Ok(target) => Ok(entry_info(directory, link, &target)),
            Err(error @ (FsError::Timeout | FsError::ConnectionLost)) => Err(fail(error)),
            Err(error) => {
                debug!(%error, path = %path.as_str(), "listing a link as it is");
                Ok(entry_info(directory, link, &link.attr))
            }
        }
    }

    /// A new file's attributes, after making it read-only if that was asked for.
    async fn new_file_attr(&self, handle: &RemoteHandle, read_only: bool) -> FsResult<Attr> {
        if read_only {
            let changes = AttrChanges {
                read_only: Some(true),
                ..AttrChanges::default()
            };
            self.fs.set_attr(AttrTarget::Handle(handle), changes).await
        } else {
            self.fs.fstat(handle).await
        }
    }

    async fn volume_stats(&self) -> winfsp::Result<FsStats> {
        let remembered = *self.volume.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((asked, stats)) = remembered
            && asked.elapsed() < VOLUME_INFO_TTL
        {
            return Ok(stats);
        }
        let asked = Instant::now();
        let stats = self.fs.statfs().await.map_err(fail)?;
        *self.volume.lock().unwrap_or_else(PoisonError::into_inner) = Some((asked, stats));
        Ok(stats)
    }
}

/// Where `path` is after `from` moved to `to`, if it was within `from`.
fn moved_path(path: &RemotePath, from: &RemotePath, to: &RemotePath) -> Option<RemotePath> {
    let rest = path.as_str().strip_prefix(from.as_str())?;
    if rest.is_empty() {
        return Some(to.clone());
    }
    let rest = rest.strip_prefix('/')?;
    to.join(rest).ok()
}

/// An entry's name and details, ready for WinFsp. A name Windows cannot be given is left out, with a
/// note in the log: it could not be opened anyway.
fn entry_info(directory: &RemotePath, entry: &DirEntry, attr: &Attr) -> Option<DirInfo> {
    let Ok(path) = directory.join(&entry.name) else {
        warn!(name = %entry.name, "skipping a listed name that is not a path component");
        return None;
    };
    let shown = remote_to_windows(&entry.name);
    let info = dir_info(&shown, &attrs::file_info(attr, &path));
    if info.is_none() {
        warn!(name = %entry.name, "skipping a listed name that is too long for Windows");
    }
    info
}

fn dir_info(name: &str, info: &FileInfo) -> Option<DirInfo> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    if wide.len() > MAX_NAME {
        return None;
    }
    let mut entry = DirInfo::new();
    *entry.file_info_mut() = info.clone();
    entry.set_name_raw(wide.as_slice()).ok()?;
    Some(entry)
}

impl FileSystemContext for Adapter {
    type FileContext = Arc<OpenFile>;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let components = paths::components(file_name)?;
        let found = self.run(self.find(&components))?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: security::copy_descriptor(
                &self.descriptor,
                security_descriptor,
            )?,
            attributes: attrs::file_attributes(&found.attr),
        })
    }

    fn get_security(
        &self,
        _context: &Arc<OpenFile>,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        security::copy_descriptor(&self.descriptor, security_descriptor)
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Arc<OpenFile>> {
        let components = paths::components(file_name)?;
        let found = self.run(self.find(&components))?;
        *file_info.as_mut() = attrs::file_info(&found.attr, &found.path);
        // The name as it is on the server, whatever case was asked for.
        file_info.set_normalized_name(&paths::windows_path(&found.path), None);
        Ok(self.track(found.path, found.attr.is_dir(), None))
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Arc<OpenFile>> {
        self.check_writable()?;
        let components = paths::components(file_name)?;
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let read_only = file_attributes & FILE_ATTRIBUTE_READONLY.0 != 0;

        let (path, attr, writer) = self.run(async {
            let place = self.place(&components).await?;
            if place.existing.is_some() {
                return Err(status(STATUS_OBJECT_NAME_COLLISION));
            }
            let path = place.spelled;
            if is_dir {
                let attr = self.fs.mkdir(&path).await.map_err(fail)?;
                return Ok((path, attr, None));
            }
            let mode = OpenMode::write(Disposition::CreateNew);
            let handle = self.fs.open(&path, mode).await.map_err(fail)?;
            match self.new_file_attr(&handle, read_only).await {
                Ok(attr) => Ok((path, attr, Some(handle))),
                Err(error) => {
                    // The file stays: all that is known about the server now is that it is failing.
                    let _ = self.fs.close(handle).await;
                    Err(fail(error))
                }
            }
        })?;

        *file_info.as_mut() = attrs::file_info(&attr, &path);
        file_info.set_normalized_name(&paths::windows_path(&path), None);
        Ok(self.track(path, is_dir, writer))
    }

    fn overwrite(
        &self,
        context: &Arc<OpenFile>,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        let read_only = file_attributes & FILE_ATTRIBUTE_READONLY.0 != 0;
        let changes = AttrChanges {
            size: Some(0),
            // Without `replace`, attributes are only added.
            read_only: (replace_file_attributes || read_only).then_some(read_only),
            ..AttrChanges::default()
        };
        let path = context.path();
        let attr = self.run(async { self.set_attr_of(context, changes).await.map_err(fail) });
        self.contents_changed(&path);
        *file_info = attrs::file_info(&attr?, &path);
        Ok(())
    }

    fn write(
        &self,
        context: &Arc<OpenFile>,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        self.check_writable()?;
        if context.is_dir {
            return Err(status(STATUS_INVALID_DEVICE_REQUEST));
        }
        let path = context.path();
        let result = self.run(async {
            let handle = self.writer(context).await.map_err(fail)?;
            let mut offset = offset;
            let mut len = buffer.len() as u64;
            if write_to_eof || constrained_io {
                let size = self.fs.fstat(&handle).await.map_err(fail)?.size;
                if write_to_eof {
                    offset = size;
                } else {
                    // Paging I/O does not change a file's size.
                    len = len.min(size.saturating_sub(offset));
                }
            }
            let data = &buffer[..usize::try_from(len).unwrap_or(buffer.len())];
            self.write_all(&handle, offset, Bytes::copy_from_slice(data))
                .await
                .map_err(fail)?;
            let attr = self.fs.fstat(&handle).await.map_err(fail)?;
            Ok((data.len(), attr))
        });
        self.contents_changed(&path);
        let (written, attr) = result?;
        *file_info = attrs::file_info(&attr, &path);
        Ok(u32::try_from(written).unwrap_or(u32::MAX))
    }

    fn flush(
        &self,
        context: Option<&Arc<OpenFile>>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        // Writes are acknowledged before they return, so the volume has nothing waiting.
        let Some(context) = context else {
            return Ok(());
        };
        let attr = self.run(async {
            if let Some(writer) = context.writer.read().await.as_ref() {
                self.fs.flush(writer).await.map_err(fail)?;
            }
            self.attr_of(context).await.map_err(fail)
        })?;
        *file_info = attrs::file_info(&attr, &context.path());
        Ok(())
    }

    fn cleanup(&self, context: &Arc<OpenFile>, _file_name: Option<&U16CStr>, flags: u32) {
        // A file opened to be deleted on close gets here without any callback that can refuse.
        if self.read_only {
            return;
        }
        let path = context.path();
        let done = if FspCleanupFlags::FspCleanupDelete.is_flagged(flags) {
            // Handles go first: some servers will not remove an open file.
            self.run(async {
                if let Err(error) = release(&self.fs, context).await {
                    debug!(%error, path = %path.as_str(), "closing failed");
                }
                if context.is_dir && !self.is_link(&path).await.map_err(fail)? {
                    self.fs.remove_dir(&path).await.map_err(fail)
                } else {
                    self.fs.remove_file(&path).await.map_err(fail)
                }
            })
        } else {
            self.run(async {
                match context.writer.read().await.as_ref() {
                    Some(writer) => self.fs.flush(writer).await.map_err(fail),
                    None => Ok(()),
                }
            })
        };
        // Windows has no way to hear about a failed cleanup: the handle is already closed.
        if let Err(error) = done {
            warn!(status = error.to_ntstatus(), path = %path.as_str(), "cleanup failed");
        }
    }

    fn set_basic_info(
        &self,
        context: &Arc<OpenFile>,
        file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        let changes = AttrChanges {
            // On a directory the flag means nothing to Windows, and taking the owner's write bit
            // away would stop new files going in.
            read_only: (file_attributes != ATTRIBUTES_UNCHANGED && !context.is_dir)
                .then_some(file_attributes & FILE_ATTRIBUTE_READONLY.0 != 0),
            mtime: attrs::system_time(last_write_time),
            atime: attrs::system_time(last_access_time),
            size: None,
        };
        let attr = self.run(async {
            if changes == AttrChanges::default() {
                self.attr_of(context).await
            } else {
                self.set_attr_of(context, changes).await
            }
            .map_err(fail)
        })?;
        *file_info = attrs::file_info(&attr, &context.path());
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Arc<OpenFile>,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        let path = context.path();
        let attr = self.run(async {
            if set_allocation_size {
                // Servers have no allocation to set, but cutting it short does cut the file.
                let attr = self.attr_of(context).await.map_err(fail)?;
                if new_size >= attr.size {
                    return Ok(attr);
                }
            }
            let changes = AttrChanges {
                size: Some(new_size),
                ..AttrChanges::default()
            };
            self.set_attr_of(context, changes).await.map_err(fail)
        });
        self.contents_changed(&path);
        *file_info = attrs::file_info(&attr?, &path);
        Ok(())
    }

    /// Only checks. The driver remembers the flag, and asks for the removal in `cleanup` once the last
    /// handle is closing. It also refuses a read-only file before this.
    fn set_delete(
        &self,
        context: &Arc<OpenFile>,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        if !delete_file || !context.is_dir {
            return Ok(());
        }
        let path = context.path();
        self.run(async {
            if self.is_link(&path).await.map_err(fail)? {
                return Ok(());
            }
            if self.fs.read_dir(&path).await.map_err(fail)?.is_empty() {
                Ok(())
            } else {
                Err(status(STATUS_DIRECTORY_NOT_EMPTY))
            }
        })
    }

    fn rename(
        &self,
        context: &Arc<OpenFile>,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        let components = paths::components(new_file_name)?;
        let from = context.path();
        let to = self.run(async {
            let place = self.place(&components).await?;
            let to = match place.existing {
                // The same file, perhaps in another case.
                Some(target) if target.path == from => place.spelled,
                Some(_) if !replace_if_exists => {
                    return Err(status(STATUS_OBJECT_NAME_COLLISION));
                }
                // A server would take an empty directory. Windows does not.
                Some(target) if context.is_dir || target.attr.is_dir() => {
                    return Err(status(STATUS_ACCESS_DENIED));
                }
                // The name that is already there stays, in whatever case it has.
                Some(target) => target.path,
                None => place.spelled,
            };
            if to != from {
                self.fs
                    .rename(&from, &to, replace_if_exists)
                    .await
                    .map_err(fail)?;
            }
            Ok(to)
        })?;
        self.moved(&from, &to);
        Ok(())
    }

    /// Every file has one fixed descriptor (spec §5.6). Saying this worked would leave someone
    /// believing a file was protected. `ReplaceFile` does not need it to.
    fn set_security(
        &self,
        _context: &Arc<OpenFile>,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        Err(status(STATUS_NOT_SUPPORTED))
    }

    /// The label comes from the profile.
    fn set_volume_label(
        &self,
        _volume_label: &U16CStr,
        _volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        self.check_writable()?;
        Err(status(STATUS_NOT_SUPPORTED))
    }

    fn close(&self, context: Arc<OpenFile>) {
        self.open_files()
            .retain(|file| !Arc::ptr_eq(file, &context));
        let closed = self.run(async { release(&self.fs, &context).await.map_err(fail) });
        if let Err(error) = closed {
            debug!(status = error.to_ntstatus(), path = %context.path().as_str(), "closing failed");
        }
    }

    fn get_file_info(
        &self,
        context: &Arc<OpenFile>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self.run(async { self.attr_of(context).await.map_err(fail) })?;
        *file_info = attrs::file_info(&attr, &context.path());
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let stats = self.run(self.volume_stats())?;
        out_volume_info.total_size = stats.total_bytes;
        out_volume_info.free_size = stats.free_bytes;
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }

    fn read(&self, context: &Arc<OpenFile>, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(status(STATUS_INVALID_DEVICE_REQUEST));
        }
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let result = self.run(async {
            let mut slot = context.reader.lock().await;
            let stale = context.stale.swap(false, Ordering::AcqRel);
            let reader = match &mut *slot {
                Some(reader) => {
                    if stale {
                        reader.discard().await;
                    }
                    reader
                }
                empty => {
                    let handle = self
                        .fs
                        .open(&context.path(), OpenMode::read())
                        .await
                        .map_err(fail)?;
                    empty.insert(ReadAhead::new(Arc::clone(&self.fs), handle))
                }
            };
            reader.read(offset, len).await.map_err(fail)
        });
        if result.is_err() {
            // A request still in flight would hand its failure to the next read too, moments later.
            // Only an overlapping read can make this wait, and then it is not worth waiting for.
            self.runtime.block_on(async {
                let _ = timeout(DISCARD_LIMIT, async {
                    if let Some(reader) = context.reader.lock().await.as_ref() {
                        reader.discard().await;
                    }
                })
                .await;
            });
        }
        let bytes = result?;
        if bytes.is_empty() && len > 0 {
            return Err(status(STATUS_END_OF_FILE));
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(u32::try_from(bytes.len()).unwrap_or(len))
    }

    fn read_directory(
        &self,
        context: &Arc<OpenFile>,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.is_dir {
            return Err(status(STATUS_NOT_A_DIRECTORY));
        }
        // A new enumeration lists the directory again. Later pages come from the buffer, for which
        // WinFsp reports success as a failure to acquire it.
        match context.listing.acquire(marker.is_none(), None) {
            Ok(lock) => {
                for mut entry in self.run(self.listing(&context.path()))? {
                    lock.write(&mut entry)?;
                }
            }
            Err(error) if error.to_ntstatus() == 0 => {}
            Err(error) => return Err(error),
        }
        Ok(context.listing.read(marker, buffer))
    }

    fn get_dir_info_by_name(
        &self,
        context: &Arc<OpenFile>,
        file_name: &U16CStr,
        out_dir_info: &mut DirInfo,
    ) -> winfsp::Result<()> {
        let name = String::from_utf16(file_name.as_slice())
            .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        *out_dir_info = self.run(self.find_listed(&context.path(), &name))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RemotePath {
        RemotePath::new(text).unwrap()
    }

    #[test]
    fn a_path_follows_a_rename_of_itself_or_of_a_directory_above_it() {
        let moved = |p: &str| moved_path(&path(p), &path("/a/b"), &path("/x/y"));
        assert_eq!(moved("/a/b"), Some(path("/x/y")));
        assert_eq!(moved("/a/b/c/d"), Some(path("/x/y/c/d")));
    }

    #[test]
    fn a_path_that_only_starts_the_same_stays() {
        let moved = |p: &str| moved_path(&path(p), &path("/a/b"), &path("/x/y"));
        assert_eq!(moved("/a/bc"), None);
        assert_eq!(moved("/a"), None);
        assert_eq!(moved("/other"), None);
    }
}
