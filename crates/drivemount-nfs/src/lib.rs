// SPDX-License-Identifier: GPL-3.0-or-later

//! `RemoteFs` over NFSv3 (milestone M4). No Windows dependencies.

#![forbid(unsafe_code)]

mod fs;
mod handles;
mod pool;

pub use fs::{NfsFs, NfsOptions};
pub use handles::{CAPACITY, FileHandle, HandleCache, HandleError, Lookup, root_stale};
pub use pool::{Dialer, Identity};
