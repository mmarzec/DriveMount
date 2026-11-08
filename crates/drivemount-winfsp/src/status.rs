// SPDX-License-Identifier: GPL-3.0-or-later

//! The error mapping table of spec §5.8.

use drivemount_vfs::FsError;
use tracing::warn;
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL,
    STATUS_IO_TIMEOUT, STATUS_MEDIA_WRITE_PROTECTED, STATUS_NETWORK_UNREACHABLE,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_TOO_MANY_OPENED_FILES, STATUS_UNEXPECTED_NETWORK_ERROR,
};
use winfsp::FspError;

/// What Windows is told for a failed remote call.
pub(crate) fn status_of(error: &FsError) -> NTSTATUS {
    match error {
        FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FsError::PathNotFound => STATUS_OBJECT_PATH_NOT_FOUND,
        FsError::PermissionDenied => STATUS_ACCESS_DENIED,
        FsError::Exists => STATUS_OBJECT_NAME_COLLISION,
        FsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        FsError::NotSupported => STATUS_NOT_SUPPORTED,
        FsError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        FsError::Timeout => STATUS_IO_TIMEOUT,
        FsError::ConnectionLost => STATUS_NETWORK_UNREACHABLE,
        FsError::NoSpace => STATUS_DISK_FULL,
        FsError::TooManyOpenFiles => STATUS_TOO_MANY_OPENED_FILES,
        FsError::Other(_) => STATUS_UNEXPECTED_NETWORK_ERROR,
    }
}

/// The error for a callback to return. Only `Other` carries details, and only the log sees them.
pub(crate) fn fail(error: FsError) -> FspError {
    if let FsError::Other(details) = &error {
        warn!(%details, "remote call failed");
    }
    status_of(&error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_the_status_in_the_spec() {
        for (error, status) in [
            (FsError::NotFound, STATUS_OBJECT_NAME_NOT_FOUND),
            (FsError::PathNotFound, STATUS_OBJECT_PATH_NOT_FOUND),
            (FsError::PermissionDenied, STATUS_ACCESS_DENIED),
            (FsError::Exists, STATUS_OBJECT_NAME_COLLISION),
            (FsError::NotEmpty, STATUS_DIRECTORY_NOT_EMPTY),
            (FsError::NotSupported, STATUS_NOT_SUPPORTED),
            (FsError::ReadOnly, STATUS_MEDIA_WRITE_PROTECTED),
            (FsError::Timeout, STATUS_IO_TIMEOUT),
            (FsError::ConnectionLost, STATUS_NETWORK_UNREACHABLE),
            (FsError::NoSpace, STATUS_DISK_FULL),
            (FsError::TooManyOpenFiles, STATUS_TOO_MANY_OPENED_FILES),
            (
                FsError::Other("details".to_owned()),
                STATUS_UNEXPECTED_NETWORK_ERROR,
            ),
        ] {
            assert_eq!(status_of(&error), status, "{error:?}");
            assert_eq!(fail(error).to_ntstatus(), status.0);
        }
    }
}
