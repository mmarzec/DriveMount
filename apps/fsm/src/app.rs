// SPDX-License-Identifier: GPL-3.0-or-later

//! The program: the configuration, the log, the runtime, the window and the engine, put together and
//! taken apart in the right order. `examples/connect.rs` runs the same thing on scratch files.

mod engine;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use fsm_core::config::{ConfigStore, LogLevel};
use fsm_core::manager::{ManagerParts, MountManager};
use fsm_vfs::SecretStore;
use fsm_win::credentials::CredentialStore;
use slint::ComponentHandle;
use tokio::runtime::{self, Runtime};
use tokio::sync::mpsc;

use self::engine::UiAction;
use crate::backend::{SftpWinFspFactory, SystemDriveLetters, default_known_hosts};
use crate::logging::{self, LogGuard};
use crate::ui::MainUi;

/// Spec §5.3's limits on the runtime's workers.
const FEWEST_WORKERS: usize = 2;
const MOST_WORKERS: usize = 4;
/// How long quitting waits for drives to go. A process that has ended leaves none (NFR-REL-04).
const UNMOUNTING_AT_QUIT: Duration = Duration::from_secs(10);
/// How long quitting waits for what is left on the runtime, such as a connection being tested.
const WINDING_DOWN: Duration = Duration::from_secs(1);

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
        })
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
    let result = run_logged(setup, store);
    if let Err(error) = &result {
        tracing::error!("{error:#}");
    }
    drop(log);
    result
}

fn run_logged(setup: Setup, store: ConfigStore) -> Result<()> {
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

    let settings = &store.config().settings;
    ui.set_theme(settings.theme);
    ui.set_config(store.config().clone());
    ui.set_read_only(store.read_only());
    ui.set_setting_problems(store.setting_problems());
    let winfsp = fsm_win::winfsp::detect();
    tracing::info!(?winfsp, "looked for WinFsp");
    ui.set_winfsp_ready(winfsp.is_ready());

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
    engine::start(
        runtime.handle(),
        manager.clone(),
        inbox,
        Arc::new(post_to_window),
    );

    ui.show().context("showing the window")?;
    slint::run_event_loop_until_quit().context("running the window")?;

    tracing::info!("quitting");
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
