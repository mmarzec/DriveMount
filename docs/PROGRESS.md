# FS Manager: Progress notes

Notes for the next session: decisions, open issues and anything half-done. Newest first.

## 2026-11-04: M1-07 `known_hosts` (branch `feat/m1-07-known-hosts`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and both script checks also pass for `fsm-sftp` and `fsm-vfs` in a `rust:1.99.1` container. `fsm-sftp` now has 38 tests (7 unit, 22 fixture and append, 9 against OpenSSH, of which 4 need the S-03 server and are `#[ignore]`d). `Cargo.lock` gained `base64`, `hmac`, `sha1`, `sha2` and what they need; `getrandom` is a new direct pin, at the version already locked.

- **Added:** `fsm_sftp::known_hosts`: `HostKey::from_wire`, `KnownHosts::{load, parse, check, known_key_types, uses_hashing}`, `append_if_unknown`. Nothing calls them yet. Also `ConnectError::HostKeyRevoked` and code E-HOSTKEY-REVOKED, the gap the M1-02 notes left; its hint text is new in §7.8, so the owner may want to reword it.
- **The rules come from running `ssh`, not from memory**, and two surprised me (now in spec §6.3):
  - **A known key of another type counts as changed.** With FR-SSH-10's no-override rule, that is a refusal.
  - **A plain `host` line also serves other ports**, but only for its exact key; a different key there is just unknown.
  - `ssh` reports the last offending line, and the Windows build skips a UTF-8 byte order mark. Both are followed.
- **For M1-08a:**
  - **Must ask the server for the key types on file first** (`known_key_types`), as `ssh` does. Otherwise a server with RSA on file and a newer ed25519 key is refused with no way out. The plan row now says so.
  - **`append_if_unknown` returns a status, and `Changed` or `Revoked` must still refuse.** The file is read again under the lock, because the dialog may have been open for minutes.
  - It blocks, for up to 5 s on the lock: use `spawn_blocking`. Treat every `KnownHostsError` as not trusted.
  - `HostKey` takes the wire blob. The spike's `check_server_key` receives `&PublicKeyOrCertificate`: **a certificate branch needs a decision**, since `@cert-authority` lines are ignored here. Getting the blob from `russh` is untested.
  - The caller resolves `%USERPROFILE%\.ssh\known_hosts`.
- **One deliberate difference from OpenSSH:** a `@revoked` line under plain `host` wins even if the key is good under `[host]:port`. OpenSSH accepts that. Tested.
- **Host names are checked before saving.** A host with a newline, space, comma or pattern character is refused, or a profile could make the file trust a key for another host.
- **`known_hosts.lock` stays behind, empty.** It uses an operating-system lock, so a crash cannot leave it stuck. Removing it safely would need a second lock.
- **Known limits:**
  - Only the one file is read, as §6.3 says. A `@revoked` line in `known_hosts2` or the system-wide file has no effect here.
  - A UTF-16 file (what `>` writes in PowerShell 5.1) is not handled and was not tested: it would read as empty, and an append would add UTF-8 to it.
  - A line with a valid type but a truncated key is kept, so it can report `Changed`, where `ssh` probably skips the line (not tried). The result is a refusal naming the line to remove.
  - The concurrency tests use threads, each with its own handle on the lock file. Two separate processes were not tested.
  - The comparison with real `ssh` ran on OpenSSH 9.5 on Windows only. On Debian's 9.2 only a few cases were tried by hand (plain-name fallback, another curve counting as changed), and they agreed.
- **For CI:** the `ssh-keygen` tests fail if it is missing, on purpose, so runners need it (noted in `ci.yml`). The `#[ignore]`d `ssh` tests do not run in CI yet. **M1-08a:** run them where it starts the S-03 server. To run them now: `tests/docker/servers.ps1 up`, then `cargo test -p fsm-sftp --test known_hosts_openssh -- --ignored`.
- **The tests are checked too:** 46 faults injected by hand. 41 failed a test at once. 2 hung rather than failed (a broken glob loop, a lock that never times out); they are noticed, but only by a time limit. 3 were test gaps, now closed and re-run. The script was not kept.
- **Next:** M1-08a.

