// SPDX-License-Identifier: GPL-3.0-or-later

//! An `fsx`-style exerciser through a mounted drive (M1-12b, NFR-REL-01).
//!
//! `fsx` has no ready Windows build, so this does what it does through Win32 (`support/exerciser.rs`):
//! random reads, writes, truncates, mapped reads and writes and reopens on one file, with a copy kept in
//! memory that every read is checked against, and the server's file checked now and then. `FSX_OPS`
//! and `FSX_SEED` set the length and the seed of a run, which is how M2-09's longer runs and a failure
//! can be repeated. The same over a real OpenSSH server is in `apps/drivemount/tests/real_drive.rs`.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use support::Drive;
use support::exerciser::{self, DEFAULT_OPS, DEFAULT_SEED, compare, from_env};

fn run(ops: u64, seed: u64) {
    let drive = Drive::new();
    exerciser::run(&drive.path("fsx"), ops, seed, &mut |copy, log| {
        compare("the server's file", &drive.get("/fsx"), copy, 0, log);
    });
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
