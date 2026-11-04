// SPDX-License-Identifier: GPL-3.0-or-later

//! The read path of a mounted drive, through the same Win32 calls programs make (M1-12a).

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fsm_vfs::{AttrChanges, AttrTarget, FsError, RemoteFs};
use fsm_win::drives::is_letter_free;
use support::{
    Drive, Op, Options, eventually, find, find_first, pattern, raw_handle, remote, wide,
};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetFileSecurityW,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, GetDiskFreeSpaceExW, GetDriveTypeW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationW,
    VOLUME_NAME_NONE,
};
use windows::core::{PCWSTR, PWSTR};

const ERROR_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_TOO_MANY_OPEN: i32 = 4;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_WRITE_PROTECT: i32 = 19;
const ERROR_SEM_TIMEOUT: i32 = 121;
const ERROR_INVALID_NAME: i32 = 123;
const ERROR_NETWORK_UNREACHABLE: i32 = 1231;

const DRIVE_REMOTE: u32 = 4;

/// What Windows calls the file system's capability bits we rely on.
const FILE_CASE_PRESERVED_NAMES: u32 = 0x2;
const FILE_UNICODE_ON_DISK: u32 = 0x4;
const FILE_CASE_SENSITIVE_SEARCH: u32 = 0x1;
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

fn code<T: std::fmt::Debug>(result: io::Result<T>) -> i32 {
    result.unwrap_err().raw_os_error().unwrap()
}

fn sorted_names(drive: &Drive, directory: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(drive.path(directory))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------- listing

#[test]
fn the_root_lists_every_entry_and_no_dot_entries() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/a.txt", b"a");
    drive.put("/.hidden", b"");

    let mut names: Vec<String> = find(&drive.path("*"))
        .unwrap()
        .into_iter()
        .map(|found| found.name)
        .collect();
    names.sort();
    assert_eq!(names, [".hidden", "a.txt", "dir"]);
}

#[test]
fn a_subdirectory_lists_its_entries_and_dot_entries() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/dir/b.txt", b"bb");

    let found = find(&drive.path(r"dir\*")).unwrap();
    let names: Vec<&str> = found.iter().map(|found| found.name.as_str()).collect();
    assert_eq!(names, [".", "..", "b.txt"]);
    for dot in &found[..2] {
        assert_ne!(dot.attributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
    }
    assert_eq!(found[2].size, 2);
}

#[test]
fn an_empty_directory_lists_without_error() {
    let drive = Drive::new();
    drive.mkdir("/empty");
    assert!(sorted_names(&drive, "empty").is_empty());
}

/// A directory this big is never cached, so a listing per page would show up here (M1-05).
#[test]
fn a_listing_of_many_pages_lists_the_directory_once() {
    let drive = Drive::new();
    drive.mkdir("/big");
    let mut expected: Vec<String> = (0..10_050)
        .map(|index| {
            format!("a-rather-long-file-name-so-that-pages-fill-up-quickly-{index:05}.txt")
        })
        .collect();
    for name in &expected {
        drive.put(&format!("/big/{name}"), b"");
    }
    expected.sort();

    let names = sorted_names(&drive, "big");
    assert_eq!(names, expected);
    assert_eq!(drive.probe.listings_of("/big"), 1);
}

/// Win32's searches open the directory anew each time, so only this API restarts on one handle.
fn list_from_the_start(directory: &File) -> Vec<String> {
    let mut buffer = vec![0u64; 8192];
    let bytes = u32::try_from(buffer.len() * size_of::<u64>()).unwrap();
    let mut names = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: the handle is open, and the buffer holds `bytes` bytes and outlives the call.
        let result = unsafe {
            GetFileInformationByHandleEx(
                raw_handle(directory),
                class,
                buffer.as_mut_ptr().cast(),
                bytes,
            )
        };
        if result.is_err() {
            return names;
        }
        class = FileFullDirectoryInfo;

        let mut entry = buffer.as_ptr().cast::<FILE_FULL_DIR_INFO>();
        loop {
            // SAFETY: the call filled the buffer with a chain of entries, each aligned, and with its
            // name inside the buffer. The chain ends at an offset of 0.
            let (next, name) = unsafe {
                let units = (*entry).FileNameLength as usize / size_of::<u16>();
                let name =
                    std::slice::from_raw_parts((&raw const (*entry).FileName).cast::<u16>(), units);
                (
                    (*entry).NextEntryOffset as usize,
                    String::from_utf16(name).unwrap(),
                )
            };
            names.push(name);
            if next == 0 {
                break;
            }
            // SAFETY: a non-zero offset leads to the next entry in the buffer.
            entry = unsafe { entry.byte_add(next) };
        }
    }
}

