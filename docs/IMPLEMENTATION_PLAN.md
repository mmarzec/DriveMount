# DriveMount: Implementation Plan

| | |
|---|---|
| Document | Implementation plan |
| Status | Draft 2: all implementation work is done by an AI coding agent (Claude) |
| Date | 2026-11-03 |
| Inputs | [SPECIFICATION.md](SPECIFICATION.md) (what to build), [ANALYSIS.md](ANALYSIS.md) (why) |

This plan turns the specification into ordered, testable tasks.
- Every task has an ID (`M1-08a`), a "done when" check, a **You** column saying what the project owner must do (if anything), and a status box.
- Requirement IDs (FR-…, NFR-…) refer to the specification.

---

## 1. Working model

### 1.1 Roles

| Role | Does |
|---|---|
| **AI agent** | Writes all code, tests, CI workflows, scripts and docs. Runs builds, tests and benchmarks. Updates this plan's status boxes and `docs/TRACEABILITY.md`. |
| **Project owner (you)** | Makes decisions (OD-1 to OD-5). Approves admin/UAC prompts and permissions. Runs the short manual checks the AI cannot do. Tests on real hardware (second PC, NAS). Owns accounts: GitHub, code-signing certificate. |

### 1.2 What the AI cannot do alone

The AI works through a terminal and files. In this setup it cannot see the screen. Anything that needs eyes, hands, admin rights or accounts becomes a **human checkpoint** (H-xx, §1.5). This plan keeps those checkpoints few and short:

- **Automate first.** Anything checkable by script must be scripted, so the AI can verify it itself:
  - file integrity (hashes, `fsx`);
  - Win32 file operations;
  - WinFsp's test suite;
  - benchmarks;
  - log assertions;
  - UI logic through Slint's testing back-end.
- **Observable behaviour.** Every state transition is logged (spec NFR-MAINT-04), so the AI can verify reconnect, tray and lifecycle behaviour from logs after a manual step.
- **Short manual checklists.** Each checkpoint is a list that takes minutes, with a clear pass/fail per item. The owner reports the results back in the session.

### 1.3 Session protocol (work spans many AI sessions)

1. **Start:** read this plan (status boxes), `docs/TRACEABILITY.md` and the relevant spec sections. Pick the first unchecked task whose dependencies are done.
2. **Implement:** write the task's code and tests. Run `cargo fmt`, `cargo clippy -D warnings` and the test suites locally, the same steps CI runs.
3. **Close:**
   - tick the task's status box;
   - update `docs/TRACEABILITY.md`;
   - update the spec if behaviour differs;
   - add notes for the next session to `docs/PROGRESS.md`: decisions, open issues, anything half-done.
4. **Commit** each task on its own branch, as the owner allows: standing permission to commit to feature branches is recommended. The owner merges or approves pushes.

Tasks are sized to fit in one session, or a few. Large items are split (`a`/`b`/`c`).

### 1.4 State of the development machine (checked 2026-11-03)

| Item | State | Action |
|---|---|---|
| OS | Windows 10 Home 22H2, 24 logical CPUs | Also the main manual test target (PLAT-01) |
| Git | Installed | `C:\marcin\fs-manager` is not a repository yet → S-04 |
| Rust toolchain | **Not installed** | S-01 |
| MSVC build tools / Windows SDK | Not verified | S-01 |
| WinFsp | **2.2 beta installed** (`winfsp-x64.dll` 2.2.26286, "2026 Beta5"), runtime only: no Developer files, no test tools | S-02 |
| Docker | Docker Engine 24.0.6 + Compose v2.21 already inside WSL2 Ubuntu-22.04 (found during S-03); no Docker Desktop | S-03 ✅ |
| WSL | WSL 2 (3.0.3); Virtual Machine Platform enabled 2026-11-03; systemd on. VirtualBox 7.0.10 also used on this PC and must coexist with Hyper-V | S-03 ✅ |
| GitHub CLI (`gh`) | Installed, but unused: the remote will be the owner's LAN Gitea | S-05 |
| LLVM / libclang | LLVM 23.1.4 (`C:\Program Files\LLVM`), installed in M0-1: `winfsp-sys` runs `bindgen` on every build. CI runners need it too | M0-1 ✅ |

### 1.5 Human checkpoints

| ID | When | What you do | Time needed |
|---|---|---|---|
| H-01 | Setup | Approve UAC prompts for the Rust/VS Build Tools, WinFsp Developer and Docker/WSL installs; reboot if asked | ~30 min (mostly waiting) |
| H-02 | Later | Create the repository on the LAN Gitea instance, register Gitea Actions runners (Linux + Windows), give the AI a way to push | ~30 min |
| H-03 | Before M1 | ✅ **OD-1 decided 2026-11-03: GPLv3 open source** ([ADR-0003](adr/0003-licence-gplv3.md)) | done |
| H-04 | M0-1 | Explorer checklist on the spike drive (drag-drop, Office save, Network locations…) | ~20 min |
| H-05 | M0-3 | Check the tray, close-to-tray, toast click and sleep/resume on the UI spike | ~15 min |
| H-06 | M1-16/17 | Look at the main window and dialogs (100% and 150% scaling) and report anything wrong | ~20 min per round |
| H-07 | M1-19 | Manual items of the M1 exit checklist (§9.2) | ~45 min |
| H-08 | M2-02/03/04/06 | Sleep/resume, Wi-Fi switch, sign-out with mounts, tray and toast behaviour | ~45 min |
| H-09 | M2-09 | Start the 72 h soak; keep the PC on, and don't use it for anything else during the run | 3 days of wall clock |
| H-10 | M2-11 | **Decide OD-3** and obtain the code-signing certificate / Azure Trusted Signing account | account setup |
| H-11 | M2-10/13 | Install, upgrade and uninstall the MSI on a clean Windows 11 Pro machine (or VM) | ~30 min |
| H-12 | M3-05 | SMB tests against your real servers (NAS, Windows share) | ~30 min |
| H-13 | M4-06 | NFS tests against your real servers (NAS) | ~30 min |

### 1.6 Gates

Work does not pass a gate until its conditions hold.

| Gate | Before | Conditions |
|---|---|---|
| **G0** | M0 | Setup tasks S-01 to S-04 done (S-05 deferred until the Gitea remote exists) |
| **G1** | M1 | All three spikes are "go" (or a fallback is chosen); OD-1 licence decided ✅ (ADR-0003); OD-4 (write-behind) decided |
| **G2** | M2 | M1 exit checklist passes |
| **G3** | v1.0.0 release | v1 release checklist passes |
| **G4 / G5** | M3 / M4 release | Each milestone's exit criteria pass |

### 1.7 Refinement of the specification found while planning

