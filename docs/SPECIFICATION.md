# FS Manager: Software Specification

| | |
|---|---|
| Document | Software specification |
| Status | Draft 2 (licence decided: GPLv3) |
| Date | 2026-11-03 |
| Product | FS Manager (working title), executable `fsm.exe` |
| Companion | [ANALYSIS.md](ANALYSIS.md): reference program, problems, technology rationale |

**Requirement keywords.** "Must" = required for the milestone. "Should" = expected unless there is a documented reason not to. "May" = optional.

---

## 1. Introduction

### 1.1 Purpose

FS Manager is a Windows desktop application that mounts remote disks as **drive letters**:
- it keeps a list of connections;
- it mounts or unmounts each one with a single click;
- it restores mounts at start-up and after network drops;
- it lives in the system tray.

It replaces tools such as NEO SSH-Win Manager with a native Rust program. That program needs no external SSHFS tooling and has a deliberately small UI.

### 1.2 Scope and releases

| Release | Content |
|---|---|
| **v1** (milestones M0–M2) | SSH/SFTP connections mounted as drive letters; tray; auto-mount; auto-reconnect; Windows Credential Manager; host-key verification; settings; MSI installer |
| **M3** | SMB 2/3 connections (Windows' own SMB client) |
| **M4** | NFSv3 connections (built-in userspace client) |

**Explicitly excluded** (not to be added without changing this document):
- SSH terminal, file browser, server system info, CLI;
- duplicate/clone profile, templates;
- FTP/FTPS/WebDAV/S3;
- multi-user accounts or an app login, telemetry;
- Linux/macOS.

### 1.3 Definitions

| Term | Meaning |
|---|---|
| Profile | A saved connection: protocol, server, credentials reference, remote path, drive letter, options |
| Mount | A live connection exposed as a drive letter |
| Backend | Code that implements one protocol (SFTP, SMB, NFS) |
| WinFsp | Windows File System Proxy, a user-mode file system framework (https://winfsp.dev) |
| Known hosts | `%USERPROFILE%\.ssh\known_hosts`, the OpenSSH file of trusted server keys |
| TOFU | Trust on first use: the user confirms a new server key once |
| NTSTATUS | Windows kernel status code returned by file system operations |

### 1.4 Platform requirements

| ID | Requirement |
|---|---|
| PLAT-01 | Must run on Windows 10 22H2 (all editions, including Home) and Windows 11 23H2 or later, x64. ARM64 may follow later. |
| PLAT-02 | Must require **WinFsp 2.1 or later**. It is not needed for SMB profiles (M3). |
| PLAT-03 | Must run as a normal, **unelevated** user process. It must never ask for elevation, except in the installer. |
| PLAT-04 | Must support display scaling from 100% to 200% and per-monitor DPI. |

---

## 2. Product overview

### 2.1 Typical use

1. The user installs FS Manager. The installer checks for WinFsp and offers to install it.
2. The user clicks **+ Add** and enters a host, user, authentication method, remote path and drive letter, then saves.
3. The user clicks **Mount**. On first contact, the user confirms the server's key fingerprint. Drive `Z:` appears in Explorer under *Network locations* with the profile's name as its label.
4. The user closes the window. The app stays in the tray, and the drive stays mounted.
5. After sleep or a Wi-Fi change, the drive reconnects by itself. If it cannot, a notification says why.

### 2.2 Design principles

- **One job.** Manage and mount remote disks. Everything else is out of scope (§1.2).
- **Event-driven.** No polling loops. State changes come from the connection, the file system or Windows notifications.
- **Nothing left behind.** Mounts are hosted inside the app process. If the process exits, the mounts go with it, so ghost drives are impossible.
- **Secrets stay with Windows.** Secrets go to Windows Credential Manager. They are never written to config files or logs, and never passed on command lines.
- **Protocols are plug-ins.** A new protocol adds a backend crate, a profile variant and form fields. Nothing else changes.

---

## 3. Functional requirements

Each requirement has an ID, a priority (M = must, S = should, May) and an acceptance criterion (AC).

### 3.1 Profiles (FR-PROF)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-PROF-01 | M | The user can create, edit and delete profiles. | A profile created in the dialog appears in the list, survives an app restart, and is gone after Delete. |
| FR-PROF-02 | M | Each profile has a stable UUID that never changes, including on rename. | Renaming a profile keeps its stored credentials working. |
| FR-PROF-03 | M | Profile names are unique, compared case-insensitively. | Saving a second "Production" or "production" shows an inline error. |
| FR-PROF-04 | M | All fields are validated before save (rules in §7.3). Invalid fields are marked inline and Save is disabled. | Each rule in §7.3 has a unit test. |
| FR-PROF-05 | M | Pasting `user@host:port/path` or `sftp://user@host:port/path` into the Host field splits it into the matching fields. | Pasting `deploy@10.0.4.21:2222/var/www` fills user, host, port and path. |
| FR-PROF-06 | M | **Test connection** connects, authenticates and checks that the remote path exists and is a directory, without mounting. It reports the result in the dialog: OK with round-trip time, or the error from §7.8. | Wrong password → "Authentication failed"; missing path → "Remote path not found". |
| FR-PROF-07 | M | Deleting a profile also deletes its Credential Manager entries. A mounted profile is unmounted first, after confirmation. | After delete, no `FsManager:<uuid>:*` credentials remain. |
| FR-PROF-08 | M | While a profile is mounted, connection fields are read-only, with the hint "Unmount to change". These fields are protocol, host, port, user, authentication, remote path, drive letter, jump host and read-only. Name, group and the start/reconnect flags stay editable. | Editing the host of a mounted profile is impossible; renaming is possible. |
| FR-PROF-09 | S | Optional **group** label; the list groups rows under collapsible headers. | Profiles with group "Web" are shown under a "Web" header. |

### 3.2 SSH/SFTP backend (FR-SSH), v1

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-SSH-01 | M | Auth method **Password**: the password is entered in the dialog, and "Remember" stores it in Credential Manager. Without "Remember", it is asked at every mount. | Mount works with a stored password and with a prompted one. |
| FR-SSH-02 | M | Auth method **Key file**: the key is OpenSSH or PEM format (Ed25519, ECDSA, RSA ≥ 2048 with rsa-sha2-256/512). A passphrase is prompted at mount; "Remember" stores it in Credential Manager. The decrypted key is held only in memory while mounted. | Passphrase-protected Ed25519 key mounts after one prompt; nothing is prompted on reconnect. |
| FR-SSH-03 | M | PuTTY `.ppk` files are rejected with the hint "Convert with PuTTYgen → Conversions → Export OpenSSH key". | Selecting a `.ppk` shows the hint, and Save is disabled. |
| FR-SSH-04 | M | Auth method **SSH agent**: uses the Windows OpenSSH agent (`\\.\pipe\openssh-ssh-agent`). It should also use Pageant if it is running. | With the key loaded in ssh-agent, mount succeeds without any prompt. |
| FR-SSH-05 | M | Auth method **Ask each time**: nothing is stored. The prompt offers password, or a key file plus passphrase. | No credential entries exist for such profiles. |
| FR-SSH-06 | M | **OpenSSH certificates**: if `<key>-cert.pub` exists next to the key file, it is offered to the server. | Login to a server that only accepts certificates succeeds. |
| FR-SSH-07 | M | **Keyboard-interactive** (e.g., 2FA): server prompts are shown in the prompt dialog with the server's text. A prompt whose text matches `password` is answered from the stored password if one exists. Other answers are never stored. | A TOTP-protected server mounts after the code is entered. |
| FR-SSH-08 | M | **Host keys** are checked against known hosts, with plain and hashed entries and the `[host]:port` form. `@revoked` lines must be honoured. | See FR-SSH-09/10. |
| FR-SSH-09 | M | **Unknown host key**: the trust dialog (§7.4.2) offers *Trust and save*, *Trust once* and *Cancel*. *Trust and save* appends a line to known hosts. | After *Trust and save*, a second mount asks nothing. `ssh.exe` also accepts the server without asking. |
| FR-SSH-10 | M | **Changed host key**: the connection is refused (E-HOSTKEY-CHANGED). The message shows the old and new SHA256 fingerprints and the file and line number. There is no override button. | Replacing the server key causes a refusal, and no data is sent after key exchange. |
| FR-SSH-11 | M | **Jump host** (optional, one hop): host, port, user and its own auth (agent, key file or password). The password is stored as `FsManager:<uuid>:jump-password`. The jump host's key is verified like any other host. | Mount through a bastion succeeds, and the bastion's key appears in known hosts. |
| FR-SSH-12 | M | **Keep-alive**: an SSH keep-alive is sent every `keepalive_secs` (default 15 s, 0 = off). Three unanswered keep-alives mark the connection as lost. | With the network cable pulled, the state changes to *Reconnecting* within 60 s. |
| FR-SSH-13 | M | **Remote path**: `~` (the default) means the user's home directory, resolved with SFTP `realpath`. Paths are absolute POSIX paths. For Windows OpenSSH servers, `C:/data` or `C:\data` is normalised to `/C:/data`. | Mounting `~` on Windows OpenSSH shows the user's profile folder. |
| FR-SSH-14 | M | **SFTP extensions** are used when the server advertises them, with fallbacks: `limits@openssh.com` (packet sizes), `posix-rename@openssh.com` (atomic rename-replace), `fsync@openssh.com` (flush), `statvfs@openssh.com` (free space). | Tests pass against OpenSSH (has extensions) and ProFTPD mod_sftp (lacks them). |
| FR-SSH-15 | M | Supported servers: OpenSSH 7.4+, Windows OpenSSH (Windows Server 2019+/Windows 10+), and other SFTP v3 servers. | Test matrix in §10.4. |

### 3.3 Mounting (FR-MNT)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-MNT-01 | M | **Mount/Unmount** toggles from the row button, the tray menu, the Enter key, or the ⋯ menu. | Two clicks or fewer from the window or tray. |
| FR-MNT-02 | M | Each mount gets its drive letter: the profile's letter, or *Auto*, which picks the highest free letter from Z downwards. | *Auto* with Z: free → Z:. |
| FR-MNT-03 | M | **Free letter** means not present in `GetLogicalDrives()` *and* not a remembered network connection (`WNetGetConnectionW` ≠ `ERROR_NOT_CONNECTED`). A, B and C are never used. | A disconnected persistent `net use Y:` mapping makes Y: count as taken. |
| FR-MNT-04 | M | **Letter taken** at mount time: behaviour depends on the `letter_conflict` setting. *auto* (default) mounts on the next free letter, shows "Y: (Z: was in use)" in the row and sends a notification. *fail* stops with E-LETTER-TAKEN. | Both behaviours are covered by tests. |
| FR-MNT-05 | M | Mounts appear as **network drives**: WinFsp volume prefix `\fsm\<profile-slug>`, shown in Explorer under *Network locations*. The volume label is the profile name, truncated to 32 characters. Explorer names network drives after the UNC path ("slug (\\fsm)"), not the volume label, so the profile name is also written to `_LabelFromReg` under `HKCU\…\Explorer\MountPoints2\##fsm#<slug>` (verified in M1-12c; spike M0-1 §7). | Explorer shows "Production (Z:)" with a network-drive icon. |
| FR-MNT-06 | M | An already-running Explorer is told about drive changes at once. Spike M0-1 showed that a WinFsp letter created with `DefineDosDevice` stays invisible to a running Explorer, and `SHChangeNotify` from another process did not help. The mechanism is chosen in M1-12c: first candidate, assign the letter with `WNetAddConnection2W` through WinFsp's network provider (the `net use` path); otherwise `SHChangeNotify(SHCNE_DRIVEADD / SHCNE_DRIVEREMOVED)` plus an **asynchronous** device broadcast. Synchronous `WM_DEVICECHANGE` broadcasts are forbidden, because they hang on unresponsive windows. | An open Explorer window shows or hides the drive within 1 s, without restarting Explorer. |
| FR-MNT-07 | M | **Read-only** profiles mount a read-only volume. Writes fail with "media is write protected" (`STATUS_MEDIA_WRITE_PROTECTED`). | Copying into a read-only drive fails cleanly. |
| FR-MNT-08 | M | **Mount on start**: profiles with `mount_on_start` are mounted in parallel when the app starts. Profiles that need a prompt queue their prompts one at a time. | 5 such profiles mount in parallel at start-up. |
| FR-MNT-09 | M | **Mount all** (toolbar) mounts every unmounted profile in the current filter. **Unmount all** (toolbar and tray) unmounts everything. | Mount all with search "web" mounts only the matching profiles. |
| FR-MNT-10 | M | **Unmount** flushes open handles and stops the WinFsp host. Unmount is not blocked by files Explorer has open; those handles are invalidated. | Unmount with a file open in Notepad completes; Notepad's next save shows an error. |
| FR-MNT-11 | M | **Cancel**: a mount in *Connecting* can be cancelled from the row. | Cancel during a slow handshake returns to *Disconnected* within 1 s. |
| FR-MNT-12 | M | **Open in Explorer** opens the drive root (`explorer.exe Z:\`). | An Explorer window opens at Z:\ showing the contents of the remote path. |

### 3.4 Connection state and reconnect (FR-REC)

States, transitions and events are defined in §5.5.

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-REC-01 | M | A lost connection is detected by keep-alive timeout, transport error, or an operation timeout followed by a failed keep-alive probe. | Each of the three triggers, simulated in tests, moves a *Mounted* profile to *Reconnecting* (or to *Error* when auto-reconnect is off). |
| FR-REC-02 | M | With `auto_reconnect = true` (default), a lost connection keeps the drive letter and enters *Reconnecting*. Attempts back off 1, 2, 4, 8, 16, 32, then every 60 s, without limit. | Server restart → drive comes back without user action. |
| FR-REC-03 | M | When Windows resumes from sleep (`PBT_APMRESUMEAUTOMATIC`) or a network interface changes (`NotifyIpInterfaceChange`), every *Reconnecting* mount retries **at once**, and every *Mounted* SFTP mount is probed with a keep-alive. | Resume from sleep → drive usable within 5 s after the network is back. |
| FR-REC-04 | M | While *Reconnecting*, file system calls wait for the reconnect up to the operation timeout (default 15 s). After that they fail with `STATUS_NETWORK_UNREACHABLE`. **No call blocks longer than the timeout.** | With the cable pulled, Explorer browsing the drive shows an error within 15 s and never freezes. |
| FR-REC-05 | M | After a reconnect, open file handles are reopened by path (never truncated) when they are next used. Directory enumerations restart. | Reading a large file across a 10 s outage resumes and the copy's hash matches. |
| FR-REC-06 | M | Authentication failures (wrong password, key removed) and host-key changes during reconnect **stop** retries. They set *Error* and send a notification. | Changing the password on the server → Error "Authentication failed", no retry loop. |
| FR-REC-07 | M | With `auto_reconnect = false`, a lost connection unmounts the drive and sets *Error (Connection lost)*. | Stopping sshd on the server removes the drive letter, the row shows E-CONN-LOST, and a toast appears. |

### 3.5 Tray (FR-TRAY)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-TRAY-01 | M | Tray icon is always present while the app runs. Its icon variant shows the overall state: *idle* (no mounts), *mounted*, *reconnecting* (any), *error* (any). | With a mock backend driven through the four states, the icon variant matches each state. |
| FR-TRAY-02 | M | Tooltip: "FS Manager: 2 mounted, 1 reconnecting". | Tooltip counts always equal the status-bar counts. |
| FR-TRAY-03 | M | Left click shows or hides the main window. | A hidden window is shown and focused; a visible one is hidden. |
| FR-TRAY-04 | M | Right-click menu (§7.6): one item per profile with a check mark when mounted (click toggles), *Unmount all*, *Open FS Manager*, *Quit*. | Toggling a profile from the tray mounts it without opening the window, unless a prompt is needed. |

### 3.6 Notifications (FR-NOT)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-NOT-01 | M | A Windows toast is shown when a mount fails, a connection is lost without auto-reconnect, reconnect stops (FR-REC-06), or a letter is reassigned (FR-MNT-04). | Each of the four events produces exactly one toast naming the profile and the error text from §7.8. |
| FR-NOT-02 | M | No toasts for successful mounts or successful reconnects. | Mount-on-start of 5 profiles produces no toasts. |
| FR-NOT-03 | M | Toasts can be switched off in Settings. | With notifications off, the FR-NOT-01 events produce no toasts; row errors are still shown. |
| FR-NOT-04 | M | Clicking a toast opens the main window with the profile selected. Spike M0-3: works while the toast is on screen; a click from Action Center does nothing so far (to settle in M2-04). | Clicking a toast for "Dev box" shows the window with that row selected. |

### 3.7 Application lifecycle (FR-APP)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-APP-01 | M | **Single instance** per Windows user, enforced with the named mutex `Local\FsManager`. A second launch brings the existing window to the front and exits. | Starting `fsm.exe` twice leaves one process, with its window in front. |
| FR-APP-02 | M | **Close to tray** (default on): the window's close button hides the window. With it off, closing quits (FR-APP-04). | With the setting on, × hides the window and the mounts stay; with it off, × starts the quit flow. |
| FR-APP-03 | M | **Start with Windows** (default off) writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\FsManager = "<exe>" --tray`. `--tray` starts without showing the window. | After sign-in, the tray icon appears, no window is shown, and mount-on-start profiles are mounted. Turning the setting off removes the Run value. |
| FR-APP-04 | M | **Quit** with mounts present asks "Unmount N drives and quit?". On confirm, it unmounts everything and exits. | After confirming, no FS Manager drive letters remain and the process has exited. Cancel keeps everything as it was. |
| FR-APP-05 | M | **Windows sign-out or shutdown** (`WM_QUERYENDSESSION`/`WM_ENDSESSION`): flush and unmount everything without asking, within 5 s. | Signing out while a file is being written does not block sign-out. The written part of the file is intact on the server. |
| FR-APP-06 | M | **WinFsp missing or too old**: the main window shows a banner "WinFsp 2.1 or later is required to mount SSH drives" with a *Download WinFsp* button that opens https://winfsp.dev/rel/. Mount actions for WinFsp-backed profiles are disabled. | With WinFsp uninstalled, the banner shows and Mount buttons are disabled; profiles can still be edited. |
| FR-APP-07 | M | **Crash handling**: a panic hook writes `crash-<timestamp>.txt` to the logs folder. The next start shows a notice with *Open logs folder*. | A forced panic (debug build) produces the file, and the notice appears on the next start. |

### 3.8 Settings (FR-SET)

A single settings page (§7.5). The settings are stored in `config.toml` (§6.1).

| ID | Pri | Setting | Default |
|---|---|---|---|
| FR-SET-01 | M | Start with Windows | Off |
| FR-SET-02 | M | Close button hides to tray | On |
| FR-SET-03 | M | Theme: System / Light / Dark | System |
| FR-SET-04 | M | When a drive letter is in use: Use another free letter / Don't mount | Use another free letter |
| FR-SET-05 | M | Show notifications | On |
| FR-SET-06 | M | Log level: Error / Warning / Info / Debug, plus an *Open logs folder* button | Info |
| FR-SET-07 | M | WinFsp status (read-only line): "WinFsp 2.1.25156 installed", or the missing banner | n/a |

Acceptance for every setting: the value persists across restarts and takes effect without restarting the app.

These settings live only in `config.toml` and are not shown in the UI: `op_timeout_secs` (default 15, range 5–120) and `winfsp_threads` (default = CPU cores, max 8).

### 3.9 SMB backend (FR-SMB), milestone M3

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-SMB-01 | M | Profile fields: server, share, optional sub-path, entered as fields or pasted as `\\server\share\sub`. Also domain (optional), user, auth method, drive letter, read-only flag, and the start/reconnect flags. | Pasting `\\nas\media\films` fills server, share and sub-path. |
| FR-SMB-02 | M | Auth methods: *Password* (optionally remembered), *Ask each time*, *Current Windows user* (Kerberos/NTLM single sign-on). | Each method mounts a Samba share; *Current Windows user* mounts a domain share without a prompt. |
| FR-SMB-03 | M | Mount uses `WNetAddConnection2W` with `CONNECT_TEMPORARY`, so it is never persisted by Windows. The credentials go in the API call. **No `net use` subprocess.** | Credentials never appear in any process command line. |
| FR-SMB-04 | M | Unmount uses `WNetCancelConnection2W` (force after a confirmation if files are open). | Unmount with a file open asks first, then removes the mapping. |
| FR-SMB-05 | M | Health check: the mount manager subscribes to the same Windows network events. It reports *Disconnected* when `GetDiskFreeSpaceExW` on the share root fails with a network error. **Windows' SMB client does its own reconnecting**; the app only reflects the state. | With the server offline, the row shows *Disconnected* after the next network event; it returns to *Mounted* when the server is back. |
| FR-SMB-06 | M | Windows error **1219** (another connection to this server with a different user name) maps to E-SMB-1219. The message explains the cause and suggests using the server's IP address or another DNS alias. | Mapping `\\nas\a` as user1 and then `\\nas\b` as user2 shows the E-SMB-1219 text. |
| FR-SMB-07 | M | On Quit or sign-out, the app removes all SMB mappings it created. | No mapping remains after Quit. |
| FR-SMB-08 | S | SMB profiles work without WinFsp installed. | With WinFsp uninstalled, SMB profiles mount normally; only SSH/NFS rows are disabled. |

### 3.10 NFS backend (FR-NFS), milestone M4

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-NFS-01 | M | Profile fields: server, export path, NFS version (v3 only in M4), UID, GID (default 1000/1000), drive letter, read-only flag, and the start/reconnect flags. Advanced: fixed NFS port, fixed mount port (else portmapper on 111), *use reserved source port* (default on). | A server with portmapper disabled mounts with fixed ports set. Files created through the drive are owned by the configured UID/GID on the server. |
| FR-NFS-02 | M | NFSv3 over TCP with AUTH_SYS, mounted through the same WinFsp adapter as SFTP. It must work on Windows **Home** editions. | Mount of a Linux `knfsd` export on Windows 10 Home. |
| FR-NFS-03 | M | Reserved source port: bind 1023 downwards until a port is free. On Windows this needs no administrator rights. | Mount of an export with the `secure` option succeeds. |
| FR-NFS-04 | M | Reconnect follows FR-REC. NFS file handles stay valid across reconnects (stateless v3), so open handles keep working without reopening. | A copy that runs across a 10 s server restart completes, and the hashes match. |
| FR-NFS-05 | M | `NFS3ERR_ACCES` at mount maps to E-NFS-ACCESS, with hints (client IP not in exports, `secure` option, wrong UID/GID). | Mounting an export that doesn't list the client's IP shows the E-NFS-ACCESS text. |

---

## 4. Non-functional requirements

### 4.1 Performance (NFR-PERF)

**Test environment.**
- **Client:** Windows 11, 8 cores.
- **Server:** Ubuntu 24.04 with OpenSSH 9.x on a 1 GbE LAN (≤ 1 ms RTT).
- **WAN case:** the same link with 50 ms of RTT added by netem.

| ID | Requirement | Budget |
|---|---|---|
| NFR-PERF-01 | Cold start to window visible (from process creation to first frame, logged) | < 250 ms |
| NFR-PERF-02 | Cold start with `--tray` to tray icon visible | < 150 ms |
| NFR-PERF-03 | Size of `fsm.exe` (release, stripped) | < 15 MB |
| NFR-PERF-04 | Idle working set after 60 s: no mounts / each additional idle mount | < 40 MB / < 8 MB |
| NFR-PERF-05 | CPU while idle with 5 mounts | < 0.1% average over 10 min; no periodic wake-ups apart from keep-alives |
| NFR-PERF-06 | Sequential read and write of a 1 GB file through Explorer copy, compared with OpenSSH `sftp` get/put on the same link | ≥ 90% (LAN and WAN) |
| NFR-PERF-07 | Copying 10,000 × 4 KB files to the drive (robocopy) | ≤ 1.3× the time of `scp -r` |
| NFR-PERF-08 | Listing a directory of 1,000 entries: first listing on LAN / repeated within cache TTL | < 300 ms / < 20 ms |
| NFR-PERF-09 | Opening a mounted drive root in Explorer on LAN | < 500 ms until contents are shown |
| NFR-PERF-10 | UI responsiveness: no UI-thread work > 16 ms. All network and file system work runs off the UI thread. | Verified with tracing spans |

### 4.2 Reliability (NFR-REL)

| ID | Requirement |
|---|---|
| NFR-REL-01 | **Data integrity**: for any file written through the drive, close → reopen → SHA-256 equals the source. This is tested for sizes 0 B, 1 B, 4095 B, 4 KiB, 1 MiB + 1 B and 1 GiB, and with `fsx` (§10.3). |
| NFR-REL-02 | **Visibility**: a file created, renamed or deleted through the drive shows in the very next directory listing (no stale cache from our own changes). |
| NFR-REL-03 | **Bounded waits**: no WinFsp callback blocks longer than `op_timeout_secs`. |
| NFR-REL-04 | **No orphans**: after the process exits for any reason, including a crash or a kill from Task Manager, no FS Manager drive letter remains. |
| NFR-REL-05 | **Config safety**: config writes are atomic (write a temp file, then `ReplaceFileW`). The previous good file is kept as `config.toml.bak`. A config that cannot be parsed is never overwritten; the app starts read-only and shows an error. |
| NFR-REL-06 | **Soak**: 72 h with 5 mounts, periodic network drops and sleep/resume cycles. There must be no leak (working set growth < 10%) and no stuck state. |

### 4.3 Security (NFR-SEC)

| ID | Requirement |
|---|---|
| NFR-SEC-01 | Passwords and passphrases are stored **only** in Windows Credential Manager. They use `CRED_TYPE_GENERIC` and `CRED_PERSIST_LOCAL_MACHINE` (stored only on this PC, not synced as a roaming credential), with targets `FsManager:<uuid>:password`, `FsManager:<uuid>:passphrase` and `FsManager:<uuid>:jump-password`. |
| NFR-SEC-02 | Secrets are never written to config files or logs and never passed on command lines. In code, secrets are wrapped in a type whose `Debug`/`Display` prints `***` and which zeroes its memory on drop (`zeroize`). |
| NFR-SEC-03 | Decrypted private keys and passphrases are held in memory only while the profile is mounted, and are zeroed on unmount. |
| NFR-SEC-04 | Host keys are always verified (FR-SSH-08 to FR-SSH-10). There is no option to turn verification off. |
| NFR-SEC-05 | The app runs unelevated. Mounts belong to the user's logon session and are not visible to other users. |
| NFR-SEC-06 | No telemetry, no update checks, no network traffic other than to the configured servers. |
| NFR-SEC-07 | Dependencies are checked in CI with `cargo deny` (advisories and licences) and `cargo audit`. Release binaries and the MSI are Authenticode-signed. |

### 4.4 Usability (NFR-UX)

| ID | Requirement |
|---|---|
| NFR-UX-01 | Mount or unmount takes ≤ 2 clicks from the window or the tray. |
| NFR-UX-02 | The UI consists of the main window, one profile dialog, one settings page and small prompts. There are no other windows. |
| NFR-UX-03 | Every error message says what happened and what to do (catalogue in §7.8). No raw error codes appear without explanation. |
| NFR-UX-04 | Everything can be done with the keyboard (§7.7). Every control has an accessible name for screen readers. |
| NFR-UX-05 | English only in v1. All strings are wrapped in Slint `@tr()` / `tr!` so translations can be added without code changes. |

### 4.5 Maintainability (NFR-MAINT)

| ID | Requirement |
|---|---|
| NFR-MAINT-01 | Rust stable, edition 2024. The minimum Rust version (MSRV) is pinned in `rust-toolchain.toml`. |
| NFR-MAINT-02 | CI runs `cargo fmt --check` and `cargo clippy -- -D warnings`. |
| NFR-MAINT-03 | Protocol crates (`fsm-vfs`, `fsm-sftp`, `fsm-nfs`) must not depend on Windows APIs, so they build and test on Linux. |
| NFR-MAINT-04 | Structured logging with `tracing`; each mount is a span carrying the profile ID and name. |

### 4.6 Licensing (NFR-LIC)

FS Manager is open source under **GPL-3.0-or-later**, with WinFsp used through the `winfsp` crate. See [ADR-0003](adr/0003-licence-gplv3.md).

| ID | Requirement | Acceptance criterion |
|---|---|---|
| NFR-LIC-01 | `LICENSE` (full GPLv3 text) in the repository root; `license = "GPL-3.0-or-later"` in every crate; an SPDX header (`// SPDX-License-Identifier: GPL-3.0-or-later`) in every source file. | A CI check fails when a `.rs` or `.slint` file lacks the header. |
| NFR-LIC-02 | **WinFsp attribution** (a condition of WinFsp's FLOSS exception): the About section and the user guide show "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" with a link to https://github.com/winfsp/winfsp. | Visible in About and in `USER_GUIDE.md`. |
| NFR-LIC-03 | The About section shows "Licensed under the GNU GPL v3", a link to the source repository, and a button that opens the bundled third-party licence file. | Button opens `THIRD-PARTY-LICENSES.html`. |
| NFR-LIC-04 | `THIRD-PARTY-LICENSES.html` is generated by `cargo-about` at build time and shipped in the MSI and the portable zip. | Present in both artefacts of every release. |
| NFR-LIC-05 | No proprietary component is linked or bundled. `cargo deny` enforces the licence allow-list from ADR-0003 in CI. | A dependency with a non-allowed licence (e.g. `GPL-2.0-only`) fails CI. |
| NFR-LIC-06 | Every release's source is public: the release tag exists in the public repository, and the release notes link to it. | Checked in the release checklist. |

---

## 5. Architecture

### 5.1 Overview

```
┌────────────────────────────── fsm.exe (one unelevated process) ──────────────────────────────┐
│                                                                                              │
│  UI thread                          Tokio runtime (2–4 workers)        WinFsp dispatcher      │
│  ┌──────────────────────┐  commands ┌──────────────────────────┐       threads (per mount)    │
│  │ Slint main window     │─────────▶│ MountManager             │       ┌──────────────────┐  │
│  │ Profile dialog        │          │  • state machine/profile │       │ fsm-winfsp       │  │
│  │ Prompts               │◀─────────│  • reconnect scheduler   │       │ FileSystemContext│  │
│  │ tray-icon             │  events  │  • letter allocator      │       │  → RemoteFs      │  │
│  │ Win32 msgs (power,    │          │ ConfigStore  SecretStore │       │  (block_on with  │  │
│  │ session, net change)  │          └────────────┬─────────────┘       │   deadline)      │  │
│  └──────────────────────┘                        │ MountBackend        └────────┬─────────┘  │
│                                    ┌─────────────┼───────────────┐              │            │
│                                    ▼             ▼               ▼              │            │
│                              fsm-sftp       fsm-nfs (M4)    fsm-smb (M3)        │            │
│                              RemoteFs       RemoteFs        native WNet         │            │
│                              (russh)        (nfs3_client)   mapping             │            │
│                                    └──── fsm-vfs: caches, timeouts, names ◀──────┘            │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
          ▲ SSH :22                ▲ NFS :2049/:111                ▲ SMB :445 (by Windows)
```

### 5.2 Workspace layout

```
fs-manager/
  Cargo.toml                 workspace: pinned dependencies, lints, release profile
  rust-toolchain.toml  rustfmt.toml  clippy.toml  .cargo/config.toml
  deny.toml                  cargo-deny config (licences, advisories)
  LICENSE                    GPLv3 text
  .github/workflows/ci.yml   CI (GitHub-Actions-compatible; run by Gitea Actions)
  scripts/                   CI checks: SPDX headers, exe size
  crates/
    fsm-vfs/                 Shared traits and types, at the bottom of the graph: RemoteFs, Connector,
                             MountBackend, Prompter, Secret, Attr/DirEntry; caches, read-ahead, name and
                             permission mapping, operation deadlines, error model     (no Windows deps)
    fsm-core/                Profile model, ConfigStore (TOML), SecretStore trait, MountManager, state
                             machine, letter allocator, event bus. Gets its backends from a factory
                             supplied by the app                                      (no Windows deps)
    fsm-sftp/                RemoteFs over russh + russh-sftp; connector (auth, jump host, host keys,
                             keep-alive)                                                (no Windows deps)
    fsm-winfsp/              Adapter RemoteFs → winfsp::FileSystemContext; volume params; mount/unmount
    fsm-smb/                 [M3] WNetAddConnection2W / WNetCancelConnection2W / health check
    fsm-nfs/                 [M4] RemoteFs over nfs3_client (MOUNT, NFSv3, portmap, AUTH_SYS)
    fsm-win/                 Small Windows helpers: Credential Manager, Run key, SHChangeNotify, power,
                             session and network notifications, single-instance mutex, toasts
  apps/
    fsm/                     GUI binary (windows subsystem): Slint UI (.slint files), tray, wiring,
                             BackendFactory (Profile → Box<dyn MountBackend>)
  docs/
    ANALYSIS.md  SPECIFICATION.md  IMPLEMENTATION_PLAN.md  TRACEABILITY.md  PROGRESS.md  adr/  spikes/
  installer/                 WiX sources for the MSI
  tests/                     cross-crate integration tests, Docker compose files for test servers
```

Dependencies between the crates. Only `fsm-vfs` is shared, so there is no cycle, and `fsm-core` stays platform-independent, with its tests running on Linux:

```
fsm-vfs ◀── fsm-sftp, fsm-nfs                 (portable)
fsm-vfs ◀── fsm-core                          (portable)
fsm-vfs ◀── fsm-winfsp, fsm-smb, fsm-win      (Windows; empty crates on other platforms)
apps/fsm ──▶ all of the above
```

### 5.3 Process and threading model

- **One process** holds the UI and the engine. It runs unelevated in the user's session, so drive letters land in the user's logon session and are visible to their Explorer.
- **UI thread**: runs the Slint event loop, `tray-icon`, and a hidden message window. That window is a hidden top-level window, not a message-only one, and receives `WM_POWERBROADCAST`, `WM_QUERYENDSESSION`/`WM_ENDSESSION` and the "show window" message from a second instance. Toast clicks arrive through the toast's `Activated` handler, not on that window. Everything reaches Slint through `invoke_from_event_loop`.
- **Tokio runtime**: multi-threaded with 2–4 workers. All network I/O, the mount manager and the reconnect timers run here.
- **WinFsp dispatcher threads**: WinFsp calls our `FileSystemContext` on its own thread pool, sized by `winfsp_threads`. Each callback runs `runtime_handle.block_on(timeout(op_deadline, fut))`. Dispatcher threads are not Tokio threads, so `block_on` is allowed there.
- **UI ↔ engine**:
  - Commands go from the UI to the engine over an `mpsc` channel: `Mount(id)`, `Unmount(id)`, `Cancel(id)`, `Test(profile)`, `SaveProfile`, `Quit`…
  - State events come back over a `broadcast` channel: `MountEvent { id, state, detail }`. The UI applies them with `slint::invoke_from_event_loop`.
- **Prompts**: backends never touch the UI. They call the `Prompter` trait, which the GUI implements by showing a dialog and awaiting a `oneshot` reply. Prompts are serialised, so there is one dialog at a time.

### 5.4 Core abstractions (summary; `fsm-vfs` holds the signatures and their full contract)

```rust
/// Async file system operations on a remote. Implemented by fsm-sftp and fsm-nfs.
#[async_trait]
pub trait RemoteFs: Send + Sync + 'static {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr>;            // follows symlinks
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr>;
    async fn fstat(&self, h: &RemoteHandle) -> FsResult<Attr>;
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>>;
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle>;
    async fn read(&self, h: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes>;
    async fn write(&self, h: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()>;
    async fn flush(&self, h: &RemoteHandle) -> FsResult<()>;
    async fn close(&self, h: RemoteHandle) -> FsResult<()>;
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr>;
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr>;
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()>;
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()>;
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()>;
    async fn statfs(&self) -> FsResult<FsStats>;
    fn capabilities(&self) -> Capabilities;   // posix_rename, fsync, statvfs, max_read, max_write
}

/// Creates (and re-creates on reconnect) a RemoteFs session for one profile.
#[async_trait]
pub trait Connector: Send + Sync + 'static {
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError>;
    async fn probe(&self) -> Result<Duration, ConnectError>;   // keep-alive round trip
}

/// What the MountManager drives. Two kinds of implementation:
///  - WinFspBackend<C: Connector>: hosts a RemoteFs as a drive (SFTP, NFS)
///  - SmbBackend: native Windows mapping (M3)
#[async_trait]
pub trait MountBackend: Send + Sync + 'static {
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError>;
    async fn unmount(&self) -> Result<(), MountError>;
    fn health(&self) -> watch::Receiver<Health>;   // Ok | Lost(reason)
}

/// UI-mediated user interaction; implemented by the GUI.
#[async_trait]
pub trait Prompter: Send + Sync {
    async fn secret(&self, req: SecretRequest) -> Option<(Secret<String>, Remember)>;
    async fn login(&self, req: LoginRequest) -> Option<LoginAnswer>;   // Ask each time: password, or key file + passphrase
    async fn keyboard_interactive(&self, req: KbdIntRequest) -> Option<Vec<Secret<String>>>;
    async fn host_key(&self, req: HostKeyRequest) -> HostKeyDecision;   // TrustSave | TrustOnce | Reject
}
```

Async functions in traits need `dyn` dispatch here, so the traits use `async-trait` ([ADR-0001](adr/0001-async-traits.md)).

`RemotePath` is always absolute and normalised. `..` is resolved lexically, and a `..` above the root is an error, not clamped to the root. `AttrChanges` carries the Windows read-only attribute as a flag rather than permission bits, so the backend applies it to the server's current permissions (§5.6) and not to a cached copy.

### 5.5 Mount state machine

States: `Disconnected`, `Connecting`, `Mounted`, `Reconnecting`, `Unmounting`, `Error(reason)`.

| From | Event | To | Action |
|---|---|---|---|
| Disconnected / Error | Mount requested | Connecting | allocate letter, `connect()`; prompts if needed |
| Connecting | Connected + mounted | Mounted | `SHChangeNotify(DRIVEADD)` |
| Connecting | Cancel | Disconnected | drop connection |
| Connecting | Error (auth, host key, unreachable, path…) | Error(reason) | toast (FR-NOT-01) |
| Mounted | Unmount requested | Unmounting | flush, stop host |
| Unmounting | Done | Disconnected | `SHChangeNotify(DRIVEREMOVED)`, zero secrets |
| Mounted | Connection lost, auto_reconnect | Reconnecting | keep the letter; start backoff |
| Mounted | Connection lost, no auto_reconnect | Error(Connection lost) | unmount, toast |
| Reconnecting | Reconnected | Mounted | reopen handles lazily |
| Reconnecting | Resume or network change | Reconnecting | retry at once, reset backoff |
| Reconnecting | Auth failure / host key changed | Error(reason) | unmount, toast; no further retries |
| Reconnecting | Unmount requested | Unmounting | stop retries |
| any | App quit / sign-out | Unmounting → (exit) | FR-APP-04/05 |

Every transition is emitted as a `MountEvent` and logged at Info level.

### 5.6 VFS layer (fsm-vfs)

**Caching.** All caches are per mount and in memory.

| Cache | Default TTL | Invalidation |
|---|---|---|
| Attributes (path → Attr) | `cache_ttl_secs` (5 s; 0–60) | Any local create/write/set_attr/rename/remove on the path, and flush or close after a write. Rename also invalidates the destination. A new or removed entry also invalidates its directory's attributes. |
| Directory listings | same TTL | Any local change to an entry inside the directory, including a write, because listings show size and time |
| Negative lookups (path not found) | 1 s; 60 s for Explorer "noise" names | Local create or rename onto the path; also a listing that shows the name, or opening the file |

- **Explorer noise names** get the longer negative TTL: `desktop.ini`, `Desktop.ini`, `thumbs.db`, `Thumbs.db`, `autorun.inf`, `AutoRun.inf`, `folder.jpg`, `folder.gif`, `$RECYCLE.BIN`, `System Volume Information`. They are looked up normally first, so real files with these names still work.
- **What is cached.** `stat` and `read_dir` only; `lstat` always reaches the server. A listing also fills the attributes of its entries, except symbolic links, whose entries do not describe the target. A TTL of 0 turns attributes and listings off; the negative TTLs stay.
- **Whole subtrees.** Rename, `remove_dir` and `mkdir` invalidate everything cached below the path as well as the path itself. Open handles follow local renames, so a later write invalidates the file's new path.
- **Invalidation cannot be skipped.** It runs when the call ends, even if the call failed or was dropped by a layer above, because the server may have acted anyway. An answer that was on its way while a change to its path finished is returned but not stored.
- **Age.** A TTL counts from when the question was asked, not from when the answer arrived.
- **Size.** Each of the three caches holds at most 10,000 entries, least recently used going first. A listing counts as one plus one per directory entry, so a directory of 10,000 or more entries is not cached.
- **Other clients.** Changes made by others, and local changes that reach a path through a symbolic link, show up when the TTL runs out.
- **Kernel-side caches.** WinFsp's own caches are configured **short and never infinite**: `FileInfoTimeout` = 1000 ms. This fixes NEO's NUL-byte bug class. Final values are confirmed in spike M0-1.

**Reads.**
- Each open handle tracks sequential access. On sequential reads, read-ahead grows from 128 KiB to 4 MiB.
- Requests are split into chunks of the server's maximum read size (from `limits@openssh.com`, else 32 KiB; never above 4 MiB) and sent in parallel, with up to 64 chunks and at most 4 MiB in flight per handle. More does not raise the rate and delays metadata on the shared channel (spike M0-2).
- **What counts as sequential.** A read that starts where the last one ended. The first read on a handle and any other jump fetch only what was asked for, so reading a file's header costs no extra traffic. So does the read after an end of file or an error.
- **Growth.** The window doubles each time the caller has consumed a window's worth, not on every read, so what is fetched stays in proportion to what is used. Beyond the range a read needs, requests wait until a chunk's worth or half a window is due, so small reads do not become small requests.
- **Freshness.** Data and errors fetched ahead are kept for at most 5 s from when they were asked for, the default `cache_ttl_secs`. A reader that takes longer goes back to a 128 KiB window, since its window was larger than it could use. A change through the drive drops what the reader holds. A speculative request that fails matters only if a read reaches it.

**Writes (v1).**
- Each WinFsp `Write` call is split into chunks of the maximum write size, sent in parallel, and **acknowledged before the call returns**. There is no write-back cache.
- `Flush` and `Cleanup` call `fsync@openssh.com` when the server supports it.
- Spike M0-2 showed this misses NFR-PERF-06 on WAN (49% of OpenSSH with a 50 ms delay), so a **bounded write-behind is added in M2-08** (OD-4): at most 4 MiB unacknowledged per handle, the same limit as reads; the first error is latched and returned on the next Write/Flush/Cleanup. The spike measured a free-running pipeline with that bound (102% of OpenSSH); the design itself is still to be built and tested.

**Operation deadlines.** Every remote call runs under `op_timeout_secs`. On timeout:
- the call returns `STATUS_IO_TIMEOUT`;
- the connector runs a keep-alive probe;
- if the probe also fails, the mount becomes *Reconnecting*.

A timeout reported by the backend itself counts the same as an expired deadline. The deadline layer sits below the caches (`CachedFs<DeadlineFs<…>>`), so a call that timed out still invalidates them.

**Name mapping.**
- Characters invalid in Windows names (`\ : * ? " < > |` and 0x01–0x1F) and trailing dots or spaces are shown as the Unicode private-use character U+F000 + code. This is the same scheme as Cygwin and SSHFS-Win, so names look the same as in NEO.
- All the dots and spaces at the end of a name are mapped, not just the last: `a..` is shown as `a` + U+F02E + U+F02E.
- The mapping is reversed when sending names to the server. A Windows name that would reach the server as `.` or `..`, or that contains `/` or NUL, is refused, so it cannot address a different directory.
- A remote name that really contains one of these private-use characters is shown as it is, so `a` + U+F03A and `a:` look the same on Windows; the scheme cannot escape them. The resolver below still finds such a name. If both exist, the Windows name means `a:`.
- Name matching is **case-preserving**: an exact-case lookup first, then a case-insensitive match against the cached listing, used only when exactly one entry matches.
- Names that differ only by case are all listed.

**Permission and attribute mapping.**

| Remote | Windows |
|---|---|
| Directory | `FILE_ATTRIBUTE_DIRECTORY` |
| Regular file whose mode has no owner-write bit (`0o200`) | `FILE_ATTRIBUTE_READONLY` |
| Setting or clearing the Windows read-only attribute | clears or sets `0o200` on the remote |
| Symlink | Resolved by the server (`stat`) and shown as its target. A broken link is listed, and opening it returns `STATUS_OBJECT_NAME_NOT_FOUND`. Creating symlinks or hard links returns `STATUS_NOT_SUPPORTED`. |
| Dot-files | Shown as normal files (not hidden) |
| mtime / atime | LastWriteTime / LastAccessTime. SFTP v3 has 1 s resolution. |
| (no birth time in SFTP v3) | CreationTime = ChangeTime = mtime |
| Owner/ACLs | Not mapped. Every file has one fixed security descriptor: full control for the current user. Access is decided by the server. |
| New files and directories | Created without explicit permissions, so the server's umask applies |

### 5.7 SFTP backend (fsm-sftp)

- **Transport:** `russh` client over TCP with the `ring` crypto backend (its default, `aws-lc-rs`, needs NASM to build on Windows). The default algorithm preferences are curve25519 key exchange, Ed25519 host keys, and AES-256-GCM then chacha20-poly1305 ciphers (AES-GCM was 20–35% faster in spike M0-2). Compression is off.
- **Setting up a connection:** each stage has `op_timeout_secs` to finish: name lookup and TCP, the SSH handshake, the login, and the SFTP setup. Time in the trust dialog (§7.4.2) does not count, and the handshake's limit starts again after it. Dropping the connect call at any point, including during the dialog, abandons the connection (FR-MNT-11).
- **How failures map to §7.8:**
  - A failed name lookup, including a slow one, is E-DNS.
  - No TCP connection in time, or a refused one, is E-HOST-UNREACHABLE: its hint to check the network fits a firewall that drops packets. E-TIMEOUT is for a server that accepted TCP and then went silent.
  - A connection that closes during setup is E-CONN-LOST. That includes an HTTP server answering with an error page and closing, which cannot be told apart from it.
  - A peer that keeps sending something other than an SSH banner is E-SFTP-UNAVAILABLE, like a subsystem that is refused, exits at once or speaks something else.
  - No algorithm in common is E-ALGORITHMS, with both lists in the log.
  - A `known_hosts` file that cannot be read, or a key that cannot be saved after *Trust and save*, refuses the connection with E-INTERNAL.
  - A remote path the server will not let this user see is E-PATH-NOT-FOUND: the server does not say whether it exists.
- **Remote path:** `~`, `~/below/home` or an absolute path. Anything else is E-PATH-NOT-FOUND. Windows-style paths come with M1-10.
- **Host certificates** are not requested, so a server cannot present one. If one arrived anyway, it would be refused.
- **Logging in:**
  - The client first asks the server which methods it takes, and only uses those, so a method it does not take costs neither a dialog nor a failed attempt.
  - The profile's method is tried once. A refusal is E-AUTH, without falling back to another method: a refused key does not lead to a password prompt, and a refused password is not sent again through keyboard-interactive, so one wrong password counts as one failure on the server.
  - A *Password* or *Ask each time* profile uses keyboard-interactive when the server does not take the password method. After a partial success, any profile continues with the password method or keyboard-interactive (a key plus TOTP, for example).
  - In keyboard-interactive, the first prompt matching `password` (any case) in a login is answered from the known password. A *Password* profile with none yet asks through the password dialog, so *Remember* is offered (§7.4.1). A later matching prompt, and every other prompt, goes to the user in the keyboard-interactive dialog, and those answers are never kept.
  - A password or decrypted key stays in memory for the rest of the mount, so a reconnect asks nothing. A refused login clears it. A password goes to Credential Manager only after the server has accepted it; a passphrase, once it has opened the key.
  - A key file that has gone or cannot be read is E-AUTH (FR-REC-06), and so is a wrong passphrase. Asking again after a wrong passphrase is left to mounting again, like a wrong password.
  - A file that is not a supported private key is E-KEY-FORMAT: `.ppk` by its content, `.pub`, RSA under 2048 bits, DSA, and hardware keys. So are two kinds `russh` cannot read: an encrypted legacy `EC PRIVATE KEY` PEM file, and encrypted PKCS#8 using PBKDF2 with HMAC-SHA1, which `ssh-keygen -m PKCS8` on Windows writes.
  - RSA signatures use the best of rsa-sha2-512 and rsa-sha2-256 that the server names, and rsa-sha2-512 if it names neither. SHA-1 is never used, for keys or for certificates.
  - `<key>-cert.pub` is read at each login, and offered before the plain key if it belongs to that key. One that cannot be read or belongs to another key is logged and skipped.
  - The agent method tries each key of the Windows OpenSSH agent, then of Pageant. An agent that is not running or has no accepted key leaves E-AUTH.
- **Jump host:** connect to the jump host, open a `direct-tcpip` channel to the target, and run the target SSH session over that channel. Each host is verified separately.
- **One SSH connection per mount.** Metadata and data share one SFTP channel with pipelined requests. Spike M0-2 measured head-of-line blocking that grows with bytes in flight; the 4 MiB limit (§5.6) keeps it to about one extra round trip. A second SFTP channel for bulk transfers removes it and stays an option if M2-08 finds browsing during a copy too slow.
- **Window sizes:** the SSH channel window is 16 MiB (spike M0-2: 2 MiB halves WAN downloads, and 64 MiB gains nothing).
- **Keep-alive:** a `keepalive@openssh.com` global request every `keepalive_secs`. The connection is lost after 3 misses. `russh` decides that at the next interval, so a dead link is noticed 4 intervals after the last reply: at the default 15 s, that is on FR-SSH-12's 60 s limit (M1-10 decides whether to lower it).
- **Free space:** `statvfs@openssh.com`. Without it, total and free are reported as 1 PiB, so Windows' copy pre-checks never fail.
- **Rename:** `posix-rename@openssh.com` for rename-with-replace. Without it, the target is removed and then the source renamed; this non-atomic fallback is logged at Debug level.
- **Windows OpenSSH servers:** detected from the server version string or a `/C:/` style `realpath`. Paths are normalised (FR-SSH-13).

### 5.8 WinFsp adapter (fsm-winfsp)

**Volume parameters**

| Parameter | Value |
|---|---|
| Sector size / sectors per allocation unit | 4096 / 1 |
| `MaxComponentLength` | 255 |
| `CaseSensitiveSearch` / `CasePreservedNames` / `UnicodeOnDisk` | false / true / true |
| `PersistentAcls` | false; fixed security descriptor `O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)` |
| `ReadOnlyVolume` | `read_only` |
| `VolumePrefix` | `\fsm\<profile-slug>` (network-drive mode) |
| `FileSystemName` | `FSM-SFTP` / `FSM-NFS` |
| Volume label | profile name, ≤ 32 chars |
| `FileInfoTimeout` | 1000 ms (see §5.6) |
| `PostCleanupWhenModifiedOnly` | true |
| `VolumeSerialNumber` / `VolumeCreationTime` | fixed per profile / mount time |
| Other flags | crate defaults: one file context per open handle, double buffering on; no hard links, reparse points, named streams or EAs |
| Dispatcher | `FineGuard`; thread count from settings (0 = WinFsp default) |

Confirmed by spike M0-1 (`docs/spikes/m0-1-winfsp.md`): unelevated network-mode mounts pass all scripted and manual checks. Build notes:
- the `winfsp` crate is used with its `system` feature, so the installed WinFsp DLL is found through the registry;
- builds need LLVM (libclang for `bindgen`) and the WinFsp Developer feature;
- `windows` must stay on the version `winfsp` uses (0.61 for `winfsp` 0.13).

**Operation mapping**

| WinFsp callback | RemoteFs call(s) |
|---|---|
| `GetVolumeInfo` | `statfs` (cached 30 s) |
| `GetSecurityByName` | `stat` (cached), fixed security descriptor |
| `Create` | `open(create, exclusive)` or `mkdir` |
| `Open` | `stat`; the remote handle is opened on first read or write |
| `Overwrite` | `open(truncate)` or `set_attr(size = 0)` |
| `Read` / `Write` | `read` / `write` (§5.6) |
| `Flush` | `flush` (fsync) |
| `GetFileInfo` | cached `stat` / `fstat` |
| `SetBasicInfo` | `set_attr(times, read-only bit)` |
| `SetFileSize` | `set_attr(size)` |
| `CanDelete` | check that a directory is empty |
| `Cleanup` (delete flag) | `remove_file` / `remove_dir`; flush pending writes |
| `Close` | `close` |
| `Rename` | `rename(replace = flag)` |
| `ReadDirectory` | cached `read_dir`, paged by marker |
| `GetDirInfoByName` | lookup in the cached listing |

**Error mapping**

| Remote error | NTSTATUS |
|---|---|
| No such file | `STATUS_OBJECT_NAME_NOT_FOUND` (or `STATUS_OBJECT_PATH_NOT_FOUND` for a missing parent) |
| Permission denied | `STATUS_ACCESS_DENIED` |
| Already exists (exclusive create, mkdir) | `STATUS_OBJECT_NAME_COLLISION` |
| Directory not empty | `STATUS_DIRECTORY_NOT_EMPTY` |
| Operation unsupported | `STATUS_NOT_SUPPORTED` |
| Read-only volume | `STATUS_MEDIA_WRITE_PROTECTED` |
| Deadline exceeded | `STATUS_IO_TIMEOUT` |
| Connection lost / reconnecting past deadline | `STATUS_NETWORK_UNREACHABLE` |
| Disk full (`SSH_FX_FAILURE` after statvfs shows 0 free, or NFS `NOSPC`) | `STATUS_DISK_FULL` |
| Anything else | `STATUS_UNEXPECTED_NETWORK_ERROR`, logged with details |

### 5.9 SMB backend (fsm-smb), M3

- **Mount:** `WNetAddConnection2W` with:
  - `NETRESOURCEW { dwType: RESOURCETYPE_DISK, lpLocalName: "Z:", lpRemoteName: "\\server\share" }`;
  - user and password from the SecretStore;
  - flags: `CONNECT_TEMPORARY`.

  With *Current Windows user*, user and password are null.
- A sub-path is handled by mapping the share and opening Explorer at the sub-folder. Windows cannot map a drive letter to a sub-folder of a share; the UI states this next to the field.
- **Unmount:** `WNetCancelConnection2W(L"Z:", 0, force)`.
- **Health:**
  - re-checked on Windows network events and before *Open in Explorer*;
  - a failing `GetDiskFreeSpaceExW` sets the row to *Disconnected (Windows will reconnect)*;
  - there is no app-side reconnect loop, because the Windows SMB client handles it.
- **Error mapping** for WNet calls: `ERROR_SESSION_CREDENTIAL_CONFLICT` (1219) → E-SMB-1219; `ERROR_LOGON_FAILURE` → E-AUTH; `ERROR_BAD_NET_NAME` → E-SMB-SHARE-NOT-FOUND; `ERROR_BAD_NETPATH` → E-HOST-UNREACHABLE; `ERROR_ALREADY_ASSIGNED` → E-LETTER-TAKEN.

### 5.10 NFS backend (fsm-nfs), M4

- `nfs3_client` over TCP; MOUNT v3 gets the root handle. Ports come from the portmapper (111) unless they are fixed in the profile.
- AUTH_SYS credentials use the profile's UID/GID. The source port is bound from 1023 downwards (FR-NFS-03).
- The connection is stateless: file handles survive reconnects (FR-NFS-04).
- Mapping:
  - Unix mode bits map as in §5.6;
  - NFS `READDIRPLUS` fills the attribute cache together with the listing;
  - `FSSTAT` gives free space;
  - `COMMIT` is issued on Flush and Cleanup after `UNSTABLE` writes.
- NFSv4.1 (`nfs-rs`) is a later option. It adds single-port operation (2049).

---

## 6. Data

### 6.1 Configuration file

`%APPDATA%\FsManager\config.toml` is UTF-8 and contains no secrets. Writes are atomic (NFR-REL-05).

```toml
schema_version = 1

[settings]
start_with_windows = false
close_to_tray      = true
theme              = "system"     # system | light | dark
letter_conflict    = "auto"       # auto | fail
notifications      = true
log_level          = "info"       # error | warn | info | debug
op_timeout_secs    = 15           # not in UI, 5..=120
winfsp_threads     = 0            # not in UI, 0 = number of cores (max 8)

[[profiles]]
id             = "6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10"
name           = "Production"
group          = "Web"            # optional
protocol       = "sftp"           # sftp | smb (M3) | nfs (M4)
host           = "10.0.4.21"
port           = 22
username       = "deploy"
auth           = "key"            # password | key | agent | ask
key_path       = 'C:\Users\me\.ssh\id_ed25519'
remember       = true             # secret (password or passphrase) stored in Credential Manager
remote_path    = "/var/www"       # "~" = home directory
drive_letter   = "Z"              # "auto" or D..Z
read_only      = false
mount_on_start = true
auto_reconnect = true
cache_ttl_secs = 5
keepalive_secs = 15

[profiles.jump]                   # optional
host     = "bastion.example.com"
port     = 22
username = "jump"
auth     = "agent"                # agent | key | password
key_path = ""

# M3 example
# [[profiles]]
# protocol = "smb"
# server = "nas"           share = "media"      sub_path = "films"
# domain = ""              username = "marcin"  auth = "password"   # password | ask | windows

# M4 example
# [[profiles]]
# protocol = "nfs"
# server = "nas"           export = "/volume1/data"   uid = 1000  gid = 1000
# nfs_port = 0             mount_port = 0             reserved_port = true
```

- **Migrations:** when `schema_version` is older, the file is migrated in code with one function per version step. The old file is kept as `config.v<N>.bak`. A newer `schema_version` than the app understands → the app starts read-only and says "This configuration was written by a newer FS Manager".
- **Unknown keys** are kept on save, for forward compatibility.

### 6.2 Secrets

Windows Credential Manager entries (NFR-SEC-01) are created only when "Remember" is ticked. They are deleted on profile delete, and when "Remember" is unticked.

### 6.3 Known hosts

- **File:** reads and appends to `%USERPROFILE%\.ssh\known_hosts`; the directory is created if missing. New lines use the OpenSSH format, hashed (`|1|…`) only if the existing file already uses hashed entries.
- **Concurrency:** the file is never rewritten, only appended. A lock file prevents concurrent appends: `known_hosts.lock` beside the file, held with an operating-system lock, so a crash cannot leave it stuck. The empty file stays behind. The file is read again under the lock, and the line is written only if the key is still unknown; if another program saved a different key while the trust dialog was open, the connection is refused.
- **Matching:** the same as OpenSSH, so both programs give the same answer for one file (checked against OpenSSH 9.5 on Windows; see `known_hosts_openssh.rs`):
  - Names are compared in lower case. `*`, `?`, `!` and comma-separated lists work in plain entries. A server on another port is looked up as `[host]:port`.
  - The key is **known** if any entry for the name holds it. If there is no entry at all under `[host]:port`, an entry for plain `host` with exactly this key also counts.
  - The key has **changed** if the name has other keys on file, **whatever their type**. The line shown is the last of them, as `ssh` reports it. A connection must therefore ask the server for the key types on file first, as `ssh` does; otherwise a server that also has a newer key type looks changed.
  - The key is **unknown** otherwise. A different key under plain `host` does not make a server on another port *changed*.
  - `@cert-authority` lines are ignored, because host certificates are not supported. Lines OpenSSH would skip are skipped, and a UTF-8 byte order mark at the start of the file is ignored, as in the Windows build.
- **Revoked keys:** a `@revoked` line for `[host]:port` or plain `host` with this key always refuses the connection (E-HOSTKEY-REVOKED). This is the one difference from OpenSSH, which accepts a key found under `[host]:port` without looking at plain `host`.
- **Only this file is read.** `known_hosts2` and the system-wide `%PROGRAMDATA%\ssh\ssh_known_hosts` are not, so a `@revoked` line there has no effect here.

### 6.4 Logs

- **Location:** `%LOCALAPPDATA%\FsManager\logs\fsm.log`, rolled daily, keeping 7 files of at most 5 MB each.
- **Format and secrets:** text with timestamp, level, span (profile) and message. Secrets cannot appear (NFR-SEC-02).
- **Crash reports:** `crash-<timestamp>.txt` in the same folder.

---

## 7. User interface

Built with **Slint** (Fluent style, software renderer). Theme: System / Light / Dark.

### 7.1 Main window (default 640 × 420, minimum 480 × 320; size and position remembered)

```
┌──────────────────────────────────────────────────────────────────┐
│  FS Manager                                           ─   □   ×  │
├──────────────────────────────────────────────────────────────────┤
│  🔍 Search…                    [ + Add ]  [ Mount all ]   ⚙      │
├──────────────────────────────────────────────────────────────────┤
│  ⚠ WinFsp 2.1 or later is required…        [ Download WinFsp ]   │  ← only if missing
├──────────────────────────────────────────────────────────────────┤
│  ▾ Web                                                           │
│   ●  Production         deploy@10.0.4.21:/var/www   Z:  [Unmount] ⋯ │
│   ◐  Staging            deploy@stage:/srv           Y:  [ Cancel] ⋯ │
│  ▾ Home                                                          │
│   ○  NAS backup         marcin@nas:~                X:  [ Mount ] ⋯ │
│   ●  Dev box            me@devbox:/home/me          W:  [Unmount] ⋯ │
│      ⚠ Authentication failed. Check user name and key.            │  ← error line
├──────────────────────────────────────────────────────────────────┤
│  2 mounted · 1 connecting                                        │
└──────────────────────────────────────────────────────────────────┘
```

**Row status and primary button**

| Status | Dot | Button |
|---|---|---|
| Disconnected | ○ grey | Mount |
| Connecting | ◐ amber, animated | Cancel |
| Mounted | ● green | Unmount |
| Reconnecting | ◐ amber + "Reconnecting…" line | Unmount |
| Error | ● red + one-line message | Retry |

**Rows and menus**
- **Drive letter:** the row shows it when mounted, and the preferred letter (dimmed) otherwise.
- **⋯ menu:** *Open in Explorer* (when mounted), *Edit…*, *Delete…*.
- **Mouse:** double-clicking a row opens Explorer if mounted, otherwise mounts. The right-click menu is the same as ⋯.

**Search and list**
- Search filters by name, host, user, group and drive letter.
- Groups are sorted alphabetically, with ungrouped rows last; rows are sorted by name.
- Group headers collapse, and the collapsed state is remembered.
- **Empty state:** "No connections yet." with an **Add connection** button.

### 7.2 Profile dialog (Add / Edit)

A modal dialog, about 460 × 520.

- **v1:** shows SSH fields only.
- **From M3:** a segmented control **SSH | SMB | NFS** appears at the top, and the fields below change with it.

```
┌ Add connection ───────────────────────────────────┐
│ Name        [ Production                       ]  │
│ Group       [ Web                              ]  │
│ Host        [ 10.0.4.21               ] Port [22] │
│ User        [ deploy                           ]  │
│ Auth        (•) Key file  ( ) Password            │
│             ( ) SSH agent ( ) Ask each time       │
│ Key file    [ C:\Users\me\.ssh\id_ed25519  ][…]  │
│             [✓] Remember passphrase               │
│ Remote path [ /var/www                         ]  │
│ Drive       [ Z: ▾ ]   (Auto, D:…Z:, free ones marked) │
│ [✓] Mount when FS Manager starts                  │
│ ▸ Advanced                                        │
│   Jump host  [ user@host:port ] Auth [Agent ▾]    │
│   [✓] Reconnect automatically                     │
│   [ ] Read-only                                   │
│   Cache (s) [5]   Keep-alive (s) [15]             │
│                                                   │
│ [ Test connection ]   ✓ Connected (12 ms)         │
│                              [ Cancel ] [ Save ]  │
└───────────────────────────────────────────────────┘
```

### 7.3 Field validation

| Field | Rule | Message |
|---|---|---|
| Name | 1–64 chars; unique (case-insensitive); no `\ / : * ? " < > \|` | "Name is required" / "A connection with this name already exists" |
| Group | 0–32 chars | |
| Host | DNS name, IPv4 or IPv6 literal; no spaces; no scheme, `user@` or `:port` (paste is split, FR-PROF-05) | "Enter a host name or IP address" |
| Port | 1–65535 | "Port must be 1–65535" |
| User | 1–64 chars, no spaces | "User name is required" |
| Key file | Exists, is readable and is OpenSSH/PEM format; `.ppk` rejected (FR-SSH-03); `.pub` rejected | "This is a public key; select the private key file" |
| Remote path | `~`, `~/…`, or an absolute path starting with `/`, or a Windows `C:/…` form | "Use an absolute path such as /var/www, or ~ for home" |
| Drive | Auto or D–Z; a letter in use by another *profile* gives a warning only (allowed, because only one can be mounted) | "Also used by 'NAS backup'" |
| Cache (s) | 0–60 | |
| Keep-alive (s) | 0 or 5–300 | |
| Jump host | Empty or `[user@]host[:port]` | "Use user@host:port" |

### 7.4 Prompts

#### 7.4.1 Secret prompt

- **Title:** "Password for deploy@10.0.4.21", or "Passphrase for id_ed25519", or the keyboard-interactive server text.
- **Controls:** a masked field with a show toggle and a Caps Lock hint; a *Remember* checkbox (not shown for keyboard-interactive prompts other than the password); *OK* / *Cancel*.
- **Window behaviour:** brings the main window to the front if it is hidden.

An *Ask each time* profile first shows a prompt offering a password, or a key file plus passphrase (FR-SSH-05). It has no *Remember*, and neither has a passphrase prompt that follows it.

#### 7.4.2 Host key trust prompt

```
┌ Unknown server ──────────────────────────────────────────┐
│ FS Manager has not connected to 10.0.4.21:22 before.     │
│                                                          │
│ Key type:    ssh-ed25519                                 │
│ Fingerprint: SHA256:Xq3Vb…4Q8                            │
│                                                          │
│ Check that this fingerprint matches the server before    │
│ trusting it.                                             │
│              [ Cancel ]  [ Trust once ]  [ Trust and save ]│
└──────────────────────────────────────────────────────────┘
```

#### 7.4.3 Changed host key

An error dialog (no trust button) titled **"Server key has changed: connection refused"**. It shows:
- the host;
- the old and new fingerprints;
- `known_hosts` line N;
- the text: "This can mean someone is intercepting the connection, or the server was reinstalled. If you are sure the change is expected, remove line N from the file and connect again."

Buttons: *Open file location*, *Close*.

### 7.5 Settings page

This page replaces the list area inside the main window; a ← back arrow returns to the list. It holds the items in §3.8, one per row, with an **About** footer showing:
- the version;
- "Licensed under the GNU GPL v3" and a link to the source repository (NFR-LIC-03);
- "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" with a link to https://github.com/winfsp/winfsp (NFR-LIC-02);
- a *Third-party licences* button;
- a link to the logs folder. Changes apply immediately. Only "Start with Windows" touches the registry.

### 7.6 Tray menu

```
 ✓ Production (Z:)
   Staging (Y:)        Connecting…
 ✓ Dev box (W:)        Reconnecting…
   NAS backup (X:)
 ─────────────────────
   Unmount all
   Open FS Manager
 ─────────────────────
   Quit
```

### 7.7 Keyboard

| Key | Action |
|---|---|
| Ctrl+N | Add connection |
| Ctrl+E | Edit selected |
| Enter | Mount/Unmount selected |
| Ctrl+Enter | Open selected in Explorer |
| Del | Delete selected (asks first) |
| Ctrl+F | Focus search |
| Esc | Close dialog / clear search / hide window to tray |
| ↑ ↓ | Move selection |

### 7.8 Error catalogue

| Code | Shown text (short line in the row; dialog/toast may add detail) | Hint / action |
|---|---|---|
| E-AUTH | "Authentication failed for {user}@{host}." | "Check the user name and password or key." Clears a remembered password only when the user re-enters one. |
| E-HOSTKEY-CHANGED | "Server key has changed: connection refused." | Dialog §7.4.3 |
| E-HOSTKEY-REVOKED | "Server key has been revoked: connection refused." | "`known_hosts` line {line} marks this key as revoked. Ask the server's administrator for its current key." |
| E-HOSTKEY-REJECTED | "Server key not trusted." | Mount again to see the prompt |
| E-DNS | "Host {host} not found." | "Check the host name." |
| E-HOST-UNREACHABLE | "Cannot reach {host}:{port}." | "Check the network, VPN or firewall." |
| E-TIMEOUT | "{host} did not respond in time." | |
| E-PATH-NOT-FOUND | "Remote path {path} not found." | "Edit the connection and check the remote path." |
| E-PATH-NOT-DIR | "Remote path {path} is not a folder." | |
| E-SFTP-UNAVAILABLE | "The server does not provide SFTP." | "Ask the administrator to enable the SFTP subsystem." |
| E-ALGORITHMS | "No encryption method in common with the server." | "The server may offer only outdated algorithms. Ask its administrator to update its SSH settings." |
| E-LETTER-TAKEN | "Drive {L}: is already in use." | "Choose another letter or set Drive to Auto." |
| E-NO-FREE-LETTER | "No free drive letter." | |
| E-WINFSP-MISSING | "WinFsp 2.1 or later is required." | Download button |
| E-KEY-FORMAT | "Unsupported key file format." | PuTTY conversion hint |
| E-CONN-LOST | "Connection lost." | Shown when auto-reconnect is off |
| E-SMB-1219 (M3) | "Windows is already connected to {server} as a different user." | "Disconnect the other connection, or use the server's IP address in this connection." |
| E-SMB-SHARE-NOT-FOUND (M3) | "Share {share} not found on {server}." | |
| E-NFS-ACCESS (M4) | "The NFS server refused access to {export}." | "Check that this PC's IP address is allowed in the export list, and the UID/GID." |
| E-INTERNAL | "Unexpected error." | "See the log for details." with an Open logs button |

---

## 8. Security design

- **Threat model.**
  - *In scope:* network attackers (MITM, eavesdropping); other local users; secrets leaking through logs, config files, the process list or crash dumps.
  - *Out of scope:* malware running as the same Windows user, which can read that user's Credential Manager anyway.
- **Controls.**
  - Host-key verification (FR-SSH-08 to FR-SSH-10).
  - Credential Manager storage and the `Secret<T>` type (NFR-SEC-01 to NFR-SEC-03).
  - Unelevated process and per-session mounts (NFR-SEC-05).
  - No external processes are started with credentials. SMB uses the API (FR-SMB-03).
  - Crash reports contain a backtrace and the app version only; no memory dump and no config.
- **SSH algorithms.** russh's secure defaults; `ssh-rsa` (SHA-1) signatures are disabled, for host keys here and for login keys in M1-08b. An RSA key on file is still used, through `rsa-sha2-512` or `rsa-sha2-256`. A server that only offers legacy algorithms fails with a clear message (E-ALGORITHMS). The `rsa` crate's timing advisory is accepted for a client: [ADR-0004](adr/0004-rustsec-2023-0071.md).
- **NFS caveat.** AUTH_SYS trusts the UID/GID the client sends, so NFSv3 security is enforced by the server's export list (client IP). The NFS profile dialog says this in one line.

---

## 9. Packaging, installation and licensing

- **Build:** `cargo build --release` with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` (after the crash hook runs) and `strip = true`. The C runtime is statically linked (`+crt-static`), so there is no VC++ redistributable dependency.
- **Installer:** MSI built with WiX (`cargo-wix`).
  - Per-machine install to `C:\Program Files\FS Manager\`; requires admin, which is also needed for WinFsp.
  - Checks the WinFsp version in the registry (`HKLM\SOFTWARE\WOW6432Node\WinFsp`, `InstallDir` + DLL version). If WinFsp is missing or older than 2.1, it offers the official, **unmodified** WinFsp MSI as a chained install (allowed by WinFsp's FLOSS exception, ADR-0003) or a link (OD-5).
  - Start-menu shortcut; optional *Start with Windows* checkbox (writes the HKCU Run key for the installing user).
  - Uninstall removes the program. It keeps `%APPDATA%\FsManager` and the credentials unless "Remove my settings" is ticked.
- **Toast registration:** at start, the app writes `HKCU\Software\Classes\AppUserModelId\<id>` with a `DisplayName`. Spike M0-3 showed this is what makes toasts appear; a shortcut with an AUMID alone did not.
- **Portable:** a zip with `fsm.exe`. It needs WinFsp installed separately.
- **Signing:** Authenticode for `fsm.exe` and the MSI (OD-3).
- **Updates:** none in v1. Users install a newer MSI over the old one. Settings and profiles are kept and migrated (§6.1).
- **Licence:** FS Manager is GPL-3.0-or-later ([ADR-0003](adr/0003-licence-gplv3.md)). The MSI and zip include `LICENSE` and `THIRD-PARTY-LICENSES.html`. `cargo deny` enforces the allow-list from ADR-0003 (NFR-LIC).

---

## 10. Testing strategy

### 10.1 Unit tests (all crates, run on every push)

- Profile validation (every rule in §7.3) and paste parsing (FR-PROF-05).
- Config serialisation, round-trips, unknown-key preservation, migrations and corrupt-file handling.
- Mount state machine: every transition in §5.5, using a mock `MountBackend`.
- Letter allocator: free and taken letters, remembered network connections, A/B/C excluded, auto from Z downwards.
- VFS caches: TTL expiry, invalidation on each mutation, negative cache and noise names.
- Name mapping round-trips (all invalid characters, trailing dot or space); case-insensitive fallback with ambiguous names.
- Error mapping tables (§5.8).
- Known-hosts parser: plain, hashed, `[host]:port`, `@revoked`, appending.

### 10.2 Backend integration tests (Linux CI, Docker)

`tests/docker-compose.yml` starts:
- an OpenSSH server built from `tests/docker/sshd` (Debian, own image instead of `linuxserver/openssh-server`, which is built around one user), configured with password, key, certificate and keyboard-interactive (password + TOTP) users, and a second instance acting as a jump host. Further instances each differ in one way, to produce one failure: no SFTP subsystem, one that exits at once, one that never answers, only SHA-1 host key signatures, and two host keys;
- an SFTP server without OpenSSH extensions (ProFTPD `mod_sftp`);
- *(M4)* an NFSv3 server (`erichough/nfs-server` or Ganesha).

The `RemoteFs` contract-test suite (one generic suite run against each backend) covers:
- create, read and write at offsets, truncate, rename with and without replace, mkdir, rmdir on a non-empty directory, remove, set times and the read-only bit, statfs;
- large files (1 GiB);
- pipelined throughput;
- connection kill mid-read with reconnect and handle reopen;
- deadline expiry.

The generic part is `fsm_vfs::contract` (feature `testing`): everything down to large files. It needs a server that allows any POSIX name and a user who is not root, and it cannot cover symbolic links because `RemoteFs` cannot create them. The last three items depend on the backend or on other layers and are tested with them.

### 10.3 Windows integration tests (Windows CI runner with WinFsp installed)

- Mount an **in-memory `RemoteFs`** and an **in-process russh SFTP server** (from the `russh-sftp` server side) as drives, then run:
  - `winfsp-tests-x64.exe` with the test subset that applies to a network file system without ACLs and streams. The excluded tests are listed in the repo with reasons;
  - `fsx` (file system exerciser), 100k operations, for data integrity (NFR-REL-01);
  - scripted Explorer-like operations through Win32 `CopyFileEx`, `MoveFileEx`, `FindFirstFile` and `ReplaceFile`;
  - NEO regression cases: write → close → reopen → hash; a new file visible in the next listing; a host-key prompt for an unknown server; no drive left after the process is killed.
- Crash test: kill `fsm.exe` with a mount active, then check with `GetLogicalDrives` that the letter is gone (NFR-REL-04).

### 10.4 Manual and lab test matrix (before each release)

| Client OS | Servers |
|---|---|
| Windows 10 Home 22H2 | Ubuntu 24.04 OpenSSH, Windows Server 2022 OpenSSH, OpenSSH 7.4 (CentOS 7), ProFTPD mod_sftp |
| Windows 11 Pro 24H2 | same, plus *(M3)* Windows Server SMB, Samba 4.x, Synology DSM; *(M4)* Linux knfsd, Synology NFS, TrueNAS NFS |

Scenarios:
- sleep/resume;
- Wi-Fi switch;
- VPN up/down;
- server reboot;
- password change on the server;
- host key change;
- drive letter taken by a USB stick;
- sign-out with mounts;
- 72 h soak (NFR-REL-06).

### 10.5 Performance tests

- **Comparison tooling:** `fsbench-x64.exe` (WinFsp) and `robocopy` / `fio` on Windows against the §4.1 environment, compared with OpenSSH `sftp`/`scp` on the same link.
- **Micro-benchmarks:** criterion benchmarks for the VFS caches and name mapping.
- **Recording:** start time, working set and exe size are recorded on every release build, and a regression of more than 10% fails the build.

---

## 11. Milestones

All implementation work is done by an AI coding agent. Tasks, human checkpoints and rough timing are in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md).

| Milestone | Content | Exit criteria |
|---|---|---|
| **M0 Spikes** | **M0-1:** `winfsp` crate mounts an in-memory FS as an unelevated network drive; Explorer copy/rename/delete/drag-drop work; volume flags settled. **M0-2:** `russh` + `russh-sftp` pipelined read/write throughput vs OpenSSH `sftp` on LAN and 50 ms WAN; decide write-behind (§5.6). **M0-3:** Slint + `tray-icon` on one event loop; close-to-tray; hidden message window receives power and session messages. | Each spike has a short report in `docs/spikes/` with numbers and a go/no-go. A no-go triggers the fallback in §12. |
| **M1 Core mount** | Workspace, fsm-core, fsm-vfs, fsm-sftp, fsm-winfsp, main window, profile dialog, prompts, Credential Manager, known hosts | Mount an SFTP profile from the UI; NFR-REL-01/02 pass; FR-PROF, FR-SSH and FR-MNT acceptance criteria pass |
| **M2 v1 release** | Reconnect (FR-REC), tray, notifications, settings, lifecycle (FR-APP), MSI, signing, user guide | All v1 requirements' acceptance criteria pass; NFR-PERF budgets met; 72 h soak passes |
| **M3 SMB** | fsm-smb, SMB form fields, E-SMB errors | FR-SMB acceptance criteria pass on Windows 10 Home and 11 Pro against Samba and Windows Server |
| **M4 NFS** | fsm-nfs (NFSv3), NFS form fields | FR-NFS acceptance criteria pass against knfsd, Synology and TrueNAS on Windows 10 Home |

**Later (only on request):**
- NFSv4.1;
- pure-Rust SMB fallback via WinFsp, for error 1219;
- import from `~/.ssh/config`;
- profile export/import;
- translations;
- update check;
- ARM64 build.

---

## 12. Risks and open decisions

### 12.1 Risks

| Risk | Likelihood / impact | Mitigation / fallback |
|---|---|---|
| `russh-sftp` too slow or buggy | Medium / High | Spike M0-2. Fallback: our own minimal SFTP v3 client on `russh` channels (the protocol is small: about 20 packet types). |
| Network-drive mode or some volume flags misbehave from an unelevated process | Low / High | Spike M0-1. Fallback: a plain drive-letter mount (not network mode). |
| Explorer is slow on high-latency links (many metadata calls) | Medium / Medium | Attribute, directory and negative caches; noise names; read-ahead; a configurable cache TTL |
| Slint and tray integration issues | Low / Medium | Spike M0-3. Fallback: a raw Win32 `Shell_NotifyIcon` in `fsm-win`. |
| A dependency with a GPLv3-incompatible licence (e.g. `GPL-2.0-only`, proprietary) is added | Low / Medium | `cargo deny` allow-list in CI (NFR-LIC-05). Licence model itself is decided (ADR-0003). |
| NFS crates are young (no NFSv4, no Kerberos) | Medium / Medium | M4 is NFSv3 only, isolated behind `RemoteFs`. Fallback: our own ONC-RPC/NFSv3 client (a well-documented protocol). |
| SMB error 1219 confuses users | High / Low | A clear message (E-SMB-1219). Later: a pure-Rust SMB fallback. |

### 12.2 Open decisions

| ID | Decision | Needed by | Recommendation |
|---|---|---|---|
| OD-1 | Licence model | **Decided 2026-11-03** | **GPL-3.0-or-later open source, WinFsp + `winfsp` crate** ([ADR-0003](adr/0003-licence-gplv3.md)) |
| OD-2 | Final product name and icon | Before M2 | |
| OD-3 | Code-signing certificate (OV/EV or Azure Trusted Signing) | Before M2 | Azure Trusted Signing (low cost, SmartScreen reputation) |
| OD-4 | Write-behind on or off | End of M0 | ✅ **On**, 4 MiB bound (spike M0-2, §5.6) |
| OD-5 | Bundle WinFsp MSI in our installer, or link to download | Before M2 | Bundle the unmodified MSI: WinFsp's FLOSS exception allows it for FLOSS apps that show the attribution (ADR-0003) |

---

## Appendix A: Requirement traceability to analysis problems

| Problem (ANALYSIS.md §3) | Addressed by |
|---|---|
| 1 Slow start / unpacking | NFR-PERF-01..03, §9 build |
| 2 Ghost / stale drives | §5.3 in-process mounts, NFR-REL-04, FR-APP-05 |
| 3 Passphrase keys, jump host, keep-alive | FR-SSH-02, FR-SSH-04, FR-SSH-11, FR-SSH-12 |
| 4 Only SSH mountable | §5.4 `MountBackend`, FR-SMB, FR-NFS |
| 5 Data integrity / invisible files | §5.6 caching, NFR-REL-01/02, §10.3 |
| 6 Slow drop detection | FR-REC-01..03 |
| 7 UI sprawl | §1.2 exclusions, NFR-UX-02, §7 |
| 8 ACL lock-out | §6 (no custom ACLs, secrets in Credential Manager) |
| 9 Elevated drives invisible | PLAT-03, NFR-SEC-05 |
| 10 Permission oddities | §5.6 permission mapping |
