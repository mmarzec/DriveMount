// SPDX-License-Identifier: GPL-3.0-or-later

//! The window itself, through Slint's testing back-end: real markup, real key and pointer events, and
//! no screen. What the list holds is tested in `list.rs`, and the profile dialog in `tests/dialog.rs`.

mod dialog;

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use fsm_core::config::{Config, ConfigStore, ReadOnlyReason, Theme};
use fsm_core::manager::{MountDetail, MountEvent, MountState};
use fsm_core::profile::{DriveChoice, Profile, Protocol, SftpAuth, SftpProfile};
use fsm_vfs::{ChangedHostKey, ConnectError, DriveLetter, MountError, RevokedHostKey};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, Model, SharedString};
use uuid::Uuid;

use super::errors::{read_only_text, setting_problems_text, shown_error};
use super::{ActionFailure, MainUi, RowStatus, Texts, UiCommand};

thread_local! {
    static BACKEND_READY: Cell<bool> = const { Cell::new(false) };
}

/// Each test thread has its own back-end, and `--test-threads=1` runs them all on one.
fn init_backend() {
    if !BACKEND_READY.replace(true) {
        i_slint_backend_testing::init_no_event_loop();
    }
}

struct Fixture {
    ui: MainUi,
    sent: Rc<RefCell<Vec<UiCommand>>>,
}

impl Fixture {
    fn with_path(path: Option<PathBuf>) -> Self {
        init_backend();
        let sent = Rc::new(RefCell::new(Vec::new()));
        let recorder = Rc::clone(&sent);
        let ui =
            MainUi::with_remembered_path(move |command| recorder.borrow_mut().push(command), path)
                .unwrap();
        ui.window()
            .window()
            .set_size(LogicalSize::new(640.0, 420.0));
        ui.window().show().unwrap();
        Self { ui, sent }
    }

    fn new(profiles: Vec<Profile>) -> Self {
        let fixture = Self::with_path(None);
        fixture.ui.set_config(Config {
            profiles,
            ..Config::default()
        });
        fixture
    }

    fn take_sent(&self) -> Vec<UiCommand> {
        self.sent.borrow_mut().drain(..).collect()
    }

    fn dispatch(&self, event: WindowEvent) {
        self.ui.window().window().dispatch_event(event);
    }

    /// What rendering a frame would do first: makes what has just appeared, which is what takes the
    /// focus. Without it, keys would go to something that has just gone away.
    fn make_what_appeared(&self) {
        let _ = ElementQuery::from_root(self.ui.window())
            .match_descendants()
            .find_all();
    }

    fn key(&self, key: impl Into<SharedString>) {
        self.make_what_appeared();
        let text = key.into();
        self.dispatch(WindowEvent::KeyPressed { text: text.clone() });
        self.dispatch(WindowEvent::KeyReleased { text });
    }

    fn ctrl(&self, key: impl Into<SharedString>) {
        let control = SharedString::from(Key::Control);
        self.dispatch(WindowEvent::KeyPressed {
            text: control.clone(),
        });
        self.key(key);
        self.dispatch(WindowEvent::KeyReleased { text: control });
    }

    fn labelled(&self, label: &str) -> Option<ElementHandle> {
        self.make_what_appeared();
        ElementHandle::find_by_accessible_label(self.ui.window(), label).next()
    }

    fn press(&self, label: &str) {
        self.labelled(label)
            .unwrap_or_else(|| panic!("nothing is labelled {label}"))
            .invoke_accessible_default_action();
    }

    fn is_enabled(&self, label: &str) -> bool {
        self.labelled(label)
            .unwrap_or_else(|| panic!("nothing is labelled {label}"))
            .accessible_enabled()
            .unwrap()
    }

    fn entry(&self, place: usize) -> super::ListEntry {
        self.ui.window().get_entries().row_data(place).unwrap()
    }

    fn set_state(&self, id: Uuid, state: MountState, on: Option<char>) {
        self.ui.apply_event(&MountEvent {
            id,
            state,
            detail: on.map(|on| MountDetail {
                letter: letter(on),
                instead_of: None,
            }),
        });
    }

