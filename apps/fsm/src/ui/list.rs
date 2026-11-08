// SPDX-License-Identifier: GPL-3.0-or-later

//! What the list shows (spec §7.1), from the file and the latest events: which rows, in which order,
//! and what each may do. It knows nothing of Slint, and holds no text to translate, so all of it can
//! be tested without a window.

use std::collections::{BTreeSet, HashMap};

use fsm_core::config::{Config, UnreadableProfile};
use fsm_core::manager::{MountDetail, MountEvent, MountState};
use fsm_core::profile::{DEFAULT_PORT, DriveChoice, Profile, Protocol};
use fsm_vfs::{DriveLetter, MountError};
use uuid::Uuid;

/// Logical pixels. Fixed, so that where a row is can be worked out without laying it out, which is
/// how the selection is kept in view. Every row is one line: what does not fit is on its page.
const ROW_HEIGHT: f32 = 44.0;

/// Which thing a list item is, so that the selection stays on it when the list changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Key {
    /// A profile.
    Profile(Uuid),
    /// An entry of [`Config::unreadable`], by its place there, as it may have no id.
    Unreadable(usize),
}

/// What a row's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Primary {
    /// *Mount*.
    Mount,
    /// *Cancel*.
    Cancel,
    /// *Unmount*.
    Unmount,
    /// *Retry*, which mounts.
    Retry,
}

/// What a row has to say beyond its state, on its page and in its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Line {
    /// There is none.
    None,
    /// "Y: (Z: was in use)" (FR-MNT-04).
    LetterMoved {
        /// Where it is.
        letter: DriveLetter,
        /// Where it wanted to be.
        wanted: DriveLetter,
    },
    /// It breaks a rule of spec §7.3, so it cannot be mounted until edited.
    NeedsCorrecting,
    /// Why it is in *Error*.
    Failed(MountError),
}

/// A profile's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProfileItem {
    /// The profile.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// `user@host:path`.
    pub address: String,
    /// The letter it is on, or the one it asks for.
    pub letter: Option<DriveLetter>,
    /// Whether it is on that letter now. Otherwise it is dimmed.
    pub letter_is_live: bool,
    /// Its state.
    pub state: MountState,
    /// The line under it.
    pub line: Line,
    /// Its button.
    pub primary: Primary,
    /// Whether its button works now.
    pub primary_enabled: bool,
    /// Whether it can be opened in Explorer.
    pub can_open: bool,
    /// Whether it can be deleted.
    pub can_delete: bool,
}

/// What a list item is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Body {
    /// A profile.
    Profile(Box<ProfileItem>),
    /// An entry of the file that the model cannot hold.
    Unreadable {
        /// Its name, if it has one.
        name: Option<String>,
        /// What is wrong with it.
        problem: String,
        /// Whether it can be deleted.
        can_delete: bool,
    },
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    /// What the selection knows it by.
    pub key: Key,
    /// What it shows.
    pub body: Body,
}

impl Item {
    /// How high it is, in logical pixels.
    #[expect(
        clippy::unused_self,
        reason = "rows were not always one height, and may not stay so"
    )]
    pub(super) const fn height(&self) -> f32 {
        ROW_HEIGHT
    }
}

/// The status bar's counts (spec §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Counts {
    /// *Mounted*.
    pub mounted: usize,
    /// *Connecting*.
    pub connecting: usize,
    /// *Error*.
    pub failed: usize,
}

/// Everything the list is made from.
#[derive(Debug, Default)]
pub(super) struct ListState {
    config: Config,
    invalid: BTreeSet<Uuid>,
    status: HashMap<Uuid, (MountState, Option<MountDetail>)>,
    selected: Option<Key>,
    /// Where the selection was, so that it can stay near there when its row goes.
    selected_place: usize,
    can_mount: bool,
    read_only: bool,
}

impl ListState {
    /// An empty list, with mounting allowed.
    pub(super) fn new() -> Self {
        Self {
            can_mount: true,
            ..Self::default()
        }
    }

    /// What is in the file. A profile keeps its state across this, as a Save does not change it.
    pub(super) fn set_config(&mut self, config: Config) {
        self.invalid = config
            .invalid_profiles()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        self.status
            .retain(|id, _| config.profiles.iter().any(|profile| profile.id == *id));
        self.config = config;
        self.settle_selection();
    }

