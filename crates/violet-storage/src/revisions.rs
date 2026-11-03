//! Earlier versions of saved scripts.
//!
//! Every save of a script of the scripts folder keeps a copy of what was
//! written, at `<root>/.history/<path of the script below root>/<id>.<ext>`.
//! The id is the save's time in milliseconds, so copies sort by age and
//! the folder is not a connection's, which keeps [`ScriptStore::list`]
//! from seeing it. Scripts saved elsewhere have no history.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use violet_core::Result;

use crate::json::{open_private, storage_error};
use crate::scripts::ScriptStore;

/// Folder of the scripts folder holding the copies.
const HISTORY: &str = ".history";

/// How many copies a script keeps; older ones are dropped.
pub const KEPT: usize = 50;

/// One earlier version of a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// Milliseconds since the Unix epoch, as saved.
    pub id: u64,
    pub saved_at: SystemTime,
}

/// What the user and the app keep with a version: a comment, and how the
/// script performed when it was saved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RevisionNote {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performance: Option<Performance>,
}

impl RevisionNote {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.comment.is_empty() && self.performance.is_none()
    }
}

/// The last run of a script and the last plan read, as they stood when a
/// version was saved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Performance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// What ran, cut short.
    pub statement: String,
    pub elapsed_ms: f64,
    /// Rows of the first result; none for an explain or a command.
    pub rows: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub statement: String,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    /// The costliest steps, one line each.
    pub heaviest: Vec<String>,
    pub warnings: Vec<String>,
}

impl ScriptStore {
    /// Where the copies of the script at `path` are; `None` for a file
    /// outside the scripts folder.
    fn revisions_dir(&self, path: &Path) -> Option<PathBuf> {
        let relative = path.strip_prefix(self.root()).ok()?;
        (relative.components().count() > 1).then(|| self.root().join(HISTORY).join(relative))
    }

    /// Keeps `text` as the newest version of the script at `path`, with
    /// `note`. When the newest already holds `text` no version is added:
    /// `note` goes to that one, its comment replaced if `note` has one.
    /// Does nothing for a script outside the scripts folder.
    pub fn record_revision(&self, path: &Path, text: &str, note: &RevisionNote) -> Result<()> {
        let Some(directory) = self.revisions_dir(path) else {
            return Ok(());
        };
        let known = self.revisions(path)?;
        if let Some(newest) = known.first()
            && self.read_revision(path, newest.id)? == text
        {
            if !note.is_empty() {
                let mut merged = self.revision_note(path, newest.id);
                if !note.comment.is_empty() {
                    merged.comment.clone_from(&note.comment);
                }
                if note.performance.is_some() {
                    merged.performance.clone_from(&note.performance);
                }
                self.set_revision_note(path, newest.id, &merged)?;
            }
            return Ok(());
        }
        fs::create_dir_all(&directory).map_err(|error| {
            storage_error(format!("Cannot create {}: {error}", directory.display()))
        })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let id = known.first().map_or(now, |newest| now.max(newest.id + 1));
        let file = revision_file(&directory, path, id);
        let mut out = open_private(&file)
            .map_err(|error| storage_error(format!("Cannot write {}: {error}", file.display())))?;
        out.write_all(text.as_bytes())
            .and_then(|()| out.sync_all())
            .map_err(|error| storage_error(format!("Cannot write {}: {error}", file.display())))?;
        self.set_revision_note(path, id, note)?;
        for stale in known.iter().skip(KEPT - 1) {
            let _ = fs::remove_file(revision_file(&directory, path, stale.id));
            let _ = fs::remove_file(note_file(&directory, stale.id));
        }
        Ok(())
    }

