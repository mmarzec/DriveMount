// SPDX-License-Identifier: GPL-3.0-or-later

//! What M3-04 adds to the window for SMB: other network drives as *External* rows (FR-SMB-09,
//! FR-SET-09), the question for open files (FR-SMB-04), and SMB rows without WinFsp (FR-SMB-08).

use std::cell::RefCell;
use std::rc::Rc;

use drivemount_core::config::Config;
use drivemount_core::manager::MountState;
use drivemount_core::profile::{DriveChoice, Profile, Protocol, SmbAuth, SmbProfile};
use drivemount_smb::{DriveStatus, OtherDrive};
use slint::Model;
use slint::platform::Key;

use super::super::{EntryKind, UiCommand};
use super::{Fixture, letter, profile};

fn other(on: char, unc: &str) -> OtherDrive {
    OtherDrive {
        letter: letter(on),
        unc: unc.to_owned(),
        provider: "Microsoft Windows Network".to_owned(),
        status: DriveStatus::Mapped,
        user: Some(r"CORP\anna".to_owned()),
        smb: true,
        remembered: true,
    }
}

fn share(name: &str) -> Profile {
    Profile::new(
        name,
        Protocol::Smb(SmbProfile::new("nas", "media", "me", SmbAuth::Ask)),
    )
}

/// What Windows lists, which a test changes as `net use` would.
fn listing(fixture: &Fixture, drives: Vec<OtherDrive>) -> Rc<RefCell<Vec<OtherDrive>>> {
    let listed = Rc::new(RefCell::new(drives));
    let source = Rc::clone(&listed);
    fixture
        .ui
        .set_other_drives(move || Some(source.borrow().clone()));
    listed
}

fn kinds_and_names(fixture: &Fixture) -> Vec<(EntryKind, String)> {
    let entries = fixture.ui.window().get_entries();
    (0..entries.row_count())
        .map(|place| {
            let entry = entries.row_data(place).unwrap();
            (entry.kind, entry.name.to_string())
        })
        .collect()
}

/// FR-SMB-09: shown at once, by letter among the profiles, with no button.
#[test]
fn another_programs_drive_is_an_external_row_with_no_button() {
    let mut on_x = profile("X box");
    on_x.drive = DriveChoice::Letter(letter('X'));
    let fixture = Fixture::new(vec![on_x]);
    listing(&fixture, vec![other('W', r"\\nas\photos")]);
    assert_eq!(
        kinds_and_names(&fixture),
        [
            (EntryKind::External, r"\\nas\photos".to_owned()),
            (EntryKind::Profile, "X box".to_owned()),
        ]
    );
    let row = fixture.entry(0);
    assert_eq!(row.letter, "W:");
    assert!(row.letter_is_live);
    assert_eq!(row.primary, super::super::PrimaryAction::None);
    assert!(row.line.contains("Mapped outside DriveMount"));
    assert!(row.line.contains(r"as CORP\anna"));
    assert_eq!(
        fixture.ui.window().get_status_text(),
        "Nothing mounted",
        "never counted"
    );
}

/// Spec §7.1: Enter, Ctrl+E and Del do nothing on it; Space opens its page; Ctrl+Enter opens it in
/// Explorer.
#[test]
fn an_external_row_has_a_page_and_opens_in_explorer() {
    let fixture = Fixture::new(Vec::new());
    listing(&fixture, vec![other('W', r"\\nas\photos")]);
    fixture.key(Key::DownArrow);
    fixture.key(Key::Return);
    fixture.ctrl("e");
    fixture.key(Key::Delete);
    assert!(!fixture.ui.window().get_page_open());
    assert!(!fixture.ui.window().get_dialog_open());
    assert_eq!(fixture.ui.window().get_question(), "");
    assert!(fixture.take_sent().is_empty());

    fixture.ctrl(Key::Return);
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::OpenInExplorer(letter('W'), String::new())]
    );

    fixture.key(" ");
    assert!(fixture.ui.window().get_page_open());
}

