// SPDX-License-Identifier: GPL-3.0-or-later

//! The UI's side of the engine (spec §5.3). What the user asks for arrives over an `mpsc` channel, and
//! what the engine says goes back as actions to run on the UI thread. Nothing here runs on that
//! thread until it is an action, so nothing here holds up the window (NFR-PERF-10).

use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use drivemount_core::config::Settings;
use drivemount_core::manager::{MountEvent, MountManager, MountRefused};
use drivemount_vfs::DriveLetter;
use drivemount_win::drives::is_letter_free;
use drivemount_win::shell;
use tokio::runtime::Handle;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::logging::LogLevelControl;
use crate::ui::{ActionFailure, MainUi, TypedSecrets, UiCommand, WINFSP_DOWNLOAD_URL};

/// Something for the UI thread to do.
pub(super) type UiAction = Box<dyn FnOnce(&MainUi) + Send>;
/// How an action gets there.
pub(super) type ToUi = Arc<dyn Fn(UiAction) + Send + Sync>;

/// The tests in progress, by attempt.
type RunningTests = Mutex<HashMap<u64, AbortHandle>>;

fn running(tests: &RunningTests) -> MutexGuard<'_, HashMap<u64, AbortHandle>> {
    tests.lock().unwrap_or_else(PoisonError::into_inner)
}

/// *Start with Windows* (FR-APP-03): the Run value in the app, something that records it in tests.
pub(super) trait StartWithWindows: Send + Sync {
    /// Writes the value, or removes it. Blocks.
    fn set(&self, on: bool) -> io::Result<()>;
}

/// What the settings page reaches beyond the file (FR-SET).
pub(super) struct Surroundings {
    /// The log, whose level changes with the setting.
    pub log_level: LogLevelControl,
    /// The Run value.
    pub start_with_windows: Arc<dyn StartWithWindows>,
    /// The logs folder, if there is a log.
    pub logs: Option<PathBuf>,
    /// `THIRD-PARTY-LICENSES.html`, beside the exe.
    pub licences: Option<PathBuf>,
}

/// Starts following commands from `inbox` and events from `manager`. Both go on until the runtime
/// ends.
pub(super) fn start(
    runtime: &Handle,
    manager: MountManager,
    mut inbox: mpsc::UnboundedReceiver<UiCommand>,
    to_ui: ToUi,
    surroundings: Surroundings,
) {
    // Before any command, to miss nothing.
    let events = manager.subscribe();
    runtime.spawn(follow_events(events, manager.clone(), Arc::clone(&to_ui)));
    let (settings, wanted) = mpsc::unbounded_channel();
    runtime.spawn(keep_settings(
        wanted,
        manager.clone(),
        Arc::clone(&to_ui),
        surroundings.log_level,
        surroundings.start_with_windows,
    ));
    let engine = Engine {
        manager,
        to_ui,
        tests: Arc::default(),
        settings,
        logs: surroundings.logs,
        licences: surroundings.licences,
    };
    runtime.spawn(async move {
        while let Some(command) = inbox.recv().await {
            engine.handle(command);
        }
    });
}

