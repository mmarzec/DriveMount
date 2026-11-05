// SPDX-License-Identifier: GPL-3.0-or-later

//! What the mount manager drives: connecting, showing the drive and telling Explorer (spec §5.4).

use std::sync::Arc;

use fsm_vfs::{
    Connector, DriveLetter, Health, MountBackend, MountError, Prompter, RemoteFs, async_trait,
};
use fsm_win::shell;
use tokio::runtime::Handle;
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, spawn_blocking};
use tracing::{debug, info, warn};

use crate::volume::{Volume, VolumeConfig, fit_label};

/// The profile's drive: connects when mounted, and is gone again when unmounted.
///
/// The connection is made for each mount and dropped with it, so what it has remembered goes too
/// (NFR-SEC-03).
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

    /// Follows the suspect signal: each rise is worth asking whether the connection still answers.
    /// Reconnecting is M2-01's, which also adds keep-alive misses.
    fn watch_health(&self, mut suspect: watch::Receiver<u64>) -> JoinHandle<()> {
        let connector = Arc::clone(&self.connector);
        let health = self.health.clone();
        tokio::spawn(async move {
            while suspect.changed().await.is_ok() {
                let state = match connector.probe().await {
                    Ok(round_trip) => {
                        debug!(?round_trip, "the connection still answers");
                        Health::Ok
                    }
                    Err(error) => {
                        warn!(%error, "the connection does not answer");
                        Health::Lost(error.to_string())
                    }
                };
                health.send_if_modified(|current| {
                    let changed = *current != state;
                    current.clone_from(&state);
                    changed
                });
            }
        })
    }
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
        let watcher = self.watch_health(visible.volume.suspect());
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

        let visible = spawn_blocking(move || {
            visible.hide();
            visible
        })
        .await
        .map_err(|error| MountError::Other(format!("stopping the drive: {error}")))?;
        visible.volume.close_left_open().await;
        info!(slug = %self.config.slug, "unmounted");
        Ok(())
    }

    fn health(&self) -> watch::Receiver<Health> {
        self.health.subscribe()
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
