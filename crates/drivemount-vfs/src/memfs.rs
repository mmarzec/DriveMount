// SPDX-License-Identifier: GPL-3.0-or-later

//! An in-memory [`RemoteFs`] for tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};

use crate::{
    Access, Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FileKind, FsError,
    FsResult, FsStats, OpenMode, RemoteFs, RemoteHandle, RemotePath, apply_read_only,
};

/// Files are stored in pages of this size, and pages that were never written take no memory.
const PAGE_SIZE: u64 = 64 * 1024;

/// What [`statfs`](RemoteFs::statfs) reports as the size of the file system. Nothing enforces it.
const TOTAL_BYTES: u64 = 1 << 40;

const DEFAULT_MAX_IO: u32 = 256 * 1024;
const FILE_PERM: u32 = 0o644;
const DIR_PERM: u32 = 0o755;

/// An in-memory [`RemoteFs`], which behaves like a POSIX server seen by a non-root user.
///
/// - File data is sparse, so tests can use huge offsets and sizes without the memory to match.
/// - A handle keeps its file alive after the file is removed or replaced.
/// - Names are case-sensitive and any name is allowed.
/// - Reads are cut to [`Capabilities::max_read`], giving the short reads callers must handle. Writes
///   above [`Capabilities::max_write`] fail, to catch callers that forget to split them.
///
/// It has no symbolic links, because [`RemoteFs`] has no way to make one. [`Capabilities`] change
/// only what is reported and those limits; [`statfs`](RemoteFs::statfs) counts written pages against
/// a nominal 1 TiB that writes cannot run out of.
pub struct MemFs {
    state: Mutex<State>,
    capabilities: Capabilities,
}

impl MemFs {
    /// An empty file system with every capability and limits of 256 KiB.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capabilities(Capabilities {
            posix_rename: true,
            fsync: true,
            statvfs: true,
            max_read: DEFAULT_MAX_IO,
            max_write: DEFAULT_MAX_IO,
        })
    }

    /// An empty file system reporting these capabilities. The limits must not be zero.
    #[must_use]
    pub fn with_capabilities(capabilities: Capabilities) -> Self {
        Self {
            state: Mutex::new(State {
                root: Node::Dir(Dir::new()),
                handles: HashMap::new(),
                next_handle: 1,
            }),
            capabilities,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

/// A panic in another test thread leaves the data as consistent as the panicking test left it, so a
/// poisoned lock is not worth a second panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_a_directory() -> FsError {
    FsError::Other("not a directory".to_owned())
}

fn is_a_directory() -> FsError {
    FsError::Other("is a directory".to_owned())
}

fn invalid_handle() -> FsError {
    FsError::Other("invalid handle".to_owned())
}

/// Permissions and times, which files and directories share.
#[derive(Clone, Copy)]
struct Meta {
    perm: u32,
    mtime: SystemTime,
    atime: SystemTime,
}

impl Meta {
    fn new(perm: u32) -> Self {
        let now = SystemTime::now();
        Self {
            perm,
            mtime: now,
            atime: now,
        }
    }

    fn apply(&mut self, changes: AttrChanges) {
        if let Some(read_only) = changes.read_only {
            self.perm = apply_read_only(self.perm, read_only);
        }
        self.mtime = changes.mtime.unwrap_or(self.mtime);
        self.atime = changes.atime.unwrap_or(self.atime);
    }

    fn attr(&self, kind: FileKind, size: u64) -> Attr {
        Attr {
            kind,
            size,
            perm: Some(self.perm),
            mtime: Some(self.mtime),
            atime: Some(self.atime),
        }
    }
}

/// A file's contents and attributes. Bytes of a page beyond `size` are always zero, so extending a
/// file needs no work and shrinking must clear them.
struct File {
    meta: Meta,
    size: u64,
    pages: BTreeMap<u64, Box<[u8]>>,
}

impl File {
    fn new() -> Self {
        Self {
            meta: Meta::new(FILE_PERM),
            size: 0,
            pages: BTreeMap::new(),
        }
    }

    fn attr(&self) -> Attr {
        self.meta.attr(FileKind::File, self.size)
    }

    fn read(&self, offset: u64, len: u32) -> Bytes {
        let len = u64::from(len).min(self.size.saturating_sub(offset));
        let mut out = BytesMut::zeroed(len as usize);
        if len > 0 {
            let first = offset / PAGE_SIZE;
            let last = (offset + len - 1) / PAGE_SIZE;
            for (&index, page) in self.pages.range(first..=last) {
                let page_start = index * PAGE_SIZE;
                let start = offset.max(page_start);
                let end = (offset + len).min(page_start + PAGE_SIZE);
                let from = (start - page_start) as usize..(end - page_start) as usize;
                let to = (start - offset) as usize..(end - offset) as usize;
                out[to].copy_from_slice(&page[from]);
            }
        }
        out.freeze()
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> FsResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FsError::Other("offset too large".to_owned()))?;
        let mut position = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let index = position / PAGE_SIZE;
            let in_page = (position % PAGE_SIZE) as usize;
            let count = rest.len().min(PAGE_SIZE as usize - in_page);
            let page = self
                .pages
                .entry(index)
                .or_insert_with(|| vec![0; PAGE_SIZE as usize].into_boxed_slice());
            page[in_page..in_page + count].copy_from_slice(&rest[..count]);
            position += count as u64;
            rest = &rest[count..];
        }
        self.size = self.size.max(end);
        self.meta.mtime = SystemTime::now();
        Ok(())
    }

    fn set_size(&mut self, size: u64) {
        if size < self.size {
            let first_gone = size.div_ceil(PAGE_SIZE);
            drop(self.pages.split_off(&first_gone));
            if let Some(page) = self.pages.get_mut(&(size / PAGE_SIZE)) {
                page[(size % PAGE_SIZE) as usize..].fill(0);
            }
        }
        self.size = size;
        self.meta.mtime = SystemTime::now();
    }
}

