//! Saved connections. A profile holds everything needed to reach a
//! database except the password, which lives in a [`crate::ports::SecretStore`].

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::secret::Secret;

/// Stable identity of a saved connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(pub Uuid);

impl ConnectionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
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

/// Which driver opens a profile, such as `postgres`. A string rather than an
/// enum so drivers can be added without touching this crate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriverId(pub String);

impl DriverId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DriverId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Transport encryption, with libpq's names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Plain text. Only sensible on localhost.
    Disable,
    /// TLS when the server offers it, plain text otherwise.
    #[default]
    Prefer,
    /// TLS or fail. The server certificate is verified against the bundled
    /// web PKI roots.
    Require,
}

impl SslMode {
    pub const ALL: [Self; 3] = [Self::Disable, Self::Prefer, Self::Require];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disable => "Disable",
            Self::Prefer => "Prefer",
            Self::Require => "Require",
        }
    }
}

/// A saved connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    pub driver: DriverId,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    #[serde(default)]
    pub ssl: SslMode,
    /// Whether the password is kept in the keychain.
    #[serde(default)]
    pub save_password: bool,
    /// An accent to tell environments apart (production in red, say).
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Driver specific settings the generic form does not cover.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl ConnectionProfile {
    /// A blank profile for `driver`, listening on `port`.
    pub fn new(driver: DriverId, port: u16) -> Self {
        Self {
            id: ConnectionId::new(),
            name: String::new(),
            driver,
            host: "localhost".to_owned(),
            port,
            database: String::new(),
            username: String::new(),
            ssl: SslMode::default(),
            save_password: true,
            color: None,
            options: BTreeMap::new(),
        }
    }

    /// The name shown in lists: the profile's own, or `user@host/db`.
    #[must_use]
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{}@{}/{}", self.username, self.host, self.database)
        } else {
            self.name.clone()
        }
    }

    /// Checks the fields a connection cannot do without.
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("Host is required".to_owned());
        }
        if self.port == 0 {
            return Err("Port must be between 1 and 65535".to_owned());
        }
        if self.username.trim().is_empty() {
            return Err("User is required".to_owned());
        }
        Ok(())
    }
}

/// What a connection is opened with besides the profile.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub password: Option<Secret>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_names_the_missing_field() {
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        assert_eq!(profile.validate(), Err("User is required".to_owned()));
        profile.username = "app".to_owned();
        assert_eq!(profile.validate(), Ok(()));
    }
}
