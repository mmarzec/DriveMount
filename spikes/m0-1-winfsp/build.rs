// SPDX-License-Identifier: GPL-3.0-or-later
fn main() {
    // Link args from winfsp-sys's own build script do not reach the final binary.
    winfsp::build::winfsp_link_delayload();
}
