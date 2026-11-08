// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use drivemount_vfs::{DriveLetter, MemorySecretStore};
use proptest::prelude::*;
use tempfile::TempDir;

use super::*;
use crate::profile::{
    DriveChoice, Field, JumpAuth, JumpHost, Problem, Protocol, SftpAuth, SftpProfile,
};

const ID_A: &str = "6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10";
const ID_B: &str = "11111111-2222-4333-8444-555555555555";

/// Spec §6.1's example, with its comments.
const SPEC_EXAMPLE: &str = r#"
schema_version = 1

[settings]
start_with_windows = false
close_to_tray      = true
theme              = "system"     # system | light | dark
letter_conflict    = "auto"       # auto | fail
notifications      = true
log_level          = "info"       # error | warn | info | debug
op_timeout_secs    = 15           # not in UI, 5..=120
winfsp_threads     = 0            # not in UI, 0 = number of cores (max 8)

[[profiles]]
id             = "6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10"
name           = "Production"
protocol       = "sftp"           # sftp | smb (M3) | nfs (M4)
host           = "10.0.4.21"
port           = 22
username       = "deploy"
auth           = "key"            # password | key | agent | ask
key_path       = 'C:\Users\me\.ssh\id_ed25519'
remember       = true             # secret (password or passphrase) stored in Credential Manager
remote_path    = "/var/www"       # "~" = home directory
drive_letter   = "Z"              # "auto" or D..Z
read_only      = false
mount_on_start = true
auto_reconnect = true
cache_ttl_secs = 5
keepalive_secs = 15

[profiles.jump]                   # optional
host     = "bastion.example.com"
port     = 22
username = "jump"
auth     = "agent"                # agent | key | password
key_path = ""
"#;

/// A folder for one test, and the file in it.
struct Folder {
    dir: TempDir,
    path: PathBuf,
}

