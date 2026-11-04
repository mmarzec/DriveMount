// SPDX-License-Identifier: GPL-3.0-or-later

//! The OpenSSH `known_hosts` file (spec §6.3, FR-SSH-08 to FR-SSH-10).
//!
//! Trust is shared with `ssh.exe`, so this module gives the answers OpenSSH gives. The rules below
//! are compared with real `ssh` in `tests/known_hosts_openssh.rs`, so far with OpenSSH 9.5 on Windows:
//!
//! - A server on another port is looked up as `[host]:port`. If nothing at all is on file under that
//!   name, a line for plain `host` still counts when it holds exactly the key presented; a different
//!   key under plain `host` does not make it [`HostKeyStatus::Changed`].
//! - Any other key for the name makes a key that is not on file [`HostKeyStatus::Changed`], whatever
//!   its type. A connector must therefore prefer the key types on file
//!   ([`KnownHosts::known_key_types`]), or a server that also has a newer key type looks changed.
//! - `@cert-authority` lines are ignored: host certificates are not supported.
//! - Lines OpenSSH would skip are skipped.
//!
//! One deliberate difference: a `@revoked` line for either name always wins. OpenSSH does not look
//! at plain `host` once the key was found under `[host]:port`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Longer than any append should take, and short enough to report while a user waits.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(20);

/// The size of an HMAC-SHA1 salt and digest. OpenSSH skips hashed entries of any other size.
const SHA1_LEN: usize = 20;
const HASH_PREFIX: &str = "|1|";
const DEFAULT_PORT: u16 = 22;

/// Why a `known_hosts` operation failed. The caller must treat any of these as *not trusted*.
#[derive(Debug, Error)]
pub enum KnownHostsError {
    /// The key is not in the SSH wire format.
    #[error("invalid host key")]
    InvalidKey,
    /// The host name holds characters that would change the meaning of the line.
    #[error("host name {0:?} cannot be saved in known_hosts")]
    InvalidHost(String),
    /// The file, its directory or its lock file could not be read or written.
    #[error("known_hosts file {}: {source}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// Another append held the lock for too long.
    #[error("{} stayed locked", path.display())]
    Locked {
        /// The lock file.
        path: PathBuf,
    },
    /// No random salt was available for a hashed entry.
    #[error("no random salt: {0}")]
    Random(String),
}

/// A server's public key in the SSH wire format, as `known_hosts` stores it in base64.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    blob: Vec<u8>,
    key_type: String,
}

impl HostKey {
    /// Takes the wire-format key blob. Only the type at its start is checked.
    ///
    /// # Errors
    ///
    /// [`KnownHostsError::InvalidKey`] if the blob does not start with a key type.
    pub fn from_wire(blob: impl Into<Vec<u8>>) -> Result<Self, KnownHostsError> {
        let blob = blob.into();
        let key_type = wire_key_type(&blob)
            .ok_or(KnownHostsError::InvalidKey)?
            .to_owned();
        Ok(Self { blob, key_type })
    }

    /// The key type, such as `ssh-ed25519`.
    #[must_use]
    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    /// The fingerprint as `ssh-keygen -l` prints it: `SHA256:` and unpadded base64.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(&self.blob);
        format!("SHA256:{}", STANDARD_NO_PAD.encode(digest))
    }
}

/// The first SSH string of a key blob: a big-endian `u32` length and that many printable bytes.
fn wire_key_type(blob: &[u8]) -> Option<&str> {
    let (length, rest) = blob.split_first_chunk::<4>()?;
    let name = rest.get(..usize::try_from(u32::from_be_bytes(*length)).ok()?)?;
    if name.is_empty() || !name.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    std::str::from_utf8(name).ok()
}

/// What the file says about a key a server presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is on file for this host.
    Known,
    /// Nothing relevant is on file. Ask the user (FR-SSH-09).
    Unknown,
    /// Other keys are on file for this host. Refuse (FR-SSH-10).
    Changed {
        /// The 1-based line of the other key; the last one, if several, as `ssh` reports it.
        line: u32,
        /// That key's fingerprint.
        old_fingerprint: String,
    },
    /// A `@revoked` line names this key. Refuse.
    Revoked {
        /// The 1-based line of the `@revoked` entry.
        line: u32,
    },
}

/// The host column of a line.
#[derive(Debug)]
enum HostField {
    /// A comma-separated list of patterns, in lower case.
    Patterns(String),
    /// `|1|salt|hash`, naming one exact host.
    Hashed {
        salt: [u8; SHA1_LEN],
        hash: [u8; SHA1_LEN],
    },
}

