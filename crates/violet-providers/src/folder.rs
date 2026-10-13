//! The `providers` folder:
//!
//! ```text
//! providers/
//!   index.json              last catalogue fetched, for offline use
//!   postgres/
//!     provider.wasm
//!     manifest.json         version, digest, what it said about itself
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use violet_core::{DbError, DriverInfo, ErrorKind, Result};

use crate::index::sha256;

const COMPONENT: &str = "provider.wasm";
const MANIFEST: &str = "manifest.json";
const INDEX: &str = "index.json";

/// What was installed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub api: String,
    /// Of `provider.wasm`, checked every time it is loaded.
    pub sha256: String,
    pub info: DriverInfo,
}

fn error(message: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Provider, message)
}

fn io(path: &Path, cause: &std::io::Error) -> DbError {
    error(format!("{}: {cause}", path.display()))
}

#[derive(Clone, Debug)]
pub struct Folder {
    root: PathBuf,
}

impl Folder {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `id`'s directory. Ids are checked to be plain names first.
    fn dir(&self, id: &str) -> Result<PathBuf> {
        if !violet_wasm::valid_id(id) {
            return Err(error(format!("Invalid provider id {id:?}")));
        }
        Ok(self.root.join(id))
    }

    /// Every installed provider's manifest. A broken install is skipped
    /// with a warning, not fatal.
    pub fn manifests(&self) -> Result<Vec<Manifest>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&self.root, &e)),
        };
        let mut manifests = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().filter(|id| violet_wasm::valid_id(id)) else {
                continue;
            };
            match self.manifest(id) {
                Ok(Some(manifest)) => manifests.push(manifest),
                Ok(None) => {}
                Err(e) => log::warn!("provider {id}: {e}"),
            }
        }
        manifests.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        Ok(manifests)
    }

    pub fn manifest(&self, id: &str) -> Result<Option<Manifest>> {
        let path = self.dir(id)?.join(MANIFEST);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&path, &e)),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| error(format!("{}: {e}", path.display())))?;
        if manifest.info.id.as_str() != id {
            return Err(error(format!("{} names another provider", path.display())));
        }
        Ok(Some(manifest))
    }

    /// The installed component, checked against its manifest.
    pub fn component(&self, manifest: &Manifest) -> Result<Vec<u8>> {
        let path = self.dir(manifest.info.id.as_str())?.join(COMPONENT);
        let bytes = std::fs::read(&path).map_err(|e| io(&path, &e))?;
        if sha256(&bytes) != manifest.sha256 {
            return Err(error(format!(
                "{} changed since it was installed; reinstall the provider",
                path.display()
            )));
        }
        Ok(bytes)
    }

    /// Installs `component` as `manifest` says, replacing any previous
    /// version. A half-written install never replaces a working one: it is
    /// staged beside it and swapped in by renames.
    pub fn install(&self, manifest: &Manifest, component: &[u8]) -> Result<()> {
        let dir = self.dir(manifest.info.id.as_str())?;
        std::fs::create_dir_all(&self.root).map_err(|e| io(&self.root, &e))?;
        let tag = uuid::Uuid::new_v4().simple();
        let staging = self.root.join(format!(".staging-{tag}"));
        let old = self.root.join(format!(".old-{tag}"));

        let write = || -> Result<()> {
            std::fs::create_dir(&staging).map_err(|e| io(&staging, &e))?;
            write_file(&staging.join(COMPONENT), component)?;
            let json =
                serde_json::to_vec_pretty(manifest).map_err(|e| error(format!("manifest: {e}")))?;
            write_file(&staging.join(MANIFEST), &json)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        let had_previous = dir.exists();
        if had_previous {
            std::fs::rename(&dir, &old).map_err(|e| io(&dir, &e))?;
        }
        if let Err(e) = std::fs::rename(&staging, &dir) {
            if had_previous {
                let _ = std::fs::rename(&old, &dir);
            }
            let _ = std::fs::remove_dir_all(&staging);
            return Err(io(&dir, &e));
        }
        if had_previous {
            let _ = std::fs::remove_dir_all(&old);
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let dir = self.dir(id)?;
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(&dir, &e)),
            _ => Ok(()),
        }
    }

    /// The catalogue last fetched.
    #[must_use]
    pub fn cached_index(&self) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(INDEX)).ok()
    }

    pub fn cache_index(&self, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| io(&self.root, &e))?;
        write_file(&self.root.join(INDEX), bytes)
    }

    /// Leftovers of installs interrupted by a crash.
    pub fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".staging-") || name.starts_with(".old-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Writes through a temporary file renamed into place, so a reader never
/// sees half a file.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary).map_err(|e| io(&temporary, &e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io(&temporary, &e))?;
    std::fs::rename(&temporary, path).map_err(|e| io(path, &e))
}