impl Folder {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Self { dir, path }
    }

    fn with(text: impl AsRef<[u8]>) -> Self {
        let folder = Self::new();
        fs::write(&folder.path, text).unwrap();
        folder
    }

    fn open(&self) -> ConfigStore {
        ConfigStore::open(&self.path)
    }

    fn text(&self) -> String {
        fs::read_to_string(&self.path).unwrap()
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn table(&self) -> Table {
        self.text().parse().unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

/// Saves what the store has, with `change` made to it.
fn save_with(store: &mut ConfigStore, change: impl FnOnce(&mut Config)) {
    let mut config = store.config().clone();
    change(&mut config);
    store.save(&config).unwrap();
}

fn a_profile(name: &str) -> Profile {
    let sftp = SftpProfile::new("example.com", "me", SftpAuth::Agent);
    Profile::new(name, Protocol::Sftp(sftp))
}

fn sftp(profile: &Profile) -> &SftpProfile {
    let Protocol::Sftp(sftp) = &profile.protocol;
    sftp
}

fn sftp_mut(profile: &mut Profile) -> &mut SftpProfile {
    let Protocol::Sftp(sftp) = &mut profile.protocol;
    sftp
}

fn a_profile_entry(id: &str, name: &str, more: &str) -> String {
    format!(
        "[[profiles]]\nid = \"{id}\"\nname = \"{name}\"\nprotocol = \"sftp\"\nhost = \"h\"\n\
         username = \"u\"\nauth = \"agent\"\n{more}\n"
    )
}

fn a_file(entries: &[String]) -> String {
    format!("schema_version = 1\n{}", entries.concat())
}

fn profile_tables(table: &Table) -> Vec<Table> {
    table["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry.as_table().unwrap().clone())
        .collect()
}

// Round trip

#[test]
fn the_example_in_the_spec_reads_as_it_says() {
    let folder = Folder::with(SPEC_EXAMPLE);
    let store = folder.open();

    assert_eq!(store.read_only(), None);
    assert!(store.setting_problems().is_empty());
    assert!(store.config().unreadable.is_empty());
    assert_eq!(store.config().settings, Settings::default());

    let profile = &store.config().profiles[0];
    assert_eq!(profile.id, Uuid::parse_str(ID_A).unwrap());
    assert_eq!(profile.name, "Production");
    assert_eq!(
        profile.drive,
        DriveChoice::Letter(DriveLetter::new('Z').unwrap())
    );
    assert!(!profile.read_only);
    assert!(profile.mount_on_start);
    assert!(profile.auto_reconnect);

    let sftp = sftp(profile);
    assert_eq!(sftp.host, "10.0.4.21");
    assert_eq!(sftp.port, 22);
    assert_eq!(sftp.username, "deploy");
    assert_eq!(
        sftp.auth,
        SftpAuth::KeyFile {
            path: PathBuf::from(r"C:\Users\me\.ssh\id_ed25519"),
            remember: true
        }
    );
    assert_eq!(sftp.remote_path, "/var/www");
    assert_eq!(sftp.cache_ttl_secs, 5);
    assert_eq!(sftp.keepalive_secs, 15);
    assert_eq!(
        sftp.jump,
        Some(JumpHost {
            host: "bastion.example.com".to_owned(),
            port: 22,
            username: Some("jump".to_owned()),
            auth: JumpAuth::Agent,
        })
    );
}

#[test]
fn saving_the_example_keeps_every_value() {
    let folder = Folder::with(SPEC_EXAMPLE);
    let before: Table = SPEC_EXAMPLE.parse().unwrap();

    let mut store = folder.open();
    let config = store.config().clone();
    // Saved under a changed setting, as saving an unchanged file need not write.
    let mut changed = config.clone();
    changed.settings.notifications = false;
    store.save(&changed).unwrap();
    store.save(&config).unwrap();

    assert_eq!(folder.table(), before);
}

#[test]
fn the_file_starts_with_its_version_and_uses_the_names_in_the_spec() {
    let folder = Folder::new();
    let mut store = folder.open();
    let mut profile = a_profile("One");
    profile.drive = DriveChoice::Letter(DriveLetter::new('z').unwrap());
    sftp_mut(&mut profile).auth = SftpAuth::Password { remember: true };
    save_with(&mut store, |config| config.profiles.push(profile));

    assert!(folder.text().starts_with("schema_version = 1\n"));
    let table = folder.table();
    assert_eq!(table["settings"]["theme"].as_str(), Some("system"));
    assert_eq!(table["settings"]["letter_conflict"].as_str(), Some("auto"));
    assert_eq!(table["settings"]["log_level"].as_str(), Some("info"));
    let entry = &profile_tables(&table)[0];
    assert_eq!(entry["protocol"].as_str(), Some("sftp"));
    assert_eq!(entry["auth"].as_str(), Some("password"));
    assert_eq!(entry["remember"].as_bool(), Some(true));
    assert_eq!(entry["drive_letter"].as_str(), Some("Z"));
    assert!(!entry.contains_key("jump"));
    assert!(!entry.contains_key("key_path"));
}

#[test]
fn each_kind_of_login_comes_back() {
    let auths = [
        SftpAuth::Password { remember: true },
        SftpAuth::Password { remember: false },
        SftpAuth::KeyFile {
            path: PathBuf::from(r"C:\Users\zażółć\id_ed25519"),
            remember: true,
        },
        SftpAuth::KeyFile {
            path: PathBuf::from("/home/me/key"),
            remember: false,
        },
        SftpAuth::Agent,
        SftpAuth::Ask,
    ];
    let jumps = [
        None,
        Some(JumpAuth::Agent),
        Some(JumpAuth::Password),
        Some(JumpAuth::KeyFile {
            path: PathBuf::from(r"D:\keys\jump"),
        }),
    ];
    let mut config = Config::default();
    for auth in &auths {
        for jump in &jumps {
            let mut profile = a_profile(&format!("Profile {}", config.profiles.len()));
            sftp_mut(&mut profile).auth = auth.clone();
            sftp_mut(&mut profile).jump = jump.clone().map(|auth| JumpHost {
                host: "bastion".to_owned(),
                port: 2200,
                username: None,
                auth,
            });
            config.profiles.push(profile);
        }
    }
    let folder = Folder::new();
    folder.open().save(&config).unwrap();

    assert_eq!(folder.open().config(), &config);
}

fn any_auth() -> impl Strategy<Value = SftpAuth> {
    prop_oneof![
        any::<bool>().prop_map(|remember| SftpAuth::Password { remember }),
        (any::<String>(), any::<bool>()).prop_map(|(path, remember)| SftpAuth::KeyFile {
            path: PathBuf::from(path),
            remember
        }),
        Just(SftpAuth::Agent),
        Just(SftpAuth::Ask),
    ]
}

fn any_jump() -> impl Strategy<Value = Option<JumpHost>> {
    let auth = prop_oneof![
        Just(JumpAuth::Agent),
        Just(JumpAuth::Password),
        any::<String>().prop_map(|path| JumpAuth::KeyFile {
            path: PathBuf::from(path)
        }),
    ];
    proptest::option::of((any::<String>(), any::<u16>(), ".*[^\n]", auth).prop_map(
        |(host, port, user, auth)| JumpHost {
            host,
            port,
            username: Some(user),
            auth,
        },
    ))
}

fn any_profile() -> impl Strategy<Value = Profile> {
    let values = 0..=i64::MAX.cast_unsigned();
    let drive = prop_oneof![
        Just(DriveChoice::Auto),
        (b'D'..=b'Z').prop_map(|letter| {
            DriveChoice::Letter(DriveLetter::new(char::from(letter)).unwrap())
        }),
    ];
    let sftp = (
        any::<String>(),
        any::<u16>(),
        any::<String>(),
        any_auth(),
        any::<String>(),
        values.clone(),
        values,
        any_jump(),
    )
        .prop_map(
            |(host, port, username, auth, remote_path, cache_ttl_secs, keepalive_secs, jump)| {
                SftpProfile {
                    host,
                    port,
                    username,
                    auth,
                    remote_path,
                    cache_ttl_secs,
                    keepalive_secs,
                    jump,
                }
            },
        );
    (
        any::<u128>(),
        any::<String>(),
        sftp,
        drive,
        any::<[bool; 3]>(),
    )
        .prop_map(
            |(id, name, sftp, drive, [read_only, mount_on_start, auto_reconnect])| Profile {
                id: Uuid::from_u128(id),
                name,
                protocol: Protocol::Sftp(sftp),
                drive,
                read_only,
                mount_on_start,
                auto_reconnect,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever the model holds, rules or not, the file gives back.
    #[test]
    fn any_profile_comes_back(profile in any_profile()) {
        let folder = Folder::new();
        let config = Config { profiles: vec![profile], ..Config::default() };
        folder.open().save(&config).unwrap();

        let store = folder.open();
        prop_assert_eq!(store.read_only(), None);
        prop_assert_eq!(store.config(), &config);
    }
}

#[test]
fn settings_come_back() {
    let settings = Settings {
        start_with_windows: true,
        close_to_tray: false,
        theme: Theme::Dark,
        letter_conflict: LetterConflict::Fail,
        notifications: false,
        log_level: LogLevel::Debug,
        op_timeout_secs: 120,
        winfsp_threads: 8,
    };
    let config = Config {
        settings: settings.clone(),
        ..Config::default()
    };
    let folder = Folder::new();
    folder.open().save(&config).unwrap();

    let store = folder.open();
    assert_eq!(store.config().settings, settings);
    assert!(store.setting_problems().is_empty());
}

#[test]
fn only_the_required_keys_are_required() {
    let folder = Folder::with(a_file(&[a_profile_entry(ID_A, "One", "")]));
    let store = folder.open();

    assert_eq!(store.config().settings, Settings::default());
    let profile = &store.config().profiles[0];
    assert_eq!(profile.drive, DriveChoice::Auto);
    assert!(!profile.read_only);
    assert!(!profile.mount_on_start);
    assert!(profile.auto_reconnect);
    let sftp = sftp(profile);
    assert_eq!(sftp.port, 22);
    assert_eq!(sftp.remote_path, "~");
    assert_eq!(sftp.cache_ttl_secs, 5);
    assert_eq!(sftp.keepalive_secs, 15);
    assert_eq!(sftp.jump, None);
}

#[test]
fn a_blank_jump_user_is_the_profiles_own() {
    let entry = a_profile_entry(
        ID_A,
        "One",
        "[profiles.jump]\nhost = \"j\"\nusername = \"\"\nauth = \"agent\"\n",
    );
    let folder = Folder::with(a_file(&[entry]));
    let store = folder.open();

    let profile = &store.config().profiles[0];
    assert_eq!(sftp(profile).jump.as_ref().unwrap().username, None);
}

#[test]
fn no_file_is_a_first_run_and_writes_nothing_until_saved() {
    let folder = Folder::new();
    let store = folder.open();

    assert_eq!(store.read_only(), None);
    assert_eq!(store.config(), &Config::default());
    assert!(folder.names().is_empty());
}

#[test]
fn the_first_save_makes_the_folders_and_no_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("DriveMount")
        .join("deeper")
        .join("config.toml");
    let mut store = ConfigStore::open(&path);

    store.save(&Config::default()).unwrap();

    assert_eq!(store.read_only(), None);
    assert!(path.exists());
    assert!(!store.backup_path().exists());
}

#[test]
fn the_default_path_is_under_appdata() {
    let path = path_under(Some(OsString::from("somewhere"))).unwrap();
    assert_eq!(
        path,
        Path::new("somewhere")
            .join("DriveMount")
            .join("config.toml")
    );
    assert_eq!(path_under(None), None);
    assert_eq!(path_under(Some(OsString::new())), None);
}

#[test]
fn a_file_with_a_byte_order_mark_is_read() {
    let folder = Folder::with(format!("\u{FEFF}{}", a_file(&[])));
    assert_eq!(folder.open().read_only(), None);
}

// Unknown keys

const WITH_UNKNOWN_KEYS: &str = r#"
schema_version = 1
top_number = 7
top_date = 2026-11-05T12:00:00Z
top_list = [1, "two", { three = 3.5 }]

[top_table]
inner = "kept"

[settings]
theme = "dark"
new_setting = "kept"

[settings.new_table]
deep = true

[[profiles]]
id = "6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10"
name = "One"
protocol = "sftp"
host = "h"
username = "u"
auth = "agent"
new_option = 12
new_when = 1979-05-27

[profiles.new_section]
value = "kept"
when = 07:32:00

[profiles.jump]
host = "j"
auth = "agent"
new_jump_option = "kept"
new_jump_when = 1979-05-27T07:32:00
"#;

fn assert_unknown_keys_are_there(table: &Table) {
    assert_eq!(table["top_number"].as_integer(), Some(7));
    assert!(table["top_date"].is_datetime());
    assert_eq!(table["top_list"][2]["three"].as_float(), Some(3.5));
    assert_eq!(table["top_table"]["inner"].as_str(), Some("kept"));
    assert_eq!(table["settings"]["new_setting"].as_str(), Some("kept"));
    assert_eq!(table["settings"]["new_table"]["deep"].as_bool(), Some(true));
    let entry = &profile_tables(table)[0];
    assert_eq!(entry["new_option"].as_integer(), Some(12));
    assert!(entry["new_when"].is_datetime());
    assert_eq!(entry["new_section"]["value"].as_str(), Some("kept"));
    assert!(entry["new_section"]["when"].is_datetime());
    assert!(entry["jump"]["new_jump_when"].is_datetime());
    assert_eq!(entry["jump"]["new_jump_option"].as_str(), Some("kept"));
}

#[test]
fn unknown_keys_survive_saving_at_every_level() {
    let folder = Folder::with(WITH_UNKNOWN_KEYS);
    let mut store = folder.open();
    assert_eq!(store.read_only(), None);

    save_with(&mut store, |config| config.settings.theme = Theme::Light);
    assert_unknown_keys_are_there(&folder.table());
    assert_eq!(folder.table()["settings"]["theme"].as_str(), Some("light"));

    // And again: from the store, and from a file this program wrote.
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    assert_unknown_keys_are_there(&folder.table());
    let mut store = folder.open();
    save_with(&mut store, |config| config.settings.notifications = false);
    assert_unknown_keys_are_there(&folder.table());
}

#[test]
fn what_the_auth_method_has_no_use_for_stays_in_the_file() {
    let entry = a_profile_entry(
        ID_A,
        "One",
        "key_path = 'C:\\k'\nremember = true\n[profiles.jump]\nhost = \"j\"\nauth = \"agent\"\nkey_path = 'C:\\j'\n",
    );
    let folder = Folder::with(a_file(&[entry]));
    let mut store = folder.open();
    assert_eq!(sftp(&store.config().profiles[0]).auth, SftpAuth::Agent);

    save_with(&mut store, |config| config.settings.notifications = false);

    let entry = &profile_tables(&folder.table())[0];
    assert_eq!(entry["key_path"].as_str(), Some(r"C:\k"));
    assert_eq!(entry["remember"].as_bool(), Some(true));
    assert_eq!(entry["jump"]["key_path"].as_str(), Some(r"C:\j"));
}

/// The same as for a file that was read, for a profile this store has only written.
#[test]
fn what_is_kept_follows_each_save() {
    let folder = Folder::new();
    let mut store = folder.open();
    let mut profile = a_profile("One");
    sftp_mut(&mut profile).auth = SftpAuth::KeyFile {
        path: PathBuf::from(r"C:\k"),
        remember: true,
    };
    save_with(&mut store, |config| config.profiles.push(profile));

    save_with(&mut store, |config| {
        sftp_mut(&mut config.profiles[0]).auth = SftpAuth::Agent;
    });

    let entry = &profile_tables(&folder.table())[0];
    assert_eq!(entry["auth"].as_str(), Some("agent"));
    assert_eq!(entry["key_path"].as_str(), Some(r"C:\k"));
}

#[test]
fn a_login_the_model_has_replaces_what_was_kept() {
    let entry = a_profile_entry(ID_A, "One", "key_path = 'C:\\k'\nremember = true");
    let folder = Folder::with(a_file(&[entry]));
    let mut store = folder.open();

    save_with(&mut store, |config| {
        sftp_mut(&mut config.profiles[0]).auth = SftpAuth::KeyFile {
            path: PathBuf::from(r"C:\new"),
            remember: false,
        };
    });

    let entry = &profile_tables(&folder.table())[0];
    assert_eq!(entry["auth"].as_str(), Some("key"));
    assert_eq!(entry["key_path"].as_str(), Some(r"C:\new"));
    assert_eq!(entry["remember"].as_bool(), Some(false));
}

/// FR-PROF-02.
#[test]
fn a_rename_keeps_the_id_and_the_unknown_keys() {
    let folder = Folder::with(a_file(&[a_profile_entry(ID_A, "One", "extra_key = 1")]));
    let mut store = folder.open();

    save_with(&mut store, |config| {
        config.profiles[0].name = "Renamed".to_owned();
    });

    let store = folder.open();
    assert_eq!(store.config().profiles[0].name, "Renamed");
    assert_eq!(
        store.config().profiles[0].id,
        Uuid::parse_str(ID_A).unwrap()
    );
    assert_eq!(
        profile_tables(&folder.table())[0]["extra_key"].as_integer(),
        Some(1)
    );
}

/// FR-PROF-01.
#[test]
fn a_deleted_profile_is_gone_and_the_others_keep_their_keys() {
    let folder = Folder::with(a_file(&[
        a_profile_entry(ID_A, "One", "extra_key = 1"),
        a_profile_entry(ID_B, "Two", "extra_key = 2"),
    ]));
    let mut store = folder.open();

    save_with(&mut store, |config| {
        config.profiles.remove(0);
    });

    let entries = profile_tables(&folder.table());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"].as_str(), Some("Two"));
    assert_eq!(entries[0]["extra_key"].as_integer(), Some(2));
}

#[test]
fn a_new_profile_starts_with_nothing_kept_from_another() {
    let folder = Folder::with(a_file(&[a_profile_entry(ID_A, "One", "extra_key = 1")]));
    let mut store = folder.open();

    save_with(&mut store, |config| config.profiles.push(a_profile("New")));

    let entries = profile_tables(&folder.table());
    assert_eq!(entries[1]["name"].as_str(), Some("New"));
    assert!(!entries[1].contains_key("extra_key"));
}

// One bad profile

#[test]
fn a_profile_that_does_not_fit_leaves_the_rest_usable_and_is_written_back_as_it_was() {
    let bad_entries = [
        ("port too large", "port = 70000"),
        ("port is text", "port = \"22\""),
        ("port negative", "port = -1"),
        ("seconds negative", "cache_ttl_secs = -1"),
        ("flag is text", "read_only = \"yes\""),
        ("no such letter", "drive_letter = \"AB\""),
        ("a letter never allowed", "drive_letter = \"C\""),
        ("jump is text", "jump = \"bastion\""),
    ];
    for (what, line) in bad_entries {
        let bad = a_profile_entry(ID_B, "Bad", &format!("{line}\nkeep_this = 1"));
        let folder = Folder::with(a_file(&[a_profile_entry(ID_A, "Good", ""), bad]));
        let mut store = folder.open();

        assert_eq!(store.read_only(), None, "{what}");
        assert_eq!(store.config().profiles.len(), 1, "{what}");
        assert_eq!(store.config().profiles[0].name, "Good", "{what}");
        let [unreadable] = &store.config().unreadable[..] else {
            panic!("{what}: expected one");
        };
        assert_eq!(unreadable.name.as_deref(), Some("Bad"), "{what}");
        assert_eq!(
            unreadable.id,
            Some(Uuid::parse_str(ID_B).unwrap()),
            "{what}"
        );
        assert!(!unreadable.problem.is_empty(), "{what}");

        let before = profile_tables(&folder.text().parse().unwrap())[1].clone();
        save_with(&mut store, |config| config.settings.notifications = false);
        let entries = profile_tables(&folder.table());
        assert_eq!(entries.len(), 2, "{what}");
        assert_eq!(entries[1], before, "{what}");
        assert_eq!(folder.open().config().unreadable.len(), 1, "{what}");
    }
}

#[test]
fn entries_of_a_later_milestone_or_with_parts_missing_are_kept() {
    let smb = "[[profiles]]\nid = \"11111111-2222-4333-8444-555555555555\"\nname = \"Nas\"\n\
               protocol = \"smb\"\nserver = \"nas\"\nshare = \"media\"\n"
        .to_owned();
    let unknown_login = a_profile_entry(ID_A, "Agent", "").replace("agent", "fido");
    let no_id = "[[profiles]]\nname = \"No id\"\n".to_owned();
    let bad_id = "[[profiles]]\nid = \"not-a-uuid\"\n".to_owned();
    let nothing = "[[profiles]]\n".to_owned();

    let folder = Folder::with(a_file(&[smb, unknown_login, no_id, bad_id, nothing]));
    let mut store = folder.open();

    assert_eq!(store.read_only(), None);
    assert!(store.config().profiles.is_empty());
    let unreadable = &store.config().unreadable;
    assert_eq!(unreadable.len(), 5);
    assert_eq!(unreadable[0].name.as_deref(), Some("Nas"));
    assert_eq!(unreadable[2].id, None);
    assert_eq!(unreadable[3].id, None);
    assert_eq!(unreadable[4].name, None);

    save_with(&mut store, |config| config.settings.notifications = false);
    let entries = profile_tables(&folder.table());
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0]["share"].as_str(), Some("media"));
}

#[test]
fn an_unreadable_profile_can_be_taken_out_of_the_file() {
    let bad = a_profile_entry(ID_B, "Bad", "port = 70000");
    let folder = Folder::with(a_file(&[a_profile_entry(ID_A, "Good", ""), bad]));
    let mut store = folder.open();

    save_with(&mut store, |config| config.unreadable.clear());

    assert_eq!(profile_tables(&folder.table()).len(), 1);
}

/// They would share one set of secrets, and deleting one would delete the other's.
#[test]
fn the_second_profile_with_an_id_is_unreadable() {
    let folder = Folder::with(a_file(&[
        a_profile_entry(ID_A, "First", "extra_key = 1"),
        a_profile_entry(ID_A, "Second", "extra_key = 2"),
    ]));
    let mut store = folder.open();

    assert_eq!(store.config().profiles.len(), 1);
    assert_eq!(store.config().profiles[0].name, "First");
    assert_eq!(store.config().unreadable[0].name.as_deref(), Some("Second"));

    save_with(&mut store, |config| config.settings.notifications = false);
    let entries = profile_tables(&folder.table());
    assert_eq!(entries[0]["extra_key"].as_integer(), Some(1));
    assert_eq!(entries[1]["extra_key"].as_integer(), Some(2));
}

#[test]
fn values_that_fit_but_break_a_rule_are_loaded_and_listed() {
    let folder = Folder::with(a_file(&[
        a_profile_entry(
            ID_A,
            "One",
            "cache_ttl_secs = 9999\nkeepalive_secs = 1\nport = 0",
        ),
        a_profile_entry(ID_B, "Fine", ""),
    ]));
    let store = folder.open();

    assert_eq!(store.read_only(), None);
    assert_eq!(store.config().profiles.len(), 2);
    let invalid = store.config().invalid_profiles();
    let [(id, errors)] = &invalid[..] else {
        panic!("expected one: {invalid:?}");
    };
    assert_eq!(*id, Uuid::parse_str(ID_A).unwrap());
    let fields: Vec<_> = errors.iter().map(|error| error.field).collect();
    assert_eq!(fields, [Field::Port, Field::Cache, Field::KeepAlive]);
}

#[test]
fn a_missing_key_file_is_not_a_problem_on_load_but_a_bad_name_is() {
    let key = |path: &str| format!("key_path = '{path}'");
    let missing =
        a_profile_entry(ID_A, "Unplugged", &key(r"Q:\not\plugged\in")).replace("agent", "key");
    let ppk = a_profile_entry(ID_B, "Putty", &key(r"Q:\key.ppk")).replace("agent", "key");
    let folder = Folder::with(a_file(&[missing, ppk]));

    let invalid = folder.open().config().invalid_profiles();

    let [(id, errors)] = &invalid[..] else {
        panic!("expected one: {invalid:?}");
    };
    assert_eq!(*id, Uuid::parse_str(ID_B).unwrap());
    assert_eq!(errors[0].problem, Problem::KeyFilePpk);
}

#[test]
fn a_key_method_without_a_path_is_marked_not_lost() {
    let entry = a_profile_entry(ID_A, "One", "").replace("agent", "key");
    let folder = Folder::with(a_file(&[entry]));

    let invalid = folder.open().config().invalid_profiles();

    assert_eq!(invalid[0].1[0].problem, Problem::KeyFileRequired);
}

#[test]
fn two_names_the_same_are_both_listed() {
    let folder = Folder::with(a_file(&[
        a_profile_entry(ID_A, "Same", ""),
        a_profile_entry(ID_B, "same", ""),
    ]));

    let invalid = folder.open().config().invalid_profiles();

    assert_eq!(invalid.len(), 2);
    assert_eq!(invalid[0].1[0].problem, Problem::NameTaken);
}

// Bad settings

#[test]
fn a_bad_setting_gets_its_default_and_the_file_stays_usable() {
    let cases = [
        ("theme", "\"purple\""),
        ("theme", "3"),
        ("letter_conflict", "\"maybe\""),
        ("log_level", "\"trace\""),
        ("notifications", "\"yes\""),
        ("start_with_windows", "1"),
        ("close_to_tray", "\"no\""),
        ("op_timeout_secs", "4"),
        ("op_timeout_secs", "121"),
        ("op_timeout_secs", "-1"),
        ("op_timeout_secs", "\"long\""),
        ("winfsp_threads", "9"),
        ("winfsp_threads", "-1"),
    ];
    for (key, value) in cases {
        let what = format!("{key} = {value}");
        let folder = Folder::with(format!("schema_version = 1\n[settings]\n{what}\n"));
        let store = folder.open();

        assert_eq!(store.read_only(), None, "{what}");
        assert_eq!(store.config().settings, Settings::default(), "{what}");
        let [problem] = store.setting_problems() else {
            panic!("{what}: {:?}", store.setting_problems());
        };
        assert_eq!(problem.key, key, "{what}");
    }
}

#[test]
fn the_limits_of_a_setting_are_allowed() {
    let cases = [
        ("op_timeout_secs", 5),
        ("op_timeout_secs", 120),
        ("winfsp_threads", 0),
        ("winfsp_threads", 8),
    ];
    for (key, value) in cases {
        let folder = Folder::with(format!("schema_version = 1\n[settings]\n{key} = {value}\n"));
        assert!(
            folder.open().setting_problems().is_empty(),
            "{key} = {value}"
        );
    }
}

#[test]
fn saving_replaces_a_bad_setting_and_clears_its_problem() {
    let folder = Folder::with("schema_version = 1\n[settings]\ntheme = \"purple\"\n");
    let mut store = folder.open();

    save_with(&mut store, |_| {});

    assert!(store.setting_problems().is_empty());
    assert_eq!(folder.table()["settings"]["theme"].as_str(), Some("system"));
    assert!(folder.open().setting_problems().is_empty());
}

// Corrupt files

fn assert_read_only_and_untouched(folder: &Folder, bytes: &[u8]) -> ReadOnlyReason {
    let mut store = folder.open();
    let reason = store.read_only().cloned().expect("should be read-only");
    assert_eq!(store.config(), &Config::default());

    let mut config = Config::default();
    config.profiles.push(a_profile("Would overwrite"));
    match store.save(&config) {
        Err(SaveError::ReadOnly(refused)) => assert_eq!(refused, reason),
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read(&folder.path).unwrap(), bytes);
    assert_eq!(folder.names(), ["config.toml"], "nothing else is written");
    assert_eq!(store.config(), &Config::default());
    reason
}

#[test]
fn a_damaged_file_means_read_only_and_is_never_written() {
    let cases: [(&str, &[u8]); 11] = [
        ("not TOML", b"this is not toml"),
        ("cut short", b"schema_version = 1\n[settings\n"),
        ("not UTF-8", b"schema_version = 1\n# \xFF\xFE\n"),
        ("empty", b""),
        ("no version", b"[settings]\ntheme = \"dark\"\n"),
        ("version is text", b"schema_version = \"1\"\n"),
        ("version is zero", b"schema_version = 0\n"),
        ("version is negative", b"schema_version = -1\n"),
        (
            "settings is not a table",
            b"schema_version = 1\nsettings = 5\n",
        ),
        (
            "profiles is not a list",
            b"schema_version = 1\nprofiles = 5\n",
        ),
        (
            "a profile is not a table",
            b"schema_version = 1\nprofiles = [1]\n",
        ),
    ];
    for (what, bytes) in cases {
        let folder = Folder::with(bytes);
        let reason = assert_read_only_and_untouched(&folder, bytes);
        assert!(
            matches!(reason, ReadOnlyReason::Damaged { .. }),
            "{what}: {reason:?}"
        );
    }
}

#[test]
fn a_file_that_cannot_be_opened_means_read_only() {
    let folder = Folder::new();
    // A folder in its place cannot be read as a file.
    fs::create_dir(&folder.path).unwrap();

    let mut store = folder.open();

    assert!(matches!(
        store.read_only(),
        Some(ReadOnlyReason::CannotRead { .. })
    ));
    assert!(matches!(
        store.save(&Config::default()),
        Err(SaveError::ReadOnly(_))
    ));
    assert_eq!(folder.names(), ["config.toml"]);
}

#[test]
fn a_file_damaged_while_running_is_not_overwritten_and_the_backup_is_kept() {
    let folder = Folder::new();
    let mut store = folder.open();
    save_with(&mut store, |config| {
        config.profiles.push(a_profile("First"))
    });
    save_with(&mut store, |config| {
        config.profiles.push(a_profile("Second"))
    });
    let backup = fs::read(store.backup_path()).unwrap();
    fs::write(&folder.path, "half a fi").unwrap();

    let config = store.config().clone();
    let mut changed = config.clone();
    changed.settings.theme = Theme::Dark;
    assert!(matches!(store.save(&changed), Err(SaveError::ReadOnly(_))));

    assert_eq!(folder.text(), "half a fi");
    assert_eq!(fs::read(store.backup_path()).unwrap(), backup);
    assert!(matches!(
        store.read_only(),
        Some(ReadOnlyReason::Damaged { .. })
    ));
    assert_eq!(store.config(), &config, "what is in memory is still shown");
}

// Newer files

#[test]
fn a_newer_file_means_read_only_with_the_words_in_the_spec() {
    let text = format!(
        "schema_version = 2\nfrom_the_future = 1\n{}",
        a_profile_entry(ID_A, "One", "")
    );
    let folder = Folder::with(&text);
    let mut store = folder.open();

    assert_eq!(store.read_only(), Some(&ReadOnlyReason::Newer { found: 2 }));
    assert_eq!(
        store.read_only().unwrap().to_string(),
        "This configuration was written by a newer DriveMount"
    );
    assert_eq!(
        store.config().profiles[0].name,
        "One",
        "shown if understood"
    );

    let config = store.config().clone();
    let mut changed = config.clone();
    changed.settings.theme = Theme::Dark;
    assert!(matches!(store.save(&changed), Err(SaveError::ReadOnly(_))));
    assert_eq!(folder.text(), text);
    assert_eq!(folder.names(), ["config.toml"]);
}

#[test]
fn a_newer_file_of_another_shape_is_still_only_read_only() {
    for text in [
        "schema_version = 2\nprofiles = \"somewhere else\"\n",
        "schema_version = 2\nsettings = 1\n",
        "schema_version = 99999999999\n",
    ] {
        let folder = Folder::with(text);
        let store = folder.open();
        assert!(
            matches!(store.read_only(), Some(ReadOnlyReason::Newer { .. })),
            "{text}"
        );
        assert_eq!(store.config(), &Config::default(), "{text}");
    }
}

#[test]
fn a_file_replaced_by_a_newer_one_while_running_is_not_overwritten() {
    let folder = Folder::new();
    let mut store = folder.open();
    store.save(&Config::default()).unwrap();
    fs::write(&folder.path, "schema_version = 2\n").unwrap();

    let mut config = Config::default();
    config.settings.theme = Theme::Dark;
    assert!(matches!(store.save(&config), Err(SaveError::ReadOnly(_))));

    assert_eq!(folder.text(), "schema_version = 2\n");
    assert!(matches!(
        store.read_only(),
        Some(ReadOnlyReason::Newer { found: 2 })
    ));
    assert!(!store.backup_path().exists());
}

// Migrations

fn rename_host_to_server(table: &mut Table) -> Result<(), String> {
    let profiles = table
        .get_mut("profiles")
        .and_then(toml::Value::as_array_mut);
    for profile in profiles.into_iter().flatten() {
        let profile = profile.as_table_mut().ok_or("not a table")?;
        let server = profile.remove("server").ok_or("no server")?;
        profile.insert("host".to_owned(), server);
    }
    Ok(())
}

fn add_step_two_marker(table: &mut Table) -> Result<(), String> {
    table.insert("step_two".to_owned(), toml::Value::Integer(2));
    Ok(())
}

fn fail(_: &mut Table) -> Result<(), String> {
    Err("the step failed".to_owned())
}

const TWO_STEPS: Schema = Schema {
    current: 3,
    steps: &[rename_host_to_server, add_step_two_marker],
};

const V1_FILE: &str = "schema_version = 1\n[[profiles]]\n\
    id = \"6f1c2c1e-4d0b-4c55-9a51-7d8a1c3b2e10\"\nname = \"One\"\nprotocol = \"sftp\"\n\
    server = \"old-name\"\nusername = \"u\"\nauth = \"agent\"\n";

#[test]
fn every_version_below_this_one_has_its_migration() {
    assert_eq!(SCHEMA.current, SCHEMA_VERSION);
    assert_eq!(
        SCHEMA.steps.len() + 1,
        usize::try_from(SCHEMA.current).unwrap()
    );
}

#[test]
fn an_older_file_is_migrated_step_by_step_and_kept_on_the_first_save() {
    let folder = Folder::with(V1_FILE);
    let mut store = ConfigStore::open_with(folder.path.clone(), TWO_STEPS);

    assert_eq!(store.read_only(), None);
    assert_eq!(sftp(&store.config().profiles[0]).host, "old-name");
    assert_eq!(folder.text(), V1_FILE, "opening writes nothing");
    assert_eq!(folder.names(), ["config.toml"]);

    let config = store.config().clone();
    store.save(&config).unwrap();

    let table = folder.table();
    assert_eq!(table["schema_version"].as_integer(), Some(3));
    assert_eq!(table["step_two"].as_integer(), Some(2));
    assert_eq!(profile_tables(&table)[0]["host"].as_str(), Some("old-name"));
    assert_eq!(
        fs::read_to_string(folder.sibling("config.v1.bak")).unwrap(),
        V1_FILE
    );
    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), V1_FILE);

    // From now on the file is current, so there is no new versioned backup.
    let mut changed = config;
    changed.settings.theme = Theme::Dark;
    store.save(&changed).unwrap();
    assert_eq!(
        fs::read_to_string(folder.sibling("config.v1.bak")).unwrap(),
        V1_FILE
    );
    assert_ne!(fs::read_to_string(store.backup_path()).unwrap(), V1_FILE);
    assert!(!folder.sibling("config.v3.bak").exists());
}

