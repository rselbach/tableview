use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::ids::ConnectionId;

/// Supported relational database engine backends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbEngine {
    #[default]
    Postgres,
    Mysql,
    Sqlite,
}

impl DbEngine {
    pub const ALL: [Self; 3] = [Self::Postgres, Self::Mysql, Self::Sqlite];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Sqlite => "SQLite",
        }
    }

    pub const fn short_badge(self) -> &'static str {
        match self {
            Self::Postgres => "PG",
            Self::Mysql => "MY",
            Self::Sqlite => "SL",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Mysql => 3306,
            Self::Sqlite => 0,
        }
    }

    pub const fn default_user(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "root",
            Self::Sqlite => "",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        let trimmed = label.trim();
        Self::ALL.into_iter().find(|v| {
            v.label().eq_ignore_ascii_case(trimmed)
                || v.short_badge().eq_ignore_ascii_case(trimmed)
                || (*v == Self::Postgres && trimmed.eq_ignore_ascii_case("postgres"))
        })
    }

    pub const fn uses_schemas(self) -> bool {
        matches!(self, Self::Postgres)
    }

    pub const fn supports_multi_database(self) -> bool {
        matches!(self, Self::Postgres | Self::Mysql)
    }

    pub const fn supports_explain_analyze(self) -> bool {
        matches!(self, Self::Postgres | Self::Mysql)
    }

    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Self::Postgres | Self::Sqlite => format!("\"{}\"", ident.replace('"', "\"\"")),
            Self::Mysql => format!("`{}`", ident.replace('`', "``")),
        }
    }

    pub fn qualify_ident(self, schema: &str, name: &str) -> String {
        if schema.trim().is_empty() {
            self.quote_ident(name)
        } else {
            format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
        }
    }
}

/// Environment classification used to frame connections visually and inform safety defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvTag {
    #[default]
    Local,
    Development,
    Staging,
    Production,
}

impl EnvTag {
    pub const ALL: [Self; 4] = [Self::Local, Self::Development, Self::Staging, Self::Production];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::Development => "Development",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }

    pub const fn short_tag(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub const fn banner_tag(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Development => "DEV",
            Self::Staging => "STAGING",
            Self::Production => "PROD",
        }
    }

    pub const fn default_color(self) -> AccentColor {
        match self {
            Self::Local => AccentColor::Green,
            Self::Development => AccentColor::Blue,
            Self::Staging => AccentColor::Amber,
            Self::Production => AccentColor::Red,
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|v| v.label().eq_ignore_ascii_case(label) || v.short_tag().eq_ignore_ascii_case(label))
    }
}

/// User-selectable status accent color for a connection profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentColor {
    #[default]
    Neutral,
    Green,
    Blue,
    Amber,
    Red,
    Purple,
}

impl AccentColor {
    pub const ALL: [Self; 6] = [
        Self::Neutral,
        Self::Green,
        Self::Blue,
        Self::Amber,
        Self::Red,
        Self::Purple,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Neutral => "Neutral",
            Self::Green => "Green",
            Self::Blue => "Blue",
            Self::Amber => "Amber",
            Self::Red => "Red",
            Self::Purple => "Purple",
        }
    }
}

/// Execution guardrails enforced before sending mutating SQL to a connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyPolicy {
    #[default]
    Unrestricted,
    ConfirmWrites,
    StagedOnly,
    ReadOnly,
}

impl SafetyPolicy {
    pub const ALL: [Self; 4] = [
        Self::Unrestricted,
        Self::ConfirmWrites,
        Self::StagedOnly,
        Self::ReadOnly,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Unrestricted => "Silent Mode",
            Self::ConfirmWrites => "Confirm Writes",
            Self::StagedOnly => "Staged Diff Only",
            Self::ReadOnly => "Read-Only",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Unrestricted => "Send queries directly without confirmation",
            Self::ConfirmWrites => "Prompt before executing UPDATE, DELETE, or DDL",
            Self::StagedOnly => "Require staged edits review before committing writes",
            Self::ReadOnly => "Block all mutating statements in this session",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(label))
    }
}

