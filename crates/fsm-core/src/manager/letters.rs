// SPDX-License-Identifier: GPL-3.0-or-later

//! Which letter a mount goes on (FR-MNT-02 to FR-MNT-04).

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fsm_vfs::{DriveLetter, MountError};

use crate::config::LetterConflict;
use crate::profile::DriveChoice;

/// The first letter a mount may use: A, B and C never are (FR-MNT-03).
const FIRST_LETTER: char = 'D';
const LAST_LETTER: char = 'Z';

/// What the system says about a letter. On Windows this is `fsm_win::drives::is_letter_free`.
pub trait DriveLetters: Send + Sync {
    /// Whether nothing on the system has this letter, by FR-MNT-03's definition. It may make system
    /// calls, and is never called on an async worker thread.
    ///
    /// # Errors
    /// The system could not say.
    fn is_free(&self, letter: DriveLetter) -> io::Result<bool>;
}

type Reserved = Arc<Mutex<BTreeSet<DriveLetter>>>;

/// Picks letters, and keeps each one from being picked again until its [`Allocation`] goes.
///
/// A letter only shows as taken once the drive is up, which can be after a long dialog, so mounts
/// started together (FR-MNT-08) would all find Z: free.
pub(super) struct Allocator {
    system: Arc<dyn DriveLetters>,
    reserved: Reserved,
}

/// A letter for one mount, until this is dropped.
#[derive(Debug)]
pub(super) struct Allocation {
    letter: DriveLetter,
    instead_of: Option<DriveLetter>,
    reserved: Reserved,
}

impl Allocation {
    pub(super) const fn letter(&self) -> DriveLetter {
        self.letter
    }

    /// The profile's own letter, if this is another one (FR-MNT-04).
    pub(super) const fn instead_of(&self) -> Option<DriveLetter> {
        self.instead_of
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        lock(&self.reserved).remove(&self.letter);
    }
}

