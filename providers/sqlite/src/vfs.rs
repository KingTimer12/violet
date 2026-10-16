//! SQLite's file layer, over Violet's `files` interface.
//!
//! SQLite is built with `SQLITE_OS_OTHER`, without an operating-system
//! layer of its own, and calls [`sqlite3_os_init`] to get one: this VFS. It
//! knows three files: the database ([`DATABASE`]), its rollback journal
//! and its write-ahead log, all opened by Violet next to the file the user
//! picked. Files SQLite opens without a name (temporary tables, sorting,
//! `VACUUM`) live in memory.
//!
//! WAL needs shared memory between connections, which a sandbox cannot
//! map, so this VFS has none: a database in WAL mode is opened with
//! `locking_mode=EXCLUSIVE` (see `session.rs`), where SQLite keeps the WAL
//! index in its own memory.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::hash::{BuildHasher, Hasher};
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libsqlite3_sys::{
    SQLITE_BUSY, SQLITE_CANTOPEN, SQLITE_IOERR, SQLITE_IOERR_DELETE, SQLITE_IOERR_FSTAT,
    SQLITE_IOERR_FSYNC, SQLITE_IOERR_LOCK, SQLITE_IOERR_READ, SQLITE_IOERR_SHORT_READ,
    SQLITE_IOERR_TRUNCATE, SQLITE_IOERR_UNLOCK, SQLITE_IOERR_WRITE, SQLITE_LOCK_EXCLUSIVE,
    SQLITE_LOCK_PENDING, SQLITE_LOCK_RESERVED, SQLITE_LOCK_SHARED, SQLITE_NOTFOUND, SQLITE_OK,
    SQLITE_OPEN_CREATE, SQLITE_OPEN_DELETEONCLOSE, SQLITE_OPEN_MAIN_DB, SQLITE_OPEN_MAIN_JOURNAL,
    SQLITE_OPEN_READONLY, SQLITE_OPEN_WAL, sqlite3_file, sqlite3_int64, sqlite3_io_methods,
    sqlite3_vfs,
};
use violet_sdk::files::{self, LockLevel};

/// The name SQLite opens the database by. The VFS maps it, and the
/// journals named after it, to the file the profile names.
pub const DATABASE: &CStr = c"violet.db";

/// The VFS's own name.
const NAME: &CStr = c"violet";

/// Longest path SQLite may build from [`DATABASE`].
const MAX_PATH: c_int = 64;

thread_local! {
    /// The last host error, for `xGetLastError` and the session's error
    /// messages.
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The last file error the host reported, if any since the last call.
pub fn take_last_error() -> Option<String> {
    LAST_ERROR.with(|last| last.borrow_mut().take())
}

fn remember(error: &violet_sdk::DbError) {
    LAST_ERROR.with(|last| *last.borrow_mut() = Some(error.message.clone()));
}

/// The open file behind a `sqlite3_file`.
enum Backing {
    Host(files::File),
    Memory(Vec<u8>),
}

/// The `sqlite3_file` SQLite allocates (`szOsFile` bytes) and this VFS
/// fills: SQLite's header, then the file.
#[repr(C)]
struct VioletFile {
    base: sqlite3_file,
    backing: *mut Backing,
}

/// Which of the three files a name means.
fn suffix(name: &CStr) -> Option<&'static str> {
    let name = name.to_bytes();
    let base = DATABASE.to_bytes();
    let rest = name.strip_prefix(base)?;
    ["", "-journal", "-wal"]
        .into_iter()
        .find(|suffix| rest == suffix.as_bytes())
}

/// Registers the VFS as the default. SQLite calls it from
/// `sqlite3_initialize` when built with `SQLITE_OS_OTHER`.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_os_init() -> c_int {
    static VFS: OnceLock<usize> = OnceLock::new();
    let vfs = *VFS.get_or_init(|| {
        let vfs = Box::new(sqlite3_vfs {
            iVersion: 2,
            szOsFile: c_int::try_from(size_of::<VioletFile>()).unwrap_or(c_int::MAX),
            mxPathname: MAX_PATH,
            pNext: ptr::null_mut(),
            zName: NAME.as_ptr(),
            pAppData: ptr::null_mut(),
            xOpen: Some(open),
            xDelete: Some(delete),
            xAccess: Some(access),
            xFullPathname: Some(full_pathname),
            xDlOpen: None,
            xDlError: None,
            xDlSym: None,
            xDlClose: None,
            xRandomness: Some(randomness),
            xSleep: Some(sleep),
            xCurrentTime: Some(current_time),
            xGetLastError: Some(last_error),
            xCurrentTimeInt64: Some(current_time_int64),
            xSetSystemCall: None,
            xGetSystemCall: None,
            xNextSystemCall: None,
        });
        Box::into_raw(vfs) as usize
    });
    // SAFETY: `vfs` is the leaked box above, alive for the program.
    unsafe { libsqlite3_sys::sqlite3_vfs_register(vfs as *mut sqlite3_vfs, 1) }
}

