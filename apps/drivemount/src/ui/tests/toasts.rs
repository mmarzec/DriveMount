// SPDX-License-Identifier: GPL-3.0-or-later

//! Toasts (spec §3.6) through the window: a recording sink stands in for Windows, which
//! `drivemount_win::toast` tests.

use std::cell::RefCell;
use std::rc::Rc;

use drivemount_core::config::{Config, Settings};
use drivemount_core::manager::{MountDetail, MountEvent, MountState};
use drivemount_core::profile::DriveChoice;
use drivemount_vfs::{ConnectError, MountError};
use drivemount_win::toast::Toast;
use slint::ComponentHandle;

use super::{Fixture, letter, profile};
use crate::ui::errors::shown_error;
use crate::ui::{Texts, ToastSink};

#[derive(Clone, Default)]
struct Recorded(Rc<RefCell<Vec<Toast>>>);

impl ToastSink for Recorded {
    fn show(&self, toast: Toast) {
        self.0.borrow_mut().push(toast);
    }
}

impl Recorded {
    fn take(&self) -> Vec<Toast> {
        self.0.borrow_mut().drain(..).collect()
    }
}

fn with_toasts(fixture: &Fixture) -> Recorded {
    let toasts = Recorded::default();
    fixture.ui.set_toasts(Some(Box::new(toasts.clone())));
    toasts
}

fn auth() -> MountError {
    MountError::Connect(ConnectError::Auth {
        user: "me".to_owned(),
        host: "host".to_owned(),
    })
}

fn lost() -> MountError {
    MountError::Connect(ConnectError::ConnectionLost)
}

/// FR-NOT-01's acceptance: each of the four events is exactly one toast, naming the profile, with the
/// error text of spec §7.8.
#[test]
fn each_of_the_four_events_is_one_toast_with_the_name_and_the_text() {
    let mut dev = profile("Dev box");
    dev.drive = DriveChoice::Letter(letter('Z'));
    let id = dev.id;
    let fixture = Fixture::new(vec![dev]);
    let toasts = with_toasts(&fixture);
    let texts = fixture.ui.window().global::<Texts>();
    let expected = |error: &MountError| {
        let shown = shown_error(&texts, error);
        vec![shown.text.to_string(), shown.hint.to_string()]
    };

    // A mount fails.
    fixture.set_state(id, MountState::Connecting, None);
    fixture.set_state(id, MountState::Error(auth()), None);
    let shown = toasts.take();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].title, "Dev box");
    assert_eq!(shown[0].lines, expected(&auth()));
    assert_eq!(shown[0].launch, format!("profile={id}"));
    assert_eq!(shown[0].tag, id.to_string());

    // The connection is lost without auto-reconnect.
    fixture.set_state(id, MountState::Connecting, None);
    fixture.set_state(id, MountState::Mounted, Some('Z'));
    fixture.set_state(id, MountState::Error(lost()), None);
    let shown = toasts.take();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].lines, expected(&lost()));

    // Reconnecting stops.
    fixture.set_state(id, MountState::Connecting, None);
    fixture.set_state(id, MountState::Mounted, Some('Z'));
    fixture.set_state(id, MountState::Reconnecting, Some('Z'));
    fixture.set_state(id, MountState::Error(auth()), None);
    assert_eq!(toasts.take().len(), 1);

    // The letter was taken.
    fixture.set_state(id, MountState::Connecting, None);
    let moved = MountEvent {
        id,
        state: MountState::Mounted,
        detail: Some(MountDetail {
            letter: letter('Y'),
            instead_of: Some(letter('Z')),
        }),
    };
    fixture.ui.apply_event(&moved);
    let shown = toasts.take();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].title, "Dev box");
    assert_eq!(shown[0].lines, ["Mounted on Y: because Z: was in use."]);
    fixture.ui.apply_event(&MountEvent {
        state: MountState::Reconnecting,
        ..moved
    });
    assert!(toasts.take().is_empty(), "nothing new");
}

