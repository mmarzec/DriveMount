# Spike M0-2: SFTP throughput with russh

| | |
|---|---|
| Risk | "`russh-sftp` maturity and throughput" |
| Code | `spikes/m0-2-sftp/` (throw-away): `src/main.rs` (benchmark), `bench.ps1` (driver), `net.sh` (server-side network and process controls), `results/` (raw data) |
| Machine | Windows 10 Home 22H2, Ryzen 9 5900X, Rust 1.99.1, `russh` 0.65.1, `russh-sftp` 3.0.1, `ring` 0.17.14. Server: S-03 container (OpenSSH 9.2p1, Debian) on Docker Engine in WSL2. Baseline: Windows OpenSSH 9.5p1 `sftp.exe` |
| Date | 2026-11-04 |
| Result | **Go.** With the chosen settings, reads and free-running writes reach 102–261% of OpenSSH on LAN and WAN. **OD-4: write-behind is needed**, because per-call writes reach only 49% on WAN. |

## 1. What was tried

- `russh` client with key auth, SFTP subsystem through `russh_sftp::client::RawSftpSession`.
- **Low-level API: available, no fallback client needed.** `read(handle, offset, len)` and `write(handle, offset, data)` take `&self`, so any number can be outstanding. `limits@openssh.com` is exposed through `limits()` and `set_limits()`.
- Each benchmark cell uses a new connection. Every downloaded byte is compared with the expected data. Uploads in the final runs are checked with `sha256sum` on the server. There were no mismatches and no short reads in any run.
- The test file is 1 GiB of incompressible data. Downloads go to no local disk on either side: `sftp.exe` writes to `NUL`, and the benchmark discards after comparing. `put` reads a file in the warm cache; the benchmark uploads from memory.
- Both sides use chacha20-poly1305 (the default that `sftp.exe` negotiates), unless stated otherwise. Connection settings: 16 MiB window, `nodelay` on, compression off.
- **WAN profile:** `netem delay 50ms limit 200000` on the server's egress (measured `stat` round trip: 50.7 ms). The limit is raised from the default 1000 packets, which would measure netem's queue rather than a link.

### What these numbers do and do not show

- "LAN" is a VM on the same host, so it is limited by CPU and the virtual network, not by a 1 Gb/s link.
- The time-capped matrix cells (20 s) do not transfer the whole file. The final runs do, three times each, except the cipher runs noted in §5.
- This measures the SFTP layer only. NFR-PERF-06 is defined through an Explorer copy, so it is still to be verified end to end in M2-08. The per-call mode assumes sequential 1 MiB WinFsp writes, as the plan does; the real call pattern has not been measured.
- Only OpenSSH was tested. M1-10 adds ProFTPD.

## 2. Throughput against the baseline

Medians of three whole 1 GiB runs, MiB/s. Largest chunk = 261,120 bytes (`limits@openssh.com`). The percentage is against OpenSSH's default settings.

| Operation | LAN | % of `sftp.exe` | WAN | % of `sftp.exe` |
|---|---|---|---|---|
| **Baseline** `sftp.exe` get / put | 108.2 / 84.6 | | 35.4 / 35.0 | |
| Download, 64 KiB × 64 | 182.2 | 168% | 50.3 | 142% |
| Download, largest × 64 | 190.1 | 176% | 43.9 | 124% |
| Download, largest × 32 | 187.0 | 173% | 46.2 | 131% |
| **Download, largest × 16 (chosen)** | **187.8** | **174%** | **49.6** | **140%** |
| Upload free-running, largest × 64 | 216.9 | 256% | 35.6 | 102% |
| Upload free-running, largest × 32 (≈ 8 MiB) | 213.4 | 252% | 35.4 | 101% |
| **Upload free-running, largest × 16 (≈ 4 MiB, chosen)** | **221.1** | **261%** | **35.6** | **102%** |
| **Upload per-call, 1 MiB, largest × 64** | **151.8** | **179%** | **17.2** | **49%** |

Time-capped matrix, MiB/s (`results/matrix-*-vm.csv`, all chunk × in-flight cells). Largest chunk shown:

