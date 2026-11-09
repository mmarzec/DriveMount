// SPDX-License-Identifier: GPL-3.0-or-later

//! Between the text of the file and a [`Config`]: versions and migrations, and how far one fault
//! reaches.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml::{Table, Value};
use uuid::Uuid;

use super::record::ProfileRecord;
use super::{Config, ReadOnlyReason, SaveError, SettingProblem, Settings, UnreadableProfile};

/// The versions a build understands.
#[derive(Debug, Clone, Copy)]
pub(super) struct Schema {
    /// The version it writes.
    pub current: u32,
    /// `steps[n]` takes version `n + 1` to version `n + 2`.
    pub steps: &'static [Step],
}

/// One migration. It changes the file as a table, so it does not depend on the types of the day.
pub(super) type Step = fn(&mut Table) -> Result<(), String>;

/// What the store keeps from the file and the model has no place for (spec §6.1: unknown keys).
#[derive(Debug, Default)]
pub(super) struct Kept {
    /// Unknown keys at the top of the file.
    pub top: Table,
    /// Unknown keys in `[settings]`.
    pub settings: Table,
    /// The profiles as they are on file, for their unknown keys and for what their auth method has
    /// no use for.
    pub records: HashMap<Uuid, ProfileRecord>,
}

/// A file that could be read.
#[derive(Debug)]
pub(super) struct Decoded {
    /// Its `schema_version`, clamped to what fits, and not the version it was migrated to.
    pub version: u32,
    found: i64,
    current: u32,
    pub config: Config,
    pub kept: Kept,
    pub setting_problems: Vec<SettingProblem>,
}

impl Decoded {
    /// The reason it must not be written, if it is from a newer version.
    pub fn newer(&self) -> Option<ReadOnlyReason> {
        (self.found > i64::from(self.current))
            .then_some(ReadOnlyReason::Newer { found: self.found })
    }
}

/// Reads a file.
///
/// # Errors
///
/// When none of it can be trusted, which means a read-only app.
pub(super) fn decode(schema: &Schema, bytes: &[u8]) -> Result<Decoded, ReadOnlyReason> {
    let text = str::from_utf8(bytes).map_err(|_| damaged("it is not UTF-8"))?;
    let mut table: Table = text
        .parse()
        .map_err(|error: toml::de::Error| damaged(error.to_string()))?;

    let found = read_version(&table)?;
    let newer = found > i64::from(schema.current);
    let version = u32::try_from(found).unwrap_or(u32::MAX);
    if !newer {
        migrate(schema, version, &mut table)?;
    }

    let shaped = shape(table);
    // A newer file may not have the shape this version expects, and only has to be shown if it can be.
    let (config, kept, setting_problems) = if newer {
        shaped.unwrap_or_default()
    } else {
        shaped?
    };
    Ok(Decoded {
        version,
        found,
        current: schema.current,
        config,
        kept,
        setting_problems,
    })
}

/// A file with no version did not come from this program, and may be what is left of one that did:
/// an empty file is what a crashed editor leaves.
fn read_version(table: &Table) -> Result<i64, ReadOnlyReason> {
    match table.get("schema_version") {
        None => Err(damaged("it has no schema_version")),
        Some(Value::Integer(version)) if *version >= 1 => Ok(*version),
        Some(other) => Err(damaged(format!("schema_version cannot be {other}"))),
    }
}

fn migrate(schema: &Schema, from: u32, table: &mut Table) -> Result<(), ReadOnlyReason> {
    for version in from..schema.current {
        let step = usize::try_from(version - 1)
            .ok()
            .and_then(|index| schema.steps.get(index));
        let Some(step) = step else {
            return Err(ReadOnlyReason::MigrationFailed {
                from: version,
                detail: "there is no migration for this version".to_owned(),
            });
        };
        step(table).map_err(|detail| ReadOnlyReason::MigrationFailed {
            from: version,
            detail,
        })?;
    }
    Ok(())
}

fn shape(mut table: Table) -> Result<(Config, Kept, Vec<SettingProblem>), ReadOnlyReason> {
    table.remove("schema_version");
    let mut settings = match table.remove("settings") {
        None => Table::new(),
        Some(Value::Table(settings)) => settings,
        Some(_) => return Err(damaged("settings is not a table")),
    };
    let entries = match table.remove("profiles") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(damaged("profiles is not a list of tables")),
    };

    let mut setting_problems = Vec::new();
    let mut config = Config {
        settings: read_settings(&mut settings, &mut setting_problems),
        ..Config::default()
    };
    let mut kept = Kept {
        top: table,
        settings,
        records: HashMap::new(),
    };
    let mut ids = HashSet::new();
    for entry in entries {
        let Value::Table(raw) = entry else {
            return Err(damaged("profiles is not a list of tables"));
        };
        match read_profile(&raw, &mut ids) {
            Ok((profile, record)) => {
                kept.records.insert(profile.id, record);
                config.profiles.push(profile);
            }
            Err(problem) => config.unreadable.push(unreadable(raw, problem)),
        }
    }
    Ok((config, kept, setting_problems))
}

