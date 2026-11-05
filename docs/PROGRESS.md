# FS Manager: Progress notes

Notes for the next session: decisions, open issues and anything half-done. Newest first.

## 2026-11-05: M1-13 Profile model and validation (branch `feat/m1-13-profile-model`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `clippy` and `test` for `fsm-core` also pass in a `rust:1.99.1` container. `fsm-core` has 55 tests, all unit tests, of which 2 are property tests. The workspace has 618 passing and 133 ignored. **Not run:** the ignored server and desktop tests, as nothing they use changed, and the exe size check, as `fsm.exe` does not use this crate yet. `Cargo.lock` gained only `fsm-core`'s new dependencies, all already in the tree.

- **Added:** `fsm_core::profile`: `Profile`, `Protocol::Sftp(SftpProfile)`, `SftpAuth`, `JumpHost`, `JumpAuth`, `DriveChoice`. `validate`: a `check_*` function per rule of spec §7.3, a `parse_*` for each text box whose value is not text, and `validate_profile` for a whole profile. Errors are `FieldError { field, problem }`, and `Problem`'s `Display` is the text to show. `paste`: `parse_pasted_host`. Spec §7.3 now holds the messages it did not have, and the rules below.
- **Not everything is pure:** whether a key file exists cannot be. `read_key_head` is the one function that touches the disk, and `check_key_file` and `validate_profile` take it as a parameter, so tests pass a fake. It reads 8 KiB at most, into a `Secret`.
- **For the owner to check:**
  - **A real OpenSSH key named `.ppk` cannot be selected,** though `fsm-sftp` would connect with it, as it goes by content. FR-SSH-03 says selecting a `.ppk` disables Save, so the name is enough. Renaming the file gets round it.
  - **No user in the Jump host box means the profile's own user,** as with `ssh -J`, since §7.3 has `[user@]` as optional and FR-SSH-11 needs a user. So `JumpHost::username` is an `Option`, where §6.1 shows it always set.
  - **Rules added to §7.3:** control characters are refused in a name, as they are in a Windows file name, and spaces around a name do not make it a different one. A remote path above the root (`/..`) is refused, as it could never be opened.
  - **Hosts refused for now:** an IPv6 scope (`fe80::1%4`) and a non-ASCII name (`żółw.pl`, which needs `xn--…`). Neither was tried against the connector, and either is a small change. `_` is allowed, although not legal DNS.
- **Beyond FR-PROF-05, since these are what people will paste:** the scp form the main window shows (`user@host:/path`, `user@host:~`), `%XX` in a URL, IPv6 in or out of brackets, and a domain in a user. A password in the text is dropped, and `PastedHost` has no field for one. `None` means nothing is split. Only `sftp://` is taken as a scheme, as the spec says, so `ssh://` is left in the box and marked.
- **What Save does not know:** the key check only reads the start of the file, as `fsm-core` cannot depend on `russh`. A 1024-bit RSA key or an encrypted form `russh` cannot read (M1-08b) saves, and gives E-KEY-FORMAT at connect. The private-key headers were copied from `russh`'s decoder, which also skips text before the header, so the check does too. **At each `russh` upgrade,** compare them with `PRIVATE_KEY_HEADERS`.
- **The tests are checked too:** 59 faults injected by script (kept out of the repository). 58 failed a test at once. The other was a test gap, now closed: control characters in pasted text, as the only case also had a space. A harmless change as a control passed, as it should.
- **My own mistake, for whoever injects faults:** proptest saves failing seeds in `proptest-regressions/`, next to the crate, so a fault's seeds stay and can fail the next run, which then looks like a caught fault. Clear the folder after each fault, and keep a control. Restoring a file with `mv` can also leave the old test binary in use, as cargo goes by the file's time. The first control did show that `valid_parts_come_back`'s generator could make `/..`, which would have failed at random in CI. Fixed.
- **For later tasks:**
  - **M1-14:**
    - **The model has no serde,** on purpose. §6.1 is flat (`protocol`, `auth`, `key_path`, `remember`, `drive_letter`), and the model is enums, so a separate record type for the file looks simplest. `remember` only exists on `SftpAuth::Password` and `KeyFile`, and `key_path` only on `KeyFile`: for `agent` and `ask` they have nowhere to go, so a file's value is lost on save unless the record keeps it. `parse_drive` reads `drive_letter`, and `check_drive` takes the result. Blank `group` should load as `None`.
    - **`Profile` has no `extra` yet,** so that is M1-14's to add, or to keep in the record type.
    - **Don't refuse a file over one bad profile by accident.** With `port: u16`, `port = 70000` in a hand-edited file fails to parse, and that means read-only mode for all of it unless the record uses a wider type. The same goes for `cache_ttl_secs` and `keepalive_secs` being out of range, which do parse. Whether to run `validate_profile` on load is M1-14's choice; if so, don't let it read key files, as a key on a drive that is not plugged in is not a broken config. Pass a `read_key` that says yes.
    - **`JumpAuth::Password` has no `remember`,** as §6.1 has none for a jump host. Whether `FsManager:<uuid>:jump-password` exists is all there is.
  - **M1-15:**
    - `apps/fsm`'s `BackendFactory` turns this `SftpProfile` into `fsm_sftp`'s, which has the same name and holds secrets and limits as well. A jump host with no user takes `username.clone().unwrap_or(profile's)`. `DEFAULT_*` are the same as the file's defaults.
    - **The slug:** a name that passes `check_name` has no backslash, which answers M1-12c's note, but it can have spaces and non-ASCII. Names are unique ignoring case, so slugs can be too.
    - The Save command should call `validate_profile` too: `others` may include the profile itself. FR-PROF-08's lock is not here.
  - **M1-17:**
    - **Call `validate_profile` off the UI thread,** or at least the key check: `read_key_head` on a network path that is down can take a while.
    - `parse_pasted_host` fills only the parts that are `Some`, leaving the others. Trim Name and Group before storing, and store a blank Group as `None`: the checks ignore spaces around a name, but keep what they are given.
    - The `Problem` and `DriveWarning` texts are English `Display`s, and plan §11's one place for localised text does not exist yet, as M1-12c noted.

## 2026-11-05: M1-12c `fsm-winfsp` mount lifecycle and test harness (branch `feat/m1-12c-winfsp-lifecycle`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). The owner ran `tests/explorer.rs` on 2026-11-05 and reports all fine: the drive appears and goes without restarting Explorer, with its name, the network-drive icon and *Network locations*, and no stall seen. The drive is only up for moments in that test, so a longer look at the icon and grouping, and real programs on the drive, come with H-07 (M1-19). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows. The workspace has 563 passing and 133 ignored. `tests/lifecycle.rs` has 26 tests, which passed 15 runs of 15 before and after the last changes to them. The drive tests also pass with `FSM_TEST_SERVER=sftp`. **Not run:** the Linux container check (`fsm-win` and `fsm-winfsp` are still `#![cfg(windows)]`, tests included), the ignored server tests, as `fsm-sftp` did not change, and the exe size check, as `fsm.exe` does not use this crate yet. `Cargo.lock` gained only `fsm-winfsp`'s new dependencies, all already in the tree.