/// FR-NOT-02's acceptance: mount-on-start of 5 profiles, and a reconnect that works, are no toasts.
#[test]
fn mounts_and_reconnects_that_work_are_no_toasts() {
    let profiles: Vec<_> = ["A", "B", "C", "D", "E"]
        .iter()
        .map(|name| profile(name))
        .collect();
    let ids: Vec<_> = profiles.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(profiles);
    let toasts = with_toasts(&fixture);

    for id in &ids {
        fixture.set_state(*id, MountState::Connecting, None);
    }
    for (id, on) in ids.iter().zip(['P', 'Q', 'R', 'S', 'T']) {
        fixture.set_state(*id, MountState::Mounted, Some(on));
    }
    fixture.set_state(ids[0], MountState::Reconnecting, Some('P'));
    fixture.set_state(ids[0], MountState::Mounted, Some('P'));
    fixture.set_state(ids[1], MountState::Unmounting, Some('Q'));
    fixture.set_state(ids[1], MountState::Disconnected, None);
    assert!(toasts.take().is_empty());
}

/// FR-NOT-03's acceptance: with notifications off, no toasts, and the row still has the error.
#[test]
fn with_notifications_off_there_are_none_and_the_row_still_says_it() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(Vec::new());
    fixture.ui.set_config(Config {
        profiles: vec![a],
        settings: Settings {
            notifications: false,
            ..Settings::default()
        },
        ..Config::default()
    });
    let toasts = with_toasts(&fixture);

    fixture.set_state(id, MountState::Connecting, None);
    fixture.set_state(id, MountState::Error(auth()), None);
    assert!(toasts.take().is_empty());
    let texts = fixture.ui.window().global::<Texts>();
    assert_eq!(fixture.entry(0).line, shown_error(&texts, &auth()).line());
}

/// FR-SET-08: toasts are silent by default, and play the sound when it is turned on.
#[test]
fn toasts_are_silent_unless_the_sound_is_on() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(vec![a.clone()]);
    let toasts = with_toasts(&fixture);
    fixture.set_state(id, MountState::Error(auth()), None);
    assert!(toasts.take()[0].silent, "silent by default");

    fixture.ui.set_config(Config {
        profiles: vec![a],
        settings: Settings {
            notification_sound: true,
            ..Settings::default()
        },
        ..Config::default()
    });
    fixture.set_state(id, MountState::Error(lost()), None);
    assert!(!toasts.take()[0].silent);
}

/// FR-NOT-04's acceptance: a click on the toast for "Dev box" shows the window with that row selected.
#[test]
fn a_click_shows_the_window_with_the_profile_selected() {
    let (a, dev, c) = (profile("A"), profile("Dev box"), profile("C"));
    let (a_id, dev_id) = (a.id, dev.id);
    let fixture = Fixture::new(vec![a, dev, c]);
    let toasts = with_toasts(&fixture);
    fixture.set_state(dev_id, MountState::Error(auth()), None);
    let launch = toasts.take().remove(0).launch;
    fixture.ui.window().hide().unwrap();

    fixture.ui.toast_clicked(&launch);
    assert!(fixture.ui.is_window_up());
    let selected = usize::try_from(fixture.ui.window().get_selected_index()).unwrap();
    assert_eq!(fixture.entry(selected).name, "Dev box");

    // With a page open, it shows the toast's profile.
    fixture.ui.toast_clicked(&format!("profile={a_id}"));
    fixture.key(" ");
    assert!(fixture.ui.window().get_page_open());
    assert_eq!(fixture.ui.window().get_page_view().name, "A");
    fixture.ui.toast_clicked(&launch);
    assert_eq!(fixture.ui.window().get_page_view().name, "Dev box");
}

#[test]
fn a_click_for_a_profile_that_has_gone_only_shows_the_window() {
    let a = profile("A");
    let fixture = Fixture::new(vec![a]);
    fixture.ui.window().window().set_minimized(true);
    fixture
        .ui
        .toast_clicked(&format!("profile={}", uuid::Uuid::new_v4()));
    assert!(fixture.ui.is_window_up());
    assert_eq!(fixture.ui.window().get_selected_index(), -1);
    fixture.ui.toast_clicked("something else");
    assert!(fixture.ui.is_window_up());
    assert!(fixture.take_sent().is_empty());
}