    /// Whether WinFsp can be used (FR-APP-06).
    pub(super) fn set_can_mount(&mut self, can_mount: bool) {
        self.can_mount = can_mount;
    }

    /// Whether the file can be written. If not, nothing can be added or deleted.
    pub(super) fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Whether anything can be added.
    pub(super) fn profile(&self, id: Uuid) -> Option<&Profile> {
        self.config.profiles.iter().find(|profile| profile.id == id)
    }

    pub(super) fn profiles(&self) -> &[Profile] {
        &self.config.profiles
    }

    pub(super) const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether connection fields are locked (FR-PROF-08).
    pub(super) fn is_in_use(&self, id: Uuid) -> bool {
        matches!(
            self.state_of(id),
            MountState::Connecting | MountState::Mounted | MountState::Unmounting
        )
    }

    pub(super) const fn can_add(&self) -> bool {
        !self.read_only
    }

    /// A state from the manager.
    pub(super) fn apply(&mut self, event: &MountEvent) {
        self.status
            .insert(event.id, (event.state.clone(), event.detail));
    }

    /// The selection.
    pub(super) const fn selected(&self) -> Option<&Key> {
        self.selected.as_ref()
    }

    /// Selects a line of the list.
    pub(super) fn select_place(&mut self, place: usize) {
        if let Some(item) = self.items().into_iter().nth(place) {
            self.selected = Some(item.key);
            self.selected_place = place;
        }
    }

    /// Moves the selection. From nothing selected, it goes to one end.
    pub(super) fn move_selection(&mut self, delta: i32) {
        let items = self.items();
        let Some(last) = items.len().checked_sub(1) else {
            return;
        };
        let place = match self.selected_place_in(&items) {
            Some(place) if delta < 0 => place.saturating_sub(1),
            Some(place) => (place + 1).min(last),
            None if delta < 0 => last,
            None => 0,
        };
        self.selected = Some(items[place].key.clone());
        self.selected_place = place;
    }

