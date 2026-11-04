// SPDX-License-Identifier: GPL-3.0-or-later
//! The log the scripted checks read: one line per event, stamped in ms since process creation.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();
static CREATED: OnceLock<u64> = OnceLock::new();

fn ticks(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

pub fn init(path: &str) {
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    let _ = unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) };
    let _ = CREATED.set(ticks(times[0]));
    if let Ok(file) = File::options().create(true).append(true).open(path) {
        let _ = FILE.set(Mutex::new(file));
    }
}

/// Milliseconds since the process was created (NFR-PERF-01's starting point).
pub fn since_start_ms() -> f64 {
    let now = ticks(unsafe { GetSystemTimePreciseAsFileTime() });
    let created = CREATED.get().copied().unwrap_or(now);
    now.saturating_sub(created) as f64 / 10_000.0
}

pub fn log(msg: impl AsRef<str>) {
    if let Some(file) = FILE.get()
        && let Ok(mut file) = file.lock()
    {
        let _ = writeln!(file, "{:10.1} {}", since_start_ms(), msg.as_ref());
    }
}

pub fn log_memory(label: &str) {
    let mut c = PROCESS_MEMORY_COUNTERS::default();
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size) }.as_bool() {
        log(format!(
            "memory {label}: working_set={:.1}MB peak={:.1}MB private={:.1}MB",
            c.WorkingSetSize as f64 / 1048576.0,
            c.PeakWorkingSetSize as f64 / 1048576.0,
            c.PagefileUsage as f64 / 1048576.0,
        ));
    }
}