fn read_settings(table: &mut Table, problems: &mut Vec<SettingProblem>) -> Settings {
    let defaults = Settings::default();
    let mut reader = SettingReader { table, problems };
    Settings {
        start_with_windows: reader.read("start_with_windows", defaults.start_with_windows),
        close_to_tray: reader.read("close_to_tray", defaults.close_to_tray),
        theme: reader.read("theme", defaults.theme),
        letter_conflict: reader.read("letter_conflict", defaults.letter_conflict),
        notifications: reader.read("notifications", defaults.notifications),
        notification_sound: reader.read("notification_sound", defaults.notification_sound),
        log_level: reader.read("log_level", defaults.log_level),
        op_timeout_secs: reader.read_in_range("op_timeout_secs", defaults.op_timeout_secs, 5..=120),
        winfsp_threads: reader.read_in_range("winfsp_threads", defaults.winfsp_threads, 0..=8),
    }
}

/// Takes the known keys out of the table, which leaves the unknown ones.
struct SettingReader<'a> {
    table: &'a mut Table,
    problems: &'a mut Vec<SettingProblem>,
}

impl SettingReader<'_> {
    fn read<T: DeserializeOwned>(&mut self, key: &'static str, default: T) -> T {
        let Some(value) = self.table.remove(key) else {
            return default;
        };
        value.try_into().unwrap_or_else(|error: toml::de::Error| {
            self.problems.push(SettingProblem {
                key,
                detail: error.message().to_owned(),
            });
            default
        })
    }

    fn read_in_range<T>(
        &mut self,
        key: &'static str,
        default: T,
        range: std::ops::RangeInclusive<T>,
    ) -> T
    where
        T: DeserializeOwned + PartialOrd + Copy + std::fmt::Display,
    {
        let value = self.read(key, default);
        if range.contains(&value) {
            return value;
        }
        self.problems.push(SettingProblem {
            key,
            detail: format!(
                "{value} is not in the range {} to {}",
                range.start(),
                range.end()
            ),
        });
        default
    }
}

fn read_profile(
    raw: &Table,
    ids: &mut HashSet<Uuid>,
) -> Result<(crate::profile::Profile, ProfileRecord), String> {
    let record = ProfileRecord::read(raw).map_err(|error| error.message().to_owned())?;
    let profile = record.to_profile()?;
    // Two profiles under one id would share their secrets, and FR-PROF-02 says an id is one profile's.
    if !ids.insert(profile.id) {
        return Err("another profile has the same id".to_owned());
    }
    Ok((profile, record))
}

fn unreadable(raw: Table, problem: String) -> UnreadableProfile {
    UnreadableProfile {
        name: raw.get("name").and_then(Value::as_str).map(str::to_owned),
        id: raw
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok()),
        problem,
        raw,
    }
}

fn damaged(detail: impl Into<String>) -> ReadOnlyReason {
    ReadOnlyReason::Damaged {
        detail: detail.into(),
    }
}

/// The file's layout. Known keys come first, in the order spec §6.1 shows.
#[derive(Serialize)]
struct FileOut<'a> {
    schema_version: u32,
    #[serde(flatten)]
    top: &'a Table,
    settings: SettingsOut<'a>,
    profiles: Vec<ProfileOut<'a>>,
}

#[derive(Serialize)]
struct SettingsOut<'a> {
    #[serde(flatten)]
    settings: &'a Settings,
    #[serde(flatten)]
    extra: &'a Table,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ProfileOut<'a> {
    Read(&'a ProfileRecord),
    AsItWas(&'a Table),
}

/// The text of the file, and the records it was made from.
pub(super) fn encode(
    version: u32,
    config: &Config,
    kept: &Kept,
) -> Result<(String, HashMap<Uuid, ProfileRecord>), SaveError> {
    let mut records = Vec::with_capacity(config.profiles.len());
    for profile in &config.profiles {
        records.push(ProfileRecord::from_profile(
            profile,
            kept.records.get(&profile.id),
        )?);
    }

    let profiles = records
        .iter()
        .map(ProfileOut::Read)
        .chain(
            config
                .unreadable
                .iter()
                .map(|entry| ProfileOut::AsItWas(&entry.raw)),
        )
        .collect();
    let text = toml::to_string_pretty(&FileOut {
        schema_version: version,
        top: &kept.top,
        settings: SettingsOut {
            settings: &config.settings,
            extra: &kept.settings,
        },
        profiles,
    })?;

    let records = config
        .profiles
        .iter()
        .map(|profile| profile.id)
        .zip(records)
        .collect();
    Ok((text, records))
}
