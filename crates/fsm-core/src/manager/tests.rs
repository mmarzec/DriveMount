// SPDX-License-Identifier: GPL-3.0-or-later

//! The state machine of spec §5.5 against a mock backend, and what Save and Delete do. Everything
//! here is real but the backend, the secret store's failures and the system's drive letters: the
//! file is a real one in a temporary folder.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fsm_vfs::{
    ConnectError, DriveLetter, Health, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MemorySecretStore, MountBackend, MountError, Prompter, Remember, Secret,
    SecretRequest, SecretSlot, SecretStore, async_trait,
};
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast, watch};
use tokio::time::timeout;
use uuid::Uuid;

use super::*;
use crate::config::LetterConflict;
use crate::profile::{DriveChoice, Field, JumpAuth, JumpHost, Protocol, SftpAuth, SftpProfile};

const PATIENCE: Duration = Duration::from_secs(10);

/// A profile name, and what was in the dialog.
type Tested = (String, Vec<(SecretSlot, String)>);
type LockedChange = (&'static str, fn(&mut Profile));

/// What the next call of `MountBackend::mount` does.
enum NextMount {
    Fail(MountError),
    /// Waits until it is dropped, like a slow handshake or a dialog.
    Hang,
}

/// The backends' shared script and log. Without a script, a mount works.
struct Mock {
    next_mounts: Mutex<VecDeque<NextMount>>,
    next_unmount: Mutex<Option<MountError>>,
    created: AtomicUsize,
    alive: AtomicUsize,
    unmounts: AtomicUsize,
    letters: Mutex<Vec<char>>,
    /// Names of the profiles backends were made for, with the conflict setting.
    made_for: Mutex<Vec<(String, LetterConflict)>>,
    hanging: Notify,
    health: watch::Sender<Health>,
    /// Handed over by the first `take_remembered` after a mount that worked.
    to_remember: Mutex<Vec<(SecretSlot, String)>>,
    test_result: Mutex<Result<Duration, ConnectError>>,
    tested: Mutex<Vec<Tested>>,
    cannot_create: Mutex<Option<MountError>>,
    /// Listens to the manager, so that a backend going can tell whether it was too late.
    watcher: Mutex<Option<broadcast::Receiver<MountEvent>>>,
    announced_first: AtomicBool,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            next_mounts: Mutex::default(),
            next_unmount: Mutex::default(),
            created: AtomicUsize::default(),
            alive: AtomicUsize::default(),
            unmounts: AtomicUsize::default(),
            letters: Mutex::default(),
            made_for: Mutex::default(),
            hanging: Notify::new(),
            health: watch::Sender::new(Health::Ok),
            to_remember: Mutex::default(),
            test_result: Mutex::new(Ok(Duration::from_millis(12))),
            tested: Mutex::default(),
            cannot_create: Mutex::default(),
            watcher: Mutex::default(),
            announced_first: AtomicBool::new(false),
        }
    }
}

impl Mock {
    fn script(&self, next: NextMount) {
        self.next_mounts.lock().unwrap().push_back(next);
    }

    fn mounted_on(&self) -> Vec<char> {
        self.letters.lock().unwrap().clone()
    }

    fn alive(&self) -> usize {
        self.alive.load(Ordering::SeqCst)
    }

    fn created(&self) -> usize {
        self.created.load(Ordering::SeqCst)
    }

    fn unmounts(&self) -> usize {
        self.unmounts.load(Ordering::SeqCst)
    }

    /// NFR-SEC-03: a backend holds what was typed, so none may outlive the news that it has ended.
    /// Tests on this runtime take turns, so looking afterwards would not show it. For a profile's
    /// first mount only: the next backend's going would find the news of the one before.
    fn assert_gone_before_it_was_announced(&self) {
        assert_eq!(self.alive(), 0);
        assert!(!self.announced_first.load(Ordering::SeqCst));
    }

    async fn wait_until_hanging(&self) {
        timeout(PATIENCE, self.hanging.notified())
            .await
            .expect("no mount hung");
    }
}

struct MockBackend(Arc<Mock>);

impl Drop for MockBackend {
    fn drop(&mut self) {
        self.0.alive.fetch_sub(1, Ordering::SeqCst);
        if let Some(watcher) = self.0.watcher.lock().unwrap().as_mut() {
            while let Ok(event) = watcher.try_recv() {
                if matches!(event.state, MountState::Disconnected | MountState::Error(_)) {
                    self.0.announced_first.store(true, Ordering::SeqCst);
                }
            }
        }
    }
}

#[async_trait]
impl MountBackend for MockBackend {
    async fn mount(&self, letter: DriveLetter, _: &dyn Prompter) -> Result<(), MountError> {
        let next = self.0.next_mounts.lock().unwrap().pop_front();
        match next {
            Some(NextMount::Fail(error)) => return Err(error),
            Some(NextMount::Hang) => {
                self.0.hanging.notify_one();
                std::future::pending::<()>().await;
            }
            None => {}
        }
        self.0.letters.lock().unwrap().push(letter.as_char());
        Ok(())
    }

    async fn unmount(&self) -> Result<(), MountError> {
        self.0.unmounts.fetch_add(1, Ordering::SeqCst);
        match self.0.next_unmount.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.0.health.subscribe()
    }

    fn take_remembered(&self) -> Vec<(SecretSlot, Secret<String>)> {
        let mut remembered = self.0.to_remember.lock().unwrap();
        remembered
            .drain(..)
            .map(|(slot, value)| (slot, Secret::new(value)))
            .collect()
    }
}

struct MockFactory(Arc<Mock>);

#[async_trait]
impl BackendFactory for MockFactory {
    fn create(
        &self,
        profile: &Profile,
        settings: &Settings,
    ) -> Result<Box<dyn MountBackend>, MountError> {
        if let Some(error) = self.0.cannot_create.lock().unwrap().take() {
            return Err(error);
        }
        self.0.created.fetch_add(1, Ordering::SeqCst);
        self.0.alive.fetch_add(1, Ordering::SeqCst);
        let made_for = (profile.name.clone(), settings.letter_conflict);
        self.0.made_for.lock().unwrap().push(made_for);
        Ok(Box::new(MockBackend(Arc::clone(&self.0))))
    }

    async fn test(
        &self,
        profile: &Profile,
        _: &Settings,
        typed: Vec<(SecretSlot, Secret<String>)>,
        _: &dyn Prompter,
    ) -> Result<Duration, ConnectError> {
        let typed = typed
            .iter()
            .map(|(slot, value)| (*slot, value.expose_secret().clone()))
            .collect();
        self.0
            .tested
            .lock()
            .unwrap()
            .push((profile.name.clone(), typed));
        self.0.test_result.lock().unwrap().clone()
    }
}

/// No test gets as far as asking: the mock backend does not.
struct NoPrompts;

#[async_trait]
impl Prompter for NoPrompts {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        None
    }
    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        None
    }
    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }
    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
}

#[derive(Default)]
struct Letters {
    taken: Mutex<HashSet<char>>,
}

impl DriveLetters for Letters {
    fn is_free(&self, letter: DriveLetter) -> io::Result<bool> {
        Ok(!self.taken.lock().unwrap().contains(&letter.as_char()))
    }
}

/// A [`MemorySecretStore`] that can be told to fail.
#[derive(Default)]
struct Secrets {
    inner: MemorySecretStore,
    deleting_fails: Mutex<bool>,
    storing_fails: Mutex<bool>,
    deleted: Mutex<Vec<SecretSlot>>,
}

impl SecretStore for Secrets {
    fn store(&self, profile: Uuid, slot: SecretSlot, value: &Secret<String>) -> io::Result<()> {
        if *self.storing_fails.lock().unwrap() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.inner.store(profile, slot, value)
    }

    fn load(&self, profile: Uuid, slot: SecretSlot) -> io::Result<Option<Secret<String>>> {
        self.inner.load(profile, slot)
    }

    fn delete(&self, profile: Uuid, slot: SecretSlot) -> io::Result<bool> {
        if *self.deleting_fails.lock().unwrap() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        self.deleted.lock().unwrap().push(slot);
        self.inner.delete(profile, slot)
    }
}

