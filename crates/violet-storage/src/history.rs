//! The query history as JSON Lines: appending a query writes one line,
//! never the whole file.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use violet_core::Result;
use violet_core::ports::{HistoryEntry, QueryHistory};

use crate::json::{open_private, storage_error};

/// A history file holding at most `keep` entries, the newest. It may grow
/// past that while Violet runs; loading trims it.
#[derive(Debug)]
pub struct JsonLinesHistory {
    path: PathBuf,
    keep: usize,
}

impl JsonLinesHistory {
    pub fn new(path: impl Into<PathBuf>, keep: usize) -> Self {
        Self {
            path: path.into(),
            keep,
        }
    }

    /// Replaces the file with `entries`, as `JsonFile` does: a private
    /// sibling, flushed, then renamed over it.
    fn rewrite(&self, entries: &[HistoryEntry]) -> Result<()> {
        let mut bytes = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry)
                .map_err(|error| storage_error(format!("Cannot encode the history: {error}")))?;
            bytes.push(b'\n');
        }
        let temporary = self.path.with_extension("jsonl.tmp");
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

impl QueryHistory for JsonLinesHistory {
    fn load(&self) -> Result<Vec<HistoryEntry>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(storage_error(format!(
                    "Cannot read {}: {error}",
                    self.path.display()
                )));
            }
        };
        let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
        // A line cut short by a crash, or from a newer Violet, is skipped.
        let mut entries: Vec<HistoryEntry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if entries.len() > self.keep || entries.len() != lines {
            let excess = entries.len().saturating_sub(self.keep);
            entries.drain(..excess);
            self.rewrite(&entries)?;
        }
        Ok(entries)
    }

    fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let mut line = serde_json::to_vec(entry)
            .map_err(|error| storage_error(format!("Cannot encode the history: {error}")))?;
        line.push(b'\n');
        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory).map_err(|error| {
                storage_error(format!("Cannot create {}: {error}", directory.display()))
            })?;
        }
        let append = || -> std::io::Result<()> {
            let mut file = append_private(&self.path)?;
            // One write per line, so lines from a crash are whole or absent.
            file.write_all(&line)
        };
        append().map_err(|error| {
            storage_error(format!("Cannot write {}: {error}", self.path.display()))
        })
    }

    fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_error(format!(
                "Cannot delete {}: {error}",
                self.path.display()
            ))),
        }
    }
}

#[cfg(unix)]
fn append_private(path: &std::path::Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn append_private(path: &std::path::Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().append(true).create(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> HistoryEntry {
        HistoryEntry {
            at: n,
            connection: None,
            source: "local".into(),
            sql: format!("SELECT {n}"),
            elapsed_ms: 1,
            rows: Some(1),
            error: None,
        }
    }

    #[test]
    fn appends_reload_and_trim_to_the_newest() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("state").join("history.jsonl");
        let history = JsonLinesHistory::new(&path, 3);
        assert!(history.load().expect("empty").is_empty());
        for n in 1..=5 {
            history.append(&entry(n)).expect("append");
        }
        let loaded = history.load().expect("load");
        assert_eq!(loaded.iter().map(|e| e.at).collect::<Vec<_>>(), [3, 4, 5]);
        // Trimmed on disk too.
        assert_eq!(fs::read_to_string(&path).expect("read").lines().count(), 3);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        history.clear().expect("clear");
        assert!(history.load().expect("cleared").is_empty());
    }

    #[test]
    fn a_torn_line_is_dropped() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("history.jsonl");
        let history = JsonLinesHistory::new(&path, 10);
        history.append(&entry(1)).expect("append");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(b"{\"at\":2,\"sq").expect("write");
        assert_eq!(history.load().expect("load"), [entry(1)]);
        history.append(&entry(3)).expect("append");
        assert_eq!(history.load().expect("load").len(), 2);
    }
}
