// SPDX-License-Identifier: GPL-3.0-or-later

//! The write path of a mounted drive, through the same Win32 calls programs make (M1-12b).

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drivemount_vfs::{Attr, AttrChanges, AttrTarget, FsError, OpenMode, RemoteFs};
use support::{
    Drive, MappedFile, Op, Options, eventually, find, pattern, raw_handle, remote, wide,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR,
    SECURITY_DESCRIPTOR, SetFileSecurityW, SetSecurityDescriptorDacl,
};
use windows::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, CREATE_ALWAYS, CopyFileExW, CreateFileW, DeleteFileW,
    FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_DISPOSITION_INFO, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_MODE, FILE_WRITE_ATTRIBUTES, FileAllocationInfo,
    FileDispositionInfo, MOVE_FILE_FLAGS, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING,
    MoveFileExW, REPLACE_FILE_FLAGS, ReplaceFileW, SetFileAttributesW, SetFileInformationByHandle,
};
use windows::core::PCWSTR;

const ERROR_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_WRITE_PROTECT: i32 = 19;
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_FILE_EXISTS: i32 = 80;
const ERROR_DISK_FULL: i32 = 112;
const ERROR_SEM_TIMEOUT: i32 = 121;
const ERROR_DIR_NOT_EMPTY: i32 = 145;
const ERROR_DIRECTORY: i32 = 267;
const ERROR_ALREADY_EXISTS: i32 = 183;

const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

/// Other handles may do anything meanwhile.
const SHARE_ALL: u32 = 7;
/// `FILE_GENERIC_READ` and `DELETE`.
const READ_AND_DELETE: u32 = 0x0012_0089 | 0x0001_0000;
/// `FILE_GENERIC_READ`, `FILE_GENERIC_WRITE` and `DELETE`.
const READ_WRITE_AND_DELETE: u32 = 0x0012_019F | 0x0001_0000;

fn code<T: std::fmt::Debug>(result: io::Result<T>) -> i32 {
    result.unwrap_err().raw_os_error().unwrap()
}

/// What the Win32 functions here return, as the error `std` would give.
fn win32(result: windows::core::Result<()>) -> io::Result<()> {
    result.map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))
}

fn copy(from: &Path, to: &Path, fail_if_exists: bool) -> io::Result<()> {
    let (from, to) = (wide(from), wide(to));
    let flags = if fail_if_exists {
        COPY_FILE_FAIL_IF_EXISTS
    } else {
        Default::default()
    };
    // SAFETY: both names are NUL-terminated and outlive the call, which is given no callback.
    win32(unsafe {
        CopyFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            None,
            None,
            None,
            flags,
        )
    })
}

fn move_file(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both names are NUL-terminated and outlive the call.
    win32(unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) })
}

fn replace(replaced: &Path, replacement: &Path, backup: Option<&Path>) -> io::Result<()> {
    let (replaced, replacement) = (wide(replaced), wide(replacement));
    let backup = backup.map(wide);
    let backup = backup
        .as_ref()
        .map_or(PCWSTR::null(), |backup| PCWSTR(backup.as_ptr()));
    // SAFETY: the names are NUL-terminated or null, and outlive the call.
    win32(unsafe {
        ReplaceFileW(
            PCWSTR(replaced.as_ptr()),
            PCWSTR(replacement.as_ptr()),
            backup,
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    })
}

/// `CREATE_ALWAYS`. `File::create` is not this: it opens the file and sets its length afterwards, which
/// never reaches `Overwrite`.
fn create_always(path: &Path, attributes: u32) -> io::Result<File> {
    let path = wide(path);
    // SAFETY: the name is NUL-terminated and outlives the call. The handle is new, and only `File`
    // owns it.
    unsafe {
        let handle = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(SHARE_ALL),
            None,
            CREATE_ALWAYS,
            FILE_FLAGS_AND_ATTRIBUTES(attributes),
            None,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))?;
        Ok(File::from_raw_handle(handle.0))
    }
}

fn delete(path: &Path) -> io::Result<()> {
    let path = wide(path);
    // SAFETY: the name is NUL-terminated and outlives the call.
    win32(unsafe { DeleteFileW(PCWSTR(path.as_ptr())) })
}

fn set_attributes(path: &Path, attributes: u32) -> io::Result<()> {
    let path = wide(path);
    // SAFETY: the name is NUL-terminated and outlives the call.
    win32(unsafe {
        SetFileAttributesW(PCWSTR(path.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(attributes))
    })
}

/// Gives a file a descriptor that lets everyone do anything.
fn set_open_security(path: &Path) -> io::Result<()> {
    let path = wide(path);
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor = PSECURITY_DESCRIPTOR((&raw mut descriptor).cast());
    // SAFETY: the descriptor is big enough, lives until after the last call, and has no pointers in
    // it. The name is NUL-terminated and outlives the call.
    win32(unsafe {
        InitializeSecurityDescriptor(descriptor, 1)?;
        SetSecurityDescriptorDacl(descriptor, true, None, false)?;
        SetFileSecurityW(PCWSTR(path.as_ptr()), DACL_SECURITY_INFORMATION, descriptor).ok()
    })
}

fn set_delete_pending(file: &File, pending: bool) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: pending,
    };
    // SAFETY: the handle is open, and the size is that of what is passed, which outlives the call.
    win32(unsafe {
        SetFileInformationByHandle(
            raw_handle(file),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    })
}