    fn click_at(&self, element: &ElementHandle, count: u32) {
        let at = element.absolute_position();
        // Away from the row's buttons, which are at its right.
        let position = LogicalPosition::new(at.x + 60.0, at.y + 20.0);
        for _ in 0..count {
            self.dispatch(WindowEvent::PointerMoved { position });
            self.dispatch(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            self.dispatch(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        }
    }

    /// The row of this profile, as a screen reader finds it.
    fn row(&self, name: &str) -> ElementHandle {
        let found = ElementQuery::from_root(self.ui.window())
            .match_descendants()
            .match_accessible_role(AccessibleRole::ListItem)
            .find_all();
        found
            .into_iter()
            .find(|row| {
                row.accessible_label()
                    .is_some_and(|label| label.starts_with(&format!("{name},")))
            })
            .unwrap_or_else(|| panic!("no row for {name}"))
    }
}

fn letter(letter: char) -> DriveLetter {
    DriveLetter::new(letter).unwrap()
}

fn profile(name: &str) -> Profile {
    let sftp = SftpProfile::new("host", "me", SftpAuth::Agent);
    Profile::new(name, Protocol::Sftp(sftp))
}

fn profiles(names: &[&str]) -> Vec<Profile> {
    names.iter().map(|name| profile(name)).collect()
}

#[test]
fn a_row_shows_its_name_address_and_preferred_letter() {
    let mut saved = profile("Production");
    saved.drive = DriveChoice::Letter(letter('X'));
    let fixture = Fixture::new(vec![saved]);
    let entry = fixture.entry(0);
    assert_eq!(entry.name, "Production");
    assert_eq!(entry.address, "me@host:~");
    assert_eq!(entry.letter, "X:");
    assert!(!entry.letter_is_live);
    assert_eq!(entry.status, RowStatus::Disconnected);
    assert_eq!(fixture.ui.window().get_status_text(), "Nothing mounted");
}

/// A row sized by its contents is narrow and centred, with its name cut down to nothing.
#[test]
fn a_row_fills_the_list_and_lines_up_with_the_others() {
    let fixture = Fixture::new(profiles(&[
        "a short name",
        "b much longer than that one is",
    ]));
    let (first, second) = (
        fixture.row("a short name"),
        fixture.row("b much longer than that one is"),
    );
    assert!(first.size().width > 600.0, "{:?}", first.size());
    assert_eq!(first.absolute_position().x, second.absolute_position().x);
    assert_eq!(first.size().width, second.size().width);
    assert_eq!(first.size().height, 40.0);
    assert_eq!(
        second.absolute_position().y - first.absolute_position().y,
        40.0
    );
}

#[test]
fn the_button_follows_the_state_and_sends_the_right_command() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);

    fixture.press("Mount A");
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(id)]);

    fixture.set_state(id, MountState::Connecting, None);
    fixture.press("Cancel A");
    assert_eq!(fixture.take_sent(), [UiCommand::Cancel(id)]);

    fixture.set_state(id, MountState::Mounted, Some('Z'));
    assert_eq!(fixture.entry(0).letter, "Z:");
    assert!(fixture.entry(0).letter_is_live);
    fixture.press("Unmount A");
    assert_eq!(fixture.take_sent(), [UiCommand::Unmount(id)]);

    fixture.set_state(id, MountState::Unmounting, Some('Z'));
    assert!(!fixture.is_enabled("Unmount A"));

    fixture.set_state(id, MountState::Error(MountError::NoFreeLetter), None);
    assert_eq!(fixture.entry(0).line, "No free drive letter.");
    assert!(fixture.entry(0).line_is_problem);
    fixture.press("Retry A");
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(id)]);
}

#[test]
fn the_status_bar_counts_as_in_the_mock_up() {
    let list = profiles(&["a", "b", "c", "d"]);
    let ids: Vec<Uuid> = list.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(list);
    fixture.set_state(ids[0], MountState::Mounted, Some('Z'));
    fixture.set_state(ids[1], MountState::Mounted, Some('Y'));
    fixture.set_state(ids[2], MountState::Connecting, None);
    assert_eq!(
        fixture.ui.window().get_status_text(),
        "2 mounted · 1 connecting"
    );
    fixture.set_state(ids[3], MountState::Error(MountError::NoFreeLetter), None);
    assert_eq!(
        fixture.ui.window().get_status_text(),
        "2 mounted · 1 connecting · 1 failed"
    );
}

