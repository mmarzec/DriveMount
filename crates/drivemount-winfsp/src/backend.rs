// SPDX-License-Identifier: GPL-3.0-or-later

//! What the mount manager drives: connecting, showing the drive and telling Explorer (spec §5.4),
//! and noticing a lost connection and connecting again under the same drive (FR-REC).

use std::sync::Arc;

use drivemount_vfs::{
    Connector, DriveLetter, Health, MountBackend, MountError, Prompter, RemoteFs, SessionFs,
    async_trait,
};
use drivemount_win::shell;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, spawn_blocking};
use tracing::{debug, info, warn};

use crate::volume::{Volume, VolumeConfig, fit_label};

/// The profile's drive: connects when mounted, and is gone again when unmounted.
///
/// The connection is made for each mount and dropped with it, so what it has remembered goes too
/// (NFR-SEC-03). The connector stays for the mount, so a [reconnect](MountBackend::reconnect)
/// asks nothing that was answered at the mount.
///
/// [`health`](MountBackend::health) goes to `Lost` on any of FR-REC-01's triggers: the connection
/// ending by itself (keep-alives unanswered, or the transport failing), a call finding it gone, or a
/// timed-out call followed by a probe that fails.
pub struct WinFspBackend<C> {
    connector: Arc<C>,
    config: VolumeConfig,
    mounted: Mutex<Option<Mounted>>,
    health: watch::Sender<Health>,
}

/// What a mounted drive holds.
struct Mounted {
    visible: VisibleDrive,
    watcher: JoinHandle<()>,
}

impl<C: Connector> WinFspBackend<C> {
    /// A backend for one profile, not connected yet.
    #[must_use]
    pub fn new(connector: C, config: VolumeConfig) -> Self {
        Self {
            connector: Arc::new(connector),
            config,
            mounted: Mutex::new(None),
            health: watch::Sender::new(Health::Ok),
        }
    }

    /// Watches each session in turn until it is lost, then says so. One loss is one `Lost`: the
    /// next is looked for once a reconnect has put a new session in.
    fn watch_health(
        &self,
        sessions: Arc<SessionFs>,
        mut suspect: watch::Receiver<u64>,
    ) -> JoinHandle<()> {
        let connector = Arc::clone(&self.connector);
        let health = self.health.clone();
        tokio::spawn(async move {
            while let Some(generation) = sessions.up().await {
                // Timeouts while there was no session say nothing about this one.
                suspect.borrow_and_update();
                let reason = tokio::select! {
                    lost = sessions.lost(generation) => match lost {
                        Some(reason) => reason,
                        // Replaced or shut down: nothing was lost.
                        None => continue,
                    },
                    () = connector.session_ended() => "the connection ended".to_owned(),
                    reason = unanswered_probe(&*connector, &mut suspect) => reason,
                };
                // A session that was replaced meanwhile is no loss.
                if sessions.mark_lost(generation, reason.clone()) || sessions.current().is_none() {
                    warn!(%reason, generation, "the connection is lost");
                    health.send_replace(Health::Lost(reason));
                }
            }
        })
    }
}

/// Asks whether the connection still answers at each rise of the suspect signal (spec §5.6), and
/// gives why once it does not.
async fn unanswered_probe<C: Connector>(
    connector: &C,
    suspect: &mut watch::Receiver<u64>,
) -> String {
    while suspect.changed().await.is_ok() {
        match connector.probe().await {
            Ok(round_trip) => debug!(?round_trip, "the connection still answers"),
            Err(error) => return format!("a keep-alive probe failed: {error}"),
        }
    }
    std::future::pending().await
}

#[async_trait]
impl<C: Connector> MountBackend for WinFspBackend<C> {
    /// Dropping this future cancels the mount (FR-MNT-11), at any point: what has been set up by
    /// then goes away with it.
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError> {
        let mut mounted = self.mounted.lock().await;
        if mounted.is_some() {
            return Err(MountError::Other("the drive is already mounted".to_owned()));
        }

        let fs = self.connector.connect(prompter).await?;
        let config = self.config.clone();
        let runtime = Handle::current();
        // The drive's output goes unread if this future is dropped, which takes the drive away again.
        let visible = spawn_blocking(move || VisibleDrive::show(fs, config, runtime, letter))
            .await
            .map_err(|error| MountError::Other(format!("starting the drive: {error}")))??;

        self.health.send_replace(Health::Ok);
        let sessions = Arc::clone(visible.volume.sessions());
        let watcher = self.watch_health(sessions, visible.volume.suspect());
        *mounted = Some(Mounted { visible, watcher });
        info!(%letter, slug = %self.config.slug, "mounted");
        Ok(())
    }

