//! A workspace in and out of a backup file.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use violet_core::Secret;
use violet_core::ports::{SavedProfiles, SavedSecret, SecretStore};
use violet_storage::{create_private_dir, open_private};
use zeroize::Zeroizing;

use crate::archive::safe_path;
use crate::bundle::{Bundle, BundleFile, FORMAT, Manifest};
use crate::container::{self, MAX_FILE, MAX_PAYLOAD};
use crate::{Error, Result};

/// The workspace to back up.
pub struct Source<'a> {
    pub profiles: &'a SavedProfiles,
    pub secrets: &'a dyn SecretStore,
    pub scripts_dir: &'a Path,
    pub app_version: &'a str,
}

/// The backup of `source`, sealed with `password`, ready to be written.
pub fn export(source: &Source<'_>, password: &Secret) -> Result<Vec<u8>> {
    let bundle = gather(source)?;
    let archive = bundle.encode()?;
    container::seal(&archive, password)
}

/// What `file` holds, if `password` opens it.
pub fn open(file: &[u8], password: &Secret) -> Result<Bundle> {
    let archive = container::open(file, password)?;
    Bundle::decode(&archive)
}

/// Reads a backup file, refusing one larger than any backup can be
/// before reading it.
pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_FILE {
        return Err(Error::TooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes a backup file, readable by its owner only on Unix.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = open_private(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The bundle of `source`: its connections, the passwords saved for them
/// (each read for its connection's own target) and its scripts folder.
pub fn gather(source: &Source<'_>) -> Result<Bundle> {
    let mut passwords = Vec::new();
    for profile in source
        .profiles
        .connections
        .iter()
        .filter(|p| p.save_password)
    {
        if let SavedSecret::Found(secret) =
            source.secrets.get(profile.id, &profile.secret_target())?
        {
            passwords.push((profile.id, secret));
        }
    }
    let mut files = Vec::new();
    let mut total = 0;
    if source.scripts_dir.is_dir() {
        walk(source.scripts_dir, "", &mut files, &mut total)?;
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Bundle {
        manifest: Manifest {
            format: FORMAT,
            app_version: source.app_version.to_owned(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            os: std::env::consts::OS.to_owned(),
            connections: source.profiles.connections.len(),
            passwords: passwords.len(),
            files: files.len(),
        },
        profiles: source.profiles.clone(),
        passwords,
        files,
    })
}

/// Adds the regular files below `dir` to `files`, as `prefix/name`. Links
/// are skipped: a backup holds what the folder holds, not what it points
/// at.
fn walk(dir: &Path, prefix: &str, files: &mut Vec<BundleFile>, total: &mut u64) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if !safe_path(&path) {
            continue;
        }
        if kind.is_dir() {
            walk(&entry.path(), &path, files, total)?;
        } else if kind.is_file() {
            let mut file = fs::File::open(entry.path())?;
            let len = file.metadata()?.len();
            *total += len;
            if *total > MAX_PAYLOAD {
                return Err(Error::TooLarge);
            }
            let mut data = Zeroizing::new(Vec::with_capacity(usize::try_from(len).unwrap_or(0)));
            file.read_to_end(&mut data)?;
            files.push(BundleFile { path, data });
        }
    }
    Ok(())
}

/// Writes `files` into the scripts folder `root`, replacing files of the
/// same path. Each path is checked again and never written through a link.
/// Returns how many were written.
pub fn restore_files(root: &Path, files: &[BundleFile]) -> Result<usize> {
    for file in files {
        if !safe_path(&file.path) {
            return Err(Error::Malformed(format!(
                "{} points outside the backup",
                file.path
            )));
        }
        let target: PathBuf = file
            .path
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = target.parent() {
            create_private_dir(parent)?;
        }
        // Writing through a link would put the file wherever it points.
        if fs::symlink_metadata(&target).is_ok_and(|meta| !meta.file_type().is_file()) {
            return Err(Error::Io(format!(
                "{} is not a plain file; not replacing it",
                target.display()
            )));
        }
        let mut out = open_private(&target)?;
        out.write_all(&file.data)?;
        out.sync_all()?;
    }
    Ok(files.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::{ConnectionProfile, DriverId};
    use violet_storage::MemorySecretStore;

    #[test]
    fn a_workspace_round_trips_through_a_file() {
        let from = tempfile::tempdir().expect("temp dir");
        let scripts = from.path().join("scripts");
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.host = "db.example.com".to_owned();
        let mut unsaved = ConnectionProfile::new(DriverId::new("mysql"), 3306);
        unsaved.save_password = false;
        let secrets = MemorySecretStore::default();
        secrets
            .set(profile.id, &profile.secret_target(), &Secret::new("pw"))
            .expect("set");
        // A password left for a connection that does not save one is not
        // carried.
        secrets
            .set(unsaved.id, &unsaved.secret_target(), &Secret::new("no"))
            .expect("set");
        let script = scripts.join(profile.id.to_string()).join("app");
        fs::create_dir_all(script.join("reports")).expect("dirs");
        fs::write(script.join("a.sql"), "select 1").expect("write");
        fs::write(script.join("reports/b.sql"), "select 2").expect("write");
        fs::create_dir_all(scripts.join(".history/x")).expect("dirs");
        fs::write(scripts.join(".history/x/1.sql"), "old").expect("write");

        let profiles = SavedProfiles {
            connections: vec![profile.clone(), unsaved],
            folders: vec!["Empty".to_owned()],
        };
        let password = Secret::new("export password");
        let file = export(
            &Source {
                profiles: &profiles,
                secrets: &secrets,
                scripts_dir: &scripts,
                app_version: "0.1.9",
            },
            &password,
        )
        .expect("export");
        let path = from.path().join(format!("w.{}", container::EXTENSION));
        write_file(&path, &file).expect("write");

        let bundle = open(&read_file(&path).expect("read"), &password).expect("open");
        assert_eq!(bundle.profiles, profiles);
        assert_eq!(bundle.passwords, vec![(profile.id, Secret::new("pw"))]);
        assert_eq!(bundle.manifest.files, 3);

        let to = tempfile::tempdir().expect("temp dir");
        let restored = to.path().join("scripts");
        assert_eq!(restore_files(&restored, &bundle.files).expect("restore"), 3);
        let id = profile.id.to_string();
        assert_eq!(
            fs::read_to_string(restored.join(&id).join("app/reports/b.sql")).expect("read"),
            "select 2"
        );
        assert_eq!(
            fs::read_to_string(restored.join(".history/x/1.sql")).expect("read"),
            "old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restoring_never_writes_through_a_link() {
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let victim = outside.path().join("victim");
        fs::write(&victim, "keep").expect("write");
        std::os::unix::fs::symlink(&victim, root.path().join("a.sql")).expect("link");
        let files = [BundleFile {
            path: "a.sql".to_owned(),
            data: Zeroizing::new(b"overwritten".to_vec()),
        }];
        assert!(restore_files(root.path(), &files).is_err());
        assert_eq!(fs::read_to_string(&victim).expect("read"), "keep");
    }

    #[test]
    fn restoring_refuses_paths_that_climb_out() {
        let root = tempfile::tempdir().expect("temp dir");
        let files = [BundleFile {
            path: "../escape.sql".to_owned(),
            data: Zeroizing::new(Vec::new()),
        }];
        assert!(matches!(
            restore_files(root.path(), &files),
            Err(Error::Malformed(_))
        ));
    }
}
