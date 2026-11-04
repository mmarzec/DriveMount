// SPDX-License-Identifier: GPL-3.0-or-later

//! Adapter from `RemoteFs` to a WinFsp drive. Empty on other platforms.

#![cfg(windows)]
// Implements WinFsp callbacks. clippy::undocumented_unsafe_blocks requires a SAFETY comment on each block.
#![allow(unsafe_code)]
