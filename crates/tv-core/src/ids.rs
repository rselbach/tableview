use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable unique identifier for a saved database connection profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(Uuid);

impl ConnectionId {
    /// Generates a new random v4 connection identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Creates a connection identifier from an existing UUID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Returns the underlying UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ConnectionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

/// Stable unique identifier for a saved SQL query in local storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SavedQueryId(Uuid);

impl SavedQueryId {
    /// Generates a new random v4 saved query identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Creates a saved query identifier from an existing UUID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Returns the underlying UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for SavedQueryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SavedQueryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SavedQueryId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

/// Unique identifier for an open connection session in the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Unique identifier for a workspace tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

impl TabId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tab-{}", self.0)
    }
}

/// Monotonic identifier for matching asynchronous worker responses to their issuing request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "req-{}", self.0)
    }
}
