// SPDX-License-Identifier: GPL-3.0-or-later

//! Mounting and unmounting profiles (spec §5.5), and saving and deleting them. Reconnecting is
//! M2-01's.
//!
//! Each profile has a task of its own, which owns its state. States come out as [`MountEvent`]s on a
//! broadcast channel, and [`status`](MountManager::status) gives the latest. What the commands
//! answer is whether they were accepted: a mount that was accepted and failed is an event.
//!
//! Backends come from a [`BackendFactory`] the app supplies (spec §5.2), one for each mount. A backend
//! holds what the user typed for as long as it lives, so one is never kept beyond its mount
//! (NFR-SEC-03).
//!
//! | Profile is… | Mount | Unmount | Cancel | Save | Delete |
//! |---|---|---|---|---|---|
//! | *Disconnected*, *Error* | connects | – | – | anything | deletes |
//! | *Connecting* | [`Busy`](MountRefused::Busy) | cancels | cancels | not connection fields | cancels, deletes |
//! | *Mounted* | [`Busy`](MountRefused::Busy) | unmounts | – | not connection fields | unmounts, deletes |
//! | *Unmounting* | waits, then connects | – | – | waits, then anything | waits, then deletes |

mod actor;
mod change;
mod letters;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use fsm_vfs::{
    ConnectError, DriveLetter, MountBackend, MountError, Prompter, Secret, SecretSlot, SecretStore,
    async_trait,
};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use self::actor::{Actor, Message};
use self::letters::Allocator;
pub use self::letters::DriveLetters;
use crate::config::{Config, ConfigStore, ReadOnlyReason, SaveError, Settings, UnreadableProfile};
use crate::profile::{FieldError, Profile};

/// Events a slow listener can fall behind by before it misses some. It can read
/// [`status`](MountManager::status) then.
const EVENTS: usize = 256;
/// Commands a profile can have waiting. They wait only while it finishes unmounting or saving.
const INBOX: usize = 16;

/// Whether profiles may be mounted from a file this store will not write. Save stays off either way.
///
/// **Decided at M1-15, and the owner has not confirmed it (`docs/PROGRESS.md`).** A newer version may
/// have changed what a field means, but the alternative is an app that does nothing until it is
/// upgraded. To reverse it, make `Newer` false: nothing else depends on this. The other reasons only
/// matter for a file damaged while running, whose profiles were read when it was still good. A file
/// that was damaged from the start has no profiles to mount.
const fn may_mount_while_read_only(reason: &ReadOnlyReason) -> bool {
    match reason {
        ReadOnlyReason::Newer { .. }
        | ReadOnlyReason::Damaged { .. }
        | ReadOnlyReason::CannotRead { .. }
        | ReadOnlyReason::MigrationFailed { .. } => true,
    }
}

/// Makes backends. The app has the only real one, as this crate knows no backend (spec §5.2).
#[async_trait]
pub trait BackendFactory: Send + Sync + 'static {
    /// A backend for one mount of `profile`, with what it needs from the [`SecretStore`] already read.
    /// It may block, and is never called on an async worker thread.
    ///
    /// # Errors
    /// The backend cannot exist on this system.
    fn create(
        &self,
        profile: &Profile,
        settings: &Settings,
    ) -> Result<Box<dyn MountBackend>, MountError>;

    /// Connects, logs in and checks the remote path, without mounting, and gives the round trip
    /// (FR-PROF-06). Dropping the future stops it.
    ///
    /// `typed` is what is in the dialog's boxes, which goes before anything stored. Nothing is stored
    /// by this, whatever the user ticks: the profile may never be saved.
    ///
    /// # Errors
    /// What stopped it. [`ConnectError::Cancelled`] if the user cancelled a prompt.
    async fn test(
        &self,
        profile: &Profile,
        settings: &Settings,
        typed: Vec<(SecretSlot, Secret<String>)>,
        prompter: &dyn Prompter,
    ) -> Result<Duration, ConnectError>;
}

/// A profile's state (spec §5.5, without *Reconnecting* until M2-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    /// Not mounted, and nothing went wrong.
    Disconnected,
    /// Choosing a letter and connecting, which may be waiting for the user.
    Connecting,
    /// The drive is up.
    Mounted,
    /// The drive is going away.
    Unmounting,
    /// Not mounted, because of this. The codes of spec §7.8 go by the variant.
    Error(MountError),
}