/// Nothing to undo: the VFS lives as long as the program.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_os_end() -> c_int {
    SQLITE_OK
}

static METHODS: sqlite3_io_methods = sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(check_reserved),
    xFileControl: Some(file_control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn open(
    _vfs: *mut sqlite3_vfs,
    name: *const c_char,
    file: *mut sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes `szOsFile` writable bytes, which `xClose` is
    // only called for when this sets `pMethods`.
    let file = unsafe { &mut *file.cast::<VioletFile>() };
    file.base.pMethods = ptr::null();
    let backing = if name.is_null() || flags & SQLITE_OPEN_DELETEONCLOSE != 0 {
        Backing::Memory(Vec::new())
    } else {
        // SAFETY: a non-null name is a C string SQLite owns.
        let name = unsafe { CStr::from_ptr(name) };
        let Some(suffix) = suffix(name) else {
            return SQLITE_CANTOPEN;
        };
        let kind = flags & (SQLITE_OPEN_MAIN_DB | SQLITE_OPEN_MAIN_JOURNAL | SQLITE_OPEN_WAL);
        if kind == 0 {
            // A super-journal, or a file of an attached database.
            return SQLITE_CANTOPEN;
        }
        let create = flags & SQLITE_OPEN_CREATE != 0;
        let read_only = flags & SQLITE_OPEN_READONLY != 0;
        match files::open(suffix, create, read_only) {
            Ok(host) => Backing::Host(host),
            Err(error) => {
                remember(&error);
                return SQLITE_CANTOPEN;
            }
        }
    };
    file.backing = Box::into_raw(Box::new(backing));
    file.base.pMethods = &METHODS;
    if !out_flags.is_null() {
        // SAFETY: SQLite's out parameter.
        unsafe { *out_flags = flags };
    }
    SQLITE_OK
}

unsafe extern "C" fn delete(
    _vfs: *mut sqlite3_vfs,
    name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    // SAFETY: SQLite passes a C string.
    let Some(suffix) = suffix(unsafe { CStr::from_ptr(name) }) else {
        return SQLITE_IOERR_DELETE;
    };
    match files::delete(suffix) {
        Ok(()) => SQLITE_OK,
        Err(error) => {
            remember(&error);
            SQLITE_IOERR_DELETE
        }
    }
}

unsafe extern "C" fn access(
    _vfs: *mut sqlite3_vfs,
    name: *const c_char,
    // Whether the file can be written is found out when it opens.
    _flags: c_int,
    out: *mut c_int,
) -> c_int {
    // SAFETY: SQLite passes a C string and an out parameter.
    let suffix = suffix(unsafe { CStr::from_ptr(name) });
    let exists = match suffix {
        Some(suffix) => match files::exists(suffix) {
            Ok(exists) => exists,
            Err(error) => {
                remember(&error);
                return SQLITE_IOERR;
            }
        },
        None => false,
    };
    // SAFETY: as above.
    unsafe { *out = c_int::from(exists) };
    SQLITE_OK
}

unsafe extern "C" fn full_pathname(
    _vfs: *mut sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    // SAFETY: SQLite passes a C string and `size` bytes to write to.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes_with_nul();
    if name.len() > usize::try_from(size).unwrap_or(0) {
        return SQLITE_CANTOPEN;
    }
    // SAFETY: checked to fit.
    unsafe { ptr::copy_nonoverlapping(name.as_ptr().cast::<c_char>(), out, name.len()) };
    SQLITE_OK
}

unsafe extern "C" fn randomness(_vfs: *mut sqlite3_vfs, size: c_int, out: *mut c_char) -> c_int {
    // Seeds SQLite's own generator, which names nothing on disk here: the
    // standard library's randomly keyed hasher is enough.
    let size = usize::try_from(size).unwrap_or(0);
    // SAFETY: SQLite passes `size` writable bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), size) };
    for chunk in out.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(chunk.as_ptr() as usize);
        let bytes = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    c_int::try_from(size).unwrap_or(c_int::MAX)
}

unsafe extern "C" fn sleep(_vfs: *mut sqlite3_vfs, microseconds: c_int) -> c_int {
    std::thread::sleep(Duration::from_micros(
        u64::try_from(microseconds).unwrap_or(0),
    ));
    microseconds
}

/// Milliseconds since the Julian epoch, as SQLite counts time.
fn julian_millis() -> i64 {
    const UNIX_EPOCH_JULIAN_MS: i64 = 210_866_760_000_000;
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_EPOCH_JULIAN_MS + i64::try_from(since.as_millis()).unwrap_or(0)
}

unsafe extern "C" fn current_time(_vfs: *mut sqlite3_vfs, out: *mut f64) -> c_int {
    #[allow(clippy::cast_precision_loss)]
    let days = julian_millis() as f64 / 86_400_000.0;
    // SAFETY: SQLite's out parameter.
    unsafe { *out = days };
    SQLITE_OK
}

unsafe extern "C" fn current_time_int64(_vfs: *mut sqlite3_vfs, out: *mut sqlite3_int64) -> c_int {
    // SAFETY: SQLite's out parameter.
    unsafe { *out = julian_millis() };
    SQLITE_OK
}

unsafe extern "C" fn last_error(_vfs: *mut sqlite3_vfs, size: c_int, out: *mut c_char) -> c_int {
    let message = LAST_ERROR
        .with(|last| last.borrow().clone())
        .unwrap_or_default();
    let size = usize::try_from(size).unwrap_or(0);
    if size > 0 {
        let len = message.len().min(size - 1);
        // SAFETY: SQLite passes `size` writable bytes.
        unsafe {
            ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), out, len);
            *out.add(len) = 0;
        }
    }
    0
}