## 2026-11-04: M1-06 `DeadlineFs` and `ReadAhead` (branch `feat/m1-06-deadline-readahead`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `fmt`, `clippy`, `test` and both script checks also pass for `fsm-vfs` in a `rust:1.99.1` container. The size check was not run: nothing in `fsm.exe` uses this yet. `fsm-vfs` now has 229 tests (78 unit, 60 cache, 41 read-ahead, 31 contract, 12 deadline, 3 `dyn` trait, 4 doc). `Cargo.lock` did not change.

- **Added:** `DeadlineFs::new(inner, op_timeout)` and `ReadAhead::new(fs, handle)` in `fsm_vfs`. Nothing uses them yet.
- **Order settled:** the M1-05 note said the plan leaves this open, but M1-12a says `CachedFs<DeadlineFs<…>>`. A test covers that order: a `mkdir` that timed out after the server acted shows in the next listing.
- **Suspect signal:** `DeadlineFs::suspect()` gives a `watch::Receiver<u64>` counting timeouts. A count, so nobody has to clear it. A `Timeout` from the backend counts too, since M1-10 will produce its own. **M2-01:** probe on each change.
- **`ReadAhead` owns the handle.** Its requests are tasks that outlive the read, and `RemoteHandle` cannot be cloned. Other calls use `reader.handle()`; `reader.close()` stops the tasks and closes the handle. It must run inside the runtime, which `block_on` on the dispatcher threads provides.
- **For M1-12a/b:**
  - **Call `discard()` after any change to the file**, however the change ended. That includes changes through **another handle** on the same file: the reader cannot see those, and until then it can return data up to 5 s old. M1-12b needs a way to reach the other readers of a path.
  - `ReadAhead::read` returns the full length unless the file ends, unlike `RemoteFs::read`, so the adapter does not need its own loop.
  - **A read fails as a whole**, even if part of its range arrived.
- **Choices the plan left open** (now in spec §5.6): the first read does not fetch ahead, so reading a header costs nothing; the window doubles per window consumed, not per read; chunks never exceed 4 MiB; speculative requests wait until a chunk or half a window is due, or small reads become small requests.
- **Added beyond the plan: a 5 s limit on data and errors fetched ahead.** Without it, a reader that pauses would later get bytes or an error from any time in the past. A reader that hits the limit returns to a 128 KiB window, so a slow consumer does not keep refetching 4 MiB.
- **Known limits:**
  - An `open` dropped by the deadline can leave a handle open on the server until the session ends.
  - **Small files cost a second round trip:** a short answer must be asked for again under the `RemoteFs::read` contract, so a read past the end of a small file takes two. **M2-08:** measure; the adapter's known size could avoid it.
  - Only an exact continuation counts as sequential. Slightly out-of-order reads from the kernel would reset the window. **M1-12a/M2-08:** check real traces.
  - `Arc<dyn RemoteFs>` does not implement `RemoteFs`, so `DeadlineFs<Arc<dyn RemoteFs>>` as M1-12a describes needs a small impl. `ReadAhead<dyn RemoteFs>` works.
- **The tests are checked too:** 31 faults injected by hand. 25 of the first 30 were caught at once. Of the other 5, 3 were test gaps, now closed and re-run (retrying at the same offset after an error, over-long answers, top-ups in a small window); one showed a redundant condition, now removed, and its replacement fault is caught; one remains uncaught and is harmless: `>` for `>=` at exactly 5 s. The script was not kept.
- **Gotcha for fault runs on Windows:** killing a shell does not kill its children. The first run hung for 40 minutes on one fault because the hung test binary kept the output pipe open; use `taskkill /T` and write output to a file. A test that waits on a signal needs its own time limit, or a broken signal hangs it instead of failing it.
- **Not measured:** real throughput and memory. NFR-PERF rows stay open until M1-10 and M2-08. Spike M0-2's rates were for a free-running pipeline, not this reader.
- **Next:** M1-07.

## 2026-11-04: M1-05 `CachedFs` (branch `feat/m1-05-cachedfs`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `fmt`, `clippy`, `test` and both script checks also pass for `fsm-vfs` in a `rust:1.99.1` container. The size check was not run: nothing in `fsm.exe` uses this yet. `fsm-vfs` now has 176 tests (78 unit, 60 cache, 31 contract, 3 `dyn` trait, 4 doc).