#[test]
fn restarting_a_listing_on_one_handle_lists_the_directory_again() {
    let drive = Drive::with_options(Options {
        cache_ttl: Duration::ZERO,
        ..Options::default()
    });
    drive.mkdir("/dir");
    drive.put("/dir/first", b"");
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(drive.path("dir"))
        .unwrap();
    assert!(list_from_the_start(&directory).contains(&"first".to_owned()));

    drive.put("/dir/later", b"");
    assert!(
        eventually(|| list_from_the_start(&directory).contains(&"later".to_owned())),
        "the handle kept its first listing"
    );
}

#[test]
fn a_failed_listing_says_why() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.probe.fail(Op::ReadDir, FsError::PermissionDenied);
    assert_eq!(code(fs::read_dir(drive.path("dir"))), ERROR_ACCESS_DENIED);
}

// ---------------------------------------------------------------- attributes

#[test]
fn metadata_shows_kind_size_and_read_only_bit() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/file", &[7; 5000]);
    drive.put("/locked", b"x");
    let locked = AttrChanges {
        read_only: Some(true),
        ..AttrChanges::default()
    };
    let path = remote("/locked");
    drive
        .server(drive.probe.inner.set_attr(AttrTarget::Path(&path), locked))
        .unwrap();

    let dir = fs::metadata(drive.path("dir")).unwrap();
    assert!(dir.is_dir());

    let file = fs::metadata(drive.path("file")).unwrap();
    assert!(file.is_file());
    assert_eq!(file.len(), 5000);
    assert!(!file.permissions().readonly());

    let locked = fs::metadata(drive.path("locked")).unwrap();
    assert!(locked.permissions().readonly());
    assert_ne!(locked.file_attributes() & FILE_ATTRIBUTE_READONLY.0, 0);

    let listed = find_first(&drive.path("locked")).unwrap();
    assert_ne!(listed.attributes & FILE_ATTRIBUTE_READONLY.0, 0);
}

#[test]
fn the_root_is_a_directory() {
    let drive = Drive::new();
    assert!(fs::metadata(drive.path("")).unwrap().is_dir());
}

#[test]
fn times_come_from_the_server() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let modified = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    let accessed = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let changes = AttrChanges {
        mtime: Some(modified),
        atime: Some(accessed),
        ..AttrChanges::default()
    };
    let path = remote("/file");
    drive
        .server(drive.probe.inner.set_attr(AttrTarget::Path(&path), changes))
        .unwrap();

    let metadata = fs::metadata(drive.path("file")).unwrap();
    assert_eq!(metadata.modified().unwrap(), modified);
    assert_eq!(metadata.accessed().unwrap(), accessed);
    // No birth time on SFTP v3.
    assert_eq!(metadata.created().unwrap(), modified);
}

#[test]
fn an_open_file_reports_what_the_server_says_now() {
    let drive = Drive::with_options(Options {
        cache_ttl: Duration::from_secs(60),
        ..Options::default()
    });
    drive.put("/grows", b"1234");
    let mut file = File::open(drive.path("grows")).unwrap();
    let mut first = [0; 2];
    file.read_exact(&mut first).unwrap();

    drive.put("/grows", &[9; 9000]);
    assert!(
        eventually(|| file.metadata().unwrap().len() == 9000),
        "the size of an open file stayed stale"
    );
    assert!(drive.probe.count(Op::Fstat) > 0);
}

#[test]
fn files_have_ids_of_their_own() {
    let drive = Drive::new();
    drive.put("/a", b"");
    drive.put("/b", b"");

    let id = |name| {
        let file = File::open(drive.path(name)).unwrap();
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open, and `info` outlives the call.
        unsafe { GetFileInformationByHandle(raw_handle(&file), &raw mut info) }.unwrap();
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
    };
    assert_eq!(id("a"), id("a"));
    assert_ne!(id("a"), id("b"));
}

// ---------------------------------------------------------------- reading

