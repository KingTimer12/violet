//! What sets one SQL database apart from another for the editor: the
//! statements, keywords, functions and types completion offers on it.
//! [`Family::Other`] gets everything, as before databases were told apart.

use crate::sql;

/// The database family a driver speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Family {
    Postgres,
    MySql,
    Sqlite,
    #[default]
    Other,
}

impl Family {
    pub fn of(driver: &str) -> Self {
        let driver = driver.to_ascii_lowercase();
        if driver.contains("postgres") {
            Self::Postgres
        } else if driver.contains("mysql") || driver.contains("maria") {
            Self::MySql
        } else if driver.contains("sqlite") {
            Self::Sqlite
        } else {
            Self::Other
        }
    }

    /// A backtick quotes a name.
    pub fn backtick_quotes(self) -> bool {
        matches!(self, Self::MySql | Self::Sqlite)
    }

    /// `[name]` quotes a name, which SQLite accepts for SQL Server's sake.
    pub fn bracket_quotes(self) -> bool {
        self == Self::Sqlite
    }

    /// `#` starts a line comment.
    pub fn hash_comments(self) -> bool {
        self == Self::MySql
    }

    /// A backslash escapes a quote inside a string.
    pub fn escapes_with_backslash(self) -> bool {
        self == Self::MySql
    }