impl Secrets {
    fn get(&self, profile: Uuid, slot: SecretSlot) -> Option<String> {
        let value = self.inner.load(profile, slot).unwrap();
        value.map(|value| value.expose_secret().clone())
    }

    fn put(&self, profile: Uuid, slot: SecretSlot, value: &str) {
        self.inner
            .store(profile, slot, &Secret::new(value.to_owned()))
            .unwrap();
    }
}

fn sftp(host: &str, auth: SftpAuth) -> SftpProfile {
    SftpProfile::new(host, "deploy", auth)
}

fn named(name: &str) -> Profile {
    Profile::new(name, Protocol::Sftp(sftp("example.com", SftpAuth::Agent)))
}

fn with_auth(name: &str, auth: SftpAuth) -> Profile {
    Profile::new(name, Protocol::Sftp(sftp("example.com", auth)))
}

fn sftp_mut(profile: &mut Profile) -> &mut SftpProfile {
    let Protocol::Sftp(sftp) = &mut profile.protocol;
    sftp
}

fn letter(character: char) -> DriveLetter {
    DriveLetter::new(character).unwrap()
}

fn typed(slot: SecretSlot, value: &str) -> Vec<(SecretSlot, Secret<String>)> {
    vec![(slot, Secret::new(value.to_owned()))]
}

struct World {
    folder: TempDir,
    mock: Arc<Mock>,
    secrets: Arc<Secrets>,
    letters: Arc<Letters>,
    manager: MountManager,
    events: broadcast::Receiver<MountEvent>,
}

fn path_in(folder: &TempDir) -> PathBuf {
    folder.path().join("config.toml")
}

/// Writes the profiles to a real file, and opens it the way the app would.
fn world_with(profiles: Vec<Profile>, settings: Settings) -> World {
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(path_in(&folder));
    let config = Config {
        settings,
        profiles,
        unreadable: Vec::new(),
    };
    store.save(&config).unwrap();
    world_from(folder)
}

fn world(profiles: Vec<Profile>) -> World {
    world_with(profiles, Settings::default())
}

fn world_from(folder: TempDir) -> World {
    let store = ConfigStore::open(path_in(&folder));
    let mock = Arc::new(Mock::default());
    let secrets = Arc::new(Secrets::default());
    let letters = Arc::new(Letters::default());
    let manager = MountManager::new(ManagerParts {
        store,
        factory: Arc::new(MockFactory(Arc::clone(&mock))),
        secrets: Arc::clone(&secrets) as _,
        letters: Arc::clone(&letters) as _,
        prompter: Arc::new(NoPrompts),
    });
    let events = manager.subscribe();
    *mock.watcher.lock().unwrap() = Some(manager.subscribe());
    World {
        folder,
        mock,
        secrets,
        letters,
        manager,
        events,
    }
}

impl World {
    async fn next_event(&mut self) -> MountEvent {
        timeout(PATIENCE, self.events.recv())
            .await
            .expect("no event came")
            .expect("the event channel broke")
    }

    async fn next_state(&mut self) -> MountState {
        self.next_event().await.state
    }

    /// Mounts and waits for it to work.
    async fn mounted(&mut self, id: Uuid) -> MountEvent {
        self.manager.mount(id).await.unwrap();
        assert_eq!(self.next_state().await, MountState::Connecting);
        let event = self.next_event().await;
        assert_eq!(event.state, MountState::Mounted, "{event:?}");
        event
    }

    /// Starts a mount that hangs, and waits for it to be hanging.
    async fn hanging(&mut self, id: Uuid) {
        self.mock.script(NextMount::Hang);
        self.manager.mount(id).await.unwrap();
        assert_eq!(self.next_state().await, MountState::Connecting);
        self.mock.wait_until_hanging().await;
    }

    async fn unmounted(&mut self, id: Uuid) {
        self.manager.unmount(id).await;
        assert_eq!(self.next_state().await, MountState::Unmounting);
        assert_eq!(self.next_state().await, MountState::Disconnected);
    }

    /// Waits for the profile's task to have looked at everything sent so far, health included:
    /// commands are answered in order, and it looks at health again after each one.
    async fn settled(&self, id: Uuid) {
        for _ in 0..2 {
            let config = self.manager.config().await;
            let same = config.profiles.iter().find(|p| p.id == id).cloned();
            let same = same.expect("the profile is in the file");
            self.manager.save(same, Vec::new()).await.unwrap();
        }
    }

    async fn no_more_events(&mut self, id: Uuid) {
        self.settled(id).await;
        let next = self.events.try_recv();
        assert!(
            matches!(next, Err(broadcast::error::TryRecvError::Empty)),
            "an event came that should not have: {next:?}"
        );
    }

    fn state(&self, id: Uuid) -> MountState {
        self.manager.status(id).unwrap().state
    }

    fn in_the_file(&self) -> Config {
        ConfigStore::open(path_in(&self.folder)).config().clone()
    }

    fn profile_in_the_file(&self, id: Uuid) -> Profile {
        let config = self.in_the_file();
        let found = config.profiles.iter().find(|p| p.id == id);
        found.expect("the profile is in the file").clone()
    }
}

fn ids(world: &World) -> Vec<Uuid> {
    let store = ConfigStore::open(path_in(&world.folder));
    store.config().profiles.iter().map(|p| p.id).collect()
}

/// A folder with a file that passes for a private key, for Save's check of the *Key file* box.
fn key_file(folder: &TempDir, name: &str) -> PathBuf {
    let path = folder.path().join(name);
    fs::write(&path, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
    path
}

// Spec §5.5, row by row.

#[tokio::test]
async fn a_profile_starts_disconnected_and_mounts_through_connecting() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    assert_eq!(world.state(id), MountState::Disconnected);

    world.manager.mount(id).await.unwrap();

    let connecting = world.next_event().await;
    assert_eq!(connecting.id, id);
    assert_eq!(connecting.state, MountState::Connecting);
    assert_eq!(connecting.detail, None);
    let mounted = world.next_event().await;
    assert_eq!(mounted.id, id);
    assert_eq!(mounted.state, MountState::Mounted);
    let detail = MountDetail {
        letter: letter('Z'),
        instead_of: None,
    };
    assert_eq!(mounted.detail, Some(detail));
    assert_eq!(world.state(id), MountState::Mounted);
    assert_eq!(world.manager.status(id).unwrap().detail, Some(detail));
    assert_eq!(world.mock.mounted_on(), ['Z']);
}

#[tokio::test]
async fn unmounting_goes_back_to_disconnected() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;

    world.manager.unmount(id).await;

    let unmounting = world.next_event().await;
    assert_eq!(unmounting.state, MountState::Unmounting);
    assert_eq!(unmounting.detail.map(|d| d.letter), Some(letter('Z')));
    let disconnected = world.next_event().await;
    assert_eq!(disconnected.state, MountState::Disconnected);
    assert_eq!(disconnected.detail, None);
    assert_eq!(world.state(id), MountState::Disconnected);
    assert_eq!(world.mock.unmounts(), 1);
}

#[tokio::test]
async fn the_backend_is_gone_by_the_time_disconnected_is_announced() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;
    assert_eq!(world.mock.alive(), 1);

    world.unmounted(id).await;

    world.mock.assert_gone_before_it_was_announced();
}

#[tokio::test]
async fn each_mount_gets_a_new_backend_for_the_profile_as_it_is_then() {
    let mut profile = named("Before");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);
    world.mounted(id).await;
    world.unmounted(id).await;
    profile.name = "After".to_owned();
    world.manager.save(profile, Vec::new()).await.unwrap();

    world.mounted(id).await;

    assert_eq!(world.mock.created(), 2);
    assert_eq!(world.mock.alive(), 1);
    let made_for = world.mock.made_for.lock().unwrap();
    assert_eq!(made_for[0].0, "Before");
    assert_eq!(made_for[1].0, "After");
}

