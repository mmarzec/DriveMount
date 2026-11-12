# DriveMount: Software Specification

| | |
|---|---|
| Document | Software specification |
| Status | Draft 2 (licence decided: GPLv3) |
| Date | 2026-11-03 |
| Product | DriveMount (working title), executable `drivemount.exe` |
| Companion | [ANALYSIS.md](ANALYSIS.md): reference program, problems, technology rationale |

**Requirement keywords.** "Must" = required for the milestone. "Should" = expected unless there is a documented reason not to. "May" = optional.

---

## 1. Introduction

### 1.1 Purpose

DriveMount is a Windows desktop application that mounts remote disks as **drive letters**:
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

1. The user installs DriveMount. The installer checks for WinFsp and offers to install it.
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
| FR-PROF-07 | M | Deleting a profile also deletes its Credential Manager entries. A mounted profile is unmounted first, after confirmation. | After delete, no `DriveMount:<uuid>:*` credentials remain. |
| FR-PROF-08 | M | While a profile is mounted, connection fields are read-only, with the hint "Unmount to change". These fields are protocol, host, port, user, authentication, remote path, drive letter, jump host and read-only. Name and the start/reconnect flags stay editable. The same goes for a mount in progress. Cache and keep-alive are locked too, as a drive takes them when it starts. | Editing the host of a mounted profile is impossible; renaming is possible. |

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
| FR-SSH-11 | M | **Jump host** (optional, one hop): host, port, user and its own auth (agent, key file or password). The password is stored as `DriveMount:<uuid>:jump-password`. The jump host's key is verified like any other host. | Mount through a bastion succeeds, and the bastion's key appears in known hosts. |
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
| FR-MNT-05 | M | Mounts appear as **network drives**: WinFsp volume prefix `\drivemount\<profile-slug>`, shown in Explorer under *Network locations*. The volume label is the profile name, truncated to 32 characters. Explorer names network drives after the UNC path ("slug (\\drivemount)"), not the volume label, so the profile name is also written to `_LabelFromReg` under `HKCU\…\Explorer\MountPoints2\##drivemount#<slug>` before Explorer hears of the drive, and removed on unmount (§5.8). | Explorer shows "Production (Z:)" with a network-drive icon. |
| FR-MNT-06 | M | An already-running Explorer is told about drive changes at once. A WinFsp letter stays invisible to a running Explorer, and `SHChangeNotify` alone does not help. So `WM_DEVICECHANGE` goes to Explorer's desktop window alone, limited to 500 ms, alongside `SHChangeNotify(SHCNE_DRIVEADD / SHCNE_DRIVEREMOVED)` (§5.8). Broadcasts of `WM_DEVICECHANGE` are forbidden, because they hang on unresponsive windows. | An open Explorer window shows the drive within 1 s and hides it in about 1.2 s, without restarting Explorer. The owner accepted the time for hiding on 2026-11-05. |
| FR-MNT-07 | M | **Read-only** profiles mount a read-only volume. Writes fail with "media is write protected" (`STATUS_MEDIA_WRITE_PROTECTED`). | Copying into a read-only drive fails cleanly. |
| FR-MNT-08 | M | **Mount on start**: profiles with `mount_on_start` are mounted in parallel when the app starts. Profiles that need a prompt queue their prompts one at a time. | 5 such profiles mount in parallel at start-up. |
| FR-MNT-09 | M | **Mount all** (toolbar) mounts every unmounted profile. **Unmount all** (toolbar and tray) unmounts everything. | Mount all with two profiles unmounted mounts both. |
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
| FR-TRAY-02 | M | Tooltip: "DriveMount: 2 mounted, 1 reconnecting". | Tooltip counts always equal the status-bar counts. |
| FR-TRAY-03 | M | Left click shows or hides the main window. | A hidden window is shown and focused; a visible one is hidden. |
| FR-TRAY-04 | M | Right-click menu (§7.6): one item per profile with a check mark when mounted (click toggles), *Unmount all*, *Open DriveMount*, *Quit*. | Toggling a profile from the tray mounts it without opening the window, unless a prompt is needed. |

### 3.6 Notifications (FR-NOT)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-NOT-01 | M | A Windows toast is shown when a mount fails, a connection is lost without auto-reconnect, reconnect stops (FR-REC-06), or a letter is reassigned (FR-MNT-04). | Each of the four events produces exactly one toast naming the profile and the error text from §7.8. |
| FR-NOT-02 | M | No toasts for successful mounts or successful reconnects. | Mount-on-start of 5 profiles produces no toasts. |
| FR-NOT-03 | M | Toasts can be switched off in Settings. | With notifications off, the FR-NOT-01 events produce no toasts; row errors are still shown. |
| FR-NOT-04 | M | Clicking a toast opens the main window with the profile selected, from the screen or from Action Center, whether or not the app is running (through a COM activator, §5.3). | Clicking a toast for "Dev box" shows the window with that row selected. |

- **What a toast says:** the profile's name, then the error's text and hint from §7.8; for a moved letter, "Mounted on Y: because Z: was in use."
- **Which changes:** every change into *Error* from another state, or to a different error; and the first event of a mount with `instead_of`. The same error or letter sent again is not another toast.
- **Sound:** none unless FR-SET-08 is on, as the row and the tray icon already show what happened.
- **One per profile in Action Center:** a toast is tagged with its profile's id, so a later one for the same profile replaces it.
- **A click** brings the window up and selects the profile's row; an open connection page shows that profile. A profile deleted since leaves the selection as it was.

### 3.7 Application lifecycle (FR-APP)

| ID | Pri | Requirement | Acceptance criterion |
|---|---|---|---|
| FR-APP-01 | M | **Single instance** per Windows user, enforced with the named mutex `Local\DriveMount`. A second launch brings the existing window to the front and exits. | Starting `drivemount.exe` twice leaves one process, with its window in front. |
| FR-APP-02 | M | **Close to tray** (default on): the window's close button hides the window. With it off, closing quits (FR-APP-04). | With the setting on, × hides the window and the mounts stay; with it off, × starts the quit flow. |
| FR-APP-03 | M | **Start with Windows** (default off) writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\DriveMount = "<exe>" --tray`. `--tray` starts without showing the window. | After sign-in, the tray icon appears, no window is shown, and mount-on-start profiles are mounted. Turning the setting off removes the Run value. |
| FR-APP-04 | M | **Quit** with mounts present asks "Unmount N drives and quit?". On confirm, it unmounts everything and exits. | After confirming, no DriveMount drive letters remain and the process has exited. Cancel keeps everything as it was. |
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
| FR-SET-08 | M | Notification sound (under *Show notifications*, off while it is off). Off adds `<audio silent="true"/>` to the toast | Off |
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
| NFR-PERF-03 | Size of `drivemount.exe` (release, stripped) | < 30 MB |
| NFR-PERF-04 | Idle working set after 60 s: no mounts / each additional idle mount | < 40 MB / < 8 MB |
| NFR-PERF-05 | CPU while idle with 5 mounts | < 0.1% average over 10 min; no periodic wake-ups apart from keep-alives |
| NFR-PERF-06 | Sequential read and write of a 1 GB file through Explorer copy, compared with OpenSSH `sftp` get/put on the same link | ≥ 90% (LAN and WAN) |
| NFR-PERF-07 | Copying 10,000 × 4 KB files to the drive (robocopy) on LAN | ≤ 3× the time of `scp -r` (relaxed from 1.3× in M2-08: robocopy needs about 10 round trips a file where `scp` streams, docs/perf/v1.md) |
| NFR-PERF-08 | Listing a directory of 1,000 entries: first listing on LAN / repeated within cache TTL | < 300 ms / < 20 ms |
| NFR-PERF-09 | Opening a mounted drive root in Explorer on LAN | < 500 ms until contents are shown |
| NFR-PERF-10 | UI responsiveness: no UI-thread work > 16 ms. All network and file system work runs off the UI thread. | Verified with the app's UI-thread clock (logged). One first redraw of new content a session at up to ~18 ms is accepted (M2-08, docs/perf/v1.md) |

### 4.2 Reliability (NFR-REL)

| ID | Requirement |
|---|---|
| NFR-REL-01 | **Data integrity**: for any file written through the drive, close → reopen → SHA-256 equals the source. This is tested for sizes 0 B, 1 B, 4095 B, 4 KiB, 1 MiB + 1 B and 1 GiB, and with `fsx` (§10.3). |
| NFR-REL-02 | **Visibility**: a file created, renamed or deleted through the drive shows in the very next directory listing (no stale cache from our own changes). |
| NFR-REL-03 | **Bounded waits**: no WinFsp callback blocks longer than `op_timeout_secs`. |
| NFR-REL-04 | **No orphans**: after the process exits for any reason, including a crash or a kill from Task Manager, no DriveMount drive letter remains. |
| NFR-REL-05 | **Config safety**: config writes are atomic (write a temp file in the same folder, flush it, then replace the file with `MoveFileExW`, through `tempfile`'s `persist`). The previous good file is kept as `config.toml.bak`. A config that cannot be parsed is never overwritten; the app starts read-only and shows an error. |
| NFR-REL-06 | **Soak**: 72 h with 5 mounts, periodic network drops and sleep/resume cycles. There must be no leak (working set growth < 10%) and no stuck state. |

### 4.3 Security (NFR-SEC)

| ID | Requirement |
|---|---|
| NFR-SEC-01 | Passwords and passphrases are stored **only** in Windows Credential Manager. They use `CRED_TYPE_GENERIC` and `CRED_PERSIST_LOCAL_MACHINE` (stored only on this PC, not synced as a roaming credential), with targets `DriveMount:<uuid>:password`, `DriveMount:<uuid>:passphrase` and `DriveMount:<uuid>:jump-password`. |
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
| NFR-UX-02 | The UI consists of the main window, three pages that each take the list's place (the connection page, the Add/Edit page and the settings page), and small prompts. There are no other windows. |
| NFR-UX-03 | Every error message says what happened and what to do (catalogue in §7.8). No raw error codes appear without explanation. |
| NFR-UX-04 | Everything can be done with the keyboard (§7.7). Every control has an accessible name for screen readers. |
| NFR-UX-05 | English only in v1. All strings are wrapped in Slint `@tr()` so translations can be added without code changes. `slint` has no `tr!` for Rust, so what Rust has to show comes from the `Texts` global in `apps/drivemount/ui/texts.slint`. |

### 4.5 Maintainability (NFR-MAINT)

| ID | Requirement |
|---|---|
| NFR-MAINT-01 | Rust stable, edition 2024. The minimum Rust version (MSRV) is pinned in `rust-toolchain.toml`. |
| NFR-MAINT-02 | CI runs `cargo fmt --check` and `cargo clippy -- -D warnings`. |
| NFR-MAINT-03 | Protocol crates (`drivemount-vfs`, `drivemount-sftp`, `drivemount-nfs`) must not depend on Windows APIs, so they build and test on Linux. |
| NFR-MAINT-04 | Structured logging with `tracing`; each mount is a span carrying the profile ID and name. |

### 4.6 Licensing (NFR-LIC)

DriveMount is open source under **GPL-3.0-or-later**, with WinFsp used through the `winfsp` crate. See [ADR-0003](adr/0003-licence-gplv3.md).

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
┌────────────────────────────── drivemount.exe (one unelevated process) ──────────────────────────────┐
│                                                                                              │
│  UI thread                          Tokio runtime (2–4 workers)        WinFsp dispatcher      │
│  ┌──────────────────────┐  commands ┌──────────────────────────┐       threads (per mount)    │
│  │ Slint main window     │─────────▶│ MountManager             │       ┌──────────────────┐  │
│  │ Profile dialog        │          │  • state machine/profile │       │ drivemount-winfsp       │  │
│  │ Prompts               │◀─────────│  • reconnect scheduler   │       │ FileSystemContext│  │
│  │ Tray (own thread)     │  events  │  • letter allocator      │       │  → RemoteFs      │  │
│  │ Win32 msgs (power,    │          │ ConfigStore  SecretStore │       │  (block_on with  │  │
│  │ session, net change)  │          └────────────┬─────────────┘       │   deadline)      │  │
│  └──────────────────────┘                        │ MountBackend        └────────┬─────────┘  │
│                                    ┌─────────────┼───────────────┐              │            │
│                                    ▼             ▼               ▼              │            │
│                              drivemount-sftp       drivemount-nfs (M4)    drivemount-smb (M3)        │            │
│                              RemoteFs       RemoteFs        native WNet         │            │
│                              (russh)        (nfs3_client)   mapping             │            │
│                                    └──── drivemount-vfs: caches, timeouts, names ◀──────┘            │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
          ▲ SSH :22                ▲ NFS :2049/:111                ▲ SMB :445 (by Windows)
```