struct Dir {
    meta: Meta,
    entries: BTreeMap<String, Node>,
}

impl Dir {
    fn new() -> Self {
        Self {
            meta: Meta::new(DIR_PERM),
            entries: BTreeMap::new(),
        }
    }

    fn attr(&self) -> Attr {
        self.meta.attr(FileKind::Dir, 0)
    }
}

enum Node {
    /// Shared with the handles open on it.
    File(Arc<Mutex<File>>),
    Dir(Dir),
}

impl Node {
    fn attr(&self) -> Attr {
        match self {
            Self::File(file) => lock(file).attr(),
            Self::Dir(dir) => dir.attr(),
        }
    }

    fn written_bytes(&self) -> u64 {
        match self {
            Self::File(file) => lock(file).pages.len() as u64 * PAGE_SIZE,
            Self::Dir(dir) => dir.entries.values().map(Self::written_bytes).sum(),
        }
    }
}

#[derive(Clone)]
struct OpenFile {
    file: Arc<Mutex<File>>,
    access: Access,
}

/// The tree and the handle table. Locks are taken in the order state, then file.
struct State {
    root: Node,
    handles: HashMap<u64, OpenFile>,
    next_handle: u64,
}

impl State {
    fn find(&mut self, path: &RemotePath) -> FsResult<&mut Node> {
        let mut node = &mut self.root;
        let mut names = path.as_str().split('/').filter(|name| !name.is_empty());
        let mut name = names.next();
        while let Some(current) = name {
            let next = names.next();
            let Node::Dir(dir) = node else {
                return Err(FsError::PathNotFound);
            };
            node = dir.entries.get_mut(current).ok_or(if next.is_some() {
                FsError::PathNotFound
            } else {
                FsError::NotFound
            })?;
            name = next;
        }
        Ok(node)
    }