#[tokio::test]
async fn cancel_while_connecting_returns_to_disconnected_at_once() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.hanging(id).await;

    let start = Instant::now();
    world.manager.cancel(id);

    assert_eq!(world.next_state().await, MountState::Disconnected);
    assert!(start.elapsed() < Duration::from_secs(1), "FR-MNT-11");
    world.mock.assert_gone_before_it_was_announced();
    assert_eq!(world.mock.unmounts(), 0, "nothing was up to unmount");
}

#[tokio::test]
async fn a_cancelled_mount_gives_its_letter_back_and_can_be_mounted_again() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.hanging(id).await;
    world.manager.cancel(id);
    assert_eq!(world.next_state().await, MountState::Disconnected);

    let mounted = world.mounted(id).await;

    assert_eq!(mounted.detail.map(|d| d.letter), Some(letter('Z')));
}

#[tokio::test]
async fn cancel_does_nothing_to_a_profile_that_is_not_connecting() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);

    world.manager.cancel(id);
    world.no_more_events(id).await;

    // An old cancel does not stay around for the next mount.
    world.mounted(id).await;
    world.manager.cancel(id);
    world.no_more_events(id).await;
    assert_eq!(world.state(id), MountState::Mounted);
}

#[tokio::test]
async fn cancel_only_reaches_the_profile_it_is_for() {
    let (one, two) = (named("One"), named("Two"));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    world.hanging(one_id).await;

    world.manager.cancel(two_id);
    world.no_more_events(two_id).await;

    assert_eq!(world.state(one_id), MountState::Connecting);
}

#[tokio::test]
async fn unmount_while_connecting_cancels() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.hanging(id).await;

    world.manager.unmount(id).await;

    assert_eq!(world.next_state().await, MountState::Disconnected);
    assert_eq!(world.mock.alive(), 0);
}

#[tokio::test]
async fn unmount_does_nothing_to_a_profile_with_no_drive() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);

    world.manager.unmount(id).await;
    world.no_more_events(id).await;

    assert_eq!(world.mock.unmounts(), 0);
}

#[tokio::test]
async fn unmount_does_nothing_to_a_profile_in_error_and_leaves_the_reason() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let reason = MountError::Other("boom".to_owned());
    world.mock.script(NextMount::Fail(reason.clone()));
    world.manager.mount(id).await.unwrap();
    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(world.next_state().await, MountState::Error(reason.clone()));

    world.manager.unmount(id).await;
    world.no_more_events(id).await;

    assert_eq!(world.state(id), MountState::Error(reason));
}

#[tokio::test]
async fn a_user_cancelling_a_prompt_is_not_an_error() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let cancelled = MountError::Connect(ConnectError::Cancelled);
    world.mock.script(NextMount::Fail(cancelled));

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(world.next_state().await, MountState::Disconnected);
    world.mock.assert_gone_before_it_was_announced();
}

#[tokio::test]
async fn a_failure_while_connecting_is_an_error_with_its_reason() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let reason = MountError::Connect(ConnectError::SftpUnavailable);
    world.mock.script(NextMount::Fail(reason.clone()));

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(world.next_state().await, MountState::Error(reason.clone()));
    assert_eq!(world.state(id), MountState::Error(reason));
    world.mock.assert_gone_before_it_was_announced();
}

#[tokio::test]
async fn a_backend_that_cannot_be_made_is_an_error() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.cannot_create.lock().unwrap() = Some(MountError::WinFspMissing);

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(
        world.next_state().await,
        MountState::Error(MountError::WinFspMissing)
    );
}

#[tokio::test]
async fn an_error_can_be_mounted_again() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let reason = MountError::Other("boom".to_owned());
    world.mock.script(NextMount::Fail(reason));
    world.manager.mount(id).await.unwrap();
    assert_eq!(world.next_state().await, MountState::Connecting);
    assert!(matches!(world.next_state().await, MountState::Error(_)));

    world.mounted(id).await;

    assert_eq!(world.state(id), MountState::Mounted);
}

#[tokio::test]
async fn a_mount_that_failed_gives_its_letter_back() {
    let (one, two) = (named("One"), named("Two"));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    let reason = MountError::Other("boom".to_owned());
    world.mock.script(NextMount::Fail(reason));
    world.manager.mount(one_id).await.unwrap();
    assert_eq!(world.next_state().await, MountState::Connecting);
    assert!(matches!(world.next_state().await, MountState::Error(_)));

    let mounted = world.mounted(two_id).await;

    assert_eq!(mounted.detail.map(|d| d.letter), Some(letter('Z')));
}

#[tokio::test]
async fn an_unmount_that_fails_ends_in_error_and_lets_go_of_the_backend_and_letter() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;
    let reason = MountError::Other("stopping the drive".to_owned());
    *world.mock.next_unmount.lock().unwrap() = Some(reason.clone());

    world.manager.unmount(id).await;

    assert_eq!(world.next_state().await, MountState::Unmounting);
    assert_eq!(world.next_state().await, MountState::Error(reason));
    world.mock.assert_gone_before_it_was_announced();
    let again = world.mounted(id).await;
    assert_eq!(again.detail.map(|d| d.letter), Some(letter('Z')));
}

#[tokio::test]
async fn mounting_what_is_in_use_is_refused_and_changes_nothing() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);

    world.hanging(id).await;
    assert_eq!(world.manager.mount(id).await, Err(MountRefused::Busy));
    assert_eq!(world.state(id), MountState::Connecting);
    world.manager.cancel(id);
    assert_eq!(world.next_state().await, MountState::Disconnected);

    world.mounted(id).await;
    assert_eq!(world.manager.mount(id).await, Err(MountRefused::Busy));
    assert_eq!(world.state(id), MountState::Mounted);
    assert_eq!(world.mock.created(), 2);
}

#[tokio::test]
async fn a_mount_right_after_an_unmount_waits_for_it() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;

    world.manager.unmount(id).await;
    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Unmounting);
    assert_eq!(world.next_state().await, MountState::Disconnected);
    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(world.next_state().await, MountState::Mounted);
}

#[tokio::test]
async fn a_profile_waiting_for_a_dialog_holds_up_no_other() {
    let (slow, quick) = (named("Slow"), named("Quick"));
    let (slow_id, quick_id) = (slow.id, quick.id);
    let mut world = world(vec![slow, quick]);
    world.hanging(slow_id).await;

    world.mounted(quick_id).await;
    world.unmounted(quick_id).await;

    assert_eq!(world.state(slow_id), MountState::Connecting);
}

// Connection lost: only what has no Reconnecting in it.

fn lost() -> Health {
    Health::Lost("gone".to_owned())
}

#[tokio::test]
async fn a_lost_connection_without_auto_reconnect_unmounts_and_is_an_error() {
    let mut profile = named("Production");
    profile.auto_reconnect = false;
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;

    world.mock.health.send_replace(lost());

    let event = world.next_event().await;
    let reason = MountError::Connect(ConnectError::ConnectionLost);
    assert_eq!(event.state, MountState::Error(reason));
    assert_eq!(event.detail, None);
    assert_eq!(world.mock.unmounts(), 1);
    world.mock.assert_gone_before_it_was_announced();
}

#[tokio::test]
async fn a_connection_lost_before_the_mount_was_noticed_counts_too() {
    let mut profile = named("Production");
    profile.auto_reconnect = false;
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mock.health.send_replace(lost());

    world.mounted(id).await;

    let reason = MountError::Connect(ConnectError::ConnectionLost);
    assert_eq!(world.next_state().await, MountState::Error(reason));
}

/// Reconnecting is M2-01's. Until then, FR-REC-02's "keeps the drive letter" is all it can do.
#[tokio::test]
async fn a_lost_connection_with_auto_reconnect_leaves_the_drive_for_now() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;

    world.mock.health.send_replace(lost());
    world.no_more_events(id).await;

    assert_eq!(world.state(id), MountState::Mounted);
    assert_eq!(world.mock.unmounts(), 0);
    world.unmounted(id).await;
}

/// The flag stays editable while mounted (FR-PROF-08), so what counts is its value when it happens.
#[tokio::test]
async fn auto_reconnect_turned_off_while_mounted_counts() {
    let mut profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);
    world.mounted(id).await;

    profile.auto_reconnect = false;
    world.manager.save(profile, Vec::new()).await.unwrap();
    world.mock.health.send_replace(lost());

    let reason = MountError::Connect(ConnectError::ConnectionLost);
    assert_eq!(world.next_state().await, MountState::Error(reason));
}