#[test]
fn a_migration_starts_from_the_version_of_the_file() {
    let v2 = V1_FILE
        .replace("schema_version = 1", "schema_version = 2")
        .replace("server", "host");
    let folder = Folder::with(&v2);
    let mut store = ConfigStore::open_with(folder.path.clone(), TWO_STEPS);
    assert_eq!(store.read_only(), None);

    let config = store.config().clone();
    store.save(&config).unwrap();

    assert_eq!(folder.table()["step_two"].as_integer(), Some(2));
    assert_eq!(
        fs::read_to_string(folder.sibling("config.v2.bak")).unwrap(),
        v2
    );
    assert!(!folder.sibling("config.v1.bak").exists());
}

#[test]
fn a_failed_migration_means_read_only() {
    let schema = Schema {
        current: 3,
        steps: &[rename_host_to_server, fail],
    };
    let folder = Folder::with(V1_FILE);
    let mut store = ConfigStore::open_with(folder.path.clone(), schema);

    assert_eq!(
        store.read_only(),
        Some(&ReadOnlyReason::MigrationFailed {
            from: 2,
            detail: "the step failed".to_owned()
        })
    );
    assert!(matches!(
        store.save(&Config::default()),
        Err(SaveError::ReadOnly(_))
    ));
    assert_eq!(folder.text(), V1_FILE);
    assert_eq!(folder.names(), ["config.toml"]);
}

