// SPDX-License-Identifier: GPL-3.0-or-later

//! The real window on Slint's testing back-end, the real manager on a real file and a real runtime,
//! and a pretend drive. Actions for the UI thread wait in a queue until the test runs them, as the test
//! back-end has no event loop, which also lets a test see their order.

use std::cell::Cell;

use slint::ComponentHandle;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use drivemount_core::config::{Config, ConfigStore, Settings};
use drivemount_core::manager::{BackendFactory, DriveLetters, ManagerParts, MountState};
use drivemount_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use drivemount_vfs::{
    ConnectError, Health, MemorySecretStore, MountBackend, MountError, Prompter, Secret,
    SecretSlot, async_trait,
};
use i_slint_backend_testing::{AccessibleRole, ElementQuery};
use slint::Model;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use uuid::Uuid;

use super::*;

const PATIENCE: Duration = Duration::from_secs(10);

thread_local! {
    static BACKEND_READY: Cell<bool> = const { Cell::new(false) };
}

fn init_backend() {
    if !BACKEND_READY.replace(true) {
        i_slint_backend_testing::init_no_event_loop();
    }
}

/// Says how the pretend drives and tests behave.
#[derive(Default)]
struct Script {
    tests_hang: AtomicBool,
    tests_started: AtomicUsize,
    tests_dropped: AtomicUsize,
}

struct CountsDrop(Arc<Script>);