#[tokio::test]
async fn health_coming_back_changes_nothing() {
    let mut profile = named("Production");
    profile.auto_reconnect = false;
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;

    world.mock.health.send_replace(Health::Ok);
    world.no_more_events(id).await;

    assert_eq!(world.state(id), MountState::Mounted);
}

// Quitting.

#[tokio::test]
async fn shutdown_unmounts_what_is_up_cancels_what_is_connecting_and_waits() {
    let (up, connecting, idle) = (named("Up"), named("Connecting"), named("Idle"));
    let (up_id, connecting_id) = (up.id, connecting.id);
    let idle_id = idle.id;
    let mut world = world(vec![up, connecting, idle]);
    world.mounted(up_id).await;
    world.hanging(connecting_id).await;

    timeout(PATIENCE, world.manager.shutdown()).await.unwrap();

    // All of it happened before shutdown returned.
    let mut states = Vec::new();
    while let Ok(event) = world.events.try_recv() {
        states.push((event.id, event.state));
    }
    assert!(states.contains(&(up_id, MountState::Unmounting)));
    assert!(states.contains(&(up_id, MountState::Disconnected)));
    assert!(states.contains(&(connecting_id, MountState::Disconnected)));
    assert!(!states.iter().any(|(id, _)| *id == idle_id));
    assert_eq!(world.mock.unmounts(), 1);
    assert_eq!(world.mock.alive(), 0);

    assert_eq!(world.manager.status(up_id), None);
    assert_eq!(
        world.manager.mount(up_id).await,
        Err(MountRefused::UnknownProfile)
    );
}

#[tokio::test]
async fn the_last_handle_going_takes_the_drives_with_it() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.mounted(id).await;
    let World {
        manager,
        mut events,
        mock,
        folder: _folder,
        ..
    } = world;
    let another = manager.clone();
    drop(manager);
    assert_eq!(another.status(id).unwrap().state, MountState::Mounted);

    drop(another);

    let next = timeout(PATIENCE, events.recv()).await.unwrap().unwrap();
    assert_eq!(next.state, MountState::Unmounting);
    let next = timeout(PATIENCE, events.recv()).await.unwrap().unwrap();
    assert_eq!(next.state, MountState::Disconnected);
    assert_eq!(mock.alive(), 0);
}

// Letters.

#[tokio::test]
async fn mounts_in_progress_do_not_get_the_same_letter() {
    let (one, two) = (named("One"), named("Two"));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    world.hanging(one_id).await;

    let second = world.mounted(two_id).await;

    assert_eq!(second.detail.map(|d| d.letter), Some(letter('Y')));
}

#[tokio::test]
async fn a_letter_comes_free_when_its_drive_goes() {
    let (one, two) = (named("One"), named("Two"));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    world.mounted(one_id).await;
    world.unmounted(one_id).await;

    let second = world.mounted(two_id).await;

    assert_eq!(second.detail.map(|d| d.letter), Some(letter('Z')));
}

#[tokio::test]
async fn a_mounted_drive_keeps_its_letter_from_other_profiles() {
    let (one, mut two) = (named("One"), named("Two"));
    two.drive = DriveChoice::Letter(letter('Z'));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    world.mounted(one_id).await;

    let second = world.mounted(two_id).await;

    let expected = MountDetail {
        letter: letter('Y'),
        instead_of: Some(letter('Z')),
    };
    assert_eq!(second.detail, Some(expected));
}

#[tokio::test]
async fn two_profiles_on_one_letter_cannot_both_have_it() {
    let (mut one, mut two) = (named("One"), named("Two"));
    one.drive = DriveChoice::Letter(letter('M'));
    two.drive = DriveChoice::Letter(letter('M'));
    let (one_id, two_id) = (one.id, two.id);
    let mut world = world(vec![one, two]);
    world.mounted(one_id).await;

    let second = world.mounted(two_id).await;

    let expected = MountDetail {
        letter: letter('Z'),
        instead_of: Some(letter('M')),
    };
    assert_eq!(second.detail, Some(expected));
}

#[tokio::test]
async fn a_taken_letter_under_auto_mounts_on_another_and_says_so() {
    let mut profile = named("Production");
    profile.drive = DriveChoice::Letter(letter('Z'));
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.letters.taken.lock().unwrap().insert('Z');

    let mounted = world.mounted(id).await;

    let expected = MountDetail {
        letter: letter('Y'),
        instead_of: Some(letter('Z')),
    };
    assert_eq!(mounted.detail, Some(expected));
    assert_eq!(world.mock.mounted_on(), ['Y']);
}

#[tokio::test]
async fn a_taken_letter_under_fail_stops_before_any_backend_is_made() {
    let mut profile = named("Production");
    profile.drive = DriveChoice::Letter(letter('Z'));
    let id = profile.id;
    let settings = Settings {
        letter_conflict: LetterConflict::Fail,
        ..Settings::default()
    };
    let mut world = world_with(vec![profile], settings);
    world.letters.taken.lock().unwrap().insert('Z');

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(
        world.next_state().await,
        MountState::Error(MountError::LetterTaken(letter('Z')))
    );
    assert_eq!(world.mock.created(), 0);
}

#[tokio::test]
async fn no_free_letter_is_an_error() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.letters.taken.lock().unwrap().extend('D'..='Z');

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(
        world.next_state().await,
        MountState::Error(MountError::NoFreeLetter)
    );
}

/// The letter can go during a long dialog, which only the backend finds out.
#[tokio::test]
async fn a_letter_taken_during_the_mount_moves_on_with_the_same_backend() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let taken = MountError::LetterTaken(letter('Z'));
    world.mock.script(NextMount::Fail(taken));

    let mounted = world.mounted(id).await;

    let expected = MountDetail {
        letter: letter('Y'),
        instead_of: None,
    };
    assert_eq!(mounted.detail, Some(expected));
    assert_eq!(world.mock.created(), 1, "what was typed is in this one");
}

#[tokio::test]
async fn a_profiles_letter_taken_during_the_mount_goes_by_the_setting() {
    for (conflict, moves_on) in [(LetterConflict::Auto, true), (LetterConflict::Fail, false)] {
        let mut profile = named("Production");
        profile.drive = DriveChoice::Letter(letter('Z'));
        let id = profile.id;
        let settings = Settings {
            letter_conflict: conflict,
            ..Settings::default()
        };
        let mut world = world_with(vec![profile], settings);
        let taken = MountError::LetterTaken(letter('Z'));
        world.mock.script(NextMount::Fail(taken.clone()));

        world.manager.mount(id).await.unwrap();

        assert_eq!(world.next_state().await, MountState::Connecting);
        let event = world.next_event().await;
        if moves_on {
            assert_eq!(event.state, MountState::Mounted);
            let expected = MountDetail {
                letter: letter('Y'),
                instead_of: Some(letter('Z')),
            };
            assert_eq!(event.detail, Some(expected));
        } else {
            assert_eq!(event.state, MountState::Error(taken));
        }
    }
}

#[tokio::test]
async fn letters_that_keep_being_taken_end_in_no_free_letter() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    // In the order the allocator goes through them.
    for taken in ('D'..='Z').rev() {
        let taken = MountError::LetterTaken(letter(taken));
        world.mock.script(NextMount::Fail(taken));
    }

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert_eq!(
        world.next_state().await,
        MountState::Error(MountError::NoFreeLetter)
    );
}

// What may not be mounted (M1-14's list), and the file that cannot be saved.

/// The decision is M1-15's and unconfirmed: this is the test to change if the owner reverses it.
#[test]
fn profiles_from_a_newer_file_may_be_mounted() {
    assert!(may_mount_while_read_only(&ReadOnlyReason::Newer {
        found: 99
    }));
}