#[test]
fn a_missing_migration_means_read_only_rather_than_a_panic() {
    let schema = Schema {
        current: 3,
        steps: &[],
    };
    let folder = Folder::with(V1_FILE);
    let store = ConfigStore::open_with(folder.path.clone(), schema);
    assert!(matches!(
        store.read_only(),
        Some(ReadOnlyReason::MigrationFailed { from: 1, .. })
    ));
}

// Atomic writes and the backup

#[test]
fn the_backup_is_the_previous_file() {
    let folder = Folder::new();
    let mut store = folder.open();

    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    assert_eq!(folder.names(), ["config.toml"], "no earlier file to keep");
    let first = folder.text();

    save_with(&mut store, |config| config.settings.theme = Theme::Light);
    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), first);
    let second = folder.text();

    save_with(&mut store, |config| config.settings.theme = Theme::System);
    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), second);
    assert_eq!(
        folder.names(),
        ["config.toml", "config.toml.bak"],
        "no temp files left"
    );
}

/// A hard link still sees the file it was made from, so it tells replacing a file from writing into
/// it. The old file is never opened for writing, which is what makes a crash harmless.
#[test]
fn the_file_and_its_backup_are_replaced_not_written_into() {
    let folder = Folder::new();
    let mut store = folder.open();
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    let first = folder.text();
    save_with(&mut store, |config| config.settings.theme = Theme::Light);
    let second = folder.text();
    let link_to_file = folder.sibling("link-to-file");
    let link_to_backup = folder.sibling("link-to-backup");
    fs::hard_link(&folder.path, &link_to_file).unwrap();
    fs::hard_link(store.backup_path(), &link_to_backup).unwrap();

    save_with(&mut store, |config| config.settings.theme = Theme::System);

    assert_eq!(fs::read_to_string(link_to_file).unwrap(), second);
    assert_eq!(fs::read_to_string(link_to_backup).unwrap(), first);
    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), second);
    assert_ne!(folder.text(), second);
}

