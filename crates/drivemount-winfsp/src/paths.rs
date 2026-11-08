// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows paths ↔ remote paths. Which entry a name means is decided by the adapter, over a listing.

use drivemount_vfs::{RemotePath, remote_to_windows, windows_to_remote};
use windows::Win32::Foundation::STATUS_OBJECT_NAME_INVALID;
use winfsp::{FspError, U16CStr};

fn invalid_name() -> FspError {
    STATUS_OBJECT_NAME_INVALID.into()
}

/// The components of a path from WinFsp, such as `\a\b`.
///
/// A name with a lone surrogate is refused rather than repaired. It cannot exist on the server, and
/// a repaired name could be another file's.
pub(crate) fn components(path: &U16CStr) -> Result<Vec<String>, FspError> {
    let text = String::from_utf16(path.as_slice()).map_err(|_| invalid_name())?;
    Ok(text
        .split('\\')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The remote path if every component exists with just this spelling. Refuses a component that has
/// no remote form, which keeps `..` and `/` from reaching another directory (spec §5.6).
pub(crate) fn exact_path(components: &[String]) -> Result<RemotePath, FspError> {
    let mut path = RemotePath::root();
    for component in components {
        let name = windows_to_remote(component).map_err(|_| invalid_name())?;
        path = path.join(&name).map_err(|_| invalid_name())?;
    }
    Ok(path)
}

/// How Windows names a remote path, as UTF-16, starting at the drive's root.
pub(crate) fn windows_path(path: &RemotePath) -> Vec<u16> {
    let mut shown = String::with_capacity(path.as_str().len());
    for name in path.as_str().split('/').filter(|name| !name.is_empty()) {
        shown.push('\\');
        shown.push_str(&remote_to_windows(name));
    }
    if shown.is_empty() {
        shown.push('\\');
    }
    shown.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use winfsp::U16CString;

    use super::*;

    fn wide(text: &str) -> U16CString {
        U16CString::from_str(text).unwrap()
    }

    fn parts(text: &str) -> Vec<String> {
        components(&wide(text)).unwrap()
    }

    fn exact(text: &str) -> Result<String, i32> {
        exact_path(&parts(text))
            .map(|path| path.as_str().to_owned())
            .map_err(|error| error.to_ntstatus())
    }

    #[test]
    fn a_path_splits_at_backslashes() {
        assert_eq!(parts(r"\a\b c\d.txt"), ["a", "b c", "d.txt"]);
        assert_eq!(parts(r"\a\"), ["a"]);
        assert!(parts(r"\").is_empty());
    }

    #[test]
    fn a_lone_surrogate_is_refused() {
        let name = U16CString::from_vec([u16::from(b'\\'), 0xD800, u16::from(b'a')]).unwrap();
        assert_eq!(
            components(&name).unwrap_err().to_ntstatus(),
            STATUS_OBJECT_NAME_INVALID.0
        );
    }

    #[test]
    fn names_are_mapped_on_the_way_out() {
        assert_eq!(exact(r"\").unwrap(), "/");
        assert_eq!(exact(r"\a\b").unwrap(), "/a/b");
        assert_eq!(exact("\\a\u{F03A}b\\end\u{F02E}").unwrap(), "/a:b/end.");
    }

    #[test]
    fn a_name_cannot_reach_another_directory() {
        for name in ["\\\u{F02E}", "\\\u{F02E}\u{F02E}", r"\a/b", r"\..", r"\."] {
            assert_eq!(exact(name), Err(STATUS_OBJECT_NAME_INVALID.0), "{name}");
        }
    }

    #[test]
    fn a_remote_path_is_shown_with_mapped_names() {
        let shown = |text| String::from_utf16(&windows_path(&RemotePath::new(text).unwrap()));
        assert_eq!(shown("/").unwrap(), r"\");
        assert_eq!(shown("/Dir/File.txt").unwrap(), r"\Dir\File.txt");
        assert_eq!(shown("/a:b/end.").unwrap(), "\\a\u{F03A}b\\end\u{F02E}");
    }
}
