// SPDX-License-Identifier: GPL-3.0-or-later

//! Crash reports (FR-APP-07): a panic writes `crash-<time>.txt` to the logs folder (spec §6.4), and
//! the next start says so once, with *Open logs folder*.
//!
//! The release build has `panic = "abort"`, which takes effect after the hook has run, so the report is
//! written before the process goes. Which reports have been told about is kept in
//! [`NOTICED_FILE`] beside them: the name of the newest. Report names sort by time, so anything
//! after it is new.

use std::backtrace::Backtrace;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const PREFIX: &str = "crash-";
const SUFFIX: &str = ".txt";
/// The newest report that the user has been told about.
pub const NOTICED_FILE: &str = "last-crash-noticed";

/// From now on, a panic anywhere writes a report to `logs` before it goes on as it would have.
pub fn install(logs: PathBuf) {
    let before = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // First, as anything after it may fail too.
        let written = write_report(&logs, &report(info, SystemTime::now()));
        match &written {
            Ok(path) => tracing::error!(report = %path.display(), "{}", what_happened(info)),
            Err(error) => {
                tracing::error!(%error, "{} (no report was written)", what_happened(info))
            }
        }
        before(info);
    }));
}

/// The report's name for a panic at `now`: `crash-2026-11-09T171153.123Z.txt`, in UTC.
fn report_name(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (year, month, day, seconds) = civil(since.as_secs());
    format!(
        "{PREFIX}{year:04}-{month:02}-{day:02}T{:02}{:02}{:02}.{:03}Z{SUFFIX}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        since.subsec_millis(),
    )
}

/// The date, and the second of the day, for seconds since 1970 (Howard Hinnant's `civil_from_days`).
const fn civil(since_1970: u64) -> (u64, u64, u64, u64) {
    let days = since_1970 / 86_400;
    let seconds = since_1970 % 86_400;
    // Days since 0000-03-01, so that a leap day is the last of its year.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day, seconds)
}

fn what_happened(info: &PanicHookInfo<'_>) -> String {
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    let place = info.location().map_or_else(
        || "an unknown place".to_owned(),
        |at| format!("{}:{}:{}", at.file(), at.line(), at.column()),
    );
    format!("panicked at {place}: {message}")
}

fn report(info: &PanicHookInfo<'_>, now: SystemTime) -> (String, String) {
    let mut text = String::new();
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (year, month, day, seconds) = civil(since.as_secs());
    let _ = writeln!(
        text,
        "DriveMount {} stopped at {year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC.",
        env!("CARGO_PKG_VERSION"),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
    );
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "Thread: {}",
        thread::current().name().unwrap_or("(unnamed)")
    );
    let _ = writeln!(text, "{}", what_happened(info));
    let _ = writeln!(text);
    let _ = writeln!(text, "Backtrace:");
    let _ = writeln!(text, "{}", Backtrace::force_capture());
    (report_name(now), text)
}

