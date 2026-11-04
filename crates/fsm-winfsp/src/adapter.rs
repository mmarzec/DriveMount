// SPDX-License-Identifier: GPL-3.0-or-later

//! The WinFsp callbacks (spec §5.8). So far the read path, and a refusal for everything else.

use std::ffi::c_void;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use fsm_vfs::{
    Attr, CachedFs, DeadlineFs, DirEntry, FileKind, FsError, FsStats, OpenMode, ReadAhead,
    RemoteFs, RemotePath, Resolved, remote_to_windows, resolve_name,
};
use futures::{StreamExt, TryStreamExt, stream};
use tokio::runtime::Handle;
use tokio::sync::OnceCell;
use tokio::time::timeout;
use tracing::{debug, warn};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_END_OF_FILE, STATUS_INVALID_DEVICE_REQUEST, STATUS_IO_TIMEOUT,
    STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext,
    ModificationDescriptor, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use crate::status::fail;
use crate::{attrs, paths, security};

/// The deadline layer, which sits below the caches so that a call that timed out still invalidates
/// them (spec §5.6).
pub(crate) type Deadline = DeadlineFs<Arc<dyn RemoteFs>>;
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

fn status(status: NTSTATUS) -> FspError {
    status.into()
}

/// What every change gets. WinFsp's read-only flag only tells programs that the volume is read-only:
/// its driver still passes their changes down, so refusing them is up to us (FR-MNT-07). Opening a
/// file with write access does work, because a program asking for `MAXIMUM_ALLOWED` must not be
/// turned away.
fn write_protected() -> FspError {
    status(STATUS_MEDIA_WRITE_PROTECTED)
}

/// One open file or directory, per Windows handle.
pub(crate) struct OpenFile {
    path: RemotePath,
    is_dir: bool,
    /// The remote handle and its read-ahead state, opened on the first read (spec §5.8). Plenty of
    /// opens only look at attributes, and a server allows only so many open files.
    reader: OnceCell<ReadAhead<Stack>>,
    /// The listing as of the start of an enumeration, so that later pages neither list the directory
    /// again nor see it change.
    listing: DirBuffer,
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
    pub(crate) op_timeout: Duration,
    pub(crate) runtime: Handle,
    pub(crate) label: String,
    pub(crate) descriptor: Vec<u8>,
    pub(crate) volume: Mutex<Option<(Instant, FsStats)>>,
}

impl Adapter {
    /// Runs `work` to its end on this dispatcher thread, for at most the operation timeout in all,
    /// however many remote calls it makes (NFR-REL-03).
    fn run<T>(&self, work: impl Future<Output = winfsp::Result<T>>) -> winfsp::Result<T> {
        self.runtime.block_on(async {
            timeout(self.op_timeout, work).await.unwrap_or_else(|_| {
                self.deadline.mark_suspect();
                Err(status(STATUS_IO_TIMEOUT))
            })
        })
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
    type FileContext = OpenFile;

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
        _context: &OpenFile,
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
    ) -> winfsp::Result<OpenFile> {
        let components = paths::components(file_name)?;
        let found = self.run(self.find(&components))?;
        *file_info.as_mut() = attrs::file_info(&found.attr, &found.path);
        // The name as it is on the server, whatever case was asked for.
        file_info.set_normalized_name(&paths::windows_path(&found.path), None);
        Ok(OpenFile {
            path: found.path,
            is_dir: found.attr.is_dir(),
            reader: OnceCell::new(),
            listing: DirBuffer::new(),
        })
    }

    fn create(
        &self,
        _file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        _file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<OpenFile> {
        Err(write_protected())
    }

    fn overwrite(
        &self,
        _context: &OpenFile,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn write(
        &self,
        _context: &OpenFile,
        _buffer: &[u8],
        _offset: u64,
        _write_to_eof: bool,
        _constrained_io: bool,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        Err(write_protected())
    }

    fn flush(&self, _context: Option<&OpenFile>, _file_info: &mut FileInfo) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn set_basic_info(
        &self,
        _context: &OpenFile,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn set_file_size(
        &self,
        _context: &OpenFile,
        _new_size: u64,
        _set_allocation_size: bool,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn set_delete(
        &self,
        _context: &OpenFile,
        _file_name: &U16CStr,
        _delete_file: bool,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn rename(
        &self,
        _context: &OpenFile,
        _file_name: &U16CStr,
        _new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn set_security(
        &self,
        _context: &OpenFile,
        _security_information: u32,
        _modification_descriptor: ModificationDescriptor,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn set_volume_label(
        &self,
        _volume_label: &U16CStr,
        _volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        Err(write_protected())
    }

    fn close(&self, context: OpenFile) {
        if let Some(reader) = context.reader.into_inner() {
            let closed = self.run(async { reader.close().await.map_err(fail) });
            if let Err(error) = closed {
                debug!(status = error.to_ntstatus(), path = %context.path.as_str(), "closing failed");
            }
        }
    }

    fn get_file_info(&self, context: &OpenFile, file_info: &mut FileInfo) -> winfsp::Result<()> {
        // An open file has an answer that does not depend on its path, and that is never stale: a
        // size up to a TTL old is how readers run into the zeros of NEO's bug class (spec §5.6).
        let attr = self.run(async {
            match context.reader.get() {
                Some(reader) => self.fs.fstat(reader.handle()).await,
                None => self.fs.stat(&context.path).await,
            }
            .map_err(fail)
        })?;
        *file_info = attrs::file_info(&attr, &context.path);
        Ok(())
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let stats = self.run(self.volume_stats())?;
        out_volume_info.total_size = stats.total_bytes;
        out_volume_info.free_size = stats.free_bytes;
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }

    fn read(&self, context: &OpenFile, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(status(STATUS_INVALID_DEVICE_REQUEST));
        }
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let result = self.run(async {
            let reader = context
                .reader
                .get_or_try_init(|| async {
                    let handle = self.fs.open(&context.path, OpenMode::read()).await?;
                    Ok(ReadAhead::new(Arc::clone(&self.fs), handle))
                })
                .await
                .map_err(fail)?;
            reader.read(offset, len).await.map_err(fail)
        });
        if result.is_err()
            && let Some(reader) = context.reader.get()
        {
            // A request still in flight would hand its failure to the next read too, moments later.
            // Only an overlapping read can make this wait, and then it is not worth waiting for.
            self.runtime.block_on(async {
                let _ = timeout(DISCARD_LIMIT, reader.discard()).await;
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
        context: &OpenFile,
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
                for mut entry in self.run(self.listing(&context.path))? {
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
        context: &OpenFile,
        file_name: &U16CStr,
        out_dir_info: &mut DirInfo,
    ) -> winfsp::Result<()> {
        let name = String::from_utf16(file_name.as_slice())
            .map_err(|_| status(STATUS_OBJECT_NAME_INVALID))?;
        *out_dir_info = self.run(self.find_listed(&context.path, &name))?;
        Ok(())
    }
}
