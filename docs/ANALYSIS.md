# FS Manager: Analysis

| | |
|---|---|
| Document | Analysis of the reference program and of the technology options |
| Status | Draft 1 |
| Date | 2026-11-03 |
| Companion | [SPECIFICATION.md](SPECIFICATION.md) |

## 1. Purpose

We want a new Windows application that mounts remote disks as drive letters, in the spirit of **NEO SSH-Win Manager**. The goals are:

- **Performance.** It is a native program written in Rust, not a Python/Qt application.
- **A simple UI.** The program does one job: manage and mount remote disks.
- **More protocols.** SSH/SFTP comes in v1; SMB and NFS follow in later milestones.

This document records:
- what the reference program does;
- where it falls short;
- which technologies we considered, and why we chose the ones in the specification.

## 2. Reference program: NEO SSH-Win Manager

### 2.1 Facts

| Item | Value |
|---|---|
| Repository | https://github.com/gregorkrebs/NeoSSHWinManager (site: https://www.neosshwinmanager.org) |
| Licence | MIT |
| Latest version at time of analysis | v1.7.2 "Terminal Host Key Fix" (October 2026) |
| Platform | Windows 10 / 11 |
| Language / GUI | Python 3.11+ (frozen with Python 3.14), PyQt6 |
| Packaging | PyInstaller 6 onefile exe (GUI + console CLI), Inno Setup 6 installer |
| Key libraries | paramiko (SSH/SFTP), ftplib (FTP/FTPS), psutil, keyring, xterm.js, pywin32 |
| External prerequisites | **WinFsp** and **SSHFS-Win** (both installed separately; the app checks for them at start) |
| Lineage | From-scratch Python rewrite of the earlier Electron "SSHWinManager" |

### 2.2 How it mounts drives

NEO does not implement a file system itself. It drives **SSHFS-Win**, a Cygwin port of the Linux `sshfs` FUSE client running on WinFsp:

```
net use X: \\sshfs.r\user@host!22\var\www /persistent:no
```

Newer versions start `sshfs.exe` directly in foreground mode. The app keeps the process handle and finds the process with psutil to disconnect it. Every mounted drive is therefore a separate `sshfs.exe` process. Mount state is tracked by enumerating drives on a timer (default 30 s, range 5–300 s).

### 2.3 Feature inventory and what the new app does with each

| NEO feature | New app | Reason |
|---|---|---|
| Connection profiles: name, host, port, user, auth, remote path, drive letter | **Keep, simplified** | Core of the product. Groups are dropped, as the owner finds them clutter. |
| Templates, "save as template", duplicate | Drop | Out of scope at the user's request. |
| Mount / unmount, "Mount all", "Dismount all" | **Keep** | Core. |
| Auto-mount at start, auto-reconnect | **Keep, redesigned** | Event-driven instead of 30 s polling. |
| Shared drive letters, auto-pick when taken | **Keep (auto-pick only)** | Sharing one letter between hosts adds confusion for little value. |
| Tray with per-connection toggles; close to tray | **Keep** | Core for a background mount manager. |
| Host-key verification (`known_hosts`, prompt on first contact, refuse changed keys) | **Keep** | Security baseline. |
| Auth: password, private key, "ask each time", certificates | **Keep and extend** | Add SSH agent, passphrase keys, keyboard-interactive, jump host. |
| FTP / FTPS (browse only, no mount) | Drop | Not a disk protocol the user asked for. |
| Embedded terminal (xterm.js), PuTTY, ssh.exe launcher | Drop | Out of scope at the user's request. |
| SFTP/FTP file browser (queue, resume, sync, diff, archives, open-with) | Drop | Out of scope at the user's request. Explorer browses the mounted drive. |
| Server system info (CPU, RAM, disks…) | Drop | Not related to disk management. |
| CLI companion over a named pipe | Drop | Out of scope at the user's request. |
| Multi-user accounts, login screen, AES-256-GCM database, lockout | Drop | The Windows account is the security boundary. Windows Credential Manager already protects secrets per user. |
| Four themes, accent colours, animated background, six languages | Simplify | System/Light/Dark only; English only, ready for translation. |
| In-app updater | Later | Not needed for v1. |
| Opt-in telemetry | Drop | No network traffic except to the user's servers. |
| Debug tools: "Fix ghost drives", "Restart Explorer" | Not needed | Ghost drives cannot happen with in-process mounts (§3, problem 2). |

## 3. Problems in the reference program