/// PostgreSQL TLS verification mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    #[default]
    Prefer,
    Disable,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    pub const ALL: [Self; 5] = [
        Self::Disable,
        Self::Prefer,
        Self::Require,
        Self::VerifyCa,
        Self::VerifyFull,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Disable => "DISABLE",
            Self::Prefer => "PREFER",
            Self::Require => "REQUIRE",
            Self::VerifyCa => "VERIFY-CA",
            Self::VerifyFull => "VERIFY-FULL",
        }
    }

    pub const fn pg_param(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    pub fn from_pg_param(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disable" => Some(Self::Disable),
            "allow" | "prefer" => Some(Self::Prefer),
            "require" => Some(Self::Require),
            "verify-ca" => Some(Self::VerifyCa),
            "verify-full" => Some(Self::VerifyFull),
            _ => None,
        }
    }

    pub const fn mysql_param(self) -> &'static str {
        match self {
            Self::Disable => "disabled",
            Self::Prefer => "preferred",
            Self::Require => "required",
            Self::VerifyCa => "verify_ca",
            Self::VerifyFull => "verify_identity",
        }
    }

    pub fn from_mysql_param(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "disable" => Some(Self::Disable),
            "preferred" | "prefer" | "allow" => Some(Self::Prefer),
            "required" | "require" => Some(Self::Require),
            "verify_ca" | "verify-ca" => Some(Self::VerifyCa),
            "verify_identity" | "verify-identity" | "verify-full" | "verify_full" => Some(Self::VerifyFull),
            _ => None,
        }
    }
}

/// Optional TLS certificates and verification settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SslConfig {
    #[serde(default)]
    pub mode: SslMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<PathBuf>,
}

/// Where a connection's password is stored or obtained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordMode {
    #[default]
    Keychain,
    AskEveryTime,
    None,
}

impl PasswordMode {
    pub const ALL: [Self; 3] = [Self::Keychain, Self::AskEveryTime, Self::None];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Keychain => "Store in keychain",
            Self::AskEveryTime => "Ask every time",
            Self::None => "No password",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.label().eq_ignore_ascii_case(label))
    }
}

/// Saved database connection profile. Never holds plaintext passwords.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    #[serde(default)]
    pub engine: DbEngine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default)]
    pub env: EnvTag,
    #[serde(default)]
    pub color: AccentColor,
    #[serde(default)]
    pub safety: SafetyPolicy,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password_mode: PasswordMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    #[serde(default)]
    pub ssl: SslConfig,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<DateTime<Utc>>,
}

