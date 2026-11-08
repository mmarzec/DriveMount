# M1 exit verification (M1-19)

| | |
|---|---|
| Checklist | [IMPLEMENTATION_PLAN.md §9.2](../IMPLEMENTATION_PLAN.md) |
| Machine | Windows 10 Home 22H2, WinFsp 2.2.26286 (2026 Beta5), Rust 1.99.1; S-03 servers on Docker Engine in WSL2 Ubuntu-22.04 |
| Date | 2026-11-08 |
| Result | **Automated part: all items pass, locally.** CI has never run (H-02, S-05), so "in CI" is not met as written. **Manual part (H-07): items 1–5 checked by the owner, all fine after one fix (H7-1). Item 6, the Windows agent, passes** once the owner had started its service. One performance finding goes to M2-08 (§4). |

## 1. Automated items

All the tests named here are `#[ignore]`d, as they need the S-03 servers, WinFsp, or a desktop. Commands are in §3.

| §9.2 item | Result | Evidence |
|---|---|---|
| Each SSH auth method mounts and unmounts | **Pass** | `apps/drivemount/tests/mount_one.rs`, 15 of 15, each through `examples/mount_one.rs`: real manager, factory, SFTP and WinFsp, with a listing, a write, a read-back and an unmount checked. Password; key with and without a passphrase; RSA PEM with a passphrase; certificate (`certuser`); keyboard-interactive with a TOTP code (`totpuser`); keyboard-interactive then password (`kbdpass`); key, then password and code (`keytotp`); *Ask each time* with a password and with a key file; the real Windows agent (see below) |
| Unknown host key → prompt; *Trust and save* → `ssh.exe -o BatchMode=yes` accepts; changed key → refused | **Pass** | `an_unknown_host_key_is_refused_unless_the_script_says_and_a_saved_one_satisfies_ssh`: *Reject* exits 1 with "server key not trusted". `ssh.exe` (Windows OpenSSH, `StrictHostKeyChecking=yes`, only our file) says "Host key verification failed" before the save and "Permission denied" after it. It gets no login method, so the host check is all it can pass. The next mount asks nothing. `a_changed_host_key_is_refused_and_not_asked_about`: "server key has changed", no question, and the file is left alone |
| Jump-host profile mounts | **Pass** | `a_profile_behind_a_jump_host_mounts`: `sshd-target` through `sshd-bastion`, password on both. The target resolves only on the Docker network, and both host keys were asked about |
| 1 GB in and out, and 10,000 small files, through Win32; hashes match | **Pass** | `a_gibibyte_and_ten_thousand_small_files_go_in_and_out_unchanged` (`apps/drivemount/tests/real_drive.rs`), over OpenSSH at the WSL address. Copies use `CopyFileExW` (`std::fs::copy`). 1 GiB of pseudo-random data: in 8.4 s, out 10.5 s, SHA-256 equal. 10,000 files of 0 to 4,099 bytes: in 2,640 s (see §4), out 147 s, every SHA-256 equal |
| New file visible in the next listing | **Pass** | The same test lists the folder right after each copy: the GiB, then all 10,000 names. `mount_one` does the same for each mount |
| Kill `drivemount.exe` → no drive left | **Pass** | `killing_it_with_a_drive_up_leaves_no_drive` (`apps/drivemount/tests/drivemount_exe.rs`): the real binary mounts on V: from the window, through UI Automation, and is killed with `taskkill /F`. V: was free 139 µs later |
| Read-only profile rejects writes | **Pass** | `a_read_only_profile_refuses_the_write` (`mount_one`), `a_read_only_profile_gets_a_read_only_drive` (`real_drive`): `ERROR_WRITE_PROTECT` |
| `fsx` 10k and the `winfsp-tests` subset pass in CI | **Pass locally; CI has not run** | `crates/drivemount-winfsp/tests/fsx.rs`: 2 of 2 on `MemFs` and with `DRIVEMOUNT_TEST_SERVER=sftp`. `winfsp_tests.rs`: 2 of 2, over `MemFs` and over SFTP, less `tests/winfsp-tests-exclusions.txt`. The CI workflow has these steps but has no runner until H-02 |

**The agent** was first left to the owner: the Windows OpenSSH agent's service was *Disabled*, and `ssh-agent.exe -d` from a normal user fails with "cannot create agent root reg key, ERROR:5". The owner started the service as administrator, and `the_windows_agent_mounts` then passed: it puts `id_ed25519` in the agent through standard input (`ssh-add` refuses a key file others can read, which the repository's is), mounts `keyonly` with *SSH agent* and no secret given, and takes the key out again. Pageant is not installed here, so it was not tried.

Also run at the same time, all passing: `drivemount_exe.rs` 3 of 3, `real_drive.rs` 7 of 7, `fmt`, `clippy --workspace --all-targets -D warnings`, the SPDX and secret-format checks, and the workspace's non-ignored tests (949 passing, 158 ignored).

### Changes made for this task

- `examples/mount_one.rs`: `--answers-env` answers are now used in order across rounds, not from the start each round, so a server that asks for a password and then a code in two rounds can be answered.
- New tests: 8 in `mount_one.rs` (one replaces `an_unknown_host_key_is_refused_unless_the_script_says`), 1 in `drivemount_exe.rs` and 1 in `real_drive.rs`.
- `drivemount`'s dev-dependencies gain `hmac`, `sha1` and `sha2`, all already in the tree at the same versions.

Nothing in the product failed, so nothing in it was fixed.

## 2. Manual items (H-07), for the owner

About 45 minutes. Start the servers, then a release build of the app with your own settings:

```bash
tests\docker\servers.ps1 up
```

