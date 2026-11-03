# Spike M0-1: WinFsp in-memory drive

| | |
|---|---|
| Risk | "Network-drive mode from an unelevated process" |
| Code | `spikes/m0-1-winfsp/` (throw-away): `src/main.rs` (in-memory file system), `checks.ps1` (scripted checks) |
| Machine | Windows 10 Home 22H2, WinFsp 2.2.26286 (2026 Beta5), Rust 1.99.1, `winfsp` crate 0.13.2+winfsp-2.1 |
| Date | 2026-11-03 |
| Result | **Go.** Scripted checks 32/32 and manual checks (H-04) 6/6 pass unelevated. Two Explorer integration issues go to M1-12c (§7). |

## 1. What was tried

- A minimal in-memory file system implementing `FileSystemContext`:
  - metadata and security: `get_security_by_name`, `get_security`, `set_security` (ignored), `get_file_info`, `set_basic_info`, `get_volume_info`;
  - handles: `create`, `open`, `overwrite`, `cleanup` (deletes on `FspCleanupDelete`), `close`, `flush`;
  - data: `read`, `write` (including constrained I/O and write-to-EOF), `set_file_size` (file and allocation size);
  - directories, rename and delete: `read_directory` (through `DirBuffer`), `rename` (re-keys the subtree; case-only renames), `set_delete` (refuses non-empty directories).
- Case-insensitive, case-preserving names: lookups use an upper-cased key, and `open` returns the stored case through `set_normalized_name`.
- A fixed security descriptor, because there is no ACL support: `O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)`.
- The spike counts calls per callback. `checks.ps1` reads the counts through stdin commands (`stats`, `reset`, `quit`).
- Mounted from a **normal, unelevated** PowerShell. Network mode used `VolumePrefix = \fsm\spike`, then disk mode with no prefix.

## 2. Build and link mode

| Mode | Build | Runtime |
|---|---|---|
| Bundled (crate default) | Builds with the crate's own WinFsp 2.1 headers and import library | **Fails** with exit 1285 (`ERROR_DELAY_LOAD_FAILED`) unless `winfsp-x64.dll` is on PATH or next to the exe |
| `system` feature | Builds against the installed Developer files (`C:\Program Files (x86)\WinFsp\inc`, `lib`) | **Works**: `winfsp_init` finds the installed DLL through `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` |

**Decision: use the `system` feature.** FS Manager uses the WinFsp the user installed, as the spec requires, and does not ship a private DLL.

Consequences:
- **libclang is required at build time.** `winfsp-sys` runs `bindgen` on every build. It was missing from setup and is now installed (LLVM 23.1.4 via winget, `C:\Program Files\LLVM`). CI runners need LLVM too.
- **CI needs the WinFsp Developer feature:** `msiexec /i winfsp-*.msi ADDLOCAL=ALL /qn`.
- The binary crate needs `build.rs` calling `winfsp::build::winfsp_link_delayload()`. The `/DELAYLOAD` argument from `winfsp-sys`'s own build script does not reach the final binary.
- `winfsp` 0.13 uses `windows` 0.61. Our crates must use the same version, or enable the crate's `windows-62` compatibility feature, so that `NTSTATUS` converts into `FspError`.
- `FileSystemHost::new`/`mount` return `windows::core::Error`, while callbacks return `FspError`. The host type must name its guard strategy (`FileSystemHost<T, FineGuard>`). Under `FineGuard`, both `T` and `T::FileContext` must be `Sync`.
- A panic in a callback becomes `STATUS_NONCONTINUABLE_EXCEPTION` (0xC0000025) for the caller, and the process keeps running.

## 3. Scripted checks (AI)

`checks.ps1` was run unelevated (`Elevated: False`) against the `system` build, in network mode (`--prefix \fsm\spike`) and disk mode (no prefix). Result: **32 / 32 passed.**