#[test]
fn mount_all_and_unmount_all_send_one_command_each() {
    let list = profiles(&["a", "b", "c"]);
    let ids: Vec<Uuid> = list.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(list);
    assert!(!fixture.is_enabled("Unmount all"));

    fixture.set_state(ids[0], MountState::Mounted, Some('Z'));
    fixture.press("Mount all");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::Mount(ids[1]), UiCommand::Mount(ids[2])]
    );
    fixture.press("Unmount all");
    assert_eq!(fixture.take_sent(), [UiCommand::Unmount(ids[0])]);

    fixture.set_state(ids[1], MountState::Mounted, Some('Y'));
    fixture.set_state(ids[2], MountState::Mounted, Some('X'));
    assert!(!fixture.is_enabled("Mount all"));
}

#[test]
fn the_empty_state_offers_to_add() {
    let fixture = Fixture::new(Vec::new());
    assert!(fixture.ui.window().get_no_connections());
    fixture.press("Add connection");
    assert!(fixture.ui.window().get_dialog_open());
    assert_eq!(fixture.take_sent(), [UiCommand::FindFreeLetters]);
    fixture.key(Key::Escape);

    fixture.ui.set_config(Config {
        profiles: profiles(&["a"]),
        ..Config::default()
    });
    assert!(!fixture.ui.window().get_no_connections());
}

#[test]
fn without_winfsp_the_banner_shows_and_mounting_is_off_and_editing_is_not() {
    let fixture = Fixture::new(vec![profile("A")]);
    let banner = "WinFsp 2.1 or later is required to mount SSH drives";
    assert!(fixture.labelled(banner).is_none());

    fixture.ui.set_winfsp_ready(false);
    assert!(fixture.labelled(banner).is_some());
    assert!(!fixture.is_enabled("Mount A"));
    assert!(!fixture.is_enabled("Mount all"));
    fixture.press("Download WinFsp");
    assert_eq!(fixture.take_sent(), [UiCommand::DownloadWinFsp]);

    fixture.key(Key::DownArrow);
    fixture.key(Key::Return);
    assert!(fixture.take_sent().is_empty(), "Enter must not mount");
    fixture.ctrl("e");
    assert!(fixture.ui.window().get_dialog_open());
    fixture.key(Key::Escape);
    assert_eq!(fixture.take_sent(), [UiCommand::FindFreeLetters]);

    fixture.ui.set_winfsp_ready(true);
    assert!(fixture.labelled(banner).is_none());
    assert!(fixture.is_enabled("Mount A"));
}

#[test]
fn a_failed_action_has_a_banner_until_it_is_taken_back() {
    let fixture = Fixture::new(vec![profile("A")]);
    let failure = ActionFailure::Delete("the disk is full".to_owned());
    let shown = "The connection could not be deleted: the disk is full";

    fixture.ui.set_action_failure(Some(&failure));
    assert!(fixture.labelled(shown).is_some());

    let failure = ActionFailure::Mount("this connection needs correcting first".to_owned());
    fixture.ui.set_action_failure(Some(&failure));
    assert!(fixture.labelled(shown).is_none());
    let shown = "The drive could not be mounted: this connection needs correcting first";
    assert!(fixture.labelled(shown).is_some());

    fixture.ui.set_action_failure(None);
    assert!(fixture.labelled(shown).is_none());
}

#[test]
fn a_read_only_file_shows_why_and_stops_adding_and_deleting() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture
        .ui
        .set_read_only(Some(&ReadOnlyReason::Newer { found: 99 }));
    let text = fixture.ui.window().get_read_only_text();
    assert!(text.contains("newer FS Manager"), "{text}");

    assert!(!fixture.is_enabled("Add connection"));
    fixture.ctrl("n");
    assert!(fixture.take_sent().is_empty());

    fixture.key(Key::DownArrow);
    fixture.key(Key::Delete);
    assert_eq!(fixture.ui.window().get_question(), "");

    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(id)]);

    fixture.ui.set_read_only(None);
    assert_eq!(fixture.ui.window().get_read_only_text(), "");
    assert!(fixture.is_enabled("Add connection"));
}

