// SPDX-License-Identifier: GPL-3.0-or-later

//! The program: the configuration, the log, the runtime, the window and the engine, put together and
//! taken apart in the right order. `examples/connect.rs` runs the same thing on scratch files.

mod engine;
mod system;

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use drivemount_core::config::{ConfigStore, LogLevel};
use drivemount_core::manager::{ManagerParts, MountManager};
use drivemount_vfs::SecretStore;
use drivemount_win::credentials::CredentialStore;
use drivemount_win::startup;
use drivemount_win::toast::{self, AppId, Toasts};
use drivemount_win::tray::{Tray, TrayContent};
use slint::ComponentHandle;
use tokio::runtime::{self, Runtime};
use tokio::sync::mpsc;
use uuid::Uuid;

use self::engine::{StartWithWindows, Surroundings, UiAction};
use crate::backend::{SftpWinFspFactory, SystemDriveLetters, default_known_hosts};
use crate::logging::{self, LogGuard, LogLevelControl};
use crate::ui::{MainUi, TrayIcon};

/// Spec §5.3's limits on the runtime's workers.
const FEWEST_WORKERS: usize = 2;
const MOST_WORKERS: usize = 4;
/// How long quitting waits for drives to go. A process that has ended leaves none (NFR-REL-04).
const UNMOUNTING_AT_QUIT: Duration = Duration::from_secs(10);
/// How long quitting waits for what is left on the runtime, such as a connection being tested.
const WINDING_DOWN: Duration = Duration::from_secs(1);

/// The AppUserModelID toasts come from. An installer's Start-menu shortcut should carry the same one
/// (M2-10), and an uninstaller should remove the keys of [`toast::register`].
pub const AUMID: &str = "DriveMount";
/// The COM class a click on a toast calls (spec §5.3). Fixed, as Action Center keeps toasts across
/// restarts of the app.
const TOAST_ACTIVATOR: Uuid = Uuid::from_u128(0xf458_b6d8_450a_4371_8114_627b_7e93_ebbc);

/// Where the program keeps things.
pub struct Setup {
    /// `config.toml`.
    pub config: PathBuf,
    /// `known_hosts`.
    pub known_hosts: PathBuf,
    /// What the window remembers, or nowhere.
    pub window_state: Option<PathBuf>,
    /// The log's folder, or no log.
    pub logs: Option<PathBuf>,
    /// Where remembered secrets go.
    pub secrets: Arc<dyn SecretStore>,
    /// Who toasts are from, or no toasts.
    pub toasts: Option<AppId>,
    /// The name of the value under the Run key for *Start with Windows* (FR-APP-03), or none, which
    /// makes the setting change only the file.
    pub run_value: Option<String>,
}

impl Setup {
    /// The places of spec §6, for the user running this.
    ///
    /// # Errors
    /// `APPDATA` or `USERPROFILE` is not set, so there is nowhere to keep them.
    pub fn for_this_user() -> Result<Self> {
        let config = ConfigStore::default_path().context("APPDATA is not set")?;
        Ok(Self {
            window_state: config.parent().map(|folder| folder.join("ui-state.toml")),
            config,
            known_hosts: default_known_hosts().context("USERPROFILE is not set")?,
            logs: logging::default_folder(),
            secrets: Arc::new(CredentialStore),
            toasts: Some(Self::toasts_from()),
            run_value: Some(startup::RUN_VALUE.to_owned()),
        })
    }

    /// The app's own toast identity (FR-NOT).
    #[must_use]
    pub fn toasts_from() -> AppId {
        AppId {
            aumid: AUMID.to_owned(),
            display_name: "DriveMount".to_owned(),
            activator: TOAST_ACTIVATOR,
        }
    }
}

thread_local! {
    /// The window, for what comes from other threads. It cannot leave the UI thread.
    static UI: RefCell<Option<Rc<MainUi>>> = const { RefCell::new(None) };
}

/// Runs an action on the UI thread, from any other.
fn post_to_window(action: UiAction) {
    let posted = slint::invoke_from_event_loop(move || {
        let ui = UI.with(|ui| ui.borrow().clone());
        if let Some(ui) = ui {
            action(&ui);
        }
    });
    // Only as the app ends.
    if let Err(error) = posted {
        tracing::debug!(%error, "the window has gone");
    }
}

/// Puts the icon in the tray (FR-TRAY-01). Without it the window cannot be hidden, so it stays up.
fn start_tray(ui: &MainUi) {
    // Filled in from the list at once.
    let empty = TrayContent {
        icon: TrayIcon::Idle.file(),
        tooltip: String::new(),
        menu: Vec::new(),
    };
    let on_event = Box::new(|event| post_to_window(Box::new(move |ui| ui.tray_event(event))));
    match Tray::start(empty, on_event) {
        Ok(tray) => {
            if tray.is_in_tray() {
                tracing::info!("the icon is in the tray");
            } else {
                tracing::warn!(
                    "the tray did not take the icon, so trying again; the window stays up meanwhile"
                );
            }
            ui.set_tray(Some(Box::new(tray)));
        }
        Err(error) => tracing::warn!(%error, "there is no tray icon"),
    }
}