fn set_allocation(file: &File, size: i64) -> io::Result<()> {
    let info = FILE_ALLOCATION_INFO {
        AllocationSize: size,
    };
    // SAFETY: the handle is open, and the size is that of what is passed, which outlives the call.
    win32(unsafe {
        SetFileInformationByHandle(
            raw_handle(file),
            FileAllocationInfo,
            (&raw const info).cast(),
            size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    })
}

fn sorted_names(drive: &Drive, directory: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(drive.path(directory))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn read_write(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

/// A folder for the far end of copies, on the disk tests run from.
struct LocalDir(PathBuf);

impl LocalDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("drivemount-{}-{name}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for LocalDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A drive that gives up quickly, on a server that answers after longer than that.
fn short_drive() -> Drive {
    Drive::with_options(Options {
        op_timeout: Duration::from_millis(200),
        ..Options::default()
    })
}

fn assert_times_out<T: std::fmt::Debug>(drive: &Drive, call: impl FnOnce() -> io::Result<T>) {
    let before = *drive.backend.suspect().borrow();
    let start = Instant::now();
    assert_eq!(code(call()), ERROR_SEM_TIMEOUT);
    assert!(start.elapsed() < Duration::from_secs(5), "took too long");
    assert!(*drive.backend.suspect().borrow() > before);
}

const HANG: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- creating and writing

/// NFR-REL-01, below the 1 GiB that `a_gibibyte_round_trips` covers.
#[test]
fn what_is_written_reads_back_the_same_at_every_size() {
    let drive = Drive::new();
    for (index, size) in [0, 1, 4095, 4096, 4097, (1 << 20) + 1, 16 << 20]
        .into_iter()
        .enumerate()
    {
        let data = pattern(size, index as u8);
        let name = format!("file{index}");
        fs::write(drive.path(&name), &data).unwrap();
        assert_eq!(fs::read(drive.path(&name)).unwrap(), data, "{size}");
        assert!(drive.get(&format!("/{name}")) == data, "{size}");
        assert_eq!(fs::metadata(drive.path(&name)).unwrap().len(), size as u64);
    }
}

#[test]
#[ignore = "takes a while, and over a GiB of memory"]
fn a_gibibyte_round_trips() {
    let drive = Drive::new();
    let block = pattern(1 << 20, 7);
    let mut file = File::create(drive.path("big")).unwrap();
    for _ in 0..1024 {
        file.write_all(&block).unwrap();
    }
    drop(file);

    let mut file = File::open(drive.path("big")).unwrap();
    let mut read = vec![0; 1 << 20];
    for index in 0..1024 {
        file.read_exact(&mut read).unwrap();
        assert!(read == block, "block {index}");
    }
    assert_eq!(file.read(&mut read).unwrap(), 0);
}

/// `MemFs` refuses a write over its limit, which is well below this.
#[test]
fn one_write_above_the_servers_limit_is_split() {
    let drive = Drive::new();
    let data = pattern(3 << 20, 1);
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(&data).unwrap();
    drop(file);
    assert!(drive.get("/file") == data);
    assert!(drive.probe.count(Op::Write) >= 12);
}

#[test]
fn writes_can_land_anywhere_and_a_gap_reads_as_zeros() {
    let drive = Drive::new();
    let mut file = read_write(&{
        File::create(drive.path("file")).unwrap();
        drive.path("file")
    });
    file.seek(SeekFrom::Start(10)).unwrap();
    file.write_all(b"end").unwrap();
    file.seek(SeekFrom::Start(2)).unwrap();
    file.write_all(b"mid").unwrap();
    assert_eq!(file.metadata().unwrap().len(), 13);
    drop(file);
    assert_eq!(drive.get("/file"), b"\0\0mid\0\0\0\0\0end");
}

#[test]
fn appending_goes_to_the_end_of_what_is_there() {
    let drive = Drive::new();
    drive.put("/log", b"one");
    let mut file = OpenOptions::new()
        .append(true)
        .open(drive.path("log"))
        .unwrap();
    file.write_all(b"two").unwrap();
    // Another client adds to it. An append goes after that too.
    drive.server(async {
        let fs = &drive.probe.inner;
        let mode = OpenMode::write(drivemount_vfs::Disposition::OpenExisting);
        let handle = fs.open(&remote("/log"), mode).await.unwrap();
        fs.write(&handle, 6, drivemount_vfs::Bytes::from_static(b"other"))
            .await
            .unwrap();
        fs.close(handle).await.unwrap();
    });
    file.write_all(b"three").unwrap();
    drop(file);
    assert_eq!(drive.get("/log"), b"onetwootherthree");
}

/// Paging I/O, which does not extend a file, is the one way writes arrive with `ConstrainedIo`.
#[test]
fn a_mapped_view_writes_through_to_the_file() {
    let drive = Drive::new();
    fs::write(drive.path("file"), pattern(100_000, 3)).unwrap();
    let file = read_write(&drive.path("file"));
    let mut view = MappedFile::new(&file, 100_000);
    view.bytes()[70_000..70_005].copy_from_slice(b"hello");
    assert_eq!(&view.bytes()[70_000..70_005], b"hello");
    view.flush();
    drop(view);
    drop(file);

    let mut expected = pattern(100_000, 3);
    expected[70_000..70_005].copy_from_slice(b"hello");
    assert!(eventually(|| drive.get("/file") == expected));
    assert_eq!(fs::metadata(drive.path("file")).unwrap().len(), 100_000);
}

#[test]
fn a_new_file_can_be_read_through_the_handle_that_created_it() {
    let drive = Drive::new();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(drive.path("file"))
        .unwrap();
    file.write_all(b"contents").unwrap();
    file.seek(SeekFrom::Start(3)).unwrap();
    let mut read = String::new();
    file.read_to_string(&mut read).unwrap();
    assert_eq!(read, "tents");
}

#[test]
fn a_handle_that_writes_and_reads_sees_its_own_writes() {
    let drive = Drive::new();
    drive.put("/file", &pattern(1 << 20, 0));
    let mut file = read_write(&drive.path("file"));

    // Enough sequential reads to have fetched well ahead.
    let mut read = vec![0; 64 * 1024];
    for _ in 0..4 {
        file.read_exact(&mut read).unwrap();
    }
    let position = file.stream_position().unwrap();
    file.write_all(&[0xAB; 4096]).unwrap();
    file.seek(SeekFrom::Start(position)).unwrap();
    file.read_exact(&mut read[..4096]).unwrap();
    assert_eq!(read[..4096], [0xAB; 4096]);
}

/// M1-06's warning: what a reader fetched ahead must not outlive a change through another handle.
#[test]
fn a_write_through_one_handle_reaches_a_reader_on_another() {
    let drive = Drive::new();
    drive.put("/file", &pattern(1 << 20, 0));
    let mut reader = File::open(drive.path("file")).unwrap();
    let mut read = vec![0; 64 * 1024];
    for _ in 0..4 {
        reader.read_exact(&mut read).unwrap();
    }
    let position = reader.stream_position().unwrap();

    let mut writer = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    writer.seek(SeekFrom::Start(position)).unwrap();
    writer.write_all(&[0xCD; 4096]).unwrap();
    drop(writer);

    reader.read_exact(&mut read[..4096]).unwrap();
    assert_eq!(read[..4096], [0xCD; 4096]);
}

#[test]
fn cutting_a_file_short_reaches_a_reader_on_another_handle() {
    let drive = Drive::new();
    drive.put("/file", &pattern(1 << 20, 0));
    let mut reader = File::open(drive.path("file")).unwrap();
    let mut read = vec![0; 64 * 1024];
    for _ in 0..4 {
        reader.read_exact(&mut read).unwrap();
    }
    let writer = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    writer.set_len(4 * 64 * 1024).unwrap();
    drop(writer);
    assert_eq!(reader.read(&mut read).unwrap(), 0);
}

#[test]
fn a_remote_handle_is_opened_on_the_first_write_and_closed_with_the_last_handle() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    let (opens, closes) = (drive.probe.count(Op::Open), drive.probe.count(Op::Close));

    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    file.metadata().unwrap();
    assert_eq!(drive.probe.count(Op::Open), opens);
    file.write_all(b"after!").unwrap();
    file.write_all(b"more").unwrap();
    assert_eq!(drive.probe.count(Op::Open), opens + 1);
    drop(file);
    assert!(eventually(|| drive.probe.count(Op::Close) == closes + 1));
}

#[test]
fn a_handle_that_reads_and_writes_closes_both_remote_handles() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    let (opens, closes) = (drive.probe.count(Op::Open), drive.probe.count(Op::Close));

    let mut file = read_write(&drive.path("file"));
    file.read_exact(&mut [0; 2]).unwrap();
    file.write_all(b"xx").unwrap();
    assert_eq!(drive.probe.count(Op::Open), opens + 2);
    drop(file);
    assert!(eventually(|| drive.probe.count(Op::Close) == closes + 2));
}

/// A file the server lets this user only read still works through a handle that could also write.
#[test]
fn a_handle_for_both_only_opens_what_it_uses() {
    let drive = Drive::new();
    drive.put("/file", b"contents");
    drive.probe.fail(Op::Write, FsError::PermissionDenied);
    let mut file = read_write(&drive.path("file"));
    let mut read = String::new();
    file.read_to_string(&mut read).unwrap();
    assert_eq!(read, "contents");
    // The write is refused after its call returned, and the next call hears of it (spec §5.6).
    file.write_all(b"x").unwrap();
    assert_eq!(code(file.sync_all()), ERROR_ACCESS_DENIED);
}

#[test]
fn creating_what_exists_is_refused() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    drive.mkdir("/dir");
    let create_new = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(drive.path(name))
    };
    assert_eq!(code(create_new("file")), ERROR_FILE_EXISTS);
    assert_eq!(code(create_new("FILE")), ERROR_FILE_EXISTS);
    assert_eq!(
        code(fs::create_dir(drive.path("dir"))),
        ERROR_ALREADY_EXISTS
    );
    assert_eq!(
        code(fs::create_dir(drive.path("DIR"))),
        ERROR_ALREADY_EXISTS
    );
    assert_eq!(
        code(fs::create_dir(drive.path("file"))),
        ERROR_ALREADY_EXISTS
    );
    assert_eq!(drive.get("/file"), b"before");
    assert_eq!(sorted_names(&drive, ""), ["dir", "file"]);
}

