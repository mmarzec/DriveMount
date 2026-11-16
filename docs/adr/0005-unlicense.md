# ADR-0005: Allow the Unlicense

| | |
|---|---|
| Status | **Accepted** |
| Date | 2026-11-16 |
| Decided by | Project owner, after spike M4-01 |
| Applies to | ADR-0003's licence allow-list, `deny.toml`, `about.toml` |

## Context

Spike M4-01 chose `nfs3_client` for the NFSv3 backend (`docs/spikes/m4-01-nfs.md`). It and the crates it brings in, `nfs3_types` and `nfs3_macros`, are under the **Unlicense** (SPDX `Unlicense`). ADR-0003's allow-list does not include it, so `cargo deny check` and `cargo about` fail as soon as `drivemount-nfs` depends on them. ADR-0003 says a change to the list needs a new ADR.

## The licence

The Unlicense dedicates a work to the public domain. Where that is not legally possible, a fallback grants everyone the right to do anything with it, with no conditions, and disclaims warranty. It is OSI-approved, and the FSF lists it as a free licence compatible with the GPL. It belongs with entries already on the list: `CC0-1.0`, `0BSD` and `MIT-0`.

Some lawyers doubt that a public-domain dedication holds in every country. Then the fallback grant applies, which is still more permissive than MIT. For a GPLv3 program that is no risk.

## Options

| | Allow `Unlicense` | Keep the list as it is |
|---|---|---|
| NFS backend | `nfs3_client`, as the spike found | Our own ONC-RPC client and XDR types (about 1,900 lines, spike report §7) |
| ADR-0003's aim (nothing proprietary, all GPLv3-compatible) | Kept | Kept |

## Decision

Add `Unlicense` to the allow-list in ADR-0003, `deny.toml` and `about.toml`. The crates are listed in `THIRD-PARTY-LICENSES.html` like any other, although the Unlicense does not require it.