Each problem below is taken from NEO's changelog, docs or troubleshooting guide. The specification answers each one.

| # | Problem | Root cause | Answer in the new design |
|---|---|---|---|
| 1 | Slow cold start; "Failed to load Python DLL" after an in-app update (1.6.1 → 1.7.0); antivirus suspicion | PyInstaller onefile unpacks the Python runtime into `%TEMP%\_MEI…` on every launch and locks the exe | One native, statically linked Rust exe with nothing to unpack |
| 2 | Ghost drives, "mount point in use", stale mounts, debug-only clean-up tools | One external `sshfs.exe` process per mount; the app learns the state by polling drive enumeration | Mounts are hosted inside the app process through WinFsp. They disappear when the process exits, so no orphan drive can be left behind. State changes are events, not polling results. |
| 3 | Key passphrases not supported for drives; no jump host; no keep-alive tuning | Limits of sshfs-win (it runs in the background and cannot prompt; it does not do tunnelling) | Our own SSH client: passphrase prompted once per mount, SSH agent, jump host, configurable keep-alive |
| 4 | Only SSH can be mounted | Hard dependency on SSHFS-Win | A `MountBackend` abstraction: SFTP now, SMB and NFS as drop-in backends |
| 5 | Files written through the drive became NUL bytes (`FileInfoTimeout=-1`, fixed in 1.5.2); new files invisible until refresh | Caching flags passed to WinFsp/sshfs that the app does not control | Our own cache layer with explicit invalidation on every local change, and integrity tests in CI |
| 6 | Up to 30 s before a dropped connection is noticed; periodic wake-ups | Timer-based polling | SSH keep-alive, I/O errors, and Windows sleep/resume and network-change notifications trigger reconnect at once |
| 7 | Large and growing UI and settings surface | Accounts, themes, browser, terminal, system info, telemetry, licensing history | One window, one dialog, one settings page with about six options |
| 8 | Startup crash loop from a hardened data-folder ACL (#22, 1.4.0 → 1.6.0) | Custom ACLs on the app data folder | No custom ACLs. Secrets live in Windows Credential Manager; the config file holds no secrets. |
| 9 | Drives invisible when the app runs as administrator | Elevated and normal logon sessions have separate drive maps | The app always runs unelevated and offers no "run as admin" option |
| 10 | "Access denied" and permission oddities on mounted drives | sshfs uid/gid/umask mapping options | Our adapter maps remote permissions to Windows attributes in one documented way (see the specification, section "Name and permission mapping") |

## 4. Requirements given by the user

| Requirement | Decision |
|---|---|
| Similar to NEO SSH-Win Manager | Mount manager for remote disks as drive letters, with tray and auto-mount |
| More performant technology, Rust or Go | **Rust** (§5.1) |
| Simplified UI | One window, one dialog, one settings page; no extra tools |
| Manage remote disks over SSH, NFS, SMB | **v1: SSH only.** SMB and NFS are specified now and delivered in later milestones. |
| Platforms | **Windows only** |
| How to mount | **Built-in engine**: our own SFTP (and later NFS) client exposed through WinFsp; SMB through Windows' own client |
| Extra features (terminal, file browser, CLI, duplicate profiles) | **None** |

## 5. Technology analysis

### 5.1 Language: Rust or Go

| Criterion | Rust | Go |
|---|---|---|
| Drive layer | `winfsp` crate, 0.13: safe bindings to WinFsp's **native** API, tested with a port of the `ntptfs` test suite | `cgofuse` (WinFsp's **FUSE compatibility** layer) or the newer `go-winfsp` |
| Callback overhead | Plain function calls; no runtime in the I/O path | Every file system call crosses from native code into Go; the garbage collector runs in the same process |
| SSH/SFTP libraries | `russh` + `russh-sftp` (async, pure Rust; younger) | `golang.org/x/crypto/ssh` + `pkg/sftp` (very mature, used by rclone) |
| SMB | Windows native API via `windows-rs`; pure-Rust `smb` / `smb2` crates exist | Windows native API via `x/sys/windows`; `go-smb2` |
| NFS | `nfs3_client` (NFSv3, async); `nfs-rs` (v3 plus experimental v4) | `vmware/go-nfs-client` (NFSv3) |
| GUI | Slint, egui, iced (native, small) | Fyne, Gio, Wails (webview) |
| Binary and memory | Smallest; no runtime | Larger; GC heap |
| Development speed | Slower | Faster |

