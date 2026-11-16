// SPDX-License-Identifier: GPL-3.0-or-later

//! `RemoteFs` over NFSv3 (milestone M4). No Windows dependencies.

#![forbid(unsafe_code)]

mod handles;

pub use handles::{CAPACITY, FileHandle, HandleCache, HandleError, Lookup, root_stale};