/// Writes the report, under a name of its own even if another panic came in the same millisecond.
fn write_report(logs: &Path, (name, text): &(String, String)) -> io::Result<PathBuf> {
    fs::create_dir_all(logs)?;
    let stem = name.trim_end_matches(SUFFIX);
    for attempt in 0..100 {
        let path = if attempt == 0 {
            logs.join(name)
        } else {
            logs.join(format!("{stem}-{attempt}{SUFFIX}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many crash reports at once",
    ))
}

fn is_report(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// The newest report the user has not been told about, and marks it told, so that the next start says
/// nothing of it. `None` if there is none, or the folder cannot be read.
pub fn take_unnoticed(logs: &Path) -> Option<String> {
    let newest = fs::read_dir(logs)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| is_report(name))
        .max()?;
    let noticed_path = logs.join(NOTICED_FILE);
    let noticed = fs::read_to_string(&noticed_path).unwrap_or_default();
    if newest.as_str() <= noticed.trim() {
        return None;
    }
    if let Err(error) = fs::write(&noticed_path, &newest) {
        // Told again at the next start, which is better than never.
        tracing::warn!(%error, "could not note that a crash report was told about");
    }
    Some(newest)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis)
    }

    #[test]
    fn names_are_the_utc_time_and_sort_by_it() {
        assert_eq!(report_name(at(0, 0)), "crash-1970-01-01T000000.000Z.txt");
        // 2026-11-09 17:11:53 UTC.
        assert_eq!(
            report_name(at(1_794_244_313, 42)),
            "crash-2026-11-09T171153.042Z.txt"
        );
        // A leap day, and the day after.
        assert_eq!(
            report_name(at(1_709_164_800, 0)),
            "crash-2024-02-29T000000.000Z.txt"
        );
        assert_eq!(
            report_name(at(1_709_251_199, 999)),
            "crash-2024-02-29T235959.999Z.txt"
        );
        assert_eq!(
            report_name(at(1_709_251_200, 0)),
            "crash-2024-03-01T000000.000Z.txt"
        );
        assert_eq!(
            report_name(at(4_102_444_800, 0)),
            "crash-2100-01-01T000000.000Z.txt"
        );
        assert!(report_name(at(1_709_251_199, 999)) < report_name(at(1_709_251_200, 0)));
    }

    #[test]
    fn a_report_gets_a_name_of_its_own() {
        let logs = tempfile::tempdir().unwrap();
        let folder = logs.path().join("logs");
        let report = (report_name(at(0, 0)), "first".to_owned());
        let first = write_report(&folder, &report).unwrap();
        let second = write_report(&folder, &(report.0, "second".to_owned())).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), "first");
        assert_eq!(
            second.file_name().unwrap(),
            "crash-1970-01-01T000000.000Z-1.txt"
        );
    }

    #[test]
    fn a_new_report_is_told_about_once() {
        let logs = tempfile::tempdir().unwrap();
        let folder = logs.path();
        assert_eq!(take_unnoticed(&folder.join("none")), None);
        fs::write(folder.join("drivemount.log"), "").unwrap();
        assert_eq!(take_unnoticed(folder), None);

        fs::write(folder.join("crash-2026-11-09T100000.000Z.txt"), "").unwrap();
        fs::write(folder.join("crash-2026-11-09T090000.000Z.txt"), "").unwrap();
        assert_eq!(
            take_unnoticed(folder).as_deref(),
            Some("crash-2026-11-09T100000.000Z.txt")
        );
        assert_eq!(take_unnoticed(folder), None, "told already");

        fs::write(folder.join("crash-2026-11-10T080000.000Z.txt"), "").unwrap();
        assert_eq!(
            take_unnoticed(folder).as_deref(),
            Some("crash-2026-11-10T080000.000Z.txt")
        );
        assert_eq!(take_unnoticed(folder), None);
    }

    /// FR-APP-07's acceptance, in the test process: a panic leaves the report, and the hook before goes
    /// on to run. The panic is on a thread of its own, caught by `join`.
    #[test]
    fn a_panic_writes_a_report() {
        let logs = tempfile::tempdir().unwrap();
        install(logs.path().to_path_buf());
        let panicked = thread::Builder::new()
            .name("crash test".to_owned())
            .spawn(|| panic!("on purpose, to test the crash report"))
            .unwrap()
            .join();
        // Other tests' panics are reported too while this hook is in place, and only in the test
        // process: put the default back.
        let _ = panic::take_hook();
        assert!(panicked.is_err());

        let name = take_unnoticed(logs.path()).expect("no report");
        let text = fs::read_to_string(logs.path().join(&name)).unwrap();
        assert!(text.starts_with("DriveMount "), "{text}");
        assert!(text.contains("Thread: crash test"), "{text}");
        assert!(
            text.contains("on purpose, to test the crash report"),
            "{text}"
        );
        assert!(text.contains("crash.rs"), "{text}");
        assert!(text.contains("Backtrace:"), "{text}");
    }
}
