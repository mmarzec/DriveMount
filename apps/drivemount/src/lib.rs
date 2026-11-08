// SPDX-License-Identifier: GPL-3.0-or-later

//! What the binary is put together from, here so that tests and `examples/` can use it too. The tray
//! comes in M2-03.

// Not `forbid`: the Slint compiler's output in `ui` has unsafe blocks, and is the one thing allowed them.
#![deny(unsafe_code)]
#![deny(missing_docs)]

#[cfg(windows)]
pub mod app;
pub mod backend;
#[cfg(windows)]
pub mod logging;
#[cfg(windows)]
pub mod ui;
