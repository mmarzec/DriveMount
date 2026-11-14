// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows network drives through MPR and the Net API: mapping and removing SMB drives (FR-SMB-03,
//! FR-SMB-04), the free-space call the health check makes (FR-SMB-05), and the listings of every
//! mapped drive (FR-SMB-09). Spec §5.9 and spike M3-01 say how each behaves.
//!
//! Every call here blocks, some for half a minute or more (spike M3-01 §3, §6), so none belongs on
//! an async worker or the UI thread, except the listings, which only read what Windows already knows
//! and take 2 ms or less.

use std::fmt;
use std::io;

use drivemount_vfs::DriveLetter;
use windows::Win32::Foundation::{
    ERROR_EXTENDED_ERROR, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HANDLE, NO_ERROR, WIN32_ERROR,
};
use windows::Win32::NetworkManagement::NetManagement::{NetApiBufferFree, NetUseEnum, USE_INFO_2};
use windows::Win32::NetworkManagement::WNet::{
    CONNECT_TEMPORARY, CONNECT_UPDATE_PROFILE, NET_CONNECT_FLAGS, NET_RESOURCE_SCOPE, NETRESOURCEW,
    RESOURCE_CONNECTED, RESOURCE_REMEMBERED, RESOURCETYPE_DISK, WNET_OPEN_ENUM_USAGE,
    WNetAddConnection2W, WNetCancelConnection2W, WNetCloseEnum, WNetEnumResourceW,
    WNetGetLastErrorW, WNetGetProviderNameW, WNetOpenEnumW,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::{PCWSTR, PWSTR};
use zeroize::Zeroize;

use crate::{os_error, wide};

/// The SMB client's network type (`winnetwk.h`). The `windows` crate has it only under
/// `Storage::InstallableFileSystems`.
const WNNC_NET_LANMAN: u32 = 0x0002_0000;
/// `NetUseEnum`: as much as there is, in one buffer.
const MAX_PREFERRED_LENGTH: u32 = u32::MAX;
/// `NetUseEnum` answers with a Net API status; 0 is success.
const NERR_SUCCESS: u32 = 0;

/// A failed WNet call: the Win32 code, which spec §5.9 maps, and Windows' text for the log. For
/// `ERROR_EXTENDED_ERROR` the code and text are the provider's own, from `WNetGetLastErrorW`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WNetError {
    /// The Win32 error code.
    pub code: u32,
    /// What Windows says about it.
    pub text: String,
}

impl WNetError {
    fn new(status: WIN32_ERROR) -> Self {
        if status == ERROR_EXTENDED_ERROR
            && let Some(extended) = last_extended_error()
        {
            return extended;
        }
        Self {
            code: status.0,
            text: system_text(status.0),
        }
    }
}

impl fmt::Display for WNetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error {}: {}", self.code, self.text)
    }
}

impl std::error::Error for WNetError {}

fn system_text(code: u32) -> String {
    io::Error::from_raw_os_error(code.cast_signed()).to_string()
}

fn last_extended_error() -> Option<WNetError> {
    let mut code = 0u32;
    let mut text = [0u16; 512];
    let mut provider = [0u16; 256];
    // SAFETY: the out pointer and both buffers are valid for the call, which takes their lengths
    // from the slices.
    let status = unsafe { WNetGetLastErrorW(&raw mut code, &mut text, &mut provider) };
    (status == NO_ERROR).then(|| {
        let text = String::from_utf16_lossy(until_nul(&text));
        let provider = String::from_utf16_lossy(until_nul(&provider));
        WNetError {
            code,
            text: format!("{} [{provider}]", text.trim()),
        }
    })
}

fn until_nul(buffer: &[u16]) -> &[u16] {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    &buffer[..end]
}

/// The SMB client's provider name, as `WNetAddConnection2W` must be given it (spec §5.9): without
/// it every provider is asked, and errors merge into 1203. It is localised on some systems, so it
/// is asked for rather than written down.
///
/// # Errors
/// The SMB client is not installed, or MPR failed.
pub fn smb_provider() -> Result<String, WNetError> {
    let mut name = [0u16; 256];
    let mut length = u32::try_from(name.len()).unwrap_or(u32::MAX);
    // SAFETY: `length` is the size of `name` in characters, and both outlive the call.
    let status =
        unsafe { WNetGetProviderNameW(WNNC_NET_LANMAN, PWSTR(name.as_mut_ptr()), &raw mut length) };
    if status != NO_ERROR {
        return Err(WNetError::new(status));
    }
    Ok(String::from_utf16_lossy(until_nul(&name)))
}

