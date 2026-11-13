// SPDX-License-Identifier: GPL-3.0-or-later

//! The `fsx`-style exerciser itself, on any file: random reads, writes, truncates, mapped reads and
//! writes and reopens, with a copy kept in memory that every read is checked against.
//!
//! It needs only the standard library and `windows`, so that a test outside this crate can take it in
//! with `#[path]`: `tests/fsx.rs` runs it on a drive over `MemFs` or the in-process SFTP server, and
//! `apps/drivemount/tests/real_drive.rs` on a real drive over OpenSSH (M2-09).

#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, FlushViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

/// `fsx`'s defaults for the longest file and the longest operation.
pub const MAX_FILE: u64 = 256 * 1024;
pub const MAX_OP: u64 = 64 * 1024;

/// How often the file is closed and opened again, on average, in operations.
const REOPEN_EVERY: u64 = 64;

/// How often the whole file is read through a new handle and handed to the server check.
const WHOLE_FILE_EVERY: u64 = 1024;

pub const DEFAULT_OPS: u64 = 10_000;
pub const DEFAULT_SEED: u64 = 0x5EED_0FF5;

/// xorshift64*: good enough to pick operations, and the same on every machine.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number from 0 up to, not including, `limit`.
    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit.max(1)
    }
}

/// A number from the environment, or `default`.
pub fn from_env(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |text| {
        text.parse().unwrap_or_else(|_| panic!("{name}: {text}"))
    })
}

fn open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

/// Where a file and its copy first differ, with the operations leading up to it.
pub fn compare(what: &str, got: &[u8], expected: &[u8], offset: u64, log: &[String]) {
    if got == expected {
        return;
    }
    let first = got.iter().zip(expected).position(|(a, b)| a != b);
    panic!(
        "{what} at {offset}: {} bytes where {} were expected, first different at {first:?}\nthe last operations:\n{}",
        got.len(),
        expected.len(),
        log.join("\n"),
    );
}

