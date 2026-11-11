# ADR-0002: Bounded write-behind, per Windows handle

| | |
|---|---|
| Status | **Accepted** |
| Date | 2026-11-11 |
| Decided by | Owner: write-behind on, 4 MiB (OD-4, spike M0-2). AI agent: where it lives and how errors and ordering work (plan task M2-08); the owner reviews it at merge |
| Applies to | `drivemount-winfsp`: `behind.rs` and the adapter's `Write`, `Read`, `Flush`, `Cleanup`, `Close` and attribute changes |

## Context

Until M2-08 each WinFsp `Write` was acknowledged by the server before it returned, then asked `fstat` for the size Windows is told. On a 50 ms link that is at least two round trips per 1 MiB call, so Explorer copies to the drive ran at a quarter of `sftp put` (NFR-PERF-06 asks for ≥ 90%). Spike M0-2 showed that a pipeline with ~4 MiB unacknowledged reaches 102%, and OD-4 chose a bounded write-behind with that limit, with the first error latched for the next call.

## Decision

**Where.** In the adapter, per Windows handle (`OpenFile`), not as a `RemoteFs` layer. The adapter already owns each handle's remote writer, follows renames for open files, knows every other handle on the same path (to mark readers stale), and works out what Windows is told. A layer below would have had to do all of this again, and would need to forge `RemoteHandle`s for its background tasks.

**A `Write`:**
1. returns a failure kept from an earlier write, if there is one (once: see below);
2. takes the size it needs from what this handle last told Windows. It asks the server (`fstat`) only if something else on this drive changed the file since, or for an append, as another client may have added to the file;
3. waits for writes still on their way through other handles to the same path, and for any of its own it overlaps, since two writes in flight can arrive in either order;
4. waits for room: at most 4 MiB unacknowledged per handle (a larger call takes the whole 4 MiB);
5. sends the data from a task of the runtime and returns. Windows is told the new size; the times stay the server's last answer until the writes have arrived, rather than a guess from this machine's clock.

**Paging I/O does not go behind.** A `Write` with `ConstrainedIo` is the cache manager writing back or a mapped view being flushed. Windows waits for those (`FlushViewOfFile`, and `CloseHandle` after a mapped write), and they can come after `Cleanup`, which can then no longer wait for them. So such a write waits for its own arrival, as before. `fsx` found this: a flushed view was not yet on the server when the handle closed. A program's own `WriteFile` calls arrive as ordinary writes, which is what a copy makes.

**What waits for writes on their way:**
- `Read`, on any handle of the path, so a read sees every write before it.
- Size and attribute changes (`SetFileSize`, `SetBasicInfo`, `Overwrite`), so a size set afterwards is not undone by a write that lands later.
- `Flush`, then the fsync.
- `Cleanup`, so nothing is unacknowledged once the program's `CloseHandle` returns, and whoever opens the file next sees it. `Close` and the unmount's closing of files left open wait as well, before the remote handle goes.

**Errors.** The first failure is kept and returned once, by the next `Write` or `Flush` of that handle, then cleared, as Linux reports a failed write-back once per file. A program that starts over after hearing it can succeed. One that never calls again cannot hear it. `Cleanup` cannot fail, so it writes the failure to the log at Warn. Each background write is still a remote call under `op_timeout_secs` (NFR-REL-03), and a timeout counts for the "suspect" signal as before.

## What this gives up

- **An error comes one call late.** Most programs check `FlushFileBuffers` or the next `WriteFile`. One that checks neither (or only `CloseHandle`) loses the error to the log. Windows' own write-back cache has the same property: its "Delayed Write Failed" message has no program to go to either.
- **Other handles see sizes up to 4 MiB behind** while a file is being written: an `Open` or a listing from another program asks the server, which has not got everything yet. Reads wait, so data is never stale; only the size and time shown can be, and only until the writing handle is closed.
- **Closing waits for at most 4 MiB to arrive.** On a slow link `CloseHandle` takes longer, but that time was spent in the writes before.

## Alternatives considered

- **A `RemoteFs` layer (`WriteBehindFs`).** It would also work for other front ends. But it would have to keep paths through renames again, could not see what Windows was told, and its tasks would need handles that the trait does not let it copy. If a second front end appears, the per-handle state in `behind.rs` can move down then.
- **Waiting for writes before every lookup of the path** (`Open`, `GetSecurityByName`), not only reads. Sizes would never lag, but a program, or Explorer, looking at a file while it is copied would stall the copy each time. Rejected for the copy rate, which is the reason for write-behind.
- **Keeping a failure for the rest of the handle's life.** Safer against programs that ignore the first error, but a program that retries could then never succeed. Linux's behaviour was preferred.
- **No write-behind, more parallel `Write` calls instead.** Windows sends a copy's `Write`s one after another, so the drive cannot get more in flight without returning early.
