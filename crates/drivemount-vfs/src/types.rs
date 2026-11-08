// SPDX-License-Identifier: GPL-3.0-or-later

//! The plain data that passes through [`RemoteFs`](crate::RemoteFs).

use std::time::SystemTime;

use crate::RemotePath;

/// The owner-write permission bit, which decides the Windows read-only attribute (spec §5.6).
const OWNER_WRITE: u32 = 0o200;

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link, seen only when it is not followed ([`RemoteFs::lstat`] and
    /// [`RemoteFs::read_dir`]).
    ///
    /// [`RemoteFs::lstat`]: crate::RemoteFs::lstat
    /// [`RemoteFs::read_dir`]: crate::RemoteFs::read_dir
    Symlink,
    /// Anything else, such as a socket or a device.
    Other,
}

/// The attributes of a remote file.
///
/// SFTP v3 makes everything but the kind optional, so a server may leave these out. Times have the
/// server's resolution, which is 1 s on SFTP v3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    /// What this is.
    pub kind: FileKind,
    /// Size in bytes; 0 when the server does not report it.
    pub size: u64,
    /// Permission bits (`mode & 0o7777`), if reported.
    pub perm: Option<u32>,
    /// Last modification time, if reported.
    pub mtime: Option<SystemTime>,
    /// Last access time, if reported.
    pub atime: Option<SystemTime>,
}

impl Attr {
    /// Whether this is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind == FileKind::Dir
    }

    /// Whether Windows should show the read-only attribute: a regular file whose reported
    /// permissions lack the owner-write bit (spec §5.6).
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.kind == FileKind::File && self.perm.is_some_and(|perm| perm & OWNER_WRITE == 0)
    }
}

/// `perm` with the owner-write bit changed to match a new Windows read-only attribute: setting
/// read-only clears the bit and clearing read-only sets it (spec §5.6). Backends apply this to
/// permissions fetched fresh from the server, when handling [`AttrChanges::read_only`].
#[must_use]
pub const fn apply_read_only(perm: u32, read_only: bool) -> u32 {
    if read_only {
        perm & !OWNER_WRITE
    } else {
        perm | OWNER_WRITE
    }
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's name on the server, before any name mapping.
    pub name: String,
    /// The entry's attributes; symbolic links are not followed.
    pub attr: Attr,
}

/// The access an open handle has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Reading only.
    Read,
    /// Writing only.
    Write,
    /// Reading and writing.
    ReadWrite,
}

/// What [`RemoteFs::open`](crate::RemoteFs::open) does about the file's existence and contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// Open an existing file, or fail with `NotFound`.
    OpenExisting,
    /// Create a new file, or fail with `Exists`.
    CreateNew,
    /// Open the file, creating it if needed.
    OpenOrCreate,
    /// Open an existing file and empty it, or fail with `NotFound`.
    TruncateExisting,
    /// Open the file and empty it, creating it if needed.
    CreateOrTruncate,
}

/// How to open a file.
///
/// The constructors rule out modes that make no sense, such as creating a file for reading only,
/// so backends don't have to reject them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenMode {
    access: Access,
    disposition: Disposition,
}

impl OpenMode {
    /// Read an existing file.
    #[must_use]
    pub const fn read() -> Self {
        Self {
            access: Access::Read,
            disposition: Disposition::OpenExisting,
        }
    }

    /// Write only. Needed for files whose permissions forbid reading.
    #[must_use]
    pub const fn write(disposition: Disposition) -> Self {
        Self {
            access: Access::Write,
            disposition,
        }
    }

    /// Read and write.
    #[must_use]
    pub const fn read_write(disposition: Disposition) -> Self {
        Self {
            access: Access::ReadWrite,
            disposition,
        }
    }

    /// The access the handle will have.
    #[must_use]
    pub const fn access(&self) -> Access {
        self.access
    }

    /// What happens to the file on open.
    #[must_use]
    pub const fn disposition(&self) -> Disposition {
        self.disposition
    }
}

/// An open remote file.
///
/// Only meaningful to the [`RemoteFs`](crate::RemoteFs) that issued it. It cannot be cloned, because
/// [`close`](crate::RemoteFs::close) consumes it.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct RemoteHandle(u64);

impl RemoteHandle {
    /// Wraps the identifier a backend chose, typically a key into its handle table.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The identifier passed to [`new`](Self::new).
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.0
    }
}