### 5.2 Workspace layout

```
drivemount/
  Cargo.toml                 workspace: pinned dependencies, lints, release profile
  rust-toolchain.toml  rustfmt.toml  clippy.toml  .cargo/config.toml
  deny.toml                  cargo-deny config (licences, advisories)
  LICENSE                    GPLv3 text
  .github/workflows/ci.yml   CI (GitHub-Actions-compatible; run by Gitea Actions)
  scripts/                   CI checks: SPDX headers, exe size
  crates/
    drivemount-vfs/                 Shared traits and types, at the bottom of the graph: RemoteFs, Connector,
                             MountBackend, Prompter, Secret, SecretStore, Attr/DirEntry; caches, read-ahead, name and
                             permission mapping, operation deadlines, error model     (no Windows deps)
    drivemount-core/                Profile model, ConfigStore (TOML), MountManager, state
                             machine, letter allocator, event bus. Gets its backends from a factory
                             supplied by the app                                      (no Windows deps)
    drivemount-sftp/                RemoteFs over russh + russh-sftp; connector (auth, jump host, host keys,
                             keep-alive)                                                (no Windows deps)
    drivemount-winfsp/              Adapter RemoteFs → winfsp::FileSystemContext; volume params; mount/unmount
    drivemount-smb/                 [M3] WNetAddConnection2W / WNetCancelConnection2W / health check
    drivemount-nfs/                 [M4] RemoteFs over nfs3_client (MOUNT, NFSv3, portmap, AUTH_SYS)
    drivemount-win/                 Small Windows helpers: Credential Manager, Run key, SHChangeNotify, power,
                             session and network notifications, single-instance mutex, toasts
  apps/
    drivemount/                     GUI binary (windows subsystem): Slint UI (.slint files), tray, wiring.
                             A library beside it holds BackendFactory (Profile → Box<dyn MountBackend>)
  docs/
    ANALYSIS.md  SPECIFICATION.md  IMPLEMENTATION_PLAN.md  TRACEABILITY.md  PROGRESS.md  adr/  spikes/
  installer/                 WiX sources for the MSI
  tests/                     cross-crate integration tests, Docker compose files for test servers
```

Dependencies between the crates. Only `drivemount-vfs` is shared, so there is no cycle, and `drivemount-core` stays platform-independent, with its tests running on Linux:

```
drivemount-vfs ◀── drivemount-sftp, drivemount-nfs                 (portable)
drivemount-vfs ◀── drivemount-core                          (portable)
drivemount-vfs ◀── drivemount-winfsp, drivemount-smb, drivemount-win      (Windows; empty crates on other platforms)
drivemount-win ◀── drivemount-winfsp                        (the Explorer label and notifications)
apps/drivemount ──▶ all of the above
```

### 5.3 Process and threading model

- **One process** holds the UI and the engine. It runs unelevated in the user's session, so drive letters land in the user's logon session and are visible to their Explorer.
- **UI thread**: runs the Slint event loop, and shows toasts. Everything reaches Slint through `invoke_from_event_loop`.
- **Toast activator thread** (`drivemount_win::toast`): in the multithreaded apartment, it registers the COM class named by the AUMID key's `CustomActivator` (§9). Windows calls its `INotificationActivationCallback::Activate` on a COM thread for every click on a toast, on screen or in Action Center; a toast still on screen also fires its own `Activated` event, and both go to the window, where a second one changes nothing. If the app is not running, Windows starts the exe (with `-Embedding`) and makes the call once the class is registered. M2-04 chose this over `activationType="protocol"`, which starts a new process for every click, even with the app running, and so needs the single-instance hand-over of FR-APP-01 to work at all.
- **System events thread** (`drivemount_win::events`): a hidden top-level window, not a message-only one, as those get no broadcasts. It receives `WM_POWERBROADCAST` and `WM_QUERYENDSESSION`/`WM_ENDSESSION`. It has a thread of its own because the session's end is handled inside `WM_ENDSESSION`: Windows waits for that message, and may end the process once it returns. Network changes come from `NotifyIpInterfaceChange` and `NotifyUnicastIpAddressChange`, on the system's thread pool. Resume and network changes are handled as below (§5.5, "Network changes"); the session's end unmounts everything from that thread, for at most 4 s, then quits the event loop (FR-APP-05).
- **Tray thread** (`drivemount_win::tray`): the notification-area icon, on `Shell_NotifyIconW` directly, with a hidden top-level window of its own, so that it hears `TaskbarCreated` and puts the icon back when Explorer restarts, and the menu's modal loop holds up nothing else. Clicks and menu choices reach the window through `invoke_from_event_loop`; the window sends the icon, tooltip and menu after every change to its list. If the shell does not take the icon, it is tried again every 2 s for a minute, and on `TaskbarCreated`. M2-03 chose this over Slint's `SystemTrayIcon` (its Windows menu has no check marks, and its window is message-only, so the icon is lost when Explorer restarts) and over `tray-icon` (it ignores a failed `NIM_ADD`, so the app could not know the window has no way back).
- **Single instance** (`drivemount_win::instance`, FR-APP-01): before anything else, even the log, the app creates the mutex `Local\DriveMount`. If it existed already, the app looks for the running one's window for up to 5 s, lets it take the foreground (`AllowSetForegroundWindow`), posts it the registered message `DriveMount.ShowWindow`, and exits with 0, writing nothing. That window is message-only, of class `DriveMountInstance`, titled with the mutex's name, on a thread of its own; the message brings the main window up through `invoke_from_event_loop`. A mutex that cannot be made is logged, and the app runs anyway. `examples/connect.rs` uses `Local\DriveMount.Preview`, so it runs beside the app; a debug build takes the name from `DRIVEMOUNT_TEST_INSTANCE` if set, for the tests of the real exe.
- **Start in the tray** (FR-APP-03): `--tray` starts with the window hidden once the icon is in the tray. At sign-in Explorer may still be starting, so the app looks every 0.5 s for 70 s, longer than the tray's own minute of retries, and shows the window if the icon never got in, as there would be no way to it. Without a tray at all, the window shows at once. Any other argument, such as COM's `-Embedding` (toasts, above), starts the app as usual.
- **Quitting** (FR-APP-04): the tray's *Quit*, and × when it does not hide, ask "Unmount N drives and quit?" in the window (brought up if hidden) while any profile is *Connecting*, *Mounted* or *Reconnecting*, the profiles *Unmount all* would take. *Unmount and quit* or nothing to ask ends the event loop; then the drives go, for at most 10 s. *Cancel* and Esc leave everything as it was.
- **Crash hook** (FR-APP-07): installed first, once the logs folder is known. It writes the report (§6.4), logs the panic at Error, and then the default hook runs; the release build's `panic = "abort"` ends the process after that. A debug build unwinds as usual.
- **Tokio runtime**: multi-threaded with 2–4 workers. All network I/O, the mount manager and the reconnect timers run here.
- **WinFsp dispatcher threads**: WinFsp calls our `FileSystemContext` on its own thread pool, sized by `winfsp_threads`. Each callback runs `runtime_handle.block_on(timeout(op_deadline, fut))`. Dispatcher threads are not Tokio threads, so `block_on` is allowed there.
- **UI ↔ engine**:
  - Commands go from the UI to the engine over an `mpsc` channel: `Mount(id)`, `Unmount(id)`, `Cancel(id)`, `Test(profile)`, `SaveProfile`, `Quit`…
  - State events come back over a `broadcast` channel: `MountEvent { id, state, detail }`. The UI applies them with `slint::invoke_from_event_loop`.