    /// The directory that holds `path`, and the name in it. The root has no parent, and cannot be
    /// created, removed or renamed.
    fn parent(&mut self, path: &RemotePath) -> FsResult<(&mut Dir, String)> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(FsError::PermissionDenied);
        };
        match self.find(&parent) {
            Ok(Node::Dir(dir)) => Ok((dir, name.to_owned())),
            Ok(Node::File(_)) | Err(FsError::NotFound) => Err(FsError::PathNotFound),
            Err(error) => Err(error),
        }
    }

    fn open(&mut self, path: &RemotePath, mode: OpenMode) -> FsResult<Arc<Mutex<File>>> {
        let disposition = mode.disposition();
        match self.find(path) {
            Ok(_) if disposition == Disposition::CreateNew => Err(FsError::Exists),
            Ok(Node::Dir(_)) => Err(is_a_directory()),
            Ok(Node::File(file)) => {
                let file = Arc::clone(file);
                let mut data = lock(&file);
                if mode.access() != Access::Read && data.attr().is_read_only() {
                    return Err(FsError::PermissionDenied);
                }
                if matches!(
                    disposition,
                    Disposition::TruncateExisting | Disposition::CreateOrTruncate
                ) {
                    data.set_size(0);
                }
                drop(data);
                Ok(file)
            }
            Err(FsError::NotFound)
                if !matches!(
                    disposition,
                    Disposition::OpenExisting | Disposition::TruncateExisting
                ) =>
            {
                let file = Arc::new(Mutex::new(File::new()));
                let (dir, name) = self.parent(path)?;
                dir.entries.insert(name, Node::File(Arc::clone(&file)));
                Ok(file)
            }
            Err(error) => Err(error),
        }
    }

    fn handle(&self, handle: &RemoteHandle) -> FsResult<OpenFile> {
        self.handles
            .get(&handle.id())
            .cloned()
            .ok_or_else(invalid_handle)
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        if from.is_root() || to.is_root() {
            return Err(FsError::PermissionDenied);
        }
        let from_is_dir = matches!(self.find(from)?, Node::Dir(_));
        self.parent(to)?;
        if from == to {
            return if replace {
                Ok(())
            } else {
                Err(FsError::Exists)
            };
        }
        if to.as_str().starts_with(&format!("{from}/")) {
            return Err(FsError::Other(
                "cannot move a directory into itself".to_owned(),
            ));
        }
        match self.find(to) {
            Ok(_) if !replace => return Err(FsError::Exists),
            Ok(Node::Dir(dir)) if !dir.entries.is_empty() => return Err(FsError::NotEmpty),
            Ok(Node::Dir(_)) if !from_is_dir => return Err(is_a_directory()),
            Ok(Node::File(_)) if from_is_dir => return Err(not_a_directory()),
            Ok(_) | Err(FsError::NotFound) => {}
            Err(error) => return Err(error),
        }

        let (dir, name) = self.parent(from)?;
        let node = dir.entries.remove(&name).ok_or(FsError::NotFound)?;
        let (dir, name) = self.parent(to)?;
        dir.entries.insert(name, node);
        Ok(())
    }
}