/// Where a drive is. There is one while *Mounted* and *Unmounting*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountDetail {
    /// The drive's letter.
    pub letter: DriveLetter,
    /// The profile's own letter, if that was taken (FR-MNT-04): "Y: (Z: was in use)". The event that
    /// has this first is the one to send a notification for.
    pub instead_of: Option<DriveLetter>,
}

/// A profile's new state, for the UI. Every transition sends one (spec §5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEvent {
    /// The profile.
    pub id: Uuid,
    /// Its new state.
    pub state: MountState,
    /// Where the drive is.
    pub detail: Option<MountDetail>,
}

/// Why a mount did not start.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MountRefused {
    /// There is no profile with this id.
    #[error("there is no such connection")]
    UnknownProfile,
    /// It is in [`Config::unreadable`], so there is no model of it to mount.
    #[error("this connection could not be read: {problem}")]
    Unreadable {
        /// What is wrong with it.
        problem: String,
    },
    /// It is in [`Config::invalid_profiles`]: the dialog should mark these.
    #[error("this connection needs correcting first")]
    Invalid(Vec<FieldError>),
    /// The configuration file does not allow it. Nothing gets this for now: see `docs/PROGRESS.md`.
    #[error(transparent)]
    ReadOnly(ReadOnlyReason),
    /// It is connecting or mounted already.
    #[error("this connection is already in use")]
    Busy,
}

/// Why Save or Delete did not do all it should have.
#[derive(Debug, Error)]
pub enum ProfileChangeError {
    /// There is no such profile.
    #[error("there is no such connection")]
    UnknownProfile,
    /// The configuration file cannot be written. Nothing was changed, or unmounted.
    #[error(transparent)]
    ReadOnly(ReadOnlyReason),
    /// FR-PROF-08's hint. Nothing was changed.
    #[error("Unmount to change")]
    ConnectionFieldsLocked,
    /// Spec §7.3. Nothing was changed.
    #[error("the connection has fields that need correcting")]
    Invalid(Vec<FieldError>),
    /// A secret that had to go could not be deleted. The file was not changed.
    #[error("the saved password could not be removed: {0}")]
    DeletingSecrets(#[source] io::Error),
    /// The file could not be written. Secrets the change had no use for are gone, and nothing else
    /// changed, so this can be tried again.
    #[error(transparent)]
    Save(SaveError),
    /// **The profile was saved,** but the password or passphrase from the dialog was not remembered.
    #[error("the connection was saved, but its password could not be remembered: {0}")]
    StoringSecret(#[source] io::Error),
    /// E-INTERNAL, with details for the log.
    #[error("{0}")]
    Failed(String),
}

/// What the manager is made of.
pub struct ManagerParts {
    /// `config.toml`. The manager is the one user of it from now on.
    pub store: ConfigStore,
    /// Makes the backends.
    pub factory: Arc<dyn BackendFactory>,
    /// Where remembered secrets go.
    pub secrets: Arc<dyn SecretStore>,
    /// What the system says about drive letters.
    pub letters: Arc<dyn DriveLetters>,
    /// Asks the user things on behalf of backends. It has to take one question at a time, as profiles
    /// mounting together all use it (FR-MNT-08).
    pub prompter: Arc<dyn Prompter>,
}

struct Shared {
    /// Held across saves, which take about 100 ms, so only by [`spawn_blocking`] tasks then.
    store: tokio::sync::Mutex<ConfigStore>,
    factory: Arc<dyn BackendFactory>,
    secrets: Arc<dyn SecretStore>,
    allocator: Allocator,
    prompter: Arc<dyn Prompter>,
    events: broadcast::Sender<MountEvent>,
}

/// How to reach a profile's task.
struct ActorHandle {
    inbox: mpsc::Sender<Message>,
    status: watch::Receiver<MountEvent>,
    /// The token of the mount in progress, or of one that is over, which cancelling does nothing to.
    cancel: Arc<Mutex<CancellationToken>>,
}

struct Inner {
    shared: Arc<Shared>,
    actors: Mutex<HashMap<Uuid, ActorHandle>>,
}

/// The engine's front. Cloning it gives another handle on the same manager. When the last one goes,
/// every drive is unmounted, as long as the runtime lives to do it. [`shutdown`](Self::shutdown) is
/// the way to wait for that.
#[derive(Clone)]
pub struct MountManager {
    inner: Arc<Inner>,
}

impl MountManager {
    /// Starts a task for each profile. Must run inside the Tokio runtime.
    #[must_use]
    pub fn new(parts: ManagerParts) -> Self {
        let ManagerParts {
            store,
            factory,
            secrets,
            letters,
            prompter,
        } = parts;
        let ids: Vec<Uuid> = store.config().profiles.iter().map(|p| p.id).collect();
        let shared = Arc::new(Shared {
            store: tokio::sync::Mutex::new(store),
            factory,
            secrets,
            allocator: Allocator::new(letters),
            prompter,
            events: broadcast::Sender::new(EVENTS),
        });
        let manager = Self {
            inner: Arc::new(Inner {
                shared,
                actors: Mutex::new(HashMap::new()),
            }),
        };
        {
            let mut actors = manager.actors();
            for id in ids {
                actors.insert(id, manager.start_actor(id));
            }
        }
        manager
    }

    fn actors(&self) -> MutexGuard<'_, HashMap<Uuid, ActorHandle>> {
        let actors = self.inner.actors.lock();
        actors.unwrap_or_else(PoisonError::into_inner)
    }

    fn start_actor(&self, id: Uuid) -> ActorHandle {
        let (inbox_sender, inbox) = mpsc::channel(INBOX);
        let (status_sender, status) = watch::channel(MountEvent {
            id,
            state: MountState::Disconnected,
            detail: None,
        });
        let cancel = Arc::new(Mutex::new(CancellationToken::new()));
        let actor = Actor::new(
            id,
            Arc::clone(&self.inner.shared),
            inbox,
            status_sender,
            Arc::clone(&cancel),
        );
        tokio::spawn(actor.run());
        ActorHandle {
            inbox: inbox_sender,
            status,
            cancel,
        }
    }

    fn inbox(&self, id: Uuid) -> Option<mpsc::Sender<Message>> {
        self.actors().get(&id).map(|actor| actor.inbox.clone())
    }

    /// Follows every profile's state. Subscribe before sending commands, to miss nothing.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<MountEvent> {
        self.inner.shared.events.subscribe()
    }

