//! The one file a file-based provider (SQLite) can open, and the locks on
//! it.
//!
//! The profile's host is the database's absolute path. A provider may open
//! that file and the `-journal` and `-wal` files beside it, nothing else.
//!
//! Locks follow SQLite's scheme (`os_unix.c`, `os_win.c`): byte-range locks
//! at the 1 GiB mark, on the same bytes native SQLite locks, so Violet and
//! other programs using the database take turns. Every connection Violet
//! opens to one database shares one handle and one lock state, as SQLite
//! does per inode: POSIX locks belong to the process, so connections here
//! are told apart before the operating system is asked.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind as IoKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use file_guard::{FileGuard, Lock};

use crate::bindings::types::{DbError, ErrorKind};
use crate::bindings::violet::provider::files::LockLevel;
use crate::socket::{Target, error};

/// SQLite's lock bytes: a page at 1 GiB that no database reads or writes.
const PENDING_BYTE: usize = 0x4000_0000;
const RESERVED_BYTE: usize = PENDING_BYTE + 1;
const SHARED_FIRST: usize = PENDING_BYTE + 2;
const SHARED_SIZE: usize = 510;

/// The files beside the database a provider may open.
const SUFFIXES: [&str; 3] = ["", "-journal", "-wal"];

/// Largest read a provider can ask for at once.
const MAX_READ: u32 = 16 << 20;

/// Files one instance may hold at once.
pub(crate) const MAX_FILES: usize = 8;

/// A lock level; each allows what the ones before it do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    None,
    Shared,
    Reserved,
    Pending,
    Exclusive,
}

impl From<LockLevel> for Level {
    fn from(level: LockLevel) -> Self {
        match level {
            LockLevel::None => Self::None,
            LockLevel::Shared => Self::Shared,
            LockLevel::Reserved => Self::Reserved,
            LockLevel::Pending => Self::Pending,
            LockLevel::Exclusive => Self::Exclusive,
        }
    }
}

type Guard = FileGuard<Arc<File>>;

/// One database file, shared by every connection Violet has to it.
struct Inode {
    file: Arc<File>,
    writable: bool,
    locks: Mutex<Locks>,
}

/// The locks this process holds on a database, and how.
struct Locks {
    /// The highest level any connection here holds.
    level: Level,
    /// Connections holding `Shared` or more.
    shared: usize,
    pending: Option<Guard>,
    reserved: Option<Guard>,
    /// Shared at `Shared`, exclusive at `Exclusive`.
    range: Option<Guard>,
}

/// The databases open now, by canonical path.
static INODES: LazyLock<Mutex<HashMap<PathBuf, Weak<Inode>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a lock attempt failed because someone else holds the bytes.
fn busy(error: &io::Error) -> bool {
    error.kind() == IoKind::WouldBlock
}