#[test]
fn a_file_reads_back_the_same() {
    let drive = Drive::new();
    // Over the size of a window, and over MemFs's largest request.
    let data = pattern(9 * 1024 * 1024 + 123, 1);
    drive.put("/file.bin", &data);

    assert_eq!(fs::read(drive.path("file.bin")).unwrap(), data);
}

#[test]
fn an_empty_file_reads_as_nothing() {
    let drive = Drive::new();
    drive.put("/empty", b"");
    assert!(fs::read(drive.path("empty")).unwrap().is_empty());
}

#[test]
fn reads_can_start_anywhere_and_end_at_the_end_of_the_file() {
    let drive = Drive::new();
    let data = pattern(1_000_000, 2);
    drive.put("/file", &data);
    let mut file = File::open(drive.path("file")).unwrap();

    for offset in [999_990, 0, 123_457, 500_000] {
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut buffer = [0; 100];
        let count = file.read(&mut buffer).unwrap();
        let start = usize::try_from(offset).unwrap();
        assert_eq!(count, 100.min(data.len() - start), "at {offset}");
        assert_eq!(&buffer[..count], &data[start..start + count], "at {offset}");
    }

    file.seek(SeekFrom::Start(1_000_000)).unwrap();
    assert_eq!(file.read(&mut [0; 10]).unwrap(), 0);
    file.seek(SeekFrom::Start(5_000_000)).unwrap();
    assert_eq!(file.read(&mut [0; 10]).unwrap(), 0);
}

#[test]
fn a_remote_file_is_opened_on_the_first_read_and_closed_with_the_last_handle() {
    let drive = Drive::new();
    drive.put("/file", &pattern(100_000, 3));

    let mut file = File::open(drive.path("file")).unwrap();
    let _ = file.metadata().unwrap();
    assert_eq!(drive.probe.count(Op::Open), 0, "opened before any read");

    file.read_exact(&mut [0; 10]).unwrap();
    file.read_exact(&mut [0; 10]).unwrap();
    assert_eq!(drive.probe.count(Op::Open), 1);
    assert_eq!(drive.probe.count(Op::Close), 0);

    drop(file);
    assert!(eventually(|| drive.probe.count(Op::Close) == 1));
}

#[test]
fn looking_at_a_file_needs_no_remote_handle() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    for _ in 0..5 {
        drop(File::open(drive.path("file")).unwrap());
        let _ = fs::metadata(drive.path("file")).unwrap();
    }
    assert_eq!(drive.probe.count(Op::Open), 0);
}

#[test]
fn sequential_reads_are_fetched_ahead_and_a_single_read_is_not() {
    let drive = Drive::new();
    let data = pattern(4 * 1024 * 1024, 4);
    drive.put("/file", &data);

    let mut file = File::open(drive.path("file")).unwrap();
    let mut buffer = [0; 4096];
    file.read_exact(&mut buffer).unwrap();
    assert_eq!(
        drive.probe.count(Op::Read),
        1,
        "a first read fetches no more"
    );

    let mut read = buffer.to_vec();
    while read.len() < data.len() {
        file.read_exact(&mut buffer).unwrap();
        read.extend_from_slice(&buffer);
    }
    assert_eq!(read, data);
    let requests = drive.probe.count(Op::Read);
    assert!(requests < 100, "{requests} requests for 1024 reads");
}

#[test]
fn several_readers_get_the_right_bytes() {
    let drive = Drive::new();
    let contents: Vec<Vec<u8>> = (0..4).map(|seed| pattern(3_000_000, seed)).collect();
    for (index, data) in contents.iter().enumerate() {
        drive.put(&format!("/{index}"), data);
    }

    std::thread::scope(|scope| {
        for round in 0..12 {
            let index = round % contents.len();
            let (drive, data) = (&drive, &contents[index]);
            scope.spawn(move || {
                assert_eq!(&fs::read(drive.path(&index.to_string())).unwrap(), data);
            });
        }
    });
}

#[test]
fn a_directory_cannot_be_read_as_a_file_or_a_file_listed() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/file", b"x");

    assert_eq!(code(File::open(drive.path("dir"))), ERROR_ACCESS_DENIED);
    // A search looks in `file\`, and WinFsp's driver refuses a trailing backslash on a file itself.
    assert_eq!(code(fs::read_dir(drive.path("file"))), ERROR_INVALID_NAME);
}

// ---------------------------------------------------------------- names