#[test]
fn saving_with_no_change_keeps_the_backup_one_version_behind() {
    let folder = Folder::new();
    let mut store = folder.open();
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    let first = folder.text();
    save_with(&mut store, |config| config.settings.theme = Theme::Light);

    save_with(&mut store, |_| {});
    save_with(&mut store, |_| {});

    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), first);
}

#[test]
fn if_the_backup_cannot_be_written_the_file_is_left_alone() {
    let folder = Folder::new();
    let mut store = folder.open();
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    let before = folder.text();
    // A folder in its place cannot be replaced.
    fs::create_dir(store.backup_path()).unwrap();

    let mut changed = store.config().clone();
    changed.settings.theme = Theme::Light;
    assert!(matches!(store.save(&changed), Err(SaveError::Io(_))));

    assert_eq!(folder.text(), before);
    assert_eq!(
        store.config().settings.theme,
        Theme::Dark,
        "not what was refused"
    );
    assert_eq!(store.read_only(), None, "only this save failed");
    assert_eq!(
        folder.names(),
        ["config.toml", "config.toml.bak"],
        "no temp files left"
    );

    // And it can try again.
    fs::remove_dir(store.backup_path()).unwrap();
    store.save(&changed).unwrap();
    assert_eq!(fs::read_to_string(store.backup_path()).unwrap(), before);
    assert_eq!(folder.table()["settings"]["theme"].as_str(), Some("light"));
}