**SC-1: crate layering.** Spec §5.2 places the `Prompter` trait and the mount manager in `drivemount-core`. But `drivemount-sftp` also needs `Prompter`, and `drivemount-core` would need to know every backend. That creates a dependency cycle and ties `drivemount-core` to Windows. The fix:

- The shared traits move to the lowest crate, `drivemount-vfs`: `RemoteFs`, `Connector`, `MountBackend`, `Prompter` and `Secret`.
- `drivemount-core` depends only on `drivemount-vfs`. It gets backends from a factory that the app supplies, so it stays platform-independent and its tests run on Linux.

```
drivemount-vfs ◀── drivemount-sftp, drivemount-nfs                 (portable)
drivemount-vfs ◀── drivemount-core                          (portable: profiles, config, MountManager)
drivemount-vfs ◀── drivemount-winfsp, drivemount-smb, drivemount-win      (Windows)
apps/drivemount ──▶ all of the above; BackendFactory: Profile → Box<dyn MountBackend>
```

Spec §5.2 now matches (M1-01).

---

## 2. Overall sequence and timing

```
Setup ─▶ M0 spikes ─▶ G1 ─▶ M1 core mount ─▶ G2 ─▶ M2 v1 release ─▶ G3 (v1.0.0) ─▶ M3 SMB ─▶ G4 ─▶ M4 NFS ─▶ G5
 H-01      H-04         H-03     H-06, H-07           H-08…H-11                      H-12          H-13
 H-02      H-05
```

**Rough calendar for AI-driven work:**

| Phase | Calendar (rough) | What sets the pace |
|---|---|---|
| Setup | hours | Your installs (H-01, H-02) |
| M0 spikes | 1–3 days | The Explorer and tray checks (H-04, H-05) |
| M1 core mount | ~1½–2½ weeks | Rust compile/test loops, WinFsp debugging, UI review rounds (H-06) |
| M2 v1 release | ~1½–2 weeks | 72 h soak (H-09), signing certificate (H-10), installer check (H-11) |
| **v1.0.0 (SSH)** | **≈ 4–6 weeks** | |
| M3 SMB + M4 NFS | ~1½–2 weeks | Tests on your real servers (H-12, H-13) |

These are rough ranges, not commitments:
- They assume checkpoints are answered within about a day.
- Waiting on checkpoints, wall-clock tests and accounts dominate the schedule more than writing code does.
- Low-level Windows file system work can need several debugging rounds.

**Critical path:** S-01 → M0-1 → M1-12a–c (WinFsp adapter) → M1-19 → M2-01 (reconnect) → M2-09 (reliability, soak) → M2-13 (release).
---

