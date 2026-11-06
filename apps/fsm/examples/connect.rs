// SPDX-License-Identifier: GPL-3.0-or-later

//! The window with the real engine, for trying the dialog and its prompts against a real server (H-06)
//! until M1-18 makes `fsm.exe` of it. Nothing of the user's is read or changed: its `config.toml` and
//! `known_hosts` are in a folder of their own in the temp folder, and secrets stay in memory, so a
//! *Remember* lasts until it ends. It mounts real drives. Not shipped.
//!
//! ```text
//! cargo run -p fsm --example connect -- [--fresh]
//! ```
//!
//! `--fresh` starts with no connections and no known hosts. With the S-03 server running
//! (`tests/docker/servers.ps1 up`), try `tester@localhost:2222/`.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    real::run()
}

#[cfg(windows)]
mod real {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::env;
    use std::error::Error;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::thread;

    use fsm_app::backend::{SftpWinFspFactory, SystemDriveLetters};
    use fsm_app::ui::{MainUi, TypedSecrets, UiCommand, WINFSP_DOWNLOAD_URL};
    use fsm_core::config::ConfigStore;
    use fsm_core::manager::{ManagerParts, MountManager};
    use fsm_vfs::{DriveLetter, MemorySecretStore, SecretStore};
    use fsm_win::drives::is_letter_free;
    use fsm_win::shell;
    use tokio::runtime::{Handle, Runtime};
    use tokio::task::AbortHandle;

    thread_local! {
        static UI: RefCell<Option<Rc<MainUi>>> = const { RefCell::new(None) };
    }

    /// Runs this on the UI thread, where the window is, from any other.
    fn on_ui(action: impl FnOnce(&MainUi) + Send + 'static) {
        let posted = slint::invoke_from_event_loop(move || {
            let ui = UI.with(|ui| ui.borrow().clone());
            if let Some(ui) = ui {
                action(&ui);
            }
        });
        if let Err(error) = posted {
            eprintln!("the window has gone: {error}");
        }
    }

    /// Reads the file again, as `set_config` wants after each change.
    async fn show_config(manager: &MountManager) {
        let config = manager.config().await;
        on_ui(move |ui| ui.set_config(config));
    }

    type Running = Mutex<HashMap<u64, AbortHandle>>;

    fn running(tests: &Running) -> MutexGuard<'_, HashMap<u64, AbortHandle>> {
        tests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    struct Engine {
        manager: MountManager,
        runtime: Handle,
        tests: Arc<Running>,
    }

