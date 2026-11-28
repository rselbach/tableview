use std::cmp::Ordering;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
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
    #[serde(default = "default_record_query_history")]
    pub record_query_history: bool,
}

const fn default_settings_version() -> u32 {
    SETTINGS_SCHEMA_VERSION
}

const fn default_record_query_history() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_SCHEMA_VERSION,
            appearance: AppearancePreference::System,
            record_query_history: true,
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

    /// Loads saved connections from disk, returning empty defaults when the file does not exist yet.
    pub fn load(&self) -> Result<StoreData, StorageError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| StorageError::Parse(e.to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                check_missing_or_dangling_symlink(&self.path, "connections file")?;
                Ok(StoreData::default())
            }
            Err(err) => Err(StorageError::Io(err.to_string())),
        }
    }

    /// Moves an existing corrupt or unreadable store out of the way to a unique sibling backup path
    /// and initializes an empty store in its place.
    pub fn backup_and_reset(&self) -> Result<Option<PathBuf>, StorageError> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {
                let parent = self
                    .path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let file_name = self
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("connections.json");
                let stamp = Utc::now().timestamp_millis();
                let mut backup_path = None;
                for attempt in 0u32..1000 {
                    let suffix = if attempt == 0 {
                        format!("{file_name}.backup-{stamp}")
                    } else {
                        format!("{file_name}.backup-{stamp}-{attempt}")
                    };
                    let candidate = parent.join(suffix);
                    match fs::symlink_metadata(&candidate) {
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            backup_path = Some(candidate);
                            break;
                        }
                        Ok(_) => continue,
                        Err(err) => return Err(StorageError::Io(err.to_string())),
                    }
                }
                let backup_path = backup_path.ok_or_else(|| {
                    StorageError::Io("unable to allocate unique backup path for connections file".to_owned())
                })?;
                fs::rename(&self.path, &backup_path).map_err(|e| StorageError::Io(e.to_string()))?;
                if let Err(save_err) = self.save(&StoreData::default()) {
                    if let Err(restore_err) = fs::rename(&backup_path, &self.path) {
                        return Err(StorageError::Io(format!(
                            "{save_err}; also failed to restore backup from {}: {restore_err}",
                            backup_path.display()
                        )));
                    }
                    return Err(save_err);
                }
                Ok(Some(backup_path))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.save(&StoreData::default())?;
                Ok(None)
            }
            Err(err) => Err(StorageError::Io(err.to_string())),
        }
    }

    /// Atomically writes connection data via a temporary sibling file and rename.
    pub fn save(&self, data: &StoreData) -> Result<(), StorageError> {
        atomic_write_json(&self.path, data)
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                check_missing_or_dangling_symlink(&self.path, "settings file")?;
                Ok(AppSettings::default())
            }
            Err(err) => Err(StorageError::Io(err.to_string())),
        }
    }

    /// Atomically writes settings via a temporary sibling file and rename.
    pub fn save(&self, settings: &AppSettings) -> Result<(), StorageError> {
        atomic_write_json(&self.path, settings)
    }
}

fn check_missing_or_dangling_symlink(path: &Path, label: &str) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(StorageError::Io(format!(
            "{label} {} is a dangling symbolic link",
            path.display()
        ))),
        Err(meta_err) if meta_err.kind() == std::io::ErrorKind::NotFound => {
            if path_or_ancestor_is_dangling_symlink(path) {
                Err(StorageError::Io(format!(
                    "{label} {} traverses a dangling symbolic link",
                    path.display()
                )))
            } else {
                Ok(())
            }
        }
        Err(meta_err) => Err(StorageError::Io(meta_err.to_string())),
    }
}

fn path_or_ancestor_is_dangling_symlink(path: &Path) -> bool {
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
            && fs::metadata(ancestor).is_err()
        {
            return true;
        }
    }
    false
}

fn resolve_atomic_write_target(path: &Path) -> std::io::Result<PathBuf> {
    const MAX_SYMLINK_HOPS: usize = 40;
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link_target = fs::read_link(&current)?;
                current = if link_target.is_absolute() {
                    link_target
                } else {
                    match current.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => parent.join(link_target),
                        _ => Path::new(".").join(link_target),
                    }
                };
            }
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("too many levels of symbolic links while resolving {}", path.display()),
    ))
}

