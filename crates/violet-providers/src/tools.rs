//! Client programs downloaded with a provider, such as PostgreSQL's
//! `pg_dump`, kept beside the providers:
//!
//! ```text
//! providers/
//!   .tools/
//!     postgres/
//!       current.json        which download is in, and where its programs are
//!       a257bcdb8aa3301a/   the archive's digest: what was kept of it
//! ```
//!
//! The archive is checked against the digest the provider declared, and
//! only the paths it asked for are unpacked, inside the folder: no
//! absolute paths, no `..`, no links out, no special files.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use violet_core::{DbError, Download, ErrorKind, Result};

/// The most an archive may unpack to.
const MAX_UNPACKED: u64 = 1 << 30;

const CURRENT: &str = "current.json";

/// What `current.json` records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub sha256: String,
    pub label: String,
    /// The folder with the programs, under the provider's tools folder.
    pub bin: String,
}

fn error(message: impl Into<String>) -> DbError {
    DbError::new(ErrorKind::Provider, message)
}

/// The download meant for this machine, if any.
#[must_use]
pub fn for_this_platform(downloads: &[Download]) -> Option<&Download> {
    let platform = Download::this_platform()?;
    downloads.iter().find(|d| d.platform == platform)
}

/// A provider's tools folder.
#[derive(Clone, Debug)]
pub struct ToolsFolder {
    dir: PathBuf,
}

impl ToolsFolder {
    /// `root` is the providers folder; `id` was checked to be a plain name.
    #[must_use]
    pub fn new(root: &Path, id: &str) -> Self {
        Self {
            dir: root.join(".tools").join(id),
        }
    }

    /// What is installed, if its folder is still there.
    #[must_use]
    pub fn installed(&self) -> Option<Installed> {
        let bytes = std::fs::read(self.dir.join(CURRENT)).ok()?;
        let installed: Installed = serde_json::from_slice(&bytes).ok()?;
        self.bin(&installed).is_dir().then_some(installed)
    }

    /// Where an installed download's programs are.
    #[must_use]
    pub fn bin(&self, installed: &Installed) -> PathBuf {
        let mut path = self.dir.clone();
        for part in installed.bin.split('/') {
            path.push(part);
        }
        path
    }

