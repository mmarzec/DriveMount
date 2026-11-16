# Spike M4-01: NFSv3 with `nfs3_client`

| | |
|---|---|
| Risk | Whether `nfs3_client` can do what FR-NFS needs from Windows: reserved source ports, MOUNT with and without the portmapper, AUTH_SYS, and enough throughput |
| Code | `spikes/m4-01-nfs/` (throw-away): `src/main.rs`, a command-line tool with `ports`, `mount`, `bench` and `reconnect`. It also has a small pipelined ONC-RPC transport of its own over `nfs3_types`, to compare with the crate's client. Test server: `tests/docker/nfs/` (service `nfs` in `tests/docker-compose.yml`) |
| Machine | Windows 10 Home 22H2 (10.0.19045), Polish UI, Rust 1.99.1, `nfs3_client` 0.9.1, `nfs3_types` 0.5.3, Tokio 1.54. Server: Linux knfsd (WSL2 kernel 6.18.58.1, `nfsd` module) configured by a Debian bookworm container, Docker Engine 24.0.6 in WSL2 |
| Date | 2026-11-16 |
| Result | **Crate: go, with conditions** (§8). The crate takes a caller-made connection, so we bind the reserved port ourselves. MOUNT works through the portmapper and with fixed ports, and AUTH_SYS UID/GID reach the server. A pool of connections, one call in flight each, reaches 70–89 MiB/s up and 105–133 MiB/s down over a 50 ms WAN, above SFTP (M0-2). The crates are under the Unlicense, which the owner added to the allow-list after the spike ([ADR-0005](../adr/0005-unlicense.md), §7). |

## 1. What was tried

- Every run is unelevated, from Windows to knfsd at the WSL VM's address (`172.23.105.89` on the day), like Samba in M3-01.
- knfsd serves NFSv3 over TCP only (no v4, no UDP), with fixed ports: portmapper 111, NFS 2049, MOUNT 20048. Exports, each on its own 2 GiB tmpfs:

  | Export | Options |
  |---|---|
  | `/srv/nfs/rw` | `rw`, `secure` (the default), `root_squash` |
  | `/srv/nfs/insecure` | `rw`, `insecure` |
  | `/srv/nfs/ro` | `ro`, `insecure` |
  | `/srv/nfs/denied` | `rw` for `192.0.2.1` only |