    /// What is kept with a version; nothing when it has no note or the
    /// note cannot be read.
    #[must_use]
    pub fn revision_note(&self, path: &Path, id: u64) -> RevisionNote {
        let Some(directory) = self.revisions_dir(path) else {
            return RevisionNote::default();
        };
        fs::read(note_file(&directory, id))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Replaces what is kept with a version. An empty note removes the
    /// file.
    pub fn set_revision_note(&self, path: &Path, id: u64, note: &RevisionNote) -> Result<()> {
        let Some(directory) = self.revisions_dir(path) else {
            return Ok(());
        };
        let file = note_file(&directory, id);
        if note.is_empty() {
            let _ = fs::remove_file(&file);
            return Ok(());
        }
        let json = serde_json::to_vec_pretty(note)
            .map_err(|error| storage_error(format!("Cannot write {}: {error}", file.display())))?;
        let mut out = open_private(&file)
            .map_err(|error| storage_error(format!("Cannot write {}: {error}", file.display())))?;
        out.write_all(&json)
            .and_then(|()| out.sync_all())
            .map_err(|error| storage_error(format!("Cannot write {}: {error}", file.display())))
    }

    /// The saved versions of the script at `path`, newest first.
    pub fn revisions(&self, path: &Path) -> Result<Vec<Revision>> {
        let Some(directory) = self.revisions_dir(path) else {
            return Ok(Vec::new());
        };
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(storage_error(format!(
                    "Cannot read {}: {error}",
                    directory.display()
                )));
            }
        };
        let mut revisions: Vec<Revision> = entries
            .filter_map(|entry| {
                let name = entry.ok()?.path();
                if name.extension().is_some_and(|extension| extension == "json") {
                    return None;
                }
                let id: u64 = name.file_stem()?.to_str()?.parse().ok()?;
                Some(Revision {
                    id,
                    saved_at: UNIX_EPOCH + Duration::from_millis(id),
                })
            })
            .collect();
        revisions.sort_by_key(|revision| std::cmp::Reverse(revision.id));
        Ok(revisions)
    }

    /// The text of a saved version of the script at `path`.
    pub fn read_revision(&self, path: &Path, id: u64) -> Result<String> {
        let directory = self
            .revisions_dir(path)
            .ok_or_else(|| storage_error(format!("{} has no history", path.display())))?;
        let file = revision_file(&directory, path, id);
        fs::read_to_string(&file)
            .map_err(|error| storage_error(format!("Cannot read {}: {error}", file.display())))
    }

    /// Moves the history of the script or folder at `from` to where `to`
    /// is, after it was moved. Best effort: a move that fails loses the
    /// history, not the script.
    pub fn move_revisions(&self, from: &Path, to: &Path) {
        let (Some(from), Some(to)) = (self.revisions_dir(from), self.revisions_dir(to)) else {
            return;
        };
        if !from.exists() || to.exists() {
            return;
        }
        if let Some(parent) = to.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            log::warn!("Cannot move history to {}: {error}", to.display());
            return;
        }
        if let Err(error) = fs::rename(&from, &to) {
            log::warn!("Cannot move history to {}: {error}", to.display());
        }
    }
}

fn note_file(directory: &Path, id: u64) -> PathBuf {
    directory.join(format!("{id}.json"))
}