#[test]
fn a_new_name_needs_a_parent_that_is_a_directory() {
    let drive = Drive::new();
    drive.put("/file", b"");
    assert_eq!(
        code(File::create(drive.path(r"missing\new"))),
        ERROR_PATH_NOT_FOUND
    );
    assert_eq!(
        code(fs::create_dir(drive.path(r"missing\new"))),
        ERROR_PATH_NOT_FOUND
    );
    assert_eq!(
        code(File::create(drive.path(r"file\new"))),
        ERROR_PATH_NOT_FOUND
    );
}

#[test]
fn a_new_name_goes_into_the_parent_as_it_is_on_the_server() {
    let drive = Drive::new();
    drive.mkdir("/Dir");
    fs::write(drive.path(r"DIR\New.txt"), b"x").unwrap();
    fs::create_dir(drive.path(r"dir\Sub")).unwrap();
    assert_eq!(drive.get("/Dir/New.txt"), b"x");
    assert!(drive.attr("/Dir/Sub").unwrap().is_dir());
    assert_eq!(sorted_names(&drive, "Dir"), ["New.txt", "Sub"]);
}

#[test]
fn opening_in_the_wrong_case_writes_the_file_that_is_there() {
    let drive = Drive::new();
    drive.put("/Notes.txt", b"before");
    fs::write(drive.path("NOTES.TXT"), b"after").unwrap();
    assert_eq!(drive.get("/Notes.txt"), b"after");
    assert_eq!(sorted_names(&drive, ""), ["Notes.txt"]);
}

#[test]
fn a_name_windows_forbids_is_created_with_its_real_form() {
    let drive = Drive::new();
    fs::write(drive.path("a\u{F03A}b\u{F02E}"), b"x").unwrap();
    assert_eq!(drive.get("/a:b."), b"x");
}

#[test]
fn a_new_file_can_start_out_read_only() {
    let drive = Drive::new();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .attributes(FILE_ATTRIBUTE_READONLY.0)
        .open(drive.path("file"))
        .unwrap();
    file.write_all(b"contents").unwrap();
    drop(file);
    assert!(drive.attr("/file").unwrap().is_read_only());
    assert_eq!(drive.get("/file"), b"contents");
}

#[test]
fn creating_a_directory_makes_a_directory_that_takes_files() {
    let drive = Drive::new();
    fs::create_dir_all(drive.path(r"a\b\c")).unwrap();
    fs::write(drive.path(r"a\b\c\file"), b"x").unwrap();
    assert!(fs::metadata(drive.path(r"a\b")).unwrap().is_dir());
    assert_eq!(drive.get("/a/b/c/file"), b"x");
}

// ---------------------------------------------------------------- replacing contents

#[test]
fn creating_over_a_file_empties_it() {
    let drive = Drive::new();
    drive.put("/file", b"long contents");
    let mut file = create_always(&drive.path("file"), 0).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    file.write_all(b"new").unwrap();
    drop(file);
    assert_eq!(drive.get("/file"), b"new");
}

/// What `fs::write` does to a file that exists.
#[test]
fn opening_a_file_and_setting_its_length_replaces_its_contents_too() {
    let drive = Drive::new();
    drive.put("/file", b"long contents");
    fs::write(drive.path("file"), b"new").unwrap();
    assert_eq!(drive.get("/file"), b"new");
}

#[test]
fn truncating_on_open_empties_a_file() {
    let drive = Drive::new();
    drive.put("/file", b"long contents");
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(drive.path("file"))
        .unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    drop(file);
    assert_eq!(drive.attr("/file").unwrap().size, 0);
}

/// `Overwrite` reaches a reader on another handle, like any other change.
#[test]
fn emptying_a_file_reaches_a_reader_on_another_handle() {
    let drive = Drive::new();
    drive.put("/file", &pattern(1 << 20, 0));
    let mut reader = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(drive.path("file"))
        .unwrap();
    let mut read = vec![0; 64 * 1024];
    for _ in 0..4 {
        reader.read_exact(&mut read).unwrap();
    }
    drop(create_always(&drive.path("file"), 0).unwrap());
    // No seek: a jump would drop what was fetched ahead anyway.
    assert_eq!(reader.read(&mut read).unwrap(), 0);
}

#[test]
fn creating_over_a_file_can_make_it_read_only() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    drop(create_always(&drive.path("file"), FILE_ATTRIBUTE_READONLY.0).unwrap());
    let attr = drive.attr("/file").unwrap();
    assert!(attr.is_read_only());
    assert_eq!(attr.size, 0);
}

#[test]
fn creating_over_a_read_only_file_is_refused() {
    let drive = Drive::new();
    drive.put_read_only("/file", b"before");
    assert_eq!(
        code(create_always(&drive.path("file"), 0)),
        ERROR_ACCESS_DENIED
    );
    assert_eq!(drive.get("/file"), b"before");
}

#[test]
fn a_size_can_be_set_up_or_down() {
    let drive = Drive::new();
    drive.put("/file", b"0123456789");
    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    file.set_len(4).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 4);
    assert_eq!(drive.get("/file"), b"0123");
    file.set_len(6).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 6);
    assert_eq!(drive.get("/file"), b"0123\0\0");
}

/// The driver rounds up to the allocation unit before this reaches us.
/// The server may have moved the file meanwhile, which only the handle still knows.
#[test]
fn a_size_change_goes_through_the_handle_if_there_is_one() {
    let drive = Drive::new();
    drive.put("/file", b"0123456789");
    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    file.write_all(b"x").unwrap();
    drive
        .server(
            drive
                .probe
                .inner
                .rename(&remote("/file"), &remote("/moved"), false),
        )
        .unwrap();

    file.set_len(4).unwrap();
    assert_eq!(drive.get("/moved"), b"x123");
}

/// A handle that writes answers from the file, not from what a path meant up to a TTL ago.
#[test]
fn a_written_file_reports_what_the_server_says_now() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    file.write_all(b"y").unwrap();
    drive
        .server(
            drive
                .probe
                .inner
                .rename(&remote("/file"), &remote("/moved"), false),
        )
        .unwrap();
    drive.put("/moved", b"much longer now");
    assert!(eventually(|| file.metadata().unwrap().len() == 15));
}

#[test]
fn allocation_only_ever_shortens_a_file() {
    let drive = Drive::new();
    let data = pattern(10_000, 1);
    drive.put("/file", &data);
    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();

    set_allocation(&file, 1 << 20).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10_000);
    assert!(drive.get("/file") == data);

    set_allocation(&file, 4096).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 4096);
    assert!(drive.get("/file") == data[..4096]);
}

// ---------------------------------------------------------------- write-behind (spec §5.6, ADR-0002)

/// Long enough to tell waiting from not, well under any limit.
const SLOW_WRITE: Duration = Duration::from_millis(300);

#[test]
fn a_write_returns_before_the_server_has_it() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.delay(Op::Write, SLOW_WRITE);
    let start = Instant::now();
    file.write_all(b"contents").unwrap();
    assert!(start.elapsed() < SLOW_WRITE, "{:?}", start.elapsed());
    // Windows is told the size all the same.
    assert_eq!(file.metadata().unwrap().len(), 8);
    drop(file);
}

#[test]
fn closing_a_file_waits_for_its_writes() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.delay(Op::Write, SLOW_WRITE);
    file.write_all(b"contents").unwrap();
    drop(file);
    // Whoever looks next sees them, without waiting.
    assert_eq!(drive.get("/file"), b"contents");
    assert_eq!(fs::read(drive.path("file")).unwrap(), b"contents");
}