/// Atomically writes `bytes` to `path` (following symlinks) via a unique same-directory temporary file,
/// `sync_all`, metadata/ACL preservation, and rename.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = resolve_atomic_write_target(path)?;
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;
    let existing_meta = match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_file() => Some(meta),
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                format!("cannot atomically write to directory {}", target.display()),
            ));
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cannot atomically replace non-regular file {}", target.display()),
            ));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    if existing_meta.is_some() {
        // Require write permission on the destination before creating a replacement inode.
        drop(fs::OpenOptions::new().write(true).open(&target)?);
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".tableview-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    if let Some(dest_meta) = existing_meta {
        #[cfg(unix)]
        {
            let tmp_meta = tmp.as_file().metadata()?;
            let owner = (dest_meta.uid() != tmp_meta.uid()).then(|| rustix::fs::Uid::from_raw(dest_meta.uid()));
            let group = (dest_meta.gid() != tmp_meta.gid()).then(|| rustix::fs::Gid::from_raw(dest_meta.gid()));
            if owner.is_some() || group.is_some() {
                rustix::fs::fchown(tmp.as_file(), owner, group)
                    .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
            }
        }
        tmp.as_file().set_permissions(dest_meta.permissions())?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        preserve_xattrs(&target, tmp.as_file())?;
        #[cfg(target_os = "macos")]
        preserve_macos_acl(&target, tmp.path())?;
    }
    if let Err(persist_err) = tmp.persist(&target) {
        #[cfg(target_os = "macos")]
        clear_macos_acl_best_effort(persist_err.file.path());
        return Err(persist_err.error);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_missing_xattr(err: rustix::io::Errno) -> bool {
    // macOS ENOATTR = 93, Linux ENODATA = 61
    err == rustix::io::Errno::NOENT || err.raw_os_error() == 93 || err.raw_os_error() == 61
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn list_xattr_names(path: &Path) -> std::io::Result<Vec<u8>> {
    for _ in 0..4 {
        let size = match rustix::fs::listxattr_query(path) {
            Ok(size) => size,
            Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS | rustix::io::Errno::NOENT) => {
                return Ok(Vec::new());
            }
            Err(err) => return Err(std::io::Error::from_raw_os_error(err.raw_os_error())),
        };
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size];
        match rustix::fs::listxattr(path, &mut *buf) {
            Ok(len) => {
                buf.truncate(len);
                return Ok(buf);
            }
            Err(rustix::io::Errno::RANGE) => continue,
            Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS | rustix::io::Errno::NOENT) => {
                return Ok(Vec::new());
            }
            Err(err) => return Err(std::io::Error::from_raw_os_error(err.raw_os_error())),
        }
    }
    Err(std::io::Error::from_raw_os_error(
        rustix::io::Errno::RANGE.raw_os_error(),
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_xattr_value(path: &Path, name: &std::ffi::CStr) -> std::io::Result<Option<Vec<u8>>> {
    for _ in 0..4 {
        let size = match rustix::fs::getxattr_query(path, name) {
            Ok(size) => size,
            Err(err) if is_missing_xattr(err) => return Ok(None),
            Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS) => return Ok(None),
            Err(err) => return Err(std::io::Error::from_raw_os_error(err.raw_os_error())),
        };
        if size == 0 {
            return Ok(Some(Vec::new()));
        }
        let mut val = vec![0u8; size];
        match rustix::fs::getxattr(path, name, &mut *val) {
            Ok(len) => {
                val.truncate(len);
                return Ok(Some(val));
            }
            Err(rustix::io::Errno::RANGE) => continue,
            Err(err) if is_missing_xattr(err) => return Ok(None),
            Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS) => return Ok(None),
            Err(err) => return Err(std::io::Error::from_raw_os_error(err.raw_os_error())),
        }
    }
    Err(std::io::Error::from_raw_os_error(
        rustix::io::Errno::RANGE.raw_os_error(),
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn preserve_xattrs(src: &Path, dst_file: &std::fs::File) -> std::io::Result<()> {
    let names = list_xattr_names(src)?;
    for raw_name in names.split(|&b| b == 0) {
        if raw_name.is_empty() {
            continue;
        }
        #[cfg(target_os = "macos")]
        if raw_name == b"com.apple.provenance" || raw_name == b"com.apple.macl" {
            continue;
        }
        let Ok(c_name) = std::ffi::CString::new(raw_name) else {
            continue;
        };
        let Some(value) = read_xattr_value(src, &c_name)? else {
            continue;
        };
        if let Err(err) =
            rustix::fs::fsetxattr(dst_file, &c_name, &value, rustix::fs::XattrFlags::empty())
            && !matches!(err, rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS)
        {
            return Err(std::io::Error::from_raw_os_error(err.raw_os_error()));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_macos_acl_best_effort(path: &Path) {
    drop(exacl::setfacl(&[path], &[], None));
}

#[cfg(target_os = "macos")]
fn preserve_macos_acl(src: &Path, dst: &Path) -> std::io::Result<()> {
    let src_acls = exacl::getfacl(src, None)?;
    let dst_acls = exacl::getfacl(dst, None)?;
    if src_acls == dst_acls {
        return Ok(());
    }
    exacl::setfacl(&[dst], &src_acls, None)
}

fn atomic_write_json<T: Serialize>(path: &Path, data: &T) -> Result<(), StorageError> {
    let encoded = serde_json::to_vec_pretty(data).map_err(|e| StorageError::Serialize(e.to_string()))?;
    atomic_write_bytes(path, &encoded).map_err(|e| StorageError::Io(e.to_string()))
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
