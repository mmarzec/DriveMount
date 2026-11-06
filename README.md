# FS Manager

FS Manager (working title; executable `fsm.exe`) is a Windows desktop application that mounts remote disks as drive letters. It keeps a list of connections, mounts or unmounts each with one click, restores mounts at start-up and after network drops, and lives in the system tray.

It is a native Rust program built on [WinFsp](https://winfsp.dev). It needs no external SSHFS tooling and runs as a normal, unelevated user process.

| Release | Content |
|---|---|
| **v1** (milestones M0–M2) | SSH/SFTP connections as drive letters, tray, auto-mount, auto-reconnect, Windows Credential Manager, host-key verification, settings, MSI installer |
| **M3** | SMB 2/3 connections, through Windows' own SMB client |
| **M4** | NFSv3 connections, with a built-in userspace client |

It targets Windows 10 22H2 and Windows 11 23H2 or later, on x64.

## Status

**Early development: there is no usable application yet.** Milestone M1 is in progress. The SFTP backend and the WinFsp drive adapter are done, but profiles, the mount manager and the UI are still to come, and `fsm.exe` currently does nothing. Task-by-task notes are in [docs/PROGRESS.md](docs/PROGRESS.md).

## User guide

*To be written.*

## Development

### Prerequisites

- **Rust**, through [rustup](https://rustup.rs). [rust-toolchain.toml](rust-toolchain.toml) pins the version, and rustup installs it on first use.
- **Visual Studio Build Tools**, with the "Desktop development with C++" workload (MSVC linker and Windows SDK).
- **LLVM**, for the `libclang` that `winfsp-sys` needs at build time.
- **WinFsp with its Developer feature** (headers and import libraries). From an administrator PowerShell, this installs the pinned version unless WinFsp is already there:

  ```powershell
  scripts\install-winfsp.ps1
  ```

- **OpenSSH client**: the `known_hosts` tests call `ssh-keygen`.
- **Docker Engine inside WSL2** (Ubuntu-22.04), only for the tests that need real servers. Docker Desktop is not used.

For the lint checks that CI runs, also install the same versions of these tools:

```powershell
cargo install --locked --version 0.20.2 cargo-deny
cargo install --locked --version 0.22.2 cargo-audit
```

### Layout

| Path | Contents |
|---|---|
| `apps/fsm` | The GUI binary, `fsm.exe`, and the library behind it that makes real drives and the main window. `cargo run -p fsm --example main_window` shows the window with made-up connections, and `--example connect` runs it with the real engine and a scratch configuration |
| `crates/fsm-vfs` | Shared traits and types (`RemoteFs`, `Connector`, `MountBackend`), caches, read-ahead, deadlines |
| `crates/fsm-core` | Profiles, configuration and the mount manager |
| `crates/fsm-sftp` | `RemoteFs` over SFTP, and its connector |
| `crates/fsm-smb` | Native Windows SMB mappings (M3) |
| `crates/fsm-nfs` | `RemoteFs` over NFSv3 (M4) |
| `crates/fsm-win` | Windows helpers: credentials, drive letters, shell, WinFsp detection |
| `crates/fsm-winfsp` | Adapter from `RemoteFs` to a WinFsp drive |
| `scripts` | CI checks and installers |
| `tests` | Test servers (Docker) and the `winfsp-tests` exclusions |
| `spikes` | Throw-away experiments from M0, in their own workspaces |

### Build

```powershell
cargo build --workspace --locked
cargo build --release -p fsm --locked
```

The workspace also builds on Linux, where the Windows-only crates (`fsm-win`, `fsm-winfsp`, `fsm-smb`) are empty.

### Checks

CI runs all of these, so run them before committing. The two `.sh` scripts need a POSIX shell, such as Git Bash.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny --locked check
cargo audit
sh scripts/check-spdx.sh
sh scripts/check-secret-format.sh
scripts\check-exe-size.ps1    # after a release build; fsm.exe must stay under 15 MB
```

### Conventions

- Every `.rs` and `.slint` file starts with `// SPDX-License-Identifier: GPL-3.0-or-later`.
- Dependencies are pinned to exact versions in the root [Cargo.toml](Cargo.toml). Changing one is a deliberate edit.
- `unwrap` and `expect` are denied outside tests. `unsafe` is allowed only in `fsm-win` and `fsm-winfsp`, and each block needs a `// SAFETY:` comment.

## Tests

### Unit and drive tests

```powershell
cargo test --workspace --locked
```

On Windows this mounts real, short-lived drives through WinFsp, so the WinFsp driver must be usable. No servers are needed.

Tests marked `#[ignore]` need something extra, and each says what. Run them per crate or test file as shown below, rather than with `--workspace --ignored`.

### Drive tests over SFTP

This runs the same drive tests through a real SFTP session, against a server inside the test process:

```powershell
$env:FSM_TEST_SERVER = 'sftp'
cargo test -p fsm-winfsp --locked --test read_path --test write_path --test fsx
Remove-Item Env:FSM_TEST_SERVER
```

`FSX_OPS` and `FSX_SEED` set the length and seed of a `tests/fsx.rs` run, which is how to repeat a failure or run longer.

### Tests against real servers

These need the OpenSSH and ProFTPD containers, which run on Docker Engine in WSL2. The users, keys and ports are listed in [tests/docker/README.md](tests/docker/README.md).

```powershell
tests\docker\servers.ps1 up       # generate keys if missing, build, start
tests\docker\servers.ps1 check    # verify every auth method
cargo test -p fsm-sftp --locked -- --ignored
tests\docker\servers.ps1 down
```

The tests look for the servers on `localhost`. `tests/sftpfs.rs` alone can use another host, through `FSM_TEST_HOST`.

The mount manager with the real factory, through real drives, needs those servers and WinFsp together. Secrets go to a store in memory, and Explorer is told about the drives:

```powershell
cargo test -p fsm --test real_drive -- --ignored --test-threads=1
```

For throughput measurements, connect to the WSL VM's IP (`wsl hostname -I`) rather than `localhost`, because the Windows port-forward relay distorts the results:

```powershell
cargo run --release -p fsm-sftp --example sftpfs_throughput -- <host> [MiB] [runs]
```

### WinFsp's own test suite

`winfsp-tests` is not part of the WinFsp installer, so this script fetches a pinned copy into `target\winfsp-tests`:

```powershell
powershell -File scripts/get-winfsp-tests.ps1
cargo test -p fsm-winfsp --locked --test winfsp_tests -- --ignored --test-threads=1 --nocapture
```

Set `WINFSP_TESTS` to use a copy from elsewhere. The tests left out, with the reason for each, are in [tests/winfsp-tests-exclusions.txt](tests/winfsp-tests-exclusions.txt).

### Tests that need a desktop

CI cannot run these. They need a signed-in desktop, and they open an Explorer window on "This PC" for a moment:

```powershell
cargo test -p fsm-winfsp --test explorer -- --ignored --test-threads=1
```

`cargo test -p fsm-win -- --ignored` runs the `fsm-win` tests that need a desktop or an installed WinFsp. Some of them change this user's settings for a moment, and each says what it touches.

### Continuous integration

[.github/workflows/ci.yml](.github/workflows/ci.yml) defines all of the above except the desktop tests, for GitHub Actions or Gitea Actions. The header of that file lists what a runner needs. The workflow has not run on a runner yet.

## Documentation

| Document | Contents |
|---|---|
| [docs/SPECIFICATION.md](docs/SPECIFICATION.md) | Requirements and design |
| [docs/ANALYSIS.md](docs/ANALYSIS.md) | Reference program, problems, technology rationale |
| [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md) | Milestones and tasks |
| [docs/PROGRESS.md](docs/PROGRESS.md) | Notes per task: decisions and open issues |
| [docs/TRACEABILITY.md](docs/TRACEABILITY.md) | Requirements traced to tasks and tests |
| [docs/adr](docs/adr) | Architecture decision records |
| [docs/spikes](docs/spikes) | Results of the M0 spikes |

## Licence

GPL-3.0-or-later: see [LICENSE](LICENSE) and [ADR-0003](docs/adr/0003-licence-gplv3.md).