#[test]
fn another_handle_reads_what_was_written_before() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    let mut reader = File::open(drive.path("file")).unwrap();
    let mut writer = OpenOptions::new()
        .write(true)
        .share_mode(SHARE_ALL)
        .open(drive.path("file"))
        .unwrap();
    drive.probe.delay(Op::Write, SLOW_WRITE);
    writer.write_all(b"AFTER!").unwrap();
    let mut read = Vec::new();
    reader.read_to_end(&mut read).unwrap();
    assert_eq!(read, b"AFTER!");
    // And the handle that wrote, too.
    let mut both = read_write(&drive.path("file"));
    both.write_all(b"again").unwrap();
    both.seek(SeekFrom::Start(0)).unwrap();
    let mut read = Vec::new();
    both.read_to_end(&mut read).unwrap();
    assert_eq!(read, b"again!");
}

#[test]
fn overlapping_writes_arrive_in_order() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.delay(Op::Write, SLOW_WRITE);
    file.write_all(b"aaaa").unwrap();
    drive.probe.delay(Op::Write, Duration::ZERO);
    file.seek(SeekFrom::Start(1)).unwrap();
    file.write_all(b"bb").unwrap();
    drop(file);
    assert_eq!(drive.get("/file"), b"abba");
}

/// Windows waits for a mapped view's paging writes, which can come after `Cleanup`, so these do not
/// go behind (fsx found this).
#[test]
fn a_flushed_mapped_view_is_on_the_server() {
    let drive = Drive::new();
    fs::write(drive.path("file"), pattern(100_000, 3)).unwrap();
    let file = read_write(&drive.path("file"));
    let mut view = MappedFile::new(&file, 100_000);
    drive.probe.delay(Op::Write, SLOW_WRITE);
    view.bytes()[70_000..70_005].copy_from_slice(b"hello");
    view.flush();
    assert_eq!(&drive.get("/file")[70_000..70_005], b"hello");
    drop(view);
    drop(file);
}

#[test]
fn a_size_set_after_writes_is_not_undone_by_them() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.delay(Op::Write, SLOW_WRITE);
    file.write_all(&[7; 100]).unwrap();
    file.set_len(10).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10);
    drop(file);
    assert_eq!(drive.get("/file"), [7; 10]);
}

#[test]
fn a_failed_write_is_heard_once() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.fail(Op::Write, FsError::NoSpace);
    file.write_all(b"lost").unwrap();
    assert_eq!(code(file.sync_all()), ERROR_DISK_FULL);
    // Heard, so not again: a program that starts over can succeed.
    drive.probe.heal(Op::Write);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"kept").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_eq!(drive.get("/file"), b"kept");
}

/// What a program that never asks again cannot hear goes to the log, and the drive goes on.
#[test]
fn a_failed_write_nobody_heard_of_does_not_stop_the_close() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    drive.probe.fail(Op::Write, FsError::NoSpace);
    file.write_all(b"lost").unwrap();
    drop(file);
    assert!(eventually(|| drive.probe.open_handles() == 0));
    assert_eq!(drive.get("/file"), b"");
}

// ---------------------------------------------------------------- flushing

#[test]
fn flushing_a_file_reaches_the_server() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"x").unwrap();
    let flushes = drive.probe.count(Op::Flush);
    file.sync_all().unwrap();
    assert_eq!(drive.probe.count(Op::Flush), flushes + 1);
}

#[test]
fn closing_a_file_does_not_flush_it() {
    // As on Windows, only `FlushFileBuffers` asks for the disk; a close promises no more than the
    // acknowledged writes (spec §5.6).
    let drive = Drive::new();
    drive.put("/read", b"x");
    let flushes = drive.probe.count(Op::Flush);
    fs::read(drive.path("read")).unwrap();
    fs::write(drive.path("written"), b"x").unwrap();
    assert!(eventually(|| drive.probe.open_handles() == 0));
    assert_eq!(drive.probe.count(Op::Flush), flushes);
    assert_eq!(drive.get("/written"), b"x");
}

#[test]
fn closing_a_new_file_that_was_never_written_does_not_flush_it() {
    let drive = Drive::new();
    let flushes = drive.probe.count(Op::Flush);
    drop(File::create(drive.path("empty")).unwrap());
    assert!(eventually(|| drive.probe.count(Op::Close) >= 1));
    assert_eq!(drive.probe.count(Op::Flush), flushes);
    assert_eq!(drive.get("/empty"), b"");
}

#[test]
fn a_failed_flush_is_reported_to_whoever_asked_for_it() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"x").unwrap();
    drive.probe.fail(Op::Flush, FsError::NoSpace);
    assert_eq!(code(file.sync_all()), ERROR_DISK_FULL);
}

// ---------------------------------------------------------------- attributes and times

#[test]
fn the_read_only_attribute_is_the_owner_write_bit() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let perm = |drive: &Drive| drive.attr("/file").unwrap().perm;
    assert_eq!(perm(&drive), Some(0o644));

    set_attributes(&drive.path("file"), FILE_ATTRIBUTE_READONLY.0).unwrap();
    assert_eq!(perm(&drive), Some(0o444));
    let metadata = fs::metadata(drive.path("file")).unwrap();
    assert!(metadata.permissions().readonly());

    set_attributes(&drive.path("file"), FILE_ATTRIBUTE_NORMAL.0).unwrap();
    assert_eq!(perm(&drive), Some(0o644));
    let metadata = fs::metadata(drive.path("file")).unwrap();
    assert!(!metadata.permissions().readonly());
}

#[test]
fn attributes_with_no_remote_form_change_nothing_and_do_not_fail() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    set_attributes(&drive.path("file"), FILE_ATTRIBUTE_HIDDEN.0).unwrap();
    assert_eq!(drive.attr("/file").unwrap().perm, Some(0o644));
    let shown = fs::metadata(drive.path("file")).unwrap().file_attributes();
    assert_eq!(shown & FILE_ATTRIBUTE_HIDDEN.0, 0);
}

#[test]
fn a_directory_keeps_its_permissions_whatever_it_is_told() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    set_attributes(&drive.path("dir"), FILE_ATTRIBUTE_READONLY.0).unwrap();
    assert_eq!(drive.attr("/dir").unwrap().perm, Some(0o755));
    fs::write(drive.path(r"dir\file"), b"x").unwrap();
}

/// Setting a time arrives with no attributes given, which must not read as "not read-only".
#[test]
fn setting_a_time_leaves_a_read_only_file_read_only() {
    let drive = Drive::new();
    drive.put_read_only("/file", b"x");
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let file = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES.0)
        .open(drive.path("file"))
        .unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    let attr = drive.attr("/file").unwrap();
    assert_eq!(attr.mtime, Some(modified));
    assert!(attr.is_read_only());
}

#[test]
fn times_can_be_set_without_opening_a_remote_handle() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let accessed = UNIX_EPOCH + Duration::from_secs(1_100_000_000);
    let opens = drive.probe.count(Op::Open);

    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    let times = fs::FileTimes::new()
        .set_modified(modified)
        .set_accessed(accessed);
    file.set_times(times).unwrap();
    assert_eq!(file.metadata().unwrap().modified().unwrap(), modified);
    drop(file);

    assert_eq!(drive.probe.count(Op::Open), opens);
    let attr = drive.attr("/file").unwrap();
    assert_eq!(attr.mtime, Some(modified));
    assert_eq!(attr.atime, Some(accessed));
}

#[test]
fn one_time_can_be_set_and_the_other_stays() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let before = drive.attr("/file").unwrap();
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    let after = drive.attr("/file").unwrap();
    assert_eq!(after.mtime, Some(modified));
    assert!(support::same_time(
        after.atime.unwrap(),
        before.atime.unwrap()
    ));
    assert!(!after.is_read_only());
}

/// SFTP has nowhere to put these, so there is nothing to ask of the server.
#[test]
fn a_creation_time_alone_is_not_sent_to_the_server() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let before = drive.attr("/file").unwrap();
    let calls = drive.probe.count(Op::SetAttr);
    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    let created = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    file.set_times(fs::FileTimes::new().set_created(created))
        .unwrap();
    drop(file);
    assert_eq!(drive.probe.count(Op::SetAttr), calls);
    assert_eq!(drive.attr("/file"), Some(before));
}

