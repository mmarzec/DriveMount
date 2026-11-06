// SPDX-License-Identifier: GPL-3.0-or-later

//! The main window with made-up connections and a pretend engine, for looking at it (H-06). Nothing is
//! mounted, and nothing the user has saved is read or changed. Not shipped. For the dialog's prompts,
//! which need a real connection, see `examples/connect.rs`.
//!
//! ```text
//! cargo run -p fsm --example main_window -- [--empty] [--no-winfsp] [--read-only] [--light|--dark] [--many]
//! ```
//!
//! Set `SLINT_SCALE_FACTOR=1.5` to see it at 150% without changing the display's settings. Esc and the
//! close button end it, as there is no tray yet. Its size and place are kept in a file in the temp
//! folder.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    preview::run()
}

#[cfg(windows)]
mod preview {
    use std::cell::OnceCell;
    use std::env;
    use std::error::Error;
    use std::rc::Rc;
    use std::time::Duration;

    use fsm_app::ui::{MainUi, UiCommand};
    use fsm_core::config::{Config, ConfigStore, Theme};
    use fsm_core::manager::{MountDetail, MountEvent, MountState};
    use fsm_vfs::{ConnectError, DriveLetter, MountError};
    use fsm_win::drives::is_letter_free;
    use fsm_win::shell::pick_file;
    use slint::Timer;
    use uuid::Uuid;

    const CONFIG: &str = r#"
[[profiles]]
id = "00000000-0000-4000-8000-000000000001"
name = "Production"
protocol = "sftp"
host = "10.0.4.21"
username = "deploy"
auth = "agent"
remote_path = "/var/www"
drive_letter = "Z"

[[profiles]]
id = "00000000-0000-4000-8000-000000000002"
name = "Staging"
protocol = "sftp"
host = "stage"
username = "deploy"
auth = "agent"
remote_path = "/srv"
drive_letter = "Y"

[[profiles]]
id = "00000000-0000-4000-8000-000000000003"
name = "NAS backup"
protocol = "sftp"
host = "nas"
username = "marcin"
auth = "agent"
drive_letter = "X"

[[profiles]]
id = "00000000-0000-4000-8000-000000000004"
name = "Dev box"
protocol = "sftp"
host = "devbox"
username = "me"
auth = "agent"
remote_path = "/home/me"

[[profiles]]
id = "00000000-0000-4000-8000-000000000005"
name = "A connection with quite a long name, to see where it is cut off"
protocol = "sftp"
host = "a-rather-long-host-name.internal.example.com"
port = 2222
username = "somebody"
auth = "agent"
remote_path = "/a/remote/path/that/goes/on/for/some/time"

[[profiles]]
id = "00000000-0000-4000-8000-000000000006"
name = "Needs correcting"
protocol = "sftp"
host = "invalid"
username = "me"
auth = "agent"
keepalive_secs = 9999

[[profiles]]
id = "00000000-0000-4000-8000-000000000007"
name = "Bad port"
protocol = "sftp"
host = "h"
port = 70000
username = "me"
auth = "agent"
"#;