#[test]
fn a_name_in_the_wrong_case_finds_the_file_and_gives_its_real_name() {
    let drive = Drive::new();
    drive.mkdir("/Dir");
    drive.put("/Dir/File.TXT", b"contents");

    assert_eq!(fs::read(drive.path(r"dIR\fILE.txt")).unwrap(), b"contents");
    assert!(fs::metadata(drive.path(r"DIR\FILE.TXT")).unwrap().is_file());

    let file = File::open(drive.path(r"dir\file.txt")).unwrap();
    let mut buffer = [0; 260];
    // SAFETY: the handle is open, and the buffer outlives the call.
    let len =
        unsafe { GetFinalPathNameByHandleW(raw_handle(&file), &mut buffer, VOLUME_NAME_NONE) };
    let shown = String::from_utf16(&buffer[..len as usize]).unwrap();
    assert!(shown.ends_with(r"\Dir\File.TXT"), "{shown}");
}

#[test]
fn a_name_outside_ascii_matches_in_either_case() {
    let drive = Drive::new();
    drive.put("/Żółć.txt", b"polish");
    assert_eq!(sorted_names(&drive, ""), ["Żółć.txt"]);
    assert_eq!(fs::read(drive.path("żÓŁĆ.TXT")).unwrap(), b"polish");
}

#[test]
fn names_that_differ_by_case_are_all_listed_and_a_guess_opens_neither() {
    let drive = Drive::new();
    drive.put("/notes", b"lower");
    drive.put("/NOTES", b"UPPER");

    assert_eq!(sorted_names(&drive, ""), ["NOTES", "notes"]);
    assert_eq!(fs::read(drive.path("notes")).unwrap(), b"lower");
    assert_eq!(code(fs::metadata(drive.path("Notes"))), ERROR_NOT_FOUND);
}

#[test]
fn names_windows_forbids_are_shown_mapped_and_can_be_read() {
    let drive = Drive::new();
    for name in ["a:b.txt", "q?", "trail.", "space ", "back\\slash"] {
        drive.put(&format!("/{name}"), name.as_bytes());
    }

    assert_eq!(
        sorted_names(&drive, ""),
        [
            "a\u{F03A}b.txt",
            "back\u{F05C}slash",
            "q\u{F03F}",
            "space\u{F020}",
            "trail\u{F02E}",
        ]
    );
    let read = |shown: &str| fs::read(drive.path(shown)).unwrap();
    assert_eq!(read("a\u{F03A}b.txt"), b"a:b.txt");
    assert_eq!(read("q\u{F03F}"), b"q?");
    assert_eq!(read("trail\u{F02E}"), b"trail.");
    assert_eq!(read("space\u{F020}"), b"space ");
    assert_eq!(read("back\u{F05C}slash"), b"back\\slash");
}

#[test]
fn a_name_that_really_has_the_private_character_can_be_read() {
    let drive = Drive::new();
    drive.put("/x\u{F03A}", b"only one");
    drive.put("/y:", b"mapped");
    drive.put("/y\u{F03A}", b"literal");

    assert_eq!(fs::read(drive.path("x\u{F03A}")).unwrap(), b"only one");
    assert_eq!(fs::read(drive.path("y\u{F03A}")).unwrap(), b"mapped");
    let names = sorted_names(&drive, "");
    assert_eq!(names.iter().filter(|name| *name == "y\u{F03A}").count(), 2);
}

#[test]
fn a_name_too_long_for_windows_is_left_out_of_the_listing() {
    let drive = Drive::new();
    let longest = "n".repeat(255);
    let too_long = "m".repeat(256);
    drive.put(&format!("/{longest}"), b"fits");
    drive.put(&format!("/{too_long}"), b"does not");
    drive.put("/other", b"");

    assert_eq!(
        sorted_names(&drive, ""),
        [longest.clone(), "other".to_owned()]
    );
    assert_eq!(fs::read(drive.path(&longest)).unwrap(), b"fits");
}

#[test]
fn missing_paths_get_the_right_errors() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/file", b"");

    let error_for = |path: &str| code(fs::metadata(drive.path(path)));
    assert_eq!(error_for("missing"), ERROR_NOT_FOUND);
    assert_eq!(error_for(r"dir\missing"), ERROR_NOT_FOUND);
    assert_eq!(error_for(r"missing\file"), ERROR_PATH_NOT_FOUND);
    assert_eq!(error_for(r"missing\deeper\file"), ERROR_PATH_NOT_FOUND);
    assert_eq!(error_for(r"file\below"), ERROR_PATH_NOT_FOUND);
    assert_eq!(error_for(r"DIR\MISSING"), ERROR_NOT_FOUND);
}