/// `CopyFile` sets the time last, and closing must not lose it.
#[test]
fn a_time_set_after_writing_stays_after_closing() {
    let drive = Drive::new();
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"x").unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    assert_eq!(drive.attr("/file").unwrap().mtime, Some(modified));
    assert_eq!(
        fs::metadata(drive.path("file"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
}

#[test]
fn a_write_moves_the_modification_time() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let old = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let changes = AttrChanges {
        mtime: Some(old),
        ..AttrChanges::default()
    };
    drive
        .server(
            drive
                .probe
                .inner
                .set_attr(AttrTarget::Path(&remote("/file")), changes),
        )
        .unwrap();
    fs::write(drive.path("file"), b"y").unwrap();
    assert_ne!(drive.attr("/file").unwrap().mtime, Some(old));
}

// ---------------------------------------------------------------- deleting

#[test]
fn a_file_can_be_deleted() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    delete(&drive.path("file")).unwrap();
    assert!(drive.attr("/file").is_none());
    assert_eq!(code(fs::metadata(drive.path("file"))), ERROR_NOT_FOUND);
}

#[test]
fn deleting_in_the_wrong_case_deletes_the_file_that_is_there() {
    let drive = Drive::new();
    drive.put("/File", b"x");
    delete(&drive.path("FILE")).unwrap();
    assert!(drive.attr("/File").is_none());
}

#[test]
fn deleting_what_is_not_there_says_so() {
    let drive = Drive::new();
    assert_eq!(code(delete(&drive.path("missing"))), ERROR_NOT_FOUND);
}

#[test]
fn a_read_only_file_is_not_deleted_until_the_attribute_goes() {
    let drive = Drive::new();
    drive.put_read_only("/file", b"x");
    assert_eq!(code(delete(&drive.path("file"))), ERROR_ACCESS_DENIED);
    assert!(drive.attr("/file").is_some());

    set_attributes(&drive.path("file"), FILE_ATTRIBUTE_NORMAL.0).unwrap();
    delete(&drive.path("file")).unwrap();
    assert!(drive.attr("/file").is_none());
}

#[test]
fn an_empty_directory_can_be_removed_and_a_full_one_cannot() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/dir/file", b"x");

    assert_eq!(code(fs::remove_dir(drive.path("dir"))), ERROR_DIR_NOT_EMPTY);
    assert!(drive.attr("/dir/file").is_some());

    delete(&drive.path(r"dir\file")).unwrap();
    fs::remove_dir(drive.path("dir")).unwrap();
    assert!(drive.attr("/dir").is_none());
}

#[test]
fn a_whole_tree_can_be_removed() {
    let drive = Drive::new();
    fs::create_dir_all(drive.path(r"top\sub\deep")).unwrap();
    fs::write(drive.path(r"top\a"), b"x").unwrap();
    fs::write(drive.path(r"top\sub\b"), b"x").unwrap();
    fs::write(drive.path(r"top\sub\deep\c"), b"x").unwrap();
    fs::remove_dir_all(drive.path("top")).unwrap();
    assert!(drive.attr("/top").is_none());
}

#[test]
fn delete_on_close_removes_the_file_when_the_handle_closes() {
    let drive = Drive::new();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
        .share_mode(SHARE_ALL)
        .open(drive.path("temp"))
        .unwrap();
    file.write_all(b"scratch").unwrap();
    assert!(eventually(|| drive.get("/temp") == b"scratch"));
    drop(file);
    assert!(eventually(|| drive.attr("/temp").is_none()));
    assert!(sorted_names(&drive, "").is_empty());
}

#[test]
fn a_delete_waits_for_the_last_handle() {
    let drive = Drive::new();
    drive.put("/file", b"contents");
    let share_all = || {
        OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .access_mode(READ_AND_DELETE)
            .open(drive.path("file"))
    };
    let mut other = share_all().unwrap();
    let doomed = share_all().unwrap();
    set_delete_pending(&doomed, true).unwrap();
    drop(doomed);

    // Still there, and readable through what is open.
    assert!(drive.attr("/file").is_some());
    let mut read = String::new();
    other.read_to_string(&mut read).unwrap();
    assert_eq!(read, "contents");

    drop(other);
    assert!(eventually(|| drive.attr("/file").is_none()));
}

#[test]
fn a_delete_can_be_taken_back() {
    let drive = Drive::new();
    drive.put("/file", b"contents");
    let file = OpenOptions::new()
        .read(true)
        .access_mode(READ_AND_DELETE)
        .open(drive.path("file"))
        .unwrap();
    set_delete_pending(&file, true).unwrap();
    set_delete_pending(&file, false).unwrap();
    drop(file);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(drive.get("/file"), b"contents");
}

#[test]
fn deleting_a_file_closes_its_remote_handles_first() {
    let drive = Drive::new();
    let closes = drive.probe.count(Op::Close);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
        .share_mode(SHARE_ALL)
        .open(drive.path("temp"))
        .unwrap();
    file.write_all(b"x").unwrap();
    // `Close` cannot be told apart from a later one by count alone, so make the removal refuse
    // while a handle is open: this layer records the order.
    drive.probe.require_closed_before_remove();
    drop(file);
    assert!(eventually(|| drive.attr("/temp").is_none()));
    assert_eq!(drive.probe.count(Op::Close), closes + 1);
    assert!(!drive.probe.removed_while_open());
}

/// A link is shown as its target, but what goes is the link, and nothing behind it.
#[test]
fn removing_a_link_to_a_full_directory_leaves_the_directory() {
    let drive = Drive::new();
    drive.mkdir("/target");
    drive.put("/target/file", b"x");
    drive.probe.add_link("/link", "/target");

    fs::remove_dir(drive.path("link")).unwrap();
    assert!(eventually(
        || !sorted_names(&drive, "").contains(&"link".to_owned())
    ));
    assert_eq!(drive.get("/target/file"), b"x");
}

#[test]
fn removing_a_link_to_a_file_leaves_the_file() {
    let drive = Drive::new();
    drive.put("/target", b"x");
    drive.probe.add_link("/link", "/target");
    delete(&drive.path("link")).unwrap();
    assert!(eventually(
        || !sorted_names(&drive, "").contains(&"link".to_owned())
    ));
    assert_eq!(drive.get("/target"), b"x");
}

// ---------------------------------------------------------------- renaming

#[test]
fn a_file_can_be_renamed_and_moved() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/a", b"contents");

    move_file(&drive.path("a"), &drive.path("b"), MOVE_FILE_FLAGS(0)).unwrap();
    assert!(drive.attr("/a").is_none());
    assert_eq!(drive.get("/b"), b"contents");

    move_file(&drive.path("b"), &drive.path(r"DIR\c"), MOVE_FILE_FLAGS(0)).unwrap();
    assert_eq!(drive.get("/dir/c"), b"contents");
    assert_eq!(sorted_names(&drive, ""), ["dir"]);
}

#[test]
fn a_directory_can_be_renamed_with_everything_in_it() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.mkdir("/dir/sub");
    drive.put("/dir/sub/file", b"x");

    // Looked at first, so that the old names are in the caches.
    assert_eq!(sorted_names(&drive, r"dir\sub"), ["file"]);
    move_file(&drive.path("dir"), &drive.path("new"), MOVE_FILE_FLAGS(0)).unwrap();

    assert_eq!(fs::read(drive.path(r"new\sub\file")).unwrap(), b"x");
    assert_eq!(
        code(fs::metadata(drive.path(r"dir\sub\file"))),
        ERROR_PATH_NOT_FOUND
    );
    assert_eq!(sorted_names(&drive, ""), ["new"]);
}