- **Prompts**: backends never touch the UI. They call the `Prompter` trait, which the GUI implements by showing a dialog and awaiting a `oneshot` reply. Prompts are serialised, so there is one dialog at a time.

### 5.4 Core abstractions (summary; `drivemount-vfs` holds the signatures and their full contract)

```rust
/// Async file system operations on a remote. Implemented by drivemount-sftp and drivemount-nfs.
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
    async fn session_ended(&self);   // the last session ended by itself (keep-alives, transport); default: never
}

/// What the MountManager drives. Two kinds of implementation:
///  - WinFspBackend<C: Connector>: hosts a RemoteFs as a drive (SFTP, NFS)
///  - SmbBackend: native Windows mapping (M3)
#[async_trait]
pub trait MountBackend: Send + Sync + 'static {
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError>;
    async fn unmount(&self) -> Result<(), MountError>;
    async fn reconnect(&self, prompter: &dyn Prompter) -> Result<(), MountError>;   // same drive, new session
    fn health(&self) -> watch::Receiver<Health>;   // Ok | Lost(reason): Lost once per loss, until a reconnect works
    fn take_remembered(&self) -> Vec<(SecretSlot, Secret<String>)>;   // what the user ticked Remember for
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
| Connecting | Connected + mounted | Mounted | the backend has told Explorer (§5.8) |
| Connecting | Cancel | Disconnected | drop connection |
| Connecting | Error (auth, host key, unreachable, path…) | Error(reason) | toast (FR-NOT-01) |
| Mounted | Unmount requested | Unmounting | flush, stop host |
| Unmounting | Done | Disconnected | the backend has told Explorer (§5.8); zero secrets |
| Mounted | Connection lost, auto_reconnect | Reconnecting | keep the letter; start backoff |
| Mounted | Connection lost, no auto_reconnect | Error(Connection lost) | unmount, toast |
| Reconnecting | Reconnected | Mounted | reopen handles lazily |
| Reconnecting | Resume or network change | Reconnecting | retry at once, reset backoff |
| Reconnecting | Auth failure / host key changed | Error(reason) | unmount, toast; no further retries |
| Reconnecting | Unmount requested | Unmounting | stop retries |
| any | App quit / sign-out | Unmounting → (exit) | FR-APP-04/05 |

Every transition is emitted as a `MountEvent` and logged at Info level.

What the table leaves open (`drivemount_core::manager`):

- **A backend for each mount.** It is made when the mount starts and dropped when it ends, however it ends, and before the state is announced, so what the user typed goes with it (NFR-SEC-03). Going back to *Disconnected* or *Error* is what zeroes secrets.
- **One task for each profile,** so a dialog holds up no other profile. Cancel is a `CancellationToken`, and reaches a mount at any point.
- **A user cancelling a prompt** goes to *Disconnected*, as Cancel does. It is not an error.
- **Commands that do not fit the state:** Mount on a profile connecting or mounted is refused, with no event. Unmount while *Connecting* is a cancel, as that is what *Unmount all* means. Unmount and Cancel otherwise do nothing where there is nothing to stop. While *Unmounting*, commands wait, so a Mount right after an Unmount works.
- **A failed unmount** goes to *Error* with its reason, and lets go of the backend all the same.
- **`auto_reconnect`** is read when the connection is lost, as it can change while mounted.
- **Reconnecting.** The first attempt goes at once, on the same backend, whose connector remembers what was typed and a key trusted once, so nothing is asked again (FR-SSH-02). After each failure it waits 1, 2, 4, 8, 16, 32, then 60 s (FR-REC-02). A secret ticked *Remember* in a prompt during a reconnect is kept, as after a mount. The row keeps its letter, and the event carries it.
- **What stops the retries** is what the next attempt would meet again: E-AUTH, a changed, revoked or rejected host key, and E-KEY-FORMAT. The drive is unmounted and the profile goes to *Error* (FR-REC-06). A prompt the user cancels during a reconnect unmounts too, to *Disconnected*. Anything else, such as E-DNS, E-HOST-UNREACHABLE or E-PATH-NOT-FOUND, is tried again: a network or a NAS volume can come back.
- **Network changes** (FR-REC-03): waking from sleep, an interface changing and an IP address coming or going all count. One change brings a burst of notifications, so the app acts at the first and once more after a second with none, or after 5 s of a network that keeps changing. *Reconnecting* profiles try again at once and wait 1 s after the next failure. An attempt in progress is given up for a new one, as it may be stuck on the network that went, unless it is asking the user something. *Mounted* profiles ask the backend to check its connection: a WinFsp drive raises its suspect signal, so its watcher probes and says `Lost` if nothing answers. Other states ignore it.
- **Commands while *Reconnecting*** are as while *Mounted*: Mount is refused, Save keeps the connection fields locked, and Unmount, Delete and quitting drop the attempt in progress or the wait, and unmount. *Unmount all* includes a profile that is reconnecting.
- **Letters:** a letter is held from when it is chosen until the mount ends, because a drive only shows as taken once it is up, and profiles mounting together would otherwise all choose Z:. *auto*'s "next free letter" (FR-MNT-04) is the highest one free, as for *Auto*. A letter the backend finds taken after the check, as it may be after a long dialog, is dealt with the same way, without asking the user again. E-LETTER-TAKEN only comes from a profile's own letter under *fail*.
- **What is not mounted:** a profile that breaks a rule of §7.3 (only a key file's name is looked at), and one the file holds but the model cannot (§6.1). Both are refused with no event.
- **Test connection** has nothing to do with this table, and stores nothing, whatever the user ticks: the profile may never be saved.

### 5.6 VFS layer (drivemount-vfs)

**Caching.** All caches are per mount and in memory.

| Cache | Default TTL | Invalidation |
|---|---|---|
| Attributes (path → Attr) | `cache_ttl_secs` (5 s; 0–60) | Any local create/write/set_attr/rename/remove on the path, and flush or close after a write. Rename also invalidates the destination. A new or removed entry also invalidates its directory's attributes. |
| Directory listings | same TTL | Any local change to an entry inside the directory, including a write, because listings show size and time |
| Negative lookups (path not found) | 1 s; 60 s for Explorer "noise" names | Local create or rename onto the path; also a listing that shows the name, or opening the file |

- **Explorer noise names** get the longer negative TTL: `desktop.ini`, `Desktop.ini`, `thumbs.db`, `Thumbs.db`, `autorun.inf`, `AutoRun.inf`, `folder.jpg`, `folder.gif`, `$RECYCLE.BIN`, `System Volume Information`. They are looked up normally first, so real files with these names still work.
- **What is cached.** `stat` and `read_dir` only; `lstat` always reaches the server. What `set_attr` and `fstat` answer is kept as a `stat`'s answer for the path (M2-08), as it is the file after the change; `fstat`'s only while the handle's path still names its file, so not after a local remove or a rename over it, nor after a rename whose outcome is unknown. A write still drops the file's attributes, as the write itself returns none. A listing also fills the attributes of its entries, except symbolic links, whose entries do not describe the target. A TTL of 0 turns attributes and listings off; the negative TTLs stay.
- **Whole subtrees.** Rename, `remove_dir` and `mkdir` invalidate everything cached below the path as well as the path itself. Open handles follow local renames, so a later write invalidates the file's new path.
- **Invalidation cannot be skipped.** It runs when the call ends, even if the call failed or was dropped by a layer above, because the server may have acted anyway. An answer that was on its way while a change to its path finished is returned but not stored.
- **Age.** A TTL counts from when the question was asked, not from when the answer arrived.
- **Size.** Each of the three caches holds at most 10,000 entries, least recently used going first. A listing counts as one plus one per directory entry, so a directory of 10,000 or more entries is not cached.
- **Names, for case-insensitive lookups** (M2-08). Each listing also keeps the directory's names, indexed by how they compare on Windows ignoring case, with the listing's TTL. Unlike the listing, the names are updated in place by this mount's own creates, `mkdir`s, removes and renames that succeed, and dropped by one whose outcome is unknown, so filling a folder lists it once rather than once per new file (NFR-PERF-07). They have their own limit of 20,000, counted the same way, so a folder of 10,000 files still has its names kept. Changes by other clients show when the TTL runs out, as for listings.
- **Other clients.** Changes made by others, and local changes that reach a path through a symbolic link, show up when the TTL runs out.
- **Kernel-side caches.** WinFsp's own caches are configured **short and never infinite**: `FileInfoTimeout` = 1000 ms. This fixes NEO's NUL-byte bug class. Final values are confirmed in spike M0-1.

**Reads.**
- Each open handle tracks sequential access. On sequential reads, read-ahead grows from 128 KiB to 4 MiB.
- Requests are split into chunks of the server's maximum read size (from `limits@openssh.com`, else 32 KiB; never above 4 MiB) and sent in parallel, with up to 64 chunks and at most 4 MiB in flight per handle. More does not raise the rate and delays metadata on the shared channel (spike M0-2).
- **What counts as sequential.** A read that starts where the last one ended. The first read on a handle and any other jump fetch only what was asked for, so reading a file's header costs no extra traffic. So does the read after an end of file or an error.
- **Growth.** The window doubles each time the caller has consumed a window's worth, not on every read, so what is fetched stays in proportion to what is used. Beyond the range a read needs, requests wait until a chunk's worth or half a window is due, so small reads do not become small requests.
- **Freshness.** Data and errors fetched ahead are kept for at most 5 s from when they were asked for, the default `cache_ttl_secs`. A reader that takes longer goes back to a 128 KiB window, since its window was larger than it could use. A change through the drive drops what the reader holds. A speculative request that fails matters only if a read reaches it.

**Writes: bounded write-behind** (OD-4, M2-08, [ADR-0002](adr/0002-write-behind.md)).
- Each WinFsp `Write` call is split into chunks of the maximum write size, sent in parallel, and **returns once its data is on its way**, as long as no more than **4 MiB** is unacknowledged on that Windows handle (the same limit as reads). Acknowledging each call first ran at 49% of OpenSSH on a 50 ms link (spike M0-2).
- **Order.** A write waits for writes on their way that it overlaps, and for those of other handles on the same path. A read on any handle of the path, a size or attribute change, `Flush` and `Cleanup` wait for all of them first. So data read back is never stale, and nothing is unacknowledged once `CloseHandle` returns.
- **Paging I/O waits for its own arrival** (`ConstrainedIo`: the cache manager writing back, or a mapped view being flushed), as Windows waits for it and it can come after `Cleanup`.
- **Errors.** The first failure is kept and returned once, by the handle's next `Write` or `Flush`, then cleared, as Linux reports a failed write-back once. `Cleanup` cannot fail, so it logs a failure nobody heard of at Warn.
- **What Windows is told.** The size follows from the handle's last answer and its own writes; it is asked again only if something else on the drive changed the file, or for an append. The times stay the server's last answer until the writes arrive. Other handles, and listings, see a size up to 4 MiB behind while a file is being written.
- `Flush` calls `fsync@openssh.com` when the server supports it. Closing a file does not (M2-08): as on Windows, a closed file is promised only its writes, and `FlushFileBuffers` is what asks for the disk. The fsync at `Cleanup` cost ~5 ms a file, a third of a small file's time.

**Operation deadlines.** Every remote call runs under `op_timeout_secs`. On timeout:
- the call returns `STATUS_IO_TIMEOUT`;
- the connector runs a keep-alive probe;
- if the probe also fails, the mount becomes *Reconnecting*.

A timeout reported by the backend itself counts the same as an expired deadline. The deadline layer sits below the caches (`CachedFs<DeadlineFs<…>>`), so a call that timed out still invalidates them.

**Sessions across reconnects** (`SessionFs`, below the deadline layer: `CachedFs<DeadlineFs<SessionFs>>`). It holds the session in use, numbered by generation, in a `watch`, and a reconnect puts the next one there without the layers above noticing.
- **While there is no session,** calls wait for one. The deadline layer above limits the wait as it limits any call (FR-REC-04), and the WinFsp adapter reports a callback that ran out of time while there was no session as `STATUS_NETWORK_UNREACHABLE`, not `STATUS_IO_TIMEOUT`, and asks for no probe. Once the drive is unmounting, calls fail at once.
- **A call that finds the connection gone** marks the session lost, which is FR-REC-01's transport-error trigger. A call that is safe to repeat then waits for the next session and goes again: lookups, listings, reads, writes at an offset, flushes, attribute changes and opens that create nothing new. Creating a file exclusively, `mkdir`, removing and renaming are not repeated, as the server may have acted, and fail with `ConnectionLost`.
- **Handles are its own.** Each knows the generation it was opened in, and is opened again by path on its first use after a reconnect, never creating or truncating (FR-REC-05). Paths follow renames made through the drive. A handle of a session that has gone is closed by going with it, so closing it asks nobody. Several reads after a reconnect open the file once.
- **Directory enumerations restart** by themselves: each enumeration lists the directory afresh (§5.8).

**Name mapping.**
- Characters invalid in Windows names (`\ : * ? " < > |` and 0x01–0x1F) and trailing dots or spaces are shown as the Unicode private-use character U+F000 + code. This is the same scheme as Cygwin and SSHFS-Win, so names look the same as in NEO.
- All the dots and spaces at the end of a name are mapped, not just the last: `a..` is shown as `a` + U+F02E + U+F02E.
- The mapping is reversed when sending names to the server. A Windows name that would reach the server as `.` or `..`, or that contains `/` or NUL, is refused, so it cannot address a different directory.
- A remote name that really contains one of these private-use characters is shown as it is, so `a` + U+F03A and `a:` look the same on Windows; the scheme cannot escape them. The resolver below still finds such a name. If both exist, the Windows name means `a:`.
- Name matching is **case-preserving**: an exact-case lookup first, then a case-insensitive match against the directory's cached names (§5.6), used only when exactly one entry matches.
- Names that differ only by case are all listed.