    /// Words reserved here beyond the ones every family reserves, sorted.
    pub fn reserved(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &["FETCH", "ILIKE", "OFFSET", "RETURNING"],
            Self::Sqlite => &["OFFSET", "RETURNING"],
            Self::MySql | Self::Other => &[],
        }
    }

    /// Statements offered at the start of one.
    pub fn starts(self) -> Vec<&'static str> {
        let only: &[&str] = match self {
            Self::Postgres => &["EXPLAIN ANALYZE", "SHOW", "VACUUM", "COPY"],
            Self::MySql => &[
                "EXPLAIN ANALYZE",
                "SHOW TABLES",
                "SHOW DATABASES",
                "SHOW COLUMNS FROM",
                "SHOW CREATE TABLE",
                "DESCRIBE",
                "USE",
                "REPLACE INTO",
                "INSERT IGNORE INTO",
            ],
            Self::Sqlite => &[
                "EXPLAIN QUERY PLAN",
                "PRAGMA",
                "ATTACH DATABASE",
                "DETACH DATABASE",
                "VACUUM",
                "REPLACE INTO",
                "INSERT OR REPLACE INTO",
                "INSERT OR IGNORE INTO",
            ],
            Self::Other => &["EXPLAIN ANALYZE", "SHOW"],
        };
        let mut starts = vec![
            "SELECT",
            "WITH",
            "INSERT INTO",
            "UPDATE",
            "DELETE FROM",
            "CREATE TABLE",
            "CREATE VIEW",
            "CREATE INDEX",
            "ALTER TABLE",
            "DROP TABLE",
        ];
        if self != Self::Sqlite {
            starts.push("TRUNCATE");
        }
        starts.extend(["EXPLAIN", "BEGIN", "COMMIT", "ROLLBACK", "VALUES"]);
        starts.extend(only);
        starts
    }

    /// What may follow a table in `FROM`, `JOIN` or `UPDATE`.
    pub fn after_relation(self) -> Vec<&'static str> {
        let mut words = vec![
            "AS",
            "WHERE",
            "JOIN",
            "INNER JOIN",
            "LEFT JOIN",
            "RIGHT JOIN",
            "FULL JOIN",
            "CROSS JOIN",
            "ON",
            "USING",
            "GROUP BY",
            "ORDER BY",
            "HAVING",
            "LIMIT",
            "OFFSET",
            "UNION",
            "UNION ALL",
            "EXCEPT",
            "INTERSECT",
            "SET",
            "VALUES",
            "RETURNING",
            "WINDOW",
        ];
        match self {
            // MySQL has no full joins and no `RETURNING`.
            Self::MySql => {
                words.retain(|w| !matches!(*w, "FULL JOIN" | "RETURNING"));
                words.push("STRAIGHT_JOIN");
            }
            Self::Sqlite => words.push("INDEXED BY"),
            Self::Postgres | Self::Other => {}
        }
        words
    }

    /// Keywords offered in expressions: the lexer's, less the ones this
    /// database lacks, plus its own.
    pub fn keywords(self) -> Vec<&'static str> {
        let (lacks, own): (&[&str], &[&str]) = match self {
            Self::Postgres => (&[], &["SIMILAR", "TABLESAMPLE"]),
            Self::MySql => (
                &[
                    "CONFLICT",
                    "COPY",
                    "EXTENSION",
                    "FILTER",
                    "ILIKE",
                    "MATERIALIZED",
                    "NOTHING",
                    "NULLS",
                    "REFRESH",
                    "RETURNING",
                    "VACUUM",
                ],
                &[
                    "AUTO_INCREMENT",
                    "DESCRIBE",
                    "DIV",
                    "DUPLICATE",
                    "ENGINE",
                    "IGNORE",
                    "REGEXP",
                    "RLIKE",
                    "STRAIGHT_JOIN",
                    "UNSIGNED",
                    "USE",
                    "XOR",
                ],
            ),
            Self::Sqlite => (
                &[
                    "COMMENT",
                    "COPY",
                    "EXTENSION",
                    "FUNCTION",
                    "GRANT",
                    "ILIKE",
                    "LANGUAGE",
                    "LATERAL",
                    "MATERIALIZED",
                    "PROCEDURE",
                    "REFRESH",
                    "RETURNS",
                    "REVOKE",
                    "SCHEMA",
                    "SEQUENCE",
                    "SHOW",
                    "TRUNCATE",
                    "TYPE",
                ],
                &[
                    "ABORT",
                    "ATTACH",
                    "AUTOINCREMENT",
                    "DETACH",
                    "FAIL",
                    "GLOB",
                    "IGNORE",
                    "INDEXED",
                    "PRAGMA",
                    "REGEXP",
                    "ROWID",
                    "STRICT",
                    "TEMP",
                    "WITHOUT",
                ],
            ),
            Self::Other => (&[], &[]),
        };
        let mut words: Vec<&'static str> = sql::KEYWORDS
            .iter()
            .copied()
            .filter(|word| !lacks.contains(word))
            .collect();
        words.extend(own);
        words
    }

    /// Built-in functions, in lower case.
    pub fn functions(self) -> Vec<&'static str> {
        // What every one of them has.
        let mut functions = vec![
            "abs",
            "avg",
            "cast",
            "coalesce",
            "count",
            "dense_rank",
            "lag",
            "lead",
            "length",
            "lower",
            "max",
            "min",
            "nullif",
            "rank",
            "replace",
            "round",
            "row_number",
            "sum",
            "trim",
            "upper",
        ];
        let own: &[&str] = match self {
            Self::Postgres => &[
                "age",
                "array_agg",
                "array_length",
                "ceil",
                "concat",
                "current_setting",
                "date_trunc",
                "extract",
                "floor",
                "gen_random_uuid",
                "generate_series",
                "greatest",
                "json_agg",
                "json_build_object",
                "jsonb_agg",
                "jsonb_build_object",
                "least",
                "now",
                "regexp_replace",
                "split_part",
                "string_agg",
                "substring",
                "to_char",
                "to_timestamp",
                "unnest",
            ],
            Self::MySql => &[
                "ceil",
                "concat",
                "concat_ws",
                "curdate",
                "date_add",
                "date_format",
                "date_sub",
                "datediff",
                "extract",
                "floor",
                "greatest",
                "group_concat",
                "if",
                "ifnull",
                "json_arrayagg",
                "json_extract",
                "json_object",
                "json_objectagg",
                "last_insert_id",
                "least",
                "now",
                "str_to_date",
                "substring",
                "unix_timestamp",
                "uuid",
            ],
            Self::Sqlite => &[
                "ceil",
                "date",
                "datetime",
                "floor",
                "group_concat",
                "ifnull",
                "iif",
                "instr",
                "json_extract",
                "json_group_array",
                "json_group_object",
                "julianday",
                "last_insert_rowid",
                "printf",
                "random",
                "strftime",
                "substr",
                "total",
                "typeof",
            ],
            Self::Other => &[
                "array_agg",
                "ceil",
                "concat",
                "date_trunc",
                "extract",
                "floor",
                "greatest",
                "json_agg",
                "least",
                "now",
                "string_agg",
                "substring",
                "to_char",
            ],
        };
        functions.extend(own);
        functions.sort_unstable();
        functions
    }

    /// Column types, offered after `::` and in definitions.
    pub fn types(self) -> &'static [&'static str] {
        match self {
            Self::Postgres | Self::Other => sql::TYPES,
            Self::MySql => &[
                "BIGINT",
                "BINARY",
                "BIT",
                "BLOB",
                "BOOLEAN",
                "CHAR",
                "DATE",
                "DATETIME",
                "DECIMAL",
                "DOUBLE",
                "ENUM",
                "FLOAT",
                "INT",
                "INTEGER",
                "JSON",
                "LONGBLOB",
                "LONGTEXT",
                "MEDIUMINT",
                "MEDIUMTEXT",
                "NUMERIC",
                "SMALLINT",
                "TEXT",
                "TIME",
                "TIMESTAMP",
                "TINYINT",
                "VARBINARY",
                "VARCHAR",
                "YEAR",
            ],
            // Storage classes; `ANY` in strict tables.
            Self::Sqlite => &["ANY", "BLOB", "INTEGER", "NUMERIC", "REAL", "TEXT"],
        }
    }

    /// Schemas whose objects need no qualifier, given the schemas there
    /// are and who the session is: PostgreSQL's own schema and `public`;
    /// MySQL's database, which is its schema; else the first one.
    pub fn search_path(self, schemas: &[String], user: &str, database: &str) -> Vec<String> {
        let candidates: &[&str] = match self {
            Self::Postgres => &[user, "public"],
            Self::MySql => &[database],
            Self::Sqlite => &["main"],
            Self::Other => &[user, "public", database],
        };
        let mut path: Vec<String> = Vec::new();
        for name in candidates {
            if schemas.iter().any(|schema| schema == name) && !path.iter().any(|p| p == name) {
                path.push((*name).to_owned());
            }
        }
        if path.is_empty()
            && let Some(first) = schemas.first()
        {
            path.push(first.clone());
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_words_are_sorted() {
        for family in [
            Family::Postgres,
            Family::MySql,
            Family::Sqlite,
            Family::Other,
        ] {
            assert!(family.reserved().windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn each_database_gets_its_own_words() {
        assert!(Family::Postgres.functions().contains(&"array_agg"));
        assert!(!Family::MySql.functions().contains(&"array_agg"));
        assert!(!Family::Sqlite.functions().contains(&"date_trunc"));
        assert!(Family::MySql.functions().contains(&"group_concat"));
        assert!(!Family::MySql.keywords().contains(&"RETURNING"));
        assert!(Family::Sqlite.keywords().contains(&"PRAGMA"));
        assert!(Family::Sqlite.starts().contains(&"PRAGMA"));
        assert!(!Family::Sqlite.starts().contains(&"EXPLAIN ANALYZE"));
        assert!(Family::MySql.starts().contains(&"DESCRIBE"));
        assert!(Family::MySql.types().contains(&"DATETIME"));
        assert!(Family::Other.functions().contains(&"date_trunc"));
    }

    #[test]
    fn the_search_path_follows_the_database() {
        let schemas = ["app".to_owned(), "public".to_owned(), "shop".to_owned()];
        assert_eq!(
            Family::Postgres.search_path(&schemas, "app", "shop"),
            ["app", "public"]
        );
        assert_eq!(Family::MySql.search_path(&schemas, "app", "shop"), ["shop"]);
        let main = ["main".to_owned(), "temp".to_owned()];
        assert_eq!(Family::Sqlite.search_path(&main, "", "x.db"), ["main"]);
        assert_eq!(Family::MySql.search_path(&main, "", "nope"), ["main"]);
    }
}
