// SPDX-License-Identifier: GPL-3.0-or-later

//! The profile dialog and the prompts in the window, through the testing back-end. What the boxes mean
//! is tested in `form.rs`, and the queue itself in `prompts.rs`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use drivemount_core::config::{Config, ReadOnlyReason};
use drivemount_core::manager::{MountEvent, MountState, ProfileChangeError};
use drivemount_core::profile::{DriveChoice, Field, FieldError, Problem, Protocol, SftpAuth};
use drivemount_vfs::{
    ChangedHostKey, ConnectError, HostKeyDecision, HostKeyRequest, KbdIntPrompt, KbdIntRequest,
    LoginAnswer, LoginRequest, MountError, Prompter, Remember, SecretKind, SecretRequest,
};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use super::super::{KeyFileBox, TypedSecrets, UiCommand, UiPrompter};
use super::{Fixture, letter, profile};

const OPENSSH_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk=\n";

impl Fixture {
    fn dialog_is_open(&self) -> bool {
        self.ui.window().get_dialog_open()
    }

    fn prompt_is_open(&self) -> bool {
        self.ui.window().get_prompt_open()
    }

    /// Opens the dialog for a new profile, leaving out the command that always comes with it.
    fn open_new(&self) {
        self.ctrl("n");
        assert!(self.dialog_is_open());
        assert_eq!(self.take_sent(), [UiCommand::FindFreeLetters]);
    }

    fn open_selected(&self) {
        self.key(Key::DownArrow);
        self.ctrl("e");
        assert!(self.dialog_is_open());
        assert_eq!(self.take_sent(), [UiCommand::FindFreeLetters]);
    }