impl HostField {
    fn parse(field: &str) -> Option<Self> {
        if !field.starts_with('|') {
            return Some(Self::Patterns(field.to_ascii_lowercase()));
        }
        let (salt, hash) = field.strip_prefix(HASH_PREFIX)?.split_once('|')?;
        Some(Self::Hashed {
            salt: STANDARD.decode(salt).ok()?.try_into().ok()?,
            hash: STANDARD.decode(hash).ok()?.try_into().ok()?,
        })
    }

    /// `name` is in lower case, as [`lookup_names`] gives it.
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Patterns(patterns) => patterns_match(patterns, name),
            Self::Hashed { salt, hash } => hash_name(salt, name).is_some_and(|h| h == *hash),
        }
    }
}

fn hash_name(salt: &[u8], name: &str) -> Option<[u8; SHA1_LEN]> {
    let mut mac = Hmac::<Sha1>::new_from_slice(salt).ok()?;
    mac.update(name.as_bytes());
    Some(mac.finalize().into_bytes().into())
}

/// OpenSSH's `match_hostname`: at least one pattern matches and no negated one does.
fn patterns_match(patterns: &str, name: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split(',') {
        match pattern.strip_prefix('!') {
            Some(negated) if glob_match(negated.as_bytes(), name.as_bytes()) => return false,
            Some(_) => {}
            None => matched |= glob_match(pattern.as_bytes(), name.as_bytes()),
        }
    }
    matched
}

/// `*` matches any run of characters and `?` any one. Nothing else is special.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    // Where to resume after the last `*`: the pattern after it, and the text it now covers up to.
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p + 1, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((after_star, covered)) = star else {
                    return false;
                };
                star = Some((after_star, covered + 1));
                p = after_star;
                t = covered + 1;
            }
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// The name to look up first, and the plain name to fall back on when the port is not 22.
fn lookup_names(host: &str, port: u16) -> (String, Option<String>) {
    let host = host.to_ascii_lowercase();
    if port == DEFAULT_PORT {
        (host, None)
    } else {
        (format!("[{host}]:{port}"), Some(host))
    }
}

#[derive(Debug)]
struct Entry {
    line: u32,
    revoked: bool,
    hosts: HostField,
    key: HostKey,
}

/// The parsed contents of a `known_hosts` file.
#[derive(Debug, Default)]
pub struct KnownHosts {
    entries: Vec<Entry>,
    uses_hashing: bool,
}

impl KnownHosts {
    /// Parses a file's contents. Lines OpenSSH would skip are skipped, so this cannot fail.
    #[must_use]
    pub fn parse(contents: &[u8]) -> Self {
        // The Windows build of OpenSSH skips a UTF-8 byte order mark, which editors like to add.
        let contents = contents.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(contents);
        let mut parsed = Self::default();
        for (line, text) in (1u32..).zip(contents.split(|&b| b == b'\n')) {
            parsed.parse_line(line, &String::from_utf8_lossy(text));
        }
        parsed
    }

    fn parse_line(&mut self, line: u32, text: &str) {
        let mut fields = text
            .trim_end_matches('\r')
            .split([' ', '\t'])
            .filter(|field| !field.is_empty());
        let Some(mut first) = fields.next().filter(|field| !field.starts_with('#')) else {
            return;
        };
        let mut revoked = false;
        let mut cert_authority = false;
        if first.starts_with('@') {
            revoked = first == "@revoked";
            cert_authority = first == "@cert-authority";
            if !revoked && !cert_authority {
                return;
            }
            let Some(host) = fields.next() else {
                return;
            };
            first = host;
        }
        let (Some(hosts), Some(key_type), Some(key)) =
            (HostField::parse(first), fields.next(), fields.next())
        else {
            return;
        };
        let Some(key) = STANDARD
            .decode(key)
            .ok()
            .and_then(|blob| HostKey::from_wire(blob).ok())
            .filter(|key| key.key_type() == key_type)
        else {
            return;
        };
        self.uses_hashing |= matches!(hosts, HostField::Hashed { .. });
        if !cert_authority {
            self.entries.push(Entry {
                line,
                revoked,
                hosts,
                key,
            });
        }
    }

