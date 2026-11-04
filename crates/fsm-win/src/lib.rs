// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows helpers. Empty on other platforms.

#![cfg(windows)]
// Calls Win32 APIs directly. clippy::undocumented_unsafe_blocks requires a SAFETY comment on each block.
#![allow(unsafe_code)]
