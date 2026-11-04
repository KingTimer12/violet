//! The tar inside a backup, built and read in memory.
//!
//! Reading is strict, as the archive comes from a file anyone could have
//! made (the tag only proves it was made with the password): every entry
//! must be a regular file, its path relative, with no `..`, no root, no
//! drive or UNC prefix and no backslash, and no path may appear twice.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::Component;

use zeroize::Zeroizing;

use crate::{Error, Result};

/// Most entries an archive may hold.
pub const MAX_ENTRIES: usize = 100_000;

/// A file of the archive.
pub struct Entry {
    /// Relative, with `/` between its parts.
    pub path: String,
    pub data: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("path", &self.path)
            .field("len", &self.data.len())
            .finish()
    }
}

/// Whether `path` is one an archive may hold: relative parts joined by `/`,
/// none of them empty, `.` or `..`, and none holding `\`, `:` or NUL (a
/// Windows drive, separator or stream).
#[must_use]
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':', '\0'])
        })
}

/// A tar of `entries`, in a buffer sized up front and cleared when
/// dropped.
pub fn build<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a [u8])> + Clone,
) -> Result<Zeroizing<Vec<u8>>> {
    // Each entry: a header block, a GNU long name (two blocks and the name)
    // when the path is long, and the data rounded up to a block. Then the
    // two blocks that end the archive.
    let capacity = entries
        .clone()
        .into_iter()
        .map(|(path, data)| {
            3 * 512 + path.len().next_multiple_of(512) + data.len().next_multiple_of(512)
        })
        .sum::<usize>()
        + 2 * 512;
    let mut buffer = Zeroizing::new(Vec::with_capacity(capacity));
    {
        let mut builder = tar::Builder::new(&mut *buffer);
        for (path, data) in entries {
            if !safe_path(path) {
                return Err(Error::Io(format!("Cannot archive {path}")));
            }
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(0o600);
            header.set_mtime(0);
            builder.append_data(&mut header, path, data)?;
        }
        builder.finish()?;
    }
    Ok(buffer)
}

/// The files of `archive`, or why it is refused.
pub fn extract(archive: &[u8]) -> Result<Vec<Entry>> {
    let malformed = |what: String| Error::Malformed(what);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut archive = tar::Archive::new(archive);
    for entry in archive
        .entries()
        .map_err(|error| malformed(format!("cannot read its archive: {error}")))?
    {
        let mut entry =
            entry.map_err(|error| malformed(format!("cannot read its archive: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| malformed(format!("an entry has an unreadable name: {error}")))?
            .into_owned();
        let shown = path.display().to_string();
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(malformed(format!(
                "{shown} is a link, folder or special file, which backups never hold"
            )));
        }
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(
                    part.to_str()
                        .ok_or_else(|| malformed(format!("{shown} is not UTF-8")))?,
                ),
                Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_)
                | Component::CurDir => {
                    return Err(malformed(format!("{shown} points outside the backup")));
                }
            }
        }
        let path = parts.join("/");
        if !safe_path(&path) {
            return Err(malformed(format!("{shown} points outside the backup")));
        }
        if !seen.insert(path.clone()) {
            return Err(malformed(format!("{path} appears twice")));
        }
        if out.len() >= MAX_ENTRIES {
            return Err(malformed("it holds too many files".to_owned()));
        }
        // The archive is in memory and whole, so the size is bounded by it.
        let size = usize::try_from(entry.size())
            .ok()
            .filter(|size| *size <= archive_len_hint())
            .ok_or_else(|| malformed(format!("{path} is too large")))?;
        let mut data = Zeroizing::new(Vec::with_capacity(size));
        entry
            .read_to_end(&mut data)
            .map_err(|error| malformed(format!("cannot read {path}: {error}")))?;
        out.push(Entry { path, data });
    }
    Ok(out)
}

/// Largest size an entry may claim: what a whole payload may hold.
fn archive_len_hint() -> usize {
    usize::try_from(crate::container::MAX_PAYLOAD).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_round_trip() {
        let long = format!("scripts/{}query.sql", "deep/".repeat(40));
        let files: Vec<(&str, &[u8])> = vec![
            ("manifest.json", b"{}"),
            ("scripts/a/b.sql", b"select 1"),
            (&long, b"select 2"),
            ("empty", b""),
        ];
        let archive = build(files.iter().copied()).expect("build");
        let entries = extract(&archive).expect("extract");
        let got: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.data.as_slice()))
            .collect();
        assert_eq!(got, files);
    }

    /// A tar with one entry, its header written by hand so nothing checks
    /// the name or type on the way in.
    fn raw(path: &[u8], kind: tar::EntryType, data: &[u8], link: Option<&[u8]>) -> Vec<u8> {
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..path.len()].copy_from_slice(path);
        if let Some(link) = link {
            header.as_old_mut().linkname[..link.len()].copy_from_slice(link);
        }
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
        out.extend_from_slice(&[0; 1024]);
        out
    }

    #[test]
    fn paths_that_climb_out_are_refused() {
        for path in [
            &b"../escape.sql"[..],
            b"scripts/../../escape.sql",
            b"/etc/passwd",
            b"./manifest.json",
            b"scripts/C:\\Windows\\x",
            b"scripts\\..\\..\\x",
        ] {
            let archive = raw(path, tar::EntryType::Regular, b"x", None);
            assert!(
                matches!(extract(&archive), Err(Error::Malformed(_))),
                "{}",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn links_folders_and_special_files_are_refused() {
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Directory,
            tar::EntryType::Fifo,
            tar::EntryType::Char,
            tar::EntryType::Block,
        ] {
            let archive = raw(b"scripts/link", kind, b"", Some(b"/etc/passwd"));
            assert!(
                matches!(extract(&archive), Err(Error::Malformed(_))),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_path_twice_is_refused() {
        let mut archive = raw(b"a.sql", tar::EntryType::Regular, b"1", None);
        archive.truncate(archive.len() - 1024);
        archive.extend(raw(b"a.sql", tar::EntryType::Regular, b"2", None));
        assert!(matches!(extract(&archive), Err(Error::Malformed(_))));
    }

    #[test]
    fn unsafe_names_are_not_archived() {
        assert!(build([("../x", &b""[..])]).is_err());
        assert!(safe_path("scripts/a b/c.sql"));
        assert!(!safe_path("scripts//c.sql"));
        assert!(!safe_path(""));
    }
}