#[test]
fn a_name_with_a_lone_surrogate_is_invalid() {
    let drive = Drive::new();
    let mut name: Vec<u16> = drive.path("").as_os_str().encode_wide().collect();
    name.extend([u16::from(b'a'), 0xD800]);
    let path = OsString::from_wide(&name);
    assert_eq!(code(fs::metadata(path)), ERROR_INVALID_NAME);
}

/// Windows passes the name upper-cased, so a search cannot try the spelling it was given. The cached
/// listing answers instead, without a call per search or a call per link in the directory.
#[test]
fn searches_for_one_name_share_a_cached_listing() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/dir/File.txt", &[0; 12]);
    drive.put("/dir/other", b"");
    drive.put("/target", b"");
    drive.probe.add_link("/dir/link", "/target");

    for _ in 0..3 {
        let found = find_first(&drive.path(r"dir\File.txt")).unwrap();
        assert_eq!(found.name, "File.txt");
        assert_eq!(found.size, 12);
        assert_eq!(find_first(&drive.path(r"dir\other")).unwrap().name, "other");
    }
    assert_eq!(drive.probe.listings_of("/dir"), 1);
}

/// What sets a search for one name apart from filtering a whole listing, which would follow them all.
#[test]
fn a_search_for_one_name_does_not_follow_other_links() {
    let drive = Drive::with_options(Options {
        cache_ttl: Duration::from_secs(60),
        ..Options::default()
    });
    drive.mkdir("/dir");
    drive.put("/dir/other", b"");
    drive.put("/target", b"");
    drive.probe.add_link("/dir/link", "/target");
    // Remembers the directory, so that only following the link would need another stat.
    let _ = fs::metadata(drive.path("dir")).unwrap();

    drive.probe.fail(Op::Stat, FsError::Timeout);
    assert_eq!(find_first(&drive.path(r"dir\other")).unwrap().name, "other");
}

#[test]
fn a_search_finds_a_name_that_several_entries_match() {
    let drive = Drive::new();
    drive.put("/notes", b"lower");
    drive.put("/Notes", b"mixed");

    // An open can tell them apart, and has to refuse a guess. A search has no such choice: Windows
    // hands it the name upper-cased, and finding nothing would make both look missing.
    for name in ["notes", "Notes", "NOTES"] {
        assert_eq!(
            find_first(&drive.path(name)).unwrap().name,
            "Notes",
            "{name}"
        );
    }
}

#[test]
fn a_search_for_one_name_gives_its_real_spelling() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/dir/File.txt", b"");
    drive.put("/dir/a:b", b"");

    let name_of = |pattern: &str| find_first(&drive.path(pattern)).unwrap().name;
    assert_eq!(name_of(r"dir\FILE.TXT"), "File.txt");
    assert_eq!(name_of("dir\\A\u{F03A}B"), "a\u{F03A}b");
    let missing = find_first(&drive.path(r"dir\missing")).unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(ERROR_NOT_FOUND));
}

// ---------------------------------------------------------------- symbolic links

fn drive_with_links() -> Drive {
    let drive = Drive::new();
    drive.mkdir("/target");
    drive.put("/target/inner.txt", b"through the link");
    drive.put("/file.txt", &[1; 1234]);
    drive.probe.add_link("/to-dir", "/target");
    drive.probe.add_link("/to-file", "/file.txt");
    drive.probe.add_link("/broken", "/nowhere");
    drive
}

