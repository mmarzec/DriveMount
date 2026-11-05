// SPDX-License-Identifier: GPL-3.0-or-later

//! What the binary is put together from, here so that tests and M1-18's `mount_one` example can use
//! it too. The tray and `main` come in M1-18 and M2-03.

// Not `forbid`: the Slint compiler's output in `ui` has unsafe blocks, and is the one thing allowed them.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod backend;
#[cfg(windows)]
pub mod ui;
