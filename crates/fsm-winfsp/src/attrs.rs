// SPDX-License-Identifier: GPL-3.0-or-later

//! Remote attributes as Windows shows them (spec §5.6).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fsm_vfs::{Attr, RemotePath};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
};
use winfsp::filesystem::FileInfo;

/// The allocation unit of the volume: sector size × sectors per unit (spec §5.8).
pub(crate) const ALLOCATION_UNIT: u64 = 4096;

/// 1970-01-01 as a FILETIME, which counts 100 ns ticks from 1601-01-01.
const FILETIME_OF_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
const NANOS_PER_TICK: u128 = 100;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// A FILETIME, or 0, which Windows shows as no time, for a time the server left out.
pub(crate) fn filetime(time: Option<SystemTime>) -> u64 {
    let Some(time) = time else {
        return 0;
    };
    let ticks =
        |since: Duration| u64::try_from(since.as_nanos() / NANOS_PER_TICK).unwrap_or(u64::MAX);
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => FILETIME_OF_UNIX_EPOCH.saturating_add(ticks(after)),
        Err(before) => FILETIME_OF_UNIX_EPOCH.saturating_sub(ticks(before.duration())),
    }
}

/// The time in a FILETIME, or nothing for 0, which means "leave it as it is" when setting times. So do
/// the values with the top bit set, which `SetFileTime` uses for "stop updating this".
pub(crate) fn system_time(filetime: u64) -> Option<SystemTime> {
    if filetime == 0 || filetime > i64::MAX as u64 {
        return None;
    }
    let ticks = |count: u64| {
        Duration::from_secs(count / TICKS_PER_SECOND)
            + Duration::from_nanos((count % TICKS_PER_SECOND) * NANOS_PER_TICK as u64)
    };
    Some(if filetime >= FILETIME_OF_UNIX_EPOCH {
        UNIX_EPOCH + ticks(filetime - FILETIME_OF_UNIX_EPOCH)
    } else {
        UNIX_EPOCH - ticks(FILETIME_OF_UNIX_EPOCH - filetime)
    })
}

/// The Windows attributes. Everything that is not a directory is shown as a file, including a socket,
/// and a broken symbolic link, which has no target to describe.
pub(crate) fn file_attributes(attr: &Attr) -> u32 {
    if attr.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY.0
    } else if attr.is_read_only() {
        FILE_ATTRIBUTE_ARCHIVE.0 | FILE_ATTRIBUTE_READONLY.0
    } else {
        FILE_ATTRIBUTE_ARCHIVE.0
    }
}

/// Everything Windows asks about a file. SFTP v3 has no birth time, so the creation and change times
/// are the modification time, and a server that leaves out the access time gets that too.
pub(crate) fn file_info(attr: &Attr, path: &RemotePath) -> FileInfo {
    let modified = filetime(attr.mtime);
    let size = if attr.is_dir() { 0 } else { attr.size };
    FileInfo {
        file_attributes: file_attributes(attr),
        file_size: size,
        allocation_size: size
            .div_ceil(ALLOCATION_UNIT)
            .saturating_mul(ALLOCATION_UNIT),
        creation_time: modified,
        last_access_time: attr.atime.map_or(modified, |time| filetime(Some(time))),
        last_write_time: modified,
        change_time: modified,
        index_number: index_number(path),
        ..FileInfo::default()
    }
}

