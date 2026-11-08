// SPDX-License-Identifier: GPL-3.0-or-later

//! What the tests of several modules share.

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};

use drivemount_vfs::DriveLetter;

use crate::drives::is_letter_free;

static LETTERS: Mutex<()> = Mutex::new(());

/// Tests that use a free letter run one at a time, or they would take each other's.
pub fn lock_letters() -> MutexGuard<'static, ()> {
    LETTERS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn letter(c: char) -> DriveLetter {
    DriveLetter::new(c).unwrap()
}

/// A free letter, away from both ends of the alphabet, where people's own drives tend to be.
pub fn free_letter() -> DriveLetter {
    ('H'..='Q')
        .rev()
        .map(letter)
        .find(|&candidate| is_letter_free(candidate).unwrap())
        .expect("no free letter from H to Q")
}

/// A `subst` drive, removed on drop. Hold [`lock_letters`] for as long as it lives.
pub struct SubstDrive(DriveLetter);

impl SubstDrive {
    pub fn new(target: &Path) -> Self {
        let free = free_letter();
        let status = Command::new("subst")
            .arg(free.to_string())
            .arg(target)
            .status()
            .unwrap();
        assert!(status.success(), "subst failed: {status}");
        Self(free)
    }

    pub const fn letter(&self) -> DriveLetter {
        self.0
    }
}

impl Drop for SubstDrive {
    fn drop(&mut self) {
        let _ = Command::new("subst")
            .arg(self.0.to_string())
            .arg("/D")
            .status();
    }
}
