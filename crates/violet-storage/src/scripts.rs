//! Scripts saved for a connection's database, as plain `.sql` files (or
//! `.redis` ones, for Redis connections).
//!
//! A script lives at `<root>/<connection id>/<database>/<name>.sql`, or in
//! folders below the database's: the folders say what it runs on, so
//! nothing else has to be kept in step with the files, and they can be
//! copied, renamed or edited elsewhere.
//! Names are kept as typed; characters a file name cannot hold are
//! written `%XX`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;
use violet_core::{ConnectionId, Result};

use crate::json::{open_private, storage_error};

/// The extension SQL scripts are saved with.
pub const EXTENSION: &str = "sql";

/// Every extension a saved script may have: SQL, and Redis commands.
pub const EXTENSIONS: &[&str] = &[EXTENSION, "redis"];

/// A script saved for a connection's database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedScript {
    pub path: PathBuf,
    /// The file name, without `.sql`.
    pub name: String,
    pub connection: ConnectionId,
    pub database: String,
    /// The folders below the database's it is in, as `a/b`; empty for
    /// none.
    pub folder: String,
}

/// A folder of scripts below a database's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFolder {
    pub connection: ConnectionId,
    pub database: String,
    /// As `a/b`.
    pub folder: String,
}

/// The scripts folder.
#[derive(Clone, Debug)]
pub struct ScriptStore {
    root: PathBuf,
}

impl ScriptStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a script named `name` for `database` of `connection` goes,
    /// with `extension` (one of [`EXTENSIONS`]).
    #[must_use]
    pub fn path_for(
        &self,
        connection: ConnectionId,
        database: &str,
        name: &str,
        extension: &str,
    ) -> PathBuf {
        self.path_in(connection, database, "", name, extension)
    }

    /// Where a script named `name` goes in `folder` (as `a/b`, empty for
    /// none) of `database` of `connection`.
    #[must_use]
    pub fn path_in(
        &self,
        connection: ConnectionId,
        database: &str,
        folder: &str,
        name: &str,
        extension: &str,
    ) -> PathBuf {
        self.folder_path(connection, database, folder)
            .join(format!("{}.{extension}", encode(name)))
    }

    /// The directory of `folder` (as `a/b`, empty for the database's own).
    #[must_use]
    pub fn folder_path(&self, connection: ConnectionId, database: &str, folder: &str) -> PathBuf {
        let mut path = self
            .root
            .join(connection.to_string())
            .join(encode(database));
        for part in folder.split('/').filter(|part| !part.is_empty()) {
            path.push(encode(part));
        }
        path
    }

    /// Makes a folder of scripts.
    pub fn create_folder(
        &self,
        connection: ConnectionId,
        database: &str,
        folder: &str,
    ) -> Result<()> {
        let path = self.folder_path(connection, database, folder);
        fs::create_dir_all(&path)
            .map_err(|error| storage_error(format!("Cannot create {}: {error}", path.display())))
    }

    /// Moves `folder` of a database, with what it holds, to `to`; refuses
    /// when `to` is taken. Returns the directories it moved from and to.
    pub fn move_folder(
        &self,
        connection: ConnectionId,
        database: &str,
        folder: &str,
        to: &str,
    ) -> Result<(PathBuf, PathBuf)> {
        let from = self.folder_path(connection, database, folder);
        let target = self.folder_path(connection, database, to);
        if target.exists() {
            return Err(storage_error(format!(
                "{} already exists",
                target.display()
            )));
        }
        rename(&from, &target)?;
        self.move_revisions(&from, &target);
        Ok((from, target))
    }

    /// The connection and database a file in the scripts folder belongs
    /// to; `None` for a file anywhere else.
    #[must_use]
    pub fn locate(&self, path: &Path) -> Option<SavedScript> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut parts = relative.iter();
        let connection = parts.next()?.to_str()?;
        let database = parts.next()?.to_str()?;
        let mut folders: Vec<String> = parts
            .map(|part| part.to_str().map(decode))
            .collect::<Option<_>>()?;
        folders.pop()?;
        let file = Path::new(path.file_name()?);
        if !EXTENSIONS
            .iter()
            .any(|e| file.extension() == Some(e.as_ref()))
        {
            return None;
        }
        Some(SavedScript {
            path: path.to_path_buf(),
            name: decode(file.file_stem()?.to_str()?),
            connection: ConnectionId(Uuid::parse_str(connection).ok()?),
            database: decode(database),
            folder: folders.join("/"),
        })
    }

    /// Every saved script, by connection, database, folder and name, and
    /// every folder below a database's, empty ones too. A missing folder
    /// has none.
    pub fn list(&self) -> Result<(Vec<SavedScript>, Vec<ScriptFolder>)> {
        let mut scripts = Vec::new();
        let mut folders = Vec::new();
        for connection_dir in entries(&self.root)?
            .into_iter()
            .filter(|path| path.is_dir())
        {
            let Some(connection) = connection_dir
                .file_name()
                .and_then(|name| Uuid::parse_str(name.to_str()?).ok())
                .map(ConnectionId)
            else {
                continue;
            };
            for database_dir in entries(&connection_dir)?
                .into_iter()
                .filter(|path| path.is_dir())
            {
                let Some(database) = database_dir.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let database = decode(database);
                let mut pending = vec![(database_dir.clone(), String::new())];
                while let Some((directory, folder)) = pending.pop() {
                    for path in entries(&directory)? {
                        if !path.is_dir() {
                            scripts.extend(self.locate(&path));
                            continue;
                        }
                        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                            continue;
                        };
                        let below = if folder.is_empty() {
                            decode(name)
                        } else {
                            format!("{folder}/{}", decode(name))
                        };
                        folders.push(ScriptFolder {
                            connection,
                            database: database.clone(),
                            folder: below.clone(),
                        });
                        pending.push((path, below));
                    }
                }
            }
        }
        scripts.sort_by(|a, b| {
            (a.connection, &a.database, &a.folder, a.name.to_lowercase()).cmp(&(
                b.connection,
                &b.database,
                &b.folder,
                b.name.to_lowercase(),
            ))
        });
        folders.sort_by(|a, b| {
            (a.connection, &a.database, a.folder.to_lowercase()).cmp(&(
                b.connection,
                &b.database,
                b.folder.to_lowercase(),
            ))
        });
        Ok((scripts, folders))
    }
}