    fn with_role(&self, label: &str, role: AccessibleRole) -> ElementHandle {
        ElementQuery::from_root(self.ui.window())
            .match_descendants()
            .match_accessible_role(role)
            .find_all()
            .into_iter()
            .find(|element| element.accessible_label().as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
    }

    fn text_box(&self, label: &str) -> ElementHandle {
        self.with_role(label, AccessibleRole::TextInput)
    }

    /// As typing or pasting it would: the box's own edited callback runs.
    fn type_in(&self, label: &str, text: &str) {
        self.text_box(label).set_accessible_value(text);
    }

    fn box_holds(&self, label: &str) -> String {
        self.text_box(label).accessible_value().unwrap().to_string()
    }

    fn box_is_enabled(&self, label: &str) -> bool {
        self.text_box(label).accessible_enabled().unwrap()
    }

    /// With the mouse wheel over the dialog's fields, to what is below the edge of its scroll area. The
    /// tests can only find what can be seen.
    fn scroll_dialog_down(&self) {
        self.make_what_appeared();
        let position = LogicalPosition::new(320.0, 300.0);
        self.dispatch(WindowEvent::PointerMoved { position });
        self.dispatch(WindowEvent::PointerScrolled {
            position,
            delta_x: 0.0,
            delta_y: -2000.0,
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
    }

    fn choose_auth(&self, label: &str) {
        self.with_role(label, AccessibleRole::RadioButton)
            .invoke_accessible_default_action();
    }

    /// The button of this name that is in front, as those behind come first in the tree.
    fn press_front(&self, label: &str) {
        ElementHandle::find_by_accessible_label(self.ui.window(), label)
            .last()
            .unwrap_or_else(|| panic!("nothing is labelled {label}"))
            .invoke_accessible_default_action();
    }

    fn fill_in_a_valid_new_profile(&self) {
        self.type_in("Name", "New");
        self.type_in("Host", "h");
        self.type_in("User", "me");
        self.choose_auth("SSH agent");
    }

    fn can_save(&self) -> bool {
        self.ui.window().get_dialog_view().can_save
    }

    fn can_test(&self) -> bool {
        self.ui.window().get_dialog_view().can_test
    }

    fn message(&self) -> String {
        self.ui.window().get_dialog_view().message.to_string()
    }

    fn test_text(&self) -> String {
        self.ui.window().get_dialog_view().test_text.to_string()
    }

    /// Takes what other threads have posted, as the event loop would.
    fn read_mail(&self) {
        self.ui.window().invoke_mail_arrived();
    }

    /// Until the key file has been looked at.
    fn wait_until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        loop {
            self.read_mail();
            if done(self) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn take_save(&self) -> (drivemount_core::profile::Profile, TypedSecrets) {
        let mut sent = self.take_sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        match sent.remove(0) {
            UiCommand::Save { profile, secrets } => (*profile, secrets),
            other => panic!("{other:?}"),
        }
    }

    fn take_test(&self) -> (u64, drivemount_core::profile::Profile, TypedSecrets) {
        let mut sent = self.take_sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        match sent.remove(0) {
            UiCommand::Test {
                attempt,
                profile,
                typed,
            } => (attempt, *profile, typed),
            other => panic!("{other:?}"),
        }
    }
}

fn host_of(saved: &drivemount_core::profile::Profile) -> &str {
    let Protocol::Sftp(sftp) = &saved.protocol;
    &sftp.host
}

#[test]
fn ctrl_n_opens_an_empty_dialog_and_esc_closes_it() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    assert_eq!(
        fixture.ui.window().get_dialog_view().title,
        "Add connection"
    );
    assert_eq!(fixture.box_holds("Port"), "22");
    assert_eq!(fixture.box_holds("Remote path"), "~");
    assert!(!fixture.can_save());

    fixture.type_in("Name", "typed");
    fixture.key(Key::Escape);
    assert!(!fixture.dialog_is_open());
    assert!(fixture.take_sent().is_empty(), "nothing is saved or hidden");
    assert_eq!(fixture.ui.window().get_form().name, "", "nothing stays");

    fixture.open_new();
    assert_eq!(fixture.box_holds("Name"), "", "a new dialog starts empty");
}

#[test]
fn the_window_grows_for_the_dialog_and_goes_back_after() {
    let fixture = Fixture::new(Vec::new());
    let window = fixture.ui.window().window();
    let scale = window.scale_factor();
    let before = window.size().to_logical(scale);
    assert!(before.height < 600.0, "the default size has no room");

    fixture.open_new();
    let grown = window.size().to_logical(scale);
    assert!(grown.height >= 600.0, "{grown:?}");
    assert!(grown.width >= before.width);

    fixture.key(Key::Escape);
    assert_eq!(window.size().to_logical(scale), before);
}

/// The connection page has the same room, and editing from it keeps that room until the page goes too.
#[test]
fn the_window_grows_for_the_connection_page_and_keeps_it_for_editing() {
    let fixture = Fixture::new(vec![profile("A")]);
    let window = fixture.ui.window().window();
    let scale = window.scale_factor();
    let before = window.size().to_logical(scale);

    fixture.key(Key::DownArrow);
    fixture.key(" ");
    assert!(fixture.ui.window().get_page_open());
    let grown = window.size().to_logical(scale);
    assert!(grown.height >= 600.0, "{grown:?}");

    fixture.ctrl("e");
    assert!(fixture.dialog_is_open());
    fixture.key(Key::Escape);
    assert_eq!(
        window.size().to_logical(scale),
        grown,
        "the page is still up"
    );

    fixture.key(Key::Escape);
    assert!(!fixture.ui.window().get_page_open());
    assert_eq!(window.size().to_logical(scale), before);
}

#[test]
fn a_window_the_user_resized_meanwhile_stays_as_they_left_it() {
    let fixture = Fixture::new(Vec::new());
    let window = fixture.ui.window().window();
    fixture.open_new();
    let chosen = slint::LogicalSize::new(800.0, 700.0);
    window.set_size(chosen);
    fixture.key(Key::Escape);
    assert_eq!(window.size().to_logical(window.scale_factor()), chosen);
}

#[test]
fn nothing_behind_the_dialog_can_be_reached_by_keys() {
    let saved = profile("A");
    let fixture = Fixture::new(vec![saved]);
    fixture.key(Key::DownArrow);
    fixture.open_selected_without_moving();
    fixture.key(Key::Delete);
    fixture.ctrl("n");
    fixture.ctrl(Key::Return);
    fixture.key(Key::UpArrow);
    assert_eq!(fixture.ui.window().get_question(), "");
    assert!(fixture.take_sent().is_empty());
    // The page is in the list's place, so the rows are not there to be reached.
    assert!(fixture.labelled("Mount A").is_none());
    assert!(!fixture.is_enabled("Mount all"));
    fixture.key(Key::Escape);
    assert!(fixture.is_enabled("Mount A"));
}

impl Fixture {
    fn open_selected_without_moving(&self) {
        self.ctrl("e");
        assert!(self.dialog_is_open());
        assert_eq!(self.take_sent(), [UiCommand::FindFreeLetters]);
    }
}

#[test]
fn a_new_form_opens_with_nothing_marked_though_save_is_off() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    let marks = fixture.ui.window().get_marks();
    assert_eq!(marks.name, "");
    assert_eq!(marks.host, "");
    assert_eq!(marks.user, "");
    assert_eq!(marks.key_file, "");
    assert!(!fixture.can_save());

    // A rule other than being empty shows at once, in a box touched or not.
    fixture.type_in("Port", "0");
    assert_eq!(fixture.ui.window().get_marks().port, "Port must be 1–65535");
}

#[test]
fn a_saved_profile_with_an_empty_box_has_it_marked_at_once() {
    let mut saved = profile("A");
    let Protocol::Sftp(sftp) = &mut saved.protocol;
    sftp.username = String::new();
    let fixture = Fixture::new(vec![saved]);
    fixture.open_selected();
    assert_eq!(
        fixture.ui.window().get_marks().user,
        "User name is required"
    );
}

/// The focus went with the box of the prompt before, and even Esc did nothing until Tab was pressed.
#[test]
fn the_next_prompt_has_the_keys_whatever_kind_came_before() {
    let asking = Asking::new();
    let first = asking.password();
    let second = asking.host_key();
    let third = asking.password();
    let fixture = &asking.fixture;

    fixture.key(Key::Escape);
    assert_eq!(asking.answer(first), None);
    fixture.key(Key::Return);
    assert_eq!(
        asking.answer(second),
        HostKeyDecision::Reject,
        "Enter is safe"
    );
    fixture.key("x");
    fixture.key(Key::Return);
    assert_eq!(asking.answer(third), Some(("x".to_owned(), Remember::No)));
}

#[test]
fn each_rule_shows_under_its_box_and_save_comes_on_when_all_are_met() {
    let fixture = Fixture::new(vec![profile("Taken")]);
    fixture.open_new();
    let marks = || fixture.ui.window().get_marks();

    fixture.type_in("Name", "x");
    fixture.type_in("Name", "");
    assert_eq!(marks().name, "Name is required");
    fixture.type_in("Name", "taken");
    assert_eq!(marks().name, "A connection with this name already exists");
    fixture.type_in("Name", "New");
    assert_eq!(marks().name, "");

    fixture.type_in("Host", "a b");
    assert_eq!(marks().host, "Enter a host name or IP address");
    fixture.type_in("Host", "h");
    assert_eq!(marks().host, "");
    fixture.type_in("Port", "0");
    assert_eq!(marks().port, "Port must be 1–65535");
    fixture.type_in("Port", "2222");
    assert_eq!(marks().port, "");

    fixture.type_in("User", "x");
    fixture.type_in("User", "");
    assert_eq!(marks().user, "User name is required");
    fixture.type_in("User", "me");
    assert!(!fixture.can_save(), "no key file yet");
    fixture.type_in("Key file", "x.ppk");
    fixture.type_in("Key file", "");
    assert_eq!(marks().key_file, "Select a private key file");
    assert!(!fixture.can_save());
    fixture.choose_auth("SSH agent");
    assert_eq!(marks().key_file, "");
    assert!(fixture.can_save());

    fixture.type_in("Remote path", "nope");
    assert_eq!(
        marks().remote_path,
        "Use an absolute path such as /var/www, or ~ for home"
    );
    assert!(!fixture.can_save(), "FR-PROF-04");
}

/// FR-PROF-05's acceptance case, in the box itself.
#[test]
fn pasting_into_the_host_box_fills_the_others() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.type_in("Host", "deploy@10.0.4.21:2222/var/www");
    assert_eq!(fixture.box_holds("Host"), "10.0.4.21");
    assert_eq!(fixture.box_holds("User"), "deploy");
    assert_eq!(fixture.box_holds("Port"), "2222");
    assert_eq!(fixture.box_holds("Remote path"), "/var/www");
    assert_eq!(fixture.ui.window().get_marks().host, "");
}

#[test]
fn typing_a_host_and_port_is_not_split_along_the_way() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    let mut typed = String::new();
    for character in "host:22".chars() {
        typed.push(character);
        fixture.type_in("Host", &typed);
    }
    assert_eq!(fixture.box_holds("Host"), "host:22");
    assert_eq!(fixture.box_holds("Port"), "22", "as it was");
    assert_eq!(
        fixture.ui.window().get_marks().host,
        "Enter a host name or IP address"
    );
}

