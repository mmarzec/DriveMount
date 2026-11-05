// SPDX-License-Identifier: GPL-3.0-or-later

//! An `fsx`-style exerciser through a mounted drive (M1-12b, NFR-REL-01).
//!
//! `fsx` has no ready Windows build, so this does what it does through Win32: random reads, writes,
//! truncates, mapped reads and writes and reopens on one file, with a copy kept in memory that every
//! read is checked against. `FSX_OPS` and `FSX_SEED` set the length and the seed of a run, which is
//! how M2-09's longer runs and a failure can be repeated.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use support::{Drive, MappedFile};

/// `fsx`'s defaults for the longest file and the longest operation.
const MAX_FILE: u64 = 256 * 1024;
const MAX_OP: u64 = 64 * 1024;

/// How often the file is closed and opened again, on average, in operations.
const REOPEN_EVERY: u64 = 64;

const DEFAULT_OPS: u64 = 10_000;
const DEFAULT_SEED: u64 = 0x5EED_0FF5;

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

fn from_env(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |text| {
        text.parse().unwrap_or_else(|_| panic!("{name}: {text}"))
    })
}

fn open(drive: &Drive) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(drive.path("fsx"))
        .unwrap()
}

/// Where a file and its copy first differ, with the operations leading up to it.
fn compare(what: &str, got: &[u8], expected: &[u8], offset: u64, log: &[String]) {
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

fn run(ops: u64, seed: u64) {
    let drive = Drive::new();
    let mut random = Random(seed | 1);
    let mut copy: Vec<u8> = Vec::new();
    let mut log: Vec<String> = Vec::new();
    File::create(drive.path("fsx")).unwrap();
    let mut file = open(&drive);

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
            file = open(&drive);
        }

        // Now and then, the whole file through a new handle, and as another client would see it.
        if number % 1024 == 1023 {
            let mut other = File::open(drive.path("fsx")).unwrap();
            let mut got = Vec::new();
            other.read_to_end(&mut got).unwrap();
            compare("whole file", &got, &copy, 0, &log);
            let server = drive.get("/fsx");
            compare("the server's file", &server, &copy, 0, &log);
        }
    }

    drop(file);
    let server = drive.get("/fsx");
    compare("the server's file at the end", &server, &copy, 0, &log);
}

#[test]
fn ten_thousand_operations_lose_nothing() {
    let ops = from_env("FSX_OPS", DEFAULT_OPS);
    let seed = from_env("FSX_SEED", DEFAULT_SEED);
    run(ops, seed);
}

/// Another seed, as one exercises only so many orders of things.
#[test]
fn another_seed_loses_nothing_either() {
    run(from_env("FSX_OPS", DEFAULT_OPS) / 10, 0xB0A7_F00D);
}