impl Drop for CountsDrop {
    fn drop(&mut self) {
        self.0.tests_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct PretendFactory(Arc<Script>);

#[async_trait]
impl BackendFactory for PretendFactory {
    fn create(&self, _: &Profile, _: &Settings) -> Result<Box<dyn MountBackend>, MountError> {
        Ok(Box::new(PretendDrive(watch::channel(Health::Ok).0)))
    }

    async fn test(
        &self,
        _: &Profile,
        _: &Settings,
        _: Vec<(SecretSlot, Secret<String>)>,
        _: &dyn Prompter,
    ) -> Result<Duration, ConnectError> {
        let _counts = CountsDrop(Arc::clone(&self.0));
        self.0.tests_started.fetch_add(1, Ordering::SeqCst);
        if self.0.tests_hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(Duration::from_millis(7))
    }
}

struct PretendDrive(watch::Sender<Health>);

#[async_trait]
impl MountBackend for PretendDrive {
    async fn mount(&self, _: DriveLetter, _: &dyn Prompter) -> Result<(), MountError> {
        Ok(())
    }

    async fn unmount(&self) -> Result<(), MountError> {
        Ok(())
    }

    async fn reconnect(&self, _: &dyn Prompter) -> Result<(), MountError> {
        self.0.send_replace(Health::Ok);
        Ok(())
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.0.subscribe()
    }
}

struct AllFree;

impl DriveLetters for AllFree {
    fn is_free(&self, _: DriveLetter) -> io::Result<bool> {
        Ok(true)
    }
}

struct Fixture {
    _folder: TempDir,
    ui: MainUi,
    manager: MountManager,
    commands: mpsc::UnboundedSender<UiCommand>,
    actions: std_mpsc::Receiver<UiAction>,
    script: Arc<Script>,
    runtime: Runtime,
}

fn profile(name: &str) -> Profile {
    let sftp = SftpProfile::new("example.com", "deploy", SftpAuth::Agent);
    Profile::new(name, Protocol::Sftp(sftp))
}

/// A manager over this file, with a window showing what it holds.
fn fixture_from(folder: TempDir) -> Fixture {
    init_backend();
    let runtime = Runtime::new().unwrap();
    let store = ConfigStore::open(folder.path().join("config.toml"));
    let (commands, inbox) = mpsc::unbounded_channel();
    let sender = commands.clone();
    let ui = MainUi::with_remembered_path(
        move |command| {
            let _ = sender.send(command);
        },
        None,
    )
    .unwrap();
    ui.set_config(store.config().clone());
    ui.window().show().unwrap();

    let script = Arc::new(Script::default());
    let manager = {
        let _inside = runtime.enter();
        MountManager::new(ManagerParts {
            store,
            factory: Arc::new(PretendFactory(Arc::clone(&script))),
            secrets: Arc::new(MemorySecretStore::new()),
            letters: Arc::new(AllFree),
            prompter: ui.prompter(),
        })
    };
    let (post, actions) = std_mpsc::channel::<UiAction>();
    let to_ui: ToUi = Arc::new(move |action| {
        let _ = post.send(action);
    });
    start(runtime.handle(), manager.clone(), inbox, to_ui);
    Fixture {
        _folder: folder,
        ui,
        manager,
        commands,
        actions,
        script,
        runtime,
    }
}

fn fixture(profiles: Vec<Profile>) -> Fixture {
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(folder.path().join("config.toml"));
    let config = Config {
        profiles,
        ..Config::default()
    };
    store.save(&config).unwrap();
    fixture_from(folder)
}

impl Fixture {
    fn send(&self, command: UiCommand) {
        self.commands.send(command).unwrap();
    }

    fn run_next_action(&self) {
        let action = self.actions.recv_timeout(PATIENCE).expect("nothing came");
        action(&self.ui);
    }

    /// Runs what comes until this holds.
    fn until(&self, what: &str, holds: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !holds(self) {
            assert!(start.elapsed() < PATIENCE, "never happened: {what}");
            if let Ok(action) = self.actions.recv_timeout(Duration::from_millis(50)) {
                action(&self.ui);
            }
        }
    }

    fn rows(&self) -> usize {
        self.ui.window().get_entries().row_count()
    }

    fn failure(&self) -> String {
        self.ui.window().get_failure_text().to_string()
    }

    /// As [`is_mounted`](Self::is_mounted), for a row that may have moved: rows sort by letter.
    fn is_named_one_mounted(&self, name: &str) -> bool {
        let entries = self.ui.window().get_entries();
        let found = entries.iter().find(|entry| entry.name == name);
        found.unwrap().can_open
    }

    /// A row can only be opened while its drive is up.
    fn is_mounted(&self, place: usize) -> bool {
        let entry = self.ui.window().get_entries().row_data(place).unwrap();
        entry.can_open
    }
}

#[test]
fn a_save_shows_in_the_list_before_the_dialog_closes() {
    let fixture = fixture(Vec::new());
    let window = fixture.ui.window();
    window.invoke_add_pressed();
    assert!(window.get_dialog_open());
    // The dialog asks for free letters as it opens, which is not what this is about.
    fixture.until("free letters came", |f| {
        let items = f.ui.window().get_drive_items();
        items.iter().any(|item| item.contains("free"))
    });
    let mut form = window.get_form();
    form.name = "New one".into();
    form.host = "example.com".into();
    form.user = "deploy".into();
    window.set_form(form);
    let agent = ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::RadioButton)
        .match_predicate(|element| {
            element
                .accessible_label()
                .is_some_and(|label| label == "SSH agent")
        })
        .find_first()
        .unwrap();
    agent.invoke_accessible_default_action();
    window.invoke_form_edited();
    assert!(window.get_dialog_view().can_save);
    assert_eq!(fixture.rows(), 0);

    window.invoke_save_pressed();

    fixture.run_next_action();
    assert_eq!(fixture.rows(), 1, "the list comes first");
    assert!(window.get_dialog_open());
    fixture.run_next_action();
    assert!(!window.get_dialog_open());
}

#[test]
fn a_mount_comes_back_to_the_row_as_events_and_so_does_the_unmount() {
    let saved = profile("NAS");
    let id = saved.id;
    let fixture = fixture(vec![saved]);

    fixture.send(UiCommand::Mount(id));
    fixture.until("mounted", |f| f.is_mounted(0));

    fixture.send(UiCommand::Unmount(id));
    fixture.until("unmounted", |f| !f.is_mounted(0));
    assert_eq!(
        fixture.manager.status(id).unwrap().state,
        MountState::Disconnected
    );
}

#[test]
fn a_delete_takes_the_row_away() {
    let saved = profile("NAS");
    let id = saved.id;
    let fixture = fixture(vec![saved]);
    assert_eq!(fixture.rows(), 1);

    fixture.send(UiCommand::Delete(id));

    fixture.until("the row went", |f| f.rows() == 0);
    assert_eq!(fixture.failure(), "");
}

/// M1-16's note: the row cannot say this, as there is no state for it.
#[test]
fn a_delete_that_fails_says_so_and_the_row_stays() {
    let folder = TempDir::new().unwrap();
    let path = folder.path().join("config.toml");
    let mut store = ConfigStore::open(&path);
    let saved = profile("NAS");
    let id = saved.id;
    let config = Config {
        profiles: vec![saved],
        ..Config::default()
    };
    store.save(&config).unwrap();
    // From a newer version, which this one will not write.
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace("schema_version = 1", "schema_version = 999"),
    )
    .unwrap();
    let fixture = fixture_from(folder);