/// Who to map a share as.
#[derive(Clone, Copy)]
pub enum Credentials<'a> {
    /// This user name, `DOMAIN\user` or plain, and password.
    Explicit {
        /// The user name, as Windows will compare it (spike M3-01 §4).
        user: &'a str,
        /// The password. Its UTF-16 copy is zeroed after the call.
        password: &'a str,
    },
    /// Null user and password: the signed-in user, or whatever session Windows already has to the
    /// server (spike M3-01 §5).
    Current,
}

impl fmt::Debug for Credentials<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Explicit { user, .. } => f
                .debug_struct("Explicit")
                .field("user", user)
                .finish_non_exhaustive(),
            Self::Current => f.write_str("Current"),
        }
    }
}

/// Maps `unc` (`\\server\share`) on `letter` with `CONNECT_TEMPORARY`, so Windows never remembers
/// it (FR-SMB-03). The credentials go in the call: no process is started. Blocks, for about 27 s
/// against a host that does not answer.
///
/// With no letter it is a connection without a drive, as *Test connection* makes (FR-PROF-06), and
/// [`cancel_connection_to`] removes it.
///
/// # Errors
/// What Windows answered; spec §5.9 maps the codes.
pub fn add_connection(
    letter: Option<DriveLetter>,
    unc: &str,
    provider: &str,
    credentials: Credentials<'_>,
) -> Result<(), WNetError> {
    let mut local = letter.map(|letter| wide(letter.to_string()));
    let mut remote = wide(unc);
    let mut provider = wide(provider);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpLocalName: local
            .as_mut()
            .map_or(PWSTR::null(), |local| PWSTR(local.as_mut_ptr())),
        lpRemoteName: PWSTR(remote.as_mut_ptr()),
        lpProvider: PWSTR(provider.as_mut_ptr()),
        ..Default::default()
    };
    let (mut user, mut password) = match credentials {
        Credentials::Explicit { user, password } => (Some(wide(user)), Some(wide(password))),
        Credentials::Current => (None, None),
    };
    let pointer =
        |text: &Option<Vec<u16>>| text.as_ref().map_or(PCWSTR::null(), |t| PCWSTR(t.as_ptr()));
    // SAFETY: every string is nul-terminated and outlives the call, which only reads them.
    let status = unsafe {
        WNetAddConnection2W(
            &raw const resource,
            pointer(&password),
            pointer(&user),
            CONNECT_TEMPORARY,
        )
    };
    if let Some(password) = password.as_mut() {
        password.zeroize();
    }
    if let Some(user) = user.as_mut() {
        user.zeroize();
    }
    if status == NO_ERROR {
        Ok(())
    } else {
        Err(WNetError::new(status))
    }
}

/// Removes the mapping on `letter`. Without `force`, open files give 2401 `ERROR_OPEN_FILES` and the
/// mapping stays; with it, their handles fail from then on (spike M3-01 §2). Nothing is removed from
/// the user's profile, as `CONNECT_TEMPORARY` never put it there.
///
/// # Errors
/// What Windows answered: 2250 `ERROR_NOT_CONNECTED` if there is no mapping.
pub fn cancel_connection(letter: DriveLetter, force: bool) -> Result<(), WNetError> {
    cancel(&letter.to_string(), NET_CONNECT_FLAGS(0), force)
}

/// Removes the mapping on `letter` and Windows' memory of it (`CONNECT_UPDATE_PROFILE`), so that it
/// does not come back at the next sign-in: for a drive another program mapped, which DriveMount takes
/// over (FR-SMB-10). A remembered drive that is not connected is only forgotten. Open files behave as
/// for [`cancel_connection`].
///
/// # Errors
/// What Windows answered: 2401 `ERROR_OPEN_FILES` without `force` while files are open, 2250
/// `ERROR_NOT_CONNECTED` if there is no such drive, connected or remembered.
pub fn forget_connection(letter: DriveLetter, force: bool) -> Result<(), WNetError> {
    cancel(&letter.to_string(), CONNECT_UPDATE_PROFILE, force)
}