**Permission and attribute mapping.**

| Remote | Windows |
|---|---|
| Directory | `FILE_ATTRIBUTE_DIRECTORY` |
| Regular file whose mode has no owner-write bit (`0o200`) | `FILE_ATTRIBUTE_READONLY` |
| Setting or clearing the Windows read-only attribute | clears or sets `0o200` on the remote. Ignored on a directory, where Windows gives it no meaning and losing the bit would stop new files going in. |
| Hidden, system and other Windows attributes | Not stored: setting them succeeds and changes nothing |
| Symlink | Resolved by the server (`stat`) and shown as its target. A broken link is listed, and opening it returns `STATUS_OBJECT_NAME_NOT_FOUND`. Creating symlinks or hard links returns `STATUS_NOT_SUPPORTED`. |
| Dot-files | Shown as normal files (not hidden) |
| mtime / atime | LastWriteTime / LastAccessTime. SFTP v3 has 1 s resolution. |
| (no birth time in SFTP v3) | CreationTime = ChangeTime = mtime |
| Owner/ACLs | Not mapped. Every file has one fixed security descriptor: full control for the current user. Access is decided by the server. Setting a descriptor gets `STATUS_NOT_SUPPORTED`, so that nobody believes a file was protected. `ReplaceFile` works without it. |
| New files and directories | Created without explicit permissions, so the server's umask applies |

### 5.7 SFTP backend (drivemount-sftp)

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
- **Remote path:** `~`, `~/below/home` or an absolute path; or `C:/data` or `C:\data`, which mean `/C:/data`. Anything else is E-PATH-NOT-FOUND. `SftpFs` paths are relative to the result: its `/` is the folder the mount shows, so nothing above it needs to know where that is.
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
  - The jump host goes first, through the same key check and login as any server, and with the profile's timeout, keep-alive and `known_hosts` file. A failure there never contacts the target. An error names the host it is about.
  - The target's name is resolved by the jump host. A channel the jump host refuses or cannot open in time is E-HOST-UNREACHABLE for the target, whatever the reason (bad name, refused port, forwarding not allowed), with the reason in the log: OpenSSH does not separate the first two, and §7.8 has no code for the third.
  - The two logins have separate memories. Only the jump host's password can be stored (NFR-SEC-01), so a passphrase for its key is asked for without *Remember*: once per mount, or not at all with an agent.
  - The connection to the jump host lasts as long as the target's session, and losing it ends that session.
- **One SSH connection per mount.** Metadata and data share one SFTP channel with pipelined requests. Spike M0-2 measured head-of-line blocking that grows with bytes in flight; the 4 MiB limit (§5.6) keeps it to about one extra round trip. A second SFTP channel for bulk transfers removes it and stays an option if M2-08 finds browsing during a copy too slow.
- **Window sizes:** the SSH channel window is 16 MiB (spike M0-2: 2 MiB halves WAN downloads, and 64 MiB gains nothing).
- **Keep-alive:** a `keepalive@openssh.com` global request every `keepalive_secs`. `keepalive_max` is 2, and `russh` closes at the interval after that many go unanswered, so a dead link is noticed 3 intervals after the last reply: 45 s at the default 15 s, within FR-SSH-12's 60 s. Only 2 keep-alives are actually sent in that time; the third would be due as it closes.
- **A connection ending by itself,** through unanswered keep-alives, the server closing it or the transport failing, ends `russh`'s task and drops its handler, which `Connector::session_ended` waits for. So an idle drive notices too, with no call going.
- **Reconnecting** uses the mount's connector, so a key trusted once is still trusted for that mount, and remembered passwords and keys are used again (§5.7, Logging in). The next mount has a connector of its own, and asks again.
- **Free space:** `statvfs@openssh.com`. Without it, total and free are reported as 1 PiB, so Windows' copy pre-checks never fail.
- **Rename:** `posix-rename@openssh.com` for rename-with-replace. Without it, a non-atomic fallback, logged at Debug level: first check that the source exists, that the two are the same kind and that a target directory is empty; then move the target aside, rename the source, and remove what was moved aside. If renaming the source fails, the target is moved back, so a failed rename leaves both paths as they were (`RemoteFs::rename`). This differs from the first design, which removed the target first and could lose it.
- **Windows OpenSSH servers** are not detected, because nothing depends on it: a drive-letter path is normalised on any server (FR-SSH-13), and a server that is not Windows has no such folder, which is E-PATH-NOT-FOUND. The home directory arrives from `realpath` as `/C:/Users/me` and needs nothing done to it.
- **Requests:** reads and writes longer than the largest request are split and sent together, within §5.6's limits per handle (64 chunks, 4 MiB). A write waits for everything it sent, and its first error is the result. A read returns what it has up to the first chunk that fell short or failed, so the caller meets a later failure on its next read. Every request has `op_timeout_secs`.
- **Errors:** `no such file` is `NotFound`, `permission denied` is `PermissionDenied` and `operation unsupported` is `NotSupported`. Any other reply that is not a timeout means the connection is gone, except a limit `russh-sftp` applies on this side. SFTP v3's catch-all code says little, so:
  - A name that was taken gives `Exists` after a look shows it is (exclusive create, `mkdir`, rename without replace), and `rmdir` gives `NotEmpty` after a listing shows entries.
  - A failed write gives `NoSpace` if `statvfs` then shows nothing free (§5.8), or if the server's text says so, on any request.
  - ProFTPD's text `Not a directory` gives `PathNotFound`.