    fixture.send(UiCommand::Delete(id));

    fixture.until("it says so", |f| f.failure().contains("delete"));
    assert_eq!(fixture.rows(), 1);
}

#[test]
fn a_mount_that_cannot_start_says_so_until_one_starts() {
    let saved = profile("NAS");
    let id = saved.id;
    let fixture = fixture(vec![saved]);

    fixture.send(UiCommand::Mount(Uuid::new_v4()));
    fixture.until("it says so", |f| f.failure().contains("mount"));

    fixture.send(UiCommand::Mount(id));
    fixture.until("the notice went", |f| f.failure().is_empty());
}

#[test]
fn pressing_mount_twice_is_not_a_failure() {
    let saved = profile("NAS");
    let id = saved.id;
    let fixture = fixture(vec![saved]);

    fixture.send(UiCommand::Mount(id));
    fixture.until("mounted", |f| f.is_mounted(0));
    fixture.send(UiCommand::Mount(id));

    // A refusal that counted would come as an action, so this gives it time to.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        if let Ok(action) = fixture.actions.recv_timeout(Duration::from_millis(50)) {
            action(&fixture.ui);
        }
    }
    assert_eq!(fixture.failure(), "");
    assert!(fixture.is_mounted(0));
}

#[test]
fn a_test_can_be_cancelled_and_then_nothing_comes_back() {
    let saved = profile("NAS");
    let fixture = fixture(Vec::new());
    fixture.script.tests_hang.store(true, Ordering::SeqCst);

    fixture.send(UiCommand::Test {
        attempt: 1,
        profile: Box::new(saved),
        typed: TypedSecrets::default(),
    });
    let start = Instant::now();
    while fixture.script.tests_started.load(Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < PATIENCE, "the test did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    fixture.send(UiCommand::CancelTest { attempt: 1 });
    while fixture.script.tests_dropped.load(Ordering::SeqCst) == 0 {
        assert!(start.elapsed() < PATIENCE, "the test was not stopped");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(fixture.actions.try_recv().is_err());
}

#[test]
fn a_test_that_finishes_answers_the_window() {
    let fixture = fixture(Vec::new());
    fixture.send(UiCommand::Test {
        attempt: 1,
        profile: Box::new(profile("NAS")),
        typed: TypedSecrets::default(),
    });
    fixture.run_next_action();
    assert_eq!(fixture.script.tests_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn the_free_letters_reach_the_dialog() {
    let fixture = fixture(Vec::new());
    fixture.ui.window().invoke_add_pressed();
    fixture.until("the box marks free letters", |f| {
        let items = f.ui.window().get_drive_items();
        items.iter().any(|item| item.contains("free"))
    });
}

/// If the broadcast overflows, what was missed is read instead of being lost. The channel always has
/// the newest events, so what would be lost is the state of a profile that has gone quiet.
#[test]
fn falling_behind_the_events_ends_with_the_right_state() {
    let quiet = profile("A quiet one");
    let busy = profile("B busy one");
    let (quiet_id, busy_id) = (quiet.id, busy.id);
    let fixture = fixture(vec![quiet, busy]);
    // A listener nobody reads until there is more than the channel holds.
    let behind = fixture.manager.subscribe();
    let manager = fixture.manager.clone();
    fixture.runtime.block_on(async {
        let mut events = manager.subscribe();
        let mut next = async |wanted: MountState| {
            while events.recv().await.unwrap().state != wanted {}
        };
        manager.mount(quiet_id).await.unwrap();
        next(MountState::Mounted).await;
        for _ in 0..70 {
            manager.mount(busy_id).await.unwrap();
            next(MountState::Mounted).await;
            manager.unmount(busy_id).await;
            next(MountState::Disconnected).await;
        }
    });
    // Only what the late listener says counts from here.
    while fixture.actions.try_recv().is_ok() {}
    assert!(!fixture.is_named_one_mounted("A quiet one"));

    let (post, actions) = std_mpsc::channel::<UiAction>();
    let to_ui: ToUi = Arc::new(move |action| {
        let _ = post.send(action);
    });
    fixture
        .runtime
        .spawn(follow_events(behind, fixture.manager.clone(), to_ui));
    let start = Instant::now();
    while !fixture.is_named_one_mounted("A quiet one") {
        assert!(start.elapsed() < PATIENCE, "it never caught up");
        if let Ok(action) = actions.recv_timeout(Duration::from_millis(50)) {
            action(&fixture.ui);
        }
    }
}
