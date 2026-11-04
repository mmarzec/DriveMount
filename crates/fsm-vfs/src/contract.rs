// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`RemoteFs`] contract suite (spec §10.2): one generic suite that every backend must pass,
//! holding it to what the trait's doc comments promise.
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! use fsm_vfs::{MemFs, RemotePath, contract};
//!
//! contract::run_quick(&MemFs::new(), &RemotePath::root()).await;
//! # }
//! ```
//!
//! A broken promise panics, so a call is one test. Each case prints its name first, which the test
//! harness shows when the test fails. A failed run leaves its files behind for inspection.
//!
//! The server under test must allow any POSIX name, including backslashes and names differing only
//! by case, and the user must not be root, who can write to read-only files. That rules out Windows
//! servers, which the lab matrix (spec §10.4) covers instead.
//!
//! Symbolic links are not covered: [`RemoteFs`] cannot create them and the suite has no other way
//! to. Nor are reconnect and deadlines, which belong to other layers (M1-06, M2-01).

// The whole module is test code that fails by panicking. Being behind a feature rather than
// cfg(test), it does not get the exemption that clippy.toml gives tests.
#![allow(clippy::expect_used)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::{
    Attr, AttrChanges, AttrTarget, Disposition, FileKind, FsError, FsResult, FsStats, OpenMode,
    RemoteFs, RemoteHandle, RemotePath, apply_read_only,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Runs every case, including the 1 GiB file.
///
/// `root` must be an existing, writable directory. The suite works in a new directory inside it and
/// removes that again if everything passes.
///
/// # Panics
///
/// If the backend breaks the contract.
pub async fn run_all<F: RemoteFs + ?Sized>(fs: &F, root: &RemotePath) {
    let scratch = make_scratch(fs, root).await;
    run_cases(fs, &scratch).await;
    let case = Case::new(fs, &scratch, "large_file").await;
    large_file(&case).await;
    remove_scratch(fs, root, &scratch).await;
}

/// Runs every case except the 1 GiB file, for extra configurations of a backend whose full run is
/// covered elsewhere. See [`run_all`].
///
/// # Panics
///
/// If the backend breaks the contract.
pub async fn run_quick<F: RemoteFs + ?Sized>(fs: &F, root: &RemotePath) {
    let scratch = make_scratch(fs, root).await;
    run_cases(fs, &scratch).await;
    remove_scratch(fs, root, &scratch).await;
}

async fn run_cases<F: RemoteFs + ?Sized>(fs: &F, scratch: &RemotePath) {
    macro_rules! run {
        ($($case:ident),* $(,)?) => {
            $($case(&Case::new(fs, scratch, stringify!($case)).await).await;)*
        };
    }
    run!(
        capabilities,
        stat_agrees_on_files_and_directories,
        missing_paths,
        open_existing,
        create_new,
        open_or_create,
        truncate_existing,
        create_or_truncate,
        sizes,
        write_at_offsets,
        read_at_offsets,
        gaps_read_as_zeros,
        offsets_beyond_four_gib,
        handles_see_each_others_writes,
        handle_access_is_enforced,
        flush_and_fstat,
        truncate_by_path,
        truncate_by_handle,
        rename_without_replace,
        rename_with_replace,
        rename_onto_itself,
        rename_moves_between_directories,
        rename_a_directory,
        failed_rename_changes_nothing,
        mkdir,
        remove_dir,
        remove_file,
        read_dir,
        names,
        set_times,
        set_times_by_handle,
        set_nothing,
        read_only_bit,
        read_only_bit_by_handle,
        statfs,
        overlapping_calls,
    );
}

/// Unique, so that runs against one server do not meet, including what a failed run left behind.
async fn make_scratch<F: RemoteFs + ?Sized>(fs: &F, root: &RemotePath) -> RemotePath {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!("fsm-contract-{}-{nanos}", std::process::id());
    let scratch = root.join(&name).expect("a valid name");
    fs.mkdir(&scratch)
        .await
        .expect("create the scratch directory");
    scratch
}

async fn remove_scratch<F: RemoteFs + ?Sized>(fs: &F, root: &RemotePath, scratch: &RemotePath) {
    remove_tree(fs, scratch).await;
    let name = scratch.file_name().expect("not the root");
    let left = fs.read_dir(root).await.expect("list the root");
    assert!(
        left.iter().all(|entry| entry.name != name),
        "the scratch directory is still listed after removal"
    );
}

async fn remove_tree<F: RemoteFs + ?Sized>(fs: &F, dir: &RemotePath) {
    // A stack rather than recursion, which async fn cannot do without boxing.
    let mut pending = vec![dir.clone()];
    let mut dirs = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs.read_dir(&dir).await.expect("list for cleanup") {
            let path = dir.join(&entry.name).expect("a valid name");
            if entry.attr.is_dir() {
                pending.push(path);
            } else {
                fs.remove_file(&path).await.expect("remove for cleanup");
            }
        }
        dirs.push(dir);
    }
    for dir in dirs.iter().rev() {
        fs.remove_dir(dir).await.expect("remove for cleanup");
    }
}

/// One case's own directory, and helpers that work within the session's limits.
struct Case<'a, F: ?Sized> {
    fs: &'a F,
    dir: RemotePath,
}

