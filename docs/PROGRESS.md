# FS Manager: Progress notes

Notes for the next session: decisions, open issues and anything half-done. Newest first.

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