- **Added:** `CachedFs::new(inner, ttl)` in `fsm_vfs`. Nothing wraps a backend with it yet. The profile's `cache_ttl_secs` becomes `ttl` where the mount is put together. `tokio`'s `test-util` is a dev-dependency only; `Cargo.lock` did not change.
- **Spec §5.6 tightened, because the table's rules alone leave stale data.** See the bullets added under it:
  - **Subtrees:** rename, `remove_dir` and `mkdir` clear everything below the path. Without this, `/d/f` stays visible after `/d` is renamed.
  - **Failed and dropped calls invalidate too:** a timeout says nothing about whether the server acted. It runs from a drop guard, so **M1-06 can put `DeadlineFs` above or below `CachedFs`**; the plan does not say which.
  - **Overlapping lookups:** an answer overtaken by a change to its path is returned but not stored. Paths share 256 generation counters, plus one for subtree changes, so memory stays fixed; a collision only loses one fill.
  - **Writes also invalidate the parent's listing**, since listings show size and time, and again on flush and close, since a server can update times that late. Handles that wrote nothing do not invalidate, or browsing would empty the caches. Handles follow local renames.
- **Choices the plan left open:**
  - **10k per cache, a listing weighing 1 + its entries.** Counting a listing as one entry would allow 10k huge listings, and expired entries stay until pushed out. So **a directory of 10,000 or more entries is never cached**, and its entries' attributes are not filled either. **M1-12a:** keep a snapshot per directory handle for paged `ReadDirectory`, or a big folder is listed again on each page. Worth a second look in M2-08 if that hurts.
  - Only `stat` is cached, not `lstat`; nothing in the spec's mapping needs `lstat` to be fast. `open` always goes to the server, so a negative entry can never stop a file being opened.
  - A TTL of 0 leaves the negative TTLs on, as the spec's table reads. So with TTL 0, a `desktop.ini` created by someone else can still look missing for up to 60 s, unless a listing or an open shows it.
  - Noise names match exactly as listed: `DESKTOP.INI` is an ordinary name.
- **Settled from M1-02:** `CachedFs` does not change what `read_dir` returns, so showing a link as its target is still for `SftpFs` or the adapter (M1-10/M1-12a). `CachedFs` only avoids filling `stat` from a symlink's entry.
- **Known limits:** a change that reaches a path through a symbolic link, and a rename that failed without saying whether it happened (a handle then keeps its old path), can leave attributes stale until the TTL runs out.
- **The tests are checked too:** 65 faults injected by hand, one per rule. 57 failed at least one test at once. Of the other 8, 6 were test gaps, now closed and re-run; one was a harmful line, now removed: an older "not found" arriving last wiped a newer positive answer. One remains uncaught and changes nothing observable: dropping the zero-TTL check, which only avoids a clone, since `insert` refuses expired entries anyway. The script was not kept.
- **Not measured:** memory. Three full caches may take several MiB, and entries are not removed on a timer (NFR-PERF-05 forbids wake-ups). **M2-08/M2-09:** check against NFR-PERF-04's 8 MiB per idle mount. NFR-PERF-08's "repeated listing < 20 ms" should hold but is untested. Each `mkdir` and rename walks up to 30k entries; this should be small next to a round trip, but it was not measured against NFR-PERF-07.
- **Next:** M1-06.

## 2026-11-04: M1-04 name mapping (branch `feat/m1-04-name-mapping`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and both script checks also pass in a `rust:1.99.1` container. `fsm-vfs` now has 104 tests (66 unit, 31 contract, 3 `dyn` trait, 4 doc).

- **Added:** `remote_to_windows`, `windows_to_remote` and `resolve_name` in `fsm_vfs`, working on one path component. Nothing calls them yet. **Plan gap:** no M1-12 row mentions name mapping, though the adapter is the only place that can call it; M1-12a needs to pick it up. No requirement names this behaviour, only spec §5.6, so `TRACEABILITY.md` has no row to update.
- **Spec §5.6 tightened, because it left these open:**
  - The whole trailing run is mapped, the plain reading of "trailing dots or spaces". **Not confirmed against NEO or SSHFS-Win:** Cygwin's manual says trailing dots and spaces are converted but gives no per-character rule, and WinFsp's source could not be fetched. If M1-12 finds that NEO shows `a..` differently, only `remote_to_windows` and its inverse change; nothing is stored.
  - `windows_to_remote` can fail. Without that, `` reaches the server as `..` and leaves the directory.
