# DriveMount user guide

DriveMount mounts folders on remote servers as Windows drive letters, over SSH (SFTP). You keep a list of connections, mount or unmount each with one click, and DriveMount restores them at start-up and after network drops. It lives in the system tray.

This guide is for DriveMount v1, which handles SSH/SFTP connections only.

**Contents**

1. [Installing](#1-installing)
2. [Adding a connection](#2-adding-a-connection)
3. [Ways to log in](#3-ways-to-log-in)
4. [Server keys](#4-server-keys)
5. [Mounting and using a drive](#5-mounting-and-using-a-drive)
6. [The tray](#6-the-tray)
7. [Settings](#7-settings)
8. [Keyboard](#8-keyboard)
9. [Troubleshooting](#9-troubleshooting)
10. [Where DriveMount keeps things](#10-where-drivemount-keeps-things)
11. [Licence and acknowledgements](#11-licence-and-acknowledgements)

---

## 1. Installing

### What you need

- Windows 10 22H2 or Windows 11 23H2 or later, 64-bit (x64). Home editions work.
- **WinFsp 2.1 or later.** WinFsp is the free driver that lets a program show files as a drive. The installer offers it if it is missing.

DriveMount itself runs as your normal user and never asks for administrator rights. Only installing it (and WinFsp) needs them.

### With the installer (MSI)

1. Run the DriveMount MSI and approve the administrator prompt.
2. If WinFsp is missing or older than 2.1, the installer offers to install it. WinFsp's own, unmodified installer is used.
3. Tick **Start with Windows** if you want DriveMount in the tray every time you sign in. You can change this later in Settings.

DriveMount goes to `C:\Program Files\DriveMount\` and gets a Start-menu shortcut.

**Upgrading:** run the newer MSI over the old one. Your connections and settings are kept, and are updated to the new version's format when needed.

**Uninstalling:** use *Settings → Apps* in Windows. Your connections, settings and saved passwords are kept, in case you install again, unless you tick **Remove my settings**. WinFsp is a separate program and stays installed; remove it on its own if nothing else uses it.

### Portable zip

The zip holds `drivemount.exe`, the licence and the third-party licences. Unpack it anywhere and run `drivemount.exe`. Install WinFsp yourself first, from <https://winfsp.dev/rel/>.

### Checking the download

The setup exe, the MSI and `drivemount.exe` are digitally signed. In File Explorer, right-click the file, choose **Properties** and open **Digital Signatures**: the signature should be there, valid, and by **SignPath Foundation**, which signs DriveMount for free as an open-source project. Windows shows the same name as the publisher when the installer asks for administrator rights. Each release also lists the files' SHA-256 checksums, in its notes and in `sha256sums.txt`. To compare, run in PowerShell:

```powershell
Get-FileHash .\DriveMount-1.0.0-setup.exe
```

The portable copy has no uninstaller. To remove it, quit DriveMount, delete its folder, and if you like, delete the data listed in [section 10](#10-where-drivemount-keeps-things).

### If WinFsp is missing

DriveMount still starts, and you can add and edit connections. A banner at the top of the window says *WinFsp 2.1 or later is required to mount SSH drives*, and **Mount** is disabled. Choose **Download WinFsp** to open the WinFsp download page, install it, then start DriveMount again.

---

## 2. Adding a connection

Choose **Add** on the toolbar (or press Ctrl+N). The *Add connection* page takes the place of the list.

| Field | What to enter |
|---|---|
| **Name** | Any name, up to 64 characters, different from your other connections. It becomes the drive's name in Explorer. It cannot contain `\ / : * ? " < > \|`. |
| **Host** | The server's name or IP address. |
| **Port** | The SSH port, usually 22. |
| **User** | Your user name on the server. |
| **Auth** | How you log in: *Key file*, *Password*, *SSH agent* or *Ask each time*. See [section 3](#3-ways-to-log-in). |
| **Remote path** | The folder on the server that becomes the drive's root. `~` is your home folder; otherwise an absolute path such as `/var/www`. For a Windows server, `C:/data` or `C:\data` works too. |
| **Drive** | *Auto*, or a letter from D: to Z:. Free letters are marked *(free)*; a letter another connection prefers says *(used by 'Name')*. *Auto* takes the highest free letter, starting from Z:. |
| **Mount when DriveMount starts** | Mount this connection each time DriveMount starts. |

**Pasting an address.** You can paste a whole address into **Host**, such as `deploy@10.0.4.21:2222/var/www` or `sftp://deploy@10.0.4.21:2222/var/www`, and DriveMount fills in user, host, port and path. `user@host:/path` and `user@host:~` work too. A password inside the address (`user:secret@host`) is dropped and goes in no field.

### Advanced

Choose **Advanced** to see:

| Field | What it does | Default |
|---|---|---|
| **Jump host** | A server to go through first, as `user@host:port` (like `ssh -J`). No user means the connection's own user; no port means 22. Choose how to log in to it: *Agent*, *Key file* or *Password*. A jump host password typed here is saved; left empty, it is asked for at each mount. | none |
| **Reconnect automatically** | After a network drop, keep the drive letter and keep trying to reconnect. Off: the drive is removed when the connection is lost. | on |
| **Read-only** | Mount the drive so nothing on it can be changed. | off |
| **Cache (s)** | How long, in seconds (0–60), DriveMount trusts what it last saw of a folder before asking the server again. Changes made through the drive itself always show at once; this only matters for changes made on the server by someone else. | 5 |
| **Keep-alive (s)** | How often, in seconds, DriveMount checks that the server is still there: 0 for off, or 5–300. After three missed checks, the connection counts as lost. | 15 |

### Test and save

**Test connection** logs in and checks that the remote path is a folder, without mounting. It says *✓ Connected (12 ms)* or shows what went wrong (see [section 9](#9-troubleshooting)). Any edit stops a test under way.

Fields with a problem are marked in red, and **Save** stays disabled until they are corrected. **Cancel** or Esc leaves without saving.

### Viewing, editing and deleting

Click a row (anywhere but its button) to open the connection's page: its state, any problem with what to do about it, and its settings. No password is ever shown.

- **Edit** (Ctrl+E) opens the same page as *Add*. While a connection is mounted, the fields that affect the connection are locked, with the hint *Unmount to change*. You can still rename it and change *Mount when DriveMount starts* and *Reconnect automatically*.
- **Delete** (Del) asks first. Deleting also removes the connection's saved passwords. A mounted connection is unmounted first.

The right-click menu on a row has **Details**, **Open in Explorer** (when mounted), **Edit…** and **Delete…**.

---

## 3. Ways to log in

| Method | How it works |
|---|---|
| **Key file** | Choose your private key file. OpenSSH and PEM formats work: Ed25519, ECDSA, and RSA of 2048 bits or more. If the key has a passphrase, you are asked for it at the first mount. With **Remember passphrase** ticked, it is saved and not asked again. |
| **Password** | Type the password. With **Remember password** ticked, it is saved; otherwise you are asked at each mount. When editing, leave the box empty to keep the saved one. |
| **SSH agent** | Uses the key already loaded in the Windows OpenSSH agent, or in PuTTY's Pageant. Nothing is asked and nothing is saved. |
| **Ask each time** | Nothing is saved. At each mount you choose **Use a password** or **Use a key file** (with its passphrase, if it has one). |

**Where passwords go.** A remembered password or passphrase is kept in Windows Credential Manager, on this PC only. It is never written to DriveMount's own files or its log. See [section 10](#10-where-drivemount-keeps-things).

**PuTTY keys (`.ppk`)** cannot be used directly. Convert the key in PuTTYgen: *Conversions → Export OpenSSH key*, save it, and choose that file.

**Certificates.** If a file named `<key>-cert.pub` (for example `id_ed25519-cert.pub`) is next to your key file, DriveMount offers that certificate to the server too.

**Two-factor login.** If the server asks extra questions, such as a one-time code, DriveMount shows each question in its own prompt, with the server's text. If one of the questions is the password and the connection uses *Password* with a remembered password, that one is answered for you. Other answers are never saved.

**The prompt.** A password prompt has a show button (the eye) to check what you typed, a *Caps Lock is on* warning, and a **Remember** box. Prompts come one at a time and bring the window to the front.

---

## 4. Server keys

Each SSH server has a key that proves which server it is. DriveMount checks it on every connection, as `ssh` does, using the same file: `%USERPROFILE%\.ssh\known_hosts`. This check cannot be switched off.

### The first connection

The first time you connect to a server, DriveMount shows **Unknown server**, with the key type and its fingerprint (`SHA256:…`). If you can, check the fingerprint with the server's administrator, or on the server itself:

```
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

- **Trust and save** adds the key to `known_hosts`. DriveMount, and `ssh`, will not ask again.
- **Trust once** connects this time only.
- **Cancel** does not connect.

A jump host's key is checked the same way.

### When the key has changed

If the server shows a different key from the one saved, DriveMount refuses to connect and says **Server key has changed: connection refused**. It shows the old and new fingerprints, and the line in `known_hosts` that holds the old key.

This can mean someone is intercepting the connection, or that the server was reinstalled. There is no button to connect anyway. If you are sure the change is expected:

1. Choose **Open file location**, and open `known_hosts` in a text editor.
2. Delete the line number shown.
3. Mount again. DriveMount now treats the server as unknown and asks you to trust its new key.

### Revoked keys

A line starting with `@revoked` in `known_hosts` marks a key that must never be trusted. DriveMount refuses such a server, and also checks the plain host name for a `@revoked` line when the server runs on another port.

Only `%USERPROFILE%\.ssh\known_hosts` is read; `known_hosts2` and the system-wide file are not.

---

## 5. Mounting and using a drive

### Mounting and unmounting

Each row shows the connection's state, its name, one button and its drive letter.

| State | Icon | Button |
|---|---|---|
| Not mounted | grey empty circle | **Mount** |
| Connecting | amber loader | **Cancel** |
| Mounted | green circle with a tick | **Unmount** |
| Reconnecting | amber loader | **Unmount** |
| Failed | red circle with "!" | **Retry** |

You can also press Enter on the selected row, or use the tray menu. **Mount all** and **Unmount all** on the toolbar act on every connection. The status bar at the bottom counts what is mounted, connecting, reconnecting and failed.

The drive appears in Explorer under *Network locations*, named after the connection. **Open in Explorer** (Ctrl+Enter, or the right-click menu) opens it.

**Unmount** finishes any writes still under way and removes the drive. It does not wait for programs that have files open on it; those programs get an error the next time they use the file.

### When the drive letter is taken

If the connection's letter is in use when it mounts, DriveMount by default uses the next free letter and tells you: the row says, for example, *Y: (Z: was in use)*. You can make it stop instead, in Settings: *When a drive letter is in use → Don't mount*.

Drive letters that Windows remembers for a disconnected network drive (a `net use` mapping) count as taken.

### Network drops and sleep

With *Reconnect automatically* on (the default), a lost connection keeps its drive letter and the row shows *Reconnecting…*. DriveMount tries again after 1, 2, 4, 8, 16 and 32 seconds, then every minute, for as long as it takes. When the PC wakes from sleep or the network changes, it tries at once.

While it is reconnecting:

- programs that use the drive wait up to 15 seconds; after that they get a "network unreachable" error rather than hanging;
- files left open carry on where they were once the connection is back;
- writes that were on their way when the connection dropped are sent once it is back.

Reconnecting stops, and the row shows the error, if the server refuses your login or its key has changed. Correct the problem, then choose **Retry**.

With *Reconnect automatically* off, a lost connection removes the drive and the row says *Connection lost.*

### Read-only drives

On a read-only drive, programs can open and copy files but not change them. Trying to save shows Windows' "media is write protected" error.

### Quitting

**Quit** in the tray menu asks *Unmount N drives and quit?* if anything is mounted. While DriveMount is not running, none of its drives exist: they cannot be left behind, even after a crash.

When you sign out or shut down Windows, DriveMount finishes its writes and unmounts everything without asking.

---

## 6. The tray

DriveMount keeps an icon in the notification area for as long as it runs. If Windows hides it under the ^ arrow, drag it out to keep it in view.

**The icon** shows the overall state: grey when nothing is mounted, grey-blue with a green tick when something is mounted, an amber ring while something is reconnecting, and a red "!" when something has failed. The worst state wins.

**The tooltip** has the same counts as the status bar, such as *DriveMount: 2 mounted, 1 reconnecting*.

**Left-click** the icon to show or hide the window.

**Right-click** for the menu:

- one line per connection, with its letter and a tick when it is mounted. Choosing it does what the row's button would: mount, unmount, cancel or retry. A connection that needs a password still shows its prompt;
- **Unmount all**;
- **Open DriveMount**;
- **Quit**.

**Closing the window** hides it to the tray by default, and the drives stay mounted. To make the close button quit instead, turn off *Close button hides to tray* in Settings.

**Notifications.** Windows notifications appear when a mount fails, when a connection is lost and is not reconnecting, when reconnecting stops, and when a drive went to another letter. Successful mounts make none. Clicking one opens DriveMount with that connection selected.

---

## 7. Settings

The gear button on the toolbar opens Settings. Changes take effect at once.

| Setting | What it does | Default |
|---|---|---|
| **Start with Windows** | Starts DriveMount in the tray when you sign in, without showing the window. Connections set to *Mount when DriveMount starts* are mounted. | off |
| **Close button hides to tray** | The window's × hides it, and the drives stay. Off: × quits, as **Quit** does. | on |
| **Theme** | *System*, *Light* or *Dark*. | System |
| **When a drive letter is in use** | *Use another free letter*, or *Don't mount*. | Use another free letter |
| **Show notifications** | Windows notifications for failures and moved letters. Errors still show in the window when this is off. | on |
| **Notification sound** | Plays Windows' sound with a notification. | off |
| **Log level** | How much goes into the log: *Error*, *Warning*, *Info* or *Debug*. **Open logs folder** opens the folder. | Info |

The last line shows the installed WinFsp version.

Two more settings are in the configuration file only (see [section 10](#10-where-drivemount-keeps-things)), under `[settings]`:

- `op_timeout_secs`: how long a program waits on the drive during an outage, 5 to 120 seconds (default 15);
- `winfsp_threads`: how many threads serve each drive, at most 8 (default 0, which means one per processor core).

Edit them while DriveMount is not running.

The **ⓘ** button, or F1, opens the information page: the version, a link to the source code, the keys, the licence and the third-party components.

---

## 8. Keyboard

| Key | Action |
|---|---|
| Ctrl+N | Add a connection |
| ↑ ↓ | Move the selection |
| Enter | Mount or unmount the selected connection |
| Ctrl+Enter | Open the selected drive in Explorer |
| Space, Ctrl+I | Open the selected connection's page |
| Ctrl+E | Edit the selected connection |
| Del | Delete the selected connection (asks first; Enter or Esc keeps it) |
| Tab, Shift+Tab | Move between buttons and boxes |
| F1 | Show the information page |
| Esc | Go back, close a dialog, or hide the window to the tray |

---

## 9. Troubleshooting

### Error messages

Each message says what happened, then what to do. They appear on the connection's page, in the row's tooltip, after **Test connection**, and in notifications. The code in brackets is what the log records.

#### Authentication failed for user@host. (E-AUTH)

The server refused the login.

- Check the user name, and the password or key file. Edit the connection, or use **Test connection** to try again.
- A remembered password that has changed on the server: edit the connection and type the new one. The old one stays saved until you do.
- *Key file*: check that the matching public key is in `~/.ssh/authorized_keys` on the server, and that the passphrase is right.
- *SSH agent*: check that the key is loaded (`ssh-add -l`). On Windows the *OpenSSH Authentication Agent* service is disabled by default; an administrator has to set it to start automatically.
- Reconnecting stops after this error rather than trying the same login again; choose **Retry** once it is corrected.

#### Server key has changed: connection refused. (E-HOSTKEY-CHANGED)

The server's key is not the one saved in `known_hosts`. See [When the key has changed](#when-the-key-has-changed).

#### Server key has been revoked: connection refused. (E-HOSTKEY-REVOKED)

A `@revoked` line in `known_hosts` (its number is shown) marks this server's key as one never to trust. Ask the server's administrator for its current key. Do not remove the line unless you know why it is there.

#### Server key not trusted. (E-HOSTKEY-REJECTED)

You chose **Cancel** at the *Unknown server* prompt. Mount again to see the prompt.

#### Host … not found. (E-DNS)

The host name could not be looked up. Check its spelling. If it is a name on a company network, check that the VPN is connected.

#### Cannot reach host:port. (E-HOST-UNREACHABLE)

The server could not be reached, or nothing answers on that port. Check the network, VPN or firewall, and that the port is right. Try `ssh -p <port> <user>@<host>` from a terminal to compare.

#### … did not respond in time. (E-TIMEOUT)

The server was found but did not answer in time. It may be overloaded, or a firewall may be dropping the connection without refusing it. Try again later, or check with the server's administrator.

#### Remote path … not found. (E-PATH-NOT-FOUND)

The folder in **Remote path** does not exist on the server, or you have no permission to see it. Edit the connection and check the remote path. Paths on Linux are case-sensitive.

#### Remote path … is not a folder. (E-PATH-NOT-DIR)

**Remote path** names a file. Change it to the folder that holds the file.

#### The server does not provide SFTP. (E-SFTP-UNAVAILABLE)

The login worked, but the server does not offer SFTP, which DriveMount needs. Ask the administrator to enable the SFTP subsystem (in OpenSSH, the `Subsystem sftp` line in `sshd_config`).

#### No encryption method in common with the server. (E-ALGORITHMS)

The server offers only old encryption or signature methods that DriveMount does not use, such as `ssh-rsa` with SHA-1. Ask its administrator to update the server's SSH settings. This is a deliberate safety choice and cannot be overridden.

#### Drive …: is already in use. (E-LETTER-TAKEN)

The connection's letter was taken and *When a drive letter is in use* is set to *Don't mount*. Choose another letter, set **Drive** to *Auto*, or free the letter. A letter held by a disconnected `net use` mapping counts as taken; remove it with `net use Z: /delete`.

#### No free drive letter. (E-NO-FREE-LETTER)

Every letter from D: to Z: is in use. Unmount or disconnect another drive.

#### WinFsp 2.1 or later is required. (E-WINFSP-MISSING)

WinFsp is not installed, or is too old. See [If WinFsp is missing](#if-winfsp-is-missing).

#### Unsupported key file format. (E-KEY-FORMAT)

DriveMount cannot read the key. Common causes:

- a PuTTY key: convert it with PuTTYgen, *Conversions → Export OpenSSH key*;
- an RSA key shorter than 2048 bits: make a new key, for example with `ssh-keygen -t ed25519`;
- a key in an older encrypted format: convert it with `ssh-keygen -p -f <keyfile>`, which rewrites it in the OpenSSH format.

When choosing a file, DriveMount also refuses a public key (`.pub`) and a `.ppk` file, and says so under the field.

#### Connection lost. (E-CONN-LOST)

The connection dropped and *Reconnect automatically* is off, so the drive was removed. Choose **Retry** to mount again, or turn on *Reconnect automatically* in the connection's **Advanced** settings.

#### Unexpected error. (E-INTERNAL)

Something went wrong that DriveMount has no specific message for. See the log for details: *Settings → Log level → Open logs folder*. Set *Log level* to *Debug* and try again for more detail. If you report it, include the log.

### Other problems

**The configuration file is damaged / could not be read.** A banner says so, and nothing can be saved until it is fixed, so that the file is not overwritten. Correct `config.toml`, or replace it with `config.toml.bak`, the previous good copy, and start DriveMount again.

**This configuration was written by a newer DriveMount.** You have opened the file with an older version. Connections can still be mounted, but nothing can be saved. Install the newer version to make changes.

**Some settings were not valid.** A setting in the file had a value DriveMount does not accept; its default is used and the next change of settings replaces it.

**A connection could not be read.** One entry in the file is broken, for example a port of 70000. It is listed with a red warning, cannot be mounted, and can only be deleted. Or correct it in `config.toml` while DriveMount is not running.

**This connection needs correcting.** The connection breaks one of the rules for a field, perhaps because the file was edited by hand. **Mount** stays disabled until you edit it.

**DriveMount closed unexpectedly last time.** DriveMount crashed and saved a report in the logs folder. Choose **Open logs folder**. If you report the problem, include that file and the log. Reports hold the version and where it failed, but no settings or passwords.

**Explorer does not show the drive.** It should appear within a second. Press F5 in Explorer. DriveMount drives belong to your Windows session only: programs running as administrator, or as another user, do not see them.

**A second copy will not start.** DriveMount runs once per Windows user. Starting it again brings the existing window to the front.

**The window cannot be found.** It is hidden in the tray: click the DriveMount icon, or choose **Open DriveMount** from its menu. Starting `drivemount.exe` again also brings it up.

---

## 10. Where DriveMount keeps things

| What | Where |
|---|---|
| Connections and settings | `%APPDATA%\DriveMount\config.toml` |
| Previous copy of that file | `%APPDATA%\DriveMount\config.toml.bak` (and `config.v<N>.bak` after an upgrade changed its format) |
| Window size and position | `%APPDATA%\DriveMount\ui-state.toml` |
| Log | `%LOCALAPPDATA%\DriveMount\logs\drivemount.log`, and up to 6 older ones, `drivemount.log.1` to `.6`, of at most 5 MiB each |
| Crash reports | `crash-<time>.txt` in the logs folder, and `last-crash-noticed` |
| Saved passwords and passphrases | Windows Credential Manager, *Windows Credentials → Generic Credentials*, named `DriveMount:<id>:password`, `DriveMount:<id>:passphrase` and `DriveMount:<id>:jump-password` |
| Trusted server keys | `%USERPROFILE%\.ssh\known_hosts`, shared with `ssh`; `known_hosts.lock` is left beside it, empty |
| Start with Windows | the `DriveMount` value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| Notifications | `HKCU\Software\Classes\AppUserModelId\DriveMount` and `HKCU\Software\Classes\CLSID\{F458B6D8-450A-4371-8114-627B7E93EBBC}` |
| Drive names in Explorer | `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##drivemount#…`, while a drive is mounted |

`config.toml` holds no passwords; you can back it up or copy it to another PC. The `<id>` is the connection's `id` in that file. Passwords saved on one PC are not copied with it, so you are asked for them on the new one.

DriveMount sends nothing anywhere but to the servers you set up: no telemetry and no update checks.

**Removing everything by hand** (for the portable zip, or after uninstalling without *Remove my settings*): quit DriveMount, then delete the `%APPDATA%\DriveMount` and `%LOCALAPPDATA%\DriveMount` folders, the `DriveMount:` entries in Credential Manager, and the registry keys above. Leave `known_hosts` alone if you use `ssh`.

---

## 11. Licence and acknowledgements

DriveMount is free software: you can share and change it under the terms of the [GNU General Public License, version 3](../LICENSE) or later. It comes with no warranty. The source code is at <https://github.com/mmarzec/DriveMount>.

DriveMount is built on these components, among others. The full list, with every licence, is in `THIRD-PARTY-LICENSES.html` beside `drivemount.exe`, which **Third-party licences** on the information page opens.

| Component | Licence |
|---|---|
| **WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos**, <https://github.com/winfsp/winfsp> | GPL v3 with FLOSS exception |
| [Slint](https://slint.dev), the user interface toolkit | GPL v3 (used under that option) |
| [Inter](https://rsms.me/inter/), the typeface, by Rasmus Andersson | SIL Open Font License 1.1 |
| [Lucide](https://lucide.dev), the icons, by Lucide Contributors and Feather | ISC, and MIT |
| russh, tokio, windows-rs, serde, tracing and the other Rust crates | MIT, Apache-2.0 and others |