#[test]
fn what_cannot_be_split_stays_in_the_host_box_and_is_marked() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.type_in("Host", "ftp://user@host");
    assert_eq!(fixture.box_holds("Host"), "ftp://user@host");
    assert_eq!(fixture.box_holds("User"), "");
    assert_eq!(
        fixture.ui.window().get_marks().host,
        "Enter a host name or IP address"
    );
}

#[test]
fn save_sends_the_profile_and_waits_for_the_answer() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.choose_auth("Password");
    fixture.type_in("Password", "hunter2");
    fixture.press("Save");

    let (saved, secrets) = fixture.take_save();
    assert_eq!(saved.name, "New");
    assert_eq!(host_of(&saved), "h");
    let Protocol::Sftp(sftp) = &saved.protocol;
    assert_eq!(sftp.auth, SftpAuth::Password { remember: true });
    assert_eq!(secrets.0.len(), 1);
    assert_eq!(secrets.0[0].1.expose_secret(), "hunter2");

    assert!(fixture.dialog_is_open(), "it waits for the file");
    assert!(!fixture.can_save(), "no second save");
    fixture.press("Save");
    assert!(fixture.take_sent().is_empty());
    fixture.key(Key::Escape);
    assert!(fixture.dialog_is_open(), "it cannot be taken back");

    fixture.ui.save_finished(Ok(()));
    assert!(!fixture.dialog_is_open());
    assert_eq!(fixture.ui.window().get_form().password, "", "nothing stays");
}

#[test]
fn a_refused_save_marks_the_box_and_keeps_what_was_typed() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Save");
    let _ = fixture.take_save();

    fixture
        .ui
        .save_finished(Err(ProfileChangeError::Invalid(vec![FieldError {
            field: Field::Name,
            problem: Problem::NameTaken,
        }])));
    assert!(fixture.dialog_is_open());
    assert_eq!(fixture.box_holds("Name"), "New");
    assert_eq!(
        fixture.ui.window().get_marks().name,
        "A connection with this name already exists"
    );
    assert_eq!(
        fixture.message(),
        "Correct the marked fields and save again."
    );
    assert!(!fixture.can_save());

    fixture.type_in("Name", "Other");
    assert_eq!(fixture.ui.window().get_marks().name, "");
    assert_eq!(fixture.message(), "");
    assert!(fixture.can_save());
}

#[test]
fn every_way_a_save_can_fail_says_so_and_can_be_tried_again() {
    let io_error = || io::Error::other("detail for the log");
    let cases: [(ProfileChangeError, &str); 5] = [
        (
            ProfileChangeError::StoringSecret(io_error()),
            "The connection was saved, but its password could not be remembered.",
        ),
        (
            ProfileChangeError::DeletingSecrets(io_error()),
            "Nothing was saved, as the saved password could not be removed.",
        ),
        (
            ProfileChangeError::ConnectionFieldsLocked,
            "Unmount to change",
        ),
        (
            ProfileChangeError::Failed("detail for the log".to_owned()),
            "The connection could not be saved. See the log for details.",
        ),
        (
            ProfileChangeError::ReadOnly(ReadOnlyReason::Newer { found: 99 }),
            "This configuration was written by a newer DriveMount, so nothing can be saved. Install the newer version to make changes.",
        ),
    ];
    for (error, text) in cases {
        let fixture = Fixture::new(Vec::new());
        fixture.open_new();
        fixture.fill_in_a_valid_new_profile();
        fixture.press("Save");
        let _ = fixture.take_save();
        fixture.ui.save_finished(Err(error));
        assert!(fixture.dialog_is_open(), "{text}");
        assert_eq!(fixture.message(), text);
        assert!(fixture.can_save(), "{text}");
        assert!(!fixture.message().contains("detail for the log"));
    }
}

#[test]
fn an_answer_nobody_is_waiting_for_is_ignored() {
    let fixture = Fixture::new(Vec::new());
    fixture.ui.save_finished(Ok(()));
    fixture.ui.test_finished(1, Ok(Duration::from_millis(1)));
    fixture.open_new();
    fixture.ui.save_finished(Ok(()));
    assert!(fixture.dialog_is_open(), "it did not ask");
}

#[test]
fn editing_shows_what_is_saved_and_keeps_the_id() {
    let mut saved = profile("Production");
    saved.drive = DriveChoice::Letter(letter('X'));
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.open_selected();
    assert_eq!(
        fixture.ui.window().get_dialog_view().title,
        "Edit connection"
    );
    assert_eq!(fixture.box_holds("Name"), "Production");
    assert_eq!(fixture.box_holds("Host"), "host");
    assert_eq!(fixture.box_holds("User"), "me");
    let items = fixture.ui.window().get_drive_items();
    let chosen = usize::try_from(fixture.ui.window().get_form().drive_index).unwrap();
    assert_eq!(items.row_data(chosen).unwrap(), "X:");
    assert!(fixture.can_save(), "it is valid as it is");

    fixture.type_in("Name", "Renamed");
    fixture.press("Save");
    let (changed, secrets) = fixture.take_save();
    assert_eq!(changed.id, id, "FR-PROF-02");
    assert_eq!(changed.name, "Renamed");
    assert_eq!(changed.drive, DriveChoice::Letter(letter('X')));
    assert!(secrets.0.is_empty());
}

#[test]
fn a_profile_breaking_a_rule_opens_with_the_box_marked() {
    let mut saved = profile("Bad");
    let Protocol::Sftp(sftp) = &mut saved.protocol;
    sftp.keepalive_secs = 9999;
    let fixture = Fixture::new(vec![saved]);
    fixture.open_selected();
    assert_eq!(
        fixture.ui.window().get_marks().keep_alive,
        "Keep-alive must be 0 (off) or 5–300 seconds"
    );
    assert!(fixture.ui.window().get_advanced_open(), "it must not hide");
    assert!(!fixture.can_save());
}

/// FR-PROF-08's acceptance case.
#[test]
fn while_mounted_the_host_is_locked_and_the_name_is_not() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved.clone()]);
    fixture.set_state(id, MountState::Mounted, Some('Z'));
    fixture.open_selected();

    assert!(fixture.ui.window().get_dialog_view().locked);
    assert!(fixture.labelled("Unmount to change").is_some());
    assert!(fixture.box_is_enabled("Name"));
    for label in ["Host", "Port", "User", "Remote path"] {
        assert!(!fixture.box_is_enabled(label), "{label}");
    }
    assert!(
        !fixture
            .with_role("Key file", AccessibleRole::RadioButton)
            .accessible_enabled()
            .unwrap()
    );
    fixture.type_in("Name", "Renamed");
    fixture.press("Advanced");
    assert!(!fixture.box_is_enabled("Jump host"));
    fixture.scroll_dialog_down();
    assert!(!fixture.box_is_enabled("Cache (s)"));

    fixture.press("Save");
    let (changed, _) = fixture.take_save();
    assert_eq!(changed.name, "Renamed");
    assert_eq!(changed.protocol, saved.protocol);
}