fn revision_file(directory: &Path, script: &Path, id: u64) -> PathBuf {
    let extension = script
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or(crate::scripts::EXTENSION);
    directory.join(format!("{id}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripts::{EXTENSION, write};
    use violet_core::ConnectionId;

    fn store() -> Result<(tempfile::TempDir, ScriptStore)> {
        let dir = tempfile::tempdir().map_err(|error| storage_error(error.to_string()))?;
        let store = ScriptStore::new(dir.path());
        Ok((dir, store))
    }

    #[test]
    fn each_different_save_is_kept_newest_first() -> Result<()> {
        let (_dir, store) = store()?;
        let path = store.path_for(ConnectionId::new(), "app", "q", EXTENSION);
        for text in ["SELECT 1;", "SELECT 1;", "SELECT 2;"] {
            write(&path, text)?;
            store.record_revision(&path, text, &RevisionNote::default())?;
        }
        let revisions = store.revisions(&path)?;
        assert_eq!(revisions.len(), 2);
        assert_eq!(store.read_revision(&path, revisions[0].id)?, "SELECT 2;");
        assert_eq!(store.read_revision(&path, revisions[1].id)?, "SELECT 1;");
        Ok(())
    }

    #[test]
    fn only_the_newest_are_kept() -> Result<()> {
        let (_dir, store) = store()?;
        let path = store.path_for(ConnectionId::new(), "app", "q", EXTENSION);
        for n in 0..KEPT + 5 {
            store.record_revision(&path, &format!("SELECT {n};"), &RevisionNote::default())?;
        }
        let revisions = store.revisions(&path)?;
        assert_eq!(revisions.len(), KEPT);
        assert_eq!(
            store.read_revision(&path, revisions[0].id)?,
            format!("SELECT {};", KEPT + 4)
        );
        Ok(())
    }

    #[test]
    fn history_follows_a_moved_script_and_stays_out_of_the_listing() -> Result<()> {
        let (_dir, store) = store()?;
        let connection = ConnectionId::new();
        let from = store.path_for(connection, "app", "q", EXTENSION);
        let to = store.path_in(connection, "app", "reports", "q", EXTENSION);
        write(&from, "SELECT 1;")?;
        store.record_revision(&from, "SELECT 1;", &RevisionNote::default())?;
        assert_eq!(store.list()?.0.len(), 1);

        store.move_revisions(&from, &to);
        assert!(store.revisions(&from)?.is_empty());
        assert_eq!(store.revisions(&to)?.len(), 1);
        Ok(())
    }

    #[test]
    fn a_note_stays_with_its_version_and_follows_the_move() -> Result<()> {
        let (_dir, store) = store()?;
        let connection = ConnectionId::new();
        let from = store.path_for(connection, "app", "q", EXTENSION);
        let to = store.path_in(connection, "app", "reports", "q", EXTENSION);
        let note = RevisionNote {
            comment: "added the index".to_owned(),
            performance: Some(Performance {
                run: Some(RunSummary {
                    statement: "SELECT 1".to_owned(),
                    elapsed_ms: 12.5,
                    rows: Some(3),
                }),
                plan: None,
            }),
        };
        store.record_revision(&from, "SELECT 1;", &note)?;
        store.record_revision(&from, "SELECT 2;", &RevisionNote::default())?;
        let revisions = store.revisions(&from)?;
        assert_eq!(revisions.len(), 2);
        assert_eq!(store.revision_note(&from, revisions[1].id), note);
        assert!(store.revision_note(&from, revisions[0].id).is_empty());

        store.move_revisions(&from, &to);
        let moved = store.revisions(&to)?;
        assert_eq!(store.revision_note(&to, moved[1].id), note);
        Ok(())
    }

    #[test]
    fn saving_the_same_text_again_adds_the_comment_to_the_newest() -> Result<()> {
        let (_dir, store) = store()?;
        let path = store.path_for(ConnectionId::new(), "app", "q", EXTENSION);
        store.record_revision(&path, "SELECT 1;", &RevisionNote::default())?;
        let commented = RevisionNote {
            comment: "baseline".to_owned(),
            performance: None,
        };
        store.record_revision(&path, "SELECT 1;", &commented)?;
        let revisions = store.revisions(&path)?;
        assert_eq!(revisions.len(), 1);
        assert_eq!(store.revision_note(&path, revisions[0].id).comment, "baseline");

        store.set_revision_note(&path, revisions[0].id, &RevisionNote::default())?;
        assert!(store.revision_note(&path, revisions[0].id).is_empty());
        assert_eq!(store.revisions(&path)?.len(), 1);
        Ok(())
    }

    #[test]
    fn a_script_outside_the_folder_has_no_history() -> Result<()> {
        let (_dir, store) = store()?;
        let elsewhere = Path::new("/elsewhere/x.sql");
        store.record_revision(elsewhere, "SELECT 1;", &RevisionNote::default())?;
        assert!(store.revisions(elsewhere)?.is_empty());
        Ok(())
    }
}