#[test]
fn the_arrows_move_the_selection_and_enter_acts_on_it() {
    let list = profiles(&["a", "b"]);
    let ids: Vec<Uuid> = list.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(list);
    assert_eq!(fixture.ui.window().get_selected_index(), -1);
    fixture.key(Key::Return);
    assert!(fixture.take_sent().is_empty(), "nothing is selected");

    fixture.key(Key::DownArrow);
    assert_eq!(fixture.ui.window().get_selected_index(), 0);
    fixture.key(Key::DownArrow);
    fixture.key(Key::DownArrow);
    assert_eq!(fixture.ui.window().get_selected_index(), 1);
    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(ids[1])]);

    fixture.set_state(ids[1], MountState::Mounted, Some('Z'));
    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Unmount(ids[1])]);

    fixture.key(Key::UpArrow);
    assert_eq!(fixture.ui.window().get_selected_index(), 0);
}

#[test]
fn the_selection_is_kept_in_view() {
    let names: Vec<String> = (10..40).map(|n| format!("p{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let fixture = Fixture::new(profiles(&names));
    let window = fixture.ui.window();
    assert_eq!(window.get_list_scroll(), 0.0);
    assert!(window.get_list_visible_height() > 0.0);

    for _ in 0..30 {
        fixture.key(Key::DownArrow);
        let top = window.get_selected_top();
        let bottom = top + window.get_selected_height();
        let scroll = window.get_list_scroll();
        assert!(top >= scroll, "{top} is above {scroll}");
        assert!(
            bottom <= scroll + window.get_list_visible_height() + 0.5,
            "{bottom} is below the view at {scroll}"
        );
    }
    assert!(window.get_list_scroll() > 0.0);
    for _ in 0..30 {
        fixture.key(Key::UpArrow);
    }
    assert_eq!(window.get_list_scroll(), 0.0);
}

#[test]
fn the_shortcuts_of_the_table_work() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);

    let opens_the_dialog = |title: &str| {
        let window = fixture.ui.window();
        assert!(window.get_dialog_open());
        assert_eq!(window.get_dialog_view().title, title);
        assert_eq!(fixture.take_sent(), [UiCommand::FindFreeLetters]);
        fixture.key(Key::Escape);
        assert!(!window.get_dialog_open());
    };
    fixture.ctrl("n");
    opens_the_dialog("Add connection");
    fixture.ctrl("N");
    opens_the_dialog("Add connection");

    fixture.ctrl("e");
    assert!(fixture.take_sent().is_empty(), "nothing is selected");
    assert!(!fixture.ui.window().get_dialog_open());
    fixture.key(Key::DownArrow);
    fixture.ctrl("e");
    opens_the_dialog("Edit connection");

    fixture.ctrl(Key::Return);
    assert!(fixture.take_sent().is_empty(), "it is not mounted");
    fixture.set_state(id, MountState::Mounted, Some('Q'));
    fixture.ctrl(Key::Return);
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::OpenInExplorer(letter('Q'))]
    );

    fixture.key(Key::Escape);
    assert_eq!(fixture.take_sent(), [UiCommand::HideWindow]);
}

#[test]
fn a_letter_without_ctrl_does_nothing() {
    let fixture = Fixture::new(profiles(&["A"]));
    fixture.key("n");
    fixture.key("e");
    assert!(fixture.take_sent().is_empty());
}

#[test]
fn delete_asks_first_and_escape_or_cancel_keep_the_profile() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    let window = fixture.ui.window();

    fixture.key(Key::Delete);
    assert_eq!(window.get_question(), "", "nothing is selected");

    fixture.key(Key::DownArrow);
    fixture.key(Key::Delete);
    assert!(window.get_question().contains("\"A\""));
    assert!(fixture.take_sent().is_empty(), "it must ask first");

    // Nothing else works while it asks, and Esc only closes the question.
    fixture.ctrl("n");
    fixture.key(Key::DownArrow);
    assert!(fixture.take_sent().is_empty());
    assert!(!fixture.is_enabled("Mount A"));
    assert!(!fixture.is_enabled("Add connection"));
    fixture.key(Key::Escape);
    assert_eq!(window.get_question(), "");
    assert!(fixture.take_sent().is_empty());
    assert!(fixture.is_enabled("Mount A"));

    // The focus starts on Cancel, so Enter is the safe answer.
    fixture.key(Key::Delete);
    fixture.key(Key::Return);
    assert_eq!(window.get_question(), "");
    assert!(fixture.take_sent().is_empty());

    fixture.key(Key::Delete);
    fixture.press("Delete");
    assert_eq!(window.get_question(), "");
    assert_eq!(fixture.take_sent(), [UiCommand::Delete(id)]);
}