- **Added:** `WinFspBackend<C: Connector>` with `MountBackend`. What was `WinFspBackend` is now `Volume`, the drive alone. `fsm-win`: `set_drive_label`, `forget_drive_label`, and `notify_drive_added` and `notify_drive_removed` now also send Explorer the message. Tests: `tests/lifecycle.rs`, `tests/explorer.rs`, `tests/winfsp_tests.rs`, `tests/support/sftp_server.rs`. `scripts/get-winfsp-tests.ps1`, `scripts/install-winfsp.ps1`, `tests/winfsp-tests-exclusions.txt`, and a `drives` job in CI. `fsm-winfsp` now depends on `fsm-win` (spec §5.2 updated).
- **Decided by the owner, 2026-11-05: about 1.2 s to hide a drive is acceptable,** so FR-MNT-06's criterion now says so, with 1 s for showing as before. No more tuning is planned. The measurements: read from Explorer's own window through UI Automation, on a quiet machine, with the window already open and timed from when `mount` or `unmount` returned. One look at the window costs about 100 ms of that.
  - **Showing:** mostly 720–900 ms, with one 1462 ms. Within the limit, usually.
  - **Hiding:** mostly 1100–1340 ms, with a few near 550 ms. WinFsp's own `memfs` killed by script measured 585–810 ms with the same message, and why ours is slower was not found.
  - Tried without effect on hiding: `SHChangeNotify` before, after or not at all; `SHCNF_FLUSHNOWAIT`; 150 and 400 ms of waiting before telling Explorer; leaving the registry key; `SHCNE_UPDATEDIR` on "This PC".
  - `tests/explorer.rs` allows 3 s, so it catches Explorer not being told without failing on a slow moment, and prints what it measured.
  - **Twice, something much slower was seen, cause unknown.** Once Explorer did not answer UI Automation for 10–20 s (a round read 9982 and 20180 ms). Once `a_running_explorer_shows_the_name_and_hides_it_within_a_second` failed its 3 s limit, right after `winfsp-tests` had run; its details were not kept. The next 12 rounds were fine, and the owner saw nothing of the kind. Watch for it at H-07.
- **Explorer, settled (spec §5.8):**
  - **`WNetAddConnection2W` through `WinFsp.Np`, the plan's first candidate, does not work:** error 67 for `\\fsm\<slug>`, as the provider only takes classes registered as launcher services under HKLM. That needs administrator rights, which the portable zip does not have.
  - **`SHChangeNotify` alone does nothing for an added drive,** agreeing with the spike. A `BroadcastSystemMessage` with `BSF_POSTMESSAGE` returns 0, as a message with a pointer cannot be posted.
  - **`SendMessageTimeoutW` to `Progman`, 500 ms, `SMTO_ABORTIFHUNG` works,** for open and new windows, and leaves no ghost. `Shell_TrayWnd` does nothing. It goes to one window, so a suspended program cannot hold it up as it did the spike's broadcast.
  - **`_LabelFromReg` works,** Polish included, and a renamed profile shows at its next mount. Removed on unmount, along with the key Explorer keeps.
  - **Gotcha: Explorer's `Folder.Items()` is not what the window shows.** Calling it makes Explorer learn of the drive, and then new windows list it after it has gone, until told. An early experiment was fooled by this. Measure with UI Automation, on a letter Explorer has not seen.
- **Known limits, not fixed:**
  - **After a crash nobody tells Explorer,** so an open window can list a dead drive until that letter is next mounted or unmounted, and the label stays until then. The letter itself goes (`a_killed_process_leaves_no_drive`).
  - **Windows set to open folders in a separate process** were not tried: the message reaches only the desktop's process.
  - **`##fsm#t…` keys in `HKCU\…\MountPoints2`:** Explorer makes one for any drive it sees, so `Volume` tests, which do not remove them, leave some. Older ones from earlier sessions are still on this PC, deliberately not touched: they are harmless, and can be deleted by hand.
- **Choices the plan left open:**
  - **`Volume` is split from the backend** so that the 150-odd drive tests do not tell Explorer about each drive.
  - **Cancelling is dropping the future,** at any point. A drive started by then is in the blocking task's output, and goes with it unread.
  - **`Health` only follows the suspect signal.** A first attempt also raised suspect on `ConnectionLost`, which broke `other_errors_pass_through_without_suspicion`. That test is right, as FR-REC-01 keeps transport errors as a trigger of their own, so it was taken out. **So a dropped connection fails calls with the right error, and `Health` stays `Ok` until M2-01.**
  - **`unmount` never fails** over a flush or close: the drive has gone by then, and nobody is left to tell. The log has it.