fn newer_file(profiles: Vec<Profile>) -> TempDir {
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(path_in(&folder));
    let config = Config {
        profiles,
        ..Config::default()
    };
    store.save(&config).unwrap();
    let text = fs::read_to_string(path_in(&folder)).unwrap();
    let text = text.replace("schema_version = 1", "schema_version = 99");
    fs::write(path_in(&folder), text).unwrap();
    folder
}

#[tokio::test]
async fn a_newer_file_mounts_but_does_not_save_or_delete() {
    let mut profile = named("Production");
    let id = profile.id;
    let folder = newer_file(vec![profile.clone()]);
    let before = fs::read(path_in(&folder)).unwrap();
    let mut world = world_from(folder);
    assert!(matches!(
        world.manager.read_only().await,
        Some(ReadOnlyReason::Newer { found: 99 })
    ));
    world.secrets.put(id, SecretSlot::Password, "kept");

    world.mounted(id).await;

    profile.name = "Renamed".to_owned();
    assert!(matches!(
        world.manager.save(profile, Vec::new()).await,
        Err(ProfileChangeError::ReadOnly(ReadOnlyReason::Newer { .. }))
    ));
    assert!(matches!(
        world.manager.delete(id).await,
        Err(ProfileChangeError::ReadOnly(ReadOnlyReason::Newer { .. }))
    ));
    assert_eq!(world.state(id), MountState::Mounted, "not unmounted for it");
    assert_eq!(world.mock.unmounts(), 0);
    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("kept")
    );
    assert_eq!(fs::read(path_in(&world.folder)).unwrap(), before);
}

#[tokio::test]
async fn a_delete_of_a_disconnected_profile_is_refused_too_when_read_only() {
    let profile = named("Production");
    let id = profile.id;
    let world = world_from(newer_file(vec![profile]));
    world.secrets.put(id, SecretSlot::Password, "kept");

    assert!(matches!(
        world.manager.delete(id).await,
        Err(ProfileChangeError::ReadOnly(_))
    ));

    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("kept")
    );
    assert_eq!(world.state(id), MountState::Disconnected);
}

#[tokio::test]
async fn a_damaged_file_has_nothing_to_mount_and_saves_nothing() {
    let folder = TempDir::new().unwrap();
    fs::write(path_in(&folder), "this is not toml [").unwrap();
    let world = world_from(folder);

    assert!(matches!(
        world.manager.read_only().await,
        Some(ReadOnlyReason::Damaged { .. })
    ));
    assert_eq!(
        world.manager.mount(Uuid::new_v4()).await,
        Err(MountRefused::UnknownProfile)
    );
    assert!(matches!(
        world.manager.save(named("New"), Vec::new()).await,
        Err(ProfileChangeError::ReadOnly(_))
    ));
    assert_eq!(world.mock.created(), 0);
}

#[tokio::test]
async fn an_unknown_profile_is_refused() {
    let world = world(Vec::new());
    assert_eq!(
        world.manager.mount(Uuid::new_v4()).await,
        Err(MountRefused::UnknownProfile)
    );
    assert_eq!(world.manager.status(Uuid::new_v4()), None);
}

#[tokio::test]
async fn a_profile_that_breaks_a_rule_is_not_mounted() {
    let mut profile = named("Production");
    sftp_mut(&mut profile).keepalive_secs = 9999;
    let id = profile.id;
    let world = world(vec![profile]);

    let refused = world.manager.mount(id).await.unwrap_err();

    let MountRefused::Invalid(errors) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, Field::KeepAlive);
    assert_eq!(world.mock.created(), 0);
    assert_eq!(world.state(id), MountState::Disconnected);
}

const UNREADABLE: &str = r#"
[[profiles]]
id = "ID"
name = "Broken"
protocol = "sftp"
host = "example.com"
port = 70000
username = "deploy"
auth = "agent"
"#;

fn with_unreadable(profiles: Vec<Profile>, id: Uuid) -> TempDir {
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(path_in(&folder));
    let config = Config {
        profiles,
        ..Config::default()
    };
    store.save(&config).unwrap();
    let mut text = fs::read_to_string(path_in(&folder)).unwrap();
    text.push_str(&UNREADABLE.replace("ID", &id.to_string()));
    fs::write(path_in(&folder), text).unwrap();
    folder
}

#[tokio::test]
async fn an_unreadable_profile_is_not_mounted() {
    let id = Uuid::new_v4();
    let world = world_from(with_unreadable(vec![named("Other")], id));

    let refused = world.manager.mount(id).await.unwrap_err();

    assert!(
        matches!(refused, MountRefused::Unreadable { .. }),
        "{refused:?}"
    );
    assert_eq!(world.mock.created(), 0);
}

// Secrets the user asked to remember.

#[tokio::test]
async fn a_password_ticked_in_the_prompt_is_stored_and_the_profile_marked() {
    let profile = with_auth("Production", SftpAuth::Password { remember: false });
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.to_remember.lock().unwrap() = vec![(SecretSlot::Password, "typed".to_owned())];

    world.mounted(id).await;

    // By the time the row says Mounted.
    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("typed")
    );
    let Protocol::Sftp(saved) = world.profile_in_the_file(id).protocol;
    assert_eq!(saved.auth, SftpAuth::Password { remember: true });
}

#[tokio::test]
async fn a_passphrase_and_a_jump_password_go_in_their_own_slots() {
    let keys = TempDir::new().unwrap();
    let path = key_file(&keys, "id_ed25519");
    let auth = SftpAuth::KeyFile {
        path,
        remember: false,
    };
    let mut profile = with_auth("Production", auth);
    sftp_mut(&mut profile).jump = Some(JumpHost {
        host: "bastion".to_owned(),
        port: 22,
        username: None,
        auth: JumpAuth::Password,
    });
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.to_remember.lock().unwrap() = vec![
        (SecretSlot::Passphrase, "phrase".to_owned()),
        (SecretSlot::JumpPassword, "jump".to_owned()),
    ];

    world.mounted(id).await;

    assert_eq!(
        world.secrets.get(id, SecretSlot::Passphrase).as_deref(),
        Some("phrase")
    );
    assert_eq!(
        world.secrets.get(id, SecretSlot::JumpPassword).as_deref(),
        Some("jump")
    );
    let Protocol::Sftp(saved) = world.profile_in_the_file(id).protocol;
    assert!(matches!(
        saved.auth,
        SftpAuth::KeyFile { remember: true, .. }
    ));
}

/// FR-SSH-05, whatever a backend does.
#[tokio::test]
async fn ask_each_time_never_stores_anything() {
    let profile = with_auth("Production", SftpAuth::Ask);
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.to_remember.lock().unwrap() = vec![
        (SecretSlot::Password, "typed".to_owned()),
        (SecretSlot::Passphrase, "typed".to_owned()),
        (SecretSlot::JumpPassword, "typed".to_owned()),
    ];

    world.mounted(id).await;

    assert!(world.secrets.inner.is_empty());
}

#[tokio::test]
async fn nothing_is_stored_from_a_mount_that_failed() {
    let profile = with_auth("Production", SftpAuth::Password { remember: true });
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.to_remember.lock().unwrap() = vec![(SecretSlot::Password, "wrong".to_owned())];
    let refused = MountError::Connect(ConnectError::Auth {
        user: "deploy".to_owned(),
        host: "example.com".to_owned(),
    });
    world.mock.script(NextMount::Fail(refused));

    world.manager.mount(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Connecting);
    assert!(matches!(world.next_state().await, MountState::Error(_)));
    assert!(world.secrets.inner.is_empty());
}

#[tokio::test]
async fn a_secret_that_cannot_be_stored_does_not_stop_the_drive() {
    let profile = with_auth("Production", SftpAuth::Password { remember: false });
    let id = profile.id;
    let mut world = world(vec![profile]);
    *world.mock.to_remember.lock().unwrap() = vec![(SecretSlot::Password, "typed".to_owned())];
    *world.secrets.storing_fails.lock().unwrap() = true;

    world.mounted(id).await;

    assert_eq!(world.state(id), MountState::Mounted);
    let Protocol::Sftp(saved) = world.profile_in_the_file(id).protocol;
    assert_eq!(
        saved.auth,
        SftpAuth::Password { remember: false },
        "the box stays unticked, as nothing is there"
    );
}