    /// Reads a file. A file that does not exist is empty.
    ///
    /// # Errors
    ///
    /// [`KnownHostsError::Io`] if the file exists but cannot be read.
    pub fn load(path: &Path) -> Result<Self, KnownHostsError> {
        match fs::read(path) {
            Ok(contents) => Ok(Self::parse(&contents)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(io_error(path, source)),
        }
    }

    /// Whether any entry uses a hashed name, which new lines then follow.
    #[must_use]
    pub fn uses_hashing(&self) -> bool {
        self.uses_hashing
    }

    /// Checks the key a server presented.
    #[must_use]
    pub fn check(&self, host: &str, port: u16, key: &HostKey) -> HostKeyStatus {
        let (name, plain) = lookup_names(host, port);
        let plain = plain.as_deref();

        let revoked = self
            .entries_for(&name, true)
            .chain(plain.into_iter().flat_map(|p| self.entries_for(p, true)))
            .find(|entry| entry.key == *key);
        if let Some(entry) = revoked {
            return HostKeyStatus::Revoked { line: entry.line };
        }

        if self.entries_for(&name, false).any(|e| e.key == *key) {
            return HostKeyStatus::Known;
        }
        if let Some(entry) = self.entries_for(&name, false).last() {
            return HostKeyStatus::Changed {
                line: entry.line,
                old_fingerprint: entry.key.fingerprint(),
            };
        }
        if plain.is_some_and(|p| self.entries_for(p, false).any(|e| e.key == *key)) {
            return HostKeyStatus::Known;
        }
        HostKeyStatus::Unknown
    }

    fn entries_for<'a>(&'a self, name: &'a str, revoked: bool) -> impl Iterator<Item = &'a Entry> {
        self.entries
            .iter()
            .filter(move |entry| entry.revoked == revoked && entry.hosts.matches(name))
    }

    /// The key types on file for this host, `[host]:port` lines first. `ssh` asks the server for these first,
    /// and a connector must too: see the module documentation.
    #[must_use]
    pub fn known_key_types(&self, host: &str, port: u16) -> Vec<String> {
        let (name, plain) = lookup_names(host, port);
        let plain = plain.as_deref();
        let mut types: Vec<String> = Vec::new();
        for entry in self
            .entries_for(&name, false)
            .chain(plain.into_iter().flat_map(|p| self.entries_for(p, false)))
        {
            if !types.iter().any(|t| t == entry.key.key_type()) {
                types.push(entry.key.key_type().to_owned());
            }
        }
        types
    }
}

/// *Trust and save* (FR-SSH-09): appends a line for the key, if the file still does not know it.
///
/// The user's answer may be minutes old, so the file is read again under the lock. The line is
/// written only if that says [`HostKeyStatus::Unknown`], and the status is returned either way: on
/// `Changed` or `Revoked` the caller must still refuse the connection.
///
/// The directory is created if missing. The file is only ever appended to. A new line is hashed if
/// the file already holds a hashed entry (spec §6.3). `known_hosts.lock` beside the file stays
/// behind, empty: removing it safely would need a second lock. The operating system releases the
/// lock if the process dies, so a crash cannot leave it stuck.
///
/// This blocks on file I/O, for up to 5 s on the lock. Call it with `spawn_blocking`.
///
/// # Errors
///
/// See [`KnownHostsError`]. On any error the key must not be reported as saved.
pub fn append_if_unknown(
    path: &Path,
    host: &str,
    port: u16,
    key: &HostKey,
) -> Result<HostKeyStatus, KnownHostsError> {
    if !can_be_saved(host) {
        return Err(KnownHostsError::InvalidHost(host.to_owned()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let _lock = acquire_lock(&path.with_added_extension("lock"), LOCK_TIMEOUT)?;

    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(io_error(path, source)),
    };
    let existing = KnownHosts::parse(&contents);
    let status = existing.check(host, port, key);
    if status != HostKeyStatus::Unknown {
        return Ok(status);
    }

    let (name, _) = lookup_names(host, port);
    let host_field = if existing.uses_hashing() {
        hashed_field(&name)?
    } else {
        name
    };
    // One write, so nothing can land between the separator and the line.
    let mut text = String::new();
    if contents.last().is_some_and(|&last| last != b'\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "{host_field} {} {}\n",
        key.key_type(),
        STANDARD.encode(&key.blob)
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(path, source))?;
    Ok(status)
}

/// Refuses anything that could add a line or a pattern: a profile's host must not be able to make
/// the file trust a key for another host.
fn can_be_saved(host: &str) -> bool {
    !host.is_empty()
        && !host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || ",*?!|#@[]".contains(c))
}

fn hashed_field(name: &str) -> Result<String, KnownHostsError> {
    let mut salt = [0; SHA1_LEN];
    getrandom::fill(&mut salt).map_err(|error| KnownHostsError::Random(error.to_string()))?;
    let hash = hash_name(&salt, name).ok_or(KnownHostsError::InvalidKey)?;
    Ok(format!(
        "{HASH_PREFIX}{}|{}",
        STANDARD.encode(salt),
        STANDARD.encode(hash)
    ))
}

/// The lock is held until the returned file is dropped.
fn acquire_lock(path: &Path, timeout: Duration) -> Result<File, KnownHostsError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(source)) => return Err(io_error(path, source)),
            Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                return Err(KnownHostsError::Locked {
                    path: path.to_owned(),
                });
            }
            Err(TryLockError::WouldBlock) => thread::sleep(LOCK_POLL),
        }
    }
}

