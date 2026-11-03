# Spike M0-1: WinFsp in-memory drive

| | |
|---|---|
| Risk | "Network-drive mode from an unelevated process" |
| Code | `spikes/m0-1-winfsp/` (throw-away): `src/main.rs` (in-memory file system), `checks.ps1` (scripted checks) |
| Machine | Windows 10 Home 22H2, WinFsp 2.2.26286 (2026 Beta5), Rust 1.99.1, `winfsp` crate 0.13.2+winfsp-2.1 |
| Date | 2026-11-03 |
| Result | __RESULT__ |

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

__CHECKS__

## 4. `FileInfoTimeout`

__TIMEOUT__

## 5. Manual checks (H-04)

__MANUAL__

## 6. Volume parameters (final set → spec §5.8)

__PARAMS__

## 7. Other findings

- **Remembered network mappings.** `GetLogicalDrives` does not report a remembered but disconnected `net use` mapping (here `Y:` → `\\wsl$\Ubuntu-22.04`), and WinFsp mounts over such a letter without error. This confirms FR-MNT-03: the letter allocator must also check `WNetGetConnectionW`/`HKCU\Network`. The owner's mapping was unaffected (`HKCU\Network\Y` intact).
- Windows PowerShell 5.1 prefixes the first line written to a child's redirected stdin with a BOM. The spike strips it. This only matters for test harnesses.

## 8. Go / no-go

__GO__