| In flight | Download LAN | Upload free LAN | Upload per-call LAN | Download WAN | Upload free WAN | Upload per-call WAN |
|---|---|---|---|---|---|---|
| 1 | 130.9 | 114.8 | 97.3 | 4.7 | 4.6 | 3.7 |
| 8 | 204.1 | 228.6 | 167.4 | 34.6 | 34.3 | 16.6 |
| 32 | 195.6 | 231.6 | 164.3 | 47.9 | 35.2 | 16.7 |
| 64 | 112.8 | 228.9 | 166.1 | 43.1 | 35.3 | 16.6 |

- **Chunk size matters less than bytes in flight.** On WAN at 32 in flight, 32 KiB gives 18.2 MiB/s and the largest chunk 47.9. At 64 in flight, 32 KiB reaches 38.2.
- **WAN uploads level off at about 35 MiB/s on both clients.** This matches OpenSSH's server-side channel window (2 MiB per 50 ms = 40 MiB/s at most), although that limit was inferred, not measured directly. More than about 4 MiB of pending writes gains nothing against this server.
- **Per-call writes cost one round trip per call.** That gives 1 MiB per 50 ms = 20 MiB/s at most, and 17.2 was measured.

## 3. The Windows `localhost` relay stalls single small writes

The first matrix ran through `localhost:2222`, the `wslrelay` port forward. A single outstanding write of 16–64 KiB took about 44 ms. Connecting to the WSL VM's IP removes the stall, so it belongs to the relay and not to russh. Linux delayed ACK was ruled out with `quickack`. `sftp.exe` also stalls through the relay, at other sizes: 4 KiB with `-R 1` gave 0.17 MiB/s, but 32 KiB gave 32 MiB/s.

Upload, one write in flight, MiB/s:

| Chunk | via `localhost` | via VM IP |
|---|---|---|
| 4 KiB | 5.0 | 7.9 |
| 16 KiB | 0.4 | 25.5 |
| 32 KiB | 0.7 | 41.6 |
| 64 KiB | 1.4 | 56.8 |
| 128 KiB | 56.4 | 78.0 |

**Consequence:** performance tests must use the VM's IP (`wsl hostname -I`). All numbers here do, except the `*-relay.csv` files, which are kept for comparison. Small-file budgets such as NFR-PERF-07 would be badly wrong through the relay. Whether a real remote server shows any such stall was not tested.

## 4. Settings from the data

| Setting | Value | Evidence |
|---|---|---|
| Chunk size | The server's maximum from `limits@openssh.com`, else 32 KiB | §2 |
| Limit in flight per handle | **64 chunks and at most 4 MiB** (16 × largest chunk) | Same rate as more (§2), steadier (§5), far less head-of-line delay (§7) |
| SSH channel window | 16 MiB | WAN download, largest × 64: 2 MiB gives 19.8 MiB/s, 16 MiB gives 43.4, 64 MiB gives 43.1 |
| SSH max packet, channel buffer | 32 KiB, 100 (russh defaults) | 64 KiB packets 45.5 MiB/s and a 1000-message buffer 43.5, no clear gain over 43.4 |
| TCP `nodelay` | On | No effect in the probes through the relay; kept because the requests are latency-sensitive. Not separately measured over the VM path |
| Cipher preference | **AES-256-GCM first**, then chacha20-poly1305 | §5 |
| Crypto backend | `ring` | §8 |

## 5. Ciphers, and one unexplained slow mode

LAN, largest chunk, MiB/s (`results/tune*.csv`). Downloads: median of five, range in brackets. The upload rows come from a single run.

| Client | Download × 16 | Download × 32 | Download × 64 | Upload × 64 |
|---|---|---|---|---|
| russh chacha20 | 192.4 (182–207) | 185.9 (150–192) | 180.9 (122–192) | 217.4 |
| russh AES-256-GCM | 230.7 (222–260) | 242.7 (170–262) | 133.7 (114–138) | 293.7 |
| `sftp.exe` chacha20 (default) | | | 108.2 | 84.6 |
| `sftp.exe` AES-256-GCM | | | 217.2 | 189.7 |

