// SPDX-License-Identifier: GPL-3.0-or-later

//! The program: the configuration, the log, the runtime, the window and the engine, put together and
//! taken apart in the right order. `examples/connect.rs` runs the same thing on scratch files.

mod engine;
mod system;
mod ui_clock;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, io, thread};

use anyhow::{Context, Result};
use drivemount_core::config::{ConfigStore, LogLevel};
use drivemount_core::manager::{ManagerParts, MountManager};
use drivemount_vfs::SecretStore;
use drivemount_win::credentials::CredentialStore;
use drivemount_win::instance::{self, Claim, Instance, ShowRequests};
use drivemount_win::process;
use drivemount_win::startup;
use drivemount_win::toast::{self, AppId, Toasts};
use drivemount_win::tray::{Tray, TrayContent};
use slint::{ComponentHandle, Timer, TimerMode};
use tokio::runtime::{self, Runtime};
use tokio::sync::mpsc;
use uuid::Uuid;

use self::engine::{StartWithWindows, Surroundings, UiAction};
use crate::backend::{SftpWinFspFactory, SystemDriveLetters, default_known_hosts};
use crate::crash;
use crate::logging::{self, LogGuard, LogLevelControl};
use crate::ui::{MainUi, TrayIcon};

/// Spec §5.3's limits on the runtime's workers.
const FEWEST_WORKERS: usize = 2;
const MOST_WORKERS: usize = 4;
/// How long quitting waits for drives to go. A process that has ended leaves none (NFR-REL-04).
const UNMOUNTING_AT_QUIT: Duration = Duration::from_secs(10);
/// How long quitting waits for what is left on the runtime, such as a connection being tested.
const WINDING_DOWN: Duration = Duration::from_secs(1);
/// How long a second start looks for the first one's window, which may be starting too.
const HANDING_OVER: Duration = Duration::from_secs(5);
/// How long a start in the tray waits for the icon to get in before it shows the window. The tray
/// tries for a minute, as Explorer may still be starting at sign-in.
const WAITING_FOR_THE_TRAY: Duration = Duration::from_secs(70);
const LOOKING_AT_THE_TRAY: Duration = Duration::from_millis(500);
/// In a debug build, a single-instance name of the caller's own, so that tests of the real exe can
/// run beside each other and beside the user's own app.
const TEST_INSTANCE_VARIABLE: &str = "DRIVEMOUNT_TEST_INSTANCE";

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
    /// The single-instance name (FR-APP-01), or none, to allow any number.
    pub instance: Option<String>,
    /// Start with the window hidden, once the icon is in the tray: `--tray` (FR-APP-03).
    pub start_in_tray: bool,
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
            instance: Some(instance_name()),
            start_in_tray: false,
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

fn instance_name() -> String {
    if cfg!(debug_assertions)
        && let Some(name) = env::var_os(TEST_INSTANCE_VARIABLE).and_then(|n| n.into_string().ok())
    {
        return name;
    }
    instance::APP_INSTANCE.to_owned()
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
            ui_clock::time("action", || action(&ui));
        }
    });
    // Only as the app ends.
    if let Err(error) = posted {
        tracing::debug!(%error, "the window has gone");
    }
}

/// How long ago the process was created, in ms, for the start-up budgets (NFR-PERF-01, 02).
fn ms_since_start() -> Option<f64> {
    process::since_start().map(|age| age.as_secs_f64() * 1000.0)
}

/// Puts the icon in the tray (FR-TRAY-01). Without it the window cannot be hidden, so it stays up.
/// Returns whether there is a tray, which may still be trying to get the icon in.
fn start_tray(ui: &MainUi) -> bool {
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
                // NFR-PERF-02 for a start with `--tray`.
                tracing::info!(since_start_ms = ms_since_start(), "the icon is in the tray");
            } else {
                tracing::warn!(
                    "the tray did not take the icon, so trying again; the window stays up meanwhile"
                );
            }
            ui.set_tray(Some(Box::new(tray)));
            true
        }
        Err(error) => {
            tracing::warn!(%error, "there is no tray icon");
            false
        }
    }
}

/// Hears other starts of the app asking for the window (FR-APP-01).
fn listen_for_other_starts(instance: &Instance) -> Option<ShowRequests> {
    let on_show = Box::new(|| {
        post_to_window(Box::new(|ui| {
            tracing::info!("the app was started again, so showing the window");
            ui.bring_to_front();
        }));
    });
    instance
        .listen(on_show)
        .inspect_err(|error| {
            tracing::warn!(%error, "another start of the app cannot bring up this window");
        })
        .ok()
}

