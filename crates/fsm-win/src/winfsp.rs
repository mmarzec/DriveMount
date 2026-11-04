// SPDX-License-Identifier: GPL-3.0-or-later

//! Whether a usable WinFsp is installed (PLAT-02, FR-APP-06).

use std::ffi::{OsString, c_void};
use std::fmt;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{PCWSTR, w};

use crate::wide;

/// The file version of a WinFsp DLL, whose first two parts are the release, as in `2.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WinFspVersion {
    /// The major version.
    pub major: u16,
    /// The minor version.
    pub minor: u16,
    /// The build number.
    pub build: u16,
}

impl WinFspVersion {
    /// The oldest version FS Manager works with (PLAT-02).
    pub const MINIMUM: Self = Self {
        major: 2,
        minor: 1,
        build: 0,
    };

    /// From `VS_FIXEDFILEINFO`'s `dwFileVersionMS` and `dwFileVersionLS`.
    #[must_use]
    pub const fn from_file_version(most: u32, least: u32) -> Self {
        Self {
            major: (most >> 16) as u16,
            minor: most as u16,
            build: (least >> 16) as u16,
        }
    }
}

impl fmt::Display for WinFspVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

/// What [`detect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinFspStatus {
    /// WinFsp is not installed, or its DLL is gone or has no version.
    Missing,
    /// WinFsp is older than [`WinFspVersion::MINIMUM`].
    TooOld(WinFspVersion),
    /// WinFsp can be used.
    Ready(WinFspVersion),
}

impl WinFspStatus {
    /// Whether mounting through WinFsp can work.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// Looks for WinFsp through its registry key and the version of its DLL.
#[must_use]
pub fn detect() -> WinFspStatus {
    status_of(install_dir().as_deref())
}

fn status_of(install_dir: Option<&Path>) -> WinFspStatus {
    classify(install_dir.and_then(|dir| file_version(&dir.join("bin").join(DLL_NAME))))
}

fn classify(version: Option<WinFspVersion>) -> WinFspStatus {
    match version {
        None => WinFspStatus::Missing,
        Some(version) if version < WinFspVersion::MINIMUM => WinFspStatus::TooOld(version),
        Some(version) => WinFspStatus::Ready(version),
    }
}

/// The DLL this build of FS Manager would load.
const DLL_NAME: &str = if cfg!(target_arch = "aarch64") {
    "winfsp-a64.dll"
} else if cfg!(target_arch = "x86") {
    "winfsp-x86.dll"
} else {
    "winfsp-x64.dll"
};

/// `InstallDir` from WinFsp's key, which its 32-bit installer writes to the 32-bit view.
fn install_dir() -> Option<PathBuf> {
    let read = |data: Option<*mut c_void>, size: &mut u32| {
        // SAFETY: the strings are constants, and `data`, where given, has room for `size` bytes.
        unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\WOW6432Node\WinFsp"),
                w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                data,
                Some(ptr::from_mut(size)),
            )
        }
    };

    let mut size = 0;
    if read(None, &mut size) != ERROR_SUCCESS {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    if read(Some(buffer.as_mut_ptr().cast()), &mut size) != ERROR_SUCCESS {
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0)?;
    Some(OsString::from_wide(&buffer[..end]).into())
}

/// The file version in a file's version resource. `None` if it has none.
fn file_version(path: &Path) -> Option<WinFspVersion> {
    const SIGNATURE: u32 = 0xFEEF_04BD;

    let name = wide(path);
    // SAFETY: `name` is nul-terminated and outlives the call.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(name.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: `block` has room for `size` bytes, and `name` is as above.
    unsafe { GetFileVersionInfoW(PCWSTR(name.as_ptr()), None, size, block.as_mut_ptr().cast()) }
        .ok()?;

    let mut info = ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: `block` holds what `GetFileVersionInfoW` wrote, and the outs can be written to.
    let found = unsafe {
        VerQueryValueW(
            block.as_ptr().cast(),
            w!(r"\"),
            &raw mut info,
            &raw mut length,
        )
    };
    if !found.as_bool() || info.is_null() || (length as usize) < mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: `info` points into `block`, with at least this many bytes as checked just above.
    let fixed = unsafe { info.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    (fixed.dwSignature == SIGNATURE)
        .then(|| WinFspVersion::from_file_version(fixed.dwFileVersionMS, fixed.dwFileVersionLS))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const fn version(major: u16, minor: u16, build: u16) -> WinFspVersion {
        WinFspVersion {
            major,
            minor,
            build,
        }
    }

    #[test]
    fn file_version_numbers_are_taken_apart() {
        // What the installed 2.2 beta's DLL has.
        let parsed = WinFspVersion::from_file_version(0x0002_0002, 26286 << 16);
        assert_eq!(parsed, version(2, 2, 26286));
        assert_eq!(parsed.to_string(), "2.2.26286");

        // Each part is in its own bits, and the fourth part plays no role.
        let parsed = WinFspVersion::from_file_version(0x0003_0007, 0x0009_0005);
        assert_eq!(parsed, version(3, 7, 9));
    }

    #[test]
    fn versions_are_held_to_the_minimum_of_2_1() {
        for old in [
            version(1, 12, 22339),
            version(1, 99, 0),
            version(2, 0, 23075),
        ] {
            assert_eq!(classify(Some(old)), WinFspStatus::TooOld(old), "{old}");
            assert!(!WinFspStatus::TooOld(old).is_ready());
        }
        for good in [
            version(2, 1, 0),
            version(2, 1, 25156),
            version(2, 2, 26286),
            version(3, 0, 0),
        ] {
            assert_eq!(classify(Some(good)), WinFspStatus::Ready(good), "{good}");
            assert!(WinFspStatus::Ready(good).is_ready());
        }
    }

    #[test]
    fn no_version_is_missing() {
        assert_eq!(classify(None), WinFspStatus::Missing);
        assert!(!WinFspStatus::Missing.is_ready());
    }

    #[test]
    fn no_install_directory_is_missing() {
        assert_eq!(status_of(None), WinFspStatus::Missing);
    }

    #[test]
    fn an_install_directory_without_the_dll_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(status_of(Some(directory.path())), WinFspStatus::Missing);
    }

    #[test]
    fn a_dll_with_no_version_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("bin")).unwrap();
        fs::write(directory.path().join("bin").join(DLL_NAME), b"not a DLL").unwrap();
        assert_eq!(status_of(Some(directory.path())), WinFspStatus::Missing);
    }

    #[test]
    fn a_real_version_resource_is_read() {
        let kernel32 = std::env::var("SystemRoot").unwrap() + r"\System32\kernel32.dll";
        let found = file_version(Path::new(&kernel32)).unwrap();
        assert!(found.major >= 6, "{found}");
    }

    /// Against the WinFsp on this machine, in the registry and on disk.
    #[test]
    #[ignore = "needs WinFsp 2.1 or later"]
    fn the_installed_winfsp_is_ready() {
        let directory = install_dir().unwrap();
        assert!(directory.join("bin").join(DLL_NAME).is_file());

        let status = detect();
        let WinFspStatus::Ready(found) = status else {
            panic!("{status:?}");
        };
        assert!(found >= WinFspVersion::MINIMUM);
        println!("WinFsp {found} in {}", directory.display());
    }
}
