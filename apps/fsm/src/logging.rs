// SPDX-License-Identifier: GPL-3.0-or-later

//! The log (spec §6.4): `fsm.log`, then `fsm.log.1` up to `fsm.log.6`, rolled at midnight UTC and at
//! 5 MiB. `tracing-appender`'s own rolling has no size limit and dates the name of the live file, so
//! only its background writer is used: nothing here waits for a disk on the thread that logs
//! (NFR-PERF-10).
//!
//! Only this program's own crates log at the level the user chose. Everything else stays at Warn or
//! below, so that a library's debug output cannot put what it was handed into the file (NFR-SEC-02).

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fsm_core::config::LogLevel;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{Layer, Registry, fmt};

/// The name of the file being written.
pub const LOG_FILE: &str = "fsm.log";
const MOST_BYTES: u64 = 5 * 1024 * 1024;
/// The live file and the rolled ones together.
const MOST_FILES: u32 = 7;
const SECONDS_IN_A_DAY: u64 = 24 * 60 * 60;

/// The crates whose level the setting is for.
const OUR_CRATES: [&str; 8] = [
    "fsm",
    "fsm_app",
    "fsm_core",
    "fsm_nfs",
    "fsm_sftp",
    "fsm_smb",
    "fsm_vfs",
    "fsm_winfsp",
];

/// `%LOCALAPPDATA%\FsManager\logs`. `None` if `LOCALAPPDATA` is not set.
#[must_use]
pub fn default_folder() -> Option<PathBuf> {
    folder_under(env::var_os("LOCALAPPDATA")?)
}

fn folder_under(local_app_data: impl Into<PathBuf>) -> Option<PathBuf> {
    let base: PathBuf = local_app_data.into();
    (!base.as_os_str().is_empty()).then(|| base.join("FsManager").join("logs"))
}

/// Keeps the log being written. What is still waiting is written when this goes, so hold it until
/// the end of `main`.
#[must_use]
pub struct LogGuard {
    _worker: WorkerGuard,
}

/// Logs to `folder` from now on, for the whole process.
///
/// A log that cannot be written never stops the app: a folder that cannot be made just means no file.
///
/// # Errors
/// Something already took the process's logging.
pub fn start(folder: &Path, level: LogLevel) -> Result<LogGuard, TryInitError> {
    let (writer, guard) = tracing_appender::non_blocking(RollingFile::new(folder));
    subscriber(writer, level).try_init()?;
    Ok(LogGuard { _worker: guard })
}

/// One line for each event: time (UTC), level, the spans it is in, and the message.
fn subscriber<W>(writer: W, level: LogLevel) -> impl SubscriberExt + SubscriberInitExt + use<W>
where
    W: for<'writer> fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    let layer = fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(filter(level));
    Registry::default().with(layer)
}

fn filter(level: LogLevel) -> Targets {
    let level = match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
    };
    Targets::new()
        .with_default(level.min(LevelFilter::WARN))
        .with_targets(OUR_CRATES.map(|target| (target, level)))
}

fn day_of(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    since.as_secs() / SECONDS_IN_A_DAY
}

/// `fsm.log`, which moves to `fsm.log.1` when it has had enough.
struct RollingFile {
    folder: PathBuf,
    /// `None` until the first line, and while it cannot be opened.
    file: Option<File>,
    size: u64,
    day: u64,
}

impl RollingFile {
    fn new(folder: &Path) -> Self {
        Self {
            folder: folder.to_owned(),
            file: None,
            size: 0,
            day: 0,
        }
    }

    fn live_path(&self) -> PathBuf {
        self.folder.join(LOG_FILE)
    }

    fn rolled_path(&self, number: u32) -> PathBuf {
        self.folder.join(format!("{LOG_FILE}.{number}"))
    }

    /// Opens the live file, which may be there from an earlier run. Its day is the day it last
    /// changed, so yesterday's run does not get today's lines.
    fn open(&mut self, now: SystemTime) -> io::Result<()> {
        fs::create_dir_all(&self.folder)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.live_path())?;
        let about = file.metadata()?;
        self.size = about.len();
        self.day = about.modified().map_or_else(|_| day_of(now), day_of);
        self.file = Some(file);
        Ok(())
    }

    /// Everything moves up one, and the oldest goes. Whatever cannot be moved, perhaps as something has
    /// it open, stays where it is, and the live file carries on being written.
    fn roll(&mut self) {
        self.file = None;
        // A rename replaces what it lands on, which is how the oldest goes.
        for number in (1..MOST_FILES - 1).rev() {
            let _ = fs::rename(self.rolled_path(number), self.rolled_path(number + 1));
        }
        let _ = fs::rename(self.live_path(), self.rolled_path(1));
    }

    fn line_at(&mut self, line: &[u8], now: SystemTime) -> io::Result<()> {
        if self.file.is_none() {
            self.open(now)?;
        }
        let today = day_of(now);
        let bytes = line.len() as u64;
        if self.size > 0 && (self.day != today || self.size.saturating_add(bytes) > MOST_BYTES) {
            self.roll();
            self.open(now)?;
        }
        self.day = today;
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        file.write_all(line)?;
        self.size = self.size.saturating_add(bytes);
        Ok(())
    }
}