#[test]
fn a_file_that_cannot_be_read_on_saving_fails_only_that_save() {
    let folder = Folder::new();
    let mut store = folder.open();
    fs::create_dir(&folder.path).unwrap();

    assert!(matches!(
        store.save(&Config::default()),
        Err(SaveError::Io(_))
    ));
    assert_eq!(store.read_only(), None);
    assert_eq!(folder.names(), ["config.toml"]);
}

#[test]
fn a_number_toml_cannot_read_back_leaves_everything_as_it_was() {
    let folder = Folder::new();
    let mut store = folder.open();
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);
    let before = folder.text();

    let mut profile = a_profile("Too long a wait");
    sftp_mut(&mut profile).keepalive_secs = u64::MAX;
    let mut config = store.config().clone();
    config.profiles.push(profile);

    assert!(matches!(
        store.save(&config),
        Err(SaveError::WouldNotLoad { .. })
    ));
    assert_eq!(folder.text(), before);
    assert_eq!(folder.names(), ["config.toml"]);
    assert!(store.config().profiles.is_empty());
}

#[cfg(windows)]
#[test]
fn a_path_that_is_not_unicode_refuses_to_save_rather_than_change_it() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let path = OsString::from_wide(&[0x0043, 0xD800]);
    let mut profile = a_profile("Odd path");
    sftp_mut(&mut profile).auth = SftpAuth::KeyFile {
        path: PathBuf::from(path),
        remember: false,
    };
    let folder = Folder::new();
    let config = Config {
        profiles: vec![profile],
        ..Config::default()
    };

    match folder.open().save(&config) {
        Err(SaveError::PathNotText { profile }) => assert_eq!(profile, "Odd path"),
        other => panic!("{other:?}"),
    }
    assert!(folder.names().is_empty());
}