impl ConnectionProfile {
    /// Creates a new PostgreSQL connection profile with sensible local defaults.
    pub fn new_postgres(name: impl Into<String>, host: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            engine: DbEngine::Postgres,
            folder: None,
            env: EnvTag::Local,
            color: AccentColor::Green,
            safety: SafetyPolicy::Unrestricted,
            host: host.into(),
            port: 5432,
            user: user.into(),
            password_mode: PasswordMode::Keychain,
            database: None,
            file_path: None,
            ssl: SslConfig::default(),
            created_at: Utc::now(),
            last_connected_at: None,
        }
    }

    /// Creates a new MySQL connection profile with sensible local defaults.
    pub fn new_mysql(name: impl Into<String>, host: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            engine: DbEngine::Mysql,
            folder: None,
            env: EnvTag::Local,
            color: AccentColor::Green,
            safety: SafetyPolicy::Unrestricted,
            host: host.into(),
            port: 3306,
            user: user.into(),
            password_mode: PasswordMode::Keychain,
            database: None,
            file_path: None,
            ssl: SslConfig::default(),
            created_at: Utc::now(),
            last_connected_at: None,
        }
    }

    /// Creates a new SQLite connection profile targeting a local file path or `:memory:`.
    pub fn new_sqlite(name: impl Into<String>, file_path: impl Into<PathBuf>) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            engine: DbEngine::Sqlite,
            folder: None,
            env: EnvTag::Local,
            color: AccentColor::Green,
            safety: SafetyPolicy::Unrestricted,
            host: String::new(),
            port: 0,
            user: String::new(),
            password_mode: PasswordMode::None,
            database: None,
            file_path: Some(file_path.into()),
            ssl: SslConfig {
                mode: SslMode::Disable,
                ..Default::default()
            },
            created_at: Utc::now(),
            last_connected_at: None,
        }
    }

    /// Returns the OS credential service identifier for this profile's password.
    pub fn credential_service(&self) -> String {
        format!("dev.tableview.connection.{}", self.id)
    }

    /// Returns `true` when changing from `self` to `other` changes the database target,
    /// engine, transport, or authentication settings (requiring a fresh connection generation).
    pub fn connection_target_changed(&self, other: &Self) -> bool {
        self.engine != other.engine
            || self.host != other.host
            || self.port != other.port
            || self.user != other.user
            || self.password_mode != other.password_mode
            || self.database != other.database
            || self.file_path != other.file_path
            || self.ssl != other.ssl
    }

    /// Returns `true` when changing from `self` to `other` alters environment or safety rules.
    pub fn safety_context_changed(&self, other: &Self) -> bool {
        self.env != other.env || self.safety != other.safety
    }

    /// Validates required fields and TLS path invariants at the boundary before saving or connecting.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.name.trim().is_empty() {
            return Err(ValidationError::EmptyField("name"));
        }
        if self.engine == DbEngine::Sqlite {
            return match &self.file_path {
                Some(p) if !p.as_os_str().is_empty() => Ok(()),
                _ => Err(ValidationError::EmptyField("database file path")),
            };
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Err(ValidationError::EmptyField("host"));
        }
        if !host.starts_with('/') && self.port == 0 {
            return Err(ValidationError::InvalidPort);
        }
        if self.user.trim().is_empty() {
            return Err(ValidationError::EmptyField("user"));
        }
        if matches!(self.ssl.mode, SslMode::VerifyCa | SslMode::VerifyFull)
            && let Some(ca) = &self.ssl.ca_cert_path
            && !ca.exists()
        {
            return Err(ValidationError::MissingFile("CA certificate", ca.clone()));
        }
        match (&self.ssl.client_cert_path, &self.ssl.client_key_path) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(ValidationError::IncompleteClientCert);
            }
            (Some(cert), Some(key)) => {
                if !cert.exists() {
                    return Err(ValidationError::MissingFile("client certificate", cert.clone()));
                }
                if !key.exists() {
                    return Err(ValidationError::MissingFile("client key", key.clone()));
                }
            }
            (None, None) => {}
        }
        Ok(())
    }

    /// Parses any supported database URL (`postgres://`, `postgresql://`, `mysql://`, `mysql2://`, `sqlite://`, `sqlite3://`, `file:`).
    pub fn from_url(raw: &str) -> Result<(Self, Option<String>), UrlParseError> {
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            Self::from_postgres_url(trimmed)
        } else if lower.starts_with("mysql://") || lower.starts_with("mysql2://") {
            Self::from_mysql_url(trimmed)
        } else if lower.starts_with("sqlite://")
            || lower.starts_with("sqlite3://")
            || lower.starts_with("file:")
        {
            Self::from_sqlite_url(trimmed)
        } else if let Some(idx) = trimmed.find("://") {
            Err(UrlParseError::UnsupportedScheme(trimmed[..idx].to_owned()))
        } else {
            Err(UrlParseError::InvalidUrl(
                "missing URL scheme (expected postgres://, mysql://, or sqlite://)".to_owned(),
            ))
        }
    }

    /// Parses a `postgres://` or `postgresql://` URL, returning the profile and any extracted password separately.
    pub fn from_postgres_url(raw: &str) -> Result<(Self, Option<String>), UrlParseError> {
        let trimmed = raw.trim();
        let parsed = Url::parse(trimmed).map_err(|e| UrlParseError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "postgres" | "postgresql" => {}
            other => return Err(UrlParseError::UnsupportedScheme(other.to_owned())),
        }

        let mut query_host: Option<String> = None;
        let mut ssl = SslConfig::default();
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "host" if !value.trim().is_empty() => {
                    query_host = Some(value.into_owned());
                }
                "sslmode" => {
                    ssl.mode = SslMode::from_pg_param(&value)
                        .ok_or_else(|| UrlParseError::InvalidSslMode(value.into_owned()))?;
                }
                "sslrootcert" if !value.trim().is_empty() => {
                    ssl.ca_cert_path = Some(PathBuf::from(value.as_ref()));
                }
                "sslcert" if !value.trim().is_empty() => {
                    ssl.client_cert_path = Some(PathBuf::from(value.as_ref()));
                }
                "sslkey" if !value.trim().is_empty() => {
                    ssl.client_key_path = Some(PathBuf::from(value.as_ref()));
                }
                _ => {}
            }
        }

        let host = query_host
            .or_else(|| parsed.host_str().map(str::to_owned))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_owned());
        let port = parsed.port().unwrap_or(5432);

        let user = if parsed.username().is_empty() {
            "postgres".to_owned()
        } else {
            percent_decode(parsed.username())
        };

        let secret = parsed.password().map(percent_decode).filter(|s| !s.is_empty());
        let password_mode = if secret.is_some() {
            PasswordMode::Keychain
        } else {
            PasswordMode::None
        };

        let database = parsed
            .path()
            .trim_start_matches('/')
            .split('/')
            .next()
            .map(percent_decode)
            .filter(|db| !db.is_empty());

        let name = match &database {
            Some(db) => format!("{db} ({host})"),
            None => host.clone(),
        };

        let env = if host == "127.0.0.1" || host == "localhost" || host.starts_with('/') {
            EnvTag::Local
        } else {
            EnvTag::Development
        };

        let mut profile = Self::new_postgres(name, host, user);
        profile.port = port;
        profile.password_mode = password_mode;
        profile.database = database;
        profile.ssl = ssl;
        profile.env = env;
        profile.color = env.default_color();
        Ok((profile, secret))
    }

    /// Parses a `mysql://` or `mysql2://` URL, returning the profile and any extracted password separately.
    pub fn from_mysql_url(raw: &str) -> Result<(Self, Option<String>), UrlParseError> {
        let trimmed = raw.trim();
        let parsed = Url::parse(trimmed).map_err(|e| UrlParseError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "mysql" | "mysql2" => {}
            other => return Err(UrlParseError::UnsupportedScheme(other.to_owned())),
        }

        let mut query_host: Option<String> = None;
        let mut ssl = SslConfig::default();
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "host" | "socket" if !value.trim().is_empty() => {
                    query_host = Some(value.into_owned());
                }
                "ssl-mode" | "sslmode" => {
                    ssl.mode = SslMode::from_mysql_param(&value)
                        .ok_or_else(|| UrlParseError::InvalidSslMode(value.into_owned()))?;
                }
                "ssl-ca" | "sslrootcert" if !value.trim().is_empty() => {
                    ssl.ca_cert_path = Some(PathBuf::from(value.as_ref()));
                }
                "ssl-cert" | "sslcert" if !value.trim().is_empty() => {
                    ssl.client_cert_path = Some(PathBuf::from(value.as_ref()));
                }
                "ssl-key" | "sslkey" if !value.trim().is_empty() => {
                    ssl.client_key_path = Some(PathBuf::from(value.as_ref()));
                }
                _ => {}
            }
        }

        let host = query_host
            .or_else(|| parsed.host_str().map(str::to_owned))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_owned());
        let port = parsed.port().unwrap_or(3306);

        let user = if parsed.username().is_empty() {
            "root".to_owned()
        } else {
            percent_decode(parsed.username())
        };

        let secret = parsed.password().map(percent_decode).filter(|s| !s.is_empty());
        let password_mode = if secret.is_some() {
            PasswordMode::Keychain
        } else {
            PasswordMode::None
        };

        let database = parsed
            .path()
            .trim_start_matches('/')
            .split('/')
            .next()
            .map(percent_decode)
            .filter(|db| !db.is_empty());

        let name = match &database {
            Some(db) => format!("{db} ({host})"),
            None => host.clone(),
        };

        let env = if host == "127.0.0.1" || host == "localhost" || host.starts_with('/') {
            EnvTag::Local
        } else {
            EnvTag::Development
        };

        let mut profile = Self::new_mysql(name, host, user);
        profile.port = port;
        profile.password_mode = password_mode;
        profile.database = database;
        profile.ssl = ssl;
        profile.env = env;
        profile.color = env.default_color();
        Ok((profile, secret))
    }

    /// Parses a `sqlite://`, `sqlite3://`, or `file:` URL into a SQLite [`ConnectionProfile`].
    pub fn from_sqlite_url(raw: &str) -> Result<(Self, Option<String>), UrlParseError> {
        let trimmed = raw.trim();
        let rest = if let Some(r) = strip_scheme_ci(trimmed, "sqlite://") {
            r
        } else if let Some(r) = strip_scheme_ci(trimmed, "sqlite3://") {
            r
        } else if let Some(r) = strip_scheme_ci(trimmed, "file://") {
            r
        } else if let Some(r) = strip_scheme_ci(trimmed, "file:") {
            r
        } else if let Some(idx) = trimmed.find("://") {
            return Err(UrlParseError::UnsupportedScheme(trimmed[..idx].to_owned()));
        } else if let Some(idx) = trimmed.find(':') {
            return Err(UrlParseError::UnsupportedScheme(trimmed[..idx].to_owned()));
        } else {
            return Err(UrlParseError::InvalidUrl(
                "missing sqlite://, sqlite3://, or file: scheme".to_owned(),
            ));
        };

        let path_part = rest.split(['?', '#']).next().unwrap_or(rest);
        let decoded = percent_decode(path_part);
        if decoded.trim().is_empty() {
            return Err(UrlParseError::InvalidUrl("empty SQLite database path".to_owned()));
        }

        let path_buf = PathBuf::from(&decoded);
        let name = if decoded == ":memory:" {
            "SQLite (memory)".to_owned()
        } else {
            path_buf
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| decoded.clone())
        };

        Ok((Self::new_sqlite(name, path_buf), None))
    }

    /// Formats a shareable URL using the profile's configured engine.
    pub fn to_url(&self, include_placeholder_password: bool) -> String {
        match self.engine {
            DbEngine::Postgres => self.to_postgres_url(include_placeholder_password),
            DbEngine::Mysql => self.to_mysql_url(include_placeholder_password),
            DbEngine::Sqlite => self.to_sqlite_url(),
        }
    }

    /// Formats a shareable PostgreSQL connection URL without ever embedding plaintext credentials.
    pub fn to_postgres_url(&self, include_placeholder_password: bool) -> String {
        let mut url = String::from("postgresql://");
        url.push_str(&percent_encode(&self.user));
        if include_placeholder_password && self.password_mode != PasswordMode::None {
            url.push_str(":********");
        }
        url.push('@');
        if self.host.starts_with('/') {
            url.push_str("localhost");
        } else if self.host.contains(':') && !self.host.starts_with('[') {
            url.push('[');
            url.push_str(&self.host);
            url.push(']');
        } else {
            url.push_str(&self.host);
        }
        url.push(':');
        url.push_str(&self.port.to_string());
        if let Some(db) = &self.database
            && !db.trim().is_empty()
        {
            url.push('/');
            url.push_str(&percent_encode(db.trim()));
        }

        let mut params = vec![format!("sslmode={}", self.ssl.mode.pg_param())];
        if self.host.starts_with('/') {
            params.push(format!("host={}", percent_encode(&self.host)));
        }
        if let Some(ca) = &self.ssl.ca_cert_path {
            params.push(format!("sslrootcert={}", percent_encode(&ca.to_string_lossy())));
        }
        if let Some(cert) = &self.ssl.client_cert_path {
            params.push(format!("sslcert={}", percent_encode(&cert.to_string_lossy())));
        }
        if let Some(key) = &self.ssl.client_key_path {
            params.push(format!("sslkey={}", percent_encode(&key.to_string_lossy())));
        }
        url.push('?');
        url.push_str(&params.join("&"));
        url
    }

    /// Formats a shareable MySQL connection URL without ever embedding plaintext credentials.
    pub fn to_mysql_url(&self, include_placeholder_password: bool) -> String {
        let mut url = String::from("mysql://");
        url.push_str(&percent_encode(&self.user));
        if include_placeholder_password && self.password_mode != PasswordMode::None {
            url.push_str(":********");
        }
        url.push('@');
        if self.host.starts_with('/') {
            url.push_str("localhost");
        } else if self.host.contains(':') && !self.host.starts_with('[') {
            url.push('[');
            url.push_str(&self.host);
            url.push(']');
        } else {
            url.push_str(&self.host);
        }
        url.push(':');
        url.push_str(&self.port.to_string());
        if let Some(db) = &self.database
            && !db.trim().is_empty()
        {
            url.push('/');
            url.push_str(&percent_encode(db.trim()));
        }

        let mut params = vec![format!("ssl-mode={}", self.ssl.mode.mysql_param())];
        if self.host.starts_with('/') {
            params.push(format!("socket={}", percent_encode(&self.host)));
        }
        if let Some(ca) = &self.ssl.ca_cert_path {
            params.push(format!("ssl-ca={}", percent_encode(&ca.to_string_lossy())));
        }
        if let Some(cert) = &self.ssl.client_cert_path {
            params.push(format!("ssl-cert={}", percent_encode(&cert.to_string_lossy())));
        }
        if let Some(key) = &self.ssl.client_key_path {
            params.push(format!("ssl-key={}", percent_encode(&key.to_string_lossy())));
        }
        url.push('?');
        url.push_str(&params.join("&"));
        url
    }

    /// Formats a shareable SQLite connection URL.
    pub fn to_sqlite_url(&self) -> String {
        let path_str = self
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path_str == ":memory:" {
            "sqlite://:memory:".to_owned()
        } else {
            format!("sqlite://{}", percent_encode_path(&path_str))
        }
    }

    /// Compact host summary for launcher subtitles.
    pub fn endpoint_summary(&self) -> String {
        if self.engine == DbEngine::Sqlite {
            return self
                .file_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| ":memory:".to_owned());
        }
        let base = if self.host.starts_with('/') || self.port == self.engine.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        match &self.database {
            Some(db) if !db.trim().is_empty() => format!("{base} : {}", db.trim()),
            _ => base,
        }
    }
}

