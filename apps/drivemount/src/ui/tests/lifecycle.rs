// SPDX-License-Identifier: GPL-3.0-or-later

//! The window's part of the app's lifecycle (spec §3.7): the quit question (FR-APP-04), the crash
//! notice (FR-APP-07), and coming to the front for another start (FR-APP-01).

use drivemount_core::config::{Config, Settings};
use drivemount_core::manager::MountState;
use drivemount_win::tray::TrayEvent;
use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use super::tray::with_tray;
use super::{Fixture, profile};
use crate::ui::UiCommand;

fn closing_quits() -> Config {
    Config {
        settings: Settings {
            close_to_tray: false,
            ..Settings::default()
        },
        ..Config::default()
    }
}

fn question(fixture: &Fixture) -> String {
    fixture.ui.window().get_question().to_string()
}

/// FR-APP-04's acceptance: with drives up, quitting asks how many; Cancel keeps everything, and the
/// answer yes quits, which unmounts them (`app::run`).
#[test]
fn quitting_with_drives_up_asks_first() {
    let (a, b, c) = (profile("A"), profile("B"), profile("C"));
    let ids = (a.id, b.id);
    let fixture = Fixture::new(Vec::new());
    fixture.ui.set_config(Config {
        profiles: vec![a, b, c],
        ..closing_quits()
    });
    fixture.set_state(ids.0, MountState::Mounted, Some('X'));

    fixture.dispatch(WindowEvent::CloseRequested);
    assert!(fixture.take_sent().is_empty(), "not before the answer");
    assert_eq!(question(&fixture), "Unmount 1 drive and quit?");
    fixture.press("Cancel");
    assert!(fixture.take_sent().is_empty());
    assert_eq!(question(&fixture), "");
    assert!(fixture.ui.is_window_up(), "everything as it was");

    fixture.set_state(ids.1, MountState::Reconnecting, Some('Y'));
    fixture.dispatch(WindowEvent::CloseRequested);
    assert_eq!(question(&fixture), "Unmount 2 drives and quit?");
    fixture.key(Key::Escape);
    assert!(fixture.take_sent().is_empty(), "Esc is Cancel");

    fixture.dispatch(WindowEvent::CloseRequested);
    fixture.press("Unmount and quit");
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);
    assert_eq!(question(&fixture), "");
}

#[test]
fn quitting_with_nothing_up_asks_nothing() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(Vec::new());
    fixture.ui.set_config(Config {
        profiles: vec![a],
        ..closing_quits()
    });
    fixture.set_state(id, MountState::Mounted, Some('X'));
    fixture.set_state(id, MountState::Disconnected, None);

    fixture.dispatch(WindowEvent::CloseRequested);
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);
    assert_eq!(question(&fixture), "");
}

/// The tray's *Quit* with the window hidden: the question needs the window, so it comes up.
#[test]
fn quitting_from_the_tray_brings_the_question_up() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(vec![a]);
    let tray = with_tray(&fixture);
    fixture.set_state(id, MountState::Connecting, None);
    fixture.ui.window().hide().unwrap();

    fixture
        .ui
        .tray_event(TrayEvent::Chosen(tray.item("Quit").id));
    assert!(fixture.take_sent().is_empty());
    assert_eq!(question(&fixture), "Unmount 1 drive and quit?");
    assert!(fixture.ui.is_window_up());

    fixture.press("Unmount and quit");
    assert_eq!(fixture.take_sent(), [UiCommand::Quit]);
}

/// The delete question still says Delete after a quit question.
#[test]
fn the_delete_question_keeps_its_own_button() {
    let a = profile("A");
    let id = a.id;
    let fixture = Fixture::new(Vec::new());
    fixture.ui.set_config(Config {
        profiles: vec![a],
        ..closing_quits()
    });
    fixture.set_state(id, MountState::Mounted, Some('X'));
    fixture.dispatch(WindowEvent::CloseRequested);
    fixture.press("Cancel");

    fixture.key(Key::DownArrow);
    fixture.key(Key::Delete);
    assert!(question(&fixture).starts_with("Unmount and delete"));
    fixture.press("Delete");
    assert_eq!(fixture.take_sent(), [UiCommand::Delete(id)]);
}

/// FR-APP-07: the notice names the report, and *Open logs folder* opens it and puts it away.
#[test]
fn a_crash_report_is_noticed_until_the_logs_are_opened() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ui
        .set_crash_report(Some("crash-2026-11-09T171153.042Z.txt"));
    let text = fixture.ui.window().get_crash_text();
    assert!(
        text.contains("closed unexpectedly") && text.contains("crash-2026-11-09T171153.042Z.txt"),
        "{text}"
    );
    assert!(fixture.labelled(&text).is_some(), "not on screen");

    fixture.press("Open logs folder");
    assert_eq!(fixture.take_sent(), [UiCommand::OpenLogsFolder]);
    assert_eq!(fixture.ui.window().get_crash_text(), "");
}

/// FR-APP-07: the × puts the crash notice away without opening anything.
#[test]
fn a_crash_report_can_be_closed() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ui
        .set_crash_report(Some("crash-2026-11-09T171153.042Z.txt"));
    fixture.press("Close");
    assert_eq!(fixture.take_sent(), []);
    assert_eq!(fixture.ui.window().get_crash_text(), "");
}

/// FR-APP-01: another start brings the window up, from hidden or minimized.
#[test]
fn another_start_brings_the_window_up() {
    let fixture = Fixture::new(Vec::new());
    fixture.ui.window().hide().unwrap();
    fixture.ui.bring_to_front();
    assert!(fixture.ui.is_window_up());

    fixture.ui.window().window().set_minimized(true);
    fixture.ui.bring_to_front();
    assert!(fixture.ui.is_window_up());
    assert!(fixture.take_sent().is_empty());
}