#[async_trait]
impl RemoteFs for MemFs {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        Ok(self.state().find(path)?.attr())
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.stat(path).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        let open = self.state().handle(handle)?;
        Ok(lock(&open.file).attr())
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        let mut state = self.state();
        let Node::Dir(dir) = state.find(path)? else {
            return Err(not_a_directory());
        };
        Ok(dir
            .entries
            .iter()
            .map(|(name, node)| DirEntry {
                name: name.clone(),
                attr: node.attr(),
            })
            .collect())
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let mut state = self.state();
        let file = state.open(path, mode)?;
        let id = state.next_handle;
        state.next_handle += 1;
        let access = mode.access();
        state.handles.insert(id, OpenFile { file, access });
        Ok(RemoteHandle::new(id))
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        let open = self.state().handle(handle)?;
        if open.access == Access::Write {
            return Err(FsError::PermissionDenied);
        }
        let len = len.min(self.capabilities.max_read);
        Ok(lock(&open.file).read(offset, len))
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let open = self.state().handle(handle)?;
        if open.access == Access::Read {
            return Err(FsError::PermissionDenied);
        }
        if data.len() as u64 > u64::from(self.capabilities.max_write) {
            return Err(FsError::Other(format!(
                "write of {} bytes exceeds max_write",
                data.len()
            )));
        }
        lock(&open.file).write(offset, &data)
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.state().handle(handle).map(|_| ())
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        let removed = self.state().handles.remove(&handle.id());
        removed.map(drop).ok_or_else(invalid_handle)
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        let mut state = self.state();
        let (file, may_resize) = match target {
            AttrTarget::Handle(handle) => {
                let open = state.handle(handle)?;
                let may_resize = open.access != Access::Read;
                (open.file, may_resize)
            }
            AttrTarget::Path(path) => match state.find(path)? {
                Node::Dir(dir) => {
                    if changes.size.is_some() {
                        return Err(is_a_directory());
                    }
                    dir.meta.apply(changes);
                    return Ok(dir.attr());
                }
                Node::File(file) => {
                    let file = Arc::clone(file);
                    let may_resize = !lock(&file).attr().is_read_only();
                    (file, may_resize)
                }
            },
        };
        let mut file = lock(&file);
        if let Some(size) = changes.size {
            if !may_resize {
                return Err(FsError::PermissionDenied);
            }
            file.set_size(size);
        }
        file.meta.apply(changes);
        Ok(file.attr())
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let mut state = self.state();
        match state.find(path) {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let dir = Dir::new();
        let attr = dir.attr();
        let (parent, name) = state.parent(path)?;
        parent.entries.insert(name, Node::Dir(dir));
        Ok(attr)
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let mut state = self.state();
        if matches!(state.find(path)?, Node::Dir(_)) {
            return Err(is_a_directory());
        }
        let (parent, name) = state.parent(path)?;
        parent.entries.remove(&name);
        Ok(())
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let mut state = self.state();
        match state.find(path)? {
            Node::File(_) => return Err(not_a_directory()),
            Node::Dir(dir) if !dir.entries.is_empty() => return Err(FsError::NotEmpty),
            Node::Dir(_) => {}
        }
        let (parent, name) = state.parent(path)?;
        parent.entries.remove(&name);
        Ok(())
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.state().rename(from, to, replace)
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        if !self.capabilities.statvfs {
            return Ok(FsStats::PLACEHOLDER);
        }
        let written = self.state().root.written_bytes();
        Ok(FsStats {
            total_bytes: TOTAL_BYTES,
            free_bytes: TOTAL_BYTES.saturating_sub(written),
        })
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
}

// The behaviour every backend shares is in `contract`, run by tests/contract.rs. These tests cover
// what is particular to MemFs.
#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> RemotePath {
        RemotePath::new(path).unwrap()
    }

    const CREATE: OpenMode = OpenMode::read_write(Disposition::CreateNew);

    #[tokio::test]
    async fn a_missing_parent_is_told_apart_from_a_missing_file() {
        let fs = MemFs::new();
        assert_eq!(fs.stat(&path("/none")).await, Err(FsError::NotFound));
        assert_eq!(fs.stat(&path("/no/ne")).await, Err(FsError::PathNotFound));
        assert_eq!(
            fs.open(&path("/no/ne"), CREATE).await,
            Err(FsError::PathNotFound)
        );

        let file = fs.open(&path("/file"), CREATE).await.unwrap();
        fs.close(file).await.unwrap();
        assert_eq!(
            fs.stat(&path("/file/x")).await,
            Err(FsError::PathNotFound),
            "a path through a file"
        );
        assert_eq!(fs.mkdir(&path("/file/x")).await, Err(FsError::PathNotFound));
    }

    #[tokio::test]
    async fn the_root_cannot_be_removed_or_renamed() {
        let fs = MemFs::new();
        let root = RemotePath::root();
        assert!(fs.stat(&root).await.unwrap().is_dir());
        assert_eq!(fs.mkdir(&root).await, Err(FsError::Exists));
        assert_eq!(fs.remove_dir(&root).await, Err(FsError::PermissionDenied));
        assert_eq!(
            fs.rename(&root, &path("/x"), false).await,
            Err(FsError::PermissionDenied)
        );
        assert_eq!(
            fs.rename(&path("/x"), &root, true).await,
            Err(FsError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn a_closed_handle_is_no_longer_valid() {
        let fs = MemFs::new();
        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        let id = handle.id();
        fs.close(handle).await.unwrap();

        let stale = RemoteHandle::new(id);
        assert!(fs.fstat(&stale).await.is_err());
        assert!(fs.read(&stale, 0, 1).await.is_err());
        assert!(fs.flush(&stale).await.is_err());
        assert!(fs.close(stale).await.is_err());
    }

    #[tokio::test]
    async fn a_handle_keeps_its_file_after_rename_and_remove() {
        let fs = MemFs::new();
        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        fs.write(&handle, 0, Bytes::from_static(b"kept"))
            .await
            .unwrap();

        fs.rename(&path("/f"), &path("/g"), false).await.unwrap();
        fs.write(&handle, 4, Bytes::from_static(b"!"))
            .await
            .unwrap();
        let reader = fs.open(&path("/g"), OpenMode::read()).await.unwrap();
        assert_eq!(fs.read(&reader, 0, 16).await.unwrap(), "kept!");

        fs.remove_file(&path("/g")).await.unwrap();
        assert_eq!(fs.fstat(&handle).await.unwrap().size, 5);
        assert_eq!(fs.read(&handle, 0, 16).await.unwrap(), "kept!");
    }

    #[tokio::test]
    async fn reads_are_cut_to_max_read_and_oversized_writes_fail() {
        let fs = MemFs::with_capabilities(Capabilities {
            max_read: 4,
            max_write: 8,
            ..MemFs::new().capabilities()
        });
        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        fs.write(&handle, 0, Bytes::from_static(b"12345678"))
            .await
            .unwrap();
        assert!(matches!(
            fs.write(&handle, 0, Bytes::from_static(b"123456789")).await,
            Err(FsError::Other(_))
        ));
        assert_eq!(fs.read(&handle, 2, 100).await.unwrap(), "3456");
    }

    #[tokio::test]
    async fn resizing_needs_write_access() {
        let fs = MemFs::new();
        let writer = fs.open(&path("/f"), CREATE).await.unwrap();
        let reader = fs.open(&path("/f"), OpenMode::read()).await.unwrap();
        let grow = AttrChanges {
            size: Some(10),
            ..AttrChanges::default()
        };
        assert_eq!(
            fs.set_attr(AttrTarget::Handle(&reader), grow).await,
            Err(FsError::PermissionDenied)
        );
        let attr = fs
            .set_attr(AttrTarget::Handle(&writer), grow)
            .await
            .unwrap();
        assert_eq!(attr.size, 10);

        let read_only = AttrChanges {
            read_only: Some(true),
            ..AttrChanges::default()
        };
        fs.set_attr(AttrTarget::Path(&path("/f")), read_only)
            .await
            .unwrap();
        assert_eq!(
            fs.set_attr(AttrTarget::Path(&path("/f")), grow).await,
            Err(FsError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn a_directory_has_no_size_to_set() {
        let fs = MemFs::new();
        fs.mkdir(&path("/d")).await.unwrap();
        let grow = AttrChanges {
            size: Some(1),
            ..AttrChanges::default()
        };
        assert!(
            fs.set_attr(AttrTarget::Path(&path("/d")), grow)
                .await
                .is_err()
        );
        assert_eq!(fs.stat(&path("/d")).await.unwrap().size, 0);
    }

    #[tokio::test]
    async fn holes_take_no_space() {
        let fs = MemFs::new();
        let empty = fs.statfs().await.unwrap();
        assert_eq!(empty.free_bytes, empty.total_bytes);

        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        let huge = AttrChanges {
            size: Some(1 << 45),
            ..AttrChanges::default()
        };
        fs.set_attr(AttrTarget::Handle(&handle), huge)
            .await
            .unwrap();
        assert_eq!(fs.statfs().await.unwrap(), empty);
        assert_eq!(fs.read(&handle, 1 << 44, 3).await.unwrap(), &[0, 0, 0][..]);

        fs.write(&handle, 1 << 44, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let stats = fs.statfs().await.unwrap();
        assert_eq!(stats.free_bytes, stats.total_bytes - PAGE_SIZE);

        // Pages under removed files come back.
        fs.remove_file(&path("/f")).await.unwrap();
        assert_eq!(fs.statfs().await.unwrap(), empty);
    }

    #[tokio::test]
    async fn shrinking_clears_the_rest_of_the_last_page() {
        let fs = MemFs::new();
        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        fs.write(&handle, 0, Bytes::from_static(b"abcdef"))
            .await
            .unwrap();
        for size in [2, 6] {
            let changes = AttrChanges {
                size: Some(size),
                ..AttrChanges::default()
            };
            fs.set_attr(AttrTarget::Handle(&handle), changes)
                .await
                .unwrap();
        }
        assert_eq!(fs.read(&handle, 0, 6).await.unwrap(), &b"ab\0\0\0\0"[..]);
    }

    #[tokio::test]
    async fn offsets_near_the_limit_fail_instead_of_wrapping() {
        let fs = MemFs::new();
        let handle = fs.open(&path("/f"), CREATE).await.unwrap();
        assert!(
            fs.write(&handle, u64::MAX, Bytes::from_static(b"xy"))
                .await
                .is_err()
        );
        assert_eq!(fs.fstat(&handle).await.unwrap().size, 0);
    }

    #[tokio::test]
    async fn without_statvfs_the_placeholder_is_reported() {
        let fs = MemFs::with_capabilities(Capabilities {
            statvfs: false,
            ..MemFs::new().capabilities()
        });
        assert_eq!(fs.statfs().await.unwrap(), FsStats::PLACEHOLDER);
    }
}
