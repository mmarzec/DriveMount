// SPDX-License-Identifier: GPL-3.0-or-later

//! What the binary is put together from, here so that tests and `examples/` can use it too.

// Not `forbid`: the Slint compiler's output in `ui` has unsafe blocks, and is the one thing allowed them.
#![deny(unsafe_code)]
#![deny(missing_docs)]

#[cfg(windows)]
pub mod app;
pub mod backend;
#[cfg(windows)]
pub mod crash;
#[cfg(windows)]
pub mod logging;
#[cfg(windows)]
pub mod ui;
#[cfg(windows)]
pub mod uninstall;