    /// Unpacks a verified archive and makes it the current one, removing
    /// what was there before.
    pub fn install(&self, download: &Download, archive: &[u8]) -> Result<PathBuf> {
        download.check()?;
        let name = &download.sha256[..16];
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, &e))?;
        let staging = self
            .dir
            .join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
        let final_dir = self.dir.join(name);
        let result = unpack(archive, download, &staging).and_then(|()| {
            if final_dir.exists() {
                std::fs::remove_dir_all(&final_dir).map_err(|e| io(&final_dir, &e))?;
            }
            std::fs::rename(&staging, &final_dir).map_err(|e| io(&final_dir, &e))
        });
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        let installed = Installed {
            sha256: download.sha256.clone(),
            label: download.label.clone(),
            bin: format!("{name}/{}", download.bin),
        };
        let json = serde_json::to_vec_pretty(&installed).map_err(|e| error(e.to_string()))?;
        let current = self.dir.join(CURRENT);
        let temporary = self.dir.join("current.tmp");
        std::fs::write(&temporary, json).map_err(|e| io(&temporary, &e))?;
        std::fs::rename(&temporary, &current).map_err(|e| io(&current, &e))?;
        // Older downloads go.
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name().to_string_lossy() != name {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
        Ok(self.bin(&installed))
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn io(path: &Path, cause: &std::io::Error) -> DbError {
    error(format!("{}: {cause}", path.display()))
}

/// The plain components of a relative path, or `None` for anything else.
fn plain(path: &Path) -> Option<Vec<String>> {
    path.components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Whether `relative` (under the archive's root) is one of the paths kept.
fn wanted(relative: &str, extract: &[String]) -> bool {
    extract.iter().any(|keep| match keep.strip_suffix('/') {
        Some(folder) => relative == folder || relative.starts_with(keep.as_str()),
        None => {
            relative == keep
                || relative
                    .strip_suffix(".exe")
                    .is_some_and(|stem| stem == keep)
        }
    })
}

fn unpack(archive: &[u8], download: &Download, dest: &Path) -> Result<()> {
    // The address says how the tar is compressed: xz, or gzip otherwise.
    let decoder: Box<dyn std::io::Read + '_> = if download.url.ends_with(".tar.xz") {
        Box::new(lzma_rust2::XzReader::new(archive, true))
    } else {
        Box::new(flate2::read::GzDecoder::new(archive))
    };
    let mut tar = tar::Archive::new(decoder);
    std::fs::create_dir_all(dest).map_err(|e| io(dest, &e))?;
    let mut total = 0u64;
    let mut kept = 0usize;
    let entries = tar
        .entries()
        .map_err(|e| error(format!("Not a tar archive: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| error(format!("Broken archive: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| error(format!("Broken archive: {e}")))?
            .into_owned();
        let Some(parts) = plain(&path) else {
            return Err(error(format!("The archive holds {}", path.display())));
        };
        let Some((root, rest)) = parts.split_first() else {
            continue;
        };
        if *root != download.root || rest.is_empty() {
            continue;
        }
        let relative = rest.join("/");
        if !wanted(&relative, &download.extract) {
            continue;
        }
        let target = rest
            .iter()
            .fold(dest.to_path_buf(), |path, part| path.join(part));
        let kind = entry.header().entry_type();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| io(&target, &e))?;
        } else if kind.is_file() {
            total = total.saturating_add(entry.size());
            if total > MAX_UNPACKED {
                return Err(error("The archive unpacks to too much"));
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|e| error(format!("Broken archive: {e}")))?;
            std::fs::write(&target, &bytes).map_err(|e| io(&target, &e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.header().mode().unwrap_or(0o644);
                // Executable or not; never writable by others, never setuid.
                let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| io(&target, &e))?;
            }
            kept += 1;
        } else if kind.is_symlink() {
            let link = entry
                .link_name()
                .map_err(|e| error(format!("Broken archive: {e}")))?
                .ok_or_else(|| error("A link without a target"))?
                .into_owned();
            // Only links to a file beside them, as libraries have.
            let beside = plain(&link).is_some_and(|parts| parts.len() == 1);
            if !beside {
                return Err(error(format!(
                    "The archive links {} to {}",
                    path.display(),
                    link.display()
                )));
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target).map_err(|e| io(&target, &e))?;
        }
        // Anything else (devices, fifos, hard links) is left out.
    }
    if kept == 0 {
        return Err(error(format!(
            "Nothing to keep under {} in the archive",
            download.root
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, &[u8], Option<&str>)]) -> Vec<u8> {
        let tar = tarball(entries);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).expect("gz");
        gz.finish().expect("gz")
    }

    fn tarball(entries: &[(&str, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, body, link) in entries {
            let mut header = tar::Header::new_gnu();
            match link {
                Some(target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    header.set_mode(0o777);
                    builder
                        .append_link(&mut header, path, target)
                        .expect("link");
                }
                None => {
                    header.set_size(body.len() as u64);
                    header.set_mode(0o4755);
                    header.set_cksum();
                    builder.append_data(&mut header, path, *body).expect("file");
                }
            }
        }
        builder.into_inner().expect("tar")
    }

    fn download() -> Download {
        Download {
            platform: "linux-x86_64".into(),
            url: "https://example.com/pg.tar.gz".into(),
            sha256: "ab".repeat(32),
            size: 1,
            label: "Client".into(),
            root: "pg".into(),
            extract: vec!["bin/pg_dump".into(), "lib/".into()],
            bin: "bin".into(),
        }
    }

    #[test]
    fn only_asked_paths_are_kept_and_made_safe() {
        let dir = std::env::temp_dir().join(format!("violet-tools-{}", uuid::Uuid::new_v4()));
        let folder = ToolsFolder::new(&dir, "postgres");
        let bytes = archive(&[
            ("pg/bin/pg_dump", b"#!dump", None),
            ("pg/bin/postgres", b"server", None),
            ("pg/lib/libpq.5.dylib", b"lib", None),
            ("pg/lib/libpq.dylib", b"", Some("libpq.5.dylib")),
            ("pg/share/x", b"x", None),
        ]);
        let bin = folder.install(&download(), &bytes).expect("installed");
        assert!(bin.join("pg_dump").is_file());
        assert!(!bin.join("postgres").exists());
        assert!(
            bin.parent()
                .expect("parent")
                .join("lib/libpq.dylib")
                .exists()
        );
        assert!(!bin.parent().expect("parent").join("share").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(bin.join("pg_dump"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o7777, 0o755, "no setuid");
        }
        assert_eq!(folder.installed().map(|i| folder.bin(&i)), Some(bin));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn xz_archives_unpack_too() {
        let dir = std::env::temp_dir().join(format!("violet-tools-{}", uuid::Uuid::new_v4()));
        let folder = ToolsFolder::new(&dir, "mysql");
        let tar = tarball(&[("pg/bin/pg_dump", b"#!dump", None)]);
        let options = lzma_rust2::XzOptions::with_preset(1);
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), options).expect("xz");
        std::io::Write::write_all(&mut xz, &tar).expect("xz");
        let bytes = xz.finish().expect("xz");
        let download = Download {
            url: "https://example.com/pg.tar.xz".into(),
            ..download()
        };
        let bin = folder.install(&download, &bytes).expect("installed");
        assert!(bin.join("pg_dump").is_file());
        // The same bytes are not gzip.
        assert!(folder.install(&self::download(), &bytes).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_out_and_odd_paths_are_refused() {
        let dir = std::env::temp_dir().join(format!("violet-tools-{}", uuid::Uuid::new_v4()));
        let folder = ToolsFolder::new(&dir, "postgres");
        let out = archive(&[
            ("pg/bin/pg_dump", b"x", None),
            ("pg/lib/evil", b"", Some("../../../../.zshrc")),
        ]);
        assert!(folder.install(&download(), &out).is_err());
        let nothing = archive(&[("other/bin/pg_dump", b"x", None)]);
        assert!(folder.install(&download(), &nothing).is_err());
        assert!(folder.installed().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