**Decision: Rust.** A mounted drive sits on the hot path of every Explorer operation, and Rust gives predictable latency there with no GC. Rust also has native-API WinFsp bindings. The one weakness is the younger SFTP library, so it gets a go/no-go throughput spike before any product code is written. The fallback is a thin SFTP v3 client written directly on `russh` channels.

### 5.2 Drive layer: how a remote file system becomes a drive letter

| Option | Notes | Verdict |
|---|---|---|
| Wrap SSHFS-Win (as NEO does) | Least code, but brings back problems 1–5 from §3 and needs a separate install | Rejected |
| **WinFsp** with our own file system (`winfsp` crate) | Fast, mature (used by rclone, SSHFS-Win, cryptomator); supports network-drive mode through a volume prefix; mounts from an unelevated process | **Chosen** |
| Dokany (`dokan` crate) | Also mature. Library and driver are LGPL; the Rust crate is MIT. Generally slower than WinFsp in WinFsp's published benchmarks. | **Technical fallback only.** It is not needed for licensing, because GPLv3 was chosen (§6). |
| Windows Projected File System (ProjFS) | Built for hydrating placeholders (like OneDrive); not a general network file system; no drive letters | Rejected |
| Windows Cloud Files API | Sync-root model; files are downloaded locally; wrong model for live remote disks | Rejected |

### 5.3 SSH/SFTP client

- **`russh` + `russh-sftp` (chosen).** This is an async SSH stack in Rust with no OpenSSL. Its crypto backend does need a C compiler at build time: spike M0-2 uses `ring`, which builds with MSVC alone, because the default `aws-lc-rs` also needs NASM.
  - **SSH layer (`russh`):** public key, password, keyboard-interactive and agent auth, plus OpenSSH certificates. Jump hosts work through `direct-tcpip` channels.
  - **SFTP layer (`russh-sftp`):** supports the `limits@openssh.com`, `fsync@openssh.com` and `statvfs@openssh.com` extensions, which give us maximum packet size, durable writes and free-space reporting.
  - **Throughput:** depends on keeping many read/write requests in flight. This is verified in spike M0-2.
- **libssh2 (`ssh2` crate):** mature, but blocking. It also needs OpenSSL or WinCNG at build time and is slower per connection. Rejected.
- **Running OpenSSH `sftp.exe` as a subprocess:** this reintroduces external process management, the source of NEO's problem 2. Rejected.

### 5.4 SMB (milestone M3)

| Option | Notes | Verdict |
|---|---|---|
| **Windows' own SMB client** via `WNetAddConnection2W` | Native speed, oplocks, signing/encryption and Kerberos/NTLM, all handled by Windows. Credentials are passed through the API, never on a command line. The drive is a real Windows network drive. | **Chosen** |
| Pure-Rust client (`smb` 0.12, `smb2` 0.11) + WinFsp | Lets one user connect to the same server with different credentials. Duplicates what Windows already does well. | Later, only as a fallback for Windows error 1219 |
| `net use` subprocess | Credentials would appear in the process list | Rejected |

One Windows limitation must be handled in the UI: **error 1219**. Windows refuses a second connection to the same server under a different user name. The app must explain this clearly; see the specification.

### 5.5 NFS (milestone M4)

- **The Windows built-in "Client for NFS"** exists only on Pro, Enterprise and Education editions. **Windows Home does not have it**, and that is the user's own edition. It is NFSv3 only.
- **Chosen: a userspace NFSv3 client plus WinFsp.**
  - `nfs3_client` handles NFSv3 over TCP with AUTH_SYS, on Tokio. It is mounted through the same WinFsp adapter as SFTP.
  - It works on every Windows edition.
  - Windows has no privileged-port restriction, so the client can bind a source port below 1024 without admin rights. Many NFS exports require this through the `secure` option.
- **NFSv4.1** (`nfs-rs`, experimental) is a later option. It is attractive because it needs only one firewall port (2049).

### 5.6 GUI toolkit

| Option | Notes | Verdict |
|---|---|---|
| **Slint** | Declarative `.slint` markup; software renderer (about 30 MB RSS in one published comparison, vs about 135 MB for the same app in egui on wgpu); native-looking Fluent style; royalty-free licence for desktop apps; built-in translation support (`@tr`) | **Chosen** |
| egui | Very fast to build with, but immediate-mode and GPU-backed: larger memory, non-native look | Rejected |
| iced | Elm-style, good, but the API still changes between releases | Rejected |
| Tauri (webview) | Small exe, but WebView2 processes cost 100 MB+ of RAM | Rejected |
| Win32 / WinUI directly | Native, but slow to build and harder to maintain | Rejected |

