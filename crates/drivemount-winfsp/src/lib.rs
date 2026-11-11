// SPDX-License-Identifier: GPL-3.0-or-later

//! Adapter from `RemoteFs` to a WinFsp drive (spec §5.8). Empty on other platforms.
//!
//! [`WinFspBackend`] is what the mount manager uses: it connects, shows the drive and tells Explorer.
//! [`Volume`] is the drive alone, over a file system that is already connected.

#![cfg(windows)]
// Implements WinFsp callbacks. clippy::undocumented_unsafe_blocks requires a SAFETY comment on each block.
#![allow(unsafe_code)]
#![deny(missing_docs)]

mod adapter;
mod attrs;
mod backend;
mod behind;
mod paths;
mod security;
mod status;
mod volume;

pub use backend::WinFspBackend;
pub use volume::{Volume, VolumeConfig};