/// The secret is not in the file, so a read-only file does not stop it being remembered.
#[tokio::test]
async fn a_read_only_file_is_not_written_for_a_remembered_secret() {
    let profile = with_auth("Production", SftpAuth::Password { remember: false });
    let id = profile.id;
    let folder = newer_file(vec![profile]);
    let before = fs::read(path_in(&folder)).unwrap();
    let mut world = world_from(folder);
    *world.mock.to_remember.lock().unwrap() = vec![(SecretSlot::Password, "typed".to_owned())];

    world.mounted(id).await;

    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("typed")
    );
    assert_eq!(fs::read(path_in(&world.folder)).unwrap(), before);
}

// Save.

#[tokio::test]
async fn a_new_profile_is_saved_and_can_be_mounted() {
    let mut world = world(Vec::new());
    let profile = named("Production");
    let id = profile.id;

    world
        .manager
        .save(profile.clone(), Vec::new())
        .await
        .unwrap();

    assert_eq!(world.profile_in_the_file(id), profile);
    assert_eq!(world.manager.config().await.profiles, vec![profile]);
    assert_eq!(world.state(id), MountState::Disconnected);
    world.mounted(id).await;
}

#[tokio::test]
async fn save_checks_the_fields_and_changes_nothing_if_they_are_wrong() {
    let world = world(Vec::new());
    let mut profile = named("Production");
    sftp_mut(&mut profile).port = 0;
    let id = profile.id;

    let refused = world.manager.save(profile, Vec::new()).await.unwrap_err();

    let ProfileChangeError::Invalid(errors) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(errors[0].field, Field::Port);
    assert!(world.in_the_file().profiles.is_empty());
    assert_eq!(world.manager.status(id), None, "nothing is left behind");
}

#[tokio::test]
async fn a_second_profile_cannot_take_a_name_ignoring_case() {
    let world = world(vec![named("Production")]);

    let refused = world
        .manager
        .save(named("production"), Vec::new())
        .await
        .unwrap_err();

    let ProfileChangeError::Invalid(errors) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(errors[0].field, Field::Name);
    assert_eq!(world.in_the_file().profiles.len(), 1);
}

#[tokio::test]
async fn two_saves_at_once_cannot_both_take_a_name() {
    let world = world(Vec::new());

    let (one, two) = tokio::join!(
        world.manager.save(named("Production"), Vec::new()),
        world.manager.save(named("production"), Vec::new()),
    );

    assert!(one.is_ok() != two.is_ok(), "{one:?} {two:?}");
    assert_eq!(world.in_the_file().profiles.len(), 1);
}

/// FR-PROF-08's list, field by field.
fn locked_changes() -> Vec<LockedChange> {
    vec![
        ("host", |p| {
            sftp_mut(p).host = "other.example.com".to_owned();
        }),
        ("port", |p| sftp_mut(p).port = 2222),
        ("user", |p| sftp_mut(p).username = "other".to_owned()),
        ("authentication", |p| sftp_mut(p).auth = SftpAuth::Ask),
        ("remote path", |p| {
            sftp_mut(p).remote_path = "/srv".to_owned();
        }),
        ("drive letter", |p| {
            p.drive = DriveChoice::Letter(letter('M'));
        }),
        ("jump host", |p| {
            sftp_mut(p).jump = Some(JumpHost {
                host: "bastion".to_owned(),
                port: 22,
                username: None,
                auth: JumpAuth::Agent,
            });
        }),
        ("read-only", |p| p.read_only = true),
        ("cache", |p| sftp_mut(p).cache_ttl_secs = 30),
        ("keep-alive", |p| sftp_mut(p).keepalive_secs = 30),
    ]
}

#[tokio::test]
async fn connection_fields_are_locked_while_mounted_and_free_after() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);
    world.mounted(id).await;

    for (what, change) in locked_changes() {
        let mut changed = profile.clone();
        change(&mut changed);
        let refused = world.manager.save(changed, Vec::new()).await;
        assert!(
            matches!(refused, Err(ProfileChangeError::ConnectionFieldsLocked)),
            "{what}: {refused:?}"
        );
    }
    assert_eq!(world.profile_in_the_file(id), profile);

    world.unmounted(id).await;
    for (what, change) in locked_changes() {
        let mut changed = profile.clone();
        change(&mut changed);
        let saved = world.manager.save(changed.clone(), Vec::new()).await;
        assert!(saved.is_ok(), "{what}: {saved:?}");
        assert_eq!(world.profile_in_the_file(id), changed, "{what}");
    }
}

#[tokio::test]
async fn connection_fields_are_locked_while_connecting_too() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);
    world.hanging(id).await;

    let mut changed = profile.clone();
    sftp_mut(&mut changed).host = "other.example.com".to_owned();

    assert!(matches!(
        world.manager.save(changed, Vec::new()).await,
        Err(ProfileChangeError::ConnectionFieldsLocked)
    ));
    assert_eq!(world.state(id), MountState::Connecting, "the mount goes on");
}

#[tokio::test]
async fn what_is_not_a_connection_field_can_change_while_mounted() {
    let mut profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);
    world.mounted(id).await;

    profile.name = "Renamed".to_owned();
    profile.mount_on_start = true;
    profile.auto_reconnect = false;
    world
        .manager
        .save(profile.clone(), Vec::new())
        .await
        .unwrap();

    assert_eq!(world.profile_in_the_file(id), profile);
    assert_eq!(world.state(id), MountState::Mounted);
    assert_eq!(world.mock.unmounts(), 0);
}

/// FR-PROF-02's criterion.
#[tokio::test]
async fn a_rename_keeps_the_secrets() {
    let mut profile = with_auth("Production", SftpAuth::Password { remember: true });
    let id = profile.id;
    let world = world(vec![profile.clone()]);
    world.secrets.put(id, SecretSlot::Password, "kept");

    profile.name = "Renamed".to_owned();
    world.manager.save(profile, Vec::new()).await.unwrap();

    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("kept")
    );
}

#[tokio::test]
async fn the_password_in_the_dialog_is_stored_if_remember_is_ticked() {
    let world = world(Vec::new());
    let profile = with_auth("Production", SftpAuth::Password { remember: true });
    let id = profile.id;

    let secrets = typed(SecretSlot::Password, "typed-secret");
    world.manager.save(profile, secrets).await.unwrap();

    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("typed-secret")
    );
    let text = fs::read_to_string(path_in(&world.folder)).unwrap();
    assert!(!text.contains("typed-secret"), "NFR-SEC-02");
}

#[tokio::test]
async fn the_password_in_the_dialog_is_not_stored_if_it_is_not() {
    let world = world(Vec::new());
    let cases = [
        with_auth("One", SftpAuth::Password { remember: false }),
        with_auth("Two", SftpAuth::Ask),
        with_auth("Three", SftpAuth::Agent),
    ];
    for profile in cases {
        let secrets = typed(SecretSlot::Password, "typed");
        world.manager.save(profile, secrets).await.unwrap();
    }
    assert!(world.secrets.inner.is_empty());
}

#[tokio::test]
async fn a_secret_that_cannot_be_stored_leaves_the_profile_saved_and_says_so() {
    let world = world(Vec::new());
    let profile = with_auth("Production", SftpAuth::Password { remember: true });
    let id = profile.id;
    *world.secrets.storing_fails.lock().unwrap() = true;

    let secrets = typed(SecretSlot::Password, "too long");
    let result = world.manager.save(profile.clone(), secrets).await;

    assert!(matches!(result, Err(ProfileChangeError::StoringSecret(_))));
    assert_eq!(world.profile_in_the_file(id), profile);
    assert!(world.manager.status(id).is_some(), "it can be mounted");
}