fn io_error(path: &Path, source: io::Error) -> KnownHostsError {
    KnownHostsError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(key_type: &str, body: &[u8]) -> Vec<u8> {
        let length = u32::try_from(key_type.len()).unwrap();
        let mut blob = length.to_be_bytes().to_vec();
        blob.extend_from_slice(key_type.as_bytes());
        blob.extend_from_slice(body);
        blob
    }

    #[test]
    fn key_type_comes_from_the_blob() {
        let key = HostKey::from_wire(blob("ssh-ed25519", b"body")).unwrap();
        assert_eq!(key.key_type(), "ssh-ed25519");
    }

    #[test]
    fn blobs_without_a_key_type_are_refused() {
        for bad in [
            &b""[..],
            &[0, 0, 0],
            &[0, 0, 0, 0],
            &[0, 0, 0, 9, b's'],
            &[0xFF, 0xFF, 0xFF, 0xFF, b's'],
            &[0, 0, 0, 2, b'a', b' '],
            &[0, 0, 0, 2, 0xC3, 0xA9],
        ] {
            assert!(
                matches!(HostKey::from_wire(bad), Err(KnownHostsError::InvalidKey)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn glob_rules() {
        for (pattern, text, want) in [
            ("abc", "abc", true),
            ("abc", "abd", false),
            ("abc", "ab", false),
            ("ab", "abc", false),
            ("", "", true),
            ("", "a", false),
            ("*", "", true),
            ("*", "anything", true),
            ("a*", "a", true),
            ("*c", "abc", true),
            ("*c", "abd", false),
            ("a*c", "abbbc", true),
            ("a*c", "abbbd", false),
            ("a*b*c", "a1b2b3c", true),
            ("a*b*c", "a1c2b", false),
            ("**", "x", true),
            ("?", "a", true),
            ("?", "", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*?", "", false),
            ("[a]:1", "[a]:1", true),
            ("[ab]", "a", false),
        ] {
            assert_eq!(
                glob_match(pattern.as_bytes(), text.as_bytes()),
                want,
                "{pattern} against {text}"
            );
        }
    }

    #[test]
    fn pattern_list_rules() {
        for (patterns, name, want) in [
            ("a,b", "b", true),
            ("a,b", "c", false),
            ("*.x,!no.x", "yes.x", true),
            ("*.x,!no.x", "no.x", false),
            ("!no.x,*.x", "no.x", false),
            ("!no.x", "other", false),
            ("!no.x", "no.x", false),
            ("", "a", false),
        ] {
            assert_eq!(
                patterns_match(patterns, name),
                want,
                "{patterns} against {name}"
            );
        }
    }

    #[test]
    fn hashed_name_matches_the_rfc_2202_vector() {
        // RFC 2202 test case 2 is keyed with a short ASCII key, so it checks the HMAC wiring.
        let digest = hash_name(b"Jefe", "what do ya want for nothing?").unwrap();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn hosts_that_could_rewrite_the_file_are_refused() {
        for bad in [
            "",
            "a b",
            "a\tb",
            "a\nevil.example ssh-ed25519 AAAA",
            "a\r",
            "a,b",
            "*",
            "a?",
            "!a",
            "|1|x|y",
            "#a",
            "@revoked",
            "[a]:1",
        ] {
            assert!(!can_be_saved(bad), "{bad:?}");
        }
        for good in ["server", "10.0.4.21", "::1", "fe80::1%eth0", "zażółć.test"] {
            assert!(can_be_saved(good), "{good:?}");
        }
    }

    #[test]
    fn a_held_lock_times_out_and_a_released_one_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts.lock");
        let held = acquire_lock(&path, Duration::ZERO).unwrap();
        assert!(matches!(
            acquire_lock(&path, Duration::from_millis(50)),
            Err(KnownHostsError::Locked { .. })
        ));
        drop(held);
        acquire_lock(&path, Duration::ZERO).unwrap();
    }
}
