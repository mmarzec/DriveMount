// SPDX-License-Identifier: GPL-3.0-or-later

//! The rules of spec §7.3. Each is a function of its field's value, so the dialog can check one field
//! as it is typed (FR-PROF-04), and [`validate_profile`] checks a whole profile before saving.
//!
//! `check_*` take a value of the model's type. `parse_*` take what is in a text box, apply the same
//! rule and give that value.
//!
//! All of this is free of I/O but the key file, which has to be looked at (`read_key_head`), and that
//! is passed in. It only looks at the start of the file: what `drivemount-sftp` decides at connect time
//! (spec §5.7), such as an RSA key's size, is not known here.

use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::net::IpAddr;
use std::path::Path;

use drivemount_vfs::{DriveLetter, RemotePath, Secret};
use thiserror::Error;

use super::paste::split_host;
use super::{
    DEFAULT_PORT, DriveChoice, JumpAuth, JumpHost, NfsProfile, Profile, Protocol, SftpAuth,
    SftpProfile, SmbAuth, SmbProfile,
};

const MAX_NAME_CHARS: usize = 64;
const MAX_USER_CHARS: usize = 64;
const NAME_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
const MAX_CACHE_SECS: u64 = 60;
const MIN_KEEPALIVE_SECS: u64 = 5;
const MAX_KEEPALIVE_SECS: u64 = 300;
const MAX_DNS_NAME: usize = 253;
const MAX_DNS_LABEL: usize = 63;
/// The longest share name Windows allows.
const MAX_SHARE_CHARS: usize = 80;
const SHARE_FORBIDDEN: &[char] = &[
    '\\', '/', '[', ']', ':', '|', '<', '>', '+', '=', ';', ',', '?', '*', '"',
];
/// Characters a Windows file or folder name cannot hold, besides the separators.
const FOLDER_FORBIDDEN: &[char] = &[':', '*', '?', '"', '<', '>', '|'];
/// The longest user principal name (`user@domain`) Windows takes.
const MAX_SMB_USER_CHARS: usize = 104;
const SMB_USER_FORBIDDEN: &[char] = &[
    '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>',
];
const MAX_DOMAIN_CHARS: usize = 255;
/// MOUNT's limit on a path (RFC 1813, `MNTPATHLEN`), in bytes.
const MAX_EXPORT_BYTES: usize = 1024;
const DOMAIN_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|', '@'];

/// Enough for the headers OpenSSL can put in front of a key.
const KEY_HEAD_BYTES: u64 = 8192;
/// What `russh` starts reading a key from, as whole lines. Text before it is skipped.
const PRIVATE_KEY_HEADERS: &[&[u8]] = &[
    b"-----BEGIN OPENSSH PRIVATE KEY-----",
    b"-----BEGIN RSA PRIVATE KEY-----",
    b"-----BEGIN EC PRIVATE KEY-----",
    b"-----BEGIN PRIVATE KEY-----",
    b"-----BEGIN ENCRYPTED PRIVATE KEY-----",
];
const PPK_HEADER: &[u8] = b"PuTTY-User-Key-File-";
const PUBLIC_KEY_STARTS: &[&[u8]] = &[
    b"ssh-",
    b"ecdsa-",
    b"sk-",
    b"-----BEGIN PUBLIC KEY-----",
    b"-----BEGIN RSA PUBLIC KEY-----",
    b"---- BEGIN SSH2 PUBLIC KEY ----",
];

/// A box in the profile dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// Name.
    Name,
    /// Host.
    Host,
    /// Port.
    Port,
    /// User.
    User,
    /// Key file.
    KeyFile,
    /// Remote path.
    RemotePath,
    /// Drive.
    Drive,
    /// Cache (s).
    Cache,
    /// Keep-alive (s).
    KeepAlive,
    /// Jump host: its `[user@]host[:port]`.
    JumpHost,
    /// The jump host's own key file.
    JumpKeyFile,
    /// SMB: the server.
    Server,
    /// SMB: the share.
    Share,
    /// SMB: the folder inside the share.
    SubPath,
    /// SMB: the user's domain.
    Domain,
    /// NFS: the export's path.
    Export,
    /// NFS: the UID.
    Uid,
    /// NFS: the GID.
    Gid,
    /// NFS: the fixed NFS port.
    NfsPort,
    /// NFS: the fixed MOUNT port.
    MountPort,
}

/// What is wrong with a value. `Display` is the text to show (spec §7.3), in English for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Problem {
    /// Blank.
    #[error("Name is required")]
    NameRequired,
    /// Over 64 characters.
    #[error("Use at most 64 characters")]
    NameTooLong,
    /// One of the characters Windows does not allow in a file name.
    #[error("A name cannot contain \\ / : * ? \" < > |")]
    NameForbiddenCharacter,
    /// Another profile has this name, in any case (FR-PROF-03).
    #[error("A connection with this name already exists")]
    NameTaken,
    /// Not a DNS name or an address.
    #[error("Enter a host name or IP address")]
    HostInvalid,
    /// Outside 1–65535.
    #[error("Port must be 1–65535")]
    PortOutOfRange,
    /// Blank.
    #[error("User name is required")]
    UserRequired,
    /// Over 64 characters.
    #[error("Use at most 64 characters")]
    UserTooLong,
    /// Has white space in it.
    #[error("A user name cannot contain spaces")]
    UserHasSpaces,
    /// Blank, with the key file method chosen.
    #[error("Select a private key file")]
    KeyFileRequired,
    /// No such file.
    #[error("This file does not exist")]
    KeyFileMissing,
    /// There, but it cannot be read by this user, or is a folder.
    #[error("This file cannot be read")]
    KeyFileUnreadable,
    /// A PuTTY key, by name or by content (FR-SSH-03).
    #[error("Convert with PuTTYgen → Conversions → Export OpenSSH key")]
    KeyFilePpk,
    /// A public key, by name or by content.
    #[error("This is a public key; select the private key file")]
    KeyFilePublic,
    /// Not an OpenSSH or PEM private key (E-KEY-FORMAT).
    #[error("Unsupported key file format.")]
    KeyFileFormat,
    /// Not one of the forms of remote path.
    #[error("Use an absolute path such as /var/www, or ~ for home")]
    RemotePathInvalid,
    /// Not Auto or D–Z.
    #[error("Choose Auto or a letter from D to Z")]
    DriveInvalid,
    /// Outside 0–60.
    #[error("Cache must be 0–60 seconds")]
    CacheOutOfRange,
    /// Not 0 or 5–300.
    #[error("Keep-alive must be 0 (off) or 5–300 seconds")]
    KeepAliveOutOfRange,
    /// Not `[user@]host[:port]`.
    #[error("Use user@host:port")]
    JumpHostInvalid,
    /// Not a name or an IPv4 address.
    #[error("Enter a server name or IP address")]
    ServerInvalid,
    /// An IPv6 address, which a UNC path cannot hold as it is.
    #[error("Write an IPv6 address as its ipv6-literal.net name, such as fe80--1.ipv6-literal.net")]
    ServerIpv6,
    /// Blank.
    #[error("Share is required")]
    ShareRequired,
    /// Over 80 characters.
    #[error("Use at most 80 characters")]
    ShareTooLong,
    /// A character Windows does not allow in a share name.
    #[error("A share name cannot contain \\ / [ ] : | < > + = ; , ? * \"")]
    ShareForbiddenCharacter,
    /// Not folder names separated by `\`.
    #[error("Use folder names separated by \\, such as films\\2024")]
    SubPathInvalid,
    /// Over 104 characters.
    #[error("Use at most 104 characters")]
    SmbUserTooLong,
    /// A character Windows does not allow in a user name.
    #[error("A user name cannot contain \" / \\ [ ] : ; | = , + * ? < >")]
    SmbUserForbiddenCharacter,
    /// Not a NetBIOS or DNS domain name.
    #[error("Enter a domain such as CORP or corp.example")]
    DomainInvalid,
    /// A domain in both boxes: `me@corp.example` in User and something in Domain.
    #[error("Leave Domain empty when the user name ends in @domain")]
    DomainTwice,
    /// Not an absolute path MOUNT can take.
    #[error("Use the export's absolute path, such as /srv/nfs/data")]
    ExportInvalid,
    /// Not a whole number that fits AUTH_SYS's 32 bits.
    #[error("Use a number from 0 to 4294967295")]
    IdOutOfRange,
    /// Neither empty nor 1–65535.
    #[error("Leave empty to ask the portmapper, or use 1–65535")]
    NfsPortInvalid,
}