impl<'a, F: RemoteFs + ?Sized> Case<'a, F> {
    async fn new(fs: &'a F, scratch: &RemotePath, name: &str) -> Self {
        // Printed for the test harness to show if this case panics.
        println!("contract case: {name}");
        let dir = scratch.join(name).expect("a valid name");
        fs.mkdir(&dir).await.expect("create the case's directory");
        Self { fs, dir }
    }

    fn path(&self, name: &str) -> RemotePath {
        self.dir.join(name).expect("a valid name")
    }

    /// Creates a file with these contents.
    async fn file(&self, name: &str, data: &[u8]) -> RemotePath {
        let path = self.path(name);
        let handle = self.open(&path, CREATE_RW).await;
        self.write_at(&handle, 0, data).await;
        self.close(handle).await;
        path
    }

    async fn dir(&self, name: &str) -> RemotePath {
        let path = self.path(name);
        self.fs.mkdir(&path).await.expect("mkdir");
        path
    }

    async fn open(&self, path: &RemotePath, mode: OpenMode) -> RemoteHandle {
        self.fs.open(path, mode).await.expect("open")
    }

    async fn close(&self, handle: RemoteHandle) {
        self.fs.close(handle).await.expect("close");
    }

    async fn stat(&self, path: &RemotePath) -> Attr {
        self.fs.stat(path).await.expect("stat")
    }

    /// Writes in requests the session allows.
    async fn write_at(&self, handle: &RemoteHandle, offset: u64, data: &[u8]) {
        let limit = self.fs.capabilities().max_write as usize;
        let mut position = offset;
        for chunk in data.chunks(limit) {
            self.fs
                .write(handle, position, Bytes::copy_from_slice(chunk))
                .await
                .expect("write");
            position += chunk.len() as u64;
        }
    }

    /// Reads until it has `len` bytes or reaches the end of the file, allowing for short reads.
    async fn read_at(&self, handle: &RemoteHandle, offset: u64, len: u64) -> Vec<u8> {
        let limit = u64::from(self.fs.capabilities().max_read);
        let mut out = Vec::new();
        while (out.len() as u64) < len {
            let ask = (len - out.len() as u64).min(limit);
            let got = self
                .fs
                .read(handle, offset + out.len() as u64, ask as u32)
                .await
                .expect("read");
            assert!(got.len() as u64 <= ask, "read returned more than asked for");
            if got.is_empty() {
                break;
            }
            out.extend_from_slice(&got);
        }
        out
    }

    async fn contents(&self, path: &RemotePath) -> Vec<u8> {
        let handle = self.open(path, OpenMode::read()).await;
        let data = self.read_at(&handle, 0, u64::MAX).await;
        self.close(handle).await;
        data
    }

    async fn listing(&self, path: &RemotePath) -> Vec<String> {
        let entries = self.fs.read_dir(path).await.expect("read_dir");
        let mut names: Vec<_> = entries.into_iter().map(|entry| entry.name).collect();
        names.sort();
        names
    }
}

const CREATE_RW: OpenMode = OpenMode::read_write(Disposition::CreateNew);
const OPEN_RW: OpenMode = OpenMode::read_write(Disposition::OpenExisting);

fn is_missing<T>(result: &FsResult<T>) -> bool {
    matches!(result, Err(FsError::NotFound | FsError::PathNotFound))
}

/// Compares file contents without printing megabytes of them.
#[track_caller]
fn assert_same(actual: &[u8], expected: &[u8], what: &str) {
    if actual == expected {
        return;
    }
    let first = actual.iter().zip(expected).position(|(a, e)| a != e);
    panic!(
        "{what}: got {} bytes, expected {}, first difference at {first:?}",
        actual.len(),
        expected.len()
    );
}

/// Bytes that differ from one position to the next, so misplaced data does not pass by accident.
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = vec![0; len];
    for chunk in out.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    out
}

fn time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

// Whole seconds within 32 bits, which SFTP v3 can store.
const TIME_A: u64 = 1_600_000_000;
const TIME_B: u64 = 1_500_000_000;
const TIME_C: u64 = 1_400_000_000;

async fn capabilities<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let capabilities = c.fs.capabilities();
    assert!(capabilities.max_read > 0);
    assert!(capabilities.max_write > 0);
    assert_eq!(c.fs.capabilities(), capabilities, "fixed for the session");
}

async fn stat_agrees_on_files_and_directories<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"abc").await;
    let attr = c.stat(&file).await;
    assert_eq!(attr.kind, FileKind::File);
    assert!(!attr.is_dir());
    assert_eq!(attr.size, 3);
    assert_eq!(c.fs.lstat(&file).await, Ok(attr));

    let handle = c.open(&file, OpenMode::read()).await;
    assert_eq!(c.fs.fstat(&handle).await, Ok(attr));
    c.close(handle).await;

    let dir = c.dir("dir").await;
    let attr = c.stat(&dir).await;
    assert_eq!(attr.kind, FileKind::Dir);
    assert!(attr.is_dir());
    assert_eq!(c.fs.lstat(&dir).await, Ok(attr));
}