    fn selected_place_in(&self, items: &[Item]) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        items.iter().position(|item| &item.key == selected)
    }

    /// Where the selection is now, in [`items`](Self::items).
    pub(super) fn selected_place(&self) -> Option<usize> {
        self.selected_place_in(&self.items())
    }

    /// Keeps a selection valid once the list has changed: near where it was if its row has gone.
    fn settle_selection(&mut self) {
        if self.selected.is_none() {
            return;
        }
        let items = self.items();
        if let Some(place) = self.selected_place_in(&items) {
            self.selected_place = place;
            return;
        }
        let place = self.selected_place.min(items.len().saturating_sub(1));
        self.selected = items.get(place).map(|item| item.key.clone());
        self.selected_place = place;
    }

    /// Whether the file has no connections at all, which is the empty state.
    pub(super) fn is_empty(&self) -> bool {
        self.config.profiles.is_empty() && self.config.unreadable.is_empty()
    }

    /// An entry of [`Config::unreadable`], by its place there.
    pub(super) fn unreadable(&self, place: usize) -> Option<&UnreadableProfile> {
        self.config.unreadable.get(place)
    }

    /// A profile's state.
    pub(super) fn state_of(&self, id: Uuid) -> MountState {
        self.status
            .get(&id)
            .map_or(MountState::Disconnected, |(state, _)| state.clone())
    }

    /// The list as it is now: rows by the drive letter they show, then by name, then what could not be read.
    pub(super) fn items(&self) -> Vec<Item> {
        let mut shown: Vec<&Profile> = self.config.profiles.iter().collect();
        shown.sort_by_cached_key(|profile| self.sort_key(profile));

        let mut items = Vec::with_capacity(shown.len() + self.config.unreadable.len());
        for profile in shown {
            items.push(Item {
                key: Key::Profile(profile.id),
                body: Body::Profile(Box::new(self.profile_item(profile))),
            });
        }
        for (place, entry) in self.config.unreadable.iter().enumerate() {
            items.push(Item {
                key: Key::Unreadable(place),
                body: Body::Unreadable {
                    name: entry.name.clone(),
                    problem: entry.problem.clone(),
                    can_delete: !self.read_only,
                },
            });
        }
        items
    }

    /// By the letter the row shows, D to Z: the one a mounted profile holds, else the one it asks for,
    /// with rows that have neither last. Then by name ignoring case, with the exact text breaking
    /// ties, so that the order never depends on the file's. A row moves when it is mounted or
    /// unmounted. Only case is ignored in names: `Ż` comes after `z`, as sorting by language needs
    /// tables this program has no other use for.
    fn sort_key(&self, profile: &Profile) -> (bool, Option<DriveLetter>, String, String) {
        let live = self
            .status
            .get(&profile.id)
            .and_then(|(state, detail)| live_letter(state, *detail));
        let wanted = match profile.drive {
            DriveChoice::Letter(letter) => Some(letter),
            DriveChoice::Auto => None,
        };
        let letter = live.or(wanted);
        (
            letter.is_none(),
            letter,
            profile.name.to_lowercase(),
            profile.name.clone(),
        )
    }

    fn profile_item(&self, profile: &Profile) -> ProfileItem {
        let (state, detail) = self
            .status
            .get(&profile.id)
            .cloned()
            .unwrap_or((MountState::Disconnected, None));
        let invalid = self.invalid.contains(&profile.id);
        let live_letter = live_letter(&state, detail);
        let wanted = match profile.drive {
            DriveChoice::Auto => None,
            DriveChoice::Letter(letter) => Some(letter),
        };
        let line = match (&state, detail) {
            (MountState::Error(error), _) => Line::Failed(error.clone()),
            (
                MountState::Mounted | MountState::Unmounting,
                Some(MountDetail {
                    letter,
                    instead_of: Some(wanted),
                }),
            ) => Line::LetterMoved { letter, wanted },
            _ if invalid => Line::NeedsCorrecting,
            _ => Line::None,
        };
        let (primary, primary_enabled) = match state {
            MountState::Disconnected => (Primary::Mount, self.can_mount && !invalid),
            MountState::Error(_) => (Primary::Retry, self.can_mount && !invalid),
            MountState::Connecting => (Primary::Cancel, true),
            MountState::Mounted => (Primary::Unmount, true),
            MountState::Unmounting => (Primary::Unmount, false),
        };
        ProfileItem {
            id: profile.id,
            name: profile.name.clone(),
            address: address_of(profile),
            letter: live_letter.or(wanted),
            letter_is_live: live_letter.is_some(),
            can_open: matches!(state, MountState::Mounted) && live_letter.is_some(),
            state,
            line,
            primary,
            primary_enabled,
            can_delete: !self.read_only,
        }
    }

    /// What *Mount all* acts on: every unmounted profile (FR-MNT-09).
    pub(super) fn mountable(&self) -> Vec<Uuid> {
        if !self.can_mount {
            return Vec::new();
        }
        self.config
            .profiles
            .iter()
            .filter(|profile| {
                !self.invalid.contains(&profile.id)
                    && matches!(
                        self.state_of(profile.id),
                        MountState::Disconnected | MountState::Error(_)
                    )
            })
            .map(|profile| profile.id)
            .collect()
    }

    /// What *Unmount all* acts on: everything (FR-MNT-09). A profile that is connecting is included,
    /// as the manager takes that to mean cancel.
    pub(super) fn unmountable(&self) -> Vec<Uuid> {
        self.config
            .profiles
            .iter()
            .filter(|profile| {
                matches!(
                    self.state_of(profile.id),
                    MountState::Connecting | MountState::Mounted
                )
            })
            .map(|profile| profile.id)
            .collect()
    }

    /// The status bar's numbers.
    pub(super) fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for profile in &self.config.profiles {
            match self.state_of(profile.id) {
                MountState::Mounted => counts.mounted += 1,
                MountState::Connecting => counts.connecting += 1,
                MountState::Error(_) => counts.failed += 1,
                MountState::Disconnected | MountState::Unmounting => {}
            }
        }
        counts
    }
}

fn live_letter(state: &MountState, detail: Option<MountDetail>) -> Option<DriveLetter> {
    match state {
        MountState::Mounted | MountState::Unmounting => detail.map(|detail| detail.letter),
        MountState::Disconnected | MountState::Connecting | MountState::Error(_) => None,
    }
}