/// Removes the connection without a drive to `unc` that [`add_connection`] made with no letter.
///
/// # Errors
/// What Windows answered: 2250 `ERROR_NOT_CONNECTED` if there is none.
pub fn cancel_connection_to(unc: &str, force: bool) -> Result<(), WNetError> {
    cancel(unc, NET_CONNECT_FLAGS(0), force)
}

fn cancel(name: &str, flags: NET_CONNECT_FLAGS, force: bool) -> Result<(), WNetError> {
    let name = wide(name);
    // SAFETY: `name` is nul-terminated and outlives the call.
    let status = unsafe { WNetCancelConnection2W(PCWSTR(name.as_ptr()), flags, force) };
    if status == NO_ERROR {
        Ok(())
    } else {
        Err(WNetError::new(status))
    }
}

/// The bytes free to this user under `root` (`Z:\`). **Blocks for over a minute** on a network
/// drive whose server has gone (spike M3-01 §6), so call it on a thread that can be left behind.
///
/// # Errors
/// What Windows answered.
pub fn free_space(root: &str) -> io::Result<u64> {
    let root = wide(root);
    let mut available = 0u64;
    // SAFETY: `root` is nul-terminated, and the out pointer is valid; both outlive the call.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), Some(&raw mut available), None, None) }
        .map_err(|error| os_error(&error))?;
    Ok(available)
}

/// A network drive as MPR lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConnection {
    /// `W:`, or `None` for a connection without a letter.
    pub local: Option<String>,
    /// `\\server\share`.
    pub remote: String,
    /// The network provider, as "Microsoft Windows Network" or `WinFsp.Np`.
    pub provider: String,
}

/// Every disk connection of this logon session, of any provider (`RESOURCE_CONNECTED`).
///
/// # Errors
/// What MPR answered.
pub fn connected() -> Result<Vec<NetConnection>, WNetError> {
    enumerate(RESOURCE_CONNECTED)
}

/// Every remembered disk connection (`HKCU\Network`), connected now or not (`RESOURCE_REMEMBERED`).
///
/// # Errors
/// What MPR answered.
pub fn remembered() -> Result<Vec<NetConnection>, WNetError> {
    enumerate(RESOURCE_REMEMBERED)
}

/// Closes an enumeration however it ends.
struct Enumeration(HANDLE);

impl Drop for Enumeration {
    fn drop(&mut self) {
        // SAFETY: the handle came from WNetOpenEnumW and is closed once.
        let _ = unsafe { WNetCloseEnum(self.0) };
    }
}

fn enumerate(scope: NET_RESOURCE_SCOPE) -> Result<Vec<NetConnection>, WNetError> {
    let mut handle = HANDLE::default();
    // SAFETY: the out pointer is valid; no starting resource means the top level.
    let status = unsafe {
        WNetOpenEnumW(
            scope,
            RESOURCETYPE_DISK,
            WNET_OPEN_ENUM_USAGE(0),
            None,
            &raw mut handle,
        )
    };
    if status != NO_ERROR {
        return Err(WNetError::new(status));
    }
    let enumeration = Enumeration(handle);
    let mut found = Vec::new();
    // u64 for NETRESOURCEW's alignment: the buffer holds the structs, then their strings.
    let mut buffer = vec![0u64; 16 * 1024 / 8];
    loop {
        let mut count = u32::MAX;
        let mut size = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the buffer is `size` bytes, and every pointer is valid for the call.
        let status = unsafe {
            WNetEnumResourceW(
                enumeration.0,
                &raw mut count,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            return Ok(found);
        }
        if status == ERROR_MORE_DATA {
            // Not even one entry fitted; `size` is what it needs.
            buffer.resize((size as usize).div_ceil(8).max(buffer.len() * 2), 0);
            continue;
        }
        if status != NO_ERROR {
            return Err(WNetError::new(status));
        }
        // SAFETY: WNet wrote `count` NETRESOURCEW structs at the start of the buffer, which is
        // aligned for them, and their strings after them inside it.
        let resources = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<NETRESOURCEW>(), count as usize)
        };
        found.extend(resources.iter().map(|resource| NetConnection {
            local: text(resource.lpLocalName).filter(|local| !local.is_empty()),
            remote: text(resource.lpRemoteName).unwrap_or_default(),
            provider: text(resource.lpProvider).unwrap_or_default(),
        }));
    }
}

