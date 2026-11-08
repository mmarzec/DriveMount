// SPDX-License-Identifier: GPL-3.0-or-later

//! What the profile dialog's boxes mean (spec §7.2, §7.3): from a profile to the boxes and back, and
//! what is wrong with what is typed. The rules themselves are `fsm_core::profile`'s. Nothing here
//! touches the window, or a file: a key file is looked at on another thread, and its answer is passed
//! in as [`KeyChecks`] (NFR-PERF-10).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fsm_core::profile::{
    DEFAULT_CACHE_TTL_SECS, DEFAULT_KEEPALIVE_SECS, DEFAULT_PORT, DEFAULT_REMOTE_PATH, DriveChoice,
    DriveWarning, Field, FieldError, JumpAuth, JumpHost, Problem, Profile, Protocol, SftpAuth,
    SftpProfile, parse_cache_secs, parse_jump_host, parse_keepalive_secs, parse_pasted_host,
    parse_port, validate_profile,
};
use fsm_vfs::{DriveLetter, Secret, SecretSlot};
use fsm_win::credentials::MAX_SECRET_UNITS;
use uuid::Uuid;

use super::generated::{AuthChoice, DialogForm};

const JUMP_BY_AGENT: i32 = 0;
const JUMP_BY_KEY_FILE: i32 = 1;
const JUMP_BY_PASSWORD: i32 = 2;

/// Stands in for a key file while checking the rest: the key files have their own answers.
const ANY_PRIVATE_KEY: &[u8] = b"-----BEGIN OPENSSH PRIVATE KEY-----\n";

/// What the dialog is editing.
#[derive(Debug, Clone)]
pub(super) struct Base {
    /// A new id for a new profile, which stays through Test and failed Saves.
    pub(super) id: Uuid,
    /// The profile as saved. `None` for a new one.
    pub(super) original: Option<Profile>,
    /// Connection fields cannot change (FR-PROF-08).
    pub(super) locked: bool,
    /// What each line of the Drive dropdown means.
    pub(super) drives: Vec<DriveChoice>,
}

impl Base {
    pub(super) fn new(original: Option<Profile>, locked: bool) -> Self {
        let current = original
            .as_ref()
            .map_or(DriveChoice::Auto, |profile| profile.drive);
        Self {
            id: original
                .as_ref()
                .map_or_else(Uuid::new_v4, |profile| profile.id),
            drives: drive_choices(current),
            original,
            locked,
        }
    }
}

/// Auto, then D to Z. A letter from the file that is not allowed is listed too, so that the box can
/// show what is there and the rule can mark it.
fn drive_choices(current: DriveChoice) -> Vec<DriveChoice> {
    let mut choices = vec![DriveChoice::Auto];
    if let DriveChoice::Letter(letter) = current
        && letter.as_char() < 'D'
    {
        choices.push(current);
    }
    choices.extend(
        ('D'..='Z')
            .filter_map(DriveLetter::new)
            .map(DriveChoice::Letter),
    );
    choices
}

/// The answers about key files so far. No answer means it is still being looked at.
#[derive(Debug, Default)]
pub(super) struct KeyChecks(HashMap<PathBuf, Result<(), Problem>>);

impl KeyChecks {
    pub(super) fn answer(&mut self, path: PathBuf, result: Result<(), Problem>) {
        self.0.insert(path, result);
    }

    pub(super) fn forget(&mut self, path: &Path) {
        self.0.remove(path);
    }
}

/// The boxes of a new profile.
pub(super) fn new_form() -> DialogForm {
    DialogForm {
        port: DEFAULT_PORT.to_string().into(),
        remote_path: DEFAULT_REMOTE_PATH.into(),
        auth: AuthChoice::KeyFile,
        remember_password: true,
        remember_passphrase: true,
        auto_reconnect: true,
        cache: DEFAULT_CACHE_TTL_SECS.to_string().into(),
        keep_alive: DEFAULT_KEEPALIVE_SECS.to_string().into(),
        ..DialogForm::default()
    }
}

