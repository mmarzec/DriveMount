// SPDX-License-Identifier: GPL-3.0-or-later

//! The settings page (spec §7.5): what its controls mean as [`Settings`], and back; and what the
//! information page (spec §7.5.1) says of this build.

use drivemount_core::config::{LetterConflict, LogLevel, Settings, Theme};
use drivemount_win::winfsp::WinFspStatus;

use super::generated::{InfoView, SettingsForm, SettingsView, WinFspState};

const THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];
const CONFLICTS: [LetterConflict; 2] = [LetterConflict::Auto, LetterConflict::Fail];
const LOG_LEVELS: [LogLevel; 4] = [
    LogLevel::Error,
    LogLevel::Warn,
    LogLevel::Info,
    LogLevel::Debug,
];

fn index_of<T: PartialEq>(choices: &[T], value: &T) -> i32 {
    let place = choices.iter().position(|choice| choice == value);
    place
        .and_then(|place| i32::try_from(place).ok())
        .unwrap_or(0)
}

/// The choice at a drop-down's place, or `kept` for a place that is not there, as with nothing
/// chosen.
fn choice_at<T: Copy>(choices: &[T], index: i32, kept: T) -> T {
    usize::try_from(index)
        .ok()
        .and_then(|place| choices.get(place).copied())
        .unwrap_or(kept)
}

/// What the controls show for these settings.
pub(super) fn form_of(settings: &Settings) -> SettingsForm {
    SettingsForm {
        start_with_windows: settings.start_with_windows,
        close_to_tray: settings.close_to_tray,
        theme_index: index_of(&THEMES, &settings.theme),
        letter_index: index_of(&CONFLICTS, &settings.letter_conflict),
        notifications: settings.notifications,
        notification_sound: settings.notification_sound,
        log_index: index_of(&LOG_LEVELS, &settings.log_level),
    }
}

/// `settings` with what the controls say. What the page has no control for stays as it is
/// (spec §3.8: `op_timeout_secs` and `winfsp_threads` are only in the file).
pub(super) fn settings_from(form: &SettingsForm, settings: &Settings) -> Settings {
    Settings {
        start_with_windows: form.start_with_windows,
        close_to_tray: form.close_to_tray,
        theme: choice_at(&THEMES, form.theme_index, settings.theme),
        letter_conflict: choice_at(&CONFLICTS, form.letter_index, settings.letter_conflict),
        notifications: form.notifications,
        notification_sound: form.notification_sound,
        log_level: choice_at(&LOG_LEVELS, form.log_index, settings.log_level),
        ..settings.clone()
    }
}

/// The page's read-only parts.
pub(super) fn view(editable: bool, winfsp: WinFspStatus, has_logs: bool) -> SettingsView {
    let (state, version) = match winfsp {
        WinFspStatus::Ready(version) => (WinFspState::Ready, version.to_string()),
        WinFspStatus::TooOld(version) => (WinFspState::TooOld, version.to_string()),
        WinFspStatus::Missing => (WinFspState::Missing, String::new()),
    };
    SettingsView {
        editable,
        winfsp: state,
        winfsp_version: version.into(),
        has_logs,
    }
}

/// What the information page says of this build.
pub(super) fn info_view() -> InfoView {
    InfoView {
        version: env!("CARGO_PKG_VERSION").into(),
        build_date: env!("DRIVEMOUNT_BUILD_DATE").into(),
        source: shown_url(super::SOURCE_URL).into(),
    }
}

/// A web address as the page shows it: without the scheme or a closing slash.
fn shown_url(url: &str) -> &str {
    let bare = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    bare.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use drivemount_win::winfsp::WinFspVersion;

    use super::*;

    #[test]
    fn every_setting_goes_to_the_form_and_comes_back() {
        let settings = Settings {
            start_with_windows: true,
            close_to_tray: false,
            theme: Theme::Dark,
            letter_conflict: LetterConflict::Fail,
            notifications: false,
            notification_sound: true,
            log_level: LogLevel::Debug,
            op_timeout_secs: 30,
            winfsp_threads: 3,
        };
        let form = form_of(&settings);
        assert_eq!(form.theme_index, 2);
        assert_eq!(form.letter_index, 1);
        assert_eq!(form.log_index, 3);
        assert_eq!(
            settings_from(&form, &Settings::default()).log_level,
            LogLevel::Debug
        );

        let back = settings_from(&form, &settings);
        assert_eq!(back, settings);
    }

    #[test]
    fn the_defaults_are_the_first_choices_but_info() {
        let form = form_of(&Settings::default());
        assert_eq!(form.theme_index, 0, "System");
        assert_eq!(form.letter_index, 0, "Use another free letter");
        assert_eq!(form.log_index, 2, "Info");
    }

    #[test]
    fn what_the_page_has_no_control_for_is_kept() {
        let settings = Settings {
            op_timeout_secs: 60,
            winfsp_threads: 2,
            ..Settings::default()
        };
        let changed = settings_from(&form_of(&Settings::default()), &settings);
        assert_eq!(changed.op_timeout_secs, 60);
        assert_eq!(changed.winfsp_threads, 2);
    }

    #[test]
    fn nothing_chosen_keeps_the_setting() {
        let settings = Settings {
            theme: Theme::Light,
            ..Settings::default()
        };
        let form = SettingsForm {
            theme_index: -1,
            letter_index: 7,
            log_index: -1,
            ..form_of(&settings)
        };
        assert_eq!(settings_from(&form, &settings), settings);
    }

    #[test]
    fn the_winfsp_line_has_the_version() {
        let version = WinFspVersion {
            major: 2,
            minor: 1,
            build: 25156,
        };
        let ready = view(true, WinFspStatus::Ready(version), true);
        assert_eq!(ready.winfsp, WinFspState::Ready);
        assert_eq!(ready.winfsp_version, "2.1.25156");

        let old = WinFspVersion {
            minor: 0,
            ..version
        };
        assert_eq!(
            view(true, WinFspStatus::TooOld(old), true).winfsp,
            WinFspState::TooOld
        );
        let missing = view(false, WinFspStatus::Missing, false);
        assert_eq!(missing.winfsp, WinFspState::Missing);
        assert_eq!(missing.winfsp_version, "");
        assert!(!missing.editable);
    }

    #[test]
    fn the_information_page_has_the_version_and_the_day_of_the_build() {
        let info = info_view();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        let date = info.build_date.as_str();
        assert_eq!(date.len(), 10, "{date}");
        let shaped = date.char_indices().all(|(at, c)| {
            if at == 4 || at == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
        assert!(shaped, "{date}");
        assert!(date >= "2026-01-01", "{date}");
    }

    #[test]
    fn a_web_address_is_shown_without_its_scheme() {
        assert_eq!(
            shown_url("https://github.com/mmarzec/DriveMount/"),
            "github.com/mmarzec/DriveMount"
        );
        assert_eq!(shown_url("http://example.org"), "example.org");
        assert_eq!(shown_url(""), "");
    }
}