#[test]
fn a_mount_starting_with_the_dialog_open_locks_it_and_puts_the_boxes_back() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.open_selected();
    assert!(fixture.box_is_enabled("Host"));
    fixture.type_in("Name", "Renamed");
    fixture.type_in("Host", "other");

    fixture.set_state(id, MountState::Connecting, None);
    assert!(!fixture.box_is_enabled("Host"));
    assert_eq!(fixture.box_holds("Host"), "host");
    assert_eq!(
        fixture.box_holds("Name"),
        "Renamed",
        "this is still allowed"
    );

    fixture.set_state(id, MountState::Disconnected, None);
    assert!(fixture.box_is_enabled("Host"));
}

/// NFR-REL-05: editing is allowed to look, and Test still works, but nothing can be saved.
#[test]
fn a_file_that_cannot_be_written_leaves_save_off_and_says_why() {
    let saved = profile("A");
    let fixture = Fixture::new(vec![saved]);
    fixture
        .ui
        .set_read_only(Some(&ReadOnlyReason::Newer { found: 99 }));
    fixture.open_selected();
    assert!(!fixture.can_save());
    assert!(fixture.can_test());
    assert_eq!(
        fixture.message(),
        "The configuration file cannot be changed, so this cannot be saved."
    );
    fixture.press("Save");
    assert!(fixture.take_sent().is_empty());
}

#[test]
fn free_letters_are_marked_when_they_are_known() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    let items = fixture.ui.window().get_drive_items();
    assert_eq!(items.row_count(), 24);
    assert_eq!(items.row_data(0).unwrap(), "Auto");
    assert_eq!(items.row_data(1).unwrap(), "D:");
    assert_eq!(items.row_data(23).unwrap(), "Z:");

    fixture.ui.set_free_letters(vec![letter('Z'), letter('E')]);
    assert_eq!(items.row_data(1).unwrap(), "D:");
    assert_eq!(items.row_data(2).unwrap(), "E: (free)");
    assert_eq!(items.row_data(23).unwrap(), "Z: (free)");
    assert_eq!(fixture.ui.window().get_form().drive_index, 0, "still Auto");
}

#[test]
fn a_letter_another_profile_prefers_is_not_marked_free() {
    let mut other = profile("NAS backup");
    other.drive = DriveChoice::Letter(letter('X'));
    let fixture = Fixture::new(vec![other]);
    fixture.open_new();
    let items = fixture.ui.window().get_drive_items();
    assert_eq!(items.row_data(21).unwrap(), "X: (used by 'NAS backup')");

    // Not mounted, so Windows says it is free; the profile wins.
    fixture.ui.set_free_letters(vec![letter('X'), letter('Y')]);
    assert_eq!(items.row_data(21).unwrap(), "X: (used by 'NAS backup')");
    assert_eq!(items.row_data(22).unwrap(), "Y: (free)");
}

#[test]
fn a_profile_s_own_letter_is_not_used_by_another() {
    let mut own = profile("NAS backup");
    own.drive = DriveChoice::Letter(letter('X'));
    let fixture = Fixture::new(vec![own]);
    fixture.open_selected();
    fixture.ui.set_free_letters(vec![letter('X')]);
    let items = fixture.ui.window().get_drive_items();
    assert_eq!(items.row_data(21).unwrap(), "X: (free)");
}

#[test]
fn a_letter_another_profile_uses_gets_a_warning_and_can_be_saved() {
    let mut other = profile("NAS backup");
    other.drive = DriveChoice::Letter(letter('X'));
    let fixture = Fixture::new(vec![other]);
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    let mut form = fixture.ui.window().get_form();
    form.drive_index = 21;
    fixture.ui.window().set_form(form);
    fixture.ui.window().invoke_form_edited();
    assert_eq!(
        fixture.ui.window().get_marks().drive_warning,
        "Also used by 'NAS backup'"
    );
    assert!(fixture.can_save());
}

fn write(folder: &Path, name: &str, text: &str) -> PathBuf {
    let path = folder.join(name);
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn a_key_file_is_looked_at_off_the_ui_thread_and_only_then_can_be_saved() {
    let folder = tempfile::tempdir().unwrap();
    let good = write(folder.path(), "id_good", OPENSSH_KEY);
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.choose_auth("Key file");

    fixture.type_in("Key file", good.to_str().unwrap());
    assert!(!fixture.can_save(), "it has not been looked at yet");
    assert!(!fixture.can_test());
    assert_eq!(fixture.ui.window().get_marks().key_file, "");
    fixture.wait_until("the file to be looked at", Fixture::can_save);
    assert!(fixture.can_test());
    assert_eq!(fixture.ui.window().get_marks().key_file, "");
}

#[test]
fn each_kind_of_wrong_key_file_gets_its_own_text() {
    let folder = tempfile::tempdir().unwrap();
    let public = write(folder.path(), "looks_private", "ssh-ed25519 AAAA me\n");
    let putty = write(
        folder.path(),
        "putty",
        "PuTTY-User-Key-File-3: ssh-ed25519\n",
    );
    let other = write(folder.path(), "notes", "not a key\n");
    let cases = [
        (folder.path().join("missing"), "This file does not exist"),
        (public, "This is a public key; select the private key file"),
        (
            putty,
            "Convert with PuTTYgen → Conversions → Export OpenSSH key",
        ),
        (other, "Unsupported key file format."),
    ];
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.choose_auth("Key file");
    for (path, text) in cases {
        fixture.type_in("Key file", path.to_str().unwrap());
        fixture.wait_until(text, |fixture| {
            fixture.ui.window().get_marks().key_file == text
        });
        assert!(!fixture.can_save(), "{text}");
    }
}

/// FR-SSH-03's acceptance case: even if there is no such file yet.
#[test]
fn a_ppk_shows_the_hint_at_once_and_save_stays_off() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.choose_auth("Key file");
    fixture.type_in("Key file", r"C:\keys\id.ppk");
    assert_eq!(
        fixture.ui.window().get_marks().key_file,
        "Convert with PuTTYgen → Conversions → Export OpenSSH key"
    );
    assert!(!fixture.can_save());
}