- **The harness:** `FSM_TEST_SERVER=sftp` runs `read_path`, `write_path` and `fsx` over `SftpFs`, SSH and a socket, to a `russh` server in the process, over the same `Probe` and `MemFs`. Almost everything passed at the first try. 6 tests skip themselves there with a reason, as their fault cannot cross the wire or a hung request stays at the head of a server that answers in order, as OpenSSH's does. 2 compare times to the second, as SFTP v3 has no less. It is not OpenSSH: things only a real server can show still wait for M2-09, such as rename and delete with handles open on Windows OpenSSH.
- **`winfsp-tests`** (2.2.26286, fetched with the owner's approval, pinned by hash, which matches what GitHub publishes): each test alone, in its own folder, 120 s at most. **MemFs 65 of 94; SFTP 63 of 94.**
  - Started from no exclusions. Each of the 29 was read against the assertion that fails, in WinFsp's source at v2.2B5: attributes SFTP cannot keep, allocation, per-file security, reparse points, EAs, streams. `wsl_stat_test`'s reason is the least examined, and says so. 2 more over SFTP only: whole-second times, and 200 created files held open against `MAX_OPEN_FILES` of 112.
  - Excluding `setfileinfo_test` for its hidden attribute also loses the rest of that test. `tests/write_path.rs` covers times and sizes.
  - Its README warns it can lock up a machine or leave unkillable processes. Neither happened here, and only `stream_dirnotify_test` hung, and was killed.
- **The tests are checked too:** 22 faults injected by script (kept out of the repository). 19 failed a test at once, the Explorer ones through `tests/explorer.rs`, which CI cannot run. Of the other 3: 2 were test gaps, now closed, and both faults now fail a test (the limit on closing many files at once, and dropping a mounted backend). 1 changed nothing, `threads + 0`, so what the setting does is only known to reach WinFsp by reading the code.
- **My own mistake, for whoever runs Explorer tests:** the first `tests/explorer.rs` killed its PowerShell, which left 8 "This PC" windows open on the owner's desktop, and made the times grow from run to run. All 8 were closed, and the script now ends through its input. Numbers above are from after that.
- **For later tasks:**
  - **H-07:** repeat spike M0-1's manual checks, as the drive's security descriptor changed in M1-12a (see there).
  - **M1-15:** the backend already tells Explorer, so the manager need not (spec §5.5 updated). Pass `read_only` and `threads` in `VolumeConfig`. `slug` must have no backslash: nothing checks it here.
  - **M1-18:** `fsm.exe` needs `fsm-winfsp`'s `build.rs` call (noted in M1-12a).
  - **M2-01:** add transport errors and keep-alive misses to `Health`. `close_left_open` is where handles end up today.
  - **M2-08:** creating a file holds a remote handle until it closes, so a program can hold 112 new files at most over SFTP.
  - **CI:** `scripts/install-winfsp.ps1` needs administrator rights on the runner unless WinFsp is already there. Only its "already installed" path has run: the install itself is untested, as it would have reinstalled WinFsp here. The hash is what GitHub publishes for the MSI.

## 2026-11-05: M1-12b `fsm-winfsp` write path (branch `feat/m1-12b-winfsp-write`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `clippy` and `test` for `fsm-vfs` and `fsm-winfsp` also pass in a `rust:1.99.1` container, where `fsm-winfsp` is empty. `fsm-winfsp` has 179 tests: 23 unit tests and 156 that mount real drives through WinFsp: 51 in `tests/read_path.rs`, 103 in `tests/write_path.rs` and 2 in `tests/fsx.rs`. One is `#[ignore]`d, `a_gibibyte_round_trips`, for its memory; it passes. The mounted tests passed 30 runs of 30 in a row on the final code. The exerciser passed 100k operations on its usual seed, before the last small change, which removed a flag, and 10k on 5 other seeds earlier still; since that change it has run in those 30 runs at 10k. The workspace has 534 passing and 128 ignored. The ignored server tests were not run, as `fsm-sftp` did not change. The exe size check was not run: `fsm.exe` does not use this crate yet. `Cargo.lock` did not change.

- **Added:** all the write callbacks of spec §5.8, and `VolumeConfig::read_only`. `tests/write_path.rs` and `tests/fsx.rs`.
- **Not tried yet: a real `SftpFs` through a drive, and Explorer.** Everything here is `MemFs` through Win32 calls. M1-12c brings the SFTP harness; Explorer is H-06. Things only a real server can show: rename and delete with handles open on Windows OpenSSH, a rename that only changes the case on a case-insensitive server, and what the `fstat` after every write costs.
- **For the owner to check:**
  - **`fsx` is our own.** It has no ready Windows build and `winfsp-tests` is not on this machine, so `tests/fsx.rs` does the same operations through Win32, with `fsx`'s limits of 256 KiB and 64 KiB and mapped reads and writes, and checks every read against a copy in memory. It is not the original program, which the plan names. `FSX_OPS=100000` is what M2-09 needs. Building the original was not tried.
  - **`SetSecurity` gets `STATUS_NOT_SUPPORTED`.** Accepting it and doing nothing would leave someone believing a file was protected. `ReplaceFile` works either way (tested). If a real program turns out to give up over this, H-06 will show it, and accepting it is a one-line change.
  - **A failed delete looks like success,** as Windows has no way to hear about a failed `Cleanup`. The file is still there in the next listing, and the log has it. Spec §5.8 records this. An earlier check against the server is possible for permissions, but not reliable, so none was added.
- **Found in WinFsp, none of it visible from M1-12a:**
  - **The crate has no `CanDelete`,** only `SetDelete`, which the plan lists as an alternative. The driver remembers the flag and passes it to `Cleanup`, so nothing here keeps delete-pending state, as the plan's row suggested.
  - **`FILE_DELETE_ON_CLOSE` reaches `Cleanup` with no callback that can refuse,** also on a read-only drive, so `Cleanup` checks that itself. Without that the file would be removed. A test covers it.
  - **Moving a directory into itself and replacing a read-only file are refused by the driver,** as is a parent that is a file (`ERROR_DIRECTORY` for a rename). Guards for the last two were written and then taken out, when injecting faults showed they did nothing.
  - **Allocation seems to be rounded up to 4096 before it reaches us:** allocating 4 bytes on a 10-byte file does nothing, while allocating 4096 on a 10,000-byte file cuts it there. Seen from the outside only.
  - **Setting only times arrives with attributes `u32::MAX`,** not 0, going by a fault that read them as a change and made files read-only.
- **Gotcha for tests:** `File::create` and `fs::write` on an existing file use `OPEN_ALWAYS` and then set the length, so they never reach `Overwrite`. Use `create_always` in `write_path.rs`. The first overwrite tests were testing something else because of this.
- **Choices the plan left open (now in §5.8):**
  - **Two remote handles per Windows handle at most,** one to read and one to write, rather than one opened for both. A program opening for both can use a file the server only lets it read or only write, with no retry. A file read and written through one handle costs two of the 112 a mount can hold. This settles M1-12a's question about a read-only handle for a file later written.
  - **Readers are marked rather than discarded.** A change sets a flag on each handle of the path, and the next read drops what it fetched, so a write never waits for a read. A `discard()` on the write path would have needed either a wait or a limit that could leave stale data behind.
  - **`Overwrite` is `set_attr(size = 0)` by path,** not `open(truncate)`: no remote handle is needed unless the program goes on to write.
  - **A name that exists in another case is taken,** for `Create` and `Rename`, and replacing it keeps the name already there. `Ambiguous` still means not found, as M1-04 decided.
  - **Allocation only shortens a file.** `GetFileInformationByHandle` shows the allocation from the size, not what was asked for.
- **For later tasks:**
  - **M1-12c:** the harness should check that closing a handle whose `Cleanup` failed leaves the mount usable. Open files are now also in `Adapter::open_files`, so **`stop` not calling `Close`** (noted in M1-12a) leaves them there too. They go with the `Adapter`.
  - **M1-15:** pass the profile's read-only setting to `VolumeConfig::read_only`.
  - **M2-01:** a remote handle lost on reconnect is in `OpenFile::reader` and `writer`, both of which can be taken and opened again, by the path that follows renames.
  - **M2-08:**
    - **Every `Write` waits for `fstat`, one more round trip,** on top of what spec §5.6 expects from having no write-behind. The size could be worked out instead, but a guessed size is the bug class to avoid, so this needs thought along with write-behind.
    - **Each `Create` lists the parent** to look for the name in any case, and each change empties the cached listing, so creating N files lists a directory N times. `Cleanup` adds an fsync per written file. Both will show in NFR-PERF-07.
    - Overlapping reads and `GetFileInfo` on one handle now take turns.
- **Known limits, not tested:**
  - **A callback that runs out of time can leak a remote handle:** if `open` was on its way, nobody closes what it returns. The same was true of reads. It lasts until the session ends, and counts against the limit of 112.
  - **A change through a symbolic link does not reach readers of its target,** until their 5 s limit.
  - **A create that fails after the server made the file** (`fstat` or `set_attr` failing) leaves the file there.
- **The tests are checked too:** 54 faults injected by script (kept out of the repository), then all run again on the code after the changes they led to. On the first pass 43 failed a test. Of the other 11:
  - 6 were test gaps, now closed: `Overwrite` not reaching readers (the test sought to 0, which resets read-ahead anyway), attributes `u32::MAX` read as a change, an empty change sent to the server, a taken name left for the server to refuse, and `GetFileInfo` and size changes not using the handle.
  - 2 were code that did nothing, now removed: the read-only target and file-as-parent guards.
  - 1 was a wrong claim: a comment said refusing `SetSecurity` would break `ReplaceFile`. It does not, which is why it is now refused.
  - 1 was a flag that made no difference, `written`, since the driver sends no `Cleanup` for an untouched handle. Removed.
  - 1 stays uncaught: never taking a closed file out of `open_files`. It is a small leak, and mounted tests cannot see it.
  - The last full run was before the flag went: 50 of 52 applied were caught, all but the leak and the flag. Removing the flag changed only `Cleanup` and a few lines beside it, so the 8 faults on those were run again and all were caught. That leaves 49 of 50 faults caught on the final code.
- **Next:** M1-12c.

## 2026-11-04: M1-12a `fsm-winfsp` read path (branch `feat/m1-12a-winfsp-read`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `clippy` and `test` for `fsm-vfs` and `fsm-winfsp` also pass in a `rust:1.99.1` container, where `fsm-winfsp` is empty. `fsm-winfsp` has 70 tests: 19 unit tests and 51 in `tests/read_path.rs`, which mount real drives through WinFsp, none `#[ignore]`d since building the crate needs WinFsp anyway. Those 51 passed 30 runs of 30 in a row. The workspace has 426 passing and 127 ignored. Of the ignored server tests only `the_limit_on_open_files_is_clean_*` was run, as that is all that changed in `fsm-sftp`. The exe size check was not run: `fsm.exe` does not use this crate yet. `Cargo.lock` gained `winfsp`, `winfsp-sys` and what they need.

- **Added:** `WinFspBackend` with `start`, `stop` and `suspect`, and `VolumeConfig`. All the read callbacks of spec §5.8. In `fsm-vfs`: `RemoteFs for Arc<F>`, `DeadlineFs::mark_suspect` and `FsError::TooManyOpenFiles`, which `SftpFs` now returns at its limit.
- **Not tried yet: a real `SftpFs` through a drive, and Explorer.** Everything here is `MemFs`, plus a test layer for links, delays and failures, through Win32 calls. M1-12c brings the SFTP harness; Explorer is H-06.
- **For the owner to check:**
  - **The security descriptor gives the mounting user alone full control,** as the plan and §5.6 say, rather than §5.8's Everyone from spike M0-1. `\\fsm\<slug>` can be reached by other users of the PC, who would act with this user's server login. §5.8 is updated. **SYSTEM is left out too,** so a service running as SYSTEM, some antivirus perhaps, cannot open files on the drive. Only this descriptor's form was checked, not another account being refused, and the manual checks of spike M0-1 ran with the old one, so H-06 should repeat them.
  - **`paste` (RUSTSEC-2024-0436) is ignored in `deny.toml`, with a reason.** This was the open item from M1-01. The narrow ignore was chosen over `unmaintained = "workspace"`, which would hide other crates' advisories. `cargo audit` only warns about it.
- **Found in WinFsp, none of it visible from the spike:**
  - **`ReadOnlyVolume` only sets a flag.** The driver still passes changes down, so every drive refuses them in its callbacks with `STATUS_MEDIA_WRITE_PROTECTED`. Opening for write succeeds, and the write fails, so that `MAXIMUM_ALLOWED` is not turned away. There is no `read_only` in `VolumeConfig` yet, since every drive is read-only until M1-12b.
  - **The driver upper-cases the name before `GetDirInfoByName`,** so the case asked for is gone. Only the listing can answer, and if several entries match, the first by name is shown. So with `FILE.TXT` and `file.txt`, a search for `file.txt` finds `FILE.TXT`, while opening it opens the right one.
  - **`FindNextFileW` after a search for one name reads the directory** through `ReadDirectory`, which follows every link in it.
  - **`FileInfoTimeout` other than `u32::MAX` means no Windows file cache,** so every read reaches us, and a closed handle reaches `Close` at once.
  - **A file's trailing backslash gets `ERROR_INVALID_NAME` from the driver,** so listing a file as a directory is 123, not 267.
- **Choices the plan left open (now in §5.8):**
  - **A callback has `op_timeout_secs` in all.** Its limit can run out just before `DeadlineFs`'s, so it marks suspect itself.
  - **After a failed read the reader drops what is in flight,** or the next read met the same failure moments later. Waiting for that adds at most 50 ms to a callback, and only if a read overlaps. Strictly, that is over NFR-REL-03 by that much.
  - **`GetFileInfo` uses `fstat` once a remote handle is open,** so a growing file's size is not up to a TTL old.
  - **The file id is a hash of the path,** as `Attr` has no inode. It changes on rename.
  - **A broken link, or one the user may not follow, is listed as a file with the link's own details.** Only `Timeout` and `ConnectionLost` fail a listing.
  - **A name over 255 UTF-16 units is left out of listings;** a lone surrogate gets `STATUS_OBJECT_NAME_INVALID`.
- **For later tasks:**
  - **M1-12b:** the write callbacks are stubs returning write-protected. `OpenFile::path` is fixed for now and has to follow renames. Call `discard()` on every reader of a path after a change (see M1-06's note). A remote handle opened read-only for a file later written needs thought. `read_only` goes in `VolumeConfig` then.
  - **M1-12c:** map WinFsp's `STATUS_OBJECT_NAME_COLLISION` from `mount` to `MountError::LetterTaken`: it reads as `Other` now. **Check that `stop` closes what is open:** by reading the crate, `FspFileSystemDelete` calls no `Close`, so open `OpenFile`s and their remote handles may leak until the session ends. Not tested. A mounted drive's tests need `MAX_DRIVES` kept low: 24 threads ran out of letters.
  - **M1-18:** `fsm.exe` needs its own `build.rs` calling `winfsp_link_delayload()`. The one in this crate reaches only this package's tests.
  - **CI:** the Windows runner now needs LLVM and WinFsp's Developer feature just to build, and has to be able to mount (noted in `ci.yml`). The install steps are still for M1-12c.
  - **M2-08:**
    - A directory of 10,000 or more entries is never cached, so each lookup of a missing name, and each search for one name, lists all of it. Creating many files in such a folder will feel this.
    - One read has one `op_timeout_secs`. A large read on a slow link could run out, though `CopyFile` uses 1 MiB.
- **Known limit, not tested:** WinFsp's driver treats `notes` and `NOTES` as one name, so having both open at once may mix them up. The test reads them one after the other.
- **The tests are checked too:** 30 faults injected by script (kept out of the repository). 24 failed a test on the first pass. Of the other 6:
  - 2 were test gaps, now closed and re-run, and both fail a test: restarting a listing on one handle, and whether search by name is in use.
  - 1 was a second `start` on another letter. A test was added, but the prefix collides anyway, so the guard only gives a clearer message.
  - 3 stay uncaught and are harmless: reading a directory (the driver refuses first), not cutting the label (the crate cuts it too, and `fit_label` has its own tests), and not reporting end of file (`ReadFile` looks the same).
  - The other 24 were not run again after these last test changes. `cargo test` stops at the first failing binary, so a fault caught by a unit test did not show whether the mounted tests catch it as well.
- **Next:** M1-12b.

## 2026-11-04: M1-11 `fsm-win` helpers (branch `feat/m1-11-win-helpers`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `check` and `clippy` for `fsm-win` also pass in a `rust:1.99.1` container, where the crate is empty. `fsm-win` has 34 tests: 31 that always run and 3 `#[ignore]`d, which passed here with `cargo test -p fsm-win -- --ignored --test-threads=1`. The exe size check was not run: `fsm.exe` does not use `fsm-win` yet. No faults were injected to check the tests, as was done for M1-10. `Cargo.lock` gained `uuid` and the `windows` 0.61 family; `pageant` still brings in 0.62, so there are two versions (`cargo deny` only warns).

- **Added:** `credentials` (`store`, `load`, `delete`, `delete_all`, `SecretSlot`), `drives::is_letter_free`, `shell` (`notify_drive_added`, `notify_drive_removed`, `open_in_explorer`) and `winfsp::detect`.
- **Not shown: that `SHChangeNotify` reaches anyone.** The code follows the documentation, but the test only shows the call returns at once. A test listener (`SHChangeNotifyRegister`, on a hidden window, for all events, new and old delivery) received nothing, not even a flushed `SHCNE_UPDATEDIR` for a real folder from its own process, so it was removed. The cause was not found: it may be the listener, or something about how this session's processes run. Spike M0-1 also saw no effect from `SHChangeNotify`, from another process, so **M1-12c should not count on it**, and should try it in the running app, with H-06 as the check.
- **For the owner to check:**
  - **A secret over 1280 UTF-16 units cannot be remembered,** a limit of Credential Manager. Spec §6.2 now says so; `store` returns `InvalidInput`.
  - **If the Workstation service is stopped, every letter may read as taken.** FR-MNT-03 says anything but `ERROR_NOT_CONNECTED` is taken, so `ERROR_NO_NETWORK` is too, and that is what the code does. Not tried, as it needs admin rights. If it matters, the fallback from spike M0-1 is reading `HKCU\Network`.
- **Choices the plan left open:**
  - **Errors are `io::Error`,** with the Win32 code. M1-14's `SecretStore` can wrap them as it likes.
  - **`open_in_explorer` starts `explorer.exe` by full path,** as FR-MNT-12 says, rather than opening `Z:\` with the default folder handler, and because `ShellExecuteW` would look in the current directory first.
  - **A DLL with no version reads as `Missing`,** as there is one banner for missing and too old. WinFsp's DLL has the version in its file version (2.2.26286 here), not in its product version ("2026 Beta5").
  - **`is_letter_free` is only the definition.** Skipping A, B and C is the allocator's (M1-15).
- **For later tasks:**
  - **M1-14:** the Windows `SecretStore` is `credentials::{store, load, delete}`. They take a `Uuid`, which `fsm-win` now depends on.
  - **M1-15:** `DriveLetters` is `is_letter_free`, which takes one letter, so it is two system calls per letter.
  - **M1-16:** `winfsp::detect()` for the banner. `https://winfsp.dev/rel/` still needs a helper to open it: `shell` opens drives only.
  - **CI:** the credential tests are not `#[ignore]`d. They need a logon session with a loaded profile, and a Windows runner started as a service may fail with `ERROR_NO_SUCH_LOGON_SESSION`. If so, run it as a user. The 3 `#[ignore]`d tests run in no CI job.
- **Tests that touch this user's settings:** the credential tests use random UUIDs and remove their entries. `a_remembered_mapping_that_is_down_is_taken` writes `HKCU\Network\<letter>` for a moment and refuses a letter that has a key already. After the runs here, no `subst` drive, `FsManager` credential or extra mapping remained.
- **Gotcha:** `GetLogicalDrives` does list some disconnected mappings: `Z:`, marked disconnected by `net use` here, is in it. So the remembered-mapping test makes its own, on an unlisted letter.
- **Next:** M1-12a.

## 2026-11-04: M1-10 `SftpFs` (branch `feat/m1-10-sftpfs`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test` and both script checks pass on Windows; `fmt`, `clippy` and the server tests also pass for `fsm-sftp` in a `rust:1.99.1` container. `fsm-sftp` now has 220 tests on Windows: 96 that need no server and 124 that do, 27 of those in the new `tests/sftpfs.rs`. The server tests passed 3 runs of 3 in a row on Windows. The exe size check was not run: `fsm.exe` does not use `fsm-sftp` yet.

- **Added:** `fsm_sftp::fs::SftpFs`, `Connector for SftpConnector` with `probe`, `SftpSession::key_exchanges`, and `examples/sftpfs_throughput.rs`. The rules are in spec §5.7.
- **The contract suite passes, 1 GiB file included, on OpenSSH 9.2 and on ProFTPD 1.3.8 with every extension off.**
- **Throughput, 1 GiB, MiB/s, VM IP, medians of 3 unless noted.** All meet the done-when:

| | LAN now | LAN M0-2 | WAN now | WAN M0-2 |
|---|---|---|---|---|
| Download, `ReadAhead`, 1 MiB reads | 227 (5 runs, 210–279) | 230.7 (AES-GCM) | 50.8 | 49.6 (chacha20) |
| Upload, 1 MiB per call | 207 | 151.8 (chacha20) | 18.2 | 17.2 (chacha20) |

  - `SftpFs` negotiates AES-GCM, and M0-2's WAN and per-call rows used chacha20, so those two are not like for like. M0-2 has no per-call figure for AES-GCM.
  - LAN downloads are noisy: a first batch of 3 gave a median of 197. The spike's own binary, alternating with `SftpFs` on the same day, gave 190–341 with a median of 214, so the spread is not `SftpFs`'s.
- **Found in `russh-sftp` 3.0.1, none of it visible from the spike. For the owner to decide:** spec §12.1's fallback, our own small SFTP v3 client, would remove all three. It was not started: it is a change of plan, and everything here passes without it.
  - **File handles are decoded as lossy UTF-8.** OpenSSH's are 4-byte indexes, so the 129th open at once is garbled and useless (`russh_sftp_garbles_the_129th_openssh_handle`). **A mount can hold 112 files open, and the next open fails** with `FsError::Other`. The other 16 slots are for listings. Whether that is enough depends on M1-12a's lazy handles and on what programs do; robocopy with `/MT:128` could reach it. A server with binary handles from the start cannot be used at all, whatever its other merits (FR-SSH-15's "other SFTP v3 servers").
  - **A status code above 8 ends the whole session** (`russh_sftp_loses_the_session_over_proftpds_rename_refusal`). ProFTPD sends one for a rename onto a taken name, so a rename without `posix-rename` looks first, at the cost of 2 requests. Another client taking the name in between costs a reconnect, not data.
  - **Names that are not UTF-8 are lost.** This settles the question open since M1-02, as far as this crate allows: such a name is listed with U+FFFD and cannot be opened, renamed or removed. It stays listed, so the folder does not look empty when it cannot be removed.
- **Differs from the plan or spec, for the owner to check:**
  - **A rename by hand moves the target aside, rather than removing it first as §5.7 said.** Removing first breaks `RemoteFs::rename`'s promise, and loses the target if the source cannot move: `a_failed_rename_by_hand_gives_the_target_back`. §5.7 is updated.
  - **Windows servers are not detected,** though §5.7 said they would be. A check on `realpath` turned out to change nothing, so it was removed: a server that is not Windows has no `/C:/data`, which is E-PATH-NOT-FOUND either way.
  - **`Limited` is not `ConnectionLost`,** though the plan says every error that is neither a status nor a timeout is. It comes from this side, and a reconnect would not help.
  - **`keepalive_max` is 2,** as the plan says, now that M0-2's 60 s is confirmed: 3 intervals, 45 s at the default. Only 2 keep-alives are sent in that time, which reads differently from FR-SSH-12's "three unanswered"; the 60 s criterion is what it meets.
- **Choices the plan left open:**
  - **`SftpFs`'s `/` is the profile's remote path,** since the trait has no way to hand over a root. M1-12a just maps `\a\b` to `/a/b`.
  - **`request_timeout_secs` stays at `op_timeout_secs` for the session,** so the backend's limit and `DeadlineFs`'s are the same. Time spent waiting for the in-flight limit counts only against `DeadlineFs`.
  - The in-flight limits are per handle and shared by reads and writes. `ReadAhead` stays within them by itself, so they matter only for overlapping calls.
  - Read and write access is checked here before anything is sent, so every server behaves the same. Flushing a read handle does nothing, as it has nothing to flush; this also avoids asking a server to fsync a handle it may not allow that on. Not tried on Windows OpenSSH.
- **For later tasks:**
  - **M1-12a:** show a link as its target by `stat`ing each `Symlink` entry of a listing (§5.6); `read_dir` reports the link, as the trait says. Opening a directory as a file is untested and may succeed on some servers, so stat first. A file that cannot be opened because of the limit should read as "too many open files".
  - **M2-01:** `probe` needs `connect` to have run, and reads as `ConnectionLost` after the file system is dropped. An open abandoned by a deadline stays open on the server until the session ends, using up a slot, and enough of those make handles garbled, which are then refused.
  - **M2-08:** per-call uploads on WAN are still 18 MiB/s, as OD-4 expected. Through a jump host is still unmeasured.
  - **§10.4:** **real Windows OpenSSH has never run.** `sshd-winpaths` shows the paths as Windows does, on Linux, and that is all. OpenSSH 7.4 has not run either.
- **Rekey:** tested both ways. The server asks after every MiB in `a_transfer_survives_rekeying`, which requires at least 19 rekeys in 64 MiB, and `russh` asks after 1 GiB in the OpenSSH contract run. Pipelined requests pass through both, and the data comes back the same.
- **S-03 changes (run `servers.ps1 up`):** `proftpd` on 2230, `sshd-rekey` on 2231, `sshd-winpaths` on 2232; `/srv/fixture` and a 1 MiB `tmpfs` at `/srv/small` on `sshd`. CI's `integration` job needs no change.
- **Gotchas:**
  - `TimeoutIdle 0` made `mod_sftp` drop every connection at once, logging polls that timed out after 0 s, rather than turning the limit off. It cannot log to `/dev/stderr` either, as that is a link.
  - Through the Windows `localhost` relay, the 1 GiB case on ProFTPD takes many minutes: 32 KiB writes one at a time meet M0-2's stall. Set `FSM_TEST_HOST` to the VM's IP.
- **The tests are checked too:** 33 faults injected by script (kept out of the repository). All 33 seemed to fail a test, but **that count was wrong**: one fault left the `tmpfs` full, and `a_full_disk_is_no_space` then failed in every later run, so 4 faults had no other test behind them. That test now clears the disk first, and those 4 and the first were run again. Of the 4: 1 was caught, 1 was a test gap, now closed (`a_probe_in_flight_when_the_connection_ends_says_so`), and 1 was the Windows check above, now removed. The last stays uncaught and is harmless: not stopping for a missing source in a rename by hand, as the target moves aside and back and the error is the same. The other 28 were not run again after these changes.
- **Not measured:** memory, including against NFR-PERF-04. NFR-PERF rows stay open until M2-08.
- **Next:** M1-11.

## 2026-11-04: M1-09 jump host (branch `feat/m1-09-jump-host`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, the server tests and both script checks also pass for `fsm-sftp` in a `rust:1.99.1` container. `fsm-sftp` now has 173 tests on Windows (27 unit, 45 `auth`, 7 `auth_script`, 35 connector, 28 `jump`, 22 `known_hosts`, 9 against OpenSSH). The server tests passed 3 runs of 3 in a row on Windows. `Cargo.lock` did not change.

- **Added:** `SftpProfile::jump` with `JumpHost`, and `SftpConnector::take_remembered_jump`. The jump host is a second `SftpConnector` inside the first, so it gets the same key check and login. The rules are in spec §5.7.
- **For the owner to check:**
  - **A jump host's key passphrase cannot be remembered.** NFR-SEC-01 has `jump-password` but no `jump-passphrase`, so it is asked for once per mount, without *Remember*. Adding a target would be a small change if that is wrong.
  - **A jump host that does not allow forwarding reads as E-HOST-UNREACHABLE for the target**, and its hint to check the network does not fit that case. The reason is in the log. §7.8 has no better code; a new one could be added.
  - **`Auth::Ask` works on a jump host**, although FR-SSH-11 lists only agent, key file and password. Nothing prevents it at this level; the profile format (`agent | key | password`) and M1-17 do not offer it.
- **Found in `russh`:** a channel holds its own sender to the session, so dropping the jump host's `Handle` does not end the connection. The tunnel keeps it going until the target's session ends. `SftpSession` needs no field for it; `the_jump_connection_stays_for_the_session_and_ends_with_it` shows if that changes at a `russh` upgrade.
- **For later tasks:**
  - **M1-15:** after a successful connect, also call `take_remembered_jump` and store the result as `FsManager:<uuid>:jump-password`. Pass a stored one as the `JumpHost`'s `Auth::Password`.
  - **M1-17:** the password dialog names the host, so it already shows which of the two is asked for. `known_hosts` dialogs arrive in order: jump host, then target.
  - **M2-08:** throughput through a jump host was not measured, and data is encrypted twice. It does pass: 40 MiB both ways in `more_than_a_channel_window_of_data_passes_both_ways`.
- **S-03 changes (run `servers.ps1 up`; existing keys are kept):** `sshd-bastion` on 2228, `sshd-target` with no published port and an RSA host key, `sshd-noforward` on 2229. All are in the existing compose file rather than a separate one, so CI's `integration` job needs no change.
- **Known limits:**
  - **A target that accepts TCP and then stays silent is untested through a jump host**, since no test server behaves that way on the Docker network. It uses the same handshake limit as a direct connection, tested in `tests/connector.rs`.
  - A jump host that hangs while opening the channel reads as E-HOST-UNREACHABLE for the target, not E-TIMEOUT for the jump host: the two cannot be told apart.
- **The tests are checked too:** 15 faults injected by script (kept out of the repository). 11 failed a test. Of the other 4, 2 showed that a `jump` field on `SftpSession` did nothing, so it was removed, and that led to the two tests that the jump connection ends with the session or a failed login. 1 was a test gap, now closed and re-run: the jump host's own timeout. The last stays uncaught and is harmless: turning off the jump host's keep-alive, because the target's keep-alives cross the same link and notice the same stall.
- **Next:** M1-10.

## 2026-11-04: M1-08b SFTP connector: authentication (branch `feat/m1-08b-sftp-auth`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and the server tests also pass for `fsm-sftp` and `fsm-vfs` in a `rust:1.99.1` container. `fsm-sftp` now has 143 tests on Windows (27 unit, 43 `auth`, 7 `auth_script`, 35 connector, 22 `known_hosts`, 9 against OpenSSH), of which 70 need the S-03 servers. Two more run on Linux only. The server tests passed 3 runs of 3 in a row on Windows.

- **Added:** `fsm_sftp::connector::auth`, behind `log_in`: `Auth::{Password, KeyFile, Agent, Ask}` and `AgentSource`. `SftpConnector::take_remembered`. `Prompter::login`, with `LoginRequest` and `LoginAnswer`, for FR-SSH-05's prompt, which `secret` could not express. The rules are in spec §5.7.
- **Not run: Pageant and the real Windows OpenSSH agent.** **The owner should try both at M1-19.**
  - The Windows agent's service is disabled on this PC. `ssh-agent -d` needs admin rights (`cannot create agent root reg key`), and enabling the service is a system setting, so nothing was changed.
  - PuTTY is not installed. Pageant's pipe name comes from a private function of the `pageant` crate, so it cannot be faked.
  - What did run: an in-process `russh` agent on a real named pipe on Windows; real `ssh-agent` on a Unix socket on Linux, with keys and certificates; `AgentSource::System` once on this PC with neither running (E-AUTH in 0.1 s, test not kept); and a unit test of the order.
- **Differs from the plan or the spec, for the owner to check:**
  - **Two kinds of key file are E-KEY-FORMAT, though FR-SSH-02 says PEM:** encrypted legacy `EC PRIVATE KEY` PEM (`russh` decrypts it, then reads it as RSA), and encrypted PKCS#8 with PBKDF2-HMAC-SHA1, which is what `ssh-keygen -m PKCS8` writes **on Windows** (LibreSSL). `ssh-keygen`'s defaults are unaffected, and PKCS#8 from OpenSSL works. Both would otherwise show a right passphrase as wrong. RSA PEM with AES-256 (`openssl rsa -aes256`) should land in the same place, by reading the code; not tried. Supporting them means crypto dependencies of our own.
  - **`.ppk` is recognised by content, not by file name** here. A real OpenSSH key that happens to be named `.ppk` works. M1-13 and M1-17 can still look at the name for FR-SSH-03's hint.
  - **The first `password` prompt of a *Password* profile with nothing stored uses the password dialog, not the server's text**, as FR-SSH-07 reads, so that §7.4.1's *Remember* can be offered.
  - **A wrong passphrase is E-AUTH with no second prompt**, like a wrong password. `ssh` asks three times. The spec does not say.
- **Choices the plan left open (now in spec §5.7):** the server's method list decides what is tried. A refused key never leads to a password prompt, and a refused password is not sent again through keyboard-interactive, because a second failure can count against a lockout. After a partial success, a profile continues with the password method or keyboard-interactive.
- **Found in `russh`, none of it visible from the spike:**
  - **`authenticate_openssh_cert` names an RSA certificate with SHA-1**, so OpenSSH refuses it. `authenticate_certificate_with` and a small local signer (`KeySigner`) do it properly.
  - **Encrypted PKCS#8 without a passphrase is not `KeyIsEncrypted`**, but a parse error. The header is checked first.
  - **The unsupported PKCS#8 algorithm's error type is out of reach** without a direct dependency on its crate, so it is matched in the `Debug` text. **At each `russh` upgrade:** the fixture test `pkcs8_with_a_sha1_key_derivation_…` shows if that stops working. The failure would be E-AUTH instead of E-KEY-FORMAT, not a security problem.
- **For later tasks:**
  - **M1-15:** after a successful connect, call `take_remembered` and store the result (`FsManager:<uuid>:password` or `:passphrase`, by the profile's method). Keep one `SftpConnector` for a whole mount: its memory is what makes a reconnect silent, and dropping it is what clears the key (NFR-SEC-03). A secret that has already been stored is not reported again.
  - **M1-09:** the jump host needs `Auth` for its own login. `log_in` takes a `Handle` and reads the connector's profile, so the jump host will need its own connector or a split.
  - **M1-17:** `allow_remember` is decided here, so the dialog only follows it. `Prompter::login` is new and needs its dialog.
- **S-03 changes (run `servers.ps1 up`; existing keys are kept):** five more login keys, of which two have a passphrase; an RSA certificate; a key whose certificate has the wrong principal; a 1024-bit RSA key; users `keytotp` (key, then keyboard-interactive) and `kbdpass` (keyboard-interactive, then password). See `tests/docker/README.md`.
- **Known limits:**
  - **A long dialog can outlast the server's `LoginGraceTime`**, as with the trust dialog. A passphrase is kept once it has opened the key, so mounting again does not ask for it. A typed password is not kept, so it is asked again.
  - **An agent holding more keys than `MaxAuthTries` (6 by default) can get the connection closed before the right key is tried, which reads as E-CONN-LOST**, not E-AUTH, so a reconnect would keep retrying. `IdentitiesOnly` has no counterpart here.
  - Passwords and keyboard-interactive answers pass through `russh` as plain `String`s, whose copies are not zeroed. Decrypted keys rely on `ssh-key` zeroing them on drop; that was not checked.
  - **That RSA logins avoid SHA-1 rests on the server:** OpenSSH 9.2 refuses `ssh-rsa` by default, so a login proves it. The traffic was not captured.
- **The tests are checked too:** 49 faults injected by script (kept out of the repository). 37 failed a test on the first pass. Of the other 12, 11 were test gaps, now closed and re-run, and all fail a test. The last one was the `.ppk` file name check, which nothing depended on: removed. Closing the gaps needed `auth_script.rs`, an in-process `russh` server that follows a script, because OpenSSH will not ask a question before the password, or ask for ever. Before that, the tests found two real bugs: *Ask each time* stored a passphrase if the dialog said *Remember* anyway, and encrypted PKCS#8 was reported as a bad format.
- **Next:** M1-09.

## 2026-11-04: M1-08a SFTP connector: transport and host keys (branch `feat/m1-08a-sftp-connector`)

**Status: done locally; the CI workflow has still never run** (H-02, S-05). `fmt`, `clippy`, `deny`, `audit`, `build`, `test`, the size check and both script checks pass on Windows; `fmt`, `clippy`, `test` and the server tests also pass for `fsm-sftp` and `fsm-vfs` in a `rust:1.99.1` container. The script checks ran under WSL. `fsm-sftp` now has 78 tests (12 unit, 35 connector, 22 `known_hosts`, 9 against OpenSSH), of which 28 need the S-03 servers and are `#[ignore]`d. The server tests passed 4 runs of 4 in a row on Windows. The size check says nothing yet: `fsm.exe` is still empty (0.20 MB). `Cargo.lock` gained `russh`, `russh-sftp` and about 150 crates they need.

- **Added:** `fsm_sftp::connector`: `SftpConnector::new(SftpProfile)`, `connect_session(&dyn Prompter)` and `SftpSession` (`root`, `limits`, `has_extension`, `sftp`, `is_closed`). Also `ConnectError::NoCommonAlgorithm` and code E-ALGORITHMS, because spec §8 wants a clear message and §7.8 had none; **the owner may want to reword its text.**
- **Decided by the owner: RUSTSEC-2023-0071 is ignored, with a record** ([ADR-0004](adr/0004-rustsec-2023-0071.md)), in `deny.toml` and `.cargo/audit.toml`. Revisit at each `russh` or `rsa` upgrade. The `paste` and Linux-only advisories in the M1-01 notes still arrive with `winfsp` and `slint`.
- **Differs from the plan:**
  - **`Connector` is not implemented yet.** It must return an `Arc<dyn RemoteFs>`, and `SftpFs` is M1-10. **M1-10:** implement it, with `probe`, around `connect_session`.
  - **Logging in supports a given password only** (`Auth::Password`), since nothing past the login can be tested without one. **M1-08b:** add to `Auth`; `log_in` is the one place to change. Prompting and *Remember* are not there. `russh` takes the password as a plain `String`, so its copy is not zeroed.
- **Found in `russh`, none of it visible from the spike:**
  - **The handler must be `'static`, so it cannot hold the `&dyn Prompter`.** It sends the key over a channel to the future that holds the prompter. This also keeps dialog time out of the timeout, and dropping that future answers "no".
  - **`request_subsystem` does not wait for the reply**, though it takes `want_reply`. The spike's code would hang on a refused subsystem until the request timeout. The connector waits for `Success` or `Failure`.
  - **The default host key list includes SHA-1 `ssh-rsa`**, against spec §8, so the list is set here. An RSA key on file is still called `ssh-rsa` in `known_hosts`, and maps to `rsa-sha2-512` and `-256`.
  - **`check_server_key` runs for the first key exchange only**, so a rekey is not checked against the key that was trusted.
- **Certificate branch settled:** host certificates are not requested, and one that arrived anyway would be refused. That path cannot be reached with a test.
- **Choices the plan left open (now in spec §5.7):** each stage gets `op_timeout_secs`, since the spec gave no connect timeout. No TCP in time is E-HOST-UNREACHABLE and silence after TCP is E-TIMEOUT. *Trust once* also reads the file again, like *Trust and save*.
- **Verified on the wire, once by hand:** `curve25519-sha256`, `aes256-gcm@openssh.com`, and `ssh-ed25519`, or `rsa-sha2-512` with an RSA key on file. The test suite checks only the preference lists. The negotiated names are logged at Debug.
- **For later tasks:**
  - **M1-09:** `handshake` is generic over the stream, so it can run over a `direct-tcpip` channel. `connect_session` still needs splitting for that, and each hop needs its own key check.
  - **M1-10:** **M0-2's 60 s is confirmed in code and by test:** `russh` closes when the count passes `keepalive_max`, so a dead link is noticed 4 intervals after the last reply (`unanswered_keep_alives_close_the_connection`: about 4 s at 1 s). That is on FR-SSH-12's limit at the default 15 s. `keepalive_max = 2` means 3 intervals, but only 2 keep-alives are actually sent, which reads differently from "three unanswered". Decide there.
  - **M1-10:** `request_timeout_secs` is set to the connect timeout and stays for the session; choose the value for file operations. Windows path normalisation is not here: `C:/data` is E-PATH-NOT-FOUND for now, and FR-SSH-13's Windows case is untested. Non-UTF-8 names are still open.
  - **M1-10:** add ProFTPD: **the path for a server without `limits@openssh.com` has never run**, because every S-03 server has it.
  - **M1-17:** the dialogs can use `ChangedHostKey` and `RevokedHostKey` as they are. Set `known_hosts` to `%USERPROFILE%\.ssh\known_hosts`.
- **S-03 changes:** five more services from the same image, on ports 2223 to 2227 (see `tests/docker/README.md`): no subsystem, two host keys, SHA-1 only, a subsystem that exits and one that hangs. A new RSA host key comes from `gen-keys.sh`. `MaxStartups 100`, because parallel tests could otherwise be dropped at random. **CI:** a new `integration` job runs the `#[ignore]`d `fsm-sftp` tests, including those from M1-07; its runner needs Docker Engine and Compose v2.
- **Known limits:**
  - **A long trust dialog can outlast the server's `LoginGraceTime`** (120 s by default). The user then gets E-CONN-LOST, but a *Trust and save* answer is already on file, so mounting again works. A retry could hide this; none was added.
  - A web server that answers with an error page and closes gives E-CONN-LOST, not E-SFTP-UNAVAILABLE: the two cannot be told apart without reading the stream ourselves.
  - A folder this user may not look at gives E-PATH-NOT-FOUND; §7.8 has no permission code.
  - `localhost` is the only name tried with two addresses, so the time-sharing between addresses is not tested with a dead first address that stays silent, only with one that refuses.
  - That no data follows a refused key rests on `russh` ending the session when the check says no. Tests check only the error; nobody captured the traffic.
- **The tests are checked too:** 42 faults injected by script (kept out of the repository). 37 failed a test. 3 of those were found only on the first pass as gaps and closed: a dead first address, a clean close rather than a reset, and a hung subsystem (new server variant). 5 stay uncaught:
  - Three are one redundancy: the setup limit and the SFTP request limit cover for each other. Removing **both** fails `a_subsystem_that_never_answers_times_out`, checked.
  - The limit after the dialog: the data that a stall could hold back has normally arrived already, so the login limit fires instead. Kept as a safety net.
  - Treating an abandoned call as trusted changes nothing observable, as the connection ends anyway.
- **Gotcha:** in PowerShell, `-match` ignores case, so `'0 failed' -match 'FAILED'` is true. It marked every fault as caught on the first pass; only the empty list of failing tests gave it away. Use `-cmatch`. Also, `2>&1` or `*>` on `servers.ps1` throws at the first line of `docker compose` output (the known 5.1 trap): run it in a child `powershell -File`.
- **Next:** M1-08b.

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
