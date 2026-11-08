// SPDX-License-Identifier: GPL-3.0-or-later

//! The one security descriptor every file on the drive has (spec §5.6): full control for the user
//! who mounted it. The server decides what that user may really do.

use std::ffi::c_void;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetLengthSid, GetSecurityDescriptorLength, GetTokenInformation, IsValidSid,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

/// A self-relative descriptor for the current user, ready to hand to WinFsp.
pub(crate) fn current_user_descriptor() -> windows::core::Result<Vec<u8>> {
    descriptor_from_sddl(&current_user_sddl()?)
}

fn current_user_sddl() -> windows::core::Result<String> {
    let sid = current_user_sid()?;
    Ok(format!("O:{sid}G:{sid}D:P(A;;FA;;;{sid})"))
}

/// Memory that Windows allocated and the caller must free with `LocalFree`.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the API that allocated it for us, and nothing uses it after
        // this. LocalFree accepts null, and what it returns on failure cannot be acted on.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0))) };
    }
}

/// The token of this process, closed on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is not used after this.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn current_user_sid() -> windows::core::Result<String> {
    let mut handle = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing, and `handle` outlives
    // the call.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut handle) }?;
    let token = Token(handle);

    let mut needed = 0;
    // SAFETY: with no buffer and a size of 0, this only writes the size it needs.
    let sized = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &raw mut needed) };
    if let Err(error) = sized
        && error.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
    {
        return Err(error);
    }

    // In u64s for alignment: TOKEN_USER holds a pointer.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &raw mut needed,
        )
    }?;
    // SAFETY: the call above filled the buffer with a TOKEN_USER, and it is aligned for one.
    let sid: PSID = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_to_string(sid)
}

fn sid_to_string(sid: PSID) -> windows::core::Result<String> {
    // SAFETY: `sid` points into a live TOKEN_USER; these only read it.
    debug_assert!(unsafe { IsValidSid(sid).as_bool() && GetLengthSid(sid) > 0 });

    let mut text = PWSTR::null();
    // SAFETY: `sid` is valid, and `text` receives a string to free with LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }?;
    let _free = Local(text.0.cast());
    // SAFETY: on success `text` is a valid NUL-terminated string.
    unsafe { text.to_string() }.map_err(|_| windows::core::Error::from_win32())
}

fn descriptor_from_sddl(sddl: &str) -> windows::core::Result<Vec<u8>> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the string is NUL-terminated and outlives the call, and `descriptor` receives memory
    // to free with LocalFree.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }?;
    let _free = Local(descriptor.0);
    // SAFETY: on success `descriptor` is a valid self-relative descriptor of the length this reports.
    let bytes = unsafe {
        let len = GetSecurityDescriptorLength(descriptor) as usize;
        std::slice::from_raw_parts(descriptor.0.cast::<u8>(), len)
    };
    Ok(bytes.to_vec())
}

/// Copies the descriptor out for WinFsp and returns its size. With no buffer, WinFsp only wants the
/// size.
pub(crate) fn copy_descriptor(
    descriptor: &[u8],
    out: Option<&mut [c_void]>,
) -> winfsp::Result<u64> {
    if let Some(out) = out {
        if out.len() < descriptor.len() {
            return Err(windows::Win32::Foundation::STATUS_BUFFER_OVERFLOW.into());
        }
        // SAFETY: `out` holds at least `descriptor.len()` bytes, checked above, and is a buffer of
        // WinFsp's, which cannot overlap our Vec.
        unsafe {
            std::ptr::copy_nonoverlapping(
                descriptor.as_ptr(),
                out.as_mut_ptr().cast::<u8>(),
                descriptor.len(),
            );
        }
    }
    Ok(descriptor.len() as u64)
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::STATUS_BUFFER_OVERFLOW;
    use windows::Win32::Security::IsValidSecurityDescriptor;

    use super::*;

    #[test]
    fn the_descriptor_is_valid_and_names_this_user_alone() {
        let sddl = current_user_sddl().unwrap();
        assert!(sddl.starts_with("O:S-1-"), "{sddl}");
        assert!(!sddl.contains("WD"), "{sddl}");
        assert_eq!(sddl.matches("(A;;").count(), 1, "{sddl}");

        let mut descriptor = current_user_descriptor().unwrap();
        // SAFETY: the pointer is to a live buffer holding a descriptor, which this only reads.
        let valid = unsafe {
            IsValidSecurityDescriptor(PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast()))
        };
        assert!(valid.as_bool());
    }

    #[test]
    fn copying_reports_the_size_and_refuses_a_short_buffer() {
        let descriptor = [1u8, 2, 3, 4];
        assert_eq!(copy_descriptor(&descriptor, None).unwrap(), 4);

        let mut short = [0u8; 3];
        // SAFETY: c_void and u8 have the same size, and the length matches the array.
        let short = unsafe { std::slice::from_raw_parts_mut(short.as_mut_ptr().cast(), 3) };
        let error = copy_descriptor(&descriptor, Some(short)).unwrap_err();
        assert_eq!(error.to_ntstatus(), STATUS_BUFFER_OVERFLOW.0);

        let mut enough = [0u8; 5];
        // SAFETY: as above.
        let out = unsafe { std::slice::from_raw_parts_mut(enough.as_mut_ptr().cast(), 5) };
        assert_eq!(copy_descriptor(&descriptor, Some(out)).unwrap(), 4);
        assert_eq!(enough, [1, 2, 3, 4, 0]);
    }
}