#[test]
fn the_choose_buttons_ask_for_a_file_and_what_comes_back_is_checked() {
    let folder = tempfile::tempdir().unwrap();
    let good = write(folder.path(), "id_good", OPENSSH_KEY);
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.choose_auth("Key file");

    fixture.press("Choose key file");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::PickKeyFile {
            which: KeyFileBox::Profile,
            title: "Choose a private key file".to_owned(),
        }]
    );
    fixture.ui.key_file_picked(KeyFileBox::Profile, None);
    assert_eq!(fixture.box_holds("Key file"), "", "they cancelled");

    fixture
        .ui
        .key_file_picked(KeyFileBox::Profile, Some(good.clone()));
    assert_eq!(fixture.box_holds("Key file"), good.to_str().unwrap());
    fixture.wait_until("the file to be looked at", Fixture::can_save);
}

#[test]
fn a_file_chosen_after_the_dialog_closed_goes_nowhere() {
    let fixture = Fixture::new(Vec::new());
    fixture
        .ui
        .key_file_picked(KeyFileBox::Profile, Some(PathBuf::from("late")));
    assert_eq!(fixture.ui.window().get_form().key_file, "");
}

#[test]
fn advanced_is_closed_until_asked_for_and_holds_its_fields() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    assert!(!fixture.ui.window().get_advanced_open());
    fixture.press("Advanced");
    assert!(fixture.ui.window().get_advanced_open());

    fixture.fill_in_a_valid_new_profile();
    fixture.type_in("Jump host", "bastion@jump:2200");
    fixture.scroll_dialog_down();
    assert_eq!(fixture.box_holds("Cache (s)"), "5");
    assert_eq!(fixture.box_holds("Keep-alive (s)"), "15");
    fixture.type_in("Cache (s)", "0");
    fixture.press("Read-only");
    fixture.press("Reconnect automatically");
    fixture.press("Save");
    let (saved, _) = fixture.take_save();
    assert!(saved.read_only);
    assert!(!saved.auto_reconnect);
    let Protocol::Sftp(sftp) = &saved.protocol;
    assert_eq!(sftp.cache_ttl_secs, 0);
    let jump = sftp.jump.as_ref().unwrap();
    assert_eq!(jump.host, "jump");
    assert_eq!(jump.port, 2200);
    assert_eq!(jump.username.as_deref(), Some("bastion"));
}

#[test]
fn test_connection_reports_in_the_dialog() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    assert!(!fixture.can_test());
    fixture.type_in("Host", "h");
    fixture.type_in("User", "me");
    fixture.choose_auth("Password");
    fixture.type_in("Password", "typed");
    assert!(fixture.can_test(), "it needs no name");

    fixture.press("Test connection");
    let (attempt, tested, typed) = fixture.take_test();
    assert_eq!(host_of(&tested), "h");
    assert_eq!(typed.0[0].1.expose_secret(), "typed");
    assert!(fixture.ui.window().get_dialog_view().testing);
    assert_eq!(fixture.test_text(), "Connecting…");

    fixture
        .ui
        .test_finished(attempt, Ok(Duration::from_millis(12)));
    assert_eq!(fixture.test_text(), "✓ Connected (12 ms)");
    assert!(!fixture.ui.window().get_dialog_view().test_failed);
    assert!(!fixture.ui.window().get_dialog_view().testing);
}

/// FR-PROF-06's acceptance cases.
#[test]
fn test_connection_says_what_went_wrong() {
    let cases = [
        (
            ConnectError::Auth {
                user: "me".to_owned(),
                host: "h".to_owned(),
            },
            "Authentication failed for me@h. Check the user name and password or key.",
        ),
        (
            ConnectError::PathNotFound {
                path: "/nope".to_owned(),
            },
            "Remote path /nope not found. Edit the connection and check the remote path.",
        ),
    ];
    for (error, text) in cases {
        let fixture = Fixture::new(Vec::new());
        fixture.open_new();
        fixture.fill_in_a_valid_new_profile();
        fixture.press("Test connection");
        let (attempt, ..) = fixture.take_test();
        fixture.ui.test_finished(attempt, Err(error));
        assert_eq!(fixture.test_text(), text);
        assert!(fixture.ui.window().get_dialog_view().test_failed);
    }
}

#[test]
fn cancelling_a_prompt_in_a_test_says_nothing() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    for answer in [ConnectError::Cancelled, ConnectError::HostKeyRejected] {
        fixture.press("Test connection");
        let (attempt, ..) = fixture.take_test();
        fixture.ui.test_finished(attempt, Err(answer));
        assert_eq!(fixture.test_text(), "");
        assert!(!fixture.ui.window().get_dialog_view().test_failed);
    }
}

#[test]
fn a_test_can_be_cancelled_and_its_answer_no_longer_counts() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Test connection");
    let (attempt, ..) = fixture.take_test();

    fixture.press("Cancel test");
    assert_eq!(fixture.take_sent(), [UiCommand::CancelTest { attempt }]);
    assert_eq!(fixture.test_text(), "");
    fixture
        .ui
        .test_finished(attempt, Ok(Duration::from_millis(1)));
    assert_eq!(fixture.test_text(), "");
}

#[test]
fn an_edit_stops_a_test_and_clears_what_a_test_said() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Test connection");
    let (attempt, ..) = fixture.take_test();

    fixture.type_in("Host", "other");
    assert_eq!(fixture.take_sent(), [UiCommand::CancelTest { attempt }]);
    assert!(!fixture.ui.window().get_dialog_view().testing);

    fixture.press("Test connection");
    let (attempt, ..) = fixture.take_test();
    fixture
        .ui
        .test_finished(attempt, Ok(Duration::from_millis(1)));
    assert_ne!(fixture.test_text(), "");
    fixture.type_in("Host", "again");
    assert_eq!(fixture.test_text(), "");
    assert!(fixture.take_sent().is_empty());
}

#[test]
fn closing_the_dialog_stops_a_test() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Test connection");
    let (attempt, ..) = fixture.take_test();
    fixture.key(Key::Escape);
    assert!(!fixture.dialog_is_open());
    assert_eq!(fixture.take_sent(), [UiCommand::CancelTest { attempt }]);
}

/// The prompter on a task, and the window on this thread, as a test has only the one.
struct Asking {
    fixture: Fixture,
    runtime: Runtime,
    prompter: Arc<UiPrompter>,
}

impl Asking {
    fn new() -> Self {
        let fixture = Fixture::new(vec![profile("A")]);
        let prompter = fixture.ui.prompter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        Self {
            fixture,
            runtime,
            prompter,
        }
    }

    /// Lets the tasks run, and the window read what they posted.
    fn settle(&self) {
        for _ in 0..8 {
            self.runtime.block_on(tokio::task::yield_now());
        }
        self.fixture.read_mail();
        for _ in 0..8 {
            self.runtime.block_on(tokio::task::yield_now());
        }
    }