/// A [`Problem`] and the box to mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{problem}")]
pub struct FieldError {
    /// The box to mark.
    pub field: Field,
    /// What to say next to it.
    pub problem: Problem,
}

/// Something to point out that does not stop a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveWarning {
    /// Another profile prefers this letter too, which is allowed: only one can be mounted on it.
    AlsoUsedBy(String),
}

impl fmt::Display for DriveWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlsoUsedBy(name) => write!(f, "Also used by '{name}'"),
        }
    }
}

/// The result of checking a whole profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Validation {
    /// At most one for each box.
    pub errors: Vec<FieldError>,
    /// On the Drive box.
    pub drive_warning: Option<DriveWarning>,
}

impl Validation {
    /// Whether Save is enabled (FR-PROF-04). Warnings do not count.
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.errors.is_empty()
    }

    /// What to show next to a box.
    #[must_use]
    pub fn problem(&self, field: Field) -> Option<Problem> {
        self.errors
            .iter()
            .find(|error| error.field == field)
            .map(|error| error.problem)
    }

    fn check(&mut self, field: Field, result: Result<(), Problem>) {
        if let Err(problem) = result {
            self.errors.push(FieldError { field, problem });
        }
    }
}

/// Checks a whole profile. `others` may include the profile itself, which is told apart by its id, so
/// the dialog can pass the whole list. Pass [`read_key_head`] as `read_key`.
#[must_use]
pub fn validate_profile(
    profile: &Profile,
    others: &[Profile],
    read_key: &dyn Fn(&Path) -> io::Result<Secret<Vec<u8>>>,
) -> Validation {
    let others = || others.iter().filter(|other| other.id != profile.id);
    let mut result = Validation::default();

    result.check(
        Field::Name,
        check_name(&profile.name, others().map(|other| other.name.as_str())),
    );
    result.check(Field::Drive, check_drive(profile.drive));
    result.drive_warning = drive_warning(
        profile.drive,
        others().map(|other| (other.name.as_str(), other.drive)),
    );

    match &profile.protocol {
        Protocol::Sftp(sftp) => check_sftp(&mut result, sftp, read_key),
        Protocol::Smb(smb) => check_smb(&mut result, smb),
        Protocol::Nfs(nfs) => check_nfs(&mut result, nfs),
    }
    result
}

/// The UID and GID cannot be wrong once they are numbers: the model's type is AUTH_SYS's.
fn check_nfs(result: &mut Validation, nfs: &NfsProfile) {
    result.check(Field::Server, check_nfs_server(&nfs.server));
    result.check(Field::Export, check_export(&nfs.export));
    result.check(Field::NfsPort, check_nfs_port(nfs.nfs_port));
    result.check(Field::MountPort, check_nfs_port(nfs.mount_port));
    result.check(Field::Cache, check_cache_secs(nfs.cache_ttl_secs));
}

/// The user and domain are only checked when they are sent: *Current Windows user* sends neither, and
/// keeps them as they were for when the method changes back.
fn check_smb(result: &mut Validation, smb: &SmbProfile) {
    result.check(Field::Server, check_server(&smb.server));
    result.check(Field::Share, check_share(&smb.share));
    result.check(Field::SubPath, check_sub_path(&smb.sub_path));
    if smb.auth == SmbAuth::WindowsUser {
        return;
    }
    result.check(Field::User, check_smb_user(&smb.username));
    let domain = check_domain(&smb.domain).and_then(|()| {
        if !smb.domain.is_empty() && smb.username.contains('@') {
            Err(Problem::DomainTwice)
        } else {
            Ok(())
        }
    });
    result.check(Field::Domain, domain);
}

fn check_sftp(
    result: &mut Validation,
    sftp: &SftpProfile,
    read_key: &dyn Fn(&Path) -> io::Result<Secret<Vec<u8>>>,
) {
    result.check(Field::Host, check_host(&sftp.host));
    result.check(Field::Port, check_port(sftp.port));
    result.check(Field::User, check_user(&sftp.username));
    if let SftpAuth::KeyFile { path, .. } = &sftp.auth {
        result.check(Field::KeyFile, check_key_file(path, read_key));
    }
    result.check(Field::RemotePath, check_remote_path(&sftp.remote_path));
    result.check(Field::Cache, check_cache_secs(sftp.cache_ttl_secs));
    result.check(Field::KeepAlive, check_keepalive_secs(sftp.keepalive_secs));
    if let Some(jump) = &sftp.jump {
        result.check(Field::JumpHost, check_jump_host(jump));
        if let JumpAuth::KeyFile { path } = &jump.auth {
            result.check(Field::JumpKeyFile, check_key_file(path, read_key));
        }
    }
}

/// Name: 1–64 characters, no `\ / : * ? " < > |` or control characters, and unique ignoring case and
/// the spaces around it. `others` must leave out the profile being edited.
///
/// # Errors
///
/// The first thing wrong with it.
pub fn check_name<'a>(
    name: &str,
    others: impl IntoIterator<Item = &'a str>,
) -> Result<(), Problem> {
    if name.trim().is_empty() {
        return Err(Problem::NameRequired);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Problem::NameTooLong);
    }
    if name.contains(NAME_FORBIDDEN) || name.contains(char::is_control) {
        return Err(Problem::NameForbiddenCharacter);
    }
    let folded = fold_name(name);
    if others.into_iter().any(|other| fold_name(other) == folded) {
        return Err(Problem::NameTaken);
    }
    Ok(())
}

fn fold_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Host: a DNS name, or an IPv4 or IPv6 address. That leaves no room for spaces, a scheme, `user@` or
/// `:port`, and brackets around an IPv6 address are not part of it. Pasted text gets those split off
/// first, by [`parse_pasted_host`](super::parse_pasted_host).
///
/// # Errors
///
/// [`Problem::HostInvalid`].
pub fn check_host(host: &str) -> Result<(), Problem> {
    if host.parse::<IpAddr>().is_ok() || is_dns_name(host) {
        Ok(())
    } else {
        Err(Problem::HostInvalid)
    }
}

/// ASCII only, so an international name goes in as `xn--…`. An underscore is not legal in DNS, but
/// such names are common on local networks, and `ssh` takes them.
fn is_dns_name(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.len() > MAX_DNS_NAME {
        return false;
    }
    // A last part of only digits is a mistyped address, such as 10.0.4.256, not a name.
    let is_address_like = name
        .rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|byte| byte.is_ascii_digit()));
    !is_address_like && name.split('.').all(is_dns_label)
}

fn is_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_DNS_LABEL
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// SMB server: a DNS or NetBIOS name, or an IPv4 address. A UNC path cannot hold an IPv6 address as it
/// is: Windows takes it as `fe80--1.ipv6-literal.net`, which is a name here.
///
/// # Errors
///
/// [`Problem::ServerIpv6`] or [`Problem::ServerInvalid`].
pub fn check_server(server: &str) -> Result<(), Problem> {
    match server.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => Ok(()),
        Ok(IpAddr::V6(_)) => Err(Problem::ServerIpv6),
        Err(_) if is_dns_name(server) => Ok(()),
        Err(_) => Err(Problem::ServerInvalid),
    }
}

/// NFS server: the Host rule, IPv6 addresses included, as nothing needs to fit it into a UNC path.
///
/// # Errors
///
/// [`Problem::ServerInvalid`].
pub fn check_nfs_server(server: &str) -> Result<(), Problem> {
    check_host(server).map_err(|_| Problem::ServerInvalid)
}

/// NFS export: an absolute path, as the server's `exports` lists it, of at most 1024 bytes, with no
/// `.` or `..` in it and no NUL. Doubled separators and one at the end are allowed, as `mount` takes
/// them.
///
/// # Errors
///
/// [`Problem::ExportInvalid`].
pub fn check_export(export: &str) -> Result<(), Problem> {
    let is_valid = export.starts_with('/')
        && export.len() <= MAX_EXPORT_BYTES
        && !export.contains('\0')
        && export.split('/').all(|name| name != "." && name != "..");
    if is_valid {
        Ok(())
    } else {
        Err(Problem::ExportInvalid)
    }
}