#[test]
fn a_listing_shows_links_as_their_targets_and_broken_ones_as_files() {
    let drive = drive_with_links();
    let mut found = find(&drive.path("*")).unwrap();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    let of = |name: &str| found.iter().find(|found| found.name == name).unwrap();

    assert_ne!(of("to-dir").attributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
    assert_eq!(of("to-file").attributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
    assert_eq!(of("to-file").size, 1234);
    assert_eq!(of("broken").attributes & FILE_ATTRIBUTE_DIRECTORY.0, 0);
}

#[test]
fn links_can_be_used_like_their_targets() {
    let drive = drive_with_links();
    assert!(fs::metadata(drive.path("to-dir")).unwrap().is_dir());
    assert_eq!(sorted_names(&drive, "to-dir"), ["inner.txt"]);
    assert_eq!(
        fs::read(drive.path(r"to-dir\inner.txt")).unwrap(),
        b"through the link"
    );
    assert_eq!(fs::read(drive.path("to-file")).unwrap(), [1; 1234]);
    assert_eq!(find_first(&drive.path("to-file")).unwrap().size, 1234);
}

#[test]
fn a_link_in_the_wrong_case_still_leads_to_its_target() {
    let drive = drive_with_links();
    assert_eq!(
        fs::read(drive.path(r"TO-DIR\INNER.TXT")).unwrap(),
        b"through the link"
    );
    assert_eq!(
        code(fs::metadata(drive.path(r"TO-FILE\x"))),
        ERROR_PATH_NOT_FOUND
    );
}

#[test]
fn a_broken_link_is_listed_but_does_not_open() {
    let drive = drive_with_links();
    assert!(sorted_names(&drive, "").contains(&"broken".to_owned()));
    assert_eq!(code(File::open(drive.path("broken"))), ERROR_NOT_FOUND);
    assert_eq!(find_first(&drive.path("broken")).unwrap().name, "broken");
    assert_eq!(
        code(fs::metadata(drive.path(r"broken\x"))),
        ERROR_PATH_NOT_FOUND
    );
}

#[test]
fn a_link_that_times_out_fails_the_listing() {
    let drive = drive_with_links();
    drive.mkdir("/dir");
    drive.probe.add_link("/dir/slow", "/target");
    // The directory's own lookup is the first stat, and it has to pass.
    let _ = fs::metadata(drive.path("dir")).unwrap();
    drive.probe.fail(Op::Stat, FsError::Timeout);
    assert_eq!(code(fs::read_dir(drive.path("dir"))), ERROR_SEM_TIMEOUT);
}

#[test]
fn a_link_that_cannot_be_followed_for_another_reason_is_still_listed() {
    let drive = Drive::with_options(Options {
        cache_ttl: Duration::from_secs(60),
        ..Options::default()
    });
    drive.mkdir("/dir");
    drive.put("/secret", b"");
    drive.probe.add_link("/dir/denied", "/secret");
    let _ = fs::metadata(drive.path("dir")).unwrap();
    drive.probe.fail(Op::Stat, FsError::PermissionDenied);
    assert_eq!(sorted_names(&drive, "dir"), ["denied"]);
}

// ---------------------------------------------------------------- deadlines and errors

const SHORT: Duration = Duration::from_millis(300);
/// Far under any hang, and far over what a callback should take beyond `SHORT`.
const SOON: Duration = Duration::from_secs(5);

fn short_drive() -> Drive {
    Drive::with_options(Options {
        op_timeout: SHORT,
        ..Options::default()
    })
}

const FOR_EVER: Duration = Duration::from_secs(3600);

fn assert_times_out<T: std::fmt::Debug>(drive: &Drive, call: impl FnOnce() -> io::Result<T>) {
    let suspect = drive.backend.suspect();
    let before = *suspect.borrow();
    let start = Instant::now();
    assert_eq!(code(call()), ERROR_SEM_TIMEOUT);
    assert!(start.elapsed() < SOON, "took {:?}", start.elapsed());
    assert!(*suspect.borrow() > before, "not marked as suspect");
}

#[test]
fn a_hung_lookup_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    drive.probe.delay(Op::Stat, FOR_EVER);
    assert_times_out(&drive, || fs::metadata(drive.path("file")));
}

#[test]
fn a_hung_listing_times_out() {
    let drive = short_drive();
    drive.mkdir("/dir");
    let _ = fs::metadata(drive.path("dir")).unwrap();
    drive.probe.delay(Op::ReadDir, FOR_EVER);
    assert_times_out(&drive, || fs::read_dir(drive.path("dir")));
}

#[test]
fn a_hung_open_times_out() {
    let drive = short_drive();
    drive.put("/file", b"x");
    let mut file = File::open(drive.path("file")).unwrap();
    drive.probe.delay(Op::Open, FOR_EVER);
    assert_times_out(&drive, || file.read(&mut [0; 4]));
}

#[test]
fn a_hung_read_times_out_and_the_next_can_work() {
    let drive = short_drive();
    drive.put("/file", b"contents");
    let mut file = File::open(drive.path("file")).unwrap();
    drive.probe.delay(Op::Read, FOR_EVER);
    assert_times_out(&drive, || file.read(&mut [0; 4]));

    drive.probe.delay(Op::Read, Duration::ZERO);
    let mut buffer = [0; 8];
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_exact(&mut buffer).unwrap();
    assert_eq!(&buffer, b"contents");
}

#[test]
fn a_hung_volume_query_times_out() {
    let drive = short_drive();
    drive.probe.delay(Op::Statfs, FOR_EVER);
    let root = wide(&drive.path(""));
    assert_times_out(&drive, || {
        let mut free = 0;
        // SAFETY: `root` is NUL-terminated, and both outlive the call.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), Some(&raw mut free), None, None) }
            .map(|()| free)
            .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xFFFF))
    });
}

