//! A thin, safe face on SQLite's C API: a connection and its statements.

use std::ffi::{CStr, c_char, c_int};
use std::fmt::Write as _;
use std::ptr;

use libsqlite3_sys as ffi;
use violet_sdk::{DbError, ErrorKind};

use crate::vfs;

/// What a column holds in one row.
pub enum Value<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl Value<'_> {
    /// The cell as the grid shows it.
    pub fn display(&self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::Integer(value) => Some(value.to_string()),
            Self::Real(value) => Some(real(*value)),
            Self::Text(text) => Some((*text).to_owned()),
            Self::Blob(bytes) => {
                // As the other providers show binary.
                let mut hex = String::with_capacity(bytes.len() * 2 + 2);
                hex.push_str("0x");
                for byte in *bytes {
                    let _ = write!(hex, "{byte:02X}");
                }
                Some(hex)
            }
        }
    }
}

/// A float as SQLite's shell prints it: `1.0` rather than `1`, and very
/// large or small ones with an exponent.
fn real(value: f64) -> String {
    let magnitude = value.abs();
    if !value.is_finite() {
        value.to_string()
    } else if magnitude != 0.0 && !(1e-4..1e15).contains(&magnitude) {
        format!("{value:e}")
    } else if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

/// An open connection.
pub struct Db(*mut ffi::sqlite3);

impl Db {
    /// Opens the profile's database through the VFS.
    pub fn open(read_only: bool) -> Result<Self, DbError> {
        let flags = ffi::SQLITE_OPEN_EXRESCODE
            | if read_only {
                ffi::SQLITE_OPEN_READONLY
            } else {
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE
            };
        let mut handle = ptr::null_mut();
        // SAFETY: a C string name, an out pointer, the default VFS.
        let rc = unsafe {
            ffi::sqlite3_open_v2(vfs::DATABASE.as_ptr(), &raw mut handle, flags, ptr::null())
        };
        let db = Self(handle);
        if rc != ffi::SQLITE_OK {
            return Err(db.error(rc, None));
        }
        // SAFETY: an open connection.
        unsafe { ffi::sqlite3_busy_timeout(db.0, 5000) };
        Ok(db)
    }

    /// The error SQLite reports for `rc`, with its message, and where in
    /// `statement` (byte offset `start` into the whole script) it is.
    pub fn error(&self, rc: c_int, statement: Option<(&str, usize)>) -> DbError {
        let message = if self.0.is_null() {
            // SAFETY: a static string for any code.
            unsafe { text(ffi::sqlite3_errstr(rc)) }
        } else {
            // SAFETY: an open (or failed to open) connection.
            unsafe { text(ffi::sqlite3_errmsg(self.0)) }
        };
        let code = if self.0.is_null() {
            rc
        } else {
            // SAFETY: as above.
            unsafe { ffi::sqlite3_extended_errcode(self.0) }
        };
        let kind = match code & 0xff {
            ffi::SQLITE_CANTOPEN | ffi::SQLITE_NOTADB | ffi::SQLITE_PERM => ErrorKind::Connection,
            ffi::SQLITE_INTERRUPT => ErrorKind::Cancelled,
            _ => ErrorKind::Query,
        };
        let mut error = DbError::new(kind, message).with_code(code.to_string());
        if let Some(host) = vfs::take_last_error() {
            error.detail = Some(host);
        }
        if matches!(code & 0xff, ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED) {
            error.hint =
                Some("Another program, or another connection, is writing to the database.".into());
        }
        if let Some((script, start)) = statement
            && !self.0.is_null()
        {
            // SAFETY: as above.
            let offset = unsafe { ffi::sqlite3_error_offset(self.0) };
            if let Ok(offset) = usize::try_from(offset) {
                let byte = (start + offset).min(script.len());
                let chars = script
                    .char_indices()
                    .take_while(|(index, _)| *index < byte)
                    .count();
                error.position = u32::try_from(chars + 1).ok();
            }
        }
        error
    }

    /// Prepares the first statement of `sql`; `None` when it holds only
    /// space and comments. Also returns how many bytes it took.
    pub fn prepare(&self, sql: &str) -> Result<(Option<Stmt<'_>>, usize), (c_int, usize)> {
        let mut stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let len = c_int::try_from(sql.len()).map_err(|_| (ffi::SQLITE_TOOBIG, 0))?;
        // SAFETY: `len` bytes of UTF-8 and out pointers.
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                self.0,
                sql.as_ptr().cast(),
                len,
                &raw mut stmt,
                &raw mut tail,
            )
        };
        let used = if tail.is_null() {
            sql.len()
        } else {
            (tail as usize)
                .saturating_sub(sql.as_ptr() as usize)
                .min(sql.len())
        };
        if rc != ffi::SQLITE_OK {
            return Err((rc, used));
        }
        let stmt = (!stmt.is_null()).then_some(Stmt {
            db: self,
            raw: stmt,
        });
        Ok((stmt, used))
    }

    /// Runs statements that return nothing worth keeping.
    pub fn exec(&self, sql: &str) -> Result<(), DbError> {
        let mut rest = sql;
        while !rest.trim().is_empty() {
            let (stmt, used) = self.prepare(rest).map_err(|(rc, _)| self.error(rc, None))?;
            if let Some(stmt) = stmt {
                while stmt.step().map_err(|rc| self.error(rc, None))? {}
            }
            rest = &rest[used..];
            if used == 0 {
                break;
            }
        }
        Ok(())
    }

    /// The rows of a query, each cell as text.
    pub fn rows(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let (stmt, _) = self.prepare(sql).map_err(|(rc, _)| self.error(rc, None))?;
        let Some(stmt) = stmt else {
            return Ok(Vec::new());
        };
        for (index, param) in params.iter().enumerate() {
            stmt.bind_text(index + 1, param)
                .map_err(|rc| self.error(rc, None))?;
        }
        let mut rows = Vec::new();
        while stmt.step().map_err(|rc| self.error(rc, None))? {
            rows.push(
                (0..stmt.column_count())
                    .map(|column| stmt.value(column).display())
                    .collect(),
            );
        }
        Ok(rows)
    }

    /// Rows the last `INSERT`, `UPDATE` or `DELETE` changed.
    pub fn changes(&self) -> u64 {
        // SAFETY: an open connection.
        u64::try_from(unsafe { ffi::sqlite3_changes64(self.0) }).unwrap_or(0)
    }

    pub fn version() -> String {
        // SAFETY: a static string.
        unsafe { text(ffi::sqlite3_libversion()) }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // SAFETY: closed once; every statement is finalized first, as each
        // borrows the connection.
        unsafe { ffi::sqlite3_close(self.0) };
    }
}