/// The UID and GID boxes: digits only, 0 to 4294967295.
///
/// # Errors
///
/// [`Problem::IdOutOfRange`].
pub fn parse_id(text: &str) -> Result<u32, Problem> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Problem::IdOutOfRange);
    }
    text.parse().map_err(|_| Problem::IdOutOfRange)
}

/// A fixed NFS or MOUNT port: none, which asks the portmapper, or 1–65535.
fn check_nfs_port(port: Option<u16>) -> Result<(), Problem> {
    match port {
        Some(0) => Err(Problem::NfsPortInvalid),
        _ => Ok(()),
    }
}

/// The NFS port and Mount port boxes: empty or 0 asks the portmapper, as 0 does in the file.
///
/// # Errors
///
/// [`Problem::NfsPortInvalid`].
pub fn parse_nfs_port(text: &str) -> Result<Option<u16>, Problem> {
    let text = text.trim();
    if text.is_empty() || text == "0" {
        return Ok(None);
    }
    port_from_digits(text)
        .map(Some)
        .map_err(|_| Problem::NfsPortInvalid)
}

/// SMB share: 1–80 characters, none of `\ / [ ] : | < > + = ; , ? * "` or control characters. `C$`
/// and other names ending in `$` are fine.
///
/// # Errors
///
/// The first thing wrong with it.
pub fn check_share(share: &str) -> Result<(), Problem> {
    if share.trim().is_empty() {
        return Err(Problem::ShareRequired);
    }
    if share.chars().count() > MAX_SHARE_CHARS {
        return Err(Problem::ShareTooLong);
    }
    if share.contains(SHARE_FORBIDDEN) || share.contains(char::is_control) {
        return Err(Problem::ShareForbiddenCharacter);
    }
    Ok(())
}

/// SMB sub-path: empty, or folder names separated by `\` or `/`. No empty part (so no separator at
/// either end), no `.` or `..`, none of `: * ? " < > |` or control characters, and no name ending in a
/// dot or a space, which Windows would drop.
///
/// # Errors
///
/// [`Problem::SubPathInvalid`].
pub fn check_sub_path(sub_path: &str) -> Result<(), Problem> {
    if sub_path.is_empty() {
        return Ok(());
    }
    let is_folder = |name: &str| {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && !name.contains(FOLDER_FORBIDDEN)
            && !name.contains(char::is_control)
    };
    if sub_path.split(['\\', '/']).all(is_folder) {
        Ok(())
    } else {
        Err(Problem::SubPathInvalid)
    }
}

/// SMB user: 1–104 characters (a user principal name such as `me@corp.example` fits), none of
/// `" / \ [ ] : ; | = , + * ? < >` or control characters. Spaces are allowed, as Windows has local
/// accounts with them. `DOMAIN\user` typed in the box is split first, by
/// [`split_smb_user`](super::split_smb_user).
///
/// # Errors
///
/// The first thing wrong with it.
pub fn check_smb_user(user: &str) -> Result<(), Problem> {
    if user.trim().is_empty() {
        return Err(Problem::UserRequired);
    }
    if user.chars().count() > MAX_SMB_USER_CHARS {
        return Err(Problem::SmbUserTooLong);
    }
    if user.contains(SMB_USER_FORBIDDEN) || user.contains(char::is_control) {
        return Err(Problem::SmbUserForbiddenCharacter);
    }
    Ok(())
}

/// SMB domain: empty, or a NetBIOS or DNS domain name of at most 255 characters, with no white space
/// and none of `\ / : * ? " < > | @`. `.` is the server's own accounts.
///
/// # Errors
///
/// [`Problem::DomainInvalid`].
pub fn check_domain(domain: &str) -> Result<(), Problem> {
    let is_valid = domain.is_empty()
        || (domain.chars().count() <= MAX_DOMAIN_CHARS
            && !domain.contains(DOMAIN_FORBIDDEN)
            && !domain.contains(|c: char| c.is_whitespace() || c.is_control()));
    if is_valid {
        Ok(())
    } else {
        Err(Problem::DomainInvalid)
    }
}

/// Port: 1–65535.
///
/// # Errors
///
/// [`Problem::PortOutOfRange`].
pub fn check_port(port: u16) -> Result<(), Problem> {
    if port == 0 {
        return Err(Problem::PortOutOfRange);
    }
    Ok(())
}

/// The Port box.
///
/// # Errors
///
/// [`Problem::PortOutOfRange`], also for anything that is not a number.
pub fn parse_port(text: &str) -> Result<u16, Problem> {
    port_from_digits(text.trim())
}

/// `str::parse` alone would also take `+22`.
pub(super) fn port_from_digits(digits: &str) -> Result<u16, Problem> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Problem::PortOutOfRange);
    }
    let port = digits.parse().map_err(|_| Problem::PortOutOfRange)?;
    check_port(port)?;
    Ok(port)
}

/// User: 1–64 characters, no white space.
///
/// # Errors
///
/// The first thing wrong with it.
pub fn check_user(user: &str) -> Result<(), Problem> {
    if user.is_empty() {
        return Err(Problem::UserRequired);
    }
    if user.chars().count() > MAX_USER_CHARS {
        return Err(Problem::UserTooLong);
    }
    if user.contains(char::is_whitespace) {
        return Err(Problem::UserHasSpaces);
    }
    Ok(())
}

/// Key file: not named `.ppk` or `.pub` (FR-SSH-03), there, readable, and starting like an OpenSSH or
/// PEM private key. The name is enough to refuse a file, so `drivemount-sftp`, which goes by content alone,
/// would take a real key named `.ppk` that this refuses. Pass [`read_key_head`] as `read`.
///
/// # Errors
///
/// The first thing wrong with it.
pub fn check_key_file(
    path: &Path,
    read: impl FnOnce(&Path) -> io::Result<Secret<Vec<u8>>>,
) -> Result<(), Problem> {
    if path.as_os_str().is_empty() {
        return Err(Problem::KeyFileRequired);
    }
    let extension = path.extension().and_then(|extension| extension.to_str());
    if extension.is_some_and(|extension| extension.eq_ignore_ascii_case("ppk")) {
        return Err(Problem::KeyFilePpk);
    }
    if extension.is_some_and(|extension| extension.eq_ignore_ascii_case("pub")) {
        return Err(Problem::KeyFilePublic);
    }
    let head = read(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => Problem::KeyFileMissing,
        _ => Problem::KeyFileUnreadable,
    })?;
    check_key_head(head.expose_secret())
}

fn check_key_head(head: &[u8]) -> Result<(), Problem> {
    let mut lines = head
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let first = lines
        .clone()
        .find(|line| !line.trim_ascii().is_empty())
        .unwrap_or_default();
    if first.starts_with(PPK_HEADER) {
        return Err(Problem::KeyFilePpk);
    }
    if lines.any(|line| PRIVATE_KEY_HEADERS.contains(&line)) {
        return Ok(());
    }
    if PUBLIC_KEY_STARTS
        .iter()
        .any(|start| first.starts_with(start))
    {
        return Err(Problem::KeyFilePublic);
    }
    Err(Problem::KeyFileFormat)
}

/// Reads the start of a file for [`check_key_file`]. This can hold part of a private key, so it is
/// wiped afterwards. A file on a network path that is down can take a while.
///
/// # Errors
///
/// What opening or reading the file gave.
pub fn read_key_head(path: &Path) -> io::Result<Secret<Vec<u8>>> {
    let mut head = Vec::new();
    let read = File::open(path).and_then(|file| file.take(KEY_HEAD_BYTES).read_to_end(&mut head));
    let head = Secret::new(head);
    read?;
    Ok(head)
}