/// `user@host:path`, as in the mock-up, with a port other than 22 after the host.
fn address_of(profile: &Profile) -> String {
    let Protocol::Sftp(sftp) = &profile.protocol;
    let host = if sftp.host.contains(':') {
        format!("[{}]", sftp.host)
    } else {
        sftp.host.clone()
    };
    let (user, path) = (&sftp.username, &sftp.remote_path);
    if sftp.port == DEFAULT_PORT {
        format!("{user}@{host}:{path}")
    } else {
        format!("{user}@{host}:{}:{path}", sftp.port)
    }
}

#[cfg(test)]
mod tests {
    use fsm_core::profile::{SftpAuth, SftpProfile};
    use fsm_vfs::ConnectError;

    use super::*;

    fn profile(name: &str) -> Profile {
        let host = format!("{}.example.com", name.to_lowercase());
        let sftp = SftpProfile::new(host, "me", SftpAuth::Agent);
        Profile::new(name, Protocol::Sftp(sftp))
    }

    fn state_with(profiles: Vec<Profile>) -> ListState {
        let mut state = ListState::new();
        state.set_config(Config {
            profiles,
            ..Config::default()
        });
        state
    }

    fn letter(letter: char) -> DriveLetter {
        DriveLetter::new(letter).unwrap()
    }

    fn mounted(id: Uuid, on: char) -> MountEvent {
        MountEvent {
            id,
            state: MountState::Mounted,
            detail: Some(MountDetail {
                letter: letter(on),
                instead_of: None,
            }),
        }
    }

    fn in_state(id: Uuid, state: MountState) -> MountEvent {
        MountEvent {
            id,
            state,
            detail: None,
        }
    }

    fn names(state: &ListState) -> Vec<String> {
        state
            .items()
            .iter()
            .map(|item| match &item.body {
                Body::Profile(row) => row.name.clone(),
                Body::Unreadable { name, .. } => format!("? {}", name.as_deref().unwrap_or("-")),
            })
            .collect()
    }

    fn row(state: &ListState, id: Uuid) -> ProfileItem {
        state
            .items()
            .into_iter()
            .find_map(|item| match item.body {
                Body::Profile(row) if row.id == id => Some(*row),
                Body::Profile(_) | Body::Unreadable { .. } => None,
            })
            .unwrap()
    }

    #[test]
    fn rows_with_the_same_letter_are_by_name_ignoring_case() {
        let state = state_with(vec![
            profile("Zeta"),
            profile("beta"),
            profile("alpha"),
            profile("Alpha"),
        ]);
        assert_eq!(names(&state), ["Alpha", "alpha", "beta", "Zeta"]);
    }

    #[test]
    fn rows_are_by_the_letter_they_ask_for_with_auto_last() {
        let on = |name: &str, drive: DriveChoice| {
            let mut profile = profile(name);
            profile.drive = drive;
            profile
        };
        let state = state_with(vec![
            on("A-auto", DriveChoice::Auto),
            on("Z-on-z", DriveChoice::Letter(letter('Z'))),
            on("B-on-e", DriveChoice::Letter(letter('E'))),
            on("C-on-e", DriveChoice::Letter(letter('e'))),
            on("D-on-d", DriveChoice::Letter(letter('D'))),
        ]);
        assert_eq!(
            names(&state),
            ["D-on-d", "B-on-e", "C-on-e", "Z-on-z", "A-auto"]
        );
    }

    #[test]
    fn a_mounted_auto_row_is_where_its_letter_puts_it() {
        let auto = profile("A-auto");
        let id = auto.id;
        let mut fixed = profile("B-on-e");
        fixed.drive = DriveChoice::Letter(letter('E'));
        let mut state = state_with(vec![auto, fixed]);
        assert_eq!(names(&state), ["B-on-e", "A-auto"]);

        state.apply(&mounted(id, 'W'));
        assert_eq!(names(&state), ["B-on-e", "A-auto"]);

        state.apply(&mounted(id, 'D'));
        assert_eq!(names(&state), ["A-auto", "B-on-e"]);
    }

    #[test]
    fn each_state_has_the_button_of_the_table() {
        let saved = profile("a");
        let id = saved.id;
        let mut state = state_with(vec![saved]);
        let failed = MountState::Error(MountError::NoFreeLetter);
        for (event, primary, enabled) in [
            (in_state(id, MountState::Disconnected), Primary::Mount, true),
            (in_state(id, MountState::Connecting), Primary::Cancel, true),
            (mounted(id, 'Z'), Primary::Unmount, true),
            (
                in_state(id, MountState::Unmounting),
                Primary::Unmount,
                false,
            ),
            (in_state(id, failed), Primary::Retry, true),
        ] {
            state.apply(&event);
            let row = row(&state, id);
            assert_eq!(
                (row.primary, row.primary_enabled),
                (primary, enabled),
                "{event:?}"
            );
        }
    }