- The container runs privileged with `network_mode: host`. knfsd is per network namespace, and Docker's port proxy would replace the client's source port, which `secure` checks. It needs `modprobe nfsd` in WSL once per boot.
- **WAN profile:** as in M0-2, `netem delay 50ms limit 200000`, here only on packets from source ports 2049 and 20048 (a `prio` qdisc with `u32` filters on the VM's `eth0`), so the other test servers are not slowed.
- Throughput: incompressible data, written with `WRITE UNSTABLE` and one `COMMIT` at the end, then read back. Every run compares a SHA-256 of what was read with what was written: **no mismatches in any run.** The server's disk is tmpfs, so this measures the client and the link, not a disk.
- What "LAN" means here: a VM on the same host, limited by CPU and the virtual network, as in M0-2.

## 2. Reserved source ports

The crate does not open sockets itself unless asked. Its `net::Connector` trait has `connect(addr)` and `connect_with_port(addr, local_port)`, and `MountClient::new_with_auth(io, …)` and `Nfs3Client::new_with_auth(io, …)` take any stream. So **we bind the port ourselves and hand the crate the connected stream.** No own RPC client is needed for this.

The spike binds `0.0.0.0:<port>` from 1023 downwards with Tokio's `TcpSocket`, and moves to the next port on `AddrInUse` or `PermissionDenied`.

| Case | Result |
|---|---|
| Bind 1023, unelevated | **OK**, no administrator rights needed (FR-NFS-03). First connection in 3 ms |
| Reconnect at once after closing 1023 | 1023 skipped, **1022** used; 0.9 ms |
| Port in TIME_WAIT, to the same server and port | `bind` succeeds, **`connect` fails with 10048** (`AddrInUse`) |
| Port in use to `:2049`, bound again for `:111` | `bind` fails with **10048**: without `SO_REUSEADDR`, Windows does not share a port between destinations |
| 16 connections held at once | 1021 … 1006 in 19 ms, each skipping the ports still in TIME_WAIT |
| Excluded port ranges (`PermissionDenied`, 10013) | Not met in 1023–512 on this machine |

- **Ports in TIME_WAIT stay taken for 2 minutes** (Windows' default). Each closed connection uses a port for that long, also across processes: in a day of spike runs, mounts started at 1009 and lower. With 512 ports (1023–512), that leaves room for several drives with pools of 8, and for reconnects. The port pool needs no state of its own: trying downwards finds the free ones in under 1 ms each.
- The error comes at `connect`, not at `bind`, for a TIME_WAIT port. The crate's own loop (`connect_from_privileged_port`) treats `AddrInUse` from either the same way, as our loop does.
- The crate's builder picks ports at random in **300–1023**, not "1023 downwards" (FR-NFS-03). We do not use the builder (§8).

## 3. MOUNT, portmapper and AUTH_SYS

Mounting `/srv/nfs/rw` takes 9–19 ms on LAN (portmapper 3–6 ms, MNT 1–8 ms) and 181 ms at 50 ms WAN. knfsd's root handle is 8 bytes; it offers `AUTH_SYS` only (flavors `[1]`).

| Case | Result |
|---|---|
| Portmapper (111), reserved port | OK. GETPORT gives MOUNT 20048, NFS 2049 |
| Fixed ports, no portmapper | OK (FR-NFS-01's "portmapper disabled" case, by not asking it) |
| The crate's own `Nfs3ConnectionBuilder`, both ways | OK |
| `secure` export, MOUNT from an unreserved port | **`MNT3ERR_ACCES`** |
| `insecure` export, unreserved ports throughout | OK |
| `secure` export, MOUNT from a reserved port, **NFS from an unreserved one** | MOUNT OK, `NULL` OK, then **every call `NFS3ERR_PERM`** |
| Client IP not in the export list | **`MNT3ERR_ACCES`** |
| **Export path that does not exist** | **`MNT3ERR_ACCES`** too, not `NOENT` |
| `ro` export, CREATE | `NFS3ERR_ROFS` |
| `FSINFO` | `rtmax` = `wtmax` = 1 MiB, `dtpref` 1 MiB, properties `0x1b` (link, symlink, homogeneous, can set time) |
| CREATE GUARDED as UID/GID 1000/1000 | File owned **1000:1000**, mode 644 |
| As 1234/5678 | Owned **1234:5678**: AUTH_SYS is taken as sent (spec §8.4's caveat) |
| As 0/0 | Owned **65534:65534** (`root_squash`) |
| CREATE GUARDED of an existing name | `NFS3ERR_EXIST` |
| GETATTR on the handle of a removed file | **`NFS3ERR_STALE`** (M4-02's re-resolve case) |

- **E-NFS-ACCESS has more causes than the spec lists.** knfsd answers `MNT3ERR_ACCES` for a wrong client IP, a `secure` export reached from an unreserved port, *and* an export path that does not exist. `NFS3ERR_PERM` on the first call after a good MOUNT means the NFS connection's port was not reserved. The hint text should name the path too (§9).

## 4. Throughput

The crate's `RpcClient::call` takes `&mut self` and waits for each reply before the next call: **one call in flight per connection.** So the crate's client was run over a pool of connections, each on its own reserved port, taking the next chunk from a shared counter. For comparison, the spike's own transport keeps many calls in flight on one connection: a writer task, a reader task, and replies matched by XID (about 120 lines, XDR through `nfs3_types`).

MiB/s, 1 MiB calls unless noted. LAN: medians of three 256 MiB runs (`results/bench-lan.txt`). WAN: two 128 MiB runs, which agreed within 2–8% (`results/bench-wan.txt`), so the range is shown.

| Client | Write LAN | Read LAN | Write WAN | Read WAN |
|---|---|---|---|---|
| Crate, 1 connection | 338 | 480 | 16.3 | 18.1 |
| Crate, 2 connections | 544 | 912 | – | – |
| Crate, 4 connections | 833 | 1,242 | 47–48 | 59 |
| **Crate, 8 connections** | **903** | **1,927** | **70–71** | **105–107** |
| Crate, 16 connections | – | – | 89 | 132–133 |
| Own, 1 connection, 4 in flight | 686 | 519 | 43 | 48–52 |
| Own, 1 connection, 16 in flight | 737 | 526 | 58–59 | 63–64 |
| Own, 1 connection, 32 in flight | 755 | 558 | 57–58 | 60–62 |
| Own, 1 connection, 64 in flight | 670 | 567 | 57 | 62 |
| Crate, 1 connection, 128 KiB calls | 205 | 198 | – | – |
| Crate, 8 connections, 128 KiB calls | – | – | 18.5 | 18.8–18.9 |
| Own, 32 / 64 in flight, 128 KiB calls | 623 / – | 511 / – | – / 59 | – / 62–64 |
| *SFTP, M0-2's chosen settings, for reference* | *221* | *188* | *35.6* | *49.6* |

- **One connection levels off at about 60 MiB/s over 50 ms, however many calls are in flight.** That is about 3 MiB in flight, a TCP window limit; it was not traced to which side's buffer. More connections get past it; more depth on one does not. On LAN the own transport is limited by its one reader task.
- **The crate over 8 connections beats the own transport everywhere**, and beats SFTP by 2–10× on LAN and 2× on WAN. So pipelining on one connection, the main reason for an own client, does not pay. A pool also keeps metadata calls from queueing behind a 1 MiB READ, as long as one connection is free.
- **Use the server's `rtmax`/`wtmax` (1 MiB here) as the call size.** 128 KiB calls are 2–4× slower without pipelining: at 8 connections over WAN, 18.5 MiB/s instead of 70.
- 16 connections gain another 25–30% over WAN. 8 is the proposed default (§8); more costs reserved ports (§2).
- These are protocol-layer numbers, without WinFsp or the caches. NFR-PERF is measured end to end in M4-06.

## 5. Reconnect (FR-NFS-04)

`reconnect` writes 32 MiB in 256 KiB `FILE_SYNC` calls, 50 ms apart. On an error or a 5 s timeout it opens a new connection from a new reserved port (no new MOUNT), checks the old file handle with GETATTR, and carries on with the call that failed. At the end it reads the file back and compares the SHA-256.

| Outage | What the client saw | Back after | Old handle | Hash |
|---|---|---|---|---|
| knfsd stopped for 10 s (`rpc.nfsd 0`, then started again) | `10053` `ConnectionAborted` at once; then `10061` (refused) on each try | 11.5 s | valid | matches |
| Replies dropped for 10 s (`netem loss 100%` on port 2049) | Nothing: the call hung until our 5 s timeout | 7.6 s after the timeout | valid | matches |

- **Handles survive a server restart and a new TCP connection, with no new MOUNT**, as the spec expects of stateless v3.
- **A refused connect takes about 2–3 s on Windows**, not milliseconds: only 4 tries fitted into the 11.5 s, 500 ms apart. Windows retries the SYN after an RST. The connector's backoff (FR-REC) should count from when each try ends.
- **A silent outage needs our own timeout**: the crate has none, and TCP alone did not notice within 10 s.
- **The crate's client is not cancel-safe.** Dropping a call mid-way (a timeout) leaves its reply unread on the connection, so the next call there gets the old reply and fails with `UnexpectedXid`. **A connection whose call was cancelled or failed must be dropped**, unless `RpcError::is_connection_reusable()` says otherwise. The spike always dropped it, and that worked.

## 6. Other things found in the crate

- **Fragmented replies are refused** (`RpcError::FragmentedReply`). knfsd sends each reply as one record fragment, so nothing failed here. Other servers in M4-06's matrix (TrueNAS CORE runs FreeBSD's server; Synology runs knfsd) may not. If one does, the fix is small (a loop in `recv_reply`, upstream or as a patch), or our own transport (§4) has it already.
- The crate panics only on calls over 4 GiB (`expect("message is too large")`), which we never send.
- A reply's length comes from the server and is allocated as it is (up to 2 GiB). A hostile server could make us allocate that; a limit belongs in a patch or upstream. Not a risk with trusted servers, but worth a note in M4-03's review.
- `nfs3_types` derives XDR for every RFC 1813 type. `READDIRPLUS`, `FSSTAT`, `SETATTR` and `RENAME` exist with the same pattern but were not called here.
- No idle timeout was tested: knfsd closes idle TCP connections after some minutes, which the pool will see as an error on the next call and reconnect, like an outage.

## 7. Licence

`nfs3_client`, `nfs3_types` and `nfs3_macros` are all **Unlicense**: a public-domain dedication, OSI-approved, and compatible with GPLv3 according to the FSF. It was **not** on the `cargo deny` allow-list, which ADR-0003 fixes, so `cargo deny check` would have failed as soon as `drivemount-nfs` depends on the crate. The fallback would have been our own client: the spike's transport plus XDR types, about the size of `nfs3_types` (1,750 lines, mostly declarations).

**Decided 2026-11-16 (owner):** `Unlicense` is allowed ([ADR-0005](../adr/0005-unlicense.md)), in ADR-0003's list, `deny.toml` and `about.toml`. Checked with `nfs3_client` added to `drivemount-nfs` for a moment: `cargo deny check licenses` rejects the three crates without the entry and passes with it.

## 8. Decision

**Use `nfs3_client` and `nfs3_types`; no own ONC-RPC client.** Conditions, for M4-03 and M4-04:

1. **Our own connector.** Bind from 1023 downwards to 512, skipping on `AddrInUse` (at `bind` or at `connect`) and `PermissionDenied`, and hand the stream to `MountClient::new_with_auth` and `Nfs3Client::new_with_auth`. Do not use `Nfs3ConnectionBuilder`: its ports are random in 300–1023. The portmapper connection needs no reserved port. `nodelay` on.
2. **A pool of NFS connections per drive, 8 by default**, each on its own reserved port, one call in flight each. Data and metadata share it. Calls of `min(rtmax, 1 MiB)` and `min(wtmax, 1 MiB)`.
3. **Drop a connection** after a timeout, a cancelled call, or an `RpcError` that is not `is_connection_reusable()`. Open a new one from a new reserved port; no new MOUNT, the handles are still valid.
4. **Every call has our own deadline** (`op_timeout_secs`); the crate has none.
5. **Retried non-idempotent calls.** After a reconnect, a retried CREATE GUARDED can give `EXIST`, and REMOVE or RMDIR `NOENT`, because the first try may have succeeded. NFSv3 has no exactly-once semantics across a server restart. M4-03 decides how to treat these, as RFC 1813 clients do. Not tested here.
6. **The MOUNT connection is closed after MNT.** We do not send UMNT on unmount, as the crate's `unmount()` would: it opens no state that matters to knfsd, and costs another reserved port.
7. **E-NFS-ACCESS** for `MNT3ERR_ACCES`, `MNT3ERR_PERM`, and `NFS3ERR_PERM`/`NFS3ERR_ACCES` on the first call after MOUNT. The hint names the export path as well as the IP, the `secure` option and the UID/GID.
8. The licence: settled, ADR-0005 (§7).

## 9. Consequences for the spec

- §5.10: the connector and the pool as in §8; `nfs3_client` without its builder; drop connections after cancellation; the crate's client has no deadline of its own.
- FR-NFS-03: "1023 downwards" is kept, with the lower bound 512.
- §7.8 E-NFS-ACCESS: add "the export path is wrong" to the hint, as knfsd gives the same answer.

## 10. Data

`spikes/m4-01-nfs/results/`: `ports.txt` (§2, saved from a re-run, same results), `mount.txt` (the mount cases), `bench-lan.txt` and `bench-wan.txt` (one `CSV,` line per run: MiB, chunk KiB, connections, depth, write, read), `reconnect-restart.txt`, `reconnect-blackhole.txt`.