- **Choices the plan left open:**
  - A private-use dot or space is reversed only in the run at the end, and not if a real dot or space comes just before it. That is what makes every valid Windows name round-trip, including ones nobody would type, like `a.`.
  - U+F000 and U+F02F are ordinary characters both ways, so they can never become NUL or `/`.
  - Names with a literal U+F03A and the like are the one case that does not round-trip. `resolve_name` compares what Windows would show, so they still open. **M1-12a:** use the name it returns, not `windows_to_remote`, for anything that exists; use `windows_to_remote` only for creating.
  - Case folding is per-character upper-casing, like the upper-cased keys of spike M0-1: `ß` is not `SS`. Non-ASCII never folds to ASCII, so `file` does not open `fıle`. It uses Unicode's table, not NTFS's `$UpCase`; rare differences only affect the fallback.
- **The tests are checked too:** 12 faults, injected by hand, each failed at least one test. The script was not kept.
- **For later tasks:**
  - **M1-12a:** WinFsp names are UTF-16 and can hold lone surrogates, which a Rust `str` cannot; decide how to refuse them. `Ambiguous` means not found, so `NOTES` with `notes` and `Notes` listed makes a third file on create. The spec does not cover that case; it seems right, because it opens nothing by guesswork.
  - **M1-05:** `resolve_name` is O(n) over a listing and allocates only for names it has to map. Spec §10.5 wants a criterion benchmark for name mapping, not done here: no task has claimed it, and it is not in M1-04's done-when.
  - **M1-10:** the non-UTF-8 names question from M1-02 is still open.
- **Next:** M1-05.

## 2026-11-04: M1-03 `MemFs` and the contract suite (branch `feat/m1-03-memfs-contract`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and both script checks also pass in a `rust:1.99.1` container. `fsm-vfs` now has 75 tests (39 unit, 31 contract, 3 `dyn` trait, 2 doc). The full suite with the 1 GiB file takes about 3 s in a debug build on Windows. Peak memory was not measured.

- **Differs from the plan:** `run_all(fs, root)` rather than `run_all(fs)`, because SFTP users cannot write to `/`. It works in a new directory under `root` and removes it if everything passes. `run_quick` is the same without the 1 GiB file, for extra configurations of a backend. Each call needs about 1 GiB of RAM on `MemFs`, so only one test uses `run_all`.
- **Trait docs tightened, because the suite tests them:** gaps and extensions read as zeros; a failed rename leaves both paths alone, including a missing source with `replace`; renaming a path onto itself with `replace` keeps the file; a file and a directory cannot replace each other; a missing parent may give `PathNotFound` or `NotFound`, since SFTP cannot tell them apart. **M1-10:** the §5.7 rename fallback (remove the target, then rename) has to check these first, or it loses data; the suite has cases for it.
- **Added:** `FsStats::PLACEHOLDER` (1 PiB, spec §5.7), for `SftpFs` too.
- **`MemFs` choices:** sparse 64 KiB pages, so writes past 4 GiB cost nothing. One lock for the tree and one per file, state before file. Handles keep removed files alive, as on POSIX. Reads are cut to `max_read`, and writes above `max_write` fail, to catch callers that do not split them. `statfs` counts written pages against a nominal 1 TiB, and writes never run out, so **`NoSpace` cannot be tested with it yet**.
- **`testing` feature:** `fsm-vfs` has a dev-dependency on itself so plain `cargo test` covers it. Other crates should take the feature in `[dev-dependencies]` only. `cargo tree` shows it stays off in normal builds.
- **The suite is checked too:** `tests/contract.rs` wraps `MemFs` in 25 faults, each breaking one promise, and each must make the suite panic. `should_panic` cannot say why, so the panic sites were read by hand once; all were relevant. The fault-free wrapper must pass.
- **Not covered:**
  - **Symbolic links:** the `stat`/`lstat`/broken-link promises are untested, because `RemoteFs` cannot create links and `MemFs` has none. M1-10 needs a fixture on the Docker server, or a separate case taking a prepared link. Settle with the `DirEntry` question from M1-02.
  - **Rest of §10.2:** pipelined throughput, connection kill and deadline expiry belong to M1-06, M1-10 and M2-01.
  - **Running it for real:** the suite needs a non-root user and POSIX names, so Windows servers stay in the lab matrix. The `tester` Docker user fits, but nothing but `MemFs` has run it yet; expect real servers to find mistakes in the suite as well as in `SftpFs`.
