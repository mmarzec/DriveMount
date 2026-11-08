// SPDX-License-Identifier: GPL-3.0-or-later

//! Normalised remote paths.

use std::fmt;

use thiserror::Error;

/// Why a string cannot become a [`RemotePath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathError {
    /// [`RemotePath::new`] needs a path starting with `/`.
    #[error("remote path is not absolute")]
    NotAbsolute,
    /// [`RemotePath::join`] needs a path not starting with `/`.
    #[error("path to join is not relative")]
    NotRelative,
    /// A `..` would go above the root.
    #[error("remote path goes above the root")]
    EscapesRoot,
    /// SFTP and NFS servers treat paths as C strings, so a NUL would cut the path short.
    #[error("remote path contains a NUL character")]
    ContainsNul,
}

/// An absolute, normalised POSIX path on a remote.
///
/// The value always starts with `/`, has no empty, `.` or `..` components, and has no trailing `/`
/// unless it is the root. Backslashes and other characters that Windows forbids are ordinary name
/// characters here; name mapping is a separate step.
///
/// `..` is resolved lexically, and going above the root is an error rather than being clamped, so
/// a path can never silently turn into a different file. Lexical resolution differs from the
/// server's when a component is a symlink (`/link/..` is `/` here), so callers should pass paths
/// without `..` where they can.
///
/// ```
/// use drivemount_vfs::RemotePath;
///
/// let path = RemotePath::new("/data//logs/./old/../app.log").unwrap();
/// assert_eq!(path.as_str(), "/data/logs/app.log");
/// assert_eq!(path.file_name(), Some("app.log"));
/// assert_eq!(path.parent().unwrap().as_str(), "/data/logs");
/// assert!(RemotePath::new("/data/../..").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

impl RemotePath {
    /// The root path, `/`.
    #[must_use]
    pub fn root() -> Self {
        Self("/".to_owned())
    }

    /// Normalises an absolute path.
    ///
    /// # Errors
    ///
    /// [`PathError::NotAbsolute`] without a leading `/`, [`PathError::EscapesRoot`] if a `..` goes
    /// above the root, and [`PathError::ContainsNul`] if the path contains a NUL.
    pub fn new(path: &str) -> Result<Self, PathError> {
        if !path.starts_with('/') {
            return Err(PathError::NotAbsolute);
        }
        Self::root().append(path)
    }

    /// Appends a relative path of one or more components.
    ///
    /// Unlike [`std::path::Path::join`], an absolute argument is an error and does not replace the
    /// path. `..` may walk back up, but not above the root.
    ///
    /// # Errors
    ///
    /// [`PathError::NotRelative`] with a leading `/`, [`PathError::EscapesRoot`] if a `..` goes
    /// above the root, and [`PathError::ContainsNul`] if the path contains a NUL.
    pub fn join(&self, relative: &str) -> Result<Self, PathError> {
        if relative.starts_with('/') {
            return Err(PathError::NotRelative);
        }
        self.clone().append(relative)
    }

