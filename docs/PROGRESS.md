# FS Manager: Progress notes

Notes for the next session: decisions, open issues and anything half-done. Newest first.

## 2026-11-03: Setup (S-01 to S-05)

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

**Open / next**
- **S-03 is blocked on a reboot.** WSL2 couldn't start because the Virtual Machine Platform feature was off; it has been enabled (dism) and takes effect after a restart. After the reboot:
  - install `docker-ce` + `docker-compose-plugin` in Ubuntu-22.04;
  - turn on systemd in `/etc/wsl.conf`;
  - write `tests/docker/docker-compose.yml` with an SFTP server on port 2222 and users for password, key, certificate and TOTP auth;
  - check `sftp -P 2222 tester@localhost` from Windows.
- After S-03 → G0 (minus S-05) → M0 spikes.