/// FR-SMB-09, FR-SMB-10: a click opens the drive's page, with what Windows says of it and its
/// buttons.
#[test]
fn an_external_drives_page_shows_what_windows_says_and_can_take_it_over() {
    let fixture = Fixture::new(Vec::new());
    listing(&fixture, vec![other('W', r"\\nas\photos")]);
    fixture.click_at(&fixture.row(r"\\nas\photos"), 1);
    let window = fixture.ui.window();
    assert!(window.get_page_open());
    let view = window.get_page_view();
    assert_eq!(view.kind, EntryKind::External);
    assert_eq!(view.unc, r"\\nas\photos");
    assert_eq!(view.drive, "W:");
    assert_eq!(view.windows_status, "Mapped");
    assert_eq!(view.provider, "Microsoft Windows Network");
    assert_eq!(view.user, r"CORP\anna");
    assert!(view.remembered);
    assert!(view.can_take_over);

    fixture.press("Copy");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::CopyText(r"\\nas\photos".to_owned())]
    );
    fixture.press("Open in Explorer");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::OpenInExplorer(letter('W'), String::new())]
    );

    fixture.press("Manage with DriveMount…");
    assert!(window.get_dialog_open());
    assert_eq!(fixture.take_sent(), [UiCommand::FindFreeLetters]);
}

/// Once the take-over is saved, the page shows the profile that took the drive's place.
#[test]
fn after_a_take_over_the_page_shows_the_new_profile() {
    let fixture = Fixture::new(Vec::new());
    let listed = listing(&fixture, vec![other('W', r"\\nas\photos")]);
    fixture.click_at(&fixture.row(r"\\nas\photos"), 1);
    fixture.press("Manage with DriveMount…");
    fixture.take_sent();
    fixture.ui.inner.save_taking_over(false);
    let mut sent = fixture.take_sent();
    let Some(UiCommand::TakeOver { profile, .. }) = sent.pop() else {
        panic!("{sent:?}");
    };
    listed.borrow_mut().clear();
    fixture.ui.set_config(Config {
        profiles: vec![*profile],
        ..Config::default()
    });
    fixture.ui.save_finished(Ok(()));

    let window = fixture.ui.window();
    assert!(!window.get_dialog_open());
    assert!(window.get_page_open());
    assert_eq!(window.get_page_view().kind, EntryKind::Profile);
    assert_eq!(window.get_page_view().name, "photos");
    assert_eq!(kinds_and_names(&fixture).len(), 1, "the external row went");
}

/// A drive Windows has not connected cannot be opened: from its page, its menu or Ctrl+Enter.
#[test]
fn an_external_drive_that_is_not_connected_cannot_be_opened() {
    for status in [
        DriveStatus::NotConnected,
        DriveStatus::Disconnected,
        DriveStatus::Reconnecting,
    ] {
        let fixture = Fixture::new(Vec::new());
        let mut drive = other('W', r"\\nas\photos");
        drive.status = status;
        listing(&fixture, vec![drive]);
        assert!(!fixture.entry(0).can_open, "{status:?}");
        let items = right_click_menu(&fixture, r"\\nas\photos");
        assert!(
            !items.iter().any(|item| item == "Open in Explorer"),
            "{items:?}"
        );

        fixture.key(Key::Escape);
        fixture.key(Key::DownArrow);
        fixture.ctrl(Key::Return);
        fixture.key(" ");
        assert!(fixture.ui.window().get_page_open());
        assert!(fixture.labelled("Open in Explorer").is_none(), "{status:?}");
        fixture.ctrl(Key::Return);
        assert!(fixture.take_sent().is_empty(), "{status:?}");
    }
}

/// WSL's and other providers' drives have a page, but nothing to take over.
#[test]
fn a_drive_of_another_provider_cannot_be_taken_over_from_its_page() {
    let fixture = Fixture::new(Vec::new());
    let mut wsl = other('Y', r"\\wsl$\Ubuntu");
    wsl.provider = "Plan 9 Network Provider".to_owned();
    wsl.smb = false;
    wsl.user = None;
    listing(&fixture, vec![wsl]);
    fixture.click_at(&fixture.row(r"\\wsl$\Ubuntu"), 1);
    assert!(fixture.ui.window().get_page_open());
    assert!(!fixture.ui.window().get_page_view().can_take_over);
    assert!(fixture.labelled("Manage with DriveMount…").is_none());
}