    fn id(last: u128) -> Uuid {
        Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | last)
    }

    fn event(id: Uuid, state: MountState, on: Option<char>) -> MountEvent {
        MountEvent {
            id,
            state,
            detail: on.and_then(DriveLetter::new).map(|letter| MountDetail {
                letter,
                instead_of: None,
            }),
        }
    }

    fn load(has: impl Fn(&str) -> bool) -> Result<ConfigStore, Box<dyn Error>> {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("config.toml");
        let schema = if has("--read-only") { 99 } else { 1 };
        let mut text = format!("schema_version = {schema}\n");
        if !has("--empty") {
            text.push_str(CONFIG);
        }
        if has("--many") {
            for number in 100..140 {
                text.push_str(&format!(
                    "[[profiles]]\nid = \"00000000-0000-4000-8000-000000000{number}\"\n\
                     name = \"Server {number}\"\nprotocol = \"sftp\"\nhost = \"server{number}\"\n\
                     username = \"me\"\nauth = \"agent\"\n"
                ));
            }
        }
        std::fs::write(&path, text)?;
        Ok(ConfigStore::open(path))
    }

    /// Pretends to be the manager: mounting takes a moment, and one profile always fails.
    fn handle(ui: &Rc<MainUi>, config: &Rc<std::cell::RefCell<Config>>, command: UiCommand) {
        eprintln!("{command:?}");
        match command {
            UiCommand::Mount(mounting) => {
                ui.apply_event(&event(mounting, MountState::Connecting, None));
                let ui = Rc::clone(ui);
                Timer::single_shot(Duration::from_millis(1500), move || {
                    if mounting == id(2) {
                        let error = MountError::Connect(ConnectError::Auth {
                            user: "deploy".to_owned(),
                            host: "stage".to_owned(),
                        });
                        ui.apply_event(&event(mounting, MountState::Error(error), None));
                    } else {
                        ui.apply_event(&event(mounting, MountState::Mounted, Some('V')));
                    }
                });
            }
            UiCommand::Cancel(cancelled) => {
                ui.apply_event(&event(cancelled, MountState::Disconnected, None));
            }
            UiCommand::Unmount(unmounting) => {
                ui.apply_event(&event(unmounting, MountState::Unmounting, Some('V')));
                let ui = Rc::clone(ui);
                Timer::single_shot(Duration::from_millis(500), move || {
                    ui.apply_event(&event(unmounting, MountState::Disconnected, None));
                });
            }
            UiCommand::Delete(deleted) => {
                let mut config = config.borrow_mut();
                config.profiles.retain(|profile| profile.id != deleted);
                ui.set_config(config.clone());
            }
            UiCommand::DeleteUnreadable(entry) => {
                let mut config = config.borrow_mut();
                config.unreadable.retain(|other| *other != entry);
                ui.set_config(config.clone());
            }
            UiCommand::HideWindow | UiCommand::CloseWindow => {
                // There is no tray to come back from.
                let _ = slint::quit_event_loop();
            }
            UiCommand::Save { profile, .. } => {
                let mut config = config.borrow_mut();
                config.profiles.retain(|saved| saved.id != profile.id);
                config.profiles.push(*profile);
                ui.set_config(config.clone());
                ui.save_finished(Ok(()));
            }
            UiCommand::Test { attempt, .. } => {
                let ui = Rc::clone(ui);
                Timer::single_shot(Duration::from_millis(800), move || {
                    ui.test_finished(attempt, Ok(Duration::from_millis(12)));
                });
            }
            UiCommand::FindFreeLetters => {
                let free = ('D'..='Z')
                    .filter_map(DriveLetter::new)
                    .filter(|letter| is_letter_free(*letter).unwrap_or(false))
                    .collect();
                ui.set_free_letters(free);
            }
            // It blocks the window, which does not matter for a look at it.
            UiCommand::PickKeyFile { which, title } => {
                ui.key_file_picked(which, pick_file(&title, None).ok().flatten());
            }
            UiCommand::CancelTest { .. }
            | UiCommand::OpenFileLocation(_)
            | UiCommand::OpenInExplorer(_)
            | UiCommand::OpenSettings
            | UiCommand::DownloadWinFsp => {}
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let arguments: Vec<String> = env::args().skip(1).collect();
        let has = |flag: &str| arguments.iter().any(|argument| argument == flag);

        let store = load(has)?;
        let config = Rc::new(std::cell::RefCell::new(store.config().clone()));
        let cell: Rc<OnceCell<Rc<MainUi>>> = Rc::new(OnceCell::new());

        let for_commands = Rc::clone(&cell);
        let config_for_commands = Rc::clone(&config);
        let ui = Rc::new(MainUi::with_remembered_path(
            move |command| {
                if let Some(ui) = for_commands.get() {
                    handle(ui, &config_for_commands, command);
                }
            },
            Some(env::temp_dir().join("fsm-main-window-preview.toml")),
        )?);
        let _ = cell.set(Rc::clone(&ui));

        ui.set_config(store.config().clone());
        ui.set_read_only(store.read_only());
        ui.set_setting_problems(store.setting_problems());
        ui.set_winfsp_ready(!has("--no-winfsp"));
        if has("--light") {
            ui.set_theme(Theme::Light);
        } else if has("--dark") {
            ui.set_theme(Theme::Dark);
        }
        if !has("--empty") && !has("--read-only") {
            // As in the mock-up of spec 7.1.
            ui.apply_event(&event(id(1), MountState::Mounted, Some('Z')));
            ui.apply_event(&event(id(2), MountState::Connecting, None));
            let error = MountError::Connect(ConnectError::Auth {
                user: "me".to_owned(),
                host: "devbox".to_owned(),
            });
            ui.apply_event(&event(id(4), MountState::Error(error), None));
        }

        ui.show()?;
        slint::run_event_loop_until_quit()?;
        Ok(())
    }
}