/// Makes toasts show (spec §9), and clicks on them reach the window, from Action Center too
/// (FR-NOT-04). Without them, errors are still in the rows and the tray.
fn start_toasts(ui: &MainUi, app: &AppId) {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(error) = toast::register(app, &exe) {
                tracing::warn!(%error, "toasts could not be registered, so may not show");
            }
        }
        Err(error) => tracing::warn!(%error, "the exe's path is not known, so toasts may not show"),
    }
    let on_click = Arc::new(|launch: String| {
        tracing::info!(%launch, "a toast was clicked");
        post_to_window(Box::new(move |ui| ui.toast_clicked(&launch)));
    });
    match Toasts::start(app, on_click) {
        Ok(toasts) => ui.set_toasts(Some(Box::new(toasts))),
        Err(error) => tracing::warn!(%error, "there are no toasts"),
    }
}

/// The Run value under a name, for this exe.
struct RunValue {
    name: Option<String>,
}

impl StartWithWindows for RunValue {
    fn set(&self, on: bool) -> io::Result<()> {
        let Some(name) = &self.name else {
            tracing::debug!(on, "no Run value in this build");
            return Ok(());
        };
        if on {
            startup::enable(name, &std::env::current_exe()?)
        } else {
            startup::disable(name)
        }
    }
}

/// With the setting on, the Run value is written again, as the exe may have moved since. With it
/// off, nothing is touched.
fn refresh_run_value(run_value: &RunValue, on: bool) {
    if on && let Err(error) = run_value.set(true) {
        tracing::warn!(%error, "the Run value could not be written");
    }
}

fn start_logging(setup: &Setup, level: LogLevel) -> Option<LogGuard> {
    logging::start(setup.logs.as_deref()?, level).ok()
}

/// Runs the program until the user quits. All drives are gone when this returns.
///
/// # Errors
/// There is no way to run the window or the runtime. This also goes to the log.
pub fn run(setup: Setup) -> Result<()> {
    // First, as it says what to log. It logs nothing itself: what it found is reported below.
    let store = ConfigStore::open(&setup.config);
    let log = start_logging(&setup, store.config().settings.log_level);
    let log_level = log.as_ref().map(LogGuard::control).unwrap_or_default();
    let result = run_logged(setup, store, log_level);
    if let Err(error) = &result {
        tracing::error!("{error:#}");
    }
    drop(log);
    result
}

fn run_logged(setup: Setup, store: ConfigStore, log_level: LogLevelControl) -> Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %setup.config.display(),
        profiles = store.config().profiles.len(),
        "starting"
    );
    if let Some(reason) = store.read_only() {
        tracing::warn!(%reason, "the configuration cannot be saved");
    }
    for problem in store.setting_problems() {
        tracing::warn!(%problem, "a setting was not used");
    }

    let workers = thread::available_parallelism().map_or(FEWEST_WORKERS, |cores| {
        cores.get().clamp(FEWEST_WORKERS, MOST_WORKERS)
    });
    let runtime = runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .context("starting the runtime")?;

    let (commands, inbox) = mpsc::unbounded_channel();
    let ui = MainUi::with_remembered_path(
        move |command| {
            // The engine goes only as the app ends.
            let _ = commands.send(command);
        },
        setup.window_state.clone(),
    )
    .context("making the window")?;
    let ui = Rc::new(ui);
    UI.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&ui)));

    let run_value = RunValue {
        name: setup.run_value.clone(),
    };
    refresh_run_value(&run_value, store.config().settings.start_with_windows);
    ui.set_config(store.config().clone());
    ui.set_read_only(store.read_only());
    ui.set_setting_problems(store.setting_problems());
    let winfsp = drivemount_win::winfsp::detect();
    tracing::info!(?winfsp, "looked for WinFsp");
    ui.set_winfsp(winfsp);
    ui.set_has_logs(setup.logs.is_some());

    let manager = {
        let _inside = runtime.enter();
        MountManager::new(ManagerParts {
            store,
            factory: Arc::new(SftpWinFspFactory::new(
                Arc::clone(&setup.secrets),
                setup.known_hosts,
            )),
            secrets: setup.secrets,
            letters: Arc::new(SystemDriveLetters),
            prompter: ui.prompter(),
        })
    };
    let licences = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("THIRD-PARTY-LICENSES.html")));
    engine::start(
        runtime.handle(),
        manager.clone(),
        inbox,
        Arc::new(post_to_window),
        Surroundings {
            log_level,
            start_with_windows: Arc::new(run_value),
            logs: setup.logs.clone(),
            licences,
        },
    );
    let system_events = system::start(runtime.handle(), &manager);
    start_tray(&ui);
    if let Some(app) = &setup.toasts {
        start_toasts(&ui, app);
    }

    ui.show().context("showing the window")?;
    slint::run_event_loop_until_quit().context("running the window")?;

    tracing::info!("quitting");
    ui.set_tray(None);
    ui.set_toasts(None);
    drop(system_events);
    if let Err(error) = ui.window().hide() {
        tracing::debug!(%error, "the window could not be hidden");
    }
    stop(&runtime, &manager);
    // What was still being asked is answered with a cancel.
    UI.with(|cell| *cell.borrow_mut() = None);
    drop(ui);
    runtime.shutdown_timeout(WINDING_DOWN);
    tracing::info!("stopped");
    Ok(())
}

fn stop(runtime: &Runtime, manager: &MountManager) {
    let stopped = runtime
        .block_on(async { tokio::time::timeout(UNMOUNTING_AT_QUIT, manager.shutdown()).await });
    if stopped.is_err() {
        tracing::warn!("a drive was still unmounting at the time limit");
    }
}
