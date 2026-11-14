// SPDX-License-Identifier: GPL-3.0-or-later

//! Native Windows SMB mappings (milestone M3, spec §5.9). Empty on other platforms.
//!
//! [`SmbBackend`] maps a share on a drive letter through the WNet API and follows its health; Windows'
//! own SMB client does the reconnecting. [`other_drives`] lists the network drives DriveMount did not
//! make (FR-SMB-09).
//!
//! The Win32 calls themselves are in `drivemount_win::wnet`, as this crate has no `unsafe`.
//!
//! The tests marked `#[ignore]` need the Samba test server (`tests\docker\servers.ps1 up`) and map
//! drives in this logon session for a moment; `tests/samba.rs` says how to run them.

#![cfg(windows)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod backend;
mod errors;
mod other;

pub use backend::{SmbAuth, SmbBackend, SmbTarget};
pub use other::{DriveStatus, OtherDrive, other_drives};