    /// The latest state of a profile.
    #[must_use]
    pub fn status(&self, id: Uuid) -> Option<MountEvent> {
        let actors = self.actors();
        let status = &actors.get(&id)?.status;
        Some(status.borrow().clone())
    }

    /// What is in the file now.
    pub async fn config(&self) -> Config {
        self.inner.shared.store.lock().await.config().clone()
    }

    /// Why Save and Delete are off, if they are, which the app has to show (NFR-REL-05).
    pub async fn read_only(&self) -> Option<ReadOnlyReason> {
        self.inner.shared.store.lock().await.read_only().cloned()
    }

    /// Starts a mount. This returns once it is under way, and how it goes is told by events.
    ///
    /// # Errors
    /// It did not start, and no event was sent.
    pub async fn mount(&self, id: Uuid) -> Result<(), MountRefused> {
        let Some(inbox) = self.inbox(id) else {
            return Err(self.why_there_is_no_task(id).await);
        };
        let (reply, answer) = oneshot::channel();
        if inbox.send(Message::Mount { reply }).await.is_err() {
            return Err(MountRefused::UnknownProfile);
        }
        answer.await.unwrap_or(Err(MountRefused::UnknownProfile))
    }

    async fn why_there_is_no_task(&self, id: Uuid) -> MountRefused {
        let store = self.inner.shared.store.lock().await;
        let mut unreadable = store.config().unreadable.iter();
        unreadable.find(|entry| entry.id == Some(id)).map_or(
            MountRefused::UnknownProfile,
            |entry| MountRefused::Unreadable {
                problem: entry.problem.clone(),
            },
        )
    }

    /// Unmounts a drive (FR-MNT-10), or cancels a mount in progress, which is what someone asking
    /// for everything to be unmounted means. For a profile doing neither, it does nothing.
    pub async fn unmount(&self, id: Uuid) {
        if let Some(inbox) = self.inbox(id) {
            // A closed inbox is a profile that has just been deleted.
            let _ = inbox.send(Message::Unmount).await;
        }
    }

    /// Cancels a mount in progress (FR-MNT-11), at once, including one waiting for a dialog. It goes
    /// back to *Disconnected*. For a profile not connecting, it does nothing.
    pub fn cancel(&self, id: Uuid) {
        if let Some(actor) = self.actors().get(&id) {
            actor
                .cancel
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .cancel();
        }
    }