impl Write for RollingFile {
    /// Says it wrote the line whatever happened, as there is nobody to tell. The next line tries again.
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        if self.line_at(line, SystemTime::now()).is_err() {
            self.file = None;
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use fsm_vfs::Secret;
    use tempfile::TempDir;
    use tracing::{debug, info, info_span, warn};

    use super::*;

    const DAY: Duration = Duration::from_secs(SECONDS_IN_A_DAY);

    fn noon() -> SystemTime {
        UNIX_EPOCH + DAY * 20_000 + DAY / 2
    }

    fn read(folder: &TempDir, name: &str) -> String {
        fs::read_to_string(folder.path().join(name)).unwrap_or_default()
    }

    fn names_in(folder: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_folder_is_under_local_app_data() {
        assert_eq!(
            folder_under(r"C:\Users\me\AppData\Local").unwrap(),
            Path::new(r"C:\Users\me\AppData\Local\FsManager\logs"),
        );
        assert_eq!(folder_under(""), None);
    }

    #[test]
    fn lines_go_to_the_file_and_a_folder_is_made_for_them() {
        let folder = TempDir::new().unwrap();
        let inner = folder.path().join("a").join("b");
        let mut file = RollingFile::new(&inner);
        file.line_at(b"one\n", noon()).unwrap();
        file.line_at(b"two\n", noon()).unwrap();
        assert_eq!(
            fs::read_to_string(inner.join("fsm.log")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn a_new_day_starts_a_new_file() {
        let folder = TempDir::new().unwrap();
        let mut file = RollingFile::new(folder.path());
        file.line_at(b"today\n", noon()).unwrap();
        file.line_at(b"tomorrow\n", noon() + DAY).unwrap();
        assert_eq!(read(&folder, "fsm.log"), "tomorrow\n");
        assert_eq!(read(&folder, "fsm.log.1"), "today\n");
    }

    #[test]
    fn a_file_left_from_an_earlier_day_is_rolled_at_the_first_line() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("fsm.log");
        fs::write(&path, "earlier\n").unwrap();
        let left = OpenOptions::new().write(true).open(&path).unwrap();
        left.set_modified(noon() - DAY).unwrap();
        drop(left);

        let mut file = RollingFile::new(folder.path());
        file.line_at(b"now\n", noon()).unwrap();
        assert_eq!(read(&folder, "fsm.log"), "now\n");
        assert_eq!(read(&folder, "fsm.log.1"), "earlier\n");
    }

    #[test]
    fn a_file_left_from_today_is_carried_on_with() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("fsm.log");
        fs::write(&path, "earlier\n").unwrap();
        let left = OpenOptions::new().write(true).open(&path).unwrap();
        left.set_modified(noon()).unwrap();
        drop(left);

        let mut file = RollingFile::new(folder.path());
        file.line_at(b"now\n", noon()).unwrap();
        assert_eq!(read(&folder, "fsm.log"), "earlier\nnow\n");
        assert_eq!(names_in(&folder), ["fsm.log"]);
    }

    #[test]
    fn no_file_goes_over_the_limit() {
        let folder = TempDir::new().unwrap();
        let mut file = RollingFile::new(folder.path());
        let line = vec![b'x'; 2 * 1024 * 1024];
        for _ in 0..3 {
            file.line_at(&line, noon()).unwrap();
        }
        let size = |name: &str| fs::metadata(folder.path().join(name)).unwrap().len();
        assert_eq!(size("fsm.log.1"), 4 * 1024 * 1024);
        assert_eq!(size("fsm.log"), 2 * 1024 * 1024);
    }

    #[test]
    fn exactly_the_limit_still_fits() {
        let folder = TempDir::new().unwrap();
        let mut file = RollingFile::new(folder.path());
        let rest = vec![b'x'; usize::try_from(MOST_BYTES).unwrap() - 1];
        file.line_at(b"y", noon()).unwrap();
        file.line_at(&rest, noon()).unwrap();
        assert_eq!(names_in(&folder), ["fsm.log"]);
        file.line_at(b"z", noon()).unwrap();
        assert_eq!(names_in(&folder), ["fsm.log", "fsm.log.1"]);
    }

    #[test]
    fn seven_files_are_kept_and_the_oldest_goes() {
        let folder = TempDir::new().unwrap();
        let mut file = RollingFile::new(folder.path());
        for day in 0..10_u32 {
            let line = format!("day {day}\n");
            file.line_at(line.as_bytes(), noon() + DAY * day).unwrap();
        }
        assert_eq!(
            names_in(&folder),
            [
                "fsm.log",
                "fsm.log.1",
                "fsm.log.2",
                "fsm.log.3",
                "fsm.log.4",
                "fsm.log.5",
                "fsm.log.6"
            ],
        );
        assert_eq!(read(&folder, "fsm.log"), "day 9\n");
        assert_eq!(read(&folder, "fsm.log.1"), "day 8\n");
        assert_eq!(read(&folder, "fsm.log.6"), "day 3\n");
    }

    #[test]
    fn a_log_that_cannot_be_written_is_not_an_error_and_is_tried_again() {
        let folder = TempDir::new().unwrap();
        // A file where the folder should be.
        let in_the_way = folder.path().join("logs");
        fs::write(&in_the_way, "").unwrap();
        let mut file = RollingFile::new(&in_the_way);
        assert_eq!(file.write(b"lost\n").unwrap(), 5);

        fs::remove_file(&in_the_way).unwrap();
        assert_eq!(file.write(b"kept\n").unwrap(), 5);
        assert_eq!(
            fs::read_to_string(in_the_way.join("fsm.log")).unwrap(),
            "kept\n"
        );
    }

    /// A cloneable place for a subscriber to write to.
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

    impl<'writer> fmt::MakeWriter<'writer> for Captured {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self {
            self.clone()
        }
    }

    fn logged(level: LogLevel, what: impl FnOnce()) -> String {
        let captured = Captured::default();
        let subscriber = subscriber(captured.clone(), level);
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, what);
        let bytes = captured.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_line_has_the_time_the_level_the_span_and_the_message() {
        let text = logged(LogLevel::Info, || {
            let span = info_span!(target: "fsm_core", "mount", profile_id = 7, name = "NAS");
            let _inside = span.enter();
            info!(target: "fsm_core", letter = "Z:", "up");
        });
        assert!(
            text.contains(r#"INFO mount{profile_id=7 name="NAS"}: fsm_core: up letter="Z:""#),
            "{text}"
        );
        assert!(text.starts_with("20") || text.starts_with("19"), "{text}");
        assert!(text.contains("Z "), "the time says it is UTC: {text}");
        assert!(!text.contains('\x1b'), "no colours in a file: {text}");
    }

    #[test]
    fn the_setting_decides_how_much_of_ours_is_written() {
        let events = || {
            debug!(target: "fsm_sftp", "a debug line");
            info!(target: "fsm_core", "an info line");
            warn!(target: "fsm_app", "a warning");
            tracing::error!(target: "fsm_winfsp", "an error");
        };
        let lines = |level| logged(level, events).lines().count();
        assert_eq!(lines(LogLevel::Debug), 4);
        assert_eq!(lines(LogLevel::Info), 3);
        assert_eq!(lines(LogLevel::Warn), 2);
        assert_eq!(lines(LogLevel::Error), 1);
    }

    /// NFR-SEC-02: a library's debug lines can hold anything.
    #[test]
    fn other_crates_stay_at_warn_whatever_the_setting() {
        let text = logged(LogLevel::Debug, || {
            debug!(target: "russh::client", "client said secret-looking-thing");
            info!(target: "russh::client", "an info line");
            warn!(target: "russh::client", "a warning");
        });
        assert!(!text.contains("secret-looking-thing"), "{text}");
        assert!(!text.contains("an info line"), "{text}");
        assert!(text.contains("a warning"), "{text}");

        let text = logged(LogLevel::Error, || {
            warn!(target: "russh::client", "a warning");
        });
        assert!(text.is_empty(), "{text}");
    }

    #[test]
    fn a_secret_in_a_field_comes_out_as_stars() {
        let secret = Secret::new("hunter2".to_owned());
        let text = logged(LogLevel::Debug, || {
            info!(target: "fsm_core", ?secret, "typed");
        });
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
}