- **Limits of `russh-sftp` 3.0.1.** Tests pin the first two, so that an upgrade shows when they go away, and a third test shows the last. The fallback in §12.1 would remove all three:
  - **File handles are decoded as lossy UTF-8**, though they are opaque bytes. OpenSSH's are 4-byte indexes, lowest free first, so the 129th open at once is garbled and useless. A session therefore holds at most 112 files open, and more gives an error, leaving room for 16 listings at once, beyond which listings wait. A handle that arrives garbled anyway is refused, and stays open on the server until the session ends. A server with binary handles from the start cannot be used.
  - **Status codes above SFTP v3's 8 end the session.** ProFTPD sends one when a rename without replace meets a taken name, so on a server without `posix-rename` such a rename looks first. Another client taking the name between the look and the rename would cost a reconnect.
  - **Names are decoded as lossy UTF-8 too.** A name that is not UTF-8 is listed with U+FFFD in it and cannot be reached: opening it gives `NotFound`, and two such names can look the same. It stays listed, so a folder that cannot be removed does not look empty.
- **Symbolic links:** `read_dir` and `lstat` report the link, as `RemoteFs` promises, and `stat` the target. Showing a link as its target (§5.6) is the WinFsp adapter's work: `stat` each `Symlink` entry, and list a broken one (M1-12a).

### 5.8 WinFsp adapter (drivemount-winfsp)

**Volume parameters**

| Parameter | Value |
|---|---|
| Sector size / sectors per allocation unit | 4096 / 1 |
| `MaxComponentLength` | 255 |
| `CaseSensitiveSearch` / `CasePreservedNames` / `UnicodeOnDisk` | false / true / true |
| `PersistentAcls` | false; fixed security descriptor giving the mounting user alone full control, and naming that user as owner and group: `O:<sid>G:<sid>D:P(A;;FA;;;<sid>)`. Not the Everyone descriptor of spike M0-1: `\\drivemount\<slug>` is reachable by other users of the PC, and they would act with this user's server login. |
| `ReadOnlyVolume` | `read_only`. This only tells programs: WinFsp's driver still passes changes down, so the adapter refuses them with `STATUS_MEDIA_WRITE_PROTECTED`. Opening a file with write access works, so that a program asking for `MAXIMUM_ALLOWED` is not turned away; the write itself fails. |
| `VolumePrefix` | `\drivemount\<profile-slug>` (network-drive mode). The slug is the name in lower-case ASCII, up to 32 characters, with anything else as `-`, then the first 8 hex digits of the id: `nas-backup-6f1c2c1e`. The id keeps it unique, since "a b" and "a-b" are different names, and it is taken at each mount. |
| `FileSystemName` | `DM-SFTP` / `DM-NFS` |
| Volume label | profile name, ≤ 32 chars |
| `FileInfoTimeout` | 1000 ms (see §5.6) |
| `PostCleanupWhenModifiedOnly` | true |
| `VolumeSerialNumber` / `VolumeCreationTime` | fixed per profile / mount time |
| `PassQueryDirectoryFileName` | true, for `GetDirInfoByName` |
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
| `Create` | `open(create, exclusive)` or `mkdir`, after looking for the name in any case |
| `Open` | `stat`; the remote handles are opened on first read and on first write |
| `Overwrite` | `set_attr(size = 0)` |
| `Read` / `Write` | `read` / `write` (§5.6); a write returns before it is acknowledged (write-behind, §5.6) |
| `Flush` | waits for this handle's writes, then `flush` (fsync) if it has a remote handle for writes; nothing for the whole volume |
| `GetFileInfo` | `fstat` once the remote handle is open, otherwise cached `stat` |
| `SetBasicInfo` | `set_attr(times, read-only bit)`; the creation and change times are ignored, as SFTP v3 has neither |
| `SetFileSize` | `set_attr(size)`. Allocation only cuts a file shorter, as no server has any to set. |
| `SetDelete` | check that a directory is empty. The `winfsp` crate offers this rather than `CanDelete`. The driver refuses a read-only file before this. |
| `Cleanup` | Waits for this handle's writes. With the delete flag: `close`, then `remove_file` / `remove_dir`. |
| `Close` | `close` |
| `Rename` | `rename(replace = flag)`, after looking for the new name in any case |
| `SetSecurity` | `STATUS_NOT_SUPPORTED` (§5.6) |
| `SetVolumeLabel` | `STATUS_NOT_SUPPORTED`: the label comes from the profile |
| `ReadDirectory` | cached `read_dir`, paged by marker |
| `GetDirInfoByName` | lookup in the cached listing |

- **Deadline.** A callback has `op_timeout_secs` in all, however many remote calls it makes (NFR-REL-03). Running out counts as a timeout for the "suspect" signal (§5.6), as a remote call's timeout does. After a failed read, the reader drops what it has in flight, so the next read does not meet the same failure.
- **Lookups.** `Open` and `GetSecurityByName` first try the name as spelled, and go down the listings only if that fails (§5.6).
- **Searches for one name.** WinFsp's driver upper-cases the name before `GetDirInfoByName`, so the case that was asked for is lost and only the listing can answer. If several entries match, the first by name is shown, as finding nothing would make `notes` and `Notes` both look missing. `FindNextFile` after such a search reads the directory as well.
- **File ids.** `Attr` has no inode, so the index number is a hash of the path. With one id for all, programs that compare ids would take all files for one.
- **Names Windows cannot take.** A name with a lone surrogate gets `STATUS_OBJECT_NAME_INVALID`. A listed name over 255 UTF-16 units is left out of listings, with a note in the log.
- **Remote handles.** A Windows handle has up to two: a read-only one with the read-ahead state, and a write-only one. A program can open a file for both and use only what the server allows it, with nothing to retry. A file both read and written through one handle costs two of the server's open files. They close with `Close`, or before the removal in `Cleanup`, as some servers will not remove an open file.
- **Open files follow renames** through the drive, those of a directory above them included. Handles that have opened nothing remotely yet use the new path too.
- **A change reaches every reader of the path.** Writes, size changes and `Overwrite` mark all handles on that path, and each drops what it fetched ahead before its next read (§5.6). So a change never waits for a read. Marking is by path, so a change through a symbolic link does not reach readers of its target until the 5 s limit.
- **`Write`.** An append (`WriteToEndOfFile`) asks `fstat` for where the end is, as another client may have added to the file. Paging I/O (`ConstrainedIo`), which does not change a file's size, and every other write use the size this handle last told Windows, kept up to date by its own writes; `fstat` is asked only if another handle on the drive changed the file since (§5.6, write-behind).
- **New names.** A name that exists in another case counts as taken, for `Create` and for `Rename`. Replacing it keeps the name already there rather than making a second that Windows would take for the first, so moving `a` over `Taken` as `TAKEN` leaves `Taken`. A rename that only changes the case uses the new spelling.
- **`Rename` refuses what a server would allow,** with `STATUS_ACCESS_DENIED` as on Windows: replacing a directory or replacing with one. The driver does the same for a read-only target.
- **`Cleanup` cannot report failure,** as Windows has already closed the handle: a failed removal, or a write that failed after its call returned and was not yet reported, goes to the log. So a failed `DeleteFile` reads as success, and the file is still there in the next listing. The removal never reaches a server on a read-only drive, where `FILE_DELETE_ON_CLOSE` would otherwise get this far unasked.
- **Mounting.** `MountBackend::mount` connects, writes the Explorer label, starts the drive and tells Explorer. Dropping the future cancels it at any point, and what was set up by then goes with it (FR-MNT-11). A letter in use gives `LetterTaken`. Each mount makes its own connection, and `unmount` drops it.
- **Unmounting.** `unmount` stops the host, tells Explorer, removes the label, then waits for their writes, flushes and closes on the server what programs still had open, in one `op_timeout_secs` at most. WinFsp calls no `Close` for those. Failures go to the log, and unmounting never fails over them (FR-MNT-10). The programs get an error at their next use of the handle.
- **Health.** `Lost` on the first of FR-REC-01's three triggers for the current session: the connection ending by itself (`Connector::session_ended`, which keep-alives drive), a call finding it gone (§5.6, sessions), or a rise of the suspect signal followed by a `Connector::probe` that fails. Timeouts while there was no session do not count for the next one. It stays `Lost` until `reconnect` puts a new session in, which sets `Ok`.
- **Reconnecting.** `reconnect` connects again through the same connector and hands the session to the drive (§5.6), which stays on its letter throughout. A failed attempt leaves the drive as it was, and dropping the call gives it up. `unmount` while there is no session first ends the calls waiting for one, so that stopping the drive is not held up by them.
- **Creating a file can take longer than one timeout while the connection hangs.** WinFsp's `FineGuard` lets a `Create` in only once no other callback runs on the drive, and each of those may take a timeout. Each callback still keeps to NFR-REL-03, and browsing (lookups and listings) does not wait like this.
- **Explorer.** WinFsp sets the letter without telling anyone, and keeps it tied to the process so that a kill removes it (NFR-REL-04), so the letter stays WinFsp's. `WNetAddConnection2W` through `WinFsp.Np` is not an option: it gives error 67 unless the class `drivemount` is registered as a launcher service under `HKLM`, which needs administrator rights. What works is `SendMessageTimeoutW(Progman, WM_DEVICECHANGE, DBT_DEVICEARRIVAL or DBT_DEVICEREMOVECOMPLETE)` with a `DEV_BROADCAST_VOLUME`, with `SMTO_ABORTIFHUNG` and 500 ms. It reaches one window, so no other program can hold it up. It is not sent after a crash, so an open window can list a dead drive until the next mount or unmount of that letter. Windows set to open folders in a separate process were not tried.
- **`.` and `..`** are listed outside the root, and `..` shows the directory's own details rather than cost a round trip for its parent's.

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
| The session's limit on open files | `STATUS_TOO_MANY_OPENED_FILES` |
| Anything else | `STATUS_UNEXPECTED_NETWORK_ERROR`, logged with details |