fn strip_scheme_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if input.len() >= prefix.len() && input.is_char_boundary(prefix.len()) {
        let (head, tail) = input.split_at(prefix.len());
        if head.eq_ignore_ascii_case(prefix) {
            return Some(tail);
        }
    }
    None
}

fn percent_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let is_safe = byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':');
        if is_safe {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(hex_nibble(byte >> 4));
            out.push(hex_nibble(byte & 0x0f));
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(hex_nibble(byte >> 4));
            out.push(hex_nibble(byte & 0x0f));
        }
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => '0',
    }
}

/// Errors returned when validating a [`ConnectionProfile`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("connection {0} cannot be empty")]
    EmptyField(&'static str),
    #[error("port must be between 1 and 65535")]
    InvalidPort,
    #[error("{0} file not found: {1}")]
    MissingFile(&'static str, PathBuf),
    #[error("client certificate and client key must both be provided")]
    IncompleteClientCert,
}

/// Errors returned when parsing a PostgreSQL connection URL.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UrlParseError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme '{0}' (expected postgres:// or postgresql://)")]
    UnsupportedScheme(String),
    #[error("unsupported sslmode '{0}'")]
    InvalidSslMode(String),
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn db_engine_metadata_and_quoting() {
        assert_eq!(DbEngine::ALL, [DbEngine::Postgres, DbEngine::Mysql, DbEngine::Sqlite]);
        assert_eq!(DbEngine::Postgres.short_badge(), "PG");
        assert_eq!(DbEngine::Mysql.short_badge(), "MY");
        assert_eq!(DbEngine::Sqlite.short_badge(), "SL");
        assert_eq!(DbEngine::from_label("mysql"), Some(DbEngine::Mysql));
        assert_eq!(DbEngine::from_label("SL"), Some(DbEngine::Sqlite));
        assert!(DbEngine::Postgres.uses_schemas());
        assert!(!DbEngine::Mysql.uses_schemas());
        assert!(!DbEngine::Sqlite.uses_schemas());
        assert!(DbEngine::Mysql.supports_multi_database());
        assert!(!DbEngine::Sqlite.supports_multi_database());
        assert!(DbEngine::Mysql.supports_explain_analyze());
        assert!(!DbEngine::Sqlite.supports_explain_analyze());
        assert_eq!(DbEngine::Mysql.quote_ident("weird`col"), "`weird``col`");
        assert_eq!(DbEngine::Mysql.qualify_ident("db", "tbl"), "`db`.`tbl`");
        assert_eq!(DbEngine::Sqlite.qualify_ident("", "tbl"), "\"tbl\"");
    }

    #[test]
    fn mysql_and_sqlite_urls_parse_validate_and_roundtrip() {
        let (mysql, pw) = ConnectionProfile::from_url(
            "mysql://abed:cool%20cool@greendale.internal:3307/study_db?ssl-mode=VERIFY_IDENTITY",
        )
        .expect("mysql url");
        assert_eq!(mysql.engine, DbEngine::Mysql);
        assert_eq!(mysql.user, "abed");
        assert_eq!(pw.as_deref(), Some("cool cool"));
        assert_eq!(mysql.port, 3307);
        assert_eq!(mysql.database.as_deref(), Some("study_db"));
        assert_eq!(mysql.ssl.mode, SslMode::VerifyFull);
        assert!(mysql.validate().is_ok());
        let exported = mysql.to_url(true);
        assert!(exported.starts_with("mysql://abed:********@greendale.internal:3307/study_db"));
        assert!(exported.contains("ssl-mode=verify_identity"));

        let (mem, mem_pw) = ConnectionProfile::from_url("sqlite://:memory:").expect("sqlite memory");
        assert_eq!(mem.engine, DbEngine::Sqlite);
        assert_eq!(mem.name, "SQLite (memory)");
        assert!(mem_pw.is_none());
        assert!(mem.validate().is_ok());
        assert_eq!(mem.endpoint_summary(), ":memory:");
        assert_eq!(mem.to_url(false), "sqlite://:memory:");

        let (file_prof, _) =
            ConnectionProfile::from_sqlite_url("sqlite:///tmp/greendale%20campus.db").expect("sqlite file");
        assert_eq!(file_prof.name, "greendale campus");
        assert_eq!(file_prof.file_path, Some(PathBuf::from("/tmp/greendale campus.db")));
        assert!(file_prof.validate().is_ok());

        let (rel_prof, _) = ConnectionProfile::from_sqlite_url("sqlite://./local.sqlite3").expect("sqlite rel");
        assert_eq!(rel_prof.file_path, Some(PathBuf::from("./local.sqlite3")));

        let mut empty_sqlite = ConnectionProfile::new_sqlite("Empty", "");
        assert_eq!(
            empty_sqlite.validate(),
            Err(ValidationError::EmptyField("database file path"))
        );
        empty_sqlite.file_path = None;
        assert_eq!(
            empty_sqlite.validate(),
            Err(ValidationError::EmptyField("database file path"))
        );
    }
}
