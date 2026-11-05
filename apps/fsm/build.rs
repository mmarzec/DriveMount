// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    // The WinFsp DLL is found at run time through the registry (spike M0-1). Link arguments do not
    // cross packages, so fsm-winfsp's build.rs does not reach this one's binary and tests.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