/// Each remote call stays within its limit here. Only the callback as a whole runs out of time, and
/// that has to count as suspect as well.
#[test]
fn the_limit_is_for_the_callback_not_for_each_call() {
    let drive = short_drive();
    drive.probe.delay(Op::Stat, Duration::from_millis(200));
    drive.probe.delay(Op::ReadDir, Duration::from_millis(200));
    assert_times_out(&drive, || fs::metadata(drive.path("missing")));
}

#[test]
fn a_lost_connection_reads_as_an_unreachable_network() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let mut file = File::open(drive.path("file")).unwrap();
    drive.probe.fail(Op::Read, FsError::ConnectionLost);
    assert_eq!(code(file.read(&mut [0; 1])), ERROR_NETWORK_UNREACHABLE);
}

#[test]
fn a_server_that_refuses_the_open_is_refused_on_the_first_read() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let mut file = File::open(drive.path("file")).unwrap();
    drive.probe.fail(Op::Open, FsError::PermissionDenied);
    assert_eq!(code(file.read(&mut [0; 1])), ERROR_ACCESS_DENIED);
}

#[test]
fn hitting_the_limit_on_open_files_says_so_and_can_be_retried() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    let mut file = File::open(drive.path("file")).unwrap();
    drive.probe.fail(Op::Open, FsError::TooManyOpenFiles);
    assert_eq!(code(file.read(&mut [0; 1])), ERROR_TOO_MANY_OPEN);
}

// ---------------------------------------------------------------- the volume

#[test]
fn the_volume_describes_itself() {
    let drive = Drive::with_options(Options {
        label: "A label that is longer than thirty-two characters".to_owned(),
        ..Options::default()
    });
    let root = wide(&drive.path(""));

    // SAFETY: `root` is NUL-terminated and outlives the call.
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    assert_eq!(drive_type, DRIVE_REMOTE);

    let (mut label, mut file_system) = ([0; 64], [0; 64]);
    let (mut serial, mut max_component, mut flags) = (0, 0, 0);
    // SAFETY: `root` is NUL-terminated, and everything outlives the call.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            Some(&mut label),
            Some(&raw mut serial),
            Some(&raw mut max_component),
            Some(&raw mut flags),
            Some(&mut file_system),
        )
    }
    .unwrap();

    let text = |buffer: &[u16]| {
        let end = buffer.iter().position(|unit| *unit == 0).unwrap();
        String::from_utf16(&buffer[..end]).unwrap()
    };
    assert_eq!(text(&label), "A label that is longer than thir");
    assert_eq!(text(&file_system), "FSM-SFTP");
    assert_eq!(max_component, 255);
    assert_ne!(flags & FILE_CASE_PRESERVED_NAMES, 0);
    assert_ne!(flags & FILE_UNICODE_ON_DISK, 0);
    assert_eq!(flags & FILE_CASE_SENSITIVE_SEARCH, 0);
    assert_ne!(flags & FILE_READ_ONLY_VOLUME, 0);
}

#[test]
fn free_space_comes_from_the_server_and_is_remembered() {
    let drive = Drive::new();
    let root = wide(&drive.path(""));
    let space = || {
        let (mut free, mut total) = (0, 0);
        // SAFETY: `root` is NUL-terminated, and everything outlives the call.
        unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(root.as_ptr()),
                Some(&raw mut free),
                Some(&raw mut total),
                None,
            )
        }
        .unwrap();
        (free, total)
    };

    let expected = drive.server(drive.probe.inner.statfs()).unwrap();
    assert_eq!(space(), (expected.free_bytes, expected.total_bytes));

    // Longer than Windows's own memory of it, which is FileInfoTimeout.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(space(), (expected.free_bytes, expected.total_bytes));
    assert_eq!(drive.probe.count(Op::Statfs), 1);
}

