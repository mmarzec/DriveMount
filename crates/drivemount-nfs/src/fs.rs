// SPDX-License-Identifier: GPL-3.0-or-later

//! [`NfsFs`]: [`RemoteFs`] over NFSv3 (spec §5.10, M4-03).
//!
//! Paths are relative to the export, so `/` here is the export's root, whose handle came from
//! MOUNT. Each path becomes a file handle through [`HandleCache`]; an open file keeps the handle it
//! was opened with, which stays valid across renames and reconnects (FR-NFS-04).
//!
//! # What NFSv3 does differently from the trait
//!
//! - **There is no open or close.** Opening a file looks it up, creates it, and checks with ACCESS
//!   that this user may read or write it, as the server does not refuse a write to a read-only
//!   file its owner opened. The access asked for is enforced here.
//! - **Writes are `UNSTABLE`**, and [`flush`](RemoteFs::flush) and [`close`](RemoteFs::close)
//!   send COMMIT. If the server's write verifier changed in between, it restarted and may have
//!   lost what was written since the last COMMIT. That is reported as an error by the flush or close
//!   that finds out; the data is not kept here to be sent again.
//! - **RENAME always replaces**, so a rename that must not replace looks the target up first. The
//!   two calls are not atomic: a file another client creates at the target in between is replaced.
//! - **New files and directories get mode 644 and 755**: the server applies no umask of its own,
//!   and a mode left out gives 000 on Linux knfsd.
//! - **Symbolic links** are followed for the last component of a path, when the target is a
//!   relative path or an absolute one inside the export. A link to anywhere else, and a link in the
//!   middle of a path, count as missing: LOOKUP does not follow them, and the target may not be in
//!   this export at all.
//! - **Names that are not UTF-8** are left out of listings: no path here can name them.
//!
//! # Calls after a broken connection
//!
//! The pool opens a new connection when one breaks. A call that is safe to repeat goes again at
//! once; CREATE, MKDIR, REMOVE, RMDIR and RENAME do not, as the first try may have acted and the
//! second would then fail with `NFS3ERR_EXIST` or `NFS3ERR_NOENT` (spike M4-01 §8). They give
//! `ConnectionLost`, which `SessionFs` does not repeat either: the caller has to look.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use drivemount_vfs::{
    Access, Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FileKind, FsError,
    FsResult, FsStats, OpenMode, RemoteFs, RemoteHandle, RemotePath, apply_read_only,
};
use futures::future::join_all;
use nfs3_types::nfs3::{
    ACCESS3_EXTEND, ACCESS3_MODIFY, ACCESS3_READ, ACCESS3args, COMMIT3args, CREATE3args,
    FSINFO3args, FSSTAT3args, GETATTR3args, LOOKUP3args, MKDIR3args, Nfs3Option, Nfs3Result,
    READ3args, READDIRPLUS3args, READLINK3args, REMOVE3args, RENAME3args, RMDIR3args, SETATTR3args,
    WRITE3args, cookieverf3, createhow3, diropargs3, fattr3, filename3, ftype3, nfs_fh3, nfsstat3,
    nfstime3, sattr3, set_atime, set_mtime, stable_how, writeverf3,
};
use nfs3_types::xdr_codec::Opaque;
use tracing::{debug, warn};

use crate::handles::{FileHandle, HandleCache, HandleError, Lookup, root_stale};
use crate::pool::{Dialer, Identity, Pool, Retry};

/// The largest READ or WRITE, whatever the server allows (spike M4-01 §4).
const MAX_CHUNK: u32 = 1024 * 1024;
/// For a server whose FSINFO gives no size: what RFC 1813 servers commonly take.
const DEFAULT_CHUNK: u32 = 32 * 1024;
/// The bounds on one READDIRPLUS reply.
const MIN_DIR_CHUNK: u32 = 8 * 1024;
const MAX_DIR_CHUNK: u32 = 1024 * 1024;
/// How many times a listing starts over after the directory changed under its cookie.
const LISTING_RESTARTS: usize = 3;
/// How many symbolic links one path may lead through, as Linux's `MAXSYMLINKS` is 40 but 8 is plenty
/// for a share.
const MAX_LINKS: usize = 8;

/// Modes of new files and directories: 666 and 777 less a umask of 022.
const FILE_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;
const PERMISSION_BITS: u32 = 0o7777;

/// How an [`NfsFs`] works. M4-04's connector fills this from the profile.
#[derive(Debug, Clone)]
pub struct NfsOptions {
    /// The AUTH_SYS identity (FR-NFS-01).
    pub identity: Identity,
    /// The export's path on the server, as mounted, for following absolute symbolic links.
    pub export: String,
    /// Connections in the pool (spec §5.10: 8).
    pub connections: usize,
    /// The deadline of one RPC, after which its connection is dropped (`op_timeout_secs`).
    pub call_timeout: Duration,
    /// How long a path's file handle is trusted without a LOOKUP. Another client's rename shows up
    /// after this, as for the attribute cache (spec §5.6).
    pub handle_ttl: Duration,
}

impl NfsOptions {
    /// The pool size of spec §5.10.
    pub const DEFAULT_CONNECTIONS: usize = 8;
}

/// [`RemoteFs`] over NFSv3, below one export's root. See the module's docs.
pub struct NfsFs {
    pool: Pool,
    handles: HandleCache,
    export: String,
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_id: AtomicU64,
    capabilities: Capabilities,
    dir_chunk: u32,
}

