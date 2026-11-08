// SPDX-License-Identifier: GPL-3.0-or-later

//! Runs the contract suite against `MemFs`, and checks that the suite can fail: each test with a
//! fault makes `MemFs` break one promise of the contract, and the suite has to notice.

// clippy.toml exempts only code inside #[test] functions, not the wrapper's methods.
#![allow(clippy::expect_used)]

use std::sync::Arc;

use drivemount_vfs::{
    Access, Attr, AttrChanges, AttrTarget, Bytes, Capabilities, DirEntry, Disposition, FsResult,
    FsStats, MemFs, OpenMode, RemoteFs, RemoteHandle, RemotePath, async_trait, contract,
};

#[tokio::test]
async fn memfs_passes_everything() {
    contract::run_all(&MemFs::new(), &RemotePath::root()).await;
}

#[tokio::test]
async fn memfs_passes_in_a_subdirectory_and_cleans_up() {
    let fs = MemFs::new();
    let root = RemotePath::new("/some").unwrap();
    fs.mkdir(&root).await.unwrap();
    contract::run_quick(&fs, &root).await;
    assert!(fs.read_dir(&root).await.unwrap().is_empty());
}

#[tokio::test]
async fn memfs_passes_with_the_limits_of_a_plain_sftp_server() {
    let fs = MemFs::with_capabilities(Capabilities {
        posix_rename: false,
        fsync: false,
        statvfs: false,
        max_read: 32 * 1024,
        max_write: 32 * 1024,
    });
    contract::run_quick(&fs, &RemotePath::root()).await;
}

#[tokio::test]
async fn memfs_passes_with_tiny_and_uneven_limits() {
    let fs = MemFs::with_capabilities(Capabilities {
        max_read: 4099,
        max_write: 65_537,
        ..MemFs::new().capabilities()
    });
    contract::run_quick(&fs, &RemotePath::root()).await;
}

/// The suite's future has to move between worker threads, as a backend's will, and it has to work
/// behind `dyn`, which is how the engine holds a backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suite_runs_on_worker_threads_behind_dyn() {
    let fs: Arc<dyn RemoteFs> = Arc::new(MemFs::new());
    tokio::spawn(async move { contract::run_quick(&*fs, &RemotePath::root()).await })
        .await
        .unwrap();
}

/// One broken promise. `None` must pass, so a panic in the other tests comes from their fault and
/// not from the wrapper.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    WriteOffsetIn32Bits,
    WriteAtZero,
    GapNotZero,
    ReadTooMuch,
    OpenIgnoresExclusive,
    OpenIgnoresTruncate,
    OpenDoesNotCreate,
    ListsDotEntries,
    ListsRecursiveNames,
    RenameAlwaysReplaces,
    RenameNeverReplaces,
    RenameRemovesTargetFirst,
    RemoveDirRemovesFilesToo,
    RemoveFileRemovesDirs,
    MkdirIgnoresExisting,
    SizeIgnored,
    ShrinkKeepsData,
    TimesSwapped,
    MtimeClearsAtime,
    ReadOnlyIgnored,
    ReadOnlyClobbersPermissions,
    WriteToReadOnlyAllowed,
    WriteOnReadHandleAllowed,
    FreeAboveTotal,
    WrongPlaceholder,
}

struct Faulty {
    inner: MemFs,
    fault: Fault,
}

impl Faulty {
    fn has(&self, fault: Fault) -> bool {
        self.fault == fault
    }
}

const FAR_PAST: std::time::SystemTime = std::time::UNIX_EPOCH;