/// The file behind SQLite's handle.
///
/// # Safety
///
/// `file` must be a handle `open` filled.
unsafe fn backing<'a>(file: *mut sqlite3_file) -> &'a mut Backing {
    // SAFETY: per the contract, `backing` is the box `open` leaked.
    unsafe { &mut *(*file.cast::<VioletFile>()).backing }
}

unsafe extern "C" fn close(file: *mut sqlite3_file) -> c_int {
    // SAFETY: SQLite closes each file it opened once.
    let file = unsafe { &mut *file.cast::<VioletFile>() };
    if !file.backing.is_null() {
        // SAFETY: the box `open` leaked, dropped once: dropping the host
        // file releases its locks.
        drop(unsafe { Box::from_raw(file.backing) });
        file.backing = ptr::null_mut();
    }
    SQLITE_OK
}

unsafe extern "C" fn read(
    file: *mut sqlite3_file,
    out: *mut c_void,
    amount: c_int,
    offset: sqlite3_int64,
) -> c_int {
    let amount = usize::try_from(amount).unwrap_or(0);
    let offset = u64::try_from(offset).unwrap_or(0);
    // SAFETY: SQLite passes `amount` writable bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), amount) };
    // SAFETY: an open file.
    let got = match unsafe { backing(file) } {
        Backing::Host(host) => match host.read(offset, u32::try_from(amount).unwrap_or(u32::MAX)) {
            Ok(bytes) => {
                out[..bytes.len()].copy_from_slice(&bytes);
                bytes.len()
            }
            Err(error) => {
                remember(&error);
                return SQLITE_IOERR_READ;
            }
        },
        Backing::Memory(bytes) => {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let end = start.saturating_add(amount).min(bytes.len());
            out[..end - start].copy_from_slice(&bytes[start..end]);
            end - start
        }
    };
    if got < amount {
        // SQLite requires the rest zeroed on a short read.
        out[got..].fill(0);
        return SQLITE_IOERR_SHORT_READ;
    }
    SQLITE_OK
}

unsafe extern "C" fn write(
    file: *mut sqlite3_file,
    data: *const c_void,
    amount: c_int,
    offset: sqlite3_int64,
) -> c_int {
    let amount = usize::try_from(amount).unwrap_or(0);
    // SAFETY: SQLite passes `amount` readable bytes.
    let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), amount) };
    let offset = u64::try_from(offset).unwrap_or(0);
    // SAFETY: an open file.
    match unsafe { backing(file) } {
        Backing::Host(host) => match host.write(offset, data) {
            Ok(()) => SQLITE_OK,
            Err(error) => {
                remember(&error);
                SQLITE_IOERR_WRITE
            }
        },
        Backing::Memory(bytes) => {
            let Ok(start) = usize::try_from(offset) else {
                return SQLITE_IOERR_WRITE;
            };
            let end = start + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(data);
            SQLITE_OK
        }
    }
}