#[test]
fn a_taken_name_is_only_taken_over_when_asked() {
    let drive = Drive::new();
    drive.put("/a", b"new");
    drive.put("/b", b"old");

    drive.put_read_only("/readonly", b"old");
    drive.mkdir("/dir");
    let (a, b) = (drive.path("a"), drive.path("b"));
    for taken in ["b", "readonly", "dir"] {
        assert_eq!(
            code(move_file(&a, &drive.path(taken), MOVE_FILE_FLAGS(0))),
            ERROR_ALREADY_EXISTS,
            "{taken}"
        );
    }
    assert_eq!(drive.get("/a"), b"new");
    assert_eq!(drive.get("/b"), b"old");

    move_file(&a, &b, MOVEFILE_REPLACE_EXISTING).unwrap();
    assert_eq!(drive.get("/b"), b"new");
    assert_eq!(sorted_names(&drive, ""), ["b", "dir", "readonly"]);
}

#[test]
fn a_taken_name_in_another_case_counts_as_taken() {
    let drive = Drive::new();
    drive.put("/a", b"new");
    drive.put("/Taken", b"old");

    let (a, taken) = (drive.path("a"), drive.path("TAKEN"));
    assert_eq!(
        code(move_file(&a, &taken, MOVE_FILE_FLAGS(0))),
        ERROR_ALREADY_EXISTS
    );
    assert_eq!(drive.get("/Taken"), b"old");

    // Replacing does not leave two files that Windows would take for one.
    move_file(&a, &taken, MOVEFILE_REPLACE_EXISTING).unwrap();
    assert_eq!(sorted_names(&drive, ""), ["Taken"]);
    assert_eq!(drive.get("/Taken"), b"new");
}

#[test]
fn a_rename_can_change_only_the_case() {
    let drive = Drive::new();
    drive.put("/name", b"x");
    move_file(&drive.path("name"), &drive.path("NAME"), MOVE_FILE_FLAGS(0)).unwrap();
    assert_eq!(sorted_names(&drive, ""), ["NAME"]);
    assert_eq!(drive.get("/NAME"), b"x");
}

#[test]
fn a_rename_onto_itself_does_nothing() {
    let drive = Drive::new();
    drive.put("/name", b"x");
    move_file(&drive.path("name"), &drive.path("name"), MOVE_FILE_FLAGS(0)).unwrap();
    move_file(
        &drive.path("name"),
        &drive.path("name"),
        MOVEFILE_REPLACE_EXISTING,
    )
    .unwrap();
    assert_eq!(drive.get("/name"), b"x");
}

#[test]
fn windows_refuses_what_a_server_would_replace() {
    let drive = Drive::new();
    drive.put("/file", b"new");
    drive.mkdir("/emptydir");
    drive.mkdir("/dir");
    drive.put_read_only("/readonly", b"old");
    drive.put("/other", b"old");

    let denied = |from: &str, to: &str| {
        code(move_file(
            &drive.path(from),
            &drive.path(to),
            MOVEFILE_REPLACE_EXISTING,
        ))
    };
    assert_eq!(denied("file", "emptydir"), ERROR_ACCESS_DENIED);
    assert_eq!(denied("file", "readonly"), ERROR_ACCESS_DENIED);
    assert_eq!(denied("dir", "other"), ERROR_ACCESS_DENIED);
    assert_eq!(denied("dir", "emptydir"), ERROR_ACCESS_DENIED);

    assert_eq!(drive.get("/readonly"), b"old");
    assert_eq!(drive.get("/file"), b"new");
    assert!(drive.attr("/emptydir").unwrap().is_dir());
}

#[test]
fn a_rename_needs_a_parent_that_is_a_directory() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let to = |name: &str| move_file(&drive.path("file"), &drive.path(name), MOVE_FILE_FLAGS(0));
    assert_eq!(code(to(r"missing\new")), ERROR_PATH_NOT_FOUND);
    // This one is the driver's, opening the parent as a directory.
    assert_eq!(code(to(r"file\new")), ERROR_DIRECTORY);
    assert_eq!(drive.get("/file"), b"x");
}

#[test]
fn a_directory_cannot_move_into_itself() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.mkdir("/dir/sub");
    let moved = move_file(
        &drive.path("dir"),
        &drive.path(r"dir\sub\dir"),
        MOVE_FILE_FLAGS(0),
    );
    assert_eq!(code(moved), ERROR_ACCESS_DENIED);
    assert!(drive.attr("/dir/sub").is_some());
}

/// The way a program saves through a temporary name, with the handle still open.
#[test]
fn an_open_file_keeps_working_after_it_is_renamed() {
    let drive = Drive::new();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .access_mode(READ_WRITE_AND_DELETE)
        .share_mode(SHARE_ALL)
        .open(drive.path("old"))
        .unwrap();
    file.write_all(b"one").unwrap();

    move_file(&drive.path("old"), &drive.path("new"), MOVE_FILE_FLAGS(0)).unwrap();

    file.write_all(b"two").unwrap();
    assert_eq!(file.metadata().unwrap().len(), 6);
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut read = String::new();
    file.read_to_string(&mut read).unwrap();
    assert_eq!(read, "onetwo");
    file.set_len(3).unwrap();
    drop(file);

    assert!(drive.attr("/old").is_none());
    assert_eq!(drive.get("/new"), b"one");
}

/// A handle with nothing open remotely yet has only its path to go by.
#[test]
fn a_handle_that_has_not_touched_the_file_follows_a_rename_too() {
    let drive = Drive::new();
    drive.put("/old", b"contents");
    let mut other = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(drive.path("old"))
        .unwrap();

    move_file(&drive.path("old"), &drive.path("new"), MOVE_FILE_FLAGS(0)).unwrap();

    let mut read = String::new();
    other.read_to_string(&mut read).unwrap();
    assert_eq!(read, "contents");
    assert_eq!(other.metadata().unwrap().len(), 8);
}

#[test]
fn a_failed_rename_leaves_the_handle_where_it_was() {
    let drive = Drive::new();
    drive.put("/file", b"contents");
    let mut other = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(drive.path("file"))
        .unwrap();
    drive.probe.fail(Op::Rename, FsError::PermissionDenied);
    assert_eq!(
        code(move_file(
            &drive.path("file"),
            &drive.path("new"),
            MOVE_FILE_FLAGS(0)
        )),
        ERROR_ACCESS_DENIED
    );
    let mut read = String::new();
    other.read_to_string(&mut read).unwrap();
    assert_eq!(read, "contents");
}

// ---------------------------------------------------------------- copying

#[test]
fn copying_onto_the_drive_keeps_contents_time_and_attributes() {
    let drive = Drive::new();
    let local = LocalDir::new("copy-to");
    let source = local.file("source");
    let data = pattern(3 << 20, 9);
    fs::write(&source, &data).unwrap();
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(modified)
        .unwrap();

    copy(&source, &drive.path("copy"), false).unwrap();
    assert!(drive.get("/copy") == data);
    assert_eq!(drive.attr("/copy").unwrap().mtime, Some(modified));
    assert_eq!(
        fs::metadata(drive.path("copy"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );

    set_attributes(&source, FILE_ATTRIBUTE_READONLY.0).unwrap();
    copy(&source, &drive.path("copy2"), false).unwrap();
    assert!(drive.attr("/copy2").unwrap().is_read_only());
    assert!(drive.get("/copy2") == data);
    set_attributes(&source, FILE_ATTRIBUTE_NORMAL.0).unwrap();
}

#[test]
fn copying_off_the_drive_keeps_contents_and_time() {
    let drive = Drive::new();
    let local = LocalDir::new("copy-from");
    let data = pattern(3 << 20, 4);
    drive.put("/source", &data);
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let changes = AttrChanges {
        mtime: Some(modified),
        ..AttrChanges::default()
    };
    drive
        .server(
            drive
                .probe
                .inner
                .set_attr(AttrTarget::Path(&remote("/source")), changes),
        )
        .unwrap();

    let target = local.file("target");
    copy(&drive.path("source"), &target, false).unwrap();
    assert!(fs::read(&target).unwrap() == data);
    assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), modified);
}