    #[test]
    fn without_winfsp_only_starting_a_mount_is_off() {
        let saved = profile("a");
        let id = saved.id;
        let mut state = state_with(vec![saved]);
        state.set_can_mount(false);
        assert!(!row(&state, id).primary_enabled);
        state.apply(&in_state(id, MountState::Error(MountError::WinFspMissing)));
        assert!(!row(&state, id).primary_enabled);
        assert!(state.mountable().is_empty());
        state.apply(&in_state(id, MountState::Connecting));
        assert!(row(&state, id).primary_enabled);
        state.apply(&mounted(id, 'Z'));
        assert!(row(&state, id).primary_enabled);
    }

    #[test]
    fn the_letter_is_the_live_one_when_mounted_and_the_preferred_one_otherwise() {
        let mut asked = profile("a");
        asked.drive = DriveChoice::Letter(letter('X'));
        let id = asked.id;
        let auto = profile("b");
        let auto_id = auto.id;
        let mut state = state_with(vec![asked, auto]);

        let row_a = row(&state, id);
        assert_eq!(
            (row_a.letter, row_a.letter_is_live),
            (Some(letter('X')), false)
        );
        assert!(!row_a.can_open);
        assert_eq!(row(&state, auto_id).letter, None);

        state.apply(&mounted(id, 'X'));
        state.apply(&mounted(auto_id, 'Z'));
        let row_a = row(&state, id);
        assert_eq!(
            (row_a.letter, row_a.letter_is_live),
            (Some(letter('X')), true)
        );
        assert!(row_a.can_open);
        let row_b = row(&state, auto_id);
        assert_eq!(
            (row_b.letter, row_b.letter_is_live),
            (Some(letter('Z')), true)
        );
    }

    #[test]
    fn a_moved_letter_and_an_error_each_have_a_line_and_the_row_stays_one_high() {
        let saved = profile("a");
        let id = saved.id;
        let mut state = state_with(vec![saved]);
        assert_eq!(row(&state, id).line, Line::None);
        assert_eq!(state.items()[0].height(), ROW_HEIGHT);

        state.apply(&MountEvent {
            id,
            state: MountState::Mounted,
            detail: Some(MountDetail {
                letter: letter('Y'),
                instead_of: Some(letter('Z')),
            }),
        });
        assert_eq!(
            row(&state, id).line,
            Line::LetterMoved {
                letter: letter('Y'),
                wanted: letter('Z'),
            }
        );
        assert_eq!(state.items()[0].height(), ROW_HEIGHT);

        let error = MountError::Connect(ConnectError::SftpUnavailable);
        state.apply(&in_state(id, MountState::Error(error.clone())));
        assert_eq!(row(&state, id).line, Line::Failed(error));
    }

    #[test]
    fn a_profile_that_breaks_a_rule_says_so_and_cannot_be_mounted() {
        let mut broken = profile("a");
        let Protocol::Sftp(sftp) = &mut broken.protocol;
        sftp.keepalive_secs = 9999;
        let id = broken.id;
        let state = state_with(vec![broken]);
        let row = row(&state, id);
        assert_eq!(row.line, Line::NeedsCorrecting);
        assert!(!row.primary_enabled);
        assert!(state.mountable().is_empty());
    }

    #[test]
    fn mount_all_takes_what_is_unmounted_and_unmount_all_what_is_up() {
        let idle = profile("idle");
        let failed = profile("failed");
        let up = profile("up");
        let connecting = profile("connecting");
        let (idle_id, failed_id, up_id, connecting_id) = (idle.id, failed.id, up.id, connecting.id);
        let mut state = state_with(vec![idle, failed, up, connecting]);
        state.apply(&in_state(
            failed_id,
            MountState::Error(MountError::NoFreeLetter),
        ));
        state.apply(&mounted(up_id, 'Z'));
        state.apply(&in_state(connecting_id, MountState::Connecting));
        assert_eq!(state.mountable(), [idle_id, failed_id]);
        assert_eq!(state.unmountable(), [up_id, connecting_id]);
    }