The tray uses the `tray-icon` crate on the same Windows message loop, proven in spike M0-3. Slint 1.18 added its own `SystemTrayIcon`, which M2-03 will compare.

### 5.7 Configuration and secrets

- **Config: one TOML file.** It is human-readable, easy to back up and holds no secrets. NEO's encrypted SQLite was needed only because it kept secrets in the database.
- **Secrets: Windows Credential Manager.** It is protected with DPAPI by the user's Windows sign-in, and it is what Windows itself uses for saved network passwords. There is no master password and no app login.
- **Host keys:** the user's OpenSSH `known_hosts` file, so trust decisions are shared with `ssh.exe`.

## 6. Licensing analysis

| Component | Licence | Consequence |
|---|---|---|
| WinFsp | GPLv3 with a FLOSS exception (commercial licence available) | The exception lets an open-source app under **any** OSI/FSF-approved licence link the WinFsp DLL and redistribute the unmodified WinFsp installer, on two conditions: it shows "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" plus a repository link in its UI and docs, and it is not combined with proprietary software. A closed-source app needs a commercial licence. (Source: `License.txt` in the WinFsp install folder.) |
| `winfsp` / `winfsp-sys` crates | GPL-3.0, no exception | Linking them makes the distributed app GPLv3. **This**, not WinFsp itself, is what requires GPLv3. |
| Dokany (fallback) | LGPL (library and driver); `dokan` crate MIT | Allows a permissive or proprietary app |
| Slint | GPLv3, royalty-free (desktop, with attribution) or commercial | Used under its GPLv3 option |
| russh, russh-sftp, tokio, windows-rs, serde, toml, tracing | Apache-2.0 / MIT | No constraint |

**Decision (2026-11-03): GPLv3 open source.**
- FS Manager is licensed GPL-3.0-or-later and uses WinFsp through the `winfsp` crate, as specified.
- It also meets the FLOSS exception's conditions (attribution, no proprietary components), so the unmodified WinFsp installer can be bundled.
- Details and obligations: [ADR-0003](adr/0003-licence-gplv3.md).

The drive layer stays isolated in one crate (`fsm-winfsp`). A later switch to Dokany or a commercial WinFsp licence would therefore not affect the rest of the design.

## 7. Conclusions

1. NEO's problems come mostly from its architecture (wrapping external `sshfs.exe` processes, polling, a Python runtime packaged into one exe), not from bugs that a rewrite of the same design would avoid.
2. A Rust application that hosts its own file systems in-process through WinFsp, with Windows' native SMB client for SMB, removes those problems. It also opens the way to SMB and NFS behind one backend interface.
3. The riskiest assumptions are SFTP throughput with `russh-sftp`, network-drive mode from an unelevated process, and Slint + tray integration. Each gets a short go/no-go spike (milestone M0) before product work starts.
4. The licence model is decided: GPLv3 open source with WinFsp ([ADR-0003](adr/0003-licence-gplv3.md)).

## 8. Sources

- NEO SSH-Win Manager repository and README: https://github.com/gregorkrebs/NeoSSHWinManager
- NEO documentation (configuration, connections, troubleshooting, interface): https://www.neosshwinmanager.org/en/docs/configuration
- SSHFS-Win: https://github.com/winfsp/sshfs-win · WinFsp: https://github.com/winfsp/winfsp
- `winfsp` crate: https://docs.rs/crate/winfsp/latest · licence: https://docs.rs/crate/winfsp/latest/source/Cargo.toml
- `dokan` crate: https://docs.rs/crate/dokan/latest · Dokany licensing: https://en.wikipedia.org/wiki/Dokan_Library
- `russh-sftp`: https://docs.rs/russh-sftp
- `smb` crate: https://docs.rs/crate/smb/latest · `smb2` crate: https://docs.rs/crate/smb2/latest
- `nfs3_client`: https://docs.rs/nfs3_client/latest/nfs3_client/ · `nfs-rs`: https://docs.rs/crate/nfs-rs/latest
- NFS client availability on Windows editions: https://woshub.com/mount-nfs-share-in-windows
- Slint vs egui memory comparison: https://neura.market/directories/deepseek/blog/devto-3847725
- cgofuse: https://github.com/winfsp/cgofuse · go-winfsp: https://github.com/winfsp/go-winfsp