| Check | Network | Disk |
|---|---|---|
| Drive type | `Network` | `Fixed` |
| Create/write/read; nested mkdir and listing | ✅ | ✅ |
| Rename; case-only rename (`b.txt` → `B.TXT`, case kept, case-insensitive open) | ✅ | ✅ |
| Move a directory with contents | ✅ | ✅ |
| Rename onto an existing file: refused without replace, works with replace | ✅ | ✅ |
| Delete a non-empty directory refused; recursive delete works | ✅ | ✅ |
| Overwrite, append, sparse write at 100 000 (zero fill), truncate | ✅ | ✅ |
| Read-only attribute blocks writes; `LastWriteTime` round-trips | ✅ | ✅ |
| `ReplaceFile` with backup (`File.Replace`) | ✅ | ✅ |
| `robocopy /MIR` in, all SHA-256 equal | 10 000 files, 70 s | 1 000 files, 13 s |
| `robocopy /MIR` again after adding 100, changing 100, deleting 100 (extra files purged) | ✅ | ✅ |
| `robocopy /MIR` out, all SHA-256 equal | ✅ | ✅ |
| 1 GiB `CopyFile` in, SHA-256 equal | ✅ 2 301 MiB/s | (network run only) |
| 1 GiB `CopyFile` out, SHA-256 equal | ✅ 2 841 MiB/s | (network run only) |
| `quit` unmounts; letter gone | ✅ | ✅ |
| **Kill** (`Stop-Process -Force`) with a file handle open: letter gone from `GetLogicalDrives` (NFR-REL-04) | ✅ within 0.3 s | ✅ within 0.3 s |

- The `robocopy` times are mostly the harness hashing 20 000 files, not the drive.
- The 1 GiB rates are in-memory, so they show WinFsp's own overhead is far below any SFTP link speed.
- Two bugs found by the checks were fixed during the spike. A case-only rename removed its own node, which caused a panic. The root listing contained the root itself.

## 4. `FileInfoTimeout`

Same Explorer-like workload for each value, in network mode: 200 files in a directory, five rounds 200 ms apart, each round listing the directory, reading attributes and refreshing every entry, and reading one file. The table shows callback counts.

| `FileInfoTimeout` | `get_security_by_name` | `open` | `get_file_info` | `read_directory` | `read` | Total |
|---|---|---|---|---|---|---|
| 0 ms | 2 431 | 2 430 | 4 460 | 80 | 10 | 11 841 |
| 1000 ms | 2 431 | 2 430 | 0 | 5 | 10 | 7 306 |
| 5000 ms | 2 431 | 2 430 | 0 | 5 | 10 | 7 306 |

- **0 ms** turns the kernel's metadata and directory cache off: about 4 500 extra `get_file_info` calls and 16× more `read_directory` calls. Over SFTP each of those would be a round trip or a cache lookup, so 0 is not an option.
- **1000 ms and 5000 ms** give identical counts for this workload. A longer value only helps workloads that revisit the same entries more than 1 s apart, and it widens the window for stale metadata, which is the NEO bug class (spec §5.6).
- `open`/`get_security_by_name` are not reduced by any value: every `CreateFile` reaches the file system. Our own stat cache (spec §5.6) has to absorb these.
- **Chosen: `FileInfoTimeout` = 1000 ms**, as the spec already says. No separate `DirInfoTimeout`/`VolumeInfoTimeout` is needed.

## 5. Manual checks (H-04)

Done by the owner on 2026-11-03: **all pass**. Mounted from a normal PowerShell with:

```powershell
C:\marcin\fs-manager\spikes\m0-1-winfsp\target\system\release\m0-1-winfsp.exe --mount M: --prefix \fsm\spike
```

| # | Check | Result |
|---|---|---|
| 1 | Explorer shows the drive under *Network locations* with a network-drive icon. It is named **"spike (\\fsm) (M:)"**, not the volume label. It appeared only after Explorer was restarted (§7) | ✅ |
| 2 | Drag and drop a few files and a folder from the desktop to M:, and back | ✅ |
| 3 | Notepad: create a file on M:, save, edit, save again, reopen | ✅ |
| 4 | LibreOffice Writer 7.1: save a document to M:, edit, save again, reopen | ✅ |
| 5 | Delete a file in Explorer: it asks "permanently delete" (no Recycle Bin) | ✅ |
| 6 | Copy a few images to M: and switch to *Large icons*: thumbnails appear | ✅ |

Total Commander also saw and used `M:` (`\\fsm\spike`) immediately.

## 6. Volume parameters (final set → spec §5.8)

