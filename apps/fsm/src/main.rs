// SPDX-License-Identifier: GPL-3.0-or-later

//! FS Manager: the GUI binary. The main window is in the library, and M1-17 and M1-18 bring the rest.

#![windows_subsystem = "windows"]
#![forbid(unsafe_code)]
// build.rs delay-loads WinFsp, and the linker points out that nothing here calls it yet. Remove this
// at M1-18.
#![allow(linker_messages)]

fn main() {}