/// The boxes of a saved profile. A secret is never put in a box.
pub(super) fn form_of(profile: &Profile, drives: &[DriveChoice]) -> DialogForm {
    let Protocol::Sftp(sftp) = &profile.protocol;
    let mut form = DialogForm {
        name: (&profile.name).into(),
        host: (&sftp.host).into(),
        port: sftp.port.to_string().into(),
        user: (&sftp.username).into(),
        remote_path: (&sftp.remote_path).into(),
        drive_index: drives
            .iter()
            .position(|drive| *drive == profile.drive)
            .and_then(|place| i32::try_from(place).ok())
            .unwrap_or(0),
        mount_on_start: profile.mount_on_start,
        auto_reconnect: profile.auto_reconnect,
        read_only: profile.read_only,
        cache: sftp.cache_ttl_secs.to_string().into(),
        keep_alive: sftp.keepalive_secs.to_string().into(),
        remember_password: true,
        remember_passphrase: true,
        ..DialogForm::default()
    };
    match &sftp.auth {
        SftpAuth::Password { remember } => {
            form.auth = AuthChoice::Password;
            form.remember_password = *remember;
        }
        SftpAuth::KeyFile { path, remember } => {
            form.auth = AuthChoice::KeyFile;
            form.key_file = path.display().to_string().into();
            form.remember_passphrase = *remember;
        }
        SftpAuth::Agent => form.auth = AuthChoice::Agent,
        SftpAuth::Ask => form.auth = AuthChoice::Ask,
    }
    if let Some(jump) = &sftp.jump {
        form.jump_host = jump_text(jump).into();
        match &jump.auth {
            JumpAuth::Agent => form.jump_auth_index = JUMP_BY_AGENT,
            JumpAuth::KeyFile { path } => {
                form.jump_auth_index = JUMP_BY_KEY_FILE;
                form.jump_key_file = path.display().to_string().into();
            }
            JumpAuth::Password => form.jump_auth_index = JUMP_BY_PASSWORD,
        }
    }
    form
}

/// `[user@]host[:port]`, as the Jump host box takes it, with no port if it is the usual one.
pub(super) fn jump_text(jump: &JumpHost) -> String {
    let user = jump
        .username
        .as_ref()
        .map_or_else(String::new, |user| format!("{user}@"));
    let host = &jump.host;
    if jump.port == DEFAULT_PORT {
        format!("{user}{host}")
    } else if host.contains(':') {
        format!("{user}[{host}]:{}", jump.port)
    } else {
        format!("{user}{host}:{}", jump.port)
    }
}

/// What the boxes add up to.
#[derive(Debug)]
pub(super) struct Checked {
    /// The profile the boxes describe, with the default in place of anything that makes no sense.
    pub(super) profile: Profile,
    /// At most one for each box (FR-PROF-04).
    pub(super) errors: Vec<FieldError>,
    /// On the Drive box.
    pub(super) drive_warning: Option<DriveWarning>,
    /// Key files with no answer yet, which hold up Save and Test but mark nothing.
    pub(super) pending: Vec<PathBuf>,
    /// More than Credential Manager holds, for a password that is to be remembered.
    pub(super) password_too_long: bool,
    /// The same for the jump host's.
    pub(super) jump_password_too_long: bool,
}

impl Checked {
    /// Whether Save is enabled (FR-PROF-04), as far as the boxes go. Warnings do not count.
    pub(super) const fn can_save(&self) -> bool {
        self.errors.is_empty()
            && self.pending.is_empty()
            && !self.password_too_long
            && !self.jump_password_too_long
    }

    /// Whether Test connection can run. It does not need a name.
    pub(super) fn can_test(&self) -> bool {
        self.pending.is_empty() && self.errors.iter().all(|error| error.field == Field::Name)
    }

    /// Whether a box in the Advanced section has something wrong, which must not stay hidden.
    pub(super) fn advanced_needs_a_look(&self) -> bool {
        self.jump_password_too_long
            || self.errors.iter().any(|error| {
                matches!(
                    error.field,
                    Field::Cache | Field::KeepAlive | Field::JumpHost | Field::JumpKeyFile
                )
            })
    }
}

