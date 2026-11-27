use std::cmp::Ordering;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::connection::ConnectionProfile;
use crate::ids::ConnectionId;

pub const STORE_SCHEMA_VERSION: u32 = 1;
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Persisted theme appearance preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearancePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// Persisted global application preferences stored in `settings.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_settings_version")]
    pub version: u32,
    #[serde(default)]
    pub appearance: AppearancePreference,
}

const fn default_settings_version() -> u32 {
    SETTINGS_SCHEMA_VERSION
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_SCHEMA_VERSION,
            appearance: AppearancePreference::System,
        }
    }
}

/// Ordering options for the Connection Launcher list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Name,
    Env,
    LastConnected,
}

impl SortOrder {
    pub const ALL: [Self; 3] = [Self::Name, Self::Env, Self::LastConnected];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Env => "Environment",
            Self::LastConnected => "Recent",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(label))
    }
}

/// Persisted connection profiles and folder metadata. Never contains passwords.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreData {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub connections: Vec<ConnectionProfile>,
}

const fn default_version() -> u32 {
    STORE_SCHEMA_VERSION
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            version: STORE_SCHEMA_VERSION,
            folders: Vec::new(),
            connections: Vec::new(),
        }
    }
}

impl StoreData {
    /// Inserts a new connection or updates an existing connection with the same ID.
    pub fn upsert(&mut self, profile: ConnectionProfile) {
        if let Some(folder) = &profile.folder {
            let trimmed = folder.trim();
            if !trimmed.is_empty() && !self.folders.iter().any(|f| f == trimmed) {
                self.folders.push(trimmed.to_owned());
            }
        }
        if let Some(existing) = self.connections.iter_mut().find(|c| c.id == profile.id) {
            *existing = profile;
        } else {
            self.connections.push(profile);
        }
    }

    /// Clones an existing connection with a new ID and a `" copy"` suffix directly after the original.
    pub fn duplicate(&mut self, id: ConnectionId) -> Option<ConnectionProfile> {
        let index = self.connections.iter().position(|c| c.id == id)?;
        let mut copy = self.connections[index].clone();
        copy.id = ConnectionId::new();
        copy.name = format!("{} copy", copy.name);
        copy.created_at = Utc::now();
        copy.last_connected_at = None;
        self.connections.insert(index + 1, copy.clone());
        Some(copy)
    }

    /// Removes a connection profile by ID. Returns `true` if a connection was removed.
    pub fn delete(&mut self, id: ConnectionId) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.id != id);
        self.connections.len() != before
    }

    /// Sorts stored connections in place using the selected sort order.
    pub fn sort(&mut self, order: SortOrder) {
        self.connections.sort_by(|a, b| match order {
            SortOrder::Name => cmp_name(a, b),
            SortOrder::Env => b.env.cmp(&a.env).then_with(|| cmp_name(a, b)),
            SortOrder::LastConnected => b
                .last_connected_at
                .cmp(&a.last_connected_at)
                .then_with(|| cmp_name(a, b)),
        });
    }

    /// Serializes connection profiles and folders to pretty JSON without credentials.
    pub fn export_json(&self) -> Result<String, StorageError> {
        serde_json::to_string_pretty(self).map_err(|e| StorageError::Serialize(e.to_string()))
    }

    /// Imports profiles from exported JSON, assigning fresh IDs when an ID already exists.
    pub fn import_json(&mut self, raw: &str) -> Result<usize, StorageError> {
        let imported: StoreData = serde_json::from_str(raw).map_err(|e| StorageError::Parse(e.to_string()))?;
        let mut added = 0;
        for mut profile in imported.connections {
            if self.connections.iter().any(|c| c.id == profile.id) {
                profile.id = ConnectionId::new();
            }
            self.upsert(profile);
            added += 1;
        }
        Ok(added)
    }
}

fn cmp_name(a: &ConnectionProfile, b: &ConnectionProfile) -> Ordering {
    a.name
        .to_ascii_lowercase()
        .cmp(&b.name.to_ascii_lowercase())
        .then_with(|| a.name.cmp(&b.name))
}

/// Atomic JSON persistence for connection profiles.
#[derive(Clone, Debug)]
pub struct ConnectionStore {
    path: PathBuf,
}

impl ConnectionStore {
    /// Resolves the platform configuration file path (`connections.json`).
    pub fn default_path() -> Result<PathBuf, StorageError> {
        let dirs = ProjectDirs::from("dev", "tableview", "TableView").ok_or(StorageError::NoConfigDir)?;
        Ok(dirs.config_dir().join("connections.json"))
    }

    /// Opens a store backed by the given path (used for custom paths and isolated tests).
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Opens the default platform connection store.
    pub fn open_default() -> Result<Self, StorageError> {
        Ok(Self::at(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads saved connections from disk, returning empty defaults if the file does not exist yet.
    pub fn load(&self) -> Result<StoreData, StorageError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| StorageError::Parse(e.to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreData::default()),
            Err(err) => Err(StorageError::Io(err.to_string())),
        }
    }

    /// Atomically writes connection data via a temporary sibling file and rename.
    pub fn save(&self, data: &StoreData) -> Result<(), StorageError> {
        atomic_write_json(&self.path, data, "connections.json")
    }
}

/// Atomic JSON persistence for global application settings.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Resolves the platform settings file path (`settings.json`).
    pub fn default_path() -> Result<PathBuf, StorageError> {
        let dirs = ProjectDirs::from("dev", "tableview", "TableView").ok_or(StorageError::NoConfigDir)?;
        Ok(dirs.config_dir().join("settings.json"))
    }

    /// Opens a store backed by the given path (used for custom paths and isolated tests).
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Opens the default platform settings store.
    pub fn open_default() -> Result<Self, StorageError> {
        Ok(Self::at(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads saved settings from disk, returning defaults if the file does not exist yet.
    pub fn load(&self) -> Result<AppSettings, StorageError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| StorageError::Parse(e.to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
            Err(err) => Err(StorageError::Io(err.to_string())),
        }
    }

    /// Atomically writes settings via a temporary sibling file and rename.
    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        atomic_write_json(&self.path, settings, "settings.json")
    }
}

fn atomic_write_json<T: Serialize>(path: &Path, data: &T, fallback_name: &str) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| StorageError::Io(e.to_string()))?;
    }
    let encoded = serde_json::to_vec_pretty(data).map_err(|e| StorageError::Serialize(e.to_string()))?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or(fallback_name),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp_path).map_err(|e| StorageError::Io(e.to_string()))?;
        file.write_all(&encoded).map_err(|e| StorageError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| StorageError::Io(e.to_string()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| StorageError::Io(e.to_string()))?;
    Ok(())
}

/// Errors produced when loading or persisting connection storage.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum StorageError {
    #[error("unable to locate platform configuration directory")]
    NoConfigDir,
    #[error("failed to read or write connections file: {0}")]
    Io(String),
    #[error("failed to parse connections file: {0}")]
    Parse(String),
    #[error("failed to serialize connections data: {0}")]
    Serialize(String),
}
