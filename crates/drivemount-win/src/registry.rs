// SPDX-License-Identifier: GPL-3.0-or-later

//! String values under `HKEY_CURRENT_USER`, for the toast registration and the Run value.

use std::io;
use std::ptr;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::{os_error, wide};

fn check(result: WIN32_ERROR) -> io::Result<()> {
    result.ok().map_err(|error| os_error(&error))
}

/// Sets a string value of an `HKCU` key, made if it is not there. `None` is the key's default value.
pub(crate) fn set_string(key: &str, name: Option<&str>, value: &str) -> io::Result<()> {
    let key_name = wide(key);
    let mut opened = HKEY::default();
    // SAFETY: the name is nul-terminated, and `opened` outlives the call.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &raw mut opened,
            None,
        )
    })?;
    let name = name.map(wide);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
    // SAFETY: `opened` was just opened to set values, the name is nul-terminated or null, and the
    // data is a nul-terminated UTF-16 string for the type given.
    let set = check(unsafe { RegSetValueExW(opened, name, None, REG_SZ, Some(&data)) });
    // SAFETY: `opened` is open and is not used after this. Nothing can be done if closing fails.
    let _ = unsafe { RegCloseKey(opened) };
    set
}

/// A string value of an `HKCU` key. `None` if the key or the value is not there.
pub(crate) fn get_string(key: &str, name: &str) -> io::Result<Option<String>> {
    let key = wide(key);
    let name = wide(name);
    let read = |data: Option<*mut core::ffi::c_void>, size: &mut u32| {
        // SAFETY: the strings are nul-terminated, and `data`, where given, has room for `size` bytes.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                data,
                Some(ptr::from_mut(size)),
            )
        }
    };
    let mut size = 0;
    let first = read(None, &mut size);
    if first == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(first)?;
    // Bytes, rounded up to whole units. It may have grown since, which the second call reports.
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let second = read(Some(buffer.as_mut_ptr().cast()), &mut size);
    if second == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(second)?;
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..end])))
}

/// Deletes one value of an `HKCU` key. Having nothing to delete is fine.
pub(crate) fn delete_value(key: &str, name: &str) -> io::Result<()> {
    let key = wide(key);
    let name = wide(name);
    // SAFETY: both strings are nul-terminated.
    let result = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(result)
}

/// Deletes an `HKCU` key and all under it. Having nothing to delete is fine.
pub(crate) fn delete_tree(key: &str) -> io::Result<()> {
    let key_name = wide(key);
    // SAFETY: the name is nul-terminated.
    let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key_name.as_ptr())) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(result)
}