- **Next:** M1-04.

## 2026-11-04: M1-02 `fsm-vfs` types and traits (branch `feat/m1-02-vfs-types`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and the secret check also pass in a `rust:1.99.1` container. The types have 32 tests (28 unit, 3 `dyn` trait, 1 doc); backends do not exist yet, so the traits' contract is first exercised in M1-03.

- **Decided (ADR-0001):** `async-trait`. It is a new dependency: `russh` does not use it. Native `async fn` still fails behind `dyn` on 1.99.1; boxing costs 95 ns per call by either method. `fsm-vfs` re-exports `async_trait` and `Bytes`.
- **Differs from spec §5.4, spec updated:**
  - Added `RemoteFs::fstat`: §5.8 maps `GetFileInfo` to it, but the trait did not have it.
  - `AttrChanges::read_only` is a flag, not permission bits. The backend fetches the current permissions and calls `apply_read_only`, so a stale cache cannot overwrite the mode.
- **Choices the plan left open:**
  - `RemotePath`: a `..` above the root is an error, not clamped. `join` rejects absolute arguments rather than replacing the path as `std::path` does. NUL is rejected. Paths are UTF-8 `String`s, so `fsm-sftp` must decide what to do with non-UTF-8 names from a server (M1-10); the spec does not say.
  - `OpenMode` constructors make nonsense like create-for-read impossible, so backends need no check.
  - `RemoteHandle` is a `u64` and not `Clone`, because `close` consumes it. `Secret` has no `Clone` either; add one only if M1-08b needs it.
  - `DirEntry::attr` does not follow symlinks. Showing a link as its target (§5.6) is left to the layer above, or to `SftpFs`; settle it in M1-05/M1-10.
- **For later tasks:**
  - **M1-07/M1-08a:** `ConnectError` has no variant for a `@revoked` host key, and §7.8 has no code. E-HOSTKEY-REJECTED's hint ("Mount again to see the prompt") would be wrong for it. Add both there.
  - **M1-08b/M1-17:** FR-SSH-05's *Ask each time* prompt offers "password, or a key file plus passphrase". `Prompter::secret` only returns a string, so it cannot pick a key file yet. `SecretRequest::allow_remember` covers the "stores nothing" half.
  - **M1-15/M1-16:** the variant docs name the §7.8 codes, but plan §11's one place mapping errors to codes and localised text does not exist yet. The `Display` text is English, for logs. E-NO-FREE-LETTER belongs to the allocator's own error. M3 and M4 add their E-SMB and E-NFS variants.
- **Secret check:** `scripts/check-secret-format.sh` finds `expose_secret()` inside formatting and logging macros, including multi-line calls. It cannot follow a value through a variable. `assert*` macros are excluded because tests compare secrets. Negative-tested with Git Bash's `awk` and with `mawk` in the container, the kind Linux runners have.
- `cargo deny` and `cargo audit` stay clean: the `russh`/`winfsp` problems in the M1-01 notes below still arrive with those crates.
- **Next:** M1-03.

## 2026-11-04: M1-01 workspace skeleton (branch `feat/m1-01-workspace`)

