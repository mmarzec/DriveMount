// SPDX-License-Identifier: GPL-3.0-or-later

//! What the window remembers (spec §7.1): its size and position.
//!
//! This is `ui-state.toml` beside `config.toml` (spec §6.1), and not part of it. Moving a window should
//! not rewrite and back up the user's profiles, and this still works when that file is read-only. It
//! is worth little to the user, so anything wrong with it costs only what it held.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// The window's place. Size is in logical pixels, which survive a change of scaling, and position in
/// physical ones, as that is what monitors are laid out in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(super) struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Remembered {
    pub maximized: bool,
    pub placement: Option<Placement>,
}

impl Remembered {
    /// What the file holds. No file, or a bad one, is the same as nothing remembered.
    pub(super) fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(%error, "could not read what the window remembers");
                }
                return Self::default();
            }
        };
        toml::from_str(&text).unwrap_or_else(|error| {
            warn!(%error, "ignoring what the window remembers");
            Self::default()
        })
    }

    /// Replaces the file. Failing costs only what this was going to remember. It is small and not
    /// worth flushing, which is what lets it be on the UI thread.
    pub(super) fn save(&self, path: &Path) {
        if let Err(error) = self.try_save(path) {
            warn!(%error, "could not save what the window remembers");
        }
    }

    fn try_save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let folder = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        fs::create_dir_all(&folder)?;
        let mut file = tempfile::NamedTempFile::new_in(&folder)?;
        file.write_all(text.as_bytes())?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_was_saved_comes_back() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("new folder").join("ui-state.toml");
        assert_eq!(Remembered::load(&path), Remembered::default());

        let remembered = Remembered {
            maximized: true,
            placement: Some(Placement {
                x: -1200,
                y: 40,
                width: 700.5,
                height: 500.0,
            }),
        };
        remembered.save(&path);
        assert_eq!(Remembered::load(&path), remembered);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_bad_file_is_the_same_as_none_and_is_replaced() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("ui-state.toml");
        fs::write(&path, "not [ toml").unwrap();
        assert_eq!(Remembered::load(&path), Remembered::default());

        let remembered = Remembered {
            maximized: true,
            ..Remembered::default()
        };
        remembered.save(&path);
        assert_eq!(Remembered::load(&path), remembered);
    }

    #[test]
    fn a_file_missing_a_part_keeps_the_rest() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("ui-state.toml");
        fs::write(&path, "maximized = true\nsomething_new = 1\n").unwrap();
        let loaded = Remembered::load(&path);
        assert!(loaded.maximized);
        assert_eq!(loaded.placement, None);
    }

    #[test]
    fn a_save_that_cannot_happen_is_not_an_error() {
        let folder = tempfile::tempdir().unwrap();
        // A folder where the file should go.
        let path = folder.path().join("ui-state.toml");
        fs::create_dir(&path).unwrap();
        Remembered::default().save(&path);
        assert!(path.is_dir());
    }
}