#[test]
fn copying_within_the_drive_makes_a_second_file() {
    let drive = Drive::new();
    let data = pattern(1 << 20, 2);
    drive.put("/a", &data);
    // Through the drive: Windows Server 2025 lists a new drive's root as soon as it is up, and its
    // CopyFileExW looks up the target's directory in that listing, which would not show a
    // directory made on the server until the cache's TTL ran out.
    fs::create_dir(drive.path("dir")).unwrap();
    copy(&drive.path("a"), &drive.path(r"dir\b"), false).unwrap();
    assert!(drive.get("/dir/b") == data);
    assert!(drive.get("/a") == data);
}

#[test]
fn a_copy_replaces_a_target_unless_told_not_to() {
    let drive = Drive::new();
    drive.put("/source", b"new");
    drive.put("/target", b"old, and longer");

    let (source, target) = (drive.path("source"), drive.path("target"));
    assert_eq!(code(copy(&source, &target, true)), ERROR_FILE_EXISTS);
    assert_eq!(drive.get("/target"), b"old, and longer");

    copy(&source, &target, false).unwrap();
    assert_eq!(drive.get("/target"), b"new");
}

/// Every file downloaded through a browser has one of these. The stream has nowhere to go here, and
/// the copy must still work.
#[test]
fn a_source_with_an_alternate_stream_can_be_copied_on_to_the_drive() {
    let drive = Drive::new();
    let local = LocalDir::new("copy-stream");
    let source = local.file("source");
    fs::write(&source, b"contents").unwrap();
    let mut stream = source.clone().into_os_string();
    stream.push(":Zone.Identifier");
    if fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n").is_err() {
        // The temporary folder is not on NTFS, so this has nothing to show.
        return;
    }
    copy(&source, &drive.path("copy"), false).unwrap();
    assert_eq!(drive.get("/copy"), b"contents");
    assert_eq!(sorted_names(&drive, ""), ["copy"]);
}

#[test]
fn a_file_moved_on_to_the_drive_is_copied_and_the_source_goes() {
    let drive = Drive::new();
    let local = LocalDir::new("move-to");
    let source = local.file("source");
    fs::write(&source, b"contents").unwrap();
    move_file(&source, &drive.path("moved"), MOVEFILE_COPY_ALLOWED).unwrap();
    assert_eq!(drive.get("/moved"), b"contents");
    assert!(!source.exists());
}

#[test]
fn a_directory_of_many_files_can_be_written_and_listed() {
    let drive = Drive::new();
    drive.mkdir("/many");
    let names: Vec<String> = (0..300).map(|index| format!("file{index:03}")).collect();
    for (index, name) in names.iter().enumerate() {
        fs::write(
            drive.path(&format!(r"many\{name}")),
            pattern(100, index as u8),
        )
        .unwrap();
    }
    assert_eq!(sorted_names(&drive, "many"), names);
    for (index, name) in names.iter().enumerate() {
        assert_eq!(
            drive.get(&format!("/many/{name}")),
            pattern(100, index as u8)
        );
    }
}

// ---------------------------------------------------------------- replacing files

#[test]
fn replacefile_swaps_in_the_new_file() {
    let drive = Drive::new();
    drive.put("/document", b"old");
    drive.put("/document.tmp", b"new");
    replace(&drive.path("document"), &drive.path("document.tmp"), None).unwrap();
    assert_eq!(drive.get("/document"), b"new");
    assert_eq!(sorted_names(&drive, ""), ["document"]);
}

#[test]
fn replacefile_keeps_a_backup_when_asked() {
    let drive = Drive::new();
    drive.put("/document", b"old");
    drive.put("/document.tmp", b"new");
    let backup = drive.path("document.bak");
    replace(
        &drive.path("document"),
        &drive.path("document.tmp"),
        Some(&backup),
    )
    .unwrap();
    assert_eq!(drive.get("/document"), b"new");
    assert_eq!(drive.get("/document.bak"), b"old");
    assert_eq!(sorted_names(&drive, ""), ["document", "document.bak"]);
}

#[test]
fn replacefile_reports_a_missing_file() {
    let drive = Drive::new();
    drive.put("/document.tmp", b"new");
    replace(&drive.path("document"), &drive.path("document.tmp"), None).unwrap_err();
    assert_eq!(drive.get("/document.tmp"), b"new");
}

/// What a text editor does to save: write a temporary file, replace, repeat.
#[test]
fn saving_over_and_over_keeps_the_latest() {
    let drive = Drive::new();
    drive.put("/document", b"first");
    for round in 0..20 {
        let contents = format!("round {round}");
        fs::write(drive.path("document.tmp"), &contents).unwrap();
        replace(&drive.path("document"), &drive.path("document.tmp"), None).unwrap();
        assert_eq!(
            fs::read_to_string(drive.path("document")).unwrap(),
            contents
        );
    }
    assert_eq!(sorted_names(&drive, ""), ["document"]);
}

// ---------------------------------------------------------------- what the next listing shows (NFR-REL-02)

#[test]
fn changes_show_in_the_very_next_listing() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    let listed = |drive: &Drive| sorted_names(drive, "dir");
    assert!(listed(&drive).is_empty());

    fs::write(drive.path(r"dir\a"), b"x").unwrap();
    assert_eq!(listed(&drive), ["a"]);

    fs::create_dir(drive.path(r"dir\sub")).unwrap();
    assert_eq!(listed(&drive), ["a", "sub"]);

    move_file(
        &drive.path(r"dir\a"),
        &drive.path(r"dir\b"),
        MOVE_FILE_FLAGS(0),
    )
    .unwrap();
    assert_eq!(listed(&drive), ["b", "sub"]);

    delete(&drive.path(r"dir\b")).unwrap();
    assert_eq!(listed(&drive), ["sub"]);

    fs::remove_dir(drive.path(r"dir\sub")).unwrap();
    assert!(listed(&drive).is_empty());
}

#[test]
fn a_new_size_shows_in_the_very_next_listing() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let size = |drive: &Drive| {
        find(&drive.path("*"))
            .unwrap()
            .iter()
            .find(|found| found.name == "file")
            .unwrap()
            .size
    };
    assert_eq!(size(&drive), 1);
    fs::write(drive.path("file"), b"longer").unwrap();
    assert_eq!(size(&drive), 6);
}

#[test]
fn a_name_looked_for_and_then_created_opens_at_once() {
    let drive = Drive::new();
    assert_eq!(code(fs::metadata(drive.path("file"))), ERROR_NOT_FOUND);
    fs::write(drive.path("file"), b"x").unwrap();
    assert_eq!(fs::read(drive.path("file")).unwrap(), b"x");
    delete(&drive.path("file")).unwrap();
    assert_eq!(code(fs::metadata(drive.path("file"))), ERROR_NOT_FOUND);
}

// ---------------------------------------------------------------- failures and deadlines

#[test]
fn a_full_disk_says_so_and_what_was_written_before_stays() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"before").unwrap();
    file.sync_all().unwrap();
    drive.probe.fail(Op::Write, FsError::NoSpace);
    // Writes return before the server answers, so a later call is the one to hear (spec §5.6).
    file.write_all(b"x").unwrap();
    assert_eq!(code(file.sync_all()), ERROR_DISK_FULL);
    drop(file);
    assert_eq!(drive.get("/file"), b"before");
}

#[test]
fn a_server_that_says_no_is_heard() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    drive.mkdir("/dir");

    drive.probe.fail(Op::Open, FsError::PermissionDenied);
    assert_eq!(code(File::create(drive.path("new"))), ERROR_ACCESS_DENIED);
    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    assert_eq!(code(file.write_all(b"y")), ERROR_ACCESS_DENIED);

    drive.probe.fail(Op::Mkdir, FsError::PermissionDenied);
    assert_eq!(
        code(fs::create_dir(drive.path("newdir"))),
        ERROR_ACCESS_DENIED
    );

    drive.probe.fail(Op::SetAttr, FsError::PermissionDenied);
    assert_eq!(code(file.set_len(0)), ERROR_ACCESS_DENIED);
    assert_eq!(
        code(set_attributes(
            &drive.path("file"),
            FILE_ATTRIBUTE_READONLY.0
        )),
        ERROR_ACCESS_DENIED
    );
    assert_eq!(
        code(create_always(&drive.path("file"), 0)),
        ERROR_ACCESS_DENIED
    );
    assert_eq!(drive.get("/file"), b"x");
}

