// SPDX-License-Identifier: GPL-3.0-or-later

//! `RemoteFs` over NFSv3 (milestone M4). No Windows dependencies.

#![forbid(unsafe_code)]

mod connector;
mod fs;
mod handles;
mod pool;

pub use connector::{NfsConnector, NfsProfile, PORTMAPPER_PORT, RESERVED_PORTS, connect_reserved};
pub use fs::{NfsFs, NfsOptions};
pub use handles::{CAPACITY, FileHandle, HandleCache, HandleError, Lookup, root_stale};
pub use pool::{Dialer, Identity};