#[test]
fn deleting_a_mounted_profile_says_it_unmounts_first() {
    let saved = profile("A");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.set_state(id, MountState::Mounted, Some('Z'));
    fixture.key(Key::DownArrow);
    fixture.key(Key::Delete);
    assert!(
        fixture
            .ui
            .window()
            .get_question()
            .starts_with("Unmount and delete")
    );
}

#[test]
fn a_file_entry_that_cannot_be_read_has_a_row_and_can_only_be_deleted() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("config.toml");
    fs::write(
        &path,
        "schema_version = 1\n\
         [[profiles]]\n\
         id = \"6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10\"\n\
         name = \"Broken\"\n\
         protocol = \"sftp\"\n\
         host = \"h\"\n\
         port = 70000\n\
         username = \"u\"\n\
         auth = \"agent\"\n",
    )
    .unwrap();
    let store = ConfigStore::open(path);
    let unreadable = store.config().unreadable.clone();
    assert_eq!(unreadable.len(), 1);

    let fixture = Fixture::with_path(None);
    fixture.ui.set_config(store.config().clone());
    let entry = fixture.entry(0);
    assert_eq!(entry.name, "Broken");
    assert!(entry.line.starts_with("This connection could not be read"));
    assert!(!entry.can_edit);
    assert!(entry.can_delete);

    fixture.key(Key::DownArrow);
    fixture.key(Key::Return);
    fixture.ctrl("e");
    assert!(fixture.take_sent().is_empty());
    fixture.key(Key::Delete);
    assert!(fixture.ui.window().get_question().contains("Broken"));
    fixture.press("Delete");
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::DeleteUnreadable(unreadable[0].clone())]
    );
}

#[test]
fn a_click_selects_and_a_double_click_mounts_or_opens() {
    let list = profiles(&["a", "b"]);
    let id = list[0].id;
    let fixture = Fixture::new(list);

    fixture.click_at(&fixture.row("b"), 1);
    assert_eq!(fixture.ui.window().get_selected_index(), 1);
    assert!(fixture.take_sent().is_empty());
    // Taking the focus back is what lets the keys work after using the mouse.
    fixture.key(Key::UpArrow);
    assert_eq!(fixture.ui.window().get_selected_index(), 0);

    fixture.click_at(&fixture.row("a"), 2);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(id)]);

    fixture.set_state(id, MountState::Connecting, None);
    fixture.click_at(&fixture.row("a"), 2);
    assert!(fixture.take_sent().is_empty(), "already on its way");

    fixture.set_state(id, MountState::Mounted, Some('Z'));
    fixture.click_at(&fixture.row("a"), 2);
    assert_eq!(
        fixture.take_sent(),
        [UiCommand::OpenInExplorer(letter('Z'))]
    );
}

/// Otherwise Enter goes on pressing the button that had the focus, on another row.
#[test]
fn a_click_on_a_row_takes_the_focus_from_a_button() {
    let list = profiles(&["a", "b"]);
    let ids: Vec<Uuid> = list.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(list);

    // Add, Mount all, Settings (Unmount all is disabled), then the first row's button.
    for _ in 0..4 {
        fixture.key(Key::Tab);
    }
    fixture.click_at(&fixture.row("b"), 1);
    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(ids[1])]);
}

#[test]
fn a_status_event_leaves_the_focus_on_the_row_button() {
    let list = profiles(&["a", "b"]);
    let ids: Vec<Uuid> = list.iter().map(|p| p.id).collect();
    let fixture = Fixture::new(list);

    // Add, Mount all, Settings (Unmount all is disabled), then the first row's button.
    for _ in 0..4 {
        fixture.key(Key::Tab);
    }
    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(ids[0])]);

    fixture.set_state(ids[1], MountState::Connecting, None);
    fixture.key(Key::Return);
    assert_eq!(fixture.take_sent(), [UiCommand::Mount(ids[0])]);
}

