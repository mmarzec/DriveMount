// SPDX-License-Identifier: GPL-3.0-or-later

//! What the connection page shows (spec §7.1.1): a profile's settings as text, for reading. It knows
//! nothing of Slint, and holds no text to translate, so all of it can be tested without a window.
//! Secrets are never part of it.

use drivemount_core::profile::{DriveChoice, Profile, Protocol, SftpAuth, SmbAuth};
use drivemount_vfs::DriveLetter;

use super::form::jump_text;

/// The port Windows maps SMB shares on.
const SMB_PORT: u16 = 445;

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
    /// SMB: the signed-in Windows user.
    WindowsUser,
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
    /// What the page says about this profile. An SMB profile fills the SSH lines as best it can
    /// until M3-04 gives the page its own: the server, port 445, the account, and the UNC path.
    pub(super) fn of(profile: &Profile) -> Self {
        let drive = match profile.drive {
            DriveChoice::Auto => None,
            DriveChoice::Letter(letter) => Some(letter),
        };
        let sftp = match &profile.protocol {
            Protocol::Sftp(sftp) => sftp,
            Protocol::Smb(smb) => {
                return Self {
                    host: smb.server.clone(),
                    port: SMB_PORT,
                    user: smb.account(),
                    login: match smb.auth {
                        SmbAuth::Password { .. } => Login::Password,
                        SmbAuth::Ask => Login::Ask,
                        SmbAuth::WindowsUser => Login::WindowsUser,
                    },
                    remote_path: smb.unc_with_sub_path(),
                    drive,
                    jump_host: String::new(),
                    mount_on_start: profile.mount_on_start,
                    auto_reconnect: profile.auto_reconnect,
                    read_only: profile.read_only,
                    cache_secs: 0,
                    keepalive_secs: 0,
                };
            }
        };
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
            drive,
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

    #[test]
    fn an_smb_profile_shows_its_server_account_and_unc_path() {
        use drivemount_core::profile::SmbProfile;

        let mut smb = SmbProfile::new("nas", "media", "marcin", SmbAuth::WindowsUser);
        smb.domain = "CORP".to_owned();
        smb.sub_path = "films".to_owned();
        let details = Details::of(&Profile::new("NAS", Protocol::Smb(smb)));
        assert_eq!(details.host, "nas");
        assert_eq!(details.port, 445);
        assert_eq!(details.user, r"CORP\marcin");
        assert_eq!(details.login, Login::WindowsUser);
        assert_eq!(details.remote_path, r"\\nas\media\films");
        assert_eq!(details.jump_host, "");
    }
}
