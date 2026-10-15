//! Catalog queries. Names from the user reach the SQL only as hex
//! literals ([`literal`]), which no quoting or `sql_mode` can break out
//! of.

use std::fmt::Write as _;

pub(crate) const SERVER_INFO: &str = "SELECT VERSION(), DATABASE(), CURRENT_USER()";

/// Databases, which MySQL also calls schemas: the user's first, then the
/// system ones.
pub(crate) const SCHEMAS: &str = "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
     ORDER BY SCHEMA_NAME IN ('information_schema', 'mysql', 'performance_schema', 'sys'), \
     SCHEMA_NAME";

pub(crate) fn relations(schema: &str) -> String {
    format!(
        "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
        literal(schema)
    )
}

pub(crate) fn routines(schema: &str) -> String {
    format!(
        "SELECT ROUTINE_NAME, ROUTINE_COMMENT FROM information_schema.ROUTINES \
         WHERE ROUTINE_SCHEMA = {} ORDER BY ROUTINE_NAME",
        literal(schema)
    )
}

pub(crate) fn columns(schema: &str, relation: &str) -> String {
    format!(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE = 'YES', COLUMN_DEFAULT, \
         COLUMN_KEY = 'PRI', COLUMN_COMMENT FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
        literal(schema),
        literal(relation)
    )
}

/// `text` as a UTF-8 string literal written in hex: `_utf8mb4 X'…'`.
pub(crate) fn literal(text: &str) -> String {
    if text.is_empty() {
        return "''".into();
    }
    let mut out = String::with_capacity(12 + text.len() * 2);
    out.push_str("_utf8mb4 X'");
    for byte in text.bytes() {
        let _ = write!(out, "{byte:02X}");
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_their_literal() {
        assert_eq!(literal("a'b"), "_utf8mb4 X'612762'");
        assert_eq!(literal(""), "''");
        assert!(columns("s", "x\\' OR 1=1 -- ").contains("X'785C27204F5220313D31202D2D20'"));
    }
}
