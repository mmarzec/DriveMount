// SPDX-License-Identifier: GPL-3.0-or-later

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The information page shows the day the exe was built. SOURCE_DATE_EPOCH makes it reproducible.
    // The script runs again when the code changes, so the date follows each build of new code.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rustc-env=DRIVEMOUNT_BUILD_DATE={}", build_date());

    #[cfg(windows)]
    {
        // The WinFsp DLL is found at run time through the registry (spike M0-1). Link arguments do not
        // cross packages, so drivemount-winfsp's build.rs does not reach this one's binary and tests.
        winfsp::build::winfsp_link_delayload();

        // The testing back-end finds elements through debug info, which a release build has no use for.
        let for_tests = std::env::var("PROFILE").is_ok_and(|profile| profile == "debug");
        let config = slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .with_debug_info(for_tests);
        slint_build::compile_with_config("ui/main_window.slint", config)?;
    }
    Ok(())
}

/// Today in UTC as `YYYY-MM-DD`, or the day of `SOURCE_DATE_EPOCH`.
fn build_date() -> String {
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs())
        });
    let (year, month, day) = civil_from_days(seconds / 86_400);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The calendar day of a count of days since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}
