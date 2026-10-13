//! The catalogue of providers on offer: `providers.json`.
//!
//! ```json
//! {
//!   "providers": [{
//!     "id": "postgres",
//!     "version": "0.1.0",
//!     "api": "0.1.0",
//!     "description": "PostgreSQL 10 and later",
//!     "url": "postgres-0.1.0.wasm",
//!     "sha256": "9f86d08...",
//!     "size": 396112,
//!     "info": { "id": "postgres", "name": "PostgreSQL", ... }
//!   }]
//! }
//! ```
//!
//! `url` is relative to the index or an absolute `https://` URL. The
//! digest pins the exact file: a download that does not match it is
//! thrown away.

use serde::{Deserialize, Serialize};
use violet_core::{DbError, DriverInfo, ErrorKind, Result};

/// Largest provider Violet will download.
pub const MAX_PROVIDER_SIZE: u64 = 64 << 20;

/// Largest index Violet will read.
pub const MAX_INDEX_SIZE: u64 = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub providers: Vec<Entry>,
}

/// One provider version on offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub version: String,
    /// The provider contract it was built for.
    pub api: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    pub size: u64,
    /// What the provider says about itself, so the interface can offer it
    /// before it is installed.
    pub info: DriverInfo,
}

impl Index {
    /// Reads an index, dropping entries that are malformed or built for a
    /// provider contract this Violet does not speak.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let index: Self = serde_json::from_slice(bytes).map_err(|error| {
            DbError::new(
                ErrorKind::Provider,
                format!("The provider catalogue is malformed: {error}"),
            )
        })?;
        let providers = index
            .providers
            .into_iter()
            .filter(|entry| match entry.check() {
                Ok(()) => true,
                Err(reason) => {
                    log::warn!(
                        "skipping provider {:?} in the catalogue: {reason}",
                        entry.id
                    );
                    false
                }
            })
            .collect();
        Ok(Self { providers })
    }

    /// The entry for `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.providers.iter().find(|entry| entry.id == id)
    }

    /// Adds `entry`, replacing any other version of the same provider.
    pub fn upsert(&mut self, entry: Entry) {
        self.providers.retain(|other| other.id != entry.id);
        self.providers.push(entry);
        self.providers.sort_by(|a, b| a.id.cmp(&b.id));
    }
}

impl Entry {
    fn check(&self) -> std::result::Result<(), String> {
        if !violet_wasm::valid_id(&self.id) || self.info.id.as_str() != self.id {
            return Err("invalid id".into());
        }
        if !compatible(&self.api) {
            return Err(format!(
                "built for provider API {}, Violet speaks {}",
                self.api,
                violet_wasm::API_VERSION
            ));
        }
        if self.size == 0 || self.size > MAX_PROVIDER_SIZE {
            return Err("invalid size".into());
        }
        if !is_sha256(&self.sha256) {
            return Err("invalid digest".into());
        }
        if self.url.is_empty() || self.url.contains("..") {
            return Err("invalid url".into());
        }
        Ok(())
    }
}

/// Whether a provider built for API `api` runs here: same major version,
/// or the same minor while the major is 0.
#[must_use]
pub fn compatible(api: &str) -> bool {
    let parts = |version: &str| -> Option<(u64, u64)> {
        let mut parts = version.split('.');
        Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
    };
    match (parts(api), parts(violet_wasm::API_VERSION)) {
        (Some((0, theirs)), Some((0, ours))) => theirs == ours,
        (Some((theirs, _)), Some((ours, _))) => theirs == ours,
        _ => false,
    }
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use violet_core::{Capabilities, DriverId, SqlDialect};

    use super::*;

    pub(crate) fn entry(id: &str) -> Entry {
        Entry {
            id: id.into(),
            version: "0.1.0".into(),
            api: violet_wasm::API_VERSION.into(),
            description: String::new(),
            url: format!("{id}.wasm"),
            sha256: sha256(b"x"),
            size: 1,
            info: DriverInfo {
                id: DriverId::new(id),
                name: id.into(),
                default_port: 1,
                default_database: String::new(),
                default_user: String::new(),
                capabilities: Capabilities {
                    databases: false,
                    schemas: true,
                    cancel: false,
                    tls: false,
                },
                dialect: SqlDialect::ANSI,
            },
        }
    }

    #[test]
    fn digests_are_hex_sha256() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha256(&sha256(b"")));
        assert!(!is_sha256("BA7816BF"));
    }

    #[test]
    fn only_compatible_well_formed_entries_are_kept() {
        let mut index = Index::default();
        index.upsert(entry("postgres"));
        index.upsert(entry("mysql"));

        let mut old_api = entry("old");
        old_api.api = "0.0.9".into();
        let mut escaping = entry("sneaky");
        escaping.url = "../../etc/passwd".into();
        let mut huge = entry("huge");
        huge.size = MAX_PROVIDER_SIZE + 1;
        let mut renamed = entry("liar");
        renamed.info.id = DriverId::new("postgres");
        let mut bad_id = entry("x");
        bad_id.id = "../x".into();
        for bad in [old_api, escaping, huge, renamed, bad_id] {
            index.providers.push(bad);
        }

        let parsed = Index::parse(&serde_json::to_vec(&index).expect("json")).expect("parse");
        let ids: Vec<_> = parsed.providers.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["mysql", "postgres"]);
        assert!(Index::parse(b"{nope").is_err());
    }

    #[test]
    fn api_versions_follow_semver() {
        assert!(compatible(violet_wasm::API_VERSION));
        assert!(compatible("0.1.7"));
        assert!(!compatible("0.2.0"));
        assert!(!compatible("1.1.0"));
        assert!(!compatible("garbage"));
    }
}