async fn missing_paths<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"").await;
    let absent = c.path("absent");
    let under_absent = c.path("absent/child");
    let under_file = file.join("child").expect("a valid name");

    assert_eq!(c.fs.stat(&absent).await, Err(FsError::NotFound));
    assert_eq!(c.fs.lstat(&absent).await, Err(FsError::NotFound));
    assert!(is_missing(&c.fs.read_dir(&absent).await));
    assert!(is_missing(&c.fs.stat(&under_absent).await));
    assert!(is_missing(&c.fs.stat(&under_file).await));
    assert!(is_missing(&c.fs.lstat(&under_absent).await));
    assert!(is_missing(&c.fs.open(&under_absent, CREATE_RW).await));
    assert!(is_missing(&c.fs.mkdir(&under_absent).await));
    assert!(is_missing(
        &c.fs
            .set_attr(AttrTarget::Path(&absent), AttrChanges::default())
            .await
    ));
}

async fn open_existing<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let absent = c.path("absent");
    assert_eq!(
        c.fs.open(&absent, OpenMode::read()).await,
        Err(FsError::NotFound)
    );
    assert_eq!(c.fs.open(&absent, OPEN_RW).await, Err(FsError::NotFound));
    assert_eq!(
        c.fs.stat(&absent).await,
        Err(FsError::NotFound),
        "must not create the file"
    );

    let file = c.file("file", b"keep").await;
    let handle = c.open(&file, OPEN_RW).await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 4);
    c.close(handle).await;
    assert_same(&c.contents(&file).await, b"keep", "opening must keep data");
}

async fn create_new<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.path("file");
    let handle = c.open(&file, CREATE_RW).await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 0);
    c.write_at(&handle, 0, b"data").await;
    c.close(handle).await;

    for mode in [CREATE_RW, OpenMode::write(Disposition::CreateNew)] {
        assert_eq!(c.fs.open(&file, mode).await, Err(FsError::Exists));
    }
    assert_same(&c.contents(&file).await, b"data", "the file must survive");

    let dir = c.dir("dir").await;
    assert_eq!(c.fs.open(&dir, CREATE_RW).await, Err(FsError::Exists));

    let new = c.stat(&c.file("new", b"").await).await;
    assert!(!new.is_read_only(), "a new file must be writable");
}

async fn open_or_create<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let mode = OpenMode::read_write(Disposition::OpenOrCreate);
    let file = c.path("file");

    let handle = c.open(&file, mode).await;
    c.write_at(&handle, 0, b"first").await;
    c.close(handle).await;

    let handle = c.open(&file, mode).await;
    assert_same(&c.read_at(&handle, 0, 5).await, b"first", "must keep data");
    c.close(handle).await;
}

async fn truncate_existing<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let mode = OpenMode::write(Disposition::TruncateExisting);
    let absent = c.path("absent");
    assert_eq!(c.fs.open(&absent, mode).await, Err(FsError::NotFound));
    assert_eq!(c.fs.stat(&absent).await, Err(FsError::NotFound));

    let file = c.file("file", b"old contents").await;
    let handle = c.open(&file, mode).await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 0);
    c.write_at(&handle, 0, b"new").await;
    c.close(handle).await;
    assert_same(&c.contents(&file).await, b"new", "after truncating");
}

async fn create_or_truncate<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let mode = OpenMode::read_write(Disposition::CreateOrTruncate);
    let file = c.path("file");

    let handle = c.open(&file, mode).await;
    c.write_at(&handle, 0, b"old contents").await;
    c.close(handle).await;

    let handle = c.open(&file, mode).await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 0);
    assert_same(&c.read_at(&handle, 0, 16).await, b"", "after truncating");
    c.close(handle).await;
}

/// 0 B and up, around the page sizes and the session's request limits, where splitting goes wrong.
async fn sizes<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let capabilities = c.fs.capabilities();
    let max_read = u64::from(capabilities.max_read);
    let max_write = u64::from(capabilities.max_write);
    let sizes = [
        0,
        1,
        2,
        4 * KIB - 1,
        4 * KIB,
        4 * KIB + 1,
        64 * KIB - 1,
        64 * KIB,
        64 * KIB + 1,
        max_read - 1,
        max_read,
        max_read + 1,
        max_write - 1,
        max_write,
        max_write + 1,
        2 * max_write + 1,
        MIB,
        MIB + 1,
        16 * MIB + 1,
    ];
    let largest = sizes.iter().copied().max().unwrap_or(0);
    let data = pattern(1, largest as usize);
    for (index, &size) in sizes.iter().enumerate() {
        let expected = &data[..size as usize];
        let file = c.file(&format!("size-{index}"), expected).await;
        assert_eq!(c.stat(&file).await.size, size);
        assert_same(&c.contents(&file).await, expected, "after a round trip");
        c.fs.remove_file(&file).await.expect("remove");
    }
}

async fn write_at_offsets<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"0123456789").await;
    let handle = c.open(&file, OPEN_RW).await;

    c.write_at(&handle, 3, b"abc").await;
    assert_same(&c.read_at(&handle, 0, 64).await, b"012abc6789", "middle");
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 10);

    c.write_at(&handle, 0, b"X").await;
    assert_same(&c.read_at(&handle, 0, 64).await, b"X12abc6789", "start");

    c.write_at(&handle, 8, b"YZW").await;
    assert_same(
        &c.read_at(&handle, 0, 64).await,
        b"X12abc67YZW",
        "over the end",
    );
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 11);

    c.write_at(&handle, 11, b"!").await;
    assert_same(
        &c.read_at(&handle, 0, 64).await,
        b"X12abc67YZW!",
        "at the end",
    );

    // Writing nothing must not extend the file.
    c.fs.write(&handle, 100, Bytes::new()).await.expect("write");
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 12);
    c.close(handle).await;
}