    fn secret(
        &self,
        kind: SecretKind,
        allow_remember: bool,
    ) -> JoinHandle<Option<(String, Remember)>> {
        let prompter = Arc::clone(&self.prompter);
        let task = self.runtime.spawn(async move {
            let answer = prompter
                .secret(SecretRequest {
                    kind,
                    allow_remember,
                })
                .await;
            answer.map(|(secret, remember)| (secret.expose_secret().clone(), remember))
        });
        self.settle();
        task
    }

    fn password(&self) -> JoinHandle<Option<(String, Remember)>> {
        self.secret(
            SecretKind::Password {
                user: "deploy".to_owned(),
                host: "10.0.4.21".to_owned(),
            },
            true,
        )
    }

    fn host_key(&self) -> JoinHandle<HostKeyDecision> {
        let prompter = Arc::clone(&self.prompter);
        let task = self.runtime.spawn(async move {
            prompter
                .host_key(HostKeyRequest {
                    host: "10.0.4.21".to_owned(),
                    port: 22,
                    key_type: "ssh-ed25519".to_owned(),
                    fingerprint: "SHA256:Xq3Vb4Q8".to_owned(),
                })
                .await
        });
        self.settle();
        task
    }

    /// With a limit, as a prompt nobody closed would otherwise hang the test.
    fn answer<T>(&self, task: JoinHandle<T>) -> T {
        self.settle();
        let limit = Duration::from_secs(10);
        let waited = self
            .runtime
            .block_on(async { tokio::time::timeout(limit, task).await });
        waited.expect("the prompt was not answered").unwrap()
    }

    fn title(&self) -> String {
        self.fixture.ui.window().get_prompt_view().title.to_string()
    }
}

#[test]
fn the_secret_prompt_asks_with_the_titles_of_the_spec() {
    let asking = Asking::new();
    let task = asking.password();
    assert!(asking.fixture.prompt_is_open());
    assert_eq!(asking.title(), "Password for deploy@10.0.4.21");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);

    let task = asking.secret(
        SecretKind::Passphrase {
            key_name: "id_ed25519".to_owned(),
        },
        true,
    );
    assert_eq!(asking.title(), "Passphrase for id_ed25519");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
}

#[test]
fn a_secret_is_answered_with_what_was_typed_and_then_leaves_the_window() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    let task = asking.password();
    assert!(!fixture.is_enabled("OK"), "nothing has been typed");

    fixture.type_in("Password for deploy@10.0.4.21", "hunter2");
    assert!(fixture.is_enabled("OK"));
    fixture.press("OK");
    assert_eq!(
        asking.answer(task),
        Some(("hunter2".to_owned(), Remember::No))
    );
    assert!(!fixture.prompt_is_open());
    assert_eq!(fixture.ui.window().get_prompt_input().secret, "");

    let task = asking.password();
    assert_eq!(fixture.box_holds("Password for deploy@10.0.4.21"), "");
    fixture.type_in("Password for deploy@10.0.4.21", "second");
    fixture.press("Remember");
    fixture.press("OK");
    assert_eq!(
        asking.answer(task),
        Some(("second".to_owned(), Remember::Yes))
    );
}

#[test]
fn enter_in_the_box_answers_once_something_is_typed() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    let task = asking.password();
    fixture.key(Key::Return);
    assert!(fixture.prompt_is_open(), "nothing has been typed");
    fixture.key("x");
    fixture.key(Key::Return);
    assert_eq!(asking.answer(task), Some(("x".to_owned(), Remember::No)));
}

/// FR-SSH-05.
#[test]
fn where_nothing_may_be_stored_there_is_no_remember() {
    let asking = Asking::new();
    let task = asking.secret(
        SecretKind::Passphrase {
            key_name: "id".to_owned(),
        },
        false,
    );
    assert!(asking.fixture.labelled("Remember").is_none());
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
}

#[test]
fn the_caps_lock_hint_shows_when_it_is_on() {
    let asking = Asking::new();
    let task = asking.password();
    let window = asking.fixture.ui.window();
    window.set_caps_lock(true);
    assert!(asking.fixture.labelled("Caps Lock is on").is_some());
    window.set_caps_lock(false);
    assert!(asking.fixture.labelled("Caps Lock is on").is_none());
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
}

/// FR-MNT-11: the row's Cancel drops the call, and the dialog must not stay behind.
#[test]
fn dropping_the_call_closes_its_prompt() {
    let asking = Asking::new();
    let task = asking.password();
    assert!(asking.fixture.prompt_is_open());
    task.abort();
    asking.settle();
    asking.settle();
    assert!(!asking.fixture.prompt_is_open());
}

#[test]
fn dropping_a_call_that_is_waiting_only_takes_it_out_of_the_queue() {
    let asking = Asking::new();
    let first = asking.password();
    let second = asking.host_key();
    second.abort();
    asking.settle();
    asking.settle();
    assert!(asking.fixture.prompt_is_open());
    assert_eq!(asking.title(), "Password for deploy@10.0.4.21");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(first), None);
    assert!(!asking.fixture.prompt_is_open());
}

/// FR-MNT-08: profiles mounting together all ask through one prompter.
#[test]
fn prompts_come_one_at_a_time_in_order() {
    let asking = Asking::new();
    let first = asking.password();
    let second = asking.host_key();
    let third = asking.secret(
        SecretKind::Passphrase {
            key_name: "id".to_owned(),
        },
        true,
    );

    assert_eq!(asking.title(), "Password for deploy@10.0.4.21");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(first), None);

    assert_eq!(asking.title(), "Unknown server");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(second), HostKeyDecision::Reject);

    assert_eq!(asking.title(), "Passphrase for id");
    asking.fixture.key(Key::Escape);
    assert_eq!(asking.answer(third), None);
    assert!(!asking.fixture.prompt_is_open());
}

#[test]
fn the_host_key_prompt_shows_the_key_and_takes_each_answer() {
    let asking = Asking::new();
    let fixture = &asking.fixture;

    let task = asking.host_key();
    assert!(
        fixture
            .labelled("DriveMount has not connected to 10.0.4.21:22 before.")
            .is_some()
    );
    assert!(fixture.labelled("ssh-ed25519").is_some());
    assert!(fixture.labelled("SHA256:Xq3Vb4Q8").is_some());
    fixture.press("Trust and save");
    assert_eq!(asking.answer(task), HostKeyDecision::TrustSave);

    let task = asking.host_key();
    fixture.press("Trust once");
    assert_eq!(asking.answer(task), HostKeyDecision::TrustOnce);

    let task = asking.host_key();
    fixture.press("Cancel");
    assert_eq!(asking.answer(task), HostKeyDecision::Reject);

    let task = asking.host_key();
    fixture.key(Key::Return);
    assert_eq!(
        asking.answer(task),
        HostKeyDecision::Reject,
        "Enter is safe"
    );
}

