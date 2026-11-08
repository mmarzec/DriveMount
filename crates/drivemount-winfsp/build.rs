// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    // The WinFsp DLL is found at run time through the registry (spike M0-1).
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