// No secrets

/// NFR-SEC-02: the secret goes one way and the file the other, and they do not meet.
#[test]
fn a_remembered_secret_is_not_in_the_file_or_its_backup() {
    const SECRET: &str = "correct horse battery staple";

    let folder = Folder::new();
    let secrets: &dyn SecretStore = &MemorySecretStore::new();
    let mut store = folder.open();
    let mut profile = a_profile("One");
    sftp_mut(&mut profile).auth = SftpAuth::Password { remember: true };
    secrets
        .store(
            profile.id,
            SecretSlot::Password,
            &Secret::new(SECRET.to_owned()),
        )
        .unwrap();
    save_with(&mut store, |config| config.profiles.push(profile.clone()));
    save_with(&mut store, |config| config.settings.theme = Theme::Dark);

    let entry = &profile_tables(&folder.table())[0];
    let keys: Vec<_> = entry.keys().map(String::as_str).collect();
    assert!(
        !keys
            .iter()
            .any(|key| key.contains("password") || key.contains("secret")),
        "{keys:?}"
    );
    for name in folder.names() {
        let text = fs::read_to_string(folder.sibling(&name)).unwrap();
        assert!(!text.contains(SECRET), "{name}");
    }
    let stored = secrets
        .load(profile.id, SecretSlot::Password)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.expose_secret(),
        SECRET,
        "and it is where it should be"
    );
}