/// Remote path: `~`, `~/…`, an absolute path, or `C:/…` or `C:\…` for Windows OpenSSH (FR-SSH-13).
/// One that goes above the root, or has a NUL in it, could never be opened.
///
/// # Errors
///
/// [`Problem::RemotePathInvalid`].
pub fn check_remote_path(path: &str) -> Result<(), Problem> {
    let is_valid = if path == "~" {
        true
    } else if let Some(below_home) = path.strip_prefix("~/") {
        !below_home.contains('\0')
    } else if path.starts_with('/') {
        RemotePath::new(path).is_ok()
    } else {
        is_windows_path(path) && RemotePath::new(&format!("/{}", path.replace('\\', "/"))).is_ok()
    };
    if is_valid {
        Ok(())
    } else {
        Err(Problem::RemotePathInvalid)
    }
}

fn is_windows_path(path: &str) -> bool {
    let mut chars = path.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && matches!(chars.next(), Some('/' | '\\'))
}

/// Drive: Auto or D–Z.
///
/// # Errors
///
/// [`Problem::DriveInvalid`].
pub fn check_drive(drive: DriveChoice) -> Result<(), Problem> {
    match drive {
        DriveChoice::Letter(letter) if letter.as_char() < 'D' => Err(Problem::DriveInvalid),
        DriveChoice::Auto | DriveChoice::Letter(_) => Ok(()),
    }
}

/// The Drive box: `Auto`, `Z` or `Z:`, in either case.
///
/// # Errors
///
/// [`Problem::DriveInvalid`].
pub fn parse_drive(text: &str) -> Result<DriveChoice, Problem> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Ok(DriveChoice::Auto);
    }
    let letter = text.strip_suffix(':').unwrap_or(text);
    let mut chars = letter.chars();
    let drive = match (chars.next().and_then(DriveLetter::new), chars.next()) {
        (Some(letter), None) => DriveChoice::Letter(letter),
        _ => return Err(Problem::DriveInvalid),
    };
    check_drive(drive)?;
    Ok(drive)
}

/// The warning when another profile prefers the same letter, naming the first. `others` must leave out
/// the profile being edited. Auto never gets one.
#[must_use]
pub fn drive_warning<'a>(
    drive: DriveChoice,
    others: impl IntoIterator<Item = (&'a str, DriveChoice)>,
) -> Option<DriveWarning> {
    if drive == DriveChoice::Auto {
        return None;
    }
    others
        .into_iter()
        .find(|(_, other)| *other == drive)
        .map(|(name, _)| DriveWarning::AlsoUsedBy(name.to_owned()))
}

/// Cache (s): 0–60.
///
/// # Errors
///
/// [`Problem::CacheOutOfRange`].
pub fn check_cache_secs(secs: u64) -> Result<(), Problem> {
    if secs > MAX_CACHE_SECS {
        return Err(Problem::CacheOutOfRange);
    }
    Ok(())
}

/// The Cache (s) box.
///
/// # Errors
///
/// [`Problem::CacheOutOfRange`], also for anything that is not a number.
pub fn parse_cache_secs(text: &str) -> Result<u64, Problem> {
    let secs = secs_from_text(text).ok_or(Problem::CacheOutOfRange)?;
    check_cache_secs(secs)?;
    Ok(secs)
}

/// Keep-alive (s): 0 or 5–300.
///
/// # Errors
///
/// [`Problem::KeepAliveOutOfRange`].
pub fn check_keepalive_secs(secs: u64) -> Result<(), Problem> {
    if secs == 0 || (MIN_KEEPALIVE_SECS..=MAX_KEEPALIVE_SECS).contains(&secs) {
        Ok(())
    } else {
        Err(Problem::KeepAliveOutOfRange)
    }
}

/// The Keep-alive (s) box.
///
/// # Errors
///
/// [`Problem::KeepAliveOutOfRange`], also for anything that is not a number.
pub fn parse_keepalive_secs(text: &str) -> Result<u64, Problem> {
    let secs = secs_from_text(text).ok_or(Problem::KeepAliveOutOfRange)?;
    check_keepalive_secs(secs)?;
    Ok(secs)
}

fn secs_from_text(text: &str) -> Option<u64> {
    let text = text.trim();
    let is_number = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if is_number { text.parse().ok() } else { None }
}

fn check_jump_host(jump: &JumpHost) -> Result<(), Problem> {
    let user_ok = jump
        .username
        .as_deref()
        .is_none_or(|user| check_user(user).is_ok());
    if user_ok && check_host(&jump.host).is_ok() && check_port(jump.port).is_ok() {
        Ok(())
    } else {
        Err(Problem::JumpHostInvalid)
    }
}

/// What the Jump host box holds: where to go, without how to log in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpTarget {
    /// `None` is the profile's own user.
    pub username: Option<String>,
    /// A DNS name, or an IPv4 or IPv6 address without brackets.
    pub host: String,
    /// 22 if the box gives none.
    pub port: u16,
}