/// A prepared statement.
pub struct Stmt<'db> {
    db: &'db Db,
    raw: *mut ffi::sqlite3_stmt,
}

impl Stmt<'_> {
    /// Runs to the next row: true when there is one.
    pub fn step(&self) -> Result<bool, c_int> {
        // SAFETY: a live statement.
        match unsafe { ffi::sqlite3_step(self.raw) } {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            rc => Err(rc),
        }
    }

    pub fn bind_text(&self, index: usize, value: &str) -> Result<(), c_int> {
        let index = c_int::try_from(index).map_err(|_| ffi::SQLITE_RANGE)?;
        let len = c_int::try_from(value.len()).map_err(|_| ffi::SQLITE_TOOBIG)?;
        // SAFETY: SQLite copies the text (SQLITE_TRANSIENT).
        let rc = unsafe {
            ffi::sqlite3_bind_text(
                self.raw,
                index,
                value.as_ptr().cast(),
                len,
                ffi::SQLITE_TRANSIENT(),
            )
        };
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(rc)
        }
    }

    pub fn column_count(&self) -> usize {
        // SAFETY: a live statement.
        usize::try_from(unsafe { ffi::sqlite3_column_count(self.raw) }).unwrap_or(0)
    }

    pub fn column_name(&self, column: usize) -> String {
        // SAFETY: a live statement and a column in range.
        unsafe { text(ffi::sqlite3_column_name(self.raw, column as c_int)) }
    }

    /// The declared type of a column that reads a table's column.
    pub fn declared_type(&self, column: usize) -> Option<String> {
        // SAFETY: as above.
        let raw = unsafe { ffi::sqlite3_column_decltype(self.raw, column as c_int) };
        // SAFETY: null or a C string.
        (!raw.is_null()).then(|| unsafe { text(raw) })
    }

    /// The value in the current row.
    pub fn value(&self, column: usize) -> Value<'_> {
        let column = column as c_int;
        // SAFETY: a statement on a row, and a column in range. Text and
        // blobs stay valid until the next step, which borrows `self`.
        unsafe {
            match ffi::sqlite3_column_type(self.raw, column) {
                ffi::SQLITE_INTEGER => Value::Integer(ffi::sqlite3_column_int64(self.raw, column)),
                ffi::SQLITE_FLOAT => Value::Real(ffi::sqlite3_column_double(self.raw, column)),
                ffi::SQLITE_TEXT => {
                    let data = ffi::sqlite3_column_text(self.raw, column);
                    let len =
                        usize::try_from(ffi::sqlite3_column_bytes(self.raw, column)).unwrap_or(0);
                    if data.is_null() {
                        Value::Text("")
                    } else {
                        let bytes = std::slice::from_raw_parts(data, len);
                        // SQLite stores what it is given; show bad UTF-8 as
                        // the bytes it is.
                        std::str::from_utf8(bytes).map_or(Value::Blob(bytes), Value::Text)
                    }
                }
                ffi::SQLITE_BLOB => {
                    let data = ffi::sqlite3_column_blob(self.raw, column);
                    let len =
                        usize::try_from(ffi::sqlite3_column_bytes(self.raw, column)).unwrap_or(0);
                    if data.is_null() {
                        Value::Blob(&[])
                    } else {
                        Value::Blob(std::slice::from_raw_parts(data.cast::<u8>(), len))
                    }
                }
                _ => Value::Null,
            }
        }
    }

    /// Whether the statement leaves the database as it was.
    pub fn read_only(&self) -> bool {
        // SAFETY: a live statement.
        unsafe { ffi::sqlite3_stmt_readonly(self.raw) != 0 }
    }

    pub fn db(&self) -> &Db {
        self.db
    }
}

impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        // SAFETY: finalized once.
        unsafe { ffi::sqlite3_finalize(self.raw) };
    }
}

/// A C string SQLite returned, copied.
///
/// # Safety
///
/// `raw` is null or a valid C string.
unsafe fn text(raw: *const c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: per the contract.
    unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::real;

    #[test]
    fn reals_print_like_sqlite() {
        assert_eq!(real(1.0), "1.0");
        assert_eq!(real(-2.5), "-2.5");
        assert_eq!(real(0.1), "0.1");
        assert_eq!(real(1e300), "1e300");
        assert_eq!(real(1.5e-7), "1.5e-7");
        assert_eq!(real(0.0), "0.0");
    }
}