#[test]
fn every_file_gives_full_control_to_this_user_alone() {
    let drive = Drive::new();
    drive.mkdir("/dir");
    drive.put("/file", b"");

    for name in ["", "dir", "file"] {
        let sddl = security_of(&drive.path(name));
        let owner = sddl
            .strip_prefix("O:")
            .and_then(|rest| rest.split("G:").next())
            .unwrap();
        assert!(owner.starts_with("S-1-") || owner.len() == 2, "{sddl}");
        assert_eq!(sddl, format!("O:{owner}G:{owner}D:P(A;;FA;;;{owner})"));
    }
}

fn security_of(path: &std::path::Path) -> String {
    let path = wide(path);
    let info = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut descriptor = vec![0u64; 512];
    let mut needed = 0;
    let bytes = (descriptor.len() * size_of::<u64>()) as u32;
    let pointer = PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast());
    // SAFETY: the path is NUL-terminated, and the buffer holds `bytes` bytes. Everything outlives
    // the call.
    unsafe {
        GetFileSecurityW(
            PCWSTR(path.as_ptr()),
            info.0,
            Some(pointer),
            bytes,
            &raw mut needed,
        )
    }
    .ok()
    .unwrap();

    let mut text = PWSTR::null();
    // SAFETY: the descriptor is the valid one just read, and `text` receives a string to free below.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            pointer,
            SDDL_REVISION_1,
            info,
            &raw mut text,
            None,
        )
    }
    .unwrap();
    // SAFETY: on success `text` is a NUL-terminated string, freed once here and not used after.
    unsafe {
        let sddl = text.to_string().unwrap();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sddl
    }
}

#[test]
fn a_read_only_volume_refuses_changes_before_they_reach_the_server() {
    let drive = Drive::new();
    drive.put("/file", b"before");
    drive.mkdir("/dir");

    // Opening for writing works, and the writing does not.
    let mut file = OpenOptions::new()
        .write(true)
        .open(drive.path("file"))
        .unwrap();
    assert_eq!(code(file.write_all(b"x")), ERROR_WRITE_PROTECT);
    assert_eq!(code(file.set_len(0)), ERROR_WRITE_PROTECT);
    assert_eq!(code(file.sync_all()), ERROR_WRITE_PROTECT);
    let times = fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1));
    assert_eq!(code(file.set_times(times)), ERROR_WRITE_PROTECT);
    drop(file);

    assert_eq!(code(File::create(drive.path("new"))), ERROR_WRITE_PROTECT);
    assert_eq!(code(File::create(drive.path("file"))), ERROR_WRITE_PROTECT);
    assert_eq!(
        code(fs::create_dir(drive.path("newdir"))),
        ERROR_WRITE_PROTECT
    );
    assert_eq!(
        code(fs::remove_file(drive.path("file"))),
        ERROR_WRITE_PROTECT
    );
    assert_eq!(code(fs::remove_dir(drive.path("dir"))), ERROR_WRITE_PROTECT);
    assert_eq!(
        code(fs::rename(drive.path("file"), drive.path("moved"))),
        ERROR_WRITE_PROTECT
    );

    assert_eq!(fs::read(drive.path("file")).unwrap(), b"before");
    assert_eq!(sorted_names(&drive, ""), ["dir", "file"]);
}

// ---------------------------------------------------------------- starting and stopping

#[test]
fn a_drive_appears_goes_away_and_can_come_back() {
    let drive = Drive::new();
    drive.put("/file", b"x");
    assert!(!is_letter_free(drive.letter).unwrap());
    assert_eq!(fs::read(drive.path("file")).unwrap(), b"x");

    // A second start is refused, even on a letter that would work, and does no harm.
    assert!(drive.backend.start(drive.letter).is_err());
    assert!(support::try_start_on_a_free_letter(&drive.backend).is_err());
    assert_eq!(fs::read(drive.path("file")).unwrap(), b"x");

    drive.backend.stop();
    assert!(eventually(|| is_letter_free(drive.letter).unwrap()));
    assert!(fs::metadata(drive.path("file")).is_err());
    drive.backend.stop();

    // On any letter: another test may have taken this one by now.
    let letter = support::start_on_a_free_letter(&drive.backend);
    assert_eq!(fs::read(format!(r"{letter}\file")).unwrap(), b"x");
}