/// Not knowing whether it is empty is no reason to go ahead.
#[test]
fn a_directory_that_cannot_be_checked_is_not_removed() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.probe.fail(Op::ReadDir, FsError::PermissionDenied);
    assert_eq!(code(fs::remove_dir(drive.path("dir"))), ERROR_ACCESS_DENIED);
    assert!(drive.attr("/dir").is_some());
}

#[test]
fn a_failed_delete_leaves_the_file() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    drive.probe.fail(Op::RemoveFile, FsError::PermissionDenied);
    // Windows has no way to hear about this one.
    delete(&drive.path("file")).unwrap();
    assert_eq!(drive.get("/file"), b"x");
    assert_eq!(fs::read(drive.path("file")).unwrap(), b"x");
}

#[test]
fn a_hung_create_times_out() {
    let drive = short_drive();
    drive.probe.delay(Op::Open, HANG);
    assert_times_out(&drive, || File::create(drive.path("new")));
}

#[test]
fn a_hung_mkdir_times_out() {
    let drive = short_drive();
    drive.probe.delay(Op::Mkdir, HANG);
    assert_times_out(&drive, || fs::create_dir(drive.path("new")));
}

#[test]
fn a_hung_write_times_out_and_the_next_can_work() {
    if support::not_over_sftp("a hung request stays at the head of the server's queue") {
        return;
    }
    let drive = short_drive();
    drive.put("/file", b"before");
    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    drive.probe.delay(Op::Write, HANG);
    // The write returns at once; the flush waits for it, until the write's own limit.
    file.write_all(b"x").unwrap();
    assert_times_out(&drive, || file.sync_all());
    drive.probe.delay(Op::Write, Duration::ZERO);
    // The flush's own limit may end it just before the write's: then the write's failure is still to
    // be heard, once.
    let _ = file.sync_all();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"after!").unwrap();
    drop(file);
    assert_eq!(drive.get("/file"), b"after!");
}

#[test]
fn a_hung_flush_times_out() {
    let drive = short_drive();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"x").unwrap();
    drive.probe.delay(Op::Flush, HANG);
    assert_times_out(&drive, || file.sync_all());
}

#[test]
fn a_hung_size_change_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    let file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    drive.probe.delay(Op::SetAttr, HANG);
    assert_times_out(&drive, || file.set_len(0));
}

#[test]
fn a_hung_attribute_change_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    drive.probe.delay(Op::SetAttr, HANG);
    assert_times_out(&drive, || {
        set_attributes(&drive.path("file"), FILE_ATTRIBUTE_READONLY.0)
    });
}

#[test]
fn a_hung_overwrite_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    drive.probe.delay(Op::SetAttr, HANG);
    assert_times_out(&drive, || create_always(&drive.path("file"), 0));
}

#[test]
fn a_hung_rename_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    drive.probe.delay(Op::Rename, HANG);
    assert_times_out(&drive, || {
        move_file(&drive.path("file"), &drive.path("new"), MOVE_FILE_FLAGS(0))
    });
}

#[test]
fn a_hung_check_for_emptiness_times_out() {
    let drive = short_drive();
    drive.mkdir("/dir");
    drive.probe.delay(Op::ReadDir, HANG);
    assert_times_out(&drive, || fs::remove_dir(drive.path("dir")));
}

/// Windows waits for cleanup when a handle closes, and there is no error to report it with.
#[test]
fn closing_a_handle_is_bounded_when_the_server_hangs() {
    if support::not_over_sftp("a hung request stays at the head of the server's queue") {
        return;
    }
    let drive = short_drive();
    drive.put("/file", b"x");

    let mut written = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    written.write_all(b"y").unwrap();
    drive.probe.delay(Op::Flush, HANG);
    let start = Instant::now();
    drop(written);
    assert!(start.elapsed() < Duration::from_secs(5));

    drive.probe.delay(Op::RemoveFile, HANG);
    let start = Instant::now();
    delete(&drive.path("file")).unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_limit_is_for_the_whole_callback() {
    let drive = short_drive();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(b"x").unwrap();
    // Each call is under the limit, and a flush makes one of each.
    drive.probe.delay(Op::Flush, Duration::from_millis(150));
    drive.probe.delay(Op::Fstat, Duration::from_millis(150));
    assert_times_out(&drive, || file.sync_all());
}

// ---------------------------------------------------------------- a read-only drive (FR-MNT-07)

fn read_only_drive() -> Drive {
    Drive::with_options(Options {
        read_only: true,
        ..Options::default()
    })
}

#[test]
fn copying_into_a_read_only_drive_fails_cleanly() {
    let drive = read_only_drive();
    let local = LocalDir::new("copy-read-only");
    let source = local.file("source");
    fs::write(&source, b"contents").unwrap();
    assert_eq!(
        code(copy(&source, &drive.path("copy"), false)),
        ERROR_WRITE_PROTECT
    );
    assert!(sorted_names(&drive, "").is_empty());
}

#[test]
fn delete_on_close_does_nothing_on_a_read_only_drive() {
    let drive = read_only_drive();
    drive.put("/file", b"x");
    let opened = OpenOptions::new()
        .read(true)
        .access_mode(READ_AND_DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE.0)
        .open(drive.path("file"));
    drop(opened);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(drive.get("/file"), b"x");
}

#[test]
fn replacefile_and_attributes_are_refused_on_a_read_only_drive() {
    let drive = read_only_drive();
    drive.put("/a", b"a");
    drive.put("/b", b"b");
    replace(&drive.path("a"), &drive.path("b"), None).unwrap_err();
    assert_eq!(
        code(set_attributes(&drive.path("a"), FILE_ATTRIBUTE_READONLY.0)),
        ERROR_WRITE_PROTECT
    );
    assert_eq!(drive.get("/a"), b"a");
    assert_eq!(drive.get("/b"), b"b");
}

#[test]
fn setting_permissions_says_it_did_nothing() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    assert_eq!(
        code(set_open_security(&drive.path("file"))),
        ERROR_NOT_SUPPORTED
    );
    let drive = read_only_drive();
    drive.put("/file", b"x");
    assert_eq!(
        code(set_open_security(&drive.path("file"))),
        ERROR_WRITE_PROTECT
    );
}

#[test]
fn only_a_read_only_drive_says_it_is_one() {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let flags = |drive: &Drive| {
        let root = wide(&drive.path(""));
        let mut flags = 0;
        // SAFETY: `root` is NUL-terminated, and everything outlives the call.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                None,
                None,
                None,
                Some(&raw mut flags),
                None,
            )
        }
        .unwrap();
        flags
    };
    assert_ne!(flags(&read_only_drive()) & FILE_READ_ONLY_VOLUME, 0);
    assert_eq!(flags(&Drive::new()) & FILE_READ_ONLY_VOLUME, 0);
}

// ---------------------------------------------------------------- what a written file shows

#[test]
fn a_written_file_shows_times_and_size_from_the_server() {
    let drive = Drive::new();
    let mut file = File::create(drive.path("file")).unwrap();
    file.write_all(&[1; 5000]).unwrap();
    // Once the writes have arrived: until then the times are the server's last answer.
    file.sync_all().unwrap();
    let metadata = file.metadata().unwrap();
    assert_eq!(metadata.len(), 5000);
    let attr: Attr = drive.attr("/file").unwrap();
    assert!(support::same_time(
        metadata.modified().unwrap(),
        attr.mtime.unwrap()
    ));
    assert!(attr.mtime.unwrap() <= SystemTime::now());
}
