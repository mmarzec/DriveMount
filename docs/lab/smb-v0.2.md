# SMB test matrix, v0.2 (M3-05)

| | |
|---|---|
| Requirements | [SPECIFICATION.md](../SPECIFICATION.md) FR-SMB-01 to 10, §5.9, §10.4 |
| Plan | M3-05, owner's checkpoint H-12 |
| Date | 2026-11-16 (automated rows); H-12 rows to be filled in by the owner |
| Result | **Automated: Samba passes (9 tests) and the Windows-share suite passes (9 tests) on the dev machine; CI's `smb` job has not run yet.** H-12: the owner declared H-12 done on 2026-11-16 without recording the matrix row by row (§2 is left blank). |

## 1. Automated

| Suite | Where it runs | Server | Result |
|---|---|---|---|
| `crates/drivemount-smb/tests/samba.rs` (9 tests: map/unmap, E-AUTH, E-SMB-SHARE-NOT-FOUND, E-SMB-1219 naming the other drive, open files and forced unmount, a dropped backend, an outage, `net use` drives listed and taken over) | Dev machine only (`#[ignore]`): Docker in WSL2 | Samba 4 container, `tests\docker\servers.ps1 up` | **Pass**, 2026-11-16, Windows 10 Home 22H2, 14.6 s |
| `crates/drivemount-smb/tests/local_share.rs` (9 tests: map/unmap with read and write, *Current Windows user*, E-AUTH, E-SMB-SHARE-NOT-FOUND, *Test connection* with a sub-path, E-SMB-1219 at one address, another name avoiding it, open files and forced unmount, a dropped backend) | CI job `smb` (`windows-latest`), and the dev machine after `scripts\smb-test-share.ps1` in an elevated PowerShell | This machine's own Windows server: `\\127.0.0.1`, `\\localhost`, `\\%COMPUTERNAME%` | **Pass** on the dev machine, 2026-11-16, Windows 10 Home 22H2, signed in with a Microsoft account (share made by the owner, elevated), 0.13 s. Not run on GitHub yet |

Run them on the dev machine:

```powershell
# elevated, once
scripts\smb-test-share.ps1
# any shell
cargo test -p drivemount-smb --test local_share -- --ignored --test-threads=1
# elevated, when done
scripts\smb-test-share.ps1 -Remove
```

## 2. Owner's matrix (H-12)

**The owner declared H-12 done on 2026-11-16 and did not fill in the rows below.** No row is recorded as passed or failed; the table stays as the checklist for a later run (G4, or before v0.2.0).

In the installed app (or `cargo run -p drivemount --release`). One column per server you have; leave a cell empty if it does not apply. Mark ✓, ✗ (with a note below) or –.

| # | Scenario | Requirement | Windows share | NAS | Notes |
|---|---|---|---|---|---|
| 1 | Add an SMB profile by pasting `\\server\share\folder`: server, share and sub-path filled | FR-SMB-01 | | | |
| 2 | *Test connection* reports OK; with a wrong sub-path it says the path is missing | FR-PROF-06 | | | |
| 3 | *Password*, *Remember* ticked: mounts; a running Explorer shows the letter at once (open from H-06) | FR-SMB-02/03 | | | |
| 4 | Quit and start the app again: it mounts without asking for the password | FR-SMB-02 | | | |
| 5 | *Ask each time*: asks at every mount, nothing in Credential Manager (`cmdkey /list`) | FR-SMB-02 | | | |
| 6 | *Current Windows user*: mounts with no password (a Windows share signed into with the same account, or a domain) | FR-SMB-02 | | | |
| 7 | Wrong password: E-AUTH, nothing mapped | §7.8 | | | |
| 8 | Two profiles, same server, different users: the second shows E-SMB-1219 naming the first's letter (open from H-06); with the IP address instead of the name it mounts | FR-SMB-06 | | | |
| 9 | A file open in Notepad on the drive, then *Unmount*: the question; *No* keeps the drive, *Unmount anyway* removes it | FR-SMB-04 | | | |
| 10 | Pull the cable or turn off Wi-Fi for a minute: the row shows *Disconnected (Windows will reconnect)*, then *Mounted* again, without the app remounting | FR-SMB-05 | | | |
| 11 | Restart the server with the drive mounted: the drive comes back; open files have to be reopened (Windows' behaviour, spike M3-01 §6) | FR-SMB-05 | | | |
| 12 | *Quit* with a drive mounted: the letter is gone from Explorer | FR-SMB-07 | | | |
| 13 | Sign out with a drive mounted, sign in: no stale letter | FR-SMB-07 | | | |
| 14 | Optional, Windows 11 24H2: rows 3 and 7 (SMB signing is required there by default) | §10.4 | | | |
| 15 | Optional, a PC without WinFsp: an SMB profile mounts; only SSH rows are disabled | FR-SMB-08 | | | |

Server details (version, DSM / Windows edition, SMB dialect if known):

- Windows share:
- NAS:

Notes on ✗ cells:

-