#[test]
fn every_button_has_a_name_for_screen_readers() {
    let list = profiles(&["a", "b"]);
    let id = list[0].id;
    let fixture = Fixture::new(list);
    fixture.ui.set_winfsp_ready(false);
    fixture.set_state(id, MountState::Mounted, Some('Z'));

    let buttons = ElementQuery::from_root(fixture.ui.window())
        .match_descendants()
        .match_accessible_role(AccessibleRole::Button)
        .find_all();
    assert!(buttons.len() >= 9, "only {} buttons", buttons.len());
    let mut labels = Vec::new();
    for button in buttons {
        let label = button.accessible_label().unwrap_or_default();
        assert!(!label.is_empty());
        labels.push(label.to_string());
    }
    // What is on each row's buttons says which row it is.
    assert!(labels.iter().any(|label| label == "Unmount a"));
    assert!(labels.iter().any(|label| label == "More actions for b"));
    assert!(labels.iter().any(|label| label == "Settings"));
}

#[test]
fn a_row_reads_out_its_state_and_address() {
    let saved = profile("Prod");
    let id = saved.id;
    let fixture = Fixture::new(vec![saved]);
    fixture.set_state(id, MountState::Mounted, Some('Z'));
    let label = fixture.row("Prod").accessible_label().unwrap();
    assert_eq!(label, "Prod, mounted, me@host:~, Z:");
    assert_eq!(fixture.row("Prod").accessible_item_selected(), Some(false));
    fixture.key(Key::DownArrow);
    assert_eq!(fixture.row("Prod").accessible_item_selected(), Some(true));
}

#[test]
fn the_theme_can_be_forced_either_way_and_handed_back() {
    let fixture = Fixture::new(Vec::new());
    fixture.ui.set_theme(Theme::Dark);
    assert!(fixture.ui.window().get_is_dark());
    fixture.ui.set_theme(Theme::Light);
    assert!(!fixture.ui.window().get_is_dark());
    fixture.ui.set_theme(Theme::Dark);
    fixture.ui.set_theme(Theme::System);
    assert!(!fixture.ui.window().get_is_dark());
}

#[test]
fn closing_sends_the_command_and_remembers_the_size_for_next_time() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("ui-state.toml");

    let fixture = Fixture::with_path(Some(path.clone()));
    fixture
        .ui
        .window()
        .window()
        .set_size(LogicalSize::new(700.0, 500.0));
    fixture.dispatch(WindowEvent::CloseRequested);
    assert_eq!(fixture.take_sent(), [UiCommand::CloseWindow]);
    drop(fixture);

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("width = 700"), "{text}");

    let again = Fixture::with_path(Some(path));
    again
        .ui
        .window()
        .window()
        .set_size(LogicalSize::new(1.0, 1.0));
    again.ui.show().unwrap();
    let size = again.ui.window().window().size();
    assert_eq!((size.width, size.height), (700, 500));
}

#[test]
fn escape_remembers_the_size_too() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("ui-state.toml");
    let fixture = Fixture::with_path(Some(path.clone()));
    fixture.key(Key::Escape);
    assert_eq!(fixture.take_sent(), [UiCommand::HideWindow]);
    assert!(fs::read_to_string(&path).unwrap().contains("width = 640"));
}

