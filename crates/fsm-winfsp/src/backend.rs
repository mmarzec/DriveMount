// SPDX-License-Identifier: GPL-3.0-or-later

//! Starting and stopping a drive.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use fsm_vfs::{CachedFs, DeadlineFs, DriveLetter, MountError, RemoteFs};
use tokio::runtime::Handle;
use tokio::sync::watch;
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};

use crate::adapter::{Adapter, Deadline, Stack};
use crate::{attrs, security};

/// Volume labels are limited to this many UTF-16 units (spec §5.8).
const MAX_LABEL: usize = 32;

/// What Windows caches for: short, and never for ever (spec §5.6, spike M0-1).
const FILE_INFO_TIMEOUT_MS: u32 = 1000;

const SECTOR_SIZE: u16 = 4096;
const SECTORS_PER_ALLOCATION_UNIT: u16 = 1;
const _: () =
    assert!(SECTOR_SIZE as u64 * SECTORS_PER_ALLOCATION_UNIT as u64 == attrs::ALLOCATION_UNIT);

const MAX_COMPONENT_LENGTH: u16 = 255;

/// What a drive looks like and how it behaves.
#[derive(Debug, Clone)]
pub struct VolumeConfig {
    /// The last part of the volume prefix `\fsm\<slug>`, which makes this a network drive.
    pub slug: String,
    /// The volume label, cut to fit. Usually the profile's name.
    pub label: String,
    /// The file system's name, `FSM-SFTP` or `FSM-NFS`.
    pub file_system_name: String,
    /// The limit for each callback, and for each remote call within it (`op_timeout_secs`).
    pub op_timeout: Duration,
    /// How long attributes and listings are remembered (`cache_ttl_secs`).
    pub cache_ttl: Duration,
    /// Dispatcher threads. 0 is WinFsp's default.
    pub threads: u32,
    /// Refuse every change with "media is write protected" (FR-MNT-07).
    pub read_only: bool,
}

/// A [`RemoteFs`] shown as a drive, through `CachedFs<DeadlineFs<…>>`.
///
/// `MountBackend` comes with M1-12c.
pub struct WinFspBackend {
    config: VolumeConfig,
    fs: Arc<Stack>,
    deadline: Arc<Deadline>,
    runtime: Handle,
    host: Mutex<Option<FileSystemHost<Adapter, FineGuard>>>,
}

impl WinFspBackend {
    /// A backend for `fs`, not yet on any drive. Callbacks run their remote calls on `runtime`, which
    /// has to outlive the drive.
    #[must_use]
    pub fn new(fs: Arc<dyn RemoteFs>, config: VolumeConfig, runtime: Handle) -> Self {
        let deadline = Arc::new(DeadlineFs::new(fs, config.op_timeout));
        let fs = Arc::new(CachedFs::new(Arc::clone(&deadline), config.cache_ttl));
        Self {
            config,
            fs,
            deadline,
            runtime,
            host: Mutex::new(None),
        }
    }

    /// The "suspect" signal of spec §5.6: how many calls have timed out so far, including callbacks
    /// that ran out of time between remote calls.
    #[must_use]
    pub fn suspect(&self) -> watch::Receiver<u64> {
        self.deadline.suspect()
    }

    /// Makes the drive appear on `letter`.
    ///
    /// # Errors
    ///
    /// [`MountError::WinFspMissing`] if WinFsp cannot be loaded. [`MountError::Other`] if the drive
    /// cannot start, or is already on a letter.
    pub fn start(&self, letter: DriveLetter) -> Result<(), MountError> {
        let mut slot = self.host();
        if slot.is_some() {
            return Err(MountError::Other("the drive is already mounted".to_owned()));
        }
        winfsp::winfsp_init().map_err(|_| MountError::WinFspMissing)?;

        let adapter = Adapter {
            fs: Arc::clone(&self.fs),
            deadline: Arc::clone(&self.deadline),
            op_timeout: self.config.op_timeout,
            read_only: self.config.read_only,
            runtime: self.runtime.clone(),
            label: fit_label(&self.config.label),
            descriptor: security::current_user_descriptor().map_err(other("security"))?,
            volume: Mutex::new(None),
            open_files: Mutex::new(Vec::new()),
        };
        let options = FileSystemParams {
            use_dir_info_by_name: true,
            volume_params: volume_params(&self.config),
            debug_mode: Default::default(),
        };
        let mut host = FileSystemHost::<Adapter, FineGuard>::new_with_options(options, adapter)
            .map_err(other("create"))?;
        host.mount(letter.to_string()).map_err(other("mount"))?;
        host.start_with_threads(self.config.threads)
            .map_err(other("start"))?;
        *slot = Some(host);
        Ok(())
    }

    /// Removes the drive. Does nothing if it is not there. Waits for callbacks in progress, which
    /// are limited by the operation timeout.
    pub fn stop(&self) {
        let host = self.host().take();
        drop(host);
    }

    /// Nothing panics while holding this lock in a way that matters: the host is either there or not.
    fn host(&self) -> MutexGuard<'_, Option<FileSystemHost<Adapter, FineGuard>>> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn other<E: std::fmt::Display>(step: &'static str) -> impl FnOnce(E) -> MountError {
    move |error| MountError::Other(format!("WinFsp {step}: {error}"))
}

/// The volume parameters of spec §5.8.
fn volume_params(config: &VolumeConfig) -> VolumeParams {
    let mut params = VolumeParams::new();
    params
        .sector_size(SECTOR_SIZE)
        .sectors_per_allocation_unit(SECTORS_PER_ALLOCATION_UNIT)
        .max_component_length(MAX_COMPONENT_LENGTH)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .read_only_volume(config.read_only)
        .post_cleanup_when_modified_only(true)
        .pass_query_directory_filename(true)
        .file_info_timeout(FILE_INFO_TIMEOUT_MS)
        .volume_creation_time(attrs::filetime(Some(SystemTime::now())))
        .volume_serial_number(serial_number(&config.slug))
        .filesystem_name(&config.file_system_name)
        .prefix(format!(r"\fsm\{}", config.slug));
    params
}

/// A serial number that stays the same from one mount to the next (spec §5.8).
fn serial_number(slug: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    slug.hash(&mut hasher);
    // Keeps the low half of the hash on purpose.
    hasher.finish() as u32
}

/// The longest start of `label` that fits, without cutting a character in two.
fn fit_label(label: &str) -> String {
    let mut used = 0;
    label
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= MAX_LABEL
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_label_is_kept() {
        assert_eq!(fit_label("Production"), "Production");
        assert_eq!(fit_label(""), "");
    }

    #[test]
    fn a_long_label_is_cut_to_32_units() {
        let exact = "x".repeat(32);
        assert_eq!(fit_label(&exact), exact);
        assert_eq!(fit_label(&"x".repeat(33)), exact);
        assert_eq!(fit_label(&"ż".repeat(33)), "ż".repeat(32));
    }

    #[test]
    fn a_label_is_not_cut_inside_a_character() {
        // 😀 takes two UTF-16 units, and would end on unit 33.
        let label = format!("{}😀", "x".repeat(31));
        assert_eq!(fit_label(&label), "x".repeat(31));
    }

    #[test]
    fn the_serial_number_depends_on_the_slug_alone() {
        assert_eq!(serial_number("web"), serial_number("web"));
        assert_ne!(serial_number("web"), serial_number("db"));
    }
}
