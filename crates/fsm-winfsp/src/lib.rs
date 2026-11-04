// SPDX-License-Identifier: GPL-3.0-or-later

//! Adapter from `RemoteFs` to a WinFsp drive (spec §5.8). Empty on other platforms.
//!
//! So far this is the read path: a drive can be listed and read, and everything that would change it
//! is refused as write-protected. [`WinFspBackend`] starts and stops the drive; `MountBackend` comes
//! with the mount lifecycle (M1-12c).

#![cfg(windows)]
// Implements WinFsp callbacks. clippy::undocumented_unsafe_blocks requires a SAFETY comment on each block.
#![allow(unsafe_code)]
#![deny(missing_docs)]

mod adapter;
mod attrs;
mod backend;
mod paths;
mod security;
mod status;

pub use backend::{VolumeConfig, WinFspBackend};