/// Every error of the catalogue, with what spec 7.8 says and its code.
#[test]
fn each_error_has_the_text_and_code_of_the_catalogue() {
    init_backend();
    let window = super::MainWindow::new().unwrap();
    let texts = window.global::<Texts>();
    let connect = |error| MountError::Connect(error);
    let host = || "nas".to_owned();
    let cases: Vec<(MountError, &str, &str, &str)> = vec![
        (
            connect(ConnectError::Auth {
                user: "me".to_owned(),
                host: host(),
            }),
            "E-AUTH",
            "Authentication failed for me@nas.",
            "Check the user name and password or key.",
        ),
        (
            connect(ConnectError::HostKeyChanged(Box::new(ChangedHostKey {
                host: host(),
                port: 22,
                old_fingerprint: String::new(),
                new_fingerprint: String::new(),
                known_hosts: PathBuf::new(),
                line: 1,
            }))),
            "E-HOSTKEY-CHANGED",
            "Server key has changed: connection refused.",
            "",
        ),
        (
            connect(ConnectError::HostKeyRevoked(Box::new(RevokedHostKey {
                host: host(),
                port: 22,
                fingerprint: String::new(),
                known_hosts: PathBuf::new(),
                line: 7,
            }))),
            "E-HOSTKEY-REVOKED",
            "Server key has been revoked: connection refused.",
            "known_hosts line 7 marks this key as revoked. Ask the server's administrator for its current key.",
        ),
        (
            connect(ConnectError::HostKeyRejected),
            "E-HOSTKEY-REJECTED",
            "Server key not trusted.",
            "Mount again to see the prompt.",
        ),
        (
            connect(ConnectError::Dns { host: host() }),
            "E-DNS",
            "Host nas not found.",
            "Check the host name.",
        ),
        (
            connect(ConnectError::HostUnreachable {
                host: host(),
                port: 2222,
            }),
            "E-HOST-UNREACHABLE",
            "Cannot reach nas:2222.",
            "Check the network, VPN or firewall.",
        ),
        (
            connect(ConnectError::Timeout { host: host() }),
            "E-TIMEOUT",
            "nas did not respond in time.",
            "",
        ),
        (
            connect(ConnectError::PathNotFound {
                path: "/x".to_owned(),
            }),
            "E-PATH-NOT-FOUND",
            "Remote path /x not found.",
            "Edit the connection and check the remote path.",
        ),
        (
            connect(ConnectError::PathNotDir {
                path: "/x".to_owned(),
            }),
            "E-PATH-NOT-DIR",
            "Remote path /x is not a folder.",
            "",
        ),
        (
            connect(ConnectError::SftpUnavailable),
            "E-SFTP-UNAVAILABLE",
            "The server does not provide SFTP.",
            "Ask the administrator to enable the SFTP subsystem.",
        ),
        (
            connect(ConnectError::NoCommonAlgorithm),
            "E-ALGORITHMS",
            "No encryption method in common with the server.",
            "The server may offer only outdated algorithms. Ask its administrator to update its SSH settings.",
        ),
        (
            connect(ConnectError::KeyFormat),
            "E-KEY-FORMAT",
            "Unsupported key file format.",
            "Convert a PuTTY key to OpenSSH format with PuTTYgen.",
        ),
        (
            connect(ConnectError::ConnectionLost),
            "E-CONN-LOST",
            "Connection lost.",
            "",
        ),
        (
            MountError::LetterTaken(letter('Z')),
            "E-LETTER-TAKEN",
            "Drive Z: is already in use.",
            "Choose another letter or set Drive to Auto.",
        ),
        (
            MountError::NoFreeLetter,
            "E-NO-FREE-LETTER",
            "No free drive letter.",
            "",
        ),
        (
            MountError::WinFspMissing,
            "E-WINFSP-MISSING",
            "WinFsp 2.1 or later is required.",
            "",
        ),
    ];
    for (error, code, text, hint) in cases {
        let shown = shown_error(&texts, &error);
        assert_eq!(shown.code, code);
        assert_eq!(shown.text, text);
        assert_eq!(shown.hint, hint);
    }
}

#[test]
fn details_meant_for_the_log_are_not_shown() {
    init_backend();
    let window = super::MainWindow::new().unwrap();
    let texts = window.global::<Texts>();
    for error in [
        MountError::Other("secret detail".to_owned()),
        MountError::Connect(ConnectError::Other("secret detail".to_owned())),
    ] {
        let shown = shown_error(&texts, &error);
        assert_eq!(shown.code, "E-INTERNAL");
        assert_eq!(shown.line(), "Unexpected error. See the log for details.");
    }
}

#[test]
fn a_banner_says_what_to_do_and_takes_one_line_of_a_long_detail() {
    init_backend();
    let window = super::MainWindow::new().unwrap();
    let texts = window.global::<Texts>();
    let text = read_only_text(
        &texts,
        &ReadOnlyReason::Damaged {
            detail: "bad value at line 3\n  | port = \n  | ^^^".to_owned(),
        },
    );
    assert!(text.contains("bad value at line 3"), "{text}");
    assert!(!text.contains("port ="), "{text}");
    assert!(text.contains("config.toml.bak"), "{text}");

    for reason in [
        ReadOnlyReason::CannotRead {
            detail: "denied".to_owned(),
        },
        ReadOnlyReason::MigrationFailed {
            from: 3,
            detail: "denied".to_owned(),
        },
    ] {
        assert!(read_only_text(&texts, &reason).contains("denied"));
    }

    assert_eq!(setting_problems_text(&texts, &[]), "");
}