async fn read_at_offsets<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let data = pattern(2, 300 * KIB as usize);
    let file = c.file("file", &data).await;
    let handle = c.open(&file, OpenMode::read()).await;

    for (offset, len) in [
        (0, 1),
        (1, 1),
        (4095, 2),
        (64 * KIB - 1, 2),
        (64 * KIB, 64 * KIB),
        (100_000, 150_000),
    ] {
        let start = offset as usize;
        let end = start + len as usize;
        let got = c.read_at(&handle, offset, len).await;
        assert_same(&got, &data[start..end], "a read in the middle");
    }

    let size = data.len() as u64;
    let tail = c.read_at(&handle, size - 3, 10).await;
    assert_same(&tail, &data[data.len() - 3..], "a read over the end");
    for offset in [size, size + 1, size + GIB] {
        let got =
            c.fs.read(&handle, offset, 16)
                .await
                .expect("read past the end");
        assert!(got.is_empty(), "a read at or past the end must be empty");
    }
    c.close(handle).await;

    let empty = c.file("empty", b"").await;
    let handle = c.open(&empty, OpenMode::read()).await;
    let got = c.fs.read(&handle, 0, 16).await.expect("read an empty file");
    assert!(got.is_empty());
    c.close(handle).await;
}

async fn gaps_read_as_zeros<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"start").await;
    let handle = c.open(&file, OPEN_RW).await;
    let gap_end = 200 * KIB;
    c.write_at(&handle, gap_end, b"end").await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, gap_end + 3);

    let mut expected = vec![0; gap_end as usize];
    expected[..5].copy_from_slice(b"start");
    expected.extend_from_slice(b"end");
    let got = c.read_at(&handle, 0, u64::MAX).await;
    assert_same(&got, &expected, "with a gap");
    c.close(handle).await;
}

/// A backend storing an offset in 32 bits would overwrite the start of the file.
async fn offsets_beyond_four_gib<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"start").await;
    let handle = c.open(&file, OPEN_RW).await;
    let offset = 5 * GIB + 7;

    c.write_at(&handle, offset, b"far").await;
    let size = c.fs.fstat(&handle).await.expect("fstat").size;
    assert_eq!(size, offset + 3);
    assert_same(&c.read_at(&handle, offset, 3).await, b"far", "far away");
    assert_same(&c.read_at(&handle, 0, 5).await, b"start", "at the start");

    let cut = AttrChanges {
        size: Some(4 * GIB + 1),
        ..AttrChanges::default()
    };
    let attr = c.fs.set_attr(AttrTarget::Handle(&handle), cut).await;
    assert_eq!(attr.expect("set_attr").size, 4 * GIB + 1);
    assert_same(&c.read_at(&handle, 0, 5).await, b"start", "after cutting");
    c.close(handle).await;

    c.fs.remove_file(&file).await.expect("remove");
}

async fn handles_see_each_others_writes<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"").await;
    let writer = c
        .open(&file, OpenMode::write(Disposition::OpenExisting))
        .await;
    let reader = c.open(&file, OpenMode::read()).await;

    c.write_at(&writer, 0, b"visible").await;
    assert_same(&c.read_at(&reader, 0, 16).await, b"visible", "other handle");
    assert_eq!(c.fs.fstat(&reader).await.expect("fstat").size, 7);
    assert_eq!(c.stat(&file).await.size, 7, "by path");

    c.close(reader).await;
    c.write_at(&writer, 7, b"!").await;
    c.close(writer).await;
    assert_same(&c.contents(&file).await, b"visible!", "after closing one");
}

async fn handle_access_is_enforced<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;

    let reader = c.open(&file, OpenMode::read()).await;
    let other = c.open(&file, OpenMode::read()).await;
    assert_ne!(reader.id(), other.id(), "each open gets its own handle");
    c.close(other).await;
    let write = c.fs.write(&reader, 0, Bytes::from_static(b"x")).await;
    assert!(write.is_err(), "write on a read-only handle");
    c.close(reader).await;
    assert_same(&c.contents(&file).await, b"data", "after the failed write");

    let writer = c
        .open(&file, OpenMode::write(Disposition::OpenExisting))
        .await;
    assert!(
        c.fs.read(&writer, 0, 4).await.is_err(),
        "read on a write-only handle"
    );
    c.close(writer).await;

    let both = c.open(&file, OPEN_RW).await;
    c.write_at(&both, 0, b"DATA").await;
    assert_same(
        &c.read_at(&both, 0, 4).await,
        b"DATA",
        "on a read-write handle",
    );
    c.close(both).await;
}

async fn flush_and_fstat<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.path("file");
    let handle = c.open(&file, CREATE_RW).await;
    c.fs.flush(&handle)
        .await
        .expect("flush with nothing written");
    c.write_at(&handle, 0, b"abc").await;
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 3);
    c.fs.flush(&handle).await.expect("flush");
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 3);
    c.close(handle).await;

    let reader = c.open(&file, OpenMode::read()).await;
    c.fs.flush(&reader).await.expect("flush on a read handle");
    c.close(reader).await;
}

/// The change to `size` alone.
fn resize(size: u64) -> AttrChanges {
    AttrChanges {
        size: Some(size),
        ..AttrChanges::default()
    }
}