#[async_trait]
impl RemoteFs for Faulty {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.inner.stat(path).await
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.inner.lstat(path).await
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.inner.fstat(handle).await
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        let mut entries = self.inner.read_dir(path).await?;
        let dir = self.inner.stat(path).await?;
        if self.has(Fault::ListsDotEntries) {
            entries.push(DirEntry {
                name: ".".to_owned(),
                attr: dir,
            });
        }
        if self.has(Fault::ListsRecursiveNames) {
            for entry in entries.clone() {
                if entry.attr.is_dir() {
                    let sub = path.join(&entry.name).expect("a valid name");
                    entries.extend(self.inner.read_dir(&sub).await?);
                }
            }
        }
        Ok(entries)
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let replace_disposition = |disposition| match mode.access() {
            Access::ReadWrite => OpenMode::read_write(disposition),
            _ => OpenMode::write(disposition),
        };
        let mode = match mode.disposition() {
            Disposition::CreateNew if self.has(Fault::OpenIgnoresExclusive) => {
                replace_disposition(Disposition::OpenOrCreate)
            }
            Disposition::TruncateExisting if self.has(Fault::OpenIgnoresTruncate) => {
                replace_disposition(Disposition::OpenExisting)
            }
            Disposition::OpenOrCreate if self.has(Fault::OpenDoesNotCreate) => {
                replace_disposition(Disposition::OpenExisting)
            }
            _ => mode,
        };
        if self.has(Fault::WriteToReadOnlyAllowed)
            && let Ok(attr) = self.inner.stat(path).await
            && attr.is_read_only()
        {
            let writable = AttrChanges {
                read_only: Some(false),
                ..AttrChanges::default()
            };
            self.inner
                .set_attr(AttrTarget::Path(path), writable)
                .await?;
        }
        let write_anywhere = self.has(Fault::WriteOnReadHandleAllowed) && mode == OpenMode::read();
        let mode = if write_anywhere {
            OpenMode::read_write(Disposition::OpenExisting)
        } else {
            mode
        };
        self.inner.open(path, mode).await
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        let data = self.inner.read(handle, offset, len).await?;
        if self.has(Fault::ReadTooMuch) && !data.is_empty() {
            let mut more = data.to_vec();
            more.push(0);
            return Ok(Bytes::from(more));
        }
        Ok(data)
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        if self.has(Fault::GapNotZero) {
            let size = self.inner.fstat(handle).await?.size;
            if offset > size && !data.is_empty() {
                let junk = (offset - size).min(1024) as usize;
                self.inner
                    .write(handle, size, Bytes::from(vec![0xAA; junk]))
                    .await?;
            }
        }
        let offset = match self.fault {
            Fault::WriteOffsetIn32Bits => u64::from(offset as u32),
            Fault::WriteAtZero => 0,
            _ => offset,
        };
        self.inner.write(handle, offset, data).await
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.inner.flush(handle).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.inner.close(handle).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, mut changes: AttrChanges) -> FsResult<Attr> {
        match self.fault {
            Fault::SizeIgnored => changes.size = None,
            Fault::ReadOnlyIgnored => changes.read_only = None,
            Fault::TimesSwapped => std::mem::swap(&mut changes.mtime, &mut changes.atime),
            Fault::MtimeClearsAtime if changes.mtime.is_some() => {
                changes.atime = changes.atime.or(Some(FAR_PAST));
            }
            Fault::ShrinkKeepsData => {
                let current = match target {
                    AttrTarget::Path(path) => self.inner.stat(path).await?,
                    AttrTarget::Handle(handle) => self.inner.fstat(handle).await?,
                };
                if changes.size.is_some_and(|size| size < current.size) {
                    changes.size = None;
                }
            }
            _ => {}
        }
        let attr = self.inner.set_attr(target, changes).await?;
        if self.has(Fault::ReadOnlyClobbersPermissions) && changes.read_only.is_some() {
            return Ok(Attr {
                perm: attr.perm.map(|perm| perm & 0o700),
                ..attr
            });
        }
        Ok(attr)
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        if self.has(Fault::MkdirIgnoresExisting)
            && let Ok(attr) = self.inner.stat(path).await
        {
            return Ok(attr);
        }
        self.inner.mkdir(path).await
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        if self.has(Fault::RemoveFileRemovesDirs) && self.inner.stat(path).await?.is_dir() {
            return self.inner.remove_dir(path).await;
        }
        self.inner.remove_file(path).await
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        if self.has(Fault::RemoveDirRemovesFilesToo) {
            for entry in self.inner.read_dir(path).await? {
                if !entry.attr.is_dir() {
                    let file = path.join(&entry.name).expect("a valid name");
                    self.inner.remove_file(&file).await?;
                }
            }
        }
        self.inner.remove_dir(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        let replace = match self.fault {
            Fault::RenameAlwaysReplaces => true,
            Fault::RenameNeverReplaces => false,
            _ => replace,
        };
        // The fallback of spec §5.7 done carelessly, without looking at the source.
        if self.has(Fault::RenameRemovesTargetFirst)
            && replace
            && let Ok(target) = self.inner.stat(to).await
            && !target.is_dir()
        {
            self.inner.remove_file(to).await?;
        }
        self.inner.rename(from, to, replace).await
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        match self.fault {
            Fault::FreeAboveTotal => Ok(FsStats {
                total_bytes: 1,
                free_bytes: 2,
            }),
            Fault::WrongPlaceholder => Ok(FsStats {
                total_bytes: 2,
                free_bytes: 1,
            }),
            _ => self.inner.statfs().await,
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
}

async fn run_with(fault: Fault) {
    // The placeholder is only expected of a session without statvfs.
    let capabilities = Capabilities {
        statvfs: fault != Fault::WrongPlaceholder,
        ..MemFs::new().capabilities()
    };
    let fs = Faulty {
        inner: MemFs::with_capabilities(capabilities),
        fault,
    };
    contract::run_quick(&fs, &RemotePath::root()).await;
}

#[tokio::test]
async fn the_wrapper_is_harmless_without_a_fault() {
    run_with(Fault::None).await;
}

macro_rules! suite_notices {
    ($($name:ident: $fault:ident,)*) => {
        $(
            #[tokio::test]
            #[should_panic]
            async fn $name() {
                run_with(Fault::$fault).await;
            }
        )*
    };
}

suite_notices! {
    notices_a_write_offset_kept_in_32_bits: WriteOffsetIn32Bits,
    notices_a_write_that_ignores_its_offset: WriteAtZero,
    notices_a_gap_that_is_not_zeros: GapNotZero,
    notices_a_read_longer_than_asked: ReadTooMuch,
    notices_exclusive_create_not_being_exclusive: OpenIgnoresExclusive,
    notices_truncate_on_open_not_truncating: OpenIgnoresTruncate,
    notices_open_or_create_not_creating: OpenDoesNotCreate,
    notices_dot_entries_in_a_listing: ListsDotEntries,
    notices_a_listing_going_too_deep: ListsRecursiveNames,
    notices_rename_replacing_when_told_not_to: RenameAlwaysReplaces,
    notices_rename_not_replacing_when_told_to: RenameNeverReplaces,
    notices_rename_removing_the_target_first: RenameRemovesTargetFirst,
    notices_rmdir_removing_files: RemoveDirRemovesFilesToo,
    notices_remove_file_removing_a_directory: RemoveFileRemovesDirs,
    notices_mkdir_accepting_an_existing_name: MkdirIgnoresExisting,
    notices_a_size_change_being_ignored: SizeIgnored,
    notices_shrinking_not_shrinking: ShrinkKeepsData,
    notices_times_being_swapped: TimesSwapped,
    notices_setting_mtime_changing_atime: MtimeClearsAtime,
    notices_the_read_only_bit_being_ignored: ReadOnlyIgnored,
    notices_the_read_only_bit_changing_other_permissions: ReadOnlyClobbersPermissions,
    notices_read_only_files_being_writable: WriteToReadOnlyAllowed,
    notices_writes_on_a_read_handle: WriteOnReadHandleAllowed,
    notices_more_free_space_than_total: FreeAboveTotal,
    notices_a_wrong_placeholder: WrongPlaceholder,
}
