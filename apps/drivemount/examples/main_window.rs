// SPDX-License-Identifier: GPL-3.0-or-later

//! The main window with made-up connections and a pretend engine, for looking at it (H-06). Nothing is
//! mounted, and nothing the user has saved is read or changed. Not shipped. For the dialog's prompts,
//! which need a real connection, see `examples/connect.rs`.
//!
//! ```text
//! cargo run -p drivemount --example main_window -- [--empty] [--no-winfsp] [--read-only] [--light|--dark] [--many] [--showcase] [--real-drives]
//! ```
//!
//! `--showcase` swaps the edge cases for a tidy set of connections, as in the README's screenshot.
//! The list shows two made-up drives mapped outside DriveMount (FR-SMB-09), or with `--real-drives`
//! the ones this PC really has, read every 5 s as the app does. Unmounting the SMB profile pretends
//! that files are open on it, to show FR-SMB-04's question.
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

    use drivemount_app::ui::{MainUi, TakeOverRefused, UiCommand};
    use drivemount_core::config::{Config, ConfigStore, Theme};
    use drivemount_core::manager::{MountDetail, MountEvent, MountState};
    use drivemount_smb::{DriveStatus, OtherDrive};
    use drivemount_vfs::{ConnectError, DriveLetter, MountError};
    use drivemount_win::drives::is_letter_free;
    use drivemount_win::shell::pick_file;
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
id = "00000000-0000-4000-8000-000000000008"
name = "NAS media"
protocol = "smb"
server = "nas"
share = "media"
sub_path = 'films\2024'
username = "marcin"
auth = "password"
remember = true
drive_letter = "M"

[[profiles]]
id = "00000000-0000-4000-8000-000000000007"
name = "Bad port"
protocol = "sftp"
host = "h"
port = 70000
username = "me"
auth = "agent"
"#;

    /// A tidy set, for the README's screenshot.
    const SHOWCASE: &str = r#"
[[profiles]]
id = "00000000-0000-4000-8000-000000000001"
name = "Web server"
protocol = "sftp"
host = "web01.example.com"
username = "deploy"
auth = "agent"
remote_path = "/var/www"
drive_letter = "W"

[[profiles]]
id = "00000000-0000-4000-8000-000000000002"
name = "Home NAS"
protocol = "sftp"
host = "nas.local"
username = "alex"
auth = "agent"
remote_path = "/volume1/share"
drive_letter = "N"

[[profiles]]
id = "00000000-0000-4000-8000-000000000003"
name = "Build machine"
protocol = "sftp"
host = "build.example.com"
port = 2222
username = "ci"
auth = "agent"
remote_path = "/srv/artifacts"
drive_letter = "R"

[[profiles]]
id = "00000000-0000-4000-8000-000000000004"
name = "Dev box"
protocol = "sftp"
host = "devbox"
username = "alex"
auth = "agent"
remote_path = "~"

[[profiles]]
id = "00000000-0000-4000-8000-000000000005"
name = "Backups"
protocol = "sftp"
host = "backup.example.com"
username = "alex"
auth = "password"
remote_path = "/backups"
drive_letter = "K"