    /// The containing directory, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        (!self.is_root()).then(|| Self(self.0[..self.parent_len()].to_owned()))
    }

    /// The last component, or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        let (_, name) = self.0.rsplit_once('/')?;
        (!name.is_empty()).then_some(name)
    }

    /// Whether this is the root path.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// The path as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The length of the parent's path. The parent of `/a` is `/`, so the slash stays there.
    fn parent_len(&self) -> usize {
        self.0.rfind('/').filter(|&slash| slash > 0).unwrap_or(1)
    }

    /// Applies each component of `input` to a path that is already normalised.
    fn append(mut self, input: &str) -> Result<Self, PathError> {
        if input.contains('\0') {
            return Err(PathError::ContainsNul);
        }
        for component in input.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    if self.is_root() {
                        return Err(PathError::EscapesRoot);
                    }
                    self.0.truncate(self.parent_len());
                }
                name => {
                    if !self.is_root() {
                        self.0.push('/');
                    }
                    self.0.push_str(name);
                }
            }
        }
        Ok(self)
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RemotePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn path(s: &str) -> RemotePath {
        RemotePath::new(s).unwrap()
    }

    #[test]
    fn new_normalises() {
        for (input, expected) in [
            ("/", "/"),
            ("//", "/"),
            ("/.", "/"),
            ("/a", "/a"),
            ("/a/", "/a"),
            ("//a///b//", "/a/b"),
            ("/a/./b/.", "/a/b"),
            ("/a/b/..", "/a"),
            ("/a/..", "/"),
            ("/a/../b/../c", "/c"),
            ("/a/.../b", "/a/.../b"),
            ("/a/..b/c..", "/a/..b/c.."),
            ("/with space/ü/日本語", "/with space/ü/日本語"),
            ("/back\\slash", "/back\\slash"),
        ] {
            assert_eq!(path(input).as_str(), expected, "input {input:?}");
        }
    }

    #[test]
    fn new_rejects_bad_input() {
        assert_eq!(RemotePath::new(""), Err(PathError::NotAbsolute));
        assert_eq!(RemotePath::new("a/b"), Err(PathError::NotAbsolute));
        assert_eq!(RemotePath::new("./a"), Err(PathError::NotAbsolute));
        assert_eq!(RemotePath::new("~/a"), Err(PathError::NotAbsolute));
        assert_eq!(RemotePath::new("/.."), Err(PathError::EscapesRoot));
        assert_eq!(RemotePath::new("/a/../.."), Err(PathError::EscapesRoot));
        assert_eq!(RemotePath::new("/../a"), Err(PathError::EscapesRoot));
        assert_eq!(RemotePath::new("/a\0b"), Err(PathError::ContainsNul));
    }

    #[test]
    fn join_appends_relative_paths() {
        assert_eq!(path("/a").join("b").unwrap(), path("/a/b"));
        assert_eq!(path("/").join("b").unwrap(), path("/b"));
        assert_eq!(path("/a").join("b/c/").unwrap(), path("/a/b/c"));
        assert_eq!(path("/a").join("").unwrap(), path("/a"));
        assert_eq!(path("/a").join(".").unwrap(), path("/a"));
        assert_eq!(path("/a/b").join("..").unwrap(), path("/a"));
        assert_eq!(path("/a/b").join("../c").unwrap(), path("/a/c"));
    }

    #[test]
    fn join_cannot_escape_or_replace() {
        assert_eq!(path("/a").join("/etc"), Err(PathError::NotRelative));
        assert_eq!(path("/a").join("../.."), Err(PathError::EscapesRoot));
        assert_eq!(path("/").join(".."), Err(PathError::EscapesRoot));
        assert_eq!(path("/a").join("b\0"), Err(PathError::ContainsNul));
    }

    #[test]
    fn parent_and_file_name() {
        assert_eq!(RemotePath::root().parent(), None);
        assert_eq!(RemotePath::root().file_name(), None);
        assert_eq!(path("/a").parent(), Some(RemotePath::root()));
        assert_eq!(path("/a").file_name(), Some("a"));
        assert_eq!(path("/a/b/c.txt").parent(), Some(path("/a/b")));
        assert_eq!(path("/a/b/c.txt").file_name(), Some("c.txt"));
    }

    #[test]
    fn root_is_root_and_displays() {
        assert!(RemotePath::root().is_root());
        assert!(!path("/a").is_root());
        assert_eq!(path("/a/b").to_string(), "/a/b");
    }

    /// Strings of `/`-separated components, weighted towards the awkward ones.
    fn raw_path() -> impl Strategy<Value = String> {
        let component = prop_oneof![
            Just(String::new()),
            Just(".".to_owned()),
            Just("..".to_owned()),
            Just("...".to_owned()),
            "[a-zA-Z0-9 ü_.-]{1,6}",
        ];
        prop::collection::vec(component, 0..8).prop_map(|parts| parts.join("/"))
    }

    /// The expected result, worked out independently of the implementation.
    fn model(raw: &str) -> Option<Vec<&str>> {
        let mut stack = Vec::new();
        for component in raw.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    stack.pop()?;
                }
                name => stack.push(name),
            }
        }
        Some(stack)
    }

    proptest! {
        #[test]
        fn normalising_matches_the_model(raw in raw_path()) {
            let input = format!("/{raw}");
            match (RemotePath::new(&input), model(&raw)) {
                (Ok(path), Some(parts)) => {
                    prop_assert_eq!(path.as_str(), format!("/{}", parts.join("/")));
                }
                (Err(error), None) => prop_assert_eq!(error, PathError::EscapesRoot),
                (result, expected) => {
                    prop_assert!(false, "{input:?}: got {result:?}, model {expected:?}");
                }
            }
        }

        #[test]
        fn normalising_is_idempotent(raw in raw_path()) {
            if let Ok(path) = RemotePath::new(&format!("/{raw}")) {
                prop_assert_eq!(RemotePath::new(path.as_str()).unwrap(), path);
            }
        }

        #[test]
        fn parent_and_file_name_rebuild_the_path(raw in raw_path()) {
            if let Ok(path) = RemotePath::new(&format!("/{raw}")) {
                match (path.parent(), path.file_name()) {
                    (Some(parent), Some(name)) => {
                        prop_assert_eq!(parent.join(name).unwrap(), path);
                    }
                    (None, None) => prop_assert!(path.is_root()),
                    other => prop_assert!(false, "{path:?}: got {other:?}"),
                }
            }
        }

        #[test]
        fn join_matches_concatenation(base in raw_path(), relative in raw_path()) {
            prop_assume!(!relative.starts_with('/'));
            if let Ok(path) = RemotePath::new(&format!("/{base}")) {
                prop_assert_eq!(
                    path.join(&relative),
                    RemotePath::new(&format!("{path}/{relative}"))
                );
            }
        }
    }
}