/// The Jump host box: empty, or `[user@]host[:port]`, with brackets around an IPv6 address that has a
/// port. Empty gives `None`.
///
/// # Errors
///
/// [`Problem::JumpHostInvalid`].
pub fn parse_jump_host(text: &str) -> Result<Option<JumpTarget>, Problem> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (username, rest) = match text.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, text),
    };
    let invalid = |_| Problem::JumpHostInvalid;
    if let Some(user) = username {
        check_user(user).map_err(invalid)?;
    }
    let (host, port) = split_host(rest).ok_or(Problem::JumpHostInvalid)?;
    check_host(host).map_err(invalid)?;
    let port = port
        .map_or(Ok(DEFAULT_PORT), port_from_digits)
        .map_err(invalid)?;
    Ok(Some(JumpTarget {
        username: username.map(str::to_owned),
        host: host.to_owned(),
        port,
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use uuid::Uuid;

    const OPENSSH_KEY: &[u8] = b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk=\n";

    fn no_file(_: &Path) -> io::Result<Secret<Vec<u8>>> {
        Err(ErrorKind::NotFound.into())
    }

    fn key_file(_: &Path) -> io::Result<Secret<Vec<u8>>> {
        Ok(Secret::new(OPENSSH_KEY.to_vec()))
    }

    fn containing(head: &[u8]) -> impl FnOnce(&Path) -> io::Result<Secret<Vec<u8>>> {
        let head = head.to_vec();
        move |_| Ok(Secret::new(head))
    }

    fn sftp(profile: &mut Profile) -> &mut SftpProfile {
        let Protocol::Sftp(sftp) = &mut profile.protocol else {
            panic!("not SFTP");
        };
        sftp
    }

    fn valid() -> Profile {
        let mut sftp = SftpProfile::new("10.0.4.21", "deploy", SftpAuth::Agent);
        sftp.remote_path = "/var/www".to_owned();
        Profile::new("Production", Protocol::Sftp(sftp))
    }

    fn letter(letter: char) -> DriveChoice {
        DriveChoice::Letter(DriveLetter::new(letter).unwrap())
    }

    // ---- Name. ----

    #[test]
    fn a_name_is_required() {
        assert_eq!(check_name("", []), Err(Problem::NameRequired));
        assert_eq!(check_name("   ", []), Err(Problem::NameRequired));
        assert_eq!(Problem::NameRequired.to_string(), "Name is required");
    }

    #[test]
    fn a_name_is_at_most_64_characters_not_bytes() {
        assert_eq!(check_name(&"a".repeat(64), []), Ok(()));
        assert_eq!(check_name(&"ż".repeat(64), []), Ok(()));
        assert_eq!(check_name(&"a".repeat(65), []), Err(Problem::NameTooLong));
    }

    #[test]
    fn a_name_has_none_of_the_forbidden_characters() {
        for c in ['\\', '/', ':', '*', '?', '"', '<', '>', '|', '\n', '\t'] {
            let name = format!("a{c}b");
            assert_eq!(
                check_name(&name, []),
                Err(Problem::NameForbiddenCharacter),
                "{name:?}"
            );
        }
        assert_eq!(check_name("Dev box (żółw) #1", []), Ok(()));
    }

    #[test]
    fn a_name_is_unique_ignoring_case() {
        for name in ["Production", "production", "PRODUCTION", " production "] {
            assert_eq!(
                check_name(name, ["NAS", "Production"]),
                Err(Problem::NameTaken),
                "{name:?}"
            );
        }
        assert_eq!(
            check_name("ŻÓŁW", ["żółw"]),
            Err(Problem::NameTaken),
            "beyond ASCII"
        );
        assert_eq!(check_name("Staging", ["NAS", "Production"]), Ok(()));
        assert_eq!(
            Problem::NameTaken.to_string(),
            "A connection with this name already exists"
        );
    }

    // ---- Host. ----

    #[test]
    fn a_host_can_be_a_dns_name() {
        for host in [
            "nas",
            "NAS",
            "bastion.example.com",
            "example.com.",
            "my-host_1.local",
            "xn--wyw-tna5c.pl",
            "1.example.com",
            &format!("{}.com", "a".repeat(63)),
        ] {
            assert_eq!(check_host(host), Ok(()), "{host}");
        }
    }

    #[test]
    fn a_host_can_be_an_ipv4_or_ipv6_address() {
        for host in ["10.0.4.21", "::1", "2001:db8::1", "::ffff:10.0.4.21"] {
            assert_eq!(check_host(host), Ok(()), "{host}");
        }
    }

    #[test]
    fn a_host_has_no_spaces_scheme_user_or_port() {
        for host in [
            "my host",
            " nas",
            "nas ",
            "sftp://nas",
            "deploy@nas",
            "nas:22",
            "[::1]",
            "[::1]:22",
            "nas/var/www",
        ] {
            assert_eq!(check_host(host), Err(Problem::HostInvalid), "{host}");
        }
        assert_eq!(
            Problem::HostInvalid.to_string(),
            "Enter a host name or IP address"
        );
    }

    #[test]
    fn a_host_that_is_neither_a_name_nor_an_address_is_refused() {
        let long_label = format!("{}.com", "a".repeat(64));
        let long_name = vec!["a".repeat(63); 4].join(".") + ".b";
        for host in [
            "",
            ".",
            "a..b",
            ".nas",
            "-nas",
            "nas-",
            "a.-b.c",
            "10.0.4.256",
            "1234",
            "żółw.pl",
            "1::2::3",
            &long_label,
            &long_name,
        ] {
            assert_eq!(check_host(host), Err(Problem::HostInvalid), "{host}");
        }
    }

    // ---- Port. ----

    #[test]
    fn a_port_is_1_to_65535() {
        assert_eq!(check_port(0), Err(Problem::PortOutOfRange));
        assert_eq!(check_port(1), Ok(()));
        assert_eq!(check_port(65535), Ok(()));
        assert_eq!(parse_port(" 2222 "), Ok(2222));
        assert_eq!(parse_port("65535"), Ok(65535));
        for text in [
            "",
            "0",
            "65536",
            "99999999999999999999",
            "abc",
            "+22",
            "-1",
            "2 2",
        ] {
            assert_eq!(parse_port(text), Err(Problem::PortOutOfRange), "{text:?}");
        }
        assert_eq!(Problem::PortOutOfRange.to_string(), "Port must be 1–65535");
    }

    // ---- User. ----

    #[test]
    fn a_user_is_required() {
        assert_eq!(check_user(""), Err(Problem::UserRequired));
        assert_eq!(Problem::UserRequired.to_string(), "User name is required");
    }

    #[test]
    fn a_user_is_at_most_64_characters() {
        assert_eq!(check_user(&"ż".repeat(64)), Ok(()));
        assert_eq!(check_user(&"a".repeat(65)), Err(Problem::UserTooLong));
    }

    #[test]
    fn a_user_has_no_spaces() {
        for user in ["a b", " a", "a\tb", "a\nb"] {
            assert_eq!(check_user(user), Err(Problem::UserHasSpaces), "{user:?}");
        }
        for user in ["deploy", r"DOMAIN\me", "me@domain.example"] {
            assert_eq!(check_user(user), Ok(()), "{user}");
        }
    }

    // ---- Key file. ----

    #[test]
    fn a_key_file_is_required_for_the_key_method() {
        assert_eq!(
            check_key_file(Path::new(""), key_file),
            Err(Problem::KeyFileRequired)
        );
    }

    #[test]
    fn a_key_file_must_exist() {
        assert_eq!(
            check_key_file(Path::new("id_ed25519"), no_file),
            Err(Problem::KeyFileMissing)
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_key_file(&dir.path().join("id_ed25519"), read_key_head),
            Err(Problem::KeyFileMissing)
        );
    }

    #[test]
    fn a_key_file_must_be_readable() {
        let denied = |_: &Path| Err(ErrorKind::PermissionDenied.into());
        assert_eq!(
            check_key_file(Path::new("id_ed25519"), denied),
            Err(Problem::KeyFileUnreadable)
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_key_file(dir.path(), read_key_head),
            Err(Problem::KeyFileUnreadable),
            "a folder"
        );
    }

    #[test]
    fn a_ppk_file_gets_the_conversion_hint_and_blocks_saving() {
        for name in ["key.ppk", "KEY.PPK", r"C:\Users\me\.ssh\key.Ppk"] {
            assert_eq!(
                check_key_file(Path::new(name), key_file),
                Err(Problem::KeyFilePpk),
                "going by the name, whatever is in it: {name}"
            );
        }
        let ppk = b"PuTTY-User-Key-File-3: ssh-ed25519\r\nEncryption: none\r\n";
        assert_eq!(
            check_key_file(Path::new("renamed"), containing(ppk)),
            Err(Problem::KeyFilePpk)
        );
        assert_eq!(
            Problem::KeyFilePpk.to_string(),
            "Convert with PuTTYgen → Conversions → Export OpenSSH key"
        );

        let mut profile = valid();
        sftp(&mut profile).auth = SftpAuth::KeyFile {
            path: PathBuf::from("key.ppk"),
            remember: false,
        };
        let result = validate_profile(&profile, &[], &key_file);
        assert_eq!(result.problem(Field::KeyFile), Some(Problem::KeyFilePpk));
        assert!(!result.can_save());
    }

    #[test]
    fn a_public_key_is_refused_by_name_and_by_content() {
        for name in ["id_ed25519.pub", "ID_RSA.PUB", "id_ed25519-cert.pub"] {
            assert_eq!(
                check_key_file(Path::new(name), key_file),
                Err(Problem::KeyFilePublic),
                "{name}"
            );
        }
        let heads: [&[u8]; 6] = [
            b"ssh-ed25519 AAAAC3Nza me@pc\n",
            b"ssh-rsa AAAAB3Nza me@pc\n",
            b"ecdsa-sha2-nistp256 AAAAE2Vj me@pc\n",
            b"sk-ssh-ed25519@openssh.com AAAAGnNr me@pc\n",
            b"-----BEGIN PUBLIC KEY-----\nMCowBQ==\n",
            b"---- BEGIN SSH2 PUBLIC KEY ----\nAAAA\n",
        ];
        for head in heads {
            assert_eq!(
                check_key_file(Path::new("copy"), containing(head)),
                Err(Problem::KeyFilePublic),
                "{}",
                String::from_utf8_lossy(head)
            );
        }
        assert_eq!(
            Problem::KeyFilePublic.to_string(),
            "This is a public key; select the private key file"
        );
    }

    #[test]
    fn a_key_file_is_openssh_or_pem_format() {
        for header in PRIVATE_KEY_HEADERS {
            let mut head = header.to_vec();
            head.extend_from_slice(b"\r\nAAAA\r\n");
            assert_eq!(
                check_key_file(Path::new("key"), containing(&head)),
                Ok(()),
                "{}",
                String::from_utf8_lossy(header)
            );
        }
        let with_bag_attributes =
            b"Bag Attributes\n    friendlyName: me\n-----BEGIN PRIVATE KEY-----\n";
        assert_eq!(
            check_key_file(Path::new("key"), containing(with_bag_attributes)),
            Ok(()),
            "russh skips text before the header too"
        );
        let heads: [&[u8]; 4] = [
            b"",
            b"just some text\n",
            b"\xff\xfe\x00binary",
            b"-----BEGIN DSA PRIVATE KEY-----\nAAAA\n",
        ];
        for head in heads {
            assert_eq!(
                check_key_file(Path::new("key"), containing(head)),
                Err(Problem::KeyFileFormat),
                "{}",
                String::from_utf8_lossy(head)
            );
        }
    }

    #[test]
    fn reading_the_head_of_a_key_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_ed25519");
        fs::write(&path, OPENSSH_KEY).unwrap();
        assert_eq!(read_key_head(&path).unwrap().expose_secret(), OPENSSH_KEY);
        assert_eq!(check_key_file(&path, read_key_head), Ok(()));

        let large = vec![b'a'; 100_000];
        fs::write(&path, large).unwrap();
        assert_eq!(read_key_head(&path).unwrap().expose_secret().len(), 8192);
        assert_eq!(
            check_key_file(&path, read_key_head),
            Err(Problem::KeyFileFormat)
        );
    }

    // ---- Remote path. ----

    #[test]
    fn a_remote_path_can_be_home_or_below_it() {
        for path in ["~", "~/", "~/www", "~/a b/ż", "~/../shared"] {
            assert_eq!(check_remote_path(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn a_remote_path_can_be_absolute() {
        for path in ["/", "/var/www", "/a b/ż/", "/var/../srv", "/C:/data"] {
            assert_eq!(check_remote_path(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn a_remote_path_can_be_a_windows_form() {
        for path in ["C:/", "C:/data", "c:/Users/me", r"C:\data"] {
            assert_eq!(check_remote_path(path), Ok(()), "{path}");
        }
    }

    #[test]
    fn any_other_remote_path_is_refused() {
        for path in [
            "", " ", "var/www", "./www", "~me", "~me/www", " /var", "C:", "C:data", "CC:/data",
            "1:/data", "ż:/data", r"\data", "/..", "/a/../..", "C:/../..", "/a\0b", "~/a\0b",
        ] {
            assert_eq!(
                check_remote_path(path),
                Err(Problem::RemotePathInvalid),
                "{path:?}"
            );
        }
        assert_eq!(
            Problem::RemotePathInvalid.to_string(),
            "Use an absolute path such as /var/www, or ~ for home"
        );
    }

    // ---- Drive. ----

    #[test]
    fn a_drive_is_auto_or_d_to_z() {
        assert_eq!(check_drive(DriveChoice::Auto), Ok(()));
        for c in 'D'..='Z' {
            assert_eq!(check_drive(letter(c)), Ok(()), "{c}");
        }
        for c in 'A'..='C' {
            assert_eq!(check_drive(letter(c)), Err(Problem::DriveInvalid), "{c}");
        }
    }

    #[test]
    fn the_drive_box_takes_auto_or_a_letter() {
        for text in ["Auto", "auto", " AUTO "] {
            assert_eq!(parse_drive(text), Ok(DriveChoice::Auto), "{text}");
        }
        for text in ["Z", "z", "Z:", " z: "] {
            assert_eq!(parse_drive(text), Ok(letter('Z')), "{text}");
        }
        for text in ["", ":", "A", "c:", "1", "ZZ", "Z::", "ż", "Z:\\"] {
            assert_eq!(parse_drive(text), Err(Problem::DriveInvalid), "{text:?}");
        }
    }

    #[test]
    fn a_letter_another_profile_uses_only_warns() {
        let others = [("Dev box", letter('X')), ("NAS backup", letter('Z'))];
        let warning = drive_warning(letter('Z'), others).unwrap();
        assert_eq!(warning.to_string(), "Also used by 'NAS backup'");
        assert_eq!(drive_warning(letter('Y'), others), None);
        assert_eq!(drive_warning(DriveChoice::Auto, others), None);
        assert_eq!(
            drive_warning(DriveChoice::Auto, [("Auto too", DriveChoice::Auto)]),
            None,
            "two on Auto are not a conflict"
        );

        let mut other = valid();
        other.name = "NAS backup".to_owned();
        other.drive = letter('Z');
        let mut profile = valid();
        profile.drive = letter('Z');
        let result = validate_profile(&profile, &[other], &key_file);
        assert_eq!(
            result.drive_warning,
            Some(DriveWarning::AlsoUsedBy("NAS backup".to_owned()))
        );
        assert!(result.can_save());
    }

    // ---- Cache and keep-alive. ----

    #[test]
    fn a_cache_is_0_to_60_seconds() {
        assert_eq!(check_cache_secs(0), Ok(()));
        assert_eq!(check_cache_secs(60), Ok(()));
        assert_eq!(check_cache_secs(61), Err(Problem::CacheOutOfRange));
        assert_eq!(parse_cache_secs(" 5 "), Ok(5));
        for text in ["", "61", "abc", "-1", "+5", "1.5", "99999999999999999999"] {
            assert_eq!(
                parse_cache_secs(text),
                Err(Problem::CacheOutOfRange),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_keepalive_is_0_or_5_to_300_seconds() {
        for secs in [0, 5, 15, 300] {
            assert_eq!(check_keepalive_secs(secs), Ok(()), "{secs}");
        }
        for secs in [1, 4, 301] {
            assert_eq!(
                check_keepalive_secs(secs),
                Err(Problem::KeepAliveOutOfRange),
                "{secs}"
            );
        }
        assert_eq!(parse_keepalive_secs(" 15 "), Ok(15));
        for text in ["", "4", "301", "abc", "-1", "+5", "99999999999999999999"] {
            assert_eq!(
                parse_keepalive_secs(text),
                Err(Problem::KeepAliveOutOfRange),
                "{text:?}"
            );
        }
    }

    // ---- Jump host. ----

    #[test]
    fn a_jump_host_can_be_empty() {
        assert_eq!(parse_jump_host(""), Ok(None));
        assert_eq!(parse_jump_host("  "), Ok(None));
    }

    #[test]
    fn a_jump_host_is_host_with_optional_user_and_port() {
        let target = |username: Option<&str>, host: &str, port| {
            Ok(Some(JumpTarget {
                username: username.map(str::to_owned),
                host: host.to_owned(),
                port,
            }))
        };
        assert_eq!(parse_jump_host("bastion"), target(None, "bastion", 22));
        assert_eq!(
            parse_jump_host("bastion:2222"),
            target(None, "bastion", 2222)
        );
        assert_eq!(
            parse_jump_host("jump@bastion"),
            target(Some("jump"), "bastion", 22)
        );
        assert_eq!(
            parse_jump_host(" jump@bastion.example.com:2222 "),
            target(Some("jump"), "bastion.example.com", 2222)
        );
        assert_eq!(
            parse_jump_host("2001:db8::1"),
            target(None, "2001:db8::1", 22)
        );
        assert_eq!(parse_jump_host("[::1]:2222"), target(None, "::1", 2222));
        assert_eq!(
            parse_jump_host("me@x.org@[::1]"),
            target(Some("me@x.org"), "::1", 22)
        );
    }

    #[test]
    fn any_other_jump_host_is_refused() {
        for text in [
            "@bastion",
            "jump@",
            "a b@bastion",
            "my host",
            "bastion:",
            "bastion:0",
            "bastion:65536",
            "bastion:abc",
            "bastion:+22",
            "bastion:22/path",
            "bastion/path",
            "ssh://bastion",
            "[::1",
            "[bastion]",
            "[::1]22",
            "[::1]:",
            &format!("{}@bastion", "a".repeat(65)),
        ] {
            assert_eq!(
                parse_jump_host(text),
                Err(Problem::JumpHostInvalid),
                "{text:?}"
            );
        }
        assert_eq!(Problem::JumpHostInvalid.to_string(), "Use user@host:port");
    }

    // ---- A whole profile. ----

    #[test]
    fn a_good_profile_can_be_saved() {
        let result = validate_profile(&valid(), &[], &no_file);
        assert_eq!(result, Validation::default());
        assert!(result.can_save());
    }

    #[test]
    fn each_bad_field_is_reported_against_its_own_box() {
        let mut profile = valid();
        profile.name = String::new();
        profile.drive = letter('C');
        let sftp_fields = sftp(&mut profile);
        sftp_fields.host = "my host".to_owned();
        sftp_fields.port = 0;
        sftp_fields.username = String::new();
        sftp_fields.auth = SftpAuth::KeyFile {
            path: PathBuf::from("missing"),
            remember: false,
        };
        sftp_fields.remote_path = "www".to_owned();
        sftp_fields.cache_ttl_secs = 61;
        sftp_fields.keepalive_secs = 1;
        sftp_fields.jump = Some(JumpHost {
            host: "my jump".to_owned(),
            port: 22,
            username: None,
            auth: JumpAuth::KeyFile {
                path: PathBuf::from("jump.pub"),
            },
        });

        let result = validate_profile(&profile, &[], &no_file);

        let expected = [
            (Field::Name, Problem::NameRequired),
            (Field::Drive, Problem::DriveInvalid),
            (Field::Host, Problem::HostInvalid),
            (Field::Port, Problem::PortOutOfRange),
            (Field::User, Problem::UserRequired),
            (Field::KeyFile, Problem::KeyFileMissing),
            (Field::RemotePath, Problem::RemotePathInvalid),
            (Field::Cache, Problem::CacheOutOfRange),
            (Field::KeepAlive, Problem::KeepAliveOutOfRange),
            (Field::JumpHost, Problem::JumpHostInvalid),
            (Field::JumpKeyFile, Problem::KeyFilePublic),
        ];
        assert_eq!(result.errors.len(), expected.len(), "{result:?}");
        for (field, problem) in expected {
            assert_eq!(result.problem(field), Some(problem), "{field:?}");
        }
        assert!(!result.can_save());
    }

    #[test]
    fn a_profile_does_not_clash_with_itself() {
        let mut profile = valid();
        profile.drive = letter('Z');
        let saved = [profile.clone()];

        let result = validate_profile(&profile, &saved, &no_file);

        assert_eq!(result, Validation::default(), "same id, name and letter");
    }

    #[test]
    fn a_second_profile_cannot_take_the_name_of_another() {
        let saved = [valid()];
        let mut profile = valid();
        profile.id = Uuid::new_v4();
        profile.name = "production".to_owned();

        let result = validate_profile(&profile, &saved, &no_file);

        assert_eq!(result.problem(Field::Name), Some(Problem::NameTaken));
    }

    #[test]
    fn a_key_file_is_only_looked_at_when_the_method_uses_it() {
        let panics = |_: &Path| -> io::Result<Secret<Vec<u8>>> { panic!("read") };
        for auth in [
            SftpAuth::Password { remember: true },
            SftpAuth::Agent,
            SftpAuth::Ask,
        ] {
            let mut profile = valid();
            sftp(&mut profile).auth = auth;
            assert!(validate_profile(&profile, &[], &panics).can_save());
        }
    }

    #[test]
    fn the_key_method_needs_a_good_file_for_the_profile_and_the_jump_host() {
        let mut profile = valid();
        let fields = sftp(&mut profile);
        fields.auth = SftpAuth::KeyFile {
            path: PathBuf::from("id_ed25519"),
            remember: true,
        };
        fields.jump = Some(JumpHost {
            host: "bastion".to_owned(),
            port: 22,
            username: Some("jump".to_owned()),
            auth: JumpAuth::KeyFile {
                path: PathBuf::from("jump_key"),
            },
        });

        assert!(validate_profile(&profile, &[], &key_file).can_save());

        let result = validate_profile(&profile, &[], &no_file);
        assert_eq!(
            result.problem(Field::KeyFile),
            Some(Problem::KeyFileMissing)
        );
        assert_eq!(
            result.problem(Field::JumpKeyFile),
            Some(Problem::KeyFileMissing)
        );
    }

    #[test]
    fn a_jump_host_in_a_profile_follows_the_same_rule() {
        for (host, port, username) in [
            ("sftp://bastion", 22, None),
            ("bastion", 0, None),
            ("bastion", 22, Some("a b")),
            ("bastion", 22, Some("")),
        ] {
            let mut profile = valid();
            sftp(&mut profile).jump = Some(JumpHost {
                host: host.to_owned(),
                port,
                username: username.map(str::to_owned),
                auth: JumpAuth::Agent,
            });
            let result = validate_profile(&profile, &[], &no_file);
            assert_eq!(
                result.problem(Field::JumpHost),
                Some(Problem::JumpHostInvalid),
                "{host} {port} {username:?}"
            );
        }
    }

    #[test]
    fn the_defaults_of_a_new_profile_are_valid_and_each_gets_its_own_id() {
        let new = || {
            let sftp = SftpProfile::new("nas", "me", SftpAuth::Agent);
            Profile::new("NAS", Protocol::Sftp(sftp))
        };
        let (mut first, mut second) = (new(), new());
        assert_ne!(first.id, second.id);
        assert!(validate_profile(&first, &[], &no_file).can_save());
        assert_eq!(first.drive, DriveChoice::Auto);
        assert!(first.auto_reconnect);
        let fields = sftp(&mut first);
        assert_eq!(fields.port, 22);
        assert_eq!(fields.remote_path, "~");
        assert_eq!(fields.cache_ttl_secs, 5);
        assert_eq!(sftp(&mut second).keepalive_secs, 15);
    }

    // ---- SMB (M3-03). ----

    fn share(auth: SmbAuth) -> Profile {
        let smb = SmbProfile::new("nas", "media", "marcin", auth);
        Profile::new("NAS", Protocol::Smb(smb))
    }

    fn smb(profile: &mut Profile) -> &mut SmbProfile {
        let Protocol::Smb(smb) = &mut profile.protocol else {
            panic!("not SMB");
        };
        smb
    }

    #[test]
    fn a_server_is_a_name_or_an_ipv4_address() {
        for server in [
            "nas",
            "NAS-01",
            "nas.corp.example",
            "192.168.1.10",
            "fe80--1.ipv6-literal.net",
            "my_nas",
        ] {
            assert_eq!(check_server(server), Ok(()), "{server:?}");
        }
        for server in [
            "",
            " ",
            "nas host",
            r"\\nas",
            "nas/x",
            "nas:445",
            "10.0.4.256",
            "-nas",
        ] {
            assert_eq!(
                check_server(server),
                Err(Problem::ServerInvalid),
                "{server:?}"
            );
        }
        assert_eq!(
            Problem::ServerInvalid.to_string(),
            "Enter a server name or IP address"
        );
    }

    #[test]
    fn an_ipv6_server_is_asked_for_in_its_literal_net_form() {
        for server in ["::1", "fe80::1", "2001:db8::10"] {
            assert_eq!(check_server(server), Err(Problem::ServerIpv6), "{server:?}");
        }
    }

    #[test]
    fn a_share_is_1_to_80_characters_without_the_forbidden_ones() {
        for share in ["media", "C$", "Team Files", "zdjęcia", &"a".repeat(80)] {
            assert_eq!(check_share(share), Ok(()), "{share:?}");
        }
        assert_eq!(check_share(""), Err(Problem::ShareRequired));
        assert_eq!(check_share("  "), Err(Problem::ShareRequired));
        assert_eq!(check_share(&"ż".repeat(81)), Err(Problem::ShareTooLong));
        for c in [
            '\\', '/', '[', ']', ':', '|', '<', '>', '+', '=', ';', ',', '?', '*', '"', '\n',
        ] {
            let share = format!("a{c}b");
            assert_eq!(
                check_share(&share),
                Err(Problem::ShareForbiddenCharacter),
                "{share:?}"
            );
        }
        assert_eq!(
            Problem::ShareForbiddenCharacter.to_string(),
            r#"A share name cannot contain \ / [ ] : | < > + = ; , ? * ""#
        );
    }

    #[test]
    fn a_sub_path_is_empty_or_folder_names() {
        for sub_path in [
            "",
            "films",
            r"films\2024",
            "films/2024",
            "Nowe filmy",
            "a.b",
            "$x",
        ] {
            assert_eq!(check_sub_path(sub_path), Ok(()), "{sub_path:?}");
        }
        for sub_path in [
            r"\films",
            r"films\",
            r"films\\2024",
            "films//2024",
            ".",
            r"a\..\b",
            r"a\.\b",
            "films.",
            "films ",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a\tb",
        ] {
            assert_eq!(
                check_sub_path(sub_path),
                Err(Problem::SubPathInvalid),
                "{sub_path:?}"
            );
        }
        assert_eq!(
            Problem::SubPathInvalid.to_string(),
            r"Use folder names separated by \, such as films\2024"
        );
    }

    #[test]
    fn an_smb_user_may_have_spaces_and_a_domain_after_an_at() {
        for user in [
            "marcin",
            "Jan Kowalski",
            "me@corp.example",
            "żółw",
            &"a".repeat(104),
        ] {
            assert_eq!(check_smb_user(user), Ok(()), "{user:?}");
        }
        assert_eq!(check_smb_user(""), Err(Problem::UserRequired));
        assert_eq!(check_smb_user(" "), Err(Problem::UserRequired));
        assert_eq!(
            check_smb_user(&"a".repeat(105)),
            Err(Problem::SmbUserTooLong)
        );
        for c in [
            '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '\0',
        ] {
            let user = format!("a{c}b");
            assert_eq!(
                check_smb_user(&user),
                Err(Problem::SmbUserForbiddenCharacter),
                "{user:?}"
            );
        }
    }

    #[test]
    fn a_domain_is_empty_or_a_name() {
        for domain in [
            "",
            "CORP",
            "corp.example",
            ".",
            "my-domain_1",
            &"a".repeat(255),
        ] {
            assert_eq!(check_domain(domain), Ok(()), "{domain:?}");
        }
        for domain in [
            "CO RP",
            r"CORP\",
            "a/b",
            "me@corp",
            "a:b",
            "a*b",
            &"a".repeat(256),
        ] {
            assert_eq!(
                check_domain(domain),
                Err(Problem::DomainInvalid),
                "{domain:?}"
            );
        }
    }

    #[test]
    fn a_whole_smb_profile_is_checked_field_by_field() {
        assert_eq!(
            validate_profile(&share(SmbAuth::Ask), &[], &no_file),
            Validation::default()
        );

        let mut profile = share(SmbAuth::Password { remember: true });
        let fields = smb(&mut profile);
        fields.server = "::1".to_owned();
        fields.share = String::new();
        fields.sub_path = r"a\..".to_owned();
        fields.domain = "CO RP".to_owned();
        fields.username = String::new();
        let marked: Vec<_> = validate_profile(&profile, &[], &no_file)
            .errors
            .into_iter()
            .map(|error| (error.field, error.problem))
            .collect();
        assert_eq!(
            marked,
            [
                (Field::Server, Problem::ServerIpv6),
                (Field::Share, Problem::ShareRequired),
                (Field::SubPath, Problem::SubPathInvalid),
                (Field::User, Problem::UserRequired),
                (Field::Domain, Problem::DomainInvalid),
            ]
        );
    }

    #[test]
    fn a_domain_in_both_boxes_is_marked_on_the_domain() {
        let mut profile = share(SmbAuth::Ask);
        smb(&mut profile).username = "me@corp.example".to_owned();
        assert!(validate_profile(&profile, &[], &no_file).can_save());

        smb(&mut profile).domain = "CORP".to_owned();
        let result = validate_profile(&profile, &[], &no_file);
        assert_eq!(result.problem(Field::Domain), Some(Problem::DomainTwice));
        assert_eq!(result.problem(Field::User), None);
    }

    /// Single sign-on sends no user, so a blank one, or anything left from another method, is fine.
    #[test]
    fn the_windows_user_needs_no_user_or_domain() {
        let mut profile = share(SmbAuth::WindowsUser);
        smb(&mut profile).username = String::new();
        smb(&mut profile).domain = "CO RP".to_owned();
        assert!(validate_profile(&profile, &[], &no_file).can_save());
    }

    #[test]
    fn an_smb_profile_shares_the_name_and_drive_rules() {
        let mut other = valid();
        other.drive = letter('M');
        let mut profile = share(SmbAuth::Ask);
        profile.name = "production".to_owned();
        profile.drive = letter('M');

        let result = validate_profile(&profile, &[other], &no_file);
        assert_eq!(result.problem(Field::Name), Some(Problem::NameTaken));
        assert_eq!(
            result.drive_warning,
            Some(DriveWarning::AlsoUsedBy("Production".to_owned()))
        );
    }

    #[test]
    fn the_unc_and_account_are_put_together_as_windows_writes_them() {
        let mut profile = share(SmbAuth::Ask);
        let fields = smb(&mut profile);
        assert_eq!(fields.unc(), r"\\nas\media");
        assert_eq!(fields.unc_with_sub_path(), r"\\nas\media");
        assert_eq!(fields.account(), "marcin");

        fields.sub_path = "films/2024".to_owned();
        fields.domain = "CORP".to_owned();
        assert_eq!(fields.unc_with_sub_path(), r"\\nas\media\films\2024");
        assert_eq!(fields.account(), r"CORP\marcin");
    }

    // NFS (M4-05)

    fn export() -> Profile {
        Profile::new(
            "Data",
            Protocol::Nfs(NfsProfile::new("nas", "/volume1/data")),
        )
    }

    fn nfs(profile: &mut Profile) -> &mut NfsProfile {
        let Protocol::Nfs(nfs) = &mut profile.protocol else {
            panic!("not NFS");
        };
        nfs
    }

    #[test]
    fn an_nfs_server_follows_the_host_rule_with_ipv6_allowed() {
        for server in ["nas", "nas.local", "10.0.4.21", "fe80::1", "::1"] {
            assert_eq!(check_nfs_server(server), Ok(()), "{server:?}");
        }
        for server in ["", "n as", "nas:/x", "10.0.4.256", "[::1]"] {
            assert_eq!(
                check_nfs_server(server),
                Err(Problem::ServerInvalid),
                "{server:?}"
            );
        }
    }

    #[test]
    fn an_export_is_an_absolute_path_mount_can_take() {
        for export in ["/", "/srv", "/volume1/my data", "/srv/nfs/", "//srv"] {
            assert_eq!(check_export(export), Ok(()), "{export:?}");
        }
        let too_long = format!("/{}", "a".repeat(1024));
        for export in [
            "",
            "srv",
            "~/data",
            "/srv/../etc",
            "/srv/./x",
            "/a\0b",
            too_long.as_str(),
        ] {
            assert_eq!(
                check_export(export),
                Err(Problem::ExportInvalid),
                "{export:?}"
            );
        }
        assert_eq!(check_export(&format!("/{}", "a".repeat(1023))), Ok(()));
    }

    #[test]
    fn a_uid_or_gid_is_any_32_bit_number() {
        assert_eq!(parse_id("0"), Ok(0));
        assert_eq!(parse_id(" 1000 "), Ok(1000));
        assert_eq!(parse_id("4294967295"), Ok(u32::MAX));
        for text in ["", "-1", "+1", "4294967296", "1e3", "abc"] {
            assert_eq!(parse_id(text), Err(Problem::IdOutOfRange), "{text:?}");
        }
    }

    #[test]
    fn an_empty_nfs_port_asks_the_portmapper() {
        assert_eq!(parse_nfs_port(""), Ok(None));
        assert_eq!(parse_nfs_port("  "), Ok(None));
        assert_eq!(parse_nfs_port("0"), Ok(None));
        assert_eq!(parse_nfs_port("2049"), Ok(Some(2049)));
        for text in ["65536", "-1", "+2049", "nfs"] {
            assert_eq!(
                parse_nfs_port(text),
                Err(Problem::NfsPortInvalid),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_whole_nfs_profile_is_checked_field_by_field() {
        assert_eq!(
            validate_profile(&export(), &[], &no_file),
            Validation::default()
        );

        let mut profile = export();
        let fields = nfs(&mut profile);
        fields.server = "n as".to_owned();
        fields.export = "data".to_owned();
        fields.nfs_port = Some(0);
        fields.mount_port = Some(0);
        fields.cache_ttl_secs = 61;
        let marked: Vec<_> = validate_profile(&profile, &[], &no_file)
            .errors
            .into_iter()
            .map(|error| (error.field, error.problem))
            .collect();
        assert_eq!(
            marked,
            [
                (Field::Server, Problem::ServerInvalid),
                (Field::Export, Problem::ExportInvalid),
                (Field::NfsPort, Problem::NfsPortInvalid),
                (Field::MountPort, Problem::NfsPortInvalid),
                (Field::Cache, Problem::CacheOutOfRange),
            ]
        );
    }

    #[test]
    fn an_nfs_address_is_written_as_mount_takes_it() {
        let mut profile = export();
        assert_eq!(nfs(&mut profile).address(), "nas:/volume1/data");
        nfs(&mut profile).server = "fe80::1".to_owned();
        assert_eq!(nfs(&mut profile).address(), "[fe80::1]:/volume1/data");
    }
}
