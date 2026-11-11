// SPDX-License-Identifier: GPL-3.0-or-later

//! This process's age, for the start-up budgets (NFR-PERF-01, 02), which count from the moment
//! Windows created the process, before any of the program's own code ran.

use std::time::Duration;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// How long ago Windows created this process, or `None` if it will not say.
#[must_use]
pub fn since_start() -> Option<Duration> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the pseudo-handle of the current process needs no closing, and each pointer is to a
    // local that lives through the call.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .ok()?;
    // SAFETY: no arguments; it only reads the clock.
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    let ticks = ticks(now).checked_sub(ticks(created))?;
    // FILETIME counts 100 ns.
    Some(Duration::from_nanos(ticks.saturating_mul(100)))
}

fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_running_process_is_some_time_old_and_ages() {
        let first = since_start().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = since_start().unwrap();
        assert!(second > first, "{first:?} then {second:?}");
        // A test process is not a day old.
        assert!(second < Duration::from_secs(86_400), "{second:?}");
    }
}