async fn truncate_by_path<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let data = pattern(3, 200 * KIB as usize);
    let file = c.file("file", &data).await;
    let target = AttrTarget::Path(&file);

    // Shrink, then grow: the old bytes must not come back.
    let attr = c.fs.set_attr(target, resize(10)).await.expect("shrink");
    assert_eq!(attr.size, 10);
    assert_same(&c.contents(&file).await, &data[..10], "after shrinking");

    let attr =
        c.fs.set_attr(target, resize(150 * KIB))
            .await
            .expect("grow");
    assert_eq!(attr.size, 150 * KIB);
    assert_eq!(c.stat(&file).await.size, 150 * KIB);
    let mut expected = data[..10].to_vec();
    expected.resize(150 * KIB as usize, 0);
    assert_same(&c.contents(&file).await, &expected, "after growing");

    let attr =
        c.fs.set_attr(target, resize(150 * KIB))
            .await
            .expect("same");
    assert_eq!(attr.size, 150 * KIB);

    let attr = c.fs.set_attr(target, resize(0)).await.expect("to zero");
    assert_eq!(attr.size, 0);
    assert_same(&c.contents(&file).await, b"", "after emptying");
}

async fn truncate_by_handle<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"0123456789").await;
    let handle = c.open(&file, OPEN_RW).await;
    let target = AttrTarget::Handle(&handle);

    let attr = c.fs.set_attr(target, resize(4)).await.expect("shrink");
    assert_eq!(attr.size, 4);
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, 4);
    assert_same(&c.read_at(&handle, 0, 64).await, b"0123", "after shrinking");

    // Writing after shrinking leaves a gap of zeros, not the old bytes.
    c.write_at(&handle, 6, b"x").await;
    let got = c.read_at(&handle, 0, 64).await;
    assert_same(&got, b"0123\0\0x", "after writing past the new end");
    c.close(handle).await;
}

async fn rename_without_replace<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let from = c.file("from", b"source").await;
    let to = c.file("to", b"target").await;
    assert_eq!(c.fs.rename(&from, &to, false).await, Err(FsError::Exists));
    assert_same(&c.contents(&from).await, b"source", "source must stay");
    assert_same(&c.contents(&to).await, b"target", "target must stay");

    let free = c.path("free");
    c.fs.rename(&from, &free, false).await.expect("rename");
    assert_eq!(c.fs.stat(&from).await, Err(FsError::NotFound));
    assert_same(&c.contents(&free).await, b"source", "at the new name");
}

async fn rename_with_replace<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let from = c.file("from", b"source").await;
    let to = c.file("to", b"longer target").await;
    c.fs.rename(&from, &to, true).await.expect("rename");
    assert_eq!(c.fs.stat(&from).await, Err(FsError::NotFound));
    assert_same(&c.contents(&to).await, b"source", "the replaced file");
    assert_eq!(c.listing(&c.dir).await, ["to"]);

    // Without a target, replace changes nothing.
    let free = c.path("free");
    c.fs.rename(&to, &free, true).await.expect("rename");
    assert_same(&c.contents(&free).await, b"source", "at the new name");
    assert_eq!(c.fs.stat(&to).await, Err(FsError::NotFound));
}

/// A rename that removes the target first would destroy the only copy here.
async fn rename_onto_itself<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    c.fs.rename(&file, &file, true).await.expect("rename");
    assert_same(&c.contents(&file).await, b"data", "with replace");
    assert_eq!(c.fs.rename(&file, &file, false).await, Err(FsError::Exists));
    assert_same(&c.contents(&file).await, b"data", "without replace");
}

async fn rename_moves_between_directories<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let a = c.dir("a").await;
    let b = c.dir("b").await;
    let from = c.file("a/file", b"data").await;
    let to = b.join("moved").expect("a valid name");

    c.fs.rename(&from, &to, false).await.expect("rename");
    assert!(c.listing(&a).await.is_empty());
    assert_eq!(c.listing(&b).await, ["moved"]);
    assert_same(&c.contents(&to).await, b"data", "after moving");
}

async fn rename_a_directory<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let from = c.dir("from").await;
    c.dir("from/sub").await;
    c.file("from/sub/file", b"deep").await;
    let to = c.path("to");

    c.fs.rename(&from, &to, false).await.expect("rename");
    assert_eq!(c.fs.stat(&from).await, Err(FsError::NotFound));
    assert!(c.stat(&to).await.is_dir());
    let moved = to.join("sub/file").expect("a valid name");
    assert_same(&c.contents(&moved).await, b"deep", "inside the moved tree");
}