fn lock(reserved: &Reserved) -> MutexGuard<'_, BTreeSet<DriveLetter>> {
    reserved.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Allocator {
    pub(super) fn new(system: Arc<dyn DriveLetters>) -> Self {
        Self {
            system,
            reserved: Reserved::default(),
        }
    }

    /// A letter for `choice`, not one of `tried`, which are those the backend has found taken.
    ///
    /// *Auto* is the highest free letter. A profile's own letter that is taken goes by `conflict`,
    /// and *auto* then means the same highest free letter.
    pub(super) fn allocate(
        &self,
        choice: DriveChoice,
        conflict: LetterConflict,
        tried: &[DriveLetter],
    ) -> Result<Allocation, MountError> {
        let mut reserved = lock(&self.reserved);
        let wanted = match choice {
            DriveChoice::Auto => None,
            DriveChoice::Letter(letter) => Some(letter),
        };
        if let Some(letter) = wanted {
            if self.can_use(letter, &reserved, tried)? {
                reserved.insert(letter);
                return Ok(self.allocation(letter, None));
            }
            if conflict == LetterConflict::Fail {
                return Err(MountError::LetterTaken(letter));
            }
        }
        for letter in (FIRST_LETTER..=LAST_LETTER)
            .rev()
            .filter_map(DriveLetter::new)
        {
            if self.can_use(letter, &reserved, tried)? {
                reserved.insert(letter);
                return Ok(self.allocation(letter, wanted));
            }
        }
        Err(MountError::NoFreeLetter)
    }

    fn can_use(
        &self,
        letter: DriveLetter,
        reserved: &BTreeSet<DriveLetter>,
        tried: &[DriveLetter],
    ) -> Result<bool, MountError> {
        if letter.as_char() < FIRST_LETTER || reserved.contains(&letter) || tried.contains(&letter)
        {
            return Ok(false);
        }
        self.system
            .is_free(letter)
            .map_err(|error| MountError::Other(format!("checking drive {letter}: {error}")))
    }

    fn allocation(&self, letter: DriveLetter, instead_of: Option<DriveLetter>) -> Allocation {
        Allocation {
            letter,
            instead_of,
            reserved: Arc::clone(&self.reserved),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[derive(Default)]
    struct System {
        taken: Mutex<HashSet<char>>,
        asked: Mutex<Vec<char>>,
        broken: bool,
    }

    impl DriveLetters for System {
        fn is_free(&self, letter: DriveLetter) -> io::Result<bool> {
            self.asked.lock().unwrap().push(letter.as_char());
            if self.broken {
                return Err(io::Error::other("no answer"));
            }
            Ok(!self.taken.lock().unwrap().contains(&letter.as_char()))
        }
    }

    fn letter(character: char) -> DriveLetter {
        DriveLetter::new(character).unwrap()
    }

    fn with_taken(taken: impl IntoIterator<Item = char>) -> (Allocator, Arc<System>) {
        let system = Arc::new(System {
            taken: Mutex::new(taken.into_iter().collect()),
            ..System::default()
        });
        (Allocator::new(Arc::clone(&system) as _), system)
    }

    fn auto(allocator: &Allocator) -> Result<Allocation, MountError> {
        allocator.allocate(DriveChoice::Auto, LetterConflict::Auto, &[])
    }

    #[test]
    fn auto_takes_z_when_it_is_free() {
        let (allocator, _) = with_taken([]);
        let allocation = auto(&allocator).unwrap();
        assert_eq!(allocation.letter(), letter('Z'));
        assert_eq!(allocation.instead_of(), None);
    }

    #[test]
    fn auto_goes_down_from_z_past_what_is_taken() {
        let (allocator, _) = with_taken(['Z', 'Y', 'W']);
        assert_eq!(auto(&allocator).unwrap().letter(), letter('X'));
    }

    #[test]
    fn a_b_and_c_are_never_used_or_even_asked_about() {
        let (allocator, system) = with_taken(('D'..='Z').collect::<Vec<_>>());
        assert_eq!(auto(&allocator).unwrap_err(), MountError::NoFreeLetter);
        let asked = system.asked.lock().unwrap();
        assert_eq!(asked.first(), Some(&'Z'));
        assert_eq!(asked.last(), Some(&'D'));
        assert_eq!(asked.len(), 23);
    }

    #[test]
    fn d_is_the_last_letter_auto_can_take() {
        let (allocator, _) = with_taken(('E'..='Z').collect::<Vec<_>>());
        assert_eq!(auto(&allocator).unwrap().letter(), letter('D'));
    }

    #[test]
    fn a_free_letter_of_the_profile_is_used_under_either_policy() {
        for conflict in [LetterConflict::Auto, LetterConflict::Fail] {
            let (allocator, _) = with_taken(['Z']);
            let allocation = allocator
                .allocate(DriveChoice::Letter(letter('M')), conflict, &[])
                .unwrap();
            assert_eq!(allocation.letter(), letter('M'));
            assert_eq!(allocation.instead_of(), None);
        }
    }

    #[test]
    fn a_taken_letter_under_auto_moves_on_and_says_from_where() {
        let (allocator, _) = with_taken(['Z']);
        let allocation = allocator
            .allocate(DriveChoice::Letter(letter('Z')), LetterConflict::Auto, &[])
            .unwrap();
        assert_eq!(allocation.letter(), letter('Y'));
        assert_eq!(allocation.instead_of(), Some(letter('Z')));
    }

    #[test]
    fn a_taken_letter_under_fail_is_an_error() {
        let (allocator, _) = with_taken(['Z']);
        let error = allocator
            .allocate(DriveChoice::Letter(letter('Z')), LetterConflict::Fail, &[])
            .unwrap_err();
        assert_eq!(error, MountError::LetterTaken(letter('Z')));
    }

    #[test]
    fn a_taken_letter_with_nothing_else_free_is_no_free_letter() {
        let (allocator, _) = with_taken(('D'..='Z').collect::<Vec<_>>());
        let error = allocator
            .allocate(DriveChoice::Letter(letter('Z')), LetterConflict::Auto, &[])
            .unwrap_err();
        assert_eq!(error, MountError::NoFreeLetter);
    }

    /// `check_drive` refuses these and the file cannot hold them, but the rule is the allocator's.
    #[test]
    fn a_profile_cannot_ask_for_a_b_or_c() {
        for reserved_letter in ['A', 'B', 'C'] {
            let (allocator, system) = with_taken([]);
            let choice = DriveChoice::Letter(letter(reserved_letter));
            let error = allocator
                .allocate(choice, LetterConflict::Fail, &[])
                .unwrap_err();
            assert_eq!(error, MountError::LetterTaken(letter(reserved_letter)));
            assert!(system.asked.lock().unwrap().is_empty());

            let moved = allocator
                .allocate(choice, LetterConflict::Auto, &[])
                .unwrap();
            assert_eq!(moved.letter(), letter('Z'));
        }
    }

    #[test]
    fn a_letter_in_use_by_another_mount_is_not_given_out_until_that_goes() {
        let (allocator, _) = with_taken([]);
        let first = auto(&allocator).unwrap();
        let second = auto(&allocator).unwrap();
        assert_eq!(first.letter(), letter('Z'));
        assert_eq!(second.letter(), letter('Y'));

        let wanted = DriveChoice::Letter(letter('Z'));
        let error = allocator
            .allocate(wanted, LetterConflict::Fail, &[])
            .unwrap_err();
        assert_eq!(error, MountError::LetterTaken(letter('Z')));

        drop(first);
        assert_eq!(auto(&allocator).unwrap().letter(), letter('Z'));
        drop(second);
    }

    #[test]
    fn a_profiles_own_letter_is_held_too() {
        let (allocator, _) = with_taken([]);
        let wanted = DriveChoice::Letter(letter('M'));
        let held = allocator
            .allocate(wanted, LetterConflict::Fail, &[])
            .unwrap();

        let error = allocator
            .allocate(wanted, LetterConflict::Fail, &[])
            .unwrap_err();
        assert_eq!(error, MountError::LetterTaken(letter('M')));

        drop(held);
        allocator
            .allocate(wanted, LetterConflict::Fail, &[])
            .unwrap();
    }

    #[test]
    fn letters_the_backend_found_taken_are_passed_over() {
        let (allocator, _) = with_taken([]);
        let tried = [letter('Z'), letter('Y')];
        let auto = allocator
            .allocate(DriveChoice::Auto, LetterConflict::Auto, &tried)
            .unwrap();
        assert_eq!(auto.letter(), letter('X'));
        assert_eq!(auto.instead_of(), None);

        let own = allocator
            .allocate(
                DriveChoice::Letter(letter('Z')),
                LetterConflict::Auto,
                &tried,
            )
            .unwrap();
        assert_eq!(own.letter(), letter('W'));
        assert_eq!(own.instead_of(), Some(letter('Z')));
    }

    #[test]
    fn a_failed_allocation_reserves_nothing() {
        let (allocator, system) = with_taken(['Z']);
        let wanted = DriveChoice::Letter(letter('Z'));
        allocator
            .allocate(wanted, LetterConflict::Fail, &[])
            .unwrap_err();
        system.taken.lock().unwrap().clear();
        assert_eq!(auto(&allocator).unwrap().letter(), letter('Z'));
    }

    #[test]
    fn a_system_that_cannot_answer_is_an_error_and_not_a_free_letter() {
        let system = Arc::new(System {
            broken: true,
            ..System::default()
        });
        let allocator = Allocator::new(system);
        assert!(matches!(auto(&allocator), Err(MountError::Other(_))));
    }
}