## 3. Setup

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| S-01 | **Rust toolchain:** install `rustup` with the stable `x86_64-pc-windows-msvc` target, then Visual Studio Build Tools with the "Desktop development with C++" workload (MSVC linker + Windows 10/11 SDK), both via `winget`. Then `cargo install cargo-deny cargo-audit cargo-about cargo-wix`. | `cargo new hello && cargo run` works; `link.exe` found | H-01: approve UAC | ☑ |
| S-02 | **WinFsp for development:** re-run the WinFsp installer with the **Developer** feature (headers, import libraries, `fsext`). Download the matching `winfsp-tests` zip. Keep the latest stable 2.1.x installer, so tests also run on the minimum supported version (PLAT-02). | `winfsp-tests-x64.exe --list` runs (needs `C:\Program Files (x86)\WinFsp\bin` on PATH; installers and tests kept in `C:\marcin\tools\winfsp`); Developer files present under `C:\Program Files (x86)\WinFsp\inc` and `lib` | H-01: approve UAC | ☑ |
| S-03 | **Test servers:**<br>• **Option A (recommended):** Docker Engine (`docker-ce` + compose plugin) inside the WSL2 Ubuntu distro, driven from Windows with `wsl docker compose …`, so the same `docker-compose.yml` runs locally and in CI. **No Docker Desktop** (owner's decision, 2026-11-03).<br>• **Option B:** WSL2 Ubuntu with `openssh-server`, set up by script (`tests/wsl/setup-sshd.sh`).<br>• Either way, add users for password, key, certificate and keyboard-interactive (TOTP) logins. | `sftp -P 2222 tester@localhost` works from Windows for each auth method. Done: `tests/docker/servers.ps1 up` / `check` (see `tests/docker/README.md`) | H-01: approve installs, reboot | ☑ |
| S-04 | `git init`; `.gitignore` (target/, *.msi); first commit of `docs/`. | Repository with `docs/` committed | Approve commit | ☑ |
| S-05 | **CI skeleton (deferred):** the repository stays local for now; it will later be pushed to the owner's LAN Gitea instance, not GitHub. The GitHub-Actions-compatible workflow now exists (`.github/workflows/ci.yml`, M1-01; Gitea Actions runs it). Running it needs self-hosted `act_runner`s, including one on Windows. | Both jobs green on Gitea | H-02: set up the Gitea repo and runners | ☐ |

---

## 4. M0: Spikes

Spikes are throw-away code in `spikes/<name>/`, outside the main workspace. Each one ends with a report in `docs/spikes/<id>.md` covering:
- what was tried;
- the measured numbers;
- the settings chosen;
- go / no-go;
- the consequences for the spec.

### M0-1: WinFsp in-memory drive (risk: "network-drive mode from an unelevated process")

**Status: ☑ Go (2026-11-03).** Report: [spikes/m0-1-winfsp.md](spikes/m0-1-winfsp.md). Explorer-notification and label issues moved to M1-12c.

1. Create `spikes/m0-1-winfsp`, depending on the `winfsp` crate.
   - Add `build.rs` with `winfsp::build::winfsp_link_delayload()`.
   - Find out whether the default (bundled) link mode works, or whether the `system` feature plus S-02's Developer files are needed.
2. Implement a minimal in-memory file system with `FileSystemContext`. It needs these callbacks:
   - metadata and security: `get_security_by_name`, `get_file_info`, `set_basic_info`, `get_volume_info`;
   - handles: `create`, `open`, `overwrite`, `cleanup`, `close`, `flush`;
   - data: `read`, `write`, `set_file_size`;
   - directories, rename and delete: `read_directory`, `rename`, `set_delete` / `can_delete`.
3. Mount it as `Z:` from a **normal (unelevated)** console with `VolumePrefix = \drivemount\spike`, then repeat without a prefix.
4. **Scripted checks (AI):**
   - create, rename and delete files and folders through Win32 calls;
   - copy in and out a 1 GB file and 10,000 small files with hash checks;
   - `robocopy /MIR`;
   - `ReplaceFile`;
   - kill the process while mounted and check with `GetLogicalDrives` that the letter is gone (NFR-REL-04);
   - `FileInfoTimeout` = 0, 1000 ms and 5000 ms, compared by operation counts from logging.
5. **Manual checks (H-04):**
   - the drive shows under *Network locations* with its label;
   - drag and drop;
   - Notepad save;
   - Word/LibreOffice save;
   - delete confirmation (network drives delete permanently);
   - thumbnails.
6. Record the final volume parameter set. This becomes spec §5.8.

**Go:** all checks pass unelevated in network mode, and the letter disappears on kill.

**Fallbacks:**
- if only network mode fails → plain drive mount (no prefix);
- if the crate itself is unusable → a Dokany (`dokan` crate) spike.

### M0-2: SFTP throughput with russh (risk: "`russh-sftp` maturity and throughput"), fully automated

**Status: ☑ Go (2026-11-04).** Report: [spikes/m0-2-sftp.md](spikes/m0-2-sftp.md). OD-4 decided: write-behind on, 4 MiB bound (built in M2-08). Performance tests must connect to the WSL VM's IP, not `localhost`.

1. Create `spikes/m0-2-sftp`: connect with `russh` (key auth) to the S-03 server and open the SFTP subsystem with `russh-sftp`.
   - Check that its **low-level request API** (offset-based `read`/`write` on a handle, many requests in flight) is available, e.g. a raw session type.
   - Otherwise, plan the fallback client.
2. Benchmark downloads and uploads of a 1 GB file:
   - **Variables:** chunk size (32 KiB / 64 KiB / max from `limits@openssh.com`) × requests in flight (1, 8, 32, 64).
   - **Upload modes:** (a) "per-call": 1 MiB calls, each acknowledged before the next, as WinFsp writes would be in v1; (b) a free-running pipeline.
3. Baseline: Windows OpenSSH `sftp.exe` get/put of the same file (default settings).
4. Repeat on a WAN profile with 50 ms delay on the server side: `tc qdisc add dev eth0 root netem delay 50ms` in WSL/Docker.
5. Also measure the cost of listing 1,000 entries and of `stat`.
6. Check `keepalive@openssh.com` handling and that dropping the TCP connection is detected promptly.

**Go:** ≥ 90% of OpenSSH on LAN and WAN (NFR-PERF-06).

**Decides OD-4:**
- if per-call writes reach ≥ 90% on WAN → no write-behind;
- otherwise → bounded write-behind (spec §5.6).

**Fallback:** our own SFTP v3 client directly on a `russh` channel. It adds one extra work package to M1.

### M0-3: Slint window + tray + Windows messages (risk: "Slint/tray integration")

**Status: ☑ Go with open items (2026-11-04).** Report: [spikes/m0-3-ui.md](spikes/m0-3-ui.md). Toasts need a registry AUMID, not a shortcut. Open: Action Center toast clicks (M2-04), real sleep/resume deferred by the owner (M2-02, H-08), idle-CPU bursts with the window visible (M2-08).

1. Create `spikes/m0-3-ui`: a Slint window (winit back-end, software renderer, Fluent style) with a list of 20 fake rows.
2. Tray icon and menu with the `tray-icon` crate on the UI thread. Events go to Slint through `slint::invoke_from_event_loop`, with no polling timers.
3. Close-to-tray: the close request hides the window, `run_event_loop_until_quit()` keeps running, and the tray "Open" item shows the window again.
4. Add a **hidden top-level window** (not message-only, because message-only windows do not get broadcasts). Log its `WM_POWERBROADCAST` (sleep/resume) and `WM_QUERYENDSESSION` messages.
5. Toasts: show a toast from an unpackaged exe. Confirm that a Start-menu shortcut carrying an AppUserModelID is needed, and that a click on the toast activates the window.
6. **AI measures:** cold start to first frame (logged), working set after 60 s, release exe size.
7. **Manual checks (H-05):** tray menu, close-to-tray, toast click, sleep/resume. The AI then confirms the expected messages in the log.

**Go:** all items work; start < 250 ms; working set < 40 MB; exe size leaves room under 15 MB (raised to 30 MB by the owner on 2026-11-08).

**Fallbacks:**
- raw `Shell_NotifyIcon` in `drivemount-win`;
- if memory is too high, try another Slint renderer.

**G1 checklist:**
- three spike reports written ✅;
- OD-1 (licence) decided ✅ GPLv3 (ADR-0003);
- OD-4 decided ✅ write-behind on (M0-2);
- spec updated with the spike results (volume flags, write mode) ✅.

---

## 5. M1: Core mount

**Goal:** a user can add an SSH profile in the UI, mount it as a drive, use it in Explorer, and unmount it. Reconnect, tray and installer come in M2.

### WP 1.A: Workspace and shared types

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-01 | **Workspace skeleton**:<br>• root `Cargo.toml` with `[workspace]`, `[workspace.dependencies]` (versions pinned) and `[workspace.lints]`: `unsafe_code = "deny"` except in `drivemount-win`/`drivemount-winfsp`; clippy `unwrap_used` and `expect_used` denied outside tests;<br>• `rust-toolchain.toml`, `rustfmt.toml`, `deny.toml` (licence allow-list from ADR-0003);<br>• licensing per NFR-LIC-01: `LICENSE` (GPLv3 text), `license = "GPL-3.0-or-later"` in every crate, SPDX headers, plus a CI check for missing headers;<br>• empty crates from spec §5.2 and the `apps/drivemount` binary (`windows_subsystem = "windows"`);<br>• CI jobs: `fmt`, `clippy -D warnings`, `deny`, `test`, and an exe size check (M0-3 leaves about 2.2 MiB of margin);<br>• `docs/TRACEABILITY.md` and `docs/PROGRESS.md`;<br>• apply SC-1 to spec §5.2. | `cargo build --workspace` and CI green on Windows and Linux. **Done locally:** all CI steps pass on Windows and in a Linux container; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-02 | **`drivemount-vfs` types and traits**:<br>• `RemotePath` (normalised POSIX path: join, parent, file_name, no `..` escapes);<br>• `Attr`, `DirEntry`, `OpenMode`, `AttrChanges`, `FsStats`, `Capabilities`;<br>• `FsError` (NotFound, PathNotFound, PermissionDenied, Exists, NotEmpty, NotSupported, ReadOnly, Timeout, ConnectionLost, NoSpace, Other);<br>• the traits `RemoteFs`, `Connector`, `MountBackend`, `Prompter`;<br>• `Secret<T>` (zeroize on drop, `Debug` = `***`).<br>Write **ADR-0001**: `async-trait` vs boxed futures for `dyn` traits. | Types compile; doc comments on all public items; ADR committed. **Done:** `async-trait` chosen ([ADR-0001](adr/0001-async-traits.md)); the compiler enforces doc comments (`deny(missing_docs)`) | – | ☑ |
| M1-03 | **`MemFs` and the contract test suite**:<br>• `MemFs` (in-memory `RemoteFs`, behind the `testing` feature);<br>• a generic async suite `drivemount_vfs::contract::run_all(fs, root)` covering spec §10.2: offsets, truncate, rename ± replace, mkdir/rmdir non-empty, remove, set times and the read-only bit, statfs, 0 B…1 GiB sizes. | Suite passes against `MemFs` on Linux and Windows. **Done locally:** passes on Windows and in a Linux container; the workflow itself first runs with S-05/H-02 | – | ☑ |

### WP 1.B: VFS behaviour (portable)

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-04 | **Name mapping**: Windows ↔ remote names. `\ : * ? " < > \|`, 0x01–0x1F and trailing dot/space map to U+F000 + code and back. Add a case-insensitive resolver over a listing: exact match first, then a unique case-insensitive match. | Property tests (`proptest`) prove round-trips; ambiguity tests. **Done locally:** passes on Windows and in a Linux container; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-05 | **`CachedFs<F>`**: a `RemoteFs` wrapper with attribute, directory and negative caches.<br>• Bounded size (10k entries).<br>• TTL from the profile; negative TTL 1 s, or 60 s for the noise names.<br>• Invalidation rules from spec §5.6.<br>• Tests use `tokio::time::pause()`. | Each invalidation rule has a test; NFR-REL-02 test passes on `CachedFs<MemFs>`. **Done locally:** passes on Windows and in a Linux container; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-06 | **`DeadlineFs<F>`** and **read-ahead**.<br>• **Deadline wrapper:** applies `op_timeout`, maps expiry to `FsError::Timeout`, and signals "suspect" on a `watch` channel.<br>• **Read-ahead:** a per-handle reader that detects sequential access, grows its window from 128 KiB to 4 MiB, and splits reads into chunks with up to 64 in flight. | Tests: a slow `MemFs` produces Timeout at the deadline; a sequential read issues the expected chunk pattern. **Done locally:** passes on Windows and in a Linux container; the workflow itself first runs with S-05/H-02 | – | ☑ |

### WP 1.C: SFTP backend (portable; tests against Docker/WSL servers)

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-07 | **`known_hosts` module**:<br>• parse plain, hashed (`\|1\|` HMAC-SHA1), `[host]:port`, multiple keys per host and `@revoked`;<br>• match → Known / Unknown / Changed{line} / Revoked;<br>• append with a lock file; follow the existing file's hashing style;<br>• SHA256 fingerprint formatting identical to `ssh-keygen -l`. | Fixture tests, including one file shared with `ssh.exe`. **Done locally:** passes on Windows and in a Linux container, and real `ssh.exe` agrees on one file against the S-03 server; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-08a | **SFTP `Connector`: transport and host keys**:<br>• DNS resolution (→ `Dns` error), TCP connect with a timeout;<br>• the `russh` client config: algorithms per spec §8, channel window ≥ 16 MiB, keep-alive;<br>• `check_server_key` → known_hosts → `Prompter::host_key`; ask the server for the key types on file first (`KnownHosts::known_key_types`), as `ssh` does, or a server with a newer key type looks changed;<br>• SFTP subsystem (→ `SftpUnavailable`), `limits@openssh.com`, `realpath` for `~`;<br>• every failure classified into the §7.8 codes;<br>• settle RUSTSEC-2023-0071 with the owner: `russh`'s `rsa` feature makes `cargo deny` and `cargo audit` fail (M1-01, see `docs/PROGRESS.md`). | Integration tests for each error class and each host-key outcome against the S-03 server. **Done locally:** passes on Windows and in a Linux container against the S-03 servers, with [ADR-0004](adr/0004-rustsec-2023-0071.md) settling the advisory. `Connector` itself waits for `SftpFs` (M1-10), and login is a given password until M1-08b; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-08b | **SFTP `Connector`: authentication**:<br>• agent: `\\.\pipe\openssh-ssh-agent`, then Pageant;<br>• key file: OpenSSH/PEM; passphrase via `Prompter`; `-cert.pub` detection; `.ppk` → `KeyFormat`;<br>• password;<br>• keyboard-interactive, with automatic answering of password prompts. | Integration tests for each auth method (password, key, passphrase key, agent, certificate, TOTP). **Done locally:** passes on Windows and in a Linux container against the S-03 server. The agent tests use an in-process agent on a named pipe on Windows, and real `ssh-agent` on Linux; Windows' own agent and Pageant have not run (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-09 | **Jump host**: connect to the jump host, open a `direct-tcpip` channel, run the target session over the channel stream. Each host key is verified separately. | Test with a two-container compose file (bastion + target). **Done locally:** passes on Windows and in a Linux container against the S-03 servers, with bastion and target services in the existing compose file; the workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-10 | **`SftpFs`** (`RemoteFs` over SFTP):<br>• a handle table;<br>• pipelined chunked read/write (sizes from M0-2: 64 chunks and at most 4 MiB in flight);<br>• extensions with fallbacks: `posix-rename`, `fsync`, `statvfs` (1 PiB when absent);<br>• Windows OpenSSH path normalisation (`/C:/`);<br>• SFTP status → `FsError` mapping: every error that is neither a status nor a timeout is `ConnectionLost` (russh-sftp reports dead sessions as varying strings);<br>• `keepalive_max` = 2 if M0-2's 60 s for a hung server is confirmed (FR-SSH-12);<br>• a test that a rekey during a transfer works. | Contract suite passes against OpenSSH and ProFTPD `mod_sftp`; throughput within 5% of the M0-2 numbers. **Done locally:** passes on Windows and in a Linux container against the S-03 servers; throughput measured on Windows only (see `docs/PROGRESS.md`). Real Windows OpenSSH has not run, and `russh-sftp`'s limits need the owner's decision; the workflow itself first runs with S-05/H-02 | – | ☑ |

### WP 1.D: Windows layer

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-11 | **`drivemount-win` helpers** (windows-rs):<br>• Credential Manager wrapper (`CredWriteW`/`CredReadW`/`CredDeleteW`, targets from NFR-SEC-01) returning `Secret`;<br>• drive-letter queries (`GetLogicalDrives` + `WNetGetConnectionW`);<br>• `SHChangeNotify` drive add/remove;<br>• open Explorer (`ShellExecuteW`);<br>• WinFsp detection (registry `InstallDir` + DLL file version ≥ 2.1). | Windows-only tests: credential round-trip and delete; letter detection with a test `subst` drive; version parse. **Done locally:** passes on Windows, with the crate empty and building in a Linux container. `SHChangeNotify` reaching a listener could not be shown (see `docs/PROGRESS.md`); M1-12c settles what Explorer needs. The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-12a | **`drivemount-winfsp` adapter: read path**:<br>• `WinFspBackend` skeleton over `Arc<dyn RemoteFs>` wrapped as `CachedFs<DeadlineFs<…>>`;<br>• `block_on` with a deadline on the dispatcher threads;<br>• per-file context (path, lazily opened handle, is-directory, read-ahead state);<br>• `get_volume_info`, `get_security_by_name` (fixed security descriptor: full control for the current user), `open`, `read`, `get_file_info`, `read_directory`, `get_dir_info_by_name`, `close`;<br>• NTSTATUS mapping table. | Mounted `MemFs`: listing, reading and attribute queries pass scripted Win32 tests. **Done locally:** passes on Windows, with the crate empty and building in a Linux container. Every drive is read-only until M1-12b, and a real `SftpFs` through a drive waits for M1-12c (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-12b | **`drivemount-winfsp` adapter: write path**: `create`, `overwrite`, `write`, `flush`, `set_basic_info`, `set_file_size`, `can_delete`/`set_delete`, `cleanup` (delete-pending, flush), `rename`. | Mounted `MemFs` passes CopyFileEx, MoveFileEx, ReplaceFile, delete and attribute tests, and `fsx` 10k operations. **Done locally:** passes on Windows, with the crate empty and building in a Linux container. `fsx` is a Rust version with the same operations (`crates/drivemount-winfsp/tests/fsx.rs`), as it has no ready Windows build. A real `SftpFs` through a drive waits for M1-12c (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-12c | **`drivemount-winfsp` adapter: mount lifecycle and test harness**:<br>• volume parameters from M0-1; `MountBackend::mount`/`unmount`; thread count from settings;<br>• a Windows test harness that mounts `MemFs` and an in-process `russh` SFTP server;<br>• the agreed `winfsp-tests` subset, with exclusions and reasons in `tests/winfsp-tests-exclusions.txt`;<br>• the same tests wired into CI;<br>• **Explorer integration** (spike M0-1 §7): make a running Explorer show and hide the drive (try `WNetAddConnection2W` via `WinFsp.Np` first; never a synchronous broadcast), and the profile name as the Explorer label (`_LabelFromReg`). | Harness and `winfsp-tests` subset green locally and in CI; owner confirms a running Explorer shows "Name (X:)" within 1 s, without restarting Explorer. **Done locally:** passes on Windows, and the owner confirmed Explorer on 2026-11-05 through `tests/explorer.rs`. Hiding takes about 1.2 s, which the owner accepted (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | H-06 | ☑ |

### WP 1.E: Engine (portable)

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-13 | **Profile model and validation**:<br>• `Profile { id, name, protocol: Sftp(SftpProfile) /* Smb, Nfs later */, flags… }`;<br>• all spec §7.3 rules as pure functions returning field-level errors;<br>• paste parsing (FR-PROF-05). | One unit test per validation rule; paste-parsing tests. **Done locally:** passes on Windows and in a Linux container. The dialog that uses it is M1-17 (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-14 | **`ConfigStore`**:<br>• `config.toml` load and save with serde/toml;<br>• `schema_version` and a migration hook;<br>• unknown keys kept (`#[serde(flatten)] extra`);<br>• atomic write via temp file + persist, with `.bak`;<br>• a broken file → read-only mode (NFR-REL-05);<br>• a `SecretStore` trait (Windows implementation from M1-11; in-memory for tests). | Round-trip, unknown-key, corrupt-file and newer-schema tests. **Done locally:** passes on Windows and, for `drivemount-core`, in a Linux container (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-15 | **`MountManager` (without reconnect)**:<br>• one actor task per profile owning its state machine (spec §5.5, minus the Reconnecting rows);<br>• commands Mount, Unmount, Cancel (`CancellationToken`), Test, Save, Delete;<br>• `MountEvent` broadcast;<br>• a letter allocator over a `DriveLetters` trait (Auto from Z downwards; skip A, B and C; conflict policy);<br>• backends created through the app-supplied `BackendFactory`;<br>• profile delete also deletes its secrets (FR-PROF-07); connection fields are locked while mounted (FR-PROF-08). | State-machine tests with a mock backend cover every transition implemented in M1; allocator tests. **Done locally:** passes on Windows and, for `drivemount-core`, in a Linux container, along with the real factory through real drives against the S-03 server (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |

### WP 1.F: Application and UI

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M1-16 | **Main window (Slint)**:<br>• rows with status dot, name, address and letter, primary button, ⋯ menu;<br>• status bar and empty state;<br>• WinFsp banner (FR-APP-06);<br>• keyboard shortcuts (spec §7.7);<br>• System/Light/Dark theme;<br>• all strings in `@tr()`.<br>UI logic is tested with Slint's testing back-end. | UI logic tests pass; owner confirms the layout matches the spec §7.1 mock-up at 100% and 150% scaling; keyboard-only walkthrough works. **Search and groups were dropped by the owner on 2026-11-05.** **Done locally, and the owner confirmed H-06 on 2026-11-05:** `cargo run -p drivemount --example main_window` shows the window with made-up connections (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | H-06 | ☑ |
| M1-17 | **Profile dialog and prompts**:<br>• the Add/Edit dialog with an Advanced section;<br>• inline validation through Rust callbacks;<br>• drive dropdown that marks free letters;<br>• Test connection (FR-PROF-06);<br>• secret prompt, host-key trust prompt and changed-key dialog (spec §7.4);<br>• a `Prompter` implementation that serialises prompts and brings the window to the front. | Dialog logic tests pass; owner confirms the flows look right against the S-03 server. **Done locally, and the owner confirmed H-06 on 2026-11-06:** `cargo run -p drivemount --example connect -- --fresh` is the real engine with a scratch configuration (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | H-06 | ☑ |
| M1-18 | **App wiring**:<br>• `main.rs` builds the Tokio runtime, initialises `tracing` (file appender, rolling, level from config) and loads the config;<br>• `BackendFactory` (SFTP → WinFsp), which M1-15 has written, in `apps/drivemount`'s library: put it together with `MountManager` here;<br>• UI ↔ engine channels (`mpsc` commands; `broadcast` events through `invoke_from_event_loop`);<br>• a dev-only example `examples/mount_one.rs` (not shipped), so the AI can drive mounts without the UI. | `examples/mount_one.rs` mounts, verifies and unmounts a profile by script; `drivemount.exe` starts and shows saved profiles. **Done locally:** `tests/mount_one.rs` and `tests/drivemount_exe.rs` pass against the S-03 server (see `docs/PROGRESS.md`). The workflow itself first runs with S-05/H-02 | – | ☑ |
| M1-19 | **M1 exit verification**: run the automated part of the M1 checklist (§9.2), hand the manual part to the owner, fix what fails, write `docs/spikes/m1-exit.md` with the results. | All M1 checklist items pass. **Done locally:** every item passes, with H-07 done by the owner (one fix, H7-1); "in CI" waits for H-02. See `docs/spikes/m1-exit.md` | H-07 | ☑ |

---

## 6. M2: v1 release

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M2-01 | **Reconnect** (FR-REC-01 to FR-REC-07):<br>• **Health:** `Connector` reports health from keep-alive misses, transport errors and the DeadlineFs suspect signal.<br>• **State and backoff:** the Reconnecting state uses backoff 1→2→4→8→16→32→60 s; auth failure or host-key change stops it.<br>• **Adapter during reconnect:** it holds the current session in an `ArcSwap` plus a `watch`, so calls during reconnect wait up to the deadline and then return `ConnectionLost` → `STATUS_NETWORK_UNREACHABLE`.<br>• **Handles:** each handle carries a session generation and is reopened by path (no truncation) on first use after a reconnect. | Automated tests: server kill/restart during a large read finishes with a matching hash; cable-pull simulation (iptables DROP) gives an error within 15 s and never hangs; a password change on the server → Error, no retries. **Done locally:** all three pass, the first and third against OpenSSH in S-03 (`apps/drivemount/tests/real_drive.rs`). The cable pull is a proxy in the test process that holds all data back with TCP open, which is what iptables DROP looks like to the client, at a 3 s timeout. The session sits in a `watch` alone (`SessionFs`), with no `ArcSwap`. See `docs/PROGRESS.md` | – | ☑ |
| M2-02 | **Windows events**: hidden top-level window (from M0-3) for `PBT_APMRESUMEAUTOMATIC` and `WM_QUERYENDSESSION`/`WM_ENDSESSION`; `NotifyIpInterfaceChange` callback → engine command `NetworkChanged` (immediate retry, keep-alive probe). | Owner runs sleep/resume, Wi-Fi switch and sign-out; logs show the drive working within 5 s of the network being up and an unmount within 5 s of sign-out (FR-APP-05) | H-08 | ☐ |
| M2-03 | **Tray** (FR-TRAY): first compare Slint's own `SystemTrayIcon` (Slint ≥ 1.18, not tested in M0-3) with `tray-icon`, which hides `NIM_ADD` failures; four icon variants (`.ico`, 16/20/24/32 px); tooltip with counts; left-click toggles the window; menu rebuilt on state events (spec §7.6). | Menu/tooltip logic unit-tested; owner confirms the look and behaviour | H-08 | ☐ |
| M2-04 | **Notifications** (FR-NOT): toasts for the four events through the `windows` crate; the app writes `HKCU\Software\Classes\AppUserModelId\<id>` at start (spike M0-3: a shortcut alone showed no toast); clicking a toast opens the window with the profile selected; **settle clicks from Action Center, which did nothing in M0-3** (COM activator, protocol activation, or the owner agrees to change FR-NOT-04); respects the setting. | Event → toast logic tested; owner confirms toast and click behaviour | H-08 | ☐ |
| M2-05 | **Settings page** (FR-SET): six items plus About (version, GPLv3 notice and source link, WinFsp attribution, third-party licences button: NFR-LIC-02/03); changes apply immediately (log level through a `tracing` reload handle; theme live); Start with Windows writes or removes the HKCU Run value with `--tray`. | Persistence and Run-key tests; each setting takes effect without restart | – | ☐ |
| M2-06 | **Lifecycle** (FR-APP):<br>• single instance: named mutex plus a registered "show" message to the existing window;<br>• `--tray` start; close to tray; quit confirmation;<br>• crash hook: panic hook writes `crash-*.txt`, then `panic = "abort"`; notice on next start. | Scripted tests (second instance exits, crash file written); owner confirms close-to-tray and quit dialog | H-08 | ☐ |
| M2-07 | **Bulk and start-up mounting**: mount on start (parallel, prompts queued); Mount all and Unmount all; letter-conflict auto-pick with row note and toast (FR-MNT-04). | FR-MNT-04/08/09 acceptance criteria pass | – | ☐ |
| M2-08 | **Performance pass**: measure every NFR-PERF budget on the spec §4.1 environment (the local machine + WSL/Docker server, with netem for WAN); profile with `tracing` spans and Windows Performance Recorder; tune cache TTLs, chunk sizes and thread count; implement write-behind if OD-4 says so (ADR-0002). | `docs/perf/v1.md` shows all budgets met | – | ☐ |
| M2-09 | **Reliability campaign**:<br>• `fsx` 100k operations on SFTP mounts;<br>• full `winfsp-tests` subset;<br>• NEO regressions (spec §10.3);<br>• kill test;<br>• **72 h soak:** 5 mounts; a PowerShell driver copies files in a loop while a WSL/Docker script toggles iptables DROP every 20 min and a wake-timer script sleeps and resumes the PC;<br>• working set logged. | No data mismatch; no stuck state; working-set growth < 10% (NFR-REL-06) | H-09 | ☐ |
| M2-10 | **Installer** (`cargo-wix`, WiX):<br>• per-machine install;<br>• Start-menu shortcut with an AppUserModelID (toasts rely on the HKCU key from M2-04, not on this);<br>• optional Start with Windows;<br>• WinFsp ≥ 2.1 check, then chain-install of the unmodified WinFsp MSI or a link (OD-5);<br>• uninstall with "Remove my settings";<br>• portable zip;<br>• `LICENSE` and `THIRD-PARTY-LICENSES.html` (generated with `cargo-about`) in both artefacts (NFR-LIC-04). | Install, upgrade and uninstall scripted on the dev machine; owner verifies on a clean Windows 11 Pro machine | H-11 | ☐ |
| M2-11 | **Signing and release workflow**: GitHub Actions on tag builds, signs (OD-3), builds the MSI and zip, writes `sha256sums.txt` and drafts the release. | A signed test tag produces the artefacts; SmartScreen does not block | H-10 | ☐ |
| M2-12 | **User guide** `docs/USER_GUIDE.md`: install, add a connection, auth methods, host keys, tray, troubleshooting (one entry per §7.8 error), data locations, licence and WinFsp attribution (NFR-LIC-02). | Reviewed against the UI and the error catalogue | – | ☐ |
| M2-13 | **Release v1.0.0**: run the full checklist (§9.3), update the traceability table, tag. | G3 passed | Approve the release | ☐ |

---

## 7. M3: SMB backend

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M3-01 | **Spike**:<br>• `WNetAddConnection2W` with `CONNECT_TEMPORARY`, explicit credentials, current-user credentials;<br>• reproduce error 1219;<br>• check behaviour after a server reboot;<br>• test `\\127.0.0.1\share` on the CI runner and Samba in WSL/Docker as test targets. | `docs/spikes/m3-01-smb.md` | – | ☐ |
| M3-02 | **`drivemount-smb`**:<br>• `SmbBackend: MountBackend`;<br>• WNet calls run under `spawn_blocking`;<br>• health check via `GetDiskFreeSpaceExW` on network events;<br>• WNet error mapping (spec §5.9);<br>• removes its mappings on quit (FR-SMB-07). | FR-SMB-03/04/05/07 acceptance criteria pass | – | ☐ |
| M3-03 | **Profile and config**: `Protocol::Smb(SmbProfile)`, validation (UNC paste, domain\user), config keys from spec §6.1 (additive; bump `schema_version` only if existing fields change), secrets. | Unit tests for validation and config round-trip | – | ☐ |
| M3-04 | **UI**: protocol selector (SSH \| SMB \| NFS, with NFS hidden until M4); SMB fields; the sub-path note; the E-SMB-1219 text; SMB rows usable without WinFsp (FR-SMB-08). | FR-SMB-01/02/06/08 acceptance criteria pass; owner checks the form | H-06 | ☐ |
| M3-05 | **Tests**: Windows CI against a local share; Samba in WSL/Docker; owner runs the real-server matrix (NAS, Windows share). | Matrix recorded | H-12 | ☐ |
| M3-06 | **M3 release** (v1.1.0). | G4 passed | Approve the release | ☐ |

---

## 8. M4: NFSv3 backend

| ID | Task | Done when | You | ☐ |
|---|---|---|---|---|
| M4-01 | **Spike**:<br>• `nfs3_client` against Linux `knfsd` (Docker, privileged);<br>• check that the TCP socket can be bound to a reserved source port (≤ 1023): if the crate cannot take a caller-provided stream, plan our own ONC-RPC client;<br>• MOUNT v3 and portmapper with fixed ports;<br>• measure READ/WRITE throughput. | `docs/spikes/m4-01-nfs.md` with a crate-or-own decision | – | ☐ |
| M4-02 | **Path → file-handle cache**: WinFsp is path-based and NFS is handle-based, so path lookups resolve through a LOOKUP chain with cached handles. Invalidate on local rename/remove; handle `NFS3ERR_STALE` by re-resolving. | Unit tests against a fake NFS server | – | ☐ |
| M4-03 | **`NfsFs`** (`RemoteFs`): AUTH_SYS with the profile's UID/GID; READDIRPLUS feeds the attribute cache; READ; WRITE UNSTABLE + COMMIT on Flush/Cleanup; CREATE GUARDED; MKDIR, REMOVE, RMDIR, RENAME, SETATTR, FSSTAT; error mapping. | Contract suite passes against `knfsd` | – | ☐ |
| M4-04 | **`NfsConnector`**: portmapper or fixed ports; reserved-port binding from 1023 down; reconnect without reopening handles (FR-NFS-04); `NFS3ERR_ACCES` → E-NFS-ACCESS. | FR-NFS-03/04/05 acceptance criteria pass | – | ☐ |
| M4-05 | **Profile, config and UI**: NFS form (server, export, UID/GID, ports, reserved port), the AUTH_SYS note (spec §8), show NFS in the protocol selector. | FR-NFS-01 acceptance criterion passes; owner checks the form | H-06 | ☐ |
| M4-06 | **Tests**: Linux CI with an NFS container; `fsx` on an NFS mount; owner runs the real-server matrix (Synology/TrueNAS/knfsd) on **Windows 10 Home**. | Matrix recorded | H-13 | ☐ |
| M4-07 | **M4 release** (v1.2.0). | G5 passed | Approve the release | ☐ |

---

## 9. Testing and CI

### 9.1 CI pipeline (GitHub Actions)

| Job | Runner | When | Content |
|---|---|---|---|
| `lint` | windows-latest + ubuntu-latest | every push | `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo deny check`, `cargo audit` |
| `test-portable` | ubuntu-latest | every push | Unit tests for `drivemount-vfs`, `drivemount-sftp`, `drivemount-core` (and `drivemount-nfs` in M4); `docker compose up` (OpenSSH ×2, ProFTPD `mod_sftp`; NFS from M4) → integration and contract suites |
| `test-windows` | windows-latest | every push | Install WinFsp (silent MSI); all unit tests; mount tests with `MemFs` and an in-process `russh` SFTP server; `fsx` 10k operations; the `winfsp-tests` subset; Slint UI logic tests |
| `nightly` | windows-latest | nightly | `fsx` 100k operations; longer contract runs |
| `perf` | local machine | on demand / before release | NFR-PERF measurements → `docs/perf/` |
| `release` | windows-latest | tag `v*` | Build, sign, MSI, zip, checksums, licence notices |

### 9.2 M1 exit checklist

**Automated (AI):**
- [ ] Each SSH auth method mounts and unmounts (through `examples/mount_one.rs` and integration tests).
- [ ] Unknown host key → prompt path taken; Trust and save → `ssh.exe -o BatchMode=yes` accepts the host; changed key → refused.
- [ ] Jump-host profile mounts.
- [ ] Copy 1 GB in and out and 10,000 small files through Win32 calls; hashes match (NFR-REL-01).
- [ ] New file visible in the next listing (NFR-REL-02).
- [ ] Kill `drivemount.exe` → no drive left (NFR-REL-04).
- [ ] Read-only profile rejects writes (FR-MNT-07).
- [ ] `fsx` 10k and the `winfsp-tests` subset pass in CI.

**Manual (owner, H-07):**
- [ ] Add a profile in the UI, mount it, open it in Explorer, unmount it.
- [ ] Explorer: drag and drop, rename, delete, Notepad save, Office save.
- [ ] Host-key prompt and changed-key dialog look right.
- [ ] The drive shows under *Network locations* with the profile name.

### 9.3 v1 release checklist

- [ ] All FR-PROF, FR-SSH, FR-MNT, FR-REC, FR-TRAY, FR-NOT, FR-APP and FR-SET acceptance criteria verified and ticked in `docs/TRACEABILITY.md`.
- [ ] All NFR-PERF budgets met (`docs/perf/v1.md`).
- [ ] 72 h soak passed (NFR-REL-06).
- [ ] Manual matrix (spec §10.4, SSH rows) passed on Windows 10 Home 22H2 and Windows 11 Pro.
- [ ] MSI installs, upgrades and uninstalls cleanly; binaries signed.
- [ ] Licence obligations met (NFR-LIC-01 to NFR-LIC-06): `cargo deny` clean, About shows the GPLv3 notice and WinFsp attribution, licence files shipped, release tag public with source link.
- [ ] User guide complete.

---

## 10. Requirement → task traceability

| Requirement group | Tasks |
|---|---|
| FR-PROF | M1-13, M1-14, M1-15, M1-17 |
| FR-SSH-01…07 (auth) | M1-08b, M1-17 |
| FR-SSH-08…10 (host keys) | M1-07, M1-08a, M1-17 |
| FR-SSH-11 (jump host) | M1-09 |
| FR-SSH-12 (keep-alive) | M1-08a, M2-01 |
| FR-SSH-13…15 (paths, extensions, servers) | M1-08a, M1-10 |
| FR-MNT | M1-11, M1-12a–c, M1-15, M2-07 |
| FR-REC | M2-01, M2-02 |
| FR-TRAY | M0-3, M2-03 |
| FR-NOT | M0-3, M2-04 |
| FR-APP | M1-11, M2-02, M2-06 |
| FR-SET | M2-05 |
| FR-SMB | M3-01 … M3-05 |
| FR-NFS | M4-01 … M4-06 |
| NFR-PERF | M0-2, M0-3, M1-06, M1-10, M2-08 |
| NFR-REL | M1-05, M1-12a–c, M2-01, M2-09 |
| NFR-SEC | M1-02 (`Secret`), M1-07, M1-11, M2-11, CI `deny`/`audit` |
| NFR-UX | M1-16, M1-17 |
| NFR-MAINT | M1-01, CI |
| NFR-LIC | M1-01, M2-05, M2-10, M2-11, M2-12, CI `deny` |

`docs/TRACEABILITY.md` (created in M1-01) has one row per requirement, with columns for task, test and verified-in-version.

---

## 11. Engineering conventions

- **Errors:** `thiserror` enums in library crates; `anyhow` only in `apps/drivemount`. User-facing errors map to the §7.8 codes in one place.
- **Panics:** no `unwrap`/`expect` in library code (clippy-enforced). Panics are bugs; the crash hook records them.
- **`unsafe`:** only in `drivemount-win` and `drivemount-winfsp`, each block with a `// SAFETY:` comment. Everything else is `#![forbid(unsafe_code)]`.
- **Async:** Tokio everywhere. Blocking Win32 calls (WNet, Credential Manager) run under `spawn_blocking`. Nothing blocks the UI thread (NFR-PERF-10).
- **Secrets:** only inside `Secret<T>`. Never formatted or logged; zeroed on drop. A grep check in CI rejects `format!` of `Secret` contents.
- **Logging:** `tracing` with one span per profile (`profile_id`, `name`). Info for state transitions, Debug for operations, Trace for protocol detail. The logs are also the AI's main way to verify behaviour after a manual step.
- **Tests:** each task lands with its tests. Backends must pass the shared contract suite. Prefer scripted checks over manual ones whenever possible.
- **Decisions:** architecture decisions go in `docs/adr/NNNN-title.md` (ADR-0002 write mode; ADR-0001 async traits, ADR-0003 licence and ADR-0004 `rsa` advisory are accepted).
- **Version control:** one task per branch; Conventional Commits (`feat:`, `fix:`, `test:`…); `main` always green; the owner approves merges and pushes.
- **Versioning:** `0.x` pre-releases during M1–M2; `1.0.0` at G3; minor versions for M3 and M4.

### Definition of done (every task)

1. The code builds on Windows (and on Linux for portable crates), with no clippy warnings.
2. Tests for the task are written and pass locally and in CI.
3. The linked requirements' acceptance criteria are verified (by script, or by the owner at the listed checkpoint) and recorded in `docs/TRACEABILITY.md`.
4. The spec is updated if behaviour or design differs from it.
5. The status box is ticked and `docs/PROGRESS.md` has the hand-over notes for the next session.

---

## 12. Risks during implementation

| Risk | Early warning | Response |
|---|---|---|
| `russh-sftp` lacks a usable low-level API or is too slow | M0-2 | Own SFTP v3 client on a `russh` channel (one extra work package). The `RemoteFs` boundary keeps the rest unchanged. |
| `winfsp` crate build or soundness problems | M0-1, M1-12a | Use the `system` feature with Developer files; report upstream. Last resort: switch to Dokany behind the same `MountBackend`. |
| Problems the AI cannot observe (visual glitches, Explorer quirks) | H-04, H-06, H-07 | Owner reports with a screenshot or exact steps; the AI turns each report into a scripted regression test where possible. |
| Context lost between AI sessions | Any session | Status boxes, `docs/PROGRESS.md`, `docs/TRACEABILITY.md` and ADRs carry the state; each session starts by reading them (§1.3). |
| Explorer too chatty over WAN | M2-08 | Tune TTLs; extend the noise list; consider longer `FileInfoTimeout` for directories only. |
| Toasts clicked in Action Center do nothing (M0-3); a fix may need a shortcut the portable zip lacks | M2-04 | Toasts themselves work without a shortcut. Worst case, the error is still in the row and tray tooltip, and FR-NOT-04 is narrowed with the owner's agreement. |
| Soak test needs sleep/resume automation on Windows 10 Home | M2-09 | `rundll32 powrprof.dll,SetSuspendState` with a wake timer, or run the soak on a second machine. |
| NFS crate cannot bind reserved ports | M4-01 | Own ONC-RPC/NFSv3 client (the protocol is well documented, RFC 1813). |

---

## Appendix A: Initial dependency set

Versions are pinned in M1-01 to the latest stable releases at that time. Any change later goes through `cargo deny`.

| Area | Crates |
|---|---|
| Async and utilities | `tokio` (rt-multi-thread, net, time, sync, macros), `tokio-util` (CancellationToken), `bytes`, `arc-swap`, `futures` |
| SSH / SFTP | `russh`, `russh-sftp` (key parsing through russh's key module) |
| WinFsp | `winfsp` (+ `winfsp-sys`), GPL-3.0, allowed by ADR-0003 |
| Windows API | `windows` (Win32: Credentials, NetworkManagement::WNet, Storage::FileSystem, UI::Shell, System::Power, NetworkManagement::IpHelper, Threading) |
| UI | `slint` (+ `slint-build`; testing back-end for UI logic tests), `tray-icon` |
| Toasts | `windows` (`UI::Notifications`), decided in M0-3 |
| Config and data | `serde`, `toml`, `uuid` (v4, serde), `tempfile` |
| Crypto helpers | `sha2`, `hmac` + `sha1` (hashed known_hosts), `base64`, `zeroize` |
| Logging | `tracing`, `tracing-subscriber` (fmt, with `reload` at M2-05), `tracing-appender` |
| Errors | `thiserror`, `anyhow` (app only), `async-trait` (ADR-0001) |
| Dev and test | `proptest`, `criterion`, `tokio` test-util, `russh` server side (in-process SFTP test server), `sha2` for integrity checks |
| Tooling | `cargo-deny`, `cargo-audit`, `cargo-about`, `cargo-wix` |