/// What [`RemoteFs::set_attr`](crate::RemoteFs::set_attr) changes, by path or by open handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrTarget<'a> {
    /// The file at this path, following symbolic links.
    Path(&'a RemotePath),
    /// This open file.
    Handle(&'a RemoteHandle),
}

/// The attributes to change. `None` leaves an attribute as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttrChanges {
    /// New size: truncates or extends.
    pub size: Option<u64>,
    /// New Windows read-only attribute. A flag rather than permission bits, so the backend works
    /// from the server's current permissions instead of a possibly stale cached copy.
    pub read_only: Option<bool>,
    /// New modification time.
    pub mtime: Option<SystemTime>,
    /// New access time.
    pub atime: Option<SystemTime>,
}

/// Space on the remote file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    /// Total size in bytes.
    pub total_bytes: u64,
    /// Bytes available to this user, which can be less than the bytes actually free.
    pub free_bytes: u64,
}

impl FsStats {
    /// What a session without [`Capabilities::statvfs`] reports: 1 PiB of each, so Windows' copy
    /// pre-checks never fail (spec §5.7).
    pub const PLACEHOLDER: Self = Self {
        total_bytes: 1 << 50,
        free_bytes: 1 << 50,
    };
}

/// What a session can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Rename can replace its target atomically. Otherwise the target is removed first.
    pub posix_rename: bool,
    /// [`flush`](crate::RemoteFs::flush) reaches stable storage. Otherwise it does nothing.
    pub fsync: bool,
    /// [`statfs`](crate::RemoteFs::statfs) reports real numbers. Otherwise it reports
    /// [`FsStats::PLACEHOLDER`].
    pub statvfs: bool,
    /// Largest single read request in bytes.
    pub max_read: u32,
    /// Largest single write request in bytes.
    pub max_write: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(kind: FileKind, perm: Option<u32>) -> Attr {
        Attr {
            kind,
            size: 0,
            perm,
            mtime: None,
            atime: None,
        }
    }

    #[test]
    fn read_only_follows_the_owner_write_bit_of_regular_files() {
        assert!(attr(FileKind::File, Some(0o444)).is_read_only());
        assert!(attr(FileKind::File, Some(0o577)).is_read_only());
        assert!(!attr(FileKind::File, Some(0o644)).is_read_only());
        assert!(!attr(FileKind::File, Some(0o200)).is_read_only());
    }

    #[test]
    fn only_regular_files_with_known_permissions_are_read_only() {
        assert!(!attr(FileKind::File, None).is_read_only());
        assert!(!attr(FileKind::Dir, Some(0o555)).is_read_only());
        assert!(!attr(FileKind::Symlink, Some(0o444)).is_read_only());
        assert!(!attr(FileKind::Other, Some(0o444)).is_read_only());
    }

    #[test]
    fn apply_read_only_changes_only_the_owner_write_bit() {
        assert_eq!(apply_read_only(0o644, true), 0o444);
        assert_eq!(apply_read_only(0o444, false), 0o644);
        assert_eq!(apply_read_only(0o444, true), 0o444);
        assert_eq!(apply_read_only(0o644, false), 0o644);
        assert_eq!(apply_read_only(0o4755, true), 0o4555);
    }

    #[test]
    fn apply_read_only_agrees_with_is_read_only() {
        for read_only in [true, false] {
            let perm = apply_read_only(0o640, read_only);
            assert_eq!(attr(FileKind::File, Some(perm)).is_read_only(), read_only);
        }
    }

    #[test]
    fn open_modes_report_their_parts() {
        let read = OpenMode::read();
        assert_eq!(read.access(), Access::Read);
        assert_eq!(read.disposition(), Disposition::OpenExisting);

        let create = OpenMode::read_write(Disposition::CreateNew);
        assert_eq!(create.access(), Access::ReadWrite);
        assert_eq!(create.disposition(), Disposition::CreateNew);

        let write = OpenMode::write(Disposition::CreateOrTruncate);
        assert_eq!(write.access(), Access::Write);
        assert_eq!(write.disposition(), Disposition::CreateOrTruncate);
    }

    #[test]
    fn handle_keeps_its_id() {
        assert_eq!(RemoteHandle::new(42).id(), 42);
    }

    #[test]
    fn default_attr_changes_change_nothing() {
        let changes = AttrChanges::default();
        assert_eq!(changes.size, None);
        assert_eq!(changes.read_only, None);
        assert_eq!(changes.mtime, None);
        assert_eq!(changes.atime, None);
    }
}
