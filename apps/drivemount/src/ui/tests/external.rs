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

/// Spec §7.1: Enter, Space, Ctrl+E and Del do nothing on it; Ctrl+Enter opens it.
#[test]
fn an_external_row_only_opens_in_explorer() {
    let fixture = Fixture::new(Vec::new());
    listing(&fixture, vec![other('W', r"\\nas\photos")]);
    fixture.key(Key::DownArrow);
    fixture.key(Key::Return);
    fixture.key(" ");
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