### 5.9 SMB backend (drivemount-smb), M3

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

### 5.10 NFS backend (drivemount-nfs), M4

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

`%APPDATA%\DriveMount\config.toml` is UTF-8 and contains no secrets. Writes are atomic (NFR-REL-05).

```toml
schema_version = 1

[settings]
start_with_windows = false
close_to_tray      = true
theme              = "system"     # system | light | dark
letter_conflict    = "auto"       # auto | fail
notifications      = true
notification_sound = false
log_level          = "info"       # error | warn | info | debug
op_timeout_secs    = 15           # not in UI, 5..=120
winfsp_threads     = 0            # not in UI, 0 = number of cores (max 8)

[[profiles]]
id             = "6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10"
name           = "Production"
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

- **`ui-state.toml`,** beside this file, holds the window's size and position (§7.1). It is apart because moving a window should not rewrite and back up the profiles, and because it still works when this file is read-only. Anything wrong with it costs only what it held.
- **Migrations:** when `schema_version` is older, the file is migrated in code with one function per version step, in memory on opening and on disk at the first save. The old file is kept as `config.v<N>.bak`. A failed migration leaves the app read-only. A newer `schema_version` than the app understands → the app starts read-only and says "This configuration was written by a newer DriveMount". Its profiles can still be mounted, read by this version's rules; Save and Delete are off.
- **Unknown keys** are kept on save, for forward compatibility, at the top of the file, in `[settings]`, in a profile and in its `jump`. Comments are not kept, and known keys are written in the order above.
- **A damaged file:** one that is not UTF-8 or TOML, has no `schema_version` (so an empty file too), or has `settings` or `profiles` of the wrong type. The app starts read-only with nothing loaded (NFR-REL-05). A save also refuses, and the app turns read-only, if the file has become like this since it was read.
- **One bad value does not cost the whole file:**
  - A profile that the model cannot hold (`port = 70000`, a `protocol` or `auth` it does not know, no `id`, a second use of an `id`, a `drive_letter` other than Auto or D–Z) is listed as unreadable, cannot be mounted, and is written back as it was, after the others, until it is deleted.
  - A profile that fits but breaks a rule of §7.3 (`keepalive_secs = 9999`) is loaded and listed, and the dialog marks the fields. A key file is not looked at on loading, only its name.
  - A setting with a bad value or out of range takes its default and is reported, and the next save replaces it.
- **Required in a profile:** `id`, `name`, `protocol`, `host`, `username`, `auth`, and `host` and `auth` in `jump`. The others take the defaults above: no `drive_letter` is Auto, no `key_path` is blank. No `username` in `jump`, or a blank one, is the profile's own (§7.3).
- `key_path` and `remember` stay in the file for an auth method with no use for them.

### 6.2 Secrets

Windows Credential Manager entries (NFR-SEC-01) are created only when "Remember" is ticked. They are deleted on profile delete, and when "Remember" is unticked.

- **On Save,** a profile keeps only what it says it has: the password if it is *Password* with "Remember", the passphrase if it is *Key file* with "Remember", and the jump password if the jump host uses one. Anything else is deleted. So is the passphrase when the key file changes, as it is of no use for another key, and a wrong one is E-AUTH with no second prompt. A change of host or user keeps them.
- **Ticked in a prompt:** the secret is stored once the login has worked, before the row shows *Mounted*, and the profile's "Remember" is turned on, unless the file is read-only. A secret that cannot be stored costs a question at the next mount, not the drive.
- **Order:** secrets go before the file changes. If the file then cannot be written, the worst that has happened is a question at the next mount, and the change can be tried again. The other way round, secrets could be left under a profile that no longer exists. A secret that cannot be deleted stops the Save or Delete. For an unreadable entry (§6.1), they go only if nothing else in the file uses that id.

The value is stored as UTF-16LE. Credential Manager holds at most 2560 bytes, so a secret longer than 1280 UTF-16 code units cannot be remembered, and storing one fails without changing the existing entry.

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

- **Location:** `%LOCALAPPDATA%\DriveMount\logs\drivemount.log`, rolled at midnight UTC and at 5 MiB to `drivemount.log.1`, up to `drivemount.log.6`, so 7 files of at most 5 MiB each. A log that cannot be written never stops the app.
- **Format and secrets:** text with timestamp (UTC), level, span (profile) and message. Secrets cannot appear (NFR-SEC-02). The *Log level* setting applies to DriveMount's own crates; other crates log Warn and above at most, as their debug output is not ours to vouch for.
- **Crash reports:** `crash-<timestamp>.txt` in the same folder, the time in UTC to the millisecond (`crash-2026-11-09T171153.042Z.txt`), so names sort by time; a second report in the same millisecond gets `-1` and so on. It holds the version, the time, the thread, where it panicked and its message, and a backtrace. `last-crash-noticed` beside them holds the newest report's name once a start has told the user of it: the window shows a banner, "DriveMount closed unexpectedly last time…", naming the report, with *Open logs folder*, which opens the folder and puts the banner away. A start in the tray shows the banner whenever the window is next opened; the next start does not show it again.

---

## 7. User interface

Built with **Slint** (Fluent style, software renderer). Theme: System / Light / Dark.

**Look.** Both themes use a warm palette: off-white or warm dark grey surfaces, and a grey-blue accent for focus and for the one main button in view. It is defined in `apps/drivemount/ui/theme.slint`. Text is **Inter** (SIL OFL 1.1) at 15 px, in Regular and SemiBold, subset to Latin and Latin Extended-A and embedded in the exe. Icons are **Lucide** SVGs (ISC), tinted to the theme. Both licences ship as `apps/drivemount/ui/fonts/OFL.txt` and `apps/drivemount/ui/icons/LICENSE`, and belong in the third-party licence file (NFR-LIC-04). DriveMount draws the window's own buttons. Text boxes, check boxes and drop-downs are Fluent's, and keep its blue focus line.

### 7.1 Main window (default 640 × 420, minimum 480 × 320; size and position remembered)

```
┌──────────────────────────────────────────────────────────────────┐
│  DriveMount                                           ─   □   ×  │
├──────────────────────────────────────────────────────────────────┤
│  DriveMount           [+ Add] [▷ Mount all] [□ Unmount all] [⚙]  │
├──────────────────────────────────────────────────────────────────┤
│  ⚠ WinFsp 2.1 or later is required…        [ Download WinFsp ]   │  ← only if missing
├──────────────────────────────────────────────────────────────────┤
│   ○  NAS backup                              [ Mount  ]    X:    │
│   ◌  Staging                                 [ Cancel ]    Y:    │
│   ✓  Production                              [Unmount ]    Z:  › │  ← › on hover
│   !  Dev box                                 [ Retry  ]          │
├──────────────────────────────────────────────────────────────────┤
│  1 mounted · 1 connecting · 1 failed                             │
└──────────────────────────────────────────────────────────────────┘
```

**Row status and primary button.** Every row is one line: status icon, name, primary button, drive letter.

| Status | Icon | Button |
|---|---|---|
| Disconnected | empty circle, grey | Mount |
| Connecting | loader, amber, pulsing | Cancel |
| Mounted | circle with a tick, green | Unmount |
| Reconnecting | loader, amber, and "Reconnecting…" on the page | Unmount |
| Error | circle with "!", red. The message is on the page | Retry (accent) |

**Rows and menus**
- **Drive letter:** the row shows it when mounted, and the preferred letter (dimmed) otherwise. *Auto* has none to show until it is mounted.
- **Toolbar:** *Add*, *Mount all*, *Unmount all* (FR-MNT-09) and Settings, each with an icon. *Mount all* and *Unmount all* are disabled when they have nothing to act on. *Add* is always there, and is the only way to add with the mouse.
- **Status bar:** also counts reconnecting and failed profiles ("· 1 reconnecting · 1 failed", in FR-TRAY-02's words), and says "Nothing mounted" when there is nothing to count.
- **Reconnecting** shows the drive's letter, as the drive is still there, and its page says "Reconnecting… (on Z:)". The loader does not pulse, as reconnecting can last for hours; *Open in Explorer* is off until it is mounted again.
- **Unmounting** has no row in the table above: an amber loader, with *Unmount* disabled.
- **What a row has no room for:** the address, "Y: (Z: was in use)" (FR-MNT-04), an error, and a profile that breaks a rule of §7.3 (its *Mount* is disabled until it is edited). These are on the connection page, and in a tooltip that shows after 0.7 s over the row. An entry of the file that cannot be read (§6.1) has a red warning icon and no button, and can only be deleted.
- **Mouse:** a click anywhere on a row except its button opens the row's **connection page** (§7.1.1). The right-click menu has *Details*, *Open in Explorer* (when mounted), *Edit…* and *Delete…*.

**List**
- Rows are one flat list, sorted by the drive letter the row shows (D to Z): the letter a mounted profile holds, otherwise the preferred one. Rows with no letter to show (*Auto*, not mounted) come last. Rows with the same letter are sorted by name, ignoring case. A row therefore moves when it is mounted or unmounted.
- **Rows** are separated by a 1 px line.
- **Empty state:** a drive icon, "No connections yet." and "Choose Add to set one up." There is no button in the middle: the toolbar's *Add* is the one.

### 7.1.1 Connection page

This page replaces the list inside the main window, as the settings page does. A ← back button or Esc returns to the list. While the page is up, the selection is its profile, so Enter, Ctrl+Enter, Ctrl+E and Del act on it.

```
┌──────────────────────────────────────────────────────────────────┐
│ [←]  (!) Dev box                                          [Retry] │
│          Failed                                                  │
│ ┌──────────────────────────────────────────────────────────────┐ │
│ │ (!) Authentication failed for me@devbox.                     │ │  ← only with a problem
│ │     Check the user name and password or key.                 │ │
│ └──────────────────────────────────────────────────────────────┘ │
│ ┌──────────────────────────────────────────────────────────────┐ │
│ │ Host            devbox                                       │ │
│ │ Port            22                                           │ │
│ │ User            me                                           │ │
│ │ Authentication  Key file: C:\Users\me\.ssh\id_ed25519        │ │
│ │ Remote path     /home/me                                     │ │
│ │ Drive           W:                                           │ │
│ │ Jump host       gate                       (only if set)     │ │
│ │ Mount at start, Reconnect, Read-only, Cache, Keep-alive      │ │
│ └──────────────────────────────────────────────────────────────┘ │
│ [✎ Edit] [🗑 Delete]                                              │
└──────────────────────────────────────────────────────────────────┘
```

- **Header:** the state icon, the name, the state in words ("Mounted on Z:", "Connecting…", "Failed", "Not mounted"), and the row's primary button. There is no *Open in Explorer* button: Ctrl+Enter and the row's right-click menu do that.
- **Size:** the settings are at 14 px, one tight line each, and the buttons come right under them. Opening the page grows the window to at least 600 × 680 so that nothing scrolls, as §7.2 does.
- **Problem:** what happened and what to do (§7.8). It is red for an error, and amber for a moved letter or a profile that needs correcting.
- **Settings:** read-only. No secret is ever shown.
- **Edit** opens the Add/Edit page (§7.2) in its place, and Cancel or Save returns to it. **Delete** asks first (FR-PROF-07), and the page closes when the profile is gone.
- An entry of the file that cannot be read shows why it cannot be read, and offers only *Delete*.

### 7.2 Add/Edit page

A page in the list's place, built like the connection page (§7.1.1): a ← back button (the same as Cancel) and the title, then a card of label and box rows, then *Test connection*, *Cancel* and *Save*. The toolbar stays, disabled. The fields scroll when they do not fit, and Tab scrolls to what it reaches. The main window's default size has no room for it, so opening it grows the window to at least 600 × 680, moved up if that would pass the bottom of the screen. Once neither this page nor the connection page is up, the window goes back as it was, unless the user has resized it meanwhile.

- **Advanced** is a button with a chevron icon, pointing right when closed and down when open. It opens by itself when something in it needs correcting, and stays open until that is done.
- **Drive:** lines of free letters read `Z: (free)`. Finding them does not hold up the page, so the marks come a moment after it opens.
- **Passwords:** a password is never read back. A box left empty keeps what is saved. *Remember* starts ticked in a new profile. A jump host's password box has no *Remember*: filled, it is stored; empty, it is asked for at each mount. A key's passphrase has no box, as the mock-up shows: it is asked for at the first use.
- **Saving:** Save waits for the file, and Cancel and Esc wait with it. If it fails, the page stays with what was typed and says why.
- **Test connection** needs no name. Any edit stops a test under way and clears what the last one said, and so does leaving the page. Cancelling a prompt during a test says nothing.
- **Hint "Unmount to change"** shows at the top, and locked fields are disabled. If a mount starts with the page open, the locked fields go back to what is saved.

- **v1:** shows SSH fields only.
- **From M3:** a segmented control **SSH | SMB | NFS** appears at the top, and the fields below change with it.

```
┌ Add connection ───────────────────────────────────┐
│ Name        [ Production                       ]  │
│ Host        [ 10.0.4.21               ] Port [22] │
│ User        [ deploy                           ]  │
│ Auth        (•) Key file  ( ) Password            │
│             ( ) SSH agent ( ) Ask each time       │
│ Key file    [ C:\Users\me\.ssh\id_ed25519  ][…]  │
│             [✓] Remember passphrase               │
│ Remote path [ /var/www                         ]  │
│ Drive       [ Z: ▾ ]   (Auto, D:…Z:, free ones marked) │
│ [✓] Mount when DriveMount starts                  │
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
| Name | 1–64 chars; unique (case-insensitive, ignoring surrounding spaces); no `\ / : * ? " < > \|` or control characters | "Name is required" / "Use at most 64 characters" / "A name cannot contain \ / : * ? " < > \|" / "A connection with this name already exists" |
| Host | DNS name, IPv4 or IPv6 literal; no spaces; no scheme, `user@` or `:port` (paste is split, FR-PROF-05) | "Enter a host name or IP address" |
| Port | 1–65535 | "Port must be 1–65535" |
| User | 1–64 chars, no spaces | "User name is required" / "Use at most 64 characters" / "A user name cannot contain spaces" |
| Key file | Exists, is readable and is OpenSSH/PEM format; `.ppk` rejected (FR-SSH-03); `.pub` rejected. Only for the *Key file* method | "Select a private key file" / "This file does not exist" / "This file cannot be read" / "Convert with PuTTYgen → Conversions → Export OpenSSH key" / "This is a public key; select the private key file" / "Unsupported key file format." |
| Remote path | `~`, `~/…`, or an absolute path starting with `/`, or a Windows `C:/…` or `C:\…` form; not above the root | "Use an absolute path such as /var/www, or ~ for home" |
| Drive | Auto or D–Z; a letter in use by another *profile* gives a warning only (allowed, because only one can be mounted) | "Choose Auto or a letter from D to Z" / warning: "Also used by 'NAS backup'" |
| Cache (s) | 0–60 | "Cache must be 0–60 seconds" |
| Keep-alive (s) | 0 or 5–300 | "Keep-alive must be 0 (off) or 5–300 seconds" |
| Jump host | Empty or `[user@]host[:port]`, with `[…]` around an IPv6 address that has a port. No user means the profile's own user, as with `ssh -J`; no port means 22 | "Use user@host:port" |

