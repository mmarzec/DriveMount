# NFS test matrix, v1.0 (M4-06)

| | |
|---|---|
| Requirements | [SPECIFICATION.md](../SPECIFICATION.md) FR-NFS-01 to 05, §5.10, §10.4 |
| Plan | M4-06, owner's checkpoint H-13 |
| Date | 2026-11-17 (automated rows); H-13 run by the owner 2026-11-18 (knfsd only) |
| Result | **Automated: every NFS suite passes against knfsd on the dev machine (Windows 10 Home 22H2) and in a Linux container run as CI's `nfs` job runs them; `fsx` 100,000 operations on an NFS drive lose nothing. CI's `nfs` job has not run on GitHub yet.** H-13 (§2): passed on knfsd (Ubuntu 26.04); Synology and TrueNAS not tested. |

## 1. Automated

| Suite | Where it runs | Server | Result |
|---|---|---|---|
| `crates/drivemount-nfs/tests/nfsfs.rs` (6 tests: the `RemoteFs` contract, the 1 GiB case included; new files owned by the UID/GID; symbolic links; a file another client replaced; an open file another client removed; the `ro` export) | CI job `nfs` (`ubuntu-latest`, knfsd on localhost); the dev machine at the WSL VM's IP | knfsd, `tests/docker/nfs` | **Pass**: Linux (non-root, as in CI) 3.9 s; Windows 10 Home at M4-03 |
| `crates/drivemount-nfs/tests/connector.rs` (6 tests: FR-NFS-03 reserved ports both ways, FR-NFS-04 a 40 MiB copy across a 10 s knfsd restart, FR-NFS-05 a refused and a missing export, fixed ports) | Same | Same | **Pass**, 2026-11-17: Linux 21.7 s, Windows 10 Home 22.3 s |
| `apps/drivemount/tests/nfs_drive.rs` (4 tests through the manager and the real factory onto a WinFsp drive: FR-NFS-01 fixed ports with `rpcbind` stopped and ownership checked on the server, *Test connection*, a read-only profile, `fsx`) | Dev machine only: needs WinFsp and knfsd in WSL | Same | **Pass**, 2026-11-17, Windows 10 Home 22H2 (FR-NFS-02) |
| `fsx_over_knfsd_loses_nothing` (`nfs_drive.rs`): the `fsx`-style exerciser on an NFS drive, from a reserved port on the `secure` export, the server's file hashed every 1,024 operations and at the end, then read through a new session | Dev machine | Same | **Pass**: 10,000 operations in 14.7 s; 100,000 in 144 s (seed `0x5eed0ff5`); 10,000 with seed `0x12345678` in 15.2 s |

Run them:

```powershell
tests\docker\servers.ps1 up
$env:DRIVEMOUNT_TEST_HOST = (wsl hostname -I).Split(' ')[0]
cargo test -p drivemount-nfs -- --ignored --test-threads=1
cargo test -p drivemount --test nfs_drive -- --ignored --test-threads=1
$env:FSX_OPS = '100000'; cargo test -p drivemount --test nfs_drive fsx -- --ignored --nocapture
```

On Linux, as CI's `nfs` job does (a reserved source port needs root unless the unprivileged range starts at 0):

```sh
sudo modprobe nfsd
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=0
docker compose -f tests/docker-compose.yml up -d --build --wait nfs
cargo test -p drivemount-nfs --locked -- --ignored --test-threads=1
```

## 2. Owner's matrix (H-13)

On **Windows 10 Home** (FR-NFS-02), in the installed app or `cargo run -p drivemount --release`. One column per server you have; mark ✓, ✗ (with a note below) or –. The knfsd column is a real Linux server, not the test container.

Before you start: the export must list this PC's IP (`/etc/exports`; on Synology, *Control Panel → Shared Folder → Edit → NFS Permissions*; on TrueNAS, the share's *Networks* or *Hosts*). Use the UID and GID of your account on the server (`id` there) in the profile, so that files you make are yours. Set *Settings → Log level* to *Debug* for the session, so a ✗ comes with a log.

| # | Scenario | Requirement | knfsd | Synology | TrueNAS | Notes |
|---|---|---|---|---|---|---|
| 1 | Paste `server:/export` into *Server*: Server and Export are filled | FR-NFS-01 | ✓ | – | – | |
| 2 | *Test connection* reports OK | FR-PROF-06 | ✓ | – | – | |
| 3 | Mount with the defaults (portmapper, reserved port): the letter shows in Explorer, the folders list, a file opens | FR-NFS-02 | ✓ | – | – | |
| 4 | Make a file and a folder in Explorer: on the server (`ls -ln`) they belong to the profile's UID and GID | FR-NFS-01 | ✓ | – | – | |
| 5 | A folder with a few thousand files lists completely, and again after the cache time | §5.10 (large replies) | ✓ | – | – | |
| 6 | Copy a 1 GiB file to the drive and back: `Get-FileHash` matches the original. Note the time each way | FR-NFS-02, NFR-PERF-06 | ✓ | – | – | |
| 7 | Save a Word or Excel file on the drive, close it, open it again | FR-MNT | ✓ | – | – | |
| 8 | NFS port 2049 and the server's MOUNT port (`rpcinfo -p server`) set under *Advanced*: mounts | FR-NFS-01 | ✓ | – | – | |
| 9 | An export with `secure` (the Linux and Synology default): mounts with *Use reserved source port* on; with it off, E-NFS-ACCESS | FR-NFS-03 | ✓ | – | – | |
| 10 | Remove this PC's IP from the export (or use an export that does not list it): *Test connection* and Mount show E-NFS-ACCESS with its hints | FR-NFS-05 | ✓ | – | – | |
| 11 | Restart the NFS service on the server (or the server itself) during a 1 GiB copy: the copy finishes, the hash matches, the drive comes back without being mounted again | FR-NFS-04 | ✓ | – | – | |
| 12 | Pull the cable or turn off Wi-Fi for a minute with the drive mounted: *Reconnecting*, then *Mounted*; an open Notepad file saves afterwards | FR-REC | ✓ | – | – | |
| 13 | A *Read-only* profile: writing is refused at once. A read-only export (`ro`): files read, writing is refused | FR-MNT-07 | ✓ | – | – | |
| 14 | Sleep and wake the PC with the drive mounted: the drive works afterwards | §10.4 | ✓ | – | – | |
| 15 | *Quit* with the drive mounted: the letter is gone from Explorer | FR-APP | ✓ | – | – | |

**If a drive keeps going to *Reconnecting* on one folder or one large read** while the server is fine, the server may be splitting a reply into several RPC record fragments, which `nfs3_client` refuses (spike M4-01 §6; knfsd never does). The Debug log then shows `NFS connection failed` with "Fragmented replies are not supported". Note the row and keep the log: the fix is a loop in the crate's reply reader.

Server details (product and version, kernel or OS, NFS options of the export):

- knfsd: Ubuntu 26.04, `nfs-kernel-server` from the distribution's package (exact package and kernel versions not noted).
- Synology: not tested.
- TrueNAS (CORE runs FreeBSD's server, SCALE runs knfsd): not tested.

Notes on ✗ cells:

- None. The owner ran the matrix on 2026-11-18 and reported no issues (all 15 rows ✓ for knfsd, as reported in chat; no per-row notes or timings kept). Synology and TrueNAS were not available, so their columns stay open.
