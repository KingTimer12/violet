//! JSON files written so that a crash never leaves half a file behind.

use std::fs;
use std::io::Write as _;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use violet_core::ports::{ProfileRepository, SavedProfiles};
use violet_core::{ConnectionProfile, DbError, ErrorKind, Result};

/// A value stored as one JSON file.
#[derive(Debug)]
pub struct JsonFile<T> {
    path: PathBuf,
    _value: PhantomData<fn() -> T>,
}

impl<T: Serialize + DeserializeOwned> JsonFile<T> {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            _value: PhantomData,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored value, or `None` when the file does not exist yet.
    pub fn read(&self) -> Result<Option<T>> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                storage_error(format!("{} is not valid: {error}", self.path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(format!(
                "Cannot read {}: {error}",
                self.path.display()
            ))),
        }
    }

    /// Replaces the file: writes a sibling, flushes it, then renames it over
    /// the old one. On Unix the file is readable by its owner only.
    pub fn write(&self, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| storage_error(format!("Cannot encode settings: {error}")))?;
        let directory = self
            .path
            .parent()
            .ok_or_else(|| storage_error("The settings path has no directory"))?;
        fs::create_dir_all(directory).map_err(|error| {
            storage_error(format!("Cannot create {}: {error}", directory.display()))
        })?;

        let temporary = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = open_private(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        };
        write().map_err(|error| {
            let _ = fs::remove_file(&temporary);
            storage_error(format!("Cannot write {}: {error}", self.path.display()))
        })
    }
}

#[cfg(unix)]
pub(crate) fn open_private(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(crate) fn open_private(path: &Path) -> std::io::Result<fs::File> {
    fs::File::create(path)
}

pub(crate) fn storage_error(message: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Storage, message)
}

/// The profiles file's layout, versioned for later migrations.
#[derive(serde::Serialize, serde::Deserialize)]
struct ProfilesFile {
    version: u32,
    connections: Vec<ConnectionProfile>,
    #[serde(default)]
    folders: Vec<String>,
}

/// Saved connections in `connections.json`.
#[derive(Debug)]
pub struct JsonProfileRepository {
    file: JsonFile<ProfilesFile>,
}

impl std::fmt::Debug for ProfilesFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilesFile")
            .field("version", &self.version)
            .field("connections", &self.connections.len())
            .field("folders", &self.folders.len())
            .finish()
    }
}

impl JsonProfileRepository {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file: JsonFile::new(path),
        }
    }
}

impl ProfileRepository for JsonProfileRepository {
    fn load(&self) -> Result<SavedProfiles> {
        Ok(self
            .file
            .read()?
            .map(|file| SavedProfiles {
                connections: file.connections,
                folders: file.folders,
            })
            .unwrap_or_default())
    }

    fn save(&self, profiles: &SavedProfiles) -> Result<()> {
        self.file.write(&ProfilesFile {
            version: 1,
            connections: profiles.connections.clone(),
            folders: profiles.folders.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::DriverId;

    #[test]
    fn profiles_round_trip_and_hold_no_password() {
        let directory = tempfile::tempdir().expect("temp dir");
        let repository = JsonProfileRepository::new(directory.path().join("c/connections.json"));
        assert_eq!(repository.load().expect("empty"), SavedProfiles::default());

        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.name = "Local".to_owned();
        profile.username = "app".to_owned();
        profile.folder = "Prod/Clients".to_owned();
        let saved = SavedProfiles {
            connections: vec![profile],
            folders: vec!["Prod/Clients".to_owned(), "Empty".to_owned()],
        };
        repository.save(&saved).expect("save");

        assert_eq!(repository.load().expect("load"), saved);
        let text = fs::read_to_string(directory.path().join("c/connections.json")).expect("read");
        assert!(!text.contains("\"password\""));
    }

    #[cfg(unix)]
    #[test]
    fn the_profiles_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("connections.json");
        JsonProfileRepository::new(&path)
            .save(&SavedProfiles::default())
            .expect("save");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_file_from_before_folders_loads() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("connections.json");
        let id = violet_core::ConnectionId::new();
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"connections":[{{"id":"{}","name":"Old","driver":"postgres","host":"localhost","port":5432,"database":"app","username":"app"}}]}}"#,
                id.0
            ),
        )
        .expect("write");
        let saved = JsonProfileRepository::new(&path).load().expect("load");
        assert!(saved.folders.is_empty());
        assert_eq!(saved.connections.len(), 1);
        assert_eq!(saved.connections[0].folder, "");
        assert_eq!(saved.connections[0].name, "Old");
    }

    #[test]
    fn a_corrupt_file_is_reported_not_overwritten() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("connections.json");
        fs::write(&path, "{ not json").expect("write");
        let error = JsonProfileRepository::new(&path)
            .load()
            .expect_err("corrupt");
        assert_eq!(error.kind, ErrorKind::Storage);
        assert_eq!(fs::read_to_string(&path).expect("read"), "{ not json");
    }
}
