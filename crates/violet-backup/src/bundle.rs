//! What a backup's archive holds:
//!
//! ```text
//! manifest.json      format, app version, date, source OS, counts
//! connections.json   the workspace's connections and folders
//! vault.json         saved passwords, by connection id
//! scripts/...        the scripts folder as is, versions included
//! ```
//!
//! Paths are relative and `/`-separated whatever the OS, and connections
//! keep their ids, so scripts (filed under their connection's id) still
//! belong to them on the other machine.

use serde::{Deserialize, Serialize};
use violet_core::ports::SavedProfiles;
use violet_core::{ConnectionId, ConnectionProfile, Secret};
use zeroize::Zeroizing;

use crate::archive::{self, Entry};
use crate::{Error, Result};

/// The layout of the archive's files.
pub const FORMAT: u32 = 1;

const MANIFEST: &str = "manifest.json";
const CONNECTIONS: &str = "connections.json";
const VAULT: &str = "vault.json";
const SCRIPTS: &str = "scripts/";

/// Facts about a backup, readable once it is open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// Violet's version that made it.
    pub app_version: String,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    /// `macos`, `linux`, `windows`.
    pub os: String,
    pub connections: usize,
    pub passwords: usize,
    pub files: usize,
}

#[derive(Serialize, Deserialize)]
struct Connections {
    connections: Vec<ConnectionProfile>,
    #[serde(default)]
    folders: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct VaultEntry {
    id: ConnectionId,
    password: Zeroizing<String>,
}

/// A file of the scripts folder.
pub struct BundleFile {
    /// Below the scripts folder, `/`-separated.
    pub path: String,
    pub data: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for BundleFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleFile")
            .field("path", &self.path)
            .field("len", &self.data.len())
            .finish()
    }
}

/// A workspace, as a backup carries it.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    pub profiles: SavedProfiles,
    pub passwords: Vec<(ConnectionId, Secret)>,
    pub files: Vec<BundleFile>,
}

impl Bundle {
    /// The archive of this bundle.
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        let json =
            |error: serde_json::Error| Error::Io(format!("Cannot encode the backup: {error}"));
        let manifest = serde_json::to_vec_pretty(&self.manifest).map_err(json)?;
        let connections = serde_json::to_vec_pretty(&Connections {
            connections: self.profiles.connections.clone(),
            folders: self.profiles.folders.clone(),
        })
        .map_err(json)?;
        // Sized for the worst case (every character escaped as `\uXXXX`)
        // so the buffer holding the passwords never moves.
        let capacity = 2 + self
            .passwords
            .iter()
            .map(|(_, secret)| 64 + 6 * secret.expose().len())
            .sum::<usize>();
        let mut vault = Zeroizing::new(Vec::with_capacity(capacity));
        let entries: Vec<VaultEntry> = self
            .passwords
            .iter()
            .map(|(id, secret)| VaultEntry {
                id: *id,
                password: Zeroizing::new(secret.expose().to_owned()),
            })
            .collect();
        serde_json::to_writer(&mut *vault, &entries).map_err(json)?;
        drop(entries);

        let scripts: Vec<String> = self
            .files
            .iter()
            .map(|file| format!("{SCRIPTS}{}", file.path))
            .collect();
        let mut all: Vec<(&str, &[u8])> = vec![
            (MANIFEST, &manifest),
            (CONNECTIONS, &connections),
            (VAULT, &vault),
        ];
        all.extend(
            scripts
                .iter()
                .zip(&self.files)
                .map(|(path, file)| (path.as_str(), file.data.as_slice())),
        );
        archive::build(all)
    }

    /// The bundle `archive` holds.
    pub fn decode(archive: &[u8]) -> Result<Self> {
        let mut manifest = None;
        let mut connections = None;
        let mut vault = None;
        let mut files = Vec::new();
        for Entry { path, data } in archive::extract(archive)? {
            match path.as_str() {
                MANIFEST => manifest = Some(data),
                CONNECTIONS => connections = Some(data),
                VAULT => vault = Some(data),
                _ => match path.strip_prefix(SCRIPTS) {
                    Some(rest) if archive::safe_path(rest) => files.push(BundleFile {
                        path: rest.to_owned(),
                        data,
                    }),
                    _ => return Err(Error::Malformed(format!("{path} is not part of a backup"))),
                },
            }
        }
        let missing = |name: &str| Error::Malformed(format!("{name} is missing"));
        let invalid = |name: &str, error: serde_json::Error| {
            Error::Malformed(format!("{name} is not valid: {error}"))
        };

        let manifest: Manifest =
            serde_json::from_slice(&manifest.ok_or_else(|| missing(MANIFEST))?)
                .map_err(|error| invalid(MANIFEST, error))?;
        if manifest.format > FORMAT {
            return Err(Error::NewerFormat);
        }
        let connections: Connections =
            serde_json::from_slice(&connections.ok_or_else(|| missing(CONNECTIONS))?)
                .map_err(|error| invalid(CONNECTIONS, error))?;
        let vault: Vec<VaultEntry> = serde_json::from_slice(&vault.ok_or_else(|| missing(VAULT))?)
            .map_err(|error| invalid(VAULT, error))?;
        // Only passwords of connections the backup holds.
        let passwords = vault
            .into_iter()
            .filter(|entry| connections.connections.iter().any(|c| c.id == entry.id))
            .map(|entry| (entry.id, Secret::new(entry.password.as_str())))
            .collect();
        Ok(Self {
            manifest,
            profiles: SavedProfiles {
                connections: connections.connections,
                folders: connections.folders,
            },
            passwords,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::DriverId;

    fn bundle() -> Bundle {
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.name = "Prod".to_owned();
        profile.folder = "Clients".to_owned();
        Bundle {
            manifest: Manifest {
                format: FORMAT,
                app_version: "0.1.9".to_owned(),
                created_at: 1,
                os: "linux".to_owned(),
                connections: 1,
                passwords: 1,
                files: 1,
            },
            passwords: vec![(profile.id, Secret::new("p\"w\u{1}"))],
            files: vec![BundleFile {
                path: format!("{}/app/report.sql", profile.id),
                data: Zeroizing::new(b"select 1".to_vec()),
            }],
            profiles: SavedProfiles {
                connections: vec![profile],
                folders: vec!["Clients".to_owned(), "Empty".to_owned()],
            },
        }
    }

    #[test]
    fn a_bundle_round_trips() {
        let original = bundle();
        let decoded = Bundle::decode(&original.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.manifest, original.manifest);
        assert_eq!(decoded.profiles, original.profiles);
        assert_eq!(decoded.passwords, original.passwords);
        assert_eq!(decoded.files.len(), 1);
        assert_eq!(decoded.files[0].path, original.files[0].path);
        assert_eq!(*decoded.files[0].data, *original.files[0].data);
    }

    #[test]
    fn unknown_files_and_later_formats_are_refused() {
        let stray = archive::build([(MANIFEST, &b"{}"[..]), ("elsewhere/x", b"")]).expect("build");
        assert!(matches!(Bundle::decode(&stray), Err(Error::Malformed(_))));

        let mut later = bundle();
        later.manifest.format = FORMAT + 1;
        assert_eq!(
            Bundle::decode(&later.encode().expect("encode")).map(|_| ()),
            Err(Error::NewerFormat)
        );
    }

    #[test]
    fn passwords_of_connections_not_in_the_backup_are_dropped() {
        let mut stray = bundle();
        stray
            .passwords
            .push((ConnectionId::new(), Secret::new("x")));
        let decoded = Bundle::decode(&stray.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.passwords.len(), 1);
    }
}
