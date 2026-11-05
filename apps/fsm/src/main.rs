// SPDX-License-Identifier: GPL-3.0-or-later

//! FS Manager: the GUI binary. UI and wiring come in M1-16 to M1-18.

#![windows_subsystem = "windows"]
#![forbid(unsafe_code)]
// build.rs delay-loads WinFsp, and the linker points out that nothing here calls it yet. Remove this
// at M1-18.
#![allow(linker_messages)]

fn main() {}