- **Empty boxes in a new profile:** Name, Host, User and Key file are not marked until they have held something, so that a new form does not open in red. Save is off all the same. A saved profile that breaks a rule shows it at once.
- **Passwords to be stored:** one longer than Credential Manager holds (1,280 UTF-16 units) is marked as "Too long to be saved: use at most {n} characters", and Save stays off, as it could otherwise only fail once the profile was saved.
- **Chars** are Unicode characters, not bytes.
- **Host:** letters, digits, `-` and `_` in labels. `_` is not legal DNS, but common on local networks. An international name goes in as `xn--…`, and a last label of only digits is refused as a mistyped address (`10.0.4.256`). An IPv6 scope (`fe80::1%4`) is not accepted.
- **Key file:** the name alone is enough to refuse (`.ppk`, `.pub`, in any case). Otherwise the start of the file decides: a PuTTY or public key by its content too, and a private key by its header. What only reading the key can show (an RSA key under 2048 bits, encrypted forms `russh` cannot read, §5.7) is E-KEY-FORMAT at connect. So a real OpenSSH key named `.ppk` can connect (§5.7) but cannot be selected here.

**Paste (FR-PROF-05).** Besides `[user@]host[:port][/path]` and `sftp://` before it, the form the main window shows is understood: `user@host:/path`, `user@host:~`, `user@host:C:/data`. So are `[::1]:22`, a bare `::1`, and a user with a domain (`me@corp.example@host`).
- In a URL, `%XX` is decoded. `/~` and `/~/…` after a host mean home, in both forms.
- A password (`user:secret@host`) is dropped, and goes in no field.
- All or nothing: text with any part that breaks its rule, or with another scheme, is not split. It stays in the Host box, which the Host rule marks.

### 7.4 Prompts

#### 7.4.1 Secret prompt

- **Title:** "Password for deploy@10.0.4.21", or "Passphrase for id_ed25519", or the keyboard-interactive server text.
- **Controls:** a masked field with a show toggle and a Caps Lock hint; a *Remember* checkbox, unticked to start with (not shown for keyboard-interactive prompts); *OK* / *Cancel*. The show toggle is the toolkit's own, the eye in the box, and shows once the box has something in it and the focus. *OK* needs something typed, except for a server's own question, whose answer may be empty if it says so.
- **Keyboard-interactive:** one prompt for each of the server's questions, with the user and host and the server's instructions above the first. An answer the server allows to be shown is not masked. A password question has no *Remember*, unlike what this section first said, as `Prompter::keyboard_interactive` returns answers only. To remember that password, use the *Password* method, whose stored password answers it (FR-SSH-07).
- **Order:** one prompt shows at a time, in the order they came, in front of the dialog and the delete question. A prompt whose mount is cancelled goes away.
- **Window behaviour:** brings the main window to the front if it is hidden.

An *Ask each time* profile first shows a prompt offering a password, or a key file plus passphrase (FR-SSH-05). It has no *Remember*, and neither has a passphrase prompt that follows it.

#### 7.4.2 Host key trust prompt

