// SPDX-License-Identifier: GPL-3.0-or-later

//! What the connection page shows (spec §7.1.1): a profile's settings as text, for reading. It knows
//! nothing of Slint, and holds no text to translate, so all of it can be tested without a window.
//! Secrets are never part of it.

use drivemount_core::profile::{DriveChoice, Profile, Protocol, SftpAuth};
use drivemount_vfs::DriveLetter;

use super::form::jump_text;

/// How the profile logs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Login {
    /// A password.
    Password,
    /// A key file, by its path.
    KeyFile(String),
    /// An SSH agent.
    Agent,
    /// Asked for at each mount.
    Ask,
}

/// A profile's settings, as the page lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Details {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub login: Login,
    pub remote_path: String,
    /// `None` is *Auto*.
    pub drive: Option<DriveLetter>,
    /// `[user@]host[:port]`, empty for none.
    pub jump_host: String,
    pub mount_on_start: bool,
    pub auto_reconnect: bool,
    pub read_only: bool,
    pub cache_secs: u64,
    pub keepalive_secs: u64,
}

impl Details {
    /// What the page says about this profile.
    pub(super) fn of(profile: &Profile) -> Self {
        let Protocol::Sftp(sftp) = &profile.protocol;
        Self {
            host: sftp.host.clone(),
            port: sftp.port,
            user: sftp.username.clone(),
            login: match &sftp.auth {
                SftpAuth::Password { .. } => Login::Password,
                SftpAuth::KeyFile { path, .. } => Login::KeyFile(path.display().to_string()),
                SftpAuth::Agent => Login::Agent,
                SftpAuth::Ask => Login::Ask,
            },
            remote_path: sftp.remote_path.clone(),
            drive: match profile.drive {
                DriveChoice::Auto => None,
                DriveChoice::Letter(letter) => Some(letter),
            },
            jump_host: sftp.jump.as_ref().map(jump_text).unwrap_or_default(),
            mount_on_start: profile.mount_on_start,
            auto_reconnect: profile.auto_reconnect,
            read_only: profile.read_only,
            cache_secs: sftp.cache_ttl_secs,
            keepalive_secs: sftp.keepalive_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use drivemount_core::profile::{JumpAuth, JumpHost, SftpProfile};

    use super::*;

    #[test]
    fn the_page_lists_what_the_profile_holds() {
        let mut sftp = SftpProfile::new(
            "nas",
            "marcin",
            SftpAuth::KeyFile {
                path: PathBuf::from(r"C:\keys\id"),
                remember: true,
            },
        );
        sftp.port = 2222;
        sftp.remote_path = "/srv".to_owned();
        sftp.jump = Some(JumpHost {
            host: "gate".to_owned(),
            port: 22,
            username: Some("j".to_owned()),
            auth: JumpAuth::Agent,
        });
        let mut profile = Profile::new("NAS", Protocol::Sftp(sftp));
        profile.drive = DriveChoice::Letter(DriveLetter::new('X').unwrap());
        profile.read_only = true;

        let details = Details::of(&profile);
        assert_eq!(details.host, "nas");
        assert_eq!(details.port, 2222);
        assert_eq!(details.user, "marcin");
        assert_eq!(details.login, Login::KeyFile(r"C:\keys\id".to_owned()));
        assert_eq!(details.remote_path, "/srv");
        assert_eq!(details.drive, DriveLetter::new('X'));
        assert_eq!(details.jump_host, "j@gate");
        assert!(details.read_only);
    }

    #[test]
    fn auto_and_no_jump_host_have_nothing_to_show() {
        let sftp = SftpProfile::new("h", "u", SftpAuth::Password { remember: true });
        let details = Details::of(&Profile::new("a", Protocol::Sftp(sftp)));
        assert_eq!(details.drive, None);
        assert_eq!(details.jump_host, "");
        assert_eq!(details.login, Login::Password);
    }
}
