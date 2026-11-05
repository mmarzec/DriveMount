// SPDX-License-Identifier: GPL-3.0-or-later

//! What the binary is put together from, here so that tests and M1-18's `mount_one` example can use
//! it too. The window, the tray and `main` come in M1-16 to M1-18.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod backend;