/// Spec §6.2, from each method to each method.
#[tokio::test]
async fn secrets_go_when_the_profile_stops_having_a_use_for_them() {
    let keys = TempDir::new().unwrap();
    let path = key_file(&keys, "id_ed25519");
    let password = |remember| SftpAuth::Password { remember };
    let key = |remember| SftpAuth::KeyFile {
        path: path.clone(),
        remember,
    };
    let cases = [
        (password(true), password(true), Some(SecretSlot::Password)),
        (password(true), password(false), None),
        (password(true), key(true), None),
        (password(true), SftpAuth::Agent, None),
        (password(true), SftpAuth::Ask, None),
        (key(true), key(true), Some(SecretSlot::Passphrase)),
        (key(true), key(false), None),
        (key(true), password(true), None),
        (key(true), SftpAuth::Agent, None),
        (key(true), SftpAuth::Ask, None),
    ];
    for (from, to, kept) in cases {
        let mut profile = with_auth("Production", from.clone());
        let id = profile.id;
        let world = world(vec![profile.clone()]);
        let stored = match from {
            SftpAuth::Password { .. } => SecretSlot::Password,
            _ => SecretSlot::Passphrase,
        };
        world.secrets.put(id, stored, "secret");
        sftp_mut(&mut profile).auth = to.clone();

        world.manager.save(profile, Vec::new()).await.unwrap();

        for slot in [SecretSlot::Password, SecretSlot::Passphrase] {
            assert_eq!(
                world.secrets.get(id, slot).is_some(),
                kept == Some(slot),
                "{from:?} to {to:?}, {slot:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_passphrase_of_another_key_file_goes() {
    let keys = TempDir::new().unwrap();
    let (old, new) = (key_file(&keys, "old"), key_file(&keys, "new"));
    let auth = |path: &PathBuf| SftpAuth::KeyFile {
        path: path.clone(),
        remember: true,
    };
    let mut profile = with_auth("Production", auth(&old));
    let id = profile.id;
    let world = world(vec![profile.clone()]);
    world.secrets.put(id, SecretSlot::Passphrase, "passphrase");

    sftp_mut(&mut profile).auth = auth(&new);
    world.manager.save(profile, Vec::new()).await.unwrap();

    assert_eq!(world.secrets.get(id, SecretSlot::Passphrase), None);
}

#[tokio::test]
async fn the_jump_password_goes_with_the_jump_host_or_its_password() {
    let jump = |auth| JumpHost {
        host: "bastion".to_owned(),
        port: 22,
        username: None,
        auth,
    };
    let cases = [
        (Some(jump(JumpAuth::Password)), true),
        (Some(jump(JumpAuth::Agent)), false),
        (None, false),
    ];
    for (to, kept) in cases {
        let mut profile = named("Production");
        sftp_mut(&mut profile).jump = Some(jump(JumpAuth::Password));
        let id = profile.id;
        let world = world(vec![profile.clone()]);
        world.secrets.put(id, SecretSlot::JumpPassword, "jump");

        sftp_mut(&mut profile).jump = to;
        world.manager.save(profile, Vec::new()).await.unwrap();

        assert_eq!(
            world.secrets.get(id, SecretSlot::JumpPassword).is_some(),
            kept
        );
    }
}

#[tokio::test]
async fn a_secret_that_cannot_be_deleted_stops_the_save() {
    let mut profile = with_auth("Production", SftpAuth::Password { remember: true });
    let id = profile.id;
    let world = world(vec![profile.clone()]);
    world.secrets.put(id, SecretSlot::Password, "kept");
    *world.secrets.deleting_fails.lock().unwrap() = true;
    let before = profile.clone();

    sftp_mut(&mut profile).auth = SftpAuth::Agent;
    let result = world.manager.save(profile, Vec::new()).await;

    assert!(matches!(
        result,
        Err(ProfileChangeError::DeletingSecrets(_))
    ));
    assert_eq!(world.profile_in_the_file(id), before);
}

#[tokio::test]
async fn a_save_the_disk_refuses_changes_no_profile_and_can_be_tried_again() {
    let mut profile = named("Production");
    let id = profile.id;
    let world = world(vec![profile.clone()]);
    let path = path_in(&world.folder);
    let good = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();

    profile.name = "Renamed".to_owned();
    let result = world.manager.save(profile.clone(), Vec::new()).await;

    assert!(matches!(
        result,
        Err(ProfileChangeError::Save(SaveError::Io(_)))
    ));
    let config = world.manager.config().await;
    assert_eq!(config.profiles[0].name, "Production");

    fs::remove_dir(&path).unwrap();
    fs::write(&path, good).unwrap();
    world
        .manager
        .save(profile.clone(), Vec::new())
        .await
        .unwrap();
    assert_eq!(world.profile_in_the_file(id), profile);
}

#[tokio::test]
async fn a_new_profile_the_disk_refuses_leaves_no_task_behind() {
    let world = world(Vec::new());
    let path = path_in(&world.folder);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let profile = named("Production");
    let id = profile.id;

    assert!(world.manager.save(profile, Vec::new()).await.is_err());

    assert_eq!(world.manager.status(id), None);
}

// Delete.

#[tokio::test]
async fn delete_takes_the_profile_and_its_secrets_and_leaves_the_others() {
    let (gone, stays) = (named("Gone"), named("Stays"));
    let (gone_id, stays_id) = (gone.id, stays.id);
    let world = world(vec![gone, stays]);
    for slot in SecretSlot::ALL {
        world.secrets.put(gone_id, slot, "value");
        world.secrets.put(stays_id, slot, "value");
    }

    world.manager.delete(gone_id).await.unwrap();

    assert_eq!(ids(&world), [stays_id]);
    assert_eq!(world.secrets.inner.len(), 3, "FR-PROF-07");
    for slot in SecretSlot::ALL {
        assert_eq!(world.secrets.get(gone_id, slot), None);
        assert!(world.secrets.get(stays_id, slot).is_some());
    }
    assert_eq!(world.manager.status(gone_id), None);
    assert_eq!(
        world.manager.mount(gone_id).await,
        Err(MountRefused::UnknownProfile)
    );
}

#[tokio::test]
async fn deleting_a_mounted_profile_unmounts_it_first() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.secrets.put(id, SecretSlot::Password, "secret");
    world.mounted(id).await;

    world.manager.delete(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Unmounting);
    assert_eq!(world.next_state().await, MountState::Disconnected);
    assert_eq!(world.mock.unmounts(), 1);
    assert_eq!(world.mock.alive(), 0);
    assert!(ids(&world).is_empty());
    assert!(world.secrets.inner.is_empty());
}

#[tokio::test]
async fn deleting_a_connecting_profile_cancels_it_first() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    world.hanging(id).await;

    world.manager.delete(id).await.unwrap();

    assert_eq!(world.next_state().await, MountState::Disconnected);
    assert_eq!(world.mock.alive(), 0);
    assert!(ids(&world).is_empty());
}

#[tokio::test]
async fn deleting_an_unknown_profile_says_so() {
    let world = world(Vec::new());
    assert!(matches!(
        world.manager.delete(Uuid::new_v4()).await,
        Err(ProfileChangeError::UnknownProfile)
    ));
}

/// Secrets first: otherwise they would be left where nothing would ever find them.
#[tokio::test]
async fn a_secret_that_cannot_be_deleted_keeps_the_profile_so_it_can_be_tried_again() {
    let profile = named("Production");
    let id = profile.id;
    let world = world(vec![profile]);
    world.secrets.put(id, SecretSlot::Password, "secret");
    *world.secrets.deleting_fails.lock().unwrap() = true;

    let result = world.manager.delete(id).await;

    assert!(matches!(
        result,
        Err(ProfileChangeError::DeletingSecrets(_))
    ));
    assert_eq!(ids(&world), [id]);
    assert_eq!(world.state(id), MountState::Disconnected);

    *world.secrets.deleting_fails.lock().unwrap() = false;
    world.manager.delete(id).await.unwrap();
    assert!(ids(&world).is_empty());
    assert!(world.secrets.inner.is_empty());
}

#[tokio::test]
async fn a_delete_the_disk_refuses_keeps_the_profile() {
    let profile = named("Production");
    let id = profile.id;
    let world = world(vec![profile]);
    let path = path_in(&world.folder);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();

    let result = world.manager.delete(id).await;

    assert!(matches!(result, Err(ProfileChangeError::Save(_))));
    assert_eq!(world.manager.config().await.profiles.len(), 1);
    assert!(world.manager.status(id).is_some());
}

#[tokio::test]
async fn an_unreadable_entry_can_be_deleted_with_its_secrets() {
    let id = Uuid::new_v4();
    let world = world_from(with_unreadable(vec![named("Other")], id));
    world.secrets.put(id, SecretSlot::Password, "secret");
    let entry = world.manager.config().await.unreadable[0].clone();

    world
        .manager
        .delete_unreadable(entry.clone())
        .await
        .unwrap();

    assert!(world.in_the_file().unreadable.is_empty());
    assert!(world.secrets.inner.is_empty());
    assert!(matches!(
        world.manager.delete_unreadable(entry).await,
        Err(ProfileChangeError::UnknownProfile)
    ));
}

/// "A second use of an id": the secrets under it are the good profile's.
#[tokio::test]
async fn deleting_a_duplicate_of_a_good_id_leaves_that_profiles_secrets() {
    let profile = named("Good");
    let id = profile.id;
    let world = world_from(with_unreadable(vec![profile], id));
    world.secrets.put(id, SecretSlot::Password, "secret");
    let entry = world.manager.config().await.unreadable[0].clone();

    world.manager.delete_unreadable(entry).await.unwrap();

    assert!(world.in_the_file().unreadable.is_empty());
    assert_eq!(ids(&world), [id]);
    assert_eq!(
        world.secrets.get(id, SecretSlot::Password).as_deref(),
        Some("secret")
    );
}

#[tokio::test]
async fn an_unreadable_entry_stays_when_the_file_is_read_only() {
    let id = Uuid::new_v4();
    let folder = with_unreadable(vec![named("Other")], id);
    let text = fs::read_to_string(path_in(&folder)).unwrap();
    let text = text.replace("schema_version = 1", "schema_version = 99");
    fs::write(path_in(&folder), text).unwrap();
    let world = world_from(folder);
    world.secrets.put(id, SecretSlot::Password, "secret");
    let entry = world.manager.config().await.unreadable[0].clone();

    assert!(matches!(
        world.manager.delete_unreadable(entry).await,
        Err(ProfileChangeError::ReadOnly(_))
    ));
    assert!(!world.secrets.inner.is_empty());
}

// Test connection.

#[tokio::test]
async fn test_reports_the_round_trip_and_touches_nothing() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile.clone()]);

    let secrets = typed(SecretSlot::Password, "typed");
    let round_trip = world.manager.test(&profile, secrets).await.unwrap();

    assert_eq!(round_trip, Duration::from_millis(12));
    let tested = world.mock.tested.lock().unwrap().clone();
    let expected = vec![(SecretSlot::Password, "typed".to_owned())];
    assert_eq!(tested, [("Production".to_owned(), expected)]);
    assert!(world.secrets.inner.is_empty(), "nothing is stored");
    assert_eq!(world.mock.created(), 0);
    world.no_more_events(id).await;
}