struct OpenFile {
    fh: nfs_fh3,
    access: Access,
    unstable: Mutex<Unstable>,
}

/// What an open file has written that may not be on the server's disk yet.
#[derive(Debug, Default)]
struct Unstable {
    /// Writes acknowledged so far, and how many of them a COMMIT has covered.
    written: u64,
    committed: u64,
    /// The server's write verifier with the first uncommitted write.
    verifier: Option<writeverf3>,
    /// A write came back with another verifier: the server restarted in between.
    restarted: bool,
}

impl Unstable {
    fn wrote(&mut self, verifier: writeverf3) {
        self.written += 1;
        match self.verifier {
            None => self.verifier = Some(verifier),
            Some(seen) if seen != verifier => self.restarted = true,
            Some(_) => {}
        }
    }

    fn pending(&self) -> bool {
        self.written > self.committed
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing here panics while holding a lock in a way that leaves the data half changed.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl NfsFs {
    /// A file system below the export whose root handle MOUNT gave. Opens the first connection and
    /// asks FSINFO for the server's transfer sizes.
    ///
    /// # Errors
    ///
    /// Those of FSINFO: a connection that cannot be opened is `ConnectionLost`, a server that
    /// refuses this user `PermissionDenied` (E-NFS-ACCESS, FR-NFS-05).
    pub async fn new(
        dialer: Box<dyn Dialer>,
        root: FileHandle,
        options: NfsOptions,
    ) -> FsResult<Self> {
        let pool = Pool::new(
            dialer,
            options.identity,
            options.connections,
            options.call_timeout,
        );
        let fsroot = to_fh(&root);
        let info = pool
            .call(Retry::Idempotent, |c| {
                let fsroot = fsroot.clone();
                Box::pin(async move { c.fsinfo(&FSINFO3args { fsroot }).await })
            })
            .await?;
        let info = match info {
            Nfs3Result::Ok(info) => info,
            Nfs3Result::Err((stat, _)) => return Err(fs_status(stat)),
        };
        let capabilities = Capabilities {
            posix_rename: true,
            fsync: true,
            statvfs: true,
            max_read: chunk_size(info.rtmax, info.rtpref),
            max_write: chunk_size(info.wtmax, info.wtpref),
        };
        debug!(
            rtmax = info.rtmax,
            wtmax = info.wtmax,
            dtpref = info.dtpref,
            "NFS export opened"
        );
        Ok(Self {
            pool,
            handles: HandleCache::new(root, options.handle_ttl),
            export: options.export.trim_end_matches('/').to_owned(),
            files: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            capabilities,
            dir_chunk: info.dtpref.clamp(MIN_DIR_CHUNK, MAX_DIR_CHUNK),
        })
    }

    fn file(&self, handle: &RemoteHandle) -> FsResult<Arc<OpenFile>> {
        lock(&self.files)
            .get(&handle.id())
            .cloned()
            .ok_or_else(|| FsError::Other("unknown file handle".to_owned()))
    }

    // ---- Paths ----

    /// The handle and attributes of `path`, following a symbolic link at the end if `follow`, with
    /// the path they belong to after following.
    async fn lookup_attr(
        &self,
        path: &RemotePath,
        follow: bool,
    ) -> FsResult<(FileHandle, fattr3, RemotePath)> {
        let mut path = path.clone();
        for _ in 0..=MAX_LINKS {
            let (handle, attr) = self
                .handles
                .with_handle(&self.pool, &path, |handle| async move {
                    let attr = self.getattr(&to_fh(&handle)).await?;
                    Ok((handle, attr))
                })
                .await?;
            if !follow || attr.type_ != ftype3::NF3LNK {
                return Ok((handle, attr, path));
            }
            let target = self.readlink(&to_fh(&handle)).await.map_err(stale_gone)?;
            path = link_target(&self.export, &path, &target)?;
        }
        Err(FsError::Other(
            "too many levels of symbolic links".to_owned(),
        ))
    }

    /// Runs `op` on the file at `path`. If its handle turns out stale, the path is looked up
    /// afresh and `op` runs once more, so `op` must be safe to repeat after a stale handle.
    async fn on_path<T, Op, Fut>(&self, path: &RemotePath, follow: bool, mut op: Op) -> FsResult<T>
    where
        Op: FnMut(nfs_fh3, fattr3) -> Fut,
        Fut: Future<Output = Result<T, HandleError>>,
    {
        for _ in 0..2 {
            let (handle, attr, found) = self.lookup_attr(path, follow).await?;
            match op(to_fh(&handle), attr).await {
                Ok(value) => return Ok(value),
                Err(HandleError::Fs(error)) => return Err(error),
                Err(HandleError::Stale) if found.is_root() => return Err(root_stale()),
                Err(HandleError::Stale) => {
                    debug!(%found, "stale file handle, looking the path up again");
                    self.handles.forget(&found);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /// Runs `op` with the handle of `path`'s parent directory and `path`'s name. A missing parent
    /// is `PathNotFound`. As [`on_path`](Self::on_path), `op` runs again after a stale handle.
    async fn in_parent<T, Op, Fut>(&self, path: &RemotePath, mut op: Op) -> FsResult<T>
    where
        Op: FnMut(nfs_fh3, filename3<'static>) -> Fut,
        Fut: Future<Output = Result<T, HandleError>>,
    {
        let (parent, name) = parent_and_name(path)?;
        for _ in 0..2 {
            let dir = self
                .handles
                .resolve(&self.pool, &parent)
                .await
                .map_err(parent_missing)?;
            match op(to_fh(&dir), name.clone()).await {
                Ok(value) => return Ok(value),
                Err(HandleError::Fs(error)) => return Err(error),
                Err(HandleError::Stale) if parent.is_root() => return Err(root_stale()),
                Err(HandleError::Stale) => self.handles.forget(&parent),
            }
        }
        Err(FsError::PathNotFound)
    }

    /// After a call on `path` that failed in a way that may still have changed it.
    fn forget_if_unsure(&self, path: &RemotePath, error: &FsError) {
        if is_unsure(error) {
            self.handles.forget(path);
        }
    }

    // ---- Calls ----

    async fn getattr(&self, object: &nfs_fh3) -> Result<fattr3, HandleError> {
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let object = object.clone();
                Box::pin(async move { c.getattr(&GETATTR3args { object }).await })
            })
            .await?;
        ok(reply).map(|ok| ok.obj_attributes)
    }

    async fn readlink(&self, symlink: &nfs_fh3) -> Result<Vec<u8>, HandleError> {
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let symlink = symlink.clone();
                Box::pin(async move { c.readlink(&READLINK3args { symlink }).await })
            })
            .await?;
        ok(reply).map(|ok| ok.data.0.into_owned())
    }

    /// Whether this user may do what `access` needs to the file, asked of the server.
    async fn check_access(&self, object: &nfs_fh3, access: Access) -> Result<(), HandleError> {
        let wanted = access_bits(access);
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let object = object.clone();
                Box::pin(async move {
                    c.access(&ACCESS3args {
                        object,
                        access: wanted,
                    })
                    .await
                })
            })
            .await?;
        let granted = ok(reply)?.access;
        if granted & wanted == wanted {
            Ok(())
        } else {
            Err(HandleError::Fs(FsError::PermissionDenied))
        }
    }

    async fn setattr(&self, object: &nfs_fh3, changes: &sattr3) -> Result<Attr, HandleError> {
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let object = object.clone();
                let new_attributes = changes.clone();
                Box::pin(async move {
                    let args = SETATTR3args {
                        object,
                        new_attributes,
                        guard: Nfs3Option::None,
                    };
                    c.setattr(&args).await
                })
            })
            .await?;
        match ok(reply)?.obj_wcc.after {
            Nfs3Option::Some(attr) => Ok(to_attr(&attr)),
            Nfs3Option::None => self.getattr(object).await.map(|attr| to_attr(&attr)),
        }
    }

    /// The attributes `changes` asks for, given the file's current ones where the read-only flag
    /// needs them.
    async fn set_attr_on(
        &self,
        object: &nfs_fh3,
        current: Option<&fattr3>,
        changes: AttrChanges,
    ) -> Result<Attr, HandleError> {
        if changes == AttrChanges::default() {
            return match current {
                Some(attr) => Ok(to_attr(attr)),
                None => self.getattr(object).await.map(|attr| to_attr(&attr)),
            };
        }
        let mode = match changes.read_only {
            None => None,
            Some(read_only) => {
                let fresh;
                let current = match current {
                    Some(current) => current,
                    None => {
                        fresh = self.getattr(object).await?;
                        &fresh
                    }
                };
                Some(apply_read_only(current.mode & PERMISSION_BITS, read_only))
            }
        };
        self.setattr(object, &to_sattr(changes, mode)).await
    }

    /// CREATE GUARDED: the new file's handle.
    async fn create(&self, path: &RemotePath) -> FsResult<FileHandle> {
        let created = self
            .in_parent(path, |dir, name| async move {
                let reply = self
                    .pool
                    .call(Retry::Once, |c| {
                        let (dir, name) = (dir.clone(), name.clone());
                        Box::pin(async move {
                            let how = createhow3::GUARDED(sattr3 {
                                mode: Nfs3Option::Some(FILE_MODE),
                                ..sattr3::default()
                            });
                            let args = CREATE3args {
                                where_: diropargs3 { dir, name },
                                how,
                            };
                            c.create(&args).await
                        })
                    })
                    .await?;
                let made = ok(reply)?;
                match made.obj {
                    Nfs3Option::Some(fh) => Ok(Some(from_fh(&fh))),
                    Nfs3Option::None => Ok(None),
                }
            })
            .await;
        match created {
            Ok(Some(handle)) => {
                self.handles.insert(path, handle.clone());
                Ok(handle)
            }
            // The server need not say: look it up.
            Ok(None) => {
                self.handles.forget(path);
                self.handles.resolve(&self.pool, path).await
            }
            Err(error) => {
                self.forget_if_unsure(path, &error);
                Err(error)
            }
        }
    }

    /// The handle of an existing file at `path`, after checking `access` and emptying it if asked.
    async fn open_existing(
        &self,
        path: &RemotePath,
        access: Access,
        truncate: bool,
    ) -> FsResult<nfs_fh3> {
        self.on_path(path, true, |fh, _attr| async move {
            self.check_access(&fh, access).await?;
            if truncate {
                let empty = sattr3 {
                    size: Nfs3Option::Some(0),
                    ..sattr3::default()
                };
                self.setattr(&fh, &empty).await?;
            }
            Ok(fh)
        })
        .await
    }

    async fn read_dir_of(
        &self,
        path: &RemotePath,
        dir: &nfs_fh3,
    ) -> Result<Vec<DirEntry>, HandleError> {
        let mut restarts = 0;
        'listing: loop {
            let mut entries = Vec::new();
            let mut cookie = 0;
            let mut cookieverf = cookieverf3::default();
            loop {
                let chunk = self.dir_chunk;
                let reply = self
                    .pool
                    .call(Retry::Idempotent, |c| {
                        let dir = dir.clone();
                        Box::pin(async move {
                            let args = READDIRPLUS3args {
                                dir,
                                cookie,
                                cookieverf,
                                dircount: chunk,
                                maxcount: chunk,
                            };
                            c.readdirplus(&args).await
                        })
                    })
                    .await?;
                let page = match reply {
                    Nfs3Result::Ok(page) => page,
                    Nfs3Result::Err((nfsstat3::NFS3ERR_BAD_COOKIE, _))
                        if restarts < LISTING_RESTARTS =>
                    {
                        debug!(%path, "the directory changed while it was listed; starting again");
                        restarts += 1;
                        continue 'listing;
                    }
                    Nfs3Result::Err((stat, _)) => return Err(status(stat)),
                };
                cookieverf = page.cookieverf;
                let eof = page.reply.eof;
                let listed = page.reply.entries.0;
                if listed.is_empty() && !eof {
                    return Err(HandleError::Fs(FsError::Other(
                        "NFS: the server sent an empty part of a listing".to_owned(),
                    )));
                }
                for entry in listed {
                    cookie = entry.cookie;
                    let Ok(name) = String::from_utf8(entry.name.0.into_owned()) else {
                        debug!(%path, "left out a name that is not UTF-8");
                        continue;
                    };
                    if name == "." || name == ".." {
                        continue;
                    }
                    let Ok(child) = path.join(&name) else {
                        continue;
                    };
                    if let Nfs3Option::Some(fh) = &entry.name_handle {
                        self.handles.insert(&child, from_fh(fh));
                    }
                    let attr = match entry.name_attributes {
                        Nfs3Option::Some(attr) => to_attr(&attr),
                        // READDIRPLUS may leave some out.
                        Nfs3Option::None => match self.lookup_attr(&child, false).await {
                            Ok((_, attr, _)) => to_attr(&attr),
                            // Gone since the listing was made.
                            Err(FsError::NotFound) => continue,
                            Err(error) => return Err(HandleError::Fs(error)),
                        },
                    };
                    entries.push(DirEntry { name, attr });
                }
                if eof {
                    return Ok(entries);
                }
            }
        }
    }

    async fn write_chunk(&self, file: &OpenFile, mut offset: u64, mut data: Bytes) -> FsResult<()> {
        while !data.is_empty() {
            let reply = self
                .pool
                .call(Retry::Idempotent, |c| {
                    let (file, data) = (file.fh.clone(), data.clone());
                    Box::pin(async move {
                        let args = WRITE3args {
                            file,
                            offset,
                            count: data.len() as u32,
                            stable: stable_how::UNSTABLE,
                            data: Opaque::borrowed(&data),
                        };
                        c.write(&args).await
                    })
                })
                .await?;
            let written = match reply {
                Nfs3Result::Ok(written) => written,
                Nfs3Result::Err((stat, _)) => return Err(file_status(stat)),
            };
            lock(&file.unstable).wrote(written.verf);
            let count = (written.count as usize).min(data.len());
            if count == 0 {
                return Err(FsError::Other(
                    "NFS: the server accepted none of a write".to_owned(),
                ));
            }
            data = data.slice(count..);
            offset += count as u64;
        }
        Ok(())
    }

    /// COMMIT what `file` has written, if anything.
    async fn commit(&self, file: &OpenFile) -> FsResult<()> {
        let covered = {
            let unstable = lock(&file.unstable);
            if !unstable.pending() {
                return Ok(());
            }
            unstable.written
        };
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let file = file.fh.clone();
                Box::pin(async move {
                    let args = COMMIT3args {
                        file,
                        offset: 0,
                        count: 0,
                    };
                    c.commit(&args).await
                })
            })
            .await?;
        let verifier = match reply {
            Nfs3Result::Ok(committed) => committed.verf,
            Nfs3Result::Err((stat, _)) => return Err(file_status(stat)),
        };
        let mut unstable = lock(&file.unstable);
        unstable.committed = unstable.committed.max(covered);
        let lost = unstable.restarted || unstable.verifier.is_some_and(|seen| seen != verifier);
        // Writes after this COMMIT start a new run, with whatever verifier they bring.
        unstable.verifier = None;
        unstable.restarted = false;
        if unstable.pending() {
            unstable.verifier = Some(verifier);
        }
        if lost {
            warn!("the NFS server restarted before written data reached its disk");
            return Err(FsError::Other(
                "the NFS server restarted; data written since the last flush may be lost"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn lookup_fresh(&self, path: &RemotePath) -> FsResult<FileHandle> {
        // Not from the cache: whether the name is taken is the question.
        self.handles.forget(path);
        self.handles.resolve(&self.pool, path).await
    }

    async fn rename_call(&self, from: &RemotePath, to: &RemotePath) -> FsResult<()> {
        let (from_dir, from_name) = parent_and_name(from)?;
        let (to_dir, to_name) = parent_and_name(to)?;
        for _ in 0..2 {
            let source = self.handles.resolve(&self.pool, &from_dir).await;
            let source = to_fh(&source.map_err(parent_missing)?);
            let target = self.handles.resolve(&self.pool, &to_dir).await;
            let target = to_fh(&target.map_err(parent_missing)?);
            let reply = self
                .pool
                .call(Retry::Once, |c| {
                    let from = diropargs3 {
                        dir: source.clone(),
                        name: from_name.clone(),
                    };
                    let to = diropargs3 {
                        dir: target.clone(),
                        name: to_name.clone(),
                    };
                    Box::pin(async move { c.rename(&RENAME3args { from, to }).await })
                })
                .await?;
            match ok(reply) {
                Ok(_) => return Ok(()),
                Err(HandleError::Fs(error)) => return Err(error),
                Err(HandleError::Stale) if from_dir.is_root() && to_dir.is_root() => {
                    return Err(root_stale());
                }
                Err(HandleError::Stale) => {
                    self.handles.forget(&from_dir);
                    self.handles.forget(&to_dir);
                }
            }
        }
        Err(FsError::PathNotFound)
    }
}

impl Lookup for Pool {
    fn lookup(
        &self,
        dir: &FileHandle,
        name: &str,
    ) -> impl Future<Output = Result<FileHandle, HandleError>> + Send {
        let dir = to_fh(dir);
        let name = filename3::from(name.as_bytes().to_vec());
        async move {
            let reply = self
                .call(Retry::Idempotent, |c| {
                    let what = diropargs3 {
                        dir: dir.clone(),
                        name: name.clone(),
                    };
                    Box::pin(async move { c.lookup(&LOOKUP3args { what }).await })
                })
                .await?;
            ok(reply).map(|found| from_fh(&found.object))
        }
    }
}

#[async_trait]
impl RemoteFs for NfsFs {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        let (_, attr, _) = self.lookup_attr(path, true).await?;
        Ok(to_attr(&attr))
    }

    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        let (_, attr, _) = self.lookup_attr(path, false).await?;
        Ok(to_attr(&attr))
    }

    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        let file = self.file(handle)?;
        let attr = self.getattr(&file.fh).await.map_err(stale_gone)?;
        Ok(to_attr(&attr))
    }

    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.on_path(path, true, |dir, attr| async move {
            if attr.type_ != ftype3::NF3DIR {
                return Err(HandleError::Fs(FsError::PathNotFound));
            }
            self.read_dir_of(path, &dir).await
        })
        .await
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        let access = mode.access();
        let fh = match mode.disposition() {
            Disposition::OpenExisting => self.open_existing(path, access, false).await?,
            Disposition::TruncateExisting => self.open_existing(path, access, true).await?,
            Disposition::CreateNew => to_fh(&self.create(path).await?),
            Disposition::OpenOrCreate | Disposition::CreateOrTruncate => {
                let truncate = mode.disposition() == Disposition::CreateOrTruncate;
                match self.open_existing(path, access, truncate).await {
                    Ok(fh) => fh,
                    Err(FsError::NotFound) => match self.create(path).await {
                        Ok(handle) => to_fh(&handle),
                        // Another client was quicker.
                        Err(FsError::Exists) => self.open_existing(path, access, truncate).await?,
                        Err(error) => return Err(error),
                    },
                    Err(error) => return Err(error),
                }
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let file = Arc::new(OpenFile {
            fh,
            access,
            unstable: Mutex::new(Unstable::default()),
        });
        lock(&self.files).insert(id, file);
        Ok(RemoteHandle::new(id))
    }

    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        let file = self.file(handle)?;
        if file.access == Access::Write {
            return Err(FsError::PermissionDenied);
        }
        // A shorter read is allowed; the read-ahead layer keeps several in flight.
        let count = len.min(self.capabilities.max_read);
        if count == 0 {
            return Ok(Bytes::new());
        }
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let file = file.fh.clone();
                Box::pin(async move {
                    c.read(&READ3args {
                        file,
                        offset,
                        count,
                    })
                    .await
                })
            })
            .await?;
        match reply {
            Nfs3Result::Ok(read) => {
                let mut data = read.data.0.into_owned();
                data.truncate(count as usize);
                Ok(Bytes::from(data))
            }
            Nfs3Result::Err((stat, _)) => Err(file_status(stat)),
        }
    }

    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        let file = self.file(handle)?;
        if file.access == Access::Read {
            return Err(FsError::PermissionDenied);
        }
        let chunk = self.capabilities.max_write as usize;
        let file = file.as_ref();
        let parts = (0..data.len()).step_by(chunk).map(|start| {
            let end = (start + chunk).min(data.len());
            self.write_chunk(file, offset + start as u64, data.slice(start..end))
        });
        join_all(parts).await.into_iter().collect()
    }

    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        let file = self.file(handle)?;
        self.commit(&file).await
    }

    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        let file = lock(&self.files).remove(&handle.id());
        let file = file.ok_or_else(|| FsError::Other("unknown file handle".to_owned()))?;
        self.commit(&file).await
    }

    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        match target {
            AttrTarget::Path(path) => {
                self.on_path(path, true, |fh, attr| async move {
                    self.set_attr_on(&fh, Some(&attr), changes).await
                })
                .await
            }
            AttrTarget::Handle(handle) => {
                let file = self.file(handle)?;
                let set = self.set_attr_on(&file.fh, None, changes).await;
                set.map_err(stale_gone)
            }
        }
    }

    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        let made = self
            .in_parent(path, |dir, name| async move {
                let reply = self
                    .pool
                    .call(Retry::Once, |c| {
                        let (dir, name) = (dir.clone(), name.clone());
                        Box::pin(async move {
                            let args = MKDIR3args {
                                where_: diropargs3 { dir, name },
                                attributes: sattr3 {
                                    mode: Nfs3Option::Some(DIR_MODE),
                                    ..sattr3::default()
                                },
                            };
                            c.mkdir(&args).await
                        })
                    })
                    .await?;
                let made = ok(reply)?;
                let handle = match made.obj {
                    Nfs3Option::Some(fh) => Some(from_fh(&fh)),
                    Nfs3Option::None => None,
                };
                let attr = match made.obj_attributes {
                    Nfs3Option::Some(attr) => Some(to_attr(&attr)),
                    Nfs3Option::None => None,
                };
                Ok((handle, attr))
            })
            .await;
        match made {
            Ok((handle, attr)) => {
                match handle {
                    Some(handle) => self.handles.insert(path, handle),
                    None => self.handles.forget(path),
                }
                match attr {
                    Some(attr) => Ok(attr),
                    None => self.lstat(path).await,
                }
            }
            Err(error) => {
                self.forget_if_unsure(path, &error);
                Err(error)
            }
        }
    }

    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        let removed = self
            .in_parent(path, |dir, name| async move {
                let reply = self
                    .pool
                    .call(Retry::Once, |c| {
                        let object = diropargs3 {
                            dir: dir.clone(),
                            name: name.clone(),
                        };
                        Box::pin(async move { c.remove(&REMOVE3args { object }).await })
                    })
                    .await?;
                ok(reply).map(drop)
            })
            .await;
        if removed.is_ok() {
            self.handles.forget(path);
        } else if let Err(error) = &removed {
            self.forget_if_unsure(path, error);
        }
        removed
    }

    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        let removed = self
            .in_parent(path, |dir, name| async move {
                let reply = self
                    .pool
                    .call(Retry::Once, |c| {
                        let object = diropargs3 {
                            dir: dir.clone(),
                            name: name.clone(),
                        };
                        Box::pin(async move { c.rmdir(&RMDIR3args { object }).await })
                    })
                    .await?;
                ok(reply).map(drop)
            })
            .await;
        if removed.is_ok() {
            self.handles.forget(path);
        } else if let Err(error) = &removed {
            self.forget_if_unsure(path, error);
        }
        removed
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        if from == to {
            self.lstat(from).await?;
            return if replace {
                Ok(())
            } else {
                Err(FsError::Exists)
            };
        }
        if !replace {
            // The source first: a missing source is `NotFound` whatever the target.
            self.handles.resolve(&self.pool, from).await?;
            match self.lookup_fresh(to).await {
                Ok(_) => return Err(FsError::Exists),
                Err(FsError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        let renamed = self.rename_call(from, to).await;
        match &renamed {
            Ok(()) => self.handles.renamed(from, to),
            Err(error) if is_unsure(error) => {
                self.handles.forget(from);
                self.handles.forget(to);
            }
            Err(_) => {}
        }
        renamed
    }

    async fn statfs(&self) -> FsResult<FsStats> {
        let fsroot = to_fh(self.handles.root());
        let reply = self
            .pool
            .call(Retry::Idempotent, |c| {
                let fsroot = fsroot.clone();
                Box::pin(async move { c.fsstat(&FSSTAT3args { fsroot }).await })
            })
            .await?;
        match reply {
            Nfs3Result::Ok(stats) => Ok(FsStats {
                total_bytes: stats.tbytes,
                free_bytes: stats.abytes.min(stats.tbytes),
            }),
            Nfs3Result::Err((stat, _)) => Err(fs_status(stat)),
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
}

// ---- Conversions ----

fn to_fh(handle: &FileHandle) -> nfs_fh3 {
    nfs_fh3 {
        data: Opaque::owned(handle.as_bytes().to_vec()),
    }
}

fn from_fh(fh: &nfs_fh3) -> FileHandle {
    FileHandle::new(&fh.data)
}

fn parent_and_name(path: &RemotePath) -> FsResult<(RemotePath, filename3<'static>)> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, filename3::from(name.as_bytes().to_vec()))),
        // The export's root cannot be created, removed or renamed.
        _ => Err(FsError::PermissionDenied),
    }
}

/// The parent of a path not being there makes the path's own error `PathNotFound`.
fn parent_missing(error: FsError) -> FsError {
    match error {
        FsError::NotFound => FsError::PathNotFound,
        error => error,
    }
}

/// An open file's handle going stale means the file was removed: by another client, or by this
/// mount while it was open, which NFSv3 cannot keep alive as a local file system would.
fn stale_gone(error: HandleError) -> FsError {
    match error {
        HandleError::Stale => FsError::NotFound,
        HandleError::Fs(error) => error,
    }
}

/// Whether a failed call may have acted on the server all the same.
fn is_unsure(error: &FsError) -> bool {
    matches!(
        error,
        FsError::Timeout | FsError::ConnectionLost | FsError::Other(_)
    )
}

/// Where a symbolic link at `link` pointing to `target` leads, within the export at `export`
/// (without a trailing `/`). A target outside it is `NotFound`.
fn link_target(export: &str, link: &RemotePath, target: &[u8]) -> FsResult<RemotePath> {
    let target = std::str::from_utf8(target).map_err(|_| FsError::NotFound)?;
    let resolved = if target.starts_with('/') {
        let inside = target
            .strip_prefix(export)
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .ok_or(FsError::NotFound)?;
        RemotePath::new(if inside.is_empty() { "/" } else { inside })
    } else {
        link.parent().unwrap_or_else(RemotePath::root).join(target)
    };
    resolved.map_err(|_| FsError::NotFound)
}

fn ok<T, E>(reply: Nfs3Result<T, E>) -> Result<T, HandleError> {
    match reply {
        Nfs3Result::Ok(value) => Ok(value),
        Nfs3Result::Err((stat, _)) => Err(status(stat)),
    }
}

fn status(stat: nfsstat3) -> HandleError {
    match stat {
        nfsstat3::NFS3ERR_STALE | nfsstat3::NFS3ERR_BADHANDLE => HandleError::Stale,
        stat => HandleError::Fs(fs_status(stat)),
    }
}

/// An error about an open file's handle.
fn file_status(stat: nfsstat3) -> FsError {
    stale_gone(status(stat))
}

/// Spec §5.8's errors for an NFS status.
fn fs_status(stat: nfsstat3) -> FsError {
    match stat {
        nfsstat3::NFS3ERR_NOENT | nfsstat3::NFS3ERR_STALE | nfsstat3::NFS3ERR_BADHANDLE => {
            FsError::NotFound
        }
        // A component on the way is a file.
        nfsstat3::NFS3ERR_NOTDIR => FsError::PathNotFound,
        nfsstat3::NFS3ERR_PERM | nfsstat3::NFS3ERR_ACCES => FsError::PermissionDenied,
        nfsstat3::NFS3ERR_EXIST => FsError::Exists,
        nfsstat3::NFS3ERR_NOTEMPTY => FsError::NotEmpty,
        nfsstat3::NFS3ERR_ROFS => FsError::ReadOnly,
        nfsstat3::NFS3ERR_NOSPC | nfsstat3::NFS3ERR_DQUOT => FsError::NoSpace,
        nfsstat3::NFS3ERR_NOTSUPP => FsError::NotSupported,
        stat => FsError::Other(format!("NFS: {stat}")),
    }
}

fn kind(kind: ftype3) -> FileKind {
    match kind {
        ftype3::NF3REG => FileKind::File,
        ftype3::NF3DIR => FileKind::Dir,
        ftype3::NF3LNK => FileKind::Symlink,
        _ => FileKind::Other,
    }
}

fn to_attr(attr: &fattr3) -> Attr {
    Attr {
        kind: kind(attr.type_),
        size: attr.size,
        perm: Some(attr.mode & PERMISSION_BITS),
        mtime: Some(SystemTime::from(&attr.mtime)),
        atime: Some(SystemTime::from(&attr.atime)),
    }
}

/// NFSv3 times are unsigned 32-bit seconds, so anything earlier than 1970 is held there.
fn to_time(time: SystemTime) -> nfstime3 {
    nfstime3::try_from(time).unwrap_or_default()
}

fn to_sattr(changes: AttrChanges, mode: Option<u32>) -> sattr3 {
    sattr3 {
        mode: mode.map_or(Nfs3Option::None, Nfs3Option::Some),
        size: changes.size.map_or(Nfs3Option::None, Nfs3Option::Some),
        mtime: changes.mtime.map_or(set_mtime::DONT_CHANGE, |time| {
            set_mtime::SET_TO_CLIENT_TIME(to_time(time))
        }),
        atime: changes.atime.map_or(set_atime::DONT_CHANGE, |time| {
            set_atime::SET_TO_CLIENT_TIME(to_time(time))
        }),
        ..sattr3::default()
    }
}

fn access_bits(access: Access) -> u32 {
    match access {
        Access::Read => ACCESS3_READ,
        Access::Write => ACCESS3_MODIFY | ACCESS3_EXTEND,
        Access::ReadWrite => ACCESS3_READ | ACCESS3_MODIFY | ACCESS3_EXTEND,
    }
}

/// One READ or WRITE: the server's largest, else its preferred, up to [`MAX_CHUNK`].
fn chunk_size(max: u32, preferred: u32) -> u32 {
    let size = if max > 0 {
        max
    } else if preferred > 0 {
        preferred
    } else {
        DEFAULT_CHUNK
    };
    size.min(MAX_CHUNK)
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use nfs3_types::nfs3::specdata3;

    use super::*;

    #[test]
    fn statuses_map_to_errors() {
        let cases = [
            (nfsstat3::NFS3ERR_NOENT, FsError::NotFound),
            (nfsstat3::NFS3ERR_NOTDIR, FsError::PathNotFound),
            (nfsstat3::NFS3ERR_PERM, FsError::PermissionDenied),
            (nfsstat3::NFS3ERR_ACCES, FsError::PermissionDenied),
            (nfsstat3::NFS3ERR_EXIST, FsError::Exists),
            (nfsstat3::NFS3ERR_NOTEMPTY, FsError::NotEmpty),
            (nfsstat3::NFS3ERR_ROFS, FsError::ReadOnly),
            (nfsstat3::NFS3ERR_NOSPC, FsError::NoSpace),
            (nfsstat3::NFS3ERR_DQUOT, FsError::NoSpace),
            (nfsstat3::NFS3ERR_NOTSUPP, FsError::NotSupported),
        ];
        for (stat, expected) in cases {
            assert_eq!(fs_status(stat), expected, "{stat}");
            assert_eq!(status(stat), HandleError::Fs(expected), "{stat}");
        }
        assert_eq!(
            fs_status(nfsstat3::NFS3ERR_IO),
            FsError::Other("NFS: NFS3ERR_IO".to_owned())
        );
    }

    #[test]
    fn a_stale_handle_is_looked_up_again_or_gone() {
        assert_eq!(status(nfsstat3::NFS3ERR_STALE), HandleError::Stale);
        assert_eq!(status(nfsstat3::NFS3ERR_BADHANDLE), HandleError::Stale);
        assert_eq!(file_status(nfsstat3::NFS3ERR_STALE), FsError::NotFound);
    }

    #[test]
    fn a_missing_parent_makes_the_path_not_found() {
        assert_eq!(parent_missing(FsError::NotFound), FsError::PathNotFound);
        assert_eq!(parent_missing(FsError::Timeout), FsError::Timeout);
        assert!(parent_and_name(&RemotePath::root()).is_err());
        let (parent, name) = parent_and_name(&RemotePath::new("/a/b").unwrap()).unwrap();
        assert_eq!(parent.as_str(), "/a");
        assert_eq!(name.as_ref(), b"b");
    }

    fn fattr(type_: ftype3, mode: u32) -> fattr3 {
        fattr3 {
            type_,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size: 7,
            used: 4096,
            rdev: specdata3::default(),
            fsid: 1,
            fileid: 2,
            atime: nfstime3 {
                seconds: 6,
                nseconds: 500,
            },
            mtime: nfstime3 {
                seconds: 5,
                nseconds: 0,
            },
            ctime: nfstime3::default(),
        }
    }

    #[test]
    fn attributes_convert() {
        let attr = to_attr(&fattr(ftype3::NF3REG, 0o104_755));
        assert_eq!(attr.kind, FileKind::File);
        assert_eq!(attr.size, 7);
        assert_eq!(attr.perm, Some(0o4755));
        assert_eq!(attr.mtime, Some(UNIX_EPOCH + Duration::from_secs(5)));
        assert_eq!(
            attr.atime,
            Some(UNIX_EPOCH + Duration::from_secs(6) + Duration::from_nanos(500))
        );
        assert_eq!(to_attr(&fattr(ftype3::NF3DIR, 0o755)).kind, FileKind::Dir);
        assert_eq!(
            to_attr(&fattr(ftype3::NF3LNK, 0o777)).kind,
            FileKind::Symlink
        );
        assert_eq!(
            to_attr(&fattr(ftype3::NF3SOCK, 0o755)).kind,
            FileKind::Other
        );
    }

    #[test]
    fn changes_set_only_what_they_name() {
        let changes = AttrChanges {
            size: Some(9),
            mtime: Some(UNIX_EPOCH + Duration::from_secs(30)),
            ..AttrChanges::default()
        };
        let sattr = to_sattr(changes, Some(0o444));
        assert_eq!(sattr.mode, Nfs3Option::Some(0o444));
        assert_eq!(sattr.size, Nfs3Option::Some(9));
        assert_eq!(sattr.uid, Nfs3Option::None);
        assert!(matches!(
            sattr.mtime,
            set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                seconds: 30,
                nseconds: 0
            })
        ));
        assert!(matches!(sattr.atime, set_atime::DONT_CHANGE));
    }

    #[test]
    fn times_before_1970_are_held_at_zero() {
        assert_eq!(
            to_time(UNIX_EPOCH - Duration::from_secs(9)),
            nfstime3::default()
        );
    }

    #[test]
    fn chunk_sizes_follow_the_server() {
        assert_eq!(chunk_size(1024 * 1024, 1024 * 1024), MAX_CHUNK);
        assert_eq!(chunk_size(64 * 1024 * 1024, 0), MAX_CHUNK);
        assert_eq!(chunk_size(65_536, 32_768), 65_536);
        assert_eq!(chunk_size(0, 8_192), 8_192);
        assert_eq!(chunk_size(0, 0), DEFAULT_CHUNK);
    }

    #[test]
    fn access_asks_for_what_the_mode_needs() {
        assert_eq!(access_bits(Access::Read), ACCESS3_READ);
        assert_eq!(access_bits(Access::Write), ACCESS3_MODIFY | ACCESS3_EXTEND);
        assert_eq!(
            access_bits(Access::ReadWrite),
            ACCESS3_READ | ACCESS3_MODIFY | ACCESS3_EXTEND
        );
    }

    #[test]
    fn a_changed_write_verifier_is_noticed() {
        let mut unstable = Unstable::default();
        assert!(!unstable.pending());
        unstable.wrote(writeverf3([1; 8]));
        unstable.wrote(writeverf3([1; 8]));
        assert!(unstable.pending() && !unstable.restarted);
        unstable.wrote(writeverf3([2; 8]));
        assert!(unstable.restarted);
    }

    fn target(export: &str, to: &str) -> FsResult<String> {
        let link = RemotePath::new("/dir/link").unwrap();
        let export = export.trim_end_matches('/');
        link_target(export, &link, to.as_bytes()).map(|path| path.as_str().to_owned())
    }

    #[test]
    fn link_targets_stay_inside_the_export() {
        assert_eq!(target("/srv/nfs/rw", "file").unwrap(), "/dir/file");
        assert_eq!(target("/srv/nfs/rw", "../top").unwrap(), "/top");
        assert_eq!(target("/srv/nfs/rw", "../../out"), Err(FsError::NotFound));
        assert_eq!(target("/srv/nfs/rw", "/srv/nfs/rw/a/b").unwrap(), "/a/b");
        assert_eq!(target("/srv/nfs/rw/", "/srv/nfs/rw").unwrap(), "/");
        assert_eq!(
            target("/srv/nfs/rw", "/srv/nfs/rwx/a"),
            Err(FsError::NotFound)
        );
        assert_eq!(target("/srv/nfs/rw", "/etc/passwd"), Err(FsError::NotFound));
        assert_eq!(target("/", "/etc/passwd").unwrap(), "/etc/passwd");
    }
}