- The `sftp.exe` rows use its own default request settings; they sit in the × 64 column only for layout.
- AES-GCM is 20–35% faster in russh on this CPU (at 16 and 32 in flight, and on upload), and about twice as fast in `sftp.exe`. Against a user who selects AES-GCM in OpenSSH, russh needs AES-GCM too in order to stay above 90%: 230.7 / 217.2 = 106% at 16 in flight.
- **Unexplained: downloads at 64 × largest chunk sometimes run at about half speed.** It was intermittent (for example 114.7, 197.4 and 190.1 in the final run), and AES-256-GCM hit it in all five runs of one batch but in only one of three later. A similar drop was occasionally seen at 32 in flight (144–150). The 16 MiB window was suspected, since 64 × 261,120 is 16.7 MiB, but a 64 MiB window still gave one slow run in three (293, 139, 313). The cause was not found. None of the 13 whole-file runs at 16 in flight showed it, which is one reason for the limit in §4; that is an observation, not a proof.

## 6. Listing and stat

| Operation | LAN | WAN |
|---|---|---|
| Connect, auth, SFTP init (p50) | 93 ms | 697 ms |
| List 1,000 entries: `opendir`, `readdir`…, `close` (p50 / p95) | 24.8 / 28.4 ms | 732 / 746 ms |
| `stat`, one at a time (p50 / p95) | 0.45 / 0.57 ms | 50.7 / 51.3 ms |
| `stat` 1,000 files, 8 in flight | 78 ms | 6.4 s |
| `stat` 1,000 files, 64 in flight | 18 ms | 0.87 s |

The first listing is well inside NFR-PERF-08's LAN budget of 300 ms. These are SFTP-layer numbers, without the VFS or caches.

## 7. Head-of-line blocking

Latency of `stat` while a bulk transfer runs, largest chunk, ms (p50 / p95). Idle: LAN about 0.5, WAN about 51.

| Transfer, in flight | LAN, same channel | LAN, second channel | WAN, same channel | WAN, second channel | Bulk MiB/s (LAN / WAN) |
|---|---|---|---|---|---|
| Download × 64 | 0.9 / 314 | 0.6 / 14 | 307 / 348 | 53 / 70 | 117 / 37 |
| Download × 32 | 0.8 / 51 | 0.6 / 1.5 | 152 / 184 | 52 / 59 | 183 / 45 |
| **Download × 16** | **0.8 / 20** | 0.6 / 1.2 | **68 / 93** | 52 / 76 | 206 / 50 |
| Upload × 64 | 0.8 / 69 | 0.6 / 3.0 | 422 / 436 | 52 / 53 | 220 / 35 |
| Upload × 32 | 0.9 / 35 | 0.6 / 2.7 | 208 / 210 | 52 / 53 | 218 / 35 |
| **Upload × 16** | **0.8 / 18** | 0.6 / 2.7 | **103 / 107** | 52 / 53 | 222 / 35 |

- Metadata does queue behind data, and the delay grows with bytes in flight. Single stalls were larger: up to 3.2 s (WAN, download × 64) and 1.1 s (WAN, download × 32), but 100 ms at download × 16 and 225 ms at upload × 16.
- A second SFTP channel on the same connection removes nearly all of it.
- **Decision: keep one channel and limit bytes in flight to 4 MiB.** That is enough to bring the delay down to one extra round trip or less on WAN, at no cost in rate. A second channel stays an option for M2-08 if Explorer browsing during a copy feels slow, as this measures `stat` and not browsing.

## 8. Maturity of the crates

No crash, hang, data error or protocol failure in any run. Points for M1-10:

- **`russh`'s default crypto backend, `aws-lc-rs`, does not build here:** `aws-lc-sys` needs NASM on Windows. `ring` (`default-features = false, features = ["ring", "rsa"]`) builds with the existing MSVC tools. ANALYSIS.md's "no C toolchain at build time" was wrong and is corrected. `aws-lc-rs` was not benchmarked. Its licence, and `ring`'s, must pass the M1-01 `cargo deny` allow-list.
- **`RawSftpSession` defaults:** a 10 s request timeout (set it from `op_timeout_secs` through `Config::request_timeout_secs`) and a 256 KiB packet limit, which fits OpenSSH.
- **Cost per request:** `write` takes a `Vec<u8>` and `read`/`write` take the handle as `String`, so each chunk costs a copy and an allocation. This did not limit the rates above.
- **Dead-session errors are strings:** `UnexpectedBehavior("sender dropped")` or `"session closed"`, and which one you get varies. `fsm-sftp` must map every non-`Status`, non-`Timeout` error to `ConnectionLost` rather than matching text.
- **`send_ping` ignores a dropped reply channel** (from the source: it discards the receive result and returns `Ok`), so it must not be the only liveness check. Use `is_closed()` and the `disconnected` handler.
- **Short reads** must be handled: none occurred with OpenSSH, but the benchmark's retry path was therefore not exercised.
- Whether a rekey happened during these transfers was not checked. russh's rekey path needs its own test in M1.

## 9. Keep-alive and connection loss

Spec defaults: `keepalive_secs` = 15, three misses, request timeout 15 s (`op_timeout_secs`). Times are from the moment the trigger finished. The abrupt-close and silent-loss cases ran twice, with matching results except where noted; the hung-server cases ran once.

| Failure | Idle: `disconnected()` | With 8 reads in flight: first read fails / `disconnected()` | Detected by |
|---|---|---|---|
| Server closes abruptly (`SIGKILL`) | 0.00 s | 0.00 s / 0.00 s | EOF or reset |
| Link silently lost (netem 100% loss) | 32.7 s | 15.0 s / 34.1 s | Windows TCP gives up on unacknowledged data (inferred from the `ConnectionReset`, 10054, and the timing) |
| Server hung, TCP up (`SIGSTOP`) | 58.4 s | 15.0 s / 60.0 s | russh `KeepaliveTimeout` |

(An earlier run of silent loss with reads in flight disconnected at 19.2 s rather than 34.1 s. The read failure at 15.0 s was the same.)

- **Abrupt close is detected at once.** New requests then fail immediately.
- **Client keep-alive works:** `keepalive@openssh.com` round trip 0.34 ms (p50).
- **Server keep-alive is answered:** with `ClientAliveInterval 1`, `ClientAliveCountMax 2`, an idle russh session stayed connected for 15 s and then worked. As a control, sshd dropped the session (`Timeout, client not responding`) once it could not hear the replies.
- **FR-SSH-12's limit of 60 s:** silent loss (the cable-pulled case in its acceptance test) takes about 33 s, inside the limit. The hung server takes 58–60 s, right on it, because russh gives up at the fourth interval rather than the third (15 × 4 = 60). In practice the 15 s operation timeout and its probe (spec §5.6) will notice first when requests are pending. For an idle mount, M1 should set `keepalive_max` to 2, which should give about 45 s and "three unanswered" in effect; that value was not tested.
- Requests in flight fail at their own timeout, so nothing waits for keep-alive (NFR-REL-03).

## 10. Go / no-go

**Go on `russh` + `russh-sftp`.** Criterion: at least 90% of OpenSSH on LAN and WAN (NFR-PERF-06), met at the SFTP layer by reads and by free-running writes. The fallback SFTP client is not needed.

**OD-4: bounded write-behind is on.** Per-call writes reach 179% on LAN but only 49% on WAN, below the 90% rule. A free-running pipeline with at most about 4 MiB unacknowledged reaches 102% on WAN (three whole 1 GiB uploads, SHA-256 verified). This pipeline stands in for write-behind: **the real design (1 MiB calls returning before acknowledgement, error latched for the next call) was not built here**, and comes in M2-08 as the plan already says (ADR-0002).

The bound is **4 MiB, not the spec's earlier 8 MiB**. Both reach the same rate (35.6 and 35.4 MiB/s), 4 MiB halves the delay for metadata (103 against 208 ms on WAN), it means less data is at risk if the connection fails, and OpenSSH's 2 MiB server window means more cannot help against this server.

Carried into the spec and plan:
- §5.6: chunk and in-flight limits, write-behind decided with the 4 MiB bound;
- §5.7: AES-GCM first, `ring`, one channel kept, window confirmed;
- OD-4 decided; M1-10 notes from §8 and §9; test-server README notes the relay.