    impl Engine {
        /// Each command goes to the runtime, and only the answer comes back to the window.
        fn handle(&self, command: UiCommand) {
            let manager = self.manager.clone();
            match command {
                UiCommand::Save { profile, secrets } => {
                    let TypedSecrets(secrets) = secrets;
                    self.runtime.spawn(async move {
                        let saved = manager.save(*profile, secrets).await;
                        show_config(&manager).await;
                        on_ui(move |ui| ui.save_finished(saved));
                    });
                }
                UiCommand::Test {
                    attempt,
                    profile,
                    typed,
                } => {
                    let TypedSecrets(typed) = typed;
                    let tests = Arc::clone(&self.tests);
                    let task = self.runtime.spawn(async move {
                        let result = manager.test(&profile, typed).await;
                        running(&tests).remove(&attempt);
                        on_ui(move |ui| ui.test_finished(attempt, result));
                    });
                    running(&self.tests).insert(attempt, task.abort_handle());
                }
                UiCommand::CancelTest { attempt } => {
                    if let Some(task) = running(&self.tests).remove(&attempt) {
                        task.abort();
                    }
                }
                UiCommand::FindFreeLetters => {
                    self.runtime.spawn_blocking(|| {
                        let free: Vec<DriveLetter> = ('D'..='Z')
                            .filter_map(DriveLetter::new)
                            .filter(|letter| is_letter_free(*letter).unwrap_or(false))
                            .collect();
                        on_ui(move |ui| ui.set_free_letters(free));
                    });
                }
                UiCommand::PickKeyFile { which, title } => {
                    thread::spawn(move || {
                        let ssh =
                            env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".ssh"));
                        let path = shell::pick_file(&title, ssh.as_deref()).unwrap_or_else(|e| {
                            eprintln!("{e}");
                            None
                        });
                        on_ui(move |ui| ui.key_file_picked(which, path));
                    });
                }
                UiCommand::OpenFileLocation(file) => {
                    thread::spawn(move || {
                        if let Err(error) = shell::show_in_explorer(&file) {
                            eprintln!("{error}");
                        }
                    });
                }
                UiCommand::Delete(id) => {
                    self.runtime.spawn(async move {
                        if let Err(error) = manager.delete(id).await {
                            eprintln!("{error}");
                        }
                        show_config(&manager).await;
                    });
                }
                UiCommand::DeleteUnreadable(entry) => {
                    self.runtime.spawn(async move {
                        if let Err(error) = manager.delete_unreadable(entry).await {
                            eprintln!("{error}");
                        }
                        show_config(&manager).await;
                    });
                }
                UiCommand::Mount(id) => {
                    self.runtime.spawn(async move {
                        if let Err(error) = manager.mount(id).await {
                            eprintln!("{error}");
                        }
                    });
                }
                UiCommand::Unmount(id) => {
                    self.runtime.spawn(async move { manager.unmount(id).await });
                }
                UiCommand::Cancel(id) => manager.cancel(id),
                UiCommand::OpenInExplorer(letter) => {
                    thread::spawn(move || {
                        if let Err(error) = shell::open_in_explorer(letter) {
                            eprintln!("{error}");
                        }
                    });
                }
                UiCommand::DownloadWinFsp => {
                    thread::spawn(|| {
                        if let Err(error) = shell::open_web_page(WINFSP_DOWNLOAD_URL) {
                            eprintln!("{error}");
                        }
                    });
                }
                UiCommand::OpenSettings => eprintln!("there is no settings page yet"),
                // There is no tray to come back from.
                UiCommand::HideWindow | UiCommand::CloseWindow => {
                    let _ = slint::quit_event_loop();
                }
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let folder = env::temp_dir().join("fsm-connect-preview");
        if env::args().any(|argument| argument == "--fresh") && folder.exists() {
            fs::remove_dir_all(&folder)?;
        }
        fs::create_dir_all(&folder)?;

        let runtime = Runtime::new()?;
        let engine: Rc<RefCell<Option<Engine>>> = Rc::new(RefCell::new(None));
        let for_commands = Rc::clone(&engine);
        let ui = Rc::new(MainUi::with_remembered_path(
            move |command| {
                if let Some(engine) = for_commands.borrow().as_ref() {
                    engine.handle(command);
                }
            },
            Some(folder.join("ui-state.toml")),
        )?);
        UI.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&ui)));

        let store = ConfigStore::open(folder.join("config.toml"));
        ui.set_config(store.config().clone());
        ui.set_read_only(store.read_only());
        ui.set_setting_problems(store.setting_problems());
        ui.set_winfsp_ready(fsm_win::winfsp::detect().is_ready());

        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let manager = {
            let _inside = runtime.enter();
            MountManager::new(ManagerParts {
                store,
                factory: Arc::new(SftpWinFspFactory::new(
                    Arc::clone(&secrets),
                    folder.join("known_hosts"),
                )),
                secrets,
                letters: Arc::new(SystemDriveLetters),
                prompter: ui.prompter(),
            })
        };

        let mut events = manager.subscribe();
        runtime.spawn(async move {
            while let Ok(event) = events.recv().await {
                on_ui(move |ui| ui.apply_event(&event));
            }
        });
        *engine.borrow_mut() = Some(Engine {
            manager: manager.clone(),
            runtime: runtime.handle().clone(),
            tests: Arc::default(),
        });

        ui.show()?;
        slint::run_event_loop_until_quit()?;
        runtime.block_on(manager.shutdown());
        UI.with(|cell| *cell.borrow_mut() = None);
        Ok(())
    }
}