/// Runs `ops` operations on a new file at `path`, and returns what it should hold.
///
/// Every [`WHOLE_FILE_EVERY`] operations, and at the end with the file closed, `server` is called with
/// what the file should hold and the last operations: it checks the file as another client would see
/// it, and panics with [`compare`] if it differs.
pub fn run(path: &Path, ops: u64, seed: u64, server: &mut dyn FnMut(&[u8], &[String])) -> Vec<u8> {
    let mut random = Random(seed | 1);
    let mut copy: Vec<u8> = Vec::new();
    let mut log: Vec<String> = Vec::new();
    File::create(path).unwrap();
    let mut file = open(path);

    for number in 0..ops {
        let size = copy.len() as u64;
        let operation = random.below(100);
        if log.len() == 32 {
            log.remove(0);
        }
        match operation {
            // Reads.
            0..30 => {
                let offset = random.below(size);
                let len = random.below(MAX_OP.min(size - offset) + 1);
                log.push(format!("{number}: read {offset} + {len}"));
                let mut got = vec![0; len as usize];
                file.seek(SeekFrom::Start(offset)).unwrap();
                file.read_exact(&mut got).unwrap();
                let range = offset as usize..(offset + len) as usize;
                compare("read", &got, &copy[range], offset, &log);
            }
            // Writes, which can leave a gap or extend the file.
            30..60 => {
                let offset = random.below(MAX_FILE);
                let len = 1 + random.below(MAX_OP.min(MAX_FILE - offset));
                log.push(format!("{number}: write {offset} + {len}"));
                let start = random.next() as u8;
                let data: Vec<u8> = (0..len)
                    .map(|index| start.wrapping_add((index / 3) as u8) ^ (index >> 8) as u8)
                    .collect();
                file.seek(SeekFrom::Start(offset)).unwrap();
                file.write_all(&data).unwrap();
                let end = (offset + len) as usize;
                if copy.len() < end {
                    copy.resize(end, 0);
                }
                copy[offset as usize..end].copy_from_slice(&data);
                assert_eq!(
                    file.metadata().unwrap().len(),
                    copy.len() as u64,
                    "{number}"
                );
            }
            // Truncates, down and up.
            60..72 => {
                let new_size = random.below(MAX_FILE + 1);
                log.push(format!("{number}: truncate {size} to {new_size}"));
                file.set_len(new_size).unwrap();
                copy.resize(new_size as usize, 0);
                assert_eq!(file.metadata().unwrap().len(), new_size, "{number}");
            }
            // Mapped reads.
            72..80 if size > 0 => {
                let offset = random.below(size);
                let len = 1 + random.below(MAX_OP.min(size - offset));
                log.push(format!("{number}: mapped read {offset} + {len}"));
                let mut view = MappedFile::new(&file, size as usize);
                let range = offset as usize..(offset + len) as usize;
                compare(
                    "mapped read",
                    &view.bytes()[range.clone()],
                    &copy[range],
                    offset,
                    &log,
                );
            }
            // Mapped writes, which do not change the size.
            80..88 if size > 0 => {
                let offset = random.below(size);
                let len = 1 + random.below(MAX_OP.min(size - offset));
                log.push(format!("{number}: mapped write {offset} + {len}"));
                let start = random.next() as u8;
                let data: Vec<u8> = (0..len)
                    .map(|index| start.wrapping_add(index as u8))
                    .collect();
                let range = offset as usize..(offset + len) as usize;
                let mut view = MappedFile::new(&file, size as usize);
                view.bytes()[range.clone()].copy_from_slice(&data);
                view.flush();
                drop(view);
                copy[range].copy_from_slice(&data);
            }
            // Flushes.
            88..92 => {
                log.push(format!("{number}: flush"));
                file.sync_all().unwrap();
            }
            // The size and the end of the file.
            _ => {
                log.push(format!("{number}: size"));
                assert_eq!(file.metadata().unwrap().len(), size, "{number}");
                file.seek(SeekFrom::Start(size)).unwrap();
                assert_eq!(file.read(&mut [0; 16]).unwrap(), 0, "{number}");
            }
        }

        if random.below(REOPEN_EVERY) == 0 {
            log.push(format!("{number}: reopen"));
            drop(file);
            file = open(path);
        }

        // Now and then, the whole file through a new handle, and as another client would see it.
        if number % WHOLE_FILE_EVERY == WHOLE_FILE_EVERY - 1 {
            let mut other = File::open(path).unwrap();
            let mut got = Vec::new();
            other.read_to_end(&mut got).unwrap();
            compare("whole file", &got, &copy, 0, &log);
            server(&copy, &log);
        }
    }

    drop(file);
    log.push("the end, closed".to_owned());
    server(&copy, &log);
    copy
}

/// A view of an open file in memory, as far as the size given.
pub struct MappedFile {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl MappedFile {
    /// `file` needs to be open for reading and writing, and at least `len` bytes long.
    pub fn new(file: &File, len: usize) -> Self {
        use std::os::windows::io::AsRawHandle;
        let handle = HANDLE(file.as_raw_handle());
        // SAFETY: the file's handle is open. A mapping keeps what it needs of it.
        let mapping =
            unsafe { CreateFileMappingW(handle, None, PAGE_READWRITE, 0, 0, PCWSTR::null()) }
                .unwrap();
        // SAFETY: the mapping was just made, and is for at least `len` bytes.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, len) };
        assert!(!view.Value.is_null());
        Self { mapping, view, len }
    }

    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view is `len` bytes, readable and writable, until drop, and borrowed from here.
        unsafe { std::slice::from_raw_parts_mut(self.view.Value.cast(), self.len) }
    }

    /// Sends what was changed to the file. Windows only promises it for "soon" otherwise.
    pub fn flush(&self) {
        // SAFETY: the view is `len` bytes.
        unsafe { FlushViewOfFile(self.view.Value, self.len) }.unwrap();
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: both are open, and `bytes` cannot be borrowed any more.
        unsafe {
            UnmapViewOfFile(self.view).unwrap();
            CloseHandle(self.mapping).unwrap();
        }
    }
}