fn try_lock(file: &Arc<File>, kind: Lock, offset: usize, len: usize) -> io::Result<Option<Guard>> {
    match file_guard::try_lock(Arc::clone(file), kind, offset, len) {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if busy(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

impl Inode {
    /// The shared handle to the database at `path`, opening it if this
    /// process has not yet.
    fn open(path: &Path, create: bool) -> io::Result<Arc<Self>> {
        let mut inodes = lock(&INODES);
        inodes.retain(|_, inode| inode.strong_count() > 0);
        if let Ok(canonical) = path.canonicalize()
            && let Some(inode) = inodes.get(&canonical).and_then(Weak::upgrade)
        {
            return Ok(inode);
        }
        let (file, writable) = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)
        {
            Ok(file) => (file, true),
            Err(error) if error.kind() == IoKind::PermissionDenied => {
                (OpenOptions::new().read(true).open(path)?, false)
            }
            Err(error) => return Err(error),
        };
        let inode = Arc::new(Self {
            file: Arc::new(file),
            writable,
            locks: Mutex::new(Locks {
                level: Level::None,
                shared: 0,
                pending: None,
                reserved: None,
                range: None,
            }),
        });
        inodes.insert(path.canonicalize()?, Arc::downgrade(&inode));
        Ok(inode)
    }

    /// Raises one connection's lock from `*held` to `want`, as SQLite's
    /// `unixLock` does. False when the database is busy.
    fn lock(&self, held: &mut Level, want: Level) -> io::Result<bool> {
        if *held >= want {
            return Ok(true);
        }
        let mut locks = lock(&self.locks);
        // Another connection here is writing, or about to.
        if *held != locks.level && (locks.level >= Level::Pending || want > Level::Shared) {
            return Ok(false);
        }
        // Another connection here already reads: share its lock.
        if want == Level::Shared && matches!(locks.level, Level::Shared | Level::Reserved) {
            *held = Level::Shared;
            locks.shared += 1;
            return Ok(true);
        }
        // PENDING keeps new readers out while a reader arrives or a writer
        // waits for the readers to leave.
        let pending = want == Level::Shared || (want == Level::Exclusive && *held < Level::Pending);
        if pending && locks.pending.is_none() {
            match try_lock(&self.file, Lock::Exclusive, PENDING_BYTE, 1)? {
                Some(guard) => locks.pending = Some(guard),
                None => return Ok(false),
            }
        }
        match want {
            Level::None => Ok(true),
            Level::Shared => {
                let range = try_lock(&self.file, Lock::Shared, SHARED_FIRST, SHARED_SIZE);
                locks.pending = None;
                let Some(range) = range? else {
                    return Ok(false);
                };
                locks.range = Some(range);
                locks.shared = 1;
                locks.level = Level::Shared;
                *held = Level::Shared;
                Ok(true)
            }
            Level::Reserved => {
                let Some(reserved) = try_lock(&self.file, Lock::Exclusive, RESERVED_BYTE, 1)?
                else {
                    return Ok(false);
                };
                locks.reserved = Some(reserved);
                locks.level = Level::Reserved;
                *held = Level::Reserved;
                Ok(true)
            }
            Level::Pending | Level::Exclusive => {
                // Holding PENDING now, whatever happens next.
                locks.level = Level::Pending;
                *held = Level::Pending;
                // Other connections here still read.
                if want == Level::Pending || locks.shared > 1 {
                    return Ok(want == Level::Pending);
                }
                // Swap the shared range for an exclusive one. Nobody else
                // can take it in between: new readers need PENDING, which
                // this connection holds.
                locks.range = None;
                if let Some(range) =
                    try_lock(&self.file, Lock::Exclusive, SHARED_FIRST, SHARED_SIZE)?
                {
                    locks.range = Some(range);
                    locks.level = Level::Exclusive;
                    *held = Level::Exclusive;
                    return Ok(true);
                }
                // Readers elsewhere: read again, and wait.
                locks.range = Some(file_guard::try_lock(
                    Arc::clone(&self.file),
                    Lock::Shared,
                    SHARED_FIRST,
                    SHARED_SIZE,
                )?);
                Ok(false)
            }
        }
    }

    /// Lowers one connection's lock from `*held` to `to`, `Shared` or
    /// `None`.
    fn unlock(&self, held: &mut Level, to: Level) -> io::Result<()> {
        let to = to.min(Level::Shared);
        if *held <= to {
            return Ok(());
        }
        let mut locks = lock(&self.locks);
        if *held > Level::Shared {
            if locks.level == Level::Exclusive
                && let Some(range) = &mut locks.range
            {
                range.downgrade()?;
            }
            locks.reserved = None;
            locks.pending = None;
            locks.level = Level::Shared;
        }
        if to == Level::None {
            locks.shared = locks.shared.saturating_sub(1);
            if locks.shared == 0 {
                locks.range = None;
                locks.level = Level::None;
            }
        }
        *held = to;
        Ok(())
    }

    /// Whether any connection, here or elsewhere, holds RESERVED or more.
    fn reserved(&self) -> io::Result<bool> {
        let locks = lock(&self.locks);
        if locks.level > Level::Shared {
            return Ok(true);
        }
        Ok(try_lock(&self.file, Lock::Exclusive, RESERVED_BYTE, 1)?.is_none())
    }
}

enum Handle {
    /// The database, with this connection's lock on it.
    Database { inode: Arc<Inode>, held: Level },
    /// A journal beside it.
    Side(File),
}

/// A file a provider opened.
pub struct DbFile {
    handle: Handle,
    read_only: bool,
}

impl DbFile {
    /// Opens the profile's database, or the file `suffix` names beside it.
    pub(crate) fn open(
        target: &Target,
        suffix: &str,
        create: bool,
        read_only: bool,
    ) -> Result<Self, DbError> {
        let path = path(target, suffix)?;
        let handle = if suffix.is_empty() {
            let inode =
                Inode::open(&path, create && !read_only).map_err(|e| io_error(&path, &e))?;
            if !inode.writable && !read_only {
                return Err(error(
                    ErrorKind::Connection,
                    format!("{} is read-only", path.display()),
                ));
            }
            Handle::Database {
                inode,
                held: Level::None,
            }
        } else {
            let file = OpenOptions::new()
                .read(true)
                .write(!read_only)
                .create(create && !read_only)
                .open(&path)
                .map_err(|e| io_error(&path, &e))?;
            Handle::Side(file)
        };
        Ok(Self { handle, read_only })
    }

    fn file(&self) -> &File {
        match &self.handle {
            Handle::Database { inode, .. } => &inode.file,
            Handle::Side(file) => file,
        }
    }

    fn writable(&self) -> Result<&File, DbError> {
        if self.read_only {
            return Err(error(ErrorKind::Other, "The database is open read-only"));
        }
        Ok(self.file())
    }

    pub(crate) fn read(&self, offset: u64, len: u32) -> Result<Vec<u8>, DbError> {
        let mut buffer = vec![0; len.min(MAX_READ) as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match read_at(self.file(), &mut buffer[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == IoKind::Interrupted => {}
                Err(e) => return Err(failed("read", &e)),
            }
        }
        buffer.truncate(filled);
        Ok(buffer)
    }

    pub(crate) fn write(&self, offset: u64, bytes: &[u8]) -> Result<(), DbError> {
        let file = self.writable()?;
        let mut written = 0;
        while written < bytes.len() {
            match write_at(file, &bytes[written..], offset + written as u64) {
                Ok(0) => return Err(failed("write", &IoKind::WriteZero.into())),
                Ok(n) => written += n,
                Err(e) if e.kind() == IoKind::Interrupted => {}
                Err(e) => return Err(failed("write", &e)),
            }
        }
        Ok(())
    }

    pub(crate) fn size(&self) -> Result<u64, DbError> {
        Ok(self
            .file()
            .metadata()
            .map_err(|e| failed("inspect", &e))?
            .len())
    }

    pub(crate) fn truncate(&self, size: u64) -> Result<(), DbError> {
        self.writable()?
            .set_len(size)
            .map_err(|e| failed("truncate", &e))
    }

    pub(crate) fn sync(&self) -> Result<(), DbError> {
        if self.read_only {
            return Ok(());
        }
        self.file().sync_all().map_err(|e| failed("sync", &e))
    }

    pub(crate) fn lock(&mut self, level: LockLevel) -> Result<bool, DbError> {
        match &mut self.handle {
            Handle::Database { inode, held } => inode
                .lock(held, level.into())
                .map_err(|e| failed("lock", &e)),
            Handle::Side(_) => Ok(true),
        }
    }

    pub(crate) fn unlock(&mut self, level: LockLevel) -> Result<(), DbError> {
        match &mut self.handle {
            Handle::Database { inode, held } => inode
                .unlock(held, level.into())
                .map_err(|e| failed("unlock", &e)),
            Handle::Side(_) => Ok(()),
        }
    }

    pub(crate) fn reserved(&self) -> Result<bool, DbError> {
        match &self.handle {
            Handle::Database { inode, .. } => inode.reserved().map_err(|e| failed("lock", &e)),
            Handle::Side(_) => Ok(false),
        }
    }
}

impl Drop for DbFile {
    fn drop(&mut self) {
        if let Handle::Database { inode, held } = &mut self.handle
            && let Err(e) = inode.unlock(held, Level::None)
        {
            log::warn!(target: "violet::provider", "could not unlock the database: {e}");
        }
    }
}

/// Whether the file `suffix` names exists.
pub(crate) fn exists(target: &Target, suffix: &str) -> Result<bool, DbError> {
    let path = path(target, suffix)?;
    path.try_exists().map_err(|e| io_error(&path, &e))
}

/// Deletes a journal; never the database.
pub(crate) fn delete(target: &Target, suffix: &str) -> Result<(), DbError> {
    if suffix.is_empty() {
        return Err(error(
            ErrorKind::Unsupported,
            "A provider cannot delete the database",
        ));
    }
    let path = path(target, suffix)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == IoKind::NotFound => Ok(()),
        Err(e) => Err(io_error(&path, &e)),
    }
}

/// The file `suffix` names beside the profile's database. Only profiles
/// of file-based providers (port 0) with an absolute path have one.
fn path(target: &Target, suffix: &str) -> Result<PathBuf, DbError> {
    if !SUFFIXES.contains(&suffix) {
        return Err(error(
            ErrorKind::Unsupported,
            format!("A provider cannot open files named {suffix:?}"),
        ));
    }
    let database = Path::new(&target.host);
    if target.port != 0 || !database.is_absolute() || database.file_name().is_none() {
        return Err(error(
            ErrorKind::Unsupported,
            "This connection is not to a database file",
        ));
    }
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    Ok(PathBuf::from(path))
}

fn io_error(path: &Path, e: &io::Error) -> DbError {
    let kind = match e.kind() {
        IoKind::NotFound | IoKind::PermissionDenied => ErrorKind::Connection,
        _ => ErrorKind::Other,
    };
    error(kind, format!("{}: {e}", path.display()))
}

fn failed(what: &str, e: &io::Error) -> DbError {
    error(
        ErrorKind::Other,
        format!("Could not {what} the database file: {e}"),
    )
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(unix)]
fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, bytes, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(windows)]
fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, bytes, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &Path) -> Target {
        Target {
            host: path.to_string_lossy().into_owned(),
            port: 0,
            tls_required: false,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("violet-files-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("test.db")
    }

    #[test]
    fn only_the_database_and_its_journals_open() {
        let db = temp("names");
        let target = target(&db);
        assert!(path(&target, "").is_ok());
        assert!(path(&target, "-journal").is_ok());
        for bad in ["/../x", "-shm2", ".bak", "/etc/passwd"] {
            assert!(path(&target, bad).is_err(), "{bad}");
        }
        let mut network = target.clone();
        network.port = 5432;
        assert!(path(&network, "").is_err());
        let relative = Target {
            host: "test.db".into(),
            port: 0,
            tls_required: false,
        };
        assert!(path(&relative, "").is_err());
        assert!(delete(&target, "").is_err());
    }

    #[test]
    fn reads_and_writes_go_to_the_file() {
        let db = temp("io");
        let _ = std::fs::remove_file(&db);
        let target = target(&db);
        assert!(DbFile::open(&target, "", false, false).is_err(), "missing");
        let file = DbFile::open(&target, "", true, false).expect("create");
        file.write(10, b"hello").expect("write");
        assert_eq!(file.size().expect("size"), 15);
        assert_eq!(file.read(10, 100).expect("read"), b"hello");
        file.truncate(12).expect("truncate");
        assert_eq!(file.read(10, 100).expect("read"), b"he");
        let reader = DbFile::open(&target, "", false, true).expect("open read-only");
        assert!(reader.write(0, b"x").is_err());
    }

    #[test]
    fn connections_here_take_turns_like_sqlite() {
        let db = temp("locks");
        let target = target(&db);
        let mut a = DbFile::open(&target, "", true, false).expect("a");
        let mut b = DbFile::open(&target, "", false, false).expect("b");
        assert!(a.lock(LockLevel::Shared).expect("a shared"));
        assert!(b.lock(LockLevel::Shared).expect("b shared"));
        assert!(a.lock(LockLevel::Reserved).expect("a reserved"));
        assert!(b.reserved().expect("reserved"));
        assert!(
            !b.lock(LockLevel::Reserved).expect("b reserved"),
            "one writer"
        );
        // b still reads: a waits at PENDING.
        assert!(!a.lock(LockLevel::Exclusive).expect("a exclusive"));
        b.unlock(LockLevel::None).expect("b unlock");
        assert!(
            !b.lock(LockLevel::Shared).expect("b shared"),
            "pending keeps readers out"
        );
        assert!(a.lock(LockLevel::Exclusive).expect("a exclusive"));
        a.unlock(LockLevel::Shared).expect("a shared again");
        assert!(b.lock(LockLevel::Shared).expect("b shared"));
        a.unlock(LockLevel::None).expect("a unlock");
        drop(b);
        assert!(a.lock(LockLevel::Exclusive).expect("a alone"));
    }
}
