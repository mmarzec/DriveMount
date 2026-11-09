// SPDX-License-Identifier: GPL-3.0-or-later

//! The tray (spec §3.5, §7.6) through the window: a recording tray stands in for the real one, which
//! `drivemount_win::tray` tests on a desktop.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use drivemount_core::config::{Config, Settings};
use drivemount_core::manager::MountState;
use drivemount_core::profile::DriveChoice;
use drivemount_vfs::{ConnectError, MountError};
use drivemount_win::tray::{MenuEntry, MenuItem, TrayContent, TrayEvent};
use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use super::{Fixture, letter, profile};
use crate::ui::{TrayIcon, TraySink, UiCommand};

/// What the window last showed in the tray.
#[derive(Clone, Default)]
struct Recorded {
    shown: Rc<RefCell<Option<TrayContent>>>,
    in_tray: Rc<Cell<bool>>,
}

impl TraySink for Recorded {
    fn show(&self, content: TrayContent) {
        *self.shown.borrow_mut() = Some(content);
    }

    fn is_in_tray(&self) -> bool {
        self.in_tray.get()
    }
}

impl Recorded {
    fn content(&self) -> TrayContent {
        self.shown.borrow().clone().expect("nothing was shown")
    }

    fn icon(&self) -> TrayIcon {
        TrayIcon::from_file(self.content().icon).expect("not one of the four")
    }

    fn items(&self) -> Vec<MenuItem> {
        self.content()
            .menu
            .into_iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => Some(item),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    fn item(&self, text: &str) -> MenuItem {
        self.items()
            .into_iter()
            .find(|item| item.text == text)
            .unwrap_or_else(|| panic!("no item {text} in {:?}", self.items()))
    }
}

fn with_tray(fixture: &Fixture) -> Recorded {
    let tray = Recorded::default();
    tray.in_tray.set(true);
    fixture.ui.set_tray(Some(Box::new(tray.clone())));
    tray
}

fn failed() -> MountState {
    MountState::Error(MountError::Connect(ConnectError::HostKeyRejected))
}

/// FR-TRAY-01's acceptance: driven through the four states, the icon matches each.
#[test]
fn the_icon_follows_the_overall_state() {
    let (a, b) = (profile("A"), profile("B"));
    let (a_id, b_id) = (a.id, b.id);
    let fixture = Fixture::new(vec![a, b]);
    let tray = with_tray(&fixture);
    assert_eq!(tray.icon(), TrayIcon::Idle);

    fixture.set_state(a_id, MountState::Connecting, None);
    assert_eq!(tray.icon(), TrayIcon::Idle, "nothing is mounted yet");
    fixture.set_state(a_id, MountState::Mounted, Some('X'));
    assert_eq!(tray.icon(), TrayIcon::Mounted);
    fixture.set_state(b_id, MountState::Reconnecting, Some('Y'));
    assert_eq!(tray.icon(), TrayIcon::Reconnecting);
    fixture.set_state(a_id, failed(), None);
    assert_eq!(tray.icon(), TrayIcon::Error);
    fixture.set_state(a_id, MountState::Disconnected, None);
    fixture.set_state(b_id, MountState::Disconnected, None);
    assert_eq!(tray.icon(), TrayIcon::Idle);
}

/// FR-TRAY-02: the tooltip's counts are the status bar's.
#[test]
fn the_tooltip_has_the_status_bars_counts() {
    let names = ["A", "B", "C", "D", "E"];
    let profiles: Vec<_> = names.iter().map(|name| profile(name)).collect();
    let ids: Vec<_> = profiles.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(profiles);
    let tray = with_tray(&fixture);
    assert_eq!(tray.content().tooltip, "DriveMount");

    fixture.set_state(ids[0], MountState::Mounted, Some('P'));
    fixture.set_state(ids[1], MountState::Mounted, Some('Q'));
    fixture.set_state(ids[2], MountState::Reconnecting, Some('R'));
    assert_eq!(
        tray.content().tooltip,
        "DriveMount: 2 mounted, 1 reconnecting"
    );
    fixture.set_state(ids[3], MountState::Connecting, None);
    fixture.set_state(ids[4], failed(), None);
    let status = fixture.ui.window().get_status_text();
    assert_eq!(
        status,
        "2 mounted · 1 connecting · 1 reconnecting · 1 failed"
    );
    assert_eq!(
        tray.content().tooltip,
        format!("DriveMount: {}", status.replace(" · ", ", "))
    );
}

/// Spec §7.6: a line per profile, in the window's order, with a check mark when it is up.
#[test]
fn the_menu_has_a_line_per_profile_then_its_own_items() {
    let mut production = profile("Production");
    production.drive = DriveChoice::Letter(letter('Z'));
    let mut staging = profile("Staging");
    staging.drive = DriveChoice::Letter(letter('Y'));
    let mut dev = profile("Dev box");
    dev.drive = DriveChoice::Letter(letter('W'));
    let auto = profile("R&D");
    let ids = (production.id, staging.id, dev.id, auto.id);
    let fixture = Fixture::new(vec![production, staging, dev, auto]);
    let tray = with_tray(&fixture);
    fixture.set_state(ids.0, MountState::Mounted, Some('Z'));
    fixture.set_state(ids.1, MountState::Connecting, None);
    fixture.set_state(ids.2, MountState::Reconnecting, Some('W'));

    let lines: Vec<_> = tray
        .content()
        .menu
        .into_iter()
        .map(|entry| match entry {
            MenuEntry::Item(item) => format!(
                "{}{}{}{}",
                if item.checked { "✓ " } else { "  " },
                item.text,
                if item.detail.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", item.detail)
                },
                if item.enabled { "" } else { " (off)" },
            ),
            MenuEntry::Separator => "---".to_owned(),
        })
        .collect();
    assert_eq!(
        lines,
        [
            "✓ Dev box (W:) [Reconnecting…]",
            "  Staging (Y:) [Connecting…]",
            "✓ Production (Z:)",
            "  R&D",
            "---",
            "  Unmount all",
            "  Open DriveMount",
            "---",
            "  Quit",
        ]
    );
    assert!(tray.item("Open DriveMount").default);
}