```bash
cargo run --release -p drivemount
```

The server is `tester@localhost:2222`, password `tester`. Its files go to the container, which `servers.ps1 down` throws away. [tests/docker/README.md](../../tests/docker/README.md) has the other users.

| # | Check | What to look for |
|---|---|---|
| 1 | **Add a profile in the UI, mount it, open it in Explorer, unmount it** | *Add*, fill in the above, *Test*, *Save*, then *Mount*. The drive opens with Ctrl+Enter or the menu. Unmount, and the letter goes from *This PC* |
| 2 | **Host-key prompt and changed-key dialog look right** | The first mount of a new profile asks about the key. To see the changed-key dialog, edit the `[localhost]:2222` line in `%USERPROFILE%\.ssh\known_hosts` so the key differs (swap one letter in the middle), mount again, then put the line back |
| 3 | **Explorer: drag and drop, rename, delete** | Drag a few files and a folder in and out, rename one, delete one (it goes for good, with no Recycle Bin, as on any network drive) |
| 4 | **Notepad save, Office save** | Open a `.txt` on the drive in Notepad, change it, save, open it again. The same with a Word or Excel file, if Office is installed. Office saves through a temporary file and a rename, which is what to watch |
| 5 | **The drive under *Network locations* with the profile name** | In *This PC*, with the network-drive icon. Leave it up a few minutes this time: the M1-12c check only saw it for moments |
| 6 | **The Windows agent** (done: the owner started the service, then I ran `the_windows_agent_mounts`) (FR-SSH-04, not in §9.2's list, but never tried) | Needs an administrator once: `Set-Service ssh-agent -StartupType Manual`, then `Start-Service ssh-agent`. As yourself: copy `tests\docker\keys\id_ed25519` somewhere private and `ssh-add` it. A profile for `keyonly@localhost:2222` with *SSH agent* should mount with no question. Pageant too, if you have it. Afterwards `ssh-add -D`, and set the service back to *Disabled* if you want |

Watch while doing these for a stall in Explorer of 10–20 s, which was seen twice in M1-12c and never explained (PROGRESS, 2026-11-05). If one happens, note the time: the log is in `%LOCALAPPDATA%\DriveMount\logs`.

Report anything wrong, and I fix what fails and run the affected part of §1 again.

### Found at H-07

| # | Item | What was wrong | Fixed |
|---|---|---|---|
| H7-1 | 1 | **Text boxes had no border, and some had invisible text, on the Add page,** with the theme at *System* and Windows in dark mode. Our colours tested `Palette.color-scheme == dark`, but under *System* it is `unknown`, which Fluent takes as "follow Windows". So the pages were light while Fluent drew its controls dark. `Theme.dark` now follows the background Fluent resolved (`Palette.background`), so the two always agree, live changes included. Test: `under_system_the_page_and_the_text_boxes_follow_windows_together` sets the system scheme dark, then light. It failed before the fix. Checked by screenshot under *System* (dark) and `--light` | 2026-11-08 |

Items 1 to 5 were otherwise fine (owner, 2026-11-08), and item 6 passed (§1).

## 3. How to run the automated part again

```bash
tests\docker\servers.ps1 up
```

```bash
cargo test -p drivemount --test mount_one -- --ignored --test-threads=1
```

`the_windows_agent_mounts` among them fails unless the `ssh-agent` service is running.

```bash
cargo test -p drivemount --test drivemount_exe -- --ignored --test-threads=1
```

The address is the WSL VM's, from `wsl hostname -I`, as the localhost relay stalls writes (`tests/docker/README.md`). This takes about 70 minutes, of which 44 are the small files (§4):

```bash
DRIVEMOUNT_TEST_HOST=172.23.105.89 cargo test -p drivemount --test real_drive -- --ignored --test-threads=1 --nocapture
```

```bash
cargo test -p drivemount-winfsp --test fsx
```

```bash
DRIVEMOUNT_TEST_SERVER=sftp cargo test -p drivemount-winfsp --test fsx
```

```bash
cargo test -p drivemount-winfsp --test winfsp_tests -- --ignored --test-threads=1
```

`drivemount_exe.rs` opens windows on the desktop for a few seconds and presses through UI Automation. It sends no keys.

## 4. Finding for M2-08: a new file costs a listing of its folder

Copying the 10,000 small files in took 264 ms a file on average, against 15 ms to copy each one out. A file copied into an empty folder takes 19 ms. Measured with 20 new files at each size, through the same drive:

| Entries already in the folder | New file | Reading an existing file |
|---|---|---|
| 0 | 16 ms | 1.3 ms |
| 500 | 29 ms | 2.7 ms |
| 1,000 | 59 ms | 3.8 ms |
| 2,000 | 111 ms | 4.1 ms |
| 4,000 | 209 ms | 3.9 ms |

So each new file costs about 50 µs for every entry already in its folder, and filling a folder is quadratic. **Why:** a name the exact `stat` does not find is looked up case-insensitively in the folder's listing (`Adapter::find_entry`, `crates/drivemount-winfsp/src/adapter.rs`). Every create then drops that listing from the cache (`CachedFs::open` → `name_changed`, `crates/drivemount-vfs/src/cache.rs`), so the next new name lists the whole folder again.

No §9.2 item fails on this: every hash matched. It does break **NFR-PERF-07** (10,000 × 4 KB with robocopy ≤ 1.3× `scp -r`), which M2-08 measures. The likely fix is for a create to add its name to the cached listing rather than drop the listing, while keeping NFR-REL-02 (next listing sees it) and the case-insensitive rules (spec §5.6). That changes the cache's invalidation rules, which have their own property tests, so it is not done in this task.