**Status: done locally; the CI workflow has never run.** It needs the Gitea runners (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and the header check pass on Windows and in a `rust:1.99.1` container; the size check passes on Windows (empty `fsm.exe`: 0.20 MB).

- **Open, blocks M1-08a:** adding `russh` makes `cargo deny` and `cargo audit` fail. Its `rsa` feature pulls in `rsa` 0.10.0-rc.18, RUSTSEC-2023-0071 (Marvin timing attack, medium, no fixed version). FR-SSH-02 requires RSA keys, so dropping the feature would mean changing that requirement; the likely answer is a documented ignore in `deny.toml` and for `cargo audit`. Decide there, with the owner. Nothing is ignored yet. The workspace is clean today only because its crates have no third-party dependencies.
- **Open, blocks M1-12a:** `winfsp` pulls in `paste`, unmaintained (RUSTSEC-2024-0436). That also fails `cargo deny`. Add an ignore there, or set `unmaintained = "workspace"`, which only checks direct dependencies.
- **`deny.toml` targets take a union.** With both targets listed, Linux-only crates under Windows-gated `slint` and `tray-icon` are still checked: `ttf-parser`, `zvariant*` and `zbus_names` (via winit and accesskit), `proc-macro-error` (via gtk). Those five advisories fail too, though with `--target x86_64-pc-windows-msvc` only `rsa` and `paste` remain. Settle this when `slint` arrives (M1-16). `cargo audit` reads the whole `Cargo.lock` and ignores targets, but fails only on the `rsa` vulnerability.
- **Checked on the full pinned set in a scratch crate:** it resolves (772 crates) and compiles on Windows; the ADR-0003 licence list accepts all of it, including `ring`, Slint and `winfsp`. `winfsp` and `winfsp-sys` declare the deprecated id `GPL-3.0`: a warning only, accepted by the allow-list.
- **Pins:** the latest stable of everything, except `windows` =0.61.3, which is the version spike M0-1 tested with `winfsp`. Slint and winfsp feature sets come from the spikes. `sha2`, `hmac` and `sha1` are one major version newer than the spikes used; they compile, but M1-07 is their first real use.
- **Negative tests run (scratch copy):** `unwrap`/`expect` outside tests fail clippy and pass inside `#[cfg(test)]`; `unsafe` fails outside `fsm-win`/`fsm-winfsp`, and without a `// SAFETY:` comment inside them; a missing header fails, including on new untracked files; an exe over the limit fails; a `GPL-2.0-only` crate fails `deny`.
- **For later tasks:**
  - Linux needs the Windows-only crates to stay empty: keep their third-party dependencies under `[target.'cfg(windows)'.dependencies]`. `fsm-win`, `fsm-winfsp` and `fsm-smb` start with `#![cfg(windows)]`.
  - `clippy.toml`'s test exemption covers `#[test]` and `#[cfg(test)]` code. Helper functions in `tests/*.rs` may need `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
  - When `winfsp` is added, CI runners need LLVM/libclang and the WinFsp MSI with `ADDLOCAL=ALL` (M0-1), and `fsm` needs a `build.rs` calling `winfsp_link_delayload()`.
  - `Secret` arrives in M1-02; add the CI grep check for `format!` of its contents (plan §11) there.
  - The workflow is in `.github/workflows/`, which Gitea also reads. It has no Docker or WinFsp steps yet (plan §9.1 adds them with M1-10 and M1-12c).
- The root `.cargo/config.toml` and `rust-toolchain.toml` also apply to `spikes/*` when built from there. Spikes now get `+crt-static`, so old size numbers may not reproduce exactly. Spike sources gained SPDX headers, so the check has no exclusions.
- **Next:** M1-02.

## 2026-11-04: M0-3 UI spike (branch `spike/m0-3-ui`)

**Status: done, go with open items.** Report: `docs/spikes/m0-3-ui.md`; logs and screenshots in `spikes/m0-3-ui/results/`. Start 160 ms, `--tray` 50 ms, working set 17.6 MiB (25.7 after a toast), exe 9.31 MiB. Plan, spec and analysis updated.

- **Decided:** toasts through `windows` directly. The app writes `HKCU\Software\Classes\AppUserModelId\<id>` at start. On this PC, a shortcut with an AUMID alone showed no toast even after 5 min (cause not found); the key alone or both together work.
- **Open for M2-04:** a toast clicked in Action Center does nothing, whether or not the app runs, so FR-NOT-04 is unmet on that path. COM activator and protocol activation not tried.
- **Open for M2-02/H-08:** real sleep/resume not tested; the owner deferred it. Only sent messages reached the hidden window. Do it first in M2-02.
- **Open for M2-08:** idle CPU with the window visible bursts past 0.1% in some 60 s windows, sometimes with no redraw; cause not found. Hidden: 0.000% every time.
- **Owner accepted** the go verdict on 2026-11-04 and considers the open items very minor, to be dealt with later.
- **Exe size:** UI + SFTP spikes total 12.13 MiB, leaving about 2.2 MiB under 15 MB. `opt-level = "s"` saves 1.3 MiB (speed not measured). M1-01 gets a CI size check.
- **Don't trust:** `Show()`, `History` and `Setting` for whether a toast appeared, `tray.rect()` (flaky), `tray-icon`'s `build()` (ignores `NIM_ADD` failure). Screen captures of the corner gave the toast results.
- **Compare manual results with the logs:** the first H-05 report and the logs disagreed on toast clicks and sleep; a short retest cleared it up.
- Run: `cargo build --release`, then `checks.ps1 all` (actions at the top of the file). It shows the window, tray icon and toasts on the desktop. `checks.ps1 cleanup` removes the shortcut and registry keys.
- PowerShell 5.1 traps met: `$null` to a .NET `string` parameter arrives as `""` (use `[NullString]::Value`); a GUI exe called with `&` doesn't block and loses its exit code.
- **Next:** G1, then M1-01.

## 2026-11-04: M0-2 SFTP spike (branch `spike/m0-2-sftp`)

**Status: done, go.** At the SFTP layer, reads and free-running writes reach 102–261% of `sftp.exe` on LAN and WAN (50 ms). Report: `docs/spikes/m0-2-sftp.md`; raw data in `spikes/m0-2-sftp/results/`. Spec §5.6, §5.7, OD-4 and plan M1-10 updated.

- **Decided:** OD-4 write-behind on, because per-call writes reach only 49% on WAN. Bound changed from the spec's 8 MiB to 4 MiB: same rate, half the metadata delay. Built in M2-08; the spike only measured an equivalent pipeline. The owner has not reviewed the 8 → 4 MiB change or the AES-GCM-first preference yet.
- **Decided:** `ring` backend (default `aws-lc-rs` needs NASM), 16 MiB window, at most 4 MiB in flight per handle, one SFTP channel.
- **Gotcha: use the WSL VM's IP for performance tests.** The `localhost:2222` relay (`wslrelay`) stalls single writes of 16–64 KiB by about 44 ms each. `bench.ps1` does this by default.
- **Open:** downloads at 64 × largest chunk intermittently run at half speed; cause not found (not the channel window). Not seen at 16 in flight.
- **Open for M1:** a hung server is detected at 58–60 s, right on FR-SSH-12's limit (`keepalive_max` = 2 should fix it; untested). Rekey during transfer untested. `cargo deny` must accept `ring`'s licence.
- Run: `cargo build --release`, `target\release\m0-2-sftp.exe prepare`, then `bench.ps1` (actions at the top of the file). `tests\docker\servers.ps1 check` first, for the key copy `sftp.exe` needs.
- PowerShell 5.1 traps met: `Tee-Object` has no `-Encoding`; `{0:F2}` uses the Polish decimal comma unless given InvariantCulture; `R` is an alias, and aliases beat functions.
- **Next:** M0-3 (UI spike), then G1.

## 2026-11-03: M0-1 WinFsp spike (branch `spike/m0-1-winfsp`)

**Status: done, go.** Scripted checks 32/32, owner's H-04 checks 6/6. Report: `docs/spikes/m0-1-winfsp.md`. Spec §5.8, FR-MNT-05/06 and plan M1-12c updated.

- **Open for M1-12c:** a running Explorer did not show the new `M:` until Explorer was restarted. `SHChangeNotify` from another process did nothing, and synchronous `WM_DEVICECHANGE` broadcasts hung on a suspended Store app. Try `WNetAddConnection2W` via `WinFsp.Np` first. Explorer also names the drive "spike (\\fsm)" (from the UNC path), not the volume label; use `_LabelFromReg`.

- Build the spike: `cargo build --release --features system --target-dir target/system` in `spikes/m0-1-winfsp`. Run the checks: `checks.ps1` (unelevated; mounts `Q:`).
- **Decided:** `winfsp` crate with the `system` feature (installed DLL found via the registry; bundled mode fails at runtime with 1285). `FileInfoTimeout` = 1000 ms confirmed.
- **New prerequisite:** LLVM/libclang for `bindgen`, installed via winget. CI needs it, plus the WinFsp MSI with `ADDLOCAL=ALL`.
- Use `windows` 0.61 alongside `winfsp` 0.13 (error conversions), or the crate's `windows-62` feature.
- The owner's PC has remembered drive mappings `W:`–`Z:` (`Y:` → `\\wsl$\Ubuntu-22.04` is disconnected but remembered). Don't mount tests there. FR-MNT-03's check is needed for real.
- **Next:** M0-2 (SFTP throughput with russh against the S-03 server), then M0-3 (UI spike).

## 2026-11-03: Setup (S-01 to S-05)

**Status:** setup complete except S-05 (deferred to Gitea). G0 is met.

**Done**
- **S-04:** git repository (`main`), `.gitignore`, first commit of `docs/`.
- **S-01:** Rust stable 1.99.1 (`x86_64-pc-windows-msvc`, with clippy and rustfmt) via rustup; VS 2022 Build Tools 17.14 with the C++ workload; `cargo new hello && cargo run` links. Cargo tools: `cargo-deny`, `cargo-audit`, `cargo-wix`, `cargo-about` (needs `--features cli`).
- **S-02:** WinFsp 2.2 Beta5 (2.2.26286) re-installed with `ADDLOCAL=ALL`: `inc`, `lib`, `opt`, `samples` present.
  - Installers and test suites for 2.2.26286 and 2.1.25156 (minimum version, PLAT-02) are in `C:\marcin\tools\winfsp`. The MSIs are Authenticode-signed by Navimatics LLC; the test exes are unsigned.
  - `winfsp-tests-x64.exe` needs `C:\Program Files (x86)\WinFsp\bin` on PATH (otherwise it fails with STATUS_DLL_NOT_FOUND). It has no `--help`; use `--list`.
- Also installed: GitHub CLI (`gh`). It is not needed now; see below.

**Decisions (owner)**
- **No Docker Desktop.** Test servers run on Docker Engine inside WSL2 Ubuntu-22.04 (confirmed by owner).
- **No GitHub for now.** The repository stays local; it will later be pushed to the owner's LAN Gitea instance. S-05 is deferred: CI will be a GitHub-Actions-compatible workflow for Gitea Actions (self-hosted `act_runner`s, one on Windows). G0 no longer waits on S-05.

- **S-03:** after enabling Virtual Machine Platform and rebooting, WSL2 runs. Docker Engine 24.0.6 + Compose v2.21 were already installed in Ubuntu-22.04 with systemd on, so nothing was installed.
  - `tests/docker-compose.yml` (project `fsm-test`) builds `tests/docker/sshd`: a Debian OpenSSH server on `localhost:2222` with users `tester` (password/key), `keyonly`, `certuser` (OpenSSH cert) and `totpuser` (keyboard-interactive: password + TOTP). Credentials are in `tests/docker/README.md`.
  - `tests/docker/servers.ps1 up|down|check`. `check` runs the key and cert logins from Windows (proves the WSL port forward) and the password, passphrase and TOTP logins inside the container with `expect` + `oathtool`. All six pass.
  - Keys are generated by `tests/docker/gen-keys.sh` into `tests/docker/keys/` (git-ignored).
- Added `.gitattributes`: LF everywhere except `*.ps1` (CRLF), so the shell scripts survive checkout on Windows.

**Gotchas found**
- expect: `(?i)` inside `-re` is defeated by expect's case-sensitive "gate keeper" glob optimization; use `-nocase -re`.
- Windows PowerShell 5.1: piping text to a native exe adds a BOM (sftp says "Invalid command."), and stderr from a native exe under `ErrorActionPreference=Stop` with `2>&1` throws. Use files and `LogLevel=ERROR` instead.
- Windows OpenSSH rejects private keys with inherited ACLs. `servers.ps1` copies them to `%TEMP%\fsm-test-keys` with owner-only full control (read-only owner ACL blocks later deletion).
- PAM modules log to syslog, which the container lacks. To debug, install `busybox-syslogd` in the running container and add `debug` to the PAM line.

**Open / next**
- VirtualBox 7.0.10 and Hyper-V coexistence is being handled in a separate session (owner's request); WSL2 stays on.
- Docker Engine in WSL is from 2023 (24.0.6); upgrade if anything needs a newer API.
- Next: G0 (S-01 to S-04 done; S-05 deferred) → M0 spikes, starting with M0-1 (WinFsp in-memory drive).