    /// Never held up by files that programs have open: their handles stop working (FR-MNT-10). What
    /// cannot be flushed by then goes to the log.
    async fn unmount(&self) -> Result<(), MountError> {
        let Some(Mounted { visible, watcher }) = self.mounted.lock().await.take() else {
            return Ok(());
        };
        watcher.abort();
        let sessions = Arc::clone(visible.volume.sessions());
        // Without a connection there is nothing to flush, and calls waiting for one would hold up
        // stopping the drive for a timeout each.
        if sessions.current().is_none() {
            sessions.shut_down();
        }

        let visible = spawn_blocking(move || {
            visible.hide();
            visible
        })
        .await
        .map_err(|error| MountError::Other(format!("stopping the drive: {error}")))?;
        visible.volume.close_left_open().await;
        sessions.shut_down();
        info!(slug = %self.config.slug, "unmounted");
        Ok(())
    }

    /// Gets a new session from the same connector, which remembers what was typed at the mount, and
    /// puts it under the drive.
    async fn reconnect(&self, prompter: &dyn Prompter) -> Result<(), MountError> {
        let mounted = self.mounted.lock().await;
        let Some(mounted) = mounted.as_ref() else {
            return Err(MountError::Other("the drive is not mounted".to_owned()));
        };
        let fs = self.connector.connect(prompter).await?;
        let generation = mounted.visible.volume.sessions().replace(fs);
        let generation =
            generation.ok_or_else(|| MountError::Other("the drive is stopping".to_owned()))?;
        self.health.send_replace(Health::Ok);
        info!(slug = %self.config.slug, generation, "reconnected");
        Ok(())
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.health.subscribe()
    }

    /// The watcher probes at each rise of the suspect signal, and says `Lost` if the probe fails. A
    /// drive being mounted, unmounted or reconnected is not checked: each of those finds out anyway.
    async fn check_connection(&self) {
        let Ok(mounted) = self.mounted.try_lock() else {
            return;
        };
        if let Some(mounted) = mounted.as_ref() {
            debug!(slug = %self.config.slug, "checking the connection");
            mounted.visible.volume.mark_suspect();
        }
    }
}

/// A drive that Explorer knows about, until it is hidden or dropped. Everything here blocks, for
/// callbacks in progress and for Explorer, so it stays off async worker threads.
struct VisibleDrive {
    volume: Volume,
    letter: DriveLetter,
    slug: String,
    hidden: std::sync::Once,
}

impl VisibleDrive {
    fn show(
        fs: Arc<dyn RemoteFs>,
        config: VolumeConfig,
        runtime: Handle,
        letter: DriveLetter,
    ) -> Result<Self, MountError> {
        let slug = config.slug.clone();
        // Without the label the drive still works, under a name made from its UNC path.
        if let Err(error) = shell::set_drive_label(&slug, &fit_label(&config.label)) {
            warn!(%error, %slug, "could not set the name Explorer shows");
        }
        let volume = Volume::new(fs, config, runtime);
        if let Err(error) = volume.start(letter) {
            forget_label(&slug);
            return Err(error);
        }
        shell::notify_drive_added(letter);
        Ok(Self {
            volume,
            letter,
            slug,
            hidden: std::sync::Once::new(),
        })
    }

    fn hide(&self) {
        self.hidden.call_once(|| {
            self.volume.stop();
            shell::notify_drive_removed(self.letter);
            forget_label(&self.slug);
        });
    }
}

impl Drop for VisibleDrive {
    fn drop(&mut self) {
        self.hide();
    }
}

fn forget_label(slug: &str) {
    if let Err(error) = shell::forget_drive_label(slug) {
        warn!(%error, %slug, "could not remove the name Explorer shows");
    }
}