async fn failed_rename_changes_nothing<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"file data").await;
    let full = c.dir("full").await;
    let inner = c.file("full/inner", b"inner data").await;
    let empty = c.dir("empty").await;
    let absent = c.path("absent");

    // A missing source must not cost the target its data.
    assert_eq!(
        c.fs.rename(&absent, &file, true).await,
        Err(FsError::NotFound)
    );
    assert_eq!(
        c.fs.rename(&absent, &file, false).await,
        Err(FsError::NotFound)
    );
    let no_parent = c.path("no/parent");
    assert!(is_missing(&c.fs.rename(&file, &no_parent, true).await));

    let cases = [
        (&file, &full, "a file onto a non-empty directory"),
        (&file, &empty, "a file onto a directory"),
        (&empty, &file, "a directory onto a file"),
        (&empty, &full, "a directory onto a non-empty directory"),
    ];
    for (from, to, what) in cases {
        assert!(c.fs.rename(from, to, true).await.is_err(), "{what}");
    }
    let inside = full.join("sub").expect("a valid name");
    let result = c.fs.rename(&full, &inside, false).await;
    assert!(result.is_err(), "a directory into itself");

    assert_same(&c.contents(&file).await, b"file data", "the file");
    assert_same(&c.contents(&inner).await, b"inner data", "the directory");
    assert!(c.listing(&empty).await.is_empty());
    assert_eq!(c.listing(&c.dir).await, ["empty", "file", "full"]);
}

async fn mkdir<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let dir = c.path("dir");
    let attr = c.fs.mkdir(&dir).await.expect("mkdir");
    assert!(attr.is_dir());
    assert!(c.stat(&dir).await.is_dir());
    assert!(c.listing(&dir).await.is_empty());

    assert_eq!(c.fs.mkdir(&dir).await, Err(FsError::Exists));
    let file = c.file("file", b"data").await;
    assert_eq!(c.fs.mkdir(&file).await, Err(FsError::Exists));
    assert_same(&c.contents(&file).await, b"data", "the file must survive");
}

async fn remove_dir<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let dir = c.dir("dir").await;
    let inner = c.file("dir/inner", b"data").await;

    assert_eq!(c.fs.remove_dir(&dir).await, Err(FsError::NotEmpty));
    assert_same(&c.contents(&inner).await, b"data", "must not be touched");

    assert!(c.fs.remove_dir(&inner).await.is_err(), "rmdir on a file");
    assert_same(&c.contents(&inner).await, b"data", "must not be touched");

    c.fs.remove_file(&inner).await.expect("remove");
    c.fs.remove_dir(&dir).await.expect("rmdir");
    assert_eq!(c.fs.stat(&dir).await, Err(FsError::NotFound));
    assert_eq!(c.fs.remove_dir(&dir).await, Err(FsError::NotFound));

    c.dir("outer").await;
    c.dir("outer/inner").await;
    let outer = c.path("outer");
    assert_eq!(
        c.fs.remove_dir(&outer).await,
        Err(FsError::NotEmpty),
        "holding only a directory"
    );
}

async fn remove_file<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    c.fs.remove_file(&file).await.expect("remove");
    assert_eq!(c.fs.stat(&file).await, Err(FsError::NotFound));
    assert!(c.listing(&c.dir).await.is_empty());
    assert_eq!(c.fs.remove_file(&file).await, Err(FsError::NotFound));

    // The name can be used again, and starts empty.
    let again = c.file("file", b"").await;
    assert_same(
        &c.contents(&again).await,
        b"",
        "a new file of the same name",
    );

    let dir = c.dir("dir").await;
    assert!(
        c.fs.remove_file(&dir).await.is_err(),
        "remove on a directory"
    );
    assert!(c.stat(&dir).await.is_dir());
}

async fn read_dir<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let dir = c.dir("dir").await;
    assert!(c.listing(&dir).await.is_empty(), "no . or ..");

    c.file("dir/file", b"abc").await;
    c.dir("dir/sub").await;
    c.file("dir/sub/deeper", b"").await;
    assert_eq!(c.listing(&dir).await, ["file", "sub"], "one level only");

    let entries = c.fs.read_dir(&dir).await.expect("read_dir");
    for entry in &entries {
        let path = dir.join(&entry.name).expect("a valid name");
        let attr = c.stat(&path).await;
        assert_eq!(entry.attr.kind, attr.kind, "{}", entry.name);
        assert_eq!(entry.attr.perm, attr.perm, "{}", entry.name);
        if !attr.is_dir() {
            assert_eq!(entry.attr.size, attr.size, "{}", entry.name);
        }
    }

    // A listing reflects changes made since the last one.
    c.file("dir/new", b"").await;
    c.fs.remove_file(&c.path("dir/file")).await.expect("remove");
    assert_eq!(c.listing(&dir).await, ["new", "sub"]);

    let file = c.path("dir/new");
    assert!(c.fs.read_dir(&file).await.is_err(), "listing a file");

    // More entries than fit in one SFTP READDIR reply.
    let many = c.dir("many").await;
    let mut expected = Vec::new();
    for index in 0..500 {
        let name = format!("entry-with-a-rather-long-name-{index:03}");
        c.file(&format!("many/{name}"), b"").await;
        expected.push(name);
    }
    assert_eq!(c.listing(&many).await, expected);
}

async fn names<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let names = [
        "with space",
        " leading",
        "trailing ",
        "trailing.",
        ".hidden",
        "...",
        "back\\slash",
        "co:lon*?\"<>|",
        "ünï çødé 日本語",
        "percent%20d",
        "CON",
        "Case",
        "case",
    ];
    let mut expected = Vec::new();
    for name in names {
        let path = c.file(name, name.as_bytes()).await;
        assert_same(&c.contents(&path).await, name.as_bytes(), name);
        expected.push(name.to_owned());
    }
    expected.sort();
    assert_eq!(c.listing(&c.dir).await, expected);
}

/// The change to the two times alone.
fn set_both(mtime: u64, atime: u64) -> AttrChanges {
    AttrChanges {
        mtime: Some(time(mtime)),
        atime: Some(time(atime)),
        ..AttrChanges::default()
    }
}

