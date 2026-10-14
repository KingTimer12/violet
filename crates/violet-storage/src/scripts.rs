//! SQL scripts saved for a connection's database, as plain `.sql` files.
//!
//! A script lives at `<root>/<connection id>/<database>/<name>.sql`: the
//! folders say what it runs on, so nothing else has to be kept in step
//! with the files, and they can be copied, renamed or edited elsewhere.
//! Names are kept as typed; characters a file name cannot hold are
//! written `%XX`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;
use violet_core::{ConnectionId, Result};

use crate::json::{open_private, storage_error};

/// The extension scripts are saved with.
pub const EXTENSION: &str = "sql";

/// A script saved for a connection's database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedScript {
    pub path: PathBuf,
    /// The file name, without `.sql`.
    pub name: String,
    pub connection: ConnectionId,
    pub database: String,
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

    /// Where a script named `name` for `database` of `connection` goes.
    #[must_use]
    pub fn path_for(&self, connection: ConnectionId, database: &str, name: &str) -> PathBuf {
        self.root
            .join(connection.to_string())
            .join(encode(database))
            .join(format!("{}.{EXTENSION}", encode(name)))
    }

    /// The connection and database a file in the scripts folder belongs
    /// to; `None` for a file anywhere else.
    #[must_use]
    pub fn locate(&self, path: &Path) -> Option<SavedScript> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut parts = relative.iter();
        let connection = parts.next()?.to_str()?;
        let database = parts.next()?.to_str()?;
        let file = Path::new(parts.next()?);
        if parts.next().is_some() || file.extension()? != EXTENSION {
            return None;
        }
        Some(SavedScript {
            path: path.to_path_buf(),
            name: decode(file.file_stem()?.to_str()?),
            connection: ConnectionId(Uuid::parse_str(connection).ok()?),
            database: decode(database),
        })
    }

    /// Every saved script, by connection, database and name. A missing
    /// folder has none.
    pub fn list(&self) -> Result<Vec<SavedScript>> {
        let mut scripts = Vec::new();
        for connection in entries(&self.root)?
            .into_iter()
            .filter(|path| path.is_dir())
        {
            for database in entries(&connection)?
                .into_iter()
                .filter(|path| path.is_dir())
            {
                for file in entries(&database)? {
                    if let Some(script) = self.locate(&file) {
                        scripts.push(script);
                    }
                }
            }
        }
        scripts.sort_by(|a, b| {
            (a.connection, &a.database, a.name.to_lowercase()).cmp(&(
                b.connection,
                &b.database,
                b.name.to_lowercase(),
            ))
        });
        Ok(scripts)
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
        let path = store.path_for(connection, "app/prod", "Relatório: vendas");
        write(&path, "SELECT 1;")?;
        write(&store.path_for(connection, "postgres", "a"), "SELECT 2;")?;
        // Files that are not scripts are left out.
        write(&dir.path().join("notes.txt"), "")?;
        write(&dir.path().join("not-an-id").join("db").join("x.sql"), "")?;

        let scripts = store.list()?;
        assert_eq!(scripts.len(), 2);
        let script = scripts
            .iter()
            .find(|script| script.path == path)
            .ok_or_else(|| storage_error("missing"))?;
        assert_eq!(script.name, "Relatório: vendas");
        assert_eq!(script.database, "app/prod");
        assert_eq!(script.connection, connection);
        assert_eq!(read(&path)?, "SELECT 1;");
        assert_eq!(store.locate(Path::new("/elsewhere/x.sql")), None);
        Ok(())
    }

    #[test]
    fn a_missing_folder_has_no_scripts() -> Result<()> {
        let store = ScriptStore::new("/no/such/violet/scripts");
        assert!(store.list()?.is_empty());
        Ok(())
    }
}