/// Checks all the boxes. `others` may include the profile itself.
pub(super) fn check(
    form: &DialogForm,
    base: &Base,
    others: &[Profile],
    keys: &KeyChecks,
) -> Checked {
    let mut errors = Vec::new();
    let profile = build(form, base, &mut errors);
    let validation = validate_profile(&profile, others, &|_| {
        Ok(Secret::new(ANY_PRIVATE_KEY.to_vec()))
    });
    errors.extend(validation.errors);

    let mut pending = Vec::new();
    for (field, path) in key_files(&profile) {
        if errors.iter().any(|error| error.field == field) {
            continue;
        }
        match keys.0.get(path) {
            Some(Ok(())) => {}
            Some(Err(problem)) => errors.push(FieldError {
                field,
                problem: *problem,
            }),
            None => pending.push(path.to_path_buf()),
        }
    }

    let too_long = |text: &str| text.encode_utf16().count() > MAX_SECRET_UNITS;
    let Protocol::Sftp(sftp) = &profile.protocol;
    let password_too_long =
        matches!(sftp.auth, SftpAuth::Password { remember: true }) && too_long(&form.password);
    let jump_password_too_long = matches!(
        sftp.jump.as_ref().map(|jump| &jump.auth),
        Some(JumpAuth::Password)
    ) && too_long(&form.jump_password);

    Checked {
        profile,
        errors,
        drive_warning: validation.drive_warning,
        pending,
        password_too_long,
        jump_password_too_long,
    }
}

/// The key files a profile uses, and the box each is in.
fn key_files(profile: &Profile) -> Vec<(Field, &Path)> {
    let Protocol::Sftp(sftp) = &profile.protocol;
    let mut files = Vec::new();
    if let SftpAuth::KeyFile { path, .. } = &sftp.auth {
        files.push((Field::KeyFile, path.as_path()));
    }
    if let Some(JumpHost {
        auth: JumpAuth::KeyFile { path },
        ..
    }) = &sftp.jump
    {
        files.push((Field::JumpKeyFile, path.as_path()));
    }
    files
}

fn build(form: &DialogForm, base: &Base, errors: &mut Vec<FieldError>) -> Profile {
    let name = form.name.trim().to_owned();
    if base.locked
        && let Some(original) = &base.original
    {
        // Only what FR-PROF-08 leaves editable, so that nothing else can differ by a detail of how it
        // went through the boxes.
        return Profile {
            name,
            mount_on_start: form.mount_on_start,
            auto_reconnect: form.auto_reconnect,
            ..original.clone()
        };
    }

    let mut parsed = |field, result: Result<u64, Problem>, default| {
        result.unwrap_or_else(|problem| {
            errors.push(FieldError { field, problem });
            default
        })
    };
    let port = parsed(
        Field::Port,
        parse_port(&form.port).map(u64::from),
        u64::from(DEFAULT_PORT),
    );
    let cache_ttl_secs = parsed(
        Field::Cache,
        parse_cache_secs(&form.cache),
        DEFAULT_CACHE_TTL_SECS,
    );
    let keepalive_secs = parsed(
        Field::KeepAlive,
        parse_keepalive_secs(&form.keep_alive),
        DEFAULT_KEEPALIVE_SECS,
    );

    let jump = parse_jump_host(&form.jump_host).unwrap_or_else(|problem| {
        errors.push(FieldError {
            field: Field::JumpHost,
            problem,
        });
        None
    });
    let jump = jump.map(|target| JumpHost {
        host: target.host,
        port: target.port,
        username: target.username,
        auth: match form.jump_auth_index {
            JUMP_BY_KEY_FILE => JumpAuth::KeyFile {
                path: PathBuf::from(form.jump_key_file.trim()),
            },
            JUMP_BY_PASSWORD => JumpAuth::Password,
            _ => JumpAuth::Agent,
        },
    });

    let auth = match form.auth {
        AuthChoice::KeyFile => SftpAuth::KeyFile {
            path: PathBuf::from(form.key_file.trim()),
            remember: form.remember_passphrase,
        },
        AuthChoice::Password => SftpAuth::Password {
            remember: form.remember_password,
        },
        AuthChoice::Agent => SftpAuth::Agent,
        AuthChoice::Ask => SftpAuth::Ask,
    };

    Profile {
        id: base.id,
        name,
        protocol: Protocol::Sftp(SftpProfile {
            host: form.host.trim().to_owned(),
            port: u16::try_from(port).unwrap_or(DEFAULT_PORT),
            username: form.user.trim().to_owned(),
            auth,
            remote_path: form.remote_path.trim().to_owned(),
            cache_ttl_secs,
            keepalive_secs,
            jump,
        }),
        drive: usize::try_from(form.drive_index)
            .ok()
            .and_then(|place| base.drives.get(place).copied())
            .unwrap_or(DriveChoice::Auto),
        read_only: form.read_only,
        mount_on_start: form.mount_on_start,
        auto_reconnect: form.auto_reconnect,
    }
}