    /// The Test connection button (FR-PROF-06). It has nothing to do with the profile's state, and
    /// the profile need not have been saved. Dropping the future stops it.
    ///
    /// # Errors
    /// What the test found, with [`ConnectError::Cancelled`] if the user cancelled a prompt.
    pub async fn test(
        &self,
        profile: &Profile,
        typed: Vec<(SecretSlot, Secret<String>)>,
    ) -> Result<Duration, ConnectError> {
        let shared = &self.inner.shared;
        let settings = shared.store.lock().await.config().settings.clone();
        (shared.factory)
            .test(profile, &settings, typed, &*shared.prompter)
            .await
    }

    /// The Save button: adds or replaces a profile, and checks it first (spec §7.3).
    ///
    /// `secrets` are the password and passphrase boxes of the dialog. One the profile does not
    /// remember is not stored. What the profile no longer has a use for is deleted (spec §6.2).
    ///
    /// # Errors
    /// See [`ProfileChangeError`].
    pub async fn save(
        &self,
        profile: Profile,
        secrets: Vec<(SecretSlot, Secret<String>)>,
    ) -> Result<(), ProfileChangeError> {
        let id = profile.id;
        let (inbox, is_new) = {
            let mut actors = self.actors();
            match actors.get(&id) {
                Some(actor) => (actor.inbox.clone(), false),
                None => {
                    let actor = self.start_actor(id);
                    let inbox = actor.inbox.clone();
                    actors.insert(id, actor);
                    (inbox, true)
                }
            }
        };
        let (reply, answer) = oneshot::channel();
        let message = Message::Save {
            profile: Box::new(profile),
            secrets,
            reply,
        };
        if inbox.send(message).await.is_err() {
            return Err(ProfileChangeError::UnknownProfile);
        }
        let saved = answer
            .await
            .unwrap_or(Err(ProfileChangeError::UnknownProfile));
        let is_in_the_file = matches!(saved, Ok(()) | Err(ProfileChangeError::StoringSecret(_)));
        if is_new && !is_in_the_file {
            self.actors().remove(&id);
        }
        saved
    }

    /// The Delete command: stops the profile using its drive, then deletes its secrets (FR-PROF-07)
    /// and removes it from the file. Asking the user first is for the caller.
    ///
    /// # Errors
    /// See [`ProfileChangeError`]. A read-only file is noticed before anything is stopped. After any
    /// other error the profile stays, and this can be tried again.
    pub async fn delete(&self, id: Uuid) -> Result<(), ProfileChangeError> {
        let inbox = self.inbox(id).ok_or(ProfileChangeError::UnknownProfile)?;
        let (reply, answer) = oneshot::channel();
        if inbox.send(Message::Delete { reply }).await.is_err() {
            return Err(ProfileChangeError::UnknownProfile);
        }
        let deleted = answer
            .await
            .unwrap_or(Err(ProfileChangeError::UnknownProfile));
        if deleted.is_ok() {
            self.actors().remove(&id);
        }
        deleted
    }

    /// Takes an entry of [`Config::unreadable`] out of the file, with its secrets, unless a profile
    /// that can be read uses the same id.
    ///
    /// # Errors
    /// See [`ProfileChangeError`].
    pub async fn delete_unreadable(
        &self,
        entry: UnreadableProfile,
    ) -> Result<(), ProfileChangeError> {
        let shared = Arc::clone(&self.inner.shared);
        spawn_blocking(move || change::delete_unreadable(&shared, &entry))
            .await
            .map_err(|error| ProfileChangeError::Failed(error.to_string()))?
    }

    /// App quit or sign-out: unmounts everything, and returns when that is done (spec §5.5). The
    /// manager does nothing after this.
    pub async fn shutdown(&self) {
        let actors: Vec<ActorHandle> = self.actors().drain().map(|(_, actor)| actor).collect();
        let mut stopped = Vec::with_capacity(actors.len());
        for actor in &actors {
            let (reply, answer) = oneshot::channel();
            if actor.inbox.send(Message::Shutdown { reply }).await.is_ok() {
                stopped.push(answer);
            }
        }
        for answer in stopped {
            // A task that is gone has nothing left to stop.
            let _ = answer.await;
        }
    }
}

#[cfg(test)]
mod tests;
