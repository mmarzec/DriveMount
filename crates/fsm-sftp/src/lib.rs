// SPDX-License-Identifier: GPL-3.0-or-later

//! `RemoteFs` over SFTP, and its connector. No Windows dependencies.

#![forbid(unsafe_code)]

pub mod connector;
pub mod fs;
pub mod known_hosts;