/// Reads a script.
pub fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|error| storage_error(format!("Cannot read {}: {error}", path.display())))
}

/// Writes a script whole: a sibling first, then renamed over the old one,
/// so a crash never leaves half a script. Scripts may hold passwords as
/// typed, so on Unix they are readable by their owner only.
pub fn write(path: &Path, sql: &str) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| storage_error(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(directory).map_err(|error| {
        storage_error(format!("Cannot create {}: {error}", directory.display()))
    })?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let write = || -> std::io::Result<()> {
        let mut file = open_private(&temporary)?;
        file.write_all(sql.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    };
    write().map_err(|error| {
        let _ = fs::remove_file(&temporary);
        storage_error(format!("Cannot write {}: {error}", path.display()))
    })
}

/// Moves a script, creating the folder it goes to.
pub fn rename(from: &Path, to: &Path) -> Result<()> {
    if let Some(directory) = to.parent() {
        fs::create_dir_all(directory).map_err(|error| {
            storage_error(format!("Cannot create {}: {error}", directory.display()))
        })?;
    }
    fs::rename(from, to)
        .map_err(|error| storage_error(format!("Cannot move {}: {error}", from.display())))
}

/// The entries of `directory`; none when it does not exist.
fn entries(directory: &Path) -> Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => Ok(entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(storage_error(format!(
            "Cannot read {}: {error}",
            directory.display()
        ))),
    }
}

