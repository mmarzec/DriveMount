// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows helpers. Empty on other platforms.
//!
//! The tests marked `#[ignore]` need a signed-in desktop or an installed WinFsp, and some change
//! this user's settings for a moment. Each says what it needs and what it touches.

#![cfg(windows)]
// Calls Win32 APIs directly. clippy::undocumented_unsafe_blocks requires a SAFETY comment on each block.
#![allow(unsafe_code)]
#![deny(missing_docs)]

pub mod credentials;
pub mod drives;
pub mod events;
pub mod shell;
pub mod winfsp;

#[cfg(test)]
mod test_support;

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

/// `text` as a nul-terminated UTF-16 string.
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

/// An `io::Error` carrying the Win32 code, so it has the right kind and message. The conversion in
/// the `windows` crate keeps the HRESULT, which `io::ErrorKind` does not recognise.
fn os_error(error: &windows::core::Error) -> io::Error {
    const WIN32_FACILITY: u32 = 0x8007_0000;

    let code = error.code().0.cast_unsigned();
    if code & 0xFFFF_0000 == WIN32_FACILITY {
        io::Error::from_raw_os_error((code & 0xFFFF).cast_signed())
    } else {
        io::Error::from_raw_os_error(code.cast_signed())
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{E_FAIL, ERROR_ACCESS_DENIED};
    use windows::core::Error;

    use super::*;

    #[test]
    fn strings_end_in_one_nul() {
        assert_eq!(wide("Z:"), [u16::from(b'Z'), u16::from(b':'), 0]);
        assert_eq!(wide(""), [0]);
    }

    #[test]
    fn win32_codes_keep_their_kind() {
        let error = os_error(&Error::from_hresult(ERROR_ACCESS_DENIED.to_hresult()));
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn other_codes_pass_through() {
        let error = os_error(&Error::from_hresult(E_FAIL));
        assert_eq!(error.raw_os_error(), Some(E_FAIL.0));
    }
}