fn round(texts: &[(&str, bool)]) -> KbdIntRequest {
    KbdIntRequest {
        user: "me".to_owned(),
        host: "h".to_owned(),
        name: String::new(),
        instructions: "Have your phone ready".to_owned(),
        prompts: texts
            .iter()
            .map(|(text, echo)| KbdIntPrompt {
                text: (*text).to_owned(),
                echo: *echo,
            })
            .collect(),
    }
}

/// FR-SSH-07: in the server's words, and with nothing to remember.
#[test]
fn a_keyboard_interactive_round_asks_each_question_with_the_servers_text() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    let prompter = Arc::clone(&asking.prompter);
    let task = asking.runtime.spawn(async move {
        let request = round(&[("Password:", false), ("Verification code:", true)]);
        let answers = prompter.keyboard_interactive(request).await;
        answers.map(|answers| {
            answers
                .iter()
                .map(|answer| answer.expose_secret().clone())
                .collect::<Vec<_>>()
        })
    });
    asking.settle();

    assert_eq!(asking.title(), "Password:");
    assert!(fixture.labelled("Remember").is_none());
    assert!(!fixture.ui.window().get_prompt_view().echo);
    assert_eq!(
        fixture.ui.window().get_prompt_view().message,
        "me@h\nHave your phone ready"
    );
    fixture.type_in("Password:", "pw");
    fixture.press("OK");

    assert!(fixture.prompt_is_open());
    assert_eq!(asking.title(), "Verification code:");
    assert!(fixture.ui.window().get_prompt_view().echo);
    assert_eq!(
        fixture.box_holds("Verification code:"),
        "",
        "not the last answer"
    );
    fixture.type_in("Verification code:", "123456");
    fixture.press("OK");

    assert_eq!(
        asking.answer(task),
        Some(vec!["pw".to_owned(), "123456".to_owned()])
    );
    assert!(!fixture.prompt_is_open());
}

#[test]
fn cancelling_a_round_cancels_all_of_it() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    let prompter = Arc::clone(&asking.prompter);
    let task = asking.runtime.spawn(async move {
        let request = round(&[("One:", false), ("Two:", false)]);
        prompter.keyboard_interactive(request).await.is_none()
    });
    asking.settle();
    fixture.type_in("One:", "x");
    fixture.press("OK");
    fixture.key(Key::Escape);
    assert!(asking.answer(task));
    assert!(!fixture.prompt_is_open());
}

#[test]
fn a_round_with_nothing_to_ask_shows_nothing() {
    let asking = Asking::new();
    let prompter = Arc::clone(&asking.prompter);
    let task = asking.runtime.spawn(async move {
        let answers = prompter.keyboard_interactive(round(&[])).await;
        answers.map(|answers| answers.len())
    });
    asking.settle();
    assert!(!asking.fixture.prompt_is_open());
    assert_eq!(asking.answer(task), Some(0));
}

fn login_task(asking: &Asking) -> JoinHandle<Option<(Option<PathBuf>, Option<String>)>> {
    let prompter = Arc::clone(&asking.prompter);
    let task = asking.runtime.spawn(async move {
        let answer = prompter
            .login(LoginRequest {
                user: "me".to_owned(),
                host: "h".to_owned(),
            })
            .await;
        answer.map(|answer| match answer {
            LoginAnswer::Password(password) => (None, Some(password.expose_secret().clone())),
            LoginAnswer::KeyFile { path, passphrase } => (
                Some(path),
                passphrase.map(|passphrase| passphrase.expose_secret().clone()),
            ),
        })
    });
    asking.settle();
    task
}

/// FR-SSH-05.
#[test]
fn an_ask_each_time_profile_is_asked_for_a_password_or_a_key_file() {
    let asking = Asking::new();
    let fixture = &asking.fixture;

    let task = login_task(&asking);
    assert_eq!(asking.title(), "Log in to me@h");
    assert!(fixture.labelled("Remember").is_none());
    assert!(!fixture.is_enabled("OK"));
    fixture.type_in("Log in to me@h", "pw");
    fixture.press("OK");
    assert_eq!(asking.answer(task), Some((None, Some("pw".to_owned()))));

    let task = login_task(&asking);
    fixture.press("Use a key file");
    assert!(!fixture.is_enabled("OK"), "no file yet");
    fixture.type_in("Key file", r"C:\keys\id");
    assert!(fixture.is_enabled("OK"), "a key may have no passphrase");
    fixture.press("OK");
    assert_eq!(
        asking.answer(task),
        Some((Some(PathBuf::from(r"C:\keys\id")), None))
    );

    let task = login_task(&asking);
    fixture.press("Use a key file");
    fixture.press("Choose key file");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::PickKeyFile {
            which: KeyFileBox::Login,
            title: "Choose a private key file".to_owned(),
        }]
    );
    fixture
        .ui
        .key_file_picked(KeyFileBox::Login, Some(PathBuf::from(r"C:\keys\other")));
    fixture.type_in("Passphrase", "phrase");
    fixture.press("OK");
    assert_eq!(
        asking.answer(task),
        Some((
            Some(PathBuf::from(r"C:\keys\other")),
            Some("phrase".to_owned())
        ))
    );

    let task = login_task(&asking);
    fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
}

fn changed_key() -> Box<ChangedHostKey> {
    Box::new(ChangedHostKey {
        host: "10.0.4.21".to_owned(),
        port: 22,
        old_fingerprint: "SHA256:old".to_owned(),
        new_fingerprint: "SHA256:new".to_owned(),
        known_hosts: PathBuf::from(r"C:\Users\me\.ssh\known_hosts"),
        line: 7,
    })
}

/// FR-SSH-10: all it should show, and no way to go ahead.
#[test]
fn a_changed_key_brings_up_its_dialog_with_no_way_to_trust_it() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    let error = MountError::Connect(ConnectError::HostKeyChanged(changed_key()));
    fixture.set_state(id, MountState::Error(error), None);

    assert!(fixture.prompt_is_open());
    let view = fixture.ui.window().get_prompt_view();
    assert_eq!(view.title, "Server key has changed: connection refused");
    assert_eq!(view.message, "10.0.4.21:22");
    assert!(fixture.labelled("SHA256:old").is_some());
    assert!(fixture.labelled("SHA256:new").is_some());
    assert!(
        fixture
            .labelled(r"C:\Users\me\.ssh\known_hosts line 7")
            .is_some()
    );
    assert!(view.advice.contains("remove line 7 from the file"));
    assert!(fixture.labelled("Trust once").is_none());
    assert!(fixture.labelled("Trust and save").is_none());

    fixture.press("Open file location");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::OpenFileLocation(PathBuf::from(
            r"C:\Users\me\.ssh\known_hosts"
        ))]
    );
    assert!(fixture.prompt_is_open(), "it says which line to remove");
    fixture.press("Close");
    assert!(!fixture.prompt_is_open());
}