[[profiles]]
id = "00000000-0000-4000-8000-000000000006"
name = "Media library"
protocol = "sftp"
host = "media.local"
username = "alex"
auth = "agent"
remote_path = "/mnt/media"
drive_letter = "M"
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
        if has("--showcase") {
            text.push_str(SHOWCASE);
        } else if !has("--empty") {
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

    thread_local! {
        /// The made-up drives taken over so far, which Windows would no longer list (FR-SMB-10).
        static TAKEN_OVER: std::cell::RefCell<Vec<char>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Two drives as `net use` and WSL would have mapped them, less those taken over.
    #[expect(clippy::unnecessary_wraps, reason = "the lister's type")]
    fn made_up_drives() -> Option<Vec<OtherDrive>> {
        let drive = |letter, unc: &str, provider: &str, status, user: Option<&str>| {
            Some(OtherDrive {
                letter: DriveLetter::new(letter)?,
                unc: unc.to_owned(),
                provider: provider.to_owned(),
                status,
                user: user.map(str::to_owned),
                smb: provider == "Microsoft Windows Network",
                remembered: false,
            })
        };
        let drives = [
            drive(
                'P',
                r"\\nas\photos",
                "Microsoft Windows Network",
                DriveStatus::Mapped,
                Some(r"NAS\marcin"),
            ),
            drive(
                'U',
                r"\\wsl.localhost\Ubuntu-with-a-rather-long-name\home\marcin\projects",
                "Plan 9 Network Provider",
                DriveStatus::Disconnected,
                None,
            ),
        ];
        let taken = TAKEN_OVER.with_borrow(Clone::clone);
        Some(
            drives
                .into_iter()
                .flatten()
                .filter(|drive| !taken.contains(&drive.letter.as_char()))
                .collect(),
        )
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
            // The SMB profile pretends that files are open on it, once (FR-SMB-04).
            UiCommand::Unmount(unmounting) if unmounting == id(8) => {
                ui.apply_event(&event(unmounting, MountState::Unmounting, Some('M')));
                let ui = Rc::clone(ui);
                Timer::single_shot(Duration::from_millis(300), move || {
                    ui.apply_event(&event(unmounting, MountState::Mounted, Some('M')));
                });
            }
            UiCommand::Unmount(unmounting) | UiCommand::ForceUnmount(unmounting) => {
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
            UiCommand::Quit => {
                let _ = slint::quit_event_loop();
            }
            UiCommand::Save { profile, .. } => {
                let mut config = config.borrow_mut();
                config.profiles.retain(|saved| saved.id != profile.id);
                config.profiles.push(*profile);
                ui.set_config(config.clone());
                ui.save_finished(Ok(()));
            }
            // Taking over a made-up drive pretends files are open on it, until the user says
            // remove it anyway (FR-SMB-10).
            UiCommand::TakeOver { force: false, .. } => {
                ui.take_over_refused(&TakeOverRefused::FilesOpen);
            }
            // As the app does: the drive goes, the profile is saved, then mounted on its letter.
            UiCommand::TakeOver {
                letter, profile, ..
            } => {
                TAKEN_OVER.with_borrow_mut(|taken| taken.push(letter.as_char()));
                let taken_over = profile.id;
                {
                    let mut config = config.borrow_mut();
                    config.profiles.push(*profile);
                    ui.set_config(config.clone());
                }
                ui.save_finished(Ok(()));
                ui.apply_event(&event(taken_over, MountState::Connecting, None));
                let ui = Rc::clone(ui);
                Timer::single_shot(Duration::from_millis(800), move || {
                    ui.apply_event(&event(
                        taken_over,
                        MountState::Mounted,
                        Some(letter.as_char()),
                    ));
                });
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
            | UiCommand::OpenInExplorer(..)
            | UiCommand::CopyText(_)
            | UiCommand::SaveSettings(_)
            | UiCommand::OpenLogsFolder
            | UiCommand::OpenLicences
            | UiCommand::OpenWebPage(_)
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
            Some(env::temp_dir().join("drivemount-main-window-preview.toml")),
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
        if has("--showcase") {
            ui.apply_event(&event(id(1), MountState::Mounted, Some('W')));
            ui.apply_event(&event(id(2), MountState::Mounted, Some('N')));
            ui.apply_event(&event(id(3), MountState::Reconnecting, Some('R')));
            ui.apply_event(&event(id(6), MountState::Mounted, Some('M')));
        } else if !has("--empty") && !has("--read-only") {
            // As in the mock-up of spec 7.1.
            ui.apply_event(&event(id(1), MountState::Mounted, Some('Z')));
            ui.apply_event(&event(id(2), MountState::Connecting, None));
            let error = MountError::Connect(ConnectError::Auth {
                user: "me".to_owned(),
                host: "devbox".to_owned(),
            });
            ui.apply_event(&event(id(4), MountState::Error(error), None));
        }

        if has("--real-drives") {
            ui.set_other_drives(|| drivemount_smb::other_drives(&[]).ok());
        } else if !has("--showcase") {
            ui.set_other_drives(made_up_drives);
        }
        ui.show()?;
        slint::run_event_loop_until_quit()?;
        Ok(())
    }
}