unsafe extern "C" fn truncate(file: *mut sqlite3_file, size: sqlite3_int64) -> c_int {
    let size = u64::try_from(size).unwrap_or(0);
    // SAFETY: an open file.
    match unsafe { backing(file) } {
        Backing::Host(host) => match host.truncate(size) {
            Ok(()) => SQLITE_OK,
            Err(error) => {
                remember(&error);
                SQLITE_IOERR_TRUNCATE
            }
        },
        Backing::Memory(bytes) => {
            bytes.truncate(usize::try_from(size).unwrap_or(usize::MAX));
            SQLITE_OK
        }
    }
}

unsafe extern "C" fn sync(file: *mut sqlite3_file, _flags: c_int) -> c_int {
    // SAFETY: an open file.
    match unsafe { backing(file) } {
        Backing::Host(host) => match host.sync() {
            Ok(()) => SQLITE_OK,
            Err(error) => {
                remember(&error);
                SQLITE_IOERR_FSYNC
            }
        },
        Backing::Memory(_) => SQLITE_OK,
    }
}

unsafe extern "C" fn file_size(file: *mut sqlite3_file, out: *mut sqlite3_int64) -> c_int {
    // SAFETY: an open file.
    let size = match unsafe { backing(file) } {
        Backing::Host(host) => match host.size() {
            Ok(size) => size,
            Err(error) => {
                remember(&error);
                return SQLITE_IOERR_FSTAT;
            }
        },
        Backing::Memory(bytes) => bytes.len() as u64,
    };
    // SAFETY: SQLite's out parameter.
    unsafe { *out = sqlite3_int64::try_from(size).unwrap_or(sqlite3_int64::MAX) };
    SQLITE_OK
}

fn level(level: c_int) -> LockLevel {
    match level {
        SQLITE_LOCK_SHARED => LockLevel::Shared,
        SQLITE_LOCK_RESERVED => LockLevel::Reserved,
        SQLITE_LOCK_PENDING => LockLevel::Pending,
        SQLITE_LOCK_EXCLUSIVE => LockLevel::Exclusive,
        _ => LockLevel::None,
    }
}

unsafe extern "C" fn lock(file: *mut sqlite3_file, wanted: c_int) -> c_int {
    // SAFETY: an open file.
    match unsafe { backing(file) } {
        Backing::Host(host) => match host.lock(level(wanted)) {
            Ok(true) => SQLITE_OK,
            Ok(false) => SQLITE_BUSY,
            Err(error) => {
                remember(&error);
                SQLITE_IOERR_LOCK
            }
        },
        Backing::Memory(_) => SQLITE_OK,
    }
}

unsafe extern "C" fn unlock(file: *mut sqlite3_file, to: c_int) -> c_int {
    // SAFETY: an open file.
    match unsafe { backing(file) } {
        Backing::Host(host) => match host.unlock(level(to)) {
            Ok(()) => SQLITE_OK,
            Err(error) => {
                remember(&error);
                SQLITE_IOERR_UNLOCK
            }
        },
        Backing::Memory(_) => SQLITE_OK,
    }
}

unsafe extern "C" fn check_reserved(file: *mut sqlite3_file, out: *mut c_int) -> c_int {
    // SAFETY: an open file.
    let reserved = match unsafe { backing(file) } {
        Backing::Host(host) => match host.reserved() {
            Ok(reserved) => reserved,
            Err(error) => {
                remember(&error);
                return SQLITE_IOERR;
            }
        },
        Backing::Memory(_) => false,
    };
    // SAFETY: SQLite's out parameter.
    unsafe { *out = c_int::from(reserved) };
    SQLITE_OK
}

unsafe extern "C" fn file_control(
    _file: *mut sqlite3_file,
    _op: c_int,
    _arg: *mut c_void,
) -> c_int {
    SQLITE_NOTFOUND
}

unsafe extern "C" fn sector_size(_file: *mut sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn device_characteristics(_file: *mut sqlite3_file) -> c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_the_three_files() {
        assert_eq!(suffix(c"violet.db"), Some(""));
        assert_eq!(suffix(c"violet.db-journal"), Some("-journal"));
        assert_eq!(suffix(c"violet.db-wal"), Some("-wal"));
        assert_eq!(suffix(c"violet.db-shm"), None);
        assert_eq!(suffix(c"other.db"), None);
        assert_eq!(suffix(c"violet.db-journal2"), None);
    }
}