Used in all spike runs, unchanged from spec §5.8 apart from the explicit additions marked *new*:

| Parameter | Value |
|---|---|
| `SectorSize` / `SectorsPerAllocationUnit` | 4096 / 1 |
| `MaxComponentLength` | 255 |
| `CaseSensitiveSearch` / `CasePreservedNames` / `UnicodeOnDisk` | false / true / true |
| `PersistentAcls` | false (fixed descriptor: SY, BA and Everyone full access, owner BA) *new: descriptor* |
| `ReadOnlyVolume` | per profile |
| `PostCleanupWhenModifiedOnly` | true |
| `FileInfoTimeout` | 1000 ms |
| `VolumePrefix` | `\fsm\<profile-slug>` (network mode) |
| `FileSystemName` | `FSM-SFTP` (shown as the drive's format in `DriveInfo`) |
| `VolumeSerialNumber` / `VolumeCreationTime` | fixed per profile / mount time *new* |
| Left at crate defaults | `UmFileContextIsUserContext2` = 1 (one context per open handle), `AlwaysUseDoubleBuffering` = 1, `HardLinks`, `ReparsePoints`, `NamedStreams`, `ExtendedAttributes`, `PassQueryDirectoryPattern`, `FlushAndPurgeOnCleanup` = 0 |
| Dispatcher | `FineGuard`, default thread count (`start_with_threads(0)`) |

## 7. Other findings

- **A running Explorer does not notice the new drive (open issue → FR-MNT-06).** WinFsp creates the letter with `DefineDosDevice` and sends no notification. In the owner's already-running Explorer, `M:` stayed invisible after F5 and in new windows, while Total Commander and a fresh process's shell namespace (`Shell.Application`, "This PC") listed it. Restarting Explorer made it appear.
  - `SHChangeNotify(SHCNE_DRIVEADD, M:\)` from another process had no effect.
  - A synchronous `WM_DEVICECHANGE`/`DBT_DEVICEARRIVAL` broadcast (`BroadcastSystemMessage`, and `SendMessageTimeout(HWND_BROADCAST, SMTO_ABORTIFHUNG)`) **hung** on unresponsive windows (a suspended Store app) and had to be killed. Its effect on Explorer is unknown. FS Manager must never broadcast synchronously.
  - Candidate fix to test in M1-12c: mount with the `\fsm\<slug>` prefix but no letter, then assign the letter with `WNetAddConnection2W` through WinFsp's network provider (`WinFsp.Np`). That is the `net use` path, which Explorer picks up immediately. Fallback: `SHChangeNotify` called from inside our own process plus an asynchronous (posted) device broadcast, then check again.
- **Explorer names network drives after the UNC path, not the volume label (→ FR-MNT-05).** `\fsm\spike` shows as "spike (\\fsm) (M:)", while `DriveInfo.VolumeLabel` is "FSM Spike". Getting "Production (Z:)" needs the per-user `_LabelFromReg` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##fsm#<slug>` (how renamed mapped drives keep their names); not tested in the spike. Alternative: make the slug readable, since it becomes the visible name.
- **Remembered network mappings.** `GetLogicalDrives` does not report a remembered but disconnected `net use` mapping (here `Y:` → `\\wsl$\Ubuntu-22.04`), and WinFsp mounts over such a letter without error. This confirms FR-MNT-03: the letter allocator must also check `WNetGetConnectionW`/`HKCU\Network`. The owner's mapping was unaffected (`HKCU\Network\Y` intact).
- Windows PowerShell 5.1 prefixes the first line written to a child's redirected stdin with a BOM. The spike strips it. This only matters for test harnesses.

## 8. Go / no-go

**Go.** All scripted checks (32/32) and all manual checks (6/6) pass unelevated in network mode, and the letter disappears on kill. No fallback (plain drive or Dokany) is needed.

Carried into M1 (spec updated):
- the `winfsp` crate with the `system` feature; LLVM and the WinFsp Developer feature on build machines and CI;
- volume parameters as in §6, with `FileInfoTimeout` = 1000 ms;
- FR-MNT-06: how a running Explorer learns about the drive (see §7) is solved and tested in M1-12c;
- FR-MNT-05: the Explorer name comes from `_LabelFromReg` or the slug, tested in M1-12c.
