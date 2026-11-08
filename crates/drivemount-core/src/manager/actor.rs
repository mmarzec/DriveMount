// SPDX-License-Identifier: GPL-3.0-or-later

//! One profile's state machine (spec §5.5), in a task of its own. Nothing a profile does holds up
//! another's: one can sit in a dialog for as long as the user likes.

use std::sync::{Arc, Mutex, PoisonError};

use drivemount_vfs::{ConnectError, Health, MountBackend, MountError, Secret, SecretSlot};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, info_span, warn};
use uuid::Uuid;

use super::letters::Allocation;
use super::{
    MountDetail, MountEvent, MountRefused, MountState, ProfileChangeError, Shared, change,
    may_mount_while_read_only,
};
use crate::config::{LetterConflict, Settings};
use crate::profile::{DriveChoice, Profile};

/// What the manager sends to a profile's task. Cancel is not here: it has to get through at once, so
/// it is the token in [`ActorHandle`](super::ActorHandle).
pub(super) enum Message {
    Mount {
        reply: oneshot::Sender<Result<(), MountRefused>>,
    },
    Unmount,
    Save {
        profile: Box<Profile>,
        secrets: Vec<(SecretSlot, Secret<String>)>,
        reply: oneshot::Sender<Result<(), ProfileChangeError>>,
    },
    Delete {
        reply: oneshot::Sender<Result<(), ProfileChangeError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A mount that is up. Dropping it is what clears what the backend remembers (NFR-SEC-03) and frees
/// the letter.
struct Connected {
    backend: Box<dyn MountBackend>,
    allocation: Allocation,
}

/// Why a profile in use is to stop being in use.
enum Leaving {
    Unmount,
    Delete(oneshot::Sender<Result<(), ProfileChangeError>>),
    Shutdown(oneshot::Sender<()>),
    ManagerGone,
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

pub(super) struct Actor {
    id: Uuid,
    shared: Arc<Shared>,
    inbox: mpsc::Receiver<Message>,
    status: watch::Sender<MountEvent>,
    cancel: Arc<Mutex<CancellationToken>>,
}

impl Actor {
    pub(super) const fn new(
        id: Uuid,
        shared: Arc<Shared>,
        inbox: mpsc::Receiver<Message>,
        status: watch::Sender<MountEvent>,
        cancel: Arc<Mutex<CancellationToken>>,
    ) -> Self {
        Self {
            id,
            shared,
            inbox,
            status,
            cancel,
        }
    }

    /// *Disconnected* or *Error*: only Mount changes the state here.
    pub(super) async fn run(mut self) {
        while let Some(message) = self.inbox.recv().await {
            let flow = match message {
                Message::Mount { reply } => self.mount(reply).await,
                Message::Unmount => Flow::Continue,
                Message::Save {
                    profile,
                    secrets,
                    reply,
                } => {
                    let saved = self.save(profile, secrets, false).await;
                    answer(reply, saved);
                    Flow::Continue
                }
                Message::Delete { reply } => self.delete(reply).await,
                Message::Shutdown { reply } => {
                    answer(reply, ());
                    Flow::Exit
                }
            };
            if flow == Flow::Exit {
                return;
            }
        }
    }

    /// Every transition goes through here (spec §5.5).
    fn set(&self, state: MountState, detail: Option<MountDetail>) {
        info!(id = %self.id, ?state, ?detail, "mount state");
        let event = MountEvent {
            id: self.id,
            state,
            detail,
        };
        self.status.send_replace(event.clone());
        // Nobody listening is fine: the state can still be asked for.
        let _ = self.shared.events.send(event);
    }

    async fn mount(&mut self, reply: oneshot::Sender<Result<(), MountRefused>>) -> Flow {
        let (profile, settings) = match self.mountable().await {
            Ok(found) => found,
            Err(refused) => {
                answer(reply, Err(refused));
                return Flow::Continue;
            }
        };
        // NFR-MAINT-04. It lasts as long as the mount does, so all its log lines say whose they are.
        let span = info_span!("mount", profile_id = %self.id, name = %profile.name);
        self.mount_found(profile, settings, reply)
            .instrument(span)
            .await
    }

    async fn mount_found(
        &mut self,
        profile: Profile,
        settings: Settings,
        reply: oneshot::Sender<Result<(), MountRefused>>,
    ) -> Flow {
        let token = CancellationToken::new();
        *self.cancel.lock().unwrap_or_else(PoisonError::into_inner) = token.clone();
        self.set(MountState::Connecting, None);
        answer(reply, Ok(()));

        let connecting = connect(Arc::clone(&self.shared), profile, settings);
        let finished = {
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => break Err(Leaving::Unmount),
                    connected = &mut connecting => break Ok(connected),
                    message = self.inbox.recv() => {
                        if let Some(leaving) = self.while_in_use(message).await {
                            break Err(leaving);
                        }
                    }
                }
            }
            // Leaving the block drops a mount still in progress, which cancels it (FR-MNT-11), and
            // takes what it set up and what it remembered with it, before anyone hears of it.
        };

        match finished {
            Err(leaving) => {
                self.set(MountState::Disconnected, None);
                self.after_use(leaving).await
            }
            Ok(Err(MountError::Connect(ConnectError::Cancelled))) => {
                self.set(MountState::Disconnected, None);
                Flow::Continue
            }
            Ok(Err(error)) => {
                warn!(id = %self.id, %error, "mount failed");
                self.set(MountState::Error(error), None);
                Flow::Continue
            }
            Ok(Ok(connected)) => {
                self.keep_remembered(connected.backend.as_ref()).await;
                let detail = MountDetail {
                    letter: connected.allocation.letter(),
                    instead_of: connected.allocation.instead_of(),
                };
                self.set(MountState::Mounted, Some(detail));
                self.mounted(connected, detail).await
            }
        }
    }

    /// What a mount needs to start, or why it may not (M1-14's list).
    async fn mountable(&self) -> Result<(Profile, Settings), MountRefused> {
        let store = self.shared.store.lock().await;
        if let Some(reason) = store.read_only()
            && !may_mount_while_read_only(reason)
        {
            return Err(MountRefused::ReadOnly(reason.clone()));
        }
        let config = store.config();
        let profile = config.profiles.iter().find(|p| p.id == self.id);
        let profile = profile.ok_or(MountRefused::UnknownProfile)?;
        let invalid = config.invalid_profiles();
        if let Some((_, errors)) = invalid.into_iter().find(|(id, _)| *id == self.id) {
            return Err(MountRefused::Invalid(errors));
        }
        Ok((profile.clone(), config.settings.clone()))
    }

    /// *Mounted*: until something ends it.
    async fn mounted(&mut self, connected: Connected, detail: MountDetail) -> Flow {
        let mut health = connected.backend.health();
        let mut health_open = true;
        let leaving = loop {
            if health_open && matches!(*health.borrow_and_update(), Health::Lost(_)) {
                let auto_reconnect = self.auto_reconnect().await;
                if auto_reconnect {
                    // Reconnecting is M2-01's. Until then the drive stays, and calls fail.
                    warn!(id = %self.id, "the connection is lost");
                } else {
                    return self.lost(connected).await;
                }
            }
            tokio::select! {
                message = self.inbox.recv() => {
                    if let Some(leaving) = self.while_in_use(message).await {
                        break leaving;
                    }
                }
                changed = health.changed(), if health_open => {
                    health_open = changed.is_ok();
                }
            }
        };

        self.set(MountState::Unmounting, Some(detail));
        let unmounted = connected.backend.unmount().await;
        drop(connected);
        match unmounted {
            Ok(()) => self.set(MountState::Disconnected, None),
            Err(error) => {
                warn!(id = %self.id, %error, "unmount failed");
                self.set(MountState::Error(error), None);
            }
        }
        self.after_use(leaving).await
    }

    /// The flag can change while mounted (FR-PROF-08), so this is the file's value now. A profile
    /// that is not there has nobody left to want a reconnect.
    async fn auto_reconnect(&self) -> bool {
        let store = self.shared.store.lock().await;
        let mut profiles = store.config().profiles.iter();
        profiles
            .find(|profile| profile.id == self.id)
            .is_some_and(|profile| profile.auto_reconnect)
    }

    /// *Mounted* to *Error (Connection lost)*, with no reconnect to wait for (FR-REC-07).
    async fn lost(&self, connected: Connected) -> Flow {
        if let Err(error) = connected.backend.unmount().await {
            warn!(id = %self.id, %error, "unmount failed after losing the connection");
        }
        drop(connected);
        let lost = MountError::Connect(ConnectError::ConnectionLost);
        self.set(MountState::Error(lost), None);
        Flow::Continue
    }

    /// A message for a profile that is connecting or mounted. `Some` means it has to stop.
    async fn while_in_use(&self, message: Option<Message>) -> Option<Leaving> {
        let Some(message) = message else {
            return Some(Leaving::ManagerGone);
        };
        match message {
            Message::Mount { reply } => {
                answer(reply, Err(MountRefused::Busy));
                None
            }
            Message::Unmount => Some(Leaving::Unmount),
            Message::Save {
                profile,
                secrets,
                reply,
            } => {
                let saved = self.save(profile, secrets, true).await;
                answer(reply, saved);
                None
            }
            Message::Delete { reply } => {
                // A delete that cannot happen must not unmount anything first.
                let refused = self.shared.store.lock().await.read_only().cloned();
                match refused {
                    Some(reason) => {
                        answer(reply, Err(ProfileChangeError::ReadOnly(reason)));
                        None
                    }
                    None => Some(Leaving::Delete(reply)),
                }
            }
            Message::Shutdown { reply } => Some(Leaving::Shutdown(reply)),
        }
    }

    /// Back at *Disconnected* or *Error*, with whatever came next done.
    async fn after_use(&mut self, leaving: Leaving) -> Flow {
        match leaving {
            Leaving::Unmount => Flow::Continue,
            Leaving::Delete(reply) => self.delete(reply).await,
            Leaving::Shutdown(reply) => {
                answer(reply, ());
                Flow::Exit
            }
            Leaving::ManagerGone => Flow::Exit,
        }
    }

    async fn save(
        &self,
        profile: Box<Profile>,
        secrets: Vec<(SecretSlot, Secret<String>)>,
        locked: bool,
    ) -> Result<(), ProfileChangeError> {
        let shared = Arc::clone(&self.shared);
        let saved = spawn_blocking(move || change::save(&shared, &profile, &secrets, locked)).await;
        saved.map_err(|error| ProfileChangeError::Failed(error.to_string()))?
    }

    async fn delete(&self, reply: oneshot::Sender<Result<(), ProfileChangeError>>) -> Flow {
        let shared = Arc::clone(&self.shared);
        let id = self.id;
        let deleted = spawn_blocking(move || change::delete(&shared, id)).await;
        let deleted = deleted.map_err(|error| ProfileChangeError::Failed(error.to_string()));
        let deleted = deleted.and_then(|deleted| deleted);
        let flow = if deleted.is_ok() {
            Flow::Exit
        } else {
            Flow::Continue
        };
        answer(reply, deleted);
        flow
    }

    async fn keep_remembered(&self, backend: &dyn MountBackend) {
        let remembered = backend.take_remembered();
        if remembered.is_empty() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let id = self.id;
        let kept = spawn_blocking(move || change::keep_remembered(&shared, id, &remembered)).await;
        if let Err(error) = kept {
            warn!(%id, %error, "could not remember a secret");
        }
    }
}

/// Everything from choosing a letter to the drive being up. It owns all it uses, so that dropping it,
/// at any point, cancels the mount.
async fn connect(
    shared: Arc<Shared>,
    profile: Profile,
    settings: Settings,
) -> Result<Connected, MountError> {
    let conflict = settings.letter_conflict;
    let mut allocation = allocate(&shared, profile.drive, conflict, Vec::new()).await?;
    let backend = create_backend(&shared, &profile, &settings).await?;
    let mut tried = Vec::new();
    loop {
        let letter = allocation.letter();
        match backend.mount(letter, &*shared.prompter).await {
            Ok(()) => {
                return Ok(Connected {
                    backend,
                    allocation,
                });
            }
            // Something took the letter while connecting. The allocator goes by the setting, as
            // before. The backend remembers what the user typed, so going round again asks nothing.
            Err(MountError::LetterTaken(taken)) => {
                tried.push(taken);
                allocation = allocate(&shared, profile.drive, conflict, tried.clone()).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn create_backend(
    shared: &Arc<Shared>,
    profile: &Profile,
    settings: &Settings,
) -> Result<Box<dyn MountBackend>, MountError> {
    let factory = Arc::clone(&shared.factory);
    let (profile, settings) = (profile.clone(), settings.clone());
    spawn_blocking(move || factory.create(&profile, &settings))
        .await
        .map_err(|error| MountError::Other(format!("creating the backend: {error}")))?
}

async fn allocate(
    shared: &Arc<Shared>,
    choice: DriveChoice,
    conflict: LetterConflict,
    tried: Vec<drivemount_vfs::DriveLetter>,
) -> Result<Allocation, MountError> {
    let shared = Arc::clone(shared);
    // If this is cancelled, the allocation goes unread, and frees its letter when it is dropped.
    spawn_blocking(move || shared.allocator.allocate(choice, conflict, &tried))
        .await
        .map_err(|error| MountError::Other(format!("choosing a letter: {error}")))?
}

/// The caller may have stopped waiting, which is its own business.
fn answer<T>(reply: oneshot::Sender<T>, value: T) {
    let _ = reply.send(value);
}