/// A file id. Servers report no inode through [`Attr`], and if every file had the same id, programs
/// that compare ids to spot one file under two names would take all files for one. It lasts as long
/// as the path does.
fn index_number(path: &RemotePath) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.as_str().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use fsm_vfs::FileKind;

    use super::*;

    fn attr(kind: FileKind, size: u64, perm: Option<u32>) -> Attr {
        Attr {
            kind,
            size,
            perm,
            mtime: None,
            atime: None,
        }
    }

    fn path(text: &str) -> RemotePath {
        RemotePath::new(text).unwrap()
    }

    #[test]
    fn filetime_counts_from_1601() {
        assert_eq!(filetime(Some(UNIX_EPOCH)), FILETIME_OF_UNIX_EPOCH);
        assert_eq!(
            filetime(Some(UNIX_EPOCH + Duration::from_secs(1))),
            FILETIME_OF_UNIX_EPOCH + 10_000_000
        );
        assert_eq!(
            filetime(Some(UNIX_EPOCH - Duration::from_secs(1))),
            FILETIME_OF_UNIX_EPOCH - 10_000_000
        );
    }

    #[test]
    fn a_time_the_server_left_out_is_no_time() {
        assert_eq!(filetime(None), 0);
    }

    #[test]
    fn a_filetime_turns_back_into_the_same_time() {
        for time in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            UNIX_EPOCH + Duration::from_nanos(1_700_000_000_000_000_100),
            UNIX_EPOCH - Duration::from_secs(86_400),
        ] {
            assert_eq!(system_time(filetime(Some(time))), Some(time));
        }
    }

    #[test]
    fn times_that_mean_no_change_turn_into_nothing() {
        assert_eq!(system_time(0), None);
        assert_eq!(system_time(u64::MAX), None);
        assert_eq!(system_time(u64::MAX - 1), None);
    }

    #[test]
    fn attributes_follow_the_kind_and_the_owner_write_bit() {
        let of = |kind, perm| file_attributes(&attr(kind, 0, perm));
        let (archive, read_only) = (FILE_ATTRIBUTE_ARCHIVE.0, FILE_ATTRIBUTE_READONLY.0);

        assert_eq!(of(FileKind::File, Some(0o644)), archive);
        assert_eq!(of(FileKind::File, Some(0o444)), archive | read_only);
        assert_eq!(of(FileKind::File, None), archive);
        assert_eq!(of(FileKind::Dir, Some(0o555)), FILE_ATTRIBUTE_DIRECTORY.0);
        assert_eq!(of(FileKind::Symlink, Some(0o444)), archive);
        assert_eq!(of(FileKind::Other, Some(0o644)), archive);
    }

    #[test]
    fn allocation_is_whole_units() {
        for (size, allocated) in [(0, 0), (1, 4096), (4096, 4096), (4097, 8192)] {
            let info = file_info(&attr(FileKind::File, size, None), &path("/a"));
            assert_eq!(info.file_size, size);
            assert_eq!(info.allocation_size, allocated, "{size}");
        }
    }

    #[test]
    fn a_directory_has_no_size_whatever_the_server_says() {
        let info = file_info(&attr(FileKind::Dir, 4096, None), &path("/a"));
        assert_eq!((info.file_size, info.allocation_size), (0, 0));
    }

    #[test]
    fn times_without_a_birth_time_come_from_the_modification_time() {
        let modified = UNIX_EPOCH + Duration::from_secs(1000);
        let accessed = UNIX_EPOCH + Duration::from_secs(2000);
        let mut remote = attr(FileKind::File, 0, None);
        remote.mtime = Some(modified);

        let info = file_info(&remote, &path("/a"));
        let expected = filetime(Some(modified));
        assert_eq!(info.last_write_time, expected);
        assert_eq!(info.creation_time, expected);
        assert_eq!(info.change_time, expected);
        assert_eq!(info.last_access_time, expected);

        remote.atime = Some(accessed);
        let info = file_info(&remote, &path("/a"));
        assert_eq!(info.last_access_time, filetime(Some(accessed)));
        assert_eq!(info.last_write_time, expected);
    }

    #[test]
    fn each_path_has_its_own_lasting_id() {
        let of = |text| file_info(&attr(FileKind::File, 0, None), &path(text)).index_number;
        assert_eq!(of("/a"), of("/a"));
        assert_ne!(of("/a"), of("/b"));
    }
}
