// SPDX-License-Identifier: GPL-3.0-or-later

//! Profiles, configuration and the mount manager. Depends only on `drivemount-vfs`; no Windows dependencies.

#![forbid(unsafe_code)]

pub mod config;
pub mod manager;
pub mod profile;