    #[test]
    fn the_counts_cover_every_state() {
        let (a, b, c, d) = (profile("a"), profile("b"), profile("c"), profile("d"));
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);
        let mut state = state_with(vec![a, b, c, d]);
        state.apply(&mounted(a_id, 'Z'));
        state.apply(&in_state(b_id, MountState::Connecting));
        state.apply(&in_state(c_id, MountState::Error(MountError::NoFreeLetter)));
        assert_eq!(
            state.counts(),
            Counts {
                mounted: 1,
                connecting: 1,
                failed: 1,
            }
        );
    }

    #[test]
    fn the_selection_moves_stops_at_the_ends_and_starts_from_an_end() {
        let mut state = state_with(vec![profile("a"), profile("b"), profile("c")]);
        assert_eq!(state.selected_place(), None);
        state.move_selection(1);
        assert_eq!(state.selected_place(), Some(0));
        state.move_selection(-1);
        assert_eq!(state.selected_place(), Some(0));
        state.move_selection(1);
        state.move_selection(1);
        state.move_selection(1);
        assert_eq!(state.selected_place(), Some(2));

        let mut state = state_with(vec![profile("a"), profile("b")]);
        state.move_selection(-1);
        assert_eq!(state.selected_place(), Some(1));

        let mut state = state_with(Vec::new());
        state.move_selection(1);
        assert_eq!(state.selected_place(), None);
    }

    #[test]
    fn the_selection_follows_its_row_and_stays_near_when_the_row_goes() {
        let (a, b, c) = (profile("a"), profile("b"), profile("c"));
        let (b_id, c_id) = (b.id, c.id);
        let mut state = state_with(vec![a.clone(), b.clone(), c.clone()]);
        state.select_place(1);
        assert_eq!(state.selected(), Some(&Key::Profile(b_id)));

        let first = profile("0");
        state.set_config(Config {
            profiles: vec![first.clone(), a.clone(), b, c.clone()],
            ..Config::default()
        });
        assert_eq!(state.selected(), Some(&Key::Profile(b_id)));
        assert_eq!(state.selected_place(), Some(2));

        state.set_config(Config {
            profiles: vec![first, a, c],
            ..Config::default()
        });
        assert_eq!(state.selected(), Some(&Key::Profile(c_id)));

        state.set_config(Config::default());
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn a_state_is_kept_across_a_save_and_goes_with_the_profile() {
        let saved = profile("a");
        let id = saved.id;
        let mut state = state_with(vec![saved.clone()]);
        state.apply(&mounted(id, 'Z'));

        let mut renamed = saved;
        renamed.name = "b".to_owned();
        state.set_config(Config {
            profiles: vec![renamed],
            ..Config::default()
        });
        assert_eq!(state.state_of(id), MountState::Mounted);

        state.set_config(Config::default());
        assert_eq!(state.state_of(id), MountState::Disconnected);
    }

    #[test]
    fn a_read_only_file_stops_adding_and_deleting_but_not_mounting() {
        let saved = profile("a");
        let id = saved.id;
        let mut state = state_with(vec![saved]);
        assert!(state.can_add());
        assert!(row(&state, id).can_delete);
        state.set_read_only(true);
        assert!(!state.can_add());
        let row = row(&state, id);
        assert!(!row.can_delete);
        assert!(row.primary_enabled);
        assert_eq!(state.mountable(), [id]);
    }

    #[test]
    fn an_address_shows_a_port_only_when_it_is_not_the_usual_one() {
        let mut saved = profile("a");
        let Protocol::Sftp(sftp) = &mut saved.protocol;
        sftp.host = "nas".to_owned();
        sftp.username = "marcin".to_owned();
        assert_eq!(address_of(&saved), "marcin@nas:~");

        let Protocol::Sftp(sftp) = &mut saved.protocol;
        sftp.port = 2222;
        sftp.remote_path = "/srv".to_owned();
        assert_eq!(address_of(&saved), "marcin@nas:2222:/srv");

        let Protocol::Sftp(sftp) = &mut saved.protocol;
        sftp.port = 22;
        sftp.host = "fe80::1".to_owned();
        assert_eq!(address_of(&saved), "marcin@[fe80::1]:/srv");
    }
}