#[tokio::test]
async fn test_works_on_a_profile_that_is_not_saved_and_reports_what_it_found() {
    let world = world(Vec::new());
    let error = ConnectError::PathNotFound {
        path: "/nope".to_owned(),
    };
    *world.mock.test_result.lock().unwrap() = Err(error.clone());

    let result = world.manager.test(&named("New"), Vec::new()).await;

    assert_eq!(result, Err(error));
    assert!(world.in_the_file().profiles.is_empty());
}

#[tokio::test]
async fn test_works_on_a_profile_in_use_and_from_a_read_only_file() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world_from(newer_file(vec![profile.clone()]));
    world.mounted(id).await;

    assert!(world.manager.test(&profile, Vec::new()).await.is_ok());

    assert_eq!(world.state(id), MountState::Mounted);
}

// Events.

#[tokio::test]
async fn every_listener_hears_every_transition() {
    let profile = named("Production");
    let id = profile.id;
    let mut world = world(vec![profile]);
    let mut other = world.manager.subscribe();

    world.mounted(id).await;
    world.unmounted(id).await;

    for expected in [
        MountState::Connecting,
        MountState::Mounted,
        MountState::Unmounting,
        MountState::Disconnected,
    ] {
        assert_eq!(other.recv().await.unwrap().state, expected);
    }
    assert!(other.try_recv().is_err());
}

#[tokio::test]
async fn a_refused_mount_sends_no_event() {
    let mut profile = named("Production");
    sftp_mut(&mut profile).keepalive_secs = 9999;
    let id = profile.id;
    let mut world = world(vec![profile]);

    world.manager.mount(id).await.unwrap_err();

    assert!(matches!(
        world.events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

/// The box says what is there, so one secret failing must not tick it because of another.
#[tokio::test]
async fn remember_is_only_ticked_for_a_secret_that_was_stored() {
    /// Stores jump passwords only.
    #[derive(Default)]
    struct OnlyJump(MemorySecretStore);

    impl SecretStore for OnlyJump {
        fn store(&self, p: Uuid, s: SecretSlot, v: &Secret<String>) -> io::Result<()> {
            if s != SecretSlot::JumpPassword {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            self.0.store(p, s, v)
        }
        fn load(&self, p: Uuid, s: SecretSlot) -> io::Result<Option<Secret<String>>> {
            self.0.load(p, s)
        }
        fn delete(&self, p: Uuid, s: SecretSlot) -> io::Result<bool> {
            self.0.delete(p, s)
        }
    }

    let mut profile = with_auth("Production", SftpAuth::Password { remember: false });
    sftp_mut(&mut profile).jump = Some(JumpHost {
        host: "bastion".to_owned(),
        port: 22,
        username: None,
        auth: JumpAuth::Password,
    });
    let id = profile.id;
    let folder = TempDir::new().unwrap();
    let mut store = ConfigStore::open(path_in(&folder));
    let config = Config {
        profiles: vec![profile],
        ..Config::default()
    };
    store.save(&config).unwrap();
    let mock = Arc::new(Mock::default());
    *mock.to_remember.lock().unwrap() = vec![
        (SecretSlot::Password, "typed".to_owned()),
        (SecretSlot::JumpPassword, "jump".to_owned()),
    ];
    let secrets = Arc::new(OnlyJump::default());
    let manager = MountManager::new(ManagerParts {
        store,
        factory: Arc::new(MockFactory(Arc::clone(&mock))),
        secrets: Arc::clone(&secrets) as _,
        letters: Arc::new(Letters::default()),
        prompter: Arc::new(NoPrompts),
    });
    let mut events = manager.subscribe();

    manager.mount(id).await.unwrap();
    let mut state = MountState::Connecting;
    while state != MountState::Mounted {
        state = timeout(PATIENCE, events.recv())
            .await
            .unwrap()
            .unwrap()
            .state;
    }

    assert_eq!(secrets.0.len(), 1);
    let config = manager.config().await;
    let Protocol::Sftp(saved) = &config.profiles[0].protocol;
    assert_eq!(saved.auth, SftpAuth::Password { remember: false });
}

/// Two entries can claim one id, and the secrets under it may be the other's.
#[tokio::test]
async fn deleting_one_of_two_unreadable_entries_with_an_id_leaves_the_secrets() {
    let id = Uuid::new_v4();
    let folder = with_unreadable(vec![named("Other")], id);
    let mut text = fs::read_to_string(path_in(&folder)).unwrap();
    let second = UNREADABLE.replace("ID", &id.to_string());
    text.push_str(&second.replace("Broken", "Also broken"));
    fs::write(path_in(&folder), text).unwrap();
    let world = world_from(folder);
    world.secrets.put(id, SecretSlot::Password, "secret");
    let config = world.manager.config().await;
    assert_eq!(config.unreadable.len(), 2);

    let first = config.unreadable[0].clone();
    world.manager.delete_unreadable(first).await.unwrap();
    assert!(!world.secrets.inner.is_empty());

    let last = config.unreadable[1].clone();
    world.manager.delete_unreadable(last).await.unwrap();
    assert!(world.secrets.inner.is_empty(), "the last one takes them");
    assert!(world.in_the_file().unreadable.is_empty());
}
