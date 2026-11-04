# ADR-0001: Async functions in `dyn` traits: `async-trait`

| | |
|---|---|
| Status | **Accepted** |
| Date | 2026-11-04 |
| Decided by | AI agent, as plan task M1-02 asks; the owner reviews it at merge |
| Applies to | `RemoteFs`, `Connector`, `MountBackend`, `Prompter` in `fsm-vfs` |

## Context

The four core traits have async methods and are used as trait objects: a `Connector` returns `Arc<dyn RemoteFs>`, `Prompter` is passed as `&dyn Prompter`, and the app's factory returns `Box<dyn MountBackend>`. The futures must be `Send`, because they run on Tokio's worker threads and are polled from WinFsp dispatcher threads through `block_on`.

Native `async fn` in traits does not work here. On the pinned Rust 1.99.1, `Arc<dyn Trait>` with such a method fails with E0038, "the trait is not dyn compatible". So each call has to return a boxed future, written either by hand or by a macro.

## Options

| | `async-trait` macro | Hand-boxed `BoxFuture` |
|---|---|---|
| Trait and impl code | Reads as `async fn`, plus one attribute | Explicit lifetimes in every signature, and `Box::pin(async move { … })` in every body |
| Code to write | `RemoteFs` alone has 16 methods and at least 5 implementors (`MemFs`, `CachedFs`, `DeadlineFs`, `SftpFs`, `NfsFs`), plus test mocks | The same, with the extra lines above in each |
| Cost per call | 95 ns | 95 ns: both make one heap allocation |
| Dependencies | One new crate (MIT/Apache-2.0), not used by `russh` or the others. It needs only `syn`, `quote` and `proc-macro2`, which `thiserror` and `tokio`'s macros already bring | None |
| Compile errors | Sometimes point at generated code | Direct |
| `Send` | Required by default, so it cannot be forgotten | Written in each signature or type alias |

### Is boxing fast enough?

Measured with a release build on the development machine, 50 million calls of a method that does nothing and is awaited through `Arc<dyn …>`:

| Dispatch | Time per call |
|---|---|
| `async-trait`, `dyn` | 95.0 ns |
| Hand-boxed, `dyn` | 95.2 ns |
| Native `async fn`, static generic (not usable behind `dyn`) | 1.6 ns |

A `stat` over SFTP takes 450,000 ns on a LAN and 50,700,000 ns on the WAN profile (spike M0-2, p50). One box is therefore about 0.02% of the fastest remote call. A call that passes through `CachedFs` and `DeadlineFs` boxes up to three times, about 0.3 µs. A cache hit does pay that without a network round trip, but it is still small next to the kernel round trip WinFsp makes for each callback (not measured here).

## Decision

Use **`async-trait`** on all four traits. The cost is the same as boxing by hand, so the choice is about code, and the macro removes the most repetition from the traits that are implemented most often. `fsm-vfs` re-exports the macro, so implementors use the same version.

## Consequences

- Every implementor writes `#[async_trait]` on its `impl` block.
- `CachedFs<F>` and `DeadlineFs<F>` stay generic over `F: RemoteFs`, but their calls into `F` still box, because the trait's methods return boxed futures.
- If a future Rust makes `async fn` work behind `dyn` with `Send` futures, removing the attribute is a mechanical change.
- If M2-08 profiling shows boxing matters on a hot path, the fix is a specific fast path, such as a non-async cache lookup in the WinFsp adapter, not a change to these traits.
