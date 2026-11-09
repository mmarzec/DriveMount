// SPDX-License-Identifier: GPL-3.0-or-later

//! What the tray shows (spec §3.5, §7.6), from the same list state as the window: the icon for the
//! overall state, the tooltip with the status bar's counts, and the menu.

use std::collections::HashMap;

use drivemount_core::manager::MountState;
use drivemount_win::tray::{MenuEntry, MenuItem, TrayContent};
use slint::SharedString;
use uuid::Uuid;

use super::generated::Texts;
use super::list::{Body, Counts, ListState};

/// The menu's own items. Profiles come after these.
const UNMOUNT_ALL: u32 = 1;
const OPEN: u32 = 2;
const QUIT: u32 = 3;
const FIRST_PROFILE: u32 = 16;

/// What an item of the tray's menu does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayChoice {
    /// What the profile's row button would do: mount, cancel or unmount.
    Toggle(Uuid),
    /// *Unmount all*.
    UnmountAll,
    /// *Open DriveMount*.
    Open,
    /// *Quit*.
    Quit,
}

/// The icon's four variants (FR-TRAY-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIcon {
    /// Nothing mounted.
    Idle,
    /// Something mounted, and nothing reconnecting or failed.
    Mounted,
    /// Something reconnecting, and nothing failed.
    Reconnecting,
    /// Something failed.
    Error,
}

impl TrayIcon {
    /// Which one the counts call for. A failure matters most, as it needs the user.
    pub(super) const fn of(counts: Counts) -> Self {
        if counts.failed > 0 {
            Self::Error
        } else if counts.reconnecting > 0 {
            Self::Reconnecting
        } else if counts.mounted > 0 {
            Self::Mounted
        } else {
            Self::Idle
        }
    }

    /// The `.ico` file, drawn by `scripts/make-tray-icons.py`.
    #[must_use]
    pub const fn file(self) -> &'static [u8] {
        match self {
            Self::Idle => include_bytes!("../../ui/tray/idle.ico"),
            Self::Mounted => include_bytes!("../../ui/tray/mounted.ico"),
            Self::Reconnecting => include_bytes!("../../ui/tray/reconnecting.ico"),
            Self::Error => include_bytes!("../../ui/tray/error.ico"),
        }
    }

    /// Which one a file is, for tests.
    #[must_use]
    pub fn from_file(file: &[u8]) -> Option<Self> {
        [Self::Idle, Self::Mounted, Self::Reconnecting, Self::Error]
            .into_iter()
            .find(|icon| icon.file() == file)
    }
}

/// Menu ids for profiles. A profile keeps its id while the app runs, so that a menu opened before a
/// change still does what it said.
#[derive(Debug, Default)]
pub(super) struct TrayIds {
    profiles: HashMap<Uuid, u32>,
}

impl TrayIds {
    fn of(&mut self, id: Uuid) -> u32 {
        let next =
            FIRST_PROFILE + u32::try_from(self.profiles.len()).unwrap_or(u32::MAX - FIRST_PROFILE);
        *self.profiles.entry(id).or_insert(next)
    }

    /// What a chosen id means. `None` for one no menu had.
    pub(super) fn choice(&self, chosen: u32) -> Option<TrayChoice> {
        match chosen {
            UNMOUNT_ALL => Some(TrayChoice::UnmountAll),
            OPEN => Some(TrayChoice::Open),
            QUIT => Some(TrayChoice::Quit),
            _ => self
                .profiles
                .iter()
                .find(|(_, id)| **id == chosen)
                .map(|(profile, _)| TrayChoice::Toggle(*profile)),
        }
    }
}