/// FR-SMB-09's acceptance: after `net use W: /delete` the row goes at the next listing; FR-SET-09
/// hides them all.
#[test]
fn rows_follow_the_listing_and_the_setting() {
    let fixture = Fixture::new(vec![profile("A")]);
    let listed = listing(&fixture, vec![other('W', r"\\nas\photos")]);
    assert_eq!(kinds_and_names(&fixture).len(), 2);

    listed.borrow_mut().clear();
    fixture.ui.inner.poll_other_drives();
    assert_eq!(kinds_and_names(&fixture).len(), 1);

    listed.borrow_mut().push(other('W', r"\\nas\photos"));
    let mut config = Config {
        profiles: vec![profile("A")],
        ..Config::default()
    };
    config.settings.show_other_drives = false;
    fixture.ui.set_config(config);
    fixture.ui.inner.poll_other_drives();
    assert_eq!(
        kinds_and_names(&fixture),
        [(EntryKind::Profile, "A".to_owned())]
    );
}

/// FR-SMB-04: the manager takes the row from *Unmounting* back to *Mounted*; the user is asked, and
/// yes unmounts it anyway.
#[test]
fn files_open_on_a_drive_ask_and_yes_forces_the_unmount() {
    let saved = share("NAS");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.set_state(id, MountState::Mounted, Some('S'));
    fixture.set_state(id, MountState::Unmounting, Some('S'));
    assert_eq!(fixture.ui.window().get_question(), "");
    fixture.set_state(id, MountState::Mounted, Some('S'));
    let question = fixture.ui.window().get_question();
    assert!(question.contains("S:"), "{question}");
    assert!(question.contains("\"NAS\""), "{question}");

    fixture.ui.inner.answer(true);
    assert_eq!(fixture.take_sent(), [UiCommand::ForceUnmount(id)]);
}

#[test]
fn saying_no_keeps_the_drive() {
    let saved = share("NAS");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.set_state(id, MountState::Unmounting, Some('S'));
    fixture.set_state(id, MountState::Mounted, Some('S'));
    fixture.key(Key::Escape);
    assert_eq!(fixture.ui.window().get_question(), "");
    assert!(fixture.take_sent().is_empty());
}

/// FR-SMB-08.
#[test]
fn without_winfsp_a_share_can_still_be_mounted() {
    let fixture = Fixture::new(vec![share("NAS"), profile("SSH")]);
    fixture.ui.set_winfsp_ready(false);
    assert!(fixture.is_enabled("Mount NAS"));
    assert!(!fixture.is_enabled("Mount SSH"));
}

/// What a right-click on this row shows: every label in the window then, as the testing back-end
/// gives menu items no role of their own. Menu items show only once a menu is up.
fn right_click_menu(fixture: &Fixture, name: &str) -> Vec<String> {
    use i_slint_backend_testing::ElementQuery;
    use slint::LogicalPosition;
    use slint::platform::{PointerEventButton, WindowEvent};

    let at = fixture.row(name).absolute_position();
    let position = LogicalPosition::new(at.x + 60.0, at.y + 20.0);
    fixture.dispatch(WindowEvent::PointerMoved { position });
    fixture.dispatch(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Right,
    });
    fixture.dispatch(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Right,
    });
    let mut items: Vec<String> = ElementQuery::from_root(fixture.ui.window())
        .match_descendants()
        .find_all()
        .into_iter()
        .filter_map(|item| item.accessible_label().map(|label| label.to_string()))
        .collect();
    items.dedup();
    items.sort();
    items.dedup();
    items
}

/// The row's TouchArea took every button, so a right-click never reached the menu (owner,
/// 2026-11-14). It is on top now.
#[test]
fn a_right_click_on_a_profile_row_shows_its_menu() {
    let fixture = Fixture::new(vec![profile("Zed")]);
    let items = right_click_menu(&fixture, "Zed");
    assert!(items.iter().any(|item| item == "Details"), "{items:?}");
    assert!(items.iter().any(|item| item == "Edit…"), "{items:?}");
    assert!(
        fixture.take_sent().is_empty(),
        "a right-click is not a click"
    );
    assert!(!fixture.ui.window().get_page_open());
}

#[test]
fn a_right_click_on_an_external_row_shows_its_menu() {
    let fixture = Fixture::new(Vec::new());
    listing(&fixture, vec![other('P', r"\\nas\photos")]);
    let items = right_click_menu(&fixture, r"\\nas\photos");
    for wanted in [
        "Details",
        "Open in Explorer",
        "Copy path",
        "Manage with DriveMount…",
    ] {
        assert!(
            items.iter().any(|item| item == wanted),
            "{wanted}: {items:?}"
        );
    }
    assert!(!items.iter().any(|item| item == "Edit…"), "{items:?}");
}