/// A string Windows wrote into a buffer that is still alive, or `None` for a null pointer.
fn text(pointer: PWSTR) -> Option<String> {
    // SAFETY: callers pass pointers to nul-terminated strings inside a live buffer from WNet or the
    // Net API, or null.
    (!pointer.is_null()).then(|| String::from_utf16_lossy(unsafe { pointer.as_wide() }))
}

/// An SMB use as `net use` lists it (`NetUseEnum` level 2), with its letter if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetUse {
    /// `W:`, or `None` for a share used by its path.
    pub local: Option<String>,
    /// `\\server\share`, or `\\server\IPC$`.
    pub remote: String,
    /// `ui2_status`: 0 OK, 1 paused, 2 disconnected, 3 network error, 4 connecting, 5 reconnecting.
    pub status: u32,
    /// The user it was made as.
    pub user: String,
    /// That user's domain, which may be empty.
    pub domain: String,
}

/// Frees a Net API buffer however the listing ends.
struct NetBuffer(*mut u8);

impl Drop for NetBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the buffer came from NetUseEnum and is freed once.
            let _ = unsafe { NetApiBufferFree(Some(self.0.cast_const().cast())) };
        }
    }
}

/// This logon session's SMB uses, with their status and user (spike M3-01 §11).
///
/// # Errors
/// What the Net API answered.
pub fn net_uses() -> Result<Vec<NetUse>, WNetError> {
    let mut buffer = NetBuffer(std::ptr::null_mut());
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: the out pointers are valid; the buffer is freed by `NetBuffer`.
    let status = unsafe {
        NetUseEnum(
            PCWSTR::null(),
            2,
            Some(&raw mut buffer.0),
            MAX_PREFERRED_LENGTH,
            Some(&raw mut read),
            &raw mut total,
            None,
        )
    };
    if status != NERR_SUCCESS {
        return Err(WNetError {
            code: status,
            text: system_text(status),
        });
    }
    if buffer.0.is_null() || read == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: NetUseEnum wrote `read` USE_INFO_2 structs at the start of the buffer it allocated.
    let uses = unsafe { std::slice::from_raw_parts(buffer.0.cast::<USE_INFO_2>(), read as usize) };
    Ok(uses
        .iter()
        .map(|item| NetUse {
            local: text(item.ui2_local).filter(|local| !local.is_empty()),
            remote: text(item.ui2_remote).unwrap_or_default(),
            status: item.ui2_status,
            user: text(item.ui2_username).unwrap_or_default(),
            domain: text(item.ui2_domainname).unwrap_or_default(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{free_letter, lock_letters};

    #[test]
    fn the_smb_provider_has_a_name() {
        let name = smb_provider().unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn listings_work_with_or_without_drives() {
        connected().unwrap();
        remembered().unwrap();
        net_uses().unwrap();
    }

    #[test]
    fn removing_a_letter_with_no_mapping_is_not_connected() {
        let _letters = lock_letters();
        let error = cancel_connection(free_letter(), false).unwrap_err();
        assert_eq!(error.code, 2250, "{error}");
    }

    #[test]
    fn forgetting_a_letter_with_no_mapping_is_not_connected() {
        let _letters = lock_letters();
        let error = forget_connection(free_letter(), false).unwrap_err();
        assert_eq!(error.code, 2250, "{error}");
    }

    #[test]
    fn a_share_on_a_name_that_does_not_resolve_is_bad_netpath() {
        let _letters = lock_letters();
        let provider = smb_provider().unwrap();
        let error = add_connection(
            Some(free_letter()),
            r"\\drivemount-test.invalid\share",
            &provider,
            Credentials::Current,
        )
        .unwrap_err();
        assert_eq!(error.code, 53, "{error}");
    }

    #[test]
    fn credentials_never_show_the_password() {
        let credentials = Credentials::Explicit {
            user: "smbuser1",
            password: "hunter2",
        };
        let shown = format!("{credentials:?}");
        assert!(shown.contains("smbuser1"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn strings_stop_at_the_nul() {
        assert_eq!(until_nul(&[65, 66, 0, 67]), [65, 66]);
        assert_eq!(until_nul(&[65]), [65]);
    }
}