/// The status bar's counts as texts, the parts that are not zero, in its order.
pub(super) fn count_texts(texts: &Texts<'_>, counts: Counts) -> Vec<SharedString> {
    let number = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
    [
        (counts.mounted > 0).then(|| texts.invoke_count_mounted(number(counts.mounted))),
        (counts.connecting > 0).then(|| texts.invoke_count_connecting(number(counts.connecting))),
        (counts.reconnecting > 0)
            .then(|| texts.invoke_count_reconnecting(number(counts.reconnecting))),
        (counts.failed > 0).then(|| texts.invoke_count_failed(number(counts.failed))),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn join(parts: &[SharedString], separator: &str) -> String {
    parts
        .iter()
        .map(SharedString::as_str)
        .collect::<Vec<_>>()
        .join(separator)
}

/// The tooltip: "DriveMount: 2 mounted, 1 reconnecting" (FR-TRAY-02), or "DriveMount" alone.
pub(super) fn tooltip(texts: &Texts<'_>, counts: Counts) -> String {
    let parts = count_texts(texts, counts);
    if parts.is_empty() {
        texts.invoke_tray_tooltip_idle().into()
    } else {
        let separator = texts.invoke_tray_count_separator();
        texts
            .invoke_tray_tooltip(join(&parts, &separator).into())
            .into()
    }
}

/// Everything the tray shows.
pub(super) fn content(texts: &Texts<'_>, list: &ListState, ids: &mut TrayIds) -> TrayContent {
    let counts = list.counts();
    let mut menu = Vec::new();
    for item in list.items() {
        let Body::Profile(row) = item.body else {
            continue;
        };
        let text = match row.letter {
            Some(letter) => texts
                .invoke_tray_profile((&row.name).into(), letter.to_string().into())
                .into(),
            None => row.name.clone(),
        };
        let detail = match row.state {
            MountState::Connecting => texts.invoke_tray_connecting(),
            MountState::Reconnecting => texts.invoke_tray_reconnecting(),
            MountState::Unmounting => texts.invoke_tray_unmounting(),
            MountState::Error(_) => texts.invoke_tray_failed(),
            MountState::Disconnected | MountState::Mounted => SharedString::new(),
        };
        menu.push(MenuEntry::Item(MenuItem {
            id: ids.of(row.id),
            text,
            detail: detail.into(),
            checked: matches!(row.state, MountState::Mounted | MountState::Reconnecting),
            enabled: row.primary_enabled,
            default: false,
        }));
    }
    if !menu.is_empty() {
        menu.push(MenuEntry::Separator);
    }
    let own = |id, text: SharedString, enabled, default| {
        MenuEntry::Item(MenuItem {
            id,
            text: text.into(),
            detail: String::new(),
            checked: false,
            enabled,
            default,
        })
    };
    menu.extend([
        own(
            UNMOUNT_ALL,
            texts.invoke_tray_unmount_all(),
            !list.unmountable().is_empty(),
            false,
        ),
        own(OPEN, texts.invoke_tray_open(), true, true),
        MenuEntry::Separator,
        own(QUIT, texts.invoke_tray_quit(), true, false),
    ]);
    TrayContent {
        icon: TrayIcon::of(counts).file(),
        tooltip: tooltip(texts, counts),
        menu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(mounted: usize, connecting: usize, reconnecting: usize, failed: usize) -> Counts {
        Counts {
            mounted,
            connecting,
            reconnecting,
            failed,
        }
    }

    #[test]
    fn the_icon_shows_the_state_that_matters_most() {
        assert_eq!(TrayIcon::of(counts(0, 0, 0, 0)), TrayIcon::Idle);
        assert_eq!(TrayIcon::of(counts(0, 2, 0, 0)), TrayIcon::Idle);
        assert_eq!(TrayIcon::of(counts(1, 1, 0, 0)), TrayIcon::Mounted);
        assert_eq!(TrayIcon::of(counts(3, 0, 1, 0)), TrayIcon::Reconnecting);
        assert_eq!(TrayIcon::of(counts(3, 0, 1, 1)), TrayIcon::Error);
        assert_eq!(TrayIcon::of(counts(0, 0, 0, 1)), TrayIcon::Error);
    }

    #[test]
    fn each_icon_has_a_file_of_its_own() {
        for icon in [
            TrayIcon::Idle,
            TrayIcon::Mounted,
            TrayIcon::Reconnecting,
            TrayIcon::Error,
        ] {
            assert_eq!(TrayIcon::from_file(icon.file()), Some(icon));
        }
    }

    #[test]
    fn a_profile_keeps_its_id_and_ids_say_what_they_do() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ids = TrayIds::default();
        let id_a = ids.of(a);
        let id_b = ids.of(b);
        assert_ne!(id_a, id_b);
        assert_eq!(ids.of(a), id_a);
        assert!(id_a >= FIRST_PROFILE && id_b >= FIRST_PROFILE);
        assert_eq!(ids.choice(id_b), Some(TrayChoice::Toggle(b)));
        assert_eq!(ids.choice(UNMOUNT_ALL), Some(TrayChoice::UnmountAll));
        assert_eq!(ids.choice(OPEN), Some(TrayChoice::Open));
        assert_eq!(ids.choice(QUIT), Some(TrayChoice::Quit));
        assert_eq!(ids.choice(id_b + 1), None);
    }
}