async fn set_times<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    let target = AttrTarget::Path(&file);

    let attr = c.fs.set_attr(target, set_both(TIME_A, TIME_B)).await;
    let attr = attr.expect("set both");
    assert_eq!(attr.mtime, Some(time(TIME_A)));
    assert_eq!(attr.atime, Some(time(TIME_B)));
    assert_eq!(c.stat(&file).await, attr);

    // Each alone leaves the other, though SFTP v3 can only send both.
    let mtime_only = AttrChanges {
        mtime: Some(time(TIME_C)),
        ..AttrChanges::default()
    };
    let attr = c.fs.set_attr(target, mtime_only).await.expect("set mtime");
    assert_eq!(attr.mtime, Some(time(TIME_C)));
    assert_eq!(attr.atime, Some(time(TIME_B)));

    let atime_only = AttrChanges {
        atime: Some(time(TIME_A)),
        ..AttrChanges::default()
    };
    let attr = c.fs.set_attr(target, atime_only).await.expect("set atime");
    assert_eq!(attr.mtime, Some(time(TIME_C)));
    assert_eq!(attr.atime, Some(time(TIME_A)));

    assert_eq!(attr.size, 4);
    assert_same(
        &c.contents(&file).await,
        b"data",
        "times must not touch data",
    );

    let dir = c.dir("dir").await;
    let attr =
        c.fs.set_attr(AttrTarget::Path(&dir), set_both(TIME_A, TIME_B))
            .await
            .expect("set times on a directory");
    assert_eq!(attr.mtime, Some(time(TIME_A)));
    assert_eq!(c.stat(&dir).await.mtime, Some(time(TIME_A)));
}

async fn set_times_by_handle<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    let handle = c.open(&file, OPEN_RW).await;

    let attr =
        c.fs.set_attr(AttrTarget::Handle(&handle), set_both(TIME_A, TIME_B))
            .await
            .expect("set times");
    assert_eq!(attr.mtime, Some(time(TIME_A)));
    assert_eq!(attr.atime, Some(time(TIME_B)));
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").mtime, attr.mtime);
    c.close(handle).await;
    assert_eq!(c.stat(&file).await.mtime, Some(time(TIME_A)));
}

async fn set_nothing<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    c.fs.set_attr(AttrTarget::Path(&file), set_both(TIME_A, TIME_B))
        .await
        .expect("set times");
    let before = c.stat(&file).await;

    let after =
        c.fs.set_attr(AttrTarget::Path(&file), AttrChanges::default())
            .await;
    assert_eq!(after, Ok(before));
    assert_eq!(c.stat(&file).await, before);
}

/// The change to the read-only bit alone.
fn read_only(read_only: bool) -> AttrChanges {
    AttrChanges {
        read_only: Some(read_only),
        ..AttrChanges::default()
    }
}

async fn read_only_bit<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    let target = AttrTarget::Path(&file);
    let before = c.stat(&file).await;
    let perm = before.perm.expect("the server must report permissions");
    assert!(!before.is_read_only());

    let attr = c.fs.set_attr(target, read_only(true)).await.expect("set");
    assert!(attr.is_read_only());
    assert_eq!(
        attr.perm,
        Some(apply_read_only(perm, true)),
        "other bits stay"
    );
    assert_eq!(attr.size, before.size);
    assert_eq!(c.stat(&file).await.perm, attr.perm);

    let set_again = c.fs.set_attr(target, read_only(true)).await;
    assert_eq!(set_again.expect("set again").perm, attr.perm);

    for mode in [
        OPEN_RW,
        OpenMode::write(Disposition::OpenExisting),
        OpenMode::write(Disposition::TruncateExisting),
        OpenMode::write(Disposition::CreateOrTruncate),
    ] {
        assert_eq!(
            c.fs.open(&file, mode).await,
            Err(FsError::PermissionDenied),
            "{mode:?}"
        );
    }
    assert_same(&c.contents(&file).await, b"data", "readable and unchanged");

    let attr =
        c.fs.set_attr(target, read_only(false))
            .await
            .expect("clear");
    assert!(!attr.is_read_only());
    assert_eq!(attr.perm, Some(perm), "back where it started");

    let handle = c.open(&file, OPEN_RW).await;
    c.write_at(&handle, 0, b"DATA").await;
    c.close(handle).await;
    assert_same(&c.contents(&file).await, b"DATA", "writable again");

    // Times and the bit together, as SetBasicInfo sends them (spec §5.8).
    let both = AttrChanges {
        read_only: Some(true),
        ..set_both(TIME_A, TIME_B)
    };
    let attr = c.fs.set_attr(target, both).await.expect("set both");
    assert!(attr.is_read_only());
    assert_eq!(attr.mtime, Some(time(TIME_A)));

    let times = c.fs.set_attr(target, set_both(TIME_B, TIME_A)).await;
    assert_eq!(
        times.expect("set times of a read-only file").mtime,
        Some(time(TIME_B))
    );
    assert!(
        c.stat(&file).await.is_read_only(),
        "times must not clear it"
    );

    c.fs.remove_file(&file)
        .await
        .expect("remove a read-only file");
}