#[test]
fn a_failed_profile_says_so_and_what_cannot_be_done_is_off() {
    let (a, b) = (profile("A"), profile("B"));
    let a_id = a.id;
    let fixture = Fixture::new(vec![a, b]);
    let tray = with_tray(&fixture);
    assert!(!tray.item("Unmount all").enabled, "nothing is up");

    fixture.set_state(a_id, failed(), None);
    assert_eq!(tray.item("A").detail, "Failed");
    assert!(tray.item("A").enabled, "it can be retried");

    fixture.ui.set_winfsp_ready(false);
    assert!(!tray.item("B").enabled, "nothing can mount without WinFsp");
}

/// FR-TRAY-04's acceptance: a profile toggles from the tray with the window left as it is.
#[test]
fn choosing_a_profile_mounts_or_unmounts_it_without_the_window() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(vec![a]);
    let tray = with_tray(&fixture);
    fixture.ui.window().hide().unwrap();

    let chosen = tray.item("A").id;
    fixture.ui.tray_event(TrayEvent::Chosen(chosen));
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(id)]);
    assert!(!fixture.ui.is_window_up(), "the window stays hidden");

    fixture.set_state(id, MountState::Connecting, None);
    fixture.ui.tray_event(TrayEvent::Chosen(chosen));
    assert_eq!(fixture.take_sent(), [UiCommand::Cancel(id)]);

    fixture.set_state(id, MountState::Mounted, Some('M'));
    // Its text has the letter now, and its id is the same.
    assert_eq!(tray.item("A (M:)").id, chosen);
    fixture.ui.tray_event(TrayEvent::Chosen(chosen));
    assert_eq!(fixture.take_sent(), [UiCommand::Unmount(id)]);

    fixture.set_state(id, MountState::Unmounting, Some('M'));
    fixture.ui.tray_event(TrayEvent::Chosen(chosen));
    assert!(fixture.take_sent().is_empty(), "it is already going");
}

#[test]
fn unmount_all_open_and_quit_do_what_they_say() {
    let (a, b) = (profile("A"), profile("B"));
    let (a_id, b_id) = (a.id, b.id);
    let fixture = Fixture::new(vec![a, b]);
    let tray = with_tray(&fixture);
    fixture.set_state(a_id, MountState::Mounted, Some('M'));
    fixture.set_state(b_id, MountState::Reconnecting, Some('N'));

    fixture
        .ui
        .tray_event(TrayEvent::Chosen(tray.item("Unmount all").id));
    let mut sent = fixture.take_sent();
    sent.sort_by_key(|command| format!("{command:?}"));
    let mut expected = vec![UiCommand::Unmount(a_id), UiCommand::Unmount(b_id)];
    expected.sort_by_key(|command| format!("{command:?}"));
    assert_eq!(sent, expected);

    fixture.ui.window().hide().unwrap();
    fixture
        .ui
        .tray_event(TrayEvent::Chosen(tray.item("Open DriveMount").id));
    assert!(fixture.ui.is_window_up());

    fixture
        .ui
        .tray_event(TrayEvent::Chosen(tray.item("Quit").id));
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);

    fixture.ui.tray_event(TrayEvent::Chosen(9_999));
    assert!(fixture.take_sent().is_empty(), "no menu had it");
}

/// FR-TRAY-03: a visible window is hidden, and a hidden or minimized one is shown.
#[test]
fn a_click_on_the_icon_hides_or_shows_the_window() {
    let fixture = Fixture::new(Vec::new());
    let _tray = with_tray(&fixture);
    assert!(fixture.ui.is_window_up());

    fixture.ui.tray_event(TrayEvent::Clicked);
    assert!(!fixture.ui.window().window().is_visible());
    fixture.ui.tray_event(TrayEvent::Clicked);
    assert!(fixture.ui.is_window_up());

    fixture.ui.window().window().set_minimized(true);
    fixture.ui.tray_event(TrayEvent::Clicked);
    assert!(fixture.ui.is_window_up(), "a minimized window comes back");
    assert!(fixture.take_sent().is_empty());
}

/// FR-APP-02: with `close_to_tray` on, × hides the window; with it off, × quits. With no icon in the
/// tray the window could not be got back, so × quits and Esc minimizes.
#[test]
fn closing_hides_to_the_tray_only_when_set_to_and_there_is_one() {
    let fixture = Fixture::new(Vec::new());
    let tray = with_tray(&fixture);

    fixture.dispatch(WindowEvent::CloseRequested);
    assert!(fixture.take_sent().is_empty());
    assert!(!fixture.ui.window().window().is_visible());

    fixture.ui.tray_event(TrayEvent::Clicked);
    fixture.ui.set_config(Config {
        settings: Settings {
            close_to_tray: false,
            ..Settings::default()
        },
        ..Config::default()
    });
    fixture.dispatch(WindowEvent::CloseRequested);
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);

    fixture.ui.set_config(Config::default());
    tray.in_tray.set(false);
    fixture.dispatch(WindowEvent::CloseRequested);
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);
}

#[test]
fn escape_hides_to_the_tray() {
    let fixture = Fixture::new(Vec::new());
    let _tray = with_tray(&fixture);
    fixture.key(Key::Escape);
    assert!(fixture.take_sent().is_empty());
    assert!(!fixture.ui.window().window().is_visible());
    assert!(!fixture.ui.window().window().is_minimized());
}