/// The password boxes that are in use and hold something. Empty boxes keep what is stored, and locked
/// fields have nothing to say (FR-PROF-08).
pub(super) fn typed_secrets(
    form: &DialogForm,
    profile: &Profile,
    locked: bool,
) -> Vec<(SecretSlot, Secret<String>)> {
    let mut typed = Vec::new();
    if locked {
        return typed;
    }
    let Protocol::Sftp(sftp) = &profile.protocol;
    if matches!(sftp.auth, SftpAuth::Password { .. }) && !form.password.is_empty() {
        typed.push((
            SecretSlot::Password,
            Secret::new(form.password.as_str().to_owned()),
        ));
    }
    let jump_auth = sftp.jump.as_ref().map(|jump| &jump.auth);
    if matches!(jump_auth, Some(JumpAuth::Password)) && !form.jump_password.is_empty() {
        typed.push((
            SecretSlot::JumpPassword,
            Secret::new(form.jump_password.as_str().to_owned()),
        ));
    }
    typed
}

/// Whether going from `old` to `new` was more than typing one character. There is no event for a
/// paste, and typing `host:22` must not be split at `host:2` with the last digit landing in the host.
pub(super) fn looks_pasted(old: &str, new: &str) -> bool {
    let old: Vec<char> = old.chars().collect();
    let new: Vec<char> = new.chars().collect();
    let start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    new.len() - start - end > 1
}

