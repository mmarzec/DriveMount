# Test timing report, 10 November 2026

Machine: Windows 10, debug builds, the S-03 Docker servers running in WSL2 (`localhost` relay).
Every binary was run with `--include-ignored --test-threads=1`, so each time below is that test's
own cost. Per-test times from parallel runs were wrong by up to 40x: `lifecycle` tests wait on a
global mutex, so a test that costs 1 s showed 110 s.

## Totals by type

| Type | Tests | Time (serial) | Failed |
|---|---:|---:|---:|
| Unit tests (in `src/`) | 448 + 224¹ | 29.9 s + 17.8 s¹ | 0 |
| Integration, default (what CI's `cargo test --workspace` runs) | 406 | 228.8 s | 0 |
| Integration, `#[ignore]` (Docker servers / WinFsp / desktop) | 153 | 383.5 s | 2 |
| Doc tests | 4 | 0.2 s (plus about 45 s to build them) | 0 |
| **Total measured** | **1,235** | **≈ 660 s (11 min)** | **2** |

¹ `drivemount_app` lib (224 UI tests): measured in the parallel run only (17.8 s wall, all passed).

### Skipped (not timed)

| What | Why |
|---|---|
| `apps/drivemount/tests/real_drive.rs` (10 tests, all `#[ignore]`) | **SKIPPED: too long.** In the default parallel run it deadlocked. `a_server_restart_…` restarted the sshd container while `a_gibibyte_…` was copying, and the process (PID 433812) still can't be killed; it will probably take a reboot. Run single-threaded, `a_gibibyte_and_ten_thousand_small_files_go_in_and_out_unchanged` had run for over 30 minutes and was about halfway through its 10,000 files (≈ 4 files/s over the localhost relay). |
| `crates/drivemount-winfsp/tests/winfsp_tests.rs` | **SKIPPED:** needs the external `winfsp-tests` program (`scripts/get-winfsp-tests.ps1`), which isn't downloaded. |
| `sftpfs::the_contract_holds_on_a_server_without_the_extensions` | Before the fix: **killed at the 20-minute limit**. After the fix: 32 s (see below). |

### Failures

`explorer::a_running_explorer_shows_the_name_and_hides_it_within_a_second` and
`explorer::a_name_that_needs_unicode_is_shown_as_it_is` (`#[ignore]`, need a signed-in desktop)
failed in both runs with "Explorer never got there". Not investigated. The unkillable `real_drive`
process, which still owns a drive, may be confusing Explorer, so rerun them after a reboot.

## Time by binary (serial)

| Crate | Binary | Tests | `#[ignore]` | Time |
|---|---|---:|---:|---:|
| drivemount | mount_one | 16 | 15 | 137.7 s |
| drivemount-winfsp | lifecycle | 38 | 1 | 136.9 s |
| drivemount-sftp | sftpfs² | 26 | 26 | 64.6 s |
| drivemount-sftp | connector | 39 | 28 | 52.7 s |
| drivemount | drivemount_exe | 7 | 7 | 44.5 s |
| drivemount-winfsp | read_path | 51 | 0 | 37.5 s |
| drivemount-sftp | jump | 28 | 25 | 36.5 s |
| drivemount-sftp | auth | 45 | 44 | 29.0 s |
| drivemount-winfsp | explorer | 2 | 2 | 20.7 s |
| drivemount | drivemount_app (lib) | 224 | – | 17.8 s¹ |
| drivemount-win | lib | 65 | 7 | 13.0 s |
| drivemount-vfs | readahead | 41 | 0 | 10.1 s |
| drivemount-winfsp | write_path | 103 | 1 | 9.6 s |
| drivemount-sftp | lib | 47 | 0 | 8.6 s |
| drivemount-vfs | contract | 31 | 0 | 7.6 s |
| drivemount-core | lib | 227 | 0 | 7.4 s |
| drivemount-sftp | known_hosts_openssh | 9 | 4 | 6.3 s |
| drivemount-winfsp | fsx | 2 | 0 | 6.1 s |
| drivemount-sftp | known_hosts | 22 | 0 | 5.5 s |
| drivemount-vfs | session, deadline, cache, lib, dyn_traits | 177 | 0 | 7.7 s |
| others | auth_script, mount_span, winfsp lib, nfs, smb | 31 | 0 | 0.8 s |

² Without the ProFTPD contract test, which was skipped in this run.

## The slowest 10 %

The 101 slowest tests (10 % of the 1,007 timed serially) take **89.6 %** of the time. The top 5 alone take 43 %.
No test was removed: none of the slow ones turned out to be low-importance. Each is the only test
for its requirement, or it was slow for a reason that could be fixed.

### Optimized

| Test | Before | After | What was slow, and the fix |
|---|---:|---:|---|
| `sftpfs::the_contract_holds_on_a_server_without_the_extensions` | > 20 min (killed) | 32 s | `run_all` includes the 1 GiB case: 32,768 × 32 KiB writes and as many reads to ProFTPD through the localhost relay. Now `run_quick`, which exists for exactly this ("extra configurations of a backend whose full run is covered elsewhere"). The 1 GiB case still runs on OpenSSH, and ProFTPD's 32 KiB chunking has its own test. |
| `mount_one::a_certificate_mounts` (first test in the binary) | 104 s | 34 s | Not the test itself: each test ran `cargo run -p drivemount --example mount_one`. `-p` resolves fewer dependency features than `cargo test --workspace`, so the first test rebuilt the shared crates, and the next `cargo test` rebuilt them back. Now the example is built once (`OnceLock`) with `--workspace` and run directly, which also removes the cargo start-up from every other test. Binary: 137.7 s → 44.4 s. Some rebuild (about 33 s) remains, and I didn't find its cause (stopped to keep this short). |
| `lifecycle::losing_the_server_fails_calls_and_does_not_stop_unmounting` | 72.8 s | ≈ 2.5 s | Each failing write waits one `op_timeout` (30 s) for a reconnect that never comes (`SessionFs` design, FR-REC-04). The test now uses a 1 s `op_timeout`. |
| `lifecycle::a_flush_that_fails_as_the_drive_stops_does_not_fail_the_unmount` | 30.1 s | ≈ 1 s | Same cause and fix. |
| `read_path::a_lost_connection_reads_as_an_unreachable_network` | 30.0 s | 1.0 s | Same cause and fix. |
| `connector::auth::tests::encrypted_keys_ask_open_and_notice_a_wrong_passphrase` (unit, in CI) | 5.4 s | 1.1 s | `ssh-keygen` used 16 bcrypt-pbkdf rounds, paid on each key load in a debug build. It now uses `-a 1`; the key format is the same. |

Saved per full run: ≈ 3.5 minutes of the measured time, plus the ProFTPD test, which went from
over 20 minutes (killed) to 32 s. In CI's default `cargo test`, the three `op_timeout` tests and the
key test save ≈ 135 s.

### Kept as they are

| Tests | Time each | Why they stay |
|---|---:|---|
| `sftpfs::the_contract_holds_on_openssh` | 40 s | The only full contract run (1 GiB plus the rekey check) against a real OpenSSH. High value. |
| `drivemount_exe::*` (7) | 4–12 s | Start the real `drivemount.exe`: windows, tray, single instance, crash report. These are acceptance tests. |
| `connector` / `jump` / `auth` keep-alive and dialog-timing tests | 3–7 s | Their time is the keep-alive interval or timeout that they test. |
| `readahead::reads_match_the_file` (proptest, in CI) | 8.4 s | Already reduced to 64 cases on a paused clock. This is the main correctness test for read-ahead. |
| `fsx::ten_thousand_operations_lose_nothing` | 5.4 s | Random-operation stress test of the whole drive. |
| `lifecycle::with_the_cable_pulled_…`, `an_idle_drive_notices_…`, `a_large_read_carries_on_…` | 5–9 s | Real-SFTP reconnect tests (M2-01). Their time is the keep-alive interval they test. |
| `mount_one::*`, `auth::*`, `jump::*`, `connector::*` (≈ 70 tests) | 1–5 s | One real login or mount each, one per auth method or failure. Each covers a separate requirement. |

## Fixed: `real_drive` deadlocked when run in parallel

Its tests restart and modify the sshd container, which broke the tests running beside them; with
the default threading, that left a process stuck in a drive's I/O, impossible to kill. They now take
turns through a static `ONE_AT_A_TIME` mutex held by `World`, as `lifecycle`'s do, so `--test-threads=1`
is no longer needed. Checked with the default threading: 9 passed in 36 s (the 1 GiB test skipped).

## After the changes

`cargo test --workspace` (what CI runs): all passed in 167 s, including the build. `lifecycle` is 33 s
(was 128 s). The one default test in `mount_one` still pays about 30 s for the remaining rebuild.

## Worth doing next (not done)

- **The rest of `mount_one`'s rebuild:** the nested `cargo build --workspace --example mount_one`
  still recompiles something once after `cargo test --workspace`. Find out what with
  `CARGO_LOG=cargo::core::compiler::fingerprint=info` passed to the nested cargo.
- **`a_gibibyte_and_ten_thousand_small_files_…`** needs `DRIVEMOUNT_TEST_HOST` set to the WSL address
  (its doc says so). Through the localhost relay it takes over an hour.
- Measuring: `RUSTC_BOOTSTRAP=1`, needed for libtest's `--report-time`, changes cargo's fingerprints,
  so the next `cargo test` rebuilds everything. Time tests serially from the streamed output instead.