/// The first version passed all its tests with the buttons below the panel's edge, as wrapped lines
/// were not counted in its height.
#[test]
fn the_panel_holds_all_it_shows_however_the_text_wraps() {
    let fixture = Fixture::new(Vec::new());
    let mut details = changed_key();
    details.known_hosts = PathBuf::from(format!(r"C:\{}\known_hosts", "long folder ".repeat(12)));
    fixture.ui.apply_event(&MountEvent {
        id: uuid::Uuid::new_v4(),
        state: MountState::Error(MountError::Connect(ConnectError::HostKeyChanged(details))),
        detail: None,
    });
    let close = fixture.labelled("Close").unwrap();
    let frame = ElementHandle::find_by_element_id(fixture.ui.window(), "PromptPanel::frame")
        .next()
        .unwrap();
    let bottom = |element: &ElementHandle| element.absolute_position().y + element.size().height;
    assert!(
        bottom(&close) <= bottom(&frame),
        "the button ends at {}, the panel at {}",
        bottom(&close),
        bottom(&frame)
    );
}

#[test]
fn another_error_brings_up_no_dialog() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    let error = MountError::Connect(ConnectError::Auth {
        user: "me".to_owned(),
        host: "h".to_owned(),
    });
    fixture.set_state(id, MountState::Error(error), None);
    assert!(!fixture.prompt_is_open());
}

#[test]
fn test_connection_brings_up_the_changed_key_dialog_too() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Test connection");
    let (attempt, ..) = fixture.take_test();
    fixture
        .ui
        .test_finished(attempt, Err(ConnectError::HostKeyChanged(changed_key())));
    assert!(fixture.prompt_is_open());
    assert_eq!(
        fixture.test_text(),
        "Server key has changed: connection refused."
    );
    fixture.key(Key::Escape);
    assert!(!fixture.prompt_is_open());
    assert!(fixture.dialog_is_open(), "Esc closes one at a time");
}

#[test]
fn a_changed_key_waits_its_turn_behind_a_prompt() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    let task = asking.password();
    let id = uuid::Uuid::new_v4();
    let error = MountError::Connect(ConnectError::HostKeyChanged(changed_key()));
    fixture.set_state(id, MountState::Error(error), None);

    assert_eq!(asking.title(), "Password for deploy@10.0.4.21");
    fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
    assert_eq!(asking.title(), "Server key has changed: connection refused");
}

/// Test connection asks through the same prompter, with the dialog behind.
#[test]
fn a_prompt_goes_in_front_of_the_dialog_and_esc_closes_only_it() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    fixture.open_new();
    assert!(fixture.box_is_enabled("Name"));

    let task = asking.password();
    assert!(fixture.prompt_is_open());
    assert!(
        !fixture.box_is_enabled("Name"),
        "Tab must not reach behind it"
    );
    fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
    assert!(!fixture.prompt_is_open());
    assert!(fixture.dialog_is_open());
    assert!(fixture.box_is_enabled("Name"));
    fixture.press_front("Cancel");
    assert!(!fixture.dialog_is_open());
}

#[test]
fn a_prompt_goes_in_front_of_the_delete_question_too() {
    let asking = Asking::new();
    let fixture = &asking.fixture;
    fixture.key(Key::DownArrow);
    fixture.key(Key::Delete);
    assert_ne!(fixture.ui.window().get_question(), "");

    let task = asking.password();
    fixture.key(Key::Escape);
    assert_eq!(asking.answer(task), None);
    assert_ne!(fixture.ui.window().get_question(), "", "it is still asked");
    assert!(fixture.take_sent().is_empty());
}

#[test]
fn when_the_window_goes_whoever_is_waiting_is_answered_with_a_cancel() {
    let asking = Asking::new();
    let first = asking.password();
    let second = asking.password();
    let Asking {
        fixture,
        runtime,
        prompter,
    } = asking;
    drop(fixture);
    let limit = Duration::from_secs(10);
    let first = runtime.block_on(async { tokio::time::timeout(limit, first).await });
    assert_eq!(first.unwrap().unwrap(), None);
    let second = runtime.block_on(async { tokio::time::timeout(limit, second).await });
    assert_eq!(second.unwrap().unwrap(), None);

    let late = prompter.secret(SecretRequest {
        kind: SecretKind::Passphrase {
            key_name: "id".to_owned(),
        },
        allow_remember: false,
    });
    let late = runtime.block_on(async { tokio::time::timeout(limit, late).await });
    assert!(late.unwrap().is_none());
}

#[test]
fn typed_secrets_compare_by_slot_and_never_by_value() {
    use drivemount_vfs::{Secret, SecretSlot};
    let typed = |slot, text: &str| TypedSecrets(vec![(slot, Secret::new(text.to_owned()))]);
    assert_eq!(
        typed(SecretSlot::Password, "a"),
        typed(SecretSlot::Password, "b")
    );
    assert_ne!(
        typed(SecretSlot::Password, "a"),
        typed(SecretSlot::JumpPassword, "a")
    );
    assert!(!format!("{:?}", typed(SecretSlot::Password, "hunter2")).contains("hunter2"));
}

/// Config has to be usable the way M1-18 will use it: a saved profile shows up, and the dialog follows.
#[test]
fn a_save_that_stored_nothing_still_becomes_an_edit_when_the_file_has_it() {
    let fixture = Fixture::new(Vec::new());
    fixture.open_new();
    fixture.fill_in_a_valid_new_profile();
    fixture.press("Save");
    let (saved, _) = fixture.take_save();
    fixture.ui.set_config(Config {
        profiles: vec![saved],
        ..Config::default()
    });
    fixture
        .ui
        .save_finished(Err(ProfileChangeError::StoringSecret(io::Error::other(
            "too long",
        ))));
    assert!(fixture.dialog_is_open());
    assert_eq!(
        fixture.ui.window().get_dialog_view().title,
        "Edit connection"
    );
    assert_eq!(
        fixture.ui.window().get_marks().name,
        "",
        "not against itself"
    );
}