/// Splits pasted text into the boxes it belongs in (FR-PROF-05). If it does not make sense as a whole,
/// nothing happens, and the Host rule marks it. Returns whether anything changed.
pub(super) fn fill_from_paste(form: &mut DialogForm, text: &str) -> bool {
    let Some(pasted) = parse_pasted_host(text) else {
        return false;
    };
    let splits = pasted.user.is_some()
        || pasted.port.is_some()
        || pasted.path.is_some()
        || pasted.host != text;
    if !splits {
        return false;
    }
    form.host = pasted.host.into();
    if let Some(user) = pasted.user {
        form.user = user.into();
    }
    if let Some(port) = pasted.port {
        form.port = port.to_string().into();
    }
    if let Some(path) = pasted.path {
        form.remote_path = path.into();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: SftpAuth = SftpAuth::Agent;

    fn saved(name: &str, auth: SftpAuth) -> Profile {
        Profile::new(
            name,
            Protocol::Sftp(SftpProfile::new("host.example", "me", auth)),
        )
    }

    fn sftp(profile: &Profile) -> &SftpProfile {
        let Protocol::Sftp(sftp) = &profile.protocol;
        sftp
    }

    fn all_fine(profile: &Profile) -> KeyChecks {
        let mut keys = KeyChecks::default();
        for (_, path) in key_files(profile) {
            keys.answer(path.to_path_buf(), Ok(()));
        }
        keys
    }

    /// Through the boxes and back.
    fn round_trip(profile: &Profile) -> Checked {
        let base = Base::new(Some(profile.clone()), false);
        let form = form_of(profile, &base.drives);
        check(&form, &base, &[], &all_fine(profile))
    }

    fn valid_new_form() -> DialogForm {
        DialogForm {
            name: "New".into(),
            host: "h".into(),
            user: "u".into(),
            auth: AuthChoice::Agent,
            ..new_form()
        }
    }

    fn problem_of(checked: &Checked, field: Field) -> Option<Problem> {
        checked
            .errors
            .iter()
            .find(|error| error.field == field)
            .map(|error| error.problem)
    }

    #[test]
    fn every_kind_of_profile_comes_back_from_the_boxes_as_it_was() {
        let key = PathBuf::from(r"C:\keys\id_ed25519");
        let mut with_everything = saved(
            "All",
            SftpAuth::KeyFile {
                path: key.clone(),
                remember: false,
            },
        );
        with_everything.drive = DriveChoice::Letter(DriveLetter::new('M').unwrap());
        with_everything.read_only = true;
        with_everything.mount_on_start = true;
        with_everything.auto_reconnect = false;
        let Protocol::Sftp(inner) = &mut with_everything.protocol;
        inner.port = 2222;
        inner.remote_path = "/var/www".to_owned();
        inner.cache_ttl_secs = 0;
        inner.keepalive_secs = 300;
        inner.jump = Some(JumpHost {
            host: "fe80::1".to_owned(),
            port: 2200,
            username: Some("bastion".to_owned()),
            auth: JumpAuth::KeyFile { path: key },
        });

        let mut by_password_jump = saved("B", SftpAuth::Password { remember: false });
        let Protocol::Sftp(inner) = &mut by_password_jump.protocol;
        inner.jump = Some(JumpHost {
            host: "jump".to_owned(),
            port: 22,
            username: None,
            auth: JumpAuth::Password,
        });

        for profile in [
            with_everything,
            by_password_jump,
            saved("C", SftpAuth::Password { remember: true }),
            saved("D", AGENT),
            saved("E", SftpAuth::Ask),
        ] {
            let checked = round_trip(&profile);
            assert_eq!(checked.errors, [], "{}", profile.name);
            assert_eq!(checked.profile, profile);
        }
    }

    #[test]
    fn a_new_profile_keeps_one_id_through_every_check() {
        let base = Base::new(None, false);
        let form = valid_new_form();
        let first = check(&form, &base, &[], &KeyChecks::default());
        let second = check(&form, &base, &[], &KeyChecks::default());
        assert_eq!(first.profile.id, base.id);
        assert_eq!(second.profile.id, base.id);
    }

    #[test]
    fn the_defaults_of_a_new_form_are_those_of_a_new_profile() {
        let checked = check(
            &valid_new_form(),
            &Base::new(None, false),
            &[],
            &KeyChecks::default(),
        );
        assert_eq!(checked.errors, []);
        let expected = Profile {
            id: checked.profile.id,
            ..Profile::new("New", Protocol::Sftp(SftpProfile::new("h", "u", AGENT)))
        };
        assert_eq!(checked.profile, expected);
    }

    #[test]
    fn spaces_around_what_is_typed_do_not_count() {
        let form = DialogForm {
            name: " New ".into(),
            host: " h ".into(),
            user: " u ".into(),
            remote_path: " /srv ".into(),
            ..valid_new_form()
        };
        let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
        assert_eq!(checked.errors, []);
        assert_eq!(checked.profile.name, "New");
        assert_eq!(sftp(&checked.profile).host, "h");
        assert_eq!(sftp(&checked.profile).username, "u");
        assert_eq!(sftp(&checked.profile).remote_path, "/srv");
    }

    #[test]
    fn each_box_that_makes_no_sense_is_marked_and_stops_saving() {
        let cases = [
            (
                DialogForm {
                    name: "".into(),
                    ..valid_new_form()
                },
                Field::Name,
                Problem::NameRequired,
            ),
            (
                DialogForm {
                    host: "a b".into(),
                    ..valid_new_form()
                },
                Field::Host,
                Problem::HostInvalid,
            ),
            (
                DialogForm {
                    port: "70000".into(),
                    ..valid_new_form()
                },
                Field::Port,
                Problem::PortOutOfRange,
            ),
            (
                DialogForm {
                    user: "".into(),
                    ..valid_new_form()
                },
                Field::User,
                Problem::UserRequired,
            ),
            (
                DialogForm {
                    remote_path: "relative".into(),
                    ..valid_new_form()
                },
                Field::RemotePath,
                Problem::RemotePathInvalid,
            ),
            (
                DialogForm {
                    cache: "61".into(),
                    ..valid_new_form()
                },
                Field::Cache,
                Problem::CacheOutOfRange,
            ),
            (
                DialogForm {
                    keep_alive: "4".into(),
                    ..valid_new_form()
                },
                Field::KeepAlive,
                Problem::KeepAliveOutOfRange,
            ),
            (
                DialogForm {
                    jump_host: "a b".into(),
                    ..valid_new_form()
                },
                Field::JumpHost,
                Problem::JumpHostInvalid,
            ),
            (
                DialogForm {
                    auth: AuthChoice::KeyFile,
                    ..valid_new_form()
                },
                Field::KeyFile,
                Problem::KeyFileRequired,
            ),
        ];
        for (form, field, problem) in cases {
            let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
            assert_eq!(problem_of(&checked, field), Some(problem), "{field:?}");
            assert_eq!(checked.errors.len(), 1, "{field:?}: {:?}", checked.errors);
            assert!(!checked.can_save(), "{field:?}");
        }
    }

    #[test]
    fn a_name_in_use_is_marked_in_any_case_but_not_against_itself() {
        let other = saved("Production", AGENT);
        let form = DialogForm {
            name: "PRODUCTION".into(),
            ..valid_new_form()
        };
        let checked = check(
            &form,
            &Base::new(None, false),
            std::slice::from_ref(&other),
            &KeyChecks::default(),
        );
        assert_eq!(problem_of(&checked, Field::Name), Some(Problem::NameTaken));

        let base = Base::new(Some(other.clone()), false);
        let same = form_of(&other, &base.drives);
        let checked = check(&same, &base, &[other], &KeyChecks::default());
        assert_eq!(checked.errors, []);
    }

    #[test]
    fn a_letter_another_profile_uses_warns_and_does_not_stop_saving() {
        let mut other = saved("NAS backup", AGENT);
        other.drive = DriveChoice::Letter(DriveLetter::new('X').unwrap());
        let base = Base::new(None, false);
        let place = base.drives.iter().position(|d| *d == other.drive).unwrap();
        let form = DialogForm {
            drive_index: i32::try_from(place).unwrap(),
            ..valid_new_form()
        };
        let checked = check(&form, &base, &[other], &KeyChecks::default());
        assert_eq!(
            checked.drive_warning,
            Some(DriveWarning::AlsoUsedBy("NAS backup".to_owned()))
        );
        assert!(checked.can_save());
    }

    #[test]
    fn the_dropdown_lists_auto_and_d_to_z_and_a_forbidden_letter_of_the_file() {
        let plain = Base::new(None, false);
        assert_eq!(plain.drives.len(), 24);
        assert_eq!(plain.drives[0], DriveChoice::Auto);

        let mut bad = saved("Bad", AGENT);
        bad.drive = DriveChoice::Letter(DriveLetter::new('B').unwrap());
        let base = Base::new(Some(bad.clone()), false);
        assert_eq!(base.drives.len(), 25);
        let form = form_of(&bad, &base.drives);
        let checked = check(&form, &base, &[], &KeyChecks::default());
        assert_eq!(checked.profile.drive, bad.drive);
        assert_eq!(
            problem_of(&checked, Field::Drive),
            Some(Problem::DriveInvalid)
        );
    }

    #[test]
    fn a_key_file_holds_things_up_until_it_has_been_looked_at() {
        let path = PathBuf::from(r"C:\keys\id");
        let form = DialogForm {
            auth: AuthChoice::KeyFile,
            key_file: r"C:\keys\id".into(),
            ..valid_new_form()
        };
        let base = Base::new(None, false);
        let mut keys = KeyChecks::default();

        let checked = check(&form, &base, &[], &keys);
        assert_eq!(checked.pending, std::slice::from_ref(&path));
        assert_eq!(checked.errors, [], "nothing to mark yet");
        assert!(!checked.can_save());
        assert!(!checked.can_test());

        keys.answer(path.clone(), Err(Problem::KeyFileMissing));
        let checked = check(&form, &base, &[], &keys);
        assert_eq!(
            problem_of(&checked, Field::KeyFile),
            Some(Problem::KeyFileMissing)
        );
        assert!(checked.pending.is_empty());

        keys.answer(path, Ok(()));
        assert!(check(&form, &base, &[], &keys).can_save());
    }

    /// FR-SSH-03: by its name alone, with no need to wait for the file.
    #[test]
    fn a_ppk_gets_the_hint_at_once_and_save_stays_off() {
        let form = DialogForm {
            auth: AuthChoice::KeyFile,
            key_file: r"C:\keys\id.PPK".into(),
            ..valid_new_form()
        };
        let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
        assert_eq!(
            problem_of(&checked, Field::KeyFile),
            Some(Problem::KeyFilePpk)
        );
        assert!(checked.pending.is_empty());
        assert!(!checked.can_save());
    }

    #[test]
    fn the_jump_hosts_key_file_is_marked_on_its_own_box() {
        let form = DialogForm {
            jump_host: "jump".into(),
            jump_auth_index: JUMP_BY_KEY_FILE,
            jump_key_file: r"C:\keys\jump".into(),
            ..valid_new_form()
        };
        let mut keys = KeyChecks::default();
        keys.answer(PathBuf::from(r"C:\keys\jump"), Err(Problem::KeyFileFormat));
        let checked = check(&form, &Base::new(None, false), &[], &keys);
        assert_eq!(
            problem_of(&checked, Field::JumpKeyFile),
            Some(Problem::KeyFileFormat)
        );
        assert!(checked.advanced_needs_a_look());
    }

    #[test]
    fn a_blank_jump_host_ignores_how_it_would_log_in() {
        let form = DialogForm {
            jump_auth_index: JUMP_BY_KEY_FILE,
            jump_key_file: "".into(),
            jump_password: "typed".into(),
            ..valid_new_form()
        };
        let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
        assert_eq!(checked.errors, []);
        assert_eq!(sftp(&checked.profile).jump, None);
        assert!(typed_secrets(&form, &checked.profile, false).is_empty());
    }

    #[test]
    fn test_connection_needs_no_name_but_save_does() {
        let form = DialogForm {
            name: "".into(),
            ..valid_new_form()
        };
        let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
        assert!(checked.can_test());
        assert!(!checked.can_save());

        let form = DialogForm {
            host: "".into(),
            ..valid_new_form()
        };
        let checked = check(&form, &Base::new(None, false), &[], &KeyChecks::default());
        assert!(!checked.can_test());
    }

    /// FR-PROF-08: whatever the boxes hold, only these can differ.
    #[test]
    fn while_locked_only_the_name_and_flags_can_change() {
        let original = saved("Old", SftpAuth::Password { remember: true });
        let mut base = Base::new(Some(original.clone()), true);
        base.locked = true;
        let form = DialogForm {
            name: "New name".into(),
            mount_on_start: true,
            auto_reconnect: false,
            host: "somewhere.else".into(),
            port: "not a number".into(),
            read_only: true,
            password: "typed".into(),
            auth: AuthChoice::Agent,
            ..form_of(&original, &base.drives)
        };
        let checked = check(&form, &base, &[], &KeyChecks::default());
        assert_eq!(checked.errors, []);
        assert_eq!(
            checked.profile,
            Profile {
                name: "New name".to_owned(),
                mount_on_start: true,
                auto_reconnect: false,
                ..original
            }
        );
        assert!(typed_secrets(&form, &checked.profile, true).is_empty());
    }

    #[test]
    fn only_a_box_in_use_that_holds_something_is_a_secret() {
        let base = Base::new(None, false);
        let form = DialogForm {
            auth: AuthChoice::Password,
            password: "pw".into(),
            jump_host: "jump".into(),
            jump_auth_index: JUMP_BY_PASSWORD,
            jump_password: "jp".into(),
            ..valid_new_form()
        };
        let profile = check(&form, &base, &[], &KeyChecks::default()).profile;
        let typed = typed_secrets(&form, &profile, false);
        assert_eq!(typed.len(), 2);
        assert_eq!(typed[0].0, SecretSlot::Password);
        assert_eq!(typed[0].1.expose_secret(), "pw");
        assert_eq!(typed[1].0, SecretSlot::JumpPassword);
        assert_eq!(typed[1].1.expose_secret(), "jp");

        let empty = DialogForm {
            password: "".into(),
            jump_password: "".into(),
            ..form.clone()
        };
        assert!(typed_secrets(&empty, &profile, false).is_empty());

        let other_method = DialogForm {
            auth: AuthChoice::Agent,
            jump_host: "".into(),
            ..form
        };
        let profile = check(&other_method, &base, &[], &KeyChecks::default()).profile;
        assert!(typed_secrets(&other_method, &profile, false).is_empty());
    }

    /// M1-15's note: otherwise it fails only after the profile has been saved.
    #[test]
    fn a_password_credential_manager_cannot_hold_stops_saving_only_if_it_is_to_be_kept() {
        let long = "x".repeat(MAX_SECRET_UNITS + 1);
        let fits = "x".repeat(MAX_SECRET_UNITS);
        let check_it = |password: &str, remember: bool| {
            let form = DialogForm {
                auth: AuthChoice::Password,
                password: password.into(),
                remember_password: remember,
                ..valid_new_form()
            };
            check(&form, &Base::new(None, false), &[], &KeyChecks::default())
        };
        assert!(check_it(&fits, true).can_save());
        assert!(check_it(&long, false).can_save());
        let too_long = check_it(&long, true);
        assert!(too_long.password_too_long);
        assert!(!too_long.can_save());
    }

    #[test]
    fn typing_is_not_pasting() {
        for (old, new) in [
            ("", "h"),
            ("hos", "host"),
            ("host", "hos"),
            ("hst", "host"),
            ("aaaa", "aaaaa"),
            ("host", ""),
            ("selected text", "x"),
        ] {
            assert!(!looks_pasted(old, new), "{old} -> {new}");
        }
        for (old, new) in [
            ("", "ab"),
            ("", "deploy@10.0.4.21:2222/var/www"),
            ("old", "me@new"),
            ("aa", "aaaa"),
            ("żółć", "żółć@h"),
        ] {
            assert!(looks_pasted(old, new), "{old} -> {new}");
        }
    }

    /// FR-PROF-05's acceptance case.
    #[test]
    fn pasting_fills_the_boxes_it_names() {
        let mut form = new_form();
        assert!(fill_from_paste(&mut form, "deploy@10.0.4.21:2222/var/www"));
        assert_eq!(form.user, "deploy");
        assert_eq!(form.host, "10.0.4.21");
        assert_eq!(form.port, "2222");
        assert_eq!(form.remote_path, "/var/www");
    }

    #[test]
    fn pasting_leaves_the_boxes_it_does_not_name() {
        let mut form = DialogForm {
            user: "kept".into(),
            port: "2200".into(),
            remote_path: "/kept".into(),
            ..new_form()
        };
        assert!(fill_from_paste(&mut form, "sftp://other.example"));
        assert_eq!(form.host, "other.example");
        assert_eq!(form.user, "kept");
        assert_eq!(form.port, "2200");
        assert_eq!(form.remote_path, "/kept");
    }

    #[test]
    fn a_plain_host_and_what_makes_no_sense_change_nothing() {
        let mut form = new_form();
        form.host = "plain.example".into();
        let before = form.clone();
        assert!(!fill_from_paste(&mut form, "plain.example"));
        assert!(!fill_from_paste(&mut form, "ftp://user@host"));
        assert_eq!(form, before);
    }

    #[test]
    fn a_jump_host_is_written_back_as_it_can_be_read() {
        let jump = |host: &str, port, username: Option<&str>| JumpHost {
            host: host.to_owned(),
            port,
            username: username.map(str::to_owned),
            auth: JumpAuth::Agent,
        };
        assert_eq!(jump_text(&jump("h", 22, None)), "h");
        assert_eq!(jump_text(&jump("h", 2200, Some("u"))), "u@h:2200");
        assert_eq!(jump_text(&jump("::1", 22, None)), "::1");
        assert_eq!(jump_text(&jump("::1", 2200, Some("u"))), "u@[::1]:2200");
    }
}
