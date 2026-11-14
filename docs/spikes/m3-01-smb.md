# Spike M3-01: SMB mappings through the WNet API

| | |
|---|---|
| Risk | How `WNetAddConnection2W` behaves: credentials, error 1219, server reboots, and what we can test against |
| Code | `spikes/m3-01-smb/` (throw-away): `src/main.rs`, a command-line tool that calls `WNetAddConnection2W`, `WNetCancelConnection2W`, `WNetGetConnectionW` and `GetDiskFreeSpaceExW` and prints the Win32 result and how long each call took. For §11 also `WNetOpenEnumW`, `NetUseEnum`, and a window that prints `WM_DEVICECHANGE` and shell change notifications. Test server: `tests/docker/samba/` (service `samba` in `tests/docker-compose.yml`) |
| Machine | Windows 10 Home 22H2 (10.0.19045), Polish UI, Rust 1.99.1, `windows` 0.61. Server: Samba 4.17.12 (Debian bookworm) in Docker Engine 24.0.6 in WSL2; Windows' own server for `\\127.0.0.1` |
| Date | 2026-11-14 |
| Result | **Go**, with spec changes. `WNetAddConnection2W` does everything FR-SMB needs, but only with the network provider named in the call; without it, errors blur into 1203 and a call can take 47 s. The health check must have a timeout of its own: during an outage `GetDiskFreeSpaceExW` blocks for 36–82 s. Not tested: the CI runner (§8). Addendum (§11): other mappings can be listed in 2 ms without touching any server, also during an outage, but Windows sends no event when they change, so the list is polled. |

## 1. What was tried

