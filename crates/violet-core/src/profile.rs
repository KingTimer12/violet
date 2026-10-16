//! Saved connections. A profile holds everything needed to reach a
//! database except the password, which lives in a [`crate::ports::SecretStore`].

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

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
    /// TLS when the server offers it, plain text otherwise — but only on
    /// this machine. Anyone on the path to a remote server could refuse TLS
    /// on its behalf and read the password, so off loopback `Prefer` is
    /// enforced as [`SslMode::Require`]; see [`SslMode::effective`].
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

    /// The mode a driver must actually use to reach `host`.
    ///
    /// Falling back to plain text is a downgrade an attacker can force, so
    /// it is only allowed for loopback hosts and Unix sockets. Choosing
    /// [`SslMode::Disable`] explicitly is still honoured.
    #[must_use]
    pub fn effective(self, host: &str) -> Self {
        match self {
            Self::Prefer if !is_loopback(host) => Self::Require,
            mode => mode,
        }
    }
}

/// Whether `host` is this machine: `localhost`, a loopback address, or a
/// Unix socket directory.
#[must_use]
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.starts_with('/')
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
    /// Whether the tree lists every database on the server, not only this
    /// one, where the driver can list them.
    #[serde(default)]
    pub show_all_databases: bool,
    /// Driver specific settings the generic form does not cover.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

impl ConnectionProfile {
    /// A blank profile for `driver`, listening on `port`. Port 0 is a
    /// file-based driver's: the host is then the database file's path.
    pub fn new(driver: DriverId, port: u16) -> Self {
        Self {
            id: ConnectionId::new(),
            name: String::new(),
            driver,
            host: if port == 0 {
                String::new()
            } else {
                "localhost".to_owned()
            },
            port,
            database: String::new(),
            username: String::new(),
            ssl: SslMode::default(),
            save_password: true,
            color: None,
            show_all_databases: false,
            options: BTreeMap::new(),
        }
    }

    /// Whether the profile opens a database file (a file-based driver's,
    /// port 0) rather than a server: the host is then the file's path.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.port == 0
    }

    /// The database file's name, for a file profile.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.host.rsplit(['/', '\\']).next().unwrap_or(&self.host)
    }

    /// Where the profile connects, as lists show it: `host:port`, or the
    /// file's path.
    #[must_use]
    pub fn address(&self) -> String {
        if self.is_file() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The name shown in lists: the profile's own, or `user@host/db` (the
    /// file's name for a file).
    #[must_use]
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            self.name.clone()
        } else if self.is_file() {
            self.file_name().to_owned()
        } else {
            format!("{}@{}/{}", self.username, self.host, self.database)
        }
    }

    /// Checks the fields a connection cannot do without.
    pub fn validate(&self) -> Result<(), String> {
        if self.is_file() {
            let path = std::path::Path::new(self.host.trim());
            if self.host.trim().is_empty() {
                return Err("Database file is required".to_owned());
            }
            if !path.is_absolute() {
                return Err("Database file must be a full path".to_owned());
            }
            return Ok(());
        }
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
    fn file_profiles_need_a_full_path_and_nothing_else() {
        let mut profile = ConnectionProfile::new(DriverId::new("sqlite"), 0);
        assert!(profile.is_file());
        assert_eq!(
            profile.validate(),
            Err("Database file is required".to_owned())
        );
        profile.host = "shop.db".to_owned();
        assert_eq!(
            profile.validate(),
            Err("Database file must be a full path".to_owned())
        );
        let absolute = std::env::temp_dir().join("shop.db");
        profile.host = absolute.to_string_lossy().into_owned();
        assert_eq!(profile.validate(), Ok(()));
        assert_eq!(profile.file_name(), "shop.db");
        assert_eq!(profile.display_name(), "shop.db");
        assert_eq!(profile.address(), profile.host);
    }

    #[test]
    fn validation_names_the_missing_field() {
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        assert_eq!(profile.validate(), Err("User is required".to_owned()));
        profile.username = "app".to_owned();
        assert_eq!(profile.validate(), Ok(()));
    }

    #[test]
    fn prefer_cannot_fall_back_to_plain_text_off_loopback() {
        for host in [
            "localhost",
            "127.0.0.1",
            "127.8.0.1",
            "::1",
            "[::1]",
            "/var/run/postgresql",
        ] {
            assert!(is_loopback(host), "{host}");
            assert_eq!(SslMode::Prefer.effective(host), SslMode::Prefer, "{host}");
        }
        for host in [
            "db.example.com",
            "10.0.0.5",
            "localhost.evil.com",
            "",
            "::2",
        ] {
            assert!(!is_loopback(host), "{host}");
            assert_eq!(SslMode::Prefer.effective(host), SslMode::Require, "{host}");
        }
        // An explicit choice is kept.
        assert_eq!(
            SslMode::Disable.effective("db.example.com"),
            SslMode::Disable
        );
        assert_eq!(SslMode::Require.effective("localhost"), SslMode::Require);
    }
}