```
┌ Unknown server ──────────────────────────────────────────┐
│ DriveMount has not connected to 10.0.4.21:22 before.     │
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

An error dialog (no trust button) titled **"Server key has changed: connection refused"**, from a mount or from Test connection, in its turn among the prompts. It shows:
- the host;
- the old and new fingerprints;
- `known_hosts` line N;
- the text: "This can mean someone is intercepting the connection, or the server was reinstalled. If you are sure the change is expected, remove line N from the file and connect again."

Buttons: *Open file location*, *Close*.

### 7.5 Settings page

This page replaces the list area inside the main window; a ← back arrow returns to the list. It holds the items in §3.8, one per row; the log level row has *Open logs folder*. About is on the information page (§7.5.1). Changes apply immediately. Only "Start with Windows" touches the registry.

- **How a change is saved:** each change is in use in the window at once (theme, close button, toasts) and is then saved in the background, one after another, so a later change is never overtaken; changes made while one is saving count only as the latest. The log level changes once the file has it.
- **Start with Windows** writes or removes the Run value before the file is saved. If the registry refuses, the box goes back to off and a banner says why. If the file then cannot be saved, the Run value is put back. At start, with the setting on, the value is written again, as the exe may have moved; with it off, nothing is touched, so a value written by someone else (the installer) stays.
- **A failed save** shows a banner, and the page goes back to what is in use.
- **Read-only file:** the controls are disabled; the banner above says why.
- **WinFsp line** (FR-SET-07): "WinFsp 2.1.25156 installed"; too old: "WinFsp 2.0.x is installed, but 2.1 or later is required to mount SSH drives"; missing: the banner's text. The last two have *Download WinFsp*.

### 7.5.1 Information page

The toolbar's ⓘ button, left of Settings, or F1 opens this page in place of the list, as Settings does; the two never show together. A ← back button or Esc returns to the list. It has four sections:
- **About:** the name, one sentence on what DriveMount is for, the version, the build date (UTC, from `SOURCE_DATE_EPOCH` when set), and a link to the source repository: the package's `repository` (NFR-LIC-03). When that is empty, the line is not shown.
- **Keyboard:** the keys of §7.7.
- **Licence:** "Licensed under the GNU GPL v3", a sentence on what that means, and a link to the licence text.
- **Third-party components:** WinFsp with "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos" and a link to https://github.com/winfsp/winfsp (NFR-LIC-02); Slint (GPLv3 option); Inter (SIL OFL 1.1); Lucide (ISC); the Rust crates (MIT, Apache-2.0 and others); and a *Third-party licences* button, which opens `THIRD-PARTY-LICENSES.html` beside the exe in the browser. If it is not there (a development build), a banner says so.

### 7.6 Tray menu

```
 ✓ Production (Z:)
   Staging (Y:)        Connecting…
 ✓ Dev box (W:)        Reconnecting…
   NAS backup (X:)
 ─────────────────────
   Unmount all
   Open DriveMount
 ─────────────────────
   Quit
```

- **Icon** (FR-TRAY-01): a drive, grey when idle and grey-blue otherwise, with a badge: a green tick (*mounted*), an amber ring (*reconnecting*), a red "!" (*error*). Error outranks reconnecting, which outranks mounted. Each is an `.ico` at 16, 20, 24 and 32 px, drawn by `scripts/make-tray-icons.py`; the size nearest the system's small-icon size is used.
- **Tooltip** (FR-TRAY-02): "DriveMount: " and the status bar's counts joined by commas, or "DriveMount" alone when there is nothing to count.
- **Profiles:** in the list's order, with the letter the row shows. A check mark when *Mounted* or *Reconnecting*. On the right: *Connecting…*, *Reconnecting…*, *Unmounting…* or *Failed*. Choosing one does what the row's button does (mount, retry, cancel or unmount), and is greyed when that button is. Unreadable entries are not listed.
- **Unmount all** is greyed when nothing is up. **Open DriveMount** is the default item, in bold.
- **Clicks:** a left click, or Enter on the icon, hides a window that is on screen, and shows and focuses one that is hidden or minimized (FR-TRAY-03). A double click counts once. The right button, or the menu key, opens the menu.
- **No icon in the tray:** the window cannot be got back once hidden, so until the shell takes the icon × quits and Esc minimizes, whatever `close_to_tray` says.

### 7.7 Keyboard

| Key | Action |
|---|---|
| Ctrl+N | Add connection |
| Ctrl+E | Edit selected |
| Enter | Mount/Unmount selected |
| Ctrl+Enter | Open selected in Explorer |
| Space, Ctrl+I | Open the selected connection's page (§7.1.1) |
| Del | Delete selected (asks first) |
| Tab, Shift+Tab | Move between buttons and boxes |
| F1 | Open the information page (§7.5.1) |
| Esc | Close dialog / leave the connection, settings or information page / hide window to tray |
| Del, in the question it asks | Focus starts on *Cancel*, so Enter keeps the profile; Esc does too |
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
  - Per-machine install to `C:\Program Files\DriveMount\`; requires admin, which is also needed for WinFsp.
  - Checks the WinFsp version in the registry (`HKLM\SOFTWARE\WOW6432Node\WinFsp`, `InstallDir` + DLL version). If WinFsp is missing or older than 2.1, it offers the official, **unmodified** WinFsp MSI as a chained install (allowed by WinFsp's FLOSS exception, ADR-0003) or a link (OD-5).
  - Start-menu shortcut; optional *Start with Windows* checkbox (writes the HKCU Run key for the installing user).
  - Uninstall removes the program. It keeps `%APPDATA%\DriveMount` and the credentials unless "Remove my settings" is ticked.
- **Toast registration:** at start, the app writes `HKCU\Software\Classes\AppUserModelId\DriveMount` with a `DisplayName` and a `CustomActivator` (a CLSID), and `HKCU\Software\Classes\CLSID\{F458B6D8-450A-4371-8114-627B7E93EBBC}\LocalServer32` = `"<exe>"`. Spike M0-3 showed the AUMID key is what makes toasts appear; a shortcut with an AUMID alone did not. The CLSID is what a click in Action Center starts or calls (§5.3). Both are rewritten at every start, as the exe may have moved. The uninstaller removes them; the portable zip leaves them behind, as it has no uninstaller.
- **Portable:** a zip with `drivemount.exe`. It needs WinFsp installed separately.
- **Signing:** Authenticode for `drivemount.exe` and the MSI (OD-3).
- **Updates:** none in v1. Users install a newer MSI over the old one. Settings and profiles are kept and migrated (§6.1).
- **Licence:** DriveMount is GPL-3.0-or-later ([ADR-0003](adr/0003-licence-gplv3.md)). The MSI and zip include `LICENSE` and `THIRD-PARTY-LICENSES.html`. `cargo deny` enforces the allow-list from ADR-0003 (NFR-LIC).

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
- an OpenSSH server built from `tests/docker/sshd` (Debian, own image instead of `linuxserver/openssh-server`, which is built around one user), configured with password, key, certificate and keyboard-interactive (password + TOTP) users, and a second instance acting as a jump host. Further instances each differ in one way: no SFTP subsystem, one that exits at once, one that never answers, only SHA-1 host key signatures, two host keys, a rekey after every MiB, and paths laid out like Windows OpenSSH's;
- an SFTP server without OpenSSH extensions (ProFTPD `mod_sftp`), with all of them switched off;
- *(M4)* an NFSv3 server (`erichough/nfs-server` or Ganesha).

The `RemoteFs` contract-test suite (one generic suite run against each backend) covers:
- create, read and write at offsets, truncate, rename with and without replace, mkdir, rmdir on a non-empty directory, remove, set times and the read-only bit, statfs;
- large files (1 GiB);
- pipelined throughput;
- connection kill mid-read with reconnect and handle reopen;
- deadline expiry.

The generic part is `drivemount_vfs::contract` (feature `testing`): everything down to large files. It needs a server that allows any POSIX name and a user who is not root, and it cannot cover symbolic links because `RemoteFs` cannot create them. The last three items depend on the backend or on other layers and are tested with them.

### 10.3 Windows integration tests (Windows CI runner with WinFsp installed)

- Mount an **in-memory `RemoteFs`** and an **in-process russh SFTP server** (from the `russh-sftp` server side) as drives, then run:
  - `winfsp-tests-x64.exe` with the test subset that applies to a network file system without ACLs and streams. The excluded tests are in `tests/winfsp-tests-exclusions.txt` with reasons. It is not in the WinFsp installer, so `scripts/get-winfsp-tests.ps1` fetches it, pinned by hash. Each test runs alone, in its own folder and with a time limit, as the suite stops at its first failure and can hang;
  - `fsx` (file system exerciser), 100k operations, for data integrity (NFR-REL-01). There is no ready Windows build, so this is `crates/drivemount-winfsp/tests/fsx.rs`: the same operations and limits through Win32, mapped reads and writes included, with `FSX_OPS` and `FSX_SEED` to set the length and repeat a run;
  - scripted Explorer-like operations through Win32 `CopyFileEx`, `MoveFileEx`, `FindFirstFile` and `ReplaceFile`;
  - NEO regression cases: write → close → reopen → hash; a new file visible in the next listing; a host-key prompt for an unknown server; no drive left after the process is killed.
- Crash test: kill `drivemount.exe` with a mount active, then check with `GetLogicalDrives` that the letter is gone (NFR-REL-04).

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
| **M1 Core mount** | Workspace, drivemount-core, drivemount-vfs, drivemount-sftp, drivemount-winfsp, main window, profile dialog, prompts, Credential Manager, known hosts | Mount an SFTP profile from the UI; NFR-REL-01/02 pass; FR-PROF, FR-SSH and FR-MNT acceptance criteria pass |
| **M2 v1 release** | Reconnect (FR-REC), tray, notifications, settings, lifecycle (FR-APP), MSI, signing, user guide | All v1 requirements' acceptance criteria pass; NFR-PERF budgets met; 72 h soak passes |
| **M3 SMB** | drivemount-smb, SMB form fields, E-SMB errors | FR-SMB acceptance criteria pass on Windows 10 Home and 11 Pro against Samba and Windows Server |
| **M4 NFS** | drivemount-nfs (NFSv3), NFS form fields | FR-NFS acceptance criteria pass against knfsd, Synology and TrueNAS on Windows 10 Home |

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
| `russh-sftp` too slow or buggy | Medium / High | Spike M0-2 found the speed fine. M1-10 found three decoding limits (§5.7), worked around for now. Fallback: our own minimal SFTP v3 client on `russh` channels (the protocol is small: about 20 packet types). |
| Network-drive mode or some volume flags misbehave from an unelevated process | Low / High | Spike M0-1. Fallback: a plain drive-letter mount (not network mode). |
| Explorer is slow on high-latency links (many metadata calls) | Medium / Medium | Attribute, directory and negative caches; noise names; read-ahead; a configurable cache TTL |
| Slint and tray integration issues | Low / Medium | Spike M0-3. M2-03 took the fallback, a raw Win32 `Shell_NotifyIcon` in `drivemount-win` (§5.3). |
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