- Every call goes through the spike tool, unelevated, in the owner's logon session. Passwords come from environment variables, never the command line.
- Drive letters `S:` to `V:` only. The owner's mappings `W:` to `Z:` were not touched, except one call that tried to map `W:` and was refused (§3).
- Samba has users `smbuser1` and `smbuser2` and shares `a`, `b` (writable) and `ro`. It is reached at the WSL VM's address (`172.23.105.89` on the day), see §8.
- The local share was the existing `\\127.0.0.1\Filmy` (Everyone: full), used read-only: map, free space, unmap.
- Outages: `docker restart` (a server reboot), `docker stop` / `start` (the host up, SMB down: connections refused), and `iptables -I DOCKER-USER -p tcp --dport 445 -j DROP` in WSL (a cable pull or a server that is powered off). `INPUT` does not see the container's traffic; `DOCKER-USER` does.
- During each outage, two processes ran side by side for 90–260 s: `watch` called `GetDiskFreeSpaceExW` on `S:\` every 500 ms, and `hold` kept a file open on `S:` and every second appended a line and read the file back.

## 2. Mapping and unmapping

| Case | Result |
|---|---|
| Explicit user and password, `CONNECT_TEMPORARY` | OK in 15–66 ms; `WNetGetConnectionW` returns the UNC; `GetDiskFreeSpaceExW` works |
| Same, without `CONNECT_TEMPORARY` (flags 0) | OK. **Not remembered either:** only `CONNECT_UPDATE_PROFILE` writes `HKCU\Network\<L>`. `CONNECT_TEMPORARY` stays, as the spec says, but flags 0 would not persist anything |
| `HKCU\Network\<L>` after a temporary mapping | Absent |
| Credential Manager after a mapping with explicit credentials | Unchanged (`cmdkey /list` the same). Nothing is saved without `CONNECT_SAVECRED` |
| A second share on the same server, no credentials given | OK: reuses the existing session |
| Unmap, no files open | OK in 13–19 ms |
| Unmap with a file open, `force = FALSE` | **2401** `ERROR_OPEN_FILES` in 15 ms |
| Unmap with a file open, `force = TRUE` | OK. The open handle then fails with **59** `ERROR_UNEXP_NET_ERR` |
| Unmap a letter that is not mapped | **2250** `ERROR_NOT_CONNECTED` |

FR-SMB-04's "ask first, then force" works as written: 2401 is the cue to ask.

## 3. Errors, and the network provider

Without `NETRESOURCEW.lpProvider`, MPR asks every provider in the order of `HKLM\SYSTEM\CurrentControlSet\Control\NetworkProvider\Order`. Here that is `WinFsp.Np, P9NP, RDPNP, LanmanWorkstation, webclient`. When SMB fails, the others are asked too and the result is merged. With `lpProvider` set to the SMB provider's name, only SMB is asked.

| Case | No provider given | Provider = SMB |
|---|---|---|
| Wrong password | 1326 `ERROR_LOGON_FAILURE`, 2.1 s | 1326, 24 ms |
| Share does not exist | **1203** `ERROR_NO_NET_OR_BAD_PATH`, 2.1 s | **67** `ERROR_BAD_NET_NAME`, 28 ms |
| Host name does not resolve | 1203, 2.1 s | **53** `ERROR_BAD_NETPATH`, 1.0 s |
| Address that does not answer (`192.0.2.1`) | 1203, **47.7 s** | 53, **26.7 s** |
| The same address again at once | – | 53, 13 ms (cached; see §6) |
| Another user, same server | 1219, 2.1 s | 1219, 12–14 ms |
| Letter of a local disk (`C:`) | 85 `ERROR_ALREADY_ASSIGNED`, 0 ms | – |
| Letter with a remembered mapping (`W:`) | **1202** `ERROR_DEVICE_ALREADY_REMEMBERED`, 0 ms | – |
| UNC without a share (`\\server`) | 67, 12 ms | – |
| *Current Windows user* refused by the server | – | **86** `ERROR_INVALID_PASSWORD`, 26 ms |
| Explicit wrong user and password at the Windows server (`\\localhost`) | – | 86, 16 ms |

- **Name the provider in every call.** Its name comes from `WNetGetProviderNameW(WNNC_NET_LANMAN)`. Here it is "Microsoft Windows Network" on a Polish Windows, and the same as `HKLM\...\LanmanWorkstation\NetworkProvider\Name`. Asking MPR is safer than a constant, in case the name is localised somewhere.
- Where the 2 s and the extra 21 s without a provider go was not traced. WinFsp's provider is first in the order, and it is on every machine that runs DriveMount's SFTP drives.
- **An unreachable host blocks the call for about 27 s**, and the call cannot be cancelled. It must run under `spawn_blocking`, as planned, and the row shows *Mounting…* meanwhile. A host name that does not resolve and an address that does not answer both give 53, so SMB has no E-DNS; both are E-HOST-UNREACHABLE.
- An authentication failure is 1326 at Samba but 86 at a Windows server, and 86 again for *Current Windows user* at Samba. Both are E-AUTH.

## 4. Error 1219

Reproduced at Samba: map `\\ip\a` as `smbuser1`, then `\\ip\ro` as `smbuser2` → 1219.

- Also with a mapping without a letter (`lpLocalName` null).
- **Also for the same user with a wrong password**, and for the same user written as `WORKGROUP\smbuser1` or `172.23.105.89\smbuser1`. Windows compares the credentials as given, not the account they name. The same user with the right password, in the same form, is OK. So a profile with a remembered password must spell the user exactly as the other mapping to that server did, or get 1219.
- Unmapping every drive of that server, then mapping as the other user, works at once, although the old session stays on the server for about 15 s more (`smbstatus`).
- **Another name for the server avoids it.** A second address on the WSL VM (`ip addr add 172.23.105.200/20 dev eth0`): `\\172.23.105.89\a` as `smbuser1` and `\\172.23.105.200\b` as `smbuser2` are mapped side by side. At the local server, `\\127.0.0.1`, `\\localhost` and `\\DESKTOP-…` are three servers too: with `\\127.0.0.1` mapped as the current user, `\\127.0.0.1` with other credentials gives 1219 and `\\localhost` gives 86. E-SMB-1219's hint (the IP address or another name) is right.
- 1219 comes back in 12 ms with the provider named. Nothing reaches the server: Windows decides it.

## 5. *Current Windows user*

- `\\127.0.0.1\Filmy` with null user and password: OK in 59 ms, and `\\localhost\Filmy` the same. That is the single sign-on path at a Windows server; a domain share needs a domain PC (owner's matrix in M3-05).
- At Samba, which has no such user: 86, as above.
- **Null credentials reuse an existing session to that server, whoever it belongs to.** With a `smbuser2` session still open from a mapping a moment before, null credentials mapped `\\ip\a` as `smbuser2`. With no session left (about 15 s after the last unmap), they gave 86. So *Current Windows user* after another profile's mapping to the same server means "as that profile's user". That is Windows' behaviour and the app cannot see it; the user guide should say so.

## 6. Server reboot and outages

`S:` mapped as `smbuser1`. Times are from the start of each run.

| Event | `GetDiskFreeSpaceExW` | The open file |
|---|---|---|
| **`docker restart`** at 10 s (about 1 s down) | One call took 1.0 s at 11.6 s, then OK. Never reported an error | **Lost: 59 on every call** from 11.6 s to the end (90 s) |
| **Port closed** (`docker stop`) 10–50 s | 53 after 6.1 s; then 53 at once, with a new attempt about every 30 s (2.0 s at 48.7 s); **OK again only at 78.8 s**, 29 s after the server was back | 53 while down; **59 after**: lost, as the server had restarted |
| **Packets dropped** 10–40 s | **Blocked for 36.4 s**, then OK at 47.0 s (7 s after the network came back). No error | **Kept**: the write blocked with the same call, then went on; no line lost |
| **Packets dropped** 10–160 s | **Blocked for 82 s**, then 53 at 92.6 s; then 53 at once, with a 21 s attempt at 143.7 s; OK at 173.8 s (14 s after) | 53 while down; **kept after**: the next write went through on the same handle |

- **The mapping itself survives every case**, and the letter stays. Windows reconnects on the next use, as FR-SMB-05 expects. Nothing in the app needs to remap.
- **Open files survive network outages but not a server restart.** Windows reopens them on a new connection (SMB 3 durable handles), even after 150 s, but a restarted Samba has forgotten them. A restarted Windows Server forgets them too, unless the share is continuously available (a cluster). Applications see 59 and have to reopen. That is Windows' behaviour; the app does not hold these handles.
- **A health check must have its own timeout.** In the first 36–82 s of a drop `GetDiskFreeSpaceExW` does not fail, it blocks. A check that waits for it shows *Mounted* for over a minute, or never, if the network comes back first. A check that gives up after a few seconds and calls the mount *Disconnected* reflects what Explorer shows (a hung drive). Only one check per mount may be in flight; a timed-out call keeps its thread until Windows returns.
- **Windows notices a returned server up to about 30 s late.** After a failure it tries again about every 30 s and answers 53 at once in between, as with the cached unreachable address in §3. So a health check on a network event can still see 53 for half a minute after the server is back. Checking again before *Open in Explorer* (spec §5.9) covers the case a user notices.
- Open handles to that server keep their own state; they are not refreshed by the drive's recovery (`hold` saw 59 to the end after the restart).

## 7. What the spike did not cover

- **Explorer**: whether a running Explorer shows a drive mapped by our process at once, as it did not for WinFsp in M0-1. WNet is the API Explorer itself uses, so it is likely, but it needs eyes. M3-04's H-06 should include it.
- **Elevation**: mappings belong to the logon session's token. An elevated process does not see the unelevated session's drives unless `EnableLinkedConnections` is set (documented behaviour, not tried). DriveMount runs unelevated, so its drives are where Explorer is.
- **Sign-out** (FR-SMB-07): a temporary mapping should not outlive the logon session anyway; the app removes its mappings at Quit and at `WM_ENDSESSION`. Tested in M3-02.
- Windows 11 24H2 requires SMB signing and refuses guest access by default. Samba signed this session (`smbstatus`: `partial(AES-128-CMAC)`), so it should be fine; to check on 11 Pro in M3-05.
- Kerberos, a domain, and a NAS: the owner's matrix in M3-05.

## 8. Test targets

| Target | Works | How |
|---|---|---|
| **Samba in Docker in WSL2** (dev machine) | Yes | `tests\docker\servers.ps1 up` starts it with the others. Port 445 is published inside the VM; Windows connects to the VM's address (`wsl hostname -I`), **not `localhost`**: Windows' own server holds 445, and Windows 10's SMB client cannot use another port (Windows 11 24H2 can, through `net use /TCPPORT` and `New-SmbMapping -TcpPort`; `NETRESOURCEW` has no port field, and this was not tried). The address changes when WSL restarts, so tests read it each time |
| A **second name** for Samba (1219 tests) | Yes | `ip addr add <ip+n>/20 dev eth0` in WSL, as root. Lost when WSL restarts; the test adds it and removes it |
| **Server down / cable pull** | Yes | `docker stop` / `start`; `iptables -I DOCKER-USER -p tcp --dport 445 -j DROP` and `-D` |
| **`\\127.0.0.1\share`** (dev machine) | Yes, current-user only | The owner's existing share, read only. A test share of our own, and a second local user for 1219, need administrator rights (`New-SmbShare`, `New-LocalUser`) |
| **`\\127.0.0.1\share` on the CI runner** | **Not tested** | Nothing runs on GitHub from here. A GitHub-hosted Windows runner is an administrator, so the job can create the share and two local users; WSL2 does not run there, so Samba cannot. On the owner's Gitea Windows runner it depends on that machine. Left for M3-05 |

The CI split for M3-05 is therefore: the Windows job maps a share it makes on its own machine (mount, unmount, forced unmount, 1219 between `\\127.0.0.1` and `\\localhost` with two local users, and the error codes of §3 that need no outage); Samba, outages and the second address stay local tests (ignored in CI, like `real_drive.rs`).

## 9. Consequences

For **M3-02 (`drivemount-smb`)** and the spec (§5.9, updated with this spike):

1. Set `lpProvider` to `WNetGetProviderNameW(WNNC_NET_LANMAN)` in every `WNetAddConnection2W` call.
2. Error mapping, from what Windows returned here:

   | Win32 | Meaning | Code |
   |---|---|---|
   | 1219 | Other credentials to that server | E-SMB-1219 |
   | 1326, 86 | Logon refused | E-AUTH |
   | 67 | No such share | E-SMB-SHARE-NOT-FOUND |
   | 53 | Host name not found or host not answering | E-HOST-UNREACHABLE |
   | 85, 1202 | Letter used by a disk, or remembered for another mapping | E-LETTER-TAKEN |
   | 2401 (unmap) | Files open | ask, then force |
   | 2250 (unmap) | Already gone | success |
   | anything else, including 1203 | | generic, logged |

3. Mount runs under `spawn_blocking` and can take 27 s against a host that does not answer.
4. The health check calls `GetDiskFreeSpaceExW` on its own thread with a timeout of a few seconds: no answer in time is *Disconnected*. One check per mount at a time.
5. Nothing to do after a server reboot: the drive comes back by itself; open files do not.

## 10. Data

Raw output was not kept as files; the numbers above are from single runs, 2026-11-14, on the machine in the header. To repeat: `cargo build --release` in `spikes/m3-01-smb`, start the server with `tests\docker\servers.ps1 up`, and run `target\release\m3-01-smb.exe` with no arguments for its commands. §11 uses the commands `list`, `list-loop` and `notify`, added for it.

## 11. Addendum: listing the mappings DriveMount did not make

Asked for FR-SMB-09 (show other network drives, read only). Same machine and day. The owner's mappings were `W:` and `X:` (connected), `Z:` (persistent, its server off) and `Y:` (`\\wsl$\…`, WSL's Plan 9 provider).

**What lists what**

| Call | Gives | Cost |
|---|---|---|
| `WNetOpenEnumW(RESOURCE_CONNECTED, RESOURCETYPE_DISK)` | Every mapped letter of the logon session, any provider: SMB, Plan 9, WinFsp. Letter, UNC, provider name | 10–19 ms for the first call in a process, 1–2 ms after |
| `WNetOpenEnumW(RESOURCE_REMEMBERED, …)` | The persistent ones (`HKCU\Network`), connected or not | under 1 ms |
| `NetUseEnum` level 2 | SMB uses only (no Plan 9, no WinFsp), with their status and the user name each was made with | under 1 ms |

- **"Remembered, not connected"** is a letter in the remembered list and not in the connected one. Made by `net use T: … /persistent:yes` and then `WNetCancelConnection2W` without `CONNECT_UPDATE_PROFILE`: `T:` left `GetLogicalDrives`, stayed in the remembered list, and `WNetGetConnectionW` gave **1201** `ERROR_CONNECTION_UNAVAIL` with the UNC. That is the state of a persistent mapping whose server was off at sign-in.
- **A remembered mapping whose server is off can still be "connected".** `Z:` was in the connected list, in `GetLogicalDrives` (type 4, remote) and `WNetGetConnectionW` returned 0, while `net use` called it *Disconnected*. Only `NetUseEnum`'s `ui2_status` tells: `USE_DISCONN` (2), with `USE_RECONN` (5) for a moment each time Windows tries again.
- **A temporary mapping is never remembered**: `S:` (our `CONNECT_TEMPORARY`) was only in the connected list, as §2 found through the registry.
- **DriveMount's own WinFsp drives** are in the connected list as `\\drivemount\<slug>`, provider `WinFsp.Np` (a test drive from `drivemount-winfsp`'s test support, `V:`). Not in `NetUseEnum`. Its SMB drives are in all but the remembered list, like any temporary mapping. So the app tells its own apart by the letters it holds, and drops `\\drivemount\…` too in case one is mid-mount or mid-unmount.
- Another WinFsp application's drives (`\\sshfs\…` from SSHFS-Win, say) would come out as their provider, `WinFsp.Np`; not tried.

**During an outage.** `S:` mapped to Samba as `smbuser1`; packets dropped from 5 s to 60 s (`iptables … DROP`, §1); `list-loop` every 250 ms; from 10 s a `watch` on `S:\` every second.

| Time | Listings | `NetUseEnum` status of `S:` | `GetDiskFreeSpaceExW` on `S:\` |
|---|---|---|---|
| 0–29 s | 2 ms or less | OK (0) | started at 10 s, blocked |
| 29–65 s | 2 ms or less | **RECONN (5)** | still blocked |
| 65 s on | 2 ms or less | OK (0) | returned OK at 65 s, after **55 s** |

- **No listing call blocked, at any point.** They read what the redirector already knows and never touch the server. They can run on the UI's timer without a thread of their own, unlike the health check (§6).
- The status lags: *OK* for about 24 s after the network went, *reconnecting* until about 5 s after it came back. It is only ever what Windows last found, and a drive no one uses can show *OK* for as long as the server is gone. So the app shows it as Windows' view, not as a check.

**Change notifications: none.** A hidden top-level window (message-only windows get no broadcasts) listened 20–35 s while `S:` was mapped and removed through WNet and `T:` through `net use`, temporary and persistent:

- no `WM_DEVICECHANGE` at all (no `DBT_DEVICEARRIVAL` with `DBTF_NET`);
- no shell notification through `SHChangeNotifyRegister` (all sources, `SHCNE_DRIVEADD`, `DRIVEREMOVED`, `NETSHARE`, `NETUNSHARE`, `SERVERDISCONNECT`, `MEDIA*`, `UPDATEITEM`), though the same window caught an `SHCNE_DRIVEADD` that a test sent with `SHChangeNotify` 0.5 s later. Mappings made by Explorer's own dialog may send one; not tried, and the app cannot count on it.

So the list is refreshed by polling. At 2 ms a call, every few seconds while the window is shown, plus on the events the app already handles (network change, its own mount and unmount, the window shown), costs nothing worth measuring.

**Consequences for the spec** (FR-SMB-09, §5.9, §7.1):

1. List with `WNetOpenEnumW`, connected then remembered, merged by letter; status from `NetUseEnum` for SMB letters: *Mapped* (0), *Reconnecting* (5, also 4), *Disconnected* (2, 3); a remembered letter that is not connected is *Not connected*. Others (Plan 9, other WinFsp) show *Mapped*.
2. Drop the letters the app holds and every `\\drivemount\…` UNC.
3. Never call anything on the drive itself (no free space, no `GetDiskFreeSpaceExW`): the listing is safe, the drive is not.
4. Poll every 5 s while the window is visible, and refresh on the events above; nothing while it is hidden.
5. The user name in `NetUseEnum` lets E-SMB-1219 name the drive *and* the user it was made as.