async fn follow_events(
    mut events: broadcast::Receiver<MountEvent>,
    manager: MountManager,
    to_ui: ToUi,
) {
    loop {
        match events.recv().await {
            Ok(event) => to_ui(Box::new(move |ui| ui.apply_event(&event))),
            Err(RecvError::Lagged(missed)) => {
                warn!(
                    missed,
                    "fell behind the mount events, so reading the states"
                );
                let config = manager.config().await;
                let profiles = config.profiles.iter();
                let latest: Vec<MountEvent> =
                    profiles.filter_map(|p| manager.status(p.id)).collect();
                to_ui(Box::new(move |ui| {
                    for event in &latest {
                        ui.apply_event(event);
                    }
                }));
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Saves the settings the page asks for, one change after another, so that a later one is never
/// overtaken by an earlier one. Changes that came while one was saving count only as the latest.
///
/// *Start with Windows* goes to the registry before the file: if the registry says no, the setting
/// stays as it was. If the file then says no, the registry is put back.
async fn keep_settings(
    mut wanted: mpsc::UnboundedReceiver<Settings>,
    manager: MountManager,
    to_ui: ToUi,
    log_level: LogLevelControl,
    start_with_windows: Arc<dyn StartWithWindows>,
) {
    let set_run_value = |on: bool| {
        let start_with_windows = Arc::clone(&start_with_windows);
        async move {
            tokio::task::spawn_blocking(move || start_with_windows.set(on))
                .await
                .unwrap_or_else(|error| Err(io::Error::other(error.to_string())))
        }
    };
    let mut applied = manager.config().await.settings;
    while let Some(mut next) = wanted.recv().await {
        while let Ok(newer) = wanted.try_recv() {
            next = newer;
        }
        let mut failure = None;
        if next.start_with_windows != applied.start_with_windows
            && let Err(error) = set_run_value(next.start_with_windows).await
        {
            warn!(%error, on = next.start_with_windows, "Start with Windows could not be changed");
            failure = Some(ActionFailure::StartWithWindows(error.to_string()));
            next.start_with_windows = applied.start_with_windows;
        }
        let run_value_changed = next.start_with_windows != applied.start_with_windows;
        if next != applied {
            match manager.save_settings(next.clone()).await {
                Ok(()) => {
                    info!(settings = ?next, "the settings were saved");
                    if next.log_level != applied.log_level {
                        log_level.set(next.log_level);
                        info!(level = ?next.log_level, "the log level changed");
                    }
                    applied = next;
                }
                Err(error) => {
                    warn!(%error, "the settings could not be saved");
                    if run_value_changed
                        && let Err(error) = set_run_value(applied.start_with_windows).await
                    {
                        warn!(%error, "Start with Windows could not be put back");
                    }
                    failure = Some(ActionFailure::Settings(error.to_string()));
                }
            }
        }
        let went_wrong = failure.is_some();
        show_failure(&to_ui, failure);
        // The page shows what is in use again.
        if went_wrong {
            show_config(&manager, &to_ui).await;
        }
    }
}

/// Reads the file again, as `set_config` wants after each change.
async fn show_config(manager: &MountManager, to_ui: &ToUi) {
    let config = manager.config().await;
    to_ui(Box::new(move |ui| ui.set_config(config)));
}

fn show_failure(to_ui: &ToUi, failure: Option<ActionFailure>) {
    to_ui(Box::new(move |ui| {
        ui.set_action_failure(failure.as_ref());
    }));
}

/// For the file picker and Explorer: modal or slow, with no use for the runtime.
fn on_a_thread_of_its_own(what: impl FnOnce() + Send + 'static) {
    thread::spawn(what);
}

struct Engine {
    manager: MountManager,
    to_ui: ToUi,
    tests: Arc<RunningTests>,
    /// To [`keep_settings`].
    settings: mpsc::UnboundedSender<Settings>,
    logs: Option<PathBuf>,
    licences: Option<PathBuf>,
}

impl Engine {
    /// Each command goes off to do its work, so a slow one holds up no other. Called on the runtime.
    fn handle(&self, command: UiCommand) {
        let manager = self.manager.clone();
        let to_ui = Arc::clone(&self.to_ui);
        match command {
            UiCommand::Save { profile, secrets } => {
                let TypedSecrets(secrets) = secrets;
                tokio::spawn(async move {
                    let id = profile.id;
                    let saved = manager.save(*profile, secrets).await;
                    if let Err(error) = &saved {
                        warn!(%id, %error, "a save did not finish");
                    }
                    // The dialog closes on the second, and the list has to be right by then.
                    show_config(&manager, &to_ui).await;
                    to_ui(Box::new(move |ui| ui.save_finished(saved)));
                });
            }
            UiCommand::Test {
                attempt,
                profile,
                typed,
            } => {
                let TypedSecrets(typed) = typed;
                let tests = Arc::clone(&self.tests);
                // Held until the handle is in, so that a test finishing at once finds it to take out.
                let mut held = running(&self.tests);
                let task = tokio::spawn(async move {
                    let result = manager.test(&profile, typed).await;
                    running(&tests).remove(&attempt);
                    match &result {
                        Ok(round_trip) => info!(?round_trip, "a test connected"),
                        Err(error) => info!(%error, "a test failed"),
                    }
                    to_ui(Box::new(move |ui| ui.test_finished(attempt, result)));
                });
                held.insert(attempt, task.abort_handle());
            }
            UiCommand::CancelTest { attempt } => {
                if let Some(task) = running(&self.tests).remove(&attempt) {
                    task.abort();
                }
            }
            UiCommand::FindFreeLetters => {
                tokio::task::spawn_blocking(move || {
                    let free: Vec<DriveLetter> = ('D'..='Z')
                        .filter_map(DriveLetter::new)
                        .filter(|letter| is_letter_free(*letter).unwrap_or(false))
                        .collect();
                    to_ui(Box::new(move |ui| ui.set_free_letters(free)));
                });
            }
            UiCommand::PickKeyFile { which, title } => {
                on_a_thread_of_its_own(move || {
                    let ssh =
                        env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".ssh"));
                    let path = shell::pick_file(&title, ssh.as_deref()).unwrap_or_else(|error| {
                        warn!(%error, "the file dialog failed");
                        None
                    });
                    to_ui(Box::new(move |ui| ui.key_file_picked(which, path)));
                });
            }
            UiCommand::OpenFileLocation(file) => {
                on_a_thread_of_its_own(move || {
                    if let Err(error) = shell::show_in_explorer(&file) {
                        warn!(%error, "could not show a file in Explorer");
                    }
                });
            }
            UiCommand::Delete(id) => {
                tokio::spawn(async move {
                    let deleted = manager.delete(id).await;
                    if let Err(error) = &deleted {
                        warn!(%id, %error, "a delete did not finish");
                    }
                    let failure = deleted.err().map(|e| ActionFailure::Delete(e.to_string()));
                    show_failure(&to_ui, failure);
                    show_config(&manager, &to_ui).await;
                });
            }
            UiCommand::DeleteUnreadable(entry) => {
                tokio::spawn(async move {
                    let deleted = manager.delete_unreadable(entry).await;
                    if let Err(error) = &deleted {
                        warn!(%error, "deleting an unreadable entry did not finish");
                    }
                    let failure = deleted.err().map(|e| ActionFailure::Delete(e.to_string()));
                    show_failure(&to_ui, failure);
                    show_config(&manager, &to_ui).await;
                });
            }
            UiCommand::Mount(id) => {
                tokio::spawn(async move {
                    match manager.mount(id).await {
                        Ok(()) => show_failure(&to_ui, None),
                        // A second press before the row caught up. It is doing what was asked.
                        Err(MountRefused::Busy) => debug!(%id, "already in use"),
                        Err(error) => {
                            warn!(%id, %error, "a mount did not start");
                            let failure = ActionFailure::Mount(error.to_string());
                            show_failure(&to_ui, Some(failure));
                        }
                    }
                });
            }
            UiCommand::Unmount(id) => {
                tokio::spawn(async move { manager.unmount(id).await });
            }
            UiCommand::Cancel(id) => manager.cancel(id),
            UiCommand::OpenInExplorer(letter) => {
                on_a_thread_of_its_own(move || {
                    if let Err(error) = shell::open_in_explorer(letter) {
                        warn!(%letter, %error, "could not open a drive in Explorer");
                    }
                });
            }
            UiCommand::DownloadWinFsp => {
                on_a_thread_of_its_own(|| {
                    if let Err(error) = shell::open_web_page(WINFSP_DOWNLOAD_URL) {
                        warn!(%error, "could not open the WinFsp page");
                    }
                });
            }
            UiCommand::SaveSettings(settings) => {
                // Only as the runtime ends.
                let _ = self.settings.send(*settings);
            }
            UiCommand::OpenLogsFolder => {
                let Some(logs) = self.logs.clone() else {
                    return;
                };
                on_a_thread_of_its_own(move || {
                    if let Err(error) = shell::open_folder(&logs) {
                        warn!(%error, "could not open the logs folder");
                    }
                });
            }
            UiCommand::OpenLicences => {
                let licences = self.licences.clone();
                on_a_thread_of_its_own(move || {
                    let opened = licences.map_or_else(
                        || Err(io::Error::other("the folder of the exe is not known")),
                        |file| shell::open_html_file(&file),
                    );
                    if let Err(error) = opened {
                        warn!(%error, "could not open the licence file");
                        show_failure(&to_ui, Some(ActionFailure::Licences(error.to_string())));
                    }
                });
            }
            UiCommand::OpenWebPage(url) => {
                if url.is_empty() {
                    return;
                }
                on_a_thread_of_its_own(move || {
                    if let Err(error) = shell::open_web_page(url) {
                        warn!(%url, %error, "could not open a web page");
                    }
                });
            }
            // The window has asked, if drives were up (FR-APP-04). `app::run` unmounts them.
            UiCommand::Quit => {
                to_ui(Box::new(|_| {
                    if let Err(error) = slint::quit_event_loop() {
                        warn!(%error, "the window's loop could not be ended");
                    }
                }));
            }
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