/// `name` as a file name: `%`, separators, characters Windows refuses,
/// control characters and a leading dot are written `%XX`.
fn encode(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for (at, c) in name.char_indices() {
        let escape = matches!(
            c,
            '%' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || c.is_control()
            || (at == 0 && c == '.');
        if escape {
            let mut bytes = [0; 4];
            for byte in c.encode_utf8(&mut bytes).bytes() {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        } else {
            encoded.push(c);
        }
    }
    encoded
}

/// Undoes [`encode`]; a `%` not followed by two hex digits stays as is.
fn decode(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                at += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_survive_the_trip_through_a_file_name() {
        for name in [
            "vendas 2026",
            "a/b\\c:d",
            "100%",
            ".hidden",
            "relatório?",
            "x%2Fy",
        ] {
            let encoded = encode(name);
            assert!(!encoded.contains(['/', '\\', ':', '?']), "{encoded}");
            assert!(!encoded.starts_with('.'), "{encoded}");
            assert_eq!(decode(&encoded), name);
        }
    }

    #[test]
    fn scripts_are_found_where_they_were_saved() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|error| storage_error(error.to_string()))?;
        let store = ScriptStore::new(dir.path());
        let connection = ConnectionId::new();
        let path = store.path_for(connection, "app/prod", "Relatório: vendas", EXTENSION);
        write(&path, "SELECT 1;")?;
        write(
            &store.path_for(connection, "postgres", "a", EXTENSION),
            "SELECT 2;",
        )?;
        write(
            &store.path_for(connection, "db0", "keys", "redis"),
            "KEYS *",
        )?;
        // Files that are not scripts are left out.
        write(&dir.path().join("notes.txt"), "")?;
        write(&dir.path().join("not-an-id").join("db").join("x.sql"), "")?;

        let nested = store.path_in(connection, "postgres", "reports/2026", "q1", EXTENSION);
        write(&nested, "SELECT 3;")?;
        store.create_folder(connection, "postgres", "empty")?;

        let (scripts, folders) = store.list()?;
        assert_eq!(scripts.len(), 4);
        let found = scripts
            .iter()
            .find(|script| script.path == nested)
            .ok_or_else(|| storage_error("missing nested"))?;
        assert_eq!(
            (
                found.connection,
                found.database.as_str(),
                found.folder.as_str()
            ),
            (connection, "postgres", "reports/2026")
        );
        assert_eq!(found.name, "q1");
        let folders: Vec<&str> = folders.iter().map(|f| f.folder.as_str()).collect();
        assert_eq!(folders, ["empty", "reports", "reports/2026"]);
        let script = scripts
            .iter()
            .find(|script| script.path == path)
            .ok_or_else(|| storage_error("missing"))?;
        assert_eq!(script.name, "Relatório: vendas");
        assert_eq!(script.database, "app/prod");
        assert_eq!(script.connection, connection);
        assert_eq!(script.folder, "");
        assert_eq!(read(&path)?, "SELECT 1;");
        assert_eq!(store.locate(Path::new("/elsewhere/x.sql")), None);
        Ok(())
    }

    #[test]
    fn a_folder_moves_with_its_scripts() -> Result<()> {
        let dir = tempfile::tempdir().map_err(|error| storage_error(error.to_string()))?;
        let store = ScriptStore::new(dir.path());
        let connection = ConnectionId::new();
        write(
            &store.path_in(connection, "postgres", "a/b", "q", EXTENSION),
            "SELECT 1;",
        )?;
        store.create_folder(connection, "postgres", "c")?;
        store.create_folder(connection, "postgres", "taken/b")?;

        assert!(
            store
                .move_folder(connection, "postgres", "a/b", "taken/b")
                .is_err()
        );
        let (from, to) = store.move_folder(connection, "postgres", "a/b", "c/d/b")?;
        assert!(!from.exists());
        assert_eq!(to, store.folder_path(connection, "postgres", "c/d/b"));

        let (scripts, folders) = store.list()?;
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].folder, "c/d/b");
        assert_eq!(read(&scripts[0].path)?, "SELECT 1;");
        let folders: Vec<&str> = folders.iter().map(|f| f.folder.as_str()).collect();
        assert_eq!(folders, ["a", "c", "c/d", "c/d/b", "taken", "taken/b"]);
        Ok(())
    }

    #[test]
    fn a_missing_folder_has_no_scripts() -> Result<()> {
        let store = ScriptStore::new("/no/such/violet/scripts");
        assert!(store.list()?.0.is_empty());
        Ok(())
    }
}