async fn read_only_bit_by_handle<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.file("file", b"data").await;
    let perm = c.stat(&file).await.perm.expect("the server must report it");
    let handle = c.open(&file, OPEN_RW).await;
    let target = AttrTarget::Handle(&handle);

    let attr = c.fs.set_attr(target, read_only(true)).await.expect("set");
    assert!(attr.is_read_only());
    assert_eq!(attr.perm, Some(apply_read_only(perm, true)));
    assert!(c.stat(&file).await.is_read_only());

    let attr =
        c.fs.set_attr(target, read_only(false))
            .await
            .expect("clear");
    assert_eq!(attr.perm, Some(perm));
    c.close(handle).await;
}

async fn statfs<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let stats = c.fs.statfs().await.expect("statfs");
    assert!(stats.free_bytes <= stats.total_bytes);
    if c.fs.capabilities().statvfs {
        assert!(stats.total_bytes > 0);
    } else {
        assert_eq!(stats, FsStats::PLACEHOLDER);
    }
}

/// Calls can overlap, including on one handle.
async fn overlapping_calls<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    let file = c.path("file");
    let handle = c.open(&file, CREATE_RW).await;
    let block = 16 * KIB;
    let data = pattern(4, 4 * block as usize);
    let part = |index: u64| {
        let start = (index * block) as usize;
        Bytes::copy_from_slice(&data[start..start + block as usize])
    };

    let (a, b, c0, d, stat) = tokio::join!(
        c.fs.write(&handle, 3 * block, part(3)),
        c.fs.write(&handle, 0, part(0)),
        c.fs.write(&handle, 2 * block, part(2)),
        c.fs.write(&handle, block, part(1)),
        c.fs.fstat(&handle),
    );
    for result in [a, b, c0, d] {
        result.expect("an overlapping write");
    }
    stat.expect("an overlapping fstat");

    let (a, b, c0, d) = tokio::join!(
        c.read_at(&handle, 2 * block, block),
        c.read_at(&handle, 0, block),
        c.read_at(&handle, 3 * block, block),
        c.read_at(&handle, block, block),
    );
    let got = [b, d, a, c0].concat();
    assert_same(&got, &data, "after overlapping writes and reads");
    c.close(handle).await;

    // Exclusive create has one winner, however close the race.
    let contested = c.path("contested");
    let (first, second) = tokio::join!(
        c.fs.open(&contested, CREATE_RW),
        c.fs.open(&contested, CREATE_RW),
    );
    let winner = match (first, second) {
        (Ok(handle), Err(FsError::Exists)) | (Err(FsError::Exists), Ok(handle)) => handle,
        other => panic!("expected one winner and one Exists: {other:?}"),
    };
    c.close(winner).await;

    // Directory changes side by side.
    let (dir_a, dir_b, dir_c) = (c.path("dir-a"), c.path("dir-b"), c.path("dir-c"));
    let (a, b, c0) = tokio::join!(c.fs.mkdir(&dir_a), c.fs.mkdir(&dir_b), c.fs.mkdir(&dir_c));
    for result in [a, b, c0] {
        result.expect("an overlapping mkdir");
    }
    let listing = c.listing(&c.dir).await;
    assert_eq!(listing, ["contested", "dir-a", "dir-b", "dir-c", "file"]);
}

/// 1 MiB of a 1 GiB file: a shared pattern with each 4 KiB page stamped with its offset, so that
/// generating 1 GiB stays cheap and a misplaced page still shows.
fn large_block(base: &[u8], index: u64) -> Vec<u8> {
    let mut block = base.to_vec();
    for (page, chunk) in block.chunks_mut(4 * KIB as usize).enumerate() {
        let offset = index * MIB + page as u64 * 4 * KIB;
        chunk[..8].copy_from_slice(&offset.to_le_bytes());
    }
    block
}

async fn large_file<F: RemoteFs + ?Sized>(c: &Case<'_, F>) {
    const SIZE: u64 = GIB;
    let base = pattern(5, MIB as usize);
    let file = c.path("large");

    let handle = c.open(&file, OpenMode::write(Disposition::CreateNew)).await;
    for index in 0..SIZE / MIB {
        c.write_at(&handle, index * MIB, &large_block(&base, index))
            .await;
    }
    c.fs.flush(&handle).await.expect("flush");
    assert_eq!(c.fs.fstat(&handle).await.expect("fstat").size, SIZE);
    c.close(handle).await;
    assert_eq!(c.stat(&file).await.size, SIZE);

    let handle = c.open(&file, OpenMode::read()).await;
    for index in 0..SIZE / MIB {
        let got = c.read_at(&handle, index * MIB, MIB).await;
        assert_same(
            &got,
            &large_block(&base, index),
            "a block of the large file",
        );
    }

    // The join between two blocks, and the end of the file.
    let got = c.read_at(&handle, MIB - 8, 16).await;
    let first = large_block(&base, 0);
    let second = large_block(&base, 1);
    assert_same(&got[..8], &first[first.len() - 8..], "before the join");
    assert_same(&got[8..], &second[..8], "after the join");
    assert_eq!(c.read_at(&handle, SIZE - 1, 16).await.len(), 1);
    assert_eq!(c.read_at(&handle, SIZE, 16).await.len(), 0);
    c.close(handle).await;

    c.fs.remove_file(&file).await.expect("remove");
    assert_eq!(c.fs.stat(&file).await, Err(FsError::NotFound));
}