/// Shows the window, unless started in the tray (FR-APP-03). Then it stays hidden once the icon is
/// in, and is shown if the icon does not get in in time, as there would be no way to it. The timer
/// that waits is returned, to keep it going.
fn show_at_start(ui: &Rc<MainUi>, in_tray: bool, tray_trying: bool) -> Result<Option<Rc<Timer>>> {
    if !in_tray || !tray_trying {
        if in_tray {
            tracing::warn!("started for the tray, but there is none, so showing the window");
        }
        ui.show().context("showing the window")?;
        return Ok(None);
    }
    if ui.has_tray() {
        tracing::info!("started in the tray");
        return Ok(None);
    }
    tracing::info!("started for the tray, so waiting for the icon to get in");
    let timer = Rc::new(Timer::default());
    let this = Rc::downgrade(&timer);
    let ui: Weak<MainUi> = Rc::downgrade(ui);
    let started = Instant::now();
    timer.start(TimerMode::Repeated, LOOKING_AT_THE_TRAY, move || {
        let Some(ui) = ui.upgrade() else {
            return;
        };
        if ui.has_tray() {
            tracing::info!("the icon is in the tray, so the window stays hidden");
        } else if started.elapsed() >= WAITING_FOR_THE_TRAY {
            tracing::warn!("the icon did not get into the tray, so showing the window");
            if !ui.is_window_up() {
                ui.bring_to_front();
            }
        } else {
            return;
        }
        // Done: nothing more to wake up for (NFR-PERF-05).
        if let Some(timer) = this.upgrade() {
            timer.stop();
        }
    });
    Ok(Some(timer))
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
    // Before anything, so that a second start touches neither the files nor the log.
    let claimed = setup.instance.as_deref().map(instance::claim);
    let (instance, not_claimed) = match claimed {
        None => (None, None),
        Some(Ok(Claim::First(instance))) => (Some(instance), None),
        Some(Ok(Claim::Running)) => {
            return hand_over(setup.instance.as_deref().unwrap_or_default());
        }
        // One app is better than none.
        Some(Err(error)) => (None, Some(error)),
    };
    if let Some(logs) = &setup.logs {
        crash::install(logs.clone());
    }
    // First, as it says what to log. It logs nothing itself: what it found is reported below.
    let store = ConfigStore::open(&setup.config);
    let log = start_logging(&setup, store.config().settings.log_level);
    let log_level = log.as_ref().map(LogGuard::control).unwrap_or_default();
    if let Some(error) = not_claimed {
        tracing::warn!(%error, "could not tell whether the app is running already");
    }
    let result = run_logged(setup, store, log_level, instance.as_ref());
    if let Err(error) = &result {
        tracing::error!("{error:#}");
    }
    drop(log);
    result
}

/// The app is running already: its window comes up instead (FR-APP-01), and this start ends. Nothing
/// is logged, as the log is the other one's.
fn hand_over(name: &str) -> Result<()> {
    instance::ask_to_show(name, HANDING_OVER).context("asking the running app to show its window")
}

fn run_logged(
    setup: Setup,
    store: ConfigStore,
    log_level: LogLevelControl,
    instance: Option<&Instance>,
) -> Result<()> {
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
    // NFR-PERF-01 for a start that shows the window; one in the tray draws it when it is asked for.
    let in_tray = setup.start_in_tray;
    ui_clock::watch(
        ui.window().window(),
        Box::new(move || {
            tracing::info!(
                since_start_ms = ms_since_start(),
                in_tray,
                "the first frame is drawn"
            );
        }),
    );

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
    if let Some(report) = setup.logs.as_deref().and_then(crash::take_unnoticed) {
        tracing::warn!(%report, "the last run crashed");
        ui.set_crash_report(Some(&report));
    }

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
    let tray_trying = start_tray(&ui);
    if let Some(app) = &setup.toasts {
        start_toasts(&ui, app);
    }
    let other_starts = instance.and_then(listen_for_other_starts);

    let waiting_for_the_tray = show_at_start(&ui, setup.start_in_tray, tray_trying)?;
    slint::run_event_loop_until_quit().context("running the window")?;

    tracing::info!("quitting");
    let (longest, over) = ui_clock::longest();
    if let Some((took, what)) = longest {
        tracing::info!(
            longest_ms = took.as_secs_f64() * 1000.0,
            what,
            over_a_frame = over,
            "the UI thread's longest busy stretch (NFR-PERF-10)"
        );
    }
    drop(waiting_for_the_tray);
    // A start from now on finds this one still running, and gives up after a while.
    drop(other_starts);
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
