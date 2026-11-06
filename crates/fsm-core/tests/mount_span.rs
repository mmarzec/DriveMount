// SPDX-License-Identifier: GPL-3.0-or-later

//! NFR-MAINT-04: each mount is a span carrying the profile's id and name. This is a process of its own
//! with one test, as a scoped `tracing` subscriber loses events to other threads: a log line first
//! reached on a thread with no subscriber is remembered as wanted by nobody.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fsm_core::config::{Config, ConfigStore, Settings};
use fsm_core::manager::{
    BackendFactory, DriveLetters, ManagerParts, MountEvent, MountManager, MountState,
};
use fsm_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use fsm_vfs::{
    ConnectError, DriveLetter, Health, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer,
    LoginRequest, MemorySecretStore, MountBackend, MountError, Prompter, Remember, Secret,
    SecretRequest, SecretSlot, async_trait,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, watch};

struct Drive(watch::Sender<Health>);

#[async_trait]
impl MountBackend for Drive {
    async fn mount(&self, _: DriveLetter, _: &dyn Prompter) -> Result<(), MountError> {
        Ok(())
    }

    async fn unmount(&self) -> Result<(), MountError> {
        Ok(())
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.0.subscribe()
    }
}

struct Factory;

#[async_trait]
impl BackendFactory for Factory {
    fn create(&self, _: &Profile, _: &Settings) -> Result<Box<dyn MountBackend>, MountError> {
        Ok(Box::new(Drive(watch::channel(Health::Ok).0)))
    }

    async fn test(
        &self,
        _: &Profile,
        _: &Settings,
        _: Vec<(SecretSlot, Secret<String>)>,
        _: &dyn Prompter,
    ) -> Result<Duration, ConnectError> {
        Ok(Duration::ZERO)
    }
}

struct AllFree;

impl DriveLetters for AllFree {
    fn is_free(&self, _: DriveLetter) -> io::Result<bool> {
        Ok(true)
    }
}

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

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn until(events: &mut broadcast::Receiver<MountEvent>, wanted: MountState) {
    while events.recv().await.unwrap().state != wanted {}
}

/// `#[tokio::test]` has one thread, so the profile's task runs under this subscriber.
#[tokio::test]
async fn a_mount_is_a_span_with_the_id_and_name_of_its_profile() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _in_use = tracing::subscriber::set_default(subscriber);

    let folder = TempDir::new().unwrap();
    let sftp = SftpProfile::new("example.com", "deploy", SftpAuth::Agent);
    let profile = Profile::new("Span NAS", Protocol::Sftp(sftp));
    let id = profile.id;
    let mut store = ConfigStore::open(folder.path().join("config.toml"));
    store
        .save(&Config {
            profiles: vec![profile],
            ..Config::default()
        })
        .unwrap();
    let manager = MountManager::new(ManagerParts {
        store,
        factory: Arc::new(Factory),
        secrets: Arc::new(MemorySecretStore::new()),
        letters: Arc::new(AllFree),
        prompter: Arc::new(NoPrompts),
    });

    let mut events = manager.subscribe();
    manager.mount(id).await.unwrap();
    until(&mut events, MountState::Mounted).await;
    manager.unmount(id).await;
    until(&mut events, MountState::Disconnected).await;

    let bytes = captured.0.lock().unwrap().clone();
    let text = String::from_utf8(bytes).unwrap();
    let span = format!("mount{{profile_id={id} name=Span NAS}}");
    let states: Vec<&str> = text.lines().filter(|l| l.contains("mount state")).collect();
    assert_eq!(states.len(), 4, "Connecting to Disconnected: {text}");
    for line in states {
        assert!(line.contains(&span), "{line}");
    }
}
